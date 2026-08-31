//! Characterization tests for the authenticated request path: `POST
//! /v1/chat/completions` through admission, the candidate walk, and settlement.
//!
//! Every test drives the real axum handler over a real Postgres; only the
//! upstream leaf is swapped, via `RouterState::with_injected_route` and the
//! scriptable fakes in `zerorouter::testing` (both behind the `testing`
//! feature). The catalog is `tests/request_path_tiers.toml`, so these
//! assertions pin router behavior rather than the production candidate list.
//!
//! These tests document what the code does, not what it ought to do. Where
//! current behavior is known-wrong but not yet fixable here the actual behavior
//! is still asserted, with a comment saying so — do not "fix" the code to make
//! one of these pass; change the assertion deliberately when the behavior
//! changes. Both walks now record a `request_attempts` ledger, so the
//! known-gap assertion that used to live here is gone.
//!
//! Gated on `DATABASE_URL` like `tests/billing.rs`: when unset each test
//! returns early (skips) instead of failing.

use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::provider::{TokenUsage, ToolCall};
use zerorouter::{
    RouterState,
    api::InjectedRoute,
    app,
    auth::{generate_api_key, hash_api_key},
    billing::{balance, grant_promo},
    config::ResolvedRoute,
    db::migrate,
    openai::{TASK_SIGNATURE_SCHEME, tool_names_digest},
    providers::{ProviderCandidate, ProviderRoute},
    testing::{FakeModelProvider, FakeOutcome, FakeStreamStep},
};

/// Output bound every request asks for. Large enough that metered usage stays
/// under the reservation, so the settle debit is the metered cost and not the
/// reservation clamp (which `tests/billing.rs` already pins).
const MAX_TOKENS: u32 = 4_096;

/// Pooled connections each test opens up front.
///
/// Two used to be enough: a request admitted, walked, and settled in sequence
/// while the test read rows back. A request now has one CONCURRENT database
/// user as well — the dispatch marker (`UsageSession::dispatch_marker`), fired
/// and never awaited at the moment the walk reaches an upstream — so a settle
/// can find both connections held. That matters here and essentially nowhere
/// else: the deadline and backoff tests run under `tokio::time::pause()`, and
/// a paused runtime advances the mocked clock to the next armed timer whenever
/// it parks, which turns a momentary wait for a connection into an instant
/// `PoolTimedOut` and a 503 in place of the answer under test.
const POOL_CONNECTIONS: u32 = 3;

/// What the fakes report when they serve. Chosen well below the reservation.
fn served_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(1_000),
        output_tokens: Some(20),
        cached_input_tokens: None,
    }
}

/// `1000 * $3.00/Mtok + 20 * $6.00/Mtok` at the fixture tier's sell rate.
fn served_sell_cost() -> Decimal {
    decimal("0.00312")
}

/// The per-chunk `token_count` lower bound a seven-character delta reports.
/// Never billed to a customer — the settle policy is metered actuals only — but
/// still written to the attempt ledger, where it prices ZeroRouter's own COGS
/// for an attempt the upstream abandoned without a usage report.
const ESTIMATED_OUTPUT_TOKENS: i32 = 2;

fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).expect("test decimal must parse")
}

/// The waits between a fake's consecutive calls, in whole tens of milliseconds.
///
/// Tokio's timer rounds a deadline up to the next millisecond tick, so a 500ms
/// sleep lands at 501ms on the mocked clock. The retry schedule's steps are
/// hundreds of milliseconds apart, so a tens-of-ms reading is exact for what is
/// being asserted and immune to that tick.
fn backoff_steps_ms(fake: &FakeModelProvider) -> Vec<u64> {
    fake.call_gaps()
        .into_iter()
        .map(|gap| u64::try_from(gap.as_millis() / 10 * 10).unwrap_or(u64::MAX))
        .collect()
}

fn tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/request_path_tiers.toml")
}

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(POOL_CONNECTIONS)
        // The deadline tests reach a 15-minute constant by pausing the clock,
        // and a paused runtime auto-advances to the nearest timer whenever it
        // parks on socket I/O. A liveness ping would arm the acquire timeout
        // around exactly such a park, so acquire is kept timer-free and every
        // connection is opened up front by `warm_pool`.
        .test_before_acquire(false)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    warm_pool(&pool).await;
    Some(pool)
}

/// Open every pooled connection so no later acquire has to dial out.
async fn warm_pool(pool: &PgPool) {
    let mut connections = Vec::with_capacity(POOL_CONNECTIONS as usize);
    for _ in 0..POOL_CONNECTIONS {
        connections.push(pool.acquire().await.expect("pool connection must open"));
    }
    drop(connections);
}

/// A funded user with one API key, returned as `(api_key_id, plaintext key)`.
/// Every test gets its own so `usage_events` can be scoped by key.
async fn create_funded_key(pool: &PgPool, label: &str) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("request-path-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    let key_id = Uuid::new_v4();
    let plaintext = generate_api_key();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'request-path', 20, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(pool)
    .await
    .expect("test API key must insert");
    grant_promo(pool, user_id, Decimal::from(50), "request-path")
        .await
        .expect("funding promo must apply");
    (key_id, plaintext)
}

async fn user_of(pool: &PgPool, api_key_id: Uuid) -> Uuid {
    query_scalar::<_, Uuid>("SELECT user_id FROM api_keys WHERE id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("owning user must query")
}

/// A catalog with one healthy tier and one tier priced below its own cost
/// basis, for the withheld-tier test.
fn withheld_tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/withheld_tier_tiers.toml")
}

/// A catalog whose two candidates name the SAME upstream provider, so anything
/// the walk keyed by provider rather than by candidate would leak between them.
fn twin_tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/two_on_one_provider_tiers.toml")
}

/// A completion the emptiness check reads as blank: no text, no tool calls, no
/// reasoning. It still carries a usage report, so a blank turn the walk chooses
/// to RETURN settles as an ordinary metered 200 rather than falling into the
/// unmetered-gap branch — which is what makes the re-roll a money question.
fn blank_completion() -> FakeOutcome {
    FakeOutcome::Chat {
        text: Some(String::new()),
        tool_calls: Vec::new(),
        usage: Some(served_usage()),
        reasoning_content: None,
        stop_reason: None,
    }
}

/// A router whose candidates are served by `fakes` in tier order. Panics if a
/// resolved route has more candidates than the test scripted, which would
/// silently leave a real (credential-less) upstream in the walk.
fn router(pool: PgPool, fakes: Vec<Arc<FakeModelProvider>>) -> RouterState {
    router_with_catalog(pool, fakes, tier_config_path())
}

/// [`router`], but matching fakes to candidates by alias (the candidate id's
/// suffix) instead of positionally — one state can then serve tiers with
/// different candidate counts at once, which the soak needs.
fn router_matching_aliases(pool: PgPool, fakes: Vec<Arc<FakeModelProvider>>) -> RouterState {
    let route: InjectedRoute = Arc::new(move |resolved: &ResolvedRoute, _max_output_tokens| {
        ProviderRoute::from_candidates(
            resolved
                .candidates
                .iter()
                .cloned()
                .map(|definition| {
                    let alias = definition.id.split('/').next_back().unwrap_or_default();
                    let fake = fakes
                        .iter()
                        .find(|fake| {
                            zerorouter::provider::ModelProvider::alias(fake.as_ref()) == alias
                        })
                        .unwrap_or_else(|| {
                            panic!("no scripted fake for candidate {}", definition.id)
                        })
                        .clone();
                    ProviderCandidate::with_provider(definition, fake)
                })
                .collect(),
        )
    });
    RouterState::with_injected_route(tier_config_path(), pool, true, route)
}

/// [`router`], reading a caller-chosen catalog.
fn router_with_catalog(
    pool: PgPool,
    fakes: Vec<Arc<FakeModelProvider>>,
    catalog: PathBuf,
) -> RouterState {
    let route: InjectedRoute = Arc::new(move |resolved: &ResolvedRoute, _max_output_tokens| {
        assert_eq!(
            resolved.candidates.len(),
            fakes.len(),
            "every resolved candidate needs a scripted fake"
        );
        ProviderRoute::from_candidates(
            resolved
                .candidates
                .iter()
                .cloned()
                .zip(fakes.iter().cloned())
                .map(|(definition, fake)| ProviderCandidate::with_provider(definition, fake))
                .collect(),
        )
    });
    RouterState::with_injected_route(catalog, pool, true, route)
}

fn completion_body(model: &str, stream: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello" }],
        "max_tokens": MAX_TOKENS,
        "temperature": 0.25,
        "stream": stream,
    });
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    body
}

fn completion_request(key: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("completion request should build")
}

fn header(response: &axum::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should be readable")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("response body should be JSON")
}

/// The `data:` payloads of an SSE body, in order. Keep-alive comment lines are
/// not `data:` frames and drop out here.
async fn sse_payloads(response: axum::response::Response) -> Vec<String> {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("stream body should be readable")
        .to_bytes();
    String::from_utf8(bytes.to_vec())
        .expect("stream body should be UTF-8")
        .lines()
        .filter_map(|line| line.strip_prefix("data: ").map(str::to_owned))
        .collect()
}

/// The JSON `data:` payloads, dropping the terminal `[DONE]` sentinel.
async fn sse_chunks(response: axum::response::Response) -> Vec<Value> {
    sse_payloads(response)
        .await
        .into_iter()
        .filter(|payload| payload != "[DONE]")
        .map(|payload| serde_json::from_str(&payload).expect("stream chunk should be JSON"))
        .collect()
}

/// `(upstream_provider, upstream_model, input_tokens, output_tokens, cost_usd, status)`
async fn settled_event(
    pool: &PgPool,
    api_key_id: Uuid,
) -> (String, String, i32, i32, Decimal, i16) {
    query_as(
        r#"
        SELECT upstream_provider, upstream_model, input_tokens, output_tokens, cost_usd, status
        FROM usage_events
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("settled row must query")
}

/// `(candidate_id, cost_basis_usd, attempt_count, finish_reason)`
async fn settled_provenance(
    pool: &PgPool,
    api_key_id: Uuid,
) -> (Option<String>, Option<Decimal>, Option<i16>, Option<String>) {
    query_as(
        r#"
        SELECT candidate_id, cost_basis_usd, attempt_count, finish_reason
        FROM usage_events
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("settled provenance must query")
}

/// `(attempt_no, candidate_id, outcome, served)` for every walk attempt.
async fn attempt_rows(pool: &PgPool, api_key_id: Uuid) -> Vec<(i16, String, String, bool)> {
    query_as(
        r#"
        SELECT attempt_no, candidate_id, outcome, served
        FROM request_attempts
        WHERE api_key_id = $1
        ORDER BY attempt_no
        "#,
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await
    .expect("attempt rows must query")
}

/// Which check rejected each attempt, in walk order. `None` on every row a
/// check was not the reason for.
async fn attempt_validator_kinds(pool: &PgPool, api_key_id: Uuid) -> Vec<Option<String>> {
    query_scalar::<_, Option<String>>(
        r#"
        SELECT validator_kind
        FROM request_attempts
        WHERE api_key_id = $1
        ORDER BY attempt_no
        "#,
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await
    .expect("validator kinds must query")
}

/// `(attempts_cost_basis_usd, attempts_cost_basis_complete)` — what the losing
/// attempts burnt, and whether that number is the whole story.
///
/// Both are NULL only when no walk was recorded at all. A request whose single
/// attempt served has no losing attempts, which is a genuine zero rather than
/// an unknown, and reads `Some(0)` + `Some(true)`.
async fn attempts_cogs(pool: &PgPool, api_key_id: Uuid) -> (Option<Decimal>, Option<bool>) {
    query_as(
        r#"
        SELECT attempts_cost_basis_usd, attempts_cost_basis_complete
        FROM usage_events
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("attempts COGS must query")
}

/// The after-the-fact metering-gap detector documented on
/// `StreamDelivery::settled_usage`, run verbatim: requests where an attempt was
/// served yet the settled row carries no tokens, i.e. output the customer
/// received that ZeroRouter could not bill. Needs no column beyond migration
/// 0004 — a genuine usage report can never be all-zero
/// (`OpenAiUsage::try_from_provider` rejects that), so zero tokens on a served
/// request means "never metered".
async fn unbilled_served_requests(pool: &PgPool, api_key_id: Uuid) -> i64 {
    query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM usage_events e
        JOIN request_attempts a USING (request_id)
        WHERE e.api_key_id = $1
          AND a.served
          AND e.input_tokens = 0
          AND e.output_tokens = 0
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("metering-gap count must query")
}

/// The implicit shape label on the settled row — the success estimator's
/// day-one training signal.
async fn settled_shape_ok(pool: &PgPool, api_key_id: Uuid) -> Option<bool> {
    query_scalar::<_, Option<bool>>("SELECT shape_ok FROM usage_events WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("shape label must query")
}

/// The resolved priority written on the settled row (rollout stage 3a):
/// always present once the knob ships, `'balanced'` when nothing engaged it.
async fn settled_priority(pool: &PgPool, api_key_id: Uuid) -> Option<String> {
    query_scalar::<_, Option<String>>("SELECT priority FROM usage_events WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("settled priority must query")
}

/// The tier name on the settled row — the STRIPPED model name when the
/// request carried a priority suffix.
async fn settled_tier(pool: &PgPool, api_key_id: Uuid) -> String {
    query_scalar::<_, String>("SELECT tier FROM usage_events WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("settled tier must query")
}

/// How many rows this key settled — for asserting a refused request settled
/// nothing.
async fn settled_count(pool: &PgPool, api_key_id: Uuid) -> i64 {
    query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_events WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("settled count must query")
}

async fn open_reservations(pool: &PgPool, api_key_id: Uuid) -> i64 {
    query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("reservation count must query")
}

// ---------------------------------------------------------------------------
// `POST /v1/responses` — the second inbound dialect.
//
// These tests exist to prove ONE claim in particular: a Responses request is
// admitted, priced, walked, attested and settled by exactly the same code a
// chat-completions request is. So each of them asserts the ledger as well as
// the envelope, and several assert the two endpoints answer identically where
// the pipeline — not the serializer — decides the answer.
// ---------------------------------------------------------------------------

/// A user with one API key and NO credit, for the admission-parity test.
async fn create_unfunded_key(pool: &PgPool, label: &str) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("request-path-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    let key_id = Uuid::new_v4();
    let plaintext = generate_api_key();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'request-path', 20, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(pool)
    .await
    .expect("test API key must insert");
    (key_id, plaintext)
}

fn responses_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "input": "hello",
        "max_output_tokens": MAX_TOKENS,
        "temperature": 0.25,
        "stream": stream,
        // What a Codex-shaped client sends and what this router can honour.
        "store": false,
    })
}

fn responses_request(key: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("responses request should build")
}

/// The `(event, data)` pairs of an SSE body, in order.
///
/// The Responses dialect NAMES its events on the wire where chat completions
/// leaves the line off, so this reads both — a frame with no `event:` line
/// reports an empty name.
async fn sse_events(response: axum::response::Response) -> Vec<(String, String)> {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("stream body should be readable")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("stream body should be UTF-8");
    let mut events = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            pending = name.to_owned();
        } else if let Some(data) = line.strip_prefix("data: ") {
            events.push((std::mem::take(&mut pending), data.to_owned()));
        }
    }
    events
}

/// The three transparency labels a response carries, as a tuple.
fn transparency(response: &axum::response::Response) -> (String, String, String) {
    (
        header(response, "x-zerorouter-provider"),
        header(response, "x-zerorouter-byok"),
        header(response, "x-zerorouter-retention"),
    )
}

#[tokio::test]
async fn responses_non_streaming_serves_through_the_shared_pipeline_and_settles() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "responses-serve").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(responses_request(
            &key,
            &responses_body("zero/test-solo", false),
        ))
        .await
        .expect("responses request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = header(&response, "x-request-id");
    assert!(request_id.starts_with("chatcmpl-"));
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    // The envelope, in this dialect's shape.
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["incomplete_details"], Value::Null);
    assert_eq!(body["model"], "zero/test-solo");
    // One identity: the body id embeds the id the ledger and `x-request-id`
    // are keyed on, so a support ticket quoting either finds the settled row.
    assert_eq!(body["id"], format!("resp_{request_id}"));
    assert_eq!(body["output"][0]["type"], "message");
    assert_eq!(body["output"][0]["role"], "assistant");
    assert_eq!(body["output"][0]["content"][0]["text"], "hello from solo");
    assert_eq!(body["usage"]["input_tokens"], 1_000);
    assert_eq!(body["usage"]["output_tokens"], 20);
    assert_eq!(body["usage"]["total_tokens"], 1_020);

    // The request reached the upstream exactly as a chat request would: the
    // candidate's pinned model, the request's temperature, one message.
    let call = solo.calls().remove(0);
    assert_eq!(call.model, "upstream/solo");
    assert_eq!(call.temperature, Some(0.25));
    assert_eq!(call.message_count, 1);
    assert!(!call.streaming);

    // And it settled through the same transaction, at the same price, as the
    // chat-completions twin of this test.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
    let (candidate_id, cost_basis_usd, attempt_count, finish_reason) =
        settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/solo"));
    assert_eq!(cost_basis_usd, Some(decimal("0.00104")));
    assert_eq!(attempt_count, Some(1));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "openai/solo".to_owned(), "ok".to_owned(), true)]
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - served_sell_cost()
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn responses_streaming_emits_the_dialect_events_and_settles_the_metered_row() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "responses-stream").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("hel"),
            FakeStreamStep::text("lo"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(responses_request(
            &key,
            &responses_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = header(&response, "x-request-id");
    let events = sse_events(response).await;
    state.wait_for_background_tasks().await;

    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "response.created",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.completed",
        ],
        "and NO [DONE] sentinel: it is not valid JSON and this dialect does not send one"
    );
    let payloads: Vec<Value> = events
        .iter()
        .map(|(_, data)| serde_json::from_str(data).expect("every frame is JSON"))
        .collect();
    assert_eq!(payloads[0]["response"]["status"], "in_progress");
    assert_eq!(payloads[0]["response"]["id"], format!("resp_{request_id}"));
    assert_eq!(payloads[1]["delta"], "hel");
    assert_eq!(payloads[2]["delta"], "lo");
    // The terminal carries the whole answer and the settled usage, so a
    // consumer that reads only it still gets the response.
    let terminal = &payloads[3]["response"];
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["output"][0]["content"][0]["text"], "hello");
    assert_eq!(terminal["usage"]["input_tokens"], 1_000);
    assert_eq!(terminal["usage"]["output_tokens"], 20);

    let call = solo.calls().remove(0);
    assert!(call.streaming);
    // The SAME settled row a chat stream writes: the serializer changed, the
    // money path did not.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "openai/solo".to_owned(), "ok".to_owned(), true)]
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn responses_tool_calls_round_trip_out_and_back_in() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "responses-tools").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::Chat {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: r#"{"command":"pwd"}"#.to_owned(),
                    extra_content: None,
                }],
                usage: Some(served_usage()),
                reasoning_content: None,
                stop_reason: None,
            },
            FakeOutcome::chat("it is /home", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let tools = json!([{
        "type": "function",
        "name": "shell",
        "description": "run a command",
        "strict": false,
        "parameters": { "type": "object" },
    }]);
    let mut first = responses_body("zero/test-solo", false);
    first["tools"] = tools.clone();
    first["tool_choice"] = json!("auto");
    first["input"] = json!([{ "role": "user", "content": "run pwd" }]);

    let response = app(state.clone())
        .oneshot(responses_request(&key, &first))
        .await
        .expect("first responses request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;

    // OUT: the tool call as a `function_call` item, in this dialect's shape.
    assert_eq!(body["output"][0]["type"], "function_call");
    assert_eq!(body["output"][0]["call_id"], "call_1");
    assert_eq!(body["output"][0]["name"], "shell");
    assert_eq!(body["output"][0]["arguments"], r#"{"command":"pwd"}"#);
    // The tools are echoed back FLAT, which is this dialect's shape rather
    // than chat completions' nested one.
    assert_eq!(body["tools"][0]["name"], "shell");
    assert!(body["tools"][0].get("function").is_none());

    // BACK IN: the client replays the whole history, including the router's
    // own echoed item ids, and the result feeds the tool result to the model.
    let mut second = responses_body("zero/test-solo", false);
    second["tools"] = tools;
    second["tool_choice"] = json!("auto");
    second["input"] = json!([
        { "type": "message", "role": "user",
          "content": [{ "type": "input_text", "text": "run pwd" }] },
        { "type": "function_call", "id": body["output"][0]["id"].clone(),
          "status": "completed", "call_id": "call_1", "name": "shell",
          "arguments": r#"{"command":"pwd"}"# },
        { "type": "function_call_output", "call_id": "call_1", "output": "/home" },
    ]);
    let response = app(state.clone())
        .oneshot(responses_request(&key, &second))
        .await
        .expect("second responses request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["output"][0]["content"][0]["text"], "it is /home");

    // Three internal messages, not four: the assistant's turn carries its tool
    // call rather than sitting beside it, which is the shape every upstream
    // wire models and the one Anthropic requires.
    let calls = solo.calls();
    assert_eq!(calls[1].message_count, 3);
    assert_eq!(calls[1].tool_count, 1);
    assert_eq!(settled_count(&pool, api_key_id).await, 2);
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The transparency labels, on both endpoints, from the lane that served.
///
/// `zero/test-private` overrides its provider's `standard` pin with `zero`, so
/// this fails if the header ever stops resolving through
/// `TierCatalog::candidate_retention` — reading the provider map directly
/// would publish `standard` for a lane sold as zero retention.
#[tokio::test]
async fn transparency_headers_disclose_the_served_lane_on_both_endpoints() {
    let Some(pool) = connect().await else {
        return;
    };
    let (_, key) = create_funded_key(&pool, "transparency").await;
    let private = FakeModelProvider::new(
        "private",
        vec![
            FakeOutcome::chat("a", served_usage()),
            FakeOutcome::chat("b", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![private.clone()]);

    let chat = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-private", false),
        ))
        .await
        .expect("chat request should complete");
    assert_eq!(chat.status(), StatusCode::OK);
    assert_eq!(
        transparency(&chat),
        (
            "openai".to_owned(),
            "false".to_owned(),
            // The TIER's override, not the `standard` its provider is pinned
            // at — the same resolution `/v1/models` publishes.
            "zero".to_owned()
        )
    );

    let responses = app(state.clone())
        .oneshot(responses_request(
            &key,
            &responses_body("zero/test-private", false),
        ))
        .await
        .expect("responses request should complete");
    assert_eq!(responses.status(), StatusCode::OK);
    assert_eq!(
        transparency(&responses),
        ("openai".to_owned(), "false".to_owned(), "zero".to_owned()),
        "the disclosure is a property of the lane, not of the dialect asking"
    );
    state.wait_for_background_tasks().await;
}

/// A route whose rungs disagree publishes nothing rather than a label that
/// might name the wrong lane.
///
/// The buffered path never needs this — it knows which rung served — so the
/// two halves of this test are deliberately asymmetric: the same two-rung
/// tier discloses on `/v1/chat/completions` and withholds on the stream.
#[tokio::test]
async fn streaming_transparency_headers_are_withheld_when_the_route_disagrees() {
    let Some(pool) = connect().await else {
        return;
    };
    let (_, key) = create_funded_key(&pool, "transparency-mixed").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Stream(vec![
                FakeStreamStep::text("hi"),
                FakeStreamStep::Usage(served_usage()),
                FakeStreamStep::Final,
            ]),
            FakeOutcome::chat("hi", served_usage()),
        ],
    );
    let secondary = FakeModelProvider::new("secondary", Vec::new());
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    // `zero/test-pair` runs on two different providers, so which one answers
    // decides the labels — and a stream's head is written before it does.
    let streamed = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(streamed.status(), StatusCode::OK);
    assert_eq!(
        transparency(&streamed),
        (String::new(), String::new(), String::new()),
        "an unknowable label must be absent, never guessed"
    );
    let _ = sse_payloads(streamed).await;

    // The same tier, buffered: here the served rung IS known.
    let buffered = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("buffered request should complete");
    assert_eq!(buffered.status(), StatusCode::OK);
    assert_eq!(
        transparency(&buffered),
        (
            "openai".to_owned(),
            "false".to_owned(),
            "standard".to_owned()
        )
    );
    state.wait_for_background_tasks().await;
}

/// The SSRF gate is the shared one, reached because a Responses `input_image`
/// is translated into the chat content shape rather than carried separately.
#[tokio::test]
async fn responses_image_parts_meet_the_same_ssrf_gate_as_chat() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "responses-image").await;
    let vision = FakeModelProvider::new("vision", vec![FakeOutcome::chat("a cat", served_usage())]);
    let state = router(pool.clone(), vec![vision.clone()]);

    let image_body = |url: &str| {
        let mut body = responses_body("zero/test-vision", false);
        body["input"] = json!([{
            "role": "user",
            "content": [
                { "type": "input_text", "text": "what is this?" },
                { "type": "input_image", "image_url": url },
            ],
        }]);
        body
    };

    // The cloud-metadata endpoint. Refused BEFORE anything is reserved and
    // before any upstream is dialled.
    let refused = app(state.clone())
        .oneshot(responses_request(
            &key,
            &image_body("https://169.254.169.254/latest/meta-data/"),
        ))
        .await
        .expect("request should complete");
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(refused).await["error"]["code"],
        "unsupported_request_fields"
    );
    assert_eq!(vision.call_count(), 0, "no upstream may be dialled");
    assert_eq!(
        open_reservations(&pool, api_key_id).await,
        0,
        "a request refused at the SSRF gate must reserve nothing"
    );
    assert_eq!(settled_count(&pool, api_key_id).await, 0);

    // ...and the gate is not a blanket refusal of images: an inline data URI
    // is forwarded, because nothing is fetched.
    let served = app(state.clone())
        .oneshot(responses_request(
            &key,
            &image_body("data:image/png;base64,AAAA"),
        ))
        .await
        .expect("request should complete");
    assert_eq!(served.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    assert_eq!(vision.call_count(), 1);
    assert_eq!(settled_count(&pool, api_key_id).await, 1);
}

/// One admission path, proved by its refusals.
///
/// An unfunded key must meet the SAME 402 on both endpoints, because both
/// reach the same `admit_usage` — and the walk must never start, because
/// admission runs before it.
#[tokio::test]
async fn an_unfunded_key_is_refused_identically_on_both_endpoints() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_unfunded_key(&pool, "responses-unfunded").await;
    let solo = FakeModelProvider::new("solo", Vec::new());
    let state = router(pool.clone(), vec![solo.clone()]);

    let chat = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("chat request should complete");
    assert_eq!(chat.status(), StatusCode::PAYMENT_REQUIRED);
    let chat_body = json_body(chat).await;

    let responses = app(state.clone())
        .oneshot(responses_request(
            &key,
            &responses_body("zero/test-solo", false),
        ))
        .await
        .expect("responses request should complete");
    assert_eq!(responses.status(), StatusCode::PAYMENT_REQUIRED);
    let responses_body = json_body(responses).await;

    assert_eq!(chat_body["error"]["code"], "insufficient_credits");
    assert_eq!(
        responses_body, chat_body,
        "the two endpoints share one admission path, so they share its refusal byte for byte"
    );
    assert_eq!(solo.call_count(), 0, "admission runs before the walk");
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    assert_eq!(settled_count(&pool, api_key_id).await, 0);
}

/// The two storage refusals, over HTTP, with the codes a client branches on.
#[tokio::test]
async fn responses_storage_requests_are_refused_by_their_own_codes() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "responses-storage").await;
    let solo = FakeModelProvider::new("solo", Vec::new());
    let state = router(pool.clone(), vec![solo.clone()]);

    for (field, value, code) in [
        ("store", json!(true), "responses_store_unsupported"),
        (
            "previous_response_id",
            json!("resp_1"),
            "responses_previous_response_unsupported",
        ),
    ] {
        let mut body = responses_body("zero/test-solo", false);
        body[field] = value;
        let refused = app(state.clone())
            .oneshot(responses_request(&key, &body))
            .await
            .expect("request should complete");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        let refused = json_body(refused).await;
        assert_eq!(refused["error"]["code"], code, "for {field}");
        assert_eq!(refused["error"]["param"], field);
    }

    // A knob the router cannot forward is refused BY NAME, which is the whole
    // difference between a debuggable 400 and an afternoon lost.
    let mut body = responses_body("zero/test-solo", false);
    body["reasoning"] = json!({ "effort": "high" });
    body["include"] = json!(["reasoning.encrypted_content"]);
    let refused = app(state.clone())
        .oneshot(responses_request(&key, &body))
        .await
        .expect("request should complete");
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let refused = json_body(refused).await;
    assert_eq!(refused["error"]["code"], "unsupported_request_fields");
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("include, reasoning"), "{message}");

    assert_eq!(solo.call_count(), 0);
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    assert_eq!(settled_count(&pool, api_key_id).await, 0);
}

/// Admission tells the same story the catalog does.
///
/// This runs the REAL provider-construction path — no injected route — against
/// a process holding no upstream credentials, so every candidate drops out and
/// the route cannot be built. That used to answer `no_provider_available`,
/// which reads as "the upstream fleet is down" and sends an operator to look at
/// the wrong thing. It is now the same `model_unavailable` a tier withheld for
/// below-cost pricing returns, naming the lane, because both mean the identical
/// thing to a caller: ZeroRouter cannot serve this and you cannot fix it.
///
/// The money property is asserted alongside, because this refusal moved: route
/// construction runs BEFORE `admit_usage`, so a refused request must leave no
/// reservation behind. It never did, and this is what keeps that true if the
/// order is ever shuffled.
#[tokio::test]
async fn a_lane_with_no_credential_is_refused_by_name_and_reserves_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "uncredentialed").await;
    // The production constructor: no injected route, so `provider_route` builds
    // from the environment exactly as it does in a real deployment.
    let state = RouterState::with_database(tier_config_path(), pool.clone(), true, None);

    // `zero/test-solo` runs on `openai`, whose key this process does not hold —
    // the same condition production hit with `bedrock`.
    let refused = app(state)
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");

    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(refused).await;
    assert_eq!(body["error"]["code"], "model_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("zero/test-solo"),
        "the refusal must name the lane the caller asked for: {body}"
    );
    assert_eq!(
        open_reservations(&pool, api_key_id).await,
        0,
        "a request refused for want of a credential must reserve nothing"
    );
}

#[tokio::test]
async fn a_withheld_tier_is_refused_while_a_healthy_tier_in_the_same_catalog_serves() {
    // The whole point of scoping the margin verdict, proven on the real
    // request path: `zero/test-below-cost` cannot cover its own candidate, so a
    // request for it is refused as a misconfiguration — named, distinct from a
    // missing model, and short of admission so nothing is reserved or billed —
    // while `zero/test-solo`, sitting in the same file and served by the same
    // process moments later, completes and settles exactly as it always did.
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "withheld").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router_with_catalog(
        pool.clone(),
        vec![solo.clone()],
        withheld_tier_config_path(),
    );

    let refused = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-below-cost", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    let refused_body = json_body(refused).await;
    assert_eq!(refused_body["error"]["code"], "model_unavailable");
    assert!(
        refused_body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("zero/test-below-cost"),
        "{refused_body}"
    );
    // Refused before the walk and before admission: no upstream call, no
    // reservation, no charge.
    assert_eq!(solo.call_count(), 0);
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );

    let served = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(served.status(), StatusCode::OK);
    let served_body = json_body(served).await;
    state.wait_for_background_tasks().await;
    assert_eq!(
        served_body["choices"][0]["message"]["content"],
        "hello from solo"
    );
    assert_eq!(solo.call_count(), 1);

    // One settled row, for the healthy tier only.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - served_sell_cost()
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn non_streaming_first_candidate_serves_and_settles_its_metered_usage() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "serve").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![FakeOutcome::chat("hello from primary", served_usage())],
    );
    let secondary = FakeModelProvider::new("secondary", Vec::new());
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "openai");
    assert_eq!(header(&response, "x-zerorouter-model"), "upstream/primary");
    let request_id = header(&response, "x-request-id");
    assert!(request_id.starts_with("chatcmpl-"));
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(body["id"], request_id.as_str());
    assert_eq!(body["model"], "zero/test-pair");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from primary"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 1_000);
    assert_eq!(body["usage"]["completion_tokens"], 20);

    // Dispatch actually happened, against the candidate's pinned upstream model
    // and carrying the request's temperature — never the tier id.
    assert_eq!(primary.call_count(), 1);
    assert_eq!(secondary.call_count(), 0);
    let call = primary.calls().remove(0);
    assert_eq!(call.model, "upstream/primary");
    assert_eq!(call.temperature, Some(0.25));
    assert_eq!(call.message_count, 1);
    assert!(!call.streaming);

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/primary".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
    let (candidate_id, cost_basis_usd, attempt_count, finish_reason) =
        settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/primary"));
    // COGS at the candidate's own cost basis: 1000 * $1.00 + 20 * $2.00 per Mtok.
    assert_eq!(cost_basis_usd, Some(decimal("0.00104")));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    // One dispatch, recorded as the served attempt.
    assert_eq!(attempt_count, Some(1));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "openai/primary".to_owned(), "ok".to_owned(), true)]
    );
    // No losing attempts is a genuine zero, not an unknown — the served
    // attempt's COGS is counted once, on `cost_basis_usd` above.
    assert_eq!(
        attempts_cogs(&pool, api_key_id).await,
        (Some(Decimal::ZERO), Some(true))
    );

    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - served_sell_cost()
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn non_streaming_failover_retries_the_first_candidate_twice_then_bills_the_second() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "failover").await;
    // The walk allows `CANDIDATE_RETRIES` retries on a 500ms base backoff
    // (api.rs), so a retryable failure costs 3 upstream calls and ~1.5s before
    // the walk moves on. Scripting exactly 3 failures pins that budget: a 4th
    // call would fall off the script and fail the request instead of failing
    // over.
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");
    assert_eq!(
        header(&response, "x-zerorouter-model"),
        "upstream/secondary"
    );
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from secondary"
    );
    assert_eq!(primary.call_count(), 3, "one attempt plus two retries");
    assert_eq!(secondary.call_count(), 1);

    let (upstream_provider, upstream_model, _, _, cost_usd, status) =
        settled_event(&pool, api_key_id).await;
    assert_eq!(upstream_provider, "anthropic");
    assert_eq!(upstream_model, "upstream/secondary");
    assert_eq!(cost_usd, served_sell_cost(), "the sell rate is the tier's");
    assert_eq!(status, 200);
    let (candidate_id, cost_basis_usd, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("anthropic/secondary"));
    // COGS moves to the second candidate's basis: 1000 * $1.50 + 20 * $3.00.
    assert_eq!(cost_basis_usd, Some(decimal("0.00156")));

    // The three burnt calls on the first candidate are now on the record. They
    // used to leave no trace at all: the walk was delegated, and the only
    // per-request outcome it surfaced was which candidate had served.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "upstream_error".to_owned(),
                false
            ),
            (
                2,
                "openai/primary".to_owned(),
                "upstream_error".to_owned(),
                false
            ),
            (
                3,
                "openai/primary".to_owned(),
                "upstream_error".to_owned(),
                false
            ),
            (4, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ],
        "one row per dispatched upstream call, ordinals continuing across candidates"
    );
    // Zero AND incomplete, which is the honest reading: three calls were
    // burnt and none of them reported what it consumed. Not NULL — NULL now
    // means only "no walk recorded" — and not a total, which a bare zero
    // would claim.
    assert_eq!(
        attempts_cogs(&pool, api_key_id).await,
        (Some(Decimal::ZERO), Some(false))
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn non_streaming_rate_limited_candidate_moves_on_without_burning_retries() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "ratelimit").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    // A 429 is classified retryable-but-cool-down, so the walk abandons the
    // candidate after a single call instead of spending the retry budget.
    assert_eq!(primary.call_count(), 1);
    assert_eq!(secondary.call_count(), 1);
    let (candidate_id, _, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("anthropic/secondary"));
    // The abandoned 429 is on the record, labelled as the rate limit it was
    // rather than as a generic upstream error — the distinction a health
    // estimator needs and the delegated walk never surfaced.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (2, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ]
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn non_streaming_every_candidate_failing_releases_the_reservation_without_charge() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "allfail").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new("secondary", vec![FakeOutcome::RateLimited]);
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "upstream_unavailable");

    assert_eq!(primary.call_count(), 1);
    assert_eq!(secondary.call_count(), 1);
    // No tokens were delivered, so the reservation is released at zero cost.
    // The row names the last candidate the walk actually reached: the
    // `fallback-chain` sentinel means "no candidate had been selected", and
    // after the unroll that is only true before the first dispatch.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "anthropic".to_owned(),
            "upstream/secondary".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    let (candidate_id, cost_basis_usd, attempt_count, _) =
        settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("anthropic/secondary"));
    // Zero tokens at a real candidate's basis prices to zero, so margin is
    // `0 - 0 - attempts_cost_basis_usd` — arithmetically the burnt COGS,
    // where it used to be `0 - NULL - NULL`.
    assert_eq!(cost_basis_usd, Some(Decimal::ZERO));
    assert_eq!(attempt_count, Some(2));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (
                2,
                "anthropic/secondary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
        ]
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "a failed request must not move the balance"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The exact failure text the Responses wire composes for OpenAI's
/// `max_output_tokens` refusal — the live case that bought the
/// `upstream_rejected_parameters` terminal (finding
/// `2026-08-26-byok-failure-was-max-tokens-below-the-responses-api-minimum...`).
const PARAMETER_REFUSAL: &str = concat!(
    "openai responses API error: HTTP 400 Bad Request: ",
    r#"{"error":{"message":"Invalid 'max_output_tokens': integer below minimum value. Expected a value >= 16, but got 5 instead.","type":"invalid_request_error","param":"max_output_tokens","code":"integer_below_min_value"}}"#,
);

/// An upstream 400 that names the parameter it refused must reach the caller
/// as the 400 it is — code `upstream_rejected_parameters`, the parameter in
/// the message, the same status on the ledger — not as the generic
/// `upstream_unavailable` 502 that reads as an outage and invites the retry
/// guaranteed to fail identically. The reservation still releases at zero.
#[tokio::test]
async fn non_streaming_parameter_refusal_reaches_the_caller_as_a_400() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "paramrej").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::Failure(PARAMETER_REFUSAL)]);
    // The second candidate fails for an ordinary transient reason AFTER the
    // refusal: first-occurrence latching means the terminal still names the
    // parameter instead of the later, less useful failure. A 429 rather than
    // a transport fault, deliberately — the walk abandons a rate-limited rung
    // after exactly one call, where a retryable transport failure is retried
    // on a backoff schedule and its call count depends on timing.
    let secondary = FakeModelProvider::new("secondary", vec![FakeOutcome::RateLimited]);
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "upstream_rejected_parameters");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().expect("message is text");
    assert!(
        message.contains("max_output_tokens") && message.contains("Expected a value >= 16"),
        "the message hands the caller the fix: {message}"
    );

    // A parameter 400 is non-retryable for its candidate; the walk still
    // tried the next rung before exhausting.
    assert_eq!(primary.call_count(), 1);
    assert_eq!(secondary.call_count(), 1);
    // The ledger records the 400 the customer was sent, and the reservation
    // released without a charge.
    let (_, _, _, _, cost_usd, status) = settled_event(&pool, api_key_id).await;
    assert_eq!(status, 400);
    assert_eq!(cost_usd, Decimal::ZERO);
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The exact failure text the xAI wire produces when an upstream declines to
/// attest zero data retention, leaked to `'static` so the scripted fake can
/// report it verbatim.
///
/// Built from `ResponseAttestation` rather than hand-written: the whole point
/// of these two tests is the journey from the wire's message to the customer's
/// error code, and a hand-copied string would keep passing after the real
/// message changed underneath it.
fn attestation_failure_text() -> &'static str {
    let attestation = zerorouter::wire::ResponseAttestation::new("x-zero-data-retention", "true")
        .expect("the shipped declaration must be constructible");
    let failure = attestation
        .verify(
            "xai",
            reqwest::StatusCode::OK,
            &reqwest::header::HeaderMap::new(),
        )
        .expect_err("an empty header map attests nothing");
    Box::leak(failure.into_boxed_str())
}

/// An upstream that answers but will not confirm the retention guarantee its
/// lane is sold under is refused, named as such, and billed for nothing.
///
/// This is the whole xAI lane reduced to one assertion. Every other test of
/// this mechanism proves a piece — the header is read, the text classifies, the
/// stream yields nothing — and this one proves the pieces are connected: a
/// customer on a zero-retention lane gets a 502 that says why, an untouched
/// balance, and a closed reservation.
#[tokio::test]
async fn non_streaming_an_unattested_upstream_is_refused_by_name_and_billed_for_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "no-attestation").await;
    // Three scripted failures, but only one may ever be consumed.
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::Failure(attestation_failure_text()),
            FakeOutcome::Failure(attestation_failure_text()),
            FakeOutcome::Failure(attestation_failure_text()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    // Named, not generic. `upstream_unavailable` would invite the retry that
    // must not happen and would hide the one fact the customer needs.
    assert_eq!(body["error"]["code"], "retention_attestation_failed");
    let message = body["error"]["message"]
        .as_str()
        .expect("the error carries a message");
    assert!(
        message.contains("zero-data-retention"),
        "the customer must be told which guarantee was not met: {message}"
    );
    assert!(
        !message.contains("x-zero-data-retention") && !message.contains("xai"),
        "the customer is told the guarantee, not the header or the vendor: {message}"
    );

    // ONE call. This is the retry-suppression assertion, and it is the reason
    // the fake was scripted with three failures: on a single-candidate route an
    // ordinary retryable failure burns all three (see
    // `non_streaming_rate_limit_on_a_single_candidate_route_burns_the_full_budget`).
    // A retry here would deliver the customer's prompt to the unattested
    // upstream two more times.
    assert_eq!(
        solo.call_count(),
        1,
        "a retention failure must end the candidate immediately, not be retried"
    );

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(
            1,
            "openai/solo".to_owned(),
            "upstream_error".to_owned(),
            false
        )],
        "the attempt is ledgered under a status migration 0004 admits, and is not served"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "a refused request must not move the balance"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The same refusal on the streaming path, where it has to arrive before any
/// content does.
#[tokio::test]
async fn streaming_an_unattested_upstream_is_refused_before_any_content_is_sent() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "no-attestation-sse").await;
    // The wire asserts on the initial response headers, so a real xAI stream
    // that fails attestation produces the error as its FIRST event and never
    // opens a body. The fake reproduces that shape: an error step with nothing
    // ahead of it.
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![FakeStreamStep::Error(
            attestation_failure_text().to_owned(),
        )])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    // 200 with an error frame is the SSE contract: the response head is
    // committed before the upstream is dialled, so a streamed failure can only
    // ever be reported in-band. What matters is that the frame is the FIRST
    // one carrying anything and that no delta precedes it.
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(
        chunks
            .last()
            .expect("an error chunk should terminate the stream")["error"]["code"],
        "retention_attestation_failed"
    );
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["choices"][0]["delta"]["content"].is_null()),
        "no model output may reach a customer whose lane could not be attested: {chunks:?}"
    );

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "a refused stream must not move the balance"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The 15-minute `UPSTREAM_REQUEST_TIMEOUT` is a compile-time constant, so the
/// deadline is reached on a paused clock rather than in wall time. The clock is
/// paused only after the fixtures are in place, since the pool cannot dial out
/// while the runtime is auto-advancing.
#[tokio::test]
async fn non_streaming_timeout_releases_the_reservation_without_charge() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "timeout-sync").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stall(Duration::from_secs(20 * 60))],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    tokio::time::pause();

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    // Back to real time before the DB assertions: sqlx arms its acquire
    // timeout around every checkout, and under a paused clock any park on the
    // Postgres socket auto-advances straight into that timer — a PoolTimedOut
    // flake with two warm connections sitting free.
    tokio::time::resume();
    assert_eq!(body["error"]["code"], "upstream_timeout");

    // The non-streaming deadline releases the reservation at zero cost, and so
    // does its streaming sibling — see
    // `streaming_timeout_releases_the_reservation_without_charge`. The row now
    // names the candidate that was in flight when the deadline hit; the
    // delegated walk destroyed that with the dropped future.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            504,
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "openai/solo".to_owned(), "timeout".to_owned(), false)]
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn streaming_happy_path_emits_deltas_then_usage_and_settles_the_metered_row() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-ok").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("hel"),
            FakeStreamStep::text("lo"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "x-request-id").starts_with("chatcmpl-"));
    // CHANGED DELIBERATELY (retention transparency headers). This used to
    // assert the streaming response carried NO attribution, because the served
    // candidate is not known when the SSE head is written. That is still true;
    // what changed is that a single-candidate route cannot disagree with
    // itself, so the three labels are knowable before the walk starts and are
    // published. `zero/test-pair` still publishes nothing — see
    // `streaming_transparency_headers_are_withheld_when_the_route_disagrees`.
    assert_eq!(header(&response, "x-zerorouter-provider"), "openai");
    assert_eq!(header(&response, "x-zerorouter-byok"), "false");
    assert_eq!(header(&response, "x-zerorouter-retention"), "standard");
    let payloads = sse_payloads(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(payloads.last().map(String::as_str), Some("[DONE]"));
    let chunks = payloads
        .iter()
        .filter(|payload| payload.as_str() != "[DONE]")
        .map(|payload| serde_json::from_str::<Value>(payload).expect("chunk should be JSON"))
        .collect::<Vec<_>>();
    // role primer, two content deltas, the finish delta, then the usage chunk.
    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hel");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "lo");
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[4]["usage"]["completion_tokens"], 20);
    assert!(chunks[4]["choices"].as_array().expect("choices").is_empty());

    let call = solo.calls().remove(0);
    assert_eq!(call.model, "upstream/solo");
    assert!(call.streaming);

    // Metered-actuals-only cuts both ways: when the upstream DOES report usage
    // the billed row is exactly that report, token for token, and the balance
    // moves by exactly its sell-rate cost. Nothing about the unmetered policy
    // touches the normal path.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            i32::try_from(served_usage().input_tokens.expect("metered input")).expect("fits"),
            i32::try_from(served_usage().output_tokens.expect("metered output")).expect("fits"),
            served_sell_cost(),
            200,
        )
    );
    assert_eq!(
        unbilled_served_requests(&pool, api_key_id).await,
        0,
        "a metered request is not a metering gap"
    );
    let (candidate_id, cost_basis_usd, attempt_count, finish_reason) =
        settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/solo"));
    assert_eq!(cost_basis_usd, Some(decimal("0.00104")));
    assert_eq!(attempt_count, Some(1));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "ok".to_owned(), true)]
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - served_sell_cost()
    );
}

#[tokio::test]
async fn streaming_candidate_failing_before_any_bytes_fails_over_to_the_next() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-failover").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![FakeOutcome::Stream(vec![FakeStreamStep::Error(
            "upstream exploded".to_owned(),
        )])],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("served"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    // The failed candidate is invisible to the client: no error chunk is
    // emitted before the second candidate's output.
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");
    assert!(chunks.iter().all(|chunk| chunk.get("error").is_none()));
    assert_eq!(primary.call_count(), 1, "a stream error is not retried");
    assert_eq!(secondary.call_count(), 1);

    let (upstream_provider, _, _, _, cost_usd, status) = settled_event(&pool, api_key_id).await;
    assert_eq!(upstream_provider, "anthropic");
    assert_eq!(cost_usd, served_sell_cost());
    assert_eq!(status, 200);
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![
            (
                1,
                "openai/primary".to_owned(),
                "stream_error".to_owned(),
                false
            ),
            (2, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ]
    );
}

/// The highest-frequency unmetered settle there is: several provider families
/// never surface streaming usage at all, so "the stream succeeded and reported
/// no usage" is an ordinary case, not a failure mode. Billing policy is metered
/// actuals only, so ZeroRouter serves this one for free rather than guess.
#[tokio::test]
async fn streaming_success_without_upstream_usage_bills_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-unmetered").await;
    // Seven characters are delivered and the stream runs cleanly to `Final`;
    // the upstream simply never emits a usage chunk.
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("partial"),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "partial");
    // The delivered output is still followed by an unmetered marker, so the
    // gap stays visible to the caller and in the ledger's 502.
    assert_eq!(
        chunks
            .last()
            .expect("an error chunk should terminate the stream")["error"]["code"],
        "metering_unavailable"
    );

    // Nothing was metered, so nothing is billed. Both halves of the estimate
    // this used to charge were heuristics — a byte-length prompt bound and a
    // `len()/4` output floor — and pricing per-token rates against either is a
    // guess at a customer's bill. The settled row carries zero tokens and zero
    // cost; the reservation is released, not spent.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "an unmetered delivery must not move the balance"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);

    // The gap is countable after the fact: the served attempt is now recorded
    // (it was not before, leaving this request with no walk ledger at all) with
    // NULL token columns, and joined against the zero-token settled row it is
    // exactly one unbilled served request.
    let (_, cost_basis_usd, attempt_count, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(attempt_count, Some(1));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "ok".to_owned(), true)],
        "the candidate whose output the client received is still the served one"
    );
    let attempt_tokens = query_as::<_, (Option<i32>, Option<i32>, bool)>(
        r#"
        SELECT input_tokens, output_tokens, tokens_estimated
        FROM request_attempts
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("attempt row must query");
    assert_eq!(
        attempt_tokens,
        (None, None, false),
        "no tokens were measured, so none are claimed on the attempt either"
    );
    assert_eq!(unbilled_served_requests(&pool, api_key_id).await, 1);
    // The customer is billed nothing, but ZeroRouter's own cost for this
    // delivery is not zero — it is UNKNOWN, because the only party who could
    // have measured it did not. Re-pricing the billed usage here reported
    // `cost_basis_usd = 0`, an assertion that a real upstream completion was
    // free; sourcing it from the served attempt (whose token columns are NULL
    // above) carries the ignorance through instead.
    assert_eq!(
        cost_basis_usd, None,
        "unmetered served COGS is unknown, not zero"
    );
}

#[tokio::test]
async fn streaming_error_after_delivered_bytes_bills_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-broken").await;
    // Seven characters reach the client, then the upstream dies without ever
    // reporting usage.
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("partial"),
            FakeStreamStep::Error("upstream exploded".to_owned()),
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "partial");
    assert_eq!(
        chunks
            .last()
            .expect("an error chunk should terminate the stream")["error"]["code"],
        "upstream_unavailable"
    );

    // Tokens reached the client and the upstream never metered them, so nothing
    // is billed. The per-chunk `token_count` floor the stream reported is not a
    // measurement — it is `len()/4`, zero on adapters that never opt in — and
    // the reservation's prompt side is a byte-length bound, so neither may
    // price a customer's row.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    assert_eq!(unbilled_served_requests(&pool, api_key_id).await, 1);
    // The estimate survives on the attempt row and only there: it prices
    // ZeroRouter's own COGS for the delivery it just absorbed, flagged
    // `tokens_estimated` so it can never be mistaken for a metered actual.
    let attempt_usage = query_as::<_, (Option<i32>, Option<i32>, bool, Option<Decimal>)>(
        r#"
        SELECT input_tokens, output_tokens, tokens_estimated, cost_basis_usd
        FROM request_attempts
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("attempt row must query");
    assert_eq!(
        attempt_usage,
        (
            // NULL, not 0. This attempt certainly consumed the prompt, and the
            // ledger used to write `input_tokens = 0` — an upstream call that
            // read nothing, which never happens. The only prompt quantity on
            // hand is the reservation's BYTE bound, and pricing a per-token
            // rate against bytes inflates the input side about fourfold, so the
            // honest record is that the prompt side was never measured.
            None,
            Some(ESTIMATED_OUTPUT_TOKENS),
            true,
            Some(decimal("0.000004"))
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "stream_error".to_owned(), true)],
        "the broken candidate is still the served one"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "an unmetered delivery must not move the balance"
    );
}

/// The positive control for the delivery signal, at the terminal that consults
/// it: real content reaches the client, the upstream reports usage, and the
/// stream then breaks — so the metered usage IS billed. Its mirror image is
/// `api.rs`'s `a_stream_whose_only_accepted_frame_is_the_role_primer_settles_at_zero`,
/// where the same terminal settles at zero because the only frame the client
/// accepted was scaffolding. Narrowing "delivered" to model output must not
/// suppress a charge for output that genuinely arrived.
#[tokio::test]
async fn streaming_error_after_metered_content_bills_the_metered_usage() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-metered-break").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("partial"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Error("upstream exploded".to_owned()),
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "partial");
    assert_eq!(
        chunks
            .last()
            .expect("an error chunk should terminate the stream")["error"]["code"],
        "upstream_unavailable"
    );

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            502,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - served_sell_cost(),
        "content that reached the client is billed at the metered actuals"
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "stream_error".to_owned(), true)],
        "the candidate whose content the client received is the served one"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// See `non_streaming_timeout_releases_the_reservation_without_charge` for why
/// the clock is paused, and paused only here.
#[tokio::test]
async fn streaming_timeout_releases_the_reservation_without_charge() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-timeout").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::Stall(Duration::from_secs(20 * 60)),
            FakeStreamStep::text("never sent"),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    tokio::time::pause();

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;
    // Real time for the DB assertions — see
    // `non_streaming_timeout_releases_the_reservation_without_charge` for why.
    tokio::time::resume();

    assert_eq!(chunks.len(), 1, "only the error chunk reaches the client");
    assert_eq!(chunks[0]["error"]["code"], "upstream_timeout");

    // Nothing was delivered, so the streaming deadline releases the reservation
    // at zero cost — the same answer its non-streaming sibling gives for the
    // identical request (see
    // `non_streaming_timeout_releases_the_reservation_without_charge`). The
    // shutdown terminal shares this settle site, so draining the router for a
    // deploy no longer charges in-flight streams their full output bound.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            504,
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "timeout".to_owned(), false)],
        "the burnt attempt is still recorded even though nothing is billed"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "a stream that delivered nothing must not move the balance"
    );
}

#[tokio::test]
async fn synthetic_stream_serves_a_candidate_that_cannot_stream() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "synthetic").await;
    let solo = FakeModelProvider::without_streaming(
        "solo",
        vec![FakeOutcome::Chat {
            text: Some("whole answer".to_owned()),
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "shell".to_owned(),
                arguments: r#"{"command":"pwd"}"#.to_owned(),
                extra_content: None,
            }],
            usage: Some(served_usage()),
            reasoning_content: None,
            stop_reason: None,
        }],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    // A non-streaming candidate is served as one buffered turn replayed as SSE:
    // role primer, the whole body, each tool call, the finish delta, usage.
    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "whole answer");
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "shell"
    );
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[4]["usage"]["prompt_tokens"], 1_000);

    // The router reached it through `chat`, not `stream_chat`.
    let call = solo.calls().remove(0);
    assert!(!call.streaming);
    assert_eq!(call.model, "upstream/solo");

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
    let (_, _, attempt_count, finish_reason) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(attempt_count, Some(1));
    assert_eq!(finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "ok".to_owned(), true)]
    );
}

/// The buffered sibling of the streaming gap. A non-streaming candidate returns
/// a complete response with no usage on it: the router has the whole answer in
/// hand, which is why this settle used to be argued as a special case and left
/// at the reservation bound. Under metered-actuals-only there is no special
/// case — nothing was measured, so nothing is billed.
#[tokio::test]
async fn synthetic_stream_without_upstream_usage_bills_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "synthetic-unmetered").await;
    let solo = FakeModelProvider::without_streaming(
        "solo",
        vec![FakeOutcome::Chat {
            text: Some("whole answer".to_owned()),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
            stop_reason: None,
        }],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    // This branch aborts instead of replaying the buffered answer, so the
    // customer receives nothing at all — which made billing the 4096-token
    // reservation bound here the starkest overcharge of the set.
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["error"]["code"], "metering_unavailable");

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "an unmetered response must not move the balance"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    // The attempt is recorded so the gap is countable, but `served` is false:
    // nothing was replayed to the client. That is the difference between a
    // metering gap ZeroRouter ate and one where the customer got the output.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "openai/solo".to_owned(), "ok".to_owned(), false)]
    );
    assert_eq!(unbilled_served_requests(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn streaming_client_disconnect_mid_stream_still_bills_the_metered_usage() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "disconnect").await;
    // The stall holds the upstream open long enough for the test to read the
    // opening frames and hang up before the rest arrives.
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("first"),
            FakeStreamStep::Stall(Duration::from_millis(250)),
            FakeStreamStep::text("second"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut delivered = 0_usize;
    while delivered < 2 {
        assert!(
            body.frame().await.is_some(),
            "the role primer and first delta should arrive"
        );
        delivered += 1;
    }
    drop(body);
    state.wait_for_background_tasks().await;

    // A client that hangs up mid-stream is still billed the upstream's metered
    // usage — the tokens were generated — but the row is labelled 499 so the
    // delivery failure stays visible in the ledger.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            499,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - served_sell_cost()
    );
}

/// A thinking model can answer entirely in `reasoning_content`. All three
/// label sites — the buffered walk, the synthetic stream it replays through,
/// and the live stream — must see that as a non-empty response. They consulted
/// only `text` and `tool_calls`, so a reasoning-only answer trained the success
/// estimator that reasoning models fail.
#[tokio::test]
async fn a_reasoning_only_answer_labels_as_output_on_every_path() {
    let Some(pool) = connect().await else {
        return;
    };

    // 1. Buffered (non-streaming) walk.
    let (buffered_key_id, buffered_key) = create_funded_key(&pool, "reasoning-buffered").await;
    let buffered = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::reasoning_only(
            "thinking it over",
            served_usage(),
        )],
    );
    let state = router(pool.clone(), vec![buffered.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &buffered_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("buffered request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(
        body["choices"][0]["message"]["reasoning_content"],
        "thinking it over"
    );
    assert_eq!(
        buffered.call_count(),
        1,
        "reasoning is output, so the emptiness check must not re-roll this turn"
    );
    assert_eq!(
        settled_shape_ok(&pool, buffered_key_id).await,
        Some(true),
        "a buffered answer that is entirely reasoning is a non-empty response"
    );

    // 2. Synthetic stream: the same buffered response, replayed as SSE.
    let (synthetic_key_id, synthetic_key) = create_funded_key(&pool, "reasoning-synthetic").await;
    let synthetic = FakeModelProvider::without_streaming(
        "solo",
        vec![FakeOutcome::reasoning_only(
            "thinking it over",
            served_usage(),
        )],
    );
    let state = router(pool.clone(), vec![synthetic]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &synthetic_key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("synthetic stream should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = sse_chunks(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(
        settled_shape_ok(&pool, synthetic_key_id).await,
        Some(true),
        "the synthetic path labels from the same evidence as its buffered sibling"
    );

    // 3. Live stream: reasoning deltas and nothing else.
    let (streamed_key_id, streamed_key) = create_funded_key(&pool, "reasoning-streamed").await;
    let streamed = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::reasoning("thinking"),
            FakeStreamStep::reasoning(" it over"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![streamed]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &streamed_key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["reasoning_content"],
        "thinking"
    );
    assert_eq!(
        settled_shape_ok(&pool, streamed_key_id).await,
        Some(true),
        "reasoning deltas are emitted output"
    );
}

/// The live streaming path used to infer "did the model produce output?" from
/// `usage.completion_tokens`, which is the provider's accounting rather than a
/// transcript. A stream that ran cleanly to `Final` having emitted nothing,
/// while the upstream cheerfully reported 20 output tokens, therefore labelled
/// as a healthy response — teaching a success estimator that the empty answer
/// was the good one.
#[tokio::test]
async fn a_stream_that_emitted_nothing_is_not_rescued_by_reported_output_tokens() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "empty-but-metered").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(
        settled_shape_ok(&pool, api_key_id).await,
        Some(false),
        "no delta was emitted, so the shape label must say so whatever usage claims"
    );
    // Billing is untouched by the label: the upstream metered it, so it bills.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
}

/// Migration 0004 said signatures could be re-keyed retroactively from the
/// persisted raw features. They could not: only `tool_count` was stored, and a
/// count cannot reproduce a key computed over tool NAMES. The settled row now
/// carries the exact digest the key was built from, plus the scheme that built
/// it, so the claim is true instead of aspirational.
#[tokio::test]
async fn a_settled_row_carries_the_signature_provenance_a_rekey_needs() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "signature-provenance").await;
    let solo = FakeModelProvider::new("solo", vec![FakeOutcome::chat("hello", served_usage())]);
    let state = router(pool.clone(), vec![solo]);

    let mut body = completion_body("zero/test-solo", false);
    body["tools"] = json!([
        { "type": "function", "function": { "name": "shell" } },
        { "type": "function", "function": { "name": "read" } },
    ]);
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let (signature, scheme, digest, tool_count) =
        query_as::<_, (String, Option<i16>, Option<String>, Option<i32>)>(
            r#"
        SELECT task_signature, task_signature_scheme, tool_names_sha256, tool_count
        FROM usage_events WHERE api_key_id = $1
        "#,
        )
        .bind(api_key_id)
        .fetch_one(&pool)
        .await
        .expect("settled row must query");
    assert_eq!(signature.len(), 16);
    assert_eq!(
        scheme,
        Some(TASK_SIGNATURE_SCHEME),
        "every key this build writes is stamped with the scheme that produced it"
    );
    assert_eq!(tool_count, Some(2));
    assert_eq!(
        digest,
        Some(tool_names_digest(&["read".to_owned(), "shell".to_owned()])),
        "the digest is the exact tool input the key was hashed from, order-independent"
    );
}

// ---------------------------------------------------------------------------
// Non-streaming walk semantics.
//
// Everything below pins behavior that the delegated walk had but that no test
// observed: the retry budget's shape, the classifier's verdicts, the
// empty-completion re-roll, context-window truncation, the shared deadline, and
// reservation release on every terminal. They were written against the
// delegated implementation and must keep passing verbatim once the router owns
// the loop — that invariance is the whole evidence that the unroll preserved
// behavior, so do not relax one to make a refactor fit.
// ---------------------------------------------------------------------------

/// A blank turn is re-rolled inside the candidate's OWN retry budget: the walk
/// does not treat it as a candidate failure and does not fall through.
#[tokio::test]
async fn non_streaming_empty_completion_is_rerolled_within_the_candidate_budget() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "empty-reroll").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            blank_completion(),
            blank_completion(),
            FakeOutcome::chat("hello on the third try", served_usage()),
        ],
    );
    let secondary = FakeModelProvider::new("secondary", Vec::new());
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(
        primary.call_count(),
        3,
        "two blank turns are re-rolled against the same candidate"
    );
    assert_eq!(
        secondary.call_count(),
        0,
        "a blank completion is not a candidate failure"
    );
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello on the third try"
    );
    let (candidate_id, _, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/primary"));
    assert_eq!(settled_shape_ok(&pool, api_key_id).await, Some(true));
    // A re-rolled blank turn is `validation_failed`, not `upstream_error`: the
    // HTTP call succeeded and the RESPONSE was rejected. `validator_kind`
    // records which check did the rejecting, so a later declared validator's
    // failures stay distinguishable from this one.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (
                2,
                "openai/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (3, "openai/primary".to_owned(), "ok".to_owned(), true),
        ]
    );
    assert_eq!(
        attempt_validator_kinds(&pool, api_key_id).await,
        [
            Some("empty_completion".to_owned()),
            Some("empty_completion".to_owned()),
            None,
        ]
    );
    // The two discarded responses reported usage, so the walk knows exactly
    // what re-rolling cost: 2 x $0.00104 of burnt COGS that was previously
    // dropped on the floor. It never reaches `cost_usd` — this is ZeroRouter's
    // spend, not the customer's bill.
    assert_eq!(
        attempts_cogs(&pool, api_key_id).await,
        (Some(decimal("0.00208")), Some(true))
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The re-roll is bounded by the same budget as an error retry, so the THIRD
/// blank turn is returned to the customer and billed. Dropping the re-roll
/// would turn the first blank into this outcome — which is why the re-roll is a
/// billing behavior, not a resilience nicety.
#[tokio::test]
async fn non_streaming_empty_completion_on_the_final_attempt_is_returned_as_a_blank_turn() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "empty-final").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![blank_completion(), blank_completion(), blank_completion()],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(primary.call_count(), 3);
    assert_eq!(
        secondary.call_count(),
        0,
        "the budget is spent re-rolling, never failed over"
    );
    assert_eq!(body["choices"][0]["message"]["content"], "");
    let (_, _, _, _, cost_usd, status) = settled_event(&pool, api_key_id).await;
    assert_eq!(status, 200);
    assert_eq!(
        cost_usd,
        served_sell_cost(),
        "a blank turn returned on the final attempt is a billed turn"
    );
    let (candidate_id, cost_basis_usd, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/primary"));
    assert_eq!(cost_basis_usd, Some(decimal("0.00104")));
    assert_eq!(
        settled_shape_ok(&pool, api_key_id).await,
        Some(false),
        "the shape label is what tells the estimator this turn was blank"
    );
    // The blank turn that IS returned is `ok` and served — the re-roll budget
    // ran out, not the response's validity.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (
                2,
                "openai/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (3, "openai/primary".to_owned(), "ok".to_owned(), true),
        ]
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// Emptiness is a three-way test. A completion carrying only tool calls is a
/// complete answer and must not be re-rolled.
#[tokio::test]
async fn non_streaming_tool_calls_only_completion_is_not_empty() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "toolonly").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Chat {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "shell".to_owned(),
                arguments: r#"{"command":"pwd"}"#.to_owned(),
                extra_content: None,
            }],
            usage: Some(served_usage()),
            reasoning_content: None,
            stop_reason: None,
        }],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(solo.call_count(), 1, "tool calls are output, not emptiness");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "shell"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(settled_shape_ok(&pool, api_key_id).await, Some(true));
}

/// A candidate whose error carries a 4xx status token is abandoned after a
/// single call: no backoff is spent and no retry is burnt on a condition that
/// cannot resolve.
#[tokio::test]
async fn non_streaming_non_retryable_error_moves_on_without_burning_retries() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "nonretryable").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::Failure("401 Unauthorized")]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(primary.call_count(), 1, "a 4xx is not retried");
    assert_eq!(secondary.call_count(), 1);
    let (candidate_id, _, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("anthropic/secondary"));
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The 429 short-circuit is gated on there being somewhere else to go. On a
/// one-candidate route a rate limit is retried like any other transient
/// failure, spending the whole budget.
#[tokio::test]
async fn non_streaming_rate_limit_on_a_single_candidate_route_burns_the_full_budget() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "solo-429").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::RateLimited,
            FakeOutcome::RateLimited,
            FakeOutcome::RateLimited,
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "upstream_unavailable");

    assert_eq!(
        solo.call_count(),
        3,
        "with nowhere to fail over to, a 429 is retried"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The retry budget is per candidate, not per walk: a candidate abandoned after
/// one call does not shorten the next candidate's budget.
#[tokio::test]
async fn non_streaming_the_second_candidate_gets_its_own_retry_budget() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "own-budget").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::Failure("401 Unauthorized")]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    state.wait_for_background_tasks().await;

    assert_eq!(primary.call_count(), 1);
    assert_eq!(
        secondary.call_count(),
        3,
        "the second candidate starts from a full budget"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// Two candidates on the SAME upstream provider are still two candidates. The
/// budget, and any transient penalty, is keyed by candidate — a provider-keyed
/// one would let the first rung's failures shorten the second's.
#[tokio::test]
async fn non_streaming_two_candidates_on_one_provider_each_get_a_full_budget() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "twin").await;
    let twin_a = FakeModelProvider::new(
        "twin-a",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let twin_b = FakeModelProvider::new(
        "twin-b",
        vec![FakeOutcome::chat("hello from twin b", served_usage())],
    );
    let state = router_with_catalog(
        pool.clone(),
        vec![twin_a.clone(), twin_b.clone()],
        twin_tier_config_path(),
    );

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-twin", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(twin_a.call_count(), 3);
    assert_eq!(twin_b.call_count(), 1);
    let (candidate_id, _, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("anthropic/twin-b"));
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// Retries are spaced, not spun. The schedule is 500ms then 1000ms with no
/// sleep after the final attempt, so three calls cost at least 1.5s.
///
/// A LOWER bound only: the walk clock is `std::time::Instant`, which
/// `tokio::time::pause` does not mock, so any park on socket I/O auto-advances
/// the mocked clock to the next armed timer. Equality here would be a flake.
#[tokio::test]
async fn non_streaming_backoff_is_spent_between_retries() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "backoff").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);
    tokio::time::pause();
    let started = tokio::time::Instant::now();

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    // Real time for the DB assertion below — see
    // `non_streaming_timeout_releases_the_reservation_without_charge` for why.
    // Elapsed only grows from here, and the bound is a floor, so measuring
    // after the resume stays sound.
    tokio::time::resume();

    assert_eq!(primary.call_count(), 3);
    assert!(
        started.elapsed() >= Duration::from_millis(1_500),
        "500ms + 1000ms of backoff must be spent, saw {:?}",
        started.elapsed()
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// A context-window rejection is recovered in place: the oldest half of the
/// non-system history is dropped and the SAME candidate is called again with
/// the shortened prompt.
#[tokio::test]
async fn non_streaming_context_window_error_truncates_and_retries() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "ctxtruncate").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Failure("maximum context length exceeded"),
            FakeOutcome::chat("hello from a shorter prompt", served_usage()),
        ],
    );
    let secondary = FakeModelProvider::new("secondary", Vec::new());
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let mut body = completion_body("zero/test-pair", false);
    body["messages"] = json!([
        { "role": "user", "content": "one" },
        { "role": "assistant", "content": "two" },
        { "role": "user", "content": "three" },
    ]);
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let calls = primary.calls();
    assert_eq!(calls.len(), 2, "truncation consumes an attempt");
    assert_eq!(calls[0].message_count, 3);
    assert_eq!(
        calls[1].message_count, 2,
        "the oldest half of the non-system history is dropped"
    );
    assert_eq!(secondary.call_count(), 0);
    let (candidate_id, _, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/primary"));
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// One error can be BOTH: a TPM rejection reading
/// `429 Too Many Requests: token limit exceeded` matches the context-window
/// hints (`token limit exceeded`) and the rate-limit check (`429` + `limit`) at
/// once. The walk owes it one truncation and then owes it nothing.
///
/// The second occurrence must be read as the live 429 it is and move on, not as
/// a context window the walk has already tried to repair. Reading it as the
/// latter costs a 500ms wait and a THIRD dispatch to a rung the upstream has
/// refused twice — pure COGS, on every candidate, invisible in the response.
///
/// The ledger is the second half of the same point: both abandoned attempts are
/// labelled `rate_limited`, because migration 0004 documents that column as
/// what feeds the health cooldown and a 429 the router happened to have a
/// repair for is still a 429.
#[tokio::test]
async fn non_streaming_a_rate_limited_context_window_truncates_once_then_moves_on() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "ctx429").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Failure("429 Too Many Requests: token limit exceeded"),
            FakeOutcome::Failure("429 Too Many Requests: token limit exceeded"),
            FakeOutcome::chat("a third call nobody should pay for", served_usage()),
        ],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let mut body = completion_body("zero/test-pair", false);
    body["messages"] = json!([
        { "role": "user", "content": "one" },
        { "role": "assistant", "content": "two" },
        { "role": "user", "content": "three" },
    ]);
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let calls = primary.calls();
    assert_eq!(
        calls.len(),
        2,
        "the repair is owed once; the second refusal is a 429 and ends the rung"
    );
    assert_eq!(calls[0].message_count, 3);
    assert_eq!(
        calls[1].message_count, 2,
        "the one truncation still happens — the class is not degraded early"
    );
    assert_eq!(secondary.call_count(), 1);
    let (candidate_id, _, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("anthropic/secondary"));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (
                2,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (3, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ]
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// A context-window rejection with nothing left to drop aborts the WHOLE walk.
/// It does not fall through to a candidate with a larger context window —
/// making it do so would be a resilience change with no baseline to measure.
#[tokio::test]
async fn non_streaming_irreducible_context_window_aborts_the_whole_walk() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "ctxirreducible").await;
    let primary =
        FakeModelProvider::new("primary", vec![FakeOutcome::Failure("prompt is too long")]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("never reached", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "upstream_unavailable");

    assert_eq!(primary.call_count(), 1, "there is nothing to truncate");
    assert_eq!(
        secondary.call_count(),
        0,
        "an irreducible prompt ends the walk rather than moving on"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The upstream deadline is a property of the REQUEST, not of a candidate: it
/// ends the walk where it stands instead of counting as this candidate's
/// failure and handing the next candidate a fresh budget.
#[tokio::test]
async fn non_streaming_deadline_ends_the_walk_instead_of_falling_through() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "deadline-walk").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![FakeOutcome::Stall(Duration::from_secs(20 * 60))],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("never reached", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);
    tokio::time::pause();

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    // Real time for the DB assertions — see
    // `non_streaming_timeout_releases_the_reservation_without_charge` for why.
    tokio::time::resume();
    assert_eq!(body["error"]["code"], "upstream_timeout");

    assert_eq!(primary.call_count(), 1);
    assert_eq!(
        secondary.call_count(),
        0,
        "the deadline is the request's, so there is no budget left to hand on"
    );
    let (_, _, input_tokens, output_tokens, cost_usd, status) =
        settled_event(&pool, api_key_id).await;
    assert_eq!(
        (input_tokens, output_tokens, cost_usd, status),
        (0, 0, Decimal::ZERO, 504)
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// `dispatched_at` on a reservation whose walk is in flight right now.
///
/// Read while the upstream is still stalling, because that is the only moment
/// it is observable: a request that finishes settles, and the settle destroys
/// the row this column lives on.
async fn dispatch_marker(pool: &PgPool, api_key_id: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT dispatched_at FROM usage_reservations WHERE api_key_id = $1",
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("dispatch marker must query")
}

/// Wait for the fire-and-forget marker to land on an in-flight reservation.
/// Polled, not awaited: the request path never blocks on this write, which is
/// the whole reason it is safe on the hot path.
async fn await_dispatch_marker(pool: &PgPool, api_key_id: Uuid, walk: &str) {
    for _ in 0..500 {
        if dispatch_marker(pool, api_key_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the {walk} walk dispatched upstream without marking its reservation");
}

/// The dispatch marker, proven on the real request path rather than at the
/// database seam: by the time an upstream has been called, the reservation
/// already says so.
///
/// This is the fact the expiry sweep's classification rests on. Without it a
/// reservation whose request was dispatched but whose settlement intent never
/// landed is byte-identical to one that never left admission, and the sweep
/// reclaims it — releasing the encumbrance on inference the customer received
/// and leaving no record that anything was owed (sol review #3).
///
/// Both walks are exercised because they dispatch at different sites: the
/// buffered walk inside the `select!` that races the shutdown token, the
/// streaming walk immediately before the event stream is built.
#[tokio::test]
async fn a_walk_marks_its_reservation_dispatched_before_the_upstream_answers() {
    let Some(pool) = connect().await else {
        return;
    };

    // --- the buffered walk ------------------------------------------------
    let (buffered_key_id, buffered_key) = create_funded_key(&pool, "marker-buffered").await;
    let solo = FakeModelProvider::new("solo", vec![FakeOutcome::Stall(Duration::from_secs(60))]);
    let state = router(pool.clone(), vec![solo.clone()]);
    let inflight = tokio::spawn(app(state.clone()).oneshot(completion_request(
        &buffered_key,
        &completion_body("zero/test-solo", false),
    )));
    while solo.call_count() == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    await_dispatch_marker(&pool, buffered_key_id, "buffered").await;

    // Drain the deploy to end the stall; the reservation settles and goes,
    // which is also the proof that the marker never blocks a terminal.
    state.begin_shutdown();
    let response = inflight
        .await
        .expect("request task should not panic")
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    state.wait_for_background_tasks().await;
    assert_eq!(open_reservations(&pool, buffered_key_id).await, 0);

    // --- the streaming walk -----------------------------------------------
    let (stream_key_id, stream_key) = create_funded_key(&pool, "marker-stream").await;
    let streaming_solo =
        FakeModelProvider::new("solo", vec![FakeOutcome::Stall(Duration::from_secs(60))]);
    let stream_state = router(pool.clone(), vec![streaming_solo.clone()]);
    // SSE headers return before the walk resolves, so the response arrives
    // while the upstream is still stalling. Holding it keeps the client
    // connected, so nothing settles until the drain below.
    let stream_response = app(stream_state.clone())
        .oneshot(completion_request(
            &stream_key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(stream_response.status(), StatusCode::OK);
    while streaming_solo.call_count() == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    await_dispatch_marker(&pool, stream_key_id, "streaming").await;

    stream_state.begin_shutdown();
    stream_state.wait_for_background_tasks().await;
    drop(stream_response);
    assert_eq!(open_reservations(&pool, stream_key_id).await, 0);
}

/// A drained deploy releases the reservation without a charge: nothing reached
/// the customer, so nothing is billed, and the reservation must not be left for
/// the TTL sweep.
#[tokio::test]
async fn non_streaming_shutdown_releases_the_reservation_without_charge() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "shutdown-sync").await;
    let solo = FakeModelProvider::new("solo", vec![FakeOutcome::Stall(Duration::from_secs(60))]);
    let state = router(pool.clone(), vec![solo.clone()]);

    let request = completion_request(&key, &completion_body("zero/test-solo", false));
    let inflight = tokio::spawn(app(state.clone()).oneshot(request));
    // Cancel only once a candidate is genuinely in flight, so the test pins the
    // mid-dispatch terminal rather than racing the walk's first line.
    while solo.call_count() == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    state.begin_shutdown();

    let response = inflight
        .await
        .expect("request task should not panic")
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "server_shutting_down");

    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            503,
        )
    );
    // A call was in flight when the drain began, so it burnt an upstream
    // request and is recorded as aborted. Cancelled before dispatch, there
    // would be no row and the sentinel would still be the honest answer.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "openai/solo".to_owned(), "aborted".to_owned(), false)]
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// A retry that eventually succeeds is still the FIRST candidate's success.
/// Attribution is what prices the request's COGS, so a walk that mislabelled a
/// retried primary as a fallback would settle the wrong cost basis — 0.00156
/// instead of 0.00104 here.
#[tokio::test]
async fn non_streaming_attribution_survives_a_retry_on_the_primary() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "retry-attrib").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::chat("hello on the third try", served_usage()),
        ],
    );
    let secondary = FakeModelProvider::new("secondary", Vec::new());
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "openai");
    assert_eq!(header(&response, "x-zerorouter-model"), "upstream/primary");
    state.wait_for_background_tasks().await;

    assert_eq!(primary.call_count(), 3);
    assert_eq!(secondary.call_count(), 0);
    let (upstream_provider, upstream_model, _, _, cost_usd, status) =
        settled_event(&pool, api_key_id).await;
    assert_eq!(upstream_provider, "openai");
    assert_eq!(upstream_model, "upstream/primary");
    assert_eq!(cost_usd, served_sell_cost());
    assert_eq!(status, 200);
    let (candidate_id, cost_basis_usd, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("openai/primary"));
    assert_eq!(
        cost_basis_usd,
        Some(decimal("0.00104")),
        "the retried primary's own basis, not the secondary's"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// An upstream that answers but reports no usage is the one shape where the
/// buffered path used to bill an ESTIMATE: a byte-length input bound plus the
/// whole requested output bound, for a completion this branch then throws away.
/// On this fixture tier that was $0.024795 against a real completion's
/// $0.00312 — eight times the price of the answer, for no answer.
///
/// The policy is metered actuals only, on every path. The exact twin of
/// `synthetic_stream_without_upstream_usage_bills_nothing`.
#[tokio::test]
async fn non_streaming_success_without_upstream_usage_bills_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "unmetered-sync").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Chat {
            text: Some("an answer nobody is charged for".to_owned()),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
            stop_reason: None,
        }],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "metering_unavailable");
    assert!(
        !body.to_string().contains("an answer nobody is charged for"),
        "the unmetered completion is discarded, not returned: {body}"
    );

    assert_eq!(solo.call_count(), 1);
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "openai".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        ),
        "an unmetered turn settles at zero, naming the candidate that produced it"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "nothing metered, nothing billed"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    // The call is on the record as `ok` — it completed — but NOT served: the
    // body was discarded. That is what keeps the gap detector below silent
    // while `log_metering_gap` fires, and it is the whole reason `served`
    // tracks possession rather than completion.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "openai/solo".to_owned(), "ok".to_owned(), false)]
    );
    // The gap detector stays silent: nothing reached the customer, so no
    // attempt on this request is `served` and the zero-token row is not a
    // delivery ZeroRouter failed to bill.
    assert_eq!(unbilled_served_requests(&pool, api_key_id).await, 0);
}

/// Each candidate starts its backoff schedule over. A walk that carried the
/// interval across candidates would have the last rung waiting eight times as
/// long as the first for no reason the upstream ever gave it: 500ms then 1000ms
/// per candidate here, not 500/1000 followed by 2000/4000.
#[tokio::test]
async fn non_streaming_each_candidate_starts_its_backoff_schedule_over() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "backoff-reset").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);
    tokio::time::pause();

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    state.wait_for_background_tasks().await;
    // Real time for the DB assertion below — see
    // `non_streaming_timeout_releases_the_reservation_without_charge` for why.
    tokio::time::resume();

    assert_eq!(primary.call_count(), 3);
    assert_eq!(secondary.call_count(), 3);
    assert_eq!(backoff_steps_ms(&primary), [500, 1_000]);
    assert_eq!(
        backoff_steps_ms(&secondary),
        [500, 1_000],
        "a schedule carried across candidates would wait 2s then 4s here"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// A drained router must not be held open by a backoff it is only waiting out
/// to be polite to an upstream. Here the upstream asks for twenty seconds and
/// the walk honors it — `Retry-After` lengthens the wait, capped at thirty
/// seconds — so a shutdown that did not race the sleep would keep this request,
/// its reservation, and the drain itself waiting the full twenty.
#[tokio::test]
async fn non_streaming_shutdown_during_a_backoff_releases_the_reservation_without_charge() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "shutdown-backoff").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::Failure("429 Too Many Requests; Retry-After: 20"),
            FakeOutcome::Failure("429 Too Many Requests; Retry-After: 20"),
            FakeOutcome::Failure("429 Too Many Requests; Retry-After: 20"),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let request = completion_request(&key, &completion_body("zero/test-solo", false));
    let inflight = tokio::spawn(app(state.clone()).oneshot(request));
    while solo.call_count() == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // Real time, deliberately: what this pins is how long a drain waits, and
    // the outcome is identical either way — a sleep that ran to completion
    // would meet the shutdown at the next dispatch instead and settle exactly
    // the same row, twenty seconds later. Latency IS the property here.
    let drained_at = std::time::Instant::now();
    state.begin_shutdown();

    let response = inflight
        .await
        .expect("request task should not panic")
        .expect("completion request should complete");
    let drain_took = drained_at.elapsed();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert_eq!(body["error"]["code"], "server_shutting_down");

    assert!(
        drain_took < Duration::from_secs(5),
        "a backoff that ignored shutdown would hold the drain for the upstream's \
         full twenty seconds; this took {drain_took:?}"
    );
    assert_eq!(
        solo.call_count(),
        1,
        "the walk stops in the backoff rather than finishing it"
    );
    // No `aborted` row: nothing was in flight, so no upstream call was burnt by
    // the drain. The 429 that started the backoff is on the record.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(
            1,
            "openai/solo".to_owned(),
            "rate_limited".to_owned(),
            false
        )]
    );
    let (_, _, input_tokens, output_tokens, cost_usd, status) =
        settled_event(&pool, api_key_id).await;
    assert_eq!(
        (input_tokens, output_tokens, cost_usd, status),
        (0, 0, Decimal::ZERO, 503)
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// Cross-request health (stage 2b, design "Provider-health state"; ordering
/// since stage 3a): a 429 sets a 60-second cooldown keyed
/// `(provider, upstream_model)`, and the very next request through the same
/// router sinks the cooling rung to the back of its route
/// (`order_candidates`), walking straight to the healthy rung. The walk
/// ledger records the walk that happened — one served position, no skip row,
/// because the demoted rung was never a position of this walk. Stage 2b
/// pinned the interim shape (a recorded `health_skipped` at position 1);
/// before 2b, the walk kept no state at all and dispatched the 429'd rung
/// again.
#[tokio::test]
async fn non_streaming_a_rate_limited_rung_sinks_behind_the_healthy_rung_for_the_next_request() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "cooldown-sync-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "cooldown-sync-2").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![
            FakeOutcome::chat("hello from secondary", served_usage()),
            FakeOutcome::chat("hello from secondary", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("first completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");

    // The second request walks the same tier through the same router process,
    // inside the rate-limited rung's cooldown window.
    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("second completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");
    state.wait_for_background_tasks().await;

    assert_eq!(
        primary.call_count(),
        1,
        "a cooling rung is not dispatched to again"
    );
    assert_eq!(secondary.call_count(), 2);
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [(1, "anthropic/secondary".to_owned(), "ok".to_owned(), true)],
        "the demoted rung sank out of the walk entirely: one position, served"
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

/// The streaming twin of the test above, because health lands on both walks
/// together or not at all: a 429-shaped stream failure cools the rung for
/// the streaming walk exactly as a buffered 429 does, and the next request's
/// route is reordered before the streaming walk starts.
#[tokio::test]
async fn streaming_a_rate_limited_rung_sinks_behind_the_healthy_rung_for_the_next_request() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "cooldown-stream-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "cooldown-stream-2").await;
    let served_stream = || {
        FakeOutcome::Stream(vec![
            FakeStreamStep::text("served"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])
    };
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new("secondary", vec![served_stream(), served_stream()]);
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-pair", true),
        ))
        .await
        .expect("first stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");

    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-pair", true),
        ))
        .await
        .expect("second stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");
    state.wait_for_background_tasks().await;

    assert_eq!(
        primary.call_count(),
        1,
        "a cooling rung is not dispatched to again"
    );
    assert_eq!(secondary.call_count(), 2);
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [(1, "anthropic/secondary".to_owned(), "ok".to_owned(), true)],
        "the demoted rung sank out of the walk entirely: one position, served"
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

/// A 429-shaped stream failure is recorded as the rate limit it was, not as a
/// generic broken stream. Migration 0004 documents `outcome` as what feeds the
/// health cooldown, and the streaming walk is one of the two paths that has to
/// feed it — a `stream_error` label here would leave a rate-limited rung
/// invisible to health while the buffered walk could see it.
#[tokio::test]
async fn streaming_a_rate_limited_stream_failure_is_labelled_as_the_429_it_was() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "stream-429-label").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("served"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");
    assert_eq!(primary.call_count(), 1, "a stream 429 is still not retried");
    assert_eq!(secondary.call_count(), 1);
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![
            (
                1,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (2, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ]
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The synthetic-stream sibling: a candidate that cannot stream fails its
/// buffered call with a 429, and the row says so. The label comes from the
/// same classifier the buffered walk dispatches on; only the label is taken —
/// the walk still moves on after one call rather than retrying.
#[tokio::test]
async fn synthetic_stream_a_rate_limited_chat_failure_is_labelled_as_the_429_it_was() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "synthetic-429-label").await;
    let primary = FakeModelProvider::without_streaming("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("served"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");
    assert_eq!(primary.call_count(), 1);
    assert_eq!(secondary.call_count(), 1);
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![
            (
                1,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (2, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ]
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

/// The EWMA half of demotion: three availability failures on one request push
/// the rung's error EWMA past 0.5 (0.3 → 0.51 → 0.657), so the next request
/// sinks it behind the healthy rung without a 429 ever having been involved.
#[tokio::test]
async fn non_streaming_an_error_heavy_rung_sinks_behind_the_healthy_rung_for_the_next_request() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "ewma-sync-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "ewma-sync-2").await;
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![
            FakeOutcome::chat("hello from secondary", served_usage()),
            FakeOutcome::chat("hello from secondary", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("first completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");

    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("second completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");
    state.wait_for_background_tasks().await;

    assert_eq!(
        primary.call_count(),
        3,
        "all three dispatches belong to the first request's retry budget"
    );
    assert_eq!(secondary.call_count(), 2);
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [(1, "anthropic/secondary".to_owned(), "ok".to_owned(), true)]
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

/// The never-below-one-candidate floor on a solo route: health may not skip a
/// rung when doing so would leave the walk with nothing to dispatch, so a
/// cooling solo rung is still tried — and its success both serves the request
/// and ends the cooldown early.
#[tokio::test]
async fn non_streaming_a_cooling_solo_rung_is_still_dispatched() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "solo-cooling-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "solo-cooling-2").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::RateLimited,
            FakeOutcome::RateLimited,
            FakeOutcome::RateLimited,
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("first completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    // The rung is cooling, but it is also the only rung there is.
    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("second completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(
        solo.call_count(),
        4,
        "the cooldown does not starve a solo route"
    );
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [(1, "openai/solo".to_owned(), "ok".to_owned(), true)],
        "no health_skipped row: the guard dispatched rather than skipped"
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

/// The same floor on a multi-candidate route where EVERY rung is cooling: the
/// walk records a skip for each rung it can afford to lose and dispatches the
/// last one rather than exhausting without an upstream call.
#[tokio::test]
async fn non_streaming_a_walk_of_cooling_rungs_still_dispatches_the_last() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "all-cooling-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "all-cooling-2").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![
            FakeOutcome::RateLimited,
            FakeOutcome::chat("hello from secondary", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("first completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("second completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");
    state.wait_for_background_tasks().await;

    assert_eq!(primary.call_count(), 1);
    assert_eq!(secondary.call_count(), 2);
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "health_skipped".to_owned(),
                false
            ),
            (2, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ],
        "the walk skips what it can afford to and dispatches what it cannot"
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

/// Health is keyed `(provider, upstream_model)`, not by provider alone: a
/// demoted rung must not drag its provider-mate down with it. Both twin
/// candidates name `together`; only the model that actually failed sinks.
#[tokio::test]
async fn non_streaming_a_demoted_rung_does_not_demote_its_provider_mate() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "twin-health-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "twin-health-2").await;
    let twin_a = FakeModelProvider::new(
        "twin-a",
        vec![
            FakeOutcome::Transport,
            FakeOutcome::Transport,
            FakeOutcome::Transport,
        ],
    );
    let twin_b = FakeModelProvider::new(
        "twin-b",
        vec![
            FakeOutcome::chat("hello from twin b", served_usage()),
            FakeOutcome::chat("hello from twin b", served_usage()),
        ],
    );
    let state = router_with_catalog(
        pool.clone(),
        vec![twin_a.clone(), twin_b.clone()],
        twin_tier_config_path(),
    );

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-twin", false),
        ))
        .await
        .expect("first completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);

    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-twin", false),
        ))
        .await
        .expect("second completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(twin_a.call_count(), 3);
    assert_eq!(twin_b.call_count(), 2);
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [(1, "anthropic/twin-b".to_owned(), "ok".to_owned(), true)],
        "the verdict follows the upstream model, not the provider name"
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

/// The streaming twin of the solo-route floor, for the same reason the
/// cooldown tests come in pairs: a cooling solo rung is still dispatched by a
/// streaming walk, which also proves an all-skipped streaming walk can never
/// strand its reservation — the guard makes at least one dispatch happen, so
/// every streaming terminal keeps a candidate to settle against.
#[tokio::test]
async fn streaming_a_cooling_solo_rung_is_still_dispatched() {
    let Some(pool) = connect().await else {
        return;
    };
    let (first_key_id, first_key) = create_funded_key(&pool, "solo-cooling-stream-1").await;
    let (second_key_id, second_key) = create_funded_key(&pool, "solo-cooling-stream-2").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::RateLimited,
            FakeOutcome::Stream(vec![
                FakeStreamStep::text("served"),
                FakeStreamStep::Usage(served_usage()),
                FakeStreamStep::Final,
            ]),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &first_key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("first stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    assert_eq!(
        chunks
            .last()
            .expect("an error chunk should terminate the stream")["error"]["code"],
        "upstream_unavailable"
    );

    let response = app(state.clone())
        .oneshot(completion_request(
            &second_key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("second stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");
    state.wait_for_background_tasks().await;

    assert_eq!(
        solo.call_count(),
        2,
        "the cooldown does not starve a solo route"
    );
    assert_eq!(
        attempt_rows(&pool, second_key_id).await,
        [(1, "openai/solo".to_owned(), "ok".to_owned(), true)],
        "no health_skipped row: the guard dispatched rather than skipped"
    );
    assert_eq!(open_reservations(&pool, first_key_id).await, 0);
    assert_eq!(open_reservations(&pool, second_key_id).await, 0);
}

// ---------------------------------------------------------------------------
// Stage 3a: the priority knob, visibility-only (design doc: "The priority
// knob"). The knob is accepted from three carriers, resolved by precedence,
// and recorded on every settled row; ordering stays the identity in every
// mode until the estimator ships (3b).
// ---------------------------------------------------------------------------

/// The frozen control group: a request that never mentions the knob resolves
/// `balanced` and still records it — migration 0004 documents NULL as "row
/// predates the knob", so a post-knob row must never write NULL.
#[tokio::test]
async fn a_request_without_the_knob_records_balanced() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-default").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    // The attempts header is additive and rides every served response; the
    // BODY of a knob-less request stays byte-identical, which is the
    // backward-compatibility anchor — no `zerorouter` key at all, not a null.
    assert_eq!(header(&response, "x-zerorouter-attempts"), "1");
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;
    assert!(
        !body
            .as_object()
            .expect("body is an object")
            .contains_key("zerorouter"),
        "a request that never engaged the knob keeps the legacy response shape: {body}"
    );

    assert_eq!(
        settled_priority(&pool, api_key_id).await,
        Some("balanced".to_owned())
    );
}

/// The model-suffix carrier: `zero/test-solo:cost` resolves the stripped
/// name, records the carried priority, and every surface that names the model
/// — the settled tier and the response `model` field — reads the stripped
/// name, exactly as `usage_events.tier` is specified to.
#[tokio::test]
async fn a_priority_suffix_is_stripped_carried_and_recorded() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-suffix").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-attempts"), "1");
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(body["model"], "zero/test-solo");
    // Engaging the knob through any carrier attaches the response block:
    // resolved priority, the walk story, and the (null until validators
    // exist) declared-validator verdict.
    assert_eq!(body["zerorouter"]["priority"], "cost");
    assert_eq!(
        body["zerorouter"]["attempts"][0]["candidate"],
        "openai/solo"
    );
    assert_eq!(body["zerorouter"]["attempts"][0]["outcome"], "ok");
    assert!(body["zerorouter"]["attempts"][0]["latency_ms"].is_i64());
    assert!(body["zerorouter"]["validated"].is_null());
    // Stage 3b: an unmeasured segment's estimate is the cold byte-bound
    // answer — the request's own max_tokens, labeled as such.
    assert_eq!(body["zerorouter"]["estimate"]["basis"], "cold");
    assert_eq!(
        body["zerorouter"]["estimate"]["output_tokens_p50"],
        MAX_TOKENS
    );
    assert_eq!(
        body["zerorouter"]["estimate"]["output_tokens_p90"],
        MAX_TOKENS
    );
    assert_eq!(
        settled_priority(&pool, api_key_id).await,
        Some("cost".to_owned())
    );
    assert_eq!(settled_tier(&pool, api_key_id).await, "zero/test-solo");
}

/// The typed carrier: `zerorouter.priority` is consumed by serde before the
/// unknown-field flatten, so the same request that was 400-rejected as an
/// unsupported extension before the knob now resolves and records.
#[tokio::test]
async fn the_typed_zerorouter_object_carries_a_priority() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-typed").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let mut body = completion_body("zero/test-solo", false);
    body["zerorouter"] = json!({ "priority": "success" });
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(
        settled_priority(&pool, api_key_id).await,
        Some("success".to_owned())
    );
}

/// Typed field and suffix disagreeing is a client bug, refused loudly before
/// anything is reserved or dispatched — precedence is for filling gaps, not
/// for silently picking a winner between two explicit contradictory asks.
#[tokio::test]
async fn a_typed_priority_disagreeing_with_the_suffix_is_refused_before_admission() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-conflict").await;
    let solo = FakeModelProvider::new("solo", vec![]);
    let state = router(pool.clone(), vec![solo.clone()]);

    let mut body = completion_body("zero/test-solo:cost", false);
    body["zerorouter"] = json!({ "priority": "success" });
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "priority_conflict");

    assert_eq!(solo.call_count(), 0);
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    assert_eq!(settled_count(&pool, api_key_id).await, 0);

    // The same two carriers AGREEING is not a conflict: redundancy is fine,
    // contradiction is not.
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    let mut body = completion_body("zero/test-solo:cost", false);
    body["zerorouter"] = json!({ "priority": "cost" });
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    assert_eq!(
        settled_priority(&pool, api_key_id).await,
        Some("cost".to_owned())
    );
}

/// The per-key default is the weakest carrier: it governs a bare request, and
/// any request-level carrier overrides it.
#[tokio::test]
async fn a_key_default_priority_governs_bare_requests_and_yields_to_the_request() {
    let Some(pool) = connect().await else {
        return;
    };
    let (bare_key_id, bare_key) = create_funded_key(&pool, "knob-key-default-bare").await;
    let (typed_key_id, typed_key) = create_funded_key(&pool, "knob-key-default-typed").await;
    for key_id in [bare_key_id, typed_key_id] {
        query("UPDATE api_keys SET default_priority = 'cost' WHERE id = $1")
            .bind(key_id)
            .execute(&pool)
            .await
            .expect("key default must update");
    }
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &bare_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("bare completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = completion_body("zero/test-solo", false);
    body["zerorouter"] = json!({ "priority": "success" });
    let response = app(state.clone())
        .oneshot(completion_request(&typed_key, &body))
        .await
        .expect("typed completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(
        settled_priority(&pool, bare_key_id).await,
        Some("cost".to_owned()),
        "a bare request takes the key default"
    );
    assert_eq!(
        settled_priority(&pool, typed_key_id).await,
        Some("success".to_owned()),
        "a request-level carrier overrides the key default"
    );
}

/// ZeroRouter's own namespace is strictly validated: a typo'd field or an
/// unknown priority value inside `zerorouter` is a loud 400, never a silently
/// ignored no-op — while the object's absence stays perfectly legal.
#[tokio::test]
async fn garbage_inside_the_zerorouter_object_is_a_loud_400() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-strict").await;
    let solo = FakeModelProvider::new("solo", vec![]);
    let state = router(pool.clone(), vec![solo.clone()]);

    for zerorouter in [
        json!({ "priorty": "cost" }),
        json!({ "priority": "fast" }),
        json!({ "priority": "Balanced" }),
    ] {
        let mut body = completion_body("zero/test-solo", false);
        body["zerorouter"] = zerorouter.clone();
        let response = app(state.clone())
            .oneshot(completion_request(&key, &body))
            .await
            .expect("completion request should complete");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{zerorouter} must be refused"
        );
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "invalid_request");
    }
    assert_eq!(solo.call_count(), 0);
    assert_eq!(settled_count(&pool, api_key_id).await, 0);
}

/// Resolve-first fall-through: a priority suffix cannot conjure a model that
/// does not exist, and a colon segment that is not a priority keyword is not
/// a carrier at all — both land on the same 404 an unknown model always got.
#[tokio::test]
async fn a_suffix_never_invents_a_model() {
    let Some(pool) = connect().await else {
        return;
    };
    let (_api_key_id, key) = create_funded_key(&pool, "knob-404").await;
    let solo = FakeModelProvider::new("solo", vec![]);
    let state = router(pool.clone(), vec![solo.clone()]);

    for model in ["zero/nope:cost", "zero/test-solo:turbo", ":cost"] {
        let response = app(state.clone())
            .oneshot(completion_request(&key, &completion_body(model, false)))
            .await
            .expect("completion request should complete");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{model} must not resolve"
        );
    }
    assert_eq!(solo.call_count(), 0);
}

/// The streaming twin of the suffix test: the resolved priority reaches the
/// settled row through the streaming walk's terminals too, the stream's
/// chunks carry the stripped model name, and — because SSE headers left
/// before the walk resolved — the response block rides the final usage chunk
/// for exactly the clients that opted into usage. A knob-less stream's usage
/// chunk stays byte-identical.
#[tokio::test]
async fn streaming_carries_the_block_on_the_usage_chunk_and_records_the_priority() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-stream").await;
    let (bare_key_id, bare_key) = create_funded_key(&pool, "knob-stream-bare").await;
    let served_stream = || {
        FakeOutcome::Stream(vec![
            FakeStreamStep::text("served"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])
    };
    let solo = FakeModelProvider::new("solo", vec![served_stream(), served_stream()]);
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "served");
    assert_eq!(chunks[1]["model"], "zero/test-solo");
    let usage_chunk = chunks.last().expect("stream ends with the usage chunk");
    assert_eq!(usage_chunk["usage"]["prompt_tokens"], 1_000);
    assert_eq!(usage_chunk["zerorouter"]["priority"], "cost");
    assert_eq!(
        usage_chunk["zerorouter"]["attempts"][0]["candidate"],
        "openai/solo"
    );
    assert_eq!(usage_chunk["zerorouter"]["attempts"][0]["outcome"], "ok");
    assert!(usage_chunk["zerorouter"]["validated"].is_null());
    assert_eq!(usage_chunk["zerorouter"]["estimate"]["basis"], "cold");
    assert_eq!(
        usage_chunk["zerorouter"]["estimate"]["output_tokens_p50"],
        MAX_TOKENS
    );

    let response = app(state.clone())
        .oneshot(completion_request(
            &bare_key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("bare stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    let usage_chunk = chunks.last().expect("stream ends with the usage chunk");
    assert_eq!(usage_chunk["usage"]["prompt_tokens"], 1_000);
    assert!(
        !usage_chunk
            .as_object()
            .expect("usage chunk is an object")
            .contains_key("zerorouter"),
        "a knob-less stream keeps its legacy usage chunk: {usage_chunk}"
    );
    state.wait_for_background_tasks().await;

    assert_eq!(
        settled_priority(&pool, api_key_id).await,
        Some("cost".to_owned())
    );
    assert_eq!(settled_tier(&pool, api_key_id).await, "zero/test-solo");
    assert_eq!(
        settled_priority(&pool, bare_key_id).await,
        Some("balanced".to_owned())
    );
}

/// The walk story in the block is the whole walk, skips and failures
/// included: a rate-limited first rung appears beside the serving rung, and
/// the attempts header counts both — the customer-visible mirror of the
/// `request_attempts` rows the same walk settled.
#[tokio::test]
async fn the_response_block_tells_the_whole_walk_story() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-walk-story").await;
    let primary = FakeModelProvider::new("primary", vec![FakeOutcome::RateLimited]);
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![FakeOutcome::chat("hello from secondary", served_usage())],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let mut body = completion_body("zero/test-pair", false);
    body["zerorouter"] = json!({ "priority": "balanced" });
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-attempts"), "2");
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(body["zerorouter"]["priority"], "balanced");
    let attempts = body["zerorouter"]["attempts"]
        .as_array()
        .expect("attempts is an array");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["candidate"], "openai/primary");
    assert_eq!(attempts[0]["outcome"], "rate_limited");
    assert_eq!(attempts[1]["candidate"], "anthropic/secondary");
    assert_eq!(attempts[1]["outcome"], "ok");
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "openai/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (2, "anthropic/secondary".to_owned(), "ok".to_owned(), true),
        ],
        "the block and the ledger are the same story"
    );
}

/// The synthetic-stream serve path — a non-streaming candidate replayed as
/// SSE — attaches the same block to its usage chunk as a live stream does.
#[tokio::test]
async fn synthetic_stream_carries_the_block_on_the_usage_chunk() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "knob-synthetic").await;
    let solo = FakeModelProvider::without_streaming(
        "solo",
        vec![FakeOutcome::chat("whole answer", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:success", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    let usage_chunk = chunks.last().expect("stream ends with the usage chunk");
    assert_eq!(usage_chunk["usage"]["prompt_tokens"], 1_000);
    assert_eq!(usage_chunk["zerorouter"]["priority"], "success");
    assert_eq!(
        usage_chunk["zerorouter"]["attempts"][0]["candidate"],
        "openai/solo"
    );
    assert_eq!(
        settled_priority(&pool, api_key_id).await,
        Some("success".to_owned())
    );
}

// ---------------------------------------------------------------------------
// Stage 3b: the estimator read path. Cost mode orders by expected cost basis
// once its segment warms; balanced stays the frozen control group; everything
// stays cache-only on the request path (tests drive the refresher through
// the synchronous testing seam).
// ---------------------------------------------------------------------------

/// Seed one settled row into a segment's trailing window, shaped for the
/// estimator scan. `candidate` NULL feeds only the candidate-agnostic cell
/// (the shared fallback); a concrete id feeds that candidate's selection
/// cell as well. Backdated one hour: far inside the estimator's 14-day
/// window, safely outside the per-minute velocity sum — seeding a verbose
/// segment must not spend the test key's own velocity budget.
async fn seed_segment_row(
    pool: &PgPool,
    api_key_id: Uuid,
    signature: &str,
    candidate: Option<&str>,
    output_tokens: i32,
) {
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status, task_signature, task_signature_scheme,
            candidate_id, ts
        )
        VALUES ($1, $2, 'zero/test-pricier-first', 'fireworks', 'upstream/seed',
                100, 0, $3, 0.001, 10, 200, $4, $5, $6,
                NOW() - INTERVAL '1 hour')
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(api_key_id)
    .bind(output_tokens)
    .bind(signature)
    .bind(TASK_SIGNATURE_SCHEME)
    .bind(candidate)
    .execute(pool)
    .await
    .expect("segment seed row must insert");
}

/// The segment key the serve path stamped on this key's settled rows — the
/// exact signature a same-shaped request will look up, without the test
/// re-deriving the hash.
async fn settled_signature(pool: &PgPool, api_key_id: Uuid) -> String {
    query_scalar::<_, String>(
        "SELECT task_signature FROM usage_events WHERE api_key_id = $1 LIMIT 1",
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("settled signature must query")
}

/// The 3b flip end to end: a cold segment keeps today's table order, a warm
/// segment reorders cost mode by expected cost basis — cheapest rates first,
/// because the candidate-agnostic p50 is every rung's shared expected output
/// — and balanced traffic on the same warm segment still walks the table
/// order. The frozen control group stays frozen.
#[tokio::test]
async fn cost_mode_reorders_by_expected_cost_once_the_segment_warms() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "estimator-cost-flip").await;
    let pricier = FakeModelProvider::new(
        "pricier",
        vec![
            FakeOutcome::chat("hello from pricier", served_usage()),
            FakeOutcome::chat("hello from pricier", served_usage()),
        ],
    );
    let cheaper = FakeModelProvider::new(
        "cheaper",
        vec![FakeOutcome::chat("hello from cheaper", served_usage())],
    );
    let state = router(pool.clone(), vec![pricier.clone(), cheaper.clone()]);

    // Cold segment: cost mode falls through to the table order, so the
    // pricier-but-first rung serves — bit-for-bit today's behavior.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("cold cost request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "openai");
    state.wait_for_background_tasks().await;

    // Warm the segment: enough settled rows to clear the n >= 50 gate, then
    // one refresher batch — the synchronous twin of the production loop.
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    state.refresh_estimator_once().await;

    // Warm segment, cost mode: both rungs price at the shared p50, so the
    // ordering is rate order and the cheaper rung now serves.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("warm cost request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-zerorouter-provider"),
        "anthropic",
        "a warm segment orders cost mode cheapest-first"
    );
    let body = json_body(response).await;
    assert_eq!(
        body["zerorouter"]["estimate"]["basis"], "learned",
        "a warm segment's estimate is the measured one"
    );
    assert_eq!(body["zerorouter"]["estimate"]["output_tokens_p50"], 200);

    // Same warm segment, balanced: identity, exactly as before the flip.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first", false),
        ))
        .await
        .expect("balanced request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-zerorouter-provider"),
        "openai",
        "balanced stays the frozen control group"
    );
    // And the legacy shape survives a WARM cache: byte-stability is pinned
    // cold elsewhere, but production segments warm from other traffic — a
    // knob-less body must stay free of the zerorouter key even then.
    let body = json_body(response).await;
    assert!(
        !body
            .as_object()
            .expect("body is an object")
            .contains_key("zerorouter"),
        "a knob-less request keeps the legacy body even on a warm segment: {body}"
    );
    state.wait_for_background_tasks().await;

    assert_eq!(pricier.call_count(), 2);
    assert_eq!(cheaper.call_count(), 1);
}

/// Staleness closes the loop the other way: a warm cell past its TTL answers
/// cold again, so cost mode falls back to the table order until the
/// refresher re-measures the segment.
#[tokio::test]
async fn a_stale_segment_falls_back_to_table_order_until_remeasured() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "estimator-stale").await;
    let pricier = FakeModelProvider::new(
        "pricier",
        vec![
            FakeOutcome::chat("hello from pricier", served_usage()),
            FakeOutcome::chat("hello from pricier", served_usage()),
        ],
    );
    let cheaper = FakeModelProvider::new(
        "cheaper",
        vec![
            FakeOutcome::chat("hello from cheaper", served_usage()),
            FakeOutcome::chat("hello from cheaper", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![pricier.clone(), cheaper.clone()]);

    // Warm the segment through a first request (which also enqueues the
    // cells), then confirm the reorder is in force.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("first request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("warm request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "anthropic");

    // Cross the TTL without touching the clock: the cells stale out, the
    // next cost request keeps table order, and the lookup re-enqueued the
    // segment for the next refresher pass.
    state.age_estimator_cells(Duration::from_secs(6 * 60));
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("stale request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-zerorouter-provider"),
        "openai",
        "a stale segment is a cold segment"
    );
    // One refresher pass re-measures exactly what the stale lookups
    // enqueued, and the reorder returns — the full cold → warm → stale →
    // re-warmed cycle on one router.
    state.refresh_estimator_once().await;
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("re-warmed request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-zerorouter-provider"),
        "anthropic",
        "a re-measured segment reorders again"
    );
    state.wait_for_background_tasks().await;

    assert_eq!(pricier.call_count(), 2);
    assert_eq!(cheaper.call_count(), 2);
}

/// A rung's OWN warm cell overrides the shared fallback. The seeding gives
/// the table-first (pricier-rate) rung a terse measured history and the
/// cheap-rate rung a verbose one, so per-candidate expected cost inverts the
/// rate order: cost mode keeps serving the pricier-rate rung. The `learned`
/// basis proves the segment was warm — under the shared fallback alone, a
/// warm segment would have flipped to the cheap-rate rung, so serving
/// `fireworks` warm is only explainable by the per-candidate cells.
#[tokio::test]
async fn a_rungs_own_cell_overrides_the_shared_fallback_in_cost_mode() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "estimator-own-cell").await;
    let pricier = FakeModelProvider::new(
        "pricier",
        vec![
            FakeOutcome::chat("hello from pricier", served_usage()),
            FakeOutcome::chat("hello from pricier", served_usage()),
        ],
    );
    let cheaper = FakeModelProvider::new("cheaper", vec![]);
    let state = router(pool.clone(), vec![pricier.clone(), cheaper.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("cold request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-zerorouter-provider"), "openai");
    state.wait_for_background_tasks().await;

    // Terse history for the pricier-rate rung, verbose for the cheap-rate
    // rung: 100 tokens at 4 $/mtok beats 50k tokens at 2 $/mtok.
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, Some("openai/pricier"), 100).await;
        seed_segment_row(
            &pool,
            api_key_id,
            &signature,
            Some("anthropic/cheaper"),
            50_000,
        )
        .await;
    }
    assert_eq!(
        state.estimator_pending_len(),
        3,
        "sig + two candidate cells queued"
    );
    state.refresh_estimator_once().await;
    assert_eq!(
        state.estimator_pending_len(),
        0,
        "refresh must consume the queue"
    );

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pricier-first:cost", false),
        ))
        .await
        .expect("warm request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-zerorouter-provider"),
        "openai",
        "the rung's own terse measurement outweighs its pricier rates"
    );
    let body = json_body(response).await;
    assert_eq!(
        body["zerorouter"]["estimate"]["basis"], "learned",
        "warm segment — so the identity outcome above is the sort's verdict, \
         not the cold fallback's"
    );
    state.wait_for_background_tasks().await;

    assert_eq!(pricier.call_count(), 2);
    assert_eq!(cheaper.call_count(), 0);
}

/// The learned estimate rides the streaming usage chunk too. Stream-ness is
/// part of the task signature, so the segment is warmed through a streamed
/// request's own settled signature — a non-streaming warm-up would warm a
/// different segment entirely.
#[tokio::test]
async fn streaming_shows_the_learned_estimate_once_the_segment_warms() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "estimator-stream-warm").await;
    let served_stream = || {
        FakeOutcome::Stream(vec![
            FakeStreamStep::text("served"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])
    };
    let solo = FakeModelProvider::new("solo", vec![served_stream(), served_stream()]);
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", true),
        ))
        .await
        .expect("cold stream should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    let usage_chunk = chunks.last().expect("stream ends with the usage chunk");
    assert_eq!(usage_chunk["zerorouter"]["estimate"]["basis"], "cold");
    state.wait_for_background_tasks().await;

    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", true),
        ))
        .await
        .expect("warm stream should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let chunks = sse_chunks(response).await;
    let usage_chunk = chunks.last().expect("stream ends with the usage chunk");
    assert_eq!(
        usage_chunk["zerorouter"]["estimate"]["basis"], "learned",
        "a warm segment's estimate reaches streaming customers in-band"
    );
    assert_eq!(
        usage_chunk["zerorouter"]["estimate"]["output_tokens_p50"],
        200
    );
    state.wait_for_background_tasks().await;

    // Stage 4: the streaming request sized learned too — sizing is decided
    // before the paths split, and this pins it.
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(reserved, Some(1_024));
    assert_eq!(basis.as_deref(), Some("learned"));
}

/// The refresher's failure arm, pinned as the design records it: a failed
/// scan re-enqueues its cell for the next pass instead of stranding it cold
/// until TTL. Forced by closing the pool under the router before running the
/// batch.
#[tokio::test]
async fn a_failed_refresh_re_enqueues_its_cells() {
    let Some(pool) = connect().await else {
        return;
    };
    let (_api_key_id, key) = create_funded_key(&pool, "estimator-refresh-error").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::chat("hello from solo", served_usage())],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    // A cost-mode request enqueues its segment cell and its one candidate
    // cell.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let queued = state.estimator_pending_len();
    assert_eq!(queued, 2, "one signature cell + one candidate cell");

    // Every scan in the batch now fails; each failed cell must come back.
    pool.close().await;
    state.refresh_estimator_once().await;
    assert_eq!(
        state.estimator_pending_len(),
        queued,
        "failed scans re-enqueue rather than strand their cells"
    );
}

// ---------------------------------------------------------------------------
// Stage 4: learned reservations. The estimator's first dollar of consequence
// — sizing at admission, provenance on the row, the clamp holding the
// customer harmless, and the dollar-denominated auto-revert.
// ---------------------------------------------------------------------------

/// Reservation provenance of this key's LATEST settled row:
/// `(reserved_output_tokens, estimator_basis, reserved_cost_usd, cost_usd)`.
async fn settled_reservation(
    pool: &PgPool,
    api_key_id: Uuid,
) -> (Option<i32>, Option<String>, Option<Decimal>, Decimal) {
    query_as(
        r#"
        SELECT reserved_output_tokens, estimator_basis, reserved_cost_usd, cost_usd
        FROM usage_events
        WHERE api_key_id = $1
        ORDER BY ts DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("settled reservation must query")
}

/// One learned-basis settled row with an explicit cost/reservation gap — the
/// clamp-loss evidence the auto-revert evaluator aggregates. Backdated an
/// hour like every other seed.
async fn seed_loss_row(
    pool: &PgPool,
    api_key_id: Uuid,
    signature: &str,
    cost_usd: &str,
    reserved_cost_usd: &str,
) {
    seed_loss_row_aged(pool, api_key_id, signature, cost_usd, reserved_cost_usd, 1).await;
}

/// A loss row backdated `age_hours` — for placing evidence inside or outside
/// specific trailing windows.
async fn seed_loss_row_aged(
    pool: &PgPool,
    api_key_id: Uuid,
    signature: &str,
    cost_usd: &str,
    reserved_cost_usd: &str,
    age_hours: i32,
) {
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status, task_signature, task_signature_scheme,
            estimator_basis, reserved_cost_usd, ts
        )
        VALUES ($1, $2, 'zero/test-solo', 'deepinfra', 'upstream/seed',
                100, 0, 100, $3::NUMERIC, 10, 200, $4, $5,
                'learned', $6::NUMERIC, NOW() - ($7 * INTERVAL '1 hour'))
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(api_key_id)
    .bind(cost_usd)
    .bind(signature)
    .bind(TASK_SIGNATURE_SCHEME)
    .bind(reserved_cost_usd)
    .bind(age_hours)
    .execute(pool)
    .await
    .expect("loss seed row must insert");
}

/// Eligible segments reserve the learned bound and stamp it: the floor when
/// the segment is terse, the p99 × 1.25 when it binds — and a BARE balanced
/// request on the same warm segment sizes learned too, because eligibility
/// excludes only the escalation-capable, not the knob-less.
#[tokio::test]
async fn an_eligible_segment_reserves_the_learned_bound_and_stamps_it() {
    let Some(pool) = connect().await else {
        return;
    };
    // Terse segment: p99 × 1.25 = 250 loses to the 1024 floor.
    let (floor_key_id, floor_key) = create_funded_key(&pool, "learned-floor").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &floor_key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("cold request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, floor_key_id).await;
    assert_eq!(
        reserved,
        Some(4_096),
        "a cold segment reserves the byte bound"
    );
    assert_eq!(basis.as_deref(), Some("cold"));

    let signature = settled_signature(&pool, floor_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, floor_key_id, &signature, None, 200).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &floor_key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warm request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, reserved_cost, _) = settled_reservation(&pool, floor_key_id).await;
    assert_eq!(
        reserved,
        Some(1_024),
        "the floor binds: max(250, 0.25 × 4096) = 1024"
    );
    assert_eq!(basis.as_deref(), Some("learned"));
    assert!(reserved_cost.is_some());

    // The bare twin: no knob anywhere, same segment, still learned-sized —
    // eligibility excludes success mode, not the knob-less.
    let response = app(state.clone())
        .oneshot(completion_request(
            &floor_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("bare request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, floor_key_id).await;
    assert_eq!(reserved, Some(1_024));
    assert_eq!(basis.as_deref(), Some("learned"));

    // Verbose segment on a second user: p99 × 1.25 = 2500 beats the floor.
    let (p99_key_id, p99_key) = create_funded_key(&pool, "learned-p99").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &p99_key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("cold request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, p99_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, p99_key_id, &signature, None, 2_000).await;
    }
    state.refresh_estimator_once().await;
    let response = app(state.clone())
        .oneshot(completion_request(
            &p99_key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warm request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, p99_key_id).await;
    assert_eq!(
        reserved,
        Some(2_500),
        "p99 × 1.25 binds between floor and cap"
    );
    assert_eq!(basis.as_deref(), Some("learned"));
}

/// Success mode keeps the byte bound outright, even on a warm segment: the
/// escalation-capable cohort is exactly where a whole-segment p99
/// under-reserves most.
#[tokio::test]
async fn success_mode_keeps_the_byte_bound_even_on_a_warm_segment() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-success").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:success", false),
        ))
        .await
        .expect("success-mode request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(reserved, Some(4_096));
    assert_eq!(basis.as_deref(), Some("cold"));
}

/// A heavy-tailed segment never leaves cold sizing — while its estimate
/// stays learned in the response, because the tail gate guards reservation
/// money, not visibility (recorded 3b decision).
#[tokio::test]
async fn a_heavy_tailed_segment_never_leaves_cold_sizing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-tail").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    // p50 = 10, p99 = 10_000: ratio far past 8.
    for _ in 0..55 {
        seed_segment_row(&pool, api_key_id, &signature, None, 10).await;
    }
    for _ in 0..5 {
        seed_segment_row(&pool, api_key_id, &signature, None, 10_000).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("tail-gated request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body["zerorouter"]["estimate"]["basis"], "learned",
        "the tail gate guards money, not visibility"
    );
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(reserved, Some(4_096));
    assert_eq!(basis.as_deref(), Some("cold"));
}

/// The payoff the rollout row names: a tighter bound directly buys
/// admissible velocity headroom. Under a cap the byte bound cannot fit, the
/// cold request 429s and the learned request admits.
#[tokio::test]
async fn a_learned_reservation_reclaims_velocity_headroom() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-velocity").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }

    // A cap the byte bound cannot fit (recent settled usage + input bound +
    // 4096) but the learned bound can (… + 1024).
    query("UPDATE api_keys SET velocity_cap_tokens_per_min = 4000 WHERE id = $1")
        .bind(api_key_id)
        .execute(&pool)
        .await
        .expect("velocity cap must update");

    // Still cold (no refresh yet): the byte bound blows the cap.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("cold request should complete");
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the byte bound cannot fit the cap"
    );

    state.refresh_estimator_once().await;
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("learned request should complete");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the learned bound admits under the same cap: reclaimed headroom"
    );
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(reserved, Some(1_024));
    assert_eq!(basis.as_deref(), Some("learned"));
}

/// Under-reservation is ZeroRouter's tail, not the customer's: a served
/// completion that outruns its learned reservation bills the metered
/// actuals on the ROW but debits only the reserved ceiling — the clamp-loss
/// row the auto-revert aggregates.
#[tokio::test]
async fn an_under_reserved_row_clamps_the_debit_and_records_the_loss() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-clamp").await;
    let overrun = TokenUsage {
        input_tokens: Some(1_000),
        cached_input_tokens: Some(0),
        output_tokens: Some(4_000),
    };
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", overrun),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("overrun request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let (reserved, basis, reserved_cost, cost_usd) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(reserved, Some(1_024));
    assert_eq!(basis.as_deref(), Some("learned"));
    let reserved_cost = reserved_cost.expect("learned row snapshots its ceiling");
    assert!(
        cost_usd > reserved_cost,
        "the overrun outran its reservation: {cost_usd} vs {reserved_cost}"
    );
    // The customer's balance moved by the CLAMPED amount — the first row's
    // metered cost plus this row's reserved ceiling, never its actuals.
    // Exclude the backdated estimator seeds: only the two SERVED rows moved
    // the ledger.
    let first_row_cost = query_scalar::<_, Decimal>(
        r#"
        SELECT cost_usd FROM usage_events
        WHERE api_key_id = $1 AND upstream_model <> 'upstream/seed'
        ORDER BY ts ASC, id ASC LIMIT 1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("first row must query");
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50) - first_row_cost - reserved_cost,
        "the debit is clamped at the reservation; the overrun is ZeroRouter's loss"
    );
}

/// The dollar-denominated segment revert: one row past the single-row limit
/// flips the segment back to cold sizing at the next evaluation — while the
/// response estimate keeps showing the learned percentiles, because display
/// is measurement and sizing is policy.
#[tokio::test]
async fn a_lossy_segment_reverts_to_cold_sizing_while_its_estimate_stays_learned() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-revert-segment").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    // One clamp-loss row past the $1 single-row limit.
    seed_loss_row(&pool, api_key_id, &signature, "1.50", "0.10").await;
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("reverted request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body["zerorouter"]["estimate"]["basis"], "learned",
        "display is measurement; sizing is policy"
    );
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(reserved, Some(4_096), "the reverted segment sizes cold");
    assert_eq!(basis.as_deref(), Some("cold"));
}

/// The per-user aggregate: enough trailing-30d loss on one segment reverts
/// EVERY segment of the user — re-slicing traffic cannot escape it because
/// segments are user-scoped.
#[tokio::test]
async fn a_lossy_user_reverts_every_segment() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-revert-user").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    // Segment A (max_tokens 4096) and segment B (max_tokens 2048): different
    // buckets, different signatures, one user.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("segment A request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body_b = completion_body("zero/test-solo:cost", false);
    body_b["max_tokens"] = json!(2_048);
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body_b))
        .await
        .expect("segment B request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let signatures = query_scalar::<_, String>(
        "SELECT DISTINCT task_signature FROM usage_events WHERE api_key_id = $1",
    )
    .bind(api_key_id)
    .fetch_all(&pool)
    .await
    .expect("signatures must query");
    assert_eq!(signatures.len(), 2, "two request shapes, two segments");
    let signature_b = settled_signature_for_max(&pool, api_key_id, 2_048).await;
    let signature_a = signatures
        .iter()
        .find(|signature| **signature != signature_b)
        .expect("segment A signature")
        .clone();

    // The seeded losses are settled cost the spend-cap gate would count;
    // lift the cap so the test exercises the revert, not the cap.
    query("UPDATE api_keys SET spend_cap_usd = 1000 WHERE id = $1")
        .bind(api_key_id)
        .execute(&pool)
        .await
        .expect("spend cap must update");
    // Segment A: $60 of trailing loss — far past the $50 user aggregate.
    for _ in 0..40 {
        seed_loss_row(&pool, api_key_id, &signature_a, "1.55", "0.05").await;
    }
    // Segment B: perfectly healthy and warm.
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature_b, None, 200).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(&key, &body_b))
        .await
        .expect("segment B request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(
        reserved,
        Some(2_048),
        "segment B sizes cold — its own health cannot outvote its user's losses"
    );
    assert_eq!(basis.as_deref(), Some("cold"));
}

/// The segment key of this key's settled row with the given requested
/// max_tokens — for telling one user's segments apart.
async fn settled_signature_for_max(pool: &PgPool, api_key_id: Uuid, max_tokens: i32) -> String {
    query_scalar::<_, String>(
        r#"
        SELECT task_signature FROM usage_events
        WHERE api_key_id = $1 AND requested_max_tokens = $2
        LIMIT 1
        "#,
    )
    .bind(api_key_id)
    .bind(max_tokens)
    .fetch_one(pool)
    .await
    .expect("segment signature must query")
}

/// The re-derivation contract, pinned as a restart: a FRESH router (fresh
/// in-process registry — exactly what a deploy leaves behind) whose database
/// holds loss evidence older than the 7-day trigger window but inside the
/// re-derivation window re-fires both reverts from the rows alone. Before
/// the two-window evaluator, this state sized learned: the segment's 7d
/// stats were clean and the user check sat behind a learned-rows gate the
/// reverted user could never satisfy.
#[tokio::test]
async fn standing_evidence_outside_the_trigger_window_still_reverts() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "learned-rederive").await;
    query("UPDATE api_keys SET spend_cap_usd = 1000 WHERE id = $1")
        .bind(api_key_id)
        .execute(&pool)
        .await
        .expect("spend cap must update");
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("hello from solo", served_usage()),
            FakeOutcome::chat("hello from solo", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("warming request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let signature = settled_signature(&pool, api_key_id).await;
    for _ in 0..60 {
        seed_segment_row(&pool, api_key_id, &signature, None, 200).await;
    }
    // Eight-day-old losses: outside the 7-day trigger window, inside the
    // 14-day segment and 37-day user re-derivation windows. $60 total also
    // crosses the user aggregate.
    for _ in 0..40 {
        seed_loss_row_aged(&pool, api_key_id, &signature, "1.55", "0.05", 8 * 24).await;
    }
    state.refresh_estimator_once().await;

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo:cost", false),
        ))
        .await
        .expect("re-derived request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    let (reserved, basis, _, _) = settled_reservation(&pool, api_key_id).await;
    assert_eq!(
        reserved,
        Some(4_096),
        "aged-but-standing evidence re-derives the revert on a fresh registry"
    );
    assert_eq!(basis.as_deref(), Some("cold"));
}

/// The money-conservation soak: many users racing mixed-outcome traffic —
/// successes, transport retries, rate limits, hard failures, real streams,
/// synthetic streams — and afterwards the books must balance exactly. For
/// every user, credits spent equals the sum of that user's settled usage
/// costs to the micro-dollar, no reservation is left open anywhere, and no
/// balance went negative. This is the invariant that lets ZeroClaw adopt
/// the router as a provider: agent workloads are exactly this shape of
/// concurrent, failure-riddled traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn soak_concurrent_mixed_traffic_conserves_money() {
    let Some(pool) = connect().await else {
        return;
    };
    const USERS: usize = 10;
    const REQUESTS_PER_USER: usize = 24;

    // Oversized repeating scripts: outcome order across racing requests is
    // nondeterministic, and retries consume extra entries — conservation
    // must hold regardless of which request drew which outcome.
    let mut primary_script = Vec::new();
    let mut secondary_script = Vec::new();
    let mut solo_script = Vec::new();
    for index in 0..(USERS * REQUESTS_PER_USER * 4) {
        primary_script.push(match index % 6 {
            0 | 1 => FakeOutcome::chat("soak answer", served_usage()),
            2 => FakeOutcome::Transport,
            3 => FakeOutcome::RateLimited,
            4 => FakeOutcome::Stream(vec![
                FakeStreamStep::text("soak "),
                FakeStreamStep::text("stream"),
                FakeStreamStep::Usage(served_usage()),
                FakeStreamStep::Final,
            ]),
            _ => FakeOutcome::Failure("upstream exploded"),
        });
        // The failover rung answers everything, so a walk that abandons the
        // primary still settles deterministically.
        secondary_script.push(FakeOutcome::chat("failover answer", served_usage()));
        solo_script.push(match index % 3 {
            0 => FakeOutcome::chat("solo answer", served_usage()),
            1 => FakeOutcome::Stream(vec![
                FakeStreamStep::text("solo stream"),
                FakeStreamStep::Usage(served_usage()),
                FakeStreamStep::Final,
            ]),
            _ => FakeOutcome::RateLimited,
        });
    }
    let primary = FakeModelProvider::new("primary", primary_script);
    let secondary = FakeModelProvider::new("secondary", secondary_script);
    // No native streaming: streaming requests against zero/test-solo also
    // exercise the synthetic-stream path under the same storm.
    let solo = FakeModelProvider::without_streaming("solo", solo_script);
    let state = router_matching_aliases(
        pool.clone(),
        vec![primary.clone(), secondary.clone(), solo.clone()],
    );

    let mut keys = Vec::new();
    for user in 0..USERS {
        keys.push(create_funded_key(&pool, &format!("soak-{user}")).await);
    }

    let mut workers = Vec::new();
    for (api_key_id, key) in keys.clone() {
        let state = state.clone();
        workers.push(tokio::spawn(async move {
            for request in 0..REQUESTS_PER_USER {
                let streaming = request % 2 == 0;
                let model = if request % 3 == 0 {
                    "zero/test-solo"
                } else {
                    "zero/test-pair"
                };
                let response = app(state.clone())
                    .oneshot(completion_request(&key, &completion_body(model, streaming)))
                    .await
                    .expect("soak request must complete");
                if streaming && response.status() == StatusCode::OK {
                    // Drain the stream so settlement happens.
                    let _ = sse_chunks(response).await;
                }
            }
            api_key_id
        }));
    }
    for worker in workers {
        worker.await.expect("soak worker must not panic");
    }
    state.wait_for_background_tasks().await;

    for (api_key_id, _) in &keys {
        let user_id = user_of(&pool, *api_key_id).await;
        let spent = query_scalar::<_, Decimal>(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events WHERE api_key_id = $1",
        )
        .bind(api_key_id)
        .fetch_one(&pool)
        .await
        .expect("usage sum must query");
        let balance = balance(&pool, user_id).await.expect("balance must query");
        assert_eq!(
            Decimal::from(50) - balance,
            spent,
            "user {user_id}: every debited micro-dollar must be a settled usage row"
        );
        assert!(
            balance >= Decimal::ZERO,
            "user {user_id}: balance must never go negative"
        );
        assert_eq!(
            open_reservations(&pool, *api_key_id).await,
            0,
            "user {user_id}: the storm must leave no reservation open"
        );
    }
}

/// A client that walks away mid-stream is not billed for output it never
/// received. The distinction this pins is the subtle one: an EMPTY answer
/// still bills (the model produced nothing, the customer received exactly
/// that — see
/// `a_stream_that_emitted_nothing_is_not_rescued_by_reported_output_tokens`),
/// while output that bounced off a closed channel does not. Before this,
/// the live streaming path passed the upstream's usage straight to the
/// ledger and charged in full for both.
#[tokio::test]
async fn a_client_that_disconnects_mid_stream_is_not_billed_for_lost_output() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "abandoned").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("output the client will never read"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    // Take the response and drop it without reading the body: the channel
    // closes, so every model-output frame is produced but none is accepted.
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    state.wait_for_background_tasks().await;

    let (_, _, input_tokens, output_tokens, cost_usd, status) =
        settled_event(&pool, api_key_id).await;
    assert_eq!(
        (input_tokens, output_tokens, cost_usd),
        (0, 0, Decimal::ZERO),
        "output that never reached the client is not billed"
    );
    assert_eq!(
        status, 499,
        "the settled row records the client having closed the request"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "the balance is untouched"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    // A disconnect settles through the client-closed terminal, which does
    // not carry the walk ledger — the billing fact this test pins is the
    // zero charge above.
}

// ---------------------------------------------------------------------------
// Real upstream stop reasons, and the countable usage gap (migration 0020)
// ---------------------------------------------------------------------------

/// `(finish_reason, finish_reason_source, usage_gap)` off the settled row —
/// the three columns the finish-reason plumbing writes.
async fn settled_finish(
    pool: &PgPool,
    api_key_id: Uuid,
) -> (Option<String>, Option<String>, Option<String>) {
    query_as(
        r#"
        SELECT finish_reason, finish_reason_source, usage_gap
        FROM usage_events
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("settled finish columns must query")
}

/// Every money column on the settled row, for twin comparison.
async fn settled_money(
    pool: &PgPool,
    api_key_id: Uuid,
) -> (Decimal, Option<Decimal>, i32, i32, i16) {
    query_as(
        r#"
        SELECT cost_usd, cost_basis_usd, input_tokens, output_tokens, status
        FROM usage_events
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("settled money columns must query")
}

/// An upstream that reports its OWN stop reason has that reason persisted, and
/// the row says so: `finish_reason_source = 'upstream'`, the second of the two
/// values migration 0004 reserved and the first time anything writes it.
#[tokio::test]
async fn a_real_upstream_stop_reason_is_persisted_and_labelled_upstream() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "real-finish").await;
    // Output is far under the 4096 ceiling, so the SYNTHESIS would say "stop".
    // The upstream says it clipped on its own, smaller ceiling.
    let solo = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("clipped answer", served_usage())
                .with_stop_reason(zerorouter::provider::StopReason::Length),
        ],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    state.wait_for_background_tasks().await;

    let (finish_reason, source, usage_gap) = settled_finish(&pool, api_key_id).await;
    assert_eq!(
        finish_reason.as_deref(),
        Some("length"),
        "the upstream's own word, not the token arithmetic"
    );
    assert_eq!(
        source.as_deref(),
        Some("upstream"),
        "migration 0004 reserved exactly this token for exactly this change"
    );
    assert_eq!(usage_gap, None, "a buffered completion has no stream gap");

    // The label the success estimator trains on follows the real reason.
    assert_eq!(
        settled_shape_ok(&pool, api_key_id).await,
        Some(false),
        "a real `length` fails the shape check the synthesized `stop` would have passed"
    );

    // And the customer-visible body deliberately still reports the SYNTHESIZED
    // value. Changing what an agent loop reads here changes how many requests
    // it sends, which is a product decision this change does not take.
    assert_eq!(
        body["choices"][0]["finish_reason"], "stop",
        "the response body is deliberately unchanged; the ledger carries the real reason"
    );

    // The shape label is not a billing input: this row still bills in full.
    let (cost_usd, _, input_tokens, output_tokens, status) = settled_money(&pool, api_key_id).await;
    assert_eq!(status, 200);
    assert_eq!((input_tokens, output_tokens), (1_000, 20));
    assert_eq!(
        cost_usd,
        served_sell_cost(),
        "a `length` label does not change what the customer is charged"
    );
}

/// An upstream that reports NOTHING keeps the synthesis, unchanged, and the
/// row still says `'synthetic'` — the absent case must not start claiming
/// ground truth.
#[tokio::test]
async fn an_upstream_that_reports_no_stop_reason_keeps_the_synthesis() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "synthetic-finish").await;
    let solo = FakeModelProvider::new("solo", vec![FakeOutcome::chat("an answer", served_usage())]);
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let (finish_reason, source, usage_gap) = settled_finish(&pool, api_key_id).await;
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        source.as_deref(),
        Some("synthetic"),
        "no real reason means the row must keep saying the value was inferred"
    );
    assert_eq!(usage_gap, None);
}

/// The metered twin: a request whose REAL reason agrees with what the
/// synthesis would have produced must bill byte-identically to one that
/// reported no reason at all. This is the proof that carrying the value moved
/// no money on the agreeing majority of traffic.
#[tokio::test]
async fn a_real_reason_that_agrees_with_the_synthesis_bills_identically() {
    let Some(pool) = connect().await else {
        return;
    };

    // Twin A: upstream reports nothing, synthesis says "stop".
    let (synthetic_key_id, synthetic_key) = create_funded_key(&pool, "twin-synthetic").await;
    let synthetic_upstream =
        FakeModelProvider::new("solo", vec![FakeOutcome::chat("an answer", served_usage())]);
    let state = router(pool.clone(), vec![synthetic_upstream.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &synthetic_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    // Twin B: upstream reports "stop" — the same verdict, from the wire.
    let (real_key_id, real_key) = create_funded_key(&pool, "twin-real").await;
    let real_upstream = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("an answer", served_usage())
                .with_stop_reason(zerorouter::provider::StopReason::Stop),
        ],
    );
    let state = router(pool.clone(), vec![real_upstream.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &real_key,
            &completion_body("zero/test-solo", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    assert_eq!(
        settled_money(&pool, synthetic_key_id).await,
        settled_money(&pool, real_key_id).await,
        "an agreeing real reason must move no money: same charge, same COGS, \
         same tokens, same status"
    );
    assert_eq!(
        settled_shape_ok(&pool, synthetic_key_id).await,
        settled_shape_ok(&pool, real_key_id).await,
        "and the same shape label, since the verdicts agree"
    );

    // The ONE column that differs is the provenance, which is the point.
    let (synthetic_reason, synthetic_source, _) = settled_finish(&pool, synthetic_key_id).await;
    let (real_reason, real_source, _) = settled_finish(&pool, real_key_id).await;
    assert_eq!(synthetic_reason, real_reason, "the value itself agrees");
    assert_eq!(synthetic_source.as_deref(), Some("synthetic"));
    assert_eq!(real_source.as_deref(), Some("upstream"));
}

/// The usage gap becomes a COUNTABLE column rather than a trace line, and
/// counting it costs nobody anything: the row still bills zero, exactly as it
/// did when the label existed only in a log.
#[tokio::test]
async fn the_usage_gap_lands_on_the_settled_row_and_bills_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "usage-gap-countable").await;
    // A stream that delivers output and runs to a terminal carrying the
    // `done_missing` label — the shape a truncating middlebox produces, which
    // must not hide inside the ordinary "this server ignores include_usage".
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("partial"),
            FakeStreamStep::FinalWith(zerorouter::provider::StreamFinal {
                stop_reason: Some(zerorouter::provider::StopReason::Stop),
                usage_gap: Some(zerorouter::provider::UsageGap::DoneMissing),
            }),
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    let (_, _, usage_gap) = settled_finish(&pool, api_key_id).await;
    assert_eq!(
        usage_gap.as_deref(),
        Some("done_missing"),
        "the gap is now countable in SQL instead of only greppable in a log"
    );

    // Inert to billing: this is the same unbilled settle it always was.
    let (cost_usd, _, input_tokens, output_tokens, status) = settled_money(&pool, api_key_id).await;
    assert_eq!(
        (cost_usd, input_tokens, output_tokens, status),
        (Decimal::ZERO, 0, 0, 502),
        "labelling a gap must not change what a gap costs"
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50),
        "an unmetered delivery must not move the balance, labelled or not"
    );
}

/// A stream that reports usage has no gap to record — `usage_gap` is NULL, not
/// a sentinel. NULL has to keep meaning "nothing to say" or a census over the
/// column counts healthy rows as gaps.
#[tokio::test]
async fn a_metered_stream_records_no_usage_gap() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "usage-gap-absent").await;
    let solo = FakeModelProvider::new(
        "solo",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("answer"),
            FakeStreamStep::Usage(TokenUsage {
                input_tokens: Some(1_000),
                output_tokens: Some(20),
                cached_input_tokens: None,
            }),
            FakeStreamStep::FinalWith(zerorouter::provider::StreamFinal {
                stop_reason: Some(zerorouter::provider::StopReason::Stop),
                usage_gap: None,
            }),
        ])],
    );
    let state = router(pool.clone(), vec![solo.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-solo", true),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = sse_chunks(response).await;
    state.wait_for_background_tasks().await;

    let (finish_reason, source, usage_gap) = settled_finish(&pool, api_key_id).await;
    assert_eq!(usage_gap, None, "usage was reported, so there is no gap");
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        source.as_deref(),
        Some("upstream"),
        "a live stream's terminal reason is the upstream's own"
    );
}

// ---------------------------------------------------------------------------
// MULTIMODAL ADMISSION (2026-08-21)
//
// The catalog advertised `image` on most lanes while the compat surface 400'd
// every structured content array, so the advertised capability was
// unreachable. These pin the admission half of closing that: what is accepted,
// what is refused, and — the part that moves money — that a refusal happens
// before any reservation is taken, and that an accepted image is HELD FOR
// against the worst case the upstream may meter for it.
// ---------------------------------------------------------------------------

/// The OpenAI-shape multimodal body, as the official SDKs emit it.
fn multimodal_body(model: &str, stream: bool, image_url: &str) -> Value {
    let mut body = completion_body(model, stream);
    body["messages"] = json!([{
        "role": "user",
        "content": [
            {"type": "text", "text": "what is in this image?"},
            {"type": "image_url", "image_url": {"url": image_url}},
        ],
    }]);
    body
}

#[tokio::test]
async fn an_image_sent_to_a_text_only_lane_is_refused_before_anything_is_reserved() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "modality-gate").await;
    let textonly = FakeModelProvider::new("textonly", vec![]);
    let state = router_matching_aliases(pool.clone(), vec![textonly.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &multimodal_body("zero/test-text-only", false, "https://example.com/x.jpg"),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "modality_unsupported");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["param"], "messages");
    // The message must name the lane and what it DOES take: a caller acts on
    // this by choosing another model, and cannot without both.
    let message = body["error"]["message"].as_str().expect("a message");
    assert!(
        message.contains("zero/test-text-only"),
        "the refusal must name the model: {message}"
    );
    assert!(
        message.contains("text"),
        "the refusal must say what the lane accepts: {message}"
    );

    // The money assertions, which are the point of gating at admission rather
    // than discovering the mismatch as an upstream 400.
    assert_eq!(textonly.call_count(), 0, "no upstream may be dialled");
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
    assert_eq!(settled_count(&pool, api_key_id).await, 0);
}

#[tokio::test]
async fn a_lane_that_declares_image_serves_it_and_one_that_declares_nothing_is_not_refused() {
    let Some(pool) = connect().await else {
        return;
    };
    let (_, key) = create_funded_key(&pool, "modality-served").await;

    // Declared `image`: served.
    let vision = FakeModelProvider::new("vision", vec![FakeOutcome::chat("a cat", served_usage())]);
    let state = router_matching_aliases(pool.clone(), vec![vision.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &multimodal_body("zero/test-vision", false, "https://example.com/x.jpg"),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    assert_eq!(vision.call_count(), 1);

    // Declares NO metadata at all: also served. "Unknown is never a refusal"
    // is not a nicety — several shipped lanes omit `input_modalities` because
    // their two sources contradict each other, and every one of them takes
    // images in reality. Turning silence into a 400 would break them.
    let solo = FakeModelProvider::new("solo", vec![FakeOutcome::chat("a dog", served_usage())]);
    let state = router_matching_aliases(pool.clone(), vec![solo.clone()]);
    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &multimodal_body("zero/test-solo", false, "https://example.com/x.jpg"),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a lane that declares nothing must keep serving everything"
    );
    state.wait_for_background_tasks().await;
    assert_eq!(solo.call_count(), 1);
}

#[tokio::test]
async fn a_text_only_content_array_is_the_same_request_as_the_joined_string() {
    let Some(pool) = connect().await else {
        return;
    };
    let (_, key) = create_funded_key(&pool, "modality-text-array").await;
    let textonly =
        FakeModelProvider::new("textonly", vec![FakeOutcome::chat("hello", served_usage())]);
    let state = router_matching_aliases(pool.clone(), vec![textonly.clone()]);

    let mut body = completion_body("zero/test-text-only", false);
    body["messages"] = json!([{
        "role": "user",
        "content": [{"type": "text", "text": "hello"}],
    }]);
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an array carrying only text asks a lane for nothing a string does not"
    );
    state.wait_for_background_tasks().await;
    assert_eq!(textonly.call_count(), 1);
}

#[tokio::test]
async fn an_image_is_reserved_for_at_its_worst_case_not_its_url_length() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "modality-reserve").await;

    // A text-only request through the same lane, as the control.
    let vision = FakeModelProvider::new("vision", vec![FakeOutcome::chat("plain", served_usage())]);
    let state = router_matching_aliases(pool.clone(), vec![vision.clone()]);
    let mut plain = completion_body("zero/test-vision", false);
    plain["messages"] = json!([{"role": "user", "content": "what is in this image?"}]);
    app(state.clone())
        .oneshot(completion_request(&key, &plain))
        .await
        .expect("completion request should complete");
    state.wait_for_background_tasks().await;
    let (_, _, plain_reserved, _) = settled_reservation(&pool, api_key_id).await;
    let plain_reserved = plain_reserved.expect("a reserved cost is recorded");

    // The same prompt with an image appended by URL. On the wire that adds
    // ~40 bytes; at the upstream it adds thousands of metered tokens, and the
    // reservation has to hold for the second number.
    let vision = FakeModelProvider::new("vision", vec![FakeOutcome::chat("a cat", served_usage())]);
    let state = router_matching_aliases(pool.clone(), vec![vision.clone()]);
    app(state.clone())
        .oneshot(completion_request(
            &key,
            &multimodal_body("zero/test-vision", false, "https://example.com/x.jpg"),
        ))
        .await
        .expect("completion request should complete");
    state.wait_for_background_tasks().await;
    let (_, _, imaged_reserved, imaged_cost) = settled_reservation(&pool, api_key_id).await;
    let imaged_reserved = imaged_reserved.expect("a reserved cost is recorded");

    assert!(
        imaged_reserved > plain_reserved * Decimal::from(2),
        "an image must move the reservation by its worst case, not by the length of its URL: \
         plain reserved {plain_reserved}, imaged reserved {imaged_reserved}"
    );
    // And settlement is untouched: metered actuals only, exactly as for text.
    assert_eq!(
        imaged_cost,
        served_sell_cost(),
        "the hold grew; the CHARGE is still whatever the upstream metered"
    );
}

#[tokio::test]
async fn streaming_carries_a_multimodal_request_end_to_end() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "modality-stream").await;
    let vision = FakeModelProvider::new(
        "vision",
        vec![FakeOutcome::Stream(vec![
            FakeStreamStep::text("a "),
            FakeStreamStep::text("cat"),
            FakeStreamStep::Usage(served_usage()),
            FakeStreamStep::Final,
        ])],
    );
    let state = router_matching_aliases(pool.clone(), vec![vision.clone()]);

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &multimodal_body(
                "zero/test-vision",
                true,
                "data:image/png;base64,iVBORw0KGgo=",
            ),
        ))
        .await
        .expect("stream request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let payloads = sse_payloads(response).await;
    state.wait_for_background_tasks().await;

    assert_eq!(payloads.last().map(String::as_str), Some("[DONE]"));
    let chunks = payloads
        .iter()
        .filter(|payload| payload.as_str() != "[DONE]")
        .map(|payload| serde_json::from_str::<Value>(payload).expect("chunk should be JSON"))
        .collect::<Vec<_>>();
    // Byte-identical framing to the text-only happy path: role primer, two
    // content deltas, the finish delta, the usage chunk. Multimodal input
    // changes what goes UP, and nothing about what comes back.
    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "a ");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "cat");
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[4]["usage"]["completion_tokens"], 20);

    let call = vision.calls().remove(0);
    assert!(call.streaming, "the multimodal request streamed");
    assert_eq!(
        settled_count(&pool, api_key_id).await,
        1,
        "a streamed multimodal request settles exactly like a text one"
    );
}

#[tokio::test]
async fn a_file_part_is_refused_because_no_lane_can_carry_one() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "modality-file").await;
    let vision = FakeModelProvider::new("vision", vec![]);
    let state = router_matching_aliases(pool.clone(), vec![vision.clone()]);

    // Sent to the lane with the WIDEST declared modalities, so the refusal is
    // unambiguously about the part and not about the lane. No upstream this
    // router dials accepts an OpenAI-shape file part on the endpoint it
    // dials, which is why `pdf` was removed from the catalog rather than
    // mapped.
    let mut body = completion_body("zero/test-vision", false);
    body["messages"] = json!([{
        "role": "user",
        "content": [
            {"type": "text", "text": "summarise this"},
            {"type": "file", "file": {"filename": "a.pdf", "file_data": "JVBERi0="}},
        ],
    }]);
    let response = app(state.clone())
        .oneshot(completion_request(&key, &body))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "unsupported_request_fields");
    assert_eq!(vision.call_count(), 0);
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

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
use zeroclaw_providers::traits::{TokenUsage, ToolCall};
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

/// Pooled connections each test opens up front. Two is enough for a request
/// that admits, walks, and settles in sequence while the test reads back rows.
const POOL_CONNECTIONS: u32 = 2;

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
    }
}

/// A router whose candidates are served by `fakes` in tier order. Panics if a
/// resolved route has more candidates than the test scripted, which would
/// silently leave a real (credential-less) upstream in the walk.
fn router(pool: PgPool, fakes: Vec<Arc<FakeModelProvider>>) -> RouterState {
    router_with_catalog(pool, fakes, tier_config_path())
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

async fn open_reservations(pool: &PgPool, api_key_id: Uuid) -> i64 {
    query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("reservation count must query")
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
            "deepinfra".to_owned(),
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
    assert_eq!(header(&response, "x-zerorouter-provider"), "fireworks");
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
            "fireworks".to_owned(),
            "upstream/primary".to_owned(),
            1_000,
            20,
            served_sell_cost(),
            200,
        )
    );
    let (candidate_id, cost_basis_usd, attempt_count, finish_reason) =
        settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("fireworks/primary"));
    // COGS at the candidate's own cost basis: 1000 * $1.00 + 20 * $2.00 per Mtok.
    assert_eq!(cost_basis_usd, Some(decimal("0.00104")));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    // One dispatch, recorded as the served attempt.
    assert_eq!(attempt_count, Some(1));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "fireworks/primary".to_owned(), "ok".to_owned(), true)]
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
    // Today's `ReliableModelProvider` is built with 2 retries and a 500ms base
    // backoff (providers.rs `PROVIDER_RETRIES` / `PROVIDER_BACKOFF_MS`), so a
    // retryable failure costs 3 upstream calls and ~1.5s before the walk moves
    // on. Scripting exactly 3 failures pins that budget: a 4th call would fall
    // off the script and fail the request instead of failing over.
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
    assert_eq!(header(&response, "x-zerorouter-provider"), "together");
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
    assert_eq!(upstream_provider, "together");
    assert_eq!(upstream_model, "upstream/secondary");
    assert_eq!(cost_usd, served_sell_cost(), "the sell rate is the tier's");
    assert_eq!(status, 200);
    let (candidate_id, cost_basis_usd, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("together/secondary"));
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
                "fireworks/primary".to_owned(),
                "upstream_error".to_owned(),
                false
            ),
            (
                2,
                "fireworks/primary".to_owned(),
                "upstream_error".to_owned(),
                false
            ),
            (
                3,
                "fireworks/primary".to_owned(),
                "upstream_error".to_owned(),
                false
            ),
            (4, "together/secondary".to_owned(), "ok".to_owned(), true),
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
    assert_eq!(candidate_id.as_deref(), Some("together/secondary"));
    // The abandoned 429 is on the record, labelled as the rate limit it was
    // rather than as a generic upstream error — the distinction a health
    // estimator needs and the delegated walk never surfaced.
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [
            (
                1,
                "fireworks/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (2, "together/secondary".to_owned(), "ok".to_owned(), true),
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
            "together".to_owned(),
            "upstream/secondary".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    let (candidate_id, cost_basis_usd, attempt_count, _) =
        settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("together/secondary"));
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
                "fireworks/primary".to_owned(),
                "rate_limited".to_owned(),
                false
            ),
            (
                2,
                "together/secondary".to_owned(),
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
    assert_eq!(body["error"]["code"], "upstream_timeout");

    // The non-streaming deadline releases the reservation at zero cost, and so
    // does its streaming sibling — see
    // `streaming_timeout_releases_the_reservation_without_charge`. The row now
    // names the candidate that was in flight when the deadline hit; the
    // delegated walk destroyed that with the dropped future.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "deepinfra".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            504,
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        [(1, "deepinfra/solo".to_owned(), "timeout".to_owned(), false)]
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
    // The streaming response carries no upstream attribution headers: the
    // candidate is not known when the SSE head is written.
    assert_eq!(header(&response, "x-zerorouter-provider"), "");
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
            "deepinfra".to_owned(),
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
    assert_eq!(candidate_id.as_deref(), Some("deepinfra/solo"));
    assert_eq!(cost_basis_usd, Some(decimal("0.00104")));
    assert_eq!(attempt_count, Some(1));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "deepinfra/solo".to_owned(), "ok".to_owned(), true)]
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
    assert_eq!(upstream_provider, "together");
    assert_eq!(cost_usd, served_sell_cost());
    assert_eq!(status, 200);
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![
            (
                1,
                "fireworks/primary".to_owned(),
                "stream_error".to_owned(),
                false
            ),
            (2, "together/secondary".to_owned(), "ok".to_owned(), true),
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
            "deepinfra".to_owned(),
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
        vec![(1, "deepinfra/solo".to_owned(), "ok".to_owned(), true)],
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
            "deepinfra".to_owned(),
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
        vec![(
            1,
            "deepinfra/solo".to_owned(),
            "stream_error".to_owned(),
            true
        )],
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
            "deepinfra".to_owned(),
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
        vec![(
            1,
            "deepinfra/solo".to_owned(),
            "stream_error".to_owned(),
            true
        )],
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
            "deepinfra".to_owned(),
            "upstream/solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            504,
        )
    );
    assert_eq!(
        attempt_rows(&pool, api_key_id).await,
        vec![(1, "deepinfra/solo".to_owned(), "timeout".to_owned(), false)],
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
            "deepinfra".to_owned(),
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
        vec![(1, "deepinfra/solo".to_owned(), "ok".to_owned(), true)]
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
            "deepinfra".to_owned(),
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
        vec![(1, "deepinfra/solo".to_owned(), "ok".to_owned(), false)]
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
            "deepinfra".to_owned(),
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
            "deepinfra".to_owned(),
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
    assert_eq!(candidate_id.as_deref(), Some("fireworks/primary"));
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
                "fireworks/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (
                2,
                "fireworks/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (3, "fireworks/primary".to_owned(), "ok".to_owned(), true),
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
    assert_eq!(candidate_id.as_deref(), Some("fireworks/primary"));
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
                "fireworks/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (
                2,
                "fireworks/primary".to_owned(),
                "validation_failed".to_owned(),
                false
            ),
            (3, "fireworks/primary".to_owned(), "ok".to_owned(), true),
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
    assert_eq!(candidate_id.as_deref(), Some("together/secondary"));
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
    assert_eq!(candidate_id.as_deref(), Some("together/twin-b"));
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
    assert_eq!(candidate_id.as_deref(), Some("fireworks/primary"));
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
            "deepinfra".to_owned(),
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
        [(1, "deepinfra/solo".to_owned(), "aborted".to_owned(), false)]
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
    assert_eq!(header(&response, "x-zerorouter-provider"), "fireworks");
    assert_eq!(header(&response, "x-zerorouter-model"), "upstream/primary");
    state.wait_for_background_tasks().await;

    assert_eq!(primary.call_count(), 3);
    assert_eq!(secondary.call_count(), 0);
    let (upstream_provider, upstream_model, _, _, cost_usd, status) =
        settled_event(&pool, api_key_id).await;
    assert_eq!(upstream_provider, "fireworks");
    assert_eq!(upstream_model, "upstream/primary");
    assert_eq!(cost_usd, served_sell_cost());
    assert_eq!(status, 200);
    let (candidate_id, cost_basis_usd, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id.as_deref(), Some("fireworks/primary"));
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
            "deepinfra".to_owned(),
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
        [(1, "deepinfra/solo".to_owned(), "ok".to_owned(), false)]
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
            "deepinfra/solo".to_owned(),
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

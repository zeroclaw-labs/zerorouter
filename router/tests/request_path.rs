//! Characterization tests for the authenticated request path: `POST
//! /v1/chat/completions` through admission, the candidate walk, and settlement.
//!
//! Every test drives the real axum handler over a real Postgres; only the
//! upstream leaf is swapped, via `RouterState::with_injected_route` and the
//! scriptable fakes in `zerorouter::testing` (both behind the `testing`
//! feature). The catalog is `tests/request_path_tiers.toml`, so these
//! assertions pin router behavior rather than the production candidate list.
//!
//! These tests document what the code does TODAY, ahead of the failover-loop
//! rewrite. Where current behavior is known-wrong but not yet fixable here the
//! actual behavior is still asserted, with a comment saying so — do not "fix"
//! the code to make one of these pass; change the assertion deliberately when
//! the behavior changes. One such assertion is left: the non-streaming walk
//! keeps no attempt ledger, which is blocked on unrolling that walk (see
//! `non_streaming_failover_retries_the_first_candidate_twice_then_bills_the_second`).
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
    // The non-streaming walk keeps no attempt ledger; only the streaming walk
    // records `request_attempts` rows today.
    assert_eq!(attempt_count, None);
    assert!(attempt_rows(&pool, api_key_id).await.is_empty());

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

    // CONFIRMED GAP, blocked on the walk rewrite (api.rs `run_non_streaming`):
    // the three burnt calls on the first candidate leave no trace, because the
    // non-streaming walk is delegated to zeroclaw's `ReliableModelProvider` and
    // the only per-request outcome it surfaces is `ProviderFallbackInfo`
    // (requested/actual provider + model, recorded on success only). Losing
    // attempts exist there solely as a `Vec<String>` of log lines folded into
    // the error on total failure, so no honest ledger row can be reconstructed
    // without unrolling the walk into the router the way the streaming path
    // already does. NULL is the correct value until then: migration 0004
    // documents `attempts_cost_basis_usd` as NULL until the router-owned walk
    // records attempts, and writing a placeholder here would claim the burnt
    // calls cost nothing.
    assert!(attempt_rows(&pool, api_key_id).await.is_empty());
    let attempts_cogs = query_scalar::<_, Option<Decimal>>(
        "SELECT attempts_cost_basis_usd FROM usage_events WHERE api_key_id = $1",
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("attempts COGS must query");
    assert_eq!(attempts_cogs, None);
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
    // No tokens were delivered, so the reservation is released with a
    // zero-cost sentinel row rather than a charge.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "fallback-chain".to_owned(),
            "zero/test-pair".to_owned(),
            0,
            0,
            Decimal::ZERO,
            502,
        )
    );
    let (candidate_id, cost_basis_usd, _, _) = settled_provenance(&pool, api_key_id).await;
    assert_eq!(candidate_id, None, "no candidate was selected");
    assert_eq!(cost_basis_usd, None);
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
    // `streaming_timeout_releases_the_reservation_without_charge`.
    assert_eq!(
        settled_event(&pool, api_key_id).await,
        (
            "fallback-chain".to_owned(),
            "zero/test-solo".to_owned(),
            0,
            0,
            Decimal::ZERO,
            504,
        )
    );
    assert_eq!(
        balance(&pool, user_of(&pool, api_key_id).await)
            .await
            .expect("balance must query"),
        Decimal::from(50)
    );
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
    let state = router(pool.clone(), vec![buffered]);
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

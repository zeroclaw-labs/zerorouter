//! Edge mode, stages 2 and 3 (`docs/design/edge-mode-local-rung.md`): an
//! operator-declared local provider, $0 candidates that route to it, the
//! cost-led ordering that puts them first — and the metering seam that lets a
//! wholly-$0 route skip reserve, the per-user advisory lock, and settle.
//!
//! Its own test binary on purpose. Installing an operator provider inventory is
//! a once-per-process act — the shipped inventory is `include_str!`-embedded and
//! the overlay is a `OnceLock` beside it — so a binary that installs one must be
//! a binary where every test wants it installed. Putting these anywhere else
//! would leak an extra provider into suites that pin the shipped inventory
//! exactly.
//!
//! The stage-3 tests come in pairs, and the pairing is the design. Every claim
//! about what the free lane SKIPS is made again, in the opposite direction, on
//! a metered route — so "the skip happened" and "the metering mechanism still
//! works" fail independently and a reader always knows which one broke. The
//! sharpest of them is
//! `a_free_rung_that_fails_over_to_a_paid_one_is_fully_metered_and_charged`:
//! the reservation is taken at admission, before the walk knows which rung will
//! answer, so a route holding even one paid rung must reserve for it.

use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    RouterState,
    api::InjectedRoute,
    app,
    auth::{generate_api_key, hash_api_key},
    billing::grant_promo,
    config::ResolvedRoute,
    db::migrate,
    load_tier_catalog,
    providers::{
        ProviderBuildError, ProviderCandidate, ProviderRoute, is_supported_provider,
        load_operator_inventory, provider_settles_free,
    },
    testing::{FakeModelProvider, FakeOutcome},
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

/// Install the operator inventory for this process, and prove the ways it can
/// be refused on the way in.
///
/// The negative cases live here rather than in tests of their own because
/// installation happens at most once per process: a refused load leaves the
/// slot empty (validation runs before anything is installed), so they must run
/// BEFORE the successful one, and the only way to guarantee that ordering in a
/// parallel test binary is to make them part of the same once-init.
fn operator_inventory() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let missing = load_operator_inventory(Path::new("no/such/providers.json"))
            .expect_err("a missing inventory file must fail loudly, not silently do nothing");
        assert!(
            matches!(
                missing,
                ProviderBuildError::OperatorInventoryUnreadable { .. }
            ),
            "unexpected error {missing:?}"
        );

        let shadow = load_operator_inventory(&fixture("local_providers_shadow.json"))
            .expect_err("an entry shadowing a shipped provider must be refused");
        assert!(
            shadow.to_string().contains("shadows"),
            "the refusal must say why: {shadow}"
        );

        let count = load_operator_inventory(&fixture("local_providers.json"))
            .expect("the local inventory should load");
        assert_eq!(count, 3);

        let again = load_operator_inventory(&fixture("local_providers.json"))
            .expect_err("a second install must be refused rather than silently ignored");
        assert!(
            matches!(again, ProviderBuildError::OperatorInventoryAlreadyLoaded),
            "unexpected error {again:?}"
        );
    });
}

#[tokio::test]
async fn only_a_provider_that_declares_free_settlement_may_be_free() {
    operator_inventory();

    assert!(is_supported_provider("local-llama"));
    assert!(is_supported_provider("openai"), "shipped providers survive");

    // The free lane is entered by an explicit declaration, never inferred. In
    // particular it is NOT inferred from the adapter: `hosted-zr` is on the
    // same chat-completions wire as the local rungs and bills real money, which
    // is precisely the dual use the design gives that adapter.
    assert!(provider_settles_free("local-llama"));
    assert!(
        provider_settles_free("secure-local"),
        "a token-protected local server is still a local server"
    );
    for metered in ["hosted-zr", "openai", "anthropic", "nothing-like-this"] {
        assert!(
            !provider_settles_free(metered),
            "{metered} bills somebody and must never be free"
        );
    }
}

#[tokio::test]
async fn an_edge_catalog_loads_and_declares_its_local_rung_free() {
    operator_inventory();
    let catalog = load_tier_catalog(&fixture("local_candidates_tiers.toml"))
        .await
        .expect("the edge catalog should load");

    let route = catalog
        .resolve("zero/edge")
        .expect("the edge tier should resolve");
    let ids: Vec<&str> = route
        .candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["openai/edge-burst", "local-llama/qwen3-8b"],
        "resolution keeps the table order; $0-first is a SELECTION rule"
    );
    assert!(!route.candidates[0].is_free());
    assert!(route.candidates[1].is_free());

    // The local rung is addressable by its own id too, like any other pin.
    let pinned = catalog
        .resolve("local-llama/qwen3-8b")
        .expect("the local candidate should resolve by id");
    assert_eq!(pinned.candidates.len(), 1);
    assert_eq!(pinned.candidates[0].model, "qwen3-8b");
}

/// Write a one-tier catalog naming `provider` at `basis`, and try to load it.
async fn load_catalog_pricing(
    name: &str,
    provider: &str,
    basis: &str,
) -> Result<zerorouter::TierCatalog, zerorouter::config::TierConfigError> {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.toml"));
    tokio::fs::write(
        &path,
        format!(
            r#"
schema_version = 1
[tiers."zero/{name}"]
[tiers."zero/{name}".rates]
input_per_mtok = 2.00
output_per_mtok = 10.00
[[tiers."zero/{name}".candidates]]
id = "{provider}/probe"
provider = "{provider}"
model = "probe"
[tiers."zero/{name}".candidates.rates]
{basis}
"#
        ),
    )
    .await
    .expect("the fixture should write");
    load_tier_catalog(&path).await
}

/// The blocker the adversarial review found. `chat_completions` is dual-use by
/// the design's own scope — the keyless local rung AND a credentialed hosted
/// ZeroRouter taking metered burst traffic — so keying "may be $0" on the
/// adapter would have let a real-money upstream be priced at zero and marked
/// free by `is_free`, the key stage 3's metering skip is specified to read.
/// The declaration, not the wire, is what enters the free lane.
#[tokio::test]
async fn a_metered_upstream_on_the_local_wire_may_not_be_priced_free() {
    operator_inventory();
    let error = load_catalog_pricing(
        "metered-chat",
        "hosted-zr",
        "input_per_mtok = 0.00\noutput_per_mtok = 0.00",
    )
    .await
    .expect_err("a $0 basis on a metered upstream must refuse the catalog");
    let detail = error.to_string();
    assert!(detail.contains("hosted-zr"), "{detail}");
    assert!(detail.contains("settlement"), "{detail}");

    // Same provider, priced: perfectly legal, and the shape an edge deployment
    // uses for its cloud burst.
    load_catalog_pricing(
        "metered-priced",
        "hosted-zr",
        "input_per_mtok = 1.00\noutput_per_mtok = 2.00",
    )
    .await
    .expect("a metered upstream priced at what it costs must keep loading");
}

/// The other half of the same decision: credential presence was the WRONG key.
/// A local vLLM behind a bearer token is a normal, common deployment, and it is
/// free because its operator says it is, not because it happens to be
/// unauthenticated.
#[tokio::test]
async fn a_credentialed_local_server_may_still_be_free() {
    operator_inventory();
    let catalog = load_catalog_pricing(
        "credentialed-free",
        "secure-local",
        "input_per_mtok = 0.00\noutput_per_mtok = 0.00",
    )
    .await
    .expect("a credentialed provider declaring free settlement may be priced at zero");
    let route = catalog
        .resolve("zero/credentialed-free")
        .expect("the tier should resolve");
    assert!(
        route.candidates[0].is_free(),
        "declared free settlement plus a $0 price is what the stage-3 skip will read"
    );
}

#[tokio::test]
async fn a_zero_price_on_a_cloud_provider_is_still_refused_with_the_overlay_installed() {
    operator_inventory();
    // Registering a local provider must not open the door for the shipped
    // ones: the rule is per-provider, and installing an inventory that HAS a
    // free-capable provider is exactly the state in which a mistake — or an
    // attempt — would otherwise slip through.
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("zero_priced_cloud.toml");
    tokio::fs::write(
        &path,
        r#"
schema_version = 1
[tiers."zero/sneaky"]
[tiers."zero/sneaky".rates]
input_per_mtok = 2.00
output_per_mtok = 10.00
[[tiers."zero/sneaky".candidates]]
id = "openai/free-lunch"
provider = "openai"
model = "gpt-5.6-sol"
[tiers."zero/sneaky".candidates.rates]
input_per_mtok = 0.00
output_per_mtok = 0.00
"#,
    )
    .await
    .expect("the fixture should write");

    let error = load_tier_catalog(&path)
        .await
        .expect_err("a $0 basis on a cloud provider must refuse the catalog");
    let detail = error.to_string();
    assert!(detail.contains("openai/free-lunch"), "{detail}");
    assert!(detail.contains("settlement"), "{detail}");
}

#[tokio::test]
async fn the_models_endpoint_publishes_the_local_rung_at_zero() {
    operator_inventory();
    let state = RouterState::new(fixture("local_candidates_tiers.toml"));
    let response = app(state)
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("models request should build"),
        )
        .await
        .expect("models request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("models body should be readable")
        .to_bytes();
    let models: Value = serde_json::from_slice(&body).expect("models body should be JSON");
    let rows = models["data"].as_array().expect("data should be an array");
    let row = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} should be listed"))
            .clone()
    };

    // A local model exposed as a pin: published like any other model — owned by
    // the provider that serves it, priced at what a request for it is metered
    // at, carrying what the operator declared it can take. This is the $0 row
    // an edge deployment puts in front of its agents.
    let pinned = row("local-llama/phi4-mini");
    assert_eq!(pinned["owned_by"], "local-llama");
    assert_eq!(pinned["pricing"]["prompt"], "0");
    assert_eq!(pinned["pricing"]["completion"], "0");
    assert_eq!(pinned["pricing"]["input_cache_read"], "0");
    assert_eq!(pinned["context_length"], 128_000);
    assert_eq!(pinned["max_output_tokens"], 4_096);
    assert_eq!(pinned["tool_call"], true);
    assert_eq!(pinned["input_modalities"], json!(["text"]));

    // A $0 rung INSIDE a mixed tier is a different claim, and the listing keeps
    // making the honest one: every candidate advertises its owning tier's sell
    // rate, because that is what a request naming it is actually metered at
    // (`TierCatalog::resolve`). Quoting $0 here would undercut the customer's
    // real price by the width of the tier. A free rung does not make the tier
    // free — it makes ZeroRouter's cost zero on the requests it serves, which
    // is a fact about margin, not about the price list.
    let rung = row("local-llama/qwen3-8b");
    assert_eq!(rung["owned_by"], "local-llama");
    assert_eq!(rung["pricing"]["prompt"], "0.000003");
    assert_eq!(rung["context_length"], 4_096);
    assert_eq!(rung["tool_call"], true);

    // The tier row is unchanged in kind: it advertises the NARROWEST thing its
    // candidates can do, which now means the local rung's window — a customer
    // addressing `zero/edge` can land there, so the small number is the honest
    // one.
    let tier = row("zero/edge");
    assert_eq!(tier["owned_by"], "zerorouter");
    assert_eq!(tier["context_length"], 4_096);
    assert_eq!(tier["input_modalities"], json!(["text"]));
    assert_eq!(tier["pricing"]["prompt"], "0.000003");
}

// ---------------------------------------------------------------------------
// The request path, over a real database. Same harness shape as
// tests/request_path.rs: scripted fakes behind an injected route, so the walk,
// admission, and settlement are all the production ones.
// ---------------------------------------------------------------------------

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

async fn create_funded_key(pool: &PgPool, label: &str) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    sqlx_core::query::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("edge-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    let plaintext = generate_api_key();
    let key_id = Uuid::new_v4();
    sqlx_core::query::query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'edge', 20, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(pool)
    .await
    .expect("test API key must insert");
    grant_promo(pool, user_id, Decimal::from(50), "edge")
        .await
        .expect("funding promo must apply");
    (key_id, plaintext)
}

/// A router over the edge catalog whose candidates are served by `fakes`,
/// matched by the last segment of each candidate id — so one state serves both
/// fixture tiers.
fn router(pool: PgPool, fakes: Vec<Arc<FakeModelProvider>>) -> RouterState {
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
                        .unwrap_or_else(|| panic!("no scripted fake for {}", definition.id))
                        .clone();
                    ProviderCandidate::with_provider(definition, fake)
                })
                .collect(),
        )
    });
    RouterState::with_injected_route(fixture("local_candidates_tiers.toml"), pool, true, route)
}

fn served_usage() -> zerorouter::provider::TokenUsage {
    zerorouter::provider::TokenUsage {
        input_tokens: Some(1_000),
        output_tokens: Some(20),
        cached_input_tokens: None,
    }
}

/// A fake that will answer as many times as any test here dispatches to it.
fn upstream(alias: &str) -> Arc<FakeModelProvider> {
    FakeModelProvider::new(
        alias,
        (0..4)
            .map(|_| FakeOutcome::chat("hello from the edge", served_usage()))
            .collect(),
    )
}

fn completion(model: &str, prompt: &str, tools: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": 1_024,
        "stream": false,
    });
    if tools {
        body["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "shell",
                "description": "run a command",
                "parameters": {"type": "object", "properties": {}}
            }
        }]);
    }
    body
}

/// [`serve`], also returning the request id the response carries — which is
/// the reservation's own id, and so the thread from the response back to the
/// admission that authorized it.
async fn serve_with_request_id(state: &RouterState, key: &str, body: &Value) -> (String, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("completion request should build");
    let response = app(state.clone())
        .oneshot(request)
        .await
        .expect("completion should complete");
    assert_eq!(response.status(), StatusCode::OK, "request should succeed");
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    (header("x-zerorouter-provider"), header("x-request-id"))
}

async fn serve(state: &RouterState, key: &str, body: &Value) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("completion request should build");
    let response = app(state.clone())
        .oneshot(request)
        .await
        .expect("completion should complete");
    assert_eq!(response.status(), StatusCode::OK, "request should succeed");
    response
        .headers()
        .get("x-zerorouter-provider")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The whole stage in one walk: cost-led traffic serves the free rung on a
/// cold estimator, and the existing fallback machinery bursts to cloud the
/// moment the local rung mechanically cannot take the request.
#[tokio::test]
async fn cost_led_traffic_serves_the_free_rung_and_bursts_when_it_cannot() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (_, key) = create_funded_key(&pool, "cost-led").await;
    let qwen = upstream("qwen3-8b");
    let burst = upstream("edge-burst");
    let state = router(
        pool.clone(),
        vec![
            qwen.clone(),
            burst.clone(),
            upstream("gemma3-4b"),
            upstream("toolless-burst"),
            upstream("phi4-mini"),
        ],
    );

    // Cold estimator, cost mode, a prompt that fits: the $0 rung serves even
    // though the table lists the cloud rung first. Nothing has been measured —
    // which is the state every fresh edge deployment is in.
    assert_eq!(
        serve(&state, &key, &completion("zero/edge:cost", "hello", false)).await,
        "local-llama"
    );

    // A prompt past the local rung's declared window bursts to cloud. The
    // bound is a byte bound, so this is comfortably over 4096 either way.
    let long_prompt = "x".repeat(6_000);
    assert_eq!(
        serve(
            &state,
            &key,
            &completion("zero/edge:cost", &long_prompt, false)
        )
        .await,
        "openai",
        "an overflowing prompt must burst rather than truncate on the local rung"
    );

    // Tools the local rung declares it lacks burst too, and the same tier
    // serves locally without them.
    assert_eq!(
        serve(
            &state,
            &key,
            &completion("zero/edge-toolless:cost", "hello", true)
        )
        .await,
        "openai"
    );
    assert_eq!(
        serve(
            &state,
            &key,
            &completion("zero/edge-toolless:cost", "hello", false)
        )
        .await,
        "local-llama"
    );

    // Balanced is untouched: the table order is the operator's own statement of
    // preference and $0-first is a cost-mode rule.
    assert_eq!(
        serve(&state, &key, &completion("zero/edge", "hello", false)).await,
        "openai",
        "balanced stays the frozen control group"
    );

    state.wait_for_background_tasks().await;
    assert_eq!(qwen.call_count(), 1);
    assert_eq!(burst.call_count(), 2);
}

/// Poll for the free lane's asynchronous observability row.
///
/// The row is written off the response path — spawned, never awaited — so a
/// test that read straight after the response would be racing the write it is
/// asserting on. Polling is the honest shape for an eventually-consistent
/// write; a `wait_for_background_tasks` would not help, because the task
/// deliberately does not ride the request's tracker (the request is over).
async fn await_usage_rows(pool: &PgPool, api_key_id: Uuid, expected: i64) {
    for _ in 0..200 {
        let rows: i64 = sqlx_core::query_scalar::query_scalar(
            "SELECT COUNT(*) FROM usage_events WHERE api_key_id = $1",
        )
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("usage row count must query");
        if rows >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the free lane's usage row never appeared");
}

/// Every column the settle transaction fills in and nothing else can.
struct MeteringEvidence {
    request_id: Uuid,
    cost_usd: Decimal,
    status: i16,
    reserved_output_tokens: Option<i32>,
    reserved_cost_usd: Option<Decimal>,
    estimator_basis: Option<String>,
    task_signature: Option<String>,
    input_tokens: i32,
    output_tokens: i32,
}

/// The row shape [`metering_evidence`] reads, named so the tuple stays legible
/// (and so clippy's complexity gate has something to point at).
type EvidenceRow = (
    Uuid,
    Decimal,
    i16,
    Option<i32>,
    Option<Decimal>,
    Option<String>,
    Option<String>,
    i32,
    i32,
);

async fn metering_evidence(pool: &PgPool, api_key_id: Uuid) -> MeteringEvidence {
    let row: EvidenceRow = sqlx_core::query_as::query_as(
        r#"
        SELECT request_id, cost_usd, status, reserved_output_tokens, reserved_cost_usd,
               estimator_basis, task_signature, input_tokens, output_tokens
        FROM usage_events WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("exactly one usage row must exist for this key");
    MeteringEvidence {
        request_id: row.0,
        cost_usd: row.1,
        status: row.2,
        reserved_output_tokens: row.3,
        reserved_cost_usd: row.4,
        estimator_basis: row.5,
        task_signature: row.6,
        input_tokens: row.7,
        output_tokens: row.8,
    }
}

async fn attempt_rows(pool: &PgPool, request_id: Uuid) -> (i64, i64) {
    sqlx_core::query_as::query_as(
        r#"
        SELECT COUNT(*), COUNT(*) FILTER (WHERE served)
        FROM request_attempts WHERE request_id = $1
        "#,
    )
    .bind(request_id)
    .fetch_one(pool)
    .await
    .expect("attempt rows must query")
}

async fn reservation_count(pool: &PgPool, api_key_id: Uuid) -> i64 {
    sqlx_core::query_scalar::query_scalar(
        "SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1",
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("reservation count must query")
}

/// Every `usage`-type ledger entry for a user, with the balance it left behind.
async fn usage_ledger(pool: &PgPool, user_id: Uuid) -> Vec<(Decimal, Decimal)> {
    sqlx_core::query_as::query_as(
        r#"
        SELECT amount_usd, balance_after_usd
        FROM credit_ledger
        WHERE user_id = $1 AND entry_type = 'usage'
        ORDER BY id
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("ledger must query")
}

async fn balance_of(pool: &PgPool, user_id: Uuid) -> Decimal {
    sqlx_core::query_scalar::query_scalar("SELECT credit_balance_usd FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("balance must query")
}

/// Send a completion and report only its status — for the requests that are
/// supposed to be refused.
async fn serve_status(state: &RouterState, key: &str, body: &Value) -> StatusCode {
    app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("completion request should build"),
        )
        .await
        .expect("completion should complete")
        .status()
}

/// A fake whose first answer is a non-retryable upstream failure, so the walk
/// abandons this rung and moves to the next one without burning backoff.
fn broken_upstream(alias: &str) -> Arc<FakeModelProvider> {
    FakeModelProvider::new(alias, vec![FakeOutcome::Failure("401 upstream refused")])
}

/// A router that serves the edge catalog with `drop_provider`'s candidates
/// removed from every route — the shape [`ProviderRoute::new`] produces in
/// production when that provider's credential is missing from the environment.
fn router_without(
    pool: PgPool,
    fakes: Vec<Arc<FakeModelProvider>>,
    drop_provider: &'static str,
) -> RouterState {
    let route: InjectedRoute = Arc::new(move |resolved: &ResolvedRoute, _max_output_tokens| {
        ProviderRoute::from_candidates(
            resolved
                .candidates
                .iter()
                .filter(|definition| definition.provider != drop_provider)
                .cloned()
                .map(|definition| {
                    let alias = definition.id.split('/').next_back().unwrap_or_default();
                    let fake = fakes
                        .iter()
                        .find(|fake| {
                            zerorouter::provider::ModelProvider::alias(fake.as_ref()) == alias
                        })
                        .unwrap_or_else(|| panic!("no scripted fake for {}", definition.id))
                        .clone();
                    ProviderCandidate::with_provider(definition, fake)
                })
                .collect(),
        )
    });
    RouterState::with_injected_route(fixture("local_candidates_tiers.toml"), pool, true, route)
}

// ---------------------------------------------------------------------------
// Stage 3: the metering seam.
//
// The stage-2 canary that stood here asserted the opposite of everything below
// — it existed to make this change deliberate rather than accidental, and it
// named the exact artifacts that would have to fall. They have. Each one is
// re-asserted here in its new direction, and every one of them is re-asserted
// UNCHANGED on a metered route by `the_metered_path_is_unchanged_on_a_paid_route`,
// so "the free lane skipped it" and "the mechanism still works" are two
// separate, independently failing claims.
// ---------------------------------------------------------------------------

/// The stage's whole claim, in terms of MECHANISM rather than outcome.
///
/// Every assertion is chosen for being an artifact only the reserve → settle
/// path can produce, so none of them can be satisfied by an off-path recorder:
///
/// - **`reserved_output_tokens`, `reserved_cost_usd` and `estimator_basis` are
///   NULL.** The settle INSERT binds all three from the settlement intent, and
///   all three are non-nullable Rust values there — `settle_once` is
///   structurally incapable of writing this row. NULL is not "a zero
///   reservation"; it is the ledger's word for *no reservation existed*.
/// - **No `request_attempts` rows.** Those ride inside the settle transaction,
///   on the event row's foreign key. Off-path recording is not that
///   transaction, and this stage deliberately does not write them (see the
///   report: a known observability gap, not a silent one).
/// - **No reservation row, and no `usage` ledger entry.** Nothing was
///   encumbered and nothing was debited.
/// - **The response still carries an `x-request-id` in the same shape**, and it
///   still leads to the row. What the id refers to changed; what a customer or
///   a support ticket can do with it did not.
#[tokio::test]
async fn a_zero_priced_route_skips_reserve_lock_and_settle() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "free-skips").await;
    let user_id: Uuid =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(&pool)
            .await
            .expect("owner must query");
    let funded = balance_of(&pool, user_id).await;
    let phi = upstream("phi4-mini");
    let state = router(pool.clone(), vec![phi.clone()]);

    let (provider, request_id) = serve_with_request_id(
        &state,
        &key,
        &completion("local-llama/phi4-mini", "hello", false),
    )
    .await;
    assert_eq!(provider, "local-llama");
    state.wait_for_background_tasks().await;
    await_usage_rows(&pool, api_key_id, 1).await;

    let evidence = metering_evidence(&pool, api_key_id).await;
    assert_eq!(evidence.cost_usd, Decimal::ZERO);
    assert_eq!(evidence.status, 200);
    assert_eq!(
        request_id,
        format!("chatcmpl-{}", evidence.request_id.simple()),
        "the response's id still leads to the row, so nothing keyed on it breaks"
    );
    assert_eq!(
        (
            evidence.reserved_output_tokens,
            evidence.reserved_cost_usd,
            evidence.estimator_basis.as_deref(),
        ),
        (None, None, None),
        "a settled row carries the reservation's terms; this row has none to carry, \
         which is the proof no settle wrote it"
    );
    assert_eq!(
        (evidence.input_tokens, evidence.output_tokens),
        (1_000, 20),
        "tokens as the upstream reported them — the row is observability, and \
         observability that rounds is not observability"
    );

    assert_eq!(
        attempt_rows(&pool, evidence.request_id).await,
        (0, 0),
        "the attempt ledger rides the settle transaction, and there was none"
    );
    assert_eq!(reservation_count(&pool, api_key_id).await, 0);
    assert!(
        usage_ledger(&pool, user_id).await.is_empty(),
        "no debit, so no ledger entry — the free lane never touches the balance"
    );
    assert_eq!(balance_of(&pool, user_id).await, funded);
    assert_eq!(phi.call_count(), 1);
}

/// The twin, and the reason it exists: every mechanism assertion the stage-2
/// canary made, made again on a PAID route, so this change is provably a fork
/// in the road rather than a removal.
///
/// If the metered path ever regresses, this fails and the free-lane test above
/// does not — which is what tells a reader which of the two lanes broke.
#[tokio::test]
async fn the_metered_path_is_unchanged_on_a_paid_route() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "metered-twin").await;
    let user_id: Uuid =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(&pool)
            .await
            .expect("owner must query");
    let funded = balance_of(&pool, user_id).await;
    let burst = upstream("edge-burst");
    let state = router(
        pool.clone(),
        vec![burst.clone(), upstream("qwen3-8b"), upstream("phi4-mini")],
    );

    // Balanced mode over the mixed tier: the table lists the cloud rung first,
    // so this serves the PAID candidate under a tier that sells at 3.00/6.00.
    let (provider, request_id) =
        serve_with_request_id(&state, &key, &completion("zero/edge", "hello", false)).await;
    assert_eq!(provider, "openai");
    state.wait_for_background_tasks().await;

    let evidence = metering_evidence(&pool, api_key_id).await;
    // 1000 input and 20 output at the tier's sell rate: 0.003 + 0.00012.
    let expected = Decimal::from_str("0.00312").expect("literal must parse");
    assert_eq!(evidence.cost_usd, expected);
    assert_eq!(evidence.status, 200);
    assert_eq!(
        request_id,
        format!("chatcmpl-{}", evidence.request_id.simple()),
        "the settled row is keyed by the reservation the request took out"
    );
    assert!(
        evidence
            .reserved_output_tokens
            .is_some_and(|bound| bound > 0),
        "admission reserved an output bound and the settled row snapshots it"
    );
    assert!(
        evidence
            .reserved_cost_usd
            .is_some_and(|cost| cost >= expected),
        "the reservation was taken, and for at least what the request cost"
    );
    assert_eq!(evidence.estimator_basis.as_deref(), Some("cold"));
    assert!(
        evidence.task_signature.is_some(),
        "the settled row carries its segment key, which is what trains the estimator"
    );

    assert_eq!(
        attempt_rows(&pool, evidence.request_id).await,
        (1, 1),
        "the attempt ledger rides the settle transaction, on the event row's key"
    );
    assert_eq!(reservation_count(&pool, api_key_id).await, 0);
    assert_eq!(
        usage_ledger(&pool, user_id).await,
        vec![(-expected, funded - expected)],
        "one debit, clamped to the reservation, in the settle transaction"
    );
    assert_eq!(balance_of(&pool, user_id).await, funded - expected);
}

/// **The test this stage turns on.** A free rung is tried first, it fails, the
/// walk falls back to a PAID rung, and the customer is charged the paid rung's
/// price — because the whole route was metered from admission onwards.
///
/// This is the disaster the `all`-not-`any` rule exists to prevent. The
/// reservation is taken before the walk begins, when nothing is known about
/// which rung will answer; a skip keyed on "the rung we expect to serve is
/// free", or on "the first candidate is free", would have dispatched this
/// request's fallback to a metered upstream with no reservation behind it, no
/// exactly-once settle, and no way to bill for inference already delivered.
#[tokio::test]
async fn a_free_rung_that_fails_over_to_a_paid_one_is_fully_metered_and_charged() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "mixed-fallback").await;
    let user_id: Uuid =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(&pool)
            .await
            .expect("owner must query");
    let funded = balance_of(&pool, user_id).await;
    // Cost mode puts the $0 rung first; it then refuses, so the cloud rung
    // behind it serves.
    let qwen = broken_upstream("qwen3-8b");
    let burst = upstream("edge-burst");
    let state = router(pool.clone(), vec![qwen.clone(), burst.clone()]);

    let (provider, request_id) =
        serve_with_request_id(&state, &key, &completion("zero/edge:cost", "hello", false)).await;
    assert_eq!(
        provider, "openai",
        "the free rung failed, so the paid rung served"
    );
    assert_eq!(qwen.call_count(), 1, "the free rung was tried first");
    assert_eq!(burst.call_count(), 1);
    state.wait_for_background_tasks().await;

    let evidence = metering_evidence(&pool, api_key_id).await;
    let expected = Decimal::from_str("0.00312").expect("literal must parse");
    assert_eq!(
        evidence.cost_usd, expected,
        "the customer is charged the tier's sell rate for the paid rung that served"
    );
    assert_eq!(
        request_id,
        format!("chatcmpl-{}", evidence.request_id.simple()),
        "the response carried the reservation's id, so a reservation existed"
    );
    assert!(
        evidence.reserved_output_tokens.is_some() && evidence.reserved_cost_usd.is_some(),
        "the settled row snapshots the reservation admission actually took"
    );
    assert_eq!(evidence.estimator_basis.as_deref(), Some("cold"));
    assert_eq!(
        attempt_rows(&pool, evidence.request_id).await,
        (2, 1),
        "both walk positions are on the ledger, and exactly one served"
    );
    assert_eq!(reservation_count(&pool, api_key_id).await, 0);
    assert_eq!(
        usage_ledger(&pool, user_id).await,
        vec![(-expected, funded - expected)],
        "the balance moved, in the settle transaction, for inference already delivered"
    );
}

/// The other half of the same rule, and the one a candidate-only predicate gets
/// wrong: a MIXED tier whose paid rung is unavailable.
///
/// `ProviderRoute` drops a candidate whose credential is missing from the
/// environment, so an unset cloud key collapses `zero/edge` to a route whose
/// every candidate is free — while the tier still SELLS at 3.00/6.00. Reading
/// candidate freeness as customer freeness would turn a deployment mistake into
/// free paid-tier inference, silently, for as long as the variable stayed
/// unset. It stays metered, and the customer is charged exactly what the tier
/// says.
#[tokio::test]
async fn an_all_free_route_under_a_priced_tier_is_still_metered_and_charged() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "priced-tier-local").await;
    let user_id: Uuid =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(&pool)
            .await
            .expect("owner must query");
    let funded = balance_of(&pool, user_id).await;
    let qwen = upstream("qwen3-8b");
    let state = router_without(pool.clone(), vec![qwen.clone()], "openai");

    let (provider, _) =
        serve_with_request_id(&state, &key, &completion("zero/edge", "hello", false)).await;
    assert_eq!(provider, "local-llama", "only the free rung was available");
    state.wait_for_background_tasks().await;

    let evidence = metering_evidence(&pool, api_key_id).await;
    let expected = Decimal::from_str("0.00312").expect("literal must parse");
    assert_eq!(
        evidence.cost_usd, expected,
        "a $0 COST BASIS is not a $0 PRICE; the tier's sell rate is what the \
         customer owes, whichever rung served"
    );
    assert!(
        evidence.reserved_cost_usd.is_some(),
        "a route that can charge must reserve, however free its rungs are"
    );
    assert_eq!(attempt_rows(&pool, evidence.request_id).await, (1, 1));
    assert_eq!(
        usage_ledger(&pool, user_id).await,
        vec![(-expected, funded - expected)],
    );
}

/// Every admission gate the free lane keeps, in one place.
///
/// The velocity half is the stage-2 canary's, unchanged and still passing —
/// which is the point: it was the cleanest proof that admission ran, and it
/// still is. A mechanism-level skip that bypassed admission could not refuse
/// this request, because it would never have measured it.
#[tokio::test]
async fn the_free_lane_is_still_gated_by_admission() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "free-admission").await;
    // A budget far under the request's own output bound, so the request cannot
    // fit however cheap it is.
    sqlx_core::query::query("UPDATE api_keys SET velocity_cap_tokens_per_min = 8 WHERE id = $1")
        .bind(api_key_id)
        .execute(&pool)
        .await
        .expect("velocity cap must update");

    let phi = upstream("phi4-mini");
    let state = router(pool.clone(), vec![phi.clone()]);
    assert_eq!(
        serve_status(
            &state,
            &key,
            &completion("local-llama/phi4-mini", "hello", false)
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS,
        "the token-denominated velocity cap binds on a route that costs nothing — \
         free inference still burns the operator's own hardware"
    );
    state.wait_for_background_tasks().await;
    assert_eq!(
        phi.call_count(),
        0,
        "refused before dispatch, exactly as a metered route would be"
    );
    assert_eq!(
        reservation_count(&pool, api_key_id).await,
        0,
        "a refusal reserves nothing on either lane"
    );
}

/// What the prepaid gate actually does on a $0 route — pinned because it is
/// easy, and wrong, to assume it does something else.
///
/// The gate is `balance - encumbered < this request's reserved COST`, and on a
/// route that sells at $0 that cost is $0. So a **zero balance passes**: an
/// account with no credit may serve a route that will never ask it for any.
/// That is not a property of this lane — it is the metered path's own
/// arithmetic, true of a $0-sell tier on `main` before this change, and it is
/// preserved here rather than introduced. (Proved by mutation: forcing this
/// route back onto the reserved lane produces the same 200.)
///
/// What still refuses, and must:
///
/// - a **negative** balance — a spent-through or reversed account is over its
///   credit however cheap the next request is;
/// - a **frozen** account — checked before any arithmetic, so a chargeback
///   stops free traffic exactly as it stops paid traffic.
///
/// The consequence worth stating for the roadmap: this lane is NOT a
/// free-tier-on-signup mechanism as it stands. A fresh account with a
/// non-negative zero balance can already use it; an account in credit trouble
/// cannot. Whether that is the product's intent is a decision for the repo
/// owner, and this test exists to make the current answer explicit rather than
/// discovered later.
#[tokio::test]
async fn the_free_lane_keeps_the_prepaid_and_freeze_gates() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let body = completion("local-llama/phi4-mini", "hello", false);
    let owner = |pool: PgPool, api_key_id: Uuid| async move {
        sqlx_core::query_scalar::query_scalar::<_, Uuid>(
            "SELECT user_id FROM api_keys WHERE id = $1",
        )
        .bind(api_key_id)
        .fetch_one(&pool)
        .await
        .expect("owner must query")
    };

    // Zero balance: admitted, because the request's reserved cost is zero too.
    let (zero_key_id, zero_key) = create_funded_key(&pool, "free-zero-balance").await;
    let zero_user = owner(pool.clone(), zero_key_id).await;
    sqlx_core::query::query("UPDATE users SET credit_balance_usd = 0 WHERE id = $1")
        .bind(zero_user)
        .execute(&pool)
        .await
        .expect("balance must zero out");
    let zero_state = router(pool.clone(), vec![upstream("phi4-mini")]);
    assert_eq!(
        serve_status(&zero_state, &zero_key, &body).await,
        StatusCode::OK,
        "a $0 route asks a $0 account for nothing it does not have"
    );
    zero_state.wait_for_background_tasks().await;

    // Fully encumbered: refused. The gate subtracts what is already held for
    // in-flight metered requests, so an account whose credit is spoken for is
    // over its limit however cheap the next request is. (The balance is driven
    // there by an encumbrance rather than by a negative number, because
    // `reject_unauthorized_overdraft` — correctly — refuses to let a test write
    // one.)
    let (owing_key_id, owing_key) = create_funded_key(&pool, "free-encumbered").await;
    sqlx_core::query::query(
        r#"
        INSERT INTO usage_reservations (id, api_key_id, expires_at, reserved_tokens, reserved_cost_usd)
        VALUES ($1, $2, NOW() + INTERVAL '10 minutes', 1000, 100)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(owing_key_id)
    .execute(&pool)
    .await
    .expect("encumbering reservation must insert");
    let owing_phi = upstream("phi4-mini");
    let owing_state = router(pool.clone(), vec![owing_phi.clone()]);
    assert_eq!(
        serve_status(&owing_state, &owing_key, &body).await,
        StatusCode::PAYMENT_REQUIRED,
        "an account whose credit is fully encumbered is refused the free lane too"
    );
    owing_state.wait_for_background_tasks().await;
    assert_eq!(owing_phi.call_count(), 0, "refused before dispatch");

    // Frozen: refused, whatever the balance says.
    let (frozen_key_id, frozen_key) = create_funded_key(&pool, "free-frozen").await;
    let frozen_user = owner(pool.clone(), frozen_key_id).await;
    zerorouter::billing::freeze_account(
        &pool,
        frozen_user,
        zerorouter::billing::FreezeReason::Dispute,
    )
    .await
    .expect("freeze must apply");
    let frozen_phi = upstream("phi4-mini");
    let frozen_state = router(pool.clone(), vec![frozen_phi.clone()]);
    assert_eq!(
        serve_status(&frozen_state, &frozen_key, &body).await,
        StatusCode::PAYMENT_REQUIRED,
        "a chargeback stops free traffic exactly as it stops paid traffic"
    );
    frozen_state.wait_for_background_tasks().await;
    assert_eq!(frozen_phi.call_count(), 0);
}

/// The concurrency claim, proved structurally rather than by timing.
///
/// The per-user `pg_advisory_xact_lock` and the reservation INSERT live in the
/// same transaction, and the lock is taken first — so a request that completes
/// while somebody ELSE holds that user's advisory lock provably ran neither.
/// The test holds the lock from outside and watches both lanes:
///
/// - the free lane answers 200, so it took no lock and created no reservation;
/// - the metered lane cannot, and says so (`SET LOCAL lock_timeout = '5s'`
///   turns the wait into a refusal rather than a hang).
///
/// That is the throughput property the design is after — a user's concurrent
/// free traffic does not queue behind itself — stated in a form that cannot
/// pass by being lucky with a scheduler.
#[tokio::test]
async fn the_free_lane_takes_no_per_user_advisory_lock() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "free-nolock").await;
    let user_id: Uuid =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(&pool)
            .await
            .expect("owner must query");
    let state = router(
        pool.clone(),
        vec![
            upstream("phi4-mini"),
            upstream("edge-burst"),
            upstream("qwen3-8b"),
        ],
    );

    let mut holder = pool.begin().await.expect("lock holder must begin");
    sqlx_core::query::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *holder)
        .await
        .expect("the test must be able to take this user's admission lock");

    assert_eq!(
        serve_status(
            &state,
            &key,
            &completion("local-llama/phi4-mini", "hello", false)
        )
        .await,
        StatusCode::OK,
        "the free lane never waits on the lock it never takes"
    );
    assert_eq!(
        serve_status(&state, &key, &completion("zero/edge", "hello", false)).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a metered request on the same user still serializes on that lock, and \
         times out behind a holder rather than admitting unserialized"
    );
    assert_eq!(
        reservation_count(&pool, api_key_id).await,
        0,
        "the free request completed while the lock was held, so it can have \
         created no reservation: the INSERT is inside the locked transaction"
    );

    holder.rollback().await.expect("lock must release");
    state.wait_for_background_tasks().await;
}

/// Two concurrent free requests on one key both serve, and neither waits for
/// the other. The lane's reason for existing, end to end.
#[tokio::test]
async fn two_concurrent_free_requests_on_one_key_both_serve() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "free-concurrent").await;
    let phi = FakeModelProvider::new(
        "phi4-mini",
        (0..2)
            .map(|_| FakeOutcome::chat("hello from the edge", served_usage()))
            .collect(),
    );
    let state = router(pool.clone(), vec![phi.clone()]);
    let body = completion("local-llama/phi4-mini", "hello", false);

    let (first, second) = tokio::join!(
        serve_status(&state, &key, &body),
        serve_status(&state, &key, &body),
    );
    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
    state.wait_for_background_tasks().await;
    await_usage_rows(&pool, api_key_id, 2).await;
    assert_eq!(phi.call_count(), 2);
    assert_eq!(reservation_count(&pool, api_key_id).await, 0);
}

/// The design's hard security requirement — *"async usage recording must not
/// become a billing input"* — walked consumer by consumer over a real database.
///
/// Every reader of `usage_events` in the crate is one of five things, and this
/// covers all five:
///
/// 1. **Balance and ledger** (`settle_once`, the purchase paths): they do not
///    read `usage_events` at all, and the free lane writes neither.
/// 2. **Autopay** (`billing::autopay_candidates`,
///    `AUTOPAY_ELIGIBILITY_PREDICATE`): reads `users` and
///    `stripe_autopay_intents` only. Asserted by running the real selection
///    either side of a free request over an autopay-armed account.
/// 3. **Spend caps** (`begin_usage_session`) and the spend reports
///    (`billing::usage_summary`, `admin`, `portal`): they sum `cost_usd`, and
///    the row's is zero — so it is visible as usage and worth nothing as money,
///    which is exactly the intent.
/// 4. **The estimator** (`output_token_percentiles`, `segment_clamp_stats`,
///    `user_clamp_loss`): all key on `task_signature` / `estimator_basis`, both
///    NULL here, so the row is invisible to reservation sizing. This is the
///    sharp one — the segment key omits the model, so local output lengths
///    would otherwise train the percentiles that size METERED reservations.
/// 5. **The velocity cap**: it DOES count the row, deliberately, and that is
///    pinned separately below.
#[tokio::test]
async fn the_free_lane_usage_row_is_inert_to_every_billing_consumer() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "free-inert").await;
    let user_id: Uuid =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(&pool)
            .await
            .expect("owner must query");
    // Arm autopay and put the account under its threshold, so this user is a
    // live candidate for an off-session charge before and after.
    sqlx_core::query::query(
        r#"
        UPDATE users
        SET autopay_enabled = TRUE,
            autopay_threshold_usd = 100,
            autopay_topup_usd = 25,
            stripe_customer_id = $2
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .bind(format!("cus_free_lane_{}", user_id.simple()))
    .execute(&pool)
    .await
    .expect("autopay arming must apply");

    let armed_before: Vec<Uuid> = zerorouter::billing::autopay_candidates(&pool, 50)
        .await
        .expect("autopay selection must run")
        .into_iter()
        .map(|candidate| candidate.user_id)
        .collect();
    assert!(
        armed_before.contains(&user_id),
        "the fixture must be a live autopay candidate, or this proves nothing"
    );
    let balance_before = balance_of(&pool, user_id).await;

    let phi = upstream("phi4-mini");
    let state = router(pool.clone(), vec![phi.clone()]);
    serve(
        &state,
        &key,
        &completion("local-llama/phi4-mini", "hello", false),
    )
    .await;
    state.wait_for_background_tasks().await;
    await_usage_rows(&pool, api_key_id, 1).await;

    // 1. Balance and ledger: untouched.
    assert_eq!(balance_of(&pool, user_id).await, balance_before);
    assert!(usage_ledger(&pool, user_id).await.is_empty());

    // 2. Autopay: the same selection, unmoved. Free usage cannot make an
    //    account eligible for a card charge, nor take eligibility away.
    let armed_after: Vec<Uuid> = zerorouter::billing::autopay_candidates(&pool, 50)
        .await
        .expect("autopay selection must run")
        .into_iter()
        .map(|candidate| candidate.user_id)
        .collect();
    assert_eq!(armed_before, armed_after);

    // 3. Spend: visible as usage, worth nothing as money.
    let summary = zerorouter::billing::usage_summary(&pool, user_id)
        .await
        .expect("usage summary must query");
    assert_eq!(summary.requests, 1);
    assert_eq!(summary.input_tokens, 1_000);
    assert_eq!(summary.output_tokens, 20);
    assert_eq!(
        summary.spend_usd,
        Decimal::ZERO,
        "the row sums into every spend aggregate at exactly zero"
    );

    // 4. The estimator cannot see it, on any of its three queries.
    let evidence = metering_evidence(&pool, api_key_id).await;
    assert_eq!(evidence.task_signature, None);
    assert_eq!(evidence.estimator_basis, None);
    let signature_rows: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT COUNT(*) FROM usage_events WHERE api_key_id = $1 AND task_signature IS NOT NULL",
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("signature count must query");
    assert_eq!(
        signature_rows, 0,
        "every estimator query is an equality on the segment key, and a NULL key \
         matches none of them — so a local rung's output lengths can never size a \
         metered reservation"
    );
    let (loss_30d, loss_37d) = zerorouter::db::user_clamp_loss(&pool, user_id)
        .await
        .expect("clamp loss must query");
    assert_eq!((loss_30d, loss_37d), (Decimal::ZERO, Decimal::ZERO));
}

/// The one deliberate exception to inertness, pinned in both directions.
///
/// Free usage COUNTS toward the token-denominated velocity cap. The cap is
/// abuse control, not accounting: free inference still burns the operator's own
/// GPU, and a lane exempt from rate limiting would be the cheapest
/// denial-of-service in the product. Counting is also the safe direction to be
/// wrong in — it can refuse traffic, never authorize a charge.
#[tokio::test]
async fn free_usage_counts_toward_the_velocity_cap() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let phi = || {
        FakeModelProvider::new(
            "phi4-mini",
            vec![FakeOutcome::chat("hello from the edge", served_usage())],
        )
    };
    // The served usage is 1000 input + 20 output, so a cap of exactly 1020 is
    // spent to the last token by ONE free request.
    let cap = 1_020;
    let small = json!({
        "model": "local-llama/phi4-mini",
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 512,
        "stream": false,
    });

    // Control: the same cap, the same request, no prior free usage.
    let (control_key_id, control_key) = create_funded_key(&pool, "velocity-control").await;
    sqlx_core::query::query("UPDATE api_keys SET velocity_cap_tokens_per_min = $2 WHERE id = $1")
        .bind(control_key_id)
        .bind(cap)
        .execute(&pool)
        .await
        .expect("velocity cap must update");
    let control_state = router(pool.clone(), vec![phi()]);
    assert_eq!(
        serve_status(&control_state, &control_key, &small).await,
        StatusCode::OK,
        "the request fits under the cap on its own; anything else and the test \
         below would pass for the wrong reason"
    );
    control_state.wait_for_background_tasks().await;

    // The real case: one free request first, then the same request again.
    let (api_key_id, key) = create_funded_key(&pool, "velocity-counts").await;
    sqlx_core::query::query("UPDATE api_keys SET velocity_cap_tokens_per_min = $2 WHERE id = $1")
        .bind(api_key_id)
        .bind(cap)
        .execute(&pool)
        .await
        .expect("velocity cap must update");
    let state = router(pool.clone(), vec![phi()]);
    assert_eq!(
        serve_status(&state, &key, &small).await,
        StatusCode::OK,
        "the first free request is admitted"
    );
    state.wait_for_background_tasks().await;
    await_usage_rows(&pool, api_key_id, 1).await;

    let second = router(pool.clone(), vec![phi()]);
    assert_eq!(
        serve_status(&second, &key, &small).await,
        StatusCode::TOO_MANY_REQUESTS,
        "the free request's tokens are on the ledger and they spend the budget"
    );
    second.wait_for_background_tasks().await;
}

/// Edge mode's drift story, end to end through the real `reconcile` with the
/// operator inventory installed — the path `zerorouter admin catalog-drift`
/// takes once it loads that inventory (which it now does).
///
/// Without the inventory this command could not even load an edge catalog: the
/// tier file names a provider the shipped list does not have, so the load fails
/// with "unsupported provider" and the deployment shape that most needs a drift
/// report is the one that cannot run it.
#[tokio::test]
async fn drift_reconciles_an_edge_catalog_and_never_fails_on_the_local_rungs() {
    operator_inventory();
    let catalog = load_tier_catalog(&fixture("local_candidates_tiers.toml"))
        .await
        .expect("the edge catalog should load once the operator inventory is installed");

    // models.dev knows the cloud rung's model and nothing about the local ones,
    // which is the permanent state of affairs for a model on someone's desk.
    let source = r#"{"openai": {"models": {
        "upstream/edge-burst": {"cost": {"input": 1.0, "output": 2.0, "cache_read": 0.2}},
        "upstream/toolless-burst": {"cost": {"input": 1.0, "output": 2.0, "cache_read": 0.2}}
    }}}"#;
    let findings = zerorouter::drift::reconcile(&catalog, source);

    let verdict = |candidate_id: &str| {
        findings
            .iter()
            .find(|found| found.candidate_id == candidate_id)
            .unwrap_or_else(|| panic!("{candidate_id} should be reconciled"))
            .verdict
            .clone()
    };
    assert_eq!(
        verdict("openai/edge-burst"),
        zerorouter::drift::Verdict::Match,
        "the cloud rung reconciles exactly as it always did"
    );
    for local in [
        "local-llama/qwen3-8b",
        "local-llama/gemma3-4b",
        "local-llama/phi4-mini",
    ] {
        assert_eq!(
            verdict(local),
            zerorouter::drift::Verdict::Unreconcilable,
            "{local} runs on the operator's hardware; no public catalog covers it"
        );
    }
    assert!(
        !findings
            .iter()
            .any(|found| found.verdict.is_actionable() || found.has_actionable_metadata_drift()),
        "an edge catalog that is entirely correct must not fail the command"
    );
}

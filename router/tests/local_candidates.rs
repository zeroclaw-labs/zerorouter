//! Edge mode, stage 2: an operator-declared local provider, $0 candidates that
//! route to it, and the cost-led ordering that puts them first
//! (`docs/design/edge-mode-local-rung.md`).
//!
//! Its own test binary on purpose. Installing an operator provider inventory is
//! a once-per-process act — the shipped inventory is `include_str!`-embedded and
//! the overlay is a `OnceLock` beside it — so a binary that installs one must be
//! a binary where every test wants it installed. Putting these anywhere else
//! would leak an extra provider into suites that pin the shipped inventory
//! exactly.
//!
//! What is NOT here, deliberately: anything about metering. A $0 route still
//! reserves and still settles, at its tier's sell rate, exactly as every other
//! route does. The reserve/settle skip is stage 3 and arrives with its own
//! adversarial review.

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
        ProviderBuildError, ProviderCandidate, ProviderRoute, is_operator_declared_provider,
        is_supported_provider, load_operator_inventory, provider_allows_zero_price,
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
        assert_eq!(count, 1);

        let again = load_operator_inventory(&fixture("local_providers.json"))
            .expect_err("a second install must be refused rather than silently ignored");
        assert!(
            matches!(again, ProviderBuildError::OperatorInventoryAlreadyLoaded),
            "unexpected error {again:?}"
        );
    });
}

#[tokio::test]
async fn a_local_provider_is_addressable_and_only_it_may_be_free() {
    operator_inventory();

    assert!(is_supported_provider("local-llama"));
    assert!(is_supported_provider("openai"), "shipped providers survive");

    // The guard that keeps a paid model out of the free rung, at the level it
    // is enforced: the adapter, read off the inventory itself rather than off a
    // list of vendor names kept somewhere else.
    assert!(provider_allows_zero_price("local-llama"));
    for shipped in ["openai", "anthropic"] {
        assert!(
            !provider_allows_zero_price(shipped),
            "{shipped} owns a cloud endpoint and may never carry a $0 basis"
        );
    }

    // And which entries the drift reconciliation may exempt.
    assert!(is_operator_declared_provider("local-llama"));
    assert!(!is_operator_declared_provider("openai"));
    assert!(!is_operator_declared_provider("nothing-like-this"));
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
    assert!(detail.contains("chat_completions"), "{detail}");
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

/// Stage 2 changes nothing about metering, and this is what that means in the
/// ledger: a $0 route reserves, dispatches, and settles through exactly the
/// same path every other route takes — it just settles at zero. The skip
/// (design doc: "Metering seam: $0 routes skip reserve/settle") is stage 3,
/// behind its own adversarial review, and this test is what will have to change
/// when it lands.
#[tokio::test]
async fn a_zero_priced_route_still_reserves_and_settles_like_any_other() {
    operator_inventory();
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "free-settles").await;
    let phi = upstream("phi4-mini");
    let state = router(pool.clone(), vec![phi.clone()]);

    assert_eq!(
        serve(
            &state,
            &key,
            &completion("local-llama/phi4-mini", "hello", false)
        )
        .await,
        "local-llama"
    );
    state.wait_for_background_tasks().await;

    let (provider, cost, status): (String, Decimal, i16) = sqlx_core::query_as::query_as(
        "SELECT upstream_provider, cost_usd, status FROM usage_events WHERE api_key_id = $1",
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("a $0 route must still write exactly one settled row");

    assert_eq!(provider, "local-llama");
    assert_eq!(status, 200);
    assert_eq!(cost, Decimal::ZERO, "priced at zero, settled at zero");
    assert_eq!(phi.call_count(), 1);

    // No reservation is left behind: the settle path ran to completion, the
    // same one a metered route takes.
    let reservations: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1",
    )
    .bind(api_key_id)
    .fetch_one(&pool)
    .await
    .expect("reservation count must query");
    assert_eq!(reservations, 0);
}

use std::{path::PathBuf, str::FromStr};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;
use zerorouter::{RouterState, app, error::ApiError, load_tier_catalog};

fn tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/tiers.toml")
}

/// A catalog fixture written under this test binary's target directory, at a
/// path the loader and `RouterState` can be pointed at. Each caller passes its
/// own `name` so tests running in parallel never share a file.
async fn catalog_fixture(name: &str, source: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.toml"));
    tokio::fs::write(&path, source)
        .await
        .expect("catalog fixture should write");
    path
}

/// A two-tier fixture. `zero/fixture-healthy` is always priced sanely;
/// `zero/fixture-dear` sells at 2.00/10.00 and carries `dear_basis` as its
/// candidate's input cost — at or below 2.00 for a healthy pair, above it to
/// put that one tier (and only that one) below its own cost basis.
fn two_tier_source(dear_basis: &str) -> String {
    format!(
        r#"
schema_version = 1

[tiers."zero/fixture-healthy"]
[tiers."zero/fixture-healthy".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/fixture-healthy".candidates]]
id = "fireworks/healthy"
provider = "fireworks"
model = "upstream/healthy"
[tiers."zero/fixture-healthy".candidates.rates]
input_per_mtok = 0.50
output_per_mtok = 1.00

[tiers."zero/fixture-dear"]
[tiers."zero/fixture-dear".rates]
input_per_mtok = 2.00
output_per_mtok = 10.00
[[tiers."zero/fixture-dear".candidates]]
id = "anthropic/dear"
provider = "anthropic"
model = "upstream/dear"
[tiers."zero/fixture-dear".candidates.rates]
input_per_mtok = {dear_basis}
output_per_mtok = 10.00
"#
    )
}

/// The `data[].id` values of a `/v1/models` response, in wire order.
async fn listed_model_ids(state: RouterState) -> Vec<String> {
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
        .expect("models response body should be readable")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("models response should be JSON");
    json["data"]
        .as_array()
        .expect("models response should contain a data array")
        .iter()
        .map(|model| {
            model["id"]
                .as_str()
                .expect("every model should carry a string id")
                .to_owned()
        })
        .collect()
}

/// The parsed JSON error envelope of a response, with its status.
async fn error_envelope(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("error body should be readable")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&body).expect("error body should be JSON"),
    )
}

#[tokio::test]
async fn healthz_reports_ok() {
    let response = app(RouterState::new(tier_config_path()))
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("health request should build"),
        )
        .await
        .expect("health request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("health response body should be readable")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("health response should be JSON");
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn completion_authentication_precedes_body_buffering() {
    let options = PgConnectOptions::from_str("postgresql://unused@127.0.0.1/unused")
        .expect("lazy test database options should parse");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    let response = app(RouterState::with_database(tier_config_path(), pool, false))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .body(Body::from(vec![b'x'; 9 * 1024 * 1024]))
                .expect("completion request should build"),
        )
        .await
        .expect("completion request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn models_are_materialized_from_tiers_toml() {
    let response = app(RouterState::new(tier_config_path()))
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
        .expect("models response body should be readable")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("models response should be JSON");
    let data = json["data"]
        .as_array()
        .expect("models response should contain a data array");

    assert_eq!(json["object"], "list");
    // 3 tier ids + 11 concrete candidates (5 low-cost, 4 balanced, 2 high-end).
    assert_eq!(data.len(), 14);
    assert!(data.iter().all(|model| model["object"] == "model"));
    assert!(data.iter().any(|model| model["id"] == "zero/balanced"));
    assert!(
        data.iter()
            .any(|model| model["id"] == "bedrock/us.anthropic.claude-sonnet-5")
    );
    assert!(
        data.iter()
            .any(|model| model["id"] == "deepinfra/deepseek-ai/DeepSeek-V4-Pro")
    );
    assert!(
        data.iter()
            .any(|model| model["id"] == "bedrock/minimax.minimax-m2.5")
    );
    assert!(
        data.iter()
            .any(|model| model["id"] == "bedrock/deepseek.v3.2")
    );
    assert!(data.iter().any(|model| model["id"] == "minimax/MiniMax-M3"));
    assert!(
        data.iter()
            .any(|model| model["id"] == "openai/gpt-5.6-luna")
    );
}

#[tokio::test]
async fn model_pricing_matches_zeroclaws_model_pricing_wire_contract() {
    // Literal JSON snapshot: the pricing block's field names must be exactly
    // what ZeroClaw's `ModelPricing` deserializes
    // (`crates/zeroclaw-api/src/model_provider.rs`) — `prompt`, `completion`,
    // `input_cache_read` — as decimal-string USD-per-single-token rates, and
    // its values must be the tier's sell rate from `config/tiers.toml`
    // (`zero/low-cost`: 0.30 / 1.20 / 0.06 USD per 1M tokens) converted
    // exactly, with no binary-float artifacts.
    let response = app(RouterState::new(tier_config_path()))
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
        .expect("models response body should be readable")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("models response should be JSON");
    let data = json["data"]
        .as_array()
        .expect("models response should contain a data array");

    let tier = data
        .iter()
        .find(|model| model["id"] == "zero/low-cost")
        .expect("zero/low-cost tier should be listed");
    assert_eq!(
        *tier,
        serde_json::json!({
            "id": "zero/low-cost",
            "object": "model",
            "created": 0,
            "owned_by": "zerorouter",
            "pricing": {
                "prompt": "0.0000003",
                "completion": "0.0000012",
                "input_cache_read": "0.00000006",
            },
        })
    );

    // A pinned concrete candidate bills at its *owning tier's* sell rate,
    // never its own cost basis — this candidate's own rates in tiers.toml
    // omit `cached_input_per_mtok` entirely, yet the served pricing still
    // carries the tier's `input_cache_read`, proving the listing reads the
    // tier rate and not the candidate rate.
    let candidate = data
        .iter()
        .find(|model| model["id"] == "bedrock/minimax.minimax-m2.5")
        .expect("bedrock/minimax.minimax-m2.5 candidate should be listed");
    assert_eq!(
        *candidate,
        serde_json::json!({
            "id": "bedrock/minimax.minimax-m2.5",
            "object": "model",
            "created": 0,
            "owned_by": "bedrock",
            "pricing": {
                "prompt": "0.0000003",
                "completion": "0.0000012",
                "input_cache_read": "0.00000006",
            },
        })
    );
}

#[tokio::test]
async fn bundled_tier_catalog_has_expected_virtual_models() {
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog should load");

    assert_eq!(catalog.schema_version, 1);
    assert!(catalog.tiers.contains_key("zero/low-cost"));
    assert!(catalog.tiers.contains_key("zero/balanced"));
    assert!(catalog.tiers.contains_key("zero/high-end"));

    let low_cost = catalog
        .tiers
        .get("zero/low-cost")
        .expect("low-cost tier should exist");
    let low_cost_bedrock = low_cost
        .candidates
        .iter()
        .filter(|candidate| candidate.provider == "bedrock")
        .map(|candidate| (candidate.id.as_str(), candidate.model.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        low_cost_bedrock,
        [("bedrock/minimax.minimax-m2.5", "minimax.minimax-m2.5")]
    );

    let balanced = catalog
        .tiers
        .get("zero/balanced")
        .expect("balanced tier should exist");
    let balanced_bedrock = balanced
        .candidates
        .iter()
        .filter(|candidate| candidate.provider == "bedrock")
        .map(|candidate| (candidate.id.as_str(), candidate.model.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        balanced_bedrock,
        [("bedrock/deepseek.v3.2", "deepseek.v3.2")]
    );

    let high_end = catalog
        .tiers
        .get("zero/high-end")
        .expect("high-end tier should exist");
    let bedrock_profiles = high_end
        .candidates
        .iter()
        .filter(|candidate| candidate.provider == "bedrock")
        .map(|candidate| (candidate.id.as_str(), candidate.model.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        bedrock_profiles,
        [(
            "bedrock/us.anthropic.claude-sonnet-5",
            "us.anthropic.claude-sonnet-5"
        )]
    );
}

#[tokio::test]
async fn no_shipped_candidate_costs_more_than_its_tier_sells() {
    // The margin-leak regression guard, re-derived from the table rather than
    // delegated to the loader: three candidates once sat above their tier's
    // sell rate (opus in zero/high-end on all three dimensions, haiku in
    // zero/balanced on output alone) and lost money on every request they
    // served. `load_tier_catalog` now refuses such a table outright, so this
    // test failing at the assert rather than the load would mean the rule
    // itself regressed.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load and validate");

    for (tier_id, tier) in &catalog.tiers {
        for candidate in &tier.candidates {
            for (dimension, basis, sell) in [
                (
                    "input_per_mtok",
                    candidate.rates.input_per_mtok,
                    tier.rates.input_per_mtok,
                ),
                (
                    "output_per_mtok",
                    candidate.rates.output_per_mtok,
                    tier.rates.output_per_mtok,
                ),
                (
                    "cached_input_per_mtok",
                    candidate.rates.cached_input_per_mtok,
                    tier.rates.cached_input_per_mtok,
                ),
            ] {
                let (Some(basis), Some(sell)) = (basis, sell) else {
                    continue;
                };
                assert!(
                    basis <= sell,
                    "{} in {tier_id} has a {dimension} basis of {basis} above the tier sell rate {sell}",
                    candidate.id
                );
            }
        }
    }
}

#[tokio::test]
async fn a_below_cost_tier_is_withheld_while_every_other_tier_keeps_serving() {
    // The proportionality contract, end to end through the real loader: a tier
    // that cannot cover its own candidates is dropped from the servable
    // catalog, and the tiers that have nothing to do with it are untouched.
    let path = catalog_fixture("one_below_cost", &two_tier_source("3.00")).await;
    let catalog = load_tier_catalog(&path)
        .await
        .expect("one below-cost tier must not fail the whole catalog");

    assert_eq!(
        catalog.tiers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["zero/fixture-healthy"]
    );
    assert!(catalog.resolve("zero/fixture-healthy").is_some());
    assert!(catalog.resolve("fireworks/healthy").is_some());
    assert!(catalog.resolve("zero/fixture-dear").is_none());

    let withheld = catalog
        .unavailable_for("zero/fixture-dear")
        .expect("the below-cost tier should be withheld, not forgotten");
    assert_eq!(withheld.tier, "zero/fixture-dear");
    assert_eq!(
        withheld.reason,
        "candidate anthropic/dear in tier zero/fixture-dear costs more than the tier sells: \
         input_per_mtok cost basis 3 exceeds tier sell rate 2"
    );
}

#[tokio::test]
async fn models_omit_a_withheld_tier_and_its_pinned_candidates() {
    // Do not sell what cannot be served: the withheld tier id and the concrete
    // candidate pinned inside it both disappear from the public catalog, so a
    // customer never sees a model that a request for it would refuse.
    let path = catalog_fixture("models_below_cost", &two_tier_source("3.00")).await;

    assert_eq!(
        listed_model_ids(RouterState::new(path)).await,
        ["fireworks/healthy", "zero/fixture-healthy"]
    );
}

#[tokio::test]
async fn a_withheld_tier_is_refused_as_misconfigured_rather_than_missing_or_transient() {
    // The honest answer for a model that exists but cannot be sold. It is not
    // a 404 (the id is right there in the file), and it is not the generic
    // catalog 503 (which reads as a blip that clears itself). Its own code and
    // a message naming the tier are what tell an operator where to look.
    let (status, body) = error_envelope(
        ApiError::ModelUnavailable {
            tier: "zero/high-end".to_owned(),
        }
        .into_response(),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_ne!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "model_unavailable");
    assert_eq!(body["error"]["param"], "model");
    let message = body["error"]["message"]
        .as_str()
        .expect("the error message should be a string");
    assert!(message.contains("zero/high-end"), "{message}");
    assert!(message.contains("below its own cost basis"), "{message}");
    assert!(message.contains("not a transient outage"), "{message}");

    // Distinct from both neighbours it must never be confused with.
    let (not_found, not_found_body) = error_envelope(ApiError::ModelNotFound.into_response()).await;
    assert_eq!(not_found, StatusCode::NOT_FOUND);
    assert_eq!(not_found_body["error"]["code"], "model_not_found");
    let (_, catalog_body) = error_envelope(ApiError::TierCatalogUnavailable.into_response()).await;
    assert_eq!(catalog_body["error"]["code"], "tier_catalog_unavailable");
}

#[tokio::test]
async fn a_structural_fault_still_refuses_the_whole_catalog() {
    // Only the *economic* verdict is per tier. A duplicate concrete id is
    // cross-tier by nature and makes the file ambiguous about what a customer
    // buys, so it still refuses everything — including the healthy tier — even
    // though it is spliced into a tier that is itself below cost.
    let source = format!(
        r#"{}
[[tiers."zero/fixture-dear".candidates]]
id = "fireworks/healthy"
provider = "fireworks"
model = "upstream/healthy"
[tiers."zero/fixture-dear".candidates.rates]
input_per_mtok = 0.10
output_per_mtok = 0.20
"#,
        two_tier_source("3.00")
    );
    let path = catalog_fixture("structural_fault", &source).await;

    assert!(
        load_tier_catalog(&path).await.is_err(),
        "a duplicate concrete id must refuse the whole catalog"
    );

    let response = app(RouterState::new(path))
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("models request should build"),
        )
        .await
        .expect("models request should complete");
    let (status, body) = error_envelope(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "tier_catalog_unavailable");
}

#[tokio::test]
async fn a_catalog_with_no_servable_tier_left_refuses_to_load() {
    // Degrading every tier is not degradation, it is an outage that pretends
    // to be a catalog: an empty listing would 404 every model in the file.
    // Fail the load instead.
    let source = two_tier_source("3.00").replace(
        r#"[tiers."zero/fixture-healthy".candidates.rates]
input_per_mtok = 0.50"#,
        r#"[tiers."zero/fixture-healthy".candidates.rates]
input_per_mtok = 9.00"#,
    );
    let path = catalog_fixture("all_below_cost", &source).await;

    assert!(
        load_tier_catalog(&path).await.is_err(),
        "a catalog with nothing left to sell must fail to load"
    );
}

#[tokio::test]
async fn sonnet_intro_pricing_expiry_withholds_high_end_and_nothing_else() {
    // The scheduled outage this whole mechanism exists for, simulated against
    // the *shipped* table. `config/tiers.toml` records that Sonnet 5's
    // introductory pricing ends after 2026-08-31; when it lapses both sonnet
    // rows go above zero/high-end's sell rates. Here the two candidate bases
    // (and only they — the tier's own sell rates are left exactly as shipped)
    // are raised to the post-intro standard rate, and the catalog must lose
    // zero/high-end alone while zero/low-cost and zero/balanced keep serving.
    let shipped = tokio::fs::read_to_string(tier_config_path())
        .await
        .expect("shipped catalog should read");
    let split = shipped
        .find(r#"[[tiers."zero/high-end".candidates]]"#)
        .expect("the shipped catalog should define high-end candidates");
    let (head, candidates) = shipped.split_at(split);
    let lapsed = format!(
        "{head}{}",
        candidates
            .replace("input_per_mtok = 2.00", "input_per_mtok = 3.00")
            .replace(
                "cached_input_per_mtok = 0.20",
                "cached_input_per_mtok = 0.30"
            )
            .replace("output_per_mtok = 10.00", "output_per_mtok = 15.00")
    );
    let path = catalog_fixture("sonnet_intro_lapsed", &lapsed).await;

    let catalog = load_tier_catalog(&path)
        .await
        .expect("a lapsed high-end intro price must not take the catalog down");

    // The two tiers that have nothing to do with Sonnet's pricing still route,
    // through their tier ids and through their pinned candidates alike.
    assert_eq!(
        catalog.tiers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["zero/balanced", "zero/low-cost"]
    );
    for model in [
        "zero/low-cost",
        "zero/balanced",
        "fireworks/accounts/fireworks/models/minimax-m3",
        "deepinfra/deepseek-ai/DeepSeek-V4-Pro",
    ] {
        assert!(
            catalog.resolve(model).is_some(),
            "{model} must still resolve after high-end goes below cost"
        );
    }

    // High-end is withheld, and says so for its tier id and for either sonnet
    // row pinned directly.
    for requested in [
        "zero/high-end",
        "anthropic/claude-sonnet-5",
        "bedrock/us.anthropic.claude-sonnet-5",
    ] {
        assert!(catalog.resolve(requested).is_none(), "{requested}");
        let withheld = catalog
            .unavailable_for(requested)
            .unwrap_or_else(|| panic!("{requested} should report zero/high-end as withheld"));
        assert_eq!(withheld.tier, "zero/high-end");
        assert!(
            withheld
                .reason
                .contains("cost basis 3 exceeds tier sell rate 2"),
            "{}",
            withheld.reason
        );
    }
    // The tier's own sell rates are untouched — only the cost basis moved,
    // which is exactly what the calendar does to this table.
    let sell = catalog.unavailable["zero/high-end"].definition.rates;
    assert_eq!(sell.input_per_mtok, Some(2.00));
    assert_eq!(sell.output_per_mtok, Some(10.00));

    // And the public catalog stops advertising what it cannot serve: the two
    // surviving tier ids plus their nine candidates, with no sonnet row.
    let listed = listed_model_ids(RouterState::new(path)).await;
    assert_eq!(listed.len(), 11);
    assert!(listed.iter().any(|id| id == "zero/low-cost"));
    assert!(listed.iter().any(|id| id == "zero/balanced"));
    assert!(
        !listed
            .iter()
            .any(|id| id.contains("sonnet") || id == "zero/high-end"),
        "{listed:?}"
    );
}

#[tokio::test]
async fn the_shipped_catalog_withholds_no_tier_today() {
    // Regression guard for the mechanism above: withholding is a response to a
    // real mispricing, never something the shipped table trips on its own. If
    // this fails, the product lost a tier in production.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");

    assert!(
        catalog.unavailable.is_empty(),
        "the shipped catalog withholds {:?}",
        catalog.unavailable.keys().collect::<Vec<_>>()
    );
    assert_eq!(catalog.tiers.len(), 3);
}

#[tokio::test]
async fn every_shipped_tier_keeps_two_candidates_across_two_providers() {
    // Availability floor for the removal above: a tier with one rung, or two
    // rungs behind a single provider credential, has no fallback when that
    // upstream is down.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");

    for (tier_id, tier) in &catalog.tiers {
        let providers = tier
            .candidates
            .iter()
            .map(|candidate| candidate.provider.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            tier.candidates.len() >= 2 && providers.len() >= 2,
            "{tier_id} has {} candidate(s) across {} provider(s)",
            tier.candidates.len(),
            providers.len()
        );
    }
}

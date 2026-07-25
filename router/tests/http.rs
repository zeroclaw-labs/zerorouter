use std::{path::PathBuf, str::FromStr};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;
use zerorouter::{RouterState, app, load_tier_catalog};

fn tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/tiers.toml")
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
    // 3 tier ids + 9 concrete candidates (4 low-cost, 3 balanced, 2 high-end).
    assert_eq!(data.len(), 12);
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

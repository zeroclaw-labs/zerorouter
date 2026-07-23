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
    let response = app(RouterState::with_database(tier_config_path(), pool))
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
    assert_eq!(data.len(), 15);
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
        [
            (
                "bedrock/us.anthropic.claude-sonnet-5",
                "us.anthropic.claude-sonnet-5"
            ),
            (
                "bedrock/us.anthropic.claude-opus-4-8",
                "us.anthropic.claude-opus-4-8"
            ),
        ]
    );
}

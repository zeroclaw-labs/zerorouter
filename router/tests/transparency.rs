//! The transparency report against a fixture ECS metadata endpoint: the
//! provenance chain's fields, the degraded shapes, and the promise that the
//! endpoint never oversells itself (the caveat travels with every payload).

use axum::{Router, routing::get};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use zerorouter::{RouterState, app, transparency::build_report};

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture must bind");
    let address = listener.local_addr().expect("fixture address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{address}")
}

/// A metadata endpoint answering like ECS container metadata v4 does.
async fn ecs_fixture(image: &str, digest: &str) -> String {
    let payload = json!({ "Image": image, "ImageDigest": digest });
    serve(Router::new().route(
        "/",
        get(move || {
            let payload = payload.clone();
            async move { axum::Json(payload) }
        }),
    ))
    .await
}

#[tokio::test]
async fn the_full_chain_is_reported_when_everything_is_known() {
    let registry = "123456789012.dkr.ecr.us-east-1.amazonaws.com/zerorouter-beta";
    let digest = "sha256:5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8";
    let metadata = ecs_fixture(&format!("{registry}:deadbeef"), digest).await;

    let report = build_report(Some(&metadata), Some("deadbeefcafe")).await;

    assert_eq!(report.source_commit.as_deref(), Some("deadbeefcafe"));
    assert_eq!(
        report.source.as_deref(),
        Some("https://github.com/zeroclaw-labs/zerorouter/tree/deadbeefcafe")
    );
    assert_eq!(report.image_digest.as_deref(), Some(digest));
    assert_eq!(
        report.attestations.as_deref(),
        Some(
            format!("https://api.github.com/repos/zeroclaw-labs/zerorouter/attestations/{digest}")
                .as_str()
        )
    );
    // The verify line must be runnable verbatim: digest-pinned reference,
    // tag stripped, repo named.
    assert_eq!(
        report.verify.as_deref(),
        Some(
            format!(
                "gh attestation verify oci://{registry}@{digest} --repo zeroclaw-labs/zerorouter"
            )
            .as_str()
        )
    );
    assert!(
        report.caveat.contains("nothing here yet proves"),
        "the caveat must state the claim's limits, not just its strength"
    );
}

#[tokio::test]
async fn a_dead_metadata_endpoint_degrades_to_unknown_not_error() {
    let metadata = serve(Router::new().route(
        "/",
        get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "no") }),
    ))
    .await;

    let report = build_report(Some(&metadata), Some("deadbeefcafe")).await;

    assert_eq!(report.image, None);
    assert_eq!(report.image_digest, None);
    assert_eq!(report.attestations, None);
    assert_eq!(report.verify, None);
    // The build-time half of the chain survives a runtime metadata outage.
    assert_eq!(report.source_commit.as_deref(), Some("deadbeefcafe"));
}

#[tokio::test]
async fn a_local_run_reports_honest_nothing() {
    let report = build_report(None, None).await;
    assert_eq!(report.source_commit, None);
    assert_eq!(report.source, None);
    assert_eq!(report.image_digest, None);
    assert_eq!(report.verify, None);
    assert!(!report.caveat.is_empty());
}

/// Whitespace-only baked commits (a local build with the arg defaulted to
/// empty) read as absent, not as an empty-string "commit".
#[tokio::test]
async fn an_empty_baked_commit_is_absent_not_empty() {
    let report = build_report(None, Some("  ")).await;
    assert_eq!(report.source_commit, None);
    assert_eq!(report.source, None);
}

/// The route is wired into the always-on plane next to /healthz and answers
/// without auth, without a database, and without ECS.
#[tokio::test]
async fn the_endpoint_is_public_and_always_on() {
    let tiers = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/tiers.toml");
    let response = app(RouterState::new(tiers))
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/transparency")
                .body(axum::body::Body::empty())
                .expect("request must build"),
        )
        .await
        .expect("router must answer");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body must collect")
        .to_bytes();
    let payload: Value = serde_json::from_slice(&body).expect("payload must be JSON");
    assert!(
        payload
            .get("caveat")
            .and_then(Value::as_str)
            .is_some_and(|caveat| !caveat.is_empty()),
        "the caveat must travel with every payload"
    );
}

/// The live-ECS shape first observed in production: `ImageDigest` empty, but
/// the task pinned to a digest-carrying image reference. The digest embedded
/// in the reference completes the chain instead of leaving it null.
#[tokio::test]
async fn a_digest_pinned_reference_fills_a_missing_image_digest() {
    let registry = "161457899654.dkr.ecr.us-east-1.amazonaws.com/zerorouter-beta-router";
    let digest = "sha256:14b5dc3374f8471df705acba7b0dcc5216cdfdb81dcbaee0f329ef72f9409a81";
    let payload = json!({ "Image": format!("{registry}:2f3dde1@{digest}"), "ImageDigest": "" });
    let metadata = serve(Router::new().route(
        "/",
        get(move || {
            let payload = payload.clone();
            async move { axum::Json(payload) }
        }),
    ))
    .await;

    let report = build_report(Some(&metadata), Some("2f3dde1")).await;

    assert_eq!(report.image_digest.as_deref(), Some(digest));
    assert_eq!(
        report.verify.as_deref(),
        Some(
            format!(
                "gh attestation verify oci://{registry}@{digest} --repo zeroclaw-labs/zerorouter"
            )
            .as_str()
        )
    );
    assert!(
        report
            .attestations
            .as_deref()
            .is_some_and(|url| url.ends_with(digest))
    );
}

/// A digest-less, non-sha256 suffix after `@` is not a digest and must not be
/// promoted into one.
#[tokio::test]
async fn a_non_sha256_suffix_is_not_mistaken_for_a_digest() {
    let payload = json!({ "Image": "registry.example/app@sig-abcdef", "ImageDigest": "" });
    let metadata = serve(Router::new().route(
        "/",
        get(move || {
            let payload = payload.clone();
            async move { axum::Json(payload) }
        }),
    ))
    .await;

    let report = build_report(Some(&metadata), None).await;
    assert_eq!(report.image_digest, None);
    assert_eq!(report.verify, None);
}

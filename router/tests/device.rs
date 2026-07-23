//! Integration tests for the RFC 8628 device-authorization flow.
//!
//! Skipped (return early) unless `DATABASE_URL` is set, matching the
//! convention in `tests/postgres.rs`. Every flow uses freshly generated
//! codes and unique emails, so parallel runs do not collide.

use std::{path::PathBuf, str::FromStr, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    auth::{KeyAuthenticator, hash_api_key},
    db::migrate,
    device,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
    web::{WebConfig, WebCtx},
};

const BASE_URL: &str = "https://router.test.invalid";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

async fn connect(database_url: &str) -> PgPool {
    let options = PgConnectOptions::from_str(database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    pool
}

fn web_router(pool: PgPool) -> Router {
    let config = WebConfig {
        public_base_url: BASE_URL.to_owned(),
        secure_cookies: true,
        oidc: None,
        stripe: None,
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    };
    device::router().with_state(WebCtx::new(pool, config))
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("request should complete");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should be readable")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).expect("response body should be JSON");
    (status, body)
}

fn form_request(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("form request should build")
}

fn json_request(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("json request should build")
}

fn portal_request(uri: &str, session_token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={session_token}"))
        .header(CSRF_HEADER, "1")
        .body(Body::from(body.to_string()))
        .expect("portal request should build")
}

/// Start a grant over the RFC form encoding and return (device_code, user_code).
async fn start_authorization(router: &Router, key_name: Option<&str>) -> (String, String) {
    let mut body = "client_id=zeroclaw".to_owned();
    if let Some(key_name) = key_name {
        body.push_str("&key_name=");
        body.push_str(key_name);
    }
    let (status, response) = send(router, form_request("/auth/device/code", body)).await;
    assert_eq!(status, StatusCode::OK, "device code start: {response}");
    let device_code = response["device_code"]
        .as_str()
        .expect("response should carry device_code")
        .to_owned();
    let user_code = response["user_code"]
        .as_str()
        .expect("response should carry user_code")
        .to_owned();
    assert_eq!(response["verification_uri"], format!("{BASE_URL}/activate"));
    assert_eq!(
        response["verification_uri_complete"],
        format!("{BASE_URL}/activate?code={user_code}")
    );
    assert_eq!(response["expires_in"], 900);
    assert_eq!(response["interval"], 5);
    (device_code, user_code)
}

async fn poll_token(router: &Router, device_code: &str, client_id: &str) -> (StatusCode, Value) {
    send(
        router,
        json_request(
            "/auth/device/token",
            json!({
                "grant_type": GRANT_TYPE,
                "device_code": device_code,
                "client_id": client_id,
            }),
        ),
    )
    .await
}

/// Create a portal user with a live session; returns (user_id, session token).
async fn portal_session(pool: &PgPool) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    let email = format!("device-{user_id}@example.invalid");
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(email)
        .execute(pool)
        .await
        .expect("test user must insert");
    let (token, _expires_at) = create_session(pool, user_id, Duration::from_secs(3600))
        .await
        .expect("portal session must create");
    (user_id, token)
}

async fn force_expire(pool: &PgPool, user_code: &str) {
    query(
        r#"
        UPDATE device_authorizations
        SET created_at = NOW() - INTERVAL '2 hours', expires_at = NOW() - INTERVAL '1 hour'
        WHERE user_code = $1
        "#,
    )
    .bind(user_code)
    .execute(pool)
    .await
    .expect("device authorization must force-expire");
}

#[tokio::test]
async fn device_flow_mints_a_single_use_key_at_claim_time() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url).await;
    let router = web_router(pool.clone());

    let (device_code, user_code) = start_authorization(&router, Some("integration")).await;
    assert!(device_code.starts_with("zdc_"));
    assert_eq!(device_code.len(), 68);
    assert_eq!(user_code.len(), 9);
    assert_eq!(user_code.as_bytes()[4], b'-');

    // The device code itself never rests in the database.
    let plaintext_hits = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM device_authorizations WHERE device_code_hash = $1 OR user_code = $1",
    )
    .bind(&device_code)
    .fetch_one(&pool)
    .await
    .expect("plaintext check must query");
    assert_eq!(plaintext_hits, 0);

    // Pending, then immediately again: pending → slow_down.
    let (status, body) = poll_token(&router, &device_code, "zeroclaw").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "authorization_pending");
    let (status, body) = poll_token(&router, &device_code, "zeroclaw").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "slow_down");

    // Portal lookup accepts a sloppy (lowercase, dashless) user code.
    let (user_id, session_token) = portal_session(&pool).await;
    let sloppy_code = user_code.replace('-', "").to_lowercase();
    let (status, body) = send(
        &router,
        portal_request(
            "/api/device/lookup",
            &session_token,
            json!({ "user_code": sloppy_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "lookup: {body}");
    assert_eq!(body["client_id"], "zeroclaw");
    assert_eq!(body["key_name"], "integration");
    assert!(body["created_at"].is_string());

    // Approve, then claim: the poll returns a freshly minted zcr_ key.
    let (status, body) = send(
        &router,
        portal_request(
            "/api/device/approve",
            &session_token,
            json!({ "user_code": user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "approve: {body}");
    assert_eq!(body["status"], "approved");

    let (status, body) = poll_token(&router, &device_code, "zeroclaw").await;
    assert_eq!(status, StatusCode::OK, "claim: {body}");
    let api_key = body["access_token"]
        .as_str()
        .expect("claim should return an access token")
        .to_owned();
    assert!(api_key.starts_with("zcr_"));
    assert_eq!(api_key.len(), 68);
    assert_eq!(body["token_type"], "bearer");
    assert_eq!(body["scope"], "inference");

    // The minted key authenticates and belongs to the approving user.
    let authenticated = KeyAuthenticator::new()
        .authenticate(&pool, &api_key)
        .await
        .expect("minted key must authenticate");
    let key_user_id = query_scalar::<_, Uuid>("SELECT user_id FROM api_keys WHERE key_hash = $1")
        .bind(hash_api_key(&api_key))
        .fetch_one(&pool)
        .await
        .expect("minted key owner must query");
    assert_eq!(key_user_id, user_id);
    let stored_name = query_scalar::<_, String>("SELECT name FROM api_keys WHERE id = $1")
        .bind(authenticated.id)
        .fetch_one(&pool)
        .await
        .expect("minted key name must query");
    assert_eq!(stored_name, "integration");

    // The grant is claimed and linked to the minted key.
    let grant_status =
        query_scalar::<_, String>("SELECT status FROM device_authorizations WHERE user_code = $1")
            .bind(&user_code)
            .fetch_one(&pool)
            .await
            .expect("grant status must query");
    assert_eq!(grant_status, "claimed");
    let linked_key = query_scalar::<_, Option<Uuid>>(
        "SELECT api_key_id FROM device_authorizations WHERE user_code = $1",
    )
    .bind(&user_code)
    .fetch_one(&pool)
    .await
    .expect("linked key must query");
    assert_eq!(linked_key, Some(authenticated.id));

    // Single use: a second poll after the claim yields invalid_grant.
    let (status, body) = poll_token(&router, &device_code, "zeroclaw").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // And the claimed code is no longer visible to the portal.
    let (status, _body) = send(
        &router,
        portal_request(
            "/api/device/lookup",
            &session_token,
            json!({ "user_code": user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_device_codes_cannot_be_polled_or_approved() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url).await;
    let router = web_router(pool.clone());

    // Polling an expired grant reports expired_token and flips the status.
    let (device_code, user_code) = start_authorization(&router, None).await;
    force_expire(&pool, &user_code).await;
    let (status, body) = poll_token(&router, &device_code, "zeroclaw").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "expired_token");
    let stored_status =
        query_scalar::<_, String>("SELECT status FROM device_authorizations WHERE user_code = $1")
            .bind(&user_code)
            .fetch_one(&pool)
            .await
            .expect("expired grant status must query");
    assert_eq!(stored_status, "expired");

    // Approving (or looking up) an expired code fails.
    let (_device_code, stale_user_code) = start_authorization(&router, None).await;
    force_expire(&pool, &stale_user_code).await;
    let (_user_id, session_token) = portal_session(&pool).await;
    let (status, body) = send(
        &router,
        portal_request(
            "/api/device/approve",
            &session_token,
            json!({ "user_code": stale_user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "device_code_not_found");
    let (status, _body) = send(
        &router,
        portal_request(
            "/api/device/lookup",
            &session_token,
            json!({ "user_code": stale_user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Starting a new grant sweeps stale pending rows to 'expired'.
    let (_device_code, swept_user_code) = start_authorization(&router, None).await;
    force_expire(&pool, &swept_user_code).await;
    let (_new_device_code, _new_user_code) = start_authorization(&router, None).await;
    let swept_status =
        query_scalar::<_, String>("SELECT status FROM device_authorizations WHERE user_code = $1")
            .bind(&swept_user_code)
            .fetch_one(&pool)
            .await
            .expect("swept grant status must query");
    assert_eq!(swept_status, "expired");
}

#[tokio::test]
async fn denied_device_codes_report_access_denied() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url).await;
    let router = web_router(pool.clone());

    // Start over JSON this time to cover the JSON body path on /code.
    let (status, response) = send(
        &router,
        json_request("/auth/device/code", json!({ "client_id": "zeroclaw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "device code start: {response}");
    let device_code = response["device_code"]
        .as_str()
        .expect("response should carry device_code")
        .to_owned();
    let user_code = response["user_code"]
        .as_str()
        .expect("response should carry user_code")
        .to_owned();

    let (_user_id, session_token) = portal_session(&pool).await;
    let (status, body) = send(
        &router,
        portal_request(
            "/api/device/deny",
            &session_token,
            json!({ "user_code": user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "deny: {body}");
    assert_eq!(body["status"], "denied");

    // Poll over the RFC form encoding to cover the form body path on /token.
    let (status, body) = send(
        &router,
        form_request(
            "/auth/device/token",
            format!(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={device_code}&client_id=zeroclaw"
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "access_denied");

    // A second deny finds nothing pending.
    let (status, _body) = send(
        &router,
        portal_request(
            "/api/device/deny",
            &session_token,
            json!({ "user_code": user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn device_endpoints_validate_client_grant_and_csrf() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url).await;
    let router = web_router(pool.clone());

    // Unknown client id is rejected before any row is created.
    let (status, body) = send(
        &router,
        form_request("/auth/device/code", "client_id=intruder".to_owned()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client");

    // Unsupported grant types are rejected.
    let (status, body) = send(
        &router,
        json_request(
            "/auth/device/token",
            json!({
                "grant_type": "authorization_code",
                "device_code": "zdc_0000",
                "client_id": "zeroclaw",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unsupported_grant_type");

    // A well-formed but unknown device code is invalid_grant.
    let unknown = format!("zdc_{}", "0".repeat(64));
    let (status, body) = poll_token(&router, &unknown, "zeroclaw").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // A real device code polled with the wrong client id is invalid_grant.
    let (device_code, _user_code) = start_authorization(&router, None).await;
    let (status, body) = poll_token(&router, &device_code, "someone-else").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // Discovery document advertises the flow in the expected shape.
    let (status, body) = send(
        &router,
        Request::builder()
            .uri("/.well-known/openid-configuration")
            .body(Body::empty())
            .expect("discovery request should build"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["issuer"], BASE_URL);
    assert_eq!(
        body["device_authorization_endpoint"],
        format!("{BASE_URL}/auth/device/code")
    );
    assert_eq!(
        body["token_endpoint"],
        format!("{BASE_URL}/auth/device/token")
    );
    assert_eq!(
        body["authorization_endpoint"],
        format!("{BASE_URL}/auth/login")
    );
    assert_eq!(body["grant_types_supported"], json!([GRANT_TYPE]));
    assert_eq!(body["scopes_supported"], json!(["inference"]));
    assert_eq!(
        body["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );

    // Portal endpoints reject requests without the CSRF header outright.
    let (status, body) = send(
        &router,
        json_request("/api/device/lookup", json!({ "user_code": "BCDF-GHJK" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "csrf_rejected");
}

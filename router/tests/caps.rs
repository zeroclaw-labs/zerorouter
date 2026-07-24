//! Cap-bypass regression tests: quotas are counted against the USER, and key
//! creation is throttled so churning keys cannot reset a quota.
//!
//! Gated on `DATABASE_URL` like `tests/postgres.rs`: when unset each test
//! returns early (skips) instead of failing.
//!
//! The bypass these cover, end to end: spend and velocity used to be projected
//! against the presenting key alone, keys were free (the active-key cap counted
//! only non-disabled rows, and disabling is a flag flip), and the device-claim
//! mint path skipped the cap entirely. So a user could exhaust a key's monthly
//! spend, disable it, mint a fresh one and continue indefinitely — or hold many
//! keys at once, each carrying its own full velocity allowance.

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
use zeroclaw_providers::pricing::ModelRates;
use zerorouter::{
    auth::{AuthenticatedKey, generate_api_key, hash_api_key},
    db::{
        KEY_CREATION_WINDOW_HOURS, MAX_KEYS_CREATED_PER_WINDOW, RequestTelemetry, UsageAdmission,
        UsageRecord, begin_usage_session, migrate,
    },
    device,
    openai::OpenAiUsage,
    portal,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
    web::{WebConfig, WebCtx},
};

const TEST_SIGNATURE: &str = "0123456789abcdef";
const BASE_URL: &str = "https://caps.test.invalid";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

async fn create_user(pool: &PgPool, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("caps-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

/// A live key with explicit caps, returned in the shape admission consumes.
async fn create_key(
    pool: &PgPool,
    user_id: Uuid,
    spend_cap_usd: Decimal,
    velocity_cap_tokens_per_min: i32,
) -> AuthenticatedKey {
    let key_id = Uuid::new_v4();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'caps-integration', $4, $5)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .bind(spend_cap_usd)
    .bind(velocity_cap_tokens_per_min)
    .execute(pool)
    .await
    .expect("test API key must insert");
    AuthenticatedKey {
        id: key_id,
        user_id,
    }
}

/// Seed a key the user has already disabled, created `hours_ago` in the past.
async fn seed_disabled_key(pool: &PgPool, user_id: Uuid, hours_ago: i64) {
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, disabled, created_at)
        VALUES ($1, $2, $3, 'churned', TRUE, NOW() - ($4 * INTERVAL '1 hour'))
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .bind(hours_ago)
    .execute(pool)
    .await
    .expect("churned key must insert");
}

fn usage_record(cost_usd: Decimal) -> UsageRecord {
    UsageRecord {
        tier: "zero/test".to_owned(),
        upstream_provider: "test".to_owned(),
        upstream_model: "test/model".to_owned(),
        usage: OpenAiUsage {
            prompt_tokens: 100,
            completion_tokens: 25,
            total_tokens: 125,
            prompt_tokens_details: None,
        },
        cost_usd,
        latency_ms: 10,
        status: 200,
        telemetry: RequestTelemetry {
            requested_max_tokens: 4096,
            stream: false,
            prompt_bytes: 128,
            message_count: 1,
            tool_count: 0,
            candidate_id: None,
            basis_rates: None,
            sell_rates: ModelRates {
                input_per_mtok: Some(2.0),
                output_per_mtok: Some(10.0),
                cached_input_per_mtok: Some(0.2),
            },
            finish_reason: None,
            shape_ok: None,
        },
        attempts: Vec::new(),
    }
}

/// Admit a reservation in cap-only mode (no credits required).
async fn admit(
    pool: &PgPool,
    key: &AuthenticatedKey,
    reserved_tokens: i64,
    reserved_cost_usd: Decimal,
) -> UsageAdmission {
    begin_usage_session(
        pool,
        key,
        reserved_tokens,
        reserved_tokens.min(64),
        reserved_cost_usd,
        TEST_SIGNATURE.to_owned(),
        false,
    )
    .await
    .expect("admission must query")
}

/// Admit and immediately settle `cost_usd` of real usage against `key`.
async fn spend(pool: &PgPool, key: &AuthenticatedKey, cost_usd: Decimal) {
    let UsageAdmission::Allowed(session) = admit(pool, key, 1_000, cost_usd).await else {
        panic!("the first spend against a fresh cap should be admitted");
    };
    session
        .record(&usage_record(cost_usd))
        .await
        .expect("settlement must succeed");
}

// ---------------------------------------------------------------------------
// Quota scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_keys_of_one_user_share_a_single_spend_cap() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "shared-spend").await;
    let first = create_key(&pool, user_id, Decimal::from(20), 1_000_000).await;
    let second = create_key(&pool, user_id, Decimal::from(20), 1_000_000).await;

    // Exhaust the user's ceiling entirely through the first key.
    spend(&pool, &first, Decimal::from(20)).await;

    // The second key has spent nothing of its own, so the per-key projection
    // would still admit it; the user-scoped projection must not.
    let second_key_spend = query_scalar::<_, Decimal>(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events WHERE api_key_id = $1",
    )
    .bind(second.id)
    .fetch_one(&pool)
    .await
    .expect("second key spend must query");
    assert_eq!(second_key_spend, Decimal::ZERO);
    assert!(matches!(
        admit(&pool, &second, 10, Decimal::ONE).await,
        UsageAdmission::SpendExceeded
    ));

    // Minting a *third* key is not an escape hatch either: a fresh key carries
    // no fresh allowance, which is the whole disable-and-remint attack.
    let third = create_key(&pool, user_id, Decimal::from(20), 1_000_000).await;
    assert!(matches!(
        admit(&pool, &third, 10, Decimal::ONE).await,
        UsageAdmission::SpendExceeded
    ));
    // Not even a zero-cost request slips past an exhausted ceiling.
    assert!(matches!(
        admit(&pool, &third, 10, Decimal::ZERO).await,
        UsageAdmission::SpendExceeded
    ));
}

#[tokio::test]
async fn spend_caps_of_different_users_do_not_interfere() {
    let Some(pool) = connect().await else {
        return;
    };
    let spender = create_user(&pool, "spender").await;
    let bystander = create_user(&pool, "bystander").await;
    let spender_key = create_key(&pool, spender, Decimal::from(20), 1_000_000).await;
    let bystander_key = create_key(&pool, bystander, Decimal::from(20), 1_000_000).await;

    spend(&pool, &spender_key, Decimal::from(20)).await;
    assert!(matches!(
        admit(&pool, &spender_key, 10, Decimal::ONE).await,
        UsageAdmission::SpendExceeded
    ));

    // The other tenant is untouched: user scoping must not become global
    // scoping.
    let UsageAdmission::Allowed(session) = admit(&pool, &bystander_key, 10, Decimal::ONE).await
    else {
        panic!("another user's exhausted cap must not block this one");
    };
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("bystander settlement must succeed");
}

#[tokio::test]
async fn velocity_is_projected_across_every_key_a_user_holds() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "velocity").await;
    let first = create_key(&pool, user_id, Decimal::from(1_000), 1_000).await;
    let second = create_key(&pool, user_id, Decimal::from(1_000), 1_000).await;

    // 800 in flight on the first key. Per-key projection on the second key sees
    // nothing; the user-scoped one sees 800 + 800 > 1000.
    let UsageAdmission::Allowed(session) = admit(&pool, &first, 800, Decimal::ZERO).await else {
        panic!("the first reservation should be admitted");
    };
    assert!(matches!(
        admit(&pool, &second, 800, Decimal::ZERO).await,
        UsageAdmission::VelocityExceeded
    ));
    // What still fits under the shared ceiling is admitted, so the gate is a
    // shared budget and not a blanket refusal.
    let UsageAdmission::Allowed(fitting) = admit(&pool, &second, 200, Decimal::ZERO).await else {
        panic!("a reservation that fits the shared ceiling should be admitted");
    };

    fitting
        .record(&usage_record(Decimal::ZERO))
        .await
        .expect("fitting reservation must settle");
    session
        .record(&usage_record(Decimal::ZERO))
        .await
        .expect("in-flight reservation must settle");
}

#[tokio::test]
async fn a_key_stays_bound_by_its_own_cap_under_the_user_ceiling() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "both-scopes").await;
    // The user ceiling is derived as the largest live key cap (50 here), so the
    // small key is still held to its own 5 — both scopes are enforced and the
    // tighter one wins.
    let small = create_key(&pool, user_id, Decimal::from(5), 1_000_000).await;
    let large = create_key(&pool, user_id, Decimal::from(50), 1_000_000).await;

    spend(&pool, &small, Decimal::from(5)).await;
    assert!(
        matches!(
            admit(&pool, &small, 10, Decimal::ONE).await,
            UsageAdmission::SpendExceeded
        ),
        "the small key is exhausted at its own cap, well under the user ceiling"
    );

    // The large key still has room under the user ceiling (5 of 50 spent).
    let UsageAdmission::Allowed(session) = admit(&pool, &large, 10, Decimal::from(10)).await else {
        panic!("the large key should still fit under the user ceiling");
    };
    session
        .record(&usage_record(Decimal::from(10)))
        .await
        .expect("large key settlement must succeed");

    // ...and once the user ceiling is reached, the large key is refused too.
    spend(&pool, &large, Decimal::from(35)).await;
    assert!(matches!(
        admit(&pool, &large, 10, Decimal::ONE).await,
        UsageAdmission::SpendExceeded
    ));
}

#[tokio::test]
async fn a_single_key_user_sees_the_pre_change_behavior() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "single-key").await;
    let key = create_key(&pool, user_id, Decimal::from(20), 100_000).await;

    // The derived user ceiling is this key's own cap, so admission is exactly
    // the B0 check: one reservation of 1 admitted, a follow-on of 20 refused.
    let UsageAdmission::Allowed(session) = admit(&pool, &key, 1_000, Decimal::ONE).await else {
        panic!("the first reservation should be admitted");
    };
    assert!(matches!(
        admit(&pool, &key, 1_000, Decimal::from(20)).await,
        UsageAdmission::SpendExceeded
    ));
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("reservation must settle into usage");
}

// ---------------------------------------------------------------------------
// Key creation throttle
// ---------------------------------------------------------------------------

fn test_web_config() -> WebConfig {
    WebConfig {
        public_base_url: BASE_URL.to_owned(),
        secure_cookies: true,
        oidc: None,
        stripe: None,
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3_600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    }
}

fn portal_app(pool: &PgPool) -> Router {
    portal::router().with_state(WebCtx::new(pool.clone(), test_web_config()))
}

fn device_app(pool: &PgPool) -> Router {
    device::router().with_state(WebCtx::new(pool.clone(), test_web_config()))
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body should be JSON")
    };
    (status, body)
}

fn post_json(uri: &str, cookie: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .header(CSRF_HEADER, "1")
        .body(Body::from(body.to_string()))
        .expect("POST request should build")
}

async fn portal_cookie(pool: &PgPool, user_id: Uuid) -> String {
    let (token, _) = create_session(pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("portal session must create");
    format!("{SESSION_COOKIE}={token}")
}

/// `(live keys, keys ever created)` for a user.
async fn key_counts(pool: &PgPool, user_id: Uuid) -> (i64, i64) {
    let active =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM api_keys WHERE user_id = $1 AND NOT disabled")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("active key count must query");
    let total = query_scalar::<_, i64>("SELECT COUNT(*) FROM api_keys WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("total key count must query");
    (active, total)
}

#[tokio::test]
async fn disabling_keys_does_not_reset_the_portal_creation_limit() {
    let Some(pool) = connect().await else {
        return;
    };
    let app = portal_app(&pool);
    let user_id = create_user(&pool, "churn").await;
    let cookie = portal_cookie(&pool, user_id).await;

    // The attack: mint, exhaust, disable, repeat. Simulated at its end state —
    // every key the user ever made is disabled, so the active-key cap sees an
    // empty account.
    for _ in 0..MAX_KEYS_CREATED_PER_WINDOW {
        seed_disabled_key(&pool, user_id, 0).await;
    }
    let (active, total) = key_counts(&pool, user_id).await;
    assert_eq!(active, 0, "no live keys: the active-key cap is satisfied");
    assert_eq!(total, MAX_KEYS_CREATED_PER_WINDOW);

    let (status, body) = send(
        &app,
        post_json("/api/keys", &cookie, json!({ "name": "remint" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "disabled keys must count against the creation limit"
    );
    assert_eq!(body["error"]["code"], "key_limit_reached");
    assert_eq!(
        key_counts(&pool, user_id).await.1,
        MAX_KEYS_CREATED_PER_WINDOW,
        "a refused mint must write no key row"
    );
}

#[tokio::test]
async fn the_creation_limit_is_a_trailing_window_not_a_lifetime_cap() {
    let Some(pool) = connect().await else {
        return;
    };
    let app = portal_app(&pool);
    let user_id = create_user(&pool, "window").await;
    let cookie = portal_cookie(&pool, user_id).await;

    // The same churn history, but entirely outside the trailing window: a
    // long-lived account that rotated keys months ago is not punished for it.
    for _ in 0..MAX_KEYS_CREATED_PER_WINDOW {
        seed_disabled_key(&pool, user_id, KEY_CREATION_WINDOW_HOURS + 1).await;
    }
    let (status, body) = send(
        &app,
        post_json("/api/keys", &cookie, json!({ "name": "after the window" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mint response: {body}");

    // ...and the freshly created key does count, so the window still closes.
    for _ in 1..MAX_KEYS_CREATED_PER_WINDOW {
        seed_disabled_key(&pool, user_id, 0).await;
    }
    let (status, _) = send(
        &app,
        post_json("/api/keys", &cookie, json!({ "name": "one too many" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn creation_limits_of_different_users_do_not_interfere() {
    let Some(pool) = connect().await else {
        return;
    };
    let app = portal_app(&pool);
    let churner = create_user(&pool, "churner").await;
    let newcomer = create_user(&pool, "newcomer").await;
    for _ in 0..MAX_KEYS_CREATED_PER_WINDOW {
        seed_disabled_key(&pool, churner, 0).await;
    }

    let (status, _) = send(
        &app,
        post_json(
            "/api/keys",
            &portal_cookie(&pool, churner).await,
            json!({ "name": "blocked" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, body) = send(
        &app,
        post_json(
            "/api/keys",
            &portal_cookie(&pool, newcomer).await,
            json!({ "name": "allowed" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mint response: {body}");
}

/// Drive a device grant to the point of claim and return its device code.
async fn approved_device_code(app: &Router, cookie: &str) -> String {
    let (status, body) = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/auth/device/code")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("client_id=zeroclaw"))
            .expect("device code request should build"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "device code start: {body}");
    let device_code = body["device_code"]
        .as_str()
        .expect("response should carry device_code")
        .to_owned();
    let user_code = body["user_code"]
        .as_str()
        .expect("response should carry user_code")
        .to_owned();

    let (status, body) = send(
        app,
        post_json(
            "/api/device/approve",
            cookie,
            json!({ "user_code": user_code }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "approve: {body}");
    device_code
}

async fn claim(app: &Router, device_code: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/auth/device/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "grant_type": GRANT_TYPE,
                    "device_code": device_code,
                    "client_id": "zeroclaw",
                })
                .to_string(),
            ))
            .expect("token request should build"),
    )
    .await
}

#[tokio::test]
async fn the_device_claim_mint_is_subject_to_the_creation_limit() {
    let Some(pool) = connect().await else {
        return;
    };
    let app = device_app(&pool);
    let user_id = create_user(&pool, "device-churn").await;
    let cookie = portal_cookie(&pool, user_id).await;

    // A grant approved before the user hits the limit still cannot mint past
    // it: the check runs at claim time, which is when the key is created.
    let device_code = approved_device_code(&app, &cookie).await;
    for _ in 0..MAX_KEYS_CREATED_PER_WINDOW {
        seed_disabled_key(&pool, user_id, 0).await;
    }

    let (status, body) = claim(&app, &device_code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "access_denied");
    assert_eq!(
        key_counts(&pool, user_id).await.1,
        MAX_KEYS_CREATED_PER_WINDOW,
        "a refused claim must mint no key"
    );
    let grant_status =
        query_scalar::<_, String>("SELECT status FROM device_authorizations WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("grant status must query");
    assert_eq!(
        grant_status, "approved",
        "the refused claim rolls back, leaving the grant unclaimed"
    );
}

#[tokio::test]
async fn device_claims_below_the_creation_limit_still_mint() {
    let Some(pool) = connect().await else {
        return;
    };
    let app = device_app(&pool);
    let user_id = create_user(&pool, "device-ok").await;
    let cookie = portal_cookie(&pool, user_id).await;

    // One short of the limit: the claim must still succeed, so the throttle
    // does not break the ordinary ZeroClaw login.
    for _ in 1..MAX_KEYS_CREATED_PER_WINDOW {
        seed_disabled_key(&pool, user_id, 0).await;
    }
    let device_code = approved_device_code(&app, &cookie).await;
    let (status, body) = claim(&app, &device_code).await;
    assert_eq!(status, StatusCode::OK, "claim: {body}");
    assert!(
        body["access_token"]
            .as_str()
            .expect("claim should return an access token")
            .starts_with("zcr_")
    );
    assert_eq!(
        key_counts(&pool, user_id).await.1,
        MAX_KEYS_CREATED_PER_WINDOW
    );
}

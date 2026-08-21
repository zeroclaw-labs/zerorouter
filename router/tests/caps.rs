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
use zerorouter::provider::ModelRates;
use zerorouter::{
    auth::{AuthenticatedKey, generate_api_key, hash_api_key},
    db::{
        KEY_CREATION_WINDOW_HOURS, MAX_KEYS_CREATED_PER_WINDOW, MeteringLane, RequestTelemetry,
        ReservationSize, ReservationSizing, UsageAdmission, UsageRecord, begin_usage_session,
        migrate,
    },
    device,
    openai::{
        OpenAiUsage, PromptTokenDetails, TASK_SIGNATURE_SCHEME, TaskSignature, tool_names_digest,
    },
    portal,
    priority::Priority,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
    web::{WebConfig, WebCtx},
};

/// The pre-Stage-4 sizing: one measured bound, offered as the full ceiling
/// with no learned alternative for admission to choose between.
fn cold_sizing(total_tokens: i64, output_tokens: i64, cost_usd: Decimal) -> ReservationSizing {
    ReservationSizing::cold(ReservationSize {
        total_tokens,
        output_tokens,
        cost_usd,
    })
}

/// A fixed segment key for tests that only need the reservation to carry one.
fn test_signature() -> TaskSignature {
    TaskSignature {
        hex: "0123456789abcdef".to_owned(),
        scheme: TASK_SIGNATURE_SCHEME,
        tool_names_sha256: tool_names_digest(&[]),
    }
}
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
        default_priority: None,
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
            finish_reason_source: None,
            usage_gap: None,
            shape_ok: None,
            priority: Some(Priority::Balanced),
            // `None`, not `Some(false)`: these fixtures describe requests from before
            // BYOK existed, so they keep pinning the pre-BYOK settled row exactly, and
            // they exercise the NULL arm of the new column while they are at it.
            byok: None,
        },
        attempts: Vec::new(),
    }
}

/// Admit a reservation in cap-only mode (no credits required).
///
/// Cap-only is passed explicitly here because it is no longer the default:
/// `ZEROROUTER_REQUIRE_CREDITS` defaults to `true`, and cap-only is reached
/// only by opting out (`router/src/web.rs`). These tests want the caps alone
/// under test, which is exactly the deployment shape that opt-out produces —
/// and exactly why the caps below have to hold on their own.
async fn admit(
    pool: &PgPool,
    key: &AuthenticatedKey,
    reserved_tokens: i64,
    reserved_cost_usd: Decimal,
) -> UsageAdmission {
    begin_usage_session(
        pool,
        key,
        cold_sizing(reserved_tokens, reserved_tokens.min(64), reserved_cost_usd),
        test_signature(),
        false,
        MeteringLane::Reserved,
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
// Revocation
// ---------------------------------------------------------------------------

/// The revoke-and-dispatch race, staged deterministically.
///
/// Admission used to read `disabled` with an unlocked SELECT, so the interleave
/// below admitted: the SELECT saw `disabled = false` under its own snapshot, the
/// revocation committed, the operator got their 204 — and the key the operator
/// had just been told was revoked went on to reserve and dispatch one more
/// inference. Admission now re-checks liveness inside a conditional UPDATE, so
/// it cannot pass the revocation without waiting for it.
///
/// Two things are asserted, and both matter:
///
/// 1. Admission **blocks** while the revocation holds the row. Under the old
///    SELECT it sailed past and returned `Allowed` in milliseconds.
/// 2. Once the revocation commits, admission re-evaluates `NOT disabled` against
///    the newly committed row version and refuses.
#[tokio::test]
async fn a_revocation_in_flight_blocks_the_admission_racing_it() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "revoke-race").await;
    let key = create_key(&pool, user_id, Decimal::from(100), 1_000_000).await;

    // The operator's disable, held open mid-flight: the api_keys row lock is
    // taken and the revocation is not yet visible to any other snapshot.
    let mut revocation = pool.begin().await.expect("revocation must begin");
    query("UPDATE api_keys SET disabled = TRUE WHERE id = $1 AND NOT disabled")
        .bind(key.id)
        .execute(&mut *revocation)
        .await
        .expect("revocation must update");

    let racing = pool.clone();
    let racing_key = key.clone();
    let mut admission =
        tokio::spawn(async move { admit(&racing, &racing_key, 1_000, Decimal::ONE).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut admission)
            .await
            .is_err(),
        "admission must wait for the in-flight revocation instead of reading around it"
    );

    revocation.commit().await.expect("revocation must commit");
    assert!(
        matches!(
            admission.await.expect("admission task must join"),
            UsageAdmission::Unauthorized
        ),
        "a key whose revocation committed first must not be admitted"
    );

    let reservations =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .expect("reservation count must query");
    assert_eq!(
        reservations, 0,
        "a refused admission must leave no reservation behind to dispatch against"
    );
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

fn patch_json(uri: &str, cookie: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .header(CSRF_HEADER, "1")
        .body(Body::from(body.to_string()))
        .expect("PATCH request should build")
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

/// The per-key priority default's full portal lifecycle (rollout stage 3a):
/// set at mint, visible in the listing, mutated by the portal's first PATCH
/// endpoint, cleared by an explicit null — with `{}` a no-op, an unknown
/// field a loud 400, and another tenant's key a 404.
#[tokio::test]
async fn default_priority_rides_mint_list_and_the_key_patch() {
    let Some(pool) = connect().await else {
        return;
    };
    let app = portal_app(&pool);
    let user_id = create_user(&pool, "knob-portal").await;
    let cookie = portal_cookie(&pool, user_id).await;

    let (status, body) = send(
        &app,
        post_json(
            "/api/keys",
            &cookie,
            json!({ "name": "knob key", "default_priority": "cost" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["default_priority"], "cost");
    let key_id = body["id"]
        .as_str()
        .expect("created key has an id")
        .to_owned();

    let (status, body) = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/api/keys")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .expect("GET request should build"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["keys"][0]["default_priority"], "cost");

    // PATCH set.
    let (status, body) = send(
        &app,
        patch_json(
            &format!("/api/keys/{key_id}"),
            &cookie,
            json!({ "default_priority": "success" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["default_priority"], "success");

    // `{}` is a no-op PATCH that answers with the current summary.
    let (status, body) = send(
        &app,
        patch_json(&format!("/api/keys/{key_id}"), &cookie, json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_priority"], "success");

    // Explicit null clears back to balanced (NULL).
    let (status, body) = send(
        &app,
        patch_json(
            &format!("/api/keys/{key_id}"),
            &cookie,
            json!({ "default_priority": null }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["default_priority"].is_null());

    // Strict namespace: a typo'd field and an unknown value are refused, and
    // neither mutates the key.
    for garbage in [
        json!({ "default_priorty": "cost" }),
        json!({ "default_priority": "fast" }),
    ] {
        // Status only: axum's Json extractor rejects these with a PLAIN TEXT
        // body, which the JSON-insisting `send` helper cannot read.
        let status = app
            .clone()
            .oneshot(patch_json(
                &format!("/api/keys/{key_id}"),
                &cookie,
                garbage.clone(),
            ))
            .await
            .expect("request should complete")
            .status();
        assert!(
            status.is_client_error(),
            "{garbage} must be refused, got {status}"
        );
    }
    let stored = query_scalar::<_, Option<String>>(
        "SELECT default_priority FROM api_keys WHERE id = $1::uuid",
    )
    .bind(&key_id)
    .fetch_one(&pool)
    .await
    .expect("stored default must query");
    assert_eq!(stored, None, "refused PATCHes must not mutate");

    // Tenancy: another user's key answers 404 through the same endpoint.
    let stranger = create_user(&pool, "knob-portal-stranger").await;
    let stranger_cookie = portal_cookie(&pool, stranger).await;
    let (status, body) = send(
        &app,
        patch_json(
            &format!("/api/keys/{key_id}"),
            &stranger_cookie,
            json!({ "default_priority": "cost" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// Velocity counts uncached work. An agent loop re-sends its whole history
/// every turn and prompt caching makes ~97% of that input cache reads — a
/// settled cached-heavy row must charge the window only its fresh tokens,
/// while a fully-uncached row of the same size still counts whole. Pinned
/// here because the dogfooded ZeroClaw loop (17.6k input/turn, ~17.2k
/// cached) tripped a 100k/min key on its second task under raw accounting.
#[tokio::test]
async fn velocity_counts_uncached_tokens_only() {
    let Some(pool) = connect().await else {
        return;
    };
    let cached_heavy = |cost: Decimal| UsageRecord {
        usage: OpenAiUsage {
            prompt_tokens: 2_000,
            completion_tokens: 25,
            total_tokens: 2_025,
            prompt_tokens_details: Some(PromptTokenDetails {
                cached_tokens: 1_950,
            }),
        },
        ..usage_record(cost)
    };

    // Cache-friendly half: 2k of input settles, but only 75 uncached tokens
    // (50 fresh input + 25 output) charge the window — the next reservation
    // fits comfortably under the 1000/min cap.
    let user_id = create_user(&pool, "velocity-cached").await;
    let key = create_key(&pool, user_id, Decimal::from(1_000), 1_000).await;
    let UsageAdmission::Allowed(session) = admit(&pool, &key, 100, Decimal::ZERO).await else {
        panic!("first reservation should be admitted");
    };
    session
        .record(&cached_heavy(Decimal::ZERO))
        .await
        .expect("cached-heavy settlement must succeed");
    assert!(
        matches!(
            admit(&pool, &key, 800, Decimal::ZERO).await,
            UsageAdmission::Allowed(_)
        ),
        "cache reads must not consume the velocity window"
    );

    // Uncacheable half: the same 2k settles fully fresh and the window is
    // spent — identical follow-up is refused.
    let user_id = create_user(&pool, "velocity-uncached").await;
    let key = create_key(&pool, user_id, Decimal::from(1_000), 1_000).await;
    let UsageAdmission::Allowed(session) = admit(&pool, &key, 100, Decimal::ZERO).await else {
        panic!("first reservation should be admitted");
    };
    let mut fresh = usage_record(Decimal::ZERO);
    fresh.usage = OpenAiUsage {
        prompt_tokens: 2_000,
        completion_tokens: 25,
        total_tokens: 2_025,
        prompt_tokens_details: None,
    };
    session
        .record(&fresh)
        .await
        .expect("fresh settlement must succeed");
    assert!(
        matches!(
            admit(&pool, &key, 800, Decimal::ZERO).await,
            UsageAdmission::VelocityExceeded
        ),
        "a fully-uncached row still meets the whole cap"
    );
}

// ---------------------------------------------------------------------------
// The monthly-spend rollup (migration 0019)
// ---------------------------------------------------------------------------
//
// Admission stopped summing `usage_events` for the month-to-date spend ceiling
// and now reads `usage_key_month_spend`, a per-(key, UTC month) running total
// accrued by trigger. That is an access-path change and nothing else, so what
// these pin is that the ceiling still binds on exactly the same values: the
// derived total must equal the ledger it came from, the cap must still refuse
// at the same boundary, and an event must land in the same month the old
// `ts >= date_trunc('month', NOW())` predicate would have put it in.

/// Insert a settled usage row directly, dated `ts`, bypassing admission.
///
/// Backdating is the point: the accrual trigger buckets on the row's own `ts`,
/// so this is how a month boundary gets exercised without waiting for one.
async fn seed_event_at(pool: &PgPool, key: &AuthenticatedKey, ts_sql: &str, cost_usd: Decimal) {
    query(&format!(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, ts, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status
        )
        VALUES ($1, $2, {ts_sql}, 'zero/test', 'test', 'test/model', 0, 0, 0, $3, 1, 200)
        "#
    ))
    .bind(Uuid::new_v4())
    .bind(key.id)
    .bind(cost_usd)
    .execute(pool)
    .await
    .expect("seeded usage event must insert");
}

/// Every rollup bucket in the database equals a fresh sum of the ledger rows it
/// derives from, and no bucket is present on one side only.
async fn rollup_disagreements(pool: &PgPool) -> i64 {
    query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM (
            SELECT api_key_id, usage_event_utc_month(ts) AS month, SUM(cost_usd) AS spend
            FROM usage_events
            GROUP BY 1, 2
        ) AS truth
        FULL OUTER JOIN usage_key_month_spend AS rollup
          ON rollup.api_key_id = truth.api_key_id AND rollup.month = truth.month
        WHERE truth.api_key_id IS NULL
           OR rollup.api_key_id IS NULL
           OR truth.spend <> rollup.spend_usd
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("rollup consistency must query")
}

#[tokio::test]
async fn the_month_rollup_never_disagrees_with_the_ledger() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rollup-truth").await;
    let first = create_key(&pool, user_id, Decimal::from(1_000), 1_000_000).await;
    let second = create_key(&pool, user_id, Decimal::from(1_000), 1_000_000).await;

    // Real settlements through the real path, across two keys.
    for _ in 0..5 {
        spend(&pool, &first, Decimal::new(133, 2)).await;
        spend(&pool, &second, Decimal::new(7, 3)).await;
    }
    // Plus rows in three different months, so the bucketing is exercised and
    // not just a single-bucket sum.
    seed_event_at(&pool, &first, "NOW()", Decimal::new(25, 2)).await;
    seed_event_at(
        &pool,
        &first,
        "NOW() - INTERVAL '2 months'",
        Decimal::from(9),
    )
    .await;
    seed_event_at(
        &pool,
        &second,
        "NOW() + INTERVAL '2 months'",
        Decimal::from(4),
    )
    .await;

    assert_eq!(
        rollup_disagreements(&pool).await,
        0,
        "every rollup bucket must equal the ledger rows it derives from"
    );
}

#[tokio::test]
async fn the_spend_ceiling_binds_on_the_same_boundary_it_always_did() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "spend-boundary").await;
    let key = create_key(&pool, user_id, Decimal::from(20), 1_000_000).await;

    // Spend to one cent short of the ceiling.
    spend(&pool, &key, Decimal::new(1999, 2)).await;

    // A request that lands exactly ON the ceiling is admitted: the cap refuses
    // strictly-greater, never equal. Rolled back rather than settled so the
    // next assertion starts from the same $19.99.
    let UsageAdmission::Allowed(session) = admit(&pool, &key, 10, Decimal::new(1, 2)).await else {
        panic!("a projection landing exactly on the ceiling must be admitted");
    };
    drop(session);
    // The reservation the admitted session took is still encumbering, so clear
    // it before probing the boundary again.
    query("DELETE FROM usage_reservations WHERE api_key_id = $1")
        .bind(key.id)
        .execute(&pool)
        .await
        .expect("reservation cleanup must run");

    // One cent past it is refused.
    assert!(
        matches!(
            admit(&pool, &key, 10, Decimal::new(2, 2)).await,
            UsageAdmission::SpendExceeded
        ),
        "a projection one cent past the ceiling must be refused"
    );
}

#[tokio::test]
async fn a_prior_month_event_does_not_count_against_this_month() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "prior-month").await;
    let key = create_key(&pool, user_id, Decimal::from(20), 1_000_000).await;

    // Far more than the ceiling, but all of it last month.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - INTERVAL '1 second'",
        Decimal::from(500),
    )
    .await;

    assert!(
        matches!(
            admit(&pool, &key, 10, Decimal::ONE).await,
            UsageAdmission::Allowed(_)
        ),
        "last month's spend must not bind this month's ceiling"
    );

    // The same amount one second later is inside this month, and does bind.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'",
        Decimal::from(500),
    )
    .await;
    assert!(
        matches!(
            admit(&pool, &key, 10, Decimal::ONE).await,
            UsageAdmission::SpendExceeded
        ),
        "an event on the first instant of the month is inside it"
    );
}

#[tokio::test]
async fn an_event_dated_past_this_month_still_counts_against_it() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "future-month").await;
    let key = create_key(&pool, user_id, Decimal::from(20), 1_000_000).await;

    // The predicate this replaced was `ts >= <start of this month>`, which has
    // no upper bound: a row landing in a later month — two routers with clocks
    // skewed across a month boundary — counted against the ceiling. Reading
    // only the CURRENT bucket would have quietly stopped counting it and
    // loosened the cap, so the rollup read is `month >= <this month>`.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' + INTERVAL '1 month'",
        Decimal::from(500),
    )
    .await;

    assert!(
        matches!(
            admit(&pool, &key, 10, Decimal::ONE).await,
            UsageAdmission::SpendExceeded
        ),
        "a future-dated event must keep counting against the ceiling, as it did before"
    );
}

/// Direct writes to the rollup are refused; the accrual trigger's are not.
///
/// Without this guard `UPDATE usage_key_month_spend SET spend_usd = ...`
/// succeeded silently, and since nothing ever recomputes the total from the
/// ledger the divergence was permanent and invisible — admission would go on
/// enforcing the wrong ceiling for that key forever. That is strictly worse
/// than the slow scan this table replaced: slow is visible, a quietly wrong
/// spend cap is not.
#[tokio::test]
async fn the_rollup_refuses_every_hand_written_change() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rollup-guard").await;
    let key = create_key(&pool, user_id, Decimal::from(1_000), 1_000_000).await;

    // Accrual through the trigger (depth 1) works and creates the bucket.
    seed_event_at(&pool, &key, "NOW()", Decimal::new(250, 2)).await;
    let bucket = query_scalar::<_, Decimal>(
        "SELECT spend_usd FROM usage_key_month_spend WHERE api_key_id = $1",
    )
    .bind(key.id)
    .fetch_one(&pool)
    .await
    .expect("the accrual trigger must have created the bucket");
    assert_eq!(bucket, Decimal::new(250, 2));

    // Every direct write (depth 0) is refused.
    for statement in [
        "UPDATE usage_key_month_spend SET spend_usd = 999 WHERE api_key_id = $1",
        "DELETE FROM usage_key_month_spend WHERE api_key_id = $1",
    ] {
        let refused = query(statement).bind(key.id).execute(&pool).await;
        assert!(
            refused.is_err(),
            "a direct write must be refused: {statement}"
        );
    }
    let hand_inserted = query(
        r#"
        INSERT INTO usage_key_month_spend (api_key_id, month, spend_usd)
        VALUES ($1, usage_event_utc_month(NOW() - INTERVAL '1 year'), 500)
        "#,
    )
    .bind(key.id)
    .execute(&pool)
    .await;
    assert!(
        hand_inserted.is_err(),
        "a hand-inserted bucket must be refused"
    );
    let truncated = query("TRUNCATE usage_key_month_spend").execute(&pool).await;
    assert!(truncated.is_err(), "TRUNCATE must be refused");

    // The ledger and the rollup still agree, and accrual still works after the
    // refusals — the guard rejects the writer, not the table.
    seed_event_at(&pool, &key, "NOW()", Decimal::ONE).await;
    let after = query_scalar::<_, Decimal>(
        "SELECT spend_usd FROM usage_key_month_spend WHERE api_key_id = $1",
    )
    .bind(key.id)
    .fetch_one(&pool)
    .await
    .expect("bucket must still be readable");
    assert_eq!(
        after,
        Decimal::new(350, 2),
        "accrual must still work after a refused hand-write"
    );
    assert_eq!(rollup_disagreements(&pool).await, 0);
}

/// The accrual trigger's row lock is bounded by the statement's `lock_timeout`.
///
/// This is the property the free-lane write depends on and it is not obvious:
/// the lock is taken inside a plpgsql trigger, one nesting level down from the
/// statement that set the timeout, and a trigger that opened its own
/// subtransaction could plausibly have escaped it. If it did, the spawned
/// free-lane task would wait forever on a wedged same-key settle while holding
/// a pool connection, and enough of those drain the pool into a stall that
/// reaches the metered lane too.
///
/// One second rather than the five the real path sets, so the test is fast; the
/// mechanism under test is identical.
#[tokio::test]
async fn the_accrual_row_lock_is_bounded_by_lock_timeout() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "accrual-lock").await;
    let key = create_key(&pool, user_id, Decimal::from(1_000), 1_000_000).await;
    seed_event_at(&pool, &key, "NOW()", Decimal::ONE).await;

    // Hold the key's rollup bucket, standing in for a wedged settle. A plain
    // SELECT ... FOR UPDATE takes the row lock without tripping the guard,
    // which only refuses INSERT/UPDATE/DELETE.
    let mut holder = pool.begin().await.expect("holder transaction must begin");
    query_scalar::<_, Decimal>(
        "SELECT spend_usd FROM usage_key_month_spend WHERE api_key_id = $1 FOR UPDATE",
    )
    .bind(key.id)
    .fetch_one(&mut *holder)
    .await
    .expect("holder must lock the bucket");

    // A second writer inserting a usage event for the same key must give up
    // rather than block forever.
    let started = std::time::Instant::now();
    let blocked = async {
        let mut writer = pool.begin().await?;
        query("SET LOCAL lock_timeout = '1s'")
            .execute(&mut *writer)
            .await?;
        query(
            r#"
            INSERT INTO usage_events (
                request_id, api_key_id, tier, upstream_provider, upstream_model,
                input_tokens, cached_input_tokens, output_tokens, cost_usd,
                latency_ms, status
            )
            VALUES ($1, $2, 'zero/test', 'test', 'test/model', 0, 0, 0, 0, 1, 200)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(key.id)
        .execute(&mut *writer)
        .await?;
        writer.commit().await
    }
    .await;
    let waited = started.elapsed();

    let error = blocked.expect_err("the blocked insert must fail rather than wait forever");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|db| db.code())
            .as_deref(),
        Some("55P03"),
        "the failure must be a lock timeout (55P03), not something else: {error}"
    );
    assert!(
        waited < Duration::from_secs(5),
        "the wait must be bounded by lock_timeout, took {waited:?}"
    );

    holder.rollback().await.expect("holder must release");
}

// ---------------------------------------------------------------------------
// Per-key expiry and credit limits (migration 0023)
// ---------------------------------------------------------------------------
//
// Two independent guards on the same hot path, and the tests below are written
// to fail in BOTH directions for each: an expired key admitted is caught, and a
// live key refused is caught. A guard that only ever refuses is not a guard,
// it is an outage.

/// A key with the 0023 fields set. `spend_cap_usd` is deliberately large in
/// these tests so the OPERATOR ceiling never binds first and the assertions are
/// about the customer's own limit.
async fn create_limited_key(
    pool: &PgPool,
    user_id: Uuid,
    expires_at_sql: &str,
    credit_limit_usd: Option<Decimal>,
    credit_limit_window: Option<&str>,
) -> AuthenticatedKey {
    let key_id = Uuid::new_v4();
    query(&format!(
        r#"
        INSERT INTO api_keys (
            id, user_id, key_hash, name,
            spend_cap_usd, velocity_cap_tokens_per_min,
            expires_at, credit_limit_usd, credit_limit_window
        )
        VALUES ($1, $2, $3, 'limits-integration', 100000, 100000000, {expires_at_sql}, $4, $5)
        "#
    ))
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .bind(credit_limit_usd)
    .bind(credit_limit_window)
    .execute(pool)
    .await
    .expect("limited test API key must insert");
    AuthenticatedKey {
        id: key_id,
        user_id,
        default_priority: None,
    }
}

/// How close `NOW()` is to the next UTC day boundary, in seconds.
///
/// The window tests seed an event one second before a boundary and then assert
/// what admission counts on the other side of it. If the suite happens to run
/// across midnight UTC, "one second before today" becomes "one second before
/// tomorrow" partway through and the assertion is about a different window than
/// the seed was. That is a ~1-in-86,400 flake, which on a busy CI is a real one.
/// Rather than paper over it with a retry, the affected tests bail out loudly
/// when they are too close to a boundary to be deterministic.
async fn seconds_to_utc_day_boundary(pool: &PgPool) -> f64 {
    query_scalar::<_, f64>(
        r#"
        SELECT EXTRACT(EPOCH FROM (
            (date_trunc('day', NOW() AT TIME ZONE 'UTC') + INTERVAL '1 day') AT TIME ZONE 'UTC'
            - NOW()
        ))::DOUBLE PRECISION
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("boundary distance must query")
}

/// Expiry is enforced, and it is enforced in BOTH directions: the lapsed key is
/// refused and the two keys that must still work are not.
///
/// The third arm is the one that catches an over-eager guard. A predicate
/// written `expires_at < NOW()` without the NULL arm refuses every key that
/// predates 0023 — every key in production — and a test that only checked the
/// expired case would pass while the router refused all traffic.
#[tokio::test]
async fn an_expired_key_is_refused_and_unexpired_keys_are_not() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "expiry-both-ways").await;

    let expired =
        create_limited_key(&pool, user_id, "NOW() - INTERVAL '1 second'", None, None).await;
    let future = create_limited_key(&pool, user_id, "NOW() + INTERVAL '1 hour'", None, None).await;
    let never = create_limited_key(&pool, user_id, "NULL", None, None).await;

    assert!(
        matches!(
            admit(&pool, &expired, 100, Decimal::new(1, 2)).await,
            UsageAdmission::Unauthorized
        ),
        "a key past its expires_at must be refused"
    );
    assert!(
        matches!(
            admit(&pool, &future, 100, Decimal::new(1, 2)).await,
            UsageAdmission::Allowed(_)
        ),
        "a key whose expiry is still ahead of it must be admitted"
    );
    assert!(
        matches!(
            admit(&pool, &never, 100, Decimal::new(1, 2)).await,
            UsageAdmission::Allowed(_)
        ),
        "a key with NULL expires_at never expires and must be admitted — this is \
         every key that predates migration 0023"
    );
}

/// An expired key and a revoked key are refused with the SAME answer, so the
/// refusal cannot be used to tell them apart.
///
/// Admission must not become an oracle. If expiry produced a distinguishable
/// outcome, a caller holding a key could learn whether it had been deliberately
/// revoked (someone acted) or had merely lapsed (nobody did) — and, at the
/// authenticator, a probe could separate "this hash exists but expired" from
/// "this hash is unknown".
#[tokio::test]
async fn an_expired_key_is_refused_exactly_as_a_revoked_one_is() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "expiry-indistinguishable").await;

    let expired =
        create_limited_key(&pool, user_id, "NOW() - INTERVAL '1 second'", None, None).await;
    let revoked = create_limited_key(&pool, user_id, "NULL", None, None).await;
    query("UPDATE api_keys SET disabled = TRUE WHERE id = $1")
        .bind(revoked.id)
        .execute(&pool)
        .await
        .expect("revocation must apply");

    let expired_outcome = admit(&pool, &expired, 100, Decimal::new(1, 2)).await;
    let revoked_outcome = admit(&pool, &revoked, 100, Decimal::new(1, 2)).await;
    assert!(
        matches!(expired_outcome, UsageAdmission::Unauthorized)
            && matches!(revoked_outcome, UsageAdmission::Unauthorized),
        "expiry and revocation must both surface as Unauthorized, so the refusal \
         does not say which"
    );

    // The same property one layer up: the authenticator refuses an expired key
    // with the same `Invalid` it gives an unknown one, and never caches it.
    let authenticator = zerorouter::auth::KeyAuthenticator::new();
    let plaintext = generate_api_key();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, expires_at)
        VALUES ($1, $2, $3, 'lapsed', NOW() - INTERVAL '1 second')
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(&pool)
    .await
    .expect("lapsed key must insert");
    assert!(
        authenticator.authenticate(&pool, &plaintext).await.is_err(),
        "the authenticator must refuse a lapsed key rather than cache it"
    );
}

/// A lifetime limit (no reset cadence) binds on everything the key has ever
/// spent, including spend from a previous calendar month.
///
/// The previous-month arm is what separates "lifetime" from "monthly". If the
/// lifetime case were wired to the 0019 month rollup by mistake, every limit
/// would silently reset on the 1st and this is the only test that would notice.
#[tokio::test]
async fn a_lifetime_credit_limit_never_resets() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "limit-lifetime").await;
    let key = create_limited_key(&pool, user_id, "NULL", Some(Decimal::ONE), None).await;

    // Spent in a previous calendar month, and it must still count.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - INTERVAL '1 second'",
        Decimal::new(90, 2),
    )
    .await;

    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(5, 2)).await,
            UsageAdmission::Allowed(_)
        ),
        "$0.90 spent against a $1 lifetime limit still leaves room for $0.05"
    );
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(20, 2)).await,
            UsageAdmission::KeyCreditLimitExceeded
        ),
        "$0.90 spent plus a $0.20 worst-case reserve exceeds a $1 lifetime limit, \
         and last month's spend counts because a lifetime limit never resets"
    );
}

/// The window-reset boundary, for the daily cadence: spend recorded one second
/// before the reset must not be counted after it.
///
/// This is the test that fails if the bucket read uses `>=` against the wrong
/// instant, if the DATE grain is replaced by something with a sub-day
/// component, or if the window start is computed from a rolling `NOW() -
/// INTERVAL '1 day'` instead of the calendar day.
#[tokio::test]
async fn a_daily_credit_limit_does_not_count_spend_from_before_the_reset() {
    let Some(pool) = connect().await else {
        return;
    };
    let remaining = seconds_to_utc_day_boundary(&pool).await;
    assert!(
        remaining > 60.0,
        "this test seeds one second either side of a UTC day boundary and cannot \
         be deterministic when the suite is running across one ({remaining:.0}s \
         to the next boundary); re-run shortly"
    );
    let user_id = create_user(&pool, "limit-daily").await;
    let key = create_limited_key(&pool, user_id, "NULL", Some(Decimal::ONE), Some("daily")).await;

    // The last second of YESTERDAY, UTC. Far more than the limit.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('day', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - INTERVAL '1 second'",
        Decimal::from(50),
    )
    .await;
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(50, 2)).await,
            UsageAdmission::Allowed(_)
        ),
        "$50 spent one second before today's reset must not count against today's \
         $1 window"
    );

    // The same money, today, does bind.
    seed_event_at(&pool, &key, "NOW()", Decimal::new(95, 2)).await;
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(50, 2)).await,
            UsageAdmission::KeyCreditLimitExceeded
        ),
        "$0.95 spent today plus a $0.50 reserve exceeds the $1 daily window"
    );
}

/// The same boundary for the weekly cadence, whose window starts on Monday
/// 00:00 UTC (`date_trunc('week', ...)`).
#[tokio::test]
async fn a_weekly_credit_limit_counts_from_monday_utc() {
    let Some(pool) = connect().await else {
        return;
    };
    let remaining = seconds_to_utc_day_boundary(&pool).await;
    assert!(
        remaining > 60.0,
        "this test straddles a UTC day boundary and cannot be deterministic when \
         the suite is running across one ({remaining:.0}s remaining); re-run shortly"
    );
    let user_id = create_user(&pool, "limit-weekly").await;
    let key = create_limited_key(&pool, user_id, "NULL", Some(Decimal::ONE), Some("weekly")).await;

    // The last second before this week began.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('week', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - INTERVAL '1 second'",
        Decimal::from(50),
    )
    .await;
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(50, 2)).await,
            UsageAdmission::Allowed(_)
        ),
        "last week's spend must not count against this week's window"
    );

    // The first second of this week does count — the inclusive end of the range.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('week', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'",
        Decimal::new(95, 2),
    )
    .await;
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(50, 2)).await,
            UsageAdmission::KeyCreditLimitExceeded
        ),
        "spend at the very start of the week is INSIDE the window: an off-by-one \
         on the lower bound would let a whole Monday escape the limit"
    );
}

/// The monthly cadence reads the 0019 rollup — the same value the operator
/// ceiling already reads — and resets on the 1st.
#[tokio::test]
async fn a_monthly_credit_limit_resets_with_the_calendar_month() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "limit-monthly").await;
    let key = create_limited_key(&pool, user_id, "NULL", Some(Decimal::ONE), Some("monthly")).await;

    seed_event_at(
        &pool,
        &key,
        "date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - INTERVAL '1 second'",
        Decimal::from(50),
    )
    .await;
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(50, 2)).await,
            UsageAdmission::Allowed(_)
        ),
        "last month's spend must not count against this month's window"
    );

    seed_event_at(&pool, &key, "NOW()", Decimal::new(95, 2)).await;
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::new(50, 2)).await,
            UsageAdmission::KeyCreditLimitExceeded
        ),
        "$0.95 spent this month plus a $0.50 reserve exceeds the $1 monthly window"
    );
}

/// A key with no credit limit is completely unaffected — the compatibility
/// claim the whole migration rests on.
///
/// It spends far past what any of the limits above would have allowed, and must
/// be admitted every time. The only thing that may still refuse it is the
/// OPERATOR ceiling, which is set high here so it does not.
#[tokio::test]
async fn a_key_with_no_credit_limit_is_unchanged_by_the_feature() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "limit-absent").await;
    let key = create_limited_key(&pool, user_id, "NULL", None, None).await;

    for _ in 0..5 {
        spend(&pool, &key, Decimal::from(100)).await;
    }
    assert!(
        matches!(
            admit(&pool, &key, 100, Decimal::from(100)).await,
            UsageAdmission::Allowed(_)
        ),
        "a key with credit_limit_usd IS NULL is unlimited and must never be \
         refused by the 0023 gate, however much it has spent"
    );
}

/// The operator ceiling and the customer's limit are separate, and each refusal
/// says which one bound.
///
/// A caller acts on the code: `spend_cap_exceeded` means talk to the operator,
/// `key_credit_limit_exceeded` means it is your own budget. Collapsing them
/// would send every customer down the wrong path half the time.
#[tokio::test]
async fn the_two_spend_ceilings_are_reported_separately() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "limit-vs-cap").await;

    // Customer limit binds, operator ceiling does not.
    let customer = create_limited_key(&pool, user_id, "NULL", Some(Decimal::ONE), None).await;
    seed_event_at(&pool, &customer, "NOW()", Decimal::new(99, 2)).await;
    assert!(
        matches!(
            admit(&pool, &customer, 100, Decimal::from(5)).await,
            UsageAdmission::KeyCreditLimitExceeded
        ),
        "the customer's own limit must report as itself, not as the operator cap"
    );

    // Operator ceiling binds while the customer limit has room. Both are set,
    // and the operator's is the tighter one, so it is the one reported — the
    // answer the customer cannot fix by editing their own budget.
    let operator = {
        let key_id = Uuid::new_v4();
        query(
            r#"
            INSERT INTO api_keys (
                id, user_id, key_hash, name,
                spend_cap_usd, velocity_cap_tokens_per_min, credit_limit_usd
            )
            VALUES ($1, $2, $3, 'operator-tighter', 1, 100000000, 100000)
            "#,
        )
        .bind(key_id)
        .bind(user_id)
        .bind(hash_api_key(&generate_api_key()))
        .execute(&pool)
        .await
        .expect("operator-capped key must insert");
        AuthenticatedKey {
            id: key_id,
            user_id,
            default_priority: None,
        }
    };
    assert!(
        matches!(
            admit(&pool, &operator, 100, Decimal::from(5)).await,
            UsageAdmission::SpendExceeded
        ),
        "when the operator ceiling is the tighter of the two it must be the one \
         reported: raising the customer's own limit would not help"
    );
}

/// Both 0023 counters equal a fresh sum of the ledger they derive from — the
/// attribution guarantee, checked the same way the 0019 rollup's is.
///
/// A counter that drifts from `usage_events` is worse than a slow one: the
/// divergence is permanent (nothing recomputes it) and invisible (admission
/// goes on enforcing the wrong number), so the limit a customer sees and the
/// limit that binds quietly part ways.
#[tokio::test]
async fn the_derived_spend_counters_never_drift_from_the_ledger() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "counter-truth").await;
    let key = create_limited_key(&pool, user_id, "NULL", None, None).await;

    // Settled through the real path, plus backdated rows in other buckets.
    spend(&pool, &key, Decimal::new(125, 2)).await;
    spend(&pool, &key, Decimal::new(37, 2)).await;
    seed_event_at(
        &pool,
        &key,
        "NOW() - INTERVAL '3 days'",
        Decimal::new(41, 2),
    )
    .await;
    seed_event_at(
        &pool,
        &key,
        "NOW() - INTERVAL '40 days'",
        Decimal::new(7, 2),
    )
    .await;

    let day_disagreements = query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM (
            SELECT api_key_id, usage_event_utc_day(ts) AS day, SUM(cost_usd) AS spend
            FROM usage_events
            GROUP BY 1, 2
        ) AS truth
        FULL OUTER JOIN usage_key_day_spend AS rollup
          ON rollup.api_key_id = truth.api_key_id AND rollup.day = truth.day
        WHERE truth.api_key_id IS NULL
           OR rollup.api_key_id IS NULL
           OR truth.spend <> rollup.spend_usd
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("day counter consistency must query");
    assert_eq!(
        day_disagreements, 0,
        "every usage_key_day_spend bucket must equal a fresh sum of the ledger \
         rows it derives from, and no bucket may exist on one side only"
    );

    let total_disagreements = query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM (
            SELECT api_key_id, SUM(cost_usd) AS spend
            FROM usage_events
            GROUP BY 1
        ) AS truth
        FULL OUTER JOIN usage_key_total_spend AS rollup
          ON rollup.api_key_id = truth.api_key_id
        WHERE truth.api_key_id IS NULL
           OR rollup.api_key_id IS NULL
           OR truth.spend <> rollup.spend_usd
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("total counter consistency must query");
    assert_eq!(
        total_disagreements, 0,
        "every usage_key_total_spend row must equal the key's whole ledger"
    );
}

/// Neither derived counter may be written by hand.
///
/// The fence is what makes the "cannot drift" claim above hold over time: with
/// it, the ONLY writer is the accrual trigger. Without it, one stray UPDATE
/// leaves a key enforcing a number no ledger supports, forever and silently.
#[tokio::test]
async fn the_derived_spend_counters_refuse_direct_writes() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "counter-fence").await;
    let key = create_limited_key(&pool, user_id, "NULL", None, None).await;
    spend(&pool, &key, Decimal::new(10, 2)).await;

    let day_write = query("UPDATE usage_key_day_spend SET spend_usd = 0 WHERE api_key_id = $1")
        .bind(key.id)
        .execute(&pool)
        .await;
    assert!(
        day_write.is_err(),
        "a hand-written UPDATE against usage_key_day_spend must be refused by the \
         database, not merely discouraged by convention"
    );
    let total_write = query("UPDATE usage_key_total_spend SET spend_usd = 0 WHERE api_key_id = $1")
        .bind(key.id)
        .execute(&pool)
        .await;
    assert!(
        total_write.is_err(),
        "a hand-written UPDATE against usage_key_total_spend must be refused"
    );
    let total_insert =
        query("INSERT INTO usage_key_total_spend (api_key_id, spend_usd) VALUES ($1, 999)")
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await;
    assert!(
        total_insert.is_err(),
        "a hand-written INSERT must be refused too — the fence is about who \
         writes, not about what they wrote"
    );
}

/// The number the portal SHOWS and the number admission ENFORCES are the same
/// number.
///
/// They are computed by two different SQL statements — admission's is one arm
/// of a hot-path query built for a single key, the portal's is a per-row
/// expression over a list — so nothing but a test binds them together. If they
/// drift, a customer reads "$0.40 of $1.00" on a page whose next request is
/// refused, and every support conversation starts from a lie.
///
/// Checked at the boundary rather than at some comfortable midpoint, because
/// the boundary is where a disagreement of one cent becomes visible.
#[tokio::test]
async fn the_portal_reports_the_window_spend_admission_enforces() {
    let Some(pool) = connect().await else {
        return;
    };
    let remaining = seconds_to_utc_day_boundary(&pool).await;
    assert!(
        remaining > 60.0,
        "this test seeds either side of a UTC day boundary and cannot be \
         deterministic when the suite is running across one ({remaining:.0}s \
         remaining); re-run shortly"
    );
    let user_id = create_user(&pool, "limit-wire").await;
    let key = create_limited_key(&pool, user_id, "NULL", Some(Decimal::ONE), Some("daily")).await;
    // Spend on BOTH sides of the reset, deliberately. Seeding only today's
    // would make this test pass against any window at least as wide as today —
    // including a portal that read a different one — and it would then be
    // pinning nothing. The out-of-window amount is large enough that reporting
    // it would be unmistakable.
    seed_event_at(
        &pool,
        &key,
        "date_trunc('day', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' - INTERVAL '1 second'",
        Decimal::from(50),
    )
    .await;
    seed_event_at(&pool, &key, "NOW()", Decimal::new(60, 2)).await;

    let app = portal_app(&pool);
    let cookie = portal_cookie(&pool, user_id).await;
    let (status, body) = send(
        &app,
        Request::builder()
            .uri("/api/keys")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .expect("GET request should build"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = body["keys"]
        .as_array()
        .expect("keys must be an envelope array")
        .iter()
        .find(|row| row["id"].as_str() == Some(&key.id.to_string()))
        .expect("the limited key must be listed")
        .clone();

    assert_eq!(
        listed["credit_limit_usd"].as_str(),
        Some("1"),
        "the limit is echoed as stored"
    );
    assert_eq!(listed["credit_limit_window"].as_str(), Some("daily"));
    let reported = Decimal::from_str(
        listed["credit_limit_used_usd"]
            .as_str()
            .expect("used must be reported for a limited key"),
    )
    .expect("reported usage must parse as a decimal");
    assert_eq!(
        reported,
        Decimal::new(60, 2),
        "the portal must report THIS window's settled spend and nothing from \
         before the reset"
    );

    // The boundary proof: with $0.60 reported used against a $1 limit, a
    // request reserving exactly the remaining $0.40 is admitted and one cent
    // more is refused. That can only hold if admission is reading the same
    // $0.60 the portal just displayed.
    let remaining = Decimal::ONE - reported;
    assert!(
        matches!(
            admit(&pool, &key, 100, remaining).await,
            UsageAdmission::Allowed(_)
        ),
        "a reserve of exactly the reported remainder must fit"
    );
    assert!(
        matches!(
            admit(&pool, &key, 100, remaining + Decimal::new(1, 2)).await,
            UsageAdmission::KeyCreditLimitExceeded
        ),
        "one cent past the reported remainder must not"
    );
}

/// Minting through the portal records expiry and the limit, and a key minted
/// with none of them is exactly the key this endpoint minted before 0023.
#[tokio::test]
async fn the_portal_mints_keys_with_expiry_and_a_credit_limit() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "limit-mint").await;
    let app = portal_app(&pool);
    let cookie = portal_cookie(&pool, user_id).await;

    let expires_at = "2099-01-01T00:00:00Z";
    let (status, body) = send(
        &app,
        post_json(
            "/api/keys",
            &cookie,
            json!({
                "name": "contractor",
                "expires_at": expires_at,
                "credit_limit_usd": "25",
                "credit_limit_window": "weekly",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mint should succeed: {body}");
    assert_eq!(body["credit_limit_usd"].as_str(), Some("25"));
    assert_eq!(body["credit_limit_window"].as_str(), Some("weekly"));
    assert_eq!(
        body["credit_limit_used_usd"].as_str(),
        Some("0"),
        "a key that has never been used has spent none of its limit"
    );
    assert!(
        body["expires_at"]
            .as_str()
            .is_some_and(|at| at.starts_with("2099")),
        "the expiry is echoed as recorded, not silently dropped: {body}"
    );

    // The unchanged path: name only.
    let (status, body) = send(
        &app,
        post_json("/api/keys", &cookie, json!({ "name": "plain" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        body["expires_at"].is_null()
            && body["credit_limit_usd"].is_null()
            && body["credit_limit_window"].is_null()
            && body["credit_limit_used_usd"].is_null(),
        "a name-only mint must still produce a never-expiring, unlimited key: {body}"
    );

    // A cadence with no limit would mint an UNLIMITED key from a request that
    // plainly asked for a budget, so it is refused rather than dropped.
    let (status, _) = send(
        &app,
        post_json(
            "/api/keys",
            &cookie,
            json!({ "name": "windowed", "credit_limit_window": "daily" }),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a reset cadence with no limit must be refused"
    );

    // An expiry already in the past mints a key that can never authenticate.
    let (status, _) = send(
        &app,
        post_json(
            "/api/keys",
            &cookie,
            json!({ "name": "stillborn", "expires_at": "2000-01-01T00:00:00Z" }),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an expiry in the past must be refused at mint"
    );
}

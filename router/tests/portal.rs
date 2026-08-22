//! Postgres-backed tenancy tests for the portal control-plane API.
//!
//! Skips (returns early) when `DATABASE_URL` is unset, matching
//! `tests/postgres.rs`. The core assertion is docs/ARCHITECTURE.md
//! "Designed-out failure classes" #2: no portal response may ever contain
//! another user's rows.

use std::{path::PathBuf, str::FromStr, time::Duration};

use axum::{
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
    auth::hash_api_key,
    db::migrate,
    portal,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
    web::{StripeSettings, WebConfig, WebCtx},
};

fn test_web_config() -> WebConfig {
    WebConfig {
        public_base_url: "http://127.0.0.1".to_owned(),
        secure_cookies: false,
        oidc: None,
        stripe: None,
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3_600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    }
}

fn portal_app(pool: &PgPool) -> axum::Router {
    portal::router().with_state(WebCtx::new(pool.clone(), test_web_config()))
}

async fn send(pool: &PgPool, request: Request<Body>) -> (StatusCode, String, Value) {
    let response = portal_app(pool)
        .oneshot(request)
        .await
        .expect("portal request should complete");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("portal response body should be readable")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("portal response should be UTF-8");
    let json = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).expect("portal response should be JSON")
    };
    (status, text, json)
}

fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder
        .body(Body::empty())
        .expect("GET request should build")
}

fn post_keys(cookie: &str, csrf: bool, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/keys")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json");
    if csrf {
        builder = builder.header(CSRF_HEADER, "1");
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("POST request should build")
}

fn delete_key(cookie: &str, key_id: Uuid) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/api/keys/{key_id}"))
        .header(header::COOKIE, cookie)
        .header(CSRF_HEADER, "1")
        .body(Body::empty())
        .expect("DELETE request should build")
}

fn decimal_value(value: &Value) -> Decimal {
    match value {
        Value::String(text) => Decimal::from_str(text).expect("decimal string should parse"),
        Value::Number(number) => {
            Decimal::from_str(&number.to_string()).expect("decimal number should parse")
        }
        other => panic!("expected a decimal value, got {other:?}"),
    }
}

/// The pool the BYOK tests below share. The tests that predate them open
/// their own inline, and are left alone.
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

async fn seed_user(pool: &PgPool, tag: &str) -> Uuid {
    let id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(id)
        .bind(format!("portal-{tag}-{id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    id
}

async fn seed_key(pool: &PgPool, user_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    query("INSERT INTO api_keys (id, user_id, key_hash, name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(user_id)
        .bind(hash_api_key(&format!("zcr_seed_{id}")))
        .bind(name)
        .execute(pool)
        .await
        .expect("test API key must insert");
    id
}

async fn seed_usage(pool: &PgPool, api_key_id: Uuid, model: &str, cost: &str) -> Uuid {
    let request_id = Uuid::new_v4();
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status
        )
        VALUES ($1, $2, 'zero/test', 'test', $3, 100, 0, 50, $4, 12, 200)
        "#,
    )
    .bind(request_id)
    .bind(api_key_id)
    .bind(model)
    .bind(Decimal::from_str(cost).expect("test cost must parse"))
    .execute(pool)
    .await
    .expect("test usage event must insert");
    request_id
}

#[tokio::test]
async fn portal_api_is_scoped_to_the_session_user() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");

    // Two tenants, each with a key and usage history.
    let user_a = seed_user(&pool, "a").await;
    let user_b = seed_user(&pool, "b").await;
    let key_a = seed_key(&pool, user_a, "alpha-key").await;
    let key_b = seed_key(&pool, user_b, "beta-key").await;
    let usage_a1 = seed_usage(&pool, key_a, "test/model-a", "0.25").await;
    let usage_a2 = seed_usage(&pool, key_a, "test/model-a", "0.75").await;
    let usage_b1 = seed_usage(&pool, key_b, "test/model-b", "0.40").await;

    let (token_a, _) = create_session(&pool, user_a, Duration::from_secs(3_600))
        .await
        .expect("session for user A must create");
    let (token_b, _) = create_session(&pool, user_b, Duration::from_secs(3_600))
        .await
        .expect("session for user B must create");
    let cookie_a = format!("{SESSION_COOKIE}={token_a}");
    let cookie_b = format!("{SESSION_COOKIE}={token_b}");

    // Unauthenticated requests are rejected outright.
    let (status, _, json) = send(&pool, get("/api/keys", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "session_required");

    // /api/me reflects the session user only.
    let (status, _, me) = send(&pool, get("/api/me", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user_id"], Value::String(user_a.to_string()));
    assert!(
        me["email"]
            .as_str()
            .expect("me response should carry an email")
            .starts_with("portal-a-")
    );
    assert_eq!(decimal_value(&me["credit_balance_usd"]), Decimal::ZERO);
    assert!(me["created_at"].is_string());
    // This deployment has no Stripe configured, so the key is explicitly null
    // rather than absent — the portal branches on it to decide whether to
    // offer checkout at all.
    assert_eq!(me["stripe_publishable_key"], Value::Null);

    // /api/keys lists only the session user's keys, and never hashes.
    let (status, text, keys) = send(&pool, get("/api/keys", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::OK);
    let listed = keys["keys"].as_array().expect("keys should be an array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], Value::String(key_a.to_string()));
    assert_eq!(listed[0]["name"], "alpha-key");
    assert_eq!(listed[0]["disabled"], Value::Bool(false));
    assert!(!text.contains(&key_b.to_string()));
    assert!(!text.contains("key_hash"));
    assert!(!text.contains("zcr_"));

    // /api/usage is joined through api_keys.user_id: A sees only A's events.
    let (status, text, usage) = send(&pool, get("/api/usage", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(usage["days"], 30);
    assert_eq!(usage["totals"]["requests"], 2);
    assert_eq!(usage["totals"]["input_tokens"], 200);
    assert_eq!(usage["totals"]["output_tokens"], 100);
    assert_eq!(decimal_value(&usage["totals"]["cost_usd"]), Decimal::ONE);
    let recent = usage["recent"]
        .as_array()
        .expect("recent should be an array");
    assert_eq!(recent.len(), 2);
    for event in recent {
        let request_id = event["request_id"]
            .as_str()
            .expect("recent event should carry a request id");
        assert!(request_id == usage_a1.to_string() || request_id == usage_a2.to_string());
        assert_eq!(event["key_name"], "alpha-key");
        assert_eq!(event["upstream_model"], "test/model-a");
    }
    assert!(!text.contains(&usage_b1.to_string()));
    let daily = usage["daily"].as_array().expect("daily should be an array");
    assert!(!daily.is_empty());
    let daily_requests: i64 = daily
        .iter()
        .map(|row| row["requests"].as_i64().expect("daily requests"))
        .sum();
    assert_eq!(daily_requests, 2);

    // ...and B sees only B's.
    let (status, text, usage_b) = send(&pool, get("/api/usage?days=7", Some(&cookie_b))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(usage_b["days"], 7);
    assert_eq!(usage_b["totals"]["requests"], 1);
    assert_eq!(
        usage_b["recent"][0]["request_id"],
        Value::String(usage_b1.to_string())
    );
    assert_eq!(usage_b["recent"][0]["key_name"], "beta-key");
    assert!(!text.contains(&usage_a1.to_string()));
    assert!(!text.contains(&usage_a2.to_string()));

    // Query bounds: out-of-range clamps, non-numeric rejects.
    let (status, _, clamped) = send(&pool, get("/api/usage?days=9999", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(clamped["days"], 90);
    let (status, _, _) = send(&pool, get("/api/usage?days=abc", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Ledger is scoped and empty for a fresh user; bad limits reject.
    let (status, _, ledger) = send(&pool, get("/api/billing/ledger", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ledger["entries"]
            .as_array()
            .expect("ledger entries should be an array")
            .len(),
        0
    );
    let (status, _, _) = send(&pool, get("/api/billing/ledger?limit=x", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Mutations without the CSRF header are rejected before anything else.
    let (status, _, json) = send(
        &pool,
        post_keys(&cookie_a, false, serde_json::json!({ "name": "no-csrf" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"]["code"], "csrf_rejected");

    // Minting returns the plaintext exactly once; the row stores the digest.
    let (status, _, minted) = send(
        &pool,
        post_keys(
            &cookie_a,
            true,
            serde_json::json!({
                "name": "minted",
                "spend_cap_usd": "5",
                "velocity_cap_tokens_per_min": 1000
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let plaintext = minted["api_key"]
        .as_str()
        .expect("mint response should carry the plaintext key")
        .to_owned();
    assert!(plaintext.starts_with("zcr_"));
    assert_eq!(plaintext.len(), 68);
    let minted_id = Uuid::from_str(
        minted["id"]
            .as_str()
            .expect("mint response should carry the key id"),
    )
    .expect("minted key id should be a UUID");
    assert_eq!(minted["name"], "minted");
    assert_eq!(decimal_value(&minted["spend_cap_usd"]), Decimal::from(5));
    assert_eq!(minted["velocity_cap_tokens_per_min"], 1000);
    let stored_hash = query_scalar::<_, String>("SELECT key_hash FROM api_keys WHERE id = $1")
        .bind(minted_id)
        .fetch_one(&pool)
        .await
        .expect("minted key hash must query");
    assert_eq!(stored_hash, hash_api_key(&plaintext));
    assert_ne!(stored_hash, plaintext);
    let owner = query_scalar::<_, Uuid>("SELECT user_id FROM api_keys WHERE id = $1")
        .bind(minted_id)
        .fetch_one(&pool)
        .await
        .expect("minted key owner must query");
    assert_eq!(owner, user_a);

    // The listing shows the new key (newest first) but never the plaintext.
    let (status, text, keys) = send(&pool, get("/api/keys", Some(&cookie_a))).await;
    assert_eq!(status, StatusCode::OK);
    let listed = keys["keys"].as_array().expect("keys should be an array");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["id"], Value::String(minted_id.to_string()));
    assert!(!text.contains(&plaintext));

    // Invalid mint requests are rejected.
    for body in [
        serde_json::json!({ "name": "" }),
        serde_json::json!({ "name": "n".repeat(101) }),
        serde_json::json!({ "name": "ok", "spend_cap_usd": "0" }),
        serde_json::json!({ "name": "ok", "spend_cap_usd": "20000" }),
        serde_json::json!({ "name": "ok", "velocity_cap_tokens_per_min": 0 }),
        serde_json::json!({ "name": "ok", "velocity_cap_tokens_per_min": 3000000 }),
    ] {
        let (status, _, _) = send(&pool, post_keys(&cookie_a, true, body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // Deleting another tenant's key is a 404 and leaves the key enabled.
    let (status, _, json) = send(&pool, delete_key(&cookie_a, key_b)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["code"], "key_not_found");
    let b_disabled = query_scalar::<_, bool>("SELECT disabled FROM api_keys WHERE id = $1")
        .bind(key_b)
        .fetch_one(&pool)
        .await
        .expect("foreign key state must query");
    assert!(!b_disabled);

    // Deleting one's own key disables the row (never deletes it).
    let (status, _, _) = send(&pool, delete_key(&cookie_a, minted_id)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let minted_disabled = query_scalar::<_, bool>("SELECT disabled FROM api_keys WHERE id = $1")
        .bind(minted_id)
        .fetch_one(&pool)
        .await
        .expect("minted key state must query");
    assert!(minted_disabled);
    let (status, _, _) = send(&pool, delete_key(&cookie_a, minted_id)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The 21st active key is rejected. A has 1 active key (alpha); add 19.
    // Two limits now stand behind `key_limit_reached` — the active-key cap and
    // the trailing-window creation throttle that counts disabled keys (A has
    // also created and disabled `minted` above). Both are tripped here; which
    // one bites first is exercised separately in `tests/caps.rs`.
    for index in 0..19 {
        seed_key(&pool, user_a, &format!("bulk-{index}")).await;
    }
    let (status, _, json) = send(
        &pool,
        post_keys(
            &cookie_a,
            true,
            serde_json::json!({ "name": "one-too-many" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "key_limit_reached");
    let active =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM api_keys WHERE user_id = $1 AND NOT disabled")
            .bind(user_a)
            .fetch_one(&pool)
            .await
            .expect("active key count must query");
    assert_eq!(active, 20);
}

/// `/api/me` is how the SPA receives the Stripe publishable key.
///
/// Embedded Checkout cannot mount without it, and it must not be baked into the
/// bundle: the test and live Stripe accounts have different keys, so a
/// hardcoded one is wrong for whichever environment it was not built for. This
/// pins the delivery mechanism, and — just as importantly — pins that the
/// SECRET key never rides along with it.
#[tokio::test]
async fn me_carries_the_stripe_publishable_key_but_never_the_secret() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");

    let user_id = seed_user(&pool, "pk").await;
    let (token, _) = create_session(&pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("session must create");
    let cookie = format!("{SESSION_COOKIE}={token}");

    let mut config = test_web_config();
    config.stripe = Some(StripeSettings {
        secret_key: "sk_test_MUST_NOT_LEAK".to_owned(),
        publishable_key: "pk_test_visible".to_owned(),
        webhook_secret: "whsec_MUST_NOT_LEAK".to_owned(),
        checkout_min_usd: Decimal::from(5),
        checkout_max_usd: Decimal::from(1000),
        api_base: "https://api.stripe.invalid".to_owned(),
        // A card-only deployment, which is what this harness describes.
        crypto_rail: false,
    });
    let app = portal::router().with_state(WebCtx::new(pool.clone(), config));

    let response = app
        .oneshot(get("/api/me", Some(&cookie)))
        .await
        .expect("portal request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("body should be UTF-8");
    let me: Value = serde_json::from_str(&text).expect("body should be JSON");

    assert_eq!(me["stripe_publishable_key"], "pk_test_visible");
    // The publishable key is the ONLY Stripe credential this response may
    // carry. Serving either of the others would hand an authenticated customer
    // the ability to create charges or forge webhooks.
    assert!(
        !text.contains("sk_test_MUST_NOT_LEAK"),
        "the Stripe secret key must never reach the browser: {text}"
    );
    assert!(
        !text.contains("whsec_MUST_NOT_LEAK"),
        "the webhook secret must never reach the browser: {text}"
    );
}

#[tokio::test]
async fn the_spa_serves_immutable_assets_and_a_revalidating_shell() {
    // A deploy must never strand a returning browser on the previous bundle:
    // the shell (and every SPA route that falls back to it) says `no-cache`,
    // while Vite's content-hashed /assets/ files are immutable for a year.
    // Found live on 2026-08-16, when the legal-page publish stayed invisible
    // to a browser that had heuristically cached the header-less shell.
    let dist = std::env::temp_dir().join(format!("zr-spa-{}", Uuid::new_v4()));
    std::fs::create_dir_all(dist.join("assets")).expect("create dist");
    std::fs::write(dist.join("index.html"), "<title>shell</title>").expect("write shell");
    std::fs::write(dist.join("assets").join("app-abc123.js"), "js").expect("write asset");

    let app = portal::spa_router(&dist);
    let cache = |path: &str| {
        let app = app.clone();
        let path = path.to_owned();
        async move {
            let response = app
                .oneshot(Request::get(&path).body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .expect("cache-control present")
                .to_str()
                .expect("ascii")
                .to_owned()
        }
    };

    assert_eq!(
        cache("/assets/app-abc123.js").await,
        "public, max-age=31536000, immutable"
    );
    assert_eq!(cache("/").await, "no-cache");
    // An unknown path falls back to the shell and must revalidate too.
    assert_eq!(cache("/terms").await, "no-cache");

    std::fs::remove_dir_all(&dist).ok();
}

// ---------------------------------------------------------------------------
// Bring-your-own-key (migration 0026)
//
// The HTTP surface, tested where the customer actually meets it: the CSRF gate,
// the tenancy scoping, and — the one that matters most — that no response body
// on any of the three endpoints ever carries the credential.
// ---------------------------------------------------------------------------

/// A portal router that HOLDS a BYOK keyring, i.e. a deployment where the
/// operator has provisioned `BYOK_ENCRYPTION_KEY`.
fn byok_portal_app(pool: &PgPool) -> axum::Router {
    let keyring = zerorouter::byok::Keyring::from_hex_for_tests(&"5e".repeat(32))
        .expect("the fixture key must build a keyring");
    portal::router().with_state(
        WebCtx::new(pool.clone(), test_web_config()).with_byok(Some(std::sync::Arc::new(keyring))),
    )
}

async fn send_to(app: axum::Router, request: Request<Body>) -> (StatusCode, String, Value) {
    let response = app
        .oneshot(request)
        .await
        .expect("portal request should complete");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("portal response body should be readable")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("portal response should be UTF-8");
    let json = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).expect("portal response should be JSON")
    };
    (status, text, json)
}

fn post_byok(cookie: &str, csrf: bool, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/byok")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json");
    if csrf {
        builder = builder.header(CSRF_HEADER, "1");
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("POST request should build")
}

#[tokio::test]
async fn the_byok_endpoints_are_tenant_scoped_csrf_guarded_and_never_echo_the_key() {
    let Some(pool) = connect().await else {
        return;
    };
    const CUSTOMER_KEY: &str = "sk-portal-OWNKEY-0123456789abcdef";

    let owner = seed_user(&pool, "byok-owner").await;
    let neighbour = seed_user(&pool, "byok-neighbour").await;
    let (owner_token, _) = create_session(&pool, owner, Duration::from_secs(3_600))
        .await
        .expect("owner session must create");
    let (neighbour_token, _) = create_session(&pool, neighbour, Duration::from_secs(3_600))
        .await
        .expect("neighbour session must create");
    let owner_cookie = format!("{SESSION_COOKIE}={owner_token}");
    let neighbour_cookie = format!("{SESSION_COOKIE}={neighbour_token}");

    // Unauthenticated is refused before anything is looked up.
    let (status, _, json) = send_to(byok_portal_app(&pool), get("/api/byok", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "session_required");

    // A mutation without the portal header is refused — a cross-site form post
    // cannot set it, and attaching a provider key is exactly the kind of thing
    // that must not be reachable that way.
    let (status, _, json) = send_to(
        byok_portal_app(&pool),
        post_byok(
            &owner_cookie,
            false,
            json!({"provider": "anthropic", "api_key": CUSTOMER_KEY}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"]["code"], "csrf_rejected");

    // A provider that cannot take a customer key is refused before sealing.
    let (status, _, json) = send_to(
        byok_portal_app(&pool),
        post_byok(
            &owner_cookie,
            true,
            json!({"provider": "vertex", "api_key": CUSTOMER_KEY}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"]["code"], "byok_provider_unsupported");

    // A paste that caught a newline is answered here rather than as an
    // undiagnosable upstream 401 later.
    let (status, _, json) = send_to(
        byok_portal_app(&pool),
        post_byok(
            &owner_cookie,
            true,
            json!({"provider": "anthropic", "api_key": format!("{CUSTOMER_KEY}\n")}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a trailing newline is trimmed, not refused: {json}"
    );

    // The attach response — the ONE moment the server has ever held the
    // plaintext — must not contain it.
    let (status, attach_text, attach) = send_to(
        byok_portal_app(&pool),
        post_byok(
            &owner_cookie,
            true,
            json!({"provider": "anthropic", "api_key": CUSTOMER_KEY}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        !attach_text.contains(CUSTOMER_KEY) && !attach_text.contains("OWNKEY"),
        "the attach response must not carry the credential: {attach_text}"
    );
    assert_eq!(attach["provider"], "anthropic");
    assert_eq!(attach["last4"], "cdef");
    assert!(
        attach["api_key"].is_null(),
        "there is no api_key field to fill"
    );

    // Neither must the listing, ever again.
    let (status, list_text, list) = send_to(
        byok_portal_app(&pool),
        get("/api/byok", Some(&owner_cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !list_text.contains(CUSTOMER_KEY) && !list_text.contains("OWNKEY"),
        "the listing must not carry the credential: {list_text}"
    );
    assert_eq!(list["keys"].as_array().expect("keys array").len(), 1);
    assert_eq!(
        list["keys"][0]["fingerprint"].as_str().map(str::len),
        Some(16)
    );

    // The neighbour sees nothing and cannot detach it.
    let (status, _, list) = send_to(
        byok_portal_app(&pool),
        get("/api/byok", Some(&neighbour_cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(list["keys"].as_array().expect("keys array").is_empty());
    let (status, _, json) = send_to(
        byok_portal_app(&pool),
        Request::builder()
            .method("DELETE")
            .uri("/api/byok/anthropic")
            .header(header::COOKIE, &neighbour_cookie)
            .header(CSRF_HEADER, "1")
            .body(Body::empty())
            .expect("DELETE should build"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["code"], "byok_key_not_found");

    // The owner can, and afterwards ZeroRouter holds nothing.
    let (status, _, _) = send_to(
        byok_portal_app(&pool),
        Request::builder()
            .method("DELETE")
            .uri("/api/byok/anthropic")
            .header(header::COOKIE, &owner_cookie)
            .header(CSRF_HEADER, "1")
            .body(Body::empty())
            .expect("DELETE should build"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, list) = send_to(
        byok_portal_app(&pool),
        get("/api/byok", Some(&owner_cookie)),
    )
    .await;
    assert!(list["keys"].as_array().expect("keys array").is_empty());
}

#[tokio::test]
async fn byok_ships_dark_when_the_deployment_has_no_encryption_key() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool, "byok-dark").await;
    let (token, _) = create_session(&pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("session must create");
    let cookie = format!("{SESSION_COOKIE}={token}");

    // `portal_app` is a deployment with no keyring — the shipping default.
    // `/api/me` reports the capability as absent, which is what makes the SPA
    // render no BYOK section rather than a form that could only ever fail.
    let (status, _, me) = send(&pool, get("/api/me", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["byok_providers"],
        json!([]),
        "an unconfigured deployment offers no providers"
    );

    // The listing answers "nothing" rather than erroring: a GET is the SPA
    // asking what exists, and nothing is the true answer.
    let (status, _, list) = send(&pool, get("/api/byok", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list["keys"].as_array().expect("keys array").is_empty());

    // An attach is refused CLEANLY — named reason, no partial write.
    let (status, _, json) = send(
        &pool,
        post_byok(
            &cookie,
            true,
            json!({"provider": "anthropic", "api_key": "sk-anything-0123456789abcdef"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(json["error"]["code"], "byok_unavailable");
    let stored =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM byok_provider_keys WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("count must query");
    assert_eq!(stored, 0, "a refused attach must store nothing");
}

// ---------------------------------------------------------------------------
// The playground key
// ---------------------------------------------------------------------------

fn post_playground_key(cookie: Option<&str>, csrf: bool) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri("/api/playground/key");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if csrf {
        builder = builder.header(CSRF_HEADER, "1");
    }
    builder
        .body(Body::empty())
        .expect("playground key request should build")
}

/// The playground's key is an ORDINARY key, and this pins the two halves of
/// that claim a customer can actually see: it carries the caps the key dialog's
/// untouched form would have given it, and it appears in the key list like any
/// other row — which is what makes the Keys page a real revoke switch for the
/// playground rather than a list that quietly omits one credential.
#[tokio::test]
async fn the_playground_key_is_an_ordinary_key_with_default_caps() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool, "playground-defaults").await;
    let (token, _) = create_session(&pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("session must create");
    let cookie = format!("{SESSION_COOKIE}={token}");

    // Mint through the playground, and mint the key the dialog produces when a
    // customer fills in nothing but a name. The two must agree on every cap: a
    // playground that quietly granted itself a larger allowance would be
    // spending the customer's balance under terms they never saw.
    let (status, _, playground) = send(&pool, post_playground_key(Some(&cookie), true)).await;
    assert_eq!(status, StatusCode::CREATED, "playground mint: {playground}");
    let (status, _, dialog) = send(
        &pool,
        post_keys(&cookie, true, json!({ "name": "by hand" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "dialog mint: {dialog}");

    assert_eq!(playground["name"], "playground");
    assert!(
        playground["api_key"]
            .as_str()
            .expect("the mint returns a plaintext key")
            .starts_with("zcr_"),
        "the playground presents the same kind of credential every client does"
    );
    for field in ["spend_cap_usd", "velocity_cap_tokens_per_min"] {
        assert_eq!(
            playground[field], dialog[field],
            "{field} must match the key the dialog mints with its defaults"
        );
    }
    // Null rather than a value: no expiry, no per-key credit limit, and so
    // nothing spent against one.
    for field in [
        "expires_at",
        "credit_limit_usd",
        "credit_limit_window",
        "credit_limit_used_usd",
    ] {
        assert_eq!(playground[field], Value::Null, "{field} must be unset");
    }

    // And it is listed. The revoke path is the ordinary one — nothing about the
    // playground needs its own switch — so the row simply has to be there.
    let (status, _, listed) = send(&pool, get("/api/keys", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let named: Vec<&Value> = listed["keys"]
        .as_array()
        .expect("keys envelope")
        .iter()
        .filter(|key| key["name"] == "playground")
        .collect();
    assert_eq!(named.len(), 1, "one playground row: {listed}");
    assert_eq!(named[0]["id"], playground["id"]);
    assert_eq!(named[0]["disabled"], json!(false));
}

/// The mint is session-gated and CSRF-gated exactly like every other mutating
/// portal route. Worth its own assertion rather than trusting the extractor:
/// this endpoint hands back a live credential, so it is the one place where
/// forgetting either gate would be worst.
#[tokio::test]
async fn the_playground_mint_requires_a_session_and_the_csrf_header() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool, "playground-gates").await;
    let (token, _) = create_session(&pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("session must create");
    let cookie = format!("{SESSION_COOKIE}={token}");

    let (status, _, json) = send(&pool, post_playground_key(None, true)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "session_required");

    let (status, _, json) = send(&pool, post_playground_key(Some(&cookie), false)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"]["code"], "csrf_rejected");

    let minted = query_scalar::<_, i64>("SELECT COUNT(*) FROM api_keys WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("count must query");
    assert_eq!(minted, 0, "neither refusal may mint a key");
}

/// **The playground has no server-side inference path, and must not grow one.**
///
/// The page runs its requests through the public `POST /v1/chat/completions`
/// with a real key, which is what makes every admission, cap and settlement
/// invariant apply to it unchanged. The alternative — a session-authenticated
/// route on this control plane — would have to construct an admission identity
/// from a cookie, and a cookie carries no key row, no caps and nothing to
/// attribute usage to. That is a second admission path, and this is the
/// tripwire for anyone who adds one here later without meaning to.
#[tokio::test]
async fn the_portal_control_plane_serves_no_inference_route() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool, "playground-no-proxy").await;
    let (token, _) = create_session(&pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("session must create");
    let cookie = format!("{SESSION_COOKIE}={token}");

    // Every shape a well-meaning proxy would plausibly take, against a VALID
    // session — so a 404 here is "this router has no such route", not "you were
    // not signed in".
    for path in [
        "/v1/chat/completions",
        "/api/playground/completions",
        "/api/playground/chat",
        "/api/chat/completions",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .header(CSRF_HEADER, "1")
            .body(Body::from(
                json!({ "model": "anthropic/claude-haiku-4-5", "messages": [] }).to_string(),
            ))
            .expect("request should build");
        let (status, _, _) = send(&pool, request).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{path} must not exist on the portal control plane: inference is \
             authenticated by a key, never by a session"
        );
    }
}

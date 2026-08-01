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
use serde_json::Value;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    auth::hash_api_key,
    db::migrate,
    portal,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
    web::{WebConfig, WebCtx},
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

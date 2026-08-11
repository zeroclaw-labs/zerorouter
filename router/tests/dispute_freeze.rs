//! Refunds, chargebacks, and the account freeze (migration 0009).
//!
//! Every webhook here goes through the real `/webhooks/stripe` handler with a
//! correctly signed body, because the signed path is the only path production
//! has — a test that reached the handler another way would prove nothing about
//! what Stripe can actually make this deployment do. The signature is never
//! weakened; the mis-signed test constructs a real HMAC over different bytes.
//!
//! The admission tests drive `POST /v1/chat/completions` end to end with only
//! the upstream leaf faked (`zerorouter::testing`, behind the `testing`
//! feature), so what they pin is the refusal a customer would actually meet.
//!
//! Gated on `DATABASE_URL` like the rest of the DB-backed suites: unset means
//! each test returns early instead of failing.

use std::{path::PathBuf, process::Command, str::FromStr, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    RouterState,
    api::InjectedRoute,
    app,
    auth::{generate_api_key, hash_api_key},
    billing::{
        FreezeReason, autopay_candidates, balance, credit_purchase, freeze_account, grant_promo,
    },
    config::ResolvedRoute,
    db::{KeyMintAdmission, admit_key_mint, migrate},
    provider::TokenUsage,
    providers::{ProviderCandidate, ProviderRoute},
    stripe::{self, STRIPE_SIGNATURE_HEADER},
    testing::{FakeModelProvider, FakeOutcome},
    web::{StripeSettings, WebConfig, WebCtx},
};

const SECRET: &str = "whsec_dispute_freeze_test";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

/// Hex HMAC-SHA256 over `{timestamp}.{payload}`, exactly as Stripe signs.
fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn signature_header(timestamp: i64, signature: &str) -> String {
    format!("t={timestamp},v1={signature}")
}

fn webhook_app(pool: &PgPool) -> axum::Router {
    let config = WebConfig {
        public_base_url: "http://127.0.0.1".to_owned(),
        secure_cookies: false,
        oidc: None,
        stripe: Some(StripeSettings {
            secret_key: "sk_test_unused".to_owned(),
            webhook_secret: SECRET.to_owned(),
            checkout_min_usd: Decimal::from(5),
            checkout_max_usd: Decimal::from(1000),
            api_base: "https://api.stripe.com".to_owned(),
        }),
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3_600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    };
    stripe::router().with_state(WebCtx::new(pool.clone(), config))
}

/// POST a payload at the real handler under a caller-supplied signature
/// header, so the unsigned and mis-signed cases use the same code path as the
/// authentic ones.
async fn post_signed(pool: &PgPool, payload: &str, header: Option<String>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header("content-type", "application/json");
    if let Some(header) = header {
        builder = builder.header(STRIPE_SIGNATURE_HEADER, header);
    }
    let request = builder
        .body(Body::from(payload.to_owned()))
        .expect("webhook request should build");
    let response = webhook_app(pool)
        .oneshot(request)
        .await
        .expect("webhook request should complete");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("webhook response body should be readable")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).expect("webhook response should be JSON");
    (status, json)
}

/// POST a correctly signed payload, signed at the current time against the
/// real clock the handler checks tolerance with.
async fn post_webhook(pool: &PgPool, payload: &str) -> (StatusCode, Value) {
    let timestamp = Utc::now().timestamp();
    let signature = sign(SECRET, timestamp, payload.as_bytes());
    post_signed(pool, payload, Some(signature_header(timestamp, &signature))).await
}

async fn create_user(pool: &PgPool, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("dispute-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

/// A user who has bought `amount_usd` of credit through Checkout, returned as
/// `(user_id, payment_intent_id)`. The purchase goes through the production
/// credit path, so what the reversal tests reverse is a real purchase row.
async fn buyer(pool: &PgPool, label: &str, amount_usd: Decimal) -> (Uuid, String) {
    let user_id = create_user(pool, label).await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    credit_purchase(
        pool,
        user_id,
        amount_usd,
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("funding purchase must apply");
    (user_id, payment_intent)
}

fn dispute_event(dispute_id: &str, payment_intent: &str, amount: i64, currency: &str) -> String {
    json!({
        "id": "evt_test_dispute",
        "type": "charge.dispute.created",
        "data": { "object": {
            "id": dispute_id,
            "object": "dispute",
            "charge": format!("ch_for_{payment_intent}"),
            "payment_intent": payment_intent,
            "amount": amount,
            "currency": currency,
            "reason": "fraudulent",
            "status": "warning_needs_response",
        }}
    })
    .to_string()
}

fn refund_event(
    charge_id: &str,
    payment_intent: &str,
    amount_refunded: i64,
    currency: &str,
) -> String {
    json!({
        "id": "evt_test_refund",
        "type": "charge.refunded",
        "data": { "object": {
            "id": charge_id,
            "object": "charge",
            "payment_intent": payment_intent,
            "amount": amount_refunded,
            "amount_refunded": amount_refunded,
            "currency": currency,
            "refunded": true,
        }}
    })
    .to_string()
}

/// Every `refund` ledger row for a user, oldest first, as
/// `(amount_usd, balance_after_usd, stripe_session_id)`.
async fn reversals(pool: &PgPool, user_id: Uuid) -> Vec<(Decimal, Decimal, String)> {
    query_as::<_, (Decimal, Decimal, String)>(
        r#"
        SELECT amount_usd, balance_after_usd, stripe_session_id
        FROM credit_ledger
        WHERE user_id = $1 AND entry_type = 'refund'
        ORDER BY id
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("reversal ledger rows must query")
}

async fn freeze_of(pool: &PgPool, user_id: Uuid) -> (Option<DateTime<Utc>>, Option<String>) {
    query_as::<_, (Option<DateTime<Utc>>, Option<String>)>(
        "SELECT frozen_at, frozen_reason FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("freeze state must query")
}

// ---------------------------------------------------------------------------
// Disputes and refunds
// ---------------------------------------------------------------------------

/// The headline case. A chargeback freezes the account and takes the credit
/// back, and it does so EXACTLY ONCE however many times Stripe redelivers —
/// including when the second delivery is a different Stripe object (a refund
/// of the same charge) rather than a replay of the same one.
#[tokio::test]
async fn a_dispute_freezes_the_account_and_reverses_the_credit_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, payment_intent) = buyer(&pool, "chargeback", Decimal::from(25)).await;
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let event = dispute_event(&dispute_id, &payment_intent, 2_500, "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the disputed credit is taken back"
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-25), Decimal::ZERO, dispute_id.clone())],
        "one refund ledger row, anchored to the dispute id"
    );
    let (frozen_at, reason) = freeze_of(&pool, user_id).await;
    assert!(frozen_at.is_some(), "a dispute freezes the account");
    assert_eq!(reason.as_deref(), Some("dispute"));
    let froze_at = frozen_at.expect("frozen");

    // Stripe redelivers on its own schedule: the replay must move nothing.
    let (status, _) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "a replayed dispute must not reverse twice"
    );
    assert_eq!(reversals(&pool, user_id).await.len(), 1);
    assert_eq!(
        freeze_of(&pool, user_id).await.0,
        Some(froze_at),
        "a replay must not restamp when the account was frozen"
    );

    // A DIFFERENT Stripe object reversing the SAME charge — an operator
    // refunding a charge that was also disputed — carries a different id, so
    // the object-id anchor alone would let it reverse the purchase a second
    // time. The per-purchase check is what stops it.
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "a second Stripe object must not reverse the same purchase again"
    );
    assert_eq!(reversals(&pool, user_id).await.len(), 1);
}

/// A refund is not an accusation: the money goes back and so does the credit,
/// but the account keeps working.
#[tokio::test]
async fn a_refund_reverses_the_credit_without_freezing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, payment_intent) = buyer(&pool, "refund", Decimal::from(10)).await;
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    let (status, body) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 1_000, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-10), Decimal::ZERO, charge_id)]
    );
    assert_eq!(
        freeze_of(&pool, user_id).await,
        (None, None),
        "a refund must not freeze the account"
    );
}

/// The receivable. When the credit has already been spent, reversing it puts
/// the balance below zero and LEAVES it there — that number is the debt, and
/// clamping it at zero would silently forgive it.
#[tokio::test]
async fn a_dispute_on_spent_credit_leaves_a_negative_receivable() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, payment_intent) = buyer(&pool, "spent", Decimal::from(25)).await;
    // Stand-in for "the customer consumed $20 of inference": settlement's own
    // arithmetic is pinned by tests/billing.rs, and what this test is about is
    // what the REVERSAL does to a balance that has already been drawn down.
    query("UPDATE users SET credit_balance_usd = 5 WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("spend simulation must apply");

    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let (status, _) = post_webhook(
        &pool,
        &dispute_event(&dispute_id, &payment_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(-20),
        "the whole credit is reversed; the shortfall is the receivable"
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-25), Decimal::from(-20), dispute_id)],
        "the ledger snapshots the negative balance it produced"
    );
    assert!(freeze_of(&pool, user_id).await.0.is_some());
}

/// The 0009 overdraft trigger. A reversal may drive the balance negative; a
/// plain balance write may not, so the 0003 backstop under settlement survives
/// the change that made the receivable possible.
#[tokio::test]
async fn only_a_declared_reversal_may_drive_the_balance_negative() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, _) = buyer(&pool, "overdraft", Decimal::from(5)).await;

    let refused = query("UPDATE users SET credit_balance_usd = -1 WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await;
    let error = refused.expect_err("an undeclared overdraft must be refused by the database");
    assert!(
        error.to_string().contains("cannot go negative"),
        "the failure must name the overdraft rule: {error}"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(5),
        "the refused write left the balance alone"
    );
}

/// A partial reversal is not apportioned by guesswork. The dispute half still
/// runs — the account freezes — but no ledger row is written, because "reverse
/// what the purchase credited" has no honest answer for a fraction.
#[tokio::test]
async fn a_partial_refund_reverses_nothing_and_leaves_the_credit_in_place() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, payment_intent) = buyer(&pool, "partial", Decimal::from(25)).await;
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    // $10 of a $25 charge.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 1_000, "usd"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "acknowledged so Stripe stops retrying something a redelivery cannot fix"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "a partial refund reverses nothing automatically"
    );
    assert!(reversals(&pool, user_id).await.is_empty());
    assert_eq!(freeze_of(&pool, user_id).await, (None, None));
}

/// A dispute in a currency the credit was not priced in is not reversed
/// either: the smallest unit of a zero-decimal currency can numerically match
/// a cents amount while being worth a fraction of it. The freeze still runs.
#[tokio::test]
async fn a_foreign_currency_dispute_freezes_but_reverses_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, payment_intent) = buyer(&pool, "currency", Decimal::from(25)).await;
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());

    let (status, _) = post_webhook(
        &pool,
        &dispute_event(&dispute_id, &payment_intent, 2_500, "jpy"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
    assert!(reversals(&pool, user_id).await.is_empty());
    assert!(
        freeze_of(&pool, user_id).await.0.is_some(),
        "the half that cannot wait still runs"
    );
}

/// A dispute against a charge this deployment never credited belongs to
/// something else in the Stripe account. It is acknowledged and ignored — no
/// reversal, and above all no freeze of an unrelated user.
#[tokio::test]
async fn a_dispute_on_an_uncredited_charge_touches_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, _) = buyer(&pool, "bystander", Decimal::from(25)).await;
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let foreign_intent = format!("pi_foreign_{}", Uuid::new_v4().simple());

    let (status, _) = post_webhook(
        &pool,
        &dispute_event(&dispute_id, &foreign_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
    assert!(reversals(&pool, user_id).await.is_empty());
    assert_eq!(freeze_of(&pool, user_id).await, (None, None));
}

/// The signature is the whole perimeter. An unsigned dispute and one signed
/// over different bytes are both refused before anything is parsed, and
/// neither freezes nor reverses.
#[tokio::test]
async fn an_unsigned_or_mis_signed_dispute_does_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, payment_intent) = buyer(&pool, "unsigned", Decimal::from(25)).await;
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let event = dispute_event(&dispute_id, &payment_intent, 2_500, "usd");

    let (status, body) = post_signed(&pool, &event, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("invalid_signature"));

    // A REAL HMAC, over different bytes — the shape a captured-and-edited
    // event has. Nothing about the signature check is relaxed for this test.
    let timestamp = Utc::now().timestamp();
    let elsewhere = sign(SECRET, timestamp, b"a different event entirely");
    let (status, body) =
        post_signed(&pool, &event, Some(signature_header(timestamp, &elsewhere))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("invalid_signature"));

    // ...and one signed with the wrong secret.
    let wrong_secret = sign("whsec_not_ours", timestamp, event.as_bytes());
    let (status, _) = post_signed(
        &pool,
        &event,
        Some(signature_header(timestamp, &wrong_secret)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "no unsigned event may move money"
    );
    assert!(reversals(&pool, user_id).await.is_empty());
    assert_eq!(
        freeze_of(&pool, user_id).await,
        (None, None),
        "no unsigned event may freeze an account"
    );
}

// ---------------------------------------------------------------------------
// What a freeze actually blocks
// ---------------------------------------------------------------------------

fn tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/request_path_tiers.toml")
}

fn served_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(1_000),
        output_tokens: Some(20),
        cached_input_tokens: None,
    }
}

/// A router whose single candidate is served by `fake`.
fn router(pool: PgPool, fake: Arc<FakeModelProvider>) -> RouterState {
    let route: InjectedRoute = Arc::new(move |resolved: &ResolvedRoute, _max_output_tokens| {
        ProviderRoute::from_candidates(
            resolved
                .candidates
                .iter()
                .cloned()
                .map(|definition| ProviderCandidate::with_provider(definition, fake.clone()))
                .collect(),
        )
    });
    RouterState::with_injected_route(tier_config_path(), pool, true, route)
}

fn completion_request(key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "zero/test-solo",
                "messages": [{ "role": "user", "content": "hello" }],
                "max_tokens": 4_096,
                "stream": false,
            })
            .to_string(),
        ))
        .expect("completion request should build")
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should be readable")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("response body should be JSON")
}

/// A funded user with one live key: `(user_id, email, plaintext key)`.
async fn funded_key(pool: &PgPool, label: &str) -> (Uuid, String, String) {
    let user_id = Uuid::new_v4();
    let email = format!("dispute-{label}-{user_id}@example.invalid");
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(&email)
        .execute(pool)
        .await
        .expect("test user must insert");
    let plaintext = generate_api_key();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'dispute-freeze', 20, 1000000)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(pool)
    .await
    .expect("test API key must insert");
    grant_promo(pool, user_id, Decimal::from(50), "dispute-freeze")
        .await
        .expect("funding promo must apply");
    (user_id, email, plaintext)
}

fn run_admin(database_url: &str, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zerorouter"))
        .env("DATABASE_URL", database_url)
        .arg("admin")
        .args(arguments)
        .output()
        .expect("admin command should start")
}

/// The customer-facing half of the freeze, over HTTP: a frozen account's
/// completion is refused by name — not as a generic failure, and not as
/// "insufficient credits", which would send a customer to buy credit that
/// cannot help — while the same request on the same key serves before the
/// freeze and again after `admin set-frozen --off` lifts it.
#[tokio::test]
async fn a_frozen_account_is_refused_by_name_and_unfreezing_restores_service() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, email, key) = funded_key(&pool, "admission").await;
    let fake = FakeModelProvider::new(
        "solo",
        vec![
            FakeOutcome::chat("before the freeze", served_usage()),
            FakeOutcome::chat("after the thaw", served_usage()),
        ],
    );
    let state = router(pool.clone(), fake.clone());

    // Baseline: this key serves.
    let served = app(state.clone())
        .oneshot(completion_request(&key))
        .await
        .expect("completion request should complete");
    assert_eq!(served.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;
    assert_eq!(
        json_body(served).await["choices"][0]["message"]["content"],
        "before the freeze"
    );

    freeze_account(&pool, user_id, FreezeReason::Dispute)
        .await
        .expect("freeze must apply");

    let refused = app(state.clone())
        .oneshot(completion_request(&key))
        .await
        .expect("completion request should complete");
    assert_eq!(
        refused.status(),
        StatusCode::PAYMENT_REQUIRED,
        "a frozen account is refused in the billing family"
    );
    let body = json_body(refused).await;
    assert_eq!(
        body["error"]["code"], "account_frozen",
        "the refusal names the freeze: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("frozen"),
        "{body}"
    );
    // Refused at admission: no upstream call and no reservation, so the freeze
    // costs ZeroRouter nothing in COGS.
    assert_eq!(
        fake.call_count(),
        1,
        "the frozen request reached no upstream"
    );
    assert_eq!(
        query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM usage_reservations r
            JOIN api_keys k ON k.id = r.api_key_id
            WHERE k.user_id = $1
            "#,
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("reservation count must query"),
        0,
        "a frozen request reserves nothing"
    );

    // The operator's release valve — the reason a freeze is safe to ship
    // before the review workflow exists.
    let thawed = run_admin(&database_url, &["set-frozen", "--email", &email, "--off"]);
    assert!(
        thawed.status.success(),
        "set-frozen --off must succeed: {}",
        String::from_utf8_lossy(&thawed.stderr)
    );
    let thawed: Value = serde_json::from_slice(&thawed.stdout).expect("set-frozen output is JSON");
    assert_eq!(thawed["frozen"], json!(false));
    assert_eq!(thawed["changed"], json!(true));

    let served_again = app(state.clone())
        .oneshot(completion_request(&key))
        .await
        .expect("completion request should complete");
    assert_eq!(
        served_again.status(),
        StatusCode::OK,
        "unfreezing restores service"
    );
    state.wait_for_background_tasks().await;
    assert_eq!(
        json_body(served_again).await["choices"][0]["message"]["content"],
        "after the thaw"
    );
}

/// `admin set-frozen --on` is the operator-initiated half, and the command
/// refuses to guess: neither flag, or both, is an error rather than a default.
#[tokio::test]
async fn set_frozen_requires_a_direction_and_an_existing_user() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Some(pool) = connect().await else {
        return;
    };
    let (user_id, email, _) = funded_key(&pool, "cli").await;

    let neither = run_admin(&database_url, &["set-frozen", "--email", &email]);
    assert!(!neither.status.success(), "a direction is required");
    assert!(
        String::from_utf8_lossy(&neither.stderr).contains("exactly one"),
        "the refusal says what is missing"
    );
    assert_eq!(freeze_of(&pool, user_id).await, (None, None));

    let missing = run_admin(
        &database_url,
        &["set-frozen", "--email", "nobody@example.invalid", "--on"],
    );
    assert!(!missing.status.success(), "an unknown user is refused");

    let frozen = run_admin(&database_url, &["set-frozen", "--email", &email, "--on"]);
    assert!(frozen.status.success());
    let frozen: Value = serde_json::from_slice(&frozen.stdout).expect("set-frozen output is JSON");
    assert_eq!(frozen["frozen"], json!(true));
    assert_eq!(frozen["frozen_reason"], json!("operator"));

    // Idempotent: freezing twice is not an error, and does not restamp.
    let again = run_admin(&database_url, &["set-frozen", "--email", &email, "--on"]);
    assert!(again.status.success());
    let again: Value = serde_json::from_slice(&again.stdout).expect("set-frozen output is JSON");
    assert_eq!(again["changed"], json!(false));
    assert_eq!(again["frozen_at"], frozen["frozen_at"]);
}

/// A freeze that stopped inference but still handed out fresh credentials
/// would be a freeze in name only. Both self-service mint paths — the portal
/// and the device claim — funnel through this one check.
#[tokio::test]
async fn a_frozen_account_cannot_mint_new_keys() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "mint").await;

    let mut transaction = pool.begin().await.expect("transaction must begin");
    assert!(
        matches!(
            admit_key_mint(&mut transaction, user_id)
                .await
                .expect("mint admission must query"),
            KeyMintAdmission::Allowed
        ),
        "a live account may mint"
    );
    transaction.rollback().await.expect("rollback must succeed");

    freeze_account(&pool, user_id, FreezeReason::Dispute)
        .await
        .expect("freeze must apply");

    let mut transaction = pool.begin().await.expect("transaction must begin");
    assert!(
        matches!(
            admit_key_mint(&mut transaction, user_id)
                .await
                .expect("mint admission must query"),
            KeyMintAdmission::AccountFrozen
        ),
        "a frozen account may not mint"
    );
    transaction.rollback().await.expect("rollback must succeed");
}

/// The freeze must also reach the autopay sweep. A chargeback reversal drives
/// the balance under the autopay threshold — often negative — and without this
/// the next sweep would charge the disputing customer's saved card again.
#[tokio::test]
async fn the_autopay_sweep_skips_frozen_accounts() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay").await;
    query(
        r#"
        UPDATE users
        SET stripe_customer_id = $2, autopay_enabled = TRUE,
            autopay_threshold_usd = 5, autopay_topup_usd = 25
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .bind(format!("cus_test_{}", user_id.simple()))
    .execute(&pool)
    .await
    .expect("autopay enablement must update");

    let listed = |candidates: Vec<zerorouter::billing::AutopayCandidate>| {
        candidates
            .into_iter()
            .any(|candidate| candidate.user_id == user_id)
    };
    assert!(
        listed(
            autopay_candidates(&pool, 1_000)
                .await
                .expect("candidates must query")
        ),
        "an eligible user is a candidate before the freeze"
    );

    freeze_account(&pool, user_id, FreezeReason::Dispute)
        .await
        .expect("freeze must apply");
    assert!(
        !listed(
            autopay_candidates(&pool, 1_000)
                .await
                .expect("candidates must query")
        ),
        "a frozen account is never charged"
    );
}

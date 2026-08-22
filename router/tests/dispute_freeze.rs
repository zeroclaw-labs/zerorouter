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
        AutopayOutcome, FreezeReason, autopay_candidates, balance, credit_purchase, freeze_account,
        grant_promo, resolve_reversal_against_credit, settle_autopay_intent, unfreeze_account,
        withheld_autopay_intents, write_off_receivable,
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
            publishable_key: "pk_test_unused".to_owned(),
            webhook_secret: SECRET.to_owned(),
            checkout_min_usd: Decimal::from(5),
            checkout_max_usd: Decimal::from(1000),
            api_base: "https://api.stripe.com".to_owned(),
            // A card-only deployment, which is what this harness describes.
            crypto_rail: false,
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
// HIGH-1: a reversal observed BEFORE its credit must converge to the same end
// state as a reversal after the credit (migration 0017)
// ---------------------------------------------------------------------------

/// The tombstone's state for a reversal object: `None` when no row exists,
/// `Some(applied)` where `applied` is whether a credit has consumed it.
async fn observed_reversal(pool: &PgPool, object_id: &str) -> Option<bool> {
    query_scalar::<_, bool>(
        "SELECT applied_at IS NOT NULL FROM stripe_observed_reversals WHERE object_id = $1",
    )
    .bind(object_id)
    .fetch_optional(pool)
    .await
    .expect("tombstone state must query")
}

/// The `purchase`/`autopay` credit rows for a user, so a test can confirm the
/// credit itself was still recorded (the reversal takes it back; it does not
/// suppress the credit — the full history stays in the ledger).
async fn purchase_rows(pool: &PgPool, user_id: Uuid) -> i64 {
    query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type IN ('purchase', 'autopay')",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("purchase rows must query")
}

/// A dispute that arrives before the credit exists is recorded, and when the
/// delayed success finally credits the PaymentIntent the account converges to
/// EXACTLY the reversal-after-credit end state: the credit is taken back (no
/// spendable refunded money), a reversal ledger row exists anchored on the
/// dispute, and the account is frozen. Consumed exactly once.
#[tokio::test]
async fn a_dispute_observed_before_the_credit_reverses_and_freezes_when_it_lands() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "pre-dispute").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());

    // The reversal arrives first — before any credit for this intent exists.
    let (status, _) = post_webhook(
        &pool,
        &dispute_event(&dispute_id, &payment_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the reversal is acknowledged");
    // Nothing has moved: there is no credit to reverse and no user to freeze.
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO
    );
    assert!(reversals(&pool, user_id).await.is_empty());
    assert_eq!(freeze_of(&pool, user_id).await, (None, None));
    // But it is durably recorded, unapplied — no longer forgotten.
    assert_eq!(
        observed_reversal(&pool, &dispute_id).await,
        Some(false),
        "the reversal is recorded and not yet applied"
    );

    // The delayed / retried success now credits the same PaymentIntent.
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(25),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");

    // Converged to the reversal-after-credit end state.
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "no spendable refunded credit remains"
    );
    assert_eq!(
        purchase_rows(&pool, user_id).await,
        1,
        "the credit itself is still recorded; the reversal takes it back"
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-25), Decimal::ZERO, dispute_id.clone())],
        "a reversal ledger entry exists, anchored on the dispute object"
    );
    let (frozen_at, reason) = freeze_of(&pool, user_id).await;
    assert!(frozen_at.is_some(), "a disputed account ends frozen");
    assert_eq!(reason.as_deref(), Some("dispute"));
    assert_eq!(
        observed_reversal(&pool, &dispute_id).await,
        Some(true),
        "the tombstone is consumed exactly once"
    );
}

/// A refund observed before the credit reverses the credit when it lands, but
/// — like the normal refund path — does NOT freeze.
#[tokio::test]
async fn a_refund_observed_before_the_credit_reverses_without_freezing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "pre-refund").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 1_000, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(observed_reversal(&pool, &charge_id).await, Some(false));

    // A $10 credit; the refund fully covers it.
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(10),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the refunded credit is not spendable"
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-10), Decimal::ZERO, charge_id.clone())]
    );
    assert_eq!(
        freeze_of(&pool, user_id).await,
        (None, None),
        "a refund must not freeze the account"
    );
    assert_eq!(observed_reversal(&pool, &charge_id).await, Some(true));
}

/// Redelivery is a no-op in BOTH directions. Once the credit has consumed the
/// tombstone (reversed + froze), a redelivered reversal webhook now finds the
/// credit row and takes the normal path, whose own idempotence (object-id
/// anchor + per-intent already-reversed check + first-writer-wins freeze) means
/// no second reversal and no restamped freeze.
#[tokio::test]
async fn a_reversal_redelivered_after_the_credit_auto_reversed_does_not_double() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "redeliver").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let event = dispute_event(&dispute_id, &payment_intent, 2_500, "usd");

    // Reversal before credit, then the credit consumes the tombstone.
    let (status, _) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK);
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(25),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");
    let frozen_first = freeze_of(&pool, user_id).await.0.expect("frozen");
    assert_eq!(reversals(&pool, user_id).await.len(), 1);

    // Stripe redelivers the SAME dispute. It now finds the credit row.
    let (status, _) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "a redelivered reversal must not reverse twice"
    );
    assert_eq!(
        reversals(&pool, user_id).await.len(),
        1,
        "still exactly one reversal ledger row"
    );
    assert_eq!(
        freeze_of(&pool, user_id).await.0,
        Some(frozen_first),
        "the redelivery must not restamp the freeze"
    );
}

/// Two distinct reversal objects naming the same intent (a dispute AND a refund
/// of the same charge) both recorded before the credit must reverse the credit
/// at most once — the per-intent already-reversed guard makes the second a
/// no-op — while the dispute still freezes.
#[tokio::test]
async fn two_reversal_objects_for_one_intent_do_not_double_count() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "two-objects").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    // Both reversals land before the credit, each covering the full $25.
    let (status, _) = post_webhook(
        &pool,
        &dispute_event(&dispute_id, &payment_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    credit_purchase(
        &pool,
        user_id,
        Decimal::from(25),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the credit is reversed once, not twice"
    );
    assert_eq!(
        reversals(&pool, user_id).await.len(),
        1,
        "exactly one reversal ledger row despite two reversal objects"
    );
    assert!(
        freeze_of(&pool, user_id).await.0.is_some(),
        "the dispute among them still freezes"
    );
    // Both tombstones are consumed.
    assert_eq!(observed_reversal(&pool, &dispute_id).await, Some(true));
    assert_eq!(observed_reversal(&pool, &charge_id).await, Some(true));
}

// ---------------------------------------------------------------------------
// FIX A (HIGH-1 round 2): the credit and reversal paths for one PaymentIntent
// serialize on the intent-keyed advisory lock (salt 1), so a credit and a
// reversal racing for the same intent converge instead of missing each other.
// ---------------------------------------------------------------------------

/// The reversed-cents recorded on a tombstone, or `None` when no row exists.
async fn tombstone_reversed_cents(pool: &PgPool, object_id: &str) -> Option<i64> {
    query_scalar::<_, Option<i64>>(
        "SELECT reversed_cents FROM stripe_observed_reversals WHERE object_id = $1",
    )
    .bind(object_id)
    .fetch_optional(pool)
    .await
    .expect("tombstone cents must query")
    .flatten()
}

/// Credit-first: while a credit transaction holds the intent lock (standing in
/// for a credit mid-flight, its ledger row inserted but not committed), a
/// reversal for the SAME intent must BLOCK on that lock rather than racing ahead
/// to write an orphan tombstone. Once the credit commits, the reversal sees it
/// and records nothing — the normal reverse-after-credit path takes over.
#[tokio::test]
async fn a_reversal_blocks_on_the_intent_lock_a_racing_credit_holds() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "race-credit-first").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());

    // A credit transaction mid-flight: holds the intent lock (salt 1) and has
    // inserted, but not committed, its purchase credit row.
    let mut credit_tx = pool.begin().await.expect("credit tx");
    query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 1))")
        .bind(&payment_intent)
        .execute(&mut *credit_tx)
        .await
        .expect("credit holds the intent lock");
    query(
        r#"
        INSERT INTO credit_ledger
            (user_id, entry_type, amount_usd, balance_after_usd,
             stripe_session_id, stripe_payment_intent_id)
        VALUES ($1, 'purchase', $2, $2, $3, $4)
        "#,
    )
    .bind(user_id)
    .bind(Decimal::from(25))
    .bind(&session_id)
    .bind(&payment_intent)
    .execute(&mut *credit_tx)
    .await
    .expect("uncommitted credit row");

    // The reversal races on its own connection; it must not complete while the
    // credit holds the lock.
    let mut reversal = tokio::spawn({
        let pool = pool.clone();
        let object_id = dispute_id.clone();
        let intent = payment_intent.clone();
        async move {
            resolve_reversal_against_credit(
                &pool,
                &object_id,
                &intent,
                true,
                Some(2_500),
                Some("usd"),
            )
            .await
        }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(750), &mut reversal)
            .await
            .is_err(),
        "the reversal blocks on the intent lock the credit holds"
    );
    assert_eq!(
        observed_reversal(&pool, &dispute_id).await,
        None,
        "no orphan tombstone is written while the reversal is blocked"
    );

    // The credit commits, releasing the lock; the reversal now sees the credit.
    credit_tx.commit().await.expect("commit credit");
    let resolved = reversal
        .await
        .expect("reversal task joins")
        .expect("resolve succeeds");
    assert!(
        resolved.is_some(),
        "the unblocked reversal sees the committed credit"
    );
    assert_eq!(
        observed_reversal(&pool, &dispute_id).await,
        None,
        "credit-first: the reversal records NO tombstone (it takes the normal path)"
    );
}

/// Reversal-first: while the reversal path holds the intent lock (its tombstone
/// written but not committed), a credit for the SAME intent must BLOCK rather
/// than committing spendable money that misses the tombstone. Once the reversal
/// commits, the credit proceeds under the lock, consumes the tombstone, and
/// converges to the reversed + frozen end state. This is the exact schedule the
/// round-1 fix could still lose.
#[tokio::test]
async fn a_credit_blocks_on_the_intent_lock_a_racing_reversal_holds_then_converges() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "race-reversal-first").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());

    // The reversal path mid-flight: holds the intent lock and has written, but
    // not committed, the dispute tombstone (covering the full $25 credit).
    let mut reversal_tx = pool.begin().await.expect("reversal tx");
    query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 1))")
        .bind(&payment_intent)
        .execute(&mut *reversal_tx)
        .await
        .expect("reversal holds the intent lock");
    query(
        r#"
        INSERT INTO stripe_observed_reversals
            (object_id, payment_intent_id, is_dispute, reversed_cents, currency)
        VALUES ($1, $2, TRUE, 2500, 'usd')
        "#,
    )
    .bind(&dispute_id)
    .bind(&payment_intent)
    .execute(&mut *reversal_tx)
    .await
    .expect("uncommitted tombstone");

    // The credit races on its own connection; it must block rather than commit.
    let mut credit = tokio::spawn({
        let pool = pool.clone();
        let session = session_id.clone();
        let intent = payment_intent.clone();
        async move {
            credit_purchase(
                &pool,
                user_id,
                Decimal::from(25),
                &session,
                Some(intent.as_str()),
            )
            .await
        }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(750), &mut credit)
            .await
            .is_err(),
        "the credit blocks on the intent lock the reversal holds"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "no spendable credit is committed while the reversal holds the lock"
    );

    // The reversal commits its tombstone, releasing the lock.
    reversal_tx.commit().await.expect("commit reversal");

    // The credit proceeds under the intent lock and converges.
    credit
        .await
        .expect("credit task joins")
        .expect("the delayed credit applies");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "converged: no spendable refunded credit remains"
    );
    assert!(
        freeze_of(&pool, user_id).await.0.is_some(),
        "the racing dispute froze the account"
    );
    assert_eq!(
        reversals(&pool, user_id).await.len(),
        1,
        "exactly one reversal ledger row"
    );
    assert_eq!(
        observed_reversal(&pool, &dispute_id).await,
        Some(true),
        "the tombstone is consumed exactly once"
    );
}

/// The autopay credit path (`settle_autopay_intent`) takes the SAME intent lock
/// after its user lock, so an off-session recharge racing a reversal for its
/// PaymentIntent serializes exactly as the checkout credit does. While the
/// reversal holds the lock (covering dispute tombstone uncommitted), the settle
/// must block rather than credit spendable money; once the reversal commits, the
/// settle consumes the tombstone and converges to reversed + frozen.
#[tokio::test]
async fn an_autopay_settle_blocks_on_the_intent_lock_a_racing_reversal_holds_then_converges() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "race-autopay-settle").await;
    // The settlement credit UPDATE now gates on `autopay_enabled` too, so the
    // account must be a genuinely armed autopay user for the credit path to run;
    // the sole disqualifier under test here is the racing reversal, not opt-out.
    arm_autopay(&pool, user_id).await;
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());

    // A pending autopay claim awaiting its terminal webhook.
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        VALUES ($1, $2, 25, 26.38)
        "#,
    )
    .bind(&payment_intent)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("pending autopay intent must insert");

    // The reversal path mid-flight: holds the intent lock with an uncommitted
    // covering dispute tombstone.
    let mut reversal_tx = pool.begin().await.expect("reversal tx");
    query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 1))")
        .bind(&payment_intent)
        .execute(&mut *reversal_tx)
        .await
        .expect("reversal holds the intent lock");
    query(
        r#"
        INSERT INTO stripe_observed_reversals
            (object_id, payment_intent_id, is_dispute, reversed_cents, currency)
        VALUES ($1, $2, TRUE, 2500, 'usd')
        "#,
    )
    .bind(&dispute_id)
    .bind(&payment_intent)
    .execute(&mut *reversal_tx)
    .await
    .expect("uncommitted tombstone");

    let mut settle = tokio::spawn({
        let pool = pool.clone();
        let intent = payment_intent.clone();
        async move { settle_autopay_intent(&pool, &intent, None).await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(750), &mut settle)
            .await
            .is_err(),
        "the autopay settle blocks on the intent lock the reversal holds"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "no autopay credit is committed while the reversal holds the lock"
    );

    reversal_tx.commit().await.expect("commit reversal");
    settle
        .await
        .expect("settle task joins")
        .expect("the recharge settles");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "converged: the recharge credit is reversed, nothing spendable"
    );
    assert!(
        freeze_of(&pool, user_id).await.0.is_some(),
        "the racing dispute froze the account"
    );
    assert_eq!(
        observed_reversal(&pool, &dispute_id).await,
        Some(true),
        "the tombstone is consumed exactly once"
    );
}

// ---------------------------------------------------------------------------
// FIX 1 (High): a frozen / indebted account is never CREDITED by autopay. A
// freeze that commits in the charge's send window — after the pre-POST guard,
// before settlement — makes settle_autopay_intent WITHHOLD the credit and move
// the collected charge to a durable needs-refund state instead of crediting it.
// ---------------------------------------------------------------------------

/// The headline FIX 1 case. A pending autopay charge has landed at Stripe. A
/// dispute-freeze on an OLDER intent commits before the terminal webhook is
/// settled. `settle_autopay_intent` re-checks eligibility under the per-user
/// lock and withholds: no credit, no `autopay` ledger row, the account stays
/// frozen, and the intent is recorded `withheld` (needs refund) and surfaced for
/// an operator. A redelivered success neither double-withholds nor credits.
#[tokio::test]
async fn an_autopay_charge_on_an_account_frozen_mid_charge_is_withheld_not_credited() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "withheld-frozen").await;
    // Armed autopay user, so the ONLY reason the settlement withholds is the
    // freeze — not the `autopay_enabled` gate the credit UPDATE now also applies.
    arm_autopay(&pool, user_id).await;
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());

    // A pending autopay claim whose charge succeeded at Stripe, awaiting its
    // terminal webhook. The gross Stripe collected is 26.38 for a net 25 credit.
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        VALUES ($1, $2, 25, 26.38)
        "#,
    )
    .bind(&payment_intent)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("pending autopay intent must insert");

    // The freeze committed during the charge's send window (a dispute on an
    // older intent), so by settlement the account is frozen.
    freeze_account(&pool, user_id, FreezeReason::Dispute)
        .await
        .expect("freeze must apply");

    let outcome = settle_autopay_intent(&pool, &payment_intent, None)
        .await
        .expect("settle must run");
    assert_eq!(
        outcome,
        AutopayOutcome::Withheld,
        "the charge on a frozen account is withheld, not credited"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the frozen account is NOT credited"
    );
    assert!(
        freeze_of(&pool, user_id).await.0.is_some(),
        "the account stays frozen"
    );
    let status = query_scalar::<_, String>(
        "SELECT status FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(&payment_intent)
    .fetch_one(&pool)
    .await
    .expect("status must query");
    assert_eq!(
        status, "withheld",
        "the collected charge is recorded as needs-refund"
    );
    let autopay_rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'autopay'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("ledger must query");
    assert_eq!(autopay_rows, 0, "no autopay credit row is written");

    // The withheld charge is surfaced for an out-of-band operator refund, with
    // the GROSS Stripe collected as the amount to refund.
    let withheld = withheld_autopay_intents(&pool)
        .await
        .expect("withheld list must query");
    assert!(
        withheld
            .iter()
            .any(|(intent, uid, gross)| intent == &payment_intent
                && *uid == user_id
                && *gross == Decimal::from_str("26.38").expect("gross parses")),
        "the withheld charge is surfaced for refund with its gross amount"
    );

    // A redelivered success is a no-op: the pending->terminal transition already
    // ran, so it neither double-withholds nor retroactively credits.
    let replay = settle_autopay_intent(&pool, &payment_intent, None)
        .await
        .expect("redelivery must run");
    assert_eq!(
        replay,
        AutopayOutcome::AlreadySettled,
        "a redelivered success for a withheld intent is a no-op"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "still not credited after redelivery"
    );
}

/// The eligible path is unchanged: a healthy account's autopay settlement still
/// credits the net top-up exactly as before.
#[tokio::test]
async fn an_autopay_charge_on_an_eligible_account_still_credits() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "withheld-eligible").await;
    // A genuinely armed autopay user: the credit UPDATE now requires
    // `autopay_enabled`, so the eligible path only fires for an opted-in account.
    arm_autopay(&pool, user_id).await;
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        VALUES ($1, $2, 25, 26.38)
        "#,
    )
    .bind(&payment_intent)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("pending autopay intent must insert");

    let outcome = settle_autopay_intent(&pool, &payment_intent, None)
        .await
        .expect("settle must run");
    assert_eq!(
        outcome,
        AutopayOutcome::Credited,
        "an eligible account is credited"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "the eligible account is credited the net top-up"
    );
    let status = query_scalar::<_, String>(
        "SELECT status FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(&payment_intent)
    .fetch_one(&pool)
    .await
    .expect("status must query");
    assert_eq!(status, "succeeded", "the eligible charge settles succeeded");
    assert!(
        withheld_autopay_intents(&pool)
            .await
            .expect("withheld list must query")
            .iter()
            .all(|(intent, _, _)| intent != &payment_intent),
        "an eligible settlement leaves nothing in the withheld list"
    );
    // Leave nothing armed and below-threshold for the global sweep suite.
    disarm_autopay(&pool, user_id).await;
}

/// FIX 1 completeness (round 5) — an autopay OPT-OUT that commits before the
/// terminal webhook is settled must be honored at settlement exactly like a
/// freeze: the collected charge is WITHHELD (surfaced for refund), never
/// credited. The credit UPDATE's `WHERE` now gates on `autopay_enabled`, so a
/// disabled account matches ZERO rows and takes the `withheld` path. Without the
/// flag in the predicate the opted-out account would be credited — this is the
/// mutation-checked guard.
#[tokio::test]
async fn an_autopay_charge_on_an_account_opted_out_mid_charge_is_withheld_not_credited() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "withheld-optout").await;
    // The account was a fully-armed autopay user when the charge was claimed.
    arm_autopay(&pool, user_id).await;
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());

    // A pending autopay claim whose charge succeeded at Stripe (gross 26.38 for a
    // net 25 credit), awaiting its terminal webhook.
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        VALUES ($1, $2, 25, 26.38)
        "#,
    )
    .bind(&payment_intent)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("pending autopay intent must insert");

    // The user disables autopay during the charge's send window (the portal's
    // off switch), and that opt-out commits before the terminal webhook settles.
    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("opt-out must commit");

    let outcome = settle_autopay_intent(&pool, &payment_intent, None)
        .await
        .expect("settle must run");
    assert_eq!(
        outcome,
        AutopayOutcome::Withheld,
        "the charge on an opted-out account is withheld, not credited"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the opted-out account is NOT credited"
    );
    let status = query_scalar::<_, String>(
        "SELECT status FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(&payment_intent)
    .fetch_one(&pool)
    .await
    .expect("status must query");
    assert_eq!(
        status, "withheld",
        "the collected charge is recorded as needs-refund"
    );
    let autopay_rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'autopay'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("ledger must query");
    assert_eq!(autopay_rows, 0, "no autopay credit row is written");

    // The withheld charge is surfaced for an out-of-band operator refund, with
    // the GROSS Stripe collected as the amount to refund.
    let withheld = withheld_autopay_intents(&pool)
        .await
        .expect("withheld list must query");
    assert!(
        withheld
            .iter()
            .any(|(intent, uid, gross)| intent == &payment_intent
                && *uid == user_id
                && *gross == Decimal::from_str("26.38").expect("gross parses")),
        "the withheld charge is surfaced for refund with its gross amount"
    );
}

/// FIX 1' (round 4), test rewritten in round 5 to actually distinguish the
/// atomic conditional UPDATE from a separate `SELECT`-then-`UPDATE`.
///
/// The credit is a single `UPDATE users SET ... WHERE id = $1 AND (eligibility)`.
/// To prove that predicate is re-evaluated AFTER a wait on the users-row lock —
/// against the newly-committed row version (EvalPlanQual), not a snapshot taken
/// earlier — this test makes the settlement's credit UPDATE itself BLOCK on the
/// users row:
///
/// 1. A second connection opens a transaction and runs an UNCOMMITTED
///    `UPDATE users SET frozen_at = NOW() ... WHERE id = $1`, taking (and
///    holding) the row lock without committing.
/// 2. Settlement runs; it claims the pending->succeeded transition and takes its
///    locks, then its credit UPDATE blocks on that held row lock.
/// 3. The freezing transaction COMMITS. The row is now frozen; the credit UPDATE
///    unblocks and EvalPlanQual re-checks `(eligibility)` against the committed
///    frozen row → ZERO rows → the credit is WITHHELD.
///
/// This is exactly the ordering the old implementation would get WRONG: a
/// separate `SELECT (eligibility)` runs BEFORE the credit UPDATE and does NOT
/// wait on the row lock — it reads the last-committed (still-eligible) version,
/// concludes "eligible", and the following unconditional `UPDATE` (which DOES
/// wait, then proceeds after the freeze commits) credits the frozen account. So
/// this test PASSES for the folded conditional UPDATE and would FAIL for the
/// separate-SELECT shape, which the round-4 intent-lock version could not tell
/// apart. The intent-lock race test above is kept — it covers a different
/// ordering (blocking before the credit statement is ever reached).
#[tokio::test]
async fn a_freeze_committing_while_the_credit_update_waits_on_the_row_lock_withholds() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "withheld-intra-txn").await;
    // Eligible-until-the-freeze: an armed autopay user whose only disqualifier is
    // the freeze that commits while the credit UPDATE is waiting on its row lock.
    arm_autopay(&pool, user_id).await;
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());

    // A pending autopay claim whose charge succeeded at Stripe (gross 26.38 for a
    // net 25 credit), awaiting its terminal webhook.
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        VALUES ($1, $2, 25, 26.38)
        "#,
    )
    .bind(&payment_intent)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("pending autopay intent must insert");

    // A second connection holds an UNCOMMITTED freeze on the users row: it owns
    // the row lock but has not committed, so the row is not yet frozen to any
    // other reader. freeze_account takes no advisory lock, so settlement still
    // acquires its per-user lock and runs all the way to the credit UPDATE, which
    // is where it meets this row lock.
    let mut freezer = pool.begin().await.expect("freezer tx");
    query("UPDATE users SET frozen_at = NOW(), frozen_reason = 'dispute' WHERE id = $1")
        .bind(user_id)
        .execute(&mut *freezer)
        .await
        .expect("freezer holds the users-row lock, uncommitted");

    let mut settle = tokio::spawn({
        let pool = pool.clone();
        let intent = payment_intent.clone();
        async move { settle_autopay_intent(&pool, &intent, None).await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(750), &mut settle)
            .await
            .is_err(),
        "the credit UPDATE blocks on the users-row lock the uncommitted freeze holds"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "nothing is credited while the credit UPDATE waits on the row lock"
    );

    // Commit the freeze NOW, while the credit UPDATE is waiting on the row lock.
    // The UPDATE unblocks and re-evaluates its WHERE against the freshly-committed
    // frozen row (EvalPlanQual), matches zero rows, and withholds.
    freezer
        .commit()
        .await
        .expect("commit the freeze under the waiting UPDATE");
    let outcome = settle
        .await
        .expect("settle task joins")
        .expect("settle must run");

    assert_eq!(
        outcome,
        AutopayOutcome::Withheld,
        "a freeze committing while the credit UPDATE waits on the row lock withholds the credit"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the frozen account is NOT credited"
    );
    let status = query_scalar::<_, String>(
        "SELECT status FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(&payment_intent)
    .fetch_one(&pool)
    .await
    .expect("status must query");
    assert_eq!(
        status, "withheld",
        "the collected charge is marked needs-refund"
    );
    let autopay_rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'autopay'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("ledger must query");
    assert_eq!(autopay_rows, 0, "no autopay credit row is written");
    let withheld = withheld_autopay_intents(&pool)
        .await
        .expect("withheld list must query");
    assert!(
        withheld
            .iter()
            .any(|(intent, uid, gross)| intent == &payment_intent
                && *uid == user_id
                && *gross == Decimal::from_str("26.38").expect("gross parses")),
        "the withheld charge is surfaced for refund with its gross amount"
    );
}

// ---------------------------------------------------------------------------
// FIX C (Medium): cumulative refunds keyed on the charge id must merge, and a
// non-covering refund tombstone must not be marked "applied".
// ---------------------------------------------------------------------------

/// Two `charge.refunded` events for one charge carry the CUMULATIVE
/// `amount_refunded` under the SAME charge id. A partial $4 followed by the full
/// $10, both before the credit, must merge to the fuller $10 (not drop the
/// second as a duplicate), so when the $10 credit lands it is reversed in full.
#[tokio::test]
async fn a_partial_then_full_cumulative_refund_before_the_credit_reverses_in_full() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "cumulative-refund").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    // Partial $4 first, then the full cumulative $10 — same charge id.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 400, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        tombstone_reversed_cents(&pool, &charge_id).await,
        Some(400),
        "the first, partial refund is recorded"
    );
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 1_000, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        tombstone_reversed_cents(&pool, &charge_id).await,
        Some(1_000),
        "the later, fuller cumulative refund is merged in, not dropped"
    );
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(false),
        "still one unapplied tombstone for the charge"
    );

    // The delayed $10 credit lands; the merged refund now covers it in full.
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(10),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the fully-refunded credit is not spendable"
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-10), Decimal::ZERO, charge_id.clone())],
        "one reversal for the full merged amount"
    );
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(true),
        "the covering tombstone is consumed"
    );
}

/// A single partial refund that never grows to cover the credit must NOT be
/// stamped applied when the credit lands: the credit stays in place (partial
/// refunds reverse nothing, per policy) and the tombstone remains an
/// operator-visible reconciliation flag a later cumulative refund can still
/// merge into.
#[tokio::test]
async fn a_non_covering_refund_tombstone_stays_unapplied_after_the_credit() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "partial-refund").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    // A $4 partial refund against a $10 credit — it does not cover.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 400, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    credit_purchase(
        &pool,
        user_id,
        Decimal::from(10),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(10),
        "a partial refund reverses nothing; the credit stands"
    );
    assert!(
        reversals(&pool, user_id).await.is_empty(),
        "no reversal ledger row for a non-covering refund"
    );
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(false),
        "the non-covering tombstone stays UNAPPLIED — a reconciliation flag, not lost"
    );
}

/// FIX 3 — a non-covering refund tombstone left unapplied after the credit must
/// not linger forever as a false "unresolved money" signal. Once a later fuller
/// refund reverses the intent through the normal credited path, the stale
/// tombstone is stamped applied. Partial $4 before the credit; the $10 credit
/// lands (tombstone stays unapplied); a full $10 cumulative refund after the
/// credit reverses it exactly once, and the original tombstone ends applied.
#[tokio::test]
async fn a_stale_tombstone_is_stamped_applied_once_the_credited_path_reverses() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "stale-tombstone").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    // Partial $4 refund before the credit — recorded, non-covering.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 400, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The $10 credit lands; the partial tombstone stays unapplied (a flag), and
    // a partial refund reverses nothing.
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(10),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(false),
        "the non-covering tombstone is still unapplied after the credit"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(10),
        "a partial refund reversed nothing"
    );

    // A later full $10 cumulative refund (same charge id) arrives AFTER the
    // credit; it takes the normal credited path, reverses in full, and stamps
    // the once-stale tombstone applied.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 1_000, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the full refund reverses the credit"
    );
    assert_eq!(
        reversals(&pool, user_id).await,
        vec![(Decimal::from(-10), Decimal::ZERO, charge_id.clone())],
        "the reversal happened exactly once"
    );
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(true),
        "the once-stale tombstone is now stamped applied"
    );
}

/// FIX 2' (round 4) — the stale tombstone is stamped only AFTER the reversal
/// actually lands, not prematurely inside `resolve_reversal_against_credit`. Here
/// a covering refund arrives after the credit, but its `reverse_purchase` step
/// FAILS (its 5s user-lock wait times out, held by a gate). The webhook 503s and
/// NOTHING is reversed, so the tombstone must stay `applied_at IS NULL` — still
/// surfaced for reconciliation — rather than falsely claiming the reversal was
/// applied. (Round 3 stamped on the found lookup, before this failure, and would
/// mark it applied here.) The successful-stamp half is covered by
/// `a_stale_tombstone_is_stamped_applied_once_the_credited_path_reverses` above.
#[tokio::test]
async fn a_covering_refund_whose_reversal_fails_leaves_the_tombstone_unapplied() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "reversal-fails").await;
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());

    // A $4 partial refund before the credit — recorded, non-covering, unapplied.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 400, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The $10 credit lands; the partial tombstone stays unapplied.
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(10),
        &session_id,
        Some(payment_intent.as_str()),
    )
    .await
    .expect("the delayed credit must apply");
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(false),
        "the non-covering tombstone is unapplied after the credit"
    );

    // Hold the per-user advisory lock (salt 0) so the covering refund's
    // `reverse_purchase` cannot take it and times out after its 5s wait — the
    // reversal FAILS. `resolve_reversal_against_credit` takes only the intent lock
    // (salt 1), so the found lookup still runs; only `reverse_purchase` blocks.
    let mut gate = pool.begin().await.expect("gate tx");
    query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *gate)
        .await
        .expect("gate holds the user lock");

    // A full $10 cumulative refund arrives AFTER the credit. It covers, so the
    // handler runs `reverse_purchase`, which blocks on the held user lock and
    // times out; the webhook 503s.
    let (status, _) = post_webhook(
        &pool,
        &refund_event(&charge_id, &payment_intent, 1_000, "usd"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a failed reversal makes the webhook ask Stripe to retry"
    );

    gate.commit().await.expect("release the user lock");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(10),
        "the failed reversal moved no money; the credit still stands"
    );
    assert!(
        reversals(&pool, user_id).await.is_empty(),
        "no reversal ledger row for a failed reversal"
    );
    assert_eq!(
        observed_reversal(&pool, &charge_id).await,
        Some(false),
        "a FAILED reversal leaves the tombstone UNAPPLIED — still surfaced, not falsely cleared"
    );
}

// ---------------------------------------------------------------------------
// FIX 4 (Low): an object-id reuse whose anchors disagree is refused, not merged.
// ---------------------------------------------------------------------------

/// A reused Stripe object id carrying a DIFFERENT payment intent must not merge
/// its amount into the original tombstone. The `ON CONFLICT ... DO UPDATE WHERE`
/// now enforces anchor equality, so the mismatch is logged and refused and the
/// original `reversed_cents` is left untouched — a larger conflicting amount
/// cannot raise this intent's coverage.
#[tokio::test]
async fn an_object_id_reuse_with_a_different_intent_does_not_merge_its_amount() {
    let Some(pool) = connect().await else {
        return;
    };
    let charge_id = format!("ch_test_{}", Uuid::new_v4().simple());
    let intent_a = format!("pi_test_{}", Uuid::new_v4().simple());
    let intent_b = format!("pi_test_{}", Uuid::new_v4().simple());

    // A $4 refund tombstone for intent A (uncredited, so it is recorded).
    let (status, _) = post_webhook(&pool, &refund_event(&charge_id, &intent_a, 400, "usd")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tombstone_reversed_cents(&pool, &charge_id).await, Some(400));

    // The SAME charge id reused with a DIFFERENT intent and a larger $10 amount.
    // The anchor mismatch is acknowledged (HTTP 200) but the amount is refused.
    let (status, _) = post_webhook(&pool, &refund_event(&charge_id, &intent_b, 1_000, "usd")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the mismatch is acknowledged, not errored"
    );
    assert_eq!(
        tombstone_reversed_cents(&pool, &charge_id).await,
        Some(400),
        "the mismatched amount is refused, not merged"
    );
    let anchored_intent = query_scalar::<_, String>(
        "SELECT payment_intent_id FROM stripe_observed_reversals WHERE object_id = $1",
    )
    .bind(&charge_id)
    .fetch_one(&pool)
    .await
    .expect("anchor must query");
    assert_eq!(
        anchored_intent, intent_a,
        "the original intent anchor is unchanged"
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

/// Arm autopay on an existing account with a threshold of $5 and a $25 top-up,
/// exactly as the portal's autopay form does.
async fn arm_autopay(pool: &PgPool, user_id: Uuid) {
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
    .execute(pool)
    .await
    .expect("autopay enablement must update");
}

/// Disarm the accounts a candidate test armed. The sweep is GLOBAL, so an
/// account left armed here becomes a charge in `tests/autopay_sweep.rs` against
/// that suite's mock Stripe and poisons its counters.
async fn disarm_autopay(pool: &PgPool, user_id: Uuid) {
    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("autopay teardown must update");
}

/// GUARD (mutation-checked): the sweep must also skip an account that OWES
/// money, whether or not it is still frozen.
///
/// The freeze exclusion above is not enough, because the freeze is the half an
/// operator lifts first: "this customer is real again" and "this customer has
/// paid us back" are separate decisions, and `set-frozen --off` only makes the
/// first. A negative balance has exactly one meaning since 0009/0013 — a
/// reversal receivable, money already clawed back through Stripe — and such an
/// account satisfies every other autopay predicate maximally: it is the
/// furthest below its threshold, so it even sorts to the front of the
/// worklist. Left alone, the first sweep after the freeze lifted would take an
/// off-session charge on the saved card of a customer fresh off a payment
/// dispute, which is how the SECOND dispute gets manufactured.
///
/// Re-entry is therefore an explicit human decision, and this pins both halves:
/// the debtor is skipped while the debt stands, and is a candidate again the
/// moment `admin disputes resolve` settles it.
#[tokio::test]
async fn the_autopay_sweep_skips_an_unfrozen_debtor_until_the_receivable_is_settled() {
    let Some(pool) = connect().await else {
        return;
    };
    // The debtor, through the real money paths: bought $25, consumed $20 of
    // it, then had the whole purchase clawed back. What is left is a $20
    // receivable and an automatic dispute freeze.
    let (debtor, payment_intent) = buyer(&pool, "autopay-debtor", Decimal::from(25)).await;
    // Stand-in for "the customer consumed $20 of inference", as in
    // `a_dispute_on_spent_credit_leaves_a_negative_receivable`: what is under
    // test is candidate selection against the balance a reversal leaves.
    query("UPDATE users SET credit_balance_usd = 5 WHERE id = $1")
        .bind(debtor)
        .execute(&pool)
        .await
        .expect("spend simulation must apply");
    arm_autopay(&pool, debtor).await;
    let dispute_id = format!("dp_test_{}", Uuid::new_v4().simple());
    let (status, body) = post_webhook(
        &pool,
        &dispute_event(&dispute_id, &payment_intent, 2_500, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, debtor).await.expect("balance must query"),
        Decimal::from(-20),
        "the spent-through chargeback leaves a receivable"
    );

    // A solvent control, armed identically and just as far under its
    // threshold: a zero balance IS the ordinary autopay case, so if this one
    // ever stops being selected the exclusion has swallowed the feature.
    let solvent = create_user(&pool, "autopay-solvent").await;
    arm_autopay(&pool, solvent).await;

    // The freeze is lifted — the operator has decided the account is real —
    // but the money is still owed, and that alone must keep the card alone.
    assert!(
        unfreeze_account(&pool, debtor)
            .await
            .expect("unfreeze must apply"),
        "the dispute freeze must be liftable"
    );
    let listed = |candidates: &[zerorouter::billing::AutopayCandidate], user: Uuid| {
        candidates.iter().any(|candidate| candidate.user_id == user)
    };
    let candidates = autopay_candidates(&pool, 10_000)
        .await
        .expect("candidates must query");
    assert!(
        !listed(&candidates, debtor),
        "an unfrozen account that still owes money must never be recharged"
    );
    assert!(
        listed(&candidates, solvent),
        "an ordinary below-threshold account is still a candidate"
    );

    // Re-entry: the operator settles the receivable, which is the human
    // decision this exclusion exists to require. `disputes resolve
    // --write-off` is that decision, and it brings the balance to exactly
    // zero — at which point autopay resumes on its own.
    write_off_receivable(&pool, debtor, "test: uncollectable receivable")
        .await
        .expect("write-off must apply");
    assert_eq!(
        balance(&pool, debtor).await.expect("balance must query"),
        Decimal::ZERO
    );
    let candidates = autopay_candidates(&pool, 10_000)
        .await
        .expect("candidates must query");
    assert!(
        listed(&candidates, debtor),
        "a settled account rejoins autopay; the exclusion is a hold, not a ban"
    );

    disarm_autopay(&pool, debtor).await;
    disarm_autopay(&pool, solvent).await;
}

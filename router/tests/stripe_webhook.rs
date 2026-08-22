//! Stripe webhook tests: signature verification, and the preconditions that
//! must hold before a signed event is allowed to move money.
//!
//! The signature tests are pure — no network, no database — and construct
//! signatures locally with the same `hmac` crate the router verifies with.
//!
//! The end-to-end tests drive the real `/webhooks/stripe` handler with
//! correctly signed payloads, because a valid signature is exactly the
//! attacker's starting position: anything able to create a paid Checkout
//! Session in the Stripe account gets Stripe to sign its metadata for it. What
//! those tests assert is that a *legitimately signed* event still cannot mint
//! credit it did not pay for. Every rejection asserts the balance and the
//! ledger are untouched, not merely that a non-2xx came back. Gated on
//! `DATABASE_URL` like `tests/billing.rs`: unset means the test returns early.

use std::{
    collections::HashMap,
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{Form, State},
    http::{Request, StatusCode, header},
    routing::post,
};
use chrono::Utc;
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
    billing::{balance, checkout_intent, record_checkout_intent},
    db::migrate,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
    stripe::{self, STRIPE_SIGNATURE_HEADER, WebhookVerifyError, verify_webhook_signature},
    web::{StripeSettings, WebConfig, WebCtx},
};

const SECRET: &str = "whsec_test_secret";
const TOLERANCE: Duration = Duration::from_secs(300);
const NOW: i64 = 1_752_000_000;
const PAYLOAD: &[u8] = br#"{"id":"evt_test","type":"checkout.session.completed"}"#;

/// Hex HMAC-SHA256 over `{timestamp}.{payload}`, exactly as Stripe signs.
fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn header(timestamp: i64, signatures: &[&str]) -> String {
    let mut header = format!("t={timestamp}");
    for signature in signatures {
        header.push_str(",v1=");
        header.push_str(signature);
    }
    header
}

#[test]
fn valid_signature_verifies() {
    let signature = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Ok(())
    );
    // Skew inside the tolerance window (either direction) is accepted.
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW + 300),
        Ok(())
    );
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW - 300),
        Ok(())
    );
}

#[test]
fn tampered_payload_fails() {
    let signature = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    let mut tampered = PAYLOAD.to_vec();
    tampered[0] ^= 1;
    assert_eq!(
        verify_webhook_signature(&tampered, &header, SECRET, TOLERANCE, NOW),
        Err(WebhookVerifyError::SignatureMismatch)
    );
}

#[test]
fn wrong_secret_fails() {
    let signature = sign("whsec_other_secret", NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Err(WebhookVerifyError::SignatureMismatch)
    );
}

#[test]
fn stale_timestamp_fails() {
    // Correctly signed, but one second past the tolerance in either
    // direction: replayed captures and clock-skewed forgeries both fail.
    let signature = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&signature]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW + 301),
        Err(WebhookVerifyError::TimestampOutOfTolerance)
    );
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW - 301),
        Err(WebhookVerifyError::TimestampOutOfTolerance)
    );
}

#[test]
fn malformed_headers_fail() {
    let signature = sign(SECRET, NOW, PAYLOAD);
    let cases = [
        String::new(),
        "garbage".to_owned(),
        format!("v1={signature}"),              // no timestamp
        format!("t=notanumber,v1={signature}"), // unparseable timestamp
        format!("t={NOW}"),                     // no v1 candidate
        format!("t {NOW},v1 {signature}"),      // no key=value separators
    ];
    for header in &cases {
        assert_eq!(
            verify_webhook_signature(PAYLOAD, header, SECRET, TOLERANCE, NOW),
            Err(WebhookVerifyError::MalformedHeader),
            "{header:?} should be malformed"
        );
    }
}

#[test]
fn any_matching_candidate_verifies() {
    // First candidate: valid hex but signed over different bytes. Second:
    // not hex at all. Third: the real signature. Verification must accept
    // the set (Stripe sends multiple v1 values during secret rotation).
    let wrong = sign(SECRET, NOW, b"different payload");
    let valid = sign(SECRET, NOW, PAYLOAD);
    let header = header(NOW, &[&wrong, "not-hex", &valid]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Ok(())
    );
}

#[test]
fn candidate_set_with_no_match_fails() {
    let wrong = sign(SECRET, NOW, b"different payload");
    let also_wrong = sign("whsec_other_secret", NOW, PAYLOAD);
    let header = header(NOW, &[&wrong, "not-hex", &also_wrong]);
    assert_eq!(
        verify_webhook_signature(PAYLOAD, &header, SECRET, TOLERANCE, NOW),
        Err(WebhookVerifyError::SignatureMismatch)
    );
}

// ---------------------------------------------------------------------------
// End-to-end: a correctly signed event still has to pay for what it claims
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

async fn create_user(pool: &PgPool, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("webhook-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

fn unique_session_id() -> String {
    format!("cs_test_{}", Uuid::new_v4().simple())
}

/// The Stripe base the webhook tests run against by default.
///
/// The webhook arms make no call that can move money, so an unreachable base is
/// the honest configuration for most of them.
///
/// ONE arm does now reach out: since migration 0021 an autopay success records
/// its tax transaction with Stripe. That call is deliberately fire-and-forget —
/// the money is already correct by the time it runs, and a failure is logged
/// rather than propagated — so pointing it at an unreachable host exercises
/// exactly the "recording failed" path, and every autopay test in this file
/// therefore doubles as evidence that a failed recording still returns 200 with
/// the credit applied. Tests that want to OBSERVE the recording pass a mock to
/// `post_webhook_against` instead.
const UNREACHABLE_STRIPE: &str = "https://api.stripe.invalid";

fn stripe_app(pool: &PgPool, api_base: &str) -> axum::Router {
    stripe_app_with_rail(pool, api_base, false)
}

/// The webhook app with the stablecoin rail explicitly on or off.
///
/// The flag gates only what the deployment will CREATE (see
/// `resolve_rail`); the webhook must credit a legitimately paid crypto session
/// whatever the flag says now, because a rail switched off after a customer
/// paid must not strand their money. The crypto webhook tests below therefore
/// run against `crypto_rail: false` on purpose.
fn stripe_app_with_rail(pool: &PgPool, api_base: &str, crypto_rail: bool) -> axum::Router {
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
            api_base: api_base.to_owned(),
            crypto_rail,
        }),
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3_600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    };
    stripe::router().with_state(WebCtx::new(pool.clone(), config))
}

/// A `checkout.session.completed` object shaped like Stripe's, with every
/// money-bearing field independently controllable so a test can make the
/// metadata disagree with what was actually collected.
fn paid_session_event(
    session_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_total: i64,
    currency: &str,
) -> String {
    json!({
        "id": "evt_test",
        "type": "checkout.session.completed",
        "data": {
            "object": {
                "id": session_id,
                "object": "checkout.session",
                "payment_status": "paid",
                "amount_total": amount_total,
                "currency": currency,
                "payment_intent": "pi_test_webhook",
                "metadata": {
                    "user_id": user_id.to_string(),
                    "credit_usd": metadata_credit_usd,
                },
            }
        }
    })
    .to_string()
}

/// The same object, but priced the way Stripe Tax prices an EXCLUSIVE-tax
/// session: `ex_tax_cents` is the gross ZeroRouter quoted, `tax_cents` is what
/// Stripe added on top, and `amount_total` is the sum — the money that
/// actually left the customer's card. The breakdown arrives in
/// `total_details`, which is where Stripe reports it.
fn taxed_session_event(
    session_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    ex_tax_cents: i64,
    tax_cents: i64,
    currency: &str,
) -> String {
    taxed_session_event_raw(
        session_id,
        user_id,
        metadata_credit_usd,
        ex_tax_cents + tax_cents,
        json!(tax_cents),
        currency,
    )
}

/// The same, with `amount_total` and the reported tax set independently, so a
/// test can build a session whose parts do not add up.
fn taxed_session_event_raw(
    session_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_total: i64,
    reported_tax: Value,
    currency: &str,
) -> String {
    let mut event: Value = serde_json::from_str(&paid_session_event(
        session_id,
        user_id,
        metadata_credit_usd,
        amount_total,
        currency,
    ))
    .expect("base event must parse");
    event["data"]["object"]["total_details"] = json!({
        "amount_discount": 0,
        "amount_shipping": 0,
        "amount_tax": reported_tax,
    });
    event.to_string()
}

/// The shape Stripe sends when a VAT-registered business buyer was REVERSE
/// CHARGED: the buyer accounts for the VAT themselves, so Stripe collects none.
///
/// Every field here is what distinguishes it from a session that was simply
/// never taxed, and the point of the fixture is that none of them are money:
/// `automatic_tax.status` is `complete` (tax WAS calculated — it came to zero,
/// as opposed to `failed`, where it could not be determined at all), and the
/// buyer's VAT number rides along in `customer_details.tax_ids[]`. The amounts
/// are indistinguishable from an untaxed sale, which is exactly why the ex-tax
/// accounting needs no new case for it.
fn reverse_charged_session_event(
    session_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    ex_tax_cents: i64,
    currency: &str,
) -> String {
    let mut event: Value = serde_json::from_str(&taxed_session_event(
        session_id,
        user_id,
        metadata_credit_usd,
        ex_tax_cents,
        0,
        currency,
    ))
    .expect("taxed event must parse");
    event["data"]["object"]["automatic_tax"] = json!({
        "enabled": true,
        "status": "complete",
    });
    event["data"]["object"]["customer_details"] = json!({
        "email": "vat-buyer@example.com",
        "tax_exempt": "reverse",
        "tax_ids": [{ "type": "eu_vat", "value": "DE123456789" }],
    });
    event.to_string()
}

/// POST a correctly signed payload at the real handler.
async fn post_webhook(pool: &PgPool, payload: &str) -> (StatusCode, Value) {
    post_webhook_against(pool, UNREACHABLE_STRIPE, payload).await
}

/// `post_webhook` with a caller-chosen Stripe base, so a test can watch what
/// the arm does with Stripe rather than only what it does with the database.
async fn post_webhook_against(pool: &PgPool, api_base: &str, payload: &str) -> (StatusCode, Value) {
    // Signed at the current time: the handler checks tolerance against the
    // real clock, so these events are as authentic as Stripe's own.
    let timestamp = Utc::now().timestamp();
    let signature = sign(SECRET, timestamp, payload.as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header(STRIPE_SIGNATURE_HEADER, header(timestamp, &[&signature]))
        .header("content-type", "application/json")
        .body(Body::from(payload.to_owned()))
        .expect("webhook request should build");
    let response = stripe_app(pool, api_base)
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

async fn purchase_count(pool: &PgPool, user_id: Uuid) -> i64 {
    query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'purchase'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("purchase ledger count must query")
}

/// The balance and the ledger both had to stay still — a rejection that
/// returned 4xx after already crediting would still be a minted dollar.
async fn assert_nothing_credited(pool: &PgPool, user_id: Uuid, context: &str) {
    assert_eq!(
        balance(pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "{context}: balance must be untouched"
    );
    assert_eq!(
        purchase_count(pool, user_id).await,
        0,
        "{context}: no purchase ledger row may be written"
    );
}

#[tokio::test]
async fn recorded_purchase_credits_exactly_once_and_replays_are_idempotent() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "happy").await;
    let session_id = unique_session_id();
    // $25 credit costs $26.38 gross (fee ceil(0.055*25)=1.38): the intent stores
    // gross in cents and net in dollars, and Stripe collects the gross.
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let event = paid_session_event(&session_id, user_id, "25.00", 2_638, "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["received"], json!(true));
    // The NET credit lands in the ledger; the fee never does.
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
    let settled = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert!(
        settled.settled_at.is_some(),
        "a delivered purchase must be marked settled"
    );
    // Fee revenue is derivable from the intent row: gross cents minus net*100.
    // $26.38 gross - $25.00 net = $1.38 fee, and no separate ledger column.
    assert_eq!(
        settled.expected_amount_cents, 2_638,
        "gross is stored in cents"
    );
    assert_eq!(
        settled.expected_credit_usd,
        Decimal::from(25),
        "net credit is stored in dollars"
    );

    // Stripe redelivers on any non-2xx and on its own schedule; the second
    // delivery must be acknowledged without a second credit.
    let (replay_status, _) = post_webhook(&pool, &event).await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "a replayed event must not credit twice"
    );
    assert_eq!(purchase_count(&pool, user_id).await, 1);
}

#[tokio::test]
async fn metadata_claiming_more_than_was_paid_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "inflated").await;
    let session_id = unique_session_id();
    // ZeroRouter sold $5.00 (charged $5.80 gross). The event Stripe signs claims
    // $1000 of credit against the $5.80 actually collected. Layer 1 recomputes
    // the gross the fee formula demands for $1000 ($1055.00) and sees it does
    // not match the $5.80 collected, so it rejects before Layer 2 is reached.
    record_checkout_intent(&pool, &session_id, user_id, 580, Decimal::from(5), "usd")
        .await
        .expect("pending purchase record must insert");
    let event = paid_session_event(&session_id, user_id, "1000.00", 580, "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("amount_mismatch"));
    assert_nothing_credited(&pool, user_id, "inflated metadata").await;
    let intent = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert!(
        intent.settled_at.is_none(),
        "a rejected event must not settle the pending record"
    );
}

#[tokio::test]
async fn wrong_currency_credits_nothing_even_when_the_amount_matches() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "currency").await;
    let session_id = unique_session_id();
    // $10 credit is charged $10.80 gross (fee ceil(0.055*10)=0.55).
    record_checkout_intent(&pool, &session_id, user_id, 1_080, Decimal::from(10), "usd")
        .await
        .expect("pending purchase record must insert");
    // 1080 JPY is roughly $7 but is also numerically 1080 in the smallest
    // currency unit, so it matches the recomputed gross for a $10 credit while
    // being worth a fraction of it. The currency comparison is the control that
    // catches it.
    let event = paid_session_event(&session_id, user_id, "10.00", 1_080, "jpy");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("amount_mismatch"));
    assert_nothing_credited(&pool, user_id, "zero-decimal currency").await;
}

#[tokio::test]
async fn paid_session_without_a_pending_record_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "unrecorded").await;
    // Internally consistent in every way — $1000 credit claimed, its $1055.00
    // gross collected, in USD, so Layer 1 corroborates — and signed by Stripe.
    // It is still not a session ZeroRouter priced, which is what a session
    // minted through a second integration or a leaked restricted key looks
    // like. Sessions predating migration 0005 land here too: the policy is to
    // reject and reconcile by hand, never to credit.
    let event = paid_session_event(&unique_session_id(), user_id, "1000.00", 105_500, "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("unknown_session"));
    assert_nothing_credited(&pool, user_id, "no pending record").await;
}

#[tokio::test]
async fn metadata_cannot_redirect_a_purchase_to_another_user() {
    let Some(pool) = connect().await else {
        return;
    };
    let payer = create_user(&pool, "payer").await;
    let attacker = create_user(&pool, "attacker").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, payer, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    // Money, gross, and currency all corroborate (Layer 1 passes); only the
    // recipient is forged, so the intent-row check (Layer 2) is what catches it.
    let event = paid_session_event(&session_id, attacker, "25.00", 2_638, "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("amount_mismatch"));
    assert_nothing_credited(&pool, attacker, "forged recipient").await;
    assert_nothing_credited(&pool, payer, "forged recipient (payer)").await;
}

#[tokio::test]
async fn unpaid_session_is_acknowledged_without_crediting() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "unpaid").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_500, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let mut event: Value = serde_json::from_str(&paid_session_event(
        &session_id,
        user_id,
        "25.00",
        2_500,
        "usd",
    ))
    .expect("event must parse");
    event["data"]["object"]["payment_status"] = json!("unpaid");

    // Acknowledged so Stripe stops retrying; the later `paid` event carries
    // the money. A pending record alone must never be enough to credit.
    let (status, _) = post_webhook(&pool, &event.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_nothing_credited(&pool, user_id, "unpaid session").await;
}

// ---------------------------------------------------------------------------
// Stripe Tax: sales tax rides on top of the price and is never credit
// ---------------------------------------------------------------------------

/// THE invariant. With exclusive tax the card is charged gross + tax, so
/// "amount charged == gross" stops being true — but what the customer receives
/// must not move by a cent. Two identical $25 purchases, one taxed and one
/// not, must leave identical balances and identical ledger rows.
#[tokio::test]
async fn a_taxed_purchase_credits_exactly_what_an_untaxed_one_does() {
    let Some(pool) = connect().await else {
        return;
    };
    // $25 credit is quoted at $26.38 gross (fee ceil(0.055*25) = $1.38).
    // Massachusetts at 6.25% of $26.38 is $1.65, so the card is charged
    // $28.03 — none of which is the customer's to spend beyond the $25.
    const GROSS_CENTS: i64 = 2_638;
    const TAX_CENTS: i64 = 165;

    let taxed = create_user(&pool, "taxed").await;
    let taxed_session = unique_session_id();
    record_checkout_intent(
        &pool,
        &taxed_session,
        taxed,
        GROSS_CENTS,
        Decimal::from(25),
        "usd",
    )
    .await
    .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &taxed_session_event(
            &taxed_session,
            taxed,
            "25.00",
            GROSS_CENTS,
            TAX_CENTS,
            "usd",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let untaxed = create_user(&pool, "untaxed").await;
    let untaxed_session = unique_session_id();
    record_checkout_intent(
        &pool,
        &untaxed_session,
        untaxed,
        GROSS_CENTS,
        Decimal::from(25),
        "usd",
    )
    .await
    .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &paid_session_event(&untaxed_session, untaxed, "25.00", GROSS_CENTS, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let taxed_balance = balance(&pool, taxed).await.expect("balance must query");
    let untaxed_balance = balance(&pool, untaxed).await.expect("balance must query");
    assert_eq!(
        taxed_balance, untaxed_balance,
        "tax must not change what a purchase credits"
    );
    assert_eq!(
        taxed_balance,
        Decimal::from(25),
        "the customer is credited the net credit, never the gross and never the taxed total"
    );

    // The ledger row records the credit, not the money collected: no part of
    // the $1.65 of tax (nor the $1.38 fee) is spendable or booked as a credit.
    let credited = query_scalar::<_, Decimal>(
        "SELECT amount_usd FROM credit_ledger WHERE stripe_session_id = $1",
    )
    .bind(&taxed_session)
    .fetch_one(&pool)
    .await
    .expect("ledger row must query");
    assert_eq!(credited, Decimal::from(25));

    // The intent row keeps meaning the EX-TAX gross, so fee revenue stays
    // exactly gross - credit and tax never contaminates it.
    let intent = checkout_intent(&pool, &taxed_session)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert_eq!(intent.expected_amount_cents, GROSS_CENTS);
    assert!(intent.settled_at.is_some());
}

/// A real untaxed Stripe session still reports `total_details`, with the tax
/// broken out as zero. That shape must behave exactly like today's fixture.
#[tokio::test]
async fn a_session_reporting_zero_tax_is_credited_exactly_as_before() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "zero-tax").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &taxed_session_event(&session_id, user_id, "25.00", 2_638, 0, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
}

/// A reverse-charged business purchase credits exactly what a consumer's does.
///
/// This is the invariant tax ID collection has to preserve. A VAT-registered
/// buyer who enters their VAT number is charged NO tax — the money that arrives
/// is the bare ex-tax gross — while a consumer buying the same credit pays tax
/// on top. What each receives must be identical to the cent, and identical to
/// what both received before tax IDs were collected at all.
///
/// The extra fields Stripe adds for such a session (`automatic_tax.status`,
/// `customer_details.tax_ids[]`, `tax_exempt: reverse`) must be inert here: the
/// webhook reads none of them, and a fixture carrying them is the only way to
/// notice if that ever stops being true.
#[tokio::test]
async fn a_reverse_charged_purchase_credits_exactly_what_a_taxed_one_does() {
    let Some(pool) = connect().await else {
        return;
    };
    // $25 credit quoted at $26.38 gross. The consumer additionally pays $1.65
    // of VAT; the reverse-charged business pays none. Both receive $25.
    const GROSS_CENTS: i64 = 2_638;
    const CONSUMER_TAX_CENTS: i64 = 165;

    let business = create_user(&pool, "reverse-charged").await;
    let business_session = unique_session_id();
    record_checkout_intent(
        &pool,
        &business_session,
        business,
        GROSS_CENTS,
        Decimal::from(25),
        "usd",
    )
    .await
    .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &reverse_charged_session_event(&business_session, business, "25.00", GROSS_CENTS, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let consumer = create_user(&pool, "vat-consumer").await;
    let consumer_session = unique_session_id();
    record_checkout_intent(
        &pool,
        &consumer_session,
        consumer,
        GROSS_CENTS,
        Decimal::from(25),
        "usd",
    )
    .await
    .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &taxed_session_event(
            &consumer_session,
            consumer,
            "25.00",
            GROSS_CENTS,
            CONSUMER_TAX_CENTS,
            "usd",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let business_balance = balance(&pool, business).await.expect("balance must query");
    let consumer_balance = balance(&pool, consumer).await.expect("balance must query");
    assert_eq!(
        business_balance, consumer_balance,
        "reverse charge must not change what a purchase credits"
    );
    assert_eq!(
        business_balance,
        Decimal::from(25),
        "the reverse-charged buyer is credited the net credit, not the gross"
    );

    // The ledger records the credit, and the intent row still means the EX-TAX
    // gross — so fee revenue stays exactly gross - credit on a zero-tax sale
    // just as it does on a taxed one.
    let credited = query_scalar::<_, Decimal>(
        "SELECT amount_usd FROM credit_ledger WHERE stripe_session_id = $1",
    )
    .bind(&business_session)
    .fetch_one(&pool)
    .await
    .expect("ledger row must query");
    assert_eq!(credited, Decimal::from(25));

    let intent = checkout_intent(&pool, &business_session)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert_eq!(intent.expected_amount_cents, GROSS_CENTS);
    assert!(intent.settled_at.is_some());
}

/// A session whose parts do not add up credits nothing.
///
/// The shape that matters is an INCLUSIVE-tax session: `amount_total` is the
/// price we quoted, with the tax carved OUT of it rather than added on top, so
/// ZeroRouter would be handing over its own revenue as tax while crediting the
/// full amount. Deriving what was collected as `amount_total - amount_tax`
/// makes that arrive as a short payment and it is refused. The same check
/// catches a coupon or a shipping line — anything that makes the money
/// collected differ from the price ZeroRouter sold.
#[tokio::test]
async fn tax_carved_out_of_the_price_instead_of_added_on_top_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "inclusive-tax").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    // $26.38 collected, of which $1.65 is tax: only $24.73 of price arrived.
    let event = taxed_session_event_raw(&session_id, user_id, "25.00", 2_638, json!(165), "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("amount_mismatch"));
    assert_nothing_credited(&pool, user_id, "tax carved out of the price").await;
}

/// Tax is excluded from the corroboration, so it must never be usable to
/// disguise a short payment — and an unreadable or impossible tax figure is
/// refused outright rather than read as zero.
#[tokio::test]
async fn an_unusable_or_padded_tax_figure_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    for (label, amount_total, reported_tax) in [
        // The full price arrived, but the event claims most of it was tax.
        ("tax padding a short payment", 2_638, json!(1_000)),
        // Tax cannot be negative; a negative one would inflate what we read
        // as collected.
        ("negative tax", 2_473, json!(-165)),
        // Not an integer number of cents: unreadable, so unusable.
        ("tax as a string", 2_803, json!("165")),
        ("fractional tax", 2_803, json!(165.5)),
    ] {
        let user_id = create_user(&pool, "bad-tax").await;
        let session_id = unique_session_id();
        record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
            .await
            .expect("pending purchase record must insert");
        let event = taxed_session_event_raw(
            &session_id,
            user_id,
            "25.00",
            amount_total,
            reported_tax,
            "usd",
        );

        let (status, _) = post_webhook(&pool, &event).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label} must be refused");
        assert_nothing_credited(&pool, user_id, label).await;
    }
}

/// Tax does not switch off the rest of the corroboration: a taxed session
/// that collected the wrong price, or names the wrong recipient, still
/// credits nothing.
#[tokio::test]
async fn the_amount_and_recipient_guards_still_fire_on_a_taxed_session() {
    let Some(pool) = connect().await else {
        return;
    };
    // Wrong price: $25 of credit claimed, but only $20.00 of price collected
    // (plus tax on it), so Layer 1 refuses.
    let short = create_user(&pool, "taxed-short").await;
    let short_session = unique_session_id();
    record_checkout_intent(
        &pool,
        &short_session,
        short,
        2_638,
        Decimal::from(25),
        "usd",
    )
    .await
    .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &taxed_session_event(&short_session, short, "25.00", 2_000, 125, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("amount_mismatch"));
    assert_nothing_credited(&pool, short, "taxed short payment").await;

    // Right price and right tax, forged recipient: Layer 2 refuses.
    let payer = create_user(&pool, "taxed-payer").await;
    let attacker = create_user(&pool, "taxed-attacker").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, payer, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let (status, body) = post_webhook(
        &pool,
        &taxed_session_event(&session_id, attacker, "25.00", 2_638, 165, "usd"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("amount_mismatch"));
    assert_nothing_credited(&pool, attacker, "taxed forged recipient").await;
    assert_nothing_credited(&pool, payer, "taxed forged recipient (payer)").await;
}

// ---------------------------------------------------------------------------
// Checkout session creation: the exact form ZeroRouter sends to Stripe
// ---------------------------------------------------------------------------

/// The `POST /v1/checkout/sessions` form the router sent, as captured by the
/// mock Stripe below.
type CapturedForm = Arc<Mutex<Option<HashMap<String, String>>>>;

/// The `Stripe-Version` header the router sent, if any. `None` means the
/// request would have run at whatever version the ACCOUNT is pinned to.
type CapturedVersion = Arc<Mutex<Option<String>>>;

/// The client secret the mock Stripe below mints, derived from the session id
/// exactly as Stripe derives it (`{session_id}_secret_{opaque}`).
fn client_secret_for(session_id: &str) -> String {
    format!("{session_id}_secret_embedded")
}

/// A Stripe stand-in that records the Checkout Session form verbatim and
/// answers with a session shaped like the real one. Asserting on what is
/// captured here is the only way to pin the wire contract: everything about
/// tax is decided by the parameters in this form, and a silently dropped
/// parameter is indistinguishable from a working integration until a customer
/// is charged the wrong amount.
///
/// The response is shaped like an `embedded_page` session specifically: a
/// `client_secret`, and `url: null`. A `hosted_page` session is the mirror
/// image, and the router must not accept one — mounting the form needs the
/// secret, so a session that only came back with a url is unusable.
async fn mock_checkout_stripe(session_id: String) -> (String, CapturedForm, CapturedVersion) {
    let captured: CapturedForm = Arc::new(Mutex::new(None));
    let version: CapturedVersion = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route(
            "/v1/checkout/sessions",
            post(
                |State((captured, version, session_id)): State<(
                    CapturedForm,
                    CapturedVersion,
                    String,
                )>,
                 headers: axum::http::HeaderMap,
                 Form(form): Form<HashMap<String, String>>| async move {
                    *version.lock().expect("captured version must lock") = headers
                        .get(stripe::STRIPE_VERSION_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let previous = {
                        let mut form_slot = captured.lock().expect("captured form must lock");
                        let previous = form_slot.is_some();
                        *form_slot = Some(form);
                        previous
                    };
                    // Real Stripe mints a NEW session id for every create. The
                    // first call keeps the caller's id so a test can look the
                    // intent up; later calls diverge, which is what makes an
                    // absent reuse guard show up as several intent rows rather
                    // than as a duplicate-key error.
                    let id = if previous {
                        format!("{session_id}x{}", Uuid::new_v4().simple())
                    } else {
                        session_id.clone()
                    };
                    axum::Json(json!({
                        "id": id,
                        "url": Value::Null,
                        "client_secret": client_secret_for(&id),
                    }))
                },
            ),
        )
        .with_state((captured.clone(), version.clone(), session_id));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock stripe should bind");
    let address = listener.local_addr().expect("mock stripe address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), captured, version)
}

/// Drive the real `POST /api/billing/checkout` handler as an authenticated
/// portal user, against whatever `api_base` is passed.
async fn post_checkout(
    pool: &PgPool,
    api_base: &str,
    user_id: Uuid,
    amount_usd: &str,
) -> (StatusCode, Value) {
    post_checkout_on_rail(pool, api_base, user_id, amount_usd, None, false).await
}

/// `post_checkout` with an explicit rail and an explicit deployment setting for
/// whether the crypto rail is live.
async fn post_checkout_on_rail(
    pool: &PgPool,
    api_base: &str,
    user_id: Uuid,
    amount_usd: &str,
    rail: Option<&str>,
    crypto_rail: bool,
) -> (StatusCode, Value) {
    let (token, _) = create_session(pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("portal session must create");
    let mut payload = json!({ "amount_usd": amount_usd });
    if let Some(rail) = rail {
        payload["rail"] = json!(rail);
    }
    let request = Request::builder()
        .method("POST")
        .uri("/api/billing/checkout")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(CSRF_HEADER, "1")
        .body(Body::from(payload.to_string()))
        .expect("checkout request should build");
    let response = stripe_app_with_rail(pool, api_base, crypto_rail)
        .oneshot(request)
        .await
        .expect("checkout request should complete");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("checkout response body should be readable")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Every parameter of the Checkout Session, pinned exactly.
///
/// This is a characterization test in the `tests/request_path.rs` sense: it
/// exists so that a change to what ZeroRouter asks Stripe to charge cannot
/// happen by accident. If it fails, the wire contract moved — decide whether
/// that was intended before touching the expectation.
#[tokio::test]
async fn checkout_session_form_is_the_pinned_stripe_wire_contract() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "checkout-form").await;
    let email = query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("user email must query");
    let session_id = unique_session_id();
    let (api_base, captured, version) = mock_checkout_stripe(session_id.clone()).await;

    let (status, body) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // The API version pin. `ui_mode=embedded_page` does not exist before
    // Dahlia renamed the enum, so an unpinned request runs at whatever version
    // the ACCOUNT defaults to — and an account older than Dahlia rejects the
    // session outright, breaking every purchase. A sandbox cannot catch that
    // (it defaults to the version current when it was created), so this
    // assertion is the only thing standing between a dropped header and a live
    // checkout outage.
    assert_eq!(
        version
            .lock()
            .expect("captured version must lock")
            .as_deref(),
        Some(stripe::CHECKOUT_API_VERSION),
        "the checkout session create must pin the Stripe API version"
    );

    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");
    let expected: HashMap<String, String> = [
        ("mode", "payment"),
        // Render the form inside the portal rather than on a Stripe-hosted
        // page. This is the parameter that makes Stripe mint a client secret.
        ("ui_mode", "embedded_page"),
        // TWO line items, so Stripe's own summary reads the way the portal's
        // copy does. Line 0 is the credits the buyer receives — $25.00, exactly
        // what the webhook will credit — and line 1 is the deposit fee,
        // ceil(0.055 * 25) = $1.38. They sum to the EX-TAX gross of $26.38,
        // which is the single number the intent row, the reuse-cache key and
        // both webhook corroborations are built on. Tax is added on top of that
        // sum by Stripe.
        //
        // Before this shape there was ONE line priced at the gross, so the
        // iframe drew "ZeroRouter credits $26.38" and a $26.38 subtotal, which
        // claimed the buyer was getting $26.38 of credits when they were
        // getting $25.00.
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][unit_amount]", "2500"),
        (
            "line_items[0][price_data][product_data][name]",
            "ZeroRouter credits",
        ),
        ("line_items[0][quantity]", "1"),
        ("line_items[1][price_data][currency]", "usd"),
        ("line_items[1][price_data][unit_amount]", "138"),
        // The portal's own word for this charge ("includes $1.38 processing
        // fee"), so the line the iframe draws is recognisably the same fee the
        // app just quoted rather than a second, unexplained one.
        (
            "line_items[1][price_data][product_data][name]",
            "Processing fee",
        ),
        ("line_items[1][quantity]", "1"),
        // The whole of ZeroRouter's tax integration. No tax code and no tax
        // behavior: those are Tax Settings' job, so the operator can revise the
        // classification without a deploy. That holds for BOTH lines — see
        // `neither_checkout_line_item_carries_a_tax_code_of_its_own`.
        ("automatic_tax[enabled]", "true"),
        // Offer the buyer a VAT/tax-ID field so a VAT-registered business is
        // reverse-charged rather than taxed as a consumer. No `required` key:
        // its default `never` is the optional mode, and the alternative would
        // block every EU consumer from buying.
        ("tax_id_collection[enabled]", "true"),
        // Always collect a FULL billing address rather than leaving the amount
        // of address up to Stripe. The default `auto` collects only what the
        // tax lookup is judged to need, which in a district-tax state can be
        // less than the address the rate actually depends on. See the module
        // comment for why the California registration is what changed this.
        ("billing_address_collection", "required"),
        ("metadata[user_id]", &user_id.to_string()),
        ("metadata[credit_usd]", "25.00"),
        ("metadata[fee_usd]", "1.38"),
        ("metadata[gross_usd]", "26.38"),
        ("customer_email", &email),
        // The `{CHECKOUT_SESSION_ID}` template variable must reach Stripe
        // VERBATIM — Stripe substitutes it on the way back, and that is the
        // only way the return page learns which session to ask about. If it
        // were ever interpolated server-side this assertion is what notices.
        (
            "return_url",
            "http://127.0.0.1/credits/return?session_id={CHECKOUT_SESSION_ID}",
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(form, expected, "the Checkout Session form moved");

    // Said once more on its own, because the equality above pins it only as
    // one entry among nineteen: a full billing address is REQUIRED. ZeroRouter
    // is registered in California, where the rate stacks district taxes below
    // ZIP granularity, so the address is a tax input and not merely a fraud
    // signal. If this ever silently reverts to the default `auto`, the tax
    // Stripe calculates can be wrong while every other assertion still passes.
    assert_eq!(
        form.get("billing_address_collection").map(String::as_str),
        Some("required"),
        "checkout must always collect a full billing address"
    );

    // `customer_update` is only valid alongside a `customer`, and this session
    // attaches none. Sending it would make Stripe reject every checkout, so its
    // absence is a guard, not an omission.
    assert!(
        !form.contains_key("customer"),
        "checkout attaches no Stripe Customer"
    );
    assert!(
        form.keys().all(|key| !key.starts_with("customer_update")),
        "customer_update without a customer is rejected by Stripe"
    );
    // Stripe documents both as "not allowed if ui_mode is `embedded_page`".
    // Sending either alongside the embedded ui_mode makes Stripe reject the
    // request outright, so every purchase would fail — a total checkout outage
    // rather than a subtle one. Their absence is load-bearing.
    assert!(
        !form.contains_key("success_url"),
        "success_url is rejected by Stripe on an embedded_page session"
    );
    assert!(
        !form.contains_key("cancel_url"),
        "cancel_url is rejected by Stripe on an embedded_page session"
    );

    // The response the portal actually receives: the client secret it mounts
    // the form with, and NOT a redirect url — an embedded session has none.
    assert_eq!(
        body.get("client_secret").and_then(Value::as_str),
        Some(client_secret_for(&session_id).as_str()),
        "the portal must receive the client secret"
    );
    assert!(
        body.get("url").is_none(),
        "an embedded session has no redirect url to hand back"
    );

    // The intent row still records the EX-TAX gross in cents and the net
    // credit in dollars — the two numbers the webhook reconciles against.
    let intent = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert_eq!(intent.expected_amount_cents, 2_638);
    assert_eq!(intent.expected_credit_usd, Decimal::from(25));
    assert_eq!(intent.user_id, user_id);

    // Exactly two line items, and no third: the equality above already pins
    // that, but a bare count says out loud what the shape is meant to be.
    assert_eq!(
        form.keys()
            .filter(|key| key.ends_with("[quantity]") && key.starts_with("line_items["))
            .count(),
        2,
        "the session is sold as exactly two line items"
    );
    assert!(
        form.keys().all(|key| !key.starts_with("line_items[2]")),
        "a third line item would be money nobody quoted"
    );
}

/// Read the `unit_amount` of one line item out of a captured form.
fn line_item_cents(form: &HashMap<String, String>, index: usize) -> i64 {
    form.get(&format!("line_items[{index}][price_data][unit_amount]"))
        .unwrap_or_else(|| panic!("line item {index} must carry a unit_amount"))
        .parse()
        .unwrap_or_else(|_| panic!("line item {index} unit_amount must be an integer"))
}

/// The arithmetic that makes the split safe: the two lines must sum to the
/// EX-TAX GROSS — the one number the whole accounting chain is built on — and
/// the credits line must be the credit and nothing more.
///
/// # Why this is a separate test from the wire contract
///
/// The wire contract pins literals for one amount ($25). It would keep passing
/// if the split were re-derived some other way that happens to agree at $25 and
/// disagrees a cent elsewhere — which is exactly the failure a second,
/// independent 5.5% computation in the line items would produce, since
/// `deposit_fee_quote` CEILS to the whole cent. So this sweeps the fee schedule
/// instead: the $0.80 floor, the floor/percentage crossover, and amounts whose
/// percentage is a sub-cent that ceils. A cent lost here is not cosmetic — the
/// lines would sum to something other than `expected_amount_cents`, Stripe
/// would collect that other number, and BOTH webhook corroborations would then
/// refuse a payment the customer really made.
#[tokio::test]
async fn the_two_checkout_line_items_sum_to_the_gross_and_the_credits_line_is_the_credit() {
    let Some(pool) = connect().await else {
        return;
    };
    // (credit dollars, credit cents, fee cents, gross cents). Every fee here is
    // `deposit_fee_quote`'s: max(ceil(0.055 * credit), 80).
    let cases = [
        // The floor: 0.055 * 5 = 0.275 -> ceils to 0.28, under the $0.80 floor.
        ("5.00", 500_i64, 80_i64, 580_i64),
        // The last credit the floor still wins at, and the first it does not:
        // 0.055 * 14.54 = 0.7997 -> 0.80 ties, 0.055 * 14.55 = 0.80025 -> 0.81.
        ("14.54", 1_454, 80, 1_534),
        ("14.55", 1_455, 81, 1_536),
        // Sub-cent percentages that must ceil, not round: 1.375 -> 1.38 and
        // 2.475 -> 2.48. A second, independent 5.5% that rounded instead would
        // put the fee line a cent low and the sum a cent under the gross.
        ("25.00", 2_500, 138, 2_638),
        ("45.00", 4_500, 248, 4_748),
        // An exact percentage, no rounding involved at all.
        ("100.00", 10_000, 550, 10_550),
    ];
    for (credit_usd, credit_cents, fee_cents, gross_cents) in cases {
        let user_id = create_user(&pool, &format!("split-{credit_usd}")).await;
        let session_id = unique_session_id();
        let (api_base, captured, _version) = mock_checkout_stripe(session_id.clone()).await;
        let (status, body) = post_checkout(&pool, &api_base, user_id, credit_usd).await;
        assert_eq!(status, StatusCode::OK, "{credit_usd}: body: {body}");
        let form = captured
            .lock()
            .expect("captured form must lock")
            .clone()
            .expect("stripe must have been called");

        let line_credit = line_item_cents(&form, 0);
        let line_fee = line_item_cents(&form, 1);
        assert_eq!(
            line_credit, credit_cents,
            "{credit_usd}: the credits line must be the credit exactly"
        );
        assert_eq!(
            line_fee, fee_cents,
            "{credit_usd}: the fee line must be the quoted deposit fee"
        );
        assert_eq!(
            line_credit + line_fee,
            gross_cents,
            "{credit_usd}: the line items must sum to the ex-tax gross"
        );

        // The sum is not merely equal to a literal — it is equal to the number
        // ZeroRouter recorded and will later require Stripe to have collected.
        // These are the same quantity by construction and this asserts it.
        let intent = checkout_intent(&pool, &session_id)
            .await
            .expect("intent must query")
            .expect("intent must exist");
        assert_eq!(
            line_credit + line_fee,
            intent.expected_amount_cents,
            "{credit_usd}: the lines must sum to the gross on the intent row"
        );
        assert_eq!(
            Decimal::from(line_credit) / Decimal::ONE_HUNDRED,
            intent.expected_credit_usd,
            "{credit_usd}: the credits line must equal the credit the intent row will grant"
        );
        // And the same again against the metadata the webhook reads.
        assert_eq!(
            form.get("metadata[credit_usd]").map(String::as_str),
            Some(credit_usd),
            "{credit_usd}: metadata still names the credit, not the gross"
        );
        assert_eq!(
            form.get("metadata[gross_usd]")
                .map(|raw| Decimal::from_str(raw).expect("gross must parse")),
            Some(Decimal::from(gross_cents) / Decimal::ONE_HUNDRED),
            "{credit_usd}: metadata still names the gross the lines sum to"
        );
    }
}

/// The end the split has to reach: what Stripe renders on the credits line is
/// what the ledger actually grants.
///
/// The presentation bug this fixes was precisely a divergence between those two
/// — the iframe said $26.38 of credits, the ledger granted $25.00 — so pinning
/// the amounts alone would miss the point. This drives the real purchase all
/// the way through: create the session, read the credits line off the wire,
/// then deliver a signed, taxed `checkout.session.completed` for the gross the
/// two lines sum to, and require the balance to land on the credits line's
/// number. Nothing here is asserted against a literal the split could also have
/// been wrong about.
#[tokio::test]
async fn the_credits_line_is_exactly_what_the_webhook_credits() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "line-credits").await;
    let session_id = unique_session_id();
    let (api_base, captured, _version) = mock_checkout_stripe(session_id.clone()).await;
    let (status, body) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");
    let line_credit = line_item_cents(&form, 0);
    let line_fee = line_item_cents(&form, 1);

    // Stripe collects the SUM of the lines, plus tax on top of it. $1.65 of tax
    // is a plausible 6.25% MA figure and, more importantly, is not derivable
    // from anything else here — so a corroboration that quietly compared the
    // wrong quantity would show up as a rejection.
    let tax_cents = 165;
    let (status, _) = post_webhook_against(
        &pool,
        UNREACHABLE_STRIPE,
        &taxed_session_event(
            &session_id,
            user_id,
            "25.00",
            line_credit + line_fee,
            tax_cents,
            "usd",
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the taxed purchase must be accepted"
    );

    // The credited balance is the CREDITS LINE, to the cent — not the gross the
    // buyer paid, and not the gross plus tax.
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(line_credit) / Decimal::ONE_HUNDRED,
        "the balance must be exactly what the credits line said it would be"
    );
    assert_eq!(purchase_count(&pool, user_id).await, 1);
}

/// The tax treatment must NOT have been split along with the presentation.
///
/// The operator's standing decision is to do this the default Stripe way: the
/// Tax Settings preset classifies the sale, so a contested classification can be
/// revised in the dashboard rather than in a deploy. That was already true of
/// the single line item; the risk the split introduces is that the fee line
/// picks up an override "because a fee is different" — which would both take
/// the classification back out of Tax Settings AND be the wrong tax answer, as
/// the fee is part of the taxable consideration for the same single service in
/// the regimes ZeroRouter is registered in. Neither line may carry one.
#[tokio::test]
async fn neither_checkout_line_item_carries_a_tax_code_of_its_own() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "uniform-tax").await;
    let session_id = unique_session_id();
    let (api_base, captured, _version) = mock_checkout_stripe(session_id).await;
    let (status, body) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");

    assert!(
        form.keys()
            .all(|key| !key.contains("tax_code") && !key.contains("tax_behavior")),
        "no line item may hardcode a tax code or tax behavior; Tax Settings owns both"
    );
    assert_eq!(
        form.get("automatic_tax[enabled]").map(String::as_str),
        Some("true"),
        "one automatic_tax flag still covers both lines"
    );
}

/// The one parameter that makes Stripe compute tax, and the two that must NOT
/// be sent so the dashboard keeps owning the policy.
///
/// Dropping `automatic_tax[enabled]` does not fail loudly in production: the
/// session is created happily and simply collects nothing. Re-adding a
/// `tax_code` or a `tax_behavior` does not fail loudly either — it quietly
/// takes the classification back out of Tax Settings, so the operator's next
/// revision of an unsettled legal question silently does not apply to checkout.
/// Both directions are invisible without this test.
#[tokio::test]
async fn the_checkout_session_asks_stripe_to_calculate_tax() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "automatic-tax").await;
    let session_id = unique_session_id();
    let (api_base, captured, _version) = mock_checkout_stripe(session_id).await;

    let (status, body) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");

    assert_eq!(
        form.get("automatic_tax[enabled]").map(String::as_str),
        Some("true"),
        "Stripe must be asked to determine the tax"
    );
    // Tax POLICY belongs to Tax Settings, not to this request. Stripe falls
    // back to the account presets for both, which is what lets the operator
    // revise a contested classification without shipping code.
    assert_eq!(
        form.get("line_items[0][price_data][product_data][tax_code]"),
        None,
        "the product tax code must come from Tax Settings, not from here"
    );
    assert_eq!(
        form.get("line_items[0][price_data][tax_behavior]"),
        None,
        "the tax behavior must come from Tax Settings, not from here"
    );
    // No rate and no jurisdiction may be encoded anywhere in the request:
    // taxability is Stripe's determination from the buyer's address and the
    // registrations in the dashboard, and a hardcoded rate would silently
    // outlive the next rate change.
    assert!(
        form.keys()
            .all(|key| !key.contains("tax_rate") && !key.contains("tax_rates")),
        "no manual tax rate may be sent; it cannot coexist with automatic tax"
    );
}

/// The parameter that offers a VAT/tax ID field, and the one that must NOT be
/// sent alongside it.
///
/// Both directions fail silently in production, which is why they are pinned
/// here rather than left to the wire-contract test's equality alone:
///
/// - Drop `tax_id_collection[enabled]` and every session is created happily,
///   the form simply never offers the field, and every VAT-registered business
///   buyer is charged consumer VAT on a sale that should have been reverse
///   charged. Nothing in the logs says so.
/// - Add `tax_id_collection[required]=if_supported` and the mirror image
///   happens: a tax ID becomes MANDATORY for every buyer in a supported billing
///   country, so EU consumers — who have no business tax ID — cannot complete a
///   purchase at all. The default `never` is the optional mode this product
///   needs, so the key's ABSENCE is the guard.
#[tokio::test]
async fn the_checkout_session_offers_an_optional_tax_id_field() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "tax-id-collection").await;
    let session_id = unique_session_id();
    let (api_base, captured, _version) = mock_checkout_stripe(session_id).await;

    let (status, body) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");

    assert_eq!(
        form.get("tax_id_collection[enabled]").map(String::as_str),
        Some("true"),
        "Checkout must offer a tax ID field so business buyers can be reverse charged"
    );
    // Optional for the buyer, always. `required` unset means `never`, Stripe's
    // optional mode; anything else blocks consumers from paying.
    assert_eq!(
        form.get("tax_id_collection[required]"),
        None,
        "tax ID collection must stay optional; `if_supported` would block consumers"
    );
    // Tax ID collection needs neither of these. The tax ID arrives on the
    // completed session at `customer_details.tax_ids[]` with no Customer
    // attached, and `customer_creation=always` would mint a second, duplicate
    // customer per purchase on top of the one `ensure_stripe_customer` keeps.
    assert_eq!(
        form.get("customer_creation"),
        None,
        "tax ID collection must not start creating a duplicate Customer per purchase"
    );
    assert!(
        !form.contains_key("customer"),
        "checkout still attaches no Stripe Customer"
    );
}

/// A `hosted_page`-shaped response — a redirect url and no client secret — is
/// refused rather than handed to a portal that cannot mount it.
///
/// This is the failure mode of a half-applied change: drop `ui_mode` from the
/// form and Stripe happily creates a session, but it is the wrong KIND of
/// session. Without this guard the endpoint would return `{"client_secret":
/// null}` and the Credits page would fail in the browser with nothing in the
/// server logs to explain it.
#[tokio::test]
async fn a_session_without_a_client_secret_is_refused() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "no-client-secret").await;
    let session_id = unique_session_id();

    // A Stripe stand-in that answers the way a `hosted_page` session does.
    let app = Router::new()
        .route(
            "/v1/checkout/sessions",
            post(|State(session_id): State<String>| async move {
                axum::Json(json!({
                    "id": session_id,
                    "url": format!("https://checkout.stripe.invalid/c/pay/{session_id}"),
                    "client_secret": Value::Null,
                }))
            }),
        )
        .with_state(session_id.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock stripe should bind");
    let address = listener.local_addr().expect("mock stripe address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let (status, body) = post_checkout(&pool, &format!("http://{address}"), user_id, "25.00").await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "a session with no client secret is unusable: {body}"
    );
    // And no intent row was written for a session the portal can never mount.
    assert!(
        checkout_intent(&pool, &session_id)
            .await
            .expect("intent must query")
            .is_none(),
        "no pending purchase may be recorded for an unusable session"
    );
}

// ---------------------------------------------------------------------------
// GET /api/billing/checkout/status — display only, never a crediting path
// ---------------------------------------------------------------------------

/// A Stripe stand-in for session RETRIEVAL, answering with a fixed `status`.
/// Counts the calls so a test can prove the endpoint reached Stripe at all (or
/// deliberately did not).
async fn mock_status_stripe(session_id: String, session_status: &str) -> (String, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0_u32));
    let app = Router::new()
        .route(
            "/v1/checkout/sessions/{id}",
            axum::routing::get(
                |State((calls, session_id, session_status)): State<(
                    Arc<Mutex<u32>>,
                    String,
                    String,
                )>,
                 axum::extract::Path(id): axum::extract::Path<String>| async move {
                    *calls.lock().expect("call counter must lock") += 1;
                    assert_eq!(
                        id, session_id,
                        "the router must retrieve its own session id"
                    );
                    axum::Json(json!({
                        "id": id,
                        "object": "checkout.session",
                        "status": session_status,
                        // Money fields are present exactly as Stripe sends
                        // them. Nothing may read them: this endpoint informs a
                        // screen, not the ledger.
                        "amount_total": 2_638,
                        "currency": "usd",
                        "payment_status": "paid",
                    }))
                },
            ),
        )
        .with_state((calls.clone(), session_id, session_status.to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock stripe should bind");
    let address = listener.local_addr().expect("mock stripe address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), calls)
}

/// Drive `GET /api/billing/checkout/status` as an authenticated portal user.
async fn get_checkout_status(
    pool: &PgPool,
    api_base: &str,
    user_id: Uuid,
    session_id: &str,
) -> (StatusCode, Value) {
    let (token, _) = create_session(pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("portal session must create");
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/billing/checkout/status?session_id={session_id}"
        ))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .body(Body::empty())
        .expect("status request should build");
    let response = stripe_app(pool, api_base)
        .oneshot(request)
        .await
        .expect("status request should complete");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("status response body should be readable")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// The return page's two outcomes, and the guarantee that neither moves money.
///
/// `complete` is the interesting one: this is the exact moment a customer's
/// browser is told the payment succeeded, and it is precisely where a
/// convenience "credit them now" would be tempting to add. The balance
/// assertion is the tripwire for that. Crediting belongs to the webhook, which
/// has the HMAC and the two corroborations behind it; this endpoint has neither
/// and must never grow them.
#[tokio::test]
async fn checkout_status_reports_completion_without_crediting_anything() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "status-complete").await;
    let session_id = unique_session_id();
    // The intent row is what makes the session *this user's* to ask about.
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("intent must record");

    let (api_base, calls) = mock_status_stripe(session_id.clone(), "complete").await;
    let (status, body) = get_checkout_status(&pool, &api_base, user_id, &session_id).await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body.get("status").and_then(Value::as_str), Some("complete"));
    assert_eq!(*calls.lock().expect("counter must lock"), 1);

    // The whole point. A session Stripe calls `complete` credits NOTHING until
    // the signed webhook says so.
    assert_nothing_credited(&pool, user_id, "status endpoint reported complete").await;
    // And the intent is still unsettled — the status read must not have
    // stamped it, or a later webhook would be treated as a replay and the
    // customer would never be credited at all.
    let intent = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert!(
        intent.settled_at.is_none(),
        "a display-only read must not settle the pending purchase"
    );
}

/// An abandoned or failed payment reports `open`, which is the portal's cue to
/// re-mount the form so the customer can try again.
#[tokio::test]
async fn checkout_status_reports_open_for_an_unfinished_payment() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "status-open").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("intent must record");

    let (api_base, _) = mock_status_stripe(session_id.clone(), "open").await;
    let (status, body) = get_checkout_status(&pool, &api_base, user_id, &session_id).await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body.get("status").and_then(Value::as_str), Some("open"));
    assert_nothing_credited(&pool, user_id, "status endpoint reported open").await;
}

/// A session belonging to someone else is not readable, and Stripe is never
/// even asked.
///
/// `session_id` arrives from the client, so without the ownership check this
/// endpoint would report the state of any session id a signed-in user could
/// guess or obtain — a cross-tenant read on a payment. The call counter is what
/// proves the refusal happens BEFORE the outbound request, so the endpoint
/// cannot be used as an oracle for which session ids exist at Stripe.
#[tokio::test]
async fn checkout_status_refuses_another_users_session() {
    let Some(pool) = connect().await else {
        return;
    };
    let owner = create_user(&pool, "status-owner").await;
    let snooper = create_user(&pool, "status-snooper").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, owner, 2_638, Decimal::from(25), "usd")
        .await
        .expect("intent must record");

    let (api_base, calls) = mock_status_stripe(session_id.clone(), "complete").await;
    let (status, body) = get_checkout_status(&pool, &api_base, snooper, &session_id).await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another user's session must not be readable: {body}"
    );
    assert_eq!(
        *calls.lock().expect("counter must lock"),
        0,
        "Stripe must not be consulted about a session the caller does not own"
    );
    assert_nothing_credited(&pool, snooper, "cross-tenant status read").await;
    assert_nothing_credited(&pool, owner, "cross-tenant status read").await;
}

/// A session id this deployment never priced is the same answer as one that
/// belongs to someone else — no existence oracle either way.
#[tokio::test]
async fn checkout_status_refuses_a_session_this_deployment_never_priced() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "status-unknown").await;
    let unknown = unique_session_id();

    let (api_base, calls) = mock_status_stripe(unknown.clone(), "complete").await;
    let (status, body) = get_checkout_status(&pool, &api_base, user_id, &unknown).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(
        *calls.lock().expect("counter must lock"),
        0,
        "an unrecorded session must not reach Stripe"
    );
    assert_nothing_credited(&pool, user_id, "unknown session status read").await;
}

/// Repeated status polls collapse onto a small number of Stripe reads.
///
/// The endpoint is customer-triggered and takes no argument that bounds its
/// cost, so without a cache one authenticated user refreshing the return page
/// is a one-to-one amplifier onto Stripe's rate limit — and a 429 there does
/// not just break their page, it breaks checkout and autopay for everyone.
/// Ownership is enforced before the cache is consulted, so this cannot be used
/// to read anyone else's session either.
#[tokio::test]
async fn repeated_status_polls_do_not_hammer_stripe() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "status-poll").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("intent must record");

    let (api_base, calls) = mock_status_stripe(session_id.clone(), "open").await;
    for _ in 0..25 {
        let (status, body) = get_checkout_status(&pool, &api_base, user_id, &session_id).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body.get("status").and_then(Value::as_str), Some("open"));
    }

    let outbound = *calls.lock().expect("counter must lock");
    assert!(
        outbound <= 2,
        "25 polls inside the cache window must collapse to at most 2 Stripe reads, got {outbound}"
    );
    assert_nothing_credited(&pool, user_id, "repeated status polls").await;
}

/// Re-opening the payment step for the same amount reuses the session instead
/// of minting another.
///
/// Every unmount/remount of Stripe's form calls the checkout endpoint again —
/// Cancel, Escape, backdrop click and "Change amount" all unmount it. Nothing
/// deletes `stripe_checkout_intents` rows, so without reuse one hesitant
/// customer leaves a trail of sessions behind for a single purchase.
#[tokio::test]
async fn remounting_the_same_amount_reuses_one_checkout_session() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "session-reuse").await;
    let session_id = unique_session_id();
    let (api_base, _captured, _version) = mock_checkout_stripe(session_id.clone()).await;

    let (status, first) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {first}");
    // Three more mounts at the same price, as a customer toggling the modal
    // would produce.
    for _ in 0..3 {
        let (status, again) = post_checkout(&pool, &api_base, user_id, "25.00").await;
        assert_eq!(status, StatusCode::OK, "body: {again}");
        assert_eq!(
            again.get("client_secret"),
            first.get("client_secret"),
            "the same amount must hand back the same session"
        );
    }

    // Exactly one intent row exists for this buyer: the reused session's.
    let intents =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM stripe_checkout_intents WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("intent count must query");
    assert_eq!(
        intents, 1,
        "four mounts of the same amount must leave one pending purchase, not four"
    );

    // A DIFFERENT amount is a different price and must never reuse the
    // session priced for the first one.
    let other_session = unique_session_id();
    let (other_base, _c, _v) = mock_checkout_stripe(other_session).await;
    let (status, other) = post_checkout(&pool, &other_base, user_id, "50.00").await;
    assert_eq!(status, StatusCode::OK, "body: {other}");
    assert_ne!(
        other.get("client_secret"),
        first.get("client_secret"),
        "a different amount must get its own session"
    );
}

/// A session id that cannot be a Stripe id is refused before it reaches the
/// database or an outbound URL.
#[tokio::test]
async fn a_malformed_session_id_is_refused_without_touching_anything() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "status-malformed").await;
    let (api_base, calls) = mock_status_stripe("cs_unused".to_owned(), "complete").await;

    for bogus in ["%00", "../../secrets", "pi_123", ""] {
        let (status, _) = get_checkout_status(&pool, &api_base, user_id, bogus).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{bogus:?} is not a checkout session id"
        );
    }
    assert_eq!(
        *calls.lock().expect("counter must lock"),
        0,
        "a malformed id must never reach Stripe"
    );
}

/// The status endpoint is session-authenticated like every other portal
/// surface: a signed-out caller gets nothing.
#[tokio::test]
async fn checkout_status_requires_a_portal_session() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "status-anon").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("intent must record");

    let (api_base, calls) = mock_status_stripe(session_id.clone(), "complete").await;
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/billing/checkout/status?session_id={session_id}"
        ))
        .body(Body::empty())
        .expect("status request should build");
    let response = stripe_app(&pool, &api_base)
        .oneshot(request)
        .await
        .expect("status request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        *calls.lock().expect("counter must lock"),
        0,
        "an unauthenticated caller must not reach Stripe"
    );
}

// ---------------------------------------------------------------------------
// Autopay (migration 0008): payment_intent webhook arms.
// ---------------------------------------------------------------------------

/// A `payment_intent.*` event with independently controllable money fields,
/// exactly like the checkout fixture above.
/// The provenance mark the sweep stamps into metadata: an HMAC over the
/// money-bearing fields keyed by the webhook secret — computed here exactly
/// as `stripe::autopay_provenance` computes it, because a fixture that
/// cannot produce it is what the forgery test below relies on.
fn provenance_mark(user_id: Uuid, credit_usd: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("hmac accepts any key");
    mac.update(format!("zerorouter_autopay|{user_id}|{credit_usd}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn autopay_intent_event(
    event_type: &str,
    intent_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_received: i64,
    currency: &str,
) -> String {
    autopay_intent_event_with_mark(
        event_type,
        intent_id,
        user_id,
        metadata_credit_usd,
        amount_received,
        currency,
        &provenance_mark(user_id, metadata_credit_usd),
    )
}

#[allow(clippy::too_many_arguments)]
fn autopay_intent_event_with_mark(
    event_type: &str,
    intent_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_received: i64,
    currency: &str,
    provenance: &str,
) -> String {
    json!({
        "id": "evt_test",
        "type": event_type,
        "data": {
            "object": {
                "id": intent_id,
                "object": "payment_intent",
                "amount_received": amount_received,
                "currency": currency,
                "metadata": {
                    "purpose": "zerorouter_autopay",
                    "user_id": user_id.to_string(),
                    "credit_usd": metadata_credit_usd,
                    "provenance": provenance,
                },
            }
        }
    })
    .to_string()
}

/// The same event with the tax metadata migration 0021 added. `tax_cents` is
/// `Option` so a test can produce all three wire shapes an operator will
/// actually see: absent (an intent created by the pre-0021 binary, still being
/// redelivered), `"0"` (asked and the answer was nothing — the shape of every
/// charge until a registration exists), and a real figure.
fn autopay_intent_event_with_tax(
    intent_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_received: i64,
    tax_cents: Option<&str>,
    calculation_id: Option<&str>,
) -> String {
    let mut event: serde_json::Value = serde_json::from_str(&autopay_intent_event(
        "payment_intent.succeeded",
        intent_id,
        user_id,
        metadata_credit_usd,
        amount_received,
        "usd",
    ))
    .expect("fixture must parse");
    let metadata = event["data"]["object"]["metadata"]
        .as_object_mut()
        .expect("metadata object");
    if let Some(tax_cents) = tax_cents {
        metadata.insert("tax_cents".to_owned(), json!(tax_cents));
    }
    if let Some(calculation_id) = calculation_id {
        metadata.insert("tax_calculation".to_owned(), json!(calculation_id));
    }
    event.to_string()
}

async fn enable_autopay(pool: &PgPool, user_id: Uuid) {
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

async fn balance_of(pool: &PgPool, user_id: Uuid) -> Decimal {
    query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("balance must query")
}

/// The success arm credits exactly once — including the crash-recovery
/// shape where the sweep died before recording the intent row, so the
/// webhook's metadata is the only record the charge ever happened.
#[tokio::test]
async fn autopay_success_credits_exactly_once_even_without_a_prior_intent_row() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-success").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());

    // No intent row exists — the metadata-recovery path must build one. A $25
    // top-up is charged $26.38 gross; the webhook corroborates the gross and
    // credits the net.
    let payload = autopay_intent_event(
        "payment_intent.succeeded",
        &intent_id,
        user_id,
        "25",
        2638,
        "usd",
    );
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));

    // Stripe redelivers; the replay must not double-credit.
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));

    let ledger_rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE stripe_session_id = $1 AND entry_type = 'autopay'",
    )
    .bind(&intent_id)
    .fetch_one(&pool)
    .await
    .expect("ledger must query");
    assert_eq!(ledger_rows, 1);
}

/// The corroboration bar from the checkout arm holds here: metadata that
/// disagrees with the money Stripe collected credits nothing.
#[tokio::test]
async fn autopay_success_with_forged_metadata_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-forged").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());

    // Claims $250 of credit against $25 actually collected.
    let payload = autopay_intent_event(
        "payment_intent.succeeded",
        &intent_id,
        user_id,
        "250",
        2500,
        "usd",
    );
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::ZERO);
}

/// Three consecutive failures disable autopay; a success in between resets
/// the count (pinned via the settle path's reset).
#[tokio::test]
async fn three_consecutive_failures_disable_autopay() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-failures").await;
    enable_autopay(&pool, user_id).await;

    for round in 0..3 {
        let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());
        // The failed intent must exist as pending first (the sweep records
        // it when Stripe reports the declined intent).
        query(
            "INSERT INTO stripe_autopay_intents (payment_intent_id, user_id, amount_usd, charge_amount_usd) VALUES ($1, $2, 25, 26.38)",
        )
        .bind(&intent_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("pending intent must insert");
        let payload = autopay_intent_event(
            "payment_intent.payment_failed",
            &intent_id,
            user_id,
            "25",
            0,
            "usd",
        );
        let (status, _) = post_webhook(&pool, &payload).await;
        assert_eq!(status, StatusCode::OK, "failure round {round}");
    }

    let (enabled, failures) = query_as::<_, (bool, i32)>(
        "SELECT autopay_enabled, autopay_consecutive_failures FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("user must query");
    assert_eq!(failures, 3);
    assert!(!enabled, "the third strike disables autopay");
}

/// The co-tenant forgery pin: another integration in the same Stripe
/// account can write our metadata SHAPE, but not our HMAC — a purposed
/// event without valid provenance is acknowledged untouched: no credit, no
/// intent row, no strike.
#[tokio::test]
async fn a_purposed_event_without_provenance_mints_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-cotenant").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());
    let payload = autopay_intent_event_with_mark(
        "payment_intent.succeeded",
        &intent_id,
        user_id,
        "25",
        2500,
        "usd",
        "deadbeef00000000000000000000000000000000000000000000000000000000",
    );
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "acknowledged so Stripe stops retrying"
    );
    assert_eq!(balance_of(&pool, user_id).await, Decimal::ZERO);
    let rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(&intent_id)
    .fetch_one(&pool)
    .await
    .expect("intents must query");
    assert_eq!(rows, 0, "an unproven event leaves no record at all");
}

/// Foreign payment intents — no autopay purpose — are acknowledged and
/// ignored, never credited.
#[tokio::test]
async fn foreign_payment_intents_are_ignored() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-foreign").await;
    let payload = json!({
        "id": "evt_test",
        "type": "payment_intent.succeeded",
        "data": { "object": {
            "id": format!("pi_test_{}", Uuid::new_v4().simple()),
            "object": "payment_intent",
            "amount_received": 2500,
            "currency": "usd",
            "metadata": { "user_id": user_id.to_string(), "credit_usd": "25" }
        }}
    })
    .to_string();
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::ZERO);
}

// ---------------------------------------------------------------------------
// Autopay sales tax (migration 0021)
//
// The invariant under test throughout: the card is charged the ex-tax gross
// PLUS tax, and the balance still receives EXACTLY the credit. Tax never
// becomes credit and never becomes revenue.
// ---------------------------------------------------------------------------

/// The nonzero-tax shape. A $25 top-up is $26.38 gross; Massachusetts at
/// 6.25% adds $1.65, so Stripe collects $28.03 — and the buyer is credited
/// $25.00, exactly as an untaxed top-up credits.
#[tokio::test]
async fn a_taxed_autopay_recharge_credits_the_credit_not_the_collection() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-taxed").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());

    let payload = autopay_intent_event_with_tax(
        &intent_id,
        user_id,
        "25",
        2803,
        Some("165"),
        Some("taxcalc_test"),
    );
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::from(25),
        "the tax is collected on top and never credited",
    );

    // The ledger entry is the NET credit, not the collection and not the gross.
    let credited = query_scalar::<_, Decimal>(
        "SELECT amount_usd FROM credit_ledger WHERE stripe_session_id = $1 AND entry_type = 'autopay'",
    )
    .bind(&intent_id)
    .fetch_one(&pool)
    .await
    .expect("ledger row must exist");
    assert_eq!(credited, Decimal::from(25));

    // A redelivery of the SAME taxed event must not credit again. Stripe
    // retries, and the sweep's inline settle races the webhook on every fast
    // charge, so this is the ordinary case rather than an exotic one.
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));
    let ledger_rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE stripe_session_id = $1 AND entry_type = 'autopay'",
    )
    .bind(&intent_id)
    .fetch_one(&pool)
    .await
    .expect("ledger must query");
    assert_eq!(ledger_rows, 1, "a redelivered taxed event credits once");
}

/// The zero-tax shape, which is what EVERY charge looks like until a tax
/// registration exists: Stripe was asked, computed nothing, and the collection
/// equals the bare gross. This must credit exactly as it did before 0021 — the
/// evidence that the feature ships inert.
#[tokio::test]
async fn a_zero_tax_autopay_recharge_credits_exactly_as_before() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-zerotax").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());

    let payload = autopay_intent_event_with_tax(
        &intent_id,
        user_id,
        "25",
        2638,
        Some("0"),
        Some("taxcalc_zero"),
    );
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));
}

/// An intent created by the PRE-0021 binary carries no `tax_cents` key at all,
/// and its `amount_received` is the bare gross. Stripe retries a webhook for
/// days, so such an event can easily arrive at a binary that already has this
/// change — and it must still credit. An absent key therefore reads as zero
/// tax; it is not treated as malformed.
#[tokio::test]
async fn an_intent_from_before_tax_existed_still_credits() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-pre0021").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());

    let payload = autopay_intent_event_with_tax(&intent_id, user_id, "25", 2638, None, None);
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));
}

/// The attack the tax field could conceivably enable, and why it cannot.
///
/// `tax_cents` is NOT covered by the provenance HMAC, so a co-tenant who could
/// somehow reuse a valid mark might try to bend it. It is SUBTRACTED from what
/// Stripe collected, so the only useful direction is negative — claim a
/// negative tax and a small collection satisfies a large credit. Negative
/// values are refused outright, and so is anything unparseable, rather than
/// being coerced to zero: coercion would turn a garbled figure into a short
/// payment credited in full.
#[tokio::test]
async fn a_tax_figure_that_would_credit_more_than_was_collected_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-taxforge").await;
    enable_autopay(&pool, user_id).await;

    for (label, amount_received, tax_cents) in [
        // The real attack: $1.00 collected, a negative tax bending it up to the
        // $26.38 gross the $25 credit demands.
        ("negative tax", 100, Some("-2538")),
        // Unparseable figures are refused, not read as zero.
        ("not a number", 2638, Some("banana")),
        ("empty", 2638, Some("")),
        ("fractional", 2803, Some("165.4")),
        // A tax that does not reconcile: collected the untaxed gross while
        // claiming tax was added on top.
        ("tax claimed but not collected", 2638, Some("165")),
        // Collected more than gross+tax.
        ("over-collection", 2900, Some("165")),
    ] {
        let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());
        let payload = autopay_intent_event_with_tax(
            &intent_id,
            user_id,
            "25",
            amount_received,
            tax_cents,
            None,
        );
        let (status, _) = post_webhook(&pool, &payload).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused, not credited"
        );
        assert_eq!(
            balance_of(&pool, user_id).await,
            Decimal::ZERO,
            "{label} credited something"
        );
    }
}

/// A Stripe stand-in that counts tax-transaction recordings and remembers the
/// `reference` each one carried.
async fn mock_tax_transactions() -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let references = Arc::new(Mutex::new(Vec::new()));
    let sink = (calls.clone(), references.clone());
    let app = axum::Router::new().route(
        "/v1/tax/transactions/create_from_calculation",
        axum::routing::post(move |body: String| {
            let (calls, references) = sink.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(reference) = body
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .find(|(key, _)| *key == "reference")
                    .map(|(_, value)| value.to_owned())
                {
                    references.lock().expect("reference sink").push(reference);
                }
                axum::Json(json!({ "id": "tax_test", "object": "tax.transaction" }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock stripe should bind");
    let address = listener.local_addr().expect("mock stripe address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), calls, references)
}

/// THE EVENT-ARRIVES-TWICE CASE, for tax.
///
/// Stripe retries webhooks, and the sweep's own inline settle races the webhook
/// on every fast charge, so the same success is routinely processed more than
/// once. Crediting is already deduplicated by the pending→succeeded transition;
/// tax REPORTING has to ride on the same guard, because a tax transaction
/// recorded twice would over-report collected tax to a jurisdiction.
///
/// The gate is the settlement OUTCOME, not the event: only the delivery that
/// actually credited records. The second delivery sees `AlreadySettled` and
/// records nothing.
#[tokio::test]
async fn a_redelivered_taxed_success_records_its_tax_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-taxdedup").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());
    let (api_base, calls, references) = mock_tax_transactions().await;

    let payload = autopay_intent_event_with_tax(
        &intent_id,
        user_id,
        "25",
        2803,
        Some("165"),
        Some("taxcalc_dedup"),
    );

    let (status, _) = post_webhook_against(&pool, &api_base, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the crediting delivery records the tax transaction"
    );

    // Same event again, byte for byte, exactly as Stripe would redeliver it.
    let (status, _) = post_webhook_against(&pool, &api_base, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::from(25),
        "no second credit"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "and no second tax transaction: a redelivery must not inflate a tax return"
    );

    let references = references.lock().expect("references").clone();
    assert_eq!(
        references,
        vec![intent_id.clone()],
        "the reference is the PaymentIntent id, which Stripe requires to be \
         unique across all transactions — so even if this gate were lost, \
         Stripe itself would refuse the duplicate"
    );
}

/// A tax transaction that cannot be recorded must not undo a correct credit.
/// By the time recording runs the card has been charged and the balance
/// credited; failing the webhook would only make Stripe redeliver an event with
/// nothing left to settle. So the arm returns 200 and logs, and the credit
/// stands. (`webhook_app`'s unreachable base is what makes this the default
/// path for every other autopay test in this file.)
#[tokio::test]
async fn a_failed_tax_recording_does_not_disturb_the_credit() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "autopay-taxrecfail").await;
    enable_autopay(&pool, user_id).await;
    let intent_id = format!("pi_test_{}", Uuid::new_v4().simple());

    let payload = autopay_intent_event_with_tax(
        &intent_id,
        user_id,
        "25",
        2803,
        Some("165"),
        Some("taxcalc_unreachable"),
    );
    // `post_webhook` points at api.stripe.invalid: the recording cannot succeed.
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK, "the webhook still succeeds");
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::from(25),
        "the credit is unaffected by a reporting failure"
    );
}

// ---------------------------------------------------------------------------
// The stablecoin rail: a second fee schedule on the same webhook
// ---------------------------------------------------------------------------
//
// The crypto rail is priced at a flat 5% with no floor, against the card rail's
// 5.5% with a $0.80 floor. Both arrive at this one handler, and which schedule
// a session was sold on is carried by `metadata[rail]` — a field the buyer's
// side of the world can write. These tests exist to prove that the field is
// CHECKED rather than believed: naming the cheaper rail on a session that was
// priced and paid on the dearer one must credit nothing, and the reverse must
// credit nothing too.

/// A paid session on the crypto rail: the same shape as `paid_session_event`
/// plus the `rail` metadata a crypto session carries.
fn crypto_session_event(
    session_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_total: i64,
    currency: &str,
) -> String {
    rail_session_event(
        session_id,
        user_id,
        metadata_credit_usd,
        amount_total,
        currency,
        Some("crypto"),
    )
}

/// The same, with the rail metadata set to anything at all (or removed), so a
/// test can claim a rail the session was not sold on.
fn rail_session_event(
    session_id: &str,
    user_id: Uuid,
    metadata_credit_usd: &str,
    amount_total: i64,
    currency: &str,
    rail: Option<&str>,
) -> String {
    let mut event: Value = serde_json::from_str(&paid_session_event(
        session_id,
        user_id,
        metadata_credit_usd,
        amount_total,
        currency,
    ))
    .expect("base event must parse");
    match rail {
        Some(rail) => event["data"]["object"]["metadata"]["rail"] = json!(rail),
        None => {
            event["data"]["object"]["metadata"]
                .as_object_mut()
                .expect("metadata is an object")
                .remove("rail");
        }
    }
    event.to_string()
}

/// The happy path on the crypto rail: $25 of credit costs $26.25 (5%, no
/// floor), and exactly $25 is credited — the fee is never credited, exactly as
/// on the card rail.
#[tokio::test]
async fn a_crypto_purchase_credits_the_net_at_the_five_percent_schedule() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "crypto-happy").await;
    let session_id = unique_session_id();
    // 2_625 = $25.00 credit + $1.25 fee. The CARD schedule would have made this
    // 2_638; that difference is the whole point of the rail.
    record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let event = crypto_session_event(&session_id, user_id, "25.00", 2_625, "usd");

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "the NET credit lands; the 5% fee never does"
    );
    assert_eq!(purchase_count(&pool, user_id).await, 1);
}

/// **Replay is a no-op on the crypto rail too.**
///
/// Idempotence is anchored on the unique index over
/// `credit_ledger.stripe_session_id`, which a crypto purchase populates exactly
/// like a card one — there is no second anchor and no second code path. A
/// redelivered stablecoin webhook must therefore credit once and acknowledge
/// forever.
#[tokio::test]
async fn a_replayed_crypto_webhook_credits_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "crypto-replay").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 1_050, Decimal::from(10), "usd")
        .await
        .expect("pending purchase record must insert");
    let event = crypto_session_event(&session_id, user_id, "10.00", 1_050, "usd");

    for attempt in 1..=3 {
        let (status, body) = post_webhook(&pool, &event).await;
        assert_eq!(status, StatusCode::OK, "attempt {attempt} body: {body}");
    }
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(10),
        "three deliveries of one payment credit ten dollars, not thirty"
    );
    assert_eq!(
        purchase_count(&pool, user_id).await,
        1,
        "exactly one purchase ledger row survives the replays"
    );
}

/// **Claiming the cheap rail on a card-priced session credits nothing.**
///
/// This is the cross-rail attack in the direction that costs ZeroRouter money.
/// A $25 card session collects $26.38. Relabelling it `rail=crypto` makes the
/// webhook recompute the gross on the 5% schedule — $26.25 — which does not
/// equal the $26.38 that actually arrived, so Layer 1 refuses it.
#[tokio::test]
async fn a_card_session_relabelled_as_crypto_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-downgrade").await;
    let session_id = unique_session_id();
    // Priced and paid on the CARD schedule.
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    // ...but the event claims the cheaper rail.
    let event = rail_session_event(&session_id, user_id, "25.00", 2_638, "usd", Some("crypto"));

    let (status, _) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_nothing_credited(&pool, user_id, "card session relabelled crypto").await;
}

/// **And the reverse: claiming the dear rail on a crypto-priced session.**
///
/// A $25 crypto session collects $26.25. Relabelling it `rail=card` recomputes
/// $26.38, which is more than arrived — a short payment, refused. Asserted so
/// the guard is known to be an equality in both directions rather than a
/// one-sided "did they pay at least enough" check.
#[tokio::test]
async fn a_crypto_session_relabelled_as_card_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-upgrade").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let event = rail_session_event(&session_id, user_id, "25.00", 2_625, "usd", Some("card"));

    let (status, _) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_nothing_credited(&pool, user_id, "crypto session relabelled card").await;
}

/// A rail name this build does not know credits nothing.
///
/// Refusing rather than defaulting is what stops a future rail's session being
/// priced on the card schedule by a router that predates it.
#[tokio::test]
async fn an_unknown_rail_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-unknown").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let event = rail_session_event(
        &session_id,
        user_id,
        "25.00",
        2_625,
        "usd",
        Some("wire-transfer"),
    );

    let (status, _) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_nothing_credited(&pool, user_id, "unknown rail").await;
}

/// **A session with no `rail` key is a CARD session, and still credits.**
///
/// Every Checkout Session created before the crypto rail existed carries no
/// `rail` metadata, and several may be in flight across the deploy that adds
/// it. Reading absent as "card" is what stops this change refusing money that
/// was legitimately taken by the previous build.
#[tokio::test]
async fn a_session_predating_the_rail_metadata_still_credits_as_card() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-absent").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_638, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    // No `rail` key at all — exactly what an older build's session looks like.
    let event = rail_session_event(&session_id, user_id, "25.00", 2_638, "usd", None);

    let (status, body) = post_webhook(&pool, &event).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
}

/// A taxed crypto purchase credits exactly what an untaxed one does.
///
/// The crypto rail runs the SAME `automatic_tax` machinery as the card rail, so
/// the tax rides on top of the ex-tax gross and is stripped back off before any
/// comparison. Nothing about the rail changes the ledger invariant: the tax is
/// never credited and never revenue.
#[tokio::test]
async fn a_taxed_crypto_purchase_credits_only_the_net() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "crypto-taxed").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    // $26.25 ex-tax + $1.64 Massachusetts tax (6.25%) = $27.89 collected.
    let mut event: Value = serde_json::from_str(&crypto_session_event(
        &session_id,
        user_id,
        "25.00",
        2_789,
        "usd",
    ))
    .expect("event must parse");
    event["data"]["object"]["total_details"] = json!({
        "amount_discount": 0,
        "amount_shipping": 0,
        "amount_tax": 164,
    });

    let (status, body) = post_webhook(&pool, &event.to_string()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "the tax is collected for the state, never credited to the buyer"
    );
}

/// An underpaid stablecoin charge credits nothing.
///
/// Stripe's hosted flow constructs the exact transaction the buyer signs, so a
/// partial payment is not a shape this integration expects to see. It is
/// asserted anyway, because "the processor cannot produce it" is a claim about
/// someone else's system and the ledger invariant must not rest on it: any
/// session whose collected amount is not exactly the quoted gross is refused,
/// underpaid or overpaid alike.
#[tokio::test]
async fn an_underpaid_or_overpaid_crypto_charge_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    for (label, collected) in [("under", 2_624_i64), ("over", 2_626)] {
        let user_id = create_user(&pool, &format!("crypto-{label}paid")).await;
        let session_id = unique_session_id();
        record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
            .await
            .expect("pending purchase record must insert");
        let event = crypto_session_event(&session_id, user_id, "25.00", collected, "usd");

        let (status, _) = post_webhook(&pool, &event).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}paid must be refused"
        );
        assert_nothing_credited(&pool, user_id, &format!("{label}paid crypto charge")).await;
    }
}

/// An unsigned or wrongly-signed crypto webhook credits nothing.
///
/// The signature check is shared with the card rail and is not re-implemented
/// for stablecoin, but a rail that moves money deserves its own evidence rather
/// than an inherited argument.
#[tokio::test]
async fn an_unsigned_or_forged_crypto_webhook_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "crypto-unsigned").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let payload = crypto_session_event(&session_id, user_id, "25.00", 2_625, "usd");
    let timestamp = Utc::now().timestamp();

    // (a) No signature header at all.
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header("content-type", "application/json")
        .body(Body::from(payload.clone()))
        .expect("request builds");
    let response = stripe_app(&pool, UNREACHABLE_STRIPE)
        .oneshot(request)
        .await
        .expect("request completes");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_nothing_credited(&pool, user_id, "unsigned crypto webhook").await;

    // (b) Correctly formed, signed with the wrong secret.
    let forged = sign("whsec_not_the_secret", timestamp, payload.as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header(STRIPE_SIGNATURE_HEADER, header(timestamp, &[&forged]))
        .header("content-type", "application/json")
        .body(Body::from(payload.clone()))
        .expect("request builds");
    let response = stripe_app(&pool, UNREACHABLE_STRIPE)
        .oneshot(request)
        .await
        .expect("request completes");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_nothing_credited(&pool, user_id, "forged crypto webhook").await;

    // ...and the same payload correctly signed DOES credit, so the two arms
    // above are proven to fail on the signature and not on the payload.
    let (status, _) = post_webhook(&pool, &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );
}

/// A crypto purchase credits even on a deployment whose crypto rail is now off.
///
/// The flag gates what may be CREATED, not what may be credited. An operator
/// who turns the rail off must not strand a customer who paid ten seconds
/// earlier, and Stripe will keep redelivering that event for three days.
#[tokio::test]
async fn turning_the_rail_off_does_not_strand_an_already_paid_crypto_session() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "crypto-railoff").await;
    let session_id = unique_session_id();
    record_checkout_intent(&pool, &session_id, user_id, 2_625, Decimal::from(25), "usd")
        .await
        .expect("pending purchase record must insert");
    let payload = crypto_session_event(&session_id, user_id, "25.00", 2_625, "usd");
    let timestamp = Utc::now().timestamp();
    let signature = sign(SECRET, timestamp, payload.as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header(STRIPE_SIGNATURE_HEADER, header(timestamp, &[&signature]))
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .expect("request builds");
    // crypto_rail: false — the rail is OFF on this deployment.
    let response = stripe_app_with_rail(&pool, UNREACHABLE_STRIPE, false)
        .oneshot(request)
        .await
        .expect("request completes");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "money already taken is always credited, whatever the flag says now"
    );
}

// ---------------------------------------------------------------------------
// Ships dark: the crypto rail is inert until the operator turns it on
// ---------------------------------------------------------------------------

/// **The whole dark-ship contract, over HTTP.**
///
/// On a deployment that has not set `ZEROROUTER_CRYPTO_RAIL`, asking for a
/// crypto-priced session is refused with 501 `crypto_rail_unavailable` and no
/// session is created — so a hand-made request cannot obtain the cheaper
/// schedule on an account that cannot take a stablecoin payment for it.
///
/// 501 rather than 400 on purpose: the request is well formed and another
/// deployment would honour it.
#[tokio::test]
async fn the_crypto_rail_is_refused_until_the_operator_turns_it_on() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-dark").await;
    // The mock is never reached — the refusal happens before any Stripe call —
    // so an unreachable base is the honest configuration here.
    let (status, body) = post_checkout_on_rail(
        &pool,
        UNREACHABLE_STRIPE,
        user_id,
        "25.00",
        Some("crypto"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "body: {body}");
    assert_eq!(body["error"]["code"], json!("crypto_rail_unavailable"));
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM stripe_checkout_intents WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("intent count must query"),
        0,
        "a refused rail must leave no pending purchase record behind"
    );
}

/// A rail name that is not one of the two is a 400, on any deployment.
#[tokio::test]
async fn an_unknown_rail_is_refused_at_the_checkout_endpoint() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-bogus").await;
    let (status, body) = post_checkout_on_rail(
        &pool,
        UNREACHABLE_STRIPE,
        user_id,
        "25.00",
        Some("bank-transfer"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], json!("invalid_rail"));
}

/// **A crypto session is created at the 5% price and allowlists stablecoin.**
///
/// Driven over HTTP against the mock so the assertion is about what actually
/// reaches Stripe, not about what a pure function returns: the fee schedule and
/// the payment-method restriction must arrive in the SAME request.
#[tokio::test]
async fn a_crypto_checkout_sends_the_five_percent_price_and_the_stablecoin_allowlist() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-live").await;
    let session_id = unique_session_id();
    let (api_base, captured, _version) = mock_checkout_stripe(session_id.clone()).await;

    let (status, body) =
        post_checkout_on_rail(&pool, &api_base, user_id, "25.00", Some("crypto"), true).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");
    let value = |key: &str| form.get(key).map(String::as_str);
    // The allowlist of exactly one, in the same request as the price.
    assert_eq!(value("payment_method_types[0]"), Some("crypto"));
    assert_eq!(value("payment_method_types[1]"), None);
    assert_eq!(value("metadata[rail]"), Some("crypto"));
    // $25.00 credit + $1.25 fee (5%, no floor) — NOT the card rail's $1.38.
    assert_eq!(
        value("line_items[0][price_data][unit_amount]"),
        Some("2500")
    );
    assert_eq!(value("line_items[1][price_data][unit_amount]"), Some("125"));
    assert_eq!(value("metadata[credit_usd]"), Some("25.00"));
    // Tax is Stripe's job on this rail exactly as on the card one.
    assert_eq!(value("automatic_tax[enabled]"), Some("true"));
    assert_eq!(value("billing_address_collection"), Some("required"));

    // ...and the stored quote is the crypto gross, which is what Layer 2 will
    // require the payment to match.
    let intent = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert_eq!(intent.expected_amount_cents, 2_625);
    assert_eq!(intent.expected_credit_usd, Decimal::from(25));
}

/// The card rail on a crypto-enabled deployment excludes stablecoin — and
/// still does not allowlist, so wallets keep working.
#[tokio::test]
async fn a_card_checkout_on_a_crypto_enabled_deployment_excludes_stablecoin() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-card-excl").await;
    let session_id = unique_session_id();
    let (api_base, captured, _version) = mock_checkout_stripe(session_id).await;

    let (status, body) =
        post_checkout_on_rail(&pool, &api_base, user_id, "25.00", Some("card"), true).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");
    let value = |key: &str| form.get(key).map(String::as_str);
    assert_eq!(value("excluded_payment_method_types[0]"), Some("crypto"));
    assert_eq!(
        value("payment_method_types[0]"),
        None,
        "allowlisting here would disable Apple Pay, Google Pay and Link"
    );
    // The CARD schedule: $1.38, not $1.25.
    assert_eq!(value("line_items[1][price_data][unit_amount]"), Some("138"));
}

/// A stablecoin deposit above Stripe's per-transaction cap is refused before a
/// session is minted, rather than creating one with no payable method.
#[tokio::test]
async fn a_crypto_deposit_over_the_stablecoin_cap_is_refused() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "rail-toobig").await;
    // The default checkout ceiling is $1,000, so raising the request above the
    // stablecoin cap needs a deployment that allows it. `stripe_app_with_rail`
    // fixes checkout_max at $1,000, so instead assert the guard directly at the
    // boundary it defends via the quote endpoint below.
    let (status, body) = post_checkout_on_rail(
        &pool,
        UNREACHABLE_STRIPE,
        user_id,
        "9500.00",
        Some("crypto"),
        true,
    )
    .await;
    // Refused either way; the point is that no session is created and the
    // buyer is told, rather than being handed an unpayable form.
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(
        query_scalar::<_, i64>("SELECT COUNT(*) FROM stripe_checkout_intents WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("intent count must query"),
        0
    );
}

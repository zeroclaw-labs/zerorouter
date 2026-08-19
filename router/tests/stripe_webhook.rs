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
    sync::{Arc, Mutex},
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

fn webhook_app(pool: &PgPool) -> axum::Router {
    // The webhook arms never call Stripe, so the unreachable default base is
    // the honest configuration for them.
    stripe_app(pool, "https://api.stripe.invalid")
}

fn stripe_app(pool: &PgPool, api_base: &str) -> axum::Router {
    let config = WebConfig {
        public_base_url: "http://127.0.0.1".to_owned(),
        secure_cookies: false,
        oidc: None,
        stripe: Some(StripeSettings {
            secret_key: "sk_test_unused".to_owned(),
            webhook_secret: SECRET.to_owned(),
            checkout_min_usd: Decimal::from(5),
            checkout_max_usd: Decimal::from(1000),
            api_base: api_base.to_owned(),
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

/// POST a correctly signed payload at the real handler.
async fn post_webhook(pool: &PgPool, payload: &str) -> (StatusCode, Value) {
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

/// A Stripe stand-in that records the Checkout Session form verbatim and
/// answers with a session shaped like the real one. Asserting on what is
/// captured here is the only way to pin the wire contract: everything about
/// tax is decided by the parameters in this form, and a silently dropped
/// parameter is indistinguishable from a working integration until a customer
/// is charged the wrong amount.
async fn mock_checkout_stripe(session_id: String) -> (String, CapturedForm) {
    let captured: CapturedForm = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route(
            "/v1/checkout/sessions",
            post(
                |State((captured, session_id)): State<(CapturedForm, String)>,
                 Form(form): Form<HashMap<String, String>>| async move {
                    *captured.lock().expect("captured form must lock") = Some(form);
                    axum::Json(json!({
                        "id": session_id,
                        "url": format!("https://checkout.stripe.invalid/c/pay/{session_id}"),
                    }))
                },
            ),
        )
        .with_state((captured.clone(), session_id));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock stripe should bind");
    let address = listener.local_addr().expect("mock stripe address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), captured)
}

/// Drive the real `POST /api/billing/checkout` handler as an authenticated
/// portal user, against whatever `api_base` is passed.
async fn post_checkout(
    pool: &PgPool,
    api_base: &str,
    user_id: Uuid,
    amount_usd: &str,
) -> (StatusCode, Value) {
    let (token, _) = create_session(pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("portal session must create");
    let request = Request::builder()
        .method("POST")
        .uri("/api/billing/checkout")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(CSRF_HEADER, "1")
        .body(Body::from(json!({ "amount_usd": amount_usd }).to_string()))
        .expect("checkout request should build");
    let response = stripe_app(pool, api_base)
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
    let (api_base, captured) = mock_checkout_stripe(session_id.clone()).await;

    let (status, body) = post_checkout(&pool, &api_base, user_id, "25.00").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let form = captured
        .lock()
        .expect("captured form must lock")
        .clone()
        .expect("stripe must have been called");
    let expected: HashMap<String, String> = [
        ("mode", "payment"),
        ("line_items[0][price_data][currency]", "usd"),
        // The unit amount is the EX-TAX gross: $25.00 credit + ceil(0.055 * 25)
        // = $1.38 fee = $26.38. Tax is added on top of this by Stripe.
        ("line_items[0][price_data][unit_amount]", "2638"),
        (
            "line_items[0][price_data][product_data][name]",
            "ZeroRouter credits",
        ),
        ("line_items[0][quantity]", "1"),
        // The whole of ZeroRouter's tax integration. No tax code and no tax
        // behavior: those are Tax Settings' job, so the operator can revise the
        // classification without a deploy.
        ("automatic_tax[enabled]", "true"),
        ("metadata[user_id]", &user_id.to_string()),
        ("metadata[credit_usd]", "25.00"),
        ("metadata[fee_usd]", "1.38"),
        ("metadata[gross_usd]", "26.38"),
        ("customer_email", &email),
        ("success_url", "http://127.0.0.1/credits?checkout=success"),
        ("cancel_url", "http://127.0.0.1/credits?checkout=cancelled"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(form, expected, "the Checkout Session form moved");

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

    // The intent row still records the EX-TAX gross in cents and the net
    // credit in dollars — the two numbers the webhook reconciles against.
    let intent = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert_eq!(intent.expected_amount_cents, 2_638);
    assert_eq!(intent.expected_credit_usd, Decimal::from(25));
    assert_eq!(intent.user_id, user_id);
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
    let (api_base, captured) = mock_checkout_stripe(session_id).await;

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

//! Minimal hand-rolled Stripe integration: Checkout Session creation,
//! webhook signature verification, and prepaid-credit application.
//!
//! The `async-stripe` SDK is deliberately not used — ZeroRouter needs exactly
//! two interactions (create a Checkout Session, verify and apply a
//! `checkout.session.completed` webhook), both small enough to implement
//! against the documented wire formats. The webhook path is fail-closed: a
//! missing or invalid signature, a stale timestamp, or unknown/malformed
//! metadata rejects the event and credits nothing. Replays are idempotent —
//! `crate::billing::credit_purchase` anchors each purchase to the unique
//! Stripe session id, so a redelivered event is acknowledged without a second
//! credit.
//!
//! # What the signature does and does not prove
//!
//! A valid HMAC proves the event came from Stripe. It proves nothing about
//! who created the session it describes: `metadata` is chosen by whoever
//! created the Checkout Session, and any party able to create a paid session
//! in this Stripe account — a second integration, an operational mistake, a
//! leaked restricted API key — can attach arbitrary `credit_usd` and
//! `user_id` to a session Stripe will then sign legitimately. Crediting the
//! metadata alone lets $1 collected mint $1000 of inference credit.
//!
//! Two independent preconditions therefore gate every credit, both applied
//! before [`billing::credit_purchase`] is reached:
//!
//! 1. **The event must corroborate itself.** The session's own `amount_total`
//!    and `currency` — what Stripe actually collected — must match the
//!    claimed credit, converted through the single [`usd_to_cents`] helper
//!    that also produces the quote, so the two directions agree by
//!    construction and no float ever touches money.
//! 2. **ZeroRouter must have priced the session.** A
//!    `stripe_checkout_intents` row written at session creation
//!    (migration `0005`) must exist and agree on user, amount, and currency.
//!    Layer 1 alone still trusts that any paid session in the account is ours;
//!    this layer does not.
//!
//! The dollars credited and the user credited both come from that stored
//! record, never from the event. A session created before migration 0005 has
//! no record and is rejected — see [`stripe_webhook`].
//!
//! # Events consumed
//!
//! | Event | Action |
//! |---|---|
//! | `checkout.session.completed` / `.async_payment_succeeded` | credit the purchase, once per session id |
//! | `payment_intent.succeeded` / `.payment_failed` | settle or fail an autopay charge (migration 0008) |
//! | `charge.dispute.created` | freeze the account and reverse the credit (migration 0009) |
//! | `charge.refunded` | reverse the credit; no freeze (migration 0009) |
//!
//! Everything else is acknowledged without action so Stripe stops retrying it.
//! **The Stripe endpoint must be subscribed to the events above** — an event
//! Stripe does not send is an event this code never runs (see
//! `docs/DEPLOY.md`).
//!
//! The reversal arm reads none of the metadata the crediting arms have to
//! defend: a dispute is mapped to a user through Stripe's own `payment_intent`
//! id joined against ZeroRouter's ledger — see [`handle_reversal_event`].
//!
//! Nothing in this module ever logs the Stripe secret key, the webhook
//! secret, a signature value, or a request/response body.

use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

use crate::{
    billing::{self, CreditOutcome},
    session::PortalUser,
    sqlx,
    web::{StripeSettings, WebCtx},
};

/// Header carrying Stripe's `t=<unix>,v1=<hex>` webhook signature.
pub const STRIPE_SIGNATURE_HEADER: &str = "stripe-signature";

/// Maximum accepted skew between the signed timestamp and the current time.
const WEBHOOK_TOLERANCE: Duration = Duration::from_secs(300);
fn checkout_sessions_url(settings: &StripeSettings) -> String {
    format!("{}/v1/checkout/sessions", settings.api_base)
}
const STRIPE_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CHECKOUT_COMPLETED_EVENT: &str = "checkout.session.completed";
const CHECKOUT_ASYNC_SUCCEEDED_EVENT: &str = "checkout.session.async_payment_succeeded";
/// A charge was refunded, in whole or in part. The event object is the CHARGE,
/// so its `amount_refunded` is CUMULATIVE across every refund on it.
const CHARGE_REFUNDED_EVENT: &str = "charge.refunded";
/// A cardholder disputed a charge. The event object is the DISPUTE; the money
/// has already left the ZeroRouter balance at Stripe by the time it arrives.
const DISPUTE_CREATED_EVENT: &str = "charge.dispute.created";
const CHECKOUT_PRODUCT_NAME: &str = "ZeroRouter credits";
/// The one ISO-4217 currency ZeroRouter prices checkout in. Quoted to Stripe
/// at session creation, stored on the pending record, and re-checked against
/// the webhook's `currency` — an amount match alone is not proof of the price,
/// because the smallest unit of a zero-decimal currency (1000 JPY, roughly $6)
/// numerically equals a cents amount ($10.00).
const CHECKOUT_CURRENCY: &str = "usd";
/// SQLSTATE for a foreign-key violation: the metadata user does not exist.
const PG_FOREIGN_KEY_VIOLATION: &str = "23503";

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Webhook signature verification (pure)
// ---------------------------------------------------------------------------

/// Why a `stripe-signature` header failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebhookVerifyError {
    /// Not in Stripe's `t=<unix>,v1=<hex>` format: the timestamp is missing
    /// or unparseable, or there is no `v1` candidate at all.
    #[error("the stripe-signature header is malformed")]
    MalformedHeader,
    /// The signed timestamp is further from now than the tolerance allows.
    #[error("the stripe-signature timestamp is outside the accepted tolerance")]
    TimestampOutOfTolerance,
    /// No `v1` candidate matches the recomputed HMAC.
    #[error("no stripe-signature candidate matches the payload")]
    SignatureMismatch,
}

/// Verify a Stripe webhook signature header against the raw request body.
///
/// Stripe signs `{t}.{payload}` with HMAC-SHA256 under the endpoint secret
/// and sends `t=<unix>,v1=<hex>[,v1=<hex>...]`. Verification succeeds when
/// the timestamp is within `tolerance` of `now_unix` and **any** `v1`
/// candidate matches the recomputed digest (constant-time comparison via
/// [`Mac::verify_slice`]). Every ambiguous input fails closed.
pub fn verify_webhook_signature(
    payload: &[u8],
    signature_header: &str,
    secret: &str,
    tolerance: Duration,
    now_unix: i64,
) -> Result<(), WebhookVerifyError> {
    let parsed = parse_signature_header(signature_header)?;
    if now_unix.abs_diff(parsed.timestamp) > tolerance.as_secs() {
        return Err(WebhookVerifyError::TimestampOutOfTolerance);
    }
    // The digest depends only on the timestamp and the payload, so it is
    // computed ONCE and compared against each candidate. Rebuilding it per
    // candidate let an unauthenticated caller — the webhook endpoint is
    // public by necessity — force arbitrary hashing work: a few thousand
    // `v1=` fields against a large body is hundreds of megabytes of SHA-256
    // before anything is rejected (sol review).
    //
    // HMAC-SHA256 accepts keys of any length, so construction cannot fail;
    // if it somehow does, fail closed.
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return Err(WebhookVerifyError::SignatureMismatch);
    };
    // Sign the exact timestamp string from the header, not a re-rendered
    // integer, so byte-level oddities cannot desynchronize the digest.
    mac.update(parsed.timestamp_raw.as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = mac.finalize().into_bytes();
    for candidate in parsed.candidates {
        // A non-hex candidate can never match; skip it rather than abort so a
        // valid sibling signature (e.g. during secret rotation) still passes.
        let Ok(candidate_bytes) = hex::decode(candidate) else {
            continue;
        };
        if candidate_bytes.len() == expected.len() && constant_time_eq(&candidate_bytes, &expected)
        {
            return Ok(());
        }
    }
    Err(WebhookVerifyError::SignatureMismatch)
}

struct ParsedSignatureHeader<'a> {
    timestamp: i64,
    timestamp_raw: &'a str,
    candidates: Vec<&'a str>,
}

/// Signatures Stripe can plausibly send at once: the current secret plus
/// one being rotated in leaves room to spare. Anything beyond this is an
/// attempt to make the endpoint do work, not to authenticate.
const MAX_SIGNATURE_CANDIDATES: usize = 8;

/// Length of a hex-encoded SHA-256 digest. A candidate of any other length
/// cannot match, so it is not worth decoding.
const SIGNATURE_HEX_LEN: usize = 64;

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn parse_signature_header(header: &str) -> Result<ParsedSignatureHeader<'_>, WebhookVerifyError> {
    let mut timestamp_raw = None;
    let mut candidates = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => timestamp_raw = Some(value.trim()),
            "v1" => {
                let value = value.trim();
                // Only well-formed candidates are kept, and only a few: an
                // unbounded list is a work amplifier, not a signature.
                if value.len() == SIGNATURE_HEX_LEN && candidates.len() < MAX_SIGNATURE_CANDIDATES {
                    candidates.push(value);
                }
            }
            // v0 (legacy) and future schemes are ignored, per Stripe's docs.
            _ => {}
        }
    }
    let timestamp_raw = timestamp_raw.ok_or(WebhookVerifyError::MalformedHeader)?;
    let timestamp = timestamp_raw
        .parse::<i64>()
        .map_err(|_| WebhookVerifyError::MalformedHeader)?;
    if candidates.is_empty() {
        return Err(WebhookVerifyError::MalformedHeader);
    }
    Ok(ParsedSignatureHeader {
        timestamp,
        timestamp_raw,
        candidates,
    })
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

/// Routes owned by the Stripe integration: portal checkout creation and the
/// webhook receiver.
pub fn router() -> Router<WebCtx> {
    Router::new()
        .route("/api/billing/checkout", post(create_checkout))
        .route(
            "/api/billing/autopay",
            axum::routing::get(get_autopay).put(put_autopay),
        )
        .route("/api/billing/autopay/setup", post(create_autopay_setup))
        .route("/webhooks/stripe", post(stripe_webhook))
}

#[derive(Debug)]
enum StripeHttpError {
    BillingUnavailable,
    InvalidAmount,
    CheckoutFailed,
    InvalidSignature,
    MalformedEvent,
    /// The signed event does not corroborate itself, or contradicts what
    /// ZeroRouter quoted for that session.
    AmountMismatch,
    /// A paid session ZeroRouter has no pending-purchase record for.
    UnknownSession,
    UnknownUser,
    DatabaseUnavailable,
}

impl IntoResponse for StripeHttpError {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            Self::BillingUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Stripe billing is not configured on this deployment.",
                "billing_unavailable",
            ),
            Self::InvalidAmount => (
                StatusCode::BAD_REQUEST,
                "The credit amount must be a USD value with at most two decimal places, within the configured checkout bounds.",
                "invalid_amount",
            ),
            Self::CheckoutFailed => (
                StatusCode::BAD_GATEWAY,
                "The Stripe checkout session could not be created; try again shortly.",
                "checkout_failed",
            ),
            Self::InvalidSignature => (
                StatusCode::BAD_REQUEST,
                "The webhook signature is missing or invalid.",
                "invalid_signature",
            ),
            Self::MalformedEvent => (
                StatusCode::BAD_REQUEST,
                "The webhook event is malformed.",
                "malformed_event",
            ),
            // 4xx rather than a silent 200: a mismatch is a security event,
            // and leaving it visibly failing in Stripe's webhook dashboard is
            // the alerting channel this deployment has.
            Self::AmountMismatch => (
                StatusCode::BAD_REQUEST,
                "The webhook event does not match the recorded checkout amount.",
                "amount_mismatch",
            ),
            Self::UnknownSession => (
                StatusCode::BAD_REQUEST,
                "The webhook event references a checkout session this deployment did not create.",
                "unknown_session",
            ),
            Self::UnknownUser => (
                StatusCode::BAD_REQUEST,
                "The webhook event references an unknown user.",
                "unknown_user",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Credit application is temporarily unavailable.",
                "database_unavailable",
            ),
        };
        (
            status,
            Json(serde_json::json!({
                "error": { "message": message, "type": "billing_error", "code": code }
            })),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// POST /api/billing/checkout
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CheckoutRequest {
    /// `rust_decimal`'s deserializer accepts both JSON strings ("25.00") and
    /// JSON numbers (25), so no untagged wrapper is needed.
    amount_usd: Decimal,
}

async fn create_checkout(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Json(request): Json<CheckoutRequest>,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let amount_usd = request.amount_usd;
    let unit_amount_cents = validate_checkout_amount(amount_usd, stripe)?;
    let session = create_checkout_session(
        stripe,
        CheckoutSessionParams {
            user_id: user.user_id,
            customer_email: &user.email,
            credit_usd: amount_usd,
            unit_amount_cents,
            success_url: ctx.config.absolute_url("/credits?checkout=success"),
            cancel_url: ctx.config.absolute_url("/credits?checkout=cancelled"),
        },
    )
    .await?;
    // Persist what this session is worth BEFORE handing back the redirect
    // url. The session id only exists after Stripe mints it, so the record
    // cannot precede the session — but it can precede the user ever seeing
    // the payment page. If this insert fails the url is withheld, so the
    // session is unreachable and expires unpaid rather than becoming a
    // payment the webhook would (correctly) refuse to credit.
    if let Err(error) = billing::record_checkout_intent(
        &ctx.pool,
        &session.id,
        user.user_id,
        unit_amount_cents,
        amount_usd,
        CHECKOUT_CURRENCY,
    )
    .await
    {
        tracing::error!(
            user_id = %user.user_id,
            stripe_session_id = %session.id,
            %error,
            "stripe checkout session created but its pending purchase record could not be \
             persisted; withholding the redirect url so the session is never paid"
        );
        return Err(StripeHttpError::CheckoutFailed);
    }
    tracing::info!(
        user_id = %user.user_id,
        stripe_session_id = %session.id,
        amount_usd = %amount_usd,
        "created stripe checkout session"
    );
    Ok(Json(serde_json::json!({ "url": session.url })))
}

/// Validate a requested credit amount and convert it to integer cents.
///
/// Rejects amounts outside the configured `[min, max]` bounds and amounts
/// with more than two decimal places (Stripe charges integer cents; anything
/// finer would silently round money).
fn validate_checkout_amount(
    amount_usd: Decimal,
    settings: &StripeSettings,
) -> Result<i64, StripeHttpError> {
    if amount_usd < settings.checkout_min_usd || amount_usd > settings.checkout_max_usd {
        return Err(StripeHttpError::InvalidAmount);
    }
    usd_to_cents(amount_usd).ok_or(StripeHttpError::InvalidAmount)
}

/// Convert decimal USD to the integer smallest currency unit Stripe quotes,
/// collects, and reports in `amount_total`.
///
/// The single conversion in this module: the checkout quote and the webhook's
/// amount check both go through it, so "what we asked Stripe to collect" and
/// "what we require Stripe to have collected" agree by construction rather
/// than by two independently maintained expressions. Exact `Decimal`
/// arithmetic throughout — no float ever touches money.
///
/// `None` when the amount is finer than a cent (anything finer would silently
/// round money) or does not fit an `i64`.
fn usd_to_cents(amount_usd: Decimal) -> Option<i64> {
    if amount_usd.normalize().scale() > 2 {
        return None;
    }
    (amount_usd * Decimal::ONE_HUNDRED).normalize().to_i64()
}

struct CheckoutSessionParams<'a> {
    user_id: Uuid,
    customer_email: &'a str,
    credit_usd: Decimal,
    unit_amount_cents: i64,
    success_url: String,
    cancel_url: String,
}

struct CheckoutSession {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct CheckoutSessionResponse {
    id: String,
    url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum CheckoutError {
    #[error("the Stripe HTTP client could not be constructed")]
    Client,
    #[error("the Stripe checkout session request failed")]
    Request,
    #[error("Stripe rejected the checkout session request")]
    Status,
    #[error("the Stripe checkout session response could not be parsed")]
    MalformedResponse,
}

impl From<CheckoutError> for StripeHttpError {
    fn from(_: CheckoutError) -> Self {
        Self::CheckoutFailed
    }
}

/// Create a Stripe Checkout Session over the form-encoded REST API.
///
/// Logs never include the secret key, the form body, or the raw response.
async fn create_checkout_session(
    settings: &StripeSettings,
    params: CheckoutSessionParams<'_>,
) -> Result<CheckoutSession, CheckoutError> {
    let client = reqwest::Client::builder()
        .timeout(STRIPE_HTTP_TIMEOUT)
        .build()
        .map_err(|_| {
            tracing::warn!("stripe HTTP client construction failed");
            CheckoutError::Client
        })?;
    let unit_amount = params.unit_amount_cents.to_string();
    let user_id = params.user_id.to_string();
    let credit_usd = params.credit_usd.to_string();
    let form: [(&str, &str); 10] = [
        ("mode", "payment"),
        ("line_items[0][price_data][currency]", CHECKOUT_CURRENCY),
        ("line_items[0][price_data][unit_amount]", &unit_amount),
        (
            "line_items[0][price_data][product_data][name]",
            CHECKOUT_PRODUCT_NAME,
        ),
        ("line_items[0][quantity]", "1"),
        ("metadata[user_id]", &user_id),
        ("metadata[credit_usd]", &credit_usd),
        ("customer_email", params.customer_email),
        ("success_url", &params.success_url),
        ("cancel_url", &params.cancel_url),
    ];
    let response = client
        .post(checkout_sessions_url(settings))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                timeout = error.is_timeout(),
                "stripe checkout session request failed"
            );
            CheckoutError::Request
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            status = status.as_u16(),
            "stripe rejected the checkout session request"
        );
        return Err(CheckoutError::Status);
    }
    let session: CheckoutSessionResponse = response.json().await.map_err(|_| {
        tracing::warn!("stripe checkout session response could not be parsed");
        CheckoutError::MalformedResponse
    })?;
    let Some(url) = session.url else {
        tracing::warn!("stripe checkout session response is missing the redirect url");
        return Err(CheckoutError::MalformedResponse);
    };
    Ok(CheckoutSession {
        id: session.id,
        url,
    })
}

// ---------------------------------------------------------------------------
// POST /webhooks/stripe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StripeEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: StripeEventData,
}

#[derive(Debug, Deserialize)]
struct StripeEventData {
    object: Value,
}

async fn stripe_webhook(
    State(ctx): State<WebCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let Some(signature) = headers
        .get(STRIPE_SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::warn!("stripe webhook rejected: missing stripe-signature header");
        return Err(StripeHttpError::InvalidSignature);
    };
    // Verify BEFORE parsing: unauthenticated bytes never reach the JSON
    // parser, and the raw payload is never logged.
    if let Err(reason) = verify_webhook_signature(
        &body,
        signature,
        &stripe.webhook_secret,
        WEBHOOK_TOLERANCE,
        Utc::now().timestamp(),
    ) {
        tracing::warn!(%reason, "stripe webhook rejected: signature verification failed");
        return Err(StripeHttpError::InvalidSignature);
    }
    let event: StripeEvent = serde_json::from_slice(&body).map_err(|_| {
        tracing::warn!("stripe webhook rejected: event is not valid JSON");
        StripeHttpError::MalformedEvent
    })?;
    if event.event_type == "payment_intent.succeeded"
        || event.event_type == "payment_intent.payment_failed"
    {
        return handle_autopay_intent_event(&ctx, &event).await;
    }
    if event.event_type == DISPUTE_CREATED_EVENT || event.event_type == CHARGE_REFUNDED_EVENT {
        return handle_reversal_event(&ctx, &event).await;
    }
    if event.event_type != CHECKOUT_COMPLETED_EVENT
        && event.event_type != CHECKOUT_ASYNC_SUCCEEDED_EVENT
    {
        // Acknowledged without action so Stripe does not retry event types
        // this deployment does not consume.
        return Ok(received());
    }
    let object = &event.data.object;
    if object.get("payment_status").and_then(Value::as_str) != Some("paid") {
        // Completed but not yet paid (asynchronous payment methods): nothing
        // to credit; a later `paid` event will carry the money.
        return Ok(received());
    }
    let Some(session_id) = object.get("id").and_then(Value::as_str) else {
        tracing::warn!("stripe webhook rejected: paid session is missing its id");
        return Err(StripeHttpError::MalformedEvent);
    };
    let metadata = object.get("metadata");
    let user_id = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok());
    let credit_usd = metadata
        .and_then(|metadata| metadata.get("credit_usd"))
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<Decimal>().ok());
    let (Some(user_id), Some(credit_usd)) = (user_id, credit_usd) else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: paid session has missing or malformed metadata"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    if credit_usd <= Decimal::ZERO {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: paid session metadata carries a non-positive credit"
        );
        return Err(StripeHttpError::MalformedEvent);
    }

    // --- Layer 1: the event must corroborate itself ------------------------
    //
    // `metadata` is chosen by whoever created the session; `amount_total` and
    // `currency` are what Stripe actually collected. Requiring them to agree
    // means forged metadata on a session we did create cannot inflate the
    // credit beyond the money that moved.
    let amount_total_cents = object.get("amount_total").and_then(Value::as_i64);
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let (Some(amount_total_cents), Some(currency)) = (amount_total_cents, currency) else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: paid session is missing amount_total or currency"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    let Some(claimed_cents) = usd_to_cents(credit_usd) else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "stripe webhook rejected: metadata credit is finer than a cent or out of range"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    if claimed_cents != amount_total_cents || currency != CHECKOUT_CURRENCY {
        // Loud and detailed: this is the shape of a credit-minting attempt,
        // not a transient fault. Everything logged here is already public to
        // whoever produced the event.
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            metadata_credit_usd = %credit_usd,
            claimed_cents,
            amount_total_cents,
            %currency,
            expected_currency = CHECKOUT_CURRENCY,
            "stripe webhook rejected: paid session does not corroborate its own metadata; \
             crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    }

    // --- Layer 2: ZeroRouter must have priced this session ------------------
    //
    // Layer 1 still assumes every paid session in the Stripe account is ours.
    // A session created by anything else — another integration, a leaked
    // restricted key — can be internally consistent and still not be a
    // purchase this deployment sold.
    let intent = match billing::checkout_intent(&ctx.pool, session_id).await {
        Ok(intent) => intent,
        Err(_) => {
            // Retryable: the record may well exist and be unreadable right
            // now. Fail closed and let Stripe redeliver.
            tracing::warn!(
                stripe_session_id = %session_id,
                "stripe webhook deferred: pending purchase record could not be read; \
                 stripe will retry"
            );
            return Err(StripeHttpError::DatabaseUnavailable);
        }
    };
    let Some(intent) = intent else {
        // POLICY — pre-existing sessions: sessions created before migration
        // 0005 also land here, and are rejected rather than credited. Failing
        // closed is the point of the record; a "credit it anyway and warn"
        // fallback would leave the original hole open behind a log line. The
        // exposure is bounded — Checkout Sessions expire after 24h, so at most
        // one day of in-flight purchases — and each is reconcilable by hand
        // from the Stripe dashboard with an 'adjustment' ledger entry.
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            metadata_credit_usd = %credit_usd,
            amount_total_cents,
            "stripe webhook rejected: paid session has no pending purchase record; \
             crediting nothing (reconcile by hand if this predates migration 0005)"
        );
        return Err(StripeHttpError::UnknownSession);
    };
    if intent.user_id != user_id
        || intent.expected_amount_cents != amount_total_cents
        || intent.currency != currency
    {
        tracing::error!(
            stripe_session_id = %session_id,
            metadata_user_id = %user_id,
            recorded_user_id = %intent.user_id,
            amount_total_cents,
            recorded_amount_cents = intent.expected_amount_cents,
            %currency,
            recorded_currency = %intent.currency,
            "stripe webhook rejected: paid session contradicts its pending purchase record; \
             crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    }

    // Both the recipient and the dollars come from ZeroRouter's own record.
    // The metadata has now been checked against it and is not used again.
    let user_id = intent.user_id;
    let credit_usd = intent.expected_credit_usd;
    let payment_intent = object.get("payment_intent").and_then(Value::as_str);
    match billing::credit_purchase(&ctx.pool, user_id, credit_usd, session_id, payment_intent).await
    {
        Ok(outcome) => {
            if matches!(outcome, CreditOutcome::AlreadyApplied) {
                tracing::info!(
                    stripe_session_id = %session_id,
                    "stripe webhook replayed: purchase already applied"
                );
            } else {
                tracing::info!(
                    stripe_session_id = %session_id,
                    user_id = %user_id,
                    amount_usd = %credit_usd,
                    "applied stripe purchase credit"
                );
            }
            // Stamped only after the credit has committed, and deliberately
            // not fatal: idempotence belongs to the unique index on
            // `credit_ledger.stripe_session_id`, so a lost marker costs a
            // reconciliation query, while stamping first would risk a
            // settled-but-uncredited session on a retry.
            if let Err(error) = billing::settle_checkout_intent(&ctx.pool, session_id).await {
                tracing::warn!(
                    stripe_session_id = %session_id,
                    %error,
                    "stripe purchase credited but its pending record could not be marked settled"
                );
            }
            Ok(received())
        }
        Err(error) if is_foreign_key_violation(&error) => {
            tracing::warn!(
                stripe_session_id = %session_id,
                "stripe webhook rejected: metadata references an unknown user"
            );
            Err(StripeHttpError::UnknownUser)
        }
        Err(_) => {
            tracing::warn!(
                stripe_session_id = %session_id,
                "stripe webhook credit application failed; stripe will retry"
            );
            Err(StripeHttpError::DatabaseUnavailable)
        }
    }
}

fn received() -> Json<Value> {
    Json(serde_json::json!({ "received": true }))
}

// ---------------------------------------------------------------------------
// Refunds and chargebacks (migration 0009)
// ---------------------------------------------------------------------------

/// The `charge.refunded` / `charge.dispute.created` arm: take the credit back,
/// and — for a dispute only — freeze the account.
///
/// # Why a dispute freezes and a refund does not
///
/// A refund is ZeroRouter or Stripe support giving money back deliberately;
/// taking the credit with it is the whole correction. A dispute is a customer
/// telling their bank the charge was not legitimate. The money is already gone
/// from the ZeroRouter balance, the customer may have consumed the inference it
/// bought, and nothing about the account can be trusted until a human looks —
/// so the account stops spending. Its history stays readable: the freeze blocks
/// spend, not visibility.
///
/// # What this trusts
///
/// Only Stripe's own fields, and only after the HMAC has been verified. The
/// charge/dispute is mapped back to a user through its `payment_intent` — a
/// Stripe-generated id — joined against ZeroRouter's OWN ledger
/// ([`billing::credited_purchase`]). `metadata` is never read here, so the
/// co-tenant problem the checkout and autopay arms have to defend against
/// (anyone able to create objects in the Stripe account can write metadata)
/// does not arise: an event naming an intent this deployment never credited
/// matches nothing and moves nothing.
///
/// # What it deliberately does not do
///
/// Reverse a PARTIAL refund or a partial dispute. The reversal takes back
/// exactly what was credited, so it only runs when the reversed amount covers
/// that credit in full; anything less is logged for an operator instead of
/// guessed at. A dispute still freezes in that case, which is the half that
/// cannot wait. Partial refunds of prepaid credit are not something this
/// deployment issues today, and inventing an apportioning rule for money
/// without an operator asking for one is exactly the kind of quiet decision
/// this module exists to avoid.
async fn handle_reversal_event(
    ctx: &WebCtx,
    event: &StripeEvent,
) -> Result<Json<Value>, StripeHttpError> {
    let object = &event.data.object;
    let is_dispute = event.event_type == DISPUTE_CREATED_EVENT;

    // The Stripe object this reversal is anchored to: the dispute id for a
    // chargeback, the charge id for a refund. It becomes the reversal's
    // `credit_ledger.stripe_session_id`, so a redelivery deduplicates against
    // the same unique index a replayed purchase does.
    let Some(object_id) = object.get("id").and_then(Value::as_str) else {
        tracing::warn!(
            event_type = %event.event_type,
            "stripe webhook rejected: reversal event is missing its object id"
        );
        return Err(StripeHttpError::MalformedEvent);
    };
    // Present on both shapes: a Dispute carries the intent it disputes, a
    // Charge the intent that created it.
    let payment_intent = object.get("payment_intent").and_then(Value::as_str);
    let Some(payment_intent) = payment_intent else {
        // Unattributable. Acknowledged so Stripe stops retrying something no
        // redelivery can fix, but logged at error level: if this is ours, a
        // human has to reconcile it by hand.
        tracing::error!(
            event_type = %event.event_type,
            stripe_object_id = %object_id,
            "stripe reversal event names no payment intent; it cannot be attributed to a user \
             and nothing was reversed or frozen — reconcile by hand if this charge is ours"
        );
        return Ok(received());
    };

    let Some(credited) = billing::credited_purchase(&ctx.pool, payment_intent)
        .await
        .map_err(|error| {
            tracing::warn!(
                stripe_object_id = %object_id,
                %error,
                "stripe reversal deferred: the credit ledger could not be read; stripe will retry"
            );
            StripeHttpError::DatabaseUnavailable
        })?
    else {
        // A charge this deployment never credited — another integration in the
        // same Stripe account, or a payment that never became credit.
        // Acknowledged untouched, exactly as a foreign payment intent is.
        tracing::info!(
            event_type = %event.event_type,
            stripe_object_id = %object_id,
            "stripe reversal event references a charge this deployment never credited; ignoring"
        );
        return Ok(received());
    };

    // Freeze FIRST, and independently of whether the reversal can be computed:
    // the account must stop spending even when the money question needs a
    // human. Idempotent, so a redelivered dispute does not restamp it.
    if is_dispute {
        let froze = billing::freeze_account(
            &ctx.pool,
            credited.user_id,
            billing::FreezeReason::Dispute,
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                stripe_object_id = %object_id,
                %error,
                "stripe dispute deferred: the account could not be frozen; stripe will retry"
            );
            StripeHttpError::DatabaseUnavailable
        })?;
        tracing::error!(
            user_id = %credited.user_id,
            stripe_dispute_id = %object_id,
            payment_intent = %payment_intent,
            credited_usd = %credited.amount_usd,
            newly_frozen = froze,
            "stripe chargeback: account frozen"
        );
    }

    // Only a reversal that covers the whole credit is applied automatically.
    // For a dispute the disputed `amount` is the money withdrawn; for a refund
    // `amount_refunded` is the cumulative refunded total on the charge.
    let reversed_cents = if is_dispute {
        object.get("amount").and_then(Value::as_i64)
    } else {
        object.get("amount_refunded").and_then(Value::as_i64)
    };
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let credited_cents = usd_to_cents(credited.amount_usd);
    let covers_the_credit = match (reversed_cents, currency.as_deref(), credited_cents) {
        (Some(reversed), Some(currency), Some(credited_cents)) => {
            // Currency is checked independently of the amount for the same
            // reason the checkout arm checks it: the smallest unit of a
            // zero-decimal currency numerically matches a cents amount while
            // being worth a fraction of it.
            currency == CHECKOUT_CURRENCY && reversed >= credited_cents
        }
        _ => false,
    };
    if !covers_the_credit {
        tracing::error!(
            event_type = %event.event_type,
            user_id = %credited.user_id,
            stripe_object_id = %object_id,
            payment_intent = %payment_intent,
            credited_usd = %credited.amount_usd,
            ?reversed_cents,
            ?currency,
            frozen = is_dispute,
            "stripe reversal does not cover the full credit (partial, foreign-currency, or \
             missing amount); NOTHING was reversed — an operator must reconcile this by hand"
        );
        return Ok(received());
    }

    let note = if is_dispute {
        format!("chargeback reversal ({object_id})")
    } else {
        format!("refund reversal ({object_id})")
    };
    let outcome = billing::reverse_purchase(&ctx.pool, payment_intent, object_id, &note)
        .await
        .map_err(|error| {
            tracing::warn!(
                stripe_object_id = %object_id,
                %error,
                "stripe reversal failed to apply; stripe will retry"
            );
            StripeHttpError::DatabaseUnavailable
        })?;
    match outcome {
        billing::ReversalOutcome::Reversed {
            amount_usd,
            balance_after,
        } => tracing::warn!(
            event_type = %event.event_type,
            user_id = %credited.user_id,
            stripe_object_id = %object_id,
            reversed_usd = %amount_usd,
            balance_after = %balance_after,
            // A negative balance is not an error state: it IS the receivable,
            // and saying so here is what makes it findable later.
            receivable = balance_after < Decimal::ZERO,
            "reversed a stripe credit"
        ),
        billing::ReversalOutcome::AlreadyReversed => tracing::info!(
            stripe_object_id = %object_id,
            "stripe reversal replayed: this purchase was already reversed"
        ),
        // Unreachable in practice: the credit was just read above. Logged
        // rather than errored, because a retry cannot improve it.
        billing::ReversalOutcome::UnknownPurchase => tracing::error!(
            stripe_object_id = %object_id,
            payment_intent = %payment_intent,
            "stripe reversal found no credit for an intent that had one moments earlier"
        ),
    }
    Ok(received())
}

fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .is_some_and(|code| code == PG_FOREIGN_KEY_VIOLATION)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> StripeSettings {
        StripeSettings {
            secret_key: "sk_test_unused".to_owned(),
            webhook_secret: "whsec_unused".to_owned(),
            checkout_min_usd: Decimal::from(5),
            checkout_max_usd: Decimal::from(1000),
            api_base: "https://api.stripe.com".to_owned(),
        }
    }

    fn decimal(raw: &str) -> Decimal {
        raw.parse().expect("test literal must parse")
    }

    #[test]
    fn checkout_amounts_convert_to_integer_cents() {
        let settings = settings();
        assert_eq!(
            validate_checkout_amount(decimal("25.00"), &settings).ok(),
            Some(2500)
        );
        assert_eq!(
            validate_checkout_amount(decimal("25"), &settings).ok(),
            Some(2500)
        );
        assert_eq!(
            validate_checkout_amount(decimal("5.01"), &settings).ok(),
            Some(501)
        );
        assert_eq!(
            validate_checkout_amount(decimal("1000"), &settings).ok(),
            Some(100_000)
        );
        // Trailing zeros beyond two places still describe a whole cent.
        assert_eq!(
            validate_checkout_amount(decimal("25.1000"), &settings).ok(),
            Some(2510)
        );
    }

    #[test]
    fn checkout_amounts_out_of_policy_are_rejected() {
        let settings = settings();
        for raw in ["4.99", "1000.01", "25.001", "0", "-25.00"] {
            assert!(
                validate_checkout_amount(decimal(raw), &settings).is_err(),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn cents_conversion_is_exact_in_both_directions() {
        // The quote and the webhook's amount check share this function, so a
        // value that can be quoted must verify against the amount Stripe then
        // reports, and nothing finer than a cent survives either way.
        let settings = settings();
        for raw in ["5", "25.00", "25.1000", "999.99", "1000"] {
            let amount = decimal(raw);
            assert_eq!(
                validate_checkout_amount(amount, &settings).ok(),
                usd_to_cents(amount),
                "{raw} must quote and verify to the same cents"
            );
        }
        assert_eq!(usd_to_cents(decimal("0.01")), Some(1));
        assert_eq!(usd_to_cents(decimal("1000")), Some(100_000));
        // Sub-cent amounts cannot be represented as an `amount_total`, so a
        // metadata credit claiming one can never corroborate a real payment.
        assert_eq!(usd_to_cents(decimal("25.001")), None);
        assert_eq!(usd_to_cents(decimal("0.005")), None);
    }

    #[test]
    fn signature_headers_parse_strictly() {
        // Candidates must be the length of a hex SHA-256 digest; anything
        // else cannot match, so it is dropped rather than decoded.
        let first = "a".repeat(SIGNATURE_HEX_LEN);
        let second = "c".repeat(SIGNATURE_HEX_LEN);
        let header = format!("t=1700000000,v1={first},v0=bb,v1={second},v1=tooshort");
        let parsed = parse_signature_header(&header).expect("well-formed header must parse");
        assert_eq!(parsed.timestamp, 1_700_000_000);
        assert_eq!(parsed.timestamp_raw, "1700000000");
        assert_eq!(
            parsed.candidates,
            vec![first.as_str(), second.as_str()],
            "v0 and malformed-length candidates are ignored"
        );

        // An unbounded candidate list is a work amplifier: the endpoint is
        // public, and every extra candidate used to mean another full HMAC
        // over the whole body. Both the count and the per-candidate cost are
        // now capped (the digest is computed once).
        let flood = std::iter::repeat_n(format!("v1={first}"), 5_000)
            .collect::<Vec<_>>()
            .join(",");
        let flooded_header = format!("t=1700000000,{flood}");
        let parsed =
            parse_signature_header(&flooded_header).expect("a flooded header still parses");
        assert_eq!(parsed.candidates.len(), MAX_SIGNATURE_CANDIDATES);

        let valid = "a".repeat(SIGNATURE_HEX_LEN);
        for header in [
            String::new(),
            "garbage".to_owned(),
            format!("t=notanumber,v1={valid}"),
            format!("v1={valid}"),
            "t=1700000000".to_owned(),
            // Present but unusable: every candidate is the wrong length.
            "t=1700000000,v1=aa,v1=bb".to_owned(),
        ] {
            assert_eq!(
                parse_signature_header(&header).err(),
                Some(WebhookVerifyError::MalformedHeader),
                "{header:?} should be malformed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Autopay (migration 0008): saved-card auto-recharge.
// ---------------------------------------------------------------------------

const AUTOPAY_PURPOSE: &str = "zerorouter_autopay";
const AUTOPAY_SWEEP_BATCH: i64 = 16;
/// Pending intents older than this are reconciled against Stripe directly.
const AUTOPAY_RECONCILE_AFTER_MINUTES: i32 = 30;
/// Oldest claim the sweep will replay. Stripe caches an idempotency key's
/// result for at least 24 hours and may prune it afterwards; a "replay"
/// past that window is a new request to Stripe, which means a second
/// charge. Twenty hours keeps a margin inside the guarantee (sol review).
const AUTOPAY_REPLAY_MAX_AGE_MINUTES: i32 = 20 * 60;

/// Provenance mark carried in PaymentIntent metadata: an HMAC over the
/// money-bearing fields, keyed by the webhook secret. The webhook's
/// metadata-recovery path only trusts events that carry it, so another
/// integration in the same Stripe account writing our metadata SHAPE
/// cannot mint credits — it does not hold the key (review finding).
/// Length-guarded constant-time comparison, same shape as
/// `auth::constant_time_eq`: XOR-fold every byte so a mismatch position is
/// not observable through timing.
fn constant_time_str_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (l, r)| acc | (l ^ r))
        == 0
}

fn autopay_provenance(settings: &StripeSettings, user_id: Uuid, credit_usd: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(settings.webhook_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(format!("{AUTOPAY_PURPOSE}|{user_id}|{credit_usd}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Ensure the user has a Stripe Customer, creating one on first use. The
/// stored id wins any race: a concurrent creation that loses the UPDATE
/// leaves an orphan customer at Stripe, which is inert.
async fn ensure_stripe_customer(
    ctx: &WebCtx,
    settings: &StripeSettings,
    user_id: Uuid,
    email: &str,
) -> Result<String, StripeHttpError> {
    if let Some(existing) = sqlx::query_scalar::<_, Option<String>>(
        "SELECT stripe_customer_id FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?
        && !existing.is_empty()
    {
        return Ok(existing);
    }
    let client = stripe_client()?;
    let user_id_text = user_id.to_string();
    let form: [(&str, &str); 2] = [("email", email), ("metadata[user_id]", &user_id_text)];
    let response = client
        .post(format!("{}/v1/customers", settings.api_base))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "stripe customer creation rejected"
        );
        return Err(StripeHttpError::CheckoutFailed);
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    let Some(customer_id) = body.get("id").and_then(Value::as_str) else {
        return Err(StripeHttpError::CheckoutFailed);
    };
    sqlx::query(
        "UPDATE users SET stripe_customer_id = $2 WHERE id = $1 AND stripe_customer_id IS NULL",
    )
    .bind(user_id)
    .bind(customer_id)
    .execute(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    let stored = sqlx::query_scalar::<_, Option<String>>(
        "SELECT stripe_customer_id FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    Ok(stored.unwrap_or_else(|| customer_id.to_owned()))
}

fn stripe_client() -> Result<reqwest::Client, StripeHttpError> {
    reqwest::Client::builder()
        .timeout(STRIPE_HTTP_TIMEOUT)
        .build()
        .map_err(|_| StripeHttpError::CheckoutFailed)
}

// POST /api/billing/autopay/setup — a Checkout session in `setup` mode that
// saves a card to the user's Stripe customer for off-session charging.
async fn create_autopay_setup(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<Json<Value>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    let customer = ensure_stripe_customer(&ctx, stripe, user.user_id, &user.email).await?;
    let client = stripe_client()?;
    let success_url = ctx.config.absolute_url("/credits?autopay=saved");
    let cancel_url = ctx.config.absolute_url("/credits?autopay=cancelled");
    let form: [(&str, &str); 4] = [
        ("mode", "setup"),
        ("customer", &customer),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
    ];
    let response = client
        .post(checkout_sessions_url(stripe))
        .bearer_auth(&stripe.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "stripe setup session rejected"
        );
        return Err(StripeHttpError::CheckoutFailed);
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| StripeHttpError::CheckoutFailed)?;
    let Some(url) = body.get("url").and_then(Value::as_str) else {
        return Err(StripeHttpError::CheckoutFailed);
    };
    Ok(Json(serde_json::json!({ "url": url })))
}

#[derive(Debug, serde::Serialize)]
struct AutopayStatus {
    enabled: bool,
    threshold_usd: Option<Decimal>,
    topup_usd: Option<Decimal>,
    consecutive_failures: i32,
    card_setup_started: bool,
}

// GET /api/billing/autopay
async fn get_autopay(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<Json<AutopayStatus>, StripeHttpError> {
    let row = sqlx::query_as::<_, (bool, Option<Decimal>, Option<Decimal>, i32, Option<String>)>(
        r#"
        SELECT autopay_enabled, autopay_threshold_usd, autopay_topup_usd,
               autopay_consecutive_failures, stripe_customer_id
        FROM users WHERE id = $1
        "#,
    )
    .bind(user.user_id)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|_| StripeHttpError::BillingUnavailable)?;
    Ok(Json(AutopayStatus {
        enabled: row.0,
        threshold_usd: row.1,
        topup_usd: row.2,
        consecutive_failures: row.3,
        card_setup_started: row.4.is_some(),
    }))
}

#[derive(Debug, Deserialize)]
struct AutopayUpdate {
    enabled: bool,
    threshold_usd: Option<Decimal>,
    topup_usd: Option<Decimal>,
}

// PUT /api/billing/autopay
async fn put_autopay(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Json(update): Json<AutopayUpdate>,
) -> Result<Json<AutopayStatus>, StripeHttpError> {
    let Some(stripe) = ctx.config.stripe.as_ref() else {
        return Err(StripeHttpError::BillingUnavailable);
    };
    if update.enabled {
        let (Some(threshold), Some(topup)) = (update.threshold_usd, update.topup_usd) else {
            return Err(StripeHttpError::MalformedEvent);
        };
        // The top-up buys credits exactly like a manual checkout, so it
        // lives inside the same bounds; the threshold only needs to be a
        // sane non-negative trigger below the ceiling.
        validate_checkout_amount(topup, stripe).map_err(|_| StripeHttpError::MalformedEvent)?;
        if threshold < Decimal::ZERO || threshold > stripe.checkout_max_usd {
            return Err(StripeHttpError::MalformedEvent);
        }
        // A Stripe customer exists the moment setup STARTS; only a saved
        // card proves it finished. Enabling without one would burn the
        // three-strikes budget on a card that was never there (review
        // finding), so verify against Stripe at enable time.
        let customer = sqlx::query_scalar::<_, Option<String>>(
            "SELECT stripe_customer_id FROM users WHERE id = $1",
        )
        .bind(user.user_id)
        .fetch_one(&ctx.pool)
        .await
        .map_err(|_| StripeHttpError::BillingUnavailable)?;
        let Some(customer) = customer else {
            return Err(StripeHttpError::MalformedEvent);
        };
        let client = stripe_client()?;
        let methods: Value = client
            .get(format!(
                "{}/v1/customers/{customer}/payment_methods",
                stripe.api_base
            ))
            .query(&[("type", "card"), ("limit", "1")])
            .bearer_auth(&stripe.secret_key)
            .send()
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?
            .json()
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
        let has_card = methods
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| !data.is_empty());
        if !has_card {
            return Err(StripeHttpError::MalformedEvent);
        }
        let updated = sqlx::query(
            r#"
            UPDATE users
            SET autopay_enabled = TRUE,
                autopay_threshold_usd = $2,
                autopay_topup_usd = $3,
                autopay_consecutive_failures = 0
            WHERE id = $1 AND stripe_customer_id IS NOT NULL
            "#,
        )
        .bind(user.user_id)
        .bind(threshold)
        .bind(topup)
        .execute(&ctx.pool)
        .await
        .map_err(|_| StripeHttpError::BillingUnavailable)?
        .rows_affected();
        if updated == 0 {
            // No Stripe customer yet: card setup has not even started.
            return Err(StripeHttpError::MalformedEvent);
        }
    } else {
        sqlx::query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
            .bind(user.user_id)
            .execute(&ctx.pool)
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
    }
    get_autopay(State(ctx), user).await
}

/// `payment_intent.*` webhook arm. Only intents this router purposed as
/// autopay are consumed; everything else is acknowledged untouched. The
/// corroboration bar matches the checkout arm: the credited amount is the
/// money Stripe says it collected, and metadata must agree with it.
async fn handle_autopay_intent_event(
    ctx: &WebCtx,
    event: &StripeEvent,
) -> Result<Json<Value>, StripeHttpError> {
    let object = &event.data.object;
    let Some(intent_id) = object.get("id").and_then(Value::as_str) else {
        return Err(StripeHttpError::MalformedEvent);
    };
    let metadata = object.get("metadata");
    let purposed = metadata
        .and_then(|metadata| metadata.get("purpose"))
        .and_then(Value::as_str)
        == Some(AUTOPAY_PURPOSE);
    if !purposed {
        return Ok(received());
    }

    let stripe = ctx
        .config
        .stripe
        .as_ref()
        .ok_or(StripeHttpError::BillingUnavailable)?;
    let user_id_raw = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str);
    let credit_usd_raw = metadata
        .and_then(|metadata| metadata.get("credit_usd"))
        .and_then(Value::as_str);
    let provenance = metadata
        .and_then(|metadata| metadata.get("provenance"))
        .and_then(Value::as_str);
    let provenance_ok = match (user_id_raw, credit_usd_raw, provenance) {
        (Some(user), Some(credit), Some(mark)) => Uuid::parse_str(user).is_ok_and(|user| {
            constant_time_str_eq(mark, &autopay_provenance(stripe, user, credit))
        }),
        _ => false,
    };
    if !provenance_ok {
        // Purposed like ours but not provably ours: acknowledged untouched.
        // Metadata is writable by any integration sharing the Stripe
        // account; the HMAC is not.
        tracing::warn!(payment_intent = %intent_id, "autopay-shaped event without valid provenance; ignoring");
        return Ok(received());
    }

    if event.event_type == "payment_intent.payment_failed" {
        // Recovery applies to failures too: a decline whose sweep-side
        // record was lost must still exist as a terminal row, or the claim
        // slot and strike ledger drift from Stripe's reality.
        let user_id = user_id_raw.and_then(|raw| Uuid::parse_str(raw).ok());
        let credit_usd = credit_usd_raw.and_then(|raw| raw.parse::<Decimal>().ok());
        if let (Some(user_id), Some(credit_usd)) = (user_id, credit_usd) {
            billing::record_autopay_charge(&ctx.pool, intent_id, user_id, credit_usd)
                .await
                .map_err(|_| StripeHttpError::BillingUnavailable)?;
        }
        let handled = billing::fail_autopay_intent(&ctx.pool, intent_id)
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
        tracing::warn!(payment_intent = %intent_id, handled, "autopay charge failed");
        return Ok(received());
    }

    // payment_intent.succeeded
    let user_id = user_id_raw.and_then(|raw| Uuid::parse_str(raw).ok());
    let credit_usd = credit_usd_raw.and_then(|raw| raw.parse::<Decimal>().ok());
    let amount_received = object.get("amount_received").and_then(Value::as_i64);
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let (Some(user_id), Some(credit_usd), Some(amount_received), Some(currency)) =
        (user_id, credit_usd, amount_received, currency)
    else {
        tracing::warn!(payment_intent = %intent_id, "autopay success event missing corroboration fields");
        return Err(StripeHttpError::MalformedEvent);
    };
    let Some(claimed_cents) = usd_to_cents(credit_usd) else {
        return Err(StripeHttpError::MalformedEvent);
    };
    if claimed_cents != amount_received || currency != CHECKOUT_CURRENCY {
        tracing::error!(
            payment_intent = %intent_id,
            metadata_user_id = %user_id,
            claimed_cents,
            amount_received,
            %currency,
            "autopay success event does not corroborate its metadata; crediting nothing"
        );
        return Err(StripeHttpError::AmountMismatch);
    }
    let outcome = billing::settle_autopay_intent(&ctx.pool, intent_id, Some((user_id, credit_usd)))
        .await
        .map_err(|_| StripeHttpError::BillingUnavailable)?;
    tracing::info!(payment_intent = %intent_id, ?outcome, "autopay charge settled");
    Ok(received())
}

/// One sweep pass: reconcile stale pending charges, then find users under
/// their threshold and charge their saved card off-session. Public and
/// synchronous so tests drive the exact code production loops.
pub async fn run_autopay_sweep_once(pool: &crate::sqlx::PgPool, settings: &StripeSettings) {
    reconcile_stale_intents(pool, settings).await;
    let candidates = match billing::autopay_candidates(pool, AUTOPAY_SWEEP_BATCH).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(%error, "autopay sweep could not list candidates");
            return;
        }
    };
    for candidate in candidates {
        if let Err(error) = charge_candidate(pool, settings, &candidate).await {
            tracing::warn!(
                user_id = %candidate.user_id,
                %error,
                "autopay charge attempt failed"
            );
        }
    }
}

/// Pending rows older than the cutoff mean a webhook or a Stripe response
/// was lost. Local claims are retried against Stripe under their original
/// idempotency key (the same PaymentIntent answers, never a second
/// charge); real intents are queried by id and settled or failed by what
/// Stripe says actually happened. Without this pass, one lost message
/// wedges the user's one-pending-per-user slot forever.
async fn reconcile_stale_intents(pool: &crate::sqlx::PgPool, settings: &StripeSettings) {
    // Claims too old to replay safely are money in an unknown state: logged
    // loudly for an operator, never retried automatically.
    match billing::overdue_autopay_intents(pool, AUTOPAY_REPLAY_MAX_AGE_MINUTES).await {
        Ok(overdue) if !overdue.is_empty() => tracing::error!(
            count = overdue.len(),
            "autopay claims are older than the idempotency-retention window and need operator reconciliation"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not list overdue autopay claims"),
    }
    let stale = match billing::stale_autopay_intents(
        pool,
        AUTOPAY_RECONCILE_AFTER_MINUTES,
        AUTOPAY_REPLAY_MAX_AGE_MINUTES,
    )
    .await
    {
        Ok(stale) => stale,
        Err(error) => {
            tracing::warn!(%error, "autopay reconciliation could not list stale intents");
            return;
        }
    };
    for (intent_id, user_id, amount_usd) in stale {
        let outcome = if let Some(idempotency_key) = intent_id.strip_prefix("local_") {
            // Up to half an hour has passed since the claim was taken. If
            // the user has turned autopay off in the meantime, the claim is
            // released rather than replayed: nobody who has opted out gets
            // charged by a message we lost (sol review).
            match billing::autopay_still_armed(pool, user_id).await {
                Ok(false) => {
                    // Deliberately NOT deleted: a stranded claim may already
                    // have been charged, and its key is the only durable
                    // handle on that charge. Dropping it would lose the
                    // credit and free the slot for a second one (sol
                    // review). Stop replaying; leave it for reconciliation.
                    tracing::warn!(
                        %user_id,
                        "not replaying a stranded autopay claim: the user has opted out (the claim is kept — it may already have been charged)"
                    );
                    Ok(())
                }
                Ok(true) => {
                    replay_charge(pool, settings, user_id, amount_usd, idempotency_key).await
                }
                Err(error) => Err(anyhow::Error::from(error)),
            }
        } else {
            reconcile_real_intent(pool, settings, &intent_id, user_id, amount_usd).await
        };
        if let Err(error) = outcome {
            tracing::warn!(payment_intent = %intent_id, %error, "autopay reconciliation failed");
        }
    }
}

async fn reconcile_real_intent(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    intent_id: &str,
    user_id: Uuid,
    amount_usd: Decimal,
) -> anyhow::Result<()> {
    let client = stripe_client().map_err(|_| anyhow::anyhow!("stripe client"))?;
    let response = client
        .get(format!(
            "{}/v1/payment_intents/{intent_id}",
            settings.api_base
        ))
        .bearer_auth(&settings.secret_key)
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("stripe lookup rejected (HTTP {})", response.status());
    }
    let body: Value = response.json().await?;
    match body.get("status").and_then(Value::as_str) {
        Some("succeeded") => {
            billing::settle_autopay_intent(pool, intent_id, Some((user_id, amount_usd))).await?;
        }
        Some("processing") => {}
        _ => {
            billing::fail_autopay_intent(pool, intent_id).await?;
        }
    }
    Ok(())
}

async fn charge_candidate(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    candidate: &billing::AutopayCandidate,
) -> anyhow::Result<()> {
    // Claim BEFORE any money can move: the one-pending-per-user index makes
    // this exclusive, so overlapping sweeps cannot double-charge, and the
    // idempotency key survives in the claim row so a lost response is
    // replayed against the SAME PaymentIntent (review findings).
    let idempotency_key = Uuid::new_v4().simple().to_string();
    if !billing::claim_autopay_attempt(
        pool,
        candidate.user_id,
        candidate.topup_usd,
        &idempotency_key,
    )
    .await?
    {
        anyhow::bail!("user already has a charge in flight");
    }
    replay_charge(
        pool,
        settings,
        candidate.user_id,
        candidate.topup_usd,
        &idempotency_key,
    )
    .await
}

/// Create (or idempotently re-create) the off-session charge for a claim.
/// Shared by the first attempt and reconciliation replays; Stripe's
/// idempotency layer guarantees both paths observe one PaymentIntent.
async fn replay_charge(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    user_id: Uuid,
    topup_usd: Decimal,
    idempotency_key: &str,
) -> anyhow::Result<()> {
    let client = stripe_client().map_err(|_| anyhow::anyhow!("stripe client"))?;
    let Some(customer) = sqlx::query_scalar::<_, Option<String>>(
        "SELECT stripe_customer_id FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?
    else {
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("user has no stripe customer");
    };

    let response = client
        .get(format!(
            "{}/v1/customers/{customer}/payment_methods",
            settings.api_base
        ))
        .query(&[("type", "card"), ("limit", "1")])
        .bearer_auth(&settings.secret_key)
        .send()
        .await?;
    // A non-2xx here is Stripe failing to answer, NOT the user having no
    // card. Reading it as "no saved card" terminal-failed the claim and
    // freed the slot on a transient blip (sol review).
    if !response.status().is_success() {
        anyhow::bail!(
            "stripe could not list payment methods (HTTP {}); holding the claim",
            response.status()
        );
    }
    let methods: Value = response.json().await?;
    let Some(payment_method) = methods
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .and_then(|method| method.get("id"))
        .and_then(Value::as_str)
    else {
        // No saved card: the claim itself becomes the terminal failed
        // intent, which both counts the strike and releases the slot —
        // exactly once even under racing sweeps, because only the claim
        // holder reaches here.
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("no saved card payment method");
    };

    let Some(amount_cents) = usd_to_cents(topup_usd) else {
        billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
        anyhow::bail!("top-up amount is not a whole cent");
    };
    let amount = amount_cents.to_string();
    let user_id_text = user_id.to_string();
    let credit_usd = topup_usd.to_string();
    let provenance = autopay_provenance(settings, user_id, &credit_usd);
    let form: Vec<(&str, &str)> = vec![
        ("amount", &amount),
        ("currency", CHECKOUT_CURRENCY),
        ("customer", &customer),
        ("payment_method", payment_method),
        ("off_session", "true"),
        ("confirm", "true"),
        ("metadata[purpose]", AUTOPAY_PURPOSE),
        ("metadata[user_id]", &user_id_text),
        ("metadata[credit_usd]", &credit_usd),
        ("metadata[provenance]", &provenance),
    ];
    let response = client
        .post(format!("{}/v1/payment_intents", settings.api_base))
        .header("Idempotency-Key", idempotency_key)
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();

    if status.is_success() {
        let intent_id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("payment intent response missing id"))?;
        billing::attach_autopay_intent(pool, idempotency_key, intent_id).await?;
        if body.get("status").and_then(Value::as_str) == Some("succeeded") {
            billing::settle_autopay_intent(pool, intent_id, Some((user_id, topup_usd))).await?;
        }
        return Ok(());
    }

    // Whether the outcome is KNOWN is decided before anything is marked
    // failed — including when the body names a PaymentIntent.
    //
    // Stripe documents 5xx outcomes as indeterminate: a 500 naming an intent
    // may still be reported succeeded later, so failing that row leaves the
    // eventual webhook unable to credit it AND frees the slot for a second
    // charge. A 409 means a concurrent replay of the same idempotency key is
    // executing right now — the peer may be charging. Neither is terminal
    // (sol review of the first version of this fix, which got both wrong).
    let named_intent = body
        .get("error")
        .and_then(|error| error.get("payment_intent"))
        .and_then(|intent| intent.get("id"))
        .and_then(Value::as_str);
    let indeterminate = status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::CONFLICT;
    if indeterminate {
        // Attaching a named intent is still worth doing: its id is what lets
        // reconciliation ask Stripe what actually happened. The row stays
        // pending, so the slot stays held.
        if let Some(intent_id) = named_intent {
            billing::attach_autopay_intent(pool, idempotency_key, intent_id).await?;
        }
        tracing::warn!(
            %status,
            attached = named_intent.is_some(),
            "autopay charge outcome is indeterminate; the claim is held for reconciliation"
        );
        anyhow::bail!("stripe returned an indeterminate autopay outcome (HTTP {status})")
    }

    // A definitive rejection: Stripe understood the request and refused it.
    // Declines carry the created (failed) intent, so the strike counts
    // against a real intent and the slot frees.
    if let Some(intent_id) = named_intent {
        billing::attach_autopay_intent(pool, idempotency_key, intent_id).await?;
        billing::fail_autopay_intent(pool, intent_id).await?;
        anyhow::bail!("stripe declined the off-session charge (HTTP {status})")
    }

    // Rejected with no intent named: nothing was created, so the claim
    // itself becomes the terminal failure — the strike counts and the slot
    // frees.
    billing::fail_autopay_intent(pool, &format!("local_{idempotency_key}")).await?;
    anyhow::bail!("stripe rejected the off-session charge (HTTP {status})")
}

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
    for candidate in parsed.candidates {
        // A non-hex candidate can never match; skip it rather than abort so a
        // valid sibling signature (e.g. during secret rotation) still passes.
        let Ok(candidate_bytes) = hex::decode(candidate) else {
            continue;
        };
        // HMAC-SHA256 accepts keys of any length, so construction cannot
        // fail; if it somehow does, fail closed.
        let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
            return Err(WebhookVerifyError::SignatureMismatch);
        };
        // Sign the exact timestamp string from the header, not a re-rendered
        // integer, so byte-level oddities cannot desynchronize the digest.
        mac.update(parsed.timestamp_raw.as_bytes());
        mac.update(b".");
        mac.update(payload);
        if mac.verify_slice(&candidate_bytes).is_ok() {
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

fn parse_signature_header(header: &str) -> Result<ParsedSignatureHeader<'_>, WebhookVerifyError> {
    let mut timestamp_raw = None;
    let mut candidates = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => timestamp_raw = Some(value.trim()),
            "v1" => candidates.push(value.trim()),
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
        let parsed = parse_signature_header("t=1700000000,v1=aa,v0=bb,v1=cc")
            .expect("well-formed header must parse");
        assert_eq!(parsed.timestamp, 1_700_000_000);
        assert_eq!(parsed.timestamp_raw, "1700000000");
        assert_eq!(parsed.candidates, vec!["aa", "cc"]);
        for header in ["", "garbage", "t=notanumber,v1=aa", "v1=aa", "t=1700000000"] {
            assert_eq!(
                parse_signature_header(header).err(),
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

    if event.event_type == "payment_intent.payment_failed" {
        let handled = billing::fail_autopay_intent(&ctx.pool, intent_id)
            .await
            .map_err(|_| StripeHttpError::BillingUnavailable)?;
        tracing::warn!(payment_intent = %intent_id, handled, "autopay charge failed");
        return Ok(received());
    }

    // payment_intent.succeeded
    let user_id = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok());
    let credit_usd = metadata
        .and_then(|metadata| metadata.get("credit_usd"))
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<Decimal>().ok());
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

/// One sweep pass: find users under their threshold and charge their saved
/// card off-session. Public and synchronous so tests drive the exact code
/// production loops; the loop itself is `RouterState::spawn_autopay_sweep`.
pub async fn run_autopay_sweep_once(pool: &crate::sqlx::PgPool, settings: &StripeSettings) {
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

async fn charge_candidate(
    pool: &crate::sqlx::PgPool,
    settings: &StripeSettings,
    candidate: &billing::AutopayCandidate,
) -> anyhow::Result<()> {
    let client = stripe_client().map_err(|_| anyhow::anyhow!("stripe client"))?;
    // The saved card: newest attached card payment method.
    let response = client
        .get(format!(
            "{}/v1/customers/{}/payment_methods",
            settings.api_base, candidate.stripe_customer_id
        ))
        .query(&[("type", "card"), ("limit", "1")])
        .bearer_auth(&settings.secret_key)
        .send()
        .await?;
    let methods: Value = response.json().await?;
    let Some(payment_method) = methods
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .and_then(|method| method.get("id"))
        .and_then(Value::as_str)
    else {
        // Enabled but no card saved (setup session abandoned): count it as
        // a failed attempt so three sweeps disable autopay instead of
        // probing Stripe forever.
        billing::bump_autopay_failure(pool, candidate.user_id).await?;
        anyhow::bail!("no saved card payment method");
    };

    let Some(amount_cents) = usd_to_cents(candidate.topup_usd) else {
        billing::bump_autopay_failure(pool, candidate.user_id).await?;
        anyhow::bail!("top-up amount is not a whole cent");
    };
    let amount = amount_cents.to_string();
    let user_id = candidate.user_id.to_string();
    let credit_usd = candidate.topup_usd.to_string();
    let idempotency_key = Uuid::new_v4().to_string();
    let form: [(&str, &str); 8] = [
        ("amount", &amount),
        ("currency", CHECKOUT_CURRENCY),
        ("customer", &candidate.stripe_customer_id),
        ("payment_method", payment_method),
        ("off_session", "true"),
        ("confirm", "true"),
        ("metadata[purpose]", AUTOPAY_PURPOSE),
        ("metadata[user_id]", &user_id),
    ];
    let mut form: Vec<(&str, &str)> = form.to_vec();
    form.push(("metadata[credit_usd]", &credit_usd));
    let response = client
        .post(format!("{}/v1/payment_intents", settings.api_base))
        .header("Idempotency-Key", &idempotency_key)
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
        billing::record_autopay_intent(pool, intent_id, candidate.user_id, candidate.topup_usd)
            .await?;
        // A synchronously succeeded card charge is credited immediately;
        // the webhook's replay lands on AlreadySettled. Any other status
        // (processing) stays pending for the webhook to settle.
        if body.get("status").and_then(Value::as_str) == Some("succeeded") {
            billing::settle_autopay_intent(
                pool,
                intent_id,
                Some((candidate.user_id, candidate.topup_usd)),
            )
            .await?;
        }
        return Ok(());
    }

    // Declines arrive as an error carrying the created (failed) intent.
    // Record it and mark it failed so the failure counter moves and the
    // pending-guard never wedges.
    if let Some(intent_id) = body
        .get("error")
        .and_then(|error| error.get("payment_intent"))
        .and_then(|intent| intent.get("id"))
        .and_then(Value::as_str)
    {
        billing::record_autopay_intent(pool, intent_id, candidate.user_id, candidate.topup_usd)
            .await?;
        billing::fail_autopay_intent(pool, intent_id).await?;
    } else {
        billing::bump_autopay_failure(pool, candidate.user_id).await?;
    }
    anyhow::bail!("stripe declined the off-session charge (HTTP {status})");
}

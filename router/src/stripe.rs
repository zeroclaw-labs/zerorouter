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
const STRIPE_CHECKOUT_SESSIONS_URL: &str = "https://api.stripe.com/v1/checkout/sessions";
const STRIPE_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CHECKOUT_COMPLETED_EVENT: &str = "checkout.session.completed";
const CHECKOUT_PRODUCT_NAME: &str = "ZeroRouter credits";
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
        .route("/webhooks/stripe", post(stripe_webhook))
}

#[derive(Debug)]
enum StripeHttpError {
    BillingUnavailable,
    InvalidAmount,
    CheckoutFailed,
    InvalidSignature,
    MalformedEvent,
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
    if amount_usd.normalize().scale() > 2 {
        return Err(StripeHttpError::InvalidAmount);
    }
    if amount_usd < settings.checkout_min_usd || amount_usd > settings.checkout_max_usd {
        return Err(StripeHttpError::InvalidAmount);
    }
    (amount_usd * Decimal::ONE_HUNDRED)
        .normalize()
        .to_i64()
        .ok_or(StripeHttpError::InvalidAmount)
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
        ("line_items[0][price_data][currency]", "usd"),
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
        .post(STRIPE_CHECKOUT_SESSIONS_URL)
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
    if event.event_type != CHECKOUT_COMPLETED_EVENT {
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

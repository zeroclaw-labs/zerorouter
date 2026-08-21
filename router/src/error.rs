use std::borrow::Cow;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    InvalidRequest,
    /// The typed `zerorouter.priority` and the model-suffix carrier are both
    /// present and disagree. A conflict is a client bug and is refused loudly
    /// rather than resolved by precedence (design doc: "Precedence and
    /// conflicts").
    PriorityConflict,
    CacheControlUnsupported,
    PayloadTooLarge,
    /// The router is already buffering as many request bodies as it will
    /// hold. Shedding here is deliberate: the alternative is queueing
    /// unboundedly and dying with everyone's request in flight.
    Overloaded,
    /// The client did not finish sending its request body in time.
    RequestTimeout,
    UnsupportedRequestFields,
    /// The request carries an input modality the requested model does not
    /// take — an image sent to a text-only lane.
    ///
    /// Its own code rather than a reuse of [`Self::UnsupportedRequestFields`],
    /// and the reason is the one [`Self::AccountFrozen`] gives for not reusing
    /// `InsufficientCredits`: the remedy differs and a caller acts on the
    /// code. `unsupported_request_fields` says "this router cannot carry that
    /// shape anywhere", and the fix is to stop sending it. This one says "that
    /// shape is fine, this MODEL does not take it", and the fix is to pick a
    /// different model — so the message names the model and lists what it does
    /// accept, which is the information needed to choose one.
    ///
    /// Refused before any reservation is taken or any upstream dialled, on the
    /// same principle as [`Self::PriorityConflict`]: a request that cannot be
    /// served must not move money first.
    ModalityUnsupported {
        model: String,
        modality: &'static str,
        accepted: String,
    },
    Unauthorized,
    SpendCapExceeded,
    /// The presenting key has spent the credit limit its owner set on it
    /// (migration 0023), for the current reset window.
    ///
    /// Its own code rather than a reuse of [`Self::SpendCapExceeded`] or
    /// [`Self::InsufficientCredits`], because the remedy differs and a caller
    /// acts on the code: this one clears when the key's own window resets, not
    /// by buying credit (the balance is untouched) and not by asking the
    /// operator to raise a ceiling (this limit is the customer's own).
    KeyCreditLimitExceeded,
    InsufficientCredits,
    /// The account is frozen (migration 0009): a Stripe chargeback, or an
    /// operator's hold. Deliberately its own code rather than a reuse of
    /// [`Self::InsufficientCredits`] — buying more credit does not clear it,
    /// and a client told "insufficient credits" would reasonably try.
    AccountFrozen,
    VelocityCapExceeded,
    ModelNotFound,
    /// The requested model is in the catalog file but its tier is withheld for
    /// below-cost pricing. Distinct from [`ApiError::ModelNotFound`] (the model
    /// exists) and from [`ApiError::TierCatalogUnavailable`] (that one is a
    /// whole-catalog fault, and reads as transient).
    ModelUnavailable {
        tier: String,
    },
    TierCatalogUnavailable,
    DatabaseUnavailable,
    NoProviderAvailable,
    UpstreamUnavailable,
    /// An upstream answered without attesting the zero-retention guarantee the
    /// requested lane is sold under, so the request was refused rather than
    /// served ([`crate::wire::ResponseAttestation`]).
    ///
    /// Its own code rather than a reuse of [`Self::UpstreamUnavailable`], and
    /// the reason is the same one [`Self::AccountFrozen`] gives for not reusing
    /// `InsufficientCredits`: the remedy differs and a caller acts on the code.
    /// "All upstream inference candidates failed" invites a retry, and a retry
    /// is the one thing that must not happen here — it would deliver the prompt
    /// again to an upstream that has just declined to say it will not keep it.
    /// A customer who chose a zero-retention lane deliberately is also owed the
    /// real reason: this is the router refusing to serve them under a weaker
    /// guarantee than the one they bought, which is a materially different
    /// event from an upstream being down.
    RetentionAttestationFailed,
    UpstreamTimeout,
    ServerShuttingDown,
    MeteringUnavailable,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    // Borrowed for every fixed message; owned only where the body has to name
    // the thing that is broken (see `ApiError::ModelUnavailable`).
    message: Cow<'static, str>,
    r#type: &'static str,
    param: Option<&'static str>,
    code: &'static str,
}

impl ApiError {
    fn response_parts(
        &self,
    ) -> (
        StatusCode,
        Cow<'static, str>,
        &'static str,
        Option<&'static str>,
        &'static str,
    ) {
        match self {
            Self::InvalidRequest => (
                StatusCode::BAD_REQUEST,
                Cow::Borrowed("The request body is not a valid chat completion request."),
                "invalid_request_error",
                None,
                "invalid_request",
            ),
            Self::PriorityConflict => (
                StatusCode::BAD_REQUEST,
                Cow::Borrowed(
                    "zerorouter.priority and the model-name priority suffix disagree; send one, or the same value in both.",
                ),
                "invalid_request_error",
                Some("zerorouter.priority"),
                "priority_conflict",
            ),
            Self::CacheControlUnsupported => (
                StatusCode::BAD_REQUEST,
                Cow::Borrowed(
                    "Client cache_control passthrough is not supported by the pinned provider interface.",
                ),
                "invalid_request_error",
                Some("messages"),
                "cache_control_unsupported",
            ),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                Cow::Borrowed("The request body exceeds the ZeroRouter size limit."),
                "invalid_request_error",
                None,
                "request_too_large",
            ),
            Self::Overloaded => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Borrowed("The router is at capacity for in-flight requests; retry shortly."),
                "server_error",
                None,
                "server_overloaded",
            ),
            Self::RequestTimeout => (
                StatusCode::REQUEST_TIMEOUT,
                Cow::Borrowed("The request body was not delivered in time."),
                "invalid_request_error",
                None,
                "request_timeout",
            ),
            Self::UnsupportedRequestFields => (
                StatusCode::BAD_REQUEST,
                Cow::Borrowed(
                    "The request contains fields or structured content that the pinned provider interface cannot preserve.",
                ),
                "invalid_request_error",
                None,
                "unsupported_request_fields",
            ),
            // 400 and `invalid_request_error`: unlike `model_unavailable`,
            // nothing here is ZeroRouter's misconfiguration — the caller
            // asked a model for something it does not do, and can fix it by
            // asking a different one. `param` is `messages` rather than
            // `model` because that is where the offending content sits, and a
            // client that highlights the named field should highlight the
            // image, not the model id it may have chosen deliberately.
            Self::ModalityUnsupported {
                model,
                modality,
                accepted,
            } => (
                StatusCode::BAD_REQUEST,
                Cow::Owned(format!(
                    "The model {model} does not accept {modality} input. It accepts: {accepted}. \
                     Nothing was reserved and no upstream was contacted. Send this content to a \
                     model whose input_modalities include {modality} — GET /v1/models lists what \
                     each one takes."
                )),
                "invalid_request_error",
                Some("messages"),
                "modality_unsupported",
            ),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Cow::Borrowed("The supplied ZeroRouter API key is invalid or disabled."),
                "authentication_error",
                None,
                "invalid_api_key",
            ),
            Self::SpendCapExceeded => (
                StatusCode::PAYMENT_REQUIRED,
                Cow::Borrowed("This API key has reached its monthly spend cap."),
                "billing_error",
                None,
                "spend_cap_exceeded",
            ),
            Self::KeyCreditLimitExceeded => (
                StatusCode::PAYMENT_REQUIRED,
                Cow::Borrowed(
                    "This API key has reached the credit limit set on it. \
                     The account balance is unaffected; the limit resets on \
                     the key's own schedule.",
                ),
                "billing_error",
                None,
                "key_credit_limit_exceeded",
            ),
            Self::InsufficientCredits => (
                StatusCode::PAYMENT_REQUIRED,
                Cow::Borrowed("This account has insufficient prepaid credits for the request."),
                "billing_error",
                None,
                "insufficient_credits",
            ),
            // 402 and not 403: this is the billing family, and the account is
            // frozen over money. The message names the freeze and points at
            // the only thing that clears it — a human — so a client does not
            // retry, top up, or mint a new key hoping one of them helps.
            Self::AccountFrozen => (
                StatusCode::PAYMENT_REQUIRED,
                Cow::Borrowed(
                    "This account is frozen and cannot be used for new requests. A frozen account \
                     is usually the result of a payment dispute or chargeback; adding credit will \
                     not lift it. Contact ZeroRouter support to have the freeze reviewed.",
                ),
                "billing_error",
                None,
                "account_frozen",
            ),
            Self::VelocityCapExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                Cow::Borrowed("This API key has reached its token-per-minute cap."),
                "rate_limit_error",
                None,
                "velocity_cap_exceeded",
            ),
            Self::ModelNotFound => (
                StatusCode::NOT_FOUND,
                Cow::Borrowed("The requested model is not present in the ZeroRouter catalog."),
                "invalid_request_error",
                Some("model"),
                "model_not_found",
            ),
            // 503 rather than 404: the model is in the catalog file, so it is
            // not the caller's request that is wrong. Its own `code` and a
            // message that names the tier keep it distinguishable from the
            // transient `tier_catalog_unavailable` — this one does not clear on
            // its own, and the body says so instead of inviting a retry loop.
            // The concrete cost basis and sell rate stay in the operator log;
            // a customer is told what is broken, not what ZeroRouter pays.
            Self::ModelUnavailable { tier } => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Owned(format!(
                    "The requested model is not being served: its tier {tier} is configured below \
                     its own cost basis, so every request it took would lose money. This is a \
                     ZeroRouter pricing misconfiguration, not a transient outage — retrying will \
                     not clear it. Every other model in the catalog is unaffected."
                )),
                "server_error",
                Some("model"),
                "model_unavailable",
            ),
            Self::TierCatalogUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Borrowed("The model catalog is temporarily unavailable."),
                "server_error",
                None,
                "tier_catalog_unavailable",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Borrowed("Authentication is temporarily unavailable."),
                "server_error",
                None,
                "database_unavailable",
            ),
            Self::NoProviderAvailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Borrowed("No configured upstream provider is currently available."),
                "server_error",
                None,
                "no_provider_available",
            ),
            Self::UpstreamUnavailable => (
                StatusCode::BAD_GATEWAY,
                Cow::Borrowed("All upstream inference candidates failed."),
                "server_error",
                None,
                "upstream_unavailable",
            ),
            // 502 and in the `server_error` family: nothing about the caller's
            // request is wrong, and the fault is upstream of ZeroRouter. The
            // message says what was not honoured, that nothing was served, and
            // that retrying will not clear it — because the cause is a setting
            // on the operator's account with the provider, which only the
            // operator can put right. It deliberately does NOT name the header
            // or the provider: a customer is told what guarantee was not met,
            // not which vendor sits behind the lane or how the check is
            // implemented. Those are in the operator's log, at ERROR.
            Self::RetentionAttestationFailed => (
                StatusCode::BAD_GATEWAY,
                Cow::Borrowed(
                    "The upstream serving this model did not confirm the zero-data-retention \
                     guarantee this lane is sold under, so ZeroRouter refused the request instead \
                     of serving it. Nothing was sent to you and nothing was billed. This is a \
                     ZeroRouter provider-configuration fault, not a transient outage — retrying \
                     will not clear it. Every model on a different provider is unaffected.",
                ),
                "server_error",
                None,
                "retention_attestation_failed",
            ),
            Self::UpstreamTimeout => (
                StatusCode::GATEWAY_TIMEOUT,
                Cow::Borrowed("The upstream inference deadline was exceeded."),
                "server_error",
                None,
                "upstream_timeout",
            ),
            Self::ServerShuttingDown => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Borrowed("The router is shutting down; retry the request."),
                "server_error",
                None,
                "server_shutting_down",
            ),
            Self::MeteringUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                Cow::Borrowed("The request completed upstream but could not be metered safely."),
                "server_error",
                None,
                "metering_unavailable",
            ),
        }
    }

    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.response_parts().0
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message, error_type, param, code) = self.response_parts();
        (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    message,
                    r#type: error_type,
                    param,
                    code,
                },
            }),
        )
            .into_response()
    }
}

#[must_use]
pub fn streaming_error_json(error: &ApiError) -> String {
    let (_, message, error_type, param, code) = error.response_parts();
    serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
            "param": param,
            "code": code,
        }
    })
    .to_string()
}

#[cfg(test)]
mod pre_admission_tests {
    use super::*;

    /// The two shed responses a caller can meet before any billing check:
    /// both must be honest about being the router's limit rather than the
    /// caller's mistake, so a client can retry intelligently.
    #[test]
    fn overload_and_body_timeout_report_retryable_server_conditions() {
        let (status, _, kind, _, code) = ApiError::Overloaded.response_parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind, "server_error");
        assert_eq!(code, "server_overloaded");

        let (status, _, _, _, code) = ApiError::RequestTimeout.response_parts();
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(code, "request_timeout");
    }
}

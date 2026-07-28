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
    UnsupportedRequestFields,
    Unauthorized,
    SpendCapExceeded,
    InsufficientCredits,
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
            Self::UnsupportedRequestFields => (
                StatusCode::BAD_REQUEST,
                Cow::Borrowed(
                    "The request contains fields or structured content that the pinned provider interface cannot preserve.",
                ),
                "invalid_request_error",
                None,
                "unsupported_request_fields",
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
            Self::InsufficientCredits => (
                StatusCode::PAYMENT_REQUIRED,
                Cow::Borrowed("This account has insufficient prepaid credits for the request."),
                "billing_error",
                None,
                "insufficient_credits",
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

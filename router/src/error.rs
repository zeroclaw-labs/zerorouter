use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    InvalidRequest,
    CacheControlUnsupported,
    PayloadTooLarge,
    UnsupportedRequestFields,
    Unauthorized,
    SpendCapExceeded,
    InsufficientCredits,
    VelocityCapExceeded,
    ModelNotFound,
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
    message: &'static str,
    r#type: &'static str,
    param: Option<&'static str>,
    code: &'static str,
}

impl ApiError {
    fn response_parts(
        &self,
    ) -> (
        StatusCode,
        &'static str,
        &'static str,
        Option<&'static str>,
        &'static str,
    ) {
        match self {
            Self::InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "The request body is not a valid chat completion request.",
                "invalid_request_error",
                None,
                "invalid_request",
            ),
            Self::CacheControlUnsupported => (
                StatusCode::BAD_REQUEST,
                "Client cache_control passthrough is not supported by the pinned provider interface.",
                "invalid_request_error",
                Some("messages"),
                "cache_control_unsupported",
            ),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "The request body exceeds the ZeroRouter size limit.",
                "invalid_request_error",
                None,
                "request_too_large",
            ),
            Self::UnsupportedRequestFields => (
                StatusCode::BAD_REQUEST,
                "The request contains fields or structured content that the pinned provider interface cannot preserve.",
                "invalid_request_error",
                None,
                "unsupported_request_fields",
            ),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "The supplied ZeroRouter API key is invalid or disabled.",
                "authentication_error",
                None,
                "invalid_api_key",
            ),
            Self::SpendCapExceeded => (
                StatusCode::PAYMENT_REQUIRED,
                "This API key has reached its monthly spend cap.",
                "billing_error",
                None,
                "spend_cap_exceeded",
            ),
            Self::InsufficientCredits => (
                StatusCode::PAYMENT_REQUIRED,
                "This account has insufficient prepaid credits for the request.",
                "billing_error",
                None,
                "insufficient_credits",
            ),
            Self::VelocityCapExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "This API key has reached its token-per-minute cap.",
                "rate_limit_error",
                None,
                "velocity_cap_exceeded",
            ),
            Self::ModelNotFound => (
                StatusCode::NOT_FOUND,
                "The requested model is not present in the ZeroRouter catalog.",
                "invalid_request_error",
                Some("model"),
                "model_not_found",
            ),
            Self::TierCatalogUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The model catalog is temporarily unavailable.",
                "server_error",
                None,
                "tier_catalog_unavailable",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Authentication is temporarily unavailable.",
                "server_error",
                None,
                "database_unavailable",
            ),
            Self::NoProviderAvailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "No configured upstream provider is currently available.",
                "server_error",
                None,
                "no_provider_available",
            ),
            Self::UpstreamUnavailable => (
                StatusCode::BAD_GATEWAY,
                "All upstream inference candidates failed.",
                "server_error",
                None,
                "upstream_unavailable",
            ),
            Self::UpstreamTimeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "The upstream inference deadline was exceeded.",
                "server_error",
                None,
                "upstream_timeout",
            ),
            Self::ServerShuttingDown => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The router is shutting down; retry the request.",
                "server_error",
                None,
                "server_shutting_down",
            ),
            Self::MeteringUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The request completed upstream but could not be metered safely.",
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

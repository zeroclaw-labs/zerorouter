//! Placeholder module; implemented by the corresponding build stage.

use axum::Router;

use crate::web::WebCtx;

#[must_use]
pub fn router() -> Router<WebCtx> {
    Router::new()
}

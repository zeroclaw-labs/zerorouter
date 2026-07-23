//! Portal sessions: opaque bearer tokens stored hashed, delivered as an
//! `HttpOnly` cookie, and resolved through the [`PortalUser`] extractor.
//!
//! The session token never rests in the database — only its SHA-256 digest —
//! and mutating requests must carry the `x-zerorouter-portal` header so a
//! cross-site form post (which cannot set custom headers) is rejected even if
//! a browser attaches the cookie.

use axum::{
    Json,
    http::{HeaderValue, Method, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    sqlx::{self, PgPool},
    web::WebCtx,
};

pub const SESSION_COOKIE: &str = "zr_session";
pub const CSRF_HEADER: &str = "x-zerorouter-portal";
const SESSION_TOKEN_PREFIX: &str = "zcs_";
const SESSION_TOKEN_BYTES: usize = 32;

/// An authenticated portal user, resolved from the session cookie.
#[derive(Clone, Debug)]
pub struct PortalUser {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub email: String,
}

#[derive(Debug)]
pub enum SessionRejection {
    Unauthenticated,
    CsrfRejected,
    DatabaseUnavailable,
}

impl IntoResponse for SessionRejection {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                "Sign in to use the ZeroRouter portal.",
                "session_required",
            ),
            Self::CsrfRejected => (
                StatusCode::FORBIDDEN,
                "The request is missing the portal request header.",
                "csrf_rejected",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Session validation is temporarily unavailable.",
                "database_unavailable",
            ),
        };
        (
            status,
            Json(serde_json::json!({
                "error": { "message": message, "type": "portal_error", "code": code }
            })),
        )
            .into_response()
    }
}

impl axum::extract::FromRequestParts<WebCtx> for PortalUser {
    type Rejection = SessionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        ctx: &WebCtx,
    ) -> Result<Self, Self::Rejection> {
        if !matches!(parts.method, Method::GET | Method::HEAD)
            && !parts.headers.contains_key(CSRF_HEADER)
        {
            return Err(SessionRejection::CsrfRejected);
        }
        let token = session_cookie_value(parts).ok_or(SessionRejection::Unauthenticated)?;
        resolve_session(&ctx.pool, &token)
            .await
            .map_err(|_| SessionRejection::DatabaseUnavailable)?
            .ok_or(SessionRejection::Unauthenticated)
    }
}

fn session_cookie_value(parts: &Parts) -> Option<String> {
    for header_value in parts.headers.get_all(header::COOKIE) {
        let raw = header_value.to_str().ok()?;
        for pair in raw.split(';') {
            let mut split = pair.trim().splitn(2, '=');
            let name = split.next()?.trim();
            if name == SESSION_COOKIE {
                let value = split.next()?.trim();
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

async fn resolve_session(
    pool: &PgPool,
    token: &str,
) -> Result<Option<PortalUser>, sqlx::Error> {
    if !valid_session_token_shape(token) {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT portal_sessions.id, users.id, users.email
        FROM portal_sessions
        INNER JOIN users ON users.id = portal_sessions.user_id
        WHERE portal_sessions.token_hash = $1
          AND portal_sessions.expires_at > NOW()
          AND portal_sessions.revoked_at IS NULL
        "#,
    )
    .bind(hash_session_token(token))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(session_id, user_id, email)| PortalUser {
        user_id,
        session_id,
        email,
    }))
}

/// Create a session and return the plaintext token exactly once.
pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    ttl: std::time::Duration,
) -> Result<(String, DateTime<Utc>), sqlx::Error> {
    let token = generate_session_token();
    let ttl_seconds = i64::try_from(ttl.as_secs())
        .map_err(|_| sqlx::Error::Protocol("session lifetime exceeds the database range".into()))?;
    let expires_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"
        INSERT INTO portal_sessions (id, user_id, token_hash, expires_at)
        VALUES ($1, $2, $3, NOW() + ($4 * INTERVAL '1 second'))
        RETURNING expires_at
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hash_session_token(&token))
    .bind(ttl_seconds)
    .fetch_one(pool)
    .await?;
    Ok((token, expires_at))
}

pub async fn revoke_session(pool: &PgPool, session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE portal_sessions SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete expired and revoked sessions; safe to run opportunistically.
pub async fn prune_sessions(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM portal_sessions WHERE expires_at <= NOW() OR revoked_at IS NOT NULL")
        .execute(pool)
        .await?;
    Ok(())
}

/// `Set-Cookie` value carrying the session token.
#[must_use]
pub fn session_cookie_header(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    build_cookie(token, max_age_secs, secure)
}

/// `Set-Cookie` value that clears the session cookie.
#[must_use]
pub fn clear_session_cookie_header(secure: bool) -> HeaderValue {
    build_cookie("", 0, secure)
}

fn build_cookie(value: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let secure_attr = if secure { "; Secure" } else { "" };
    let cookie =
        format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure_attr}");
    HeaderValue::from_str(&cookie)
        .unwrap_or_else(|_| HeaderValue::from_static("zr_session=; Path=/; HttpOnly; Max-Age=0"))
}

fn generate_session_token() -> String {
    let bytes: [u8; SESSION_TOKEN_BYTES] = rand::random();
    format!("{SESSION_TOKEN_PREFIX}{}", hex::encode(bytes))
}

fn valid_session_token_shape(token: &str) -> bool {
    token.len() == SESSION_TOKEN_PREFIX.len() + SESSION_TOKEN_BYTES * 2
        && token.starts_with(SESSION_TOKEN_PREFIX)
        && token[SESSION_TOKEN_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn hash_session_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_have_a_valid_shape() {
        let token = generate_session_token();
        assert!(valid_session_token_shape(&token));
        assert_ne!(token, generate_session_token());
    }

    #[test]
    fn foreign_tokens_are_rejected_by_shape() {
        assert!(!valid_session_token_shape(""));
        assert!(!valid_session_token_shape("zcr_deadbeef"));
        let mut uppercase = generate_session_token();
        uppercase.truncate(SESSION_TOKEN_PREFIX.len());
        uppercase.push_str(&"A".repeat(SESSION_TOKEN_BYTES * 2));
        assert!(!valid_session_token_shape(&uppercase));
    }

    #[test]
    fn cookie_headers_carry_security_attributes() {
        let header = session_cookie_header("zcs_abc", 60, true);
        let text = header.to_str().expect("cookie header should be ascii");
        assert!(text.contains("HttpOnly"));
        assert!(text.contains("SameSite=Lax"));
        assert!(text.contains("Secure"));
        let cleared = clear_session_cookie_header(false);
        let cleared_text = cleared.to_str().expect("cookie header should be ascii");
        assert!(cleared_text.contains("Max-Age=0"));
        assert!(!cleared_text.contains("Secure"));
    }
}

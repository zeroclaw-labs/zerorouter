//! RFC 8628 device-authorization grant whose end state is a freshly minted
//! `zcr_` inference API key — this is how ZeroClaw logs in.
//!
//! Plaintext credentials never rest in the database: the device code is
//! stored only as a SHA-256 digest, and the API key is minted at claim time
//! (the token poll after approval) inside a single transaction, so the
//! plaintext key exists exactly once, in the successful token response.
//!
//! The claim mint is a self-service mint and carries the same per-user key
//! limits as the portal's ([`crate::db::admit_key_mint`]); a claim refused by
//! them reports `access_denied`.
//!
//! Endpoints:
//! - `POST /auth/device/code` — start a grant (form or JSON body).
//! - `POST /auth/device/token` — poll for the key (form or JSON body).
//! - `GET /.well-known/openid-configuration` — discovery document.
//! - `POST /api/device/{lookup,approve,deny}` — portal-side approval,
//!   authenticated by [`PortalUser`] (which also enforces the CSRF header).

use axum::{
    Json, Router,
    extract::{Form, FromRequest, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    auth::{generate_api_key, hash_api_key},
    db::{KeyMintAdmission, admit_key_mint},
    session::PortalUser,
    sqlx::{self, PgPool},
    web::WebCtx,
};

const DEVICE_CODE_PREFIX: &str = "zdc_";
const DEVICE_CODE_BYTES: usize = 32;
const DEVICE_CODE_TTL_SECS: i64 = 900;
const DEVICE_POLL_INTERVAL_SECS: i32 = 5;
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_KEY_NAME: &str = "zeroclaw device";
/// Characters that survive being read off a screen and typed on a phone:
/// no vowels (no accidental words), no `0/O`, `1/I/L`, or other lookalikes.
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKMPQRSTVWXZ23456789";
const USER_CODE_CHARS: usize = 8;
const USER_CODE_GROUP: usize = 4;
/// A `user_code` collision is astronomically unlikely (26^8 codes); retry a
/// handful of times rather than surfacing the unique violation.
const USER_CODE_INSERT_ATTEMPTS: usize = 8;

pub fn router() -> Router<WebCtx> {
    Router::new()
        .route("/auth/device/code", post(start_device_authorization))
        .route("/auth/device/token", post(poll_device_token))
        .route(
            "/.well-known/openid-configuration",
            get(openid_configuration),
        )
        .route("/api/device/lookup", post(lookup_user_code))
        .route("/api/device/approve", post(approve_user_code))
        .route("/api/device/deny", post(deny_user_code))
}

/// RFC 6749 §5.2 / RFC 8628 §3.5 error response: `{"error": "<code>"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OauthError {
    InvalidRequest,
    InvalidClient,
    UnsupportedGrantType,
    InvalidGrant,
    ExpiredToken,
    AccessDenied,
    AuthorizationPending,
    SlowDown,
    ServerError,
}

impl OauthError {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidClient => "invalid_client",
            Self::UnsupportedGrantType => "unsupported_grant_type",
            Self::InvalidGrant => "invalid_grant",
            Self::ExpiredToken => "expired_token",
            Self::AccessDenied => "access_denied",
            Self::AuthorizationPending => "authorization_pending",
            Self::SlowDown => "slow_down",
            Self::ServerError => "server_error",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::ServerError => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl IntoResponse for OauthError {
    fn into_response(self) -> Response {
        (
            self.status(),
            Json(serde_json::json!({ "error": self.code() })),
        )
            .into_response()
    }
}

/// Portal-side errors, rendered in the OpenAI-shaped envelope the rest of
/// the portal API uses.
#[derive(Debug, Clone, Copy)]
enum PortalDeviceError {
    InvalidBody,
    NotFound,
    DatabaseUnavailable,
}

impl IntoResponse for PortalDeviceError {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            Self::InvalidBody => (
                StatusCode::BAD_REQUEST,
                "The request body is not a valid device authorization action.",
                "invalid_request",
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "That code was not found, has expired, or has already been handled.",
                "device_code_not_found",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Device authorization is temporarily unavailable.",
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

/// Accept both `application/x-www-form-urlencoded` (what RFC 8628 clients
/// send) and `application/json`; anything else is rejected, fail-closed.
struct FormOrJson<T>(T);

impl<T> FromRequest<WebCtx> for FormOrJson<T>
where
    T: DeserializeOwned,
{
    type Rejection = OauthError;

    async fn from_request(request: Request, state: &WebCtx) -> Result<Self, Self::Rejection> {
        let mime = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if mime == "application/json" {
            let Json(body) = Json::<T>::from_request(request, state)
                .await
                .map_err(|_| OauthError::InvalidRequest)?;
            return Ok(Self(body));
        }
        if mime == "application/x-www-form-urlencoded" {
            let Form(body) = Form::<T>::from_request(request, state)
                .await
                .map_err(|_| OauthError::InvalidRequest)?;
            return Ok(Self(body));
        }
        Err(OauthError::InvalidRequest)
    }
}

/// JSON-only body extractor whose rejection stays in the portal envelope.
struct PortalJson<T>(T);

impl<T> FromRequest<WebCtx> for PortalJson<T>
where
    T: DeserializeOwned,
{
    type Rejection = PortalDeviceError;

    async fn from_request(request: Request, state: &WebCtx) -> Result<Self, Self::Rejection> {
        let Json(body) = Json::<T>::from_request(request, state)
            .await
            .map_err(|_| PortalDeviceError::InvalidBody)?;
        Ok(Self(body))
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeRequest {
    client_id: String,
    #[serde(default)]
    key_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenRequest {
    grant_type: String,
    device_code: String,
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct UserCodeRequest {
    user_code: String,
}

#[derive(Debug, Deserialize)]
struct ApproveRequest {
    user_code: String,
    #[serde(default)]
    key_name: Option<String>,
}

async fn start_device_authorization(
    State(ctx): State<WebCtx>,
    FormOrJson(request): FormOrJson<DeviceCodeRequest>,
) -> Result<Json<serde_json::Value>, OauthError> {
    let client_id = request.client_id.trim();
    if !ctx
        .config
        .device_client_ids
        .iter()
        .any(|allowed| allowed == client_id)
    {
        return Err(OauthError::InvalidClient);
    }
    let key_name = normalized_key_name(request.key_name.as_deref());

    expire_stale_authorizations(&ctx.pool).await?;

    for _ in 0..USER_CODE_INSERT_ATTEMPTS {
        let device_code = generate_device_code();
        let user_code = generate_user_code();
        let inserted = sqlx::query(
            r#"
            INSERT INTO device_authorizations (
                id,
                device_code_hash,
                user_code,
                client_id,
                key_name,
                expires_at,
                poll_interval_secs
            )
            VALUES ($1, $2, $3, $4, $5, NOW() + ($6 * INTERVAL '1 second'), $7)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(sha256_hex(&device_code))
        .bind(&user_code)
        .bind(client_id)
        .bind(key_name.as_deref())
        .bind(DEVICE_CODE_TTL_SECS)
        .bind(DEVICE_POLL_INTERVAL_SECS)
        .execute(&ctx.pool)
        .await;
        match inserted {
            Ok(_) => {
                tracing::info!(client_id, "device authorization started");
                return Ok(Json(serde_json::json!({
                    "device_code": device_code,
                    "user_code": user_code,
                    "verification_uri": ctx.config.absolute_url("/activate"),
                    "verification_uri_complete":
                        ctx.config.absolute_url(&format!("/activate?code={user_code}")),
                    "expires_in": DEVICE_CODE_TTL_SECS,
                    "interval": DEVICE_POLL_INTERVAL_SECS,
                })));
            }
            Err(sqlx::Error::Database(database_error)) if database_error.is_unique_violation() => {
                // Draw fresh codes and retry.
            }
            Err(error) => {
                tracing::warn!(%error, "device authorization insert failed");
                return Err(OauthError::ServerError);
            }
        }
    }
    tracing::warn!("device authorization could not allocate a unique user code");
    Err(OauthError::ServerError)
}

async fn poll_device_token(
    State(ctx): State<WebCtx>,
    FormOrJson(request): FormOrJson<DeviceTokenRequest>,
) -> Result<Json<serde_json::Value>, OauthError> {
    if request.grant_type != DEVICE_CODE_GRANT_TYPE {
        return Err(OauthError::UnsupportedGrantType);
    }
    let device_code = request.device_code.trim();
    let client_id = request.client_id.trim();
    if !valid_device_code_shape(device_code) {
        return Err(OauthError::InvalidGrant);
    }

    let mut transaction = ctx.pool.begin().await.map_err(oauth_database_error)?;
    let row = sqlx::query_as::<_, (Uuid, String, Option<Uuid>, Option<String>, bool, bool)>(
        r#"
        SELECT
            id,
            status,
            user_id,
            key_name,
            expires_at <= NOW() AS expired,
            last_polled_at IS NOT NULL
                AND last_polled_at > NOW() - (poll_interval_secs * INTERVAL '1 second')
                AS polled_too_recently
        FROM device_authorizations
        WHERE device_code_hash = $1 AND client_id = $2
        FOR UPDATE
        "#,
    )
    .bind(sha256_hex(device_code))
    .bind(client_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(oauth_database_error)?;

    let Some((id, status, user_id, key_name, expired, polled_too_recently)) = row else {
        return Err(OauthError::InvalidGrant);
    };

    if expired && matches!(status.as_str(), "pending" | "approved") {
        sqlx::query("UPDATE device_authorizations SET status = 'expired' WHERE id = $1")
            .bind(id)
            .execute(&mut *transaction)
            .await
            .map_err(oauth_database_error)?;
        transaction.commit().await.map_err(oauth_database_error)?;
        return Err(OauthError::ExpiredToken);
    }

    match status.as_str() {
        "pending" => {
            // Record the poll (also on slow_down, so a tight loop stays
            // throttled) and report the grant as still pending.
            sqlx::query("UPDATE device_authorizations SET last_polled_at = NOW() WHERE id = $1")
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(oauth_database_error)?;
            transaction.commit().await.map_err(oauth_database_error)?;
            if polled_too_recently {
                Err(OauthError::SlowDown)
            } else {
                Err(OauthError::AuthorizationPending)
            }
        }
        "approved" => {
            // Claim: mint the key and mark the grant claimed in one
            // transaction. The `FOR UPDATE` lock above serializes concurrent
            // polls, so exactly one of them can observe status = 'approved'.
            let Some(user_id) = user_id else {
                // The schema forbids this; treat it as corruption, not access.
                tracing::error!(authorization = %id, "approved device authorization has no user");
                return Err(OauthError::ServerError);
            };
            // A device claim is a self-service mint like the portal's, so it
            // takes the same limits — before this check it was the mint path
            // that bypassed them entirely, which let one user hold unlimited
            // simultaneous keys. Lock the owning user's row first, exactly as
            // `portal::create_key` does, so two concurrent claims cannot both
            // observe a count below the limit. The device_authorizations row is
            // already locked (`FOR UPDATE` above) and portal mints never take
            // that lock, so the ordering introduces no cycle.
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(oauth_database_error)?;
            // Both refusals are terminal for the polling client (RFC 8628
            // §3.5), and the transaction rolls back either way, so the grant
            // keeps its 'approved' status and no key is minted. Exhaustive on
            // purpose: a new refusal reason must be answered here rather than
            // falling through to a mint.
            match admit_key_mint(&mut transaction, user_id)
                .await
                .map_err(oauth_database_error)?
            {
                KeyMintAdmission::Allowed => {}
                KeyMintAdmission::LimitReached => {
                    tracing::warn!(
                        authorization = %id,
                        user_id = %user_id,
                        "device authorization refused: the user is at its API key limit"
                    );
                    return Err(OauthError::AccessDenied);
                }
                KeyMintAdmission::AccountFrozen => {
                    tracing::warn!(
                        authorization = %id,
                        user_id = %user_id,
                        "device authorization refused: the account is frozen"
                    );
                    return Err(OauthError::AccessDenied);
                }
            }
            let api_key = generate_api_key();
            let key_id = Uuid::new_v4();
            let name = key_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(DEFAULT_KEY_NAME);
            sqlx::query(
                "INSERT INTO api_keys (id, user_id, key_hash, name) VALUES ($1, $2, $3, $4)",
            )
            .bind(key_id)
            .bind(user_id)
            .bind(hash_api_key(&api_key))
            .bind(name)
            .execute(&mut *transaction)
            .await
            .map_err(oauth_database_error)?;
            let claimed = sqlx::query(
                r#"
                UPDATE device_authorizations
                SET status = 'claimed', api_key_id = $2, last_polled_at = NOW()
                WHERE id = $1 AND status = 'approved'
                "#,
            )
            .bind(id)
            .bind(key_id)
            .execute(&mut *transaction)
            .await
            .map_err(oauth_database_error)?;
            if claimed.rows_affected() != 1 {
                // Unreachable under the row lock; fail closed regardless.
                return Err(OauthError::InvalidGrant);
            }
            transaction.commit().await.map_err(oauth_database_error)?;
            tracing::info!(authorization = %id, key_id = %key_id, "device authorization claimed");
            Ok(Json(serde_json::json!({
                "access_token": api_key,
                "token_type": "bearer",
                "scope": "inference",
            })))
        }
        "denied" => Err(OauthError::AccessDenied),
        "expired" => Err(OauthError::ExpiredToken),
        // Single use: a claimed grant never releases the key again.
        "claimed" => Err(OauthError::InvalidGrant),
        other => {
            tracing::error!(authorization = %id, status = other, "device authorization has an unknown status");
            Err(OauthError::ServerError)
        }
    }
}

/// Discovery document in the shape ZeroClaw's existing device flow expects.
async fn openid_configuration(State(ctx): State<WebCtx>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "issuer": ctx.config.public_base_url,
        "device_authorization_endpoint": ctx.config.absolute_url("/auth/device/code"),
        "token_endpoint": ctx.config.absolute_url("/auth/device/token"),
        "authorization_endpoint": ctx.config.absolute_url("/auth/login"),
        "grant_types_supported": [DEVICE_CODE_GRANT_TYPE],
        "response_types_supported": [],
        "scopes_supported": ["inference"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

async fn lookup_user_code(
    State(ctx): State<WebCtx>,
    _user: PortalUser,
    PortalJson(request): PortalJson<UserCodeRequest>,
) -> Result<Json<serde_json::Value>, PortalDeviceError> {
    let user_code = normalize_user_code(&request.user_code).ok_or(PortalDeviceError::NotFound)?;
    let (client_id, key_name, created_at) =
        sqlx::query_as::<_, (String, Option<String>, DateTime<Utc>)>(
            r#"
            SELECT client_id, key_name, created_at
            FROM device_authorizations
            WHERE user_code = $1 AND status = 'pending' AND expires_at > NOW()
            "#,
        )
        .bind(&user_code)
        .fetch_optional(&ctx.pool)
        .await
        .map_err(portal_database_error)?
        .ok_or(PortalDeviceError::NotFound)?;
    Ok(Json(serde_json::json!({
        "client_id": client_id,
        "key_name": key_name,
        "created_at": created_at,
    })))
}

async fn approve_user_code(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    PortalJson(request): PortalJson<ApproveRequest>,
) -> Result<Json<serde_json::Value>, PortalDeviceError> {
    let user_code = normalize_user_code(&request.user_code).ok_or(PortalDeviceError::NotFound)?;
    let key_name = normalized_key_name(request.key_name.as_deref());
    let updated = sqlx::query(
        r#"
        UPDATE device_authorizations
        SET status = 'approved', user_id = $2, key_name = COALESCE($3, key_name)
        WHERE user_code = $1 AND status = 'pending' AND expires_at > NOW()
        "#,
    )
    .bind(&user_code)
    .bind(user.user_id)
    .bind(key_name.as_deref())
    .execute(&ctx.pool)
    .await
    .map_err(portal_database_error)?;
    if updated.rows_affected() != 1 {
        return Err(PortalDeviceError::NotFound);
    }
    tracing::info!(user_id = %user.user_id, "device authorization approved");
    Ok(Json(serde_json::json!({ "status": "approved" })))
}

async fn deny_user_code(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    PortalJson(request): PortalJson<UserCodeRequest>,
) -> Result<Json<serde_json::Value>, PortalDeviceError> {
    let user_code = normalize_user_code(&request.user_code).ok_or(PortalDeviceError::NotFound)?;
    let updated = sqlx::query(
        r#"
        UPDATE device_authorizations
        SET status = 'denied'
        WHERE user_code = $1 AND status = 'pending' AND expires_at > NOW()
        "#,
    )
    .bind(&user_code)
    .execute(&ctx.pool)
    .await
    .map_err(portal_database_error)?;
    if updated.rows_affected() != 1 {
        return Err(PortalDeviceError::NotFound);
    }
    tracing::info!(user_id = %user.user_id, "device authorization denied");
    Ok(Json(serde_json::json!({ "status": "denied" })))
}

/// Flip stale pending/approved grants to `expired`; safe to run
/// opportunistically before creating a new grant.
async fn expire_stale_authorizations(pool: &PgPool) -> Result<(), OauthError> {
    sqlx::query(
        r#"
        UPDATE device_authorizations
        SET status = 'expired'
        WHERE status IN ('pending', 'approved') AND expires_at <= NOW()
        "#,
    )
    .execute(pool)
    .await
    .map_err(oauth_database_error)?;
    // Reclaim terminal rows past a grace window so an unauthenticated flood of
    // /auth/device/code cannot grow the table without bound; the 15-minute grant
    // TTL means nothing relevant is ever this old, and a claimed grant's minted
    // key is stored on api_keys, not here, so deleting the record loses nothing.
    sqlx::query(
        r#"
        DELETE FROM device_authorizations
        WHERE status IN ('expired', 'denied', 'claimed')
          AND expires_at <= NOW() - INTERVAL '24 hours'
        "#,
    )
    .execute(pool)
    .await
    .map_err(oauth_database_error)?;
    Ok(())
}

fn oauth_database_error(error: sqlx::Error) -> OauthError {
    tracing::warn!(%error, "device authorization database error");
    OauthError::ServerError
}

fn portal_database_error(error: sqlx::Error) -> PortalDeviceError {
    tracing::warn!(%error, "device authorization database error");
    PortalDeviceError::DatabaseUnavailable
}

fn generate_device_code() -> String {
    let bytes: [u8; DEVICE_CODE_BYTES] = rand::random();
    format!("{DEVICE_CODE_PREFIX}{}", hex::encode(bytes))
}

fn valid_device_code_shape(code: &str) -> bool {
    code.len() == DEVICE_CODE_PREFIX.len() + DEVICE_CODE_BYTES * 2
        && code.starts_with(DEVICE_CODE_PREFIX)
        && code[DEVICE_CODE_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

/// Generate a `XXXX-XXXX` user code from [`USER_CODE_ALPHABET`].
///
/// Rejection sampling keeps every character equally likely: a random byte is
/// discarded when it falls into the truncated tail above the largest multiple
/// of the alphabet length, which is where plain `byte % len` would over-weight
/// the low characters.
fn generate_user_code() -> String {
    let largest_fair_multiple =
        (usize::from(u8::MAX) + 1) / USER_CODE_ALPHABET.len() * USER_CODE_ALPHABET.len();
    let mut code = String::with_capacity(USER_CODE_CHARS + 1);
    while code.len() < USER_CODE_CHARS + 1 {
        if code.len() == USER_CODE_GROUP {
            code.push('-');
            continue;
        }
        let byte: u8 = rand::random();
        if usize::from(byte) >= largest_fair_multiple {
            continue;
        }
        code.push(char::from(
            USER_CODE_ALPHABET[usize::from(byte) % USER_CODE_ALPHABET.len()],
        ));
    }
    code
}

/// Normalize user input to the stored `XXXX-XXXX` form: trim, uppercase, and
/// re-insert the dash. Anything that is not exactly eight alphabet characters
/// is rejected (`None`), which surfaces as "not found" — never a lookup hit.
fn normalize_user_code(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|character| !matches!(character, '-' | ' '))
        .map(|character| character.to_ascii_uppercase())
        .collect();
    if cleaned.len() != USER_CODE_CHARS {
        return None;
    }
    if !cleaned
        .bytes()
        .all(|byte| USER_CODE_ALPHABET.contains(&byte))
    {
        return None;
    }
    Some(format!(
        "{}-{}",
        &cleaned[..USER_CODE_GROUP],
        &cleaned[USER_CODE_GROUP..]
    ))
}

/// Longest device-supplied key label that is stored. The portal holds
/// self-service names to the same bound; this endpoint is UNAUTHENTICATED,
/// so without it a caller can park a quarter-megabyte of incompressible
/// text per request — retained until 24 hours after the grant expires —
/// and grow the database indefinitely for free (sol review). Truncating
/// rather than rejecting keeps an honest long hostname working.
const MAX_KEY_NAME_CHARS: usize = 100;

fn normalized_key_name(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(MAX_KEY_NAME_CHARS).collect())
}

fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn user_codes_are_formatted_from_the_unambiguous_alphabet() {
        for _ in 0..64 {
            let code = generate_user_code();
            assert_eq!(code.len(), USER_CODE_CHARS + 1);
            assert_eq!(code.as_bytes()[USER_CODE_GROUP], b'-');
            for (index, byte) in code.bytes().enumerate() {
                if index == USER_CODE_GROUP {
                    continue;
                }
                assert!(
                    USER_CODE_ALPHABET.contains(&byte),
                    "unexpected character {:?} in {code}",
                    char::from(byte)
                );
            }
        }
    }

    #[test]
    fn user_code_sampling_reaches_every_alphabet_character() {
        // Modulo-bias smoke: fair sampling makes every symbol appear quickly
        // (26 symbols, ~1600 drawn characters; a missing symbol at this
        // sample size would indicate a broken or biased sampler).
        let mut seen = HashSet::new();
        for _ in 0..200 {
            for byte in generate_user_code().bytes() {
                if byte != b'-' {
                    seen.insert(byte);
                }
            }
        }
        assert_eq!(seen.len(), USER_CODE_ALPHABET.len());
        assert!(seen.iter().all(|byte| USER_CODE_ALPHABET.contains(byte)));
    }

    #[test]
    fn user_code_alphabet_excludes_ambiguous_characters() {
        for ambiguous in b"01OILAEUYN" {
            assert!(
                !USER_CODE_ALPHABET.contains(ambiguous),
                "alphabet should not contain {:?}",
                char::from(*ambiguous)
            );
        }
    }

    #[test]
    fn user_codes_normalize_from_common_input_shapes() {
        assert_eq!(
            normalize_user_code("bcdf-ghjk").as_deref(),
            Some("BCDF-GHJK")
        );
        assert_eq!(
            normalize_user_code("bcdfghjk").as_deref(),
            Some("BCDF-GHJK")
        );
        assert_eq!(
            normalize_user_code(" BCDF GHJK ").as_deref(),
            Some("BCDF-GHJK")
        );
        let generated = generate_user_code();
        assert_eq!(normalize_user_code(&generated), Some(generated.clone()));
    }

    #[test]
    fn malformed_user_codes_are_rejected() {
        assert_eq!(normalize_user_code(""), None);
        assert_eq!(normalize_user_code("BCDF-GHJ"), None);
        assert_eq!(normalize_user_code("BCDF-GHJKM"), None);
        // A and E are not in the ambiguity-free alphabet.
        assert_eq!(normalize_user_code("ABCD-EFGH"), None);
        assert_eq!(normalize_user_code("BCDF_GHJK"), None);
    }

    #[test]
    fn device_codes_have_a_valid_shape() {
        let code = generate_device_code();
        assert!(valid_device_code_shape(&code));
        assert_eq!(code.len(), DEVICE_CODE_PREFIX.len() + DEVICE_CODE_BYTES * 2);
        assert_ne!(code, generate_device_code());
        assert!(!valid_device_code_shape(""));
        assert!(!valid_device_code_shape("zcr_deadbeef"));
        assert!(!valid_device_code_shape(&code[..code.len() - 1]));
    }
}

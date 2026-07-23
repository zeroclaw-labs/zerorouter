//! OIDC relying party for portal login: `/auth/login`, `/auth/callback`, and
//! `/auth/logout`.
//!
//! Login is the authorization-code flow with PKCE against the configured
//! identity provider. The state, nonce, and PKCE verifier for an in-flight
//! login live in the single-use `oidc_states` table (not in a cookie), so any
//! task instance can complete a callback started by another. Users are keyed
//! by `(oidc_issuer, oidc_subject)`; an admin-provisioned user with a matching
//! email and no OIDC identity is claimed on first login, and brand-new users
//! receive the configured signup credit as a `promo` ledger entry.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    RequestTokenError, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::{
    session::{
        PortalUser, clear_session_cookie_header, create_session, prune_sessions, revoke_session,
        session_cookie_header,
    },
    sqlx::{self, PgPool},
    web::{OidcSettings, WebCtx},
};

/// HTTP client used for provider discovery and the code exchange.
type HttpClient = openidconnect::reqwest::Client;

/// The client typestate produced by [`CoreClient::from_provider_metadata`]:
/// the authorization endpoint is guaranteed, the token and user-info
/// endpoints are present only if the discovery document advertised them.
type RelyingParty = CoreClient<
    EndpointSet,      // authorization endpoint
    EndpointNotSet,   // device authorization endpoint
    EndpointNotSet,   // introspection endpoint
    EndpointNotSet,   // revocation endpoint
    EndpointMaybeSet, // token endpoint
    EndpointMaybeSet, // user-info endpoint
>;

#[derive(Debug)]
pub enum OidcError {
    Unavailable,
    DiscoveryFailed,
    Misconfigured,
    StateInvalid,
    CodeMissing,
    CodeRejected,
    ExchangeFailed,
    IdTokenInvalid,
    EmailRequired,
    EmailUnverified,
    EmailConflict,
    DatabaseUnavailable,
}

impl OidcError {
    fn response_parts(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "OIDC login is not configured on this deployment.",
                "oidc_unavailable",
            ),
            Self::DiscoveryFailed => (
                StatusCode::BAD_GATEWAY,
                "The identity provider discovery document could not be fetched.",
                "oidc_discovery_failed",
            ),
            Self::Misconfigured => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The OIDC relying party is misconfigured.",
                "oidc_misconfigured",
            ),
            Self::StateInvalid => (
                StatusCode::BAD_REQUEST,
                "The login state is missing, expired, or already used; restart the sign-in.",
                "login_state_invalid",
            ),
            Self::CodeMissing => (
                StatusCode::BAD_REQUEST,
                "The identity provider did not return an authorization code.",
                "login_failed",
            ),
            Self::CodeRejected => (
                StatusCode::BAD_REQUEST,
                "The identity provider rejected the authorization code; restart the sign-in.",
                "login_code_rejected",
            ),
            Self::ExchangeFailed => (
                StatusCode::BAD_GATEWAY,
                "The authorization code could not be exchanged with the identity provider.",
                "oidc_exchange_failed",
            ),
            Self::IdTokenInvalid => (
                StatusCode::BAD_REQUEST,
                "The identity token could not be verified.",
                "id_token_invalid",
            ),
            Self::EmailRequired => (
                StatusCode::FORBIDDEN,
                "The identity provider did not supply an email address.",
                "email_required",
            ),
            Self::EmailUnverified => (
                StatusCode::FORBIDDEN,
                "The email address is not verified with the identity provider.",
                "email_unverified",
            ),
            Self::EmailConflict => (
                StatusCode::CONFLICT,
                "The email address is already linked to a different sign-in identity.",
                "email_conflict",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Sign-in is temporarily unavailable.",
                "database_unavailable",
            ),
        }
    }
}

impl IntoResponse for OidcError {
    fn into_response(self) -> Response {
        let (status, message, code) = self.response_parts();
        (
            status,
            Json(serde_json::json!({
                "error": { "message": message, "type": "portal_error", "code": code }
            })),
        )
            .into_response()
    }
}

pub fn router() -> Router<WebCtx> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
}

/// Begin the authorization-code + PKCE flow: persist the single-use login
/// state and redirect the browser to the provider's authorization endpoint.
async fn login(State(ctx): State<WebCtx>) -> Result<Response, OidcError> {
    let Some(settings) = ctx.config.oidc.as_ref() else {
        return Err(OidcError::Unavailable);
    };
    let http = http_client()?;
    let metadata = discovered_metadata(&settings.issuer_url, &http).await?;
    let client = relying_party(
        metadata,
        settings,
        ctx.config.absolute_url("/auth/callback"),
    )?;

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (authorize_url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        // The `openid` scope is added automatically by the client; `profile`
        // is deliberately not requested because nothing here uses it.
        .add_scope(Scope::new("email".to_owned()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    // Opportunistic cleanup: an expired row can never be consumed, so a
    // failure here only delays garbage collection.
    if let Err(error) = sqlx::query("DELETE FROM oidc_states WHERE expires_at <= NOW()")
        .execute(&ctx.pool)
        .await
    {
        tracing::debug!(error = %error, "expired OIDC state cleanup failed");
    }

    sqlx::query(
        r#"
        INSERT INTO oidc_states (state, nonce, pkce_verifier, expires_at)
        VALUES ($1, $2, $3, NOW() + INTERVAL '10 minutes')
        "#,
    )
    .bind(state.secret().as_str())
    .bind(nonce.secret().as_str())
    .bind(pkce_verifier.secret().as_str())
    .execute(&ctx.pool)
    .await
    .map_err(|_| OidcError::DatabaseUnavailable)?;

    Ok(Redirect::to(authorize_url.as_str()).into_response())
}

// No `Debug` derive: the authorization code must never reach a log line.
#[derive(Deserialize)]
struct CallbackParams {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

/// Complete the flow: consume the single-use state, exchange the code,
/// verify the identity token, resolve the user, and establish a session.
async fn callback(
    State(ctx): State<WebCtx>,
    Query(params): Query<CallbackParams>,
) -> Result<Response, OidcError> {
    let Some(settings) = ctx.config.oidc.as_ref() else {
        return Err(OidcError::Unavailable);
    };
    let state = params
        .state
        .as_deref()
        .map(str::trim)
        .filter(|state| !state.is_empty())
        .ok_or(OidcError::StateInvalid)?;

    // Single use: consume the login state before anything else, so a replayed
    // callback can never be exchanged twice.
    let consumed = sqlx::query_as::<_, (String, String)>(
        r#"
        DELETE FROM oidc_states
        WHERE state = $1 AND expires_at > NOW()
        RETURNING nonce, pkce_verifier
        "#,
    )
    .bind(state)
    .fetch_optional(&ctx.pool)
    .await
    .map_err(|_| OidcError::DatabaseUnavailable)?;
    let (stored_nonce, stored_pkce_verifier) = consumed.ok_or(OidcError::StateInvalid)?;

    if params.error.is_some() {
        // The provider reported a failure (for example the user denied
        // consent); the state above is already spent, so this login is over.
        return Err(OidcError::CodeMissing);
    }
    let code = params
        .code
        .filter(|code| !code.trim().is_empty())
        .ok_or(OidcError::CodeMissing)?;

    let http = http_client()?;
    let metadata = discovered_metadata(&settings.issuer_url, &http).await?;
    let client = relying_party(
        metadata,
        settings,
        ctx.config.absolute_url("/auth/callback"),
    )?;

    let token_response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| {
            tracing::error!("the discovered OIDC provider metadata has no token endpoint");
            OidcError::Misconfigured
        })?
        .set_pkce_verifier(PkceCodeVerifier::new(stored_pkce_verifier))
        .request_async(&http)
        .await
        .map_err(|error| match error {
            RequestTokenError::ServerResponse(_) => OidcError::CodeRejected,
            other => {
                tracing::warn!(error = %other, "OIDC code exchange failed");
                OidcError::ExchangeFailed
            }
        })?;

    let id_token = token_response.id_token().ok_or(OidcError::IdTokenInvalid)?;
    let expected_nonce = Nonce::new(stored_nonce);
    let claims = id_token
        .claims(&client.id_token_verifier(), &expected_nonce)
        .map_err(|_| {
            tracing::warn!("OIDC identity token verification failed");
            OidcError::IdTokenInvalid
        })?;

    let issuer = claims.issuer().as_str().to_owned();
    let subject = claims.subject().as_str().to_owned();
    let email = normalize_email(claims.email().ok_or(OidcError::EmailRequired)?.as_str());
    if email.is_empty() {
        return Err(OidcError::EmailRequired);
    }
    if claims.email_verified() == Some(false) {
        return Err(OidcError::EmailUnverified);
    }

    let (user_id, is_new) = upsert_user(&ctx.pool, &issuer, &subject, &email).await?;

    if is_new && ctx.config.signup_credit_usd > Decimal::ZERO {
        // A failed grant must not fail the login: the account exists, and the
        // credit can be granted out of band.
        if let Err(error) = crate::billing::grant_promo(
            &ctx.pool,
            user_id,
            ctx.config.signup_credit_usd,
            "signup credit",
        )
        .await
        {
            tracing::warn!(user_id = %user_id, error = %error, "signup credit grant failed");
        }
    }

    let (token, _expires_at) = create_session(&ctx.pool, user_id, ctx.config.session_ttl)
        .await
        .map_err(|_| OidcError::DatabaseUnavailable)?;
    if let Err(error) = prune_sessions(&ctx.pool).await {
        tracing::debug!(error = %error, "opportunistic session pruning failed");
    }

    tracing::info!(user_id = %user_id, new_user = is_new, "portal login completed");

    let mut response = Redirect::to("/").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        session_cookie_header(
            &token,
            ctx.config.session_ttl.as_secs(),
            ctx.config.secure_cookies,
        ),
    );
    Ok(response)
}

/// Revoke the current session and clear the cookie.
async fn logout(State(ctx): State<WebCtx>, user: PortalUser) -> Result<Response, OidcError> {
    revoke_session(&ctx.pool, user.session_id)
        .await
        .map_err(|_| OidcError::DatabaseUnavailable)?;
    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        clear_session_cookie_header(ctx.config.secure_cookies),
    );
    Ok(response)
}

/// Resolve a verified OIDC identity to a local user in one transaction:
/// (a) an existing `(issuer, subject)` user, else (b) claim an
/// admin-provisioned user with the same email and no OIDC identity, else
/// (c) insert a new user. Returns the user id and whether it was inserted.
async fn upsert_user(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
    email: &str,
) -> Result<(Uuid, bool), OidcError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|_| OidcError::DatabaseUnavailable)?;

    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE oidc_issuer = $1 AND oidc_subject = $2",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OidcError::DatabaseUnavailable)?;
    if let Some(user_id) = existing {
        transaction
            .commit()
            .await
            .map_err(|_| OidcError::DatabaseUnavailable)?;
        return Ok((user_id, false));
    }

    let claimed = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE users
        SET oidc_issuer = $1, oidc_subject = $2
        WHERE email = $3 AND oidc_issuer IS NULL
        RETURNING id
        "#,
    )
    .bind(issuer)
    .bind(subject)
    .bind(email)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OidcError::DatabaseUnavailable)?;
    if let Some(user_id) = claimed {
        transaction
            .commit()
            .await
            .map_err(|_| OidcError::DatabaseUnavailable)?;
        return Ok((user_id, false));
    }

    // `ON CONFLICT (email) DO NOTHING` turns the one expected conflict — the
    // email already belonging to a different OIDC identity — into "no row"
    // without parsing database error codes. Any other failure (including a
    // concurrent `(issuer, subject)` insert) stays a retryable 503.
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO users (id, email, oidc_issuer, oidc_subject)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (email) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(email)
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OidcError::DatabaseUnavailable)?;
    let Some(user_id) = inserted else {
        if let Err(error) = transaction.rollback().await {
            tracing::debug!(error = %error, "rollback after an email conflict failed");
        }
        return Err(OidcError::EmailConflict);
    };
    transaction
        .commit()
        .await
        .map_err(|_| OidcError::DatabaseUnavailable)?;
    Ok((user_id, true))
}

/// Provider metadata cache: discovery runs once per process for the
/// configured issuer. The issuer is stored alongside the metadata so that a
/// mismatch (impossible while the configuration is immutable, but checked
/// anyway) falls back to a fresh, uncached discovery instead of trusting
/// metadata from a different issuer.
static DISCOVERED: OnceCell<(String, CoreProviderMetadata)> = OnceCell::const_new();

async fn discovered_metadata(
    issuer: &str,
    http: &HttpClient,
) -> Result<CoreProviderMetadata, OidcError> {
    let (cached_issuer, metadata) = DISCOVERED
        .get_or_try_init(|| async {
            discover(issuer, http)
                .await
                .map(|metadata| (issuer.to_owned(), metadata))
        })
        .await?;
    if cached_issuer == issuer {
        Ok(metadata.clone())
    } else {
        tracing::info!("OIDC issuer differs from the cached discovery; re-discovering");
        discover(issuer, http).await
    }
}

async fn discover(issuer: &str, http: &HttpClient) -> Result<CoreProviderMetadata, OidcError> {
    let issuer_url = IssuerUrl::new(issuer.to_owned()).map_err(|_| {
        tracing::error!("the configured OIDC issuer is not a valid URL");
        OidcError::Misconfigured
    })?;
    CoreProviderMetadata::discover_async(issuer_url, http)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "OIDC provider discovery failed");
            OidcError::DiscoveryFailed
        })
}

fn relying_party(
    metadata: CoreProviderMetadata,
    settings: &OidcSettings,
    redirect_uri: String,
) -> Result<RelyingParty, OidcError> {
    let redirect_url = RedirectUrl::new(redirect_uri).map_err(|_| {
        tracing::error!("the OIDC redirect URI derived from the public base URL is invalid");
        OidcError::Misconfigured
    })?;
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(settings.client_id.clone()),
        Some(ClientSecret::new(settings.client_secret.clone())),
    )
    .set_redirect_uri(redirect_url))
}

fn http_client() -> Result<HttpClient, OidcError> {
    openidconnect::reqwest::ClientBuilder::new()
        // Never follow provider redirects: discovery and token responses must
        // come from the configured endpoints themselves (SSRF hardening, per
        // the openidconnect documentation).
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| {
            tracing::error!(error = %error, "failed to build the OIDC HTTP client");
            OidcError::Misconfigured
        })
}

/// Trim surrounding whitespace and lowercase, so the same mailbox written
/// with different casing maps to a single account.
fn normalize_email(raw: &str) -> String {
    raw.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_normalization_trims_and_lowercases() {
        assert_eq!(
            normalize_email("  Jordan@Example.COM \n"),
            "jordan@example.com"
        );
        assert_eq!(normalize_email("already@lower.dev"), "already@lower.dev");
        assert_eq!(normalize_email(" \t "), "");
    }

    #[test]
    fn error_responses_fail_closed() {
        assert_eq!(
            OidcError::Unavailable.response_parts().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            OidcError::DiscoveryFailed.response_parts().0,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            OidcError::StateInvalid.response_parts().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            OidcError::EmailRequired.response_parts().0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            OidcError::EmailUnverified.response_parts().0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            OidcError::EmailConflict.response_parts().0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            OidcError::DatabaseUnavailable.response_parts().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn error_codes_match_the_contract() {
        assert_eq!(
            OidcError::Unavailable.response_parts().2,
            "oidc_unavailable"
        );
        assert_eq!(
            OidcError::DiscoveryFailed.response_parts().2,
            "oidc_discovery_failed"
        );
        assert_eq!(
            OidcError::StateInvalid.response_parts().2,
            "login_state_invalid"
        );
        assert_eq!(
            OidcError::EmailRequired.response_parts().2,
            "email_required"
        );
        assert_eq!(
            OidcError::EmailUnverified.response_parts().2,
            "email_unverified"
        );
        assert_eq!(
            OidcError::EmailConflict.response_parts().2,
            "email_conflict"
        );
    }
}

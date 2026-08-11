//! Portal sign-in resilience: what `/auth/callback` does when the login state
//! it is handed cannot be used.
//!
//! The bug these pin is a first-impression bug. A brand-new user's first
//! sign-in is not a redirect round trip — the provider walks them through
//! registration, passkey enrolment, and email verification, and that last step
//! means leaving for an inbox. A real signup took about twenty-five minutes,
//! which was longer than the login state lived, so the product's opening move
//! was a raw JSON error at a dead end.
//!
//! Two halves are covered here: the state now outlives that detour, and a
//! state that is nevertheless unusable restarts the sign-in instead of
//! reporting it. The restart is bounded — one bounce, and a restart that
//! cannot mint a fresh state says so rather than sending the browser around
//! again.
//!
//! Gated on `DATABASE_URL` like `tests/portal.rs`: unset means the test
//! returns early. The identity provider is either a local fixture serving a
//! discovery document, or a closed port when the flow must not reach it.

use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::get,
};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    db::migrate,
    oidc,
    web::{OidcSettings, WebConfig, WebCtx},
};

/// The browser-facing names are part of the contract with the SPA and with
/// every session already in a user's browser, so they are spelled out here
/// rather than imported.
const STATE_COOKIE: &str = "zr_oidc_state";
const RESTART_COOKIE: &str = "zr_oidc_restart";
const SESSION_COOKIE: &str = "zr_session";
const SIGN_IN_PATH: &str = "/auth/login";

/// An issuer that cannot answer: reaching it at all is a test failure, and it
/// fails fast (connection refused) when a test means to reach it.
const DEAD_ISSUER: &str = "http://127.0.0.1:1";

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

/// A pool that will never connect: every query fails, standing in for the
/// database outage that must stop the restart rather than loop it.
fn unreachable_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy_with(
            PgConnectOptions::from_str("postgres://zr@127.0.0.1:1/nothing")
                .expect("the unreachable URL must parse"),
        )
}

fn web_config(issuer: &str) -> WebConfig {
    WebConfig {
        public_base_url: "http://127.0.0.1".to_owned(),
        secure_cookies: false,
        oidc: Some(OidcSettings {
            issuer_url: issuer.to_owned(),
            client_id: "zerorouter-test".to_owned(),
            client_secret: "test-secret".to_owned(),
        }),
        stripe: None,
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3_600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    }
}

fn oidc_app(pool: PgPool, issuer: &str) -> Router {
    oidc::router().with_state(WebCtx::new(pool, web_config(issuer)))
}

struct Answer {
    status: StatusCode,
    location: Option<String>,
    cookies: Vec<String>,
    body: String,
}

impl Answer {
    fn error_code(&self) -> Option<String> {
        let json = serde_json::from_str::<Value>(&self.body).ok()?;
        Some(json["error"]["code"].as_str()?.to_owned())
    }

    /// The `Set-Cookie` for `name`, if the response sets or clears it.
    fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
            .map(String::as_str)
    }

    fn restarts_sign_in(&self) -> bool {
        self.status.is_redirection()
            && self
                .location
                .as_deref()
                .is_some_and(|location| location.starts_with(SIGN_IN_PATH))
    }
}

async fn send(app: Router, uri: &str, cookie: Option<&str>) -> Answer {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let request = builder.body(Body::empty()).expect("request should build");
    let response = app.oneshot(request).await.expect("request should complete");
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should be readable")
        .to_bytes();
    Answer {
        status,
        location,
        cookies,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A distinct state value per test, so tests sharing a database (and the
/// opportunistic expired-row cleanup in `/auth/login`) cannot collide.
fn fresh_state(tag: &str) -> String {
    format!("state-{tag}-{}", Uuid::new_v4())
}

/// `lifetime` is relative to now and may be negative — the row is backdated an
/// hour so an already-expired state still satisfies `expires_at > created_at`.
async fn seed_state(pool: &PgPool, state: &str, lifetime: &str) {
    query(&format!(
        "INSERT INTO oidc_states (state, nonce, pkce_verifier, created_at, expires_at)
         VALUES ($1, $2, $3, NOW() - INTERVAL '1 hour', NOW() + INTERVAL '{lifetime}')"
    ))
    .bind(state)
    .bind(format!("nonce-{state}"))
    .bind(format!("verifier-{state}"))
    .execute(pool)
    .await
    .expect("test login state must insert");
}

async fn state_exists(pool: &PgPool, state: &str) -> bool {
    query_scalar::<_, i64>("SELECT COUNT(*) FROM oidc_states WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .expect("state count must query")
        > 0
}

/// A local stand-in for the identity provider that answers discovery only.
/// Enough to get `/auth/login` as far as minting a state, which is all any
/// test here needs from a provider.
async fn mock_idp() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture provider must bind");
    let issuer = format!(
        "http://127.0.0.1:{}",
        listener
            .local_addr()
            .expect("fixture provider must have an address")
            .port()
    );
    let document = Arc::new(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/keys"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }));
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let document = document.clone();
                async move { Json((*document).clone()) }
            }),
        )
        .route("/keys", get(|| async { Json(json!({ "keys": [] })) }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    issuer
}

/// Half one of the fix: the state row and its cookie both outlive a signup
/// that detours through an inbox. At ten minutes this row was already gone
/// when the provider redirected back.
#[tokio::test]
async fn a_minted_state_outlives_the_signup_detour() {
    let Some(pool) = connect().await else {
        return;
    };
    let issuer = mock_idp().await;

    let answer = send(oidc_app(pool.clone(), &issuer), SIGN_IN_PATH, None).await;
    assert!(
        answer.status.is_redirection(),
        "login should redirect to the provider, got {} {}",
        answer.status,
        answer.body
    );
    let location = answer
        .location
        .as_deref()
        .expect("login should set a Location");
    assert!(
        location.starts_with(&format!("{issuer}/authorize")),
        "login should send the browser to the provider: {location}"
    );

    let cookie = answer
        .cookie(STATE_COOKIE)
        .expect("login should set the state cookie")
        .to_owned();
    assert!(
        cookie.contains("Max-Age=2700"),
        "the state cookie must live the full window: {cookie}"
    );
    let state = cookie
        .trim_start_matches(&format!("{STATE_COOKIE}="))
        .split(';')
        .next()
        .expect("the state cookie should carry a value")
        .to_owned();

    let remaining = query_scalar::<_, f64>(
        "SELECT EXTRACT(EPOCH FROM (expires_at - NOW()))::double precision
         FROM oidc_states WHERE state = $1",
    )
    .bind(&state)
    .fetch_one(&pool)
    .await
    .expect("the minted state row must exist");
    assert!(
        (2_600.0..=2_700.0).contains(&remaining),
        "the state row must live the same 45 minutes as its cookie, got {remaining}s"
    );

    query("DELETE FROM oidc_states WHERE state = $1")
        .bind(&state)
        .execute(&pool)
        .await
        .expect("test cleanup must run");
}

/// Half two: an expired state is a user standing at a dead end, not an error
/// to report. It restarts the sign-in — for a user who just registered, the
/// provider still holds their session and the restart returns immediately.
#[tokio::test]
async fn an_expired_state_restarts_the_sign_in() {
    let Some(pool) = connect().await else {
        return;
    };
    let state = fresh_state("expired");
    seed_state(&pool, &state, "-1 minute").await;

    let answer = send(
        oidc_app(pool.clone(), DEAD_ISSUER),
        &format!("/auth/callback?state={state}&code=provider-code"),
        Some(&format!("{STATE_COOKIE}={state}")),
    )
    .await;

    assert!(
        answer.restarts_sign_in(),
        "an expired state must restart the sign-in, got {} {:?} {}",
        answer.status,
        answer.location,
        answer.body
    );
    assert_ne!(
        answer.error_code().as_deref(),
        Some("login_state_invalid"),
        "the dead-end JSON error must not be what a new user sees"
    );
    assert!(
        answer.cookie(RESTART_COOKIE).is_some(),
        "the restart must be marked so it cannot repeat forever"
    );
    assert!(
        answer.cookie(SESSION_COOKIE).is_none(),
        "no session may be minted from an unusable state"
    );

    query("DELETE FROM oidc_states WHERE state = $1")
        .bind(&state)
        .execute(&pool)
        .await
        .expect("test cleanup must run");
}

/// Single use is unchanged, and the replay of a consumed callback gets the
/// same restart — never a second session.
#[tokio::test]
async fn a_used_state_restarts_and_mints_no_session() {
    let Some(pool) = connect().await else {
        return;
    };
    let state = fresh_state("used");
    seed_state(&pool, &state, "45 minutes").await;
    let cookie = format!("{STATE_COOKIE}={state}");

    // Consume it. Sending no `code` stops the flow right after the state is
    // spent, which also pins the order: the state is consumed before anything
    // about the authorization code is considered.
    let first = send(
        oidc_app(pool.clone(), DEAD_ISSUER),
        &format!("/auth/callback?state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(first.status, StatusCode::BAD_REQUEST);
    assert_eq!(first.error_code().as_deref(), Some("login_failed"));
    assert!(
        !state_exists(&pool, &state).await,
        "the login state must be single use"
    );

    // Replay the provider's redirect, code and all.
    let replay = send(
        oidc_app(pool.clone(), DEAD_ISSUER),
        &format!("/auth/callback?state={state}&code=provider-code"),
        Some(&cookie),
    )
    .await;
    assert!(
        replay.restarts_sign_in(),
        "a replayed callback must restart the sign-in, got {} {:?} {}",
        replay.status,
        replay.location,
        replay.body
    );
    assert!(
        replay.cookie(SESSION_COOKIE).is_none(),
        "a replayed callback must never mint a session"
    );
    // The dead issuer is the second proof: had the spent state been let
    // through, the handler would have gone on to the provider and failed
    // there instead of restarting.
    assert!(
        replay.error_code().is_none(),
        "the replay must stop at the state, not reach the provider: {}",
        replay.body
    );
}

/// The restart is one bounce, not a merry-go-round: a browser that cannot keep
/// the state cookie would fail identically on the restarted flow, so the
/// second consecutive failure answers instead of redirecting again.
#[tokio::test]
async fn the_restart_bounces_only_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let state = fresh_state("loop");

    let answer = send(
        oidc_app(pool.clone(), DEAD_ISSUER),
        &format!("/auth/callback?state={state}&code=provider-code"),
        Some(&format!("{STATE_COOKIE}={state}; {RESTART_COOKIE}=1")),
    )
    .await;

    assert_eq!(
        answer.status,
        StatusCode::BAD_REQUEST,
        "a browser already bounced once must be told, not bounced again: {:?}",
        answer.location
    );
    assert_eq!(answer.error_code().as_deref(), Some("login_state_invalid"));
    assert!(answer.location.is_none(), "the loop must stop here");
    let cleared = answer
        .cookie(RESTART_COOKIE)
        .expect("the spent marker must be cleared");
    assert!(
        cleared.contains("Max-Age=0"),
        "a later attempt deserves its own restart: {cleared}"
    );
}

/// The other end of the loop guard: the restart lands on `/auth/login`, and
/// when that cannot mint a fresh state it fails with an error rather than
/// redirecting the browser onward.
#[tokio::test]
async fn a_restart_that_cannot_mint_shows_the_error() {
    if std::env::var("DATABASE_URL").is_err() {
        return;
    }
    let issuer = mock_idp().await;

    let answer = send(oidc_app(unreachable_pool(), &issuer), SIGN_IN_PATH, None).await;

    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.error_code().as_deref(), Some("database_unavailable"));
    assert!(
        answer.location.is_none(),
        "a restart that cannot mint must not redirect anywhere: {:?}",
        answer.location
    );
}

/// The happy path is untouched: a live state is still accepted and still
/// consumed, and the flow carries on to the provider.
#[tokio::test]
async fn a_live_state_is_accepted_and_consumed() {
    let Some(pool) = connect().await else {
        return;
    };
    let state = fresh_state("live");
    seed_state(&pool, &state, "45 minutes").await;

    let answer = send(
        oidc_app(pool.clone(), DEAD_ISSUER),
        &format!("/auth/callback?state={state}&code=provider-code"),
        Some(&format!("{STATE_COOKIE}={state}")),
    )
    .await;

    // Reaching the (unreachable) provider is the proof that the state check
    // passed and the flow went on rather than restarting.
    assert!(
        !answer.restarts_sign_in(),
        "a live state must not be treated as unusable: {:?}",
        answer.location
    );
    assert_eq!(answer.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        answer.error_code().as_deref(),
        Some("oidc_discovery_failed")
    );
    assert!(
        !state_exists(&pool, &state).await,
        "the state must be consumed before the exchange, exactly as before"
    );
}

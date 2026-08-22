//! Tenant-scoped control-plane API for the portal SPA, plus static serving of
//! the built SPA itself.
//!
//! Every query in this module is scoped to the authenticated session's user id
//! inside the SQL (`WHERE user_id = $1` or a join through `api_keys.user_id`);
//! there is no unscoped list surface (docs/ARCHITECTURE.md, "Tenancy").
//! Plaintext API keys are returned exactly once at mint time — only their
//! SHA-256 digests are stored — and keys are disabled, never deleted, because
//! usage history references them.
//!
//! Because disabling is only a flag flip, key CREATION is throttled rather than
//! only key liveness: [`crate::db::admit_key_mint`] counts disabled keys
//! against a trailing window, and the same check guards the device-claim mint
//! path and the playground's implicit key, so no surface can be used to churn
//! keys past a quota.
//!
//! This module serves the portal's control plane and nothing else. It has no
//! inference route and must not gain one: the playground page runs its requests
//! through the public `POST /v1/chat/completions` with a real key, so every
//! admission, cap and settlement invariant applies to it unchanged. See
//! [`PLAYGROUND_KEY_NAME`] for why that is the design rather than a proxy.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserializer;
use serde::{Deserialize, Serialize};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::{
    auth::{generate_api_key, hash_api_key},
    billing::{self, LedgerEntry},
    byok,
    db::{CreditLimitWindow, KeyMintAdmission, admit_key_mint},
    priority::Priority,
    providers,
    session::PortalUser,
    sqlx,
    web::WebCtx,
};

/// Spend counted against a key's `credit_limit_usd` in its CURRENT window, as
/// a SELECT-list expression over an `api_keys` row (migration 0023).
///
/// One definition shared by `list_keys` and `update_key` rather than two
/// copies: both answer the same question about the same row, and the failure
/// mode of letting them drift is a portal that shows one number on the list and
/// another after an edit.
///
/// It reads the SAME derived counters `begin_usage_session` reads, with the
/// same cadence-to-counter mapping and the same UTC window starts. Recomputing
/// it from `usage_events` would be a second definition of "spent this window",
/// and the portal would eventually show a customer a figure that disagreed with
/// the one refusing their requests. Admission's copy is an arm of a hot-path
/// statement built for a single key and cannot share this string; the two are
/// pinned to each other by test instead.
const CREDIT_LIMIT_USED_SQL: &str = r#"
    CASE
        -- No limit, so no window, so nothing has been used OF it. NULL rather
        -- than 0: the key is unlimited, not unspent.
        WHEN api_keys.credit_limit_usd IS NULL THEN NULL
        WHEN api_keys.credit_limit_window IS NULL THEN COALESCE((
            SELECT usage_key_total_spend.spend_usd
            FROM usage_key_total_spend
            WHERE usage_key_total_spend.api_key_id = api_keys.id
        ), 0)
        WHEN api_keys.credit_limit_window = 'monthly' THEN COALESCE((
            SELECT SUM(usage_key_month_spend.spend_usd)
            FROM usage_key_month_spend
            WHERE usage_key_month_spend.api_key_id = api_keys.id
              AND usage_key_month_spend.month >= usage_event_utc_month(NOW())
        ), 0)
        ELSE COALESCE((
            SELECT SUM(usage_key_day_spend.spend_usd)
            FROM usage_key_day_spend
            WHERE usage_key_day_spend.api_key_id = api_keys.id
              AND usage_key_day_spend.day >= CASE api_keys.credit_limit_window
                      WHEN 'daily' THEN usage_event_utc_day(NOW())
                      ELSE (date_trunc('week', NOW() AT TIME ZONE 'UTC'))::DATE
                  END
        ), 0)
    END
"#;

const MAX_KEY_NAME_CHARS: usize = 100;
const MAX_SPEND_CAP_USD: u32 = 10_000;
const MAX_VELOCITY_CAP_TOKENS_PER_MIN: i32 = 2_000_000;
const DEFAULT_USAGE_DAYS: i64 = 30;
const MAX_USAGE_DAYS: i64 = 90;
const DEFAULT_LEDGER_LIMIT: i64 = 50;
const MAX_LEDGER_LIMIT: i64 = 200;
const RECENT_EVENT_LIMIT: i64 = 50;

/// The tenant-scoped `/api` surface. Session authentication (and the CSRF
/// header on mutating methods) is enforced by the [`PortalUser`] extractor on
/// every handler.
pub fn router() -> Router<WebCtx> {
    Router::new()
        .route("/api/me", get(me))
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{id}", delete(disable_key).patch(update_key))
        .route("/api/usage", get(usage))
        .route("/api/billing/ledger", get(ledger))
        .route("/api/byok", get(list_byok).post(attach_byok))
        .route(
            "/api/byok/{provider}",
            delete(remove_byok).patch(set_byok_fallback),
        )
        .route("/api/playground/key", post(ensure_playground_key))
}

/// Static serving for the built portal SPA: files from `dist_path`, with
/// unknown paths falling back to `index.html` so client-side routing works.
///
/// Whether `dist_path` exists is the caller's concern — the integration layer
/// mounts this router only when the directory is present.
pub fn spa_router(dist_path: &std::path::Path) -> Router<()> {
    let service = ServeDir::new(dist_path).fallback(ServeFile::new(dist_path.join("index.html")));
    Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn(stamp_spa_cache_control))
}

/// Cache discipline for the SPA. Vite content-hashes everything under
/// `/assets/`, so those files are immutable — cache them for a year. The HTML
/// shell, and every SPA route that falls back to it, must revalidate on each
/// load (`no-cache` still permits conditional 304s via `Last-Modified`) — or a
/// deploy strands returning browsers on the previous bundle. Found live: the
/// legal-page publish was invisible to a browser that had heuristically cached
/// the shell, because no `Cache-Control` was sent at all and the browser
/// invented its own TTL from `Last-Modified`.
async fn stamp_spa_cache_control(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let immutable = request.uri().path().starts_with("/assets/");
    let mut response = next.run(request).await;
    let value = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(value),
    );
    response
}

#[derive(Debug)]
enum PortalError {
    InvalidRequest(&'static str),
    KeyLimitReached,
    /// The account is frozen (migration 0009), so it may not mint new keys.
    /// Reads are untouched: a frozen customer can still see their balance,
    /// ledger and usage — the freeze blocks spend, not visibility.
    AccountFrozen,
    KeyNotFound,
    /// This deployment has no `BYOK_ENCRYPTION_KEY`, so it cannot seal a
    /// customer credential and must not accept one.
    ByokUnavailable,
    /// The named upstream does not exist here, or cannot take a customer's own
    /// credential (see [`crate::providers::provider_accepts_byok`]).
    ByokProviderUnsupported,
    ByokKeyNotFound,
    Database,
}

impl From<sqlx::Error> for PortalError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "portal database query failed");
        Self::Database
    }
}

impl IntoResponse for PortalError {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            Self::InvalidRequest(message) => (StatusCode::BAD_REQUEST, message, "invalid_request"),
            Self::KeyLimitReached => (
                StatusCode::CONFLICT,
                "This account has reached its API key limit — either too many active keys, \
                 or too many keys created recently. Disabling a key does not raise the \
                 creation limit; wait for the window to pass.",
                "key_limit_reached",
            ),
            Self::AccountFrozen => (
                StatusCode::FORBIDDEN,
                "This account is frozen and cannot create new API keys. A frozen account is \
                 usually the result of a payment dispute or chargeback. Contact ZeroRouter \
                 support to have the freeze reviewed.",
                "account_frozen",
            ),
            Self::KeyNotFound => (
                StatusCode::NOT_FOUND,
                "The API key was not found.",
                "key_not_found",
            ),
            // 501 rather than 404: the endpoint exists and the request was
            // well-formed, but this deployment has not been given the secret
            // that would let it hold a customer credential. A 404 would read
            // as "you asked for the wrong thing" and send someone checking
            // their URL instead of their configuration.
            Self::ByokUnavailable => (
                StatusCode::NOT_IMPLEMENTED,
                "This ZeroRouter deployment is not configured to hold your own provider keys. \
                 Contact the operator if you expected bring-your-own-key to be available.",
                "byok_unavailable",
            ),
            Self::ByokProviderUnsupported => (
                StatusCode::BAD_REQUEST,
                "That provider cannot take your own API key on this deployment. Only providers \
                 listed as bring-your-own-key capable accept one.",
                "byok_provider_unsupported",
            ),
            Self::ByokKeyNotFound => (
                StatusCode::NOT_FOUND,
                "No key is attached for that provider.",
                "byok_key_not_found",
            ),
            Self::Database => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The portal is temporarily unavailable.",
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

#[derive(Serialize)]
struct MeResponse {
    user_id: Uuid,
    email: String,
    credit_balance_usd: Decimal,
    created_at: DateTime<Utc>,
    /// The Stripe publishable key the SPA initializes Stripe.js with, or
    /// `null` when this deployment has no Stripe billing configured.
    ///
    /// This endpoint is where the portal already learns who it is talking to,
    /// so it is also where it learns the one piece of server configuration the
    /// embedded checkout needs. A publishable key is *meant* to be public —
    /// Stripe.js sends it from the browser on every call — so serving it to an
    /// authenticated session discloses nothing. It is not hardcoded in the
    /// bundle because it differs between the test and live accounts, and a
    /// bundle carrying a live key would be wrong the moment it is built for a
    /// sandbox.
    ///
    /// `null` is the honest signal for "billing is off": the Credits page shows
    /// its existing "billing is not enabled" notice rather than mounting a
    /// checkout that could never complete.
    stripe_publishable_key: Option<String>,
    /// Which upstream providers this deployment will accept a customer's own
    /// API key for, or an EMPTY list when BYOK is not configured here.
    ///
    /// Same shape of signal as `stripe_publishable_key` above and for the same
    /// reason: the SPA cannot know from its own bundle whether a feature the
    /// operator has to provision is live, and rendering an attach form that
    /// could only ever fail is worse than rendering nothing. An empty list is
    /// how BYOK ships dark — the portal shows no BYOK section at all.
    ///
    /// It is the provider LIST rather than a boolean because the form needs it
    /// anyway, and one field that answers both "is this on?" and "on for
    /// what?" cannot drift out of agreement with itself.
    byok_providers: Vec<String>,
    /// Where this customer stands against the monthly free BYOK allowance
    /// (migration 0027), or `None` when BYOK is not configured here.
    ///
    /// `None` rather than a zeroed struct, on the same contract as the empty
    /// list above: a deployment without BYOK has no allowance, and reporting
    /// "$5,000 remaining" on one would be advertising a feature it cannot
    /// serve. `skip_serializing_if` keeps the field off those responses
    /// entirely rather than sending an explicit null.
    #[serde(skip_serializing_if = "Option::is_none")]
    byok_allowance: Option<byok::AllowanceStatus>,
    /// Whether this deployment offers the stablecoin deposit rail.
    ///
    /// The same shape of signal as `stripe_publishable_key` and
    /// `byok_providers` above, and it ships dark the same way: FALSE means the
    /// Credits page renders no crypto option at all, rather than a button whose
    /// every press would answer 501.
    ///
    /// It is a plain boolean rather than something richer because there is
    /// nothing else the browser needs. Stablecoin acceptance is a Stripe
    /// payment method on the account ZeroRouter already holds, so unlike BYOK
    /// there is no per-provider list and unlike Stripe.js there is no key to
    /// hand over — the crypto session is created server-side by the same
    /// endpoint the card one is.
    crypto_rail: bool,
}

async fn me(State(ctx): State<WebCtx>, user: PortalUser) -> Result<Json<MeResponse>, PortalError> {
    let created_at =
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT created_at FROM users WHERE id = $1")
            .bind(user.user_id)
            .fetch_one(&ctx.pool)
            .await?;
    let credit_balance_usd = billing::balance(&ctx.pool, user.user_id).await?;
    // Read only where the feature is live, so a deployment without BYOK pays no
    // query for it and this endpoint stays exactly what it was there.
    let byok_allowance = if ctx.byok.is_some() {
        Some(byok::allowance_status(&ctx.pool, user.user_id).await?)
    } else {
        None
    };
    Ok(Json(MeResponse {
        user_id: user.user_id,
        email: user.email,
        credit_balance_usd,
        created_at,
        stripe_publishable_key: ctx
            .config
            .stripe
            .as_ref()
            .map(|stripe| stripe.publishable_key.clone()),
        // Both halves must hold: a keyring to seal with, and a provider that
        // can take a customer key. Either one missing means there is nothing
        // to offer.
        byok_providers: if ctx.byok.is_some() {
            providers::byok_capable_providers()
        } else {
            Vec::new()
        },
        byok_allowance,
        crypto_rail: ctx
            .config
            .stripe
            .as_ref()
            .is_some_and(|stripe| stripe.crypto_rail),
    }))
}

#[derive(Serialize)]
struct KeySummary {
    id: Uuid,
    name: String,
    disabled: bool,
    spend_cap_usd: Decimal,
    velocity_cap_tokens_per_min: i32,
    /// Per-key default for the priority knob; `null` means balanced
    /// (migration 0004). Additive, so pre-knob portal clients are
    /// undisturbed.
    default_priority: Option<Priority>,
    /// When the key stops authenticating; `null` never expires (migration
    /// 0023). Additive, like every field below it.
    expires_at: Option<DateTime<Utc>>,
    /// The customer's own spend cap on this key; `null` is unlimited.
    credit_limit_usd: Option<Decimal>,
    /// The cadence `credit_limit_usd` resets on; `null` never resets.
    credit_limit_window: Option<CreditLimitWindow>,
    /// Spend already counted against `credit_limit_usd` in the CURRENT window
    /// — what the portal shows as "used of limit", and the same number
    /// admission compares. `null` when the key has no limit, because there is
    /// then no window to have used any of.
    ///
    /// Read from the same derived counters admission reads (migration 0023),
    /// not recomputed from the ledger, so the portal cannot display a figure
    /// that disagrees with the one enforcing the limit.
    credit_limit_used_usd: Option<Decimal>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct KeysResponse {
    keys: Vec<KeySummary>,
}

async fn list_keys(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<Json<KeysResponse>, PortalError> {
    let keys = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            bool,
            Decimal,
            i32,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<Decimal>,
            Option<String>,
            Option<Decimal>,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
        ),
    >(&format!(
        r#"
        SELECT id, name, disabled, spend_cap_usd, velocity_cap_tokens_per_min,
               default_priority, expires_at, credit_limit_usd, credit_limit_window,
               {CREDIT_LIMIT_USED_SQL} AS credit_limit_used_usd,
               created_at, last_used_at
        FROM api_keys
        WHERE user_id = $1
        ORDER BY created_at DESC, id DESC
        "#
    ))
    .bind(user.user_id)
    .fetch_all(&ctx.pool)
    .await?
    .into_iter()
    .map(
        |(
            id,
            name,
            disabled,
            spend_cap_usd,
            velocity_cap_tokens_per_min,
            default_priority,
            expires_at,
            credit_limit_usd,
            credit_limit_window,
            credit_limit_used_usd,
            created_at,
            last_used_at,
        )| {
            KeySummary {
                id,
                name,
                disabled,
                spend_cap_usd,
                velocity_cap_tokens_per_min,
                default_priority: default_priority.as_deref().and_then(Priority::from_keyword),
                expires_at,
                credit_limit_usd,
                credit_limit_window: credit_limit_window
                    .as_deref()
                    .and_then(CreditLimitWindow::from_keyword),
                credit_limit_used_usd,
                created_at,
                last_used_at,
            }
        },
    )
    .collect();
    Ok(Json(KeysResponse { keys }))
}

// `Default` is for the unit tests below, which construct this by field and
// would otherwise have to be edited in seven places every time the mint wire
// gains an optional field. It changes no deserialization behavior: serde still
// requires `name` and still refuses an unknown VALUE in any typed field.
#[derive(Default, Deserialize)]
struct CreateKeyRequest {
    name: String,
    spend_cap_usd: Option<Decimal>,
    velocity_cap_tokens_per_min: Option<i32>,
    // Plain `Deserialize`, no `deny_unknown_fields`, so the added field is
    // wire-backward-compatible: a pre-knob portal build simply never sends
    // it. An unknown VALUE is still refused — `Priority` parses strictly.
    default_priority: Option<Priority>,
    /// An absolute RFC 3339 instant, not a preset like "1 week".
    ///
    /// The presets belong to the dialog, which is where OpenRouter keeps them
    /// too: the portal offers 1 hour / 1 day / 1 week / 1 month / never,
    /// computes the instant, and shows the customer the concrete date it is
    /// about to send. The API takes the instant because it is the only shape
    /// that means the same thing twice — "1 week" resolves against whenever
    /// the request happened to arrive, so a retried, queued, or replayed mint
    /// would silently produce a different expiry than the one the customer was
    /// shown. It is also the shape the column stores, so nothing between the
    /// dialog and the row reinterprets it.
    expires_at: Option<DateTime<Utc>>,
    /// The customer's own spend cap for this key; absent is unlimited.
    credit_limit_usd: Option<Decimal>,
    /// Absent means the limit never resets (a lifetime cap on the key), which
    /// is OpenRouter's "N/A" and this API's only spelling of it.
    credit_limit_window: Option<CreditLimitWindow>,
}

struct ValidatedNewKey {
    name: String,
    spend_cap_usd: Option<Decimal>,
    velocity_cap_tokens_per_min: Option<i32>,
    default_priority: Option<Priority>,
    expires_at: Option<DateTime<Utc>>,
    credit_limit_usd: Option<Decimal>,
    credit_limit_window: Option<CreditLimitWindow>,
}

fn validate_new_key(request: &CreateKeyRequest) -> Result<ValidatedNewKey, PortalError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(PortalError::InvalidRequest("name cannot be empty"));
    }
    if name.chars().count() > MAX_KEY_NAME_CHARS {
        return Err(PortalError::InvalidRequest(
            "name cannot exceed 100 characters",
        ));
    }
    if let Some(cap) = request.spend_cap_usd {
        if cap <= Decimal::ZERO {
            return Err(PortalError::InvalidRequest(
                "spend_cap_usd must be positive",
            ));
        }
        if cap > Decimal::from(MAX_SPEND_CAP_USD) {
            return Err(PortalError::InvalidRequest(
                "spend_cap_usd cannot exceed 10000",
            ));
        }
    }
    if let Some(cap) = request.velocity_cap_tokens_per_min {
        if cap <= 0 {
            return Err(PortalError::InvalidRequest(
                "velocity_cap_tokens_per_min must be positive",
            ));
        }
        if cap > MAX_VELOCITY_CAP_TOKENS_PER_MIN {
            return Err(PortalError::InvalidRequest(
                "velocity_cap_tokens_per_min cannot exceed 2000000",
            ));
        }
    }
    // A key that has already expired authenticates nothing, so minting one is
    // never what the caller meant — it is a clock skew, a timezone mistake, or
    // a preset computed against the wrong `now`. Refusing beats handing back a
    // 201 for a key that cannot be used.
    //
    // Compared against the router's clock here and against the DATABASE's in
    // admission. That is a real (if tiny) seam: a router running behind the
    // database could accept an expiry the database already considers past, and
    // mint a key that is instantly refused. It fails CLOSED and it is loud —
    // the customer sees the key refused immediately rather than silently
    // getting more time than they asked for — so it is left as a validation
    // nicety rather than made authoritative.
    if request.expires_at.is_some_and(|at| at <= Utc::now()) {
        return Err(PortalError::InvalidRequest(
            "expires_at must be in the future",
        ));
    }
    if let Some(limit) = request.credit_limit_usd {
        // Zero is refused rather than treated as "block everything" — migration
        // 0023 has the same CHECK, and revocation already says that, in one
        // place, reversibly.
        if limit <= Decimal::ZERO {
            return Err(PortalError::InvalidRequest(
                "credit_limit_usd must be positive",
            ));
        }
        if limit > Decimal::from(MAX_SPEND_CAP_USD) {
            return Err(PortalError::InvalidRequest(
                "credit_limit_usd cannot exceed 10000",
            ));
        }
    }
    // A cadence with no limit to reset is refused rather than dropped. Dropping
    // it would let a request that asked for "$0 every day, resetting" — a
    // window with no limit — come back 201 with an UNLIMITED key, which is the
    // one failure mode a budget feature must not have. Migration 0023 makes the
    // state unrepresentable in the row; this makes the mistake visible to the
    // caller instead of silently widening it.
    if request.credit_limit_window.is_some() && request.credit_limit_usd.is_none() {
        return Err(PortalError::InvalidRequest(
            "credit_limit_window requires credit_limit_usd",
        ));
    }
    Ok(ValidatedNewKey {
        name: name.to_owned(),
        spend_cap_usd: request.spend_cap_usd,
        velocity_cap_tokens_per_min: request.velocity_cap_tokens_per_min,
        default_priority: request.default_priority,
        expires_at: request.expires_at,
        credit_limit_usd: request.credit_limit_usd,
        credit_limit_window: request.credit_limit_window,
    })
}

#[derive(Serialize)]
struct CreatedKeyResponse {
    id: Uuid,
    /// The plaintext key. Returned exactly once; only its digest is stored.
    api_key: String,
    name: String,
    disabled: bool,
    spend_cap_usd: Decimal,
    velocity_cap_tokens_per_min: i32,
    default_priority: Option<Priority>,
    /// The 0023 fields echoed back exactly as stored, so the dialog can show
    /// the customer the expiry the server actually recorded rather than the one
    /// the browser computed.
    expires_at: Option<DateTime<Utc>>,
    credit_limit_usd: Option<Decimal>,
    credit_limit_window: Option<CreditLimitWindow>,
    /// Always `0` for a key with a limit and `null` for one without: a key that
    /// has just been minted has spent nothing. Present so a freshly created key
    /// has the same shape as a listed one and the SPA needs no second branch.
    credit_limit_used_usd: Option<Decimal>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

async fn create_key(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Json(request): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<CreatedKeyResponse>), PortalError> {
    let validated = validate_new_key(&request)?;

    let mut transaction = ctx.pool.begin().await?;
    // Serialize concurrent mints for this user so neither the active-key cap
    // nor the creation throttle can be exceeded by a race between two counting
    // transactions.
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(user.user_id)
        .fetch_one(&mut *transaction)
        .await?;
    // Shared with the device-claim mint path (`crate::device`), so a device
    // grant can no longer mint past a limit the portal enforces. Counts
    // disabled keys against a trailing creation window, which is what makes
    // disable-and-remint stop resetting the limit.
    // Exhaustive on purpose: a new refusal reason must be answered here rather
    // than falling through to a mint.
    match admit_key_mint(&mut transaction, user.user_id).await? {
        KeyMintAdmission::Allowed => {}
        KeyMintAdmission::LimitReached => return Err(PortalError::KeyLimitReached),
        KeyMintAdmission::AccountFrozen => return Err(PortalError::AccountFrozen),
    }

    let api_key = generate_api_key();
    let key_id = Uuid::new_v4();
    // The 0023 columns go in the INSERT rather than the COALESCE update below,
    // because for them NULL is a VALUE — "never expires", "unlimited" — not an
    // absent override to be defaulted away. `COALESCE($n, expires_at)` would
    // read the column's own NULL and happen to produce the right answer here,
    // but only by coincidence of this being an insert; stating them directly
    // means the two groups of columns cannot be confused for each other later.
    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, user_id, key_hash, name,
            expires_at, credit_limit_usd, credit_limit_window
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(key_id)
    .bind(user.user_id)
    .bind(hash_api_key(&api_key))
    .bind(&validated.name)
    .bind(validated.expires_at)
    .bind(validated.credit_limit_usd)
    .bind(validated.credit_limit_window.map(CreditLimitWindow::as_str))
    .execute(&mut *transaction)
    .await?;
    if validated.spend_cap_usd.is_some()
        || validated.velocity_cap_tokens_per_min.is_some()
        || validated.default_priority.is_some()
    {
        sqlx::query(
            r#"
            UPDATE api_keys
            SET
                spend_cap_usd = COALESCE($2, spend_cap_usd),
                velocity_cap_tokens_per_min = COALESCE($3, velocity_cap_tokens_per_min),
                default_priority = COALESCE($4, default_priority)
            WHERE id = $1
            "#,
        )
        .bind(key_id)
        .bind(validated.spend_cap_usd)
        .bind(validated.velocity_cap_tokens_per_min)
        .bind(validated.default_priority.map(Priority::as_str))
        .execute(&mut *transaction)
        .await?;
    }
    let (spend_cap_usd, velocity_cap_tokens_per_min, created_at) =
        sqlx::query_as::<_, (Decimal, i32, DateTime<Utc>)>(
            r#"
            SELECT spend_cap_usd, velocity_cap_tokens_per_min, created_at
            FROM api_keys
            WHERE id = $1
            "#,
        )
        .bind(key_id)
        .fetch_one(&mut *transaction)
        .await?;
    transaction.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyResponse {
            id: key_id,
            api_key,
            name: validated.name,
            disabled: false,
            spend_cap_usd,
            velocity_cap_tokens_per_min,
            default_priority: validated.default_priority,
            expires_at: validated.expires_at,
            credit_limit_usd: validated.credit_limit_usd,
            credit_limit_window: validated.credit_limit_window,
            credit_limit_used_usd: validated.credit_limit_usd.map(|_| Decimal::ZERO),
            created_at,
            last_used_at: None,
        }),
    ))
}

// ---------------------------------------------------------------------------
// The playground key (the portal's /playground page)
//
// The playground runs inference against the SAME `POST /v1/chat/completions`
// every customer calls, presenting a real `zcr_` key in an
// `Authorization: Bearer` header. That is the entire design, and the endpoint
// below is the entire server surface it needs: there is deliberately NO
// session-authenticated inference route, and adding one later would undo this.
//
// Why not a proxy the browser session could POST to directly. Admission is
// keyed on a PRESENTING KEY, never on a user. `crate::db::begin_usage_session`
// locks an `api_keys` row and reads `spend_cap_usd`, `credit_limit_usd`,
// `credit_limit_window` and `velocity_cap_tokens_per_min` off it, and
// `usage_events.api_key_id` is what attributes the spend afterwards. A session
// cookie carries none of that. So a proxy would have to either
//
//   - invent an admission identity, which means a second implementation of
//     every ceiling in `begin_usage_session` — precisely the second admission
//     path the settlement invariants (AGENTS.md) exist to prevent; or
//   - quietly spend against some key the customer minted for something else,
//     under caps they chose for that other thing, and file the usage under it.
//
// The first is unsafe and the second is dishonest. So the playground gets a key
// of its own — an ordinary one, which the customer can see in their key list
// and revoke like any other.
// ---------------------------------------------------------------------------

/// The name the playground's key carries in the customer's key list.
///
/// An ordinary name on an ordinary row, not a marker the rest of the system
/// reads: nothing in authentication, admission, settlement or attribution
/// branches on it. It exists so a customer scanning the Keys page can tell what
/// minted this key and revoke it deliberately, and so this endpoint can find
/// the one it is replacing.
pub const PLAYGROUND_KEY_NAME: &str = "playground";

/// Ensure this account holds exactly one live playground key, and hand back its
/// plaintext.
///
/// **Idempotent in STATE, not in secret**, and that distinction is forced by
/// the storage model rather than chosen. Only `sha256(key)` is kept, so this
/// endpoint *cannot* return the plaintext of a key that already exists — the
/// same property that makes "shown once" true for every other key. What it can
/// guarantee, and does, is that however many times it is called the account is
/// left holding exactly one live key named [`PLAYGROUND_KEY_NAME`]. Each call
/// replaces the previous one, which is why the browser asks for a key only when
/// it has none rather than on every page load.
///
/// The revoke runs BEFORE [`admit_key_mint`], and the ordering is load-bearing
/// in both directions:
///
/// - It relaxes the ACTIVE-key cap, which is right. Replacing one key with
///   another nets zero live keys, so a customer sitting at
///   [`crate::db::MAX_ACTIVE_KEYS_PER_USER`] must not be refused the one action
///   that would not increase their count.
/// - It does NOT relax the CREATION throttle, which is also right, and is the
///   whole reason [`crate::db::MAX_KEYS_CREATED_PER_WINDOW`] counts disabled
///   keys over a trailing window. Churning playground keys is exactly the
///   pattern that throttle bounds, and this path must be no cheaper than
///   `POST /api/keys`.
///
/// A refusal returns `Err` before the commit, so the transaction — the revoke
/// included — rolls back. A customer who meets the throttle keeps the
/// playground key they already had rather than losing it to a replacement that
/// never arrived.
async fn ensure_playground_key(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<(StatusCode, Json<CreatedKeyResponse>), PortalError> {
    let mut transaction = ctx.pool.begin().await?;
    // The same row lock `create_key` takes, for the same reason: the counting
    // inside `admit_key_mint` is only a limit if concurrent mints serialize.
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(user.user_id)
        .fetch_one(&mut *transaction)
        .await?;
    // Scoped by name AND user, and it disables rather than deletes, on the same
    // contract the rest of this module keeps: usage history references keys.
    sqlx::query(
        r#"
        UPDATE api_keys
        SET disabled = TRUE
        WHERE user_id = $1 AND name = $2 AND NOT disabled
        "#,
    )
    .bind(user.user_id)
    .bind(PLAYGROUND_KEY_NAME)
    .execute(&mut *transaction)
    .await?;
    // Exhaustive for the reason `create_key` states: a new refusal reason must
    // be answered here rather than falling through to a mint.
    match admit_key_mint(&mut transaction, user.user_id).await? {
        KeyMintAdmission::Allowed => {}
        KeyMintAdmission::LimitReached => return Err(PortalError::KeyLimitReached),
        KeyMintAdmission::AccountFrozen => return Err(PortalError::AccountFrozen),
    }

    let api_key = generate_api_key();
    let key_id = Uuid::new_v4();
    // Default caps, stated by omission: no expiry, no per-key credit limit, and
    // the column defaults for `spend_cap_usd` and `velocity_cap_tokens_per_min`
    // — the same key `POST /api/keys` mints when the dialog is left alone. The
    // playground is a client, not a privileged one, and must not be a cheaper
    // one either.
    sqlx::query("INSERT INTO api_keys (id, user_id, key_hash, name) VALUES ($1, $2, $3, $4)")
        .bind(key_id)
        .bind(user.user_id)
        .bind(hash_api_key(&api_key))
        .bind(PLAYGROUND_KEY_NAME)
        .execute(&mut *transaction)
        .await?;
    let (spend_cap_usd, velocity_cap_tokens_per_min, created_at) =
        sqlx::query_as::<_, (Decimal, i32, DateTime<Utc>)>(
            r#"
            SELECT spend_cap_usd, velocity_cap_tokens_per_min, created_at
            FROM api_keys
            WHERE id = $1
            "#,
        )
        .bind(key_id)
        .fetch_one(&mut *transaction)
        .await?;
    transaction.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyResponse {
            id: key_id,
            api_key,
            name: PLAYGROUND_KEY_NAME.to_owned(),
            disabled: false,
            spend_cap_usd,
            velocity_cap_tokens_per_min,
            default_priority: None,
            expires_at: None,
            credit_limit_usd: None,
            credit_limit_window: None,
            credit_limit_used_usd: None,
            created_at,
            last_used_at: None,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Bring-your-own-key (migration 0026)
//
// Three handlers, and the shape of the response is the security control: a
// stored credential is never returned by any of them, at any time, including
// the moment it is attached. `ByokKeySummary` has no field that could carry
// one — which is a stronger guarantee than remembering not to populate one,
// because there is nothing to populate. The attach handler answers with the
// same summary the listing does, so "what the customer sees after saving" and
// "what the customer sees later" are one type with one definition.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ByokKeySummary {
    provider: String,
    /// A truncated SHA-256 of the credential — an identifier for support and
    /// for the customer's own eyes, never an authenticator, and not reversible
    /// into the key.
    fingerprint: String,
    /// The trailing four characters, matching what the provider's own
    /// dashboard shows so a customer can tell which of their keys this is.
    last4: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    /// Whether this key falls back to ZeroRouter's own credential when it fails
    /// upstream (migration 0028). Returned because the consequence is a bill:
    /// a fallback attempt is charged at the FULL catalog price, so a customer
    /// must be able to see which of their keys are in that state without
    /// guessing from a form control's default.
    fallback_enabled: bool,
}

impl From<byok::StoredKey> for ByokKeySummary {
    fn from(key: byok::StoredKey) -> Self {
        Self {
            provider: key.provider,
            fingerprint: key.fingerprint,
            last4: key.last4,
            created_at: key.created_at,
            last_used_at: key.last_used_at,
            fallback_enabled: key.fallback_enabled,
        }
    }
}

/// The body of the fallback toggle. One field, and it is explicit rather than a
/// bare toggle endpoint: a request that says which state it wants is idempotent
/// and cannot be doubled by a retry into the opposite of what the customer
/// clicked.
#[derive(Deserialize)]
struct ByokFallbackRequest {
    enabled: bool,
}

#[derive(Serialize)]
struct ByokKeysResponse {
    keys: Vec<ByokKeySummary>,
}

#[derive(Deserialize)]
struct AttachByokRequest {
    provider: String,
    /// The customer's own provider credential. Read once, sealed, and dropped:
    /// it is never echoed back, never logged, and never stored in plaintext.
    api_key: String,
}

async fn list_byok(
    State(ctx): State<WebCtx>,
    user: PortalUser,
) -> Result<Json<ByokKeysResponse>, PortalError> {
    // An unconfigured deployment lists nothing rather than erroring: a GET is
    // the SPA asking "what do I have", and "nothing" is the true answer
    // whether the reason is no keys or no keyring.
    if ctx.byok.is_none() {
        return Ok(Json(ByokKeysResponse { keys: Vec::new() }));
    }
    let keys = byok::list_keys(&ctx.pool, user.user_id).await?;
    Ok(Json(ByokKeysResponse {
        keys: keys.into_iter().map(ByokKeySummary::from).collect(),
    }))
}

async fn attach_byok(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Json(request): Json<AttachByokRequest>,
) -> Result<(StatusCode, Json<ByokKeySummary>), PortalError> {
    let Some(keyring) = ctx.byok.as_deref() else {
        return Err(PortalError::ByokUnavailable);
    };
    let provider = request.provider.trim();
    if !providers::provider_accepts_byok(provider) {
        return Err(PortalError::ByokProviderUnsupported);
    }
    // Shape-checked before it is sealed, so the common paste mistakes (a
    // trailing newline, a truncated copy) are answered here rather than as an
    // undiagnosable upstream 401 on the customer's next request.
    let credential = request.api_key.trim();
    byok::validate_credential(credential).map_err(PortalError::InvalidRequest)?;

    let stored = byok::attach_key(&ctx.pool, keyring, user.user_id, provider, credential)
        .await
        .map_err(|error| {
            // The error is logged WITHOUT its context chain and the credential
            // is not in scope for the message. `anyhow`'s Display would carry
            // whatever the sealing or database layer attached, and this is the
            // one handler in the module where a stray value in an error string
            // would be a third party's API key.
            tracing::error!(provider = %provider, "attaching a BYOK credential failed: {}", error.root_cause());
            PortalError::Database
        })?;
    Ok((StatusCode::CREATED, Json(ByokKeySummary::from(stored))))
}

async fn remove_byok(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Path(provider): Path<String>,
) -> Result<StatusCode, PortalError> {
    // Detach works even with no keyring configured, deliberately: a customer
    // asking ZeroRouter to stop holding their vendor credential must always be
    // able to, including after an operator has removed the secret that would
    // let it be read. Refusing here would leave a sealed third-party key in the
    // database with no way to delete it from the portal.
    if !byok::remove_key(&ctx.pool, user.user_id, provider.trim()).await? {
        return Err(PortalError::ByokKeyNotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Turn the house-credential fallback on or off for one attached key
/// (migration 0028).
///
/// Tenant-scoped by the same `user_id` predicate every handler here uses, so a
/// customer naming another customer's provider gets the same
/// `ByokKeyNotFound` a nonexistent one gets — the refusal is not an oracle for
/// what other tenants have attached.
///
/// No keyring is required. This changes a preference about what happens when a
/// credential fails, not the credential, so it stays available on a deployment
/// that has removed the secret — for the same reason detaching does.
async fn set_byok_fallback(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Path(provider): Path<String>,
    Json(request): Json<ByokFallbackRequest>,
) -> Result<StatusCode, PortalError> {
    if !byok::set_fallback(&ctx.pool, user.user_id, provider.trim(), request.enabled).await? {
        return Err(PortalError::ByokKeyNotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn disable_key(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, PortalError> {
    // Keys are disabled, never deleted: usage_events rows reference them. The
    // user_id predicate makes a foreign key id indistinguishable from a
    // missing one.
    //
    // The 204 this returns is a promise that no further request can dispatch on
    // the key, and the row lock this UPDATE takes is what keeps it. Admission
    // ([`crate::db::begin_usage_session`]) re-checks `NOT disabled` inside its
    // own conditional UPDATE against the same row, so the two serialize: either
    // this commits first and the racing admission re-evaluates its predicate
    // against `disabled = TRUE` and refuses, or admission commits first and this
    // statement waits behind it — the operator is not told the key is revoked
    // until the request that beat them has already been admitted. No explicit
    // lock is needed here beyond the one the UPDATE already takes; adding the
    // per-user advisory lock would only invert this crate's advisory-then-row
    // ordering and create a deadlock cycle with admission.
    //
    // What this does NOT promise: a request already dispatched upstream keeps
    // running, and [`crate::auth`]'s 30-second key cache means a revoked key can
    // still pass authentication (and reach endpoints that never admit, such as
    // model listing) until its cache entry expires. Revocation is immediate for
    // *dispatch*, which is what costs money, not for every byte of the surface.
    let result = sqlx::query(
        "UPDATE api_keys SET disabled = TRUE WHERE id = $1 AND user_id = $2 AND NOT disabled",
    )
    .bind(key_id)
    .bind(user.user_id)
    .execute(&ctx.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(PortalError::KeyNotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/keys/{id}` — the portal's first post-mint key mutation
/// (rollout stage 3a), accepting `default_priority` alone until a second
/// field earns its place.
///
/// PATCH semantics are field-presence semantics, so the field is a double
/// `Option`: absent = leave unchanged (a `{}` PATCH is a no-op that returns
/// the current summary), `null` = clear back to balanced, a keyword = set.
/// `deny_unknown_fields` keeps a typo'd or premature field a loud 400, the
/// same contract as the request-side `zerorouter` object.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateKeyRequest {
    #[serde(default, deserialize_with = "present_field")]
    default_priority: Option<Option<Priority>>,
}

/// Wrap a present-but-possibly-null field as `Some(inner)`, so absence
/// (`None` via `serde(default)`) stays distinguishable from an explicit
/// `null` (`Some(None)`).
fn present_field<'de, D>(deserializer: D) -> Result<Option<Option<Priority>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<Priority>::deserialize(deserializer).map(Some)
}

async fn update_key(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Path(key_id): Path<Uuid>,
    Json(request): Json<UpdateKeyRequest>,
) -> Result<Json<KeySummary>, PortalError> {
    if let Some(default_priority) = request.default_priority {
        // Disabled keys stay patchable: the flag governs dispatch, not
        // ownership, and a metadata edit on a disabled key is coherent (it
        // stays disabled). The user_id predicate is the tenancy wall, as in
        // `disable_key`.
        let result =
            sqlx::query("UPDATE api_keys SET default_priority = $3 WHERE id = $1 AND user_id = $2")
                .bind(key_id)
                .bind(user.user_id)
                .bind(default_priority.map(Priority::as_str))
                .execute(&ctx.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(PortalError::KeyNotFound);
        }
    }
    let row = sqlx::query_as::<
        _,
        (
            String,
            bool,
            Decimal,
            i32,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<Decimal>,
            Option<String>,
            Option<Decimal>,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
        ),
    >(&format!(
        r#"
        SELECT name, disabled, spend_cap_usd, velocity_cap_tokens_per_min,
               default_priority, expires_at, credit_limit_usd, credit_limit_window,
               {CREDIT_LIMIT_USED_SQL} AS credit_limit_used_usd,
               created_at, last_used_at
        FROM api_keys
        WHERE id = $1 AND user_id = $2
        "#
    ))
    .bind(key_id)
    .bind(user.user_id)
    .fetch_optional(&ctx.pool)
    .await?
    .ok_or(PortalError::KeyNotFound)?;
    let (
        name,
        disabled,
        spend_cap_usd,
        velocity_cap_tokens_per_min,
        default_priority,
        expires_at,
        credit_limit_usd,
        credit_limit_window,
        credit_limit_used_usd,
        created_at,
        last_used_at,
    ) = row;
    Ok(Json(KeySummary {
        id: key_id,
        name,
        disabled,
        spend_cap_usd,
        velocity_cap_tokens_per_min,
        default_priority: default_priority.as_deref().and_then(Priority::from_keyword),
        expires_at,
        credit_limit_usd,
        credit_limit_window: credit_limit_window
            .as_deref()
            .and_then(CreditLimitWindow::from_keyword),
        credit_limit_used_usd,
        created_at,
        last_used_at,
    }))
}

#[derive(Deserialize)]
struct UsageParams {
    days: Option<String>,
}

#[derive(Serialize)]
struct UsageTotals {
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: Decimal,
}

#[derive(Serialize)]
struct DailyUsage {
    date: NaiveDate,
    requests: i64,
    cost_usd: Decimal,
}

#[derive(Serialize)]
struct RecentEvent {
    request_id: Uuid,
    ts: DateTime<Utc>,
    tier: String,
    upstream_provider: String,
    upstream_model: String,
    input_tokens: i32,
    cached_input_tokens: i32,
    output_tokens: i32,
    cost_usd: Decimal,
    latency_ms: i32,
    status: i16,
    key_name: String,
}

#[derive(Serialize)]
struct UsageResponse {
    days: i64,
    totals: UsageTotals,
    daily: Vec<DailyUsage>,
    recent: Vec<RecentEvent>,
}

async fn usage(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Query(params): Query<UsageParams>,
) -> Result<Json<UsageResponse>, PortalError> {
    let days = parse_bounded(
        params.days.as_deref(),
        DEFAULT_USAGE_DAYS,
        1,
        MAX_USAGE_DAYS,
        "days must be a whole number",
    )?;

    let (requests, input_tokens, output_tokens, cost_usd) =
        sqlx::query_as::<_, (i64, i64, i64, Decimal)>(
            r#"
            SELECT
                COUNT(*),
                COALESCE(SUM(usage_events.input_tokens), 0)::BIGINT,
                COALESCE(SUM(usage_events.output_tokens), 0)::BIGINT,
                COALESCE(SUM(usage_events.cost_usd), 0)
            FROM usage_events
            INNER JOIN api_keys ON api_keys.id = usage_events.api_key_id
            WHERE api_keys.user_id = $1
              AND usage_events.ts >= NOW() - ($2 * INTERVAL '1 day')
            "#,
        )
        .bind(user.user_id)
        .bind(days)
        .fetch_one(&ctx.pool)
        .await?;

    let daily = sqlx::query_as::<_, (NaiveDate, i64, Decimal)>(
        r#"
        SELECT
            (usage_events.ts AT TIME ZONE 'UTC')::DATE AS day,
            COUNT(*),
            COALESCE(SUM(usage_events.cost_usd), 0)
        FROM usage_events
        INNER JOIN api_keys ON api_keys.id = usage_events.api_key_id
        WHERE api_keys.user_id = $1
          AND usage_events.ts >= NOW() - ($2 * INTERVAL '1 day')
        GROUP BY day
        ORDER BY day DESC
        "#,
    )
    .bind(user.user_id)
    .bind(days)
    .fetch_all(&ctx.pool)
    .await?
    .into_iter()
    .map(|(date, requests, cost_usd)| DailyUsage {
        date,
        requests,
        cost_usd,
    })
    .collect();

    let recent = sqlx::query_as::<
        _,
        (
            Uuid,
            DateTime<Utc>,
            String,
            String,
            String,
            i32,
            i32,
            i32,
            Decimal,
            i32,
            i16,
            String,
        ),
    >(
        r#"
        SELECT
            usage_events.request_id,
            usage_events.ts,
            usage_events.tier,
            usage_events.upstream_provider,
            usage_events.upstream_model,
            usage_events.input_tokens,
            usage_events.cached_input_tokens,
            usage_events.output_tokens,
            usage_events.cost_usd,
            usage_events.latency_ms,
            usage_events.status,
            api_keys.name
        FROM usage_events
        INNER JOIN api_keys ON api_keys.id = usage_events.api_key_id
        WHERE api_keys.user_id = $1
          AND usage_events.ts >= NOW() - ($2 * INTERVAL '1 day')
        ORDER BY usage_events.ts DESC, usage_events.id DESC
        LIMIT $3
        "#,
    )
    .bind(user.user_id)
    .bind(days)
    .bind(RECENT_EVENT_LIMIT)
    .fetch_all(&ctx.pool)
    .await?
    .into_iter()
    .map(
        |(
            request_id,
            ts,
            tier,
            upstream_provider,
            upstream_model,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            cost_usd,
            latency_ms,
            status,
            key_name,
        )| RecentEvent {
            request_id,
            ts,
            tier,
            upstream_provider,
            upstream_model,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            cost_usd,
            latency_ms,
            status,
            key_name,
        },
    )
    .collect();

    Ok(Json(UsageResponse {
        days,
        totals: UsageTotals {
            requests,
            input_tokens,
            output_tokens,
            cost_usd,
        },
        daily,
        recent,
    }))
}

#[derive(Deserialize)]
struct LedgerParams {
    limit: Option<String>,
}

#[derive(Serialize)]
struct LedgerResponse {
    limit: i64,
    entries: Vec<LedgerEntry>,
}

async fn ledger(
    State(ctx): State<WebCtx>,
    user: PortalUser,
    Query(params): Query<LedgerParams>,
) -> Result<Json<LedgerResponse>, PortalError> {
    let limit = parse_bounded(
        params.limit.as_deref(),
        DEFAULT_LEDGER_LIMIT,
        1,
        MAX_LEDGER_LIMIT,
        "limit must be a whole number",
    )?;
    let entries = billing::ledger_entries(&ctx.pool, user.user_id, limit).await?;
    Ok(Json(LedgerResponse { limit, entries }))
}

/// Parse an optional query parameter as an integer, clamping in-range values
/// and rejecting anything non-numeric. Absent means the default.
fn parse_bounded(
    raw: Option<&str>,
    default: i64,
    min: i64,
    max: i64,
    message: &'static str,
) -> Result<i64, PortalError> {
    match raw {
        None => Ok(default),
        Some(text) => text
            .trim()
            .parse::<i64>()
            .map(|value| value.clamp(min, max))
            .map_err(|_| PortalError::InvalidRequest(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_parsing_defaults_clamps_and_rejects() {
        assert!(matches!(parse_bounded(None, 30, 1, 90, "m"), Ok(30)));
        assert!(matches!(parse_bounded(Some("7"), 30, 1, 90, "m"), Ok(7)));
        assert!(matches!(parse_bounded(Some("0"), 30, 1, 90, "m"), Ok(1)));
        assert!(matches!(
            parse_bounded(Some("9999"), 30, 1, 90, "m"),
            Ok(90)
        ));
        assert!(matches!(parse_bounded(Some("-3"), 30, 1, 90, "m"), Ok(1)));
        assert!(matches!(
            parse_bounded(Some("abc"), 30, 1, 90, "m"),
            Err(PortalError::InvalidRequest(_))
        ));
        assert!(matches!(
            parse_bounded(Some(""), 30, 1, 90, "m"),
            Err(PortalError::InvalidRequest(_))
        ));
    }

    #[test]
    fn new_key_validation_enforces_name_and_cap_limits() {
        let valid = validate_new_key(&CreateKeyRequest {
            name: "  ci key  ".to_owned(),
            spend_cap_usd: Some(Decimal::from(5)),
            velocity_cap_tokens_per_min: Some(1_000),
            default_priority: Some(Priority::Cost),
            ..CreateKeyRequest::default()
        })
        .expect("a well-formed key request should validate");
        assert_eq!(valid.name, "ci key");
        assert_eq!(valid.spend_cap_usd, Some(Decimal::from(5)));
        assert_eq!(valid.velocity_cap_tokens_per_min, Some(1_000));
        assert_eq!(valid.default_priority, Some(Priority::Cost));

        let rejects = [
            CreateKeyRequest {
                name: "   ".to_owned(),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "n".repeat(MAX_KEY_NAME_CHARS + 1),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                spend_cap_usd: Some(Decimal::ZERO),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                spend_cap_usd: Some(Decimal::from(MAX_SPEND_CAP_USD) + Decimal::ONE),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                velocity_cap_tokens_per_min: Some(0),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                velocity_cap_tokens_per_min: Some(MAX_VELOCITY_CAP_TOKENS_PER_MIN + 1),
                ..CreateKeyRequest::default()
            },
        ];
        for request in &rejects {
            assert!(matches!(
                validate_new_key(request),
                Err(PortalError::InvalidRequest(_))
            ));
        }
    }

    /// The 0023 mint fields, at the boundaries that decide whether a key can be
    /// used at all or can spend without limit.
    #[test]
    fn new_key_validation_enforces_expiry_and_credit_limit() {
        let valid = validate_new_key(&CreateKeyRequest {
            name: "contractor".to_owned(),
            expires_at: Some(Utc::now() + chrono::Duration::days(7)),
            credit_limit_usd: Some(Decimal::from(25)),
            credit_limit_window: Some(CreditLimitWindow::Weekly),
            ..CreateKeyRequest::default()
        })
        .expect("a well-formed limited key should validate");
        assert!(valid.expires_at.is_some());
        assert_eq!(valid.credit_limit_usd, Some(Decimal::from(25)));
        assert_eq!(valid.credit_limit_window, Some(CreditLimitWindow::Weekly));

        // Absent everywhere is the unlimited, never-expiring key every caller
        // minted before 0023, and it must stay valid.
        let unlimited = validate_new_key(&CreateKeyRequest {
            name: "plain".to_owned(),
            ..CreateKeyRequest::default()
        })
        .expect("a key with no expiry and no limit should validate");
        assert_eq!(unlimited.expires_at, None);
        assert_eq!(unlimited.credit_limit_usd, None);
        assert_eq!(unlimited.credit_limit_window, None);

        // A limit with no cadence is the lifetime cap, not an error.
        let lifetime = validate_new_key(&CreateKeyRequest {
            name: "lifetime".to_owned(),
            credit_limit_usd: Some(Decimal::ONE),
            ..CreateKeyRequest::default()
        })
        .expect("a limit with no window is the lifetime cap");
        assert_eq!(lifetime.credit_limit_usd, Some(Decimal::ONE));
        assert_eq!(lifetime.credit_limit_window, None);

        let rejects = [
            // Already lapsed: the key would authenticate nothing.
            CreateKeyRequest {
                name: "ok".to_owned(),
                expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
                ..CreateKeyRequest::default()
            },
            // Zero is a revocation wearing a budget's clothes.
            CreateKeyRequest {
                name: "ok".to_owned(),
                credit_limit_usd: Some(Decimal::ZERO),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                credit_limit_usd: Some(-Decimal::ONE),
                ..CreateKeyRequest::default()
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                credit_limit_usd: Some(Decimal::from(MAX_SPEND_CAP_USD) + Decimal::ONE),
                ..CreateKeyRequest::default()
            },
            // The one that must never be silently dropped: a cadence with no
            // limit would otherwise mint an UNLIMITED key from a request that
            // plainly asked for a budget.
            CreateKeyRequest {
                name: "ok".to_owned(),
                credit_limit_window: Some(CreditLimitWindow::Daily),
                ..CreateKeyRequest::default()
            },
        ];
        for request in &rejects {
            assert!(
                matches!(
                    validate_new_key(request),
                    Err(PortalError::InvalidRequest(_))
                ),
                "request for {:?} should have been refused",
                request.name
            );
        }
    }
}

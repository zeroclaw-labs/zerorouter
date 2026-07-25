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
//! path, so neither surface can be used to churn keys past a quota.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::{
    auth::{generate_api_key, hash_api_key},
    billing::{self, LedgerEntry},
    db::{KeyMintAdmission, admit_key_mint},
    session::PortalUser,
    sqlx,
    web::WebCtx,
};

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
        .route("/api/keys/{id}", delete(disable_key))
        .route("/api/usage", get(usage))
        .route("/api/billing/ledger", get(ledger))
}

/// Static serving for the built portal SPA: files from `dist_path`, with
/// unknown paths falling back to `index.html` so client-side routing works.
///
/// Whether `dist_path` exists is the caller's concern — the integration layer
/// mounts this router only when the directory is present.
pub fn spa_router(dist_path: &std::path::Path) -> Router<()> {
    let service = ServeDir::new(dist_path).fallback(ServeFile::new(dist_path.join("index.html")));
    Router::new().fallback_service(service)
}

#[derive(Debug)]
enum PortalError {
    InvalidRequest(&'static str),
    KeyLimitReached,
    KeyNotFound,
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
            Self::KeyNotFound => (
                StatusCode::NOT_FOUND,
                "The API key was not found.",
                "key_not_found",
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
}

async fn me(State(ctx): State<WebCtx>, user: PortalUser) -> Result<Json<MeResponse>, PortalError> {
    let created_at =
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT created_at FROM users WHERE id = $1")
            .bind(user.user_id)
            .fetch_one(&ctx.pool)
            .await?;
    let credit_balance_usd = billing::balance(&ctx.pool, user.user_id).await?;
    Ok(Json(MeResponse {
        user_id: user.user_id,
        email: user.email,
        credit_balance_usd,
        created_at,
    }))
}

#[derive(Serialize)]
struct KeySummary {
    id: Uuid,
    name: String,
    disabled: bool,
    spend_cap_usd: Decimal,
    velocity_cap_tokens_per_min: i32,
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
            DateTime<Utc>,
            Option<DateTime<Utc>>,
        ),
    >(
        r#"
        SELECT id, name, disabled, spend_cap_usd, velocity_cap_tokens_per_min,
               created_at, last_used_at
        FROM api_keys
        WHERE user_id = $1
        ORDER BY created_at DESC, id DESC
        "#,
    )
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
            created_at,
            last_used_at,
        )| {
            KeySummary {
                id,
                name,
                disabled,
                spend_cap_usd,
                velocity_cap_tokens_per_min,
                created_at,
                last_used_at,
            }
        },
    )
    .collect();
    Ok(Json(KeysResponse { keys }))
}

#[derive(Deserialize)]
struct CreateKeyRequest {
    name: String,
    spend_cap_usd: Option<Decimal>,
    velocity_cap_tokens_per_min: Option<i32>,
}

struct ValidatedNewKey {
    name: String,
    spend_cap_usd: Option<Decimal>,
    velocity_cap_tokens_per_min: Option<i32>,
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
    Ok(ValidatedNewKey {
        name: name.to_owned(),
        spend_cap_usd: request.spend_cap_usd,
        velocity_cap_tokens_per_min: request.velocity_cap_tokens_per_min,
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
    if matches!(
        admit_key_mint(&mut transaction, user.user_id).await?,
        KeyMintAdmission::LimitReached
    ) {
        return Err(PortalError::KeyLimitReached);
    }

    let api_key = generate_api_key();
    let key_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(key_id)
    .bind(user.user_id)
    .bind(hash_api_key(&api_key))
    .bind(&validated.name)
    .execute(&mut *transaction)
    .await?;
    if validated.spend_cap_usd.is_some() || validated.velocity_cap_tokens_per_min.is_some() {
        sqlx::query(
            r#"
            UPDATE api_keys
            SET
                spend_cap_usd = COALESCE($2, spend_cap_usd),
                velocity_cap_tokens_per_min = COALESCE($3, velocity_cap_tokens_per_min)
            WHERE id = $1
            "#,
        )
        .bind(key_id)
        .bind(validated.spend_cap_usd)
        .bind(validated.velocity_cap_tokens_per_min)
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
            created_at,
            last_used_at: None,
        }),
    ))
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
        })
        .expect("a well-formed key request should validate");
        assert_eq!(valid.name, "ci key");
        assert_eq!(valid.spend_cap_usd, Some(Decimal::from(5)));
        assert_eq!(valid.velocity_cap_tokens_per_min, Some(1_000));

        let rejects = [
            CreateKeyRequest {
                name: "   ".to_owned(),
                spend_cap_usd: None,
                velocity_cap_tokens_per_min: None,
            },
            CreateKeyRequest {
                name: "n".repeat(MAX_KEY_NAME_CHARS + 1),
                spend_cap_usd: None,
                velocity_cap_tokens_per_min: None,
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                spend_cap_usd: Some(Decimal::ZERO),
                velocity_cap_tokens_per_min: None,
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                spend_cap_usd: Some(Decimal::from(MAX_SPEND_CAP_USD) + Decimal::ONE),
                velocity_cap_tokens_per_min: None,
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                spend_cap_usd: None,
                velocity_cap_tokens_per_min: Some(0),
            },
            CreateKeyRequest {
                name: "ok".to_owned(),
                spend_cap_usd: None,
                velocity_cap_tokens_per_min: Some(MAX_VELOCITY_CAP_TOKENS_PER_MIN + 1),
            },
        ];
        for request in &rejects {
            assert!(matches!(
                validate_new_key(request),
                Err(PortalError::InvalidRequest(_))
            ));
        }
    }
}

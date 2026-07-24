use std::{borrow::Cow, env, str::FromStr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::json;
// Named directly rather than through the `crate::sqlx` shim: the shim exists to
// keep the sqlx-core/sqlx-postgres split out of call sites, and [`admit_key_mint`]
// is the only place in the crate that needs a bare connection handle.
use sqlx_postgres::PgConnection;
use uuid::Uuid;
use zeroclaw_providers::pricing::ModelRates;

use crate::{
    auth::AuthenticatedKey,
    openai::{OpenAiUsage, usage_cost},
    sqlx::{
        self, PgPool,
        migrate::{Migration, MigrationType, Migrator},
        postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    },
};

/// The permanent Stage-1 reservation sizing basis: byte bound + max_tokens.
/// Learned/quote sizings arrive in later rollout stages (migration 0004
/// comment on `estimator_basis`).
const ESTIMATOR_BASIS_COLD: &str = "cold";

#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub tier: String,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub usage: OpenAiUsage,
    pub cost_usd: Decimal,
    pub latency_ms: i32,
    pub status: i16,
    pub telemetry: RequestTelemetry,
    pub attempts: Vec<AttemptRecord>,
}

/// The estimate-and-select telemetry captured on the append-only
/// `usage_events` row at settle (migration 0004). Every field is written at
/// INSERT time only; the reservation provenance (task signature, reserved
/// output/cost, estimator basis) rides [`UsageSession`] from admission because
/// the reservation row is destroyed by the settle `DELETE...RETURNING`.
///
/// Priority and declared-validator columns are Stage-1-inert (the knob and
/// validators ship in later stages) and are always written NULL here.
#[derive(Clone, Debug)]
pub struct RequestTelemetry {
    // Raw request-shape features, persisted so task signatures can be
    // re-bucketed retroactively. Prompt content itself is never persisted.
    pub requested_max_tokens: i32,
    pub stream: bool,
    pub prompt_bytes: i64,
    pub message_count: i32,
    pub tool_count: i32,
    // Candidate provenance for the row: the served candidate, or the last one
    // tried when the request died mid-walk. `None` only where
    // upstream_provider/upstream_model themselves carry a sentinel because no
    // candidate had been selected ('fallback-chain', 'none'/'client-disconnected').
    pub candidate_id: Option<String>,
    pub basis_rates: Option<ModelRates>,
    pub sell_rates: ModelRates,
    // Synthesized outcome labels; `None` where no completion was produced.
    pub finish_reason: Option<String>,
    pub shape_ok: Option<bool>,
}

/// One row of the append-only `request_attempts` walk ledger (migration
/// 0004), buffered in memory during the router-owned streaming walk and
/// inserted in the same transaction as the `usage_events` row, after it.
#[derive(Clone, Debug)]
pub struct AttemptRecord {
    pub attempt_no: i16,
    pub started_at: DateTime<Utc>,
    pub candidate_id: String,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub outcome: String,
    pub served: bool,
    pub latency_ms: i32,
    /// `None` when the upstream reported no usage for this attempt.
    pub usage: Option<OpenAiUsage>,
    pub tokens_estimated: bool,
    pub cost_basis_usd: Option<Decimal>,
    pub finish_reason: Option<String>,
    pub validator_kind: Option<String>,
}

fn rates_snapshot(rates: &ModelRates) -> String {
    json!({
        "input_per_mtok": rates.input_per_mtok,
        "cached_input_per_mtok": rates.cached_input_per_mtok,
        "output_per_mtok": rates.output_per_mtok,
    })
    .to_string()
}

/// The outcome of a cap/credit admission decision.
///
/// # Quota model
///
/// Spend and velocity are enforced at TWO scopes and the tighter one wins:
///
/// * **per key** — the presenting key's own settled + in-flight usage against
///   its own `api_keys.spend_cap_usd` / `velocity_cap_tokens_per_min`. This is
///   B0's original check, unchanged.
/// * **per user** — every key the owning user holds, settled + in-flight,
///   against a ceiling *derived* from the same columns: the largest cap
///   configured on any of the user's live keys.
///
/// The user scope exists because keys are free and churnable (`disable_key` is
/// a flag flip, `portal::disable_key`), so a per-key-only quota is reset by
/// minting a new key and multiplied by holding several at once. Counting a
/// user's whole key set closes both.
///
/// The ceiling is **derived, not configured**: the schema has no per-user cap
/// column and this change deliberately adds no configuration surface for one.
/// Taking the maximum over the user's live keys means minting more keys can
/// never raise the ceiling past what one key could already reach, while a
/// single-key user's behavior is unchanged (the maximum is that key's own cap).
pub enum UsageAdmission {
    Allowed(UsageSession),
    Unauthorized,
    SpendExceeded,
    VelocityExceeded,
    InsufficientCredits,
}

pub struct UsageSession {
    pool: PgPool,
    reservation_id: Uuid,
    api_key_id: Uuid,
    user_id: Uuid,
    require_credits: bool,
    // Reservation provenance copied into the ledger at settle: the
    // usage_reservations row is destroyed by the settle DELETE...RETURNING, so
    // these must be carried in memory from admission.
    task_signature: String,
    reserved_output_tokens: i32,
    reserved_cost_usd: Decimal,
}

const DATABASE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const RESERVATION_TTL: Duration = Duration::from_secs(20 * 60);

pub async fn database_pool_from_env() -> Result<PgPool> {
    let pool = if let Ok(database_url) = env::var("DATABASE_URL") {
        let mut options = PgConnectOptions::from_str(&database_url)
            .map_err(|_| anyhow!("DATABASE_URL is invalid"))?;
        // The DATABASE_URL path is a developer convenience for a loopback
        // database. A non-loopback target must be at least as hardened as the
        // production DB_* path: verify-full TLS with the pinned CA. Otherwise
        // this branch would silently weaken the documented TLS invariant.
        if !is_loopback_host(options.get_host()) {
            let ssl_root_cert = env::var("DB_SSL_ROOT_CERT")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let Some(ssl_root_cert) = ssl_root_cert else {
                bail!(
                    "DATABASE_URL points at a non-loopback host without DB_SSL_ROOT_CERT; \
                     use the DB_HOST/DB_* variables (which enforce verify-full TLS) for remote databases"
                );
            };
            options = options
                .ssl_mode(PgSslMode::VerifyFull)
                .ssl_root_cert(&ssl_root_cert);
        }
        PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(DATABASE_ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .context("failed to connect using DATABASE_URL")?
    } else {
        let host = required_env("DB_HOST")?;
        let database = required_env("DB_NAME")?;
        let username = required_env("DB_USERNAME")?;
        let password = required_env("DB_PASSWORD")?;
        let ssl_root_cert = required_env("DB_SSL_ROOT_CERT")?;
        let port = env::var("DB_PORT")
            .unwrap_or_else(|_| "5432".to_owned())
            .parse::<u16>()
            .context("DB_PORT must be a valid TCP port")?;
        let options = PgConnectOptions::new()
            .host(&host)
            .port(port)
            .database(&database)
            .username(&username)
            .password(&password)
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert(&ssl_root_cert);
        PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(DATABASE_ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .context("failed to connect using DB_HOST credentials")?
    };
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    let migrator = Migrator {
        migrations: Cow::Owned(vec![
            Migration::new(
                1,
                Cow::Borrowed("b0 schema"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0001_b0_schema.sql")),
                false,
            ),
            Migration::new(
                2,
                Cow::Borrowed("billing and web"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0002_billing_and_web.sql")),
                false,
            ),
            Migration::new(
                3,
                Cow::Borrowed("balance nonnegative"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0003_balance_nonnegative.sql")),
                false,
            ),
            Migration::new(
                4,
                Cow::Borrowed("estimate and select substrate"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!(
                    "../migrations/0004_estimate_and_select_substrate.sql"
                )),
                false,
            ),
            Migration::new(
                5,
                Cow::Borrowed("stripe checkout intents"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!(
                    "../migrations/0005_stripe_checkout_intents.sql"
                )),
                false,
            ),
        ]),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator
        .run(pool)
        .await
        .context("database migration failed")
}

pub async fn begin_usage_session(
    pool: &PgPool,
    key: &AuthenticatedKey,
    reserved_tokens: i64,
    reserved_output_tokens: i64,
    reserved_cost_usd: Decimal,
    task_signature: String,
    require_credits: bool,
) -> Result<UsageAdmission, sqlx::Error> {
    if reserved_tokens < 0 || reserved_output_tokens < 0 || reserved_cost_usd < Decimal::ZERO {
        return Err(sqlx::Error::Protocol(
            "usage reservation cannot be negative".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Serialize admission per USER (not per key) so concurrent requests
    // across a user's keys observe each other's reservations and cannot
    // jointly overdraw the prepaid balance.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(key.user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    // Both ceilings in one round trip. `spend_cap_usd` /
    // `velocity_cap_tokens_per_min` are the presenting key's own caps; the two
    // derived columns are the USER ceiling (see [`UsageAdmission`] on why a
    // user ceiling exists and why it is derived rather than configured, and on
    // why it is the MAX and not the sum). The presenting key must still be live for
    // admission to proceed, so it is always inside the sibling MAX: the
    // derived ceiling can never fall below the presenting key's own cap, and a
    // single-key user's admission is bit-for-bit the pre-change one.
    //
    // Matching `presenting.user_id` against the id the advisory lock was taken
    // on makes the lock's scope and the query's scope provably the same set of
    // rows; a stale [`AuthenticatedKey`] whose cached owner disagrees with the
    // row finds nothing and is rejected.
    let key_state = sqlx::query_as::<_, (bool, Decimal, i32, Decimal, i32)>(
        r#"
        SELECT
            presenting.disabled,
            presenting.spend_cap_usd,
            presenting.velocity_cap_tokens_per_min,
            COALESCE((
                SELECT MAX(sibling.spend_cap_usd)
                FROM api_keys AS sibling
                WHERE sibling.user_id = $2 AND NOT sibling.disabled
            ), presenting.spend_cap_usd) AS user_spend_cap_usd,
            COALESCE((
                SELECT MAX(sibling.velocity_cap_tokens_per_min)
                FROM api_keys AS sibling
                WHERE sibling.user_id = $2 AND NOT sibling.disabled
            ), presenting.velocity_cap_tokens_per_min) AS user_velocity_cap
        FROM api_keys AS presenting
        WHERE presenting.id = $1 AND presenting.user_id = $2
        "#,
    )
    .bind(key.id)
    .bind(key.user_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((
        disabled,
        spend_cap_usd,
        velocity_cap_tokens_per_min,
        user_spend_cap_usd,
        user_velocity_cap_tokens_per_min,
    )) = key_state
    else {
        transaction.rollback().await?;
        return Ok(UsageAdmission::Unauthorized);
    };
    if disabled {
        transaction.rollback().await?;
        return Ok(UsageAdmission::Unauthorized);
    }
    sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE id = $1")
        .bind(key.id)
        .execute(&mut *transaction)
        .await?;

    sqlx::query("DELETE FROM usage_reservations WHERE expires_at <= NOW()")
        .execute(&mut *transaction)
        .await?;

    // Settled usage, aggregated across EVERY key the user owns (disabled ones
    // included — a disabled key's history is still the user's spend) and, in
    // the same scan, restricted to the presenting key so the per-key ceiling
    // can be checked without a second round trip.
    //
    // Access path, measured on 500k `usage_events` with a 20-key user holding
    // 50k of them: nested loop over `api_keys_user_id_idx` (0001), then one
    // per-key range scan of `usage_events_key_timestamp_idx (api_key_id, ts
    // DESC)` (0001). No sequential scan, so the existing indexes serve this and
    // no new one is needed. The cost that DOES grow is row count — the scan
    // reads a user's whole month-to-date history (~14 ms for 30k rows here).
    // If that ever gets hot, the measured fix is widening the 0001 index to
    // INCLUDE (cost_usd, input_tokens, output_tokens), which turns it into an
    // index-only scan (0 heap fetches, ~5 ms for the same 30k rows).
    let (user_monthly_spend, user_recent_tokens, monthly_spend, recent_tokens) =
        sqlx::query_as::<_, (Decimal, i64, Decimal, i64)>(
            r#"
        SELECT
            COALESCE(
                SUM(usage_events.cost_usd) FILTER (
                    WHERE usage_events.ts
                        >= date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                ),
                0
            ) AS user_monthly_spend,
            COALESCE(
                SUM(usage_events.input_tokens::BIGINT + usage_events.output_tokens::BIGINT) FILTER (
                    WHERE usage_events.ts >= NOW() - INTERVAL '1 minute'
                ),
                0
            )::BIGINT AS user_recent_tokens,
            COALESCE(
                SUM(usage_events.cost_usd) FILTER (
                    WHERE usage_events.api_key_id = $2
                      AND usage_events.ts
                        >= date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                ),
                0
            ) AS monthly_spend,
            COALESCE(
                SUM(usage_events.input_tokens::BIGINT + usage_events.output_tokens::BIGINT) FILTER (
                    WHERE usage_events.api_key_id = $2
                      AND usage_events.ts >= NOW() - INTERVAL '1 minute'
                ),
                0
            )::BIGINT AS recent_tokens
        FROM usage_events
        INNER JOIN api_keys ON api_keys.id = usage_events.api_key_id
        WHERE api_keys.user_id = $1
          AND usage_events.ts >= LEAST(
              date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC',
              NOW() - INTERVAL '1 minute'
          )
        "#,
        )
        .bind(key.user_id)
        .bind(key.id)
        .fetch_one(&mut *transaction)
        .await?;
    // In-flight reservations, same two scopes. Expired rows were deleted above,
    // and the `expires_at > NOW()` predicate is kept so a row that expires
    // between the two statements is not counted.
    let (
        user_active_reserved_cost,
        user_active_reserved_tokens,
        active_reserved_cost,
        active_reserved_tokens,
    ) = sqlx::query_as::<_, (Decimal, i64, Decimal, i64)>(
        r#"
        SELECT
            COALESCE(SUM(usage_reservations.reserved_cost_usd), 0),
            COALESCE(SUM(usage_reservations.reserved_tokens), 0)::BIGINT,
            COALESCE(
                SUM(usage_reservations.reserved_cost_usd) FILTER (
                    WHERE usage_reservations.api_key_id = $2
                ),
                0
            ),
            COALESCE(
                SUM(usage_reservations.reserved_tokens) FILTER (
                    WHERE usage_reservations.api_key_id = $2
                ),
                0
            )::BIGINT
        FROM usage_reservations
        INNER JOIN api_keys ON api_keys.id = usage_reservations.api_key_id
        WHERE api_keys.user_id = $1
          AND usage_reservations.expires_at > NOW()
        "#,
    )
    .bind(key.user_id)
    .bind(key.id)
    .fetch_one(&mut *transaction)
    .await?;

    // Both ceilings are enforced and the tighter one wins: a request is
    // admitted only if it fits under the presenting key's own cap AND under the
    // user's derived cap. Aggregating across the user's keys is what makes
    // disable-and-remint (and device-grant fan-out) stop multiplying the
    // allowance — the churned keys' usage still counts.
    if !spend_within_cap(
        monthly_spend,
        active_reserved_cost,
        reserved_cost_usd,
        spend_cap_usd,
    ) || !spend_within_cap(
        user_monthly_spend,
        user_active_reserved_cost,
        reserved_cost_usd,
        user_spend_cap_usd,
    ) {
        transaction.rollback().await?;
        return Ok(UsageAdmission::SpendExceeded);
    }

    if !velocity_within_cap(
        recent_tokens,
        active_reserved_tokens,
        reserved_tokens,
        i64::from(velocity_cap_tokens_per_min),
    ) || !velocity_within_cap(
        user_recent_tokens,
        user_active_reserved_tokens,
        reserved_tokens,
        i64::from(user_velocity_cap_tokens_per_min),
    ) {
        transaction.rollback().await?;
        return Ok(UsageAdmission::VelocityExceeded);
    }

    if require_credits {
        let balance =
            sqlx::query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
                .bind(key.user_id)
                .fetch_optional(&mut *transaction)
                .await?;
        let Some(balance) = balance else {
            transaction.rollback().await?;
            return Ok(UsageAdmission::Unauthorized);
        };
        let active_user_reserved = sqlx::query_scalar::<_, Decimal>(
            r#"
            SELECT COALESCE(SUM(usage_reservations.reserved_cost_usd), 0)
            FROM usage_reservations
            INNER JOIN api_keys ON api_keys.id = usage_reservations.api_key_id
            WHERE api_keys.user_id = $1
              AND usage_reservations.expires_at > NOW()
            "#,
        )
        .bind(key.user_id)
        .fetch_one(&mut *transaction)
        .await?;
        if balance - active_user_reserved < reserved_cost_usd {
            transaction.rollback().await?;
            return Ok(UsageAdmission::InsufficientCredits);
        }
    }

    // Narrowed only after every admission gate has run, so this can never
    // displace a cap decision: reserved_tokens is >= reserved_output_tokens for
    // every caller, and the velocity cap is an INTEGER column, so any output
    // bound past the i32 range is rejected by the velocity gate above first.
    let reserved_output_tokens = i32::try_from(reserved_output_tokens).map_err(|_| {
        sqlx::Error::Protocol("reserved output tokens exceed the database integer range".to_owned())
    })?;
    let reservation_id = Uuid::new_v4();
    let reservation_ttl_seconds = i64::try_from(RESERVATION_TTL.as_secs()).map_err(|_| {
        sqlx::Error::Protocol("usage reservation lifetime exceeds the database range".to_owned())
    })?;
    sqlx::query(
        r#"
        INSERT INTO usage_reservations (
            id,
            api_key_id,
            expires_at,
            reserved_tokens,
            reserved_cost_usd
        )
        VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second'), $4, $5)
        "#,
    )
    .bind(reservation_id)
    .bind(key.id)
    .bind(reservation_ttl_seconds)
    .bind(reserved_tokens)
    .bind(reserved_cost_usd)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(UsageAdmission::Allowed(UsageSession {
        pool: pool.clone(),
        reservation_id,
        api_key_id: key.id,
        user_id: key.user_id,
        require_credits,
        task_signature,
        reserved_output_tokens,
        reserved_cost_usd,
    }))
}

impl UsageSession {
    #[must_use]
    pub fn request_id(&self) -> String {
        format!("chatcmpl-{}", self.reservation_id.simple())
    }

    pub async fn record(self, record: &UsageRecord) -> Result<(), sqlx::Error> {
        let input_tokens = checked_token_count(record.usage.prompt_tokens, "prompt_tokens")?;
        let cached_input_tokens =
            checked_token_count(record.usage.cached_input_tokens(), "cached_input_tokens")?;
        let output_tokens =
            checked_token_count(record.usage.completion_tokens, "completion_tokens")?;

        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *transaction)
            .await?;
        // Same USER-keyed lock as admission so settlement serializes against
        // concurrent admissions and balance reads for this user.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
            .bind(self.user_id.to_string())
            .execute(&mut *transaction)
            .await?;

        // Settle the reservation and recover what admission reserved in the
        // same statement; a missing row means it was already settled or expired.
        let reserved_cost_usd = sqlx::query_scalar::<_, Decimal>(
            "DELETE FROM usage_reservations WHERE id = $1 AND api_key_id = $2 RETURNING reserved_cost_usd",
        )
        .bind(self.reservation_id)
        .bind(self.api_key_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| {
            sqlx::Error::Protocol("usage reservation is missing or already settled".to_owned())
        })?;

        // Telemetry derived at settle from the candidate's cost-basis rates and
        // the buffered walk attempts (migration 0004). COGS is priced from the
        // same usage the customer is billed on, so an unmetered row settled at
        // the conservative estimate keeps a comparable basis instead of reading
        // as pure margin.
        let telemetry = &record.telemetry;
        let cost_basis_usd = telemetry
            .basis_rates
            .map(|rates| usage_cost(rates, record.usage));
        let attempts_cost_basis_usd = if record.attempts.is_empty() {
            None
        } else {
            Some(
                record
                    .attempts
                    .iter()
                    .filter(|attempt| !attempt.served)
                    .filter_map(|attempt| attempt.cost_basis_usd)
                    .sum::<Decimal>(),
            )
        };
        let finish_reason_source = telemetry.finish_reason.as_ref().map(|_| "synthetic");
        let attempt_count = if record.attempts.is_empty() {
            None
        } else {
            Some(i16::try_from(record.attempts.len()).unwrap_or(i16::MAX))
        };
        let sell_rates_json = rates_snapshot(&telemetry.sell_rates);
        let basis_rates_json = telemetry.basis_rates.as_ref().map(rates_snapshot);

        sqlx::query(
            r#"
            INSERT INTO usage_events (
                request_id,
                api_key_id,
                tier,
                upstream_provider,
                upstream_model,
                input_tokens,
                cached_input_tokens,
                output_tokens,
                cost_usd,
                latency_ms,
                status,
                candidate_id,
                cost_basis_usd,
                attempts_cost_basis_usd,
                sell_rates,
                basis_rates,
                finish_reason,
                finish_reason_source,
                requested_max_tokens,
                stream,
                prompt_bytes,
                message_count,
                tool_count,
                task_signature,
                attempt_count,
                shape_ok,
                reserved_output_tokens,
                reserved_cost_usd,
                estimator_basis
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                $12, $13, $14, $15::JSONB, $16::JSONB, $17, $18, $19, $20, $21,
                $22, $23, $24, $25, $26, $27, $28, $29
            )
            "#,
        )
        .bind(self.reservation_id)
        .bind(self.api_key_id)
        .bind(&record.tier)
        .bind(&record.upstream_provider)
        .bind(&record.upstream_model)
        .bind(input_tokens)
        .bind(cached_input_tokens.min(input_tokens))
        .bind(output_tokens)
        .bind(record.cost_usd)
        .bind(record.latency_ms)
        .bind(record.status)
        .bind(telemetry.candidate_id.as_deref())
        .bind(cost_basis_usd)
        .bind(attempts_cost_basis_usd)
        .bind(sell_rates_json)
        .bind(basis_rates_json)
        .bind(telemetry.finish_reason.as_deref())
        .bind(finish_reason_source)
        .bind(telemetry.requested_max_tokens)
        .bind(telemetry.stream)
        .bind(telemetry.prompt_bytes)
        .bind(telemetry.message_count)
        .bind(telemetry.tool_count)
        .bind(&self.task_signature)
        .bind(attempt_count)
        .bind(telemetry.shape_ok)
        .bind(self.reserved_output_tokens)
        .bind(self.reserved_cost_usd)
        .bind(ESTIMATOR_BASIS_COLD)
        .execute(&mut *transaction)
        .await?;

        // Attempts ride the settle transaction, inserted AFTER the event row so
        // the FK to the UNIQUE usage_events.request_id holds; exactly-once is
        // inherited from the reservation DELETE...RETURNING above.
        for attempt in &record.attempts {
            let (attempt_input, attempt_cached, attempt_output) = match attempt.usage {
                Some(usage) => {
                    let input = checked_token_count(usage.prompt_tokens, "attempt_input_tokens")?;
                    let cached = checked_token_count(
                        usage.cached_input_tokens(),
                        "attempt_cached_input_tokens",
                    )?
                    .min(input);
                    let output =
                        checked_token_count(usage.completion_tokens, "attempt_output_tokens")?;
                    (Some(input), Some(cached), Some(output))
                }
                None => (None, None, None),
            };
            sqlx::query(
                r#"
                INSERT INTO request_attempts (
                    request_id,
                    api_key_id,
                    user_id,
                    ts,
                    attempt_no,
                    candidate_id,
                    upstream_provider,
                    upstream_model,
                    outcome,
                    served,
                    latency_ms,
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    tokens_estimated,
                    cost_basis_usd,
                    finish_reason,
                    validator_kind
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
                "#,
            )
            .bind(self.reservation_id)
            .bind(self.api_key_id)
            .bind(self.user_id)
            .bind(attempt.started_at)
            .bind(attempt.attempt_no)
            .bind(&attempt.candidate_id)
            .bind(&attempt.upstream_provider)
            .bind(&attempt.upstream_model)
            .bind(&attempt.outcome)
            .bind(attempt.served)
            .bind(attempt.latency_ms)
            .bind(attempt_input)
            .bind(attempt_cached)
            .bind(attempt_output)
            .bind(attempt.tokens_estimated)
            .bind(attempt.cost_basis_usd)
            .bind(attempt.finish_reason.as_deref())
            .bind(attempt.validator_kind.as_deref())
            .execute(&mut *transaction)
            .await?;
        }

        // Debit the prepaid balance in the same transaction that settles the
        // reservation; the unique request_id and the settle-exactly-once
        // reservation make the debit idempotent. The debit is clamped to what
        // admission reserved (and thus verified against the balance), so actual
        // usage exceeding the reserved output bound can never overdraw. The
        // balance is only touched when credits gate admission — cap-only
        // deployments record usage_events for metering without moving money.
        // Zero-cost debits write no ledger row (the ledger forbids zero amounts).
        if self.require_credits {
            let debit = record.cost_usd.min(reserved_cost_usd);
            if debit > Decimal::ZERO {
                let balance_after = sqlx::query_scalar::<_, Decimal>(
                    r#"
                    UPDATE users
                    SET credit_balance_usd = credit_balance_usd - $2
                    WHERE id = $1
                    RETURNING credit_balance_usd
                    "#,
                )
                .bind(self.user_id)
                .bind(debit)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or_else(|| {
                    sqlx::Error::Protocol("usage settlement found no owning user".to_owned())
                })?;
                sqlx::query(
                    r#"
                    INSERT INTO credit_ledger (
                        user_id,
                        entry_type,
                        amount_usd,
                        balance_after_usd,
                        request_id
                    )
                    VALUES ($1, 'usage', $2, $3, $4)
                    "#,
                )
                .bind(self.user_id)
                .bind(-debit)
                .bind(balance_after)
                .bind(self.reservation_id)
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await
    }
}

/// Whether one spend ceiling still admits `additional` on top of what is
/// already settled and reserved against it.
///
/// Two conditions, both inherited verbatim from B0's single-scope check: a
/// ceiling already reached exactly admits nothing (so a zero-cost reservation
/// cannot slip through an exhausted cap), and the projection must land at or
/// below the ceiling.
fn spend_within_cap(
    settled: Decimal,
    reserved: Decimal,
    additional: Decimal,
    cap: Decimal,
) -> bool {
    let committed = settled + reserved;
    committed < cap && committed + additional <= cap
}

/// Velocity counterpart of [`spend_within_cap`]. Saturating on the committed
/// side and checked on the projection, so an overflowing projection is refused
/// rather than wrapping into an admission.
fn velocity_within_cap(settled: i64, reserved: i64, additional: i64, cap: i64) -> bool {
    settled.saturating_add(reserved) < cap
        && settled
            .checked_add(reserved)
            .and_then(|committed| committed.checked_add(additional))
            .is_some_and(|projected| projected <= cap)
}

/// The most live (non-disabled) keys one user may hold at once.
pub const MAX_ACTIVE_KEYS_PER_USER: i64 = 20;
/// The most keys one user may CREATE inside [`KEY_CREATION_WINDOW_HOURS`],
/// counting keys they have since disabled.
pub const MAX_KEYS_CREATED_PER_WINDOW: i64 = 20;
/// Trailing window the creation throttle counts over.
pub const KEY_CREATION_WINDOW_HOURS: i64 = 24;

/// Whether a user may mint another API key.
pub enum KeyMintAdmission {
    Allowed,
    LimitReached,
}

/// Decide whether `user_id` may mint another API key, inside the caller's
/// transaction.
///
/// Two limits, both required:
///
/// 1. **Active-key cap** — at most [`MAX_ACTIVE_KEYS_PER_USER`] non-disabled
///    keys. This is the original limit and it counts live keys only.
/// 2. **Creation throttle** — at most [`MAX_KEYS_CREATED_PER_WINDOW`] keys
///    *created* in the trailing [`KEY_CREATION_WINDOW_HOURS`], counting
///    disabled ones. Limit 1 alone is resettable: disabling a key is a flag
///    flip, so a user could exhaust a key's quota, disable it, mint a fresh one
///    and repeat without bound. Counting creations over a window makes the
///    churn itself the scarce resource.
///
/// The caller must already hold a serializing lock on the owning user (both
/// mint paths take `SELECT ... FROM users WHERE id = $1 FOR UPDATE`), otherwise
/// two concurrent mints can each observe a count below the limit.
///
/// Scope note: this governs the two SELF-SERVICE mint paths only —
/// `portal::create_key` and the device-claim mint in [`crate::device`]. The
/// `admin mint-key` CLI is operator-only (it needs database credentials, not a
/// session) and is deliberately left exempt, so an operator can always issue a
/// key for a user who has hit the throttle.
pub async fn admit_key_mint(
    connection: &mut PgConnection,
    user_id: Uuid,
) -> Result<KeyMintAdmission, sqlx::Error> {
    let (active_keys, recently_created_keys) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE NOT disabled) AS active_keys,
            COUNT(*) FILTER (
                WHERE created_at > NOW() - ($2 * INTERVAL '1 hour')
            ) AS recently_created_keys
        FROM api_keys
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .bind(KEY_CREATION_WINDOW_HOURS)
    .fetch_one(connection)
    .await?;
    if active_keys >= MAX_ACTIVE_KEYS_PER_USER
        || recently_created_keys >= MAX_KEYS_CREATED_PER_WINDOW
    {
        return Ok(KeyMintAdmission::LimitReached);
    }
    Ok(KeyMintAdmission::Allowed)
}

fn checked_token_count(value: u64, field: &'static str) -> Result<i32, sqlx::Error> {
    i32::try_from(value)
        .map_err(|_| sqlx::Error::Protocol(format!("{field} exceeds the database integer range")))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).unwrap_or_default();
    if value.trim().is_empty() {
        bail!("{name} is required")
    }
    Ok(value)
}

pub fn parse_decimal(value: &str, field: &str) -> Result<Decimal> {
    Decimal::from_str(value).with_context(|| format!("{field} must be a decimal number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_fail_instead_of_saturating() {
        assert_eq!(
            checked_token_count(i32::MAX as u64, "tokens").expect("i32 max should fit"),
            i32::MAX
        );
        assert!(checked_token_count(i32::MAX as u64 + 1, "tokens").is_err());
    }

    #[test]
    fn spend_cap_refuses_at_the_ceiling_and_past_the_projection() {
        let cap = Decimal::from(20);
        assert!(spend_within_cap(
            Decimal::from(5),
            Decimal::from(5),
            Decimal::from(10),
            cap
        ));
        // The projection may land exactly on the ceiling.
        assert!(spend_within_cap(
            Decimal::from(19),
            Decimal::ZERO,
            Decimal::ONE,
            cap
        ));
        // ...but one cent past it is refused.
        assert!(!spend_within_cap(
            Decimal::from(19),
            Decimal::ZERO,
            Decimal::from(2),
            cap
        ));
        // A ceiling already reached admits nothing, not even a zero-cost
        // reservation: that is what stops a free request from being smuggled
        // past an exhausted cap.
        assert!(!spend_within_cap(
            Decimal::from(20),
            Decimal::ZERO,
            Decimal::ZERO,
            cap
        ));
        assert!(!spend_within_cap(
            Decimal::from(15),
            Decimal::from(5),
            Decimal::ZERO,
            cap
        ));
    }

    #[test]
    fn velocity_cap_refuses_at_the_ceiling_and_on_overflow() {
        assert!(velocity_within_cap(400, 400, 200, 1_000));
        assert!(!velocity_within_cap(400, 400, 201, 1_000));
        assert!(!velocity_within_cap(1_000, 0, 0, 1_000));
        assert!(!velocity_within_cap(600, 400, 0, 1_000));
        // An overflowing projection must fail closed rather than wrap.
        assert!(!velocity_within_cap(1, i64::MAX, 1, i64::MAX));
        assert!(!velocity_within_cap(0, 0, i64::MAX, 10));
    }

    #[test]
    fn key_creation_throttle_is_not_looser_than_the_active_cap() {
        // The throttle must be able to bite before the active-key cap does,
        // otherwise disable-and-remint stays free: a user who disables every
        // key they mint never accumulates active keys.
        const { assert!(MAX_KEYS_CREATED_PER_WINDOW <= MAX_ACTIVE_KEYS_PER_USER) };
        const { assert!(KEY_CREATION_WINDOW_HOURS > 0) };
    }
}

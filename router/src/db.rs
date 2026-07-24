use std::{borrow::Cow, env, str::FromStr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::json;
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

    let key_state = sqlx::query_as::<_, (bool, Decimal, i32)>(
        r#"
        SELECT disabled, spend_cap_usd, velocity_cap_tokens_per_min
        FROM api_keys
        WHERE id = $1
        "#,
    )
    .bind(key.id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((disabled, spend_cap_usd, velocity_cap_tokens_per_min)) = key_state else {
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

    let (monthly_spend, recent_tokens) = sqlx::query_as::<_, (Decimal, i64)>(
        r#"
        SELECT
            COALESCE(
                SUM(cost_usd) FILTER (
                    WHERE ts >= date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                ),
                0
            ) AS monthly_spend,
            COALESCE(
                SUM(input_tokens::BIGINT + output_tokens::BIGINT) FILTER (
                    WHERE ts >= NOW() - INTERVAL '1 minute'
                ),
                0
            )::BIGINT AS recent_tokens
        FROM usage_events
        WHERE api_key_id = $1
          AND ts >= LEAST(
              date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC',
              NOW() - INTERVAL '1 minute'
          )
        "#,
    )
    .bind(key.id)
    .fetch_one(&mut *transaction)
    .await?;
    let (active_reserved_cost, active_reserved_tokens) = sqlx::query_as::<_, (Decimal, i64)>(
        r#"
        SELECT
            COALESCE(SUM(reserved_cost_usd), 0),
            COALESCE(SUM(reserved_tokens), 0)::BIGINT
        FROM usage_reservations
        WHERE api_key_id = $1
          AND expires_at > NOW()
        "#,
    )
    .bind(key.id)
    .fetch_one(&mut *transaction)
    .await?;

    let projected_spend = monthly_spend + active_reserved_cost + reserved_cost_usd;
    if monthly_spend + active_reserved_cost >= spend_cap_usd || projected_spend > spend_cap_usd {
        transaction.rollback().await?;
        return Ok(UsageAdmission::SpendExceeded);
    }

    let velocity_cap = i64::from(velocity_cap_tokens_per_min);
    let projected_tokens = recent_tokens
        .checked_add(active_reserved_tokens)
        .and_then(|tokens| tokens.checked_add(reserved_tokens));
    if recent_tokens.saturating_add(active_reserved_tokens) >= velocity_cap
        || projected_tokens.is_none_or(|tokens| tokens > velocity_cap)
    {
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
}

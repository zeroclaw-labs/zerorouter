use std::{borrow::Cow, env, str::FromStr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::{
    auth::AuthenticatedKey,
    openai::OpenAiUsage,
    sqlx::{
        self, PgPool,
        migrate::{Migration, MigrationType, Migrator},
        postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    },
};

#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub tier: String,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub usage: OpenAiUsage,
    pub cost_usd: Decimal,
    pub latency_ms: i32,
    pub status: i16,
}

pub enum UsageAdmission {
    Allowed(UsageSession),
    Unauthorized,
    SpendExceeded,
    VelocityExceeded,
}

pub struct UsageSession {
    pool: PgPool,
    reservation_id: Uuid,
    api_key_id: Uuid,
}

const DATABASE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const RESERVATION_TTL: Duration = Duration::from_secs(20 * 60);

pub async fn database_pool_from_env() -> Result<PgPool> {
    let pool = if let Ok(database_url) = env::var("DATABASE_URL") {
        let options = PgConnectOptions::from_str(&database_url)
            .map_err(|_| anyhow!("DATABASE_URL is invalid"))?;
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
        migrations: Cow::Owned(vec![Migration::new(
            1,
            Cow::Borrowed("b0 schema"),
            MigrationType::Simple,
            Cow::Borrowed(include_str!("../migrations/0001_b0_schema.sql")),
            false,
        )]),
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
    reserved_cost_usd: Decimal,
) -> Result<UsageAdmission, sqlx::Error> {
    if reserved_tokens < 0 || reserved_cost_usd < Decimal::ZERO {
        return Err(sqlx::Error::Protocol(
            "usage reservation cannot be negative".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(key.id.to_string())
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
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
            .bind(self.api_key_id.to_string())
            .execute(&mut *transaction)
            .await?;

        let reservation_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM usage_reservations WHERE id = $1 AND api_key_id = $2)",
        )
        .bind(self.reservation_id)
        .bind(self.api_key_id)
        .fetch_one(&mut *transaction)
        .await?;
        if !reservation_exists {
            return Err(sqlx::Error::Protocol(
                "usage reservation is missing or already settled".to_owned(),
            ));
        }

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
                status
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
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
        .execute(&mut *transaction)
        .await?;
        let deleted = sqlx::query("DELETE FROM usage_reservations WHERE id = $1")
            .bind(self.reservation_id)
            .execute(&mut *transaction)
            .await?;
        if deleted.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "usage reservation could not be settled".to_owned(),
            ));
        }
        transaction.commit().await
    }
}

fn checked_token_count(value: u64, field: &'static str) -> Result<i32, sqlx::Error> {
    i32::try_from(value)
        .map_err(|_| sqlx::Error::Protocol(format!("{field} exceeds the database integer range")))
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

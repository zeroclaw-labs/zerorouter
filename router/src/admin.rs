use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use rust_decimal::Decimal;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth::{generate_api_key, hash_api_key},
    db::{
        database_pool_from_env, migrate, parse_decimal, quarantined_settlements,
        recover_owed_settlements,
    },
    sqlx::{self, PgPool},
};

#[derive(Debug, Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Mint a key and print its plaintext value exactly once.
    MintKey(MintKeyArgs),
    /// Disable an existing key.
    RevokeKey(RevokeKeyArgs),
    /// List key metadata without hashes or plaintext credentials.
    ListKeys(ListKeysArgs),
    /// List settlements that could not be recorded and are awaiting
    /// reconciliation (migration 0006).
    OwedSettlements(OwedSettlementsArgs),
    /// Replay settlements that were recorded as owed and never committed. Safe
    /// to run at any time and safe to run twice: the settle is idempotent.
    SettleOwed(SettleOwedArgs),
}

#[derive(Debug, Args)]
pub struct MintKeyArgs {
    #[arg(long)]
    pub email: String,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub spend_cap_usd: Option<String>,
    #[arg(long)]
    pub velocity_cap_tokens_per_min: Option<i32>,
}

#[derive(Debug, Args)]
pub struct RevokeKeyArgs {
    #[arg(long)]
    pub key_id: Uuid,
}

#[derive(Debug, Args)]
pub struct ListKeysArgs {
    #[arg(long)]
    pub email: Option<String>,
}

#[derive(Debug, Args)]
pub struct OwedSettlementsArgs {
    #[arg(long, default_value_t = 100)]
    pub limit: i64,
}

#[derive(Debug, Args)]
pub struct SettleOwedArgs {
    #[arg(long, default_value_t = 100)]
    pub limit: i64,
}

#[derive(Serialize)]
struct MintedKey {
    key_id: Uuid,
    api_key: String,
}

#[derive(Debug, Serialize)]
struct KeyMetadata {
    id: Uuid,
    email: String,
    name: String,
    spend_cap_usd: Decimal,
    velocity_cap_tokens_per_min: i32,
    disabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn run(args: AdminArgs) -> Result<()> {
    let pool = database_pool_from_env().await?;
    migrate(&pool).await?;

    match args.command {
        AdminCommand::MintKey(args) => mint_key(&pool, args).await,
        AdminCommand::RevokeKey(args) => revoke_key(&pool, args).await,
        AdminCommand::ListKeys(args) => list_keys(&pool, args).await,
        AdminCommand::OwedSettlements(args) => owed_settlements(&pool, args).await,
        AdminCommand::SettleOwed(args) => settle_owed(&pool, args).await,
    }
}

/// The reconciliation queue: delivered inference whose settlement could not be
/// recorded automatically. An empty list is the healthy state.
async fn owed_settlements(pool: &PgPool, args: OwedSettlementsArgs) -> Result<()> {
    let rows = quarantined_settlements(pool, args.limit)
        .await
        .context("failed to list quarantined settlements")?;
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

async fn settle_owed(pool: &PgPool, args: SettleOwedArgs) -> Result<()> {
    let summary = recover_owed_settlements(pool, args.limit)
        .await
        .context("failed to recover owed settlements")?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn mint_key(pool: &PgPool, args: MintKeyArgs) -> Result<()> {
    let email = args.email.trim().to_lowercase();
    let name = args.name.trim();
    if email.is_empty() || !email.contains('@') {
        bail!("email must be a non-empty email address")
    }
    if name.is_empty() {
        bail!("name cannot be empty")
    }
    let spend_cap = args
        .spend_cap_usd
        .as_deref()
        .map(|value| parse_decimal(value, "spend-cap-usd"))
        .transpose()?;
    if spend_cap.is_some_and(|cap| cap < Decimal::ZERO) {
        bail!("spend-cap-usd cannot be negative")
    }
    if args.velocity_cap_tokens_per_min.is_some_and(|cap| cap <= 0) {
        bail!("velocity-cap-tokens-per-min must be positive")
    }

    let mut transaction = pool.begin().await.context("failed to begin transaction")?;
    let user_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO users (id, email)
        VALUES ($1, $2)
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(user_id)
    .bind(&email)
    .execute(&mut *transaction)
    .await
    .context("failed to create user")?;
    let user_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_one(&mut *transaction)
        .await
        .context("failed to resolve user")?;

    let api_key = generate_api_key();
    let key_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO api_keys (
            id,
            user_id,
            key_hash,
            name
        )
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&api_key))
    .bind(name)
    .execute(&mut *transaction)
    .await
    .context("failed to store API key digest")?;
    if spend_cap.is_some() || args.velocity_cap_tokens_per_min.is_some() {
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
        .bind(spend_cap)
        .bind(args.velocity_cap_tokens_per_min)
        .execute(&mut *transaction)
        .await
        .context("failed to apply API key cap overrides")?;
    }
    transaction
        .commit()
        .await
        .context("failed to commit key mint")?;

    println!(
        "{}",
        serde_json::to_string_pretty(&MintedKey { key_id, api_key })?
    );
    Ok(())
}

async fn revoke_key(pool: &PgPool, args: RevokeKeyArgs) -> Result<()> {
    let result = sqlx::query("UPDATE api_keys SET disabled = TRUE WHERE id = $1 AND NOT disabled")
        .bind(args.key_id)
        .execute(pool)
        .await
        .context("failed to revoke API key")?;
    if result.rows_affected() != 1 {
        bail!("API key was not found or was already disabled")
    }
    println!("revoked {}", args.key_id);
    Ok(())
}

async fn list_keys(pool: &PgPool, args: ListKeysArgs) -> Result<()> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Decimal,
            i32,
            bool,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"
        SELECT
            api_keys.id,
            users.email,
            api_keys.name,
            api_keys.spend_cap_usd,
            api_keys.velocity_cap_tokens_per_min,
            api_keys.disabled,
            api_keys.created_at,
            api_keys.last_used_at
        FROM api_keys
        INNER JOIN users ON users.id = api_keys.user_id
        WHERE ($1::TEXT IS NULL OR users.email = $1)
        ORDER BY api_keys.created_at DESC
        "#,
    )
    .bind(args.email.map(|email| email.trim().to_lowercase()))
    .fetch_all(pool)
    .await
    .context("failed to list API keys")?
    .into_iter()
    .map(
        |(
            id,
            email,
            name,
            spend_cap_usd,
            velocity_cap_tokens_per_min,
            disabled,
            created_at,
            last_used_at,
        )| KeyMetadata {
            id,
            email,
            name,
            spend_cap_usd,
            velocity_cap_tokens_per_min,
            disabled,
            created_at,
            last_used_at,
        },
    )
    .collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

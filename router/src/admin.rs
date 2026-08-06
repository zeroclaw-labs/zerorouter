use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use rust_decimal::Decimal;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth::{generate_api_key, hash_api_key},
    db::{
        database_pool_from_env, migrate, parse_decimal, provider_cogs, quarantined_settlements,
        recover_owed_settlements, recover_quarantined_settlement,
    },
    priority::Priority,
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
    /// Per-provider trailing-window COGS: what each upstream account is
    /// owed, for invoice reconciliation and deposit sizing.
    Treasury(TreasuryArgs),
    /// What an operator needs to answer "did this customer's money land?"
    /// — balance, recent ledger, key count — without shell access to the
    /// database. Read-only.
    UserStatus(UserStatusArgs),
    /// Grant promo credits to an existing user, atomically: the same
    /// transactional path Stripe purchases use, with entry type 'promo'.
    /// The user must already exist (mint-key creates one) — a typo'd email
    /// must fail loudly, never mint a funded ghost account.
    GrantCredit(GrantCreditArgs),
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
    /// Per-key default for the priority knob (cost | balanced | success);
    /// omitted means NULL, which reads as balanced.
    #[arg(long, value_parser = parse_priority)]
    pub default_priority: Option<Priority>,
}

#[derive(Debug, Args)]
pub struct UserStatusArgs {
    #[arg(long)]
    pub email: String,
    /// Ledger entries to show, newest first.
    #[arg(long, default_value_t = 10)]
    pub entries: i64,
}

#[derive(Debug, Args)]
pub struct GrantCreditArgs {
    #[arg(long)]
    pub email: String,
    /// Credit amount in USD, e.g. 5 or 5.00. Positive, at most 10000.
    #[arg(long)]
    pub amount_usd: String,
    /// Ledger note recorded with the grant.
    #[arg(long, default_value = "beta promo credit")]
    pub note: String,
}

fn parse_priority(value: &str) -> Result<Priority, String> {
    Priority::from_keyword(value)
        .ok_or_else(|| format!("unknown priority '{value}' (expected cost, balanced, or success)"))
}

#[derive(Debug, Args)]
pub struct TreasuryArgs {
    /// Trailing window in days.
    #[arg(long, default_value_t = 7)]
    pub days: i32,
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
    /// Collect ONE settlement by request id, even if it is quarantined.
    /// Quarantine ends automatic retry; this is the operator's path from
    /// "parked" to "collected", single-row on purpose — you read
    /// `owed-settlements`, you decide about that debt, you collect it.
    #[arg(long)]
    pub request_id: Option<Uuid>,
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
    default_priority: Option<String>,
    disabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn run(args: AdminArgs) -> Result<()> {
    let pool = database_pool_from_env().await?;
    migrate(&pool).await?;

    match args.command {
        AdminCommand::MintKey(args) => mint_key(&pool, args).await,
        AdminCommand::UserStatus(args) => user_status(&pool, args).await,
        AdminCommand::GrantCredit(args) => grant_credit(&pool, args).await,
        AdminCommand::Treasury(args) => treasury(&pool, args).await,
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
    let summary = if let Some(request_id) = args.request_id {
        recover_quarantined_settlement(pool, request_id)
            .await
            .context("failed to collect the quarantined settlement")?
    } else {
        recover_owed_settlements(pool, args.limit)
            .await
            .context("failed to recover owed settlements")?
    };
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn treasury(pool: &PgPool, args: TreasuryArgs) -> Result<()> {
    if args.days <= 0 {
        bail!("days must be positive")
    }
    let rows = provider_cogs(pool, args.days)
        .await
        .context("failed to aggregate provider COGS")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "window_days": args.days,
            "providers": rows,
        }))?
    );
    Ok(())
}

async fn user_status(pool: &PgPool, args: UserStatusArgs) -> Result<()> {
    let email = args.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        bail!("email must be a non-empty email address")
    }
    let Some((user_id, balance)) = sqlx::query_as::<_, (Uuid, Decimal)>(
        "SELECT id, credit_balance_usd FROM users WHERE email = $1",
    )
    .bind(&email)
    .fetch_optional(pool)
    .await
    .context("failed to resolve user")?
    else {
        bail!("no user with email {email}")
    };
    let entries = crate::billing::ledger_entries(pool, user_id, args.entries.max(1))
        .await
        .context("failed to read the ledger")?;
    let (live_keys, spent) = sqlx::query_as::<_, (i64, Decimal)>(
        r#"
        SELECT
            (SELECT COUNT(*) FROM api_keys WHERE user_id = $1 AND NOT disabled),
            COALESCE((
                SELECT SUM(usage_events.cost_usd)
                FROM usage_events
                JOIN api_keys ON api_keys.id = usage_events.api_key_id
                WHERE api_keys.user_id = $1
            ), 0)
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .context("failed to summarize usage")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "user_id": user_id,
            "email": email,
            "balance_usd": balance,
            "lifetime_spend_usd": spent,
            "live_keys": live_keys,
            "ledger": entries,
        }))?
    );
    Ok(())
}

async fn grant_credit(pool: &PgPool, args: GrantCreditArgs) -> Result<()> {
    let email = args.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        bail!("email must be a non-empty email address")
    }
    let amount = parse_decimal(&args.amount_usd, "amount-usd")?;
    if amount <= Decimal::ZERO {
        bail!("amount-usd must be positive")
    }
    // Fat-finger guard, not a policy knob: a beta promo grant should never
    // be five digits. Run twice for more.
    if amount > Decimal::from(10_000) {
        bail!("amount-usd must be at most 10000")
    }
    let Some(user_id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(pool)
        .await
        .context("failed to resolve user")?
    else {
        bail!("no user with email {email}; mint-key creates one")
    };
    crate::billing::grant_promo(pool, user_id, amount, args.note.trim())
        .await
        .context("failed to grant credit")?;
    let balance = crate::billing::balance(pool, user_id)
        .await
        .context("failed to read balance")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "user_id": user_id,
            "granted_usd": amount,
            "balance_usd": balance,
        }))?
    );
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
    if spend_cap.is_some()
        || args.velocity_cap_tokens_per_min.is_some()
        || args.default_priority.is_some()
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
        .bind(spend_cap)
        .bind(args.velocity_cap_tokens_per_min)
        .bind(args.default_priority.map(Priority::as_str))
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
            Option<String>,
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
            api_keys.default_priority,
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
            default_priority,
            disabled,
            created_at,
            last_used_at,
        )| KeyMetadata {
            id,
            email,
            name,
            spend_cap_usd,
            velocity_cap_tokens_per_min,
            default_priority,
            disabled,
            created_at,
            last_used_at,
        },
    )
    .collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

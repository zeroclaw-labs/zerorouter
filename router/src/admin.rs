use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use rust_decimal::Decimal;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth::{generate_api_key, hash_api_key},
    billing::ResolutionOutcome,
    db::{
        ReservationRelease, database_pool_from_env, migrate, parse_decimal, provider_cogs,
        quarantined_settlements, recover_owed_settlements, recover_quarantined_settlement,
        release_quarantined_reservation,
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
    /// Freeze or unfreeze an account. A frozen account is refused at
    /// admission and cannot mint new API keys; its history stays readable.
    /// `--off` is the release valve for the automatic freeze a Stripe
    /// chargeback applies (migration 0009) — without it, lifting a freeze
    /// would mean hand-written SQL against the users table.
    SetFrozen(SetFrozenArgs),
    /// The dispute review workflow: the queue of accounts a chargeback or a
    /// freeze left needing a decision, one account's full history, the
    /// settlement of a receivable, and the release of a quarantined
    /// reservation. `list` and `show` are read-only; `resolve` and
    /// `release-reservation` are the writers, and neither changes the freeze
    /// state.
    #[command(subcommand)]
    Disputes(DisputesCommand),
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
    /// Reconcile `tiers.toml` against a public model catalog: prices, context
    /// windows, modalities. Read-only and database-free, so it runs in CI.
    /// Exits non-zero when a basis drifted or a model vanished — never writes
    /// a price, because a bad fetch that repriced a live billing catalog would
    /// be worse than the staleness it fixed.
    CatalogDrift(CatalogDriftArgs),
}

#[derive(Debug, Args)]
pub struct CatalogDriftArgs {
    /// Public catalog to reconcile against.
    #[arg(long, default_value = crate::drift::DEFAULT_SOURCE_URL)]
    pub source_url: String,
    /// Read the source from a file instead of the network (offline / CI).
    #[arg(long)]
    pub source_file: Option<std::path::PathBuf>,
    /// Tier file to check. Defaults to the same path the server serves.
    #[arg(long)]
    pub tiers: Option<std::path::PathBuf>,
    /// Operator provider inventory, if the tier file names upstreams the
    /// shipped inventory does not (edge mode). Defaults to
    /// `ZEROROUTER_PROVIDERS_PATH`, the same variable the server reads.
    #[arg(long)]
    pub providers: Option<std::path::PathBuf>,
    /// Report and exit zero even when something drifted.
    #[arg(long)]
    pub allow_drift: bool,
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

#[derive(Debug, Args)]
pub struct SetFrozenArgs {
    #[arg(long)]
    pub email: String,
    /// Freeze the account: an operator-initiated hold.
    #[arg(long, conflicts_with = "off")]
    pub on: bool,
    /// Lift the freeze and restore service, whatever applied it.
    #[arg(long)]
    pub off: bool,
}

#[derive(Debug, Subcommand)]
pub enum DisputesCommand {
    /// The review queue: every account that is frozen, owes money, or was
    /// reversed inside the trailing window. Read-only.
    List(DisputesListArgs),
    /// One account in full — freeze state, balance, every ledger entry with
    /// its Stripe anchors, a usage summary, and live key metadata. Read-only,
    /// and never prints a key hash or a plaintext key.
    Show(DisputesShowArgs),
    /// Settle a receivable: forgive it (`--write-off`) or record money
    /// recovered outside Stripe (`--recover`). Does NOT unfreeze — "the money
    /// is settled" and "we trust this account again" are separate decisions,
    /// and the second one is `set-frozen --off`.
    Resolve(DisputesResolveArgs),
    /// Resolve ONE quarantined reservation from `owed-settlements`, so it
    /// leaves that queue with its reason recorded. This is the exit for a
    /// dispatched row that holds no settlement intent — inference the customer
    /// received whose charge was lost, in an amount nobody can reconstruct, and
    /// which `settle-owed` therefore cannot collect. A row that CAN be
    /// collected is refused here unless `--forgive` says otherwise. Debits
    /// nobody, ever.
    ReleaseReservation(DisputesReleaseReservationArgs),
}

#[derive(Debug, Args)]
pub struct DisputesListArgs {
    /// Trailing window, in days, for the "was reversed recently" trigger. A
    /// freeze and a negative balance are durable and never age out of this
    /// list regardless of the window.
    #[arg(long, default_value_t = 30)]
    pub days: i32,
}

#[derive(Debug, Args)]
pub struct DisputesShowArgs {
    #[arg(long)]
    pub email: String,
}

#[derive(Debug, Args)]
pub struct DisputesResolveArgs {
    #[arg(long)]
    pub email: String,
    /// Forgive the whole receivable: bring a negative balance up to exactly
    /// zero. The amount is read from the account under the money lock, never
    /// passed in, so it cannot overshoot into credit.
    #[arg(long, conflicts_with = "recover")]
    pub write_off: bool,
    /// Record money recovered outside Stripe (a dispute won, a wire received)
    /// and credit the balance by that amount, in USD.
    #[arg(long)]
    pub recover: Option<String>,
    /// Why. Required rather than defaulted: a resolution points at no Stripe
    /// object and no request, so this note is the entire record of why an
    /// operator moved the money.
    #[arg(long)]
    pub note: String,
}

#[derive(Debug, Args)]
pub struct DisputesReleaseReservationArgs {
    /// The reservation to resolve, as printed by `admin owed-settlements`.
    #[arg(long)]
    pub request_id: Uuid,
    /// Give up a collectable charge. Required for a reservation that still
    /// holds a settlement intent, and only for that case: such a debt CAN be
    /// collected (`settle-owed --request-id`), so releasing it is ZeroRouter
    /// deciding not to. It must be typed out, never be the easy default.
    #[arg(long)]
    pub forgive: bool,
    /// Why. Required rather than defaulted, exactly as `resolve --note` is: a
    /// release anchors to no Stripe object and writes no ledger row, so this
    /// sentence is the whole record of why a charge stopped being pursued.
    #[arg(long)]
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
    // Catalog drift is a property of a FILE, not of the ledger. Answering it
    // before a pool is opened is what lets CI run it with no database.
    if let AdminCommand::CatalogDrift(args) = args.command {
        return catalog_drift(args).await;
    }

    let pool = database_pool_from_env().await?;
    migrate(&pool).await?;

    match args.command {
        AdminCommand::MintKey(args) => mint_key(&pool, args).await,
        AdminCommand::UserStatus(args) => user_status(&pool, args).await,
        AdminCommand::GrantCredit(args) => grant_credit(&pool, args).await,
        AdminCommand::SetFrozen(args) => set_frozen(&pool, args).await,
        AdminCommand::Disputes(command) => match command {
            DisputesCommand::List(args) => disputes_list(&pool, args).await,
            DisputesCommand::Show(args) => disputes_show(&pool, args).await,
            DisputesCommand::Resolve(args) => disputes_resolve(&pool, args).await,
            DisputesCommand::ReleaseReservation(args) => {
                disputes_release_reservation(&pool, args).await
            }
        },
        AdminCommand::Treasury(args) => treasury(&pool, args).await,
        AdminCommand::RevokeKey(args) => revoke_key(&pool, args).await,
        AdminCommand::ListKeys(args) => list_keys(&pool, args).await,
        AdminCommand::OwedSettlements(args) => owed_settlements(&pool, args).await,
        AdminCommand::SettleOwed(args) => settle_owed(&pool, args).await,
        // Handled above, before the pool exists.
        AdminCommand::CatalogDrift(_) => unreachable!("dispatched before the pool"),
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
    // `frozen_at` rides along because this is the command an operator runs to
    // answer "why is this customer being refused?", and a freeze is now one of
    // the answers. Listing every frozen account is the review workflow's job,
    // not this one's.
    let Some((user_id, balance, frozen_at, frozen_reason)) = sqlx::query_as::<
        _,
        (
            Uuid,
            Decimal,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
        ),
    >(
        "SELECT id, credit_balance_usd, frozen_at, frozen_reason FROM users WHERE email = $1",
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
            "frozen": frozen_at.is_some(),
            "frozen_at": frozen_at,
            "frozen_reason": frozen_reason,
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

/// Freeze or lift the freeze on one account, by email.
///
/// Shaped like `grant-credit` on purpose — resolve the user, refuse loudly if
/// there is not exactly one, act through the same helpers the webhook uses,
/// print the resulting state as JSON. Exactly one of `--on` / `--off` is
/// required: a "set" command whose default is silently one direction is how an
/// operator unfreezes an account they meant to freeze.
async fn set_frozen(pool: &PgPool, args: SetFrozenArgs) -> Result<()> {
    let email = args.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        bail!("email must be a non-empty email address")
    }
    let freeze = match (args.on, args.off) {
        (true, false) => true,
        (false, true) => false,
        _ => bail!("pass exactly one of --on or --off"),
    };
    let Some(user_id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(pool)
        .await
        .context("failed to resolve user")?
    else {
        bail!("no user with email {email}")
    };
    let changed = if freeze {
        crate::billing::freeze_account(pool, user_id, crate::billing::FreezeReason::Operator)
            .await
            .context("failed to freeze the account")?
    } else {
        crate::billing::unfreeze_account(pool, user_id)
            .await
            .context("failed to lift the freeze")?
    };
    let state = crate::billing::freeze_state(pool, user_id)
        .await
        .context("failed to read the freeze state")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "user_id": user_id,
            "email": email,
            "frozen": state.is_some(),
            "frozen_at": state.as_ref().map(|(at, _)| at),
            "frozen_reason": state.as_ref().map(|(_, reason)| reason),
            // False means the account was already in the requested state; the
            // command is idempotent and that is not an error.
            "changed": changed,
        }))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispute review (migration 0013)
// ---------------------------------------------------------------------------

/// Normalize an operator-supplied email and resolve it to exactly one user.
///
/// Refuses loudly on an unknown address rather than returning an empty result:
/// every caller here is about to act on, or report on, real money, and a typo
/// that quietly produced "nothing to see" is how an operator concludes a
/// receivable was already settled.
async fn resolve_user(pool: &PgPool, email: &str) -> Result<(Uuid, String)> {
    let email = email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        bail!("email must be a non-empty email address")
    }
    let Some(user_id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(pool)
        .await
        .context("failed to resolve user")?
    else {
        bail!("no user with email {email}")
    };
    Ok((user_id, email))
}

/// The review queue. An empty list is the healthy state.
async fn disputes_list(pool: &PgPool, args: DisputesListArgs) -> Result<()> {
    if args.days <= 0 {
        bail!("days must be positive")
    }
    let rows = crate::billing::review_queue(pool, args.days)
        .await
        .context("failed to build the dispute review queue")?;
    // The receivable total is the number the operator is actually managing, and
    // summing a JSON array by eye is how it gets read wrong.
    let receivable_total: Decimal = rows.iter().filter_map(|row| row.receivable_usd).sum();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "window_days": args.days,
            "accounts": rows.len(),
            "receivable_total_usd": receivable_total,
            "queue": rows,
        }))?
    );
    Ok(())
}

/// One account, in full. This is the "reviews logs" surface.
async fn disputes_show(pool: &PgPool, args: DisputesShowArgs) -> Result<()> {
    let (user_id, email) = resolve_user(pool, &args.email).await?;
    let balance = crate::billing::balance(pool, user_id)
        .await
        .context("failed to read balance")?;
    let freeze = crate::billing::freeze_state(pool, user_id)
        .await
        .context("failed to read the freeze state")?;
    let ledger = crate::billing::ledger_history(pool, user_id)
        .await
        .context("failed to read the ledger")?;
    let usage = crate::billing::usage_summary(pool, user_id)
        .await
        .context("failed to summarize usage")?;
    // Same projection as `list_keys`: metadata only. The key hash is a
    // credential-equivalent and has no place in a review transcript.
    let keys = list_key_metadata(pool, Some(email.clone()))
        .await
        .context("failed to list API keys")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "user_id": user_id,
            "email": email,
            "balance_usd": balance,
            "receivable_usd": (balance < Decimal::ZERO).then(|| -balance),
            "frozen": freeze.is_some(),
            "frozen_at": freeze.as_ref().map(|(at, _)| at),
            "frozen_reason": freeze.as_ref().map(|(_, reason)| reason),
            "usage": usage,
            "ledger": ledger,
            "keys": keys,
        }))?
    );
    Ok(())
}

/// Settle a receivable. The only writer in this subcommand tree.
async fn disputes_resolve(pool: &PgPool, args: DisputesResolveArgs) -> Result<()> {
    let note = args.note.trim();
    if note.is_empty() {
        bail!("note cannot be empty; a resolution's note is the only record of why")
    }
    let recover = args
        .recover
        .as_deref()
        .map(|value| parse_decimal(value, "recover"))
        .transpose()?;
    // A "resolve" whose default is silently one direction is how a receivable
    // gets forgiven by an operator who meant to record a wire.
    let mode = match (args.write_off, recover) {
        (true, None) => None,
        (false, Some(amount)) => Some(amount),
        _ => bail!("pass exactly one of --write-off or --recover <amount>"),
    };
    if let Some(amount) = mode {
        if amount <= Decimal::ZERO {
            bail!("recover must be positive")
        }
        // The same fat-finger bound `grant-credit` uses, for the same reason:
        // an operator recording a wire should never be typing five digits by
        // accident. Run it twice for more.
        if amount > Decimal::from(10_000) {
            bail!("recover must be at most 10000")
        }
    }

    let (user_id, email) = resolve_user(pool, &args.email).await?;
    let frozen_before = crate::billing::freeze_state(pool, user_id)
        .await
        .context("failed to read the freeze state")?;

    let outcome = match mode {
        None => crate::billing::write_off_receivable(pool, user_id, note)
            .await
            .context("failed to write off the receivable")?,
        Some(amount) => crate::billing::record_recovery(pool, user_id, amount, note)
            .await
            .context("failed to record the recovery")?,
    };

    // Read back rather than trusting the outcome: this is the line an operator
    // will quote when they say the account was settled.
    let balance = crate::billing::balance(pool, user_id)
        .await
        .context("failed to read balance")?;
    let frozen_after = crate::billing::freeze_state(pool, user_id)
        .await
        .context("failed to re-read the freeze state")?;

    let (action, detail) = match &outcome {
        ResolutionOutcome::WrittenOff { forgiven_usd, .. } => (
            "written_off",
            serde_json::json!({ "forgiven_usd": forgiven_usd }),
        ),
        ResolutionOutcome::AlreadyWrittenOff { .. } => (
            "already_written_off",
            serde_json::json!({
                "detail": "this account was already written off and owes nothing; \
                           no second ledger entry was made",
            }),
        ),
        ResolutionOutcome::Recovered { amount_usd, .. } => (
            "recovered",
            serde_json::json!({ "recovered_usd": amount_usd }),
        ),
        ResolutionOutcome::Refused { reason } => {
            ("refused", serde_json::json!({ "reason": reason }))
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "user_id": user_id,
            "email": email,
            "action": action,
            "detail": detail,
            "balance_usd": balance,
            "receivable_usd": (balance < Decimal::ZERO).then(|| -balance),
            // Resolving settles money, never trust. Reported on both sides so
            // the transcript itself shows the freeze did not move.
            "frozen_before": frozen_before.is_some(),
            "frozen": frozen_after.is_some(),
            "frozen_reason": frozen_after.as_ref().map(|(_, reason)| reason),
            "note": note,
        }))?
    );
    // Loud failure last, so the operator still sees the state that refused.
    if let ResolutionOutcome::Refused { reason } = outcome {
        bail!("refused to resolve {email}: {reason}")
    }
    Ok(())
}

/// Resolve one quarantined reservation. The second writer in this subcommand
/// tree, and the only one that touches no balance at all.
///
/// Shaped like `disputes resolve`: validate the note before anything else, act
/// through one helper, print the resulting state as JSON, and turn a refusal
/// into a non-zero exit AFTER printing — so an operator whose command was
/// refused still sees the state that refused it.
async fn disputes_release_reservation(
    pool: &PgPool,
    args: DisputesReleaseReservationArgs,
) -> Result<()> {
    let note = args.note.trim();
    if note.is_empty() {
        bail!("note cannot be empty; a release's note is the only record of why")
    }

    let outcome = release_quarantined_reservation(pool, args.request_id, note, args.forgive)
        .await
        .context("failed to release the quarantined reservation")?;

    let (action, detail) = match &outcome {
        ReservationRelease::Released {
            released_at,
            owed,
            forgiven_usd,
        } => (
            "released",
            serde_json::json!({
                "released_at": released_at,
                // Which class was resolved, in the operator's own terms: a
                // debt given up, or a row that never had a collectable amount.
                "forgave_a_collectable_charge": owed,
                "forgiven_usd": forgiven_usd,
                "detail": if *owed {
                    "the settlement intent will not be collected; the credit it \
                     encumbered is released and the row is kept as the record"
                } else {
                    "no settlement intent was ever recorded, so there was no \
                     charge to collect and no encumbrance to release"
                },
            }),
        ),
        ReservationRelease::AlreadyReleased { released_at, note } => (
            "already_released",
            serde_json::json!({
                "released_at": released_at,
                "released_note": note,
                "detail": "this reservation was already released; nothing was \
                           written and the original note stands",
            }),
        ),
        ReservationRelease::Refused { reason } => {
            ("refused", serde_json::json!({ "reason": reason }))
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "request_id": args.request_id,
            "action": action,
            "detail": detail,
            "note": note,
        }))?
    );
    // Loud failure last, so the operator still sees what refused. A repeat is
    // a refusal here rather than the success `disputes resolve` reports for a
    // repeated write-off: that command's second run re-states a settled
    // BALANCE, which is a fact about the account; this one would be claiming
    // to have resolved a queue entry that someone else already resolved, for
    // a different stated reason.
    match outcome {
        ReservationRelease::Refused { reason } => {
            bail!("refused to release {}: {reason}", args.request_id)
        }
        ReservationRelease::AlreadyReleased { released_at, .. } => bail!(
            "reservation {} was already released at {released_at}",
            args.request_id
        ),
        ReservationRelease::Released { .. } => Ok(()),
    }
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
    let rows = list_key_metadata(pool, args.email).await?;
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

/// Key metadata for an optional email filter — hashes and plaintext excluded by
/// projection, not by redaction, so there is no way to widen it by accident.
///
/// Extracted from `list_keys` so `disputes show` renders keys through exactly
/// the same query. A second hand-written projection is how a review transcript
/// eventually grows a `key_hash` column.
async fn list_key_metadata(pool: &PgPool, email: Option<String>) -> Result<Vec<KeyMetadata>> {
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
    .bind(email.map(|email| email.trim().to_lowercase()))
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
    Ok(rows)
}

/// Reconcile the shipped tier file against a public model catalog.
///
/// Read-only by construction: it prints what it found and sets an exit code.
/// Prices are never written back. A catalog that reprices itself from a
/// network fetch can turn one bad upstream document into a billing incident,
/// and the staleness this detects is slow enough that a human in the loop
/// costs nothing.
async fn catalog_drift(args: CatalogDriftArgs) -> Result<()> {
    use crate::drift::{fetch_source, reconcile};

    let tiers_path = args.tiers.unwrap_or_else(|| {
        std::env::var("ZEROROUTER_TIERS_PATH")
            .unwrap_or_else(|_| crate::config::DEFAULT_TIER_CONFIG_PATH.to_owned())
            .into()
    });
    // An edge deployment's tier file names upstreams that exist only in the
    // operator's inventory, so this must be loaded before the catalog is — or
    // the load fails with "unsupported provider" and the one deployment shape
    // that most needs a drift report is the one that cannot run it. Same
    // variable the server reads, so `catalog-drift` and `serve` are looking at
    // the same world; also what makes the local-rung exemption reachable at all
    // (`drift::Verdict::Unreconcilable` keys on the provider's declaration).
    let providers_path = args.providers.or_else(|| {
        std::env::var_os(crate::providers::PROVIDER_INVENTORY_PATH_ENV)
            .map(std::path::PathBuf::from)
    });
    if let Some(path) = providers_path {
        let count = crate::providers::load_operator_inventory(&path).with_context(|| {
            format!(
                "loading the operator provider inventory from {}",
                path.display()
            )
        })?;
        println!(
            "operator providers: {count} from {path}",
            path = path.display()
        );
    }
    let catalog = crate::config::load_tier_catalog(&tiers_path)
        .await
        .with_context(|| format!("loading the tier catalog from {}", tiers_path.display()))?;

    let source = match &args.source_file {
        Some(path) => tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading the catalog source from {}", path.display()))?,
        None => fetch_source(&args.source_url).await?,
    };

    let findings = reconcile(&catalog, &source);
    println!(
        "{:<22} {:<32} {:<18} {:>26} {:>26} {:>13} {:>10}",
        "TIER", "CANDIDATE", "VERDICT", "RECORDED BASIS", "UPSTREAM COST", "UPSTREAM CTX", "MARKUP"
    );
    let rate = |r: crate::provider::ModelRates| {
        let show = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v}"));
        format!(
            "{}/{}/{}",
            show(r.input_per_mtok),
            show(r.cached_input_per_mtok),
            show(r.output_per_mtok)
        )
    };
    for found in &findings {
        println!(
            "{:<22} {:<32} {:<18} {:>26} {:>26} {:>13} {:>10}",
            found.tier,
            found.candidate_id,
            found.row_label(),
            rate(found.recorded_basis),
            rate(found.upstream_cost),
            found
                .upstream_metadata
                .context_window
                .map_or_else(|| "-".to_owned(), |c| format!("{c}")),
            found
                .sell_markup()
                .map_or_else(|| "-".to_owned(), |m| format!("{m:.2}x")),
        );
    }

    // An upstream that reprices past a threshold cannot be expressed by a row
    // holding one rate per dimension, so the table's flat comparison says
    // nothing about requests past it. Spelled out with the boundary and the
    // rates, because "somewhere above some size you are underpricing" is not
    // something anyone can act on.
    let tiered: Vec<_> = findings
        .iter()
        .filter(|f| f.upstream_tier.is_some())
        .collect();
    if !tiered.is_empty() {
        println!(
            "\nUpstream reprices past a threshold — the catalog holds one rate per dimension,"
        );
        println!("so every request past the boundary bills at a basis ZeroRouter does not pay:");
        for found in &tiered {
            println!(
                "  {} base {} then {}",
                found.tier,
                rate(found.upstream_cost),
                found.upstream_tier.as_deref().unwrap_or_default()
            );
        }
    }

    // A markup on a tier that advertises pass-through is not drift in the
    // file's own terms — basis == sell, so the validator is satisfied — but it
    // is the customer paying more than the model costs, which is exactly the
    // claim an operator needs told.
    let markups: Vec<_> = findings
        .iter()
        .filter(|f| f.is_undisclosed_markup())
        .collect();
    if !markups.is_empty() {
        println!("\nSelling above upstream cost:");
        for found in &markups {
            println!(
                "  {} charges {:.2}x the upstream output rate ({} vs {})",
                found.tier,
                found.sell_markup().unwrap_or_default(),
                found
                    .sell
                    .output_per_mtok
                    .map_or_else(|| "-".to_owned(), |v| format!("{v}")),
                found
                    .upstream_cost
                    .output_per_mtok
                    .map_or_else(|| "-".to_owned(), |v| format!("{v}")),
            );
        }
    }

    // Model metadata drifts for the same reason a price does — an upstream
    // moves and the file keeps asserting the old number — but it fails
    // differently. A stale rate costs margin; a stale context window costs
    // correctness, silently, on the client side: a window ZeroRouter
    // overstates becomes requests the upstream rejects, and one it understates
    // becomes long-context work quietly truncated. Neither shows up in the
    // ledger, so this is the only place it can be seen.
    let metadata: Vec<_> = findings
        .iter()
        .filter(|f| !f.metadata_drift.is_empty())
        .collect();
    if !metadata.is_empty() {
        println!("\nModel metadata:");
        for found in &metadata {
            for drift in &found.metadata_drift {
                println!(
                    "  {:<32} {:<18} {:<18} file {} vs source {}",
                    found.candidate_id,
                    drift.kind.label(),
                    drift.field,
                    drift.recorded,
                    drift.upstream,
                );
            }
        }
    }

    let actionable: Vec<_> = findings
        .iter()
        .filter(|f| f.verdict.is_actionable() || f.has_actionable_metadata_drift())
        .collect();
    if actionable.is_empty() {
        println!("\n{} candidates reconciled, no drift.", findings.len());
        return Ok(());
    }
    println!("\n{} candidate(s) need attention:", actionable.len());
    for found in &actionable {
        // A candidate can fail on both axes at once — a repriced model whose
        // window also moved — so report every reason rather than the first.
        if found.verdict.is_actionable() {
            println!(
                "  {} ({}) — {}",
                found.candidate_id,
                found.model,
                found.verdict.label()
            );
        }
        for drift in found
            .metadata_drift
            .iter()
            .filter(|drift| drift.kind.is_actionable())
        {
            println!(
                "  {} ({}) — {} {}: file says {}, source says {}",
                found.candidate_id,
                found.model,
                drift.kind.label(),
                drift.field,
                drift.recorded,
                drift.upstream,
            );
        }
    }
    if args.allow_drift {
        return Ok(());
    }
    anyhow::bail!(
        "{} candidate(s) drifted from {}; update tiers.toml deliberately, or pass --allow-drift",
        actionable.len(),
        args.source_file
            .as_ref()
            .map_or(args.source_url.clone(), |p| p.display().to_string())
    )
}

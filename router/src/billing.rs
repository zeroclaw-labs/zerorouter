//! Prepaid credit accounting: balance reads, the append-only ledger, and the
//! server-side record of what each Stripe Checkout Session was priced at.
//!
//! Every balance mutation happens inside a transaction that (a) holds the
//! per-user advisory lock used by admission and settlement in [`crate::db`],
//! and (b) appends a `credit_ledger` row snapshotting `balance_after_usd`.
//! Purchases are idempotent by `stripe_session_id`; a replayed webhook is a
//! no-op reported as [`CreditOutcome::AlreadyApplied`].
//!
//! [`CheckoutIntent`] is the other half of the purchase path: it is what
//! ZeroRouter decided to sell, written before the user is handed to Stripe and
//! required to exist before [`credit_purchase`] is ever reached. A webhook
//! HMAC proves Stripe sent the event; only this record proves ZeroRouter
//! priced it (migration `0005_stripe_checkout_intents.sql`).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::sqlx::{self, PgPool};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreditOutcome {
    Applied { balance_after: Decimal },
    AlreadyApplied,
}

/// What ZeroRouter quoted when it created a Stripe Checkout Session.
///
/// The webhook credits [`Self::expected_credit_usd`] to [`Self::user_id`] —
/// the event's `metadata` only has to agree with these fields, it is never the
/// source of truth for either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutIntent {
    pub stripe_session_id: String,
    pub user_id: Uuid,
    /// The GROSS `unit_amount` quoted to Stripe (credit + deposit fee), in the
    /// smallest currency unit (cents for USD), for comparison against the
    /// event's `amount_total`.
    pub expected_amount_cents: i64,
    /// The decimal-dollar NET credit this session buys — what the webhook
    /// applies to the balance. `expected_amount_cents >= expected_credit_usd *
    /// 100` by a database CHECK (the fee is the difference); before migration
    /// 0016 the two were held equal.
    pub expected_credit_usd: Decimal,
    /// Lowercase ISO-4217 code the session was priced in.
    pub currency: String,
    /// Stamped once the purchase has been credited; `None` while the session
    /// is unpaid, abandoned, or paid but not yet delivered.
    pub settled_at: Option<DateTime<Utc>>,
}

/// Record what a freshly created Checkout Session is worth, before the user is
/// sent to Stripe to pay it.
///
/// Fails on a duplicate `stripe_session_id`: Stripe session ids are unique, so
/// a collision means something other than this code path wrote the row.
pub async fn record_checkout_intent(
    pool: &PgPool,
    stripe_session_id: &str,
    user_id: Uuid,
    expected_amount_cents: i64,
    expected_credit_usd: Decimal,
    currency: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO stripe_checkout_intents (
            stripe_session_id,
            user_id,
            expected_amount_cents,
            expected_credit_usd,
            currency
        )
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(stripe_session_id)
    .bind(user_id)
    .bind(expected_amount_cents)
    .bind(expected_credit_usd)
    .bind(currency)
    .execute(pool)
    .await?;
    Ok(())
}

/// The pending-purchase record for a Stripe session, if ZeroRouter created it.
///
/// `None` is the fail-closed signal the webhook acts on: a paid session with
/// no record here was not priced by this deployment.
pub async fn checkout_intent(
    pool: &PgPool,
    stripe_session_id: &str,
) -> Result<Option<CheckoutIntent>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, Uuid, i64, Decimal, String, Option<DateTime<Utc>>)>(
        r#"
        SELECT stripe_session_id, user_id, expected_amount_cents, expected_credit_usd,
               currency, settled_at
        FROM stripe_checkout_intents
        WHERE stripe_session_id = $1
        "#,
    )
    .bind(stripe_session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(
            stripe_session_id,
            user_id,
            expected_amount_cents,
            expected_credit_usd,
            currency,
            settled_at,
        )| CheckoutIntent {
            stripe_session_id,
            user_id,
            expected_amount_cents,
            expected_credit_usd,
            currency,
            settled_at,
        },
    ))
}

/// Mark a pending purchase delivered, after its credit has committed.
///
/// A reconciliation marker, not a lock: idempotence of crediting belongs to
/// the unique index on `credit_ledger.stripe_session_id`, so this is safe to
/// call on a replay (it updates no rows) and safe to lose (the money is
/// already right). Returns whether this call was the one that stamped it.
pub async fn settle_checkout_intent(
    pool: &PgPool,
    stripe_session_id: &str,
) -> Result<bool, sqlx::Error> {
    let settled = sqlx::query(
        r#"
        UPDATE stripe_checkout_intents
        SET settled_at = NOW()
        WHERE stripe_session_id = $1 AND settled_at IS NULL
        "#,
    )
    .bind(stripe_session_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(settled > 0)
}

/// What one [`sweep_expired_checkout_intents`] pass removed.
///
/// Reported rather than merely counted so the sweep's log line is a usable
/// forensic record: the rows themselves are gone, and this is what survives of
/// them in the operator's logs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckoutIntentSweep {
    /// Rows deleted this pass. Equal to the batch limit means there is more to
    /// do and the next pass will do it.
    pub removed: u64,
    /// `created_at` of the oldest row removed — how far the backlog reached.
    pub oldest: Option<DateTime<Utc>>,
    /// Summed `expected_credit_usd` of the removed rows: credit that was
    /// quoted and never bought. Not money that moved (by construction none of
    /// these rows was ever credited), so it is an abandonment figure, not a
    /// ledger figure.
    pub quoted_credit_usd: Decimal,
}

/// The advisory-lock key the cleanup sweep serializes on, and salt 2's only
/// user.
///
/// Salts 0 and 1 are the per-user and per-PaymentIntent locks (see
/// [`lock_payment_intent`] for how `hashtextextended`'s salt selects a hash
/// function rather than a disjoint range, and why a cross-salt collision is
/// harmless). This transaction takes ONLY this key — never a user or intent
/// lock — so it cannot participate in a lock cycle with the money paths.
const CHECKOUT_INTENT_CLEANUP_LOCK: &str = "stripe_checkout_intent_cleanup";

/// Remove abandoned checkout intents: rows for Checkout Sessions that were
/// created, never paid, and are now older than Stripe can possibly complete.
///
/// # The predicate is the safety argument
///
/// ```text
/// settled_at IS NULL                      -- the webhook never delivered it
/// AND created_at < NOW() - retention_days -- stripe can no longer complete it
/// AND NOT EXISTS (credit_ledger row)      -- and it was never credited
/// ```
///
/// The third condition is the one that protects money, and it is deliberately
/// NOT expressed as `settled_at IS NULL`. That marker is stamped after the
/// credit commits and is explicitly allowed to be lost (see
/// [`settle_checkout_intent`]), so a row can be unsettled and yet credited. The
/// `credit_ledger` row cannot be lost — it is the purchase's idempotence anchor
/// and the ledger is append-only — so it is the authoritative answer to "did
/// money move for this session?".
///
/// **The database enforces the same three conditions independently** (migration
/// 0022 narrows `reject_stripe_checkout_intent_mutation` to permit DELETE only
/// for rows meeting them, with a hard seven-day floor). This query is therefore
/// the fast path, not the guarantee: a bug that widened it would abort against
/// the trigger rather than delete a credited row.
///
/// # Shape
///
/// The inner `SELECT` rides `stripe_checkout_intents_unsettled_idx` — a partial
/// index on `(created_at) WHERE settled_at IS NULL`, which matches this
/// predicate and this ordering exactly — so the candidate scan is an ordered
/// index read that stops at `limit`, never a table scan.
///
/// **The candidate scan carries the whole predicate, not just the cheap half,
/// and that is not redundancy.** The rows a credit landed on but a
/// [`settle_checkout_intent`] failed to stamp are permanent AND unsettled AND
/// (in time) the oldest rows in the table, so a candidate scan filtered only on
/// `settled_at` would hand the same undeletable rows to every pass forever,
/// consuming the batch and deleting nothing. The sweep would report success and
/// silently stop working. Selecting on the full predicate is what keeps a
/// bounded batch a batch of *deletable* rows.
///
/// The predicate is then repeated on the DELETE itself, and that repetition IS
/// the race guard: a candidate credited between the scan and the delete is
/// dropped from the batch by the query rather than aborting the whole statement
/// on migration 0022's trigger.
///
/// Returns `None` when another task already holds the sweep lock: cleanup is
/// pure maintenance, so a skipped pass costs nothing and the next one picks up
/// the same rows.
pub async fn sweep_expired_checkout_intents(
    pool: &PgPool,
    retention_days: i32,
    limit: i64,
) -> Result<Option<CheckoutIntentSweep>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // `try`, not the blocking `pg_advisory_xact_lock` the money paths use: two
    // sweeps doing the same maintenance is waste, not a fault, and waiting for
    // the other one only to find it has already deleted the batch is more
    // waste. Transaction-scoped so it is released by the commit or the rollback
    // and can never be leaked onto a pooled connection.
    let acquired = sqlx::query_scalar::<_, bool>(
        "SELECT pg_try_advisory_xact_lock(hashtextextended($1::TEXT, 2))",
    )
    .bind(CHECKOUT_INTENT_CLEANUP_LOCK)
    .fetch_one(&mut *transaction)
    .await?;
    if !acquired {
        transaction.rollback().await?;
        return Ok(None);
    }
    let removed = sqlx::query_as::<_, (DateTime<Utc>, Decimal)>(
        r#"
        DELETE FROM stripe_checkout_intents
        WHERE stripe_session_id IN (
            SELECT candidate.stripe_session_id
            FROM stripe_checkout_intents candidate
            WHERE candidate.settled_at IS NULL
              AND candidate.created_at < NOW() - ($1 * INTERVAL '1 day')
              AND NOT EXISTS (
                  SELECT 1
                  FROM credit_ledger
                  WHERE credit_ledger.stripe_session_id
                        = candidate.stripe_session_id
              )
            ORDER BY candidate.created_at
            LIMIT $2
        )
          AND settled_at IS NULL
          AND created_at < NOW() - ($1 * INTERVAL '1 day')
          AND NOT EXISTS (
              SELECT 1
              FROM credit_ledger
              WHERE credit_ledger.stripe_session_id
                    = stripe_checkout_intents.stripe_session_id
          )
        RETURNING created_at, expected_credit_usd
        "#,
    )
    .bind(retention_days)
    .bind(limit)
    .fetch_all(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Some(CheckoutIntentSweep {
        removed: removed.len() as u64,
        oldest: removed.iter().map(|(created_at, _)| *created_at).min(),
        quoted_credit_usd: removed.iter().map(|(_, credit)| *credit).sum(),
    }))
}

/// Apply a completed Stripe Checkout purchase to the user's balance.
///
/// Idempotent by `stripe_session_id`: the first call credits the balance and
/// appends a `purchase` ledger row; every replay returns
/// [`CreditOutcome::AlreadyApplied`] without touching the balance.
pub async fn credit_purchase(
    pool: &PgPool,
    user_id: Uuid,
    amount_usd: Decimal,
    stripe_session_id: &str,
    stripe_payment_intent_id: Option<&str>,
) -> Result<CreditOutcome, sqlx::Error> {
    if amount_usd <= Decimal::ZERO {
        return Err(sqlx::Error::Protocol(
            "credit purchase amount must be positive".to_owned(),
        ));
    }
    if stripe_session_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "credit purchase requires a Stripe session id".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Same USER-keyed advisory lock as admission/settlement in `crate::db`.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;
    // FIX A (HIGH-1 round 2): also take the PaymentIntent lock — AFTER the user
    // lock, never before — so this whole credit (including apply_observed_reversals
    // below) serializes with a reversal for the same intent that has no user to
    // lock on. The reversal path takes ONLY the intent lock, so this fixed
    // user→intent order cannot deadlock. Only when the credit names an intent.
    if let Some(payment_intent_id) = stripe_payment_intent_id {
        lock_payment_intent(&mut transaction, payment_intent_id).await?;
    }

    let already_applied =
        sqlx::query_scalar::<_, i32>("SELECT 1 FROM credit_ledger WHERE stripe_session_id = $1")
            .bind(stripe_session_id)
            .fetch_optional(&mut *transaction)
            .await?
            .is_some();
    if already_applied {
        transaction.rollback().await?;
        return Ok(CreditOutcome::AlreadyApplied);
    }

    let balance_after = sqlx::query_scalar::<_, Decimal>(
        r#"
        UPDATE users
        SET credit_balance_usd = credit_balance_usd + $2
        WHERE id = $1
        RETURNING credit_balance_usd
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| sqlx::Error::Protocol("credit purchase found no such user".to_owned()))?;
    sqlx::query(
        r#"
        INSERT INTO credit_ledger (
            user_id,
            entry_type,
            amount_usd,
            balance_after_usd,
            stripe_session_id,
            stripe_payment_intent_id
        )
        VALUES ($1, 'purchase', $2, $3, $4, $5)
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .bind(balance_after)
    .bind(stripe_session_id)
    .bind(stripe_payment_intent_id)
    .execute(&mut *transaction)
    .await?;
    // HIGH-1 (migration 0017): if a refund/dispute for this PaymentIntent was
    // observed BEFORE this credit existed, converge to the reversed (and, for a
    // dispute, frozen) end state now — under the same advisory lock and in the
    // same transaction — so no spendable refunded credit is ever visible.
    if let Some(payment_intent_id) = stripe_payment_intent_id {
        apply_observed_reversals(&mut transaction, user_id, payment_intent_id, amount_usd).await?;
    }
    transaction.commit().await?;
    Ok(CreditOutcome::Applied { balance_after })
}

/// Grant promotional credit (e.g. the first-login signup credit).
///
/// A zero amount is a no-op: no ledger row is written (the ledger forbids
/// zero amounts) and the balance is untouched.
pub async fn grant_promo(
    pool: &PgPool,
    user_id: Uuid,
    amount_usd: Decimal,
    note: &str,
) -> Result<(), sqlx::Error> {
    if amount_usd == Decimal::ZERO {
        return Ok(());
    }
    if amount_usd < Decimal::ZERO {
        return Err(sqlx::Error::Protocol(
            "promo credit amount cannot be negative".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    let balance_after = sqlx::query_scalar::<_, Decimal>(
        r#"
        UPDATE users
        SET credit_balance_usd = credit_balance_usd + $2
        WHERE id = $1
        RETURNING credit_balance_usd
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| sqlx::Error::Protocol("promo grant found no such user".to_owned()))?;
    sqlx::query(
        r#"
        INSERT INTO credit_ledger (user_id, entry_type, amount_usd, balance_after_usd, note)
        VALUES ($1, 'promo', $2, $3, $4)
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .bind(balance_after)
    .bind(note)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

/// Current prepaid balance for a user.
pub async fn balance(pool: &PgPool, user_id: Uuid) -> Result<Decimal, sqlx::Error> {
    sqlx::query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
}

#[derive(Debug, serde::Serialize)]
pub struct LedgerEntry {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub entry_type: String,
    pub amount_usd: Decimal,
    pub balance_after_usd: Decimal,
    pub note: Option<String>,
}

/// Newest-first ledger entries scoped to a single user.
pub async fn ledger_entries(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<LedgerEntry>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, DateTime<Utc>, String, Decimal, Decimal, Option<String>)>(
        r#"
        SELECT id, created_at, entry_type, amount_usd, balance_after_usd, note
        FROM credit_ledger
        WHERE user_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        "#,
    )
    .bind(user_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, created_at, entry_type, amount_usd, balance_after_usd, note)| LedgerEntry {
                id,
                created_at,
                entry_type,
                amount_usd,
                balance_after_usd,
                note,
            },
        )
        .collect())
}

// ---------------------------------------------------------------------------
// Refunds, chargebacks, and the account freeze (migration 0009)
// ---------------------------------------------------------------------------

/// Why an account is frozen. A closed set, mirroring the CHECK in `0009`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezeReason {
    /// A Stripe chargeback. Set by the webhook, never by a human.
    Dispute,
    /// An operator's deliberate hold (`admin set-frozen --on`).
    Operator,
}

impl FreezeReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dispute => "dispute",
            Self::Operator => "operator",
        }
    }
}

/// Freeze an account: admission refuses new inference and the self-service
/// mint paths refuse new keys, until an operator lifts it.
///
/// Idempotent, and deliberately first-writer-wins: a dispute freeze that is
/// already in place is not restamped by a redelivered webhook, so `frozen_at`
/// keeps saying when the account actually stopped. Returns whether this call
/// was the one that froze it.
///
/// Freezing is intentionally NOT part of the reversal transaction. The two
/// answer different questions — "stop the spend" and "state the debt" — and a
/// dispute whose purchase cannot be mapped back to a ledger row must still
/// freeze if the user is known, while a reversal that fails must be retryable
/// without the freeze flapping.
pub async fn freeze_account(
    pool: &PgPool,
    user_id: Uuid,
    reason: FreezeReason,
) -> Result<bool, sqlx::Error> {
    let frozen = sqlx::query(
        r#"
        UPDATE users
        SET frozen_at = NOW(), frozen_reason = $2
        WHERE id = $1 AND frozen_at IS NULL
        "#,
    )
    .bind(user_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(frozen > 0)
}

/// Lift a freeze, restoring inference and key minting.
///
/// The operator's only path back, and the reason a freeze is safe to ship
/// before the review workflow exists: a control with no documented release is
/// an outage waiting for a support ticket. Returns whether this call was the
/// one that thawed it.
pub async fn unfreeze_account(pool: &PgPool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let thawed = sqlx::query(
        r#"
        UPDATE users
        SET frozen_at = NULL, frozen_reason = NULL
        WHERE id = $1 AND frozen_at IS NOT NULL
        "#,
    )
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(thawed > 0)
}

/// Current freeze state: `(frozen_at, reason)`, or `None` when the account is
/// live.
pub async fn freeze_state(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<(DateTime<Utc>, String)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Option<DateTime<Utc>>, Option<String>)>(
        "SELECT frozen_at, frozen_reason FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(match row {
        (Some(frozen_at), Some(reason)) => Some((frozen_at, reason)),
        // The 0009 CHECK makes the two columns one fact, so a half-set pair
        // cannot exist; treating anything else as "live" is fail-safe anyway.
        _ => None,
    })
}

/// A credit ZeroRouter applied for a Stripe PaymentIntent, as its own ledger
/// records it — the server-side answer to "what did this charge buy?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreditedPurchase {
    pub user_id: Uuid,
    /// The dollars credited, positive.
    pub amount_usd: Decimal,
    /// `purchase` (Checkout) or `autopay` (off-session recharge).
    pub entry_type: String,
}

/// The credit a PaymentIntent bought, looked up the way a dispute arrives:
/// by the Stripe object id, not by metadata.
///
/// This is the reversal's equivalent of the checkout arm's
/// [`checkout_intent`] precondition, and it is a strictly stronger one. A
/// dispute event names its `payment_intent` and `charge` — Stripe's own
/// fields, not the attacker-writable `metadata` — and this joins that id
/// against ZeroRouter's own ledger. An event naming an intent this deployment
/// never credited returns `None` and moves nothing.
///
/// Both credit shapes are covered: a Checkout purchase records the intent in
/// `stripe_payment_intent_id`, while an autopay recharge (0008) anchors itself
/// on `stripe_session_id` because the intent id IS its idempotence key. Stripe
/// object ids are globally unique, so accepting either column cannot
/// cross-match.
pub async fn credited_purchase(
    pool: &PgPool,
    payment_intent_id: &str,
) -> Result<Option<CreditedPurchase>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Decimal, String)>(
        r#"
        SELECT user_id, amount_usd, entry_type
        FROM credit_ledger
        WHERE entry_type IN ('purchase', 'autopay')
          AND (stripe_payment_intent_id = $1 OR stripe_session_id = $1)
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(payment_intent_id)
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map(|(user_id, amount_usd, entry_type)| CreditedPurchase {
            user_id,
            amount_usd,
            entry_type,
        }),
    )
}

/// What a reversal did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReversalOutcome {
    Reversed {
        amount_usd: Decimal,
        balance_after: Decimal,
    },
    /// This purchase has already been reversed — by a redelivery of the same
    /// event, or by a different Stripe object reversing the same charge.
    AlreadyReversed,
    /// No credit in this deployment's ledger belongs to that PaymentIntent.
    UnknownPurchase,
}

/// Take back what a refunded or disputed charge credited.
///
/// The mirror image of [`credit_purchase`], and deliberately built from the
/// same parts: the per-user advisory lock that admission and settlement
/// serialize on, one transaction, and a `credit_ledger` row snapshotting the
/// balance it produced.
///
/// # Idempotence
///
/// Two anchors, both checked under the lock:
///
/// 1. `stripe_object_id` — the dispute or charge id — occupies the unique
///    `credit_ledger.stripe_session_id` index, exactly as a Checkout session id
///    anchors a purchase. A redelivered webhook finds its own row.
/// 2. The originating PaymentIntent. A charge can be reversed by more than one
///    Stripe object (a dispute, then an operator refund of the same charge),
///    and those carry DIFFERENT ids, so anchor 1 alone would reverse the same
///    purchase twice. Both writers serialize on the user's advisory lock, so
///    this check is not racy.
///
/// # Why the balance may go negative
///
/// The reversal takes back what was credited, not what is left. A customer who
/// spent the credit before disputing it lands below zero, and that number is
/// the receivable — visible in the ledger, for the review workflow to act on.
/// Clamping at zero would silently forgive the debt. The transaction declares
/// itself to the `0009` overdraft trigger for exactly this reason; no other
/// path may do so, so settlement is still backstopped as `0003` left it.
pub async fn reverse_purchase(
    pool: &PgPool,
    payment_intent_id: &str,
    stripe_object_id: &str,
    note: &str,
) -> Result<ReversalOutcome, sqlx::Error> {
    if stripe_object_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "credit reversal requires a Stripe object id".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Declares this transaction — and only this one — allowed to drive the
    // balance below zero (migration 0009). SET LOCAL, so it is discarded at
    // commit and cannot leak to the next transaction on a pooled connection.
    sqlx::query("SET LOCAL zerorouter.credit_reversal = 'on'")
        .execute(&mut *transaction)
        .await?;

    let credited = sqlx::query_as::<_, (Uuid, Decimal)>(
        r#"
        SELECT user_id, amount_usd
        FROM credit_ledger
        WHERE entry_type IN ('purchase', 'autopay')
          AND (stripe_payment_intent_id = $1 OR stripe_session_id = $1)
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(payment_intent_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((user_id, amount_usd)) = credited else {
        transaction.rollback().await?;
        return Ok(ReversalOutcome::UnknownPurchase);
    };

    // Same USER-keyed advisory lock as admission, settlement, and
    // `credit_purchase`. Taken before the idempotence check so two reversals
    // of the same charge cannot both observe "not yet reversed".
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    let already_reversed = sqlx::query_scalar::<_, i32>(
        r#"
        SELECT 1 FROM credit_ledger
        WHERE stripe_session_id = $1
           OR (entry_type = 'refund' AND stripe_payment_intent_id = $2)
        LIMIT 1
        "#,
    )
    .bind(stripe_object_id)
    .bind(payment_intent_id)
    .fetch_optional(&mut *transaction)
    .await?
    .is_some();
    if already_reversed {
        transaction.rollback().await?;
        return Ok(ReversalOutcome::AlreadyReversed);
    }

    let balance_after = sqlx::query_scalar::<_, Decimal>(
        r#"
        UPDATE users
        SET credit_balance_usd = credit_balance_usd - $2
        WHERE id = $1
        RETURNING credit_balance_usd
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| sqlx::Error::Protocol("credit reversal found no such user".to_owned()))?;
    sqlx::query(
        r#"
        INSERT INTO credit_ledger (
            user_id,
            entry_type,
            amount_usd,
            balance_after_usd,
            stripe_session_id,
            stripe_payment_intent_id,
            note
        )
        VALUES ($1, 'refund', $2, $3, $4, $5, $6)
        "#,
    )
    .bind(user_id)
    .bind(-amount_usd)
    .bind(balance_after)
    .bind(stripe_object_id)
    .bind(payment_intent_id)
    .bind(note)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(ReversalOutcome::Reversed {
        amount_usd,
        balance_after,
    })
}

// ---------------------------------------------------------------------------
// Reversal observed before its credit existed (migration 0017, HIGH-1)
// ---------------------------------------------------------------------------

/// Take the PaymentIntent-keyed advisory lock (`hashtextextended(intent, 1)`),
/// the serialization point between a credit and a reversal for the SAME
/// PaymentIntent (migration 0017, HIGH-1 round 2 / FIX A).
///
/// Salt **1** selects a DIFFERENT `hashtextextended` hash function than the
/// per-user lock's salt 0 — NOT a disjoint output range. The two 64-bit spaces
/// therefore overlap, and a user-lock key can occasionally collide with an
/// intent-lock key. That collision is harmless: it only adds extra advisory-lock
/// contention (resolved by the 5s `lock_timeout` every holder sets), and it
/// cannot lose same-intent serialization or move money, because correctness
/// rests on both sides of ONE intent taking the SAME key, never on user and
/// intent keys being distinct. LOCK ORDER, everywhere both are held: the USER
/// lock (salt 0) FIRST, then this intent lock — so no cycle is possible. The
/// reversal path takes ONLY this lock (it has no user to lock on until it finds
/// a credit); the credit paths take the user lock and then this one before
/// checking and crediting. That makes "credit vs reversal for one intent" a race
/// exactly one side wins, and the loser sees the winner's committed effect.
async fn lock_payment_intent(
    conn: &mut sqlx_postgres::PgConnection,
    payment_intent_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 1))")
        .bind(payment_intent_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Write (or monotonically merge) the tombstone for a reversal observed before
/// its credit, inside the caller's transaction — which already holds the intent
/// lock, so this is serialized against the crediting side.
///
/// `charge.refunded` carries the charge's CUMULATIVE `amount_refunded`, so two
/// refund events for one charge share the same object id. `ON CONFLICT DO
/// NOTHING` kept the first, possibly-partial amount and dropped a later fuller
/// one (FIX C). Instead merge NULL-safe-monotonically with `GREATEST`, and ONLY
/// while the tombstone is still unapplied — never resurrect a consumed one,
/// because once the credit exists a later refund takes the normal webhook path.
/// The anchoring fields (`payment_intent_id` / `is_dispute` / `currency`) are
/// NOT overwritten: a differing value on the same object id is a data error (a
/// dispute id and a charge id live in different namespaces and cannot collide),
/// logged for an operator rather than silently clobbered.
async fn insert_observed_reversal(
    conn: &mut sqlx_postgres::PgConnection,
    object_id: &str,
    payment_intent_id: &str,
    is_dispute: bool,
    reversed_cents: Option<i64>,
    currency: Option<&str>,
) -> Result<(), sqlx::Error> {
    if object_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "observed reversal requires a Stripe object id".to_owned(),
        ));
    }
    if payment_intent_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "observed reversal requires a payment intent id".to_owned(),
        ));
    }
    // A reused object id whose anchoring fields disagree is a data error we
    // record but refuse to act on. The DO UPDATE below touches only
    // reversed_cents, so the original row's intent/type/currency always stand;
    // and its WHERE now ALSO requires the anchors to match, so a mismatched
    // event cannot even merge its amount in (a larger conflicting amount must
    // not raise this intent's coverage). The equality is enforced atomically in
    // the ON CONFLICT clause rather than trusting the SELECT above — that SELECT
    // is only for the operator log, and is racy because two conflicting intents
    // take different intent locks. We surface the collision for reconciliation
    // instead of guessing.
    if let Some((existing_intent, existing_is_dispute, existing_currency)) =
        sqlx::query_as::<_, (String, bool, Option<String>)>(
            "SELECT payment_intent_id, is_dispute, currency \
             FROM stripe_observed_reversals WHERE object_id = $1",
        )
        .bind(object_id)
        .fetch_optional(&mut *conn)
        .await?
        && (existing_intent != payment_intent_id
            || existing_is_dispute != is_dispute
            || existing_currency.as_deref() != currency)
    {
        tracing::error!(
            stripe_object_id = %object_id,
            %existing_intent,
            new_intent = %payment_intent_id,
            existing_is_dispute,
            new_is_dispute = is_dispute,
            "observed reversal object id reused with a different intent / type / currency; \
             keeping the original tombstone and NOT overwriting it — reconcile by hand"
        );
    }
    sqlx::query(
        r#"
        INSERT INTO stripe_observed_reversals
            (object_id, payment_intent_id, is_dispute, reversed_cents, currency)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (object_id) DO UPDATE
            SET reversed_cents = GREATEST(
                    COALESCE(EXCLUDED.reversed_cents, stripe_observed_reversals.reversed_cents),
                    COALESCE(stripe_observed_reversals.reversed_cents, EXCLUDED.reversed_cents)
                )
            WHERE stripe_observed_reversals.applied_at IS NULL
              AND stripe_observed_reversals.payment_intent_id = EXCLUDED.payment_intent_id
              AND stripe_observed_reversals.is_dispute = EXCLUDED.is_dispute
              AND stripe_observed_reversals.currency IS NOT DISTINCT FROM EXCLUDED.currency
        "#,
    )
    .bind(object_id)
    .bind(payment_intent_id)
    .bind(is_dispute)
    .bind(reversed_cents)
    .bind(currency)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Resolve a Stripe reversal against the credit ledger UNDER THE INTENT LOCK,
/// atomically closing the credit/tombstone race (migration 0017, HIGH-1 round
/// 2 / FIX A).
///
/// The reversal path has no user to lock on while the charge is uncredited, so
/// before this it ran as two autocommit statements — a `credited_purchase`
/// lookup and a tombstone insert — and a credit committing between them was
/// seen by neither: refunded money stayed spendable and a dispute stayed
/// unfrozen. Now the lookup and, when no credit exists yet, the tombstone write
/// happen in ONE transaction that first takes the PaymentIntent lock. The
/// credit paths take the SAME lock (after the user lock) before crediting, so
/// whichever side takes the intent lock first is fully visible to the other:
///
/// - reversal first → tombstone committed under the lock → the credit's
///   [`apply_observed_reversals`] consumes it (reverses, and freezes a dispute);
/// - credit first → this lookup returns `Some` → the caller takes the normal
///   reverse-and-freeze path.
///
/// Returns the credit when one exists (the caller reverses/freezes as it always
/// has), or `None` after durably recording the tombstone (nothing to do now;
/// the credit converges when it lands). The lookup mirrors [`credited_purchase`]
/// — attribution is by the reversal's PaymentIntent against ZeroRouter's OWN
/// ledger, never metadata.
pub async fn resolve_reversal_against_credit(
    pool: &PgPool,
    object_id: &str,
    payment_intent_id: &str,
    is_dispute: bool,
    reversed_cents: Option<i64>,
    currency: Option<&str>,
) -> Result<Option<CreditedPurchase>, sqlx::Error> {
    if object_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "observed reversal requires a Stripe object id".to_owned(),
        ));
    }
    if payment_intent_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "observed reversal requires a payment intent id".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Only the intent lock (salt 1). The reversal path never takes the user
    // lock, so it can never hold the user lock while waiting on the intent lock
    // — that is what keeps the user→intent order the credit paths use free of a
    // cycle.
    lock_payment_intent(&mut transaction, payment_intent_id).await?;

    // A Checkout purchase records the intent in stripe_payment_intent_id; an
    // autopay recharge anchors on stripe_session_id because the intent id IS its
    // idempotence key. Stripe object ids are globally unique, so accepting
    // either column cannot cross-match.
    let credited = sqlx::query_as::<_, (Uuid, Decimal, String)>(
        r#"
        SELECT user_id, amount_usd, entry_type
        FROM credit_ledger
        WHERE entry_type IN ('purchase', 'autopay')
          AND (stripe_payment_intent_id = $1 OR stripe_session_id = $1)
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(payment_intent_id)
    .fetch_optional(&mut *transaction)
    .await?;

    if let Some((user_id, amount_usd, entry_type)) = credited {
        // The credit is committed — and, because we hold the intent lock, fully
        // committed, not a half-applied credit still inside its transaction. The
        // caller reverses/freezes on the returned credit. This found path only
        // READS: it stamps no tombstone.
        //
        // FIX 2' (round 4): the stale-tombstone stamp that FIX 3 did here was
        // premature. `handle_reversal_event` calls this, gets the credit back,
        // and only AFTERWARDS — in separate transactions — freezes and runs
        // `reverse_purchase`. Stamping `applied_at` before that reversal actually
        // lands meant a `reverse_purchase` that then FAILED (e.g. its 5s user-lock
        // wait timed out and the webhook 503'd) left the tombstone falsely marked
        // "applied", hiding a real unreconciled reversal — permanently if retries
        // exhausted. The stamp now happens only after the reversal succeeds, via
        // `mark_intent_reversals_applied`, called from `handle_reversal_event`
        // once `reverse_purchase` returns Reversed/AlreadyReversed (and, for a
        // dispute, after the freeze succeeds). A failed reversal therefore leaves
        // the tombstone unapplied — correctly retryable and operator-visible.
        transaction.commit().await?;
        return Ok(Some(CreditedPurchase {
            user_id,
            amount_usd,
            entry_type,
        }));
    }

    // No credit yet: record the tombstone before releasing the lock, so a credit
    // that is waiting on the intent lock is guaranteed to see it and converge.
    insert_observed_reversal(
        &mut transaction,
        object_id,
        payment_intent_id,
        is_dispute,
        reversed_cents,
        currency,
    )
    .await?;
    transaction.commit().await?;
    Ok(None)
}

/// Stamp every still-unapplied observed-reversal tombstone for
/// `payment_intent_id` `applied_at = NOW()` — the reconciliation-flag discharge
/// FIX 3 recorded, moved (FIX 2', round 4) to run ONLY after the reversal has
/// actually landed.
///
/// [`handle_reversal_event`](crate::stripe) calls this after `reverse_purchase`
/// returns `Reversed`/`AlreadyReversed` (and, for a dispute, after the freeze
/// succeeds), so a covering reversal that fully discharges the credit clears any
/// stale non-covering tombstone left behind — while a reversal that FAILED never
/// reaches here, leaving the tombstone unapplied and operator-visible. The caller
/// has already established coverage (`reverse_purchase` runs only on a covering
/// event), so every unapplied tombstone for this now-reversed intent is genuinely
/// resolved; `reverse_purchase` is per-intent idempotent, so this moves no money —
/// it only clears the flag.
///
/// Takes the SAME per-intent advisory lock (salt 1) the credit and reversal paths
/// take, so it serializes with them; it never takes the user lock, so it cannot
/// deadlock against a concurrent credit. Returns how many tombstones were stamped
/// (0 when there was no stale flag to clear).
pub async fn mark_intent_reversals_applied(
    pool: &PgPool,
    payment_intent_id: &str,
) -> Result<u64, sqlx::Error> {
    if payment_intent_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "stamping observed reversals requires a payment intent id".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    lock_payment_intent(&mut transaction, payment_intent_id).await?;
    let stamped = sqlx::query(
        r#"
        UPDATE stripe_observed_reversals
        SET applied_at = NOW()
        WHERE payment_intent_id = $1
          AND applied_at IS NULL
        "#,
    )
    .bind(payment_intent_id)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    transaction.commit().await?;
    Ok(stamped)
}

/// Bring a just-applied credit to the same end state a reversal arriving AFTER
/// it would have produced, for every reversal this deployment observed BEFORE
/// the credit existed (migration 0017, HIGH-1).
///
/// Runs INSIDE the credit path's transaction, which already holds the per-user
/// advisory lock, so the credit and its compensating reversal commit together
/// and no window exists where the credit is spendable. It mirrors the webhook
/// reversal path rather than inventing new logic:
///
/// - a dispute tombstone FREEZES the account (inline, idempotent), exactly as
///   the normal dispute path does — even when the amount does not cover the
///   credit, because the freeze is the half that cannot wait;
/// - a reversal is applied to the credit only when it COVERS the whole credit
///   and only once per intent — the same two guards [`reverse_purchase`]
///   enforces; a partial / foreign-currency / amount-less tombstone reverses
///   nothing (an operator reconciles it), and a second covering tombstone for
///   the same intent finds the reversal already present and skips it;
/// - the reversal accounting is the same shape as [`reverse_purchase`] (a
///   `refund` ledger row anchored on the object id and the intent, snapshotting
///   the balance), done inline because [`reverse_purchase`] and
///   [`freeze_account`] each open their own pool connection and would deadlock
///   against the advisory lock this transaction holds.
///
/// Every consumed tombstone is stamped `applied_at`, so it is applied exactly
/// once no matter how the credit is later replayed.
async fn apply_observed_reversals(
    conn: &mut sqlx_postgres::PgConnection,
    user_id: Uuid,
    payment_intent_id: &str,
    credited_amount_usd: Decimal,
) -> Result<(), sqlx::Error> {
    // Claim every unapplied tombstone for this intent, oldest first. `covers`
    // is computed in SQL so the cents comparison happens in the same numeric
    // domain the amounts are stored in; COALESCE guarantees a non-NULL bool
    // even when currency or amount is missing.
    let tombstones = sqlx::query_as::<_, (String, bool, bool)>(
        r#"
        SELECT object_id,
               is_dispute,
               COALESCE(
                   currency = 'usd'
                   AND reversed_cents IS NOT NULL
                   AND reversed_cents >= ROUND($2 * 100)::bigint,
                   FALSE
               ) AS covers
        FROM stripe_observed_reversals
        WHERE payment_intent_id = $1 AND applied_at IS NULL
        ORDER BY observed_at, object_id
        FOR UPDATE
        "#,
    )
    .bind(payment_intent_id)
    .bind(credited_amount_usd)
    .fetch_all(&mut *conn)
    .await?;
    if tombstones.is_empty() {
        return Ok(());
    }

    // A reversal may drive the balance below zero when the account already
    // carried a receivable; declare this transaction to the 0009 overdraft
    // trigger exactly as reverse_purchase does. SET LOCAL is discarded at
    // commit, so it never leaks to the next transaction on a pooled connection.
    sqlx::query("SET LOCAL zerorouter.credit_reversal = 'on'")
        .execute(&mut *conn)
        .await?;

    for (object_id, is_dispute, covers) in tombstones {
        if is_dispute {
            // Inline freeze mirroring freeze_account: idempotent
            // (first-writer-wins on frozen_at), and the account must stop
            // spending even when the reversal cannot be computed.
            sqlx::query(
                r#"
                UPDATE users
                SET frozen_at = NOW(), frozen_reason = 'dispute'
                WHERE id = $1 AND frozen_at IS NULL
                "#,
            )
            .bind(user_id)
            .execute(&mut *conn)
            .await?;
        }

        if covers {
            // The same "already reversed?" check reverse_purchase makes: a
            // redelivered object (anchor 1) or any prior refund on this intent
            // (anchor 2) means the credit is already taken back, so a second
            // covering tombstone reverses nothing.
            let already_reversed = sqlx::query_scalar::<_, i32>(
                r#"
                SELECT 1 FROM credit_ledger
                WHERE stripe_session_id = $1
                   OR (entry_type = 'refund' AND stripe_payment_intent_id = $2)
                LIMIT 1
                "#,
            )
            .bind(&object_id)
            .bind(payment_intent_id)
            .fetch_optional(&mut *conn)
            .await?
            .is_some();
            if !already_reversed {
                let balance_after = sqlx::query_scalar::<_, Decimal>(
                    r#"
                    UPDATE users
                    SET credit_balance_usd = credit_balance_usd - $2
                    WHERE id = $1
                    RETURNING credit_balance_usd
                    "#,
                )
                .bind(user_id)
                .bind(credited_amount_usd)
                .fetch_one(&mut *conn)
                .await?;
                let note = if is_dispute {
                    format!("chargeback reversal ({object_id}); reversal observed before credit")
                } else {
                    format!("refund reversal ({object_id}); reversal observed before credit")
                };
                sqlx::query(
                    r#"
                    INSERT INTO credit_ledger (
                        user_id,
                        entry_type,
                        amount_usd,
                        balance_after_usd,
                        stripe_session_id,
                        stripe_payment_intent_id,
                        note
                    )
                    VALUES ($1, 'refund', $2, $3, $4, $5, $6)
                    "#,
                )
                .bind(user_id)
                .bind(-credited_amount_usd)
                .bind(balance_after)
                .bind(&object_id)
                .bind(payment_intent_id)
                .bind(&note)
                .execute(&mut *conn)
                .await?;
            }
        }

        // Stamp the tombstone applied ONLY when its obligation is discharged, so
        // the operator's unapplied index stays honest (FIX C):
        //   - a DISPUTE's obligation is the freeze, which always ran above, so a
        //     dispute tombstone is consumed here whether or not it also covered;
        //   - a REFUND's obligation is the reversal, so it is consumed only when
        //     it covered. A NON-COVERING refund stays unapplied — an
        //     operator-visible reconciliation flag that a later cumulative refund
        //     event can still merge into, rather than being lost as "applied".
        let discharged = is_dispute || covers;
        if discharged {
            sqlx::query(
                "UPDATE stripe_observed_reversals SET applied_at = NOW() WHERE object_id = $1",
            )
            .bind(&object_id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispute review and resolution (migration 0013)
// ---------------------------------------------------------------------------

/// Why an account is in the review queue. An account can carry more than one.
///
/// These are the three shapes a chargeback leaves behind, and they come apart
/// in practice: a reversal that was not spent through freezes the account and
/// writes a `refund` row but leaves the balance at zero, while an account whose
/// freeze an operator already lifted can still owe money. Reporting the reasons
/// rather than just the membership is what lets an operator tell "needs a
/// decision about money" from "needs a decision about trust".
pub const REVIEW_TRIGGER_FROZEN: &str = "frozen";
pub const REVIEW_TRIGGER_NEGATIVE_BALANCE: &str = "negative_balance";
pub const REVIEW_TRIGGER_RECENT_REFUND: &str = "recent_refund";

/// One account awaiting dispute review.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ReviewQueueRow {
    pub user_id: Uuid,
    pub email: String,
    pub balance_usd: Decimal,
    /// The debt, positive, present only when the balance is below zero. This is
    /// the number `disputes resolve` acts on; a non-negative account has no
    /// receivable even when it is frozen.
    pub receivable_usd: Option<Decimal>,
    pub frozen: bool,
    pub frozen_at: Option<DateTime<Utc>>,
    pub frozen_reason: Option<String>,
    /// `refund` entries inside the window.
    pub recent_refunds: i64,
    /// Dollars reversed inside the window, reported positive — the ledger
    /// stores reversals as negative amounts.
    pub reversed_in_window_usd: Decimal,
    /// The dispute or charge ids anchoring this account's reversals, newest
    /// first. Every reversal ever recorded, not only the windowed ones: an
    /// operator looking a case up in the Stripe dashboard wants the account's
    /// whole reversal history, and the window only decides membership.
    pub stripe_object_ids: Vec<String>,
    /// The PaymentIntents those reversals took back.
    pub stripe_payment_intent_ids: Vec<String>,
    pub triggers: Vec<String>,
}

/// Every account that is frozen, owes money, or was reversed inside the
/// trailing window.
///
/// Read-only. The window bounds only the `recent_refund` trigger — a freeze and
/// a receivable are durable states that do not age out, and an account that
/// dropped off this list because its chargeback got old would be a receivable
/// nobody is ever shown again.
pub async fn review_queue(pool: &PgPool, days: i32) -> Result<Vec<ReviewQueueRow>, sqlx::Error> {
    if days <= 0 {
        return Err(sqlx::Error::Protocol(
            "review window must be a positive number of days".to_owned(),
        ));
    }
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Decimal,
            Option<DateTime<Utc>>,
            Option<String>,
            i64,
            Decimal,
            Vec<String>,
            Vec<String>,
        ),
    >(
        r#"
        WITH reversals AS (
            SELECT
                user_id,
                COUNT(*) FILTER (
                    WHERE created_at >= NOW() - MAKE_INTERVAL(days => $1)
                ) AS recent,
                COALESCE(SUM(-amount_usd) FILTER (
                    WHERE created_at >= NOW() - MAKE_INTERVAL(days => $1)
                ), 0) AS reversed,
                ARRAY_REMOVE(
                    ARRAY_AGG(stripe_session_id ORDER BY id DESC), NULL
                ) AS object_ids,
                ARRAY_REMOVE(
                    ARRAY_AGG(stripe_payment_intent_id ORDER BY id DESC), NULL
                ) AS intent_ids
            FROM credit_ledger
            WHERE entry_type = 'refund'
            GROUP BY user_id
        )
        SELECT
            users.id,
            users.email,
            users.credit_balance_usd,
            users.frozen_at,
            users.frozen_reason,
            COALESCE(reversals.recent, 0),
            COALESCE(reversals.reversed, 0),
            COALESCE(reversals.object_ids, ARRAY[]::TEXT[]),
            COALESCE(reversals.intent_ids, ARRAY[]::TEXT[])
        FROM users
        LEFT JOIN reversals ON reversals.user_id = users.id
        WHERE users.frozen_at IS NOT NULL
           OR users.credit_balance_usd < 0
           OR COALESCE(reversals.recent, 0) > 0
        ORDER BY users.credit_balance_usd ASC,
                 users.frozen_at ASC NULLS LAST,
                 users.email ASC
        "#,
    )
    .bind(days)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                user_id,
                email,
                balance_usd,
                frozen_at,
                frozen_reason,
                recent_refunds,
                reversed_in_window_usd,
                stripe_object_ids,
                stripe_payment_intent_ids,
            )| {
                let mut triggers = Vec::new();
                if frozen_at.is_some() {
                    triggers.push(REVIEW_TRIGGER_FROZEN.to_owned());
                }
                if balance_usd < Decimal::ZERO {
                    triggers.push(REVIEW_TRIGGER_NEGATIVE_BALANCE.to_owned());
                }
                if recent_refunds > 0 {
                    triggers.push(REVIEW_TRIGGER_RECENT_REFUND.to_owned());
                }
                ReviewQueueRow {
                    user_id,
                    email,
                    balance_usd,
                    receivable_usd: (balance_usd < Decimal::ZERO).then(|| -balance_usd),
                    frozen: frozen_at.is_some(),
                    frozen_at,
                    frozen_reason,
                    recent_refunds,
                    reversed_in_window_usd,
                    stripe_object_ids,
                    stripe_payment_intent_ids,
                    triggers,
                }
            },
        )
        .collect())
}

/// A ledger row with the Stripe anchors attached, for operator review only.
///
/// Deliberately NOT [`LedgerEntry`], which the customer portal serializes
/// straight to the browser (`portal.rs`, `portal/src/api.ts`). Stripe object
/// ids are internal reconciliation handles; widening the customer-facing struct
/// to carry them would leak them to every portal user. Two structs is the price
/// of that separation and it is worth paying.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LedgerEntryDetail {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub entry_type: String,
    pub amount_usd: Decimal,
    pub balance_after_usd: Decimal,
    pub note: Option<String>,
    pub stripe_session_id: Option<String>,
    pub stripe_payment_intent_id: Option<String>,
    pub request_id: Option<Uuid>,
}

/// The account's entire ledger, newest first, with Stripe anchors.
///
/// Unbounded on purpose: this is the "reviews logs" surface, and a review that
/// silently truncated the history would let the entry that explains the case
/// fall off the bottom. A disputed account's ledger is tens of rows.
pub async fn ledger_history(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<LedgerEntryDetail>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            DateTime<Utc>,
            String,
            Decimal,
            Decimal,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<Uuid>,
        ),
    >(
        r#"
        SELECT id, created_at, entry_type, amount_usd, balance_after_usd, note,
               stripe_session_id, stripe_payment_intent_id, request_id
        FROM credit_ledger
        WHERE user_id = $1
        ORDER BY created_at DESC, id DESC
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                created_at,
                entry_type,
                amount_usd,
                balance_after_usd,
                note,
                stripe_session_id,
                stripe_payment_intent_id,
                request_id,
            )| LedgerEntryDetail {
                id,
                created_at,
                entry_type,
                amount_usd,
                balance_after_usd,
                note,
                stripe_session_id,
                stripe_payment_intent_id,
                request_id,
            },
        )
        .collect())
}

/// What an account actually consumed, summarized.
#[derive(Clone, Debug, serde::Serialize)]
pub struct UsageSummary {
    pub requests: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    /// What the customer was billed, summed.
    pub spend_usd: Decimal,
    pub first_request_at: Option<DateTime<Utc>>,
    pub last_request_at: Option<DateTime<Utc>>,
}

/// Metered usage across every key the account has ever held, live or revoked.
///
/// Scoped by user rather than by key because a disputing account's keys are
/// routinely revoked before review, and usage attached to a revoked key is
/// still usage the customer received.
pub async fn usage_summary(pool: &PgPool, user_id: Uuid) -> Result<UsageSummary, sqlx::Error> {
    let (requests, input_tokens, cached_input_tokens, output_tokens, spend_usd, first, last) =
        sqlx::query_as::<
            _,
            (
                i64,
                i64,
                i64,
                i64,
                Decimal,
                Option<DateTime<Utc>>,
                Option<DateTime<Utc>>,
            ),
        >(
            r#"
        SELECT
            COUNT(*),
            COALESCE(SUM(usage_events.input_tokens), 0),
            COALESCE(SUM(usage_events.cached_input_tokens), 0),
            COALESCE(SUM(usage_events.output_tokens), 0),
            COALESCE(SUM(usage_events.cost_usd), 0),
            MIN(usage_events.ts),
            MAX(usage_events.ts)
        FROM usage_events
        JOIN api_keys ON api_keys.id = usage_events.api_key_id
        WHERE api_keys.user_id = $1
        "#,
        )
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(UsageSummary {
        requests,
        input_tokens,
        cached_input_tokens,
        output_tokens,
        spend_usd,
        first_request_at: first,
        last_request_at: last,
    })
}

/// What a resolution attempt did, or why it refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolutionOutcome {
    /// The receivable was forgiven and the balance is now exactly zero.
    WrittenOff {
        forgiven_usd: Decimal,
        balance_after: Decimal,
    },
    /// A write-off already settled this account and the balance is no longer
    /// negative. The idempotent no-op: no second ledger row.
    AlreadyWrittenOff { balance_usd: Decimal },
    Recovered {
        amount_usd: Decimal,
        balance_after: Decimal,
    },
    /// The account is not in a state this command may act on. Carries the
    /// operator-facing reason; `admin` turns it into a non-zero exit.
    Refused { reason: String },
}

/// Forgive a receivable: bring a negative balance up to exactly zero.
///
/// Built from the same parts as every other credit write — one transaction,
/// the per-user advisory lock that admission, settlement, `credit_purchase`
/// and `reverse_purchase` all serialize on, and a `credit_ledger` row
/// snapshotting the balance it produced.
///
/// # Why the amount is not a parameter
///
/// A write-off's amount is a FACT about the account, not an operator's choice:
/// it is whatever is owed at the moment the lock is held. Passing it in would
/// let a stale figure — read before a concurrent settlement moved the balance —
/// overshoot into positive credit, which is a customer being handed money
/// because an operator's terminal was out of date. Reading it under the lock
/// makes that unrepresentable.
///
/// # Idempotence
///
/// The intent is "leave this account owing nothing". Once that holds, a repeat
/// is [`ResolutionOutcome::AlreadyWrittenOff`] and writes nothing — the second
/// row would be indistinguishable from a real second forgiveness, and the
/// ledger is append-only, so there is no taking it back.
///
/// A non-negative account with no prior write-off is refused instead: there is
/// no receivable, so there is nothing to forgive, and the likeliest cause is
/// the wrong email.
///
/// # This does not unfreeze
///
/// Settling the money and trusting the account again are different decisions
/// belonging to different people at different times. The freeze columns are not
/// in this transaction's write set at all.
pub async fn write_off_receivable(
    pool: &PgPool,
    user_id: Uuid,
    note: &str,
) -> Result<ResolutionOutcome, sqlx::Error> {
    let note = note.trim();
    if note.is_empty() {
        return Err(sqlx::Error::Protocol(
            "a write-off must state its reason".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // NOTE the absence of `SET LOCAL zerorouter.credit_reversal`. A write-off
    // only ever moves the balance UP, so it has no business declaring itself to
    // the 0009 overdraft trigger, and the tripwire keeps its meaning precisely
    // because the paths that cannot overdraft never claim they might.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    // Read under the lock, after it is held: this is the authoritative balance
    // and no settlement can move it until this transaction commits.
    let balance = sqlx::query_scalar::<_, Decimal>(
        "SELECT credit_balance_usd FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| sqlx::Error::Protocol("write-off found no such user".to_owned()))?;

    if balance >= Decimal::ZERO {
        let settled = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM credit_ledger WHERE user_id = $1 AND entry_type = 'writeoff' LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&mut *transaction)
        .await?
        .is_some();
        transaction.rollback().await?;
        return Ok(if settled {
            ResolutionOutcome::AlreadyWrittenOff {
                balance_usd: balance,
            }
        } else {
            ResolutionOutcome::Refused {
                reason: format!(
                    "balance is {balance}, so there is no receivable to write off; \
                     a write-off only settles an account that owes money"
                ),
            }
        });
    }

    let forgiven = -balance;
    let balance_after = sqlx::query_scalar::<_, Decimal>(
        r#"
        UPDATE users
        SET credit_balance_usd = 0
        WHERE id = $1
        RETURNING credit_balance_usd
        "#,
    )
    .bind(user_id)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO credit_ledger (user_id, entry_type, amount_usd, balance_after_usd, note)
        VALUES ($1, 'writeoff', $2, $3, $4)
        "#,
    )
    .bind(user_id)
    .bind(forgiven)
    .bind(balance_after)
    .bind(note)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(ResolutionOutcome::WrittenOff {
        forgiven_usd: forgiven,
        balance_after,
    })
}

/// Record money that came back outside Stripe — a dispute won, a wire received.
///
/// Credits the balance by exactly `amount_usd`. It is NOT capped at the
/// outstanding receivable, and that is deliberate: a dispute ZeroRouter WINS
/// leaves the customer having genuinely paid, so restoring the credit the
/// reversal took back is the honest entry even when the reversal was never
/// spent through and the balance is sitting at zero.
///
/// The guard against that generality is the precondition, not a cap: the
/// account must already be in the review queue. On a healthy account this
/// refuses, because a credit to a healthy account is a promo grant and
/// `grant-credit` is the command that writes one.
///
/// # Idempotence
///
/// Weaker than the write-off's, honestly so. Two wires of the same size are two
/// recoveries, and nothing in a `--recover 50` invocation distinguishes "the
/// one I already recorded" from "another one". What bounds a repeat is the
/// precondition: once a recovery lifts the account out of the review queue,
/// the next `--recover` is refused. A still-frozen account stays reviewable by
/// design, so an operator settling a frozen account can record several
/// receipts — which is the real workflow.
///
/// # This does not unfreeze
///
/// Same reason as [`write_off_receivable`].
pub async fn record_recovery(
    pool: &PgPool,
    user_id: Uuid,
    amount_usd: Decimal,
    note: &str,
) -> Result<ResolutionOutcome, sqlx::Error> {
    let note = note.trim();
    if note.is_empty() {
        return Err(sqlx::Error::Protocol(
            "a recovery must state its reason".to_owned(),
        ));
    }
    if amount_usd <= Decimal::ZERO {
        return Err(sqlx::Error::Protocol(
            "recovery amount must be positive".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // As in `write_off_receivable`: a recovery only credits, so it never
    // declares itself to the 0009 overdraft trigger.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    // The reviewable-state precondition, evaluated under the lock against the
    // same three triggers `review_queue` reports. Recency is checked over ALL
    // reversals rather than a window: `disputes list --days` narrows what an
    // operator is shown, but an operator who has decided to record a receipt
    // against a 90-day-old chargeback should not be told the account is
    // healthy.
    let (balance, frozen, reversed) = sqlx::query_as::<_, (Decimal, bool, bool)>(
        r#"
        SELECT
            users.credit_balance_usd,
            users.frozen_at IS NOT NULL,
            EXISTS (
                SELECT 1 FROM credit_ledger
                WHERE credit_ledger.user_id = users.id
                  AND credit_ledger.entry_type = 'refund'
            )
        FROM users
        WHERE users.id = $1
        FOR UPDATE
        "#,
    )
    .bind(user_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| sqlx::Error::Protocol("recovery found no such user".to_owned()))?;

    if !frozen && !reversed && balance >= Decimal::ZERO {
        transaction.rollback().await?;
        return Ok(ResolutionOutcome::Refused {
            reason: format!(
                "account is not under review (balance {balance}, not frozen, no reversal on \
                 record), so there is nothing to recover; promotional credit is `grant-credit`"
            ),
        });
    }

    let balance_after = sqlx::query_scalar::<_, Decimal>(
        r#"
        UPDATE users
        SET credit_balance_usd = credit_balance_usd + $2
        WHERE id = $1
        RETURNING credit_balance_usd
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO credit_ledger (user_id, entry_type, amount_usd, balance_after_usd, note)
        VALUES ($1, 'recovery', $2, $3, $4)
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .bind(balance_after)
    .bind(note)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(ResolutionOutcome::Recovered {
        amount_usd,
        balance_after,
    })
}

// ---------------------------------------------------------------------------
// Autopay (migration 0008)
// ---------------------------------------------------------------------------

/// The account-state half of "may this account be auto-charged?", as one SQL
/// fragment (bare `users` column names, so it drops into any query that has the
/// row in scope). It is asserted at EVERY boundary that can reach an
/// off-session charge — `autopay_candidates`, `claim_autopay_attempt`,
/// `replay_charge`, and `autopay_still_armed` — so the invariant cannot be
/// stated once and silently dropped downstream (HIGH-2).
///
/// - `frozen_at IS NULL`: a frozen account is never charged (migration 0009).
///   The freeze exists to STOP the spend on a disputing customer; charging the
///   saved card of a customer fresh off a dispute is how the next dispute is
///   manufactured.
/// - `credit_balance_usd >= 0`: nor is an account that OWES money. Since
///   0009/0013 a negative balance means a reversal receivable — money already
///   clawed back through Stripe — and such an account is maximally eligible on
///   every other predicate (its balance is furthest below the threshold), so
///   re-entry into autopay must be a deliberate operator decision (`admin
///   disputes resolve`), never a side effect of a sweep.
/// - `autopay_consecutive_failures < 3`: a dead card gets three attempts, not a
///   retry loop.
///
/// Each site combines this with its own flag/amount/threshold checks. The
/// freeze webhook and a balance reversal can commit AFTER selection but before
/// the charge, so re-reading this fragment at the later boundaries — above all
/// immediately before the POST in `replay_charge` — is what closes the
/// freeze-vs-charge race (both the live sweep and the reconciliation replay).
pub const AUTOPAY_ELIGIBILITY_PREDICATE: &str =
    "frozen_at IS NULL AND credit_balance_usd >= 0 AND autopay_consecutive_failures < 3";

/// A user the autopay sweep should recharge: enabled, configured, under
/// threshold, not mid-charge, and not disabled by consecutive failures.
#[derive(Clone, Debug)]
pub struct AutopayCandidate {
    pub user_id: Uuid,
    pub stripe_customer_id: String,
    pub topup_usd: Decimal,
}

/// The sweep's worklist. Every predicate is in SQL so a candidate observed
/// here is consistent at read time; the one-pending-per-user partial unique
/// index is what makes racing sweeps safe rather than this SELECT.
pub async fn autopay_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<AutopayCandidate>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, String, Decimal)>(&format!(
        r#"
        SELECT u.id, u.stripe_customer_id, u.autopay_topup_usd
        FROM users u
        WHERE u.autopay_enabled
          AND u.credit_balance_usd < u.autopay_threshold_usd
          AND u.stripe_customer_id IS NOT NULL
          -- The account-state eligibility set — frozen / receivable / failure
          -- cap — as ONE shared fragment, asserted identically at the claim,
          -- replay, and still-armed boundaries so a later stage cannot drop it
          -- (HIGH-2). See AUTOPAY_ELIGIBILITY_PREDICATE for why each of the
          -- three disqualifies an account from an off-session charge.
          AND ({AUTOPAY_ELIGIBILITY_PREDICATE})
          AND NOT EXISTS (
              SELECT 1 FROM stripe_autopay_intents i
              WHERE i.user_id = u.id AND i.status = 'pending'
          )
        ORDER BY u.credit_balance_usd ASC
        LIMIT $1
        "#
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(user_id, stripe_customer_id, topup_usd)| AutopayCandidate {
                user_id,
                stripe_customer_id,
                topup_usd,
            },
        )
        .collect())
}

/// Claim a user for one charge attempt BEFORE any money moves, by
/// inserting the pending row under a local id carrying the Stripe
/// idempotency key (`local_<key>`). The one-pending-per-user partial
/// unique index makes the claim exclusive: a racing sweep's claim fails
/// and that sweep skips the user, so two sweeps can never charge the same
/// user twice — and because the idempotency key survives in the row, a
/// lost Stripe response is retried with the SAME key and lands on the same
/// PaymentIntent rather than a second charge.
pub async fn claim_autopay_attempt(
    pool: &PgPool,
    user_id: Uuid,
    amount_usd: Decimal,
    charge_amount_usd: Decimal,
    idempotency_key: &str,
) -> Result<bool, sqlx::Error> {
    // The candidate list is an unlocked snapshot taken before this call, so
    // the user may have turned autopay off — and had that request return
    // successfully — between the SELECT and here. Re-reading the flag inside
    // the claim closes that window: a disable that commits first makes the
    // INSERT ... SELECT match nothing, and no charge follows (sol review).
    // The amount is re-read from the row for the same reason, so a topup the
    // user lowered cannot be charged at its old size. `amount_usd` is the NET
    // credit re-read from the row; `charge_amount_usd` is the GROSS the caller
    // priced from that same net topup.
    let claimed = sqlx::query(&format!(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        SELECT $1, id, $3, $4
        FROM users
        WHERE id = $2
          AND autopay_enabled
          AND autopay_topup_usd = $3
          -- The whole eligibility test, not just the flag: a manual credit
          -- or a raised threshold between selection and claim must stop the
          -- charge too (sol review).
          AND autopay_threshold_usd IS NOT NULL
          AND credit_balance_usd < autopay_threshold_usd
          -- HIGH-2: re-assert the SAME frozen / receivable / failure-cap set
          -- autopay_candidates selected on. A dispute freeze and a balance
          -- reversal can commit between selection and this claim; without this
          -- the INSERT still matches (a negative balance is only FURTHER below
          -- the threshold) and the disputing customer is charged.
          AND ({AUTOPAY_ELIGIBILITY_PREDICATE})
        ON CONFLICT DO NOTHING
        "#
    ))
    .bind(format!("local_{idempotency_key}"))
    .bind(user_id)
    .bind(amount_usd)
    .bind(charge_amount_usd)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(claimed > 0)
}

/// Whether a user is still eligible for autopay, read at the moment of use.
///
/// The reconciliation pass replays a stranded local claim under its
/// original idempotency key up to ~20 hours later; by then the user may
/// have opted out, or — the HIGH-2 case — been frozen and driven into a
/// receivable by a dispute. Replaying either charges someone who must not be
/// charged, so this is the full eligibility gate, not just the enabled flag:
/// enabled AND the shared frozen / receivable / failure-cap predicate. A
/// `false` result routes the reconciliation pass to KEEP (not delete) the
/// claim, which is correct for a stranded charge that may already have
/// happened.
pub async fn autopay_still_armed(pool: &PgPool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(&format!(
        "SELECT autopay_enabled AND ({AUTOPAY_ELIGIBILITY_PREDICATE}) FROM users WHERE id = $1"
    ))
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map(|armed| armed.unwrap_or(false))
}

/// Attach the real PaymentIntent id to a claim once Stripe has answered.
/// If the webhook's recovery path already inserted the real id (it raced
/// the rename), the local claim is dropped in its favor.
pub async fn attach_autopay_intent(
    pool: &PgPool,
    idempotency_key: &str,
    payment_intent_id: &str,
) -> Result<(), sqlx::Error> {
    let renamed = sqlx::query(
        r#"
        UPDATE stripe_autopay_intents
        SET payment_intent_id = $2, updated_at = NOW()
        WHERE payment_intent_id = $1
          AND NOT EXISTS (
              SELECT 1 FROM stripe_autopay_intents WHERE payment_intent_id = $2
          )
        "#,
    )
    .bind(format!("local_{idempotency_key}"))
    .bind(payment_intent_id)
    .execute(pool)
    .await?
    .rows_affected();
    if renamed == 0 {
        sqlx::query("DELETE FROM stripe_autopay_intents WHERE payment_intent_id = $1")
            .bind(format!("local_{idempotency_key}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Pending intents older than the cutoff, for the sweep's reconciliation
/// pass: local claims whose Stripe response was lost (retried by
/// idempotency key) and real intents whose terminal webhook never arrived
/// (queried by id). Without this, one lost webhook wedges a user's
/// one-pending-per-user slot forever — the review's finding.
pub async fn stale_autopay_intents(
    pool: &PgPool,
    older_than_minutes: i32,
    replayable_within_minutes: i32,
) -> Result<Vec<(String, Uuid, Decimal, Decimal)>, sqlx::Error> {
    // Bounded at BOTH ends. The lower bound is the reconciliation delay; the
    // upper bound is Stripe's idempotency-key retention. Replaying past that
    // window stops being a replay and becomes a second charge, because
    // Stripe may have pruned the key and will treat the request as new (sol
    // review). Rows past it are surfaced by `overdue_autopay_intents`
    // instead of being retried forever. Returns (intent, user, NET credit,
    // GROSS charge) so reconciliation settles the net and re-corroborates the
    // gross.
    sqlx::query_as::<_, (String, Uuid, Decimal, Decimal)>(
        r#"
        SELECT payment_intent_id, user_id, amount_usd, charge_amount_usd
        FROM stripe_autopay_intents
        WHERE status = 'pending'
          AND created_at < NOW() - ($1 * INTERVAL '1 minute')
          AND created_at >= NOW() - ($2 * INTERVAL '1 minute')
        ORDER BY created_at
        "#,
    )
    .bind(older_than_minutes)
    .bind(replayable_within_minutes)
    .fetch_all(pool)
    .await
}

/// Pending claims too old to replay safely. These are money in an unknown
/// state that no automation may touch: an operator has to ask Stripe what
/// happened and settle or release them deliberately.
pub async fn overdue_autopay_intents(
    pool: &PgPool,
    older_than_minutes: i32,
) -> Result<Vec<(String, Uuid, Decimal)>, sqlx::Error> {
    sqlx::query_as::<_, (String, Uuid, Decimal)>(
        r#"
        SELECT payment_intent_id, user_id, amount_usd
        FROM stripe_autopay_intents
        WHERE status = 'pending'
          AND created_at < NOW() - ($1 * INTERVAL '1 minute')
        ORDER BY created_at
        "#,
    )
    .bind(older_than_minutes)
    .fetch_all(pool)
    .await
}

/// Autopay charges collected at Stripe whose credit was WITHHELD at settlement
/// because the account had become ineligible in the charge's send window (FIX
/// 1). Each row is money taken from a card that must be REFUNDED — never
/// credited to a frozen / indebted account — so it is surfaced here for an
/// operator to refund out of band, the same shape as `overdue_autopay_intents`
/// surfaces claims automation must not touch. Returns (payment_intent_id,
/// user_id, refundable): the amount to refund.
///
/// The refundable amount is `charge_amount_usd + tax_amount_usd`, NOT the
/// ex-tax gross alone (migration 0021). Everywhere else in this module the
/// ex-tax gross is the right figure, because it is what the ledger and the fee
/// arithmetic are denominated in — but this one caller is answering a different
/// question: *how much money left the customer's card?* With tax that is the
/// taxed total, and refunding only the ex-tax gross would short the customer by
/// exactly the tax. `COALESCE` because the column is NULL both for pre-0021
/// rows and for a charge that took the untaxed fallback.
pub async fn withheld_autopay_intents(
    pool: &PgPool,
) -> Result<Vec<(String, Uuid, Decimal)>, sqlx::Error> {
    sqlx::query_as::<_, (String, Uuid, Decimal)>(
        r#"
        SELECT payment_intent_id, user_id,
               charge_amount_usd + COALESCE(tax_amount_usd, 0)
        FROM stripe_autopay_intents
        WHERE status = 'withheld'
        ORDER BY updated_at
        "#,
    )
    .fetch_all(pool)
    .await
}

/// The tax figures frozen on an autopay claim: the tax collected on top of the
/// ex-tax gross, and the Stripe Tax Calculation that priced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutopayTax {
    pub tax_usd: Decimal,
    pub calculation_id: Option<String>,
}

/// Read the tax already frozen on a claim, if any.
///
/// `None` means no tax has been computed for this claim yet, so the caller must
/// price one. `Some` — including `Some` with a zero amount — means a previous
/// attempt already froze an answer and the caller MUST reuse it rather than
/// asking Stripe again: a reconciliation replay re-POSTs under the original
/// idempotency key, and Stripe rejects a replay whose parameters differ from
/// the first request's (see migration 0021).
pub async fn autopay_tax(
    pool: &PgPool,
    payment_intent_id: &str,
) -> Result<Option<AutopayTax>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Option<Decimal>, Option<String>)>(
        "SELECT tax_amount_usd, tax_calculation_id FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(payment_intent_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(tax_usd, calculation_id)| {
        tax_usd.map(|tax_usd| AutopayTax {
            tax_usd,
            calculation_id,
        })
    }))
}

/// Freeze a computed tax onto a claim, first writer winning, and return the
/// figures that are now authoritative.
///
/// This is deliberately ONE statement rather than a read followed by a write.
/// Two sweeps — a live pass on one instance and a reconciliation replay on
/// another — can price the same stranded claim concurrently, and they can get
/// different answers (a rate change, or one of them falling back to untaxed
/// because the Tax API was briefly unavailable). If both then charged their own
/// figure under the same idempotency key, the second would be rejected by
/// Stripe as a parameter mismatch and the claim terminal-failed even though the
/// first may already have taken the money.
///
/// `COALESCE(tax_amount_usd, $2)` makes the first writer's answer stick and
/// hands every later caller that same answer in the same round trip, so both
/// racers POST an identical `amount`. The loser's calculation is simply
/// abandoned, which costs nothing: a Tax Calculation is only a quote until a
/// transaction is created from it.
///
/// The amount and the calculation id are frozen as an ATOMIC PAIR, which is why
/// the id is not a second `COALESCE`. Consider a first writer that fell back to
/// untaxed (tax 0, no calculation) and a second that priced 2.50 against a real
/// calculation: two independent `COALESCE`s would keep the first writer's 0 and
/// adopt the SECOND writer's calculation id, leaving the row claiming that a
/// calculation for 2.50 of tax priced a charge that collected none. Recording a
/// tax transaction from that calculation would report tax to a jurisdiction
/// that was never collected from the customer. The `CASE` keys both columns off
/// the same pre-update `tax_amount_usd IS NULL` test — every `SET` expression in
/// one `UPDATE` sees the OLD row — so the pair moves together or not at all.
///
/// Scoped to `status = 'pending'` so a settled or failed row is never rewritten.
/// Returns `None` if there is no such pending row.
pub async fn freeze_autopay_tax(
    pool: &PgPool,
    payment_intent_id: &str,
    tax_usd: Decimal,
    calculation_id: Option<&str>,
) -> Result<Option<AutopayTax>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Option<Decimal>, Option<String>)>(
        r#"
        UPDATE stripe_autopay_intents
        SET tax_amount_usd = COALESCE(tax_amount_usd, $2),
            tax_calculation_id = CASE
                WHEN tax_amount_usd IS NULL THEN $3
                ELSE tax_calculation_id
            END,
            updated_at = NOW()
        WHERE payment_intent_id = $1 AND status = 'pending'
        RETURNING tax_amount_usd, tax_calculation_id
        "#,
    )
    .bind(payment_intent_id)
    .bind(tax_usd)
    .bind(calculation_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(tax_usd, calculation_id)| {
        tax_usd.map(|tax_usd| AutopayTax {
            tax_usd,
            calculation_id,
        })
    }))
}

/// Remember that a claim's tax transaction is recorded at Stripe, and under
/// which id (migration 0024).
///
/// First writer wins (`tax_recorded_at IS NULL`): the inline record after a
/// credited settle and the sweep's retry pass can both confirm the same
/// recording — the reference is the PaymentIntent id, so Stripe returns the
/// same transaction either way — and the second confirmation must not
/// overwrite the stamp or the id the first one stored.
///
/// The id is the half that matters later: `create_reversal` takes only a
/// transaction ID, and the Tax API has no lookup by reference, so a row whose
/// id was lost (the recording POST succeeded but its response did not come
/// back) can never be reversed automatically. Such a row stays
/// `tax_recorded_at IS NULL` and the sweep retries it; the retry is refused as
/// a duplicate reference and logged until an operator resolves it from the
/// dashboard.
pub async fn freeze_autopay_tax_transaction(
    pool: &PgPool,
    payment_intent_id: &str,
    tax_transaction_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE stripe_autopay_intents
        SET tax_transaction_id = $2,
            tax_recorded_at = NOW(),
            updated_at = NOW()
        WHERE payment_intent_id = $1 AND tax_recorded_at IS NULL
        "#,
    )
    .bind(payment_intent_id)
    .bind(tax_transaction_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Credited claims whose priced tax has no confirmed tax transaction yet:
/// each is a sale COLLECTED from a card but missing from the Stripe Tax
/// filing report. Returns (payment_intent_id, tax_calculation_id) for the
/// sweep to re-record; the endpoint deduplicates on the intent-id reference,
/// so a retry can never double-report.
///
/// Scoped to `status = 'succeeded'` on purpose: `withheld` rows deliberately
/// record no tax (their money is queued for refund), `pending` rows have not
/// been collected, and `failed` rows never will be.
pub async fn unrecorded_autopay_tax(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT payment_intent_id, tax_calculation_id
        FROM stripe_autopay_intents
        WHERE status = 'succeeded'
          AND tax_calculation_id IS NOT NULL
          AND tax_recorded_at IS NULL
        ORDER BY updated_at
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Recorded tax transactions whose underlying credit the ledger shows
/// reversed — a `refund` row naming the intent, written by `reverse_purchase`
/// or `apply_observed_reversals` — and whose tax has not been reversed yet.
/// Each is tax standing in the filing report for money that went back to the
/// customer. Returns (payment_intent_id, tax_transaction_id) for the sweep to
/// reverse in full.
///
/// Detection is BY THE LEDGER, not by webhook bookkeeping, which is what makes
/// the pass order-independent: whether the refund arrived before or after the
/// credit (the tombstone path), and whether the tax transaction was recorded
/// inline or by a later sweep retry, the reversal is due exactly when both a
/// refund row and a recorded transaction exist. Zero-tax transactions are
/// reversed on the same terms as taxed ones — they evidence per-jurisdiction
/// sales volume, and a refunded sale's evidence should be withdrawn too.
pub async fn unreversed_autopay_tax(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT i.payment_intent_id, i.tax_transaction_id
        FROM stripe_autopay_intents i
        WHERE i.tax_recorded_at IS NOT NULL
          AND i.tax_transaction_id IS NOT NULL
          AND i.tax_reversed_at IS NULL
          AND EXISTS (
              SELECT 1 FROM credit_ledger l
              WHERE l.entry_type = 'refund'
                AND l.stripe_payment_intent_id = i.payment_intent_id
          )
        ORDER BY i.updated_at
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Stamp a claim's tax reversal as recorded at Stripe.
///
/// First writer wins, mirroring [`freeze_autopay_tax_transaction`]: the stamp
/// is what stops the sweep re-reversing, so it is only written once and only
/// by a caller that saw Stripe accept the reversal.
pub async fn mark_autopay_tax_reversed(
    pool: &PgPool,
    payment_intent_id: &str,
    reversal_transaction_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE stripe_autopay_intents
        SET tax_reversal_transaction_id = $2,
            tax_reversed_at = NOW(),
            updated_at = NOW()
        WHERE payment_intent_id = $1 AND tax_reversed_at IS NULL
        "#,
    )
    .bind(payment_intent_id)
    .bind(reversal_transaction_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Reversed credits whose tax transaction is confirmed recorded but whose id
/// is unknown (the migration 0024 backfill, or a recording confirmed via a
/// duplicate-reference refusal). Automation cannot reverse these —
/// `create_reversal` needs the transaction ID — so they are surfaced loudly
/// for an operator to reverse in Dashboard → Tax → Transactions, the same
/// per-sweep pattern `withheld_autopay_intents` uses.
pub async fn unreversible_autopay_tax(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT i.payment_intent_id
        FROM stripe_autopay_intents i
        WHERE i.tax_recorded_at IS NOT NULL
          AND i.tax_transaction_id IS NULL
          AND i.tax_reversed_at IS NULL
          AND EXISTS (
              SELECT 1 FROM credit_ledger l
              WHERE l.entry_type = 'refund'
                AND l.stripe_payment_intent_id = i.payment_intent_id
          )
        ORDER BY i.updated_at
        "#,
    )
    .fetch_all(pool)
    .await
}

/// What settling an autopay charge did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutopayOutcome {
    Credited,
    AlreadySettled,
    /// No intent row and no metadata to recover one from — acknowledged and
    /// ignored (some other system's payment intent).
    Unknown,
    /// The charge succeeded at Stripe, but the account had become INELIGIBLE
    /// (frozen / indebted / max-failed) between the pre-POST guard and this
    /// settlement, so the credit is WITHHELD (FIX 1): no balance credit and no
    /// `autopay` ledger row. The intent moves to the durable `withheld` state —
    /// money collected at Stripe that must be refunded — surfaced for an
    /// operator (`withheld_autopay_intents`) rather than credited to an account
    /// that must not be charged.
    Withheld,
}

/// Credit a succeeded off-session charge, exactly once.
///
/// The pending→succeeded transition on the intents row is the guard; the
/// credit itself mirrors `credit_purchase` (user advisory lock, balance
/// update, `autopay` ledger row naming the payment intent). When the sweep
/// crashed between creating the PaymentIntent at Stripe and recording it,
/// the webhook passes `recovered` metadata and the row is inserted here —
/// money taken from a card can never fail to become credits for lack of a
/// bookkeeping row.
pub async fn settle_autopay_intent(
    pool: &PgPool,
    payment_intent_id: &str,
    recovered: Option<(Uuid, Decimal, Decimal)>,
) -> Result<AutopayOutcome, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;

    if let Some((user_id, amount_usd, charge_amount_usd)) = recovered {
        sqlx::query(
            r#"
            INSERT INTO stripe_autopay_intents
                (payment_intent_id, user_id, amount_usd, charge_amount_usd)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (payment_intent_id) DO NOTHING
            "#,
        )
        .bind(payment_intent_id)
        .bind(user_id)
        .bind(amount_usd)
        .bind(charge_amount_usd)
        .execute(&mut *transaction)
        .await?;
        // The stored row carries the NET credit that will be applied
        // (amount_usd) and the GROSS Stripe was to collect (charge_amount_usd).
        // The webhook corroborated the gross against amount_received and passes
        // both here; if the stored row disagrees on EITHER, refuse to credit
        // rather than pick a side — a mismatch is either a partial capture or
        // tampering, and both deserve eyes, not money movement (review finding:
        // the stored $100 must not be credited on a $1 collection).
        let stored = sqlx::query_as::<_, (Uuid, Decimal, Decimal)>(
            "SELECT user_id, amount_usd, charge_amount_usd FROM stripe_autopay_intents WHERE payment_intent_id = $1",
        )
        .bind(payment_intent_id)
        .fetch_one(&mut *transaction)
        .await?;
        if stored != (user_id, amount_usd, charge_amount_usd) {
            sqlx::query(
                r#"
                UPDATE stripe_autopay_intents
                SET status = 'failed', updated_at = NOW()
                WHERE payment_intent_id = $1 AND status = 'pending'
                "#,
            )
            .bind(payment_intent_id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Err(sqlx::Error::Protocol(format!(
                "autopay intent {payment_intent_id} stored amount disagrees with the                  corroborated collection; refusing to credit"
            )));
        }
    }

    let Some((user_id, amount_usd)) = sqlx::query_as::<_, (Uuid, Decimal)>(
        r#"
        UPDATE stripe_autopay_intents
        SET status = 'succeeded', updated_at = NOW()
        WHERE payment_intent_id = $1 AND status = 'pending'
        RETURNING user_id, amount_usd
        "#,
    )
    .bind(payment_intent_id)
    .fetch_optional(&mut *transaction)
    .await?
    else {
        let known = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM stripe_autopay_intents WHERE payment_intent_id = $1",
        )
        .bind(payment_intent_id)
        .fetch_optional(&mut *transaction)
        .await?
        .is_some();
        transaction.rollback().await?;
        return Ok(if known {
            AutopayOutcome::AlreadySettled
        } else {
            AutopayOutcome::Unknown
        });
    };

    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;
    // FIX A (HIGH-1 round 2): the intent lock, AFTER the user lock, so this
    // recharge serializes with a reversal for the same PaymentIntent that has no
    // user to lock on. The autopay intent id is always present here.
    lock_payment_intent(&mut transaction, payment_intent_id).await?;

    // FIX 1' (settlement-side re-check, round 4): the eligibility check and the
    // credit are now ONE conditional statement, not a separate `SELECT
    // (eligibility)` followed by an unconditional `UPDATE users SET
    // credit_balance_usd = …`. A dispute-freeze on an OLDER intent — and the
    // balance reversal that drives the account into a receivable — can commit
    // during the charge's `send().await` window (pool acquire + DNS/TCP/TLS +
    // transmit, up to the 15s HTTP timeout), AFTER the pre-POST guard passed AND
    // after this settlement transaction has already begun and taken its locks;
    // that freeze belongs to a DIFFERENT intent than this new autopay charge, so
    // nothing else catches it. The charge itself is best-effort and a card may
    // rarely be charged in that race, but the CREDIT must never land on a frozen /
    // indebted account — the exact catastrophe the freeze exists to prevent.
    //
    // A separate SELECT-then-UPDATE is a TOCTOU: under READ COMMITTED the credit
    // UPDATE is a new statement that DOES see a freeze committed after the SELECT,
    // but with no predicate it credits anyway. Folding the SAME eligibility
    // predicate into the UPDATE's WHERE closes that intra-transaction window with
    // no reliance on the freezer taking a lock: Postgres locks the users row, and
    // when a freeze committed underneath it re-evaluates the predicate against the
    // freshly-committed row version (EvalPlanQual), so an account that turned
    // ineligible anywhere inside the window matches ZERO rows. This is the exact
    // pattern the revoked-key admission fix uses (`db.rs`, `UPDATE … WHERE NOT
    // disabled`). It is deliberately NOT "make freeze_account take the advisory
    // lock": the conditional update is the stronger, LOCAL guarantee — it does not
    // depend on every present and future writer of `frozen_at` remembering to lock.
    //
    // Zero rows returned == ineligible == WITHHOLD: no balance credit, no `autopay`
    // ledger row, and no observed-reversal application; the intent moves to the
    // durable `withheld` state instead of `succeeded`. The money WAS collected at
    // Stripe, so it must be refunded — surfaced to an operator via
    // `withheld_autopay_intents`, NEVER silently kept and NEVER credited. The
    // Stripe refund is deliberately NOT issued inline: that would hold this
    // advisory lock across an external HTTP call, the very anti-pattern we avoid;
    // an operator / out-of-lock refund path settles the actual refund. Idempotent
    // by the same pending→terminal guard as crediting: that transition already ran
    // exactly once above, so a redelivered success finds no pending row and returns
    // AlreadySettled — it neither double-withholds nor double-credits.
    // The charge-time predicate is the account-state fragment AND `autopay_enabled`:
    // an opt-out that commits during the charge window (the portal's off switch)
    // must WITHHOLD the credit exactly like a freeze, never credit an account that
    // told us to stop. This mirrors the `autopay_enabled AND (…)` gate the claim,
    // replay, and still-armed boundaries apply; folding it into the conditional
    // UPDATE lets EvalPlanQual re-check it against a freshly-committed opt-out too.
    // The current-threshold / top-up-amount checks are NOT re-asserted here: they
    // are enforced at `claim_autopay_attempt` (the row's amount/threshold at claim
    // time is what was charged), and re-reading them at settlement would wrongly
    // withhold a legitimately-collected credit when a concurrent top-up merely
    // lifted the balance back above threshold.
    let balance_after = sqlx::query_scalar::<_, Decimal>(&format!(
        r#"
        UPDATE users
        SET credit_balance_usd = credit_balance_usd + $2,
            autopay_consecutive_failures = 0
        WHERE id = $1 AND autopay_enabled AND ({AUTOPAY_ELIGIBILITY_PREDICATE})
        RETURNING credit_balance_usd
        "#
    ))
    .bind(user_id)
    .bind(amount_usd)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(balance_after) = balance_after else {
        sqlx::query(
            r#"
            UPDATE stripe_autopay_intents
            SET status = 'withheld', updated_at = NOW()
            WHERE payment_intent_id = $1
            "#,
        )
        .bind(payment_intent_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        tracing::error!(
            %user_id,
            payment_intent = %payment_intent_id,
            "autopay charge collected at Stripe but the account is ineligible (frozen / indebted / max-failed) at settlement; credit WITHHELD and the charge marked needs-refund — an operator must refund it out of band"
        );
        return Ok(AutopayOutcome::Withheld);
    };
    sqlx::query(
        r#"
        INSERT INTO credit_ledger
            (user_id, entry_type, amount_usd, balance_after_usd, stripe_session_id, note)
        VALUES ($1, 'autopay', $2, $3, $4, 'autopay recharge')
        "#,
    )
    .bind(user_id)
    .bind(amount_usd)
    .bind(balance_after)
    .bind(payment_intent_id)
    .execute(&mut *transaction)
    .await?;
    // HIGH-1 (migration 0017): an autopay recharge is credited by exactly the
    // same PaymentIntent id a reversal would name, so a refund/dispute observed
    // before this credit landed is consumed here — under the advisory lock this
    // transaction already holds — reversing the credit and freezing on a
    // dispute, so an off-session recharge can never leave spendable reversed
    // money either.
    apply_observed_reversals(&mut transaction, user_id, payment_intent_id, amount_usd).await?;
    transaction.commit().await?;
    Ok(AutopayOutcome::Credited)
}

/// Record a failed off-session charge: the intent goes terminal and the
/// user's consecutive-failure count rises; the sweep's candidate query
/// stops at three, so a dead card gets three attempts, not a retry loop.
pub async fn fail_autopay_intent(
    pool: &PgPool,
    payment_intent_id: &str,
) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let Some(user_id) = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE stripe_autopay_intents
        SET status = 'failed', updated_at = NOW()
        WHERE payment_intent_id = $1 AND status = 'pending'
        RETURNING user_id
        "#,
    )
    .bind(payment_intent_id)
    .fetch_optional(&mut *transaction)
    .await?
    else {
        transaction.rollback().await?;
        return Ok(false);
    };
    sqlx::query(
        r#"
        UPDATE users
        SET autopay_consecutive_failures = autopay_consecutive_failures + 1,
            autopay_enabled = autopay_enabled AND autopay_consecutive_failures + 1 < 3
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(true)
}

/// Count a charge attempt that failed before a PaymentIntent existed (no
/// saved card, malformed amount): the same three-strikes ledger as a
/// declined intent, without an intent row to anchor it.
pub async fn bump_autopay_failure(pool: &PgPool, user_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE users
        SET autopay_consecutive_failures = autopay_consecutive_failures + 1,
            autopay_enabled = autopay_enabled AND autopay_consecutive_failures + 1 < 3
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a terminal-bound charge record recovered from a webhook (used by
/// the failed arm when the sweep-side record was lost). Idempotent on the
/// intent id; the one-pending-per-user index may reject it while a local
/// claim still holds the slot, in which case reconciliation resolves the
/// claim first and the webhook retry lands cleanly.
pub async fn record_autopay_charge(
    pool: &PgPool,
    payment_intent_id: &str,
    user_id: Uuid,
    amount_usd: Decimal,
    charge_amount_usd: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(payment_intent_id)
    .bind(user_id)
    .bind(amount_usd)
    .bind(charge_amount_usd)
    .execute(pool)
    .await?;
    Ok(())
}

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
    /// The `unit_amount` quoted to Stripe, in the smallest currency unit
    /// (cents for USD), for comparison against the event's `amount_total`.
    pub expected_amount_cents: i64,
    /// The decimal-dollar credit this session buys. Equal to
    /// `expected_amount_cents / 100` by a database CHECK.
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
// Autopay (migration 0008)
// ---------------------------------------------------------------------------

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
    let rows = sqlx::query_as::<_, (Uuid, String, Decimal)>(
        r#"
        SELECT u.id, u.stripe_customer_id, u.autopay_topup_usd
        FROM users u
        WHERE u.autopay_enabled
          AND u.autopay_consecutive_failures < 3
          AND u.credit_balance_usd < u.autopay_threshold_usd
          AND u.stripe_customer_id IS NOT NULL
          AND NOT EXISTS (
              SELECT 1 FROM stripe_autopay_intents i
              WHERE i.user_id = u.id AND i.status = 'pending'
          )
        ORDER BY u.credit_balance_usd ASC
        LIMIT $1
        "#,
    )
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

/// Record a freshly created PaymentIntent. `ON CONFLICT DO NOTHING`: the
/// webhook's metadata-recovery path may have raced us and inserted first,
/// and the PK is the exactly-once guard either way.
pub async fn record_autopay_intent(
    pool: &PgPool,
    payment_intent_id: &str,
    user_id: Uuid,
    amount_usd: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO stripe_autopay_intents (payment_intent_id, user_id, amount_usd)
        VALUES ($1, $2, $3)
        ON CONFLICT (payment_intent_id) DO NOTHING
        "#,
    )
    .bind(payment_intent_id)
    .bind(user_id)
    .bind(amount_usd)
    .execute(pool)
    .await?;
    Ok(())
}

/// What settling an autopay charge did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutopayOutcome {
    Credited,
    AlreadySettled,
    /// No intent row and no metadata to recover one from — acknowledged and
    /// ignored (some other system's payment intent).
    Unknown,
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
    recovered: Option<(Uuid, Decimal)>,
) -> Result<AutopayOutcome, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;

    if let Some((user_id, amount_usd)) = recovered {
        sqlx::query(
            r#"
            INSERT INTO stripe_autopay_intents (payment_intent_id, user_id, amount_usd)
            VALUES ($1, $2, $3)
            ON CONFLICT (payment_intent_id) DO NOTHING
            "#,
        )
        .bind(payment_intent_id)
        .bind(user_id)
        .bind(amount_usd)
        .execute(&mut *transaction)
        .await?;
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
    let balance_after = sqlx::query_scalar::<_, Decimal>(
        r#"
        UPDATE users
        SET credit_balance_usd = credit_balance_usd + $2,
            autopay_consecutive_failures = 0
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

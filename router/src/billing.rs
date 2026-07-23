//! Prepaid credit accounting: balance reads and the append-only ledger.
//!
//! Every balance mutation happens inside a transaction that (a) holds the
//! per-user advisory lock used by admission and settlement in [`crate::db`],
//! and (b) appends a `credit_ledger` row snapshotting `balance_after_usd`.
//! Purchases are idempotent by `stripe_session_id`; a replayed webhook is a
//! no-op reported as [`CreditOutcome::AlreadyApplied`].

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::sqlx::{self, PgPool};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreditOutcome {
    Applied { balance_after: Decimal },
    AlreadyApplied,
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

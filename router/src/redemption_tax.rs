//! Redemption-time sales tax: the dormant half of the stored-value
//! determination.
//!
//! # Why this exists
//!
//! Whether tax on prepaid credits is due when they are BOUGHT or when they
//! are SPENT is unsettled — see `# Sales tax` in [`crate::stripe`] for the
//! authorities pointing each way. The purchase-time answer is what runs
//! today: checkout and autopay price tax through Stripe and redemption is a
//! bare metered debit. If the operator's accountant or a DOR letter ruling
//! lands on the stored-value reading instead (credits are "rights and
//! credits", excluded at sale, taxable at redemption), the dashboard side of
//! that flip is a dropdown — select the multi-purpose stored-value tax code
//! and every purchase surface starts pricing zero — but the redemption side
//! previously did not exist, so the flip was in practice a decision to
//! collect nothing anywhere. This module is the redemption side.
//!
//! It ships OFF. [`REDEMPTION_TAX_ENV`] gates it three ways: `off` (default
//! — nothing runs, nothing changes), `dry_run` (periods are built and priced
//! so the operator can see what WOULD be collected; no balance is touched
//! and nothing reaches Stripe's filing reports), and `collect` (the full
//! lifecycle). Purchase-time and redemption-time taxation must never both be
//! live — that taxes the same dollar twice — and the code cannot see the
//! dashboard's tax code to enforce it, so the flip procedure in DEPLOY.md is
//! the contract: change the dashboard code and set `collect` together.
//!
//! # The shape: aggregate, then tax
//!
//! Redemption is thousands of sub-cent settlement debits on a hot,
//! advisory-locked money path; a Tax API call per debit would put network
//! latency and per-calculation cost inside it. So the request path is
//! untouched, byte for byte. A background sweep tiles each user's `usage`
//! ledger rows into PERIODS — spans `(from_ledger_id, through_ledger_id]`
//! whose bounds are ledger ids, so consecutive periods provably partition
//! the ledger — and each period is priced with ONE tax calculation against
//! the billing address the checkout webhook stores on `users` (migration
//! 0025; capture runs even while this module is off, so the addresses exist
//! before the flip). The period lifecycle follows the 0021/0024 discipline:
//! every step's answer is frozen by a guarded single-statement write, NULL
//! means "not yet", and each stamp is what stops the sweep repeating that
//! step.
//!
//! # The opening-balance exemption
//!
//! Credits bought before the flip were taxed at purchase; taxing their
//! redemption too would tax the same dollar twice, in the other direction.
//! At enrollment — a user's first sweep pass with the mechanism on — the
//! user's then-current balance becomes an exemption that periods consume
//! before anything is taxable, so only usage funded by post-flip (untaxed)
//! deposits is taxed. Users created after activation get a zero exemption:
//! nothing they ever deposited was taxed at purchase. Promotional credit
//! held at enrollment sits inside the exemption — it was granted free,
//! never taxed, and whether redeeming it is taxable at all is an accountant
//! question this code deliberately does not answer; an exemption errs toward
//! not collecting.
//!
//! # Collection is clamped; the filing is not
//!
//! The tax debit takes `min(tax, balance)` under the same per-user advisory
//! lock admission and settlement serialize on, so the non-negative balance
//! CHECK stays intact and a drained balance cannot be overdrawn. What the
//! balance cannot cover is recorded as a shortfall and absorbed: the
//! vendor's tax liability does not depend on whether the customer could be
//! charged for it, so the recorded tax transaction always carries the FULL
//! calculated figure. Shortfalls are logged loudly; systematic shortfall is
//! prevented by the exemption above (the one structural case — pre-flip
//! balances — never reaches the debit).
//!
//! # What this module deliberately does not do
//!
//! No reversal path (usage is never refunded — disputes reverse PURCHASES,
//! and those are the 0024 lifecycle's job). No retroactive taxation of usage
//! before enrollment. No address invention — a user with no stored address
//! stays unpriced, logged with [`REDEMPTION_TAX_FALLBACK_FIELD`] on every
//! pass, and is priced correctly the day an address exists; nothing is
//! frozen wrong just to make a log line stop. And no answer to the legal
//! question itself: with the env unset, a deployment behaves as if this
//! module did not exist.

use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use serde_json::Value;
use uuid::Uuid;

use crate::sqlx::{self, PgPool};
use crate::stripe::{CHECKOUT_CURRENCY, TaxAddress};
use crate::web::StripeSettings;

/// The mode switch. Unset means `off`; any other value than the three names
/// refuses startup — a misread tax knob must not quietly resolve to either
/// "collect nothing" or "start debiting balances".
pub const REDEMPTION_TAX_ENV: &str = "ZEROROUTER_REDEMPTION_TAX";

/// The one structured log field every unpriced-period site sets, mirroring
/// autopay's `autopay_tax_fallback`: "which periods cannot be priced, and
/// why?" is one query. Values are the shared [`crate::stripe::TaxFallback`]
/// strings plus `amount_below_one_cent`.
pub const REDEMPTION_TAX_FALLBACK_FIELD: &str = "redemption_tax_fallback";

/// The line-item reference on redemption tax calculations, next to autopay's
/// "ZeroRouter credits (autopay)".
const REDEMPTION_TAX_LINE_REFERENCE: &str = "ZeroRouter credits (redemption)";

/// Rows per pass per queue. The sweep is hourly and self-catching-up; a
/// backlog spread over a few passes beats one unbounded pass holding user
/// locks back to back.
const REDEMPTION_TAX_BATCH: i64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedemptionTaxMode {
    /// The default: this module does not run.
    Off,
    /// Build and price periods so the operator can see what WOULD be
    /// collected — the evidence for (or against) the flip — but touch no
    /// balance and record nothing at Stripe. A dry run leaves no trace in
    /// filing reports: calculations are quotes until a transaction is
    /// created from one, and dry-run never creates one.
    DryRun,
    /// The full lifecycle: price, debit the clamped figure from the balance
    /// as a `tax` ledger entry, and record the full figure as a Stripe tax
    /// transaction.
    Collect,
}

/// Parse [`REDEMPTION_TAX_ENV`]. `Err` is a startup refusal, not a default.
pub fn mode_from_env() -> Result<RedemptionTaxMode, String> {
    match std::env::var(REDEMPTION_TAX_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(RedemptionTaxMode::Off),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "{REDEMPTION_TAX_ENV} is not valid unicode; expected off, dry_run, or collect"
        )),
        Ok(value) => match value.trim() {
            "off" | "" => Ok(RedemptionTaxMode::Off),
            "dry_run" => Ok(RedemptionTaxMode::DryRun),
            "collect" => Ok(RedemptionTaxMode::Collect),
            other => Err(format!(
                "{REDEMPTION_TAX_ENV}={other:?} is not a mode; expected off, dry_run, or collect"
            )),
        },
    }
}

/// Keep a completed checkout's billing address on the buyer's row.
///
/// Called from the paid-session webhook AFTER the credit committed;
/// best-effort by design (a lost address costs one future period a logged
/// fallback, never money), so every failure is a WARN and none propagates.
/// Last write wins: the newest address a buyer checked out with is the best
/// location evidence there is. Components are stored as given — completeness
/// is judged at use by [`TaxAddress::from_parts`], so a partial address
/// today can still become useful context tomorrow.
pub async fn store_buyer_address(pool: &PgPool, user_id: Uuid, address: Option<&Value>) {
    fn field<'a>(address: &'a Value, key: &str) -> Option<&'a str> {
        let value = address.get(key)?.as_str()?.trim();
        (!value.is_empty()).then_some(value)
    }
    let Some(address) = address.filter(|address| address.is_object()) else {
        return;
    };
    // No country, no location: an address object with only a city cannot
    // ever price tax, and overwriting a previously usable address with it
    // would destroy information.
    let Some(country) = field(address, "country") else {
        return;
    };
    let result = sqlx::query(
        r#"
        UPDATE users
        SET tax_address_country = $2,
            tax_address_postal_code = $3,
            tax_address_state = $4,
            tax_address_city = $5,
            tax_address_line1 = $6,
            tax_address_updated_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .bind(country)
    .bind(field(address, "postal_code"))
    .bind(field(address, "state"))
    .bind(field(address, "city"))
    .bind(field(address, "line1"))
    .execute(pool)
    .await;
    if let Err(error) = result {
        tracing::warn!(
            %user_id,
            %error,
            "a checkout billing address could not be stored; redemption tax periods for this user may fall back until the next purchase"
        );
    }
}

/// Keep an autopay buyer's card billing address on their row — but only if
/// nothing is there yet.
///
/// # Why this exists
///
/// [`store_buyer_address`] runs from the checkout webhook, so a user who ONLY
/// ever arms autopay and never runs a manual checkout keeps
/// `tax_address_country IS NULL` forever. The redemption-tax sweep then logs
/// that the period "will be priced when the user's next checkout stores one" —
/// a promise that, for an autopay-only account, never comes true. The addresses
/// are deliberately collected before the operator flips redemption taxation on
/// (see the checkout call site); an autopay-only account accumulated nothing to
/// flip.
///
/// # Why fill-only, when checkout is last-write-wins
///
/// The two addresses are not equal evidence, so they do not get equal
/// precedence. Checkout's comes from a form the buyer filled in for this
/// purpose under `billing_address_collection=required`, so it is complete by
/// construction. A card's `billing_details.address` is a byproduct of saving
/// the card, and for a card saved BEFORE the setup session started requiring a
/// full address it can legitimately be country + postal code and nothing else.
///
/// Last-write-wins across that pair is information-destroying: an autopay
/// charge on an old ZIP-only card would overwrite a full checkout address,
/// silently coarsening every future rating for a user who had already given us
/// the good one. Since autopay charges recur, it would also do so repeatedly.
/// Filling only the empty case cannot lose anything and still closes the gap
/// this function exists for.
///
/// The `country IS NULL` test is the same completeness floor
/// [`store_buyer_address`] uses to decide an address is worth storing at all,
/// so "has an address" means the same thing in both directions.
///
/// Best-effort exactly like its sibling: a lost address costs one future period
/// a logged fallback, never money, so every failure is a WARN and none
/// propagates. It is safe to call on every charge attempt — once a row is
/// filled, later calls match no rows and do nothing.
pub async fn backfill_buyer_address_if_absent(
    pool: &PgPool,
    user_id: Uuid,
    address: Option<&Value>,
) {
    fn field<'a>(address: &'a Value, key: &str) -> Option<&'a str> {
        let value = address.get(key)?.as_str()?.trim();
        (!value.is_empty()).then_some(value)
    }
    let Some(address) = address.filter(|address| address.is_object()) else {
        return;
    };
    let Some(country) = field(address, "country") else {
        return;
    };
    let result = sqlx::query(
        r#"
        UPDATE users
        SET tax_address_country = $2,
            tax_address_postal_code = $3,
            tax_address_state = $4,
            tax_address_city = $5,
            tax_address_line1 = $6,
            tax_address_updated_at = NOW()
        WHERE id = $1 AND tax_address_country IS NULL
        "#,
    )
    .bind(user_id)
    .bind(country)
    .bind(field(address, "postal_code"))
    .bind(field(address, "state"))
    .bind(field(address, "city"))
    .bind(field(address, "line1"))
    .execute(pool)
    .await;
    if let Err(error) = result {
        tracing::warn!(
            %user_id,
            %error,
            "an autopay card billing address could not be stored; redemption tax periods for this user may fall back until a checkout stores one"
        );
    }
}

/// One full sweep pass. Public and synchronous so tests drive the exact code
/// production loops, like `run_autopay_sweep_once`.
pub async fn run_redemption_tax_sweep_once(
    pool: &PgPool,
    settings: &StripeSettings,
    mode: RedemptionTaxMode,
) {
    if mode == RedemptionTaxMode::Off {
        return;
    }
    enroll_users(pool).await;
    open_periods(pool).await;
    price_periods(pool, settings).await;
    if mode == RedemptionTaxMode::Collect {
        collect_periods(pool).await;
        record_periods(pool, settings).await;
    }
}

/// Enroll every user the mechanism has not seen yet: stamp the moment, take
/// the ledger cursor (usage at or before it predates the flip and is never
/// taxed), and grant the opening-balance exemption.
///
/// ACTIVATION — the earliest enrollment stamp anywhere — is what separates
/// "existed before the flip" (their balance was bought tax-paid: exempt it)
/// from "signed up after" (every deposit they will ever make is untaxed:
/// zero exemption). Both reads and the enrollment write run in one
/// transaction; the enrollment UPDATE is a single statement, so the balance
/// and the ledger cursor come from one snapshot and a settlement committing
/// around it moves both or neither.
async fn enroll_users(pool: &PgPool) {
    let result = async {
        let mut transaction = pool.begin().await?;
        let activation = sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT MIN(redemption_tax_enrolled_at) FROM users",
        )
        .fetch_one(&mut *transaction)
        .await?;
        let enrolled = sqlx::query(
            r#"
            UPDATE users u
            SET redemption_tax_enrolled_at = NOW(),
                redemption_tax_from_ledger_id = COALESCE(
                    (SELECT MAX(l.id) FROM credit_ledger l WHERE l.user_id = u.id), 0),
                redemption_tax_exempt_remaining_usd = CASE
                    WHEN u.created_at < $1 THEN GREATEST(u.credit_balance_usd, 0)
                    ELSE 0
                END
            WHERE u.redemption_tax_enrolled_at IS NULL
            "#,
        )
        .bind(activation.unwrap_or_else(chrono::Utc::now))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        transaction.commit().await?;
        Ok::<u64, sqlx::Error>(enrolled)
    }
    .await;
    match result {
        Ok(0) => {}
        Ok(enrolled) => tracing::info!(enrolled, "enrolled users into redemption tax"),
        Err(error) => {
            tracing::warn!(%error, "redemption tax enrollment failed; the sweep will retry")
        }
    }
}

/// Tile new usage into periods: for each enrolled user with usage rows past
/// their cursor, open one period covering everything up to their newest
/// usage row.
///
/// The per-user body runs under the same advisory lock admission and
/// settlement serialize on, and re-reads the cursor inside it, so two sweeps
/// racing the same user agree on the span; the unique
/// `(user_id, through_ledger_id)` index is the backstop that refuses a
/// duplicate outright. Exemption consumption happens in the same
/// transaction as the period insert — the split the period records is the
/// split the user row paid for.
async fn open_periods(pool: &PgPool) {
    let candidates = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT u.id
        FROM users u
        WHERE u.redemption_tax_enrolled_at IS NOT NULL
          AND EXISTS (
              SELECT 1 FROM credit_ledger l
              WHERE l.user_id = u.id
                AND l.entry_type = 'usage'
                AND l.id > GREATEST(
                    u.redemption_tax_from_ledger_id,
                    COALESCE((SELECT MAX(p.through_ledger_id)
                              FROM redemption_tax_periods p
                              WHERE p.user_id = u.id), 0))
          )
        LIMIT $1
        "#,
    )
    .bind(REDEMPTION_TAX_BATCH)
    .fetch_all(pool)
    .await;
    let candidates = match candidates {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(%error, "redemption tax could not list users with new usage");
            return;
        }
    };
    for user_id in candidates {
        if let Err(error) = open_period_for(pool, user_id).await {
            tracing::warn!(%user_id, %error, "could not open a redemption tax period; the sweep will retry");
        }
    }
}

async fn open_period_for(pool: &PgPool, user_id: Uuid) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // The same user-keyed advisory lock as admission, settlement, and the
    // reversal paths: nothing about this user's money moves while the span
    // is being read and split.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;
    let Some((cursor, exempt_remaining)) = sqlx::query_as::<_, (Option<i64>, Option<Decimal>)>(
        r#"
        SELECT GREATEST(
                   u.redemption_tax_from_ledger_id,
                   COALESCE((SELECT MAX(p.through_ledger_id)
                             FROM redemption_tax_periods p
                             WHERE p.user_id = u.id), 0)),
               u.redemption_tax_exempt_remaining_usd
        FROM users u
        WHERE u.id = $1 AND u.redemption_tax_enrolled_at IS NOT NULL
        "#,
    )
    .bind(user_id)
    .fetch_optional(&mut *transaction)
    .await?
    else {
        transaction.rollback().await?;
        return Ok(());
    };
    let (Some(cursor), Some(exempt_remaining)) = (cursor, exempt_remaining) else {
        // The enrollment CHECK makes this unreachable; refusing beats
        // inventing a cursor.
        transaction.rollback().await?;
        return Ok(());
    };
    let (through, usage_usd) = sqlx::query_as::<_, (Option<i64>, Option<Decimal>)>(
        r#"
        SELECT MAX(id), -SUM(amount_usd)
        FROM credit_ledger
        WHERE user_id = $1 AND entry_type = 'usage' AND id > $2
        "#,
    )
    .bind(user_id)
    .bind(cursor)
    .fetch_one(&mut *transaction)
    .await?;
    let (Some(through), Some(usage_usd)) = (through, usage_usd) else {
        // A racing sweep already swept this span.
        transaction.rollback().await?;
        return Ok(());
    };
    if usage_usd <= Decimal::ZERO {
        transaction.rollback().await?;
        return Ok(());
    }
    let exempt = exempt_remaining.min(usage_usd);
    if exempt > Decimal::ZERO {
        sqlx::query(
            r#"
            UPDATE users
            SET redemption_tax_exempt_remaining_usd
                    = redemption_tax_exempt_remaining_usd - $2
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(exempt)
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query(
        r#"
        INSERT INTO redemption_tax_periods
            (user_id, from_ledger_id, through_ledger_id,
             usage_usd, exempt_usd, taxable_usd)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(user_id)
    .bind(cursor)
    .bind(through)
    .bind(usage_usd)
    .bind(exempt)
    .bind(usage_usd - exempt)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

/// Price unpriced periods: one tax calculation per period against the stored
/// checkout address.
///
/// Failure NEVER freezes a wrong answer. A missing or incomplete address
/// leaves the period unpriced — logged with
/// [`REDEMPTION_TAX_FALLBACK_FIELD`] on every pass, priced correctly the day
/// the user's next checkout stores an address — and a refused, unreachable,
/// or tax-inclusive calculation likewise leaves NULL for the next pass.
/// (Autopay freezes an untaxed fallback in these cases because its charge
/// must go out NOW; a period has nowhere to be and can wait for the truth.)
/// The two frozen-without-asking cases are real answers, not failures: a
/// taxable amount below one cent, and a taxable amount of exactly zero
/// (a span fully inside the opening-balance exemption).
async fn price_periods(pool: &PgPool, settings: &StripeSettings) {
    let periods = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Decimal,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        r#"
        SELECT p.id, p.user_id, p.taxable_usd,
               u.tax_address_country, u.tax_address_postal_code,
               u.tax_address_state, u.tax_address_city, u.tax_address_line1
        FROM redemption_tax_periods p
        JOIN users u ON u.id = p.user_id
        WHERE p.tax_usd IS NULL
        ORDER BY p.updated_at
        LIMIT $1
        "#,
    )
    .bind(REDEMPTION_TAX_BATCH)
    .fetch_all(pool)
    .await;
    let periods = match periods {
        Ok(periods) => periods,
        Err(error) => {
            tracing::warn!(%error, "redemption tax could not list unpriced periods");
            return;
        }
    };
    let Ok(client) = crate::stripe::stripe_client() else {
        return;
    };
    for (period_id, user_id, taxable_usd, country, postal_code, state, city, line1) in periods {
        // Whole cents, rounded DOWN: a cent that was not fully spent is
        // never taxed, and `usd_to_cents`-style scale errors cannot arise.
        let taxable_cents = (taxable_usd.round_dp_with_strategy(2, RoundingStrategy::ToZero)
            * Decimal::ONE_HUNDRED)
            .normalize()
            .to_i64()
            .unwrap_or(0);
        if taxable_cents <= 0 {
            freeze_zero_tax(pool, period_id, "amount_below_one_cent").await;
            continue;
        }
        let address = match TaxAddress::from_parts(
            country.as_deref(),
            postal_code.as_deref(),
            state.as_deref(),
            city.as_deref(),
            line1.as_deref(),
        ) {
            Ok(address) => address,
            Err(reason) => {
                tracing::warn!(
                    %user_id,
                    %period_id,
                    { REDEMPTION_TAX_FALLBACK_FIELD } = reason.as_str(),
                    "a redemption tax period cannot be priced: no usable stored address; it will be priced when the user's next checkout stores one"
                );
                touch_period(pool, period_id).await;
                continue;
            }
        };
        match calculate_period_tax(settings, &client, &address, taxable_cents).await {
            Ok((tax_usd, calculation_id)) => {
                let frozen = sqlx::query(
                    r#"
                    UPDATE redemption_tax_periods
                    SET tax_usd = $2, tax_calculation_id = $3, updated_at = NOW()
                    WHERE id = $1 AND tax_usd IS NULL
                    "#,
                )
                .bind(period_id)
                .bind(tax_usd)
                .bind(&calculation_id)
                .execute(pool)
                .await;
                if let Err(error) = frozen {
                    tracing::warn!(%period_id, %error, "a priced redemption tax period could not be frozen; the sweep will re-price it");
                }
            }
            Err(()) => touch_period(pool, period_id).await,
        }
    }
}

/// Rotate a failed period to the back of its work queue (the partial
/// indexes and the pass ORDER BY are both on `updated_at`), so a stuck row
/// keeps logging without starving everything created after it.
async fn touch_period(pool: &PgPool, period_id: Uuid) {
    let _ = sqlx::query("UPDATE redemption_tax_periods SET updated_at = NOW() WHERE id = $1")
        .bind(period_id)
        .execute(pool)
        .await;
}

async fn freeze_zero_tax(pool: &PgPool, period_id: Uuid, reason: &str) {
    let result = sqlx::query(
        r#"
        UPDATE redemption_tax_periods
        SET tax_usd = 0, fallback_reason = $2, updated_at = NOW()
        WHERE id = $1 AND tax_usd IS NULL
        "#,
    )
    .bind(period_id)
    .bind(reason)
    .execute(pool)
    .await;
    if let Err(error) = result {
        tracing::warn!(%period_id, %error, "a zero-tax redemption period could not be frozen; the sweep will retry");
    }
}

/// One tax calculation for one period. The request shape matches autopay's
/// ([`crate::stripe`]): no `tax_code` and no `tax_behavior`, so the Tax
/// Settings preset governs every surface and they cannot drift — which is
/// also exactly what makes the flip coherent: the day the preset is the
/// stored-value code, purchases price zero and THIS is the surface that
/// prices the real figure.
///
/// `Err(())` always means "leave the period unpriced and try again later";
/// the reason is already logged here with enough to act on.
async fn calculate_period_tax(
    settings: &StripeSettings,
    client: &reqwest::Client,
    address: &TaxAddress,
    taxable_cents: i64,
) -> Result<(Decimal, String), ()> {
    let amount = taxable_cents.to_string();
    let mut form: Vec<(&str, &str)> = vec![
        ("currency", CHECKOUT_CURRENCY),
        ("line_items[0][amount]", &amount),
        ("line_items[0][reference]", REDEMPTION_TAX_LINE_REFERENCE),
        ("customer_details[address][country]", &address.country),
        // The stored address is the card's billing address from checkout.
        ("customer_details[address_source]", "billing"),
    ];
    for (key, value) in [
        (
            "customer_details[address][postal_code]",
            &address.postal_code,
        ),
        ("customer_details[address][state]", &address.state),
        ("customer_details[address][city]", &address.city),
        ("customer_details[address][line1]", &address.line1),
    ] {
        if let Some(value) = value {
            form.push((key, value));
        }
    }
    let response = client
        .post(format!("{}/v1/tax/calculations", settings.api_base))
        .bearer_auth(&settings.secret_key)
        .form(&form)
        .send()
        .await
        .map_err(|_| {
            tracing::warn!(
                { REDEMPTION_TAX_FALLBACK_FIELD } = "calculation_unavailable",
                "a redemption tax calculation could not be sent; the period stays unpriced"
            );
        })?;
    let status = response.status();
    if !status.is_success() {
        let reason = if status.is_client_error() {
            "calculation_rejected"
        } else {
            "calculation_unavailable"
        };
        tracing::warn!(
            status = status.as_u16(),
            { REDEMPTION_TAX_FALLBACK_FIELD } = reason,
            "stripe refused a redemption tax calculation; the period stays unpriced"
        );
        return Err(());
    }
    let body = response.json::<Value>().await.map_err(|_| {
        tracing::warn!(
            { REDEMPTION_TAX_FALLBACK_FIELD } = "calculation_unavailable",
            "a redemption tax calculation response did not parse; the period stays unpriced"
        );
    })?;
    let inclusive = body
        .get("tax_amount_inclusive")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let exclusive = body.get("tax_amount_exclusive").and_then(Value::as_i64);
    let calculation_id = body.get("id").and_then(Value::as_str);
    let (Some(tax_cents), Some(calculation_id)) = (exclusive, calculation_id) else {
        tracing::warn!(
            { REDEMPTION_TAX_FALLBACK_FIELD } = "calculation_unavailable",
            "a redemption tax calculation omitted its id or exclusive tax; the period stays unpriced"
        );
        return Err(());
    };
    if tax_cents < 0 || inclusive != 0 {
        // The same misconfiguration autopay refuses: an inclusive figure
        // means Tax Settings is carving tax OUT of amounts. Autopay freezes
        // untaxed because its charge cannot wait; a period can, so this
        // stays unpriced until the operator fixes the setting.
        tracing::error!(
            tax_cents,
            inclusive,
            { REDEMPTION_TAX_FALLBACK_FIELD } = "calculation_rejected",
            "a redemption tax calculation came back negative or tax-inclusive; check Tax Settings' default tax behavior — the period stays unpriced"
        );
        return Err(());
    }
    Ok((
        Decimal::from(tax_cents) / Decimal::ONE_HUNDRED,
        calculation_id.to_owned(),
    ))
}

/// Debit priced, uncollected periods from their users' balances — clamped,
/// exactly once, under the money lock.
async fn collect_periods(pool: &PgPool) {
    let periods = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM redemption_tax_periods
        WHERE tax_usd IS NOT NULL AND debited_at IS NULL
        ORDER BY updated_at
        LIMIT $1
        "#,
    )
    .bind(REDEMPTION_TAX_BATCH)
    .fetch_all(pool)
    .await;
    let periods = match periods {
        Ok(periods) => periods,
        Err(error) => {
            tracing::warn!(%error, "redemption tax could not list uncollected periods");
            return;
        }
    };
    for period_id in periods {
        match collect_period(pool, period_id).await {
            Ok(Some((collected, shortfall))) if shortfall > Decimal::ZERO => {
                // Loud: this is money the vendor absorbs. Structural cases
                // are prevented by the exemption; what remains is a balance
                // spent to (or below) zero between pricing and collection.
                tracing::warn!(
                    %period_id,
                    collected_usd = %collected,
                    shortfall_usd = %shortfall,
                    "a redemption tax debit was clamped by the balance; the shortfall is absorbed and the full figure is still filed"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%period_id, %error, "a redemption tax debit failed; the sweep will retry");
                touch_period(pool, period_id).await;
            }
        }
    }
}

/// The one place redemption tax moves money. Returns what was collected and
/// what could not be, or `None` if another pass got here first.
async fn collect_period(
    pool: &PgPool,
    period_id: Uuid,
) -> Result<Option<(Decimal, Decimal)>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Advisory lock FIRST (the same order settlement and the reversal paths
    // use), then the period row lock: under the user's money lock nothing
    // can debit or credit this balance until commit, so read-then-write is
    // race-free without OLD-row RETURNING gymnastics.
    let Some(user_id) =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM redemption_tax_periods WHERE id = $1")
            .bind(period_id)
            .fetch_optional(&mut *transaction)
            .await?
    else {
        transaction.rollback().await?;
        return Ok(None);
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;
    let Some(tax_usd) = sqlx::query_scalar::<_, Option<Decimal>>(
        r#"
        SELECT tax_usd FROM redemption_tax_periods
        WHERE id = $1 AND debited_at IS NULL
        FOR UPDATE
        "#,
    )
    .bind(period_id)
    .fetch_optional(&mut *transaction)
    .await?
    .flatten() else {
        // Already collected, or unpriced (cannot happen from the pass's
        // WHERE, but refuse rather than debit an unpriced figure).
        transaction.rollback().await?;
        return Ok(None);
    };
    let collected = if tax_usd > Decimal::ZERO {
        let balance =
            sqlx::query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&mut *transaction)
                .await?;
        let collect = tax_usd.min(balance.max(Decimal::ZERO));
        if collect > Decimal::ZERO {
            let balance_after = sqlx::query_scalar::<_, Decimal>(
                r#"
                UPDATE users
                SET credit_balance_usd = credit_balance_usd - $2
                WHERE id = $1
                RETURNING credit_balance_usd
                "#,
            )
            .bind(user_id)
            .bind(collect)
            .fetch_one(&mut *transaction)
            .await?;
            let ledger_id = sqlx::query_scalar::<_, i64>(
                r#"
                INSERT INTO credit_ledger
                    (user_id, entry_type, amount_usd, balance_after_usd, note)
                VALUES ($1, 'tax', $2, $3, $4)
                RETURNING id
                "#,
            )
            .bind(user_id)
            .bind(-collect)
            .bind(balance_after)
            .bind(format!("redemption tax period {period_id}"))
            .fetch_one(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE redemption_tax_periods SET collection_ledger_id = $2 WHERE id = $1",
            )
            .bind(period_id)
            .bind(ledger_id)
            .execute(&mut *transaction)
            .await?;
        }
        collect
    } else {
        Decimal::ZERO
    };
    sqlx::query(
        r#"
        UPDATE redemption_tax_periods
        SET debited_at = NOW(), collected_usd = $2, shortfall_usd = $3,
            updated_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(period_id)
    .bind(collected)
    .bind(tax_usd - collected)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Some((collected, tax_usd - collected)))
}

/// Record collected periods' tax transactions, 0024-style: the reference is
/// derived from the period id (unique across all transactions, so the
/// endpoint deduplicates a retry), the returned id is stored at record time
/// because it cannot be looked up later, and a failure is logged and
/// retried, never dropped.
async fn record_periods(pool: &PgPool, settings: &StripeSettings) {
    let periods = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, tax_calculation_id FROM redemption_tax_periods
        WHERE debited_at IS NOT NULL
          AND tax_calculation_id IS NOT NULL
          AND tax_recorded_at IS NULL
        ORDER BY updated_at
        LIMIT $1
        "#,
    )
    .bind(REDEMPTION_TAX_BATCH)
    .fetch_all(pool)
    .await;
    let periods = match periods {
        Ok(periods) => periods,
        Err(error) => {
            tracing::warn!(%error, "redemption tax could not list unrecorded periods");
            return;
        }
    };
    let Ok(client) = crate::stripe::stripe_client() else {
        return;
    };
    for (period_id, calculation_id) in periods {
        let reference = format!("rtx_{period_id}");
        let form: [(&str, &str); 2] = [("calculation", &calculation_id), ("reference", &reference)];
        let response = client
            .post(format!(
                "{}/v1/tax/transactions/create_from_calculation",
                settings.api_base
            ))
            .bearer_auth(&settings.secret_key)
            .form(&form)
            .send()
            .await;
        let transaction_id = match response {
            Ok(response) if response.status().is_success() => response
                .json::<Value>()
                .await
                .ok()
                .and_then(|body| Some(body.get("id")?.as_str()?.to_owned())),
            Ok(response) => {
                tracing::error!(
                    %period_id,
                    tax_calculation = %calculation_id,
                    status = response.status().as_u16(),
                    "redemption tax was collected but its tax transaction was not recorded; the sweep will retry"
                );
                None
            }
            Err(_) => {
                tracing::error!(
                    %period_id,
                    tax_calculation = %calculation_id,
                    "the redemption tax transaction call could not be sent; the sweep will retry"
                );
                None
            }
        };
        match transaction_id {
            Some(transaction_id) => {
                let stamped = sqlx::query(
                    r#"
                    UPDATE redemption_tax_periods
                    SET tax_transaction_id = $2, tax_recorded_at = NOW(),
                        updated_at = NOW()
                    WHERE id = $1 AND tax_recorded_at IS NULL
                    "#,
                )
                .bind(period_id)
                .bind(&transaction_id)
                .execute(pool)
                .await;
                match stamped {
                    Ok(_) => tracing::info!(
                        %period_id,
                        tax_transaction = %transaction_id,
                        "redemption tax transaction recorded"
                    ),
                    Err(error) => tracing::error!(
                        %period_id,
                        tax_transaction = %transaction_id,
                        %error,
                        "recorded a redemption tax transaction but could not store its id"
                    ),
                }
            }
            None => touch_period(pool, period_id).await,
        }
    }
}

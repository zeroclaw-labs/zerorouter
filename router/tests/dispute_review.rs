//! The dispute review workflow: `admin disputes list | show | resolve`
//! (migration 0013).
//!
//! Every test drives the real `zerorouter admin` binary, the way an operator
//! does, because the JSON on stdout IS the deliverable — a test that called the
//! `billing` helpers directly would prove the arithmetic and nothing about the
//! surface the operator reads.
//!
//! Accounts are seeded through the real money paths (`credit_purchase`,
//! `reverse_purchase`, `freeze_account`) so the states under review are the
//! states a chargeback actually produces. The one exception is the backdated
//! reversal in [`seed_stale_reversal`], which exists to isolate the
//! negative-balance trigger from the recent-refund one; it writes exactly what
//! `reverse_purchase` writes, with an older timestamp, because `credit_ledger`
//! is append-only and a row cannot be backdated after the fact.
//!
//! Gated on `DATABASE_URL` like the rest of the DB-backed suites: unset means
//! each test returns early instead of failing. Assertions are scoped to the
//! emails a test created — the queue is global and other suites share the
//! database.

use std::{process::Command, str::FromStr};

use rust_decimal::Decimal;
use serde_json::Value;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{
    billing::{FreezeReason, balance, credit_purchase, freeze_account, reverse_purchase},
    db::migrate,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn run_admin(database_url: &str, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zerorouter"))
        .env("DATABASE_URL", database_url)
        .arg("admin")
        .args(arguments)
        .output()
        .expect("admin command should start")
}

fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "admin output should be JSON ({error}); stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

async fn connect() -> Option<(PgPool, String)> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some((pool, database_url))
}

/// A user with one key, created through the real mint path.
async fn seed_user(database_url: &str, label: &str) -> String {
    let email = format!("dispute-{label}-{}@example.invalid", Uuid::new_v4());
    let minted = run_admin(
        database_url,
        &["mint-key", "--email", &email, "--name", label],
    );
    assert!(
        minted.status.success(),
        "mint should succeed: {}",
        String::from_utf8_lossy(&minted.stderr)
    );
    email
}

async fn user_id(pool: &PgPool, email: &str) -> Uuid {
    query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .expect("seeded user should resolve")
}

/// Buy `amount`, consume `spent` of it, then have the charge reversed — the
/// spent-through chargeback that leaves a receivable behind.
///
/// The spend is applied as a direct balance write rather than by driving real
/// inference: what is under test is the review of the resulting state, and a
/// downward write to a positive balance is exactly what settlement does.
async fn seed_spent_through_reversal(
    pool: &PgPool,
    user: Uuid,
    amount: i64,
    spent: i64,
    anchor: &str,
) {
    let session = format!("cs_test_{anchor}");
    let intent = format!("pi_test_{anchor}");
    credit_purchase(
        pool,
        user,
        Decimal::from(amount),
        &session,
        Some(intent.as_str()),
    )
    .await
    .expect("purchase should credit");
    query("UPDATE users SET credit_balance_usd = credit_balance_usd - $2 WHERE id = $1")
        .bind(user)
        .bind(Decimal::from(spent))
        .execute(pool)
        .await
        .expect("simulated spend should apply");
    reverse_purchase(
        pool,
        &intent,
        &format!("dp_test_{anchor}"),
        "test chargeback",
    )
    .await
    .expect("reversal should apply");
}

/// A reversal older than any review window, with the negative balance it left.
///
/// Written directly because `credit_ledger` is append-only: a row's
/// `created_at` cannot be moved after the INSERT, so an old reversal has to be
/// inserted old. The balance write declares itself to the 0009 overdraft
/// trigger exactly as `reverse_purchase` does — without that declaration
/// Postgres refuses the write, which is the tripwire doing its job.
async fn seed_stale_reversal(pool: &PgPool, user: Uuid, amount: i64, age_days: i64, anchor: &str) {
    let mut transaction = pool.begin().await.expect("transaction should begin");
    query("SET LOCAL zerorouter.credit_reversal = 'on'")
        .execute(&mut *transaction)
        .await
        .expect("reversal declaration should apply");
    let balance_after = query_scalar::<_, Decimal>(
        "UPDATE users SET credit_balance_usd = credit_balance_usd - $2 WHERE id = $1 \
         RETURNING credit_balance_usd",
    )
    .bind(user)
    .bind(Decimal::from(amount))
    .fetch_one(&mut *transaction)
    .await
    .expect("stale reversal should drive the balance negative");
    query(
        "INSERT INTO credit_ledger (user_id, entry_type, amount_usd, balance_after_usd, \
         stripe_session_id, stripe_payment_intent_id, note, created_at) \
         VALUES ($1, 'refund', $2, $3, $4, $5, $6, NOW() - MAKE_INTERVAL(days => $7))",
    )
    .bind(user)
    .bind(-Decimal::from(amount))
    .bind(balance_after)
    .bind(format!("dp_stale_{anchor}"))
    .bind(format!("pi_stale_{anchor}"))
    .bind("stale test chargeback")
    .bind(i32::try_from(age_days).expect("age fits"))
    .execute(&mut *transaction)
    .await
    .expect("stale reversal row should insert");
    transaction.commit().await.expect("commit should succeed");
}

/// The queue row for one email, or `None` when the account is not listed.
fn queue_row<'a>(listing: &'a Value, email: &str) -> Option<&'a Value> {
    listing["queue"]
        .as_array()
        .expect("queue should be an array")
        .iter()
        .find(|row| row["email"].as_str() == Some(email))
}

fn triggers(row: &Value) -> Vec<String> {
    row["triggers"]
        .as_array()
        .expect("triggers should be an array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("trigger should be a string")
                .to_owned()
        })
        .collect()
}

async fn ledger_count(pool: &PgPool, user: Uuid, entry_type: &str) -> i64 {
    query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = $2",
    )
    .bind(user)
    .bind(entry_type)
    .fetch_one(pool)
    .await
    .expect("ledger count should query")
}

// ---------------------------------------------------------------------------
// `disputes list` — the review queue
// ---------------------------------------------------------------------------

/// Each trigger class puts an account on the queue on its own, an account with
/// several is reported with all of them, and an untouched account stays off.
#[tokio::test]
async fn list_surfaces_every_trigger_class_and_leaves_healthy_accounts_alone() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };

    // Frozen only: an operator hold on an account that owes nothing and has
    // never been reversed.
    let frozen_email = seed_user(&database_url, "frozen").await;
    let frozen_id = user_id(&pool, &frozen_email).await;
    freeze_account(&pool, frozen_id, FreezeReason::Operator)
        .await
        .expect("freeze should apply");

    // Negative balance only: the reversal that caused it is older than the
    // window, so `recent_refund` must not fire.
    let owing_email = seed_user(&database_url, "owing").await;
    let owing_id = user_id(&pool, &owing_email).await;
    seed_stale_reversal(&pool, owing_id, 40, 90, &Uuid::new_v4().to_string()).await;

    // Recent refund only: reversed inside the window, but nothing was spent, so
    // the balance landed at zero and the account was never frozen.
    let refunded_email = seed_user(&database_url, "refunded").await;
    let refunded_id = user_id(&pool, &refunded_email).await;
    seed_spent_through_reversal(&pool, refunded_id, 25, 0, &Uuid::new_v4().to_string()).await;

    // All three at once: the spent-through chargeback the freeze path produces.
    let disputed_email = seed_user(&database_url, "disputed").await;
    let disputed_id = user_id(&pool, &disputed_email).await;
    seed_spent_through_reversal(&pool, disputed_id, 50, 40, &Uuid::new_v4().to_string()).await;
    freeze_account(&pool, disputed_id, FreezeReason::Dispute)
        .await
        .expect("dispute freeze should apply");

    // Untouched: funded, live, never reversed.
    let healthy_email = seed_user(&database_url, "healthy").await;
    let healthy_id = user_id(&pool, &healthy_email).await;
    credit_purchase(
        &pool,
        healthy_id,
        Decimal::from(20),
        &format!("cs_healthy_{healthy_id}"),
        Some(&format!("pi_healthy_{healthy_id}")),
    )
    .await
    .expect("healthy purchase should credit");

    let listed = run_admin(&database_url, &["disputes", "list"]);
    assert!(
        listed.status.success(),
        "list should succeed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listing = json(&listed);
    assert_eq!(listing["window_days"], 30);

    let frozen = queue_row(&listing, &frozen_email).expect("a frozen account is under review");
    assert_eq!(triggers(frozen), vec!["frozen"]);
    assert_eq!(frozen["frozen"], true);
    assert_eq!(frozen["frozen_reason"], "operator");
    // Frozen but solvent: there is no debt to settle, only trust to restore.
    assert!(frozen["receivable_usd"].is_null());

    let owing = queue_row(&listing, &owing_email).expect("a negative balance is under review");
    assert_eq!(triggers(owing), vec!["negative_balance"]);
    assert_eq!(owing["frozen"], false);
    assert_eq!(owing["receivable_usd"].as_str(), Some("40"));
    // The stale reversal is still reported as history even though it did not
    // put the account on the list — the operator needs the Stripe ids.
    assert_eq!(owing["recent_refunds"], 0);
    assert_eq!(
        owing["stripe_object_ids"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(1)
    );

    let refunded = queue_row(&listing, &refunded_email).expect("a recent reversal is under review");
    assert_eq!(triggers(refunded), vec!["recent_refund"]);
    assert_eq!(refunded["recent_refunds"], 1);
    assert!(refunded["receivable_usd"].is_null());
    assert_eq!(refunded["reversed_in_window_usd"].as_str(), Some("25"));

    let disputed = queue_row(&listing, &disputed_email).expect("a chargeback is under review");
    assert_eq!(
        triggers(disputed),
        vec!["frozen", "negative_balance", "recent_refund"]
    );
    assert_eq!(disputed["frozen_reason"], "dispute");
    assert_eq!(disputed["receivable_usd"].as_str(), Some("40"));
    let object_ids = disputed["stripe_object_ids"]
        .as_array()
        .expect("stripe object ids should be an array");
    assert!(
        object_ids
            .iter()
            .any(|id| id.as_str().is_some_and(|id| id.starts_with("dp_test_"))),
        "the dispute id should be listed for dashboard lookup: {object_ids:?}"
    );
    assert!(
        disputed["stripe_payment_intent_ids"]
            .as_array()
            .is_some_and(|ids| ids
                .iter()
                .any(|id| id.as_str().is_some_and(|id| id.starts_with("pi_test_")))),
        "the reversed PaymentIntent should be listed"
    );

    assert!(
        queue_row(&listing, &healthy_email).is_none(),
        "an untouched, funded account must not be in the review queue"
    );

    // The window narrows only the recent-refund trigger: a receivable never
    // ages off the list, because nothing else would ever show it again.
    let narrow = run_admin(&database_url, &["disputes", "list", "--days", "1"]);
    assert!(narrow.status.success());
    let narrow = json(&narrow);
    assert_eq!(narrow["window_days"], 1);
    assert!(
        queue_row(&narrow, &owing_email).is_some(),
        "a receivable must not age out of the queue"
    );
    assert!(
        queue_row(&narrow, &refunded_email).is_some(),
        "a reversal from moments ago is inside a one-day window"
    );

    let rejected = run_admin(&database_url, &["disputes", "list", "--days", "0"]);
    assert!(!rejected.status.success(), "a zero-day window is nonsense");
}

// ---------------------------------------------------------------------------
// `disputes show` — the review transcript
// ---------------------------------------------------------------------------

/// One account rendered in full: freeze state, balance, the WHOLE ledger with
/// its Stripe anchors, usage, and key metadata with no credential material.
#[tokio::test]
async fn show_renders_the_full_history_without_credential_material() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "show").await;
    let user = user_id(&pool, &email).await;
    let anchor = Uuid::new_v4().to_string();
    seed_spent_through_reversal(&pool, user, 50, 40, &anchor).await;
    freeze_account(&pool, user, FreezeReason::Dispute)
        .await
        .expect("dispute freeze should apply");

    let shown = run_admin(&database_url, &["disputes", "show", "--email", &email]);
    assert!(
        shown.status.success(),
        "show should succeed: {}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let view = json(&shown);

    assert_eq!(view["email"], email.as_str());
    assert_eq!(view["frozen"], true);
    assert_eq!(view["frozen_reason"], "dispute");
    assert_eq!(view["balance_usd"].as_str(), Some("-40"));
    assert_eq!(view["receivable_usd"].as_str(), Some("40"));

    // Both money movements are present, and the reversal carries the Stripe
    // ids an operator needs to find the case in the dashboard.
    let ledger = view["ledger"]
        .as_array()
        .expect("ledger should be an array");
    assert_eq!(ledger.len(), 2, "purchase and reversal: {ledger:?}");
    let purchase = ledger
        .iter()
        .find(|entry| entry["entry_type"] == "purchase")
        .expect("the purchase should be in the history");
    assert_eq!(purchase["amount_usd"].as_str(), Some("50"));
    assert_eq!(
        purchase["stripe_session_id"].as_str(),
        Some(format!("cs_test_{anchor}").as_str())
    );
    let reversal = ledger
        .iter()
        .find(|entry| entry["entry_type"] == "refund")
        .expect("the reversal should be in the history");
    assert_eq!(reversal["amount_usd"].as_str(), Some("-50"));
    assert_eq!(reversal["balance_after_usd"].as_str(), Some("-40"));
    assert_eq!(
        reversal["stripe_session_id"].as_str(),
        Some(format!("dp_test_{anchor}").as_str())
    );
    assert_eq!(
        reversal["stripe_payment_intent_id"].as_str(),
        Some(format!("pi_test_{anchor}").as_str())
    );
    assert!(reversal["created_at"].is_string());
    assert!(reversal["note"].is_string());

    // The usage summary exists and is honest about an account that never made
    // a request; the seeded spend was a balance write, not metered inference.
    assert_eq!(view["usage"]["requests"], 0);
    assert_eq!(view["usage"]["spend_usd"].as_str(), Some("0"));
    assert!(view["usage"]["first_request_at"].is_null());

    // Keys are metadata only.
    let keys = view["keys"].as_array().expect("keys should be an array");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["disabled"], false);
    let transcript = String::from_utf8(shown.stdout).expect("output should be UTF-8");
    assert!(
        !transcript.contains("key_hash"),
        "a review transcript must never carry a key digest"
    );
    assert!(
        !transcript.contains("zcr_"),
        "a review transcript must never carry a plaintext key"
    );

    let missing = run_admin(
        &database_url,
        &["disputes", "show", "--email", "nobody@example.invalid"],
    );
    assert!(
        !missing.status.success(),
        "an unknown email must fail loudly, not render an empty account"
    );
}

// ---------------------------------------------------------------------------
// `disputes resolve --write-off`
// ---------------------------------------------------------------------------

/// A write-off lands the balance on exactly zero, records what it forgave, and
/// leaves the freeze exactly where it found it.
#[tokio::test]
async fn write_off_zeroes_the_receivable_exactly_and_does_not_unfreeze() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "writeoff").await;
    let user = user_id(&pool, &email).await;
    seed_spent_through_reversal(&pool, user, 50, 40, &Uuid::new_v4().to_string()).await;
    freeze_account(&pool, user, FreezeReason::Dispute)
        .await
        .expect("dispute freeze should apply");
    let frozen_at_before = query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT frozen_at FROM users WHERE id = $1",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .expect("freeze timestamp should query");

    let resolved = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--write-off",
            "--note",
            "uncollectable; card issuer ruled for the customer",
        ],
    );
    assert!(
        resolved.status.success(),
        "write-off should succeed: {}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let report = json(&resolved);
    assert_eq!(report["action"], "written_off");
    assert_eq!(report["detail"]["forgiven_usd"].as_str(), Some("40"));
    assert_eq!(report["balance_usd"].as_str(), Some("0"));
    assert!(report["receivable_usd"].is_null());

    // Exactly zero, not merely non-negative.
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::ZERO
    );
    assert_eq!(ledger_count(&pool, user, "writeoff").await, 1);
    let (amount, note) = sqlx_core::query_as::query_as::<_, (Decimal, Option<String>)>(
        "SELECT amount_usd, note FROM credit_ledger WHERE user_id = $1 AND entry_type = 'writeoff'",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .expect("the write-off entry should query");
    // The forgiven amount is recorded as a positive credit: the entry states
    // what was given up, and only a reversal may be negative.
    assert_eq!(amount, Decimal::from(40));
    assert_eq!(
        note.as_deref(),
        Some("uncollectable; card issuer ruled for the customer")
    );

    // Settling the money is not restoring trust.
    assert_eq!(report["frozen_before"], true);
    assert_eq!(report["frozen"], true);
    assert_eq!(report["frozen_reason"], "dispute");
    let frozen_at_after = query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT frozen_at FROM users WHERE id = $1",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .expect("freeze timestamp should query");
    assert_eq!(
        frozen_at_before, frozen_at_after,
        "resolving must not restamp or lift the freeze"
    );
}

/// GUARD (mutation-checked): repeating a completed write-off is a no-op that
/// says so. The ledger is append-only, so a second entry could not be taken
/// back — and it would be indistinguishable from a real second forgiveness.
#[tokio::test]
async fn repeating_a_completed_write_off_is_a_no_op_that_says_so() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "writeoff-twice").await;
    let user = user_id(&pool, &email).await;
    seed_spent_through_reversal(&pool, user, 30, 30, &Uuid::new_v4().to_string()).await;

    let arguments = [
        "disputes",
        "resolve",
        "--email",
        &email,
        "--write-off",
        "--note",
        "first pass",
    ];
    let first = run_admin(&database_url, &arguments);
    assert!(first.status.success(), "the first write-off should succeed");
    assert_eq!(json(&first)["action"], "written_off");
    assert_eq!(ledger_count(&pool, user, "writeoff").await, 1);

    let second = run_admin(&database_url, &arguments);
    assert!(
        second.status.success(),
        "a repeated write-off is a no-op, not a failure: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let report = json(&second);
    assert_eq!(
        report["action"], "already_written_off",
        "the repeat must SAY it did nothing"
    );
    assert_eq!(report["balance_usd"].as_str(), Some("0"));
    assert_eq!(
        ledger_count(&pool, user, "writeoff").await,
        1,
        "a repeated write-off must not append a second forgiveness"
    );
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::ZERO
    );
}

/// GUARD (mutation-checked): a write-off against an account that owes nothing
/// is refused. A solvent customer's credit is not a receivable, and the
/// likeliest cause of the attempt is the wrong email.
#[tokio::test]
async fn write_off_refuses_an_account_with_a_positive_balance() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "solvent").await;
    let user = user_id(&pool, &email).await;
    let anchor = Uuid::new_v4();
    credit_purchase(
        &pool,
        user,
        Decimal::from(25),
        &format!("cs_solvent_{anchor}"),
        Some(&format!("pi_solvent_{anchor}")),
    )
    .await
    .expect("purchase should credit");
    // Frozen, so the account is genuinely under review: what refuses the
    // write-off is the absence of a debt, not the absence of a case.
    freeze_account(&pool, user, FreezeReason::Operator)
        .await
        .expect("freeze should apply");

    let refused = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--write-off",
            "--note",
            "should not be possible",
        ],
    );
    assert!(
        !refused.status.success(),
        "writing off a solvent account must fail"
    );
    let report = json(&refused);
    assert_eq!(report["action"], "refused");
    assert!(
        report["detail"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no receivable")),
        "the refusal must say why: {report}"
    );

    // The customer's money is untouched and nothing was appended.
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::from(25)
    );
    assert_eq!(ledger_count(&pool, user, "writeoff").await, 0);
}

// ---------------------------------------------------------------------------
// `disputes resolve --recover`
// ---------------------------------------------------------------------------

/// A recovery credits exactly what it was told, and leaves the freeze alone.
#[tokio::test]
async fn recover_credits_the_balance_by_exactly_the_amount_given() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "recover").await;
    let user = user_id(&pool, &email).await;
    seed_spent_through_reversal(&pool, user, 50, 40, &Uuid::new_v4().to_string()).await;
    freeze_account(&pool, user, FreezeReason::Dispute)
        .await
        .expect("dispute freeze should apply");

    let recovered = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--recover",
            "15.50",
            "--note",
            "wire received 2026-08-09, ref 8841",
        ],
    );
    assert!(
        recovered.status.success(),
        "recovery should succeed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let report = json(&recovered);
    assert_eq!(report["action"], "recovered");
    assert_eq!(report["detail"]["recovered_usd"].as_str(), Some("15.50"));
    // -40 + 15.50: a partial recovery leaves the rest of the debt standing.
    assert_eq!(report["balance_usd"].as_str(), Some("-24.50"));
    assert_eq!(report["receivable_usd"].as_str(), Some("24.50"));
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::from_str("-24.50").expect("literal parses")
    );
    assert_eq!(ledger_count(&pool, user, "recovery").await, 1);

    // Settled money, unchanged trust.
    assert_eq!(report["frozen"], true);
    assert_eq!(report["frozen_reason"], "dispute");
}

/// A recovery on an account that is not under review is refused: crediting a
/// healthy account is a promo grant, and `grant-credit` is the command for it.
#[tokio::test]
async fn recover_refuses_an_account_that_is_not_under_review() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "healthy-recover").await;
    let user = user_id(&pool, &email).await;
    let anchor = Uuid::new_v4();
    credit_purchase(
        &pool,
        user,
        Decimal::from(10),
        &format!("cs_ok_{anchor}"),
        Some(&format!("pi_ok_{anchor}")),
    )
    .await
    .expect("purchase should credit");

    let refused = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--recover",
            "100",
            "--note",
            "should not be possible",
        ],
    );
    assert!(
        !refused.status.success(),
        "recovering against a healthy account must fail"
    );
    assert_eq!(json(&refused)["action"], "refused");
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::from(10)
    );
    assert_eq!(ledger_count(&pool, user, "recovery").await, 0);
}

/// 0013 narrows the 0009 overdraft tripwire from "the result is negative" to
/// "this write carried the balance downward into the negative", so that a
/// partial recovery is expressible. This pins the half that must NOT have
/// changed: an undeclared write that takes money away below zero is still
/// refused by Postgres, on a positive balance and on a negative one alike.
///
/// Without this, the narrowing would be indistinguishable from deleting the
/// tripwire — every other test in this file only exercises writes that go up.
#[tokio::test]
async fn the_overdraft_tripwire_still_refuses_a_write_that_takes_money_away() {
    let Some((pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "tripwire").await;
    let user = user_id(&pool, &email).await;
    let anchor = Uuid::new_v4();
    credit_purchase(
        &pool,
        user,
        Decimal::from(10),
        &format!("cs_trip_{anchor}"),
        Some(&format!("pi_trip_{anchor}")),
    )
    .await
    .expect("purchase should credit");

    // Positive -> negative, undeclared. This is the settlement-overshoot shape
    // 0003 and 0009 exist to catch.
    let overdraft = query("UPDATE users SET credit_balance_usd = -5 WHERE id = $1")
        .bind(user)
        .execute(&pool)
        .await;
    assert!(
        overdraft.is_err(),
        "an undeclared write must not drive a positive balance below zero"
    );
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::from(10)
    );

    // Now take the account genuinely negative through the reversal path, and
    // prove that an undeclared write may still not make the debt DEEPER.
    seed_stale_reversal(&pool, user, 30, 90, &anchor.to_string()).await;
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::from(-20)
    );
    let deeper =
        query("UPDATE users SET credit_balance_usd = credit_balance_usd - 1 WHERE id = $1")
            .bind(user)
            .execute(&pool)
            .await;
    assert!(
        deeper.is_err(),
        "an undeclared write must not deepen an existing receivable"
    );

    // The one write the narrowing newly permits: strictly upward, still
    // negative. This is what `record_recovery` does.
    let upward =
        query("UPDATE users SET credit_balance_usd = credit_balance_usd + 1 WHERE id = $1")
            .bind(user)
            .execute(&pool)
            .await;
    assert!(
        upward.is_ok(),
        "paying a receivable down is not an overdraft: {upward:?}"
    );
    assert_eq!(
        balance(&pool, user).await.expect("balance should read"),
        Decimal::from(-19)
    );
}

/// The mode flags are mutually exclusive and one is required: a "resolve" that
/// defaulted to a direction would forgive a debt an operator meant to collect.
#[tokio::test]
async fn resolve_requires_exactly_one_mode_and_a_note() {
    let Some((_pool, database_url)) = connect().await else {
        return;
    };
    let email = seed_user(&database_url, "modes").await;

    let neither = run_admin(
        &database_url,
        &[
            "disputes", "resolve", "--email", &email, "--note", "nothing",
        ],
    );
    assert!(!neither.status.success(), "no mode must be refused");

    let both = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--write-off",
            "--recover",
            "10",
            "--note",
            "both",
        ],
    );
    assert!(!both.status.success(), "two modes must be refused");

    let noteless = run_admin(
        &database_url,
        &["disputes", "resolve", "--email", &email, "--write-off"],
    );
    assert!(
        !noteless.status.success(),
        "a resolution with no stated reason must be refused"
    );

    let blank_note = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--write-off",
            "--note",
            "   ",
        ],
    );
    assert!(
        !blank_note.status.success(),
        "a whitespace note is no reason at all"
    );

    let oversized = run_admin(
        &database_url,
        &[
            "disputes",
            "resolve",
            "--email",
            &email,
            "--recover",
            "10001",
            "--note",
            "fat finger",
        ],
    );
    assert!(
        !oversized.status.success(),
        "the fat-finger bound should refuse five digits"
    );
}

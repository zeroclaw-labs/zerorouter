//! Prepaid-credit integration tests: purchase idempotency, settlement debits,
//! credit-gated admission, and tenant scoping of the ledger.
//!
//! Gated on `DATABASE_URL` like `tests/postgres.rs`: when unset each test
//! returns early (skips) instead of failing.

use std::str::FromStr;

use rust_decimal::Decimal;
use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::provider::ModelRates;
use zerorouter::{
    auth::{AuthenticatedKey, generate_api_key, hash_api_key},
    billing::{
        CheckoutIntent, CreditOutcome, ReversalOutcome, balance, checkout_intent, credit_purchase,
        grant_promo, ledger_entries, record_checkout_intent, reverse_purchase,
        settle_checkout_intent,
    },
    db::{
        ByokReservation, LEARNED_SIZING_CONCURRENCY_LIMIT, MeteringLane, RequestTelemetry,
        ReservationBasis, ReservationRelease, ReservationSize, ReservationSizing, UsageAdmission,
        UsageRecord, UsageSession, begin_usage_session, migrate, quarantined_settlements,
        recover_owed_settlements, recover_quarantined_settlement, release_quarantined_reservation,
    },
    openai::{OpenAiUsage, TASK_SIGNATURE_SCHEME, TaskSignature, tool_names_digest},
    priority::Priority,
};

/// The pre-Stage-4 sizing: one measured bound, offered as the full ceiling
/// with no learned alternative for admission to choose between.
fn cold_sizing(total_tokens: i64, output_tokens: i64, cost_usd: Decimal) -> ReservationSizing {
    ReservationSizing::cold(ReservationSize {
        total_tokens,
        output_tokens,
        cost_usd,
    })
}

/// A fixed segment key for tests that only need the reservation to carry one.
fn test_signature() -> TaskSignature {
    TaskSignature {
        hex: "0123456789abcdef".to_owned(),
        scheme: TASK_SIGNATURE_SCHEME,
        tool_names_sha256: tool_names_digest(&[]),
    }
}

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

async fn create_user(pool: &PgPool, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    let email = format!("billing-{label}-{user_id}@example.invalid");
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(email)
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

async fn create_key(pool: &PgPool, user_id: Uuid) -> AuthenticatedKey {
    let key_id = Uuid::new_v4();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'billing-integration', 1000, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .execute(pool)
    .await
    .expect("test API key must insert");
    AuthenticatedKey {
        id: key_id,
        user_id,
        default_priority: None,
    }
}

fn unique_session_id() -> String {
    format!("cs_test_{}", Uuid::new_v4().simple())
}

fn usage_record(cost_usd: Decimal) -> UsageRecord {
    UsageRecord {
        tier: "zero/test".to_owned(),
        upstream_provider: "test".to_owned(),
        upstream_model: "test/model".to_owned(),
        usage: OpenAiUsage {
            prompt_tokens: 100,
            completion_tokens: 25,
            total_tokens: 125,
            prompt_tokens_details: None,
        },
        cost_usd,
        byok_catalog_usd: None,
        latency_ms: 10,
        status: 200,
        telemetry: RequestTelemetry {
            requested_max_tokens: 4096,
            stream: false,
            prompt_bytes: 128,
            message_count: 1,
            tool_count: 0,
            candidate_id: None,
            basis_rates: None,
            sell_rates: ModelRates {
                input_per_mtok: Some(2.0),
                output_per_mtok: Some(10.0),
                cached_input_per_mtok: Some(0.2),
            },
            finish_reason: None,
            finish_reason_source: None,
            usage_gap: None,
            shape_ok: None,
            priority: Some(Priority::Balanced),
            // `None`, not `Some(false)`: these fixtures describe requests from before
            // BYOK existed, so they keep pinning the pre-BYOK settled row exactly, and
            // they exercise the NULL arm of the new column while they are at it.
            byok: None,
        },
        attempts: Vec::new(),
    }
}

#[tokio::test]
async fn duplicate_stripe_sessions_credit_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "purchase").await;
    let session_id = unique_session_id();

    let first = credit_purchase(&pool, user_id, Decimal::TEN, &session_id, Some("pi_test"))
        .await
        .expect("first purchase must apply");
    assert_eq!(
        first,
        CreditOutcome::Applied {
            balance_after: Decimal::TEN
        }
    );
    let replay = credit_purchase(&pool, user_id, Decimal::TEN, &session_id, Some("pi_test"))
        .await
        .expect("replayed purchase must query");
    assert_eq!(replay, CreditOutcome::AlreadyApplied);

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::TEN
    );
    let entries = ledger_entries(&pool, user_id, 10)
        .await
        .expect("ledger must query");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].entry_type, "purchase");
    assert_eq!(entries[0].amount_usd, Decimal::TEN);
    assert_eq!(entries[0].balance_after_usd, Decimal::TEN);

    assert!(
        credit_purchase(&pool, user_id, Decimal::ZERO, &unique_session_id(), None)
            .await
            .is_err(),
        "non-positive purchase amounts must be rejected"
    );
}

#[tokio::test]
async fn checkout_intents_are_immutable_quotes_that_settle_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "intent").await;
    let session_id = unique_session_id();
    assert_eq!(
        checkout_intent(&pool, &session_id)
            .await
            .expect("missing intent must query"),
        None,
        "a session ZeroRouter never priced has no record"
    );

    record_checkout_intent(&pool, &session_id, user_id, 2_500, Decimal::from(25), "usd")
        .await
        .expect("intent must insert");
    let stored = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist");
    assert_eq!(
        stored,
        CheckoutIntent {
            stripe_session_id: session_id.clone(),
            user_id,
            expected_amount_cents: 2_500,
            expected_credit_usd: Decimal::from(25),
            currency: "usd".to_owned(),
            settled_at: None,
        }
    );

    // Settlement advances exactly once; the second call is the replay path and
    // must be a silent no-op rather than an error or a re-stamp.
    assert!(
        settle_checkout_intent(&pool, &session_id)
            .await
            .expect("settle must query"),
        "the first settle stamps the record"
    );
    let settled_at = checkout_intent(&pool, &session_id)
        .await
        .expect("intent must query")
        .expect("intent must exist")
        .settled_at
        .expect("settled record must carry a timestamp");
    assert!(
        !settle_checkout_intent(&pool, &session_id)
            .await
            .expect("replayed settle must query"),
        "a replayed settle stamps nothing"
    );
    assert_eq!(
        checkout_intent(&pool, &session_id)
            .await
            .expect("intent must query")
            .expect("intent must exist")
            .settled_at,
        Some(settled_at),
        "the settlement timestamp is final"
    );

    // The quote itself is immutable and the row cannot be removed: rewriting a
    // pending record is equivalent to rewriting the price of a sale, and
    // deleting one erases the evidence that a payment was ever quoted.
    for statement in [
        "UPDATE stripe_checkout_intents SET expected_amount_cents = 100000, \
         expected_credit_usd = 1000 WHERE stripe_session_id = $1",
        "UPDATE stripe_checkout_intents SET user_id = gen_random_uuid() \
         WHERE stripe_session_id = $1",
        "UPDATE stripe_checkout_intents SET settled_at = NULL WHERE stripe_session_id = $1",
        "DELETE FROM stripe_checkout_intents WHERE stripe_session_id = $1",
    ] {
        assert!(
            query(statement)
                .bind(&session_id)
                .execute(&pool)
                .await
                .is_err(),
            "{statement} must be rejected"
        );
    }
    // A duplicate session id is a collision on Stripe's unique id — something
    // other than create_checkout wrote it; never silently overwrite.
    assert!(
        record_checkout_intent(&pool, &session_id, user_id, 2_500, Decimal::from(25), "usd")
            .await
            .is_err(),
        "a second record for the same session must be rejected"
    );
}

#[tokio::test]
async fn usage_settlement_debits_the_balance_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "settle").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = match begin_usage_session(
        &pool,
        &key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        true,
        MeteringLane::Reserved,
    )
    .await
    .expect("funded admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("funded admission should be allowed"),
    };
    let request_id = session.request_id();
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("settlement must succeed");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(9)
    );
    let entries = ledger_entries(&pool, user_id, 10)
        .await
        .expect("ledger must query");
    let usage_rows = entries
        .iter()
        .filter(|entry| entry.entry_type == "usage")
        .collect::<Vec<_>>();
    assert_eq!(usage_rows.len(), 1, "settlement debits exactly once");
    assert_eq!(usage_rows[0].amount_usd, Decimal::NEGATIVE_ONE);
    assert_eq!(usage_rows[0].balance_after_usd, Decimal::from(9));

    let ledger_request_id = query_scalar::<_, Uuid>(
        "SELECT request_id FROM credit_ledger WHERE user_id = $1 AND entry_type = 'usage'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("usage ledger request id must query");
    assert_eq!(
        format!("chatcmpl-{}", ledger_request_id.simple()),
        request_id
    );
    let event_request_id =
        query_scalar::<_, Uuid>("SELECT request_id FROM usage_events WHERE api_key_id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .expect("usage event request id must query");
    assert_eq!(ledger_request_id, event_request_id);
}

#[tokio::test]
async fn credit_admission_fails_closed_and_cannot_jointly_overdraw() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "admission").await;
    let key_a = create_key(&pool, user_id).await;
    let key_b = create_key(&pool, user_id).await;
    grant_promo(&pool, user_id, Decimal::ONE, "starter")
        .await
        .expect("starter promo must apply");

    assert!(matches!(
        begin_usage_session(
            &pool,
            &key_a,
            cold_sizing(100, 50, Decimal::from(2)),
            ByokReservation::default(),
            test_signature(),
            true,
            MeteringLane::Reserved
        )
        .await
        .expect("underfunded admission must query"),
        UsageAdmission::InsufficientCredits
    ));
    // Cap-only mode still admits the same reservation; settle it zero-cost so
    // it neither debits the balance nor lingers as an active reservation.
    match begin_usage_session(
        &pool,
        &key_a,
        cold_sizing(100, 50, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        false,
        MeteringLane::Reserved,
    )
    .await
    .expect("cap-only admission must query")
    {
        UsageAdmission::Allowed(session) => session
            .record(&usage_record(Decimal::ZERO))
            .await
            .expect("zero-cost settlement must succeed"),
        _ => panic!("cap-only admission should be allowed"),
    }
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ONE,
        "zero-cost settlement must not touch the balance"
    );

    // Balance 3 covers ONE reservation of cost 2; concurrent admissions
    // through two different keys of the same user must not jointly overdraw.
    grant_promo(&pool, user_id, Decimal::from(2), "top-up")
        .await
        .expect("top-up promo must apply");
    let (first, second) = tokio::join!(
        begin_usage_session(
            &pool,
            &key_a,
            cold_sizing(100, 50, Decimal::from(2)),
            ByokReservation::default(),
            test_signature(),
            true,
            MeteringLane::Reserved
        ),
        begin_usage_session(
            &pool,
            &key_b,
            cold_sizing(100, 50, Decimal::from(2)),
            ByokReservation::default(),
            test_signature(),
            true,
            MeteringLane::Reserved
        ),
    );
    let mut admitted = 0;
    let mut rejected = 0;
    for admission in [first, second] {
        match admission.expect("concurrent admission must query") {
            UsageAdmission::Allowed(session) => {
                admitted += 1;
                session
                    .record(&usage_record(Decimal::from(2)))
                    .await
                    .expect("admitted reservation must settle");
            }
            UsageAdmission::InsufficientCredits => rejected += 1,
            _ => panic!("unexpected concurrent admission result"),
        }
    }
    assert_eq!((admitted, rejected), (1, 1));
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ONE
    );
}

#[tokio::test]
async fn zero_promo_grants_write_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "promo-zero").await;
    grant_promo(&pool, user_id, Decimal::ZERO, "no-op")
        .await
        .expect("zero promo must be a no-op");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO
    );
    assert!(
        ledger_entries(&pool, user_id, 10)
            .await
            .expect("ledger must query")
            .is_empty()
    );
}

/// The deposit fee is charge-side only. A promo grant has no Stripe charge, so
/// the fee must never touch it: the full amount is credited and the ledger row
/// records exactly that, with no wedge between charged and credited.
#[tokio::test]
async fn promo_grant_credits_the_full_amount_with_no_fee() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "promo-fee").await;
    // A round-number grant makes any accidental 5.5% wedge glaring: a fee would
    // credit less than $100, or bill a phantom gross above it.
    grant_promo(&pool, user_id, Decimal::from(100), "grant")
        .await
        .expect("promo must apply");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(100),
        "the full promo amount is credited; no fee is deducted"
    );
    let entries = ledger_entries(&pool, user_id, 10)
        .await
        .expect("ledger must query");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].entry_type, "promo");
    assert_eq!(
        entries[0].amount_usd,
        Decimal::from(100),
        "the ledger records the full grant, not a net-of-fee figure"
    );
}

/// Refunds/disputes reverse the NET credit only. The fee is charge-side and
/// never entered the ledger, so a full reversal claws back exactly what was
/// credited (non-refundable fee) and the balance math is exact. This pins
/// `reverse_purchase`, which the deposit-fee change deliberately does NOT touch.
#[tokio::test]
async fn refund_reverses_the_net_credit_only() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "refund-net").await;
    let session_id = unique_session_id();
    let payment_intent = format!("pi_test_{}", Uuid::new_v4().simple());
    // The user bought $25 of credit (they were charged $26.38 gross, but only
    // the $25 net ever reached the ledger).
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(25),
        &session_id,
        Some(&payment_intent),
    )
    .await
    .expect("purchase must credit the net");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );

    let outcome = reverse_purchase(
        &pool,
        &payment_intent,
        &format!("re_test_{}", Uuid::new_v4().simple()),
        "customer refund",
    )
    .await
    .expect("reversal must apply");
    // The reversal equals the NET credit — not the gross the card was charged.
    assert_eq!(
        outcome,
        ReversalOutcome::Reversed {
            amount_usd: Decimal::from(25),
            balance_after: Decimal::ZERO,
        },
        "the reversal claws back exactly the net credit; the fee is non-refundable"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "balance returns exactly to zero"
    );
}

#[tokio::test]
async fn ledger_entries_are_scoped_to_their_user_and_newest_first() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_a = create_user(&pool, "ledger-a").await;
    let user_b = create_user(&pool, "ledger-b").await;
    grant_promo(&pool, user_a, Decimal::from(5), "welcome")
        .await
        .expect("promo for user A must apply");
    credit_purchase(&pool, user_a, Decimal::from(7), &unique_session_id(), None)
        .await
        .expect("purchase for user A must apply");
    grant_promo(&pool, user_b, Decimal::from(3), "other tenant")
        .await
        .expect("promo for user B must apply");

    let entries_a = ledger_entries(&pool, user_a, 10)
        .await
        .expect("user A ledger must query");
    assert_eq!(entries_a.len(), 2);
    assert_eq!(entries_a[0].entry_type, "purchase", "newest entry first");
    assert_eq!(entries_a[0].balance_after_usd, Decimal::from(12));
    assert_eq!(entries_a[1].entry_type, "promo");
    assert_eq!(entries_a[1].note.as_deref(), Some("welcome"));

    let entries_b = ledger_entries(&pool, user_b, 10)
        .await
        .expect("user B ledger must query");
    assert_eq!(entries_b.len(), 1);
    assert_eq!(entries_b[0].amount_usd, Decimal::from(3));
    assert_eq!(entries_b[0].note.as_deref(), Some("other tenant"));

    let limited = ledger_entries(&pool, user_a, 1)
        .await
        .expect("limited ledger must query");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].entry_type, "purchase");
}

#[tokio::test]
async fn settlement_debit_is_clamped_to_the_reservation_and_cannot_overdraw() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "clamp").await;
    let key = create_key(&pool, user_id).await;
    // Fund exactly the reserved amount, then have actual usage exceed it.
    credit_purchase(&pool, user_id, Decimal::from(2), &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = match begin_usage_session(
        &pool,
        &key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        true,
        MeteringLane::Reserved,
    )
    .await
    .expect("funded admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("funded admission should be allowed"),
    };
    // Actual settled cost (5) exceeds the reservation (2): the debit must clamp
    // to 2 so the balance lands at exactly zero, never negative.
    session
        .record(&usage_record(Decimal::from(5)))
        .await
        .expect("settlement must succeed even when actual usage exceeds the reservation");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "settlement must clamp to the reservation and never overdraw"
    );
    let entries = ledger_entries(&pool, user_id, 10)
        .await
        .expect("ledger must query");
    let usage_row = entries
        .iter()
        .find(|entry| entry.entry_type == "usage")
        .expect("a usage ledger row must exist");
    assert_eq!(usage_row.amount_usd, -Decimal::from(2));
    assert_eq!(usage_row.balance_after_usd, Decimal::ZERO);
}

#[tokio::test]
async fn cap_only_settlement_records_usage_without_touching_the_balance() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "caponly").await;
    let key = create_key(&pool, user_id).await;
    // require_credits = false: no balance is funded, and settlement must record
    // the usage event for metering without moving money below zero.
    let session = match begin_usage_session(
        &pool,
        &key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        false,
        MeteringLane::Reserved,
    )
    .await
    .expect("cap-only admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("cap-only admission should be allowed"),
    };
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("cap-only settlement must succeed");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "cap-only settlement must not move the balance"
    );
    let usage_events =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_events WHERE api_key_id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .expect("usage event count must query");
    assert_eq!(usage_events, 1, "usage is still metered in cap-only mode");
    let usage_ledger = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'usage'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("usage ledger count must query");
    assert_eq!(usage_ledger, 0, "cap-only mode writes no money ledger row");
}

// ============================================================================
// Settlement durability (migration 0006).
//
// Settlement used to be attempted exactly once, with the payload in memory and
// the handle consumed by the attempt, so any failure after that point lost the
// charge entirely and the reservation was deleted unbilled at expiry. These
// tests pin the three properties that replaced it: a transient failure is
// retried and still bills once, a permanent failure leaves a replayable record
// instead of nothing, and a retry against an already-committed settle bills
// nothing further.
//
// Failures are injected with a BEFORE INSERT trigger on `usage_events` that is
// scoped to one `request_id` — every other request, including concurrently
// running tests, passes straight through. The counter is a SEQUENCE because
// `nextval` survives the rollback the injected exception causes; an ordinary
// counter row would be rolled back with it and every attempt would fail
// identically.
// ============================================================================

/// A settle fault injector installed on `usage_events`, dropped by [`Self::remove`].
struct SettleFault {
    name: String,
}

/// Serializes the fault harness. `SettleFault` installs and drops a TRIGGER
/// on the shared `usage_events` table, and concurrent DDL there contends for
/// an ACCESS EXCLUSIVE lock — two fault-using tests running side by side
/// made each other's teardown fail. Every fault user holds this for the
/// lifetime of its fault.
static FAULT_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Serializes admission's GLOBAL quarantine sweep against the two tests that
/// depend on a row *staying* unquarantined across their own ageing/recovery
/// window. Every admission (`begin_usage_session`) sweeps aged owed
/// reservations catalog-wide, so a sibling test's admission can quarantine
/// this binary's owed-reservation fixture between `age_settlement_intent` and
/// `recover_owed_settlements` — recovery skips quarantined rows, and the test
/// fails on a behaviour that is correct in production. Admission-calling
/// tests take `read()` (they parallelize freely among themselves); the two
/// sweep-sensitive tests take `write()` (no admission runs concurrently).
///
/// Lock order, always: `FAULT_LOCK` first, then `SWEEP_LOCK`. Several tests
/// hold both, and a consistent order is what keeps two static locks from
/// deadlocking.
static SWEEP_LOCK: std::sync::LazyLock<tokio::sync::RwLock<()>> =
    std::sync::LazyLock::new(|| tokio::sync::RwLock::new(()));

impl SettleFault {
    /// Fail the first `failures` settle INSERTs for `request_id` with
    /// `errcode`, then let them through.
    async fn install(pool: &PgPool, request_id: Uuid, failures: i64, errcode: &str) -> Self {
        let name = format!("zr_settle_fault_{}", Uuid::new_v4().simple());
        query(&format!("CREATE SEQUENCE {name}"))
            .execute(pool)
            .await
            .expect("fault sequence must create");
        // Nested IFs, not `AND`: Postgres does not promise left-to-right
        // evaluation, so a single condition could burn the sequence on another
        // test's insert.
        query(&format!(
            r#"
            CREATE FUNCTION {name}() RETURNS TRIGGER LANGUAGE plpgsql AS $fault$
            BEGIN
                IF NEW.request_id = '{request_id}'::UUID THEN
                    IF nextval('{name}') <= {failures} THEN
                        RAISE EXCEPTION 'injected settle failure'
                            USING ERRCODE = '{errcode}';
                    END IF;
                END IF;
                RETURN NEW;
            END;
            $fault$
            "#
        ))
        .execute(pool)
        .await
        .expect("fault function must create");
        query(&format!(
            "CREATE TRIGGER {name} BEFORE INSERT ON usage_events \
             FOR EACH ROW EXECUTE FUNCTION {name}()"
        ))
        .execute(pool)
        .await
        .expect("fault trigger must create");
        Self { name }
    }

    /// How many settle INSERTs this fault has seen for its request.
    async fn insert_attempts(&self, pool: &PgPool) -> i64 {
        query_scalar::<_, i64>(&format!(
            "SELECT last_value FROM {} WHERE is_called",
            self.name
        ))
        .fetch_optional(pool)
        .await
        .expect("fault sequence must query")
        .unwrap_or(0)
    }

    async fn remove(self, pool: &PgPool) {
        let name = self.name;
        for statement in [
            format!("DROP TRIGGER {name} ON usage_events"),
            format!("DROP FUNCTION {name}()"),
            format!("DROP SEQUENCE {name}"),
        ] {
            query(&statement)
                .execute(pool)
                .await
                .expect("fault teardown must succeed");
        }
    }
}

/// `(reservations, settlement_intents, quarantined, settle_attempts)` for a key.
async fn reservation_state(pool: &PgPool, api_key_id: Uuid) -> (i64, i64, i64, i64) {
    query_as::<_, (i64, i64, i64, i64)>(
        r#"
        SELECT
            COUNT(*),
            COUNT(*) FILTER (WHERE settlement_intent IS NOT NULL),
            COUNT(*) FILTER (WHERE quarantined_at IS NOT NULL),
            COALESCE(SUM(settle_attempts), 0)::BIGINT
        FROM usage_reservations
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .expect("reservation state must query")
}

async fn settled_rows(pool: &PgPool, api_key_id: Uuid) -> i64 {
    query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_events WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("settled row count must query")
}

async fn usage_debits(pool: &PgPool, user_id: Uuid) -> i64 {
    query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'usage'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("usage ledger count must query")
}

async fn admit(pool: &PgPool, key: &AuthenticatedKey, require_credits: bool) -> UsageSession {
    match begin_usage_session(
        pool,
        key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        require_credits,
        MeteringLane::Reserved,
    )
    .await
    .expect("admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("the test request should be admitted"),
    }
}

/// Age a reservation's intent past the recovery sweep's grace period, leaving
/// `expires_at` in the future so a concurrent test's admission sweep cannot
/// quarantine the row out from under the assertions.
///
/// `created_at` shifts by the same interval rather than being rewritten from
/// `NOW()`: `settlement_intent_at >= created_at` is a CHECK, and only a uniform
/// shift preserves it.
async fn age_settlement_intent(pool: &PgPool, api_key_id: Uuid) {
    query(
        r#"
        UPDATE usage_reservations
        SET created_at = created_at - INTERVAL '30 minutes',
            settlement_intent_at = settlement_intent_at - INTERVAL '30 minutes'
        WHERE api_key_id = $1
        "#,
    )
    .bind(api_key_id)
    .execute(pool)
    .await
    .expect("reservation ageing must apply");
}

/// A settle that fails on something momentary — a lost connection, a lock
/// timeout, a serialization failure — is retried inside the request, and the
/// customer is billed once, not once per attempt. The rollback that follows the
/// failed attempt restores the reservation, so the retry is the first
/// transaction to consume it and the `DELETE ... RETURNING` still gates the
/// debit exactly once.
#[tokio::test]
async fn a_transiently_failing_settlement_is_retried_and_bills_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "settle-transient").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let request_id = request_uuid(&session);
    // 40001 (serialization_failure) is in the transient set, so the settle is
    // retried rather than abandoned.
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.read().await;
    let fault = SettleFault::install(&pool, request_id, 1, "40001").await;
    let outcome = session.record(&usage_record(Decimal::ONE)).await;
    let insert_attempts = fault.insert_attempts(&pool).await;
    fault.remove(&pool).await;

    outcome.expect("the retry must settle the request");
    assert_eq!(
        insert_attempts, 2,
        "the injected failure must have cost exactly one extra settle attempt"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(9),
        "the retried settle debits the cost once, not once per attempt"
    );
    assert_eq!(usage_debits(&pool, user_id).await, 1);
    assert_eq!(settled_rows(&pool, key.id).await, 1);
    assert_eq!(
        reservation_state(&pool, key.id).await.0,
        0,
        "the successful attempt consumed the reservation"
    );
}

/// The ambiguous COMMIT, reproduced exactly: the settle committed but its
/// caller never learned that it did, so it settles again. The second attempt
/// finds no reservation to consume, finds the settled row that proves the work
/// is done, and reports success without touching the balance a second time.
#[tokio::test]
async fn a_retry_after_an_ambiguous_commit_does_not_double_debit() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "settle-ambiguous").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("the first settle must succeed");
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("a settle whose work is already done must report success");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(9),
        "the balance moved exactly once across two settle attempts"
    );
    assert_eq!(usage_debits(&pool, user_id).await, 1);
    assert_eq!(settled_rows(&pool, key.id).await, 1);
    assert_eq!(reservation_state(&pool, key.id).await.0, 0);
}

/// The torn state a retry can land in: the settled row is already there while
/// the reservation somehow still is too. The metering INSERT then fails on the
/// UNIQUE `request_id`, and reading that as an error would strand the
/// reservation forever while telling the caller the request could not be
/// metered. It is read as success instead — the customer has exactly one
/// settled row, which is the whole point of the unique index — and the orphaned
/// reservation is reclaimed under a guard that requires the settled row to
/// exist.
#[tokio::test]
async fn a_duplicate_settled_row_is_success_and_reclaims_the_reservation() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "settle-duplicate").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let request_id = request_uuid(&session);
    // Stand in for a settle that committed its row while its reservation
    // survived: the state a duplicate-key error actually reports.
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd, latency_ms, status
        )
        VALUES ($1, $2, 'zero/test', 'test', 'test/model', 100, 0, 25, 1, 10, 200)
        "#,
    )
    .bind(request_id)
    .bind(key.id)
    .execute(&pool)
    .await
    .expect("the pre-existing settled row must insert");

    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("a duplicate settled row is success, not a metering failure");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::TEN,
        "the settled row was already there, so this attempt debits nothing"
    );
    assert_eq!(usage_debits(&pool, user_id).await, 0);
    assert_eq!(settled_rows(&pool, key.id).await, 1);
    assert_eq!(
        reservation_state(&pool, key.id).await.0,
        0,
        "the orphaned reservation is reclaimed rather than left to expire"
    );
}

/// A settle that cannot succeed no matter how often it is tried leaves the
/// charge recoverable rather than losing it. The payload is on the reservation
/// row before the first attempt runs, the request gives up immediately (the
/// failure is not transient, so retrying only delays the caller), and the
/// recovery sweep bills it later — for exactly the amount the original settle
/// would have billed.
#[tokio::test]
async fn a_permanently_failing_settlement_is_recoverable_and_bills_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "settle-permanent").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let request_id = request_uuid(&session);
    // P0001 is a plain `RAISE EXCEPTION`: nothing about it clears on a retry,
    // which is exactly how a CHECK violation or a trigger rejection presents.
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.read().await;
    let fault = SettleFault::install(&pool, request_id, i64::MAX, "P0001").await;
    assert!(
        session.record(&usage_record(Decimal::ONE)).await.is_err(),
        "a permanently failing settle must be reported to the caller"
    );

    // Nothing was billed, and nothing was lost: the intent is on the row.
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::TEN
    );
    assert_eq!(settled_rows(&pool, key.id).await, 0);
    let (reservations, intents, quarantined, attempts) = reservation_state(&pool, key.id).await;
    assert_eq!(
        reservations, 1,
        "the reservation survives the failed settle"
    );
    assert_eq!(intents, 1, "carrying the payload needed to replay it");
    assert_eq!(quarantined, 0);
    assert_eq!(
        attempts, 1,
        "a permanent failure is not retried inside the request"
    );

    // A fresh intent is off limits to the sweep: the request may still be
    // working through its own retries. The sweep is global and other tests run
    // beside this one, so the evidence is this row's own state, never the
    // summary counters.
    recover_owed_settlements(&pool, 100)
        .await
        .expect("recovery must query");
    assert_eq!(
        reservation_state(&pool, key.id).await,
        (1, 1, 0, 1),
        "an intent younger than the grace period is not touched by the sweep"
    );

    age_settlement_intent(&pool, key.id).await;
    recover_owed_settlements(&pool, 100)
        .await
        .expect("recovery must query");
    assert_eq!(
        reservation_state(&pool, key.id).await,
        (1, 1, 0, 2),
        "the fault is still installed, so the replay fails and is counted"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::TEN,
        "a failed replay moves no money"
    );

    fault.remove(&pool).await;
    let recovered = recover_owed_settlements(&pool, 100)
        .await
        .expect("recovery must query");
    assert!(recovered.settled >= 1, "{recovered:?}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(9),
        "recovery bills exactly what the original settle would have billed"
    );
    assert_eq!(usage_debits(&pool, user_id).await, 1);
    assert_eq!(settled_rows(&pool, key.id).await, 1);
    assert_eq!(
        reservation_state(&pool, key.id).await.0,
        0,
        "the recovered settle consumed the reservation"
    );
}

/// The admission sweep used to DELETE every expired reservation. One that still
/// owes a settlement is now quarantined instead, so delivered inference that
/// could not be billed automatically ends up in an operator queue rather than
/// erased. A reservation that owes nothing is still reclaimed exactly as before.
#[tokio::test]
async fn an_expired_reservation_owing_a_settlement_is_quarantined_not_deleted() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "settle-quarantine").await;
    let owed_key = create_key(&pool, user_id).await;
    let idle_key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    // One reservation that owes a settlement...
    let session = admit(&pool, &owed_key, true).await;
    let request_id = request_uuid(&session);
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.write().await;
    let fault = SettleFault::install(&pool, request_id, i64::MAX, "P0001").await;
    assert!(session.record(&usage_record(Decimal::ONE)).await.is_err());
    fault.remove(&pool).await;
    // ...and one that was admitted and never settled, which owes nothing.
    let _abandoned = admit(&pool, &idle_key, true).await;

    // Every timestamp shifts by the same interval so the row's ordering CHECKs
    // survive; a 30-minute shift puts the 20-minute TTL 10 minutes in the past.
    query(
        r#"
        UPDATE usage_reservations
        SET created_at = created_at - INTERVAL '30 minutes',
            expires_at = expires_at - INTERVAL '30 minutes',
            settlement_intent_at = settlement_intent_at - INTERVAL '30 minutes'
        WHERE api_key_id = $1 OR api_key_id = $2
        "#,
    )
    .bind(owed_key.id)
    .bind(idle_key.id)
    .execute(&pool)
    .await
    .expect("reservation expiry must apply");

    // Any admission runs the sweep.
    let _sweeper = admit(&pool, &owed_key, true).await;

    let (reservations, intents, quarantined, _) = reservation_state(&pool, owed_key.id).await;
    assert_eq!(
        (reservations, intents, quarantined),
        (2, 1, 1),
        "the owed reservation is kept and quarantined; only the sweeping \
         admission's own fresh reservation joins it"
    );
    assert_eq!(
        reservation_state(&pool, idle_key.id).await.0,
        0,
        "an expired reservation owing nothing is still reclaimed"
    );

    let owed = quarantined_settlements(&pool, 100)
        .await
        .expect("quarantine must query");
    let entry = owed
        .iter()
        .find(|entry| entry.request_id == request_id)
        .expect("the unsettled request must be listed for reconciliation");
    assert_eq!(entry.api_key_id, owed_key.id);
    assert_eq!(
        entry.owed_cost_usd,
        Some(Decimal::ONE),
        "the queue states what the customer was never billed"
    );
    assert_eq!(entry.reserved_cost_usd, Decimal::from(2));
    assert!(entry.settle_attempts >= 1);
    assert!(entry.last_settle_error.is_some());
}

/// Age one reservation past its TTL and state the two facts the expiry sweep
/// classifies on: whether the walk ever dispatched, and whether a settlement
/// is owed.
///
/// The row itself comes from a real admission, so every other column is what
/// admission writes; only the three timestamps and the payload are stated. All
/// of them land two hours back together, which keeps the row's ordering CHECKs
/// (`expires_at > created_at`, `settlement_intent_at >= created_at`) satisfied
/// exactly as a genuinely old row would.
///
/// The intent payload is a stand-in: the sweep classifies on the column being
/// non-NULL and never reads it, and a test that needs a REPLAYABLE payload
/// gets one from a real failed settle instead (see the fault-injection tests).
async fn expire_reservation(pool: &PgPool, reservation_id: Uuid, dispatched: bool, owes: bool) {
    query(
        r#"
        UPDATE usage_reservations
        SET created_at = NOW() - INTERVAL '2 hours',
            expires_at = NOW() - INTERVAL '100 minutes',
            dispatched_at = CASE WHEN $2 THEN NOW() - INTERVAL '110 minutes' END,
            settlement_intent = CASE WHEN $3 THEN '{"version": 1}'::JSONB END,
            settlement_intent_at = CASE WHEN $3 THEN NOW() - INTERVAL '110 minutes' END
        WHERE id = $1
        "#,
    )
    .bind(reservation_id)
    .bind(dispatched)
    .bind(owes)
    .execute(pool)
    .await
    .expect("reservation ageing must apply");
}

/// `(survives, quarantined, last_settle_error)` for one reservation.
async fn swept_state(pool: &PgPool, reservation_id: Uuid) -> (bool, bool, Option<String>) {
    let row = query_as::<_, (bool, Option<String>)>(
        "SELECT quarantined_at IS NOT NULL, last_settle_error
         FROM usage_reservations WHERE id = $1",
    )
    .bind(reservation_id)
    .fetch_optional(pool)
    .await
    .expect("reservation state must query");
    row.map_or((false, false, None), |(quarantined, error)| {
        (true, quarantined, error)
    })
}

/// The hole sol's review named, closed. A reservation whose request WAS sent
/// upstream but which holds no settlement intent — the intent write failed
/// permanently, or the process died between the answer and the intent — used
/// to be byte-identical to one whose walk never dispatched at all. The sweep
/// reclaimed it: the customer kept the tokens, the encumbrance was released,
/// and nothing anywhere recorded that anything had been owed.
///
/// `dispatched_at` is what tells the two apart, and a dispatched row is now
/// parked for an operator instead of erased. There is no payload to replay, so
/// what quarantine buys here is visibility — the row is listed with a NULL
/// owed amount and a stated reason, which is exactly the state of knowledge:
/// inference was delivered and ZeroRouter cannot say what it was worth.
#[tokio::test]
async fn an_expired_dispatched_reservation_owing_no_intent_is_quarantined_not_reclaimed() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "dispatched-intentless").await;
    let key = create_key(&pool, user_id).await;

    let lost = request_uuid(&admit(&pool, &key, false).await);
    expire_reservation(&pool, lost, true, false).await;

    // Any admission runs the sweep.
    let _sweeper = admit(&pool, &key, false).await;

    let (survives, quarantined, reason) = swept_state(&pool, lost).await;
    assert!(
        survives,
        "a reservation that was dispatched upstream must never be silently reclaimed"
    );
    assert!(
        quarantined,
        "a dispatched reservation holding no intent belongs in the operator queue"
    );
    assert!(
        reason.is_some_and(|reason| reason.contains("no settlement intent")),
        "the queue has to say why the row is parked; nothing else on it explains a NULL owed amount"
    );

    let parked = quarantined_settlements(&pool, 500)
        .await
        .expect("quarantine must query");
    let entry = parked
        .iter()
        .find(|entry| entry.request_id == lost)
        .expect("the lost charge must be listed for reconciliation");
    assert_eq!(entry.api_key_id, key.id);
    assert_eq!(
        entry.owed_cost_usd, None,
        "there is no stored payload, so the queue must not claim to know the amount"
    );
    assert_eq!(
        entry.reserved_cost_usd,
        Decimal::from(2),
        "the admission ceiling is still the one bound an operator has to work from"
    );

    // Nothing automatic may act on this row: there is no payload to replay, so
    // a recovery pass must leave it exactly where the operator can see it.
    // Asserted on the row, not on the pass's counters — the recovery sweep is
    // global and another test's owed row may legitimately be settled by it.
    recover_owed_settlements(&pool, 500)
        .await
        .expect("recovery must query");
    recover_quarantined_settlement(&pool, lost)
        .await
        .expect("operator collection must query");
    let (survives, quarantined, _) = swept_state(&pool, lost).await;
    assert!(
        survives && quarantined,
        "with no payload there is nothing to replay: neither the automatic \
         sweep nor the operator command may consume this row"
    );
    drop_reservations(&pool, key.id).await;
}

/// The sweep's three classes are a partition, and only one of them may delete.
/// Asserted in a single sweep so the classification cannot be right for one
/// row by being wrong about which class it is in.
#[tokio::test]
async fn expiry_reclaims_only_the_reservation_that_never_dispatched() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "sweep-partition").await;
    let key = create_key(&pool, user_id).await;

    // Never dispatched, owes nothing: the request never reached an upstream,
    // so there is genuinely nothing to bill and the encumbrance is free.
    let never_ran = request_uuid(&admit(&pool, &key, false).await);
    expire_reservation(&pool, never_ran, false, false).await;
    // Dispatched, owes a settlement: 0006's class, unchanged.
    let owes = request_uuid(&admit(&pool, &key, false).await);
    expire_reservation(&pool, owes, true, true).await;
    // Dispatched, owes nothing: delivered inference whose charge was lost.
    let lost = request_uuid(&admit(&pool, &key, false).await);
    expire_reservation(&pool, lost, true, false).await;

    let _sweeper = admit(&pool, &key, false).await;

    assert_eq!(
        swept_state(&pool, never_ran).await,
        (false, false, None),
        "an expired reservation that never dispatched is still reclaimed"
    );
    let (owes_survives, owes_quarantined, _) = swept_state(&pool, owes).await;
    assert!(
        owes_survives && owes_quarantined,
        "an expired reservation carrying an intent is still quarantined, not deleted"
    );
    let (lost_survives, lost_quarantined, _) = swept_state(&pool, lost).await;
    assert!(
        lost_survives && lost_quarantined,
        "an expired reservation that dispatched without an intent is quarantined, not deleted"
    );
    drop_reservations(&pool, key.id).await;
}

/// The full requested ceiling used by the concurrency-gate tests, in the three
/// units admission checks: tokens against velocity, output tokens as
/// provenance, dollars against the spend cap and the balance.
const FULL_CEILING: ReservationSize = ReservationSize {
    total_tokens: 4_000,
    output_tokens: 4_000,
    cost_usd: Decimal::from_parts(4, 0, 0, false, 0),
};

/// A request the estimator sized, offering admission both options.
///
/// The learned arm is a quarter of the ceiling on every dimension — Stage 4's
/// floor (`max(p99 x 1.25, 0.25 x requested_max)`), and therefore the largest
/// gap between what is encumbered and what may be delivered that learned
/// sizing can produce. It is the worst case the gate exists to bound, which is
/// why it is the case under test.
fn learned_sizing() -> ReservationSizing {
    ReservationSizing {
        learned: Some(ReservationSize {
            total_tokens: 1_000,
            output_tokens: 1_000,
            cost_usd: Decimal::ONE,
        }),
        full: FULL_CEILING,
    }
}

/// Offer both sizings and report which one admission took, or `None` when the
/// request was not admitted at all.
async fn admit_learned(
    pool: &PgPool,
    key: &AuthenticatedKey,
    require_credits: bool,
) -> Option<ReservationBasis> {
    match begin_usage_session(
        pool,
        key,
        learned_sizing(),
        ByokReservation::default(),
        test_signature(),
        require_credits,
        MeteringLane::Reserved,
    )
    .await
    .expect("admission must query")
    {
        UsageAdmission::Allowed(session) => Some(session.estimator_basis()),
        UsageAdmission::InsufficientCredits => None,
        other => panic!(
            "unexpected admission outcome: {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Every reservation this key holds, cheapest-first, so a test can say what was
/// encumbered rather than only how many rows exist.
async fn reserved_costs(pool: &PgPool, api_key_id: Uuid) -> Vec<Decimal> {
    query_scalar::<_, Decimal>(
        "SELECT reserved_cost_usd FROM usage_reservations WHERE api_key_id = $1
         ORDER BY reserved_cost_usd",
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await
    .expect("reserved costs must query")
}

/// The learned reservation encumbers a quarter of the ceiling that still goes
/// upstream, so concurrency multiplies the gap: four same-shape requests are
/// admitted against roughly one request's worth of balance while four
/// requests' worth of tokens may be generated (sol review #1).
///
/// The remedy is a sizing gate, not an admission gate. A user already holding
/// the limit's worth of live requests keeps getting served — the next request
/// simply encumbers the full ceiling it may actually be delivered.
#[tokio::test]
async fn learned_sizing_stops_once_the_user_holds_enough_live_reservations() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "sizing-gate").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(100),
        &unique_session_id(),
        None,
    )
    .await
    .expect("funding purchase must apply");

    // Below the limit the estimator's sizing stands: this is Stage 4 working
    // as designed, and the gate must not take it away.
    for held in 0..LEARNED_SIZING_CONCURRENCY_LIMIT {
        assert_eq!(
            admit_learned(&pool, &key, true).await,
            Some(ReservationBasis::Learned),
            "a user holding {held} live reservations is still under the limit"
        );
    }

    // At the limit the same request sizes at the full ceiling instead. Not
    // refused — the basis is what changes.
    assert_eq!(
        admit_learned(&pool, &key, true).await,
        Some(ReservationBasis::Cold),
        "at the concurrency limit the request runs, and encumbers honestly"
    );

    let mut expected: Vec<Decimal> = (0..LEARNED_SIZING_CONCURRENCY_LIMIT)
        .map(|_| Decimal::ONE)
        .collect();
    expected.push(FULL_CEILING.cost_usd);
    assert_eq!(
        reserved_costs(&pool, key.id).await,
        expected,
        "exactly the limit's worth of reservations were sized learned"
    );
    drop_reservations(&pool, key.id).await;
}

/// The gate's count is only worth anything if it cannot be read twice from the
/// same state. Two admissions launched together, with the user already holding
/// one live reservation, have exactly one learned slot left between them: if
/// the count raced, both would read "one live", both would take it, and the
/// limit would be a suggestion.
///
/// What makes it safe is that the count is read inside the transaction holding
/// this user's `pg_advisory_xact_lock`, from the same statement as the
/// encumbrance sums, and consumed by an INSERT that commits with the lock still
/// held — so the two admissions are strictly ordered and the loser sees the
/// winner's row.
#[tokio::test]
async fn two_simultaneous_admissions_cannot_both_take_the_last_learned_slot() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "sizing-race").await;
    // Two keys of one user, because the gate is per USER: a per-key count
    // would be reset by minting a second key, exactly as the spend caps were.
    let key_a = create_key(&pool, user_id).await;
    let key_b = create_key(&pool, user_id).await;
    credit_purchase(
        &pool,
        user_id,
        Decimal::from(100),
        &unique_session_id(),
        None,
    )
    .await
    .expect("funding purchase must apply");

    // Fill every learned slot but one.
    for _ in 1..LEARNED_SIZING_CONCURRENCY_LIMIT {
        assert_eq!(
            admit_learned(&pool, &key_a, true).await,
            Some(ReservationBasis::Learned)
        );
    }

    let (first, second) = tokio::join!(
        admit_learned(&pool, &key_a, true),
        admit_learned(&pool, &key_b, true),
    );
    let mut outcomes = [first, second];
    outcomes.sort_by_key(|basis| format!("{basis:?}"));
    assert_eq!(
        outcomes,
        [
            Some(ReservationBasis::Cold),
            Some(ReservationBasis::Learned)
        ],
        "the last learned slot goes to exactly one of the two; the other sizes \
         at the full ceiling"
    );
    drop_reservations(&pool, key_a.id).await;
    drop_reservations(&pool, key_b.id).await;
}

/// The overrun, priced. A balance covering exactly one full ceiling used to
/// admit four learned same-shape requests — four ceilings' worth of generation
/// against one ceiling's worth of prepaid credit. The gate bounds how many of
/// those requests may be sized learned, and the credit check does the rest:
/// once the third request has to reserve the whole ceiling, the balance it
/// would need is not there.
///
/// The bound is honest rather than absolute. Two learned reservations still
/// under-encumber by 0.75 ceilings each, so the exposure is capped at 1.5
/// ceilings — a constant, where it used to grow with whatever concurrency the
/// caller chose.
#[tokio::test]
async fn concurrent_learned_admissions_cannot_outrun_the_balance() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "sizing-overdraw").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(
        &pool,
        user_id,
        FULL_CEILING.cost_usd,
        &unique_session_id(),
        None,
    )
    .await
    .expect("funding purchase must apply");

    let (a, b, c, d) = tokio::join!(
        admit_learned(&pool, &key, true),
        admit_learned(&pool, &key, true),
        admit_learned(&pool, &key, true),
        admit_learned(&pool, &key, true),
    );
    let outcomes = [a, b, c, d];
    let learned = outcomes
        .iter()
        .filter(|basis| **basis == Some(ReservationBasis::Learned))
        .count();
    assert_eq!(
        i64::try_from(learned).expect("count fits"),
        LEARNED_SIZING_CONCURRENCY_LIMIT,
        "no more than the limit may be sized learned, however many arrive at once"
    );
    assert!(
        outcomes
            .iter()
            .all(|basis| *basis != Some(ReservationBasis::Cold)),
        "the rest could not afford the full ceiling and were refused, not \
         quietly admitted at the learned size"
    );

    let reserved: Decimal = reserved_costs(&pool, key.id).await.into_iter().sum();
    assert!(
        reserved <= FULL_CEILING.cost_usd,
        "admission never encumbers more than the balance covers: {reserved} > {}",
        FULL_CEILING.cost_usd
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        FULL_CEILING.cost_usd,
        "nothing has settled yet, so nothing is debited"
    );
    drop_reservations(&pool, key.id).await;
}

/// Remove the rows a quarantine test deliberately left behind.
///
/// Quarantine's whole contract is that nothing automatic removes these rows,
/// so a test that plants them has to clear them itself. Without this they
/// accumulate in a shared development database and crowd out later rows in
/// `quarantined_settlements`, which reads oldest-first under a caller-supplied
/// LIMIT.
async fn drop_reservations(pool: &PgPool, api_key_id: Uuid) {
    query("DELETE FROM usage_reservations WHERE api_key_id = $1")
        .bind(api_key_id)
        .execute(pool)
        .await
        .expect("test reservation cleanup must apply");
}

/// The marker the whole classification rests on. It is fire-and-forget by
/// design — the request path may not grow a round trip that can fail a request
/// — so what is pinned here is that the write lands, records the FIRST
/// dispatch, and is safe to issue on every rung of a walk.
#[tokio::test]
async fn the_dispatch_marker_records_the_first_upstream_call() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "dispatch-marker").await;
    let key = create_key(&pool, user_id).await;
    let session = admit(&pool, &key, false).await;
    let reservation_id = request_uuid(&session);

    assert_eq!(
        dispatched_at(&pool, reservation_id).await,
        None,
        "admission alone has dispatched nothing"
    );

    let marker = session.dispatch_marker();
    marker.fire();
    let first = await_dispatch_marker(&pool, reservation_id).await;

    // A walk fires the marker on every rung it dispatches to, and every rung
    // after the first must be free: no statement, no pooled connection, and
    // above all no restatement of the time. "When did this request first reach
    // an upstream" is the fact the sweep needs, and it is settled by rung one.
    marker.fire();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        dispatched_at(&pool, reservation_id).await,
        Some(first),
        "a later rung must not overwrite the first dispatch's timestamp"
    );
}

async fn dispatched_at(
    pool: &PgPool,
    reservation_id: Uuid,
) -> Option<chrono::DateTime<chrono::Utc>> {
    query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT dispatched_at FROM usage_reservations WHERE id = $1",
    )
    .bind(reservation_id)
    .fetch_one(pool)
    .await
    .expect("dispatch marker must query")
}

/// Wait for a fire-and-forget marker to land. Polled rather than awaited
/// because not awaiting it is the point: the request path never blocks on this
/// write, so a test cannot either.
async fn await_dispatch_marker(
    pool: &PgPool,
    reservation_id: Uuid,
) -> chrono::DateTime<chrono::Utc> {
    for _ in 0..200 {
        if let Some(at) = dispatched_at(pool, reservation_id).await {
            return at;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the dispatch marker never landed");
}

/// The reservation id a session will key every settled row on.
fn request_uuid(session: &UsageSession) -> Uuid {
    Uuid::parse_str(
        session
            .request_id()
            .strip_prefix("chatcmpl-")
            .expect("request id should carry the reservation"),
    )
    .expect("request id should be a uuid")
}

/// Quarantine must not be a money grave. After eight failed attempts a
/// settlement stops being retried automatically — correct, since retrying a
/// poisoned row helps nobody — but the customer already received that
/// inference, so an operator needs a path from "parked" to "collected".
/// Before this, `recover_owed_settlements` filtered quarantined rows out by
/// construction and nothing else could settle them: the debt was visible
/// and uncollectable. The collection is single-row on purpose, which is
/// also why this test can assert on its own row while other tests run
/// beside it.
#[tokio::test]
async fn a_quarantined_settlement_is_collectable_by_an_operator_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = create_user(&pool, "quarantine-recovery").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let request_id = request_uuid(&session);
    // A permanent fault, so the in-request settle stores its intent and
    // gives up; the row is then parked exactly as eight failures would
    // leave it.
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.read().await;
    let fault = SettleFault::install(&pool, request_id, i64::MAX, "P0001").await;
    assert!(session.record(&usage_record(Decimal::ONE)).await.is_err());
    query(
        "UPDATE usage_reservations
         SET quarantined_at = NOW(), settle_attempts = 8
         WHERE id = $1",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .expect("quarantine must apply");
    age_settlement_intent(&pool, key.id).await;

    // The automatic sweep must keep its hands off: quarantine means stop.
    recover_owed_settlements(&pool, 100)
        .await
        .expect("automatic recovery must query");
    assert_eq!(
        reservation_state(&pool, key.id).await,
        (1, 1, 1, 8),
        "the automatic sweep never revives a quarantined row"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::TEN,
        "and nothing is billed while it sits"
    );

    // With the fault cleared, the operator collects that one debt.
    fault.remove(&pool).await;
    let collected = recover_quarantined_settlement(&pool, request_id)
        .await
        .expect("operator collection must query");
    assert_eq!(collected.settled, 1);
    let after = balance(&pool, user_id).await.expect("balance must query");
    assert_eq!(
        Decimal::TEN - after,
        Decimal::ONE,
        "the customer is charged exactly what the stored intent owed"
    );
    assert_eq!(settled_rows(&pool, key.id).await, 1);
    assert_eq!(
        reservation_state(&pool, key.id).await.0,
        0,
        "a collected settlement leaves the queue"
    );

    // Exactly once: collecting again finds nothing and moves nothing.
    let replay = recover_quarantined_settlement(&pool, request_id)
        .await
        .expect("replay must query");
    assert_eq!(replay.settled, 0);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        after,
        "replay is a no-op"
    );
}

/// The settlement intent is the request's only safety net: the customer may
/// already hold streamed output, and a settle that fails without a stored
/// payload leaves nothing to replay — the reservation is later reclaimed as
/// owing nothing and the charge is gone. A single write attempt threw that
/// net away on a blip, so the write retries transient failures exactly like
/// the settle it protects. This pins the recovery: a fault that clears
/// after the first attempt still leaves a replayable intent.
#[tokio::test]
async fn a_transient_intent_write_failure_still_leaves_a_replayable_payload() {
    let Some(pool) = connect().await else {
        return;
    };
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.read().await;
    let user_id = create_user(&pool, "intent-retry").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    // Fail the settle permanently so the request's fate rests entirely on
    // whether the intent survived.
    let fault = SettleFault::install(&pool, request_uuid(&session), i64::MAX, "P0001").await;
    assert!(session.record(&usage_record(Decimal::ONE)).await.is_err());

    let (reservations, intents, _, _) = reservation_state(&pool, key.id).await;
    assert_eq!(reservations, 1, "the reservation survives");
    assert_eq!(
        intents, 1,
        "and carries the payload needed to replay the charge"
    );

    // The stored payload is what recovery bills, so it must be the real
    // amount rather than a placeholder.
    fault.remove(&pool).await;
    age_settlement_intent(&pool, key.id).await;
    recover_owed_settlements(&pool, 100)
        .await
        .expect("recovery must query");
    assert_eq!(
        Decimal::TEN - balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ONE,
        "the replayed charge is exactly what the intent recorded"
    );
}

/// Money the customer already received must keep encumbering their balance
/// until it is collected or written off. Before this, in-flight cost was
/// counted only while `expires_at > NOW()`, so an owed-but-expired
/// reservation quietly released the dollars it represented: the SAME dollar
/// could admit a second request, and the first debt then had nothing left
/// to collect from. Expiry may only free reservations that owe nothing.
#[tokio::test]
async fn an_owed_reservation_keeps_encumbering_the_balance_after_it_expires() {
    let Some(pool) = connect().await else {
        return;
    };
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "owed-encumbrance").await;
    let key = create_key(&pool, user_id).await;
    // Exactly one reservation's worth ($2, the helper's size), so a second
    // admission is possible only if the first one's debt stopped counting.
    credit_purchase(&pool, user_id, Decimal::from(2), &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let fault = SettleFault::install(&pool, request_uuid(&session), i64::MAX, "P0001").await;
    // Delivered, then unsettleable: the row now carries an intent — a debt.
    assert!(session.record(&usage_record(Decimal::ONE)).await.is_err());
    fault.remove(&pool).await;

    // Age it past its TTL. The reclaim sweep must not take it (it owes), and
    // the balance it represents must stay spoken for.
    // Both timestamps move: a CHECK constraint requires the expiry to
    // follow creation, which is also what makes this a realistic old row.
    query(
        "UPDATE usage_reservations
         SET created_at = created_at - INTERVAL '2 hours',
             expires_at = expires_at - INTERVAL '2 hours'
         WHERE api_key_id = $1",
    )
    .bind(key.id)
    .execute(&pool)
    .await
    .expect("ageing must apply");

    let second = begin_usage_session(
        &pool,
        &key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        true,
        MeteringLane::Reserved,
    )
    .await
    .expect("admission must query");
    assert!(
        matches!(second, UsageAdmission::InsufficientCredits),
        "the expired debt still holds the credit it owes; a second request cannot spend it again"
    );

    // And the debt is still collectable, which is the whole point of holding it.
    //
    // Which collection path applies is not this test's to choose. The expiry
    // sweep is GLOBAL — it runs inside every admission, for every user — and
    // an expired row that still owes is exactly what it quarantines. Once
    // quarantined the row leaves the automatic scan (`quarantined_at IS NULL`)
    // and belongs to the operator command instead. Whether some concurrent
    // admission got there first is a race this test cannot and should not win,
    // so both collectors are run: they settle the same debt through the same
    age_settlement_intent(&pool, key.id).await;
    recover_owed_settlements(&pool, 100)
        .await
        .expect("recovery must query");
    recover_quarantined_settlement(&pool, request_uuid(&session))
        .await
        .expect("operator collection must query");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ONE,
        "collecting the debt charges exactly what it recorded ($1 of the $2 funded)"
    );
}

// ============================================================================
// Releasing a quarantined reservation (migration 0015)
// ============================================================================

/// Whether the operator queue currently lists this request.
///
/// The queue is GLOBAL and oldest-first under a caller-supplied limit, so the
/// limit is generous and the answer is scoped to one request id — the same
/// shape every other quarantine assertion in this file uses.
async fn queued(pool: &PgPool, request_id: Uuid) -> bool {
    quarantined_settlements(pool, 1_000)
        .await
        .expect("quarantine must query")
        .iter()
        .any(|entry| entry.request_id == request_id)
}

/// The release record on a row: `(released_at, released_note)`, or `None` when
/// it has not been released.
async fn release_record(
    pool: &PgPool,
    reservation_id: Uuid,
) -> Option<(chrono::DateTime<chrono::Utc>, String)> {
    query_as::<_, (Option<chrono::DateTime<chrono::Utc>>, Option<String>)>(
        "SELECT released_at, released_note FROM usage_reservations WHERE id = $1",
    )
    .bind(reservation_id)
    .fetch_optional(pool)
    .await
    .expect("release record must query")
    .and_then(|(at, note)| Some((at?, note?)))
}

/// GUARD (mutation-checked): a released row leaves the reconciliation queue.
///
/// The dispatched-intentless class (0014) is the one that had no exit at all.
/// There is no stored payload, so `settle-owed` cannot collect it and must not
/// try; the amount is unknowable, so nothing can be billed; and it does not
/// encumber, so nothing is being held. All that is left is the FACT that a
/// charge was lost, which an operator reconciles against the provider's own
/// records and then has to be able to close. Before this, they could not: the
/// row sat in `admin owed-settlements` forever, and a queue that only grows
/// stops being read.
///
/// Marking, not deleting, is what makes the closure a record — the row keeps
/// what was reserved, when it dispatched, why it was parked, and now who gave
/// up on it and why.
#[tokio::test]
async fn releasing_an_intentless_quarantined_reservation_takes_it_out_of_the_queue() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "release-intentless").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let lost = request_uuid(&admit(&pool, &key, false).await);
    expire_reservation(&pool, lost, true, false).await;
    // Any admission runs the sweep, which is what parks the row.
    let _sweeper = admit(&pool, &key, false).await;
    assert!(
        queued(&pool, lost).await,
        "a dispatched intentless row is parked for an operator"
    );

    let outcome = release_quarantined_reservation(
        &pool,
        lost,
        "reconciled against the provider's own usage export; no charge recoverable",
        false,
    )
    .await
    .expect("release must query");
    assert!(
        matches!(
            outcome,
            ReservationRelease::Released {
                owed: false,
                forgiven_usd: None,
                ..
            }
        ),
        "an intentless row owes nothing, so nothing is forgiven and no \
         --forgive is demanded: {outcome:?}"
    );
    assert!(
        !queued(&pool, lost).await,
        "a released row must leave the operator queue"
    );

    // The record outlives the command: the fact, the time, and the reason are
    // all still on the row, and a terminal transcript was never the record.
    let (_, note) = release_record(&pool, lost)
        .await
        .expect("the release must be recorded on the row");
    assert_eq!(
        note,
        "reconciled against the provider's own usage export; no charge recoverable"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::TEN,
        "releasing a reservation moves no money in either direction"
    );

    // And no sweep may reclaim the record afterwards.
    let _sweeper = admit(&pool, &key, false).await;
    let (survives, quarantined, _) = swept_state(&pool, lost).await;
    assert!(
        survives && quarantined,
        "the release record must outlive every later sweep"
    );

    // Idempotent: the second attempt writes nothing and says why.
    let repeat = release_quarantined_reservation(&pool, lost, "second pass", false)
        .await
        .expect("release must query");
    assert!(
        matches!(repeat, ReservationRelease::AlreadyReleased { .. }),
        "a repeated release is a refusing no-op: {repeat:?}"
    );
    assert_eq!(
        release_record(&pool, lost)
            .await
            .expect("the record must still be there")
            .1,
        "reconciled against the provider's own usage export; no charge recoverable",
        "a repeat must not overwrite the sentence explaining the first decision"
    );

    drop_reservations(&pool, key.id).await;
}

/// GUARD (mutation-checked): a row that still owes a collectable settlement is
/// refused unless `--forgive` says otherwise, and forgiving it frees the credit
/// it was holding.
///
/// The two halves belong in one test because each is only correct given the
/// other. Refusing by default is what keeps `release-reservation` from becoming
/// the fast way to make a debt disappear — the money is collectable, the payload
/// is right there, and `settle-owed` collects it. But once an operator HAS
/// decided not to collect, the encumbrance that debt held has to go with it:
/// admission counts an owed row against the customer's balance forever (that is
/// the fix that stopped the same dollar funding two requests), so a forgiven
/// row left counting would freeze real credit against a charge ZeroRouter has
/// just said it will never take.
#[tokio::test]
async fn forgiving_an_owed_reservation_is_opt_in_and_frees_the_credit_it_held() {
    let Some(pool) = connect().await else {
        return;
    };
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "release-forgive").await;
    let key = create_key(&pool, user_id).await;
    // Exactly one reservation's worth ($2, the helper's size), so a second
    // admission is possible only once the first row stops encumbering.
    credit_purchase(&pool, user_id, Decimal::from(2), &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let request_id = request_uuid(&session);
    let fault = SettleFault::install(&pool, request_id, i64::MAX, "P0001").await;
    // Delivered, then unsettleable: the row carries a real $1 intent — a debt.
    assert!(session.record(&usage_record(Decimal::ONE)).await.is_err());
    fault.remove(&pool).await;
    // Park it exactly as an exhausted retry budget would. Note this leaves
    // `expires_at` in the FUTURE: a row quarantined by repeated settle
    // failures need not have expired, and it encumbers through the live arm
    // of admission's aggregate as well as the owed one. Releasing has to free
    // both, which is why the filter sits above them rather than inside one.
    query(
        "UPDATE usage_reservations
         SET quarantined_at = NOW(), settle_attempts = 8
         WHERE id = $1",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .expect("quarantine must apply");

    let refused = release_quarantined_reservation(&pool, request_id, "clear the queue", false)
        .await
        .expect("release must query");
    let ReservationRelease::Refused { reason } = &refused else {
        panic!("a collectable debt must not be released by default: {refused:?}")
    };
    assert!(
        reason.contains("settle-owed"),
        "the refusal must point at the command that collects: {reason}"
    );
    assert!(
        release_record(&pool, request_id).await.is_none(),
        "a refused release must write nothing"
    );
    assert!(
        queued(&pool, request_id).await,
        "and must leave the row in the queue"
    );
    let blocked = begin_usage_session(
        &pool,
        &key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        true,
        MeteringLane::Reserved,
    )
    .await
    .expect("admission must query");
    assert!(
        matches!(blocked, UsageAdmission::InsufficientCredits),
        "while the debt stands it goes on holding the credit it owes"
    );

    // The operator decides not to collect. Now — and only now — the row is
    // resolved and the credit it held is released.
    let forgiven = release_quarantined_reservation(
        &pool,
        request_id,
        "provider outage; charge waived as goodwill",
        true,
    )
    .await
    .expect("release must query");
    assert!(
        matches!(
            forgiven,
            ReservationRelease::Released {
                owed: true,
                forgiven_usd: Some(amount),
                ..
            } if amount == Decimal::ONE
        ),
        "the release states what charge was given up: {forgiven:?}"
    );
    assert!(!queued(&pool, request_id).await);
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(2),
        "forgiving a charge that was never taken debits nobody and credits nobody"
    );
    assert_eq!(
        usage_debits(&pool, user_id).await,
        0,
        "and writes no ledger entry: there was no debit to reverse"
    );

    let admitted = begin_usage_session(
        &pool,
        &key,
        cold_sizing(1_000, 500, Decimal::from(2)),
        ByokReservation::default(),
        test_signature(),
        true,
        MeteringLane::Reserved,
    )
    .await
    .expect("admission must query");
    assert!(
        matches!(admitted, UsageAdmission::Allowed(_)),
        "a forgiven debt holds none of the customer's credit"
    );

    drop_reservations(&pool, key.id).await;
}

/// Forgiveness is final: once an operator releases a debt with `--forgive`,
/// no collection route may charge it — not `settle-owed` by request id, and
/// not the request's own settle landing late. Without the released-row guard
/// in `settle_once`, the second is exactly what happens: a request whose row
/// was quarantined mid-flight (settle failures) and then forgiven would
/// debit the customer the moment its retry loop came back around,
/// contradicting a durable operator decision.
#[tokio::test]
async fn a_forgiven_debt_can_never_be_collected_afterwards() {
    let Some(pool) = connect().await else {
        return;
    };
    let _fault_guard = FAULT_LOCK.lock().await;
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "forgiven-uncollectable").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::from(2), &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = admit(&pool, &key, true).await;
    let request_id = request_uuid(&session);
    let fault = SettleFault::install(&pool, request_id, i64::MAX, "P0001").await;
    assert!(session.record(&usage_record(Decimal::ONE)).await.is_err());
    fault.remove(&pool).await;
    query(
        "UPDATE usage_reservations
         SET quarantined_at = NOW(), settle_attempts = 8
         WHERE id = $1",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .expect("quarantine must apply");

    let released = release_quarantined_reservation(&pool, request_id, "charge waived", true)
        .await
        .expect("release must query");
    assert!(
        matches!(released, ReservationRelease::Released { .. }),
        "{released:?}"
    );

    // Route 1: the operator command. It must report the forgiveness, not
    // replay the payload.
    let attempt = recover_quarantined_settlement(&pool, request_id)
        .await
        .expect("collection attempt must query");
    assert_eq!(
        (attempt.settled, attempt.forgiven),
        (0, 1),
        "settle-owed on a forgiven debt collects nothing and says why: {attempt:?}"
    );

    // Route 2: the request's own settle, landing after the release. The retry
    // loop must treat the forgiveness as terminal — no error, and no debit.
    session
        .record(&usage_record(Decimal::ONE))
        .await
        .expect("a late settle on a forgiven reservation must end quietly, not error");

    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(2),
        "no collection route may move money after forgiveness"
    );
    assert_eq!(
        usage_debits(&pool, user_id).await,
        0,
        "and no settled row may appear"
    );
    assert!(
        release_record(&pool, request_id).await.is_some(),
        "the release record survives the attempts on it"
    );

    drop_reservations(&pool, key.id).await;
}

/// Only a QUARANTINED reservation may be released, and each of the other
/// states is refused by name rather than by silence. A live row is finished by
/// settlement and an expired one by the sweep; a release that acted on either
/// would be reaching around the paths that exist to charge honestly, and an
/// unknown id is far more likely to be a typo than a resolution.
#[tokio::test]
async fn releasing_a_reservation_that_is_not_quarantined_is_refused_by_name() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.write().await;
    let user_id = create_user(&pool, "release-refusals").await;
    let key = create_key(&pool, user_id).await;

    let live = request_uuid(&admit(&pool, &key, false).await);
    let expired = request_uuid(&admit(&pool, &key, false).await);
    // Expired, never dispatched, owing nothing: the class the sweep reclaims.
    expire_reservation(&pool, expired, false, false).await;

    for (reservation, fragment, what) in [
        (live, "still live", "a running request"),
        (
            expired,
            "not quarantined",
            "an expired row awaiting the sweep",
        ),
        (
            Uuid::new_v4(),
            "no reservation",
            "an id that names nothing at all",
        ),
    ] {
        let refused = release_quarantined_reservation(&pool, reservation, "should refuse", true)
            .await
            .expect("release must query");
        let ReservationRelease::Refused { reason } = &refused else {
            panic!("{what} must be refused: {refused:?}")
        };
        assert!(
            reason.contains(fragment),
            "{what} must be refused by name; got: {reason}"
        );
        assert!(
            release_record(&pool, reservation).await.is_none(),
            "{what} must not be marked released"
        );
    }

    drop_reservations(&pool, key.id).await;
}

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
use zeroclaw_providers::pricing::ModelRates;
use zerorouter::{
    auth::{AuthenticatedKey, generate_api_key, hash_api_key},
    billing::{
        CheckoutIntent, CreditOutcome, balance, checkout_intent, credit_purchase, grant_promo,
        ledger_entries, record_checkout_intent, settle_checkout_intent,
    },
    db::{
        RequestTelemetry, ReservationBasis, UsageAdmission, UsageRecord, UsageSession,
        begin_usage_session, migrate, quarantined_settlements, recover_owed_settlements,
        recover_quarantined_settlement,
    },
    openai::{OpenAiUsage, TASK_SIGNATURE_SCHEME, TaskSignature, tool_names_digest},
    priority::Priority,
};

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
            shape_ok: None,
            priority: Some(Priority::Balanced),
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
    let user_id = create_user(&pool, "settle").await;
    let key = create_key(&pool, user_id).await;
    credit_purchase(&pool, user_id, Decimal::TEN, &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = match begin_usage_session(
        &pool,
        &key,
        1_000,
        500,
        Decimal::from(2),
        test_signature(),
        ReservationBasis::Cold,
        true,
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
            100,
            50,
            Decimal::from(2),
            test_signature(),
            ReservationBasis::Cold,
            true
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
        100,
        50,
        Decimal::from(2),
        test_signature(),
        ReservationBasis::Cold,
        false,
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
            100,
            50,
            Decimal::from(2),
            test_signature(),
            ReservationBasis::Cold,
            true
        ),
        begin_usage_session(
            &pool,
            &key_b,
            100,
            50,
            Decimal::from(2),
            test_signature(),
            ReservationBasis::Cold,
            true
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
    let user_id = create_user(&pool, "clamp").await;
    let key = create_key(&pool, user_id).await;
    // Fund exactly the reserved amount, then have actual usage exceed it.
    credit_purchase(&pool, user_id, Decimal::from(2), &unique_session_id(), None)
        .await
        .expect("funding purchase must apply");

    let session = match begin_usage_session(
        &pool,
        &key,
        1_000,
        500,
        Decimal::from(2),
        test_signature(),
        ReservationBasis::Cold,
        true,
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
    let user_id = create_user(&pool, "caponly").await;
    let key = create_key(&pool, user_id).await;
    // require_credits = false: no balance is funded, and settlement must record
    // the usage event for metering without moving money below zero.
    let session = match begin_usage_session(
        &pool,
        &key,
        1_000,
        500,
        Decimal::from(2),
        test_signature(),
        ReservationBasis::Cold,
        false,
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
        1_000,
        500,
        Decimal::from(2),
        test_signature(),
        ReservationBasis::Cold,
        require_credits,
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

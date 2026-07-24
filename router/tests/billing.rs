//! Prepaid-credit integration tests: purchase idempotency, settlement debits,
//! credit-gated admission, and tenant scoping of the ledger.
//!
//! Gated on `DATABASE_URL` like `tests/postgres.rs`: when unset each test
//! returns early (skips) instead of failing.

use std::str::FromStr;

use rust_decimal::Decimal;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zeroclaw_providers::pricing::ModelRates;
use zerorouter::{
    auth::{AuthenticatedKey, generate_api_key, hash_api_key},
    billing::{
        CheckoutIntent, CreditOutcome, balance, checkout_intent, credit_purchase, grant_promo,
        ledger_entries, record_checkout_intent, settle_checkout_intent,
    },
    db::{RequestTelemetry, UsageAdmission, UsageRecord, begin_usage_session, migrate},
    openai::OpenAiUsage,
};

const TEST_SIGNATURE: &str = "0123456789abcdef";

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
        TEST_SIGNATURE.to_owned(),
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
            TEST_SIGNATURE.to_owned(),
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
        TEST_SIGNATURE.to_owned(),
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
            TEST_SIGNATURE.to_owned(),
            true
        ),
        begin_usage_session(
            &pool,
            &key_b,
            100,
            50,
            Decimal::from(2),
            TEST_SIGNATURE.to_owned(),
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
        TEST_SIGNATURE.to_owned(),
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
        TEST_SIGNATURE.to_owned(),
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

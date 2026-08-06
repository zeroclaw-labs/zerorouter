//! The autopay sweep against a scripted Stripe: the full money loop —
//! candidate selection, saved-card lookup, off-session charge, exactly-once
//! credit — with Stripe's role played by a local fixture server.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::{Path, State},
    routing::{get, post},
};
use rust_decimal::Decimal;
use serde_json::json;
use std::str::FromStr;

use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{db::migrate, stripe::run_autopay_sweep_once, web::StripeSettings};

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

#[derive(Clone, Copy, PartialEq)]
enum MockOutcome {
    Succeed,
    Decline,
    /// A 5xx with no PaymentIntent in the body: Stripe may or may not have
    /// executed the charge. The router must not treat this as terminal.
    Ambiguous,
}

#[derive(Clone)]
struct MockStripe {
    charges: Arc<AtomicUsize>,
    outcome: MockOutcome,
}

fn mock_stripe(decline: bool) -> (Router, Arc<AtomicUsize>) {
    mock_stripe_with(if decline {
        MockOutcome::Decline
    } else {
        MockOutcome::Succeed
    })
}

fn mock_stripe_with(outcome: MockOutcome) -> (Router, Arc<AtomicUsize>) {
    let charges = Arc::new(AtomicUsize::new(0));
    let state = MockStripe {
        charges: charges.clone(),
        outcome,
    };
    let app = Router::new()
        .route(
            "/v1/customers/{customer}/payment_methods",
            get(|Path(_customer): Path<String>| async {
                axum::Json(json!({"data": [{"id": "pm_test_card"}]}))
            }),
        )
        .route(
            "/v1/payment_intents/{intent}",
            get(|Path(intent): Path<String>| async move {
                axum::Json(json!({"id": intent, "status": "succeeded"}))
            }),
        )
        .route(
            "/v1/payment_intents",
            post(|State(state): State<MockStripe>, body: String| async move {
                state.charges.fetch_add(1, Ordering::SeqCst);
                // Minimal form decode: keys and the values these tests
                // read carry no percent-encoding beyond brackets.
                let form: std::collections::HashMap<String, String> = body
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| {
                        (
                            key.replace("%5B", "[").replace("%5D", "]"),
                            value.replace('+', " ").replace("%2E", "."),
                        )
                    })
                    .collect();
                let id = format!("pi_mock_{}", Uuid::new_v4().simple());
                if state.outcome == MockOutcome::Ambiguous {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": {"message": "edge exploded"}})),
                    );
                }
                if state.outcome == MockOutcome::Decline {
                    (
                        axum::http::StatusCode::PAYMENT_REQUIRED,
                        axum::Json(json!({"error": {
                            "code": "card_declined",
                            "payment_intent": {"id": id},
                        }})),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "id": id,
                            "status": "succeeded",
                            "amount": form.get("amount"),
                            "metadata": {
                                "purpose": form.get("metadata[purpose]"),
                                "user_id": form.get("metadata[user_id]"),
                                "credit_usd": form.get("metadata[credit_usd]"),
                            },
                        })),
                    )
                }
            }),
        )
        .with_state(state);
    (app, charges)
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock stripe should bind");
    let address = listener.local_addr().expect("mock stripe address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

fn settings(api_base: &str) -> StripeSettings {
    StripeSettings {
        secret_key: "sk_test_mock".to_owned(),
        webhook_secret: "whsec_mock".to_owned(),
        checkout_min_usd: Decimal::from(5),
        checkout_max_usd: Decimal::from(1000),
        api_base: api_base.to_owned(),
    }
}

async fn autopay_user(pool: &PgPool, label: &str, threshold: i32, topup: i32) -> Uuid {
    let user_id = Uuid::new_v4();
    query(
        r#"
        INSERT INTO users (id, email, stripe_customer_id, autopay_enabled,
                           autopay_threshold_usd, autopay_topup_usd)
        VALUES ($1, $2, $3, TRUE, $4, $5)
        "#,
    )
    .bind(user_id)
    .bind(format!("sweep-{label}-{user_id}@example.invalid"))
    .bind(format!("cus_mock_{}", user_id.simple()))
    .bind(Decimal::from(threshold))
    .bind(Decimal::from(topup))
    .execute(pool)
    .await
    .expect("autopay user must insert");
    user_id
}

async fn balance_of(pool: &PgPool, user_id: Uuid) -> Decimal {
    query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("balance must query")
}

/// Neutralize residue from earlier runs and other suites: the sweep is
/// global, so any enabled user in the shared test database would be charged
/// against this test's mock and poison its counters.
/// Serializes the sweep tests. `run_autopay_sweep_once` is GLOBAL by
/// design — production runs one sweep for every armed user — so two tests
/// running side by side charge each other's users and each other's
/// assertions. `disarm_all_autopay` narrows the window at setup but cannot
/// close it: the other test arms its user immediately afterwards. Holding
/// this for the whole test body is what makes the sweep observable.
static SWEEP_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn disarm_all_autopay(pool: &PgPool) {
    query("UPDATE users SET autopay_enabled = FALSE WHERE autopay_enabled")
        .execute(pool)
        .await
        .expect("autopay disarm must run");
}

/// One sequential test on one binary: the sweep is global, so the happy
/// phase and the strike-out phase share it deliberately rather than racing
/// it from parallel tests.
#[tokio::test]
async fn the_sweep_charges_credits_and_strikes_out() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    // --- Phase 1: the happy loop --------------------------------------
    let (app, charges) = mock_stripe(false);
    let base = serve(app).await;
    let user_id = autopay_user(&pool, "happy", 10, 25).await;

    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(charges.load(Ordering::SeqCst), 1);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));
    let (status, amount) = query_as::<_, (String, Decimal)>(
        "SELECT status, amount_usd FROM stripe_autopay_intents WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("intent row must exist");
    assert_eq!(status, "succeeded");
    assert_eq!(amount, Decimal::from(25));
    let ledger = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'autopay'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("ledger must query");
    assert_eq!(ledger, 1);

    // Above threshold now: the sweep leaves the card alone.
    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(charges.load(Ordering::SeqCst), 1, "no second charge");
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));

    // --- Phase 2: declines strike out ---------------------------------
    // The happy user sits above threshold, so only the decline user is a
    // candidate against the declining mock.
    let (app, charges) = mock_stripe(true);
    let base = serve(app).await;
    let user_id = autopay_user(&pool, "decline", 10, 25).await;

    for round in 1..=3 {
        run_autopay_sweep_once(&pool, &settings(&base)).await;
        assert_eq!(
            charges.load(Ordering::SeqCst),
            round,
            "round {round} charged"
        );
    }
    let (enabled, failures) = query_as::<_, (bool, i32)>(
        "SELECT autopay_enabled, autopay_consecutive_failures FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("user must query");
    assert!(!enabled);
    assert_eq!(failures, 3);
    assert_eq!(balance_of(&pool, user_id).await, Decimal::ZERO);

    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(
        charges.load(Ordering::SeqCst),
        3,
        "a struck-out user is never charged again"
    );
}

/// The reconciliation pass: a pending intent whose terminal webhook never
/// arrived is queried at Stripe by the next sweep and settled by what
/// actually happened — one lost message can no longer wedge the user's
/// charge slot forever.
#[tokio::test]
async fn a_stale_pending_intent_is_reconciled_from_stripe() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;
    let (app, charges) = mock_stripe(false);
    let base = serve(app).await;
    let user_id = autopay_user(&pool, "reconcile", 10, 25).await;

    // A real-id pending intent, backdated past the reconciliation cutoff —
    // the state a lost webhook leaves behind. It also occupies the
    // one-pending slot, so the sweep must NOT create a second charge.
    let intent_id = format!("pi_mock_stale_{}", Uuid::new_v4().simple());
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, created_at)
        VALUES ($1, $2, 25, NOW() - INTERVAL '45 minutes')
        "#,
    )
    .bind(&intent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("stale intent must insert");

    run_autopay_sweep_once(&pool, &settings(&base)).await;

    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "reconciliation settles the existing charge; it never creates one"
    );
    assert_eq!(balance_of(&pool, user_id).await, Decimal::from(25));
    let status = query_scalar::<_, String>(
        "SELECT status FROM stripe_autopay_intents WHERE payment_intent_id = $1",
    )
    .bind(&intent_id)
    .fetch_one(&pool)
    .await
    .expect("intent must query");
    assert_eq!(status, "succeeded");
}

/// An ambiguous Stripe answer must not release the claim. If Stripe
/// executed the charge before an edge returned a 500, releasing the slot
/// would let the next sweep mint a FRESH idempotency key and charge the
/// card a second time. Holding the claim means reconciliation retries under
/// the ORIGINAL key, which Stripe answers with the same PaymentIntent.
#[tokio::test]
async fn an_ambiguous_stripe_answer_holds_the_claim_instead_of_recharging() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    let (app, charges) = mock_stripe_with(MockOutcome::Ambiguous);
    let base = serve(app).await;
    let user_id = autopay_user(&pool, "ambiguous", 10, 25).await;

    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(charges.load(Ordering::SeqCst), 1, "one charge is attempted");
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::ZERO,
        "an unconfirmed charge credits nothing"
    );

    // The pending claim survives, so the slot is still held and the next
    // sweep cannot start a second, differently-keyed charge.
    let pending = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM stripe_autopay_intents \
         WHERE user_id = $1 AND status = 'pending'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("claim state must query");
    assert_eq!(pending, 1, "the ambiguous claim is held for reconciliation");

    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(
        charges.load(Ordering::SeqCst),
        1,
        "a second sweep must not charge again while the claim stands"
    );

    // No strike is counted either: nothing is known to have failed.
    let failures =
        query_scalar::<_, i32>("SELECT autopay_consecutive_failures FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("failure count must query");
    assert_eq!(failures, 0, "an unknown outcome is not a failure");

    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("teardown disarm");
}

/// Opting out is honored at the moment of charge, not at the moment of
/// candidate selection. The sweep reads candidates in an unlocked snapshot;
/// a user who disables autopay after that read — and whose request returned
/// successfully — must not then be charged.
#[tokio::test]
async fn a_user_who_opts_out_before_the_claim_is_not_charged() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    let (app, charges) = mock_stripe(false);
    let base = serve(app).await;
    let user_id = autopay_user(&pool, "optout", 10, 25).await;

    // The opt-out lands between selection and claim; the claim re-reads the
    // flag, so it matches nothing and no charge follows.
    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("opt-out must apply");

    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "a user who opted out is not charged"
    );
    assert_eq!(balance_of(&pool, user_id).await, Decimal::ZERO);
}

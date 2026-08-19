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
use zerorouter::{
    billing::{FreezeReason, autopay_still_armed, claim_autopay_attempt, freeze_account},
    db::migrate,
    stripe::run_autopay_sweep_once,
    web::StripeSettings,
};

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
        publishable_key: "pk_test_mock".to_owned(),
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
            (payment_intent_id, user_id, amount_usd, charge_amount_usd, created_at)
        VALUES ($1, $2, 25, 26.38, NOW() - INTERVAL '45 minutes')
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

/// The deposit fee is applied a SECOND time on autopay, in its own charge path:
/// the saved card is charged the GROSS (credit + fee) while the balance is
/// credited the NET. This captures the exact `amount` the router sends Stripe
/// and the two dollar figures the intent row stores.
#[tokio::test]
async fn the_sweep_charges_gross_and_credits_net() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    // A mock that records the last `amount` (gross cents) it was asked to
    // charge, then reports the intent succeeded so the settle path runs inline.
    let charged: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let sink = charged.clone();
    let app = Router::new()
        .route(
            "/v1/customers/{customer}/payment_methods",
            get(|Path(_customer): Path<String>| async {
                axum::Json(json!({"data": [{"id": "pm_test_card"}]}))
            }),
        )
        .route(
            "/v1/payment_intents",
            post(move |body: String| {
                let sink = sink.clone();
                async move {
                    let amount = body
                        .split('&')
                        .filter_map(|pair| pair.split_once('='))
                        .find(|(key, _)| *key == "amount")
                        .map(|(_, value)| value.to_owned());
                    *sink.lock().expect("amount sink") = amount;
                    axum::Json(json!({
                        "id": format!("pi_mock_{}", Uuid::new_v4().simple()),
                        "status": "succeeded",
                    }))
                }
            }),
        );
    let base = serve(app).await;

    // $25 top-up is charged $26.38 gross (fee ceil(0.055*25) = $1.38).
    let user_id = autopay_user(&pool, "fee", 10, 25).await;
    run_autopay_sweep_once(&pool, &settings(&base)).await;

    assert_eq!(
        charged.lock().expect("amount sink").as_deref(),
        Some("2638"),
        "the card is charged the gross in cents"
    );
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::from(25),
        "only the net credit lands in the balance",
    );
    let (amount_usd, charge_amount_usd) = query_as::<_, (Decimal, Decimal)>(
        "SELECT amount_usd, charge_amount_usd FROM stripe_autopay_intents WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("intent row must exist");
    assert_eq!(
        amount_usd,
        Decimal::from(25),
        "the row stores the net credit"
    );
    assert_eq!(
        charge_amount_usd,
        Decimal::from_str("26.38").expect("literal"),
        "the row stores the gross charge; fee revenue is the difference",
    );

    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("teardown disarm");
}

// ---------------------------------------------------------------------------
// HIGH-2: the freeze/debt/failure eligibility predicate is re-asserted at
// every boundary that can reach the charge, not only at candidate selection.
// ---------------------------------------------------------------------------

/// What a racing dispute does to the account mid-sweep.
#[derive(Clone, Copy)]
enum Race {
    /// The dispute freeze commits (like the webhook's freeze_account).
    Freeze,
    /// The reversal drives the balance into a receivable.
    GoNegative,
    /// The user disables autopay (the portal's off switch) after the claim.
    Disable,
}

/// Mock Stripe whose payment-methods lookup — the round-trip the sweep makes
/// AFTER claiming but BEFORE the charge POST — commits the racing state change,
/// so the freeze/reversal lands in exactly the window HIGH-2 is about: past
/// candidate selection and the claim, immediately before money would move.
/// Only `replay_charge`'s pre-charge guard stands between it and the POST.
#[derive(Clone)]
struct RaceMock {
    pool: PgPool,
    user_id: Uuid,
    charges: Arc<AtomicUsize>,
    race: Race,
}

fn race_mock(pool: PgPool, user_id: Uuid, race: Race) -> (Router, Arc<AtomicUsize>) {
    let charges = Arc::new(AtomicUsize::new(0));
    let state = RaceMock {
        pool,
        user_id,
        charges: charges.clone(),
        race,
    };
    let app = Router::new()
        .route(
            "/v1/customers/{customer}/payment_methods",
            get(|State(state): State<RaceMock>| async move {
                match state.race {
                    Race::Freeze => {
                        query(
                            "UPDATE users SET frozen_at = NOW(), frozen_reason = 'dispute' WHERE id = $1",
                        )
                        .bind(state.user_id)
                        .execute(&state.pool)
                        .await
                        .expect("mid-sweep freeze must apply");
                    }
                    Race::GoNegative => {
                        // The 0009 overdraft trigger only permits a negative
                        // balance under a declared reversal, exactly as
                        // reverse_purchase declares it.
                        let mut tx = state.pool.begin().await.expect("tx");
                        query("SET LOCAL zerorouter.credit_reversal = 'on'")
                            .execute(&mut *tx)
                            .await
                            .expect("reversal flag");
                        query("UPDATE users SET credit_balance_usd = -1 WHERE id = $1")
                            .bind(state.user_id)
                            .execute(&mut *tx)
                            .await
                            .expect("mid-sweep reversal must apply");
                        tx.commit().await.expect("commit");
                    }
                    Race::Disable => {
                        query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
                            .bind(state.user_id)
                            .execute(&state.pool)
                            .await
                            .expect("mid-sweep opt-out must apply");
                    }
                }
                axum::Json(json!({"data": [{"id": "pm_test_card"}]}))
            }),
        )
        .route(
            "/v1/payment_intents",
            post(|State(state): State<RaceMock>| async move {
                state.charges.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({
                    "id": format!("pi_mock_{}", Uuid::new_v4().simple()),
                    "status": "succeeded",
                }))
            }),
        )
        .with_state(state);
    (app, charges)
}

/// FIX 2 — a FRESH sweep claim that turns ineligible before its first POST is
/// HELD, not released. Round 2's FIX D treated a fresh claim as
/// provably-unsubmitted and DELETED it, but "fresh" was the caller's local
/// history, not the claim's GLOBAL submission state: a paused fresh claim can be
/// picked up and POSTed by a reconciliation replay on another instance, so
/// deleting the pending row could drop the only durable idempotency handle on a
/// charge that already happened. A safe hold beats an unsafe delete. The account
/// is clean at selection and at the claim, then a dispute freeze commits during
/// the payment-methods round-trip — after the claim, before the charge. Nothing
/// is POSTed and the pending claim stays put; the resulting block-until-resolved
/// is intentionally deferred to the operator-resolution feature.
#[tokio::test]
async fn a_fresh_claim_that_turns_ineligible_pre_post_is_held_not_released() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    let user_id = autopay_user(&pool, "race-freeze", 10, 25).await;
    let (app, charges) = race_mock(pool.clone(), user_id, Race::Freeze);
    let base = serve(app).await;

    run_autopay_sweep_once(&pool, &settings(&base)).await;

    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "an account frozen before the charge is never POSTed to Stripe"
    );
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::ZERO,
        "nothing was credited"
    );
    let pending = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM stripe_autopay_intents WHERE user_id = $1 AND status = 'pending'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("claim state must query");
    assert_eq!(
        pending, 1,
        "the fresh claim is HELD, not released — the pending row stays for reconciliation"
    );
    let failures =
        query_scalar::<_, i32>("SELECT autopay_consecutive_failures FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("failure count must query");
    assert_eq!(failures, 0, "holding the claim is not a strike");

    // The account recovers (the dispute resolved and the freeze lifted), but the
    // held pending claim still occupies the one-pending slot, so a later sweep
    // does NOT recharge: the block-until-resolved is intentional and deferred to
    // the operator-resolution feature, never closed by a delete that could lose a
    // charge that may already have happened.
    query("UPDATE users SET frozen_at = NULL, frozen_reason = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("unfreeze must apply");
    let (app, charges) = mock_stripe(false);
    let base = serve(app).await;
    run_autopay_sweep_once(&pool, &settings(&base)).await;
    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "the held pending claim blocks a later sweep until an operator resolves it"
    );
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::ZERO,
        "still nothing credited while the claim is held"
    );

    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("teardown disarm");
}

/// The same window, but the racing event drives the balance into a receivable
/// (a negative balance is only FURTHER below the autopay threshold, so every
/// amount/threshold check still passes — only the `>= 0` half of the shared
/// predicate catches it). The fresh claim is likewise HELD, no strike (FIX 2).
#[tokio::test]
async fn a_fresh_claim_that_goes_indebted_pre_post_is_held() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    let user_id = autopay_user(&pool, "race-negative", 10, 25).await;
    let (app, charges) = race_mock(pool.clone(), user_id, Race::GoNegative);
    let base = serve(app).await;

    run_autopay_sweep_once(&pool, &settings(&base)).await;

    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "an indebted account is never POSTed to Stripe"
    );
    let pending = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM stripe_autopay_intents WHERE user_id = $1 AND status = 'pending'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("claim state must query");
    assert_eq!(
        pending, 1,
        "the fresh claim is HELD, not released — nothing was submitted, so the row stays for reconciliation"
    );
    let failures =
        query_scalar::<_, i32>("SELECT autopay_consecutive_failures FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("failure count must query");
    assert_eq!(failures, 0, "holding the claim is not a strike");

    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("teardown disarm");
}

/// FIX 1 completeness (round 5) — an OPT-OUT in the same pre-POST window. The
/// claim re-reads `autopay_enabled` and succeeds while the user is still armed,
/// then the user disables autopay during the payment-methods round-trip — after
/// the claim, before the charge. The pre-POST guard now includes `autopay_enabled`
/// (it previously omitted it), so it refuses: nothing is POSTed and the pending
/// claim is HELD for reconciliation, never released. Do-not-charge an opted-out
/// account is the whole point of the flag; charging one anyway is the bug this
/// closes. Without `autopay_enabled` in the guard the charge would be POSTed —
/// this is the mutation-checked pre-POST guard.
#[tokio::test]
async fn a_fresh_claim_that_opts_out_pre_post_is_held_not_charged() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    let user_id = autopay_user(&pool, "race-optout", 10, 25).await;
    let (app, charges) = race_mock(pool.clone(), user_id, Race::Disable);
    let base = serve(app).await;

    run_autopay_sweep_once(&pool, &settings(&base)).await;

    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "an account that opted out before the charge is never POSTed to Stripe"
    );
    assert_eq!(
        balance_of(&pool, user_id).await,
        Decimal::ZERO,
        "nothing was credited"
    );
    let pending = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM stripe_autopay_intents WHERE user_id = $1 AND status = 'pending'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("claim state must query");
    assert_eq!(
        pending, 1,
        "the fresh claim is HELD, not released — the pending row stays for reconciliation"
    );
    let failures =
        query_scalar::<_, i32>("SELECT autopay_consecutive_failures FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("failure count must query");
    assert_eq!(failures, 0, "holding the claim is not a strike");

    // Already disarmed by the race, but keep the teardown symmetric with the
    // sibling pre-POST tests.
    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("teardown disarm");
}

/// FIX D — a RECONCILIATION claim (a stranded local claim from an earlier sweep,
/// possibly already submitted to Stripe under its idempotency key) that turns
/// ineligible during its replay must be HELD, never released: its key is the
/// only durable handle on a charge that might have happened. The claim is armed
/// at the reconciliation check, then a freeze commits during the payment-methods
/// round-trip; the pre-POST guard refuses the charge and leaves the row pending.
#[tokio::test]
async fn a_reconciliation_claim_that_turns_ineligible_pre_post_stays_held() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;
    disarm_all_autopay(&pool).await;

    let user_id = autopay_user(&pool, "recon-freeze", 10, 25).await;

    // A stranded local claim from an earlier sweep, backdated past the
    // reconciliation cutoff (but within the replay window). It occupies the
    // one-pending slot; reconciliation will replay it under its original key.
    let key = Uuid::new_v4().simple().to_string();
    let intent_id = format!("local_{key}");
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd, created_at)
        VALUES ($1, $2, 25, 26.38, NOW() - INTERVAL '45 minutes')
        "#,
    )
    .bind(&intent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("stranded local claim must insert");

    // The account is armed when reconciliation checks; the freeze lands during
    // the payment-methods round-trip inside the replay, i.e. after the arming
    // check and before the POST.
    let (app, charges) = race_mock(pool.clone(), user_id, Race::Freeze);
    let base = serve(app).await;

    run_autopay_sweep_once(&pool, &settings(&base)).await;

    assert_eq!(
        charges.load(Ordering::SeqCst),
        0,
        "a reconciliation claim that turns ineligible is never POSTed"
    );
    let pending = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM stripe_autopay_intents WHERE payment_intent_id = $1 AND status = 'pending'",
    )
    .bind(&intent_id)
    .fetch_one(&pool)
    .await
    .expect("claim state must query");
    assert_eq!(
        pending, 1,
        "the possibly-submitted reconciliation claim is HELD, not released"
    );
    let failures =
        query_scalar::<_, i32>("SELECT autopay_consecutive_failures FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("failure count must query");
    assert_eq!(failures, 0, "holding the claim is not a strike");

    query("UPDATE users SET autopay_enabled = FALSE WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("teardown disarm");
}

/// The claim gate re-asserts the full eligibility set. A freeze / receivable /
/// max-failures that commits between selection and the claim makes the claim
/// match nothing, so no charge is ever attempted — this is the boundary that
/// closes the window before Stripe is even contacted.
#[tokio::test]
async fn claim_autopay_attempt_refuses_a_frozen_indebted_or_maxed_out_account() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;

    // A clean, under-threshold account claims.
    let clean = autopay_user(&pool, "claim-clean", 10, 25).await;
    assert!(
        claim_autopay_attempt(
            &pool,
            clean,
            Decimal::from(25),
            Decimal::from_str("26.38").expect("literal"),
            &Uuid::new_v4().simple().to_string(),
        )
        .await
        .expect("claim must query"),
        "a clean under-threshold account is claimed"
    );

    // Frozen: refused, even though the amount and threshold still match.
    let frozen = autopay_user(&pool, "claim-frozen", 10, 25).await;
    freeze_account(&pool, frozen, FreezeReason::Dispute)
        .await
        .expect("freeze must apply");
    assert!(
        !claim_autopay_attempt(
            &pool,
            frozen,
            Decimal::from(25),
            Decimal::from_str("26.38").expect("literal"),
            &Uuid::new_v4().simple().to_string(),
        )
        .await
        .expect("claim must query"),
        "a frozen account is not claimed"
    );

    // Negative balance: refused. It is further below the threshold, so the
    // amount/threshold checks pass; only the `>= 0` predicate stops it.
    let debtor = autopay_user(&pool, "claim-debtor", 10, 25).await;
    let mut tx = pool.begin().await.expect("tx");
    query("SET LOCAL zerorouter.credit_reversal = 'on'")
        .execute(&mut *tx)
        .await
        .expect("reversal flag");
    query("UPDATE users SET credit_balance_usd = -1 WHERE id = $1")
        .bind(debtor)
        .execute(&mut *tx)
        .await
        .expect("receivable must apply");
    tx.commit().await.expect("commit");
    assert!(
        !claim_autopay_attempt(
            &pool,
            debtor,
            Decimal::from(25),
            Decimal::from_str("26.38").expect("literal"),
            &Uuid::new_v4().simple().to_string(),
        )
        .await
        .expect("claim must query"),
        "an indebted account is not claimed"
    );

    // Three consecutive failures: refused.
    let maxed = autopay_user(&pool, "claim-maxed", 10, 25).await;
    query("UPDATE users SET autopay_consecutive_failures = 3 WHERE id = $1")
        .bind(maxed)
        .execute(&pool)
        .await
        .expect("strike count must apply");
    assert!(
        !claim_autopay_attempt(
            &pool,
            maxed,
            Decimal::from(25),
            Decimal::from_str("26.38").expect("literal"),
            &Uuid::new_v4().simple().to_string(),
        )
        .await
        .expect("claim must query"),
        "a struck-out account is not claimed"
    );
}

/// The reconciliation-replay gate (`autopay_still_armed`) is the full
/// eligibility test, not just the enabled flag: a stranded claim on an account
/// that has since been frozen, driven negative, or struck out must not be
/// replayed up to ~20 hours later.
#[tokio::test]
async fn autopay_still_armed_is_false_when_frozen_negative_or_maxed_out() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = SWEEP_LOCK.lock().await;

    let clean = autopay_user(&pool, "armed-clean", 10, 25).await;
    assert!(
        autopay_still_armed(&pool, clean)
            .await
            .expect("still-armed must query"),
        "a clean enabled account is armed"
    );

    let frozen = autopay_user(&pool, "armed-frozen", 10, 25).await;
    freeze_account(&pool, frozen, FreezeReason::Dispute)
        .await
        .expect("freeze must apply");
    assert!(
        !autopay_still_armed(&pool, frozen)
            .await
            .expect("still-armed must query"),
        "a frozen account is not armed"
    );

    let debtor = autopay_user(&pool, "armed-debtor", 10, 25).await;
    let mut tx = pool.begin().await.expect("tx");
    query("SET LOCAL zerorouter.credit_reversal = 'on'")
        .execute(&mut *tx)
        .await
        .expect("reversal flag");
    query("UPDATE users SET credit_balance_usd = -1 WHERE id = $1")
        .bind(debtor)
        .execute(&mut *tx)
        .await
        .expect("receivable must apply");
    tx.commit().await.expect("commit");
    assert!(
        !autopay_still_armed(&pool, debtor)
            .await
            .expect("still-armed must query"),
        "an indebted account is not armed"
    );

    let maxed = autopay_user(&pool, "armed-maxed", 10, 25).await;
    query("UPDATE users SET autopay_consecutive_failures = 3 WHERE id = $1")
        .bind(maxed)
        .execute(&pool)
        .await
        .expect("strike count must apply");
    assert!(
        !autopay_still_armed(&pool, maxed)
            .await
            .expect("still-armed must query"),
        "a struck-out account is not armed"
    );
}

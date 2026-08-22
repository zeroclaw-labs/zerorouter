//! The durable autopay tax lifecycle (migration 0024) against a scripted
//! Stripe Tax API: recording retries for tax transactions the inline path
//! failed to record, full reversals for refunded charges, and the rows
//! automation must refuse to touch.
//!
//! Every test drives `sweep_autopay_tax_lifecycle` — the exact code the
//! production sweep runs — and asserts BOTH the Stripe side (which calls the
//! fixture saw, keyed by calculation / transaction id so a concurrent suite
//! sharing the database cannot poison the counts) and the database side (the
//! stamps that stop the sweep repeating itself).

use std::sync::{Arc, Mutex};

use axum::{Router, extract::State, routing::post};
use rust_decimal::Decimal;
use serde_json::json;
use std::str::FromStr;

use sqlx_core::{query::query, query_as::query_as};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{db::migrate, stripe::sweep_autopay_tax_lifecycle, web::StripeSettings};

/// Serializes these tests. `sweep_autopay_tax_lifecycle` is GLOBAL by design —
/// production runs one pass over every row — so two tests running side by side
/// record each other's rows against each other's fixtures and each other's
/// assertions. The same shape as `autopay_sweep.rs`'s `SWEEP_LOCK`.
static TAX_SWEEP_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

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

/// One recorded call against the tax fixture: the form fields the sweep sent.
#[derive(Clone, Debug)]
struct TaxCall {
    calculation: Option<String>,
    original_transaction: Option<String>,
    reference: Option<String>,
    mode: Option<String>,
}

#[derive(Clone)]
struct MockTax {
    /// Every create_from_calculation call, in order.
    recordings: Arc<Mutex<Vec<TaxCall>>>,
    /// Every create_reversal call, in order.
    reversals: Arc<Mutex<Vec<TaxCall>>>,
    /// When set, create_from_calculation answers 500 instead of recording.
    fail_recordings: Arc<std::sync::atomic::AtomicBool>,
}

fn form_field(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| {
            value
                .replace('+', " ")
                .replace("%5B", "[")
                .replace("%5D", "]")
        })
    })
}

fn mock_tax() -> (Router, MockTax) {
    let state = MockTax {
        recordings: Arc::new(Mutex::new(Vec::new())),
        reversals: Arc::new(Mutex::new(Vec::new())),
        fail_recordings: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let app = Router::new()
        .route(
            "/v1/tax/transactions/create_from_calculation",
            post(|State(state): State<MockTax>, body: String| async move {
                let call = TaxCall {
                    calculation: form_field(&body, "calculation"),
                    original_transaction: None,
                    reference: form_field(&body, "reference"),
                    mode: None,
                };
                state.recordings.lock().expect("mock lock").push(call);
                if state
                    .fail_recordings
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": {"message": "tax exploded"}})),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({
                        "id": format!("tax_mock_{}", Uuid::new_v4().simple()),
                        "object": "tax.transaction",
                    })),
                )
            }),
        )
        .route(
            "/v1/tax/transactions/create_reversal",
            post(|State(state): State<MockTax>, body: String| async move {
                let call = TaxCall {
                    calculation: None,
                    original_transaction: form_field(&body, "original_transaction"),
                    reference: form_field(&body, "reference"),
                    mode: form_field(&body, "mode"),
                };
                state.reversals.lock().expect("mock lock").push(call);
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({
                        "id": format!("tax_mock_rev_{}", Uuid::new_v4().simple()),
                        "object": "tax.transaction",
                    })),
                )
            }),
        )
        .with_state(state.clone());
    (app, state)
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
        // A card-only deployment, which is what this harness describes.
        crypto_rail: false,
    }
}

async fn user(pool: &PgPool, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("tax-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("user must insert");
    user_id
}

/// A settled autopay charge with a frozen tax figure, in the given lifecycle
/// state. Returns the (unique) payment intent and calculation ids.
#[expect(clippy::too_many_arguments, reason = "a row factory names its columns")]
async fn intent(
    pool: &PgPool,
    user_id: Uuid,
    status: &str,
    tax_usd: Decimal,
    recorded: bool,
    transaction_id: Option<&str>,
    reversed: bool,
    refunded: bool,
) -> (String, String) {
    let intent_id = format!("pi_taxlife_{}", Uuid::new_v4().simple());
    let calculation_id = format!("taxcalc_mock_{}", Uuid::new_v4().simple());
    query(
        r#"
        INSERT INTO stripe_autopay_intents
            (payment_intent_id, user_id, amount_usd, charge_amount_usd, status,
             tax_amount_usd, tax_calculation_id, tax_transaction_id,
             tax_recorded_at, tax_reversed_at)
        VALUES ($1, $2, 25, 27, $3, $4, $5, $6,
                CASE WHEN $7 THEN NOW() END,
                CASE WHEN $8 THEN NOW() END)
        "#,
    )
    .bind(&intent_id)
    .bind(user_id)
    .bind(status)
    .bind(tax_usd)
    .bind(&calculation_id)
    .bind(transaction_id)
    .bind(recorded)
    .bind(reversed)
    .execute(pool)
    .await
    .expect("intent row must insert");
    if refunded {
        query(
            r#"
            INSERT INTO credit_ledger
                (user_id, entry_type, amount_usd, balance_after_usd,
                 stripe_session_id, stripe_payment_intent_id, note)
            VALUES ($1, 'refund', -25, 0, $2, $3, 'test refund reversal')
            "#,
        )
        .bind(user_id)
        .bind(format!("ch_taxlife_{}", Uuid::new_v4().simple()))
        .bind(&intent_id)
        .execute(pool)
        .await
        .expect("refund row must insert");
    }
    (intent_id, calculation_id)
}

async fn row_state(pool: &PgPool, intent_id: &str) -> (Option<String>, bool, Option<String>, bool) {
    query_as::<_, (Option<String>, bool, Option<String>, bool)>(
        r#"
        SELECT tax_transaction_id, tax_recorded_at IS NOT NULL,
               tax_reversal_transaction_id, tax_reversed_at IS NOT NULL
        FROM stripe_autopay_intents WHERE payment_intent_id = $1
        "#,
    )
    .bind(intent_id)
    .fetch_one(pool)
    .await
    .expect("intent row must exist")
}

fn recordings_for(mock: &MockTax, calculation_id: &str) -> usize {
    mock.recordings
        .lock()
        .expect("mock lock")
        .iter()
        .filter(|call| call.calculation.as_deref() == Some(calculation_id))
        .count()
}

fn reversals_for(mock: &MockTax, transaction_id: &str) -> Vec<TaxCall> {
    mock.reversals
        .lock()
        .expect("mock lock")
        .iter()
        .filter(|call| call.original_transaction.as_deref() == Some(transaction_id))
        .cloned()
        .collect()
}

/// The retry pass records a transaction the inline path missed, stores its id,
/// and — the stamp being the whole point — never records it again.
#[tokio::test]
async fn an_unrecorded_tax_transaction_is_recorded_once_and_stamped() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = TAX_SWEEP_LOCK.lock().await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;
    let user_id = user(&pool, "record").await;
    let (intent_id, calculation_id) = intent(
        &pool,
        user_id,
        "succeeded",
        Decimal::new(169, 2),
        false,
        None,
        false,
        false,
    )
    .await;

    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    let (transaction_id, recorded, _, reversed) = row_state(&pool, &intent_id).await;
    assert_eq!(recordings_for(&mock, &calculation_id), 1);
    assert!(recorded, "the recording stamp must land");
    let transaction_id = transaction_id.expect("the transaction id must be stored");
    assert!(transaction_id.starts_with("tax_mock_"));
    assert!(!reversed, "nothing here was refunded");

    // The stamp, not luck, is what stops a second recording.
    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    assert_eq!(recordings_for(&mock, &calculation_id), 1);
    let (still_transaction_id, ..) = row_state(&pool, &intent_id).await;
    assert_eq!(
        still_transaction_id.as_deref(),
        Some(transaction_id.as_str())
    );
}

/// A recording failure leaves the row unstamped — retried next pass, and
/// completed the moment Stripe recovers.
#[tokio::test]
async fn a_failed_recording_is_retried_until_stripe_recovers() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = TAX_SWEEP_LOCK.lock().await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;
    let user_id = user(&pool, "retry").await;
    let (intent_id, calculation_id) = intent(
        &pool,
        user_id,
        "succeeded",
        Decimal::new(169, 2),
        false,
        None,
        false,
        false,
    )
    .await;

    mock.fail_recordings
        .store(true, std::sync::atomic::Ordering::SeqCst);
    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    assert_eq!(recordings_for(&mock, &calculation_id), 1);
    let (transaction_id, recorded, _, _) = row_state(&pool, &intent_id).await;
    assert!(!recorded, "a 500 must not stamp the row");
    assert_eq!(transaction_id, None);

    mock.fail_recordings
        .store(false, std::sync::atomic::Ordering::SeqCst);
    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    assert_eq!(recordings_for(&mock, &calculation_id), 2);
    let (transaction_id, recorded, _, _) = row_state(&pool, &intent_id).await;
    assert!(recorded);
    assert!(transaction_id.is_some());
}

/// A refunded charge's recorded tax is reversed in full, exactly once, under
/// the deterministic reference that makes a lost response safe to retry.
#[tokio::test]
async fn a_refunded_charge_gets_a_full_tax_reversal_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = TAX_SWEEP_LOCK.lock().await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;
    let user_id = user(&pool, "reverse").await;
    let tax_transaction = format!("tax_mock_{}", Uuid::new_v4().simple());
    let (intent_id, _) = intent(
        &pool,
        user_id,
        "succeeded",
        Decimal::new(169, 2),
        true,
        Some(&tax_transaction),
        false,
        true,
    )
    .await;

    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    let calls = reversals_for(&mock, &tax_transaction);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].mode.as_deref(), Some("full"));
    assert_eq!(
        calls[0].reference.as_deref(),
        Some(format!("{intent_id}-tax-reversal").as_str())
    );
    let (_, _, reversal_id, reversed) = row_state(&pool, &intent_id).await;
    assert!(reversed, "the reversal stamp must land");
    assert!(
        reversal_id
            .as_deref()
            .is_some_and(|id| id.starts_with("tax_mock_rev_"))
    );

    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    assert_eq!(reversals_for(&mock, &tax_transaction).len(), 1);
}

/// Refund-before-recording: one pass records the missing transaction and then
/// reverses it, because the reversal pass runs after the recording pass and
/// detects by the ledger, not by event order.
#[tokio::test]
async fn an_unrecorded_then_refunded_charge_converges_in_one_pass() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = TAX_SWEEP_LOCK.lock().await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;
    let user_id = user(&pool, "converge").await;
    let (intent_id, calculation_id) = intent(
        &pool,
        user_id,
        "succeeded",
        Decimal::new(169, 2),
        false,
        None,
        false,
        true,
    )
    .await;

    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;
    assert_eq!(recordings_for(&mock, &calculation_id), 1);
    let (transaction_id, recorded, reversal_id, reversed) = row_state(&pool, &intent_id).await;
    assert!(recorded && reversed, "one pass must complete both halves");
    let transaction_id = transaction_id.expect("recorded id");
    assert_eq!(reversals_for(&mock, &transaction_id).len(), 1);
    assert!(reversal_id.is_some());
}

/// The rows automation must not touch: a recorded transaction with no stored
/// id cannot be reversed (surfaced for the operator instead), a `withheld`
/// charge records no tax at all, and a zero-tax calculation records on the
/// same terms as a taxed one (it evidences sales volume).
#[tokio::test]
async fn scoping_id_less_rows_wait_withheld_never_records_zero_tax_does() {
    let Some(pool) = connect().await else {
        return;
    };
    let _sweep_guard = TAX_SWEEP_LOCK.lock().await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;
    let user_id = user(&pool, "scope").await;

    // Recorded, refunded, but the transaction id was never stored (0024
    // backfill shape): no reversal call may be attempted.
    let (idless_intent, idless_calculation) = intent(
        &pool,
        user_id,
        "succeeded",
        Decimal::new(169, 2),
        true,
        None,
        false,
        true,
    )
    .await;
    // Withheld: money queued for refund, deliberately never reported as a sale.
    let (_, withheld_calculation) = intent(
        &pool,
        user_id,
        "withheld",
        Decimal::new(169, 2),
        false,
        None,
        false,
        false,
    )
    .await;
    // Zero tax, unrecorded: must still be recorded.
    let (zero_intent, zero_calculation) = intent(
        &pool,
        user_id,
        "succeeded",
        Decimal::ZERO,
        false,
        None,
        false,
        false,
    )
    .await;

    sweep_autopay_tax_lifecycle(&pool, &settings(&base)).await;

    assert_eq!(
        recordings_for(&mock, &idless_calculation),
        0,
        "a stamped row must not be re-recorded"
    );
    let (_, _, _, reversed) = row_state(&pool, &idless_intent).await;
    assert!(!reversed, "an id-less row cannot be auto-reversed");
    assert!(
        mock.reversals
            .lock()
            .expect("mock lock")
            .iter()
            .all(|call| {
                call.reference.as_deref() != Some(format!("{idless_intent}-tax-reversal").as_str())
            }),
        "no reversal may be attempted without the transaction id"
    );

    assert_eq!(
        recordings_for(&mock, &withheld_calculation),
        0,
        "withheld money is refunded, never reported as a sale"
    );

    assert_eq!(recordings_for(&mock, &zero_calculation), 1);
    let (transaction_id, recorded, _, _) = row_state(&pool, &zero_intent).await;
    assert!(recorded && transaction_id.is_some());
}

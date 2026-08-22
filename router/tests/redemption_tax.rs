//! The redemption-tax sweep against a scripted Stripe Tax API: enrollment
//! and the opening-balance exemption, period tiling over the usage ledger,
//! pricing, the clamped debit, and transaction recording — plus the two
//! properties the whole feature hangs on: `off` really is off, and `dry_run`
//! really moves no money.
//!
//! The fixture prices tax at a flat 6.25% (Massachusetts-shaped), floor to
//! whole cents: `tax_cents = amount * 625 / 10000`.

use std::sync::{Arc, Mutex};

use axum::{Router, extract::State, routing::post};
use rust_decimal::Decimal;
use serde_json::json;
use std::str::FromStr;

use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{
    db::migrate,
    redemption_tax::{
        RedemptionTaxMode, backfill_buyer_address_if_absent, run_redemption_tax_sweep_once,
        store_buyer_address,
    },
    web::StripeSettings,
};

/// Serializes these tests: the sweep is GLOBAL by design, so two tests
/// running side by side would price and debit each other's users. Same shape
/// as `autopay_sweep.rs`'s `SWEEP_LOCK`.
static SWEEP_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
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

/// Neutralize residue from earlier runs and other suites. The sweep is
/// global: any pre-existing user with unswept usage would grow a period,
/// hit this test's fixture, and — in collect mode — have their balance
/// debited under this test's nose. Deleting old periods and re-enrolling
/// every existing user at the current ledger head leaves nothing for the
/// sweep to find except what the test itself creates afterwards.
async fn quiesce(pool: &PgPool) {
    query("DELETE FROM redemption_tax_periods")
        .execute(pool)
        .await
        .expect("period residue must clear");
    query(
        r#"
        UPDATE users u
        SET redemption_tax_enrolled_at = NOW(),
            redemption_tax_from_ledger_id = COALESCE(
                (SELECT MAX(l.id) FROM credit_ledger l WHERE l.user_id = u.id), 0),
            redemption_tax_exempt_remaining_usd = 0
        "#,
    )
    .execute(pool)
    .await
    .expect("user residue must re-enroll");
}

#[derive(Clone, Debug)]
struct TaxCall {
    amount: Option<String>,
    calculation: Option<String>,
    reference: Option<String>,
}

#[derive(Clone)]
struct MockTax {
    calculations: Arc<Mutex<Vec<TaxCall>>>,
    recordings: Arc<Mutex<Vec<TaxCall>>>,
}

fn form_field(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        // Bracketed keys arrive percent-encoded (`line_items%5B0%5D...`);
        // decode the name before comparing, not just the value.
        let name = name.replace("%5B", "[").replace("%5D", "]");
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
        calculations: Arc::new(Mutex::new(Vec::new())),
        recordings: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/v1/tax/calculations",
            post(|State(state): State<MockTax>, body: String| async move {
                let amount = form_field(&body, "line_items[0][amount]");
                let tax_cents = amount
                    .as_deref()
                    .and_then(|amount| amount.parse::<i64>().ok())
                    .map_or(0, |amount| amount * 625 / 10_000);
                state.calculations.lock().expect("mock lock").push(TaxCall {
                    amount,
                    calculation: None,
                    reference: form_field(&body, "reference"),
                });
                axum::Json(json!({
                    "id": format!("taxcalc_mock_{}", Uuid::new_v4().simple()),
                    "tax_amount_exclusive": tax_cents,
                    "tax_amount_inclusive": 0,
                }))
            }),
        )
        .route(
            "/v1/tax/transactions/create_from_calculation",
            post(|State(state): State<MockTax>, body: String| async move {
                state.recordings.lock().expect("mock lock").push(TaxCall {
                    amount: None,
                    calculation: form_field(&body, "calculation"),
                    reference: form_field(&body, "reference"),
                });
                axum::Json(json!({
                    "id": format!("tax_mock_{}", Uuid::new_v4().simple()),
                }))
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

/// A user already enrolled with a chosen exemption and a cursor at zero, so
/// every usage row the test writes lands in a period. A Cambridge MA address
/// unless the test says otherwise.
async fn enrolled_user(pool: &PgPool, label: &str, balance: Decimal, exempt: Decimal) -> Uuid {
    let user_id = Uuid::new_v4();
    query(
        r#"
        INSERT INTO users
            (id, email, credit_balance_usd,
             redemption_tax_enrolled_at, redemption_tax_from_ledger_id,
             redemption_tax_exempt_remaining_usd,
             tax_address_country, tax_address_postal_code, tax_address_state,
             tax_address_city)
        VALUES ($1, $2, $3, NOW(), 0, $4, 'US', '02139', 'MA', 'Cambridge')
        "#,
    )
    .bind(user_id)
    .bind(format!("rtx-{label}-{user_id}@example.invalid"))
    .bind(balance)
    .bind(exempt)
    .execute(pool)
    .await
    .expect("enrolled user must insert");
    user_id
}

async fn add_usage(pool: &PgPool, user_id: Uuid, amount: Decimal) {
    query(
        r#"
        INSERT INTO credit_ledger
            (user_id, entry_type, amount_usd, balance_after_usd, request_id)
        VALUES ($1, 'usage', $2, 0, $3)
        "#,
    )
    .bind(user_id)
    .bind(-amount)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("usage row must insert");
}

#[derive(Debug)]
struct PeriodRow {
    usage_usd: Decimal,
    exempt_usd: Decimal,
    taxable_usd: Decimal,
    tax_usd: Option<Decimal>,
    collected_usd: Option<Decimal>,
    shortfall_usd: Option<Decimal>,
    debited: bool,
    recorded_reference: Option<String>,
    fallback_reason: Option<String>,
}

async fn periods_of(pool: &PgPool, user_id: Uuid) -> Vec<PeriodRow> {
    query_as::<
        _,
        (
            Uuid,
            Decimal,
            Decimal,
            Decimal,
            Option<Decimal>,
            Option<Decimal>,
            Option<Decimal>,
            bool,
            Option<String>,
            Option<String>,
        ),
    >(
        r#"
        SELECT id, usage_usd, exempt_usd, taxable_usd, tax_usd,
               collected_usd, shortfall_usd, debited_at IS NOT NULL,
               tax_transaction_id, fallback_reason
        FROM redemption_tax_periods
        WHERE user_id = $1
        ORDER BY through_ledger_id
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("periods must query")
    .into_iter()
    .map(
        |(
            id,
            usage_usd,
            exempt_usd,
            taxable_usd,
            tax_usd,
            collected_usd,
            shortfall_usd,
            debited,
            tax_transaction_id,
            fallback_reason,
        )| {
            PeriodRow {
                usage_usd,
                exempt_usd,
                taxable_usd,
                tax_usd,
                collected_usd,
                shortfall_usd,
                debited,
                recorded_reference: tax_transaction_id.map(|_| format!("rtx_{id}")),
                fallback_reason,
            }
        },
    )
    .collect()
}

async fn balance_of(pool: &PgPool, user_id: Uuid) -> Decimal {
    query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("balance must query")
}

async fn tax_ledger_rows(pool: &PgPool, user_id: Uuid) -> Vec<Decimal> {
    query_scalar::<_, Decimal>(
        "SELECT amount_usd FROM credit_ledger WHERE user_id = $1 AND entry_type = 'tax' ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("tax rows must query")
}

fn usd(cents: i64) -> Decimal {
    Decimal::new(cents, 2)
}

/// `off` is really off: no enrollment, no periods, no Stripe traffic —
/// a deployment with the variable unset behaves as if the module were not
/// compiled in.
#[tokio::test]
async fn off_mode_touches_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;

    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email, credit_balance_usd) VALUES ($1, $2, 50)")
        .bind(user_id)
        .bind(format!("rtx-off-{user_id}@example.invalid"))
        .execute(&pool)
        .await
        .expect("user must insert");
    add_usage(&pool, user_id, usd(800)).await;

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Off).await;

    let enrolled = query_scalar::<_, bool>(
        "SELECT redemption_tax_enrolled_at IS NOT NULL FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("user must exist");
    assert!(!enrolled, "off must not enroll");
    assert!(periods_of(&pool, user_id).await.is_empty());
    assert!(mock.calculations.lock().expect("mock lock").is_empty());
}

/// `dry_run` prices — the operator's evidence — but debits nothing and
/// records nothing: no balance movement, no `tax` ledger rows, no
/// create_from_calculation traffic.
#[tokio::test]
async fn dry_run_prices_but_moves_no_money() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;

    let user_id = enrolled_user(&pool, "dry", usd(10_000), Decimal::ZERO).await;
    add_usage(&pool, user_id, usd(800)).await;

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::DryRun).await;

    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0].usage_usd, usd(800));
    assert_eq!(periods[0].taxable_usd, usd(800));
    assert_eq!(periods[0].tax_usd, Some(usd(50)), "8.00 at 6.25% is 0.50");
    assert!(!periods[0].debited, "dry_run must not debit");
    assert_eq!(
        periods[0].recorded_reference, None,
        "dry_run must not record"
    );
    assert_eq!(balance_of(&pool, user_id).await, usd(10_000));
    assert!(tax_ledger_rows(&pool, user_id).await.is_empty());
    assert!(mock.recordings.lock().expect("mock lock").is_empty());
}

/// The full collect lifecycle in one sweep: period, price, clamped debit
/// (unclamped here), ledger row, recorded transaction — and a second sweep
/// with no new usage does nothing again.
#[tokio::test]
async fn collect_prices_debits_and_records_exactly_once() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;

    let user_id = enrolled_user(&pool, "collect", usd(10_000), Decimal::ZERO).await;
    add_usage(&pool, user_id, usd(300)).await;
    add_usage(&pool, user_id, usd(500)).await;

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;

    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods.len(), 1, "two usage rows tile into one period");
    assert_eq!(periods[0].usage_usd, usd(800));
    assert_eq!(periods[0].tax_usd, Some(usd(50)));
    assert!(periods[0].debited);
    assert_eq!(periods[0].collected_usd, Some(usd(50)));
    assert_eq!(periods[0].shortfall_usd, Some(Decimal::ZERO));
    assert_eq!(balance_of(&pool, user_id).await, usd(10_000) - usd(50));
    assert_eq!(tax_ledger_rows(&pool, user_id).await, vec![-usd(50)]);
    let reference = periods[0]
        .recorded_reference
        .clone()
        .expect("the transaction must be recorded");
    {
        let recordings = mock.recordings.lock().expect("mock lock");
        assert_eq!(recordings.len(), 1);
        assert_eq!(recordings[0].reference.as_deref(), Some(reference.as_str()));
        assert!(
            recordings[0]
                .calculation
                .as_deref()
                .is_some_and(|calculation| calculation.starts_with("taxcalc_mock_")),
            "the transaction is created from the calculation that priced the period"
        );
    }

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;
    assert_eq!(
        periods_of(&pool, user_id).await.len(),
        1,
        "no new usage, no new period"
    );
    assert_eq!(balance_of(&pool, user_id).await, usd(10_000) - usd(50));
    assert_eq!(mock.recordings.lock().expect("mock lock").len(), 1);
    assert_eq!(mock.calculations.lock().expect("mock lock").len(), 1);

    // New usage after the first period starts the next span exactly where
    // the last one ended: the two periods partition the ledger.
    add_usage(&pool, user_id, usd(1_600)).await;
    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;
    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods.len(), 2);
    assert_eq!(periods[1].usage_usd, usd(1_600));
    assert_eq!(periods[1].tax_usd, Some(usd(100)));
    assert_eq!(
        balance_of(&pool, user_id).await,
        usd(10_000) - usd(50) - usd(100)
    );
}

/// The opening-balance exemption is consumed before anything is taxable,
/// and runs out exactly once: pre-flip credit is never taxed at redemption,
/// post-flip credit always is.
#[tokio::test]
async fn the_exemption_is_consumed_before_taxing() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;

    let user_id = enrolled_user(&pool, "exempt", usd(10_000), usd(500)).await;
    add_usage(&pool, user_id, usd(800)).await;

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;

    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0].exempt_usd, usd(500));
    assert_eq!(periods[0].taxable_usd, usd(300));
    assert_eq!(periods[0].tax_usd, Some(usd(18)), "3.00 at 6.25%, floored");
    let remaining = query_scalar::<_, Decimal>(
        "SELECT redemption_tax_exempt_remaining_usd FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("exemption must query");
    assert_eq!(remaining, Decimal::ZERO, "the exemption is spent");
    {
        let calculations = mock.calculations.lock().expect("mock lock");
        assert_eq!(calculations.len(), 1);
        assert_eq!(
            calculations[0].amount.as_deref(),
            Some("300"),
            "only the taxable slice is priced"
        );
    }

    // A span that fits entirely inside the exemption is a real zero-tax
    // answer, frozen locally without a Stripe call and never recorded.
    let sheltered = enrolled_user(&pool, "sheltered", usd(10_000), usd(5_000)).await;
    add_usage(&pool, sheltered, usd(800)).await;
    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;
    let periods = periods_of(&pool, sheltered).await;
    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0].taxable_usd, Decimal::ZERO);
    assert_eq!(periods[0].tax_usd, Some(Decimal::ZERO));
    assert_eq!(
        periods[0].fallback_reason.as_deref(),
        Some("amount_below_one_cent")
    );
    assert!(periods[0].debited, "a zero-tax period is stamped collected");
    assert_eq!(periods[0].collected_usd, Some(Decimal::ZERO));
    assert_eq!(periods[0].recorded_reference, None, "nothing to file");
    assert!(tax_ledger_rows(&pool, sheltered).await.is_empty());
    assert_eq!(
        mock.calculations.lock().expect("mock lock").len(),
        1,
        "the sheltered span never reaches Stripe"
    );
}

/// The debit is clamped to the balance; the shortfall is recorded, absorbed,
/// and the full figure is still filed.
#[tokio::test]
async fn a_drained_balance_clamps_the_debit_but_not_the_filing() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;

    let user_id = enrolled_user(&pool, "clamp", usd(30), Decimal::ZERO).await;
    add_usage(&pool, user_id, usd(800)).await;

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;

    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods[0].tax_usd, Some(usd(50)));
    assert_eq!(periods[0].collected_usd, Some(usd(30)));
    assert_eq!(periods[0].shortfall_usd, Some(usd(20)));
    assert_eq!(balance_of(&pool, user_id).await, Decimal::ZERO);
    assert_eq!(tax_ledger_rows(&pool, user_id).await, vec![-usd(30)]);
    assert!(
        periods[0].recorded_reference.is_some(),
        "the full figure is filed even when the balance could not cover it"
    );
    assert_eq!(mock.recordings.lock().expect("mock lock").len(), 1);
}

/// No stored address: the period waits, unpriced — and is priced correctly
/// the day an address exists, rather than frozen wrong to quiet a log.
#[tokio::test]
async fn a_missing_address_waits_for_one_instead_of_freezing_wrong() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, mock) = mock_tax();
    let base = serve(app).await;

    let user_id = enrolled_user(&pool, "addressless", usd(10_000), Decimal::ZERO).await;
    query("UPDATE users SET tax_address_country = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("address must clear");
    add_usage(&pool, user_id, usd(800)).await;

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;
    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;
    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods.len(), 1);
    assert_eq!(periods[0].tax_usd, None, "no address, no answer — not zero");
    assert!(!periods[0].debited);
    assert!(mock.calculations.lock().expect("mock lock").is_empty());
    assert_eq!(balance_of(&pool, user_id).await, usd(10_000));

    // The webhook stores an address (here: directly), and the same period is
    // priced on the next pass.
    store_buyer_address(
        &pool,
        user_id,
        Some(&json!({
            "country": "US",
            "postal_code": "02139",
            "state": "MA",
            "city": "Cambridge",
        })),
    )
    .await;
    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::Collect).await;
    let periods = periods_of(&pool, user_id).await;
    assert_eq!(periods[0].tax_usd, Some(usd(50)));
    assert!(periods[0].debited);
}

/// An autopay-only account gets its location from the saved card.
///
/// Before this, `store_buyer_address` ran only from the checkout webhook, so a
/// user who armed autopay and never ran a manual checkout kept
/// `tax_address_country IS NULL` forever — and the sweep's "priced when the
/// user's next checkout stores one" never arrived for an account that never
/// checks out. This is the autopay half of that promise.
#[tokio::test]
async fn an_autopay_card_address_fills_an_empty_row() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;

    let user_id = enrolled_user(&pool, "autopay-only", usd(10_000), Decimal::ZERO).await;
    query("UPDATE users SET tax_address_country = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("address must clear");

    backfill_buyer_address_if_absent(
        &pool,
        user_id,
        Some(&json!({
            "country": "US",
            "postal_code": "94110",
            "state": "CA",
            "city": "San Francisco",
            "line1": "1 Valencia St",
        })),
    )
    .await;

    let stored = stored_address(&pool, user_id).await;
    assert_eq!(
        stored,
        (
            Some("US".to_owned()),
            Some("94110".to_owned()),
            Some("CA".to_owned()),
            Some("San Francisco".to_owned()),
        ),
        "an empty row must take the card's address"
    );
}

/// A card address must NEVER overwrite an address already on the row.
///
/// This is the guard that makes calling the backfill on every recurring charge
/// safe. A card saved before the setup session required a full billing address
/// can legitimately carry country + postal code and nothing else; letting that
/// win over an address the buyer typed into checkout under
/// `billing_address_collection=required` would silently coarsen every future
/// rating — and, because autopay recurs, would do it again on every charge.
#[tokio::test]
async fn an_autopay_card_address_never_overwrites_a_checkout_address() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;

    let user_id = enrolled_user(&pool, "checkout-then-autopay", usd(10_000), Decimal::ZERO).await;
    query("UPDATE users SET tax_address_country = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("address must clear");

    // The buyer checked out and gave a full address.
    store_buyer_address(
        &pool,
        user_id,
        Some(&json!({
            "country": "US",
            "postal_code": "02139",
            "state": "MA",
            "city": "Cambridge",
            "line1": "5 Main St",
        })),
    )
    .await;

    // Later, an autopay charge reads a coarse ZIP-only address off an old card.
    backfill_buyer_address_if_absent(
        &pool,
        user_id,
        Some(&json!({ "country": "US", "postal_code": "94110" })),
    )
    .await;

    let stored = stored_address(&pool, user_id).await;
    assert_eq!(
        stored,
        (
            Some("US".to_owned()),
            Some("02139".to_owned()),
            Some("MA".to_owned()),
            Some("Cambridge".to_owned()),
        ),
        "the complete checkout address must survive a coarser card address"
    );
}

/// `(country, postal_code, state, city)` — the stored components that decide
/// how precisely Stripe can rate a buyer.
type StoredAddress = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Read back the stored tax address components that decide rating precision.
async fn stored_address(pool: &PgPool, user_id: Uuid) -> StoredAddress {
    query_as::<_, StoredAddress>(
        "SELECT tax_address_country, tax_address_postal_code, tax_address_state, \
         tax_address_city FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("address row must read")
}

/// The enrollment pass itself: a user who existed before activation gets
/// their balance as the exemption, a user created after gets zero, and usage
/// from before enrollment is never swept into a period.
#[tokio::test]
async fn enrollment_splits_pre_and_post_activation_users() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = SWEEP_LOCK.lock().await;
    quiesce(&pool).await;
    let (app, _mock) = mock_tax();
    let base = serve(app).await;

    // Created "before activation" / "after activation" relative to the
    // enrollment stamps `quiesce` just wrote: the clamped timestamps make
    // the comparison deterministic whatever this database has seen before.
    let veteran = Uuid::new_v4();
    query(
        r#"
        INSERT INTO users (id, email, credit_balance_usd, created_at)
        VALUES ($1, $2, 42, TIMESTAMPTZ '2000-01-01')
        "#,
    )
    .bind(veteran)
    .bind(format!("rtx-veteran-{veteran}@example.invalid"))
    .execute(&pool)
    .await
    .expect("veteran must insert");
    add_usage(&pool, veteran, usd(700)).await;

    let newcomer = Uuid::new_v4();
    query(
        r#"
        INSERT INTO users (id, email, credit_balance_usd, created_at)
        VALUES ($1, $2, 42, TIMESTAMPTZ '2100-01-01')
        "#,
    )
    .bind(newcomer)
    .bind(format!("rtx-newcomer-{newcomer}@example.invalid"))
    .execute(&pool)
    .await
    .expect("newcomer must insert");

    run_redemption_tax_sweep_once(&pool, &settings(&base), RedemptionTaxMode::DryRun).await;

    let (veteran_exempt, veteran_cursor) = query_as::<_, (Decimal, i64)>(
        r#"
        SELECT redemption_tax_exempt_remaining_usd, redemption_tax_from_ledger_id
        FROM users WHERE id = $1
        "#,
    )
    .bind(veteran)
    .fetch_one(&pool)
    .await
    .expect("veteran enrollment must exist");
    assert_eq!(
        veteran_exempt,
        Decimal::from(42),
        "a pre-activation balance was bought tax-paid and is exempted"
    );
    assert!(veteran_cursor > 0, "the cursor sits at the ledger head");
    assert!(
        periods_of(&pool, veteran).await.is_empty(),
        "usage from before enrollment is pre-flip and never swept"
    );

    let newcomer_exempt = query_scalar::<_, Decimal>(
        "SELECT redemption_tax_exempt_remaining_usd FROM users WHERE id = $1",
    )
    .bind(newcomer)
    .fetch_one(&pool)
    .await
    .expect("newcomer enrollment must exist");
    assert_eq!(
        newcomer_exempt,
        Decimal::ZERO,
        "a post-activation deposit was never taxed at purchase; nothing to exempt"
    );
}

/// The webhook address capture: a usable address lands on the row, a
/// country-less fragment does not overwrite it.
#[tokio::test]
async fn buyer_addresses_are_kept_and_never_degraded() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("rtx-addr-{user_id}@example.invalid"))
        .execute(&pool)
        .await
        .expect("user must insert");

    store_buyer_address(
        &pool,
        user_id,
        Some(&json!({
            "country": "US",
            "postal_code": "02139",
            "state": "MA",
            "city": "Cambridge",
            "line1": "262 Hampshire St",
        })),
    )
    .await;
    let (country, postal) = query_as::<_, (Option<String>, Option<String>)>(
        "SELECT tax_address_country, tax_address_postal_code FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("address must query");
    assert_eq!(country.as_deref(), Some("US"));
    assert_eq!(postal.as_deref(), Some("02139"));

    // A fragment with no country cannot ever price tax; storing it would
    // destroy a usable address.
    store_buyer_address(&pool, user_id, Some(&json!({ "city": "Nowhere" }))).await;
    let country =
        query_scalar::<_, Option<String>>("SELECT tax_address_country FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("address must query");
    assert_eq!(country.as_deref(), Some("US"), "the fragment was refused");
}

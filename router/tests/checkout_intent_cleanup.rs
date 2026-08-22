//! The abandoned-checkout cleanup sweep (migration 0022).
//!
//! `stripe_checkout_intents` gets one row per Checkout Session created, and
//! most Checkout Sessions are never paid. The sweep removes the ones that went
//! nowhere; what these tests exist for is the boundary — every row it must
//! REFUSE to remove, because each of those is corroboration for money.
//!
//! The load-bearing assertions are the negative ones. A test that only proved
//! "abandoned rows are deleted" would pass just as happily against a sweep that
//! deleted everything, which is why each guard here is also mutation-checked:
//! disable it in the source and the matching test must go red.
//!
//! Gated on `DATABASE_URL` like the other DB suites: unset means the test
//! returns early.

use std::{path::PathBuf, str::FromStr, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    billing::{
        CheckoutIntentSweep, balance, checkout_intent, credit_purchase, settle_checkout_intent,
        sweep_expired_checkout_intents,
    },
    db::migrate,
    session::{SESSION_COOKIE, create_session},
    stripe::{STRIPE_SIGNATURE_HEADER, run_checkout_intent_cleanup_once},
    web::{StripeSettings, WebConfig, WebCtx},
};

/// The retention window `run_checkout_intent_cleanup_once` runs at, mirrored
/// here so the tests can age a row across it. Restated rather than imported
/// because `stripe::CHECKOUT_INTENT_RETENTION_DAYS` is private to the module
/// that owns the policy; `the_retention_window_stays_outside_stripes_own_windows`
/// drives the real entry point, so a change to the private constant that this
/// number no longer matches turns that test red.
const RETENTION_DAYS: i32 = 30;

/// The floor migration 0022 enforces in the database, independent of the
/// constant above.
const DB_FLOOR_DAYS: i32 = 7;

const SECRET: &str = "whsec_test_secret";

/// Serializes the sweep across this binary.
///
/// Two reasons, and both produce flakes rather than failures. The pass takes a
/// `pg_try_advisory_xact_lock`, so a second concurrent sweeper gets `Ok(None)`
/// and deletes nothing — correct in production, useless as a fixture. And the
/// sweep is GLOBAL: between the moment a test inserts an aged fixture row and
/// the moment it makes that row un-sweepable (by settling or crediting it), a
/// sibling test's sweep would happily delete it. Every test that either sweeps
/// or builds an aged fixture holds this.
static CLEANUP_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
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

async fn create_user(pool: &PgPool, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("cleanup-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

/// An intent for $25 (gross $26.38), created `age_days` ago.
///
/// `created_at` is set by the INSERT rather than backdated afterwards: 0005's
/// trigger makes it immutable, and reaching for `ALTER TABLE ... DISABLE
/// TRIGGER` to get around that would take an ACCESS EXCLUSIVE lock on a table
/// other suites are writing to. Same idiom as the autopay sweep's fixtures.
async fn aged_intent(pool: &PgPool, user_id: Uuid, age_days: i32) -> String {
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    query(
        r#"
        INSERT INTO stripe_checkout_intents (
            stripe_session_id, user_id, created_at,
            expected_amount_cents, expected_credit_usd, currency
        )
        VALUES ($1, $2, NOW() - ($3 * INTERVAL '1 day'), 2638, 25, 'usd')
        "#,
    )
    .bind(&session_id)
    .bind(user_id)
    .bind(age_days)
    .execute(pool)
    .await
    .expect("aged intent must insert");
    session_id
}

async fn intent_exists(pool: &PgPool, session_id: &str) -> bool {
    checkout_intent(pool, session_id)
        .await
        .expect("intent must query")
        .is_some()
}

async fn surviving(pool: &PgPool, sessions: &[String]) -> usize {
    let mut alive = 0;
    for session_id in sessions {
        if intent_exists(pool, session_id).await {
            alive += 1;
        }
    }
    alive
}

async fn purchase_count(pool: &PgPool, user_id: Uuid) -> i64 {
    query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'purchase'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("purchase ledger count must query")
}

/// Sweep with the production window, and assert the lock was won — a `None`
/// here would make every following assertion vacuous.
async fn sweep(pool: &PgPool, limit: i64) -> CheckoutIntentSweep {
    sweep_expired_checkout_intents(pool, RETENTION_DAYS, limit)
        .await
        .expect("sweep must query")
        .expect("the sweep lock must be free while CLEANUP_LOCK is held")
}

// ---------------------------------------------------------------------------
// What the sweep removes, and what it must not
// ---------------------------------------------------------------------------

/// The whole predicate, one row per arm, in a single pass — so the arms are
/// compared against each other rather than against four separate sweeps.
#[tokio::test]
async fn the_sweep_removes_only_abandoned_intents() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "predicate").await;

    // (a) Abandoned: unpaid, uncredited, past the window. The only arm that
    //     may go.
    let abandoned = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;

    // (b) Too young: identical in every other respect. Stripe's own windows
    //     (24h of session life plus three days of webhook retries) mean a
    //     payment for this row can still arrive.
    let young = aged_intent(&pool, user_id, RETENTION_DAYS - 1).await;

    // (c) Settled: the webhook delivered it. Old enough, but its quote is the
    //     record of a purchase.
    let settled = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    assert!(
        settle_checkout_intent(&pool, &settled)
            .await
            .expect("settle must query"),
        "the fixture must actually settle"
    );

    // (d) Credited but NOT settled — the case `settled_at IS NULL` alone gets
    //     wrong. `settle_checkout_intent` is deliberately allowed to fail after
    //     the credit commits, so this row shape exists in production, and
    //     deleting it would erase the corroboration behind a real ledger entry.
    let credited = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    credit_purchase(&pool, user_id, Decimal::from(25), &credited, None)
        .await
        .expect("credit must apply");
    assert!(
        checkout_intent(&pool, &credited)
            .await
            .expect("intent must query")
            .expect("intent must exist")
            .settled_at
            .is_none(),
        "the fixture's whole point is an unsettled row that WAS credited"
    );

    let swept = sweep(&pool, 256).await;

    assert!(
        !intent_exists(&pool, &abandoned).await,
        "an unpaid, uncredited, out-of-window intent must be swept"
    );
    assert!(
        intent_exists(&pool, &young).await,
        "an intent Stripe could still complete must survive"
    );
    assert!(
        intent_exists(&pool, &settled).await,
        "a settled intent must survive: it corroborates a delivered purchase"
    );
    assert!(
        intent_exists(&pool, &credited).await,
        "an intent whose session was CREDITED must survive even with settled_at \
         NULL — the ledger row is the authority, not the marker"
    );

    // The pass reported what it did. Other suites share this database, so the
    // figures are floors rather than equalities.
    assert!(
        swept.removed >= 1,
        "the pass must report the row it removed: {swept:?}"
    );
    assert!(
        swept.quoted_credit_usd >= Decimal::from(25),
        "abandoned credit is summed for the log line: {swept:?}"
    );
    assert!(
        swept.oldest.is_some(),
        "a non-empty pass reports how far back the backlog reached: {swept:?}"
    );

    // Nothing about this moved money.
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "the sweep must not touch a balance"
    );
    assert_eq!(purchase_count(&pool, user_id).await, 1);
}

/// The database refuses the deletes the sweep refuses, so the WHERE clause is
/// not the only thing standing between a bug and a lost purchase record.
///
/// This is the guard that survives a rewrite of the sweeping query. Each
/// statement below is what a careless hand-written cleanup would look like.
#[tokio::test]
async fn the_database_refuses_to_delete_a_row_money_touched() {
    let Some(pool) = connect().await else {
        return;
    };
    // Held because the fixtures below are briefly sweepable between insert and
    // settle/credit.
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "trigger").await;

    let settled = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    settle_checkout_intent(&pool, &settled)
        .await
        .expect("settle must query");

    let credited = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    credit_purchase(&pool, user_id, Decimal::from(25), &credited, None)
        .await
        .expect("credit must apply");

    let young = aged_intent(&pool, user_id, DB_FLOOR_DAYS - 1).await;

    for (session_id, why) in [
        (&settled, "a settled row"),
        (&credited, "a row a credit_ledger entry names"),
        (&young, "a row inside stripe's own completion window"),
    ] {
        let outcome = query("DELETE FROM stripe_checkout_intents WHERE stripe_session_id = $1")
            .bind(session_id)
            .execute(&pool)
            .await;
        assert!(
            outcome.is_err(),
            "{why} must be undeletable even by a direct DELETE"
        );
        assert!(
            intent_exists(&pool, session_id).await,
            "{why} must still be there after the refused DELETE"
        );
    }

    // TRUNCATE stays refused outright — it has no per-row predicate, so it can
    // never be proven to spare the credited rows.
    assert!(
        query("TRUNCATE stripe_checkout_intents")
            .execute(&pool)
            .await
            .is_err(),
        "TRUNCATE must remain refused"
    );

    // And the narrowing did not open the quote up: 0005's immutability is
    // unchanged.
    for statement in [
        "UPDATE stripe_checkout_intents SET expected_credit_usd = 1000 \
         WHERE stripe_session_id = $1",
        "UPDATE stripe_checkout_intents SET created_at = NOW() - INTERVAL '99 days' \
         WHERE stripe_session_id = $1",
    ] {
        assert!(
            query(statement).bind(&young).execute(&pool).await.is_err(),
            "{statement} must still be rejected"
        );
    }
}

/// The retention window must sit outside every delay Stripe can introduce.
///
/// A Checkout Session expires 24h after creation and Stripe retries a webhook
/// for up to three days, so the last legitimate `checkout.session.completed`
/// for a session is ~4 days old. This pins both numbers that keep the sweep
/// clear of it — the operating window and the floor the database enforces — so
/// lowering either is a deliberate act with a red test, not an edit.
#[tokio::test]
async fn the_retention_window_stays_outside_stripes_own_windows() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "window").await;

    // Stripe's worst case: created, paid at the 24h edge, webhook retried for
    // three more days. Asserted in a `const` block so the ordering is checked
    // when the crate COMPILES — an edit that inverted it would not even build,
    // which is stronger than a test that has to be run.
    const STRIPE_MAX_DELAY_DAYS: i32 = 1 + 3;
    const {
        assert!(
            DB_FLOOR_DAYS > STRIPE_MAX_DELAY_DAYS,
            "the database floor must exceed stripe's own maximum delay"
        );
        assert!(
            RETENTION_DAYS >= DB_FLOOR_DAYS,
            "the operating window must sit at or above the database floor"
        );
    }

    // And the constant the production entry point actually uses is the one
    // pinned above: a row just inside the window survives a real pass, a row
    // just outside it does not.
    let inside = aged_intent(&pool, user_id, RETENTION_DAYS - 1).await;
    let outside = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    run_checkout_intent_cleanup_once(&pool).await;
    assert!(
        intent_exists(&pool, &inside).await,
        "run_checkout_intent_cleanup_once must not reach inside {RETENTION_DAYS} days"
    );
    assert!(
        !intent_exists(&pool, &outside).await,
        "run_checkout_intent_cleanup_once must reach past {RETENTION_DAYS} days"
    );
}

/// The batch is bounded, so a backlog is worked off over passes rather than in
/// one statement.
#[tokio::test]
async fn the_sweep_batch_is_bounded() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "batch").await;
    // Drain anything a sibling test left sweepable, so the counts below are
    // this test's own rows and not a shared backlog.
    while sweep(&pool, 256).await.removed > 0 {}

    let mut sessions = Vec::new();
    for _ in 0..5 {
        sessions.push(aged_intent(&pool, user_id, RETENTION_DAYS + 1).await);
    }

    let first = sweep(&pool, 2).await;
    assert_eq!(first.removed, 2, "the pass must stop at the batch limit");
    assert_eq!(
        surviving(&pool, &sessions).await,
        3,
        "only the batch may go in one pass"
    );

    // Successive passes drain the rest.
    while sweep(&pool, 2).await.removed > 0 {}
    assert_eq!(
        surviving(&pool, &sessions).await,
        0,
        "repeated passes must drain the backlog"
    );
}

/// Permanently-undeletable rows must not consume the batch.
///
/// A row that was credited but whose `settled_at` stamp was lost is unsettled,
/// old, and undeletable forever — and being the oldest, it sorts to the front
/// of any `ORDER BY created_at` candidate scan. A sweep that selected
/// candidates on the cheap half of the predicate (`settled_at IS NULL` plus
/// age) and only applied the ledger check when deleting would hand itself the
/// same undeletable rows every pass, delete nothing, report success, and stop
/// working. Nothing about that is visible in a log.
///
/// This is a regression test, not a hypothetical: the first version of the
/// query did exactly that, and `the_sweep_batch_is_bounded` only caught it once
/// a previous run had left credited rows in the shared database.
#[tokio::test]
async fn permanently_undeletable_rows_do_not_starve_the_batch() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "starvation").await;
    while sweep(&pool, 256).await.removed > 0 {}

    // Three credited-but-unsettled rows, each OLDER than the abandoned one, so
    // an ordered candidate scan reaches them first.
    for age in [60, 59, 58] {
        let credited = aged_intent(&pool, user_id, age).await;
        credit_purchase(&pool, user_id, Decimal::from(25), &credited, None)
            .await
            .expect("credit must apply");
    }
    let abandoned = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;

    // A batch of two — smaller than the three undeletable rows ahead of it in
    // creation order. The abandoned row must still be found.
    let swept = sweep(&pool, 2).await;
    assert_eq!(
        swept.removed, 1,
        "the batch must be filled with DELETABLE candidates, not with rows the \
         ledger makes permanent: {swept:?}"
    );
    assert!(
        !intent_exists(&pool, &abandoned).await,
        "the abandoned row must be reachable behind older permanent ones"
    );
}

/// A second concurrent sweeper is told to stand down rather than duplicating
/// the work — the reason the pass takes an advisory lock at all.
#[tokio::test]
async fn a_second_concurrent_sweep_stands_down() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;

    // Hold the sweep's advisory lock in a transaction of our own, exactly as an
    // in-flight pass on another replica would.
    let mut holder = pool.begin().await.expect("lock transaction must begin");
    let held = query_scalar::<_, bool>(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('stripe_checkout_intent_cleanup'::TEXT, 2))",
    )
    .fetch_one(&mut *holder)
    .await
    .expect("lock must query");
    assert!(held, "the fixture must take the lock first");

    let outcome = sweep_expired_checkout_intents(&pool, RETENTION_DAYS, 8)
        .await
        .expect("a contended sweep must not error");
    assert!(
        outcome.is_none(),
        "a sweep that loses the lock must report None, not delete a batch"
    );

    // Transaction-scoped: rolling back releases it, so a crashed sweeper can
    // never wedge the next one.
    holder.rollback().await.expect("lock must release");
    assert!(
        sweep_expired_checkout_intents(&pool, RETENTION_DAYS, 8)
            .await
            .expect("sweep must query")
            .is_some(),
        "the lock must be released with the transaction"
    );
}

// ---------------------------------------------------------------------------
// The late webhook: what happens when the event arrives after the sweep
// ---------------------------------------------------------------------------

fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

/// The webhook and status routes, with an UNREACHABLE Stripe base: no arm
/// exercised here may need to talk to Stripe, and pointing it at a host that
/// cannot answer is how that is proven rather than asserted.
fn stripe_app(pool: &PgPool) -> axum::Router {
    let config = WebConfig {
        public_base_url: "http://127.0.0.1".to_owned(),
        secure_cookies: false,
        oidc: None,
        stripe: Some(StripeSettings {
            secret_key: "sk_test_unused".to_owned(),
            publishable_key: "pk_test_unused".to_owned(),
            webhook_secret: SECRET.to_owned(),
            checkout_min_usd: Decimal::from(5),
            checkout_max_usd: Decimal::from(1000),
            api_base: "https://api.stripe.invalid".to_owned(),
            // A card-only deployment, which is what this harness describes.
            crypto_rail: false,
        }),
        signup_credit_usd: Decimal::ZERO,
        portal_dist_path: PathBuf::from("portal/dist"),
        session_ttl: Duration::from_secs(3_600),
        device_client_ids: vec!["zeroclaw".to_owned()],
    };
    zerorouter::stripe::router().with_state(WebCtx::new(pool.clone(), config))
}

async fn read_json(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body should be readable")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// POST a correctly signed `checkout.session.completed` at the real handler.
///
/// $25 credit, $26.38 gross — internally consistent in every way, so the event
/// clears the self-corroboration layer and can only be refused by the missing
/// record.
async fn post_webhook(pool: &PgPool, session_id: &str, user_id: Uuid) -> (StatusCode, Value) {
    let payload = json!({
        "id": "evt_test",
        "type": "checkout.session.completed",
        "data": { "object": {
            "id": session_id,
            "object": "checkout.session",
            "payment_status": "paid",
            "amount_total": 2_638,
            "currency": "usd",
            "payment_intent": format!("pi_test_{}", Uuid::new_v4().simple()),
            "metadata": { "user_id": user_id.to_string(), "credit_usd": "25.00" },
        }}
    })
    .to_string();
    let timestamp = Utc::now().timestamp();
    let signature = sign(SECRET, timestamp, payload.as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header(
            STRIPE_SIGNATURE_HEADER,
            format!("t={timestamp},v1={signature}"),
        )
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .expect("webhook request should build");
    read_json(
        stripe_app(pool)
            .oneshot(request)
            .await
            .expect("webhook request should complete"),
    )
    .await
}

/// A `checkout.session.completed` that arrives after its intent was swept
/// credits nothing, and says so distinctly.
///
/// This arm is unreachable in production — Stripe stops retrying three days
/// after a payment and the window is thirty — but "unreachable" is a claim
/// about Stripe's behaviour, not about ZeroRouter's, so the failure mode is
/// pinned rather than assumed. The event here is a VALID one: correct HMAC,
/// self-consistent amounts, a real user. The only thing wrong with it is that
/// ZeroRouter no longer holds the record, and that alone must stop the credit.
///
/// The distinctness matters as much as the refusal. `unknown_session` is not
/// `invalid_signature`: an operator scanning Stripe's webhook dashboard has to
/// tell "a real payment was refused, reconcile it" apart from "someone is
/// posting junk at the endpoint", and the two arms answer with different codes.
#[tokio::test]
async fn a_webhook_arriving_after_the_sweep_credits_nothing() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "late-webhook").await;

    // Control: with the record present the very same event credits. This is
    // what proves the rejection below is caused by the sweep and not by a
    // malformed fixture.
    let present = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    let (before, body) = post_webhook(&pool, &present, user_id).await;
    assert_eq!(before, StatusCode::OK, "body: {body}");
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25)
    );

    // Now the real sequence, on a row that is never credited.
    let abandoned = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;
    run_checkout_intent_cleanup_once(&pool).await;
    assert!(
        !intent_exists(&pool, &abandoned).await,
        "the fixture must actually be swept"
    );

    let (status, body) = post_webhook(&pool, &abandoned, user_id).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a swept session must be refused, not acknowledged: {body}"
    );
    assert_eq!(
        body["error"]["code"],
        json!("unknown_session"),
        "the refusal must be distinguishable from signature noise"
    );
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::from(25),
        "a swept session must credit nothing"
    );
    assert_eq!(
        purchase_count(&pool, user_id).await,
        1,
        "no second purchase row may be written"
    );
    // The rejected event must not resurrect the record either.
    assert!(
        !intent_exists(&pool, &abandoned).await,
        "a rejected webhook must not recreate the record"
    );
}

// ---------------------------------------------------------------------------
// The user-facing consequence, made deliberate
// ---------------------------------------------------------------------------

/// A customer returning to a checkout tab after the window gets 404
/// `session_not_found` — the same answer as a session this deployment never
/// priced, which the portal renders as its "we could not confirm that payment"
/// banner.
///
/// Chosen, not inherited: deleting the row is what keeps the table bounded, and
/// this is the price. It is safe because a swept row was never credited, so it
/// can never deny a purchase that actually landed; and it is rare because
/// reaching it means returning to a tab a month after the session died at
/// Stripe. The alternative — tombstoning every row so this could still say
/// "expired" — buys a more precise sentence at the cost of the unbounded table
/// the sweep exists to prevent.
#[tokio::test]
async fn a_swept_session_reads_as_not_found_on_the_return_page() {
    let Some(pool) = connect().await else {
        return;
    };
    let _guard = CLEANUP_LOCK.lock().await;
    let user_id = create_user(&pool, "return-page").await;
    let session_id = aged_intent(&pool, user_id, RETENTION_DAYS + 1).await;

    run_checkout_intent_cleanup_once(&pool).await;
    assert!(!intent_exists(&pool, &session_id).await);

    let (token, _) = create_session(&pool, user_id, Duration::from_secs(3_600))
        .await
        .expect("portal session must create");
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/billing/checkout/status?session_id={session_id}"
        ))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .body(Body::empty())
        .expect("status request should build");
    // `stripe_app`'s base is unreachable, so this also pins the ordering: the
    // ownership lookup fails BEFORE Stripe is consulted. Were the lookup to
    // move after the retrieval, this would be a 502, not a 404.
    let (status, body) = read_json(
        stripe_app(&pool)
            .oneshot(request)
            .await
            .expect("status request should complete"),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a swept session must read as not-found: {body}"
    );
    assert_eq!(body["error"]["code"], json!("session_not_found"));
    assert_eq!(
        balance(&pool, user_id).await.expect("balance must query"),
        Decimal::ZERO,
        "the status endpoint never credits, swept row or not"
    );
}

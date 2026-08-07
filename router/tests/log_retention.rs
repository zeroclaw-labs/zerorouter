//! Log-retention contract, in its OWN test binary on purpose.
//!
//! The assertion needs a thread-local subscriber to see the walk's events,
//! and tracing caches callsite interest GLOBALLY: a walk running
//! concurrently on another thread — which `request_path.rs` now does in
//! bulk, since the money-conservation soak lives there — can register the
//! same callsites while this thread holds the only subscriber, and the
//! cached "not interested" verdict then hides the events this test exists
//! to read. A primer walk plus an interest rebuild narrowed that window but
//! could not close it. Cargo gives every test file its own process, which
//! closes it structurally: nothing else runs beside this.

//! Characterization tests for the authenticated request path: `POST
//! /v1/chat/completions` through admission, the candidate walk, and settlement.
//!
//! Every test drives the real axum handler over a real Postgres; only the
//! upstream leaf is swapped, via `RouterState::with_injected_route` and the
//! scriptable fakes in `zerorouter::testing` (both behind the `testing`
//! feature). The catalog is `tests/request_path_tiers.toml`, so these
//! assertions pin router behavior rather than the production candidate list.
//!
//! These tests document what the code does, not what it ought to do. Where
//! current behavior is known-wrong but not yet fixable here the actual behavior
//! is still asserted, with a comment saying so — do not "fix" the code to make
//! one of these pass; change the assertion deliberately when the behavior
//! changes. Both walks now record a `request_attempts` ledger, so the
//! known-gap assertion that used to live here is gone.
//!
//! Gated on `DATABASE_URL` like `tests/billing.rs`: when unset each test
//! returns early (skips) instead of failing.

use std::{
    io::Write,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex, PoisonError},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;
use zerorouter::provider::TokenUsage;
use zerorouter::{
    RouterState,
    api::InjectedRoute,
    app,
    auth::{generate_api_key, hash_api_key},
    billing::grant_promo,
    config::ResolvedRoute,
    db::migrate,
    logging,
    providers::{ProviderCandidate, ProviderRoute},
    testing::{FakeModelProvider, FakeOutcome},
};

/// Output bound every request asks for. Large enough that metered usage stays
/// under the reservation, so the settle debit is the metered cost and not the
/// reservation clamp (which `tests/billing.rs` already pins).
const MAX_TOKENS: u32 = 4_096;

/// Pooled connections each test opens up front. Two is enough for a request
/// that admits, walks, and settles in sequence while the test reads back rows.
const POOL_CONNECTIONS: u32 = 2;

/// What the fakes report when they serve. Chosen well below the reservation.
fn served_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(1_000),
        output_tokens: Some(20),
        cached_input_tokens: None,
    }
}

fn tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/request_path_tiers.toml")
}

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(POOL_CONNECTIONS)
        // The deadline tests reach a 15-minute constant by pausing the clock,
        // and a paused runtime auto-advances to the nearest timer whenever it
        // parks on socket I/O. A liveness ping would arm the acquire timeout
        // around exactly such a park, so acquire is kept timer-free and every
        // connection is opened up front by `warm_pool`.
        .test_before_acquire(false)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    warm_pool(&pool).await;
    Some(pool)
}

/// Open every pooled connection so no later acquire has to dial out.
async fn warm_pool(pool: &PgPool) {
    let mut connections = Vec::with_capacity(POOL_CONNECTIONS as usize);
    for _ in 0..POOL_CONNECTIONS {
        connections.push(pool.acquire().await.expect("pool connection must open"));
    }
    drop(connections);
}

/// A funded user with one API key, returned as `(api_key_id, plaintext key)`.
/// Every test gets its own so `usage_events` can be scoped by key.
async fn create_funded_key(pool: &PgPool, label: &str) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("request-path-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    let key_id = Uuid::new_v4();
    let plaintext = generate_api_key();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'request-path', 20, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(pool)
    .await
    .expect("test API key must insert");
    grant_promo(pool, user_id, Decimal::from(50), "request-path")
        .await
        .expect("funding promo must apply");
    (key_id, plaintext)
}

/// A router whose candidates are served by `fakes` in tier order. Panics if a
/// resolved route has more candidates than the test scripted, which would
/// silently leave a real (credential-less) upstream in the walk.
fn router(pool: PgPool, fakes: Vec<Arc<FakeModelProvider>>) -> RouterState {
    router_with_catalog(pool, fakes, tier_config_path())
}

/// [`router`], reading a caller-chosen catalog.
fn router_with_catalog(
    pool: PgPool,
    fakes: Vec<Arc<FakeModelProvider>>,
    catalog: PathBuf,
) -> RouterState {
    let route: InjectedRoute = Arc::new(move |resolved: &ResolvedRoute, _max_output_tokens| {
        assert_eq!(
            resolved.candidates.len(),
            fakes.len(),
            "every resolved candidate needs a scripted fake"
        );
        ProviderRoute::from_candidates(
            resolved
                .candidates
                .iter()
                .cloned()
                .zip(fakes.iter().cloned())
                .map(|(definition, fake)| ProviderCandidate::with_provider(definition, fake))
                .collect(),
        )
    });
    RouterState::with_injected_route(catalog, pool, true, route)
}

fn completion_request(key: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("completion request should build")
}

fn completion_body(model: &str, stream: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello" }],
        "max_tokens": MAX_TOKENS,
        "temperature": 0.25,
        "stream": stream,
    });
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    body
}

async fn open_reservations(pool: &PgPool, api_key_id: Uuid) -> i64 {
    query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await
        .expect("reservation count must query")
}

/// A log sink a test can read back, shaped like the one `logging::subscriber`
/// writes JSON into.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn contents(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

/// The retention contract, asserted against the REAL walk rather than a
/// synthetic event: no part of an upstream error body reaches the log sink.
///
/// `logging.rs` pins the boundary and `logging::UPSTREAM_DETAIL_TARGET` names
/// the one target allowed to carry provider text, but neither can see whether
/// the walk actually uses it. This drives a failing candidate through the real
/// handler with the real subscriber installed and reads the sink back, so
/// moving the `detail` field onto the metadata event — the regression this
/// replaces, which shipped a sanitized 500 characters of provider body under
/// `zerorouter::api` at the default `info` level — fails here.
///
/// The upstream text is scripted to contain a prompt fragment, because that is
/// what a real 4xx body echoes: the provider bails with `response.text()`
/// verbatim, and `sanitize_api_error` scrubs seven credential prefixes and
/// nothing else.
///
/// The subscriber is installed as this THREAD's default, not the process's:
/// `#[tokio::test]` builds a current-thread runtime, so the spawned walk is
/// polled on this same thread and inherits it, while the rest of the suite —
/// running on other threads — neither sees it nor writes into this buffer. The
/// two positive controls below are what prove the capture is live; without them
/// a subscriber that captured nothing at all would pass.
#[tokio::test]
async fn the_walk_never_logs_an_upstream_error_body() {
    let Some(pool) = connect().await else {
        return;
    };
    let (api_key_id, key) = create_funded_key(&pool, "logretention").await;
    // Shaped like a provider 4xx: a status line, then the request echoed back.
    let upstream_body = "400 Bad Request: {\"error\":{\"message\":\"invalid role\",\
                         \"input\":\"SECRET-PROMPT-TEXT\"}}";
    // Two outcome sets: the first walk is a PRIMER, run before the subscriber
    // is installed, whose only job is to hit — and therefore register — every
    // tracing callsite the asserted walk will use. Registration is monotonic
    // and global, so once the primer has run, installing the dispatch below
    // (whose `register_dispatch` rebuilds interest for every REGISTERED
    // callsite under the dispatchers write lock) deterministically repairs
    // them all — and no concurrent test's first-hit can poison a callsite
    // this test still needs, because there are none left to first-hit.
    // Without the primer this test raced the rest of the suite: a walk
    // callsite first hit on another test's thread caches `Interest::never`
    // against that thread's absent subscriber, and a never-interest callsite
    // skips this thread's subscriber without consulting it. The race was
    // invisible while the suite ran without DATABASE_URL (every other test
    // skipped; no concurrent walks) and surfaced the day the suites ran
    // against a real database.
    let primary = FakeModelProvider::new(
        "primary",
        vec![
            FakeOutcome::Failure(upstream_body),
            FakeOutcome::Failure(upstream_body),
        ],
    );
    let secondary = FakeModelProvider::new(
        "secondary",
        vec![
            FakeOutcome::chat("hello from secondary", served_usage()),
            FakeOutcome::chat("hello from secondary", served_usage()),
        ],
    );
    let state = router(pool.clone(), vec![primary.clone(), secondary.clone()]);

    let primer = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("primer request should complete");
    assert_eq!(primer.status(), StatusCode::OK);

    let captured = CapturedLog::default();
    // The production filter (`main.rs` defaults to `info`), widened to `trace`
    // so nothing is suppressed by level and only the retention layer can be
    // what stops the detail.
    let subscriber = logging::subscriber("trace", captured.clone());
    let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));
    // Belt-and-braces beside the primer: repairs any callsite that somehow
    // registered between the primer walk and the dispatch installation
    // above. The primer is what makes the test deterministic; this rebuild
    // costs nothing and narrows the window to zero even if the walk's
    // callsite set ever drifts from the primer's.
    tracing::callsite::rebuild_interest_cache();

    let response = app(state.clone())
        .oneshot(completion_request(
            &key,
            &completion_body("zero/test-pair", false),
        ))
        .await
        .expect("completion request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    state.wait_for_background_tasks().await;

    let logged = captured.contents();
    assert!(
        logged.contains("upstream candidate attempt failed"),
        "the walk's metadata event must reach the sink: {logged}"
    );
    assert!(
        logged.contains("openai/primary"),
        "which candidate failed is metadata and must survive: {logged}"
    );
    assert!(
        !logged.contains("SECRET-PROMPT-TEXT"),
        "an upstream body fragment reached the log sink: {logged}"
    );
    assert!(
        !logged.contains("invalid role"),
        "an upstream body fragment reached the log sink: {logged}"
    );
    assert_eq!(open_reservations(&pool, api_key_id).await, 0);
}

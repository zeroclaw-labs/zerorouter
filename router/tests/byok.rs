//! Bring-your-own-key: the four properties the feature is not allowed to lose.
//!
//! 1. **A stored credential is unreadable from the database alone.** The column
//!    holds an AES-256-GCM envelope, and nothing in it is the customer's key.
//! 2. **A stored credential is never returned.** Not by the listing, and not by
//!    the attach response that created it — paste-once means paste-once.
//! 3. **BYOK usage settles at 5% of catalog, not at catalog.** The fee is the
//!    product, and a request that quietly billed the full price would be a
//!    customer charged twenty times what they agreed to.
//! 4. **A BYOK dispatch does not carry ZeroRouter's credential.** Asserted at
//!    the wire, against a real socket, because that is the only place the
//!    question is actually answered — every layer above it is describing
//!    intent.
//!
//! The wire-level assertions live in ONE test rather than several. The seams
//! they need — `ZEROROUTER_PROVIDER_BASE_URL_*` and the providers' own
//! credential variables — are process-global, so tests that set them cannot run
//! beside each other and mean anything. That is the same reason
//! `web::tests::credit_enforcement_defaults_to_required_and_opts_out_only_explicitly`
//! is one test and not four.

use std::{
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use zerorouter::{
    RouterState, app,
    auth::{generate_api_key, hash_api_key},
    billing::grant_promo,
    byok::{self, Keyring},
    db::migrate,
    load_tier_catalog,
    provider::{ChatMessage, ChatRequest},
    providers::{ByokCredentials, ProviderRoute, byok_capable_providers, provider_accepts_byok},
};

/// The key the customer pasted. Recognizable on sight in a hex dump, which is
/// what the ciphertext assertion needs.
const CUSTOMER_KEY: &str = "sk-customer-OWNKEY-0123456789abcdef";
/// ZeroRouter's own key for the same upstream. Equally recognizable, and the
/// string the wire test proves never leaves the process on a BYOK dispatch.
const HOUSE_KEY: &str = "sk-house-ZEROROUTER-0123456789abcdef";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

fn test_keyring() -> Keyring {
    // A fixed key rather than a random one: a failure has to be reproducible,
    // and nothing here is protecting a real secret.
    Keyring::from_hex_for_tests(&"3c".repeat(32)).expect("the fixture key must build a keyring")
}

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
        .bind(format!("byok-{label}-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

// ---------------------------------------------------------------------------
// (1) The ciphertext column is unreadable on its own
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stored_credential_is_not_recoverable_from_the_column() {
    let Some(pool) = connect().await else {
        return;
    };
    let keyring = test_keyring();
    let user_id = create_user(&pool, "ciphertext").await;

    byok::attach_key(&pool, &keyring, user_id, "anthropic", CUSTOMER_KEY)
        .await
        .expect("attaching must succeed");

    // Read the raw column exactly as an attacker with a database dump would.
    let sealed = query_scalar::<_, Vec<u8>>(
        "SELECT sealed_credential FROM byok_provider_keys WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the sealed column must be readable as bytes");

    assert!(!sealed.is_empty(), "something must have been stored");

    // Byte-wise, not just as UTF-8: a plaintext run hiding inside otherwise
    // non-UTF-8 bytes would be just as disclosed, and `from_utf8_lossy` would
    // paper over it.
    assert!(
        !sealed
            .windows(CUSTOMER_KEY.len())
            .any(|window| window == CUSTOMER_KEY.as_bytes()),
        "the sealed column must not contain the credential"
    );
    // And no recognizable RUN of it either — a format that stored the key with
    // one byte changed would pass the check above and disclose everything.
    for run in ["sk-customer", "OWNKEY", "0123456789abcdef"] {
        assert!(
            !sealed
                .windows(run.len())
                .any(|window| window == run.as_bytes()),
            "the sealed column must not contain the substring {run:?}"
        );
    }

    // The whole row, not just that one column: a fingerprint or a label that
    // happened to carry the key would be the same disclosure by another route.
    let row_text = query_scalar::<_, String>(
        "SELECT byok_provider_keys::TEXT FROM byok_provider_keys WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the row must render as text");
    assert!(
        !row_text.contains(CUSTOMER_KEY) && !row_text.contains("OWNKEY"),
        "no column of the row may carry the credential: {row_text}"
    );

    // It is still the customer's key to the process that holds the keyring —
    // otherwise this test would pass on a column that stored garbage.
    let opened = byok::open_credentials(&pool, &keyring, user_id)
        .await
        .expect("opening must succeed");
    assert_eq!(
        opened,
        vec![("anthropic".to_owned(), CUSTOMER_KEY.to_owned())]
    );
}

// ---------------------------------------------------------------------------
// (2) The credential is never returned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_byok_response_body_ever_contains_the_credential() {
    let Some(pool) = connect().await else {
        return;
    };
    let keyring = test_keyring();
    let user_id = create_user(&pool, "never-returned").await;

    // The attach path's own return value is checked as well as the listing.
    // Attach is the ONLY moment the server has ever held the plaintext, so it
    // is the only moment it could leak one, and a test that only read the
    // listing would miss exactly the interesting case.
    let attached = byok::attach_key(&pool, &keyring, user_id, "anthropic", CUSTOMER_KEY)
        .await
        .expect("attaching must succeed");
    let listed = byok::list_keys(&pool, user_id)
        .await
        .expect("listing must succeed");

    for (label, rendered) in [
        ("attach", format!("{attached:?}")),
        ("list", format!("{listed:?}")),
    ] {
        assert!(
            !rendered.contains(CUSTOMER_KEY),
            "the {label} response must not contain the credential: {rendered}"
        );
        assert!(
            !rendered.contains("OWNKEY"),
            "the {label} response must not contain any run of the credential: {rendered}"
        );
    }

    // What it MAY carry, so the customer can still tell which key this is.
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider, "anthropic");
    assert_eq!(listed[0].last4, "cdef");
    assert_eq!(listed[0].fingerprint, byok::fingerprint(CUSTOMER_KEY));
    assert_eq!(
        listed[0].fingerprint.len(),
        16,
        "the fingerprint is a truncated digest, not the key"
    );
    assert!(listed[0].last_used_at.is_none(), "nothing has used it yet");
}

#[tokio::test]
async fn re_attaching_replaces_in_place_and_forgets_the_old_keys_history() {
    let Some(pool) = connect().await else {
        return;
    };
    let keyring = test_keyring();
    let user_id = create_user(&pool, "rotate").await;

    byok::attach_key(&pool, &keyring, user_id, "anthropic", CUSTOMER_KEY)
        .await
        .expect("first attach must succeed");
    byok::mark_used(&pool, user_id, "anthropic")
        .await
        .expect("marking used must succeed");
    assert!(
        byok::list_keys(&pool, user_id).await.expect("list")[0]
            .last_used_at
            .is_some()
    );

    let rotated = "sk-customer-ROTATED-fedcba9876543210";
    byok::attach_key(&pool, &keyring, user_id, "anthropic", rotated)
        .await
        .expect("re-attaching must succeed");

    let listed = byok::list_keys(&pool, user_id).await.expect("list");
    assert_eq!(
        listed.len(),
        1,
        "rotation replaces rather than accumulating"
    );
    assert_eq!(listed[0].fingerprint, byok::fingerprint(rotated));
    assert!(
        listed[0].last_used_at.is_none(),
        "a freshly pasted key has not been used, whatever the key it replaced had done"
    );

    // Detaching leaves ZeroRouter holding nothing.
    assert!(
        byok::remove_key(&pool, user_id, "anthropic")
            .await
            .expect("removing must succeed")
    );
    assert!(
        byok::list_keys(&pool, user_id)
            .await
            .expect("list")
            .is_empty()
    );
    assert!(
        !byok::remove_key(&pool, user_id, "anthropic")
            .await
            .expect("removing again must not error"),
        "a second detach reports nothing removed rather than pretending"
    );
}

#[tokio::test]
async fn one_tenants_credential_is_never_visible_to_another() {
    let Some(pool) = connect().await else {
        return;
    };
    let keyring = test_keyring();
    let owner = create_user(&pool, "owner").await;
    let neighbour = create_user(&pool, "neighbour").await;

    byok::attach_key(&pool, &keyring, owner, "anthropic", CUSTOMER_KEY)
        .await
        .expect("attaching must succeed");

    assert!(
        byok::list_keys(&pool, neighbour)
            .await
            .expect("listing must succeed")
            .is_empty(),
        "another tenant must not see the key"
    );
    assert!(
        byok::open_credentials(&pool, &keyring, neighbour)
            .await
            .expect("opening must succeed")
            .is_empty(),
        "another tenant must not dispatch on the key"
    );
    assert!(
        !byok::remove_key(&pool, neighbour, "anthropic")
            .await
            .expect("removing must not error"),
        "another tenant must not be able to detach it"
    );

    // Even a database write moving the ciphertext into the neighbour's row
    // fails to open it: the envelope is bound to (user_id, provider) as AAD.
    let stolen = query_scalar::<_, Vec<u8>>(
        "SELECT sealed_credential FROM byok_provider_keys WHERE user_id = $1",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("the sealed column must be readable");
    query(
        "INSERT INTO byok_provider_keys (user_id, provider, sealed_credential, fingerprint, last4) \
         VALUES ($1, 'anthropic', $2, 'stolen', 'ffff')",
    )
    .bind(neighbour)
    .bind(&stolen)
    .execute(&pool)
    .await
    .expect("the planted row must insert");
    assert!(
        byok::open_credentials(&pool, &keyring, neighbour)
            .await
            .expect("opening must not error")
            .is_empty(),
        "a ciphertext replanted under another tenant must not open"
    );
}

// ---------------------------------------------------------------------------
// (3) The fee arm
// ---------------------------------------------------------------------------

#[test]
fn byok_usage_settles_at_five_percent_of_catalog() {
    // The arithmetic a customer is charged, stated as the two prices for one
    // usage figure. A regression that billed BYOK at the catalog price would
    // charge twenty times what was agreed, so the assertion is written as an
    // exact equality against a hand-checked number rather than as a ratio.
    let catalog = Decimal::new(4_800_000, 6); // $4.80
    let house = byok::apply_fee(catalog, byok::house_rate()).expect("house price");
    let fee = byok::apply_fee(catalog, byok::fee_rate()).expect("byok fee");

    assert_eq!(
        house, catalog,
        "a house dispatch is billed the catalog price"
    );
    assert_eq!(fee, Decimal::new(240_000, 6), "$4.80 at 5% is $0.24");
    assert_ne!(
        fee, catalog,
        "the whole feature is that these two are different"
    );
    assert_eq!(
        fee * Decimal::from(20),
        catalog,
        "5% is exactly one twentieth, with no rounding drift"
    );
}

#[test]
fn the_fee_is_never_rounded_away_on_a_small_request() {
    // Nothing in ZeroRouter's money path rounds, and a fee that quietly floored
    // to zero would serve inference for free. Half a millionth of a dollar is
    // far below any currency's minor unit and must still be charged exactly.
    let tiny = Decimal::new(1, 7); // $0.0000001
    let fee = byok::apply_fee(tiny, byok::fee_rate()).expect("the fee must compute");
    assert_eq!(fee, Decimal::new(5, 9), "$0.0000001 at 5% is $0.000000005");
    assert!(
        fee > Decimal::ZERO,
        "a positive cost must keep a positive fee"
    );
}

// ---------------------------------------------------------------------------
// Which providers may take a customer key
// ---------------------------------------------------------------------------

#[test]
fn a_minting_or_keyless_provider_refuses_a_customer_key() {
    // `vertex` dispatches on a token exchanged from a service-account blob, and
    // its token cache is keyed by ZeroRouter's own environment variable. A
    // customer credential substituted into that path would be cached under the
    // house key's name and handed to the next tenant, so the attach is refused
    // outright rather than half-supported.
    assert!(
        !provider_accepts_byok("vertex"),
        "a minting provider must not accept a customer key"
    );
    assert!(
        !provider_accepts_byok("nothing-like-this"),
        "an unknown provider must not accept a customer key"
    );

    // The lanes that do take one are ordinary credentialed upstreams.
    for provider in ["anthropic", "openai", "google", "xai"] {
        assert!(
            provider_accepts_byok(provider),
            "{provider} should accept a customer key"
        );
    }

    // And the portal is only ever offered the intersection of "can take a key"
    // with "this deployment can dispatch to it at all".
    let offered = byok_capable_providers();
    assert!(
        !offered.iter().any(|provider| provider == "vertex"),
        "the attach form must never offer a minting provider: {offered:?}"
    );
}

// ---------------------------------------------------------------------------
// (4) The wire: whose credential actually goes out
// ---------------------------------------------------------------------------

/// An upstream that records the `Authorization` header of every request it
/// serves, and answers a minimal chat completion.
///
/// `attest` controls whether it returns the zero-data-retention header the
/// `xai` lane is sold under, so the same server can play both "a team with ZDR"
/// and "a team without it".
async fn recording_upstream(attest: bool) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                recorder.lock().expect("recorder lock").push(
                    headers
                        .get(axum::http::header::AUTHORIZATION)
                        .map(|value| value.to_str().unwrap_or("<binary>").to_owned()),
                );
                let body = axum::Json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 1}
                }));
                if attest {
                    ([("x-zero-data-retention", "true")], body).into_response()
                } else {
                    body.into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("recording upstream should bind");
    let address = listener
        .local_addr()
        .expect("recording upstream should report its address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1/chat/completions"), seen)
}

/// Dispatch one completion through a route built the production way, and return
/// whether it succeeded.
async fn dispatch(tier: &str, byok: &ByokCredentials) -> bool {
    let catalog = load_tier_catalog(&fixture("byok_tiers.toml"))
        .await
        .expect("the fixture catalog should load");
    let resolved = catalog.resolve(tier).expect("the fixture tier resolves");
    let route = ProviderRoute::new_with_byok(resolved.candidates.clone(), 64, byok)
        .await
        .expect("the route should build");
    let messages = vec![ChatMessage::user("hello")];
    route.candidates()[0]
        .chat(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            None,
        )
        .await
        .is_ok()
}

#[tokio::test]
async fn a_byok_dispatch_carries_the_customers_key_and_never_the_houses() {
    // One test, because every seam it uses is process-global: the provider base
    // URL overrides and the providers' own credential variables. Two tests
    // setting these would interleave and prove nothing.
    let (plain_url, plain_seen) = recording_upstream(false).await;
    let (attested_url, attested_seen) = recording_upstream(true).await;
    let (unattested_url, _) = recording_upstream(false).await;

    // SAFETY: this is the only test in this binary that reads or writes these
    // variables, and the cases below run in sequence within it.
    unsafe {
        std::env::set_var("GEMINI_API_KEY", HOUSE_KEY);
        std::env::set_var("XAI_API_KEY", HOUSE_KEY);
        std::env::set_var("ZEROROUTER_PROVIDER_BASE_URL_GOOGLE", &plain_url);
        std::env::set_var("ZEROROUTER_PROVIDER_BASE_URL_XAI", &attested_url);
    }

    let customer = ByokCredentials::new(vec![("google".to_owned(), CUSTOMER_KEY.to_owned())]);

    // 1. A BYOK dispatch presents the CUSTOMER's key.
    assert!(
        dispatch("zero/byok-plain", &customer).await,
        "the call should complete"
    );
    // 2. A dispatch with no attached key presents ZeroRouter's, unchanged.
    assert!(
        dispatch("zero/byok-plain", &ByokCredentials::default()).await,
        "the house call should complete"
    );

    let seen = plain_seen.lock().expect("recorder lock").clone();
    assert_eq!(
        seen,
        vec![
            Some(format!("Bearer {CUSTOMER_KEY}")),
            Some(format!("Bearer {HOUSE_KEY}")),
        ],
        "a BYOK dispatch must carry the customer's key and a house dispatch must carry \
         ZeroRouter's"
    );
    assert!(
        !seen[0]
            .as_deref()
            .expect("the BYOK call sent a header")
            .contains("ZEROROUTER"),
        "the house credential must not appear on a BYOK dispatch"
    );

    // 3. The retention decision, in both directions on the SAME lane.
    //
    // `xai` is sold under a per-response zero-data-retention assertion that
    // fails closed. On ZeroRouter's own key that guarantee is ZeroRouter's to
    // make, so an upstream that does not attest must be refused. On a
    // customer's key the header describes THEIR team, ZeroRouter is making no
    // such promise about their traffic, and asserting it would refuse service
    // over a guarantee nobody offered — so the check is skipped and the
    // response block says `byok: true` instead.
    let xai_customer = ByokCredentials::new(vec![("xai".to_owned(), CUSTOMER_KEY.to_owned())]);
    assert!(
        dispatch("zero/byok-attested", &ByokCredentials::default()).await,
        "an attesting upstream on the house key must serve"
    );
    let attested = attested_seen.lock().expect("recorder lock").clone();
    assert_eq!(
        attested,
        vec![Some(format!("Bearer {HOUSE_KEY}"))],
        "the house xai dispatch carries the house key"
    );

    // Point the lane at an upstream that does NOT attest.
    // SAFETY: as above — sequential, single test, single binary.
    unsafe {
        std::env::set_var("ZEROROUTER_PROVIDER_BASE_URL_XAI", &unattested_url);
    }
    assert!(
        !dispatch("zero/byok-attested", &ByokCredentials::default()).await,
        "a house dispatch must still FAIL CLOSED when the upstream does not attest — \
         ZeroRouter sells that lane as zero-retention"
    );
    assert!(
        dispatch("zero/byok-attested", &xai_customer).await,
        "a BYOK dispatch must NOT be refused for a missing house attestation: the customer's \
         own provider agreement governs their traffic, and ZeroRouter is not making the claim"
    );

    // 4. The price, end to end, through the real request path.
    //
    // This runs inside the same test as the wire assertions because it needs
    // the same process-global base-URL override. It is also the only assertion
    // in this file that would catch a settle site priced at the catalog rate:
    // the arithmetic tests above pin `apply_fee`, and `apply_fee` would still
    // be correct if `persist_usage` simply stopped calling it with the BYOK
    // rate.
    settles_a_byok_request_at_five_percent(&plain_url).await;
}

/// Drive one real request on a customer's own credential and read what the
/// ledger says it cost.
async fn settles_a_byok_request_at_five_percent(upstream_url: &str) {
    let Some(pool) = connect().await else {
        return;
    };
    let keyring = test_keyring();
    let user_id = create_user(&pool, "settle").await;
    let plaintext = generate_api_key();
    query(
        "INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, \
         velocity_cap_tokens_per_min) VALUES ($1, $2, $3, 'byok', 20, 1000000)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(&pool)
    .await
    .expect("test API key must insert");
    grant_promo(&pool, user_id, Decimal::from(50), "byok")
        .await
        .expect("funding promo must apply");
    byok::attach_key(&pool, &keyring, user_id, "google", CUSTOMER_KEY)
        .await
        .expect("attaching must succeed");

    // SAFETY: sequential, inside the one env-owning test in this binary.
    unsafe {
        std::env::set_var("ZEROROUTER_PROVIDER_BASE_URL_GOOGLE", upstream_url);
    }

    let state = RouterState::with_database(fixture("byok_tiers.toml"), pool.clone(), true)
        .with_byok(Some(Arc::new(test_keyring())));
    let response = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {plaintext}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "zero/byok-plain",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .expect("request should build"),
        )
        .await
        .expect("the request should complete");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body should read")
        .to_bytes();
    let body: Value = serde_json::from_slice(&body).expect("the response should be JSON");
    assert_eq!(status, StatusCode::OK, "the request should serve: {body}");

    // The disclosure. A BYOK request carries the block even though this request
    // never engaged the priority knob, because the customer has to be told
    // whose agreement governs their traffic.
    assert_eq!(
        body["zerorouter"]["byok"],
        json!(true),
        "a BYOK response must say so: {body}"
    );

    // The price. The fixture tier sells at $3.00/Mtok in and $15.00/Mtok out,
    // and the recording upstream reports 3 prompt tokens and 1 completion
    // token, so the catalog price is (3 x 3.00 + 1 x 15.00) / 1e6 = $0.000024.
    // Five percent of that is $0.0000012, and it is written exactly.
    let (cost_usd, byok_flag) = query_as::<_, (Decimal, Option<bool>)>(
        "SELECT usage_events.cost_usd, usage_events.byok FROM usage_events \
         JOIN api_keys ON api_keys.id = usage_events.api_key_id WHERE api_keys.user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("a settled usage row must exist");

    let catalog = Decimal::new(24, 6);
    assert_eq!(
        cost_usd,
        Decimal::new(12, 7),
        "a BYOK request must settle at 5% of the ${catalog} catalog price, not at it"
    );
    assert_ne!(
        cost_usd, catalog,
        "settling at the catalog price would charge the customer twenty times the fee"
    );
    assert_eq!(
        byok_flag,
        Some(true),
        "the metering row must record that this was a BYOK request"
    );

    // And the ledger debit matches the settled row, so the customer's balance
    // moved by the fee rather than by the catalog price.
    let debit = query_scalar::<_, Decimal>(
        "SELECT amount_usd FROM credit_ledger WHERE user_id = $1 AND entry_type = 'usage'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("a usage debit must exist");
    assert_eq!(debit, -Decimal::new(12, 7), "the debit is the fee, negated");

    // The last-used stamp is the one piece of state the dispatch writes back.
    let listed = byok::list_keys(&pool, user_id).await.expect("list");
    assert!(
        listed[0].last_used_at.is_some(),
        "dispatching on the credential must record that it was used"
    );
}

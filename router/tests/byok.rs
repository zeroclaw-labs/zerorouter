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
        // `false`: migration 0028's fallback opt-in is off until the customer
        // asks for it, and a freshly attached key has not.
        vec![("anthropic".to_owned(), CUSTOMER_KEY.to_owned(), false)]
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
    //
    // Three prices, because the monthly allowance (migration 0027) made one
    // request's price depend on what the customer has already run this month:
    // wholly inside the allowance is free, wholly outside is 5%, and the
    // request that straddles the boundary pays 5% of only the part above it.
    settles_free_inside_the_monthly_allowance(&plain_url).await;
    a_byok_response_labels_its_retention_as_byok_not_the_house_posture(&plain_url).await;
    settles_at_five_percent_once_the_allowance_is_spent(&plain_url).await;
    settles_on_only_the_part_above_the_allowance(&plain_url).await;
    concurrent_settles_cannot_both_claim_the_last_of_the_allowance(&plain_url).await;

    // 5. The opt-in fallback (migration 0028), against an upstream that refuses
    //    the customer's key and serves ZeroRouter's. Same test, same reason:
    //    every one of these needs the process-global base-URL override.
    let (refusing_url, refusing_seen) = upstream_refusing_the_customer(false).await;
    // SAFETY: as above — sequential, single test, single binary.
    unsafe {
        std::env::set_var("ZEROROUTER_PROVIDER_BASE_URL_GOOGLE", &refusing_url);
    }
    a_refused_key_fails_the_request_without_the_opt_in(&refusing_url, &refusing_seen).await;
    an_opted_in_key_falls_back_and_is_billed_at_full_catalog(&refusing_url, &refusing_seen).await;
    // The xai lane, whose house dispatch must attest. The upstream refuses the
    // customer's key and serves ZeroRouter's WITHOUT the header.
    let (refusing_unattested_url, _) = upstream_refusing_the_customer(false).await;
    the_fallback_attempt_carries_the_house_attestation(&refusing_unattested_url).await;
}

/// The catalog price of one fixture request.
///
/// The fixture tier sells at $3.00/Mtok in and $15.00/Mtok out, and the
/// recording upstream reports 3 prompt tokens and 1 completion token, so the
/// catalog price is (3 x 3.00 + 1 x 15.00) / 1e6 = $0.000024. Named because
/// every assertion below is arithmetic on it and a bare `Decimal::new(24, 6)`
/// repeated six times is six chances to typo the number the test is about.
const FIXTURE_CATALOG_USD: Decimal = Decimal::from_parts(24, 0, 0, false, 6);

/// One BYOK customer, funded, with a key attached and the fixture upstream
/// wired up. Returns the pool, the user, and the plaintext API key.
async fn byok_customer(label: &str, upstream_url: &str) -> Option<(PgPool, Uuid, String)> {
    let pool = connect().await?;
    let keyring = test_keyring();
    let user_id = create_user(&pool, label).await;
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
    Some((pool, user_id, plaintext))
}

/// Put `catalog_usd` of BYOK usage into this user's month WITHOUT going through
/// the request path.
///
/// It writes a `usage_events` row and lets migration 0019's accrual trigger do
/// the rest, which is the only way a test may move this number: the rollup
/// refuses direct writes (`usage_key_month_spend_reject_direct_mutation`), and
/// that refusal is a property worth not working around — a test that reached
/// past the trigger would be seeding a state the production path cannot produce.
///
/// `cost_usd` is bound to zero rather than to the fee. What this row seeds is
/// the ALLOWANCE basis, and leaving the spend total alone keeps the seeding
/// from also consuming the key's spend cap and changing what admission decides.
async fn seed_byok_month(pool: &PgPool, user_id: Uuid, catalog_usd: Decimal) {
    let api_key_id = query_scalar::<_, Uuid>("SELECT id FROM api_keys WHERE user_id = $1 LIMIT 1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the seeded user must own a key");
    query(
        "INSERT INTO usage_events (request_id, api_key_id, tier, upstream_provider, \
         upstream_model, input_tokens, cached_input_tokens, output_tokens, cost_usd, \
         latency_ms, status, byok, byok_catalog_usd) \
         VALUES ($1, $2, 'zero/byok-plain', 'google', 'seed', 0, 0, 0, 0, 0, 200, TRUE, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(api_key_id)
    .bind(catalog_usd)
    .execute(pool)
    .await
    .expect("the seed usage row must insert");

    // The trigger, not the test, is what made the number true. Asserted here so
    // a seeding helper that silently stopped working cannot make every test
    // below pass for the wrong reason.
    assert_eq!(
        consumed_allowance(pool, user_id).await,
        catalog_usd,
        "seeding must accrue through the 0019 trigger"
    );
}

/// This user's month-to-date BYOK catalog consumption, read the way admission
/// and the settle transaction read it.
async fn consumed_allowance(pool: &PgPool, user_id: Uuid) -> Decimal {
    query_scalar::<_, Decimal>(
        r#"
        SELECT COALESCE(SUM(rollup.byok_catalog_usd), 0)
        FROM usage_key_month_spend AS rollup
        INNER JOIN api_keys ON api_keys.id = rollup.api_key_id
        WHERE api_keys.user_id = $1
          AND rollup.month >= usage_event_utc_month(NOW())
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("the allowance rollup must read")
}

/// An upstream that REFUSES the customer's key and serves ZeroRouter's.
///
/// The shape the fallback opt-in exists for: a customer's credential that has
/// been revoked, expired, or mistyped. A 401 classifies as non-retryable, so the
/// walk abandons that rung immediately rather than burning its retries — which
/// is what makes "did the walk move to the house twin?" the only question the
/// tests below are asking.
///
/// `attest` controls whether the HOUSE response carries the zero-retention
/// header the `xai` lane is sold under, so the same helper can play both "the
/// fallback is allowed to serve" and "the fallback must fail closed".
async fn upstream_refusing_the_customer(attest: bool) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                let authorization = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .map(|value| value.to_str().unwrap_or("<binary>").to_owned());
                recorder
                    .lock()
                    .expect("recorder lock")
                    .push(authorization.clone());
                if authorization.as_deref() == Some(&format!("Bearer {CUSTOMER_KEY}")) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({
                            "error": {"message": "invalid api key", "type": "invalid_request_error"}
                        })),
                    )
                        .into_response();
                }
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
        .expect("refusing upstream should bind");
    let address = listener
        .local_addr()
        .expect("refusing upstream should report its address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1/chat/completions"), seen)
}

/// Drive one completion and return whatever came back, refusing nothing.
async fn attempt_completion(pool: &PgPool, plaintext: &str, model: &str) -> (StatusCode, Value) {
    let state = RouterState::with_database(
        fixture("byok_tiers.toml"),
        pool.clone(),
        true,
        Some(Arc::new(test_keyring())),
    );
    let response = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {plaintext}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": model,
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
    (
        status,
        serde_json::from_slice(&body).expect("the response should be JSON"),
    )
}

/// Drive one completion through the real HTTP path and return the response body.
async fn one_completion(pool: &PgPool, plaintext: &str) -> Value {
    let state = RouterState::with_database(
        fixture("byok_tiers.toml"),
        pool.clone(),
        true,
        Some(Arc::new(test_keyring())),
    );
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
    body
}

/// What the settled row says this user was charged, and on what basis.
async fn settled_row(pool: &PgPool, user_id: Uuid) -> (Decimal, Option<bool>, Option<Decimal>) {
    query_as::<_, (Decimal, Option<bool>, Option<Decimal>)>(
        "SELECT usage_events.cost_usd, usage_events.byok, usage_events.byok_catalog_usd \
         FROM usage_events JOIN api_keys ON api_keys.id = usage_events.api_key_id \
         WHERE api_keys.user_id = $1 AND usage_events.upstream_model <> 'seed'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("a settled usage row must exist")
}

/// The transparency headers on a BYOK request, and the one value on them that
/// is not simply the catalog's.
///
/// `x-zerorouter-retention` must read `byok` and NOT the lane's house posture.
/// The catalog's labels describe ZeroRouter's agreement with the provider, and
/// this file already pins that a BYOK dispatch is deliberately exempt from the
/// per-response attestation those labels rest on — so publishing `standard`
/// (or, worse, `zero`) here would attach ZeroRouter's claim to traffic
/// governed entirely by the customer's own contract.
///
/// It lives in this file rather than beside its siblings in
/// `tests/request_path.rs` because that harness cannot produce a BYOK
/// candidate at all: `ProviderCandidate::with_provider` hardcodes
/// `byok: false` (a test fake holds no credential), so the only place the flag
/// is genuinely true is the real assembly path this test drives.
async fn a_byok_response_labels_its_retention_as_byok_not_the_house_posture(upstream_url: &str) {
    let Some((pool, _, plaintext)) = byok_customer("retention-label", upstream_url).await else {
        return;
    };
    let state = RouterState::with_database(
        fixture("byok_tiers.toml"),
        pool.clone(),
        true,
        Some(Arc::new(test_keyring())),
    );
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
    assert_eq!(response.status(), StatusCode::OK);
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(header("x-zerorouter-provider"), "google");
    assert_eq!(header("x-zerorouter-byok"), "true");
    // The fixture pins `google` at `standard`. Reading that value here would
    // mean the label had been taken from the catalog without asking whose
    // credential served.
    assert_eq!(
        header("x-zerorouter-retention"),
        "byok",
        "a BYOK request is governed by the customer's own provider agreement, so ZeroRouter's \
         catalog posture does not describe it"
    );
}

/// A customer whose month is untouched pays NOTHING for a BYOK request.
async fn settles_free_inside_the_monthly_allowance(upstream_url: &str) {
    let Some((pool, user_id, plaintext)) = byok_customer("allowance-free", upstream_url).await
    else {
        return;
    };

    let body = one_completion(&pool, &plaintext).await;
    // The disclosure still fires. A free BYOK request is still a BYOK request,
    // and the customer still has to be told whose agreement governs it — the
    // block is about the contract, not about the price.
    assert_eq!(
        body["zerorouter"]["byok"],
        json!(true),
        "a BYOK response must say so even when it costs nothing: {body}"
    );

    let (cost_usd, byok_flag, catalog) = settled_row(&pool, user_id).await;
    assert_eq!(
        cost_usd,
        Decimal::ZERO,
        "a request wholly inside the ${} monthly allowance is free",
        byok::monthly_allowance()
    );
    assert_eq!(byok_flag, Some(true), "it was still a BYOK request");
    assert_eq!(
        catalog,
        Some(FIXTURE_CATALOG_USD),
        "the catalog basis must be recorded even when nothing is charged — it is \
         what consumes the allowance"
    );

    // A zero charge writes NO ledger row: the ledger forbids zero amounts, and
    // a $0.00 debit would be a line item for something that did not happen.
    let debits = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_ledger WHERE user_id = $1 AND entry_type = 'usage'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the ledger must count");
    assert_eq!(debits, 0, "a free request debits nothing");

    // And the allowance moved by the catalog price, not by the fee.
    assert_eq!(
        consumed_allowance(&pool, user_id).await,
        FIXTURE_CATALOG_USD,
        "the allowance is consumed in catalog dollars"
    );
}

/// Once the month's allowance is spent, BYOK prices exactly as #103 priced it.
async fn settles_at_five_percent_once_the_allowance_is_spent(upstream_url: &str) {
    let Some((pool, user_id, plaintext)) = byok_customer("allowance-spent", upstream_url).await
    else {
        return;
    };
    seed_byok_month(&pool, user_id, byok::monthly_allowance()).await;

    one_completion(&pool, &plaintext).await;

    let (cost_usd, _, catalog) = settled_row(&pool, user_id).await;
    assert_eq!(
        cost_usd,
        Decimal::new(12, 7),
        "with the allowance spent, a ${FIXTURE_CATALOG_USD} catalog request settles at 5%"
    );
    assert_eq!(catalog, Some(FIXTURE_CATALOG_USD));
    assert_ne!(
        cost_usd, FIXTURE_CATALOG_USD,
        "settling at the catalog price would charge twenty times the fee"
    );

    // The debit matches the settled row, so the balance moved by the fee.
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

/// THE straddle, end to end: the request that crosses the boundary is billed on
/// the part above it and free on the part below.
async fn settles_on_only_the_part_above_the_allowance(upstream_url: &str) {
    let Some((pool, user_id, plaintext)) = byok_customer("allowance-straddle", upstream_url).await
    else {
        return;
    };
    // Leave exactly $0.00001 of allowance — less than one fixture request's
    // $0.000024 catalog cost, so this request lands on BOTH sides of the line.
    let remaining = Decimal::new(1, 5);
    seed_byok_month(&pool, user_id, byok::monthly_allowance() - remaining).await;

    one_completion(&pool, &plaintext).await;

    // $0.000024 catalog, $0.00001 of it free, so $0.000014 is billable and the
    // fee is 5% of that = $0.0000007. Written as the literal it is, because the
    // whole point of the straddle is that this is neither of the two numbers a
    // simpler implementation would produce.
    let (cost_usd, _, catalog) = settled_row(&pool, user_id).await;
    assert_eq!(
        cost_usd,
        Decimal::new(7, 7),
        "only the ${} above the allowance is charged, at 5%",
        FIXTURE_CATALOG_USD - remaining
    );
    assert_ne!(
        cost_usd,
        Decimal::ZERO,
        "a straddling request must NOT ride free on its remaining allowance"
    );
    assert_ne!(
        cost_usd,
        Decimal::new(12, 7),
        "a straddling request must NOT be billed as though it had no allowance left"
    );
    assert_eq!(catalog, Some(FIXTURE_CATALOG_USD));

    // The accumulator takes the WHOLE catalog cost, not just the billed part:
    // it is the honest month-to-date figure, and clamping it at the boundary
    // would make the next request's arithmetic wrong.
    assert_eq!(
        consumed_allowance(&pool, user_id).await,
        byok::monthly_allowance() - remaining + FIXTURE_CATALOG_USD
    );
}

/// Two settles racing for the last of the allowance must not both win it.
async fn concurrent_settles_cannot_both_claim_the_last_of_the_allowance(upstream_url: &str) {
    let Some((pool, user_id, plaintext)) = byok_customer("allowance-race", upstream_url).await
    else {
        return;
    };
    // Leave room for EXACTLY one of the two requests below.
    seed_byok_month(
        &pool,
        user_id,
        byok::monthly_allowance() - FIXTURE_CATALOG_USD,
    )
    .await;

    // Both in flight at once. Whichever settles first consumes the remaining
    // allowance and pays nothing; the second finds none left and pays the full
    // 5%. Which of them is which is a race and the test does not care — what it
    // asserts is the SUM, which is the same either way and is what a lost
    // update would change.
    let (first, second) = tokio::join!(
        one_completion(&pool, &plaintext),
        one_completion(&pool, &plaintext)
    );
    assert_eq!(first["zerorouter"]["byok"], json!(true));
    assert_eq!(second["zerorouter"]["byok"], json!(true));

    let charged = query_scalar::<_, Decimal>(
        "SELECT COALESCE(SUM(usage_events.cost_usd), 0) FROM usage_events \
         JOIN api_keys ON api_keys.id = usage_events.api_key_id \
         WHERE api_keys.user_id = $1 AND usage_events.upstream_model <> 'seed'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the settled rows must sum");

    // If both settles read the same "one request's worth remaining" and both
    // treated themselves as covered, this would be $0. The advisory lock the
    // settle transaction takes is what makes it $0.0000012 instead: the second
    // settle cannot read the accumulator until the first has committed to it.
    assert_eq!(
        charged,
        Decimal::new(12, 7),
        "exactly one of two concurrent requests may claim the last of the allowance"
    );
    assert_ne!(
        charged,
        Decimal::ZERO,
        "both requests claiming the same last dollar of allowance is the lost update \
         this test exists to catch"
    );

    // Both requests consumed allowance, so the month is now past it by one
    // request's catalog cost.
    assert_eq!(
        consumed_allowance(&pool, user_id).await,
        byok::monthly_allowance() + FIXTURE_CATALOG_USD
    );
}

// ---------------------------------------------------------------------------
// (5) The opt-in fallback (migration 0028)
// ---------------------------------------------------------------------------

/// Without the opt-in, a refused customer key fails the request. #103's
/// structural no-fallback, still true.
async fn a_refused_key_fails_the_request_without_the_opt_in(
    upstream_url: &str,
    seen: &Arc<Mutex<Vec<Option<String>>>>,
) {
    let Some((pool, user_id, plaintext)) = byok_customer("fallback-off", upstream_url).await else {
        return;
    };
    seen.lock().expect("recorder lock").clear();

    let (status, body) = attempt_completion(&pool, &plaintext, "zero/byok-plain").await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "a refused key with no opt-in must fail rather than serve: {body}"
    );

    // The assertion that actually means something: ZeroRouter's own credential
    // never went anywhere. A fallback that fired without being asked for would
    // show up here as a second request carrying the house key.
    let calls = seen.lock().expect("recorder lock").clone();
    assert_eq!(
        calls,
        vec![Some(format!("Bearer {CUSTOMER_KEY}"))],
        "exactly one dispatch, on the customer's key, and never on ZeroRouter's"
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.as_deref().is_some_and(|c| c.contains("ZEROROUTER"))),
        "the house credential must not be presented for a customer who did not opt in"
    );

    // And nothing was billed, because nothing was served.
    let settled = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM usage_events JOIN api_keys ON api_keys.id = usage_events.api_key_id \
         WHERE api_keys.user_id = $1 AND usage_events.status = 200",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the settled rows must count");
    assert_eq!(settled, 0, "a failed walk serves nothing and bills nothing");
}

/// With the opt-in, the walk retries on ZeroRouter's key — and bills the FULL
/// catalog price for it.
async fn an_opted_in_key_falls_back_and_is_billed_at_full_catalog(
    upstream_url: &str,
    seen: &Arc<Mutex<Vec<Option<String>>>>,
) {
    let Some((pool, user_id, plaintext)) = byok_customer("fallback-on", upstream_url).await else {
        return;
    };
    assert!(
        byok::set_fallback(&pool, user_id, "google", true)
            .await
            .expect("the toggle must apply"),
        "the customer has a google key to toggle"
    );
    seen.lock().expect("recorder lock").clear();

    let (status, body) = attempt_completion(&pool, &plaintext, "zero/byok-plain").await;
    assert_eq!(status, StatusCode::OK, "the fallback must serve: {body}");

    // Both credentials went out, in that order: the customer's first, then
    // ZeroRouter's. The order is the product — a fallback that dialled the
    // house key first would be billing full price without ever trying the key
    // the customer attached.
    let calls = seen.lock().expect("recorder lock").clone();
    assert_eq!(
        calls,
        vec![
            Some(format!("Bearer {CUSTOMER_KEY}")),
            Some(format!("Bearer {HOUSE_KEY}")),
        ],
        "the customer's key is tried first and the house key only after it fails"
    );

    // The honest metadata shape. `byok` is FALSE — this request was served on
    // ZeroRouter's credential under ZeroRouter's agreement, and saying
    // otherwise would tell the customer their own provider contract governed
    // traffic it did not. `byok_fallback` is what explains the price.
    assert_eq!(
        body["zerorouter"]["byok_fallback"],
        json!(true),
        "a fallback response must say so: {body}"
    );
    assert!(
        body["zerorouter"].get("byok").is_none(),
        "a fallback attempt did NOT dispatch on the customer's credential: {body}"
    );

    // The price: FULL catalog, not the 5% fee and not free.
    let (cost_usd, byok_flag, catalog) = settled_row(&pool, user_id).await;
    assert_eq!(
        cost_usd, FIXTURE_CATALOG_USD,
        "a fallback attempt is a house dispatch and bills the full catalog price"
    );
    assert_ne!(
        cost_usd,
        Decimal::new(12, 7),
        "billing a fallback at the BYOK fee would sell house inference at 5% of cost"
    );
    assert_ne!(
        cost_usd,
        Decimal::ZERO,
        "the monthly allowance does not discount a house dispatch"
    );
    assert_eq!(
        byok_flag,
        Some(false),
        "the metering row records a house dispatch"
    );
    assert_eq!(
        catalog, None,
        "a house dispatch records no allowance basis, so it consumes none"
    );
    assert_eq!(
        consumed_allowance(&pool, user_id).await,
        Decimal::ZERO,
        "a fallback must not eat into the customer's free allowance"
    );

    // Exactly one attempt settled, and the walk ledger shows both rungs — the
    // refused customer attempt and the house one that served.
    let (attempt_count, served_count) = query_as::<_, (i64, i64)>(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE served) FROM request_attempts \
         WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the attempt rows must count");
    assert_eq!(attempt_count, 2, "both rungs are on the record");
    assert_eq!(
        served_count, 1,
        "exactly one attempt served, so exactly one attempt is priced"
    );

    // One usage debit, at the full price. No double billing.
    let debits = query_as::<_, (i64, Decimal)>(
        "SELECT COUNT(*), COALESCE(SUM(-amount_usd), 0) FROM credit_ledger \
         WHERE user_id = $1 AND entry_type = 'usage'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the ledger must count");
    assert_eq!(debits, (1, FIXTURE_CATALOG_USD));

    // The customer's key was not marked as having served anything, because it
    // did not.
    let listed = byok::list_keys(&pool, user_id).await.expect("list");
    assert!(
        listed[0].last_used_at.is_none(),
        "a key that was refused has not served a request"
    );
    assert!(listed[0].fallback_enabled, "the opt-in is reported back");
}

/// The fallback attempt is a HOUSE dispatch, so the house attestation applies
/// to it — and fails closed when the upstream will not attest.
async fn the_fallback_attempt_carries_the_house_attestation(unattested_url: &str) {
    let Some(pool) = connect().await else {
        return;
    };
    let keyring = test_keyring();
    let user_id = create_user(&pool, "fallback-attested").await;
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
    byok::attach_key(&pool, &keyring, user_id, "xai", CUSTOMER_KEY)
        .await
        .expect("attaching must succeed");
    byok::set_fallback(&pool, user_id, "xai", true)
        .await
        .expect("the toggle must apply");

    // SAFETY: sequential, inside the one env-owning test in this binary.
    unsafe {
        std::env::set_var("ZEROROUTER_PROVIDER_BASE_URL_XAI", unattested_url);
    }

    let (status, body) = attempt_completion(&pool, &plaintext, "zero/byok-attested").await;
    // The customer's own attempt is refused by the upstream (401) and asserts
    // no retention guarantee — that exemption is #103's and is untouched. The
    // FALLBACK attempt is ZeroRouter dispatching on ZeroRouter's key, so the
    // zero-retention guarantee this lane is sold under is ZeroRouter's to make
    // and is checked. The upstream does not attest, so it must refuse rather
    // than serve from a lane it cannot vouch for.
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "the fallback must fail closed on a missing house attestation: {body}"
    );
    assert_eq!(
        body["error"]["code"],
        json!("retention_attestation_failed"),
        "and it must say WHY, rather than reporting a generic upstream failure: {body}"
    );

    let settled = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM usage_events JOIN api_keys ON api_keys.id = usage_events.api_key_id \
         WHERE api_keys.user_id = $1 AND usage_events.status = 200",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the settled rows must count");
    assert_eq!(settled, 0, "nothing was served, so nothing was billed");
}

use std::{path::PathBuf, str::FromStr};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;
use zerorouter::{RouterState, app, error::ApiError, load_tier_catalog};

fn tier_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/tiers.toml")
}

/// A catalog fixture written under this test binary's target directory, at a
/// path the loader and `RouterState` can be pointed at. Each caller passes its
/// own `name` so tests running in parallel never share a file.
async fn catalog_fixture(name: &str, source: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.toml"));
    tokio::fs::write(&path, source)
        .await
        .expect("catalog fixture should write");
    path
}

/// A two-tier fixture. `zero/fixture-healthy` is always priced sanely;
/// `zero/fixture-dear` sells at 2.00/10.00 and carries `dear_basis` as its
/// candidate's input cost — at or below 2.00 for a healthy pair, above it to
/// put that one tier (and only that one) below its own cost basis.
fn two_tier_source(dear_basis: &str) -> String {
    format!(
        r#"
schema_version = 1

[tiers."zero/fixture-healthy"]
[tiers."zero/fixture-healthy".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/fixture-healthy".candidates]]
id = "openai/healthy"
provider = "openai"
model = "upstream/healthy"
[tiers."zero/fixture-healthy".candidates.rates]
input_per_mtok = 0.50
output_per_mtok = 1.00

[tiers."zero/fixture-dear"]
[tiers."zero/fixture-dear".rates]
input_per_mtok = 2.00
output_per_mtok = 10.00
[[tiers."zero/fixture-dear".candidates]]
id = "anthropic/dear"
provider = "anthropic"
model = "upstream/dear"
[tiers."zero/fixture-dear".candidates.rates]
input_per_mtok = {dear_basis}
output_per_mtok = 10.00

# Both lanes are `standard`, so this fixture's wire order stays the plain
# alphabetical one it has always asserted. The zero-first ordering has its own
# fixture (`retention_ordering_source`), where it is the thing under test.
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#
    )
}

/// A catalog with BOTH postures, for the ordering rule.
///
/// Deliberately adversarial to the sort. `openai` is the ZERO-retention
/// provider here and `anthropic`/`google` are the retaining ones, so the
/// zero-retention ids sort LAST alphabetically. A listing that merely kept its
/// old by-id order would read anthropic, google, openai; only a real
/// posture-first sort lifts the openai rows to the top. The two ids inside each
/// posture also check the alphabetical tie-break.
///
/// The postures here are fixture data chosen to exercise ordering, and bear no
/// relation to the shipped catalog — where all three providers are `standard`.
fn retention_ordering_source() -> String {
    let mut source = String::from(
        r#"
schema_version = 1

[retention.openai]
posture = "zero"
description = "fixture: retains nothing"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[retention.anthropic]
posture = "standard"
description = "fixture: retains"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[retention.google]
posture = "standard"
description = "fixture: retains"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#,
    );
    for (provider, model) in [
        ("openai", "private-b"),
        ("openai", "private-a"),
        ("anthropic", "retains-a"),
        ("google", "retains-b"),
    ] {
        source.push_str(&format!(
            r#"
[tiers."{provider}/{model}"]
[tiers."{provider}/{model}".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."{provider}/{model}".candidates]]
id = "{provider}/{model}"
provider = "{provider}"
model = "{model}"
[tiers."{provider}/{model}".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
"#
        ));
    }
    source
}

/// The parsed `/v1/models` body served by a state.
async fn models_json(state: RouterState) -> Value {
    let response = app(state)
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("models request should build"),
        )
        .await
        .expect("models request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("models response body should be readable")
        .to_bytes();
    serde_json::from_slice(&body).expect("models response should be JSON")
}

/// The `data` array of a `/v1/models` response, in wire order.
async fn listed_models(state: RouterState) -> Vec<Value> {
    models_json(state).await["data"]
        .as_array()
        .expect("models response should contain a data array")
        .clone()
}

/// The `data[].id` values of a `/v1/models` response, in wire order.
async fn listed_model_ids(state: RouterState) -> Vec<String> {
    listed_models(state)
        .await
        .iter()
        .map(|model| {
            model["id"]
                .as_str()
                .expect("every model should carry a string id")
                .to_owned()
        })
        .collect()
}

/// The parsed JSON error envelope of a response, with its status.
async fn error_envelope(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("error body should be readable")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&body).expect("error body should be JSON"),
    )
}

#[tokio::test]
async fn healthz_reports_ok() {
    let response = app(RouterState::new(tier_config_path()))
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("health request should build"),
        )
        .await
        .expect("health request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("health response body should be readable")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("health response should be JSON");
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn completion_authentication_precedes_body_buffering() {
    let options = PgConnectOptions::from_str("postgresql://unused@127.0.0.1/unused")
        .expect("lazy test database options should parse");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    let response = app(RouterState::with_database(tier_config_path(), pool, false))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .body(Body::from(vec![b'x'; 9 * 1024 * 1024]))
                .expect("completion request should build"),
        )
        .await
        .expect("completion request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn models_are_materialized_from_tiers_toml() {
    let response = app(RouterState::fully_credentialed(tier_config_path()))
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("models request should build"),
        )
        .await
        .expect("models request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("models response body should be readable")
        .to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("models response should be JSON");
    let data = json["data"]
        .as_array()
        .expect("models response should contain a data array");

    assert_eq!(json["object"], "list");
    // One row per model PIN, keyed by its OpenRouter-standard {vendor}/{model}
    // id. The pre-rename catalog published a `zero/*` alias row AND a concrete
    // candidate row per model (14 rows); each pin now equals its candidate, so
    // it contributes exactly one row — twelve models, twelve rows (the two
    // Bedrock zero-retention lanes joined on 2026-08-20).
    assert_eq!(data.len(), 12);
    assert!(data.iter().all(|model| model["object"] == "model"));

    let ids = data
        .iter()
        .map(|model| {
            model["id"]
                .as_str()
                .expect("every model carries a string id")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ids,
        std::collections::BTreeSet::from([
            "anthropic/claude-fable-5",
            "anthropic/claude-haiku-4-5",
            "anthropic/claude-opus-5",
            "anthropic/claude-sonnet-5",
            "bedrock/claude-opus-5",
            "bedrock/claude-sonnet-5",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.5-flash-lite",
            "google/gemini-3.7-flash",
            "openai/gpt-5.6-luna",
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-terra",
        ]),
        "exactly the twelve vendor-named pins — no zero/* alias, no gpt-5.3-codex. \
         The two `bedrock/*` ids serve the same weights as their `anthropic/*` \
         namesakes and are deliberately addressable apart from them: one lane \
         retains under Anthropic's 30-day policy and one retains nothing, which \
         is a difference a customer must be able to ask for by name"
    );

    // `owned_by` is the vendor, matching OpenRouter — never the string
    // "zerorouter" — and no id keeps the retired `zero/*` namespace. Combined
    // with the count above (9 rows, 9 distinct ids), this also pins that no
    // model is listed twice.
    for model in data {
        let id = model["id"].as_str().expect("id is a string");
        let owner = model["owned_by"].as_str().expect("owned_by is a string");
        // Derived from the id rather than checked against a list of vendors:
        // the rule IS that a pin's id is `{vendor}/{model}` and `owned_by` is
        // that vendor, so an id and an owner that disagree is the defect worth
        // catching. Enumerating vendors instead made adding one a test edit
        // that proved nothing.
        let vendor = id.split('/').next().expect("a pin id names its vendor");
        assert_eq!(
            owner, vendor,
            "{id} is owned_by {owner:?}, not the vendor its id names"
        );
        assert_ne!(owner, "zerorouter", "{id} must name the serving vendor");
        assert!(
            !id.starts_with("zero/"),
            "{id} still carries the retired zero/* namespace"
        );
    }
}

#[tokio::test]
async fn model_pricing_matches_zeroclaws_model_pricing_wire_contract() {
    // Literal JSON snapshot: the pricing block's field names must be exactly
    // what ZeroClaw's `ModelPricing` deserializes
    // (`crates/zeroclaw-api/src/model_provider.rs`) — `prompt`, `completion`,
    // `input_cache_read` — as decimal-string USD-per-single-token rates, and
    // its values must be the tier's sell rate from `config/tiers.toml`
    // (`openai/gpt-5.6-luna`: 0.20 input / 1.20 output / 0.02 cached USD per 1M
    // tokens) converted exactly, with no binary-float artifacts.
    //
    // The snapshot is whole-row on purpose, so it also pins that no field
    // appears that nobody asked for — including the metadata block below.
    //
    // A model that REPRICES publishes its bands as `pricing.overrides[]`,
    // OpenRouter's shape, keyed on the same `min_prompt_tokens` this catalog
    // uses. Quoting only the base rate would understate luna's real price by
    // 2x on every request past 272,000 prompt tokens, which is precisely where
    // a customer most needs the number to be right. The field is additive:
    // ZeroClaw's `ModelPricing` declares no `deny_unknown_fields` and its
    // normalizer reads `prompt`, `completion`, `input_cache_read` by name, so
    // an existing client sees exactly what it saw before.
    let data = listed_models(RouterState::fully_credentialed(tier_config_path())).await;

    let tier = data
        .iter()
        .find(|model| model["id"] == "openai/gpt-5.6-luna")
        .expect("openai/gpt-5.6-luna pin should be listed");
    assert_eq!(
        *tier,
        serde_json::json!({
            "id": "openai/gpt-5.6-luna",
            "object": "model",
            "created": 0,
            "owned_by": "openai",
            "pricing": {
                "prompt": "0.0000002",
                "completion": "0.0000012",
                "input_cache_read": "0.00000002",
                "overrides": [{
                    "min_prompt_tokens": 272_000,
                    "prompt": "0.0000004",
                    "completion": "0.0000018",
                    "input_cache_read": "0.00000004",
                }],
            },
            "context_length": 1_050_000,
            "max_output_tokens": 128_000,
            "input_modalities": ["text", "image", "pdf"],
            "tool_call": true,
            // Unlike every other field here, `retention` is present on EVERY
            // row and never omitted — a customer reading a row with no posture
            // would have to guess, and the guess a zero-retention brand invites
            // is the favourable one. `source_url` and `source_sha256` stay off
            // the wire: they are the operator's verification trail, not a claim.
            "retention": {
                "posture": "standard",
                "description": "OpenAI retains API inputs and outputs in abuse-monitoring logs for up to 30 days, unless longer retention is required by law; API data is not used to train its models.",
                "verified": "2026-08-20",
            },
        })
    );

    // The published override is the rate settlement will actually charge, not
    // a number rendered beside it: same schedule, same band, same source.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");
    let sell = catalog
        .resolve("openai/gpt-5.6-luna")
        .expect("the pin resolves")
        .sell_rates;
    assert_eq!(sell.at_prompt_tokens(272_000).input_per_mtok, Some(0.40));
    assert_eq!(
        tier["pricing"]["overrides"][0]["prompt"], "0.0000004",
        "the advertised override must be the band settlement bills at"
    );

    // A pinned concrete candidate bills at its *owning tier's* sell rate,
    // never its own cost basis — this candidate's own rates in tiers.toml
    // omit `cached_input_per_mtok` entirely, yet the served pricing still
    // carries the tier's `input_cache_read`, proving the listing reads the
    // tier rate and not the candidate rate.
    //
    // Haiku charges one price at every size, and its row below carries NO
    // `overrides` key at all — not an empty array. That is the whole
    // backwards-compatibility claim for this field, asserted as a whole-row
    // snapshot: a flat tier's JSON is byte-identical to what it published
    // before conditional rates existed.
    let candidate = data
        .iter()
        .find(|model| model["id"] == "anthropic/claude-haiku-4-5")
        .expect("the haiku candidate should be listed");
    assert_eq!(
        *candidate,
        serde_json::json!({
            "id": "anthropic/claude-haiku-4-5",
            "object": "model",
            "created": 0,
            "owned_by": "anthropic",
            "pricing": {
                "prompt": "0.000001",
                "completion": "0.000005",
                "input_cache_read": "0.0000001",
            },
            "context_length": 200_000,
            "max_output_tokens": 64_000,
            "input_modalities": ["text", "image", "pdf"],
            "tool_call": true,
            "retention": {
                "posture": "standard",
                "description": "Anthropic deletes API inputs and outputs from its backend within 30 days; longer only for Usage Policy enforcement or where law requires it.",
                "verified": "2026-08-20",
            },
        })
    );
}

#[tokio::test]
async fn every_shipped_model_publishes_what_it_can_take_and_produce() {
    // The regression guard for the bug this metadata exists to fix. A client
    // told nothing has to assume something, and ZeroClaw assumes 32,000 tokens
    // and no vision (`UNCONFIGURED_CONTEXT_WINDOW_FALLBACK`) — so a tier that
    // ships without a window does not degrade loudly, it degrades silently, on
    // exactly the long-context work someone reached for the big model to do.
    // A new tier has to come back and satisfy this test rather than discover
    // it in an agent transcript.
    for model in listed_models(RouterState::fully_credentialed(tier_config_path())).await {
        let id = model["id"].as_str().expect("every model carries an id");
        // The shipped catalog's real floor is haiku's 200k. The assertion is a
        // sanity bound, not a spec: it catches a unit slip (200 for 200k) or a
        // field that silently went missing, which are the ways this regresses.
        let window = model["context_length"].as_u64();
        assert!(
            window.is_some_and(|window| window >= 200_000),
            "{id} ships without a plausible context window: {}",
            model["context_length"]
        );
        assert!(
            model["max_output_tokens"]
                .as_u64()
                .is_some_and(|output| output >= 64_000),
            "{id} ships without a plausible max output: {}",
            model["max_output_tokens"]
        );
        // Every model in the inventory is multimodal and tool-calling. The
        // floor is asserted rather than the exact set: until Google arrived on
        // 2026-08-18 every rung took precisely text/image/pdf, and pinning that
        // literal was the stricter guard. Gemini genuinely takes video and
        // audio as well, and this table's job is to report what a model is (the
        // same kind of claim a rate is), not to flatten vendors to a common
        // denominator — `admin catalog-drift` reconciles each set against
        // models.dev, which is the check that would catch an invented modality.
        // A rung that cannot take text and images is still a real change and
        // still has to come back and edit this.
        let modalities = model["input_modalities"]
            .as_array()
            .expect("every model declares what it can take");
        for required in ["text", "image"] {
            assert!(
                modalities.iter().any(|modality| modality == required),
                "{id} does not take {required}: {:?}",
                model["input_modalities"]
            );
        }
        assert_eq!(model["tool_call"], true, "{id} should support tool calling");
    }
}

#[tokio::test]
async fn a_model_that_declares_no_metadata_omits_the_keys_rather_than_nulling_them() {
    // The optionality contract, on the wire. "Unknown" has to stay
    // distinguishable from "small", or a consumer cannot tell a checked claim
    // from a missing one — which is exactly why ZeroClaw's
    // `ModelInfo.context_window` is an `Option`. An absent key says unknown;
    // `null` would say "I looked and there is no answer", and a plausible
    // default would say something false. This fixture declares no metadata at
    // all, the shape of every tiers.toml written before the table existed.
    let path = catalog_fixture("models_no_metadata", &two_tier_source("1.00")).await;
    let data = listed_models(RouterState::fully_credentialed(path)).await;

    assert_eq!(data.len(), 4, "two tiers and their two pinned candidates");
    for model in &data {
        let id = model["id"].as_str().expect("every model carries an id");
        for field in [
            "context_length",
            "max_output_tokens",
            "input_modalities",
            "tool_call",
        ] {
            assert!(
                model.get(field).is_none(),
                "{id} should omit {field} entirely, not serve {}",
                model[field]
            );
        }
        // And the rest of the row is untouched: metadata is additive, so a
        // file that declares none still lists exactly as it did before.
        assert_eq!(model["object"], "model");
        assert!(
            model["pricing"]["prompt"].is_string(),
            "{id} should still carry its pricing"
        );
        // Retention is the deliberate exception to the rule this test pins.
        // A model may decline to describe its context window; it may never
        // decline to say what happens to the customer's prompt. The file this
        // fixture builds declares no metadata at all and still labels every
        // lane, because the catalog would not have loaded otherwise.
        assert_eq!(
            model["retention"]["posture"], "standard",
            "{id} must publish a posture even when it declares no metadata"
        );
    }
}

#[tokio::test]
async fn zero_retention_lanes_sort_before_standard_ones() {
    // THE ORDERING CLAIM, pinned. The operator tells upstream providers in
    // writing that this catalog "orders zero-retention lanes first"; this is
    // what makes that sentence true, and what fails if someone reorders the
    // `RetentionPosture` variants or drops the sort in `ModelList::from_listing`.
    //
    // The fixture is built so the two orderings disagree: by id alone the
    // answer would be anthropic, google, openai, openai — the zero lanes last.
    let path = catalog_fixture("models_retention_order", &retention_ordering_source()).await;
    let ids = listed_model_ids(RouterState::fully_credentialed(path)).await;

    assert_eq!(
        ids,
        [
            // Zero-retention first, alphabetical within the posture...
            "openai/private-a",
            "openai/private-b",
            // ...then the retaining lanes, also alphabetical.
            "anthropic/retains-a",
            "google/retains-b",
        ],
        "zero-retention lanes must be listed first, alphabetical within each posture"
    );
}

#[tokio::test]
async fn every_shipped_lane_publishes_a_retention_posture() {
    // The claim the whole feature exists to support: EVERY row carries a
    // posture, and today every one of them is honest about retaining. If a
    // future lane is pinned zero, this test is where the count changes and
    // someone has to have meant it.
    let data = listed_models(RouterState::fully_credentialed(tier_config_path())).await;

    for model in &data {
        let id = model["id"].as_str().expect("every model carries an id");
        let retention = &model["retention"];
        assert!(
            retention.is_object(),
            "{id} must publish a retention block, not omit it"
        );
        let posture = retention["posture"]
            .as_str()
            .unwrap_or_else(|| panic!("{id} must publish a posture"));
        assert!(
            matches!(posture, "zero" | "standard"),
            "{id} publishes an unknown posture {posture}"
        );
        assert!(
            retention["description"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
            "{id} must publish a human description of its posture"
        );
        assert!(
            retention["verified"]
                .as_str()
                .is_some_and(|date| date.len() == "YYYY-MM-DD".len()),
            "{id} must publish the date its posture was verified"
        );
    }

    // The shipped catalog's state, asserted rather than assumed. It was EMPTY
    // until 2026-08-20 and is now exactly two lanes — and it is pinned by NAME
    // rather than by count, because the thing that must not happen quietly is a
    // lane ACQUIRING this label, not the tally moving. A count of two would
    // still pass if someone relabelled `anthropic/*` zero and dropped Bedrock.
    //
    // A lane arriving in this list is a legal-adjacent claim about a customer's
    // data. It arrives only with evidence behind it — for these two, an enforced
    // `data_retention_mode: none` on the operator's AWS account plus AWS's
    // published semantics for that value. See docs/DEPLOY.md.
    let zero: Vec<&str> = data
        .iter()
        .filter(|model| model["retention"]["posture"] == "zero")
        .map(|model| model["id"].as_str().expect("id is a string"))
        .collect();
    assert_eq!(
        zero,
        ["bedrock/claude-opus-5", "bedrock/claude-sonnet-5"],
        "no lane may claim zero retention without a confirmed arrangement or an \
         enforced account configuration behind it"
    );
}

/// The ordering claim, over the SHIPPED catalog rather than a fixture.
///
/// `zero_retention_lanes_sort_before_standard_ones` proves the sort works on a
/// catalog built to exercise it. This proves the product actually ships in that
/// order, and until 2026-08-20 it could not: with every lane `standard` the
/// claim was vacuous, and any sort at all would have passed. Now that two lanes
/// carry `zero`, flipping `RetentionPosture::ordering_rank` — or dropping the
/// sort in `ModelList::from_listing` — fails here against the real file.
#[tokio::test]
async fn the_shipped_catalog_lists_its_zero_retention_lanes_first() {
    let data = listed_models(RouterState::fully_credentialed(tier_config_path())).await;
    let postures: Vec<&str> = data
        .iter()
        .map(|model| {
            model["retention"]["posture"]
                .as_str()
                .expect("every lane publishes a posture")
        })
        .collect();

    let first_standard = postures
        .iter()
        .position(|posture| *posture == "standard")
        .expect("the shipped catalog still has retaining lanes");
    let last_zero = postures
        .iter()
        .rposition(|posture| *posture == "zero")
        .expect("the shipped catalog now has zero-retention lanes");
    assert!(
        last_zero < first_standard,
        "a retaining lane precedes a zero-retention one in the published order: {postures:?}"
    );
}

// ---------------------------------------------------------------------------
// The catalog publishes only what this deployment can serve.
//
// These pin the fix for a production incident: `/v1/models` was credential-blind
// by design, so a deployment holding no `BEDROCK_API_KEY` advertised both
// Bedrock lanes — the zero-retention lanes the whole product leads with — while
// every request for them was refused. The catalog was "stable" and untrue.
// ---------------------------------------------------------------------------

/// THE INCIDENT, reproduced exactly: every other provider credentialed, Bedrock
/// not. The lanes must be absent, and nothing else may move.
#[tokio::test]
async fn a_lane_whose_credential_is_absent_is_not_advertised() {
    let ids = listed_model_ids(RouterState::credentialed_for(
        tier_config_path(),
        &["anthropic", "openai", "google"],
    ))
    .await;

    assert!(
        !ids.iter().any(|id| id.starts_with("bedrock/")),
        "an uncredentialed lane must not appear in /v1/models: {ids:?}"
    );
    // And the rest of the catalog is untouched — a missing key removes its own
    // lanes and nothing else. The count is the shipped twelve minus Bedrock's
    // two; asserting the survivors by name would just restate the catalog, but
    // asserting that the ONLY difference is the Bedrock pair is the claim.
    assert_eq!(ids.len(), 10, "{ids:?}");
    assert!(ids.iter().any(|id| id == "anthropic/claude-sonnet-5"));
}

/// The same rule for every other provider, because the bug was never
/// Bedrock-specific — the catalog consulted no credential for ANY lane, and
/// anthropic/openai/google only looked correct because their keys have always
/// been present in production. Each is dropped in turn.
#[tokio::test]
async fn every_provider_hides_its_own_lanes_when_its_key_is_absent() {
    const ALL: [&str; 4] = ["anthropic", "openai", "google", "bedrock"];
    for missing in ALL {
        let credentialed: Vec<&str> = ALL.into_iter().filter(|name| *name != missing).collect();
        let ids = listed_model_ids(RouterState::credentialed_for(
            tier_config_path(),
            &credentialed,
        ))
        .await;

        let prefix = format!("{missing}/");
        assert!(
            !ids.iter().any(|id| id.starts_with(&prefix)),
            "{missing} lanes must vanish when {missing} has no credential: {ids:?}"
        );
        assert!(
            ids.iter().all(|id| !id.starts_with(&prefix)) && !ids.is_empty(),
            "only {missing}'s lanes should go, not the catalog: {ids:?}"
        );
    }
}

/// A deployment with NO provider secrets publishes an empty catalog rather than
/// a full one it cannot serve.
///
/// The degenerate case, and it is asserted rather than assumed because it is
/// what a fresh environment looks like — and because "publish everything when
/// you know nothing" was precisely the old behaviour.
#[tokio::test]
async fn a_deployment_with_no_credentials_advertises_nothing() {
    let ids = listed_model_ids(RouterState::credentialed_for(tier_config_path(), &[])).await;
    assert!(ids.is_empty(), "{ids:?}");
}

#[tokio::test]
async fn bundled_tier_catalog_has_expected_virtual_models() {
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog should load");

    let mut tiers = catalog.tiers.keys().cloned().collect::<Vec<_>>();
    tiers.sort();
    assert_eq!(
        tiers,
        [
            "anthropic/claude-fable-5",
            "anthropic/claude-haiku-4-5",
            "anthropic/claude-opus-5",
            "anthropic/claude-sonnet-5",
            "bedrock/claude-opus-5",
            "bedrock/claude-sonnet-5",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.5-flash-lite",
            "google/gemini-3.7-flash",
            "openai/gpt-5.6-luna",
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-terra",
        ],
        "the twelve vendor-named model pins (Gemini flash + flash-lite added \
         2026-08-18; Gemini Pro joined them once conditional rates could \
         express the 200,000-token boundary Google prices it at; the two \
         Bedrock zero-retention lanes on 2026-08-20). Note there is NO \
         `bedrock/claude-fable-5`: Bedrock publishes `allowed_modes` per model \
         and fable-class allows only `provider_data_share`, so under this \
         account's `none` mode AWS reports it unavailable and blocks requests \
         to it — a lane that could not serve"
    );

    // One upstream each, and only from the providers ZeroRouter integrates
    // directly. A tier gaining a second rung is exactly the change that should
    // have to come back and edit this.
    for (tier_id, tier) in &catalog.tiers {
        assert_eq!(
            tier.candidates.len(),
            1,
            "{tier_id} should carry exactly one upstream"
        );
        let provider = tier.candidates[0].provider.as_str();
        assert!(
            matches!(provider, "openai" | "anthropic" | "google" | "bedrock"),
            "{tier_id} routes to {provider}, which is not in the shipped inventory"
        );
    }
}

#[tokio::test]
async fn renamed_pins_resolve_and_the_retired_zero_ids_do_not() {
    // The rename's contract at the routing layer. A request naming the
    // OpenRouter-standard {vendor}/{model} id resolves to its upstream and
    // dispatches the SAME model string as before (a customer's bill and the
    // served model are both unchanged). The retired `zero/*` alias — and the
    // dropped gpt-5.3-codex — resolve to nothing and are not withheld either:
    // they are gone, not kept as hidden aliases, so an old client pinned to
    // `zero/luna` gets a clean model_not_found rather than silently routing on.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");

    for (id, upstream) in [
        ("openai/gpt-5.6-luna", "gpt-5.6-luna"),
        ("openai/gpt-5.6-terra", "gpt-5.6-terra"),
        ("openai/gpt-5.6-sol", "gpt-5.6-sol"),
        ("anthropic/claude-haiku-4-5", "claude-haiku-4-5-20251001"),
        ("anthropic/claude-sonnet-5", "claude-sonnet-5"),
        ("anthropic/claude-opus-5", "claude-opus-5"),
        ("anthropic/claude-fable-5", "claude-fable-5"),
    ] {
        let route = catalog
            .resolve(id)
            .unwrap_or_else(|| panic!("{id} should resolve after the rename"));
        assert_eq!(route.candidates.len(), 1, "{id} pins exactly one upstream");
        assert_eq!(
            route.candidates[0].model, upstream,
            "{id} must still dispatch the unchanged upstream model"
        );
    }

    for gone in [
        "zero/luna",
        "zero/sol",
        "zero/haiku-4-5",
        "zero/sonnet-5",
        "zero/opus-5",
        "zero/fable-5",
        "zero/codex",
        "openai/gpt-5.3-codex",
    ] {
        assert!(catalog.resolve(gone).is_none(), "{gone} must not resolve");
        assert!(
            catalog.unavailable_for(gone).is_none(),
            "{gone} must not be kept as a hidden or withheld alias"
        );
    }
}

#[tokio::test]
async fn no_shipped_candidate_costs_more_than_its_tier_sells() {
    // The margin-leak regression guard, re-derived from the table rather than
    // delegated to the loader: three candidates once sat above their tier's
    // sell rate (opus in zero/sonnet-5 on all three dimensions, haiku in
    // zero/balanced on output alone) and lost money on every request they
    // served. `load_tier_catalog` now refuses such a table outright, so this
    // test failing at the assert rather than the load would mean the rule
    // itself regressed.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load and validate");

    for (tier_id, tier) in &catalog.tiers {
        for candidate in &tier.candidates {
            // Every band, not just the base one. A tier that reprices past a
            // prompt threshold holds several rate tables and a request lands
            // in exactly one of them, so a rule checked only at the base is a
            // rule that holds only for short requests. The bands pair up
            // positionally because the loader refuses a candidate whose
            // thresholds differ from its tier's.
            assert_eq!(
                candidate.rates.thresholds().collect::<Vec<_>>(),
                tier.rates.thresholds().collect::<Vec<_>>(),
                "{} in {tier_id} reprices at different prompt sizes than its tier",
                candidate.id
            );
            let bands = std::iter::once((0_u64, candidate.rates.base(), tier.rates.base())).chain(
                candidate
                    .rates
                    .conditional()
                    .iter()
                    .zip(tier.rates.conditional())
                    .map(|(basis, sell)| (basis.min_prompt_tokens, basis.rates, sell.rates)),
            );
            for (threshold, basis_rates, sell_rates) in bands {
                for (dimension, basis, sell) in [
                    (
                        "input_per_mtok",
                        basis_rates.input_per_mtok,
                        sell_rates.input_per_mtok,
                    ),
                    (
                        "output_per_mtok",
                        basis_rates.output_per_mtok,
                        sell_rates.output_per_mtok,
                    ),
                    (
                        "cached_input_per_mtok",
                        basis_rates.cached_input_per_mtok,
                        sell_rates.cached_input_per_mtok,
                    ),
                ] {
                    let (Some(basis), Some(sell)) = (basis, sell) else {
                        continue;
                    };
                    assert!(
                        basis <= sell,
                        "{} in {tier_id} has a {dimension} basis of {basis} above the tier sell \
                         rate {sell} in the band above {threshold} prompt tokens",
                        candidate.id
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn a_below_cost_tier_is_withheld_while_every_other_tier_keeps_serving() {
    // The proportionality contract, end to end through the real loader: a tier
    // that cannot cover its own candidates is dropped from the servable
    // catalog, and the tiers that have nothing to do with it are untouched.
    let path = catalog_fixture("one_below_cost", &two_tier_source("3.00")).await;
    let catalog = load_tier_catalog(&path)
        .await
        .expect("one below-cost tier must not fail the whole catalog");

    assert_eq!(
        catalog.tiers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["zero/fixture-healthy"]
    );
    assert!(catalog.resolve("zero/fixture-healthy").is_some());
    assert!(catalog.resolve("openai/healthy").is_some());
    assert!(catalog.resolve("zero/fixture-dear").is_none());

    let withheld = catalog
        .unavailable_for("zero/fixture-dear")
        .expect("the below-cost tier should be withheld, not forgotten");
    assert_eq!(withheld.tier, "zero/fixture-dear");
    assert_eq!(
        withheld.reason,
        "candidate anthropic/dear in tier zero/fixture-dear costs more than the tier sells: \
         input_per_mtok cost basis 3 exceeds tier sell rate 2"
    );
}

#[tokio::test]
async fn models_omit_a_withheld_tier_and_its_pinned_candidates() {
    // Do not sell what cannot be served: the withheld tier id and the concrete
    // candidate pinned inside it both disappear from the public catalog, so a
    // customer never sees a model that a request for it would refuse.
    let path = catalog_fixture("models_below_cost", &two_tier_source("3.00")).await;

    assert_eq!(
        listed_model_ids(RouterState::fully_credentialed(path)).await,
        ["openai/healthy", "zero/fixture-healthy"]
    );
}

#[tokio::test]
async fn a_withheld_tier_is_refused_as_misconfigured_rather_than_missing_or_transient() {
    // The honest answer for a model that exists but cannot be sold. It is not
    // a 404 (the id is right there in the file), and it is not the generic
    // catalog 503 (which reads as a blip that clears itself). Its own code and
    // a message naming the tier are what tell an operator where to look.
    let (status, body) = error_envelope(
        ApiError::ModelUnavailable {
            tier: "anthropic/claude-sonnet-5".to_owned(),
        }
        .into_response(),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_ne!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "model_unavailable");
    assert_eq!(body["error"]["param"], "model");
    let message = body["error"]["message"]
        .as_str()
        .expect("the error message should be a string");
    assert!(message.contains("anthropic/claude-sonnet-5"), "{message}");
    assert!(message.contains("below its own cost basis"), "{message}");
    assert!(message.contains("not a transient outage"), "{message}");

    // Distinct from both neighbours it must never be confused with.
    let (not_found, not_found_body) = error_envelope(ApiError::ModelNotFound.into_response()).await;
    assert_eq!(not_found, StatusCode::NOT_FOUND);
    assert_eq!(not_found_body["error"]["code"], "model_not_found");
    let (_, catalog_body) = error_envelope(ApiError::TierCatalogUnavailable.into_response()).await;
    assert_eq!(catalog_body["error"]["code"], "tier_catalog_unavailable");
}

#[tokio::test]
async fn a_structural_fault_still_refuses_the_whole_catalog() {
    // Only the *economic* verdict is per tier. A duplicate concrete id is
    // cross-tier by nature and makes the file ambiguous about what a customer
    // buys, so it still refuses everything — including the healthy tier — even
    // though it is spliced into a tier that is itself below cost.
    let source = format!(
        r#"{}
[[tiers."zero/fixture-dear".candidates]]
id = "openai/healthy"
provider = "openai"
model = "upstream/healthy"
[tiers."zero/fixture-dear".candidates.rates]
input_per_mtok = 0.10
output_per_mtok = 0.20
"#,
        two_tier_source("3.00")
    );
    let path = catalog_fixture("structural_fault", &source).await;

    assert!(
        load_tier_catalog(&path).await.is_err(),
        "a duplicate concrete id must refuse the whole catalog"
    );

    let response = app(RouterState::fully_credentialed(path))
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("models request should build"),
        )
        .await
        .expect("models request should complete");
    let (status, body) = error_envelope(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "tier_catalog_unavailable");
}

#[tokio::test]
async fn a_catalog_with_no_servable_tier_left_refuses_to_load() {
    // Degrading every tier is not degradation, it is an outage that pretends
    // to be a catalog: an empty listing would 404 every model in the file.
    // Fail the load instead.
    let source = two_tier_source("3.00").replace(
        r#"[tiers."zero/fixture-healthy".candidates.rates]
input_per_mtok = 0.50"#,
        r#"[tiers."zero/fixture-healthy".candidates.rates]
input_per_mtok = 9.00"#,
    );
    let path = catalog_fixture("all_below_cost", &source).await;

    assert!(
        load_tier_catalog(&path).await.is_err(),
        "a catalog with nothing left to sell must fail to load"
    );
}

#[tokio::test]
async fn a_basis_hike_above_sell_withholds_that_tier_and_nothing_else() {
    // The withhold mechanism, pinned against the *shipped* table with a
    // real event it now models: Sonnet 5's introductory rate lapses on
    // 2026-08-31, taking its basis from 2.00/0.20/10.00 to 3.00/0.30/15.00.
    // The table sells at the intro rate — we do not sell at a profit — so on
    // that day the basis crosses ABOVE sell and this mechanism is what stands
    // between a lapsed promo and serving sonnet below cost. The sonnet candidate
    // basis (and only it — the tier's sells stay exactly as shipped) is raised
    // above sell, and the catalog must lose anthropic/claude-sonnet-5 alone
    // while every other tier keeps serving.
    let shipped = tokio::fs::read_to_string(tier_config_path())
        .await
        .expect("shipped catalog should read");
    let split = shipped
        .find(r#"[[tiers."anthropic/claude-sonnet-5".candidates]]"#)
        .expect("the shipped catalog should define the sonnet pin candidates");
    let (head, candidates) = shipped.split_at(split);
    let lapsed = format!(
        "{head}{}",
        candidates
            .replace("input_per_mtok = 2.00", "input_per_mtok = 3.00")
            .replace(
                "cached_input_per_mtok = 0.20",
                "cached_input_per_mtok = 0.30"
            )
            .replace("output_per_mtok = 10.00", "output_per_mtok = 15.00")
    );
    let path = catalog_fixture("sonnet_intro_lapsed", &lapsed).await;

    let catalog = load_tier_catalog(&path)
        .await
        .expect("a lapsed sonnet intro price must not take the catalog down");

    // Every tier that has nothing to do with Sonnet's pricing still routes,
    // addressed by its vendor/model pin id.
    assert_eq!(
        catalog.tiers.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "anthropic/claude-fable-5",
            "anthropic/claude-haiku-4-5",
            "anthropic/claude-opus-5",
            // The Bedrock lanes are untouched by a first-party Anthropic
            // repricing: different account, different rate card, own pins.
            "bedrock/claude-opus-5",
            "bedrock/claude-sonnet-5",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.5-flash-lite",
            "google/gemini-3.7-flash",
            "openai/gpt-5.6-luna",
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-terra",
        ]
    );
    for model in [
        "openai/gpt-5.6-luna",
        "anthropic/claude-haiku-4-5",
        "openai/gpt-5.6-terra",
    ] {
        assert!(
            catalog.resolve(model).is_some(),
            "{model} must still resolve after sonnet goes below cost"
        );
    }

    // The sonnet PIN is withheld. Its tier id and its pinned candidate id are
    // now the same string, so one id answers for both. Withholding is the SAFE
    // outcome of a lapsed promo: the tier stops serving rather than serving
    // below cost.
    let requested = "anthropic/claude-sonnet-5";
    assert!(catalog.resolve(requested).is_none(), "{requested}");
    let withheld = catalog
        .unavailable_for(requested)
        .unwrap_or_else(|| panic!("{requested} should report itself as withheld"));
    assert_eq!(withheld.tier, "anthropic/claude-sonnet-5");
    assert!(
        withheld
            .reason
            .contains("cost basis 3 exceeds tier sell rate 2"),
        "{}",
        withheld.reason
    );
    // The tier's own sell rates are untouched — only the cost basis moved,
    // which is exactly what the calendar does to this table on 2026-08-31.
    let sell = catalog.unavailable["anthropic/claude-sonnet-5"]
        .definition
        .rates
        .base();
    assert_eq!(sell.input_per_mtok, Some(2.00));
    assert_eq!(sell.output_per_mtok, Some(10.00));

    // And the public catalog stops advertising what it cannot serve: the eleven
    // surviving pins, one row each. `bedrock/claude-sonnet-5` is among the
    // survivors and that is the point of naming it — the two lanes serve the
    // same weights on different accounts, so a repricing of the first-party
    // one withholds only the first-party one.
    let listed = listed_model_ids(RouterState::fully_credentialed(path)).await;
    assert_eq!(listed.len(), 11);
    assert!(listed.iter().any(|id| id == "bedrock/claude-sonnet-5"));
    assert!(listed.iter().any(|id| id == "openai/gpt-5.6-luna"));
    assert!(listed.iter().any(|id| id == "anthropic/claude-haiku-4-5"));

    // The mispriced PIN is gone.
    assert!(
        !listed.iter().any(|id| id == "anthropic/claude-sonnet-5"),
        "the withheld pin must not be advertised: {listed:?}"
    );
}

#[tokio::test]
async fn the_shipped_catalog_withholds_no_tier_today() {
    // Regression guard for the mechanism above: withholding is a response to a
    // real mispricing, never something the shipped table trips on its own. If
    // this fails, the product lost a tier in production.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");

    assert!(
        catalog.unavailable.is_empty(),
        "the shipped catalog withholds {:?}",
        catalog.unavailable.keys().collect::<Vec<_>>()
    );
    assert_eq!(catalog.tiers.len(), 12);
}

/// Every conditional rate the shipped catalog declares, transcribed from
/// models.dev on 2026-08-18 and asserted here so an edit to `tiers.toml` is
/// caught by `cargo test` rather than only by the networked drift check.
///
/// `(tier, threshold, input, cached, output)`. The two thresholds are
/// different on purpose — OpenAI reprices the 5.6 family at 272,000 and Google
/// reprices Gemini Pro at 200,000 — and a future edit that "tidies" them into
/// one number is exactly what this table is here to stop.
const SHIPPED_CONDITIONAL_RATES: [(&str, u64, f64, f64, f64); 4] = [
    ("openai/gpt-5.6-luna", 272_000, 0.40, 0.04, 1.80),
    ("openai/gpt-5.6-terra", 272_000, 4.00, 0.40, 18.00),
    ("openai/gpt-5.6-sol", 272_000, 10.00, 1.00, 45.00),
    ("google/gemini-3.1-pro-preview", 200_000, 4.00, 0.40, 18.00),
];

#[tokio::test]
async fn every_shipped_conditional_rate_is_the_one_the_vendor_publishes() {
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");

    for (tier_id, threshold, input, cached, output) in SHIPPED_CONDITIONAL_RATES {
        let tier = catalog
            .tiers
            .get(tier_id)
            .unwrap_or_else(|| panic!("{tier_id} must be in the shipped catalog"));
        let bands = tier.rates.conditional();
        assert_eq!(bands.len(), 1, "{tier_id} declares {} bands", bands.len());
        assert_eq!(bands[0].min_prompt_tokens, threshold, "{tier_id} boundary");
        assert_eq!(
            bands[0].rates.input_per_mtok,
            Some(input),
            "{tier_id} input"
        );
        assert_eq!(
            bands[0].rates.cached_input_per_mtok,
            Some(cached),
            "{tier_id} cached"
        );
        assert_eq!(
            bands[0].rates.output_per_mtok,
            Some(output),
            "{tier_id} output"
        );

        // The band is what a long request is actually billed at, and the base
        // is what a short one is: the two must be genuinely different, or the
        // table is decoration.
        let route = catalog.resolve(tier_id).expect("a shipped pin resolves");
        assert_eq!(route.sell_rates.at_prompt_tokens(threshold), bands[0].rates);
        assert_eq!(
            route.sell_rates.at_prompt_tokens(threshold - 1),
            tier.rates.base()
        );
        assert_ne!(bands[0].rates, tier.rates.base(), "{tier_id}");
    }

    // And no OTHER shipped tier quietly grew a band. A conditional table is a
    // price change; it does not arrive by accident.
    let declared: std::collections::BTreeSet<&str> = SHIPPED_CONDITIONAL_RATES
        .iter()
        .map(|(tier, ..)| *tier)
        .collect();
    for (tier_id, tier) in &catalog.tiers {
        assert_eq!(
            !tier.rates.conditional().is_empty(),
            declared.contains(tier_id.as_str()),
            "{tier_id}'s conditional rates disagree with the table in this test"
        );
    }
}

/// The routed tiers vs the pass-through tiers, told apart explicitly so
/// each keeps its own structural promise (the test below enforces them).
// The MVP integrates OpenAI and Anthropic directly, and no model is served
// by both, so no tier can meet the routed shape's availability floor (>=2
// rungs across >=2 providers). Every tier is pass-through until a second
// provider serves a model class again.
const ROUTED_TIERS: [&str; 0] = [];
const PASS_THROUGH_TIERS: [&str; 12] = [
    // Model pins, keyed by their OpenRouter-standard {vendor}/{model} ids.
    "openai/gpt-5.6-luna",
    "anthropic/claude-haiku-4-5",
    "openai/gpt-5.6-terra",
    "anthropic/claude-sonnet-5",
    "openai/gpt-5.6-sol",
    "anthropic/claude-opus-5",
    "anthropic/claude-fable-5",
    "google/gemini-3.7-flash",
    "google/gemini-3.5-flash-lite",
    "google/gemini-3.1-pro-preview",
    // The Bedrock zero-retention lanes (2026-08-20). Pass-through like every
    // other pin, and note what that means here: they cost 10% MORE than the
    // `anthropic/*` lanes serving the same weights, because the mantle
    // endpoint is in-region only and AWS meters in-region traffic at a 10%
    // premium over the global inference profile. Selling that through at cost
    // is the promise; the assertion below (candidate rates == tier rates) is
    // what holds it, and it would equally catch someone "fixing" these down to
    // 2.00/10.00 on the sell side only.
    "bedrock/claude-sonnet-5",
    "bedrock/claude-opus-5",
];

#[tokio::test]
async fn every_shipped_tier_keeps_its_structural_promise() {
    // Two deliberate shapes, each pinned. ROUTED tiers keep the
    // availability floor: at least two rungs across at least two providers,
    // so no single upstream outage silences a tier. PASS-THROUGH tiers are
    // the opposite promise — the customer asked for one specific model at
    // list price, so exactly one candidate, priced at the flagship shape
    // (basis == sell on every dimension): the margin on these rows is
    // volume discounts, never markup or routing. Every shipped tier must be
    // one shape or the other — a tier in neither list is a policy decision
    // nobody made.
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog must load");

    for (tier_id, tier) in &catalog.tiers {
        if ROUTED_TIERS.contains(&tier_id.as_str()) {
            let providers = tier
                .candidates
                .iter()
                .map(|candidate| candidate.provider.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            assert!(
                tier.candidates.len() >= 2 && providers.len() >= 2,
                "{tier_id} has {} candidate(s) across {} provider(s)",
                tier.candidates.len(),
                providers.len()
            );
        } else if PASS_THROUGH_TIERS.contains(&tier_id.as_str()) {
            assert_eq!(
                tier.candidates.len(),
                1,
                "{tier_id} is pass-through: one model, one rung"
            );
            let candidate = &tier.candidates[0];
            // The WHOLE schedule, thresholds included. A pin that sold at list
            // below 272,000 tokens and at a markup above it would still be a
            // markup, and comparing only the base tables is exactly how that
            // would go unnoticed.
            assert_eq!(
                candidate.rates, tier.rates,
                "{tier_id} must sell at list on EVERY dimension (cached input included) and in \
                 EVERY band: margin is volume discounts, never markup"
            );
        } else {
            panic!("{tier_id} is neither routed nor pass-through — decide");
        }
    }
}

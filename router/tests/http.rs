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
    // it contributes exactly one row — twenty-two models, twenty-two rows. All
    // eight arrived on 2026-08-20: the four Bedrock classic-runtime
    // zero-retention lanes, the five open-weight Fireworks lanes, the
    // closed-weight `fireworks/qwen3.8-max` carrying a per-tier `standard`
    // retention override, and the two xAI Grok lanes whose zero-retention claim
    // is re-attested on every response. (The two Bedrock mantle lanes added with
    // the rest are commented out in `tiers.toml` because AWS's per-account Sales
    // gate refuses this account 5-generation Claude, so they could be listed but
    // never served.) The three `vertex/*` lanes added 2026-08-21 are the same
    // three Gemini models the `google/*` pins serve, reached over Vertex AI on
    // a zero-retention Google Cloud project instead of the retaining Developer
    // API — the second pair of twins in the catalog, and the first where the
    // zero-retention half costs the customer no more than the retaining one.
    assert_eq!(data.len(), 25);
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
            "bedrock/claude-haiku-4-5",
            "bedrock/claude-opus-4-5",
            "bedrock/claude-opus-4-6",
            "bedrock/claude-sonnet-4-5",
            "fireworks/deepseek-v4-flash",
            "fireworks/deepseek-v4-pro",
            "fireworks/glm-5.2",
            "fireworks/kimi-k3",
            "fireworks/minimax-m3",
            "fireworks/qwen3.8-max",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.5-flash-lite",
            "google/gemini-3.7-flash",
            "openai/gpt-5.6-luna",
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-terra",
            "vertex/gemini-3.1-pro-preview",
            "vertex/gemini-3.5-flash-lite",
            "vertex/gemini-3.7-flash",
            "xai/grok-4.3",
            "xai/grok-4.6",
        ]),
        "exactly the vendor-named pins — no zero/* alias, no gpt-5.3-codex. \
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
    // THE TWO EXCEPTION LISTS BELOW ARE THE GUARD, not holes in it, and they
    // arrived with the Fireworks lanes on 2026-08-20.
    //
    // Until then every shipped model was a multimodal frontier model that
    // published every limit, so "assert it of all of them" and "assert it of
    // each of them deliberately" were the same test. The open-weight lineup
    // breaks that honestly: GLM 5.2 and both DeepSeek V4 lanes genuinely take
    // no images, and Fireworks documents no max-output figure that agrees with
    // models.dev for the DeepSeek pair or MiniMax M3, so `tiers.toml` states
    // nothing rather than guessing (each omission argues itself there).
    //
    // Loosening the assertion to "text-only is fine, absent is fine" would have
    // deleted the guard, because the bug it was written for — a new tier
    // shipping with no window at all, and every ZeroClaw agent silently
    // assuming 32,000 — would then pass. Naming the exceptions keeps the teeth:
    // a lane that omits metadata must appear here, so the omission is reviewed
    // once, in the diff, by a human. A NEW tier that forgets its metadata is not
    // on these lists and still fails.
    // The xAI lanes joined both lists on 2026-08-20, and they are the first
    // entries here whose omission is not about a model being modest. Both are
    // multimodal frontier models; what they lack is an AGREED number. xAI
    // publishes no per-model output cap at all (its REST reference documents
    // only a 128,000 default for `max_completion_tokens`), while models.dev
    // asserts 500,000 for grok-4.6 — identical to its context window, the same
    // tell that disqualified MiniMax M3's figure — and 30,000 for grok-4.3,
    // which no model with a 128,000 default could honour. On modalities the
    // vendor says text and image, models.dev adds `pdf`, and xAI's documents
    // path runs through the Files API that ZDR disables, so `pdf` may be true of
    // the model and false of this lane. `tiers.toml` argues each omission at the
    // tier.
    const NO_MAX_OUTPUT: [&str; 6] = [
        "fireworks/deepseek-v4-flash",
        "fireworks/deepseek-v4-pro",
        "fireworks/minimax-m3",
        // models.dev says 131072; neither Fireworks page documents a max output
        // at all, and the only figure on one of them is a sample request's
        // `max_tokens: 4000` — 32x lower, and a statement about an example
        // rather than about the model. Same shape as the DeepSeek pair above.
        "fireworks/qwen3.8-max",
        "xai/grok-4.3",
        "xai/grok-4.6",
    ];
    const NO_IMAGE_INPUT: [&str; 7] = [
        "fireworks/deepseek-v4-flash",
        "fireworks/deepseek-v4-pro",
        "fireworks/glm-5.2",
        // Omits the modality list entirely: Fireworks' own two pages contradict
        // each other about whether this model takes images.
        "fireworks/minimax-m3",
        // Also omits it entirely, and for the mirror-image reason: here
        // fireworks.ai says "Not supported" while app.fireworks.ai says
        // "Supported" and demonstrates an image URL. models.dev sides with
        // text-only. MiniMax was the same 2-1 split the other way and was also
        // left unstated — a client that wrongly believes it can send an image
        // builds a request that fails.
        "fireworks/qwen3.8-max",
        // Likewise omit the list entirely rather than claim they take no images
        // — both DO take images; the two sources disagree only about `pdf`.
        "xai/grok-4.3",
        "xai/grok-4.6",
    ];

    for model in listed_models(RouterState::fully_credentialed(tier_config_path())).await {
        let id = model["id"].as_str().expect("every model carries an id");
        // The shipped catalog's real floor is MiniMax M3's 512k, then haiku's
        // 200k. The assertion is a sanity bound, not a spec: it catches a unit
        // slip (200 for 200k) or a field that silently went missing, which are
        // the ways this regresses. NO lane is exempt from this one — a context
        // window is the single claim whose absence caused the original bug.
        let window = model["context_length"].as_u64();
        assert!(
            window.is_some_and(|window| window >= 200_000),
            "{id} ships without a plausible context window: {}",
            model["context_length"]
        );
        if NO_MAX_OUTPUT.contains(&id) {
            assert!(
                model["max_output_tokens"].is_null(),
                "{id} is listed as declining to state a max output; it now states one, so \
                 remove it from NO_MAX_OUTPUT rather than leaving a stale exemption"
            );
        } else {
            assert!(
                model["max_output_tokens"]
                    .as_u64()
                    .is_some_and(|output| output >= 64_000),
                "{id} ships without a plausible max output: {}",
                model["max_output_tokens"]
            );
        }
        // Modalities: the floor is asserted rather than the exact set, because
        // this table's job is to report what a model IS (the same kind of claim
        // a rate is), not to flatten vendors to a common denominator — `admin
        // catalog-drift` reconciles each set against models.dev, which is the
        // check that would catch an invented modality. Text is required of
        // every lane that declares anything at all; images only of the lanes
        // that actually take them.
        if NO_IMAGE_INPUT.contains(&id) {
            if let Some(modalities) = model["input_modalities"].as_array() {
                assert!(
                    modalities.iter().any(|modality| modality == "text"),
                    "{id} declares modalities without text: {:?}",
                    model["input_modalities"]
                );
                assert!(
                    !modalities.iter().any(|modality| modality == "image"),
                    "{id} is listed as taking no images and now claims to; remove it from \
                     NO_IMAGE_INPUT rather than leaving a stale exemption: {:?}",
                    model["input_modalities"]
                );
            }
        } else {
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
        }
        // No exemption here, and there should not be one: every model this
        // catalog sells is bought to drive an agent.
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
    // until 2026-08-20 and is now fourteen lanes — and it is pinned by NAME rather
    // than by count, because the thing that must not happen quietly is a lane
    // ACQUIRING this label, not the tally moving. A count would still pass if
    // someone relabelled `anthropic/*` zero and dropped an existing lane.
    //
    // A lane arriving in this list is a legal-adjacent claim about a customer's
    // data, and it arrives only with evidence behind it. The fourteen below rest
    // on THREE DIFFERENT KINDS of evidence, which is the reason to keep reading
    // rather than to append the next lane by pattern-match:
    //
    //   bedrock/*    an ENFORCED `data_retention_mode: none` on the operator's
    //                own AWS account, plus AWS's published semantics for that
    //                value — re-verifiable live against the account.
    //   fireworks/*  Fireworks' PUBLISHED DEFAULT for every customer, quoted
    //                from its own security documentation. There is no account
    //                state to re-read, so the pinned page hash is the entire
    //                re-verification loop.
    //   xai/*        an ENFORCED team-level ZDR setting in the xAI Console —
    //                the same basis as Bedrock's, with a re-verification loop
    //                that is different in kind rather than in degree: xAI
    //                restates the guarantee in an `x-zero-data-retention`
    //                header on EVERY response, and the dispatch path refuses to
    //                serve a response that does not attest `true`
    //                (`crate::wire::ResponseAttestation`). These are the only
    //                two lanes in the catalog whose posture is checked at
    //                request time rather than on a date in the past.
    //   vertex/*     an ENFORCED project configuration on the operator's own
    //                Google Cloud project — in-memory caching disabled, no
    //                request-response logging, and an account out of scope for
    //                abuse-monitoring prompt logging — plus Google's published
    //                semantics for each of those controls. Basis 2 again, and
    //                the same three models the `google/*` lanes serve
    //                `standard`: same weights, same price, different product,
    //                different data policy. Its cache setting is re-readable
    //                live (`projects/PROJECT_ID/cacheConfig`); its
    //                abuse-monitoring scope is NOT, and `[retention.vertex]`
    //                says so rather than glossing it.
    //
    // NOT EVERY `fireworks/*` LANE IS HERE, and the absentee is the point.
    // `fireworks/qwen3.8-max` dispatches the same account on the same key, and
    // it is deliberately missing from this list because it is CLOSED-WEIGHT and
    // the sentence backing the Fireworks pin is scoped to open models. It ships
    // `standard` via a per-tier `[tiers."fireworks/qwen3.8-max".retention]`
    // override — the first in the file — and this assertion is the tripwire for
    // that override being deleted or weakened: inheritance would put the lane
    // back in this list, and the failure would name it. Do not "fix" such a
    // failure by adding the id here.
    //
    // See docs/DEPLOY.md, "The rule for `posture = zero`", which names three
    // admissible bases and the conditions on each.
    let zero: Vec<&str> = data
        .iter()
        .filter(|model| model["retention"]["posture"] == "zero")
        .map(|model| model["id"].as_str().expect("id is a string"))
        .collect();
    assert_eq!(
        zero,
        [
            "bedrock/claude-haiku-4-5",
            "bedrock/claude-opus-4-5",
            "bedrock/claude-opus-4-6",
            "bedrock/claude-sonnet-4-5",
            "fireworks/deepseek-v4-flash",
            "fireworks/deepseek-v4-pro",
            "fireworks/glm-5.2",
            "fireworks/kimi-k3",
            "fireworks/minimax-m3",
            // The three vertex lanes are DELIBERATELY absent: their provider
            // pin sits at `standard` while Google's abuse-monitoring exception
            // (filed 2026-08-21) is pending — see the pin's comment in
            // tiers.toml. When approval lands and the pin flips back to zero,
            // they rejoin this list. Do not add them here before that.
            "xai/grok-4.3",
            "xai/grok-4.6"
        ],
        "no lane may claim zero retention without a confirmed arrangement, an \
         enforced account configuration, or the vendor's published default behind it"
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
// THE PER-TIER RETENTION OVERRIDE, ON THE WIRE.
//
// The override landed as a mechanism with no user: `TierCatalog::
// candidate_retention` preferred a tier's own pin over its provider's, one unit
// test in `config.rs` covered that preference on a hand-built catalog, and no
// shipped tier exercised it. Every `/v1/models` retention test above therefore
// proved only the PROVIDER-level path, because that was the only path the real
// file took.
//
// `fireworks/qwen3.8-max` is the first genuine use, and it is the case where
// getting it wrong is worst: the override is the ONLY thing standing between a
// closed-weight lane and a `zero` label its provider's evidence does not cover.
// So the override is asserted end to end here — through the real `tiers.toml`,
// through `model_listing`, through the sort, out to the JSON a customer reads —
// rather than trusted because a resolver unit test passes.
// ---------------------------------------------------------------------------

/// The override beats the provider pin, on the shipped file, at the wire.
///
/// Deliberately asserts BOTH halves against each other in one test: the lane's
/// provider is pinned `zero` and the lane publishes `standard`. Asserting only
/// the second would pass just as well if someone flipped `[retention.fireworks]`
/// to `standard` wholesale — which would be a different and much larger change,
/// silently demoting five zero-retention lanes — so the test pins that the
/// provider is still `zero` and that this one row diverges from it anyway. That
/// divergence IS the mechanism; nothing else in the catalog produces it.
#[tokio::test]
async fn a_closed_weight_fireworks_lane_publishes_standard_from_its_tier_override() {
    let data = listed_models(RouterState::fully_credentialed(tier_config_path())).await;

    let row = data
        .iter()
        .find(|model| model["id"] == "fireworks/qwen3.8-max")
        .expect("the closed-weight Fireworks lane must be published");
    assert_eq!(
        row["retention"]["posture"], "standard",
        "the per-tier override is what keeps this lane out of the zero-retention \
         group; it published {:?} instead",
        row["retention"]["posture"]
    );

    // Its sibling lanes on the SAME provider and the SAME credential still
    // publish `zero`. This is what makes the assertion above an override rather
    // than a provider-wide relabelling.
    for sibling in [
        "fireworks/kimi-k3",
        "fireworks/glm-5.2",
        "fireworks/deepseek-v4-pro",
        "fireworks/deepseek-v4-flash",
        "fireworks/minimax-m3",
    ] {
        let row = data
            .iter()
            .find(|model| model["id"] == sibling)
            .unwrap_or_else(|| panic!("{sibling} must be published"));
        assert_eq!(
            row["retention"]["posture"], "zero",
            "{sibling} inherits `[retention.fireworks]` and must still be zero — \
             if this fails the provider pin moved, which is not what the Qwen \
             override was supposed to do"
        );
    }

    // The description must give the REASON, not a generic label. The whole
    // argument for selling a closed-weight lane under a zero-retention brand is
    // that the row says plainly why it is not zero; a description that merely
    // restated the posture would leave the customer to guess whether ZeroRouter
    // had checked. `open models` is the scope limit the claim turns on.
    let description = row["retention"]["description"]
        .as_str()
        .expect("the override publishes a description");
    assert!(
        description.contains("open models"),
        "the override's description must name the scope limit that makes this \
         lane standard, not just assert the posture: {description}"
    );
    assert!(
        description.contains("closed-weight"),
        "the override's description must say what this lane is, so the reason \
         reads as a fact about the model rather than a hedge: {description}"
    );
}

/// The override decides SORT ORDER too, not just the label.
///
/// A row could carry `standard` and still be printed among the zero lanes if the
/// sort read the provider pin — or read nothing. `/v1/models` leads with the
/// lanes that keep the promise, so a retaining lane sitting inside that block is
/// a false impression created by ordering alone, which no amount of correct
/// labelling further down the row would undo.
#[tokio::test]
async fn the_overridden_lane_sorts_into_the_standard_group_not_the_zero_one() {
    let data = listed_models(RouterState::fully_credentialed(tier_config_path())).await;
    let ids: Vec<&str> = data
        .iter()
        .map(|model| model["id"].as_str().expect("id is a string"))
        .collect();

    let qwen = ids
        .iter()
        .position(|id| *id == "fireworks/qwen3.8-max")
        .expect("the overridden lane is published");
    let last_zero = data
        .iter()
        .rposition(|model| model["retention"]["posture"] == "zero")
        .expect("the shipped catalog has zero-retention lanes");
    assert!(
        qwen > last_zero,
        "the overridden lane must sort BELOW every zero-retention lane; it landed \
         at {qwen} with the last zero lane at {last_zero}: {ids:?}"
    );

    // ...and INSIDE the standard block, which is a stronger claim than the line
    // above and is what catches the posture sort being removed outright. This
    // lane's id sorts after every other `fireworks/*` id alphabetically, so a
    // catalog sorted by id alone would still place it after the Fireworks zero
    // lanes and satisfy the assertion above by accident. It would not satisfy
    // this one: without the posture sort the `anthropic/*` standard lanes come
    // first and the two groups interleave.
    let first_standard = data
        .iter()
        .position(|model| model["retention"]["posture"] == "standard")
        .expect("the shipped catalog has retaining lanes");
    assert!(
        last_zero < first_standard,
        "the two postures must form contiguous blocks, zero first: last zero at \
         {last_zero}, first standard at {first_standard}: {ids:?}"
    );
    assert!(
        qwen >= first_standard,
        "the overridden lane must sit inside the standard block, not merely after \
         the zero ones: {ids:?}"
    );

    // And specifically below its own provider's lanes, which is the ordering a
    // reader is most likely to find surprising and most needs to be able to
    // trust: same vendor prefix, different block.
    for sibling in ["fireworks/kimi-k3", "fireworks/minimax-m3"] {
        let at = ids
            .iter()
            .position(|id| *id == sibling)
            .unwrap_or_else(|| panic!("{sibling} is published"));
        assert!(
            qwen > at,
            "{sibling} is zero-retention and must precede the overridden lane: {ids:?}"
        );
    }
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
    // lanes and nothing else. The count is the shipped twenty minus Bedrock's
    // four AND minus Fireworks' six, because this deployment names only three
    // providers and so holds neither key. Note the Fireworks six includes
    // `fireworks/qwen3.8-max`, whose retention override does NOT make it a
    // separate provider: dispatchability follows the credential, and it goes
    // dark with its siblings. Asserting the survivors by name would just restate
    // the catalog; asserting that the ONLY lanes lost are the ones whose
    // credentials are absent is the claim.
    assert_eq!(ids.len(), 10, "{ids:?}");
    assert!(ids.iter().any(|id| id == "anthropic/claude-sonnet-5"));
}

/// The same rule for every other provider, because the bug was never
/// Bedrock-specific — the catalog consulted no credential for ANY lane, and
/// anthropic/openai/google only looked correct because their keys have always
/// been present in production. Each is dropped in turn.
#[tokio::test]
async fn every_provider_hides_its_own_lanes_when_its_key_is_absent() {
    const ALL: [&str; 5] = ["anthropic", "openai", "google", "bedrock", "fireworks"];
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
            "bedrock/claude-haiku-4-5",
            "bedrock/claude-opus-4-5",
            "bedrock/claude-opus-4-6",
            "bedrock/claude-sonnet-4-5",
            "fireworks/deepseek-v4-flash",
            "fireworks/deepseek-v4-pro",
            "fireworks/glm-5.2",
            "fireworks/kimi-k3",
            "fireworks/minimax-m3",
            "fireworks/qwen3.8-max",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.5-flash-lite",
            "google/gemini-3.7-flash",
            "openai/gpt-5.6-luna",
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-terra",
            "vertex/gemini-3.1-pro-preview",
            "vertex/gemini-3.5-flash-lite",
            "vertex/gemini-3.7-flash",
            "xai/grok-4.3",
            "xai/grok-4.6",
        ],
        "the twenty-five vendor-named model pins (Gemini flash + flash-lite added \
         2026-08-18; Gemini Pro joined them once conditional rates could \
         express the 200,000-token boundary Google prices it at; the four \
         Bedrock classic-runtime zero-retention lanes on 2026-08-20, the \
         five open-weight Fireworks lanes the same day, closed-weight \
         Qwen 3.8 Max alongside them under a per-tier retention override, and \
         the two xAI Grok lanes the same day again; and the three Vertex \
         Gemini lanes on 2026-08-21). \
         THE THREE `vertex/*` IDS NAME THE SAME MODELS AS THE THREE \
         `google/*` ONES and that is the point rather than a duplicate: they \
         are two different Google products under two different data policies. \
         `google/*` is the Gemini Developer API, which logs prompts for an \
         unstated period; `vertex/*` is Vertex AI on the operator's own \
         zero-retention Google Cloud project. Unlike the Bedrock twins the \
         zero-retention half costs no more — Vertex's global-endpoint price \
         for these models is identical to the Developer API's. The dispatched \
         model strings differ from the ids by a `google/` prefix, which is \
         the vendor's own naming on Vertex's OpenAI-compatible surface. \
         \
         THE XAI IDS ARE THE VENDOR'S IDS, exactly: `grok-4.6` and `grok-4.3` \
         are what xAI dispatches, so these two pins keep this file's original \
         promise literally. The `-latest` aliases xAI also publishes are \
         deliberately NOT pinned — an alias that follows the newest release is \
         not a pin, and its price would move without an edit. \
         \
         THE FIREWORKS IDS ARE `fireworks/<model>` AND THE DISPATCHED STRINGS \
         ARE NOT: Fireworks addresses models as \
         `accounts/fireworks/models/<slug>`, which carries two slashes and so \
         cannot be a {{vendor}}/{{model}} id at all, and it writes decimals as \
         `p` (`glm-5p2`). `fireworks/glm-5.2` (`glm-5p2`) and \
         `fireworks/qwen3.8-max` (`qwen3p8-max`) are the two whose model \
         strings differ by more than a prefix. \
         \
         QWEN 3.8 MAX IS CLOSED-WEIGHT AND IS HERE ANYWAY, which is the one \
         Fireworks lane that does not inherit `[retention.fireworks]`. The \
         vendor's zero-retention statement is scoped to open models, so \
         inheriting would stretch a `zero` label past its evidence; instead \
         the tier carries its own complete `standard` retention override — the \
         first per-tier override in the shipped file. Qwen 3.7 Plus is still \
         absent, by operator choice rather than by any technical bar. \
         \
         THE BEDROCK LANES ARE 4.5-GENERATION AND THAT IS NOT A TYPO. The two \
         5-generation `bedrock/claude-{{opus,sonnet}}-5` tiers were added the \
         same day against Bedrock's MANTLE plane and are commented out in \
         `tiers.toml`: AWS gates 5-generation Claude per account behind Sales \
         and refuses this one, so every call 403s with `not available for this \
         account`. Credential-presence filtering cannot see an entitlement, so \
         the catalog file is the only honest place to record it. These four \
         ride the CLASSIC RUNTIME plane, which hosts 4.5- and 4.6-generation \
         Claude and nothing else — so when the account is ungated the catalog gains the \
         5-generation pair ALONGSIDE these rather than replacing them. \
         \
         Note there is also NO `bedrock/claude-fable-5`: Bedrock publishes \
         `allowed_modes` per model and fable-class allows only \
         `provider_data_share`, so under this account's `none` mode AWS \
         reports it unavailable and blocks requests to it — a lane that could \
         not serve"
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
            matches!(
                provider,
                "openai" | "anthropic" | "google" | "bedrock" | "fireworks" | "xai" | "vertex"
            ),
            "{tier_id} routes to {provider}, which is not in the shipped inventory"
        );
    }
}

/// The shipped Bedrock lanes dispatch on the CLASSIC RUNTIME plane, with the
/// geographic inference-profile ids that plane requires.
///
/// Three claims in one test because they only work together, and each fails as
/// an opaque AWS error rather than as anything a reader would recognise:
///
/// - `surface = "classic_runtime"` selects the InvokeModel wire. Drop it and
///   these dispatch on the mantle plane's Messages wire instead — which, for
///   this account, is the plane that 403s.
/// - The model ids carry a `us.` prefix. They are geographic inference profiles;
///   the bare and dated forms are documented In-Region N/A on this plane and are
///   refused. Strip the prefix and every request fails.
/// - The ids are 4.5-generation. The classic runtime plane is the only plane
///   that hosts them, and it does not host the 5-generation models — so these
///   are not a downgrade waiting to be "fixed" upward, they are the models this
///   plane serves.
#[tokio::test]
async fn the_bedrock_lanes_dispatch_the_runtime_plane_with_geographic_profiles() {
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog should load");

    let bedrock: Vec<(&str, &str, Option<&str>)> = catalog
        .tiers
        .values()
        .flat_map(|tier| &tier.candidates)
        .filter(|candidate| candidate.provider == "bedrock")
        .map(|candidate| {
            (
                candidate.id.as_str(),
                candidate.model.as_str(),
                candidate.surface.as_deref(),
            )
        })
        .collect();

    assert_eq!(
        bedrock,
        [
            (
                "bedrock/claude-haiku-4-5",
                "us.anthropic.claude-haiku-4-5-20251001-v1:0",
                Some("classic_runtime")
            ),
            (
                "bedrock/claude-opus-4-5",
                "us.anthropic.claude-opus-4-5-20251101-v1:0",
                Some("classic_runtime")
            ),
            // The one undated profile id, and deliberately so — see the note
            // in `tiers.toml`. AWS publishes no dated `us.` profile for opus
            // 4.6, so the shape genuinely differs from its siblings and must
            // not be "regularised" into a dated one.
            (
                "bedrock/claude-opus-4-6",
                "us.anthropic.claude-opus-4-6-v1",
                Some("classic_runtime")
            ),
            (
                "bedrock/claude-sonnet-4-5",
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                Some("classic_runtime")
            ),
        ],
        "every shipped Bedrock candidate must ride the classic runtime plane with a \
         `us.`-prefixed geographic inference profile id"
    );
}

/// The Bedrock lanes' prices are exactly 1.10x their first-party twins.
///
/// The cheapest available check on the one number in this catalog with no
/// automated source. AWS's cross-region-inference doc prices geographic profiles
/// at the standard class and global at "approximately 10% savings"; Anthropic's
/// Bedrock page states the same premium from the other side. So every rate on
/// these lanes should be exactly 1.10x Anthropic's first-party rate for the same
/// weights — and it is, on all six dimensions of the two models this catalog
/// also pins first-party.
///
/// This is a consistency check, not a source: it would not catch AWS repricing
/// both classes together. What it does catch is the likely edit — someone
/// "correcting" a Bedrock rate down to its first-party figure, which would sell
/// the lane below cost on every token, and which `catalog-drift` cannot see
/// because Bedrock is exempted from reconciliation.
#[tokio::test]
async fn every_bedrock_rate_is_exactly_the_documented_premium_over_first_party() {
    let catalog = load_tier_catalog(&tier_config_path())
        .await
        .expect("bundled tier catalog should load");

    // Only haiku 4.5 has a first-party twin in this catalog today; opus 4.5 and
    // sonnet 4.5 are Bedrock-only here, so their premium is checked against the
    // published first-party list rates instead, transcribed from models.dev on
    // 2026-08-20 alongside the Price List read.
    let first_party: [(&str, f64, f64, f64); 4] = [
        ("bedrock/claude-opus-4-6", 5.00, 0.50, 25.00),
        ("bedrock/claude-opus-4-5", 5.00, 0.50, 25.00),
        ("bedrock/claude-sonnet-4-5", 3.00, 0.30, 15.00),
        ("bedrock/claude-haiku-4-5", 1.00, 0.10, 5.00),
    ];

    for (tier_id, input, cached, output) in first_party {
        let rates = catalog.tiers[tier_id].rates.base();
        for (dimension, published, actual) in [
            ("input", input, rates.input_per_mtok),
            ("cached input", cached, rates.cached_input_per_mtok),
            ("output", output, rates.output_per_mtok),
        ] {
            let actual = actual.unwrap_or_else(|| panic!("{tier_id} prices {dimension}"));
            let expected = published * 1.10;
            assert!(
                (actual - expected).abs() < 1e-9,
                "{tier_id} {dimension} is {actual}, but a geographic inference profile bills the \
                 standard class — 1.10x the first-party {published}, i.e. {expected}. If AWS \
                 genuinely moved this rate, re-read the Price List SKU table before changing it; \
                 a figure that is no longer 1.10x its twin usually means the GLOBAL SKU was read \
                 by mistake, which sells this lane below cost."
            );
        }
        // Basis == sell on every dimension, which is what makes it pass-through.
        assert_eq!(
            catalog.tiers[tier_id].candidates[0].rates, catalog.tiers[tier_id].rates,
            "{tier_id} must sell at cost"
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
            "bedrock/claude-haiku-4-5",
            "bedrock/claude-opus-4-5",
            "bedrock/claude-opus-4-6",
            "bedrock/claude-sonnet-4-5",
            "fireworks/deepseek-v4-flash",
            "fireworks/deepseek-v4-pro",
            "fireworks/glm-5.2",
            "fireworks/kimi-k3",
            "fireworks/minimax-m3",
            "fireworks/qwen3.8-max",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.5-flash-lite",
            "google/gemini-3.7-flash",
            "openai/gpt-5.6-luna",
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-terra",
            "vertex/gemini-3.1-pro-preview",
            "vertex/gemini-3.5-flash-lite",
            "vertex/gemini-3.7-flash",
            "xai/grok-4.3",
            "xai/grok-4.6",
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

    // And the public catalog stops advertising what it cannot serve: the
    // twenty-four surviving pins, one row each. A Bedrock lane is among the survivors and
    // that is the point of naming one — Bedrock lanes are a different account
    // with their own rate card, so a repricing of a first-party Anthropic lane
    // withholds only that first-party lane.
    let listed = listed_model_ids(RouterState::fully_credentialed(path)).await;
    assert_eq!(listed.len(), 24);
    assert!(listed.iter().any(|id| id == "bedrock/claude-sonnet-4-5"));
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
    assert_eq!(catalog.tiers.len(), 25);
}

/// Every conditional rate the shipped catalog declares, transcribed from
/// models.dev on 2026-08-18 and asserted here so an edit to `tiers.toml` is
/// caught by `cargo test` rather than only by the networked drift check.
///
/// `(tier, threshold, input, cached, output)`. The thresholds are NOT all the
/// same on purpose — OpenAI reprices the 5.6 family at 272,000 while Google and
/// xAI both reprice at 200,000 — and a future edit that "tidies" them into one
/// number is exactly what this table is here to stop.
///
/// The two xAI rows were added on 2026-08-20 and are the only ones whose
/// boundary the vendor states UNAMBIGUOUSLY: docs.x.ai labels its price rows
/// `< 200k prompt tokens` and `≥ 200k prompt tokens` and says in prose that
/// "requests whose prompt reaches 200k tokens are billed at the higher rate for
/// all tokens in the request". Inclusive, in the vendor's own words, which is
/// the `>=` this catalog implements — unlike the OpenAI and Google rows, where
/// `tiers.toml` records that the vendors disagree in writing about that one
/// token and `>=` was chosen as the markup-not-loss side.
const SHIPPED_CONDITIONAL_RATES: [(&str, u64, f64, f64, f64); 7] = [
    ("openai/gpt-5.6-luna", 272_000, 0.40, 0.04, 1.80),
    ("openai/gpt-5.6-terra", 272_000, 4.00, 0.40, 18.00),
    ("openai/gpt-5.6-sol", 272_000, 10.00, 1.00, 45.00),
    ("google/gemini-3.1-pro-preview", 200_000, 4.00, 0.40, 18.00),
    ("xai/grok-4.6", 200_000, 4.00, 1.00, 12.00),
    ("xai/grok-4.3", 200_000, 2.50, 0.40, 5.00),
    // The same model and the same boundary as the `google/*` row above, on the
    // zero-retention Vertex lane. Google prices the two identically on the
    // global endpoint, so a divergence between these two rows is a mistake in
    // one of them rather than a vendor decision.
    ("vertex/gemini-3.1-pro-preview", 200_000, 4.00, 0.40, 18.00),
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
const PASS_THROUGH_TIERS: [&str; 25] = [
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
    // The Bedrock classic-runtime zero-retention lanes (2026-08-20).
    // Pass-through like every other pin, and note what that means here: each
    // costs exactly 10% MORE than the first-party Anthropic rate for the same
    // weights, because these dispatch `us.`-prefixed GEOGRAPHIC inference
    // profiles and AWS prices geographic cross-region inference at the standard
    // class while the global class takes ~10% off. Selling that through at cost
    // is the promise; the assertion below (candidate rates == tier rates) is
    // what holds it, and it would equally catch someone "fixing" these down to
    // the first-party figures on the sell side only.
    "bedrock/claude-opus-4-6",
    "bedrock/claude-opus-4-5",
    "bedrock/claude-sonnet-4-5",
    "bedrock/claude-haiku-4-5",
    // The Fireworks open-weight zero-retention lanes (2026-08-20). Pass-through
    // like every other pin, and here that is the plain reading for once: these
    // sell at Fireworks' published standard-path rate with nothing added. The
    // assertion below (candidate rates == tier rates) is what holds it, and it
    // is the guard that would catch someone repointing a candidate at a
    // `routers/`-prefixed Fast or US id — which costs 10-50% more — without
    // moving the rates to match.
    "fireworks/kimi-k3",
    "fireworks/glm-5.2",
    "fireworks/deepseek-v4-pro",
    "fireworks/deepseek-v4-flash",
    "fireworks/minimax-m3",
    // The closed-weight Fireworks lane (2026-08-20). Pass-through on exactly the
    // same terms as the five above — its per-tier retention override changes
    // what the row CLAIMS about data handling and changes nothing about what it
    // costs. Worth stating because the two are easy to conflate: a `standard`
    // lane is not a lane ZeroRouter marks up, and the candidate-rates ==
    // tier-rates assertion below holds it to Fireworks' published $2.00/$0.25/
    // $6.00 standard-path rate like every other pin.
    "fireworks/qwen3.8-max",
    // The xAI runtime-attested zero-retention lanes (2026-08-20). Pass-through
    // at xAI's published rates on BOTH bands — and the equality the assertion
    // below checks holds per band, which is what makes it a real guard here:
    // these are the first lanes in the catalog that are simultaneously
    // zero-retention AND conditionally repriced, so a candidate that kept the
    // base rate while the tier gained a band (or the reverse) would be selling
    // long-context Grok at half its cost. `tiers.toml` refuses that shape at
    // load as well — thresholds must match exactly — so this is the second of
    // two independent checks on the same mistake.
    "xai/grok-4.6",
    "xai/grok-4.3",
    // The Vertex zero-retention twins of the three `google/*` Gemini pins
    // (2026-08-21). Pass-through, and the assertion below is load-bearing in a
    // way it is not for the Bedrock twins: those sell 10% ABOVE their
    // first-party counterparts and the gap is visible on inspection, while
    // these sell at EXACTLY the `google/*` rate because Google's global-endpoint
    // Vertex price is the same number. That makes a wrong basis here invisible
    // to the eye — the failure mode is someone pointing a candidate at a
    // regional endpoint, which costs 10% more, while these tables still read
    // 0.75/3.75. See the section header in `tiers.toml`.
    "vertex/gemini-3.7-flash",
    "vertex/gemini-3.5-flash-lite",
    "vertex/gemini-3.1-pro-preview",
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

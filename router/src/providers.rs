use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use crate::provider::{
    ChatRequest, ChatResponse, ModelProvider, StreamEvent, StreamOptions, StreamResult,
};
use futures_util::stream::BoxStream;
use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::TierCandidate,
    wire::{AnthropicWire, BedrockRuntimeWire, ChatCompletionsWire, OpenAiResponsesWire},
};

const PROVIDER_INVENTORY_JSON: &str = include_str!("../config/providers.json");

/// Env var naming an operator-supplied provider inventory, layered over the
/// shipped one at startup (edge mode, stage 2:
/// `docs/design/edge-mode-local-rung.md`).
///
/// Shaped exactly like [`crate::config::TIER_CONFIG_PATH_ENV`] — an env var
/// naming a file — because that is this repo's existing answer to "the
/// operator needs to say something the shipped file cannot". The two differ in
/// one deliberate way: the tier path *replaces* the shipped catalog, while
/// this one *adds to* the shipped inventory. Replacement would let a
/// deployment redefine `openai` to point somewhere else, which is the shape of
/// a credential-exfiltration bug rather than a configuration feature; an entry
/// whose key collides with a shipped one is refused (see
/// [`ProviderInventory::assembled`]).
pub const PROVIDER_INVENTORY_PATH_ENV: &str = "ZEROROUTER_PROVIDERS_PATH";

#[derive(Debug, Deserialize)]
struct ProviderInventory {
    providers: Vec<ProviderMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
// A misspelled field must not silently default. `"credentail": "none"` would
// otherwise leave `credential` at `required` and the typo invisible — and the
// two fields this struct gained in stage 2 are both safety declarations whose
// absence is meaningful, which is exactly the shape where a silent default is
// worst.
#[serde(deny_unknown_fields)]
struct ProviderMetadata {
    key: String,
    adapter: ProviderAdapter,
    /// Whether requests to this upstream cost anyone money.
    ///
    /// Defaults to [`SettlementDeclaration::Metered`], so the shipped file and
    /// every entry written before this field existed keep meaning what they
    /// meant. Declaring `free` is the ONLY way a candidate on this provider may
    /// be priced at $0 — see [`provider_settles_free`] for why the adapter
    /// cannot answer that question.
    #[serde(default)]
    settlement: SettlementDeclaration,
    /// Whether this upstream takes a credential at all. Defaults to
    /// [`CredentialRequirement::Required`], so the shipped file — and every
    /// entry written before this field existed — keeps meaning exactly what it
    /// meant.
    #[serde(default)]
    credential: CredentialRequirement,
    /// The environment variable holding the credential. Required when
    /// `credential` is `required`, and refused when it is `none`: an entry
    /// that claims to need no key while naming the variable holding one is
    /// contradicting itself, and guessing which half is the truth is how a
    /// credential ends up unused or a keyless upstream ends up authenticated.
    #[serde(default)]
    credential_env: Option<String>,
    /// The deployment's name for the secret behind `credential_env`. Same
    /// present-iff-required rule, for the same reason: there is no secret to
    /// name when there is no credential.
    #[serde(default)]
    secret_name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// The environment variable holding the AWS region this upstream's
    /// endpoint is addressed in, for an entry whose `base_url` carries the
    /// [`REGION_PLACEHOLDER`].
    ///
    /// Exists for exactly one shape of upstream: a regional endpoint whose
    /// host contains the region, where hardcoding one region into the shipped
    /// inventory would silently send a deployment in another region's traffic —
    /// and its prompts — across a boundary the operator did not choose. Bedrock
    /// is that shape (`bedrock-mantle.{region}.api.aws`), and it is in-region
    /// only, so the region is not a detail the endpoint can shrug off.
    ///
    /// Present iff the `base_url` interpolates, enforced both ways by
    /// [`ProviderMetadata::validate_region`]. A placeholder with nothing to fill
    /// it would dial a host named `{region}`; a `region_env` with no placeholder
    /// names a variable that could never be read, which is a claim about
    /// configuration that is not true.
    #[serde(default)]
    region_env: Option<String>,
    /// The environment variable holding the cloud PROJECT this upstream's
    /// endpoint is addressed in, for an entry whose `base_url` carries the
    /// [`PROJECT_PLACEHOLDER`].
    ///
    /// The sibling of [`Self::region_env`] and validated by the identical
    /// present-iff-interpolated rule, but it names a different kind of fact and
    /// the difference matters. A region is a routing choice; a project is an
    /// ACCOUNT BOUNDARY. Google addresses Vertex as
    /// `projects/{project}/locations/global/endpoints/openapi`, and the project
    /// in that path is what the zero-retention configuration is applied to —
    /// `projects/{id}/cacheConfig` is the switch that disables the 24-hour
    /// cache, and the abuse-monitoring exemption is granted per Google Cloud
    /// account. So a wrong project here is not a slower route, it is a request
    /// served under a retention posture nobody verified.
    ///
    /// That is also why it is not defaulted and never will be. There is no
    /// sensible fallback project: the shipped inventory cannot know the
    /// operator's, and inventing one would dial a stranger's account.
    #[serde(default)]
    project_env: Option<String>,
    /// What KIND of thing `credential_env` holds, when it is not simply the
    /// string to send.
    ///
    /// Defaults to [`CredentialKind::ApiKey`], so every entry written before
    /// this field existed keeps meaning exactly what it meant: read the
    /// variable, send its contents. See [`CredentialKind`] for why the second
    /// variant had to exist.
    #[serde(default)]
    credential_kind: CredentialKind,
    /// Why the public catalog `admin catalog-drift` reconciles against cannot
    /// price this upstream, and where its rates really come from.
    ///
    /// **Presence is the exemption; the text is the justification, and it is
    /// required rather than decorative** — the same rule a retention pin's
    /// evidence fields follow. A declaration with nothing behind it would let
    /// "we could not be bothered to check" wear the same shape as a real,
    /// argued gap, and this field disables the one alarm that guards margin.
    /// Blank is refused at load ([`ProviderMetadata::validate_reconciliation`]).
    ///
    /// It exists for a fault that has no honest fix inside the reconciliation:
    /// models.dev DOES carry Bedrock, under `amazon-bedrock`, and prices the
    /// bare id `anthropic.claude-sonnet-5` at the GLOBAL cross-region rate.
    /// ZeroRouter dials the mantle endpoint, which is in-region only and cannot
    /// use a global inference profile, so AWS meters the unqualified `_standard`
    /// SKU — 10% dearer. Joining the two would have reported `ok` over a basis
    /// 10% under the invoice, which is precisely the silent-margin failure
    /// [`crate::drift`] was written to break. Mapping the provider key to
    /// `amazon-bedrock` was tried and rejected for that reason: a green row
    /// against the wrong SKU class is worse than an admitted gap.
    ///
    /// It is deliberately NOT a way to skip a lane quietly. A declaring
    /// provider's candidates still appear in every report, under their own
    /// verdict, with this text printed beside them on every run.
    #[serde(default)]
    unreconcilable_reason: Option<String>,
    /// The key this upstream is filed under in the reconciliation source, when
    /// that differs from ZeroRouter's own provider key.
    ///
    /// **Declared, never inferred**, and the argument is the one
    /// [`crate::config::RetentionPin::openrouter_slug`] makes for the other
    /// third-party join in this repo: the keys look mechanical and are not.
    /// models.dev files Fireworks under `fireworks-ai` and Together under
    /// `togetherai`, while its `google` is ZeroRouter's `google` and its
    /// `amazon-bedrock` is a lane ZeroRouter deliberately does NOT join. There
    /// is no rule that turns one into the other, and a guess that lands on a
    /// real-but-wrong key is the failure mode that matters: a row that answers
    /// with somebody else's prices reconciles green over a basis nobody checked.
    ///
    /// `None` — every entry that had this field's absence baked in before it
    /// existed — means the join is on the provider key itself, which is what
    /// [`crate::drift::reconcile_with`] did unconditionally until Fireworks
    /// arrived. So this is additive: no existing entry changes meaning.
    ///
    /// It is NOT the sibling of [`Self::unreconcilable_reason`] but its
    /// opposite. That field says "the source has a row and it is the wrong
    /// one, so trust nothing"; this one says "the source has the right row,
    /// under a different name". Declaring both on one entry would be
    /// contradictory and is refused at load
    /// ([`ProviderMetadata::validate_reconciliation`]) — an upstream cannot
    /// simultaneously be reconcilable at a named key and unreconcilable
    /// everywhere.
    #[serde(default)]
    source_provider_key: Option<String>,
    /// The response header this upstream restates a data guarantee in, on every
    /// answer, and the value that satisfies it. Present together or not at all.
    ///
    /// **This is the only runtime-enforced retention check in the repo**, and
    /// it is declared here rather than hardcoded in the wire for the reason
    /// everything else about an upstream is declared here: a fact about one
    /// account belongs in that account's entry, where a reviewer reading the
    /// inventory can see the whole of what the lane promises. A constant buried
    /// in `wire/chat_completions.rs` reading `if alias == "xai"` would put a
    /// customer-facing data claim somewhere nobody looks when they change what
    /// ZeroRouter sells.
    ///
    /// It is deliberately NOT generalized past what one lane needs. One header
    /// name, one expected value, no negation, no list, no per-model override —
    /// because the second declaration is where a mechanism like this starts
    /// growing an escape hatch, and an escape hatch on a fail-closed retention
    /// check is a way to serve a retaining upstream under a `zero` label. When
    /// a second vendor publishes a different shape, widen it then, against a
    /// real example.
    ///
    /// Only the `chat_completions` adapter enforces it, and declaring it on any
    /// other adapter is REFUSED at load rather than ignored
    /// ([`ProviderMetadata::validate_attestation`]). A silently-ignored
    /// attestation is the worst available outcome: the inventory would read as
    /// though the guarantee were being checked on every request while nothing
    /// checked it at all, which is exactly the false confidence the mechanism
    /// exists to remove.
    #[serde(default)]
    attestation_header: Option<String>,
    /// The value [`Self::attestation_header`] must carry. See that field.
    #[serde(default)]
    attestation_expect: Option<String>,
    /// Additional API planes this same upstream exposes, keyed by the name a
    /// candidate selects with `surface = "..."` in `tiers.toml`.
    ///
    /// **A provider entry is an ACCOUNT; a surface is an API PLANE.** That split
    /// is the whole design, and it is what this field is for. Everything else
    /// this struct declares — the credential, the region, and (through the
    /// provider key) the retention pin and the reconciliation exemption — is a
    /// fact about the account, true of every plane it exposes. Only the endpoint
    /// and the wire differ per plane.
    ///
    /// Bedrock is that shape and is why this exists. One AWS account, one
    /// `BEDROCK_API_KEY`, one `data_retention_mode: none`, one models.dev
    /// mismatch — reached over two entirely different APIs: the mantle plane
    /// (`bedrock-mantle.{region}.api.aws`, Messages verbatim) and the classic
    /// runtime plane (`bedrock-runtime.{region}.amazonaws.com`, InvokeModel).
    ///
    /// Two second providers were considered and both are worse:
    ///
    /// - **A second entry, `bedrock-runtime`.** `/v1/models` publishes
    ///   `owned_by` as the provider key and `router/tests/http.rs` pins that it
    ///   equals the vendor half of the lane's id — so a second key forces
    ///   customer-facing ids like `bedrock-runtime/claude-opus-4-5`, splitting
    ///   one vendor across two names for an internal transport detail. It would
    ///   also duplicate the `[retention.*]` pin — the same AWS page, hashed
    ///   twice, re-fetched twice by `retention-drift`, and kept in sync by
    ///   hand — for one account. The retention pin's own doc says the posture is
    ///   "a property of the operator's account"; two keys would make that false.
    /// - **An `adapter` override on the candidate.** Half a surface: a candidate
    ///   could then name a wire while inheriting the other plane's endpoint,
    ///   which is a live foot-gun (InvokeModel bodies POSTed at the mantle
    ///   Messages path). Binding the wire and the endpoint together into one
    ///   named thing makes that unrepresentable.
    ///
    /// A surface may NOT introduce configuration of its own — no credential, no
    /// region variable. That restraint is what keeps #89's anti-drift property
    /// intact: dispatchability stays a question about the PROVIDER, so
    /// `/v1/models` and route construction still read one answer from
    /// [`ProviderMetadata::dispatchable`] rather than growing a per-surface
    /// second one. A surface's `base_url` may interpolate `{region}`, and it
    /// resolves from the entry's own `region_env` or not at all.
    #[serde(default)]
    surfaces: BTreeMap<String, ProviderSurface>,
}

/// One API plane of an upstream: where to dial it, and which wire speaks to it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSurface {
    adapter: ProviderAdapter,
    /// Required, and with no default anywhere. A surface exists precisely
    /// because its endpoint differs from the entry's, so an unstated one has
    /// nothing to mean.
    base_url: String,
}

/// The token a regional `base_url` writes where its region belongs.
const REGION_PLACEHOLDER: &str = "{region}";

/// The token a project-scoped `base_url` writes where its project belongs.
const PROJECT_PLACEHOLDER: &str = "{project}";

/// What a provider's `credential_env` actually holds.
///
/// **Why this enum exists at all**, because for five providers it did not need
/// to. Every upstream in this repo until Vertex took a credential that was
/// literally the string to send: an API key, read from the environment, handed
/// to the wire, put in a header. The variable held the secret, and there was
/// nothing to decide.
///
/// Google does not issue such a key for the surface the `vertex` lane
/// dispatches on — "Only Google Cloud Auth is supported using the OpenAI
/// library" (cloud.google.com/vertex-ai/generative-ai/docs/start/openai). What
/// the operator can store is a service-account key, and what the wire must send
/// is an OAuth2 access token minted from it that expires in an hour. Those are
/// two different strings, and the second one cannot be put in a secret because
/// it does not exist until something signs a JWT for it.
///
/// So the inventory has to say which it is holding. It is an enum rather than a
/// bool because the interesting question is not "does this need minting" but
/// "what protocol mints it", and a second cloud (Azure AD, AWS STS) would be a
/// third variant rather than a reinterpretation of a flag.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// The variable holds the credential verbatim. Send it as-is.
    #[default]
    ApiKey,
    /// The variable holds a Google service-account key in JSON — either the
    /// JSON itself or a path to a file containing it — which is exchanged for a
    /// short-lived OAuth2 access token before dispatch
    /// ([`crate::gcp_auth`]).
    GoogleServiceAccount,
}

impl CredentialKind {
    /// Whether reaching this upstream needs a token minted before dispatch.
    ///
    /// Written as a match rather than `!= ApiKey` for the reason
    /// [`ProviderAdapter::dials_a_billed_endpoint`] gives: a third variant must
    /// force a decision here instead of silently inheriting whichever answer
    /// the inequality happened to give.
    #[must_use]
    pub const fn needs_minting(self) -> bool {
        match self {
            Self::ApiKey => false,
            Self::GoogleServiceAccount => true,
        }
    }
}

/// Whether a deployment holds everything one upstream needs.
enum Dispatchable {
    Ready {
        credential: String,
        /// The endpoint to dial, region already substituted. `None` means the
        /// wire uses its own default.
        endpoint: Option<String>,
        /// Resolved endpoints for each NAMED surface, same substitution applied.
        ///
        /// Carried here rather than resolved by the caller so that "this
        /// provider is dispatchable" and "every plane of it has an address" are
        /// decided in one place, at one time. A surface shares the entry's
        /// credential and region variable, so the two can never disagree — and
        /// `surfaces_never_change_a_providers_dispatchability` is the test that
        /// keeps that true if a surface ever gains a knob of its own.
        surfaces: BTreeMap<String, String>,
    },
    /// The environment variable whose absence disqualifies this provider.
    /// Empty only for a malformed entry that validation already refuses.
    Missing { env: String },
}

/// Whether this deployment can dispatch to `provider` at all.
///
/// **The catalog's gate, and it must answer exactly as route construction
/// does** — both read [`ProviderMetadata::dispatchable`], which is why that
/// function exists rather than the two growing their own conditions.
///
/// An unknown provider is not dispatchable. That is the conservative answer for
/// the caller that matters: `/v1/models` would otherwise advertise a lane whose
/// upstream this build has no entry for, and the catalog validator already
/// refuses such a file at load, so this branch is reachable only if the two ever
/// disagree.
#[must_use]
pub fn provider_is_dispatchable(provider: &str) -> bool {
    provider_is_dispatchable_with(provider, read_credential)
}

/// [`provider_is_dispatchable`], with the environment supplied.
///
/// The seam exists so the property that matters can be tested for what it is:
/// this function and [`build_with_credentials`] must reach the SAME verdict
/// from the same environment. Reading the process environment in both would
/// make that untestable, and untestable is how the two drift.
fn provider_is_dispatchable_with<F>(provider: &str, read_env: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory.provider(provider).is_some_and(|metadata| {
            matches!(metadata.dispatchable(read_env), Dispatchable::Ready { .. })
        })
    })
}

/// Whether an upstream's traffic costs anyone money.
///
/// The free lane is entered by this declaration and by nothing else — not by
/// the adapter, and not by the absence of a credential. Both of those were
/// considered and both are wrong:
///
/// - **The adapter cannot answer it.** `chat_completions` is dual-use by the
///   design's own scope (`docs/design/edge-mode-local-rung.md`): it serves the
///   operator's local models AND a hosted ZeroRouter `/v1` taking metered burst
///   traffic. "On the local wire" and "bills nobody" are different claims, and
///   only one of them is about money.
/// - **Credential presence cannot answer it either.** A local vLLM behind a
///   bearer token is an ordinary deployment; it is free because its operator
///   runs it, not because it happens to be unauthenticated. A free provider may
///   be keyless or credentialed.
///
/// So it is stated twice, deliberately: once here, by the provider, and once in
/// `tiers.toml`, by the candidate's $0 price. [`crate::config::TierCandidate::is_free`]
/// — the key stage 3's metering skip is specified to read — requires both.
///
/// What this does NOT do, and must not be described as doing: make it
/// impossible to run a paid model through the free lane. An operator who
/// declares `settlement: free` on an upstream that bills them has lied to their
/// own configuration, and no validation here can detect that — the router has
/// no way to know what an arbitrary endpoint charges. What the double
/// declaration buys is that it cannot happen by ACCIDENT: not by a typo in a
/// rate, not by picking the wrong adapter, not by forgetting a credential. The
/// free lane is always something someone wrote down on purpose.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SettlementDeclaration {
    #[default]
    Metered,
    Free,
}

/// Whether an upstream authenticates.
///
/// The keyless case exists because most local inference servers — llama.cpp,
/// Ollama, LM Studio, vLLM started without `--api-key` — take no credential at
/// all, while [`build_with_credentials`] drops any candidate whose credential
/// is missing from the environment. Without a way to *say* "there is no
/// credential", an operator's only options would be to invent a fake key (so a
/// genuinely missing paid key becomes indistinguishable from a deliberately
/// absent local one) or to weaken the missing-credential skip for everybody.
///
/// It is declared, never inferred. A keyless entry is one that says
/// `"credential": "none"`, not one that forgot to say anything — the same
/// reasoning that makes `base_url` mandatory on this adapter rather than
/// defaulted (stage 1). Forgetting a field must never be the way a safety
/// property gets turned off.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CredentialRequirement {
    #[default]
    Required,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum ProviderAdapter {
    #[serde(rename = "anthropic")]
    Anthropic,
    /// First-party OpenAI on ZeroRouter's own Responses wire client
    /// (`crate::wire`) — billing-grade usage extraction on the wire where
    /// gpt-5.x and codex actually live. The pinned adapters cannot serve
    /// this family: the chat wire rejects it and the pinned Responses
    /// provider discards usage.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Any upstream speaking OpenAI **chat completions** — the dialect
    /// llama.cpp, vLLM, Ollama, and LM Studio all implement, and the one a
    /// hosted ZeroRouter `/v1` serves. Unlike the other two adapters this one
    /// has no implied endpoint, so a provider entry selecting it is expected
    /// to carry a `base_url` (edge mode, stage 1:
    /// `docs/design/edge-mode-local-rung.md`).
    #[serde(rename = "chat_completions")]
    ChatCompletions,
    /// Bedrock's CLASSIC runtime plane — `InvokeModel` and
    /// `InvokeModelWithResponseStream` on
    /// `bedrock-runtime.{region}.amazonaws.com` (`crate::wire::bedrock_runtime`).
    ///
    /// A separate adapter from [`Self::Anthropic`] even though the request BODY
    /// is the Messages body, because four things around that body differ and
    /// each is a 400 from AWS if got wrong: the model id rides in the URL path
    /// rather than the body, `anthropic_version` is a required body field, the
    /// `anthropic-version` header is not part of this operation, and auth is
    /// `Authorization: Bearer` rather than `x-api-key`. Streaming differs
    /// entirely — AWS event stream binary framing, not SSE.
    ///
    /// Its `base_url` is a HOST ROOT, not a full endpoint: this is the one
    /// adapter that builds its own path per request, because the path contains
    /// the model id. [`ProviderMetadata::validate_runtime_roots`] enforces that
    /// distinction, which is otherwise a silent misconfiguration.
    #[serde(rename = "anthropic_bedrock_runtime")]
    AnthropicBedrockRuntime,
}

impl ProviderAdapter {
    /// Whether this adapter dials an endpoint someone invoices ZeroRouter for.
    ///
    /// The keyless and free-settlement rules both ask this question, and both
    /// asked it as `!= ChatCompletions` before a third billed adapter existed.
    /// Naming it means adding an adapter forces an answer here rather than
    /// silently inheriting "billed" from an inequality — and getting it wrong in
    /// the other direction (a new adapter accidentally admitted to the free
    /// lane) is the failure `SettlementDeclaration` exists to prevent.
    fn dials_a_billed_endpoint(self) -> bool {
        match self {
            Self::Anthropic | Self::OpenAiResponses | Self::AnthropicBedrockRuntime => true,
            // The only adapter with no implied endpoint: an entry using it must
            // name its own host, so it is the operator's own upstream by
            // construction.
            Self::ChatCompletions => false,
        }
    }
}

/// The operator's inventory, parsed and validated once at startup by
/// [`load_operator_inventory`].
///
/// Process-global and read-only after installation, which is the honest shape
/// for this data: half the inventory is `include_str!`-embedded, so the
/// provider list is already a restart-scoped artifact — unlike
/// `config/tiers.toml`, which is re-read per request because prices change
/// under a running service. It also keeps [`ProviderInventory::load`] free of
/// file I/O, and that function runs once per candidate per request.
static OPERATOR_INVENTORY: OnceLock<Vec<ProviderMetadata>> = OnceLock::new();

fn operator_inventory() -> &'static [ProviderMetadata] {
    OPERATOR_INVENTORY.get().map_or(&[], Vec::as_slice)
}

/// Read, validate, and install the operator's provider inventory.
///
/// Called once, from `serve`, before the router accepts anything. Every
/// failure mode is a startup failure rather than a degraded run: a deployment
/// whose local upstream is misconfigured must not come up serving cloud
/// traffic as though the operator had not asked for a local rung at all, and
/// must certainly not come up with a half-applied inventory.
///
/// The merged inventory is validated *before* the overlay is installed, so a
/// bad file cannot poison the loads that follow.
pub fn load_operator_inventory(path: &Path) -> Result<usize, ProviderBuildError> {
    let source = std::fs::read_to_string(path).map_err(|source| {
        ProviderBuildError::OperatorInventoryUnreadable {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let entries = ProviderInventory::parse_operator(&source)?;
    ProviderInventory::assembled(&entries)?;
    let count = entries.len();
    let keys: Vec<String> = entries.iter().map(|entry| entry.key.clone()).collect();
    OPERATOR_INVENTORY
        .set(entries)
        .map_err(|_| ProviderBuildError::OperatorInventoryAlreadyLoaded)?;
    tracing::info!(
        path = %path.display(),
        providers = ?keys,
        "operator provider inventory loaded"
    );
    Ok(count)
}

impl ProviderInventory {
    fn load() -> Result<Self, ProviderBuildError> {
        Self::assembled(operator_inventory())
    }

    fn shipped() -> Result<Self, ProviderBuildError> {
        serde_json::from_str::<Self>(PROVIDER_INVENTORY_JSON).map_err(|source| {
            ProviderBuildError::InvalidInventory {
                detail: source.to_string(),
            }
        })
    }

    /// Parse an operator-supplied inventory document.
    fn parse_operator(source: &str) -> Result<Vec<ProviderMetadata>, ProviderBuildError> {
        let inventory = serde_json::from_str::<Self>(source).map_err(|source| {
            ProviderBuildError::InvalidInventory {
                detail: format!("operator provider inventory is not valid: {source}"),
            }
        })?;
        Ok(inventory.providers)
    }

    /// The shipped inventory with the operator's entries layered on top.
    ///
    /// Additive, and only additive. An operator entry whose key matches a
    /// shipped provider is refused rather than allowed to win: the shipped
    /// entries name the endpoints ZeroRouter's own credentials are sent to, and
    /// a config file that could silently repoint `openai` at another host is a
    /// credential-exfiltration primitive, not an extension point. Renaming the
    /// operator's entry costs one line and leaves both providers addressable.
    ///
    /// The same rule covers the credential the entry reads. Blocking the key
    /// while leaving `credential_env` free would stop an operator entry from
    /// *being* `anthropic` but not from *reading* `ANTHROPIC_API_KEY` and
    /// posting it to an address of their choosing — the identical outcome by a
    /// different door. Both halves of "where ZeroRouter's own credential goes"
    /// are therefore reserved.
    fn assembled(operator: &[ProviderMetadata]) -> Result<Self, ProviderBuildError> {
        let mut inventory = Self::shipped()?;
        if !operator.is_empty() {
            let shipped_keys: BTreeSet<String> = inventory
                .providers
                .iter()
                .map(|provider| provider.key.clone())
                .collect();
            let shipped_credentials: BTreeSet<String> = inventory
                .providers
                .iter()
                .filter_map(|provider| provider.credential_env.clone())
                .collect();
            for entry in operator {
                if shipped_keys.contains(&entry.key) {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "operator provider {} shadows a shipped provider of the same key; \
                             the shipped entry names where ZeroRouter's own credential is sent, \
                             so it may be added to but never redefined — rename the operator entry",
                            entry.key
                        ),
                    });
                }
                if let Some(credential_env) = entry
                    .credential_env
                    .as_deref()
                    .filter(|name| shipped_credentials.contains(*name))
                {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "operator provider {} reads {credential_env}, which is a shipped \
                             provider's credential; an operator entry may not borrow a credential \
                             ZeroRouter holds for an upstream of its own — give this provider its \
                             own environment variable",
                            entry.key
                        ),
                    });
                }
                inventory.providers.push(entry.clone());
            }
        }
        inventory.validate()?;
        Ok(inventory)
    }

    fn validate(&self) -> Result<(), ProviderBuildError> {
        if self.providers.is_empty() {
            return Err(ProviderBuildError::InvalidInventory {
                detail: "provider list is empty".to_owned(),
            });
        }

        let mut keys = BTreeSet::new();
        for provider in &self.providers {
            if provider.key.trim().is_empty()
                || provider
                    .display_name
                    .as_deref()
                    .is_some_and(|name| name.trim().is_empty())
            {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: "provider metadata contains an empty required value".to_owned(),
                });
            }
            provider.validate_credential()?;
            provider.validate_settlement()?;
            provider.validate_region()?;
            provider.validate_reconciliation()?;
            provider.validate_attestation()?;
            provider.validate_surfaces()?;
            provider.validate_project()?;
            provider.validate_credential_kind()?;
            provider.validate_runtime_roots()?;
            if !keys.insert(provider.key.as_str()) {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!("duplicate provider key {}", provider.key),
                });
            }
            // Neither adapter in the SHIPPED inventory takes a base_url
            // override: both of those wires own their endpoints, and the only
            // override is the documented test seam. The per-adapter validation
            // this replaced covered aggregator and Bedrock shapes that no
            // longer exist.
            if provider
                .base_url
                .as_deref()
                .is_some_and(|url| url.trim().is_empty())
            {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!("provider {} has an empty base_url", provider.key),
                });
            }
            // The chat-completions adapter is the exception, and it is a
            // CREDENTIAL-SAFETY rule rather than a tidiness one. That wire
            // defaults to `https://api.openai.com/v1/chat/completions` when no
            // endpoint is given, because that is the right default for the
            // hosted dialect owner. An operator adding a local entry —
            // llama.cpp on a LAN address, say — who forgets `base_url` would
            // otherwise get a silently valid inventory that ships their
            // credential and their prompts to OpenAI, which is the exact
            // opposite of what running a model locally is for. There is no
            // sensible default for an adapter with no implied endpoint, so the
            // entry must state one.
            if provider.adapter == ProviderAdapter::ChatCompletions && provider.base_url.is_none() {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {} uses the chat_completions adapter and must declare a \
                         base_url; it has no implied endpoint, and defaulting one would send \
                         this provider's credential to a different host than configured",
                        provider.key
                    ),
                });
            }
        }
        Ok(())
    }

    fn provider(&self, key: &str) -> Option<&ProviderMetadata> {
        self.providers.iter().find(|provider| provider.key == key)
    }

    fn providers(&self) -> impl Iterator<Item = &ProviderMetadata> {
        self.providers.iter()
    }
}

impl ProviderMetadata {
    /// Enforce the credential declaration, which is where the keyless escape
    /// hatch is kept from becoming a way to run a PAID upstream unauthenticated.
    ///
    /// Two rules, and the first is the load-bearing one:
    ///
    /// - `"credential": "none"` is legal **only** on the chat-completions
    ///   adapter. That is the adapter with no implied endpoint — an entry using
    ///   it must state its own `base_url`, so it is the operator's own upstream
    ///   by construction. The other two adapters point at api.anthropic.com and
    ///   api.openai.com, and there is no configuration in which sending those
    ///   hosts no credential is what somebody meant; allowing it would turn a
    ///   typo into a provider that silently contributes 401s to every walk it
    ///   is on, and would make "this candidate has no key" stop being a
    ///   reliable reason to skip a paid rung.
    /// - A keyless entry must not also name a credential env var or a secret.
    ///   Contradiction is refused rather than resolved: an entry saying both
    ///   things has one of them wrong, and picking a winner would mean guessing
    ///   which.
    ///
    /// A `required` entry is checked exactly as it was before this field
    /// existed — both names present and non-blank — so the shipped inventory is
    /// unaffected.
    /// Enforce the settlement declaration.
    ///
    /// One rule: `free` is legal only on the chat-completions adapter. That
    /// adapter is not *sufficient* for free settlement — the whole point of
    /// [`SettlementDeclaration`] is that it cannot be — but it is necessary.
    /// The other two adapters exist to talk to api.anthropic.com and
    /// api.openai.com, whose traffic ZeroRouter is invoiced for, so an entry
    /// claiming that traffic settles free is stating something known to be
    /// untrue and is refused before it can reach a rate table.
    fn validate_settlement(&self) -> Result<(), ProviderBuildError> {
        if self.settlement == SettlementDeclaration::Free && self.adapter.dials_a_billed_endpoint()
        {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares \"settlement\": \"free\" on an adapter that dials a \
                     cloud endpoint ZeroRouter is invoiced for; only the chat_completions adapter \
                     — which must name its own base_url — may settle free",
                    self.key
                ),
            });
        }
        Ok(())
    }

    /// Everything this upstream needs before a request can be sent to it, or
    /// the name of the one environment variable that is missing.
    ///
    /// **This is the single definition of "can this deployment dispatch to this
    /// provider", and it exists because the answer is now read from two
    /// places.** [`build_with_credentials`] reads it to decide whether a rung
    /// joins a route; [`provider_is_dispatchable`] reads it to decide whether a
    /// lane appears in `/v1/models`. Those two answers must be the same answer.
    ///
    /// They were not, and that is the incident this function was extracted for.
    /// The catalog was credential-blind by design, so a deployment missing
    /// `BEDROCK_API_KEY` advertised both Bedrock lanes on the storefront —
    /// ZeroRouter's flagship zero-retention lanes — while every call to them
    /// was refused. Two separate implementations of "is this provider usable"
    /// would let that reopen the first time one grew a condition the other
    /// lacked, which is exactly how it happened: the region check was added to
    /// dispatch and the listing never had a check at all.
    ///
    /// A test-seam base-URL override is honored here rather than in the caller,
    /// so a harness that stands a fake upstream in front of a provider makes
    /// that provider dispatchable AND listable, in one place.
    fn dispatchable<F>(&self, mut read_env: F) -> Dispatchable
    where
        F: FnMut(&str) -> Option<String>,
    {
        // The endpoint before the credential, because an unresolvable region
        // disqualifies for the same reason and by the same mechanism a missing
        // key does. A test-seam override is a complete URL supplied by a
        // harness, so it wins and never interpolates.
        let override_url = base_url_override(&self.key);
        let endpoint = match override_url.clone() {
            Some(url) => Some(Some(url)),
            None => self.endpoint(&mut read_env),
        };
        let Some(endpoint) = endpoint else {
            // Named in the same list a missing credential reports. Both answer
            // the operator's one question — which environment variable is this
            // deployment missing — and an endpoint this provider cannot address
            // is as disqualifying as a key it cannot present.
            return Dispatchable::Missing {
                env: self.unresolvable_endpoint_env(),
            };
        };
        // Every named plane, resolved by the same rules and against the same
        // region variable. A test-seam override replaces all of them: a harness
        // that stands a fake in front of `bedrock` means the whole upstream, not
        // one of its planes, and leaving the real endpoints live for the others
        // would send a fault-injection run's traffic to AWS.
        let mut surfaces = BTreeMap::new();
        for (name, surface) in &self.surfaces {
            let resolved = match override_url.clone() {
                Some(url) => Some(url),
                None => resolve_region(
                    &surface.base_url,
                    self.region_env.as_deref(),
                    self.project_env.as_deref(),
                    &mut read_env,
                ),
            };
            let Some(resolved) = resolved else {
                return Dispatchable::Missing {
                    env: self.unresolvable_endpoint_env(),
                };
            };
            surfaces.insert(name.clone(), resolved);
        }
        let credential = match self.credential {
            // A keyless upstream has nothing to look up, so it can never be the
            // rung a route loses to a missing key — which is the whole point: a
            // local server that takes no credential must not need a fake one to
            // stay in the walk. Inventory validation has already refused this
            // declaration on every adapter that owns a cloud endpoint, so
            // nothing reachable from here can be a paid upstream dispatched
            // without authentication.
            CredentialRequirement::None => String::new(),
            CredentialRequirement::Required => {
                // Validated present; `unwrap_or_default` reads an empty name
                // rather than panicking, and an empty name resolves to no
                // credential, so the rung drops out exactly as it would for a
                // genuinely absent key.
                let credential_env = self.credential_env.as_deref().unwrap_or_default();
                let Some(credential) = read_env(credential_env) else {
                    return Dispatchable::Missing {
                        env: credential_env.to_owned(),
                    };
                };
                credential
            }
        };
        Dispatchable::Ready {
            credential,
            endpoint,
            surfaces,
        }
    }

    /// The wire and endpoint one candidate dispatches on.
    ///
    /// `None` surface means the entry's own plane; a named one must exist, which
    /// catalog validation has already enforced via [`provider_has_surface`].
    fn plane(
        &self,
        surface: Option<&str>,
        endpoint: Option<&str>,
        surfaces: &BTreeMap<String, String>,
    ) -> Option<(ProviderAdapter, Option<String>)> {
        match surface {
            None => Some((self.adapter, endpoint.map(str::to_owned))),
            Some(name) => self
                .surfaces
                .get(name)
                .map(|surface| (surface.adapter, surfaces.get(name).cloned())),
        }
    }

    /// Enforce what a surface may and may not declare.
    ///
    /// Three rules, and the third is the load-bearing one:
    ///
    /// - A surface name must be non-blank; it is what a candidate writes in
    ///   `tiers.toml` to select the plane.
    /// - A surface's `base_url` must be non-blank, for the same reason the
    ///   entry-level one must: a surface exists to state an endpoint.
    /// - A surface's `base_url` may interpolate `{region}` **only if the entry
    ///   declares a `region_env`**. This is what keeps a surface from becoming a
    ///   second source of dispatchability. If a surface could name its own
    ///   region variable, a deployment could hold everything one plane needs and
    ///   not the other's, and "is this provider dispatchable" would stop having
    ///   a single answer — reopening exactly the catalog-versus-dispatch
    ///   divergence #89 was written to close, one level down.
    fn validate_surfaces(&self) -> Result<(), ProviderBuildError> {
        for (name, surface) in &self.surfaces {
            if name.trim().is_empty() {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!("provider {} declares a surface with a blank name", self.key),
                });
            }
            if surface.base_url.trim().is_empty() {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {} surface {name} has an empty base_url; a surface exists to \
                         name an endpoint, so it must name one",
                        self.key
                    ),
                });
            }
            if surface.base_url.contains(REGION_PLACEHOLDER)
                && self
                    .region_env
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty())
            {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {} surface {name} writes {REGION_PLACEHOLDER} in its base_url \
                         but the provider declares no region_env; a surface shares the provider's \
                         region rather than naming one of its own, so the placeholder would \
                         survive into the request URL",
                        self.key
                    ),
                });
            }
        }
        Ok(())
    }

    /// Enforce that the Bedrock classic-runtime adapter is given a HOST ROOT.
    ///
    /// Every other adapter here POSTs to its configured URL verbatim. This one
    /// cannot: `InvokeModel` puts the model id in the path
    /// (`/model/{id}/invoke`), so the wire appends per request and the
    /// configured value must be the host and nothing else.
    ///
    /// Refused rather than trimmed, because the two mistakes it catches are
    /// silent. Configure the mantle Messages path here by accident and the wire
    /// POSTs to `.../anthropic/v1/messages/model/us.anthropic.../invoke` — a
    /// 404 whose text names nothing an operator would connect to this file. A
    /// trailing slash is the harmless half of the same error and is accepted
    /// (the wire trims it), because refusing that would be pedantry rather than
    /// safety.
    fn validate_runtime_roots(&self) -> Result<(), ProviderBuildError> {
        let planes = std::iter::once((None, self.adapter, self.base_url.as_deref())).chain(
            self.surfaces.iter().map(|(name, surface)| {
                (
                    Some(name.as_str()),
                    surface.adapter,
                    Some(surface.base_url.as_str()),
                )
            }),
        );
        for (name, adapter, base_url) in planes {
            if adapter != ProviderAdapter::AnthropicBedrockRuntime {
                continue;
            }
            let plane = name.map_or_else(|| "entry".to_owned(), |name| format!("surface {name}"));
            let Some(base_url) = base_url else {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {} {plane} uses the anthropic_bedrock_runtime adapter and must \
                         declare a base_url; that wire builds its own path from the model id and \
                         has no implied host",
                        self.key
                    ),
                });
            };
            // The host root, with scheme and optional trailing slash stripped:
            // anything left containing `/` is a path this adapter must not have.
            let without_scheme = base_url
                .split_once("://")
                .map_or(base_url, |(_, rest)| rest)
                .trim_end_matches('/');
            if without_scheme.contains('/') {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {} {plane} declares base_url {base_url}, which carries a path; \
                         the anthropic_bedrock_runtime wire appends /model/<id>/invoke itself, so \
                         a path here would be prepended to it and dial a URL that does not exist",
                        self.key
                    ),
                });
            }
        }
        Ok(())
    }

    /// Enforce that an exemption from price reconciliation states its case.
    ///
    /// `unreconcilable_reason` turns off the alarm that guards margin for every
    /// candidate on this upstream. A present-but-blank one would do that while
    /// asserting nothing, and would be indistinguishable in the report from a
    /// gap somebody actually thought about — so an empty string is refused
    /// exactly as a blank retention-pin field is. Writing the sentence is the
    /// cost of the exemption, and it is the whole cost.
    fn validate_reconciliation(&self) -> Result<(), ProviderBuildError> {
        if self
            .unreconcilable_reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty())
        {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares an empty unreconcilable_reason; that field exempts every \
                     candidate on this upstream from price reconciliation, so it must say why the \
                     public catalog cannot price it and where the real rates come from",
                    self.key
                ),
            });
        }
        // A blank source key is refused for the mirror-image reason. Absent
        // means "join on the provider key", which is a real and common answer;
        // present-but-blank would join on the empty string, match nothing, and
        // report every candidate on this upstream `NOT IN SOURCE` — an alarm
        // that fires forever on a fault nobody can fix, which is how a report
        // stops being read.
        if self
            .source_provider_key
            .as_deref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares an empty source_provider_key; omit the field to join the \
                     reconciliation source on the provider key itself, or name the key this \
                     upstream is actually filed under",
                    self.key
                ),
            });
        }
        // Declaring both is a contradiction rather than a redundancy: one says
        // the source's row is authoritative under a different name, the other
        // says no row of the source's may be trusted at all. Whichever won
        // silently would decide whether this upstream's margin is checked,
        // which is not a thing to settle by field order in a struct.
        if self.source_provider_key.is_some() && self.unreconcilable_reason.is_some() {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares both source_provider_key and unreconcilable_reason; the \
                     first says the reconciliation source prices this upstream correctly under \
                     another key and the second says it cannot price it at all — keep the one that \
                     is true",
                    self.key
                ),
            });
        }
        Ok(())
    }

    /// Enforce the region declaration: a `base_url` interpolates iff the entry
    /// names the variable that fills it.
    ///
    /// Refused in BOTH directions, and neither is tidiness:
    ///
    /// - A `{region}` with no `region_env` has nothing to resolve it. The
    ///   substitution would not happen, and the wire would POST this provider's
    ///   credential and the customer's prompt to a host literally named
    ///   `bedrock-mantle.{region}.api.aws` — a DNS failure on a good day, and a
    ///   domain somebody else can register on a bad one.
    /// - A `region_env` with no `{region}` names a variable nothing reads. The
    ///   entry then claims to be region-configurable while dialling a fixed
    ///   host, so an operator who sets the variable and redeploys gets traffic
    ///   in the old region and no indication of it.
    ///
    /// Refuse a half-written, unenforceable, or unenforced per-response
    /// attestation.
    ///
    /// Every arm here refuses rather than repairs, and that is the point: this
    /// declaration is what stands between a customer and being served from a
    /// retaining upstream under a `zero` label, so any state where it is
    /// ambiguous whether the check runs must be a startup failure, never a
    /// default. The three faults, and why each is fatal rather than a warning:
    ///
    /// - **Half declared.** A header with no expected value has nothing to
    ///   compare against; an expected value with no header names nothing to
    ///   read it from. Neither half can be guessed — inventing `"true"` as a
    ///   default expectation would be this repo deciding what another company's
    ///   API means.
    /// - **Blank.** An empty header name matches no header, so every response
    ///   would read as absent and the lane would be permanently down; an empty
    ///   expectation would be satisfied only by an empty header value, which is
    ///   the same outage wearing a different shape.
    /// - **Declared on an adapter that cannot enforce it.** Only
    ///   [`ProviderAdapter::ChatCompletions`] threads a
    ///   [`ResponseAttestation`] into its wire. On any other adapter the fields
    ///   would parse, validate, appear in the inventory a reviewer reads — and
    ///   do nothing. That is strictly worse than not having the fields at all,
    ///   because the inventory would then assert a guarantee is being checked
    ///   on every request while no code checked it, and the failure is
    ///   invisible from every direction: no error, no log line, and a `zero`
    ///   posture still published to `/v1/models`. Refusing at load turns a
    ///   silent false claim into a process that will not start.
    ///
    /// The surfaces are checked too, for the same reason `validate_region`
    /// reads them: an entry whose named plane rides a different adapter is
    /// exactly where an unenforced declaration would hide.
    fn validate_attestation(&self) -> Result<(), ProviderBuildError> {
        let header = self.attestation_header.as_deref();
        let expect = self.attestation_expect.as_deref();
        if header.is_none() && expect.is_none() {
            return Ok(());
        }
        let (Some(header), Some(expect)) = (header, expect) else {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares half a response attestation; attestation_header names \
                     the header carrying the guarantee and attestation_expect names the value \
                     that satisfies it, and neither can be inferred from the other — declare \
                     both, or neither",
                    self.key
                ),
            });
        };
        if header.trim().is_empty() || expect.trim().is_empty() {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares a blank response attestation; a blank header name \
                     matches no response header and a blank expected value is satisfied by no \
                     real one, so either would refuse every request on this upstream forever",
                    self.key
                ),
            });
        }
        // Parsed here rather than at first use so a name HTTP cannot carry is
        // a refused inventory instead of a lane that fails closed on every
        // request with no indication that the cause is a typo.
        crate::wire::ResponseAttestation::new(header, expect).map_err(|detail| {
            ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares an attestation header that is not usable: {detail}",
                    self.key
                ),
            }
        })?;
        let unenforced: Vec<&str> = std::iter::once((None, self.adapter))
            .chain(
                self.surfaces
                    .iter()
                    .map(|(name, surface)| (Some(name.as_str()), surface.adapter)),
            )
            .filter(|(_, adapter)| *adapter != ProviderAdapter::ChatCompletions)
            .map(|(name, _)| name.unwrap_or("<the entry's own plane>"))
            .collect();
        if !unenforced.is_empty() {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares a response attestation but {} rides an adapter that \
                     does not enforce one; only chat_completions does. An attestation that is \
                     declared and not checked is worse than none, because the inventory then \
                     claims a per-response guarantee nothing verifies",
                    self.key,
                    unenforced.join(", ")
                ),
            });
        }
        Ok(())
    }

    /// The same present-iff-required shape as
    /// [`ProviderMetadata::validate_credential`], for the same reason: a
    /// half-written declaration is refused rather than half-applied.
    fn validate_region(&self) -> Result<(), ProviderBuildError> {
        // "Interpolates" means ANY plane of this entry does — the entry's own
        // endpoint or one of its surfaces. Reading only the entry's would make
        // the "declared but unused" arm fire on a perfectly good entry whose
        // regional plane is a surface, which is a false alarm on a real
        // configuration rather than a caught mistake.
        let interpolates = self
            .base_url
            .as_deref()
            .into_iter()
            .chain(
                self.surfaces
                    .values()
                    .map(|surface| surface.base_url.as_str()),
            )
            .any(|url| url.contains(REGION_PLACEHOLDER));
        let declared = self
            .region_env
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty());
        match (interpolates, declared) {
            (true, false) => Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} writes {REGION_PLACEHOLDER} in its base_url and declares no \
                     region_env to fill it; the placeholder would survive into the request URL \
                     and send this provider's credential to a host named {REGION_PLACEHOLDER}",
                    self.key
                ),
            }),
            (false, true) => Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares region_env but its base_url has no {REGION_PLACEHOLDER} \
                     to substitute; the entry would read as region-configurable while dialling a \
                     fixed host, so setting that variable would change nothing",
                    self.key
                ),
            }),
            _ => Ok(()),
        }
    }

    /// The same present-iff-interpolated rule for [`PROJECT_PLACEHOLDER`].
    ///
    /// Kept as its own function rather than folded into
    /// [`Self::validate_region`] with a parameter, because the two error
    /// messages are the useful part and they are not the same message: a
    /// missing region sends a request to the wrong PLACE, a missing project
    /// sends it to the wrong ACCOUNT. An operator reading the failure needs to
    /// know which.
    fn validate_project(&self) -> Result<(), ProviderBuildError> {
        let interpolates = self
            .base_url
            .as_deref()
            .into_iter()
            .chain(
                self.surfaces
                    .values()
                    .map(|surface| surface.base_url.as_str()),
            )
            .any(|url| url.contains(PROJECT_PLACEHOLDER));
        let declared = self
            .project_env
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty());
        match (interpolates, declared) {
            (true, false) => Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} writes {PROJECT_PLACEHOLDER} in its base_url and declares no \
                     project_env to fill it; the placeholder would survive into the request URL, \
                     so every request would name a project that does not exist",
                    self.key
                ),
            }),
            (false, true) => Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares project_env but its base_url has no \
                     {PROJECT_PLACEHOLDER} to substitute; the entry would read as \
                     project-scoped while dialling a fixed path, so setting that variable would \
                     change nothing",
                    self.key
                ),
            }),
            _ => Ok(()),
        }
    }

    /// A minted credential must name the variable it mints FROM, and must not
    /// be declared on an upstream that takes no credential at all.
    ///
    /// The second half is the one worth stating: `credential: "none"` plus a
    /// minting kind is a contradiction, and the safe reading of a contradiction
    /// on a credential is to refuse the entry rather than to pick a half. A
    /// keyless upstream that claimed to mint would reach
    /// [`crate::gcp_auth::ServiceAccountKey::from_json`] with an empty string
    /// on every request.
    /// Whether a customer may attach their own credential for this entry. See
    /// [`provider_accepts_byok`] for why each exclusion is structural.
    fn accepts_byok(&self) -> bool {
        self.credential == CredentialRequirement::Required && !self.credential_kind.needs_minting()
    }

    fn validate_credential_kind(&self) -> Result<(), ProviderBuildError> {
        if !self.credential_kind.needs_minting() {
            return Ok(());
        }
        if self.credential != CredentialRequirement::Required {
            return Err(ProviderBuildError::InvalidInventory {
                detail: format!(
                    "provider {} declares a credential_kind that must be exchanged for a token \
                     but takes no credential to exchange",
                    self.key
                ),
            });
        }
        Ok(())
    }

    /// This entry's endpoint with its region substituted, or `None` when the
    /// region cannot be resolved.
    ///
    /// `None` is NOT an error here, and that is the whole design: an
    /// unresolvable region makes this provider unavailable exactly as a missing
    /// credential does (see [`build_with_credentials`]), so a deployment that
    /// forgot `BEDROCK_REGION` loses its Bedrock rungs and keeps serving
    /// everything else. Guessing a default region would be the alternative, and
    /// it is the wrong one: `us-east-1` is a plausible guess that silently
    /// routes an eu-west-1 deployment's prompts to Virginia.
    fn endpoint<F>(&self, read_env: F) -> Option<Option<String>>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let Some(base_url) = self.base_url.as_deref() else {
            return Some(None);
        };
        resolve_region(
            base_url,
            self.region_env.as_deref(),
            self.project_env.as_deref(),
            read_env,
        )
        .map(Some)
    }

    /// The variable to name when an endpoint could not be addressed.
    ///
    /// A provider interpolates at most one of the two today, so this reports
    /// whichever it declares. When one ever declares both, the message names
    /// the region first — arbitrary, but a deployment missing both is missing
    /// its whole configuration and the operator will not be misled by which
    /// half is quoted.
    fn unresolvable_endpoint_env(&self) -> String {
        self.region_env
            .clone()
            .or_else(|| self.project_env.clone())
            .unwrap_or_default()
    }

    fn validate_credential(&self) -> Result<(), ProviderBuildError> {
        let declared = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        };
        match self.credential {
            CredentialRequirement::Required => {
                if !declared(&self.credential_env) || !declared(&self.secret_name) {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "provider {} must declare credential_env and secret_name, or declare \
                             \"credential\": \"none\" if its upstream takes no credential",
                            self.key
                        ),
                    });
                }
            }
            CredentialRequirement::None => {
                if self.adapter.dials_a_billed_endpoint() {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "provider {} declares \"credential\": \"none\" on an adapter that \
                             owns a cloud endpoint; only the chat_completions adapter — which \
                             has no implied endpoint and must name its own base_url — may be \
                             keyless",
                            self.key
                        ),
                    });
                }
                if self.credential_env.is_some() || self.secret_name.is_some() {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "provider {} declares \"credential\": \"none\" and still names a \
                             credential_env or secret_name; an entry cannot both need and not \
                             need a credential",
                            self.key
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Substitute a regional endpoint's [`REGION_PLACEHOLDER`], or `None` when the
/// region cannot be resolved.
///
/// Shared by the entry's own endpoint and by every surface, so a provider's
/// planes can never resolve their region by different rules — which is what
/// makes it safe for `dispatchable` to answer for all of them at once.
fn resolve_region<F>(
    base_url: &str,
    region_env: Option<&str>,
    project_env: Option<&str>,
    mut read_env: F,
) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut resolved = base_url.to_owned();
    // Both substitutions are all-or-nothing and both disqualify the provider
    // when their variable is unset. That is the same failure `region_env` has
    // always had, and it is deliberately NOT softened for the project: an
    // endpoint URL still containing a literal `{project}` would be dialled as a
    // host path segment named `{project}`, and the 404 that comes back reads as
    // an upstream outage rather than as a missing configuration value.
    if resolved.contains(REGION_PLACEHOLDER) {
        // Validated present whenever the placeholder is, so an empty name here
        // is unreachable; reading it as "no region" keeps this total rather
        // than panicking.
        let region = read_env(region_env.unwrap_or_default())?;
        resolved = resolved.replace(REGION_PLACEHOLDER, &region);
    }
    if resolved.contains(PROJECT_PLACEHOLDER) {
        let project = read_env(project_env.unwrap_or_default())?;
        resolved = resolved.replace(PROJECT_PLACEHOLDER, &project);
    }
    Some(resolved)
}

/// Whether `provider` declares a plane named `surface`.
///
/// The catalog's gate for a candidate's `surface = "..."`. Read from the
/// inventory rather than from a list inside `config.rs`, for the reason
/// [`provider_settles_free`] gives: per-upstream facts live here, and a second
/// list elsewhere is a second list to drift. A candidate naming a surface its
/// provider does not declare refuses the whole file — a structural fault, since
/// there is no sensible plane to fall back to and silently serving the entry's
/// own would dispatch InvokeModel bodies at a Messages endpoint.
#[must_use]
pub fn provider_has_surface(provider: &str, surface: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory
            .provider(provider)
            .is_some_and(|metadata| metadata.surfaces.contains_key(surface))
    })
}

/// Whether the plane this candidate would dispatch on can CARRY a client
/// `cache_control` breakpoint to the upstream.
///
/// Only the two adapters that speak the Anthropic Messages dialect can:
/// `cache_control` is a property of a Messages content block, and the other
/// two adapters have nowhere to put one. A chat-completions or Responses
/// upstream handed a breakpoint would either 400 or — far worse — accept the
/// request and silently drop it, which is a customer paying a cache-write
/// premium for a cache that was never written.
///
/// Read from the inventory rather than from a list in `config.rs`, for the
/// reason [`provider_has_surface`] gives. `surface` is the candidate's own
/// `surface = "..."`, so the answer is about the plane that will actually be
/// dialled: `bedrock` reaches the Messages dialect through its
/// `classic_runtime` surface and nothing else.
#[must_use]
pub fn provider_carries_cache_control(provider: &str, surface: Option<&str>) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory
            .provider(provider)
            .and_then(|metadata| match surface {
                None => Some(metadata.adapter),
                Some(name) => metadata.surfaces.get(name).map(|surface| surface.adapter),
            })
            .is_some_and(|adapter| {
                matches!(
                    adapter,
                    ProviderAdapter::Anthropic | ProviderAdapter::AnthropicBedrockRuntime
                )
            })
    })
}

/// Returns whether `provider` has a constructor in this module.
///
/// Configuration validation should use this function instead of maintaining a
/// second provider-name list.
#[must_use]
pub fn is_supported_provider(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| inventory.provider(provider).is_some())
}

/// Whether this provider declares that its traffic bills nobody (edge mode,
/// stage 2) — half of what makes a candidate free, the other half being a $0
/// price in `tiers.toml`.
///
/// Read in three places, all of which must agree on one answer: catalog
/// validation (a $0 candidate on a metered provider refuses the file),
/// [`crate::config::TierCandidate::is_free`] (the selection-time key, and
/// stage 3's metering-skip key), and the drift reconciliation (a free upstream
/// is one no public catalog covers). Answering from the inventory rather than
/// from a list of vendor names kept elsewhere is what keeps those three from
/// drifting apart.
///
/// See [`SettlementDeclaration`] for why neither the adapter nor the presence
/// of a credential can stand in for this declaration, and for the honest limit
/// of what it guarantees.
#[must_use]
pub fn provider_settles_free(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory
            .provider(provider)
            .is_some_and(|metadata| metadata.settlement == SettlementDeclaration::Free)
    })
}

/// This upstream's declared reason for being outside the reconciliation
/// source's coverage, if it declares one.
///
/// `None` — the overwhelmingly common case — means the provider is reconciled
/// exactly as it always was. `Some(reason)` means `admin catalog-drift` has no
/// trustworthy row to compare against and says so in the report, quoting this
/// text so the gap is argued in front of the operator on every run rather than
/// hidden in a config file.
///
/// Read from the inventory rather than from a table inside `drift.rs`, for the
/// reason [`provider_settles_free`] gives: the inventory is where per-upstream
/// facts live, and a second list elsewhere is a second list to drift.
#[must_use]
pub fn provider_unreconcilable_reason(provider: &str) -> Option<String> {
    ProviderInventory::load().ok().and_then(|inventory| {
        inventory
            .provider(provider)
            .and_then(|metadata| metadata.unreconcilable_reason.clone())
    })
}

/// The key this upstream is filed under in the reconciliation source, when it
/// declares one.
///
/// `None` means join on the provider key itself — the behaviour every entry had
/// before this existed, and still the right answer for `anthropic`, `openai`,
/// and `google`, whose keys models.dev happens to share.
///
/// Read from the inventory for the reason [`provider_unreconcilable_reason`]
/// gives: per-upstream facts live in one place, and a mapping table inside
/// `drift.rs` would be a second list to drift — the more dangerous kind, since
/// its failure is a silently wrong price rather than a compile error.
#[must_use]
pub fn provider_source_key(provider: &str) -> Option<String> {
    ProviderInventory::load().ok().and_then(|inventory| {
        inventory
            .provider(provider)
            .and_then(|metadata| metadata.source_provider_key.clone())
    })
}

/// Every provider this deployment is credentialed for, each paired with the
/// key it is filed under in the reconciliation source when it declares one.
///
/// The whole assembled inventory — shipped entries plus any operator overlay —
/// so a caller that must reason about ALL upstreams reads the same list the
/// dispatch path and the drift check read, rather than restating the vendor set
/// in a third place. `admin discover` is that caller: to find models the source
/// lists under a provider ZeroRouter holds a credential for, it must first know
/// which providers those are.
///
/// The pair mirrors [`provider_source_key`] exactly — `None` means "no declared
/// source key", the common case — so a caller applies the same identity-or-
/// declared rule that function documents. It deliberately does NOT resolve the
/// key down to a single string: `bedrock` declares no `source_provider_key` at
/// all (it carries an `unreconcilable_reason` instead, because the source
/// prices its SKU class wrongly), and only a caller that knows whether it is
/// asking a PRICING question or a mere EXISTENCE one can decide what to do with
/// that. Resolving here would have to pick one answer for both.
#[must_use]
pub fn inventory_source_keys() -> Vec<(String, Option<String>)> {
    ProviderInventory::load().map_or_else(
        |_| Vec::new(),
        |inventory| {
            inventory
                .providers()
                .map(|metadata| (metadata.key.clone(), metadata.source_provider_key.clone()))
                .collect()
        },
    )
}

/// The environment variables one provider reads: its credential, and its region
/// when it has a regional endpoint.
///
/// Exposed so a tool that must talk to an upstream OUTSIDE the request path —
/// `admin retention-drift --bedrock-live`, which calls Bedrock's control API
/// rather than its inference API — reads the same variable names the router
/// itself dispatches with, instead of restating them somewhere else. Two lists
/// of environment variable names is two lists to drift, and the failure would be
/// silent in the worst way: a live retention check reading a variable nobody
/// sets reports "cannot verify" forever while the router serves happily.
#[must_use]
pub fn provider_env_names(provider: &str) -> Option<(String, Option<String>)> {
    let inventory = ProviderInventory::load().ok()?;
    let metadata = inventory.provider(provider)?;
    Some((
        metadata.credential_env.clone()?,
        metadata.region_env.clone(),
    ))
}

#[derive(Debug, Error)]
pub enum ProviderBuildError {
    #[error("embedded provider inventory is invalid: {detail}")]
    InvalidInventory { detail: String },
    #[error("failed to read the operator provider inventory at {path}")]
    OperatorInventoryUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the operator provider inventory has already been loaded")]
    OperatorInventoryAlreadyLoaded,
    #[error("provider route has no candidates")]
    EmptyRoute,
    #[error("candidate {candidate} names unsupported provider {provider}")]
    UnsupportedProvider { candidate: String, provider: String },
    #[error(
        "candidate {candidate} names surface {surface}, which provider {provider} does not declare"
    )]
    UnknownSurface {
        candidate: String,
        provider: String,
        surface: String,
    },
    #[error(
        "no upstream credentials are available; checked environment variables {credential_envs:?}"
    )]
    NoAvailableCredentials { credential_envs: Vec<String> },
}

/// An available upstream candidate and its canonical tier-table definition.
///
/// The provider client is deliberately private so all attributed calls pass
/// through [`ProviderDispatch`]. Both walks try these entries in order
/// themselves; nothing in this module decides when to retry or when to move on.
pub struct ProviderCandidate {
    definition: TierCandidate,
    provider: Arc<dyn ModelProvider>,
    /// Whether `provider` holds the CUSTOMER's credential rather than
    /// ZeroRouter's (bring-your-own-key, migration 0026).
    ///
    /// It rides on the candidate rather than being looked up again later
    /// because the walk consumes the route (`ProviderRoute::into_candidates`)
    /// before any settle site runs, and this is the fact those sites need: what
    /// the client that actually served was holding. Re-deriving it downstream
    /// from "does this user have a key for this provider" would be a second
    /// definition of the same thing, and the two would disagree in exactly the
    /// cases that matter — a credential that failed to open, or a key detached
    /// while the request was in flight.
    byok: bool,
    /// The provider whose BYOK attempt this candidate exists to fall back FROM
    /// (migration 0028), on the house-credential twin of a candidate whose key
    /// the customer opted in for. `None` on every ordinary candidate.
    ///
    /// # Why the fallback is a second CANDIDATE and not a second try
    ///
    /// Expressing it as a rung means the walk needs no new dispatch path: the
    /// existing loop already tries candidates in order, already records one
    /// attempt row each, already stops at the first that serves, and already
    /// prices the settled row from whichever candidate that was
    /// (`RequestFeatures::on_candidate`). A fallback attempt is therefore
    /// billed at the full catalog price by exactly the mechanism that bills
    /// every other house dispatch — there is no second fee decision to keep in
    /// agreement with the first — and "exactly one attempt settles" is
    /// inherited rather than re-argued.
    ///
    /// It also puts the house credential back where the attestation lives: this
    /// twin is built with `byok = false`, so `create_provider` gives it the
    /// per-response retention check that a BYOK dispatch deliberately skips.
    /// The customer's own attempt stays exempt and the house attempt does not,
    /// which is the honest reading of who is promising what.
    byok_fallback_for: Option<String>,
}

#[cfg(test)]
impl ProviderCandidate {
    /// Aim a candidate at a scripted local upstream so the router-owned
    /// streaming walk can be driven end to end without a network provider.
    pub(crate) fn against_local_upstream(definition: TierCandidate, base_url: &str) -> Self {
        // A stand-in upstream for tests that only need "some provider":
        // the owned Responses wire, aimed at a local scripted server.
        Self::with_provider(
            definition,
            Arc::new(crate::wire::OpenAiResponsesWire::new(
                "test-upstream",
                "test-credential",
                Some(base_url),
                None,
                1,
            )),
        )
    }
}

#[cfg(feature = "testing")]
impl ProviderCandidate {
    /// Put a pre-built provider client behind a canonical tier candidate.
    ///
    /// The only way anything other than a configured upstream can serve a
    /// candidate, so it is gated on the `testing` feature and cannot exist in
    /// a production binary.
    #[must_use]
    pub fn with_provider(definition: TierCandidate, provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            definition,
            provider,
            // A test fake holds no credential at all, so it is not the
            // customer's. A test that means to describe BYOK dispatch drives
            // the real assembly path (`router/tests/byok.rs`).
            byok: false,
            // Nor is it anyone's fallback: the opted-in twin is built only by
            // the real assembly path, beside the BYOK candidate it belongs to.
            byok_fallback_for: None,
        }
    }
}

impl ProviderCandidate {
    #[must_use]
    pub fn definition(&self) -> &TierCandidate {
        &self.definition
    }

    /// Whether this candidate dispatches on the customer's own credential.
    #[must_use]
    pub fn is_byok(&self) -> bool {
        self.byok
    }

    /// The provider whose BYOK attempt this candidate is the opted-in
    /// house-credential fallback for, if it is one (migration 0028).
    ///
    /// The walk uses this to keep the rung CLOSED by default: a fallback twin
    /// is skipped unless its own BYOK twin has already run and failed in a way
    /// the opt-in covers. That default-closed reading is what makes a
    /// mis-ordered route lose the fallback rather than mis-bill for it.
    #[must_use]
    pub fn byok_fallback_for(&self) -> Option<&str> {
        self.byok_fallback_for.as_deref()
    }

    #[must_use]
    pub fn supports_streaming(&self) -> bool {
        self.provider.supports_streaming()
    }

    /// Run a non-streaming call directly against this candidate's configured
    /// model, without retrying another candidate.
    pub async fn chat(
        &self,
        request: ChatRequest<'_>,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        self.provider
            .chat(request, &self.definition.model, temperature)
            .await
    }

    /// Start a stream directly against this candidate's configured model.
    #[must_use]
    pub fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>> {
        self.provider
            .stream_chat(request, &self.definition.model, temperature, options)
    }
}

impl fmt::Debug for ProviderCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCandidate")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// Provider clients for one resolved route.
///
/// `candidates` preserves the tier-table order after candidates whose ECS
/// credential is unavailable have been removed. That order is the whole of the
/// route's policy: which candidate is tried, how often, and when to give up are
/// the walk's decisions, in `api.rs`.
pub struct ProviderRoute {
    candidates: Vec<ProviderCandidate>,
    /// Which providers on this route were built from the CUSTOMER's own
    /// credential rather than ZeroRouter's (bring-your-own-key).
    ///
    /// Recorded here because it is a property of route ASSEMBLY — the moment
    /// the credential was chosen — and every downstream consumer needs it
    /// after the fact: metering has to know whether the served attempt is
    /// billed at 5% or at catalog, and the response block has to tell the
    /// customer their own provider agreement governs the request. Recomputing
    /// it later from "does this user hold a key for this provider" would be a
    /// second definition of the same fact, and the two could disagree exactly
    /// when it mattered — a key detached mid-request, or a provider whose
    /// credential failed to open and fell back to the house lane.
    byok_providers: BTreeSet<String>,
}

/// A customer's own upstream credentials for the span of one request, keyed by
/// provider alias.
///
/// Held by value and dropped with the route: nothing caches these across
/// requests, which is what makes a detached or rotated key stop dispatching
/// immediately rather than at the end of some TTL.
#[derive(Default)]
pub struct ByokCredentials {
    by_provider: BTreeMap<String, (String, bool)>,
}

impl ByokCredentials {
    /// Credentials with no fallback opt-in — migration 0026's behaviour, and
    /// what every caller that does not care about 0028 should build.
    #[must_use]
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self {
            by_provider: pairs
                .into_iter()
                .map(|(provider, credential)| (provider, (credential, false)))
                .collect(),
        }
    }

    /// Credentials carrying each key's fallback opt-in (migration 0028).
    #[must_use]
    pub fn with_fallback(pairs: Vec<(String, String, bool)>) -> Self {
        Self {
            by_provider: pairs
                .into_iter()
                .map(|(provider, credential, fallback)| (provider, (credential, fallback)))
                .collect(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }

    fn get(&self, provider: &str) -> Option<&str> {
        self.by_provider
            .get(provider)
            .map(|(credential, _)| credential.as_str())
    }

    /// Whether this customer asked for a house-credential retry when their key
    /// for `provider` fails. FALSE for a provider they hold no key for, which
    /// is the same answer as declining and needs no separate arm.
    fn falls_back(&self, provider: &str) -> bool {
        self.by_provider
            .get(provider)
            .is_some_and(|(_, fallback)| *fallback)
    }
}

/// Never let a customer's credential reach a log line through a derived
/// `Debug`, the same discipline `StripeSettings` applies in [`crate::web`].
impl fmt::Debug for ByokCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ByokCredentials")
            .field("providers", &self.by_provider.keys().collect::<Vec<_>>())
            .field("credentials", &"<scrubbed>")
            .finish()
    }
}

/// Whether a customer may attach their own credential for this provider.
///
/// Two exclusions, both structural rather than cautious:
///
/// * A **keyless** upstream (`"credential": "none"`) has nothing to substitute.
///   Accepting a key for one would store a third party's secret that could
///   never be used.
/// * A **minting** upstream (Vertex's service-account JSON, exchanged for a
///   short-lived OAuth token) takes a credential of a completely different
///   shape and dispatches on the result of a network exchange, not on the
///   stored value. Substituting a customer's key into the token-mint path
///   without the mint being designed for it is how a customer's service-account
///   blob would end up in a process-global token cache keyed by ZeroRouter's
///   own environment variable ([`TOKEN_CACHES`]) — a cross-tenant credential
///   leak. Refusing the attach outright is the honest v1; the lane keeps
///   serving on the house credential at catalog price.
#[must_use]
pub fn provider_accepts_byok(provider: &str) -> bool {
    let Ok(inventory) = ProviderInventory::load() else {
        return false;
    };
    inventory
        .provider(provider)
        .is_some_and(ProviderMetadata::accepts_byok)
}

/// Every provider alias in this deployment's inventory that can take a
/// customer's own credential, for the portal's attach form.
#[must_use]
pub fn byok_capable_providers() -> Vec<String> {
    let Ok(inventory) = ProviderInventory::load() else {
        return Vec::new();
    };
    inventory
        .providers()
        .filter(|metadata| metadata.accepts_byok())
        // A provider ZeroRouter itself cannot dispatch to is not offered: the
        // catalog does not list its lanes, so a key attached for it could never
        // serve a request, and inviting a customer to paste one would be asking
        // for a secret with nothing to spend it on.
        .filter(|metadata| provider_is_dispatchable(metadata.key.as_str()))
        .map(|metadata| metadata.key.clone())
        .collect()
}

impl ProviderRoute {
    /// Build a per-request provider route from canonical tier candidates.
    ///
    /// Missing credentials make an individual candidate unavailable. Building
    /// fails only when no configured candidate can be constructed. Do not cache
    /// this value across requests: fallback selection metadata is request-scoped.
    /// **Async because of one provider, and structured so it stays that way.**
    /// Vertex's credential is a service-account key that must be exchanged for
    /// a short-lived token over the network before anything can be dispatched
    /// ([`CredentialKind`]). That exchange is the only await here, it is
    /// skipped entirely for a route with no minting provider on it, and it is
    /// almost always a cache hit ([`crate::gcp_auth::TokenCache`]).
    ///
    /// The minting is done BEFORE [`build_with_credentials`] rather than inside
    /// it, and that is deliberate: it keeps the whole of route assembly — the
    /// dispatchability rules, the surface resolution, the rung ordering —
    /// synchronous and exactly as it was, with the token substituted in through
    /// the same `credential_for` seam a test uses. A minted token is just
    /// another string that came from somewhere, which is the property that made
    /// this a small change instead of a rewrite of the request path.
    pub async fn new(
        candidates: Vec<TierCandidate>,
        max_output_tokens: u32,
    ) -> Result<Self, ProviderBuildError> {
        Self::new_with_byok(candidates, max_output_tokens, &ByokCredentials::default()).await
    }

    /// [`Self::new`], with the customer's own credentials substituted for
    /// ZeroRouter's on the providers they have attached a key for.
    ///
    /// # Why substituting HERE is what makes "no silent fallback" true
    ///
    /// The route is the only thing the walk can dispatch through, and it is
    /// built once per request. A candidate whose provider the customer has a
    /// key for gets a client holding THAT key and no other, so a BYOK upstream
    /// answering 401 retries against the same customer credential and
    /// eventually fails the request with a clear error — there is no house
    /// client on this route for that provider to fall back onto, because none
    /// was ever constructed. The guarantee is structural rather than a branch
    /// somebody has to remember not to write.
    ///
    /// Failover to a DIFFERENT provider is untouched and still correct: that
    /// candidate has no customer key, so it is built from ZeroRouter's
    /// credential and settles at full catalog price. Which of the two served is
    /// recorded per-provider in [`Self::byok_providers`], so metering prices the
    /// attempt that actually won rather than the intent the request started
    /// with.
    pub async fn new_with_byok(
        candidates: Vec<TierCandidate>,
        max_output_tokens: u32,
        byok: &ByokCredentials,
    ) -> Result<Self, ProviderBuildError> {
        let inventory = ProviderInventory::load()?;
        let minted = mint_credentials(&inventory, &candidates).await?;
        build_with_byok(&inventory, candidates, max_output_tokens, byok, |env| {
            // `Some(None)` is NOT the same as absent: it means this variable
            // holds a key that must be minted from and the mint failed, so the
            // credential is unavailable. Only a variable this route never
            // minted for reaches the environment. See [`mint_credentials`].
            resolve_credential(&minted, env, read_credential)
        })
    }

    /// Assemble a route from pre-built candidates.
    ///
    /// Goes through the same [`assemble_route`] wiring as [`Self::new`], so a
    /// test drives the production route and only the leaf provider clients
    /// differ. Gated on the `testing` feature for the same reason
    /// [`ProviderCandidate::with_provider`] is.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn from_candidates(candidates: Vec<ProviderCandidate>) -> Self {
        assemble_route(candidates)
    }

    #[must_use]
    pub fn candidates(&self) -> &[ProviderCandidate] {
        &self.candidates
    }

    /// The providers on this route whose client holds the CUSTOMER's credential.
    ///
    /// Read by metering to decide whether the served attempt is billed at the
    /// BYOK fee or at catalog, and by the response block to tell the customer
    /// which agreement governs the request.
    #[must_use]
    pub fn byok_providers(&self) -> &BTreeSet<String> {
        &self.byok_providers
    }

    /// Whether the candidate that served dispatched on the customer's own key.
    #[must_use]
    pub fn served_on_byok(&self, provider: &str) -> bool {
        self.byok_providers.contains(provider)
    }

    /// The selection-policy seam (design doc: Engine "Selection policy"):
    /// `api::order_candidates` permutes the route here, AFTER construction,
    /// so a reorder moves each candidate's definition and its transport
    /// together and can never pair one rung's definition with another's
    /// client — and an injected test route is reordered exactly as a
    /// production one.
    pub fn candidates_mut(&mut self) -> &mut Vec<ProviderCandidate> {
        &mut self.candidates
    }

    #[must_use]
    pub fn into_candidates(self) -> Vec<ProviderCandidate> {
        self.candidates
    }
}

impl fmt::Debug for ProviderRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRoute")
            .field("candidates", &self.candidates)
            .finish_non_exhaustive()
    }
}

/// Put each candidate behind a client, dropping the ones whose provider has no
/// credential in the environment.
///
/// The inventory is a parameter for the same reason `credential_for` is: both
/// are the deployment's answers to questions this function does not ask, and
/// passing them in lets a test drive the production code path over a
/// configuration it constructed rather than over whatever the process happens
/// to have installed globally.
/// Assembly for a deployment with no BYOK in play.
///
/// `#[cfg(test)]` because production reaches [`build_with_byok`] directly
/// through [`ProviderRoute::new_with_byok`], and the tests below predate BYOK
/// and describe deployments without it — which is exactly what this spelling
/// says. Keeping it means those tests were not rewritten to thread a parameter
/// they have no opinion about, so they still pin the pre-BYOK assembly.
#[cfg(test)]
fn build_with_credentials<F>(
    inventory: &ProviderInventory,
    candidates: Vec<TierCandidate>,
    max_output_tokens: u32,
    credential_for: F,
) -> Result<ProviderRoute, ProviderBuildError>
where
    F: FnMut(&str) -> Option<String>,
{
    build_with_byok(
        inventory,
        candidates,
        max_output_tokens,
        &ByokCredentials::default(),
        credential_for,
    )
}

/// [`build_with_credentials`], with the customer's own credentials substituted
/// on the providers they have attached a key for.
///
/// The two are one function with the BYOK map defaulted, rather than a copy:
/// a route built for a customer with no attached keys must go through exactly
/// the assembly a route built before BYOK existed went through, and the only
/// way to guarantee that is for there to be one assembly.
fn build_with_byok<F>(
    inventory: &ProviderInventory,
    candidates: Vec<TierCandidate>,
    max_output_tokens: u32,
    byok: &ByokCredentials,
    mut credential_for: F,
) -> Result<ProviderRoute, ProviderBuildError>
where
    F: FnMut(&str) -> Option<String>,
{
    if candidates.is_empty() {
        return Err(ProviderBuildError::EmptyRoute);
    }

    let mut available = Vec::with_capacity(candidates.len());
    let mut missing_credentials = Vec::new();
    let mut byok_providers = BTreeSet::new();
    // Keyed by (provider, surface) rather than by provider alone: two planes of
    // one upstream are two clients on two hosts speaking two wires, so sharing
    // one between them would dispatch a candidate's request at the other
    // plane's endpoint. Candidates on the SAME plane still share, which is the
    // connection-pool saving this map exists for.
    // Keyed by whose credential the client holds as well as by plane. Since
    // migration 0028 a route may legitimately carry TWO clients for one
    // (provider, surface) — the customer's and, as its opted-in fallback,
    // ZeroRouter's — and they are emphatically not interchangeable: one dials
    // as the customer and asserts no retention guarantee, the other dials as
    // ZeroRouter and asserts one. Sharing a cache slot between them would hand
    // a BYOK candidate the house client, which is the exact substitution #103's
    // no-fallback rule exists to make impossible.
    let mut providers = BTreeMap::<(&str, Option<String>, bool), Arc<dyn ModelProvider>>::new();
    let mut unavailable = BTreeSet::new();

    for definition in candidates {
        let metadata = inventory.provider(&definition.provider).ok_or_else(|| {
            ProviderBuildError::UnsupportedProvider {
                candidate: definition.id.clone(),
                provider: definition.provider.clone(),
            }
        })?;
        let provider_key = metadata.key.as_str();
        // Availability is a property of the UPSTREAM, not of one of its planes:
        // a surface shares the entry's credential and region, so a provider that
        // cannot be dispatched to cannot be dispatched to on any plane.
        if unavailable.contains(provider_key) {
            continue;
        }
        let surface = definition.surface.clone();

        // The customer's own credential for this upstream, if they attached
        // one and this entry can take it. Read before `dispatchable` rather
        // than patched over its result, so a BYOK provider is dispatchable on
        // the customer's key even in the case where ZeroRouter's own key for it
        // is absent — the substitution replaces the credential, never the
        // endpoint or region resolution beside it.
        let byok_credential = metadata
            .accepts_byok()
            .then(|| byok.get(provider_key))
            .flatten();

        let byok_slot = byok_credential.is_some();
        let provider =
            if let Some(provider) = providers.get(&(provider_key, surface.clone(), byok_slot)) {
                Arc::clone(provider)
            } else {
                // Only the entry's OWN credential variable is answered from the
                // customer's key. Every other variable `dispatchable` reads — the
                // region, the project — still comes from the deployment, because
                // those describe where ZeroRouter dials, not who it dials as.
                let credential_env = metadata.credential_env.clone();
                let mut credential_for_entry = |env: &str| -> Option<String> {
                    match byok_credential {
                        Some(credential) if Some(env) == credential_env.as_deref() => {
                            Some(credential.to_owned())
                        }
                        _ => credential_for(env),
                    }
                };
                let (credential, endpoint, surfaces) =
                    match metadata.dispatchable(&mut credential_for_entry) {
                        Dispatchable::Ready {
                            credential,
                            endpoint,
                            surfaces,
                        } => (credential, endpoint, surfaces),
                        Dispatchable::Missing { env } => {
                            missing_credentials.push(env);
                            unavailable.insert(provider_key);
                            continue;
                        }
                    };
                let (adapter, endpoint) = metadata
                    .plane(surface.as_deref(), endpoint.as_deref(), &surfaces)
                    .ok_or_else(|| ProviderBuildError::UnknownSurface {
                        candidate: definition.id.clone(),
                        provider: definition.provider.clone(),
                        surface: surface.clone().unwrap_or_default(),
                    })?;
                let provider = create_provider(
                    metadata,
                    adapter,
                    &credential,
                    max_output_tokens,
                    endpoint.as_deref(),
                    byok_credential.is_some(),
                )?;
                providers.insert(
                    (provider_key, surface.clone(), byok_slot),
                    Arc::clone(&provider),
                );
                provider
            };
        if byok_credential.is_some() {
            byok_providers.insert(provider_key.to_owned());
        }

        // The opted-in house-credential twin (migration 0028), built HERE so it
        // is bound to the candidate it falls back from and lands immediately
        // after it.
        //
        // Ordering later permutes the route, and the two rungs carry the same
        // definition — same id, same rates — so they sort on identical keys and
        // a stable sort keeps this one behind its original. That is a
        // convenience rather than the guarantee: the walk arms a fallback twin
        // only after its BYOK twin has actually failed, so a route that somehow
        // put the twin first would skip it and lose the fallback rather than
        // dispatch a house request the customer had not paid for yet.
        //
        // A twin that cannot be built is simply absent. `credential_for` is
        // consulted WITHOUT the customer's override, so this asks the real
        // question — does ZeroRouter hold its own key for this upstream? — and
        // a deployment that does not keeps serving the BYOK rung alone. Nothing
        // is added to `unavailable` on that path: the upstream is perfectly
        // dispatchable on the customer's credential, and marking it otherwise
        // would delete the very candidate this twin exists to protect.
        let fallback_twin = byok_credential
            .filter(|_| byok.falls_back(provider_key))
            .and_then(|_| {
                let cached = providers.get(&(provider_key, surface.clone(), false));
                if let Some(provider) = cached {
                    return Some(Arc::clone(provider));
                }
                let Dispatchable::Ready {
                    credential,
                    endpoint,
                    surfaces,
                } = metadata.dispatchable(&mut credential_for)
                else {
                    return None;
                };
                let (adapter, endpoint) =
                    metadata.plane(surface.as_deref(), endpoint.as_deref(), &surfaces)?;
                // `byok = false`: this client dials as ZeroRouter, so the
                // per-response retention attestation this lane is sold under is
                // asserted on it, exactly as it is for a customer who brought
                // no key at all.
                let provider = create_provider(
                    metadata,
                    adapter,
                    &credential,
                    max_output_tokens,
                    endpoint.as_deref(),
                    false,
                )
                .ok()?;
                providers.insert(
                    (provider_key, surface.clone(), false),
                    Arc::clone(&provider),
                );
                Some(provider)
            });

        available.push(ProviderCandidate {
            definition: definition.clone(),
            provider,
            byok: byok_credential.is_some(),
            byok_fallback_for: None,
        });
        if let Some(provider) = fallback_twin {
            available.push(ProviderCandidate {
                definition,
                provider,
                // Not BYOK: it holds ZeroRouter's credential, so it settles at
                // the full catalog price and consumes none of the customer's
                // monthly allowance. One flag decides both, which is what keeps
                // the price and the disclosure from ever disagreeing.
                byok: false,
                byok_fallback_for: Some(provider_key.to_owned()),
            });
        }
    }

    if available.is_empty() {
        return Err(ProviderBuildError::NoAvailableCredentials {
            credential_envs: missing_credentials,
        });
    }

    Ok(ProviderRoute {
        byok_providers,
        ..assemble_route(available)
    })
}

/// Put ordered candidates behind a route.
///
/// Kept as its own function, rather than inlined into the two constructors, so
/// a test-supplied route and a credential-built one go through the same wiring
/// and cannot diverge.
fn assemble_route(candidates: Vec<ProviderCandidate>) -> ProviderRoute {
    ProviderRoute {
        candidates,
        // An injected test route dispatches on fakes that hold no credential at
        // all, so it is not BYOK — a test that means to describe BYOK dispatch
        // drives the real assembly path (`router/tests/byok.rs`).
        byok_providers: BTreeSet::new(),
    }
}

/// Per-provider token caches, keyed by credential env variable.
///
/// Process-global because the CACHE is the point: a per-request cache would
/// mint a token per request, which is the behaviour this exists to avoid. Keyed
/// by the variable rather than by the provider key so that a rotated secret —
/// which reaches this process as a NEW TASK, with a fresh map — cannot be
/// served a token minted from the old one.
///
/// The outer lock is a std mutex held only long enough to clone an `Arc`; the
/// awaiting happens on the `TokenCache`'s own mutex, never this one.
static TOKEN_CACHES: std::sync::OnceLock<
    std::sync::Mutex<BTreeMap<String, Arc<crate::gcp_auth::TokenCache>>>,
> = std::sync::OnceLock::new();

/// Resolve the raw credential for a minting provider into a usable token,
/// building (and remembering) that provider's token cache on first use.
///
/// The service-account JSON may be given inline or as a PATH to a file holding
/// it. Both are supported because the two deployment shapes genuinely differ
/// and neither is wrong: ECS injects secrets as environment values, so the blob
/// arrives inline; a developer running locally has a downloaded key file and
/// should not have to paste 2 KB of JSON into a shell. The discriminator is
/// whether the value starts with `{` — a JSON object cannot begin any other
/// way after trimming, and a filesystem path cannot begin with `{`.
async fn minted_credential(
    metadata: &ProviderMetadata,
    raw: &str,
) -> Result<String, ProviderBuildError> {
    let env_name = metadata.credential_env.clone().unwrap_or_default();
    let cache = {
        let caches = TOKEN_CACHES.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
        let mut caches = caches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match caches.get(&env_name) {
            Some(cache) => Arc::clone(cache),
            None => {
                let trimmed = raw.trim();
                let json = if trimmed.starts_with('{') {
                    trimmed.to_owned()
                } else {
                    std::fs::read_to_string(trimmed).map_err(|error| {
                        ProviderBuildError::InvalidInventory {
                            detail: format!(
                                "provider {}: {env_name} is not JSON and could not be read as a \
                                 path to a service-account key file: {error}",
                                metadata.key
                            ),
                        }
                    })?
                };
                let key =
                    crate::gcp_auth::ServiceAccountKey::from_json(&json).map_err(|error| {
                        ProviderBuildError::InvalidInventory {
                            detail: format!("provider {}: {error}", metadata.key),
                        }
                    })?;
                tracing::info!(
                    provider = %metadata.key,
                    service_account = %key.client_email(),
                    "minting Vertex access tokens for this service account"
                );
                let cache = Arc::new(crate::gcp_auth::TokenCache::new(
                    key,
                    reqwest::Client::new(),
                ));
                caches.insert(env_name.clone(), Arc::clone(&cache));
                cache
            }
        }
    };

    cache
        .token(std::time::SystemTime::now())
        .await
        .map(|token| token.secret().to_owned())
        .map_err(|error| ProviderBuildError::InvalidInventory {
            detail: format!("provider {}: {error}", metadata.key),
        })
}

/// Mint a token for every provider on this route whose credential needs it.
///
/// Returns a map from credential env variable to the token that should stand in
/// for its raw value. Empty — and free — for a route with no minting provider,
/// which is every route in the catalog but Vertex's.
///
/// **A minting failure drops the rung; it does not fail the route.** That is
/// the same rule a missing credential follows, and it is the right one for the
/// same reason: an unreachable Google token endpoint should cost a customer the
/// Vertex lane, not the whole request when another candidate could serve it.
/// The error is logged at `warn` with the provider named, because unlike a
/// missing key — a deployment fact that is true until someone changes it — this
/// one is usually transient and worth seeing in a log.
/// **The map's value is an `Option`, and that is a safety property rather than
/// a convenience.** A key present with `None` means "this variable is
/// minting-only and minting did not succeed", and the caller must then treat
/// the credential as ABSENT. Falling back to the environment there would read
/// the raw service-account key — a 2 KB JSON blob containing an RSA private key
/// — and send it upstream as a bearer token: the private key would leave the
/// process, over the network, in an `Authorization` header, on every request,
/// because a token refresh briefly failed. `a_failed_mint_never_sends_the_service_account_key`
/// is the test that holds this shut.
async fn mint_credentials(
    inventory: &ProviderInventory,
    candidates: &[TierCandidate],
) -> Result<BTreeMap<String, Option<String>>, ProviderBuildError> {
    let mut minted = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for candidate in candidates {
        if !seen.insert(candidate.provider.clone()) {
            continue;
        }
        let Some(metadata) = inventory.provider(&candidate.provider) else {
            continue;
        };
        if !metadata.credential_kind.needs_minting() {
            continue;
        }
        let Some(env_name) = metadata.credential_env.as_deref() else {
            continue;
        };
        // The raw key comes through the ordinary environment read. A provider
        // whose key is absent is left alone entirely: `dispatchable` will drop
        // the rung by itself, and minting from an empty string would turn a
        // dark lane into a noisy error on every request.
        let Some(raw) = read_credential(env_name) else {
            continue;
        };
        match minted_credential(metadata, &raw).await {
            Ok(token) => {
                minted.insert(env_name.to_owned(), Some(token));
            }
            Err(error) => {
                tracing::warn!(
                    provider = %candidate.provider,
                    %error,
                    "could not mint an access token; this lane is unavailable for this request"
                );
                // Recorded as an explicit `None` rather than simply left out.
                // Leaving it out would let the caller fall back to the raw
                // environment value, which for this provider is the private
                // key itself. See this function's doc comment.
                minted.insert(env_name.to_owned(), None);
            }
        }
    }
    Ok(minted)
}

/// Resolve one credential variable against the tokens minted for this route.
///
/// **Extracted from the closure it used to be, because as a closure it could
/// not be tested and therefore was not.** A test asserting this rule inline had
/// no connection to the code that ran: reinstating the environment fallback in
/// `ProviderRoute::new` left the whole suite green, so the guard below was
/// decorative for the path that matters. Naming the rule is what lets
/// `a_failed_mint_never_sends_the_service_account_key` drive the real thing.
///
/// The three cases, and the middle one is the guard:
///
/// - **not in the map** — an ordinary credential this route minted nothing for.
///   Read the environment, exactly as every provider before Vertex did.
/// - **in the map as `None`** — a minting provider whose mint FAILED. The
///   credential is unavailable and the rung drops. Falling through to the
///   environment here would read the raw service-account key and send an RSA
///   private key upstream in an `Authorization` header.
/// - **in the map as `Some`** — the minted token stands in for the variable's
///   contents, so the wire is handed the token and never the key.
fn resolve_credential<F>(
    minted: &BTreeMap<String, Option<String>>,
    env: &str,
    read_env: F,
) -> Option<String>
where
    F: FnOnce(&str) -> Option<String>,
{
    match minted.get(env) {
        Some(token) => token.clone(),
        None => read_env(env),
    }
}

fn read_credential(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Test seam, `STRIPE_API_BASE`-shaped: `ZEROROUTER_PROVIDER_BASE_URL_<KEY>`
/// (key uppercased) overrides a provider's endpoint so a failure-injection
/// harness can stand a misbehaving upstream in front of a REAL router
/// process — half-open sockets, mid-stream drops, 429 storms are not
/// reachable from the in-process fakes. Leave unset in any real deployment;
/// an override is logged loudly at startup so it can never hide in one.
fn base_url_override(key: &str) -> Option<String> {
    let variable = format!(
        "ZEROROUTER_PROVIDER_BASE_URL_{}",
        key.to_ascii_uppercase().replace('-', "_")
    );
    let value = env::var(&variable).ok()?.trim().to_owned();
    if value.is_empty() {
        return None;
    }
    tracing::warn!(provider = key, %variable, "provider base URL overridden — test seam active");
    Some(value)
}

/// Put one upstream behind its wire.
///
/// `effective_base_url` is resolved by the caller rather than read from
/// `metadata` here, because resolving it can FAIL — a regional endpoint whose
/// region variable is unset has no address — and the caller is the one that can
/// answer that failure correctly, by dropping the rung instead of the route.
/// `byok` says the credential belongs to the CUSTOMER rather than to
/// ZeroRouter. It changes exactly one thing — whether the house's per-response
/// retention attestation is asserted — and the reasoning is in the
/// chat-completions arm below.
fn create_provider(
    metadata: &ProviderMetadata,
    adapter: ProviderAdapter,
    credential: &str,
    max_output_tokens: u32,
    effective_base_url: Option<&str>,
    byok: bool,
) -> Result<Arc<dyn ModelProvider>, ProviderBuildError> {
    let alias = metadata.key.as_str();
    let provider: Arc<dyn ModelProvider> = match adapter {
        ProviderAdapter::Anthropic => Arc::new(AnthropicWire::new(
            alias,
            credential,
            effective_base_url,
            max_output_tokens,
            // Honor ZR's 15-minute upstream budget (api.rs), not an adapter
            // default.
            900,
        )),
        ProviderAdapter::OpenAiResponses => Arc::new(OpenAiResponsesWire::new(
            alias,
            credential,
            effective_base_url,
            Some(max_output_tokens),
            // Same budget note as the Anthropic arm: honor ZR's 15-minute
            // upstream budget, not an adapter default.
            900,
        )),
        ProviderAdapter::ChatCompletions => {
            let wire = ChatCompletionsWire::new(
                alias,
                credential,
                effective_base_url,
                Some(max_output_tokens),
                // Same budget note as the arms above.
                900,
            );
            // The declaration reaching the wire is what makes the check run,
            // so the two failure directions are worth being explicit about.
            //
            // A declared attestation that failed to parse here would produce a
            // wire with NO check — a lane silently serving without the
            // guarantee it is sold under, which is the one outcome this
            // mechanism must never produce. `validate_attestation` has already
            // parsed the same pair at load, so this cannot happen; the `?`
            // makes the impossible case a refused route rather than an
            // unchecked one, on the principle that a violated guarantee
            // becomes a refusal instead of a silent downgrade.
            //
            // # Why a BYOK dispatch asserts NOTHING here
            //
            // The attestation reads a response header stating the ZDR state of
            // the ACCOUNT that made the request. On ZeroRouter's own key that
            // is ZeroRouter's account, and ZeroRouter sells the lane as
            // zero-retention, so asserting-and-refusing is what keeps the
            // advertised guarantee honest.
            //
            // On a customer's key the header describes the CUSTOMER's team, and
            // all three ways of handling that are not equally defensible:
            //
            // * Keep asserting. A customer whose own xAI team has not enabled
            //   ZDR would have every request refused — refused on the strength
            //   of a guarantee ZeroRouter is not making about their traffic.
            //   Worse, a PASS would be a fact about the customer's own contract
            //   that ZeroRouter would be presenting as something ZeroRouter
            //   verified on their behalf. That is false comfort, which is worse
            //   than no check. It is also a fail-closed control whose subject
            //   silently changed meaning, so it would read as an outage to
            //   exactly the customers who brought a key.
            // * Drop it silently. Then the catalog's zero-retention label reads
            //   as though it still governs their traffic, and it does not.
            // * Drop it and SAY SO. The request is governed by the customer's
            //   own agreement with the provider, so ZeroRouter stops asserting
            //   a guarantee that is not its to give and stamps `byok: true` on
            //   the response (`crate::openai::ZeroRouterResponseMetadata`) so
            //   the caller can see which contract applies. The docs say it in
            //   words; `/v1/models` is deliberately untouched, because those
            //   labels describe the HOUSE contract, which is exactly what a
            //   customer without BYOK gets.
            //
            // The third is what ships.
            match (
                metadata.attestation_header.as_deref().filter(|_| !byok),
                metadata.attestation_expect.as_deref().filter(|_| !byok),
            ) {
                (Some(header), Some(expect)) => {
                    let attestation = crate::wire::ResponseAttestation::new(header, expect)
                        .map_err(|detail| ProviderBuildError::InvalidInventory {
                            detail: format!(
                                "provider {alias} declares an attestation header that is not \
                                 usable: {detail}"
                            ),
                        })?;
                    Arc::new(wire.with_attestation(attestation))
                }
                _ => Arc::new(wire),
            }
        }
        ProviderAdapter::AnthropicBedrockRuntime => {
            // The only arm that REQUIRES an endpoint rather than accepting
            // `None`: this wire builds `/model/<id>/invoke` onto a host root and
            // has no default host to fall back on. Validation guarantees the
            // declaration; this turns a violated guarantee into a refused route
            // rather than a request to a URL beginning `/model/`.
            let base_url =
                effective_base_url.ok_or_else(|| ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "provider {alias} dispatches on the anthropic_bedrock_runtime adapter \
                         with no resolved endpoint; that wire has no implied host"
                    ),
                })?;
            Arc::new(BedrockRuntimeWire::new(
                alias,
                credential,
                base_url,
                max_output_tokens,
                // Same budget note as the arms above.
                900,
            ))
        }
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use crate::config::ModelMetadata;
    use crate::provider::ModelRates;

    use super::*;

    fn candidate(id: &str, provider: &str) -> TierCandidate {
        candidate_on(id, provider, None)
    }

    /// A candidate pinned to one of its provider's named API planes.
    fn candidate_on(id: &str, provider: &str, surface: Option<&str>) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: provider.to_owned(),
            model: format!("upstream/{id}"),
            surface: surface.map(str::to_owned),
            rates: crate::provider::RateSchedule::flat(ModelRates {
                cache_write_per_mtok: None,
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: None,
            }),
            // Provider dispatch reads `provider`, `model`, and `surface`.
            metadata: ModelMetadata::default(),
        }
    }

    #[test]
    fn supported_provider_check_uses_constructor_table() {
        for provider in [
            "anthropic",
            "openai",
            "google",
            "bedrock",
            "fireworks",
            "xai",
            "vertex",
            "groq",
            "together",
        ] {
            assert!(is_supported_provider(provider));
        }
        // Retired with the git dependency that supplied their adapters, and
        // still gone. THREE names have now left this list rather than joined
        // it: `bedrock` came BACK on 2026-08-20 as the zero-retention lane, on
        // ZeroRouter's own Messages wire rather than the pinned Converse
        // adapter that once served it, and `fireworks` came back the same day
        // on the generic chat-completions wire. `together` returned on
        // 2026-08-22, likewise on chat completions. None of them is the old
        // pinned adapter returning — all are entries in
        // `config/providers.json` speaking a wire this repo owns, which is why
        // a name leaving this list is a deliberate edit and not a regression.
        for provider in ["deepinfra", "minimax"] {
            assert!(
                !is_supported_provider(provider),
                "{provider} is no longer in the inventory"
            );
        }
        assert!(!is_supported_provider("unknown"));
    }

    #[test]
    fn the_shipped_inventory_dispatches_exactly_where_it_says_it_does() {
        // Every upstream a default deployment can reach, and the wire it speaks
        // — pinned so that adding an adapter, or a provider that uses one, is
        // always a deliberate edit to this list rather than a side effect.
        //
        // Stage 1 of edge mode added the chat-completions adapter with no
        // shipped route using it, and this assertion held that separation until
        // Google arrived on 2026-08-18: Gemini is served over Google's
        // OpenAI-compatible endpoint, so it is the first shipped provider on
        // that wire. The adapter being dual-use (local rungs AND a metered
        // cloud vendor) is the design's own scope, not a widening of it — see
        // `SettlementDeclaration`, which is why "on the local wire" still
        // cannot make anything free.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let adapters: Vec<(&str, ProviderAdapter)> = inventory
            .providers
            .iter()
            .map(|provider| (provider.key.as_str(), provider.adapter))
            .collect();
        //
        // Bedrock joined on 2026-08-20 on the ANTHROPIC adapter, which is the
        // surprising half and the reason it is pinned here. Bedrock's mantle
        // plane does serve an OpenAI-compatible `/v1/chat/completions`, and the
        // obvious move was to put it on the chat-completions wire beside
        // Google. AWS's model cards say otherwise: Claude marks Chat
        // Completions and Responses unsupported and Messages supported, so the
        // lane rides the Messages wire — which already sends the `x-api-key`
        // and `anthropic-version` headers that endpoint takes.
        //
        // Fireworks joined on 2026-08-20 as the SECOND entry on the
        // chat-completions adapter, and it is the unsurprising half: Fireworks
        // publishes an OpenAI-compatible endpoint and nothing else, so the
        // generic wire is simply correct. Worth pinning anyway, because it is
        // the shape every planned open-weight vendor takes — a key, a full
        // endpoint URL, and this adapter — so a fourth chat-completions entry
        // appearing here without a deliberate edit would be the signal that
        // somebody added a vendor without reading what that entails.
        //
        // xAI is that fourth entry, added 2026-08-20, and it WAS a deliberate
        // edit. It is on this adapter for the plain reason — xAI publishes an
        // OpenAI-compatible `/v1/chat/completions` — but it is also the only
        // entry in the inventory that declares a per-response attestation, and
        // this adapter is the only one that enforces one
        // (`validate_attestation` refuses the declaration anywhere else). So a
        // FIFTH chat-completions entry is still the signal this comment
        // describes, and moving the xai entry to another adapter would not
        // quietly drop its retention check — it would refuse to load.
        //
        // `vertex` is that fifth entry, added 2026-08-21, and it too was a
        // deliberate edit. Vertex AI publishes an OpenAI-compatible
        // `/v1/.../endpoints/openapi/chat/completions`, so the WIRE is the
        // ordinary one and no new adapter was warranted. What is new about the
        // entry is not its adapter but its CREDENTIAL: it is the first to
        // declare `credential_kind = "google_service_account"`, because Google
        // issues no long-lived key for this surface and the wire is handed a
        // minted OAuth token instead of the secret's contents
        // (`crate::gcp_auth`). THAT SIXTH ENTRY ARRIVED, and so did a seventh
        // and an eighth: `groq` and `together` on 2026-08-22. The note above
        // asked whoever crossed the line to stop and think, so here is the
        // thinking, recorded rather than silently overwritten.
        //
        // The worry a growing list is meant to catch is a shared adapter
        // quietly becoming a shared IDENTITY — many upstreams behind one wire,
        // one of them acquiring a behaviour the others inherit by accident.
        // That has not happened and the structure is why: every entry below
        // carries its own key, its own credential env, and its own `base_url`,
        // which `validate` REQUIRES of every chat-completions entry precisely
        // so that no upstream can inherit another's endpoint by omission. The
        // one per-upstream behaviour that exists — xAI's response attestation
        // — is declared per entry and asserted by a test that iterates the
        // whole inventory, so a new chat-completions entry cannot pick it up
        // by being on the same wire.
        //
        // What the count still means is that `chat_completions` is now the
        // house wire and a regression in it reaches six of eight upstreams.
        // That is an argument for the coverage in `wire/chat_completions.rs`,
        // not against another entry here.
        assert_eq!(
            adapters,
            [
                ("anthropic", ProviderAdapter::Anthropic),
                ("openai", ProviderAdapter::OpenAiResponses),
                ("google", ProviderAdapter::ChatCompletions),
                ("bedrock", ProviderAdapter::Anthropic),
                ("fireworks", ProviderAdapter::ChatCompletions),
                ("xai", ProviderAdapter::ChatCompletions),
                ("vertex", ProviderAdapter::ChatCompletions),
                ("groq", ProviderAdapter::ChatCompletions),
                ("together", ProviderAdapter::ChatCompletions),
            ]
        );

        // Bedrock is a SECOND entry on the Anthropic adapter, and the two must
        // stay distinguishable in every way that matters: its own key, its own
        // credential, its own endpoint. Sharing a wire is not sharing an
        // account, and the whole point of the lane is that the account differs.
        let bedrock = inventory.provider("bedrock").expect("bedrock is shipped");
        let anthropic = inventory
            .provider("anthropic")
            .expect("anthropic is shipped");
        assert_ne!(bedrock.credential_env, anthropic.credential_env);
        assert_eq!(bedrock.credential_env.as_deref(), Some("BEDROCK_API_KEY"));
        assert_eq!(bedrock.secret_name.as_deref(), Some("bedrock-api-key"));
        assert_eq!(bedrock.settlement, SettlementDeclaration::Metered);
        assert_eq!(bedrock.credential, CredentialRequirement::Required);
        assert!(
            anthropic.base_url.is_none(),
            "the first-party Anthropic entry still owns its endpoint; only the \
             Bedrock one overrides, and it must not drag Anthropic's traffic with it"
        );

        // The endpoint is REGIONAL and says so. Both halves are asserted
        // because either one alone is a live foot-gun: a placeholder with no
        // variable dials a host named `{region}`, and a hardcoded region sends
        // an eu-west-1 deployment's prompts to Virginia.
        let base_url = bedrock
            .base_url
            .as_deref()
            .expect("bedrock declares its endpoint");
        assert!(base_url.contains(REGION_PLACEHOLDER), "{base_url}");
        assert_eq!(bedrock.region_env.as_deref(), Some("BEDROCK_REGION"));
        assert!(
            base_url.ends_with("/anthropic/v1/messages"),
            "the Messages wire posts to the URL verbatim, so the entry must carry \
             the full mantle path — NOT the /v1/chat/completions surface, which \
             AWS documents Claude as not supporting: {base_url}"
        );

        // Google bills like any other vendor: the free lane is entered only by
        // an explicit `settlement: free`, never by the wire a provider speaks.
        let google = inventory.provider("google").expect("google is shipped");
        assert_eq!(google.settlement, SettlementDeclaration::Metered);
        assert_eq!(google.credential, CredentialRequirement::Required);
        assert!(
            google
                .base_url
                .as_deref()
                .is_some_and(|url| url.ends_with("/chat/completions")),
            "the chat-completions wire posts to the URL verbatim, so the entry \
             must carry the full endpoint path, not an OpenAI-style /v1 base"
        );
    }

    #[test]
    fn a_provider_entry_can_select_the_chat_completions_adapter() {
        // The config contract: the tag a provider entry writes to reach the
        // new wire, and the base_url that entry needs — this adapter is the
        // one with no implied endpoint.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [{
                "key": "local-llama",
                "adapter": "chat_completions",
                "credential_env": "LOCAL_LLAMA_API_KEY",
                "secret_name": "local-llama-api-key",
                "display_name": "llama.cpp (local)",
                "base_url": "http://127.0.0.1:8080/v1/chat/completions"
            }]}"#,
        )
        .expect("a chat_completions entry parses");
        inventory.validate().expect("and validates");
        let metadata = inventory
            .provider("local-llama")
            .expect("the entry is addressable by key");
        assert_eq!(metadata.adapter, ProviderAdapter::ChatCompletions);

        // And it builds a client, through the same constructor a production
        // route uses.
        let provider = create_provider(
            metadata,
            metadata.adapter,
            "secret",
            crate::provider::BASELINE_MAX_TOKENS,
            metadata.base_url.as_deref(),
            false,
        )
        .expect("the chat-completions arm builds");
        assert_eq!(provider.alias(), "local-llama");
        assert!(provider.supports_streaming());
    }

    #[test]
    fn a_chat_completions_entry_without_a_base_url_is_refused() {
        // The config foot-gun this rule exists for: the wire's default
        // endpoint is OpenAI's, so an operator's LOCAL entry that forgets
        // base_url would validate cleanly and then ship that provider's
        // credential and prompts to api.openai.com. An adapter with no
        // implied endpoint has to be told its endpoint.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [{
                "key": "local-llama",
                "adapter": "chat_completions",
                "credential_env": "LOCAL_LLAMA_API_KEY",
                "secret_name": "local-llama-api-key"
            }]}"#,
        )
        .expect("the entry parses");
        let error = inventory
            .validate()
            .expect_err("a chat_completions entry with no base_url is invalid");
        let detail = error.to_string();
        assert!(detail.contains("local-llama"), "{detail}");
        assert!(detail.contains("base_url"), "{detail}");

        // The other two adapters are unaffected: their wires own their
        // endpoints, so omitting base_url is the normal case for them.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [
                {"key": "anthropic", "adapter": "anthropic",
                 "credential_env": "A", "secret_name": "a"},
                {"key": "openai", "adapter": "openai_responses",
                 "credential_env": "O", "secret_name": "o"}
            ]}"#,
        )
        .expect("the entries parse");
        inventory
            .validate()
            .expect("the endpoint-owning adapters still need no base_url");
    }

    #[test]
    fn an_empty_base_url_is_still_refused_for_the_new_adapter() {
        // base_url is meaningful configuration for this adapter rather than a
        // test seam, which makes the existing empty-value rule load-bearing:
        // an entry that declares one must declare a real one.
        let inventory: ProviderInventory = serde_json::from_str(
            r#"{"providers": [{
                "key": "local-llama",
                "adapter": "chat_completions",
                "credential_env": "LOCAL_LLAMA_API_KEY",
                "secret_name": "local-llama-api-key",
                "base_url": "   "
            }]}"#,
        )
        .expect("the entry parses");
        assert!(matches!(
            inventory.validate(),
            Err(ProviderBuildError::InvalidInventory { .. })
        ));
    }

    #[test]
    fn an_unknown_adapter_tag_is_a_loud_inventory_error() {
        // The adapter tag is a closed set; a typo must fail the whole
        // inventory rather than silently selecting a default wire.
        assert!(
            serde_json::from_str::<ProviderInventory>(
                r#"{"providers": [{
                    "key": "x", "adapter": "chat_completion",
                    "credential_env": "E", "secret_name": "s"
                }]}"#,
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // Edge mode, stage 2: the operator's own inventory, layered over the
    // shipped one, and the keyless declaration a local server needs
    // (`docs/design/edge-mode-local-rung.md`).
    // -----------------------------------------------------------------------

    /// A one-entry operator inventory document.
    fn operator_json(body: &str) -> String {
        format!(r#"{{"providers": [{body}]}}"#)
    }

    /// The canonical local entry: keyless, free-settling, chat-completions,
    /// own endpoint.
    const LOCAL_ENTRY: &str = r#"{
        "key": "local-llama",
        "adapter": "chat_completions",
        "credential": "none",
        "settlement": "free",
        "display_name": "llama.cpp (local)",
        "base_url": "http://127.0.0.1:8080/v1/chat/completions"
    }"#;

    fn assemble(body: &str) -> Result<ProviderInventory, ProviderBuildError> {
        let entries = ProviderInventory::parse_operator(&operator_json(body))
            .expect("the operator document should parse");
        ProviderInventory::assembled(&entries)
    }

    #[test]
    fn an_operator_entry_is_added_to_the_shipped_inventory_not_merged_into_it() {
        // The layering contract in one assertion: the shipped entries are
        // untouched and the operator's is addressable beside them. Both facts
        // matter — a deployment that gains a local rung must not lose, or
        // silently alter, the cloud rungs it bursts to.
        let inventory = assemble(LOCAL_ENTRY).expect("a valid operator entry should assemble");
        let keys: Vec<&str> = inventory
            .providers
            .iter()
            .map(|provider| provider.key.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "anthropic",
                "openai",
                "google",
                "bedrock",
                "fireworks",
                "xai",
                "vertex",
                "groq",
                "together",
                "local-llama"
            ]
        );

        let local = inventory
            .provider("local-llama")
            .expect("the operator entry is addressable by key");
        assert_eq!(local.adapter, ProviderAdapter::ChatCompletions);
        assert_eq!(local.credential, CredentialRequirement::None);
        assert_eq!(local.settlement, SettlementDeclaration::Free);
        for shipped in ["anthropic", "openai"] {
            let metadata = inventory.provider(shipped).expect("shipped entry survives");
            assert_eq!(metadata.credential, CredentialRequirement::Required);
            assert_eq!(
                metadata.settlement,
                SettlementDeclaration::Metered,
                "a shipped provider bills, and says so by saying nothing"
            );
        }
    }

    #[test]
    fn an_operator_entry_may_not_shadow_a_shipped_provider() {
        // The rule that keeps this an extension point rather than a
        // credential-exfiltration primitive: `openai` names the endpoint
        // ZeroRouter's own OpenAI key is sent to, and no file on the
        // operator's disk may repoint it.
        let error = assemble(
            r#"{
                "key": "openai",
                "adapter": "chat_completions",
                "credential": "none",
                "base_url": "http://127.0.0.1:8080/v1/chat/completions"
            }"#,
        )
        .expect_err("shadowing a shipped provider must be refused");
        let detail = error.to_string();
        assert!(detail.contains("openai"), "{detail}");
        assert!(detail.contains("shadows"), "{detail}");
    }

    #[test]
    fn free_settlement_is_refused_on_every_adapter_that_dials_a_billed_endpoint() {
        // Necessary but not sufficient. The chat-completions adapter is the
        // only one that CAN settle free, because it is the only one without an
        // endpoint ZeroRouter is invoiced for — but being on it proves nothing
        // (see the dual-use note on `SettlementDeclaration`), which is why the
        // declaration exists at all.
        for adapter in ["anthropic", "openai_responses"] {
            let error = assemble(&format!(
                r#"{{"key": "wishful", "adapter": "{adapter}", "settlement": "free",
                    "credential_env": "W", "secret_name": "w"}}"#
            ))
            .expect_err("free settlement must be refused on a billed adapter");
            let detail = error.to_string();
            assert!(detail.contains("wishful"), "{adapter}: {detail}");
            assert!(detail.contains("settlement"), "{adapter}: {detail}");
        }
    }

    #[test]
    fn a_free_settling_provider_may_be_keyless_or_credentialed() {
        // The correction the review forced. Credential presence was never the
        // right key: a local vLLM behind a bearer token is an ordinary
        // deployment, and it is free because its operator runs it — not because
        // it happens to be unauthenticated. Both shapes assemble.
        let keyless = assemble(LOCAL_ENTRY).expect("a keyless free provider assembles");
        assert_eq!(
            keyless
                .provider("local-llama")
                .expect("entry exists")
                .settlement,
            SettlementDeclaration::Free
        );

        let credentialed = assemble(
            r#"{
                "key": "secure-local",
                "adapter": "chat_completions",
                "credential_env": "SECURE_LOCAL_API_KEY",
                "secret_name": "secure-local-api-key",
                "settlement": "free",
                "base_url": "http://127.0.0.1:8000/v1/chat/completions"
            }"#,
        )
        .expect("a credentialed free provider assembles too");
        let entry = credentialed.provider("secure-local").expect("entry exists");
        assert_eq!(entry.settlement, SettlementDeclaration::Free);
        assert_eq!(entry.credential, CredentialRequirement::Required);
    }

    #[test]
    fn an_operator_entry_may_not_borrow_a_shipped_providers_credential() {
        // The other door into the same room the shadowing rule guards. Blocking
        // the KEY `anthropic` while leaving `ANTHROPIC_API_KEY` readable would
        // stop an operator entry from being Anthropic and not from reading
        // Anthropic's key and posting it to an address of its choosing.
        let error = assemble(
            r#"{
                "key": "definitely-local",
                "adapter": "chat_completions",
                "credential_env": "ANTHROPIC_API_KEY",
                "secret_name": "anthropic-api-key",
                "base_url": "http://198.51.100.7:8080/v1/chat/completions"
            }"#,
        )
        .expect_err("borrowing a shipped credential must be refused");
        let detail = error.to_string();
        assert!(detail.contains("definitely-local"), "{detail}");
        assert!(detail.contains("ANTHROPIC_API_KEY"), "{detail}");
    }

    #[test]
    fn a_misspelled_field_refuses_the_inventory_instead_of_defaulting() {
        // Every field this struct gained in stage 2 is a safety declaration
        // whose ABSENCE is meaningful, which is the shape where a silent
        // default is worst: `"credentail": "none"` would leave the entry
        // `required` and the typo invisible until a route quietly lost a rung.
        for typo in [
            r#""credentail": "none""#,
            r#""settlment": "free""#,
            r#""base_urls": "http://127.0.0.1:8080/v1/chat/completions""#,
        ] {
            let refused = ProviderInventory::parse_operator(&operator_json(&format!(
                r#"{{"key": "local-llama", "adapter": "chat_completions", {typo}}}"#
            )));
            assert!(
                refused.is_err(),
                "a misspelled field must refuse the document: {typo}"
            );
        }

        // And the fields that DO exist still parse — including the shipped
        // inventory, which names none of the stage-2 ones.
        ProviderInventory::shipped().expect("the shipped inventory still parses");
        assemble(LOCAL_ENTRY).expect("a fully-specified entry still parses");
    }

    #[test]
    fn a_keyless_declaration_is_refused_on_every_adapter_that_owns_a_cloud_endpoint() {
        // The abuse this stage must not open. "No credential needed" is a fact
        // about a server on the operator's own network; on an adapter that
        // dials api.anthropic.com or api.openai.com it is either a typo or an
        // attempt to run a paid upstream unauthenticated, and either way it
        // would erode "this candidate has no key" as a reason to skip a paid
        // rung.
        for adapter in ["anthropic", "openai_responses"] {
            let error = assemble(&format!(
                r#"{{"key": "sneaky", "adapter": "{adapter}", "credential": "none"}}"#
            ))
            .expect_err("keyless must be refused on a cloud adapter");
            let detail = error.to_string();
            assert!(detail.contains("sneaky"), "{adapter}: {detail}");
            assert!(detail.contains("chat_completions"), "{adapter}: {detail}");
        }
    }

    #[test]
    fn a_keyless_entry_that_still_names_a_credential_is_refused() {
        // Contradiction is refused rather than resolved: an entry claiming
        // both to need and not need a credential has one half wrong, and
        // picking a winner means guessing which.
        for extra in [
            r#""credential_env": "LOCAL_KEY""#,
            r#""secret_name": "local-key""#,
        ] {
            let error = assemble(&format!(
                r#"{{
                    "key": "local-llama",
                    "adapter": "chat_completions",
                    "credential": "none",
                    "base_url": "http://127.0.0.1:8080/v1/chat/completions",
                    {extra}
                }}"#
            ))
            .expect_err("a contradictory credential declaration must be refused");
            assert!(error.to_string().contains("cannot both need"), "{error}");
        }
    }

    #[test]
    fn a_keyless_entry_still_has_to_name_its_endpoint() {
        // The stage-1 rule is not weakened by the stage-2 one. Without
        // base_url this wire defaults to api.openai.com, so a keyless entry
        // that omitted it would send the operator's prompts to OpenAI with no
        // credential — the exact opposite of running a model locally, twice
        // over.
        let error = assemble(
            r#"{"key": "local-llama", "adapter": "chat_completions", "credential": "none"}"#,
        )
        .expect_err("a keyless entry with no base_url must be refused");
        assert!(error.to_string().contains("base_url"), "{error}");
    }

    #[test]
    fn a_credentialed_entry_must_still_name_both_its_credential_and_its_secret() {
        // The pre-existing rule, unchanged, now that both fields are optional
        // in the SCHEMA: an entry that says nothing about its credential is
        // still `required` and must still name where the key lives. Silence is
        // not the way to become keyless.
        for body in [
            r#"{"key": "half", "adapter": "anthropic", "credential_env": "A"}"#,
            r#"{"key": "half", "adapter": "anthropic", "secret_name": "a"}"#,
            r#"{"key": "half", "adapter": "anthropic"}"#,
            r#"{"key": "half", "adapter": "anthropic", "credential_env": "  ", "secret_name": "a"}"#,
        ] {
            let error =
                assemble(body).expect_err("an under-declared credentialed entry must be refused");
            assert!(error.to_string().contains("credential_env"), "{error}");
        }
    }

    #[test]
    fn a_keyless_provider_is_never_the_rung_a_route_loses_to_a_missing_key() {
        // The behavioural point of the whole declaration, over the production
        // route builder. With NO credentials in the environment at all, the
        // paid rung drops out — as it always has — and the local rung still
        // builds. The operator never had to invent a fake key to get that, and
        // the missing-credential skip is untouched for everyone else.
        let inventory = assemble(LOCAL_ENTRY).expect("the local entry should assemble");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("cloud", "openai"),
                candidate("local", "local-llama"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| None,
        )
        .expect("a keyless provider should still build a route");

        let ids: Vec<&str> = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect();
        assert_eq!(ids, ["local"]);
    }

    // -----------------------------------------------------------------------
    // Regional endpoints (2026-08-20): Bedrock's mantle host carries the region
    // in its name, so the shipped entry interpolates rather than hardcoding one.
    // -----------------------------------------------------------------------

    #[test]
    fn a_regional_base_url_must_name_the_variable_that_fills_it() {
        // The foot-gun: without a `region_env` the placeholder survives into
        // the request URL, so this provider's credential and the customer's
        // prompt go to a host literally named `bedrock-mantle.{region}.api.aws`
        // — a domain ZeroRouter does not own and somebody else could.
        let error = assemble(
            r#"{
                "key": "regional",
                "adapter": "chat_completions",
                "credential_env": "REGIONAL_API_KEY",
                "secret_name": "regional-api-key",
                "base_url": "https://svc.{region}.example.test/v1/chat/completions"
            }"#,
        )
        .expect_err("a placeholder with nothing to fill it must be refused");
        let detail = error.to_string();
        assert!(detail.contains("regional"), "{detail}");
        assert!(detail.contains("{region}"), "{detail}");
    }

    #[test]
    fn a_region_variable_with_no_placeholder_to_fill_is_refused_too() {
        // The other direction, and it is not tidiness. The entry reads as
        // region-configurable while dialling a fixed host, so an operator who
        // sets the variable and redeploys gets traffic in the old region and
        // nothing anywhere says so.
        let error = assemble(
            r#"{
                "key": "fixed",
                "adapter": "chat_completions",
                "credential_env": "FIXED_API_KEY",
                "secret_name": "fixed-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "region_env": "FIXED_REGION"
            }"#,
        )
        .expect_err("a region_env with no placeholder must be refused");
        assert!(error.to_string().contains("fixed"), "{error}");
    }

    #[test]
    fn a_regional_endpoint_resolves_its_region_from_the_declared_variable() {
        // The substitution itself, over the SHIPPED Bedrock entry rather than a
        // fixture, so the assertion is about the endpoint real traffic goes to.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock = inventory.provider("bedrock").expect("bedrock is shipped");

        let resolved = bedrock
            .endpoint(|name| (name == "BEDROCK_REGION").then(|| "eu-west-1".to_owned()))
            .expect("a resolvable region yields an endpoint")
            .expect("the entry declares a base_url");
        assert_eq!(
            resolved,
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1/messages"
        );
        assert!(
            !resolved.contains(REGION_PLACEHOLDER),
            "no placeholder may survive into a dialled URL: {resolved}"
        );

        // A non-regional entry is untouched by any of this.
        let google = inventory.provider("google").expect("google is shipped");
        assert_eq!(
            google.endpoint(|_| None).expect("no region to resolve"),
            google.base_url.clone()
        );
    }

    /// The shipped Vertex entry, asserted by value — every field of it is a
    /// claim that something else in the repo depends on.
    #[test]
    fn the_shipped_vertex_entry_is_the_zero_retention_gemini_lane_it_claims_to_be() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let vertex = inventory.provider("vertex").expect("vertex is shipped");

        // THE GLOBAL ENDPOINT, and this is the assertion with money behind it.
        // Google prices non-global endpoints 10% above global for these models
        // (and has done since 2026-07-01). `tiers.toml` records the global
        // rates as this lane's cost basis, so a base_url pointing at a regional
        // host would sell every Vertex token ~10% below what Google invoices —
        // and the catalog's basis-vs-sell invariant could not see it, because
        // both sides are written in the same file and would simply both be
        // wrong. Nothing else in the repository checks this; this line is it.
        assert_eq!(
            vertex.base_url.as_deref(),
            Some(
                "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/endpoints/openapi/chat/completions"
            ),
            "the Vertex lane must dial the GLOBAL endpoint — a regional one costs 10% more"
        );
        assert!(
            vertex
                .base_url
                .as_deref()
                .is_some_and(|url| url.contains("/locations/global/")),
            "stated twice on purpose: the substring is what the 10% turns on"
        );

        // The credential is a service-account key to be exchanged, not a key to
        // be sent. If this ever read `ApiKey` the wire would put a 2 KB JSON
        // blob containing an RSA private key in an Authorization header.
        assert_eq!(vertex.credential_kind, CredentialKind::GoogleServiceAccount);
        assert!(vertex.credential_kind.needs_minting());
        assert_eq!(
            vertex.credential_env.as_deref(),
            Some("VERTEX_SERVICE_ACCOUNT")
        );
        assert_eq!(vertex.project_env.as_deref(), Some("VERTEX_PROJECT_ID"));

        // No attestation: Google publishes no per-response retention header,
        // and declaring one that never arrives would fail every request closed.
        assert!(vertex.attestation_header.is_none());

        // The wire is the ordinary OpenAI-compatible one. Vertex's
        // `endpoints/openapi` surface speaks it, which is why this lane needed
        // no new adapter.
        assert_eq!(vertex.adapter, ProviderAdapter::ChatCompletions);
    }

    /// Both variables or no lane. A project is an ACCOUNT boundary, and the
    /// zero-retention configuration is applied per project, so a Vertex request
    /// against the wrong project is served under a posture nobody verified.
    #[test]
    fn the_vertex_endpoint_resolves_its_project_and_disqualifies_without_one() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let vertex = inventory.provider("vertex").expect("vertex is shipped");

        let resolved = vertex
            .endpoint(|name| (name == "VERTEX_PROJECT_ID").then(|| "zr-prod-42".to_owned()))
            .expect("a resolvable project yields an endpoint")
            .expect("the entry declares a base_url");
        assert_eq!(
            resolved,
            "https://aiplatform.googleapis.com/v1/projects/zr-prod-42/locations/global/endpoints/openapi/chat/completions"
        );
        assert!(
            !resolved.contains(PROJECT_PLACEHOLDER),
            "no placeholder may survive into a dialled URL: {resolved}"
        );

        // Without the project there is no address, so the rung goes — exactly
        // as a missing region drops a Bedrock rung. Dialling a path segment
        // literally named `{project}` would 404, and a 404 reads as an upstream
        // outage rather than as the missing configuration it is.
        assert!(
            vertex.endpoint(|_| None).is_none(),
            "an unresolvable project must disqualify rather than dial a placeholder"
        );
    }

    #[test]
    fn a_project_placeholder_without_a_project_env_is_refused() {
        let error = assemble(
            r#"{
                "key": "projectless",
                "adapter": "chat_completions",
                "credential_env": "PROJECTLESS_API_KEY",
                "secret_name": "projectless-api-key",
                "base_url": "https://svc.example.test/v1/projects/{project}/chat/completions"
            }"#,
        )
        .expect_err("a project placeholder with nothing to fill it must be refused");
        assert!(error.to_string().contains("project_env"), "{error}");
    }

    #[test]
    fn a_project_env_without_a_placeholder_is_refused() {
        let error = assemble(
            r#"{
                "key": "fixedproject",
                "adapter": "chat_completions",
                "credential_env": "FIXEDPROJECT_API_KEY",
                "secret_name": "fixedproject-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "project_env": "FIXEDPROJECT_PROJECT"
            }"#,
        )
        .expect_err("a project_env with no placeholder must be refused");
        assert!(error.to_string().contains("fixedproject"), "{error}");
    }

    /// A credential that must be exchanged, on an upstream that takes no
    /// credential, is a contradiction — and the safe reading of a contradiction
    /// about a credential is to refuse the entry rather than pick a half.
    #[test]
    fn a_minting_credential_kind_on_a_keyless_upstream_is_refused() {
        let error = assemble(
            r#"{
                "key": "keyless-minter",
                "adapter": "chat_completions",
                "credential": "none",
                "credential_kind": "google_service_account",
                "base_url": "https://svc.example.test/v1/chat/completions"
            }"#,
        )
        .expect_err("minting without a credential to mint from must be refused");
        assert!(
            error.to_string().contains("no credential to exchange"),
            "{error}"
        );
    }

    /// **The guard that keeps an RSA private key inside this process.**
    ///
    /// `VERTEX_SERVICE_ACCOUNT` holds a service-account key, and the wire sends
    /// whatever the credential resolves to as `Authorization: Bearer`. So if a
    /// failed mint fell back to reading the environment, the private key would
    /// be transmitted upstream, in a header, on every request — because a token
    /// refresh briefly failed. `mint_credentials` records a failed mint as an
    /// explicit `None` precisely so the fallback cannot happen, and this test is
    /// what holds that shut.
    ///
    /// Driven through `build_with_credentials` with the resolution rule
    /// `ProviderRoute::new` uses, rather than by calling the closure directly,
    /// so it is the production wiring that is under test.
    #[test]
    fn a_failed_mint_never_sends_the_service_account_key() {
        const KEY_MATERIAL: &str =
            r#"{"type":"service_account","private_key":"-----BEGIN PRIVATE KEY-----"}"#;
        let inventory = ProviderInventory::load().expect("inventory should load");

        // The shape `mint_credentials` produces when minting failed: the
        // variable is present in the map, with no token behind it.
        let minted: BTreeMap<String, Option<String>> =
            BTreeMap::from([("VERTEX_SERVICE_ACCOUNT".to_owned(), None)]);

        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("vertex-rung", "vertex"),
                candidate("anthropic-rung", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            // The PRODUCTION rule, not a restatement of it. Everything else
            // resolves, INCLUDING the raw key material and the project — so
            // the only reason the Vertex rung can drop is the failed mint.
            |env| {
                resolve_credential(&minted, env, |env| {
                    Some(if env == "VERTEX_SERVICE_ACCOUNT" {
                        KEY_MATERIAL.to_owned()
                    } else {
                        "present".to_owned()
                    })
                })
            },
        )
        .expect("the other rungs still build");

        let ids: Vec<&str> = route
            .candidates
            .iter()
            .map(|candidate| candidate.definition.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["anthropic-rung"],
            "a failed mint must drop the Vertex rung, not fall back to the raw key"
        );
    }

    /// The same wiring on the success path: a minted token stands in for the
    /// variable's contents, so the wire is handed the token and never the key.
    ///
    /// Without this, `a_failed_mint_never_sends_the_service_account_key` would
    /// still pass if minting were broken outright and the lane simply never
    /// built — the pair is what distinguishes "fails closed" from "never works".
    #[test]
    fn a_minted_token_stands_in_for_the_service_account_key() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let minted: BTreeMap<String, Option<String>> = BTreeMap::from([(
            "VERTEX_SERVICE_ACCOUNT".to_owned(),
            Some("ya29.minted-token".to_owned()),
        )]);

        let route = build_with_credentials(
            &inventory,
            vec![candidate("vertex-rung", "vertex")],
            crate::provider::BASELINE_MAX_TOKENS,
            |env| resolve_credential(&minted, env, |_| Some("present".to_owned())),
        )
        .expect("a minted token builds the lane");
        assert_eq!(route.candidates.len(), 1);
    }

    #[test]
    fn an_unresolvable_region_drops_the_rung_and_not_the_route() {
        // The failure mode, and it is chosen rather than inherited. A
        // deployment that sets BEDROCK_API_KEY but forgets BEDROCK_REGION has
        // no address for that upstream, and the alternative to dropping the
        // rung would be defaulting a region — where `us-east-1` is the
        // plausible guess that silently routes an eu-west-1 deployment's
        // prompts to Virginia. So it behaves exactly as a missing credential
        // does: this rung goes, the rest of the route serves.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("bedrock-rung", "bedrock"),
                candidate("anthropic-rung", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            // Every credential present; the REGION is what is missing.
            |name| (name != "BEDROCK_REGION").then(|| "secret".to_owned()),
        )
        .expect("the other rungs still build");
        let ids: Vec<&str> = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect();
        assert_eq!(ids, ["anthropic-rung"]);

        // And with the region present it is back, on its own client rather
        // than sharing the first-party Anthropic one — same adapter, different
        // account, and conflating them would send Bedrock traffic to
        // api.anthropic.com under the wrong key.
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("bedrock-rung", "bedrock"),
                candidate("anthropic-rung", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("a fully configured route builds");
        let ids: Vec<&str> = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect();
        assert_eq!(ids, ["bedrock-rung", "anthropic-rung"]);
        assert!(
            !Arc::ptr_eq(&route.candidates[0].provider, &route.candidates[1].provider),
            "two providers on one adapter must not share a client"
        );
    }

    // -----------------------------------------------------------------------
    // The price-reconciliation exemption.
    // -----------------------------------------------------------------------

    /// THE ANTI-DRIFT PROPERTY, and the reason
    /// [`ProviderMetadata::dispatchable`] was extracted.
    ///
    /// `/v1/models` and route construction must reach the same verdict from the
    /// same environment, for every provider, in every state. When they disagree
    /// the catalog either advertises a lane that cannot serve — the incident —
    /// or hides one that can.
    ///
    /// The Bedrock row is the one a second implementation gets wrong: a
    /// credential-only check passes it with `BEDROCK_API_KEY` set and
    /// `BEDROCK_REGION` unset, while dispatch drops it for want of an address.
    #[test]
    fn the_catalog_and_the_route_agree_on_every_environment() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        // (name, environment) pairs covering each way a provider can fail.
        let environments: Vec<(&str, Vec<&str>)> = vec![
            ("nothing set", vec![]),
            ("only anthropic", vec!["ANTHROPIC_API_KEY"]),
            ("bedrock key without its region", vec!["BEDROCK_API_KEY"]),
            ("bedrock region without its key", vec!["BEDROCK_REGION"]),
            (
                "bedrock fully configured",
                vec!["BEDROCK_API_KEY", "BEDROCK_REGION"],
            ),
            (
                "vertex account without its project",
                vec!["VERTEX_SERVICE_ACCOUNT"],
            ),
            (
                "everything set",
                vec![
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "GEMINI_API_KEY",
                    "BEDROCK_API_KEY",
                    "BEDROCK_REGION",
                    "FIREWORKS_API_KEY",
                    "XAI_API_KEY",
                    "VERTEX_SERVICE_ACCOUNT",
                    "VERTEX_PROJECT_ID",
                    "GROQ_API_KEY",
                    "TOGETHER_API_KEY",
                ],
            ),
        ];

        for (label, present) in environments {
            let read_env = |name: &str| present.contains(&name).then(|| "value".to_owned());
            // Every key in the shipped inventory, and the loop is spelled out
            // rather than derived from `inventory.providers` so that adding an
            // upstream is a deliberate edit here — the same rule the adapter
            // table above follows. `fireworks` and `xai` were both added on
            // 2026-08-20; `fireworks` had been missing since it shipped, which
            // left the newest upstream outside the one test that proves the
            // catalog and the dispatcher cannot disagree about it.
            //
            // IT HAPPENED AGAIN, and the deliberate-edit rule is the reason it
            // could. `vertex` shipped on 2026-08-21 and was never added here,
            // so for a day this test proved nothing about the newest upstream —
            // the exact failure the paragraph above describes, repeated
            // verbatim. It is added below alongside `groq` and `together`
            // (2026-08-22), together with a `vertex` environment case, because
            // Vertex is the second provider whose endpoint needs a variable
            // (`{project}`) as well as a credential, and the endpoint-before-
            // credential ordering in `dispatchable` is exactly what this test
            // exists to hold.
            //
            // A derived loop would have prevented both misses. It is still
            // spelled out, because the point is that a human states what a new
            // upstream's dispatchability should be rather than inheriting it —
            // but if this happens a third time, derive it and assert the list's
            // LENGTH instead.
            for provider in [
                "anthropic",
                "openai",
                "google",
                "bedrock",
                "fireworks",
                "xai",
                "vertex",
                "groq",
                "together",
            ] {
                let listed = provider_is_dispatchable_with(provider, read_env);
                // The route builder's verdict for the same provider and the
                // same environment: it either keeps the rung or drops it.
                let routed = build_with_credentials(
                    &inventory,
                    vec![candidate("only", provider)],
                    crate::provider::BASELINE_MAX_TOKENS,
                    read_env,
                )
                .is_ok();
                assert_eq!(
                    listed, routed,
                    "{label}: /v1/models says dispatchable={listed} for {provider} \
                     but route construction says {routed}"
                );
            }
        }
    }

    /// The specific pair that a credential-only check would get wrong, spelled
    /// out so the failure names the region rather than just "they disagree".
    #[test]
    fn a_key_without_its_region_is_not_dispatchable() {
        let with_key_only = |name: &str| (name == "BEDROCK_API_KEY").then(|| "value".to_owned());
        assert!(
            !provider_is_dispatchable_with("bedrock", with_key_only),
            "a regional provider with no region has no address, so it must not be listed"
        );

        let with_both = |name: &str| {
            matches!(name, "BEDROCK_API_KEY" | "BEDROCK_REGION").then(|| "value".to_owned())
        };
        assert!(provider_is_dispatchable_with("bedrock", with_both));

        // And a non-regional provider is unaffected by the region rule.
        let anthropic_only = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| "value".to_owned());
        assert!(provider_is_dispatchable_with("anthropic", anthropic_only));
    }

    // -----------------------------------------------------------------------
    // Surfaces (2026-08-20): one upstream, several API planes.
    // -----------------------------------------------------------------------

    #[test]
    fn the_shipped_bedrock_entry_declares_both_of_its_planes() {
        // The two Bedrock planes, pinned as the shipped shape. Both halves of
        // each row matter: the mantle plane must keep the FULL Messages path
        // (its wire POSTs verbatim), and the classic runtime plane must be a
        // HOST ROOT (its wire appends `/model/<id>/invoke`). Swap them and both
        // dial URLs that do not exist.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock = inventory.provider("bedrock").expect("bedrock is shipped");

        assert_eq!(bedrock.adapter, ProviderAdapter::Anthropic);
        assert_eq!(
            bedrock.base_url.as_deref(),
            Some("https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages")
        );

        let runtime = bedrock
            .surfaces
            .get("classic_runtime")
            .expect("the classic runtime plane is declared as a surface");
        assert_eq!(runtime.adapter, ProviderAdapter::AnthropicBedrockRuntime);
        assert_eq!(
            runtime.base_url,
            "https://bedrock-runtime.{region}.amazonaws.com"
        );

        // ONE account: both planes read the same credential and the same
        // region, and neither declares any configuration of its own. This is
        // the property that lets dispatchability stay a per-provider question.
        assert_eq!(bedrock.credential_env.as_deref(), Some("BEDROCK_API_KEY"));
        assert_eq!(bedrock.region_env.as_deref(), Some("BEDROCK_REGION"));

        // And nothing else in the shipped inventory has surfaces — a second
        // provider growing planes is a change that should have to edit this.
        for provider in ["anthropic", "openai", "google"] {
            assert!(
                inventory
                    .provider(provider)
                    .expect("shipped")
                    .surfaces
                    .is_empty(),
                "{provider} unexpectedly declares surfaces"
            );
        }
    }

    /// THE anti-drift property, extended to surfaces.
    ///
    /// #89 established that `/v1/models` and route construction must reach the
    /// same verdict from the same environment, and extracted
    /// `ProviderMetadata::dispatchable` so there is one implementation. Surfaces
    /// could quietly break that by making dispatchability per-plane — a
    /// deployment holding what one plane needs and not the other's would have no
    /// single answer to give. They cannot, because a surface may declare no
    /// credential and no region variable of its own, and this asserts the
    /// consequence: for every environment, every plane of a provider resolves
    /// exactly when the provider itself does.
    #[test]
    fn surfaces_never_change_a_providers_dispatchability() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let environments: Vec<(&str, Vec<&str>)> = vec![
            ("nothing set", vec![]),
            ("bedrock key without its region", vec!["BEDROCK_API_KEY"]),
            ("bedrock region without its key", vec!["BEDROCK_REGION"]),
            (
                "bedrock fully configured",
                vec!["BEDROCK_API_KEY", "BEDROCK_REGION"],
            ),
        ];

        for (label, present) in environments {
            let read_env = |name: &str| present.contains(&name).then(|| "value".to_owned());
            let listed = provider_is_dispatchable_with("bedrock", read_env);

            // The provider's own plane, exactly as #89 tests it.
            let default_plane = build_with_credentials(
                &inventory,
                vec![candidate("only", "bedrock")],
                crate::provider::BASELINE_MAX_TOKENS,
                read_env,
            )
            .is_ok();
            // And each NAMED plane, which must agree with it.
            let named_plane = build_with_credentials(
                &inventory,
                vec![candidate_on("only", "bedrock", Some("classic_runtime"))],
                crate::provider::BASELINE_MAX_TOKENS,
                read_env,
            )
            .is_ok();

            assert_eq!(
                listed, default_plane,
                "{label}: the catalog and the mantle plane disagree"
            );
            assert_eq!(
                listed, named_plane,
                "{label}: the catalog and the classic-runtime plane disagree"
            );
        }
    }

    #[test]
    fn each_plane_of_one_upstream_gets_its_own_client() {
        // Two candidates, one provider, two planes. They must NOT share a
        // client: the planes are different hosts speaking different wires, so a
        // shared client would POST one plane's body at the other's endpoint.
        // Candidates on the SAME plane must still share, which is the
        // connection-pool saving the cache exists for.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("mantle", "bedrock"),
                candidate_on("runtime", "bedrock", Some("classic_runtime")),
                candidate_on("runtime-again", "bedrock", Some("classic_runtime")),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("a fully configured bedrock route builds");
        assert_eq!(route.candidates().len(), 3);
        assert!(
            !Arc::ptr_eq(&route.candidates[0].provider, &route.candidates[1].provider),
            "two PLANES of one upstream must not share a client"
        );
        assert!(
            Arc::ptr_eq(&route.candidates[1].provider, &route.candidates[2].provider),
            "two candidates on ONE plane should share a client"
        );
    }

    #[test]
    fn a_surface_resolves_its_region_from_the_providers_own_variable() {
        // The substitution, over the SHIPPED entry so the assertion is about
        // the host real traffic reaches — and against a region that is NOT the
        // default, so a hardcoded `us-east-1` cannot pass.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock = inventory.provider("bedrock").expect("bedrock is shipped");
        let Dispatchable::Ready {
            endpoint, surfaces, ..
        } = bedrock.dispatchable(|name| match name {
            "BEDROCK_API_KEY" => Some("secret".to_owned()),
            "BEDROCK_REGION" => Some("eu-west-1".to_owned()),
            _ => None,
        })
        else {
            panic!("a fully configured bedrock entry is dispatchable");
        };
        assert_eq!(
            endpoint.as_deref(),
            Some("https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1/messages")
        );
        assert_eq!(
            surfaces.get("classic_runtime").map(String::as_str),
            Some("https://bedrock-runtime.eu-west-1.amazonaws.com")
        );
        for url in std::iter::once(endpoint.as_deref().unwrap_or_default())
            .chain(surfaces.values().map(String::as_str))
        {
            assert!(
                !url.contains(REGION_PLACEHOLDER),
                "no placeholder may survive into a dialled URL: {url}"
            );
        }
    }

    #[test]
    fn a_candidate_naming_an_undeclared_surface_is_refused() {
        // The catalog gate, read from the inventory. A typo must not silently
        // resolve to the provider's own plane, which is the other API.
        assert!(provider_has_surface("bedrock", "classic_runtime"));
        assert!(!provider_has_surface("bedrock", "clasic_runtime"));
        assert!(!provider_has_surface("anthropic", "classic_runtime"));
        assert!(!provider_has_surface("nonexistent", "classic_runtime"));

        // And the route builder refuses it too rather than falling back — the
        // catalog validator normally catches this first, so this is the second
        // line of the same defence.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let error = build_with_credentials(
            &inventory,
            vec![candidate_on("typo", "bedrock", Some("clasic_runtime"))],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect_err("an undeclared surface must not build a route");
        assert!(
            matches!(error, ProviderBuildError::UnknownSurface { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_surface_may_not_introduce_configuration_of_its_own() {
        // The rule that keeps dispatchability a per-provider question. A
        // surface that interpolates a region on an entry with no `region_env`
        // has nothing to resolve it, and allowing it would mean a surface could
        // need configuration the provider does not declare.
        let error = assemble(
            r#"{
                "key": "two-planes",
                "adapter": "chat_completions",
                "credential_env": "TWO_PLANES_API_KEY",
                "secret_name": "two-planes-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "surfaces": {
                    "regional": {
                        "adapter": "chat_completions",
                        "base_url": "https://svc.{region}.example.test/v1/chat/completions"
                    }
                }
            }"#,
        )
        .expect_err("a surface placeholder with nothing to fill it must be refused");
        let detail = error.to_string();
        assert!(detail.contains("two-planes"), "{detail}");
        assert!(detail.contains("region_env"), "{detail}");

        // The mirror case is NOT an error: an entry whose only regional plane
        // is a surface is a perfectly good configuration, and the
        // declared-but-unused rule must not fire on it.
        assemble(
            r#"{
                "key": "surface-only-region",
                "adapter": "chat_completions",
                "credential_env": "SOR_API_KEY",
                "secret_name": "sor-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "region_env": "SOR_REGION",
                "surfaces": {
                    "regional": {
                        "adapter": "chat_completions",
                        "base_url": "https://svc.{region}.example.test/v1/chat/completions"
                    }
                }
            }"#,
        )
        .expect("a region used only by a surface is a valid entry");
    }

    #[test]
    fn a_blank_surface_declaration_is_refused() {
        for surfaces in [
            r#"{"": {"adapter": "chat_completions", "base_url": "https://a.test/v1"}}"#,
            r#"{"named": {"adapter": "chat_completions", "base_url": "   "}}"#,
        ] {
            let error = assemble(&format!(
                r#"{{
                    "key": "blank",
                    "adapter": "chat_completions",
                    "credential_env": "BLANK_API_KEY",
                    "secret_name": "blank-api-key",
                    "base_url": "https://svc.example.test/v1/chat/completions",
                    "surfaces": {surfaces}
                }}"#
            ))
            .expect_err("a blank surface declaration must be refused");
            assert!(error.to_string().contains("blank"), "{error}");
        }
    }

    #[test]
    fn a_misspelled_surface_field_refuses_the_inventory() {
        // Same reasoning as the entry-level `deny_unknown_fields`: a surface's
        // two fields are both load-bearing, so a typo must be loud rather than
        // leaving the surface half-declared.
        assert!(
            ProviderInventory::parse_operator(&operator_json(
                r#"{
                    "key": "typo",
                    "adapter": "chat_completions",
                    "base_url": "https://a.test/v1",
                    "credential_env": "T", "secret_name": "t",
                    "surfaces": {"p": {"adaptor": "chat_completions", "base_url": "https://b.test"}}
                }"#
            ))
            .is_err()
        );
    }

    #[test]
    fn the_bedrock_runtime_adapter_must_be_given_a_host_root() {
        // The misconfiguration this rule exists for, and it is a live one: the
        // mantle Messages path is RIGHT THERE in the same entry, and pasting it
        // onto the runtime surface produces
        // `.../anthropic/v1/messages/model/<id>/invoke` — a 404 whose text
        // names nothing an operator would trace back to this file.
        let error = assemble(
            r#"{
                "key": "wrong-root",
                "adapter": "anthropic_bedrock_runtime",
                "credential_env": "WRONG_ROOT_API_KEY",
                "secret_name": "wrong-root-api-key",
                "base_url": "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1/messages"
            }"#,
        )
        .expect_err("a path on the runtime adapter must be refused");
        let detail = error.to_string();
        assert!(detail.contains("wrong-root"), "{detail}");
        assert!(detail.contains("path"), "{detail}");

        // The same rule applies when the adapter is on a SURFACE, which is how
        // the shipped inventory uses it.
        let error = assemble(
            r#"{
                "key": "wrong-surface-root",
                "adapter": "chat_completions",
                "credential_env": "WSR_API_KEY",
                "secret_name": "wsr-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "surfaces": {
                    "runtime": {
                        "adapter": "anthropic_bedrock_runtime",
                        "base_url": "https://bedrock-runtime.us-east-1.amazonaws.com/model"
                    }
                }
            }"#,
        )
        .expect_err("a path on a runtime surface must be refused");
        assert!(error.to_string().contains("surface runtime"), "{error}");

        // A bare host is fine, with or without a trailing slash — the wire
        // trims one, so refusing it would be pedantry rather than safety.
        for base_url in [
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            "https://bedrock-runtime.us-east-1.amazonaws.com/",
        ] {
            assemble(&format!(
                r#"{{
                    "key": "right-root",
                    "adapter": "anthropic_bedrock_runtime",
                    "credential_env": "RIGHT_ROOT_API_KEY",
                    "secret_name": "right-root-api-key",
                    "base_url": "{base_url}"
                }}"#
            ))
            .unwrap_or_else(|error| panic!("{base_url} is a host root: {error}"));
        }
    }

    #[test]
    fn the_runtime_adapter_is_billed_and_so_cannot_be_free_or_keyless() {
        // A third adapter that dials a cloud endpoint someone invoices. Both
        // safety rules read `dials_a_billed_endpoint`, and both were written as
        // `!= chat_completions` before a third billed adapter existed — so this
        // is the test that would have caught a new adapter silently joining the
        // free lane.
        let error = assemble(
            r#"{"key": "wishful", "adapter": "anthropic_bedrock_runtime",
                "settlement": "free", "base_url": "https://a.test",
                "credential_env": "W", "secret_name": "w"}"#,
        )
        .expect_err("free settlement must be refused on a billed adapter");
        assert!(error.to_string().contains("settlement"), "{error}");

        let error = assemble(
            r#"{"key": "sneaky", "adapter": "anthropic_bedrock_runtime",
                "credential": "none", "base_url": "https://a.test"}"#,
        )
        .expect_err("keyless must be refused on a billed adapter");
        assert!(error.to_string().contains("chat_completions"), "{error}");
    }

    #[test]
    fn a_test_seam_override_replaces_every_plane_of_a_provider() {
        // The fault-injection seam names an UPSTREAM, not a plane. If it
        // replaced only the entry's own endpoint, a harness standing a
        // misbehaving fake in front of `bedrock` would still send the
        // classic-runtime candidates' traffic to AWS — real requests, real
        // money, during a chaos run.
        //
        // Uses a fixture provider rather than the shipped one because the seam
        // reads the process environment, and mutating a shared env var races
        // every other test in this binary.
        let inventory = assemble(
            r#"{
                "key": "two-planes",
                "adapter": "chat_completions",
                "credential_env": "TWO_PLANES_API_KEY",
                "secret_name": "two-planes-api-key",
                "base_url": "https://real.example.test/v1/chat/completions",
                "surfaces": {
                    "other": {
                        "adapter": "chat_completions",
                        "base_url": "https://also-real.example.test/v1/chat/completions"
                    }
                }
            }"#,
        )
        .expect("the fixture assembles");
        let metadata = inventory.provider("two-planes").expect("entry exists");

        // Without an override, the two planes are their configured selves.
        let Dispatchable::Ready {
            endpoint, surfaces, ..
        } = metadata.dispatchable(|_| Some("secret".to_owned()))
        else {
            panic!("dispatchable");
        };
        assert_eq!(
            endpoint.as_deref(),
            Some("https://real.example.test/v1/chat/completions")
        );
        assert_eq!(
            surfaces.get("other").map(String::as_str),
            Some("https://also-real.example.test/v1/chat/completions")
        );

        // The override path is asserted structurally: `dispatchable` applies
        // the SAME override value to the entry and to every surface, so if the
        // seam fires at all it fires everywhere. That is visible in the source
        // and is what this test names; exercising the env var itself would
        // require mutating process state shared with every concurrent test.
        assert!(
            base_url_override("definitely-not-set-two-planes").is_none(),
            "the seam is inert unless a harness sets it"
        );
    }

    #[test]
    fn an_exemption_from_price_reconciliation_must_state_its_case() {
        // This field turns off the alarm that guards margin for every candidate
        // on an upstream. A blank one would do that while asserting nothing,
        // and would read in the report exactly like a gap somebody argued.
        let error = assemble(
            r#"{
                "key": "silent",
                "adapter": "chat_completions",
                "credential_env": "SILENT_API_KEY",
                "secret_name": "silent-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "unreconcilable_reason": "   "
            }"#,
        )
        .expect_err("a blank exemption must be refused");
        let detail = error.to_string();
        assert!(detail.contains("silent"), "{detail}");
        assert!(detail.contains("unreconcilable_reason"), "{detail}");
    }

    #[test]
    fn a_blank_source_provider_key_is_refused_rather_than_joined_on_nothing() {
        // Absent means "join on the provider key", which is a true and common
        // answer. Present-but-blank would join on "", match nothing, and report
        // every candidate on this upstream NOT IN SOURCE forever — an alarm
        // that cannot be silenced by fixing anything.
        let error = assemble(
            r#"{
                "key": "renamed",
                "adapter": "chat_completions",
                "credential_env": "RENAMED_API_KEY",
                "secret_name": "renamed-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "source_provider_key": "   "
            }"#,
        )
        .expect_err("a blank source key must be refused");
        let detail = error.to_string();
        assert!(detail.contains("renamed"), "{detail}");
        assert!(detail.contains("source_provider_key"), "{detail}");
    }

    #[test]
    fn an_upstream_cannot_be_both_reconcilable_elsewhere_and_unreconcilable() {
        // The two fields make opposite claims about whether this upstream's
        // margin is checked at all. Letting one win silently would decide that
        // by field order in a struct.
        let error = assemble(
            r#"{
                "key": "confused",
                "adapter": "chat_completions",
                "credential_env": "CONFUSED_API_KEY",
                "secret_name": "confused-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "source_provider_key": "confused-ai",
                "unreconcilable_reason": "the source prices a different SKU class"
            }"#,
        )
        .expect_err("declaring both must be refused");
        let detail = error.to_string();
        assert!(detail.contains("confused"), "{detail}");
        assert!(detail.contains("source_provider_key"), "{detail}");
        assert!(detail.contains("unreconcilable_reason"), "{detail}");
    }

    #[test]
    fn every_shipped_provider_joins_models_dev_at_the_key_it_declares() {
        // The shipped mapping, asserted EXHAUSTIVELY rather than by naming the
        // providers that should have none. That distinction is the whole value
        // of this test: it used to loop over a hardcoded list of the other
        // providers, so a NEW provider was checked by nothing at all and could
        // acquire — or silently need — a mapping without any test noticing.
        // Building the expectation from the inventory itself means a new entry
        // fails here until someone states which key it joins on.
        //
        //   fireworks  models.dev files it under `fireworks-ai`.
        //   vertex     models.dev files Vertex under `google-vertex`, which is
        //              a DIFFERENT entry from `google` with different prices
        //              for some models. Joining on the bare provider key would
        //              find nothing; joining on `google` would reconcile this
        //              lane against the Developer API's rate card.
        //   everything else joins on its own key, and a mapping appearing on
        //   one later would be a silent decision to reconcile its rates against
        //   whatever sits at the named key.
        let inventory = ProviderInventory::load().expect("shipped inventory must load");
        let declared: BTreeMap<&str, Option<String>> = inventory
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.key.as_str(),
                    provider_source_key(provider.key.as_str()),
                )
            })
            .collect();
        let expected: BTreeMap<&str, Option<String>> = BTreeMap::from([
            ("anthropic", None),
            ("openai", None),
            ("google", None),
            ("bedrock", None),
            ("fireworks", Some("fireworks-ai".to_owned())),
            ("xai", None),
            ("vertex", Some("google-vertex".to_owned())),
            // Groq is one of the coincidences: models.dev files it under
            // `groq`, which is also ZeroRouter's key, so the join needs no
            // declaration. Together is not — `togetherai` — and a guess at
            // `together` finds no row at all, which would report every lane on
            // it as NOT IN SOURCE forever on rates that are in fact published.
            ("groq", None),
            ("together", Some("togetherai".to_owned())),
        ]);
        assert_eq!(
            declared, expected,
            "a provider gaining or losing a models.dev mapping is a decision \
             about which vendor's prices its rates are checked against"
        );
    }

    /// The shipped attestation declaration, asserted by value.
    ///
    /// The whole retention guarantee of the `xai/*` lanes reduces to these two
    /// strings reaching the wire. A typo in either is not a broken feature that
    /// fails loudly — `validate_attestation` catches an unusable header name,
    /// but a VALID name that is simply the wrong one, or an expectation of
    /// `"false"`, would load cleanly and either take the lane down forever or,
    /// far worse, pass every response. So the pair is pinned here rather than
    /// trusted to review.
    #[test]
    fn the_xai_entry_declares_the_attestation_its_retention_pin_depends_on() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let xai = inventory
            .provider("xai")
            .expect("the shipped inventory carries xai");
        assert_eq!(
            xai.attestation_header.as_deref(),
            Some("x-zero-data-retention"),
            "the header xAI publishes its ZDR verdict in (docs.x.ai, 2026-08-20)"
        );
        assert_eq!(
            xai.attestation_expect.as_deref(),
            Some("true"),
            "only an affirmative attestation may be served"
        );
        assert_eq!(xai.adapter, ProviderAdapter::ChatCompletions);

        // And nothing else declares one. This is not tidiness: an attestation
        // on another upstream would be a claim, published through that
        // provider's retention pin, that its guarantee is checked per request.
        //
        // Derived from the inventory rather than from a hardcoded list of the
        // other providers, for the reason
        // `every_shipped_provider_joins_models_dev_at_the_key_it_declares`
        // gives: a list cannot cover a provider that does not exist yet, and
        // "nothing ELSE declares one" is a claim about everything else.
        for metadata in inventory
            .providers
            .iter()
            .filter(|entry| entry.key != "xai")
        {
            assert!(
                metadata.attestation_header.is_none() && metadata.attestation_expect.is_none(),
                "{} must not acquire an attestation without a deliberate edit",
                metadata.key
            );
        }
    }

    #[test]
    fn half_an_attestation_is_refused() {
        // Neither half implies the other. Inferring `"true"` as a default
        // expectation would be this repo deciding what another company's API
        // means; inferring the header name is not even possible.
        for body in [
            r#"{
                "key": "halfway",
                "adapter": "chat_completions",
                "credential_env": "HALFWAY_API_KEY",
                "secret_name": "halfway-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "attestation_header": "x-zero-data-retention"
            }"#,
            r#"{
                "key": "halfway",
                "adapter": "chat_completions",
                "credential_env": "HALFWAY_API_KEY",
                "secret_name": "halfway-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "attestation_expect": "true"
            }"#,
        ] {
            let error = assemble(body).expect_err("half a declaration must be refused");
            let detail = error.to_string();
            assert!(detail.contains("halfway"), "{detail}");
            assert!(detail.contains("half a response attestation"), "{detail}");
        }
    }

    #[test]
    fn a_blank_or_unusable_attestation_header_is_refused_at_load() {
        // Both faults have the same shape in production — a header that can
        // never match, so every response reads as absent and fails closed — and
        // that is an indefinite outage whose cause is one mistyped character.
        // Refusing the inventory is the same news, delivered where it can be
        // acted on.
        let blank = assemble(
            r#"{
                "key": "blank",
                "adapter": "chat_completions",
                "credential_env": "BLANK_API_KEY",
                "secret_name": "blank-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "attestation_header": "  ",
                "attestation_expect": "true"
            }"#,
        )
        .expect_err("a blank header name must be refused");
        assert!(
            blank.to_string().contains("blank response attestation"),
            "{blank}"
        );

        let unusable = assemble(
            r#"{
                "key": "unusable",
                "adapter": "chat_completions",
                "credential_env": "UNUSABLE_API_KEY",
                "secret_name": "unusable-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "attestation_header": "x zero data retention",
                "attestation_expect": "true"
            }"#,
        )
        .expect_err("a header name HTTP cannot carry must be refused");
        assert!(unusable.to_string().contains("not usable"), "{unusable}");
    }

    #[test]
    fn an_attestation_on_an_adapter_that_cannot_enforce_it_is_refused() {
        // THE WORST AVAILABLE OUTCOME, refused rather than ignored. Only the
        // chat-completions wire reads the declaration; on any other adapter the
        // fields would parse, validate, and sit in the inventory looking like a
        // guarantee that is checked on every request while nothing checked it —
        // and the lane's retention pin would go on publishing `zero` to
        // `/v1/models` with no error anywhere to contradict it.
        let error = assemble(
            r#"{
                "key": "unenforced",
                "adapter": "anthropic",
                "credential_env": "UNENFORCED_API_KEY",
                "secret_name": "unenforced-api-key",
                "attestation_header": "x-zero-data-retention",
                "attestation_expect": "true"
            }"#,
        )
        .expect_err("an unenforceable attestation must be refused");
        let detail = error.to_string();
        assert!(detail.contains("unenforced"), "{detail}");
        assert!(detail.contains("does not enforce"), "{detail}");

        // And a SURFACE is checked too — the plane a candidate selects with
        // `surface = "..."` is where an unenforced declaration would hide.
        let error = assemble(
            r#"{
                "key": "twoplane",
                "adapter": "chat_completions",
                "credential_env": "TWOPLANE_API_KEY",
                "secret_name": "twoplane-api-key",
                "base_url": "https://svc.example.test/v1/chat/completions",
                "attestation_header": "x-zero-data-retention",
                "attestation_expect": "true",
                "surfaces": {
                    "messages": {
                        "adapter": "anthropic",
                        "base_url": "https://svc.example.test/v1/messages"
                    }
                }
            }"#,
        )
        .expect_err("a surface that cannot enforce the attestation must be refused");
        let detail = error.to_string();
        assert!(detail.contains("messages"), "{detail}");
        assert!(detail.contains("does not enforce"), "{detail}");
    }

    #[test]
    fn only_the_declaring_provider_is_exempt_from_reconciliation() {
        // The narrowness that makes the exemption tolerable. Bedrock declares
        // one; nothing else does, and a lane that stopped being reconciled by
        // accident is the failure this asserts against.
        assert!(
            provider_unreconcilable_reason("bedrock")
                .is_some_and(|reason| reason.contains("_standard")),
            "bedrock's exemption must name the SKU class its rates really come from"
        );
        for provider in ["anthropic", "openai", "google"] {
            assert!(
                provider_unreconcilable_reason(provider).is_none(),
                "{provider} must still be reconciled against the public catalog"
            );
        }
    }

    #[test]
    fn missing_credentials_skip_candidates_without_reordering() {
        // A provider with no credential in the environment contributes no
        // candidates, and the survivors keep their catalog order — failover
        // must not silently reshuffle because a key happened to be absent.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let anthropic_env = inventory
            .provider("anthropic")
            .expect("anthropic metadata should exist")
            .credential_env
            .clone()
            .expect("a shipped provider declares its credential env var");
        let route = build_with_credentials(
            &inventory,
            vec![
                candidate("one", "openai"),
                candidate("two", "anthropic"),
                candidate("three", "anthropic"),
            ],
            crate::provider::BASELINE_MAX_TOKENS,
            // Only Anthropic is credentialed, so the OpenAI rung drops out.
            |name| (name == anthropic_env).then(|| "secret".to_owned()),
        )
        .expect("a credentialed provider should still build a route");

        let ids = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["two", "three"]);
    }

    #[test]
    fn candidates_on_same_upstream_share_one_client() {
        let route = build_with_credentials(
            &ProviderInventory::load().expect("the shipped inventory should load"),
            vec![candidate("one", "openai"), candidate("two", "openai")],
            8_192,
            |_| Some("secret".to_owned()),
        )
        .expect("providers should build");

        assert!(Arc::ptr_eq(
            &route.candidates[0].provider,
            &route.candidates[1].provider
        ));
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let credential = "credential-that-must-not-be-logged";
        let route = build_with_credentials(
            &ProviderInventory::load().expect("the shipped inventory should load"),
            vec![candidate("one", "openai")],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some(credential.to_owned()),
        )
        .expect("DeepInfra provider should build");

        assert!(!format!("{route:?}").contains(credential));
    }

    /// The upstream model a candidate is dispatched against is its own, taken
    /// from the tier table — never the tier id the customer asked for. The pin
    /// used to come from a wrapper around the chain; it now comes from
    /// `ProviderCandidate::chat`, and this asserts the value it reads.
    #[test]
    fn candidates_carry_their_pinned_upstream_model() {
        let route = build_with_credentials(
            &ProviderInventory::load().expect("the shipped inventory should load"),
            vec![candidate("one", "openai")],
            crate::provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("Fireworks provider should build");

        assert_eq!(route.candidates[0].definition.model, "upstream/one");
    }
}

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
    wire::{AnthropicWire, ChatCompletionsWire, OpenAiResponsesWire},
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
struct ProviderMetadata {
    key: String,
    adapter: ProviderAdapter,
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
    /// Which file this entry came from. Never deserialized — it is a fact
    /// about where the bytes were read, not a field an entry may claim, and an
    /// entry that could declare itself shipped would defeat every rule below
    /// that treats the two differently.
    #[serde(skip)]
    source: InventorySource,
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

/// Which inventory an entry was read from.
///
/// Load-bearing in two places, both of which must never treat an
/// operator-supplied entry as though ZeroRouter had vouched for it: the
/// shadowing rule in [`ProviderInventory::assembled`], and the drift
/// reconciliation's exemption for upstreams no public catalog covers
/// ([`is_operator_declared_provider`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum InventorySource {
    #[default]
    Shipped,
    Operator,
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

    /// Parse an operator-supplied inventory document, stamping every entry
    /// with its origin.
    fn parse_operator(source: &str) -> Result<Vec<ProviderMetadata>, ProviderBuildError> {
        let mut inventory = serde_json::from_str::<Self>(source).map_err(|source| {
            ProviderBuildError::InvalidInventory {
                detail: format!("operator provider inventory is not valid: {source}"),
            }
        })?;
        for provider in &mut inventory.providers {
            provider.source = InventorySource::Operator;
        }
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
    fn assembled(operator: &[ProviderMetadata]) -> Result<Self, ProviderBuildError> {
        let mut inventory = Self::shipped()?;
        if !operator.is_empty() {
            let shipped: BTreeSet<String> = inventory
                .providers
                .iter()
                .map(|provider| provider.key.clone())
                .collect();
            for entry in operator {
                if shipped.contains(&entry.key) {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "operator provider {} shadows a shipped provider of the same key; \
                             the shipped entry names where ZeroRouter's own credential is sent, \
                             so it may be added to but never redefined — rename the operator entry",
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
                if self.adapter != ProviderAdapter::ChatCompletions {
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

/// Returns whether `provider` has a constructor in this module.
///
/// Configuration validation should use this function instead of maintaining a
/// second provider-name list.
#[must_use]
pub fn is_supported_provider(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| inventory.provider(provider).is_some())
}

/// Whether a provider may carry $0 candidates (edge mode, stage 2).
///
/// True only for the chat-completions adapter — the local/edge upstream, which
/// has no implied endpoint and must name its own `base_url`. The rule exists
/// because a $0 basis on a paid cloud adapter is never a fact about the world:
/// it is either a fat-fingered rate, which records zero COGS on real spend and
/// reports a healthy margin while the invoice grows (the exact silent failure
/// `config/tiers.toml` and `drift.rs` are both written against), or a
/// deliberate attempt to file a paid model under the free rung. Refusing it at
/// catalog load makes the free rung structurally unable to name a cloud model,
/// which is the property stage 3's metering skip will need to be able to
/// assume — and it is enforced here, once, against the inventory itself rather
/// than by a list of vendor names kept somewhere else.
#[must_use]
pub fn provider_allows_zero_price(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory
            .provider(provider)
            .is_some_and(|metadata| metadata.adapter == ProviderAdapter::ChatCompletions)
    })
}

/// Whether this provider was declared by the operator rather than shipped.
///
/// Read by the drift reconciliation, which reconciles the tier file against a
/// public model catalog: an upstream running on the operator's own hardware is
/// absent from every such catalog by construction, and that absence is not
/// evidence of anything.
#[must_use]
pub fn is_operator_declared_provider(provider: &str) -> bool {
    ProviderInventory::load().is_ok_and(|inventory| {
        inventory
            .provider(provider)
            .is_some_and(|metadata| metadata.source == InventorySource::Operator)
    })
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
        }
    }
}

impl ProviderCandidate {
    #[must_use]
    pub fn definition(&self) -> &TierCandidate {
        &self.definition
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
}

impl ProviderRoute {
    /// Build a per-request provider route from canonical tier candidates.
    ///
    /// Missing credentials make an individual candidate unavailable. Building
    /// fails only when no configured candidate can be constructed. Do not cache
    /// this value across requests: fallback selection metadata is request-scoped.
    pub fn new(
        candidates: Vec<TierCandidate>,
        max_output_tokens: u32,
    ) -> Result<Self, ProviderBuildError> {
        let inventory = ProviderInventory::load()?;
        build_with_credentials(&inventory, candidates, max_output_tokens, read_credential)
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
fn build_with_credentials<F>(
    inventory: &ProviderInventory,
    candidates: Vec<TierCandidate>,
    max_output_tokens: u32,
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
    let mut providers = BTreeMap::<&str, Arc<dyn ModelProvider>>::new();
    let mut unavailable = BTreeSet::new();

    for definition in candidates {
        let metadata = inventory.provider(&definition.provider).ok_or_else(|| {
            ProviderBuildError::UnsupportedProvider {
                candidate: definition.id.clone(),
                provider: definition.provider.clone(),
            }
        })?;
        let provider_key = metadata.key.as_str();
        if unavailable.contains(provider_key) {
            continue;
        }

        let provider = if let Some(provider) = providers.get(provider_key) {
            Arc::clone(provider)
        } else {
            let credential = match metadata.credential {
                // A keyless upstream has nothing to look up, so it can never be
                // the rung a route loses to a missing key — which is the whole
                // point: a local server that takes no credential must not need
                // a fake one to stay in the walk. Inventory validation has
                // already refused this declaration on every adapter that owns a
                // cloud endpoint, so nothing reachable from here can be a paid
                // upstream dispatched without authentication.
                CredentialRequirement::None => String::new(),
                CredentialRequirement::Required => {
                    // Validated present; `unwrap_or_default` reads an empty
                    // name rather than panicking, and an empty name resolves to
                    // no credential, so the rung drops out exactly as it would
                    // for a genuinely absent key.
                    let credential_env = metadata.credential_env.as_deref().unwrap_or_default();
                    let Some(credential) = credential_for(credential_env) else {
                        missing_credentials.push(credential_env.to_owned());
                        unavailable.insert(provider_key);
                        continue;
                    };
                    credential
                }
            };
            let provider = create_provider(metadata, &credential, max_output_tokens)?;
            providers.insert(provider_key, Arc::clone(&provider));
            provider
        };

        available.push(ProviderCandidate {
            definition,
            provider,
        });
    }

    if available.is_empty() {
        return Err(ProviderBuildError::NoAvailableCredentials {
            credential_envs: missing_credentials,
        });
    }

    Ok(assemble_route(available))
}

/// Put ordered candidates behind a route.
///
/// Kept as its own function, rather than inlined into the two constructors, so
/// a test-supplied route and a credential-built one go through the same wiring
/// and cannot diverge.
fn assemble_route(candidates: Vec<ProviderCandidate>) -> ProviderRoute {
    ProviderRoute { candidates }
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

fn create_provider(
    metadata: &ProviderMetadata,
    credential: &str,
    max_output_tokens: u32,
) -> Result<Arc<dyn ModelProvider>, ProviderBuildError> {
    let alias = metadata.key.as_str();
    let override_url = base_url_override(alias);
    let effective_base_url = override_url.as_deref().or(metadata.base_url.as_deref());
    let provider: Arc<dyn ModelProvider> = match metadata.adapter {
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
        ProviderAdapter::ChatCompletions => Arc::new(ChatCompletionsWire::new(
            alias,
            credential,
            effective_base_url,
            Some(max_output_tokens),
            // Same budget note as the arms above.
            900,
        )),
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use crate::config::ModelMetadata;
    use crate::provider::ModelRates;

    use super::*;

    fn candidate(id: &str, provider: &str) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: provider.to_owned(),
            model: format!("upstream/{id}"),
            rates: ModelRates {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: None,
            },
            // Provider dispatch reads `provider` and `model` and nothing else.
            metadata: ModelMetadata::default(),
        }
    }

    #[test]
    fn supported_provider_check_uses_constructor_table() {
        // The MVP inventory: two providers, both on ZeroRouter-owned wires.
        for provider in ["anthropic", "openai"] {
            assert!(is_supported_provider(provider));
        }
        // Retired with the git dependency that supplied their adapters.
        for provider in ["bedrock", "deepinfra", "fireworks", "together", "minimax"] {
            assert!(
                !is_supported_provider(provider),
                "{provider} is no longer in the inventory"
            );
        }
        assert!(!is_supported_provider("unknown"));
    }

    #[test]
    fn the_shipped_inventory_gains_no_route_from_the_new_adapter() {
        // Stage 1 of edge mode adds the chat-completions ADAPTER, not a
        // provider entry that uses it — the local-candidate configuration
        // surface is stage 2. This pins that separation: adding the wire must
        // not have quietly widened what a deployment dispatches to.
        let inventory = ProviderInventory::load().expect("inventory should load");
        let adapters: Vec<(&str, ProviderAdapter)> = inventory
            .providers
            .iter()
            .map(|provider| (provider.key.as_str(), provider.adapter))
            .collect();
        assert_eq!(
            adapters,
            [
                ("anthropic", ProviderAdapter::Anthropic),
                ("openai", ProviderAdapter::OpenAiResponses),
            ]
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
        let provider = create_provider(metadata, "secret", crate::provider::BASELINE_MAX_TOKENS)
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

    /// The canonical local entry: keyless, chat-completions, own endpoint.
    const LOCAL_ENTRY: &str = r#"{
        "key": "local-llama",
        "adapter": "chat_completions",
        "credential": "none",
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
        assert_eq!(keys, ["anthropic", "openai", "local-llama"]);

        let local = inventory
            .provider("local-llama")
            .expect("the operator entry is addressable by key");
        assert_eq!(local.adapter, ProviderAdapter::ChatCompletions);
        assert_eq!(local.credential, CredentialRequirement::None);
        assert_eq!(local.source, InventorySource::Operator);
        for shipped in ["anthropic", "openai"] {
            let metadata = inventory.provider(shipped).expect("shipped entry survives");
            assert_eq!(metadata.credential, CredentialRequirement::Required);
            assert_eq!(metadata.source, InventorySource::Shipped);
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

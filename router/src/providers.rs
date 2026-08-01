use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt,
    sync::Arc,
};

use futures_util::stream::BoxStream;
use serde::Deserialize;
use thiserror::Error;
use zeroclaw_providers::{
    ProviderDispatch,
    anthropic::AnthropicModelProvider,
    bedrock::BedrockModelProvider,
    compatible::{AuthStyle, OpenAiCompatibleModelProvider},
    traits::{ChatRequest, ChatResponse, ModelProvider, StreamEvent, StreamOptions, StreamResult},
};

use crate::config::TierCandidate;

const PROVIDER_INVENTORY_JSON: &str = include_str!("../config/providers.json");

#[derive(Debug, Deserialize)]
struct ProviderInventory {
    providers: Vec<ProviderMetadata>,
}

#[derive(Debug, Deserialize)]
struct ProviderMetadata {
    key: String,
    adapter: ProviderAdapter,
    credential_env: String,
    secret_name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum ProviderAdapter {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "bedrock")]
    Bedrock,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

impl ProviderInventory {
    fn load() -> Result<Self, ProviderBuildError> {
        let inventory =
            serde_json::from_str::<Self>(PROVIDER_INVENTORY_JSON).map_err(|source| {
                ProviderBuildError::InvalidInventory {
                    detail: source.to_string(),
                }
            })?;
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
                || provider.credential_env.trim().is_empty()
                || provider.secret_name.trim().is_empty()
                || provider
                    .display_name
                    .as_deref()
                    .is_some_and(|name| name.trim().is_empty())
            {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: "provider metadata contains an empty required value".to_owned(),
                });
            }
            if !keys.insert(provider.key.as_str()) {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!("duplicate provider key {}", provider.key),
                });
            }
            match provider.adapter {
                ProviderAdapter::OpenAiCompatible
                    if provider
                        .base_url
                        .as_deref()
                        .is_none_or(|url| url.trim().is_empty()) =>
                {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: format!(
                            "OpenAI-compatible provider {} requires base_url",
                            provider.key
                        ),
                    });
                }
                ProviderAdapter::Bedrock if provider.base_url.is_some() => {
                    return Err(ProviderBuildError::InvalidInventory {
                        detail: "Bedrock does not support a base_url override".to_owned(),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn provider(&self, key: &str) -> Option<&ProviderMetadata> {
        self.providers.iter().find(|provider| provider.key == key)
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

#[derive(Debug, Error)]
pub enum ProviderBuildError {
    #[error("embedded provider inventory is invalid: {detail}")]
    InvalidInventory { detail: String },
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
        let provider: Arc<dyn ModelProvider> = Arc::new(
            OpenAiCompatibleModelProvider::builder("test-upstream")
                .display_name("test upstream")
                .base_url(base_url)
                .auth_style(AuthStyle::Bearer)
                .credential(Some("test-credential"))
                .max_tokens(Some(zeroclaw_api::model_provider::BASELINE_MAX_TOKENS))
                .build(),
        );
        Self {
            definition,
            provider,
        }
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
        ProviderDispatch::new(Arc::clone(&self.provider))
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
        ProviderDispatch::new(Arc::clone(&self.provider)).stream_chat(
            request,
            &self.definition.model,
            temperature,
            options,
        )
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
        build_with_credentials(candidates, max_output_tokens, read_credential)
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

fn build_with_credentials<F>(
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

    let inventory = ProviderInventory::load()?;
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

        let credential_env = metadata.credential_env.as_str();
        let provider = if let Some(provider) = providers.get(provider_key) {
            Arc::clone(provider)
        } else {
            let Some(credential) = credential_for(credential_env) else {
                missing_credentials.push(metadata.credential_env.clone());
                unavailable.insert(provider_key);
                continue;
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

fn create_provider(
    metadata: &ProviderMetadata,
    credential: &str,
    max_output_tokens: u32,
) -> Result<Arc<dyn ModelProvider>, ProviderBuildError> {
    let alias = metadata.key.as_str();
    let provider: Arc<dyn ModelProvider> = match metadata.adapter {
        ProviderAdapter::Anthropic => {
            let mut builder = AnthropicModelProvider::builder(alias)
                .credential(Some(credential))
                .max_tokens(max_output_tokens)
                // The pinned adapter hardcoded ~120s; honor ZR's 15-minute
                // UPSTREAM_REQUEST_TIMEOUT budget (api.rs) instead.
                .timeout_secs(900);
            if let Some(base_url) = metadata.base_url.as_deref() {
                builder = builder.base_url(base_url);
            }
            Arc::new(builder.build())
        }
        ProviderAdapter::Bedrock => {
            // `.bearer_token` pins the credential and skips every ambient AWS probe.
            Arc::new(
                BedrockModelProvider::builder(alias)
                    .bearer_token(credential)
                    .max_tokens(max_output_tokens)
                    .build(),
            )
        }
        ProviderAdapter::OpenAiCompatible => {
            let Some(base_url) = metadata.base_url.as_deref() else {
                return Err(ProviderBuildError::InvalidInventory {
                    detail: format!(
                        "OpenAI-compatible provider {} requires base_url",
                        metadata.key
                    ),
                });
            };
            let name = metadata.display_name.as_deref().unwrap_or(alias);
            // `build()` panics unless display_name, base_url, and auth_style
            // are all set — set all three explicitly.
            Arc::new(
                OpenAiCompatibleModelProvider::builder(alias)
                    .display_name(name)
                    .base_url(base_url)
                    .auth_style(AuthStyle::Bearer)
                    .credential(Some(credential))
                    .max_tokens(Some(max_output_tokens))
                    .build(),
            )
        }
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use zeroclaw_providers::pricing::ModelRates;

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
        }
    }

    #[test]
    fn supported_provider_check_uses_constructor_table() {
        for provider in ["anthropic", "bedrock", "deepinfra", "fireworks", "together"] {
            assert!(is_supported_provider(provider));
        }
        assert!(!is_supported_provider("unknown"));
    }

    #[test]
    fn missing_credentials_skip_candidates_without_reordering() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock_env = inventory
            .provider("bedrock")
            .expect("Bedrock metadata should exist")
            .credential_env
            .clone();
        let together_env = inventory
            .provider("together")
            .expect("Together metadata should exist")
            .credential_env
            .clone();
        let route = build_with_credentials(
            vec![
                candidate("one", "anthropic"),
                candidate("two", "bedrock"),
                candidate("three", "together"),
            ],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
            |name| (name == bedrock_env || name == together_env).then(|| "secret".to_owned()),
        )
        .expect("two providers should be available");

        let ids = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["two", "three"]);
    }

    #[test]
    fn bedrock_only_keeps_bedrock_candidates_in_mixed_route() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock_env = inventory
            .provider("bedrock")
            .expect("Bedrock metadata should exist")
            .credential_env
            .clone();
        let route = build_with_credentials(
            vec![
                candidate("anthropic-primary", "anthropic"),
                candidate("bedrock-sonnet", "bedrock"),
                candidate("anthropic-secondary", "anthropic"),
                candidate("bedrock-opus", "bedrock"),
            ],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
            |name| (name == bedrock_env).then(|| "secret".to_owned()),
        )
        .expect("Bedrock credential should keep both Bedrock candidates");

        let ids = route
            .candidates()
            .iter()
            .map(|candidate| candidate.definition().id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["bedrock-sonnet", "bedrock-opus"]);
    }

    #[test]
    fn candidates_on_same_upstream_share_one_client() {
        let route = build_with_credentials(
            vec![candidate("one", "fireworks"), candidate("two", "fireworks")],
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
    fn bedrock_requires_its_bearer_credential() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let bedrock_env = inventory
            .provider("bedrock")
            .expect("Bedrock metadata should exist")
            .credential_env
            .clone();
        let error = build_with_credentials(
            vec![candidate("one", "bedrock")],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
            |_| None,
        )
        .expect_err("missing Bedrock bearer token should make the route unavailable");

        match error {
            ProviderBuildError::NoAvailableCredentials { credential_envs } => {
                assert_eq!(credential_envs, [bedrock_env]);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn together_uses_current_api_endpoint() {
        let inventory = ProviderInventory::load().expect("inventory should load");
        let together = inventory
            .provider("together")
            .expect("Together metadata should exist");
        assert_eq!(together.adapter, ProviderAdapter::OpenAiCompatible);
        assert_eq!(
            together.base_url.as_deref(),
            Some("https://api.together.ai/v1")
        );
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let credential = "credential-that-must-not-be-logged";
        let route = build_with_credentials(
            vec![candidate("one", "deepinfra")],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
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
            vec![candidate("one", "fireworks")],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("Fireworks provider should build");

        assert_eq!(route.candidates[0].definition.model, "upstream/one");
    }
}

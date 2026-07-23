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
    model_pin::ModelPinnedProvider,
    reliable::{
        ProviderFallbackInfo, ReliableModelProvider, scope_provider_fallback,
        take_last_provider_fallback,
    },
    traits::{ChatRequest, ChatResponse, ModelProvider, StreamEvent, StreamOptions, StreamResult},
};

use crate::config::TierCandidate;

const PROVIDER_INVENTORY_JSON: &str = include_str!("../config/providers.json");
const PROVIDER_RETRIES: u32 = 2;
const PROVIDER_BACKOFF_MS: u64 = 500;
const ROUTE_ALIAS: &str = "zerorouter";

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
/// through [`ProviderDispatch`]. Streaming callers can try these entries in
/// order without going through `ReliableModelProvider`, whose stream method
/// cannot fail over after polling has begun.
pub struct ProviderCandidate {
    definition: TierCandidate,
    provider: Arc<dyn ModelProvider>,
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
/// credential is unavailable have been removed. `non_streaming` wraps the same
/// clients with model pins and ZeroClaw's retry/fallback implementation.
pub struct ProviderRoute {
    candidates: Vec<ProviderCandidate>,
    non_streaming: Arc<dyn ModelProvider>,
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

    #[must_use]
    pub fn candidates(&self) -> &[ProviderCandidate] {
        &self.candidates
    }

    #[must_use]
    pub fn into_candidates(self) -> Vec<ProviderCandidate> {
        self.candidates
    }

    /// Run a non-streaming request with retry/fallback and report the candidate
    /// that actually served it.
    pub async fn chat<'route>(
        &'route self,
        request: ChatRequest<'_>,
        requested_model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<SelectedChatResponse<'route>> {
        let (response, fallback) = scope_provider_fallback(async {
            let response = ProviderDispatch::new(Arc::clone(&self.non_streaming))
                .chat(request, requested_model, temperature)
                .await;
            let fallback = take_last_provider_fallback();
            (response, fallback)
        })
        .await;

        let response = response?;
        let candidate = self.selected_candidate(fallback.as_ref())?;
        Ok(SelectedChatResponse {
            response,
            candidate: candidate.definition(),
        })
    }

    fn selected_candidate(
        &self,
        fallback: Option<&ProviderFallbackInfo>,
    ) -> anyhow::Result<&ProviderCandidate> {
        let Some(fallback) = fallback else {
            return self
                .candidates
                .first()
                .ok_or_else(|| anyhow::anyhow!("provider route unexpectedly has no candidates"));
        };

        self.candidates
            .iter()
            .find(|candidate| candidate.definition.id == fallback.actual_provider)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider fallback selected unknown candidate {}",
                    fallback.actual_provider
                )
            })
    }
}

/// A non-streaming response paired with the canonical candidate that served it.
pub struct SelectedChatResponse<'route> {
    pub response: ChatResponse,
    pub candidate: &'route TierCandidate,
}

impl fmt::Debug for SelectedChatResponse<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SelectedChatResponse")
            .field("candidate", &self.candidate)
            .finish_non_exhaustive()
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

    let fallback_chain = available
        .iter()
        .map(|candidate| {
            let pinned = ModelPinnedProvider::new(
                &candidate.definition.id,
                &candidate.definition.model,
                Box::new(Arc::clone(&candidate.provider)),
            );
            (
                candidate.definition.id.clone(),
                Box::new(pinned) as Box<dyn ModelProvider>,
            )
        })
        .collect();
    let non_streaming: Arc<dyn ModelProvider> = Arc::new(ReliableModelProvider::new(
        ROUTE_ALIAS,
        fallback_chain,
        PROVIDER_RETRIES,
        PROVIDER_BACKOFF_MS,
    ));

    Ok(ProviderRoute {
        candidates: available,
        non_streaming,
    })
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
            let provider = AnthropicModelProvider::with_base_url(
                alias,
                Some(credential),
                metadata.base_url.as_deref(),
            );
            Arc::new(provider.with_max_tokens(max_output_tokens))
        }
        ProviderAdapter::Bedrock => {
            let provider = BedrockModelProvider::with_bearer_token(alias, credential);
            Arc::new(provider.with_max_tokens(max_output_tokens))
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
            Arc::new(
                OpenAiCompatibleModelProvider::new(
                    alias,
                    name,
                    base_url,
                    Some(credential),
                    AuthStyle::Bearer,
                )
                .with_max_tokens(Some(max_output_tokens)),
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

    #[test]
    fn selected_response_debug_omits_completion_body() {
        let definition = candidate("one", "deepinfra");
        let selected = SelectedChatResponse {
            response: ChatResponse {
                text: Some("private completion".to_owned()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            },
            candidate: &definition,
        };

        assert!(!format!("{selected:?}").contains("private completion"));
    }

    #[test]
    fn reliable_chain_is_model_pinned() {
        let route = build_with_credentials(
            vec![candidate("one", "fireworks")],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("Fireworks provider should build");

        assert_eq!(route.candidates[0].definition.model, "upstream/one");
    }

    #[test]
    fn selected_candidate_defaults_to_primary_and_matches_fallback() {
        let route = build_with_credentials(
            vec![candidate("one", "fireworks"), candidate("two", "together")],
            zeroclaw_api::model_provider::BASELINE_MAX_TOKENS,
            |_| Some("secret".to_owned()),
        )
        .expect("providers should build");

        assert_eq!(
            route
                .selected_candidate(None)
                .expect("primary should exist")
                .definition()
                .id,
            "one"
        );

        let fallback = ProviderFallbackInfo {
            requested_provider: "one".to_owned(),
            requested_model: "zero/test".to_owned(),
            actual_provider: "two".to_owned(),
            actual_model: "zero/test".to_owned(),
        };
        assert_eq!(
            route
                .selected_candidate(Some(&fallback))
                .expect("fallback should match")
                .definition()
                .id,
            "two"
        );
    }
}

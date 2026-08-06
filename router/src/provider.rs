//! ZeroRouter's own upstream-provider interface.
//!
//! This is the boundary that makes ZeroRouter a standalone service. The
//! router previously spoke the ZeroClaw agent runtime's `ModelProvider`
//! trait through a git pin, which meant a commercial gateway's core
//! interface — and its dependency tree, and its release cadence — belonged
//! to an unrelated open-source project. The relationship runs the other way
//! now: ZeroClaw consumes ZeroRouter over HTTP like any other customer, and
//! ZeroRouter defines its own contract.
//!
//! The shapes here are deliberately the SAME shapes the pinned trait used.
//! That is not laziness: the wire clients, the billing views, and the
//! streaming path were all written against them and are covered by tests
//! that assert exact token arithmetic. Re-deriving the vocabulary at the
//! same time as re-homing it would put two changes in one diff and make a
//! metering regression indistinguishable from a refactor. What IS dropped
//! is everything the router never used — the agent-facing helpers, prompt
//! pruning, capability negotiation, tool conversion, wire-api selection.
//!
//! A [`crate::providers::PinnedAdapter`] bridges upstream adapters that
//! still implement the old trait, so the pin can be deleted one provider at
//! a time instead of in a single irreversible swap.

use std::borrow::Cow;

use async_trait::async_trait;
use futures_util::stream::BoxStream;

/// One turn of conversation, as the router hands it to an upstream.
///
/// `role` is a plain string rather than an enum because ZeroRouter's compat
/// layer accepts whatever OpenAI accepts and the wire clients match on it
/// directly; an enum here would only move the unknown-role case around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
        }
    }
}

/// A tool the model may call, as the customer declared it.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: serde_json::Value,
}

/// A tool call the model emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as a JSON *string*, which is the OpenAI wire convention the
    /// router's compat surface both accepts and emits.
    pub arguments: String,
    /// Provider-specific fields that must round-trip unchanged on the next
    /// turn (e.g. a reasoning signature). Opaque here on purpose.
    pub extra_content: Option<serde_json::Value>,
}

/// Raw token counts from one upstream response.
///
/// The contract is load-bearing for billing and is asserted by tests in
/// `wire.rs` and `openai.rs`: `input_tokens` is the TOTAL prompt the model
/// saw, and `cached_input_tokens` is the SUBSET of it served from the
/// provider's prompt cache. So `cached <= input`, and the fresh portion is
/// `input - cached`. Anthropic reports three disjoint buckets instead; the
/// Anthropic wire folds them into this convention rather than leaking a
/// second convention into the router.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

/// A non-streaming upstream answer.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
    /// Chain-of-thought from thinking models, passed through opaquely: some
    /// providers reject tool-call history that omits it.
    pub reasoning_content: Option<String>,
}

impl ChatResponse {
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// What the router asks an upstream for.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [ToolSpec]>,
}

/// One streamed delta.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    pub delta: String,
    pub reasoning: Option<String>,
    pub is_final: bool,
    /// A LOWER BOUND the provider may supply, never a bill. Billing uses
    /// metered actuals only (see `StreamDelivery::settled_usage`).
    pub token_count: usize,
}

impl StreamChunk {
    pub fn delta(text: impl Into<String>) -> Self {
        Self {
            delta: text.into(),
            ..Self::default()
        }
    }
}

/// Everything an upstream can say mid-stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(StreamChunk),
    ToolCall(ToolCall),
    /// Usage, typically just before [`StreamEvent::Final`]. Providers that
    /// never surface streaming usage simply omit it, which the settle path
    /// treats as the missing-usage case rather than guessing.
    Usage(TokenUsage),
    Final,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StreamOptions {
    pub enabled: bool,
    pub count_tokens: bool,
}

pub type StreamResult<T> = std::result::Result<T, StreamError>;

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("JSON parse error: {0}")]
    Json(serde_json::Error),
    #[error("Invalid SSE format: {0}")]
    InvalidSse(String),
    #[error("ModelProvider error: {0}")]
    ModelProvider(String),
}

/// What an upstream can do. The router reads only these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub native_tool_calling: bool,
    pub vision: bool,
    pub prompt_caching: bool,
}

/// Output ceiling assumed when a request names none.
pub const BASELINE_MAX_TOKENS: u32 = 4096;

/// One upstream ZeroRouter can dispatch to.
///
/// Three methods, because three is what routing needs: can you stream, give
/// me an answer, give me a stream. Everything the agent runtime's trait
/// carried beyond this — prompt shaping, tool conversion, capability
/// negotiation, wire selection — is the caller's business, and in a gateway
/// the caller is the customer's own request.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stable identifier for logs and attribution.
    fn alias(&self) -> Cow<'_, str>;

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse>;

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>>;
}

// ---------------------------------------------------------------------------
// Bridge to the pinned adapters
// ---------------------------------------------------------------------------

/// Wraps an upstream adapter that still implements the ZeroClaw runtime's
/// trait so it satisfies ZeroRouter's.
///
/// This exists so the git pin can be deleted one provider at a time. Two
/// adapters still come from it — the OpenAI-compatible client (deepinfra,
/// fireworks, together, minimax) and Bedrock — and rewriting both in the
/// same change that re-homes the interface would mean a metering regression
/// and a refactor arriving as one indistinguishable diff. Every provider
/// that moves to a ZeroRouter-owned wire drops out of this bridge; when the
/// last one does, the bridge and the pin go together.
///
/// The conversions are field-for-field because the shapes are identical by
/// construction (see this module's header). Nothing is inferred, defaulted,
/// or dropped on the way through — a bridge that quietly lost
/// `cached_input_tokens` would silently change what customers are charged.
pub struct PinnedAdapter {
    inner: std::sync::Arc<dyn zeroclaw_api::model_provider::ModelProvider>,
    alias: String,
}

impl PinnedAdapter {
    #[must_use]
    pub fn new(
        alias: &str,
        inner: std::sync::Arc<dyn zeroclaw_api::model_provider::ModelProvider>,
    ) -> Self {
        Self {
            inner,
            alias: alias.to_owned(),
        }
    }
}

fn to_pinned_message(message: &ChatMessage) -> zeroclaw_providers::traits::ChatMessage {
    zeroclaw_providers::traits::ChatMessage {
        role: message.role.clone(),
        content: message.content.clone(),
    }
}

fn to_pinned_tool(tool: &ToolSpec) -> zeroclaw_api::tool::ToolSpec {
    zeroclaw_api::tool::ToolSpec {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: std::sync::Arc::new(tool.parameters.clone()),
        output: None,
        param_domains: std::collections::BTreeMap::new(),
    }
}

fn from_pinned_usage(usage: zeroclaw_providers::traits::TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
    }
}

fn from_pinned_tool_call(call: zeroclaw_providers::traits::ToolCall) -> ToolCall {
    ToolCall {
        id: call.id,
        name: call.name,
        arguments: call.arguments,
        extra_content: call.extra_content,
    }
}

#[async_trait]
impl ModelProvider for PinnedAdapter {
    fn alias(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.alias)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        let inner = self.inner.capabilities();
        ProviderCapabilities {
            native_tool_calling: inner.native_tool_calling,
            vision: inner.vision,
            prompt_caching: inner.prompt_caching,
        }
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let messages: Vec<_> = request.messages.iter().map(to_pinned_message).collect();
        let tools: Option<Vec<_>> = request
            .tools
            .map(|tools| tools.iter().map(to_pinned_tool).collect());
        let pinned = zeroclaw_providers::traits::ChatRequest {
            messages: &messages,
            tools: tools.as_deref(),
            thinking: None,
        };
        let response = self.inner.chat(pinned, model, temperature).await?;
        Ok(ChatResponse {
            text: response.text,
            tool_calls: response
                .tool_calls
                .into_iter()
                .map(from_pinned_tool_call)
                .collect(),
            usage: response.usage.map(from_pinned_usage),
            reasoning_content: response.reasoning_content,
        })
    }

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>> {
        let messages: Vec<_> = request.messages.iter().map(to_pinned_message).collect();
        let tools: Option<Vec<_>> = request
            .tools
            .map(|tools| tools.iter().map(to_pinned_tool).collect());
        let inner = std::sync::Arc::clone(&self.inner);
        let model = model.to_owned();
        let pinned_options = zeroclaw_providers::traits::StreamOptions {
            enabled: options.enabled,
            count_tokens: options.count_tokens,
        };
        // The pinned stream borrows its request, so the owned copies above
        // are moved into the generator and the borrow is taken inside.
        let stream = async_stream::stream! {
            let pinned = zeroclaw_providers::traits::ChatRequest {
                messages: &messages,
                tools: tools.as_deref(),
                thinking: None,
            };
            let mut inner_stream = inner.stream_chat(pinned, &model, temperature, pinned_options);
            while let Some(event) = futures_util::StreamExt::next(&mut inner_stream).await {
                yield match event {
                    Ok(event) => Ok(match event {
                        zeroclaw_providers::traits::StreamEvent::TextDelta(chunk) => {
                            StreamEvent::TextDelta(StreamChunk {
                                delta: chunk.delta,
                                reasoning: chunk.reasoning,
                                is_final: chunk.is_final,
                                token_count: chunk.token_count,
                            })
                        }
                        zeroclaw_providers::traits::StreamEvent::ToolCall(call) => {
                            StreamEvent::ToolCall(from_pinned_tool_call(call))
                        }
                        zeroclaw_providers::traits::StreamEvent::Usage(usage) => {
                            StreamEvent::Usage(from_pinned_usage(usage))
                        }
                        zeroclaw_providers::traits::StreamEvent::Final => StreamEvent::Final,
                        // Pre-executed tool events are an agent-runtime
                        // concept: a gateway neither executes tools nor
                        // reports them, so they carry nothing to forward.
                        _ => continue,
                    }),
                    Err(error) => Err(match error {
                        zeroclaw_providers::traits::StreamError::Json(error) => {
                            StreamError::Json(error)
                        }
                        zeroclaw_providers::traits::StreamError::InvalidSse(detail) => {
                            StreamError::InvalidSse(detail)
                        }
                        // Everything else keeps its text, which is what
                        // `retry::classify` reads.
                        other => StreamError::Http(other.to_string()),
                    }),
                };
            }
        };
        Box::pin(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usage_contract_is_cached_as_a_subset_of_input() {
        // The one invariant every wire must normalize to, restated here
        // because this module is now its home rather than a git pin.
        let usage = TokenUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(50),
            cached_input_tokens: Some(900),
        };
        assert!(usage.cached_input_tokens.unwrap() <= usage.input_tokens.unwrap());
        let fresh = usage.input_tokens.unwrap() - usage.cached_input_tokens.unwrap();
        assert_eq!(
            fresh, 100,
            "the billable fresh portion is input minus cached"
        );
    }

    #[test]
    fn message_constructors_carry_the_roles_the_wires_match_on() {
        for (message, role) in [
            (ChatMessage::system("s"), "system"),
            (ChatMessage::user("u"), "user"),
            (ChatMessage::assistant("a"), "assistant"),
            (ChatMessage::tool("t"), "tool"),
        ] {
            assert_eq!(message.role, role);
        }
    }
}

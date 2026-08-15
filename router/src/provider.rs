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
//! The bridge that once wrapped pinned adapters is gone with them: the MVP
//! integrates OpenAI and Anthropic directly, and both wires implement this
//! trait natively.

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

/// Why an upstream stopped generating, in the ONE vocabulary this router
/// speaks — OpenAI's.
///
/// Every wire normalizes into this on the way in, so nothing downstream has to
/// know which dialect answered. The variants are deliberately only the four
/// OpenAI defines: a value that does not map onto one of them is reported as
/// `None` (absent) rather than guessed at, because the whole point of carrying
/// a REAL stop reason is that it is the upstream's word and not the router's
/// inference. `openai::finish_reason` still synthesizes a reason for the absent
/// case; the two are kept distinguishable all the way to the ledger by
/// `request_attempts.finish_reason_source` / `usage_events.finish_reason_source`
/// (`"provider"` vs `"synthetic"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished on its own — a natural end of turn or a stop
    /// sequence.
    Stop,
    /// Output was clipped by a token ceiling.
    Length,
    /// The model emitted tool calls and is waiting on their results.
    ToolCalls,
    /// The provider's safety layer withheld or truncated the output. The
    /// router could not observe this state at all before the wires carried it.
    ContentFilter,
}

impl StopReason {
    /// The OpenAI wire spelling, which is also what is persisted and what
    /// `openai::shape_ok` compares against.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
        }
    }

    /// Parse an OpenAI-dialect `finish_reason` string.
    ///
    /// `function_call` is the pre-`tool_calls` spelling several local servers
    /// still emit and means the same thing. Anything else — including a
    /// provider-specific extension — is `None`: an unrecognized reason is not
    /// evidence of a normal stop, and mapping it to [`Self::Stop`] would
    /// launder an unknown into a confident claim.
    #[must_use]
    pub fn from_openai(reason: &str) -> Option<Self> {
        match reason {
            "stop" => Some(Self::Stop),
            "length" => Some(Self::Length),
            "tool_calls" | "function_call" => Some(Self::ToolCalls),
            "content_filter" => Some(Self::ContentFilter),
            _ => None,
        }
    }
}

/// Why a stream settled with no usage to bill.
///
/// Both values bill nothing, and that is exactly why they are kept apart: one
/// is an ordinary local-server limitation and the other is what a truncating
/// middlebox looks like. Carried out of the wire rather than only logged so it
/// can be COUNTED on the settled row — see `usage_events.usage_gap`
/// (migration 0020). The free lane writes no `request_attempts` rows, so a
/// trace line was the only record a free-lane gap left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageGap {
    /// The stream framed itself correctly (`[DONE]` arrived) and simply never
    /// sent the optional usage chunk. Several local servers do this on every
    /// request.
    IncludeUsageIgnored,
    /// The socket closed after a real `finish_reason` without the sentinel AND
    /// without usage — indistinguishable at this layer from a proxy eating the
    /// tail of a stream whose upstream does report usage.
    DoneMissing,
}

impl UsageGap {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncludeUsageIgnored => "include_usage_ignored",
            Self::DoneMissing => "done_missing",
        }
    }

    /// Parse a persisted or replayed label back. Anything unrecognized is
    /// `None` — a settlement intent must never hand the ledger a token
    /// migration 0020's CHECK would reject.
    #[must_use]
    pub fn from_keyword(label: &str) -> Option<Self> {
        match label {
            "include_usage_ignored" => Some(Self::IncludeUsageIgnored),
            "done_missing" => Some(Self::DoneMissing),
            _ => None,
        }
    }
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
    /// The upstream's OWN stop reason, normalized, when it gave one.
    ///
    /// `None` means the upstream said nothing this router could map — not that
    /// it stopped normally. The consumption rule downstream is: present wins
    /// and is stamped `"provider"`; absent falls back to the unchanged
    /// synthesis and is stamped `"synthetic"`.
    pub stop_reason: Option<StopReason>,
}

impl ChatResponse {
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Text content, or empty. Callers that ask "did the model say
    /// anything?" want the empty string, not an Option dance.
    #[must_use]
    pub fn text_or_empty(&self) -> &str {
        self.text.as_deref().unwrap_or("")
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
    /// A rough estimate the provider may supply, never a bill. Billing uses
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

    /// A reasoning-only delta: thinking models emit these with no content.
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self {
            reasoning: Some(text.into()),
            ..Self::default()
        }
    }

    /// Attach the documented per-chunk estimate: bytes over the ~4-chars-per-
    /// token rule of thumb, rounded up so a short delta still counts as one.
    /// Observability only, never a bill — and computed over content, which is
    /// why a reasoning-only chunk contributes nothing.
    #[must_use]
    pub fn with_token_estimate(mut self) -> Self {
        self.token_count = self.delta.len().div_ceil(4);
        self
    }
}

/// What the upstream said as the stream ended.
///
/// Carried ON the terminal rather than beside it because both fields are only
/// knowable once the stream is over, and a separate event would let a consumer
/// forget to wait for it. Defaults to "the upstream told us nothing", which is
/// what every synthesized or test-authored terminal means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamFinal {
    /// The upstream's own stop reason, normalized. See [`StopReason`].
    pub stop_reason: Option<StopReason>,
    /// Why this stream has no usage to settle, when it has none. `None` means
    /// usage WAS reported. See [`UsageGap`].
    pub usage_gap: Option<UsageGap>,
}

impl StreamFinal {
    /// A terminal carrying no upstream claims — the honest default for a
    /// synthesized stream or a wire that reports neither field.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_stop_reason(stop_reason: Option<StopReason>) -> Self {
        Self {
            stop_reason,
            usage_gap: None,
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
    /// The terminal, carrying whatever the upstream said on its way out.
    Final(StreamFinal),
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

/// Per-million-token prices for one model, on the three dimensions
/// ZeroRouter meters. `None` means "not priced here", which the cost
/// function treats as unknown rather than free.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelRates {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    pub cached_input_per_mtok: Option<f64>,
}

impl ModelRates {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input_per_mtok.is_none()
            && self.output_per_mtok.is_none()
            && self.cached_input_per_mtok.is_none()
    }

    /// The cached-input rate this table will actually be BILLED at.
    ///
    /// An absent `cached_input_per_mtok` is not "no cached charge" — it falls
    /// back to the input rate, because that is precisely what
    /// [`crate::openai::usage_cost`] does with it. Every comparison between
    /// two rate tables has to be made on these values rather than on the
    /// declared ones, or a `None` silently skips a comparison that settlement
    /// will later make real: a candidate that omits the dimension is billed at
    /// its input rate, not at nothing.
    ///
    /// `None` only when the input rate is absent too, which catalog validation
    /// refuses before any comparison runs (`validate_rates`, required
    /// dimensions). Kept as an `Option` anyway so "genuinely unknown" stays
    /// distinguishable from "known to be zero" — the distinction this whole
    /// type exists to preserve.
    #[must_use]
    pub fn effective_cached_input_per_mtok(&self) -> Option<f64> {
        self.cached_input_per_mtok.or(self.input_per_mtok)
    }

    /// Whether the two REQUIRED dimensions are declared and exactly zero.
    ///
    /// Weaker than [`Self::are_zero`] on purpose, and used for a different
    /// question. This one asks "does this table CLAIM that the traffic every
    /// request generates costs nothing" — input and output tokens exist on
    /// every request, cached ones do not — which is the claim
    /// `validate_zero_price` refuses to accept about an upstream that bills.
    /// A table reading `{input: 0, output: 0, cached_input: 5.00}` is not
    /// free, but it still makes that claim, and a rule that only looked at
    /// [`Self::are_zero`] let it through.
    #[must_use]
    pub fn required_rates_are_zero(&self) -> bool {
        self.input_per_mtok == Some(0.0) && self.output_per_mtok == Some(0.0)
    }

    /// Whether this table prices every dimension at exactly zero.
    ///
    /// The mechanical half of "free", stated once and read on both sides of
    /// the question: a CANDIDATE's zero basis (ZeroRouter is charged nothing
    /// to serve it, [`crate::config::TierCandidate::rates_are_zero`]) and a
    /// TIER's zero sell rate (the customer is charged nothing to be served,
    /// [`crate::config::ResolvedRoute::sells_free`]). Those are different
    /// claims about different money; they are the same arithmetic.
    ///
    /// Zero means all three EFFECTIVE rates are zero — so an absent
    /// cached-input dimension is read as the input rate, exactly as
    /// [`crate::openai::usage_cost`] reads it, and an absent cached rate over
    /// a zero input rate therefore prices at zero. That is what lets the
    /// commonest honest local config (no cache pricing at all, because the
    /// server has no cache) be free without the operator writing `= 0` for a
    /// dimension their server does not have. A DECLARED nonzero cached rate is
    /// still money, and still disqualifies.
    #[must_use]
    pub fn are_zero(&self) -> bool {
        self.required_rates_are_zero() && self.effective_cached_input_per_mtok() == Some(0.0)
    }
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

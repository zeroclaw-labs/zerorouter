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

/// One piece of a user turn's content, carried OUT OF BAND from
/// [`ChatMessage::content`].
///
/// This type exists because the alternative did not work. Structured content
/// used to be flattened into the one `content` string with an in-band
/// `[IMAGE:<url>]` marker, and the wires split that grammar back out — which
/// meant a customer whose PROSE contained the same byte sequence had it
/// silently promoted to a real image on three dialects, and had the request
/// refused before dial on the fourth (Bedrock carries no URL image sources).
/// No escaping scheme fixes that honestly: the escape would have to be applied
/// to every user turn, including the overwhelming majority that carry no image
/// at all, and a wire that forgot to reverse it would corrupt the customer's
/// prompt rather than merely drop a picture. Carrying the structure separately
/// makes the ambiguity unrepresentable instead of merely unlikely.
///
/// Only `user` turns ever carry parts: `image_url` content parts are user-only
/// in the OpenAI schema, and `openai::content_is_supported` refuses them
/// anywhere else before a `ChatMessage` is ever built.
///
/// # Per-part cache breakpoints
///
/// Each part carries its own `cache_control` bool, distinct from the
/// MESSAGE-level [`ChatMessage::cache_control`]. This is the placeable form of
/// the spelling OpenRouter documents — `{"type":"text","text":"…",
/// "cache_control":{"type":"ephemeral"}}` — which the router used to refuse
/// because a text content array was flattened to one string before any wire
/// saw it, leaving a breakpoint on part 2 of 4 with nowhere honest to land. A
/// part now carries the mark on the block IT produces, so the boundary the
/// client named is the boundary sent upstream. A bool rather than a value for
/// the same reason [`ChatMessage::cache_control`] is: `{"type":"ephemeral"}`
/// is the only thing a breakpoint may say, validated at the edge by
/// `openai::validate_cache_control`, so by the time a part exists there is
/// exactly one thing the mark can mean.
///
/// The two levels coexist. Only the Anthropic Messages wires
/// (`wire::anthropic`, `wire::bedrock_runtime`) read the mark; the other lanes
/// cannot carry an explicit breakpoint at all, and a request that places one
/// against them is refused before dispatch by the same pre-reserve capability
/// gate the message-level feature uses (`api::unservable_prompt_caching`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    /// A run of the customer's text, verbatim. Never re-scanned for a grammar.
    Text {
        text: String,
        /// Whether the CLIENT placed a cache breakpoint on this text block.
        cache_control: bool,
    },
    /// An image, as the `url` string the customer supplied — either an
    /// `https://…` URL or a `data:<media-type>;base64,…` URI. Kept in the form
    /// it arrived in; each wire decides how its own dialect carries it.
    Image {
        url: String,
        /// Whether the CLIENT placed a cache breakpoint on this image block.
        cache_control: bool,
    },
}

impl ContentPart {
    /// A text part with no breakpoint — the overwhelmingly common shape, and
    /// the one every caller built before per-part breakpoints existed.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: false,
        }
    }

    /// An image part with no breakpoint.
    #[must_use]
    pub fn image(url: impl Into<String>) -> Self {
        Self::Image {
            url: url.into(),
            cache_control: false,
        }
    }

    /// Whether the client placed a cache breakpoint on this part.
    #[must_use]
    pub fn cache_control(&self) -> bool {
        match self {
            Self::Text { cache_control, .. } | Self::Image { cache_control, .. } => *cache_control,
        }
    }
}

/// One turn of conversation, as the router hands it to an upstream.
///
/// `role` is a plain string rather than an enum because ZeroRouter's compat
/// layer accepts whatever OpenAI accepts and the wire clients match on it
/// directly; an enum here would only move the unknown-role case around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    /// The turn's TEXT. For every turn built by a constructor other than
    /// [`ChatMessage::user_parts`] this is the whole turn, and it is literal:
    /// no wire applies any grammar to it, whatever bytes it happens to spell.
    ///
    /// For a `user_parts` turn it is the text-only flattening of `parts` (the
    /// text runs joined with `\n`, images contributing nothing), derived by the
    /// constructor. It is a convenience view for anything reading a turn
    /// generically — a log line, a length bound — and `parts` is authoritative.
    /// The property that matters is that it can never contain a marker for a
    /// reader to misinterpret, because nothing writes one any more.
    pub content: String,
    /// Whether the CLIENT asked for a prompt-cache breakpoint at the end of
    /// this turn.
    ///
    /// A bool rather than a value, because the only breakpoint this router
    /// accepts is `{"type": "ephemeral"}` — `openai::validate_cache_control`
    /// refuses everything else at the edge, so by the time a turn exists there
    /// is exactly one thing a breakpoint can say. Carrying the JSON instead
    /// would invite a wire to forward an unvalidated object.
    ///
    /// `false` on every turn built before a client asked for anything, which is
    /// almost all of them; the wires then place their own default breakpoints
    /// exactly as they always have. See `wire::anthropic` for why it is the
    /// client's placement or the wire's, never both.
    pub cache_control: bool,
    /// The turn's structured content, when it had any.
    ///
    /// EMPTY for every turn whose content is plain text, which is the
    /// overwhelming majority and the only shape that existed before the compat
    /// surface accepted OpenAI content arrays. Non-empty ONLY when the customer
    /// sent a content array carrying at least one image, and then it is the
    /// authoritative, ordered content of the turn.
    pub parts: Vec<ContentPart>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            parts: Vec::new(),
            cache_control: false,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            parts: Vec::new(),
            cache_control: false,
        }
    }

    /// A user turn whose content arrived as an OpenAI content ARRAY carrying at
    /// least one image.
    ///
    /// `content` is derived here rather than supplied, so the text view and the
    /// structured view cannot be handed in already disagreeing.
    #[must_use]
    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        let content = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            role: "user".into(),
            content,
            parts,
            cache_control: false,
        }
    }

    /// The same turn, with a client-placed cache breakpoint at its end.
    #[must_use]
    pub fn with_cache_breakpoint(mut self) -> Self {
        self.cache_control = true;
        self
    }

    /// Whether the CLIENT placed any cache breakpoint on this turn — at the
    /// message level (`cache_control`, at the end of the turn) OR on one of its
    /// content parts. The two spellings coexist, and either one means the
    /// client owns this request's cache boundaries (see `wire::anthropic` for
    /// why it is the client's placement or the wire's defaults, never both).
    #[must_use]
    pub fn has_client_breakpoint(&self) -> bool {
        self.cache_control || self.parts.iter().any(ContentPart::cache_control)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            parts: Vec::new(),
            cache_control: false,
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            parts: Vec::new(),
            cache_control: false,
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
    /// Whether the CLIENT asked for a prompt-cache breakpoint on this tool.
    /// See [`ChatMessage::cache_control`] for why it is a bool.
    pub cache_control: bool,
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
/// saw, and `cached_input_tokens` and `cache_write_input_tokens` are DISJOINT
/// SUBSETS of it — tokens served from the provider's prompt cache, and tokens
/// written INTO it under a cache breakpoint. So `cached + written <= input`,
/// and the fresh portion is `input - cached - written`. Anthropic reports
/// three disjoint buckets instead; the Anthropic wire folds them into this
/// convention rather than leaking a second convention into the router.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    /// Prompt tokens the upstream STORED under a cache breakpoint on this
    /// request, which vendors bill at a premium over a fresh read.
    ///
    /// A subset of `input_tokens` and disjoint from `cached_input_tokens`:
    /// a token is read from the cache or written to it, never both. `None`
    /// means the upstream reported no such dimension, which is every wire but
    /// the two that speak the Anthropic Messages dialect — and is why a
    /// request that reports nothing here prices exactly as it did before this
    /// dimension existed.
    pub cache_write_input_tokens: Option<u64>,
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
/// case; the two are kept distinguishable on the settled row by
/// `usage_events.finish_reason_source` (`"upstream"` vs `"synthetic"` — the
/// only two tokens migration 0004's CHECK permits).
///
/// `request_attempts` has no such column, so attempt rows mix the two kinds
/// with no provenance of their own; see `openai::AttemptFinishReason` for why
/// that is recoverable for the served attempt and nowhere else.
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
    /// and is stamped `"upstream"`; absent falls back to the unchanged
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

/// Per-million-token prices for one model, on the four dimensions
/// ZeroRouter meters. `None` means "not priced here", which the cost
/// function treats as unknown rather than free.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelRates {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    pub cached_input_per_mtok: Option<f64>,
    /// What a CACHE WRITE costs — a prompt token the upstream both read and
    /// stored under a cache breakpoint, which Anthropic and Bedrock-Claude
    /// bill at 1.25x the input rate for the 5-minute TTL this router uses.
    ///
    /// # Absence is the capability signal, and it is ALSO a billing fallback
    ///
    /// Those two readings sound contradictory and are not, because they answer
    /// different questions at different times:
    ///
    /// - **Admission** reads the DECLARED value. A lane that does not price
    ///   cache writes may not accept a client `cache_control` breakpoint at
    ///   all — [`crate::error::ApiError::PromptCachingUnsupported`] — exactly
    ///   the way an absent modality means a lane refuses images. A price
    ///   nobody transcribed is not a price, and selling a dimension at a
    ///   guessed rate is the one thing this catalog never does.
    /// - **Settlement** reads [`Self::effective_cache_write_per_mtok`], which
    ///   falls an absent rate back to the INPUT rate. That is not a licence to
    ///   accept the request; it is what keeps a write the router did not ask
    ///   for from billing at zero. The Anthropic wire sets three breakpoints of
    ///   its own on every request (see `wire::anthropic`), so cache-creation
    ///   tokens arrive on lanes that never declared the dimension, and pricing
    ///   them at nothing would be a giveaway rather than a refusal.
    ///
    /// So the honest one-line summary is: **absent means "this lane does not
    /// SELL cache writes", and the biller charges input for them anyway** —
    /// which is precisely what it charged before this dimension existed, so
    /// every lane that omits the key prices bit-for-bit as it did.
    pub cache_write_per_mtok: Option<f64>,
}

impl ModelRates {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input_per_mtok.is_none()
            && self.output_per_mtok.is_none()
            && self.cached_input_per_mtok.is_none()
            && self.cache_write_per_mtok.is_none()
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

    /// The cache-write rate this table will actually be BILLED at.
    ///
    /// The same fallback [`Self::effective_cached_input_per_mtok`] applies, and
    /// for the same reason: [`crate::openai::usage_cost`] prices a cache-write
    /// token at the input rate when the dimension is unset, so every
    /// comparison between two rate tables has to be made on this value rather
    /// than on the declared one. See the field's own doc for why the DECLARED
    /// value is nonetheless what admission reads.
    #[must_use]
    pub fn effective_cache_write_per_mtok(&self) -> Option<f64> {
        self.cache_write_per_mtok.or(self.input_per_mtok)
    }

    /// Whether this table states a cache-write price of its own — the
    /// capability question, asked on the DECLARED field.
    #[must_use]
    pub fn prices_cache_writes(&self) -> bool {
        self.cache_write_per_mtok.is_some()
    }

    /// The dearest rate any PROMPT token can be billed at under this table.
    ///
    /// Admission knows nothing about how a prompt will split between fresh,
    /// cached and written tokens — the upstream decides that, and says so only
    /// in the response — so a reservation has to hold the worst of the three.
    /// Before this dimension existed the worst was always the input rate,
    /// because `validate_cache_is_a_discount` forbids a cached rate above it;
    /// a cache-write rate is deliberately ABOVE it (1.25x), so the maximum has
    /// to be taken rather than assumed.
    ///
    /// `None` only when the input rate is absent, which catalog validation
    /// refuses before any request is priced.
    #[must_use]
    pub fn dearest_prompt_per_mtok(&self) -> Option<f64> {
        let dearest = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (rate, None) | (None, rate) => rate,
        };
        dearest(
            dearest(self.input_per_mtok, self.effective_cached_input_per_mtok()),
            self.effective_cache_write_per_mtok(),
        )
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
    /// Zero means all four EFFECTIVE rates are zero — so an absent cached or
    /// cache-write dimension is read as the input rate, exactly as
    /// [`crate::openai::usage_cost`] reads it, and an absent rate over a zero
    /// input rate therefore prices at zero. That is what lets the commonest
    /// honest local config (no cache pricing at all, because the server has no
    /// cache) be free without the operator writing `= 0` for dimensions their
    /// server does not have. A DECLARED nonzero cached or cache-write rate is
    /// still money, and still disqualifies.
    #[must_use]
    pub fn are_zero(&self) -> bool {
        self.required_rates_are_zero()
            && self.effective_cached_input_per_mtok() == Some(0.0)
            && self.effective_cache_write_per_mtok() == Some(0.0)
    }
}

/// One CONDITIONAL rate table: what applies once a request's prompt reaches
/// [`Self::min_prompt_tokens`].
///
/// `min_prompt_tokens` is a minimum, exactly as the name says: the comparison
/// is `>=`, so a request measuring precisely the threshold is priced HERE and
/// not at the table below. See [`RateSchedule::at_prompt_tokens`] for what is
/// being measured and why.
///
/// # The boundary direction is contested, and this is the safe side of it
///
/// The vendors do not agree with each other in writing. OpenRouter publishes
/// these bands under a `min_prompt_tokens` key, and a minimum is inclusive.
/// Google's own pricing page describes Gemini's band as applying to prompts
/// "over" 200,000 tokens, which reads exclusive. The source catalog ZeroRouter
/// reconciles against carries only a `size`, so it cannot settle the question
/// either way.
///
/// `>=` is chosen because it matches the field name an operator reads in
/// `tiers.toml` and the `≥` the drift report already renders. The exposure is
/// exactly one token count: a request measuring *precisely* the threshold is
/// charged the high rate, and if the vendor turns out to be exclusive there,
/// ZeroRouter collects the high rate while paying the low one. That is a
/// markup rather than a loss — the safe direction for the balance, the wrong
/// direction for a pass-through promise — and it is bounded by how often a
/// prompt lands on an exact round number.
///
/// **Drift cannot catch this**, and that is why it is written down rather than
/// left to CI: basis and sell both run through this same function, so they
/// agree with each other no matter which way the comparison points, and the
/// source publishes no inclusivity to compare against. Confirming it takes a
/// real invoice for a boundary-exact request. Flipping it afterwards is one
/// operator in this function.
///
/// Pinned by `provider::tests::the_threshold_is_a_minimum_and_includes_its_own_boundary`,
/// which fails if the direction changes silently.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConditionalRate {
    pub min_prompt_tokens: u64,
    pub rates: ModelRates,
}

/// A price for one model that may REPRICE THE WHOLE REQUEST past a prompt-size
/// threshold — the shape several vendors' long-context pricing actually has.
///
/// # The repricing is a step, never a margin
///
/// An upstream quotes one price up to some number of prompt tokens and a
/// different, higher price for everything past it. Past the boundary EVERY
/// dimension reprices for the WHOLE request: a 300,000-token prompt is billed
/// at the high input rate on all 300,000 tokens, and its entire completion at
/// the high output rate. Nothing is split at the boundary and nothing is
/// blended — the vendor charges as though the low rate had never applied. That
/// is why a [`ConditionalRate`] carries a complete [`ModelRates`] that
/// REPLACES the base table, rather than a delta that would have to be
/// integrated over a token range.
///
/// # A flat schedule is the old type, exactly
///
/// A schedule with no conditional tables answers [`Self::base`] to every
/// question — [`Self::at_prompt_tokens`] and [`Self::worst_case`] both return
/// the base table itself, by an early return rather than by arithmetic that
/// happens to agree. So a `tiers.toml` written before conditional rates
/// existed prices bit-for-bit as it did, on every path, and that guarantee is
/// a property of this type rather than something each caller has to preserve.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateSchedule {
    base: ModelRates,
    conditional: Vec<ConditionalRate>,
}

impl From<ModelRates> for RateSchedule {
    /// One rate table is a schedule that charges it at every size.
    fn from(base: ModelRates) -> Self {
        Self::flat(base)
    }
}

impl RateSchedule {
    /// A schedule that charges one price at every size — every rate table that
    /// existed before conditional rates did.
    #[must_use]
    pub fn flat(base: ModelRates) -> Self {
        Self {
            base,
            conditional: Vec::new(),
        }
    }

    /// A schedule with conditional tables. Nothing here orders or deduplicates
    /// them: `validate_rate_schedule` (`crate::config`) refuses a file whose
    /// thresholds are not strictly ascending, so the file reads in the order it
    /// prices in. [`Self::at_prompt_tokens`] is written to be correct anyway,
    /// so a schedule built in a test that skipped validation still prices
    /// right.
    #[must_use]
    pub fn new(base: ModelRates, conditional: Vec<ConditionalRate>) -> Self {
        Self { base, conditional }
    }

    /// The table that applies below every threshold.
    ///
    /// This is what the catalog ADVERTISES and what a report renders; it is
    /// never on its own what a request is billed at. Use
    /// [`Self::at_prompt_tokens`] to charge and [`Self::worst_case`] to
    /// reserve.
    #[must_use]
    pub fn base(&self) -> ModelRates {
        self.base
    }

    #[must_use]
    pub fn conditional(&self) -> &[ConditionalRate] {
        &self.conditional
    }

    /// Whether this schedule charges one price at every size.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.conditional.is_empty()
    }

    /// Every rate table this schedule can ever apply, base first.
    fn tables(&self) -> impl Iterator<Item = ModelRates> + '_ {
        std::iter::once(self.base)
            .chain(self.conditional.iter().map(|conditional| conditional.rates))
    }

    /// The rates a request measuring `prompt_tokens` is billed at: the highest
    /// threshold at or below it, or the base table when it is under all of
    /// them.
    ///
    /// # What `prompt_tokens` must be
    ///
    /// The MEASURED prompt count the upstream reported —
    /// `OpenAiUsage::prompt_tokens` — and not the byte-length prompt bound
    /// admission sizes reservations against. The two differ by roughly the
    /// bytes-per-token ratio, so selecting a tier from the bound would move the
    /// boundary by a factor of about four. Reservation sizing does not call
    /// this at all; it calls [`Self::worst_case`], which needs no measurement.
    ///
    /// # Cached prompt tokens COUNT toward the threshold
    ///
    /// Deliberate, and not a judgment call this code had to make: the wire
    /// contract is that cached input is a SUBSET of input (`TokenUsage`, and
    /// the test that pins it in this module), and [`crate::openai::usage_cost`]
    /// reads exactly that — it splits `prompt_tokens` into a cached part and an
    /// uncached remainder rather than adding them. So `prompt_tokens` is
    /// already the whole prompt, cached portion included, which is the figure
    /// vendors quote their thresholds against. A request of 250,000 cached plus
    /// 30,000 fresh tokens is a 280,000-token prompt to OpenAI and is a
    /// 280,000-token prompt here.
    #[must_use]
    pub fn at_prompt_tokens(&self, prompt_tokens: u64) -> ModelRates {
        if self.conditional.is_empty() {
            return self.base;
        }
        self.conditional
            .iter()
            .filter(|conditional| prompt_tokens >= conditional.min_prompt_tokens)
            .max_by_key(|conditional| conditional.min_prompt_tokens)
            .map_or(self.base, |conditional| conditional.rates)
    }

    /// The dearest rates this schedule could ever charge, dimension by
    /// dimension — what a RESERVATION must be sized at.
    ///
    /// Admission runs before the upstream has said anything, so the request's
    /// real prompt-token count does not exist yet and no tier can be selected
    /// honestly. Reserving at the base rate would under-reserve every request
    /// that turns out to cross the boundary, and an under-reserved request is a
    /// customer spending past their balance — the one failure this path may
    /// never have. So the reservation is sized at the worst case and settlement
    /// releases the difference, exactly as it already releases the difference
    /// between the byte-length prompt bound and the measured prompt.
    ///
    /// The maximum is taken PER DIMENSION rather than by picking the highest
    /// threshold's table wholesale. Nothing forbids a schedule whose top tier
    /// raises input while lowering output, and a per-dimension maximum can
    /// never be cheaper than the table that ends up applying, whatever shape
    /// the schedule has. The cached dimension is maximised over EFFECTIVE
    /// rates ([`ModelRates::effective_cached_input_per_mtok`]) because that is
    /// what `usage_cost` will bill, so a table that omits the dimension
    /// contributes its input rate rather than skipping the comparison.
    #[must_use]
    pub fn worst_case(&self) -> ModelRates {
        if self.conditional.is_empty() {
            return self.base;
        }
        let dearest = |pick: fn(&ModelRates) -> Option<f64>| {
            self.tables().filter_map(|rates| pick(&rates)).fold(
                None,
                |dearest: Option<f64>, rate| {
                    Some(dearest.map_or(rate, |dearest| dearest.max(rate)))
                },
            )
        };
        ModelRates {
            input_per_mtok: dearest(|rates| rates.input_per_mtok),
            output_per_mtok: dearest(|rates| rates.output_per_mtok),
            cached_input_per_mtok: dearest(ModelRates::effective_cached_input_per_mtok),
            cache_write_per_mtok: dearest(ModelRates::effective_cache_write_per_mtok),
        }
    }

    /// The rate table a RESERVATION must be sized at.
    ///
    /// [`Self::worst_case`] is the dearest table dimension by dimension, which
    /// was sufficient while every prompt-side rate was capped by the input
    /// rate. It is not sufficient any more. A reservation is priced by
    /// [`crate::openai::usage_cost`] over a usage carrying no cache detail, so
    /// EVERY prompt token in the bound is charged at `input_per_mtok` — and a
    /// request that turns out to be entirely cache writes settles at 1.25x
    /// that. The gap sits inside a single band, so no amount of maximising
    /// over the bands closes it.
    ///
    /// This closes it by raising the input dimension to
    /// [`ModelRates::dearest_prompt_per_mtok`]: the dearest rate any prompt
    /// token could be billed at, anywhere in the schedule. The output
    /// dimension is untouched — output tokens have exactly one rate.
    ///
    /// For every schedule that declares no cache-write rate this returns
    /// [`Self::worst_case`] unchanged, because the fallback makes the dearest
    /// prompt rate the input rate again. So a catalog written before this
    /// dimension existed reserves bit-for-bit as it did, and only a lane that
    /// SELLS cache writes holds more.
    #[must_use]
    pub fn reservation_rates(&self) -> ModelRates {
        let worst = self.worst_case();
        ModelRates {
            input_per_mtok: worst.dearest_prompt_per_mtok(),
            ..worst
        }
    }

    /// Whether EVERY table in this schedule states a cache-write price.
    ///
    /// `all`, not "the base table": a band that omits the dimension prices its
    /// writes at that band's INPUT rate (the fallback), so a schedule that
    /// declares the price at the base and forgets it above a threshold would
    /// sell long cache-writing requests at a rate nobody chose. Requiring every
    /// band means the capability is true wherever the request lands. On a flat
    /// schedule — which every lane that declares the dimension today is — this
    /// is the base table's answer and nothing else.
    #[must_use]
    pub fn prices_cache_writes(&self) -> bool {
        self.tables().all(|rates| rates.prices_cache_writes())
    }

    /// Whether EVERY rate this schedule could ever charge is zero.
    ///
    /// The schedule-level [`ModelRates::are_zero`], and it has to quantify over
    /// the conditional tables rather than ask the base one: a schedule reading
    /// `base 0/0/0` with a table charging $5.00 past 272,000 tokens is not
    /// free, and treating it as free would hand the free lane a rung that bills
    /// real money the moment a prompt got long. Asking [`Self::worst_case`] is
    /// the same question stated once — every dimension of the dearest table is
    /// zero exactly when every dimension of every table is.
    #[must_use]
    pub fn are_zero(&self) -> bool {
        self.worst_case().are_zero()
    }

    /// Whether ANY table in this schedule claims the two required dimensions
    /// cost nothing.
    ///
    /// `any`, not "the base table", and not [`Self::are_zero`]. This is the
    /// suspicious question `validate_zero_price` asks — see
    /// [`ModelRates::required_rates_are_zero`] for why it is deliberately
    /// weaker than freeness — and a conditional table asserting that input and
    /// output tokens cost nothing carries exactly the same fat-fingered-rate
    /// signature as a base table asserting it. On a flat schedule this is the
    /// base table's answer and nothing else, so the rule is unchanged for every
    /// catalog written before conditional rates existed.
    #[must_use]
    pub fn any_required_rates_are_zero(&self) -> bool {
        self.tables().any(|rates| rates.required_rates_are_zero())
    }

    /// The thresholds this schedule declares, in declaration order.
    ///
    /// Read by the basis/sell boundary comparison: a candidate and its tier
    /// must agree about WHERE the price changes, or there is a band of prompt
    /// sizes in which one side has repriced and the other has not.
    pub fn thresholds(&self) -> impl Iterator<Item = u64> + '_ {
        self.conditional
            .iter()
            .map(|conditional| conditional.min_prompt_tokens)
    }

    /// The per-dimension DEAREST schedule across `schedules` — the single
    /// schedule that undercharges none of them — or `None` when they do not
    /// share a band STRUCTURE.
    ///
    /// # Why a unified route needs this ("option ii")
    ///
    /// A bare model id backed by the same model on several providers bills at
    /// ONE sell schedule regardless of which provider serves (settlement reads
    /// the route's rate, not the served candidate's — see
    /// [`crate::config::TierCatalog::synthesize_unified`]). So that one schedule
    /// has to be at least as dear as every member in every dimension, or serving
    /// one provider would charge the customer LESS than that provider's own
    /// pinned price does — a silent undercharge that depends on which backend
    /// happened to answer. The reconciled schedule is therefore the per-dimension
    /// MAXIMUM: dearest wins, so no member is ever undercut.
    ///
    /// # The maximum is taken over BILLED (effective) rates, not declared ones
    ///
    /// This matters only for the two optional dimensions, and getting it wrong
    /// is a money bug:
    ///
    /// - **input, output** are required and have no fallback, so effective ==
    ///   declared and the maximum is the plain per-dimension max.
    /// - **cached_input, cache_write** fall back to the INPUT rate when a member
    ///   omits them — [`crate::openai::usage_cost`] prices an omitted dimension
    ///   at input, not at nothing. So a member that omits cache_write does not
    ///   price writes at zero; it prices them at its own input rate. The dearest
    ///   across the group is the max of those EFFECTIVE values
    ///   ([`ModelRates::effective_cache_write_per_mtok`]). Taking the max of the
    ///   DECLARED values instead — "a present rate always beats an absent one" —
    ///   is wrong exactly when a member that OMITS the dimension has a higher
    ///   input rate than another member's declared premium: that omitting member
    ///   would then bill the dimension ABOVE the unified rate, i.e. the unified
    ///   id would undercharge it. The effective max cannot be undercut that way.
    ///
    /// An optional dimension is DECLARED on the result only when at least one
    /// member declared it. When none did, the effective max is exactly the
    /// unified input rate — which an absent dimension already yields by the same
    /// fallback — so leaving it absent is both correct and preserves the "this
    /// lane does not price cache writes" capability signal for a group where no
    /// member prices them.
    ///
    /// # The structure guard, and why it returns `None`
    ///
    /// Reconciliation is per band, so it is only meaningful when every schedule
    /// has the SAME bands: the same count of conditional tables at identical
    /// `min_prompt_tokens` thresholds, in the same order. Two schedules that
    /// reprice at different sizes share no common band to take a maximum within —
    /// a per-dimension max of two structurally different schedules could be
    /// CHEAPER than one of them inside some prompt-size band, breaking the very
    /// invariant this exists to keep — so this returns `None` and the caller
    /// stays conservative (it does not unify). Every same-model twin the catalog
    /// carries today is flat, so `None` is the rare path; the guard is what keeps
    /// a future conditional twin from being reconciled unsafely.
    ///
    /// Returns `None` for an empty slice (nothing to reconcile). A single
    /// schedule reconciles to itself.
    #[must_use]
    pub fn dearest_across(schedules: &[&RateSchedule]) -> Option<RateSchedule> {
        let (first, _rest) = schedules.split_first()?;
        // Same band structure across every schedule, or there is no common band
        // to reconcile within.
        let thresholds: Vec<u64> = first.thresholds().collect();
        if schedules
            .iter()
            .any(|schedule| schedule.thresholds().ne(thresholds.iter().copied()))
        {
            return None;
        }

        // The dearest EFFECTIVE rate for one dimension across every schedule's
        // copy of one band. `effective` reads the billed value (input fallback
        // applied); `declared` decides only whether the result names an OPTIONAL
        // dimension at all — a group in which no member declares it leaves it
        // absent, so the input fallback still yields the same billed rate.
        let dearest = |tables: &[ModelRates],
                       declared: fn(&ModelRates) -> Option<f64>,
                       effective: fn(&ModelRates) -> Option<f64>,
                       optional: bool|
         -> Option<f64> {
            if optional && !tables.iter().any(|rates| declared(rates).is_some()) {
                return None;
            }
            tables
                .iter()
                .filter_map(effective)
                .fold(None, |dearest: Option<f64>, rate| {
                    Some(dearest.map_or(rate, |dearest| dearest.max(rate)))
                })
        };
        let reconcile = |tables: &[ModelRates]| ModelRates {
            input_per_mtok: dearest(tables, |r| r.input_per_mtok, |r| r.input_per_mtok, false),
            output_per_mtok: dearest(tables, |r| r.output_per_mtok, |r| r.output_per_mtok, false),
            cached_input_per_mtok: dearest(
                tables,
                |r| r.cached_input_per_mtok,
                ModelRates::effective_cached_input_per_mtok,
                true,
            ),
            cache_write_per_mtok: dearest(
                tables,
                |r| r.cache_write_per_mtok,
                ModelRates::effective_cache_write_per_mtok,
                true,
            ),
        };

        let base_tables: Vec<ModelRates> =
            schedules.iter().map(|schedule| schedule.base()).collect();
        let base = reconcile(&base_tables);
        let conditional = (0..thresholds.len())
            .map(|band| {
                let tables: Vec<ModelRates> = schedules
                    .iter()
                    .map(|schedule| schedule.conditional()[band].rates)
                    .collect();
                ConditionalRate {
                    min_prompt_tokens: thresholds[band],
                    rates: reconcile(&tables),
                }
            })
            .collect();
        Some(RateSchedule::new(base, conditional))
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

    // -----------------------------------------------------------------------
    // Conditional rates: which table applies, and what a reservation must
    // assume when nothing has been measured yet.
    // -----------------------------------------------------------------------

    fn rates(input: f64, cached: Option<f64>, output: f64) -> ModelRates {
        ModelRates {
            cache_write_per_mtok: None,
            input_per_mtok: Some(input),
            cached_input_per_mtok: cached,
            output_per_mtok: Some(output),
        }
    }

    /// `openai/gpt-5.6-luna` as the vendor publishes it: 0.2/0.02/1.2 up to
    /// 272,000 prompt tokens, 0.4/0.04/1.8 from there.
    fn luna() -> RateSchedule {
        RateSchedule::new(
            rates(0.2, Some(0.02), 1.2),
            vec![ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: rates(0.4, Some(0.04), 1.8),
            }],
        )
    }

    #[test]
    fn a_flat_schedule_answers_its_base_table_to_every_question() {
        // The backwards-compatibility guarantee, stated as a test rather than
        // left to inspection: a catalog written before conditional rates
        // existed must price EXACTLY as it did, so both selectors have to
        // return the base table itself — the same `Option` shape included, not
        // merely a table that happens to bill the same.
        let base = rates(2.0, None, 12.0);
        let schedule = RateSchedule::flat(base);
        for prompt_tokens in [0, 1, 271_999, 272_000, 1_000_000, u64::MAX] {
            assert_eq!(schedule.at_prompt_tokens(prompt_tokens), base);
        }
        assert_eq!(schedule.worst_case(), base);
        assert!(schedule.is_flat());
        // An absent cached rate must stay absent. Synthesising the input rate
        // into it would price identically but would change what the public
        // catalog advertises, which reads this same table.
        assert_eq!(schedule.worst_case().cached_input_per_mtok, None);
    }

    #[test]
    fn the_threshold_is_a_minimum_and_includes_its_own_boundary() {
        // `min_prompt_tokens` is a MINIMUM, so the comparison is `>=` and a
        // request measuring precisely the boundary is priced in the band
        // above. One token below is the last request at the base rate.
        let schedule = luna();
        assert_eq!(
            schedule.at_prompt_tokens(271_999),
            rates(0.2, Some(0.02), 1.2),
            "one token below the boundary is still a base-rate request"
        );
        assert_eq!(
            schedule.at_prompt_tokens(272_000),
            rates(0.4, Some(0.04), 1.8),
            "the threshold is a minimum: exactly the boundary reprices"
        );
        assert_eq!(
            schedule.at_prompt_tokens(272_001),
            rates(0.4, Some(0.04), 1.8)
        );
    }

    #[test]
    fn every_dimension_reprices_for_the_whole_request_and_nothing_is_blended() {
        // The step, restated as arithmetic. A schedule holds complete tables,
        // so the band above the boundary carries its own output rate as well
        // as its own input rate, and there is no combination of the two tables
        // that any prompt size produces.
        let schedule = luna();
        let high = schedule.at_prompt_tokens(300_000);
        assert_eq!(high.input_per_mtok, Some(0.4));
        assert_eq!(high.output_per_mtok, Some(1.8));
        assert_eq!(high.cached_input_per_mtok, Some(0.04));
        for prompt_tokens in [0, 100, 271_999, 272_000, 300_000, u64::MAX] {
            let applied = schedule.at_prompt_tokens(prompt_tokens);
            assert!(
                applied == rates(0.2, Some(0.02), 1.2) || applied == rates(0.4, Some(0.04), 1.8),
                "a prompt of {prompt_tokens} produced a blended table {applied:?}"
            );
        }
    }

    #[test]
    fn the_highest_threshold_at_or_below_the_prompt_wins() {
        // Multiple bands are supported in principle even though no shipped
        // upstream publishes more than one, and the rule is "the HIGHEST
        // threshold the prompt has reached" rather than "the first one that
        // matches".
        //
        // Both declaration orders are exercised, and that is not thoroughness
        // for its own sake. With the bands written high-first, "first match"
        // and "highest match" agree on every input, so a schedule declared
        // that way cannot tell a correct implementation from one that returns
        // whichever band it happens to see first. Only the ascending order —
        // which is the order the file is required to be written in — separates
        // them. Asserting both also pins that selection is independent of the
        // ordering, which is what lets validation own legibility rather than
        // correctness.
        let low = ConditionalRate {
            min_prompt_tokens: 100_000,
            rates: rates(2.0, Some(0.2), 10.0),
        };
        let high = ConditionalRate {
            min_prompt_tokens: 500_000,
            rates: rates(4.0, Some(0.4), 20.0),
        };
        for bands in [vec![low, high], vec![high, low]] {
            let order: Vec<u64> = bands.iter().map(|b| b.min_prompt_tokens).collect();
            let schedule = RateSchedule::new(rates(1.0, Some(0.1), 5.0), bands);
            for (prompt_tokens, expected) in [
                (0, rates(1.0, Some(0.1), 5.0)),
                (99_999, rates(1.0, Some(0.1), 5.0)),
                (100_000, rates(2.0, Some(0.2), 10.0)),
                (499_999, rates(2.0, Some(0.2), 10.0)),
                (500_000, rates(4.0, Some(0.4), 20.0)),
                (u64::MAX, rates(4.0, Some(0.4), 20.0)),
            ] {
                assert_eq!(
                    schedule.at_prompt_tokens(prompt_tokens),
                    expected,
                    "wrong band at {prompt_tokens} prompt tokens, declared {order:?}"
                );
            }
        }
    }

    #[test]
    fn the_worst_case_is_the_dearest_rate_on_every_dimension_independently() {
        // What a reservation is sized at. Taking the highest threshold's table
        // wholesale would under-reserve a schedule whose top band raises one
        // dimension and lowers another — nothing forbids that shape — so the
        // maximum is per dimension and can never come out below the table that
        // ends up applying.
        let schedule = RateSchedule::new(
            rates(1.0, Some(0.5), 20.0),
            vec![ConditionalRate {
                min_prompt_tokens: 200_000,
                rates: rates(4.0, Some(0.1), 8.0),
            }],
        );
        assert_eq!(
            schedule.worst_case(),
            ModelRates {
                // Neither band declares a cache-write rate, so both contribute
                // their own INPUT rate through the effective fallback and the
                // dearest of those wins — the same rule the cached dimension
                // has always followed here, and for the same reason: it is what
                // `usage_cost` will bill a written token at.
                cache_write_per_mtok: Some(4.0),
                ..rates(4.0, Some(0.5), 20.0)
            }
        );
        for prompt_tokens in [0, 199_999, 200_000, u64::MAX] {
            let applied = schedule.at_prompt_tokens(prompt_tokens);
            let worst = schedule.worst_case();
            assert!(
                applied.input_per_mtok <= worst.input_per_mtok
                    && applied.output_per_mtok <= worst.output_per_mtok
                    && applied.effective_cached_input_per_mtok()
                        <= worst.effective_cached_input_per_mtok()
                    && applied.effective_cache_write_per_mtok()
                        <= worst.effective_cache_write_per_mtok(),
                "the band at {prompt_tokens} is dearer than the worst case on some dimension"
            );
        }
    }

    #[test]
    fn a_band_that_omits_its_cached_rate_is_worst_cased_at_that_bands_input_rate() {
        // An absent cached rate is not "no cached charge": `usage_cost` bills
        // it at the same table's input rate. A worst case that skipped the
        // dimension would reserve 0.02 for cached tokens that will bill at
        // 0.40, which is an under-reservation on precisely the long, heavily
        // cached requests this whole feature is about.
        let schedule = RateSchedule::new(
            rates(0.2, Some(0.02), 1.2),
            vec![ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: rates(0.4, None, 1.8),
            }],
        );
        assert_eq!(schedule.worst_case().cached_input_per_mtok, Some(0.4));
    }

    #[test]
    fn a_conditional_band_that_charges_stops_the_schedule_being_free() {
        // The free lane must not be reachable through a schedule that gives
        // its base rate away and bills past a threshold: the skipped path
        // writes no reservation and no ledger row, so a long request would be
        // delivered with nothing to charge it against.
        let free_base_paid_band = RateSchedule::new(
            rates(0.0, Some(0.0), 0.0),
            vec![ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: rates(5.0, Some(0.5), 30.0),
            }],
        );
        assert!(!free_base_paid_band.are_zero());
        // ... while a schedule that is zero everywhere still is free.
        let free_throughout = RateSchedule::new(
            rates(0.0, None, 0.0),
            vec![ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: rates(0.0, None, 0.0),
            }],
        );
        assert!(free_throughout.are_zero());
    }

    #[test]
    fn a_conditional_band_claiming_free_required_rates_is_visible_to_the_zero_price_rule() {
        // `validate_zero_price` asks the suspicious question — "does any table
        // here claim the traffic every request generates is free" — and a
        // conditional table makes that claim just as loudly as a base one.
        let paid_base_free_band = RateSchedule::new(
            rates(5.0, Some(0.5), 30.0),
            vec![ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: rates(0.0, None, 0.0),
            }],
        );
        assert!(paid_base_free_band.any_required_rates_are_zero());
        assert!(
            !paid_base_free_band.are_zero(),
            "it is not free — which is exactly why the weaker question is the one that refuses it"
        );
        assert!(!luna().any_required_rates_are_zero());
    }

    // -----------------------------------------------------------------------
    // Cache writes: the fourth dimension, and the two different jobs its
    // absence does.
    // -----------------------------------------------------------------------

    /// A rate table with a cache-write price, at Anthropic's 1.25x multiplier.
    fn writing(input: f64, cached: f64, output: f64) -> ModelRates {
        ModelRates {
            cache_write_per_mtok: Some(input * 1.25),
            ..rates(input, Some(cached), output)
        }
    }

    #[test]
    fn a_lane_that_does_not_price_cache_writes_is_incapable_of_them() {
        // The capability question is asked on the DECLARED field and nowhere
        // else. This is what the admission gate reads, so an absent rate has
        // to answer "no" here even though the biller will happily charge such
        // a lane's writes at its input rate.
        assert!(!rates(2.0, Some(0.2), 10.0).prices_cache_writes());
        assert!(writing(2.0, 0.2, 10.0).prices_cache_writes());
        // ...and the same question at schedule level, where it must hold in
        // EVERY band. A schedule that declares the price at the base and drops
        // it above a threshold would sell long cache-writing requests at a
        // rate nobody chose, so it does not count as capable.
        let base_only = RateSchedule::new(
            writing(2.0, 0.2, 10.0),
            vec![ConditionalRate {
                min_prompt_tokens: 200_000,
                rates: rates(4.0, Some(0.4), 18.0),
            }],
        );
        assert!(
            !base_only.prices_cache_writes(),
            "a band that omits the dimension makes the whole schedule incapable"
        );
        assert!(RateSchedule::flat(writing(2.0, 0.2, 10.0)).prices_cache_writes());
    }

    #[test]
    fn an_absent_cache_write_rate_still_bills_at_the_input_rate() {
        // The other half of the same absence, and the half that keeps this
        // change from costing anybody money. `usage_cost` reads the EFFECTIVE
        // value, so a lane that declares nothing charges input for a write —
        // which is exactly what it charged before the dimension existed. The
        // alternative reading, "absent means free", would give the Anthropic
        // wire's own three breakpoints away on every lane that has not
        // transcribed a price.
        let silent = rates(2.0, Some(0.2), 10.0);
        assert_eq!(silent.cache_write_per_mtok, None);
        assert_eq!(silent.effective_cache_write_per_mtok(), Some(2.0));
        // A table with no input rate has nothing to fall back to, and says so
        // rather than inventing a zero.
        assert_eq!(ModelRates::default().effective_cache_write_per_mtok(), None);
    }

    #[test]
    fn a_conditional_band_carries_its_own_cache_write_rate() {
        // A band REPLACES the base table wholesale, on this dimension like the
        // other three, and the band that applies is selected by prompt size in
        // the usual way.
        let schedule = RateSchedule::new(
            writing(2.0, 0.2, 10.0),
            vec![ConditionalRate {
                min_prompt_tokens: 200_000,
                rates: writing(4.0, 0.4, 18.0),
            }],
        );
        assert_eq!(
            schedule.at_prompt_tokens(199_999).cache_write_per_mtok,
            Some(2.5)
        );
        assert_eq!(
            schedule.at_prompt_tokens(200_000).cache_write_per_mtok,
            Some(5.0)
        );
        // And the worst case is the dearest of them, per dimension.
        assert_eq!(schedule.worst_case().cache_write_per_mtok, Some(5.0));
    }

    #[test]
    fn a_reservation_is_sized_at_the_dearest_rate_a_prompt_token_could_carry() {
        // THE reservation-sufficiency property, stated as arithmetic.
        //
        // Every prompt token in a reservation is priced at `input_per_mtok`,
        // because the reservation's usage carries no cache detail. Settlement
        // splits the real prompt and can charge a write rate ABOVE the input
        // rate — the first prompt-side rate that has ever been allowed above
        // it — and the debit is clamped to the reservation. So the reserved
        // input rate has to be the maximum over all three prompt-side rates,
        // or a fully cache-writing request settles above its own hold and
        // ZeroRouter eats the difference.
        let schedule = RateSchedule::flat(writing(2.0, 0.2, 10.0));
        assert_eq!(schedule.worst_case().input_per_mtok, Some(2.0));
        assert_eq!(
            schedule.reservation_rates().input_per_mtok,
            Some(2.5),
            "the hold prices the prompt bound at the cache-write rate, not the input rate"
        );
        // Output is untouched: an output token has exactly one rate.
        assert_eq!(schedule.reservation_rates().output_per_mtok, Some(10.0));

        // The dominance property the clamp depends on, checked against every
        // band a request could land in and every way its prompt could split.
        for schedule in [
            RateSchedule::flat(writing(2.0, 0.2, 10.0)),
            RateSchedule::new(
                writing(2.0, 0.2, 10.0),
                vec![ConditionalRate {
                    min_prompt_tokens: 200_000,
                    rates: writing(4.0, 0.4, 18.0),
                }],
            ),
        ] {
            let reserved = schedule.reservation_rates().input_per_mtok;
            for prompt_tokens in [0, 199_999, 200_000, u64::MAX] {
                let applied = schedule.at_prompt_tokens(prompt_tokens);
                for settled in [
                    applied.input_per_mtok,
                    applied.effective_cached_input_per_mtok(),
                    applied.effective_cache_write_per_mtok(),
                ] {
                    assert!(
                        settled <= reserved,
                        "a prompt token at {prompt_tokens} can settle at {settled:?}, above the \
                         {reserved:?} the reservation held for it"
                    );
                }
            }
        }
    }

    #[test]
    fn a_catalog_without_cache_write_rates_reserves_exactly_what_it_always_did() {
        // The backwards-compatibility guarantee for the reservation change,
        // stated as a test rather than left to inspection: on every schedule
        // that declares no cache-write rate, `reservation_rates` must equal
        // `worst_case` — the same `Option` shape included, not merely a table
        // that happens to hold the same amount.
        for schedule in [
            RateSchedule::flat(rates(2.0, None, 10.0)),
            RateSchedule::flat(rates(2.0, Some(0.2), 10.0)),
            luna(),
        ] {
            assert_eq!(
                schedule.reservation_rates(),
                schedule.worst_case(),
                "a lane that does not sell cache writes must not hold a cent more"
            );
        }
    }

    #[test]
    fn a_declared_cache_write_rate_stops_a_schedule_being_free() {
        // Same rule as the cached dimension: a DECLARED nonzero rate is money,
        // and the free lane writes no reservation and no ledger row, so a
        // schedule that bills for writes must never reach it.
        let free_except_writes = ModelRates {
            cache_write_per_mtok: Some(1.0),
            ..rates(0.0, Some(0.0), 0.0)
        };
        assert!(!free_except_writes.are_zero());
        assert!(
            free_except_writes.required_rates_are_zero(),
            "which is exactly why the weaker question is not the one that refuses it"
        );
        // An OMITTED write rate over a zero input rate is still free, so the
        // commonest honest local config does not have to write `= 0` for a
        // dimension its server has never heard of.
        assert!(rates(0.0, None, 0.0).are_zero());
    }

    #[test]
    fn the_usage_contract_is_cached_as_a_subset_of_input() {
        // The one invariant every wire must normalize to, restated here
        // because this module is now its home rather than a git pin.
        let usage = TokenUsage {
            cache_write_input_tokens: None,
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
    fn cached_and_written_tokens_are_disjoint_subsets_of_the_prompt() {
        // The contract the Anthropic wire normalizes into, restated where the
        // type lives. Three buckets, and `input_tokens` is their SUM — not the
        // fresh portion, which is what Anthropic itself reports and what the
        // wire deliberately converts away from.
        let usage = TokenUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(50),
            cached_input_tokens: Some(600),
            cache_write_input_tokens: Some(300),
        };
        let cached = usage.cached_input_tokens.unwrap();
        let written = usage.cache_write_input_tokens.unwrap();
        assert!(
            cached + written <= usage.input_tokens.unwrap(),
            "the two cache buckets are disjoint and both sit inside the prompt"
        );
        assert_eq!(
            usage.input_tokens.unwrap() - cached - written,
            100,
            "the fresh portion is what neither bucket claimed"
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

    // -----------------------------------------------------------------------
    // Dearer-rate reconciliation across same-model twins (`dearest_across`),
    // the arithmetic under a unified id's one sell schedule (option ii).
    // -----------------------------------------------------------------------

    /// A full four-dimension table, so the optional cache dimensions can be set
    /// or omitted independently — which `rates` (cache_write always `None`)
    /// cannot express.
    fn full(input: f64, cached: Option<f64>, cache_write: Option<f64>, output: f64) -> ModelRates {
        ModelRates {
            input_per_mtok: Some(input),
            cached_input_per_mtok: cached,
            cache_write_per_mtok: cache_write,
            output_per_mtok: Some(output),
        }
    }

    #[test]
    fn dearest_across_takes_the_per_dimension_max_on_matching_structure() {
        // The `claude-haiku-4-5` twin: bedrock (dearer, a uniform 1.10x) and
        // anthropic. The reconciled schedule is bedrock's, dimension by
        // dimension — the DEARER rate everywhere, so neither lane is undercut.
        let bedrock = RateSchedule::flat(full(1.10, Some(0.11), Some(1.375), 5.50));
        let anthropic = RateSchedule::flat(full(1.00, Some(0.10), Some(1.25), 5.00));
        let reconciled = RateSchedule::dearest_across(&[&bedrock, &anthropic])
            .expect("two flat schedules share the empty band structure");
        assert_eq!(
            reconciled, bedrock,
            "dearest per dimension is bedrock's whole table"
        );
        // Symmetric in argument order — the max does not depend on which pin the
        // catalog happened to list first.
        assert_eq!(
            RateSchedule::dearest_across(&[&anthropic, &bedrock]),
            Some(bedrock.clone())
        );
        // A crossed schedule (each dearer in a DIFFERENT dimension) takes the
        // max of each dimension, never either input table wholesale.
        let a = RateSchedule::flat(full(2.00, Some(0.20), Some(2.50), 4.00));
        let b = RateSchedule::flat(full(1.00, Some(0.50), Some(1.25), 9.00));
        let crossed = RateSchedule::dearest_across(&[&a, &b]).expect("flat pair reconciles");
        assert_eq!(crossed.base(), full(2.00, Some(0.50), Some(2.50), 9.00));
    }

    #[test]
    fn dearest_across_is_none_on_a_mismatched_band_structure() {
        // Flat vs banded: different band COUNT, no common band to reconcile.
        let flat = RateSchedule::flat(full(1.00, Some(0.10), None, 5.00));
        assert!(
            RateSchedule::dearest_across(&[&flat, &luna()]).is_none(),
            "a flat schedule and a banded one share no common band structure"
        );
        // Same band count but a DIFFERENT threshold: still no shared structure.
        let banded_a = RateSchedule::new(
            rates(1.0, Some(0.1), 5.0),
            vec![ConditionalRate {
                min_prompt_tokens: 200_000,
                rates: rates(2.0, Some(0.2), 10.0),
            }],
        );
        let banded_b = RateSchedule::new(
            rates(1.0, Some(0.1), 5.0),
            vec![ConditionalRate {
                min_prompt_tokens: 300_000,
                rates: rates(2.0, Some(0.2), 10.0),
            }],
        );
        assert!(
            RateSchedule::dearest_across(&[&banded_a, &banded_b]).is_none(),
            "same band count, different thresholds — not a shared structure"
        );
        // IDENTICAL thresholds DO reconcile, per band, taking each band's
        // per-dimension max.
        let banded_c = RateSchedule::new(
            rates(1.5, Some(0.3), 4.0),
            vec![ConditionalRate {
                min_prompt_tokens: 200_000,
                rates: rates(3.0, Some(0.5), 8.0),
            }],
        );
        let reconciled = RateSchedule::dearest_across(&[&banded_a, &banded_c])
            .expect("identical thresholds share a structure");
        assert_eq!(reconciled.thresholds().collect::<Vec<_>>(), vec![200_000]);
        assert_eq!(reconciled.base(), rates(1.5, Some(0.3), 5.0));
        assert_eq!(
            reconciled.at_prompt_tokens(200_000),
            rates(3.0, Some(0.5), 10.0)
        );
    }

    #[test]
    fn dearest_across_prices_an_omitted_optional_at_its_effective_rate() {
        // A member that OMITS cache_write does not price writes at nothing — it
        // prices them at its own input rate (`usage_cost`'s fallback). When that
        // input rate is dearer than the OTHER member's declared premium, the
        // reconciled rate must be the omitting member's input, or the unified id
        // would undercharge the omitting lane. Max-of-declared ("present wins")
        // would pick the lower declared 1.25 and undercut the omitting lane's
        // effective 2.00.
        let omits = RateSchedule::flat(full(2.00, None, None, 5.00));
        let declares = RateSchedule::flat(full(1.00, Some(0.10), Some(1.25), 5.00));
        let reconciled =
            RateSchedule::dearest_across(&[&omits, &declares]).expect("flat pair reconciles");
        // cache_write is DECLARED on the result (one member declares it) and is
        // the dearer EFFECTIVE value across both: max(2.00 fallback, 1.25) = 2.00.
        assert_eq!(reconciled.base().cache_write_per_mtok, Some(2.00));
        // cached_input likewise: `omits` bills cached at its 2.00 input,
        // `declares` at 0.10; the dearer effective is 2.00.
        assert_eq!(reconciled.base().cached_input_per_mtok, Some(2.00));

        // When NEITHER member declares an optional, it stays ABSENT: the input
        // fallback already yields the same billed rate, and leaving it absent
        // preserves the "this lane does not price cache writes" capability.
        let neither_a = RateSchedule::flat(full(2.00, None, None, 4.00));
        let neither_b = RateSchedule::flat(full(1.00, None, None, 9.00));
        let none_declared =
            RateSchedule::dearest_across(&[&neither_a, &neither_b]).expect("flat pair reconciles");
        assert_eq!(none_declared.base().cache_write_per_mtok, None);
        assert_eq!(none_declared.base().cached_input_per_mtok, None);
        assert!(!none_declared.prices_cache_writes());
    }

    #[test]
    fn dearest_across_never_undercharges_any_member_in_any_dimension() {
        // The invariant option ii rests on, asserted DIRECTLY over the effective
        // (billed) rate of every dimension: the reconciled schedule is >= every
        // member in every dimension `usage_cost` prices, so whichever provider
        // serves under the one unified sell rate, the customer is never charged
        // less than that provider's own pin would charge.
        let members = [
            RateSchedule::flat(full(1.10, Some(0.11), Some(1.375), 5.50)),
            RateSchedule::flat(full(1.00, Some(0.10), Some(1.25), 5.00)),
            RateSchedule::flat(full(0.90, None, None, 6.00)),
        ];
        let refs: Vec<&RateSchedule> = members.iter().collect();
        let unified = RateSchedule::dearest_across(&refs)
            .expect("flat members share the empty band structure");
        let u = unified.base();
        for member in &members {
            let m = member.base();
            assert!(u.input_per_mtok.unwrap() >= m.input_per_mtok.unwrap());
            assert!(u.output_per_mtok.unwrap() >= m.output_per_mtok.unwrap());
            assert!(
                u.effective_cached_input_per_mtok().unwrap()
                    >= m.effective_cached_input_per_mtok().unwrap(),
                "cached undercharged: {u:?} vs member {m:?}"
            );
            assert!(
                u.effective_cache_write_per_mtok().unwrap()
                    >= m.effective_cache_write_per_mtok().unwrap(),
                "cache_write undercharged: {u:?} vs member {m:?}"
            );
        }
    }

    #[test]
    fn dearest_across_of_a_single_schedule_is_that_schedule() {
        // Degenerate but well-defined: the max over one member is that member.
        let one = luna();
        assert_eq!(RateSchedule::dearest_across(&[&one]), Some(one));
        // And an empty slice reconciles to nothing.
        assert!(RateSchedule::dearest_across(&[]).is_none());
    }
}

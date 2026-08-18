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

/// One CONDITIONAL rate table: what applies once a request's prompt reaches
/// [`Self::min_prompt_tokens`].
///
/// `min_prompt_tokens` is a minimum, exactly as the name says: the comparison
/// is `>=`, so a request measuring precisely the threshold is priced HERE and
/// not at the table below. See [`RateSchedule::at_prompt_tokens`] for what is
/// being measured and why.
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
        }
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
        assert_eq!(schedule.worst_case(), rates(4.0, Some(0.5), 20.0));
        for prompt_tokens in [0, 199_999, 200_000, u64::MAX] {
            let applied = schedule.at_prompt_tokens(prompt_tokens);
            let worst = schedule.worst_case();
            assert!(
                applied.input_per_mtok <= worst.input_per_mtok
                    && applied.output_per_mtok <= worst.output_per_mtok
                    && applied.effective_cached_input_per_mtok()
                        <= worst.effective_cached_input_per_mtok(),
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

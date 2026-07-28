use std::collections::BTreeMap;

use chrono::Utc;
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zeroclaw_api::tool::ToolSpec;
use zeroclaw_providers::{
    pricing::ModelRates,
    traits::{ChatMessage, ChatResponse, TokenUsage, ToolCall},
};

use crate::priority::Priority;

#[derive(Clone, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Vec<OpenAiTool>,
    pub tool_choice: Option<Value>,
    pub stream_options: Option<StreamOptionsRequest>,
    // Typed and named BEFORE the flatten, so serde consumes the key and it
    // never lands in `extra` — the one namespaced field ZeroRouter owns on an
    // otherwise OpenAI-shaped request (design doc: "The priority knob").
    pub zerorouter: Option<ZeroRouterRequestOptions>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// ZeroRouter's own request namespace, strictly validated where the
/// OpenAI-compat surface is strictly rejected: `deny_unknown_fields` makes a
/// typo like `"priorty"` a loud 400 through the same deserialization error
/// any malformed body takes, without touching
/// [`ChatCompletionRequest::contains_unsupported_extensions`]. Before this
/// field existed, a request carrying `zerorouter` landed in `extra` and was
/// 400-rejected as an unsupported field — so the typed object cannot change
/// the meaning of any request that worked before it.
///
/// Stage 3a carries `priority` alone. `validator` and `budget_usd` are
/// stage-5a fields; until then `deny_unknown_fields` keeps them loud 400s
/// rather than silently accepted no-ops.
#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZeroRouterRequestOptions {
    pub priority: Option<Priority>,
}

#[derive(Clone, Deserialize)]
pub struct StreamOptionsRequest {
    #[serde(default)]
    pub include_usage: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Vec<OpenAiToolCall>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Deserialize)]
pub struct OpenAiTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiFunctionSpec,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Deserialize)]
pub struct OpenAiFunctionSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_object")]
    pub parameters: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: OpenAiFunctionCall,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct OpenAiFunctionCall {
    pub name: String,
    pub arguments: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn empty_object() -> Value {
    json!({})
}

fn function_kind() -> String {
    "function".to_owned()
}

impl ChatCompletionRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model.trim().is_empty() {
            return Err("model is empty");
        }
        if self.messages.is_empty() {
            return Err("messages is empty");
        }
        if self.messages.iter().any(|message| {
            !matches!(
                message.role.as_str(),
                "system" | "user" | "assistant" | "tool"
            )
        }) {
            return Err("message role is unsupported");
        }
        if self
            .tools
            .iter()
            .any(|tool| tool.kind != "function" || tool.function.name.trim().is_empty())
        {
            return Err("tool definition is invalid");
        }
        if self
            .temperature
            .is_some_and(|temperature| !temperature.is_finite())
        {
            return Err("temperature must be finite");
        }
        if self.max_tokens == Some(0) {
            return Err("max_tokens must be positive");
        }
        if self
            .tool_choice
            .as_ref()
            .is_some_and(|choice| choice.as_str() != Some("auto"))
        {
            return Err("only tool_choice auto is supported");
        }
        if self.tool_choice.is_some() && self.tools.is_empty() {
            return Err("tool_choice requires tools");
        }
        for message in &self.messages {
            if !message.tool_calls.is_empty() && message.role != "assistant" {
                return Err("tool_calls are only valid on assistant messages");
            }
            if message.tool_calls.iter().any(|call| {
                call.id.trim().is_empty()
                    || call.kind != "function"
                    || call.function.name.trim().is_empty()
            }) {
                return Err("tool call is invalid");
            }
            if message.role == "tool"
                && message
                    .tool_call_id
                    .as_deref()
                    .is_none_or(|id| id.trim().is_empty())
            {
                return Err("tool messages require tool_call_id");
            }
            if message.role != "tool" && message.tool_call_id.is_some() {
                return Err("tool_call_id is only valid on tool messages");
            }
            if message.name.is_some() && message.role != "tool" {
                return Err("message name cannot be preserved for this role");
            }
            if message.reasoning_content.is_some() && message.role != "assistant" {
                return Err("reasoning_content cannot be preserved for this message");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn provider_messages(&self) -> Vec<ChatMessage> {
        self.messages.iter().map(to_provider_message).collect()
    }

    #[must_use]
    pub fn provider_tools(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|tool| {
                ToolSpec::new(
                    tool.function.name.clone(),
                    tool.function.description.clone(),
                    tool.function.parameters.clone(),
                )
            })
            .collect()
    }

    #[must_use]
    pub fn include_stream_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .is_some_and(|options| options.include_usage)
    }

    #[must_use]
    pub fn contains_cache_control(&self) -> bool {
        self.extra.contains_key("cache_control")
            || self.messages.iter().any(|message| {
                message.extra.contains_key("cache_control")
                    || message.content.as_array().is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.as_object()
                                .is_some_and(|part| part.contains_key("cache_control"))
                        })
                    })
            })
            || self
                .tools
                .iter()
                .any(|tool| tool.extra.contains_key("cache_control"))
    }

    #[must_use]
    pub fn contains_unsupported_extensions(&self) -> bool {
        self.extra.keys().any(|key| key != "cache_control")
            || self
                .stream_options
                .as_ref()
                .is_some_and(|options| !options.extra.is_empty())
            || self.messages.iter().any(|message| {
                (!message.content.is_string() && !message.content.is_null())
                    || message.extra.keys().any(|key| key != "cache_control")
                    || message
                        .tool_calls
                        .iter()
                        .any(|call| !call.extra.is_empty() || !call.function.extra.is_empty())
            })
            || self.tools.iter().any(|tool| {
                tool.extra.keys().any(|key| key != "cache_control")
                    || !tool.function.extra.is_empty()
            })
    }

    #[must_use]
    pub fn reservation_usage(&self, max_output_tokens: u32) -> OpenAiUsage {
        let mut input_bound = 0_u64;
        for message in &self.messages {
            input_bound = input_bound
                .saturating_add(64)
                .saturating_add(byte_len(&message.role))
                .saturating_add(value_byte_len(&message.content))
                .saturating_add(message.tool_call_id.as_deref().map_or(0, byte_len))
                .saturating_add(message.name.as_deref().map_or(0, byte_len))
                .saturating_add(message.reasoning_content.as_deref().map_or(0, byte_len));
            for call in &message.tool_calls {
                input_bound = input_bound
                    .saturating_add(64)
                    .saturating_add(byte_len(&call.id))
                    .saturating_add(byte_len(&call.function.name))
                    .saturating_add(byte_len(&call.function.arguments));
            }
        }
        for tool in &self.tools {
            input_bound = input_bound
                .saturating_add(128)
                .saturating_add(byte_len(&tool.function.name))
                .saturating_add(byte_len(&tool.function.description))
                .saturating_add(value_byte_len(&tool.function.parameters));
        }
        let output = u64::from(max_output_tokens);
        OpenAiUsage {
            prompt_tokens: input_bound,
            completion_tokens: output,
            total_tokens: input_bound.saturating_add(output),
            prompt_tokens_details: None,
        }
    }
}

fn byte_len(value: &str) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX)
}

fn value_byte_len(value: &Value) -> u64 {
    match value {
        Value::String(value) => byte_len(value),
        Value::Null => 0,
        value => byte_len(&value.to_string()),
    }
}

fn to_provider_message(message: &OpenAiMessage) -> ChatMessage {
    match message.role.as_str() {
        "assistant" if !message.tool_calls.is_empty() => {
            let tool_calls = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    })
                })
                .collect::<Vec<_>>();
            ChatMessage::assistant(
                json!({
                    "content": content_to_text(&message.content),
                    "tool_calls": tool_calls,
                    "reasoning_content": message.reasoning_content,
                })
                .to_string(),
            )
        }
        "assistant" if message.reasoning_content.is_some() => ChatMessage::assistant(
            json!({
                "content": content_to_text(&message.content),
                "reasoning_content": message.reasoning_content,
            })
            .to_string(),
        ),
        "tool" => ChatMessage::tool(
            json!({
                "tool_call_id": message.tool_call_id,
                "name": message.name,
                "content": content_to_text(&message.content),
            })
            .to_string(),
        ),
        "system" => ChatMessage::system(content_to_text(&message.content)),
        "assistant" => ChatMessage::assistant(content_to_text(&message.content)),
        _ => ChatMessage::user(content_to_text(&message.content)),
    }
}

fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("image_url") => part
                    .get("image_url")
                    .and_then(|image| image.get("url"))
                    .and_then(Value::as_str)
                    .map(|url| format!("[IMAGE:{url}]")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[derive(Debug, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
    pub pricing: ModelPricing,
}

/// OpenRouter-shaped per-token pricing, string-valued. This is ZeroClaw's
/// consumed wire contract (`ModelPricing`, `crates/zeroclaw-api/src/model_provider.rs`
/// in the zeroclaw tree): every field is an `Option<String>` decimal USD rate
/// *per single token* — a numeric JSON value fails that struct's serde, so
/// these must stay strings. Only the fields ZeroClaw's pricing normalizer
/// (`zeroclaw-providers/src/pricing.rs::normalize_pricing`) actually reads
/// are emitted: `prompt`, `completion`, and `input_cache_read`.
/// `input_cache_write` is part of ZeroClaw's contract but that normalizer
/// never reads it, so ZR never populates it.
#[derive(Debug, Serialize)]
pub struct ModelPricing {
    pub prompt: String,
    pub completion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_read: Option<String>,
}

impl ModelPricing {
    /// Convert per-1M-token sell rates (`config/tiers.toml`) into
    /// OpenRouter's per-single-token decimal-string convention. Division by
    /// `1_000_000` happens in `Decimal`, never `f64`, and the result is
    /// trailing-zero-normalized before rendering, so the wire value a
    /// customer reads never carries a binary-float artifact.
    #[must_use]
    pub fn from_sell_rates(rates: ModelRates) -> Self {
        Self {
            prompt: per_token_price(rates.input_per_mtok.unwrap_or(0.0)),
            completion: per_token_price(rates.output_per_mtok.unwrap_or(0.0)),
            input_cache_read: rates.cached_input_per_mtok.map(per_token_price),
        }
    }
}

fn per_token_price(rate_per_mtok: f64) -> String {
    let per_mtok = Decimal::from_f64(rate_per_mtok).unwrap_or(Decimal::ZERO);
    (per_mtok / Decimal::from(1_000_000_u64))
        .normalize()
        .to_string()
}

impl ModelList {
    #[must_use]
    pub fn from_listing(listing: BTreeMap<String, crate::config::ModelListing>) -> Self {
        Self {
            object: "list",
            data: listing
                .into_iter()
                .map(|(id, row)| ModelObject {
                    id,
                    object: "model",
                    created: 0,
                    owned_by: row.owned_by,
                    pricing: ModelPricing::from_sell_rates(row.sell_rates),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: OpenAiUsage,
}

#[derive(Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub message: AssistantMessage,
    pub finish_reason: &'static str,
}

#[derive(Serialize)]
pub struct AssistantMessage {
    pub role: &'static str,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OpenAiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokenDetails>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PromptTokenDetails {
    pub cached_tokens: u64,
}

impl OpenAiUsage {
    #[must_use]
    pub fn try_from_provider(usage: Option<&TokenUsage>) -> Option<Self> {
        let usage = usage?;
        let input = usage.input_tokens?;
        let output = usage.output_tokens?;
        // A real completion always consumes prompt tokens; an all-zero report
        // is unusable, so route it through the missing-usage path rather than
        // metering a request as free.
        if input == 0 && output == 0 {
            return None;
        }
        let cached = usage.cached_input_tokens.unwrap_or(0).min(input);
        Some(Self {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: input.saturating_add(output),
            prompt_tokens_details: (cached > 0).then_some(PromptTokenDetails {
                cached_tokens: cached,
            }),
        })
    }

    #[must_use]
    pub fn from_provider(usage: Option<&TokenUsage>) -> Self {
        Self::try_from_provider(usage).unwrap_or_default()
    }

    #[must_use]
    pub fn cached_input_tokens(self) -> u64 {
        self.prompt_tokens_details
            .map_or(0, |details| details.cached_tokens)
    }
}

impl ChatCompletionResponse {
    #[must_use]
    pub fn new(
        request_id: String,
        model: String,
        response: ChatResponse,
        usage: OpenAiUsage,
        max_tokens: Option<u32>,
    ) -> Self {
        let has_tools = !response.tool_calls.is_empty();
        let tool_calls = response
            .tool_calls
            .into_iter()
            .map(tool_call_to_openai)
            .collect();
        Self {
            id: request_id,
            object: "chat.completion",
            created: Utc::now().timestamp(),
            model,
            choices: vec![CompletionChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant",
                    content: response.text,
                    tool_calls,
                    reasoning_content: response.reasoning_content,
                },
                finish_reason: finish_reason(has_tools, usage, max_tokens),
            }],
            usage,
        }
    }
}

#[must_use]
pub fn finish_reason(
    has_tool_calls: bool,
    usage: OpenAiUsage,
    max_tokens: Option<u32>,
) -> &'static str {
    if has_tool_calls {
        "tool_calls"
    } else if max_tokens.is_some_and(|limit| usage.completion_tokens >= u64::from(limit)) {
        "length"
    } else {
        "stop"
    }
}

/// Which encoding produced a [`TaskSignature::hex`], stamped on every settled
/// row (`usage_events.task_signature_scheme`, migration 0007).
///
/// * **1** — the original scheme. Tool names were joined with `,` before
///   hashing, so one tool named `a,b` and two tools named `a` and `b` produced
///   the same key, and only `tool_count` was persisted, so a signature could
///   not be recomputed from a settled row. Rows carrying scheme 1 have a NULL
///   `task_signature_scheme` (the column predates them).
/// * **2** — length-prefixed tool encoding (see [`tool_names_digest`]), with
///   the scheme number itself in the preimage so a scheme-1 and a scheme-2 key
///   for the same request can never coincide, and the tool digest persisted
///   beside the key.
///
/// Values are NOT comparable across schemes: two rows with the same
/// `task_signature` and different `task_signature_scheme` are different
/// segments, and an estimator must group by the pair.
pub const TASK_SIGNATURE_SCHEME: i16 = 2;

/// The user-scoped request-shape segment key for one request, plus the
/// provenance a later re-key needs.
///
/// Migration 0004 promised signatures could be "re-bucketed — or re-keyed to a
/// pooled scheme — retroactively" from the raw features on the settled row, but
/// the tool dimension was not among them: only `tool_count` was persisted, and
/// a count cannot reproduce a key computed over names. [`tool_names_sha256`] is
/// that missing input, and it is the SAME value [`hex`] is derived from, so a
/// re-key is exact rather than approximate. It is a digest, never the names:
/// tool names are request metadata (like counts and byte sizes), prompt content
/// is not, and neither this type nor anything downstream of it ever carries
/// prompt content.
///
/// [`hex`]: TaskSignature::hex
/// [`tool_names_sha256`]: TaskSignature::tool_names_sha256
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSignature {
    /// First 16 hex chars of the scheme's sha256 — the persisted segment key.
    pub hex: String,
    /// The scheme that produced `hex`; see [`TASK_SIGNATURE_SCHEME`].
    pub scheme: i16,
    /// Full sha256 (64 hex chars) of the canonical tool-name multiset
    /// encoding that fed `hex`.
    pub tool_names_sha256: String,
}

/// Full sha256 of the normalized tool-name multiset: sorted, then
/// **length-prefixed**, so distinct multisets cannot encode identically.
///
/// The previous encoding joined the sorted names with `,` and hashed the
/// result. Tool-name validation only requires a non-empty name, so a single
/// tool named `a,b` produced the byte string `a,b` — exactly what two tools
/// named `a` and `b` produced. Any separator has this defect for some input;
/// prefixing each name with its length removes the separator entirely, so the
/// encoding is injective over the multiset (the count is prefixed too, so the
/// tool section cannot be confused with what follows it).
#[must_use]
pub fn tool_names_digest(tool_names: &[String]) -> String {
    let mut tools: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    tools.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update((tools.len() as u64).to_be_bytes());
    for name in tools {
        hasher.update((name.len() as u64).to_be_bytes());
        hasher.update(name.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// First 16 hex chars of sha256 over the user-scoped request-shape segment
/// key (design: Engine "Task signature"). Keyed per USER — not per API key —
/// so key churn resets nothing, and coarse buckets make request-shape gaming
/// self-defeating. Prompt *content* is never part of the key. Fields (scheme
/// number, tool-name digest, message-count bucket, log2 prompt-bytes bucket,
/// stream flag, log2 requested-max_tokens bucket) mirror the
/// `reservation_usage` walk.
#[must_use]
pub fn task_signature(
    user_id: &str,
    tool_names: &[String],
    message_count: usize,
    prompt_bytes: u64,
    stream: bool,
    requested_max_tokens: u32,
) -> TaskSignature {
    let tool_names_sha256 = tool_names_digest(tool_names);
    let mut hasher = Sha256::new();
    // The scheme number leads the preimage so a key computed under a later
    // encoding can never collide with one computed under this encoding, even
    // if every other field is identical.
    hasher.update(TASK_SIGNATURE_SCHEME.to_be_bytes());
    hasher.update([0x1f]);
    hasher.update(user_id.as_bytes());
    hasher.update([0x1f]);
    hasher.update(tool_names_sha256.as_bytes());
    hasher.update([0x1f]);
    hasher.update(message_count_bucket(message_count).as_bytes());
    hasher.update([0x1f]);
    hasher.update(log2_bucket(prompt_bytes).to_string().as_bytes());
    hasher.update([0x1f]);
    hasher.update(if stream {
        b"1".as_slice()
    } else {
        b"0".as_slice()
    });
    hasher.update([0x1f]);
    hasher.update(
        log2_bucket(u64::from(requested_max_tokens))
            .to_string()
            .as_bytes(),
    );
    let digest = format!("{:x}", hasher.finalize());
    TaskSignature {
        hex: digest[..16].to_owned(),
        scheme: TASK_SIGNATURE_SCHEME,
        tool_names_sha256,
    }
}

fn message_count_bucket(count: usize) -> &'static str {
    match count {
        0 | 1 => "1",
        2..=4 => "2-4",
        5..=16 => "5-16",
        _ => "17+",
    }
}

fn log2_bucket(value: u64) -> u32 {
    if value == 0 {
        0
    } else {
        63 - value.leading_zeros()
    }
}

/// What one attempt's model actually emitted — the evidence the shape label is
/// computed from.
///
/// A completion consists of content text, reasoning content, or tool calls, and
/// any one of the three alone is a non-empty response. This type exists so that
/// every site computing [`shape_ok`] answers "did the model produce anything?"
/// from the *output itself*, and so that the two ways of getting it wrong are
/// unrepresentable at the call site:
///
/// * a response that is entirely `reasoning_content` (thinking models routinely
///   return one) used to read as empty, because only `text` and `tool_calls`
///   were consulted;
/// * a live stream used to infer output from `usage.completion_tokens`, so a
///   stream that emitted nothing while the upstream reported output tokens
///   read as fine. A usage report is the provider's accounting, not a
///   transcript, and it is exactly the signal a success estimator must not be
///   trained on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmittedOutput {
    text: bool,
    reasoning: bool,
    tool_calls: bool,
}

impl EmittedOutput {
    /// The emitted output of a buffered (non-streamed or synthetic-stream)
    /// completion.
    #[must_use]
    pub fn from_response(response: &ChatResponse) -> Self {
        let mut emitted = Self {
            tool_calls: !response.tool_calls.is_empty(),
            ..Self::default()
        };
        emitted.record_text(response.text.as_deref().unwrap_or_default());
        emitted.record_reasoning(response.reasoning_content.as_deref().unwrap_or_default());
        emitted
    }

    /// Fold one streamed content delta in. Empty deltas carry nothing and are
    /// not output.
    pub fn record_text(&mut self, delta: &str) {
        self.text |= !delta.is_empty();
    }

    /// Fold one streamed reasoning delta in.
    pub fn record_reasoning(&mut self, delta: &str) {
        self.reasoning |= !delta.is_empty();
    }

    /// Fold one streamed tool call in. A tool call is output whatever its
    /// arguments say; whether those arguments parse is the separate
    /// `tool_args_all_json` half of the label.
    pub fn record_tool_call(&mut self) {
        self.tool_calls = true;
    }

    /// Whether the model produced anything at all.
    #[must_use]
    pub fn is_nonempty(self) -> bool {
        self.text || self.reasoning || self.tool_calls
    }

    /// Whether any tool call was emitted — the input to the synthesized finish
    /// reason, taken from the same evidence as the shape label.
    #[must_use]
    pub fn has_tool_calls(self) -> bool {
        self.tool_calls
    }
}

/// The implicit shape-validator label (migration 0004): output present and
/// non-empty, every tool-call `arguments` parses as JSON, and the synthesized
/// finish reason is not length-truncation. Label-only — it never changes
/// routing, and it labels 100% of served traffic for the success estimator.
///
/// Takes [`EmittedOutput`] rather than a bare `bool` so no caller can hand it
/// output-presence inferred from a provider usage report.
#[must_use]
pub fn shape_ok(emitted: EmittedOutput, tool_args_all_json: bool, finish_reason: &str) -> bool {
    emitted.is_nonempty() && tool_args_all_json && finish_reason != "length"
}

/// Whether every tool call's `arguments` is syntactically valid JSON.
#[must_use]
pub fn tool_args_all_json(calls: &[ToolCall]) -> bool {
    calls
        .iter()
        .all(|call| serde_json::from_str::<Value>(&call.arguments).is_ok())
}

#[must_use]
pub fn tool_call_to_openai(call: ToolCall) -> OpenAiToolCall {
    OpenAiToolCall {
        id: call.id,
        kind: "function".to_owned(),
        function: OpenAiFunctionCall {
            name: call.name,
            arguments: call.arguments,
            extra: Map::new(),
        },
        extra: Map::new(),
    }
}

pub struct StreamMetadata {
    pub request_id: String,
    pub requested_model: String,
    pub created: i64,
    pub include_usage: bool,
}

impl StreamMetadata {
    #[must_use]
    pub fn new(request_id: String, requested_model: String, include_usage: bool) -> Self {
        Self {
            request_id,
            requested_model,
            created: Utc::now().timestamp(),
            include_usage,
        }
    }
}

#[must_use]
pub fn stream_delta_json(
    metadata: &StreamMetadata,
    delta: Value,
    finish_reason: Option<&str>,
) -> String {
    let mut chunk = json!({
        "id": metadata.request_id,
        "object": "chat.completion.chunk",
        "created": metadata.created,
        "model": metadata.requested_model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    });
    if metadata.include_usage {
        chunk["usage"] = Value::Null;
    }
    chunk.to_string()
}

#[must_use]
pub fn stream_usage_json(metadata: &StreamMetadata, usage: OpenAiUsage) -> String {
    json!({
        "id": metadata.request_id,
        "object": "chat.completion.chunk",
        "created": metadata.created,
        "model": metadata.requested_model,
        "choices": [],
        "usage": usage,
    })
    .to_string()
}

#[must_use]
pub fn stream_tool_call_delta(call: ToolCall, index: u32) -> Value {
    json!({
        "tool_calls": [{
            "index": index,
            "id": call.id,
            "type": "function",
            "function": {
                "name": call.name,
                "arguments": call.arguments,
            },
        }],
    })
}

/// The largest rate the catalog may express, in USD per million tokens.
///
/// This is an *arithmetic* headroom bound, not a business ceiling. It sits four
/// orders of magnitude above any real frontier price (tens of dollars per MTok
/// at the time of writing), so it never constrains a rate an operator would
/// actually write, and it rejects the unit errors and typos that would otherwise
/// pass validation as "a finite non-negative number".
///
/// What the bound buys, precisely: `Decimal`'s maximum magnitude is ~7.9e28 and
/// token counts are `u64` (< 1.9e19), so each `tokens * rate` product in
/// [`usage_cost`] is at most ~1.9e25 and their sum at most ~5.6e25 — comfortably
/// inside `Decimal`'s range even if an upstream reported `u64::MAX` on every
/// dimension. A rate that passed validation therefore *cannot* overflow the
/// multiplication, which is the panic this bound exists to remove. The checked
/// arithmetic in [`usage_cost`] is the belt to that braces.
pub const MAX_RATE_PER_MTOK: f64 = 1_000_000.0;

/// The `Decimal` a configured rate meters at, or `None` when the `f64` cannot be
/// billed with at all.
///
/// This is the only `f64` → `Decimal` hop in the pricing path, and catalog
/// validation ([`crate::config`]) and cost calculation ([`usage_cost`]) both go
/// through it. That is deliberate: a rate the catalog accepted is a rate
/// metering can price *by construction*, rather than because two separately
/// written predicates happen to agree.
///
/// Three ways a finite, non-negative rate is still unbillable:
///
/// - **Above [`MAX_RATE_PER_MTOK`]** — representable, but large enough that
///   `tokens * rate` could overflow `Decimal` and panic mid-request.
/// - **Outside `Decimal`'s range** — `1e100` is a perfectly ordinary `f64` and
///   `Decimal::from_f64` returns `None` for it. The old code substituted
///   `Decimal::ZERO` here, which turned a fat-fingered rate into a silently free
///   price dimension: the customer is charged nothing, the catalog still
///   advertises a price, and nothing but the margin ever notices.
/// - **Nonzero but rounding to `Decimal` zero** — below ~1e-28 the conversion
///   lands on zero. Same silent-free-dimension failure as above, reached from
///   the other end.
#[must_use]
pub fn billable_rate(rate: f64) -> Option<Decimal> {
    // `contains` on the inclusive range also rejects NaN, which compares false
    // against every bound.
    if !(0.0..=MAX_RATE_PER_MTOK).contains(&rate) {
        return None;
    }
    let converted = Decimal::from_f64(rate)?;
    // A rate the operator wrote as nonzero must not meter as free.
    if rate > 0.0 && converted.is_zero() {
        return None;
    }
    Some(converted)
}

/// Price `usage` at `rates`, or `None` when the rates cannot be billed with.
///
/// `None` is a **metering failure**, never a zero charge. Every call site either
/// refuses the request with [`crate::error::ApiError::MeteringUnavailable`] (the
/// customer-billing sites: reservation sizing and the settled `cost_usd`) or
/// writes SQL NULL, the ledger's word for "not captured" (the internal COGS
/// sites). Substituting zero — which is what the `unwrap_or(Decimal::ZERO)` this
/// replaced did — is the one thing it must not do, because a zero charge is
/// indistinguishable from a correctly-priced free request.
///
/// For a catalog that loaded, `None` is unreachable: [`billable_rate`] is the
/// same gate `validate_rates` applies at load time. It is checked here anyway so
/// that the guarantee does not depend on every future caller having come through
/// a validated catalog.
///
/// A dimension the rates leave unset prices at zero (cached input falls back to
/// the uncached input rate first). That is long-standing behavior and unchanged.
#[must_use]
pub fn usage_cost(rates: ModelRates, usage: OpenAiUsage) -> Option<Decimal> {
    let input_rate = billable_rate(rates.input_per_mtok.unwrap_or(0.0))?;
    let output_rate = billable_rate(rates.output_per_mtok.unwrap_or(0.0))?;
    let cached_rate = billable_rate(
        rates
            .cached_input_per_mtok
            .unwrap_or(rates.input_per_mtok.unwrap_or(0.0)),
    )?;
    let million = Decimal::from(1_000_000_u64);
    let cached = usage.cached_input_tokens().min(usage.prompt_tokens);
    let uncached = usage.prompt_tokens.saturating_sub(cached);

    // Checked throughout: `Decimal`'s operators are the same routines with a
    // panic bolted on where these return `None`, so the arithmetic result is
    // bit-identical and nothing a customer is charged changes.
    Decimal::from(uncached)
        .checked_mul(input_rate)?
        .checked_add(Decimal::from(cached).checked_mul(cached_rate)?)?
        .checked_add(Decimal::from(usage.completion_tokens).checked_mul(output_rate)?)?
        .checked_div(million)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_token_usage_is_rejected_as_unusable() {
        // A provider reporting 0 input + 0 output must not meter as a free
        // success; it routes through the missing-usage path instead.
        let usage = TokenUsage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cached_input_tokens: None,
        };
        assert!(OpenAiUsage::try_from_provider(Some(&usage)).is_none());
    }

    #[test]
    fn nonzero_token_usage_is_accepted() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(0),
            cached_input_tokens: None,
        };
        let converted =
            OpenAiUsage::try_from_provider(Some(&usage)).expect("prompt-only usage is meterable");
        assert_eq!(converted.prompt_tokens, 10);
        assert_eq!(converted.completion_tokens, 0);
    }

    #[test]
    fn converts_openai_tool_history_to_zeroclaw_wire_markers() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/balanced",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"command\":\"pwd\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "/tmp"}
            ]
        }))
        .expect("request should parse");

        let messages = request.provider_messages();
        assert!(messages[0].content.contains("tool_calls"));
        assert!(messages[1].content.contains("tool_call_id"));
    }

    // Canary: pins the exact JSON tool-history marker encoding (full strings,
    // not substrings) that ZR emits in `ChatMessage.content` and that the
    // pinned zeroclaw `compatible.rs` parses back out (see `to_provider_message`
    // above). Serde serializes map keys sorted (no `preserve_order` feature), so
    // these literals are stable. If either the ZR encoding here or the upstream
    // parser's expectation drifts, this fails loudly — do not "fix" it by
    // relaxing to substrings; reconcile both sides of the contract first.
    #[test]
    fn tool_history_markers_match_exact_wire_encoding() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/balanced",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"command\":\"pwd\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "/tmp"}
            ]
        }))
        .expect("request should parse");

        let messages = request.provider_messages();
        assert_eq!(
            messages[0].content,
            r#"{"content":"","reasoning_content":null,"tool_calls":[{"arguments":"{\"command\":\"pwd\"}","id":"call_1","name":"shell"}]}"#,
        );
        assert_eq!(
            messages[1].content,
            r#"{"content":"/tmp","name":null,"tool_call_id":"call_1"}"#,
        );
    }

    #[test]
    fn cost_separates_cached_and_uncached_input() {
        let cost = usage_cost(
            ModelRates {
                input_per_mtok: Some(2.0),
                cached_input_per_mtok: Some(0.2),
                output_per_mtok: Some(10.0),
            },
            OpenAiUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 100_000,
                total_tokens: 1_100_000,
                prompt_tokens_details: Some(PromptTokenDetails {
                    cached_tokens: 900_000,
                }),
            },
        );
        assert_eq!(cost, Decimal::from_f64(1.38));
    }

    /// A rate outside `Decimal`'s range used to convert to `Decimal::ZERO` and
    /// price its whole dimension free. It must now refuse to price at all.
    #[test]
    fn out_of_range_rate_refuses_to_price_instead_of_billing_zero() {
        assert_eq!(billable_rate(1e100), None);
        let cost = usage_cost(
            ModelRates {
                input_per_mtok: Some(1e100),
                cached_input_per_mtok: None,
                output_per_mtok: Some(10.0),
            },
            OpenAiUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 1_000_000,
                total_tokens: 2_000_000,
                prompt_tokens_details: None,
            },
        );
        assert_eq!(
            cost, None,
            "an unpriceable rate is a metering failure, never a free request"
        );
    }

    /// A nonzero rate small enough to round to `Decimal` zero is the same
    /// silent-free-dimension failure reached from the other end.
    #[test]
    fn subdecimal_rate_refuses_to_price_instead_of_billing_zero() {
        assert_eq!(billable_rate(1e-30), None);
        assert_eq!(billable_rate(0.0), Some(Decimal::ZERO));
    }

    /// A rate large enough to overflow `tokens * rate` used to panic inside
    /// `Decimal`'s multiplication even though the post-division result would
    /// have fit. It is now refused at the bound, and the arithmetic is checked.
    #[test]
    fn rate_that_would_overflow_the_multiplication_is_refused() {
        let overflowing = 1e27;
        assert!(
            Decimal::from_f64(overflowing).is_some(),
            "the rate is representable; it is the product that is not"
        );
        assert_eq!(billable_rate(overflowing), None);
        assert_eq!(
            usage_cost(
                ModelRates {
                    input_per_mtok: Some(overflowing),
                    cached_input_per_mtok: None,
                    output_per_mtok: Some(overflowing),
                },
                OpenAiUsage {
                    prompt_tokens: 1_000_000,
                    completion_tokens: 1_000_000,
                    total_tokens: 2_000_000,
                    prompt_tokens_details: None,
                },
            ),
            None
        );
    }

    /// The bound has to leave every real price alone: a rate at the ceiling
    /// still prices, and it prices without panicking on a `u64::MAX` usage
    /// report from a misbehaving upstream.
    #[test]
    fn the_rate_ceiling_still_prices_the_worst_case_usage_report() {
        assert_eq!(
            billable_rate(MAX_RATE_PER_MTOK),
            Decimal::from_f64(MAX_RATE_PER_MTOK)
        );
        let cost = usage_cost(
            ModelRates {
                input_per_mtok: Some(MAX_RATE_PER_MTOK),
                cached_input_per_mtok: Some(MAX_RATE_PER_MTOK),
                output_per_mtok: Some(MAX_RATE_PER_MTOK),
            },
            OpenAiUsage {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                total_tokens: u64::MAX,
                prompt_tokens_details: Some(PromptTokenDetails {
                    cached_tokens: u64::MAX,
                }),
            },
        );
        assert!(
            cost.is_some(),
            "a validated rate must price any token count a u64 can hold"
        );
    }

    #[test]
    fn detects_cache_control_in_content_blocks() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/high-end",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "cache me",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .expect("request should parse");

        assert!(request.contains_cache_control());
    }

    #[test]
    fn detects_unsupported_openai_controls() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/balanced",
            "top_p": 0.9,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .expect("request should parse");

        assert!(request.contains_unsupported_extensions());
    }

    #[test]
    fn the_zerorouter_object_is_typed_not_an_unsupported_extension() {
        // Before the knob, a top-level `zerorouter` key landed in `extra` and
        // was 400-rejected as unsupported — so typing it cannot change any
        // request that worked. Typed, it must no longer read as an extension,
        // and its contents are strictly validated.
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/balanced",
            "messages": [{"role": "user", "content": "hello"}],
            "zerorouter": {"priority": "cost"}
        }))
        .expect("request should parse");
        assert!(!request.contains_unsupported_extensions());
        assert!(request.extra.is_empty());
        assert_eq!(
            request.zerorouter.and_then(|options| options.priority),
            Some(Priority::Cost)
        );

        // deny_unknown_fields: a typo inside ZeroRouter's own namespace is a
        // deserialization failure, not a silently dropped field.
        assert!(
            serde_json::from_value::<ChatCompletionRequest>(json!({
                "model": "zero/balanced",
                "messages": [{"role": "user", "content": "hello"}],
                "zerorouter": {"priorty": "cost"}
            }))
            .is_err()
        );
    }

    #[test]
    fn accepts_stock_zeroclaw_tool_policy_and_reasoning_history() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/balanced",
            "messages": [{
                "role": "assistant",
                "content": "Previous answer",
                "reasoning_content": "Prior model reasoning"
            }, {
                "role": "user",
                "content": "Continue"
            }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "Run a command",
                    "parameters": {"type": "object"}
                }
            }],
            "tool_choice": "auto"
        }))
        .expect("stock ZeroClaw-shaped request should parse");

        request.validate().expect("tool policy should validate");
        assert!(!request.contains_unsupported_extensions());
        assert!(
            request.provider_messages()[0]
                .content
                .contains("reasoning_content")
        );
    }

    #[test]
    fn finish_reason_reports_length_at_the_requested_limit() {
        let usage = OpenAiUsage {
            completion_tokens: 128,
            ..OpenAiUsage::default()
        };
        assert_eq!(finish_reason(false, usage, Some(128)), "length");
        assert_eq!(finish_reason(true, usage, Some(128)), "tool_calls");
    }

    #[test]
    fn stream_metadata_is_stable_and_adds_usage_null() {
        let metadata = StreamMetadata {
            request_id: "chatcmpl-test".to_owned(),
            requested_model: "zero/test".to_owned(),
            created: 123,
            include_usage: true,
        };
        let chunk: Value = serde_json::from_str(&stream_delta_json(
            &metadata,
            json!({"content": "hello"}),
            None,
        ))
        .expect("chunk should be JSON");

        assert_eq!(chunk["created"], 123);
        assert!(chunk["usage"].is_null());
    }

    #[test]
    fn task_signature_is_sixteen_hex_and_deterministic() {
        let tools = vec!["shell".to_owned(), "read".to_owned()];
        let first = task_signature("user-a", &tools, 3, 200, false, 4096);
        let again = task_signature("user-a", &tools, 3, 200, false, 4096);
        assert_eq!(first, again, "same inputs must hash identically");
        assert_eq!(first.hex.len(), 16);
        assert!(first.hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(first.scheme, TASK_SIGNATURE_SCHEME);
    }

    #[test]
    fn task_signature_is_user_scoped_and_tool_order_insensitive() {
        let ordered = vec!["a".to_owned(), "b".to_owned()];
        let reversed = vec!["b".to_owned(), "a".to_owned()];
        assert_eq!(
            task_signature("user-a", &ordered, 1, 10, false, 4096),
            task_signature("user-a", &reversed, 1, 10, false, 4096),
            "tool names are sorted before hashing"
        );
        assert_ne!(
            task_signature("user-a", &ordered, 1, 10, false, 4096),
            task_signature("user-b", &ordered, 1, 10, false, 4096),
            "a different user must land in a different segment"
        );
    }

    /// The collision the `,`-joined encoding admitted: tool-name validation
    /// only requires a non-empty name, so one tool actually named `a,b` hashed
    /// exactly like two tools named `a` and `b`. Length-prefixing makes the
    /// encoding injective, so the two are different segments.
    #[test]
    fn tool_names_with_a_separator_do_not_collide_with_two_tools() {
        let one_comma_name = vec!["a,b".to_owned()];
        let two_names = vec!["a".to_owned(), "b".to_owned()];
        assert_ne!(
            tool_names_digest(&one_comma_name),
            tool_names_digest(&two_names),
            "one tool named 'a,b' is not the same tool set as tools 'a' and 'b'"
        );
        assert_ne!(
            task_signature("user-a", &one_comma_name, 1, 10, false, 4096),
            task_signature("user-a", &two_names, 1, 10, false, 4096),
            "a collidable tool encoding would merge two distinct segments"
        );
        // A concatenation that is ambiguous under ANY fixed separator: {"a",
        // "b\u{1f}c"} versus {"a\u{1f}b", "c"} share every byte of a joined
        // encoding using the field separator itself.
        assert_ne!(
            tool_names_digest(&["a".to_owned(), "b\u{1f}c".to_owned()]),
            tool_names_digest(&["a\u{1f}b".to_owned(), "c".to_owned()]),
            "the tool section must not be reinterpretable as a different multiset"
        );
    }

    /// The re-keying claim migration 0004 made and only migration 0007 can
    /// honour: the persisted digest is the exact tool input the key was built
    /// from, not a lossy summary of it, so a settled row can be re-keyed
    /// without the names.
    #[test]
    fn task_signature_carries_the_exact_tool_digest_it_was_built_from() {
        let tools = vec!["shell".to_owned(), "read".to_owned()];
        let signature = task_signature("user-a", &tools, 3, 200, false, 4096);
        assert_eq!(signature.tool_names_sha256, tool_names_digest(&tools));
        assert_eq!(signature.tool_names_sha256.len(), 64);
        assert!(
            signature
                .tool_names_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        // Same multiset, different order: the digest is a property of the set.
        assert_eq!(
            signature.tool_names_sha256,
            tool_names_digest(&["read".to_owned(), "shell".to_owned()])
        );
    }

    #[test]
    fn task_signature_message_count_buckets_change_only_at_boundaries() {
        let sig = |count| task_signature("user", &[], count, 100, false, 4096);
        // Bucket edges are {1, 2-4, 5-16, 17+}.
        assert_eq!(sig(2), sig(4), "2 and 4 share the 2-4 bucket");
        assert_eq!(sig(5), sig(16), "5 and 16 share the 5-16 bucket");
        assert_ne!(sig(1), sig(2), "the 1 | 2-4 boundary shifts the bucket");
        assert_ne!(sig(4), sig(5), "the 2-4 | 5-16 boundary shifts the bucket");
        assert_ne!(
            sig(16),
            sig(17),
            "the 5-16 | 17+ boundary shifts the bucket"
        );
    }

    #[test]
    fn task_signature_prompt_bytes_buckets_are_log2() {
        let sig = |bytes| task_signature("user", &[], 1, bytes, false, 4096);
        assert_eq!(sig(256), sig(300), "256 and 300 share the log2 bucket 8");
        assert_ne!(
            sig(255),
            sig(256),
            "crossing a power of two changes the bucket"
        );
        assert_ne!(sig(256), sig(512), "the next power of two is a new bucket");
    }

    fn emitted_text() -> EmittedOutput {
        let mut emitted = EmittedOutput::default();
        emitted.record_text("hello");
        emitted
    }

    #[test]
    fn shape_ok_requires_output_valid_args_and_no_truncation() {
        assert!(shape_ok(emitted_text(), true, "stop"));
        assert!(shape_ok(emitted_text(), true, "tool_calls"));
        assert!(
            !shape_ok(EmittedOutput::default(), true, "stop"),
            "empty output fails the shape check"
        );
        assert!(
            !shape_ok(emitted_text(), false, "stop"),
            "unparseable tool args fail"
        );
        assert!(
            !shape_ok(emitted_text(), true, "length"),
            "length truncation fails"
        );
    }

    /// A thinking model can answer entirely in `reasoning_content`. That is a
    /// non-empty response and must label as one — reading only `text` and
    /// `tool_calls` labelled it a failure and would have taught the success
    /// estimator that reasoning models fail.
    #[test]
    fn reasoning_only_output_is_not_empty() {
        let mut emitted = EmittedOutput::default();
        emitted.record_reasoning("thinking about it");
        assert!(emitted.is_nonempty());
        assert!(shape_ok(emitted, true, "stop"));

        let response = ChatResponse {
            text: None,
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: Some("thinking about it".to_owned()),
        };
        assert!(
            EmittedOutput::from_response(&response).is_nonempty(),
            "a buffered response that is entirely reasoning is still output"
        );
    }

    /// Every field is folded from an actual emission, so there is no path by
    /// which a usage report alone can make output look present. Empty deltas
    /// carry nothing and must not flip the flag either.
    #[test]
    fn emitted_output_only_counts_real_emissions() {
        let mut emitted = EmittedOutput::default();
        emitted.record_text("");
        emitted.record_reasoning("");
        assert!(
            !emitted.is_nonempty(),
            "empty deltas are not emitted output"
        );
        assert!(!emitted.has_tool_calls());
        emitted.record_tool_call();
        assert!(emitted.is_nonempty(), "a tool call is output on its own");
        assert!(emitted.has_tool_calls());

        let empty = ChatResponse {
            text: Some(String::new()),
            tool_calls: Vec::new(),
            usage: Some(TokenUsage {
                input_tokens: Some(10),
                cached_input_tokens: None,
                output_tokens: Some(500),
            }),
            reasoning_content: None,
        };
        assert!(
            !EmittedOutput::from_response(&empty).is_nonempty(),
            "500 reported output tokens do not make an empty response non-empty"
        );
    }

    #[test]
    fn tool_args_all_json_rejects_malformed_arguments() {
        let good = vec![ToolCall {
            id: "call_1".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"pwd"}"#.to_owned(),
            extra_content: None,
        }];
        let bad = vec![ToolCall {
            id: "call_2".to_owned(),
            name: "shell".to_owned(),
            arguments: "{not json".to_owned(),
            extra_content: None,
        }];
        assert!(tool_args_all_json(&good));
        assert!(!tool_args_all_json(&bad));
    }

    #[test]
    fn reservation_usage_is_conservative_and_includes_output_limit() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "zero/low-cost",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .expect("request should parse");
        let reservation = request.reservation_usage(1_024);

        assert!(reservation.prompt_tokens >= 5);
        assert_eq!(reservation.completion_tokens, 1_024);
        assert_eq!(
            reservation.total_tokens,
            reservation.prompt_tokens + reservation.completion_tokens
        );
    }
}

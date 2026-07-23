use std::collections::BTreeMap;

use chrono::Utc;
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zeroclaw_api::tool::ToolSpec;
use zeroclaw_providers::{
    pricing::ModelRates,
    traits::{ChatMessage, ChatResponse, TokenUsage, ToolCall},
};

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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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

#[must_use]
pub fn usage_cost(rates: ModelRates, usage: OpenAiUsage) -> Decimal {
    let input_rate =
        Decimal::from_f64(rates.input_per_mtok.unwrap_or(0.0)).unwrap_or(Decimal::ZERO);
    let output_rate =
        Decimal::from_f64(rates.output_per_mtok.unwrap_or(0.0)).unwrap_or(Decimal::ZERO);
    let cached_rate = Decimal::from_f64(
        rates
            .cached_input_per_mtok
            .unwrap_or(rates.input_per_mtok.unwrap_or(0.0)),
    )
    .unwrap_or(Decimal::ZERO);
    let million = Decimal::from(1_000_000_u64);
    let cached = usage.cached_input_tokens().min(usage.prompt_tokens);
    let uncached = usage.prompt_tokens.saturating_sub(cached);

    (Decimal::from(uncached) * input_rate
        + Decimal::from(cached) * cached_rate
        + Decimal::from(usage.completion_tokens) * output_rate)
        / million
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
        assert_eq!(cost, Decimal::from_f64(1.38).expect("decimal"));
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

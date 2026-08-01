//! ZeroRouter-owned upstream wire clients.
//!
//! This module reverses a founding assumption: upstream adapters were
//! imported wholesale from zeroclaw-providers (the git pin), whose
//! correctness bar is agent-grade — "the agent got its answer", usage
//! optional. ZeroRouter is a metering business; usage is the product. The
//! recorded costs of that mismatch: the pinned Responses provider hardcodes
//! `usage: None` (making every gpt-5.x and codex model unmeterable here),
//! `finish_reason` is synthesized because the pinned trait carries no stop
//! reason, and `content_filter` is structurally unobservable. Wire clients
//! owned here are billing-grade by construction: usage extraction is the
//! point, not an afterthought.
//!
//! Scope discipline: these clients serve ZEROROUTER's traffic, not
//! zeroclaw-general. ZeroRouter's compat layer rejects structured content
//! and never emits multimodal markers, so the input builder handles exactly
//! the five packing shapes `openai::to_provider_message` produces — system
//! and user plain text, assistant text (with optional reasoning), assistant
//! tool-call packs, and tool-result packs — and is unit-tested against that
//! packer, not against hypothetical inputs.
//!
//! Increment 1: the OpenAI Responses wire (`/v1/responses`), the family the
//! pinned adapters cannot meter. Reasoning ITEMS from responses histories
//! are not round-tripped (ZeroRouter's compat surface has nowhere to carry
//! them); text and tool-call flows are complete. The error strings these
//! clients produce deliberately carry the HTTP status digits and the
//! upstream body verbatim (sanitized downstream by the retention layer), so
//! `retry::classify`'s status/heuristic taxonomy reads them exactly as it
//! reads the pinned adapters' errors.
//!
//! Increment 2: the Anthropic Messages wire (`/v1/messages`), replacing the
//! pinned Anthropic adapter on first-party traffic. The billing-grade
//! difference is usage NORMALIZATION, not just extraction: Anthropic's
//! `usage.input_tokens` EXCLUDES cache reads and cache writes, while
//! ZeroRouter's cost function (`openai::usage_cost`) prices `cached` as a
//! subset of `input` — the OpenAI convention every other wire reports in.
//! This client folds `cache_read_input_tokens` and
//! `cache_creation_input_tokens` back into the input total and reports the
//! read subset as `cached_input_tokens`, so an Anthropic response meters on
//! the same axes as everything else. The wire sets its own `cache_control`
//! breakpoints (system, last tool, last turn) exactly as the pinned adapter
//! did, so the upstream cache discount survives the swap. (Cache WRITES
//! bill at full input rate here; Anthropic charges 1.25× for them — a COGS
//! rounding this router accepts.)

use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use zeroclaw_api::model_provider::{
    ChatRequest, ChatResponse, ModelProvider, ProviderCapabilities, StreamChunk, StreamError,
    StreamEvent, StreamOptions, StreamResult, TokenUsage, ToolCall,
};
use zeroclaw_providers::traits::ChatMessage;

/// Default endpoint for the OpenAI Responses API.
const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

/// ZeroRouter's own Responses-API client: chat and streaming both on
/// `/v1/responses`, usage lifted from the wire on both paths.
pub struct OpenAiResponsesWire {
    alias: String,
    api_url: String,
    credential: String,
    max_tokens: Option<u32>,
    http: reqwest::Client,
}

impl OpenAiResponsesWire {
    #[must_use]
    pub fn new(
        alias: &str,
        credential: &str,
        api_url: Option<&str>,
        max_tokens: Option<u32>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            alias: alias.to_owned(),
            api_url: api_url
                .map(|url| url.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| RESPONSES_URL.to_owned()),
            credential: credential.to_owned(),
            max_tokens,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_default(),
        }
    }

    fn request_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[zeroclaw_api::tool::ToolSpec]>,
        temperature: Option<f64>,
        stream: bool,
    ) -> Value {
        let (instructions, input) = build_responses_input(messages);
        let tools_json: Option<Vec<Value>> = tools.filter(|tools| !tools.is_empty()).map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    })
                })
                .collect()
        });
        let mut body = json!({
            "model": model,
            "input": input,
            "stream": stream,
        });
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions);
        }
        if let Some(tools) = tools_json {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        if let Some(max_tokens) = self.max_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        }
        if let Some(temperature) = temperature {
            body["temperature"] = json!(temperature);
        }
        body
    }
}

/// Build `(instructions, input)` from ZeroRouter's packed provider messages
/// — exactly the shapes `openai::to_provider_message` emits, nothing more.
fn build_responses_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut input = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "system" => system_parts.push(&message.content),
            "user" => input.push(message_item("user", "input_text", &message.content)),
            "assistant" => {
                // ZR packs assistant turns that carry tool calls or
                // reasoning as a JSON envelope; a plain string is a plain
                // reply.
                // ZR's packed envelopes always carry `tool_calls` or
                // `reasoning_content`; an assistant reply that merely IS a
                // JSON object (a model answering in JSON) has neither and
                // must pass through as plain text, not vanish.
                if let Ok(envelope) = serde_json::from_str::<Value>(&message.content)
                    && envelope.is_object()
                    && (envelope.get("tool_calls").is_some()
                        || envelope.get("reasoning_content").is_some())
                {
                    if let Some(text) = envelope
                        .get("content")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                    {
                        input.push(message_item("assistant", "output_text", text));
                    }
                    if let Some(calls) = envelope.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                            let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                            let arguments = call
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}");
                            input.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                    }
                } else {
                    input.push(message_item("assistant", "output_text", &message.content));
                }
            }
            "tool" => {
                // ZR packs tool results as {tool_call_id, name, content}.
                let (call_id, output) = serde_json::from_str::<Value>(&message.content)
                    .ok()
                    .and_then(|envelope| {
                        let call_id = envelope
                            .get("tool_call_id")
                            .and_then(Value::as_str)?
                            .to_owned();
                        let output = envelope
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        Some((call_id, output))
                    })
                    .unwrap_or_else(|| (String::new(), message.content.clone()));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
            _ => {}
        }
    }

    (system_parts.join("\n\n"), input)
}

/// The next SSE event boundary: LF-LF or CRLF-CRLF, whichever comes first.
fn find_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|at| (at, 2));
    let crlf = buffer.find("\r\n\r\n").map(|at| (at, 4));
    match (lf, crlf) {
        (Some((lf_at, _)), Some((crlf_at, _))) if crlf_at + 2 == lf_at => crlf,
        (Some(lf), Some(crlf)) => {
            if crlf.0 < lf.0 {
                Some(crlf)
            } else {
                Some(lf)
            }
        }
        (only, None) => only,
        (None, only) => only,
    }
}

fn message_item(role: &str, content_type: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// The Responses envelope fields billing needs. Everything else on the
/// response is deliberately ignored.
#[derive(Deserialize)]
struct ResponsesEnvelope {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    incomplete_details: Option<Value>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputDetails>,
}

#[derive(Deserialize)]
struct ResponsesInputDetails {
    cached_tokens: Option<u64>,
}

impl ResponsesUsage {
    /// The billing-grade lift this module exists for: input, output, and
    /// the cached-input subset, straight from the wire.
    fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self
                .input_tokens_details
                .and_then(|details| details.cached_tokens),
        }
    }
}

fn parse_envelope(envelope: ResponsesEnvelope) -> ChatResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    for item in &envelope.output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(text) = part.get("text").and_then(Value::as_str)
                        {
                            text_parts.push(text.to_owned());
                        }
                    }
                }
            }
            Some("function_call") => {
                tool_calls.push(ToolCall {
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_owned(),
                    extra_content: None,
                });
            }
            _ => {}
        }
    }
    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    let _ = (&envelope.status, &envelope.incomplete_details);
    ChatResponse {
        text,
        tool_calls,
        usage: envelope.usage.map(ResponsesUsage::into_token_usage),
        reasoning_content: None,
    }
}

/// Error text carries the status digits and the upstream body verbatim so
/// `retry::classify` reads our failures exactly as it reads the pinned
/// adapters' (4xx digit parse, `429` + rate hints, context-window hints
/// from the upstream's own words). The retention layer keeps the body out
/// of default-level logs downstream.
fn upstream_error(alias: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    anyhow!(
        "{alias} responses API error: HTTP {} {}: {body}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error"),
    )
}

impl zeroclaw_api::attribution::Attributable for OpenAiResponsesWire {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Provider(zeroclaw_api::attribution::ProviderKind::Model(
            zeroclaw_api::attribution::ModelProviderKind::OpenAi,
        ))
    }

    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl ModelProvider for OpenAiResponsesWire {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            prompt_caching: true,
            extended_thinking: false,
        }
    }

    fn default_base_url(&self) -> Option<&str> {
        Some(&self.api_url)
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let response = self.chat(request, model, temperature).await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let body = self.request_body(model, request.messages, request.tools, temperature, false);
        let response = self
            .http
            .post(&self.api_url)
            .bearer_auth(&self.credential)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        // Same rule as the Anthropic wire: the status is already known, so a
        // failed body read must not erase it into a retryable-looking
        // transport error.
        let text = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<body unreadable: {error}>"));
        if !status.is_success() {
            return Err(upstream_error(&self.alias, status, &text));
        }
        let envelope: ResponsesEnvelope = serde_json::from_str(&text).map_err(|error| {
            anyhow!(
                "{} responses API returned unparseable JSON: {error}",
                self.alias
            )
        })?;
        Ok(parse_envelope(envelope))
    }

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> futures_util::stream::BoxStream<'static, StreamResult<StreamEvent>> {
        let body = self.request_body(model, request.messages, request.tools, temperature, true);
        let http = self.http.clone();
        let api_url = self.api_url.clone();
        let credential = self.credential.clone();
        let alias = self.alias.clone();
        let count_tokens = options.count_tokens;

        let stream = async_stream::try_stream! {
            let response = http
                .post(&api_url)
                .bearer_auth(&credential)
                .json(&body)
                .send()
                .await
                .map_err(|error| StreamError::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                Err(StreamError::Http(
                    upstream_error(&alias, status, &text).to_string(),
                ))?;
                return;
            }

            let mut bytes = response.bytes_stream();
            // Byte buffer, decoded only at valid UTF-8 prefixes: a multibyte
            // character split across network chunks must never become a
            // replacement character in a customer's stream.
            let mut raw_buffer: Vec<u8> = Vec::new();
            let mut buffer = String::new();
            let mut finished = false;
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|error| StreamError::Http(error.to_string()))?;
                raw_buffer.extend_from_slice(&chunk);
                let valid_up_to = match std::str::from_utf8(&raw_buffer) {
                    Ok(_) => raw_buffer.len(),
                    Err(error) => error.valid_up_to(),
                };
                buffer.push_str(
                    std::str::from_utf8(&raw_buffer[..valid_up_to])
                        .expect("prefix was just validated"),
                );
                raw_buffer.drain(..valid_up_to);

                // SSE events are separated by a blank line — LF or CRLF
                // framing are both legal; `data:` lines strip a trailing CR.
                while let Some((boundary, delimiter_len)) = find_event_boundary(&buffer) {
                    let raw = buffer[..boundary].to_owned();
                    buffer.drain(..boundary + delimiter_len);
                    let data = raw
                        .lines()
                        .map(|line| line.strip_suffix('\r').unwrap_or(line))
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if data.is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(&data)
                        .map_err(StreamError::Json)?;
                    match value.get("type").and_then(Value::as_str) {
                        Some("response.output_text.delta") => {
                            if let Some(delta) =
                                value.get("delta").and_then(Value::as_str)
                            {
                                let mut chunk = StreamChunk::delta(delta);
                                if count_tokens {
                                    // The ZR-documented per-chunk floor
                                    // convention: len()/4, a labeled lower
                                    // bound, never billing.
                                    chunk.token_count = delta.len() / 4;
                                }
                                yield StreamEvent::TextDelta(chunk);
                            }
                        }
                        Some("response.output_item.done") => {
                            if let Some(item) = value.get("item")
                                && item.get("type").and_then(Value::as_str)
                                    == Some("function_call")
                            {
                                yield StreamEvent::ToolCall(ToolCall {
                                    id: item
                                        .get("call_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    name: item
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    arguments: item
                                        .get("arguments")
                                        .and_then(Value::as_str)
                                        .unwrap_or("{}")
                                        .to_owned(),
                                    extra_content: None,
                                });
                            }
                        }
                        // `response.incomplete` is a TERMINAL event too — a
                        // max_output_tokens clip lands here with usage
                        // attached, and this router always sets
                        // max_output_tokens. Treating only `completed` as
                        // terminal would deliver the clipped output and then
                        // settle it unbilled (codex-sol review finding).
                        Some("response.completed" | "response.incomplete") => {
                            if let Some(usage) = value
                                .get("response")
                                .and_then(|response| response.get("usage"))
                                && let Ok(usage) =
                                    serde_json::from_value::<ResponsesUsage>(usage.clone())
                            {
                                yield StreamEvent::Usage(usage.into_token_usage());
                            }
                            finished = true;
                            yield StreamEvent::Final;
                        }
                        Some("response.failed" | "error") => {
                            let error_value = value
                                .get("response")
                                .and_then(|response| response.get("error"))
                                .or_else(|| value.get("error"));
                            let detail = error_value
                                .map_or_else(|| data.clone(), Value::to_string);
                            // In-band errors carry no HTTP status, but the
                            // walk's classifier reads status digits and rate
                            // hints from the TEXT. Restore the digits for
                            // the shapes that need classifying: a rate limit
                            // must feed the health cooldown, not read as a
                            // generic broken stream.
                            let code = error_value
                                .and_then(|error| error.get("code"))
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let prefix = if code.contains("rate_limit")
                                || detail.contains("rate limit")
                            {
                                "429 Too Many Requests: "
                            } else if code.contains("insufficient_quota") {
                                "429 Too Many Requests: insufficient_quota — "
                            } else {
                                ""
                            };
                            Err(StreamError::ModelProvider(format!(
                                "{alias} responses stream failed: {prefix}{detail}"
                            )))?;
                        }
                        _ => {}
                    }
                }
                if finished {
                    break;
                }
            }
            if !finished {
                // The upstream closed without `response.completed`: surface
                // it rather than synthesizing a Final the wire never sent.
                Err(StreamError::InvalidSse(format!(
                    "{alias} responses stream ended without completion"
                )))?;
            }
        };
        Box::pin(stream)
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages wire
// ---------------------------------------------------------------------------

/// Default endpoint for the Anthropic Messages API.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// The Messages API version this wire speaks. Anthropic versions by header,
/// not by URL; bump deliberately, with the parser.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// ZeroRouter's own Messages-API client: chat and streaming both on
/// `/v1/messages`, usage lifted and normalized on both paths.
pub struct AnthropicWire {
    alias: String,
    api_url: String,
    credential: String,
    /// Required by the Messages API on every request, so not optional here.
    max_tokens: u32,
    http: reqwest::Client,
}

impl AnthropicWire {
    #[must_use]
    pub fn new(
        alias: &str,
        credential: &str,
        api_url: Option<&str>,
        max_tokens: u32,
        timeout_secs: u64,
    ) -> Self {
        Self {
            alias: alias.to_owned(),
            api_url: api_url
                .map(|url| url.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| MESSAGES_URL.to_owned()),
            credential: credential.to_owned(),
            max_tokens,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_default(),
        }
    }

    fn request_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[zeroclaw_api::tool::ToolSpec]>,
        temperature: Option<f64>,
        stream: bool,
    ) -> Value {
        let (system, mut turns) = build_anthropic_messages(messages);
        // Cache breakpoints, matching what the pinned adapter set: the
        // system prompt, the last tool definition, and the last block of the
        // final turn. Dropping these on the swap would silently forfeit the
        // upstream cache discount on every multi-turn conversation (sol
        // review) — the discount the cached_input_tokens dimension exists to
        // meter. Three markers, under the API's limit of four.
        let cache_marker = json!({ "type": "ephemeral" });
        if let Some(block) = turns
            .last_mut()
            .and_then(|turn| turn["content"].as_array_mut())
            .and_then(|content| content.last_mut())
            .and_then(Value::as_object_mut)
        {
            block.insert("cache_control".to_owned(), cache_marker.clone());
        }
        let mut body = json!({
            "model": model,
            "messages": turns,
            "max_tokens": self.max_tokens,
            "stream": stream,
        });
        if !system.trim().is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": cache_marker.clone(),
            }]);
        }
        if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
            let mut tools_json = tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect::<Vec<Value>>();
            if let Some(last) = tools_json.last_mut().and_then(Value::as_object_mut) {
                last.insert("cache_control".to_owned(), cache_marker);
            }
            body["tools"] = json!(tools_json);
        }
        if let Some(temperature) = temperature {
            body["temperature"] = json!(temperature);
        }
        body
    }
}

/// Append a content block to the last turn if it has `role`, else open a new
/// turn. The Messages API requires strict user/assistant alternation, and
/// ZR's packing legitimately produces runs — parallel tool results pack as
/// consecutive `tool` messages that must land in ONE user turn.
fn push_anthropic_block(turns: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = turns.last_mut()
        && last["role"] == role
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(block);
        return;
    }
    turns.push(json!({ "role": role, "content": [block] }));
}

/// Split ZR's `[IMAGE:<url>]` markers (the compat layer's packing for OpenAI
/// image parts, `openai::content_to_text`) back into native image blocks —
/// the pinned adapter decoded these, so sending them as literal text would be
/// a silent vision regression (sol review). `data:` URIs become base64
/// sources, anything else a URL source; a malformed marker stays text.
fn push_user_text_with_images(turns: &mut Vec<Value>, text: &str) {
    let mut rest = text;
    while let Some(start) = rest.find("[IMAGE:") {
        let before = &rest[..start];
        if !before.trim().is_empty() {
            push_anthropic_block(turns, "user", json!({ "type": "text", "text": before }));
        }
        let after_marker = &rest[start + "[IMAGE:".len()..];
        let Some(end) = after_marker.find(']') else {
            // Unterminated marker: keep the raw text and stop scanning.
            rest = &rest[start..];
            break;
        };
        let url = &after_marker[..end];
        let block = url
            .strip_prefix("data:")
            .and_then(|data_uri| {
                let (media_type, payload) = data_uri.split_once(";base64,")?;
                Some(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": payload,
                    },
                }))
            })
            .unwrap_or_else(|| {
                json!({
                    "type": "image",
                    "source": { "type": "url", "url": url },
                })
            });
        push_anthropic_block(turns, "user", block);
        rest = &after_marker[end + 1..];
    }
    if !rest.trim().is_empty() {
        push_anthropic_block(turns, "user", json!({ "type": "text", "text": rest }));
    }
}

/// After the linear pass: every assistant `tool_use` must have a matching
/// `tool_result` in the NEXT user turn or the API 400s the whole request.
/// ZR accepts interrupted OpenAI histories (parallel calls where only some
/// results came back, or a history cut off mid-call), and the pinned adapter
/// kept them valid by synthesizing the missing results — so this does too,
/// inserting empty results at the head of the following user turn (creating
/// one when the history ends on the tool_use).
fn synthesize_missing_tool_results(turns: &mut Vec<Value>) {
    let mut at = 0;
    while at < turns.len() {
        if turns[at]["role"] == "assistant" {
            let pending: Vec<String> = turns[at]["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "tool_use")
                        .filter_map(|block| block["id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if !pending.is_empty() {
                if turns.get(at + 1).is_none_or(|next| next["role"] != "user") {
                    turns.insert(at + 1, json!({ "role": "user", "content": [] }));
                }
                let answered: std::collections::BTreeSet<String> = turns[at + 1]["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| block["type"] == "tool_result")
                            .filter_map(|block| block["tool_use_id"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(content) = turns[at + 1]["content"].as_array_mut() {
                    for (offset, id) in pending
                        .iter()
                        .filter(|id| !answered.contains(*id))
                        .enumerate()
                    {
                        content.insert(offset, json!({ "type": "tool_result", "tool_use_id": id }));
                    }
                }
            }
        }
        at += 1;
    }
}

/// Build `(system, messages)` from ZeroRouter's packed provider messages —
/// the same five shapes `openai::to_provider_message` emits. Tool results
/// map to `tool_result` blocks in USER turns (the Messages API's shape for
/// them); `reasoning_content` from packed envelopes is dropped, matching the
/// Responses wire's no-round-trip rule. Empty and whitespace-only text never
/// becomes a block — the API rejects empty text blocks outright (sol
/// review), and ZR's validation admits null/empty content.
fn build_anthropic_messages(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut turns = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "system" => system_parts.push(&message.content),
            "user" => push_user_text_with_images(&mut turns, &message.content),
            "assistant" => {
                // Same envelope rule as the Responses wire: only ZR's own
                // packing markers make an envelope; a model that merely
                // answered in JSON round-trips as plain text.
                if let Ok(envelope) = serde_json::from_str::<Value>(&message.content)
                    && envelope.is_object()
                    && (envelope.get("tool_calls").is_some()
                        || envelope.get("reasoning_content").is_some())
                {
                    if let Some(text) = envelope
                        .get("content")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                    {
                        push_anthropic_block(
                            &mut turns,
                            "assistant",
                            json!({ "type": "text", "text": text }),
                        );
                    }
                    if let Some(calls) = envelope.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                            let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                            // ZR packs arguments as a JSON STRING; the
                            // Messages API wants `input` as an object.
                            let input = call
                                .get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                                .filter(Value::is_object)
                                .unwrap_or_else(|| json!({}));
                            push_anthropic_block(
                                &mut turns,
                                "assistant",
                                json!({
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": input,
                                }),
                            );
                        }
                    }
                } else if !message.content.trim().is_empty() {
                    push_anthropic_block(
                        &mut turns,
                        "assistant",
                        json!({ "type": "text", "text": message.content }),
                    );
                }
            }
            "tool" => {
                // ZR packs tool results as {tool_call_id, name, content}.
                let (call_id, output) = serde_json::from_str::<Value>(&message.content)
                    .ok()
                    .and_then(|envelope| {
                        let call_id = envelope
                            .get("tool_call_id")
                            .and_then(Value::as_str)?
                            .to_owned();
                        let output = envelope
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        Some((call_id, output))
                    })
                    .unwrap_or_else(|| (String::new(), message.content.clone()));
                // `content` is omitted when empty rather than sent as "" —
                // the API's empty-text rejection extends to result text.
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                });
                if !output.is_empty() {
                    block["content"] = json!(output);
                }
                push_anthropic_block(&mut turns, "user", block);
            }
            _ => {}
        }
    }

    synthesize_missing_tool_results(&mut turns);
    (system_parts.join("\n\n"), turns)
}

/// Anthropic's usage block, as reported: `input_tokens` EXCLUDES the cache
/// dimensions. `into_token_usage` is where the convention conversion
/// happens — see the module doc.
#[derive(Default, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl AnthropicUsage {
    fn into_token_usage(self) -> TokenUsage {
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_input_tokens.unwrap_or(0);
        TokenUsage {
            // Absent input stays None (the missing-usage path downstream);
            // present input becomes the OpenAI-convention TOTAL.
            input_tokens: self.input_tokens.map(|input| {
                input
                    .saturating_add(cache_read)
                    .saturating_add(cache_creation)
            }),
            output_tokens: self.output_tokens,
            cached_input_tokens: (cache_read > 0).then_some(cache_read),
        }
    }
}

/// The Messages envelope fields billing needs.
#[derive(Deserialize)]
struct MessagesEnvelope {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

fn parse_messages_envelope(envelope: MessagesEnvelope) -> ChatResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &envelope.content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_owned());
                }
            }
            Some("tool_use") => {
                tool_calls.push(ToolCall {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    // Back to ZR's convention: arguments as a JSON string.
                    arguments: block
                        .get("input")
                        .map_or_else(|| "{}".to_owned(), Value::to_string),
                    extra_content: None,
                });
            }
            _ => {}
        }
    }
    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    ChatResponse {
        text,
        tool_calls,
        usage: envelope.usage.map(AnthropicUsage::into_token_usage),
        reasoning_content: None,
    }
}

fn anthropic_upstream_error(alias: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    anyhow!(
        "{alias} messages API error: HTTP {} {}: {body}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error"),
    )
}

/// A tool call under assembly across `input_json_delta` events.
struct PendingAnthropicTool {
    id: String,
    name: String,
    json: String,
}

/// The per-event state machine behind `AnthropicWire::stream_chat`, factored
/// out of the socket loop so the event grammar is unit-testable without a
/// server: text deltas stream through, tool inputs accumulate until their
/// block closes, usage assembles from `message_start` + `message_delta`, and
/// `message_stop` is the only terminal.
#[derive(Default)]
struct AnthropicStreamMachine {
    usage: AnthropicUsage,
    open_tools: std::collections::BTreeMap<u64, PendingAnthropicTool>,
    count_tokens: bool,
    finished: bool,
}

impl AnthropicStreamMachine {
    fn new(count_tokens: bool) -> Self {
        Self {
            count_tokens,
            ..Self::default()
        }
    }

    /// Whatever usage the upstream has genuinely reported so far. Anthropic
    /// reports input at `message_start` and output at `message_delta`, so an
    /// in-band error or a dropped connection AFTER those events still has
    /// billable, wire-reported numbers — settling such a stream with no
    /// usage would charge zero for delivered output (sol review). The caller
    /// emits this before surfacing the stream error.
    fn partial_usage(&mut self) -> Option<TokenUsage> {
        let usage = std::mem::take(&mut self.usage).into_token_usage();
        (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
    }

    fn absorb_usage(&mut self, value: Option<&Value>) {
        let Some(usage) = value else { return };
        // Field-by-field: `message_delta` carries cumulative output tokens
        // (and sometimes more) without repeating what `message_start` said —
        // absent fields must not erase known ones.
        if let Some(tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.usage.input_tokens = Some(tokens);
        }
        if let Some(tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = Some(tokens);
        }
        if let Some(tokens) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.usage.cache_read_input_tokens = Some(tokens);
        }
        if let Some(tokens) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_creation_input_tokens = Some(tokens);
        }
    }

    fn handle(&mut self, alias: &str, value: &Value) -> Result<Vec<StreamEvent>, StreamError> {
        let mut events = Vec::new();
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.absorb_usage(
                    value
                        .get("message")
                        .and_then(|message| message.get("usage")),
                );
            }
            Some("content_block_start") => {
                if let Some(block) = value.get("content_block")
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && let Some(index) = value.get("index").and_then(Value::as_u64)
                {
                    self.open_tools.insert(
                        index,
                        PendingAnthropicTool {
                            id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            json: String::new(),
                        },
                    );
                }
            }
            Some("content_block_delta") => {
                let index = value.get("index").and_then(Value::as_u64);
                match value
                    .get("delta")
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("text_delta") => {
                        if let Some(text) = value
                            .get("delta")
                            .and_then(|delta| delta.get("text"))
                            .and_then(Value::as_str)
                        {
                            let mut chunk = StreamChunk::delta(text);
                            if self.count_tokens {
                                // The ZR-documented per-chunk floor
                                // convention: len()/4, a labeled lower
                                // bound, never billing.
                                chunk.token_count = text.len() / 4;
                            }
                            events.push(StreamEvent::TextDelta(chunk));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = value
                            .get("delta")
                            .and_then(|delta| delta.get("partial_json"))
                            .and_then(Value::as_str)
                            && let Some(index) = index
                            && let Some(pending) = self.open_tools.get_mut(&index)
                        {
                            pending.json.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some(index) = value.get("index").and_then(Value::as_u64)
                    && let Some(pending) = self.open_tools.remove(&index)
                {
                    events.push(StreamEvent::ToolCall(ToolCall {
                        id: pending.id,
                        name: pending.name,
                        arguments: if pending.json.is_empty() {
                            "{}".to_owned()
                        } else {
                            pending.json
                        },
                        extra_content: None,
                    }));
                }
            }
            Some("message_delta") => {
                self.absorb_usage(value.get("usage"));
            }
            Some("message_stop") => {
                let usage = std::mem::take(&mut self.usage).into_token_usage();
                if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                    events.push(StreamEvent::Usage(usage));
                }
                self.finished = true;
                events.push(StreamEvent::Final);
            }
            Some("error") => {
                let error = value.get("error");
                let kind = error
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let detail = error.map_or_else(|| value.to_string(), Value::to_string);
                // Same rule as the Responses wire: in-band errors carry no
                // HTTP status, so restore the digits for the shapes the
                // classifier must route — a rate limit feeds the health
                // cooldown. `overloaded_error` (Anthropic's 529) stays
                // digit-free: it classifies Retryable by default, which is
                // the correct walk behavior for it.
                let prefix = if kind == "rate_limit_error" {
                    "429 Too Many Requests: "
                } else {
                    ""
                };
                return Err(StreamError::ModelProvider(format!(
                    "{alias} messages stream failed: {prefix}{detail}"
                )));
            }
            // `ping` and future event types: ignored by design.
            _ => {}
        }
        Ok(events)
    }
}

impl zeroclaw_api::attribution::Attributable for AnthropicWire {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Provider(zeroclaw_api::attribution::ProviderKind::Model(
            zeroclaw_api::attribution::ModelProviderKind::Anthropic,
        ))
    }

    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl ModelProvider for AnthropicWire {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            // ZR packs OpenAI image parts as [IMAGE:<url>] markers; this
            // wire decodes them back into native image blocks.
            vision: true,
            prompt_caching: true,
            // The models can think; ZR's compat surface has nowhere to carry
            // it, so this wire does not request it.
            extended_thinking: false,
        }
    }

    fn default_base_url(&self) -> Option<&str> {
        Some(&self.api_url)
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let response = self.chat(request, model, temperature).await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let body = self.request_body(model, request.messages, request.tools, temperature, false);
        let response = self
            .http
            .post(&self.api_url)
            .header("x-api-key", &self.credential)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        // The body read happens under the already-known status: a truncated
        // 401/429 body must not surface as a status-less transport error the
        // classifier would happily retry (sol review).
        let text = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<body unreadable: {error}>"));
        if !status.is_success() {
            return Err(anthropic_upstream_error(&self.alias, status, &text));
        }
        let envelope: MessagesEnvelope = serde_json::from_str(&text).map_err(|error| {
            anyhow!(
                "{} messages API returned unparseable JSON: {error}",
                self.alias
            )
        })?;
        Ok(parse_messages_envelope(envelope))
    }

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> futures_util::stream::BoxStream<'static, StreamResult<StreamEvent>> {
        let body = self.request_body(model, request.messages, request.tools, temperature, true);
        let http = self.http.clone();
        let api_url = self.api_url.clone();
        let credential = self.credential.clone();
        let alias = self.alias.clone();
        let count_tokens = options.count_tokens;

        let stream = async_stream::try_stream! {
            let response = http
                .post(&api_url)
                .header("x-api-key", &credential)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body)
                .send()
                .await
                .map_err(|error| StreamError::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                Err(StreamError::Http(
                    anthropic_upstream_error(&alias, status, &text).to_string(),
                ))?;
                return;
            }

            let mut bytes = response.bytes_stream();
            // Same byte-accurate UTF-8 buffering as the Responses wire: a
            // multibyte character split across network chunks must never
            // become a replacement character in a customer's stream.
            let mut raw_buffer: Vec<u8> = Vec::new();
            let mut buffer = String::new();
            let mut machine = AnthropicStreamMachine::new(count_tokens);
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|error| StreamError::Http(error.to_string()))?;
                raw_buffer.extend_from_slice(&chunk);
                let valid_up_to = match std::str::from_utf8(&raw_buffer) {
                    Ok(_) => raw_buffer.len(),
                    Err(error) => error.valid_up_to(),
                };
                buffer.push_str(
                    std::str::from_utf8(&raw_buffer[..valid_up_to])
                        .expect("prefix was just validated"),
                );
                raw_buffer.drain(..valid_up_to);

                while let Some((boundary, delimiter_len)) = find_event_boundary(&buffer) {
                    let raw = buffer[..boundary].to_owned();
                    buffer.drain(..boundary + delimiter_len);
                    let data = raw
                        .lines()
                        .map(|line| line.strip_suffix('\r').unwrap_or(line))
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if data.is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(&data)
                        .map_err(StreamError::Json)?;
                    match machine.handle(&alias, &value) {
                        Ok(events) => {
                            for event in events {
                                yield event;
                            }
                        }
                        Err(error) => {
                            // Usage the wire already reported is billable
                            // even though the stream is failing — emit it
                            // ahead of the error so the settle path meters
                            // it instead of charging zero.
                            if let Some(usage) = machine.partial_usage() {
                                yield StreamEvent::Usage(usage);
                            }
                            Err(error)?;
                        }
                    }
                }
                if machine.finished {
                    break;
                }
            }
            if !machine.finished {
                // The upstream closed without `message_stop`: surface it
                // rather than synthesizing a Final the wire never sent —
                // but surface the reported-so-far usage first, same rule as
                // the in-band error arm.
                if let Some(usage) = machine.partial_usage() {
                    yield StreamEvent::Usage(usage);
                }
                Err(StreamError::InvalidSse(format!(
                    "{alias} messages stream ended without completion"
                )))?;
            }
        };
        Box::pin(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(messages: &[(&str, &str)]) -> Vec<ChatMessage> {
        messages
            .iter()
            .map(|(role, content)| match *role {
                "system" => ChatMessage::system(*content),
                "user" => ChatMessage::user(*content),
                "assistant" => ChatMessage::assistant(*content),
                "tool" => ChatMessage::tool(*content),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn input_builder_handles_every_zr_packing_shape() {
        let messages = packed(&[
            ("system", "be terse"),
            ("user", "run pwd"),
            (
                "assistant",
                r#"{"content":"running it","tool_calls":[{"id":"call_1","name":"shell","arguments":"{\"command\":\"pwd\"}"}],"reasoning_content":null}"#,
            ),
            (
                "tool",
                r#"{"tool_call_id":"call_1","name":"shell","content":"/home"}"#,
            ),
            ("assistant", "done: /home"),
        ]);
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, "be terse");
        let types: Vec<&str> = input
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "message",
                "message",
                "function_call",
                "function_call_output",
                "message"
            ]
        );
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "/home");
        assert_eq!(input[4]["content"][0]["text"], "done: /home");
    }

    #[test]
    fn envelope_parse_lifts_usage_text_and_tool_calls() {
        let envelope: ResponsesEnvelope = serde_json::from_value(json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "hello "},
                             {"type": "output_text", "text": "world"}]},
                {"type": "function_call", "call_id": "c9", "name": "shell",
                 "arguments": "{}"}
            ],
            "usage": {"input_tokens": 40, "output_tokens": 9,
                       "input_tokens_details": {"cached_tokens": 12}}
        }))
        .expect("envelope parses");
        let response = parse_envelope(envelope);
        assert_eq!(response.text.as_deref(), Some("hello world"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "c9");
        let usage = response.usage.expect("usage is the point of this module");
        assert_eq!(usage.input_tokens, Some(40));
        assert_eq!(usage.output_tokens, Some(9));
        assert_eq!(usage.cached_input_tokens, Some(12));
    }

    #[test]
    fn missing_usage_stays_none_never_zero() {
        let envelope: ResponsesEnvelope =
            serde_json::from_value(json!({"output": []})).expect("envelope parses");
        let response = parse_envelope(envelope);
        assert!(
            response.usage.is_none(),
            "no usage on the wire = None; downstream policy decides"
        );
    }

    #[test]
    fn error_text_speaks_the_classifier_taxonomy() {
        let error = upstream_error(
            "openai",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "Rate limit reached for gpt-5.6-luna",
        );
        assert!(crate::retry::is_rate_limited(&error), "{error}");

        let error = upstream_error(
            "openai",
            reqwest::StatusCode::BAD_REQUEST,
            "maximum context length exceeded",
        );
        assert!(
            matches!(
                crate::retry::classify(&error, false),
                crate::retry::FailureClass::ContextWindow { .. }
            ),
            "{error}"
        );
    }
}

#[cfg(test)]
mod review_fix_tests {
    use super::*;

    #[test]
    fn incomplete_is_a_terminal_event_shape() {
        // The streaming arm matches completed|incomplete identically; this
        // pins the envelope side: an incomplete response still lifts usage,
        // so a max_output_tokens clip is billable, never a free delivery.
        let envelope: ResponsesEnvelope = serde_json::from_value(serde_json::json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "clipped"}]}],
            "usage": {"input_tokens": 20, "output_tokens": 64}
        }))
        .expect("envelope parses");
        let response = parse_envelope(envelope);
        assert_eq!(response.text.as_deref(), Some("clipped"));
        let usage = response.usage.expect("clipped output still meters");
        assert_eq!(usage.output_tokens, Some(64));
    }

    #[test]
    fn event_boundaries_handle_lf_and_crlf_framing() {
        assert_eq!(find_event_boundary("data: a\n\nrest"), Some((7, 2)));
        assert_eq!(find_event_boundary("data: a\r\n\r\nrest"), Some((7, 4)));
        assert_eq!(find_event_boundary("data: a\r\n"), None);
        // Mixed stream: the earlier boundary wins.
        assert_eq!(find_event_boundary("a\n\nb\r\n\r\nc"), Some((1, 2)));
    }

    #[test]
    fn a_json_object_assistant_reply_is_not_swallowed() {
        // A model that answered in pure JSON round-trips as plain assistant
        // text — only ZR's own packing markers make an envelope.
        let messages = vec![ChatMessage::assistant(r#"{"answer":42}"#)];
        let (_, input) = build_responses_input(&messages);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], r#"{"answer":42}"#);
    }

    #[test]
    fn split_multibyte_utf8_survives_chunking() {
        // The decoder logic under test: hold back an incomplete UTF-8 tail.
        let text = "naïve — 日本語";
        let bytes = text.as_bytes();
        let mut raw: Vec<u8> = Vec::new();
        let mut out = String::new();
        for chunk in bytes.chunks(1) {
            raw.extend_from_slice(chunk);
            let valid_up_to = match std::str::from_utf8(&raw) {
                Ok(_) => raw.len(),
                Err(error) => error.valid_up_to(),
            };
            out.push_str(std::str::from_utf8(&raw[..valid_up_to]).unwrap());
            raw.drain(..valid_up_to);
        }
        assert_eq!(out, text);
        assert!(raw.is_empty());
    }

    #[test]
    fn in_band_rate_limit_shapes_classify_as_429() {
        // Mirror of the streaming arm's prefixing rule, run through the
        // real classifier.
        let error = anyhow!(
            "openai responses stream failed: 429 Too Many Requests: {}",
            r#"{"code":"rate_limit_exceeded","message":"Rate limit reached"}"#
        );
        assert!(crate::retry::is_rate_limited(&error));
    }
}

#[cfg(test)]
mod anthropic_tests {
    use super::*;

    #[test]
    fn message_builder_handles_every_zr_packing_shape() {
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("run pwd"),
            ChatMessage::assistant(
                r#"{"content":"running it","tool_calls":[{"id":"toolu_1","name":"shell","arguments":"{\"command\":\"pwd\"}"}],"reasoning_content":null}"#,
            ),
            ChatMessage::tool(r#"{"tool_call_id":"toolu_1","name":"shell","content":"/home"}"#),
            ChatMessage::assistant("done: /home"),
        ];
        let (system, turns) = build_anthropic_messages(&messages);
        assert_eq!(system, "be terse");
        let roles: Vec<&str> = turns
            .iter()
            .map(|turn| turn["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        // The assistant envelope turn carries text AND the tool_use block.
        assert_eq!(turns[1]["content"][0]["type"], "text");
        assert_eq!(turns[1]["content"][0]["text"], "running it");
        assert_eq!(turns[1]["content"][1]["type"], "tool_use");
        assert_eq!(turns[1]["content"][1]["id"], "toolu_1");
        // Arguments arrive as a JSON STRING and must land as an OBJECT.
        assert_eq!(turns[1]["content"][1]["input"]["command"], "pwd");
        // The tool result is a tool_result block in a USER turn.
        assert_eq!(turns[2]["content"][0]["type"], "tool_result");
        assert_eq!(turns[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(turns[2]["content"][0]["content"], "/home");
    }

    #[test]
    fn parallel_tool_results_merge_into_one_user_turn() {
        // The Messages API requires strict role alternation; two consecutive
        // packed tool results must become two blocks in ONE user turn, not
        // two user turns.
        let messages = vec![
            ChatMessage::user("run both"),
            ChatMessage::assistant(
                r#"{"content":null,"tool_calls":[{"id":"a","name":"x","arguments":"{}"},{"id":"b","name":"y","arguments":"{}"}]}"#,
            ),
            ChatMessage::tool(r#"{"tool_call_id":"a","name":"x","content":"one"}"#),
            ChatMessage::tool(r#"{"tool_call_id":"b","name":"y","content":"two"}"#),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        let roles: Vec<&str> = turns
            .iter()
            .map(|turn| turn["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(turns[1]["content"].as_array().unwrap().len(), 2);
        let results = turns[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "a");
        assert_eq!(results[1]["tool_use_id"], "b");
    }

    #[test]
    fn a_json_object_assistant_reply_is_not_swallowed() {
        let messages = vec![ChatMessage::assistant(r#"{"answer":42}"#)];
        let (_, turns) = build_anthropic_messages(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["content"][0]["type"], "text");
        assert_eq!(turns[0]["content"][0]["text"], r#"{"answer":42}"#);
    }

    #[test]
    fn envelope_parse_normalizes_usage_to_the_openai_convention() {
        // THE reason this wire exists: Anthropic's input_tokens excludes the
        // cache dimensions; ZR prices cached as a subset of input. 100 raw +
        // 30 cache-read + 10 cache-write must meter as input=140, cached=30.
        let envelope: MessagesEnvelope = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"},
                {"type": "tool_use", "id": "toolu_9", "name": "shell",
                 "input": {"command": "pwd"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 9,
                       "cache_read_input_tokens": 30,
                       "cache_creation_input_tokens": 10}
        }))
        .expect("envelope parses");
        let response = parse_messages_envelope(envelope);
        assert_eq!(response.text.as_deref(), Some("hello world"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_9");
        let arguments: Value =
            serde_json::from_str(&response.tool_calls[0].arguments).expect("arguments are JSON");
        assert_eq!(arguments["command"], "pwd");
        let usage = response.usage.expect("usage is the point of this module");
        assert_eq!(usage.input_tokens, Some(140));
        assert_eq!(usage.output_tokens, Some(9));
        assert_eq!(usage.cached_input_tokens, Some(30));
    }

    #[test]
    fn missing_usage_stays_none_never_zero() {
        let envelope: MessagesEnvelope =
            serde_json::from_value(json!({"content": []})).expect("envelope parses");
        assert!(parse_messages_envelope(envelope).usage.is_none());
    }

    #[test]
    fn stream_machine_assembles_the_documented_event_grammar() {
        let mut machine = AnthropicStreamMachine::new(false);
        let sequence = [
            json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 100, "output_tokens": 1,
                "cache_read_input_tokens": 30}}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "hel"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "toolu_1",
                                      "name": "shell"}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta",
                             "partial_json": "{\"comman"}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta",
                             "partial_json": "d\":\"pwd\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                   "usage": {"output_tokens": 17}}),
            json!({"type": "message_stop"}),
        ];
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = None;
        let mut finals = 0;
        for value in &sequence {
            for event in machine
                .handle("anthropic", value)
                .expect("no in-band error")
            {
                match event {
                    StreamEvent::TextDelta(chunk) => text.push_str(&chunk.delta),
                    StreamEvent::ToolCall(call) => tool_calls.push(call),
                    StreamEvent::Usage(u) => usage = Some(u),
                    StreamEvent::Final => finals += 1,
                    _ => {}
                }
            }
        }
        assert!(machine.finished);
        assert_eq!(finals, 1, "message_stop is the only terminal");
        assert_eq!(text, "hello");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_1");
        assert_eq!(tool_calls[0].arguments, r#"{"command":"pwd"}"#);
        let usage = usage.expect("usage assembled from start + delta");
        // 100 raw + 30 cache-read, normalized; output from the FINAL delta,
        // not message_start's placeholder.
        assert_eq!(usage.input_tokens, Some(130));
        assert_eq!(usage.cached_input_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(17));
    }

    #[test]
    fn in_band_rate_limit_classifies_as_429_and_overloaded_stays_retryable() {
        let mut machine = AnthropicStreamMachine::new(false);
        let error = machine
            .handle(
                "anthropic",
                &json!({"type": "error",
                        "error": {"type": "rate_limit_error",
                                   "message": "Number of requests has exceeded your rate limit"}}),
            )
            .expect_err("an in-band error must fail the stream");
        let error = anyhow!(error.to_string());
        assert!(crate::retry::is_rate_limited(&error), "{error}");

        let mut machine = AnthropicStreamMachine::new(false);
        let error = machine
            .handle(
                "anthropic",
                &json!({"type": "error",
                        "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            )
            .expect_err("an in-band error must fail the stream");
        let error = anyhow!(error.to_string());
        assert!(
            matches!(
                crate::retry::classify(&error, false),
                crate::retry::FailureClass::Retryable
            ),
            "{error}"
        );
    }

    #[test]
    fn error_text_speaks_the_classifier_taxonomy() {
        let error = anthropic_upstream_error(
            "anthropic",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "Number of requests has exceeded your rate limit",
        );
        assert!(crate::retry::is_rate_limited(&error), "{error}");

        let error = anthropic_upstream_error(
            "anthropic",
            reqwest::StatusCode::BAD_REQUEST,
            "prompt is too long: 210000 tokens > 200000 maximum",
        );
        assert!(
            matches!(
                crate::retry::classify(&error, false),
                crate::retry::FailureClass::ContextWindow { .. }
            ),
            "{error}"
        );
    }
}

#[cfg(test)]
mod anthropic_review_fix_tests {
    use super::*;

    #[test]
    fn partial_usage_survives_an_in_band_error() {
        // Anthropic reports input at message_start and output at
        // message_delta; a stream that dies after those events has
        // wire-reported billable usage that must not settle as zero.
        let mut machine = AnthropicStreamMachine::new(false);
        machine
            .handle(
                "anthropic",
                &json!({"type": "message_start", "message": {"usage": {
                    "input_tokens": 50, "output_tokens": 1}}}),
            )
            .expect("message_start is not an error");
        machine
            .handle(
                "anthropic",
                &json!({"type": "message_delta", "usage": {"output_tokens": 40}}),
            )
            .expect("message_delta is not an error");
        machine
            .handle(
                "anthropic",
                &json!({"type": "error",
                        "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            )
            .expect_err("the error event fails the stream");
        let usage = machine
            .partial_usage()
            .expect("reported usage is recoverable after the error");
        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(40));
        assert!(
            machine.partial_usage().is_none(),
            "partial usage is taken once, never double-emitted"
        );
    }

    #[test]
    fn request_body_sets_the_three_cache_breakpoints() {
        let wire = AnthropicWire::new("anthropic", "k", None, 512, 900);
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("first question"),
            ChatMessage::assistant("first answer"),
            ChatMessage::user("second question"),
        ];
        let spec = |name: &str, description: &str| zeroclaw_api::tool::ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: std::sync::Arc::new(json!({"type": "object"})),
            output: None,
            param_domains: std::collections::BTreeMap::new(),
        };
        let tools = vec![spec("a", "first"), spec("b", "last")];
        let body = wire.request_body("claude-sonnet-5", &messages, Some(&tools), None, false);
        // System is a block array carrying the marker.
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        // Only the LAST tool is marked.
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        // The last block of the final turn is marked; earlier turns are not.
        let turns = body["messages"].as_array().unwrap();
        let last_block = turns.last().unwrap()["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        assert!(turns[0]["content"][0].get("cache_control").is_none());
        // Under the API's four-breakpoint limit.
        assert_eq!(body.to_string().matches("cache_control").count(), 3);
    }

    #[test]
    fn an_interrupted_parallel_tool_history_is_backfilled() {
        // Calls a and b went out, only a's result came back before the user
        // typed again. The wire must synthesize b's result or the API 400s.
        let messages = vec![
            ChatMessage::user("run both"),
            ChatMessage::assistant(
                r#"{"content":null,"tool_calls":[{"id":"a","name":"x","arguments":"{}"},{"id":"b","name":"y","arguments":"{}"}]}"#,
            ),
            ChatMessage::tool(r#"{"tool_call_id":"a","name":"x","content":"one"}"#),
            ChatMessage::user("never mind, stop"),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        let results: Vec<&str> = turns[2]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block["type"] == "tool_result")
            .map(|block| block["tool_use_id"].as_str().unwrap())
            .collect();
        assert!(results.contains(&"a"));
        assert!(results.contains(&"b"), "missing result is synthesized");
        // The trailing user text survives in the same merged turn.
        assert!(
            turns[2]["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["type"] == "text")
        );
    }

    #[test]
    fn a_history_ending_on_tool_use_gains_a_result_turn() {
        let messages = vec![
            ChatMessage::user("run it"),
            ChatMessage::assistant(
                r#"{"content":null,"tool_calls":[{"id":"only","name":"x","arguments":"{}"}]}"#,
            ),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        assert_eq!(turns.len(), 3, "a user turn is synthesized at the end");
        assert_eq!(turns[2]["role"], "user");
        assert_eq!(turns[2]["content"][0]["type"], "tool_result");
        assert_eq!(turns[2]["content"][0]["tool_use_id"], "only");
    }

    #[test]
    fn empty_and_whitespace_content_never_becomes_a_block() {
        let messages = vec![
            ChatMessage::user(""),
            ChatMessage::user("   "),
            ChatMessage::assistant(""),
            ChatMessage::user("real question"),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        assert_eq!(turns.len(), 1, "only the real question survives");
        assert_eq!(turns[0]["content"][0]["text"], "real question");
        // Empty tool output omits `content` instead of sending "".
        let messages = vec![
            ChatMessage::user("go"),
            ChatMessage::assistant(
                r#"{"content":null,"tool_calls":[{"id":"t","name":"x","arguments":"{}"}]}"#,
            ),
            ChatMessage::tool(r#"{"tool_call_id":"t","name":"x","content":""}"#),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        assert!(turns[2]["content"][0].get("content").is_none());
    }

    #[test]
    fn image_markers_become_native_image_blocks() {
        let messages = vec![ChatMessage::user(
            "what is this? [IMAGE:data:image/png;base64,AAAA] and this? [IMAGE:https://example.com/x.jpg]",
        )];
        let (_, turns) = build_anthropic_messages(&messages);
        let blocks = turns[0]["content"].as_array().unwrap();
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["text", "image", "text", "image"]);
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
        assert_eq!(blocks[3]["source"]["type"], "url");
        assert_eq!(blocks[3]["source"]["url"], "https://example.com/x.jpg");
        // A malformed marker stays literal text rather than vanishing (the
        // preceding text and the unterminated marker land as text blocks in
        // the same turn).
        let messages = vec![ChatMessage::user("broken [IMAGE:no-close")];
        let (_, turns) = build_anthropic_messages(&messages);
        let joined: String = turns[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        assert_eq!(joined, "broken [IMAGE:no-close");
    }
}

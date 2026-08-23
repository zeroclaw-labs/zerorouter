//! Increment 1: the OpenAI Responses wire (`/v1/responses`), the family the
//! pinned adapters cannot meter. Reasoning ITEMS from responses histories
//! are not round-tripped (ZeroRouter's compat surface has nowhere to carry
//! them); text and tool-call flows are complete. The error strings these
//! clients produce deliberately carry the HTTP status digits and the
//! upstream body verbatim (sanitized downstream by the retention layer), so
//! `retry::classify`'s status/heuristic taxonomy reads them exactly as it
//! reads the pinned adapters' errors.

use crate::provider::ChatMessage;
use crate::provider::{
    ChatRequest, ChatResponse, ModelProvider, ProviderCapabilities, StopReason, StreamChunk,
    StreamError, StreamEvent, StreamFinal, StreamOptions, StreamResult, TokenUsage, ToolCall,
};
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    MAX_RESPONSE_BYTES, believable, bounded_body, drain_sse_payloads, shared_upstream_clients,
};

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
    /// Same budget plus an idle ceiling; used only by `stream_chat`.
    stream_http: reqwest::Client,
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
        let (http, stream_http) = shared_upstream_clients(timeout_secs);
        Self {
            alias: alias.to_owned(),
            api_url: api_url
                .map(|url| url.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| RESPONSES_URL.to_owned()),
            credential: credential.to_owned(),
            max_tokens,
            http,
            stream_http,
        }
    }

    fn request_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[crate::provider::ToolSpec]>,
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
pub(super) fn build_responses_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut input = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "system" => system_parts.push(&message.content),
            "user" => input.push(user_item(message)),
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

fn message_item(role: &str, content_type: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// One user turn, with any structured image content mapped into this dialect's
/// image parts.
///
/// The Responses API does NOT reuse chat-completions' image shape, and the
/// difference is easy to get wrong in both directions: here the part is
/// `input_image` and `image_url` is a BARE STRING ("a fully qualified URL or
/// base64 encoded image in a data URL"), not the `{"url":…}` object
/// chat-completions takes. `detail` is a REQUIRED field on this part
/// (openai/openai-openapi, `InputImageContent`, read 2026-08-21), so it is
/// sent explicitly as `auto` — the schema's own default — rather than
/// omitted and left to the server.
///
/// A turn with no image keeps exactly the single `input_text` item it built
/// before, so every existing pin in this module is untouched.
fn user_item(message: &ChatMessage) -> Value {
    // No structured content: the string is the turn, verbatim. Checked before
    // anything else so the plain-text path cannot be reached by any grammar.
    if message.parts.is_empty() {
        return message_item("user", "input_text", &message.content);
    }
    let parts = super::user_parts(message);
    if !parts
        .iter()
        .any(|part| !matches!(part, super::UserPart::Text(_)))
    {
        return message_item("user", "input_text", &message.content);
    }
    let content: Vec<Value> = parts
        .into_iter()
        .map(|part| match part {
            super::UserPart::Text(text) => json!({ "type": "input_text", "text": text }),
            super::UserPart::Base64Image { media_type, data } => json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
                "detail": "auto",
            }),
            super::UserPart::UrlImage(url) => json!({
                "type": "input_image",
                "image_url": url,
                "detail": "auto",
            }),
        })
        .collect();
    json!({ "type": "message", "role": "user", "content": content })
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
    /// the cached-input subset, straight from the wire — subject to
    /// [`believable`], because "straight from the wire" must not mean
    /// "whatever the upstream says".
    fn into_token_usage(self) -> Option<TokenUsage> {
        believable(TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self
                .input_tokens_details
                .and_then(|details| details.cached_tokens),
        })
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
    let stop_reason = responses_stop_reason(
        envelope.status.as_deref(),
        envelope.incomplete_details.as_ref(),
        !tool_calls.is_empty(),
    );
    ChatResponse {
        text,
        tool_calls,
        usage: envelope.usage.and_then(ResponsesUsage::into_token_usage),
        reasoning_content: None,
        stop_reason,
    }
}

/// The Responses API's terminal state, normalized to the router's vocabulary.
///
/// This dialect splits across two fields what the chat dialect says in one:
/// `status` says whether the run finished, and `incomplete_details.reason`
/// says why it did not. The mapping:
///
/// * `incomplete` + `max_output_tokens` → [`StopReason::Length`]. The clipped
///   answer the router previously had to INFER from token arithmetic.
/// * `incomplete` + `content_filter` → [`StopReason::ContentFilter`], a state
///   this module's header records as structurally unobservable before now.
/// * `completed` → [`StopReason::ToolCalls`] when the output carried function
///   calls, else [`StopReason::Stop`]. A completed run that emitted tool calls
///   is exactly what the chat dialect spells `tool_calls`, and reporting it as
///   `stop` would make the two wires disagree about the same event.
/// * anything else — `failed`, `in_progress`, an absent status, an unmapped
///   incomplete reason — is `None`. A run the router cannot classify has no
///   real stop reason, and the synthesis path handles it unchanged.
fn responses_stop_reason(
    status: Option<&str>,
    incomplete_details: Option<&Value>,
    has_tool_calls: bool,
) -> Option<StopReason> {
    match status {
        Some("completed") => Some(if has_tool_calls {
            StopReason::ToolCalls
        } else {
            StopReason::Stop
        }),
        Some("incomplete") => match incomplete_details
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => Some(StopReason::Length),
            Some("content_filter") => Some(StopReason::ContentFilter),
            _ => None,
        },
        _ => None,
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

#[async_trait]
impl ModelProvider for OpenAiResponsesWire {
    fn alias(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.alias)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            // ZR packs OpenAI image parts as `[IMAGE:<url>]` markers; this
            // wire decodes them into `input_image` parts (the Responses
            // dialect's own shape, not chat-completions').
            vision: true,
            prompt_caching: true,
        }
    }

    fn supports_streaming(&self) -> bool {
        true
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
        // transport error. Bounded, because a hostile upstream can otherwise
        // stream a body until the process dies.
        let (text, truncated) = bounded_body(response).await;
        if !status.is_success() {
            return Err(upstream_error(&self.alias, status, &text));
        }
        if truncated {
            return Err(anyhow!(
                "{} responses API body exceeded {MAX_RESPONSE_BYTES} bytes",
                self.alias
            ));
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
        let http = self.stream_http.clone();
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
                let (text, _) = bounded_body(response).await;
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
            // Whether this stream emitted any function call, which is what
            // separates a completed run's `tool_calls` terminal from its
            // `stop` one — the buffered path reads the same fact off the
            // assembled output items.
            let mut saw_tool_call = false;
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|error| StreamError::Http(error.to_string()))?;
                for data in drain_sse_payloads(&mut raw_buffer, &mut buffer, &chunk)? {
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
                                saw_tool_call = true;
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
                            // Idempotent terminal, same rule as the
                            // Anthropic machine: a repeated terminal inside
                            // one chunk must not emit a second Final or a
                            // second Usage.
                            if finished {
                                continue;
                            }
                            if let Some(usage) = value
                                .get("response")
                                .and_then(|response| response.get("usage"))
                                && let Ok(usage) =
                                    serde_json::from_value::<ResponsesUsage>(usage.clone())
                                && let Some(usage) = usage.into_token_usage()
                            {
                                yield StreamEvent::Usage(usage);
                            }
                            finished = true;
                            // Read from the SAME two fields the buffered path
                            // reads, off the response object this terminal
                            // event carries, so a clip reports `length` here
                            // exactly as it does there.
                            let response = value.get("response");
                            yield StreamEvent::Final(StreamFinal::with_stop_reason(
                                responses_stop_reason(
                                    response
                                        .and_then(|response| response.get("status"))
                                        .and_then(Value::as_str),
                                    response
                                        .and_then(|response| response.get("incomplete_details")),
                                    saw_tool_call,
                                ),
                            ));
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

    /// The Responses dialect's terminal state, normalized. `status` and
    /// `incomplete_details` were read and discarded (`let _ = ...`) before the
    /// provider contract had anywhere to carry a real stop reason.
    #[test]
    fn responses_terminal_states_normalize_to_the_router_vocabulary() {
        let table = [
            // (status, incomplete reason, has tool calls, expected)
            (Some("completed"), None, false, Some(StopReason::Stop)),
            (Some("completed"), None, true, Some(StopReason::ToolCalls)),
            (
                Some("incomplete"),
                Some("max_output_tokens"),
                false,
                Some(StopReason::Length),
            ),
            (
                Some("incomplete"),
                Some("content_filter"),
                false,
                Some(StopReason::ContentFilter),
            ),
            // A clip is a clip whether or not tool calls were emitted: the
            // incomplete reason is the upstream's own word and outranks the
            // presence of output items.
            (
                Some("incomplete"),
                Some("max_output_tokens"),
                true,
                Some(StopReason::Length),
            ),
            // Absent stays absent — never guessed into `stop`.
            (Some("incomplete"), None, false, None),
            (Some("incomplete"), Some("something_new"), false, None),
            (Some("failed"), None, false, None),
            (Some("in_progress"), None, false, None),
            (None, None, false, None),
        ];
        for (status, reason, has_tool_calls, expected) in table {
            let details = reason.map(|reason| json!({"reason": reason}));
            assert_eq!(
                responses_stop_reason(status, details.as_ref(), has_tool_calls),
                expected,
                "status={status:?} reason={reason:?} tools={has_tool_calls}"
            );
        }
    }

    #[test]
    fn responses_envelope_carries_the_real_stop_reason() {
        let envelope: ResponsesEnvelope = serde_json::from_value(json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "clipped"}]}]
        }))
        .expect("envelope parses");
        assert_eq!(
            parse_envelope(envelope).stop_reason,
            Some(StopReason::Length),
            "a max_output_tokens clip is the upstream's own word, not token arithmetic"
        );
    }

    #[test]
    fn responses_envelope_without_a_status_reports_no_stop_reason() {
        let envelope: ResponsesEnvelope =
            serde_json::from_value(json!({"output": []})).expect("envelope parses");
        assert_eq!(
            parse_envelope(envelope).stop_reason,
            None,
            "absent stays None so the synthesis path still runs"
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
    use crate::provider::ContentPart;

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

    /// The byte pin for this dialect's image parts, and the reason it is a
    /// SEPARATE pin from the chat-completions one: the Responses API does not
    /// reuse that shape. Here the part is `input_image`, `image_url` is a
    /// BARE STRING rather than a `{"url":…}` object, and `detail` is a
    /// REQUIRED field (openai/openai-openapi, `InputImageContent`). Sending
    /// chat-completions' shape here, or this shape there, is the specific
    /// mistake this pin and its twin exist to catch.
    #[test]
    fn image_parts_become_responses_input_image_parts_byte_for_byte() {
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::Text("what is this? ".to_owned()),
            ContentPart::Image("data:image/png;base64,AAAA".to_owned()),
            ContentPart::Text(" and this? ".to_owned()),
            ContentPart::Image("https://example.com/x.jpg".to_owned()),
        ])];
        let (_, input) = build_responses_input(&messages);
        assert_eq!(
            serde_json::to_string(&input).unwrap(),
            concat!(
                r#"[{"content":["#,
                r#"{"text":"what is this? ","type":"input_text"},"#,
                r#"{"detail":"auto","image_url":"data:image/png;base64,AAAA","type":"input_image"},"#,
                r#"{"text":" and this? ","type":"input_text"},"#,
                r#"{"detail":"auto","image_url":"https://example.com/x.jpg","type":"input_image"}"#,
                r#"],"role":"user","type":"message"}]"#,
            )
        );
    }

    /// A turn with no image builds exactly the single `input_text` item it
    /// always did, so every other pin in this module is untouched.
    #[test]
    fn a_text_only_turn_keeps_its_single_input_text_item() {
        let (_, input) = build_responses_input(&[ChatMessage::user("plain question")]);
        assert_eq!(
            serde_json::to_string(&input).unwrap(),
            r#"[{"content":[{"text":"plain question","type":"input_text"}],"role":"user","type":"message"}]"#
        );
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

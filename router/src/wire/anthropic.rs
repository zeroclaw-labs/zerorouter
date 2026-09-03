//! Increment 2: the Anthropic Messages wire (`/v1/messages`), replacing the
//! pinned Anthropic adapter on first-party traffic. The billing-grade
//! difference is usage NORMALIZATION, not just extraction: Anthropic's
//! `usage.input_tokens` EXCLUDES cache reads and cache writes, while
//! ZeroRouter's cost function (`openai::usage_cost`) prices `cached` as a
//! subset of `input` — the OpenAI convention every other wire reports in.
//! This client folds `cache_read_input_tokens` and
//! `cache_creation_input_tokens` back into the input total and reports each as
//! its own disjoint subset — `cached_input_tokens` and
//! `cache_write_input_tokens` — so an Anthropic response meters on the same
//! axes as everything else.
//!
//! # Cache breakpoints: the wire's three, or the client's
//!
//! By DEFAULT this wire sets three `cache_control` breakpoints of its own
//! (system, last tool, last turn) exactly as the pinned adapter did, so the
//! upstream cache discount survives the swap on traffic that asks for nothing.
//!
//! When the CLIENT places breakpoints — `cache_control: {"type":"ephemeral"}`
//! on a chat-completions message or tool, admitted only on lanes that price
//! the dimension — the wire forwards the client's placement and sets NONE of
//! its own. It is one or the other and never both: Anthropic caps a request at
//! four breakpoints, so adding three to a client's four would 400 the request,
//! and silently merging them would mean the customer paying write premiums at
//! boundaries they did not choose. A client that sends breakpoints owns the
//! cache boundaries; a client that sends none gets the defaults it always got.
//!
//! Cache WRITES used to bill at the full input rate here while Anthropic
//! charged 1.25x — a COGS rounding this module's header recorded as accepted.
//! It is now a priced dimension (`cache_write_per_mtok`), so the premium is
//! metered rather than absorbed on the lanes that transcribe it.

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
    MAX_OPEN_TOOL_BLOCKS, MAX_RESPONSE_BYTES, MAX_TOOL_ARGUMENT_BYTES, believable, bounded_body,
    drain_sse_payloads, shared_upstream_clients,
};

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
    /// Same budget plus an idle ceiling; used only by `stream_chat`.
    stream_http: reqwest::Client,
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
        let (http, stream_http) = shared_upstream_clients(timeout_secs);
        Self {
            alias: alias.to_owned(),
            api_url: api_url
                .map(|url| url.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| MESSAGES_URL.to_owned()),
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
        messages_request_body(
            Some(model),
            messages,
            tools,
            temperature,
            Some(stream),
            self.max_tokens,
        )
    }
}

/// Build a Messages-API request body, shared by every wire that speaks this
/// dialect.
///
/// Extracted from `AnthropicWire::request_body` on 2026-08-20 so the Bedrock
/// classic-runtime wire (`super::bedrock_runtime`) sends the SAME body this one
/// does — same turn packing, same tool shape, and above all the same three
/// cache breakpoints. A second hand-written builder would have been the obvious
/// alternative and it is the wrong one: the breakpoints are what the
/// `cached_input_tokens` dimension exists to meter, and a copy that silently
/// lost one would forfeit the upstream cache discount on that lane's every
/// multi-turn conversation without failing anything.
///
/// Two parameters are `Option` precisely because the classic runtime differs
/// from the Messages endpoint in exactly two places, and nowhere else:
///
/// - `model` is `None` there, because the model id rides in the URL path
///   (`/model/{id}/invoke`) and AWS's InvokeModel body schema has no `model`
///   member at all — an extraneous key is refused with a `ValidationException`.
/// - `stream` is `None` there, because the runtime chooses streaming by URL
///   (`/invoke` vs `/invoke-with-response-stream`) rather than by a body flag.
///
/// Both omissions are asserted by the Bedrock wire's own contract tests, and
/// this function's output for the Messages case is pinned byte-for-byte by
/// `request_body_characterization` below.
pub(super) fn messages_request_body(
    model: Option<&str>,
    messages: &[ChatMessage],
    tools: Option<&[crate::provider::ToolSpec]>,
    temperature: Option<f64>,
    stream: Option<bool>,
    max_tokens: u32,
) -> Value {
    // Whose breakpoints are these? A request in which the CLIENT placed any
    // takes the client's placement and none of the wire's; a request that
    // placed none gets the wire's three defaults, exactly as before.
    //
    // It is one or the other because Anthropic caps a request at four
    // breakpoints: adding three defaults to a client's four would 400 the
    // request outright, and adding them to a client's one would silently
    // charge a write premium at three boundaries the customer did not choose.
    // Neither is a merge worth having, and "the client owns the cache
    // boundaries when it states any" is the rule a caching-aware agent
    // expects.
    let client_placed = messages.iter().any(ChatMessage::has_client_breakpoint)
        || tools.is_some_and(|tools| tools.iter().any(|tool| tool.cache_control));
    let (system, mut turns) = build_anthropic_messages(messages);
    // Cache breakpoints, matching what the pinned adapter set: the
    // system prompt, the last tool definition, and the last block of the
    // final turn. Dropping these on the swap would silently forfeit the
    // upstream cache discount on every multi-turn conversation (sol
    // review) — the discount the cached_input_tokens dimension exists to
    // meter. Three markers, under the API's limit of four.
    let cache_marker = json!({ "type": "ephemeral" });
    if !client_placed
        && let Some(block) = turns
            .last_mut()
            .and_then(|turn| turn["content"].as_array_mut())
            .and_then(|content| content.last_mut())
            .and_then(Value::as_object_mut)
    {
        block.insert("cache_control".to_owned(), cache_marker.clone());
    }
    let mut body = json!({
        "messages": turns,
        "max_tokens": max_tokens,
    });
    if let Some(model) = model {
        body["model"] = json!(model);
    }
    if let Some(stream) = stream {
        body["stream"] = json!(stream);
    }
    if !system.text.trim().is_empty() {
        let mut block = json!({
            "type": "text",
            "text": system.text,
        });
        // The system prompt is ONE block however many system turns produced
        // it, so a breakpoint on any of them marks that block. Under the
        // wire's own defaults it is always marked; under the client's, only
        // when the client asked.
        if if client_placed {
            system.cache_control
        } else {
            true
        } {
            block["cache_control"] = cache_marker.clone();
        }
        body["system"] = json!([block]);
    }
    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        let mut tools_json = tools
            .iter()
            .map(|tool| {
                let mut json = json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                });
                // A tool the client marked keeps its OWN position in the list.
                // The default below marks the last tool; a client that marks
                // the second of five means the second, because a breakpoint
                // caches everything above it and moving one changes both the
                // hit rate and the bill.
                if client_placed && tool.cache_control {
                    json["cache_control"] = cache_marker.clone();
                }
                json
            })
            .collect::<Vec<Value>>();
        if !client_placed && let Some(last) = tools_json.last_mut().and_then(Value::as_object_mut) {
            last.insert("cache_control".to_owned(), cache_marker);
        }
        body["tools"] = json!(tools_json);
    }
    if let Some(temperature) = temperature {
        body["temperature"] = json!(temperature);
    }
    body
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

/// Turn a user message's structured content into native Anthropic blocks —
/// sending an image as literal text would be a silent vision regression (sol
/// review). `data:` URIs become base64 sources, anything else a URL source.
///
/// The structural read itself lives in `wire::user_parts`, shared by all four
/// adapters; the block SHAPES below stay here because they are Anthropic's, and
/// the byte pin in `image_parts_become_native_image_blocks` covers both halves.
///
/// Verified against the Messages API image schema (platform.claude.com,
/// "Vision", read 2026-08-21): `source.type` is one of `base64` (with
/// `media_type` + `data`), `url`, or `file`.
fn push_user_content(turns: &mut Vec<Value>, message: &ChatMessage) {
    for (part, cache_control) in super::user_parts_cached(message) {
        let block = match part {
            super::UserPart::Text(text) => json!({ "type": "text", "text": text }),
            super::UserPart::Base64Image { media_type, data } => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                },
            }),
            super::UserPart::UrlImage(url) => json!({
                "type": "image",
                "source": { "type": "url", "url": url },
            }),
        };
        push_anthropic_block(turns, "user", block);
        // A client breakpoint on THIS part marks the block it just produced —
        // the Messages API caches everything up to and including a marked
        // block, so a mark on part 2 of 4 caches parts 1 and 2 and leaves the
        // rest fresh, which is the boundary the client named. The block was
        // just appended, so it is the last one; `mark_last_block` marks it.
        // This is the per-part twin of the message-level marking below.
        if cache_control {
            mark_last_block(turns);
        }
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
pub(super) fn build_anthropic_messages(messages: &[ChatMessage]) -> (SystemPrompt, Vec<Value>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut system_cache_control = false;
    let mut turns = Vec::new();

    for message in messages {
        // Where the last block sat BEFORE this message contributed anything,
        // so a client breakpoint can be placed on the block this message
        // actually produced. A message that produces none — empty or
        // whitespace-only content, which this builder deliberately drops —
        // must not have its breakpoint land on the previous message's block,
        // which would cache a boundary the client never named.
        let before = block_cursor(&turns);
        match message.role.as_str() {
            "system" => {
                system_parts.push(&message.content);
                system_cache_control |= message.cache_control;
            }
            "user" => push_user_content(&mut turns, message),
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
        // A client breakpoint marks the LAST block this message produced,
        // which is where the Messages API places the boundary for a turn:
        // everything above it, inclusive, becomes the cached prefix.
        if message.cache_control && block_cursor(&turns) != before {
            mark_last_block(&mut turns);
        }
    }

    // Runs AFTER every marker is placed, and safely so: it only ever inserts
    // synthesized `tool_result` blocks at the HEAD of a following user turn,
    // so nothing it does displaces a block that is already last.
    synthesize_missing_tool_results(&mut turns);
    (
        SystemPrompt {
            text: system_parts.join("\n\n"),
            cache_control: system_cache_control,
        },
        turns,
    )
}

/// The system prompt, plus whether the client asked for a breakpoint on it.
///
/// A struct rather than a tuple because the two travel together to exactly one
/// caller and a bare `(String, bool)` at that call site would read as anybody's
/// guess which bool it is.
#[derive(Debug)]
pub(super) struct SystemPrompt {
    pub(super) text: String,
    pub(super) cache_control: bool,
}

/// How many content blocks the turns hold in total — a monotonic cursor used
/// only to answer "did this message contribute anything".
fn block_cursor(turns: &[Value]) -> usize {
    turns
        .iter()
        .map(|turn| turn["content"].as_array().map_or(0, Vec::len))
        .sum()
}

/// Put a cache breakpoint on the last block of the last turn.
fn mark_last_block(turns: &mut [Value]) {
    if let Some(block) = turns
        .last_mut()
        .and_then(|turn| turn["content"].as_array_mut())
        .and_then(|content| content.last_mut())
        .and_then(Value::as_object_mut)
    {
        block.insert("cache_control".to_owned(), json!({ "type": "ephemeral" }));
    }
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
    /// Anthropic's three disjoint prompt buckets, normalized to the router's
    /// one-total-plus-two-subsets convention.
    ///
    /// `input_tokens` stays the SUM of all three, because that is what
    /// `prompt_tokens` means everywhere downstream — it is the figure the
    /// customer sees, the figure a conditional rate band is selected from
    /// ([`crate::provider::RateSchedule::at_prompt_tokens`]), and the figure
    /// every other wire reports. Removing cache creation from it would make an
    /// Anthropic prompt read shorter than it was and could select a cheaper
    /// long-context band than the vendor charged.
    ///
    /// What CHANGED when cache writes gained a price of their own is only the
    /// second half: the creation bucket is now carried out separately instead
    /// of disappearing into the total. Before this it was indistinguishable
    /// from a fresh read once summed, so `usage_cost` billed it at 1x while
    /// Anthropic invoiced 1.25x — the COGS rounding this module's header used
    /// to record as accepted. It is no longer accepted; it is metered.
    fn into_token_usage(self) -> Option<TokenUsage> {
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_input_tokens.unwrap_or(0);
        believable(TokenUsage {
            // Absent input stays None (the missing-usage path downstream);
            // present input becomes the OpenAI-convention TOTAL.
            input_tokens: self.input_tokens.map(|input| {
                input
                    .saturating_add(cache_read)
                    .saturating_add(cache_creation)
            }),
            output_tokens: self.output_tokens,
            cached_input_tokens: (cache_read > 0).then_some(cache_read),
            cache_write_input_tokens: (cache_creation > 0).then_some(cache_creation),
        })
    }
}

/// The Messages envelope fields billing needs.
#[derive(Deserialize)]
pub(super) struct MessagesEnvelope {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    /// Anthropic's own word for why generation ended. Not deserialized at all
    /// before the router had anywhere to put it.
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Anthropic's stop vocabulary, normalized to the router's.
///
/// * `end_turn` / `stop_sequence` → [`StopReason::Stop`]. A stop sequence is
///   still the model finishing on its own; OpenAI reports both as `stop`.
/// * `max_tokens` → [`StopReason::Length`].
/// * `tool_use` → [`StopReason::ToolCalls`].
/// * `refusal` → [`StopReason::ContentFilter`]. Anthropic's refusal stop is
///   the model's safety layer declining to produce the output, which is what
///   `content_filter` names on the OpenAI side.
/// * anything else — notably `pause_turn`, which means a server-tool turn is
///   to be CONTINUED rather than ended — is `None`. Mapping a pause onto any
///   of the four would assert a completion that did not happen; absent is the
///   honest answer and the synthesis path handles it unchanged.
fn anthropic_stop_reason(stop_reason: Option<&str>) -> Option<StopReason> {
    match stop_reason? {
        "end_turn" | "stop_sequence" => Some(StopReason::Stop),
        "max_tokens" => Some(StopReason::Length),
        "tool_use" => Some(StopReason::ToolCalls),
        "refusal" => Some(StopReason::ContentFilter),
        _ => None,
    }
}

pub(super) fn parse_messages_envelope(envelope: MessagesEnvelope) -> ChatResponse {
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
    let stop_reason = anthropic_stop_reason(envelope.stop_reason.as_deref());
    ChatResponse {
        text,
        tool_calls,
        usage: envelope.usage.and_then(AnthropicUsage::into_token_usage),
        reasoning_content: None,
        stop_reason,
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
pub(super) struct AnthropicStreamMachine {
    usage: AnthropicUsage,
    open_tools: std::collections::BTreeMap<u64, PendingAnthropicTool>,
    /// Cumulative tool-argument bytes across the whole stream.
    tool_argument_bytes: usize,
    count_tokens: bool,
    /// Anthropic's own stop reason, which arrives on `message_delta` — one
    /// event BEFORE the `message_stop` terminal that carries it out.
    stop_reason: Option<StopReason>,
    pub(super) finished: bool,
}

impl AnthropicStreamMachine {
    pub(super) fn new(count_tokens: bool) -> Self {
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
    pub(super) fn partial_usage(&mut self) -> Option<TokenUsage> {
        let usage = std::mem::take(&mut self.usage).into_token_usage()?;
        (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
    }

    fn absorb_usage(&mut self, value: Option<&Value>) {
        let Some(usage) = value else { return };
        // Field-by-field: `message_delta` carries cumulative output tokens
        // (and sometimes more) without repeating what `message_start` said —
        // absent fields must not erase known ones.
        // Anthropic's counters are CUMULATIVE, so they may only rise. Taking
        // the maximum rather than the latest value means a stale or replayed
        // frame reporting `output_tokens: 1` after 10_000 cannot shrink the
        // bill — or, just as bad, shrink the velocity window (sol review).
        let raise = |slot: &mut Option<u64>, field: &str| {
            if let Some(tokens) = usage.get(field).and_then(Value::as_u64) {
                *slot = Some(slot.map_or(tokens, |seen: u64| seen.max(tokens)));
            }
        };
        raise(&mut self.usage.input_tokens, "input_tokens");
        raise(&mut self.usage.output_tokens, "output_tokens");
        raise(
            &mut self.usage.cache_read_input_tokens,
            "cache_read_input_tokens",
        );
        raise(
            &mut self.usage.cache_creation_input_tokens,
            "cache_creation_input_tokens",
        );
    }

    pub(super) fn handle(
        &mut self,
        alias: &str,
        value: &Value,
    ) -> Result<Vec<StreamEvent>, StreamError> {
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
                    // The per-event cap bounds one frame; this bounds what a
                    // stream can accrete across legitimately terminated
                    // frames by never closing its blocks (sol review).
                    if self.open_tools.len() >= MAX_OPEN_TOOL_BLOCKS {
                        return Err(StreamError::InvalidSse(format!(
                            "{alias} opened more than {MAX_OPEN_TOOL_BLOCKS} concurrent tool blocks"
                        )));
                    }
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
                        {
                            // Cumulative across every open block: a stream
                            // that appends forever must not grow the process.
                            self.tool_argument_bytes =
                                self.tool_argument_bytes.saturating_add(partial.len());
                            if self.tool_argument_bytes > MAX_TOOL_ARGUMENT_BYTES {
                                return Err(StreamError::InvalidSse(format!(
                                    "{alias} tool arguments exceeded {MAX_TOOL_ARGUMENT_BYTES} bytes"
                                )));
                            }
                            if let Some(pending) = self.open_tools.get_mut(&index) {
                                pending.json.push_str(partial);
                            }
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
                // Last writer wins: the field is stated once in a conforming
                // stream, and a later restatement is the upstream's most
                // recent word either way. An unmappable value leaves the slot
                // alone rather than clearing a reason already seen.
                if let Some(reason) = anthropic_stop_reason(
                    value
                        .get("delta")
                        .and_then(|delta| delta.get("stop_reason"))
                        .and_then(Value::as_str),
                ) {
                    self.stop_reason = Some(reason);
                }
            }
            Some("message_stop") => {
                // Idempotent terminal: a chunk carrying two message_stop
                // events (upstream bug, or anything able to impersonate
                // one) must not yield two Finals — and must not yield a
                // second Usage that metering could read as authoritative.
                // Found by the event-order property test.
                if self.finished {
                    return Ok(events);
                }
                if let Some(usage) = std::mem::take(&mut self.usage).into_token_usage()
                    && (usage.input_tokens.is_some() || usage.output_tokens.is_some())
                {
                    events.push(StreamEvent::Usage(usage));
                }
                self.finished = true;
                events.push(StreamEvent::Final(StreamFinal::with_stop_reason(
                    self.stop_reason,
                )));
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

#[async_trait]
impl ModelProvider for AnthropicWire {
    fn alias(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.alias)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            // ZR packs OpenAI image parts as [IMAGE:<url>] markers; this
            // wire decodes them back into native image blocks.
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
            .header("x-api-key", &self.credential)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        // The body read happens under the already-known status: a truncated
        // 401/429 body must not surface as a status-less transport error the
        // classifier would happily retry (sol review). Bounded for the same
        // reason as its Responses sibling.
        let (text, truncated) = bounded_body(response).await;
        if !status.is_success() {
            return Err(anthropic_upstream_error(&self.alias, status, &text));
        }
        if truncated {
            return Err(anyhow!(
                "{} messages API body exceeded {MAX_RESPONSE_BYTES} bytes",
                self.alias
            ));
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
        let http = self.stream_http.clone();
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
                let (text, _) = bounded_body(response).await;
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
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(usage) = machine.partial_usage() {
                            yield StreamEvent::Usage(usage);
                        }
                        Err(StreamError::Http(error.to_string()))?;
                        return;
                    }
                };
                let payloads = match drain_sse_payloads(&mut raw_buffer, &mut buffer, &chunk) {
                    Ok(payloads) => payloads,
                    Err(error) => {
                        if let Some(usage) = machine.partial_usage() {
                            yield StreamEvent::Usage(usage);
                        }
                        Err(error)?;
                        return;
                    }
                };
                for data in payloads {
                    // A decode failure is an abnormal exit like any other:
                    // usage the upstream already reported is billable, and
                    // leaving through `?` here would settle delivered output
                    // at zero (sol review).
                    let value: Value = match serde_json::from_str::<Value>(&data) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Some(usage) = machine.partial_usage() {
                                yield StreamEvent::Usage(usage);
                            }
                            Err(StreamError::Json(error))?;
                            return;
                        }
                    };
                    match machine.handle(&alias, &value) {
                        Ok(events) => {
                            for event in events {
                                yield event;
                            }
                        }
                        Err(error) => {
                            // Same rule for an in-band error event.
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
        assert_eq!(system.text, "be terse");
        assert!(
            !system.cache_control,
            "no client asked for a breakpoint here, so the system prompt carries none of its own"
        );
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

    /// Anthropic's stop vocabulary, normalized. `stop_reason` was not even
    /// deserialized off this envelope before the contract could carry it.
    #[test]
    fn anthropic_stop_reasons_normalize_to_the_router_vocabulary() {
        let table = [
            (Some("end_turn"), Some(StopReason::Stop)),
            // A stop sequence is still the model ending on its own; OpenAI
            // reports both as `stop`.
            (Some("stop_sequence"), Some(StopReason::Stop)),
            (Some("max_tokens"), Some(StopReason::Length)),
            (Some("tool_use"), Some(StopReason::ToolCalls)),
            (Some("refusal"), Some(StopReason::ContentFilter)),
            // `pause_turn` means a server-tool turn is to be CONTINUED, not
            // ended. Mapping it onto any of the four would assert a completion
            // that did not happen.
            (Some("pause_turn"), None),
            (Some("something_new"), None),
            (None, None),
        ];
        for (reported, expected) in table {
            assert_eq!(
                anthropic_stop_reason(reported),
                expected,
                "stop_reason={reported:?}"
            );
        }
    }

    #[test]
    fn anthropic_envelope_carries_the_real_stop_reason() {
        let envelope: MessagesEnvelope = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "clipped"}],
            "stop_reason": "max_tokens",
        }))
        .expect("envelope parses");
        assert_eq!(
            parse_messages_envelope(envelope).stop_reason,
            Some(StopReason::Length)
        );
    }

    #[test]
    fn anthropic_envelope_without_a_stop_reason_reports_none() {
        let envelope: MessagesEnvelope =
            serde_json::from_value(json!({"content": []})).expect("envelope parses");
        assert_eq!(parse_messages_envelope(envelope).stop_reason, None);
    }

    /// The streaming half: Anthropic states `stop_reason` on `message_delta`,
    /// one event BEFORE the `message_stop` terminal that has to carry it out.
    #[test]
    fn anthropic_stream_terminal_carries_the_stop_reason_from_message_delta() {
        let mut machine = AnthropicStreamMachine::new(false);
        machine
            .handle(
                "anthropic",
                &json!({"type": "message_delta",
                        "delta": {"stop_reason": "max_tokens"},
                        "usage": {"output_tokens": 40}}),
            )
            .expect("message_delta is not an error");
        let events = machine
            .handle("anthropic", &json!({"type": "message_stop"}))
            .expect("message_stop is not an error");
        let Some(StreamEvent::Final(terminal)) = events.last() else {
            panic!("message_stop must yield the terminal");
        };
        assert_eq!(terminal.stop_reason, Some(StopReason::Length));
    }

    #[test]
    fn anthropic_stream_without_a_stop_reason_terminates_with_none() {
        let mut machine = AnthropicStreamMachine::new(false);
        let events = machine
            .handle("anthropic", &json!({"type": "message_stop"}))
            .expect("message_stop is not an error");
        let Some(StreamEvent::Final(terminal)) = events.last() else {
            panic!("message_stop must yield the terminal");
        };
        assert_eq!(terminal.stop_reason, None);
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
                    StreamEvent::Final(_) => finals += 1,
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
    use crate::provider::ContentPart;

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
        let spec = |name: &str, description: &str| crate::provider::ToolSpec {
            cache_control: false,
            name: name.into(),
            description: description.into(),
            parameters: (json!({"type": "object"})),
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
    fn a_client_that_places_breakpoints_gets_its_own_placement_and_not_the_wires() {
        // The transparency rule, and the whole point of accepting
        // `cache_control` at all: the boundaries the customer named are the
        // boundaries sent upstream, at the exact positions they named them.
        let wire = AnthropicWire::new("anthropic", "k", None, 512, 900);
        let messages = vec![
            ChatMessage::system("be terse").with_cache_breakpoint(),
            ChatMessage::user("first question").with_cache_breakpoint(),
            ChatMessage::assistant("first answer"),
            ChatMessage::user("second question"),
        ];
        let spec = |name: &str, marked: bool| crate::provider::ToolSpec {
            cache_control: marked,
            name: name.into(),
            description: "d".into(),
            parameters: (json!({"type": "object"})),
        };
        // The client marks the FIRST of two tools. The wire's own default
        // marks the last, so this placement is only reachable by honouring the
        // client — and it is the one that matters, because a breakpoint caches
        // everything above it and moving one changes both the hit rate and
        // the bill.
        let tools = vec![spec("a", true), spec("b", false)];
        let body = wire.request_body("claude-sonnet-5", &messages, Some(&tools), None, false);

        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert!(
            body["tools"][1].get("cache_control").is_none(),
            "the wire's default last-tool marker must not be added alongside the client's"
        );

        let turns = body["messages"].as_array().unwrap();
        assert_eq!(
            turns[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "the marked user turn keeps its breakpoint"
        );
        assert!(
            turns.last().unwrap()["content"][0]
                .get("cache_control")
                .is_none(),
            "and the wire's default final-turn marker is NOT added: three defaults on top of a \
             client's breakpoints would exceed the API's limit of four"
        );
        assert_eq!(
            body.to_string().matches("cache_control").count(),
            3,
            "exactly the three the client asked for"
        );
    }

    #[test]
    fn one_client_breakpoint_replaces_all_three_defaults() {
        // The rule is "the client's placement or the wire's, never both", and
        // a single client breakpoint is enough to switch. Merging instead
        // would charge a write premium at boundaries the customer did not
        // choose — and on a four-breakpoint client would 400 the request.
        let wire = AnthropicWire::new("anthropic", "k", None, 512, 900);
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("q").with_cache_breakpoint(),
        ];
        let spec = crate::provider::ToolSpec {
            cache_control: false,
            name: "a".into(),
            description: "d".into(),
            parameters: (json!({"type": "object"})),
        };
        let body = wire.request_body("claude-sonnet-5", &messages, Some(&[spec]), None, false);
        assert_eq!(
            body.to_string().matches("cache_control").count(),
            1,
            "one client breakpoint, and none of the wire's three"
        );
        assert!(
            body["system"][0].get("cache_control").is_none(),
            "the system default is dropped too — the client did not mark it"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn a_breakpoint_lands_on_the_block_its_own_message_produced() {
        // A message that contributes NO block — empty or whitespace-only
        // content, which this builder deliberately drops — must not push its
        // breakpoint onto the previous message's block. That would cache a
        // boundary the client never named, at a price the client did not
        // expect.
        let messages = vec![
            ChatMessage::user("real content"),
            ChatMessage::user("   ").with_cache_breakpoint(),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        assert_eq!(
            turns.len(),
            1,
            "the blank turn contributes nothing, as it always has"
        );
        assert!(
            turns[0]["content"][0].get("cache_control").is_none(),
            "a breakpoint on a message that produced no block must not migrate to another's"
        );
    }

    #[test]
    fn a_breakpoint_marks_the_last_block_of_a_multi_block_turn() {
        // The Messages API caches everything up to and including the marked
        // block, so a turn's breakpoint belongs on its LAST block — an
        // assistant turn that emits text and then two tool calls is one
        // boundary at the end, not three.
        let messages = vec![
            ChatMessage::user("go"),
            ChatMessage::assistant(
                r#"{"content":"running","tool_calls":[{"id":"t1","name":"shell","arguments":"{}"},{"id":"t2","name":"shell","arguments":"{}"}],"reasoning_content":null}"#,
            )
            .with_cache_breakpoint(),
        ];
        let (_, turns) = build_anthropic_messages(&messages);
        let blocks = turns[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3, "text plus two tool_use blocks");
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[1].get("cache_control").is_none());
        assert_eq!(blocks[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn anthropic_cache_creation_tokens_are_carried_out_rather_than_folded_away() {
        // The billing correction, at the wire. `input_tokens` stays the SUM of
        // all three buckets — it is what the customer sees and what selects a
        // conditional band — while the creation bucket now travels beside it
        // instead of disappearing into the total where it billed as a fresh
        // read.
        let usage = AnthropicUsage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cache_read_input_tokens: Some(600),
            cache_creation_input_tokens: Some(300),
        }
        .into_token_usage()
        .expect("a believable usage report");
        assert_eq!(
            usage.input_tokens,
            Some(1_000),
            "the OpenAI-convention total is fresh + read + written"
        );
        assert_eq!(usage.cached_input_tokens, Some(600));
        assert_eq!(usage.cache_write_input_tokens, Some(300));

        // Absent stays absent: a response with no cache activity must not
        // assert that zero tokens were written, which is a measurement.
        let plain = AnthropicUsage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }
        .into_token_usage()
        .expect("a believable usage report");
        assert_eq!(plain.input_tokens, Some(100));
        assert_eq!(plain.cache_write_input_tokens, None);
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
    fn a_content_part_breakpoint_lands_on_its_own_block_and_not_a_later_one() {
        // The heart of the content-part feature: a breakpoint on part 2 of a
        // text array caches parts 1 and 2 and leaves the rest fresh, so the
        // mark must sit on the SECOND block and no later one. Placing it at the
        // message end instead (the boundary the old comment warned about) would
        // cache the whole turn — a different, dearer boundary the client did
        // not ask for.
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::text("stable prefix"),
            ContentPart::Text {
                text: "cached up to here".to_owned(),
                cache_control: true,
            },
            ContentPart::text("fresh tail"),
        ])];
        let (_, turns) = build_anthropic_messages(&messages);
        let blocks = turns[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 3, "each content part is its own block");
        assert!(
            blocks[0].get("cache_control").is_none(),
            "the block before the marked one is not cached"
        );
        assert_eq!(
            blocks[1]["cache_control"]["type"], "ephemeral",
            "the marked part carries the boundary"
        );
        assert!(
            blocks[2].get("cache_control").is_none(),
            "a LATER block must stay fresh — the boundary is not hoisted to the turn's end"
        );
        // Exactly one breakpoint on the wire: the one the client placed, with
        // none of the wire's three defaults added alongside it.
        assert_eq!(
            body_of(&messages)
                .to_string()
                .matches("cache_control")
                .count(),
            1
        );
    }

    #[test]
    fn a_content_part_breakpoint_switches_off_the_wires_default_breakpoints() {
        // A content-part breakpoint makes the client the owner of this
        // request's cache boundaries, exactly as a message-level one does: the
        // wire sets NONE of its three defaults, because three defaults plus the
        // client's marks could exceed Anthropic's limit of four and would cache
        // boundaries the customer did not choose.
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user_parts(vec![ContentPart::Text {
                text: "cache me".to_owned(),
                cache_control: true,
            }]),
        ];
        let body = body_of(&messages);
        assert!(
            body["system"][0].get("cache_control").is_none(),
            "the wire's default system-prompt marker is dropped — the client did not mark it"
        );
        assert_eq!(
            body.to_string().matches("cache_control").count(),
            1,
            "one client breakpoint, and none of the wire's three"
        );
    }

    #[test]
    fn a_breakpoint_on_an_image_content_part_marks_its_image_block() {
        // The image half: a `cache_control` on an image part marks the native
        // image block the wire builds for it, so a client can cache a prefix
        // that ends on an image.
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::text("describe both"),
            ContentPart::Image {
                url: "data:image/png;base64,AAAA".to_owned(),
                cache_control: true,
            },
            ContentPart::text("and then continue"),
        ])];
        let (_, turns) = build_anthropic_messages(&messages);
        let blocks = turns[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(
            blocks[1]["cache_control"]["type"], "ephemeral",
            "the marked image block carries the boundary"
        );
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[2].get("cache_control").is_none());
    }

    /// Build a Messages body the way the wire does, for the cache-marker count
    /// assertions — a thin helper so the tests read the body a request path
    /// would send.
    fn body_of(messages: &[ChatMessage]) -> Value {
        AnthropicWire::new("anthropic", "k", None, 512, 900).request_body(
            "claude-sonnet-5",
            messages,
            None,
            None,
            false,
        )
    }

    #[test]
    fn image_parts_become_native_image_blocks() {
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::text("what is this? ".to_owned()),
            ContentPart::image("data:image/png;base64,AAAA".to_owned()),
            ContentPart::text(" and this? ".to_owned()),
            ContentPart::image("https://example.com/x.jpg".to_owned()),
        ])];
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
        // A plain string is one text block however it is spelled — including
        // the exact shape of the retired `[IMAGE:…]` marker, which this test
        // previously asserted became an image. It is prose; it stays prose.
        let messages = vec![ChatMessage::user(
            "docs write it [IMAGE:https://example.com/x.jpg] like so",
        )];
        let (_, turns) = build_anthropic_messages(&messages);
        let blocks = turns[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(
            blocks[0]["text"],
            "docs write it [IMAGE:https://example.com/x.jpg] like so"
        );
    }

    /// The byte pin for this dialect's image blocks, and the twin of the
    /// chat-completions and Responses pins. Anthropic nests the carrier under
    /// `source` with a discriminated `type` — `base64` (with `media_type` and
    /// `data`) or `url` — which is a third distinct shape from the two
    /// OpenAI dialects (platform.claude.com, "Vision", read 2026-08-21).
    ///
    /// Keys serialize alphabetically, so `source` precedes `type` and
    /// `media_type` precedes `data`'s sibling keys accordingly.
    #[test]
    fn image_blocks_are_byte_stable() {
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::text("look ".to_owned()),
            ContentPart::image("data:image/png;base64,AAAA".to_owned()),
            ContentPart::text(" and ".to_owned()),
            ContentPart::image("https://example.com/x.jpg".to_owned()),
        ])];
        let (_, turns) = build_anthropic_messages(&messages);
        assert_eq!(
            serde_json::to_string(&turns).unwrap(),
            concat!(
                r#"[{"content":["#,
                r#"{"text":"look ","type":"text"},"#,
                r#"{"source":{"data":"AAAA","media_type":"image/png","type":"base64"},"type":"image"},"#,
                r#"{"text":" and ","type":"text"},"#,
                r#"{"source":{"type":"url","url":"https://example.com/x.jpg"},"type":"image"}"#,
                r#"],"role":"user"}]"#,
            )
        );
    }
}

#[cfg(test)]
mod request_body_characterization {
    //! A byte-for-byte pin on the Messages request body, written BEFORE the
    //! body builder was factored out for the Bedrock classic-runtime wire to
    //! share (2026-08-20) and unchanged by that refactor.
    //!
    //! `request_body_sets_the_three_cache_breakpoints` above checks the
    //! breakpoints; this checks EVERYTHING ELSE at once — the full key set,
    //! the turn shape, the tool shape, and the `model`/`max_tokens`/`stream`
    //! triple — because a refactor that shares this builder with a second wire
    //! can plausibly drop or rename a field without touching a breakpoint.
    //!
    //! The comparison is against the serialized STRING rather than `Value`
    //! equality, which pins one more thing than it looks like: this crate
    //! builds `serde_json` without `preserve_order`, so a `Value::Object` is a
    //! `BTreeMap` and serializes its keys alphabetically. The expected strings
    //! below are therefore in alphabetical key order — that is the observed
    //! behavior, recorded rather than assumed (the first draft of this test
    //! asserted insertion order and failed, which is the point of writing it
    //! against the unchanged code first). The Bedrock classic-runtime wire
    //! builds the same object with `model` dropped and `anthropic_version`
    //! added, and that diff is asserted over there.
    use super::*;

    fn fixture_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("first question"),
            ChatMessage::assistant("first answer"),
            ChatMessage::user("second question"),
        ]
    }

    fn fixture_tools() -> Vec<crate::provider::ToolSpec> {
        vec![crate::provider::ToolSpec {
            cache_control: false,
            name: "shell".into(),
            description: "run a command".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }]
    }

    #[test]
    fn the_messages_request_body_is_byte_stable() {
        let wire = AnthropicWire::new("anthropic", "k", None, 512, 900);
        let body = wire.request_body(
            "claude-sonnet-5",
            &fixture_messages(),
            Some(&fixture_tools()),
            Some(0.25),
            true,
        );
        assert_eq!(
            body.to_string(),
            concat!(
                r#"{"max_tokens":512,"#,
                r#""messages":["#,
                r#"{"content":[{"text":"first question","type":"text"}],"role":"user"},"#,
                r#"{"content":[{"text":"first answer","type":"text"}],"role":"assistant"},"#,
                r#"{"content":[{"cache_control":{"type":"ephemeral"},"text":"second question","type":"text"}],"role":"user"}"#,
                r#"],"#,
                r#""model":"claude-sonnet-5","#,
                r#""stream":true,"#,
                r#""system":[{"cache_control":{"type":"ephemeral"},"text":"be terse","type":"text"}],"#,
                r#""temperature":0.25,"#,
                r#""tools":[{"cache_control":{"type":"ephemeral"},"description":"run a command","input_schema":{"properties":{"command":{"type":"string"}},"type":"object"},"name":"shell"}]}"#,
            )
        );
    }

    #[test]
    fn the_minimal_messages_request_body_is_byte_stable() {
        // No system, no tools, no temperature: the three optional keys must
        // stay ABSENT rather than becoming nulls or empty arrays.
        let wire = AnthropicWire::new("anthropic", "k", None, 64, 900);
        let body = wire.request_body("m", &[ChatMessage::user("hi")], None, None, false);
        assert_eq!(
            body.to_string(),
            concat!(
                r#"{"max_tokens":64,"messages":[{"content":"#,
                r#"[{"cache_control":{"type":"ephemeral"},"text":"hi","type":"text"}],"role":"user"}],"#,
                r#""model":"m","stream":false}"#,
            )
        );
    }
}

#[cfg(test)]
mod hostile_upstream_tests {
    //! The upstream is not trusted infrastructure — it is a network peer
    //! whose numbers become customer charges. These pin the bounds a
    //! deep-review pass identified as missing.

    use super::*;

    #[test]
    fn implausible_token_counts_are_refused_rather_than_billed() {
        // Above what the usage columns can store, every settlement would
        // fail forever — a denial of settlement, not a billing error. Treat
        // the usage as absent so the request takes the known missing-usage
        // path instead.
        let absurd = AnthropicUsage {
            input_tokens: Some(u64::from(u32::MAX)),
            output_tokens: Some(10),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert!(absurd.into_token_usage().is_none());

        let believable_usage = AnthropicUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(10),
            cache_read_input_tokens: Some(400),
            cache_creation_input_tokens: None,
        };
        let usage = believable_usage
            .into_token_usage()
            .expect("ordinary counts are believed");
        assert_eq!(usage.input_tokens, Some(1_400));
    }

    #[test]
    fn cumulative_counters_never_move_backwards() {
        // Anthropic's counters are cumulative. A stale or replayed frame
        // reporting less than an earlier one must not shrink the bill — or
        // the velocity window.
        let mut machine = AnthropicStreamMachine::new(false);
        machine
            .handle(
                "anthropic",
                &json!({"type": "message_delta", "usage": {"output_tokens": 10_000}}),
            )
            .expect("first delta");
        machine
            .handle(
                "anthropic",
                &json!({"type": "message_delta", "usage": {"output_tokens": 1}}),
            )
            .expect("stale delta");
        let events = machine
            .handle("anthropic", &json!({"type": "message_stop"}))
            .expect("terminal");
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .expect("the terminal carries usage");
        assert_eq!(
            usage.output_tokens,
            Some(10_000),
            "the high-water mark survives a stale frame"
        );
    }

    #[test]
    fn tool_assembly_is_bounded_across_events() {
        // The per-event cap bounds one frame; a stream that opens blocks or
        // appends arguments forever must still be cut off.
        let mut machine = AnthropicStreamMachine::new(false);
        let mut refused = false;
        for index in 0..(MAX_OPEN_TOOL_BLOCKS + 8) {
            let opened = machine.handle(
                "anthropic",
                &json!({"type": "content_block_start", "index": index,
                        "content_block": {"type": "tool_use", "id": "t", "name": "x"}}),
            );
            if opened.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "concurrent tool blocks are capped");

        let mut machine = AnthropicStreamMachine::new(false);
        machine
            .handle(
                "anthropic",
                &json!({"type": "content_block_start", "index": 0,
                        "content_block": {"type": "tool_use", "id": "t", "name": "x"}}),
            )
            .expect("one block opens");
        let chunk = "x".repeat(64 * 1024);
        let mut refused = false;
        for _ in 0..(MAX_TOOL_ARGUMENT_BYTES / chunk.len() + 4) {
            if machine
                .handle(
                    "anthropic",
                    &json!({"type": "content_block_delta", "index": 0,
                            "delta": {"type": "input_json_delta", "partial_json": chunk}}),
                )
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "cumulative tool arguments are capped");
    }
}

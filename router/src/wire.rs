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
//!
//! Increment 3: the OpenAI Chat Completions wire (`/v1/chat/completions`),
//! the first client here with no single upstream behind it. llama.cpp, vLLM,
//! Ollama, and LM Studio all speak this dialect, and so does a hosted
//! ZeroRouter `/v1` — one adapter serves both halves of edge mode's hybrid
//! (`docs/design/edge-mode-local-rung.md`, stage 1). Its usage block is
//! already the convention this router meters in, so there is nothing to
//! normalize; the billing work is in what the dialect leaves OPTIONAL.
//! Streaming usage arrives only if the upstream honors `stream_options:
//! {include_usage: true}`, and several local servers silently do not — those
//! requests take the missing-usage path rather than being metered at zero,
//! because a wire that invents numbers is worse than one that reports none.
//! The terminal grammar is likewise softer than its siblings': a real
//! `finish_reason` on a choice, then the usage-bearing chunk, then the
//! literal `data: [DONE]` sentinel. Taking the terminal at `finish_reason`
//! would drop the usage that follows it, so the machine flushes only at
//! `[DONE]` — while still accepting a socket that closes after a
//! `finish_reason` without one, since that is a completed generation missing
//! a framing marker rather than a broken stream.
//!
//! That soft close has one accepted cost, recorded here rather than left for
//! someone to rediscover from a revenue graph. On a stream whose only output
//! was TOOL CALLS, a soft close flushes the assembled calls, reports whatever
//! usage arrived (often none), and settles — where the strict rule its
//! siblings use would have failed the attempt and let the walk re-serve it on
//! the next candidate, which would then have billed. So the soft close can
//! convert a billable retry into a free delivery. It is still the right
//! trade: the strict rule pays for that revenue by discarding completed
//! answers on every server that ends its streams this way, and a duplicate
//! tool call re-served against a candidate that already ran the tool is worse
//! for the customer than an unbilled one. The case is made observable instead
//! — a soft close carrying no usage logs a distinct `done_missing` gap
//! (`ChatCompletionsStreamMachine::usage_gap`), so a truncating middlebox
//! cannot hide inside the ordinary "this server ignores `include_usage`"
//! shrug.
//!
//! One more divergence from the Anthropic machine, and it is deliberate:
//! usage MERGE policy. Anthropic streams cumulative counters, so a per-field
//! maximum is the honest reading. This dialect streams absolute snapshots,
//! where a per-field maximum can synthesize a pair no chunk ever sent. The
//! rule here is whole-report; see
//! `ChatCompletionsStreamMachine::absorb_usage`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::provider::ChatMessage;
use crate::provider::{
    ChatRequest, ChatResponse, ModelProvider, ProviderCapabilities, StreamChunk, StreamError,
    StreamEvent, StreamOptions, StreamResult, TokenUsage, ToolCall,
};
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

/// Idle ceiling for a STREAMING upstream: how long the wire waits between
/// bytes before declaring the stream dead. A live SSE stream is never
/// silent this long — both dialects emit events (or pings) while a model
/// thinks — but a half-open socket is silent forever, and without this the
/// customer's connection and its reservation are held for the router's
/// whole 15-minute request budget (found by live failure injection: an
/// upstream that stops mid-stream without closing). Deliberately NOT
/// applied to non-streaming calls, where a long completion legitimately
/// sends nothing until the model finishes.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Ceiling on a non-streaming upstream response body, and on the excerpt
/// kept from an error body. Nothing legitimate approaches it: a maximal
/// completion is a few megabytes of JSON. Without it a hostile upstream can
/// stream a multi-gigabyte body inside the request budget and exhaust the
/// process with a single request.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on tool-call assembly within one stream: how many tool blocks
/// may be open at once, and how many bytes of arguments may accumulate
/// across all of them. The per-event cap bounds a single SSE frame; these
/// bound the total a stream can accrete across legitimately terminated
/// frames.
const MAX_OPEN_TOOL_BLOCKS: usize = 64;
const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;

/// Largest token count either wire will believe. Postgres stores usage as
/// INTEGER, so anything at or above this both overcharges and — once it
/// exceeds the column — makes every settlement attempt fail permanently,
/// which is a denial of settlement rather than a billing error.
const MAX_BELIEVABLE_TOKENS: u64 = i32::MAX as u64;

/// Read an upstream body with a ceiling. Returns what arrived, and whether
/// it was truncated — the caller decides whether truncation is fatal (a
/// success body must parse; an error body only needs to be legible).
async fn bounded_body(response: reqwest::Response) -> (String, bool) {
    let mut stream = response.bytes_stream();
    let mut collected: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = MAX_RESPONSE_BYTES.saturating_sub(collected.len());
        if chunk.len() > remaining {
            collected.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    (String::from_utf8_lossy(&collected).into_owned(), truncated)
}

/// Refuse token counts an upstream could not honestly report. A count that
/// exceeds what the database can store would fail every settlement forever;
/// treating the usage as absent instead routes the request through the
/// missing-usage path, which is a known, handled state.
fn believable(usage: TokenUsage) -> Option<TokenUsage> {
    let over = |value: Option<u64>| value.is_some_and(|tokens| tokens > MAX_BELIEVABLE_TOKENS);
    if over(usage.input_tokens) || over(usage.output_tokens) || over(usage.cached_input_tokens) {
        tracing::warn!(
            input = ?usage.input_tokens,
            output = ?usage.output_tokens,
            "upstream reported implausible token counts; treating usage as absent"
        );
        return None;
    }
    Some(usage)
}

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

/// Shared upstream HTTP clients, keyed by request timeout budget (seconds).
///
/// `reqwest::Client` owns an `Arc`'d connection pool, so a fresh client keeps no
/// keep-alive connections. The per-request provider construction in
/// `providers::create_provider` builds a new wire — and thus new clients — on
/// every request, which discarded connection reuse and forced a fresh TCP+TLS
/// handshake to the upstream on each call (invisible against a localhost mock,
/// tens of milliseconds against a real TLS provider). Building the pair once and
/// cloning it into each wire preserves the pool across requests; cloning a
/// `Client` is a cheap `Arc` bump. Connections are pooled per host internally,
/// so one shared pair correctly serves every upstream (OpenAI, Anthropic, and
/// the test seams). Keyed by `timeout_secs` — the only knob the wires vary; the
/// streaming client additionally caps idle time at `STREAM_IDLE_TIMEOUT`, so the
/// pair stays distinct exactly as the per-wire construction made it.
fn shared_upstream_clients(timeout_secs: u64) -> (reqwest::Client, reqwest::Client) {
    static POOL: OnceLock<Mutex<HashMap<u64, (reqwest::Client, reqwest::Client)>>> =
        OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashMap::new()));
    let mut clients = pool.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    clients
        .entry(timeout_secs)
        .or_insert_with(|| {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_default();
            let stream_http = reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .read_timeout(STREAM_IDLE_TIMEOUT)
                .build()
                .unwrap_or_default();
            (http, stream_http)
        })
        .clone()
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

/// Ceiling on a single un-terminated SSE event. A stream that never emits a
/// blank line would otherwise grow the decode buffer without bound — an
/// upstream (or anything that can impersonate one) could exhaust the
/// router's memory with one request. Real events are kilobytes; this is
/// four megabytes of slack before the stream is declared malformed.
const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Feed one network chunk into the SSE decoder and return every complete
/// event's `data:` payload, in order.
///
/// Shared verbatim by both wires. Two properties it exists to hold: bytes
/// are only decoded at valid UTF-8 boundaries, so a multibyte character
/// split across chunks never becomes a replacement character in a
/// customer's stream; and the buffer cannot grow without bound on an
/// upstream that never terminates an event.
fn drain_sse_payloads(
    raw_buffer: &mut Vec<u8>,
    buffer: &mut String,
    chunk: &[u8],
) -> Result<Vec<String>, StreamError> {
    raw_buffer.extend_from_slice(chunk);
    let valid_up_to = match std::str::from_utf8(raw_buffer) {
        Ok(_) => raw_buffer.len(),
        Err(error) => error.valid_up_to(),
    };
    buffer.push_str(
        std::str::from_utf8(&raw_buffer[..valid_up_to]).expect("prefix was just validated"),
    );
    raw_buffer.drain(..valid_up_to);

    let mut payloads = Vec::new();
    // SSE events are separated by a blank line — LF or CRLF framing are
    // both legal; `data:` lines strip a trailing CR.
    while let Some((boundary, delimiter_len)) = find_event_boundary(buffer) {
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
        payloads.push(data);
    }
    // Checked AFTER draining: a chunk that completes several events and
    // leaves a large tail is fine; only an unterminated tail is fatal.
    if buffer.len() + raw_buffer.len() > MAX_SSE_EVENT_BYTES {
        return Err(StreamError::InvalidSse(format!(
            "upstream SSE event exceeded {MAX_SSE_EVENT_BYTES} bytes without terminating"
        )));
    }
    Ok(payloads)
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
    let _ = (&envelope.status, &envelope.incomplete_details);
    ChatResponse {
        text,
        tool_calls,
        usage: envelope.usage.and_then(ResponsesUsage::into_token_usage),
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

#[async_trait]
impl ModelProvider for OpenAiResponsesWire {
    fn alias(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.alias)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
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
        })
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
        usage: envelope.usage.and_then(AnthropicUsage::into_token_usage),
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
    /// Cumulative tool-argument bytes across the whole stream.
    tool_argument_bytes: usize,
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

// ---------------------------------------------------------------------------
// OpenAI Chat Completions wire
// ---------------------------------------------------------------------------

/// Default endpoint for the OpenAI Chat Completions API. Overridden per
/// provider entry for every other upstream that speaks this dialect — the
/// whole point of the wire is that one client serves llama.cpp, vLLM, Ollama,
/// LM Studio, and a hosted ZeroRouter `/v1` alike.
const CHAT_COMPLETIONS_URL: &str = "https://api.openai.com/v1/chat/completions";

/// Attach the bearer credential, unless this upstream is keyless.
///
/// Most local inference servers take no credential at all, and their inventory
/// entry says so (`"credential": "none"`, `crate::providers`), which reaches
/// this wire as an empty credential string. Sending `Authorization: Bearer `
/// with an empty value is noise on a server that ignores the header and a 401
/// on one that parses it strictly, so an empty credential omits the header
/// rather than sending an empty one.
///
/// Only this wire can be keyless — inventory validation refuses that
/// declaration on the two adapters that own cloud endpoints — so its siblings
/// keep sending the header unconditionally, and an empty credential there
/// stays the loud upstream 401 it should be.
fn authorized(request: reqwest::RequestBuilder, credential: &str) -> reqwest::RequestBuilder {
    if credential.is_empty() {
        request
    } else {
        request.bearer_auth(credential)
    }
}

/// ZeroRouter's own Chat-Completions client: chat and streaming both on
/// `/v1/chat/completions`, usage lifted from the wire on both paths.
///
/// Unlike its two siblings this wire has no single upstream. Its dialect is
/// the lingua franca every local inference server implements, so the
/// `api_url` override is load-bearing configuration here rather than a test
/// seam, and the client must tolerate servers that implement the dialect
/// partially — most visibly, servers that ignore `stream_options` and never
/// report streaming usage at all. The billing rule for those is the same rule
/// as everywhere else in this module: usage the upstream did not report stays
/// `None`, and the request takes the known missing-usage path rather than
/// being metered against an invented number.
pub struct ChatCompletionsWire {
    alias: String,
    api_url: String,
    credential: String,
    max_tokens: Option<u32>,
    http: reqwest::Client,
    /// Same budget plus an idle ceiling; used only by `stream_chat`.
    stream_http: reqwest::Client,
}

impl ChatCompletionsWire {
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
                .unwrap_or_else(|| CHAT_COMPLETIONS_URL.to_owned()),
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
        let mut body = json!({
            "model": model,
            "messages": build_chat_completions_messages(messages),
            "stream": stream,
        });
        if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
            body["tools"] = json!(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            },
                        })
                    })
                    .collect::<Vec<Value>>()
            );
            body["tool_choice"] = json!("auto");
        }
        if let Some(max_tokens) = self.max_tokens {
            // `max_tokens`, not `max_completion_tokens`. OpenAI deprecated the
            // former on its own hosted endpoint, but every local server this
            // wire exists for accepts it and several accept nothing else, and
            // ZeroRouter's own `/v1` request shape (`ChatCompletionRequest`)
            // names it `max_tokens` too — so the hosted-upstream case agrees.
            // OpenAI's reasoning families, which require the newer name, are
            // served by the Responses wire and are not this wire's traffic.
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(temperature) = temperature {
            body["temperature"] = json!(temperature);
        }
        if stream {
            // Ask for the usage-bearing final chunk. Optional in the dialect
            // and widely unimplemented: llama.cpp, Ollama, and LM Studio may
            // stream to completion without ever sending one. Requesting it is
            // free, and its absence is handled as missing usage, never as an
            // error and never as zero.
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }
}

/// Build the `messages` array from ZeroRouter's packed provider messages —
/// exactly the five shapes `openai::to_provider_message` emits.
///
/// This wire is the one case where the packing is nearly its own inverse: ZR
/// speaks OpenAI chat completions on its customer-facing side, so a `system`
/// message goes out as a `system` message and a tool result goes back to the
/// `{role: "tool", tool_call_id, content}` shape it was packed from. The one
/// asymmetry is the assistant envelope, which ZR flattens into a JSON string
/// and this unpacks back into `content` plus a `tool_calls` array.
///
/// System messages are emitted IN PLACE rather than hoisted and joined the way
/// the Responses and Anthropic wires must (their APIs carry a single system
/// field). Preserving position is the faithful transform for a dialect that
/// has no such constraint, and it is what the customer's own request said.
fn build_chat_completions_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role.as_str() {
            "system" => out.push(json!({ "role": "system", "content": message.content })),
            "user" => out.push(json!({ "role": "user", "content": message.content })),
            "assistant" => {
                // Same envelope rule as both sibling wires: only ZR's own
                // packing markers make an envelope, so a model that merely
                // answered IN JSON round-trips as plain assistant text
                // instead of being reinterpreted as a tool-call pack.
                if let Ok(envelope) = serde_json::from_str::<Value>(&message.content)
                    && envelope.is_object()
                    && (envelope.get("tool_calls").is_some()
                        || envelope.get("reasoning_content").is_some())
                {
                    let text = envelope
                        .get("content")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty());
                    let calls: Vec<Value> = envelope
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .map(|calls| {
                            calls
                                .iter()
                                .map(|call| {
                                    json!({
                                        "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                                        "type": "function",
                                        "function": {
                                            "name": call
                                                .get("name")
                                                .and_then(Value::as_str)
                                                .unwrap_or_default(),
                                            // ZR packs arguments as a JSON
                                            // STRING and this dialect wants a
                                            // JSON string — no conversion, and
                                            // deliberately no re-parse: a model
                                            // that emitted malformed arguments
                                            // must replay them verbatim rather
                                            // than have the wire tidy them.
                                            "arguments": call
                                                .get("arguments")
                                                .and_then(Value::as_str)
                                                .unwrap_or("{}"),
                                        },
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // A reasoning-only envelope whose content was empty
                    // carries nothing this dialect can express: `content:
                    // null` with no `tool_calls` is a message the API rejects
                    // outright, so the turn is dropped rather than sent as an
                    // invalid one. (Reasoning itself is not round-tripped,
                    // matching both sibling wires.)
                    if text.is_none() && calls.is_empty() {
                        continue;
                    }
                    let mut turn = json!({ "role": "assistant" });
                    // `content: null` is this dialect's shape for a turn that
                    // was nothing but tool calls.
                    turn["content"] = text.map_or(Value::Null, |text| json!(text));
                    if !calls.is_empty() {
                        turn["tool_calls"] = json!(calls);
                    }
                    out.push(turn);
                } else {
                    out.push(json!({ "role": "assistant", "content": message.content }));
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
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output,
                }));
            }
            _ => {}
        }
    }
    out
}

/// Chat-completions usage, which is already the convention ZeroRouter meters
/// in: `prompt_tokens` is the TOTAL prompt and `prompt_tokens_details.
/// cached_tokens` is the subset of it served from cache. So unlike the
/// Anthropic wire there is nothing to normalize — only to refuse when it is
/// not believable, and to leave absent when it is absent.
#[derive(Default, Deserialize)]
struct ChatCompletionsUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<ChatCompletionsPromptDetails>,
}

#[derive(Deserialize)]
struct ChatCompletionsPromptDetails {
    cached_tokens: Option<u64>,
}

impl ChatCompletionsUsage {
    fn into_token_usage(self) -> Option<TokenUsage> {
        believable(TokenUsage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            cached_input_tokens: self
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
        })
    }
}

/// The chat-completions envelope fields billing needs. Everything else on the
/// response is deliberately ignored.
#[derive(Deserialize)]
struct ChatCompletionsEnvelope {
    #[serde(default)]
    choices: Vec<Value>,
    #[serde(default)]
    usage: Option<ChatCompletionsUsage>,
}

/// `function.arguments` in ZeroRouter's JSON-STRING convention.
///
/// The dialect specifies a string and the hosted providers send one, but
/// heterogeneous local servers sometimes send the parsed OBJECT instead.
/// Reading only `as_str` turned those into the empty string, which downstream
/// reads as "the model called this tool with no arguments" — a silently wrong
/// tool call rather than a visible failure. Serializing the object back is
/// exact, since it is the same JSON the string form would have carried.
/// `null` stays absent so the caller can apply its own default.
fn tool_arguments_text(function: Option<&Value>) -> Option<String> {
    match function.and_then(|function| function.get("arguments"))? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        structured => Some(structured.to_string()),
    }
}

/// Lift a `tool_calls[]` entry in this dialect's shape into ZR's `ToolCall`.
fn parse_chat_tool_call(call: &Value) -> ToolCall {
    ToolCall {
        id: call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        name: call
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        arguments: tool_arguments_text(call.get("function")).unwrap_or_else(|| "{}".to_owned()),
        extra_content: None,
    }
}

/// Note a finish reason the router cannot otherwise observe.
///
/// `content_filter` is named in this module's header as a state the pinned
/// adapters made structurally unobservable, and `length` is a clipped answer
/// the router currently INFERS from token arithmetic (`openai::finish_reason`)
/// rather than reads from what the upstream said. This wire reads both from
/// the real field. Carrying them further — into `ChatResponse`, and so into
/// `openai::finish_reason` and `request_attempts.finish_reason_source`, which
/// is hardcoded `"synthetic"` today — means changing the shared provider
/// contract and `openai::shape_ok`, whose verdict decides whether an attempt
/// is served or retried. That is a money-path decision rather than part of
/// adding a wire, so the value is logged here and left for its own change.
fn note_finish_reason(alias: &str, finish_reason: Option<&str>) {
    if let Some(reason @ ("content_filter" | "length")) = finish_reason {
        tracing::debug!(
            provider = alias,
            finish_reason = reason,
            "upstream reported a finish_reason the router does not yet carry"
        );
    }
}

fn parse_chat_completions_envelope(alias: &str, envelope: ChatCompletionsEnvelope) -> ChatResponse {
    // `n` is not a field ZeroRouter's compat surface accepts (it lands in
    // `extra` and is 400-rejected as an unsupported extension), so a
    // conforming response has exactly one choice. Reading the first rather
    // than concatenating all of them keeps a nonconforming upstream from
    // interleaving two answers into one.
    let choice = envelope.choices.first();
    let message = choice.and_then(|choice| choice.get("message"));
    note_finish_reason(
        alias,
        choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str),
    );
    let text = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    // Thinking models on this dialect (DeepSeek-R1 and the reasoning parsers
    // in vLLM and llama.cpp) put chain-of-thought here, and ZeroRouter's own
    // response shape has a field for it — so unlike the sibling wires, whose
    // APIs give reasoning nowhere to land, this one passes it through.
    let reasoning_content = message
        .and_then(|message| message.get("reasoning_content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let tool_calls = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .map(|calls| calls.iter().map(parse_chat_tool_call).collect())
        .unwrap_or_default();
    ChatResponse {
        text,
        tool_calls,
        usage: envelope
            .usage
            .and_then(ChatCompletionsUsage::into_token_usage),
        reasoning_content,
    }
}

fn chat_completions_upstream_error(
    alias: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> anyhow::Error {
    anyhow!(
        "{alias} chat completions API error: HTTP {} {}: {body}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error"),
    )
}

/// One usage report exactly as a single upstream chunk stated it — the unit
/// this dialect's merge policy operates on.
///
/// Flat, unlike the `ChatCompletionsUsage` the buffered path deserializes,
/// because the merge rules compare the cached dimension field-to-field
/// alongside the other two rather than through a nested option.
#[derive(Clone, Copy, Default)]
struct ChatUsageReport {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cached_tokens: Option<u64>,
}

impl ChatUsageReport {
    fn from_value(usage: &Value) -> Self {
        Self {
            prompt_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
            completion_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
            cached_tokens: usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64),
        }
    }

    fn is_empty(self) -> bool {
        self.prompt_tokens.is_none()
            && self.completion_tokens.is_none()
            && self.cached_tokens.is_none()
    }

    fn fields(self) -> [Option<u64>; 3] {
        [
            self.prompt_tokens,
            self.completion_tokens,
            self.cached_tokens,
        ]
    }

    /// Whether `self` contradicts `accepted` by stating LESS on any dimension
    /// both of them set. Dimensions only one of them sets cannot contradict.
    fn shrinks_against(self, accepted: Self) -> bool {
        self.fields()
            .into_iter()
            .zip(accepted.fields())
            .any(|(incoming, accepted)| match (incoming, accepted) {
                (Some(incoming), Some(accepted)) => incoming < accepted,
                _ => false,
            })
    }

    /// Take `incoming` wholesale, keeping only the dimensions it declines to
    /// state. Applied ONLY to a report that does not shrink, so every value
    /// this returns either came from `incoming` or was never contested.
    fn replaced_by(self, incoming: Self) -> Self {
        Self {
            prompt_tokens: incoming.prompt_tokens.or(self.prompt_tokens),
            completion_tokens: incoming.completion_tokens.or(self.completion_tokens),
            cached_tokens: incoming.cached_tokens.or(self.cached_tokens),
        }
    }

    fn into_token_usage(self) -> Option<TokenUsage> {
        believable(TokenUsage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            cached_input_tokens: self.cached_tokens,
        })
    }
}

/// A tool call under assembly across `delta.tool_calls[].function.arguments`
/// fragments, keyed by the fragment's `index`.
struct PendingChatTool {
    id: String,
    name: String,
    arguments: String,
}

/// The per-payload state machine behind `ChatCompletionsWire::stream_chat`,
/// factored out of the socket loop so the event grammar is unit-testable
/// without a server.
///
/// The dialect's terminal structure is the one place it differs materially
/// from its siblings, and it is where the billing risk sits. There is no
/// per-block stop event and no `response.completed`: a choice reports a real
/// `finish_reason`, then — only if the upstream honored `include_usage` — one
/// more chunk arrives carrying `usage` and NO choices, and only then the
/// literal sentinel `data: [DONE]`. So tool calls are flushed at the terminal,
/// not mid-stream, and the terminal must not be taken at the `finish_reason`
/// chunk or the usage that follows it would be dropped and the request
/// settled unbilled.
#[derive(Default)]
struct ChatCompletionsStreamMachine {
    /// The one usage report currently accepted — always a report some chunk
    /// actually sent, never a blend of two. See [`Self::absorb_usage`].
    usage: ChatUsageReport,
    /// Whether any usage was ever accepted. Separate from `usage` because
    /// [`Self::partial_usage`] takes that field, and the metering-gap label
    /// still has to be answerable afterwards.
    usage_reported: bool,
    open_tools: std::collections::BTreeMap<u64, PendingChatTool>,
    /// Cumulative tool-argument bytes across the whole stream.
    tool_argument_bytes: usize,
    count_tokens: bool,
    /// The last real `finish_reason` seen. Present means the upstream said the
    /// generation itself completed, which is what licenses treating a socket
    /// that closes without `[DONE]` as a finished stream rather than a broken
    /// one.
    finish_reason: Option<String>,
    /// Whether the `[DONE]` sentinel actually arrived, as opposed to the
    /// socket closing after a `finish_reason`. Same billing outcome, different
    /// diagnosis — see [`Self::usage_gap`].
    saw_done: bool,
    finished: bool,
}

impl ChatCompletionsStreamMachine {
    fn new(count_tokens: bool) -> Self {
        Self {
            count_tokens,
            ..Self::default()
        }
    }

    /// Whatever usage the upstream has genuinely reported so far, same
    /// contract as the Anthropic machine's: an in-band error or a dropped
    /// connection after a usage-bearing chunk still has billable numbers, and
    /// settling such a stream at zero would give away delivered output.
    fn partial_usage(&mut self) -> Option<TokenUsage> {
        let usage = std::mem::take(&mut self.usage).into_token_usage()?;
        (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
    }

    /// Whether the upstream told us the generation finished, whether or not it
    /// went on to send `[DONE]`.
    fn saw_finish_reason(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// Why this stream has no usage to settle, or `None` if it has some.
    ///
    /// Both answers bill nothing, and that is exactly why they must be
    /// distinguishable in the log. `include_usage_ignored` is ordinary: the
    /// stream framed itself correctly and simply never sent the optional
    /// usage chunk, which several local servers do on every request.
    /// `done_missing` is not ordinary: the socket closed after a
    /// `finish_reason` without the sentinel AND without usage, which is what
    /// a truncating proxy in front of an upstream that DOES report usage
    /// looks like. Folding the second into the first would let a fleet-wide
    /// middlebox quietly erase revenue while looking like a known,
    /// tolerated limitation.
    fn usage_gap(&self) -> Option<&'static str> {
        if self.usage_reported {
            return None;
        }
        Some(if self.saw_done {
            "include_usage_ignored"
        } else {
            "done_missing"
        })
    }

    /// Merge policy for a usage report — deliberately NOT the per-field
    /// maximum its Anthropic sibling uses.
    ///
    /// The dialects differ in kind. Anthropic streams CUMULATIVE counters, so
    /// a per-field high-water mark is the honest reading of them. Chat
    /// completions streams absolute SNAPSHOTS, and taking the maximum of two
    /// snapshots field by field can synthesize a pair no chunk ever sent: a
    /// chunk stating `{prompt: 2_000_000_000, completion: 1}` followed by a
    /// corrected `{prompt: 500, completion: 400}` would bill
    /// `{2_000_000_000, 400}`, overcharging the prompt side four-million-fold
    /// against a figure the upstream had already retracted.
    ///
    /// So the unit here is the whole report:
    ///
    /// * A report that states LESS than the accepted one on any dimension
    ///   both set is a contradiction. The accepted report is kept entire —
    ///   fills it carried included, or the mixing would come back through the
    ///   side door — and the disagreement is logged.
    /// * Any other report replaces the accepted one wholesale, keeping only
    ///   the dimensions the newcomer declines to state.
    ///
    /// The result is always a report the upstream actually sent, extended at
    /// most by dimensions NO report has ever contested.
    fn absorb_usage(&mut self, alias: &str, value: Option<&Value>) {
        let Some(incoming) = value.map(ChatUsageReport::from_value) else {
            return;
        };
        if incoming.is_empty() {
            return;
        }
        if incoming.shrinks_against(self.usage) {
            tracing::warn!(
                provider = alias,
                "upstream contradicted an earlier usage report with a smaller one; \
                 keeping the earlier report"
            );
            return;
        }
        self.usage = self.usage.replaced_by(incoming);
        self.usage_reported = true;
    }

    /// One `data:` payload, including the non-JSON `[DONE]` sentinel this
    /// dialect ends with — which is why the machine, not the socket loop,
    /// owns the parse.
    fn handle_payload(&mut self, alias: &str, data: &str) -> Result<Vec<StreamEvent>, StreamError> {
        if data.trim() == "[DONE]" {
            self.saw_done = true;
            return Ok(self.terminate());
        }
        let value: Value = serde_json::from_str(data).map_err(StreamError::Json)?;
        self.handle(alias, &value)
    }

    fn handle(&mut self, alias: &str, value: &Value) -> Result<Vec<StreamEvent>, StreamError> {
        // Anything after the terminal is ignored outright: a doubled `[DONE]`,
        // or a chunk trailing it, must not emit a second Final — nor a second
        // Usage that metering would read as authoritative.
        if self.finished {
            return Ok(Vec::new());
        }

        // In-band errors carry no HTTP status. Servers in this dialect send
        // either `{"error": {...}}` or, in vLLM's older shape,
        // `{"object": "error", "message": ...}`. An explicit `"error": null`
        // — which conforming chunks from some servers carry on every frame —
        // is NOT an error, and reading it as one would fail every stream they
        // serve.
        let in_band_error = value
            .get("error")
            .filter(|error| !error.is_null())
            .or_else(|| {
                (value.get("object").and_then(Value::as_str) == Some("error")).then_some(value)
            });
        if let Some(error) = in_band_error {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| error.get("type").and_then(Value::as_str))
                .unwrap_or_default();
            let detail = error.to_string();
            // Same restoration the Responses wire does: the walk's classifier
            // reads status digits and rate hints out of the TEXT, so a rate
            // limit has to look like one or it feeds the health cooldown as a
            // generic broken stream instead.
            let prefix = if code.contains("rate_limit") || detail.contains("rate limit") {
                "429 Too Many Requests: "
            } else if code.contains("insufficient_quota") {
                "429 Too Many Requests: insufficient_quota — "
            } else {
                ""
            };
            return Err(StreamError::ModelProvider(format!(
                "{alias} chat completions stream failed: {prefix}{detail}"
            )));
        }

        // Usage rides on the chunk itself, not on a choice. With
        // `include_usage` honored it arrives on a final choice-less chunk;
        // some servers attach it to every chunk instead, which the merge
        // policy handles.
        self.absorb_usage(alias, value.get("usage"));

        let mut events = Vec::new();
        // The FIRST choice only, matching what the buffered path reads. `n` is
        // not a field ZeroRouter's compat surface accepts — it lands in
        // `extra` and is 400-rejected as an unsupported extension — so a
        // conforming response has exactly one choice, and streaming every
        // choice would let a nonconforming upstream interleave two answers
        // into one customer stream, character by character, with no way to
        // separate them afterwards. Reading position 0 rather than filtering
        // on `index == 0` also keeps an upstream that numbers its single
        // choice oddly from silently returning nothing.
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(events);
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                let mut chunk = StreamChunk::delta(content);
                if self.count_tokens {
                    // The ZR-documented per-chunk floor convention: len()/4, a
                    // labeled lower bound, never billing.
                    chunk.token_count = content.len() / 4;
                }
                events.push(StreamEvent::TextDelta(chunk));
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                // No token_count: the estimate is defined over content, so a
                // reasoning-only chunk contributes nothing to it.
                events.push(StreamEvent::TextDelta(StreamChunk::reasoning(reasoning)));
            }
            if let Some(fragments) = delta.get("tool_calls").and_then(Value::as_array) {
                self.absorb_tool_fragments(alias, fragments)?;
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            note_finish_reason(alias, Some(reason));
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(events)
    }

    /// Index-based tool assembly. `id` and `function.name` usually arrive on
    /// the first fragment for an index and `function.arguments` accumulates
    /// across the rest, but servers vary: some repeat the id on every
    /// fragment, some send the whole call in one. Non-empty values are taken
    /// whenever they appear and never overwritten with empty ones.
    fn absorb_tool_fragments(
        &mut self,
        alias: &str,
        fragments: &[Value],
    ) -> Result<(), StreamError> {
        for (position, fragment) in fragments.iter().enumerate() {
            let id = fragment.get("id").and_then(Value::as_str).unwrap_or("");
            let index = match fragment.get("index").and_then(Value::as_u64) {
                Some(index) => index,
                None => {
                    // A missing `index` is nonconforming but common on small
                    // servers. Position disambiguates the fragments WITHIN one
                    // delta, which is why it is the fallback rather than a
                    // constant — but position does not disambiguate ACROSS
                    // frames, and two complete indexless calls arriving in two
                    // separate chunks would both select slot 0: the second
                    // overwrites the first's id and name and appends its
                    // arguments onto the first's, yielding one call whose body
                    // is two concatenated JSON objects. Early Ollama and
                    // Mistral-compat builds emit exactly that shape. A
                    // non-empty id that disagrees with the one already in the
                    // selected slot is proof the fragment belongs to a
                    // DIFFERENT call, so it opens a fresh slot above every
                    // slot in use instead of corrupting that one.
                    let fallback = u64::try_from(position).unwrap_or_default();
                    let collides = !id.is_empty()
                        && self
                            .open_tools
                            .get(&fallback)
                            .is_some_and(|pending| !pending.id.is_empty() && pending.id != id);
                    if collides {
                        self.open_tools
                            .keys()
                            .next_back()
                            .map_or(0, |highest| highest.saturating_add(1))
                    } else {
                        fallback
                    }
                }
            };
            if !self.open_tools.contains_key(&index) {
                // The per-event cap bounds one frame; this bounds what a
                // stream can accrete across legitimately terminated frames.
                if self.open_tools.len() >= MAX_OPEN_TOOL_BLOCKS {
                    return Err(StreamError::InvalidSse(format!(
                        "{alias} opened more than {MAX_OPEN_TOOL_BLOCKS} concurrent tool blocks"
                    )));
                }
                self.open_tools.insert(
                    index,
                    PendingChatTool {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    },
                );
            }
            let function = fragment.get("function");
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = tool_arguments_text(function).unwrap_or_default();
            let arguments = arguments.as_str();
            if !arguments.is_empty() {
                // Cumulative across every open block: a stream that appends
                // forever must not grow the process.
                self.tool_argument_bytes = self.tool_argument_bytes.saturating_add(arguments.len());
                if self.tool_argument_bytes > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(StreamError::InvalidSse(format!(
                        "{alias} tool arguments exceeded {MAX_TOOL_ARGUMENT_BYTES} bytes"
                    )));
                }
            }
            let Some(pending) = self.open_tools.get_mut(&index) else {
                continue;
            };
            if !id.is_empty() {
                pending.id = id.to_owned();
            }
            if !name.is_empty() {
                pending.name = name.to_owned();
            }
            pending.arguments.push_str(arguments);
        }
        Ok(())
    }

    /// The terminal: flush assembled tool calls, then usage, then Final —
    /// the same relative order the Anthropic machine emits. Idempotent, and
    /// draining, so nothing can be emitted twice.
    fn terminate(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        let mut events: Vec<StreamEvent> = std::mem::take(&mut self.open_tools)
            .into_values()
            .map(|pending| {
                StreamEvent::ToolCall(ToolCall {
                    id: pending.id,
                    name: pending.name,
                    arguments: if pending.arguments.is_empty() {
                        "{}".to_owned()
                    } else {
                        pending.arguments
                    },
                    extra_content: None,
                })
            })
            .collect();
        if let Some(usage) = self.partial_usage() {
            events.push(StreamEvent::Usage(usage));
        }
        self.finished = true;
        events.push(StreamEvent::Final);
        events
    }
}

#[async_trait]
impl ModelProvider for ChatCompletionsWire {
    fn alias(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.alias)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            // ZR packs OpenAI image parts as `[IMAGE:<url>]` markers. This
            // wire does not decode them back into `image_url` content parts,
            // so it reports no vision — the same position the Responses wire
            // takes, and honest about what it actually sends. Nothing in the
            // router reads this field today; stage 2's per-candidate
            // capability config is where a local vision model gets declared.
            vision: false,
            // The dialect carries `prompt_tokens_details.cached_tokens` and
            // this wire lifts it, but no upstream on it takes cache-control
            // directives from the request.
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
        let response = authorized(self.http.post(&self.api_url), &self.credential)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        // Same rule as both sibling wires: the status is already known, so a
        // failed body read must not erase it into a retryable-looking
        // transport error. Bounded, because a hostile upstream can otherwise
        // stream a body until the process dies.
        let (text, truncated) = bounded_body(response).await;
        if !status.is_success() {
            return Err(chat_completions_upstream_error(&self.alias, status, &text));
        }
        if truncated {
            return Err(anyhow!(
                "{} chat completions API body exceeded {MAX_RESPONSE_BYTES} bytes",
                self.alias
            ));
        }
        let envelope: ChatCompletionsEnvelope = serde_json::from_str(&text).map_err(|error| {
            anyhow!(
                "{} chat completions API returned unparseable JSON: {error}",
                self.alias
            )
        })?;
        Ok(parse_chat_completions_envelope(&self.alias, envelope))
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
            let response = authorized(http.post(&api_url), &credential)
                .json(&body)
                .send()
                .await
                .map_err(|error| StreamError::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let (text, _) = bounded_body(response).await;
                Err(StreamError::Http(
                    chat_completions_upstream_error(&alias, status, &text).to_string(),
                ))?;
                return;
            }

            let mut bytes = response.bytes_stream();
            // Same byte-accurate UTF-8 buffering as both sibling wires: a
            // multibyte character split across network chunks must never
            // become a replacement character in a customer's stream.
            let mut raw_buffer: Vec<u8> = Vec::new();
            let mut buffer = String::new();
            let mut machine = ChatCompletionsStreamMachine::new(count_tokens);
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
                    // Every abnormal exit emits reported-so-far usage first:
                    // usage the upstream already gave us is billable, and
                    // leaving through `?` would settle delivered output at
                    // zero.
                    match machine.handle_payload(&alias, &data) {
                        Ok(events) => {
                            for event in events {
                                yield event;
                            }
                        }
                        Err(error) => {
                            if let Some(usage) = machine.partial_usage() {
                                yield StreamEvent::Usage(usage);
                            }
                            Err(error)?;
                            return;
                        }
                    }
                }
                if machine.finished {
                    break;
                }
            }
            if !machine.finished {
                if machine.saw_finish_reason() {
                    // The upstream reported a real finish_reason and then
                    // closed without the `[DONE]` sentinel. That is a
                    // completed generation missing a framing marker, and
                    // several servers in this dialect end exactly this way;
                    // failing it would discard an answer the customer was
                    // already streamed. The sentinel is framing, the
                    // finish_reason is the protocol's completion signal.
                    //
                    // A soft close that ALSO carries no usage is louder than
                    // that, though: it is indistinguishable at this layer from
                    // a truncating middlebox eating the tail of a stream whose
                    // upstream does report usage, and it bills nothing either
                    // way. It must not settle behind the same label as a
                    // server that merely ignores `include_usage`.
                    match machine.usage_gap() {
                        Some(gap @ "done_missing") => tracing::warn!(
                            provider = %alias,
                            usage_gap = gap,
                            "chat completions stream closed without [DONE] and without usage; \
                             settling unbilled"
                        ),
                        gap => tracing::debug!(
                            provider = %alias,
                            usage_gap = gap.unwrap_or("none"),
                            "chat completions stream closed after finish_reason without [DONE]"
                        ),
                    }
                    for event in machine.terminate() {
                        yield event;
                    }
                } else {
                    // No completion signal of any kind: surface it rather
                    // than synthesizing a Final the wire never sent — after
                    // the reported-so-far usage, same rule as the error arm.
                    if let Some(usage) = machine.partial_usage() {
                        yield StreamEvent::Usage(usage);
                    }
                    Err(StreamError::InvalidSse(format!(
                        "{alias} chat completions stream ended without completion"
                    )))?;
                }
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

/// Edge mode, stage 2: a keyless local upstream must be dialled without an
/// `Authorization` header at all.
#[cfg(test)]
mod keyless_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Stand up a chat-completions upstream that records the `Authorization`
    /// header of each request it serves, and answer one minimal completion.
    async fn recording_upstream() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder.lock().expect("recorder lock").push(
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .map(|value| value.to_str().unwrap_or("<binary>").to_owned()),
                    );
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 1}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("recording upstream should bind");
        let address = listener
            .local_addr()
            .expect("recording upstream should report its address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}/v1/chat/completions"), seen)
    }

    #[tokio::test]
    async fn a_keyless_upstream_is_dialled_with_no_authorization_header() {
        // llama.cpp, Ollama, and LM Studio ignore the header; a strict server
        // (and vLLM behind a proxy) can 401 on an empty bearer. An upstream
        // that takes no credential should be sent no credential — not an empty
        // one — and the same wire must keep sending a real one when it has it.
        let (url, seen) = recording_upstream().await;
        let messages = vec![ChatMessage::user("hello")];
        let request = || ChatRequest {
            messages: &messages,
            tools: None,
        };

        let keyless = ChatCompletionsWire::new("local-llama", "", Some(&url), Some(64), 30);
        keyless
            .chat(request(), "qwen3-8b", None)
            .await
            .expect("the keyless call should complete");

        let credentialed =
            ChatCompletionsWire::new("hosted", "zr-secret", Some(&url), Some(64), 30);
        credentialed
            .chat(request(), "qwen3-8b", None)
            .await
            .expect("the credentialed call should complete");

        let seen = seen.lock().expect("recorder lock").clone();
        assert_eq!(
            seen,
            vec![None, Some("Bearer zr-secret".to_owned())],
            "keyless must send no Authorization header, and a credential must still be sent"
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
    fn the_stream_idle_ceiling_is_pinned_and_both_wires_build() {
        // Live failure injection found the gap: an upstream that stops
        // mid-stream WITHOUT closing held the customer's connection and its
        // reservation for the router's whole 15-minute budget. The idle
        // ceiling belongs only to the streaming client — a long
        // non-streaming completion legitimately sends nothing until it is
        // done, so the same ceiling there would kill honest requests. Only the
        // `stream_http` arm sets `.read_timeout(STREAM_IDLE_TIMEOUT)`; `http`
        // does not (see `shared_upstream_clients`).
        assert_eq!(STREAM_IDLE_TIMEOUT, Duration::from_secs(120));
        // Smoke-test that both wire kinds build through the shared pool without
        // panicking. We deliberately do NOT assert here that `stream_http`
        // carries the ceiling and `http` does not, nor that two wires share a
        // pool: reqwest 0.12 exposes no getter for `read_timeout` and no client
        // identity, so neither is expressible against its public API. A prior
        // `std::ptr::eq(&self.http, &self.stream_http)` check looked like a pin
        // but compared two distinct struct-field addresses — always unequal —
        // so it guarded nothing; removed rather than kept as false confidence.
        // The distinction is enforced structurally in `shared_upstream_clients`
        // and end-to-end by the out-of-process fault-injection harness (the
        // `ZEROROUTER_PROVIDER_BASE_URL_*` seam).
        let _responses = OpenAiResponsesWire::new("openai", "k", None, Some(64), 900);
        let _anthropic = AnthropicWire::new("anthropic", "k", None, 64, 900);
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
        let spec = |name: &str, description: &str| crate::provider::ToolSpec {
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

#[cfg(test)]
mod chat_completions_tests {
    use super::*;

    /// Drive the machine over a whole scripted stream and collect what a
    /// customer would have received, in order.
    #[derive(Default)]
    struct Delivered {
        text: String,
        reasoning: String,
        token_counts: Vec<usize>,
        tool_calls: Vec<ToolCall>,
        usage: Vec<TokenUsage>,
        finals: usize,
    }

    fn run(machine: &mut ChatCompletionsStreamMachine, payloads: &[&str]) -> Delivered {
        let mut delivered = Delivered::default();
        for payload in payloads {
            for event in machine
                .handle_payload("local", payload)
                .expect("no in-band error in this script")
            {
                match event {
                    StreamEvent::TextDelta(chunk) => {
                        delivered.text.push_str(&chunk.delta);
                        if let Some(reasoning) = &chunk.reasoning {
                            delivered.reasoning.push_str(reasoning);
                        }
                        delivered.token_counts.push(chunk.token_count);
                    }
                    StreamEvent::ToolCall(call) => delivered.tool_calls.push(call),
                    StreamEvent::Usage(usage) => delivered.usage.push(usage),
                    StreamEvent::Final => delivered.finals += 1,
                }
            }
        }
        delivered
    }

    #[test]
    fn input_builder_handles_every_zr_packing_shape() {
        // The same five shapes both sibling wires are tested against, exactly
        // as `openai::to_provider_message` emits them.
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("run pwd"),
            ChatMessage::assistant(
                r#"{"content":"running it","tool_calls":[{"id":"call_1","name":"shell","arguments":"{\"command\":\"pwd\"}"}],"reasoning_content":null}"#,
            ),
            ChatMessage::tool(r#"{"tool_call_id":"call_1","name":"shell","content":"/home"}"#),
            ChatMessage::assistant("done: /home"),
        ];
        let turns = build_chat_completions_messages(&messages);
        let roles: Vec<&str> = turns
            .iter()
            .map(|turn| turn["role"].as_str().unwrap())
            .collect();
        // System stays IN PLACE rather than being hoisted: this dialect has no
        // single system field to hoist it into.
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant"]);
        assert_eq!(turns[0]["content"], "be terse");
        assert_eq!(turns[2]["content"], "running it");
        assert_eq!(turns[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(turns[2]["tool_calls"][0]["type"], "function");
        assert_eq!(turns[2]["tool_calls"][0]["function"]["name"], "shell");
        // Arguments stay a JSON STRING — this dialect's own convention, so no
        // conversion happens in either direction.
        assert_eq!(
            turns[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"pwd"}"#
        );
        assert_eq!(turns[3]["tool_call_id"], "call_1");
        assert_eq!(turns[3]["content"], "/home");
        assert_eq!(turns[4]["content"], "done: /home");
    }

    #[test]
    fn a_tool_call_only_turn_sends_null_content() {
        // The dialect's shape for an assistant turn that said nothing and only
        // called tools. `content: ""` would be a different message.
        let messages = vec![ChatMessage::assistant(
            r#"{"content":"","tool_calls":[{"id":"a","name":"x","arguments":"{}"}]}"#,
        )];
        let turns = build_chat_completions_messages(&messages);
        assert_eq!(turns.len(), 1);
        assert!(turns[0]["content"].is_null());
        assert_eq!(turns[0]["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_reasoning_only_envelope_with_no_text_is_dropped() {
        // `content: null` with no tool_calls is a message the API rejects, and
        // reasoning is not round-tripped on any wire here — so the turn
        // carries nothing and is left out rather than sent as invalid.
        let messages = vec![
            ChatMessage::assistant(r#"{"content":"","reasoning_content":"thinking hard"}"#),
            ChatMessage::user("still there?"),
        ];
        let turns = build_chat_completions_messages(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["role"], "user");
    }

    #[test]
    fn a_json_object_assistant_reply_is_not_swallowed() {
        // A model that answered in pure JSON round-trips as plain assistant
        // text — only ZR's own packing markers make an envelope.
        let messages = vec![ChatMessage::assistant(r#"{"answer":42}"#)];
        let turns = build_chat_completions_messages(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["role"], "assistant");
        assert_eq!(turns[0]["content"], r#"{"answer":42}"#);
    }

    #[test]
    fn an_unpackable_tool_message_still_reaches_the_upstream() {
        // ZR always packs tool results as {tool_call_id, name, content}, but a
        // message that is not that shape must not vanish: it becomes the tool
        // content verbatim with an empty id, matching both sibling wires.
        let messages = vec![ChatMessage::tool("raw output, unpacked")];
        let turns = build_chat_completions_messages(&messages);
        assert_eq!(turns[0]["role"], "tool");
        assert_eq!(turns[0]["tool_call_id"], "");
        assert_eq!(turns[0]["content"], "raw output, unpacked");
    }

    #[test]
    fn the_default_endpoint_is_openai_and_an_override_wins() {
        let default = ChatCompletionsWire::new("openai", "k", None, Some(64), 900);
        assert_eq!(default.api_url, CHAT_COMPLETIONS_URL);
        // The override is ordinary configuration for this wire, not a test
        // seam: it is how a local server is addressed at all.
        let local = ChatCompletionsWire::new(
            "llama",
            "k",
            Some("http://127.0.0.1:8080/v1/chat/completions/"),
            None,
            900,
        );
        assert_eq!(local.api_url, "http://127.0.0.1:8080/v1/chat/completions");
    }

    #[test]
    fn request_body_carries_tools_limits_and_asks_for_streaming_usage() {
        let wire = ChatCompletionsWire::new("local", "k", None, Some(512), 900);
        let messages = vec![ChatMessage::user("hello")];
        let tools = vec![crate::provider::ToolSpec {
            name: "shell".into(),
            description: "run a command".into(),
            parameters: json!({"type": "object"}),
        }];
        let streaming = wire.request_body("qwen3", &messages, Some(&tools), Some(0.2), true);
        assert_eq!(streaming["model"], "qwen3");
        assert_eq!(streaming["stream"], true);
        assert_eq!(streaming["temperature"], 0.2);
        // `max_tokens`, not `max_completion_tokens`: see the field's comment.
        assert_eq!(streaming["max_tokens"], 512);
        assert_eq!(streaming["tools"][0]["type"], "function");
        assert_eq!(streaming["tools"][0]["function"]["name"], "shell");
        assert_eq!(streaming["tool_choice"], "auto");
        // The whole reason streaming usage arrives at all when it does.
        assert_eq!(streaming["stream_options"]["include_usage"], true);

        let blocking = wire.request_body("qwen3", &messages, None, None, false);
        assert_eq!(blocking["stream"], false);
        assert!(
            blocking.get("stream_options").is_none(),
            "include_usage is a streaming-only request field"
        );
        assert!(blocking.get("tools").is_none());
        assert!(blocking.get("temperature").is_none());
    }

    #[test]
    fn envelope_parse_lifts_usage_text_tool_calls_and_reasoning() {
        let envelope: ChatCompletionsEnvelope = serde_json::from_value(json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "hello world",
                    "reasoning_content": "let me think",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"pwd\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 40, "completion_tokens": 9,
                       "total_tokens": 49,
                       "prompt_tokens_details": {"cached_tokens": 12}}
        }))
        .expect("envelope parses");
        let response = parse_chat_completions_envelope("local", envelope);
        assert_eq!(response.text.as_deref(), Some("hello world"));
        assert_eq!(response.reasoning_content.as_deref(), Some("let me think"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_9");
        assert_eq!(response.tool_calls[0].name, "shell");
        assert_eq!(response.tool_calls[0].arguments, r#"{"command":"pwd"}"#);
        let usage = response.usage.expect("usage is the point of this module");
        // Already ZR's convention: prompt_tokens IS the total, cached is its
        // subset. Nothing to normalize, unlike the Anthropic wire.
        assert_eq!(usage.input_tokens, Some(40));
        assert_eq!(usage.output_tokens, Some(9));
        assert_eq!(usage.cached_input_tokens, Some(12));
    }

    #[test]
    fn a_tool_call_only_reply_reports_no_text() {
        // `content: ""` (or null) alongside tool calls is the dialect's
        // tool-only reply; it must not become `Some("")`, which downstream
        // reads as "the model said something empty".
        for content in [json!(""), Value::Null] {
            let envelope: ChatCompletionsEnvelope = serde_json::from_value(json!({
                "choices": [{"message": {"role": "assistant", "content": content,
                    "tool_calls": [{"id": "a", "type": "function",
                                     "function": {"name": "x", "arguments": "{}"}}]},
                    "finish_reason": "tool_calls"}]
            }))
            .expect("envelope parses");
            let response = parse_chat_completions_envelope("local", envelope);
            assert!(response.text.is_none());
            assert_eq!(response.tool_calls.len(), 1);
        }
    }

    #[test]
    fn missing_usage_stays_none_never_zero() {
        // No usage key at all, on a response with and without choices: the
        // wire reports nothing rather than a zeroed report.
        for body in [
            json!({"choices": []}),
            json!({"choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}]}),
        ] {
            let envelope: ChatCompletionsEnvelope =
                serde_json::from_value(body.clone()).expect("envelope parses");
            assert!(
                parse_chat_completions_envelope("local", envelope)
                    .usage
                    .is_none(),
                "no usage on the wire = None; downstream policy decides ({body})"
            );
        }

        // An EMPTY usage object is the one shape that survives as
        // `Some(TokenUsage)` — with every field `None`, never zero. Both
        // sibling wires do exactly the same thing (serde defaults their
        // `Option` count fields), and the invariant that actually protects a
        // customer holds regardless of which side of the Option it lands on:
        // no field is fabricated, so nothing billable comes out of it. Pinned
        // here rather than "fixed" so this wire cannot silently drift away
        // from the other two.
        let envelope: ChatCompletionsEnvelope =
            serde_json::from_value(json!({"choices": [], "usage": {}})).expect("envelope parses");
        let usage = parse_chat_completions_envelope("local", envelope).usage;
        assert_eq!(usage, Some(TokenUsage::default()));
        assert_eq!(
            usage.map(|usage| usage.input_tokens),
            Some(None),
            "an unreported count is absent, never zero"
        );
        assert!(
            crate::openai::OpenAiUsage::try_from_provider(usage.as_ref()).is_none(),
            "and it cannot become a billable number"
        );
    }

    #[test]
    fn a_partial_usage_report_keeps_the_half_it_was_given() {
        // Some local servers report only completion tokens. The wire reports
        // exactly what it was told; `OpenAiUsage::try_from_provider` is the
        // one place that decides a half-report is unusable.
        let envelope: ChatCompletionsEnvelope =
            serde_json::from_value(json!({"choices": [], "usage": {"completion_tokens": 12}}))
                .expect("envelope parses");
        let usage = parse_chat_completions_envelope("local", envelope)
            .usage
            .expect("a reported half is still a report");
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, Some(12));
        assert!(crate::openai::OpenAiUsage::try_from_provider(Some(&usage)).is_none());
    }

    #[test]
    fn error_text_speaks_the_classifier_taxonomy() {
        let error = chat_completions_upstream_error(
            "local",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "Rate limit reached for qwen3",
        );
        assert!(crate::retry::is_rate_limited(&error), "{error}");
        assert!(error.to_string().contains("429"), "{error}");

        let error = chat_completions_upstream_error(
            "local",
            reqwest::StatusCode::BAD_REQUEST,
            "This model's maximum context length is 8192 tokens",
        );
        assert!(
            matches!(
                crate::retry::classify(&error, false),
                crate::retry::FailureClass::ContextWindow { .. }
            ),
            "{error}"
        );

        let error = chat_completions_upstream_error(
            "local",
            reqwest::StatusCode::UNAUTHORIZED,
            "invalid api key",
        );
        assert_eq!(
            crate::retry::classify(&error, false),
            crate::retry::FailureClass::NonRetryable,
            "{error}"
        );
    }

    #[test]
    fn in_band_error_shapes_classify_as_429() {
        // Both shapes servers on this dialect actually send.
        for payload in [
            json!({"error": {"message": "Rate limit reached", "type": "rate_limit_error",
                              "code": "rate_limit_exceeded"}}),
            json!({"object": "error", "message": "rate limit exceeded",
                    "type": "rate_limit_exceeded"}),
        ] {
            let mut machine = ChatCompletionsStreamMachine::new(false);
            let error = machine
                .handle("local", &payload)
                .expect_err("an in-band error must fail the stream");
            let error = anyhow!(error.to_string());
            assert!(crate::retry::is_rate_limited(&error), "{error}");
        }

        // An unremarkable in-band error stays digit-free and retryable.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let error = machine
            .handle(
                "local",
                &json!({"error": {"message": "worker crashed", "type": "internal_error"}}),
            )
            .expect_err("an in-band error must fail the stream");
        let error = anyhow!(error.to_string());
        assert_eq!(
            crate::retry::classify(&error, false),
            crate::retry::FailureClass::Retryable,
            "{error}"
        );
    }

    #[test]
    fn an_explicit_null_error_field_is_not_an_error() {
        // Several servers stamp `"error": null` on every conforming chunk.
        // Reading that as an in-band failure would break every stream they
        // serve.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let events = machine
            .handle(
                "local",
                &json!({"error": null, "choices": [{"index": 0,
                        "delta": {"content": "hi"}, "finish_reason": null}]}),
            )
            .expect("a null error field is not an error");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn stream_machine_assembles_the_documented_event_grammar() {
        // The full documented shape of a chat-completions stream: an opening
        // role delta, content deltas, index-keyed tool_call deltas, the
        // finish_reason chunk, the choice-less usage chunk that arrives when
        // include_usage is honored, and the [DONE] sentinel.
        let mut machine = ChatCompletionsStreamMachine::new(true);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"hel"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"comman"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"d\":\"pwd\"}"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":12}}}"#,
                "[DONE]",
            ],
        );
        assert!(machine.finished);
        assert_eq!(delivered.finals, 1, "[DONE] is the only terminal");
        assert_eq!(delivered.text, "hello");
        // The empty opening content delta emits nothing at all.
        assert_eq!(
            delivered.token_counts,
            vec!["hel".len() / 4, "lo".len() / 4]
        );
        assert_eq!(delivered.tool_calls.len(), 1);
        assert_eq!(delivered.tool_calls[0].id, "call_1");
        assert_eq!(delivered.tool_calls[0].name, "shell");
        assert_eq!(delivered.tool_calls[0].arguments, r#"{"command":"pwd"}"#);
        assert_eq!(delivered.usage.len(), 1);
        assert_eq!(delivered.usage[0].input_tokens, Some(40));
        assert_eq!(delivered.usage[0].output_tokens, Some(9));
        assert_eq!(delivered.usage[0].cached_input_tokens, Some(12));
        // The finish_reason is the upstream's own word, not a synthesized one.
        assert_eq!(machine.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn the_terminal_is_done_not_the_finish_reason_chunk() {
        // THE billing hazard of this dialect: usage arrives AFTER the
        // finish_reason chunk. A machine that terminated on finish_reason
        // would deliver the answer and settle it unbilled.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(delivered.finals, 0, "finish_reason is not the terminal");
        assert!(delivered.usage.is_empty());
        assert!(!machine.finished);

        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3}}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.finals, 1);
        assert_eq!(delivered.usage.len(), 1);
        assert_eq!(delivered.usage[0].input_tokens, Some(11));
    }

    #[test]
    fn a_stream_that_ignores_include_usage_reports_no_usage() {
        // llama.cpp, Ollama, and LM Studio may stream to completion without
        // ever honoring stream_options. The stream must still complete
        // normally, and must report NO usage — never a zeroed one.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"local answer"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.text, "local answer");
        assert_eq!(delivered.finals, 1);
        assert!(
            delivered.usage.is_empty(),
            "an unreported usage is absent, not zero"
        );
    }

    #[test]
    fn a_socket_that_closes_after_finish_reason_is_a_finished_stream() {
        // Not every server sends the [DONE] sentinel. The stream loop asks
        // this question before deciding whether the close was a completion or
        // a fault; the completion path flushes exactly what the terminal does.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            ],
        );
        assert!(machine.saw_finish_reason());
        assert!(!machine.finished);
        let events = machine.terminate();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::Usage(_)))
        );
        assert!(matches!(events.last(), Some(StreamEvent::Final)));

        // A stream that never said anything about finishing gets no such
        // benefit — the loop fails it instead.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[r#"{"choices":[{"index":0,"delta":{"content":"cut off"},"finish_reason":null}]}"#],
        );
        assert!(!machine.saw_finish_reason());
    }

    #[test]
    fn tool_calls_assemble_by_index_across_fragments() {
        // Parallel calls interleave by index, ids and names may arrive on any
        // fragment, and a fragment with no index at all falls back to its
        // position rather than collapsing onto one slot.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"first","arguments":"{\"x\":"}},{"index":1,"id":"b","function":{"name":"second","arguments":"{\"y\":"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.tool_calls.len(), 2);
        // Emitted in index order, whatever order the fragments arrived in.
        assert_eq!(delivered.tool_calls[0].id, "a");
        assert_eq!(delivered.tool_calls[0].name, "first");
        assert_eq!(delivered.tool_calls[0].arguments, r#"{"x":1}"#);
        assert_eq!(delivered.tool_calls[1].id, "b");
        assert_eq!(delivered.tool_calls[1].name, "second");
        assert_eq!(delivered.tool_calls[1].arguments, r#"{"y":2}"#);

        // A whole call in one indexless fragment, and a call whose arguments
        // never arrived: the latter becomes "{}" rather than an empty string,
        // which is not valid JSON for a caller to parse.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"solo","type":"function","function":{"name":"ping"}}]}}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.tool_calls.len(), 1);
        assert_eq!(delivered.tool_calls[0].id, "solo");
        assert_eq!(delivered.tool_calls[0].arguments, "{}");
    }

    #[test]
    fn two_indexless_calls_in_separate_frames_stay_two_calls() {
        // Position disambiguates indexless fragments WITHIN one frame; across
        // frames it does not, and both of these would otherwise land on slot
        // 0 — the second overwriting the first's id and name and appending its
        // arguments onto the first's, producing one call with the body
        // `{"x":1}{"y":2}`, which is not even valid JSON. Early Ollama and
        // Mistral-compat builds emit exactly this shape. A non-empty id that
        // disagrees with the slot's own is proof the fragment belongs to a
        // different call.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_a","type":"function","function":{"name":"first","arguments":"{\"x\":1}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_b","type":"function","function":{"name":"second","arguments":"{\"y\":2}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(
            delivered.tool_calls.len(),
            2,
            "two indexless calls across two frames are two calls"
        );
        assert_eq!(delivered.tool_calls[0].id, "call_a");
        assert_eq!(delivered.tool_calls[0].name, "first");
        assert_eq!(delivered.tool_calls[0].arguments, r#"{"x":1}"#);
        assert_eq!(delivered.tool_calls[1].id, "call_b");
        assert_eq!(delivered.tool_calls[1].name, "second");
        assert_eq!(delivered.tool_calls[1].arguments, r#"{"y":2}"#);
    }

    #[test]
    fn an_indexless_continuation_still_joins_its_own_call() {
        // The other half of the rule: a fragment with no id, or one repeating
        // the id already in the slot, is a CONTINUATION and must keep
        // accumulating rather than opening a second call.
        for second_fragment in [
            r#"{"function":{"arguments":"2}"}}"#,
            r#"{"id":"call_a","function":{"arguments":"2}"}}"#,
        ] {
            let mut machine = ChatCompletionsStreamMachine::new(false);
            let delivered = run(
                &mut machine,
                &[
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_a","function":{"name":"only","arguments":"{\"y\":"}}]}}]}"#,
                    &format!(
                        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{second_fragment}]}}}}]}}"#
                    ),
                    "[DONE]",
                ],
            );
            assert_eq!(delivered.tool_calls.len(), 1, "{second_fragment}");
            assert_eq!(delivered.tool_calls[0].id, "call_a");
            assert_eq!(delivered.tool_calls[0].arguments, r#"{"y":2}"#);
        }
    }

    #[test]
    fn reasoning_deltas_stream_without_a_token_estimate() {
        // The dialect's thinking models put chain-of-thought in
        // `delta.reasoning_content`, and ZeroRouter's own stream shape has a
        // field for it. The per-chunk estimate is defined over content, so a
        // reasoning-only chunk contributes nothing to it.
        let mut machine = ChatCompletionsStreamMachine::new(true);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"reasoning_content":"weighing options here"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"the answer is four"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.reasoning, "weighing options here");
        assert_eq!(delivered.text, "the answer is four");
        assert_eq!(
            delivered.token_counts,
            vec![0, "the answer is four".len() / 4]
        );
    }

    #[test]
    fn a_repeated_done_yields_one_final_and_one_usage() {
        // Same rule both sibling machines hold: a doubled terminal must not
        // emit a second Final, nor a second Usage that metering would read as
        // authoritative — and a chunk trailing the terminal is ignored
        // outright rather than reopening the stream.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":1}}"#],
        );
        let first = machine.handle_payload("local", "[DONE]").expect("terminal");
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, StreamEvent::Final))
                .count(),
            1
        );
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, StreamEvent::Usage(_)))
                .count(),
            1
        );
        assert!(
            machine
                .handle_payload("local", "[DONE]")
                .expect("a repeated terminal is ignored, not an error")
                .is_empty()
        );
        assert!(
            machine
                .handle_payload(
                    "local",
                    r#"{"choices":[],"usage":{"prompt_tokens":999999,"completion_tokens":999999}}"#
                )
                .expect("a trailing chunk is ignored, not an error")
                .is_empty()
        );
    }

    #[test]
    fn the_done_sentinel_survives_lf_and_crlf_framing() {
        // The sentinel is not JSON, so it has to survive the shared decoder
        // and reach the machine as text under either legal framing.
        for terminator in ["\n\n", "\r\n\r\n"] {
            let wire = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hi\"}}}}]}}{terminator}data: [DONE]{terminator}"
            );
            let mut raw_buffer = Vec::new();
            let mut buffer = String::new();
            let payloads = drain_sse_payloads(&mut raw_buffer, &mut buffer, wire.as_bytes())
                .expect("well-formed stream");
            assert_eq!(payloads.len(), 2);
            assert_eq!(payloads[1], "[DONE]");
            let mut machine = ChatCompletionsStreamMachine::new(false);
            let delivered = run(
                &mut machine,
                &payloads.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            assert_eq!(delivered.text, "hi");
            assert_eq!(delivered.finals, 1);
        }
    }

    #[test]
    fn a_soft_close_without_usage_gets_its_own_gap_label() {
        // Both of these settle unbilled, which is exactly why the log must
        // tell them apart. A server that framed its stream correctly and
        // simply ignored include_usage is a known, tolerated limitation; a
        // socket that vanished before the sentinel AND before any usage is
        // what a truncating middlebox looks like, and letting it wear the
        // first label would hide fleet-wide revenue loss behind a shrug.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"answer"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(machine.usage_gap(), Some("include_usage_ignored"));

        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"answer"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(
            machine.usage_gap(),
            Some("done_missing"),
            "a stream that lost its tail is not a stream that lacks a feature"
        );

        // And a stream that DID report usage has no gap to label at all —
        // before or after the terminal takes the value.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            ],
        );
        assert_eq!(machine.usage_gap(), None);
        machine.terminate();
        assert_eq!(machine.usage_gap(), None, "the label survives the take");
    }

    #[test]
    fn only_the_first_choice_is_streamed() {
        // The buffered path deliberately reads choices[0]; streaming every
        // choice would let a nonconforming upstream interleave two answers
        // into one customer stream character by character, unseparable after
        // the fact. ZeroRouter never asks for more than one choice — `n` is
        // 400-rejected as an unsupported extension — so nothing legitimate is
        // lost.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"first"}},{"index":1,"delta":{"content":"SECOND"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.text, "first");
        assert!(
            !delivered.text.contains("SECOND"),
            "a second choice must not interleave into the customer's stream"
        );
    }

    #[test]
    fn object_valued_tool_arguments_are_serialized_not_dropped() {
        // The dialect says `arguments` is a JSON string, but heterogeneous
        // local servers sometimes send the parsed object. Reading only
        // `as_str` turned that into "" — downstream, a tool call with no
        // arguments, which is a silently wrong call rather than a visible
        // failure.
        let envelope: ChatCompletionsEnvelope = serde_json::from_value(json!({
            "choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "a", "type": "function",
                    "function": {"name": "shell", "arguments": {"command": "pwd"}}}]},
                "finish_reason": "tool_calls"}]
        }))
        .expect("envelope parses");
        let response = parse_chat_completions_envelope("local", envelope);
        let arguments: Value = serde_json::from_str(&response.tool_calls[0].arguments)
            .expect("the object round-trips as a JSON string");
        assert_eq!(arguments["command"], "pwd");

        // Same on the streaming path, where a whole call arrives in one frame.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"shell","arguments":{"command":"pwd"}}}]}}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.tool_calls.len(), 1);
        let arguments: Value = serde_json::from_str(&delivered.tool_calls[0].arguments)
            .expect("the object round-trips as a JSON string");
        assert_eq!(arguments["command"], "pwd");

        // A genuinely absent `arguments` still defaults, rather than becoming
        // the string "null".
        let envelope: ChatCompletionsEnvelope = serde_json::from_value(json!({
            "choices": [{"message": {"tool_calls": [{"id": "a",
                "function": {"name": "ping", "arguments": null}}]}}]
        }))
        .expect("envelope parses");
        let response = parse_chat_completions_envelope("local", envelope);
        assert_eq!(response.tool_calls[0].arguments, "{}");
    }

    #[test]
    fn partial_usage_survives_an_in_band_error() {
        // A stream that dies after a usage-bearing chunk has wire-reported
        // billable usage that must not settle as zero — the same rule the
        // Anthropic machine holds, taken exactly once.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"partial"}}],"usage":{"prompt_tokens":50,"completion_tokens":40}}"#,
            ],
        );
        machine
            .handle(
                "local",
                &json!({"error": {"message": "worker died", "type": "internal_error"}}),
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
}

#[cfg(test)]
mod wire_property_tests {
    //! Property tests over the wire decoders, driven by a deterministic
    //! PRNG so a failure is reproducible from its seed alone (no external
    //! fuzzing dependency, and CI stays hermetic). These target the layer
    //! where adversarial bytes meet money: everything here runs on data an
    //! upstream — or anything able to impersonate one — fully controls.

    use super::*;

    /// xorshift64*: tiny, deterministic, good enough to shuffle chunk
    /// boundaries and event shapes.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, bound: usize) -> usize {
            if bound == 0 {
                0
            } else {
                usize::try_from(self.next() % bound as u64).unwrap_or(0)
            }
        }
    }

    /// Feed `bytes` through the decoder in randomly sized chunks.
    fn decode_in_random_chunks(bytes: &[u8], rng: &mut Rng) -> Result<Vec<String>, StreamError> {
        let mut raw_buffer = Vec::new();
        let mut buffer = String::new();
        let mut payloads = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let take = 1 + rng.below(bytes.len() - at);
            payloads.extend(drain_sse_payloads(
                &mut raw_buffer,
                &mut buffer,
                &bytes[at..at + take],
            )?);
            at += take;
        }
        Ok(payloads)
    }

    #[test]
    fn chunk_boundaries_never_change_what_the_decoder_sees() {
        // The property that matters for billing: how the network happened
        // to split the bytes cannot change the events — including when the
        // split lands mid-multibyte-character or mid-delimiter.
        let events = [
            r#"{"type":"response.output_text.delta","delta":"naïve — 日本語 🎉"}"#,
            r#"{"type":"response.output_text.delta","delta":"second"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":40,"output_tokens":9}}}"#,
        ];
        let mut wire = String::new();
        for (index, event) in events.iter().enumerate() {
            // Alternate LF and CRLF framing; both are legal.
            wire.push_str(&format!(
                "data: {event}{}",
                if index % 2 == 0 { "\n\n" } else { "\r\n\r\n" }
            ));
        }
        let expected: Vec<String> = events.iter().map(|event| (*event).to_owned()).collect();

        let mut rng = Rng(0x5eed_1234_abcd_0001);
        for _ in 0..2_000 {
            let decoded = decode_in_random_chunks(wire.as_bytes(), &mut rng)
                .expect("a well-formed stream decodes under any chunking");
            assert_eq!(decoded, expected, "chunking changed the decoded events");
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_decoder() {
        // Anything an upstream can put on the wire: random bytes, stray
        // delimiters, `data:` lines with no payload, invalid UTF-8.
        let mut rng = Rng(0x5eed_1234_abcd_0002);
        for _ in 0..3_000 {
            let length = 1 + rng.below(512);
            let mut noise = Vec::with_capacity(length);
            for _ in 0..length {
                noise.push(match rng.below(8) {
                    0 => b'\n',
                    1 => b'\r',
                    2 => b':',
                    3 => b'd',
                    // Lone continuation bytes: never valid UTF-8 on their own.
                    4 => 0x80 | u8::try_from(rng.below(64)).unwrap_or(0),
                    _ => u8::try_from(rng.below(256)).unwrap_or(0),
                });
            }
            // The contract is "no panic and no corruption", not "no error":
            // a malformed stream is allowed to be rejected.
            let mut raw_buffer = Vec::new();
            let mut buffer = String::new();
            let _ = drain_sse_payloads(&mut raw_buffer, &mut buffer, &noise);
            // Whatever survived must still be valid UTF-8 by construction.
            assert!(std::str::from_utf8(buffer.as_bytes()).is_ok());
        }
    }

    #[test]
    fn an_unterminated_event_cannot_grow_the_buffer_without_bound() {
        // Memory-exhaustion guard: an upstream that opens an event and
        // never closes it must be cut off, not buffered forever.
        let mut raw_buffer = Vec::new();
        let mut buffer = String::new();
        let filler = vec![b'x'; 256 * 1024];
        let mut error = None;
        for _ in 0..64 {
            if let Err(hit) = drain_sse_payloads(&mut raw_buffer, &mut buffer, &filler) {
                error = Some(hit);
                break;
            }
        }
        let error = error.expect("an unterminated event must eventually be refused");
        assert!(
            matches!(error, StreamError::InvalidSse(_)),
            "the refusal is a malformed-stream error"
        );
        assert!(
            buffer.len() <= MAX_SSE_EVENT_BYTES + filler.len(),
            "the buffer stopped growing at the ceiling"
        );
    }

    #[test]
    fn the_anthropic_machine_survives_arbitrary_event_orders() {
        // Events out of documented order, repeated, or truncated: the
        // machine must never panic, never emit more than one Final, and
        // never report usage it was not told.
        let shapes = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "x"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
            json!({"type": "content_block_delta", "index": 9, "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_stop", "index": 7}),
            json!({"type": "message_delta", "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
            json!({"type": "ping"}),
            json!({"type": "unknown_future_event", "whatever": [1, 2, 3]}),
            json!({"type": "message_start"}),
            json!(null),
            json!([1, 2, 3]),
        ];
        let mut rng = Rng(0x5eed_1234_abcd_0003);
        for _ in 0..2_000 {
            let mut machine = AnthropicStreamMachine::new(rng.below(2) == 0);
            let mut finals = 0;
            for _ in 0..1 + rng.below(12) {
                let value = &shapes[rng.below(shapes.len())];
                match machine.handle("anthropic", value) {
                    Ok(events) => {
                        for event in events {
                            if matches!(event, StreamEvent::Final) {
                                finals += 1;
                            }
                            if let StreamEvent::Usage(usage) = event {
                                // Anthropic's own numbers are folded into
                                // ZR's convention; cached can never exceed
                                // the input total it was added into.
                                assert!(
                                    usage.cached_input_tokens.unwrap_or(0)
                                        <= usage.input_tokens.unwrap_or(0),
                                    "cached must stay a subset of input"
                                );
                            }
                        }
                    }
                    // In-band errors are a legal outcome, not a panic.
                    Err(_) => break,
                }
            }
            assert!(finals <= 1, "at most one terminal event per stream");
        }
    }

    #[test]
    fn a_repeated_terminal_in_one_chunk_yields_one_final_and_one_usage() {
        // The defect the order property found: both wires processed every
        // event in a chunk before checking `finished`, so a doubled
        // terminal emitted two Finals — and a second Usage that metering
        // would have read as authoritative.
        let mut machine = AnthropicStreamMachine::new(false);
        machine
            .handle(
                "anthropic",
                &json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "output_tokens": 1}}}),
            )
            .expect("message_start is not an error");
        let first = machine
            .handle("anthropic", &json!({"type": "message_stop"}))
            .expect("first terminal");
        let second = machine
            .handle(
                "anthropic",
                &json!({"type": "message_stop", "usage": {"output_tokens": 999_999}}),
            )
            .expect("a repeated terminal is ignored, not an error");
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, StreamEvent::Final))
                .count(),
            1
        );
        assert!(
            second.is_empty(),
            "the repeated terminal emits nothing at all"
        );
    }

    #[test]
    fn chat_completions_chunk_boundaries_never_change_what_the_decoder_sees() {
        // Same billing property as the sibling wires', over the chat dialect's
        // own frames — including the non-JSON `[DONE]` sentinel, which must
        // arrive intact however the network split it.
        let events = [
            r#"{"choices":[{"index":0,"delta":{"content":"naïve — 日本語 🎉"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":9}}"#,
            "[DONE]",
        ];
        let mut wire = String::new();
        for (index, event) in events.iter().enumerate() {
            wire.push_str(&format!(
                "data: {event}{}",
                if index % 2 == 0 { "\n\n" } else { "\r\n\r\n" }
            ));
        }
        let expected: Vec<String> = events.iter().map(|event| (*event).to_owned()).collect();

        let mut rng = Rng(0x5eed_1234_abcd_0011);
        for _ in 0..2_000 {
            let decoded = decode_in_random_chunks(wire.as_bytes(), &mut rng)
                .expect("a well-formed stream decodes under any chunking");
            assert_eq!(decoded, expected, "chunking changed the decoded events");
            // And the machine derives the same delivery from them every time.
            let mut machine = ChatCompletionsStreamMachine::new(false);
            let mut text = String::new();
            let mut usage = None;
            let mut finals = 0;
            for payload in &decoded {
                for event in machine
                    .handle_payload("local", payload)
                    .expect("well-formed payloads")
                {
                    match event {
                        StreamEvent::TextDelta(chunk) => text.push_str(&chunk.delta),
                        StreamEvent::Usage(seen) => usage = Some(seen),
                        StreamEvent::Final => finals += 1,
                        StreamEvent::ToolCall(_) => {}
                    }
                }
            }
            assert_eq!(text, "naïve — 日本語 🎉");
            assert_eq!(finals, 1);
            assert_eq!(usage.and_then(|usage| usage.output_tokens), Some(9));
        }
    }

    #[test]
    fn the_chat_completions_machine_survives_arbitrary_payload_orders() {
        // Payloads out of documented order, repeated, or truncated: the
        // machine must never panic, never emit more than one Final, and never
        // report usage it was not told.
        let shapes = [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"hm"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"x","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"junk"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}"#,
            "[DONE]",
            "[DONE]",
            r#"{"error":null,"choices":[]}"#,
            r#"{"object":"error","message":"boom"}"#,
            r#"{"unknown_future_shape":[1,2,3]}"#,
            "null",
            "[1,2,3]",
            "not json at all",
            "",
        ];
        let mut rng = Rng(0x5eed_1234_abcd_0012);
        for _ in 0..3_000 {
            let mut machine = ChatCompletionsStreamMachine::new(rng.below(2) == 0);
            let mut finals = 0;
            for _ in 0..1 + rng.below(14) {
                let payload = shapes[rng.below(shapes.len())];
                match machine.handle_payload("local", payload) {
                    Ok(events) => {
                        for event in events {
                            if matches!(event, StreamEvent::Final) {
                                finals += 1;
                            }
                            if let StreamEvent::Usage(usage) = event {
                                // Only ever the numbers the scripted shapes
                                // above actually contain.
                                assert!(usage.input_tokens.unwrap_or(0) <= 10);
                                assert!(usage.output_tokens.unwrap_or(0) <= 3);
                                assert!(usage.cached_input_tokens.unwrap_or(0) <= 4);
                            }
                        }
                    }
                    // Malformed payloads and in-band errors are legal
                    // outcomes, not panics.
                    Err(_) => break,
                }
            }
            assert!(finals <= 1, "at most one terminal event per stream");
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_chat_machine() {
        // Whatever an upstream can put inside a `data:` line, including
        // near-miss sentinels and deeply nested junk.
        let mut rng = Rng(0x5eed_1234_abcd_0013);
        for _ in 0..3_000 {
            let length = 1 + rng.below(128);
            let mut payload = String::with_capacity(length);
            for _ in 0..length {
                payload.push(match rng.below(10) {
                    0 => '{',
                    1 => '}',
                    2 => '[',
                    3 => ']',
                    4 => '"',
                    5 => ':',
                    6 => ',',
                    7 => 'D',
                    8 => 'O',
                    _ => char::from(u8::try_from(32 + rng.below(94)).unwrap_or(b' ')),
                });
            }
            let mut machine = ChatCompletionsStreamMachine::new(rng.below(2) == 0);
            let _ = machine.handle_payload("local", &payload);
            // Whatever happened, the machine is still a machine: asking it to
            // terminate must not panic either.
            let _ = machine.terminate();
        }
    }

    #[test]
    fn lying_usage_cannot_produce_a_negative_or_inflated_charge() {
        // An upstream reporting cached > input (or absurd magnitudes) must
        // not manufacture a credit or an overflow: the billing view clamps
        // cached to input, and the cost function is checked arithmetic.
        for (input, output, cached) in [
            (10_u64, 5_u64, 1_000_u64),
            (0, 0, u64::MAX),
            (u64::MAX, u64::MAX, u64::MAX),
            (1, 0, u64::MAX),
        ] {
            let usage = TokenUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cached_input_tokens: Some(cached),
            };
            if let Some(view) = crate::openai::OpenAiUsage::try_from_provider(Some(&usage)) {
                assert!(
                    view.cached_input_tokens() <= view.prompt_tokens,
                    "cached is clamped to the prompt total"
                );
            }
        }
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

    #[test]
    fn chat_completions_implausible_token_counts_are_refused_rather_than_billed() {
        // Above what the usage columns can store, every settlement would fail
        // forever. Treating the usage as absent routes the request through the
        // known missing-usage path instead.
        let absurd = ChatCompletionsUsage {
            prompt_tokens: Some(u64::from(u32::MAX)),
            completion_tokens: Some(10),
            prompt_tokens_details: None,
        };
        assert!(absurd.into_token_usage().is_none());

        // The ceiling applies to the cached dimension too, which is the one an
        // upstream can inflate without inflating the total it reports.
        let absurd_cache = ChatCompletionsUsage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(10),
            prompt_tokens_details: Some(ChatCompletionsPromptDetails {
                cached_tokens: Some(u64::MAX),
            }),
        };
        assert!(absurd_cache.into_token_usage().is_none());

        let ordinary = ChatCompletionsUsage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(10),
            prompt_tokens_details: Some(ChatCompletionsPromptDetails {
                cached_tokens: Some(400),
            }),
        };
        let usage = ordinary
            .into_token_usage()
            .expect("ordinary counts are believed");
        assert_eq!(usage.input_tokens, Some(1_000));
        assert_eq!(usage.cached_input_tokens, Some(400));
    }

    /// Drive usage chunks through the machine and read the terminal's report.
    fn billed_usage(reports: &[Value]) -> Option<TokenUsage> {
        let mut machine = ChatCompletionsStreamMachine::new(false);
        for usage in reports {
            machine
                .handle("local", &json!({"choices": [], "usage": usage}))
                .expect("a usage chunk is not an error");
        }
        machine
            .terminate()
            .into_iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
    }

    #[test]
    fn a_shrinking_usage_report_keeps_the_earlier_report_whole() {
        // Chat-completions usage reports are absolute SNAPSHOTS, not the
        // cumulative counters the Anthropic dialect sends, so a later report
        // that is smaller is a contradiction rather than a rewind. The earlier
        // report is kept ENTIRE — not merged with the later one field by
        // field, which is what the franken-merge test below exists to forbid.
        let usage = billed_usage(&[
            json!({"prompt_tokens": 900, "completion_tokens": 10_000,
                    "prompt_tokens_details": {"cached_tokens": 500}}),
            json!({"prompt_tokens": 1, "completion_tokens": 1,
                    "prompt_tokens_details": {"cached_tokens": 1}}),
        ])
        .expect("the terminal carries usage");
        assert_eq!(usage.input_tokens, Some(900));
        assert_eq!(usage.output_tokens, Some(10_000));
        assert_eq!(usage.cached_input_tokens, Some(500));
    }

    #[test]
    fn a_later_non_shrinking_report_replaces_the_earlier_one_whole() {
        // The ordinary case: a server that restates usage as the stream
        // progresses. The last complete report wins, in one piece.
        let usage = billed_usage(&[
            json!({"prompt_tokens": 500, "completion_tokens": 1}),
            json!({"prompt_tokens": 500, "completion_tokens": 400}),
        ])
        .expect("the terminal carries usage");
        assert_eq!(usage.input_tokens, Some(500));
        assert_eq!(usage.output_tokens, Some(400));
    }

    #[test]
    fn a_corrected_report_never_bills_a_pair_no_report_ever_sent() {
        // THE invariant this dialect's merge policy exists for. A per-field
        // maximum would take the prompt count from the first report and the
        // completion count from the second and bill {2_000_000_000, 400} — a
        // pair no chunk ever contained, and one that overcharges the prompt
        // side by four million times the corrected figure. Whatever is billed
        // must be a report the upstream actually sent.
        let usage = billed_usage(&[
            json!({"prompt_tokens": 2_000_000_000_u64, "completion_tokens": 1}),
            json!({"prompt_tokens": 500, "completion_tokens": 400}),
        ])
        .expect("the terminal carries usage");
        assert_eq!(
            (usage.input_tokens, usage.output_tokens),
            (Some(2_000_000_000), Some(1)),
            "the billed pair must equal one report the upstream sent, whole"
        );
    }

    #[test]
    fn usage_fields_no_report_has_set_are_filled_across_chunks() {
        // The one sanctioned cross-chunk combination: neither report contested
        // the other's field, so there is no competing value to choose between.
        // A server that states the prompt side early and the completion side
        // at the end meters correctly.
        let usage = billed_usage(&[
            json!({"prompt_tokens": 500}),
            json!({"completion_tokens": 400}),
        ])
        .expect("the terminal carries usage");
        assert_eq!(usage.input_tokens, Some(500));
        assert_eq!(usage.output_tokens, Some(400));

        // But a fill riding along with a SHRINKING field is refused with the
        // rest of its report — that would be field mixing through the back
        // door.
        let usage = billed_usage(&[
            json!({"prompt_tokens": 500}),
            json!({"prompt_tokens": 400, "completion_tokens": 900}),
        ])
        .expect("the terminal carries usage");
        assert_eq!(usage.input_tokens, Some(500));
        assert_eq!(
            usage.output_tokens, None,
            "the whole contradicting report is dropped, fills included"
        );
    }

    #[test]
    fn chat_completions_tool_assembly_is_bounded_across_events() {
        // The per-event cap bounds one frame; a stream that opens indices or
        // appends arguments forever must still be cut off.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let mut refused = false;
        for index in 0..(MAX_OPEN_TOOL_BLOCKS + 8) {
            if machine
                .handle(
                    "local",
                    &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": index, "id": "t", "function": {"name": "x", "arguments": ""}}]}}]}),
                )
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "concurrent tool blocks are capped");

        let mut machine = ChatCompletionsStreamMachine::new(false);
        let chunk = "x".repeat(64 * 1024);
        let mut refused = false;
        for _ in 0..(MAX_TOOL_ARGUMENT_BYTES / chunk.len() + 4) {
            if machine
                .handle(
                    "local",
                    &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "function": {"arguments": chunk}}]}}]}),
                )
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "cumulative tool arguments are capped");
    }

    #[test]
    fn chat_completions_tool_bytes_are_capped_across_blocks_not_per_block() {
        // The sibling test above appends everything to ONE index, so it passes
        // just as happily against a cap tracked per block — 64 blocks would
        // then buy 64× the ceiling. Spreading the same bytes round-robin over
        // eight indices pins that the budget is one budget for the whole
        // stream.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let chunk = "x".repeat(64 * 1024);
        let mut written = 0_usize;
        let mut refused = false;
        for round in 0..(MAX_TOOL_ARGUMENT_BYTES / chunk.len() + 16) {
            let index = round % 8;
            if machine
                .handle(
                    "local",
                    &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": index, "function": {"arguments": chunk}}]}}]}),
                )
                .is_err()
            {
                refused = true;
                break;
            }
            written += chunk.len();
        }
        assert!(refused, "the argument budget spans every open block");
        assert!(
            written <= MAX_TOOL_ARGUMENT_BYTES,
            "refusal came at the cumulative ceiling, not a multiple of it \
             (wrote {written} bytes across 8 blocks)"
        );
    }
}

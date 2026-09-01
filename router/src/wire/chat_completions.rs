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

use crate::provider::ChatMessage;
use crate::provider::{
    ChatRequest, ChatResponse, ModelProvider, ProviderCapabilities, StopReason, StreamChunk,
    StreamError, StreamEvent, StreamFinal, StreamOptions, StreamResult, TokenUsage, ToolCall,
    UsageGap,
};
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    MAX_OPEN_TOOL_BLOCKS, MAX_RESPONSE_BYTES, MAX_TOOL_ARGUMENT_BYTES, ResponseAttestation,
    believable, bounded_body, drain_sse_payloads, shared_upstream_clients,
};

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
    /// A per-response guarantee this upstream must restate on every answer,
    /// when its inventory entry declares one ([`ResponseAttestation`]).
    ///
    /// `None` for every upstream that declares nothing, which is all of them
    /// but `xai` — so this is additive and no existing lane changes behaviour.
    /// It rides on the WIRE rather than being checked by the walk because the
    /// only place the guarantee exists is the upstream's own response headers,
    /// and this is the one layer that holds them: by the time a response
    /// reaches the walk it is a `ChatResponse` or a stream of events, and the
    /// headers are gone.
    attestation: Option<ResponseAttestation>,
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
            attestation: None,
        }
    }

    /// Require this upstream to restate a per-response guarantee on every
    /// answer it gives.
    ///
    /// A builder rather than a sixth constructor argument: every other caller
    /// of [`Self::new`] — four adapters' worth of local inference servers, the
    /// hosted-ZeroRouter rung, and this module's own tests — declares nothing,
    /// and threading a `None` through all of them would put the mechanism in
    /// front of readers who have no use for it while changing no behaviour.
    #[must_use]
    pub fn with_attestation(mut self, attestation: ResponseAttestation) -> Self {
        self.attestation = Some(attestation);
        self
    }

    /// Assert the declared guarantee, if this upstream declared one.
    ///
    /// Both dispatch paths call this at the same point — the instant response
    /// headers exist and before the body is touched — and that placement is
    /// the streaming half of the contract. See [`Self::stream_chat`].
    fn attest(&self, response: &reqwest::Response) -> Result<(), String> {
        match &self.attestation {
            Some(attestation) => {
                attestation.verify(&self.alias, response.status(), response.headers())
            }
            None => Ok(()),
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
/// A user turn's `content`, as this dialect wants it.
///
/// A turn carrying no image stays a PLAIN STRING rather than becoming a
/// one-element `[{"type":"text",…}]` array. That is not a cosmetic choice: a
/// text-only array and a string are semantically identical here, every
/// existing byte-stability pin in this module was written against the string,
/// and an upstream that is merely OpenAI-*compatible* is likelier to have
/// been tested against the string form. Only a turn that actually carries an
/// image pays the array.
///
/// The image part is OpenAI's documented shape — `{"type":"image_url",
/// "image_url":{"url":…}}`, where `url` is "either a URL of the image or the
/// base64 encoded image data" (openai/openai-openapi,
/// `ChatCompletionRequestMessageContentPartImage`, read 2026-08-21). A base64
/// image is re-assembled into the `data:` URI it arrived as, because that is
/// the one form every upstream on this wire documents.
fn user_content(message: &ChatMessage) -> Value {
    // No structured content: the string is the turn, verbatim. Checked before
    // anything else so the plain-text path cannot be reached by any grammar.
    if message.parts.is_empty() {
        return json!(message.content);
    }
    let parts = super::user_parts(message);
    if !parts
        .iter()
        .any(|part| !matches!(part, super::UserPart::Text(_)))
    {
        return json!(message.content);
    }
    let parts: Vec<Value> = parts
        .into_iter()
        .map(|part| match part {
            super::UserPart::Text(text) => json!({ "type": "text", "text": text }),
            super::UserPart::Base64Image { media_type, data } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") },
            }),
            super::UserPart::UrlImage(url) => json!({
                "type": "image_url",
                "image_url": { "url": url },
            }),
        })
        .collect();
    json!(parts)
}

pub(super) fn build_chat_completions_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role.as_str() {
            "system" => out.push(json!({ "role": "system", "content": message.content })),
            "user" => out.push(json!({ "role": "user", "content": user_content(message) })),
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
            // This dialect reports no cache-write dimension. Absent, not zero:
            // a zero here would assert the upstream measured no writes, which
            // it never said.
            cache_write_input_tokens: None,
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

fn parse_chat_completions_envelope(envelope: ChatCompletionsEnvelope) -> ChatResponse {
    // `n` is not a field ZeroRouter's compat surface accepts (it lands in
    // `extra` and is 400-rejected as an unsupported extension), so a
    // conforming response has exactly one choice. Reading the first rather
    // than concatenating all of them keeps a nonconforming upstream from
    // interleaving two answers into one.
    let choice = envelope.choices.first();
    let message = choice.and_then(|choice| choice.get("message"));
    // This dialect already speaks the router's vocabulary, so normalization is
    // a parse rather than a translation. The value used to be logged and
    // dropped here (`note_finish_reason`); it is carried and persisted now, so
    // the log said only that the router was throwing it away.
    let stop_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .and_then(StopReason::from_openai);
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
        stop_reason,
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
            cache_write_input_tokens: None,
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
pub(super) struct ChatCompletionsStreamMachine {
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
    pub(super) fn new(count_tokens: bool) -> Self {
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
    fn usage_gap(&self) -> Option<UsageGap> {
        if self.usage_reported {
            return None;
        }
        Some(if self.saw_done {
            UsageGap::IncludeUsageIgnored
        } else {
            UsageGap::DoneMissing
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
    pub(super) fn handle_payload(
        &mut self,
        alias: &str,
        data: &str,
    ) -> Result<Vec<StreamEvent>, StreamError> {
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
    pub(super) fn terminate(&mut self) -> Vec<StreamEvent> {
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
        // Both facts ride the terminal. `usage_gap` is read AFTER
        // `partial_usage` deliberately — it answers from `usage_reported`,
        // which the take does not disturb, so the label is the same either
        // way and the ordering here is the one the gap's own doc describes.
        events.push(StreamEvent::Final(StreamFinal {
            stop_reason: self
                .finish_reason
                .as_deref()
                .and_then(StopReason::from_openai),
            usage_gap: self.usage_gap(),
        }));
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
            // ZR packs OpenAI image parts as `[IMAGE:<url>]` markers and this
            // wire decodes them back into `image_url` content parts, so it
            // carries vision. Verified per upstream on this wire (Fireworks,
            // xAI, Google AI Studio, Vertex openapi) rather than assumed —
            // `tiers.toml` records what each lane may actually be sent, and
            // the Vertex shape caveat is written up there.
            vision: true,
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
        // BEFORE the status is consulted and before a byte of the body is
        // read. A response that cannot attest the guarantee is not a response
        // this lane may use for anything — not to serve, not to classify, not
        // to meter — so nothing downstream of here should ever see one.
        self.attest(&response).map_err(|failure| anyhow!(failure))?;
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
        Ok(parse_chat_completions_envelope(envelope))
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
        let attestation = self.attestation.clone();
        let count_tokens = options.count_tokens;

        let stream = async_stream::try_stream! {
            let response = authorized(http.post(&api_url), &credential)
                .json(&body)
                .send()
                .await
                .map_err(|error| StreamError::Http(error.to_string()))?;
            // THE STREAMING HALF OF THE CONTRACT, and the reason it is
            // satisfied structurally rather than by a rule someone has to
            // remember.
            //
            // A streaming answer is delivered in pieces, so "refuse rather
            // than serve" has a deadline: once the first chunk has left for
            // the customer, the request has been served under a guarantee
            // nobody checked, and there is no taking it back. The header this
            // reads arrives in the INITIAL HTTP response headers — the same
            // headers a non-streaming call gets — which land before the SSE
            // body opens. So the assertion sits here, between `send()` and
            // `bytes_stream()`, where no chunk can have been yielded yet: the
            // only `yield` statements in this function are downstream of this
            // line, which makes "assert before forwarding any chunk" a
            // property of the control flow rather than a convention.
            //
            // Worth recording what is NOT established: xAI documents the
            // header as present on "every API response" and says nothing
            // anywhere about streaming, SSE, or trailers (docs.x.ai, verified
            // 2026-08-20). If a streaming response ever omits it, this lane
            // fails closed on every streamed request — loudly, immediately,
            // and in the safe direction. That is the correct behaviour for an
            // unverified assumption, not a gap to paper over with a special
            // case for streams.
            if let Some(attestation) = &attestation
                && let Err(failure) = attestation.verify(&alias, response.status(), response.headers())
            {
                Err(StreamError::Http(failure))?;
                return;
            }
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
                        Some(gap @ UsageGap::DoneMissing) => tracing::warn!(
                            provider = %alias,
                            usage_gap = gap.as_str(),
                            "chat completions stream closed without [DONE] and without usage; \
                             settling unbilled"
                        ),
                        gap => tracing::debug!(
                            provider = %alias,
                            usage_gap = gap.map_or("none", UsageGap::as_str),
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

/// The per-response retention attestation, driven over real HTTP.
///
/// These tests are the reason the mechanism can be trusted, so they are written
/// against a real socket rather than against [`ResponseAttestation::verify`]
/// directly. Verifying the predicate in isolation would prove the comparison
/// works while proving nothing about the property that actually matters — that
/// a response failing it never reaches a customer — and the two are separable:
/// every way this feature could regress (the call site deleted, moved after the
/// body read, moved after the first streamed chunk) leaves the predicate
/// perfectly correct and the guarantee broken.
#[cfg(test)]
mod attestation_tests {
    use super::*;
    use axum::http::HeaderMap;

    fn xai_attestation() -> ResponseAttestation {
        ResponseAttestation::new("x-zero-data-retention", "true")
            .expect("the shipped declaration must be constructible")
    }

    /// An upstream that answers one completion, stamping `header` on the
    /// response when `Some`. `None` scripts an upstream that says nothing.
    async fn upstream_attesting(header: Option<&'static str>) -> String {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || async move {
                let mut headers = HeaderMap::new();
                if let Some(value) = header {
                    headers.insert(
                        "x-zero-data-retention",
                        value.parse().expect("a test header value must parse"),
                    );
                }
                (
                    headers,
                    axum::Json(serde_json::json!({
                        "choices": [{
                            "message": {"role": "assistant", "content": "the answer"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 11, "completion_tokens": 7}
                    })),
                )
            }),
        );
        serve(app).await
    }

    /// The same, for a STREAMING upstream: SSE deltas that a customer would see
    /// if anything forwarded them.
    async fn streaming_upstream_attesting(header: Option<&'static str>) -> String {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || async move {
                let mut headers = HeaderMap::new();
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    "text/event-stream".parse().expect("static content type"),
                );
                if let Some(value) = header {
                    headers.insert(
                        "x-zero-data-retention",
                        value.parse().expect("a test header value must parse"),
                    );
                }
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"leaked\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                );
                (headers, body)
            }),
        );
        serve(app).await
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("attestation upstream should bind");
        let address = listener
            .local_addr()
            .expect("attestation upstream should report its address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}/v1/chat/completions")
    }

    fn wire(url: &str, attestation: Option<ResponseAttestation>) -> ChatCompletionsWire {
        let wire = ChatCompletionsWire::new("xai", "k", Some(url), Some(64), 30);
        match attestation {
            Some(attestation) => wire.with_attestation(attestation),
            None => wire,
        }
    }

    async fn chat_against(
        url: &str,
        attestation: Option<ResponseAttestation>,
    ) -> anyhow::Result<ChatResponse> {
        let messages = vec![ChatMessage::user("hello")];
        wire(url, attestation)
            .chat(
                ChatRequest {
                    messages: &messages,
                    tools: None,
                },
                "grok-4.6",
                None,
            )
            .await
    }

    #[tokio::test]
    async fn a_response_attesting_zero_retention_is_served() {
        // The control. Without this the three refusal tests below would all
        // still pass against a wire that refused unconditionally, which is a
        // fail-closed check that has stopped being a check.
        let url = upstream_attesting(Some("true")).await;
        let response = chat_against(&url, Some(xai_attestation()))
            .await
            .expect("an attested response must be served");
        assert_eq!(response.text.as_deref(), Some("the answer"));
        assert_eq!(
            response
                .usage
                .expect("usage rides the response")
                .input_tokens,
            Some(11)
        );
    }

    #[tokio::test]
    async fn a_response_denying_zero_retention_is_refused() {
        // THE CENTRAL TEST. The upstream returns a perfectly good 200 with a
        // usable completion in it and says it is NOT operating under zero data
        // retention. Serving that completion is the failure this whole lane
        // exists to prevent: the customer bought a zero-retention model and
        // their prompt is now sitting in a 30-day audit log.
        let url = upstream_attesting(Some("false")).await;
        let error = chat_against(&url, Some(xai_attestation()))
            .await
            .expect_err("a response denying the guarantee must not be served");
        let text = error.to_string();
        assert!(
            text.contains(super::super::RETENTION_ATTESTATION_MARKER),
            "the failure must carry the marker the walk classifies on: {text}"
        );
        assert!(
            text.contains("x-zero-data-retention") && text.contains("`false`"),
            "the failure must name the header and what it actually said: {text}"
        );
    }

    #[tokio::test]
    async fn a_response_with_no_attestation_header_is_refused() {
        // Absent and `false` are the SAME state as far as a customer's data is
        // concerned: in neither case has anyone said the prompt will not be
        // kept. Treating absence as consent is the single most tempting way to
        // weaken this check — it makes the lane resilient to a vendor who stops
        // sending the header — and it would silently convert every request into
        // an unguaranteed one.
        let url = upstream_attesting(None).await;
        let error = chat_against(&url, Some(xai_attestation()))
            .await
            .expect_err("a response that attests nothing must not be served");
        let text = error.to_string();
        assert!(
            text.contains(super::super::RETENTION_ATTESTATION_MARKER),
            "{text}"
        );
        assert!(
            text.contains("absent"),
            "the failure must distinguish silence from denial: {text}"
        );
    }

    #[tokio::test]
    async fn presentation_is_normalized_but_a_denial_survives_every_normalization() {
        // xAI's own documentation renders the values as `"true"` and `"false"`
        // WITH the quote marks, and does not say whether the quotes are on the
        // wire or are the docs' typography. Both presentations are accepted, and
        // so is a casing change, because refusing either would take the lane
        // down over formatting. The property that must hold through all of it:
        // no normalization can turn a denial into a pass.
        for accepted in ["true", "\"true\"", "TRUE", "True", " true "] {
            let url = upstream_attesting(Some(accepted)).await;
            assert!(
                chat_against(&url, Some(xai_attestation())).await.is_ok(),
                "{accepted:?} is the guarantee in a different hat and must be accepted"
            );
        }
        for refused in [
            "false",
            "\"false\"",
            "FALSE",
            "",
            "\"\"",
            "1",
            "yes",
            "truthy",
        ] {
            let url = upstream_attesting(Some(refused)).await;
            assert!(
                chat_against(&url, Some(xai_attestation())).await.is_err(),
                "{refused:?} is not the guarantee and must be refused"
            );
        }
    }

    #[tokio::test]
    async fn an_upstream_declaring_no_attestation_is_unaffected() {
        // Additivity, stated as a test rather than as a comment. Every other
        // upstream on this wire — llama.cpp, Ollama, vLLM, a hosted ZeroRouter,
        // Gemini, Fireworks — declares nothing and must keep being served
        // whatever headers it happens to send, including a `false` one it means
        // nothing by.
        let url = upstream_attesting(Some("false")).await;
        let response = chat_against(&url, None)
            .await
            .expect("an upstream that declares no attestation is not checked");
        assert_eq!(response.text.as_deref(), Some("the answer"));
    }

    #[tokio::test]
    async fn a_streaming_response_denying_retention_forwards_no_chunk() {
        // THE STREAMING CONTRACT, and the reason it needs its own test: on this
        // path "refuse rather than serve" has a deadline. The scripted upstream
        // below sends a delta reading "leaked", so if the assertion ran anywhere
        // downstream of the first chunk this test sees it. What must arrive is
        // one error and nothing else — no role primer, no delta, no usage.
        let url = streaming_upstream_attesting(Some("false")).await;
        let messages = vec![ChatMessage::user("hello")];
        let mut stream = wire(&url, Some(xai_attestation())).stream_chat(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "grok-4.6",
            None,
            StreamOptions {
                enabled: true,
                count_tokens: true,
            },
        );

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        assert_eq!(
            events.len(),
            1,
            "a stream that failed attestation must yield exactly one event, the error"
        );
        let error = events
            .remove(0)
            .expect_err("the single event must be the refusal");
        let StreamError::Http(text) = error else {
            panic!("the refusal should arrive as an Http stream error: {error:?}");
        };
        assert!(
            text.contains(super::super::RETENTION_ATTESTATION_MARKER),
            "{text}"
        );
        assert!(
            !text.contains("leaked"),
            "no model output may appear anywhere in the failure: {text}"
        );
    }

    #[tokio::test]
    async fn a_streaming_response_attesting_zero_retention_streams_normally() {
        // The streaming control, for the same reason the buffered one exists.
        let url = streaming_upstream_attesting(Some("true")).await;
        let messages = vec![ChatMessage::user("hello")];
        let mut stream = wire(&url, Some(xai_attestation())).stream_chat(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "grok-4.6",
            None,
            StreamOptions {
                enabled: true,
                count_tokens: true,
            },
        );

        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta(chunk) = event.expect("an attested stream must not error")
            {
                text.push_str(&chunk.delta);
            }
        }
        assert_eq!(text, "leaked", "the attested stream delivers its content");
    }
}

#[cfg(test)]
mod chat_completions_tests {
    use super::*;
    use crate::provider::ContentPart;

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
        /// What the last terminal carried out — the upstream's own stop reason
        /// and its usage-gap label.
        terminal: StreamFinal,
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
                    StreamEvent::Final(terminal) => {
                        delivered.finals += 1;
                        delivered.terminal = terminal;
                    }
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

    /// The byte pin for this dialect's image parts.
    ///
    /// Written against OpenAI's own schema
    /// (`ChatCompletionRequestMessageContentPartImage`): the part is
    /// `image_url` and `image_url` is an OBJECT with a `url` key — NOT the
    /// bare string the Responses dialect uses, which is the mistake this pin
    /// exists to catch. A base64 image is re-assembled into the `data:` URI
    /// it arrived as, because "either a URL of the image or the base64
    /// encoded image data" is what that `url` field takes.
    ///
    /// Keys serialize alphabetically (`serde_json` without `preserve_order`),
    /// which is why `image_url` precedes `type` below.
    #[test]
    fn image_parts_become_openai_image_parts_byte_for_byte() {
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::Text("what is this? ".to_owned()),
            ContentPart::Image("data:image/png;base64,AAAA".to_owned()),
            ContentPart::Text(" and this? ".to_owned()),
            ContentPart::Image("https://example.com/x.jpg".to_owned()),
        ])];
        let built = build_chat_completions_messages(&messages);
        assert_eq!(
            serde_json::to_string(&built).unwrap(),
            concat!(
                r#"[{"content":["#,
                r#"{"text":"what is this? ","type":"text"},"#,
                r#"{"image_url":{"url":"data:image/png;base64,AAAA"},"type":"image_url"},"#,
                r#"{"text":" and this? ","type":"text"},"#,
                r#"{"image_url":{"url":"https://example.com/x.jpg"},"type":"image_url"}"#,
                r#"],"role":"user"}]"#,
            )
        );
    }

    /// A turn with no image keeps `content` as a PLAIN STRING. This is what
    /// keeps every other byte pin in this module valid, and it matters on the
    /// wire too: an upstream that is merely OpenAI-compatible is likelier to
    /// have been tested against the string than against a one-element array.
    #[test]
    fn a_text_only_turn_stays_a_string_rather_than_becoming_an_array() {
        let built = build_chat_completions_messages(&[ChatMessage::user("plain question")]);
        assert_eq!(
            serde_json::to_string(&built).unwrap(),
            r#"[{"content":"plain question","role":"user"}]"#
        );
        // Including one whose text spells the retired marker EXACTLY, closing
        // bracket and all. On the marker scheme this became a real image part;
        // it is now what it always was, the customer's words.
        let built = build_chat_completions_messages(&[ChatMessage::user(
            "docs write it [IMAGE:https://example.com/x.jpg] like so",
        )]);
        assert_eq!(
            serde_json::to_string(&built).unwrap(),
            r#"[{"content":"docs write it [IMAGE:https://example.com/x.jpg] like so","role":"user"}]"#
        );
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
            cache_control: false,
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
        let response = parse_chat_completions_envelope(envelope);
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
            let response = parse_chat_completions_envelope(envelope);
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
                parse_chat_completions_envelope(envelope).usage.is_none(),
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
        let usage = parse_chat_completions_envelope(envelope).usage;
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
        let usage = parse_chat_completions_envelope(envelope)
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
        assert!(matches!(events.last(), Some(StreamEvent::Final(_))));

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
                .filter(|event| matches!(event, StreamEvent::Final(_)))
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
        assert_eq!(machine.usage_gap(), Some(UsageGap::IncludeUsageIgnored));

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
            Some(UsageGap::DoneMissing),
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

    /// The gap label has to leave the wire on the terminal, or the free lane —
    /// which writes no attempt rows — has nowhere to record it. This is the
    /// countability requirement at its source.
    #[test]
    fn the_terminal_carries_the_usage_gap_out_of_the_wire() {
        // Framed correctly, no usage chunk: the ordinary local-server case.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let events = machine
            .handle_payload("local", "[DONE]")
            .expect("the sentinel is not an error");
        let Some(StreamEvent::Final(terminal)) = events.last() else {
            panic!("the sentinel must yield the terminal");
        };
        assert_eq!(terminal.usage_gap, Some(UsageGap::IncludeUsageIgnored));

        // Soft close after a finish_reason, no sentinel and no usage: what a
        // truncating middlebox looks like, and it must not hide inside the
        // ordinary label.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#],
        );
        let events = machine.terminate();
        let Some(StreamEvent::Final(terminal)) = events.last() else {
            panic!("terminate must yield the terminal");
        };
        assert_eq!(terminal.usage_gap, Some(UsageGap::DoneMissing));
        assert_eq!(
            terminal.stop_reason,
            Some(StopReason::Stop),
            "the soft close still carries the reason the upstream did state"
        );

        // Usage reported: no gap rides out at all.
        let mut machine = ChatCompletionsStreamMachine::new(false);
        run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            ],
        );
        let events = machine.terminate();
        let Some(StreamEvent::Final(terminal)) = events.last() else {
            panic!("terminate must yield the terminal");
        };
        assert_eq!(terminal.usage_gap, None);
    }

    /// This dialect already speaks the router's vocabulary, so extraction is a
    /// parse — but an unmapped value must still come out absent rather than be
    /// laundered into `stop`.
    #[test]
    fn chat_completions_finish_reasons_normalize_to_the_router_vocabulary() {
        let table = [
            ("stop", Some(StopReason::Stop)),
            ("length", Some(StopReason::Length)),
            ("tool_calls", Some(StopReason::ToolCalls)),
            // The pre-`tool_calls` spelling several local servers still emit.
            ("function_call", Some(StopReason::ToolCalls)),
            ("content_filter", Some(StopReason::ContentFilter)),
            ("something_new", None),
            ("", None),
        ];
        for (reported, expected) in table {
            assert_eq!(
                StopReason::from_openai(reported),
                expected,
                "finish_reason={reported:?}"
            );
        }
    }

    #[test]
    fn chat_completions_envelope_carries_the_real_finish_reason() {
        let envelope: ChatCompletionsEnvelope = serde_json::from_value(json!({
            "choices": [{"message": {"content": "clipped"}, "finish_reason": "length"}]
        }))
        .expect("envelope parses");
        assert_eq!(
            parse_chat_completions_envelope(envelope).stop_reason,
            Some(StopReason::Length)
        );
    }

    #[test]
    fn chat_completions_envelope_without_a_finish_reason_reports_none() {
        let envelope: ChatCompletionsEnvelope =
            serde_json::from_value(json!({"choices": [{"message": {"content": "hi"}}]}))
                .expect("envelope parses");
        assert_eq!(
            parse_chat_completions_envelope(envelope).stop_reason,
            None,
            "several servers in this dialect omit the field entirely"
        );
    }

    #[test]
    fn chat_completions_stream_terminal_carries_the_real_finish_reason() {
        let mut machine = ChatCompletionsStreamMachine::new(false);
        let delivered = run(
            &mut machine,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
                "[DONE]",
            ],
        );
        assert_eq!(delivered.finals, 1);
        assert_eq!(
            delivered.terminal.stop_reason,
            Some(StopReason::ContentFilter),
            "content_filter was structurally unobservable before the wires carried it"
        );
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
        let response = parse_chat_completions_envelope(envelope);
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
        let response = parse_chat_completions_envelope(envelope);
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
mod hostile_upstream_tests {
    //! The upstream is not trusted infrastructure — it is a network peer
    //! whose numbers become customer charges. These pin the bounds a
    //! deep-review pass identified as missing.

    use super::*;

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

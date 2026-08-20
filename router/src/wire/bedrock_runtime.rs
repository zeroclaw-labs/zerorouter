//! Bedrock's CLASSIC runtime plane — `bedrock-runtime.{region}.amazonaws.com`.
//!
//! ZeroRouter reaches Bedrock two ways, and they are different wires on
//! different hosts, not two spellings of one thing:
//!
//! - The **mantle** plane (`bedrock-mantle.{region}.api.aws/anthropic/v1/messages`)
//!   speaks the Messages API verbatim, so [`super::AnthropicWire`] serves it
//!   with only a base-URL override. It is the newer surface and the one AWS
//!   points 5-generation Claude at.
//! - The **classic runtime** plane, this module, speaks `InvokeModel` — the
//!   same Anthropic Messages *body*, wrapped in a different HTTP and streaming
//!   envelope. It is the only plane that serves 4.5-generation Claude, and on
//!   ZeroRouter's account it is the only Bedrock plane that answers at all
//!   today (the mantle plane's models sit behind an AWS Sales account gate and
//!   403 with "not available for this account").
//!
//! # The wire contract, and where each clause comes from
//!
//! Four things differ from the Messages endpoint. Every one of them is pinned
//! by a test below, because every one of them fails as a 400 from AWS rather
//! than as anything a reader would recognise:
//!
//! 1. **The model id is in the URL, not the body.** `POST /model/{id}/invoke`.
//!    AWS's InvokeModel reference makes `modelId` a required URI parameter and
//!    its request-body schema has no `model` member; an extraneous body key is
//!    refused (`ValidationException … extraneous key … is not permitted`). So
//!    [`super::anthropic::messages_request_body`] is called with `model: None`.
//! 2. **`anthropic_version` moves into the body.** AWS: "anthropic_version –
//!    (Required) The anthropic version. The value must be `bedrock-2023-05-31`."
//!    Note this is NOT the Anthropic API date — it is a Bedrock-specific token,
//!    which is why it is a separate constant from
//!    [`super::anthropic`]'s `ANTHROPIC_VERSION` rather than reusing it.
//! 3. **The `anthropic-version` HTTP header is not sent.** AWS splits these two
//!    explicitly: "`bedrock-runtime`: Set `"anthropic_version": "bedrock-2023-05-31"`
//!    in the request body when calling InvokeModel … `bedrock-mantle`: Set
//!    `anthropic-version: 2023-06-01` as an HTTP header." The header is not part
//!    of InvokeModel's HTTP binding. **Honest limit:** no AWS sentence says the
//!    header is *rejected* here, only that it is not modelled — so sending it
//!    would probably be ignored. It is omitted anyway, on the principle that a
//!    request should assert only what the API documents it may assert.
//! 4. **Auth is `Authorization: Bearer`, not `x-api-key`.** The same
//!    `BEDROCK_API_KEY` credential the mantle entry uses; Bedrock's auth header
//!    splits by API surface rather than by account.
//!
//! Streaming is the fifth difference and the largest: see [`Self::stream_chat`].
//!
//! # What is deliberately NOT re-implemented here
//!
//! Message packing, the three cache breakpoints, usage normalization (Anthropic
//! reports cache tokens outside `input_tokens`; ZeroRouter prices `cached` as a
//! subset of it), stop-reason mapping, and the streaming event grammar are all
//! the Messages API's, unchanged — so all of it is imported from
//! [`super::anthropic`] rather than copied. Only the envelope is new. A second
//! copy of the usage normalization in particular would be a second place for
//! the billing convention to drift, on a lane whose entire selling point is
//! that the customer is charged exactly what AWS invoices.

use crate::provider::{
    ChatMessage, ChatRequest, ChatResponse, ModelProvider, ProviderCapabilities, StreamError,
    StreamEvent, StreamOptions, StreamResult,
};
use anyhow::anyhow;
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::anthropic::{AnthropicStreamMachine, MessagesEnvelope, parse_messages_envelope};
use super::eventstream::{EventStreamDecoder, EventStreamMessage, MAX_EVENTSTREAM_PAYLOAD_BYTES};
use super::{MAX_RESPONSE_BYTES, bounded_body, shared_upstream_clients};

/// The `anthropic_version` the classic runtime requires in every request body.
///
/// Not a date in Anthropic's versioning scheme and not interchangeable with the
/// `anthropic-version: 2023-06-01` HEADER the Messages endpoint takes — it is a
/// Bedrock-specific literal, and AWS documents it as the only permitted value.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// ZeroRouter's client for Bedrock's classic runtime plane.
pub struct BedrockRuntimeWire {
    alias: String,
    /// The host root — e.g. `https://bedrock-runtime.us-east-1.amazonaws.com`.
    /// Unlike every other wire in this module this is NOT the URL that gets
    /// POSTed to: the per-request path carries the model id, so this wire
    /// builds its own path (see [`Self::invoke_url`]) and the provider entry is
    /// validated to declare a root rather than a full path.
    base_url: String,
    credential: String,
    max_tokens: u32,
    http: reqwest::Client,
    stream_http: reqwest::Client,
}

impl BedrockRuntimeWire {
    #[must_use]
    pub fn new(
        alias: &str,
        credential: &str,
        base_url: &str,
        max_tokens: u32,
        timeout_secs: u64,
    ) -> Self {
        let (http, stream_http) = shared_upstream_clients(timeout_secs);
        Self {
            alias: alias.to_owned(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            credential: credential.to_owned(),
            max_tokens,
            http,
            stream_http,
        }
    }

    /// The URL for one model, on one of the two invoke operations.
    ///
    /// The model id is interpolated into a PATH SEGMENT, and the ids this lane
    /// dispatches contain a colon (`us.anthropic.claude-opus-4-5-20251101-v1:0`).
    /// That is left unescaped deliberately: a colon is a legal path character
    /// (RFC 3986 `pchar`), AWS's own documented examples and every AWS SDK send
    /// it literally, and percent-encoding it produces a model id AWS does not
    /// recognise. `invoke_urls_carry_the_model_id_verbatim` pins that.
    fn invoke_url(&self, model: &str, streaming: bool) -> String {
        let operation = if streaming {
            "invoke-with-response-stream"
        } else {
            "invoke"
        };
        format!("{}/model/{model}/{operation}", self.base_url)
    }

    /// The request body: the Messages body minus `model`, plus
    /// `anthropic_version`.
    fn request_body(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[crate::provider::ToolSpec]>,
        temperature: Option<f64>,
    ) -> Value {
        let mut body = super::anthropic::messages_request_body(
            // No `model`: it is in the URL, and an extraneous key is a 400.
            None,
            messages,
            tools,
            temperature,
            // No `stream`: the runtime selects streaming by URL, not by a body
            // flag, and this key is not in AWS's schema either.
            None,
            self.max_tokens,
        );
        body["anthropic_version"] = json!(BEDROCK_ANTHROPIC_VERSION);
        body
    }
}

fn bedrock_upstream_error(alias: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    anyhow!(
        "{alias} bedrock runtime error: HTTP {} {}: {body}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error"),
    )
}

/// What one decoded event stream frame means to this wire.
#[derive(Debug)]
enum RuntimeFrame {
    /// A `chunk` event carrying one Anthropic streaming event.
    Chunk(Value),
    /// A frame this wire deliberately ignores — anything whose `:message-type`
    /// is neither `event` nor an error kind. Unknown-but-benign frames are
    /// skipped rather than fatal, matching how the Messages wire treats unknown
    /// SSE event types.
    Ignored,
}

/// Interpret one decoded frame.
///
/// Split out from the socket loop so the frame grammar is unit-testable without
/// a server, exactly as `AnthropicStreamMachine` is for the event grammar.
///
/// Three `:message-type` values matter, per the event stream specification:
/// `event` (a data frame; Bedrock's only member is `chunk`), `exception` (a
/// modelled error, named by `:exception-type`), and `error` (an unmodelled one,
/// carrying `:error-code` and `:error-message` in the headers with no payload).
fn interpret_frame(alias: &str, message: &EventStreamMessage) -> Result<RuntimeFrame, StreamError> {
    match message.header(":message-type") {
        Some("exception") => {
            // A modelled error. `:exception-type` names it; the payload carries
            // the human message. The digits matter: an in-band error has no HTTP
            // status, and `retry::classify` reads the text, so a throttle here
            // must still read as a 429 or the health cooldown never sees it.
            // Matched case-insensitively because AWS documents the member names
            // in lowerCamelCase and the error shapes in PascalCase, and never
            // prints an actual Bedrock exception frame to settle which is on
            // the wire.
            let kind = message.header(":exception-type").unwrap_or_default();
            let detail = String::from_utf8_lossy(message.payload());
            let prefix = if kind.eq_ignore_ascii_case("throttlingException") {
                "429 Too Many Requests: "
            } else {
                ""
            };
            Err(StreamError::ModelProvider(format!(
                "{alias} bedrock runtime stream failed: {prefix}{kind}: {detail}"
            )))
        }
        Some("error") => {
            let code = message.header(":error-code").unwrap_or("unknown");
            let detail = message.header(":error-message").unwrap_or_default();
            Err(StreamError::ModelProvider(format!(
                "{alias} bedrock runtime stream failed: {code}: {detail}"
            )))
        }
        Some("event") => {
            if message.header(":event-type") != Some("chunk") {
                return Ok(RuntimeFrame::Ignored);
            }
            // The payload is `{"bytes": "<base64>"}` — a Smithy blob, which
            // restJson1 renders as base64. The decoded bytes are one Anthropic
            // streaming event, identical to what an SSE `data:` line would
            // carry, which is what lets `AnthropicStreamMachine` handle it
            // unchanged.
            let envelope: Value = serde_json::from_slice(message.payload()).map_err(|error| {
                StreamError::InvalidSse(format!(
                    "{alias} bedrock runtime chunk is not JSON: {error}"
                ))
            })?;
            let encoded = envelope
                .get("bytes")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StreamError::InvalidSse(format!(
                        "{alias} bedrock runtime chunk has no `bytes` member"
                    ))
                })?;
            if encoded.len() > MAX_EVENTSTREAM_PAYLOAD_BYTES {
                return Err(StreamError::InvalidSse(format!(
                    "{alias} bedrock runtime chunk exceeds {MAX_EVENTSTREAM_PAYLOAD_BYTES} bytes"
                )));
            }
            let decoded = BASE64.decode(encoded).map_err(|error| {
                StreamError::InvalidSse(format!(
                    "{alias} bedrock runtime chunk is not valid base64: {error}"
                ))
            })?;
            let value: Value = serde_json::from_slice(&decoded).map_err(|error| {
                StreamError::InvalidSse(format!(
                    "{alias} bedrock runtime chunk does not decode to JSON: {error}"
                ))
            })?;
            Ok(RuntimeFrame::Chunk(value))
        }
        // No `:message-type` at all, or one this wire does not know. Ignored
        // rather than fatal: the spec allows the set to grow, and a frame this
        // router cannot read is not evidence the stream is broken.
        _ => Ok(RuntimeFrame::Ignored),
    }
}

#[async_trait]
impl ModelProvider for BedrockRuntimeWire {
    fn alias(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.alias)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Identical to the Messages wire's, and necessarily so: the body this
        // sends is the Messages body, so anything that wire can express this
        // one can too.
        ProviderCapabilities {
            native_tool_calling: true,
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
        let body = self.request_body(request.messages, request.tools, temperature);
        let response = self
            .http
            .post(self.invoke_url(model, false))
            .bearer_auth(&self.credential)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        // Same rule as the Messages wire: read the body under the already-known
        // status, so a truncated 401/429 body cannot surface as a status-less
        // transport error the classifier would happily retry.
        let (text, truncated) = bounded_body(response).await;
        if !status.is_success() {
            return Err(bedrock_upstream_error(&self.alias, status, &text));
        }
        if truncated {
            return Err(anyhow!(
                "{} bedrock runtime body exceeded {MAX_RESPONSE_BYTES} bytes",
                self.alias
            ));
        }
        // The success body is the standard Messages envelope — same `content`
        // blocks, same `usage` (including the cache dimensions), same
        // `stop_reason` — so the Messages parser reads it unchanged. Bedrock
        // adds fields of its own (`stop_details` was observed live); the
        // envelope ignores unknown members, so they cost nothing.
        let envelope: MessagesEnvelope = serde_json::from_str(&text).map_err(|error| {
            anyhow!(
                "{} bedrock runtime returned unparseable JSON: {error}",
                self.alias
            )
        })?;
        Ok(parse_messages_envelope(envelope))
    }

    /// Stream over `invoke-with-response-stream`.
    ///
    /// The transport is AWS event stream binary framing rather than SSE, so the
    /// decode is two layers where the Messages wire has one:
    /// [`super::eventstream`] turns bytes into checksummed frames, this function
    /// unwraps each frame's base64 `bytes` member into the Anthropic streaming
    /// event it holds, and `AnthropicStreamMachine` — the very same state
    /// machine the Messages wire drives — turns those into router events. The
    /// event grammar, the usage assembly, and the single-terminal rule are
    /// therefore shared rather than reimplemented.
    ///
    /// Every abnormal exit emits reported-so-far usage before the error, on the
    /// same rule the Messages wire follows: Anthropic reports input at
    /// `message_start` and output at `message_delta`, so a stream that dies
    /// after those has billable, wire-reported numbers, and settling it at zero
    /// would serve delivered output for free.
    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> futures_util::stream::BoxStream<'static, StreamResult<StreamEvent>> {
        let body = self.request_body(request.messages, request.tools, temperature);
        let http = self.stream_http.clone();
        let url = self.invoke_url(model, true);
        let credential = self.credential.clone();
        let alias = self.alias.clone();
        let count_tokens = options.count_tokens;

        let stream = async_stream::try_stream! {
            let response = http
                .post(&url)
                .bearer_auth(&credential)
                .header("content-type", "application/json")
                // Ask for the framing this function knows how to decode. AWS
                // answers `vnd.amazon.eventstream` on this operation either way,
                // but stating it makes the expectation explicit rather than
                // inherited.
                .header("accept", "application/vnd.amazon.eventstream")
                .json(&body)
                .send()
                .await
                .map_err(|error| StreamError::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                // An error on this operation is a normal JSON body, not a
                // frame — AWS only switches to eventstream once the call is
                // accepted.
                let (text, _) = bounded_body(response).await;
                Err(StreamError::Http(
                    bedrock_upstream_error(&alias, status, &text).to_string(),
                ))?;
                return;
            }

            let mut bytes = response.bytes_stream();
            let mut decoder = EventStreamDecoder::default();
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
                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        // A framing failure is terminal by design — see the
                        // eventstream module on why a desynchronised stream
                        // cannot be resynchronised. `InvalidSse` is the router's
                        // "the upstream's framing is malformed" class; its name
                        // predates a non-SSE wire, and the message says
                        // eventstream so an operator is never told to go looking
                        // for `data:` lines that were never there.
                        if let Some(usage) = machine.partial_usage() {
                            yield StreamEvent::Usage(usage);
                        }
                        Err(StreamError::InvalidSse(format!("{alias}: {error}")))?;
                        return;
                    }
                };
                for frame in frames {
                    let value = match interpret_frame(&alias, &frame) {
                        Ok(RuntimeFrame::Chunk(value)) => value,
                        Ok(RuntimeFrame::Ignored) => continue,
                        Err(error) => {
                            if let Some(usage) = machine.partial_usage() {
                                yield StreamEvent::Usage(usage);
                            }
                            Err(error)?;
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
                // The socket closed without `message_stop`. Surfaced rather
                // than synthesized, same as the Messages wire — and the
                // reported-so-far usage goes out first so delivered output is
                // still billed. A half-decoded frame in the buffer says the cut
                // landed mid-frame, which is worth telling apart from a stream
                // that ended tidily but early.
                if let Some(usage) = machine.partial_usage() {
                    yield StreamEvent::Usage(usage);
                }
                let detail = if decoder.has_partial_frame() {
                    "mid-frame"
                } else {
                    "on a frame boundary"
                };
                Err(StreamError::InvalidSse(format!(
                    "{alias} bedrock runtime stream ended without completion ({detail})"
                )))?;
            }
        };
        Box::pin(stream)
    }
}

#[cfg(test)]
mod wire_contract_tests {
    //! THE four clauses of the classic-runtime contract, each of which fails as
    //! an opaque AWS 400 if it regresses.

    use super::*;
    use crate::provider::ToolSpec;

    fn wire() -> BedrockRuntimeWire {
        BedrockRuntimeWire::new(
            "bedrock",
            "secret",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            512,
            900,
        )
    }

    #[test]
    fn the_request_body_omits_model_and_carries_anthropic_version() {
        // The two body clauses in one assertion, because they are two halves of
        // one edit: whoever adds `model` back is the same person who would drop
        // `anthropic_version`, and either alone is a ValidationException.
        let body = wire().request_body(&[ChatMessage::user("hi")], None, None);
        assert_eq!(
            body.get("anthropic_version").and_then(Value::as_str),
            Some("bedrock-2023-05-31"),
            "the classic runtime requires anthropic_version in the BODY"
        );
        assert!(
            body.get("model").is_none(),
            "the model id rides in the URL path; an extraneous body key is refused by AWS: {body}"
        );
        assert!(
            body.get("stream").is_none(),
            "streaming is selected by URL, not by a body flag: {body}"
        );
    }

    #[test]
    fn the_body_is_otherwise_the_messages_body_byte_for_byte() {
        // The claim that justifies sharing the builder rather than writing a
        // second one: apart from the three keys above, this body IS the
        // Messages body — same turn packing, same tool shape, same three cache
        // breakpoints. Asserted as a diff against the Messages wire's own
        // output so it cannot drift silently.
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("first question"),
            ChatMessage::assistant("first answer"),
            ChatMessage::user("second question"),
        ];
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "run a command".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];

        let mut runtime = wire().request_body(&messages, Some(&tools), Some(0.25));
        let mut messages_body = super::super::anthropic::messages_request_body(
            Some("claude-sonnet-5"),
            &messages,
            Some(&tools),
            Some(0.25),
            Some(false),
            512,
        );

        // Normalize away exactly the three documented differences.
        assert_eq!(
            runtime
                .as_object_mut()
                .and_then(|body| body.remove("anthropic_version")),
            Some(json!("bedrock-2023-05-31"))
        );
        if let Some(body) = messages_body.as_object_mut() {
            body.remove("model");
            body.remove("stream");
        }
        assert_eq!(
            runtime, messages_body,
            "the classic-runtime body must differ from the Messages body ONLY in model/stream/anthropic_version"
        );

        // And the cache breakpoints specifically survived, because those are
        // what the cached-input dimension meters.
        assert_eq!(runtime.to_string().matches("cache_control").count(), 3);
    }

    #[test]
    fn invoke_urls_carry_the_model_id_verbatim() {
        // The geographic inference-profile ids this lane dispatches contain a
        // colon. A well-meaning percent-encode turns
        // `...-v1:0` into `...-v1%3A0`, which is a model id AWS does not know —
        // and the failure is a 404-shaped mystery rather than anything naming
        // the encoding.
        let wire = wire();
        assert_eq!(
            wire.invoke_url("us.anthropic.claude-opus-4-5-20251101-v1:0", false),
            "https://bedrock-runtime.us-east-1.amazonaws.com\
             /model/us.anthropic.claude-opus-4-5-20251101-v1:0/invoke"
        );
        assert_eq!(
            wire.invoke_url("us.anthropic.claude-haiku-4-5-20251001-v1:0", true),
            "https://bedrock-runtime.us-east-1.amazonaws.com\
             /model/us.anthropic.claude-haiku-4-5-20251001-v1:0/invoke-with-response-stream"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_configured_host_does_not_double_up() {
        let wire = BedrockRuntimeWire::new(
            "bedrock",
            "secret",
            "https://bedrock-runtime.eu-west-1.amazonaws.com/",
            64,
            900,
        );
        assert_eq!(
            wire.invoke_url("m", false),
            "https://bedrock-runtime.eu-west-1.amazonaws.com/model/m/invoke"
        );
    }

    #[test]
    fn the_success_envelope_parses_with_bedrocks_observed_extra_fields() {
        // Verbatim shape from a live 200 on this plane: the Messages envelope
        // plus `stop_details` and the `cache_creation` breakdown, neither of
        // which the Messages parser models. Unknown members must be ignored,
        // not fatal — and the usage must still normalize to ZR's convention
        // (raw input + cache reads + cache writes as the input total).
        let envelope: MessagesEnvelope = serde_json::from_value(json!({
            "id": "msg_bdrk_01",
            "type": "message",
            "role": "assistant",
            "model": "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "stop_details": {"type": null},
            "usage": {
                "input_tokens": 100,
                "output_tokens": 9,
                "cache_creation_input_tokens": 10,
                "cache_read_input_tokens": 30,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 10,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        }))
        .expect("the observed live envelope parses");
        let response = parse_messages_envelope(envelope);
        assert_eq!(response.text.as_deref(), Some("hello"));
        let usage = response.usage.expect("usage is the point");
        assert_eq!(usage.input_tokens, Some(140));
        assert_eq!(usage.cached_input_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(9));
    }
}

#[cfg(test)]
mod frame_interpretation_tests {
    use super::super::eventstream::fixtures::{chunk_frame, frame, string_header};
    use super::*;
    use crate::provider::TokenUsage;

    /// Decode one frame's worth of bytes and interpret it.
    fn interpret(bytes: &[u8]) -> Result<RuntimeFrame, StreamError> {
        let mut decoder = EventStreamDecoder::default();
        let mut frames = decoder.push(bytes).expect("the fixture frames are valid");
        assert_eq!(frames.len(), 1);
        interpret_frame("bedrock", &frames.remove(0))
    }

    /// A `chunk` frame wrapping `event` the way Bedrock does.
    fn bedrock_chunk(event: &Value) -> Vec<u8> {
        let inner = BASE64.encode(event.to_string());
        chunk_frame(&json!({ "bytes": inner }).to_string())
    }

    #[test]
    fn a_chunk_frame_unwraps_to_its_anthropic_event() {
        let event = json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "text_delta", "text": "hi"}});
        let Ok(RuntimeFrame::Chunk(value)) = interpret(&bedrock_chunk(&event)) else {
            panic!("a chunk frame must unwrap to its event");
        };
        assert_eq!(value, event);
    }

    #[test]
    fn a_whole_stream_drives_the_shared_anthropic_machine() {
        // End to end over the decode stack, minus the socket: frames in,
        // router events out, through the SAME state machine the Messages wire
        // uses. The usage assertion is the one that matters — it proves the
        // normalization is shared rather than reimplemented.
        let events = [
            json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 100, "output_tokens": 1, "cache_read_input_tokens": 30}}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "hel"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 17}}),
            json!({"type": "message_stop"}),
        ];
        let mut wire: Vec<u8> = Vec::new();
        for event in &events {
            wire.extend_from_slice(&bedrock_chunk(event));
        }

        let mut decoder = EventStreamDecoder::default();
        let mut machine = AnthropicStreamMachine::new(false);
        let mut text = String::new();
        let mut usage: Option<TokenUsage> = None;
        let mut finals = 0;
        for frame in decoder.push(&wire).expect("valid frames") {
            let Ok(RuntimeFrame::Chunk(value)) = interpret_frame("bedrock", &frame) else {
                panic!("every frame here is a chunk");
            };
            for event in machine.handle("bedrock", &value).expect("no in-band error") {
                match event {
                    StreamEvent::TextDelta(chunk) => text.push_str(&chunk.delta),
                    StreamEvent::Usage(seen) => usage = Some(seen),
                    StreamEvent::Final(_) => finals += 1,
                    StreamEvent::ToolCall(_) => {}
                }
            }
        }
        assert_eq!(text, "hello");
        assert_eq!(finals, 1);
        let usage = usage.expect("the terminal carries usage");
        assert_eq!(usage.input_tokens, Some(130));
        assert_eq!(usage.cached_input_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(17));
        assert!(machine.finished);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn a_throttling_exception_frame_classifies_as_a_rate_limit() {
        // In-band errors carry no HTTP status, and `retry::classify` reads the
        // text — so a throttle that does not say "429" never reaches the health
        // cooldown and this rung keeps getting hammered.
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":message-type", "exception"));
        headers.extend_from_slice(&string_header(":exception-type", "throttlingException"));
        let error = interpret(&frame(&headers, br#"{"message":"Too many requests"}"#))
            .expect_err("an exception frame must fail the stream");
        let error = anyhow!(error.to_string());
        assert!(crate::retry::is_rate_limited(&error), "{error}");

        // AWS documents the member names in lowerCamelCase and the error shapes
        // in PascalCase without ever printing a real frame, so both must work.
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":message-type", "exception"));
        headers.extend_from_slice(&string_header(":exception-type", "ThrottlingException"));
        let error = interpret(&frame(&headers, b"{}")).expect_err("an exception frame fails");
        let error = anyhow!(error.to_string());
        assert!(crate::retry::is_rate_limited(&error), "{error}");
    }

    #[test]
    fn other_exception_frames_fail_the_stream_without_claiming_a_status() {
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":message-type", "exception"));
        headers.extend_from_slice(&string_header(
            ":exception-type",
            "modelStreamErrorException",
        ));
        let error = interpret(&frame(&headers, br#"{"message":"upstream cut"}"#))
            .expect_err("an exception frame must fail the stream");
        let text = error.to_string();
        assert!(text.contains("modelStreamErrorException"), "{text}");
        assert!(text.contains("upstream cut"), "{text}");
        assert!(
            !text.contains("429"),
            "only a throttle may borrow rate-limit digits: {text}"
        );
    }

    #[test]
    fn an_unmodelled_error_frame_reports_its_header_fields() {
        // These carry no payload at all — the whole message is in the headers.
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":message-type", "error"));
        headers.extend_from_slice(&string_header(":error-code", "InternalError"));
        headers.extend_from_slice(&string_header(
            ":error-message",
            "An internal server error occurred.",
        ));
        let error = interpret(&frame(&headers, b"")).expect_err("an error frame fails the stream");
        let text = error.to_string();
        assert!(text.contains("InternalError"), "{text}");
        assert!(
            text.contains("An internal server error occurred."),
            "{text}"
        );
    }

    #[test]
    fn unknown_frame_kinds_are_ignored_rather_than_fatal() {
        // The event stream's member set can grow, and an `event` frame whose
        // `:event-type` this router does not know is not evidence that anything
        // is wrong — same rule the Messages wire applies to unknown SSE types.
        for headers in [
            string_header(":message-type", "event"),
            {
                let mut headers = string_header(":message-type", "event");
                headers.extend_from_slice(&string_header(":event-type", "somethingNew"));
                headers
            },
            string_header(":irrelevant", "x"),
        ] {
            assert!(matches!(
                interpret(&frame(&headers, b"{}")).expect("an unknown frame is not an error"),
                RuntimeFrame::Ignored
            ));
        }
    }

    #[test]
    fn a_malformed_chunk_payload_is_refused_rather_than_guessed() {
        // Everything inside the frame is upstream-controlled too, and the CRCs
        // say nothing about it — a frame can be perfectly checksummed around
        // garbage. Each of these must error, and none may panic.
        let cases: Vec<Vec<u8>> = vec![
            // Not JSON at all.
            chunk_frame("not json"),
            // JSON, but no `bytes` member.
            chunk_frame(r#"{"chunk":"?"}"#),
            // `bytes` present but not a string.
            chunk_frame(r#"{"bytes":42}"#),
            // `bytes` present but not base64.
            chunk_frame(r#"{"bytes":"!!!not base64!!!"}"#),
            // Valid base64 that does not decode to JSON.
            chunk_frame(&json!({"bytes": BASE64.encode("still not json")}).to_string()),
        ];
        for wire in cases {
            let error = interpret(&wire).expect_err("a malformed chunk must be refused");
            assert!(
                matches!(error, StreamError::InvalidSse(_)),
                "a malformed payload is a framing-class error: {error}"
            );
        }
    }
}

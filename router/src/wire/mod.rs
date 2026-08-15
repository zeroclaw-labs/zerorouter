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
//! The stop-reason half of that debt is paid: `ChatResponse::stop_reason` and
//! `StreamFinal::stop_reason` carry the upstream's OWN reason, normalized to
//! the OpenAI vocabulary by each wire, and `content_filter` is observable for
//! the first time. Absence stays absent — an unmapped value is reported as
//! `None` rather than guessed — and the synthesis in `openai::finish_reason`
//! still covers exactly that case, with
//! `usage_events.finish_reason_source` ('upstream' vs 'synthetic') keeping the
//! two cohorts separable. This changed no billing: nothing in the crate
//! branches on a finish reason (see `openai::AttemptFinishReason` for the
//! divergence table and why every row's served/billed column reads
//! "unchanged").
//!
//! Scope discipline: these clients serve ZEROROUTER's traffic, not
//! zeroclaw-general. ZeroRouter's compat layer rejects structured content
//! and never emits multimodal markers, so the input builder handles exactly
//! the five packing shapes `openai::to_provider_message` produces — system
//! and user plain text, assistant text (with optional reasoning), assistant
//! tool-call packs, and tool-result packs — and is unit-tested against that
//! packer, not against hypothetical inputs.
//!

mod anthropic;
mod chat_completions;
mod responses;

pub use anthropic::AnthropicWire;
pub use chat_completions::ChatCompletionsWire;
pub use responses::OpenAiResponsesWire;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::provider::{StreamError, TokenUsage};
use futures_util::StreamExt;

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
pub(super) const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on tool-call assembly within one stream: how many tool blocks
/// may be open at once, and how many bytes of arguments may accumulate
/// across all of them. The per-event cap bounds a single SSE frame; these
/// bound the total a stream can accrete across legitimately terminated
/// frames.
pub(super) const MAX_OPEN_TOOL_BLOCKS: usize = 64;
pub(super) const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;

/// Largest token count either wire will believe. Postgres stores usage as
/// INTEGER, so anything at or above this both overcharges and — once it
/// exceeds the column — makes every settlement attempt fail permanently,
/// which is a denial of settlement rather than a billing error.
const MAX_BELIEVABLE_TOKENS: u64 = i32::MAX as u64;

/// Read an upstream body with a ceiling. Returns what arrived, and whether
/// it was truncated — the caller decides whether truncation is fatal (a
/// success body must parse; an error body only needs to be legible).
pub(super) async fn bounded_body(response: reqwest::Response) -> (String, bool) {
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
pub(super) fn believable(usage: TokenUsage) -> Option<TokenUsage> {
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
pub(super) fn shared_upstream_clients(timeout_secs: u64) -> (reqwest::Client, reqwest::Client) {
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
pub(super) fn drain_sse_payloads(
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

#[cfg(test)]
mod review_fix_tests {
    use super::*;

    #[test]
    fn event_boundaries_handle_lf_and_crlf_framing() {
        assert_eq!(find_event_boundary("data: a\n\nrest"), Some((7, 2)));
        assert_eq!(find_event_boundary("data: a\r\n\r\nrest"), Some((7, 4)));
        assert_eq!(find_event_boundary("data: a\r\n"), None);
        // Mixed stream: the earlier boundary wins.
        assert_eq!(find_event_boundary("a\n\nb\r\n\r\nc"), Some((1, 2)));
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
}

#[cfg(test)]
mod wire_property_tests {
    //! Property tests over the wire decoders, driven by a deterministic
    //! PRNG so a failure is reproducible from its seed alone (no external
    //! fuzzing dependency, and CI stays hermetic). These target the layer
    //! where adversarial bytes meet money: everything here runs on data an
    //! upstream — or anything able to impersonate one — fully controls.

    use super::anthropic::AnthropicStreamMachine;
    use super::chat_completions::ChatCompletionsStreamMachine;
    use super::*;
    use crate::provider::StreamEvent;
    use serde_json::json;

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
                            if matches!(event, StreamEvent::Final(_)) {
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
                .filter(|event| matches!(event, StreamEvent::Final(_)))
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
                        StreamEvent::Final(_) => finals += 1,
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
                            if matches!(event, StreamEvent::Final(_)) {
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

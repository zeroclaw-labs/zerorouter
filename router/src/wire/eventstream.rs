//! AWS event stream framing (`application/vnd.amazon.eventstream`).
//!
//! Bedrock's classic runtime plane does not stream SSE. `InvokeModelWithResponseStream`
//! answers with `content-type: application/vnd.amazon.eventstream` and a
//! sequence of length-prefixed, CRC-guarded binary frames — a completely
//! different transport from the `data:` lines every other wire in this module
//! decodes. This module is the frame layer for it, and nothing above it:
//! it turns bytes into `(headers, payload)` pairs and has no opinion about what
//! the payload means. `super::bedrock_runtime` supplies that opinion.
//!
//! # The format, and where it is written down
//!
//! The normative struct is the Amazon Event Stream Specification
//! (<https://smithy.io/2.0/aws/amazon-eventstream.html>), restated with byte
//! offsets in AWS's own service docs (Transcribe's "event stream encoding"
//! page is the clearest). All integers are big-endian.
//!
//! ```text
//! offset  width                         field
//! 0       4                             total_length   (whole frame, inclusive)
//! 4       4                             headers_length (headers section only)
//! 8       4                             prelude_crc    (CRC32 of bytes 0..8)
//! 12      headers_length                headers
//! 12+hl   total_length - hl - 16        payload
//! tl-4    4                             message_crc    (CRC32 of bytes 0..tl-4)
//! ```
//!
//! The two checksums cover deliberately different spans, and the reason is
//! stated in AWS's docs rather than inferred: the prelude CRC exists so a
//! corrupted LENGTH is caught **before** it is used, "without causing errors,
//! such as buffer overruns". This decoder honours that ordering — the prelude
//! checksum and the length sanity checks run before a single byte of payload is
//! addressed. The message CRC then covers everything ahead of itself, prelude
//! included. The spec is explicit that both MUST be validated and that the
//! stream MUST be terminated if either fails; [`EventStreamDecoder::push`] does
//! exactly that rather than skipping the frame, because a stream that has
//! desynchronised cannot be resynchronised — every subsequent length is read
//! from the wrong offset.
//!
//! The checksum is CRC-32 as specified by RFC 1952 (what AWS's docs call "GZIP
//! CRC32"): reflected polynomial `0xEDB88320`, init and final XOR `0xFFFFFFFF`
//! — the parameter set the `crc` crate names `CRC_32_ISO_HDLC`. AWS names the
//! RFC rather than the parameters, so the mapping to that constant is the one
//! step here taken from the referenced standard rather than from an AWS
//! sentence; `crc32_matches_rfc1952_check_vector` pins it against RFC 1952's
//! own published check value so a wrong constant fails locally instead of at
//! AWS.
//!
//! # Header entries
//!
//! Each entry is `name_len: u8`, `name: utf8[name_len]`, `value_type: u8`, then
//! a value whose width the type decides. Only type 7 (string) is surfaced —
//! every header this router reads (`:message-type`, `:event-type`,
//! `:exception-type`, `:error-code`, `:error-message`) is a string. The other
//! nine types are still *parsed*, because their widths must be traversed to
//! reach the next entry; they are simply not retained. An unknown type byte is
//! fatal for the same reason a bad CRC is: its width is unknown, so the parse
//! position is lost.

use std::collections::BTreeMap;

use crc::{CRC_32_ISO_HDLC, Crc};

/// Ceiling on one frame's payload, from the specification: "The payload of a
/// message MUST NOT exceed 25,165,824 bytes (24 MB)."
pub(super) const MAX_EVENTSTREAM_PAYLOAD_BYTES: usize = 25_165_824;

/// Ceiling on one frame's encoded headers, from the specification: "The encoded
/// headers of a message MUST NOT exceed 131,072 bytes (128 kB)."
pub(super) const MAX_EVENTSTREAM_HEADER_BYTES: usize = 131_072;

/// Bytes of framing overhead per message: the 12-byte prelude plus the trailing
/// 4-byte message CRC. Named because three separate bounds checks below are
/// only correct with respect to it.
const FRAME_OVERHEAD_BYTES: usize = 16;

/// The largest `total_length` this decoder will wait for.
///
/// Load-bearing against a hostile upstream rather than a tidy constant. Nothing
/// is preallocated from `total_length` — the decoder simply waits until the
/// buffer reaches it — so an absurd length is not an allocation bug, it is an
/// unbounded BUFFERING one: a frame claiming 4 GiB would have this decoder
/// accumulate every byte the peer cared to send while never completing a
/// message. Refusing at the spec's own ceiling bounds what one frame can hold
/// in memory to a little over 24 MB.
const MAX_FRAME_BYTES: usize =
    FRAME_OVERHEAD_BYTES + MAX_EVENTSTREAM_HEADER_BYTES + MAX_EVENTSTREAM_PAYLOAD_BYTES;

const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

/// Header value types, by their one-byte indicator. Widths are fixed except for
/// the two length-prefixed kinds, which carry a `u16` count.
const TYPE_BOOL_TRUE: u8 = 0;
const TYPE_BOOL_FALSE: u8 = 1;
const TYPE_BYTE: u8 = 2;
const TYPE_SHORT: u8 = 3;
const TYPE_INTEGER: u8 = 4;
const TYPE_LONG: u8 = 5;
const TYPE_BYTE_ARRAY: u8 = 6;
const TYPE_STRING: u8 = 7;
const TYPE_TIMESTAMP: u8 = 8;
const TYPE_UUID: u8 = 9;

/// A malformed frame. Every variant is terminal for the stream — see the module
/// docs on why a desynchronised event stream cannot be resynchronised.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EventStreamError {
    /// The prelude's own checksum failed, so the declared lengths cannot be
    /// trusted and must not be acted on.
    PreludeChecksum { declared: u32, computed: u32 },
    /// The whole-message checksum failed: the lengths were honest but the
    /// contents are corrupt.
    MessageChecksum { declared: u32, computed: u32 },
    /// A length field is self-contradictory or past a documented ceiling.
    Length(String),
    /// The headers section is malformed — a truncated entry, a non-UTF-8 name,
    /// or a value type whose width this decoder cannot skip.
    Headers(String),
}

impl std::fmt::Display for EventStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreludeChecksum { declared, computed } => write!(
                formatter,
                "eventstream prelude checksum mismatch (declared {declared:#010x}, computed {computed:#010x})"
            ),
            Self::MessageChecksum { declared, computed } => write!(
                formatter,
                "eventstream message checksum mismatch (declared {declared:#010x}, computed {computed:#010x})"
            ),
            Self::Length(detail) => write!(formatter, "eventstream framing is invalid: {detail}"),
            Self::Headers(detail) => write!(formatter, "eventstream headers are invalid: {detail}"),
        }
    }
}

/// One decoded frame: its string-valued headers and its raw payload.
#[derive(Debug)]
pub(super) struct EventStreamMessage {
    headers: BTreeMap<String, String>,
    payload: Vec<u8>,
}

impl EventStreamMessage {
    /// A string-valued header, or `None` when absent or non-string.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub(super) fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Incremental frame decoder: feed it network chunks, take back whole messages.
///
/// Shaped exactly like [`super::drain_sse_payloads`] — network chunking must not
/// be observable in the output — but it cannot share that function's code,
/// because this framing is binary and length-delimited where SSE is textual and
/// delimiter-delimited. The property they share, and the one worth stating, is
/// that a frame split across any number of network reads decodes identically to
/// one that arrived whole.
#[derive(Default)]
pub(super) struct EventStreamDecoder {
    buffer: Vec<u8>,
}

impl EventStreamDecoder {
    /// Feed one network chunk and return every frame it completed, in order.
    pub(super) fn push(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<EventStreamMessage>, EventStreamError> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some(message) = self.take_frame()? {
            messages.push(message);
        }
        Ok(messages)
    }

    /// Whether anything is held back awaiting more bytes. The caller reads this
    /// after the socket closes: a non-empty remainder means the upstream cut the
    /// stream mid-frame, which is a truncated response rather than a clean end.
    pub(super) fn has_partial_frame(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Decode one frame from the head of the buffer, or `Ok(None)` if the
    /// buffer does not hold a whole one yet.
    fn take_frame(&mut self) -> Result<Option<EventStreamMessage>, EventStreamError> {
        // The prelude is 12 bytes; without all of it neither length is known.
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let total_length = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]);
        let headers_length = u32::from_be_bytes([
            self.buffer[4],
            self.buffer[5],
            self.buffer[6],
            self.buffer[7],
        ]);
        let declared_prelude_crc = u32::from_be_bytes([
            self.buffer[8],
            self.buffer[9],
            self.buffer[10],
            self.buffer[11],
        ]);

        // FIRST, before either length is used for anything: the prelude's own
        // checksum. This ordering is the documented reason the prelude carries a
        // separate CRC at all — a corrupted length caught here never reaches the
        // arithmetic below.
        let computed_prelude_crc = CRC32.checksum(&self.buffer[..8]);
        if computed_prelude_crc != declared_prelude_crc {
            return Err(EventStreamError::PreludeChecksum {
                declared: declared_prelude_crc,
                computed: computed_prelude_crc,
            });
        }

        let total_length = total_length as usize;
        let headers_length = headers_length as usize;
        if total_length > MAX_FRAME_BYTES {
            return Err(EventStreamError::Length(format!(
                "frame declares {total_length} bytes, past the {MAX_FRAME_BYTES}-byte ceiling"
            )));
        }
        if headers_length > MAX_EVENTSTREAM_HEADER_BYTES {
            return Err(EventStreamError::Length(format!(
                "frame declares {headers_length} header bytes, past the \
                 {MAX_EVENTSTREAM_HEADER_BYTES}-byte ceiling"
            )));
        }
        // A frame must have room for its own overhead and its headers. Checked
        // as an addition on already-bounded values, so it cannot itself wrap.
        if total_length < FRAME_OVERHEAD_BYTES + headers_length {
            return Err(EventStreamError::Length(format!(
                "frame declares {total_length} total bytes but {headers_length} header bytes, \
                 which does not leave room for the {FRAME_OVERHEAD_BYTES} bytes of framing"
            )));
        }

        if self.buffer.len() < total_length {
            // A well-formed prelude for a frame still in flight.
            return Ok(None);
        }

        let declared_message_crc = u32::from_be_bytes([
            self.buffer[total_length - 4],
            self.buffer[total_length - 3],
            self.buffer[total_length - 2],
            self.buffer[total_length - 1],
        ]);
        let computed_message_crc = CRC32.checksum(&self.buffer[..total_length - 4]);
        if computed_message_crc != declared_message_crc {
            return Err(EventStreamError::MessageChecksum {
                declared: declared_message_crc,
                computed: computed_message_crc,
            });
        }

        let headers = parse_headers(&self.buffer[12..12 + headers_length])?;
        let payload = self.buffer[12 + headers_length..total_length - 4].to_vec();
        self.buffer.drain(..total_length);
        Ok(Some(EventStreamMessage { headers, payload }))
    }
}

/// Walk the headers section, retaining the string-valued entries.
fn parse_headers(mut section: &[u8]) -> Result<BTreeMap<String, String>, EventStreamError> {
    let mut headers = BTreeMap::new();
    while !section.is_empty() {
        let (name_length, rest) = split_first(section, 1, "header name length")?;
        let name_length = usize::from(name_length[0]);
        if name_length == 0 {
            return Err(EventStreamError::Headers(
                "a header name is zero bytes long; names must be at least one byte".to_owned(),
            ));
        }
        let (name, rest) = split_first(rest, name_length, "header name")?;
        let name = std::str::from_utf8(name)
            .map_err(|_| EventStreamError::Headers("a header name is not valid UTF-8".to_owned()))?
            .to_owned();
        let (value_type, rest) = split_first(rest, 1, "header value type")?;

        section = match value_type[0] {
            TYPE_BOOL_TRUE | TYPE_BOOL_FALSE => rest,
            TYPE_BYTE => split_first(rest, 1, "byte header value")?.1,
            TYPE_SHORT => split_first(rest, 2, "short header value")?.1,
            TYPE_INTEGER => split_first(rest, 4, "integer header value")?.1,
            TYPE_LONG | TYPE_TIMESTAMP => split_first(rest, 8, "64-bit header value")?.1,
            TYPE_UUID => split_first(rest, 16, "uuid header value")?.1,
            kind @ (TYPE_BYTE_ARRAY | TYPE_STRING) => {
                let (length, rest) = split_first(rest, 2, "header value length")?;
                let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
                let (value, rest) = split_first(rest, length, "header value")?;
                if kind == TYPE_STRING {
                    let value = std::str::from_utf8(value)
                        .map_err(|_| {
                            EventStreamError::Headers(format!(
                                "header {name} is typed string but is not valid UTF-8"
                            ))
                        })?
                        .to_owned();
                    // The spec forbids a repeated header name; last writer wins
                    // rather than erroring, because every header this router
                    // reads is a routing label and none of them is billing
                    // input — refusing the frame would be a harsher answer than
                    // the defect deserves.
                    headers.insert(name, value);
                }
                rest
            }
            unknown => {
                return Err(EventStreamError::Headers(format!(
                    "header {name} has unknown value type {unknown}, whose width is unknown, \
                     so the rest of the headers section cannot be located"
                )));
            }
        };
    }
    Ok(headers)
}

/// Split `count` bytes off the front, or report which field ran out.
///
/// Every read in [`parse_headers`] goes through here. That is the whole reason
/// a truncated or lying headers section produces an error instead of a panic:
/// there is no slice index in that function that this does not bounds-check.
fn split_first<'a>(
    bytes: &'a [u8],
    count: usize,
    field: &str,
) -> Result<(&'a [u8], &'a [u8]), EventStreamError> {
    if bytes.len() < count {
        return Err(EventStreamError::Headers(format!(
            "headers section ended mid-{field}: wanted {count} bytes, {} remain",
            bytes.len()
        )));
    }
    Ok(bytes.split_at(count))
}

#[cfg(test)]
pub(super) mod fixtures {
    //! Frame ENCODING, for tests only.
    //!
    //! There is no encoder in the production path — ZeroRouter only ever reads
    //! this framing — so the decoder's tests have to build their own input. The
    //! risk that creates is real and worth naming: an encoder written by reading
    //! the same lines of spec as the decoder can agree with a decoder that is
    //! wrong, and the pair will look green. Two things guard against that.
    //! `crc32_matches_rfc1952_check_vector` pins the checksum against a value
    //! published by RFC 1952 rather than produced here, and
    //! `a_frame_from_awss_documented_header_layout_decodes` builds its headers
    //! from the byte-by-byte table AWS prints in its own docs rather than from
    //! this encoder's idea of one.

    use super::*;

    /// Encode a string-valued header entry.
    pub(in crate::wire) fn string_header(name: &str, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(u8::try_from(name.len()).expect("test header names are short"));
        out.extend_from_slice(name.as_bytes());
        out.push(TYPE_STRING);
        out.extend_from_slice(
            &u16::try_from(value.len())
                .expect("test header values are short")
                .to_be_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out
    }

    /// Encode one whole frame with correct CRCs.
    pub(in crate::wire) fn frame(headers: &[u8], payload: &[u8]) -> Vec<u8> {
        let total_length = u32::try_from(FRAME_OVERHEAD_BYTES + headers.len() + payload.len())
            .expect("test frames are small");
        let headers_length = u32::try_from(headers.len()).expect("test headers are small");
        let mut out = Vec::new();
        out.extend_from_slice(&total_length.to_be_bytes());
        out.extend_from_slice(&headers_length.to_be_bytes());
        out.extend_from_slice(&CRC32.checksum(&out[..8]).to_be_bytes());
        out.extend_from_slice(headers);
        out.extend_from_slice(payload);
        let message_crc = CRC32.checksum(&out);
        out.extend_from_slice(&message_crc.to_be_bytes());
        out
    }

    /// A Bedrock-shaped `chunk` event carrying `payload` as its JSON body.
    pub(in crate::wire) fn chunk_frame(payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":event-type", "chunk"));
        headers.extend_from_slice(&string_header(":content-type", "application/json"));
        headers.extend_from_slice(&string_header(":message-type", "event"));
        frame(&headers, payload.as_bytes())
    }
}

#[cfg(test)]
mod eventstream_tests {
    use super::fixtures::{chunk_frame, frame, string_header};
    use super::*;

    #[test]
    fn crc32_matches_rfc1952_check_vector() {
        // THE pin that makes every other test in this file meaningful. AWS
        // documents the checksum only as "CRC32 (often referred to as GZIP
        // CRC32)" and links RFC 1952; it never names a polynomial. Mapping that
        // to `CRC_32_ISO_HDLC` is therefore an inference, and an inference this
        // module's own encoder cannot catch — a wrong constant would produce
        // frames this decoder happily accepts and AWS rejects.
        //
        // The check value below is the one the CRC catalogue publishes for
        // CRC-32/ISO-HDLC over the ASCII string "123456789": 0xCBF43926. It is
        // an external number, which is exactly why it is used here.
        assert_eq!(CRC32.checksum(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_well_formed_frame_decodes_to_its_headers_and_payload() {
        let mut decoder = EventStreamDecoder::default();
        let messages = decoder
            .push(&chunk_frame(r#"{"bytes":"abc"}"#))
            .expect("a well-formed frame decodes");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].header(":event-type"), Some("chunk"));
        assert_eq!(messages[0].header(":message-type"), Some("event"));
        assert_eq!(
            messages[0].header(":content-type"),
            Some("application/json")
        );
        assert_eq!(messages[0].header(":absent"), None);
        assert_eq!(messages[0].payload(), br#"{"bytes":"abc"}"#);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn a_frame_from_awss_documented_header_layout_decodes() {
        // Built from the byte-by-byte table AWS prints for an exception frame
        // (name length, name, type 7, uint16 value length, value) rather than
        // from this module's encoder, so the header grammar is checked against
        // the documentation and not against itself.
        let mut headers = Vec::new();
        headers.push(13u8);
        headers.extend_from_slice(b":content-type");
        headers.push(7);
        headers.extend_from_slice(&16u16.to_be_bytes());
        headers.extend_from_slice(b"application/json");
        headers.push(15);
        headers.extend_from_slice(b":exception-type");
        headers.push(7);
        headers.extend_from_slice(&19u16.to_be_bytes());
        headers.extend_from_slice(b"validationException");
        headers.push(13);
        headers.extend_from_slice(b":message-type");
        headers.push(7);
        headers.extend_from_slice(&9u16.to_be_bytes());
        headers.extend_from_slice(b"exception");

        let mut decoder = EventStreamDecoder::default();
        let messages = decoder
            .push(&frame(&headers, br#"{"message":"bad input"}"#))
            .expect("the documented layout decodes");
        assert_eq!(messages[0].header(":message-type"), Some("exception"));
        assert_eq!(
            messages[0].header(":exception-type"),
            Some("validationException")
        );
    }

    #[test]
    fn network_chunking_never_changes_the_decoded_frames() {
        // The billing property, same one `drain_sse_payloads` holds for SSE:
        // how the network split the bytes cannot change what was decoded. Run
        // at every possible split point rather than at a sampled few, because
        // the interesting boundaries (mid-prelude, mid-length, mid-CRC) are
        // exactly the ones a sampled test misses.
        let mut wire = chunk_frame(r#"{"bytes":"one"}"#);
        wire.extend_from_slice(&chunk_frame(r#"{"bytes":"two"}"#));
        let expected: Vec<Vec<u8>> = vec![
            br#"{"bytes":"one"}"#.to_vec(),
            br#"{"bytes":"two"}"#.to_vec(),
        ];

        for split in 0..=wire.len() {
            let mut decoder = EventStreamDecoder::default();
            let mut decoded: Vec<Vec<u8>> = Vec::new();
            for part in [&wire[..split], &wire[split..]] {
                for message in decoder.push(part).expect("a well-formed stream decodes") {
                    decoded.push(message.payload().to_vec());
                }
            }
            assert_eq!(decoded, expected, "split at {split} changed the decoding");
            assert!(!decoder.has_partial_frame(), "split at {split}");
        }

        // And byte-at-a-time, the worst chunking a network can produce.
        let mut decoder = EventStreamDecoder::default();
        let mut decoded: Vec<Vec<u8>> = Vec::new();
        for byte in &wire {
            for message in decoder
                .push(std::slice::from_ref(byte))
                .expect("a well-formed stream decodes")
            {
                decoded.push(message.payload().to_vec());
            }
        }
        assert_eq!(decoded, expected);
    }

    #[test]
    fn a_truncated_stream_reports_a_partial_frame_rather_than_a_message() {
        let wire = chunk_frame(r#"{"bytes":"cut"}"#);
        let mut decoder = EventStreamDecoder::default();
        let messages = decoder
            .push(&wire[..wire.len() - 1])
            .expect("a partial frame is not an error");
        assert!(messages.is_empty());
        assert!(
            decoder.has_partial_frame(),
            "an upstream that stops mid-frame must be distinguishable from one that ended"
        );
    }

    #[test]
    fn an_empty_payload_frame_is_legal() {
        let mut decoder = EventStreamDecoder::default();
        let messages = decoder
            .push(&frame(&string_header(":message-type", "event"), b""))
            .expect("a payload-less frame is well formed");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].payload().is_empty());
    }

    #[test]
    fn a_corrupted_message_body_is_refused_rather_than_delivered() {
        // THE reason the message CRC is checked at all. Flip one bit of the
        // payload and the frame must not be handed on — a corrupted Anthropic
        // chunk is a corrupted token count.
        let mut wire = chunk_frame(r#"{"bytes":"abc"}"#);
        let payload_at = wire.len() - 5;
        wire[payload_at] ^= 0x01;
        let error = EventStreamDecoder::default()
            .push(&wire)
            .expect_err("a corrupted payload must be refused");
        assert!(
            matches!(error, EventStreamError::MessageChecksum { .. }),
            "{error}"
        );
    }

    #[test]
    fn every_single_bit_flip_in_a_frame_is_caught() {
        // The claim the two CRCs exist to support, tested exhaustively rather
        // than by one example: no single-bit corruption anywhere in a frame can
        // produce a message this decoder accepts unchanged. A flip either fails
        // a checksum, fails the framing checks, or fails the header grammar —
        // what it must never do is yield the original payload as though nothing
        // happened, or panic.
        let wire = chunk_frame(r#"{"bytes":"abc"}"#);
        for index in 0..wire.len() {
            for bit in 0..8 {
                let mut corrupted = wire.clone();
                corrupted[index] ^= 1 << bit;
                let mut decoder = EventStreamDecoder::default();
                match decoder.push(&corrupted) {
                    Err(_) => {}
                    Ok(messages) => {
                        // Surviving without error is only acceptable if the
                        // frame did not complete (a length flip can make the
                        // decoder wait for more bytes).
                        assert!(
                            messages.is_empty(),
                            "bit {bit} of byte {index} produced an accepted frame"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_corrupted_length_is_caught_by_the_prelude_before_it_is_used() {
        // The prelude CRC's documented purpose: catch a bad length "without
        // causing errors, such as buffer overruns". Corrupt only the length and
        // the failure must name the PRELUDE — if it reported a message checksum
        // instead, the bad length had already been used to locate the trailing
        // CRC, which is the ordering bug this test exists to prevent.
        let mut wire = chunk_frame(r#"{"bytes":"abc"}"#);
        wire[3] ^= 0x40;
        let error = EventStreamDecoder::default()
            .push(&wire)
            .expect_err("a corrupted length must be refused");
        assert!(
            matches!(error, EventStreamError::PreludeChecksum { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_length_past_the_documented_ceiling_is_refused_before_buffering() {
        // A hostile peer's cheapest attack: a prelude with a HONEST checksum
        // over a dishonest length. The CRC passes, so only the ceiling stops
        // this — without it the decoder would buffer whatever the peer sent,
        // forever, waiting for a frame that will never complete.
        let mut prelude = Vec::new();
        prelude.extend_from_slice(&u32::MAX.to_be_bytes());
        prelude.extend_from_slice(&0u32.to_be_bytes());
        prelude.extend_from_slice(&CRC32.checksum(&prelude[..8]).to_be_bytes());
        let error = EventStreamDecoder::default()
            .push(&prelude)
            .expect_err("an absurd frame length must be refused");
        assert!(matches!(error, EventStreamError::Length(_)), "{error}");
    }

    #[test]
    fn a_frame_too_short_for_its_own_headers_is_refused() {
        // The other self-contradictory prelude: headers longer than the frame.
        // Unchecked, `12 + headers_length` runs past `total_length - 4` and the
        // payload slice would panic on an inverted range.
        let mut prelude = Vec::new();
        prelude.extend_from_slice(&20u32.to_be_bytes());
        prelude.extend_from_slice(&64u32.to_be_bytes());
        prelude.extend_from_slice(&CRC32.checksum(&prelude[..8]).to_be_bytes());
        prelude.resize(20, 0);
        let error = EventStreamDecoder::default()
            .push(&prelude)
            .expect_err("a frame that cannot hold its own headers must be refused");
        assert!(matches!(error, EventStreamError::Length(_)), "{error}");
    }

    #[test]
    fn a_truncated_header_entry_is_refused_rather_than_panicking() {
        // Header lengths are attacker-controlled too. A name length that runs
        // past the section, a value length that does, and a zero-length name
        // must all be errors — and none of them may index out of bounds.
        let cases: Vec<Vec<u8>> = vec![
            // A name claiming 40 bytes in a 5-byte section.
            vec![40, b':', b'a', b'b', b'c'],
            // A well-formed name, then a value length past the end.
            {
                let mut bytes = vec![2, b':', b'a', TYPE_STRING];
                bytes.extend_from_slice(&999u16.to_be_bytes());
                bytes.extend_from_slice(b"short");
                bytes
            },
            // A zero-length name, which the spec forbids.
            vec![0, TYPE_STRING, 0, 0],
            // A name and type byte, then nothing where the value belongs.
            vec![2, b':', b'a', TYPE_STRING],
            // An unknown value type, whose width cannot be skipped.
            vec![2, b':', b'a', 200, 0, 0],
        ];
        for headers in cases {
            let wire = frame(&headers, b"payload");
            let error = EventStreamDecoder::default()
                .push(&wire)
                .expect_err("a malformed headers section must be refused");
            assert!(matches!(error, EventStreamError::Headers(_)), "{error}");
        }
    }

    #[test]
    fn non_string_header_types_are_skipped_by_width_not_misread() {
        // Bedrock only sends strings, but the grammar allows nine other types
        // and their widths must be traversed correctly or every header after
        // one of them is garbage. Each fixed-width type is followed here by a
        // string whose successful decoding proves the skip landed exactly right.
        let widths: [(u8, usize); 8] = [
            (TYPE_BOOL_TRUE, 0),
            (TYPE_BOOL_FALSE, 0),
            (TYPE_BYTE, 1),
            (TYPE_SHORT, 2),
            (TYPE_INTEGER, 4),
            (TYPE_LONG, 8),
            (TYPE_TIMESTAMP, 8),
            (TYPE_UUID, 16),
        ];
        for (kind, width) in widths {
            let mut headers = vec![2, b':', b'x', kind];
            headers.extend(std::iter::repeat_n(0u8, width));
            headers.extend_from_slice(&string_header(":message-type", "event"));
            let messages = EventStreamDecoder::default()
                .push(&frame(&headers, b"{}"))
                .unwrap_or_else(|error| panic!("type {kind} should skip cleanly: {error}"));
            assert_eq!(
                messages[0].header(":message-type"),
                Some("event"),
                "type {kind} skipped the wrong number of bytes"
            );
            assert_eq!(
                messages[0].header(":x"),
                None,
                "type {kind} is not a string"
            );
        }

        // A byte array is length-prefixed like a string but must not be
        // retained as one.
        let mut headers = vec![2, b':', b'x', TYPE_BYTE_ARRAY];
        headers.extend_from_slice(&3u16.to_be_bytes());
        headers.extend_from_slice(&[1, 2, 3]);
        headers.extend_from_slice(&string_header(":message-type", "event"));
        let messages = EventStreamDecoder::default()
            .push(&frame(&headers, b"{}"))
            .expect("a byte-array header skips cleanly");
        assert_eq!(messages[0].header(":message-type"), Some("event"));
        assert_eq!(messages[0].header(":x"), None);
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_decoder() {
        // Everything above tests corruption of a VALID frame. This tests bytes
        // that were never a frame — the case where a proxy, a captive portal,
        // or an error page arrives on a stream the wire expects to be binary.
        // The contract is no panic; an error is a fine outcome.
        let mut state = 0x5eed_2026_0820_u64;
        let mut next = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_f491_4f6c_dd1d)
        };
        for _ in 0..4_000 {
            let length = 1 + usize::try_from(next() % 512).unwrap_or(1);
            let noise: Vec<u8> = (0..length)
                .map(|_| u8::try_from(next() % 256).unwrap_or(0))
                .collect();
            let mut decoder = EventStreamDecoder::default();
            let _ = decoder.push(&noise);
        }

        // Including bytes that begin with a VALID prelude and then degenerate,
        // which random noise essentially never produces on its own.
        for tail_length in [0usize, 1, 7, 64] {
            let mut wire = Vec::new();
            let headers = string_header(":message-type", "event");
            wire.extend_from_slice(&frame(&headers, b"{}"));
            wire.extend(std::iter::repeat_n(0xffu8, tail_length));
            let mut decoder = EventStreamDecoder::default();
            let _ = decoder.push(&wire);
        }
    }
}

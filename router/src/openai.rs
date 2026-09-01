use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use crate::config::RequestNeeds;
use crate::provider::ToolSpec;
use crate::provider::{
    ChatMessage, ChatResponse, ContentPart, ModelRates, RateSchedule, StopReason, TokenUsage,
    ToolCall,
};
use chrono::Utc;
use rust_decimal::{Decimal, prelude::FromPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

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

/// models.dev's spelling for image input — the vocabulary `tiers.toml`
/// declares, `/v1/models` publishes, and `admin catalog-drift` reconciles.
pub const IMAGE_MODALITY: &str = "image";

/// What ONE image adds to a reservation, in tokens: the largest per-image
/// figure any provider family this router dials PUBLISHES.
///
/// Why a constant and not a measurement: the tokens an image costs are
/// decided by the upstream, from pixel dimensions the router never sees — it
/// forwards a URL or a base64 blob and decodes neither. Settlement is
/// untouched and stays metered actuals; this figure only sizes the hold.
///
/// Why one number and not one per family: the hold is taken before the walk
/// picks a rung, and a walk that falls through to a later rung must still be
/// covered by the hold it already took. A per-family constant would be right
/// for the rung that was expected and wrong for the one that answered.
///
/// The published per-image figures, all read 2026-08-21:
///
/// | Family | Rule | Published max / image |
/// |---|---|---|
/// | Fireworks | measured table, Qwen2.5-VL | **10,549** at 3840×2160 |
/// | Anthropic (Claude 4.7+) | `⌈w/28⌉ × ⌈h/28⌉`, long edge capped at 2576px | 4,784 |
/// | Anthropic (earlier) | same formula, long edge capped at 1568px | 1,568 |
/// | Google Gemini 3 | `media_resolution` allocates a fixed budget | 2,240 (`ultra_high`) |
/// | OpenAI (patch budget, mini/nano/o4-mini) | `budget × multiplier` | ~3,779 |
/// | OpenAI (tile: 4o/4.1/gpt-5) | `base + tile × tiles` | NOT PUBLISHED |
/// | OpenAI (GPT-5.6 at `auto`/`original`) | no resize | **EXPLICITLY UNCAPPED** |
/// | xAI | acknowledged, never quantified | NOT PUBLISHED |
///
/// Sources: docs.fireworks.ai "How many tokens per image";
/// platform.claude.com "Vision" → Resolution and token cost;
/// ai.google.dev "Media resolution"; developers.openai.com
/// "Images and vision" → patch-based / tile-based tokenization.
///
/// **THIS IS A PUBLISHED-FIGURE BOUND, NOT A GUARANTEE, AND THE DIFFERENCE
/// MATTERS.** Three rows above publish no ceiling, and OpenAI documents its
/// newest models as deliberately uncapped at the DEFAULT detail level ("large
/// images can use more input tokens than they did with earlier models"). For
/// a `data:` URI none of that is reachable: the byte bound below already
/// holds the base64 length, which is orders of magnitude above any of these
/// figures. The residual exposure is an `https://` URL — ~60 bytes on the
/// wire, an arbitrarily large image at the upstream — on a family that
/// publishes no cap. That is under-recovery for ZeroRouter, never an
/// overcharge to a customer (settlement is metered actuals, clamped to the
/// hold), and closing it properly needs a product decision: either bound the
/// accepted image dimensions, or refuse URL images on lanes whose family
/// publishes no ceiling.
///
/// NOTE for anyone tempted to lower this to ~1600: that is Anthropic's
/// STANDARD-tier figure. Claude 4.7 and later are high-resolution and cost up
/// to 4,784 — roughly 3x — so a reserve sized at the older number would
/// under-hold by two thirds on the Opus and Sonnet lanes this catalog sells.
pub const MAX_IMAGE_PROMPT_TOKENS: u64 = 10_549;

/// The most cache breakpoints one request may carry — Anthropic's documented
/// maximum, and the number the upstream 400s past.
pub const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Why a `cache_control` was refused. Each variant is a different mistake with
/// a different fix, which is why they are not one string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheControlFault {
    /// Not `{"type": "ephemeral"}` — a missing or unknown `type`, or a value
    /// that is not an object at all.
    Shape,
    /// A `ttl` was named. Refused by name because the 1-hour TTL is a
    /// different price this catalog has not transcribed.
    Ttl,
    /// More breakpoints than the upstream accepts.
    TooMany { count: usize },
}

/// Accept exactly `{"type": "ephemeral"}`.
///
/// Deliberately exact rather than lenient about extra keys. A breakpoint is a
/// billing instruction — it decides which tokens are charged at 1.25x — so an
/// unrecognized key is a client believing something about this request that
/// ZeroRouter is not going to do.
fn validate_cache_control(value: &Value) -> Result<(), CacheControlFault> {
    let Some(object) = value.as_object() else {
        return Err(CacheControlFault::Shape);
    };
    if object.contains_key("ttl") {
        return Err(CacheControlFault::Ttl);
    }
    if object.len() != 1 || object.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return Err(CacheControlFault::Shape);
    }
    Ok(())
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
        self.messages
            .iter()
            .map(|message| {
                let turn = to_provider_message(message);
                if message.extra.contains_key("cache_control") {
                    turn.with_cache_breakpoint()
                } else {
                    turn
                }
            })
            .collect()
    }

    #[must_use]
    pub fn provider_tools(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|tool| ToolSpec {
                name: tool.function.name.clone(),
                description: tool.function.description.clone(),
                parameters: tool.function.parameters.clone(),
                cache_control: tool.extra.contains_key("cache_control"),
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

    /// A `cache_control` this surface cannot place, if the request carries one.
    ///
    /// Two spellings are refused, and both are refused because the router has
    /// nowhere honest to put them rather than because caching is unsupported:
    ///
    /// - **Top level.** `cache_control` beside `model` and `messages` names no
    ///   boundary at all. There is no content block it corresponds to.
    /// - **Inside a content part.** This is the spelling OpenRouter documents,
    ///   so it is the one a migrating client is most likely to send — but
    ///   `ChatMessage` carries structured parts only for IMAGES (see
    ///   `provider::ContentPart`), and a text-only content array is flattened
    ///   back to one string before any wire sees it. A breakpoint on part 2 of
    ///   4 would have to be hoisted to the message and would land at the end of
    ///   the whole turn — a different boundary from the one the client asked
    ///   for, at a different price. Refusing is the honest answer until parts
    ///   can carry a breakpoint of their own.
    ///
    /// Returning the offending PLACEMENT rather than a bool so the refusal can
    /// name it; the string is a fixed label from this function, never request
    /// content.
    #[must_use]
    pub fn unplaceable_cache_control(&self) -> Option<&'static str> {
        if self.extra.contains_key("cache_control") {
            return Some("the top level of the request");
        }
        let in_a_part = self.messages.iter().any(|message| {
            message.content.as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.as_object()
                        .is_some_and(|part| part.contains_key("cache_control"))
                })
            })
        });
        in_a_part.then_some("a message content part")
    }

    /// Every client-placed cache breakpoint, validated, or the reason the
    /// request is refused.
    ///
    /// `Ok(0)` is the overwhelmingly common answer and means the request asked
    /// for nothing — the wires then set their own default breakpoints exactly
    /// as they always have.
    ///
    /// # What a breakpoint may say
    ///
    /// `{"type": "ephemeral"}` and nothing else. Anthropic also defines a
    /// `ttl` of `"1h"`, which is priced at 2x input rather than the 1.25x this
    /// catalog transcribes, so it is refused BY NAME: accepting it and quietly
    /// writing a 5-minute entry would sell an hour of cache the upstream was
    /// never asked for, and accepting it and forwarding it would bill the
    /// customer 1.25x for a write ZeroRouter pays 2x on. Either way the fix is
    /// a transcribed price, not a lenient parser.
    ///
    /// # Why the count is capped here
    ///
    /// Anthropic accepts at most four breakpoints per request and 400s the
    /// fifth. Counting them at the edge turns that into a ZeroRouter 400 with
    /// a number in it, before any reservation is taken — rather than an
    /// upstream refusal after the walk has burned a dispatch, which arrives as
    /// `upstream_rejected_parameters` and costs a debugging session.
    pub fn cache_breakpoints(&self) -> Result<usize, CacheControlFault> {
        let mut count = 0_usize;
        for value in self
            .messages
            .iter()
            .filter_map(|message| message.extra.get("cache_control"))
            .chain(
                self.tools
                    .iter()
                    .filter_map(|tool| tool.extra.get("cache_control")),
            )
        {
            validate_cache_control(value)?;
            count += 1;
        }
        if count > MAX_CACHE_BREAKPOINTS {
            return Err(CacheControlFault::TooMany { count });
        }
        Ok(count)
    }

    #[must_use]
    pub fn contains_unsupported_extensions(&self) -> bool {
        self.extra.keys().any(|key| key != "cache_control")
            || self
                .stream_options
                .as_ref()
                .is_some_and(|options| !options.extra.is_empty())
            || self.messages.iter().any(|message| {
                !content_is_supported(&message.role, &message.content)
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

    /// What this request mechanically needs from whatever serves it (edge
    /// mode, stage 2: `docs/design/edge-mode-local-rung.md`).
    ///
    /// Measurements only — no opinion about which model would answer *better*,
    /// which is the design's B-line. `prompt_bound` is supplied by the caller
    /// rather than recomputed so selection and admission read the same number.
    ///
    /// The modality set was text-or-nothing until the compat surface learned
    /// to accept OpenAI-shape content arrays; this is the seam the old comment
    /// here nominated for that change, and the comparison on the other side
    /// ([`crate::config::ModelMetadata::can_serve`]) was already written
    /// against a set.
    ///
    /// `text` is reported for a string content AND for a text part, because a
    /// text-only array is the same request as the joined string — the wires
    /// flatten it back to one. `image` is reported for any `image_url` part.
    /// Audio and file parts cannot appear: they are refused as unsupported
    /// content by [`Self::contains_unsupported_extensions`], which runs first.
    ///
    /// The vocabulary is models.dev's, because that is what `tiers.toml`
    /// declares and what `admin catalog-drift` reconciles against.
    #[must_use]
    pub fn needs(&self, prompt_bound: u64) -> RequestNeeds {
        let mut modalities = BTreeSet::new();
        for message in &self.messages {
            match &message.content {
                Value::String(_) => {
                    modalities.insert("text".to_owned());
                }
                Value::Array(parts) => {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                modalities.insert("text".to_owned());
                            }
                            Some("image_url") => {
                                modalities.insert("image".to_owned());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        RequestNeeds {
            prompt_bound,
            tools: !self.tools.is_empty(),
            modalities,
        }
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
        // IMAGES. The byte bound above is a bound on TEXT: it reads the
        // serialized length of the content and treats a byte as a token,
        // which is conservative for prose. It is NOT conservative for an
        // image, and the two carriers fail in opposite directions:
        //
        // - A `data:` URI is enormous as bytes (megabytes of base64) and
        //   cheap as tokens (capped — see `MAX_IMAGE_PROMPT_TOKENS`), so the
        //   byte bound already over-holds by orders of magnitude.
        // - An `https://` URL is ~60 bytes and costs the SAME capped
        //   thousands of tokens, because the upstream fetches the image the
        //   router never saw. There the byte bound under-holds by ~80x, and
        //   under-holding is the direction that lets a request be served that
        //   the balance could not cover.
        //
        // So each image adds its worst case ON TOP of its bytes. For a data
        // URI that is a rounding error against a bound that was already huge;
        // for a URL it is the whole hold. Settlement is untouched and still
        // charges metered actuals.
        input_bound = input_bound.saturating_add(
            u64::try_from(image_urls(&self.messages).count())
                .unwrap_or(u64::MAX)
                .saturating_mul(MAX_IMAGE_PROMPT_TOKENS),
        );
        let output = u64::from(max_output_tokens);
        OpenAiUsage {
            prompt_tokens: input_bound,
            completion_tokens: output,
            total_tokens: input_bound.saturating_add(output),
            prompt_tokens_details: None,
        }
    }
}

/// Whether one message's `content` is a shape this compat surface carries.
///
/// A string or null is accepted as it always was. An ARRAY is accepted only
/// when every part is one this router can actually deliver to an upstream,
/// which is narrower than what OpenAI itself defines — deliberately, and in
/// the same spirit as the rest of this predicate: a part ZeroRouter cannot
/// preserve is refused with a 400 rather than accepted and silently dropped.
///
/// The accepted grammar, matched against the official schema
/// (openai/openai-openapi, `ChatCompletionRequestMessageContentPart*`, read
/// 2026-08-21):
///
/// | Part | Where OpenAI allows it | Here |
/// |---|---|---|
/// | `{"type":"text","text":…}` | every role | accepted, every role |
/// | `{"type":"image_url","image_url":{"url":…}}` | `user` only | accepted, `user` only |
/// | `{"type":"input_audio",…}` | `user` only | REFUSED |
/// | `{"type":"file",…}` | `user` only | REFUSED |
/// | `{"type":"refusal",…}` | `assistant` only | REFUSED |
///
/// `image_url` is confined to `user` messages because that is where the
/// official schema puts it — `ChatCompletionRequestSystemMessageContentPart`
/// and `…ToolMessageContentPart` are `text`-only, and the spec says outright
/// "For tool messages, only type `text` is supported". Accepting an image on
/// a system or tool turn would be inventing a shape no SDK emits and no
/// upstream on any of this router's four wires is documented to take.
///
/// `input_audio` and `file` are refused because NO lane in this catalog can
/// carry them: none of the four wires has a mapping, and no upstream behind
/// the chat-completions wire defines an OpenAI-shape file part at all
/// (Fireworks' schema has `text`/`image_url`/`video_url` only and its
/// Document Inlining was withdrawn; AI Studio's compat page documents no file
/// part; xAI's file attachments are `input_file` on `/v1/responses`, not on
/// chat completions). `tiers.toml` was narrowed to match rather than left
/// advertising a `pdf` no lane could accept.
/// Whether a client-supplied `image_url` may be forwarded to an upstream.
///
/// A `data:` URI carrying inline base64 is admitted — the bytes travel inside
/// the request body, so nothing is fetched. Every other form is a URL that some
/// upstream may dereference server-side: a cloud vendor's vision endpoint, or a
/// colocated vision server on the `chat_completions` lane sitting *inside* the
/// operator's network. Forwarding an arbitrary URL there turns this gateway
/// into a request-forgery lever, so a URL is admitted only when it is `https:`
/// AND its host is not an internal address literal — loopback, link-local (the
/// `169.254.169.254` cloud-metadata endpoint lives here), private, or
/// unspecified. `http:`, `file:`, protocol-relative `//host`, and every other
/// scheme are refused. This generalizes the URL refusal the Bedrock adapter
/// already performs so it also covers the lanes that pass URLs through.
///
/// A hostname that *resolves* to an internal address is deliberately out of
/// scope: a forward-only gateway does not perform DNS at admission, and a
/// resolve-then-forward check would still race DNS rebinding. That residual
/// belongs to whatever actually fetches the URL.
fn image_url_is_admissible(url: &str) -> bool {
    // Inline base64 data URI: no fetch, always safe. Classification mirrors
    // `wire::image_part`, which routes exactly this shape to inline bytes.
    if url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .is_some()
    {
        return true;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        // Not an absolute URL — protocol-relative `//host/x` and relative refs
        // land here and are refused.
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    match parsed.host_str() {
        // `host_str` keeps brackets on an IPv6 literal; strip them before the
        // `IpAddr` parse. A value that does not parse as an IP is a domain
        // name, which a forward-only gateway cannot resolve here — allowed.
        Some(host) => match host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            Ok(ip) => ip_literal_is_public(ip),
            Err(_) => true,
        },
        None => false,
    }
}

/// Whether an IP *literal* in an image URL points somewhere public. Internal
/// ranges (loopback, link-local, private, CGNAT, unspecified, and their IPv6
/// equivalents plus IPv4-mapped forms) are refused so a URL cannot name the
/// metadata service or a neighbour on the operator's network.
fn ip_literal_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => {
            // `::ffff:a.b.c.d` reaches the same v4 host; re-check the embedding.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ipv4_is_public(mapped);
            }
            let segments = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (segments[0] & 0xfe00) == 0xfc00  // fc00::/7  unique-local
                || (segments[0] & 0xffc0) == 0xfe80) // fe80::/10 link-local
        }
    }
}

fn ipv4_is_public(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()      // 169.254.0.0/16 — cloud metadata
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || a == 0                   // 0.0.0.0/8 "this network"
        || (a == 100 && (b & 0xc0) == 64)) // 100.64.0.0/10 carrier-grade NAT
}

fn content_is_supported(role: &str, content: &Value) -> bool {
    match content {
        Value::String(_) | Value::Null => true,
        Value::Array(parts) => parts.iter().all(|part| part_is_supported(role, part)),
        _ => false,
    }
}

fn part_is_supported(role: &str, part: &Value) -> bool {
    let Some(part) = part.as_object() else {
        return false;
    };
    match part.get("type").and_then(Value::as_str) {
        // A text part carries `type` and `text` and nothing else this surface
        // can preserve — `prompt_cache_breakpoint` is handled by the
        // cache-control predicate, which runs first and answers separately.
        Some("text") => {
            part.get("text").is_some_and(Value::is_string)
                && part
                    .keys()
                    .all(|key| matches!(key.as_str(), "type" | "text"))
        }
        Some("image_url") if role == "user" => {
            part.get("image_url")
                .and_then(Value::as_object)
                .is_some_and(|image| {
                    image
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| image_url_is_admissible(url.trim()))
                        && image.keys().all(|key| key == "url")
                })
                && part
                    .keys()
                    .all(|key| matches!(key.as_str(), "type" | "image_url"))
        }
        _ => false,
    }
}

/// Every image part in the request, as the `url` string the caller supplied.
///
/// Only `user` turns are consulted, matching [`content_is_supported`].
fn image_urls(messages: &[OpenAiMessage]) -> impl Iterator<Item = &str> {
    messages
        .iter()
        .filter(|message| message.role == "user")
        .filter_map(|message| message.content.as_array())
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
        .filter_map(|part| {
            part.get("image_url")
                .and_then(|image| image.get("url"))
                .and_then(Value::as_str)
        })
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
        // Every remaining role is carried as `user` (an unrecognized role was
        // always mapped here), and this is the ONE branch that can carry an
        // image: `content_is_supported` refuses an `image_url` part on any
        // other role before a request gets this far.
        _ => match content_to_parts(&message.content) {
            Some(parts) => ChatMessage::user_parts(parts),
            None => ChatMessage::user(content_to_text(&message.content)),
        },
    }
}

/// The structured content of a user turn, or `None` when the turn is plain
/// text and the `content` string says everything.
///
/// `None` is returned for a text-only array too, not just for a string: a
/// text-only array IS the joined string (`needs` says so, and every
/// byte-stability pin in the wires was written against the string), so
/// promoting it to structured parts would change bytes on the wire for a
/// request whose meaning did not change. Only an actual image earns the
/// structured path.
fn content_to_parts(content: &Value) -> Option<Vec<ContentPart>> {
    let Value::Array(raw) = content else {
        return None;
    };
    let mut parts = Vec::with_capacity(raw.len());
    let mut carries_image = false;
    for part in raw {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    parts.push(ContentPart::Text(text.to_owned()));
                }
            }
            Some("image_url") => {
                if let Some(url) = part
                    .get("image_url")
                    .and_then(|image| image.get("url"))
                    .and_then(Value::as_str)
                {
                    carries_image = true;
                    parts.push(ContentPart::Image(url.to_owned()));
                }
            }
            _ => {}
        }
    }
    carries_image.then_some(parts)
}

/// Flatten a content value to TEXT. Image parts contribute nothing — they are
/// carried structurally by [`content_to_parts`], never spelled into this
/// string. That is the whole of the fix for the marker ambiguity: there is no
/// longer any byte sequence a wire treats as anything but the customer's words.
fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part.get("text").and_then(Value::as_str).map(str::to_owned),
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

/// One `/v1/models` row.
///
/// Beyond `pricing`, the row carries what the model can take and produce. Those
/// four fields are `skip_serializing_if = "Option::is_none"` and that is a
/// contract, not a size optimisation: an absent key means *unknown*, and a
/// consumer must be able to tell it from a small value. Emitting `null`, or
/// worse a plausible default, would erase the difference. ZeroClaw's
/// `ModelInfo.context_window` is an `Option` for the same reason.
///
/// The names follow OpenRouter, the shape `pricing` already commits to
/// (see [`ModelPricing`]), because that is the vocabulary the OpenAI-compatible
/// clients consuming this endpoint already read. That costs one rename against
/// `config/tiers.toml`, which spells the same field models.dev's way:
///
/// | `tiers.toml`       | wire               |
/// |--------------------|--------------------|
/// | `context_window`   | `context_length`   |
/// | `max_output_tokens`| `max_output_tokens`|
/// | `input_modalities` | `input_modalities` |
/// | `tool_call`        | `tool_call`        |
///
/// Only the window is respelled, and only because a consumer already reads it
/// by that name: ZeroClaw's `fetch_openai_compatible_context_window`
/// (`zeroclaw-providers/src/lib.rs`) looks for `context_length` first and
/// `context_window` second, so `context_length` is the spelling that works
/// without asking anyone to change. The remaining three have no OpenRouter
/// top-level equivalent — OpenRouter nests them under `top_provider`,
/// `architecture` and `supported_parameters`, three objects that describe a
/// model marketplace ZeroRouter is not — so they stay flat and keep the file's
/// names.
#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
    pub pricing: ModelPricing,
    /// Maximum input window in tokens. Absent means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// Maximum tokens generated in one response. Absent means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Accepted input modalities (`text`, `image`, `pdf`, `audio`). Absent
    /// means unknown — never "text only".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    /// Native tool-calling support. Absent means unknown, which is not the
    /// same claim as `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
    /// What the upstream serving this model does with the request afterwards.
    ///
    /// **The one field on this object with no `skip_serializing_if`**, and the
    /// exception is the point. Every other field here follows "absent means
    /// unknown", which is the right contract for a capability nobody has
    /// described. It is the wrong contract for this: a customer reading a row
    /// with no retention key would have to guess, and the guess a zero-retention
    /// brand invites is the favourable one. So the catalog either states a
    /// posture for every row or does not load
    /// (`TierConfigError::UnlabelledLane`).
    ///
    /// Additive all the same — a new object-valued key that no existing
    /// consumer reads. ZeroClaw's `ModelPricing`/model structs carry no
    /// `deny_unknown_fields`, and serde ignores unknown fields by default, so an
    /// older client sees exactly what it saw before.
    pub retention: ModelRetention,
}

/// The published half of a [`crate::config::RetentionPin`].
///
/// `source_url` and `source_sha256` stay out of the wire deliberately. They are
/// ZeroRouter's *verification trail* — the page a human read and what it said
/// that day — and the label is ZeroRouter's own claim, standing on its own. A
/// published link would invite a customer to check the label against a page
/// that may have moved since `verified`, which is precisely the discrepancy
/// `admin retention-drift` exists to route to a human first.
#[derive(Debug, Serialize)]
pub struct ModelRetention {
    pub posture: crate::config::RetentionPosture,
    pub description: String,
    pub verified: String,
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
    /// What this model costs past a prompt-size threshold, when it reprices.
    ///
    /// **A price a customer cannot see is a price they cannot check**, and
    /// four of the ten models this catalog lists reprice at 2x. Publishing
    /// only the base rate meant `/v1/models` quoted half the real price on
    /// exactly the requests where the difference is largest — indefensible for
    /// a gateway whose whole claim is honest pass-through.
    ///
    /// The shape is OpenRouter's `pricing.overrides[]`, keyed on the same
    /// `min_prompt_tokens` field name this catalog uses, so a client that
    /// already understands OpenRouter understands this without being told.
    ///
    /// Purely additive: `skip_serializing_if` means a model that charges one
    /// price at every size serializes byte-for-byte as it did before this
    /// field existed. Safe for the known consumer — ZeroClaw's `ModelPricing`
    /// carries no `deny_unknown_fields` and its normalizer reads three named
    /// fields — and unknown fields are ignored by serde's default, so an older
    /// client sees exactly what it saw before.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<PricingOverride>,
}

/// One repricing band on the wire: the prompt size at which it starts, and the
/// per-single-token rates that apply from there.
///
/// `min_prompt_tokens` is a minimum — a request measuring exactly this many
/// prompt tokens is billed here, matching
/// [`crate::provider::RateSchedule::at_prompt_tokens`], which is what actually
/// charges. Rates are absolute replacements, not deltas: past the threshold the
/// whole request bills here, input and output alike.
#[derive(Debug, Serialize)]
pub struct PricingOverride {
    pub min_prompt_tokens: u64,
    pub prompt: String,
    pub completion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_read: Option<String>,
}

impl ModelPricing {
    /// Convert a per-1M-token sell SCHEDULE (`config/tiers.toml`) into
    /// OpenRouter's per-single-token decimal-string convention. Division by
    /// `1_000_000` happens in `Decimal`, never `f64`, and the result is
    /// trailing-zero-normalized before rendering, so the wire value a
    /// customer reads never carries a binary-float artifact.
    ///
    /// The base table fills the top-level fields and each conditional band
    /// becomes one [`PricingOverride`], in the order the catalog declares them
    /// (validated ascending). A flat schedule produces an empty `overrides`,
    /// which does not serialize at all.
    #[must_use]
    pub fn from_sell_rates(schedule: &RateSchedule) -> Self {
        let base = schedule.base();
        Self {
            prompt: per_token_price(base.input_per_mtok.unwrap_or(0.0)),
            completion: per_token_price(base.output_per_mtok.unwrap_or(0.0)),
            input_cache_read: base.cached_input_per_mtok.map(per_token_price),
            overrides: schedule
                .conditional()
                .iter()
                .map(|conditional| PricingOverride {
                    min_prompt_tokens: conditional.min_prompt_tokens,
                    prompt: per_token_price(conditional.rates.input_per_mtok.unwrap_or(0.0)),
                    completion: per_token_price(conditional.rates.output_per_mtok.unwrap_or(0.0)),
                    input_cache_read: conditional.rates.cached_input_per_mtok.map(per_token_price),
                })
                .collect(),
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
        // ZERO-RETENTION LANES FIRST, then alphabetical within each posture.
        //
        // The catalog's order is a statement, not a convenience: a gateway whose
        // brand is zero retention puts the lanes that keep that promise at the
        // top, and the ones that do not below them. Sorting here rather than in
        // `model_listing` keeps `BTreeMap`'s by-id order as the catalog's
        // internal shape — every other caller still gets a stable alphabetical
        // map — and makes this the single place the published order is decided.
        //
        // Ordering is by `ordering_rank`, never by the enum's derived order; see
        // `RetentionPosture::ordering_rank` for why that distinction is load
        // bearing. `then_with` on the id keeps the result total and stable, so
        // two lanes of the same posture read alphabetically exactly as the whole
        // catalog did before this existed.
        let mut rows: Vec<(String, crate::config::ModelListing)> = listing.into_iter().collect();
        rows.sort_by(|(left_id, left), (right_id, right)| {
            left.retention
                .posture
                .ordering_rank()
                .cmp(&right.retention.posture.ordering_rank())
                .then_with(|| left_id.cmp(right_id))
        });
        Self {
            object: "list",
            data: rows
                .into_iter()
                .map(|(id, row)| ModelObject {
                    id,
                    object: "model",
                    created: 0,
                    owned_by: row.owned_by,
                    pricing: ModelPricing::from_sell_rates(&row.sell_rates),
                    context_length: row.metadata.context_window,
                    max_output_tokens: row.metadata.max_output_tokens,
                    input_modalities: row.metadata.input_modalities,
                    tool_call: row.metadata.tool_call,
                    retention: ModelRetention {
                        posture: row.retention.posture,
                        description: row.retention.description,
                        verified: row.retention.verified,
                    },
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
    /// ZeroRouter's response-side namespace, attached only when the request
    /// engaged the priority knob through any carrier — `skip_serializing_if`
    /// keeps every legacy response byte-stable (response strictness is not
    /// part of ZR's contract; request strictness is).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zerorouter: Option<ZeroRouterResponseMetadata>,
}

/// The `zerorouter` response block (design doc: "Response metadata"),
/// stage-3a shape: the resolved priority, the walk story, and the
/// declared-validator verdict. The designed block's `estimate`, `limited`,
/// and `savings` fields arrive with the stages that give them meaning (3b,
/// 5a, 5c) — absent fields rather than null ones, so each field's appearance
/// is itself the capability signal.
#[derive(Clone, Debug, Serialize)]
pub struct ZeroRouterResponseMetadata {
    pub priority: Priority,
    /// Output-token guidance for this request's segment (stage 3b) —
    /// explicitly guidance, never a quote: `learned` percentiles once the
    /// segment's estimator cell is warm, else the `cold` byte-bound answer,
    /// which is the request's own `max_tokens`. The basis names which one
    /// the customer is reading.
    pub estimate: ZeroRouterEstimate,
    /// Every walk position in order, skips included: the attempts array is
    /// the customer-visible audit trail for "why did the walk look like
    /// this", mirroring the `request_attempts` rows the same walk settled.
    pub attempts: Vec<ZeroRouterAttempt>,
    /// Governing declared-validator verdict. Always `null` in 3a — no
    /// validator can be declared yet, and the design defines `null` as "no
    /// validator was declared", which is exactly true.
    pub validated: Option<bool>,
    /// Whether this request dispatched on the CUSTOMER's own provider
    /// credential (bring-your-own-key).
    ///
    /// This field is a disclosure, not a statistic, and it is the reason the
    /// block is emitted at all for a BYOK request that never touched the
    /// priority knob. A BYOK request is governed by the customer's own
    /// agreement with the provider: ZeroRouter's catalog retention labels
    /// describe ZeroRouter's contract and do not apply to that traffic, and
    /// the house's per-response retention attestation is deliberately not
    /// asserted on it (`crate::providers::create_provider`). Something has to
    /// say so on the response itself rather than only in the docs, and this is
    /// it.
    ///
    /// `skip_serializing_if` keeps every non-BYOK response byte-identical,
    /// including a knob-engaged one — the same rule the rest of this block
    /// follows: an absent field is the capability signal.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub byok: bool,
    /// Whether this request was served by the opted-in HOUSE fallback after the
    /// customer's own credential failed (migration 0028).
    ///
    /// # Why a second boolean and not `byok: "fallback"`
    ///
    /// Because `byok` already answers a question that still has a correct
    /// answer here, and that answer is `false`. A fallback attempt dispatches
    /// on ZeroRouter's credential, under ZeroRouter's agreement with the
    /// provider, with ZeroRouter's per-response retention attestation asserted
    /// on it — every consequence `byok: true` exists to warn about is untrue of
    /// it. Widening `byok` into a three-valued string would make a field that
    /// every existing client reads as a boolean start carrying a value they
    /// would coerce to truthy, and they would conclude the exact opposite of
    /// what happened: that ZeroRouter made no retention claim about traffic on
    /// which it did.
    ///
    /// So `byok` keeps its meaning and its type, and this field carries the new
    /// fact: your key was tried first and did not answer, which is why this
    /// request is billed at the full catalog price rather than at 5% — and why
    /// it did not draw on your monthly allowance.
    ///
    /// `skip_serializing_if` on the same rule as its sibling: absent on every
    /// response that did not fall back, so nothing else changes shape.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub byok_fallback: bool,
}

/// One walk position as the customer sees it.
#[derive(Clone, Debug, Serialize)]
pub struct ZeroRouterAttempt {
    pub candidate: String,
    pub outcome: String,
    pub latency_ms: i32,
}

/// The `estimate` member of the response block (design doc: "Response
/// metadata"): the segment's expected output size and the provenance of
/// that expectation. `quote` exists in the design's ladder but no code path
/// can produce it until the gated quote stage ships.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ZeroRouterEstimate {
    pub output_tokens_p50: u64,
    pub output_tokens_p90: u64,
    pub basis: EstimateBasis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimateBasis {
    Cold,
    Learned,
}

impl ZeroRouterEstimate {
    /// The cold answer: no warm cell, so the only honest output bound is the
    /// request's own `max_tokens` — p50 and p90 alike, which is itself the
    /// signal that the segment is unmeasured.
    #[must_use]
    pub fn cold(max_output_tokens: u32) -> Self {
        Self {
            output_tokens_p50: max_output_tokens.into(),
            output_tokens_p90: max_output_tokens.into(),
            basis: EstimateBasis::Cold,
        }
    }
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

/// One request's metered usage, in the shape the customer is answered in.
///
/// `Serialize` is written by hand rather than derived, and the reason is that
/// two audiences read this object and they want different things:
///
/// - The OpenAI-compatible half must stay EXACTLY OpenAI's. `prompt_tokens` is
///   the whole prompt, and `prompt_tokens_details.cached_tokens` is the cached
///   subset — nothing else belongs in that object, because a client parsing it
///   against the vendor's schema must not meet a key the vendor never defined.
/// - ZeroRouter's own dimension — cache-WRITE tokens, which no OpenAI response
///   carries — goes under a namespaced `zerorouter` key, the same way the
///   response body already namespaces its routing metadata.
///
/// A derive cannot express that split, because the write count has to live in
/// the same struct the biller reads while being serialized somewhere else
/// entirely. Both extension objects are omitted when they have nothing to say,
/// so a response from a lane with no cache activity is byte-identical to what
/// it was before this dimension existed.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub prompt_tokens_details: Option<PromptTokenDetails>,
}

impl Serialize for OpenAiUsage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        // Each extension appears only when it has something to report, and the
        // two conditions are INDEPENDENT rather than both keyed on the details
        // object being present. That is what keeps the OpenAI half unchanged: a
        // first-turn Anthropic request writes cache and reads none, and
        // emitting `prompt_tokens_details: {cached_tokens: 0}` for it would put
        // a new object into a response that never carried one.
        let cached = self.cached_input_tokens();
        let written = self.cache_write_tokens();
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("prompt_tokens", &self.prompt_tokens)?;
        map.serialize_entry("completion_tokens", &self.completion_tokens)?;
        map.serialize_entry("total_tokens", &self.total_tokens)?;
        if cached > 0 {
            map.serialize_entry("prompt_tokens_details", &json!({ "cached_tokens": cached }))?;
        }
        if written > 0 {
            map.serialize_entry("zerorouter", &json!({ "cache_write_tokens": written }))?;
        }
        map.end()
    }
}

/// The cache split of one prompt, as ZeroRouter meters it.
///
/// Both counts are subsets of `OpenAiUsage::prompt_tokens` and are disjoint
/// from each other — a token is read from the cache, written to it, or neither.
/// `cache_write_tokens` is deliberately NOT part of the serialized
/// `prompt_tokens_details` object; see [`OpenAiUsage`]'s hand-written
/// `Serialize` for where it surfaces instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptTokenDetails {
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
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
        // Clamped in sequence, cached first: the two are disjoint subsets of
        // the prompt, so what a write may claim is bounded by what is left
        // after the reads. An upstream whose three numbers do not add up
        // therefore loses the excess from the DEARER bucket rather than
        // inflating the prompt, and the fresh remainder can never go negative.
        let cached = usage.cached_input_tokens.unwrap_or(0).min(input);
        let written = usage
            .cache_write_input_tokens
            .unwrap_or(0)
            .min(input - cached);
        Some(Self {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: input.saturating_add(output),
            prompt_tokens_details: (cached > 0 || written > 0).then_some(PromptTokenDetails {
                cached_tokens: cached,
                cache_write_tokens: written,
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

    /// Prompt tokens this request WROTE into the upstream's cache — billed at
    /// [`ModelRates::cache_write_per_mtok`] where a lane prices the dimension.
    #[must_use]
    pub fn cache_write_tokens(self) -> u64 {
        self.prompt_tokens_details
            .map_or(0, |details| details.cache_write_tokens)
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
        zerorouter: Option<ZeroRouterResponseMetadata>,
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
            zerorouter,
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

/// Stamped on a row whose finish reason is the UPSTREAM's own word.
///
/// The spelling is `"upstream"`, not `"provider"`: migration 0004 created
/// `finish_reason_source` with `CHECK (... IN ('synthetic', 'upstream'))` and
/// the comment *"'synthetic' now; 'upstream' once StreamEvent/ChatResponse
/// carry the real stop reason"*. This is that change, so it uses the token the
/// schema reserved for it. Any other value would fail the constraint and abort
/// the settle transaction.
pub const FINISH_REASON_UPSTREAM: &str = "upstream";
/// Stamped on a row whose finish reason this router inferred from token
/// arithmetic, because the upstream reported none it could map.
pub const FINISH_REASON_SYNTHETIC: &str = "synthetic";

/// One attempt's finish reason and where it came from.
///
/// # The consumption rule
///
/// A real reason wins; absence falls back to the unchanged synthesis. The two
/// stay distinguishable through [`Self::source`], which is what
/// `usage_events.finish_reason_source` records — a column hardcoded
/// `"synthetic"` before any wire could report a real one.
///
/// # Where provenance is NOT recorded
///
/// `request_attempts.finish_reason` (migration 0004) is a bare TEXT column
/// with no `finish_reason_source` beside it. Attempt rows therefore now carry
/// real and synthesized reasons MIXED, with nothing on the row to tell them
/// apart. That is tolerable only because of how the column is populated: a
/// reason is written on the SERVED attempt alone (every other `build_attempt`
/// call passes `None`), and the served attempt's request has a `usage_events`
/// row carrying both the same reason and its source. So provenance is
/// recoverable by joining `request_id` — and only for the served attempt.
///
/// Do not read `request_attempts.finish_reason` as ground truth on its own,
/// and never train on it without that join. The column's own migration
/// comment still calls it "Synthesized"; correcting that text belongs to a
/// future migration header, since an applied migration is checksummed and must
/// not be edited.
///
/// # Divergence table
///
/// Where the real reason and the synthesis disagree, this is the complete set
/// of consequences. `SYNTH` is [`finish_reason`]: `tool_calls` if any tool call
/// was emitted, else `length` if output reached the requested ceiling, else
/// `stop`.
///
/// | # | real | synth | why they differ | `shape_ok` | served? | billed? |
/// |---|------|-------|-----------------|-----------|---------|---------|
/// | 1 | `stop` | `length` | output landed exactly on the ceiling but the model ended on its own | `false` → `true` | unchanged | unchanged |
/// | 2 | `stop` | `tool_calls` | upstream reported a plain stop despite emitting tool calls (nonconforming) | `true` → `true` | unchanged | unchanged |
/// | 3 | `length` | `stop` | upstream clipped on ITS own ceiling, below the one we asked for | `true` → `false` | unchanged | unchanged |
/// | 4 | `length` | `tool_calls` | tool calls emitted AND the output was clipped | `true` → `false` | unchanged | unchanged |
/// | 5 | `tool_calls` | `stop` | upstream reports tool calls the router parsed none of | `true` → `true` | unchanged | unchanged |
/// | 6 | `tool_calls` | `length` | tool-call output reached the ceiling | `false` → `true` | unchanged | unchanged |
/// | 7 | `content_filter` | `stop` | safety layer withheld output — previously unobservable | `true` → `true` | unchanged | unchanged |
/// | 8 | `content_filter` | `length` | safety layer withheld output that also hit the ceiling | `false` → `true` | unchanged | unchanged |
///
/// **Every row's served and billed columns read "unchanged", and that is a
/// verified property of this crate rather than an aspiration.** Nothing
/// branches on a finish reason: the walk's only content-driven retry is
/// `retry::is_empty_completion` (text/tool-calls/reasoning, never the reason),
/// and [`shape_ok`]'s verdict is written to a telemetry column that no
/// non-test code reads. So the change this type introduces is confined to
/// three persisted values — `finish_reason`, `finish_reason_source`, and the
/// `shape_ok` label — plus the response body, which deliberately continues to
/// report the SYNTHESIZED value (see the note on `reason` below).
///
/// Rows 7 and 8 expose a pre-existing wrinkle this type does NOT change:
/// `shape_ok` only rejects `length`, so a `content_filter` completion labels
/// as a good shape. That is the existing predicate applied to a reason the
/// router could not previously see, not a new judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptFinishReason {
    /// The reason to persist and to hand [`shape_ok`].
    ///
    /// NOT what the response body reports. The body keeps its own synthesized
    /// value because changing it changes what customers' own agent loops see
    /// and therefore how many requests they issue — a product decision that is
    /// deliberately not bundled into plumbing the ledger.
    pub reason: &'static str,
    /// [`FINISH_REASON_UPSTREAM`] or [`FINISH_REASON_SYNTHETIC`] — the only
    /// two values `usage_events.finish_reason_source` accepts.
    pub source: &'static str,
}

/// Parse a persisted or replayed `finish_reason_source` back.
///
/// Anything outside the two tokens migration 0004's CHECK permits is `None`,
/// so a malformed settlement payload can never carry a value into the INSERT
/// that would abort the settle transaction.
#[must_use]
pub fn finish_reason_source_from_keyword(source: &str) -> Option<&'static str> {
    match source {
        FINISH_REASON_UPSTREAM => Some(FINISH_REASON_UPSTREAM),
        FINISH_REASON_SYNTHETIC => Some(FINISH_REASON_SYNTHETIC),
        _ => None,
    }
}

impl AttemptFinishReason {
    /// Apply the consumption rule: the upstream's own reason if it gave one,
    /// otherwise the synthesis, with the provenance recorded either way.
    #[must_use]
    pub fn resolve(
        reported: Option<StopReason>,
        has_tool_calls: bool,
        usage: OpenAiUsage,
        max_tokens: Option<u32>,
    ) -> Self {
        reported.map_or_else(
            || Self {
                reason: finish_reason(has_tool_calls, usage, max_tokens),
                source: FINISH_REASON_SYNTHETIC,
            },
            |reason| Self {
                reason: reason.as_str(),
                source: FINISH_REASON_UPSTREAM,
            },
        )
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
/// non-empty, every tool-call `arguments` parses as JSON, and the finish
/// reason is not length-truncation. Label-only — it never changes routing, and
/// it labels 100% of served traffic for the success estimator.
///
/// "Label-only" is load-bearing and literally true, not aspirational: every
/// caller passes the result straight to `persist_usage` and no branch in the
/// crate reads it back. The walk's only content-driven retry is
/// `retry::is_empty_completion`, which inspects the response text, tool calls,
/// and reasoning — never a finish reason and never this label. That is what
/// makes feeding it a REAL finish reason (see [`AttemptFinishReason`]) a
/// telemetry change rather than a billing one.
///
/// The reason it receives is now the upstream's own where one was reported and
/// the synthesis otherwise; the predicate is unchanged, so a `content_filter`
/// completion still labels TRUE — only `length` is rejected.
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
pub fn stream_usage_json(
    metadata: &StreamMetadata,
    usage: OpenAiUsage,
    zerorouter: Option<&ZeroRouterResponseMetadata>,
) -> String {
    let mut chunk = json!({
        "id": metadata.request_id,
        "object": "chat.completion.chunk",
        "created": metadata.created,
        "model": metadata.requested_model,
        "choices": [],
        "usage": usage,
    });
    // SSE headers left before the walk resolved, so streaming metadata is
    // in-band only: the same block a buffered response carries rides the
    // final usage chunk, for exactly the clients that asked to see usage.
    if let Some(block) = zerorouter {
        chunk["zerorouter"] = serde_json::to_value(block)
            .expect("a zerorouter block of strings and integers serializes");
    }
    chunk.to_string()
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
/// A dimension the rates leave unset prices at zero (cached input and cache
/// writes both fall back to the uncached input rate first). That is
/// long-standing behavior and unchanged.
///
/// # The prompt is split three ways, not two
///
/// `prompt_tokens` is the whole prompt. Out of it come the CACHED tokens (read
/// back from the upstream's cache) and the WRITTEN tokens (stored into it under
/// a breakpoint); what is left is fresh. The three are disjoint and are priced
/// at three different rates, which on an Anthropic lane run roughly 0.1x, 1.25x
/// and 1x of each other.
///
/// The split is taken in that order — cached, then written out of the
/// remainder, then fresh — so the arithmetic is total for any numbers an
/// upstream reports, including ones that do not add up. `OpenAiUsage`'s
/// constructor has already applied the same clamp, so this is a second line
/// rather than the only one, and it is here because `usage_cost` must be
/// correct for a usage assembled anywhere, including a replayed settlement
/// intent.
///
/// A usage carrying no `prompt_tokens_details` splits into fresh alone and
/// prices exactly as it did before either cache dimension existed — which is
/// every reservation, and every request on a wire that reports no cache
/// numbers.
#[must_use]
pub fn usage_cost(rates: ModelRates, usage: OpenAiUsage) -> Option<Decimal> {
    let input_rate = billable_rate(rates.input_per_mtok.unwrap_or(0.0))?;
    let output_rate = billable_rate(rates.output_per_mtok.unwrap_or(0.0))?;
    let cached_rate = billable_rate(
        rates
            .cached_input_per_mtok
            .unwrap_or(rates.input_per_mtok.unwrap_or(0.0)),
    )?;
    let write_rate = billable_rate(
        rates
            .cache_write_per_mtok
            .unwrap_or(rates.input_per_mtok.unwrap_or(0.0)),
    )?;
    let million = Decimal::from(1_000_000_u64);
    let cached = usage.cached_input_tokens().min(usage.prompt_tokens);
    let written = usage
        .cache_write_tokens()
        .min(usage.prompt_tokens.saturating_sub(cached));
    let uncached = usage
        .prompt_tokens
        .saturating_sub(cached)
        .saturating_sub(written);

    // Checked throughout: `Decimal`'s operators are the same routines with a
    // panic bolted on where these return `None`, so the arithmetic result is
    // bit-identical and nothing a customer is charged changes.
    Decimal::from(uncached)
        .checked_mul(input_rate)?
        .checked_add(Decimal::from(cached).checked_mul(cached_rate)?)?
        .checked_add(Decimal::from(written).checked_mul(write_rate)?)?
        .checked_add(Decimal::from(usage.completion_tokens).checked_mul(output_rate)?)?
        .checked_div(million)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `openai/gpt-5.6-luna`'s published schedule.
    fn luna() -> crate::provider::RateSchedule {
        let rates = |input: f64, cached: f64, output: f64| ModelRates {
            cache_write_per_mtok: None,
            input_per_mtok: Some(input),
            cached_input_per_mtok: Some(cached),
            output_per_mtok: Some(output),
        };
        crate::provider::RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![crate::provider::ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: rates(0.4, 0.04, 1.8),
            }],
        )
    }

    #[test]
    fn cached_prompt_tokens_count_toward_the_conditional_threshold() {
        // The question the vendor answers by counting TOTAL prompt tokens, and
        // this codebase answers the same way — not by choice made here, but
        // because `prompt_tokens` already IS the whole prompt. `usage_cost`
        // below splits it into a cached part and an uncached remainder rather
        // than adding them, so a request of 250,000 cached plus 30,000 fresh
        // tokens is a 280,000-token prompt on both sides of the ledger.
        let usage = OpenAiUsage {
            prompt_tokens: 280_000,
            completion_tokens: 1_000,
            total_tokens: 281_000,
            prompt_tokens_details: Some(PromptTokenDetails {
                cache_write_tokens: 0,
                cached_tokens: 250_000,
            }),
        };
        assert_eq!(usage.cached_input_tokens(), 250_000);
        assert!(
            usage.cached_input_tokens() < 272_000,
            "the fixture is only interesting because the cached portion alone is under the \
             boundary: a rule that counted fresh tokens only would pick the base band here"
        );

        let applied = luna().at_prompt_tokens(usage.prompt_tokens);
        assert_eq!(applied.input_per_mtok, Some(0.4));

        // Priced end to end: 30,000 fresh at 0.40 + 250,000 cached at 0.04 +
        // 1,000 output at 1.80, all per million.
        let expected = (Decimal::from(30_000) * Decimal::from_f64(0.40).unwrap()
            + Decimal::from(250_000) * Decimal::from_f64(0.04).unwrap()
            + Decimal::from(1_000) * Decimal::from_f64(1.80).unwrap())
            / Decimal::from(1_000_000);
        assert_eq!(usage_cost(applied, usage), Some(expected));
    }

    #[test]
    fn a_request_under_the_threshold_is_priced_at_the_base_band() {
        let usage = OpenAiUsage {
            prompt_tokens: 271_999,
            completion_tokens: 1_000,
            total_tokens: 272_999,
            prompt_tokens_details: None,
        };
        let applied = luna().at_prompt_tokens(usage.prompt_tokens);
        assert_eq!(applied.input_per_mtok, Some(0.2));
        let expected = (Decimal::from(271_999) * Decimal::from_f64(0.20).unwrap()
            + Decimal::from(1_000) * Decimal::from_f64(1.20).unwrap())
            / Decimal::from(1_000_000);
        assert_eq!(usage_cost(applied, usage), Some(expected));
    }

    #[test]
    fn one_more_prompt_token_across_the_boundary_reprices_the_entire_request() {
        // The step, in money. The two requests differ by a single prompt
        // token and the charge nearly doubles, because the output side
        // reprices too — which is the whole reason a marginal calculation
        // would be wrong here.
        let at = |prompt_tokens: u64| {
            let usage = OpenAiUsage {
                prompt_tokens,
                completion_tokens: 10_000,
                total_tokens: prompt_tokens + 10_000,
                prompt_tokens_details: None,
            };
            usage_cost(luna().at_prompt_tokens(usage.prompt_tokens), usage)
                .expect("luna's rates price")
        };
        let below = at(271_999);
        let above = at(272_000);
        assert!(
            above > below * Decimal::from(19) / Decimal::from(10),
            "crossing the boundary must reprice the whole request, not the marginal token: \
             {below} -> {above}"
        );
    }

    #[test]
    fn all_zero_token_usage_is_rejected_as_unusable() {
        // A provider reporting 0 input + 0 output must not meter as a free
        // success; it routes through the missing-usage path instead.
        let usage = TokenUsage {
            cache_write_input_tokens: None,
            input_tokens: Some(0),
            output_tokens: Some(0),
            cached_input_tokens: None,
        };
        assert!(OpenAiUsage::try_from_provider(Some(&usage)).is_none());
    }

    #[test]
    fn nonzero_token_usage_is_accepted() {
        let usage = TokenUsage {
            cache_write_input_tokens: None,
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
                cache_write_per_mtok: None,
                input_per_mtok: Some(2.0),
                cached_input_per_mtok: Some(0.2),
                output_per_mtok: Some(10.0),
            },
            OpenAiUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 100_000,
                total_tokens: 1_100_000,
                prompt_tokens_details: Some(PromptTokenDetails {
                    cache_write_tokens: 0,
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
                cache_write_per_mtok: None,
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
                    cache_write_per_mtok: None,
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
                cache_write_per_mtok: None,
                input_per_mtok: Some(MAX_RATE_PER_MTOK),
                cached_input_per_mtok: Some(MAX_RATE_PER_MTOK),
                output_per_mtok: Some(MAX_RATE_PER_MTOK),
            },
            OpenAiUsage {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                total_tokens: u64::MAX,
                prompt_tokens_details: Some(PromptTokenDetails {
                    cache_write_tokens: 0,
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

    // -----------------------------------------------------------------------
    // Cache breakpoints: what the request surface accepts, and what it refuses
    // before anything is reserved.
    // -----------------------------------------------------------------------

    fn parse(body: serde_json::Value) -> ChatCompletionRequest {
        serde_json::from_value(body).expect("request should parse")
    }

    #[test]
    fn a_breakpoint_on_a_message_or_a_tool_is_accepted_and_counted() {
        let request = parse(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [
                {"role": "system", "content": "be terse",
                 "cache_control": {"type": "ephemeral"}},
                {"role": "user", "content": "hi"}
            ],
            "tools": [{
                "type": "function",
                "function": {"name": "shell", "description": "run", "parameters": {}},
                "cache_control": {"type": "ephemeral"}
            }]
        }));
        assert_eq!(request.cache_breakpoints(), Ok(2));
        assert_eq!(request.unplaceable_cache_control(), None);
        // And they survive into the provider view the wires read — this is the
        // seam a stripped-breakpoint bug would show up at first.
        let messages = request.provider_messages();
        assert!(messages[0].cache_control, "the system turn keeps its mark");
        assert!(
            !messages[1].cache_control,
            "an unmarked turn stays unmarked"
        );
        assert!(request.provider_tools()[0].cache_control);
    }

    #[test]
    fn a_request_that_asks_for_nothing_reports_no_breakpoints() {
        // The overwhelmingly common answer, and the one that has to keep the
        // wires on their own default placement.
        let request = parse(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(request.cache_breakpoints(), Ok(0));
        assert!(!request.provider_messages()[0].cache_control);
    }

    #[test]
    fn only_the_ephemeral_shape_is_accepted() {
        // A breakpoint is a billing instruction — it decides which tokens are
        // charged at 1.25x — so an unrecognized key is a client believing
        // something about this request that ZeroRouter is not going to do.
        for value in [
            json!({}),
            json!({"type": "persistent"}),
            json!({"type": "ephemeral", "scope": "global"}),
            json!("ephemeral"),
            json!(true),
            json!(null),
        ] {
            let request = parse(json!({
                "model": "anthropic/claude-sonnet-5",
                "messages": [{"role": "user", "content": "hi", "cache_control": value}]
            }));
            assert_eq!(
                request.cache_breakpoints(),
                Err(CacheControlFault::Shape),
                "accepted a cache_control this router does not implement: {value}"
            );
        }
    }

    #[test]
    fn a_ttl_is_refused_by_name_rather_than_quietly_downgraded() {
        // Anthropic's 1-hour TTL is priced at 2x input; this catalog has
        // transcribed only the 1.25x five-minute figure. Forwarding it would
        // bill the customer 1.25x for a write ZeroRouter pays 2x on, and
        // dropping it would sell an hour of cache that was never written —
        // so it is named, and the client is told which.
        for ttl in [json!("1h"), json!("5m"), json!(3600)] {
            let request = parse(json!({
                "model": "anthropic/claude-sonnet-5",
                "messages": [{
                    "role": "user", "content": "hi",
                    "cache_control": {"type": "ephemeral", "ttl": ttl}
                }]
            }));
            assert_eq!(
                request.cache_breakpoints(),
                Err(CacheControlFault::Ttl),
                "a ttl of {ttl} must be refused by name, even the one we do serve"
            );
        }
    }

    #[test]
    fn a_fifth_breakpoint_is_refused_here_rather_than_upstream() {
        // Anthropic 400s the fifth. Counting at the edge turns that into a
        // ZeroRouter 400 with a number in it, before any reservation is taken
        // — rather than an upstream refusal after the walk has burned a
        // dispatch and spent the customer's latency budget.
        let message = |text: &str| json!({"role": "user", "content": text, "cache_control": {"type": "ephemeral"}});
        let four = parse(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [message("a"), message("b"), message("c"), message("d")]
        }));
        assert_eq!(four.cache_breakpoints(), Ok(MAX_CACHE_BREAKPOINTS));

        let five = parse(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [message("a"), message("b"), message("c"), message("d")],
            "tools": [{
                "type": "function",
                "function": {"name": "shell", "description": "run", "parameters": {}},
                "cache_control": {"type": "ephemeral"}
            }]
        }));
        assert_eq!(
            five.cache_breakpoints(),
            Err(CacheControlFault::TooMany { count: 5 }),
            "tools count toward the same budget as messages, because upstream they do"
        );
    }

    #[test]
    fn a_breakpoint_the_router_cannot_place_is_named_rather_than_ignored() {
        // Two placements survive the old blanket refusal, and both are refused
        // because there is nowhere honest to put them rather than because
        // caching is unsupported. The content-part spelling is the one
        // OpenRouter documents, so it is the one a migrating client is most
        // likely to send — which is exactly why the answer names where a
        // breakpoint DOES go instead of just saying no.
        let in_a_part = parse(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hi",
                             "cache_control": {"type": "ephemeral"}}]
            }]
        }));
        assert_eq!(
            in_a_part.unplaceable_cache_control(),
            Some("a message content part")
        );

        let at_the_top = parse(json!({
            "model": "anthropic/claude-sonnet-5",
            "cache_control": {"type": "ephemeral"},
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(
            at_the_top.unplaceable_cache_control(),
            Some("the top level of the request")
        );

        // Neither counts as a breakpoint: the request is refused before the
        // count is ever consulted, and reporting one would let a caller that
        // ignored the refusal think a boundary had been placed.
        assert_eq!(in_a_part.cache_breakpoints(), Ok(0));
        assert_eq!(at_the_top.cache_breakpoints(), Ok(0));
    }

    // -----------------------------------------------------------------------
    // Pricing a three-way prompt split.
    // -----------------------------------------------------------------------

    /// `anthropic/claude-sonnet-5` as the catalog sells it: 2.00 input, 0.20
    /// cached, 2.50 written (1.25x input), 10.00 output.
    fn sonnet() -> ModelRates {
        ModelRates {
            input_per_mtok: Some(2.00),
            cached_input_per_mtok: Some(0.20),
            cache_write_per_mtok: Some(2.50),
            output_per_mtok: Some(10.00),
        }
    }

    fn split(prompt: u64, cached: u64, written: u64, output: u64) -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens: prompt,
            completion_tokens: output,
            total_tokens: prompt + output,
            prompt_tokens_details: Some(PromptTokenDetails {
                cached_tokens: cached,
                cache_write_tokens: written,
            }),
        }
    }

    #[test]
    fn a_prompt_is_priced_in_three_buckets_at_three_rates() {
        // The money, exactly. 10,000 prompt tokens of which 6,000 were read
        // from cache and 3,000 written into it, leaving 1,000 fresh, plus 500
        // output.
        let usage = split(10_000, 6_000, 3_000, 500);
        let expected = (Decimal::from(1_000) * Decimal::from_f64(2.00).unwrap()
            + Decimal::from(6_000) * Decimal::from_f64(0.20).unwrap()
            + Decimal::from(3_000) * Decimal::from_f64(2.50).unwrap()
            + Decimal::from(500) * Decimal::from_f64(10.00).unwrap())
            / Decimal::from(1_000_000);
        assert_eq!(usage_cost(sonnet(), usage), Some(expected));

        // And it is strictly more than the same request would have cost when
        // writes were billed as fresh reads — which is the whole point, and the
        // amount is the 25% premium on the written bucket.
        let as_fresh = ModelRates {
            cache_write_per_mtok: None,
            ..sonnet()
        };
        let premium =
            Decimal::from(3_000) * Decimal::from_f64(0.50).unwrap() / Decimal::from(1_000_000);
        assert_eq!(
            usage_cost(sonnet(), usage).unwrap() - usage_cost(as_fresh, usage).unwrap(),
            premium
        );
    }

    #[test]
    fn a_lane_that_does_not_price_writes_bills_them_as_fresh_input() {
        // Absence is not free. A lane with no transcribed write price charges
        // input for a written token — precisely what it charged before the
        // dimension existed — so the Anthropic wire's own breakpoints cannot
        // give inference away on a lane nobody has priced yet.
        let silent = ModelRates {
            cache_write_per_mtok: None,
            ..sonnet()
        };
        let written = split(10_000, 0, 4_000, 0);
        let all_fresh = split(10_000, 0, 0, 0);
        assert_eq!(
            usage_cost(silent, written),
            usage_cost(silent, all_fresh),
            "with no write rate the three buckets collapse to one"
        );
    }

    #[test]
    fn a_usage_with_no_cache_detail_prices_exactly_as_it_always_did() {
        // The backwards-compatibility guarantee for every reservation and for
        // every wire that reports no cache numbers.
        let plain = OpenAiUsage {
            prompt_tokens: 10_000,
            completion_tokens: 500,
            total_tokens: 10_500,
            prompt_tokens_details: None,
        };
        let expected = (Decimal::from(10_000) * Decimal::from_f64(2.00).unwrap()
            + Decimal::from(500) * Decimal::from_f64(10.00).unwrap())
            / Decimal::from(1_000_000);
        assert_eq!(usage_cost(sonnet(), plain), Some(expected));
    }

    #[test]
    fn buckets_that_do_not_add_up_are_clamped_rather_than_believed() {
        // An upstream whose three numbers overflow the prompt must not be able
        // to inflate a bill. Cached is taken first and written out of what is
        // left, so the excess is lost from the DEARER bucket and the fresh
        // remainder can never go negative.
        let overclaimed = split(1_000, 800, 800, 0);
        let honest = split(1_000, 800, 200, 0);
        assert_eq!(
            usage_cost(sonnet(), overclaimed),
            usage_cost(sonnet(), honest)
        );
        assert_eq!(
            usage_cost(sonnet(), split(1_000, 2_000, 2_000, 0)),
            usage_cost(sonnet(), split(1_000, 1_000, 0, 0)),
            "a prompt that is entirely cache reads is the ceiling, not a negative remainder"
        );
    }

    #[test]
    fn the_wire_reports_writes_under_zerorouters_own_key_and_never_openais() {
        // A client parsing `prompt_tokens_details` against OpenAI's schema must
        // not meet a key the vendor never defined, and a response from a lane
        // with no cache activity must be byte-identical to what it always was.
        let none = serde_json::to_value(OpenAiUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            prompt_tokens_details: None,
        })
        .unwrap();
        assert_eq!(
            none,
            json!({"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12})
        );

        // A first turn writes cache and reads none, and that must NOT
        // manufacture a `prompt_tokens_details` object out of a zero.
        let written_only = serde_json::to_value(split(10, 0, 8, 2)).unwrap();
        assert_eq!(
            written_only,
            json!({
                "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
                "zerorouter": {"cache_write_tokens": 8}
            })
        );

        let both = serde_json::to_value(split(10, 6, 3, 2)).unwrap();
        assert_eq!(
            both,
            json!({
                "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
                "prompt_tokens_details": {"cached_tokens": 6},
                "zerorouter": {"cache_write_tokens": 3}
            })
        );
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

    /// The shape the official OpenAI SDKs emit for a vision request, which
    /// used to be a flat 400 on every lane in this catalog including the ones
    /// whose `/v1/models` row advertised `image`. Both carriers are here
    /// because they behave differently everywhere downstream — the reserve
    /// sizes them differently, and the Bedrock plane takes only one of them.
    #[test]
    fn an_openai_shape_content_array_is_accepted_on_a_user_turn() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is in this image?"},
                    {"type": "image_url", "image_url": {
                        "url": "data:image/png;base64,AAAA"
                    }},
                    {"type": "image_url", "image_url": {
                        "url": "https://example.com/x.jpg"
                    }},
                ],
            }],
        }))
        .expect("an OpenAI-shape multimodal request should parse");

        request.validate().expect("it should validate");
        assert!(
            !request.contains_unsupported_extensions(),
            "the shape every official SDK emits must not be refused"
        );
        let needs = request.needs(0);
        assert!(needs.modalities.contains("text"));
        assert!(needs.modalities.contains(IMAGE_MODALITY));
    }

    /// A text-only array is the same request as the joined string: it must be
    /// accepted everywhere, and must NOT claim the image modality, or a
    /// client that merely uses the array form would be locked out of every
    /// text-only lane in the catalog.
    #[test]
    fn a_text_only_array_needs_nothing_an_ordinary_string_does_not() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "fireworks/glm-5.2",
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "be terse"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "two"},
                ]},
                {"role": "tool", "tool_call_id": "c1",
                 "content": [{"type": "text", "text": "result"}]},
            ],
        }))
        .expect("a text-part array should parse");

        request.validate().expect("it should validate");
        assert!(!request.contains_unsupported_extensions());
        assert_eq!(
            request.needs(0).modalities,
            ["text".to_owned()].into_iter().collect::<BTreeSet<_>>(),
            "a text-only array must not ask a lane for vision"
        );
        // And it flattens to exactly the string it is equivalent to.
        assert_eq!(request.provider_messages()[1].content, "one\ntwo");
    }

    /// The parts this router refuses, and each one is refused for a reason
    /// recorded at `content_is_supported`.
    ///
    /// `file` is the load-bearing row: no upstream behind ANY of this
    /// router's four wires defines an OpenAI-shape file part, so a PDF cannot
    /// be carried anywhere, and `tiers.toml` was narrowed to stop advertising
    /// one. If a lane ever gains a real file mapping, this row is what should
    /// fail first.
    #[test]
    fn parts_the_router_cannot_deliver_are_refused_rather_than_dropped() {
        let refused = [
            (
                "file",
                json!([{"type": "file", "file": {"filename": "a.pdf", "file_data": "JVBER"}}]),
            ),
            (
                "input_audio",
                json!([{"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}]),
            ),
            ("refusal", json!([{"type": "refusal", "refusal": "no"}])),
            ("an unknown part type", json!([{"type": "video_url"}])),
            ("a part that is not an object", json!(["bare string"])),
            (
                "an image part with an unpreservable field",
                json!([{"type": "image_url", "image_url": {
                    "url": "https://example.com/x.jpg", "detail": "high"
                }}]),
            ),
            (
                "an image part with an empty url",
                json!([{"type": "image_url", "image_url": {"url": "   "}}]),
            ),
            (
                "a text part carrying an extra field",
                json!([{"type": "text", "text": "hi", "annotations": []}]),
            ),
        ];
        for (what, content) in refused {
            let request: ChatCompletionRequest = serde_json::from_value(json!({
                "model": "anthropic/claude-sonnet-5",
                "messages": [{"role": "user", "content": content}],
            }))
            .expect("the body parses; it is the predicate that refuses it");
            assert!(
                request.contains_unsupported_extensions(),
                "{what} must be refused, not silently dropped on the way upstream"
            );
        }
    }

    /// An image is a `user`-turn part in OpenAI's own schema — the system and
    /// tool content-part unions are text-only, and the spec says outright
    /// "For tool messages, only type `text` is supported". Accepting one
    /// elsewhere would invent a shape no SDK emits and no upstream documents.
    #[test]
    fn an_image_part_outside_a_user_turn_is_refused() {
        for role in ["system", "assistant", "tool"] {
            let mut message = json!({
                "role": role,
                "content": [{"type": "image_url", "image_url": {
                    "url": "https://example.com/x.jpg"
                }}],
            });
            if role == "tool" {
                message["tool_call_id"] = json!("c1");
            }
            let request: ChatCompletionRequest = serde_json::from_value(json!({
                "model": "anthropic/claude-sonnet-5",
                "messages": [message],
            }))
            .expect("it parses");
            assert!(
                request.contains_unsupported_extensions(),
                "an image on a {role} turn must be refused"
            );
        }
    }

    #[test]
    fn image_url_admissibility_allows_only_https_and_inline_data() {
        // Admitted: public https and inline base64 data URIs.
        for ok in [
            "https://example.com/x.jpg",
            "https://cdn.example.com:8443/a/b/c.png?token=1",
            "data:image/png;base64,AAAA",
        ] {
            assert!(image_url_is_admissible(ok), "{ok} should be admitted");
        }
        // Refused: non-https schemes, protocol-relative, and internal hosts —
        // including the cloud-metadata endpoint reached over either scheme,
        // IPv6 loopback/link-local, and the IPv4-mapped form of the metadata IP.
        for bad in [
            "",
            "http://example.com/x.jpg",
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "https://169.254.169.254/",
            "https://127.0.0.1/x.jpg",
            "https://10.0.0.5/x.jpg",
            "https://192.168.1.1/x.jpg",
            "https://172.16.9.9/x.jpg",
            "https://100.64.0.1/x.jpg",
            "https://0.0.0.0/x.jpg",
            "https://[::1]/x.jpg",
            "https://[fe80::1]/x.jpg",
            "https://[fc00::1]/x.jpg",
            "https://[::ffff:169.254.169.254]/x.jpg",
            "file:///etc/passwd",
            "//169.254.169.254/x.jpg",
            "ftp://example.com/x.jpg",
            "not a url",
        ] {
            assert!(!image_url_is_admissible(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn a_request_naming_an_internal_image_url_is_refused_at_admission() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{
                "role": "user",
                "content": [{"type": "image_url", "image_url": {
                    "url": "http://169.254.169.254/latest/meta-data/"
                }}],
            }],
        }))
        .expect("it parses");
        assert!(
            request.contains_unsupported_extensions(),
            "an SSRF-shaped image URL must be refused before dispatch"
        );
    }

    /// The reserve arm. An `https://` image is ~60 bytes on the wire and
    /// costs the upstream thousands of tokens, so the byte bound alone
    /// under-holds by roughly 80x — this is the arm that closes that, and
    /// `MAX_IMAGE_PROMPT_TOKENS` is the documented worst case it holds.
    #[test]
    fn a_url_image_reserves_its_worst_case_rather_than_its_byte_length() {
        let with_image: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.jpg"}},
            ]}],
        }))
        .expect("it parses");
        let text_only: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
            ]}],
        }))
        .expect("it parses");

        let imaged = with_image.reservation_usage(100).prompt_tokens;
        let plain = text_only.reservation_usage(100).prompt_tokens;
        let url_bytes =
            byte_len(r#"{"image_url":{"url":"https://example.com/x.jpg"},"type":"image_url"}"#);
        assert!(
            imaged >= plain + MAX_IMAGE_PROMPT_TOKENS,
            "one image must add at least its documented worst case ({MAX_IMAGE_PROMPT_TOKENS}); \
             got {imaged} against a text-only {plain}"
        );
        assert!(
            MAX_IMAGE_PROMPT_TOKENS > url_bytes * 10,
            "the point of the arm is that the URL's own byte length ({url_bytes}) is nowhere \
             near what the upstream will meter for it"
        );
        // Two images hold twice.
        let two: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/a.jpg"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/b.jpg"}},
            ]}],
        }))
        .expect("it parses");
        assert!(
            two.reservation_usage(100).prompt_tokens
                >= plain + 2 * MAX_IMAGE_PROMPT_TOKENS - url_bytes,
            "each image reserves its own worst case"
        );
    }

    /// `IMAGE_RESERVE_COVERS_EVERY_FAMILY` — every per-image figure this
    /// catalog's providers publish, asserted against the constant so that a
    /// family raising its published number past the hold fails loudly here
    /// rather than quietly under-recovering in production.
    ///
    /// The rows are the evidence for `MAX_IMAGE_PROMPT_TOKENS`; the doc
    /// comment there carries the citations and the honest caveat that three
    /// families publish no ceiling at all.
    #[test]
    fn the_image_reserve_bounds_every_published_per_family_figure() {
        // (family, published max tokens for one image)
        const PUBLISHED: [(&str, u64); 5] = [
            ("fireworks qwen2.5-vl 3840x2160", 10_549),
            ("anthropic claude 4.7+ high-resolution", 4_784),
            ("anthropic standard tier", 1_568),
            ("openai patch-budget nano (1536 x 2.46)", 3_779),
            ("google gemini 3 ultra_high", 2_240),
        ];
        for (family, published) in PUBLISHED {
            assert!(
                MAX_IMAGE_PROMPT_TOKENS >= published,
                "{family} publishes {published} tokens for one image, above the {} this \
                 reservation holds — the hold would under-recover on that lane",
                MAX_IMAGE_PROMPT_TOKENS
            );
        }
        assert_eq!(
            MAX_IMAGE_PROMPT_TOKENS,
            PUBLISHED.iter().map(|(_, tokens)| *tokens).max().unwrap(),
            "the hold is exactly the largest published figure: lower under-recovers, higher \
             would be a number no provider document supports"
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

    /// Usage that reaches a ceiling, so the synthesis says `length`.
    fn usage_at_ceiling() -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens: 10,
            completion_tokens: 128,
            total_tokens: 138,
            prompt_tokens_details: None,
        }
    }

    /// Usage well under any ceiling, so the synthesis says `stop`.
    fn usage_under_ceiling() -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            prompt_tokens_details: None,
        }
    }

    /// The consumption rule: a real reason wins, an absent one falls back to
    /// the unchanged synthesis, and the provenance is recorded either way.
    #[test]
    fn a_real_stop_reason_wins_and_absence_falls_back_to_the_synthesis() {
        let resolved = AttemptFinishReason::resolve(
            Some(StopReason::ContentFilter),
            false,
            usage_under_ceiling(),
            Some(128),
        );
        assert_eq!(resolved.reason, "content_filter");
        assert_eq!(resolved.source, FINISH_REASON_UPSTREAM);

        let resolved = AttemptFinishReason::resolve(None, false, usage_at_ceiling(), Some(128));
        assert_eq!(
            resolved.reason, "length",
            "with no real reason the token arithmetic runs exactly as before"
        );
        assert_eq!(resolved.source, FINISH_REASON_SYNTHETIC);
    }

    /// The source is the token migration 0004's CHECK reserved for it. Any
    /// other spelling would fail the constraint and abort the settle
    /// transaction, so this is pinned rather than left to a doc comment.
    #[test]
    fn the_source_tokens_are_the_two_the_ledger_constraint_permits() {
        assert_eq!(FINISH_REASON_UPSTREAM, "upstream");
        assert_eq!(FINISH_REASON_SYNTHETIC, "synthetic");
        for source in [FINISH_REASON_UPSTREAM, FINISH_REASON_SYNTHETIC] {
            assert_eq!(finish_reason_source_from_keyword(source), Some(source));
        }
        assert_eq!(
            finish_reason_source_from_keyword("provider"),
            None,
            "'provider' is NOT a permitted value; it would violate \
             usage_events_finish_reason_source_is_known"
        );
        assert_eq!(finish_reason_source_from_keyword(""), None);
    }

    /// Every row of `AttemptFinishReason`'s divergence table, pinned.
    ///
    /// Each case is a (real, synthetic) disagreement. The assertion is on the
    /// resolved reason and on the `shape_ok` label it produces — the two
    /// things the change actually moves. Serving and billing are unaffected
    /// in every row, which the sibling test below states as its own fact.
    #[test]
    fn every_divergence_row_resolves_to_the_real_reason_and_relabels_shape() {
        // (row, real, has_tool_calls, usage, max_tokens, synth, shape with
        //  synth, shape with real)
        let table = [
            (
                1,
                StopReason::Stop,
                false,
                usage_at_ceiling(),
                "length",
                false,
                true,
            ),
            (
                2,
                StopReason::Stop,
                true,
                usage_under_ceiling(),
                "tool_calls",
                true,
                true,
            ),
            (
                3,
                StopReason::Length,
                false,
                usage_under_ceiling(),
                "stop",
                true,
                false,
            ),
            (
                4,
                StopReason::Length,
                true,
                usage_under_ceiling(),
                "tool_calls",
                true,
                false,
            ),
            (
                5,
                StopReason::ToolCalls,
                false,
                usage_under_ceiling(),
                "stop",
                true,
                true,
            ),
            (
                6,
                StopReason::ToolCalls,
                false,
                usage_at_ceiling(),
                "length",
                false,
                true,
            ),
            (
                7,
                StopReason::ContentFilter,
                false,
                usage_under_ceiling(),
                "stop",
                true,
                true,
            ),
            (
                8,
                StopReason::ContentFilter,
                false,
                usage_at_ceiling(),
                "length",
                false,
                true,
            ),
        ];
        for (row, real, has_tool_calls, usage, synth, shape_with_synth, shape_with_real) in table {
            let max_tokens = Some(128);
            assert_eq!(
                finish_reason(has_tool_calls, usage, max_tokens),
                synth,
                "row {row}: the synthesis must be the value this row claims it is"
            );
            assert_ne!(
                real.as_str(),
                synth,
                "row {row} is only a divergence row if the two actually differ"
            );

            let resolved =
                AttemptFinishReason::resolve(Some(real), has_tool_calls, usage, max_tokens);
            assert_eq!(
                resolved.reason,
                real.as_str(),
                "row {row}: the real value wins"
            );
            assert_eq!(resolved.source, FINISH_REASON_UPSTREAM, "row {row}");

            assert_eq!(
                shape_ok(emitted_text(), true, synth),
                shape_with_synth,
                "row {row}: shape label under the synthesis"
            );
            assert_eq!(
                shape_ok(emitted_text(), true, resolved.reason),
                shape_with_real,
                "row {row}: shape label under the real reason"
            );
        }
    }

    /// Rows 7 and 8: a `content_filter` completion labels as a GOOD shape,
    /// because `shape_ok` only rejects `length`. Pinned deliberately — it is a
    /// pre-existing property of the predicate now reachable for the first
    /// time, not a judgment this change made, and it is the one row of the
    /// table a reviewer is most likely to think is a bug.
    #[test]
    fn content_filter_labels_as_a_good_shape_because_only_length_is_rejected() {
        assert!(
            shape_ok(emitted_text(), true, "content_filter"),
            "shape_ok's predicate is `!= length`; withheld output still passes it"
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
            stop_reason: None,
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
                cache_write_input_tokens: None,
                input_tokens: Some(10),
                cached_input_tokens: None,
                output_tokens: Some(500),
            }),
            reasoning_content: None,
            stop_reason: None,
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
            "model": "openai/gpt-5.6-luna",
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

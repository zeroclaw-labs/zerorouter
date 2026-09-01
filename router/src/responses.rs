//! The INBOUND OpenAI Responses API — `POST /v1/responses`, the dialect a
//! modern Codex CLI speaks and the only one it speaks (`wire_api =
//! "responses"`). Until this existed the router served chat completions alone
//! and every such client met a 405.
//!
//! # One admission path, two serializers
//!
//! Nothing here admits, prices, reserves, walks, or settles anything. A
//! Responses request is TRANSLATED into the router's internal request form
//! ([`ChatCompletionRequest`]) and then runs the identical pipeline a chat
//! request runs — same validation, same modality gate, same image-URL
//! admissibility check, same reservation sizing, same walk, same settle. The
//! divergence is exactly two points: this module parses the request, and this
//! module serializes the answer. That is the playground precedent applied to a
//! wire: a second money path is the one thing a metering business must not
//! grow, and the cheapest way to not grow one is to have the new surface reach
//! the old one before any money moves.
//!
//! The practical shape of that: every image part is rewritten into the
//! chat-completions content-array shape rather than being carried in a
//! Responses-specific structure, so `openai::part_is_supported` and its SSRF
//! gate (`image_url_is_admissible`) judge it without knowing which endpoint it
//! arrived on. There is no bypass to keep in sync because there is no second
//! path.
//!
//! # Prompt caching is the CHAT surface only
//!
//! Client-controlled prompt caching — `cache_control` breakpoints on messages
//! and tools — is accepted on `/v1/chat/completions` and NOT here. This
//! dialect's own caching spellings stay refused: `prompt_cache_key` is named
//! in the unsupported-fields list, and a `cache_control` on a Responses tool
//! is refused by the same rule.
//!
//! That is a scope decision rather than a limitation of the pipeline. The
//! translation into [`ChatCompletionRequest`] would have to decide where a
//! Responses item's breakpoint lands among the chat messages it becomes, and a
//! breakpoint in the wrong place is not a cosmetic difference — it is a
//! different set of tokens billed at 1.25x. Refusing is the honest answer
//! until that mapping is written and tested; accepting and re-placing it would
//! charge a customer for boundaries they did not choose.
//!
//! Note that a Responses request served by an Anthropic lane still gets the
//! WIRE's own three default breakpoints, exactly as it always has. What is
//! unavailable here is the client's control over them, not caching itself.
//!
//! # The mirror
//!
//! [`crate::wire::responses`] is the OUTBOUND client for the same API, and its
//! mappings are the exact inverse of these: `build_responses_input` shows how
//! internal messages become Responses items, `parse_envelope` shows how a
//! Responses envelope becomes an internal response, and its SSE loop shows
//! which events a Responses consumer actually reads. Where the two modules
//! disagree about a shape, one of them is wrong.
//!
//! # Fail loud
//!
//! The chat surface refuses any field it cannot forward
//! (`ChatCompletionRequest::contains_unsupported_extensions`), and so does
//! this one — an accepted-then-dropped knob is a customer paying for a request
//! they did not send. The one addition is that the refusal NAMES the offending
//! keys ([`ApiError::UnsupportedRequestFieldsNamed`]): a modern agent client
//! sends half a dozen knobs at once, and "some field is unsupported" is not a
//! message anyone can act on.
//!
//! # What this dialect deliberately does not carry
//!
//! * **`store: true` and `previous_response_id`** — refused with their own
//!   codes. Both ask for server-side storage of a customer's conversation,
//!   which is the one thing this router's whole product says it does not do.
//! * **Reasoning items on INPUT.** A `reasoning` item is refused. The router
//!   emits reasoning as a summary item and never as the opaque
//!   `encrypted_content` a client would be expected to hand back, so a
//!   conforming client has nothing to replay; a client that replays one anyway
//!   gets a named 400 rather than a silently truncated turn.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::ApiError;
use crate::openai::{
    ChatCompletionRequest, OpenAiFunctionCall, OpenAiFunctionSpec, OpenAiMessage, OpenAiTool,
    OpenAiToolCall, OpenAiUsage, ZeroRouterRequestOptions, ZeroRouterResponseMetadata,
};
use crate::provider::ToolCall;

/// Ceiling on the field list a refusal names. The list is composed of key
/// names and type tags — never values — so it cannot carry prompt content, but
/// a request with a thousand junk keys must still not mint a thousand-key
/// error message.
const MAX_NAMED_FIELDS: usize = 12;

/// A `POST /v1/responses` body, in the narrow shape this router accepts.
///
/// Every field below is one the router can actually honor. Everything else
/// lands in `extra` through the flatten and is refused by name — the same
/// discipline `ChatCompletionRequest` applies, arrived at from the same
/// premise.
#[derive(Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    /// A bare string (one user turn) or an array of items. `Value` rather than
    /// a typed enum because the item grammar is validated by hand below, where
    /// a refusal can name the item type it did not recognize; a serde error
    /// would only say the input did not match a union.
    #[serde(default)]
    pub input: Value,
    /// The system prompt, in this dialect's spelling. Prepended as a `system`
    /// message, which is where every wire this router speaks expects it.
    pub instructions: Option<String>,
    /// This dialect's `max_tokens`. Mapped straight across: the router's
    /// internal ceiling has always been the client's requested generation
    /// limit, and the Responses API's own lower bound of 16 belongs to the
    /// OUTBOUND wire that speaks to OpenAI (`wire::responses`), not here.
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<ResponsesTool>,
    pub tool_choice: Option<Value>,
    /// `false` is accepted because it is exactly true of this router; `true`
    /// is refused by name. Absent is accepted too — OpenAI's default is
    /// `true`, but a client that never mentioned storage has not ASKED for it,
    /// and refusing every request that omitted the field would refuse every
    /// simple client for a preference it never expressed.
    pub store: Option<bool>,
    /// Present and non-null is refused: resolving it would require having
    /// stored the previous response.
    pub previous_response_id: Option<Value>,
    /// `true` and absent are accepted — parallel tool calling is what the
    /// walk's native tool path already delivers, so the client is asking for
    /// exactly what it gets. `false` is refused by name: a serial-execution
    /// guarantee no wire here forwards, and a caller who asked for one must
    /// not be told it holds (the `strict: true` rule, applied to ordering).
    pub parallel_tool_calls: Option<bool>,
    /// Accepted only in its default shape: absent, `null`, `{}`, or an
    /// explicit `{"format": {"type": "text"}}` — plain text is precisely what
    /// this router returns, so the explicit spelling of the default is not a
    /// request for anything. Any other member is refused by name:
    /// `format.type = "json_schema"` is structured-output enforcement this
    /// router does not forward, and `verbosity` changes how much a model
    /// writes — accepting either silently would bill a customer for behavior
    /// they did not get.
    pub text: Option<Value>,
    /// ZeroRouter's own request namespace, identical to the chat surface's —
    /// typed before the flatten so serde consumes the key. Carried across so a
    /// request that works on one endpoint works on the other; the precedence
    /// and conflict rules are the shared path's, not this module's.
    pub zerorouter: Option<ZeroRouterRequestOptions>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One `tools[]` entry. Responses function tools are FLAT — `name`,
/// `description`, `parameters` at the top level — where chat completions nests
/// them under `function`. Getting that backwards is the specific mistake the
/// unit tests here exist to catch.
#[derive(Deserialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    /// Nullable on this wire, so `Option` rather than a `String` default: a
    /// client that sends `"description": null` is not sending a malformed
    /// request.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
    /// Structured-output enforcement. `false` and absent are accepted;
    /// `true` is refused, because the router does not forward the flag and a
    /// caller who asked for a guarantee must not be told it holds.
    #[serde(default)]
    pub strict: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What the response envelope echoes back from the request.
///
/// Carried rather than re-read because the request is consumed by the walk:
/// the Responses envelope is required by the official SDKs to restate the
/// request's `instructions`, `tools`, `tool_choice`, and `temperature`, and
/// restating them as constants would make the router lie about what it was
/// asked for.
#[derive(Clone, Debug, Default)]
pub struct ResponsesEcho {
    pub instructions: Option<String>,
    pub temperature: Option<f64>,
    /// The tools as this dialect spells them, ready to serialize back.
    pub tools: Vec<Value>,
}

/// One entry of the `output` array this router assembles.
///
/// The same three shapes on both paths: the buffered serializer folds a
/// [`crate::provider::ChatResponse`] into them and the streaming serializer
/// accumulates them delta by delta, so the terminal `response.completed`
/// envelope and the equivalent non-streaming body are built by one function
/// and cannot drift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputItem {
    Message(String),
    /// Chain-of-thought, carried as a `reasoning` item's summary text.
    ///
    /// Emitted rather than dropped because a thinking model can answer
    /// ENTIRELY in reasoning content (`openai::EmittedOutput` exists because
    /// of exactly that case), and dropping it would bill the customer for a
    /// response they received nothing of. It is a summary, never the
    /// `encrypted_content` blob a client is expected to replay — see this
    /// module's header for why the input grammar refuses reasoning items.
    Reasoning(String),
    FunctionCall(ToolCall),
}

impl ResponsesRequest {
    /// Translate into the router's internal request form, or refuse.
    ///
    /// Every refusal here happens BEFORE the caller reaches admission, so
    /// nothing is reserved and no upstream is dialled — the placement rule
    /// `priority_conflict` and `modality_unsupported` already follow.
    pub fn into_internal(self) -> Result<(ChatCompletionRequest, ResponsesEcho), ApiError> {
        // The two storage refusals first, and before the generic unknown-field
        // sweep, so a client that sends `store: true` alongside some other
        // unsupported knob is told about the one that will never be supported
        // rather than the one that might.
        if self.store == Some(true) {
            return Err(ApiError::ResponsesStoreUnsupported);
        }
        if self
            .previous_response_id
            .as_ref()
            .is_some_and(|value| !value.is_null())
        {
            return Err(ApiError::ResponsesPreviousResponseUnsupported);
        }
        if self.parallel_tool_calls == Some(false) {
            return Err(unsupported(["parallel_tool_calls=false"]));
        }
        if let Some(text) = self.text.as_ref().filter(|text| !text.is_null()) {
            let Some(object) = text.as_object() else {
                return Err(unsupported(["text"]));
            };
            for (key, value) in object {
                let default_format = key == "format"
                    && value.get("type").and_then(Value::as_str) == Some("text")
                    && value.as_object().is_some_and(|format| format.len() == 1);
                if !(value.is_null() || default_format) {
                    return Err(unsupported([format!("text.{key}")]));
                }
            }
        }
        if !self.extra.is_empty() {
            return Err(unsupported(self.extra.keys()));
        }
        // `null` is not a request for a tool choice, and clients send it
        // routinely; anything else must be exactly `auto`, mirroring
        // `ChatCompletionRequest::validate`, which refuses the same set on the
        // chat surface for the same reason (no wire here forwards a forced
        // choice).
        let tool_choice = match self.tool_choice {
            Some(Value::Null) | None => None,
            Some(choice) => {
                if choice.as_str() != Some("auto") {
                    return Err(unsupported(["tool_choice"]));
                }
                Some(choice)
            }
        };

        let mut tools = Vec::with_capacity(self.tools.len());
        for tool in self.tools {
            tools.push(tool.into_internal()?);
        }

        let mut messages = Vec::new();
        if let Some(instructions) = self
            .instructions
            .as_deref()
            .filter(|instructions| !instructions.is_empty())
        {
            messages.push(plain("system", instructions));
        }
        translate_input(self.input, &mut messages)?;

        let echo = ResponsesEcho {
            instructions: self.instructions,
            temperature: self.temperature,
            tools: tools.iter().map(tool_to_wire).collect(),
        };
        Ok((
            ChatCompletionRequest {
                model: self.model,
                messages,
                stream: self.stream,
                temperature: self.temperature,
                max_tokens: self.max_output_tokens,
                tools,
                tool_choice,
                // Chat-completions-only: this dialect signals usage by always
                // carrying it on the terminal event, so there is no knob to
                // carry and nothing is dropped.
                stream_options: None,
                zerorouter: self.zerorouter,
                extra: Map::new(),
            },
            echo,
        ))
    }
}

impl ResponsesTool {
    fn into_internal(self) -> Result<OpenAiTool, ApiError> {
        if self.kind != "function" {
            // Named, because "web_search"/"file_search"/"mcp" are what a
            // client actually sends here and the fix is to drop that tool, not
            // to reshape it.
            return Err(unsupported([format!("tools[].type={}", self.kind)]));
        }
        if self.strict == Some(true) {
            return Err(unsupported(["tools[].strict"]));
        }
        if !self.extra.is_empty() {
            return Err(unsupported(
                self.extra.keys().map(|key| format!("tools[].{key}")),
            ));
        }
        Ok(OpenAiTool {
            kind: "function".to_owned(),
            function: OpenAiFunctionSpec {
                name: self.name,
                description: self.description.unwrap_or_default(),
                // An absent or null schema is a tool that takes no arguments,
                // which is what the empty object means on the chat surface
                // too (`OpenAiFunctionSpec::parameters` defaults to `{}`).
                parameters: self
                    .parameters
                    .filter(|parameters| !parameters.is_null())
                    .unwrap_or_else(|| json!({})),
                extra: Map::new(),
            },
            extra: Map::new(),
        })
    }
}

/// The `tools[]` entry as this dialect spells it, for the response echo.
fn tool_to_wire(tool: &OpenAiTool) -> Value {
    json!({
        "type": "function",
        "name": tool.function.name,
        "description": tool.function.description,
        "parameters": tool.function.parameters,
    })
}

/// Refuse a request, naming the fields that caused it.
///
/// Sorted and de-duplicated so the message is stable across serde's map order,
/// and bounded so a hostile body cannot mint an unbounded error.
fn unsupported<I, S>(fields: I) -> ApiError
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let named: BTreeSet<String> = fields
        .into_iter()
        .map(|field| field.as_ref().to_owned())
        .collect();
    let total = named.len();
    let mut listed: Vec<String> = named.into_iter().take(MAX_NAMED_FIELDS).collect();
    let overflow = total.saturating_sub(listed.len());
    if overflow > 0 {
        listed.push(format!("and {overflow} more"));
    }
    ApiError::UnsupportedRequestFieldsNamed {
        fields: listed.join(", "),
    }
}

fn plain(role: &str, text: &str) -> OpenAiMessage {
    message(role, Value::String(text.to_owned()))
}

fn message(role: &str, content: Value) -> OpenAiMessage {
    OpenAiMessage {
        role: role.to_owned(),
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
        reasoning_content: None,
        extra: Map::new(),
    }
}

/// Keys an item may carry beyond its own payload.
///
/// `id` and `status` are this router's OWN echo coming back on a replayed
/// history — a client returning them is returning what it was handed, so
/// ignoring them drops nothing the client meant. Every other key is refused.
const ITEM_ECHO_KEYS: [&str; 3] = ["type", "id", "status"];

fn translate_input(input: Value, messages: &mut Vec<OpenAiMessage>) -> Result<(), ApiError> {
    match input {
        // The shorthand every quick-start uses: the whole input is one user
        // turn, verbatim. Text is text — no grammar is applied to it, the same
        // rule `wire::user_parts` states for the outbound direction.
        Value::String(text) => {
            messages.push(plain("user", &text));
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                translate_item(item, messages)?;
            }
            Ok(())
        }
        // An absent or null input with instructions alone is a legal (if odd)
        // request and lands here as `Null`; the shared validator refuses it if
        // it produced no messages at all.
        Value::Null => Ok(()),
        _ => Err(ApiError::InvalidRequest),
    }
}

fn translate_item(item: Value, messages: &mut Vec<OpenAiMessage>) -> Result<(), ApiError> {
    let Some(item) = item.as_object() else {
        return Err(ApiError::InvalidRequest);
    };
    // An input message may omit `type` entirely — that is the API's own
    // "easy input message" shape (`{"role":"user","content":"hi"}`) and every
    // hand-written client uses it.
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match kind {
        "message" => translate_message(item, messages),
        "function_call" => translate_function_call(item, messages),
        "function_call_output" => translate_function_call_output(item, messages),
        other => Err(unsupported([format!("input[].type={other}")])),
    }
}

fn reject_unknown_keys(
    item: &Map<String, Value>,
    allowed: &[&str],
    context: &str,
) -> Result<(), ApiError> {
    let unknown: Vec<String> = item
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(|key| format!("{context}.{key}"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(unsupported(unknown))
}

fn translate_message(
    item: &Map<String, Value>,
    messages: &mut Vec<OpenAiMessage>,
) -> Result<(), ApiError> {
    let mut allowed = ITEM_ECHO_KEYS.to_vec();
    allowed.extend(["role", "content"]);
    reject_unknown_keys(item, &allowed, "input[]")?;
    let role = match item.get("role").and_then(Value::as_str) {
        // `developer` is this dialect's name for the system turn on the
        // o-series models; both spellings mean the same message.
        Some("system" | "developer") => "system",
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some(other) => return Err(unsupported([format!("input[].role={other}")])),
        None => return Err(ApiError::InvalidRequest),
    };
    let content = item.get("content").unwrap_or(&Value::Null);
    let content = match content {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(parts) => Value::Array(translate_parts(parts)?),
        Value::Null => Value::Null,
        _ => return Err(ApiError::InvalidRequest),
    };
    messages.push(message(role, content));
    Ok(())
}

/// Rewrite this dialect's content parts into the CHAT dialect's, which is what
/// the shared path already knows how to validate, size, and gate.
///
/// This is where the "one admission path" claim is cashed: an `input_image`
/// becomes exactly the `{"type":"image_url","image_url":{"url":…}}` shape that
/// `openai::part_is_supported` runs `image_url_is_admissible` over, so the
/// SSRF refusal and the per-image reservation surcharge apply to a Responses
/// request without either of them knowing this endpoint exists.
fn translate_parts(parts: &[Value]) -> Result<Vec<Value>, ApiError> {
    let mut translated = Vec::with_capacity(parts.len());
    for part in parts {
        let Some(part) = part.as_object() else {
            return Err(ApiError::InvalidRequest);
        };
        match part.get("type").and_then(Value::as_str) {
            // `input_text` is what a client sends; `output_text` is what this
            // router emitted, replayed back on the next turn. Both are text.
            // `annotations` and `logprobs` ride an `output_text` echo and are
            // this router's own (empty) fields coming home.
            Some(kind @ ("input_text" | "output_text")) => {
                reject_unknown_keys(
                    part,
                    &["type", "text", "annotations", "logprobs"],
                    &format!("input[].content[].{kind}"),
                )?;
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    return Err(ApiError::InvalidRequest);
                };
                translated.push(json!({ "type": "text", "text": text }));
            }
            Some("input_image") => {
                reject_unknown_keys(
                    part,
                    &["type", "image_url", "detail"],
                    "input[].content[].input_image",
                )?;
                // `detail` is a REQUIRED field of this dialect's image part
                // (openai/openai-openapi, `InputImageContent`), so it cannot
                // simply be refused. `auto` is the schema's own default and
                // the only value the outbound wires send; any other value
                // would be silently downgraded to `auto` at the upstream, and
                // a silently downgraded hint that changes what an image costs
                // is exactly the kind of drop this surface refuses.
                match part.get("detail").and_then(Value::as_str) {
                    None | Some("auto") => {}
                    Some(other) => {
                        return Err(unsupported([format!(
                            "input[].content[].input_image.detail={other}"
                        )]));
                    }
                }
                // A bare STRING here, not chat completions' `{"url":…}`
                // object — the difference `wire::responses` documents in the
                // other direction.
                let Some(url) = part.get("image_url").and_then(Value::as_str) else {
                    return Err(ApiError::InvalidRequest);
                };
                translated.push(json!({
                    "type": "image_url",
                    "image_url": { "url": url },
                }));
            }
            Some(other) => {
                return Err(unsupported([format!("input[].content[].type={other}")]));
            }
            None => return Err(ApiError::InvalidRequest),
        }
    }
    Ok(translated)
}

/// A `function_call` item becomes an assistant turn carrying a tool call —
/// and joins the assistant turn ahead of it when there is one.
///
/// The merge is not cosmetic. This dialect emits the assistant's text and each
/// of its tool calls as SEPARATE items, while every wire the router dispatches
/// on models them as ONE assistant turn; Anthropic in particular rejects two
/// consecutive assistant turns outright. Leaving them split would produce a
/// history that round-trips through this endpoint and then fails at the
/// upstream, which is the worst place to discover it.
fn translate_function_call(
    item: &Map<String, Value>,
    messages: &mut Vec<OpenAiMessage>,
) -> Result<(), ApiError> {
    let mut allowed = ITEM_ECHO_KEYS.to_vec();
    allowed.extend(["call_id", "name", "arguments"]);
    reject_unknown_keys(item, &allowed, "input[]")?;
    let call = OpenAiToolCall {
        id: item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: "function".to_owned(),
        function: OpenAiFunctionCall {
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            // Arguments are a JSON *string* on both dialects. An absent one is
            // the no-argument call, spelled the way every wire spells it.
            arguments: item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_owned(),
            extra: Map::new(),
        },
        extra: Map::new(),
    };
    match messages.last_mut() {
        Some(last) if last.role == "assistant" => last.tool_calls.push(call),
        _ => {
            let mut assistant = message("assistant", Value::Null);
            assistant.tool_calls.push(call);
            messages.push(assistant);
        }
    }
    Ok(())
}

fn translate_function_call_output(
    item: &Map<String, Value>,
    messages: &mut Vec<OpenAiMessage>,
) -> Result<(), ApiError> {
    let mut allowed = ITEM_ECHO_KEYS.to_vec();
    allowed.extend(["call_id", "output"]);
    reject_unknown_keys(item, &allowed, "input[]")?;
    let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
        return Err(ApiError::InvalidRequest);
    };
    // The API has grown a structured-content form of `output`; this router
    // carries a tool result as text on every wire it dispatches on, so a
    // structured one is refused rather than flattened into something the
    // caller did not write.
    let Some(output) = item.get("output").and_then(Value::as_str) else {
        return Err(unsupported(["input[].output"]));
    };
    let mut tool = message("tool", Value::String(output.to_owned()));
    tool.tool_call_id = Some(call_id.to_owned());
    messages.push(tool);
    Ok(())
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// Everything the envelope needs, gathered once so the buffered body and the
/// streaming terminal are built by ONE function and cannot describe the same
/// completion differently.
pub struct Envelope<'a> {
    /// The ledger's request id (`x-request-id`), verbatim.
    pub request_id: &'a str,
    pub model: &'a str,
    pub items: &'a [OutputItem],
    /// `None` while the response is still in progress.
    pub usage: Option<OpenAiUsage>,
    /// The SYNTHESIZED chat finish reason, which is what decides `status`.
    ///
    /// Deliberately the synthesis and not the upstream's own reason, because
    /// that is what the chat body reports and the two wires must not tell a
    /// customer's agent loop different stories about the same completion —
    /// see `openai::AttemptFinishReason` for why the body keeps the synthesis.
    /// `None` means the response has not finished.
    pub finish_reason: Option<&'a str>,
    pub echo: &'a ResponsesEcho,
    pub zerorouter: Option<&'a ZeroRouterResponseMetadata>,
}

/// This dialect's id for a request, derived from the ledger's.
///
/// The whole `x-request-id` is embedded rather than a fresh identifier minted,
/// so the id a customer reads off the response body leads to the settled row
/// by a substring match and nothing has to be correlated by timestamp. Item
/// ids follow the same rule with their own prefixes.
#[must_use]
pub fn response_id(request_id: &str) -> String {
    format!("resp_{request_id}")
}

fn item_id(prefix: &str, request_id: &str, index: usize) -> String {
    format!("{prefix}_{request_id}_{index}")
}

impl OutputItem {
    fn prefix(&self) -> &'static str {
        match self {
            Self::Message(_) => "msg",
            Self::Reasoning(_) => "rs",
            Self::FunctionCall(_) => "fc",
        }
    }

    fn to_wire(&self, request_id: &str, index: usize) -> Value {
        let id = item_id(self.prefix(), request_id, index);
        match self {
            Self::Message(text) => json!({
                "type": "message",
                "id": id,
                "status": "completed",
                "role": "assistant",
                // `annotations` is empty rather than absent: the official SDKs
                // model it as a required field of an output_text part, and a
                // router whose answer cannot be deserialized by the vendor's
                // own client is not compatible in any useful sense.
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            }),
            Self::Reasoning(summary) => json!({
                "type": "reasoning",
                "id": id,
                "summary": [{ "type": "summary_text", "text": summary }],
            }),
            Self::FunctionCall(call) => json!({
                "type": "function_call",
                "id": id,
                "status": "completed",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            }),
        }
    }
}

/// The three output shapes a buffered completion folds into, in the order this
/// dialect presents them: what the model thought, what it said, what it wants
/// called.
#[must_use]
pub fn items_from_completion(
    text: Option<String>,
    reasoning: Option<String>,
    tool_calls: Vec<ToolCall>,
) -> Vec<OutputItem> {
    let mut items = Vec::new();
    if let Some(reasoning) = reasoning.filter(|reasoning| !reasoning.is_empty()) {
        items.push(OutputItem::Reasoning(reasoning));
    }
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        items.push(OutputItem::Message(text));
    }
    items.extend(tool_calls.into_iter().map(OutputItem::FunctionCall));
    items
}

/// Usage in this dialect's spelling.
///
/// `output_tokens_details.reasoning_tokens` is deliberately ABSENT rather than
/// reported as zero. No wire this router dispatches on separates reasoning
/// tokens from output tokens, so the honest report is silence — the same
/// absent-means-unknown contract `openai::ModelObject` states for its
/// capability fields, and for the same reason: a plausible default is
/// indistinguishable from a measurement.
fn usage_to_wire(usage: OpenAiUsage) -> Value {
    json!({
        "input_tokens": usage.prompt_tokens,
        "input_tokens_details": { "cached_tokens": usage.cached_input_tokens() },
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens,
    })
}

#[must_use]
pub fn envelope(parts: &Envelope<'_>) -> Value {
    let (status, incomplete_details) = match parts.finish_reason {
        None => ("in_progress", Value::Null),
        // The one non-terminal terminal: output stopped at the ceiling the
        // caller asked for. `wire::responses` reads exactly this pair in the
        // other direction to produce `StopReason::Length`.
        Some("length") => ("incomplete", json!({ "reason": "max_output_tokens" })),
        Some(_) => ("completed", Value::Null),
    };
    let mut envelope = json!({
        "id": response_id(parts.request_id),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": status,
        "error": Value::Null,
        "incomplete_details": incomplete_details,
        "instructions": parts.echo.instructions,
        "model": parts.model,
        "output": parts
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| item.to_wire(parts.request_id, index))
            .collect::<Vec<_>>(),
        // Never restricted by this router, and never forwarded either: the
        // upstream's own default governs. Stated rather than omitted because
        // the SDKs model it as required.
        "parallel_tool_calls": true,
        "temperature": parts.echo.temperature,
        "tool_choice": "auto",
        "tools": parts.echo.tools,
        "top_p": Value::Null,
        "metadata": json!({}),
    });
    if let Some(usage) = parts.usage {
        envelope["usage"] = usage_to_wire(usage);
    }
    // The same block, the same field name, and the same content as the chat
    // body carries — a customer reading `byok` or the attempts array must not
    // have to learn a second place to look.
    if let Some(block) = parts.zerorouter {
        envelope["zerorouter"] = serde_json::to_value(block)
            .expect("a zerorouter block of strings and integers serializes");
    }
    envelope
}

/// One SSE frame: this dialect names its events, where chat completions leaves
/// the event line off entirely and puts the type in the payload.
pub struct SseFrame {
    pub event: &'static str,
    pub data: String,
}

/// The Responses SSE serializer, and the accumulated output the terminal
/// envelope is built from.
///
/// It is a SERIALIZER over the router's existing `StreamEvent` machinery, not
/// a second stream path: the walk, the delivery accounting, the retention
/// attestation ordering and every settle site are the chat wire's, unchanged.
/// What differs is the bytes this type produces for each event the walk
/// already emits.
pub struct ResponsesStream {
    echo: ResponsesEcho,
    items: Vec<OutputItem>,
    /// Monotonic across the whole stream, as this dialect requires. Consumers
    /// use it to detect a dropped frame, so it must never be reused or reset.
    sequence: u64,
}

impl ResponsesStream {
    #[must_use]
    pub fn new(echo: ResponsesEcho) -> Self {
        Self {
            echo,
            items: Vec::new(),
            sequence: 0,
        }
    }

    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        sequence
    }

    fn frame(&mut self, event: &'static str, mut data: Value) -> SseFrame {
        data["type"] = json!(event);
        data["sequence_number"] = json!(self.next_sequence());
        SseFrame {
            event,
            data: data.to_string(),
        }
    }

    fn envelope(
        &self,
        request_id: &str,
        model: &str,
        usage: Option<OpenAiUsage>,
        finish_reason: Option<&str>,
    ) -> Value {
        envelope(&Envelope {
            request_id,
            model,
            items: &self.items,
            usage,
            finish_reason,
            echo: &self.echo,
            zerorouter: None,
        })
    }

    /// The stream's opening frame — the exact counterpart of the chat wire's
    /// role primer, emitted at the same moment and classified the same way:
    /// scaffolding, carrying no model output, so it can never on its own make
    /// a request billable.
    pub fn created(&mut self, request_id: &str, model: &str) -> SseFrame {
        let response = self.envelope(request_id, model, None, None);
        self.frame("response.created", json!({ "response": response }))
    }

    /// Where the next delta of `kind` belongs: the last item if it is already
    /// one of those, otherwise a fresh item at the end.
    fn open_item(&mut self, reasoning: bool) -> usize {
        let matches_last = match self.items.last() {
            Some(OutputItem::Message(_)) => !reasoning,
            Some(OutputItem::Reasoning(_)) => reasoning,
            _ => false,
        };
        if !matches_last {
            self.items.push(if reasoning {
                OutputItem::Reasoning(String::new())
            } else {
                OutputItem::Message(String::new())
            });
        }
        self.items.len() - 1
    }

    pub fn text_delta(&mut self, request_id: &str, delta: &str) -> SseFrame {
        let index = self.open_item(false);
        if let Some(OutputItem::Message(text)) = self.items.get_mut(index) {
            text.push_str(delta);
        }
        let id = item_id("msg", request_id, index);
        self.frame(
            "response.output_text.delta",
            json!({
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "delta": delta,
            }),
        )
    }

    pub fn reasoning_delta(&mut self, request_id: &str, delta: &str) -> SseFrame {
        let index = self.open_item(true);
        if let Some(OutputItem::Reasoning(text)) = self.items.get_mut(index) {
            text.push_str(delta);
        }
        let id = item_id("rs", request_id, index);
        self.frame(
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": id,
                "output_index": index,
                "summary_index": 0,
                "delta": delta,
            }),
        )
    }

    /// A tool call, as the four frames a Responses consumer reads it from.
    ///
    /// `response.output_item.done` is the load-bearing one and is why this is
    /// four frames and not one: it is the ONLY event ZeroRouter's own outbound
    /// parser (`wire::responses::stream_chat`) takes a tool call from, so
    /// omitting it would make this router unable to consume its own stream.
    /// The arguments arrive whole in a single `.delta` because the walk hands
    /// this serializer a complete `ToolCall` — the internal `StreamEvent`
    /// carries no partial arguments to forward.
    pub fn tool_call(&mut self, request_id: &str, call: &ToolCall) -> Vec<SseFrame> {
        let index = self.items.len();
        self.items.push(OutputItem::FunctionCall(call.clone()));
        let id = item_id("fc", request_id, index);
        let opening = json!({
            "type": "function_call",
            "id": id,
            "status": "in_progress",
            "call_id": call.id,
            "name": call.name,
            "arguments": "",
        });
        let done = self
            .items
            .get(index)
            .map_or_else(|| json!({}), |item| item.to_wire(request_id, index));
        vec![
            self.frame(
                "response.output_item.added",
                json!({ "output_index": index, "item": opening }),
            ),
            self.frame(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "delta": call.arguments,
                }),
            ),
            self.frame(
                "response.function_call_arguments.done",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "arguments": call.arguments,
                }),
            ),
            self.frame(
                "response.output_item.done",
                json!({ "output_index": index, "item": done }),
            ),
        ]
    }

    /// The terminal frame, carrying the complete envelope — status, the whole
    /// accumulated `output`, and the settled usage.
    pub fn completed(
        &mut self,
        request_id: &str,
        model: &str,
        usage: OpenAiUsage,
        finish_reason: &str,
        zerorouter: Option<&ZeroRouterResponseMetadata>,
    ) -> SseFrame {
        let mut response = self.envelope(request_id, model, Some(usage), Some(finish_reason));
        if let Some(block) = zerorouter {
            response["zerorouter"] = serde_json::to_value(block)
                .expect("a zerorouter block of strings and integers serializes");
        }
        // `response.incomplete` is this dialect's terminal for a clipped run,
        // and it is a DIFFERENT event name — the outbound parser treats the
        // two as one terminal precisely because they are two on the wire.
        let event = if response["status"] == json!("incomplete") {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.frame(event, json!({ "response": response }))
    }

    /// An in-band failure. This dialect carries the error as a typed EVENT
    /// rather than chat completions' `{"error":…}` object, and sends no
    /// `[DONE]` sentinel after it — see [`Self::sends_done`].
    pub fn error(&mut self, error: &crate::error::ApiError) -> SseFrame {
        self.frame("error", crate::error::streaming_error_object(error))
    }
}

#[cfg(test)]
mod translation_tests {
    use super::*;

    fn parse(body: Value) -> Result<(ChatCompletionRequest, ResponsesEcho), ApiError> {
        serde_json::from_value::<ResponsesRequest>(body)
            .map_err(|_| ApiError::InvalidRequest)?
            .into_internal()
    }

    /// The refusal a body produces. `ChatCompletionRequest` is deliberately
    /// not `Debug` (it carries prompt content), so the success arm is dropped
    /// rather than formatted.
    fn refuse(body: Value) -> ApiError {
        parse(body).err().expect("the body must be refused")
    }

    #[test]
    fn a_string_input_is_one_user_turn_verbatim() {
        let (request, echo) = parse(json!({ "model": "zero/x", "input": "hello" }))
            .expect("a minimal request translates");
        assert_eq!(request.model, "zero/x");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[0].content, json!("hello"));
        assert!(!request.stream);
        assert!(request.max_tokens.is_none());
        assert!(echo.tools.is_empty());
        request.validate().expect("the shared validator accepts it");
    }

    #[test]
    fn instructions_become_the_leading_system_turn() {
        let (request, echo) = parse(json!({
            "model": "zero/x",
            "instructions": "be terse",
            "input": "hi",
        }))
        .expect("translates");
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, "system");
        assert_eq!(request.messages[0].content, json!("be terse"));
        assert_eq!(request.messages[1].role, "user");
        assert_eq!(echo.instructions.as_deref(), Some("be terse"));
    }

    #[test]
    fn max_output_tokens_temperature_and_stream_map_straight_across() {
        let (request, echo) = parse(json!({
            "model": "zero/x",
            "input": "hi",
            "max_output_tokens": 256,
            "temperature": 0.25,
            "stream": true,
        }))
        .expect("translates");
        assert_eq!(request.max_tokens, Some(256));
        assert_eq!(request.temperature, Some(0.25));
        assert!(request.stream);
        assert_eq!(echo.temperature, Some(0.25));
    }

    #[test]
    fn every_accepted_item_shape_translates_to_the_internal_history() {
        // The full agent loop as this dialect spells it: a developer turn, a
        // user turn with structured content, the assistant's reply, its tool
        // call, and the tool's result.
        let (request, _) = parse(json!({
            "model": "zero/x",
            "input": [
                { "type": "message", "role": "developer",
                  "content": [{ "type": "input_text", "text": "be terse" }] },
                { "role": "user",
                  "content": [{ "type": "input_text", "text": "run pwd" }] },
                { "type": "message", "role": "assistant", "id": "msg_1", "status": "completed",
                  "content": [{ "type": "output_text", "text": "running it",
                                "annotations": [] }] },
                { "type": "function_call", "id": "fc_1", "status": "completed",
                  "call_id": "call_1", "name": "shell",
                  "arguments": "{\"command\":\"pwd\"}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "/home" },
                { "role": "user", "content": "thanks" }
            ],
        }))
        .expect("translates");
        let roles: Vec<&str> = request
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect();
        // Five messages, not six: the assistant's text and its tool call merge
        // into ONE assistant turn, which is the shape every wire models.
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
        assert_eq!(request.messages[2].tool_calls.len(), 1);
        assert_eq!(request.messages[2].tool_calls[0].id, "call_1");
        assert_eq!(request.messages[2].tool_calls[0].function.name, "shell");
        assert_eq!(
            request.messages[2].tool_calls[0].function.arguments,
            r#"{"command":"pwd"}"#
        );
        assert_eq!(request.messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(request.messages[3].content, json!("/home"));
        request.validate().expect("the shared validator accepts it");
        assert!(!request.contains_unsupported_extensions());
    }

    #[test]
    fn consecutive_function_calls_join_one_assistant_turn() {
        let (request, _) = parse(json!({
            "model": "zero/x",
            "input": [
                { "role": "user", "content": "do both" },
                { "type": "function_call", "call_id": "a", "name": "one", "arguments": "{}" },
                { "type": "function_call", "call_id": "b", "name": "two", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "a", "output": "1" },
                { "type": "function_call_output", "call_id": "b", "output": "2" }
            ],
        }))
        .expect("translates");
        let roles: Vec<&str> = request
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool"]);
        assert_eq!(request.messages[1].tool_calls.len(), 2);
        request.validate().expect("the shared validator accepts it");
    }

    #[test]
    fn tools_translate_from_the_flat_shape_to_the_nested_one() {
        let (request, echo) = parse(json!({
            "model": "zero/x",
            "input": "hi",
            "tool_choice": "auto",
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "run a command",
                "strict": false,
                "parameters": { "type": "object" },
            }],
        }))
        .expect("translates");
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].kind, "function");
        assert_eq!(request.tools[0].function.name, "shell");
        assert_eq!(request.tools[0].function.description, "run a command");
        assert_eq!(
            request.tools[0].function.parameters,
            json!({"type":"object"})
        );
        assert_eq!(request.tool_choice, Some(json!("auto")));
        // The echo speaks the dialect back, flat.
        assert_eq!(echo.tools[0]["name"], "shell");
        assert!(echo.tools[0].get("function").is_none());
        request.validate().expect("the shared validator accepts it");
    }

    #[test]
    fn a_null_description_or_schema_is_not_a_malformed_tool() {
        let (request, _) = parse(json!({
            "model": "zero/x",
            "input": "hi",
            "tools": [{ "type": "function", "name": "noop",
                        "description": null, "parameters": null }],
        }))
        .expect("translates");
        assert_eq!(request.tools[0].function.description, "");
        assert_eq!(request.tools[0].function.parameters, json!({}));
        request.validate().expect("the shared validator accepts it");
    }

    #[test]
    fn an_input_image_becomes_the_chat_image_part_the_shared_gates_read() {
        let (request, _) = parse(json!({
            "model": "zero/x",
            "input": [{
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "what is this?" },
                    { "type": "input_image", "detail": "auto",
                      "image_url": "https://example.com/x.jpg" }
                ],
            }],
        }))
        .expect("translates");
        assert_eq!(
            request.messages[0].content,
            json!([
                { "type": "text", "text": "what is this?" },
                { "type": "image_url", "image_url": { "url": "https://example.com/x.jpg" } }
            ])
        );
        // The shared modality reader sees an image, so the capability gate and
        // the per-image reservation surcharge both engage without knowing this
        // request arrived on /v1/responses.
        assert!(request.needs(0).modalities.contains("image"));
        assert!(!request.contains_unsupported_extensions());
    }

    #[test]
    fn an_internal_image_url_is_refused_by_the_shared_ssrf_gate() {
        // Not refused HERE — refused by `openai::image_url_is_admissible`,
        // reached because the translation produced the chat shape. That is the
        // whole point of translating rather than carrying a second grammar.
        let (request, _) = parse(json!({
            "model": "zero/x",
            "input": [{
                "role": "user",
                "content": [{ "type": "input_image",
                              "image_url": "https://169.254.169.254/latest/meta-data/" }],
            }],
        }))
        .expect("translation itself succeeds");
        assert!(
            request.contains_unsupported_extensions(),
            "the cloud-metadata address must not be forwardable from any endpoint"
        );
    }

    #[test]
    fn store_true_and_previous_response_id_get_their_own_refusals() {
        assert!(matches!(
            parse(json!({ "model": "zero/x", "input": "hi", "store": true })),
            Err(ApiError::ResponsesStoreUnsupported)
        ));
        assert!(matches!(
            parse(json!({ "model": "zero/x", "input": "hi", "previous_response_id": "resp_1" })),
            Err(ApiError::ResponsesPreviousResponseUnsupported)
        ));
        // The shapes a client sends when it is NOT asking for either.
        assert!(
            parse(json!({
                "model": "zero/x", "input": "hi",
                "store": false, "previous_response_id": Value::Null
            }))
            .is_ok()
        );
    }

    #[test]
    fn unknown_top_level_fields_are_refused_by_name() {
        let error = refuse(json!({
            "model": "zero/x",
            "input": "hi",
            "reasoning": { "effort": "high" },
            "prompt_cache_key": "k1",
            "top_p": 0.1,
        }));
        let ApiError::UnsupportedRequestFieldsNamed { fields } = error else {
            panic!("expected the named refusal, got {error:?}");
        };
        // Sorted, so the message is stable whatever order serde saw them in.
        assert_eq!(fields, "prompt_cache_key, reasoning, top_p");
    }

    /// The two knobs a real Codex CLI sends whose DEFAULT spelling asks for
    /// exactly what this router already does: parallel tool calling (the
    /// walk's native tool path) and plain-text output. The default shapes are
    /// accepted; every non-default member is refused by name, because each
    /// one is a behavior or cost change no wire here forwards.
    #[test]
    fn default_shaped_parallel_tool_calls_and_text_are_accepted_the_rest_refused() {
        for accepted in [
            json!({ "model": "m", "input": "hi", "parallel_tool_calls": true }),
            json!({ "model": "m", "input": "hi", "text": {} }),
            json!({ "model": "m", "input": "hi", "text": Value::Null }),
            json!({ "model": "m", "input": "hi",
                    "text": { "format": { "type": "text" } } }),
            json!({ "model": "m", "input": "hi",
                    "parallel_tool_calls": true,
                    "text": { "format": { "type": "text" }, "verbosity": Value::Null } }),
        ] {
            assert!(parse(accepted.clone()).is_ok(), "must accept {accepted}");
        }
        for (refused, named) in [
            (
                json!({ "model": "m", "input": "hi", "parallel_tool_calls": false }),
                "parallel_tool_calls=false",
            ),
            (
                json!({ "model": "m", "input": "hi", "text": { "verbosity": "low" } }),
                "text.verbosity",
            ),
            (
                json!({ "model": "m", "input": "hi",
                        "text": { "format": { "type": "json_schema", "schema": {} } } }),
                "text.format",
            ),
            (
                json!({ "model": "m", "input": "hi", "text": "plain" }),
                "text",
            ),
        ] {
            let error = refuse(refused);
            let ApiError::UnsupportedRequestFieldsNamed { fields } = error else {
                panic!("expected the named refusal for {named}, got {error:?}");
            };
            assert_eq!(fields, named);
        }
    }

    #[test]
    fn every_refused_shape_names_what_it_refused() {
        let cases = [
            (
                json!({ "model": "m", "input": [{ "type": "reasoning", "summary": [] }] }),
                "input[].type=reasoning",
            ),
            (
                json!({ "model": "m", "input": [{ "role": "tool", "content": "x" }] }),
                "input[].role=tool",
            ),
            (
                json!({ "model": "m", "input": [{ "role": "user", "content": "x",
                                                  "encrypted_content": "z" }] }),
                "input[].encrypted_content",
            ),
            (
                json!({ "model": "m", "input": [{ "role": "user",
                    "content": [{ "type": "input_file", "file_id": "f" }] }] }),
                "input[].content[].type=input_file",
            ),
            (
                json!({ "model": "m", "input": [{ "role": "user",
                    "content": [{ "type": "input_image", "image_url": "https://x/y",
                                  "detail": "high" }] }] }),
                "input[].content[].input_image.detail=high",
            ),
            (
                json!({ "model": "m", "input": "hi", "tool_choice": "required" }),
                "tool_choice",
            ),
            (
                json!({ "model": "m", "input": "hi",
                        "tools": [{ "type": "web_search_preview" }] }),
                "tools[].type=web_search_preview",
            ),
            (
                json!({ "model": "m", "input": "hi",
                        "tools": [{ "type": "function", "name": "t", "strict": true }] }),
                "tools[].strict",
            ),
            (
                json!({ "model": "m", "input": "hi",
                        "tools": [{ "type": "function", "name": "t", "cache_control": {} }] }),
                "tools[].cache_control",
            ),
            (
                json!({ "model": "m", "input": [{ "type": "function_call_output",
                    "call_id": "c", "output": [{ "type": "output_text", "text": "x" }] }] }),
                "input[].output",
            ),
        ];
        for (body, expected) in cases {
            let error = refuse(body.clone());
            let ApiError::UnsupportedRequestFieldsNamed { fields } = error else {
                panic!("expected a named refusal for {body}, got {error:?}");
            };
            assert_eq!(fields, expected, "for {body}");
        }
    }

    #[test]
    fn the_named_field_list_is_bounded() {
        let mut body = json!({ "model": "m", "input": "hi" });
        for index in 0..40 {
            body[format!("junk{index:02}")] = json!(1);
        }
        let ApiError::UnsupportedRequestFieldsNamed { fields } = refuse(body) else {
            panic!("expected the named refusal");
        };
        assert_eq!(fields.split(", ").count(), MAX_NAMED_FIELDS + 1);
        assert!(fields.ends_with("and 28 more"), "{fields}");
    }

    #[test]
    fn an_empty_request_reaches_the_shared_validator_rather_than_being_served() {
        let (request, _) = parse(json!({ "model": "zero/x" })).expect("translates");
        assert!(request.messages.is_empty());
        assert!(
            request.validate().is_err(),
            "an input-less request must be refused by the one validator both wires share"
        );
    }
}

#[cfg(test)]
mod serialization_tests {
    use super::*;
    use crate::openai::OpenAiUsage;

    fn usage() -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens: 40,
            completion_tokens: 9,
            total_tokens: 49,
            prompt_tokens_details: None,
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"pwd"}"#.to_owned(),
            extra_content: None,
        }
    }

    #[test]
    fn a_served_completion_becomes_a_complete_response_envelope() {
        let items = items_from_completion(Some("hello".to_owned()), None, vec![call("c9")]);
        let echo = ResponsesEcho::default();
        let body = envelope(&Envelope {
            request_id: "chatcmpl-abc",
            model: "zero/test-solo",
            items: &items,
            usage: Some(usage()),
            finish_reason: Some("tool_calls"),
            echo: &echo,
            zerorouter: None,
        });
        assert_eq!(body["id"], "resp_chatcmpl-abc");
        assert_eq!(body["object"], "response");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["incomplete_details"], Value::Null);
        assert_eq!(body["model"], "zero/test-solo");
        assert_eq!(body["output"][0]["type"], "message");
        assert_eq!(body["output"][0]["id"], "msg_chatcmpl-abc_0");
        assert_eq!(body["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(body["output"][0]["content"][0]["text"], "hello");
        assert_eq!(body["output"][0]["content"][0]["annotations"], json!([]));
        assert_eq!(body["output"][1]["type"], "function_call");
        assert_eq!(body["output"][1]["call_id"], "c9");
        assert_eq!(body["output"][1]["name"], "shell");
        assert_eq!(body["output"][1]["arguments"], r#"{"command":"pwd"}"#);
        assert_eq!(body["usage"]["input_tokens"], 40);
        assert_eq!(body["usage"]["output_tokens"], 9);
        assert_eq!(body["usage"]["total_tokens"], 49);
        assert!(
            body["usage"].get("output_tokens_details").is_none(),
            "reasoning tokens are unknown to this router and stay absent, never zero"
        );
    }

    #[test]
    fn a_clipped_completion_reports_incomplete_with_its_reason() {
        let items = items_from_completion(Some("clipp".to_owned()), None, Vec::new());
        let echo = ResponsesEcho::default();
        let body = envelope(&Envelope {
            request_id: "chatcmpl-abc",
            model: "m",
            items: &items,
            usage: Some(usage()),
            finish_reason: Some("length"),
            echo: &echo,
            zerorouter: None,
        });
        assert_eq!(body["status"], "incomplete");
        assert_eq!(body["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn reasoning_content_is_carried_rather_than_dropped() {
        let items = items_from_completion(None, Some("thinking".to_owned()), Vec::new());
        assert_eq!(items, [OutputItem::Reasoning("thinking".to_owned())]);
        let echo = ResponsesEcho::default();
        let body = envelope(&Envelope {
            request_id: "chatcmpl-abc",
            model: "m",
            items: &items,
            usage: Some(usage()),
            finish_reason: Some("stop"),
            echo: &echo,
            zerorouter: None,
        });
        assert_eq!(body["output"][0]["type"], "reasoning");
        assert_eq!(body["output"][0]["summary"][0]["text"], "thinking");
    }

    #[test]
    fn the_stream_emits_the_event_sequence_a_responses_consumer_reads() {
        let mut stream = ResponsesStream::new(ResponsesEcho::default());
        let mut frames = vec![stream.created("chatcmpl-abc", "m")];
        frames.push(stream.text_delta("chatcmpl-abc", "hel"));
        frames.push(stream.text_delta("chatcmpl-abc", "lo"));
        frames.extend(stream.tool_call("chatcmpl-abc", &call("c9")));
        frames.push(stream.completed("chatcmpl-abc", "m", usage(), "tool_calls", None));

        let events: Vec<&str> = frames.iter().map(|frame| frame.event).collect();
        assert_eq!(
            events,
            [
                "response.created",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let payloads: Vec<Value> = frames
            .iter()
            .map(|frame| serde_json::from_str(&frame.data).expect("each frame is JSON"))
            .collect();
        // Every payload restates its own type and carries a monotonic
        // sequence, which is how a consumer detects a dropped frame.
        for (index, payload) in payloads.iter().enumerate() {
            assert_eq!(payload["type"], events[index]);
            assert_eq!(payload["sequence_number"], index);
        }
        assert_eq!(payloads[0]["response"]["status"], "in_progress");
        assert_eq!(payloads[1]["delta"], "hel");
        assert_eq!(payloads[1]["output_index"], 0);
        assert_eq!(payloads[3]["item"]["call_id"], "c9");
        assert_eq!(payloads[3]["output_index"], 1);
        assert_eq!(payloads[6]["item"]["arguments"], r#"{"command":"pwd"}"#);
        // The terminal carries the WHOLE accumulated answer, not just the last
        // delta, so a consumer that only reads it still gets the response.
        let terminal = &payloads[7]["response"];
        assert_eq!(terminal["status"], "completed");
        assert_eq!(terminal["output"][0]["content"][0]["text"], "hello");
        assert_eq!(terminal["output"][1]["type"], "function_call");
        assert_eq!(terminal["usage"]["output_tokens"], 9);
    }

    #[test]
    fn a_clipped_stream_terminates_on_response_incomplete() {
        let mut stream = ResponsesStream::new(ResponsesEcho::default());
        let _ = stream.created("chatcmpl-abc", "m");
        let _ = stream.text_delta("chatcmpl-abc", "clipp");
        let frame = stream.completed("chatcmpl-abc", "m", usage(), "length", None);
        assert_eq!(frame.event, "response.incomplete");
        let payload: Value = serde_json::from_str(&frame.data).expect("JSON");
        assert_eq!(payload["response"]["status"], "incomplete");
        assert_eq!(
            payload["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn reasoning_and_text_deltas_accumulate_into_separate_items() {
        let mut stream = ResponsesStream::new(ResponsesEcho::default());
        let _ = stream.reasoning_delta("chatcmpl-abc", "think");
        let _ = stream.reasoning_delta("chatcmpl-abc", "ing");
        let _ = stream.text_delta("chatcmpl-abc", "answer");
        let frame = stream.completed("chatcmpl-abc", "m", usage(), "stop", None);
        let payload: Value = serde_json::from_str(&frame.data).expect("JSON");
        let output = &payload["response"]["output"];
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "thinking");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn an_in_band_error_is_a_typed_event_carrying_the_shared_error_object() {
        let mut stream = ResponsesStream::new(ResponsesEcho::default());
        let frame = stream.error(&ApiError::InsufficientCredits);
        assert_eq!(frame.event, "error");
        let payload: Value = serde_json::from_str(&frame.data).expect("JSON");
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["code"], "insufficient_credits");
    }
}

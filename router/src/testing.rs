//! Scriptable upstream fakes for driving the request path.
//!
//! Compiled only under the `testing` feature so a production binary carries no
//! way to substitute an upstream. The injection points that accept a fake live
//! beside the types they construct — [`ProviderCandidate::with_provider`],
//! [`ProviderRoute::from_candidates`], and [`RouterState::with_injected_route`]
//! — and are gated on the same feature.
//!
//! [`ProviderCandidate::with_provider`]: crate::providers::ProviderCandidate::with_provider
//! [`ProviderRoute::from_candidates`]: crate::providers::ProviderRoute::from_candidates
//! [`RouterState::with_injected_route`]: crate::api::RouterState::with_injected_route

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
use zeroclaw_providers::traits::{
    ChatRequest, ChatResponse, ModelProvider, StreamChunk, StreamError, StreamEvent, StreamOptions,
    StreamResult, TokenUsage, ToolCall,
};

/// Failure text a [`FakeOutcome::Transport`] reports. Deliberately free of HTTP
/// status digits so the classifier (`crate::retry::is_non_retryable`) reads it
/// as retryable and the candidate burns its whole retry budget.
pub const TRANSPORT_FAILURE: &str = "connection reset by peer";

/// Failure text a [`FakeOutcome::RateLimited`] reports, shaped so the
/// classifier (`crate::retry::is_rate_limited`) reads it as a transient 429.
pub const RATE_LIMIT_FAILURE: &str = "429 Too Many Requests";

/// One scripted upstream outcome. A fake pops these in order, so a single
/// candidate can fail, be retried, and then succeed.
#[derive(Clone, Debug)]
pub enum FakeOutcome {
    /// A non-streaming completion.
    Chat {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
        /// Chain-of-thought a thinking model returns. A completion carrying
        /// only this is a non-empty response, which is what the shape label has
        /// to see.
        reasoning_content: Option<String>,
    },
    /// A transport-shaped failure that the retry loop treats as retryable.
    Transport,
    /// A 429-shaped failure that the retry loop treats as rate-limited.
    RateLimited,
    /// A failure reporting caller-chosen text, so a test can script the exact
    /// string the classifier reads — a 4xx status token, an auth phrase, a
    /// context-window phrase. The classifier only ever sees `err.to_string()`
    /// (`crate::retry`), so the text IS the upstream condition.
    Failure(&'static str),
    /// Never answers: sleeps for `Duration` and then reports a failure, so a
    /// caller-imposed deadline elapses first.
    Stall(Duration),
    /// A stream emitted step by step.
    Stream(Vec<FakeStreamStep>),
}

impl FakeOutcome {
    /// A text-only completion carrying `usage`.
    #[must_use]
    pub fn chat(text: &str, usage: TokenUsage) -> Self {
        Self::Chat {
            text: Some(text.to_owned()),
            tool_calls: Vec::new(),
            usage: Some(usage),
            reasoning_content: None,
        }
    }

    /// A completion whose entire answer is reasoning content — the shape a
    /// thinking model returns, and the one that used to read as empty output.
    #[must_use]
    pub fn reasoning_only(reasoning: &str, usage: TokenUsage) -> Self {
        Self::Chat {
            text: None,
            tool_calls: Vec::new(),
            usage: Some(usage),
            reasoning_content: Some(reasoning.to_owned()),
        }
    }
}

/// One step of a scripted stream. `Stall` yields nothing and only delays the
/// steps behind it, which is how a stream is made to outlive a deadline.
#[derive(Clone, Debug)]
pub enum FakeStreamStep {
    Text(String),
    /// A reasoning delta. Carries no `token_count`: the provider trait's
    /// estimate is `delta.len()/4` over the CONTENT delta only, so a reasoning
    /// chunk contributes nothing to the output floor — which is exactly why the
    /// shape label cannot be derived from token counts.
    Reasoning(String),
    Tool(ToolCall),
    Usage(TokenUsage),
    Error(String),
    Stall(Duration),
    Final,
}

impl FakeStreamStep {
    /// A text delta carrying the per-chunk token estimate a provider reports
    /// when the caller asked for token counts.
    #[must_use]
    pub fn text(delta: &str) -> Self {
        Self::Text(delta.to_owned())
    }

    /// A reasoning delta.
    #[must_use]
    pub fn reasoning(delta: &str) -> Self {
        Self::Reasoning(delta.to_owned())
    }
}

/// What a fake was asked for on one call.
#[derive(Clone, Debug, PartialEq)]
pub struct FakeCall {
    pub model: String,
    pub temperature: Option<f64>,
    pub message_count: usize,
    pub tool_count: usize,
    pub streaming: bool,
}

/// A [`ModelProvider`] that answers from a script and records what it was
/// asked for, so a test can assert that dispatch happened and with what.
pub struct FakeModelProvider {
    alias: String,
    supports_streaming: bool,
    script: Mutex<VecDeque<FakeOutcome>>,
    calls: Mutex<Vec<FakeCall>>,
}

impl FakeModelProvider {
    /// A streaming-capable fake that answers `script` in order.
    #[must_use]
    pub fn new(alias: &str, script: Vec<FakeOutcome>) -> Arc<Self> {
        Self::build(alias, script, true)
    }

    /// A fake that reports `supports_streaming() == false`, so a streaming
    /// request against it takes the router's synthetic-stream path.
    #[must_use]
    pub fn without_streaming(alias: &str, script: Vec<FakeOutcome>) -> Arc<Self> {
        Self::build(alias, script, false)
    }

    fn build(alias: &str, script: Vec<FakeOutcome>, supports_streaming: bool) -> Arc<Self> {
        Arc::new(Self {
            alias: alias.to_owned(),
            supports_streaming,
            script: Mutex::new(VecDeque::from(script)),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Every call this fake received, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls
            .lock()
            .expect("fake call log must not be poisoned")
            .clone()
    }

    /// How many times this fake was dispatched to.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls
            .lock()
            .expect("fake call log must not be poisoned")
            .len()
    }

    /// Record one call and take the next scripted outcome.
    fn take(
        &self,
        request: &ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        streaming: bool,
    ) -> Option<FakeOutcome> {
        self.calls
            .lock()
            .expect("fake call log must not be poisoned")
            .push(FakeCall {
                model: model.to_owned(),
                temperature,
                message_count: request.messages.len(),
                tool_count: request.tools.map_or(0, <[_]>::len),
                streaming,
            });
        self.script
            .lock()
            .expect("fake script must not be poisoned")
            .pop_front()
    }

    /// A script mismatch is a test bug, not an upstream condition. Report it
    /// with a 4xx marker so the retry loop treats it as non-retryable and the
    /// failing test does not also pay the backoff. The marker works through the
    /// digit scan in `crate::retry::is_non_retryable`, which reads `400` out of
    /// the message text.
    fn misuse(&self, detail: &str) -> String {
        format!("400 fake upstream {}: {detail}", self.alias)
    }
}

#[async_trait]
impl ModelProvider for FakeModelProvider {
    fn supports_streaming(&self) -> bool {
        self.supports_streaming
    }

    fn supports_streaming_tool_events(&self) -> bool {
        self.supports_streaming
    }

    /// The router never dispatches through this rung — it always calls `chat`
    /// or `stream_chat` — so reaching it means the fake was wired somewhere
    /// unexpected.
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            self.misuse("dispatched through chat_with_system")
        ))
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        match self.take(&request, model, temperature, false) {
            Some(FakeOutcome::Chat {
                text,
                tool_calls,
                usage,
                reasoning_content,
            }) => Ok(ChatResponse {
                text,
                tool_calls,
                usage,
                reasoning_content,
            }),
            Some(FakeOutcome::Transport) => Err(anyhow::anyhow!(TRANSPORT_FAILURE)),
            Some(FakeOutcome::RateLimited) => Err(anyhow::anyhow!(RATE_LIMIT_FAILURE)),
            Some(FakeOutcome::Failure(detail)) => Err(anyhow::anyhow!(detail)),
            Some(FakeOutcome::Stall(duration)) => {
                tokio::time::sleep(duration).await;
                Err(anyhow::anyhow!(TRANSPORT_FAILURE))
            }
            Some(FakeOutcome::Stream(_)) => Err(anyhow::anyhow!(self.misuse("scripted to stream"))),
            None => Err(anyhow::anyhow!(self.misuse("script exhausted"))),
        }
    }

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        _options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>> {
        let steps = match self.take(&request, model, temperature, true) {
            Some(FakeOutcome::Stream(steps)) => steps,
            Some(FakeOutcome::Stall(duration)) => vec![FakeStreamStep::Stall(duration)],
            Some(FakeOutcome::Transport) => {
                vec![FakeStreamStep::Error(TRANSPORT_FAILURE.to_owned())]
            }
            Some(FakeOutcome::RateLimited) => {
                vec![FakeStreamStep::Error(RATE_LIMIT_FAILURE.to_owned())]
            }
            Some(FakeOutcome::Failure(detail)) => {
                vec![FakeStreamStep::Error(detail.to_owned())]
            }
            Some(FakeOutcome::Chat { .. }) => {
                vec![FakeStreamStep::Error(self.misuse("scripted non-streaming"))]
            }
            None => vec![FakeStreamStep::Error(self.misuse("script exhausted"))],
        };
        stream::unfold(VecDeque::from(steps), |mut steps| async move {
            loop {
                let event = match steps.pop_front()? {
                    FakeStreamStep::Stall(duration) => {
                        tokio::time::sleep(duration).await;
                        continue;
                    }
                    FakeStreamStep::Text(delta) => Ok(StreamEvent::TextDelta(
                        StreamChunk::delta(delta).with_token_estimate(),
                    )),
                    FakeStreamStep::Reasoning(delta) => {
                        Ok(StreamEvent::TextDelta(StreamChunk::reasoning(delta)))
                    }
                    FakeStreamStep::Tool(call) => Ok(StreamEvent::ToolCall(call)),
                    FakeStreamStep::Usage(usage) => Ok(StreamEvent::Usage(usage)),
                    FakeStreamStep::Final => Ok(StreamEvent::Final),
                    FakeStreamStep::Error(detail) => Err(StreamError::Http(detail)),
                };
                return Some((event, steps));
            }
        })
        .boxed()
    }
}

impl Attributable for FakeModelProvider {
    fn role(&self) -> Role {
        // The same role the OpenAI-compatible upstream this stands in for
        // reports, so attribution spans are shaped identically.
        Role::Provider(ProviderKind::Model(ModelProviderKind::Plugin))
    }

    fn alias(&self) -> &str {
        &self.alias
    }
}

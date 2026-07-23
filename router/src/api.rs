use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response, Sse, sse::Event, sse::KeepAlive},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use zeroclaw_api::model_provider::BASELINE_MAX_TOKENS;
use zeroclaw_providers::{
    pricing::ModelRates,
    traits::{ChatRequest, ChatResponse, StreamEvent, StreamOptions},
};

use crate::{
    auth::{AuthenticatedKey, AuthenticationError, KeyAuthenticator},
    config::{ResolvedRoute, TierCandidate, load_tier_catalog},
    db::{UsageAdmission, UsageRecord, UsageSession, begin_usage_session},
    error::{ApiError, streaming_error_json},
    openai::{
        ChatCompletionRequest, ChatCompletionResponse, ModelList, OpenAiUsage, StreamMetadata,
        finish_reason, stream_delta_json, stream_tool_call_delta, stream_usage_json, usage_cost,
    },
    providers::{ProviderCandidate, ProviderRoute},
    sqlx::PgPool,
};

const SSE_CHANNEL_CAPACITY: usize = 32;
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const SSE_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct RouterState {
    tier_config_path: Arc<PathBuf>,
    services: Option<Arc<RouterServices>>,
}

struct RouterServices {
    pool: PgPool,
    authenticator: KeyAuthenticator,
    runtime: RuntimeControl,
    require_credits: bool,
}

#[derive(Clone)]
struct RuntimeControl {
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl RuntimeControl {
    fn new() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }
}

impl RouterState {
    /// Construct a public-surface-only state for health/catalog tests.
    #[must_use]
    pub fn new(tier_config_path: impl Into<PathBuf>) -> Self {
        Self {
            tier_config_path: Arc::new(tier_config_path.into()),
            services: None,
        }
    }

    #[must_use]
    pub fn with_database(
        tier_config_path: impl Into<PathBuf>,
        pool: PgPool,
        require_credits: bool,
    ) -> Self {
        Self {
            tier_config_path: Arc::new(tier_config_path.into()),
            services: Some(Arc::new(RouterServices {
                pool,
                authenticator: KeyAuthenticator::new(),
                runtime: RuntimeControl::new(),
                require_credits,
            })),
        }
    }

    #[must_use]
    pub fn tier_config_path(&self) -> &Path {
        self.tier_config_path.as_path()
    }

    fn services(&self) -> Result<&RouterServices, ApiError> {
        self.services
            .as_deref()
            .ok_or(ApiError::DatabaseUnavailable)
    }

    pub fn begin_shutdown(&self) {
        if let Some(services) = &self.services {
            services.runtime.shutdown.cancel();
        }
    }

    pub async fn wait_for_background_tasks(&self) {
        if let Some(services) = &self.services {
            services.runtime.tasks.close();
            services.runtime.tasks.wait().await;
        }
    }
}

pub fn app(state: RouterState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn list_models(State(state): State<RouterState>) -> Result<Json<ModelList>, ApiError> {
    let catalog = load_tier_catalog(state.tier_config_path())
        .await
        .map_err(|_| ApiError::TierCatalogUnavailable)?;
    Ok(Json(ModelList::from_owners(catalog.model_owners())))
}

async fn chat_completions(
    State(state): State<RouterState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let services = state.services()?;
    let token = bearer_token(&headers).ok_or(ApiError::Unauthorized)?;
    let authenticated = services
        .authenticator
        .authenticate(&services.pool, token)
        .await
        .map_err(authentication_error)?;

    let payload = to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let mut request = serde_json::from_slice::<ChatCompletionRequest>(&payload)
        .map_err(|_| ApiError::InvalidRequest)?;
    request.validate().map_err(|_| ApiError::InvalidRequest)?;
    if request.contains_cache_control() {
        return Err(ApiError::CacheControlUnsupported);
    }
    if request.contains_unsupported_extensions() {
        return Err(ApiError::UnsupportedRequestFields);
    }
    let catalog = load_tier_catalog(state.tier_config_path())
        .await
        .map_err(|_| ApiError::TierCatalogUnavailable)?;
    let resolved = catalog
        .resolve(&request.model)
        .ok_or(ApiError::ModelNotFound)?;
    let max_output_tokens = *request.max_tokens.get_or_insert(BASELINE_MAX_TOKENS);
    let provider_route = ProviderRoute::new(resolved.candidates.clone(), max_output_tokens)
        .map_err(|_| ApiError::NoProviderAvailable)?;
    let reservation_usage = request.reservation_usage(max_output_tokens);
    let reserved_tokens =
        i64::try_from(reservation_usage.total_tokens).map_err(|_| ApiError::InvalidRequest)?;
    let reserved_cost = usage_cost(resolved.sell_rates, reservation_usage);
    let usage_session = admit_usage(
        &services.pool,
        &authenticated,
        reserved_tokens,
        reserved_cost,
        services.require_credits,
    )
    .await?;
    let runtime = services.runtime.clone();

    if request.stream {
        streaming_response(
            runtime,
            usage_session,
            request,
            resolved,
            provider_route,
            reservation_usage,
        )
    } else {
        non_streaming_response(
            runtime,
            usage_session,
            request,
            resolved,
            provider_route,
            reservation_usage,
        )
        .await
    }
}

async fn non_streaming_response(
    runtime: RuntimeControl,
    usage_session: UsageSession,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    provider_route: ProviderRoute,
    reservation_usage: OpenAiUsage,
) -> Result<Response, ApiError> {
    runtime
        .tasks
        .spawn(run_non_streaming(
            runtime.shutdown,
            usage_session,
            request,
            resolved,
            provider_route,
            reservation_usage,
        ))
        .await
        .map_err(|_| ApiError::UpstreamUnavailable)?
}

async fn run_non_streaming(
    shutdown: CancellationToken,
    usage_session: UsageSession,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    provider_route: ProviderRoute,
    reservation_usage: OpenAiUsage,
) -> Result<Response, ApiError> {
    let request_id = usage_session.request_id();
    let messages = request.provider_messages();
    let tools = request.provider_tools();
    let max_tokens = request.max_tokens;
    let provider_request = ChatRequest {
        messages: &messages,
        tools: (!tools.is_empty()).then_some(tools.as_slice()),
        thinking: None,
    };
    let started = Instant::now();

    let selected = tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            persist_usage(
                usage_session,
                &resolved.requested_model,
                "fallback-chain",
                &resolved.requested_model,
                reservation_usage,
                resolved.sell_rates,
                started,
                503,
            )
            .await?;
            return Err(ApiError::ServerShuttingDown);
        }
        result = tokio::time::timeout(
            UPSTREAM_REQUEST_TIMEOUT,
            provider_route.chat(
                provider_request,
                &resolved.requested_model,
                request.temperature,
            ),
        ) => result,
    };

    let selected = match selected {
        Ok(Ok(selected)) => selected,
        Ok(Err(_)) => {
            persist_usage(
                usage_session,
                &resolved.requested_model,
                "fallback-chain",
                &resolved.requested_model,
                reservation_usage,
                resolved.sell_rates,
                started,
                502,
            )
            .await?;
            tracing::warn!(
                request_id,
                requested_model = resolved.requested_model,
                "all upstream candidates failed"
            );
            return Err(ApiError::UpstreamUnavailable);
        }
        Err(_) => {
            persist_usage(
                usage_session,
                &resolved.requested_model,
                "fallback-chain",
                &resolved.requested_model,
                reservation_usage,
                resolved.sell_rates,
                started,
                504,
            )
            .await?;
            tracing::warn!(
                request_id,
                requested_model = resolved.requested_model,
                "upstream inference deadline exceeded"
            );
            return Err(ApiError::UpstreamTimeout);
        }
    };

    let Some(usage) = OpenAiUsage::try_from_provider(selected.response.usage.as_ref()) else {
        persist_usage(
            usage_session,
            &resolved.requested_model,
            &selected.candidate.provider,
            &selected.candidate.model,
            reservation_usage,
            resolved.sell_rates,
            started,
            502,
        )
        .await?;
        return Err(ApiError::MeteringUnavailable);
    };
    persist_usage(
        usage_session,
        &resolved.requested_model,
        &selected.candidate.provider,
        &selected.candidate.model,
        usage,
        resolved.sell_rates,
        started,
        200,
    )
    .await?;
    tracing::info!(
        request_id,
        requested_model = resolved.requested_model,
        upstream_provider = selected.candidate.provider,
        upstream_model = selected.candidate.model,
        input_tokens = usage.prompt_tokens,
        cached_input_tokens = usage.cached_input_tokens(),
        output_tokens = usage.completion_tokens,
        "chat completion served"
    );

    let response = ChatCompletionResponse::new(
        request_id.clone(),
        resolved.requested_model,
        selected.response,
        usage,
        max_tokens,
    );
    let mut response = Json(response).into_response();
    insert_header(&mut response, "x-request-id", &request_id);
    insert_header(
        &mut response,
        "x-zerorouter-provider",
        &selected.candidate.provider,
    );
    insert_header(
        &mut response,
        "x-zerorouter-model",
        &selected.candidate.model,
    );
    Ok(response)
}

fn streaming_response(
    runtime: RuntimeControl,
    usage_session: UsageSession,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    provider_route: ProviderRoute,
    reservation_usage: OpenAiUsage,
) -> Result<Response, ApiError> {
    let metadata = StreamMetadata::new(
        usage_session.request_id(),
        resolved.requested_model.clone(),
        request.include_stream_usage(),
    );
    let response_request_id = metadata.request_id.clone();
    let (sender, receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);

    runtime.tasks.spawn(async move {
        stream_to_channel(
            sender,
            runtime.shutdown,
            usage_session,
            metadata,
            request,
            resolved,
            provider_route.into_candidates(),
            reservation_usage,
        )
        .await;
    });

    let stream = ReceiverStream::new(receiver).map(Ok::<_, Infallible>);
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(SSE_KEEPALIVE_INTERVAL)
                .text("ping"),
        )
        .into_response();
    insert_header(&mut response, "x-request-id", &response_request_id);
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
async fn stream_to_channel(
    sender: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    usage_session: UsageSession,
    metadata: StreamMetadata,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    candidates: Vec<ProviderCandidate>,
    reservation_usage: OpenAiUsage,
) {
    let messages = request.provider_messages();
    let tools = request.provider_tools();
    let max_tokens = request.max_tokens;
    let started = Instant::now();
    let mut last_candidate = None;
    let mut usage_session = Some(usage_session);
    let mut client_connected = true;
    let mut client_output_sent = false;

    for candidate in &candidates {
        if sender.is_closed() {
            if let Some(session) = usage_session.take() {
                let usage = if last_candidate.is_some() {
                    reservation_usage
                } else {
                    OpenAiUsage::default()
                };
                let (upstream_provider, upstream_model) = last_candidate.map_or(
                    ("none", "client-disconnected"),
                    |candidate: &TierCandidate| {
                        (candidate.provider.as_str(), candidate.model.as_str())
                    },
                );
                let _ = persist_usage(
                    session,
                    &resolved.requested_model,
                    upstream_provider,
                    upstream_model,
                    usage,
                    resolved.sell_rates,
                    started,
                    499,
                )
                .await;
            }
            return;
        }
        let Some(remaining) = remaining_upstream_time(started) else {
            settle_stream_interruption(
                &sender,
                &mut usage_session,
                &metadata,
                &resolved,
                last_candidate,
                None,
                reservation_usage,
                started,
                client_connected,
                StreamInterruption::Timeout,
            )
            .await;
            return;
        };
        let provider_request = ChatRequest {
            messages: &messages,
            tools: (!tools.is_empty()).then_some(tools.as_slice()),
            thinking: None,
        };

        if !candidate.supports_streaming() {
            let candidate_started = AtomicBool::new(false);
            let result = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    let interrupted_candidate = if candidate_started.load(Ordering::Relaxed) {
                        Some(candidate.definition())
                    } else {
                        last_candidate
                    };
                    settle_stream_interruption(
                        &sender,
                        &mut usage_session,
                        &metadata,
                        &resolved,
                        interrupted_candidate,
                        None,
                        reservation_usage,
                        started,
                        client_connected,
                        StreamInterruption::Shutdown,
                    )
                    .await;
                    return;
                }
                result = async {
                    candidate_started.store(true, Ordering::Relaxed);
                    tokio::time::timeout(
                        remaining,
                        candidate.chat(provider_request, request.temperature),
                    )
                    .await
                } => result,
            };
            last_candidate = Some(candidate.definition());
            match result {
                Ok(Ok(response)) => {
                    complete_synthetic_stream(
                        &sender,
                        &mut usage_session,
                        &metadata,
                        &resolved,
                        candidate,
                        response,
                        max_tokens,
                        reservation_usage,
                        started,
                    )
                    .await;
                    return;
                }
                Ok(Err(_)) => continue,
                Err(_) => {
                    settle_stream_interruption(
                        &sender,
                        &mut usage_session,
                        &metadata,
                        &resolved,
                        Some(candidate.definition()),
                        None,
                        reservation_usage,
                        started,
                        client_connected,
                        StreamInterruption::Timeout,
                    )
                    .await;
                    return;
                }
            }
        }

        let mut stream = candidate.stream_chat(
            provider_request,
            request.temperature,
            StreamOptions::new(true).with_token_count(),
        );
        last_candidate = Some(candidate.definition());
        let mut role_sent = false;
        let mut usage = None;
        let mut has_tool_calls = false;
        let mut tool_index = 0_u32;
        let mut completed = false;
        let mut interruption = None;

        loop {
            let Some(remaining) = remaining_upstream_time(started) else {
                interruption = Some(StreamInterruption::Timeout);
                break;
            };
            let event = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    interruption = Some(StreamInterruption::Shutdown);
                    None
                }
                result = tokio::time::timeout(remaining, stream.next()) => match result {
                    Ok(event) => event,
                    Err(_) => {
                        interruption = Some(StreamInterruption::Timeout);
                        None
                    }
                },
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Ok(StreamEvent::TextDelta(chunk)) => {
                    if chunk.delta.is_empty() && chunk.reasoning.as_deref().unwrap_or("").is_empty()
                    {
                        continue;
                    }
                    if !client_connected {
                        continue;
                    }
                    let role_was_sent = role_sent;
                    role_sent = ensure_stream_role(&sender, &metadata, role_sent).await;
                    client_output_sent |= !role_was_sent && role_sent;
                    if !role_sent {
                        client_connected = false;
                        continue;
                    }
                    let mut delta = serde_json::Map::new();
                    if !chunk.delta.is_empty() {
                        delta.insert("content".to_owned(), Value::String(chunk.delta));
                    }
                    if let Some(reasoning) = chunk.reasoning {
                        delta.insert("reasoning_content".to_owned(), Value::String(reasoning));
                    }
                    let delivered = send_data(
                        &sender,
                        stream_delta_json(&metadata, Value::Object(delta), None),
                    )
                    .await;
                    client_connected &= delivered;
                    client_output_sent |= delivered;
                }
                Ok(StreamEvent::ToolCall(call)) => {
                    has_tool_calls = true;
                    if !client_connected {
                        tool_index = tool_index.saturating_add(1);
                        continue;
                    }
                    let role_was_sent = role_sent;
                    role_sent = ensure_stream_role(&sender, &metadata, role_sent).await;
                    client_output_sent |= !role_was_sent && role_sent;
                    if !role_sent {
                        client_connected = false;
                        tool_index = tool_index.saturating_add(1);
                        continue;
                    }
                    let delivered = send_data(
                        &sender,
                        stream_delta_json(
                            &metadata,
                            stream_tool_call_delta(call, tool_index),
                            None,
                        ),
                    )
                    .await;
                    tool_index = tool_index.saturating_add(1);
                    client_connected &= delivered;
                    client_output_sent |= delivered;
                }
                Ok(StreamEvent::Usage(provider_usage)) => {
                    usage = OpenAiUsage::try_from_provider(Some(&provider_usage));
                }
                Ok(StreamEvent::Final) => {
                    completed = true;
                    break;
                }
                Ok(StreamEvent::PreExecutedToolCall { .. })
                | Ok(StreamEvent::PreExecutedToolResult { .. }) => {}
                Err(_) => break,
            }
        }

        if let Some(interruption) = interruption {
            settle_stream_interruption(
                &sender,
                &mut usage_session,
                &metadata,
                &resolved,
                Some(candidate.definition()),
                usage,
                reservation_usage,
                started,
                client_connected,
                interruption,
            )
            .await;
            return;
        }

        if completed {
            if client_connected {
                let role_delivered = ensure_stream_role(&sender, &metadata, role_sent).await;
                client_connected &= role_delivered;
            }
            finish_successful_stream(
                &sender,
                &mut usage_session,
                &metadata,
                &resolved,
                candidate,
                usage,
                has_tool_calls,
                max_tokens,
                reservation_usage,
                if client_connected && !sender.is_closed() {
                    200
                } else {
                    499
                },
                started,
            )
            .await;
            return;
        }

        client_connected &= !sender.is_closed();
        if client_output_sent || !client_connected {
            let Some(session) = usage_session.take() else {
                send_stream_error(&sender, &ApiError::MeteringUnavailable).await;
                return;
            };
            let metering = persist_usage(
                session,
                &resolved.requested_model,
                &candidate.definition().provider,
                &candidate.definition().model,
                usage.unwrap_or(reservation_usage),
                resolved.sell_rates,
                started,
                if client_connected { 502 } else { 499 },
            )
            .await;
            let error = if metering.is_ok() {
                ApiError::UpstreamUnavailable
            } else {
                ApiError::MeteringUnavailable
            };
            if client_connected {
                send_stream_error(&sender, &error).await;
            }
            return;
        }
    }

    let error = if let (Some(candidate), Some(session)) = (last_candidate, usage_session.take()) {
        match persist_usage(
            session,
            &resolved.requested_model,
            &candidate.provider,
            &candidate.model,
            reservation_usage,
            resolved.sell_rates,
            started,
            502,
        )
        .await
        {
            Ok(()) => ApiError::UpstreamUnavailable,
            Err(_) => ApiError::MeteringUnavailable,
        }
    } else {
        ApiError::NoProviderAvailable
    };
    send_stream_error(&sender, &error).await;
}

fn remaining_upstream_time(started: Instant) -> Option<Duration> {
    UPSTREAM_REQUEST_TIMEOUT
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
}

#[derive(Clone, Copy)]
enum StreamInterruption {
    Timeout,
    Shutdown,
}

impl StreamInterruption {
    fn status(self) -> i16 {
        match self {
            Self::Timeout => 504,
            Self::Shutdown => 503,
        }
    }

    fn api_error(self) -> ApiError {
        match self {
            Self::Timeout => ApiError::UpstreamTimeout,
            Self::Shutdown => ApiError::ServerShuttingDown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Shutdown => "shutdown",
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn settle_stream_interruption(
    sender: &mpsc::Sender<Event>,
    usage_session: &mut Option<UsageSession>,
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: Option<&TierCandidate>,
    usage: Option<OpenAiUsage>,
    reservation_usage: OpenAiUsage,
    started: Instant,
    client_connected: bool,
    interruption: StreamInterruption,
) {
    let client_connected = client_connected && !sender.is_closed();
    let (upstream_provider, upstream_model) = candidate.map_or(
        ("fallback-chain", resolved.requested_model.as_str()),
        |candidate| (candidate.provider.as_str(), candidate.model.as_str()),
    );
    let error = if let Some(session) = usage_session.take() {
        match persist_usage(
            session,
            &resolved.requested_model,
            upstream_provider,
            upstream_model,
            usage.unwrap_or(reservation_usage),
            resolved.sell_rates,
            started,
            if client_connected {
                interruption.status()
            } else {
                499
            },
        )
        .await
        {
            Ok(()) => interruption.api_error(),
            Err(_) => ApiError::MeteringUnavailable,
        }
    } else {
        ApiError::MeteringUnavailable
    };
    tracing::warn!(
        request_id = metadata.request_id,
        requested_model = resolved.requested_model,
        upstream_provider,
        upstream_model,
        interruption = interruption.label(),
        "streaming inference interrupted"
    );
    if client_connected {
        send_stream_error(sender, &error).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn complete_synthetic_stream(
    sender: &mpsc::Sender<Event>,
    usage_session: &mut Option<UsageSession>,
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: &ProviderCandidate,
    response: ChatResponse,
    max_tokens: Option<u32>,
    reservation_usage: OpenAiUsage,
    started: Instant,
) {
    let Some(session) = usage_session.take() else {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let Some(usage) = OpenAiUsage::try_from_provider(response.usage.as_ref()) else {
        let _ = persist_usage(
            session,
            &resolved.requested_model,
            &candidate.definition().provider,
            &candidate.definition().model,
            reservation_usage,
            resolved.sell_rates,
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let completion_status = if sender.is_closed() { 499 } else { 200 };
    if persist_usage(
        session,
        &resolved.requested_model,
        &candidate.definition().provider,
        &candidate.definition().model,
        usage,
        resolved.sell_rates,
        started,
        completion_status,
    )
    .await
    .is_err()
    {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    }

    if !ensure_stream_role(sender, metadata, false).await {
        return;
    }
    let has_tool_calls = !response.tool_calls.is_empty();
    if let Some(text) = response.text
        && !send_data(
            sender,
            stream_delta_json(metadata, json!({ "content": text }), None),
        )
        .await
    {
        return;
    }
    if let Some(reasoning) = response.reasoning_content
        && !send_data(
            sender,
            stream_delta_json(metadata, json!({ "reasoning_content": reasoning }), None),
        )
        .await
    {
        return;
    }
    for (index, call) in response.tool_calls.into_iter().enumerate() {
        if !send_data(
            sender,
            stream_delta_json(
                metadata,
                stream_tool_call_delta(call, u32::try_from(index).unwrap_or(u32::MAX)),
                None,
            ),
        )
        .await
        {
            return;
        }
    }
    emit_stream_finish(
        sender,
        metadata,
        resolved,
        candidate,
        usage,
        finish_reason(has_tool_calls, usage, max_tokens),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn finish_successful_stream(
    sender: &mpsc::Sender<Event>,
    usage_session: &mut Option<UsageSession>,
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: &ProviderCandidate,
    usage: Option<OpenAiUsage>,
    has_tool_calls: bool,
    max_tokens: Option<u32>,
    reservation_usage: OpenAiUsage,
    completion_status: i16,
    started: Instant,
) {
    let Some(session) = usage_session.take() else {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let Some(usage) = usage else {
        let _ = persist_usage(
            session,
            &resolved.requested_model,
            &candidate.definition().provider,
            &candidate.definition().model,
            reservation_usage,
            resolved.sell_rates,
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    if persist_usage(
        session,
        &resolved.requested_model,
        &candidate.definition().provider,
        &candidate.definition().model,
        usage,
        resolved.sell_rates,
        started,
        completion_status,
    )
    .await
    .is_err()
    {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    }

    emit_stream_finish(
        sender,
        metadata,
        resolved,
        candidate,
        usage,
        finish_reason(has_tool_calls, usage, max_tokens),
    )
    .await;
}

async fn emit_stream_finish(
    sender: &mpsc::Sender<Event>,
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: &ProviderCandidate,
    usage: OpenAiUsage,
    finish_reason: &'static str,
) {
    if !send_data(
        sender,
        stream_delta_json(metadata, json!({}), Some(finish_reason)),
    )
    .await
    {
        return;
    }
    if metadata.include_usage && !send_data(sender, stream_usage_json(metadata, usage)).await {
        return;
    }
    let _ = send_data(sender, "[DONE]".to_owned()).await;
    tracing::info!(
        request_id = metadata.request_id,
        requested_model = resolved.requested_model,
        upstream_provider = candidate.definition().provider,
        upstream_model = candidate.definition().model,
        input_tokens = usage.prompt_tokens,
        cached_input_tokens = usage.cached_input_tokens(),
        output_tokens = usage.completion_tokens,
        "streaming chat completion served"
    );
}

async fn ensure_stream_role(
    sender: &mpsc::Sender<Event>,
    metadata: &StreamMetadata,
    already_sent: bool,
) -> bool {
    already_sent
        || send_data(
            sender,
            stream_delta_json(
                metadata,
                json!({ "role": "assistant", "content": null }),
                None,
            ),
        )
        .await
}

async fn send_stream_error(sender: &mpsc::Sender<Event>, error: &ApiError) {
    if send_data(sender, streaming_error_json(error)).await {
        let _ = send_data(sender, "[DONE]".to_owned()).await;
    }
}

async fn send_data(sender: &mpsc::Sender<Event>, data: String) -> bool {
    tokio::time::timeout(SSE_SEND_TIMEOUT, sender.send(Event::default().data(data)))
        .await
        .is_ok_and(|result| result.is_ok())
}

async fn admit_usage(
    pool: &PgPool,
    key: &AuthenticatedKey,
    reserved_tokens: i64,
    reserved_cost: rust_decimal::Decimal,
    require_credits: bool,
) -> Result<UsageSession, ApiError> {
    match begin_usage_session(pool, key, reserved_tokens, reserved_cost, require_credits)
        .await
        .map_err(|_| ApiError::DatabaseUnavailable)?
    {
        UsageAdmission::Allowed(session) => Ok(session),
        UsageAdmission::Unauthorized => Err(ApiError::Unauthorized),
        UsageAdmission::SpendExceeded => Err(ApiError::SpendCapExceeded),
        UsageAdmission::VelocityExceeded => Err(ApiError::VelocityCapExceeded),
        UsageAdmission::InsufficientCredits => Err(ApiError::InsufficientCredits),
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_usage(
    usage_session: UsageSession,
    requested_model: &str,
    upstream_provider: &str,
    upstream_model: &str,
    usage: OpenAiUsage,
    sell_rates: ModelRates,
    started: Instant,
    status: i16,
) -> Result<(), ApiError> {
    let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
    usage_session
        .record(&UsageRecord {
            tier: requested_model.to_owned(),
            upstream_provider: upstream_provider.to_owned(),
            upstream_model: upstream_model.to_owned(),
            usage,
            cost_usd: usage_cost(sell_rates, usage),
            latency_ms,
            status,
        })
        .await
        .map_err(|_| ApiError::MeteringUnavailable)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let authorization = headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = authorization.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && !token.contains(' '))
        .then_some(token)
}

fn authentication_error(error: AuthenticationError) -> ApiError {
    match error {
        AuthenticationError::Invalid => ApiError::Unauthorized,
        AuthenticationError::Database(_) => ApiError::DatabaseUnavailable,
    }
}

fn insert_header(response: &mut Response, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_providers::traits::TokenUsage;

    #[test]
    fn bearer_parser_is_strict_and_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer zcr_test"));
        assert_eq!(bearer_token(&headers), Some("zcr_test"));

        headers.insert(
            "authorization",
            HeaderValue::from_static("bearer two words"),
        );
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn provider_usage_is_not_retokenized() {
        let usage = TokenUsage {
            input_tokens: Some(1_000),
            cached_input_tokens: Some(900),
            output_tokens: Some(20),
        };
        let normalized = OpenAiUsage::from_provider(Some(&usage));
        assert_eq!(normalized.prompt_tokens, 1_000);
        assert_eq!(normalized.cached_input_tokens(), 900);
        assert_eq!(normalized.completion_tokens, 20);
    }
}

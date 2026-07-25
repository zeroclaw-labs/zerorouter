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
use chrono::Utc;
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
    db::{
        AttemptRecord, RequestTelemetry, SettlementRecovery, UsageAdmission, UsageRecord,
        UsageSession, begin_usage_session, recover_owed_settlements,
    },
    error::{ApiError, streaming_error_json},
    openai::{
        ChatCompletionRequest, ChatCompletionResponse, ModelList, OpenAiUsage, StreamMetadata,
        finish_reason, shape_ok, stream_delta_json, stream_tool_call_delta, stream_usage_json,
        task_signature, tool_args_all_json, usage_cost,
    },
    providers::{ProviderCandidate, ProviderRoute},
    sqlx::PgPool,
};

/// Raw request-shape features captured once per request and written onto the
/// telemetry substrate at every settle site (migration 0004). Prompt content
/// is never included — only sizes and counts.
#[derive(Clone, Copy)]
struct RequestFeatures {
    requested_max_tokens: i32,
    stream: bool,
    prompt_bytes: i64,
    message_count: i32,
    tool_count: i32,
}

impl RequestFeatures {
    fn from_request(request: &ChatCompletionRequest, reservation_usage: OpenAiUsage) -> Self {
        Self {
            requested_max_tokens: request
                .max_tokens
                .map_or(0, |max| i32::try_from(max).unwrap_or(i32::MAX)),
            stream: request.stream,
            prompt_bytes: i64::try_from(reservation_usage.prompt_tokens).unwrap_or(i64::MAX),
            message_count: i32::try_from(request.messages.len()).unwrap_or(i32::MAX),
            tool_count: i32::try_from(request.tools.len()).unwrap_or(i32::MAX),
        }
    }
}

/// Build one `request_attempts` row from an in-walk candidate outcome. Cost
/// basis is the candidate's own cost-basis rate applied to whatever usage is
/// known (a per-chunk `token_count` lower bound for abandoned attempts).
#[allow(clippy::too_many_arguments)]
fn build_attempt(
    attempt_no: usize,
    candidate: &TierCandidate,
    outcome: &'static str,
    served: bool,
    attempt_started: Instant,
    usage: Option<OpenAiUsage>,
    tokens_estimated: bool,
    finish_reason: Option<&str>,
) -> AttemptRecord {
    let latency_ms = i32::try_from(attempt_started.elapsed().as_millis()).unwrap_or(i32::MAX);
    let started_at = Utc::now() - chrono::Duration::milliseconds(i64::from(latency_ms));
    let cost_basis_usd = usage.map(|usage| usage_cost(candidate.rates, usage));
    AttemptRecord {
        attempt_no: i16::try_from(attempt_no).unwrap_or(i16::MAX),
        started_at,
        candidate_id: candidate.id.clone(),
        upstream_provider: candidate.provider.clone(),
        upstream_model: candidate.model.clone(),
        outcome: outcome.to_owned(),
        served,
        latency_ms,
        usage,
        tokens_estimated,
        cost_basis_usd,
        finish_reason: finish_reason.map(str::to_owned),
        validator_kind: None,
    }
}

/// A conservative lower-bound usage from the per-chunk `token_count` a stream
/// already reports, used only to price an abandoned streaming attempt's COGS.
///
/// Deliberately carries no input side: an attempt row prices ZeroRouter's own
/// cost for output the customer may never have seen. Never bill a customer
/// with this, or with anything else estimated — customer billing runs through
/// [`StreamDelivery::settled_usage`], which bills metered actuals only.
fn estimated_stream_usage(estimated_output: u64) -> Option<OpenAiUsage> {
    (estimated_output > 0).then_some(OpenAiUsage {
        prompt_tokens: 0,
        completion_tokens: estimated_output,
        total_tokens: estimated_output,
        prompt_tokens_details: None,
    })
}

/// Emit the metering-gap alarm: the upstream produced output for this request
/// and reported no usage, so [`StreamDelivery::settled_usage`] bills nothing
/// and ZeroRouter absorbs the provider cost.
///
/// `error!`, not `warn!`, and deliberately one line per occurrence: under a
/// metered-actuals-only policy an unmetered delivery is lost revenue, and a
/// silent one is a revenue hole that grows without anyone noticing. The same
/// event is countable after the fact without a new column — see
/// [`StreamDelivery::settled_usage`] for the ledger query.
fn log_metering_gap(
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: Option<&TierCandidate>,
    output_delivered: bool,
    settle_site: &'static str,
) {
    let (candidate_id, upstream_provider, upstream_model) = candidate.map_or(
        ("none", "none", resolved.requested_model.as_str()),
        |candidate| {
            (
                candidate.id.as_str(),
                candidate.provider.as_str(),
                candidate.model.as_str(),
            )
        },
    );
    tracing::error!(
        request_id = metadata.request_id,
        requested_model = resolved.requested_model,
        candidate_id,
        upstream_provider,
        upstream_model,
        output_delivered,
        settle_site,
        "upstream reported no usage: settling unbilled"
    );
}

/// Whether a stream frame carries model output or is protocol scaffolding.
///
/// The distinction is a billing one. A customer who received only scaffolding
/// received none of what they asked for, so scaffolding can never be the reason
/// a request is charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frame {
    /// A content delta, a reasoning delta, or a tool-call delta: the thing the
    /// customer asked for, and the only thing a charge may rest on.
    ModelOutput,
    /// The role primer (`{"role":"assistant","content":null}`), the finish
    /// delta, the usage chunk, an error frame, the terminal `[DONE]`. Pure
    /// protocol envelope carrying no model output. Keep-alives are scaffolding
    /// too and never reach this enum at all — axum's `KeepAlive` layer writes
    /// them below the channel, so the walk never sees them.
    Scaffolding,
}

/// What the walk actually put on the wire for the candidate being settled.
///
/// `model_output_sent` is the outer-walk flag (`stream_to_channel`): once one
/// [`Frame::ModelOutput`] frame is accepted it stays true for the rest of the
/// request. It used to be set by *any* frame, which meant the role primer —
/// emitted before the first delta and carrying nothing but
/// `{"role":"assistant"}` — was enough to mark a request as delivered and,
/// where the upstream had already reported usage, to bill it.
///
/// # What this flag can and cannot mean
///
/// A frame is counted when `Sender::send` into the SSE channel succeeds, which
/// is **queued for the transport**, not **received by the client**. The
/// channel is a 32-slot `tokio::mpsc` drained by `ReceiverStream` inside
/// axum's `Sse` body, which hyper polls and writes to the socket. Nothing on
/// that path reports back: a successful send means a slot was free, a
/// successful hyper poll would mean the frame was handed to the socket, and
/// even a completed socket write means the bytes are in a kernel buffer — not
/// that a client read them. There is no ack at any layer this code can reach,
/// so "delivered" is not observable here and this type does not pretend
/// otherwise. What IS observable is the negative: once the client hangs up the
/// receiver drops, `send` starts failing, and `sender.is_closed()` reports it —
/// which is why every terminal consults `client_connected` as well.
///
/// The one signal strictly stronger than queueing — "the body stream yielded
/// this frame to hyper" — is obtainable by instrumenting the stream, and is
/// deliberately NOT used: the settle sites run concurrently with the body being
/// polled, so reading a consumption counter at settle time would race the
/// transport and under-bill correct deliveries. Making it non-racy means
/// settling only after the body drains, i.e. moving settlement after the
/// terminal `[DONE]` — a reordering of when a customer is charged that belongs
/// in its own change.
#[derive(Clone, Copy, Default)]
struct StreamDelivery {
    model_output_sent: bool,
}

impl StreamDelivery {
    /// Send one frame, recording it as a delivery only when it carries model
    /// output. Returns whether the channel accepted the frame.
    async fn send(&mut self, sender: &mpsc::Sender<Event>, data: String, frame: Frame) -> bool {
        let accepted = send_data(sender, data).await;
        self.model_output_sent |= accepted && frame == Frame::ModelOutput;
        accepted
    }

    /// Emit the role primer once, if it has not been emitted yet.
    ///
    /// Scaffolding: it opens the assistant message and carries no output, so it
    /// never marks the request as delivered.
    async fn ensure_role(
        &mut self,
        sender: &mpsc::Sender<Event>,
        metadata: &StreamMetadata,
        already_sent: bool,
    ) -> bool {
        already_sent
            || self
                .send(
                    sender,
                    stream_delta_json(
                        metadata,
                        json!({ "role": "assistant", "content": null }),
                        None,
                    ),
                    Frame::Scaffolding,
                )
                .await
    }

    /// The single implementation of the streaming billing policy: **bill only
    /// metered actuals, never an estimate.** A charge requires both halves —
    /// output that actually reached the client, and a usage report from the
    /// upstream. Missing either settles at zero.
    ///
    /// The `None` arm is the load-bearing one, and it is not a bug to be
    /// "fixed" by reaching for a fallback estimate. Every quantity available
    /// here is a heuristic, not a measurement: the admission reservation's
    /// `prompt_tokens` is a BYTE-LENGTH bound with fixed per-message overhead
    /// (`ChatCompletionRequest::reservation_usage`, openai.rs), and the
    /// per-chunk `token_count` lower bound is `len()/4` that adapters must opt
    /// into — it is hardcoded to zero on the Anthropic path and contributes
    /// nothing for reasoning or tool-call output. Applying per-token prices to
    /// bytes overcharges; applying them to a floor of zero undercharges. Both
    /// are guesses at a customer's bill, and a conservative guess is still a
    /// guess. If the upstream did not meter it, ZeroRouter does not bill it and
    /// eats the cost.
    ///
    /// A gap is loud (`log_metering_gap`, at `error!`) and countable in the
    /// ledger with no extra column, because a served attempt whose request was
    /// billed nothing is exactly the join of two existing migration-0004 facts:
    ///
    /// ```sql
    /// SELECT e.request_id, e.upstream_provider, e.upstream_model, e.status
    /// FROM usage_events e
    /// JOIN request_attempts a USING (request_id)
    /// WHERE a.served AND e.input_tokens = 0 AND e.output_tokens = 0;
    /// ```
    ///
    /// Zero tokens is unambiguous on the settled row: a real usage report can
    /// never be all-zero (`OpenAiUsage::try_from_provider` rejects it), and
    /// `request_attempts.served` is set only on the attempt whose output the
    /// customer received.
    fn settled_usage(self, usage: Option<OpenAiUsage>) -> OpenAiUsage {
        if !self.model_output_sent {
            return OpenAiUsage::default();
        }
        usage.unwrap_or_default()
    }
}

/// Cadence of the background settlement-recovery sweep. Well above the
/// `SETTLEMENT_RECOVERY_GRACE` an intent must age past before it is eligible,
/// so a sweep never contends with a request still retrying in-band.
const SETTLEMENT_RECOVERY_INTERVAL: Duration = Duration::from_secs(60);

/// Owed settlements one sweep pass replays. Bounded so a backlog is worked off
/// over several passes instead of monopolising the pool in one.
const SETTLEMENT_RECOVERY_BATCH: i64 = 64;

const SSE_CHANNEL_CAPACITY: usize = 32;
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const SSE_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Supplies the per-request [`ProviderRoute`] the walk runs over, standing in
/// for the credential-built one. Test-only, so a production binary has no way
/// to substitute an upstream; the arguments are exactly what the production
/// [`ProviderRoute::new`] call site passes.
#[cfg(feature = "testing")]
pub type InjectedRoute = Arc<dyn Fn(&ResolvedRoute, u32) -> ProviderRoute + Send + Sync>;

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
    #[cfg(feature = "testing")]
    injected_route: Option<InjectedRoute>,
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
                #[cfg(feature = "testing")]
                injected_route: None,
            })),
        }
    }

    /// Serve the walk over `route` instead of one built from upstream
    /// credentials. Everything else — authentication, admission, the walk, and
    /// settlement — is the production path.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_injected_route(
        tier_config_path: impl Into<PathBuf>,
        pool: PgPool,
        require_credits: bool,
        route: InjectedRoute,
    ) -> Self {
        Self {
            tier_config_path: Arc::new(tier_config_path.into()),
            services: Some(Arc::new(RouterServices {
                pool,
                authenticator: KeyAuthenticator::new(),
                runtime: RuntimeControl::new(),
                require_credits,
                injected_route: Some(route),
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

    /// Start the background settlement-recovery sweep: the durable backstop
    /// behind the bounded in-request settle retry (see
    /// [`crate::db::recover_owed_settlements`]).
    ///
    /// Opt-in, and called only by `serve`. A test harness must not start it —
    /// the loop exits only on shutdown, so
    /// [`RouterState::wait_for_background_tasks`] would block forever on a
    /// state that is never drained.
    pub fn spawn_settlement_recovery(&self) {
        let Some(services) = &self.services else {
            return;
        };
        let pool = services.pool.clone();
        let shutdown = services.runtime.shutdown.clone();
        services.runtime.tasks.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(SETTLEMENT_RECOVERY_INTERVAL) => {}
                }
                match recover_owed_settlements(&pool, SETTLEMENT_RECOVERY_BATCH).await {
                    // Silence is the healthy state: nothing owed, nothing said.
                    Ok(summary) if summary == SettlementRecovery::default() => {}
                    Ok(summary) => tracing::warn!(
                        settled = summary.settled,
                        already_settled = summary.already_settled,
                        failed = summary.failed,
                        quarantined = summary.quarantined,
                        "settlement recovery pass completed"
                    ),
                    Err(error) => tracing::error!(
                        error = %error,
                        "settlement recovery pass failed"
                    ),
                }
            }
        });
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

impl RouterServices {
    /// The provider route this request walks: built from upstream credentials,
    /// or supplied by the injected route when one is configured. Never cache
    /// the result — fallback selection metadata is request-scoped.
    fn provider_route(
        &self,
        resolved: &ResolvedRoute,
        max_output_tokens: u32,
    ) -> Result<ProviderRoute, ApiError> {
        #[cfg(feature = "testing")]
        if let Some(route) = &self.injected_route {
            return Ok(route(resolved, max_output_tokens));
        }
        ProviderRoute::new(resolved.candidates.clone(), max_output_tokens)
            .map_err(|_| ApiError::NoProviderAvailable)
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
    Ok(Json(ModelList::from_listing(catalog.model_listing())))
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
    let resolved = match catalog.resolve(&request.model) {
        Some(resolved) => resolved,
        // Absent from the servable catalog means one of two very different
        // things. Either the id does not exist (the caller's mistake, 404), or
        // it exists in a tier withheld for below-cost pricing — ZeroRouter's
        // mistake, which the caller cannot fix and must not be told is a
        // missing model.
        None => {
            return Err(catalog.unavailable_for(&request.model).map_or(
                ApiError::ModelNotFound,
                |withheld| ApiError::ModelUnavailable {
                    tier: withheld.tier.clone(),
                },
            ));
        }
    };
    let max_output_tokens = *request.max_tokens.get_or_insert(BASELINE_MAX_TOKENS);
    let provider_route = services.provider_route(&resolved, max_output_tokens)?;
    let reservation_usage = request.reservation_usage(max_output_tokens);
    let reserved_tokens =
        i64::try_from(reservation_usage.total_tokens).map_err(|_| ApiError::InvalidRequest)?;
    let reserved_cost = usage_cost(resolved.sell_rates, reservation_usage);
    // The user-scoped segmentation key (design: Engine "Task signature"),
    // computed beside the reservation over the same request-shape fields.
    let tool_names: Vec<String> = request
        .tools
        .iter()
        .map(|tool| tool.function.name.clone())
        .collect();
    let signature = task_signature(
        &authenticated.user_id.to_string(),
        &tool_names,
        request.messages.len(),
        reservation_usage.prompt_tokens,
        request.stream,
        max_output_tokens,
    );
    let usage_session = admit_usage(
        &services.pool,
        &authenticated,
        reserved_tokens,
        i64::from(max_output_tokens),
        reserved_cost,
        signature,
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
    let features = RequestFeatures::from_request(&request, reservation_usage);
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
            // No tokens were delivered; release the reservation without a
            // charge rather than billing the conservative estimate.
            persist_usage(
                usage_session,
                &resolved.requested_model,
                "fallback-chain",
                &resolved.requested_model,
                None,
                OpenAiUsage::default(),
                resolved.sell_rates,
                features,
                None,
                None,
                Vec::new(),
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
            // Every candidate failed and no tokens were delivered; release the
            // reservation without a charge.
            persist_usage(
                usage_session,
                &resolved.requested_model,
                "fallback-chain",
                &resolved.requested_model,
                None,
                OpenAiUsage::default(),
                resolved.sell_rates,
                features,
                None,
                None,
                Vec::new(),
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
            // The deadline elapsed with no delivered tokens; release without a
            // charge.
            persist_usage(
                usage_session,
                &resolved.requested_model,
                "fallback-chain",
                &resolved.requested_model,
                None,
                OpenAiUsage::default(),
                resolved.sell_rates,
                features,
                None,
                None,
                Vec::new(),
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
            Some(selected.candidate),
            reservation_usage,
            resolved.sell_rates,
            features,
            None,
            None,
            Vec::new(),
            started,
            502,
        )
        .await?;
        return Err(ApiError::MeteringUnavailable);
    };
    let has_tools = !selected.response.tool_calls.is_empty();
    let synthesized_finish = finish_reason(has_tools, usage, max_tokens);
    let output_nonempty = selected
        .response
        .text
        .as_deref()
        .is_some_and(|text| !text.is_empty())
        || has_tools;
    let shape_label = shape_ok(
        output_nonempty,
        tool_args_all_json(&selected.response.tool_calls),
        synthesized_finish,
    );
    persist_usage(
        usage_session,
        &resolved.requested_model,
        &selected.candidate.provider,
        &selected.candidate.model,
        Some(selected.candidate),
        usage,
        resolved.sell_rates,
        features,
        Some(synthesized_finish),
        Some(shape_label),
        Vec::new(),
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
    let features = RequestFeatures::from_request(&request, reservation_usage);
    let started = Instant::now();
    let mut last_candidate = None;
    let mut usage_session = Some(usage_session);
    let mut client_connected = true;
    let mut delivery = StreamDelivery::default();
    // The router-owned walk ledger: one row per candidate tried, drained into
    // the settle transaction at whichever terminal settles this request.
    let mut attempts: Vec<AttemptRecord> = Vec::new();

    for candidate in &candidates {
        if sender.is_closed() {
            if let Some(session) = usage_session.take() {
                // The walk only re-enters this loop after a candidate that
                // delivered nothing, and no usage report survives from it, so
                // there is no metered actual to bill — not even when a
                // candidate was already tried. This settled at the reservation
                // bound (a 4096-token output estimate) before the policy became
                // metered-actuals-only.
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
                    last_candidate,
                    OpenAiUsage::default(),
                    resolved.sell_rates,
                    features,
                    None,
                    None,
                    std::mem::take(&mut attempts),
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
                features,
                std::mem::take(&mut attempts),
                started,
                client_connected,
                // The delivery flag is passed rather than assumed, but it is
                // necessarily false here: every terminal that can follow a
                // delivery returns, so the only way the walk reaches another
                // iteration is with nothing delivered.
                delivery,
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
        let attempt_started = Instant::now();
        let attempt_no = attempts.len() + 1;

        if !candidate.supports_streaming() {
            let candidate_started = AtomicBool::new(false);
            let result = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    let interrupted_candidate = if candidate_started.load(Ordering::Relaxed) {
                        attempts.push(build_attempt(
                            attempt_no,
                            candidate.definition(),
                            "aborted",
                            false,
                            attempt_started,
                            None,
                            false,
                            None,
                        ));
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
                        features,
                        std::mem::take(&mut attempts),
                        started,
                        client_connected,
                        // This candidate cannot stream: the only site that
                        // writes anything for it is `complete_synthetic_stream`,
                        // which runs after `chat` returns and is not reached
                        // here, so nothing has been delivered for it.
                        delivery,
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
                        &mut delivery,
                        &metadata,
                        &resolved,
                        candidate,
                        response,
                        max_tokens,
                        features,
                        attempts,
                        attempt_no,
                        attempt_started,
                        started,
                    )
                    .await;
                    return;
                }
                Ok(Err(_)) => {
                    attempts.push(build_attempt(
                        attempt_no,
                        candidate.definition(),
                        "upstream_error",
                        false,
                        attempt_started,
                        None,
                        false,
                        None,
                    ));
                    continue;
                }
                Err(_) => {
                    attempts.push(build_attempt(
                        attempt_no,
                        candidate.definition(),
                        "timeout",
                        false,
                        attempt_started,
                        None,
                        false,
                        None,
                    ));
                    settle_stream_interruption(
                        &sender,
                        &mut usage_session,
                        &metadata,
                        &resolved,
                        Some(candidate.definition()),
                        None,
                        features,
                        std::mem::take(&mut attempts),
                        started,
                        client_connected,
                        // The non-streaming call timed out, so
                        // `complete_synthetic_stream` never ran and nothing was
                        // written for this candidate.
                        delivery,
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
        // Per-chunk token_count lower bound + tool-arg JSON validity, used to
        // price and label this attempt if it is abandoned or served.
        let mut estimated_output = 0_u64;
        let mut tool_args_ok = true;

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
                    estimated_output = estimated_output.saturating_add(chunk.token_count as u64);
                    if chunk.delta.is_empty() && chunk.reasoning.as_deref().unwrap_or("").is_empty()
                    {
                        continue;
                    }
                    if !client_connected {
                        continue;
                    }
                    // The role primer is scaffolding: accepting it opens the
                    // assistant message but delivers no model output, so it
                    // cannot on its own make this request billable. Only the
                    // delta below can.
                    role_sent = delivery.ensure_role(&sender, &metadata, role_sent).await;
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
                    let accepted = delivery
                        .send(
                            &sender,
                            stream_delta_json(&metadata, Value::Object(delta), None),
                            Frame::ModelOutput,
                        )
                        .await;
                    client_connected &= accepted;
                }
                Ok(StreamEvent::ToolCall(call)) => {
                    has_tool_calls = true;
                    tool_args_ok &= serde_json::from_str::<Value>(&call.arguments).is_ok();
                    if !client_connected {
                        tool_index = tool_index.saturating_add(1);
                        continue;
                    }
                    role_sent = delivery.ensure_role(&sender, &metadata, role_sent).await;
                    if !role_sent {
                        client_connected = false;
                        tool_index = tool_index.saturating_add(1);
                        continue;
                    }
                    // A tool-call delta IS model output — it is what a
                    // tool-calling completion consists of — so it counts as a
                    // delivery exactly as a content delta does.
                    let accepted = delivery
                        .send(
                            &sender,
                            stream_delta_json(
                                &metadata,
                                stream_tool_call_delta(call, tool_index),
                                None,
                            ),
                            Frame::ModelOutput,
                        )
                        .await;
                    tool_index = tool_index.saturating_add(1);
                    client_connected &= accepted;
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
            let attempt_usage = usage.or_else(|| estimated_stream_usage(estimated_output));
            attempts.push(build_attempt(
                attempt_no,
                candidate.definition(),
                interruption.attempt_outcome(),
                false,
                attempt_started,
                attempt_usage,
                usage.is_none() && attempt_usage.is_some(),
                None,
            ));
            settle_stream_interruption(
                &sender,
                &mut usage_session,
                &metadata,
                &resolved,
                Some(candidate.definition()),
                usage,
                features,
                std::mem::take(&mut attempts),
                started,
                client_connected,
                // This candidate's stream was polled, so the delivery signal is
                // real: whether any model output was accepted for the client
                // before the interruption.
                delivery,
                interruption,
            )
            .await;
            return;
        }

        if completed {
            if client_connected {
                let role_accepted = delivery.ensure_role(&sender, &metadata, role_sent).await;
                client_connected &= role_accepted;
            }
            finish_successful_stream(
                &sender,
                &mut usage_session,
                &metadata,
                &resolved,
                candidate,
                usage,
                has_tool_calls,
                tool_args_ok,
                max_tokens,
                features,
                attempts,
                attempt_no,
                attempt_started,
                if client_connected && !sender.is_closed() {
                    200
                } else {
                    499
                },
                // Only consulted when the upstream reported no usage, and then
                // only to label whether the customer saw the unbilled output.
                delivery,
                started,
            )
            .await;
            return;
        }

        client_connected &= !sender.is_closed();
        if delivery.model_output_sent || !client_connected {
            let Some(session) = usage_session.take() else {
                send_stream_error(&sender, &ApiError::MeteringUnavailable).await;
                return;
            };
            // Bill metered actuals only: the upstream's report when model
            // output reached the client, nothing otherwise. A stream that broke
            // mid-delivery having never reported usage is the common shape
            // here, and it settles at zero.
            let settled_usage = delivery.settled_usage(usage);
            if delivery.model_output_sent && usage.is_none() {
                log_metering_gap(
                    &metadata,
                    &resolved,
                    Some(candidate.definition()),
                    true,
                    "stream_error",
                );
            }
            // The customer received this candidate's partial output, so it is
            // the served attempt even though the stream broke.
            let served = delivery.model_output_sent;
            let attempt_usage = usage.or_else(|| estimated_stream_usage(estimated_output));
            attempts.push(build_attempt(
                attempt_no,
                candidate.definition(),
                "stream_error",
                served,
                attempt_started,
                attempt_usage,
                usage.is_none() && attempt_usage.is_some(),
                None,
            ));
            let metering = persist_usage(
                session,
                &resolved.requested_model,
                &candidate.definition().provider,
                &candidate.definition().model,
                Some(candidate.definition()),
                settled_usage,
                resolved.sell_rates,
                features,
                None,
                None,
                std::mem::take(&mut attempts),
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

        // Nothing delivered and the stream ended without completing: record a
        // non-served failure and fall through to the next candidate.
        attempts.push(build_attempt(
            attempt_no,
            candidate.definition(),
            "stream_error",
            false,
            attempt_started,
            usage.or_else(|| estimated_stream_usage(estimated_output)),
            usage.is_none() && estimated_output > 0,
            None,
        ));
    }

    let error = if let (Some(candidate), Some(session)) = (last_candidate, usage_session.take()) {
        // Every candidate failed before streaming any tokens; release the
        // reservation without a charge.
        match persist_usage(
            session,
            &resolved.requested_model,
            &candidate.provider,
            &candidate.model,
            Some(candidate),
            OpenAiUsage::default(),
            resolved.sell_rates,
            features,
            None,
            None,
            std::mem::take(&mut attempts),
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

    fn attempt_outcome(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Shutdown => "aborted",
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
    features: RequestFeatures,
    attempts: Vec<AttemptRecord>,
    started: Instant,
    client_connected: bool,
    delivery: StreamDelivery,
    interruption: StreamInterruption,
) {
    let client_connected = client_connected && !sender.is_closed();
    let (upstream_provider, upstream_model) = candidate.map_or(
        ("fallback-chain", resolved.requested_model.as_str()),
        |candidate| (candidate.provider.as_str(), candidate.model.as_str()),
    );
    // Same rule as every other streaming terminal: an interruption bills the
    // upstream's metered usage when it reported one before dying, and nothing
    // otherwise — whether that is because nothing was delivered or because a
    // partial delivery was never metered. A drained deploy (`Shutdown`) and an
    // upstream deadline (`Timeout`) both settle here.
    let settled_usage = delivery.settled_usage(usage);
    if delivery.model_output_sent && usage.is_none() {
        log_metering_gap(metadata, resolved, candidate, true, interruption.label());
    }
    let error = if let Some(session) = usage_session.take() {
        match persist_usage(
            session,
            &resolved.requested_model,
            upstream_provider,
            upstream_model,
            candidate,
            settled_usage,
            resolved.sell_rates,
            features,
            None,
            None,
            attempts,
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
    // Threaded from the walk so the frames replayed here are classified by the
    // same rule as a live stream's. This site settles BEFORE it replays
    // anything, so the flag it leaves behind is a record, not an input to the
    // charge — restructuring that ordering is out of scope here.
    delivery: &mut StreamDelivery,
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: &ProviderCandidate,
    response: ChatResponse,
    max_tokens: Option<u32>,
    features: RequestFeatures,
    mut attempts: Vec<AttemptRecord>,
    attempt_no: usize,
    attempt_started: Instant,
    started: Instant,
) {
    let Some(session) = usage_session.take() else {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let Some(usage) = OpenAiUsage::try_from_provider(response.usage.as_ref()) else {
        // The buffered sibling of the streaming gap, and it settles by the same
        // rule: no metered usage, no bill. This site is not the zero-delivery
        // case — a complete response is in hand, so a delivery-flag reading of
        // the rule would not obviously apply — but the rule no longer asks what
        // was delivered before it asks whether anything was measured. It used
        // to settle at the reservation bound, which billed a byte-length input
        // estimate plus the whole 4096-token output bound for a response the
        // customer never even receives (this branch aborts with
        // `metering_unavailable` instead of replaying it).
        log_metering_gap(
            metadata,
            resolved,
            Some(candidate.definition()),
            false,
            "synthetic_stream",
        );
        // The served attempt is still recorded, with NULL token columns, so the
        // gap is countable in the walk ledger rather than only in the log.
        attempts.push(build_attempt(
            attempt_no,
            candidate.definition(),
            "ok",
            false,
            attempt_started,
            None,
            false,
            None,
        ));
        let _ = persist_usage(
            session,
            &resolved.requested_model,
            &candidate.definition().provider,
            &candidate.definition().model,
            Some(candidate.definition()),
            OpenAiUsage::default(),
            resolved.sell_rates,
            features,
            None,
            None,
            attempts,
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let completion_status = if sender.is_closed() { 499 } else { 200 };
    let has_tool_calls = !response.tool_calls.is_empty();
    let synthesized_finish = finish_reason(has_tool_calls, usage, max_tokens);
    let output_nonempty = response
        .text
        .as_deref()
        .is_some_and(|text| !text.is_empty())
        || has_tool_calls;
    let shape_label = shape_ok(
        output_nonempty,
        tool_args_all_json(&response.tool_calls),
        synthesized_finish,
    );
    attempts.push(build_attempt(
        attempt_no,
        candidate.definition(),
        "ok",
        true,
        attempt_started,
        Some(usage),
        false,
        Some(synthesized_finish),
    ));
    if persist_usage(
        session,
        &resolved.requested_model,
        &candidate.definition().provider,
        &candidate.definition().model,
        Some(candidate.definition()),
        usage,
        resolved.sell_rates,
        features,
        Some(synthesized_finish),
        Some(shape_label),
        attempts,
        started,
        completion_status,
    )
    .await
    .is_err()
    {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    }

    if !delivery.ensure_role(sender, metadata, false).await {
        return;
    }
    if let Some(text) = response.text
        && !delivery
            .send(
                sender,
                stream_delta_json(metadata, json!({ "content": text }), None),
                Frame::ModelOutput,
            )
            .await
    {
        return;
    }
    if let Some(reasoning) = response.reasoning_content
        && !delivery
            .send(
                sender,
                stream_delta_json(metadata, json!({ "reasoning_content": reasoning }), None),
                Frame::ModelOutput,
            )
            .await
    {
        return;
    }
    for (index, call) in response.tool_calls.into_iter().enumerate() {
        if !delivery
            .send(
                sender,
                stream_delta_json(
                    metadata,
                    stream_tool_call_delta(call, u32::try_from(index).unwrap_or(u32::MAX)),
                    None,
                ),
                Frame::ModelOutput,
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
    tool_args_ok: bool,
    max_tokens: Option<u32>,
    features: RequestFeatures,
    mut attempts: Vec<AttemptRecord>,
    attempt_no: usize,
    attempt_started: Instant,
    completion_status: i16,
    delivery: StreamDelivery,
    started: Instant,
) {
    let Some(session) = usage_session.take() else {
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let Some(usage) = usage else {
        // The stream ran to `Final` and the upstream never reported usage —
        // the highest-frequency gap there is, since several provider families
        // never surface streaming usage at all. `settled_usage` bills nothing:
        // the customer keeps the output and ZeroRouter absorbs the provider
        // cost, which is the deliberate price of never guessing at a bill.
        log_metering_gap(
            metadata,
            resolved,
            Some(candidate.definition()),
            delivery.model_output_sent,
            "stream_final",
        );
        // Record the attempt with NULL token columns, `served` tracking whether
        // the customer actually got the unbilled output. That row joined
        // against the zero-token settled row is how the gap is counted after
        // the fact; before, this branch settled with no walk ledger at all.
        attempts.push(build_attempt(
            attempt_no,
            candidate.definition(),
            "ok",
            delivery.model_output_sent,
            attempt_started,
            None,
            false,
            None,
        ));
        let _ = persist_usage(
            session,
            &resolved.requested_model,
            &candidate.definition().provider,
            &candidate.definition().model,
            Some(candidate.definition()),
            delivery.settled_usage(None),
            resolved.sell_rates,
            features,
            None,
            None,
            attempts,
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let synthesized_finish = finish_reason(has_tool_calls, usage, max_tokens);
    let output_nonempty = usage.completion_tokens > 0 || has_tool_calls;
    let shape_label = shape_ok(output_nonempty, tool_args_ok, synthesized_finish);
    attempts.push(build_attempt(
        attempt_no,
        candidate.definition(),
        "ok",
        true,
        attempt_started,
        Some(usage),
        false,
        Some(synthesized_finish),
    ));
    if persist_usage(
        session,
        &resolved.requested_model,
        &candidate.definition().provider,
        &candidate.definition().model,
        Some(candidate.definition()),
        usage,
        resolved.sell_rates,
        features,
        Some(synthesized_finish),
        Some(shape_label),
        attempts,
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
        synthesized_finish,
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

async fn send_stream_error(sender: &mpsc::Sender<Event>, error: &ApiError) {
    if send_data(sender, streaming_error_json(error)).await {
        let _ = send_data(sender, "[DONE]".to_owned()).await;
    }
}

/// Queue one SSE frame, bounded by [`SSE_SEND_TIMEOUT`]. Returns whether the
/// channel accepted it — see [`StreamDelivery`] for why that is queueing rather
/// than delivery.
///
/// Every remaining direct caller sends [`Frame::Scaffolding`] (the finish
/// delta, the usage chunk, an error frame, `[DONE]`) after the request has
/// already settled, so none of them can affect a charge. Anything sent while a
/// settle is still ahead goes through [`StreamDelivery::send`], which is where
/// the classification lives.
async fn send_data(sender: &mpsc::Sender<Event>, data: String) -> bool {
    tokio::time::timeout(SSE_SEND_TIMEOUT, sender.send(Event::default().data(data)))
        .await
        .is_ok_and(|result| result.is_ok())
}

#[allow(clippy::too_many_arguments)]
async fn admit_usage(
    pool: &PgPool,
    key: &AuthenticatedKey,
    reserved_tokens: i64,
    reserved_output_tokens: i64,
    reserved_cost: rust_decimal::Decimal,
    task_signature: String,
    require_credits: bool,
) -> Result<UsageSession, ApiError> {
    match begin_usage_session(
        pool,
        key,
        reserved_tokens,
        reserved_output_tokens,
        reserved_cost,
        task_signature,
        require_credits,
    )
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
    candidate: Option<&TierCandidate>,
    usage: OpenAiUsage,
    sell_rates: ModelRates,
    features: RequestFeatures,
    finish_reason: Option<&str>,
    shape_label: Option<bool>,
    attempts: Vec<AttemptRecord>,
    started: Instant,
    status: i16,
) -> Result<(), ApiError> {
    let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
    let telemetry = RequestTelemetry {
        requested_max_tokens: features.requested_max_tokens,
        stream: features.stream,
        prompt_bytes: features.prompt_bytes,
        message_count: features.message_count,
        tool_count: features.tool_count,
        candidate_id: candidate.map(|candidate| candidate.id.clone()),
        basis_rates: candidate.map(|candidate| candidate.rates),
        sell_rates,
        finish_reason: finish_reason.map(str::to_owned),
        shape_ok: shape_label,
    };
    usage_session
        .record(&UsageRecord {
            tier: requested_model.to_owned(),
            upstream_provider: upstream_provider.to_owned(),
            upstream_model: upstream_model.to_owned(),
            usage,
            cost_usd: usage_cost(sell_rates, usage),
            latency_ms,
            status,
            telemetry,
            attempts,
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
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use uuid::Uuid;
    use zeroclaw_providers::traits::TokenUsage;

    use super::*;
    use crate::{
        sqlx::{
            postgres::{PgConnectOptions, PgPoolOptions},
            query, query_as, query_scalar,
        },
        testing::{FakeModelProvider, FakeOutcome, FakeStreamStep},
    };

    /// Only [`Frame::ModelOutput`] is a delivery. The role primer opens the
    /// assistant message and carries nothing the customer asked for, so
    /// accepting it must leave the request unbillable — it used to be enough on
    /// its own to mark the request as delivered.
    #[tokio::test]
    async fn only_model_output_frames_count_as_delivery() {
        let metadata =
            StreamMetadata::new("chatcmpl-test".to_owned(), "zero/test".to_owned(), false);
        let (sender, _receiver) = mpsc::channel(8);
        let mut delivery = StreamDelivery::default();

        assert!(
            delivery.ensure_role(&sender, &metadata, false).await,
            "the channel should accept the role primer"
        );
        assert!(
            !delivery.model_output_sent,
            "the role primer is scaffolding, not a delivery"
        );

        assert!(
            delivery
                .send(
                    &sender,
                    stream_delta_json(&metadata, json!({ "content": "hi" }), None),
                    Frame::ModelOutput,
                )
                .await
        );
        assert!(
            delivery.model_output_sent,
            "a content delta is model output and is a delivery"
        );
    }

    /// A frame the channel refuses is not a delivery either, whatever it
    /// carried: the flag tracks frames the transport accepted, not frames the
    /// walk produced.
    #[tokio::test]
    async fn a_refused_model_output_frame_is_not_a_delivery() {
        let metadata =
            StreamMetadata::new("chatcmpl-test".to_owned(), "zero/test".to_owned(), false);
        let (sender, receiver) = mpsc::channel(8);
        drop(receiver);
        let mut delivery = StreamDelivery::default();

        assert!(
            !delivery
                .send(
                    &sender,
                    stream_delta_json(&metadata, json!({ "content": "hi" }), None),
                    Frame::ModelOutput,
                )
                .await,
            "a closed channel refuses the frame"
        );
        assert!(!delivery.model_output_sent);
    }

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

    fn walk_candidate(id: &str) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: "test-upstream".to_owned(),
            model: format!("upstream/{id}"),
            rates: ModelRates {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: None,
            },
        }
    }

    /// A local OpenAI-compatible upstream: `/fail` answers 500 so the walk
    /// moves on, `/ok` streams one delta plus usage and `[DONE]`. Returns the
    /// two base URLs.
    async fn scripted_upstream() -> (String, String) {
        let stream_body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5,\"total_tokens\":16}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new()
            .route(
                "/fail/chat/completions",
                post(|| async {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "upstream down",
                    )
                }),
            )
            .route(
                "/ok/chat/completions",
                post(move || async move { ([("content-type", "text/event-stream")], stream_body) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("scripted upstream should bind");
        let address = listener
            .local_addr()
            .expect("scripted upstream should report its address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (
            format!("http://{address}/fail"),
            format!("http://{address}/ok"),
        )
    }

    /// A pool plus a funded key ready to admit a reservation, or `None` when
    /// no scratch database is configured.
    ///
    /// The pool is opened the way `tests/request_path.rs` opens its own: no
    /// liveness ping and every connection dialled up front, so a test that
    /// pauses the clock cannot have an acquire timer fire while the runtime is
    /// parked on socket I/O.
    async fn walk_fixture() -> Option<(PgPool, AuthenticatedKey)> {
        let database_url = std::env::var("DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .test_before_acquire(false)
            .acquire_timeout(Duration::from_secs(3_600))
            .connect_with(
                PgConnectOptions::from_str(&database_url).expect("test database URL must parse"),
            )
            .await
            .expect("test database must connect");
        crate::db::migrate(&pool).await.expect("migration must run");
        let mut warm = Vec::new();
        for _ in 0..2 {
            warm.push(pool.acquire().await.expect("pool connection must open"));
        }
        drop(warm);

        let user_id = Uuid::new_v4();
        let key = AuthenticatedKey {
            id: Uuid::new_v4(),
            user_id,
        };
        query("INSERT INTO users (id, email) VALUES ($1, $2)")
            .bind(user_id)
            .bind(format!("walk-{user_id}@example.invalid"))
            .execute(&pool)
            .await
            .expect("test user must insert");
        query(
            r#"
            INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
            VALUES ($1, $2, $3, 'walk', 20, 1000000)
            "#,
        )
        .bind(key.id)
        .bind(user_id)
        .bind(format!("{:064x}", key.id.as_u128()))
        .execute(&pool)
        .await
        .expect("test API key must insert");
        Some((pool, key))
    }

    fn walk_request() -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": "zero/test",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": true,
            "max_tokens": 64,
        }))
        .expect("walk request should deserialize")
    }

    fn walk_route(candidates: Vec<TierCandidate>) -> ResolvedRoute {
        ResolvedRoute {
            requested_model: "zero/test".to_owned(),
            candidates,
            sell_rates: ModelRates {
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(6.0),
                cached_input_per_mtok: None,
            },
        }
    }

    /// Admit a reservation for `request` and return it with the `request_id`
    /// the settle transaction will key every row on.
    async fn admit_walk(
        pool: &PgPool,
        key: &AuthenticatedKey,
        resolved: &ResolvedRoute,
        reservation_usage: OpenAiUsage,
    ) -> (UsageSession, Uuid) {
        let session = match begin_usage_session(
            pool,
            key,
            i64::try_from(reservation_usage.total_tokens).expect("reservation should fit"),
            64,
            usage_cost(resolved.sell_rates, reservation_usage),
            "0123456789abcdef".to_owned(),
            false,
        )
        .await
        .expect("admission must query")
        {
            UsageAdmission::Allowed(session) => session,
            _ => panic!("the walk request should be admitted"),
        };
        let request_id = Uuid::parse_str(
            session
                .request_id()
                .strip_prefix("chatcmpl-")
                .expect("request id should carry the reservation"),
        )
        .expect("request id should be a uuid");
        (session, request_id)
    }

    /// Drives the router-owned streaming walk over a failing candidate and a
    /// serving one, and asserts the walk ledger it drains into the settle
    /// transaction: one row per candidate tried, dense attempt numbers, and
    /// exactly one `served = true`.
    #[tokio::test]
    async fn streaming_walk_records_one_attempt_per_candidate_with_one_served() {
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let (fail_url, ok_url) = scripted_upstream().await;
        let first = walk_candidate("failing");
        let second = walk_candidate("serving");
        let resolved = walk_route(vec![first.clone(), second.clone()]);
        let request = walk_request();
        let reservation_usage = request.reservation_usage(64);
        let (session, request_id) = admit_walk(&pool, &key, &resolved, reservation_usage).await;
        let metadata = StreamMetadata::new(session.request_id(), "zero/test".to_owned(), false);

        let (sender, mut receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);
        // Keep the client attached for the whole walk: a closed receiver would
        // divert the walk to its client-disconnected terminal.
        let client = tokio::spawn(async move {
            let mut delivered = 0_usize;
            while receiver.recv().await.is_some() {
                delivered += 1;
            }
            delivered
        });
        stream_to_channel(
            sender,
            CancellationToken::new(),
            session,
            metadata,
            request,
            resolved,
            vec![
                ProviderCandidate::against_local_upstream(first, &fail_url),
                ProviderCandidate::against_local_upstream(second.clone(), &ok_url),
            ],
            reservation_usage,
        )
        .await;
        assert!(
            client.await.expect("client task should join") > 0,
            "the serving candidate should have reached the client"
        );

        let attempts = query_as::<_, (i16, String, bool)>(
            "SELECT attempt_no, outcome, served FROM request_attempts WHERE request_id = $1 ORDER BY attempt_no",
        )
        .bind(request_id)
        .fetch_all(&pool)
        .await
        .expect("attempts must query");
        assert_eq!(
            attempts,
            vec![
                (1, "stream_error".to_owned(), false),
                (2, "ok".to_owned(), true),
            ],
            "the walk records one row per candidate tried with exactly one served"
        );

        let (attempt_count, candidate_id, status) =
            query_as::<_, (Option<i16>, Option<String>, i16)>(
                "SELECT attempt_count, candidate_id, status FROM usage_events WHERE request_id = $1",
            )
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .expect("settled row must query");
        assert_eq!(attempt_count, Some(2));
        assert_eq!(candidate_id.as_deref(), Some(second.id.as_str()));
        assert_eq!(status, 200);
    }

    /// The role primer is the only frame the client ever accepts: the channel
    /// holds exactly one and nothing drains it, so the content delta queued
    /// behind the primer is refused and the customer receives no model output
    /// at all. The upstream still reports usage before it breaks — and that
    /// usage must not be billed, because the delivery that would justify
    /// billing it never happened. Before the frame classification landed, the
    /// accepted primer alone marked the request delivered and this settled at
    /// the metered cost.
    #[tokio::test]
    async fn a_stream_whose_only_accepted_frame_is_the_role_primer_settles_at_zero() {
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let candidate = walk_candidate("primer-only");
        let resolved = walk_route(vec![candidate.clone()]);
        let request = walk_request();
        let reservation_usage = request.reservation_usage(64);
        let (session, request_id) = admit_walk(&pool, &key, &resolved, reservation_usage).await;
        let metadata = StreamMetadata::new(session.request_id(), "zero/test".to_owned(), false);
        let fake = FakeModelProvider::new(
            "primer-only",
            vec![FakeOutcome::Stream(vec![
                FakeStreamStep::text("partial"),
                FakeStreamStep::Usage(TokenUsage {
                    input_tokens: Some(1_000),
                    output_tokens: Some(20),
                    cached_input_tokens: None,
                }),
                FakeStreamStep::Error("upstream exploded".to_owned()),
            ])],
        );

        // One slot, never drained: the role primer fills it and nothing behind
        // it can be accepted.
        let (sender, receiver) = mpsc::channel(1);
        // The refused delta waits out the 5s `SSE_SEND_TIMEOUT`; pausing the
        // clock spends that instantly. `walk_fixture`'s pool is warm and
        // timer-free on acquire, so the settle below is unaffected.
        tokio::time::pause();
        stream_to_channel(
            sender,
            CancellationToken::new(),
            session,
            metadata,
            request,
            resolved,
            vec![ProviderCandidate::with_provider(candidate.clone(), fake)],
            reservation_usage,
        )
        .await;
        tokio::time::resume();
        drop(receiver);

        let (cost_usd, input_tokens, output_tokens, status) =
            query_as::<_, (Decimal, i32, i32, i16)>(
                r#"
                SELECT cost_usd, input_tokens, output_tokens, status
                FROM usage_events
                WHERE request_id = $1
                "#,
            )
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .expect("settled row must query");
        assert_eq!(
            cost_usd,
            Decimal::ZERO,
            "scaffolding is not a delivery, so the metered usage behind it is not billable"
        );
        assert_eq!((input_tokens, output_tokens), (0, 0));
        assert_eq!(status, 499, "the client never got what it asked for");

        let served =
            query_scalar::<_, bool>("SELECT served FROM request_attempts WHERE request_id = $1")
                .bind(request_id)
                .fetch_one(&pool)
                .await
                .expect("attempt row must query");
        assert!(
            !served,
            "an attempt whose output never reached the client is not the served one"
        );
    }

    /// An interruption that delivered nothing settles at zero, but it still has
    /// to name the candidate that ran: the reservation is released without a
    /// charge, and the row keeps candidate provenance so the burnt attempt stays
    /// attributable in the ledger instead of vanishing.
    #[tokio::test]
    async fn interrupted_stream_releases_without_charge_but_keeps_provenance() {
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let (_, ok_url) = scripted_upstream().await;
        let candidate = walk_candidate("interrupted");
        let resolved = walk_route(vec![candidate.clone()]);
        let request = walk_request();
        let reservation_usage = request.reservation_usage(64);
        let (session, request_id) = admit_walk(&pool, &key, &resolved, reservation_usage).await;
        let metadata = StreamMetadata::new(session.request_id(), "zero/test".to_owned(), false);

        let (sender, mut receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);
        let client = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        // A shutdown already in flight: the biased select in the stream loop
        // takes the interruption terminal on its first poll.
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        stream_to_channel(
            sender,
            shutdown,
            session,
            metadata,
            request,
            resolved,
            vec![ProviderCandidate::against_local_upstream(
                candidate.clone(),
                &ok_url,
            )],
            reservation_usage,
        )
        .await;
        client.await.expect("client task should join");

        let (settled_candidate, cost_usd, cost_basis_usd, output_tokens, status) =
            query_as::<_, (Option<String>, Decimal, Option<Decimal>, i32, i16)>(
                r#"
                SELECT candidate_id, cost_usd, cost_basis_usd, output_tokens, status
                FROM usage_events
                WHERE request_id = $1
                "#,
            )
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .expect("settled row must query");
        assert_eq!(settled_candidate.as_deref(), Some(candidate.id.as_str()));
        assert_eq!(
            cost_usd,
            Decimal::ZERO,
            "the shutdown arrived before a single frame reached the client, so \
             the reservation is released without a charge"
        );
        assert_eq!(output_tokens, 0);
        assert_eq!(
            cost_basis_usd,
            Some(usage_cost(candidate.rates, OpenAiUsage::default())),
            "COGS is still priced on the same usage the customer is billed"
        );
        assert_eq!(status, 503);

        let attempts = query_as::<_, (String, bool)>(
            "SELECT outcome, served FROM request_attempts WHERE request_id = $1",
        )
        .bind(request_id)
        .fetch_all(&pool)
        .await
        .expect("attempts must query");
        assert_eq!(attempts, vec![("aborted".to_owned(), false)]);
    }
}

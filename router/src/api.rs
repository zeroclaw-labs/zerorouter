use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::provider::BASELINE_MAX_TOKENS;
use crate::provider::{ChatRequest, ChatResponse, ModelRates, StreamEvent, StreamOptions};
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

use crate::{
    auth::{AuthenticatedKey, AuthenticationError, KeyAuthenticator},
    config::{ResolvedRoute, TierCandidate, TierCatalog, load_tier_catalog},
    db::{
        AttemptRecord, AttemptTokens, RequestTelemetry, ReservationBasis, SegmentClampStats,
        SettlementRecovery, UsageAdmission, UsageRecord, UsageSession, begin_usage_session,
        output_token_percentiles, recover_owed_settlements, segment_clamp_stats, segment_user,
        user_clamp_loss,
    },
    error::{ApiError, streaming_error_json},
    estimator::{
        CellKey, CellRead, EstimatorState, REFRESH_BATCH, REFRESH_INTERVAL, ROW_LOSS_LIMIT_USD,
        SEGMENT_HIT_RATE_LIMIT, SEGMENT_LOSS_LIMIT_7D_USD, USER_LOSS_LIMIT_30D_USD,
        learned_output_bound,
    },
    health::{ProviderHealth, WalkLedger},
    openai::{
        ChatCompletionRequest, ChatCompletionResponse, EmittedOutput, EstimateBasis, ModelList,
        OpenAiUsage, StreamMetadata, TaskSignature, ZeroRouterAttempt, ZeroRouterEstimate,
        ZeroRouterResponseMetadata, finish_reason, shape_ok, stream_delta_json,
        stream_tool_call_delta, stream_usage_json, task_signature, tool_args_all_json, usage_cost,
    },
    priority::Priority,
    providers::{ProviderCandidate, ProviderRoute},
    retry,
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
    // The resolved priority knob for this request (rollout Stage 3a) —
    // resolved before admission from typed field > model suffix > per-key
    // default > balanced, and written on the settled row at every terminal.
    priority: Priority,
    // Whether any carrier actually engaged the knob. Response-block
    // visibility only (`zerorouter_block`), never persisted: a request that
    // never mentioned the knob keeps its byte-identical legacy response
    // while still recording its resolved `balanced`.
    knob_engaged: bool,
    // The segment's output estimate, read from the candidate-agnostic cell
    // once in `chat_completions` and carried to every serve site so the
    // response block shows it. Response-only, never persisted
    // (`usage_events.estimator_basis` stays 'cold' until Stage 4 sizes
    // reservations from this).
    estimate: ZeroRouterEstimate,
}

impl RequestFeatures {
    fn from_request(
        request: &ChatCompletionRequest,
        reservation_usage: OpenAiUsage,
        priority: PriorityResolution,
        estimate: ZeroRouterEstimate,
    ) -> Self {
        Self {
            requested_max_tokens: request
                .max_tokens
                .map_or(0, |max| i32::try_from(max).unwrap_or(i32::MAX)),
            stream: request.stream,
            prompt_bytes: i64::try_from(reservation_usage.prompt_tokens).unwrap_or(i64::MAX),
            message_count: i32::try_from(request.messages.len()).unwrap_or(i32::MAX),
            tool_count: i32::try_from(request.tools.len()).unwrap_or(i32::MAX),
            priority: priority.resolved,
            knob_engaged: priority.engaged,
            estimate,
        }
    }
}

/// The outcome of resolving the priority knob for one request (design doc:
/// "Precedence and conflicts"): which priority governs, and whether any
/// carrier actually set it.
#[derive(Clone, Copy)]
struct PriorityResolution {
    resolved: Priority,
    engaged: bool,
}

impl PriorityResolution {
    fn new(engaged: Option<Priority>) -> Self {
        Self {
            resolved: engaged.unwrap_or(Priority::Balanced),
            engaged: engaged.is_some(),
        }
    }
}

/// The `zerorouter` response block for a served completion, built from the
/// same walk ledger the settle transaction drains — the customer-visible
/// attempts array and the persisted `request_attempts` rows are one story by
/// construction. `None` while the knob was never engaged, which is what
/// keeps legacy responses byte-stable.
fn zerorouter_block(
    features: RequestFeatures,
    attempts: &WalkLedger,
) -> Option<ZeroRouterResponseMetadata> {
    features.knob_engaged.then(|| ZeroRouterResponseMetadata {
        priority: features.priority,
        estimate: features.estimate,
        attempts: attempts
            .rows()
            .iter()
            .map(|row| ZeroRouterAttempt {
                candidate: row.candidate_id.clone(),
                outcome: row.outcome.clone(),
                latency_ms: row.latency_ms,
            })
            .collect(),
        validated: None,
    })
}

/// Build one `request_attempts` row from an in-walk candidate outcome. Cost
/// basis is the candidate's own cost-basis rate applied to whatever tokens are
/// known (a per-chunk `token_count` output floor for abandoned attempts).
///
/// `validator_kind` names the check that rejected this attempt, on the rows
/// where one did. It is `None` everywhere a check was not the reason the
/// attempt ended — a transport failure, a timeout, a completion that was simply
/// served — and free-form by design (migration 0004), because the set of checks
/// grows over time while rows are immutable.
#[allow(clippy::too_many_arguments)]
fn build_attempt(
    attempt_no: usize,
    candidate: &TierCandidate,
    outcome: &'static str,
    served: bool,
    attempt_started: Instant,
    tokens: AttemptTokens,
    tokens_estimated: bool,
    finish_reason: Option<&str>,
    validator_kind: Option<&'static str>,
) -> AttemptRecord {
    let latency_ms = i32::try_from(attempt_started.elapsed().as_millis()).unwrap_or(i32::MAX);
    let started_at = Utc::now() - chrono::Duration::milliseconds(i64::from(latency_ms));
    // `and_then`, not `map`: a candidate whose rates cannot be priced leaves
    // this NULL — the ledger's word for "not captured", which also flips the
    // row's `attempts_cost_basis_complete` FALSE so the COGS sum is read as the
    // lower bound it is. Never zero, which would claim the attempt was free.
    let cost_basis_usd = tokens
        .priceable()
        .and_then(|usage| usage_cost(candidate.rates, usage));
    AttemptRecord {
        attempt_no: i16::try_from(attempt_no).unwrap_or(i16::MAX),
        started_at,
        candidate_id: candidate.id.clone(),
        upstream_provider: candidate.provider.clone(),
        upstream_model: candidate.model.clone(),
        outcome: outcome.to_owned(),
        served,
        latency_ms,
        tokens,
        tokens_estimated,
        cost_basis_usd,
        finish_reason: finish_reason.map(str::to_owned),
        validator_kind: validator_kind.map(str::to_owned),
    }
}

/// The tokens an attempt is known to have consumed: the upstream's report if it
/// made one, otherwise the per-chunk `token_count` output floor a stream
/// already carries, otherwise nothing.
///
/// # Why the prompt side stays unknown
///
/// Every dispatched attempt certainly consumed the prompt, so an output-only
/// figure certainly understates the attempt's real COGS, and the obvious repair
/// is to add the prompt bound admission already computed. That repair is
/// rejected: `ChatCompletionRequest::reservation_usage` measures the prompt in
/// **bytes** (a per-message constant plus `str::len` over every field), and
/// `usage_cost` prices per TOKEN. Feeding bytes to a per-token rate inflates the
/// input side by roughly the bytes-per-token ratio — about 4x for English —
/// which trades a known understatement for an error of unknown size in the
/// other direction. A floor that is labelled a floor is usable; a number that
/// might be 4x high in either direction is not, and this ledger exists to be
/// trusted.
///
/// So the prompt dimension is written NULL, not 0, and every non-measured
/// attempt marks its request's `attempts_cost_basis_complete` FALSE — the sum a
/// reader sees is explicitly a lower bound rather than a wrong total. The
/// honest bound arrives for free the day the pinned provider trait carries a
/// real prompt-token count.
///
/// Never bill a customer with this, or with anything else estimated — customer
/// billing runs through [`StreamDelivery::settled_usage`], which bills metered
/// actuals only.
fn attempt_tokens(usage: Option<OpenAiUsage>, estimated_output: u64) -> AttemptTokens {
    match usage {
        Some(usage) => AttemptTokens::measured(usage),
        None if estimated_output > 0 => AttemptTokens::output_floor(estimated_output),
        None => AttemptTokens::unknown(),
    }
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
    request_id: &str,
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
        request_id,
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
    /// Whether the model produced output at all, regardless of whether the
    /// client was still there to take it. The pair distinguishes two states
    /// that `model_output_sent` alone cannot: a model that answered with
    /// nothing (a complete, valid, empty response — the customer got what
    /// the upstream produced) from a client that vanished mid-answer (the
    /// customer got nothing that was meant for them).
    model_output_attempted: bool,
}

impl StreamDelivery {
    /// Send one frame, recording it as a delivery only when it carries model
    /// output. Returns whether the channel accepted the frame.
    async fn send(&mut self, sender: &mpsc::Sender<Event>, data: String, frame: Frame) -> bool {
        let accepted = send_data(sender, data).await;
        self.model_output_attempted |= frame == Frame::ModelOutput;
        self.model_output_sent |= accepted && frame == Frame::ModelOutput;
        accepted
    }

    /// Whether output the model produced failed to reach the client. This is
    /// the abandoned-stream case, and the only one where a usage report
    /// exists for output nobody received.
    fn abandoned_by_client(self) -> bool {
        self.model_output_attempted && !self.model_output_sent
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

/// Retries one candidate gets after its first call, so three upstream calls per
/// candidate and never a fourth. Reproduces the delegated walk's retry budget
/// exactly; changing it changes what every failing request costs in provider
/// spend.
const CANDIDATE_RETRIES: u32 = 2;

/// First wait between a candidate's attempts, doubled per retry and capped by
/// `retry::next_backoff`. Reset for each candidate, so a fresh rung starts from
/// the base interval rather than inheriting the previous one's.
const CANDIDATE_BACKOFF_MS: u64 = 500;
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How long a client may take to deliver its request body. Authentication
/// happens BEFORE the body is read, so without a deadline a valid key can
/// hold buffers open indefinitely by trickling bytes — no reservation is
/// taken and no cap is consumed while it does, which is what makes it free
/// (sol review). Thirty seconds is generous for 8 MiB on a poor link.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How many request bodies this process will buffer at once. Bounds the
/// pre-admission memory a caller can command to
/// `MAX_CONCURRENT_BODY_READS * MAX_REQUEST_BODY_BYTES`; beyond it the
/// router sheds load rather than queueing unboundedly and dying with
/// everyone's request in flight.
const MAX_CONCURRENT_BODY_READS: usize = 64;

static BODY_READ_SLOTS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_BODY_READS));

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
    /// Cross-request rung health (stage 2b). Lives exactly as long as the
    /// services — in-process and lost on restart, deliberately.
    health: ProviderHealth,
    /// The cost estimator's cell cache (stage 3b), on the same contract:
    /// in-process, lost on restart, and restart-cold is exactly today's
    /// behavior. Requests only read it; the background refresher
    /// ([`RouterState::spawn_estimator_refresher`]) is its only writer.
    estimator: EstimatorState,
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
                health: ProviderHealth::default(),
                estimator: EstimatorState::default(),
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
                health: ProviderHealth::default(),
                estimator: EstimatorState::default(),
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

    /// Start the background estimator refresher: every
    /// [`REFRESH_INTERVAL`], drain the cells requests enqueued and run their
    /// percentile scans off the request path.
    ///
    /// Opt-in and called only by `serve`, for the same reason as
    /// [`RouterState::spawn_settlement_recovery`]: the loop exits only on
    /// shutdown, so a test harness that started it could never drain
    /// [`RouterState::wait_for_background_tasks`]. Tests drive the identical
    /// batch synchronously through [`RouterState::refresh_estimator_once`].
    pub fn spawn_estimator_refresher(&self) {
        let Some(services) = &self.services else {
            return;
        };
        let services = Arc::clone(services);
        let shutdown = services.runtime.shutdown.clone();
        services.runtime.tasks.clone().spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(REFRESH_INTERVAL) => {}
                }
                refresh_estimator_batch(&services.pool, &services.estimator, Some(&shutdown)).await;
            }
        });
    }

    /// Run one refresher batch synchronously — the testing seam for the loop
    /// [`RouterState::spawn_estimator_refresher`] runs in production.
    #[cfg(feature = "testing")]
    pub async fn refresh_estimator_once(&self) {
        if let Some(services) = &self.services {
            refresh_estimator_batch(&services.pool, &services.estimator, None).await;
        }
    }

    /// How many estimator cells are queued for refresh — visibility for the
    /// re-enqueue-on-error pin, nothing more.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn estimator_pending_len(&self) -> usize {
        self.services
            .as_ref()
            .map_or(0, |services| services.estimator.pending_len())
    }

    /// Backdate every cached estimator cell, so a test can cross the
    /// staleness TTL without touching the runtime clock.
    #[cfg(feature = "testing")]
    pub fn age_estimator_cells(&self, by: Duration) {
        if let Some(services) = &self.services {
            services.estimator.age_cells(by);
        }
    }

    /// Start the autopay sweep: every minute, charge saved cards for users
    /// under their recharge threshold. Same opt-in serve-only contract as
    /// the other background loops; tests drive
    /// `stripe::run_autopay_sweep_once` directly.
    pub fn spawn_autopay_sweep(&self, settings: crate::web::StripeSettings) {
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
                    () = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
                crate::stripe::run_autopay_sweep_once(&pool, &settings).await;
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

/// One refresher pass: drain the pending cells and run each percentile scan.
/// A failed scan re-enqueues its cell so the next pass retries it — without
/// this, a cell that failed once would stay cold until its TTL re-offered
/// it. Shared verbatim by the production loop and the testing seam, so tests
/// exercise the code that ships.
///
/// The batch checks `shutdown` between scans: a full batch is up to
/// [`REFRESH_BATCH`] sequential queries, and on a degraded database each can
/// block for its own timeout — without the check, a SIGTERM landing
/// mid-batch would hold `wait_for_background_tasks` for the whole remainder.
/// Undrained keys are simply dropped on shutdown; the process is exiting and
/// a restart is cold everywhere anyway.
async fn refresh_estimator_batch(
    pool: &PgPool,
    estimator: &EstimatorState,
    shutdown: Option<&CancellationToken>,
) {
    for key in estimator.drain_pending(REFRESH_BATCH) {
        if shutdown.is_some_and(CancellationToken::is_cancelled) {
            return;
        }
        match output_token_percentiles(pool, &key.signature, key.scheme, key.candidate.as_deref())
            .await
        {
            Ok(measured) => {
                // Revert evaluation rides the same cadence as the segment's
                // percentile refresh — candidate-agnostic cells only, since
                // reverts are per segment, never per rung. A failed
                // evaluation fails toward cold, symmetric with the
                // percentile-failure arm below: the cell is NOT applied (so
                // sizing stays cold) and is re-enqueued for the next pass —
                // warming a cell whose durable loss evidence was never
                // consulted would let a lossy segment size learned for a
                // full TTL on the strength of a transient query error.
                if key.candidate.is_none()
                    && let Err(error) = evaluate_revert(pool, estimator, &key).await
                {
                    tracing::warn!(
                        error = %error,
                        signature = key.signature,
                        "clamp-loss revert evaluation failed; cell left cold and re-queued"
                    );
                    estimator.enqueue(key);
                    continue;
                }
                estimator.apply(key, measured);
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    signature = key.signature,
                    candidate = key.candidate.as_deref().unwrap_or("<signature>"),
                    "estimator cell refresh failed; cell re-queued"
                );
                estimator.enqueue(key);
            }
        }
    }
}

/// The auto-revert evaluator (design doc: "Auto-revert triggers on dollars,
/// not rates"), run beside a segment's percentile refresh and never on a
/// request. Fires per segment on trailing-7d clamp-loss dollars, any single
/// row's loss, or — secondary, a distribution-shift signal — the clamp-hit
/// rate; and per USER on the trailing-30d aggregate across all their
/// segments, which re-slicing traffic cannot escape because segments are
/// user-scoped. Reverted sizing goes cold for at least the cooldown;
/// trailing windows keep re-firing while the loss evidence is inside them,
/// which is the intended ratchet, and the marks expire on their own once
/// the evidence ages out.
async fn evaluate_revert(
    pool: &PgPool,
    estimator: &EstimatorState,
    key: &CellKey,
) -> Result<(), crate::sqlx::Error> {
    let stats = segment_clamp_stats(pool, &key.signature, key.scheme).await?;
    if segment_tripped(&stats) {
        tracing::warn!(
            signature = key.signature,
            loss_7d = %stats.loss_7d_usd,
            max_row_loss_7d = %stats.max_row_loss_7d_usd,
            clamped_rows_7d = stats.clamped_rows_7d,
            learned_rows_7d = stats.learned_rows_7d,
            "clamp-loss revert: segment returns to cold sizing"
        );
        estimator.revert_segment(&key.signature, key.scheme);
    }
    // The user-level check runs UNCONDITIONALLY of the segment's own learned
    // rows: a reverted user settles only cold rows, so any gate on this
    // segment's learned count would let the standing 30-day evidence expire
    // unconsulted the moment the mark's cooldown lapsed — the exact escape
    // the user aggregate exists to close. It depends on nothing but the
    // segment's owner and the user's own trailing windows.
    if let Some(user_id) = segment_user(pool, &key.signature, key.scheme).await? {
        let (loss_30d, loss_rederive) = user_clamp_loss(pool, user_id).await?;
        if decimal_to_f64(loss_30d) > USER_LOSS_LIMIT_30D_USD
            || decimal_to_f64(loss_rederive) > USER_LOSS_LIMIT_30D_USD
        {
            tracing::warn!(
                %user_id,
                loss_30d = %loss_30d,
                "clamp-loss revert: every segment of this user returns to cold sizing"
            );
            estimator.revert_user(user_id);
        }
    }
    Ok(())
}

/// Whether a segment's clamp-loss evidence trips a revert, in EITHER
/// window: the 7-day trigger window carries the spec's thresholds; the
/// 14-day re-derivation window (trigger + cooldown) is what lets a restart
/// re-derive a mark whose evidence has aged past the trigger window but not
/// past the cold period that evidence justified. Re-derivation can only be
/// conservative — a mark re-fired from old evidence extends cold sizing,
/// never learned exposure. The hit rate stays a within-window ratio; an
/// empty window's 0/0 is NaN, and NaN compares false — no trip.
fn segment_tripped(stats: &SegmentClampStats) -> bool {
    window_tripped(
        stats.loss_7d_usd,
        stats.max_row_loss_7d_usd,
        stats.clamped_rows_7d,
        stats.learned_rows_7d,
    ) || window_tripped(
        stats.loss_14d_usd,
        stats.max_row_loss_14d_usd,
        stats.clamped_rows_14d,
        stats.learned_rows_14d,
    )
}

fn window_tripped(
    loss_usd: rust_decimal::Decimal,
    max_row_loss_usd: rust_decimal::Decimal,
    clamped_rows: i64,
    learned_rows: i64,
) -> bool {
    #[allow(clippy::cast_precision_loss)]
    let hit_rate = clamped_rows as f64 / learned_rows as f64;
    decimal_to_f64(loss_usd) > SEGMENT_LOSS_LIMIT_7D_USD
        || decimal_to_f64(max_row_loss_usd) > ROW_LOSS_LIMIT_USD
        || hit_rate > SEGMENT_HIT_RATE_LIMIT
}

/// Threshold comparison only — never billing math. A Decimal that cannot
/// render as f64 reads as infinite loss, which fails toward cold.
fn decimal_to_f64(value: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    value.to_f64().unwrap_or(f64::INFINITY)
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

    // Bounded, and bounded in time: the slot caps how much the process can
    // be holding at once, and the deadline caps how long any one caller can
    // hold theirs. `try_acquire` rather than a wait, so a saturated router
    // answers immediately instead of growing a queue of its own.
    let _slot = BODY_READ_SLOTS
        .try_acquire()
        .map_err(|_| ApiError::Overloaded)?;
    let payload = tokio::time::timeout(BODY_READ_TIMEOUT, to_bytes(body, MAX_REQUEST_BODY_BYTES))
        .await
        .map_err(|_| ApiError::RequestTimeout)?
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
    // The model-suffix priority carrier (design doc: "Model-suffix carrier"),
    // resolve-first: the untouched model string is tried before anything is
    // stripped, so a hypothetical id ending in a priority keyword — catalog
    // validation refuses to load one — would still resolve as itself. Only
    // when that fails, and the segment after the last ':' is exactly a
    // priority keyword, is the suffix stripped and the remainder resolved;
    // anything else falls through to the same not-found answer as before.
    // Everything downstream — `usage_events.tier`, the response `model`
    // field, stream metadata — reads the resolved (stripped) name.
    let (resolved, suffix_priority) = match catalog.resolve(&request.model) {
        Some(resolved) => (resolved, None),
        None => match split_priority_suffix(&request.model) {
            Some((base, priority)) => match catalog.resolve(base) {
                Some(resolved) => (resolved, Some(priority)),
                None => return Err(model_unresolvable(&catalog, base)),
            },
            None => return Err(model_unresolvable(&catalog, &request.model)),
        },
    };
    // Precedence: typed `zerorouter.priority` > model suffix > per-key
    // default > balanced. Typed and suffix present with different values is
    // a client bug and refused loudly (`priority_conflict`), before anything
    // is reserved.
    let typed_priority = request.zerorouter.and_then(|options| options.priority);
    if let (Some(typed), Some(suffix)) = (typed_priority, suffix_priority)
        && typed != suffix
    {
        return Err(ApiError::PriorityConflict);
    }
    let priority = PriorityResolution::new(
        typed_priority
            .or(suffix_priority)
            .or(authenticated.default_priority),
    );
    let max_output_tokens = *request.max_tokens.get_or_insert(BASELINE_MAX_TOKENS);
    let reservation_usage = request.reservation_usage(max_output_tokens);
    // The user-scoped segmentation key (design: Engine "Task signature"),
    // computed over the same request-shape fields the reservation measures.
    // Moved ahead of route construction in stage 3b because selection now
    // reads the segment's estimator cells; the computation is pure, so only
    // the order changed.
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
    // One cache read per request for the candidate-agnostic cell. Every
    // request — engaged or not — offers its segment to the refresher through
    // this lookup's miss path: the flywheel warms on all traffic, which is
    // what Stage 4's reservation sizing will want already spinning.
    let signature_cell = services
        .estimator
        .lookup(&CellKey::for_signature(&signature));
    // The estimate the response block will show: learned percentiles from
    // the warm cell, else the cold byte-bound answer. Guidance, never a
    // quote. Display is deliberately NOT the sizing decision below: a
    // reverted segment keeps showing its learned percentiles (they are still
    // the segment's best measurement) while its reservations size cold.
    let estimate = match signature_cell {
        CellRead::Warm(percentiles) => ZeroRouterEstimate {
            output_tokens_p50: round_tokens(percentiles.p50),
            output_tokens_p90: round_tokens(percentiles.p90),
            basis: EstimateBasis::Learned,
        },
        CellRead::Cold => ZeroRouterEstimate::cold(max_output_tokens),
    };
    // Stage 4: reservation sizing (design doc: "Use — reservation sizing
    // only, never billing"). A request sizes learned only when every gate
    // holds: the segment cell is warm (n ≥ 50, fresh), the tail gate inside
    // `learned_output_bound` passes (p99/p50 ≤ 8), the request is not
    // escalation-capable (success mode keeps the byte bound outright — the
    // tail-correlated cohort), and neither the segment nor its user is
    // auto-reverted. Everything else reserves exactly today's byte bound.
    // The learned bound can only shrink admission (capped at the request's
    // own max_tokens), and the generation limit sent upstream is untouched —
    // only the reservation narrows.
    let learned_bound = match signature_cell {
        CellRead::Warm(percentiles)
            if priority.resolved != Priority::Success
                && !services.estimator.segment_reverted(&signature)
                && !services.estimator.user_reverted(authenticated.user_id) =>
        {
            learned_output_bound(percentiles, max_output_tokens)
        }
        _ => None,
    };
    let (reserved_output_bound, reservation_basis) = match learned_bound {
        Some(bound) => (bound, ReservationBasis::Learned),
        None => (max_output_tokens, ReservationBasis::Cold),
    };
    // Re-measure the reservation against the sized output bound. The input
    // side is byte-bound either way and identical to `reservation_usage`
    // above; only the output side narrows.
    let reservation_usage = request.reservation_usage(reserved_output_bound);
    let mut provider_route = services.provider_route(&resolved, max_output_tokens)?;
    order_candidates(
        priority.resolved,
        provider_route.candidates_mut(),
        &CostContext {
            estimator: &services.estimator,
            signature: &signature,
            signature_cell,
            input_bytes: reservation_usage.prompt_tokens,
        },
        &services.health,
    );
    let reserved_tokens =
        i64::try_from(reservation_usage.total_tokens).map_err(|_| ApiError::InvalidRequest)?;
    // Fail closed before admission: a tier whose sell rates cannot be priced
    // cannot size a reservation, and a request that cannot be metered must not
    // be dispatched. The catalog validated these rates at load, so this is a
    // backstop rather than a live path.
    let reserved_cost =
        usage_cost(resolved.sell_rates, reservation_usage).ok_or(ApiError::MeteringUnavailable)?;
    let usage_session = admit_usage(
        &services.pool,
        &authenticated,
        reserved_tokens,
        i64::from(reserved_output_bound),
        reserved_cost,
        signature,
        reservation_basis,
        services.require_credits,
    )
    .await?;
    let runtime = services.runtime.clone();
    let health = services.health.clone();

    if request.stream {
        streaming_response(
            runtime,
            health,
            usage_session,
            request,
            resolved,
            provider_route,
            reservation_usage,
            priority,
            estimate,
        )
    } else {
        non_streaming_response(
            runtime,
            health,
            usage_session,
            request,
            resolved,
            provider_route,
            reservation_usage,
            priority,
            estimate,
        )
        .await
    }
}

/// The model-suffix carrier's split: `Some((base, priority))` when the last
/// `:`-delimited segment is exactly a priority keyword and a base remains.
/// Never consulted while the untouched string resolves (resolve-first).
fn split_priority_suffix(model: &str) -> Option<(&str, Priority)> {
    let (base, keyword) = model.rsplit_once(':')?;
    let priority = Priority::from_keyword(keyword)?;
    (!base.is_empty()).then_some((base, priority))
}

/// Absent from the servable catalog means one of two very different things.
/// Either the id does not exist (the caller's mistake, 404), or it exists in
/// a tier withheld for below-cost pricing — ZeroRouter's mistake, which the
/// caller cannot fix and must not be told is a missing model.
fn model_unresolvable(catalog: &TierCatalog, requested_model: &str) -> ApiError {
    catalog
        .unavailable_for(requested_model)
        .map_or(ApiError::ModelNotFound, |withheld| {
            ApiError::ModelUnavailable {
                tier: withheld.tier.clone(),
            }
        })
}

/// Selection policy (design doc: Engine "Selection policy"), applied to the
/// built route before either walk starts.
///
/// Since stage 3b, `cost` orders ascending by expected cost basis —
/// estimator-backed, with a whole-route fall-through to the identity while
/// the segment is cold (`order_by_expected_cost`). `balanced` stays the
/// identity — the tiers.toml order, the human-curated prior and the frozen
/// control group. `success` stays the identity until its estimator and
/// escalation machinery arrive in stage 5a.
///
/// Health demotion applies last, in every mode: demoted rungs sink to the
/// back — preserving table order within each group — and never disappear.
/// Sinking replaces the recorded skip as demotion's first line (stage 2b
/// shipped the skip while ordering belonged to this stage); the walk-time
/// `should_skip` check remains as the backstop for a rung that cools between
/// this ordering and the walk reaching it, and its never-below-one-candidate
/// floor is unchanged. An all-demoted route partitions to itself, so this
/// can never manufacture an empty route.
fn order_candidates(
    priority: Priority,
    candidates: &mut Vec<ProviderCandidate>,
    estimates: &CostContext<'_>,
    health: &ProviderHealth,
) {
    match priority {
        // Since 3b, cost orders by expected cost basis (cold-fallback:
        // identity). balanced: identity by definition, the frozen control
        // group. success: identity until its machinery arrives in 5a.
        Priority::Cost => order_by_expected_cost(candidates, estimates),
        Priority::Balanced | Priority::Success => {}
    }
    let (healthy, demoted): (Vec<_>, Vec<_>) = candidates
        .drain(..)
        .partition(|candidate| !health.should_skip(candidate.definition()));
    candidates.extend(healthy);
    candidates.extend(demoted);
}

/// The request-scoped inputs cost-mode ordering reads (design doc: Engine
/// "Selection policy" and "Cost estimator"). All cache; the request path
/// never touches the database for an estimate.
struct CostContext<'a> {
    estimator: &'a EstimatorState,
    signature: &'a TaskSignature,
    /// The candidate-agnostic cell, read once per request in
    /// `chat_completions` (it also feeds the response `estimate` block).
    signature_cell: CellRead,
    /// The byte-bound input measure — the same number admission reserves
    /// against, reused as the input side of expected cost.
    input_bytes: u64,
}

/// Cost mode's base ordering: ascending expected cost basis, stable, so
/// candidates the estimator prices identically keep their table order.
///
/// Each candidate's expected output is its own warm selection cell's p50
/// when one exists, else the segment's candidate-agnostic p50 — the shared
/// fallback that breaks the cold-start circle where a candidate that never
/// serves never warms and so never gets ordered past. With the shared
/// fallback every candidate prices at the same expected output and the
/// ordering degenerates to rate order, which is exactly the right cold-ish
/// answer. Only when the segment itself is cold (no candidate-agnostic cell
/// either) does the whole route fall through to the identity — the design's
/// cold-fallback, and bit-for-bit today's behavior.
///
/// f64 per-mtok arithmetic prices an ORDERING, never a bill: within a tier
/// every candidate bills at the same tier sell rate (sell-price invariance),
/// so this chooses ZeroRouter's COGS and the customer's odds, never the
/// customer's price. Billing math stays in `Decimal` (`usage_cost`).
fn order_by_expected_cost(candidates: &mut Vec<ProviderCandidate>, estimates: &CostContext<'_>) {
    let shared_fallback = match estimates.signature_cell {
        CellRead::Warm(percentiles) => Some(percentiles.p50),
        CellRead::Cold => None,
    };
    let expected: Vec<Option<f64>> = candidates
        .iter()
        .map(|candidate| {
            let definition = candidate.definition();
            let cell = estimates
                .estimator
                .lookup(&CellKey::for_candidate(estimates.signature, &definition.id));
            let expected_output = match cell {
                CellRead::Warm(percentiles) => percentiles.p50,
                CellRead::Cold => shared_fallback?,
            };
            Some(expected_cost_basis(
                definition.rates,
                estimates.input_bytes,
                expected_output,
            ))
        })
        .collect();
    if expected.iter().any(Option::is_none) {
        // Cold fallback: some rung has no estimate from any grain, so the
        // route keeps the table order rather than sorting on partial data.
        return;
    }
    let mut priced: Vec<(f64, ProviderCandidate)> = expected
        .into_iter()
        .map(|cost| cost.unwrap_or(f64::INFINITY))
        .zip(candidates.drain(..))
        .collect();
    priced.sort_by(|left, right| left.0.total_cmp(&right.0));
    candidates.extend(priced.into_iter().map(|(_, candidate)| candidate));
}

/// A percentile as the wire shows it: output tokens are whole numbers, and
/// the scan cannot produce a negative or astronomically large quantile from
/// nonnegative integer inputs — the saturating cast is belt-and-braces.
fn round_tokens(value: f64) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = value.round().max(0.0) as u64;
    rounded
}

/// Expected COST BASIS of dispatching one candidate: the byte-bound input at
/// the candidate's input rate plus the estimated output at its output rate.
/// A candidate whose rate table cannot price a dimension prices at infinity
/// and sorts last — defensive only; catalog validation refuses such tables.
fn expected_cost_basis(rates: ModelRates, input_bytes: u64, expected_output_tokens: f64) -> f64 {
    // Precision loss on the u64 → f64 cast is irrelevant at ordering
    // magnitudes (bytes are far below 2^52).
    #[allow(clippy::cast_precision_loss)]
    let input_bytes = input_bytes as f64;
    let input_rate = rates.input_per_mtok.unwrap_or(f64::INFINITY);
    let output_rate = rates.output_per_mtok.unwrap_or(f64::INFINITY);
    input_bytes * input_rate / 1_000_000.0 + expected_output_tokens * output_rate / 1_000_000.0
}

#[allow(clippy::too_many_arguments)]
async fn non_streaming_response(
    runtime: RuntimeControl,
    health: ProviderHealth,
    usage_session: UsageSession,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    provider_route: ProviderRoute,
    reservation_usage: OpenAiUsage,
    priority: PriorityResolution,
    estimate: ZeroRouterEstimate,
) -> Result<Response, ApiError> {
    runtime
        .tasks
        .spawn(run_non_streaming(
            runtime.shutdown,
            health,
            usage_session,
            request,
            resolved,
            provider_route,
            reservation_usage,
            priority,
            estimate,
        ))
        .await
        .map_err(|_| ApiError::UpstreamUnavailable)?
}

#[allow(clippy::too_many_arguments)]
async fn run_non_streaming(
    shutdown: CancellationToken,
    health: ProviderHealth,
    usage_session: UsageSession,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    provider_route: ProviderRoute,
    reservation_usage: OpenAiUsage,
    priority: PriorityResolution,
    estimate: ZeroRouterEstimate,
) -> Result<Response, ApiError> {
    let request_id = usage_session.request_id();
    let features = RequestFeatures::from_request(&request, reservation_usage, priority, estimate);
    let tools = request.provider_tools();
    let max_tokens = request.max_tokens;
    // One clock for the whole walk, exactly as the streaming walk keeps one:
    // the upstream deadline belongs to the REQUEST, so it is spent down across
    // candidates and retries and never refreshed for a fresh rung.
    let started = Instant::now();
    let candidates = provider_route.into_candidates();

    // The prompt the walk is actually sending, which a context-window rejection
    // shortens in place. Both this and the flag are walk-scoped, so candidate
    // #1's truncation is still in force for #2..N — preserved deliberately from
    // the delegated walk rather than improved here, since making the walk more
    // available is a resilience change with no baseline to measure it against.
    let mut effective_messages = request.provider_messages();
    let mut context_truncated = false;
    // The last candidate this walk dispatched to, which is what a terminal
    // names instead of the `fallback-chain` sentinel. The sentinel means "no
    // candidate had been selected", and after the unroll that is only true
    // before the first dispatch — where `None` still says it.
    let mut last_candidate: Option<&TierCandidate> = None;
    // The router-owned walk ledger: one row per walk position, drained into
    // whichever terminal settles this request. Pushing a row is also what
    // feeds the health registry — the single funnel, so no terminal can
    // record an outcome health does not see.
    let mut attempts = WalkLedger::new(health.clone());

    'walk: for (position, candidate) in candidates.iter().enumerate() {
        // The walk-time health backstop. Since stage 3a, demotion's first
        // line is `order_candidates` sinking a cooling or error-heavy rung
        // to the back of the route, so a demoted rung is normally never
        // reached; this check catches the rung that cools BETWEEN that
        // ordering and the walk arriving here (a concurrent request's 429),
        // and the rungs an all-demoted or all-failing walk still visits.
        // The guard is the design's never-below-one-candidate floor — a
        // skip is taken only while this walk has already dispatched
        // somewhere or still has somewhere left to go — so health can cost
        // a walk a rung but never cost it the whole walk, and a
        // single-candidate route rides out a cooldown exactly as it rides
        // out the 429 itself.
        if health.should_skip(candidate.definition())
            && (last_candidate.is_some() || position + 1 < candidates.len())
        {
            attempts.push(build_attempt(
                attempts.len() + 1,
                candidate.definition(),
                "health_skipped",
                false,
                Instant::now(),
                AttemptTokens::unknown(),
                false,
                None,
                None,
            ));
            continue;
        }
        // Reset per candidate: a fresh rung starts from the base interval
        // rather than inheriting the last one's exhausted patience.
        let mut backoff_ms = CANDIDATE_BACKOFF_MS;
        for attempt in 0..=CANDIDATE_RETRIES {
            // Checked BEFORE dispatch, not only enforced by dropping a future,
            // so an expiry is attributable instead of destroying the walk's
            // state. Subtractive off the one `started` clock.
            let Some(remaining) = remaining_upstream_time(started) else {
                return Err(settle_walk_terminal(
                    usage_session,
                    &request_id,
                    &resolved,
                    last_candidate,
                    features,
                    attempts.take_rows(),
                    started,
                    WalkTerminal::Timeout,
                )
                .await);
            };
            let attempt_started = Instant::now();
            let attempt_no = attempts.len() + 1;
            let provider_request = ChatRequest {
                messages: &effective_messages,
                tools: (!tools.is_empty()).then_some(tools.as_slice()),
            };

            // `biased`, so a drained deploy wins over an upstream that is about
            // to answer. The flag distinguishes "cancelled before this call
            // started" from "cancelled with a call in flight" — only the latter
            // burnt an upstream request and deserves a ledger row.
            let candidate_started = AtomicBool::new(false);
            let result = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    if candidate_started.load(Ordering::Relaxed) {
                        attempts.push(build_attempt(
                            attempt_no,
                            candidate.definition(),
                            "aborted",
                            false,
                            attempt_started,
                            AttemptTokens::unknown(),
                            false,
                            None,
                            None,
                        ));
                        last_candidate = Some(candidate.definition());
                    }
                    return Err(settle_walk_terminal(
                        usage_session,
                        &request_id,
                        &resolved,
                        last_candidate,
                        features,
                        attempts.take_rows(),
                        started,
                        WalkTerminal::Shutdown,
                    )
                    .await);
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

            let err = match result {
                Err(_elapsed) => {
                    attempts.push(build_attempt(
                        attempt_no,
                        candidate.definition(),
                        "timeout",
                        false,
                        attempt_started,
                        AttemptTokens::unknown(),
                        false,
                        None,
                        None,
                    ));
                    return Err(settle_walk_terminal(
                        usage_session,
                        &request_id,
                        &resolved,
                        last_candidate,
                        features,
                        attempts.take_rows(),
                        started,
                        WalkTerminal::Timeout,
                    )
                    .await);
                }
                Ok(Ok(response)) => {
                    // A blank turn is re-rolled against the same candidate
                    // rather than returned, bounded by the same budget as an
                    // error retry. The guard is `attempt < CANDIDATE_RETRIES`,
                    // so a blank turn on the FINAL attempt is served and billed
                    // — dropping the re-roll would turn the first blank into
                    // that outcome, which is a billing change, not a
                    // resilience one.
                    if attempt < CANDIDATE_RETRIES && retry::is_empty_completion(&response) {
                        let measured = OpenAiUsage::try_from_provider(response.usage.as_ref());
                        attempts.push(build_attempt(
                            attempt_no,
                            candidate.definition(),
                            "validation_failed",
                            false,
                            attempt_started,
                            measured.map_or(AttemptTokens::unknown(), AttemptTokens::measured),
                            false,
                            None,
                            Some("empty_completion"),
                        ));
                        // The discarded response's own usage prices the
                        // attempt's COGS. It never touches `cost_usd`: this is
                        // ZeroRouter's burn, not the customer's bill.
                        if !sleep_backoff(backoff_ms, started, &shutdown).await {
                            return Err(settle_walk_terminal(
                                usage_session,
                                &request_id,
                                &resolved,
                                last_candidate,
                                features,
                                attempts.take_rows(),
                                started,
                                WalkTerminal::Shutdown,
                            )
                            .await);
                        }
                        backoff_ms = retry::next_backoff(backoff_ms);
                        continue;
                    }
                    return serve_completion(
                        usage_session,
                        request_id,
                        resolved,
                        candidate.definition(),
                        response,
                        max_tokens,
                        features,
                        attempts,
                        attempt_no,
                        attempt_started,
                        started,
                    )
                    .await;
                }
                Ok(Err(err)) => err,
            };

            // Evaluation order below is load-bearing: context-window repair,
            // then abandon-on-non-retryable, then move-on-rather-than-wait for
            // a live 429, then wait. Reordering any pair changes how many
            // upstream calls a failing request costs.
            //
            // The classifier is told whether this walk has already spent its
            // one truncation, because the pinned walk gated its whole
            // context-window branch on the same flag (`reliable.rs:1735`) and
            // fell through to the general classifier on a second occurrence. A
            // 429 whose text also names a token limit is the case that pays for
            // it: read as `ContextWindow` twice it would land in the backoff
            // below instead of moving on, buying a third upstream call on a
            // rung the provider has already refused.
            let class = retry::classify(&err, context_truncated);
            attempts.push(build_attempt(
                attempt_no,
                candidate.definition(),
                class.outcome(),
                false,
                attempt_started,
                AttemptTokens::unknown(),
                false,
                None,
                None,
            ));
            // Metadata only: which candidate failed, how, and on which attempt.
            // The upstream's own words are deliberately NOT on this event —
            // `zerorouter::api` is outside the retention boundary, so anything
            // here reaches the operator's sink.
            tracing::warn!(
                request_id,
                requested_model = resolved.requested_model,
                candidate_id = candidate.definition().id,
                upstream_provider = candidate.definition().provider,
                upstream_model = candidate.definition().model,
                attempt = attempt_no,
                outcome = class.outcome(),
                "upstream candidate attempt failed"
            );
            // The upstream's response text, under the one target that may carry
            // it. `compact_error_detail` returns up to 500 characters of the
            // provider's raw HTTP body — `sanitize_api_error` scrubs seven
            // credential prefixes and nothing else, and a 4xx body routinely
            // echoes the request that provoked it — while ZeroRouter's
            // retention contract permits request metadata only
            // (`docs/SECURITY.md`).
            //
            // So this callsite sits INSIDE the retention boundary
            // (`logging::RETENTION_PROTECTED_TARGETS`), which drops it before
            // any sink and which no `RUST_LOG` value can reopen. It costs
            // nothing to keep: `tracing` decides a callsite is enabled before it
            // builds the value set, so under the router's subscriber this never
            // runs the `compact_error_detail` call at all
            // (`logging::a_denied_target_never_evaluates_its_fields`).
            //
            // It is written rather than deleted so that the single place
            // upstream text is formatted is governed by the boundary rather
            // than by a reviewer remembering the rule: a deployment that wants
            // bodies has to move the boundary, once and visibly, instead of
            // adding a field to the event above.
            tracing::warn!(
                target: crate::logging::UPSTREAM_DETAIL_TARGET,
                request_id,
                candidate_id = candidate.definition().id,
                attempt = attempt_no,
                detail = retry::compact_error_detail(&err),
                "upstream candidate attempt detail"
            );

            // A `ContextWindow` here implies the walk has NOT yet truncated —
            // `classify` was told the flag, and returns the class only while the
            // repair is still available. Every path out of this block leaves the
            // attempt loop, which is what keeps the two checks below unreachable
            // for this class and lets `is_rate_limited` stay a pure control-flow
            // predicate (`retry::FailureClass::is_rate_limited`).
            if matches!(class, retry::FailureClass::ContextWindow { .. }) {
                if retry::truncate_for_context(&mut effective_messages) > 0 {
                    context_truncated = true;
                    // Consumes an attempt, deliberately.
                    continue;
                }
                // Nothing left to drop. The prompt cannot fit anywhere, so the
                // walk ends here rather than paying to learn the same thing
                // from every remaining candidate.
                break 'walk;
            }
            if class.is_non_retryable() {
                break;
            }
            // Gated on there being somewhere else to go: on a one-candidate
            // route a 429 is retried like any other transient failure.
            if class.is_rate_limited() && candidates.len() > 1 {
                break;
            }
            if attempt < CANDIDATE_RETRIES {
                let wait = retry::compute_backoff(backoff_ms, &err);
                if !sleep_backoff(wait, started, &shutdown).await {
                    return Err(settle_walk_terminal(
                        usage_session,
                        &request_id,
                        &resolved,
                        last_candidate,
                        features,
                        attempts.take_rows(),
                        started,
                        WalkTerminal::Shutdown,
                    )
                    .await);
                }
                backoff_ms = retry::next_backoff(backoff_ms);
            }
        }
    }

    // Every candidate failed. Unlike the streaming walk, this terminal always
    // settles: returning without one would leak the reservation to the TTL
    // sweep, and the buffered path has never done that.
    Err(settle_walk_terminal(
        usage_session,
        &request_id,
        &resolved,
        last_candidate,
        features,
        attempts.into_rows(),
        started,
        WalkTerminal::Exhausted,
    )
    .await)
}

/// How a walk ended with no completion to return.
///
/// Each variant fixes the status the ledger records and the error the customer
/// sees. All three settle the reservation at zero cost, because nothing reached
/// the customer on any of them — a buffered handler either returns the body or
/// returns an error, with no partial delivery to reason about.
#[derive(Clone, Copy)]
enum WalkTerminal {
    /// Every candidate failed.
    Exhausted,
    /// The request's shared upstream deadline elapsed.
    Timeout,
    /// The router is draining.
    Shutdown,
}

impl WalkTerminal {
    fn status(self) -> i16 {
        match self {
            Self::Exhausted => 502,
            Self::Timeout => 504,
            Self::Shutdown => 503,
        }
    }

    fn api_error(self) -> ApiError {
        match self {
            Self::Exhausted => ApiError::UpstreamUnavailable,
            Self::Timeout => ApiError::UpstreamTimeout,
            Self::Shutdown => ApiError::ServerShuttingDown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Exhausted => "all upstream candidates failed",
            Self::Timeout => "upstream inference deadline exceeded",
            Self::Shutdown => "router draining",
        }
    }
}

/// Release the reservation without a charge and report why.
///
/// The single settle site for every buffered terminal that has no completion:
/// nothing was delivered, so nothing is billed, and the reservation is closed
/// here rather than left for the TTL sweep. A settle failure surfaces as
/// `MeteringUnavailable`, exactly as `persist_usage`'s `?` used to.
#[allow(clippy::too_many_arguments)]
async fn settle_walk_terminal(
    usage_session: UsageSession,
    request_id: &str,
    resolved: &ResolvedRoute,
    candidate: Option<&TierCandidate>,
    features: RequestFeatures,
    attempts: Vec<AttemptRecord>,
    started: Instant,
    terminal: WalkTerminal,
) -> ApiError {
    let (upstream_provider, upstream_model) = candidate.map_or(
        ("fallback-chain", resolved.requested_model.as_str()),
        |candidate| (candidate.provider.as_str(), candidate.model.as_str()),
    );
    let error = match persist_usage(
        usage_session,
        &resolved.requested_model,
        upstream_provider,
        upstream_model,
        candidate,
        OpenAiUsage::default(),
        resolved.sell_rates,
        features,
        None,
        None,
        attempts,
        started,
        terminal.status(),
    )
    .await
    {
        Ok(()) => terminal.api_error(),
        Err(error) => error,
    };
    tracing::warn!(
        request_id,
        requested_model = resolved.requested_model,
        upstream_provider,
        upstream_model,
        terminal = terminal.label(),
        "upstream walk ended without a completion"
    );
    error
}

/// Wait out one backoff interval without outliving the request.
///
/// Two bounds a bare `tokio::time::sleep` would not have, both of which the
/// delegated walk got for free from the timeout and the shutdown select it sat
/// inside: a drained deploy must not be held open for up to a minute of
/// accumulated backoff, and a wait must never push the walk past the very
/// deadline it is retrying inside. Returns whether the wait finished rather
/// than being cut short by shutdown.
async fn sleep_backoff(wait_ms: u64, started: Instant, shutdown: &CancellationToken) -> bool {
    let remaining = remaining_upstream_time(started).unwrap_or(Duration::ZERO);
    let wait = Duration::from_millis(wait_ms).min(remaining);
    tokio::select! {
        biased;
        () = shutdown.cancelled() => false,
        () = tokio::time::sleep(wait) => true,
    }
}

/// Settle and return the completion that won the walk.
#[allow(clippy::too_many_arguments)]
async fn serve_completion(
    usage_session: UsageSession,
    request_id: String,
    resolved: ResolvedRoute,
    candidate: &TierCandidate,
    response: ChatResponse,
    max_tokens: Option<u32>,
    features: RequestFeatures,
    mut attempts: WalkLedger,
    attempt_no: usize,
    attempt_started: Instant,
    started: Instant,
) -> Result<Response, ApiError> {
    let Some(usage) = OpenAiUsage::try_from_provider(response.usage.as_ref()) else {
        // The buffered twin of `complete_synthetic_stream`'s gap, settled by
        // the same rule: no metered usage, no bill.
        //
        // This site used to settle at `reservation_usage` — a BYTE-length
        // input bound plus the whole 4096-token output bound — for a response
        // the customer never receives, since the branch discards the body and
        // answers 503 `metering_unavailable`. On the fixture tier that billed
        // $0.024795 against a real completion's $0.00312: roughly eight times
        // the price of the answer, charged for no answer at all. It was the
        // last non-metered charge left in the crate, and its structural
        // sibling four hundred lines below already settled zero.
        //
        // Loud, because an unbilled delivery is lost revenue and a silent one
        // is a revenue hole that grows unnoticed — and countable afterwards
        // without a new column, by the join documented on
        // `StreamDelivery::settled_usage`.
        log_metering_gap(
            &request_id,
            &resolved,
            Some(candidate),
            false,
            "non_streaming",
        );
        // The served attempt is still recorded, with NULL token columns, so the
        // gap is countable in the ledger and not only in the log. `served` is
        // FALSE: the body is discarded below and the customer gets a 503, so
        // nothing was served — which also keeps the metering-gap detector
        // (a served attempt on a zero-token row) correctly silent.
        attempts.push(build_attempt(
            attempt_no,
            candidate,
            "ok",
            false,
            attempt_started,
            AttemptTokens::unknown(),
            false,
            None,
            None,
        ));
        persist_usage(
            usage_session,
            &resolved.requested_model,
            &candidate.provider,
            &candidate.model,
            Some(candidate),
            OpenAiUsage::default(),
            resolved.sell_rates,
            features,
            None,
            None,
            attempts.into_rows(),
            started,
            502,
        )
        .await?;
        return Err(ApiError::MeteringUnavailable);
    };
    // Labelled from what the model actually emitted — content text, reasoning
    // content, or tool calls. Reasoning used to be excluded, so a thinking
    // model that answered entirely in `reasoning_content` was labelled an empty
    // response and would have trained the success estimator to distrust it.
    let emitted = EmittedOutput::from_response(&response);
    let synthesized_finish = finish_reason(emitted.has_tool_calls(), usage, max_tokens);
    let shape_label = shape_ok(
        emitted,
        tool_args_all_json(&response.tool_calls),
        synthesized_finish,
    );
    // The one attempt whose body becomes the 200 and whose usage prices the
    // settled row. Every other row on this request is `served = false`, and
    // this site returns immediately, which is what keeps
    // `request_attempts_one_served_per_request` satisfiable without any
    // cross-candidate bookkeeping.
    attempts.push(build_attempt(
        attempt_no,
        candidate,
        "ok",
        true,
        attempt_started,
        AttemptTokens::measured(usage),
        false,
        Some(synthesized_finish),
        None,
    ));
    // Read from the ledger before the settle drains it, so the block and the
    // persisted rows cannot diverge.
    let walk_positions = attempts.len();
    let zerorouter = zerorouter_block(features, &attempts);
    persist_usage(
        usage_session,
        &resolved.requested_model,
        &candidate.provider,
        &candidate.model,
        Some(candidate),
        usage,
        resolved.sell_rates,
        features,
        Some(synthesized_finish),
        Some(shape_label),
        attempts.into_rows(),
        started,
        200,
    )
    .await?;
    tracing::info!(
        request_id,
        requested_model = resolved.requested_model,
        upstream_provider = candidate.provider,
        upstream_model = candidate.model,
        input_tokens = usage.prompt_tokens,
        cached_input_tokens = usage.cached_input_tokens(),
        output_tokens = usage.completion_tokens,
        "chat completion served"
    );

    let completion = ChatCompletionResponse::new(
        request_id.clone(),
        resolved.requested_model,
        response,
        usage,
        max_tokens,
        zerorouter,
    );
    let mut completion = Json(completion).into_response();
    insert_header(&mut completion, "x-request-id", &request_id);
    insert_header(
        &mut completion,
        "x-zerorouter-provider",
        &candidate.provider,
    );
    insert_header(&mut completion, "x-zerorouter-model", &candidate.model);
    insert_header(
        &mut completion,
        "x-zerorouter-attempts",
        &walk_positions.to_string(),
    );
    Ok(completion)
}

#[allow(clippy::too_many_arguments)]
fn streaming_response(
    runtime: RuntimeControl,
    health: ProviderHealth,
    usage_session: UsageSession,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    provider_route: ProviderRoute,
    reservation_usage: OpenAiUsage,
    priority: PriorityResolution,
    estimate: ZeroRouterEstimate,
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
            health,
            usage_session,
            metadata,
            request,
            resolved,
            provider_route.into_candidates(),
            reservation_usage,
            priority,
            estimate,
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
    health: ProviderHealth,
    usage_session: UsageSession,
    metadata: StreamMetadata,
    request: ChatCompletionRequest,
    resolved: ResolvedRoute,
    candidates: Vec<ProviderCandidate>,
    reservation_usage: OpenAiUsage,
    priority: PriorityResolution,
    estimate: ZeroRouterEstimate,
) {
    let messages = request.provider_messages();
    let tools = request.provider_tools();
    let max_tokens = request.max_tokens;
    let features = RequestFeatures::from_request(&request, reservation_usage, priority, estimate);
    let started = Instant::now();
    let mut last_candidate = None;
    let mut usage_session = Some(usage_session);
    let mut client_connected = true;
    let mut delivery = StreamDelivery::default();
    // The router-owned walk ledger: one row per walk position, drained into
    // the settle transaction at whichever terminal settles this request.
    // Pushing a row is also what feeds the health registry — the same single
    // funnel the buffered walk records through.
    let mut attempts = WalkLedger::new(health.clone());

    for (position, candidate) in candidates.iter().enumerate() {
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
                    attempts.take_rows(),
                    started,
                    499,
                )
                .await;
            }
            return;
        }
        // The walk-time health backstop, the same rule at the same place as
        // the buffered walk (demotion's first line is `order_candidates`
        // sinking demoted rungs — see the buffered walk's twin comment):
        // a rung found cooling or error-heavy on arrival is recorded and
        // skipped, guarded by the never-below-one-candidate floor so a walk
        // can lose a rung to health but never lose its only dispatch.
        // Checked before the deadline because a skip consumes no walk time
        // — admissibility first, ceilings second.
        if health.should_skip(candidate.definition())
            && (last_candidate.is_some() || position + 1 < candidates.len())
        {
            attempts.push(build_attempt(
                attempts.len() + 1,
                candidate.definition(),
                "health_skipped",
                false,
                Instant::now(),
                AttemptTokens::unknown(),
                false,
                None,
                None,
            ));
            continue;
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
                attempts.take_rows(),
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
                            AttemptTokens::unknown(),
                            false,
                            None,
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
                        attempts.take_rows(),
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
                Ok(Err(err)) => {
                    // The buffered walk's classifier, for the LABEL only — the
                    // control flow stays one dispatch then move on. Migration
                    // 0004 documents `outcome` as what feeds the health
                    // cooldown, so a 429 recorded as a generic upstream error
                    // would hide this rung's state from the thing that has to
                    // notice it. `false`: this walk has no truncation repair,
                    // so no truncation has ever been spent.
                    attempts.push(build_attempt(
                        attempt_no,
                        candidate.definition(),
                        retry::classify(&err, false).outcome(),
                        false,
                        attempt_started,
                        AttemptTokens::unknown(),
                        false,
                        None,
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
                        AttemptTokens::unknown(),
                        false,
                        None,
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
                        attempts.take_rows(),
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
            StreamOptions {
                enabled: true,
                count_tokens: true,
            },
        );
        last_candidate = Some(candidate.definition());
        let mut role_sent = false;
        let mut usage = None;
        let mut tool_index = 0_u32;
        let mut completed = false;
        let mut interruption = None;
        // Per-chunk token_count lower bound + tool-arg JSON validity, used to
        // price and label this attempt if it is abandoned or served.
        let mut estimated_output = 0_u64;
        let mut tool_args_ok = true;
        // Whether the stream died on a 429-shaped error, read off the failure
        // text by the same check the buffered walk applies to a chat error.
        // Label-only: a broken stream falls through to the next candidate
        // whatever its text said.
        let mut stream_rate_limited = false;
        // What this candidate actually put out, folded from the deltas
        // themselves. The shape label used to be derived from
        // `usage.completion_tokens`, which is the provider's accounting rather
        // than a transcript: a stream that emitted nothing while the upstream
        // reported output tokens labelled as a healthy response.
        let mut emitted = EmittedOutput::default();

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
                    // Recorded before the connectivity check: the shape label
                    // describes what the MODEL produced, not what survived the
                    // transport. A hung-up client is a delivery fact, tracked
                    // separately by `StreamDelivery`.
                    emitted.record_text(&chunk.delta);
                    emitted.record_reasoning(chunk.reasoning.as_deref().unwrap_or_default());
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
                    emitted.record_tool_call();
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
                Err(error) => {
                    stream_rate_limited = retry::is_rate_limited(&anyhow::Error::new(error));
                    break;
                }
            }
        }

        if let Some(interruption) = interruption {
            let tokens = attempt_tokens(usage, estimated_output);
            // An interruption does not undo a delivery. If this candidate's
            // output already reached the customer, `settle_stream_interruption`
            // bills it and prices it into `usage_events.cost_basis_usd`, so
            // this attempt is the served one — recording it as a loss put the
            // SAME COGS into `attempts_cost_basis_usd` as well, understating
            // margin by exactly the served attempt's cost while the walk ledger
            // claimed no candidate had served the request at all.
            //
            // `model_output_sent` is the outer-walk flag, but it can only be
            // true for THIS candidate here: every terminal that can follow a
            // delivery returns, so the walk never reaches a second candidate
            // with anything delivered. Every site that sets `served` likewise
            // returns, which is what keeps
            // `request_attempts_one_served_per_request` satisfiable.
            let served = delivery.model_output_sent;
            attempts.push(build_attempt(
                attempt_no,
                candidate.definition(),
                interruption.attempt_outcome(),
                served,
                attempt_started,
                tokens,
                usage.is_none() && tokens != AttemptTokens::unknown(),
                None,
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
                attempts.take_rows(),
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
                emitted,
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
                    &metadata.request_id,
                    &resolved,
                    Some(candidate.definition()),
                    true,
                    "stream_error",
                );
            }
            // The customer received this candidate's partial output, so it is
            // the served attempt even though the stream broke.
            let served = delivery.model_output_sent;
            let tokens = attempt_tokens(usage, estimated_output);
            // A 429 that delivered nothing is labelled as the rate limit it
            // was. Once model output has flowed the story of this attempt is
            // the broken delivery, whatever text the failure carried.
            let outcome = if stream_rate_limited && !delivery.model_output_sent {
                "rate_limited"
            } else {
                "stream_error"
            };
            attempts.push(build_attempt(
                attempt_no,
                candidate.definition(),
                outcome,
                served,
                attempt_started,
                tokens,
                usage.is_none() && tokens != AttemptTokens::unknown(),
                None,
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
                attempts.take_rows(),
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
        // non-served failure — as the 429 it was when the failure text says so
        // — and fall through to the next candidate.
        attempts.push(build_attempt(
            attempt_no,
            candidate.definition(),
            if stream_rate_limited {
                "rate_limited"
            } else {
                "stream_error"
            },
            false,
            attempt_started,
            attempt_tokens(usage, estimated_output),
            usage.is_none() && estimated_output > 0,
            None,
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
            attempts.take_rows(),
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
        log_metering_gap(
            &metadata.request_id,
            resolved,
            candidate,
            true,
            interruption.label(),
        );
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
    mut attempts: WalkLedger,
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
            &metadata.request_id,
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
            AttemptTokens::unknown(),
            false,
            None,
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
            attempts.into_rows(),
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let completion_status = if sender.is_closed() { 499 } else { 200 };
    // Same evidence as the non-streaming sibling, including reasoning content:
    // this path replays a buffered response as a stream, and a response that is
    // entirely reasoning is a non-empty response on both.
    let emitted = EmittedOutput::from_response(&response);
    let has_tool_calls = emitted.has_tool_calls();
    let synthesized_finish = finish_reason(has_tool_calls, usage, max_tokens);
    let shape_label = shape_ok(
        emitted,
        tool_args_all_json(&response.tool_calls),
        synthesized_finish,
    );
    attempts.push(build_attempt(
        attempt_no,
        candidate.definition(),
        "ok",
        true,
        attempt_started,
        AttemptTokens::measured(usage),
        false,
        Some(synthesized_finish),
        None,
    ));
    // Read before the settle drains the ledger — same rule as the buffered
    // serve site.
    let zerorouter = zerorouter_block(features, &attempts);
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
        attempts.into_rows(),
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
        zerorouter,
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
    emitted: EmittedOutput,
    tool_args_ok: bool,
    max_tokens: Option<u32>,
    features: RequestFeatures,
    mut attempts: WalkLedger,
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
            &metadata.request_id,
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
            AttemptTokens::unknown(),
            false,
            None,
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
            attempts.into_rows(),
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    let synthesized_finish = finish_reason(emitted.has_tool_calls(), usage, max_tokens);
    // From the deltas this stream actually emitted, never from
    // `usage.completion_tokens`: the upstream's token count is its accounting,
    // and a stream that emitted nothing while reporting output tokens must
    // label as the empty response it was.
    let shape_label = shape_ok(emitted, tool_args_ok, synthesized_finish);
    // The attempt records what the UPSTREAM reported: we owe the provider for
    // the tokens it burned whether or not the customer stayed to read them,
    // and that is what COGS and the treasury measure. `served` is the
    // customer-side fact and must therefore follow the delivery, not a
    // hardcoded true.
    attempts.push(build_attempt(
        attempt_no,
        candidate.definition(),
        "ok",
        delivery.model_output_sent,
        attempt_started,
        AttemptTokens::measured(usage),
        false,
        Some(synthesized_finish),
        None,
    ));
    // A client that walked away is not billed for output it never received
    // (sol review): this path passed `usage` straight through, so a stream
    // whose deltas all bounced off a closed channel still charged in full.
    // An EMPTY answer is a different thing and still bills — the model
    // produced nothing, the customer received exactly that, and the
    // provider burned tokens either way (see
    // `a_stream_that_emitted_nothing_is_not_rescued_by_reported_output_tokens`).
    // COGS is unaffected in both cases: the attempt row above records what
    // the upstream reported.
    let billable = if delivery.abandoned_by_client() {
        OpenAiUsage::default()
    } else {
        usage
    };
    if delivery.abandoned_by_client() {
        log_metering_gap(
            &metadata.request_id,
            resolved,
            Some(candidate.definition()),
            false,
            "stream_final_abandoned",
        );
    }
    // Read before the settle drains the ledger — same rule as the buffered
    // serve site.
    let zerorouter = zerorouter_block(features, &attempts);
    if persist_usage(
        session,
        &resolved.requested_model,
        &candidate.definition().provider,
        &candidate.definition().model,
        Some(candidate.definition()),
        billable,
        resolved.sell_rates,
        features,
        Some(synthesized_finish),
        Some(shape_label),
        attempts.into_rows(),
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
        zerorouter,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn emit_stream_finish(
    sender: &mpsc::Sender<Event>,
    metadata: &StreamMetadata,
    resolved: &ResolvedRoute,
    candidate: &ProviderCandidate,
    usage: OpenAiUsage,
    finish_reason: &'static str,
    zerorouter: Option<ZeroRouterResponseMetadata>,
) {
    if !send_data(
        sender,
        stream_delta_json(metadata, json!({}), Some(finish_reason)),
    )
    .await
    {
        return;
    }
    if metadata.include_usage
        && !send_data(
            sender,
            stream_usage_json(metadata, usage, zerorouter.as_ref()),
        )
        .await
    {
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
    task_signature: TaskSignature,
    estimator_basis: ReservationBasis,
    require_credits: bool,
) -> Result<UsageSession, ApiError> {
    match begin_usage_session(
        pool,
        key,
        reserved_tokens,
        reserved_output_tokens,
        reserved_cost,
        task_signature,
        estimator_basis,
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
    // What the customer owes, computed before anything is written. Rates that
    // cannot be priced are a metering failure and settle nothing: the request
    // ends in `MeteringUnavailable` and the reservation is released by the TTL
    // sweep unspent, rather than a `Decimal::ZERO` charge being recorded as
    // though the request had genuinely been free.
    let cost_usd = usage_cost(sell_rates, usage).ok_or(ApiError::MeteringUnavailable)?;
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
        priority: Some(features.priority),
    };
    usage_session
        .record(&UsageRecord {
            tier: requested_model.to_owned(),
            upstream_provider: upstream_provider.to_owned(),
            upstream_model: upstream_model.to_owned(),
            usage,
            cost_usd,
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

    use crate::provider::TokenUsage;
    use rust_decimal::Decimal;
    use uuid::Uuid;

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
        // The Responses dialect, because that is what ZeroRouter's owned
        // OpenAI wire speaks. The dialect is incidental to what this test
        // asserts (one attempt row per candidate); it just has to be the
        // one the wire under test parses.
        let stream_body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":",
            "{\"input_tokens\":11,\"output_tokens\":5}}}\n\n",
        );
        let app = Router::new()
            .route(
                "/fail",
                post(|| async {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "upstream down",
                    )
                }),
            )
            .route(
                "/ok",
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
            default_priority: None,
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
            usage_cost(resolved.sell_rates, reservation_usage).expect("sell rates must price"),
            task_signature("walk-user", &[], 1, 128, true, 64),
            ReservationBasis::Cold,
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
            ProviderHealth::default(),
            session,
            metadata,
            request,
            resolved,
            vec![
                ProviderCandidate::against_local_upstream(first, &fail_url),
                ProviderCandidate::against_local_upstream(second.clone(), &ok_url),
            ],
            reservation_usage,
            PriorityResolution::new(None),
            ZeroRouterEstimate::cold(64),
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
            ProviderHealth::default(),
            session,
            metadata,
            request,
            resolved,
            vec![ProviderCandidate::with_provider(candidate.clone(), fake)],
            reservation_usage,
            PriorityResolution::new(None),
            ZeroRouterEstimate::cold(64),
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

    /// An interruption does not undo a delivery: when the customer already has
    /// this candidate's output and the upstream already reported usage, the
    /// shutdown terminal bills that usage — so the attempt it records is the
    /// SERVED one.
    ///
    /// It used to be written `served = false`, which put the same COGS on both
    /// sides of the margin expression at once: once in
    /// `usage_events.cost_basis_usd` (priced from the billed usage) and again
    /// in `attempts_cost_basis_usd` (which sums every non-served attempt).
    /// Margin read low by exactly the served attempt's cost, and the walk
    /// ledger simultaneously claimed no candidate had served the request.
    #[tokio::test]
    async fn a_delivered_attempt_interrupted_mid_stream_is_the_served_attempt() {
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let candidate = walk_candidate("delivered-then-shutdown");
        let resolved = walk_route(vec![candidate.clone()]);
        let request = walk_request();
        let reservation_usage = request.reservation_usage(64);
        let (session, request_id) = admit_walk(&pool, &key, &resolved, reservation_usage).await;
        let metadata = StreamMetadata::new(session.request_id(), "zero/test".to_owned(), false);
        let upstream_usage = TokenUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(20),
            cached_input_tokens: None,
        };
        // Output, then a usage report, then a stall long enough that the only
        // way out of the stream loop is the shutdown the client triggers below.
        let fake = FakeModelProvider::new(
            "delivered-then-shutdown",
            vec![FakeOutcome::Stream(vec![
                FakeStreamStep::text("partial"),
                FakeStreamStep::Usage(upstream_usage),
                FakeStreamStep::Stall(Duration::from_secs(600)),
            ])],
        );

        let (sender, mut receiver) = mpsc::channel(SSE_CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        let client_shutdown = shutdown.clone();
        // Drain as a real client would, and cancel only once model output has
        // actually arrived, so the interruption is provably post-delivery.
        let client = tokio::spawn(async move {
            let mut fired = false;
            while let Some(event) = receiver.recv().await {
                if !fired && format!("{event:?}").contains("partial") {
                    fired = true;
                    client_shutdown.cancel();
                }
            }
            fired
        });
        stream_to_channel(
            sender,
            shutdown,
            ProviderHealth::default(),
            session,
            metadata,
            request,
            resolved,
            vec![ProviderCandidate::with_provider(candidate.clone(), fake)],
            reservation_usage,
            PriorityResolution::new(None),
            ZeroRouterEstimate::cold(64),
        )
        .await;
        assert!(
            client.await.expect("client task should join"),
            "the content delta must reach the client before the shutdown"
        );

        let attempts = query_as::<_, (String, bool, Option<Decimal>)>(
            "SELECT outcome, served, cost_basis_usd FROM request_attempts WHERE request_id = $1",
        )
        .bind(request_id)
        .fetch_all(&pool)
        .await
        .expect("attempts must query");
        let billed = OpenAiUsage::try_from_provider(Some(&upstream_usage))
            .expect("the scripted usage report is usable");
        let served_basis = usage_cost(candidate.rates, billed).expect("candidate rates must price");
        assert_eq!(
            attempts,
            vec![("aborted".to_owned(), true, Some(served_basis))],
            "the attempt whose output the customer received and was billed for is the served one"
        );

        let (cost_usd, cost_basis_usd, attempts_cost_basis_usd, complete) =
            query_as::<_, (Decimal, Option<Decimal>, Option<Decimal>, Option<bool>)>(
                r#"
                SELECT cost_usd, cost_basis_usd, attempts_cost_basis_usd,
                       attempts_cost_basis_complete
                FROM usage_events
                WHERE request_id = $1
                "#,
            )
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .expect("settled row must query");
        assert!(
            cost_usd > Decimal::ZERO,
            "billing is unchanged: a delivered, metered interruption still charges"
        );
        assert_eq!(
            cost_basis_usd.map(|value| value.normalize()),
            Some(served_basis.normalize()),
            "the served attempt's COGS is the settled row's cost basis"
        );
        assert_eq!(
            attempts_cost_basis_usd,
            Some(Decimal::ZERO),
            "no attempt's COGS may be counted on both sides of the margin expression"
        );
        assert_eq!(
            complete,
            Some(true),
            "there were no losing attempts, which is a known zero rather than an unknown"
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
            ProviderHealth::default(),
            session,
            metadata,
            request,
            resolved,
            vec![ProviderCandidate::against_local_upstream(
                candidate.clone(),
                &ok_url,
            )],
            reservation_usage,
            PriorityResolution::new(None),
            ZeroRouterEstimate::cold(64),
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
            usage_cost(candidate.rates, OpenAiUsage::default()),
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

#[cfg(test)]
mod revert_trip_tests {
    use rust_decimal::Decimal;

    use super::{SegmentClampStats, segment_tripped};

    #[allow(clippy::too_many_arguments)]
    fn stats(
        loss_7d: &str,
        max_7d: &str,
        clamped_7d: i64,
        learned_7d: i64,
        loss_14d: &str,
        max_14d: &str,
        clamped_14d: i64,
        learned_14d: i64,
    ) -> SegmentClampStats {
        let decimal = |value: &str| value.parse::<Decimal>().expect("decimal literal");
        SegmentClampStats {
            loss_7d_usd: decimal(loss_7d),
            max_row_loss_7d_usd: decimal(max_7d),
            clamped_rows_7d: clamped_7d,
            learned_rows_7d: learned_7d,
            loss_14d_usd: decimal(loss_14d),
            max_row_loss_14d_usd: decimal(max_14d),
            clamped_rows_14d: clamped_14d,
            learned_rows_14d: learned_14d,
        }
    }

    #[test]
    fn each_trigger_trips_alone() {
        // Sum only: rate 12/3000 = 0.4% under the 0.5% limit, max under $1.
        assert!(segment_tripped(&stats(
            "10.80", "0.90", 12, 3_000, "10.80", "0.90", 12, 3_000
        )));
        // Single row only.
        assert!(segment_tripped(&stats(
            "1.50", "1.50", 1, 3_000, "1.50", "1.50", 1, 3_000
        )));
        // Rate only: 1/10 clamped, dollars tiny.
        assert!(segment_tripped(&stats(
            "0.05", "0.05", 1, 10, "0.05", "0.05", 1, 10
        )));
    }

    #[test]
    fn under_every_threshold_does_not_trip() {
        assert!(!segment_tripped(&stats(
            "9.99", "0.99", 10, 3_000, "9.99", "0.99", 10, 3_000
        )));
    }

    #[test]
    fn an_empty_window_cannot_trip() {
        // 0/0 hit rate is NaN; NaN comparisons are false by design.
        assert!(!segment_tripped(&stats("0", "0", 0, 0, "0", "0", 0, 0)));
    }

    #[test]
    fn the_rederivation_window_trips_when_the_trigger_window_has_aged_clean() {
        // Evidence older than 7 days but inside 14: the restart story.
        assert!(segment_tripped(&stats(
            "0", "0", 0, 0, "1.50", "1.50", 1, 40
        )));
    }
}

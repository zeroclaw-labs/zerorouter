use std::{
    collections::BTreeSet,
    convert::Infallible,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::provider::BASELINE_MAX_TOKENS;
use crate::provider::{
    ChatRequest, ChatResponse, ModelRates, RateSchedule, StreamEvent, StreamFinal, StreamOptions,
    UsageGap,
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
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    auth::{AuthenticatedKey, AuthenticationError, KeyAuthenticator},
    byok,
    config::{RequestNeeds, ResolvedRoute, TierCandidate, TierCatalog, load_tier_catalog},
    db::{
        AttemptRecord, AttemptTokens, ByokReservation, MeteringLane, RequestTelemetry,
        ReservationSize, ReservationSizing, SegmentClampStats, SettlementRecovery, UsageAdmission,
        UsageRecord, UsageSession, begin_usage_session, output_token_percentiles,
        recover_owed_settlements, segment_clamp_stats, segment_user, user_clamp_loss,
    },
    error::{ApiError, streaming_error_json},
    estimator::{
        CellKey, CellRead, EstimatorState, REFRESH_BATCH, REFRESH_INTERVAL, ROW_LOSS_LIMIT_USD,
        SEGMENT_HIT_RATE_LIMIT, SEGMENT_LOSS_LIMIT_7D_USD, USER_LOSS_LIMIT_30D_USD,
        learned_output_bound,
    },
    health::{ProviderHealth, WalkLedger},
    openai::{
        AttemptFinishReason, ChatCompletionRequest, ChatCompletionResponse, EmittedOutput,
        EstimateBasis, ModelList, OpenAiUsage, StreamMetadata, TaskSignature, ZeroRouterAttempt,
        ZeroRouterEstimate, ZeroRouterResponseMetadata, finish_reason, shape_ok, stream_delta_json,
        stream_tool_call_delta, stream_usage_json, task_signature, tool_args_all_json, usage_cost,
    },
    priority::Priority,
    providers::{ByokCredentials, ProviderBuildError, ProviderCandidate, ProviderRoute},
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
    /// Whether the attempt this settle site is about to record dispatched on
    /// the CUSTOMER's own provider credential (migration 0026).
    ///
    /// It lives here for one reason: `RequestFeatures` is already carried to
    /// every settle site and to `zerorouter_block`, which are exactly the two
    /// consumers — the price and the disclosure. Threading a parallel `bool`
    /// through six walk functions to reach the same eleven call sites would be
    /// the same fact travelling twice, and the failure mode of that is a site
    /// that gets the disclosure right and the price wrong.
    ///
    /// `false` at construction and set by the walk when a candidate is chosen
    /// ([`RequestFeatures::on_candidate`]), so the default is the house lane:
    /// a settle site reached without the walk ever selecting a candidate —
    /// every fallback-chain terminal — prices at catalog, which is correct,
    /// because nothing was dispatched on a customer key.
    byok_served: bool,
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
            byok_served: false,
        }
    }

    /// The same features, describing an attempt about to run against
    /// `candidate`.
    ///
    /// Returns a new value rather than mutating, because `RequestFeatures` is
    /// `Copy` and the walk hands a copy to each attempt: a candidate that fails
    /// over must not leave its own BYOK-ness on the features the NEXT candidate
    /// settles with. Every settle site downstream of a selection therefore
    /// describes the attempt it is actually recording.
    fn on_candidate(self, candidate: &ProviderCandidate) -> Self {
        Self {
            byok_served: candidate.is_byok(),
            ..self
        }
    }

    /// The fee multiplier this attempt settles at: 5% of catalog on the
    /// customer's own credential, the full catalog price on ZeroRouter's.
    fn byok_rate(self) -> Decimal {
        if self.byok_served {
            byok::fee_rate()
        } else {
            byok::house_rate()
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
///
/// BYOK is the second reason the block appears, and it is a disclosure rather
/// than a feature signal: a request served on the customer's own credential is
/// governed by the customer's agreement with the provider, not by ZeroRouter's
/// catalog labels, and the house retention attestation is not asserted on it.
/// A customer who never touched the priority knob still has to be told that,
/// on the response itself. Byte-stability is unaffected for everyone else —
/// the condition is only ever widened by a fact about the customer's OWN
/// configuration, and `byok` skips serialization when false.
fn zerorouter_block(
    features: RequestFeatures,
    attempts: &WalkLedger,
) -> Option<ZeroRouterResponseMetadata> {
    (features.knob_engaged || features.byok_served).then(|| ZeroRouterResponseMetadata {
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
        byok: features.byok_served,
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
    //
    // The band comes from the same usage being priced, so a MEASURED attempt
    // is exact: its reported prompt selects the band the vendor billed us at.
    //
    // An output-floor attempt has no prompt — `priceable` reconstructs it as 0
    // — and on a candidate that reprices, that silently selects the BASE band.
    // The resulting figure prices a possibly-300,000-token request at the
    // short-request rate, and it does so on precisely the long-context traffic
    // conditional rates were added for. So this leaves it NULL instead: the
    // ledger's word for "not captured", which is what an unpriceable band
    // honestly is. `attempts_cost_basis_complete` was already FALSE for such a
    // row (it requires every token dimension), so the sum a reader sees is
    // still explicitly a lower bound — it just stops including a number chosen
    // from a band nothing supports.
    //
    // Borrowing the reservation's byte bound to pick the band is rejected for
    // the reason [`attempt_tokens`] rejects it for the tokens themselves: the
    // bound over-counts by roughly the bytes-per-token ratio, so it would pick
    // the HIGH band on requests that never reached it, trading a known gap for
    // an error of unknown size and direction.
    //
    // A FLAT candidate is unaffected — there is only one band, so an unknown
    // prompt cannot select the wrong one, and its floor is recorded exactly as
    // before.
    let cost_basis_usd = tokens.priceable().and_then(|usage| {
        let band_is_knowable = candidate.rates.is_flat() || tokens.input.is_some();
        band_is_knowable
            .then(|| usage_cost(candidate.rates.at_prompt_tokens(usage.prompt_tokens), usage))
            .flatten()
    });
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
    ///
    /// # What this query does NOT see, since edge mode's stage 3
    ///
    /// **Free-lane gaps are invisible to it.** The join is on
    /// `request_attempts`, and the free lane writes no attempt rows — its
    /// observability row rides no settle transaction to insert them in. So a
    /// local upstream that answers without reporting usage produces a $0 row
    /// with zero tokens and NOTHING to join against, and drops out of the audit
    /// entirely. That is tolerable only because the amount at stake is
    /// definitionally zero: a free-lane gap is a hole in a dashboard, not in a
    /// bill, and this query exists to find unbilled revenue. It is recorded
    /// here rather than left to be rediscovered, because the query reads like a
    /// complete census and is no longer one.
    ///
    /// The related asymmetry on the same rows: `usage_events.attempt_count` IS
    /// written on a free-lane row (the walk really did consume that many
    /// positions), while the `request_attempts` rows it counts do not exist. A
    /// query that treats `attempt_count` as a row count against that table will
    /// come up short on exactly these rows. Both are one decision — attempts
    /// are deliberately not written off-path — and closing it would mean a
    /// second async insert on the free lane's hot path, which is a trade worth
    /// making only if free-lane attempt detail is wanted for its own sake.
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

/// Cadence of the abandoned-checkout-intent cleanup sweep.
///
/// Hourly rather than the minute the money sweeps run at, because this one is
/// maintenance: its rows have already sat untouched for
/// `stripe::CHECKOUT_INTENT_RETENTION_DAYS`, so nothing is improved by noticing
/// them sooner, and the interval only has to keep up with the rate abandoned
/// checkouts accumulate — which it does by three orders of magnitude.
const CHECKOUT_INTENT_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

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
    /// Which providers this deployment can dispatch to, for the catalog filter.
    ///
    /// `None` — every production path — asks the environment through
    /// [`crate::providers::provider_is_dispatchable`], because the environment
    /// is what actually decides. A `Some` is a test stating the deployment it
    /// means to describe, and there is no way to reach it from `serve`: the only
    /// constructor that sets it is [`RouterState::fully_credentialed`], which
    /// production does not call (`main.rs` builds state with
    /// [`RouterState::with_database`]).
    dispatchable: Option<DispatchableProviders>,
    services: Option<Arc<RouterServices>>,
}

/// "Can this deployment dispatch to this provider?", as a value.
type DispatchableProviders = Arc<dyn Fn(&str) -> bool + Send + Sync>;

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
    /// The key customers' own provider credentials are sealed under, or `None`
    /// when this deployment has not provisioned one.
    ///
    /// `None` is not "BYOK is off for this request" — it is "this deployment
    /// cannot read a BYOK credential at all", so the dispatch path never looks
    /// one up and every request takes exactly the house-credential path it took
    /// before this feature existed. That is what makes shipping dark free of
    /// risk to the metered lane.
    byok: Option<Arc<crate::byok::Keyring>>,
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
    ///
    /// Reads provider credentials from the environment exactly as production
    /// does, so a test built this way sees the catalog its own environment can
    /// serve — usually empty, since a test process holds no provider keys. That
    /// is the honest default: it is what a deployment with no secrets publishes.
    /// A test asserting the full catalog wants
    /// [`RouterState::fully_credentialed`] and should say so.
    #[must_use]
    pub fn new(tier_config_path: impl Into<PathBuf>) -> Self {
        Self {
            tier_config_path: Arc::new(tier_config_path.into()),
            dispatchable: None,
            services: None,
        }
    }

    /// A catalog-surface state for a deployment that holds EVERY provider's
    /// credential.
    ///
    /// The counterpart to [`RouterState::new`], and it exists so a test that
    /// asserts what the storefront publishes has to state which deployment it
    /// is describing. Before the catalog consulted credentials at all there was
    /// nothing to state, and that silence is what let the Bedrock lanes ship
    /// listed-but-unservable: every test described a fully-credentialed
    /// deployment and none of them said so, so nothing failed when production
    /// turned out not to be one.
    ///
    /// Not reachable from `serve`, which builds state through
    /// [`RouterState::with_database`].
    #[must_use]
    pub fn fully_credentialed(tier_config_path: impl Into<PathBuf>) -> Self {
        Self {
            dispatchable: Some(Arc::new(|_| true)),
            ..Self::new(tier_config_path)
        }
    }

    /// A catalog-surface state for a deployment holding only `providers`' keys.
    ///
    /// The shape of every real partial deployment, including the one that
    /// caused the incident: region configured, `BEDROCK_API_KEY` absent.
    #[must_use]
    pub fn credentialed_for(tier_config_path: impl Into<PathBuf>, providers: &[&str]) -> Self {
        let providers: Vec<String> = providers.iter().map(|name| (*name).to_owned()).collect();
        Self {
            dispatchable: Some(Arc::new(move |provider| {
                providers.iter().any(|name| name == provider)
            })),
            ..Self::new(tier_config_path)
        }
    }

    /// The catalog filter this state publishes through.
    fn dispatchable(&self) -> &(dyn Fn(&str) -> bool + Send + Sync) {
        self.dispatchable
            .as_deref()
            .unwrap_or(&crate::providers::provider_is_dispatchable)
    }

    #[must_use]
    pub fn with_database(
        tier_config_path: impl Into<PathBuf>,
        pool: PgPool,
        require_credits: bool,
    ) -> Self {
        Self {
            tier_config_path: Arc::new(tier_config_path.into()),
            dispatchable: None,
            services: Some(Arc::new(RouterServices {
                pool,
                authenticator: KeyAuthenticator::new(),
                runtime: RuntimeControl::new(),
                require_credits,
                health: ProviderHealth::default(),
                estimator: EstimatorState::default(),
                byok: None,
                #[cfg(feature = "testing")]
                injected_route: None,
            })),
        }
    }

    /// Attach the BYOK keyring this deployment read from the environment.
    ///
    /// A builder rather than a fourth argument to [`Self::with_database`] for
    /// the reason [`crate::web::WebCtx::with_byok`] gives: every existing
    /// construction site describes a deployment without BYOK, which is what
    /// they are, and a test that means otherwise has to say so.
    #[must_use]
    pub fn with_byok(mut self, byok: Option<Arc<crate::byok::Keyring>>) -> Self {
        if let Some(services) = self.services.take() {
            // `RouterServices` is behind an `Arc` that nothing else holds yet at
            // construction time, so this unwraps rather than clones. Falling
            // back to leaving the state untouched keeps the builder total: a
            // caller that has already cloned the state gets no BYOK rather than
            // a panic, and the only way to reach that is to call this after
            // spawning background tasks, which `serve` does not do.
            match Arc::try_unwrap(services) {
                Ok(mut services) => {
                    services.byok = byok;
                    self.services = Some(Arc::new(services));
                }
                Err(shared) => {
                    tracing::error!(
                        "BYOK keyring could not be attached: the router state was already shared"
                    );
                    self.services = Some(shared);
                }
            }
        }
        self
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
            dispatchable: None,
            services: Some(Arc::new(RouterServices {
                pool,
                authenticator: KeyAuthenticator::new(),
                runtime: RuntimeControl::new(),
                require_credits,
                health: ProviderHealth::default(),
                estimator: EstimatorState::default(),
                byok: None,
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

    /// Start the abandoned-checkout cleanup sweep: hourly, delete
    /// `stripe_checkout_intents` rows for sessions that were created, never
    /// paid, and are now past anything Stripe can still do with them. Same
    /// opt-in serve-only contract as the other background loops; tests drive
    /// `stripe::run_checkout_intent_cleanup_once` directly.
    ///
    /// Unlike [`RouterState::spawn_autopay_sweep`] this takes no
    /// [`crate::web::StripeSettings`] and is started even when Stripe is not
    /// configured. The rows are ZeroRouter's own and outlive the integration
    /// that wrote them: an operator who turns Stripe off would otherwise keep
    /// the accumulated backlog forever, and the cost of the loop on a
    /// deployment that never had checkout is one indexed query an hour that
    /// matches nothing.
    pub fn spawn_checkout_intent_cleanup(&self) {
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
                    () = tokio::time::sleep(CHECKOUT_INTENT_CLEANUP_INTERVAL) => {}
                }
                crate::stripe::run_checkout_intent_cleanup_once(&pool).await;
            }
        });
    }

    /// Start the redemption-tax sweep: hourly, tile new usage into periods,
    /// price them, and — in `collect` mode — debit and record them. Same
    /// opt-in serve-only contract as the other background loops; tests drive
    /// `redemption_tax::run_redemption_tax_sweep_once` directly. The caller
    /// only spawns this when the mode is not `Off`, which keeps the default
    /// deployment free of even the idle loop.
    pub fn spawn_redemption_tax_sweep(
        &self,
        settings: crate::web::StripeSettings,
        mode: crate::redemption_tax::RedemptionTaxMode,
    ) {
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
                    () = tokio::time::sleep(Duration::from_secs(3600)) => {}
                }
                crate::redemption_tax::run_redemption_tax_sweep_once(&pool, &settings, mode).await;
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
    /// Which providers this user has attached their own credential for.
    ///
    /// Empty — with no query at all — on a deployment that has not provisioned
    /// `BYOK_ENCRYPTION_KEY`, which is what makes the feature free to ship
    /// dark: the reservation path takes exactly the shape it had before BYOK
    /// existed. A database error is also empty rather than an error, because
    /// the honest failure direction here is the HOUSE rate: a request that
    /// cannot confirm the customer's coverage must reserve the larger amount,
    /// never the smaller one.
    async fn byok_covered_providers(&self, user_id: uuid::Uuid) -> BTreeSet<String> {
        if self.byok.is_none() {
            return BTreeSet::new();
        }
        match crate::byok::covered_providers(&self.pool, user_id).await {
            Ok(providers) => providers.into_iter().collect(),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "could not read BYOK coverage; pricing this request at catalog rates"
                );
                BTreeSet::new()
            }
        }
    }

    /// The customer's own credentials for this request, opened for the span of
    /// the route being built and dropped with it.
    async fn byok_credentials(&self, user_id: uuid::Uuid) -> ByokCredentials {
        let Some(keyring) = &self.byok else {
            return ByokCredentials::default();
        };
        match crate::byok::open_credentials(&self.pool, keyring, user_id).await {
            Ok(pairs) => ByokCredentials::new(pairs),
            Err(error) => {
                // The house lane is the fallback for a READ failure, which is
                // not the same as the "no silent fallback" rule: that rule is
                // about a customer's key being REJECTED upstream, where falling
                // back would surprise-bill them at full price for a request
                // they expected to pay 5% on. Here nothing has been dispatched
                // yet and no reservation has been taken at the BYOK rate — the
                // coverage read above failed the same way and already sized
                // this request at catalog — so serving on the house key is both
                // billed correctly and better than refusing.
                tracing::warn!(
                    error = %error,
                    "could not read BYOK credentials; dispatching on house credentials"
                );
                ByokCredentials::default()
            }
        }
    }

    async fn provider_route(
        &self,
        resolved: &ResolvedRoute,
        max_output_tokens: u32,
        user_id: uuid::Uuid,
    ) -> Result<ProviderRoute, ApiError> {
        #[cfg(feature = "testing")]
        if let Some(route) = &self.injected_route {
            return Ok(route(resolved, max_output_tokens));
        }
        let byok = self.byok_credentials(user_id).await;
        ProviderRoute::new_with_byok(resolved.candidates.clone(), max_output_tokens, &byok)
            .await
            .map_err(|error| {
                match error {
                    // The catalog no longer lists this lane, so the honest answer
                    // names it rather than blaming the upstream fleet. The two
                    // answers must agree: `/v1/models` omits a lane whose credential
                    // is absent, and a request that names it anyway is told THAT lane
                    // is unavailable — not that no provider anywhere is up, which
                    // reads like an outage and sends an operator looking at the
                    // wrong thing. `model_unavailable` is the same code a tier
                    // withheld for below-cost pricing returns, and deliberately so:
                    // both mean "ZeroRouter cannot serve this and you cannot fix it".
                    ProviderBuildError::NoAvailableCredentials { .. } => {
                        ApiError::ModelUnavailable {
                            tier: resolved.requested_model.clone(),
                        }
                    }
                    _ => ApiError::NoProviderAvailable,
                }
            })
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
        .route("/transparency", get(crate::transparency::transparency))
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
    // Only lanes this deployment can actually serve. A catalog that advertises
    // a model whose credential is absent is a storefront selling something the
    // till refuses, and it is worse than useless for the lane it hid behind:
    // the Bedrock zero-retention lanes shipped listed and unservable, which is
    // the incident this argument exists to prevent recurring.
    Ok(Json(ModelList::from_listing(
        catalog.model_listing(state.dispatchable()),
    )))
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
    // Capability gate. A request carrying an image that NO rung of the
    // resolved route can take is refused here — after resolution, because the
    // answer names the model, and before `sized_reservation` (below),
    // `provider_route` (which mints Vertex OAuth) and `admit_usage` (which
    // takes the money), because a request that cannot be served must not move
    // any of them first. Same placement principle as `priority_conflict`.
    if let Some(refusal) = unservable_modality(&request, &resolved) {
        return Err(refusal);
    }
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
    // Both sizings are measured here and the choice between them is made
    // inside admission. The remaining gate — how many requests this user
    // already has in flight — is only trustworthy under the per-user advisory
    // lock, which admission holds and this seam does not (sol review #1,
    // `LEARNED_SIZING_CONCURRENCY_LIMIT`). Measuring is pure and cheap; the
    // learned arm is measured only when the estimator offered a bound.
    // Bring-your-own-key, read once for this request. Two lookups rather than
    // one, and the split is deliberate: the reservation only needs to know
    // WHICH providers are covered, so admission never decrypts anything, and
    // the credentials themselves are opened only on the path that is about to
    // dispatch with them. On a deployment with no `BYOK_ENCRYPTION_KEY` — and
    // for the overwhelming majority of users on one that has it — both are a
    // single indexed read returning nothing, and every line below behaves
    // exactly as it did before this feature existed.
    let byok_covered = services.byok_covered_providers(authenticated.user_id).await;
    let byok_rate = byok_reservation_rate(&resolved, &byok_covered);
    let full_sizing = sized_reservation(&request, &resolved, max_output_tokens, byok_rate)?;
    let learned_sizing = learned_bound
        .map(|bound| sized_reservation(&request, &resolved, bound, byok_rate))
        .transpose()?;
    // What admission needs to price this request against the customer's monthly
    // allowance (migration 0027). Measured here, decided under the lock — see
    // [`ByokReservation`].
    let byok_posture = byok_reservation_posture(
        &request,
        &resolved,
        max_output_tokens,
        &byok_covered,
        byok_rate,
    )?;
    // Route construction and ordering read the FULL byte-bound usage: the
    // generation limit sent upstream is untouched by sizing, and the cost
    // ordering prices a walk rather than a reservation.
    let mut provider_route = services
        .provider_route(&resolved, max_output_tokens, authenticated.user_id)
        .await?;
    // The one definition of $0 that ships: both halves of the declaration
    // (`config::TierCandidate::is_free`), read from server-side configuration
    // at selection time.
    let is_free = TierCandidate::is_free;
    order_candidates(
        priority.resolved,
        provider_route.candidates_mut(),
        &CostContext {
            estimator: &services.estimator,
            signature: &signature,
            signature_cell,
            input_bytes: reservation_usage.prompt_tokens,
            is_free: &is_free,
        },
        &services.health,
        // Mechanical eligibility reads the same byte bound the cost ordering
        // and admission do, so every one of them describes the same request.
        &request.needs(reservation_usage.prompt_tokens),
    );
    // The metering seam (edge mode, stage 3). Decided AFTER ordering, over the
    // route the walk will actually take, and read from server-side
    // configuration only — see [`free_lane_admissible`].
    let lane = if free_lane_admissible(&resolved, provider_route.candidates(), &is_free) {
        MeteringLane::Free
    } else {
        MeteringLane::Reserved
    };
    let usage_session = admit_usage(
        &services.pool,
        &authenticated,
        ReservationSizing {
            learned: learned_sizing,
            full: full_sizing,
        },
        byok_posture,
        signature,
        services.require_credits,
        lane,
    )
    .await?;
    // Re-measure against the bound admission actually took. This is what the
    // walk bills when an upstream reports no usage, so reading it back rather
    // than assuming the learned bound is what keeps a concurrency-gated
    // request's fallback and its reservation describing the same request.
    let reservation_usage = request.reservation_usage(usage_session.reserved_output_tokens());
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

/// Whether this request may skip reserve, the per-user advisory lock, and
/// settle — the metering seam (edge mode, stage 3:
/// `docs/design/edge-mode-local-rung.md`).
///
/// Two conditions, both necessary, evaluated over the route the walk is about
/// to take:
///
/// 1. **Every candidate is free.** Not the first one, not the one that is
///    likely to serve — every one. The reservation is taken at ADMISSION,
///    before anything is known about which rung will answer, so a route that
///    holds even one metered candidate must reserve for it: a fallback that
///    dispatches to a paid upstream without a reservation is paid inference
///    delivered with no encumbrance, no exactly-once settle, and no way to
///    charge for it — the precise failure this seam exists to avoid. Freeness
///    is [`TierCandidate::is_free`], recomputed here from server-side
///    configuration; nothing about a request, a header, or a model alias
///    reaches it.
/// 2. **The tier sells at zero.** [`ResolvedRoute::sells_free`] carries the
///    argument in full: candidate freeness is a claim about ZeroRouter's cost,
///    and the customer pays the TIER's rate, so a $0 basis under a priced tier
///    is a legal 100%-margin configuration that this predicate must not give
///    away.
///
/// Together they mean the skip engages only where the metered path would have
/// computed `cost_usd = 0`, debited nothing, and written no ledger row — so
/// nothing a customer is charged can change, on any route, in either
/// direction. What changes is the reservation, the lock, and the settle
/// transaction, which on such a route were pure overhead.
///
/// # The consequence, stated plainly
///
/// The latency win belongs to routes composed ENTIRELY of free rungs — an
/// all-local tier, or a local model addressed as a pin. **The
/// local-first/cloud-burst ladder keeps full metering**, on every request,
/// including the ones the local rung serves. That is not an oversight to be
/// optimized away later: reserving at admission is what makes the burst
/// billable, and the burst is the whole point of a hybrid ladder.
///
/// # Fail-closed
///
/// An empty route takes the metered lane. `all()` is vacuously true over
/// nothing, and "no candidates" must never be the way metering gets turned
/// off — [`ProviderRoute::new`] already refuses an empty route, so this guard
/// exists to make the reasoning independent of that.
fn free_lane_admissible(
    resolved: &ResolvedRoute,
    candidates: &[ProviderCandidate],
    is_free: &dyn Fn(&TierCandidate) -> bool,
) -> bool {
    resolved.sells_free()
        && !candidates.is_empty()
        && candidates
            .iter()
            .all(|candidate| is_free(candidate.definition()))
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

/// Refuse a request whose input modalities NO rung of the resolved route
/// declares it can take.
///
/// Three rules, and each one is load-bearing:
///
/// - **Unknown is never a refusal.** A candidate that declares no
///   `input_modalities` serves everything, exactly as
///   [`crate::config::ModelMetadata::can_serve`] has it. Several shipped
///   lanes omit the field deliberately because their two sources disagree
///   (`tiers.toml` argues each one at the tier), and those lanes take images
///   in reality — turning silence into a refusal would break working requests
///   to buy a nicer error for hypothetical ones.
/// - **Any rung that can serve saves the route.** The check is over the whole
///   candidate list, not the tier's narrowed intersection, because the walk
///   will reach a later rung. Every shipped tier is one candidate today, so
///   this reduces to the obvious case; it is written for the mixed tier.
/// - **Only the modality is judged here.** `can_serve` also weighs the prompt
///   bound against the context window, and that comparison is BYTES against
///   TOKENS — fine for sinking a candidate down an ordering, a false refusal
///   if it were promoted into a 400.
fn unservable_modality(
    request: &ChatCompletionRequest,
    resolved: &ResolvedRoute,
) -> Option<ApiError> {
    // `text` is not gated: every lane that declares anything declares it, and
    // a lane that somehow did not would be refusing plain text.
    let needs_image = request
        .needs(0)
        .modalities
        .contains(crate::openai::IMAGE_MODALITY);
    if !needs_image {
        return None;
    }
    let takes_image = |candidate: &crate::config::TierCandidate| {
        candidate
            .metadata
            .input_modalities
            .as_ref()
            .is_none_or(|declared| {
                declared
                    .iter()
                    .any(|modality| modality == crate::openai::IMAGE_MODALITY)
            })
    };
    if resolved.candidates.iter().any(takes_image) {
        return None;
    }
    // Every rung declared a list and none of them had `image`. Report what
    // the FIRST rung accepts: with one candidate it is exactly the truth, and
    // with several it is the one the request would have been served by.
    let accepted = resolved
        .candidates
        .first()
        .and_then(|candidate| candidate.metadata.input_modalities.as_ref())
        .map_or_else(|| "text".to_owned(), |declared| declared.join(", "));
    Some(ApiError::ModalityUnsupported {
        model: resolved.requested_model.clone(),
        modality: crate::openai::IMAGE_MODALITY,
        accepted,
    })
}

/// Selection policy (design doc: Engine "Selection policy"), applied to the
/// built route before either walk starts.
///
/// Since stage 3b, `cost` orders ascending by expected cost basis —
/// estimator-backed, with a whole-route fall-through to the identity while
/// the segment is cold (`order_by_expected_cost`), and since edge mode's
/// stage 2, with $0 rungs ahead of all of it. `balanced` stays the
/// identity — the tiers.toml order, the human-curated prior and the frozen
/// control group. `success` stays the identity until its estimator and
/// escalation machinery arrive in stage 5a.
///
/// Health demotion applies next, in every mode: demoted rungs sink to the
/// back — preserving table order within each group — and never disappear.
/// Sinking replaces the recorded skip as demotion's first line (stage 2b
/// shipped the skip while ordering belonged to this stage); the walk-time
/// `should_skip` check remains as the backstop for a rung that cools between
/// this ordering and the walk reaching it, and its never-below-one-candidate
/// floor is unchanged. An all-demoted route partitions to itself, so this
/// can never manufacture an empty route.
///
/// **Mechanical eligibility applies last and outermost, in cost mode only**
/// (edge mode, stage 2: `docs/design/edge-mode-local-rung.md`). A rung whose
/// DECLARED capabilities cannot take this request — the prompt overflows its
/// context window, the request needs tools it does not have — sinks behind
/// every rung that can, whatever cost mode preferred and whatever health
/// thinks. It is outermost because the two verdicts differ in kind: a demoted
/// rung is one that has been failing and might still work, while an ineligible
/// rung is one the operator has told us cannot take this request at all.
/// Preferring a might-work rung over a stated-cannot one is the right order,
/// and it is what makes "the local rung overflows, so this request bursts to
/// cloud" fall out of the existing failover machinery rather than needing a new
/// refusal path.
///
/// It is scoped to cost mode because that is where the $0 rung it exists for is
/// specified, and because balanced is the frozen control group: a multi-
/// candidate PAID tier in balanced mode must keep ordering exactly as it did
/// before this stage, and a rule that reordered it on declared metadata would
/// silently change the control group's behaviour — for tiers that have nothing
/// to do with edge mode. The cost is real and accepted: in balanced mode an
/// oversized prompt still meets the rung that cannot hold it, and the walk
/// handles it exactly as it did before stage 2.
///
/// Sinking, never removing — the same discipline as health, and for the same
/// reason. An all-ineligible route partitions to itself, so a single-candidate
/// tier still dispatches and still gets whatever answer the upstream gives,
/// exactly as it does today. Nothing here can empty a route.
fn order_candidates(
    priority: Priority,
    candidates: &mut Vec<ProviderCandidate>,
    estimates: &CostContext<'_>,
    health: &ProviderHealth,
    needs: &RequestNeeds,
) {
    match priority {
        // Since 3b, cost orders by expected cost basis (cold-fallback:
        // identity), with $0 rungs first since edge mode's stage 2.
        // balanced: identity by definition, the frozen control group.
        // success: identity until its machinery arrives in 5a.
        Priority::Cost => order_by_expected_cost(candidates, estimates),
        Priority::Balanced | Priority::Success => {}
    }
    let (healthy, demoted): (Vec<_>, Vec<_>) = candidates
        .drain(..)
        .partition(|candidate| !health.should_skip(candidate.definition()));
    candidates.extend(healthy);
    candidates.extend(demoted);
    if priority == Priority::Cost {
        let (able, unable): (Vec<_>, Vec<_>) = candidates
            .drain(..)
            .partition(|candidate| candidate.definition().metadata.can_serve(needs));
        candidates.extend(able);
        candidates.extend(unable);
    }
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
    /// How a candidate is recognized as a $0 rung.
    ///
    /// Injected rather than called directly because
    /// [`TierCandidate::is_free`] reads the process-wide provider inventory —
    /// installing one is a once-per-process act, so the ordering MECHANISM
    /// could otherwise only be tested in a binary that had installed a
    /// particular inventory. Production passes `TierCandidate::is_free` and
    /// nothing else; there is no second definition of free, and the real one
    /// is exercised end to end in `tests/local_candidates.rs`.
    is_free: &'a dyn Fn(&TierCandidate) -> bool,
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
///
/// **$0 rungs come first, ahead of all of that** (edge mode, stage 2). Free is
/// free at every prompt length and every output length, so a zero-cost rung
/// needs no estimate to be known cheapest — and making it wait for one would
/// be the cold-start circle at its worst: the whole reason a local rung exists
/// is to serve traffic on a router that has never served any. That is why the
/// $0 partition sits OUTSIDE the estimator's whole-route cold fallback rather
/// than pricing free rungs at 0.0 inside it, where one cold priced rung would
/// return the route to table order and take the free rung's advantage with it.
///
/// Free rungs keep their table order among themselves: they price identically,
/// so there is nothing to sort on, and the operator's order is the only
/// statement of preference available. A route with no $0 rung partitions to
/// `priced` entirely and orders bit-for-bit as it did before this existed.
///
/// The predicate is [`TierCandidate::is_free`] — server-side configuration
/// only, one definition, no input from the request.
fn order_by_expected_cost(candidates: &mut Vec<ProviderCandidate>, estimates: &CostContext<'_>) {
    let (free, mut priced): (Vec<_>, Vec<_>) = candidates
        .drain(..)
        .partition(|candidate| (estimates.is_free)(candidate.definition()));
    order_priced_by_expected_cost(&mut priced, estimates);
    candidates.extend(free);
    candidates.extend(priced);
}

/// Ascending expected cost basis over the rungs that cost something — the
/// stage-3b ordering, unchanged, now that free rungs are handled ahead of it.
fn order_priced_by_expected_cost(
    candidates: &mut Vec<ProviderCandidate>,
    estimates: &CostContext<'_>,
) {
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
            // Base rates, deliberately. `input_bytes` is the byte-length
            // prompt bound, not a token count, so it cannot say which side of
            // a token threshold this request will land on — selecting a
            // conditional band from it would move the boundary by roughly the
            // bytes-per-token ratio. This figure orders rungs against each
            // other and is never charged to anyone, so an ordering that
            // ignores repricing is a worse ordering rather than a wrong
            // price. Every shipped tier has one candidate today, which makes
            // this a no-op in practice; it becomes worth revisiting the day a
            // tier holds two rungs that reprice differently.
            Some(expected_cost_basis(
                definition.rates.base(),
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
    // Taken before the walk so the marker can be fired from inside the same
    // `select!` whose other arm moves the session into a settle terminal.
    let dispatch_marker = usage_session.dispatch_marker();
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
    // Whether any candidate on this walk refused to attest the retention
    // guarantee its lane is sold under. Chooses the terminal's customer-facing
    // error and nothing else — the ledger, the settle path, and the status are
    // identical either way.
    let mut retention_attestation_failed = false;
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
                    // Inside the branch that actually begins the call, not
                    // before the select: a walk cancelled while still choosing
                    // has dispatched nothing. Fire-and-forget, so the marker
                    // costs the request nothing and cannot fail it.
                    dispatch_marker.fire();
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
                        // The attempt that WON, so the price and the
                        // disclosure describe the credential that actually
                        // served rather than the one the walk set out with.
                        features.on_candidate(candidate),
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
            // Latched, never cleared. A walk that met a retention failure on
            // ANY candidate reports that at its terminal even if a later
            // candidate failed for some ordinary reason afterwards: the
            // stronger, more specific fact is the one the customer needs, and
            // the alternative — last-failure-wins — would let a transient 500
            // on a second rung bury the one failure that says a data guarantee
            // was not honoured.
            if matches!(class, retry::FailureClass::RetentionAttestation) {
                retention_attestation_failed = true;
            }
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
        if retention_attestation_failed {
            WalkTerminal::RetentionAttestation
        } else {
            WalkTerminal::Exhausted
        },
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
    /// Every candidate failed and at least one of them failed by declining to
    /// attest the retention guarantee its lane is sold under.
    ///
    /// A REFINEMENT of [`Self::Exhausted`] rather than a state beside it: the
    /// walk really did exhaust, the ledger records the same 502, and the
    /// reservation settles at zero by the same path. All that differs is what
    /// the customer is told, and that difference is worth a variant because
    /// "every upstream failed" and "we would not serve you under a weaker data
    /// guarantee than you bought" are not the same message, and only one of
    /// them tells the customer their prompt was withheld on purpose.
    RetentionAttestation,
    /// The request's shared upstream deadline elapsed.
    Timeout,
    /// The router is draining.
    Shutdown,
}

impl WalkTerminal {
    fn status(self) -> i16 {
        match self {
            // Same 502 as `Exhausted`, deliberately: the ledger records what
            // the customer was sent, and both terminals send a 502.
            Self::Exhausted | Self::RetentionAttestation => 502,
            Self::Timeout => 504,
            Self::Shutdown => 503,
        }
    }

    fn api_error(self) -> ApiError {
        match self {
            Self::Exhausted => ApiError::UpstreamUnavailable,
            Self::RetentionAttestation => ApiError::RetentionAttestationFailed,
            Self::Timeout => ApiError::UpstreamTimeout,
            Self::Shutdown => ApiError::ServerShuttingDown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Exhausted => "all upstream candidates failed",
            Self::RetentionAttestation => "upstream did not attest zero data retention",
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
        &resolved.sell_rates,
        features,
        None,
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
            &resolved.sell_rates,
            features,
            None,
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
    // The upstream's own reason when it gave one, the synthesis when it did
    // not — and the provenance either way. See `AttemptFinishReason` for the
    // divergence table.
    let finish = AttemptFinishReason::resolve(
        response.stop_reason,
        emitted.has_tool_calls(),
        usage,
        max_tokens,
    );
    let shape_label = shape_ok(
        emitted,
        tool_args_all_json(&response.tool_calls),
        finish.reason,
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
        Some(finish.reason),
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
        &resolved.sell_rates,
        features,
        Some(finish),
        Some(shape_label),
        None,
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
    // Taken before the session is wrapped in the Option that every terminal
    // `take()`s, so the marker outlives whichever terminal claims the session.
    let dispatch_marker = usage_session.dispatch_marker();
    let mut usage_session = Some(usage_session);
    let mut client_connected = true;
    // The streaming twin of the buffered walk's flag, latched the same way and
    // for the same reason. It can only ever be set BEFORE any model output has
    // been sent: the attestation is asserted on the upstream's initial response
    // headers, so a candidate that fails it yields no chunk at all
    // (`crate::wire::ChatCompletionsWire::stream_chat`). That is what makes it
    // safe to convert into a customer-facing error here — there is no
    // half-delivered stream to reason about.
    let mut retention_attestation_failed = false;
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
                    &resolved.sell_rates,
                    features,
                    None,
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
        // From here down this iteration is about THIS candidate: it has passed
        // the health and deadline gates and is the one about to be dispatched
        // to, so every settle site below describes its attempt — including
        // whose credential the client is holding. Shadowed rather than
        // reassigned because `RequestFeatures` is `Copy`: the terminals ABOVE
        // this line name `last_candidate`, a previous rung, and must keep the
        // walk-level value.
        let features = features.on_candidate(candidate);
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
                    // The synthetic-stream path's dispatch. Same rule as the
                    // buffered walk: marked where the call begins, so a walk
                    // cancelled before it dispatched stays reclaimable.
                    dispatch_marker.fire();
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
                    let class = retry::classify(&err, false);
                    // A non-streaming upstream reached through the STREAMING
                    // handler (a candidate whose wire cannot stream, answered
                    // as one synthetic stream). The retention rule does not
                    // care which handler dispatched it.
                    if matches!(class, retry::FailureClass::RetentionAttestation) {
                        retention_attestation_failed = true;
                    }
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

        // The live-streaming dispatch. Marked before the stream is built
        // rather than after the first event arrives: the upstream request is
        // on its way from here, and an upstream that answers nothing at all is
        // exactly the case where the marker has to already be recorded.
        dispatch_marker.fire();
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
        // What the upstream said on its way out: its own stop reason, and why
        // it had no usage when it had none. Stays at the empty default unless
        // a terminal actually arrives, so a stream that broke mid-flight
        // reports no upstream claims rather than a stale candidate's.
        let mut stream_terminal = StreamFinal::empty();

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
                Ok(StreamEvent::Final(terminal)) => {
                    stream_terminal = terminal;
                    completed = true;
                    break;
                }
                Err(error) => {
                    // One `anyhow` wrap, read by both predicates. The
                    // attestation check runs before the stream body opens, so
                    // this arm is where a streamed retention failure surfaces —
                    // as the FIRST event of the stream, with nothing delivered
                    // ahead of it.
                    let error = anyhow::Error::new(error);
                    if retry::is_retention_attestation_failure(&error) {
                        retention_attestation_failed = true;
                    }
                    stream_rate_limited = retry::is_rate_limited(&error);
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
                stream_terminal,
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
                &resolved.sell_rates,
                features,
                None,
                None,
                None,
                attempts.take_rows(),
                started,
                if client_connected { 502 } else { 499 },
            )
            .await;
            let error = if metering.is_ok() {
                if retention_attestation_failed {
                    ApiError::RetentionAttestationFailed
                } else {
                    ApiError::UpstreamUnavailable
                }
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
            &resolved.sell_rates,
            features,
            None,
            None,
            None,
            attempts.take_rows(),
            started,
            502,
        )
        .await
        {
            Ok(()) if retention_attestation_failed => ApiError::RetentionAttestationFailed,
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
            &resolved.sell_rates,
            features,
            None,
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
            &resolved.sell_rates,
            features,
            None,
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
    // Same consumption rule as the buffered sibling: this path replays a
    // buffered response, so the reason it carries is the same one.
    let finish =
        AttemptFinishReason::resolve(response.stop_reason, has_tool_calls, usage, max_tokens);
    let shape_label = shape_ok(
        emitted,
        tool_args_all_json(&response.tool_calls),
        finish.reason,
    );
    attempts.push(build_attempt(
        attempt_no,
        candidate.definition(),
        "ok",
        true,
        attempt_started,
        AttemptTokens::measured(usage),
        false,
        Some(finish.reason),
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
        &resolved.sell_rates,
        features,
        Some(finish),
        Some(shape_label),
        // A synthetic stream is a buffered response replayed as SSE: there was
        // never a live stream to have a usage gap.
        None,
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
    // What the upstream said on its way out — its own stop reason, and why it
    // had no usage when it had none.
    terminal: StreamFinal,
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
            &resolved.sell_rates,
            features,
            None,
            None,
            // The gap this row exists to explain. This is the one settle site
            // reached by a stream that ran to its terminal and reported no
            // usage, so it is where `done_missing` has to land or it is lost —
            // on the free lane there is no attempt row to reconstruct it from.
            terminal.usage_gap,
            attempts.into_rows(),
            started,
            502,
        )
        .await;
        send_stream_error(sender, &ApiError::MeteringUnavailable).await;
        return;
    };
    // The terminal's own stop reason when the upstream sent one, the synthesis
    // when it did not — see `AttemptFinishReason` for the divergence table.
    let finish = AttemptFinishReason::resolve(
        terminal.stop_reason,
        emitted.has_tool_calls(),
        usage,
        max_tokens,
    );
    // From the deltas this stream actually emitted, never from
    // `usage.completion_tokens`: the upstream's token count is its accounting,
    // and a stream that emitted nothing while reporting output tokens must
    // label as the empty response it was.
    let shape_label = shape_ok(emitted, tool_args_ok, finish.reason);
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
        Some(finish.reason),
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
        &resolved.sell_rates,
        features,
        Some(finish),
        Some(shape_label),
        // `None` on this branch by construction: usage WAS reported, which is
        // precisely when `usage_gap` is None.
        terminal.usage_gap,
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

    // DELIBERATELY the synthesized reason, not `finish.reason`, and the one
    // place the two can disagree on a served row.
    //
    // What a customer's own agent loop reads here decides whether it issues a
    // continuation request — so switching this to the upstream's real reason
    // would change how many requests customers send us, and therefore what
    // they are charged. That is a product decision, not part of plumbing the
    // ledger, so it is left alone and the divergence is recorded rather than
    // taken. `usage_events.finish_reason_source` says which of the two any
    // given row's persisted reason was.
    let body_finish_reason = finish_reason(emitted.has_tool_calls(), usage, max_tokens);
    emit_stream_finish(
        sender,
        metadata,
        resolved,
        candidate,
        usage,
        body_finish_reason,
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

/// Price one candidate reservation: the token bounds the caps are checked
/// against and the sell-price ceiling the balance is checked against.
///
/// Fails closed. A tier whose sell rates cannot be priced cannot size a
/// reservation, and a request that cannot be metered must not be dispatched.
/// The catalog validated these rates at load, so this is a backstop rather
/// than a live path.
///
/// # Why the WORST-CASE rate, on a tier that reprices
///
/// A reservation is taken before the upstream has said anything, so the
/// request's real prompt-token count — the thing a conditional rate is
/// conditioned on — does not exist yet. The prompt figure available here is a
/// BYTE bound, which over-counts tokens by roughly the bytes-per-token ratio,
/// so it cannot decide which side of a token threshold the request will land
/// on either.
///
/// [`RateSchedule::worst_case`] sidesteps the question: it needs no
/// measurement, and it can never price below the table that ends up applying.
/// The alternative — reserving at the base rate — would under-reserve every
/// request that crosses the boundary, and an under-reserved request is a
/// customer spending past their balance. Over-reserving costs the customer
/// nothing: settlement charges the true band and releases the difference by
/// the same mechanism that already releases the gap between this byte bound
/// and the measured prompt. On a flat tier `worst_case()` IS the tier's rate
/// table, so nothing about a catalog without conditional rates changes.
///
/// # The BYOK fee arm, and why it is the WORST case too
///
/// A request dispatched on the customer's own provider key is charged 5% of
/// what the same usage would cost at catalog rates, so its reservation is 5% of
/// the figure above — and `byok_rate` is that multiplier, resolved by
/// [`byok_reservation_rate`] over the whole route rather than over one
/// candidate.
///
/// It has to be the whole route, and this is the part worth being explicit
/// about. The walk may fail over from a candidate the customer has a key for to
/// one they do not, which settles at full catalog price. Reserving 5% and then
/// settling 100% would exceed the reservation — and the settle debit is CLAMPED
/// to it (`crate::db`), so the excess is not charged at all. That is not a
/// customer overspending; it is ZeroRouter delivering inference it then cannot
/// bill for, which is the failure `AGENTS.md` names first. So a mixed route
/// reserves at the house rate, exactly as `worst_case()` above takes the higher
/// of two rate tables for the same reason, and for the same price: over-reserving
/// costs the customer nothing, because settlement releases the difference.
fn sized_reservation(
    request: &ChatCompletionRequest,
    resolved: &ResolvedRoute,
    output_bound: u32,
    byok_rate: Decimal,
) -> Result<ReservationSize, ApiError> {
    let usage = request.reservation_usage(output_bound);
    let catalog_cost =
        usage_cost(resolved.sell_rates.worst_case(), usage).ok_or(ApiError::MeteringUnavailable)?;
    Ok(ReservationSize {
        total_tokens: i64::try_from(usage.total_tokens).map_err(|_| ApiError::InvalidRequest)?,
        output_tokens: i64::from(output_bound),
        // An overflow here is a metering failure, never a wrapped charge — the
        // same rule `usage_cost` follows for the multiplication above it.
        cost_usd: byok::apply_fee(catalog_cost, byok_rate).ok_or(ApiError::MeteringUnavailable)?,
    })
}

/// The fee multiplier a reservation must be sized at, given which providers the
/// customer holds a key for.
///
/// The BYOK rate only when EVERY candidate on the route would dispatch on the
/// customer's own credential; the house rate the moment one would not. See
/// [`sized_reservation`] for why the conservative direction is the only safe
/// one, and note which set is quantified over: `resolved.candidates` is the
/// CATALOG list, before route assembly drops the ones whose credential is
/// absent. A candidate that gets dropped cannot serve, so counting it can only
/// raise the rate to the house one — over-reserving, which is free — never
/// lower it.
/// What admission needs to know to price this request against the customer's
/// monthly BYOK allowance (migration 0027): the catalog basis it may consume,
/// and whether it could only ever settle at the fee rate.
///
/// # Why the basis is priced at the HOUSE rate
///
/// It is a CATALOG figure by definition — the allowance is denominated in what
/// the usage would have cost at list price, which is what
/// [`byok::house_rate`] leaves untouched. Pricing it at `byok_rate` would
/// measure the allowance in fees and make $5,000 of allowance mean $100,000 of
/// traffic.
///
/// # Why the FULL bound and not the learned one
///
/// Admission may end up reserving from either sizing, and this figure has to
/// bound whichever it picks. The full requested ceiling is the larger of the
/// two, so measuring it here can only over-state what this request will
/// consume — which delays a zero reservation and never permits one that should
/// not have happened. It is also the only one of the two that always exists.
///
/// # Why `catalog_basis` is offered on a MIXED route
///
/// A mixed route reserves at the house rate and is not eligible to reserve
/// nothing, but it can still SERVE from its BYOK rung, and if it does it
/// settles as BYOK and consumes allowance. The commitment it records is what
/// keeps a later request from being told that allowance is still free.
fn byok_reservation_posture(
    request: &ChatCompletionRequest,
    resolved: &ResolvedRoute,
    output_bound: u32,
    covered: &BTreeSet<String>,
    byok_rate: Decimal,
) -> Result<ByokReservation, ApiError> {
    // Nothing on this route can dispatch on a customer credential, so it
    // consumes no allowance and commits none. This is the arm every request on
    // a deployment without BYOK takes, and it reaches no further code.
    if !resolved
        .candidates
        .iter()
        .any(|candidate| covered.contains(&candidate.provider))
    {
        return Ok(ByokReservation::default());
    }
    Ok(ByokReservation {
        catalog_basis: Some(
            sized_reservation(request, resolved, output_bound, byok::house_rate())?.cost_usd,
        ),
        // Read from the rate the reservation was sized at rather than
        // re-deciding it, so "every rung is covered" has ONE definition. Two
        // copies of that quantifier could disagree, and the disagreement would
        // show up as a request reserving nothing on a route that can settle at
        // catalog.
        wholly_byok: byok_rate == byok::fee_rate(),
    })
}

fn byok_reservation_rate(resolved: &ResolvedRoute, covered: &BTreeSet<String>) -> Decimal {
    if !covered.is_empty()
        && resolved
            .candidates
            .iter()
            .all(|candidate| covered.contains(&candidate.provider))
    {
        byok::fee_rate()
    } else {
        byok::house_rate()
    }
}

async fn admit_usage(
    pool: &PgPool,
    key: &AuthenticatedKey,
    sizing: ReservationSizing,
    byok: ByokReservation,
    task_signature: TaskSignature,
    require_credits: bool,
    lane: MeteringLane,
) -> Result<UsageSession, ApiError> {
    match begin_usage_session(
        pool,
        key,
        sizing,
        byok,
        task_signature,
        require_credits,
        lane,
    )
    .await
    .map_err(|_| ApiError::DatabaseUnavailable)?
    {
        UsageAdmission::Allowed(session) => Ok(session),
        UsageAdmission::Unauthorized => Err(ApiError::Unauthorized),
        UsageAdmission::SpendExceeded => Err(ApiError::SpendCapExceeded),
        UsageAdmission::KeyCreditLimitExceeded => Err(ApiError::KeyCreditLimitExceeded),
        UsageAdmission::VelocityExceeded => Err(ApiError::VelocityCapExceeded),
        UsageAdmission::InsufficientCredits => Err(ApiError::InsufficientCredits),
        UsageAdmission::AccountFrozen => Err(ApiError::AccountFrozen),
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
    sell_rates: &RateSchedule,
    features: RequestFeatures,
    finish: Option<AttemptFinishReason>,
    shape_label: Option<bool>,
    usage_gap: Option<UsageGap>,
    attempts: Vec<AttemptRecord>,
    started: Instant,
    status: i16,
) -> Result<(), ApiError> {
    let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
    // The band this request actually landed in, chosen from the MEASURED
    // prompt the upstream reported — the same figure the vendor bills us
    // against, so the customer's basis and ZeroRouter's stay the same shape.
    // `usage` here is `StreamDelivery::settled_usage`: metered actuals, never
    // an estimate, so a request that reported nothing settles at the base
    // band and bills nothing, exactly as before. On a flat tier this is the
    // tier's one rate table and every figure below is unchanged.
    let sell_rates = sell_rates.at_prompt_tokens(usage.prompt_tokens);
    let basis_rates =
        candidate.map(|candidate| candidate.rates.at_prompt_tokens(usage.prompt_tokens));
    // What the customer owes, computed before anything is written. Rates that
    // cannot be priced are a metering failure and settle nothing: the request
    // ends in `MeteringUnavailable` and the reservation is released by the TTL
    // sweep unspent, rather than a `Decimal::ZERO` charge being recorded as
    // though the request had genuinely been free.
    let catalog_cost_usd = usage_cost(sell_rates, usage).ok_or(ApiError::MeteringUnavailable)?;
    // The BYOK fee arm. `byok_served` is read from the ROUTE — which provider
    // the winning attempt's client actually holds a credential for — not from
    // "does this user have a key attached", so a walk that failed over to a
    // house candidate settles at catalog price even for a customer who brought
    // a key, and a customer whose stored credential could not be opened is
    // billed for what really happened rather than for what they intended.
    //
    // Metered actuals throughout: this is 5% of the true measured cost at the
    // band that applied, not 5% of the reservation. Same rule as the house
    // lane, one multiplication apart.
    //
    // This is the PRE-ALLOWANCE figure. The monthly allowance (migration 0027)
    // is applied by `crate::db::settle_once`, under the per-user advisory lock,
    // because it is a read-then-write against a running total and this seam
    // holds no lock — two of a customer's requests settling at once would each
    // see the same allowance remaining and each spend it. What travels from
    // here is the ceiling and the basis; what decides the charge is the settle.
    let cost_usd = byok::apply_fee(catalog_cost_usd, features.byok_rate())
        .ok_or(ApiError::MeteringUnavailable)?;
    // The basis the allowance is consumed in, on the rows that consume it. Tied
    // to the same `byok_served` flag that chose the rate above, so a row can
    // never claim to have been billed as BYOK while consuming no allowance, or
    // the reverse.
    let byok_catalog_usd = features.byok_served.then_some(catalog_cost_usd);
    let telemetry = RequestTelemetry {
        requested_max_tokens: features.requested_max_tokens,
        stream: features.stream,
        prompt_bytes: features.prompt_bytes,
        message_count: features.message_count,
        tool_count: features.tool_count,
        candidate_id: candidate.map(|candidate| candidate.id.clone()),
        // The rate tables this request was PRICED at, not the schedules they
        // came from. The ledger's job is to record what was charged, and a
        // reader asking "what did this row bill at" must get the band that
        // applied rather than a base rate the request may never have touched.
        basis_rates,
        sell_rates,
        finish_reason: finish.map(|finish| finish.reason.to_owned()),
        // Provenance rides with the value it describes, so a row can never
        // claim a reason came from one place while carrying the other's.
        finish_reason_source: finish.map(|finish| finish.source),
        shape_ok: shape_label,
        usage_gap: usage_gap.map(UsageGap::as_str),
        priority: Some(features.priority),
        // `Some`, never `None`, on every row this build settles: the live path
        // always knows which credential served. NULL is reserved for rows that
        // predate the column entirely (migration 0026), so "we did not record
        // it" and "it was not BYOK" stay distinguishable forever.
        byok: Some(features.byok_served),
    };
    usage_session
        .record(&UsageRecord {
            tier: requested_model.to_owned(),
            upstream_provider: upstream_provider.to_owned(),
            upstream_model: upstream_model.to_owned(),
            usage,
            cost_usd,
            byok_catalog_usd,
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

    use crate::config::ModelMetadata;
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

    // -----------------------------------------------------------------------
    // Edge mode, stage 2: selection over a $0 local rung
    // (`docs/design/edge-mode-local-rung.md`). These drive `order_candidates`
    // directly, which is the whole of the policy — the walk that follows is
    // unchanged and pinned elsewhere.
    // -----------------------------------------------------------------------

    /// A priced cloud rung: 1.00/2.00, with the wide window and tool support
    /// every shipped candidate declares.
    fn cloud_rung(id: &str) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: "openai".to_owned(),
            model: format!("upstream/{id}"),
            surface: None,
            rates: RateSchedule::flat(ModelRates {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: Some(0.2),
            }),
            metadata: ModelMetadata {
                context_window: Some(1_000_000),
                max_output_tokens: Some(128_000),
                input_modalities: Some(vec!["text".to_owned(), "image".to_owned()]),
                tool_call: Some(true),
            },
        }
    }

    /// A $0 local rung carrying `metadata` — the operator's declaration of
    /// what their own server can take.
    fn local_rung(id: &str, metadata: ModelMetadata) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: "local-llama".to_owned(),
            model: format!("local/{id}"),
            surface: None,
            rates: RateSchedule::flat(ModelRates {
                input_per_mtok: Some(0.0),
                output_per_mtok: Some(0.0),
                cached_input_per_mtok: None,
            }),
            metadata,
        }
    }

    /// What a local model typically declares: a small window, tools, text.
    fn local_metadata() -> ModelMetadata {
        ModelMetadata {
            context_window: Some(32_000),
            max_output_tokens: Some(4_096),
            input_modalities: Some(vec!["text".to_owned()]),
            tool_call: Some(true),
        }
    }

    /// The candidate ids of a route after ordering, given the request's
    /// mechanical needs and a health registry.
    fn ordered(
        priority: Priority,
        definitions: Vec<TierCandidate>,
        needs: &RequestNeeds,
        health: &ProviderHealth,
    ) -> Vec<String> {
        let estimator = EstimatorState::default();
        let signature = TaskSignature {
            hex: "00112233aabbccdd".to_owned(),
            scheme: 2,
            tool_names_sha256: String::new(),
        };
        let mut candidates: Vec<ProviderCandidate> = definitions
            .into_iter()
            // Never dispatched: these tests order a route and read the order.
            .map(|definition| {
                ProviderCandidate::against_local_upstream(definition, "http://127.0.0.1:1/unused")
            })
            .collect();
        // Stands in for `TierCandidate::is_free`, which needs an installed
        // operator inventory to be true of anything — a once-per-process act
        // this binary must not perform (other tests pin the shipped inventory
        // exactly). The predicate itself is tested in `config`; that the real
        // one drives real ordering is tested end to end in
        // `tests/local_candidates.rs`. What these tests own is the MECHANISM:
        // where the partition sits relative to the estimator, health, and
        // eligibility.
        let is_free = |candidate: &TierCandidate| candidate.rates_are_zero();
        order_candidates(
            priority,
            &mut candidates,
            &CostContext {
                estimator: &estimator,
                signature: &signature,
                // Cold: the state a router that has never served has, which is
                // exactly when a local rung most needs to be chosen.
                signature_cell: CellRead::Cold,
                input_bytes: needs.prompt_bound,
                is_free: &is_free,
            },
            health,
            needs,
        );
        candidates
            .iter()
            .map(|candidate| candidate.definition().id.clone())
            .collect()
    }

    // -----------------------------------------------------------------------
    // The metering seam (edge mode, stage 3). These own the PREDICATE; that
    // the real `TierCandidate::is_free` and a real database sit behind it is
    // owned end to end by `tests/local_candidates.rs`.
    //
    // Unit tests because this is where the two conjuncts are independently
    // observable. Through a loaded catalog they are very nearly the same
    // question — a tier that sells at $0 forces every candidate's basis to $0
    // (`validate_candidate_margin` refuses a basis above the sell rate, and
    // input/output rates are mandatory), and a $0 basis in turn demands a
    // free-settling provider. That near-equivalence is a property of today's
    // validation rules, not of this predicate, and it is exactly why both
    // conjuncts are written out: either rule could be relaxed by a future
    // change that has nothing to do with edge mode, and neither half may
    // quietly start resting on the other.
    // -----------------------------------------------------------------------

    /// A route of `definitions` under a tier selling at `sell_rates`, answered
    /// by the predicate the request path asks.
    fn skips_metering(
        definitions: Vec<TierCandidate>,
        sell_rates: impl Into<RateSchedule>,
    ) -> bool {
        let sell_rates = sell_rates.into();
        let candidates: Vec<ProviderCandidate> = definitions
            .iter()
            .cloned()
            .map(|definition| {
                ProviderCandidate::against_local_upstream(definition, "http://127.0.0.1:1/unused")
            })
            .collect();
        let resolved = ResolvedRoute {
            requested_model: "zero/seam".to_owned(),
            candidates: definitions,
            sell_rates,
        };
        // Same stand-in as `ordered`, and for the same reason: the real
        // `is_free` needs an installed operator inventory, which this binary
        // must not install.
        let is_free = |candidate: &TierCandidate| candidate.rates_are_zero();
        free_lane_admissible(&resolved, &candidates, &is_free)
    }

    fn free_sell_rates() -> ModelRates {
        ModelRates {
            input_per_mtok: Some(0.0),
            output_per_mtok: Some(0.0),
            cached_input_per_mtok: Some(0.0),
        }
    }

    fn priced_sell_rates() -> ModelRates {
        ModelRates {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(6.0),
            cached_input_per_mtok: Some(0.6),
        }
    }

    #[test]
    fn an_all_free_route_under_a_free_tier_skips_metering() {
        assert!(skips_metering(
            vec![
                local_rung("local/qwen", local_metadata()),
                local_rung("local/gemma", local_metadata()),
            ],
            free_sell_rates(),
        ));
    }

    #[test]
    fn one_metered_candidate_anywhere_in_the_route_keeps_full_metering() {
        // The rule the design turns on, and the reason it is `all` and not
        // `any`: the reservation is taken at ADMISSION, before the walk knows
        // which rung will answer. A route holding a paid rung may reach it —
        // that is what a fallback IS — and a paid dispatch with no reservation
        // behind it is inference delivered with nothing to charge it against.
        // Position is irrelevant, so both orders are stated.
        for route in [
            vec![
                local_rung("local/qwen", local_metadata()),
                cloud_rung("openai/burst"),
            ],
            vec![
                cloud_rung("openai/burst"),
                local_rung("local/qwen", local_metadata()),
            ],
        ] {
            assert!(
                !skips_metering(route, free_sell_rates()),
                "a route that can still reach a metered rung must reserve for it"
            );
        }
    }

    #[test]
    fn an_all_free_route_under_a_priced_tier_keeps_full_metering() {
        // Candidate freeness is a claim about ZeroRouter's COST; the customer
        // pays the TIER's rate. `validate_candidate_margin` permits a $0 basis
        // under a priced tier — that is the intended 100%-margin shape — so
        // reading candidate freeness as customer freeness would hand a $3/Mtok
        // tier away for nothing. This is also the shape a MISSING CREDENTIAL
        // produces: `ProviderRoute` drops a candidate whose credential is
        // absent, so a mixed tier collapses to exactly this route the moment an
        // operator's cloud key is unset.
        assert!(!skips_metering(
            vec![local_rung("local/qwen", local_metadata())],
            priced_sell_rates(),
        ));
    }

    #[test]
    fn a_tier_priced_on_one_dimension_alone_keeps_full_metering() {
        // Zero is not "mostly zero". Each dimension is checked, because a tier
        // that gives away its input and charges for output is still selling
        // something.
        for sell_rates in [
            ModelRates {
                input_per_mtok: Some(0.0),
                output_per_mtok: Some(6.0),
                cached_input_per_mtok: Some(0.0),
            },
            ModelRates {
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(0.0),
                cached_input_per_mtok: Some(0.0),
            },
            ModelRates {
                input_per_mtok: Some(0.0),
                output_per_mtok: Some(0.0),
                cached_input_per_mtok: Some(0.6),
            },
        ] {
            assert!(!skips_metering(
                vec![local_rung("local/qwen", local_metadata())],
                sell_rates,
            ));
        }
    }

    #[test]
    fn a_tier_that_omits_its_cached_rate_can_still_be_free() {
        // The convention the tier file already uses and `usage_cost` already
        // honours: an absent cached rate is priced at the input rate, so an
        // absent cached rate over a $0 input rate prices at zero. Requiring
        // operators to write `= 0` for a dimension their local server does not
        // have would make the commonest honest config silently not free.
        assert!(skips_metering(
            vec![local_rung("local/qwen", local_metadata())],
            ModelRates {
                input_per_mtok: Some(0.0),
                output_per_mtok: Some(0.0),
                cached_input_per_mtok: None,
            },
        ));
    }

    #[test]
    fn an_empty_route_never_skips_metering() {
        // `all()` is vacuously true over nothing. "No candidates" must never be
        // the way metering gets turned off, so the emptiness guard is stated
        // here rather than inherited from `ProviderRoute::new`'s refusal.
        assert!(!skips_metering(Vec::new(), free_sell_rates()));
    }

    /// A request that fits everything: small prompt, no tools, plain text.
    fn modest_needs() -> RequestNeeds {
        RequestNeeds {
            prompt_bound: 1_000,
            tools: false,
            modalities: ["text".to_owned()].into_iter().collect(),
        }
    }

    #[test]
    fn a_zero_cost_rung_leads_cost_mode_on_a_cold_segment() {
        // The $0 rung's whole point. The estimator is cold — no router that
        // has just been stood up has anything else — and cost mode's
        // expected-cost sort falls through to the table order in that state,
        // so a free rung that waited for an estimate would sit second behind
        // the cloud rung the table happens to list first. Free needs no
        // estimate: it is cheapest at every prompt length.
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![
                    cloud_rung("openai/cloud"),
                    local_rung("local/qwen", local_metadata())
                ],
                &modest_needs(),
                &ProviderHealth::default(),
            ),
            ["local/qwen", "openai/cloud"]
        );
    }

    #[test]
    fn a_route_with_no_free_rung_orders_exactly_as_it_did_before() {
        // The no-change contract for every deployment that has no local rung:
        // with nothing free to partition out, cost mode is the stage-3b
        // ordering unchanged — table order on a cold segment — and balanced
        // is the identity it has always been.
        for priority in [Priority::Cost, Priority::Balanced, Priority::Success] {
            assert_eq!(
                ordered(
                    priority,
                    vec![cloud_rung("openai/first"), cloud_rung("openai/second")],
                    &modest_needs(),
                    &ProviderHealth::default(),
                ),
                ["openai/first", "openai/second"],
                "{priority:?} must not reorder a route with no $0 rung"
            );
        }
    }

    #[test]
    fn balanced_keeps_the_operators_table_order_even_with_a_free_rung() {
        // $0-first is a COST-mode rule. Balanced is the frozen control group —
        // the table order is the operator's own statement of preference, and
        // an operator who wants their local box tried first in balanced mode
        // says so by writing it first. Silently overriding that would make the
        // one mode that promises "the file decides" stop meaning it.
        assert_eq!(
            ordered(
                Priority::Balanced,
                vec![
                    cloud_rung("openai/cloud"),
                    local_rung("local/qwen", local_metadata())
                ],
                &modest_needs(),
                &ProviderHealth::default(),
            ),
            ["openai/cloud", "local/qwen"]
        );
    }

    #[test]
    fn several_free_rungs_keep_their_table_order() {
        // They price identically, so there is nothing to sort on and the
        // operator's order is the only preference available.
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![
                    cloud_rung("openai/cloud"),
                    local_rung("local/first", local_metadata()),
                    local_rung("local/second", local_metadata()),
                ],
                &modest_needs(),
                &ProviderHealth::default(),
            ),
            ["local/first", "local/second", "openai/cloud"]
        );
    }

    #[test]
    fn a_prompt_past_the_local_windows_bursts_to_cloud() {
        // Mechanical eligibility, the context half. The local rung declares a
        // 32k window and the request's prompt bound is over it, so the free
        // rung sinks behind the cloud rung despite costing nothing — and the
        // existing failover machinery does the rest. Note the bound is a BYTE
        // bound, which over-counts tokens: the burst is deliberately early
        // rather than deliberately truncating someone's prompt.
        let needs = RequestNeeds {
            prompt_bound: 40_000,
            ..modest_needs()
        };
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![
                    cloud_rung("openai/cloud"),
                    local_rung("local/qwen", local_metadata())
                ],
                &needs,
                &ProviderHealth::default(),
            ),
            ["openai/cloud", "local/qwen"]
        );
    }

    #[test]
    fn a_tool_call_the_local_rung_lacks_bursts_to_cloud() {
        // The tools half, and the unknown-is-not-a-refusal rule beside it. A
        // rung that DECLARED `tool_call = false` cannot take a request that
        // brings tools. A rung that declared nothing is not excluded: absence
        // means unknown, which is the state every candidate was in before the
        // metadata table existed, and inventing a refusal from silence would
        // route around models nobody has described yet.
        let needs = RequestNeeds {
            tools: true,
            ..modest_needs()
        };
        let toolless = ModelMetadata {
            tool_call: Some(false),
            ..local_metadata()
        };
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![
                    cloud_rung("openai/cloud"),
                    local_rung("local/qwen", toolless)
                ],
                &needs,
                &ProviderHealth::default(),
            ),
            ["openai/cloud", "local/qwen"]
        );
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![
                    cloud_rung("openai/cloud"),
                    local_rung("local/qwen", ModelMetadata::default()),
                ],
                &needs,
                &ProviderHealth::default(),
            ),
            ["local/qwen", "openai/cloud"],
            "an undeclared capability is unknown, not a refusal"
        );
    }

    #[test]
    fn a_down_local_rung_bursts_to_cloud() {
        // "Local is down" is the health registry's job, unchanged: a 429 or a
        // run of upstream errors demotes the rung and demotion sinks it behind
        // the cloud rung. $0-first orders it ahead first; health moves it back;
        // the walk then dispatches to the cloud rung. No new machinery.
        let health = ProviderHealth::default();
        let local = local_rung("local/qwen", local_metadata());
        health.observe(&build_attempt(
            1,
            &local,
            "rate_limited",
            false,
            Instant::now(),
            AttemptTokens::unknown(),
            false,
            None,
            None,
        ));
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![cloud_rung("openai/cloud"), local.clone()],
                &modest_needs(),
                &health,
            ),
            ["openai/cloud", "local/qwen"]
        );
    }

    #[test]
    fn an_ineligible_rung_sinks_behind_a_merely_demoted_one() {
        // The two sinks are ordered, and the order is not arbitrary. A demoted
        // rung has been failing and might still serve; an ineligible rung is
        // one the operator has told us cannot take this request at all. So
        // eligibility is the outer partition: prefer might-work over
        // stated-cannot.
        let health = ProviderHealth::default();
        let demoted = cloud_rung("openai/cloud");
        health.observe(&build_attempt(
            1,
            &demoted,
            "rate_limited",
            false,
            Instant::now(),
            AttemptTokens::unknown(),
            false,
            None,
            None,
        ));
        let needs = RequestNeeds {
            prompt_bound: 40_000,
            ..modest_needs()
        };
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![local_rung("local/qwen", local_metadata()), demoted.clone()],
                &needs,
                &health,
            ),
            ["openai/cloud", "local/qwen"]
        );
    }

    #[test]
    fn balanced_never_reorders_a_paid_tier_on_declared_metadata() {
        // The frozen control group, protected from a rule that has nothing to
        // do with it. Eligibility exists for the $0 rung, which is specified in
        // cost mode; applying it in balanced would silently reorder ordinary
        // multi-candidate PAID tiers — tiers with no local rung and no
        // involvement in edge mode — the first time a request outgrew the
        // narrower rung's declared window. Balanced means the file decides.
        let narrow = TierCandidate {
            metadata: ModelMetadata {
                context_window: Some(8_000),
                ..cloud_rung("openai/narrow").metadata
            },
            ..cloud_rung("openai/narrow")
        };
        let needs = RequestNeeds {
            prompt_bound: 40_000,
            ..modest_needs()
        };
        for priority in [Priority::Balanced, Priority::Success] {
            assert_eq!(
                ordered(
                    priority,
                    vec![narrow.clone(), cloud_rung("openai/wide")],
                    &needs,
                    &ProviderHealth::default(),
                ),
                ["openai/narrow", "openai/wide"],
                "{priority:?} must keep the table order it had before stage 2"
            );
        }

        // Cost mode is where the rule lives, and there it does sink the rung
        // that cannot hold the prompt.
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![narrow, cloud_rung("openai/wide")],
                &needs,
                &ProviderHealth::default(),
            ),
            ["openai/wide", "openai/narrow"]
        );
    }

    #[test]
    fn a_route_whose_every_rung_is_ineligible_keeps_its_order() {
        // Sinking, never removing. An all-ineligible route partitions to
        // itself, so a request whose prompt overflows every declared window
        // still dispatches and still gets whatever answer the upstream gives —
        // today's behavior for a single-candidate tier, unchanged. Nothing
        // here can empty a route.
        let needs = RequestNeeds {
            prompt_bound: 40_000,
            ..modest_needs()
        };
        assert_eq!(
            ordered(
                Priority::Cost,
                vec![
                    local_rung("local/first", local_metadata()),
                    local_rung("local/second", local_metadata()),
                ],
                &needs,
                &ProviderHealth::default(),
            ),
            ["local/first", "local/second"]
        );
    }

    fn walk_candidate(id: &str) -> TierCandidate {
        TierCandidate {
            id: id.to_owned(),
            provider: "test-upstream".to_owned(),
            model: format!("upstream/{id}"),
            surface: None,
            rates: RateSchedule::flat(ModelRates {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: None,
            }),
            // The walk does not read metadata; it only routes and bills.
            metadata: ModelMetadata::default(),
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
        walk_route_selling(
            candidates,
            RateSchedule::flat(ModelRates {
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(6.0),
                cached_input_per_mtok: None,
            }),
        )
    }

    // -----------------------------------------------------------------------
    // Conditional rates on the money path: what admission reserves, and what
    // settlement charges. The band selection itself is pinned in
    // `provider::tests`; these pin that the two sites ask the RIGHT question —
    // admission the worst case, settlement the measured prompt.
    // -----------------------------------------------------------------------

    fn banded(input: f64, cached: f64, output: f64) -> ModelRates {
        ModelRates {
            input_per_mtok: Some(input),
            cached_input_per_mtok: Some(cached),
            output_per_mtok: Some(output),
        }
    }

    /// `openai/gpt-5.6-luna`'s shipped schedule, sell side.
    fn luna_schedule() -> RateSchedule {
        RateSchedule::new(
            banded(0.2, 0.02, 1.2),
            vec![crate::provider::ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: banded(0.4, 0.04, 1.8),
            }],
        )
    }

    /// A pass-through pin: its cost basis IS the schedule it is sold on.
    ///
    /// Taking the schedule as an argument rather than hardcoding one keeps the
    /// fixture inside what the loader will actually accept — a candidate whose
    /// thresholds differ from its tier's is refused outright
    /// (`validate_conditional_alignment`), so a test built that way would be
    /// pinning behaviour on a file that can never load.
    fn pass_through_candidate(schedule: RateSchedule) -> TierCandidate {
        TierCandidate {
            id: "openai/luna".to_owned(),
            provider: "test-upstream".to_owned(),
            model: "upstream/luna".to_owned(),
            surface: None,
            rates: schedule,
            metadata: ModelMetadata::default(),
        }
    }

    fn measured(prompt_tokens: u64, completion_tokens: u64) -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            prompt_tokens_details: None,
        }
    }

    /// A request whose prompt is genuinely long — half a megabyte of text,
    /// which admission bounds at ~500,000 "tokens" because that bound is
    /// measured in BYTES.
    ///
    /// The size is what makes the settle tests below mean anything. The real
    /// prompt behind such a request is a few hundred thousand tokens, so a
    /// 300,000-token usage report is a plausible outcome for it and the
    /// reservation's byte bound still over-counts, exactly as it does in
    /// production. Sized to stay under the fixture key's 1,000,000 token/min
    /// velocity cap.
    fn long_walk_request() -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": "zero/test",
            "messages": [{ "role": "user", "content": "x".repeat(500_000) }],
            "stream": true,
            "max_tokens": 64,
        }))
        .expect("long walk request should deserialize")
    }

    #[test]
    fn an_output_floor_on_a_repricing_candidate_records_no_cogs_rather_than_a_low_one() {
        // An abandoned stream leaves the prompt dimension unknown — the only
        // figure the router has is a per-chunk output floor — and `priceable`
        // reconstructs the missing prompt as 0. On a FLAT candidate that is a
        // sound floor: right rate, understated tokens.
        //
        // On a candidate that reprices it is not. Band selection reads the
        // prompt, so a reconstructed 0 always picks the BASE band, and a
        // request whose real prompt was 300,000 tokens gets its output priced
        // at 1.20 when ZeroRouter is paying 1.80. That is a number derived from
        // a band there is no evidence applies, on exactly the long-context
        // traffic conditional rates exist for — so the honest entry is NULL,
        // the ledger's word for "not captured", which is already what the
        // completeness flag says about this row.
        let floor = AttemptTokens::output_floor(10_000);
        let repricing = build_attempt(
            1,
            &pass_through_candidate(luna_schedule()),
            "aborted",
            false,
            Instant::now(),
            floor,
            true,
            None,
            None,
        );
        assert_eq!(
            repricing.cost_basis_usd, None,
            "an unknown prompt cannot choose a band, so this attempt has no priceable COGS"
        );

        // The flat case is untouched: a floor at the only rate there is.
        let flat_rates = banded(0.2, 0.02, 1.2);
        let flat = build_attempt(
            1,
            &pass_through_candidate(RateSchedule::flat(flat_rates)),
            "aborted",
            false,
            Instant::now(),
            floor,
            true,
            None,
            None,
        );
        assert_eq!(
            flat.cost_basis_usd,
            usage_cost(
                flat_rates,
                floor.priceable().expect("an output floor prices")
            ),
            "a flat candidate still records its floor exactly as before"
        );

        // And a MEASURED attempt on a repricing candidate still prices — the
        // prompt is known, so the band is known.
        let measured_tokens = AttemptTokens::measured(measured(300_000, 10_000));
        let measured_attempt = build_attempt(
            1,
            &pass_through_candidate(luna_schedule()),
            "served",
            true,
            Instant::now(),
            measured_tokens,
            false,
            None,
            None,
        );
        assert_eq!(
            measured_attempt.cost_basis_usd,
            usage_cost(banded(0.4, 0.04, 1.8), measured(300_000, 10_000)),
            "a measured prompt selects the high band and prices there"
        );
    }

    #[test]
    fn a_reservation_is_sized_at_the_dearest_band_never_the_base_one() {
        // INVARIANT: never under-reserve. Admission has no measured prompt —
        // only a byte bound — so it cannot know which band the request will
        // land in, and reserving at the base rate would leave every request
        // that crosses the boundary spending past what was held for it.
        let request = walk_request();
        let resolved = walk_route_selling(
            vec![pass_through_candidate(luna_schedule())],
            luna_schedule(),
        );
        let sized = sized_reservation(&request, &resolved, 64, byok::house_rate())
            .expect("luna's rates price");

        let usage = request.reservation_usage(64);
        let at_base = usage_cost(banded(0.2, 0.02, 1.2), usage).expect("base band prices");
        let at_high = usage_cost(banded(0.4, 0.04, 1.8), usage).expect("high band prices");
        assert_eq!(
            sized.cost_usd, at_high,
            "the reservation must hold the dearest rate the schedule can charge"
        );
        assert!(
            sized.cost_usd > at_base,
            "a reservation at the base band would under-reserve every long request"
        );
    }

    #[test]
    fn a_flat_tier_reserves_exactly_what_it_always_did() {
        // The other half of the same invariant: a catalog with no conditional
        // rates must reserve bit-for-bit as before, so `worst_case` may not
        // inflate anything on the tiers that ship today.
        let request = walk_request();
        let flat = ModelRates {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(6.0),
            cached_input_per_mtok: None,
        };
        let resolved = walk_route_selling(vec![walk_candidate("flat")], RateSchedule::flat(flat));
        let sized = sized_reservation(&request, &resolved, 64, byok::house_rate())
            .expect("flat rates price");
        assert_eq!(
            sized.cost_usd,
            usage_cost(flat, request.reservation_usage(64)).expect("flat rates price"),
        );
    }

    #[test]
    fn a_tier_that_reprices_past_a_threshold_never_takes_the_free_lane() {
        // The free lane writes no reservation and no ledger row, so a route
        // admitted into it has nothing to charge a long request against. A
        // schedule whose base rate is $0 and whose high band bills real money
        // must therefore stay on the metered path.
        let free_base_paid_band = RateSchedule::new(
            ModelRates {
                input_per_mtok: Some(0.0),
                output_per_mtok: Some(0.0),
                cached_input_per_mtok: Some(0.0),
            },
            vec![crate::provider::ConditionalRate {
                min_prompt_tokens: 272_000,
                rates: banded(5.0, 0.5, 30.0),
            }],
        );
        assert!(!skips_metering(
            vec![local_rung("local/qwen", local_metadata())],
            free_base_paid_band,
        ));
        // A schedule that is $0 in every band is still free — the rule
        // narrows the skip, it does not abolish it.
        assert!(skips_metering(
            vec![local_rung("local/qwen", local_metadata())],
            RateSchedule::new(
                free_sell_rates(),
                vec![crate::provider::ConditionalRate {
                    min_prompt_tokens: 272_000,
                    rates: free_sell_rates(),
                }],
            ),
        ));
    }

    /// Settle one request at `usage` against a tier selling on `schedule`, and
    /// return what the customer was charged plus the sell rates the ledger row
    /// recorded.
    async fn settle_at(
        pool: &PgPool,
        key: &AuthenticatedKey,
        schedule: RateSchedule,
        usage: OpenAiUsage,
    ) -> (Decimal, Decimal, serde_json::Value) {
        let candidate = pass_through_candidate(schedule.clone());
        let resolved = walk_route_selling(vec![candidate.clone()], schedule);
        let request = long_walk_request();
        let reservation_usage = request.reservation_usage(64);
        let (session, request_id) = admit_walk(pool, key, &resolved, reservation_usage).await;
        let reserved = query_as::<_, (Decimal,)>(
            "SELECT reserved_cost_usd FROM usage_reservations WHERE id = $1",
        )
        .bind(request_id)
        .fetch_one(pool)
        .await
        .expect("the reservation must exist before the settle")
        .0;

        persist_usage(
            session,
            "zero/test",
            &candidate.provider,
            &candidate.model,
            Some(&candidate),
            usage,
            &resolved.sell_rates,
            RequestFeatures::from_request(
                &request,
                reservation_usage,
                PriorityResolution::new(None),
                ZeroRouterEstimate::cold(64),
            ),
            None,
            None,
            None,
            Vec::new(),
            Instant::now(),
            200,
        )
        .await
        .expect("settlement must succeed");

        let (cost_usd, sell_rates, basis_rates) =
            query_as::<_, (Decimal, serde_json::Value, Option<serde_json::Value>)>(
                "SELECT cost_usd, sell_rates, basis_rates FROM usage_events WHERE request_id = $1",
            )
            .bind(request_id)
            .fetch_one(pool)
            .await
            .expect("the settled row must exist");
        // COGS is recorded in the same band the customer was charged in. A row
        // whose sell rate repriced while its basis did not would report a
        // margin that never existed.
        assert_eq!(
            basis_rates.as_ref(),
            Some(&sell_rates),
            "this pass-through pin costs what it sells for, in whichever band applied"
        );
        let held = query_as::<_, (i64,)>("SELECT COUNT(*) FROM usage_reservations WHERE id = $1")
            .bind(request_id)
            .fetch_one(pool)
            .await
            .expect("the reservation count must query")
            .0;
        assert_eq!(held, 0, "settlement releases the reservation");
        (cost_usd, reserved, sell_rates)
    }

    #[tokio::test]
    async fn a_request_past_the_boundary_settles_at_the_high_band_end_to_end() {
        // INVARIANT: settlement prices from the MEASURED prompt, mirroring how
        // the vendor bills us. 300,000 prompt tokens is past luna's 272,000
        // boundary, so all 300,000 bill at 0.40 and the whole completion at
        // 1.80 — no split, no blend.
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let usage = measured(300_000, 10_000);
        let (charged, reserved, recorded) = settle_at(&pool, &key, luna_schedule(), usage).await;

        assert_eq!(
            charged,
            usage_cost(banded(0.4, 0.04, 1.8), usage).expect("the high band prices"),
            "a request past the boundary bills entirely at the high band"
        );
        assert!(
            charged > usage_cost(banded(0.2, 0.02, 1.2), usage).expect("the base band prices"),
            "billing it at the base band would be charging a basis ZeroRouter does not pay"
        );
        // The ledger records the band that applied, not the base rate the
        // request never touched.
        assert_eq!(recorded["input_per_mtok"], serde_json::json!(0.4));
        assert_eq!(recorded["output_per_mtok"], serde_json::json!(1.8));
        // And the reservation, sized at that same worst case, covered it: the
        // over-estimate is the byte bound, never the rate.
        assert!(
            reserved >= charged,
            "reserved {reserved} did not cover the {charged} charge"
        );
    }

    #[tokio::test]
    async fn a_request_below_the_boundary_settles_at_the_base_band() {
        // The other side of the step, and the release that makes worst-case
        // reservation safe: the customer is charged the base rate they were
        // quoted, and the difference against the dearer reservation goes back.
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let usage = measured(100_000, 10_000);
        let (charged, reserved, recorded) = settle_at(&pool, &key, luna_schedule(), usage).await;

        assert_eq!(
            charged,
            usage_cost(banded(0.2, 0.02, 1.2), usage).expect("the base band prices"),
            "a request under the boundary bills at the base band"
        );
        assert_eq!(recorded["input_per_mtok"], serde_json::json!(0.2));
        assert!(
            reserved > charged,
            "the worst-case reservation must be released down to the true charge"
        );
    }

    #[tokio::test]
    async fn a_flat_tier_settles_exactly_as_it_always_did() {
        let Some((pool, key)) = walk_fixture().await else {
            return;
        };
        let flat = banded(0.2, 0.02, 1.2);
        let usage = measured(300_000, 10_000);
        let (charged, _, recorded) = settle_at(&pool, &key, RateSchedule::flat(flat), usage).await;
        assert_eq!(charged, usage_cost(flat, usage).expect("flat rates price"));
        assert_eq!(recorded["input_per_mtok"], serde_json::json!(0.2));
    }

    /// A candidate on a named provider, for the reservation-rate rule below.
    fn rung_on(provider: &str) -> TierCandidate {
        TierCandidate {
            provider: provider.to_owned(),
            ..cloud_rung(&format!("{provider}/rung"))
        }
    }

    #[test]
    fn a_reservation_takes_the_byok_rate_only_when_every_rung_is_covered() {
        // The rule that keeps a mixed route from being under-reserved, and it
        // is the one place BYOK could cost ZeroRouter money rather than a
        // customer. The settle debit is clamped to the reservation
        // (`crate::db`), so a route reserved at 5% that then fails over to a
        // house rung settling at 100% would deliver inference ZeroRouter
        // cannot bill for.
        let covered = |providers: &[&str]| -> BTreeSet<String> {
            providers.iter().map(|p| (*p).to_owned()).collect()
        };

        // Every rung covered: the customer's own keys serve whatever the walk
        // picks, so 5% is safe.
        let all_byok = walk_route(vec![rung_on("anthropic"), rung_on("openai")]);
        assert_eq!(
            byok_reservation_rate(&all_byok, &covered(&["anthropic", "openai"])),
            byok::fee_rate()
        );

        // One rung uncovered: the walk can land on the house lane, so the
        // reservation must cover the house price. Over-reserving costs the
        // customer nothing — settlement releases the difference.
        assert_eq!(
            byok_reservation_rate(&all_byok, &covered(&["anthropic"])),
            byok::house_rate(),
            "a mixed route must reserve at the house rate"
        );

        // No coverage at all, and coverage for something not on this route,
        // are both just the house rate.
        assert_eq!(
            byok_reservation_rate(&all_byok, &covered(&[])),
            byok::house_rate()
        );
        assert_eq!(
            byok_reservation_rate(&all_byok, &covered(&["google"])),
            byok::house_rate(),
            "a key for a provider this route never names changes nothing"
        );

        // A single-rung route is the common case and gets the fee it should.
        let single = walk_route(vec![rung_on("anthropic")]);
        assert_eq!(
            byok_reservation_rate(&single, &covered(&["anthropic"])),
            byok::fee_rate()
        );
    }

    #[test]
    fn the_reserved_amount_is_five_percent_when_the_route_is_wholly_byok() {
        // The rate rule above, carried through to the figure admission
        // actually checks against the balance — so the two cannot drift.
        let request = walk_request();
        let route = walk_route(vec![rung_on("anthropic")]);
        let house = sized_reservation(&request, &route, 64, byok::house_rate())
            .expect("the house reservation must size");
        let byok_sized = sized_reservation(&request, &route, 64, byok::fee_rate())
            .expect("the byok reservation must size");

        assert!(house.cost_usd > Decimal::ZERO, "the fixture tier is priced");
        assert_eq!(
            byok_sized.cost_usd * Decimal::from(20),
            house.cost_usd,
            "a wholly-BYOK route reserves exactly one twentieth of the catalog worst case"
        );
        // Everything else about the reservation is unchanged: the fee is a
        // price, not a different request.
        assert_eq!(byok_sized.total_tokens, house.total_tokens);
        assert_eq!(byok_sized.output_tokens, house.output_tokens);
    }

    fn walk_route_selling(
        candidates: Vec<TierCandidate>,
        sell_rates: RateSchedule,
    ) -> ResolvedRoute {
        ResolvedRoute {
            requested_model: "zero/test".to_owned(),
            candidates,
            sell_rates,
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
            ReservationSizing::cold(ReservationSize {
                total_tokens: i64::try_from(reservation_usage.total_tokens)
                    .expect("reservation should fit"),
                output_tokens: 64,
                // `worst_case`, mirroring `sized_reservation`: admission has
                // no measured prompt to select a band with, so it reserves at
                // the dearest rate the schedule can charge.
                cost_usd: usage_cost(resolved.sell_rates.worst_case(), reservation_usage)
                    .expect("sell rates must price"),
            }),
            ByokReservation::default(),
            task_signature("walk-user", &[], 1, 128, true, 64),
            false,
            MeteringLane::Reserved,
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
        // This spends the 5s `SSE_SEND_TIMEOUT` in real time, deliberately.
        //
        // It used to wrap the call in `tokio::time::pause()`/`resume()` to skip
        // that wait, on the stated grounds that the pool is warm so "the settle
        // below is unaffected". That premise was false and the test failed
        // roughly three runs in four: the settle happens INSIDE
        // `stream_to_channel`, so it ran under the paused clock too, and a
        // paused clock auto-advances whenever the runtime parks — which is
        // exactly what awaiting a real socket does. Time therefore jumped while
        // Postgres was mid-answer, firing a timeout that belonged to a
        // millisecond later, and the row this test then queries was never
        // written: `settled row must query: RowNotFound`. Load on the database
        // made it likelier, so it failed most often in a full suite run and
        // passed alone, which reads like DB contention and is not.
        //
        // No pause is possible while the settle shares this call: the timer to
        // skip and the I/O that must not be skipped are the same await. Five
        // seconds is the honest price of testing a timeout end to end.
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
        let served_basis = usage_cost(
            candidate.rates.at_prompt_tokens(billed.prompt_tokens),
            billed,
        )
        .expect("candidate rates must price");
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
            usage_cost(candidate.rates.base(), OpenAiUsage::default()),
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

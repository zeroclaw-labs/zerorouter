use std::{borrow::Cow, env, str::FromStr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
// Named directly rather than through the `crate::sqlx` shim: the shim exists to
// keep the sqlx-core/sqlx-postgres split out of call sites, and [`admit_key_mint`]
// is the only place in the crate that needs a bare connection handle.
use sqlx_postgres::PgConnection;
use uuid::Uuid;
use zeroclaw_providers::pricing::ModelRates;

use crate::{
    auth::AuthenticatedKey,
    openai::{OpenAiUsage, PromptTokenDetails, TaskSignature, usage_cost},
    priority::Priority,
    sqlx::{
        self, PgPool,
        migrate::{Migration, MigrationType, Migrator},
        postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    },
};

/// The permanent Stage-1 reservation sizing basis: byte bound + max_tokens.
/// Learned/quote sizings arrive in later rollout stages (migration 0004
/// comment on `estimator_basis`).
const ESTIMATOR_BASIS_COLD: &str = "cold";

#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub tier: String,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub usage: OpenAiUsage,
    pub cost_usd: Decimal,
    pub latency_ms: i32,
    pub status: i16,
    pub telemetry: RequestTelemetry,
    pub attempts: Vec<AttemptRecord>,
}

/// The estimate-and-select telemetry captured on the append-only
/// `usage_events` row at settle (migration 0004). Every field is written at
/// INSERT time only; the reservation provenance (task signature, reserved
/// output/cost, estimator basis) rides [`UsageSession`] from admission because
/// the reservation row is destroyed by the settle `DELETE...RETURNING`.
///
/// Priority and declared-validator columns are Stage-1-inert (the knob and
/// validators ship in later stages) and are always written NULL here.
#[derive(Clone, Debug)]
pub struct RequestTelemetry {
    // Raw request-shape features, persisted so task signatures can be
    // re-bucketed retroactively. Prompt content itself is never persisted.
    pub requested_max_tokens: i32,
    pub stream: bool,
    pub prompt_bytes: i64,
    pub message_count: i32,
    pub tool_count: i32,
    // Candidate provenance for the row: the served candidate, or the last one
    // tried when the request died mid-walk. `None` only where
    // upstream_provider/upstream_model themselves carry a sentinel because no
    // candidate had been selected ('fallback-chain', 'none'/'client-disconnected').
    pub candidate_id: Option<String>,
    pub basis_rates: Option<ModelRates>,
    pub sell_rates: ModelRates,
    // Synthesized outcome labels; `None` where no completion was produced.
    pub finish_reason: Option<String>,
    pub shape_ok: Option<bool>,
    // The resolved priority knob (rollout Stage 3a). `None` only when
    // replaying a settlement intent persisted before the knob shipped —
    // exactly migration 0004's "NULL = row predates the knob". The live path
    // always resolves one, `balanced` being the default.
    pub priority: Option<Priority>,
}

impl RequestTelemetry {
    /// The served attempt's COGS for the settled row.
    ///
    /// Prefer the served attempt's own priced basis over re-pricing the settled
    /// usage: on the router-owned walk they agree by construction, and where
    /// they do not it is because the delivery went unmetered and the customer
    /// was billed nothing (`StreamDelivery::settled_usage`). Re-pricing the
    /// billed usage there would report ZeroRouter's cost for that delivery as
    /// zero while the attempt row carries a floor for it, so the request's real
    /// COGS would appear on neither side of
    /// `cost_usd - cost_basis_usd - attempts_cost_basis_usd`.
    ///
    /// With no served attempt this falls back to pricing the settled usage,
    /// which is what those paths have always done. That set shrank when the
    /// non-streaming walk was unrolled into the router: the buffered success
    /// path now records a served attempt of its own, so it reads
    /// `served.cost_basis_usd` like the streaming path does. What is left on
    /// the fallback is the terminals that recorded no served attempt — the
    /// three `api::WalkTerminal` settles, the non-streaming metering-gap branch
    /// (`served = false`, because the body is discarded and the customer gets a
    /// 503), and the streaming terminals that delivered nothing.
    ///
    /// No money moved when that set shrank, and it is worth saying why rather
    /// than leaving it to be re-derived: `AttemptTokens::measured(usage)
    /// .priceable()` reconstructs exactly the prompt/cached/completion figures
    /// the fallback would have priced, and both arms price them at the same
    /// `candidate.rates`. The two arms agree by construction on that path;
    /// `non_streaming_attribution_survives_a_retry_on_the_primary` pins the
    /// value.
    fn cost_basis(&self, record: &UsageRecord) -> Option<Decimal> {
        record
            .attempts
            .iter()
            .find(|attempt| attempt.served)
            .map_or_else(
                || {
                    // `and_then`: unpriceable basis rates leave this NULL —
                    // "not captured" — rather than reporting the request's COGS
                    // as zero, which would overstate margin by the whole basis.
                    self.basis_rates
                        .and_then(|rates| usage_cost(rates, record.usage))
                },
                |served| served.cost_basis_usd,
            )
    }
}

/// What is known about one attempt's token consumption.
///
/// The three dimensions are independently optional because they are
/// independently knowable, and `request_attempts` has had three independently
/// nullable columns since migration 0004. An abandoned stream is the case that
/// forces it: the per-chunk `token_count` gives an OUTPUT floor and says
/// nothing whatever about the prompt the attempt certainly consumed. Collapsing
/// that into one `Option<OpenAiUsage>` meant writing `input_tokens = 0` — the
/// ledger asserting an upstream call consumed no prompt, which is never true of
/// a dispatched attempt.
///
/// `None` is written as SQL NULL, the ledger's word for "not captured". Zero is
/// reserved for "measured, and it was zero".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttemptTokens {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
}

impl AttemptTokens {
    /// Every dimension measured, from an upstream usage report.
    #[must_use]
    pub fn measured(usage: OpenAiUsage) -> Self {
        Self {
            input: Some(usage.prompt_tokens),
            cached_input: Some(usage.cached_input_tokens()),
            output: Some(usage.completion_tokens),
        }
    }

    /// Nothing measured: the upstream reported no usage and the attempt
    /// produced no chunk-level floor either.
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// An output floor from the per-chunk `token_count` a stream already
    /// reports, with the prompt side left unknown. See
    /// `api::attempt_tokens` for why the prompt side stays NULL rather
    /// than borrowing the reservation's byte bound.
    #[must_use]
    pub fn output_floor(output: u64) -> Self {
        Self {
            output: Some(output),
            ..Self::default()
        }
    }

    /// The usage to price this attempt's COGS from, or `None` when nothing at
    /// all is known. Unknown dimensions price as zero — a known-partial cost is
    /// a floor, which is why anything but [`Self::measured`] marks the
    /// request's `attempts_cost_basis_complete` FALSE.
    #[must_use]
    pub fn priceable(self) -> Option<OpenAiUsage> {
        self.input.or(self.cached_input).or(self.output)?;
        let input = self.input.unwrap_or(0);
        let cached = self.cached_input.unwrap_or(0).min(input);
        let output = self.output.unwrap_or(0);
        Some(OpenAiUsage {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: input.saturating_add(output),
            prompt_tokens_details: (cached > 0).then_some(PromptTokenDetails {
                cached_tokens: cached,
            }),
        })
    }

    /// Whether every dimension is known. Only a fully measured attempt can
    /// contribute to a COGS sum that claims to be a total.
    #[must_use]
    pub fn is_complete(self) -> bool {
        self.input.is_some() && self.cached_input.is_some() && self.output.is_some()
    }
}

/// One row of the append-only `request_attempts` walk ledger (migration
/// 0004), buffered in memory during the router-owned streaming walk and
/// inserted in the same transaction as the `usage_events` row, after it.
#[derive(Clone, Debug)]
pub struct AttemptRecord {
    pub attempt_no: i16,
    pub started_at: DateTime<Utc>,
    pub candidate_id: String,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub outcome: String,
    /// TRUE on the one attempt whose model output reached the customer and
    /// which the settled row is therefore priced from.
    ///
    /// This is the ledger's cost-attribution switch, and exactly one place
    /// counts each attempt's COGS: the served attempt through
    /// `usage_events.cost_basis_usd`, every other attempt through
    /// `usage_events.attempts_cost_basis_usd`. It used to be FALSE on the
    /// timeout and shutdown terminals even when that attempt's output had been
    /// delivered and billed, so its COGS was counted on BOTH sides at once —
    /// understating margin while the walk ledger simultaneously claimed no
    /// candidate had served the request.
    pub served: bool,
    pub latency_ms: i32,
    pub tokens: AttemptTokens,
    /// Whether the known token dimensions come from the per-chunk `token_count`
    /// floor rather than an upstream usage report.
    pub tokens_estimated: bool,
    pub cost_basis_usd: Option<Decimal>,
    pub finish_reason: Option<String>,
    pub validator_kind: Option<String>,
}

impl AttemptRecord {
    /// Whether this attempt's COGS is a measurement rather than a floor or a
    /// blank. Drives `usage_events.attempts_cost_basis_complete`.
    fn cogs_is_measured(&self) -> bool {
        self.cost_basis_usd.is_some() && self.tokens.is_complete() && !self.tokens_estimated
    }
}

/// The non-serving attempts' COGS, and whether that number is the whole story.
///
/// Both fields are `None` only when no attempts were recorded at all. When
/// attempts exist but none of them lost (a single-attempt request that served),
/// the sum is a genuine zero and `complete` is TRUE: there were no losing
/// attempts, which is a different statement from not knowing what they cost.
///
/// # A reading boundary in the data, not in the code
///
/// A first-try non-streaming success used to record no attempts at all and so
/// settled `(NULL, NULL)` — "unknown". Since the walk was unrolled into the
/// router it records its served attempt, so the same request shape now settles
/// `(0, TRUE)` — "there were no losing attempts", which is migration 0007's
/// intended reading (`0007_ledger_honesty.sql:44-52`) and the one the streaming
/// path already wrote. Nothing about the request changed; what changed is that
/// the ledger can now tell the two apart. Anyone comparing
/// `attempts_cost_basis_usd` across that commit is comparing a NULL that meant
/// "no rows" with a zero that means "no losses", and must not read the change
/// as COGS appearing or disappearing.
struct AttemptCogs {
    total: Option<Decimal>,
    complete: Option<bool>,
}

impl AttemptCogs {
    fn summarize(attempts: &[AttemptRecord]) -> Self {
        if attempts.is_empty() {
            return Self {
                total: None,
                complete: None,
            };
        }
        let losing = attempts.iter().filter(|attempt| !attempt.served);
        let mut total = Decimal::ZERO;
        let mut complete = true;
        for attempt in losing {
            total += attempt.cost_basis_usd.unwrap_or(Decimal::ZERO);
            complete &= attempt.cogs_is_measured();
        }
        Self {
            total: Some(total),
            complete: Some(complete),
        }
    }
}

fn rates_snapshot(rates: &ModelRates) -> String {
    json!({
        "input_per_mtok": rates.input_per_mtok,
        "cached_input_per_mtok": rates.cached_input_per_mtok,
        "output_per_mtok": rates.output_per_mtok,
    })
    .to_string()
}

/// The outcome of a cap/credit admission decision.
///
/// # Quota model
///
/// Spend and velocity are enforced at TWO scopes and the tighter one wins:
///
/// * **per key** — the presenting key's own settled + in-flight usage against
///   its own `api_keys.spend_cap_usd` / `velocity_cap_tokens_per_min`. This is
///   B0's original check, unchanged.
/// * **per user** — every key the owning user holds, settled + in-flight,
///   against a ceiling *derived* from the same columns: the largest cap
///   configured on any of the user's live keys.
///
/// The user scope exists because keys are free and churnable (`disable_key` is
/// a flag flip, `portal::disable_key`), so a per-key-only quota is reset by
/// minting a new key and multiplied by holding several at once. Counting a
/// user's whole key set closes both.
///
/// The ceiling is **derived, not configured**: the schema has no per-user cap
/// column and this change deliberately adds no configuration surface for one.
/// Taking the maximum over the user's live keys means minting more keys can
/// never raise the ceiling past what one key could already reach, while a
/// single-key user's behavior is unchanged (the maximum is that key's own cap).
pub enum UsageAdmission {
    Allowed(UsageSession),
    Unauthorized,
    SpendExceeded,
    VelocityExceeded,
    InsufficientCredits,
}

pub struct UsageSession {
    pool: PgPool,
    reservation_id: Uuid,
    api_key_id: Uuid,
    user_id: Uuid,
    require_credits: bool,
    // Reservation provenance copied into the ledger at settle: the
    // usage_reservations row is destroyed by the settle DELETE...RETURNING, so
    // these must be carried in memory from admission. The signature travels as
    // a whole [`TaskSignature`] so the scheme that computed it and the tool
    // digest it was computed from land on the same row as the key itself
    // (migration 0007) — a key with no provenance cannot be re-keyed later.
    task_signature: TaskSignature,
    reserved_output_tokens: i32,
    reserved_cost_usd: Decimal,
}

const DATABASE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const RESERVATION_TTL: Duration = Duration::from_secs(20 * 60);

/// Settle transactions one request may run before it hands the durable intent
/// to the recovery sweep. Bounded on purpose: the client is already waiting
/// (streaming terminals settle before the error frame is written), so the
/// in-request budget exists to ride out a lock-timeout or a dropped connection,
/// not to outlast an outage. Everything longer than that is recovery's job.
const SETTLEMENT_ATTEMPTS: u32 = 3;

/// Backoff before the first in-request retry, tripled for the second. The whole
/// budget is under 200 ms of added latency in the worst case.
const SETTLEMENT_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// How long a settlement intent must sit before [`recover_owed_settlements`]
/// will replay it. Longer than the in-request retry budget by three orders of
/// magnitude, so the sweep cannot race a request that is still trying — and
/// even if it did, the settle is idempotent and the two serialize on the
/// per-user advisory lock.
const SETTLEMENT_RECOVERY_GRACE: Duration = Duration::from_secs(60);

/// Total settle attempts, in-request and recovery together, before a
/// reservation is quarantined for an operator instead of retried again.
const MAX_SETTLE_ATTEMPTS: i32 = 8;

/// Payload-format discriminant on [`SettlementIntent`]. A stored payload whose
/// version this build does not know is quarantined, never guessed at: the
/// alternative is deserializing an unknown shape into a wrong charge.
const SETTLEMENT_INTENT_VERSION: u8 = 1;

pub async fn database_pool_from_env() -> Result<PgPool> {
    let pool = if let Ok(database_url) = env::var("DATABASE_URL") {
        let mut options = PgConnectOptions::from_str(&database_url)
            .map_err(|_| anyhow!("DATABASE_URL is invalid"))?;
        // The DATABASE_URL path is a developer convenience for a loopback
        // database. A non-loopback target must be at least as hardened as the
        // production DB_* path: verify-full TLS with the pinned CA. Otherwise
        // this branch would silently weaken the documented TLS invariant.
        if !is_loopback_host(options.get_host()) {
            let ssl_root_cert = env::var("DB_SSL_ROOT_CERT")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let Some(ssl_root_cert) = ssl_root_cert else {
                bail!(
                    "DATABASE_URL points at a non-loopback host without DB_SSL_ROOT_CERT; \
                     use the DB_HOST/DB_* variables (which enforce verify-full TLS) for remote databases"
                );
            };
            options = options
                .ssl_mode(PgSslMode::VerifyFull)
                .ssl_root_cert(&ssl_root_cert);
        }
        PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(DATABASE_ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .context("failed to connect using DATABASE_URL")?
    } else {
        let host = required_env("DB_HOST")?;
        let database = required_env("DB_NAME")?;
        let username = required_env("DB_USERNAME")?;
        let password = required_env("DB_PASSWORD")?;
        let ssl_root_cert = required_env("DB_SSL_ROOT_CERT")?;
        let port = env::var("DB_PORT")
            .unwrap_or_else(|_| "5432".to_owned())
            .parse::<u16>()
            .context("DB_PORT must be a valid TCP port")?;
        let options = PgConnectOptions::new()
            .host(&host)
            .port(port)
            .database(&database)
            .username(&username)
            .password(&password)
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert(&ssl_root_cert);
        PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(DATABASE_ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .context("failed to connect using DB_HOST credentials")?
    };
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    let migrator = Migrator {
        migrations: Cow::Owned(vec![
            Migration::new(
                1,
                Cow::Borrowed("b0 schema"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0001_b0_schema.sql")),
                false,
            ),
            Migration::new(
                2,
                Cow::Borrowed("billing and web"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0002_billing_and_web.sql")),
                false,
            ),
            Migration::new(
                3,
                Cow::Borrowed("balance nonnegative"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0003_balance_nonnegative.sql")),
                false,
            ),
            Migration::new(
                4,
                Cow::Borrowed("estimate and select substrate"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!(
                    "../migrations/0004_estimate_and_select_substrate.sql"
                )),
                false,
            ),
            Migration::new(
                5,
                Cow::Borrowed("stripe checkout intents"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!(
                    "../migrations/0005_stripe_checkout_intents.sql"
                )),
                false,
            ),
            Migration::new(
                6,
                Cow::Borrowed("settlement outbox"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0006_settlement_outbox.sql")),
                false,
            ),
            Migration::new(
                7,
                Cow::Borrowed("ledger honesty"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0007_ledger_honesty.sql")),
                false,
            ),
        ]),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator
        .run(pool)
        .await
        .context("database migration failed")
}

pub async fn begin_usage_session(
    pool: &PgPool,
    key: &AuthenticatedKey,
    reserved_tokens: i64,
    reserved_output_tokens: i64,
    reserved_cost_usd: Decimal,
    task_signature: TaskSignature,
    require_credits: bool,
) -> Result<UsageAdmission, sqlx::Error> {
    if reserved_tokens < 0 || reserved_output_tokens < 0 || reserved_cost_usd < Decimal::ZERO {
        return Err(sqlx::Error::Protocol(
            "usage reservation cannot be negative".to_owned(),
        ));
    }

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Serialize admission per USER (not per key) so concurrent requests
    // across a user's keys observe each other's reservations and cannot
    // jointly overdraw the prepaid balance.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(key.user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    // The presenting key's liveness and both ceilings, atomically, in one round
    // trip. `spend_cap_usd` / `velocity_cap_tokens_per_min` are the presenting
    // key's own caps; the two derived columns are the USER ceiling (see
    // [`UsageAdmission`] on why a user ceiling exists and why it is derived
    // rather than configured, and on why it is the MAX and not the sum). The
    // presenting key must still be live for admission to proceed, so it is
    // always inside the sibling MAX: the derived ceiling can never fall below
    // the presenting key's own cap, and a single-key user's admission is
    // bit-for-bit the pre-change one.
    //
    // # Why this is an UPDATE and not a SELECT
    //
    // Not for `last_used_at` — that ride-along is incidental. It is an UPDATE
    // because the unlocked `SELECT ... disabled` it replaces could read
    // `disabled = false`, have a revocation commit underneath it, and still
    // admit: the operator got their 204 and the revoked key then dispatched one
    // more inference. `UPDATE ... WHERE NOT disabled` takes the row lock, and
    // under READ COMMITTED Postgres re-evaluates the predicate against the
    // freshly committed row version once any concurrent writer releases it. A
    // revocation that commits first therefore leaves this matching zero rows,
    // and admission refuses.
    //
    // The converse ordering is equally sound: if admission's UPDATE commits
    // first, `disable_key`'s own conditional UPDATE blocks on this row lock and
    // does not return 204 until this transaction is done. So "a successful
    // disable" and "no further dispatch" never overlap in either direction.
    //
    // # How it composes with the rest of admission
    //
    // - The per-user advisory lock above is still taken first, so lock ordering
    //   in this crate is advisory-then-row everywhere. Both revocation paths
    //   ([`crate::portal`], [`crate::admin`]) take only the api_keys row lock and
    //   never the advisory lock, so there is no cycle to deadlock on.
    // - `SET LOCAL lock_timeout = '5s'` covers this statement. Waiting behind a
    //   revocation that does not commit surfaces as an `Err`, and every error
    //   path out of this function refuses admission — fail-closed is preserved.
    // - The user-scoped quota reads below are untouched and still run in this
    //   same transaction, now with the presenting key's row locked, so a
    //   revocation cannot land between the liveness check and the reservation.
    //
    // The sibling subqueries are evaluated against the statement snapshot, so a
    // sibling revoked *during* this statement can still widen the derived
    // ceiling by one statement's worth of staleness. That was true of the SELECT
    // this replaces and is not what the fix is about: siblings only relax a
    // ceiling, never authorize the presenting key, which is checked above.
    //
    // Matching `presenting.user_id` against the id the advisory lock was taken
    // on makes the lock's scope and the statement's scope provably the same set
    // of rows; a stale [`AuthenticatedKey`] whose cached owner disagrees with
    // the row matches nothing and is rejected.
    let key_state = sqlx::query_as::<_, (Decimal, i32, Decimal, i32)>(
        r#"
        UPDATE api_keys AS presenting
        SET last_used_at = NOW()
        WHERE presenting.id = $1
          AND presenting.user_id = $2
          AND NOT presenting.disabled
        RETURNING
            presenting.spend_cap_usd,
            presenting.velocity_cap_tokens_per_min,
            COALESCE((
                SELECT MAX(sibling.spend_cap_usd)
                FROM api_keys AS sibling
                WHERE sibling.user_id = $2 AND NOT sibling.disabled
            ), presenting.spend_cap_usd) AS user_spend_cap_usd,
            COALESCE((
                SELECT MAX(sibling.velocity_cap_tokens_per_min)
                FROM api_keys AS sibling
                WHERE sibling.user_id = $2 AND NOT sibling.disabled
            ), presenting.velocity_cap_tokens_per_min) AS user_velocity_cap
        "#,
    )
    .bind(key.id)
    .bind(key.user_id)
    .fetch_optional(&mut *transaction)
    .await?;
    // Absent, owned by someone else, or revoked: one answer for all three, so
    // admission never becomes an oracle over another user's key ids.
    let Some((
        spend_cap_usd,
        velocity_cap_tokens_per_min,
        user_spend_cap_usd,
        user_velocity_cap_tokens_per_min,
    )) = key_state
    else {
        transaction.rollback().await?;
        return Ok(UsageAdmission::Unauthorized);
    };

    // Reclaim expired reservations, but only the ones that owe nothing. A row
    // carrying a settlement intent is money the customer already received and
    // ZeroRouter has not yet recorded; deleting it (which is what this sweep
    // used to do unconditionally) erases the charge, the usage event, and every
    // trace that either was owed. Those are quarantined instead — parked for
    // reconciliation and readable through [`quarantined_settlements`].
    //
    // Quarantining rather than deleting cannot loosen any cap: every
    // cap/credit aggregate below filters `expires_at > NOW()`, so a row that
    // survives here is invisible to admission exactly as a deleted one was.
    sqlx::query(
        "DELETE FROM usage_reservations WHERE expires_at <= NOW() AND settlement_intent IS NULL",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        UPDATE usage_reservations
        SET quarantined_at = NOW()
        WHERE expires_at <= NOW()
          AND settlement_intent IS NOT NULL
          AND quarantined_at IS NULL
        "#,
    )
    .execute(&mut *transaction)
    .await?;

    // Settled usage, aggregated across EVERY key the user owns (disabled ones
    // included — a disabled key's history is still the user's spend) and, in
    // the same scan, restricted to the presenting key so the per-key ceiling
    // can be checked without a second round trip.
    //
    // Access path, measured on 500k `usage_events` with a 20-key user holding
    // 50k of them: nested loop over `api_keys_user_id_idx` (0001), then one
    // per-key range scan of `usage_events_key_timestamp_idx (api_key_id, ts
    // DESC)` (0001). No sequential scan, so the existing indexes serve this and
    // no new one is needed. The cost that DOES grow is row count — the scan
    // reads a user's whole month-to-date history (~14 ms for 30k rows here).
    // If that ever gets hot, the measured fix is widening the 0001 index to
    // INCLUDE (cost_usd, input_tokens, output_tokens), which turns it into an
    // index-only scan (0 heap fetches, ~5 ms for the same 30k rows).
    let (user_monthly_spend, user_recent_tokens, monthly_spend, recent_tokens) =
        sqlx::query_as::<_, (Decimal, i64, Decimal, i64)>(
            r#"
        SELECT
            COALESCE(
                SUM(usage_events.cost_usd) FILTER (
                    WHERE usage_events.ts
                        >= date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                ),
                0
            ) AS user_monthly_spend,
            COALESCE(
                SUM(usage_events.input_tokens::BIGINT + usage_events.output_tokens::BIGINT) FILTER (
                    WHERE usage_events.ts >= NOW() - INTERVAL '1 minute'
                ),
                0
            )::BIGINT AS user_recent_tokens,
            COALESCE(
                SUM(usage_events.cost_usd) FILTER (
                    WHERE usage_events.api_key_id = $2
                      AND usage_events.ts
                        >= date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
                ),
                0
            ) AS monthly_spend,
            COALESCE(
                SUM(usage_events.input_tokens::BIGINT + usage_events.output_tokens::BIGINT) FILTER (
                    WHERE usage_events.api_key_id = $2
                      AND usage_events.ts >= NOW() - INTERVAL '1 minute'
                ),
                0
            )::BIGINT AS recent_tokens
        FROM usage_events
        INNER JOIN api_keys ON api_keys.id = usage_events.api_key_id
        WHERE api_keys.user_id = $1
          AND usage_events.ts >= LEAST(
              date_trunc('month', NOW() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC',
              NOW() - INTERVAL '1 minute'
          )
        "#,
        )
        .bind(key.user_id)
        .bind(key.id)
        .fetch_one(&mut *transaction)
        .await?;
    // In-flight reservations, same two scopes. Expired rows were deleted above,
    // and the `expires_at > NOW()` predicate is kept so a row that expires
    // between the two statements is not counted.
    let (
        user_active_reserved_cost,
        user_active_reserved_tokens,
        active_reserved_cost,
        active_reserved_tokens,
    ) = sqlx::query_as::<_, (Decimal, i64, Decimal, i64)>(
        r#"
        SELECT
            COALESCE(SUM(usage_reservations.reserved_cost_usd), 0),
            COALESCE(SUM(usage_reservations.reserved_tokens), 0)::BIGINT,
            COALESCE(
                SUM(usage_reservations.reserved_cost_usd) FILTER (
                    WHERE usage_reservations.api_key_id = $2
                ),
                0
            ),
            COALESCE(
                SUM(usage_reservations.reserved_tokens) FILTER (
                    WHERE usage_reservations.api_key_id = $2
                ),
                0
            )::BIGINT
        FROM usage_reservations
        INNER JOIN api_keys ON api_keys.id = usage_reservations.api_key_id
        WHERE api_keys.user_id = $1
          AND usage_reservations.expires_at > NOW()
        "#,
    )
    .bind(key.user_id)
    .bind(key.id)
    .fetch_one(&mut *transaction)
    .await?;

    // Both ceilings are enforced and the tighter one wins: a request is
    // admitted only if it fits under the presenting key's own cap AND under the
    // user's derived cap. Aggregating across the user's keys is what makes
    // disable-and-remint (and device-grant fan-out) stop multiplying the
    // allowance — the churned keys' usage still counts.
    if !spend_within_cap(
        monthly_spend,
        active_reserved_cost,
        reserved_cost_usd,
        spend_cap_usd,
    ) || !spend_within_cap(
        user_monthly_spend,
        user_active_reserved_cost,
        reserved_cost_usd,
        user_spend_cap_usd,
    ) {
        transaction.rollback().await?;
        return Ok(UsageAdmission::SpendExceeded);
    }

    if !velocity_within_cap(
        recent_tokens,
        active_reserved_tokens,
        reserved_tokens,
        i64::from(velocity_cap_tokens_per_min),
    ) || !velocity_within_cap(
        user_recent_tokens,
        user_active_reserved_tokens,
        reserved_tokens,
        i64::from(user_velocity_cap_tokens_per_min),
    ) {
        transaction.rollback().await?;
        return Ok(UsageAdmission::VelocityExceeded);
    }

    if require_credits {
        let balance =
            sqlx::query_scalar::<_, Decimal>("SELECT credit_balance_usd FROM users WHERE id = $1")
                .bind(key.user_id)
                .fetch_optional(&mut *transaction)
                .await?;
        let Some(balance) = balance else {
            transaction.rollback().await?;
            return Ok(UsageAdmission::Unauthorized);
        };
        let active_user_reserved = sqlx::query_scalar::<_, Decimal>(
            r#"
            SELECT COALESCE(SUM(usage_reservations.reserved_cost_usd), 0)
            FROM usage_reservations
            INNER JOIN api_keys ON api_keys.id = usage_reservations.api_key_id
            WHERE api_keys.user_id = $1
              AND usage_reservations.expires_at > NOW()
            "#,
        )
        .bind(key.user_id)
        .fetch_one(&mut *transaction)
        .await?;
        if balance - active_user_reserved < reserved_cost_usd {
            transaction.rollback().await?;
            return Ok(UsageAdmission::InsufficientCredits);
        }
    }

    // Narrowed only after every admission gate has run, so this can never
    // displace a cap decision: reserved_tokens is >= reserved_output_tokens for
    // every caller, and the velocity cap is an INTEGER column, so any output
    // bound past the i32 range is rejected by the velocity gate above first.
    let reserved_output_tokens = i32::try_from(reserved_output_tokens).map_err(|_| {
        sqlx::Error::Protocol("reserved output tokens exceed the database integer range".to_owned())
    })?;
    let reservation_id = Uuid::new_v4();
    let reservation_ttl_seconds = i64::try_from(RESERVATION_TTL.as_secs()).map_err(|_| {
        sqlx::Error::Protocol("usage reservation lifetime exceeds the database range".to_owned())
    })?;
    sqlx::query(
        r#"
        INSERT INTO usage_reservations (
            id,
            api_key_id,
            expires_at,
            reserved_tokens,
            reserved_cost_usd
        )
        VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second'), $4, $5)
        "#,
    )
    .bind(reservation_id)
    .bind(key.id)
    .bind(reservation_ttl_seconds)
    .bind(reserved_tokens)
    .bind(reserved_cost_usd)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(UsageAdmission::Allowed(UsageSession {
        pool: pool.clone(),
        reservation_id,
        api_key_id: key.id,
        user_id: key.user_id,
        require_credits,
        task_signature,
        reserved_output_tokens,
        reserved_cost_usd,
    }))
}

impl UsageSession {
    #[must_use]
    pub fn request_id(&self) -> String {
        format!("chatcmpl-{}", self.reservation_id.simple())
    }

    /// Settle this request.
    ///
    /// Two durable steps, in this order and never the other way round:
    ///
    /// 1. **Record the intent.** The whole settle payload is written onto the
    ///    reservation row in its own autocommit statement, BEFORE any settle
    ///    transaction runs. Until that commits, the only copy of what the
    ///    customer owes lives in this process's memory; after it, a crash, a
    ///    dropped connection or a rolled-back settle all leave a row that
    ///    [`recover_owed_settlements`] can replay verbatim.
    /// 2. **Settle, retrying transient failures.** [`settle_once`] is
    ///    idempotent — see its documentation for why a retry can neither
    ///    double-debit nor be defeated by a duplicate `request_id` — so the
    ///    only question a retry has to answer is whether the failure was worth
    ///    retrying inside the request at all. A permanent failure returns
    ///    immediately and leaves the durable intent behind instead of burning
    ///    the budget on a settle that cannot start working.
    ///
    /// Takes `&self`. The old signature consumed the session, so the single
    /// settle attempt it ran was also the last one that could ever be made:
    /// any failure after that point returned an error with the payload gone.
    pub async fn record(&self, record: &UsageRecord) -> Result<(), sqlx::Error> {
        let intent = SettlementIntent::new(self, record);
        if let Err(error) = self.persist_intent(&intent).await {
            // Not fatal on its own — the settle below may still succeed — but
            // it means this request has lost its safety net, so it is reported
            // at the same level as a lost charge.
            tracing::error!(
                request_id = %self.reservation_id,
                error = %error,
                "settlement intent could not be persisted; a failed settle for this request would not be recoverable"
            );
        }
        settle_with_retry(&self.pool, self.reservation_id, self.api_key_id, &intent).await
    }

    /// Write the settle payload onto the reservation row that carries it.
    ///
    /// Deliberately outside any transaction: an intent that rolls back
    /// alongside the settle it exists to survive is not an intent. Zero rows
    /// affected means the reservation is already gone; nothing is decided
    /// here, because the settle attempt that follows is what distinguishes
    /// "already settled" (success) from a genuinely lost reservation (error).
    async fn persist_intent(&self, intent: &SettlementIntent) -> Result<(), sqlx::Error> {
        let payload = serde_json::to_string(intent).map_err(|error| {
            sqlx::Error::Protocol(format!("settlement intent is not serializable: {error}"))
        })?;
        sqlx::query(
            r#"
            UPDATE usage_reservations
            SET settlement_intent = $3::JSONB, settlement_intent_at = NOW()
            WHERE id = $1 AND api_key_id = $2
            "#,
        )
        .bind(self.reservation_id)
        .bind(self.api_key_id)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Whether a settle transaction did the work or found it already done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettleOutcome {
    Settled,
    /// A previous attempt already committed this request's settled row. The
    /// reservation (if one was still lying around) has been reclaimed.
    AlreadySettled,
}

/// Run [`settle_once`] until it succeeds, the failure stops being transient, or
/// the in-request budget runs out.
///
/// Every failure is stamped on the reservation row before it is retried or
/// given up on, so `settle_attempts` and `last_settle_error` describe reality
/// even if this process dies immediately afterwards.
async fn settle_with_retry(
    pool: &PgPool,
    reservation_id: Uuid,
    api_key_id: Uuid,
    intent: &SettlementIntent,
) -> Result<(), sqlx::Error> {
    let mut backoff = SETTLEMENT_RETRY_BACKOFF;
    let mut attempt = 1_u32;
    loop {
        match settle_once(pool, reservation_id, api_key_id, intent).await {
            Ok(SettleOutcome::Settled) => return Ok(()),
            Ok(SettleOutcome::AlreadySettled) => {
                // The ambiguous-COMMIT case resolved in the customer's and
                // ZeroRouter's favour: the money moved exactly once, and this
                // attempt proved it rather than moving it again.
                tracing::warn!(
                    request_id = %reservation_id,
                    attempt,
                    "settlement was already committed by an earlier attempt"
                );
                return Ok(());
            }
            Err(error) => {
                let transient = is_transient(&error);
                let quarantined = record_settle_failure(pool, reservation_id, &error)
                    .await
                    .unwrap_or(None);
                if !transient || attempt >= SETTLEMENT_ATTEMPTS {
                    tracing::error!(
                        request_id = %reservation_id,
                        attempt,
                        transient,
                        quarantined = quarantined.unwrap_or(false),
                        error = %error,
                        "settlement failed; the durable intent is left for recovery"
                    );
                    return Err(error);
                }
                tracing::warn!(
                    request_id = %reservation_id,
                    attempt,
                    error = %error,
                    "settlement failed transiently; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(3);
                attempt += 1;
            }
        }
    }
}

/// One settle transaction: consume the reservation, append the metering row and
/// its walk ledger, and debit the prepaid balance — or discover that a previous
/// attempt already did all three.
///
/// # Why a retry cannot double-charge
///
/// Exactly-once still comes from `DELETE FROM usage_reservations ... RETURNING`
/// and nothing here weakens it. The debit runs in the same transaction as that
/// DELETE and only when it returned a row, so:
///
/// * if an earlier attempt committed, the reservation row is gone — this
///   attempt's DELETE returns nothing, the balance is never touched, and the
///   presence of the settled `usage_events` row (whose `request_id` migration
///   0001 made UNIQUE) turns the attempt into
///   [`SettleOutcome::AlreadySettled`]. That is what makes an ambiguous COMMIT
///   safe to retry: "did my COMMIT land?" is answered by the database rather
///   than guessed at;
/// * if an earlier attempt rolled back, the reservation row is back and this
///   attempt is the first one to consume it;
/// * two attempts can never both consume it, because the `DELETE` is atomic
///   and both settles serialize on the same per-user advisory lock admission
///   takes.
///
/// A duplicate-key error on the metering INSERT is therefore not a failure to
/// report but a race that has already been decided — the customer has exactly
/// one settled row — so it is reported as success and the orphaned reservation
/// is reclaimed. `credit_ledger_request_unique` (0002) sits underneath all of
/// this as an independent database-level refusal of a second `usage` debit for
/// one request.
async fn settle_once(
    pool: &PgPool,
    reservation_id: Uuid,
    api_key_id: Uuid,
    intent: &SettlementIntent,
) -> Result<SettleOutcome, sqlx::Error> {
    let record = intent.to_record()?;
    let reserved_cost_snapshot = intent.reserved_cost_snapshot()?;
    let input_tokens = checked_token_count(record.usage.prompt_tokens, "prompt_tokens")?;
    let cached_input_tokens =
        checked_token_count(record.usage.cached_input_tokens(), "cached_input_tokens")?;
    let output_tokens = checked_token_count(record.usage.completion_tokens, "completion_tokens")?;

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;
    // Same USER-keyed lock as admission so settlement serializes against
    // concurrent admissions and balance reads for this user.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::TEXT, 0))")
        .bind(intent.user_id.to_string())
        .execute(&mut *transaction)
        .await?;

    // Settle the reservation and recover what admission reserved in the
    // same statement; a missing row means it was already settled or expired.
    let reserved_cost_usd = sqlx::query_scalar::<_, Decimal>(
        "DELETE FROM usage_reservations WHERE id = $1 AND api_key_id = $2 RETURNING reserved_cost_usd",
    )
    .bind(reservation_id)
    .bind(api_key_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(reserved_cost_usd) = reserved_cost_usd else {
        // Nothing to consume. Ask the ledger which of the two reasons it is:
        // a settled row means an earlier attempt won (success), no settled row
        // means the reservation was lost without ever being billed (an error
        // worth surfacing, and the case the intent-before-settle ordering
        // exists to make impossible).
        let already_settled =
            sqlx::query_scalar::<_, i32>("SELECT 1 FROM usage_events WHERE request_id = $1")
                .bind(reservation_id)
                .fetch_optional(&mut *transaction)
                .await?
                .is_some();
        transaction.rollback().await?;
        return if already_settled {
            Ok(SettleOutcome::AlreadySettled)
        } else {
            Err(sqlx::Error::Protocol(
                "usage reservation is missing and no settled row exists for it".to_owned(),
            ))
        };
    };

    // Telemetry derived at settle from the candidate's cost-basis rates and
    // the buffered walk attempts (migration 0004). COGS is priced from the
    // same usage the customer is billed on, so an unmetered row settled at
    // the conservative estimate keeps a comparable basis instead of reading
    // as pure margin.
    let record = &record;
    let telemetry = &record.telemetry;
    let cost_basis_usd = telemetry.cost_basis(record);
    // The losing attempts, summed — and flagged when that sum is only a lower
    // bound. The sum used to be a `filter_map(...).sum()`, which silently
    // dropped every attempt whose COGS was unknown and reported the remainder
    // as if it were the total: three burnt upstream calls of which two were
    // never metered settled as "the attempts cost what the one metered attempt
    // cost". See migration 0007 on why a partial sum must never present as a
    // total.
    let attempts_cogs = AttemptCogs::summarize(&record.attempts);
    let finish_reason_source = telemetry.finish_reason.as_ref().map(|_| "synthetic");
    let attempt_count = if record.attempts.is_empty() {
        None
    } else {
        Some(i16::try_from(record.attempts.len()).unwrap_or(i16::MAX))
    };
    let sell_rates_json = rates_snapshot(&telemetry.sell_rates);
    let basis_rates_json = telemetry.basis_rates.as_ref().map(rates_snapshot);

    let settled = sqlx::query(
        r#"
            INSERT INTO usage_events (
                request_id,
                api_key_id,
                tier,
                upstream_provider,
                upstream_model,
                input_tokens,
                cached_input_tokens,
                output_tokens,
                cost_usd,
                latency_ms,
                status,
                candidate_id,
                cost_basis_usd,
                attempts_cost_basis_usd,
                sell_rates,
                basis_rates,
                finish_reason,
                finish_reason_source,
                requested_max_tokens,
                stream,
                prompt_bytes,
                message_count,
                tool_count,
                task_signature,
                attempt_count,
                shape_ok,
                reserved_output_tokens,
                reserved_cost_usd,
                estimator_basis,
                attempts_cost_basis_complete,
                task_signature_scheme,
                tool_names_sha256,
                priority
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                $12, $13, $14, $15::JSONB, $16::JSONB, $17, $18, $19, $20, $21,
                $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33
            )
            "#,
    )
    .bind(reservation_id)
    .bind(api_key_id)
    .bind(&record.tier)
    .bind(&record.upstream_provider)
    .bind(&record.upstream_model)
    .bind(input_tokens)
    .bind(cached_input_tokens.min(input_tokens))
    .bind(output_tokens)
    .bind(record.cost_usd)
    .bind(record.latency_ms)
    .bind(record.status)
    .bind(telemetry.candidate_id.as_deref())
    .bind(cost_basis_usd)
    .bind(attempts_cogs.total)
    .bind(sell_rates_json)
    .bind(basis_rates_json)
    .bind(telemetry.finish_reason.as_deref())
    .bind(finish_reason_source)
    .bind(telemetry.requested_max_tokens)
    .bind(telemetry.stream)
    .bind(telemetry.prompt_bytes)
    .bind(telemetry.message_count)
    .bind(telemetry.tool_count)
    .bind(&intent.task_signature)
    .bind(attempt_count)
    .bind(telemetry.shape_ok)
    .bind(intent.reserved_output_tokens)
    .bind(reserved_cost_snapshot)
    .bind(ESTIMATOR_BASIS_COLD)
    .bind(attempts_cogs.complete)
    // Both NULL when replaying an intent persisted before migration 0007:
    // the payload predates the fields, and NULL is exactly "scheme 1, tool
    // digest not captured" (migration 0007). Guessing the current scheme for
    // a key this build did not compute would mislabel the segment.
    .bind(intent.task_signature_scheme)
    .bind(intent.tool_names_sha256.as_deref())
    .bind(telemetry.priority.map(Priority::as_str))
    .execute(&mut *transaction)
    .await;
    if let Err(error) = settled {
        if !is_unique_violation(&error) {
            return Err(error);
        }
        // The settled row for this request already exists, so the customer has
        // been billed exactly once and this attempt must not bill again. The
        // rollback puts the reservation back (it is the same transaction that
        // just consumed it), and reclaiming it is guarded on the settled row
        // actually being there.
        transaction.rollback().await?;
        discard_settled_reservation(pool, reservation_id, api_key_id).await?;
        return Ok(SettleOutcome::AlreadySettled);
    }

    // Attempts ride the settle transaction, inserted AFTER the event row so
    // the FK to the UNIQUE usage_events.request_id holds; exactly-once is
    // inherited from the reservation DELETE...RETURNING above.
    for attempt in &record.attempts {
        // Each dimension is bound independently: an unknown one is SQL NULL,
        // never 0. An attempt priced from the per-chunk output floor knows
        // nothing about the prompt it consumed, and writing 0 there would state
        // that a dispatched upstream call read no prompt.
        let attempt_input = attempt
            .tokens
            .input
            .map(|tokens| checked_token_count(tokens, "attempt_input_tokens"))
            .transpose()?;
        let attempt_cached = attempt
            .tokens
            .cached_input
            .map(|tokens| checked_token_count(tokens, "attempt_cached_input_tokens"))
            .transpose()?
            .map(|cached| cached.min(attempt_input.unwrap_or(cached)));
        let attempt_output = attempt
            .tokens
            .output
            .map(|tokens| checked_token_count(tokens, "attempt_output_tokens"))
            .transpose()?;
        sqlx::query(
            r#"
                INSERT INTO request_attempts (
                    request_id,
                    api_key_id,
                    user_id,
                    ts,
                    attempt_no,
                    candidate_id,
                    upstream_provider,
                    upstream_model,
                    outcome,
                    served,
                    latency_ms,
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    tokens_estimated,
                    cost_basis_usd,
                    finish_reason,
                    validator_kind
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
                "#,
        )
        .bind(reservation_id)
        .bind(api_key_id)
        .bind(intent.user_id)
        .bind(attempt.started_at)
        .bind(attempt.attempt_no)
        .bind(&attempt.candidate_id)
        .bind(&attempt.upstream_provider)
        .bind(&attempt.upstream_model)
        .bind(&attempt.outcome)
        .bind(attempt.served)
        .bind(attempt.latency_ms)
        .bind(attempt_input)
        .bind(attempt_cached)
        .bind(attempt_output)
        .bind(attempt.tokens_estimated)
        .bind(attempt.cost_basis_usd)
        .bind(attempt.finish_reason.as_deref())
        .bind(attempt.validator_kind.as_deref())
        .execute(&mut *transaction)
        .await?;
    }

    // Debit the prepaid balance in the same transaction that settles the
    // reservation; the unique request_id and the settle-exactly-once
    // reservation make the debit idempotent. The debit is clamped to what
    // admission reserved (and thus verified against the balance), so actual
    // usage exceeding the reserved output bound can never overdraw. The
    // balance is only touched when credits gate admission — cap-only
    // deployments record usage_events for metering without moving money.
    // Zero-cost debits write no ledger row (the ledger forbids zero amounts).
    if intent.require_credits {
        let debit = record.cost_usd.min(reserved_cost_usd);
        if debit > Decimal::ZERO {
            let balance_after = sqlx::query_scalar::<_, Decimal>(
                r#"
                    UPDATE users
                    SET credit_balance_usd = credit_balance_usd - $2
                    WHERE id = $1
                    RETURNING credit_balance_usd
                    "#,
            )
            .bind(intent.user_id)
            .bind(debit)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| {
                sqlx::Error::Protocol("usage settlement found no owning user".to_owned())
            })?;
            sqlx::query(
                r#"
                    INSERT INTO credit_ledger (
                        user_id,
                        entry_type,
                        amount_usd,
                        balance_after_usd,
                        request_id
                    )
                    VALUES ($1, 'usage', $2, $3, $4)
                    "#,
            )
            .bind(intent.user_id)
            .bind(-debit)
            .bind(balance_after)
            .bind(reservation_id)
            .execute(&mut *transaction)
            .await?;
        }
    }
    transaction.commit().await?;
    Ok(SettleOutcome::Settled)
}

/// Reclaim a reservation whose request is already settled.
///
/// The `EXISTS` guard is the whole point: this can only ever remove a
/// reservation the ledger proves was settled, so a bug here cannot release an
/// unbilled reservation.
async fn discard_settled_reservation(
    pool: &PgPool,
    reservation_id: Uuid,
    api_key_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        DELETE FROM usage_reservations
        WHERE id = $1
          AND api_key_id = $2
          AND EXISTS (SELECT 1 FROM usage_events WHERE request_id = $1)
        "#,
    )
    .bind(reservation_id)
    .bind(api_key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stamp a failed settle on the reservation row and quarantine it once the
/// budget is spent. Returns whether the row is now quarantined, or `None` when
/// the reservation is already gone.
///
/// Best-effort by nature — it runs precisely when the database is misbehaving —
/// so callers treat its own failure as "unknown" rather than propagating it.
async fn record_settle_failure(
    pool: &PgPool,
    reservation_id: Uuid,
    error: &sqlx::Error,
) -> Result<Option<bool>, sqlx::Error> {
    let detail: String = error.to_string().chars().take(500).collect();
    sqlx::query_scalar::<_, bool>(
        r#"
        UPDATE usage_reservations
        SET settle_attempts = settle_attempts + 1,
            last_settle_error = $2,
            quarantined_at = CASE
                WHEN settlement_intent IS NULL THEN NULL
                WHEN quarantined_at IS NOT NULL THEN quarantined_at
                WHEN settle_attempts + 1 >= $3 THEN NOW()
                ELSE NULL
            END
        WHERE id = $1
        RETURNING quarantined_at IS NOT NULL
        "#,
    )
    .bind(reservation_id)
    .bind(detail)
    .bind(MAX_SETTLE_ATTEMPTS)
    .fetch_optional(pool)
    .await
}

/// Take a reservation out of the automatic path with a stated reason.
async fn quarantine_settlement(
    pool: &PgPool,
    reservation_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    let detail: String = reason.chars().take(500).collect();
    sqlx::query(
        r#"
        UPDATE usage_reservations
        SET quarantined_at = COALESCE(quarantined_at, NOW()),
            last_settle_error = $2
        WHERE id = $1 AND settlement_intent IS NOT NULL
        "#,
    )
    .bind(reservation_id)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a settle failure is worth retrying at all.
///
/// Fails closed: anything not positively recognised as transient is treated as
/// permanent, which returns the request immediately and leaves the durable
/// intent for an operator instead of spending the budget on an error that
/// cannot clear. Conversion faults, CHECK violations and trigger rejections all
/// land here, and they should — retrying them only delays the customer.
fn is_transient(error: &sqlx::Error) -> bool {
    match error {
        // The pool could not hand out a connection in time, or the connection
        // died under the statement. Both clear on their own.
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => true,
        // SQLSTATEs that describe a condition of the moment rather than of the
        // statement: serialization_failure / deadlock_detected (40001, 40P01),
        // lock_not_available from the 5s `SET LOCAL lock_timeout` waiting on
        // the per-user advisory lock (55P03), query_canceled and the shutdown
        // family (57014, 57P01-57P03), resource exhaustion (53200, 53300), and
        // the whole class-08 connection-exception family.
        sqlx::Error::Database(database) => database.code().is_some_and(|code| {
            matches!(
                code.as_ref(),
                "40001"
                    | "40P01"
                    | "55P03"
                    | "57014"
                    | "57P01"
                    | "57P02"
                    | "57P03"
                    | "53300"
                    | "53200"
            ) || code.starts_with("08")
        }),
        _ => false,
    }
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.is_unique_violation())
}

/// What one [`recover_owed_settlements`] pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SettlementRecovery {
    /// Owed settlements this pass turned into a settled row and a debit.
    pub settled: u64,
    /// Owed settlements whose request turned out to be settled already — the
    /// ambiguous-COMMIT case. The orphaned reservation is now reclaimed.
    pub already_settled: u64,
    /// Owed settlements that failed again and are still queued.
    pub failed: u64,
    /// Owed settlements handed to an operator instead of being retried again.
    pub quarantined: u64,
}

/// Replay settlements that were recorded as owed and never committed.
///
/// This is the durable backstop behind the bounded in-request retry: whatever
/// killed the original settle — a crashed process, an outage longer than a
/// request, a connection that died at COMMIT — the payload is still on the
/// reservation row and this replays it through the same [`settle_once`] the
/// request path uses. Because it is the same code and the same stored payload,
/// a recovered settle charges exactly what the original would have charged and
/// can never charge more.
///
/// Only intents older than [`SETTLEMENT_RECOVERY_GRACE`] are considered, so
/// this cannot collide with a request still working through its own retries;
/// and even a collision would be harmless, since both paths serialize on the
/// per-user advisory lock and the loser sees `AlreadySettled`.
pub async fn recover_owed_settlements(
    pool: &PgPool,
    limit: i64,
) -> Result<SettlementRecovery, sqlx::Error> {
    let grace_seconds = i64::try_from(SETTLEMENT_RECOVERY_GRACE.as_secs()).unwrap_or(i64::MAX);
    let owed = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, api_key_id, settlement_intent::TEXT
        FROM usage_reservations
        WHERE settlement_intent IS NOT NULL
          AND quarantined_at IS NULL
          AND settlement_intent_at <= NOW() - ($2 * INTERVAL '1 second')
        ORDER BY settlement_intent_at
        LIMIT $1
        "#,
    )
    .bind(limit.max(0))
    .bind(grace_seconds)
    .fetch_all(pool)
    .await?;

    let mut summary = SettlementRecovery::default();
    for (reservation_id, api_key_id, payload) in owed {
        let intent = match serde_json::from_str::<SettlementIntent>(&payload) {
            Ok(intent) if intent.version == SETTLEMENT_INTENT_VERSION => intent,
            Ok(intent) => {
                // A payload written by a build that knew a different shape.
                // Guessing at it would guess at a charge.
                quarantine_settlement(
                    pool,
                    reservation_id,
                    &format!("unsupported settlement payload version {}", intent.version),
                )
                .await?;
                summary.quarantined += 1;
                continue;
            }
            Err(error) => {
                quarantine_settlement(
                    pool,
                    reservation_id,
                    &format!("settlement payload is unreadable: {error}"),
                )
                .await?;
                summary.quarantined += 1;
                continue;
            }
        };
        match settle_once(pool, reservation_id, api_key_id, &intent).await {
            Ok(SettleOutcome::Settled) => {
                tracing::info!(
                    request_id = %reservation_id,
                    "owed settlement recovered"
                );
                summary.settled += 1;
            }
            Ok(SettleOutcome::AlreadySettled) => summary.already_settled += 1,
            Err(error) => {
                let quarantined = record_settle_failure(pool, reservation_id, &error)
                    .await
                    .unwrap_or(None)
                    .unwrap_or(false);
                if quarantined {
                    tracing::error!(
                        request_id = %reservation_id,
                        error = %error,
                        "owed settlement quarantined for reconciliation"
                    );
                    summary.quarantined += 1;
                } else {
                    summary.failed += 1;
                }
            }
        }
    }
    Ok(summary)
}

/// A settlement that could not be recorded automatically and is waiting on an
/// operator. Read by `zerorouter admin owed-settlements`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct QuarantinedSettlement {
    /// Equal to the `usage_events.request_id` this settlement would have
    /// written, so the ledger and the quarantine share one key.
    pub request_id: Uuid,
    pub api_key_id: Uuid,
    pub quarantined_at: DateTime<Utc>,
    pub settle_attempts: i32,
    /// The admission-verified ceiling; the recovered debit can never exceed it.
    pub reserved_cost_usd: Decimal,
    /// What the stored payload says the customer owes, when it is readable.
    pub owed_cost_usd: Option<Decimal>,
    pub last_settle_error: Option<String>,
}

/// Every settlement currently parked for reconciliation, oldest first.
pub async fn quarantined_settlements(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<QuarantinedSettlement>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            DateTime<Utc>,
            i32,
            Decimal,
            Option<Decimal>,
            Option<String>,
        ),
    >(
        r#"
        SELECT
            id,
            api_key_id,
            quarantined_at,
            settle_attempts,
            reserved_cost_usd,
            -- Pattern-guarded so an unreadable payload cannot fail the whole
            -- reconciliation query; the row still has to be listed.
            CASE
                WHEN settlement_intent->>'cost_usd' ~ '^[0-9]+(\.[0-9]+)?$'
                    THEN (settlement_intent->>'cost_usd')::NUMERIC
            END,
            last_settle_error
        FROM usage_reservations
        WHERE quarantined_at IS NOT NULL
        ORDER BY quarantined_at
        LIMIT $1
        "#,
    )
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                request_id,
                api_key_id,
                quarantined_at,
                settle_attempts,
                reserved_cost_usd,
                owed_cost_usd,
                last_settle_error,
            )| QuarantinedSettlement {
                request_id,
                api_key_id,
                quarantined_at,
                settle_attempts,
                reserved_cost_usd,
                owed_cost_usd,
                last_settle_error,
            },
        )
        .collect())
}

/// The durable settle payload: everything a settle transaction needs, in a form
/// that outlives the process that built it.
///
/// Money is carried as exact decimal STRINGS, never JSON numbers. A JSON number
/// round-trips through a float, and this payload is replayed to move a balance;
/// a string reproduces the `Decimal` exactly and stays readable in `psql`.
/// Everything else here is the same metadata already written to `usage_events`
/// and `request_attempts` — no prompt content — so persisting it changes
/// nothing about what ZeroRouter retains.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SettlementIntent {
    version: u8,
    user_id: Uuid,
    require_credits: bool,
    task_signature: String,
    /// Signature provenance (migration 0007). `#[serde(default)]` rather than a
    /// payload-version bump: an intent written by a pre-0007 build is still
    /// perfectly replayable, and `None` is the truthful reading of it — scheme
    /// 1, tool digest not captured — where a version bump would quarantine a
    /// recoverable charge over telemetry it does not need.
    #[serde(default)]
    task_signature_scheme: Option<i16>,
    #[serde(default)]
    tool_names_sha256: Option<String>,
    reserved_output_tokens: i32,
    /// The reservation's admission-verified cost ceiling, snapshotted onto the
    /// settled row. The clamp itself still reads the live value returned by the
    /// settle `DELETE ... RETURNING`, never this copy.
    reserved_cost_usd: String,
    tier: String,
    upstream_provider: String,
    upstream_model: String,
    usage: UsagePayload,
    cost_usd: String,
    latency_ms: i32,
    status: i16,
    telemetry: TelemetryPayload,
    attempts: Vec<AttemptPayload>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct UsagePayload {
    prompt_tokens: u64,
    cached_input_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct RatesPayload {
    input_per_mtok: Option<f64>,
    cached_input_per_mtok: Option<f64>,
    output_per_mtok: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TelemetryPayload {
    requested_max_tokens: i32,
    stream: bool,
    prompt_bytes: i64,
    message_count: i32,
    tool_count: i32,
    candidate_id: Option<String>,
    basis_rates: Option<RatesPayload>,
    sell_rates: RatesPayload,
    finish_reason: Option<String>,
    shape_ok: Option<bool>,
    /// `#[serde(default)]` keeps pre-knob intents replayable; they settle
    /// with a NULL priority, which is the ledger's word for "predates the
    /// knob", never a guessed `balanced`.
    #[serde(default)]
    priority: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AttemptPayload {
    attempt_no: i16,
    started_at: DateTime<Utc>,
    candidate_id: String,
    upstream_provider: String,
    upstream_model: String,
    outcome: String,
    served: bool,
    latency_ms: i32,
    /// Per-dimension so an intent can carry "output floor known, prompt
    /// unknown" — the shape an abandoned stream produces. `#[serde(default)]`
    /// keeps pre-0007 intents (which carried a whole-usage `usage` object)
    /// replayable: they lose the attempt's token detail, not the charge.
    #[serde(default)]
    tokens: AttemptTokensPayload,
    tokens_estimated: bool,
    cost_basis_usd: Option<String>,
    finish_reason: Option<String>,
    validator_kind: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct AttemptTokensPayload {
    input: Option<u64>,
    cached_input: Option<u64>,
    output: Option<u64>,
}

impl SettlementIntent {
    fn new(session: &UsageSession, record: &UsageRecord) -> Self {
        Self {
            version: SETTLEMENT_INTENT_VERSION,
            user_id: session.user_id,
            require_credits: session.require_credits,
            task_signature: session.task_signature.hex.clone(),
            task_signature_scheme: Some(session.task_signature.scheme),
            tool_names_sha256: Some(session.task_signature.tool_names_sha256.clone()),
            reserved_output_tokens: session.reserved_output_tokens,
            reserved_cost_usd: session.reserved_cost_usd.to_string(),
            tier: record.tier.clone(),
            upstream_provider: record.upstream_provider.clone(),
            upstream_model: record.upstream_model.clone(),
            usage: UsagePayload::new(record.usage),
            cost_usd: record.cost_usd.to_string(),
            latency_ms: record.latency_ms,
            status: record.status,
            telemetry: TelemetryPayload {
                requested_max_tokens: record.telemetry.requested_max_tokens,
                stream: record.telemetry.stream,
                prompt_bytes: record.telemetry.prompt_bytes,
                message_count: record.telemetry.message_count,
                tool_count: record.telemetry.tool_count,
                candidate_id: record.telemetry.candidate_id.clone(),
                basis_rates: record.telemetry.basis_rates.map(RatesPayload::new),
                sell_rates: RatesPayload::new(record.telemetry.sell_rates),
                finish_reason: record.telemetry.finish_reason.clone(),
                shape_ok: record.telemetry.shape_ok,
                priority: record.telemetry.priority.map(|priority| priority.as_str().to_owned()),
            },
            attempts: record
                .attempts
                .iter()
                .map(|attempt| AttemptPayload {
                    attempt_no: attempt.attempt_no,
                    started_at: attempt.started_at,
                    candidate_id: attempt.candidate_id.clone(),
                    upstream_provider: attempt.upstream_provider.clone(),
                    upstream_model: attempt.upstream_model.clone(),
                    outcome: attempt.outcome.clone(),
                    served: attempt.served,
                    latency_ms: attempt.latency_ms,
                    tokens: AttemptTokensPayload {
                        input: attempt.tokens.input,
                        cached_input: attempt.tokens.cached_input,
                        output: attempt.tokens.output,
                    },
                    tokens_estimated: attempt.tokens_estimated,
                    cost_basis_usd: attempt.cost_basis_usd.map(|cost| cost.to_string()),
                    finish_reason: attempt.finish_reason.clone(),
                    validator_kind: attempt.validator_kind.clone(),
                })
                .collect(),
        }
    }

    /// Rebuild the in-memory record the settle SQL binds from. A decimal that
    /// will not parse is a permanent failure, not a retryable one: it means the
    /// payload is corrupt, and the row belongs in quarantine rather than in a
    /// retry loop.
    fn to_record(&self) -> Result<UsageRecord, sqlx::Error> {
        let mut attempts = Vec::with_capacity(self.attempts.len());
        for attempt in &self.attempts {
            attempts.push(AttemptRecord {
                attempt_no: attempt.attempt_no,
                started_at: attempt.started_at,
                candidate_id: attempt.candidate_id.clone(),
                upstream_provider: attempt.upstream_provider.clone(),
                upstream_model: attempt.upstream_model.clone(),
                outcome: attempt.outcome.clone(),
                served: attempt.served,
                latency_ms: attempt.latency_ms,
                tokens: AttemptTokens {
                    input: attempt.tokens.input,
                    cached_input: attempt.tokens.cached_input,
                    output: attempt.tokens.output,
                },
                tokens_estimated: attempt.tokens_estimated,
                cost_basis_usd: attempt
                    .cost_basis_usd
                    .as_deref()
                    .map(|cost| settlement_decimal(cost, "attempt cost_basis_usd"))
                    .transpose()?,
                finish_reason: attempt.finish_reason.clone(),
                validator_kind: attempt.validator_kind.clone(),
            });
        }
        Ok(UsageRecord {
            tier: self.tier.clone(),
            upstream_provider: self.upstream_provider.clone(),
            upstream_model: self.upstream_model.clone(),
            usage: self.usage.to_usage(),
            cost_usd: settlement_decimal(&self.cost_usd, "cost_usd")?,
            latency_ms: self.latency_ms,
            status: self.status,
            telemetry: RequestTelemetry {
                requested_max_tokens: self.telemetry.requested_max_tokens,
                stream: self.telemetry.stream,
                prompt_bytes: self.telemetry.prompt_bytes,
                message_count: self.telemetry.message_count,
                tool_count: self.telemetry.tool_count,
                candidate_id: self.telemetry.candidate_id.clone(),
                basis_rates: self.telemetry.basis_rates.map(RatesPayload::to_rates),
                sell_rates: self.telemetry.sell_rates.to_rates(),
                finish_reason: self.telemetry.finish_reason.clone(),
                shape_ok: self.telemetry.shape_ok,
                priority: self
                    .telemetry
                    .priority
                    .as_deref()
                    .and_then(Priority::from_keyword),
            },
            attempts,
        })
    }

    fn reserved_cost_snapshot(&self) -> Result<Decimal, sqlx::Error> {
        settlement_decimal(&self.reserved_cost_usd, "reserved_cost_usd")
    }
}

impl UsagePayload {
    fn new(usage: OpenAiUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            cached_input_tokens: usage.cached_input_tokens(),
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }
    }

    fn to_usage(self) -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            prompt_tokens_details: (self.cached_input_tokens > 0).then_some(PromptTokenDetails {
                cached_tokens: self.cached_input_tokens,
            }),
        }
    }
}

impl RatesPayload {
    fn new(rates: ModelRates) -> Self {
        Self {
            input_per_mtok: rates.input_per_mtok,
            cached_input_per_mtok: rates.cached_input_per_mtok,
            output_per_mtok: rates.output_per_mtok,
        }
    }

    fn to_rates(self) -> ModelRates {
        ModelRates {
            input_per_mtok: self.input_per_mtok,
            cached_input_per_mtok: self.cached_input_per_mtok,
            output_per_mtok: self.output_per_mtok,
        }
    }
}

fn settlement_decimal(value: &str, field: &'static str) -> Result<Decimal, sqlx::Error> {
    Decimal::from_str(value).map_err(|_| {
        sqlx::Error::Protocol(format!(
            "settlement payload field {field} is not a decimal number"
        ))
    })
}

/// Whether one spend ceiling still admits `additional` on top of what is
/// already settled and reserved against it.
///
/// Two conditions, both inherited verbatim from B0's single-scope check: a
/// ceiling already reached exactly admits nothing (so a zero-cost reservation
/// cannot slip through an exhausted cap), and the projection must land at or
/// below the ceiling.
fn spend_within_cap(
    settled: Decimal,
    reserved: Decimal,
    additional: Decimal,
    cap: Decimal,
) -> bool {
    let committed = settled + reserved;
    committed < cap && committed + additional <= cap
}

/// Velocity counterpart of [`spend_within_cap`]. Saturating on the committed
/// side and checked on the projection, so an overflowing projection is refused
/// rather than wrapping into an admission.
fn velocity_within_cap(settled: i64, reserved: i64, additional: i64, cap: i64) -> bool {
    settled.saturating_add(reserved) < cap
        && settled
            .checked_add(reserved)
            .and_then(|committed| committed.checked_add(additional))
            .is_some_and(|projected| projected <= cap)
}

/// The most live (non-disabled) keys one user may hold at once.
pub const MAX_ACTIVE_KEYS_PER_USER: i64 = 20;
/// The most keys one user may CREATE inside [`KEY_CREATION_WINDOW_HOURS`],
/// counting keys they have since disabled.
pub const MAX_KEYS_CREATED_PER_WINDOW: i64 = 20;
/// Trailing window the creation throttle counts over.
pub const KEY_CREATION_WINDOW_HOURS: i64 = 24;

/// Whether a user may mint another API key.
pub enum KeyMintAdmission {
    Allowed,
    LimitReached,
}

/// Decide whether `user_id` may mint another API key, inside the caller's
/// transaction.
///
/// Two limits, both required:
///
/// 1. **Active-key cap** — at most [`MAX_ACTIVE_KEYS_PER_USER`] non-disabled
///    keys. This is the original limit and it counts live keys only.
/// 2. **Creation throttle** — at most [`MAX_KEYS_CREATED_PER_WINDOW`] keys
///    *created* in the trailing [`KEY_CREATION_WINDOW_HOURS`], counting
///    disabled ones. Limit 1 alone is resettable: disabling a key is a flag
///    flip, so a user could exhaust a key's quota, disable it, mint a fresh one
///    and repeat without bound. Counting creations over a window makes the
///    churn itself the scarce resource.
///
/// The caller must already hold a serializing lock on the owning user (both
/// mint paths take `SELECT ... FROM users WHERE id = $1 FOR UPDATE`), otherwise
/// two concurrent mints can each observe a count below the limit.
///
/// Scope note: this governs the two SELF-SERVICE mint paths only —
/// `portal::create_key` and the device-claim mint in [`crate::device`]. The
/// `admin mint-key` CLI is operator-only (it needs database credentials, not a
/// session) and is deliberately left exempt, so an operator can always issue a
/// key for a user who has hit the throttle.
pub async fn admit_key_mint(
    connection: &mut PgConnection,
    user_id: Uuid,
) -> Result<KeyMintAdmission, sqlx::Error> {
    let (active_keys, recently_created_keys) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE NOT disabled) AS active_keys,
            COUNT(*) FILTER (
                WHERE created_at > NOW() - ($2 * INTERVAL '1 hour')
            ) AS recently_created_keys
        FROM api_keys
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .bind(KEY_CREATION_WINDOW_HOURS)
    .fetch_one(connection)
    .await?;
    if active_keys >= MAX_ACTIVE_KEYS_PER_USER
        || recently_created_keys >= MAX_KEYS_CREATED_PER_WINDOW
    {
        return Ok(KeyMintAdmission::LimitReached);
    }
    Ok(KeyMintAdmission::Allowed)
}

fn checked_token_count(value: u64, field: &'static str) -> Result<i32, sqlx::Error> {
    i32::try_from(value)
        .map_err(|_| sqlx::Error::Protocol(format!("{field} exceeds the database integer range")))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).unwrap_or_default();
    if value.trim().is_empty() {
        bail!("{name} is required")
    }
    Ok(value)
}

pub fn parse_decimal(value: &str, field: &str) -> Result<Decimal> {
    Decimal::from_str(value).with_context(|| format!("{field} must be a decimal number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_fail_instead_of_saturating() {
        assert_eq!(
            checked_token_count(i32::MAX as u64, "tokens").expect("i32 max should fit"),
            i32::MAX
        );
        assert!(checked_token_count(i32::MAX as u64 + 1, "tokens").is_err());
    }

    #[test]
    fn spend_cap_refuses_at_the_ceiling_and_past_the_projection() {
        let cap = Decimal::from(20);
        assert!(spend_within_cap(
            Decimal::from(5),
            Decimal::from(5),
            Decimal::from(10),
            cap
        ));
        // The projection may land exactly on the ceiling.
        assert!(spend_within_cap(
            Decimal::from(19),
            Decimal::ZERO,
            Decimal::ONE,
            cap
        ));
        // ...but one cent past it is refused.
        assert!(!spend_within_cap(
            Decimal::from(19),
            Decimal::ZERO,
            Decimal::from(2),
            cap
        ));
        // A ceiling already reached admits nothing, not even a zero-cost
        // reservation: that is what stops a free request from being smuggled
        // past an exhausted cap.
        assert!(!spend_within_cap(
            Decimal::from(20),
            Decimal::ZERO,
            Decimal::ZERO,
            cap
        ));
        assert!(!spend_within_cap(
            Decimal::from(15),
            Decimal::from(5),
            Decimal::ZERO,
            cap
        ));
    }

    #[test]
    fn velocity_cap_refuses_at_the_ceiling_and_on_overflow() {
        assert!(velocity_within_cap(400, 400, 200, 1_000));
        assert!(!velocity_within_cap(400, 400, 201, 1_000));
        assert!(!velocity_within_cap(1_000, 0, 0, 1_000));
        assert!(!velocity_within_cap(600, 400, 0, 1_000));
        // An overflowing projection must fail closed rather than wrap.
        assert!(!velocity_within_cap(1, i64::MAX, 1, i64::MAX));
        assert!(!velocity_within_cap(0, 0, i64::MAX, 10));
    }

    #[test]
    fn key_creation_throttle_is_not_looser_than_the_active_cap() {
        // The throttle must be able to bite before the active-key cap does,
        // otherwise disable-and-remint stays free: a user who disables every
        // key they mint never accumulates active keys.
        const { assert!(MAX_KEYS_CREATED_PER_WINDOW <= MAX_ACTIVE_KEYS_PER_USER) };
        const { assert!(KEY_CREATION_WINDOW_HOURS > 0) };
    }
}

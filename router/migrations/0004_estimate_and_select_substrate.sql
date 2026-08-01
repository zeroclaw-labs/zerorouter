-- 0004_estimate_and_select_substrate.sql
--
-- Telemetry substrate for the estimate-and-select engine
-- (docs/DESIGN-estimate-and-select.md). Stage-1 scope: request-shape
-- features, selection/reservation provenance, outcome AND validator-outcome
-- labels, COGS and rate/reserved-cost snapshots on usage_events; the
-- append-only per-attempt ledger; one estimator read index; and the per-key
-- priority default. Tables whose shape is still stage-gated (validators
-- registry, fee bookkeeping, savings/reference columns, estimator cell
-- caches) are deliberately NOT created here — they arrive with the stages
-- that define them (design doc: Data model, migrations 0005/0006/0007, and
-- Rollout).
--
-- REGISTRATION REQUIRED: sqlx migrations are a hardcoded include_str! vec —
-- this file does nothing until db.rs (db.rs:103-131) gains:
--
--   Migration::new(
--       4,
--       Cow::Borrowed("estimate and select substrate"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0004_estimate_and_select_substrate.sql")),
--       false,
--   ),
--
-- Migrations auto-run at startup of both `serve` (main.rs:66) and `admin`
-- (admin.rs:72-73); no deploy choreography beyond shipping the image.
--
-- APPEND-ONLY MECHANICS: usage_events rejects row UPDATE/DELETE/TRUNCATE via
-- triggers (0001_b0_schema.sql:106-124). ALTER TABLE ... ADD COLUMN is DDL
-- and does not fire row triggers. Every column added here is nullable and is
-- written AT INSERT TIME ONLY — there is no backfill and there must never be
-- one. Pre-0004 rows read NULL = "not captured"; every CHECK below passes
-- NULL, so the validation scan ADD CONSTRAINT performs succeeds trivially.
-- (That scan still holds an ACCESS EXCLUSIVE lock for its duration —
-- negligible at today's table size; batch future constraint additions
-- consciously.)

-- ============================================================================
-- 1. usage_events: request-shape features, selection provenance, and outcome
--    telemetry. The metering ledger doubles as the estimator's training log:
--    every feature the estimator consumes and every decision the router made
--    is persisted on the same append-only row as the money.
-- ============================================================================

ALTER TABLE usage_events
    -- Winning TierCandidate.id (config.rs:29-36), e.g.
    -- 'deepinfra/deepseek-ai/DeepSeek-V4-Flash'. NULL on pre-0004 rows and on
    -- sentinel rows where no candidate was selected: the 'fallback-chain'
    -- release paths (api.rs:270-336, api.rs:839-842) and
    -- 'none'/'client-disconnected' (api.rs:467-470). upstream_provider /
    -- upstream_model keep carrying the sentinel strings; this column is the
    -- machine-joinable key.
    ADD COLUMN candidate_id TEXT,

    -- Provider-rate COGS of the SERVED attempt: usage_cost(candidate.rates,
    -- usage) — the static cost-basis table config validates (config.rs:29-36,
    -- 202-226) and nothing used at runtime until now. NULL when usage was
    -- unreported or the row is a sentinel. Makes both invisibly
    -- negative-margin rungs visible in one query: opus (basis
    -- 5.00/0.50/25.00, tiers.toml:137-150, vs high-end sell 2.00/0.20/10.00,
    -- tiers.toml:106-109) and haiku's output dimension (basis 5.00,
    -- tiers.toml:86-89, vs balanced sell 3.48, tiers.toml:56-59).
    ADD COLUMN cost_basis_usd NUMERIC,

    -- Summed COGS of the NON-serving attempts of this request (ZeroRouter's
    -- money, never the customer's). Mirrors SUM(request_attempts.cost_basis_usd)
    -- so margin is a one-table expression:
    --   cost_usd - cost_basis_usd - attempts_cost_basis_usd.
    -- NULL until the router-owned walk records attempts (rollout Stage 2).
    ADD COLUMN attempts_cost_basis_usd NUMERIC,

    -- Rate snapshots at settle, as
    -- {"input_per_mtok": .., "cached_input_per_mtok": .., "output_per_mtok": ..}:
    -- the sell-rate table behind cost_usd and the served candidate's
    -- cost-basis table. tiers.toml is re-parsed per request (api.rs:179-181),
    -- so without a snapshot historical rows can never be re-priced or audited
    -- (integration map §3 gap 6). With them, every settled row is
    -- self-contained forever — the substrate for defensible savings math.
    ADD COLUMN sell_rates JSONB,
    ADD COLUMN basis_rates JSONB,

    -- Router-SYNTHESIZED finish reason (openai.rs:492-505): tool_calls beats
    -- length by design; content_filter is structurally unobservable through
    -- the pinned provider trait (map §3 gap 4). Treat as heuristic while
    -- finish_reason_source = 'synthetic'.
    ADD COLUMN finish_reason TEXT,

    -- 'synthetic' now; 'upstream' once StreamEvent/ChatResponse carry the
    -- real stop reason (map §5.8) — lets estimator training separate the
    -- cohorts instead of silently mixing a heuristic with ground truth.
    ADD COLUMN finish_reason_source TEXT,

    -- Request-shape features, RAW (not only hashed), so task signatures can
    -- be re-bucketed — or re-keyed to a pooled scheme — retroactively without
    -- a second data system. Prompt content itself is never persisted.
    ADD COLUMN requested_max_tokens INTEGER,   -- post-default value (api.rs:185; BASELINE_MAX_TOKENS = 4096)
    ADD COLUMN stream BOOLEAN,                 -- request.stream
    ADD COLUMN prompt_bytes BIGINT,            -- the byte-bound input measure (openai.rs:237-269)
    ADD COLUMN message_count INTEGER,          -- messages.len(); >= 1 by request validation
    ADD COLUMN tool_count INTEGER,             -- tools.len()

    -- First 16 hex chars of sha256 over (user_id ∥ sorted tool names ∥
    -- message-count bucket ∥ log2 prompt-bytes bucket ∥ stream flag ∥
    -- requested-max_tokens bucket): the estimator's segmentation key. Keyed
    -- per USER — not per API key — because keys are churnable at zero cost
    -- (disable_key flips a flag, portal.rs:350-369, and the active-key cap
    -- counts only non-disabled keys, portal.rs:283), so a per-key band could
    -- be reset by disable-and-remint. Gaming a user-scoped band only poisons
    -- the gamer's own estimates.
    ADD COLUMN task_signature CHAR(16),

    -- Resolved priority for this request. NULL = row predates the knob
    -- (rollout Stage 3a); once the knob ships the resolved value is always
    -- written, 'balanced' being the default.
    ADD COLUMN priority TEXT,

    -- Number of walk positions consumed (request_attempts rows for this
    -- request, dispatched and skipped), 1 for an untroubled request. NULL
    -- until attempts are recorded.
    ADD COLUMN attempt_count SMALLINT,

    -- Implicit shape-check outcome: completion present, output non-empty,
    -- every tool-call arguments parses as JSON, not truncated (per the
    -- synthesized finish_reason). Labels 100% of traffic for the success
    -- estimator from day one, before any customer declares a validator.
    -- Gates escalation only on non-streamed/buffered paths; on live streams
    -- and in 'balanced' mode it is a label only — it never changes routing.
    ADD COLUMN shape_ok BOOLEAN,

    -- Governing DECLARED validator for this request (NULL when none was
    -- declared; the implicit shape check is labeled by shape_ok, not here).
    -- Free-form TEXT by design: kinds grow over time and rows are immutable.
    ADD COLUMN validator_kind TEXT,

    -- Full sha256 (64 hex chars) of the canonical JSON of the governing
    -- declared validator spec — full-width, not a prefix, so the audit
    -- anchor for validator-gated billing cannot be collision-farmed. The
    -- registry row (migration 0006) stores the same hash; inline validators
    -- are hashed identically, so the anchor exists from the first inline
    -- request.
    ADD COLUMN validator_spec_sha256 CHAR(64),

    -- Outcome of the governing declared validator on the SERVED attempt:
    -- TRUE/FALSE only when a declared validator gated the request; NULL when
    -- none was declared. 'validated: false' rows are success-mode exhaustion
    -- (best attempt served, billed as actuals, never fee-eligible).
    ADD COLUMN validated BOOLEAN,

    -- Reservation provenance, copied into the ledger at settle because
    -- usage_reservations rows are DESTROYED by the settle DELETE...RETURNING
    -- (db.rs:335-344): the output-token bound admission reserved, the
    -- admission-verified sell-cost ceiling, and which sizing produced them.
    -- Closes the calibration loop one-table:
    --   clamp_loss = GREATEST(0, cost_usd - reserved_cost_usd)
    -- (meaningful on credit-gated rows; the automatic estimator revert
    -- triggers on clamp-loss DOLLARS per row / per segment / per user — a
    -- clamp-hit RATE is dilutable with filler traffic and is only a
    -- secondary signal).
    ADD COLUMN reserved_output_tokens INTEGER,
    ADD COLUMN reserved_cost_usd NUMERIC,

    -- 'cold'    = byte bound + max_tokens (today's behavior; the permanent
    --             fallback and the value on every Stage-1 row)
    -- 'learned' = estimator-sized reservation (rollout Stage 4; floored at
    --             0.25 x requested_max_tokens, tail-gated, and never applied
    --             to escalation-capable requests in Stage 4)
    -- 'quote'   = reserved for the gated quote stage — listed now so the enum
    --             needs no later constraint surgery; stamping it makes "no
    --             ungated quote ever shipped" provable per row.
    ADD COLUMN estimator_basis TEXT;

ALTER TABLE usage_events
    ADD CONSTRAINT usage_events_priority_is_known
        CHECK (priority IS NULL OR priority IN ('cost', 'balanced', 'success')),
    ADD CONSTRAINT usage_events_cost_basis_is_nonnegative
        CHECK (cost_basis_usd IS NULL OR cost_basis_usd >= 0),
    ADD CONSTRAINT usage_events_attempts_cost_basis_is_nonnegative
        CHECK (attempts_cost_basis_usd IS NULL OR attempts_cost_basis_usd >= 0),
    ADD CONSTRAINT usage_events_finish_reason_source_is_known
        CHECK (finish_reason_source IS NULL OR finish_reason_source IN ('synthetic', 'upstream')),
    ADD CONSTRAINT usage_events_task_signature_is_hex
        CHECK (task_signature IS NULL OR task_signature ~ '^[0-9a-f]{16}$'),
    ADD CONSTRAINT usage_events_validator_spec_sha256_is_hex
        CHECK (validator_spec_sha256 IS NULL OR validator_spec_sha256 ~ '^[0-9a-f]{64}$'),
    ADD CONSTRAINT usage_events_prompt_bytes_are_nonnegative
        CHECK (prompt_bytes IS NULL OR prompt_bytes >= 0),
    ADD CONSTRAINT usage_events_message_count_is_positive
        CHECK (message_count IS NULL OR message_count >= 1),
    ADD CONSTRAINT usage_events_tool_count_is_nonnegative
        CHECK (tool_count IS NULL OR tool_count >= 0),
    ADD CONSTRAINT usage_events_attempt_count_is_positive
        CHECK (attempt_count IS NULL OR attempt_count >= 1),
    ADD CONSTRAINT usage_events_reserved_output_is_nonnegative
        CHECK (reserved_output_tokens IS NULL OR reserved_output_tokens >= 0),
    ADD CONSTRAINT usage_events_reserved_cost_is_nonnegative
        CHECK (reserved_cost_usd IS NULL OR reserved_cost_usd >= 0),
    ADD CONSTRAINT usage_events_estimator_basis_is_known
        CHECK (estimator_basis IS NULL OR estimator_basis IN ('cold', 'learned', 'quote'));

COMMENT ON COLUMN usage_events.cost_basis_usd IS
    'Served-attempt COGS at the candidate cost-basis rate; cost_usd stays customer sell price (0001:104). Losing-attempt COGS lives in request_attempts / attempts_cost_basis_usd';
COMMENT ON COLUMN usage_events.finish_reason IS
    'Router-synthesized (tool_calls beats length; content_filter unobservable at the pin) while finish_reason_source = synthetic';
COMMENT ON COLUMN usage_events.task_signature IS
    'USER-scoped request-shape segment key (key churn resets nothing); raw shape features are persisted alongside so segments can be re-bucketed retroactively';
COMMENT ON COLUMN usage_events.shape_ok IS
    'Implicit shape-validator outcome on every request; the success estimator''s day-one training label. Label-only on live streams and in balanced mode';
COMMENT ON COLUMN usage_events.validated IS
    'Governing declared-validator outcome on the served attempt; NULL = no declared validator. Only validated = TRUE rows can ever be per-success fee-eligible';
COMMENT ON COLUMN usage_events.reserved_output_tokens IS
    'Output bound admission reserved; copied here at settle because the reservation row is deleted by settle (db.rs:335-344)';
COMMENT ON COLUMN usage_events.reserved_cost_usd IS
    'Admission-verified sell-cost ceiling (usage_reservations.reserved_cost_usd) snapshotted at settle; clamp_loss = GREATEST(0, cost_usd - reserved_cost_usd) is the calibration objective and the dollar-denominated gaming alarm';

-- Estimator read path: (signature, candidate) percentile scans over a
-- trailing window, executed by the background refresher — never on the
-- request path. Prefix scans on task_signature alone serve the
-- candidate-agnostic reservation-sizing percentiles. Partial: legacy rows
-- carry NULL signatures and are never scanned.
CREATE INDEX usage_events_signature_candidate_idx
    ON usage_events (task_signature, candidate_id, ts DESC)
    WHERE task_signature IS NOT NULL;

-- ============================================================================
-- 2. request_attempts: the per-candidate walk ledger — one row per walk
--    position of one request. Rows are buffered in memory during the walk and
--    inserted in the SAME transaction as the usage_events row at settle,
--    AFTER it (the FK holds; exactly-once is inherited from the reservation
--    DELETE...RETURNING, db.rs:335-344). A request that never settles
--    (crash; reservation expiry released un-billed, db.rs:186-188) records no
--    attempts — accepted: money is never wrong, only telemetry is lost, and
--    the planned settle-at-reserved expiry flip (map §4) shrinks that window.
--    Streamed requests can record attempts from Stage 1 (that walk is already
--    router-owned, api.rs:459-764); non-streaming rows arrive with the
--    Stage-2 unroll of ReliableModelProvider.
--
--    NOT a velocity-enforcement table: rows land at settle (minutes after
--    dispatch) with backdated ts, so a trailing-minute SUM here cannot see
--    in-flight walks and misses attempts precisely in the heavy multi-rung
--    walks a cap exists to police. Enforcement is the in-process per-key and
--    per-user minute buckets bumped at attempt DISPATCH time (design doc,
--    escalation loop); this table is the audit/backfill view. Any audit SUM
--    must filter WHERE NOT served — the served attempt's tokens are already
--    counted from usage_events by the admission query (db.rs:190-215).
-- ============================================================================

CREATE TABLE request_attempts (
    id BIGSERIAL PRIMARY KEY,
    -- usage_events.request_id is UNIQUE (0001:68), so this FK anchors every
    -- attempt to exactly one settled request.
    request_id UUID NOT NULL REFERENCES usage_events (request_id),
    -- Both denormalized so per-key velocity audits and per-USER abuse/gaming
    -- scans (attempts_cost_basis vs cost_usd ratios, escalation-budget
    -- accounting) need no join through the ledger. user_id, not api_key_id,
    -- is the abuse-monitoring key: keys are churnable, users are not.
    api_key_id UUID NOT NULL REFERENCES api_keys (id),
    user_id UUID NOT NULL REFERENCES users (id),
    -- Attempt START time, bound by the router from its in-memory record; the
    -- default only covers pathological paths (rows are inserted at settle,
    -- which can be minutes after the attempt began — see the header note on
    -- why this field must never drive velocity enforcement).
    ts TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Walk position, 1-based. Counts every position including skips; the
    -- MAX_ATTEMPTS ceiling counts dispatched attempts only.
    attempt_no SMALLINT NOT NULL,
    -- TierCandidate.id at walk time; provider/model denormalized because the
    -- catalog drifts and rows do not.
    candidate_id TEXT NOT NULL,
    upstream_provider TEXT NOT NULL,
    upstream_model TEXT NOT NULL,
    -- What happened to this attempt:
    --   'ok'                completed and passed the governing check (or none declared)
    --   'validation_failed' completed but failed the governing validator/shape check
    --   'upstream_error'    provider/transport failure
    --   'timeout'           per-attempt or shared-deadline (api.rs:47) expiry
    --   'rate_limited'      upstream 429 (feeds the health cooldown)
    --   'stream_error'      stream broke mid-delivery
    --   'aborted'           walk stopped externally (client disconnect, shutdown)
    --   'health_skipped'    not dispatched: health demotion skipped this rung
    --   'policy_skipped'    not dispatched: margin-ineligible for escalation
    --                       (candidate basis > owning-tier sell after a
    --                       validation failure), budget-inadmissible rung, or
    --                       per-user escalation budget exhausted — the audit
    --                       trail for "why did the walk not go higher"
    outcome TEXT NOT NULL,
    -- TRUE on the one attempt whose output the customer received — normally
    -- the 'ok' attempt, but also the best 'validation_failed' attempt when
    -- success-mode exhaustion returns the best effort unvalidated. The served
    -- attempt is the only one the customer is billed for.
    served BOOLEAN NOT NULL DEFAULT FALSE,
    latency_ms INTEGER NOT NULL,
    -- Token fields are NULL when the upstream reported no usage. When
    -- tokens_estimated is TRUE they derive from the per-chunk token_count
    -- lower bound ZR already requests and today ignores (api.rs:583), not
    -- from a provider usage report — a floor on abandoned-attempt COGS.
    input_tokens INTEGER,
    cached_input_tokens INTEGER,
    output_tokens INTEGER,
    tokens_estimated BOOLEAN NOT NULL DEFAULT FALSE,
    -- This attempt's COGS at the candidate cost-basis rate (config.rs:29-36).
    cost_basis_usd NUMERIC,
    -- Synthesized, same caveat as usage_events.finish_reason.
    finish_reason TEXT,
    -- Which check gated this attempt: 'shape' for the implicit check;
    -- customer kinds ('json_schema', 'assert', later 'judge') once declared
    -- validators ship. Free-form TEXT by design: kinds grow over time and
    -- rows are immutable.
    validator_kind TEXT,
    CONSTRAINT request_attempts_attempt_no_is_positive
        CHECK (attempt_no >= 1),
    CONSTRAINT request_attempts_outcome_is_known
        CHECK (outcome IN ('ok', 'validation_failed', 'upstream_error', 'timeout',
                           'rate_limited', 'stream_error', 'aborted',
                           'health_skipped', 'policy_skipped')),
    CONSTRAINT request_attempts_latency_is_nonnegative
        CHECK (latency_ms >= 0),
    CONSTRAINT request_attempts_tokens_are_nonnegative
        CHECK ((input_tokens IS NULL OR input_tokens >= 0)
               AND (cached_input_tokens IS NULL OR cached_input_tokens >= 0)
               AND (output_tokens IS NULL OR output_tokens >= 0)),
    CONSTRAINT request_attempts_cost_basis_is_nonnegative
        CHECK (cost_basis_usd IS NULL OR cost_basis_usd >= 0),
    CONSTRAINT request_attempts_unique_ordinal
        UNIQUE (request_id, attempt_no)
);

-- At most one served attempt per request.
CREATE UNIQUE INDEX request_attempts_one_served_per_request
    ON request_attempts (request_id)
    WHERE served;

-- Success/health estimation and per-candidate latency percentiles.
CREATE INDEX request_attempts_candidate_ts_idx
    ON request_attempts (candidate_id, ts DESC);

-- Per-key velocity AUDIT view (enforcement is the dispatch-time in-process
-- bucket; audit queries filter WHERE NOT served — see the table header).
CREATE INDEX request_attempts_key_ts_idx
    ON request_attempts (api_key_id, ts DESC);

-- Per-USER abuse/gaming scans (attempts_cost_basis vs cost_usd ratios,
-- escalation-budget reconciliation) — user-scoped so key churn cannot reset
-- an abuse history.
CREATE INDEX request_attempts_user_ts_idx
    ON request_attempts (user_id, ts DESC);

COMMENT ON TABLE request_attempts IS
    'Append-only per-candidate walk history; settles with its usage_events row in the same transaction. Attempts are ZeroRouter''s COGS story — the customer is billed the served attempt only, at tier sell rates. Audit view for velocity (dispatch-time in-process buckets enforce); rows are settle-time and ts is backdated';

-- Same append-only discipline as the ledgers it feeds (pattern of
-- 0001:106-124): attempts will eventually underwrite per-success fee audits,
-- so rows must be immutable. Own function so the exception names this table.
CREATE FUNCTION reject_request_attempts_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'request_attempts is append-only';
    RETURN NULL;
END;
$$;

CREATE TRIGGER request_attempts_reject_update_or_delete
    BEFORE UPDATE OR DELETE ON request_attempts
    FOR EACH ROW
    EXECUTE FUNCTION reject_request_attempts_mutation();

CREATE TRIGGER request_attempts_reject_truncate
    BEFORE TRUNCATE ON request_attempts
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_request_attempts_mutation();

-- ============================================================================
-- 3. api_keys: per-key priority default. The one deliberate step beyond pure
--    Stage-1 telemetry in this migration: the enum is fixed by the product
--    thesis, so the column carries no redesign risk, and landing it here
--    saves the knob stage a migration. Inert until Stage 3a threads it
--    through AuthenticatedKey (auth.rs:15-19; SELECT at auth.rs:67-77 — it
--    must be known BEFORE candidate ordering at api.rs:186, which runs ahead
--    of the admission SELECT). NULL means 'balanced'.
--
--    Note: the key-CREATION throttle that closes the disable-and-remint
--    loophole (portal.rs:283 counts only NOT-disabled keys) is a code-level
--    check over api_keys.created_at counting disabled keys too — no schema
--    change needed; it ships with rollout Stage 5a.
-- ============================================================================

ALTER TABLE api_keys
    ADD COLUMN default_priority TEXT;

ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_default_priority_is_known
        CHECK (default_priority IS NULL OR default_priority IN ('cost', 'balanced', 'success'));

COMMENT ON COLUMN api_keys.default_priority IS
    'Per-key default for the estimate-and-select priority knob; NULL = balanced. Request-level carriers (typed zerorouter.priority or model suffix) override';

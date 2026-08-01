-- 0007_ledger_honesty.sql
--
-- Three corrections to the estimate-and-select telemetry substrate
-- (0004_estimate_and_select_substrate.sql), all of the same kind: the ledger
-- was stating things it did not know. It is meant to be training data for a
-- cost/success estimator and the input to per-request margin
--   cost_usd - cost_basis_usd - attempts_cost_basis_usd
-- so a confidently wrong row is worse than an absent one — every downstream
-- decision inherits the error silently.
--
-- REGISTRATION REQUIRED (same as every migration here — sqlx runs a hardcoded
-- include_str! vec): this file does nothing until db.rs `migrate` gains
--
--   Migration::new(
--       7,
--       Cow::Borrowed("ledger honesty"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0007_ledger_honesty.sql")),
--       false,
--   ),
--
-- APPEND-ONLY MECHANICS: unchanged from 0004. usage_events rejects row
-- UPDATE/DELETE (0001:106-124); ADD COLUMN is DDL and does not fire row
-- triggers. Every column added here is nullable, written AT INSERT TIME ONLY,
-- and never backfilled: on a pre-0007 row NULL means "not captured", which is
-- the whole point of the exercise. Every CHECK below passes on existing rows
-- (each is either NULL-tolerant or predicated on a column that is NULL
-- everywhere until this migration lands), so the validation scan succeeds.
--
-- 0004 IS NOT EDITED. Its text is checksummed by the migrator, so amending its
-- comments would break every database that has already applied it. Where this
-- migration contradicts 0004's header, this file is the current truth.

ALTER TABLE usage_events
    -- Whether attempts_cost_basis_usd is a TOTAL or a LOWER BOUND.
    --
    -- 0004 described attempts_cost_basis_usd as "summed COGS of the
    -- NON-serving attempts", and the code summed only the attempts whose COGS
    -- was known, silently dropping the rest — so a request that burned three
    -- upstream calls of which two were never metered reported the cost of one
    -- as if it were the cost of three. "We do not know what they cost" was
    -- written as "they cost nothing".
    --
    --   TRUE  = every non-serving attempt's COGS came from an upstream usage
    --           report. The sum is the whole story.
    --   FALSE = at least one non-serving attempt reported no usage, or was
    --           priced from the per-chunk token_count floor
    --           (request_attempts.tokens_estimated). The sum is a LOWER BOUND;
    --           real COGS is at least this and unbounded above.
    --   NULL  = no attempts were recorded for the request at all
    --           (attempts_cost_basis_usd is NULL too), or the row predates
    --           this column.
    --
    -- Margin arithmetic must treat a FALSE row as a margin CEILING, not a
    -- margin. Estimator training over COGS should filter on TRUE.
    ADD COLUMN attempts_cost_basis_complete BOOLEAN,

    -- Which encoding produced task_signature (openai.rs TASK_SIGNATURE_SCHEME).
    --
    -- Scheme 1 (this column NULL) joined sorted tool names with ',' before
    -- hashing while tool-name validation only required a name to be non-empty,
    -- so one tool named 'a,b' and two tools named 'a' and 'b' produced the
    -- SAME key: two different request shapes in one segment. Scheme 2
    -- length-prefixes each name and carries the scheme number in the preimage,
    -- so a scheme-1 and a scheme-2 key for the same request cannot coincide by
    -- accident.
    --
    -- Signature VALUES are therefore discontinuous at this migration and are
    -- not comparable across schemes: a reader must group by
    -- (task_signature, task_signature_scheme), never by task_signature alone.
    -- Existing rows are left exactly as they were rather than being
    -- reinterpreted or recomputed. That is safe today because nothing consumes
    -- these keys yet — the estimator that will (design doc, Stage 4) has not
    -- shipped, the read index usage_events_signature_candidate_idx is scanned
    -- by nothing, and no routing, pricing or billing decision reads a
    -- signature. The discontinuity is a labelled seam in training data that
    -- does not exist yet, not a break in anything running.
    ADD COLUMN task_signature_scheme SMALLINT,

    -- Full sha256 of the canonical (sorted, length-prefixed) tool-name
    -- multiset that fed task_signature.
    --
    -- 0004 claimed the raw shape features were persisted "so task signatures
    -- can be re-bucketed — or re-keyed to a pooled scheme — retroactively
    -- without a second data system". That was false for the tool dimension:
    -- only tool_count was stored, and a count cannot reproduce a key computed
    -- over names. This column is the missing input, and it is the EXACT value
    -- the live scheme hashes, so a re-key is exact rather than approximate.
    --
    -- It is a digest, never the names, and it is not prompt content: tool
    -- names are request metadata of the same kind as message_count and
    -- prompt_bytes, and prompt content is still never persisted anywhere in
    -- this schema. NULL on pre-0007 rows and on any row replayed from a
    -- settlement intent written before this build.
    ADD COLUMN tool_names_sha256 CHAR(64);

ALTER TABLE usage_events
    -- One-directional on purpose. "A completeness flag only where there is a
    -- sum to qualify" is enforceable against history; the converse ("every sum
    -- carries a flag") holds for rows this build writes but not for rows
    -- written between 0004 and 0007, and a CHECK that fails its own validation
    -- scan is not a constraint, it is an outage.
    ADD CONSTRAINT usage_events_attempts_cost_basis_completeness_needs_a_sum
        CHECK (attempts_cost_basis_complete IS NULL
               OR attempts_cost_basis_usd IS NOT NULL),
    ADD CONSTRAINT usage_events_task_signature_scheme_is_positive
        CHECK (task_signature_scheme IS NULL OR task_signature_scheme >= 1),
    ADD CONSTRAINT usage_events_tool_names_sha256_is_hex
        CHECK (tool_names_sha256 IS NULL OR tool_names_sha256 ~ '^[0-9a-f]{64}$');

COMMENT ON COLUMN usage_events.attempts_cost_basis_complete IS
    'FALSE = attempts_cost_basis_usd is a LOWER BOUND (an unmetered or floor-priced non-serving attempt is missing from it); TRUE = every non-serving attempt was metered; NULL = no attempts recorded';
COMMENT ON COLUMN usage_events.task_signature_scheme IS
    'Encoding that produced task_signature; NULL = scheme 1 (collidable comma-joined tool names). Group by (task_signature, task_signature_scheme) — values are not comparable across schemes';
COMMENT ON COLUMN usage_events.tool_names_sha256 IS
    'sha256 of the sorted length-prefixed tool-name multiset behind task_signature — the one signature input not otherwise recoverable from a settled row, so re-keying is exact. A digest, not the names; still no prompt content';

-- The served attempt's COGS belongs to usage_events.cost_basis_usd and every
-- other attempt's to attempts_cost_basis_usd, so a request settles with at
-- most one attempt on the cost_basis side of the margin expression. The
-- database already enforces the "at most one" half
-- (request_attempts_one_served_per_request, 0004); the code enforces the
-- disjointness half by summing only WHERE NOT served, and by sourcing
-- cost_basis_usd from the served attempt itself when there is one.
COMMENT ON COLUMN usage_events.attempts_cost_basis_usd IS
    'Summed COGS of the NON-serving attempts only (the served attempt is priced into cost_basis_usd); read attempts_cost_basis_complete before treating it as a total';

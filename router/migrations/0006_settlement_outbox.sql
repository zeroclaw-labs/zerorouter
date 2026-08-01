-- 0006_settlement_outbox.sql
--
-- Durable settlement intent, carried on the reservation row itself, so a
-- settle that fails after the customer already has the output can be retried
-- instead of being lost.
--
-- WHY: settlement used to be attempted exactly once. `UsageSession::record`
-- consumed the only handle, built its payload in memory, and ran one
-- transaction; any failure after that point — pool exhaustion, the 5s
-- `lock_timeout`, a dropped connection, an integer conversion, an
-- attempt-row FK violation, a trigger rejection, or an ambiguous COMMIT —
-- returned an error with the payload gone. `persist_usage` mapped it to
-- `metering_unavailable` and the request ended. Nothing retried, and nothing
-- recorded that a settlement had been owed. The transaction's own rollback
-- correctly restored the reservation, but the admission sweep then DELETEd it
-- outright once the 20-minute TTL passed (db.rs, `begin_usage_session`), so
-- delivered inference disappeared with no usage event, no debit, and no trace.
-- The mirror-image failure is worse: an ambiguous COMMIT could debit the
-- balance while the client was told `metering_unavailable`, with no way to
-- tell which had happened.
--
-- SHAPE: the reservation row IS the outbox. A separate outbox table would need
-- its own insert, its own lifecycle, and its own reconciliation against the
-- reservation — three places for the same fact. The reservation row is already
-- (a) created before the upstream is dispatched, (b) keyed by exactly the id
-- that becomes `usage_events.request_id`, and (c) DELETEd by the settle
-- transaction's `DELETE ... RETURNING`. Hanging the intent on it means the
-- intent is destroyed by the same atomic act that records the money, so
-- "settlement owed" and "reservation still open" can never disagree. It also
-- leaves the exactly-once mechanism untouched: money still moves only in a
-- transaction that consumed the reservation row.
--
-- EXACTLY-ONCE UNDER RETRIES is preserved by three independent facts, none of
-- which this migration changes:
--   1. `DELETE FROM usage_reservations ... RETURNING` — the debit is gated on
--      that DELETE having returned a row, in the same transaction. A retry
--      after a committed settle finds no row and never reaches the debit.
--   2. `usage_events.request_id` is UNIQUE (0001:68) — a second settled row
--      for one request is impossible, so a retry that races a committed settle
--      is refused by the database rather than double-counted. The router now
--      reads that refusal as SUCCESS (the work is already done), which is what
--      makes an ambiguous COMMIT safe to retry.
--   3. `credit_ledger_request_unique` (0002:44-46) — a second `usage` debit
--      for one request_id cannot be written even if 1 and 2 were both wrong.
--
-- REGISTRATION REQUIRED: sqlx migrations are a hardcoded include_str! vec —
-- this file does nothing until the `migrate` Migrator in db.rs gains:
--
--   Migration::new(
--       6,
--       Cow::Borrowed("settlement outbox"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0006_settlement_outbox.sql")),
--       false,
--   ),
--
-- Migrations auto-run at startup of both `serve` (main.rs) and `admin`
-- (admin.rs); no deploy choreography beyond shipping the image.
--
-- PRE-EXISTING RESERVATIONS: rows that predate this migration carry NULL
-- intent, which reads as "nothing was owed" — the same answer the old sweep
-- gave them by deleting them. No backfill is possible or wanted.

-- ============================================================================
-- usage_reservations: settlement-intent carrier + quarantine.
--
-- Unlike the money ledgers, this table has never been append-only (0001:43-64)
-- — rows are created at admission and destroyed at settle — so UPDATE is the
-- native verb here and no trigger surgery is needed.
-- ============================================================================

ALTER TABLE usage_reservations
    -- The whole settle payload, written in its own autocommit statement
    -- BEFORE the settle transaction is attempted, so the in-memory record can
    -- never be the only copy. Deserialized and replayed verbatim by the
    -- recovery sweep (db.rs, `recover_owed_settlements`), which therefore
    -- charges exactly what the original settle would have charged and never
    -- more. Carries a `version` discriminant: a payload this build cannot
    -- understand is quarantined for an operator, never guessed at.
    --
    -- Prompt content is NOT in here. The payload is the same
    -- metadata/token-count/rate material already persisted on usage_events and
    -- request_attempts, so the retention contract is unchanged.
    ADD COLUMN settlement_intent JSONB,

    -- When the intent was recorded. Drives the recovery scan's grace period
    -- (an in-request retry is still running for the first few hundred ms) and
    -- makes "how long has this money been owed" a column, not an inference.
    ADD COLUMN settlement_intent_at TIMESTAMPTZ,

    -- Settle transactions attempted for this reservation, in-request retries
    -- included. Bounds the recovery sweep: a payload that keeps failing is
    -- quarantined rather than retried forever.
    ADD COLUMN settle_attempts INTEGER NOT NULL DEFAULT 0,

    -- Class of the last settle failure, for operator triage. Router-generated
    -- text only (sqlx error display), never an upstream body.
    ADD COLUMN last_settle_error TEXT,

    -- Set when a reservation that still owes a settlement is taken out of the
    -- automatic path: it outlived its TTL still owing, its payload could not
    -- be understood, or it exhausted the retry budget. Quarantine is the
    -- REPLACEMENT for the old unconditional expiry DELETE — money that could
    -- not be settled is parked for reconciliation instead of being erased.
    --
    -- A quarantined row is inert for admission: every cap/credit aggregate
    -- filters `expires_at > NOW()` (db.rs, `begin_usage_session`) and a
    -- quarantined row is by construction expired or failing, so leaving it in
    -- place cannot consume a customer's headroom.
    ADD COLUMN quarantined_at TIMESTAMPTZ;

ALTER TABLE usage_reservations
    -- The payload and its timestamp are one fact; neither is meaningful alone.
    ADD CONSTRAINT usage_reservations_intent_carries_its_timestamp
        CHECK ((settlement_intent IS NULL) = (settlement_intent_at IS NULL)),
    -- Nothing can be quarantined for reconciliation unless there is something
    -- to reconcile.
    ADD CONSTRAINT usage_reservations_quarantine_requires_intent
        CHECK (quarantined_at IS NULL OR settlement_intent IS NOT NULL),
    ADD CONSTRAINT usage_reservations_settle_attempts_are_nonnegative
        CHECK (settle_attempts >= 0),
    ADD CONSTRAINT usage_reservations_intent_follows_creation
        CHECK (settlement_intent_at IS NULL OR settlement_intent_at >= created_at);

-- The recovery sweep's read path: oldest owed settlement first, over the tiny
-- subset that owes anything at all. Partial, so the index is empty in steady
-- state and the scan costs nothing when no settlement has ever failed.
CREATE INDEX usage_reservations_owed_settlement_idx
    ON usage_reservations (settlement_intent_at)
    WHERE settlement_intent IS NOT NULL AND quarantined_at IS NULL;

-- The operator scan: money that was owed and could not be settled
-- automatically. `zerorouter admin owed-settlements` reads exactly this.
CREATE INDEX usage_reservations_quarantined_idx
    ON usage_reservations (quarantined_at)
    WHERE quarantined_at IS NOT NULL;

COMMENT ON COLUMN usage_reservations.settlement_intent IS
    'Durable settle payload written before the settle transaction is attempted; destroyed by the settle DELETE...RETURNING, so its presence means "settlement still owed". Replayed verbatim on recovery, so a retry can never charge more than the original settle would have';
COMMENT ON COLUMN usage_reservations.settlement_intent_at IS
    'When the intent was recorded; the recovery sweep only touches rows older than its grace period so it cannot race the in-request retry';
COMMENT ON COLUMN usage_reservations.quarantined_at IS
    'Set instead of deleting an expired reservation that still owes a settlement, or when the retry budget is exhausted. Inert for admission (every aggregate filters expires_at > NOW()); the reconciliation queue for an operator';

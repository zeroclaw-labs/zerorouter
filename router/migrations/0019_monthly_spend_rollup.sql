-- Make admission's monthly-spend read cost independent of how much a user has
-- already spent this month.
--
-- # The problem
--
-- `begin_usage_session` enforces the spend ceilings from
-- `SUM(usage_events.cost_usd)` over every row the user has month-to-date. That
-- is a per-request scan whose size is the user's own history, paid on BOTH the
-- metered and the free lane. Measured on 523k events with a 20-key user, p50:
--
--       0 month-to-date rows -> 0.52 ms
--   1,000                    -> 0.69 ms
--  10,000                    -> 5.08 ms
--  30,000                    -> 14.56 ms
--
-- ~0.47 us per row, growing all month and resetting on the 1st. A customer
-- doing 30k requests a month makes every one of their own requests 14 ms
-- slower, and the cost is admission — it is paid before anything is dispatched.
--
-- # Why the fix the db.rs comment sketched is not enough
--
-- That comment proposed widening `usage_events_key_timestamp_idx` to
-- INCLUDE (cost_usd, input_tokens, output_tokens) for an index-only scan. Two
-- findings, both measured on the workload above:
--
--   * As written it does not produce an index-only scan at all. The query also
--     reads `cached_input_tokens`, which the sketch omits, so the plan stays a
--     Bitmap Heap Scan with 24,854 heap blocks — no better than today.
--   * Corrected to INCLUDE all four payload columns it does reach
--     `Index Only Scan ... Heap Fetches: 0`, and 30k rows drops 14.56 ms ->
--     7.29 ms. But that is a constant factor, not a fix: the cost is still
--     linear in the user's month (~0.23 us/row), so it only moves the row count
--     at which the problem returns.
--
-- Any per-row aggregate over the month is O(month). Making admission
-- independent of row count means not reading the month at all, which means
-- maintaining the sum as rows arrive.
--
-- # The design
--
-- One row per (api_key_id, UTC month) carrying that key's settled spend for
-- that month, accrued by an AFTER INSERT trigger on `usage_events`. Admission
-- then reads one rollup row per key the user owns — O(keys), and keys are
-- already capped — instead of one usage row per request they have made.
--
-- The velocity ceilings are NOT moved here. They are a sliding one-minute
-- window, which no monthly bucket can answer, and they do not need it: the
-- one-minute window is bounded by throughput rather than by history, so
-- `usage_events_key_timestamp_idx` (0001) already answers it in constant time.
-- Splitting the old single scan in two is what lets each half use the access
-- path that suits it.
--
-- Deliberately NO new index on `usage_events`. The velocity read is a short
-- per-key range scan whose heap fetches are bounded by one minute of traffic,
-- so it does not need a covering index, and it therefore does not depend on
-- the visibility map being current. That matters on a hot append-only table:
-- an index-only scan over freshly inserted rows degrades to heap fetches until
-- autovacuum sets the all-visible bit, so a design that claimed index-only
-- would have quietly regressed to the old cost exactly for the busiest users.
-- This design has no such cliff — there is no plan here that VACUUM lag can
-- turn back into a full-month heap scan.
--
-- # Why the numbers are identical, not merely close
--
-- Let M(t) = date_trunc('month', t AT TIME ZONE 'UTC') AT TIME ZONE 'UTC',
-- which is `usage_event_utc_month` below. M is monotone non-decreasing (UTC has
-- no DST, so each step is), idempotent on month starts, and t >= M(t) always.
-- Admission's old predicate was `ts >= M(NOW())`; the new one is
-- `month >= M(NOW())` over buckets keyed by M(ts). Those select the same events:
--
--   ts >= M(NOW())  =>  M(ts) >= M(M(NOW())) = M(NOW())     (monotone, idempotent)
--   M(ts) >= M(NOW())  =>  ts >= M(ts) >= M(NOW())          (t >= M(t))
--
-- so { e : e.ts >= M(NOW()) } and { e : M(e.ts) >= M(NOW()) } are the same set,
-- and summing the buckets from M(NOW()) upward sums exactly that set. Note the
-- `>=` rather than `=`: a row whose ts lands in a LATER month (two routers with
-- skewed clocks across a month boundary) was counted by the old predicate, so
-- it must be counted by the new one. `= M(NOW())` would have dropped it and
-- would have been a real, if tiny, change in what the cap enforces.
--
-- The rollup cannot drift from the ledger. `usage_events` rejects UPDATE,
-- DELETE and TRUNCATE (0001), so INSERT is the only mutation there is, and the
-- trigger below is AFTER INSERT FOR EACH ROW in the inserting transaction: the
-- event and its contribution to the sum commit together or not at all. A row
-- whose INSERT fails the `request_id` UNIQUE constraint — the replayed-settle
-- path — never fires the trigger, because no row was inserted.
--
-- # Ordering inside this migration
--
-- CREATE TRIGGER before the backfill, on purpose, and both in this migration's
-- single transaction. CREATE TRIGGER takes SHARE ROW EXCLUSIVE on
-- `usage_events`, which conflicts with the ROW EXCLUSIVE an INSERT holds. So it
-- waits for every in-flight insert to commit, and every later insert waits for
-- this migration to commit. The backfill in between therefore sees a settled
-- table: no row can land after the backfill's snapshot yet before the trigger
-- exists, which is the one interleaving that would silently lose spend.
--
-- RUNBOOK: that lock is the cost of the guarantee, and it is held for the whole
-- backfill scan, not just the CREATE. Settles BLOCK for as long as the
-- aggregate over `usage_events` takes — 0.25 s on the 523k-row benchmark
-- database, so unremarkable today. On a table of tens of millions of rows it is
-- a stall measured in tens of seconds, and that is a maintenance window rather
-- than a routine deploy. Take that seriously before running this against a
-- grown production ledger.

-- Truncating the very smallest representable timestamptz to a month start can
-- underflow and raise a range error rather than returning a value. No writer
-- can reach it: `usage_events.ts` is filled by the column DEFAULT NOW() at
-- every insert site, and the backfill below reads values that got there the
-- same way. Stated so a future caller that starts passing a caller-supplied
-- `ts` knows this function has a domain edge rather than discovering it as a
-- failed settle.
CREATE FUNCTION usage_event_utc_month(ts TIMESTAMPTZ)
RETURNS TIMESTAMPTZ
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
    SELECT date_trunc('month', $1 AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
$$;

COMMENT ON FUNCTION usage_event_utc_month(TIMESTAMPTZ) IS
    'The UTC month bucket an event falls in; the single definition admission and the accrual trigger share';

CREATE TABLE usage_key_month_spend (
    api_key_id UUID NOT NULL REFERENCES api_keys (id),
    month TIMESTAMPTZ NOT NULL,
    spend_usd NUMERIC NOT NULL,
    PRIMARY KEY (api_key_id, month),
    CONSTRAINT usage_key_month_spend_is_nonnegative
        CHECK (spend_usd >= 0),
    -- The bucket key is always a month start, so a hand-written row cannot
    -- park spend at a timestamp admission's range read would skip.
    CONSTRAINT usage_key_month_spend_is_a_month_start
        CHECK (month = usage_event_utc_month(month))
);

COMMENT ON TABLE usage_key_month_spend IS
    'Derived running total of usage_events.cost_usd per (api_key_id, UTC month); maintained by trigger, never written by hand';

CREATE FUNCTION accrue_usage_key_month_spend()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO usage_key_month_spend (api_key_id, month, spend_usd)
    VALUES (NEW.api_key_id, usage_event_utc_month(NEW.ts), NEW.cost_usd)
    ON CONFLICT (api_key_id, month)
    DO UPDATE SET spend_usd = usage_key_month_spend.spend_usd + EXCLUDED.spend_usd;
    RETURN NULL;
END;
$$;

CREATE TRIGGER usage_events_accrue_month_spend
    AFTER INSERT ON usage_events
    FOR EACH ROW
    EXECUTE FUNCTION accrue_usage_key_month_spend();

INSERT INTO usage_key_month_spend (api_key_id, month, spend_usd)
SELECT api_key_id, usage_event_utc_month(ts), SUM(cost_usd)
FROM usage_events
GROUP BY api_key_id, usage_event_utc_month(ts);

-- Only the accrual trigger may write this table.
--
-- Without this, `UPDATE usage_key_month_spend SET spend_usd = ...` succeeds
-- silently, and because nothing ever recomputes the total from the ledger the
-- divergence is permanent and invisible — admission goes on enforcing the
-- wrong number for that key forever. That is a worse failure than the scan
-- this migration removed: slow is visible, a quietly wrong spend ceiling is
-- not. `usage_events` has been protected this way since 0001 and the table
-- derived from it gets the same treatment.
--
-- `pg_trigger_depth() = 0` is what distinguishes the two writers. The WHEN
-- clause is evaluated before the trigger function is entered, so a statement
-- issued by a client sees depth 0 and is refused, while the accrual trigger's
-- own INSERT ... ON CONFLICT is issued from inside a trigger function and sees
-- depth 1, so this never fires for it. The rejection is therefore about who is
-- writing, not about what they wrote.
--
-- Created AFTER the backfill on purpose: the backfill is a top-level statement
-- and would see depth 0 and refuse itself. This ordering does not weaken the
-- fencing argument in the header — that argument is about the lock CREATE
-- TRIGGER takes on `usage_events`, and this trigger is on a different table.
--
-- What this cannot defend against: a restore that bypasses triggers wholesale
-- (`SET session_replication_role = replica`, `pg_restore --disable-triggers`)
-- silences the accrual trigger and this guard together, so events land with no
-- contribution to the rollup and the totals are left short. After any such
-- restore the rollup must be REBUILT from `usage_events`, not patched — and
-- because this guard refuses hand-written repairs under the normal role, a
-- partial reset fails loudly instead of half-succeeding.
CREATE FUNCTION reject_usage_key_month_spend_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'usage_key_month_spend is maintained by trigger; it cannot be written directly';
    RETURN NULL;
END;
$$;

CREATE TRIGGER usage_key_month_spend_reject_direct_mutation
    BEFORE INSERT OR UPDATE OR DELETE ON usage_key_month_spend
    FOR EACH ROW
    WHEN (pg_trigger_depth() = 0)
    EXECUTE FUNCTION reject_usage_key_month_spend_mutation();

-- TRUNCATE is statement-level and the accrual path never issues one, so this
-- arm needs no depth exemption.
CREATE TRIGGER usage_key_month_spend_reject_truncate
    BEFORE TRUNCATE ON usage_key_month_spend
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_usage_key_month_spend_mutation();

-- A table this migration both created and filled has no statistics until
-- autoanalyze happens to reach it, and the planner reads that as "empty". The
-- admission read still works and is still flat, but measured 1.44 ms instead of
-- 0.26 ms until statistics existed — a regression window on exactly the deploy
-- that is supposed to remove one. Cheap to close here, since this transaction
-- already holds the table and just wrote every row in it.
ANALYZE usage_key_month_spend;

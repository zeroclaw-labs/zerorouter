-- Per-key expiry and a per-key credit limit with a calendar reset cadence.
--
-- Three columns on `api_keys` and two derived spend counters. Together they
-- give a key an end date and a budget of its own, which is what a customer
-- handing a key to a contractor, a CI job, or an agent actually wants: a key
-- that dies on its own and cannot spend past a number they chose.
--
-- # What already existed, and why this is not it
--
-- `api_keys.spend_cap_usd` is NOT NULL DEFAULT 20 and is already enforced
-- month-to-date by `begin_usage_session`, both for the presenting key and as a
-- USER ceiling derived as MAX over the user's live keys. That ceiling is an
-- ANTI-ABUSE guardrail: it is what stops disable-and-remint from multiplying a
-- quota, and the derivation is why churning keys no longer resets it.
--
-- `credit_limit_usd` is a different thing wearing a similar name, and the two
-- are deliberately separate columns rather than one:
--
--   * It is the CUSTOMER's own budget knob on their own key, set from the
--     portal, and NULL means unlimited. `spend_cap_usd` is the operator's
--     ceiling and has no "unlimited" — teaching it one would let a customer
--     delete their own guardrail from the portal, which is a straight
--     weakening of admission.
--   * The user ceiling is `MAX(spend_cap_usd)` over live siblings. SQL MAX
--     ignores NULLs, so a nullable `spend_cap_usd` would make one "unlimited"
--     key vanish from the derivation and leave the ceiling equal to the
--     LOOSEST REMAINING cap — simultaneously too tight for the unlimited key
--     and wrong for every other. There is no NULL semantics that fixes this,
--     because the column's meaning under aggregation is the problem.
--
-- So admission enforces both and the tighter one wins. The new column can only
-- ever REFUSE MORE: a request admitted after this migration would have been
-- admitted before it, and every key that exists today gets NULL for all three
-- columns and is therefore bit-for-bit unchanged.
--
-- # The reset cadence
--
-- `credit_limit_window` is NULL (no reset — a lifetime cap on the key) or one
-- of 'daily' / 'weekly' / 'monthly'. Windows are CALENDAR windows in UTC, not
-- rolling ones: a daily window starts at 00:00 UTC, a weekly window on Monday
-- 00:00 UTC (`date_trunc('week', ...)`), a monthly window on the 1st. Calendar
-- rather than rolling because a customer setting "$20 a day" means the day, and
-- because a rolling window cannot be answered by a bucket at all — it is the
-- same reason the velocity ceiling is NOT in a rollup and never will be.
--
-- NULL is the no-reset case and also the value every pre-existing row gets, so
-- "this key has a limit and no reset" and "this key predates the feature" are
-- distinguished by `credit_limit_usd`, not by the window. The CHECK below makes
-- a window without a limit unrepresentable, so there is no third state to
-- interpret.
--
-- # Why two new counters and not one, and not zero
--
-- Admission must answer "what has this key spent in its current window" on
-- every request, in constant time. Migration 0019 is the precedent and the
-- warning: reading that number from `usage_events` directly is O(rows in the
-- window), which is exactly the bug 0019 removed (0.47 us/row, growing all
-- month). A daily window over a busy key is the same shape of scan, just with a
-- shorter fuse, so reading the ledger per request is not an option here either.
--
--   monthly  -> ALREADY ANSWERED. `usage_key_month_spend` (0019) is keyed by
--               (api_key_id, UTC month) and admission already reads the
--               presenting key's month-to-date total out of it for the
--               `spend_cap_usd` gate. The monthly cadence reuses that value and
--               costs nothing at all.
--   daily    -> one row of `usage_key_day_spend`.
--   weekly   -> at most seven rows of `usage_key_day_spend`.
--   none     -> one row of `usage_key_total_spend`.
--
-- Day grain serves both daily and weekly, so no week-grain bucket is added: a
-- seven-row range scan on a PK is not worth a third write per settled event.
--
-- The lifetime total gets its OWN row rather than being summed from the day
-- buckets, and that is the whole reason it exists. SUM over every day bucket a
-- key has ever had is unbounded in the key's age — a two-year-old key would
-- cost 700+ rows per request and would grow forever, which is the O(history)
-- failure 0019 exists to prevent, reintroduced through a different table. One
-- row that the trigger increments is O(1) forever.
--
-- # The counters cannot drift from the ledger
--
-- Inherited verbatim from 0019's argument, and it holds for the same reasons.
-- `usage_events` rejects UPDATE, DELETE and TRUNCATE (0001), so INSERT is the
-- only mutation there is; the trigger below is AFTER INSERT FOR EACH ROW in the
-- inserting transaction, so an event and its contribution to both counters
-- commit together or not at all. A settle replay that trips the
-- `usage_events.request_id` UNIQUE constraint inserts no row and therefore
-- fires no trigger, so exactly-once settlement stays exactly-once here.
--
-- Both counters are backfilled from the ledger in this migration, and that is a
-- deliberate choice rather than an oversight of the "no backfill" rule for
-- append-only ledgers. These tables are DERIVED data, not history: nothing in
-- `usage_events` or `request_attempts` is read, rewritten, or re-attributed.
-- The alternative — start both counters at zero — would make a lifetime limit
-- set on an existing key mean something other than what the portal displays,
-- silently forgiving whatever that key had already spent. A counter that is
-- wrong in the customer's favour is still a wrong counter.
--
-- RUNBOOK: `CREATE TRIGGER` on `usage_events` takes SHARE ROW EXCLUSIVE, which
-- conflicts with the ROW EXCLUSIVE an INSERT holds, and this migration holds it
-- across both backfill scans. Settles BLOCK for that duration. On a small
-- ledger this is milliseconds; on a grown production ledger two aggregate scans
-- over every event ever recorded is a maintenance window, not a routine deploy.
-- 0019 carries the same warning and the same measurement — take it seriously.

-- The three key columns. All nullable with no default, so every existing row
-- reads NULL for all three and admission's behavior for it is unchanged.
ALTER TABLE api_keys
    ADD COLUMN expires_at TIMESTAMPTZ,
    ADD COLUMN credit_limit_usd NUMERIC,
    ADD COLUMN credit_limit_window TEXT;

COMMENT ON COLUMN api_keys.expires_at IS
    'When this key stops authenticating; NULL never expires. Enforced in the admission UPDATE predicate, so an expired key is refused exactly as a revoked one is';
COMMENT ON COLUMN api_keys.credit_limit_usd IS
    'Customer-set spend cap for this key within its reset window; NULL is unlimited. Independent of, and narrower than, the operator ceiling in spend_cap_usd';
COMMENT ON COLUMN api_keys.credit_limit_window IS
    'Calendar UTC reset cadence for credit_limit_usd: NULL never resets (lifetime), else daily | weekly | monthly';

-- Zero is excluded rather than allowed. A zero limit refuses every request
-- including the free lane, which is a revocation wearing a budget's clothes;
-- `disabled` already says that, says it in one place, and is what both
-- revocation paths and the portal's own UI operate on. `spend_cap_usd` permits
-- zero (0001) and keeps permitting it — that column is an operator's ceiling
-- and an operator may legitimately want a key that can authenticate but not
-- spend. A customer reaching for zero in the portal has made a mistake.
ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_credit_limit_is_positive
        CHECK (credit_limit_usd IS NULL OR credit_limit_usd > 0);

-- The keyword set admission switches on. Spelled here rather than only in Rust
-- so a hand-written UPDATE cannot park a key on a cadence the router has no
-- branch for — which would silently read an empty window and enforce nothing.
ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_credit_limit_window_is_known
        CHECK (credit_limit_window IS NULL
               OR credit_limit_window IN ('daily', 'weekly', 'monthly'));

-- A cadence with nothing to reset is not a state worth being able to represent:
-- it reads as a configured limit to anything scanning the column and enforces
-- nothing. Making it unrepresentable means admission never has to decide what
-- it meant.
ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_credit_limit_window_needs_a_limit
        CHECK (credit_limit_window IS NULL OR credit_limit_usd IS NOT NULL);

-- The day bucket an event falls in, as the single definition admission and the
-- accrual trigger share — 0019's `usage_event_utc_month` for the finer grain.
--
-- Returns DATE rather than a truncated TIMESTAMPTZ, which removes a whole class
-- of defect 0019 had to CHECK its way out of: a DATE has no sub-day component,
-- so there is no way to park spend at a timestamp inside a bucket that the
-- range read would skip, and no constraint is needed to say so.
--
-- `AT TIME ZONE 'UTC'` on a TIMESTAMPTZ is the immutable direction of that cast
-- (it names the zone rather than reading the session's), which is what lets
-- this be IMMUTABLE and therefore usable in an index and in a CHECK.
CREATE FUNCTION usage_event_utc_day(ts TIMESTAMPTZ)
RETURNS DATE
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
    SELECT ($1 AT TIME ZONE 'UTC')::DATE
$$;

COMMENT ON FUNCTION usage_event_utc_day(TIMESTAMPTZ) IS
    'The UTC day bucket an event falls in; the single definition admission and the accrual trigger share';

CREATE TABLE usage_key_day_spend (
    api_key_id UUID NOT NULL REFERENCES api_keys (id),
    day DATE NOT NULL,
    spend_usd NUMERIC NOT NULL,
    PRIMARY KEY (api_key_id, day),
    CONSTRAINT usage_key_day_spend_is_nonnegative
        CHECK (spend_usd >= 0)
);

COMMENT ON TABLE usage_key_day_spend IS
    'Derived running total of usage_events.cost_usd per (api_key_id, UTC day); serves the daily and weekly credit-limit windows. Maintained by trigger, never written by hand';

CREATE TABLE usage_key_total_spend (
    api_key_id UUID PRIMARY KEY REFERENCES api_keys (id),
    spend_usd NUMERIC NOT NULL,
    CONSTRAINT usage_key_total_spend_is_nonnegative
        CHECK (spend_usd >= 0)
);

COMMENT ON TABLE usage_key_total_spend IS
    'Derived lifetime total of usage_events.cost_usd per api_key_id; serves the no-reset credit-limit window in O(1). Maintained by trigger, never written by hand';

-- One trigger maintaining both counters rather than one trigger each: they
-- accrue from the same row, at the same moment, under the same transaction, and
-- splitting them would double the per-event trigger dispatch to buy nothing.
CREATE FUNCTION accrue_usage_key_spend_windows()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO usage_key_day_spend (api_key_id, day, spend_usd)
    VALUES (NEW.api_key_id, usage_event_utc_day(NEW.ts), NEW.cost_usd)
    ON CONFLICT (api_key_id, day)
    DO UPDATE SET spend_usd = usage_key_day_spend.spend_usd + EXCLUDED.spend_usd;

    INSERT INTO usage_key_total_spend (api_key_id, spend_usd)
    VALUES (NEW.api_key_id, NEW.cost_usd)
    ON CONFLICT (api_key_id)
    DO UPDATE SET spend_usd = usage_key_total_spend.spend_usd + EXCLUDED.spend_usd;

    RETURN NULL;
END;
$$;

CREATE TRIGGER usage_events_accrue_spend_windows
    AFTER INSERT ON usage_events
    FOR EACH ROW
    EXECUTE FUNCTION accrue_usage_key_spend_windows();

INSERT INTO usage_key_day_spend (api_key_id, day, spend_usd)
SELECT api_key_id, usage_event_utc_day(ts), SUM(cost_usd)
FROM usage_events
GROUP BY api_key_id, usage_event_utc_day(ts);

INSERT INTO usage_key_total_spend (api_key_id, spend_usd)
SELECT api_key_id, SUM(cost_usd)
FROM usage_events
GROUP BY api_key_id;

-- Only the accrual trigger may write these two tables, for exactly the reason
-- 0019 fenced its own rollup: nothing ever recomputes these totals from the
-- ledger, so a hand-written UPDATE is a permanent, invisible divergence, and
-- admission goes on enforcing the wrong number for that key forever. A slow
-- query is visible; a quietly wrong credit limit is not.
--
-- `pg_trigger_depth() = 0` distinguishes the two writers. The WHEN clause is
-- evaluated before the function is entered, so a client statement sees depth 0
-- and is refused, while the accrual trigger's own upserts are issued from
-- inside a trigger function at depth 1 and never reach this. The rejection is
-- about WHO is writing, not about what they wrote.
--
-- Created AFTER the backfills on purpose: those are top-level statements and
-- would see depth 0 and refuse themselves.
--
-- What this cannot defend against, inherited from 0019: a restore that bypasses
-- triggers wholesale (`SET session_replication_role = replica`,
-- `pg_restore --disable-triggers`) silences the accrual trigger and this guard
-- together, so events land contributing nothing and both counters are left
-- short. After any such restore these tables must be REBUILT from
-- `usage_events`, not patched — and because this guard refuses hand-written
-- repairs under the normal role, a partial reset fails loudly rather than
-- half-succeeding.
CREATE FUNCTION reject_usage_key_spend_window_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% is maintained by trigger; it cannot be written directly', TG_TABLE_NAME;
    RETURN NULL;
END;
$$;

CREATE TRIGGER usage_key_day_spend_reject_direct_mutation
    BEFORE INSERT OR UPDATE OR DELETE ON usage_key_day_spend
    FOR EACH ROW
    WHEN (pg_trigger_depth() = 0)
    EXECUTE FUNCTION reject_usage_key_spend_window_mutation();

CREATE TRIGGER usage_key_total_spend_reject_direct_mutation
    BEFORE INSERT OR UPDATE OR DELETE ON usage_key_total_spend
    FOR EACH ROW
    WHEN (pg_trigger_depth() = 0)
    EXECUTE FUNCTION reject_usage_key_spend_window_mutation();

-- TRUNCATE is statement-level and the accrual path never issues one, so these
-- arms need no depth exemption.
CREATE TRIGGER usage_key_day_spend_reject_truncate
    BEFORE TRUNCATE ON usage_key_day_spend
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_usage_key_spend_window_mutation();

CREATE TRIGGER usage_key_total_spend_reject_truncate
    BEFORE TRUNCATE ON usage_key_total_spend
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_usage_key_spend_window_mutation();

-- Tables this migration both created and filled have no statistics until
-- autoanalyze happens to reach them, and the planner reads that as "empty".
-- 0019 measured that as 1.44 ms instead of 0.26 ms on the admission read until
-- statistics existed — a regression window on exactly the deploy meant to avoid
-- one. Cheap to close here, since this transaction already holds both tables
-- and just wrote every row in them.
ANALYZE usage_key_day_spend;
ANALYZE usage_key_total_spend;

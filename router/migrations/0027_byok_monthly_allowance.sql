-- 0027_byok_monthly_allowance.sql
--
-- The first $5,000 of catalog-equivalent BYOK usage a customer runs in a UTC
-- calendar month carries no fee; the 5% of migration 0026 applies only beyond
-- it. This migration adds the three columns that make that arithmetic possible
-- without reading a customer's month back one row at a time.
--
-- The allowance figure itself is NOT here. It lives in `crate::byok`
-- (`monthly_allowance`), for exactly the reason 0026 gives for keeping the 5%
-- out of the schema: a number that decides what a customer is charged must not
-- be a value an operator can edit. This migration stores what was MEASURED —
-- how much catalog-equivalent BYOK usage happened — and the code decides what
-- that costs.
--
-- # What is accumulated, and why it is the list price
--
-- The allowance is denominated in CATALOG-equivalent dollars, not in the fee.
-- A customer's "$5,000 of free BYOK" means $5,000 of inference measured at
-- ZeroRouter's list rates — the same figure the 5% is a percentage of — because
-- that is the number a customer can compare against a competitor's offer and
-- against their own vendor invoice. Accumulating the FEE instead would make the
-- allowance mean "$5,000 of fees", i.e. $100,000 of traffic, which is not what
-- was decided and is not what the portal says.
--
-- # Why the accumulator extends 0019's rollup instead of being a new table
--
-- 0019 already maintains exactly this shape: one row per (api_key_id, UTC
-- month), accrued by an AFTER INSERT trigger on `usage_events`, fenced against
-- direct writes, and read by admission as one indexed probe per key the user
-- owns. Everything the allowance needs is a second measure in that same bucket:
--
--   * The read is FREE. Admission's monthly-spend subquery
--     (`begin_usage_session`) already aggregates this table for this user over
--     `month >= usage_event_utc_month(NOW())`. The allowance's consumed figure
--     is one more `SUM(...)` in a select list that was already being computed
--     over exactly the right rows — no new table, no new join, no extra round
--     trip on the hot path.
--   * The DRIFT ARGUMENT is inherited whole. `usage_events` rejects UPDATE,
--     DELETE and TRUNCATE (0001), so INSERT is the only mutation, and the
--     accrual below runs AFTER INSERT FOR EACH ROW inside the inserting
--     transaction: the event and its contribution commit together or not at
--     all. A settle that rolls back contributes nothing, and a replayed settle
--     that trips the `request_id` UNIQUE never fires the trigger because no row
--     was inserted. That is precisely the exactly-once property the allowance
--     needs, and it already exists.
--   * The FENCE is inherited whole. 0019's
--     `usage_key_month_spend_reject_direct_mutation` refuses any write at
--     `pg_trigger_depth() = 0`, so the new column is as unwritable by hand as
--     the old one. A separate table would have needed its own copy of that
--     guard, and a copy is a thing that can be forgotten.
--
-- A second table would have bought separation of concerns and paid for it in a
-- second round trip on every admission, a second trigger, a second fence, and a
-- second chance for one of them to be wrong. The measure belongs in the bucket
-- that is already keyed by the thing it is bucketed by.
--
-- # The UTC calendar discipline
--
-- Unchanged, and deliberately not re-derived: the bucket key is
-- `usage_event_utc_month(NEW.ts)`, the same function 0019 defined and the same
-- one the credit-limit windows of 0023 follow. 0019 carries the proof that
-- `month >= usage_event_utc_month(NOW())` and `ts >= <start of month>` select
-- the same set of events for every possible `ts`, and that proof covers this
-- column because it is a property of the KEY, not of the measure. The allowance
-- therefore resets at exactly the instant every other monthly figure in this
-- database resets, which is the only way a customer's "you have used $X of
-- $5,000" can agree with their own month-to-date spend.

-- ---------------------------------------------------------------------------
-- 1. What a BYOK request would have cost at catalog rates
-- ---------------------------------------------------------------------------

-- The list-price cost of a request that dispatched on the customer's own
-- credential — the figure the 5% is a percentage of, and the figure that
-- consumes the allowance.
--
-- NULLABLE with no default and written at INSERT, the rule `usage_events` has
-- had since 0001. NULL means "this row does not consume allowance": every
-- house-credential row, and every row settled before this migration existed.
--
-- # Why the pairing with `byok` is NOT a validated CHECK
--
-- The tempting constraint is `(byok IS TRUE) = (byok_catalog_usd IS NOT NULL)`,
-- and it cannot be added here. Migration 0026 shipped in #103, so a deployment
-- that has been running it already holds rows with `byok = TRUE` and no basis
-- recorded — the column did not exist when they settled. `usage_events` rejects
-- UPDATE, so those rows can never be filled in, and a validated CHECK would
-- refuse to apply this migration to exactly the deployments that have customers
-- using the feature.
--
-- So NULL on a `byok = TRUE` row reads as "settled before the allowance
-- existed", the same reading 0026 gives a NULL `byok` ("settled before BYOK
-- existed"), and for the same structural reason. Those rows contribute zero to
-- the accumulator, which is the correct treatment: the allowance began when
-- this migration did, and back-dating consumption onto a customer's first month
-- would silently spend an allowance they were never told they had.
--
-- What IS constrained is the value itself. A negative catalog basis is not a
-- row that predates anything, it is a row that would REFUND allowance to the
-- customer who produced it, and no writer can legitimately produce one.
ALTER TABLE usage_events
    ADD COLUMN byok_catalog_usd NUMERIC;

ALTER TABLE usage_events
    ADD CONSTRAINT usage_events_byok_catalog_is_nonnegative
        CHECK (byok_catalog_usd IS NULL OR byok_catalog_usd >= 0);

COMMENT ON COLUMN usage_events.byok_catalog_usd IS
    'What this request would have cost at catalog rates, on the rows that dispatched on the customer''s own credential; the basis the monthly BYOK allowance is consumed in. NULL on house-credential rows and on rows settled before migration 0027';

-- ---------------------------------------------------------------------------
-- 2. The accumulator
-- ---------------------------------------------------------------------------

-- Per (api_key_id, UTC month), beside the spend total 0019 already keeps.
--
-- NOT NULL DEFAULT 0 rather than nullable: this is a running total, and the
-- total of no BYOK usage is zero, not unknown. That also makes the ADD COLUMN a
-- metadata-only change on PostgreSQL 11+ — no table rewrite, no backfill — and
-- every row that already exists correctly reports that it accumulated no BYOK
-- catalog cost, because every event behind it recorded none.
ALTER TABLE usage_key_month_spend
    ADD COLUMN byok_catalog_usd NUMERIC NOT NULL DEFAULT 0;

ALTER TABLE usage_key_month_spend
    ADD CONSTRAINT usage_key_month_spend_byok_catalog_is_nonnegative
        CHECK (byok_catalog_usd >= 0);

COMMENT ON COLUMN usage_key_month_spend.byok_catalog_usd IS
    'Derived running total of usage_events.byok_catalog_usd for this (api_key_id, UTC month); the basis the $5,000 monthly BYOK allowance is measured against. Maintained by trigger, never written by hand';

-- Accrue both measures in the one upsert.
--
-- CREATE OR REPLACE rather than a new function and a new trigger: the trigger
-- `usage_events_accrue_month_spend` (0019) already names this function and
-- already fires AFTER INSERT FOR EACH ROW, so replacing the body is the whole
-- change. Adding a SECOND trigger for the second measure would have meant two
-- upserts against the same row in the same statement — the same work twice, and
-- two places for the month key to be computed differently.
--
-- `COALESCE(NEW.byok_catalog_usd, 0)` is what keeps a house row inert: it
-- contributes zero to the BYOK total while still contributing its `cost_usd` to
-- the spend total, so nothing about a non-BYOK request's accounting changes.
CREATE OR REPLACE FUNCTION accrue_usage_key_month_spend()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO usage_key_month_spend (
        api_key_id, month, spend_usd, byok_catalog_usd
    )
    VALUES (
        NEW.api_key_id,
        usage_event_utc_month(NEW.ts),
        NEW.cost_usd,
        COALESCE(NEW.byok_catalog_usd, 0)
    )
    ON CONFLICT (api_key_id, month)
    DO UPDATE SET
        spend_usd = usage_key_month_spend.spend_usd + EXCLUDED.spend_usd,
        byok_catalog_usd =
            usage_key_month_spend.byok_catalog_usd + EXCLUDED.byok_catalog_usd;
    RETURN NULL;
END;
$$;

-- No backfill statement, and that is a decision rather than an omission. Every
-- `usage_events` row that exists when this migration runs has
-- `byok_catalog_usd` NULL — the column is created in this same transaction — so
-- a backfill would sum zero over the whole ledger and write nothing that the
-- DEFAULT 0 above has not already written. The rollup is therefore correct the
-- instant this commits, without taking 0019's lock on `usage_events` for the
-- duration of a full scan. (0019's RUNBOOK note about that stall does not apply
-- to this migration: nothing here reads `usage_events` at all.)

-- ---------------------------------------------------------------------------
-- 3. The allowance a live reservation has already committed
-- ---------------------------------------------------------------------------

-- The worst-case catalog cost of a request that is in flight and could still
-- settle as BYOK.
--
-- # Why the reservation has to carry this at all
--
-- Admission may reserve NOTHING for a request that fits inside the remaining
-- allowance, because such a request settles at a zero fee and reserving against
-- a zero charge would hold a customer's credit for nothing — the feature is an
-- adoption lever, and a "free" tier that still demands a funded balance is not
-- one. That decision is only safe if the request is still free WHEN IT SETTLES,
-- and the settle happens later.
--
-- The threat is concurrency, not time. Consider $100 of allowance left and ten
-- simultaneous requests whose worst case is $50 each. Every one of them reads
-- the same "$100 remaining", every one of them individually fits, so every one
-- of them reserves zero — and together they consume $500, of which $400 is
-- above the allowance and should have been billed. The settle debit is clamped
-- to the reservation (`crate::db::settle_once`), so that $400 of fee cannot be
-- collected at all: it is inference delivered and not billable, which
-- `AGENTS.md` names as the first failure this repo exists to prevent.
--
-- Recording each in-flight request's basis here closes it. Admission subtracts
-- the sum of this column over the user's live reservations from the remaining
-- allowance before deciding, and both the read and the INSERT happen under the
-- per-user advisory lock, so request N+1 necessarily observes request N's
-- commitment. The bound is then structural: the total catalog basis that can
-- possibly land in a month from settled rows plus live reservations never
-- exceeds the allowance while any request is still reserving zero.
--
-- NULL means "this reservation cannot settle as BYOK and commits no allowance"
-- — every house-only route, and every reservation taken before this migration.
-- Unlike `usage_events` this table is not append-only, but nothing UPDATEs this
-- column: it is written once at INSERT and read until the row is consumed.
ALTER TABLE usage_reservations
    ADD COLUMN byok_catalog_basis_usd NUMERIC;

ALTER TABLE usage_reservations
    ADD CONSTRAINT usage_reservations_byok_catalog_basis_is_nonnegative
        CHECK (byok_catalog_basis_usd IS NULL OR byok_catalog_basis_usd >= 0);

COMMENT ON COLUMN usage_reservations.byok_catalog_basis_usd IS
    'Worst-case catalog cost of an in-flight request that could still settle as BYOK; subtracted from the remaining monthly allowance so concurrent requests cannot each claim the same last dollar of it. NULL when the route cannot dispatch on a customer credential';

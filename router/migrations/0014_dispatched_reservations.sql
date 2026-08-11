-- 0014_dispatched_reservations.sql
--
-- A reservation whose request actually reached an upstream is marked as such,
-- so the TTL sweep can stop confusing "the customer got nothing" with "the
-- customer got tokens and we lost the paperwork".
--
-- WHY: 0006 made the reservation row an outbox and taught the sweep two
-- classes — expired-and-owing-nothing is reclaimed, expired-with-an-intent is
-- quarantined. That split is only sound if "holds no settlement intent" really
-- means "nothing was delivered". It does not. Two paths produce a reservation
-- that was dispatched upstream and yet carries no intent:
--
--   1. the intent write failed permanently (`UsageSession::record` retries
--      transient failures and then gives up with an ERROR log, db.rs), and
--   2. the process died between the upstream answering and the intent commit —
--      the window `record` exists to shrink but cannot close.
--
-- In both, the customer holds inference ZeroRouter paid for. The row is
-- byte-identical to one belonging to a request that was refused before the
-- walk ever dispatched, so the sweep DELETEs it: the encumbrance is released,
-- no usage event is written, no debit is taken, and nothing anywhere records
-- that money was ever owed. The debt is not lost, it is *forgiven*, silently,
-- by a background statement nobody reads. (sol ULTRA review #3, structural
-- half; the transient half was the intent-write retry.)
--
-- SHAPE: one nullable timestamp written at the moment the walk begins its
-- first upstream call. It is deliberately NOT part of any transaction: the
-- request path may not grow a blocking round trip that can fail a request, so
-- the marker is a fire-and-forget UPDATE issued concurrently with the upstream
-- call itself (db.rs, `UsageSession::mark_dispatched`). That makes the marker
-- best-effort by construction, and the residual is stated rather than hidden:
-- a process that dies within its own marker round trip still leaves an
-- unmarked dispatched row. The window it closes is the whole upstream call
-- (seconds to fifteen minutes); the window it leaves open is one statement.
--
-- The sweep therefore reads three classes off an expired row, and the third is
-- new:
--
--   never dispatched, no intent    -> DELETE   (nothing happened; today's
--                                               behavior, and still correct)
--   dispatched, holds an intent    -> quarantine (0006's behavior, unchanged)
--   dispatched, holds NO intent    -> quarantine (new: delivered inference
--                                                 whose charge was lost)
--
-- Quarantine here means visibility, not collection. There is no stored payload
-- to replay, so `recover_owed_settlements` cannot and must not touch these
-- rows; they surface through `zerorouter admin owed-settlements`
-- (`quarantined_settlements`, db.rs) with a NULL owed amount and a stated
-- reason, for a human to reconcile against the provider's own usage records.
--
-- ENCUMBRANCE IS DELIBERATELY UNCHANGED. The admission aggregates count a row
-- while `expires_at > NOW()` OR it carries an intent (0006 + the #2 fix); an
-- expired intentless row — marked dispatched or not — is invisible to them,
-- exactly as before this migration. Holding a customer's balance against a
-- debt of unknown size, with no payload to collect it with and no operator
-- command to release it, would be a permanent unresolvable freeze. Making
-- these rows encumber is a policy decision for the repo owner, not a side
-- effect of adding visibility.
--
-- NO ORDERING CHECK ON `dispatched_at`. `settlement_intent_at` carries a
-- `>= created_at` CHECK (0006) because its writer retries and reports a
-- failure as a lost charge. This column's writer is fire-and-forget and logs a
-- WARN, so a CHECK could only ever convert a harmless clock artifact into a
-- silently missing marker — the exact failure this column exists to prevent.
--
-- PRE-EXISTING RESERVATIONS: rows that predate this migration carry NULL, which
-- reads as "never dispatched" and gives them the pre-0014 sweep answer. No
-- backfill is possible (the fact was never recorded) or wanted.
--
-- REGISTRATION REQUIRED: sqlx migrations are a hardcoded include_str! vec —
-- this file does nothing until the `migrate` Migrator in db.rs gains:
--
--   Migration::new(
--       10,
--       Cow::Borrowed("dispatched reservations"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0014_dispatched_reservations.sql")),
--       false,
--   ),
--
-- Numbered 14: 0009 (dispute freeze) and 0013 (dispute resolution) landed
-- while this was in flight; 0010-0012 remain reserved per the 0013 header.
-- a dense sequence.

ALTER TABLE usage_reservations
    -- When this reservation's request began its first upstream call. NULL
    -- means the walk never dispatched — the request was refused, cancelled, or
    -- ran out of budget before any provider was contacted — and only such a
    -- row may be reclaimed by the expiry sweep.
    ADD COLUMN dispatched_at TIMESTAMPTZ;

-- 0006 could assume a quarantined row always had a payload, because an intent
-- was the only thing that made a row worth parking. A dispatched row with no
-- intent is worth parking precisely BECAUSE it has no payload, so the
-- constraint is widened to accept either kind of evidence. It still refuses a
-- quarantine backed by nothing at all.
ALTER TABLE usage_reservations
    DROP CONSTRAINT usage_reservations_quarantine_requires_intent;

ALTER TABLE usage_reservations
    ADD CONSTRAINT usage_reservations_quarantine_requires_evidence
        CHECK (
            quarantined_at IS NULL
            OR settlement_intent IS NOT NULL
            OR dispatched_at IS NOT NULL
        );

COMMENT ON COLUMN usage_reservations.dispatched_at IS
    'Set (fire-and-forget, best effort) when the walk begins its first upstream call. NULL means nothing was ever dispatched, which is the ONLY condition under which the expiry sweep may reclaim a reservation that holds no settlement intent; a dispatched intentless row is quarantined instead, because it represents inference the customer received whose charge was lost';

-- 0015_released_reservations.sql
--
-- An operator's resolution of a quarantined reservation, recorded ON the row:
-- when it was released and why.
--
-- WHY: 0006 parked a reservation whose settlement could not be recorded, and
-- gave the operator `admin settle-owed --request-id` to collect it — a queue
-- with an exit. 0014 added a second class to that queue: a reservation that
-- reached an upstream and carries NO settlement intent. The amount is
-- unknowable (there is no stored payload), the row does not encumber, and
-- there is nothing to collect, so the one exit 0006 built does not apply to
-- it. It sits in `admin owed-settlements` forever. A queue that can only grow
-- stops being read, and an unread queue is worse than no queue: the owed rows
-- that DO need collecting drown in the ones nobody can act on.
--
-- This is the exit for both classes. `admin disputes release-reservation`
-- resolves a quarantined row against a required note; an owed row additionally
-- demands `--forgive`, because releasing one is ZeroRouter deciding not to
-- collect a debt it CAN collect.
--
-- WHY MARK AND NOT DELETE. Deleting the row and printing JSON at a terminal is
-- not a record — a scrollback is not queryable, and the row is the only place
-- the fact ever lived. Marking keeps the whole history: what was reserved,
-- when it was dispatched, what (if anything) was owed, when the queue gave up
-- on it, and now who released it and why. `credit_ledger` is deliberately NOT
-- where this goes: no resolution here moves a balance by a cent, and a ledger
-- row that moves no money would be the first one, read forever after as though
-- it might have.
--
-- WHAT A RELEASED ROW STOPS DOING. Two things, both in db.rs:
--
--   1. it leaves `quarantined_settlements`, which is the operator queue and
--      the whole point of the exit; and
--   2. it stops encumbering the customer's balance and caps. For an intentless
--      row this changes nothing (such a row never encumbered — see 0014's
--      ENCUMBRANCE IS DELIBERATELY UNCHANGED). For an owed row released under
--      `--forgive` it is the point: the debt the encumbrance was holding
--      credit against is one ZeroRouter has just said it will not collect, and
--      going on holding the customer's money against it would be a freeze with
--      no end condition.
--
-- The debt itself is NOT written off in the ledger, because it was never
-- debited: an unsettled reservation is a charge that was never taken, so there
-- is nothing to forgive in `credit_ledger` and a `writeoff` row here would
-- double-count against the 0013 bad-debt total. What is forgiven is the
-- charge, and the record of it is this row.
--
-- REGISTRATION REQUIRED (same as every migration here — sqlx runs a hardcoded
-- include_str! vec): this file does nothing until db.rs `migrate` gains
--
--   Migration::new(
--       15,
--       Cow::Borrowed("released reservations"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0015_released_reservations.sql")),
--       false,
--   ),
--
-- Numbered 15 because 0013 and 0014 are applied; 0010-0012 remain reserved for
-- branches in flight, per the 0013 header.
--
-- NO NEW INDEX. The queue read is 0006's partial
-- `usage_reservations_quarantined_idx` over the rows that are quarantined at
-- all — a set that is empty in steady state and never large enough for the
-- extra `released_at IS NULL` filter to matter. Released rows stay in that
-- index and are filtered on top of it.

ALTER TABLE usage_reservations
    -- When an operator resolved this quarantined row. NULL is the normal
    -- state; a non-NULL value means the row is a RECORD rather than a queue
    -- entry, and nothing — not the queue, not admission's encumbrance sums,
    -- not the expiry sweep — may act on it again.
    ADD COLUMN released_at TIMESTAMPTZ,
    -- Why. Required with the release rather than defaulted, on the same
    -- reasoning 0013's resolution note carries: a release points at no Stripe
    -- object, no ledger row and no usage event, so this sentence is the entire
    -- explanation of why a debt stopped being pursued.
    ADD COLUMN released_note TEXT;

ALTER TABLE usage_reservations
    -- The fact, the time and the reason arrive together or not at all. A
    -- released_at with no note would be exactly the "released by someone, at
    -- some point, for reasons now lost" record this column exists to prevent.
    ADD CONSTRAINT usage_reservations_release_states_its_reason
        CHECK (
            (released_at IS NULL AND released_note IS NULL)
            OR (released_at IS NOT NULL AND BTRIM(released_note) <> '')
        ),
    -- Only a QUARANTINED row may be released. A live reservation is released
    -- by settling it and an expired one by the sweep; releasing either would
    -- be this command reaching around the paths that exist to charge honestly.
    -- Enforced here as well as in db.rs because it is the property that makes
    -- "released" mean something narrow: nothing in this schema ever clears
    -- `quarantined_at`, so a released row is quarantined for good.
    ADD CONSTRAINT usage_reservations_release_requires_quarantine
        CHECK (released_at IS NULL OR quarantined_at IS NOT NULL);

COMMENT ON COLUMN usage_reservations.released_at IS
    'When an operator resolved this quarantined reservation through `admin disputes release-reservation`. A released row leaves the reconciliation queue and stops encumbering the balance; it is kept, never deleted, because the row is the only durable record that the charge it represents was given up on';
COMMENT ON COLUMN usage_reservations.released_note IS
    'The operator''s stated reason for the release. Required with released_at: a release anchors to no Stripe object and writes no ledger row, so this is the whole record of why';

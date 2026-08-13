-- 0017_stripe_observed_reversals.sql
--
-- A durable tombstone for a Stripe reversal (refund or dispute) that arrived
-- for a PaymentIntent this deployment has NOT yet credited.
--
-- WHY: until this migration, `handle_reversal_event` attributed a
-- `charge.refunded` / `charge.dispute.created` event solely by finding an
-- existing `purchase`/`autopay` row in `credit_ledger`. When the reversal
-- raced ahead of the credit — Stripe delivers events with no ordering
-- guarantee, and the checkout/autopay credit path returns 503 + relies on
-- Stripe redelivery, so there are real windows where no credit row exists yet
-- — the lookup returned NULL and the event was acknowledged with HTTP 200 and
-- NO durable side effect: no reversal, and for a dispute no freeze (the freeze
-- is downstream of that same NULL early-return). The later-arriving success
-- then credited the PaymentIntent with no reversal check, and the customer
-- could spend money that had already been refunded or charged back — a
-- disputed account left funded and unfrozen.
--
-- WHAT THIS FIXES: the reversal is now written down the instant it is observed,
-- keyed on the Stripe reversal OBJECT id (dispute id, or charge id for a
-- refund). When the credit finally lands, the credit path consults this table
-- UNDER THE SAME per-user advisory lock it already holds and converges the
-- account to the exact state a reversal-AFTER-credit would have produced: the
-- net credit reversed (no spendable refunded money) and, for a dispute, the
-- account frozen. The tombstone is then stamped `applied_at` so it is consumed
-- exactly once. "Reversal before credit" and "reversal after credit" now reach
-- the same end state, idempotently, from either delivery order.
--
-- This is additive and does not change the happy path: a reversal that arrives
-- AFTER its credit still finds the credit row and takes the existing
-- reverse-and-freeze path (billing::reverse_purchase / freeze_account), whose
-- own idempotence (the credit_ledger.stripe_session_id unique index plus the
-- per-intent "already reversed" check) also makes a later redelivery of the
-- same reversal a no-op even once this tombstone has been consumed.
--
-- REGISTRATION REQUIRED: sqlx migrations are a hardcoded include_str! vec —
-- this file does nothing until the `migrate` Migrator in db.rs gains
--
--   Migration::new(
--       17,
--       Cow::Borrowed("stripe observed reversals"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0017_stripe_observed_reversals.sql")),
--       false,
--   ),
--
-- Migrations auto-run at startup of both `serve` and `admin`.

-- ============================================================================
-- stripe_observed_reversals: one row per Stripe reversal object observed
-- before it could be attributed to a credit.
--
-- Keyed on the reversal OBJECT id, not the PaymentIntent: that is what makes a
-- redelivered reversal webhook a no-op (INSERT ... ON CONFLICT (object_id) DO
-- NOTHING), exactly as credit_ledger.stripe_session_id (0002) deduplicates a
-- redelivered purchase and a redelivered reversal. A single PaymentIntent can
-- be reversed by more than one object (a dispute AND a later refund of the same
-- charge carry DIFFERENT ids), so the intent id is a non-unique lookup key, not
-- the anchor.
-- ============================================================================

CREATE TABLE stripe_observed_reversals (
    -- The Stripe reversal object id: a Dispute id for a chargeback, a Charge id
    -- for a refund. The idempotence anchor — a redelivery collides here and is
    -- dropped, so an observed reversal is recorded at most once.
    object_id TEXT PRIMARY KEY,
    -- The PaymentIntent the reversal is anchored to. The credit path looks a
    -- tombstone up by this when a credit for the intent is finally applied.
    -- Not unique: two objects can reverse the same intent.
    payment_intent_id TEXT NOT NULL,
    -- Refund vs dispute. A dispute additionally FREEZES the account when the
    -- credit lands, mirroring the normal dispute path; a refund does not.
    is_dispute BOOLEAN NOT NULL,
    -- The reversed amount in the smallest currency unit: the disputed `amount`
    -- for a dispute, the cumulative `amount_refunded` for a refund. NULL when
    -- the event carried no amount. Compared against the credit at consume time
    -- to decide whether the reversal covers the whole credit — the same
    -- "reverse only a full-coverage reversal" rule the webhook applies inline;
    -- a partial or amount-less reversal reverses nothing (an operator
    -- reconciles it) but a dispute still freezes.
    reversed_cents BIGINT,
    -- Lowercase ISO-4217 code the reversal was denominated in, checked
    -- independently of the amount for the same reason the checkout path checks
    -- it: the smallest unit of a zero-decimal currency (1000 JPY) numerically
    -- matches a cents amount ($10.00) while being worth a fraction of it.
    currency TEXT,
    -- When the reversal was first observed (the racing-ahead webhook).
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Stamped when a credit consumed this tombstone and brought the account to
    -- the reversed (and, for a dispute, frozen) end state. NULL = observed but
    -- not yet reconciled against a credit; the partial index below is the
    -- credit path's lookup and the operator's "unreconciled reversals" scan.
    applied_at TIMESTAMPTZ,
    CONSTRAINT stripe_observed_reversals_object_id_is_nonempty
        CHECK (BTRIM(object_id) <> ''),
    CONSTRAINT stripe_observed_reversals_payment_intent_is_nonempty
        CHECK (BTRIM(payment_intent_id) <> ''),
    CONSTRAINT stripe_observed_reversals_applied_after_observed
        CHECK (applied_at IS NULL OR applied_at >= observed_at)
);

-- The credit path's lookup ("is there an unapplied reversal for this
-- intent?") and the operator scan for reversals that never met a credit.
-- Partial over the unapplied rows, which are the only ones either reads.
CREATE INDEX stripe_observed_reversals_unapplied_idx
    ON stripe_observed_reversals (payment_intent_id)
    WHERE applied_at IS NULL;

COMMENT ON TABLE stripe_observed_reversals IS
    'Durable record of a Stripe refund/dispute observed before its PaymentIntent was credited (migration 0017, HIGH-1). Consumed under the per-user advisory lock when the credit lands, converging reversal-before-credit to the same reversed/frozen end state as reversal-after-credit. Keyed on the reversal object id so a redelivered webhook is a no-op';
COMMENT ON COLUMN stripe_observed_reversals.reversed_cents IS
    'Reversed amount in the smallest currency unit (dispute amount, or cumulative refund amount_refunded); compared against the credit to decide full-coverage reversal, matching the webhook rule';
COMMENT ON COLUMN stripe_observed_reversals.applied_at IS
    'Stamped once a credit consumed this tombstone (reversed the credit and, for a dispute, froze the account); NULL means observed but not yet reconciled against a credit';

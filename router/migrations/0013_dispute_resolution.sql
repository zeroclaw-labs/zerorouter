-- 0013_dispute_resolution.sql
--
-- The two ledger entry types the dispute review workflow needs to settle a
-- receivable honestly: `writeoff` (the debt is forgiven, ZeroRouter eats it)
-- and `recovery` (money came back outside Stripe — a dispute won, a wire
-- received).
--
-- WHY NOT `adjustment`: the 0002 vocabulary already carries a catch-all, and
-- both of these rows would fit inside it mechanically. They are not the same
-- fact. A write-off is a LOSS — dollars ZeroRouter has decided it will never
-- collect — and a recovery is a RECEIPT. Filed under one undifferentiated
-- `adjustment` type, "how much have we written off this quarter?" becomes a
-- question you answer by grepping free-text notes, and a bad-debt total that
-- depends on note prose is exactly the confidently-wrong number 0007 was
-- written to stop. They are also constrained differently below, which a shared
-- type could not express at all.
--
-- REGISTRATION REQUIRED (same as every migration here — sqlx runs a hardcoded
-- include_str! vec): this file does nothing until db.rs `migrate` gains
--
--   Migration::new(
--       13,
--       Cow::Borrowed("dispute resolution"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0013_dispute_resolution.sql")),
--       false,
--   ),
--
-- NUMBERED 13, NOT 10: three numbers are reserved for concurrent work in
-- sibling branches. sqlx applies any migration it has not yet run regardless of
-- whether a lower number arrived later, and every statement here is additive
-- DDL that commutes with unrelated migrations, so a 0010 landing after this one
-- has already applied is fine.
--
-- ONE COLLISION HAZARD, stated so the next agent sees it: re-opening the
-- entry-type vocabulary means DROP + ADD of `credit_ledger_entry_type_is_known`
-- (the pattern 0008 established when it added `autopay`). That constraint name
-- is a shared resource. If a migration in 0010-0012 also adds an entry type,
-- the two files will each re-add the constraint from their own idea of the full
-- list and whichever runs LAST silently drops the other's type. Whoever lands
-- second must reconcile the IN list to the union rather than rebasing blind.

-- ============================================================================
-- The vocabulary
-- ============================================================================

ALTER TABLE credit_ledger
    DROP CONSTRAINT credit_ledger_entry_type_is_known;
ALTER TABLE credit_ledger
    ADD CONSTRAINT credit_ledger_entry_type_is_known
        CHECK (entry_type IN ('purchase', 'usage', 'refund', 'promo',
                              'adjustment', 'autopay', 'writeoff', 'recovery'));

-- ============================================================================
-- What a resolution row must look like
-- ============================================================================

ALTER TABLE credit_ledger
    -- Both settle a receivable by moving the balance UP. A negative row of
    -- either type would be a second reversal wearing a resolution's name, and
    -- the reversal path (`refund`, 0009) is the only thing allowed to take
    -- money back — it is also the only path that may drive the balance below
    -- zero, and it declares itself to the overdraft trigger to do it. Keeping
    -- these strictly positive is what lets `billing::write_off_receivable` and
    -- `billing::record_recovery` run WITHOUT that declaration, so the 0009
    -- tripwire keeps its meaning: an overdraft outside a reversal is still an
    -- aborted transaction.
    ADD CONSTRAINT credit_ledger_resolution_entries_are_credits
        CHECK (entry_type NOT IN ('writeoff', 'recovery') OR amount_usd > 0),

    -- Neither is anchored to a Stripe object, and saying so in the schema is
    -- load-bearing rather than tidy. `stripe_session_id` carries a UNIQUE index
    -- (0002:41-43) that the purchase and reversal paths use as their
    -- idempotence key; a resolution row that parked an id there would occupy an
    -- anchor belonging to a Stripe object and could block the reversal of a
    -- later charge. A recovery names its Stripe object, when it has one, in the
    -- note — where it is evidence for a human, not a dedupe key.
    ADD CONSTRAINT credit_ledger_resolution_entries_are_not_stripe_anchored
        CHECK (entry_type NOT IN ('writeoff', 'recovery')
               OR (stripe_session_id IS NULL
                   AND stripe_payment_intent_id IS NULL)),

    -- A resolution is an operator's judgment call, and a judgment call with no
    -- stated reason is not reviewable by the next person to open this account.
    -- Every other entry type can be reconstructed from the Stripe object or the
    -- request it points at; these two point at nothing, so the note is the
    -- entire record of why the money moved.
    ADD CONSTRAINT credit_ledger_resolution_entries_state_their_reason
        CHECK (entry_type NOT IN ('writeoff', 'recovery')
               OR BTRIM(COALESCE(note, '')) <> '');

-- ============================================================================
-- The overdraft tripwire learns the difference between owing and overdrawing
-- ============================================================================
--
-- 0009 replaced the blanket non-negative CHECK with a trigger that refuses any
-- write leaving `credit_balance_usd` below zero unless the transaction declared
-- itself a credit reversal. Its predicate is on the RESULTING VALUE alone, and
-- that is too broad by exactly one case: PAYING A RECEIVABLE DOWN.
--
-- An account reversed to -40 that recovers 15.50 of it lands on -24.50. Nothing
-- was overdrawn — the debt got smaller — but the row is still negative, so the
-- 0009 predicate raised and the partial recovery was impossible. The only ways
-- around it were to let the resolution path claim `credit_reversal = 'on'`
-- (a path that never takes money asserting that it might, which hollows out the
-- tripwire for every reader afterwards) or to forbid partial recovery
-- (an operator holding a customer's half-payment with nowhere to record it).
--
-- What 0009 is actually protecting is stated in its own header: "the paths
-- which must never overdraft do not, so a future change to them fails loudly
-- instead of quietly overdrawing". Overdrawing is taking money that is not
-- there. So the predicate becomes about the DIRECTION of the write, not the
-- sign of its result: a write may not CARRY the balance downward into, or
-- further into, the negative unless it is a declared reversal.
--
-- Nothing 0009 caught stops being caught. A settlement debit that overshoots
-- its reservation still decreases and is still refused, on a positive balance
-- and on a negative one alike; so is an INSERT of a negative opening balance,
-- which has no OLD row to have decreased from and is therefore always refused.
-- The single write newly permitted is one that strictly increases a negative
-- balance — definitionally a repayment, and never a way to spend what is not
-- there.
--
-- Deliberately CREATE OR REPLACE on 0009's function rather than a new trigger:
-- two triggers on one column, one of them a superset of the other, is how the
-- next reader ends up unsure which rule is live.

CREATE OR REPLACE FUNCTION reject_unauthorized_overdraft()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.credit_balance_usd < 0
        AND COALESCE(current_setting('zerorouter.credit_reversal', TRUE), 'off') <> 'on'
        -- An INSERT has no OLD row to have moved down from, so a negative
        -- opening balance is always an overdraft.
        AND (TG_OP = 'INSERT' OR NEW.credit_balance_usd < OLD.credit_balance_usd)
    THEN
        -- Keeps 0009's "cannot go negative" wording, which
        -- `dispute_freeze.rs::only_a_declared_reversal_may_drive_the_balance_negative`
        -- pins, and extends it with the clause the narrowing adds and the
        -- transition that was refused.
        RAISE EXCEPTION
            'users.credit_balance_usd cannot go negative, or further negative, outside a declared credit reversal (user %, % -> %)',
            NEW.id,
            CASE WHEN TG_OP = 'INSERT' THEN NULL ELSE OLD.credit_balance_usd END,
            NEW.credit_balance_usd;
    END IF;
    RETURN NEW;
END;
$$;

COMMENT ON FUNCTION reject_unauthorized_overdraft() IS
    'Successor to the 0003 non-negative CHECK, narrowed by 0013: a write may not carry the balance DOWNWARD into or within the negative unless the transaction has SET LOCAL zerorouter.credit_reversal = ''on'' (billing::reverse_purchase). Paying a receivable down (billing::record_recovery) leaves the balance negative but strictly higher, and is not an overdraft';

COMMENT ON COLUMN credit_ledger.entry_type IS
    'purchase/autopay = Stripe credit; usage = metered debit; refund = a chargeback or refund reversal (0009); promo = granted credit; writeoff = a receivable forgiven by an operator (0013); recovery = money recovered outside Stripe (0013); adjustment = anything else, and deliberately not where a write-off or a recovery belongs';

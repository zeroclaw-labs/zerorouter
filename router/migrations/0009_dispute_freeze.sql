-- 0009_dispute_freeze.sql
--
-- Chargebacks and refunds: durable account-freeze state, and the schema room
-- the ledger reversal needs.
--
-- WHY: until this migration a customer could buy prepaid credit, consume it,
-- file a card dispute, and keep the account. The webhook dispatcher
-- acknowledged every `charge.refunded` / `charge.dispute.*` event without
-- action and nothing in the codebase ever wrote the `refund` ledger entry the
-- 0002 schema already permitted, so Stripe withdrew the money and ZeroRouter's
-- books did not move. Reversal alone is not enough for a dispute: the
-- remaining balance is only part of the exposure, and an account that has just
-- charged back is an account that should stop consuming inference.
--
-- REGISTRATION REQUIRED (same as every migration here — sqlx runs a hardcoded
-- include_str! vec): this file does nothing until db.rs `migrate` gains
--
--   Migration::new(
--       9,
--       Cow::Borrowed("dispute freeze"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0009_dispute_freeze.sql")),
--       false,
--   ),
--
-- SCOPE: this is the freeze and the reversal. The manual-review surface (list
-- flagged accounts, inspect their history, capture or release the receivable)
-- is a deliberate follow-up on the admin CLI. What ships here is the pair that
-- cannot wait — stop the spend, state the debt — plus `admin set-frozen`, so a
-- freeze is never a state only raw SQL can leave.

-- ============================================================================
-- Freeze state
-- ============================================================================

ALTER TABLE users
    -- NULL means "not frozen". A timestamp rather than a boolean because the
    -- review workflow this defers will need to know WHEN, and a frozen account
    -- is a customer-visible event whose age is the first thing an operator
    -- asks about.
    ADD COLUMN frozen_at TIMESTAMPTZ,
    -- Why the freeze exists, so the follow-up review workflow can tell a
    -- chargeback apart from an operator's deliberate hold without joining back
    -- to the ledger. Deliberately a small closed set, not free text.
    ADD COLUMN frozen_reason TEXT;

ALTER TABLE users
    -- The two columns are one fact. A freeze always names its cause, and a
    -- cause without a freeze is a half-written row.
    ADD CONSTRAINT users_freeze_state_is_whole
        CHECK ((frozen_at IS NULL) = (frozen_reason IS NULL)),
    ADD CONSTRAINT users_freeze_reason_is_known
        CHECK (frozen_reason IS NULL OR frozen_reason IN ('dispute', 'operator'));

-- The review workflow's scan ("who is frozen?"). Partial, because the healthy
-- state is the overwhelming majority and never needs to be in this index.
CREATE INDEX users_frozen_idx ON users (frozen_at) WHERE frozen_at IS NOT NULL;

COMMENT ON COLUMN users.frozen_at IS
    'When the account was frozen; NULL = not frozen. A frozen account is refused at admission and cannot mint new API keys, but its history stays readable — freeze blocks spend, not visibility';
COMMENT ON COLUMN users.frozen_reason IS
    'dispute = a Stripe chargeback froze it automatically; operator = a human froze it with `admin set-frozen --on`';

-- ============================================================================
-- The ledger learns the `refund` entry type it has permitted since 0002
-- ============================================================================

-- Purchase-grade discipline, mirroring 0008's autopay clause: a reversal is a
-- DEBIT and must name the Stripe object that caused it. That object id is also
-- the idempotence anchor — the unique index on credit_ledger.stripe_session_id
-- (0002:41-43) is what makes a redelivered dispute event a no-op, exactly as it
-- makes a redelivered checkout event one — so a refund row without it would be
-- a reversal nothing could deduplicate.
ALTER TABLE credit_ledger
    ADD CONSTRAINT credit_ledger_refund_entries_are_anchored_debits
        CHECK (entry_type <> 'refund'
               OR (amount_usd < 0 AND stripe_session_id IS NOT NULL));

COMMENT ON COLUMN credit_ledger.stripe_session_id IS
    'The Stripe object this row is anchored to and deduplicated by: a Checkout Session id for a purchase, a PaymentIntent id for an autopay recharge, and the dispute or charge id for a refund reversal';

-- ============================================================================
-- The balance may go negative, but only for a reversal
-- ============================================================================
--
-- 0003 added a blanket `credit_balance_usd >= 0` CHECK as a backstop under the
-- reserve->settle invariant: settlement clamps its debit to what admission
-- reserved, so a settlement that ever drove the balance negative would be a
-- regression, and the CHECK turned that regression into a failed transaction
-- instead of a silent overdraft.
--
-- A reversal breaks that CHECK's assumption for one specific reason. When a
-- customer has already SPENT the credit a chargeback takes back, the honest
-- balance is negative: that number is the receivable, and it is what the
-- review workflow will act on. Clamping it at zero would silently forgive the
-- debt and erase the only durable record that money is owed.
--
-- So the constraint is not dropped, it is narrowed: an overdraft is refused
-- unless the writing transaction has declared itself a credit reversal by
-- setting `zerorouter.credit_reversal = 'on'` (SET LOCAL, so it cannot leak to
-- the next transaction on a pooled connection). Settlement, admission, promo
-- grants and autopay all still run without that flag and are still backstopped
-- exactly as 0003 left them — a settle that tried to overdraft still aborts.
--
-- This is a REGRESSION TRIPWIRE, not an access control. Any code path could
-- set the flag; the point is that the paths which must never overdraft do not,
-- so a future change to them fails loudly instead of quietly overdrawing.

ALTER TABLE users DROP CONSTRAINT users_credit_balance_is_nonnegative;

CREATE FUNCTION reject_unauthorized_overdraft()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.credit_balance_usd < 0
        AND COALESCE(current_setting('zerorouter.credit_reversal', TRUE), 'off') <> 'on'
    THEN
        RAISE EXCEPTION
            'users.credit_balance_usd cannot go negative outside a declared credit reversal (user %, attempted %)',
            NEW.id, NEW.credit_balance_usd;
    END IF;
    RETURN NEW;
END;
$$;

-- `UPDATE OF credit_balance_usd` keeps this off every users write that is not
-- about money (autopay settings, the Stripe customer id, the freeze columns
-- above): the trigger is only reached when the balance is in the SET list.
CREATE TRIGGER users_reject_unauthorized_overdraft
    BEFORE INSERT OR UPDATE OF credit_balance_usd ON users
    FOR EACH ROW
    EXECUTE FUNCTION reject_unauthorized_overdraft();

COMMENT ON FUNCTION reject_unauthorized_overdraft() IS
    'Successor to the 0003 non-negative CHECK: the balance may only be driven below zero by a transaction that has SET LOCAL zerorouter.credit_reversal = ''on'' (billing::reverse_purchase), so a settlement overdraft is still an error';

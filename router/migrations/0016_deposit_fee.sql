-- 0016_deposit_fee.sql
--
-- A deposit fee on credit purchases. Money-in stops being 1:1: the user picks a
-- CREDIT amount (net, what lands in the ledger), pays a GROSS amount at Stripe
-- (credit + a ~5.5% surcharge, floored so a $5 deposit still clears Stripe's
-- fixed $0.30), and the surcharge never enters the ledger — it is charge-side
-- and non-refundable. The single source of truth for the fee math is
-- stripe.rs:deposit_fee_quote; this migration only widens two invariants that
-- assumed charge == credit.
--
-- REGISTRATION REQUIRED: sqlx migrations are a hardcoded include_str! vec — this
-- file does nothing until the `migrate` Migrator in db.rs gains
-- Migration::new(16, "deposit fee", ...). Migrations auto-run at startup.

-- ============================================================================
-- stripe_checkout_intents: gross (charge) may now exceed net (credit).
--
-- 0005 pinned expected_credit_usd * 100 = expected_amount_cents so the credited
-- dollars and the amount quoted to Stripe could never drift. With a fee the
-- quote (gross, expected_amount_cents) is the credit (net, expected_credit_usd)
-- PLUS the surcharge, so equality becomes "gross covers credit": the amount
-- charged is never LESS than the credit granted, which is the invariant that
-- actually protects the customer and ZeroRouter. Fee revenue for a row is
-- exactly expected_amount_cents - (expected_credit_usd * 100).
--
-- DDL only: the 0005 row/statement immutability triggers guard DML
-- (UPDATE/DELETE/TRUNCATE) on existing rows, not ALTER TABLE, so dropping and
-- re-adding the constraint is unaffected. No existing row is rewritten; every
-- pre-fee row satisfies the relaxed check because it satisfied the old equality.
-- ============================================================================
ALTER TABLE stripe_checkout_intents
    DROP CONSTRAINT stripe_checkout_intents_amounts_agree;
ALTER TABLE stripe_checkout_intents
    ADD CONSTRAINT stripe_checkout_intents_gross_covers_credit
        CHECK (expected_amount_cents >= (expected_credit_usd * 100)::bigint);

COMMENT ON COLUMN stripe_checkout_intents.expected_amount_cents IS
    'Gross smallest-currency-unit quote sent to Stripe (credit + deposit fee), compared against the webhook event''s amount_total; both directions convert through stripe.rs:usd_to_cents. Fee revenue is this minus expected_credit_usd*100';

-- ============================================================================
-- stripe_autopay_intents: separate the GROSS charge from the NET credit.
--
-- Autopay has its own charge+settle path (not credit_purchase), so the fee is
-- applied a second time here. amount_usd stays the NET credit that
-- settle_autopay_intent applies to the balance; the new charge_amount_usd is the
-- GROSS Stripe collected (credit + fee), corroborated against the payment
-- intent's amount_received. Pre-fee rows charged exactly the credit, so the
-- backfill sets charge == credit; the CHECK then forbids a charge below the
-- credit for every future row.
-- ============================================================================
ALTER TABLE stripe_autopay_intents
    ADD COLUMN charge_amount_usd NUMERIC;

-- Backfill existing rows: before the fee, the card was charged exactly the
-- credit. Done before SET NOT NULL so historical rows satisfy it.
UPDATE stripe_autopay_intents
    SET charge_amount_usd = amount_usd
    WHERE charge_amount_usd IS NULL;

ALTER TABLE stripe_autopay_intents
    ALTER COLUMN charge_amount_usd SET NOT NULL;
ALTER TABLE stripe_autopay_intents
    ADD CONSTRAINT stripe_autopay_charge_covers_credit
        CHECK (charge_amount_usd >= amount_usd);

COMMENT ON COLUMN stripe_autopay_intents.amount_usd IS
    'NET credit applied to the balance on a successful charge (settle_autopay_intent)';
COMMENT ON COLUMN stripe_autopay_intents.charge_amount_usd IS
    'GROSS amount charged at Stripe (credit + deposit fee), corroborated against the payment intent amount_received; fee revenue is this minus amount_usd';

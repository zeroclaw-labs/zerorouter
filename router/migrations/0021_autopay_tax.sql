-- 0021_autopay_tax.sql
--
-- Sales tax on autopay top-ups.
--
-- Until now an autopay recharge charged exactly `charge_amount_usd` (the
-- ex-tax gross: credit + deposit fee), because a raw PaymentIntent takes no
-- `automatic_tax` parameter. Checkout has collected tax since the
-- `automatic_tax` work, so the same credits bought two ways collected two
-- different amounts. Autopay now prices tax through the Tax Calculation API
-- and charges `charge_amount_usd + tax_amount_usd`.
--
-- WHAT DOES *NOT* CHANGE, and why it is the whole point:
--
--   amount_usd         the NET credit applied to the balance. Untouched.
--   charge_amount_usd  the EX-TAX gross (credit + fee). Untouched.
--
-- Tax is a THIRD quantity, never folded into either. It is not revenue (it is
-- owed to a taxing jurisdiction) and it is not credit (the buyer gets exactly
-- `amount_usd` of spendable balance whatever the tax was). Keeping it in its
-- own column is what lets fee revenue stay exactly
-- `charge_amount_usd - amount_usd` for taxed and untaxed rows alike, and it
-- mirrors the checkout side, where `stripe_checkout_intents.expected_amount_cents`
-- likewise kept meaning the ex-tax gross. The existing
-- `stripe_autopay_charge_covers_credit` CHECK (charge >= credit) therefore
-- still compares the two ex-tax figures and needs no amendment.
--
-- WHY NULLABLE, AND WHY IT MUST STAY NULLABLE:
--
-- NULL means "no tax has been computed for this claim yet". A stored value —
-- INCLUDING 0 — means "tax was computed and is now FROZEN". The distinction is
-- load-bearing and a NOT NULL DEFAULT 0 would destroy it: with no tax
-- registrations Stripe computes zero tax for everyone, so zero is the common
-- successful answer, and a default of 0 would make "not yet asked" and "asked,
-- and the answer was nothing" indistinguishable.
--
-- Freezing matters because of Stripe's idempotency contract: "the idempotency
-- layer compares incoming parameters to those of the original request and
-- errors if they're not the same". A reconciliation replay re-POSTs a stranded
-- claim under its ORIGINAL idempotency key up to ~20 hours later. If it
-- recomputed the tax and got a different number — a rate change, an address
-- change, or simply a Tax API blip that routed the replay down the untaxed
-- fallback — the replay would carry a different `amount`, Stripe would reject
-- it as an idempotency mismatch, and the claim would be terminal-failed even
-- though the first attempt may already have charged the card. So the first
-- writer freezes both the amount and the calculation id here, and every later
-- attempt reuses them instead of asking Stripe again.
--
-- tax_calculation_id is frozen alongside the amount for the same reason and one
-- more: the tax TRANSACTION recorded after a successful charge must be created
-- from the calculation that actually priced that charge. A fresher calculation
-- would record a different tax figure than the one collected.
--
-- Pre-migration rows keep NULL: they were charged before tax existed on this
-- path, and no backfill can invent a tax that was never collected.
--
-- REGISTRATION REQUIRED: db.rs migration vec gains Migration::new(21, ...).

ALTER TABLE stripe_autopay_intents
    ADD COLUMN tax_amount_usd NUMERIC,
    ADD COLUMN tax_calculation_id TEXT;

-- Tax is never negative. It may legitimately be exactly zero (no registration
-- covers the buyer), which is why this permits 0 but not less.
ALTER TABLE stripe_autopay_intents
    ADD CONSTRAINT stripe_autopay_tax_is_nonnegative
        CHECK (tax_amount_usd IS NULL OR tax_amount_usd >= 0);

COMMENT ON COLUMN stripe_autopay_intents.tax_amount_usd IS
    'Sales tax collected ON TOP of charge_amount_usd, from the Tax Calculation API. NULL = not yet computed for this claim; a value (including 0) = computed and FROZEN, so a reconciliation replay re-POSTs an identical amount under the original idempotency key. Never revenue and never credit: the card is charged charge_amount_usd + tax_amount_usd while the balance still receives exactly amount_usd.';

COMMENT ON COLUMN stripe_autopay_intents.tax_calculation_id IS
    'The Stripe Tax Calculation (taxcalc_...) that priced tax_amount_usd, frozen with it. Used after a successful charge to record the tax transaction that puts the sale in Stripe Tax reports. NULL when no tax was computed (untaxed fallback) or for pre-migration rows.';

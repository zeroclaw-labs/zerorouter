-- 0018_autopay_withheld.sql
--
-- A fourth terminal state for an autopay charge: `withheld`.
--
-- WHY: an autopay charge creates a NEW PaymentIntent and POSTs it off-session.
-- A dispute-freeze on an OLDER intent — and the balance reversal that drives the
-- account into a receivable — can commit DURING that charge's `send().await`
-- window (connection-pool acquisition, DNS/TCP/TLS, transmission, up to the 15s
-- HTTP timeout), after the pre-POST eligibility guard already passed. The charge
-- may then land at Stripe on an account that must not be charged. Crediting that
-- money to a frozen / indebted account is the exact catastrophe migration 0009's
-- freeze exists to prevent (FIX 1).
--
-- `settle_autopay_intent` now re-checks the shared autopay eligibility predicate
-- under the per-user advisory lock it already holds, immediately before applying
-- the credit. When the account is ineligible it WITHHOLDS: no balance credit and
-- no `autopay` ledger row are written, and the intent moves to this `withheld`
-- state instead of `succeeded`. The money WAS collected at Stripe, so it must be
-- refunded — it is durably recorded here and surfaced to an operator
-- (`billing::withheld_autopay_intents`, and a loud per-sweep log) rather than
-- silently kept or credited. The Stripe refund itself is issued OUT OF BAND by
-- an operator, never inline, so the advisory lock is never held across an
-- external HTTP call.
--
-- `withheld` is terminal, exactly like `succeeded` and `failed`: the
-- pending->terminal transition is still the exactly-once guard, so a redelivered
-- success for a withheld intent finds no pending row and is a no-op (it neither
-- double-withholds nor retroactively credits).
--
-- This only widens the status CHECK; it is additive and changes no existing row.
--
-- REGISTRATION REQUIRED (sqlx runs a hardcoded include_str! vec): this file does
-- nothing until db.rs `migrate` gains
--
--   Migration::new(
--       18,
--       Cow::Borrowed("autopay withheld state"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0018_autopay_withheld.sql")),
--       false,
--   ),

ALTER TABLE stripe_autopay_intents
    DROP CONSTRAINT stripe_autopay_status_is_known;
ALTER TABLE stripe_autopay_intents
    ADD CONSTRAINT stripe_autopay_status_is_known
        CHECK (status IN ('pending', 'succeeded', 'failed', 'withheld'));

COMMENT ON COLUMN stripe_autopay_intents.status IS
    'pending -> succeeded (credited), failed (no money moved / declined), or withheld (money collected at Stripe but the account was ineligible at settlement, so the credit was withheld and the charge needs an out-of-band operator refund — migration 0018, FIX 1). Terminal states are the exactly-once credit guard.';

-- 0022_checkout_intent_cleanup.sql
--
-- Let the ABANDONED half of `stripe_checkout_intents` be swept, and make the
-- DATABASE — not merely the sweep's WHERE clause — the thing that decides which
-- half is abandoned.
--
-- 0005 wrote `Rows are never deleted (pruning needs a deliberate migration)`.
-- This is that migration. It exists because the table is append-only in
-- practice while its inputs are not: one row is written per Checkout Session
-- created (`stripe.rs:create_checkout`), and the overwhelming majority of those
-- sessions are never paid. A customer who opens the payment modal, closes it,
-- reconsiders the amount and opens it again leaves rows behind for a purchase
-- that never happened. The 10-minute `SESSION_REUSE_TTL` cache bounded how fast
-- that accumulates; nothing bounded the total.
--
-- WHAT IS *NOT* CHANGED, AND IS THE WHOLE POINT
--
-- A row whose session was PAID is ledger corroboration. `credit_ledger` is
-- append-only and immortal (0002:54-72), and the intent row is the server-side
-- record of what ZeroRouter priced that purchase at — the second of the two
-- preconditions that gate every credit (0005 header). Deleting one would erase
-- the evidence behind a dollar that moved. Such a row must outlive the ledger
-- entry it corroborates, which is to say: forever.
--
-- So the prohibition 0005 installed is NARROWED, as little as it can be, rather
-- than lifted. `reject_stripe_checkout_intent_mutation` is replaced below so
-- that DELETE is permitted for exactly one shape of row, and TRUNCATE and every
-- quote mutation stay refused verbatim.
--
-- THE THREE CONDITIONS, AND WHY EACH IS LOAD-BEARING
--
--   1. `settled_at IS NULL` — the webhook never marked this session delivered.
--      Necessary but NOT sufficient on its own: 0005:127-128 is explicit that
--      the marker is a reconciliation aid, stamped AFTER the credit commits and
--      deliberately non-fatal if it is lost. A crash between the two leaves a
--      row that WAS credited and is still unsettled, so a sweep keyed on this
--      column alone would delete corroboration for real money.
--
--   2. No `credit_ledger` row names this session. This is the authoritative
--      answer to `was this credited?`, because
--      `credit_ledger_stripe_session_unique` (0002:41-43) is the idempotence
--      anchor for the purchase itself — if a credit was ever applied, that row
--      exists and is immortal. Condition 1 can be lost; this one cannot.
--
--   3. The row is at least seven days old. Not tidiness — the money-safety
--      condition for the LATE WEBHOOK. Stripe documents two windows that stack:
--      a Checkout Session expires 24 hours after creation (so a payment can
--      legitimately land as late as `created_at + 24h`), and a webhook delivery
--      is retried `for up to three days with an exponential back off in live
--      mode` (so the last legitimate `checkout.session.completed` for that
--      payment can arrive at `created_at + 4 days`). Delete inside that window
--      and a customer who genuinely paid is refused at the webhook and credited
--      nothing. Seven days is the floor with three days of margin on top.
--
-- WHY THE FLOOR LIVES HERE AND THE OPERATING WINDOW DOES NOT
--
-- Seven days is an INVARIANT: it is derived from Stripe's own guarantees, not
-- from anything ZeroRouter chooses, and no future tuning may go below it
-- without money being at risk. The window the sweep actually runs at is POLICY
-- (`stripe.rs:CHECKOUT_INTENT_RETENTION_DAYS`, currently 30 days) and lives in
-- code, where it can be revised without a migration. The two are ordered on
-- purpose: policy must stay at or above the floor, and if a later edit drops it
-- below, every DELETE the sweep issues aborts on this trigger and the sweep
-- fails loudly instead of quietly deleting money's evidence.
--
-- WHAT THE CUSTOMER SEES AFTERWARDS
--
-- `GET /api/billing/checkout/status` looks the session up here BEFORE it asks
-- Stripe, so a swept session reads as 404 `session_not_found` — the same answer
-- as a session this deployment never priced. The portal renders that as `we
-- could not confirm that payment just now ... the ledger below is the record`,
-- which is true and safe: it neither claims a payment succeeded nor claims one
-- failed. It is reachable only by returning to a checkout tab more than 30 days
-- after opening it, by which time the session has been dead at Stripe for 29 of
-- them. See `stripe.rs:checkout_status`.
--
-- REGISTRATION REQUIRED: db.rs migration vec gains Migration::new(22, ...).

-- Same function, same two triggers (0005:161-169); only the DELETE arm moves.
-- Replaced rather than dropped so both triggers keep pointing at it and the
-- exception messages keep naming this table.
CREATE OR REPLACE FUNCTION reject_stripe_checkout_intent_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    -- TRUNCATE first and unconditionally. It removes rows without ever
    -- evaluating a per-row predicate, so no version of it can be proven to
    -- spare the credited ones; and the statement-level trigger that carries it
    -- has no OLD to inspect, which is why this test has to precede every
    -- reference to OLD below.
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION 'stripe_checkout_intents cannot be truncated';
    END IF;

    IF TG_OP = 'DELETE' THEN
        -- Condition 1. A delivered purchase's quote is permanent.
        IF OLD.settled_at IS NOT NULL THEN
            RAISE EXCEPTION
                'stripe_checkout_intents rows that settled are never removed (%)',
                OLD.stripe_session_id;
        END IF;
        -- Condition 2. The authoritative "was this credited?", independent of
        -- the marker above and immune to a lost stamp. Indexed:
        -- credit_ledger_stripe_session_unique is a partial unique index on
        -- exactly this column, and the equality implies its IS NOT NULL
        -- predicate, so this is one probe per candidate row.
        IF EXISTS (
            SELECT 1 FROM credit_ledger
            WHERE credit_ledger.stripe_session_id = OLD.stripe_session_id
        ) THEN
            RAISE EXCEPTION
                'stripe_checkout_intents rows corroborating a credit ledger entry are never removed (%)',
                OLD.stripe_session_id;
        END IF;
        -- Condition 3. Stripe's own windows: 24h of session life plus three
        -- days of webhook retries is four days in which a payment for this row
        -- can still legitimately arrive. Refuse anything younger than seven.
        IF OLD.created_at > NOW() - INTERVAL '7 days' THEN
            RAISE EXCEPTION
                'stripe_checkout_intents rows are not removable until stripe can no longer complete them (%)',
                OLD.stripe_session_id;
        END IF;
        RETURN OLD;
    END IF;

    -- UPDATE, verbatim from 0005:145-157: the quote can never change and
    -- settlement may advance exactly once from NULL.
    IF NEW.stripe_session_id IS DISTINCT FROM OLD.stripe_session_id
        OR NEW.user_id IS DISTINCT FROM OLD.user_id
        OR NEW.created_at IS DISTINCT FROM OLD.created_at
        OR NEW.expected_amount_cents IS DISTINCT FROM OLD.expected_amount_cents
        OR NEW.expected_credit_usd IS DISTINCT FROM OLD.expected_credit_usd
        OR NEW.currency IS DISTINCT FROM OLD.currency
    THEN
        RAISE EXCEPTION 'stripe_checkout_intents quotes are immutable';
    END IF;
    IF OLD.settled_at IS NOT NULL AND NEW.settled_at IS DISTINCT FROM OLD.settled_at THEN
        RAISE EXCEPTION 'stripe_checkout_intents settlement is final';
    END IF;
    RETURN NEW;
END;
$$;

-- 0005's table comment said rows are never deleted. Half of that is still true
-- and it is the half that matters, so the correction states which half.
COMMENT ON TABLE stripe_checkout_intents IS
    'Server-side record of what each Stripe Checkout Session was priced at; a matching row is a precondition for crediting, and the credited dollars come from this row, never from webhook metadata. A row that was CREDITED is permanent — it is the corroboration behind a credit_ledger purchase entry. A row that was never credited is swept once Stripe can no longer complete its session (migration 0022): unsettled, unreferenced by the ledger, and at least seven days old, enforced by reject_stripe_checkout_intent_mutation rather than by the sweeping query alone';

-- The index the sweep's candidate scan rides on already exists
-- (`stripe_checkout_intents_unsettled_idx` on `(created_at) WHERE settled_at IS
-- NULL`, 0005:117-119). It was built for the operator's "never delivered" scan
-- and matches the sweep's predicate and ordering exactly, so no index is added
-- here; the sweep must never become a sequential scan, and this is the reason it
-- is not one.

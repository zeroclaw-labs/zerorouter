-- 0005_stripe_checkout_intents.sql
--
-- Server-side pending-purchase records for Stripe Checkout: what ZeroRouter
-- INTENDED to sell, written when the Checkout Session is created
-- (stripe.rs:create_checkout) and required to exist, and to match, before a
-- `checkout.session.completed` webhook credits anything
-- (stripe.rs:stripe_webhook).
--
-- WHY: the webhook HMAC proves that STRIPE sent the event. It does not prove
-- that ZEROROUTER priced it. `metadata.credit_usd` and `metadata.user_id` are
-- attacker-chosen on any paid session that exists in the Stripe account — a
-- second integration on the same account, an operational mistake, a leaked
-- restricted API key — and until this table existed the metadata was the sole
-- input to the credited amount. A $1 session carrying
-- `metadata.credit_usd = 1000` was signed legitimately by Stripe, verified
-- correctly, and minted $1000 of inference credit. Two independent
-- preconditions now gate crediting:
--
--   1. the session's own `amount_total` and `currency` must corroborate the
--      metadata (defends a session we DID create against forged metadata), and
--   2. a row here must exist and agree on user, amount, and currency (defends
--      against a session we never created at all).
--
-- The dollars actually credited are read from THIS table, never from the
-- event: the metadata only has to agree, it is never the source of truth.
--
-- REGISTRATION REQUIRED: sqlx migrations are a hardcoded include_str! vec —
-- this file does nothing until the `migrate` Migrator in db.rs (db.rs:178-220,
-- the registration block described at db.rs:103-131) gains:
--
--   Migration::new(
--       5,
--       Cow::Borrowed("stripe checkout intents"),
--       MigrationType::Simple,
--       Cow::Borrowed(include_str!("../migrations/0005_stripe_checkout_intents.sql")),
--       false,
--   ),
--
-- Migrations auto-run at startup of both `serve` (main.rs:66) and `admin`
-- (admin.rs:72-73); no deploy choreography beyond shipping the image.
--
-- PRE-EXISTING SESSIONS: a Checkout Session created before this migration has
-- no row here, so its webhook is REJECTED and credits nothing — deliberately
-- fail-closed. ZeroRouter is a beta deployment with few in-flight sessions and
-- a 24h Checkout expiry, so the exposure is one day of unredeemed purchases,
-- each individually reconcilable by an operator with an 'adjustment' ledger
-- entry against a Stripe dashboard payment. The alternative — credit anything
-- with plausible metadata and log a warning — is precisely the hole this
-- migration exists to close, and a warning is not a control.

-- ============================================================================
-- stripe_checkout_intents: one row per Checkout Session ZeroRouter created.
--
-- Lifecycle, not an append-only ledger (contrast credit_ledger,
-- 0002:54-72): the quote is immutable but `settled_at` advances once, so the
-- trigger below is a targeted immutability guard rather than the blanket
-- UPDATE/DELETE rejection used by the money ledgers. Idempotence of crediting
-- is NOT anchored here — it remains the unique index on
-- credit_ledger.stripe_session_id (0002:41-43), which is what makes a replayed
-- webhook a no-op. `settled_at` is a reconciliation marker, not a lock, and
-- the webhook stamps it AFTER the credit commits so a crash between the two
-- loses only the marker and never the money.
-- ============================================================================

CREATE TABLE stripe_checkout_intents (
    -- Stripe's Checkout Session id ('cs_...'), the same value that anchors
    -- purchase idempotence in credit_ledger.stripe_session_id (0002:41-43).
    stripe_session_id TEXT PRIMARY KEY,
    -- The user the session was created FOR. The webhook requires
    -- metadata.user_id to equal this, and credits THIS id — so forged metadata
    -- cannot redirect someone else's payment into an attacker's balance.
    user_id UUID NOT NULL REFERENCES users (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Exactly the `unit_amount` quoted to Stripe, in the smallest currency
    -- unit (cents for USD), compared against the event's `amount_total`.
    -- Integer by construction: stripe.rs:usd_to_cents is the single conversion
    -- both directions go through, so quote and verification agree by
    -- construction and no float ever touches money.
    expected_amount_cents BIGINT NOT NULL,
    -- The decimal-dollar credit this session buys and the amount the webhook
    -- actually applies. Redundant with expected_amount_cents by the CHECK
    -- below; stored explicitly so the credited value is a stored server
    -- decision rather than a webhook-time reconstruction.
    expected_credit_usd NUMERIC NOT NULL,
    -- Lowercase ISO-4217 code the session was priced in ('usd'). Checked
    -- independently of the amount: 1000 JPY and $10.00 are both "1000" in the
    -- smallest unit, so an amount match alone is not proof of the price.
    currency TEXT NOT NULL,
    -- Stamped when the webhook credited this session. NULL means either
    -- unpaid/abandoned (Checkout Sessions expire after 24h) or paid-but-not-
    -- yet-delivered; the partial index below is the reconciliation scan.
    settled_at TIMESTAMPTZ,
    CONSTRAINT stripe_checkout_intents_session_id_is_nonempty
        CHECK (BTRIM(stripe_session_id) <> ''),
    CONSTRAINT stripe_checkout_intents_amount_is_positive
        CHECK (expected_amount_cents > 0),
    CONSTRAINT stripe_checkout_intents_credit_is_positive
        CHECK (expected_credit_usd > 0),
    -- The two amount representations can never disagree, so neither the
    -- amount_total comparison nor the credited dollars can drift from the
    -- other.
    CONSTRAINT stripe_checkout_intents_amounts_agree
        CHECK (expected_credit_usd * 100 = expected_amount_cents),
    CONSTRAINT stripe_checkout_intents_currency_is_iso4217_lowercase
        CHECK (currency ~ '^[a-z]{3}$'),
    CONSTRAINT stripe_checkout_intents_settle_after_creation
        CHECK (settled_at IS NULL OR settled_at >= created_at)
);

-- Per-user purchase reconciliation ("what did this user start paying for?").
CREATE INDEX stripe_checkout_intents_user_created_idx
    ON stripe_checkout_intents (user_id, created_at DESC);

-- Operator scan for sessions that were never delivered: abandoned checkouts
-- and — the case worth alerting on — anything paid at Stripe whose webhook
-- never settled here.
CREATE INDEX stripe_checkout_intents_unsettled_idx
    ON stripe_checkout_intents (created_at)
    WHERE settled_at IS NULL;

COMMENT ON TABLE stripe_checkout_intents IS
    'Server-side record of what each Stripe Checkout Session was priced at; a matching row is a precondition for crediting, and the credited dollars come from this row, never from webhook metadata. Rows are never deleted (pruning needs a deliberate migration) — they are the audit trail for money that was quoted';
COMMENT ON COLUMN stripe_checkout_intents.expected_amount_cents IS
    'Smallest-currency-unit quote sent to Stripe, compared against the webhook event''s amount_total; both directions convert through stripe.rs:usd_to_cents';
COMMENT ON COLUMN stripe_checkout_intents.currency IS
    'Checked independently of the amount: the smallest unit of a zero-decimal currency (1000 JPY) can numerically match a cents amount ($10.00) while being worth a fraction of it';
COMMENT ON COLUMN stripe_checkout_intents.settled_at IS
    'Reconciliation marker stamped after the credit commits; NOT the idempotence anchor — that is the unique index on credit_ledger.stripe_session_id (0002:41-43)';

-- Same immutability discipline as the money ledgers (pattern of 0001:106-124),
-- narrowed to fit a lifecycle row: the quote can never change and a row can
-- never be removed, but settlement may advance exactly once from NULL. Own
-- function so the exception names this table.
CREATE FUNCTION reject_stripe_checkout_intent_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    -- DELETE and TRUNCATE are refused outright; the RAISE aborts before any
    -- OLD/NEW reference is evaluated, which is what makes one function usable
    -- from both the row and the statement trigger.
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'stripe_checkout_intents rows are never removed';
    END IF;
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

CREATE TRIGGER stripe_checkout_intents_reject_quote_mutation
    BEFORE UPDATE OR DELETE ON stripe_checkout_intents
    FOR EACH ROW
    EXECUTE FUNCTION reject_stripe_checkout_intent_mutation();

CREATE TRIGGER stripe_checkout_intents_reject_truncate
    BEFORE TRUNCATE ON stripe_checkout_intents
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_stripe_checkout_intent_mutation();

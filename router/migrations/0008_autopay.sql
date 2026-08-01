-- 0008_autopay.sql
--
-- Auto-recharge: users opt in with a threshold and top-up amount; a
-- background sweep charges their saved Stripe payment method off-session
-- when the prepaid balance crosses the threshold. Nothing here touches the
-- request path: admission still reads the balance the ledger already
-- maintains, and a failed or absent autopay simply leaves the existing
-- insufficient-credits behavior in place.
--
-- REGISTRATION REQUIRED: db.rs migration vec gains Migration::new(8, ...).

ALTER TABLE users
    ADD COLUMN autopay_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN autopay_threshold_usd NUMERIC,
    ADD COLUMN autopay_topup_usd NUMERIC,
    -- Consecutive off-session charge failures; three in a row disables
    -- autopay rather than retrying a dead card forever. Reset on success.
    ADD COLUMN autopay_consecutive_failures INTEGER NOT NULL DEFAULT 0;

ALTER TABLE users
    ADD CONSTRAINT users_autopay_threshold_is_nonnegative
        CHECK (autopay_threshold_usd IS NULL OR autopay_threshold_usd >= 0),
    ADD CONSTRAINT users_autopay_topup_is_positive
        CHECK (autopay_topup_usd IS NULL OR autopay_topup_usd > 0),
    -- Enabled implies fully configured: both amounts set and a Stripe
    -- customer to charge. The card itself lives at Stripe; its absence
    -- surfaces as a charge failure, counted and capped above.
    ADD CONSTRAINT users_autopay_enabled_is_configured
        CHECK (NOT autopay_enabled
               OR (autopay_threshold_usd IS NOT NULL
                   AND autopay_topup_usd IS NOT NULL
                   AND stripe_customer_id IS NOT NULL));

-- One row per Stripe PaymentIntent. The PK plus the pending->terminal
-- status transition is the exactly-once guard for crediting; the partial
-- unique index keeps the sweep from stacking charges on a slow card.
CREATE TABLE stripe_autopay_intents (
    payment_intent_id TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users (id),
    amount_usd NUMERIC NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT stripe_autopay_amount_is_positive CHECK (amount_usd > 0),
    CONSTRAINT stripe_autopay_status_is_known
        CHECK (status IN ('pending', 'succeeded', 'failed'))
);

CREATE UNIQUE INDEX stripe_autopay_one_pending_per_user
    ON stripe_autopay_intents (user_id)
    WHERE status = 'pending';

-- The ledger learns the entry type, with purchase-grade discipline: an
-- autopay entry is a credit and must name its Stripe payment intent.
ALTER TABLE credit_ledger
    DROP CONSTRAINT credit_ledger_entry_type_is_known;
ALTER TABLE credit_ledger
    ADD CONSTRAINT credit_ledger_entry_type_is_known
        CHECK (entry_type IN ('purchase', 'usage', 'refund', 'promo',
                              'adjustment', 'autopay')),
    ADD CONSTRAINT credit_ledger_autopay_entries_are_credits
        CHECK (entry_type <> 'autopay'
               OR (amount_usd > 0 AND stripe_session_id IS NOT NULL));

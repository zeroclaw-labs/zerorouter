-- 0025_redemption_tax.sql
--
-- Redemption-time sales tax: the dormant mechanism for the stored-value
-- determination.
--
-- Whether tax on prepaid credits is due when they are BOUGHT or when they are
-- SPENT is genuinely unsettled (see `# Sales tax` in router/src/stripe.rs:
-- Massachusetts has authority pointing both ways, and nothing addresses
-- per-token AI APIs). Today the architecture can only collect at purchase —
-- redemption is a metered balance debit with no Stripe object and no tax
-- computation — so selecting a stored-value tax code in the dashboard, which
-- legally defers tax to redemption, would in practice defer it to a point
-- that collects nothing anywhere. This migration builds the redemption side
-- so that determination becomes CHOOSABLE. The code that uses it ships
-- disabled (ZEROROUTER_REDEMPTION_TAX, default off) and changes nothing
-- until the operator — on an accountant's or DOR determination — flips the
-- dashboard tax code to stored-value AND enables collection here. Running
-- both purchase-time and redemption-time taxation at once would tax the same
-- dollar twice; the flip procedure in DEPLOY.md is explicit about ordering.
--
-- WHAT THE SCHEMA ADDS, in the order a dollar moves through it:
--
-- 1. A durable buyer location on `users`. Checkout collects a full billing
--    address (billing_address_collection=required, PR #82) but nothing kept
--    it, and the Tax API reads only what it is explicitly handed. The webhook
--    now stores the completed session's address here — capture runs even
--    while the mechanism is off, so the day the flip happens the addresses
--    already exist. Last write wins; NULL means the user has never completed
--    a checkout since this migration.
--
-- 2. Enrollment and the opening-balance exemption on `users`. Credits bought
--    BEFORE the flip were taxed at purchase; taxing their redemption too
--    would tax the same dollar twice. At enrollment (a user's first sweep
--    pass with the mechanism on) the user's then-current balance is recorded
--    as an exemption that redemption consumes first: only usage beyond it —
--    necessarily funded by post-flip, untaxed deposits — is taxable. Users
--    created after activation get a zero exemption: everything they ever
--    deposited was bought untaxed. `redemption_tax_from_ledger_id` is the
--    ledger cursor enrollment starts from; usage at or before it predates the
--    flip and is never taxed. Promotional credit held at enrollment sits
--    inside the exemption — granted free and never taxed at purchase; whether
--    its redemption is taxable at all is an accountant question the sweep
--    deliberately does not answer (an exemption errs toward not collecting).
--
-- 3. `redemption_tax_periods`: one row per user per swept span of usage,
--    bounded by credit_ledger ids — (from_ledger_id, through_ledger_id], an
--    exclusive/inclusive pair, so consecutive periods provably partition the
--    ledger and a usage row can never be summed twice. Aggregation is the
--    load-bearing choice: settlement debits are thousands of sub-cent rows
--    on a hot, advisory-locked money path, and a Tax API call per debit
--    would put network latency and per-calculation cost inside it. One
--    calculation per user per sweep span keeps the request path byte-for-byte
--    untouched.
--
--    The lifecycle columns follow the 0021/0024 discipline: NULL means "not
--    yet", a stored value — including zero — is FROZEN, and each stamp is
--    what stops the sweep repeating that step. `tax_usd` freezes what the
--    calculation answered; `collected_usd` / `shortfall_usd` record what the
--    balance could actually cover (the debit is clamped — the balance CHECK
--    stays intact and the vendor absorbs what a drained balance cannot pay,
--    which is also what the tax owed does not depend on); `fallback_reason`
--    says why a period froze untaxed without a calculation.
--
-- 4. The ledger learns `tax`: the debit that moves collected redemption tax
--    out of the spendable balance. A debit like `usage`, anchored like
--    nothing else — its home is the period row (`collection_ledger_id`
--    points back), and `request_id` stays NULL by the 0002 usage constraint.
--
-- REGISTRATION REQUIRED: db.rs migration vec gains Migration::new(25, ...).

ALTER TABLE users
    ADD COLUMN tax_address_country TEXT,
    ADD COLUMN tax_address_postal_code TEXT,
    ADD COLUMN tax_address_state TEXT,
    ADD COLUMN tax_address_city TEXT,
    ADD COLUMN tax_address_line1 TEXT,
    ADD COLUMN tax_address_updated_at TIMESTAMPTZ,
    ADD COLUMN redemption_tax_enrolled_at TIMESTAMPTZ,
    ADD COLUMN redemption_tax_from_ledger_id BIGINT,
    ADD COLUMN redemption_tax_exempt_remaining_usd NUMERIC,
    ADD CONSTRAINT users_redemption_tax_enrollment_is_complete
        CHECK ((redemption_tax_enrolled_at IS NULL)
               = (redemption_tax_from_ledger_id IS NULL)
           AND (redemption_tax_enrolled_at IS NULL)
               = (redemption_tax_exempt_remaining_usd IS NULL)),
    ADD CONSTRAINT users_redemption_tax_exemption_is_nonnegative
        CHECK (redemption_tax_exempt_remaining_usd IS NULL
               OR redemption_tax_exempt_remaining_usd >= 0);

ALTER TABLE credit_ledger
    DROP CONSTRAINT credit_ledger_entry_type_is_known;
ALTER TABLE credit_ledger
    ADD CONSTRAINT credit_ledger_entry_type_is_known
        CHECK (entry_type IN ('purchase', 'usage', 'refund', 'promo',
                              'adjustment', 'autopay', 'writeoff', 'recovery',
                              'tax')),
    ADD CONSTRAINT credit_ledger_tax_entries_are_debits
        CHECK (entry_type <> 'tax' OR amount_usd < 0);

CREATE TABLE redemption_tax_periods (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users (id),
    -- The swept ledger span: (from_ledger_id, through_ledger_id]. `from` is
    -- the previous period's `through` (or the enrollment cursor), so spans
    -- tile without gaps or overlap; the unique index below refuses a racing
    -- duplicate outright and the sweep re-reads the cursor under the user's
    -- advisory lock before inserting.
    from_ledger_id BIGINT NOT NULL,
    through_ledger_id BIGINT NOT NULL,
    -- What the span's usage rows sum to (negated: a positive dollar figure),
    -- and how it splits against the opening-balance exemption.
    usage_usd NUMERIC NOT NULL,
    exempt_usd NUMERIC NOT NULL,
    taxable_usd NUMERIC NOT NULL,
    -- Why this period froze untaxed without asking Stripe (no usable address,
    -- or a taxable amount below one cent). NULL for calculated periods.
    fallback_reason TEXT,
    -- The frozen answer (0021 discipline: NULL = not yet asked; a value,
    -- including 0, is frozen and every later step reuses it).
    tax_usd NUMERIC,
    tax_calculation_id TEXT,
    -- Collection: what the clamped debit actually took, what it could not,
    -- and the ledger row that took it. `debited_at` is the stamp that stops
    -- the sweep collecting twice; it is set for zero-tax periods too, with a
    -- zero collected figure and no ledger row.
    collected_usd NUMERIC,
    shortfall_usd NUMERIC,
    collection_ledger_id BIGINT REFERENCES credit_ledger (id),
    debited_at TIMESTAMPTZ,
    -- Filing: the recorded Stripe tax transaction (0024 discipline — the id
    -- must be stored at record time; it cannot be looked up later).
    tax_transaction_id TEXT,
    tax_recorded_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT redemption_tax_span_is_forward
        CHECK (through_ledger_id > from_ledger_id),
    CONSTRAINT redemption_tax_usage_is_positive
        CHECK (usage_usd > 0),
    CONSTRAINT redemption_tax_split_adds_up
        CHECK (exempt_usd >= 0 AND taxable_usd >= 0
               AND exempt_usd + taxable_usd = usage_usd),
    CONSTRAINT redemption_tax_collection_is_clamped
        CHECK (collected_usd IS NULL
               OR (collected_usd >= 0 AND shortfall_usd >= 0
                   AND collected_usd + shortfall_usd = tax_usd))
);

CREATE UNIQUE INDEX redemption_tax_periods_one_per_span
    ON redemption_tax_periods (user_id, through_ledger_id);

-- The sweep's three work queues, each a partial index so a fully processed
-- period costs nothing to skip. Keyed on `updated_at`, not `created_at`,
-- because a failed attempt bumps `updated_at`: the batched sweep would
-- otherwise retry the same oldest stuck rows forever (a user who never
-- returns to leave an address, a calculation Stripe keeps refusing) and
-- starve every period created after them.
CREATE INDEX redemption_tax_periods_uncalculated
    ON redemption_tax_periods (updated_at)
    WHERE tax_usd IS NULL;
CREATE INDEX redemption_tax_periods_uncollected
    ON redemption_tax_periods (updated_at)
    WHERE tax_usd IS NOT NULL AND debited_at IS NULL;
CREATE INDEX redemption_tax_periods_unrecorded
    ON redemption_tax_periods (updated_at)
    WHERE debited_at IS NOT NULL
      AND tax_calculation_id IS NOT NULL
      AND tax_recorded_at IS NULL;

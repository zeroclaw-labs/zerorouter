-- Billing (prepaid credits + append-only ledger), portal sessions, OIDC login
-- state, and RFC 8628 device authorizations.

ALTER TABLE users
    ADD COLUMN credit_balance_usd NUMERIC NOT NULL DEFAULT 0;
ALTER TABLE users
    ADD COLUMN stripe_customer_id TEXT;

CREATE UNIQUE INDEX users_stripe_customer_id_unique
    ON users (stripe_customer_id)
    WHERE stripe_customer_id IS NOT NULL;

COMMENT ON COLUMN users.credit_balance_usd IS
    'Prepaid credit balance; every change must be paired with a credit_ledger row in the same transaction';

CREATE TABLE credit_ledger (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    entry_type TEXT NOT NULL,
    amount_usd NUMERIC NOT NULL,
    balance_after_usd NUMERIC NOT NULL,
    stripe_session_id TEXT,
    stripe_payment_intent_id TEXT,
    request_id UUID,
    note TEXT,
    CONSTRAINT credit_ledger_entry_type_is_known
        CHECK (entry_type IN ('purchase', 'usage', 'refund', 'promo', 'adjustment')),
    CONSTRAINT credit_ledger_amount_is_nonzero
        CHECK (amount_usd <> 0),
    CONSTRAINT credit_ledger_usage_entries_are_debits
        CHECK (entry_type <> 'usage' OR amount_usd < 0),
    CONSTRAINT credit_ledger_purchase_entries_are_credits
        CHECK (entry_type <> 'purchase' OR amount_usd > 0),
    CONSTRAINT credit_ledger_usage_references_request
        CHECK ((entry_type = 'usage') = (request_id IS NOT NULL)),
    CONSTRAINT credit_ledger_purchase_references_stripe_session
        CHECK (entry_type <> 'purchase' OR stripe_session_id IS NOT NULL)
);

CREATE UNIQUE INDEX credit_ledger_stripe_session_unique
    ON credit_ledger (stripe_session_id)
    WHERE stripe_session_id IS NOT NULL;

CREATE UNIQUE INDEX credit_ledger_request_unique
    ON credit_ledger (request_id)
    WHERE request_id IS NOT NULL;

CREATE INDEX credit_ledger_user_created_idx
    ON credit_ledger (user_id, created_at DESC);

COMMENT ON TABLE credit_ledger IS 'Append-only money ledger; balance_after_usd snapshots users.credit_balance_usd inside the same transaction';

CREATE FUNCTION reject_credit_ledger_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'credit_ledger is append-only';
    RETURN NULL;
END;
$$;

CREATE TRIGGER credit_ledger_reject_update_or_delete
    BEFORE UPDATE OR DELETE ON credit_ledger
    FOR EACH ROW
    EXECUTE FUNCTION reject_credit_ledger_mutation();

CREATE TRIGGER credit_ledger_reject_truncate
    BEFORE TRUNCATE ON credit_ledger
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_credit_ledger_mutation();

CREATE TABLE portal_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users (id),
    token_hash CHAR(64) NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    CONSTRAINT portal_sessions_hash_is_sha256_hex
        CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    CONSTRAINT portal_sessions_expire_after_creation
        CHECK (expires_at > created_at)
);

CREATE INDEX portal_sessions_user_id_idx ON portal_sessions (user_id);
CREATE INDEX portal_sessions_expiration_idx ON portal_sessions (expires_at);

CREATE TABLE oidc_states (
    state TEXT PRIMARY KEY,
    nonce TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT oidc_states_state_is_nonempty
        CHECK (BTRIM(state) <> ''),
    CONSTRAINT oidc_states_expire_after_creation
        CHECK (expires_at > created_at)
);

CREATE INDEX oidc_states_expiration_idx ON oidc_states (expires_at);

CREATE TABLE device_authorizations (
    id UUID PRIMARY KEY,
    device_code_hash CHAR(64) NOT NULL UNIQUE,
    user_code TEXT NOT NULL UNIQUE,
    client_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    user_id UUID REFERENCES users (id),
    api_key_id UUID REFERENCES api_keys (id),
    key_name TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_polled_at TIMESTAMPTZ,
    poll_interval_secs INTEGER NOT NULL DEFAULT 5,
    CONSTRAINT device_authorizations_hash_is_sha256_hex
        CHECK (device_code_hash ~ '^[0-9a-f]{64}$'),
    CONSTRAINT device_authorizations_status_is_known
        CHECK (status IN ('pending', 'approved', 'denied', 'claimed', 'expired')),
    CONSTRAINT device_authorizations_user_code_is_nonempty
        CHECK (BTRIM(user_code) <> ''),
    CONSTRAINT device_authorizations_client_id_is_nonempty
        CHECK (BTRIM(client_id) <> ''),
    CONSTRAINT device_authorizations_expire_after_creation
        CHECK (expires_at > created_at),
    CONSTRAINT device_authorizations_poll_interval_is_positive
        CHECK (poll_interval_secs > 0),
    -- Approval attaches the approving user; a minted key requires an approval.
    CONSTRAINT device_authorizations_approved_states_have_user
        CHECK (status NOT IN ('approved', 'claimed') OR user_id IS NOT NULL),
    CONSTRAINT device_authorizations_claimed_states_have_key
        CHECK (status <> 'claimed' OR api_key_id IS NOT NULL)
);

CREATE INDEX device_authorizations_expiration_idx
    ON device_authorizations (expires_at);

COMMENT ON TABLE device_authorizations IS
    'RFC 8628 device-authorization grants; the API key is minted at claim time so plaintext never rests in the database';

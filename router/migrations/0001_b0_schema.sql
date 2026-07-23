CREATE TABLE users (
    id UUID PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    oidc_issuer TEXT,
    oidc_subject TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT users_email_is_nonempty
        CHECK (BTRIM(email) <> ''),
    CONSTRAINT users_oidc_identity_is_complete
        CHECK ((oidc_issuer IS NULL) = (oidc_subject IS NULL)),
    CONSTRAINT users_oidc_issuer_is_nonempty
        CHECK (oidc_issuer IS NULL OR BTRIM(oidc_issuer) <> ''),
    CONSTRAINT users_oidc_subject_is_nonempty
        CHECK (oidc_subject IS NULL OR BTRIM(oidc_subject) <> '')
);

CREATE UNIQUE INDEX users_oidc_identity_unique
    ON users (oidc_issuer, oidc_subject)
    WHERE oidc_issuer IS NOT NULL AND oidc_subject IS NOT NULL;

CREATE TABLE api_keys (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users (id),
    key_hash CHAR(64) NOT NULL UNIQUE,
    name TEXT NOT NULL,
    spend_cap_usd NUMERIC NOT NULL DEFAULT 20,
    velocity_cap_tokens_per_min INTEGER NOT NULL DEFAULT 100000,
    disabled BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ,
    CONSTRAINT api_keys_hash_is_sha256_hex
        CHECK (key_hash ~ '^[0-9a-f]{64}$'),
    CONSTRAINT api_keys_name_is_nonempty
        CHECK (BTRIM(name) <> ''),
    CONSTRAINT api_keys_spend_cap_is_nonnegative
        CHECK (spend_cap_usd >= 0),
    CONSTRAINT api_keys_velocity_cap_is_positive
        CHECK (velocity_cap_tokens_per_min > 0)
);

CREATE INDEX api_keys_user_id_idx ON api_keys (user_id);

CREATE TABLE usage_reservations (
    id UUID PRIMARY KEY,
    api_key_id UUID NOT NULL REFERENCES api_keys (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    reserved_tokens BIGINT NOT NULL,
    reserved_cost_usd NUMERIC NOT NULL,
    CONSTRAINT usage_reservations_expire_after_creation
        CHECK (expires_at > created_at),
    CONSTRAINT usage_reservations_tokens_are_nonnegative
        CHECK (reserved_tokens >= 0),
    CONSTRAINT usage_reservations_cost_is_nonnegative
        CHECK (reserved_cost_usd >= 0)
);

CREATE INDEX usage_reservations_key_expiration_idx
    ON usage_reservations (api_key_id, expires_at);

CREATE INDEX usage_reservations_expiration_idx
    ON usage_reservations (expires_at);

COMMENT ON TABLE usage_reservations IS 'Canonical in-flight cap reservations; settled into usage_events';

CREATE TABLE usage_events (
    id BIGSERIAL PRIMARY KEY,
    request_id UUID NOT NULL UNIQUE,
    api_key_id UUID NOT NULL REFERENCES api_keys (id),
    ts TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    tier TEXT NOT NULL,
    upstream_provider TEXT NOT NULL,
    upstream_model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    cached_input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cost_usd NUMERIC NOT NULL,
    latency_ms INTEGER NOT NULL,
    status SMALLINT NOT NULL,
    CONSTRAINT usage_events_input_tokens_are_nonnegative
        CHECK (input_tokens >= 0),
    CONSTRAINT usage_events_cached_tokens_are_nonnegative
        CHECK (cached_input_tokens >= 0),
    CONSTRAINT usage_events_cached_tokens_do_not_exceed_input
        CHECK (cached_input_tokens <= input_tokens),
    CONSTRAINT usage_events_output_tokens_are_nonnegative
        CHECK (output_tokens >= 0),
    CONSTRAINT usage_events_cost_is_nonnegative
        CHECK (cost_usd >= 0),
    CONSTRAINT usage_events_latency_is_nonnegative
        CHECK (latency_ms >= 0),
    CONSTRAINT usage_events_tier_is_nonempty
        CHECK (BTRIM(tier) <> ''),
    CONSTRAINT usage_events_upstream_provider_is_nonempty
        CHECK (BTRIM(upstream_provider) <> ''),
    CONSTRAINT usage_events_upstream_model_is_nonempty
        CHECK (BTRIM(upstream_model) <> '')
);

CREATE INDEX usage_events_key_timestamp_idx
    ON usage_events (api_key_id, ts DESC);

COMMENT ON TABLE usage_events IS 'Append-only request metering ledger';
COMMENT ON COLUMN usage_events.cost_usd IS 'Customer sell-price debit derived from the canonical tier rate, not provider invoice cost';

CREATE FUNCTION reject_usage_event_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'usage_events is append-only';
    RETURN NULL;
END;
$$;

CREATE TRIGGER usage_events_reject_update_or_delete
    BEFORE UPDATE OR DELETE ON usage_events
    FOR EACH ROW
    EXECUTE FUNCTION reject_usage_event_mutation();

CREATE TRIGGER usage_events_reject_truncate
    BEFORE TRUNCATE ON usage_events
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_usage_event_mutation();

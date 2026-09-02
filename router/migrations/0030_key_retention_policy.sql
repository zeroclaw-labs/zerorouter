-- The per-key retention switch (hybrid provider routing, phase 1).
--
-- One column on `api_keys`. It is the standing default a key's routed requests
-- run under, so "zero-data-retention only" is something a customer sets once on
-- the key and relies on, rather than a flag every caller must remember on every
-- request.
--
-- # What it governs, and what it deliberately does not
--
-- The switch governs a UNIFIED (routed) model id — a bare id like
-- `gemini-3.7-flash` that resolves to the same model across several providers,
-- retention-partitioned. Under `zdr_only` such a request is eligible only for
-- the zero-retention providers of that model and FAILS CLOSED (its own 503,
-- refused before any reservation) when none is available, rather than being
-- silently routed to a provider that retains the prompt. Under `allow_non_zdr`
-- the standard-retention providers become eligible too, always ordered below
-- the zero ones.
--
-- It does NOT govern an explicitly provider-pinned id (`anthropic/…`,
-- `bedrock/…`): pinning one lane by name is a deliberate, auditable choice to
-- address exactly that provider, so a pin is served as itself whatever the
-- switch says — see the design doc, "Unified id vs explicit alias" and "The
-- switch". The switch is the default for the routed layer, not a filter over
-- direct addressing.
--
-- # Why NOT NULL DEFAULT, and why the default is the strict end
--
-- Unlike migration 0023's key knobs, this column is NOT NULL with a default,
-- and the default is the SAFE, RESTRICTIVE value: `zdr_only`. Zero-data
-- retention by default is the product — a key created today is ZDR-only without
-- anyone setting anything, and there is no "unset" state that would read as a
-- quietly weaker guarantee. Every key that exists today is backfilled to
-- `zdr_only` in this migration, so the switch can only ever be LOOSENED from
-- its safe default by a deliberate flip to `allow_non_zdr`; it never tightens a
-- key that was relying on standard routing, because no such key exists.
--
-- `api_keys` is mutable operational state (a key is disabled, a default is
-- changed), not an append-only ledger, so a backfilling DEFAULT is the right
-- tool and there is no trigger to keep in sync — contrast the derived spend
-- counters in 0023, which are append-only-derived and cannot carry a DEFAULT.
ALTER TABLE api_keys
    ADD COLUMN retention_policy TEXT NOT NULL DEFAULT 'zdr_only';

COMMENT ON COLUMN api_keys.retention_policy IS
    'The key''s standing retention switch for routed (unified) model ids: zdr_only (default) serves only zero-retention providers and fails closed when none is available; allow_non_zdr also permits standard-retention providers, ordered below the zero ones. Does not affect explicitly provider-pinned ids.';

-- The keyword set the router switches on, spelled here as well as in Rust so a
-- hand-written UPDATE cannot park a key on a value the router has no branch for
-- — which would read as a configured policy and enforce something undefined.
-- Same discipline as 0023's `credit_limit_window` CHECK and 0004's
-- `default_priority` CHECK.
ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_retention_policy_is_known
        CHECK (retention_policy IN ('zdr_only', 'allow_non_zdr'));

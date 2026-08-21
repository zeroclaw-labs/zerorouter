-- 0026_byok_provider_keys.sql
--
-- Bring-your-own-key: a customer attaches their own upstream provider
-- credential, ZeroRouter dispatches on it, and charges 5% of what the same
-- usage would have cost at catalog rates.
--
-- Two things land here: the table holding the sealed credentials, and one
-- nullable column on `usage_events` recording which requests were served that
-- way.
--
-- # The first secret in this database that is not a digest
--
-- Every other secret ZeroRouter stores is a SHA-256 digest — inference keys,
-- session tokens, device codes — because ZeroRouter only ever needs to answer
-- "is this the same string I was given?". `docs/SECURITY.md` states that as a
-- flat guarantee, and this migration is the exception to it.
--
-- A BYOK credential must be REPLAYED to Anthropic or OpenAI on the customer's
-- behalf, so it has to come back out. It is therefore sealed, not hashed:
-- AES-256-GCM under a data key that is itself sealed under `BYOK_ENCRYPTION_KEY`,
-- which lives in the deployment's secret store and never in this database. The
-- envelope binds `(user_id, provider)` as additional authenticated data, so a
-- row's ciphertext moved into another row — the shape a database write attack
-- would take — fails to open rather than decrypting into a credential the
-- attacker can spend at the customer's vendor.
--
-- That is a real widening of what a database compromise costs, and it is stated
-- plainly rather than left implicit: an attacker holding a dump of THIS table
-- and nothing else holds ciphertext; an attacker holding the dump AND the
-- deployment's KEK holds customers' vendor credentials. Sealing is what makes
-- those two different outcomes. `docs/SECURITY.md` carries the same note so the
-- "only digests" claim there is not quietly false.
--
-- # Why one row per (user, provider) and not a history
--
-- The UNIQUE below makes "which of my two Anthropic keys is being used?" a
-- question that cannot be asked. Re-pasting is how a customer rotates, and the
-- upsert replaces in place. There is deliberately no soft-delete and no
-- superseded-key history: `api_keys` are disabled-not-deleted because
-- `usage_events` references them, but nothing references this row — usage
-- history records the PROVIDER and the `byok` flag below, never the credential
-- — so a customer asking ZeroRouter to stop holding their vendor credential is
-- left with ZeroRouter holding nothing. Keeping a detached key "just in case"
-- would be retaining a third party's secret after being asked not to.
--
-- `last_used_at` is reset to NULL by that upsert rather than carried forward:
-- it describes the credential, not the row, and a freshly pasted key that
-- claimed to have been used last Tuesday would be telling the customer
-- something false about the key they are looking at.
--
-- # The fee lives in the code, not in this schema
--
-- No rate column. The 5% is applied to the same `usage_cost` figure the metered
-- path already computes, at both the reservation and the settle arm
-- (`router/src/api.rs`), so BYOK reuses the reserve->settle invariants whole
-- rather than opening a second money path beside them. A per-row rate column
-- would make the charge a property of data an operator can edit, which is
-- exactly what the tier catalog's validation exists to prevent elsewhere.

CREATE TABLE byok_provider_keys (
    user_id UUID NOT NULL REFERENCES users (id),
    -- The provider ALIAS from `config/providers.json` (`anthropic`, `openai`,
    -- ...), which is what a tier candidate names and therefore what dispatch
    -- matches on. Free text rather than a CHECK against a fixed list: the
    -- inventory is configuration and gains entries without a migration, and a
    -- CHECK here would make adding a provider lane require one. A row naming a
    -- provider this deployment does not have simply never matches a candidate.
    provider TEXT NOT NULL,
    -- The AES-256-GCM envelope. See the header: version | dek nonce |
    -- wrapped dek | data nonce | ciphertext, all bound to (user_id, provider).
    sealed_credential BYTEA NOT NULL,
    -- A 64-bit SHA-256 prefix of the plaintext. An identifier, never an
    -- authenticator: nothing is admitted because a fingerprint matched, so it
    -- only has to beat accidental collision among one customer's few keys.
    -- It is what lets support ask "is the key you hold the one we hold?"
    -- without either side disclosing anything.
    fingerprint TEXT NOT NULL,
    -- The trailing four characters, the affordance every provider dashboard
    -- uses. Four is the same number those dashboards show, so a customer can
    -- compare the two without ZeroRouter disclosing more than the vendor
    -- already does.
    last4 TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- NULL until the credential has actually served a request. Best-effort and
    -- written off the request's critical path: this is a display field, and
    -- failing a customer's inference because a timestamp did not update would
    -- be an absurd trade.
    last_used_at TIMESTAMPTZ,
    PRIMARY KEY (user_id, provider),
    CONSTRAINT byok_provider_keys_provider_is_nonempty
        CHECK (BTRIM(provider) <> ''),
    -- A blank fingerprint or last4 would read as "we hold a key and can tell
    -- you nothing about it", which is worse than not holding one. Both are
    -- derived at insert time and cannot legitimately be empty.
    CONSTRAINT byok_provider_keys_fingerprint_is_nonempty
        CHECK (BTRIM(fingerprint) <> ''),
    -- An empty envelope would be a row that claims to hold a credential and
    -- does not. The real floor is much higher (the header alone is 61 bytes),
    -- but the constraint's job is to refuse the degenerate value, not to
    -- restate the format — pinning the exact length here would have to be
    -- migrated every time the envelope gains a field.
    CONSTRAINT byok_provider_keys_credential_is_nonempty
        CHECK (LENGTH(sealed_credential) > 0)
);

COMMENT ON TABLE byok_provider_keys IS
    'Customer-supplied upstream provider credentials, sealed under AES-256-GCM with BYOK_ENCRYPTION_KEY. The only reversibly-stored secrets in this database; see docs/SECURITY.md';
COMMENT ON COLUMN byok_provider_keys.sealed_credential IS
    'AES-256-GCM envelope (version | dek nonce | wrapped dek | data nonce | ciphertext), bound to (user_id, provider) as AAD so a ciphertext cannot be replayed into another row';
COMMENT ON COLUMN byok_provider_keys.fingerprint IS
    'Truncated SHA-256 of the plaintext credential; an identifier for support and display, never an authenticator';
COMMENT ON COLUMN byok_provider_keys.last4 IS
    'Trailing four characters of the credential, matching what provider dashboards display';
COMMENT ON COLUMN byok_provider_keys.last_used_at IS
    'When this credential last served a request; NULL means never. Best-effort, updated off the request critical path';

-- Which requests were served on the customer's own credential.
--
-- NULLABLE with no default, and written at INSERT — the rule `usage_events` has
-- had since 0001, and it is load-bearing here rather than ceremonial. The table
-- rejects UPDATE and DELETE, so there is no backfill available even in
-- principle: every row settled before this migration was served on ZeroRouter's
-- own credentials, and NULL is the honest word for "this row predates the
-- distinction" rather than a FALSE asserting something nobody recorded.
--
-- # Why the flag is here and not in `credit_ledger`
--
-- The obvious alternative was a new `entry_type` — 'byok_usage' beside 'usage'
-- — and it is structurally refused by 0002's own constraint:
--
--     CONSTRAINT credit_ledger_usage_references_request
--         CHECK ((entry_type = 'usage') = (request_id IS NOT NULL))
--
-- A 'byok_usage' row carrying a request_id violates it, and a 'byok_usage' row
-- WITHOUT one is a usage debit that cannot be tied to the request it settled.
-- Relaxing that CHECK would also mean revisiting
-- `credit_ledger_usage_entries_are_debits` and the partial UNIQUE on
-- `request_id` that makes a settle replay a no-op — the four constraints are
-- one argument about idempotency, and splitting the vocabulary of `entry_type`
-- would weaken it to record a display distinction.
--
-- So the ledger row stays bit-for-bit what it has always been: a 'usage' debit
-- anchored to a request. The BYOK fact belongs to the METERING row, which is
-- already where every other "what actually happened upstream" column lives
-- (provider, model, rates, cost basis), and `/api/billing/ledger` reads it back
-- through the request_id the two tables already share.
ALTER TABLE usage_events
    ADD COLUMN byok BOOLEAN;

COMMENT ON COLUMN usage_events.byok IS
    'TRUE when the served attempt dispatched on the customer''s own provider credential and was therefore charged the BYOK fee rather than the catalog price; NULL on rows settled before BYOK existed';

-- Answers "which of this user's requests were BYOK" without scanning the whole
-- ledger. Partial, because the column is NULL on every historical row and FALSE
-- on every house-credential row after this: indexing those would be indexing
-- the overwhelming majority to find the minority. The queries that read this
-- column are all looking for the TRUE rows.
CREATE INDEX usage_events_byok_idx
    ON usage_events (api_key_id, ts DESC)
    WHERE byok;

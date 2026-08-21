-- 0028_byok_fallback_opt_in.sql
--
-- "Use ZeroRouter's key if mine fails" — per attached credential, off by
-- default.
--
-- # What #103 decided, and what this does NOT change
--
-- Migration 0026 shipped BYOK with a structural no-fallback guarantee: a route
-- builds exactly one client per provider, holding whichever credential won at
-- assembly time, so a customer's key being rejected upstream cannot silently
-- become a house dispatch billed at twenty times the fee they expected. That
-- guarantee is not weakened here. It remains the default and it remains
-- structural — a customer who does nothing gets exactly the behaviour #103
-- shipped, because with this column FALSE no house client for that provider is
-- ever constructed.
--
-- What changes is that a customer may now ASK for the other behaviour, for one
-- provider at a time, having been told in the portal what it costs: a fallback
-- attempt is a house dispatch and is billed at the FULL catalog price, not at
-- 5% and not against the monthly allowance. That is the whole reason the
-- default is off and the reason this is a column rather than a deployment-wide
-- setting — it is a decision about somebody's bill, so it has to be made by the
-- person who pays it, per key, in the open.
--
-- # NOT NULL DEFAULT FALSE, and why that is the safe direction
--
-- Every credential already attached predates this column, and FALSE is both the
-- honest reading of those rows and the conservative one: a customer who
-- attached a key under #103's no-fallback promise must not have that promise
-- quietly revoked by a migration. A nullable column would have made "unset"
-- indistinguishable from "declined" at every read site and invited a
-- `COALESCE(fallback_enabled, TRUE)` somewhere downstream to turn the default
-- inside out. There is exactly one way to enable this, and it is the customer
-- ticking the box.
--
-- Unlike `usage_events`, this table takes UPDATE freely — it is current
-- configuration, not history — so the toggle is an ordinary UPDATE and the
-- upsert in `attach_key` has to be careful NOT to reset it on rotation. Pasting
-- a replacement key is rotating a credential, not withdrawing a preference,
-- and a customer whose fallback silently switched off because they rotated
-- would find out from an outage.
ALTER TABLE byok_provider_keys
    ADD COLUMN fallback_enabled BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN byok_provider_keys.fallback_enabled IS
    'TRUE when the customer has opted in to retrying on ZeroRouter''s own credential if their key fails upstream. Fallback attempts are billed at the FULL catalog price, not the BYOK fee, and do not draw on the monthly allowance. FALSE is migration 0026''s structural no-fallback default and is what every key that predates this column carries';

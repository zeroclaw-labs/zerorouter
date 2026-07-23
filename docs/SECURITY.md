# ZeroRouter security

This document describes the threat model, the enforced fail-closed
inventory, and how to report a vulnerability. The architectural rationale
lives in [`ARCHITECTURE.md`](ARCHITECTURE.md); this file maps each invariant
to the code that enforces it.

## Threat model

ZeroRouter moves money on behalf of untrusted network clients. The assets
are: prepaid customer balances, customer API keys, upstream provider
credentials, and the usage/billing record. The adversaries considered:

- **anonymous internet clients** guessing or replaying keys, session
  cookies, device codes, or Stripe webhooks;
- **authenticated customers** trying to read another tenant's keys, usage,
  or ledger, to spend past their balance/caps, or to escape their selected
  tier to a more expensive model;
- **a compromised or misbehaving upstream provider** returning garbage,
  omitting usage, or hanging;
- **an operator misconfiguration** (missing secret, partial feature group)
  that would otherwise silently disable a control.

Out of scope: a compromised database or host (the database is the root of
trust), denial of service beyond the admission caps, and malicious code in
the pinned dependencies (mitigated by pinning to exact revisions and a
locked build).

## Fail-closed inventory (where each is enforced)

The startup contract is: **misconfiguration aborts, absence disables.**

| invariant | where enforced |
|---|---|
| Missing/partial database configuration aborts; the `DB_*` path always connects with `verify-full` TLS against a pinned CA bundle | `router/src/db.rs` (`database_pool_from_env`); the Dockerfile ships a checksum-pinned RDS CA bundle |
| A partially configured web plane, OIDC group, or Stripe group aborts startup | `router/src/web.rs` (`WebConfig::from_env`, `feature_group`) — all-or-nothing per group |
| An unknown `ZEROROUTER_REQUIRE_CREDITS` value aborts | `router/src/web.rs` (`credits_required_from_env`) — only `true`/`1`/`false`/`0` parse |
| Auth or database errors during a request yield 401/503, never admission | `router/src/auth.rs` (key auth; malformed tokens are rejected by shape, unknown keys compare against a dummy hash), `router/src/session.rs` (`SessionRejection::{Unauthenticated,DatabaseUnavailable}`) |
| Mutating portal requests without the CSRF header are rejected before any lookup | `router/src/session.rs` — the `PortalUser` extractor requires `x-zerorouter-portal` on non-GET/HEAD; cross-site form posts cannot set custom headers, and the session cookie is `HttpOnly; SameSite=Lax` |
| A successful upstream call with missing usage settles the reservation at its conservative bound and the request errors with `metering_unavailable` — an unmetered success is never delivered as a success | `router/src/db.rs` + `router/src/api.rs` (settlement paths) |
| A webhook with a bad signature, stale timestamp, or unknown user credits nothing | `router/src/stripe.rs` (HMAC signature verification, tolerance window, user resolution before any ledger write) |
| Admission under `ZEROROUTER_REQUIRE_CREDITS=true` reads the live balance under a per-**user** advisory lock in the same transaction that reserves — no cached balance exists in the admission path | `router/src/db.rs` (advisory-locked reserve) |

## The six designed-out failure classes

These flaws were found in the earlier TypeScript implementation
(see `ARCHITECTURE.md`); each is structurally prevented here. Reintroducing
any of them is release-blocking.

1. **Stub upstream auth.** Every upstream call carries real credentials via
   the pinned `zeroclaw-providers` client; a candidate whose credential is
   absent is dropped from the walk, never sent unsigned.
   *Enforced in `router/src/providers.rs` (credential-gated candidate
   construction).*
2. **Cross-tenant reads.** Every portal/control-plane query is scoped by the
   authenticated `user_id` in the SQL itself; there are no unscoped list
   endpoints. The admin CLI is the only cross-tenant surface and requires
   direct database credentials, not a network call.
   *Enforced in `router/src/portal.rs` and `router/src/billing.rs`
   (`WHERE user_id = $1` / joins through `api_keys.user_id`), identity from
   the `PortalUser` extractor in `router/src/session.rs`.*
3. **Fail-open auth.** No auth path treats a missing secret, missing header,
   or backend error as permission. Inference keys, portal sessions, and
   device codes are all validated by shape first and hash lookup second;
   errors map to 401/403/503.
   *Enforced in `router/src/auth.rs`, `router/src/session.rs`,
   `router/src/device.rs`, `router/src/stripe.rs`.*
4. **Non-idempotent debits.** Every charge is anchored to a unique key:
   usage debits to `credit_ledger.request_id` (one per reservation, settles
   exactly once), purchases to `credit_ledger.stripe_session_id`. A replay
   violates a unique index and changes nothing; the ledger itself is
   append-only via `BEFORE UPDATE OR DELETE`/`TRUNCATE` triggers.
   *Enforced in `router/migrations/0002_billing_and_web.sql` (unique partial
   indexes + `reject_credit_ledger_mutation` triggers) and the settlement
   transaction in `router/src/db.rs`.*
5. **Stale-cache overdraft.** Admission reads `users.credit_balance_usd`
   under a Postgres advisory lock keyed by the **user id** (not the key id)
   in the same transaction that inserts the reservation, so concurrent
   requests across a user's keys cannot jointly overdraw.
   *Enforced in `router/src/db.rs`.*
6. **Consent-escaping failover.** Failover walks only the candidate list of
   the tier the client selected, in the order given by `tiers.toml`. Unknown
   model IDs are hard errors — never routed, never priced at zero.
   *Enforced in `router/src/config.rs` (tier resolution) and
   `router/src/api.rs` (the invocation walk).*

## Token namespaces

Three deliberately distinct namespaces; **no plaintext token ever rests in
the database — only SHA-256 digests are stored.**

| prefix | what | minted / verified in | stored as |
|---|---|---|---|
| `zcr_` + 64 hex | inference API keys (`Authorization: Bearer`) | `router/src/auth.rs` | SHA-256 hex in `api_keys.key_hash` |
| `zcs_` + 64 hex | portal session cookie `zr_session` (`HttpOnly`, `SameSite=Lax`, `Secure` on HTTPS) | `router/src/session.rs` | SHA-256 hex in `portal_sessions.token_hash` |
| `zdc_` device codes (+ short user codes) | RFC 8628 handshake, 15-minute TTL | `router/src/device.rs` | SHA-256 hex in `device_authorizations.device_code_hash` |

The distinct prefixes mean a token can never be replayed across surfaces: a
session cookie fails the inference key's shape check and vice versa. The
device flow's `access_token` is a freshly minted `zcr_` key created at claim
time, so the plaintext exists only in the claiming response.

## What is intentionally not stored or logged

- **Plaintext keys, session tokens, or device codes** — hashes only (above).
  The admin CLI and the device-claim response print a key's plaintext exactly
  once.
- **Provider request/response bodies** — application logs carry
  request/model/provider/token metadata only. Chat messages, tool payloads,
  and completions are never logged.
- **Upstream error bodies** — the pinned provider dependency includes
  sanitized upstream response fragments in its own log events, so
  `router/src/main.rs` force-appends `zeroclaw_log_event=off,zeroclaw_providers=off`
  to whatever `RUST_LOG` requests. Operators cannot re-enable those targets
  through the environment.
- **Secrets in debug output** — `StripeSettings` scrubs its secret fields in
  its `Debug` impl (`router/src/web.rs`).

## Reporting a vulnerability

Please report vulnerabilities **privately** to the zeroclaw-labs
maintainers: use GitHub's private vulnerability reporting ("Report a
vulnerability" under the repository's Security tab). Do not open a public
issue, and do not include exploit details in public PRs. We will acknowledge
receipt, keep you informed of the fix, and credit you in the advisory unless
you prefer otherwise.

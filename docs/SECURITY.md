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
| An unknown `ZEROROUTER_REQUIRE_CREDITS` value aborts; unset or blank requires credits | `router/src/web.rs` (`credits_required_from_env`) — only `true`/`1`/`false`/`0` parse, the default is `true`, and cap-only needs an explicit `false`/`0` that logs what it gives up |
| A tier candidate priced above its owning tier's sell rate aborts the catalog load | `router/src/config.rs` (`validate_tier_catalog` → `validate_candidate_margin`) — every candidate bills at the tier sell rate, so a higher basis loses money on every request it serves |
| Auth or database errors during a request yield 401/503, never admission | `router/src/auth.rs` (key auth; malformed tokens are rejected by shape, unknown keys compare against a dummy hash), `router/src/session.rs` (`SessionRejection::{Unauthenticated,DatabaseUnavailable}`) |
| Mutating portal requests without the CSRF header are rejected before any lookup | `router/src/session.rs` — the `PortalUser` extractor requires `x-zerorouter-portal` on non-GET/HEAD; cross-site form posts cannot set custom headers, and the session cookie is `HttpOnly; SameSite=Lax` |
| A successful upstream call with missing usage settles the reservation at its conservative bound and the request errors with `metering_unavailable` — an unmetered success is never delivered as a success | `router/src/db.rs` + `router/src/api.rs` (settlement paths) |
| A webhook with a bad signature, stale timestamp, or unknown user credits nothing | `router/src/stripe.rs` (HMAC signature verification, tolerance window, user resolution before any ledger write) |
| Admission under `ZEROROUTER_REQUIRE_CREDITS=true` reads the live balance under a per-**user** advisory lock in the same transaction that reserves — no cached balance exists in the admission path | `router/src/db.rs` (advisory-locked reserve) |
| A malformed `BYOK_ENCRYPTION_KEY` aborts startup; an absent one disables bring-your-own-key and refuses attach attempts with a named reason | `router/src/byok.rs` (`Keyring::from_env`) — the same "misconfiguration aborts, absence disables" contract as the web plane |
| A route where the customer holds keys for only SOME rungs — or holds one whose opted-in fallback may retry on ZeroRouter's credential — reserves at the full catalog price, never the 5% fee: the settle debit is clamped to the reservation, so under-reserving would deliver inference ZeroRouter cannot bill for | `router/src/api.rs` (`byok_reservation_rate`) |
| The monthly BYOK allowance is priced inside the settle transaction, under the same per-user advisory lock admission takes, so two concurrent settles cannot both claim the last dollar of it; a request may reserve nothing only when the allowance still covers it after subtracting what this user's in-flight requests have already committed | `router/src/db.rs` (`settle_once`, `reserves_no_byok_fee`) |

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

Three deliberately distinct namespaces; **no plaintext token ZeroRouter
issues ever rests in the database — only SHA-256 digests are stored.** The
one secret that is stored reversibly is a secret ZeroRouter did not issue and
must replay; see "Customer-supplied provider credentials" below.

| prefix | what | minted / verified in | stored as |
|---|---|---|---|
| `zcr_` + 64 hex | inference API keys (`Authorization: Bearer`) | `router/src/auth.rs` | SHA-256 hex in `api_keys.key_hash` |
| `zcs_` + 64 hex | portal session cookie `zr_session` (`HttpOnly`, `SameSite=Lax`, `Secure` on HTTPS) | `router/src/session.rs` | SHA-256 hex in `portal_sessions.token_hash` |
| `zdc_` device codes (+ short user codes) | RFC 8628 handshake, 15-minute TTL | `router/src/device.rs` | SHA-256 hex in `device_authorizations.device_code_hash` |

The distinct prefixes mean a token can never be replayed across surfaces: a
session cookie fails the inference key's shape check and vice versa. The
device flow's `access_token` is a freshly minted `zcr_` key created at claim
time, so the plaintext exists only in the claiming response.

## Customer-supplied provider credentials (BYOK)

A customer may attach their own upstream provider API key so their traffic
dispatches on their account and is charged 5% of the catalog price above a
free monthly allowance (migration 0027). That key
is the **only reversibly-stored secret in the database**, and it is the one
exception to the digests-only rule above. It has to be: a digest answers "is
this the same string?", and this credential must be presented to Anthropic or
OpenAI on the customer's behalf, so it has to come back out.

| property | where enforced |
|---|---|
| Sealed with AES-256-GCM under a per-record data key, which is itself sealed under `BYOK_ENCRYPTION_KEY` — a secret held in the deployment's secret store, never in the database | `router/src/byok.rs` (`Keyring::seal`/`open`), migration `0026` |
| The envelope binds `(user_id, provider)` as additional authenticated data, so a ciphertext copied into another tenant's row fails to open rather than decrypting into a spendable credential | `router/src/byok.rs` (`aad_bytes`); asserted in `router/tests/byok.rs` |
| Never returned by any endpoint — including the response to the request that attached it. `ByokKeySummary` has no field that could carry one | `router/src/portal.rs`; asserted in `router/tests/portal.rs` and `router/tests/byok.rs` |
| Never logged. The attach handler logs only the root cause of a failure, and a credential that cannot be opened is warned about by provider alias alone | `router/src/portal.rs` (`attach_byok`), `router/src/byok.rs` (`open_credentials`) |
| A minting provider (Vertex) cannot take a customer key at all: its token cache is keyed by ZeroRouter's own environment variable, so a substituted credential would be cached under the house key's name | `router/src/providers.rs` (`ProviderMetadata::accepts_byok`) |
| Absent `BYOK_ENCRYPTION_KEY` disables the feature and refuses attach attempts; a malformed one aborts startup | `router/src/byok.rs` (`Keyring::from_env`), `router/src/main.rs` |

**What this widens.** Before BYOK, a database dump disclosed no usable
secret. It now discloses ciphertext, and an attacker holding *both* the dump
and the deployment's KEK holds customers' vendor credentials — secrets that
spend money at a third party, outside ZeroRouter's control. Separating the two
is what the envelope buys; it does not make the risk zero, and it is stated
here rather than left implicit.

**What a BYOK request does not get.** Retention on that traffic is governed by
the customer's own agreement with the provider. ZeroRouter's catalog labels
describe ZeroRouter's contracts, and the per-response retention attestation
that fails closed on house traffic is deliberately **not** asserted on BYOK
dispatch — the header would describe the customer's team, and presenting it as
a ZeroRouter-verified fact would be a claim ZeroRouter cannot make. Those
responses carry `zerorouter.byok = true` so the caller can tell which contract
applies. See `router/src/providers.rs` (`create_provider`).

## What is intentionally not stored or logged

- **Plaintext keys, session tokens, or device codes** — hashes only (above).
  The admin CLI and the device-claim response print a key's plaintext exactly
  once.
- **Provider request/response bodies** — application logs carry
  request/model/provider/token metadata only. Chat messages, tool payloads,
  and completions are never logged.
- **Upstream error bodies** — an upstream 4xx body is provider-controlled text
  that routinely echoes the request that provoked it, and the pinned provider
  dependency includes sanitized fragments of it in its own log events. Those
  events, and the router's own per-attempt failure detail from the candidate
  walk, are emitted under targets listed in
  `logging::RETENTION_PROTECTED_TARGETS` and denied by a filter layer *beneath*
  the operator's. Because `tracing` composes global filters by conjunction, no
  `RUST_LOG` value — including a field-qualified directive, which outranks a
  bare `target=off` on specificity — can re-enable them. Router code that
  formats a provider body belongs under `logging::UPSTREAM_DETAIL_TARGET`;
  anything logged under an unlisted target reaches the sink, which is why the
  boundary is a list to extend rather than a claim to trust.
- **Secrets in debug output** — `StripeSettings` scrubs its secret fields in
  its `Debug` impl (`router/src/web.rs`), and `byok::Keyring` and
  `providers::ByokCredentials` do the same for the BYOK key material
  (`router/src/byok.rs`, `router/src/providers.rs`).

## Reporting a vulnerability

Please report vulnerabilities **privately** to the zeroclaw-labs
maintainers: use GitHub's private vulnerability reporting ("Report a
vulnerability" under the repository's Security tab). Do not open a public
issue, and do not include exploit details in public PRs. We will acknowledge
receipt, keep you informed of the fix, and credit you in the advisory unless
you prefer otherwise.

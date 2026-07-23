# ZeroRouter Architecture

ZeroRouter is a Rust LLM gateway with prepaid billing: an OpenAI-compatible
inference surface backed by tiered multi-provider routing, plus a web plane
(portal, Stripe credits, OIDC login, device authorization) for self-service.

It is the successor to two earlier efforts, and deliberately inherits from
both:

- the **B0 inference router** (`zeroclaw-labs/zerorouter` branch
  `feat/rust-b0`), whose code seeds this repository verbatim: fail-closed
  auth, advisory-lock reserve→settle metering, an append-only usage ledger
  enforced by database triggers, and tier routing from `tiers.toml`;
- the **Bifrost/FrankenGate lineage** (`maximhq/bifrost`,
  `pierretokns/frankengate`), studied as behavioral references (no code
  copied). Bifrost proves the gateway shape; FrankenGate's roadmap defines
  the invariants this codebase adopts: money and security subsystems fail
  closed on authority loss, every charge is idempotent by request ID, and no
  feature is advertised before its runtime exists.

## Planes

```
                 ┌──────────────────────────────────────────────┐
                 │                 zerorouter                    │
 client ── /v1/* │  inference plane (api.rs)                    │ ── upstream
                 │    auth → resolve tier → reserve → invoke    │    providers
                 │    → settle (usage_events + credit_ledger)   │
                 ├──────────────────────────────────────────────┤
 browser ─ /api  │  web plane (web.rs ctx, optional)            │
        ─ /auth  │    portal SPA · keys · usage · Stripe        │ ── Stripe
        ─ /.well-│    OIDC login · RFC 8628 device flow         │ ── OIDC IdP
          known  │                                              │
                 └──────────────────────────────────────────────┘
                                    │ Postgres (RDS, verify-full TLS)
```

The inference plane is exactly B0's: `POST /v1/chat/completions` (SSE and
non-streaming), public `GET /v1/models`, `GET /healthz`. Model IDs are either
tier aliases (`zero/low-cost`, `zero/balanced`, `zero/high-end`) resolving to
an ordered candidate list, or concrete candidate IDs (`provider/model`).
Failover only ever walks the resolved candidate list — a request can never
reach a provider outside the tier the client selected.

The web plane is optional and enabled by `ZEROROUTER_PUBLIC_BASE_URL`. Each
feature group (OIDC, Stripe) must be fully configured or fully absent;
partial configuration aborts startup (`web.rs`).

## Money

Prepaid credits denominated in USD (`rust_decimal`, Postgres `NUMERIC`).

- `users.credit_balance_usd` is the balance; **every** change to it happens in
  a transaction that also appends a `credit_ledger` row carrying
  `balance_after_usd`. The ledger is append-only (database triggers).
- **Purchases**: Stripe Checkout → signature-verified webhook →
  `checkout.session.completed` credits the balance, idempotent via the unique
  `credit_ledger.stripe_session_id`. A replayed webhook is a no-op.
- **Usage**: settlement inserts the `usage_events` row (idempotent via the
  reservation: it settles exactly once) and, in the same transaction, debits
  the balance with a `usage` ledger entry keyed by the unique `request_id`.
  A double settle fails; a crash between insert and debit rolls both back.
- **Admission**: B0's per-key monthly spend cap and token-velocity cap are
  kept; when `ZEROROUTER_REQUIRE_CREDITS=true` admission additionally
  requires `balance − active user reservations ≥ reserved cost` inside the
  same advisory-locked transaction. The lock key is the **user id** (not the
  key id) so concurrent requests across a user's keys cannot jointly
  overdraw. There is no cached balance anywhere in the admission path.
- Reservations are conservative (prompt bound + full `max_tokens` at the
  tier sell rate), so settlement never exceeds the reserved amount and the
  balance cannot go negative while credits are required.

## Identity

Three token namespaces, deliberately distinct:

| prefix | what | storage |
|---|---|---|
| `zcr_` + 64 hex | inference API keys | SHA-256 in `api_keys.key_hash` |
| `zcs_` + 64 hex | portal session cookies (`zr_session`, HttpOnly, SameSite=Lax) | SHA-256 in `portal_sessions.token_hash` |
| device/user codes | RFC 8628 handshake, 15-minute TTL | SHA-256 in `device_authorizations` |

- **Portal login** is OIDC (authorization code + PKCE) against a configured
  IdP. State/nonce/verifier live in `oidc_states` (single-use, TTL) so any
  task instance can complete a callback started by another. Users are keyed
  by `(oidc_issuer, oidc_subject)`; an admin-provisioned user with a matching
  verified email is claimed on first login. First-time signups receive
  `ZEROROUTER_SIGNUP_CREDIT_USD` as a `promo` ledger entry.
- **Device flow** (for `zeroclaw` and other CLIs): `POST /auth/device/code`
  → user visits `/activate`, signs in, approves → CLI polls
  `POST /auth/device/token` → the response's `access_token` **is a freshly
  minted `zcr_` key**, created at claim time so plaintext never rests in the
  database. `/.well-known/openid-configuration` advertises the endpoints in
  the shape ZeroClaw's existing xAI-style flow expects.
- **CSRF**: mutating portal requests must carry `x-zerorouter-portal`, which
  cross-site form posts cannot set; cookies are SameSite=Lax besides.

## Fail-closed inventory

The startup contract is: misconfiguration aborts, absence disables.

- missing DB env → abort; `DB_*` path forces `verify-full` TLS.
- partially configured OIDC/Stripe/web groups → abort.
- unknown `ZEROROUTER_REQUIRE_CREDITS` value → abort.
- auth/database errors during a request → 401/503, never allow.
- unmetered success (usage missing) → the reservation settles at its
  conservative bound; the request errors with `metering_unavailable`.
- webhook with a bad signature, stale timestamp, or unknown user → rejected;
  nothing is credited.

## Tenancy

Every portal/control-plane query is scoped by the authenticated session's
`user_id` in the SQL itself (`WHERE user_id = $1` or a join through
`api_keys.user_id`). There are no unscoped list endpoints; the admin CLI
(`zerorouter admin`) is the only cross-tenant surface and it requires direct
database credentials, not a network call.

## Layout

```
router/            Rust crate (axum 0.8, sqlx 0.8, rust_decimal)
  src/api.rs         inference plane (B0)
  src/db.rs          admission/settlement, advisory-locked (B0 + credits)
  src/auth.rs        zcr_ keys (B0)
  src/web.rs         web-plane config + context (fail-closed groups)
  src/session.rs     portal sessions + CSRF extractor
  src/billing.rs     balance/ledger operations
  src/stripe.rs      Stripe client, checkout, webhook verification
  src/oidc.rs        OIDC relying party (login/callback/logout)
  src/device.rs      RFC 8628 device authorization + discovery document
  src/portal.rs      /api/me·keys·usage·ledger + SPA static serving
  migrations/        0001 (B0) · 0002 (billing + web)
  config/tiers.toml  tier catalog + sell rates (canonical)
portal/            Vite + React SPA (login, credits, keys, usage, activate)
.github/workflows  ci.yml (fmt/clippy/test with Postgres) · deploy.yml
docs/              this file · SECURITY.md · DEPLOY.md · ZEROCLAW-INTEGRATION.md
```

Terraform for the live stack lives in `zeroclaw-labs/zeroclaw-infrastructure`
(`environments/zerorouter-beta`), which is the sole IaC owner; this repo
ships only the deploy workflow that targets it. See `docs/DEPLOY.md`.

## Designed-out failure classes

These six flaws were found in the earlier TypeScript implementation; each is
structurally prevented here, and reviews should treat any reintroduction as
release-blocking:

1. **Stub upstream auth** — Bedrock uses real bearer credentials via the
   pinned `zeroclaw-providers` client; a candidate without credentials is
   dropped, never sent unsigned.
2. **Cross-tenant reads** — all list queries are scoped by construction (see
   Tenancy).
3. **Fail-open auth** — no auth path treats a missing secret or missing
   header as permission (see Fail-closed inventory).
4. **Non-idempotent debits** — every charge is anchored to a unique
   `request_id`/reservation and settles exactly once.
5. **Stale-cache overdraft** — admission reads the balance under the user
   advisory lock in the same transaction that reserves.
6. **Consent-escaping failover** — failover walks only the resolved tier
   candidates; unknown models are hard errors, never `cost = 0`.

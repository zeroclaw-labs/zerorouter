# ZeroRouter roadmap

This is the honest deferred list. Nothing here is advertised, stubbed, or
half-shipped — per the FrankenGate invariant adopted in
[`ARCHITECTURE.md`](ARCHITECTURE.md), no feature is claimed before its
runtime exists. Each item carries the reason it was deferred.

## Deferred features

- **`/v1/messages` Anthropic-wire surface** — the OpenAI wire covers every
  current consumer (ZeroClaw included); a second dialect doubles the
  conformance surface before anyone needs it.
- **Bedrock `ConverseStream` + cache-token reporting** — the pinned Bedrock
  client buffers the full Converse response before emitting SSE and does not
  report cache-read/write token counts; true streaming and cache metering
  need upstream client work, not router work.
- **Per-user/org budgets and teams** — the beta is single-user-per-account
  prepaid; org structure without a customer pulling for it is speculative
  schema.
- **Shared entitlement evaluator for `/v1/models` and invocation** (the
  FrankenGate acceptance criterion: what the catalog advertises and what
  invocation admits must come from one evaluator). **The CREDENTIAL dimension is
  now unified** — both surfaces read
  `providers::ProviderMetadata::dispatchable`, and a test asserts they agree for
  every provider in every environment. That half stopped being deferrable when a
  deploy without `BEDROCK_API_KEY` advertised two lanes it could not serve. What
  remains is the per-USER dimension: admission is also credit- and cap-gated,
  and the catalog is not, so a funded and an exhausted key still see the same
  rows. That one genuinely waits for per-plan entitlements, and it is a much
  weaker promise — a caller can discover their own balance, but they cannot
  discover which secrets the operator provisioned.
- **Cross-replica key-revocation invalidation SLO** — each replica's auth
  cache expires within 30 seconds; a revocation bus only matters above one
  replica, and the beta runs one.
- **Circuit breaker + canary routing** — tier failover already walks
  candidates on error; breaker state and weighted canaries need real traffic
  data to tune and are premature before it.
- **Alerting API** — operational alerting belongs on infrastructure metrics
  first; an in-product alerting surface has no consumer yet.
- **Audit log surface** — the append-only ledger and usage events are the
  audit substrate; a queryable operator-facing surface is presentation work
  deferred until there is an operator other than us.
- **Prometheus `/metrics`** — the beta observes via structured logs and ALB
  metrics; an unauthenticated metrics port needs its own exposure review.
- **Readiness split (`/readyz`)** — `/healthz` currently serves both
  liveness and readiness; a separate readiness probe (DB reachability,
  migration state) matters at multi-replica rollout.
- **Auto top-up (off-session payments)** — storing payment mandates and
  charging off-session is a materially higher Stripe compliance bar than
  Checkout; explicit prepaid only for now.
- **Refunds/adjustments admin API** — the ledger supports `refund` and
  `adjustment` entries; issuing them stays a direct-database admin operation
  until volume justifies an authenticated API.
- **SCIM** — no enterprise identity customer; OIDC login is sufficient.
- **Multi-region** — the advisory-lock admission design assumes one primary
  database; multi-region needs a sharding/consistency design, not a config
  flag.

## B0 lineage

- **`feat/rust-b0` is superseded.** The B0 branch (the TypeScript-era repo,
  now `zeroclaw-labs/zerorouter-ts`, `feat/rust-b0`) shares this repo's
  original ZeroClaw pin but is now formally retired in favor of this repo.
  It will **not** receive the ZeroClaw pin
  advance (the builder-rewrite / `.timeout_secs` bump landed here); all
  forward work — including the pin-advance and everything downstream of it —
  lives in this repo only. Decided in the pin-advance change per the
  integration map (§5.1).

## Licensing and branding hygiene

- **NOTICE file** — add one enumerating the B0 lineage and the pinned
  ZeroClaw crates' provenance before any public release.
- **Claim-ledger discipline** — keep the studied-not-copied record for
  Bifrost/FrankenGate (see the provenance note in the README and
  `ARCHITECTURE.md`) current whenever a new external design is consulted,
  so provenance claims stay auditable.

## Residual items from the initial adversarial review

The first security review's confirmed findings were fixed (see the
`fix: remediate confirmed findings` commit). Three lower-severity items were
deliberately deferred rather than fully closed, and are tracked here:

- **Streaming settle-after-deliver window** — CLOSED by migration
  `0006_settlement_outbox.sql`. A streaming request delivers tokens first and
  settles after, so a settlement transaction that failed durably lost the
  charge outright — and the admission sweep then deleted the reservation at
  expiry, leaving no trace at all. The settle payload is now written onto the
  reservation row before the settle transaction runs, transient failures are
  retried inside the request, an existing `usage_events.request_id` is read as
  success rather than a duplicate-key error (which is what makes an ambiguous
  COMMIT safe), and the background recovery sweep replays anything still owed.
  An expired reservation that owes a settlement is quarantined for
  reconciliation (`zerorouter admin owed-settlements`) instead of deleted.
  Exactly-once is unchanged: money still moves only in the transaction that
  consumed the reservation via `DELETE ... RETURNING`.
- **Per-IP rate limiting on unauthenticated endpoints** — `/auth/device/code`,
  `/auth/login`, and `/webhooks/stripe` are bounded by a web-plane request-body
  limit and by terminal-row reclamation, but there is no per-IP request-rate
  cap yet. Behind the beta ALB (allowlisted source) this is low risk; a public
  HTTPS endpoint should add a `tower_governor` layer keyed on the real client
  IP (behind the load balancer's `X-Forwarded-For`).
- **Tier catalog re-parsed per request** — `GET /v1/models` and the chat path
  re-read and re-validate `tiers.toml` on every request. The catalog is
  immutable at runtime, so it should be loaded once into an `Arc<TierCatalog>`
  in `RouterState` (or an `ArcSwap` if hot reload is later wanted).

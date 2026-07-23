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
  invocation admits must come from one evaluator) — today `/v1/models` is
  deliberately the stable full catalog while admission is credential- and
  credit-gated; unifying them matters once per-plan entitlements exist.
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

- **Streaming settle-after-deliver window** — a non-streaming request settles
  before any bytes reach the client, but a streaming request delivers tokens
  first and settles after. If that final settlement transaction fails durably,
  the delivered content is unbilled. The debit is idempotent and the failure
  is logged; the durable fix is to settle expired reservations at their
  reserved cost during the admission sweep (keyed by the unique `request_id`,
  so a late-succeeding retry cannot double-charge) instead of releasing them.
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

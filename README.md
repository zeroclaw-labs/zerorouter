# ZeroRouter 🦀

ZeroRouter is a Rust LLM gateway with prepaid billing:

- an **OpenAI-compatible inference surface** (`POST /v1/chat/completions` with
  SSE streaming, public `GET /v1/models`, `GET /healthz`);
- a **pinned model catalog** — OpenRouter-standard `{vendor}/{model}` ids
  (`anthropic/claude-sonnet-5`, `openai/gpt-5.6-luna`) served at the vendor's
  list rate, with the `zero/*` namespace reserved for future routed tiers;
- **prepaid credits** — Stripe Checkout purchases, an append-only USD ledger
  enforced by database triggers, and advisory-locked reserve→settle metering
  so a balance can never be overdrawn by concurrent requests;
- a **self-service portal** — OIDC login, API-key management, usage and
  ledger views;
- **RFC 8628 device login** so CLIs (ZeroClaw first) can mint an API key with
  a browser approval instead of copy-pasting secrets.

Design and invariants are documented in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
Security posture: [`docs/SECURITY.md`](docs/SECURITY.md). Operations:
[`docs/DEPLOY.md`](docs/DEPLOY.md). Using it from ZeroClaw:
[`docs/ZEROCLAW-INTEGRATION.md`](docs/ZEROCLAW-INTEGRATION.md). What is
deliberately not built yet: [`docs/ROADMAP.md`](docs/ROADMAP.md).

**Provenance:** this repository is seeded verbatim from the B0 inference
router (`zeroclaw-labs/zerorouter-ts`, branch `feat/rust-b0` — that repo
held the `zerorouter` name until this repository took it over). Bifrost
(`maximhq/bifrost`) and FrankenGate (`pierretokns/frankengate`) were studied
as behavioral references; no code was copied from either.

## Quickstart

Rust 1.96.1 and a local PostgreSQL are required. The service applies its
embedded migrations at startup.

```sh
# a throwaway postgres
docker run --rm -d --name zerorouter-pg -p 5432:5432 \
  -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=zerorouter postgres:16

cd router
export DATABASE_URL=postgres://postgres:dev@localhost:5432/zerorouter
export ANTHROPIC_API_KEY=...   # and/or OPENAI_API_KEY — see the table below

cargo run -- serve
```

The router listens on `0.0.0.0:8080` by default (`ZEROROUTER_BIND` to
change) and reads the tier catalog from `config/tiers.toml`
(`ZEROROUTER_TIERS_PATH` to change).

Mint an API key. The `zcr_` plaintext is printed exactly once; only its
SHA-256 digest is stored:

```sh
cargo run -- admin mint-key \
  --email you@example.com \
  --name local-dev \
  --spend-cap-usd 20 \
  --velocity-cap-tokens-per-min 100000
```

Call it, non-streaming:

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H 'Authorization: Bearer zcr_<shown-once>' \
  -H 'Content-Type: application/json' \
  -d '{"model":"anthropic/claude-haiku-4-5","stream":false,"messages":[{"role":"user","content":"Hello"}]}'
```

and streaming (standard OpenAI SSE chunks):

```sh
curl --no-buffer http://127.0.0.1:8080/v1/chat/completions \
  -H 'Authorization: Bearer zcr_<shown-once>' \
  -H 'Content-Type: application/json' \
  -d '{"model":"anthropic/claude-sonnet-5","stream":true,"messages":[{"role":"user","content":"Hello"}]}'
```

### Models

The catalog is pins only: `model` is an OpenRouter-standard `{vendor}/{model}`
id (for example `anthropic/claude-sonnet-5`, `openai/gpt-5.6-terra`) that
names exactly that model, priced at the vendor's list rate. The public id is
the vendor's own id and `/v1/models` reports `owned_by` as the vendor, so an
OpenRouter client migrates by changing only the base URL and key. Unknown
models are hard errors.

The `zero/*` namespace is reserved for future routed tiers that pick among
candidates; the three `zero/*` intent bands that once lived here are removed
(the header of `router/config/tiers.toml` records why, and what brings them
back).

`router/config/tiers.toml` is the sole source of truth for the catalog,
upstream model IDs, sell rates, and per-model metadata (context windows,
capabilities); `GET /v1/models` is materialized from it.

A candidate is only usable when its provider's credential is present in the
environment (unless the provider declares `credential: "none"` — edge mode,
below); a tier still needs at least one usable candidate.

### Hosted service

To use the hosted deployment instead of running your own: sign in at
[zerorouter.ai](https://zerorouter.ai), add credits, create a key, and point
any OpenAI-compatible client at `https://zerorouter.ai/v1`. Step by step:
[`docs/getting-started-hosted.md`](docs/getting-started-hosted.md).

### Local models (edge mode)

A self-hosted router can route to models on your own hardware — llama.cpp,
vLLM, Ollama, LM Studio — and burst to the cloud when they cannot take the
request. Design: [`docs/design/edge-mode-local-rung.md`](docs/design/edge-mode-local-rung.md).

**Run it in minutes:** [`docs/edge-quickstart.md`](docs/edge-quickstart.md)
walks a ready-made `docker compose` stack ([`examples/edge/`](examples/edge/))
from zero to chatting with a local model through the free lane, then adds the
hosted-burst ladder. Measured overhead for both lanes:
[`benchmarks/REPORT.md`](benchmarks/REPORT.md).

Two config files, no code:

1. **Declare the upstream.** Point `ZEROROUTER_PROVIDERS_PATH` at a JSON file
   in the shape of `router/config/providers.json`. Its entries are *added* to
   the shipped inventory; an entry whose `key` matches a shipped provider is
   refused, so nothing on disk can repoint `openai` or `anthropic`.

   ```json
   {"providers": [{
     "key": "local-llama",
     "adapter": "chat_completions",
     "credential": "none",
     "settlement": "free",
     "display_name": "llama.cpp (local)",
     "base_url": "http://127.0.0.1:8080/v1/chat/completions"
   }]}
   ```

   `"credential": "none"` is how an upstream that takes no API key says so —
   it is then never skipped for a missing credential, and no `Authorization`
   header is sent. `"settlement": "free"` says its traffic bills nobody. Both
   are legal *only* on `chat_completions`, the adapter with no implied
   endpoint, and an entry using it must always name its `base_url`. The two are
   independent: a local server behind a bearer token is credentialed **and**
   free, and that is a normal deployment.

2. **Declare the candidates.** In your `tiers.toml`, a candidate priced at `0`
   on a provider that declares `settlement: free` is a **$0 rung**, and
   cost-led requests (`:cost`, or a key defaulting to cost) order $0 rungs
   first — no estimator warm-up needed, because free is cheapest at any length.
   Declare what the model can actually take: in cost mode, a request whose
   prompt overflows the declared `context_window`, or that needs tools a
   candidate declared it lacks, sinks behind the rungs that can serve it and
   the existing failover walk bursts to cloud. An *undeclared* capability means
   unknown and never excludes anything. Selection is mechanical only — the
   router never predicts answer quality.

   **Entering the free lane takes both declarations, and neither is inferred.**
   A $0 price on a provider that has not declared free settlement refuses the
   whole catalog, and `settlement: free` on a cloud adapter refuses the whole
   inventory. Note what this does *not* claim: nothing can verify that a
   free-declaring upstream really is free — `chat_completions` also serves a
   hosted ZeroRouter taking metered traffic, and an operator who declares a
   billed upstream free has lied to their own config in a way no validation can
   detect. What the double declaration guarantees is that the free lane is
   never entered by accident: not by a typo in a rate, not by picking a wire,
   not by forgetting a credential.

   A candidate is billed at its **tier's** sell rate, so a $0 rung inside a
   mixed tier is free for the operator, not for the customer; to sell a local
   model for nothing, pin it in a tier whose sell rate is `0`. A route that is
   free on **both** sides — every candidate free *and* the tier selling at $0 —
   skips reserve/settle entirely (no advisory lock, no reservation row, no
   ledger write), with $0 usage still recorded asynchronously for visibility.
   A mixed ladder meters fully on every request, including the ones its local
   rung serves: the reservation is taken at admission, before anyone knows
   which rung will answer, and that is what makes the cloud burst billable.

`zerorouter admin catalog-drift` reads the same `ZEROROUTER_PROVIDERS_PATH`
(or `--providers`), so an edge catalog reconciles: local $0 rungs are reported
as `local $0 rung` and never fail the command, because no public model catalog
lists a model running on your hardware.

## Environment variables

Absence disables a feature; misconfiguration aborts startup. See the
fail-closed inventory in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

### Inference plane

| variable | required | default | notes |
|---|---|---|---|
| `ZEROROUTER_BIND` | no | `0.0.0.0:8080` | listen address |
| `ZEROROUTER_TIERS_PATH` | no | `config/tiers.toml` | tier catalog (container ships `/etc/zerorouter/tiers.toml`) |
| `ZEROROUTER_PROVIDERS_PATH` | no | — | operator-declared upstreams, **added to** the shipped inventory — how edge mode declares keyless / `settlement: free` local upstreams (see below). Unreadable, invalid, or shadowing a shipped key aborts startup |
| `DATABASE_URL` | yes* | — | explicit connection string, for local development |
| `DB_HOST` / `DB_PORT` / `DB_NAME` / `DB_USERNAME` / `DB_PASSWORD` / `DB_SSL_ROOT_CERT` | yes* | — | production path; always connects with `verify-full` TLS |
| `ANTHROPIC_API_KEY` | no | — | enables `anthropic/*` candidates |
| `OPENAI_API_KEY` | no | — | enables `openai/*` candidates |
| `ZEROROUTER_REQUIRE_CREDITS` | no | `true` | `true`/`1` or `false`/`0`; anything else aborts, and unset or blank means `true`. When true, admission requires prepaid balance ≥ the reserved cost. `false` opts into cap-only (see below) |

\* exactly one database path must be fully configured: either `DATABASE_URL`,
or all six `DB_*` variables.

**`ZEROROUTER_REQUIRE_CREDITS` defaults to `true`** (it previously defaulted
to `false`). Credits are the only ceiling backed by money: with enforcement
off, the per-key and derived per-user spend/velocity caps on `api_keys` are
the sole limit on what a user can consume, and those caps are self-service —
the portal lets a user raise a key's own `spend_cap_usd`. A deployment that
never set the variable was therefore running with no enforced ceiling, so the
unconfigured case now lands on the safe side. Cap-only is still supported:
set `ZEROROUTER_REQUIRE_CREDITS=false` (or `0`) to opt in explicitly, which
logs a startup warning naming what that gives up.

### Web plane

The web plane (portal, OIDC, Stripe, device flow) is enabled by setting
`ZEROROUTER_PUBLIC_BASE_URL` and disabled entirely by leaving it unset.
**Feature groups are all-or-nothing**: the three OIDC variables must be set
together or not at all, likewise the two Stripe variables; setting any of
them without `ZEROROUTER_PUBLIC_BASE_URL` — or setting only part of a group —
aborts startup rather than running with a silently disabled security control
(`router/src/web.rs`).

| variable | required | default | notes |
|---|---|---|---|
| `ZEROROUTER_PUBLIC_BASE_URL` | to enable the web plane | — | externally reachable base URL; `https://` origins get `Secure` cookies |
| `OIDC_ISSUER_URL` | group: OIDC | — | IdP issuer for portal login (authorization code + PKCE) |
| `OIDC_CLIENT_ID` | group: OIDC | — | |
| `OIDC_CLIENT_SECRET` | group: OIDC | — | |
| `STRIPE_SECRET_KEY` | group: Stripe | — | |
| `STRIPE_WEBHOOK_SECRET` | group: Stripe | — | webhook signature verification secret |
| `ZEROROUTER_SIGNUP_CREDIT_USD` | no | `0` | promo credit granted on first login; must be ≥ 0 |
| `ZEROROUTER_CHECKOUT_MIN_USD` | no | `5` | minimum Stripe Checkout amount; must be > 0 |
| `ZEROROUTER_CHECKOUT_MAX_USD` | no | `1000` | must be ≥ the minimum |
| `ZEROROUTER_SESSION_TTL_SECS` | no | `604800` (7 days) | portal session lifetime, capped at 90 days |
| `ZEROROUTER_PORTAL_DIST` | no | `portal/dist` | built portal SPA to serve |
| `ZEROROUTER_DEVICE_CLIENT_IDS` | no | `zeroclaw` | comma-separated client IDs allowed to start a device authorization |

`RUST_LOG` controls verbosity (default `info`). The provider dependency's log
targets are force-disabled regardless of `RUST_LOG` — see
[`docs/SECURITY.md`](docs/SECURITY.md).

## Local Stripe testing

The webhook endpoint is `POST /webhooks/stripe`. Locally, forward events with
the Stripe CLI and use the secret it prints as `STRIPE_WEBHOOK_SECRET`:

```sh
stripe listen --forward-to localhost:8080/webhooks/stripe
```

Purchases are credited only from a signature-verified
`checkout.session.completed` event, idempotently per Stripe session — a
replayed webhook is a no-op.

## Portal development

The portal is a Vite + React SPA in `portal/`. In production the router
serves the built assets from `ZEROROUTER_PORTAL_DIST`. For development, run
the Vite dev server proxying API calls to the router:

```sh
cd portal
pnpm install
pnpm dev          # proxies /api, /auth, /webhooks to localhost:8080
```

## Repository layout

```
router/            Rust crate: inference plane, web plane, admin CLI, migrations
  config/tiers.toml  canonical tier catalog + sell rates
portal/            Vite + React portal SPA
examples/edge/     docker compose stack for edge mode (docs/edge-quickstart.md)
docs/              ARCHITECTURE · SECURITY · DEPLOY · ZEROCLAW-INTEGRATION · ROADMAP
Dockerfile         ARM64 image: router binary + built portal SPA
.github/workflows  ci.yml (fmt/clippy/test with Postgres) · deploy.yml (ECS)
```

Terraform for the live stack lives in `zeroclaw-labs/zeroclaw-infrastructure`
(`environments/zerorouter-beta`); this repo ships only the application image
and the deploy workflow. See [`docs/DEPLOY.md`](docs/DEPLOY.md).

## License

ZeroRouter is open source under the **GNU Affero General Public License,
version 3.0** ([LICENSE](LICENSE)) — an OSI-approved license, so the code is
fully auditable and self-hostable. Its network copyleft (AGPL §13)
additionally requires anyone who runs a **modified** version as a network
service to make their modified source available to that service's users.

Contributions are accepted under AGPL-3.0. To keep open the option of
relicensing the project under a more permissive license (e.g. Apache-2.0) in
the future, external contributions require a signed Contributor License
Agreement — see [CONTRIBUTING.md](CONTRIBUTING.md).

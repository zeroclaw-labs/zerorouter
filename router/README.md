# ZeroRouter B0 service

`zerorouter` is the Rust inference router for the ZeroRouter private beta. It exposes:

- `POST /v1/chat/completions` with OpenAI-style streaming and non-streaming responses;
- `GET /v1/models`, materialized from `config/tiers.toml`;
- `GET /healthz`;
- database-backed `admin mint-key`, `admin revoke-key`, and `admin list-keys` CLI commands.

The tier file is the sole source of truth for public model IDs, fallback order, upstream model IDs, and pricing. PostgreSQL is the sole source of truth for users, key status, caps, and usage. Provider credentials belong in environment variables locally and in AWS Secrets Manager in production.

## Local checks

Rust 1.96.1 is required.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Set `DATABASE_URL` for an explicitly configured local connection, or all of
`DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USERNAME`, `DB_PASSWORD`, and
`DB_SSL_ROOT_CERT` for the production-style path. The latter always uses
`verify-full`; the container ships a checksum-pinned Amazon RDS CA bundle. The
service applies its embedded SQLx migration at startup.

Provider candidates become available when their corresponding variables are present. Each credential is optional independently; a requested tier still needs at least one credential-backed candidate:

```text
ANTHROPIC_API_KEY
OPENAI_API_KEY
GEMINI_API_KEY
BEDROCK_API_KEY     (+ BEDROCK_REGION, see below)
FIREWORKS_API_KEY
XAI_API_KEY         (the team it belongs to must have ZDR enabled — see below)
VERTEX_SERVICE_ACCOUNT  (a service-account JSON blob, + VERTEX_PROJECT_ID — see below)
GROQ_API_KEY        (the org it belongs to must have ZDR enabled for Inference — see below)
TOGETHER_API_KEY    (the org's three Privacy toggles must be OFF — see below)
```

`GROQ_API_KEY` and `TOGETHER_API_KEY` carry the same class of requirement
`XAI_API_KEY` does, and they carry it in opposite directions. The `groq/*` lanes
are sold as zero-retention on the strength of an **organization-level Zero Data
Retention toggle** in Groq's Data Controls, which must be enabled and must cover
Inference — Groq allows ZDR to be set globally or per feature, so "ZDR is on"
is not the same statement as "this lane's guarantee holds". The `together/*`
lanes rest on Together's **published default**, which is already zero retention;
what must be true there is that nobody has opted the organization back out of it
via the three Privacy toggles (store prompts, allow training, allow passthrough).

Neither is checked at runtime the way xAI's is, because neither vendor publishes
a per-response attestation header. A key from a wrongly configured account on
either provider authenticates and serves normally, and the lanes go on
publishing `zero`. Get the account right before the key lands. See
`docs/DEPLOY.md`.

`XAI_API_KEY` carries one requirement no other key here does. The `xai/*` lanes
are sold as zero-retention on the strength of xAI's team-level Zero Data
Retention setting, and every response is checked for the
`x-zero-data-retention: true` header that confirms it. A key from a team without
ZDR enabled will authenticate and then have every request refused with a 502
`retention_attestation_failed`. That is the guard working, not a fault — but
enable ZDR in the xAI Console for that team before provisioning the key.

`VERTEX_SERVICE_ACCOUNT` is the odd one out: it is **not an API key**. Google
issues no long-lived key for the surface the `vertex/*` lanes dispatch on, so
this variable holds a Google service-account key in JSON — private key included
— which the router signs a JWT with and exchanges for a one-hour OAuth2 access
token, cached and refreshed shortly before expiry (`src/gcp_auth.rs`). The value
may be the JSON itself or a path to a file holding it. `VERTEX_PROJECT_ID` must
be set alongside it, because the endpoint carries the project in its path; the
lanes go dark without either.

Those three lanes are the same Gemini models the `google/*` lanes serve, at the
same price, but zero-retention — which depends entirely on how the Google Cloud
project is configured. **Do not provision this credential before completing the
project setup in `docs/DEPLOY.md`, "What the operator must do in Google Cloud".**
Unlike the xAI case there is no per-response guard: a misconfigured project
authenticates and serves normally while the catalog tells customers their
prompts are never stored.

Do not commit these values or place them in Terraform variables. The list above
is whatever `config/providers.json` entries name in `credential_env`; that file
is the source of truth, and this list is a convenience copy of it.

**`/v1/models` publishes only the lanes this deployment can actually serve.** A
provider whose `credential_env` is absent from the environment contributes no
rows, and a request naming one of its models is refused as `model_unavailable`
rather than admitted. A deployment with no provider secrets therefore publishes
an empty catalog, which is the honest answer.

This reverses an earlier rule — the catalog used to be "the stable full catalog
rather than changing with credential availability" — and the reversal was paid
for in production. A deploy without `BEDROCK_API_KEY` advertised the Bedrock
zero-retention lanes, the ones the product leads with, while every call to them
returned 503. Stability is not worth much when what is stable is untrue. If you
need to see the catalog a fully-provisioned deployment would publish, read
`config/tiers.toml`, which is the source of truth and is not filtered by
anything.

`BEDROCK_API_KEY` is an Amazon Bedrock API key (an IAM service-specific
credential), not the AWS access-key ID and secret used for Terraform.
**`BEDROCK_REGION` must be set alongside it** — both Bedrock endpoints carry the
region in their hostname, and with no region the Bedrock rungs drop out of every
route exactly as a missing key would. Nothing is defaulted: a guessed region
would silently move customer prompts across a boundary the operator did not
choose.

One AWS account, two API planes, and the shipped lanes are on the second one.
`config/providers.json` declares both under the single `bedrock` entry — the
entry's own endpoint is Bedrock's **mantle** plane
(`bedrock-mantle.{region}.api.aws/anthropic/v1/messages`, the Anthropic Messages
API verbatim), and its `classic_runtime` **surface** is the classic runtime plane
(`bedrock-runtime.{region}.amazonaws.com`, AWS's `InvokeModel`). A candidate
picks one with `surface = "..."` in `tiers.toml`.

They are not interchangeable, and not only because the wires differ: they host
different model generations. The mantle plane serves 5-generation Claude, the
classic runtime plane serves 4.5- and 4.6-generation. On this account AWS's
per-generation Sales entitlement cuts between 4.6 and 4.7 — opus 4.6 and the
three 4.5-generation models answer, everything from 4.7 up (including every
5-generation model, and therefore the whole mantle plane) returns 403 `not
available for this account`. So the 5-generation tiers are commented out in
`tiers.toml` and the four shipped `bedrock/claude-*` lanes all ride the runtime
plane. See `docs/DEPLOY.md` for the probe results, the re-enable condition, and
why the runtime lanes stay even after it is met.

The runtime wire (`src/wire/bedrock_runtime.rs`) sends the same Messages request
body the mantle wire does, with four differences that are each an opaque AWS 400
if got wrong — the model id moves to the URL path, `anthropic_version` becomes a
required body field, the `anthropic-version` header is dropped, and auth is
`Authorization: Bearer` rather than `x-api-key`. Its streaming is AWS event
stream binary framing rather than SSE, decoded by `src/wire/eventstream.rs`.

Start the service:

```sh
ZEROROUTER_BIND=127.0.0.1:8080 \
ZEROROUTER_TIERS_PATH=config/tiers.toml \
cargo run -- serve
```

Mint a key; the `zcr_` plaintext is printed once and only its SHA-256 digest is stored:

```sh
cargo run -- admin mint-key \
  --email operator@example.com \
  --name local-beta \
  --spend-cap-usd 20 \
  --velocity-cap-tokens-per-min 100000
```

Omit either cap flag to use the database schema's canonical default for that cap.

Call the router:

```sh
curl --no-buffer http://127.0.0.1:8080/v1/chat/completions \
  -H 'Authorization: Bearer zcr_<shown-once>' \
  -H 'Content-Type: application/json' \
  -d '{"model":"zero/high-end","stream":false,"messages":[{"role":"user","content":"Hello"}]}'
```

Application logs contain request/model/provider/token metadata only. Request messages, tool payloads, completions, API keys, and provider credentials must never be logged.
The process forcibly disables the pinned provider dependency's log targets
because its error records can include a truncated upstream response body.

Authentication completes before the service buffers or parses the request
body. After authentication, requests are capped at 8 MiB. Each admitted call
creates a PostgreSQL reservation using a conservative prompt bound plus the
effective output limit. Concurrent reservations serialize per key, so spend
and token caps are hard admission bounds. Successful calls settle with the
provider's final usage frame; opaque failures, missing usage, timeouts, and
shutdown cancellation settle conservatively. `usage_events.cost_usd` is the
customer sell-price debit from the tier table, not the provider's invoice cost.

Upstream work has a 15-minute deadline below the 20-minute reservation lease.
Streaming producers stop writing after client backpressure/disconnect but keep
draining the upstream usage frame. On SIGTERM, the router cancels and meters
all tracked background work before exiting.

## Prompt caching

This gate is **lifted**, and the paragraph it replaces is worth keeping in mind
because it explains the shape of what shipped. It read: the pinned ZeroClaw
provider API normalizes messages into `ChatMessage { role, content }`, cannot
carry arbitrary client `cache_control` blocks, and its Anthropic adapter adds
its own cache breakpoints — which is not client cache transparency. So the
router rejected every detected `cache_control` rather than dropping it
silently, and nobody was to describe the service as cache-transparent until the
provider interface could preserve those fields end to end.

The pin is gone and the router owns its wires, so it can. What that took, and
what is still true of it:

- **Client breakpoints are forwarded where the client placed them.** A
  `cache_control: {"type": "ephemeral"}` on a chat-completions message or tool
  reaches the Messages API at the corresponding position. A request that places
  any takes the client's placement and NONE of the wire's three defaults —
  Anthropic caps a request at four breakpoints, and merging would both risk the
  cap and charge a write premium at boundaries the customer did not choose.
- **Transparency required a price first.** The wire's own breakpoints mean
  essentially every Claude request writes to the upstream cache, and Anthropic
  bills a write at 1.25x input while ZeroRouter billed it at 1x. Accepting
  client `cache_control` without transcribing that rate would have widened a
  loss rather than shipped a feature, so `cache_write_per_mtok` landed with it.
- **Absence of that rate is the capability signal.** A lane that does not price
  cache writes refuses `cache_control` with `prompt_caching_unsupported`, the
  way an undeclared modality refuses an image. Only the Claude lanes declare
  it; catalog validation additionally refuses a cache-write price on any plane
  that could not carry a breakpoint upstream.
- **Still refused, deliberately:** `cache_control` inside a message content
  part or at the top level (no honest placement — see `ApiError::CacheControl
  Unsupported`), a `ttl` (the 1-hour cache is priced differently and is not
  transcribed), more than four breakpoints, and the whole feature on
  `/v1/responses`, which is the chat surface only.

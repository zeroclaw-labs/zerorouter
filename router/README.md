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
BEDROCK_API_KEY
DEEPINFRA_API_KEY
FIREWORKS_API_KEY
TOGETHER_API_KEY
```

Do not commit these values or place them in Terraform variables.

The initial Terraform example injects only `BEDROCK_API_KEY`. The private-beta
fallbacks let that credential serve `zero/low-cost` through MiniMax M2.5 and
`zero/balanced` through DeepSeek V3.2. `zero/high-end` remains unavailable until
the account owner completes Anthropic's Bedrock first-time-use form. Western
OpenAI-compatible providers remain ahead of the Bedrock fallbacks when their
credentials are enabled. `/v1/models` deliberately remains the stable full
catalog rather than changing with credential availability.

`BEDROCK_API_KEY` is an Amazon Bedrock service-specific bearer key, not the AWS access-key ID and secret used for Terraform. The configured Claude models also require the account owner to complete Anthropic's first-time-use form in Bedrock. The pinned Bedrock provider does not stream upstream responses or report cache-read/cache-write token counts: `stream: true` is emitted as SSE after the full Converse response, and cached-token metering cannot be validated in Bedrock-only mode.

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

## Deployment gate

The pinned ZeroClaw provider API normalizes messages into `ChatMessage { role, content }`. It cannot carry arbitrary client `cache_control` blocks or every OpenAI request extension unchanged. Its Anthropic adapter adds its own cache breakpoints, but that is not strict client cache transparency. The router therefore rejects detected `cache_control` input instead of silently dropping it. Do not describe B0 as cache-transparent or enable customer traffic that depends on client-specified cache boundaries until the pinned provider interface is extended and this router preserves those fields end to end.

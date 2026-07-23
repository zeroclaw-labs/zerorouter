# Using ZeroRouter from ZeroClaw

ZeroRouter speaks the OpenAI chat-completions wire protocol, so ZeroClaw can
use it at three levels of integration. Level 1 works today with zero
ZeroClaw changes; levels 2 and 3 are small, well-templated additions to the
ZeroClaw repository.

## Level 1 — today: a custom provider slot (no ZeroClaw changes)

Configure a `custom` OpenAI-compatible provider slot pointing at the
router's `/v1` base and authenticate with a `zcr_` key:

```toml
[providers.models.custom.zerorouter]
uri = "https://<router-host>/v1"
api_key = "zcr_..."
model = "zero/balanced"
wire_api = "chat_completions"

[agents.default]
model_provider = "custom.zerorouter"
```

Notes:

- **The `uri` must NOT include `/chat/completions`.** ZeroClaw's
  OpenAI-compatible client appends the `/chat/completions` path itself, so
  the configured endpoint is the `/v1` base only.
- `model` is a ZeroRouter tier alias (`zero/low-cost`, `zero/balanced`,
  `zero/high-end`) or a concrete candidate ID such as
  `anthropic/claude-sonnet-5`. Tier aliases get server-side failover inside
  the tier; unknown IDs are hard errors.
- Streaming, tool calls, and usage frames flow through unchanged; ZeroClaw
  needs no special handling.

## Level 2 — a built-in `zerorouter` provider preset

The ZeroClaw provider layer is factored so that adding a first-class
OpenAI-compatible family is roughly five mechanical edits. Copy the **Kilo
Gateway** template (the most recent minimal example of the pattern):

1. **Schema struct + endpoint enum** in
   `crates/zeroclaw-config/src/schema.rs`: a `ZerorouterEndpoint` enum whose
   single variant's `ModelEndpoint::uri()` returns the canonical
   `https://<router-host>/v1`, and a `ZerorouterModelProviderConfig` with
   `#[prefix = "providers.models.zerorouter"]` flattening
   `ModelProviderConfig` — copy `KiloEndpoint` /
   `KiloModelProviderConfig`.
2. **Slot-macro row** in `crates/zeroclaw-config/src/providers.rs`: add
   `(zerorouter, "zerorouter", ZerorouterModelProviderConfig)` to the
   `providers.models` slot table (next to the `kilo` row).
3. **`CompatFamilySpec` impl** in
   `crates/zeroclaw-providers/src/factory.rs` with `DISPLAY = "ZeroRouter"`,
   `DEFAULT_URL` set to the canonical `/v1` base, and
   **`PUBLIC_MODEL_LISTING = true`** — ZeroRouter's `GET /v1/models` is
   unauthenticated and carries pricing, so ZeroClaw's live-pricing task can
   fill cost-tracking rates from the gateway itself (see the pricing-shape
   note below).
4. **Display-list entry** in `crates/zeroclaw-providers/src/lib.rs` (the
   provider display table that lists `("kilo", "Kilo", false)`).
5. **Optional: key-prefix row** — add `("zcr_", "zerorouter")` to the
   API-key prefix table in `crates/zeroclaw-providers/src/lib.rs` so a
   pasted `zcr_` key is attributed to the right provider (and mismatches
   are flagged by `check_api_key_prefix`).

Plus the Kilo-style **lockstep URL regression test** in `factory.rs`
(`kilo_gateway_default_url_matches_schema_endpoint` is the template):
assert the schema `ZerorouterEndpoint` URI and the factory `DEFAULT_URL`
agree, since the default exists in both crates.

### `/v1/models` pricing shape

ZeroRouter's model listing carries per-model pricing in the
OpenRouter-style shape ZeroClaw's live-pricing filler consumes: a `pricing`
object of **per-token USD amounts as decimal strings**, keyed `prompt`,
`completion`, and `input_cache_read`. Tier sell rates come from the
router's canonical `tiers.toml`; the gateway is the source of truth for its
own prices, and operator-configured rates always win over live-filled ones.

### TokenUsage convention

ZeroClaw's `TokenUsage` accounting treats `cached_input_tokens` as a
**subset of** `input_tokens` (cached reads are included in the input
count, not additional to it). ZeroRouter already satisfies this: its usage
frames report `prompt_tokens` inclusive of cached tokens and clamp the
cached count to at most `prompt_tokens`, so no adapter arithmetic is
needed.

## Level 3 — device-flow login (`zeroclaw auth login`)

Future work in ZeroClaw: an `AuthProvider::Zerorouter` mirroring the
existing xAI OAuth pattern (`crates/zeroclaw-providers/src/auth/xai_oauth.rs`
and its `AuthProviderFlow` registration in `auth/mod.rs`), pointed at
ZeroRouter's discovery document:

1. fetch `<router-host>/.well-known/openid-configuration`, which advertises
   the RFC 8628 endpoints in the shape the xAI-style flow already expects;
2. `POST /auth/device/code` (client id `zeroclaw`, allowlisted via the
   router's `ZEROROUTER_DEVICE_CLIENT_IDS`) → show the user code and
   verification URL; the user signs in to the portal and approves;
3. poll `POST /auth/device/token` until approval; the response's
   `access_token` **is a freshly minted `zcr_` API key** (minted at claim
   time — the plaintext never rests in the router's database);
4. write that key into the user's `providers.models` config
   (`providers.models.zerorouter.<alias>.api_key`, or the level-1 `custom`
   slot until level 2 lands), same as the existing flow persists its
   credential.

The result: `zeroclaw auth login --model-provider zerorouter` onboards a
machine with a browser approval and no copy-pasted secrets. Device and user
codes expire after 15 minutes and are stored hashed; a denied or expired
authorization mints nothing.

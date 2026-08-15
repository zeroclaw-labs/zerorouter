# Edge quickstart: ZeroRouter on your box

From `docker compose up` to chatting with a model on your own hardware
through ZeroRouter's free lane — then bursting to hosted cloud inference from
the same endpoint. Design context:
[`design/edge-mode-local-rung.md`](design/edge-mode-local-rung.md). Measured
overhead for both lanes: the two-lane tables in
[`../benchmarks/REPORT.md`](../benchmarks/REPORT.md).

What you end up with:

```
your agent ──► ZeroRouter (Docker, :8080)
                 ├── local/chat   your llama.cpp/Ollama/vLLM  ($0, unmetered)
                 └── zero/burst   local first ──► hosted ZeroRouter /v1 (metered)
```

## Prerequisites

- Docker with Compose (Docker Desktop on macOS/Windows, docker-ce on Linux).
- An OpenAI-chat-completions server on your host. Any of these works:

  | server | typical URL from inside Docker |
  |---|---|
  | Ollama | `http://host.docker.internal:11434/v1/chat/completions` |
  | llama.cpp (`llama-server`) | `http://host.docker.internal:<port>/v1/chat/completions` |
  | vLLM | `http://host.docker.internal:8000/v1/chat/completions` |
  | LM Studio | `http://host.docker.internal:1234/v1/chat/completions` |

  No model server handy? The repo's benchmark mock speaks fluent chat
  completions and needs only Go:

  ```sh
  cd benchmarks/mock-upstream
  MOCK_PORT=11434 go run .
  ```

## 1. Point the config at your model server

Two files in `examples/edge/`, both mounted into the container:

- **`providers.json`** — the operator provider overlay. It declares one
  upstream, keyed `local`: the `chat_completions` adapter, keyless
  (`"credential": "none"`), and — the load-bearing line — `"settlement":
  "free"`, the operator's signed statement that traffic to this upstream
  bills nobody. Edit `base_url` if your server is not at
  `host.docker.internal:11434` (the Ollama default).
- **`tiers.toml`** — the catalog this router serves, replacing the shipped
  one. It sells one tier, `local/chat`, at $0 on every dimension, with a
  single $0-basis candidate on the `local` provider. Edit the candidate's
  `model` to a model id your server actually serves (the default is
  `qwen3:8b`; llama.cpp ignores the field, Ollama and vLLM do not).

Entering the free lane takes **both** declarations — the provider's
`settlement: free` and the $0 rates — and a $0 price on a provider that never
declared free settlement refuses the whole catalog at load. Nothing infers
freeness from an adapter or a missing key; the free lane is only ever entered
on purpose.

## 2. Up

```sh
cd examples/edge
docker compose up --build -d
```

The first `--build` compiles the router from source (a release build; expect
minutes). After it, `docker compose ps` should show `db` healthy and `router`
running, and:

```sh
curl -s http://127.0.0.1:8080/healthz
```

answers `{"status":"ok"}`. The router applies its migrations itself at startup —
there is no separate migration step. Your catalog is live too:

```sh
curl -s http://127.0.0.1:8080/v1/models
```

lists exactly one model, `local/chat`.

## 3. Mint an API key

Free-lane requests still authenticate — the free rung is not an anonymous
proxy. The admin CLI ships in the image and creates the user as a side
effect; the `zcr_` plaintext is printed exactly once, and only its SHA-256
digest is stored:

```sh
docker compose exec router zerorouter admin mint-key \
  --email you@example.com --name edge-quickstart
```

```sh
export ZR_KEY=zcr_...   # the value printed above
```

No credit grant is needed for the free lane: admission compares your balance
against the request's *reserved* cost, which on a $0 route is zero.

## 4. Chat through the free lane

```sh
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $ZR_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"local/chat","stream":false,"messages":[{"role":"user","content":"Hello from the edge"}]}'
```

That is an OpenAI-compatible response from your own hardware, through your
own router. `"stream": true` works too (standard SSE chunks). Point any
OpenAI-SDK agent at `http://127.0.0.1:8080/v1` with the `zcr_` key and the
model id `local/chat`, and it is running on your box.

### Prove the metering skip is real

The free lane skips reserve/settle *structurally* — no advisory lock, no
reservation row, no ledger debit — while still recording $0 usage
asynchronously for the dashboard. Watch it in the database you just ran:

```sh
docker compose exec db psql -U zerorouter -c "
  SELECT
    (SELECT COALESCE(sum(n_tup_ins), 0)
       FROM pg_stat_user_tables
      WHERE relname = 'usage_reservations') AS reservation_inserts,
    (SELECT count(*) FROM usage_events WHERE cost_usd = 0) AS free_usage_rows,
    (SELECT count(*) FROM credit_ledger)                   AS ledger_rows;"
```

`reservation_inserts` is a cumulative insert counter, so `0` means no
reservation was ever *written*, not merely that none is left. After the curl
above: `0` reservation inserts, `0` ledger rows, and one `$0` usage row (it
lands async — allow a beat). The benchmark harness re-proves this before
every published number, with a metered control that moves the same counters
([`../benchmarks/REPORT.md`](../benchmarks/REPORT.md), "Free-lane skip
verification").

## 5. Hosted burst: cloud from the same endpoint

The same `chat_completions` adapter that serves your local model serves a
hosted ZeroRouter `/v1` as a metered upstream — that dual use is the design's
point. Two edits:

**`providers.json`** — add a credentialed, metered entry (no `settlement`
line: metered is the default, and silence means billed):

```json
{
  "key": "zerorouter-hosted",
  "adapter": "chat_completions",
  "credential_env": "ZEROROUTER_HOSTED_API_KEY",
  "secret_name": "zerorouter-hosted-api-key",
  "display_name": "hosted ZeroRouter /v1 (metered burst)",
  "base_url": "https://YOUR-HOSTED-ZEROROUTER/v1/chat/completions"
}
```

**`tiers.toml`** — append a mixed ladder: local rung first, hosted rung
behind it. The tier now sells at real money (pass-through of the hosted
rate), so the $0 local basis inside it is the operator's margin, not the
customer's discount.

Naming rule: a tier id must either equal one of its own candidate ids (the
pin shape, like `local/chat`) or live in the reserved `zero/*` namespace —
the namespace for routing aliases, which is exactly what a multi-candidate
ladder is. Any other id refuses the whole catalog at load, so the ladder is
`zero/burst` (and candidate ids must *not* start with `zero/`):

```toml
[tiers."zero/burst"]

# Sell rate: pass-through of the hosted rung's price.
[tiers."zero/burst".rates]
input_per_mtok = 0.20
cached_input_per_mtok = 0.02
output_per_mtok = 1.20

# Rung 1: the local model. $0 basis — but the TIER is not free.
[[tiers."zero/burst".candidates]]
id = "burst-local"
provider = "local"
model = "qwen3:8b"

[tiers."zero/burst".candidates.metadata]
context_window = 32768
input_modalities = ["text"]

[tiers."zero/burst".candidates.rates]
input_per_mtok = 0.00
cached_input_per_mtok = 0.00
output_per_mtok = 0.00

# Rung 2: hosted ZeroRouter. `model` is a tier id from the HOSTED catalog.
[[tiers."zero/burst".candidates]]
id = "burst-hosted"
provider = "zerorouter-hosted"
model = "openai/gpt-5.6-luna"

[tiers."zero/burst".candidates.metadata]
context_window = 1050000
max_output_tokens = 128000
input_modalities = ["text"]
tool_call = true

[tiers."zero/burst".candidates.rates]
input_per_mtok = 0.20
cached_input_per_mtok = 0.02
output_per_mtok = 1.20
```

Provide the credential and restart the router (the provider overlay is read
once at startup; the tier file is re-read per request):

```sh
export ZEROROUTER_HOSTED_API_KEY=zcr_...   # your hosted ZeroRouter key
docker compose up -d --force-recreate router
```

A mixed ladder meters, so the account now needs credit on *this* router
(you are the biller on your own box):

```sh
docker compose exec router zerorouter admin grant-credit \
  --email you@example.com --amount-usd 10 --note "edge operator credit"
```

Then burst-capable chat is one model id away:

```sh
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $ZR_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"zero/burst","stream":false,"messages":[{"role":"user","content":"Hello again"}]}'
```

The local rung serves it; if your local server is down, overflowing, or the
request needs more context than the rung declares, the existing failover walk
dispatches to the hosted rung instead. Appending `:cost` to the model id
orders rungs by expected cost (free first, no warm-up needed — free is
cheapest at any length).

### The honest note: mixed ladders meter fully

**Every request through `zero/burst` runs the full reserve → settle path —
including the ones your local model serves.** The reservation is taken at
admission, before anyone knows which rung will answer, because a route
holding even one metered candidate must be able to bill the burst; that is
what makes the burst billable at all. Run the psql query from step 4 again
after the curl above and watch `reservation_inserts` move — on a
local-served request (pg_stat counters flush lazily; allow a second).

The metering *skip* — and its latency — belongs only to routes composed
entirely of free rungs: an all-$0 tier like `local/chat`, or a free rung
addressed as its own pin. If a missing `ZEROROUTER_HOSTED_API_KEY` drops the
hosted rung, `zero/burst` collapses to an all-local route and **still
meters**: the tier sells at real money, so you'd be handing paid-tier
inference away otherwise. The two-lane benchmark tables
([`../benchmarks/REPORT.md`](../benchmarks/REPORT.md)) quantify both shapes
honestly — read lane 1 (free) as the all-local tier and lane 2 (metered) as
what every `zero/burst` request pays.

## Troubleshooting

- **Router can't reach your model server (Linux):** `host.docker.internal`
  maps to the Docker bridge, so the server must listen beyond loopback —
  e.g. `OLLAMA_HOST=0.0.0.0` for Ollama, `--host 0.0.0.0` for llama.cpp/vLLM.
  Docker Desktop reaches host-loopback services as-is.
- **Port clash on 8080:** llama.cpp's default port is also 8080. Move one of
  them — the router's published port lives under the `db` service in
  `docker-compose.yml` (the two containers share one network namespace; the
  comment there explains why).
- **Startup fails naming your config:** that is the config working. A
  candidate priced $0 on a provider that never declared `settlement: free`,
  an entry that shadows a shipped provider key, or a `chat_completions`
  entry with no `base_url` each refuse startup loudly rather than serve a
  route you didn't write.
- **`model` errors from Ollama/vLLM:** the candidate's `model` in
  `tiers.toml` must be an id your server serves (`ollama list`,
  `curl localhost:11434/v1/models`).
- **Fresh start:** `docker compose down -v` drops the database (users, keys,
  usage) as well as the containers.

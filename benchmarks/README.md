# Gateway overhead benchmarks

Reproducible comparison of ZeroRouter (both lanes: free/local and metered)
against LiteLLM and Bifrost, all fronting the same local mock upstream.
Results and methodology: [`REPORT.md`](./REPORT.md). Design context:
[`docs/design/edge-mode-local-rung.md`](../docs/design/edge-mode-local-rung.md),
"Benchmark plan (two honest lanes)".

## One command → full table

```sh
cd benchmarks
./run.sh            # = up + verify + bench + report + history
```

That brings up a scratch Postgres (port 5545), the Go mock upstream, a
release-build ZeroRouter (free + metered lanes), LiteLLM, and Bifrost; runs
the integrity gate; drives the full load matrix; and prints the markdown
tables. Allow ~20 minutes, most of it measurement time. `./run.sh down`
stops everything.

## Prerequisites (macOS/Homebrew paths assumed)

| Tool | Install | Used for |
|---|---|---|
| `oha` | `brew install oha` | load generation |
| `go` | `brew install go` | mock upstream |
| `cargo` | rustup | ZeroRouter release build |
| `postgresql@16` | `brew install postgresql@16` | scratch metering DB |
| Bifrost binary | `npx -y @maximhq/bifrost@1.6.10 --help` (downloads to `~/Library/Caches/bifrost/`) | Bifrost target |
| `python3` | system/brew | LiteLLM venv (created by `run.sh`), result rendering |

Everything runs on loopback. No real provider keys, no production systems,
no network egress on the request path. The scratch Postgres lives in
`.run/pgdata` on a non-default port (5545) so it can never collide with a
system cluster.

## Why usage state is reset between ZeroRouter cells

ZeroRouter's admission prices every request against the user's month-to-date
usage (the spend/velocity-cap scan in `router/src/db.rs`), so its per-request
cost grows with the rows already written this month. A benchmark writes usage
rows at benchmark rates: without a reset, each ZeroRouter cell would run
against whatever history the previous cells happened to leave behind — the
first full run of this harness had a metered cell inherit ~26k rows from the
free-lane cells and report a p50 of 792 ms that was really the cap scan, not
the metered path. `run.sh` therefore TRUNCATEs `usage_events` +
`usage_reservations` (and CHECKPOINTs) before every measured ZeroRouter cell,
and the history-growth cost is measured deliberately instead by
`./run.sh history` (sequential latency with an empty month vs. 30k seeded
rows), reported in REPORT.md.

## The integrity gate (`./run.sh verify`)

No cell is measured until:

1. **Parsed-output fidelity** — every gateway's `choices[0].message.content`
   round-trips the mock's reply text. HTTP 200 alone proves nothing: the
   first prototype of this harness had Bifrost answering 200 with
   `choices: null` because of a mis-wired base URL (see REPORT.md caveats).
2. **Free-lane metering skip, structurally proven** — 300 free-lane requests
   move the cumulative `usage_reservations` insert counter (`pg_stat`) and
   `credit_ledger` row count by exactly zero while ~300 `$0` usage rows land
   asynchronously; a 50-request metered control then moves the same counters
   by exactly +50, proving the counters would have caught a leak.

## Layout

```
run.sh              orchestrator: up | verify | bench | history | report | down | all
render_tables.py    results/results.json -> REPORT.md markdown tables
mock-upstream/      Go mock: fully-conformant chat-completions + Responses (+SSE)
configs/
  providers.bench.json  ZeroRouter operator overlay: free + metered mock providers
  tiers.bench.toml      ZeroRouter tier catalog: $0-sell pin + priced pin
  litellm.yaml          LiteLLM proxy config
  bifrost.config.json   Bifrost app config (copied into a FRESH app-dir each start)
.run/               scratch state: pgdata, venv, keys, binaries (gitignored)
results/, logs/     raw outputs (gitignored)
```

## Interpreting the two lanes

- **ZeroRouter free lane vs LiteLLM/Bifrost** is the apples-to-apples row:
  all three do stateless forwarding with no per-request billing work.
- **ZeroRouter metered** is a different product doing strictly more work —
  advisory-locked prepaid reserve → dispatch → settle against Postgres per
  request. It is reported in both the single-key shape (worst case: every
  request serializes on one user's lock) and the 16-user shape (how
  production traffic actually spreads).

# Gateway overhead benchmark — ZeroRouter (two lanes) vs LiteLLM vs Bifrost

**Date:** 2026-08-14 · **Machine:** MacBook Pro (M2 Pro, 10 cores, 32 GB) · **Load gen:** oha 1.15.0
**What this measures:** the per-request overhead each OpenAI-compatible gateway adds in front of an
identical, near-instant local mock upstream. Every number below comes from a run this harness
executed (`results/results.json`, rendered by `render_tables.py`); nothing is estimated.

ZeroRouter appears twice, because it has two lanes
(`docs/design/edge-mode-local-rung.md`, "Benchmark plan — two honest lanes"):

- **Free lane** — a `$0`-sell route on a `settlement: free` provider. The metering skip (edge mode
  stage 3) removes the per-user advisory lock, the reservation, and settlement from the hot path.
  This is the apples-to-apples row: stateless forwarding, the job LiteLLM and Bifrost actually do.
- **Metered lane** — the full prepaid path: advisory-locked reserve → dispatch → settle against
  Postgres, three append-only ledger writes per request. No competitor does this work; its row is
  reported as what it is, in both the single-key shape and the 16-user shape.

Both ZeroRouter lanes use the same `chat_completions` adapter, dial the same mock endpoint, and
parse the same response body — the only difference between them is the metering work.

> ### Read this first — the honest headline
> **ZeroRouter's free lane does not beat LiteLLM at the fixed-rate cell (p50 3.20 ms vs 2.74 ms)
> and does not approach Bifrost (0.36 ms).** The reason is not proxying and not metering: it is
> that ZeroRouter's admission gate re-scans the user's **month-to-date usage history** on every
> request, on **both** lanes, and the benchmark's own sustained traffic grows that history while
> the cell runs. Measured directly (below): the same free-lane request costs **0.86 ms** with an
> empty month and **18.07 ms** with 30k accumulated rows. ZeroRouter's intrinsic forwarding path
> is fast; its per-request cost is currently a function of how much the account has already used
> this month. The fix is already sketched in the code (`router/src/db.rs`, "Access path" comment:
> widen the 0001 index so the scan is index-only); until it lands, these are the numbers.

---

## Fixed rate (100 req/s, 60 s per cell — the latency comparison)

Constant 100 req/s (`oha -q 100 --latency-correction`), non-streaming, identical tiny body,
6,000 requests per cell. Well below every target's ceiling. ZeroRouter cells start from a reset
usage state (see "Methodology: usage-state reset").

| Target | p50 (ms) | p95 (ms) | p99 (ms) | Overhead vs baseline (p50) | Throughput | Success | CPU % (100 = 1 core) | Peak RSS (MB) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **Baseline** (mock direct, no gateway) | 0.13 | 0.23 | 0.33 | — | 100/s | 100% | 1 | 27 |
| **Bifrost** (Go) | 0.36 | 0.52 | 0.65 | +0.22 ms | 100/s | 100% | 13 | 891 |
| **LiteLLM** (Python, 1 worker) | 2.74 | 3.29 | 3.97 | +2.61 ms | 100/s | 100% | 28 | 342 |
| **ZeroRouter free lane** (Rust, metering skipped) | 3.20 | 4.61 | 4.99 | +3.06 ms | 100/s | 100% | 5 | 24 |
| **ZeroRouter metered** (Rust, full reserve→settle, single key) | 4.48 | 6.05 | 13.31 | +4.35 ms | 100/s | 100% | 8 | 25 |

Reading the ZeroRouter rows: the free lane's +3.06 ms is **not constant** across the cell — the
cell writes 6,000 usage rows as it runs and the admission scan grows with them (0.86 ms at row
zero, extrapolating to ~4–5 ms near row 6,000; the 60-second percentiles average over that
climb). The **metering skip itself is visible and real**: free vs metered is 3.20 vs 4.48 ms p50
under identical conditions — the advisory lock, reservation insert, and settle transaction cost
~1.3 ms p50 at this rate, and the metered p99 (13.3 ms) shows the settle path's tail. CPU per
request is lowest for the two compiled gateways; note ZeroRouter's CPU column excludes the
Postgres server process doing admission/settle work on its behalf.

## Saturation (open loop, 50 connections, 60 s per cell — the throughput comparison)

| Target | Concurrency | Throughput (req/s) | p50 (ms) | p95 (ms) | p99 (ms) | Success | CPU % | Peak RSS (MB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| **Baseline** (mock direct, no gateway) | 50 conns | **72,844** | 0.41 | 2.02 | 2.97 | 100% | 385 | 27 |
| **Bifrost** (Go) | 50 conns | **15,640** | 2.58 | 7.53 | 13.10 | 100% | 547 | 1007 |
| **LiteLLM** (Python, 1 worker) | 50 conns | **432** | 111.38 | 124.57 | 243.04 | 100% | 100 | 342 |
| **ZeroRouter free lane** (Rust, metering skipped) | 50 conns | **214** | 230.32 | 427.25 | 532.69 | 100% | 10 | 25 |
| **ZeroRouter metered** (Rust, full reserve→settle, single key) | 50 conns | **172** | 317.49 | 439.53 | 496.18 | 100% | 13 | 25 |
| **ZeroRouter metered** (16 users × 4 conns) | 16×4 conns | **762** | 52.50 | 229.55 | 280.46 | 100% | 75 | 25 |

Multi-user latency cells are the **median worker's** percentile (per-worker spread: p50
51.90–52.83 ms, p95 225.69–231.53 ms, p99 276.34–287.57 ms across 16 workers); throughput is the
exact sum of per-worker rates over the same wall window. oha exposes no raw samples, and pooling
percentiles across workers would fabricate a number.

Reading it:

- **Single-key metered (172/s) is a floor, not a ceiling: 16 users moved 762/s through the same
  single router process** — 4.4× — with per-worker p50 at 52 ms instead of 317 ms. The per-user
  advisory lock serializes only within a user, exactly as designed. (The prototype's 221/s
  single-key figure was reported with the same warning; this run makes the multi-user number
  real instead of an argument.)
- **The free lane saturates at 214/s — barely above metered single-key.** That similarity is the
  history scan again, not the metering: at saturation the cell wrote 12,841 rows in 60 s for one
  user, so by the end each admission was re-scanning ~13k rows, and the scan (shared by both
  lanes) dwarfs the lock/reservation difference. ZeroRouter's own process sat at 10% of one core
  — the time goes to Postgres. Fresh-state sequential latency of 0.86 ms implies the free lane's
  potential is >1,000/s once admission stops re-scanning history; today's number is today's
  number.
- LiteLLM's single worker is GIL-bound at exactly one core (CPU 100%). Bifrost saturates near
  15.6k/s on ~5.5 cores with ~1 GB peak RSS vs ZeroRouter's 25 MB.

## The dominant ZeroRouter cost: the month-to-date history scan (measured)

Admission's spend/velocity-cap check aggregates the user's `usage_events` **since the start of
the month** on every request (`router/src/db.rs`). Sequential single-connection free-lane
requests (`./run.sh history`, 500 requests per point, same release build):

| Month-to-date rows for the user | p50 (ms) | p95 (ms) | p99 (ms) |
|---|---:|---:|---:|
| 0 | **0.86** | 1.15 | 1.72 |
| 30,000 (seeded) | **18.07** | 18.84 | 19.31 |

That is ~0.57 µs per accumulated row, paid by every request, on both lanes. The code's own
"Access path" comment documents the same measurement at production scale (~14 ms at 30k rows)
and the intended fix: widening the `usage_events` (api_key_id, ts) index to INCLUDE the summed
columns, making the aggregate an index-only scan (its measured figure: ~5 ms at 30k rows), or
going further and maintaining a running counter. **Until that lands, every latency and
throughput number ZeroRouter posts is history-dependent, and this report's cells are pinned to
"fresh month" state to keep them comparable.** A real account carrying a heavy month pays more
per request than this table shows; that is the honest reading.

## What each lane actually does per request (so the numbers have referents)

- **Free lane:** cached-key auth → in-memory route resolution → **one admission transaction**
  (api_keys row lock, month-to-date cap scan, balance read, `last_used_at` stamp, synchronous
  commit against fsync'd Postgres) → dispatch over a shared keep-alive client → async `$0`
  usage-event insert (off-path). No advisory lock, no reservation, no settle. The free lane is
  deliberately **not** DB-free: it still authenticates and still enforces the velocity cap
  (abuse control counts free tokens by design).
- **Metered lane:** the same, plus the per-user `pg_advisory_xact_lock`, a reservation INSERT in
  the admission transaction, and after dispatch a settle transaction (advisory lock again,
  `DELETE ... RETURNING` the reservation, clamped debit, ledger + usage + attempts writes).

## Integrity fixes made before trusting any number

This harness replaced a workspace prototype whose numbers had two defects the design doc flagged
(and one it did not). All three are fixed and gated:

1. **Bifrost was never actually parsed end-to-end.** The prototype reported Bifrost returning
   HTTP 200 with `choices: null` and blamed a "minimal mock body". The real cause, found with
   request logging: **Bifrost appends `/v1/chat/completions` to its configured `base_url`**; the
   prototype's `base_url` ended in `/v1`, so Bifrost dialed `/v1/v1/chat/completions` — and the
   prototype mock's forgiving catch-all answered that unknown path with a *Responses*-shaped
   body, which Bifrost's chat parser accepted as a 200 with null choices. Fixes: correct
   `base_url` (no `/v1`); the mock now **404s loudly** on unknown paths; the mock emits
   fully-conformant chat-completion and Responses bodies (and full SSE event ceremonies); and
   Bifrost's app-dir is rebuilt fresh each start (it materializes `config.json` into a `config.db`
   once and ignores the file afterwards — a stale app-dir silently pins old config).
   **Verification, run before every bench:** Bifrost's parsed answer must round-trip the reply
   text — `choices[0].message.content == "Hello! How can I help you today?"`, `finish_reason:
   "stop"`, full usage `{12, 9, 21}` (evidence: `results/verify-bifrost.json`), while the mock
   log shows the request landing on `POST /v1/chat/completions`. Streaming spot-checked the same
   way (content delta + usage chunk + `[DONE]`).
2. **Single-key lock serialization is no longer presented as a ceiling.** The matrix carries a
   16-user metered cell (16 oha workers, one key each, exact-sum throughput): 762/s vs 172/s
   single-key, same process, same database.
3. **Benchmark cells no longer poison each other through the history scan** (the defect the
   prototype did not know it had). In the first full run of this harness, cells degraded with
   cell *order* — the metered fixed cell inherited ~26k rows written by the free-lane cells and
   reported a p50 of 792 ms that was really the cap scan. Usage state (`usage_events`,
   `usage_reservations`, `request_attempts`) is now TRUNCATEd (+ ANALYZE + CHECKPOINT) before
   every measured ZeroRouter cell — an explicit superuser bypass of the append-only triggers, on
   a scratch database this harness created — and the history cost is measured deliberately
   (previous section) instead of accidentally.

### Free-lane skip verification (structural, not observational)

`./run.sh verify` proves the stage-3 metering skip with cumulative counters that cannot miss
insert-then-delete pairs (`pg_stat_user_tables.n_tup_ins`; settle DELETEs its reservation, so
`count(*)` would lie):

```
300 free-lane requests  -> reservation inserts +0, credit_ledger rows +0, $0 usage rows +300
50 metered requests     -> reservation inserts +50, credit_ledger rows +50, priced usage rows +50
```

The metered control proves the counters would have caught a leak. The `$0` usage rows land
asynchronously with `cost_usd = 0` (the row-level check on the run: `all cost 0: true`).
One measurement note: PostgreSQL's cumulative stats flush lazily (idle backends on a ~10 s
timeout), so the check reads `n_tup_ins` only after it has been stable for 12 s — both a
fixed-sleep read and a naive "two reads agree" read produced false counts while building this.

## Methodology: usage-state reset

Every ZeroRouter measured cell starts from `TRUNCATE usage_events, usage_reservations,
request_attempts` + `ANALYZE` + `CHECKPOINT` ("fresh month"). Within a cell, the traffic's own
rows accumulate — that within-cell growth is real behavior at that arrival rate and is left in.
The CHECKPOINT also prevents one cell's WAL debt from bleeding into the next cell's tail (the
prototype documented a 181 s checkpoint destroying a p95; nothing similar was observed in this
run's cells).

## Machine

| | |
|---|---|
| Model | MacBook Pro (Mac14,9) |
| Chip | Apple M2 Pro — 10 cores (6 performance + 4 efficiency) |
| Memory | 32 GB |
| OS | macOS 26.5.2 (build 25F84), arm64 |

Load generator, gateways, mock, and Postgres all share this one machine over loopback; that
depresses every saturation ceiling somewhat (uniformly) and leaves the fixed-rate cells ample
headroom.

## Versions (pinned)

| Component | Version / build |
|---|---|
| ZeroRouter | this repo @ `690f089` (origin/main), `cargo build --release`, default features (no `testing`) |
| rustc / cargo | 1.96.1 |
| LiteLLM | 1.96.2 (`litellm[proxy]==1.96.2`), **fastapi pinned 0.136.3**, uvicorn 0.52.3, starlette 1.6.0, pydantic 2.13.4, openai 2.54.0, Python 3.14.6 |
| Bifrost | v1.6.10 (`@maximhq/bifrost`, prebuilt `bifrost-http-0` via npx cache) |
| Go (mock build) | 1.26.4 |
| PostgreSQL | 16.10 (Homebrew), scratch cluster on port 5545, fsync + synchronous_commit ON, otherwise defaults |
| oha | 1.15.0 |

> **LiteLLM install gotcha (pinned around here):** unpinned `pip install 'litellm[proxy]'` pulls
> a FastAPI (≥0.141) that removed `get_flat_dependant`, which LiteLLM 1.96.2 imports; the proxy
> then dies at startup with a misleading `ModuleNotFoundError: No module named 'proxy_server'`.
> `fastapi==0.136.3` is the newest that works with this LiteLLM.

## Exact commands

```sh
cd benchmarks
./run.sh up        # scratch PG (5545) + mock + ZeroRouter (both lanes) + LiteLLM + Bifrost
./run.sh verify    # integrity gate (fidelity + metering-skip proof); bench runs it anyway
./run.sh bench     # full matrix -> results/results.json
./run.sh history   # the month-to-date history measurement -> results/history.jsonl
./run.sh report    # re-render the tables from results.json
./run.sh down
```

Per-cell load shapes: fixed = `oha -z 60s -q 100 --latency-correction -c 200`; saturation =
`oha -z 60s -c 50`; multi-user = 16 × `oha -z 60s -c 4`, one key per worker. ZeroRouter runs
with `ZEROROUTER_PROVIDERS_PATH=configs/providers.bench.json` (a keyless `settlement: free`
provider and a credentialed metered provider, both `chat_completions`, both dialing the mock)
and `ZEROROUTER_TIERS_PATH=configs/tiers.bench.toml` (a `$0`-sell pin and a priced pin,
basis == sell on both). The bench user is minted with `--spend-cap-usd 1000000
--velocity-cap-tokens-per-min 2000000000`.

## Caveats — read before quoting

1. **The free lane lost the latency cell it was expected to win, and the reason is measured,
   not excused.** The month-to-date cap scan (shared by both lanes) is the dominant and growing
   cost; the code documents the index fix. Do not quote the 3.20 ms as ZeroRouter's forwarding
   overhead (fresh-state sequential p50 is 0.86 ms) — and do not quote the 0.86 ms as what a
   real account experiences mid-month either. The benchmark gate will re-run this table when
   the index fix lands.
2. **ZeroRouter does real DB work the others don't, on both lanes.** Free = auth + caps + one
   synchronous commit; metered = that plus lock/reserve/settle. LiteLLM and Bifrost, as
   configured, do no per-request database work at all (LiteLLM runs with no master key — even
   its own auth is off).
3. **Velocity cap raised, not disabled, and documented:** the bench key's cap is 2×10⁹
   tokens/min so it never binds (peak observed usage ≈ 3.8×10⁷ tokens/min at baseline rates);
   free-lane requests still execute the cap check — raising the value changes the comparison,
   not the work.
4. **Localhost hides the keep-alive fix's biggest win.** Upstream clients are shared and
   keep-alive (`wire.rs::shared_upstream_clients`); over loopback HTTP there is no TLS
   handshake to save, so against a real HTTPS upstream the relative gap between ZeroRouter/
   Bifrost (pooled) and per-request-connection setups widens beyond what this table shows.
5. **ZeroRouter's CPU/RSS columns exclude Postgres**, which does most of that lane's work; the
   free-lane saturation cell shows 10% CPU on the router while the database is the bottleneck.
6. **Local Postgres understates production metering latency** (sub-ms fsync'd commits on NVMe
   vs a networked RDS with synchronous replication). Real metered overhead is higher than
   shown, not lower.
7. **LiteLLM ran single-worker** (its default; GIL-bound). `--num_workers N` scales throughput
   roughly linearly at N× memory. Its fixed-rate latency is the apples-to-apples figure.
8. **Not directly comparable to the workspace prototype's numbers** (metered p50 8.38 ms,
   221/s): different rev (pre keep-alive fix), different upstream wire (Responses vs
   chat-completions), and no per-cell usage reset there.
9. **Tiny payloads, non-streaming, single trial per cell.** This isolates fixed per-request
   overhead; it does not model large-context or streaming workloads. Treat small deltas as
   noise; the shape of the large ones (history scan, GIL bound, lock-vs-users) is what
   replicates.

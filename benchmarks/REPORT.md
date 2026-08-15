# Gateway overhead benchmark — ZeroRouter (two lanes) vs LiteLLM vs Bifrost

**Date:** 2026-08-14 · **Machine:** MacBook Pro (M2 Pro, 10 cores, 32 GB) · **Load gen:** oha 1.15.0
**ZeroRouter build:** `2a89e09` (origin/main, includes the migration-0019 monthly-spend rollup)
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

> ### The headline, and what changed since the previous run
> An earlier run of this same harness (kept below as the appendix) found ZeroRouter's dominant
> per-request cost was admission **re-scanning the user's month-to-date usage history** — p50
> 0.86 ms with an empty month vs 18.07 ms at 30k rows, a cost that made the free lane LOSE its
> apples-to-apples cell to LiteLLM. Migration 0019 replaced that scan with a trigger-maintained
> rollup (`usage_key_month_spend`). Re-measured on the same machine and methodology:
>
> - **The history curve is flat now:** sequential p50 0.78 ms at 0 rows, 1.28 ms at 30k rows
>   (p95 1.59 vs 1.49 ms — indistinguishable). Before: 0.86 → 18.07 ms.
> - **The free lane now beats LiteLLM at the median** (p50 1.89 vs 2.86 ms) and loses to it at
>   p90/p95 (4.60/7.97 vs 3.51/6.19 ms). It does **not** approach Bifrost (0.38 ms p50): the
>   free lane still pays one fsync'd Postgres admission commit per request that Bifrost does not.
> - **Multi-user metered throughput nearly doubled** to 1,310 req/s (16 users), with per-worker
>   p95 at ~63 ms (previously 762 req/s at ~230 ms p95).

---

## Fixed rate (100 req/s, 60 s per cell — the latency comparison)

Constant 100 req/s (`oha -q 100 --latency-correction`), non-streaming, identical tiny body,
6,000 requests per cell. Well below every target's ceiling. ZeroRouter cells start from a reset
usage state (see "Methodology: usage-state reset").

| Target | p50 (ms) | p95 (ms) | p99 (ms) | Overhead vs baseline (p50) | Throughput | Success | CPU % (100 = 1 core) | Peak RSS (MB) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **Baseline** (mock direct, no gateway) | 0.14 | 0.25 | 0.35 | — | 100/s | 100% | 1 | 24 |
| **Bifrost** (Go) | 0.38 | 0.58 | 0.76 | +0.23 ms | 100/s | 100% | 13 | 476 |
| **LiteLLM** (Python, 1 worker) | 2.86 | 6.19 | 28.75 | +2.72 ms | 100/s | 100% | 30 | 366 |
| **ZeroRouter free lane** (Rust, metering skipped) | 1.89 | 7.97 | 16.47 | +1.75 ms | 100/s | 100% | 5 | 22 |
| **ZeroRouter metered** (Rust, full reserve→settle, single key) | 3.27 | 4.03 | 4.40 | +3.13 ms | 100/s | 100% | 8 | 23 |

Reading the ZeroRouter rows:

- **Free lane: p50 1.89 ms — ahead of LiteLLM's 2.86, behind Bifrost's 0.38.** The lane's floor
  is the one synchronous admission commit (auth + caps + `last_used_at`, fsync ON); Bifrost does
  no per-request database work at all.
- **The tails trade rather than dominate:** free-lane p90/p95 (4.60/7.97 ms) are worse than
  LiteLLM's (3.51/6.19), while p99 is better (16.47 vs 28.75 — LiteLLM's p99 blew out this run).
  Single trial per cell; treat tail deltas of this size as noisy.
- **The metered lane is now the TIGHTER distribution** (p50 3.27, p99 4.40) — full prepaid
  accounting adds +3.13 ms p50 over baseline, down from +4.35 ms pre-rollup and +8.09 ms in the
  original prototype. One untested hypothesis for metered p99 < free p99, flagged rather than
  asserted: every usage insert now takes the accrual trigger's row lock on the same
  (key, month) rollup bucket; the metered lane's advisory lock happens to serialize those
  writes, while the free lane's async inserts contend on the bucket unserialized. The
  free-lane insert's own `lock_timeout` (added in `2a89e09` for exactly this bucket) points the
  same direction.

## Saturation (open loop, 50 connections, 60 s per cell — the throughput comparison)

| Target | Concurrency | Throughput (req/s) | p50 (ms) | p95 (ms) | p99 (ms) | Success | CPU % | Peak RSS (MB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| **Baseline** (mock direct, no gateway) | 50 conns | **72,679** | 0.40 | 2.06 | 3.03 | 100% | 384 | 28 |
| **Bifrost** (Go) | 50 conns | **15,585** | 2.57 | 7.69 | 13.15 | 100% | 552 | 854 |
| **LiteLLM** (Python, 1 worker) | 50 conns | **433** | 112.19 | 120.82 | 250.46 | 100% | 100 | 324 |
| **ZeroRouter free lane** (Rust, metering skipped) | 50 conns | **309** | 148.00 | 282.90 | 896.51 | 100% | 15 | 23 |
| **ZeroRouter metered** (Rust, full reserve→settle, single key) | 50 conns | **277** | 174.86 | 287.20 | 328.18 | 100% | 21 | 23 |
| **ZeroRouter metered** (16 users × 4 conns) | 16×4 conns | **1,310** | 49.26 | 63.09 | 71.73 | 100% | 128 | 24 |

Multi-user latency cells are the **median worker's** percentile (per-worker spread: p50
48.55–49.52 ms, p95 61.52–63.43 ms, p99 70.43–73.48 ms across 16 workers); throughput is the
exact sum of per-worker rates over the same wall window. oha exposes no raw samples, and pooling
percentiles across workers would fabricate a number.

Reading it:

- **Single-key metered (277/s) is a floor, not a ceiling: 16 users moved 1,310/s through the
  same router process** — 4.7× — at 49 ms median instead of 175 ms. The per-user advisory lock
  serializes only within a user, exactly as designed. Router CPU was 128% of one core; the
  bottleneck remains Postgres (unsampled), not the router.
- **The free lane saturates at 309/s single-user** — the admission commit and the async `$0`
  usage insert (both against one fsync'd Postgres, both hitting one user's rows) bound it, and
  its p99 (897 ms) shows the contention. Pre-rollup this cell was 214/s, so the rollup helped
  here too, but this remains the least flattering ZeroRouter number and a single-user shape.
- LiteLLM's single worker is GIL-bound at exactly one core. Bifrost saturates near 15.6k/s on
  ~5.5 cores at ~854 MB peak RSS vs ZeroRouter's 23 MB.

## The month-to-date history cost — now measured FLAT (the rollup's receipt)

Sequential single-connection free-lane requests (`./run.sh history`, 500 requests per point,
same release build), with the user's month-to-date `usage_events` count seeded to each level.
The 30k seed inserts through ordinary origin-mode statements, so the 0019 accrual trigger
maintains the rollup exactly as production writes do (asserted by the harness: the rollup
accrued exactly 30,000 × $0.0000132 = **$0.396** for the seeded key).

| Month-to-date rows | p50 (ms) — pre-rollup (`690f089`) | p50 (ms) — post-rollup (`2a89e09`) |
|---|---:|---:|
| 0 | 0.86 | 0.78 |
| 30,000 | **18.07** | **1.28** |

Post-rollup p95: 1.59 ms (0 rows) vs 1.49 ms (30k rows) — statistically indistinguishable. The
pre-rollup slope was ~0.57 µs per accumulated row on every request; post-rollup the residual
median shift (~0.5 ms) is within this harness's run-to-run noise and does not reproduce at p95.
Admission cost no longer depends on how much the account has used this month. This command stays
in the matrix as the regression tripwire for that property.

## What each lane actually does per request (so the numbers have referents)

- **Free lane:** cached-key auth → in-memory route resolution → **one admission transaction**
  (api_keys row lock, spend read from the `usage_key_month_spend` rollup + one-minute velocity
  window scan, balance read, `last_used_at` stamp, synchronous commit against fsync'd Postgres)
  → dispatch over a shared keep-alive client → async `$0` usage-event insert (off-path, fires
  the rollup accrual trigger, bounded by a 5 s `lock_timeout`). No advisory lock, no
  reservation, no settle. The free lane is deliberately **not** DB-free: it still authenticates
  and still enforces the velocity cap (abuse control counts free tokens by design).
- **Metered lane:** the same, plus the per-user `pg_advisory_xact_lock`, a reservation INSERT in
  the admission transaction, and after dispatch a settle transaction (advisory lock again,
  `DELETE ... RETURNING` the reservation, clamped debit, ledger + usage + attempts writes).

## Integrity fixes made before trusting any number

This harness replaced a workspace prototype whose numbers had two defects the design doc flagged
(and one it did not). All three are fixed and gated; a fourth adaptation came with migration 0019.

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
   16-user metered cell (16 oha workers, one key each, exact-sum throughput): 1,310/s vs 277/s
   single-key on this run, same process, same database.
3. **Benchmark cells no longer poison each other through per-user history** (the defect the
   prototype did not know it had, and the measurement that motivated migration 0019). In the
   first full run of this harness, cells degraded with cell *order* — the metered fixed cell
   inherited ~26k rows written by the free-lane cells and reported a p50 of 792 ms that was
   really the pre-rollup cap scan. Usage state is truncated before every measured ZeroRouter
   cell, and the history cost is measured deliberately (section above) instead of accidentally.
4. **The 0019 rollup guard, adapted to rather than fought.** `usage_key_month_spend` refuses
   direct writes and TRUNCATE (trigger-fenced, like the ledger it derives from). The harness's
   per-cell reset runs under `session_replication_role = replica`, which silences the accrual
   trigger and the guard together — precisely the restore-shaped bypass 0019's own comments
   document, with the rule that afterwards the rollup must be **rebuilt from `usage_events`,
   never patched**. The reset satisfies that rule in its degenerate form by truncating the
   rollup **in the same statement as the ledger**: empty ledger, empty rollup, consistent by
   construction (and asserted). Leaving the rollup standing would strand phantom spend for
   admission to enforce against an empty ledger. The `history` seed takes the opposite path on
   purpose — ordinary origin-mode INSERTs, so the accrual trigger fires and the rollup is
   populated the way production populates it.

### Free-lane skip verification (structural, not observational)

`./run.sh verify` proves the stage-3 metering skip with cumulative counters that cannot miss
insert-then-delete pairs (`pg_stat_user_tables.n_tup_ins`; settle DELETEs its reservation, so
`count(*)` would lie). From this run:

```
300 free-lane requests  -> reservation inserts +0, credit_ledger rows +0, $0 usage rows +300
50 metered requests     -> reservation inserts +50, credit_ledger rows +50, priced usage rows +50
```

The metered control proves the counters would have caught a leak. The `$0` usage rows land
asynchronously with `cost_usd = 0` (row-level check: `all cost 0: true`).
One measurement note: PostgreSQL's cumulative stats flush lazily (idle backends on a ~10 s
timeout), so the check reads `n_tup_ins` only after it has been stable for 12 s — both a
fixed-sleep read and a naive "two reads agree" read produced false counts while building this.

## Methodology: usage-state reset

Every ZeroRouter measured cell starts from `TRUNCATE usage_events, usage_reservations,
request_attempts, usage_key_month_spend` (one replica-mode statement) + `ANALYZE` + `CHECKPOINT`
("fresh month"). Within a cell, the traffic's own rows accumulate — that within-cell growth is
real behavior at that arrival rate and is left in; post-0019 it no longer moves admission cost.
The CHECKPOINT also prevents one cell's WAL debt from bleeding into the next cell's tail.

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
| ZeroRouter | this repo @ `2a89e09` (origin/main; monthly-spend rollup included), `cargo build --release`, default features (no `testing`) |
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

1. **The free lane wins the median against LiteLLM and trades the tail; it does not approach
   Bifrost.** Its floor is one synchronous fsync'd Postgres commit per request (auth + caps) —
   work LiteLLM and Bifrost, as configured here, simply do not do. Single trial per cell: the
   free lane's p95 (7.97) vs LiteLLM's (6.19), and LiteLLM's own p99 blowout (28.75), are
   within the kind of variation one trial cannot separate.
2. **ZeroRouter does real DB work the others don't, on both lanes.** Free = auth + caps + one
   synchronous commit + an async usage insert; metered = that plus lock/reserve/settle. LiteLLM
   and Bifrost do no per-request database work at all (LiteLLM runs with no master key — even
   its own auth is off).
3. **Velocity cap raised, not disabled, and documented:** the bench key's cap is 2×10⁹
   tokens/min so it never binds; free-lane requests still execute the cap check — raising the
   value changes the comparison, not the work.
4. **Localhost hides the keep-alive fix's biggest win.** Upstream clients are shared and
   keep-alive (`wire.rs::shared_upstream_clients`); over loopback HTTP there is no TLS
   handshake to save, so against a real HTTPS upstream the relative gap between ZeroRouter/
   Bifrost (pooled) and per-request-connection setups widens beyond what this table shows.
5. **ZeroRouter's CPU/RSS columns exclude Postgres**, which does much of that lane's work; the
   free-lane saturation cell shows 15% CPU on the router while the database is the bottleneck.
6. **Local Postgres understates production metering latency** (sub-ms fsync'd commits on NVMe
   vs a networked RDS with synchronous replication). Real metered overhead is higher than
   shown, not lower.
7. **LiteLLM ran single-worker** (its default; GIL-bound). `--num_workers N` scales throughput
   roughly linearly at N× memory. Its fixed-rate latency is the apples-to-apples figure.
8. **The metered-tail-tighter-than-free-tail observation is unexplained by measurement.** The
   rollup-bucket-contention hypothesis in the fixed-rate section is exactly that — a
   hypothesis, consistent with `2a89e09`'s own lock-timeout rationale, not something this
   harness isolated.
9. **Tiny payloads, non-streaming, single trial per cell.** This isolates fixed per-request
   overhead; it does not model large-context or streaming workloads. Treat small deltas as
   noise; the shape of the large ones (rollup flatness, GIL bound, lock-vs-users) is what
   replicates.

---

## Appendix: the pre-rollup run (same harness, ZeroRouter `690f089`, 2026-08-14)

Kept as the attribution record: this is the run that isolated the month-to-date scan as
ZeroRouter's dominant cost and motivated migration 0019. Same machine, same methodology, same
integrity gates; the only change between this table and the headline table is the rollup.

### Fixed rate (100 req/s, 60 s per cell)

| Target | p50 (ms) | p95 (ms) | p99 (ms) | Overhead vs baseline (p50) |
|---|---:|---:|---:|---:|
| Baseline (mock direct) | 0.13 | 0.23 | 0.33 | — |
| Bifrost (Go) | 0.36 | 0.52 | 0.65 | +0.22 ms |
| LiteLLM (Python, 1 worker) | 2.74 | 3.29 | 3.97 | +2.61 ms |
| ZeroRouter free lane | 3.20 | 4.61 | 4.99 | +3.06 ms |
| ZeroRouter metered (single key) | 4.48 | 6.05 | 13.31 | +4.35 ms |

### Saturation (open loop, 60 s per cell)

| Target | Throughput (req/s) | p50 (ms) | p95 (ms) | p99 (ms) |
|---|---:|---:|---:|---:|
| Baseline (mock direct) | 72,844 | 0.41 | 2.02 | 2.97 |
| Bifrost (Go) | 15,640 | 2.58 | 7.53 | 13.10 |
| LiteLLM (Python, 1 worker) | 432 | 111.38 | 124.57 | 243.04 |
| ZeroRouter free lane | 214 | 230.32 | 427.25 | 532.69 |
| ZeroRouter metered (single key) | 172 | 317.49 | 439.53 | 496.18 |
| ZeroRouter metered (16 users × 4 conns) | 762 | 52.50 | 229.55 | 280.46 |

(Multi-user cells: median worker's percentiles; throughput = exact per-worker sum.)

### History curve (the measurement that became migration 0019)

Sequential single-connection free-lane p50: **0.86 ms at 0 month-to-date rows, 18.07 ms at
30,000** (~0.57 µs per accumulated row, paid by every request on both lanes). In that run the
free lane lost its apples-to-apples cell to LiteLLM (3.20 vs 2.74 ms p50) because the cell's own
6,000 writes grew the scan as it ran; the free lane's fresh-state floor was already 0.86 ms.
The headline tables above are the same cells after the scan became a rollup read.

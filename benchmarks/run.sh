#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Gateway overhead benchmark: ZeroRouter (free + metered lanes) vs LiteLLM
# vs Bifrost, over an identical local mock upstream.
#
# Two honest lanes (docs/design/edge-mode-local-rung.md, "Benchmark plan"):
#
#   Lane 1 — FREE/LOCAL routes: apples-to-apples stateless forwarding, the
#     job LiteLLM and Bifrost actually do. ZeroRouter's free lane (edge mode
#     stage 3) skips reserve/lock/settle because every candidate is free AND
#     the tier sells at $0.
#   Lane 2 — METERED cloud routes: prepaid can't-overspend accounting that no
#     competitor performs. Reported as what it is, in BOTH shapes: single-key
#     (worst case — every request serializes on one user's advisory lock) and
#     multi-user (16 users, the shape production traffic has).
#
# Both ZeroRouter lanes use the same chat_completions adapter and dial the
# same mock endpoint; the only difference between them is the metering work.
#
# Everything is local. No real provider keys, no production, no network
# egress on the request path. A dedicated scratch Postgres (port 5545) backs
# ZeroRouter's real prepaid metering path with fsync/synchronous_commit ON.
#
# Usage:
#   ./run.sh up        # build + start scratch PG, mock, ZR, LiteLLM, Bifrost
#   ./run.sh verify    # integrity gate: parsed-output fidelity + metering-skip proof
#   ./run.sh bench     # run the load matrix -> results/results.json (runs verify first)
#   ./run.sh report    # render results/results.json as markdown tables
#   ./run.sh all       # up + bench (bench includes verify) + report   (default)
#   ./run.sh down      # stop everything started by `up`
#
# Requirements: oha, go, cargo, postgresql@16 (brew), python3, and a cached
# Bifrost binary (`npx -y @maximhq/bifrost@1.6.10 --help` downloads it).
# ---------------------------------------------------------------------------
set -uo pipefail

BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ZR_SRC="$BENCH/../router"
RUN="$BENCH/.run"
PGBIN="/opt/homebrew/opt/postgresql@16/bin"
PGDATA="$RUN/pgdata"
PGPORT="${BENCH_PGPORT:-5545}"        # deliberately not 5432: never a shared/system cluster
DBURL="postgres://postgres@127.0.0.1:${PGPORT}/zerorouter_bench"

MOCK_PORT=9010
ZR_PORT=8080
LITELLM_PORT=8081
BIFROST_PORT=8082

BIFROST_VERSION=1.6.10
BIFROST_BIN="$HOME/Library/Caches/bifrost/v${BIFROST_VERSION}/bin/bifrost-http-0"
VENV="$RUN/litellm-venv"

# Load parameters ------------------------------------------------------------
WARMUP_SECS=5
FIXED_RATE=100          # constant req/s, sub-saturation for every target
FIXED_SECS=60           # 60 s * 100 rps = 6,000 requests per fixed cell
SAT_CONNS=50            # open-loop saturation concurrency
SAT_SECS=60             # >= 60 s per saturation cell
MULTI_USERS=16          # distinct users/keys in the multi-user metered cell
MULTI_CONNS_PER_USER=4  # 16 users x 4 conns = 64 concurrent, like-for-like with -c 50..64

mkdir -p "$RUN" "$RUN/keys" "$BENCH/results" "$BENCH/logs"
KEYFILE="$RUN/keys/bench.key"

log(){ printf '\033[36m[run]\033[0m %s\n' "$*"; }
die(){ printf '\033[31m[run] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

port_up(){ nc -z 127.0.0.1 "$1" >/dev/null 2>&1; }

wait_http(){ # url  tries
  local url="$1" tries="${2:-40}" i
  for ((i=0;i<tries;i++)); do
    curl -s -o /dev/null "$url" && return 0
    sleep 0.5
  done
  return 1
}

psql_bench(){ "$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -d zerorouter_bench -tA "$@"; }

# --- bring-up ---------------------------------------------------------------
start_pg(){
  [ -x "$PGBIN/pg_ctl" ] || die "postgresql@16 not found at $PGBIN (brew install postgresql@16)"
  if "$PGBIN/pg_ctl" -D "$PGDATA" status >/dev/null 2>&1; then
    log "postgres already running on $PGPORT"; return
  fi
  if [ ! -d "$PGDATA" ]; then
    log "initdb $PGDATA"
    "$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null 2>&1 || die "initdb failed"
  fi
  log "starting postgres on $PGPORT (fsync/synchronous_commit ON = real durability)"
  "$PGBIN/pg_ctl" -D "$PGDATA" -o "-p $PGPORT -c listen_addresses=127.0.0.1" \
    -l "$BENCH/logs/pg.log" start || die "pg_ctl start failed"
  sleep 2
  "$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -d postgres -tAc \
    "SELECT 1 FROM pg_database WHERE datname='zerorouter_bench'" | grep -q 1 || \
    "$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -d postgres \
      -c "CREATE DATABASE zerorouter_bench;" >/dev/null
}

build_zr(){
  if [ -x "$ZR_SRC/target/release/zerorouter" ]; then
    log "zerorouter binary present (skip build)"; return; fi
  log "building zerorouter --release (default features; testing feature stays off)"
  ( cd "$ZR_SRC" && cargo build --release --bin zerorouter ) \
    >"$BENCH/logs/zerorouter-build.log" 2>&1 \
    || die "zerorouter build failed (see logs/zerorouter-build.log)"
}

mint_user(){ # email name keyfile credit_usd
  local email="$1" name="$2" keyfile="$3" credit="$4" out
  out=$(DATABASE_URL="$DBURL" "$ZR_SRC/target/release/zerorouter" admin mint-key \
        --email "$email" --name "$name" \
        --spend-cap-usd 1000000 --velocity-cap-tokens-per-min 2000000000 \
        2>>"$BENCH/logs/seed.log") || die "mint-key $email failed"
  printf '%s' "$out" | python3 -c "import sys,json;print(json.load(sys.stdin)['api_key'])" \
    > "$keyfile" || die "mint-key $email returned no api_key: $out"
  DATABASE_URL="$DBURL" "$ZR_SRC/target/release/zerorouter" admin grant-credit \
    --email "$email" --amount-usd "$credit" --note bench >>"$BENCH/logs/seed.log" 2>&1
}

seed_zr(){
  if [ -s "$KEYFILE" ]; then log "bench users already seeded"; return; fi
  # Velocity cap note: the cap counts tokens on BOTH lanes (free usage counts
  # toward it by design). 2e9 tokens/min is far above anything a saturation
  # run here generates (~21 tokens/request x ~30k req/s = ~38M tokens/min),
  # so the cap never binds; it is raised, not disabled, and this is the only
  # non-default account setting.
  log "seeding bench user (+\$10000) and $MULTI_USERS multi-users (+\$1000 each)"
  mint_user bench@bench.local bench "$KEYFILE" 10000
  local i
  for i in $(seq -w 1 "$MULTI_USERS"); do
    mint_user "bench$i@bench.local" "bench$i" "$RUN/keys/bench$i.key" 1000
  done
}

build_mock(){
  ( cd "$BENCH/mock-upstream" && go build -o "$RUN/mock-upstream" . ) || die "mock build failed"
}

start_mock(){
  port_up "$MOCK_PORT" && { log "mock already up on $MOCK_PORT"; return; }
  build_mock
  log "starting mock upstream on $MOCK_PORT"
  ( MOCK_PORT=$MOCK_PORT "$RUN/mock-upstream" >"$BENCH/logs/mock.log" 2>&1 & )
  wait_http "http://127.0.0.1:$MOCK_PORT/healthz" || die "mock did not come up"
}

start_zr(){
  port_up "$ZR_PORT" && { log "zerorouter already up on $ZR_PORT"; return; }
  build_zr
  log "starting zerorouter on $ZR_PORT (free + metered lanes -> mock)"
  ( cd "$ZR_SRC" && \
    DATABASE_URL="$DBURL" \
    ZEROROUTER_TIERS_PATH="$BENCH/configs/tiers.bench.toml" \
    ZEROROUTER_PROVIDERS_PATH="$BENCH/configs/providers.bench.json" \
    ZEROROUTER_BIND="127.0.0.1:$ZR_PORT" \
    METERED_MOCK_API_KEY="dummy-bench-key" \
    RUST_LOG="warn" \
    ./target/release/zerorouter serve >"$BENCH/logs/zerorouter.log" 2>&1 & )
  wait_http "http://127.0.0.1:$ZR_PORT/healthz" || die "zerorouter did not come up"
  seed_zr
}

start_litellm(){
  port_up "$LITELLM_PORT" && { log "litellm already up on $LITELLM_PORT"; return; }
  if [ ! -x "$VENV/bin/litellm" ]; then
    log "creating litellm venv (litellm==1.96.2, fastapi==0.136.3 — see REPORT.md for the pin's why)"
    python3 -m venv "$VENV" || die "venv creation failed"
    "$VENV/bin/pip" install --quiet 'litellm[proxy]==1.96.2' 'fastapi==0.136.3' \
      >"$BENCH/logs/litellm-install.log" 2>&1 || die "litellm install failed"
  fi
  log "starting litellm on $LITELLM_PORT (single worker, its default)"
  ( LITELLM_MODE=PRODUCTION "$VENV/bin/litellm" --config "$BENCH/configs/litellm.yaml" \
      --port "$LITELLM_PORT" --host 127.0.0.1 >"$BENCH/logs/litellm.log" 2>&1 & )
  wait_http "http://127.0.0.1:$LITELLM_PORT/health/liveliness" 60 || die "litellm did not come up"
}

start_bifrost(){
  port_up "$BIFROST_PORT" && { log "bifrost already up on $BIFROST_PORT"; return; }
  [ -x "$BIFROST_BIN" ] || die "bifrost binary missing; run once: npx -y @maximhq/bifrost@$BIFROST_VERSION --help"
  # Fresh app-dir every start: bifrost materializes config.json into a
  # config.db on first run and IGNORES config.json afterwards — a stale
  # app-dir is how a config fix silently fails to apply.
  rm -rf "$RUN/bifrost-app"; mkdir -p "$RUN/bifrost-app"
  cp "$BENCH/configs/bifrost.config.json" "$RUN/bifrost-app/config.json"
  log "starting bifrost v$BIFROST_VERSION on $BIFROST_PORT"
  ( "$BIFROST_BIN" -app-dir "$RUN/bifrost-app" -host 127.0.0.1 \
      -port "$BIFROST_PORT" -log-level warn >"$BENCH/logs/bifrost.log" 2>&1 & )
  wait_http "http://127.0.0.1:$BIFROST_PORT/metrics" 40 || sleep 3
}

cmd_up(){
  start_pg; start_mock; start_zr; start_litellm; start_bifrost
  log "all services up"
}

cmd_down(){
  log "stopping gateways + mock"
  pkill -f 'target/release/zerorouter serve' 2>/dev/null
  pkill -f 'litellm --config' 2>/dev/null
  pkill -f 'bifrost-http-0' 2>/dev/null
  pkill -f '.run/mock-upstream' 2>/dev/null
  "$PGBIN/pg_ctl" -D "$PGDATA" stop >/dev/null 2>&1
  log "down"
}

# --- integrity gate ---------------------------------------------------------
# No cell is benchmarked until (a) every gateway's PARSED output round-trips
# the mock's reply text — HTTP 200 alone proves nothing, the prototype's
# Bifrost run answered 200 with `choices: null` for weeks — and (b) the
# free-lane metering skip is structurally proven (no reservation inserts, no
# ledger writes, $0 usage rows written async).
REPLY_TEXT='Hello! How can I help you today?'

assert_choice(){ # name url extra-curl-args...
  local name="$1" url="$2"; shift 2
  local body content
  body=$(curl -s -X POST -H 'Content-Type: application/json' "$@" "$url")
  content=$(printf '%s' "$body" | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    print(d['choices'][0]['message']['content'])
except Exception as e:
    print(f'PARSE-FAILED: {e}')" 2>/dev/null)
  if [ "$content" != "$REPLY_TEXT" ]; then
    printf '%s' "$body" > "$BENCH/results/verify-$name.fail.json"
    die "$name integrity check FAILED: choices[0].message.content != reply text (body -> results/verify-$name.fail.json)"
  fi
  printf '%s' "$body" > "$BENCH/results/verify-$name.json"
  log "verify $name: parsed choices round-trip the reply text  OK"
}

# pg_stat n_tup_ins is a CUMULATIVE insert counter — unlike counting rows, it
# cannot miss reservations that were inserted and then settled away (settle
# DELETEs its reservation, so count(*) is 0 at rest on BOTH lanes). But the
# statistics are flushed lazily: a backend reports at transaction end at most
# once a second, an IDLE backend only flushes on a ~10 s idle-stats timeout,
# and a busy one may defer up to a minute. Both failure shapes were observed
# while writing this check: a 1 s fixed sleep read a baseline stale by one
# row, and a "two reads 1 s apart agree" loop locked in 4/50 inserts because
# the pool's idle connections had not hit their idle flush yet. So a value
# only counts as settled once it has been UNCHANGED FOR LONGER THAN THE IDLE
# FLUSH TIMEOUT: 7 consecutive equal reads at 2 s spacing (a 12 s window),
# with a 3-minute ceiling.
stable_reservation_inserts(){
  local prev cur i same=0
  prev=$(psql_bench -c "SELECT COALESCE(n_tup_ins,0) FROM pg_stat_user_tables WHERE relname='usage_reservations';")
  for i in $(seq 1 90); do
    sleep 2
    cur=$(psql_bench -c "SELECT COALESCE(n_tup_ins,0) FROM pg_stat_user_tables WHERE relname='usage_reservations';")
    if [ "$cur" = "$prev" ]; then
      same=$((same + 1))
      [ "$same" -ge 6 ] && { printf '%s' "$cur"; return; }
    else
      same=0
    fi
    prev="$cur"
  done
  printf '%s' "$cur"
}

verify_free_skip(){
  local key ins0 ins1 ledger0 ledger1 free0 free1 i
  key=$(cat "$KEYFILE")
  ins0=$(stable_reservation_inserts)
  ledger0=$(psql_bench -c "SELECT count(*) FROM credit_ledger;")
  free0=$(psql_bench -c "SELECT count(*) FROM usage_events WHERE cost_usd = 0;")
  log "verify free-lane skip: driving 300 free-lane requests"
  oha -n 300 -c 8 --no-tui --output-format quiet -m POST -T 'application/json' \
    -H "Authorization: Bearer $key" \
    -d '{"model":"local-mock/mock-model","messages":[{"role":"user","content":"Hello"}]}' \
    "http://127.0.0.1:$ZR_PORT/v1/chat/completions" >/dev/null 2>&1
  # usage rows are written ASYNC on the free lane; poll until they land.
  for i in $(seq 1 20); do
    free1=$(psql_bench -c "SELECT count(*) FROM usage_events WHERE cost_usd = 0;")
    [ $((free1 - free0)) -ge 300 ] && break
    sleep 0.5
  done
  ins1=$(stable_reservation_inserts)
  ledger1=$(psql_bench -c "SELECT count(*) FROM credit_ledger;")
  [ "$ins1" = "$ins0" ] || die "free-lane skip FAILED: usage_reservations took $((ins1 - ins0)) inserts during free traffic"
  [ "$ledger1" = "$ledger0" ] || die "free-lane skip FAILED: credit_ledger grew by $((ledger1 - ledger0)) rows during free traffic"
  [ $((free1 - free0)) -ge 300 ] || die "free-lane observability FAILED: only $((free1 - free0))/300 \$0 usage rows landed"
  log "verify free-lane skip: 300 requests -> reservation inserts +0, ledger rows +0, \$0 usage rows +$((free1 - free0))  OK"

  # The metered control: the same traffic on the metered pin MUST reserve and
  # settle — this proves the counters above would have moved.
  ins0="$ins1"; ledger0="$ledger1"
  local metered0 metered1
  metered0=$(psql_bench -c "SELECT count(*) FROM usage_events WHERE cost_usd > 0;")
  log "verify metered control: driving 50 metered requests"
  oha -n 50 -c 4 --no-tui --output-format quiet -m POST -T 'application/json' \
    -H "Authorization: Bearer $key" \
    -d '{"model":"metered-mock/mock-model","messages":[{"role":"user","content":"Hello"}]}' \
    "http://127.0.0.1:$ZR_PORT/v1/chat/completions" >/dev/null 2>&1
  sleep 2
  ins1=$(stable_reservation_inserts)
  ledger1=$(psql_bench -c "SELECT count(*) FROM credit_ledger;")
  metered1=$(psql_bench -c "SELECT count(*) FROM usage_events WHERE cost_usd > 0;")
  [ $((ins1 - ins0)) -ge 50 ] || die "metered control FAILED: only $((ins1 - ins0))/50 reservation inserts"
  [ $((metered1 - metered0)) -ge 50 ] || die "metered control FAILED: only $((metered1 - metered0))/50 priced usage rows"
  log "verify metered control: 50 requests -> reservation inserts +$((ins1 - ins0)), ledger rows +$((ledger1 - ledger0)), priced usage rows +$((metered1 - metered0))  OK"

  psql_bench -c "SELECT 'free-lane rows: ' || count(*) || ', all cost 0: ' || bool_and(cost_usd = 0)
                 FROM usage_events WHERE tier = 'local-mock/mock-model';" | tee "$BENCH/results/verify-free-skip.txt"
}

cmd_verify(){
  local key; key=$(cat "$KEYFILE" 2>/dev/null) || die "no bench key; run ./run.sh up first"
  assert_choice baseline "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" \
    -d '{"model":"mock-model","messages":[{"role":"user","content":"Hello"}]}'
  assert_choice bifrost "http://127.0.0.1:$BIFROST_PORT/v1/chat/completions" \
    -d '{"model":"openai/mock-model","messages":[{"role":"user","content":"Hello"}]}'
  assert_choice litellm "http://127.0.0.1:$LITELLM_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer dummy' \
    -d '{"model":"mock-model","messages":[{"role":"user","content":"Hello"}]}'
  assert_choice zerorouter-free "http://127.0.0.1:$ZR_PORT/v1/chat/completions" \
    -H "Authorization: Bearer $key" \
    -d '{"model":"local-mock/mock-model","messages":[{"role":"user","content":"Hello"}]}'
  assert_choice zerorouter-metered "http://127.0.0.1:$ZR_PORT/v1/chat/completions" \
    -H "Authorization: Bearer $key" \
    -d '{"model":"metered-mock/mock-model","messages":[{"role":"user","content":"Hello"}]}'
  verify_free_skip
  log "ALL integrity checks passed"
}

# --- metrics helpers --------------------------------------------------------
cputime_secs(){ # pid -> cumulative CPU seconds
  ps -o cputime= -p "$1" 2>/dev/null | tr -d ' ' | python3 -c "
import sys
s=sys.stdin.read().strip()
if not s: print(0.0); raise SystemExit
p=s.split(':'); sec=float(p[-1])
if len(p)>=2: sec+=int(p[-2])*60
if len(p)>=3: sec+=int(p[-3])*3600
print(sec)"
}

pid_on_port(){ lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | head -1; }

# runs oha and samples the target PID's peak RSS + CPU delta; emits one JSON
# object on stdout. args: name mode pid rate secs conns url [curl-ish oha args...]
run_case(){
  local name="$1" mode="$2" pid="$3" rate="$4" secs="$5" conns="$6" url="$7"; shift 7
  local rssfile; rssfile=$(mktemp)
  ( while :; do ps -o rss= -p "$pid" 2>/dev/null | tr -d ' '; sleep 0.4; done >>"$rssfile" ) &
  local sampler=$!
  local cpu0; cpu0=$(cputime_secs "$pid")
  local qflag=()
  [ "$rate" != "0" ] && qflag=(-q "$rate" --latency-correction)
  local out
  # NOTE: ${qflag[@]+"..."} guard is required — macOS /bin/bash is 3.2, where
  # "${empty_array[@]}" under `set -u` aborts with "unbound variable".
  out=$(oha -z "${secs}s" -c "$conns" ${qflag[@]+"${qflag[@]}"} --no-tui --output-format json \
        -m POST -T 'application/json' "$@" "$url" 2>/dev/null)
  local cpu1; cpu1=$(cputime_secs "$pid")
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
  printf '%s' "$out" | RSSFILE="$rssfile" CPU0="$cpu0" CPU1="$cpu1" \
    NAME="$name" MODE="$mode" python3 -c "
import sys,json,os
d=json.load(sys.stdin)
s=d['summary']; lp=d['latencyPercentiles']
wall=s['total']
cpu=(float(os.environ['CPU1'])-float(os.environ['CPU0']))
rss=[int(x) for x in open(os.environ['RSSFILE']).read().split() if x.strip().isdigit()]
peak=max(rss)/1024.0 if rss else 0.0
print(json.dumps({
  'name':os.environ['NAME'],'mode':os.environ['MODE'],
  'rps':round(s['requestsPerSec'],1),'success':s['successRate'],
  'total_requests':int(round(s['requestsPerSec']*wall)),
  'wall_s':round(wall,2),
  'p50_ms':round(lp['p50']*1000,3),'p90_ms':round(lp['p90']*1000,3),
  'p95_ms':round(lp['p95']*1000,3),'p99_ms':round(lp['p99']*1000,3),
  'cpu_pct':round(cpu/wall*100,1) if wall else 0.0,
  'peak_rss_mb':round(peak,1),
}))"
  rm -f "$rssfile"
}

# Multi-user metered saturation: N distinct users/keys drive load CONCURRENTLY
# (one oha process per key), so per-user advisory locks do not contend across
# users. Throughput is the exact sum of per-worker rates over the same wall
# window; per-worker latency percentiles are kept (min/median/max across
# workers) rather than pooled, because oha does not expose raw samples and
# averaging percentiles would fabricate a number.
run_multiuser(){ # name pid secs url body
  local name="$1" pid="$2" secs="$3" url="$4" body="$5"
  local rssfile; rssfile=$(mktemp)
  ( while :; do ps -o rss= -p "$pid" 2>/dev/null | tr -d ' '; sleep 0.4; done >>"$rssfile" ) &
  local sampler=$!
  local cpu0; cpu0=$(cputime_secs "$pid")
  local outdir; outdir=$(mktemp -d)
  local i workers=()
  for i in $(seq -w 1 "$MULTI_USERS"); do
    ( oha -z "${secs}s" -c "$MULTI_CONNS_PER_USER" --no-tui --output-format json \
        -m POST -T 'application/json' \
        -H "Authorization: Bearer $(cat "$RUN/keys/bench$i.key")" \
        -d "$body" "$url" >"$outdir/$i.json" 2>/dev/null ) &
    workers+=($!)
  done
  # Wait for the oha workers ONLY — a bare `wait` would also wait on the RSS
  # sampler, which never exits on its own (found the hard way: the first full
  # run parked here forever after all 16 workers had finished).
  wait ${workers[@]+"${workers[@]}"}
  local cpu1; cpu1=$(cputime_secs "$pid")
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
  OUTDIR="$outdir" RSSFILE="$rssfile" CPU0="$cpu0" CPU1="$cpu1" NAME="$name" \
  SECS="$secs" USERS="$MULTI_USERS" CONNS="$MULTI_CONNS_PER_USER" python3 -c "
import sys,json,os,glob,statistics
files=sorted(glob.glob(os.path.join(os.environ['OUTDIR'],'*.json')))
workers=[json.load(open(f)) for f in files]
rps=sum(w['summary']['requestsPerSec'] for w in workers)
total=sum(int(round(w['summary']['requestsPerSec']*w['summary']['total'])) for w in workers)
succ=(sum(w['summary']['successRate']*w['summary']['requestsPerSec'] for w in workers)/rps) if rps else 0.0
wall=max(w['summary']['total'] for w in workers)
cpu=(float(os.environ['CPU1'])-float(os.environ['CPU0']))
rss=[int(x) for x in open(os.environ['RSSFILE']).read().split() if x.strip().isdigit()]
peak=max(rss)/1024.0 if rss else 0.0
def spread(k):
    vals=sorted(w['latencyPercentiles'][k]*1000 for w in workers)
    return {'min':round(vals[0],3),'median':round(statistics.median(vals),3),'max':round(vals[-1],3)}
print(json.dumps({
  'name':os.environ['NAME'],'mode':'saturation-multiuser',
  'users':int(os.environ['USERS']),'conns_per_user':int(os.environ['CONNS']),
  'rps':round(rps,1),'success':round(succ,4),'total_requests':total,
  'wall_s':round(wall,2),
  'p50_ms_workers':spread('p50'),'p95_ms_workers':spread('p95'),'p99_ms_workers':spread('p99'),
  'cpu_pct':round(cpu/wall*100,1) if wall else 0.0,
  'peak_rss_mb':round(peak,1),
}))"
  rm -rf "$outdir" "$rssfile"
}

# --- usage-state reset between ZeroRouter cells ------------------------------
# Admission prices every request against the user's MONTH-TO-DATE usage: the
# spend/velocity-cap query scans all of the user's usage_events rows since the
# start of the month (src/db.rs documents ~14 ms at 30k rows). The benchmark
# writes usage rows at benchmark rates, so without a reset each ZeroRouter
# cell runs against the history the previous cells wrote — the first full run
# of this harness had the metered fixed-rate cell inherit ~26k rows and
# report a p50 of 792 ms that was really "the cap scan over the free-lane
# cells' leftovers", not the metered path. Resetting between measured cells
# makes every cell start from the same state (fresh month). The growth cost
# itself is real and is measured DELIBERATELY by `./run.sh history` instead
# of accidentally by cell ordering.
#
# usage_events is append-only behind triggers that reject UPDATE, DELETE,
# *and* TRUNCATE — the discipline is thorough, and this harness does not get
# an exemption for free. The reset therefore bypasses the triggers EXPLICITLY
# (session_replication_role=replica, a superuser-only switch), in one
# statement, on a scratch database this harness itself created. That is a
# disclosed measurement-rig action, not something the production role could
# do. CHECKPOINT flushes the WAL debt the previous cell built up so it cannot
# bleed into the next cell's tail (the prototype documented exactly that
# bleed), and ANALYZE keeps the planner's stats matching the now-empty tables.
reset_usage_state(){
  # request_attempts rides along: it holds a foreign key into usage_events,
  # so the two must truncate together.
  psql_bench -v ON_ERROR_STOP=1 \
    -c "SET session_replication_role = replica; TRUNCATE usage_events, usage_reservations, request_attempts;" \
    >/dev/null || die "usage-state reset failed"
  local left
  left=$(psql_bench -c "SELECT count(*) FROM usage_events;")
  [ "$left" = "0" ] || die "usage-state reset left $left usage_events rows"
  psql_bench -c "ANALYZE usage_events; ANALYZE usage_reservations; ANALYZE request_attempts;" >/dev/null
  psql_bench -c "CHECKPOINT;" >/dev/null
}

CHAT_BODY='{"model":"mock-model","messages":[{"role":"user","content":"Hello"}]}'
BF_BODY='{"model":"openai/mock-model","messages":[{"role":"user","content":"Hello"}]}'
ZR_FREE_BODY='{"model":"local-mock/mock-model","messages":[{"role":"user","content":"Hello"}]}'
ZR_METERED_BODY='{"model":"metered-mock/mock-model","messages":[{"role":"user","content":"Hello"}]}'

bench_target(){ # name reset_flag pid url [oha args...]
  local name="$1" reset="$2" pid="$3" url="$4"; shift 4
  log "warmup $name (${WARMUP_SECS}s)"
  oha -z "${WARMUP_SECS}s" -c 20 --no-tui --output-format quiet -m POST -T 'application/json' "$@" "$url" >/dev/null 2>&1
  [ "$reset" = "reset" ] && reset_usage_state
  log "fixed-rate ($FIXED_RATE rps, ${FIXED_SECS}s) $name"
  run_case "$name" fixed "$pid" "$FIXED_RATE" "$FIXED_SECS" 200 "$url" "$@" >>"$BENCH/results/results.jsonl"
  [ "$reset" = "reset" ] && reset_usage_state
  log "saturation (-c $SAT_CONNS, ${SAT_SECS}s) $name"
  run_case "$name" saturation "$pid" 0 "$SAT_SECS" "$SAT_CONNS" "$url" "$@" >>"$BENCH/results/results.jsonl"
}

cmd_bench(){
  cmd_verify
  : > "$BENCH/results/results.jsonl"
  local mock_pid zr_pid ll_pid bf_pid key
  mock_pid=$(pid_on_port "$MOCK_PORT"); zr_pid=$(pid_on_port "$ZR_PORT")
  ll_pid=$(pid_on_port "$LITELLM_PORT"); bf_pid=$(pid_on_port "$BIFROST_PORT")
  key=$(cat "$KEYFILE")
  log "pids  mock=$mock_pid zerorouter=$zr_pid litellm=$ll_pid bifrost=$bf_pid"

  bench_target baseline noreset "$mock_pid" "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" -d "$CHAT_BODY"
  bench_target bifrost  noreset "$bf_pid"   "http://127.0.0.1:$BIFROST_PORT/v1/chat/completions" -d "$BF_BODY"
  bench_target litellm  noreset "$ll_pid"   "http://127.0.0.1:$LITELLM_PORT/v1/chat/completions" -H 'Authorization: Bearer dummy' -d "$CHAT_BODY"
  bench_target zerorouter-free reset "$zr_pid" "http://127.0.0.1:$ZR_PORT/v1/chat/completions" -H "Authorization: Bearer $key" -d "$ZR_FREE_BODY"
  bench_target zerorouter-metered reset "$zr_pid" "http://127.0.0.1:$ZR_PORT/v1/chat/completions" -H "Authorization: Bearer $key" -d "$ZR_METERED_BODY"

  reset_usage_state
  log "multi-user metered saturation ($MULTI_USERS users x $MULTI_CONNS_PER_USER conns, ${SAT_SECS}s)"
  run_multiuser zerorouter-metered-multiuser "$zr_pid" "$SAT_SECS" \
    "http://127.0.0.1:$ZR_PORT/v1/chat/completions" "$ZR_METERED_BODY" >>"$BENCH/results/results.jsonl"

  python3 -c "
import json
rows=[json.loads(l) for l in open('$BENCH/results/results.jsonl')]
json.dump(rows,open('$BENCH/results/results.json','w'),indent=2)
print('wrote results/results.json with',len(rows),'rows')
"
  cmd_report
}

cmd_report(){
  python3 "$BENCH/render_tables.py" "$BENCH/results/results.json"
}

# --- deliberate measurement of the month-to-date history cost ---------------
# Admission's cap check scans the user's whole month-to-date usage_events
# history per request (src/db.rs, "Access path" comment). That cost exists on
# BOTH lanes and grows with the user's accumulated rows. The main matrix
# resets usage state per cell so the cells stay comparable; this command
# measures the growth itself, on purpose: sequential single-connection
# latency on the free lane with an empty month vs. with 30k seeded rows.
HISTORY_ROWS=30000
HISTORY_REQS=500

history_case(){ # label
  local key; key=$(cat "$KEYFILE")
  oha -n "$HISTORY_REQS" -c 1 --no-tui --output-format json -m POST -T 'application/json' \
    -H "Authorization: Bearer $key" -d "$ZR_FREE_BODY" \
    "http://127.0.0.1:$ZR_PORT/v1/chat/completions" 2>/dev/null | \
  LABEL="$1" python3 -c "
import sys,json,os
d=json.load(sys.stdin); lp=d['latencyPercentiles']; s=d['summary']
print(json.dumps({'label':os.environ['LABEL'],'requests':int(round(s['requestsPerSec']*s['total'])),
  'success':s['successRate'],
  'p50_ms':round(lp['p50']*1000,3),'p95_ms':round(lp['p95']*1000,3),'p99_ms':round(lp['p99']*1000,3)}))"
}

cmd_history(){
  [ -s "$KEYFILE" ] || die "no bench key; run ./run.sh up first"
  local keyid
  keyid=$(psql_bench -c "SELECT k.id FROM api_keys k JOIN users u ON u.id=k.user_id WHERE u.email='bench@bench.local' LIMIT 1;")
  [ -n "$keyid" ] || die "bench user has no api key row"
  : > "$BENCH/results/history.jsonl"
  reset_usage_state
  log "history: $HISTORY_REQS sequential free-lane requests, empty month-to-date"
  history_case "0-rows" >> "$BENCH/results/history.jsonl"
  log "history: seeding $HISTORY_ROWS usage rows for the bench user (ts spread over the last ~8h, outside the velocity window)"
  psql_bench -c "
    INSERT INTO usage_events (request_id, api_key_id, tier, upstream_provider,
                              upstream_model, input_tokens, cached_input_tokens,
                              output_tokens, cost_usd, latency_ms, status, ts)
    SELECT gen_random_uuid(), '$keyid', 'metered-mock/mock-model', 'metered-mock',
           'mock-model', 12, 0, 9, 0.0000132, 5, 200,
           NOW() - ((i + 120) || ' seconds')::interval
    FROM generate_series(1, $HISTORY_ROWS) AS i;" >/dev/null || die "history seed failed"
  psql_bench -c "ANALYZE usage_events; CHECKPOINT;" >/dev/null
  log "history: $HISTORY_REQS sequential free-lane requests, $HISTORY_ROWS-row month-to-date"
  history_case "${HISTORY_ROWS}-rows" >> "$BENCH/results/history.jsonl"
  reset_usage_state
  cat "$BENCH/results/history.jsonl"
}

case "${1:-all}" in
  up) cmd_up ;;
  verify) cmd_verify ;;
  bench) cmd_bench ;;
  history) cmd_history ;;
  report) cmd_report ;;
  down) cmd_down ;;
  all) cmd_up; cmd_bench; cmd_history ;;
  *) die "usage: $0 {up|verify|bench|history|report|down|all}" ;;
esac

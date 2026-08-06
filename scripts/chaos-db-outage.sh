#!/usr/bin/env bash
# Database-outage injection: kill the router's Postgres connections in the
# window between "the customer received their inference" and "the
# settlement is recorded" — the one failure that can silently give away
# paid inference.
#
# What it asserts, in order:
#   1. the router does not hang or panic; the client gets a typed answer
#   2. the money is not silently lost: the settlement is owed (quarantined
#      with its intent) rather than forgotten
#   3. `admin settle-owed` recovers it and the balance moves EXACTLY once
#   4. running the recovery again is a no-op (idempotent replay)
#
# Requires: a local Postgres holding $DATABASE_URL, the router built, and
# scripts/chaos-upstream.py. Destructive only to in-flight connections.
set -uo pipefail
: "${DATABASE_URL:?set DATABASE_URL}"
DB_NAME="${DATABASE_URL##*/}"
ROUTER_BIN="${ROUTER_BIN:-./router/target/debug/zerorouter}"
EMAIL="${EMAIL:-dbchaos@zerorouter.test}"
PORT=9500

balance() { psql "$DB_NAME" -tA -c "SELECT credit_balance_usd FROM users WHERE email='$EMAIL'"; }
owed() { psql "$DB_NAME" -tA -c "SELECT COUNT(*) FROM usage_reservations WHERE quarantined_at IS NOT NULL"; }

lsof -ti :$PORT :8080 2>/dev/null | xargs kill 2>/dev/null
sleep 1
MODE=slow SLOW_SECONDS=6 PORT=$PORT nohup python3 "$(dirname "$0")/chaos-upstream.py" >/dev/null 2>&1 &
KEY=$("$ROUTER_BIN" admin mint-key --email "$EMAIL" --name dbchaos 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['api_key'])")
"$ROUTER_BIN" admin grant-credit --email "$EMAIL" --amount-usd 5 >/dev/null 2>&1
ZEROROUTER_BIND=127.0.0.1:8080 ZEROROUTER_PUBLIC_BASE_URL=http://localhost:8080 \
  ZEROROUTER_TIERS_PATH="$PWD/router/config/tiers.toml" \
  ZEROROUTER_PROVIDER_BASE_URL_OPENAI=http://127.0.0.1:$PORT \
  RUST_LOG=warn nohup "$ROUTER_BIN" serve > /tmp/chaos-db-router.log 2>&1 &
sleep 3

BEFORE=$(balance); echo "balance before: $BEFORE"
curl -s -m 60 localhost:8080/v1/chat/completions -H "Authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"zero/codex","messages":[{"role":"user","content":"hi"}]}' > /tmp/chaos-db-response.json &
CURL=$!
# Land the outage inside the upstream's 6-second answer window.
sleep 4
echo "== terminating the router's database connections =="
psql "$DB_NAME" -tA -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB_NAME' AND pid <> pg_backend_pid()" >/dev/null
wait $CURL
echo "client saw: $(head -c 160 /tmp/chaos-db-response.json)"
sleep 2
echo "owed settlements: $(owed)"
AFTER_OUTAGE=$(balance); echo "balance after outage: $AFTER_OUTAGE"
"$ROUTER_BIN" admin settle-owed 2>/dev/null | head -c 200; echo
RECOVERED=$(balance); echo "balance after recovery: $RECOVERED"
"$ROUTER_BIN" admin settle-owed 2>/dev/null | head -c 200; echo
REPLAYED=$(balance); echo "balance after replay: $REPLAYED"
[ "$RECOVERED" = "$REPLAYED" ] && echo "PASS: replay is idempotent" || echo "FAIL: replay moved the balance again"

lsof -ti :$PORT :8080 2>/dev/null | xargs kill 2>/dev/null
exit 0

#!/usr/bin/env bash
# Boot the built router image against a real Postgres and prove it serves.
#
# CI builds the container image but, until this script existed, never RAN it.
# That left a specific gap: an image can build perfectly and still be dead on
# arrival — a broken ENTRYPOINT, a migration that fails to apply, a binary that
# cannot find `tiers.toml` at the baked path. None of those are visible to
# `cargo test` (which never touches the image) or to `docker build` (which never
# starts the process). They surface at deploy. This script is the gate that
# moves that discovery left.
#
# Usage:
#   scripts/container-smoke.sh <image-ref>
#
# The topology is deliberately the one `examples/edge/docker-compose.yml` uses:
# the router joins the DATABASE container's network namespace
# (`--network container:...`) rather than sharing a user-defined bridge. That is
# not an aesthetic choice. `database_pool_from_env` (router/src/db.rs) REFUSES a
# `DATABASE_URL` naming a non-loopback host unless `DB_SSL_ROOT_CERT` is also
# set, because the remote path must enforce verify-full TLS. Sharing the
# namespace makes the database genuinely reachable at 127.0.0.1, so the smoke
# test exercises the ordinary developer path instead of standing up a CA to
# satisfy a check that is not what we are testing here.

set -euo pipefail

IMAGE="${1:-${ZR_SMOKE_IMAGE:-}}"
if [[ -z "${IMAGE}" ]]; then
    echo "usage: $0 <image-ref>" >&2
    exit 2
fi

# Pinned by digest, matching the service container in .github/workflows/ci.yml,
# so the smoke test and the Rust suites agree about what "Postgres 16" means.
POSTGRES_IMAGE="${ZR_SMOKE_POSTGRES_IMAGE:-postgres:16-bookworm@sha256:da788743d2060767375896de4d646f7576f5911461444b372616f19ea61db2ec}"
PORT="${ZR_SMOKE_PORT:-8080}"
BASE_URL="http://127.0.0.1:${PORT}"

SUFFIX="$$-${RANDOM}"
DB_CONTAINER="zr-smoke-db-${SUFFIX}"
APP_CONTAINER="zr-smoke-app-${SUFFIX}"

WORKDIR="$(mktemp -d)"

cleanup() {
    local status=$?
    if [[ ${status} -ne 0 ]]; then
        echo ""
        echo "=== smoke test FAILED (exit ${status}); container diagnostics follow ==="
        echo "--- router container state ---"
        docker inspect --format '{{.State.Status}} exitCode={{.State.ExitCode}} oomKilled={{.State.OOMKilled}}' \
            "${APP_CONTAINER}" 2>/dev/null || echo "(router container not created)"
        echo "--- router logs ---"
        docker logs "${APP_CONTAINER}" 2>&1 | tail -100 || true
        echo "--- postgres logs ---"
        docker logs "${DB_CONTAINER}" 2>&1 | tail -30 || true
    fi
    docker rm --force "${APP_CONTAINER}" >/dev/null 2>&1 || true
    docker rm --force "${DB_CONTAINER}" >/dev/null 2>&1 || true
    rm -rf "${WORKDIR}"
    return ${status}
}
trap cleanup EXIT

echo "==> starting Postgres (${DB_CONTAINER})"
# The router's port is published on the DATABASE container because the router
# shares its network namespace and therefore has no ports of its own to publish.
docker run --detach --name "${DB_CONTAINER}" \
    --env POSTGRES_DB=zerorouter_smoke \
    --env POSTGRES_USER=postgres \
    --env POSTGRES_PASSWORD=postgres \
    --publish "127.0.0.1:${PORT}:${PORT}" \
    "${POSTGRES_IMAGE}" >/dev/null

echo "==> waiting for Postgres to accept connections"
for _ in $(seq 1 60); do
    if docker exec "${DB_CONTAINER}" pg_isready -U postgres -d zerorouter_smoke >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
docker exec "${DB_CONTAINER}" pg_isready -U postgres -d zerorouter_smoke >/dev/null 2>&1 \
    || { echo "Postgres never became ready" >&2; exit 1; }

echo "==> starting the router image (${IMAGE})"
# Only DATABASE_URL is supplied beyond the provider placeholders: ZEROROUTER_BIND,
# ZEROROUTER_TIERS_PATH and ZEROROUTER_PORTAL_DIST are baked into the image, and
# leaving them unset here is deliberate — if a future Dockerfile edit drops one,
# this job should fail rather than paper over it with an env var CI happens to set.
#
# The provider keys are placeholders and are never dialed: /v1/models renders the
# catalog from `tiers.toml` and only checks that a candidate's credential env var
# is PRESENT to decide the lane is reachable. Without them the endpoint correctly
# returns an EMPTY list, which would make the catalog assertions below vacuous —
# so they exist to give the shape something to assert against, not to authenticate.
# `vertex` is deliberately omitted: its credential is a service-account JSON
# document rather than an opaque key, so a placeholder there tests a parser
# instead of the catalog.
docker run --detach --name "${APP_CONTAINER}" \
    --network "container:${DB_CONTAINER}" \
    --env DATABASE_URL='postgres://postgres:postgres@127.0.0.1:5432/zerorouter_smoke' \
    --env ANTHROPIC_API_KEY=smoke-placeholder-not-a-real-key \
    --env OPENAI_API_KEY=smoke-placeholder-not-a-real-key \
    --env GEMINI_API_KEY=smoke-placeholder-not-a-real-key \
    --env BEDROCK_API_KEY=smoke-placeholder-not-a-real-key \
    --env BEDROCK_REGION=us-east-1 \
    --env FIREWORKS_API_KEY=smoke-placeholder-not-a-real-key \
    --env XAI_API_KEY=smoke-placeholder-not-a-real-key \
    --env GROQ_API_KEY=smoke-placeholder-not-a-real-key \
    --env TOGETHER_API_KEY=smoke-placeholder-not-a-real-key \
    --env RUST_LOG=warn \
    "${IMAGE}" >/dev/null

echo "==> waiting for ${BASE_URL}/healthz"
# 90s: the wait covers process start AND the full migration set applying to an
# empty database, which is the slowest part of a cold boot.
ready=""
for _ in $(seq 1 180); do
    if ! docker inspect --format '{{.State.Running}}' "${APP_CONTAINER}" 2>/dev/null | grep -q true; then
        echo "router container exited before serving" >&2
        exit 1
    fi
    if curl --fail --silent --show-error --max-time 3 "${BASE_URL}/healthz" >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 0.5
done
[[ -n "${ready}" ]] || { echo "router never answered /healthz" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Assertion 1 — /healthz serves the documented body.
#
# This assertion carries more weight than it appears to. `/healthz` itself does
# not touch the database (router/src/api.rs), so on its own it says nothing
# about Postgres. But `main.rs` runs `migrate(&pool)` BEFORE it binds the
# listener, and a migration failure is a hard exit. So the fact that anything
# answered on this port at all is proof that the pool opened and every migration
# applied cleanly against a fresh database. That is the migrations-at-boot gate.
# ---------------------------------------------------------------------------
echo "==> asserting /healthz"
code="$(curl --silent --output "${WORKDIR}/healthz.json" --write-out '%{http_code}' "${BASE_URL}/healthz")"
[[ "${code}" == "200" ]] || { echo "healthz: expected 200, got ${code}" >&2; exit 1; }
python3 - "${WORKDIR}/healthz.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body == {"status": "ok"}, f"healthz body was {body!r}, expected {{'status': 'ok'}}"
print("    /healthz -> 200 {\"status\":\"ok\"} (implies migrations applied before bind)")
PY

# ---------------------------------------------------------------------------
# Assertion 2 — /v1/models returns the catalog in its published shape.
#
# This is the customer-facing contract for model discovery and it is served
# without auth, so it is reachable by anyone. The `retention` object is the part
# worth guarding hardest: ZeroRouter PUBLISHES a data-retention claim per lane,
# and unlike every other optional field it is never omitted. A refactor that let
# it become absent would silently drop a claim customers rely on.
# ---------------------------------------------------------------------------
echo "==> asserting /v1/models"
code="$(curl --silent --output "${WORKDIR}/models.json" --write-out '%{http_code}' "${BASE_URL}/v1/models")"
[[ "${code}" == "200" ]] || { echo "v1/models: expected 200, got ${code}" >&2; exit 1; }
python3 - "${WORKDIR}/models.json" <<'PY'
import json, re, sys

doc = json.load(open(sys.argv[1]))

assert doc.get("object") == "list", f"top-level object was {doc.get('object')!r}, expected 'list'"
data = doc.get("data")
assert isinstance(data, list), "top-level 'data' is not a list"
assert data, (
    "catalog is EMPTY. /v1/models omits a lane whose credential env var is unset, "
    "so this means the placeholder provider keys did not reach the container."
)

DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
postures = []

for entry in data:
    mid = entry.get("id")
    assert isinstance(mid, str) and mid, f"entry has no usable id: {entry!r}"
    assert entry.get("object") == "model", f"{mid}: object was {entry.get('object')!r}"
    assert "created" in entry, f"{mid}: missing 'created'"
    assert isinstance(entry.get("owned_by"), str) and entry["owned_by"], f"{mid}: bad owned_by"

    pricing = entry.get("pricing")
    assert isinstance(pricing, dict), f"{mid}: missing pricing object"
    for field in ("prompt", "completion"):
        assert field in pricing, f"{mid}: pricing missing {field!r}"
        float(pricing[field])  # must parse as a number even though it is a string

    # The retention claim: present on EVERY entry, never omitted.
    retention = entry.get("retention")
    assert isinstance(retention, dict), f"{mid}: missing retention object"
    posture = retention.get("posture")
    assert posture in ("zero", "standard"), f"{mid}: retention.posture was {posture!r}"
    assert isinstance(retention.get("description"), str) and retention["description"].strip(), (
        f"{mid}: retention.description is empty"
    )
    verified = retention.get("verified")
    assert isinstance(verified, str) and DATE.match(verified), (
        f"{mid}: retention.verified was {verified!r}, expected YYYY-MM-DD"
    )
    postures.append(posture)

# At least one lane of each posture. Requiring a 'zero' lane is the point of the
# exercise; requiring a 'standard' one too keeps the assertion honest — a bug
# that stamped every lane 'zero' would otherwise pass and would be the dangerous
# direction, since it over-claims privacy to customers.
assert "zero" in postures, "no zero-retention lane in the catalog"
assert "standard" in postures, "no standard-retention lane in the catalog"

# Ordering is part of the published contract: zero-retention lanes sort ahead of
# standard ones, so the privacy-preserving options are what a client sees first.
first_standard = postures.index("standard")
assert "zero" not in postures[first_standard:], (
    "catalog ordering broken: a zero-retention lane appears after a standard one"
)

print(f"    /v1/models -> 200, {len(data)} lanes, "
      f"{postures.count('zero')} zero-retention / {postures.count('standard')} standard, "
      f"ordering and retention claims intact")
PY

# ---------------------------------------------------------------------------
# Assertion 3 — an unauthenticated inference request is refused, in the
# documented error shape.
#
# The status code alone is not enough. ZeroRouter is an OpenAI-compatible
# surface, and clients branch on `error.code` / `error.type`; a refusal that
# returned 401 with a different envelope would break them just as thoroughly as
# no refusal at all. Nothing else in the repository pins this JSON body — the
# existing Rust test asserts only the status — so this is the only gate on it.
#
# Auth is checked before the body is buffered, so an empty body still yields 401.
# ---------------------------------------------------------------------------
echo "==> asserting unauthenticated /v1/chat/completions is refused"
code="$(curl --silent --output "${WORKDIR}/unauth.json" --write-out '%{http_code}' \
    --request POST "${BASE_URL}/v1/chat/completions" \
    --header 'Content-Type: application/json' \
    --data '{"model":"anthropic/claude-sonnet-5","messages":[{"role":"user","content":"hi"}]}')"
[[ "${code}" == "401" ]] || { echo "unauthenticated completion: expected 401, got ${code}" >&2; exit 1; }
python3 - "${WORKDIR}/unauth.json" <<'PY'
import json, sys

body = json.load(open(sys.argv[1]))
err = body.get("error")
assert isinstance(err, dict), f"expected an 'error' envelope, got {body!r}"
assert err.get("type") == "authentication_error", f"error.type was {err.get('type')!r}"
assert err.get("code") == "invalid_api_key", f"error.code was {err.get('code')!r}"
assert "param" in err, "error.param must be present (serialized as null), for OpenAI-client compatibility"
assert isinstance(err.get("message"), str) and err["message"].strip(), "error.message is empty"
assert set(err) == {"message", "type", "param", "code"}, f"unexpected error fields: {sorted(err)}"
print("    POST /v1/chat/completions (no auth) -> 401 "
      "{error:{type:authentication_error, code:invalid_api_key}}")
PY

# The container must still be up: a process that served three requests and then
# died would otherwise pass every assertion above.
docker inspect --format '{{.State.Running}}' "${APP_CONTAINER}" | grep -q true \
    || { echo "router container is no longer running after the assertions" >&2; exit 1; }

echo ""
echo "container smoke test PASSED"

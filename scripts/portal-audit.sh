#!/usr/bin/env bash
# Fail the build on a high or critical advisory in the portal's dependency tree.
#
# Run from anywhere:
#
#     scripts/portal-audit.sh
#
# `pnpm audit --audit-level high` would almost do this, but its exit code is
# all-or-nothing: there is no way to say "this specific advisory is understood
# and accepted" without also lowering the bar for everything else. So the gate
# is expressed here instead, where each accepted advisory carries the reasoning
# that makes it acceptable — the same discipline `router/deny.toml` applies to
# RUSTSEC ids.
#
# Moderate and low findings are REPORTED but do not fail. That is a deliberate
# line: this portal is a static SPA built by Vite and served as files from the
# container, and most moderate findings in this tree are against the local dev
# server, which no deployment runs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORTAL_DIR="${SCRIPT_DIR}/../portal"
cd "${PORTAL_DIR}"

# ---------------------------------------------------------------------------
# Accepted high/critical advisories.
#
# An entry here needs a reason that answers "why can this not be fixed, and why
# is the exposure acceptable?" — not merely "this is noisy". Anything fixable by
# a version bump inside the declared semver range MUST be bumped instead.
# ---------------------------------------------------------------------------
read -r -d '' ACCEPTED_JSON <<'JSON' || true
{
  "GHSA-fx2h-pf6j-xcff": "vite <=6.4.2 — `server.fs.deny` bypass on Windows alternate paths. NOT FIXABLE IN RANGE: the fix ships in vite 6.4.3 and there is no patched release on the 5.x line the portal declares (^5.4.20), so clearing it requires a MAJOR vite upgrade — a change to the portal's build toolchain that belongs in its own reviewed pull request, not in a CI change. Exposure: the flaw is in vite's DEV SERVER (`pnpm dev`), on Windows. Production never runs it — `pnpm build` emits static files that the Rust binary serves from /srv/portal (see Dockerfile), and vite is absent from the runtime image entirely. CI runs Linux. TODO: revisit as part of a deliberate vite 5 -> 6 upgrade."
}
JSON

echo "==> pnpm audit (portal)"
# `pnpm audit` exits non-zero whenever it finds anything, so its status is not
# usable as the gate; the JSON body is what matters.
pnpm audit --json > /tmp/portal-audit.json 2>/dev/null || true

ACCEPTED_JSON="${ACCEPTED_JSON}" python3 - <<'PY'
import json, os, sys

accepted = json.loads(os.environ["ACCEPTED_JSON"])

with open("/tmp/portal-audit.json") as fh:
    report = json.load(fh)

advisories = report.get("advisories", {})
blocking, accepted_hits, informational = [], [], []

for entry in advisories.values():
    severity = entry.get("severity", "unknown")
    ghsa = entry.get("github_advisory_id") or entry.get("url", "")
    module = entry.get("module_name", "?")
    patched = entry.get("patched_versions", "?")
    row = (severity, module, ghsa, patched)
    if severity in ("high", "critical"):
        (accepted_hits if ghsa in accepted else blocking).append(row)
    else:
        informational.append(row)

def show(title, rows):
    if not rows:
        return
    print(f"\n{title}")
    for severity, module, ghsa, patched in sorted(rows):
        print(f"  {severity:8s} {module:20s} {ghsa:24s} patched: {patched}")

show("informational (moderate/low — reported, not blocking):", informational)

if accepted_hits:
    print("\naccepted high/critical advisories:")
    for severity, module, ghsa, patched in sorted(accepted_hits):
        print(f"  {severity:8s} {module:20s} {ghsa}")
        print(f"    reason: {accepted[ghsa]}")

show("BLOCKING high/critical advisories:", blocking)

# An accepted entry that no longer matches anything is stale and must be
# removed, for the same reason `unused-allowed-license = "deny"` exists in
# deny.toml: a suppression list nobody prunes stops describing reality.
#
# Both conditions are collected before exiting, and the real vulnerability is
# reported FIRST. An earlier draft returned on the stale entry immediately,
# which meant that renaming an accepted id — the exact thing someone does while
# editing this list — hid the live advisory behind a bookkeeping complaint.
seen = {row[2] for row in accepted_hits}
stale_entries = sorted(set(accepted) - seen)
for stale in stale_entries:
    print(f"\nERROR: {stale} is listed as accepted but was not reported.")
    print("       Either the advisory is resolved (delete the entry) or the id is wrong.")

if blocking:
    print(f"\nportal audit FAILED: {len(blocking)} unaccepted high/critical advisory(ies).")
    print("Fix by upgrading, or add an entry to ACCEPTED_JSON with a reason that")
    print("explains why it cannot be fixed and why the exposure is acceptable.")

if blocking or stale_entries:
    sys.exit(1)

print(f"\nportal audit PASSED "
      f"({len(accepted_hits)} accepted high/critical, {len(informational)} informational).")
PY

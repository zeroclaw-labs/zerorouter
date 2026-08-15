# AGENTS.md — working in this repository

ZeroRouter is an LLM API gateway that resells inference. It holds prepaid
balances, takes Stripe payments, and meters and bills every request. **Code here
moves real money.** That single fact drives most of the conventions below: a bug
in this repo is not a wrong pixel, it is a customer charged for something they
did not receive, or inference served for free.

This file is read by any agent working in this repo — Claude Code, Codex, or
anything else that honors `AGENTS.md`.

## Gates

Everything below must pass before work is considered done. Run from `router/`:

```bash
export DATABASE_URL=postgres://zr@127.0.0.1:55432/zerorouter_test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build            # production profile; the testing feature must stay off
```

CI runs the same set with `--locked`. `cargo build` is not redundant: it proves
the test-only substitution surface is absent from a production binary.

**A database is required.** The DB-gated integration tests skip silently without
`DATABASE_URL`, so a green run that never touched Postgres proves very little.
Confirm the DB-backed suites actually reported non-zero counts. Any local
Postgres 16 works; create a database and point `DATABASE_URL` at it. When
several agents work concurrently, give each its own database — the tests are not
isolated from each other.

## Testing the request path

`router/src/testing.rs`, behind the `testing` feature, provides a scriptable
fake upstream and the constructors that place one behind a candidate or a
router. This is how the dispatch path is driven end to end over HTTP against a
real database without touching a provider.

The feature is deliberately **never enabled by `cargo build`**: a binary that
moves money must not carry a way to substitute the upstream. If you add to that
surface, keep it behind the feature and verify it is absent from a production
build.

`router/tests/request_path.rs` holds characterization tests — they pin current
behavior on purpose, including behavior that looks wrong, so a refactor has a
tripwire. If one fails, do not adjust it to match your change until you are
certain the change is intended. Where such a test asserts a known defect it says
so inline.

## Migrations

`router/migrations/` is the source of truth for the current head — list the
directory rather than trusting a number written down anywhere, and expect
gaps in the numbering (skipped numbers are burned, not missing). Migrations
are registered by hand in `router/src/db.rs`; a file alone does nothing.

**Never edit a migration that has been applied anywhere**, including its
comments. sqlx checksums the file text, so amending it breaks every database
that already ran it. To correct something an old migration said, state the
correction in the header of a new one.

`usage_events` and `request_attempts` are append-only, enforced by triggers.
New columns are nullable and written at INSERT time; there is no backfill.

## Invariants that must not regress

Settlement correctness is load-bearing and well tested. Do not restructure it
casually:

- Admission and settlement serialize on the same per-user advisory lock.
- Exactly-once comes from `DELETE FROM usage_reservations ... RETURNING`; the
  debit runs in that same transaction and only when the DELETE returned a row.
- The debit is clamped to the reservation, and the balance has a non-negative
  CHECK.
- Settlement is durable and replayable: the intent is written to the reservation
  row before the settle transaction, and a retry after a committed settle is
  resolved to success by the UNIQUE `usage_events.request_id`.

Billing policy is **metered actuals only**. If the upstream did not report
usage, the request is not billed. Do not reintroduce an estimate — the available
figures are a byte-length prompt bound and a `len()/4` output heuristic, so an
estimate can err in either direction, and a conservative guess is still a guess.

Each attempt's cost is counted in exactly one place: the served attempt via
`usage_events.cost_basis_usd`, every other attempt via
`attempts_cost_basis_usd`. Unknown cost is NULL and sets
`attempts_cost_basis_complete = FALSE`; it is never zero.

**If a change would alter what a customer is charged, stop and say so** rather
than proceeding. That is a decision for the repo owner, not a side effect.

## The formerly pinned upstream

The router used to pin `zeroclaw-api` and `zeroclaw-providers` to a git rev;
that pin has been cut and the router owns its wire. What survives from the
pinned era is `router/src/retry.rs`: copies of failure-classification helpers
that were private to `zeroclaw_providers::reliable`, with a disposition table
recording which copies are verbatim and which have deliberately diverged.
Treat that table as load-bearing — it is what makes a divergence a decision
instead of drift — and **if a ZeroClaw dependency is ever re-introduced,
reconcile the table first**: a silently re-imported helper can undo a fix.

## Tier catalog

`router/config/tiers.toml` is re-parsed per request. Validation distinguishes
two classes of fault:

- **Structural** (malformed ids, duplicate candidates, unbillable rates) refuses
  the whole file — it cannot be trusted at all.
- **Economic** (a candidate priced above its tier's sell rate) withholds just
  that tier; the rest keep serving.

The catalog is currently pins only — each tier is one candidate sold at cost
(the header of `tiers.toml` records why, and what the reserved `zero/*`
namespace is for) — so basis == sell is the intended shape everywhere and only
strictly-greater is a violation.

## Commits

Write the commit message as prose explaining what was wrong and why the change
is right — the repo's history is the design record. **Never add a
`Co-Authored-By` trailer or any other AI attribution line.**

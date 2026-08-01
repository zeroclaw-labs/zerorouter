# ZeroRouter Estimate-and-Select Engine

ZeroRouter becomes a per-request estimate-and-select engine: one policy knob
(`priority: cost | balanced | success`), two objective functions (minimize
expected $ subject to P(success) ≥ floor; maximize P(success) subject to
$ ≤ budget), one internal escalation loop (try cheap → validate → escalate →
return only the result that worked), and one bill line. The customer's own
declared validator defines success, which is what makes outcome-gated pricing
auditable. Phase discipline throughout: bill metered actuals and *show*
estimates as guidance first; quotes and outcome fees arrive only after
per-segment calibration gates pass — with one deliberate exception the thesis
itself names: the **flat orchestration fee ships in the same stage as the
escalation loop**, because an escalation loop with no fee is a pure COGS
faucet (Risks §4).

Status: design, pre-implementation; revised 2026-07-23 after adversarial
review (positions taken are recorded in Risks so settled questions do not
reopen). Companion migration:
`router/migrations/0004_estimate_and_select_substrate.sql` (requires a
`Migration::new(4, ...)` entry in the `include_str!` vec at `db.rs:103-131`;
migrations auto-run at startup of both `serve` and `admin`, `main.rs:66`,
`admin.rs:72-73`). Sequencing follows the integration map
(`zeroclaw-integration-map.md` §5); every attachment point below was verified
against the working tree on 2026-07-23.

## Motivation & thesis

Three facts anchor the design:

1. **The transparent pass-through lane is margin-dead** (0% token markup is
   table stakes; pricing brief, Mode 1). Durable revenue lives in routing
   *intelligence*: a savings-share on measured savings for cost optimizers,
   and validator-gated outcome fees for success optimizers — the two lanes the
   brief identifies as lightly occupied (Mode 2) and genuinely unoccupied
   (Mode 3).
2. **The substrate is thin but the seams are exact.** Candidate, latency, and
   tokens are persisted; COGS, attempts, finish_reason, and request shape are
   not (map §3). The reservation ceiling, the advisory-locked settle, and the
   debit clamp (`db.rs:387`) already provide the hard money guardrails on the
   *customer's* side of the ledger — this design extends them, never replaces
   them, and adds the guardrail they never provided: an explicit ceiling on
   **ZeroRouter's own** walk COGS, which the reservation does not and cannot
   cap.
3. **The graveyard is real.** Flat pricing dies on the usage tail (Cursor,
   Copilot); outcome pricing dies on definition disputes (Decagon/Ada). So:
   never a fixed quote before the estimator is calibrated, never a fee on an
   outcome the customer didn't define themselves, never eat an unbounded tail
   — and never run the escalation loop at GA with zero fee attached.

## Customer API

### The priority knob

**One typed, namespaced request field.** `ChatCompletionRequest`
(`openai.rs:13-27`) gains a single optional `zerorouter` object, consumed by
serde before the `#[serde(flatten)] extra` map (`openai.rs:25-26`):

```jsonc
"zerorouter": {
  "priority": "cost" | "balanced" | "success",   // the knob
  "validator": { ... },                          // success definition, inline (v1) or "slug@2" (registered, later)
  "budget_usd": "0.05"                           // success mode: hard sell-price ceiling, cap-and-block (below)
}
```

The object deserializes with `deny_unknown_fields` — ZeroRouter's own
namespace is strictly validated (a typo like `"priorty"` is a loud 400)
without ever touching the OpenAI-compat unknown-field rejection
(`contains_unsupported_extensions`, `openai.rs:216-234`, enforced at
`api.rs:176-178`). Compatibility is provable, not asserted: a client sending
`zerorouter` (or any new top-level key) today already lands in `extra` and is
400-rejected (`unsupported_request_fields`; test `openai.rs:701-710`), so the
typed field cannot silently change the meaning of any request that works
today. Value checks (`budget_usd > 0`, `validator`/`budget_usd` only with a
priority that uses them) join `validate()` (`openai.rs:98-169`) and surface as
the existing 400 path (`api.rs:172`).

**`budget_usd` is cap-and-block, never cap-and-absorb.** A rung is
admissible only if its reservation-bound sell cost —
`usage_cost(rung sell rates, reservation_usage)` — fits inside
`min(budget_usd, reserved sell cost)`; inadmissible rungs are never
dispatched (recorded `policy_skipped`). Because the served attempt's metered
sell is bounded by its reservation-bound sell, admissibility implies the bill
lands within budget with **no** absorb path: ZR never eats a
budget-vs-actuals gap, so a tiny budget paired with a huge `max_tokens`
buys nothing. (OpenRouter's `max_price` blocks rather than absorbs for the
same reason — pricing brief, Mode 3.) A budget below even the cheapest rung's
bound is a 400 (`budget_too_low`); a budget above the reservation clamps to
the reservation (the customer cannot buy past what admission verified).

**Model-suffix carrier** — the universal ZeroClaw-safe path, carrying
*priority only*: `zero/balanced:cost`. Parsed in `chat_completions`
immediately before `catalog.resolve(&request.model)` (`api.rs:182-184`) with
a **resolve-first algorithm**: try `catalog.resolve` on the untouched model
string; only if that fails, and the substring after the last `:` is exactly
one of the three priority tokens, strip it, record the priority, and resolve
the remainder; anything else falls through to `ModelNotFound`. Today no tier
or candidate id contains a colon (verified against `config/tiers.toml`), but
that is a data-file convention, not an invariant — Bedrock ARN-style ids
(`arn:aws:bedrock:...`) would break a naive last-colon split. So the
convention is also **enforced at catalog load**: `validate_tier_catalog`
(`config.rs:159-200`) gains a rule rejecting any tier or candidate id whose
final `:`-delimited segment collides with a priority keyword. Resolve-first
means even a hypothetical colliding id keeps resolving; the validation rule
means the collision can never be introduced silently. This carrier survives
every ZC path and family, unlike `provider_extra`, which the no-tools
streaming path drops and the `openai` family lacks (map §3).
`usage_events.tier` records the *stripped* model name; priority gets its own
column.

**Per-key default.** New nullable `api_keys.default_priority` (migration
0004). It must be known before candidate ordering at `api.rs:186` — which
runs *before* the admission SELECT (`db.rs:163-172`) — so it rides
`AuthenticatedKey` (`auth.rs:15-19`): extend the authenticate SELECT
(`auth.rs:67-77`) to also fetch `default_priority`. The 30s auth cache TTL
(`auth.rs:12`) gives the same staleness contract as key disablement. Set at
mint through all three surfaces: portal `CreateKeyRequest`
(`portal.rs:201-206` — plain `Deserialize`, no `deny_unknown_fields`, so the
optional field is wire-backward-compatible) via the existing COALESCE update
block (`portal.rs:306-321`); admin `MintKeyArgs` (`admin.rs:29-39`,
`mint_key` `admin.rs:82-170`); the device-claim mint (`device.rs:377-386`)
leaves it NULL. Post-mint mutation is the portal's first PATCH endpoint —
`PATCH /api/keys/{id}` beside the existing mint/list/disable routes
(`portal.rs:45-52`; `disable_key` at `:350-369` is today's only mutation) —
accepting `default_priority` alone until a second field earns its place.

**Precedence and conflicts.** `zerorouter.priority` > model suffix > key
default > `balanced`. Typed field and suffix both present with *different*
values → 400 (`priority_conflict`): a conflict is a client bug; be loud.
`balanced` with no validator is bit-for-bit today's behavior — tier-table
order, byte-bound reservation, unchanged responses. Backward compatibility is
the identity function, and it is testable byte-for-byte.

> **Shipped in Stage 3a** (`priority.rs`, `openai.rs`, `api.rs`, `auth.rs`,
> `config.rs`, `portal.rs`, `admin.rs`). Points where this section was terse
> are now decided, recorded so they are re-decided rather than re-derived:
>
> - **The typed object carries `priority` alone in 3a.** `validator` and
>   `budget_usd` are not fields yet; `deny_unknown_fields` makes them — and
>   any typo — a loud 400 until the stage that defines them, rather than
>   silently accepted no-ops. An empty `"zerorouter": {}` is legal,
>   forward-compatible, and does not engage the knob.
> - **The stripped name is the name.** After suffix stripping, every surface
>   reads the resolved model: `usage_events.tier` (as specified),
>   the response `model` field, and stream-chunk `model` — the suffix is a
>   carrier, not part of the model's identity.
> - **A suffix on an unresolvable base falls through untouched** to the same
>   404/withheld answer the base alone would get; an empty base (`":cost"`)
>   is not a carrier.
> - **The resolved priority is written on every settled row** at every
>   terminal of both walks; NULL is reserved for rows that predate the knob —
>   including a settlement intent persisted before the deploy and replayed
>   after it (`TelemetryPayload.priority` is `#[serde(default)]`), which
>   settles NULL rather than guessing `balanced`.
> - **The per-key default rides `AuthenticatedKey`** out of the authenticate
>   SELECT under its existing 30-second cache — a changed default has
>   exactly the staleness contract of a disablement, as designed. The
>   device-claim mint leaves it NULL.
> - **`PATCH /api/keys/{id}` is presence-semantics**: the body field is a
>   double `Option` — absent leaves the key unchanged (`{}` is a no-op
>   answering the current summary), explicit `null` clears to balanced, a
>   keyword sets. Strict namespace like the request object; other tenants'
>   keys 404; disabled keys stay patchable (the flag governs dispatch, not
>   ownership).

### Validators

**Zero-config default: the implicit shape check.** Every request — with or
without a declared validator — is labeled at settle: completion present,
output non-empty, every tool-call `arguments` parses as JSON, not truncated
(via the synthesized `finish_reason`, `openai.rs:492-505`, until the upstream
stop-reason plumb lands — map §5.8). The shape check gates escalation **only
where the response has not already been streamed live to the client**:
non-streaming requests and buffered walks. On a live stream (`cost` or
`balanced` streaming with no declared validator) it is a *label only* — you
cannot unsend streamed tokens, so live cost-mode streaming keeps today's flow
and TTFB, exactly like balanced (this resolves an ambiguity flagged in
review: a stream is buffered only when a validator is declared or priority is
`success`; the shape check alone never buffers a stream). In `balanced` it is
a label everywhere (the do-no-harm anchor). The label
(`usage_events.shape_ok`) trains the success estimator on 100% of traffic
from day one, before any customer declares anything.

**Declared validators, v1: inline, deterministic, in-process.** Two kinds,
both sub-millisecond, zero marginal COGS, no customer code ever executes:

```jsonc
"validator": {
  "kind": "json_schema",
  "schema": { ... },                    // draft 2020-12 via the `jsonschema` crate
  "applies_to": "content"               // or "tool_arguments"
}
"validator": {
  "kind": "assert",                     // AND-composed closed set
  "assertions": [ {"type": "contains", "value": "..."},
                  {"type": "regex", "value": "..."} ]
  // types: contains | not_contains | regex | min_length | max_length
  //        | json_parses | tool_call_named   (regex crate: no backtracking blowups)
}
```

Whether inline or (later) registered, the router persists
`validator_kind` + `validator_spec_sha256` (full sha256 of the canonical
spec JSON — full-width, not a prefix, so the audit anchor cannot be
collision-farmed) on the settled row: **the customer's own frozen predicate
defines success, so a validator-gated outcome has nothing to dispute.**

**Registered validators (later stage).** Data-plane registration with the
same Bearer `zcr_` key the customer calls with — `POST /v1/validators`,
`GET /v1/validators`, disable-only lifecycle mirroring the keys pattern —
producing immutable `slug@version` rows whose content hash is stored. A
request references `"validator": "invoice-extract@2"`; a billed success
references an immutable id. Editing a validator creates a new version, so
"quietly weaken the validator after negotiating the fee" is structurally
impossible.

**Deliberately excluded from v1: judge/rubric validators.** A judge is
another LLM call — COGS, latency, nondeterminism inside a billing gate, and a
prompt-injection surface into the fee trigger. It returns later as a
registered-only kind with the judge model pinned in the immutable spec, and
no fee rides on judge outcomes until its dispute machinery is understood.

### Response metadata

**Non-streaming:** `ChatCompletionResponse` (`openai.rs:381-389`) gains an
optional `zerorouter` object (`skip_serializing_if` none — legacy responses
stay byte-stable; response strictness is not part of ZR's contract, only
request strictness is):

```jsonc
"zerorouter": {
  "priority": "success",
  "attempts": [ {"candidate": "deepinfra/deepseek-ai/DeepSeek-V4-Pro", "outcome": "validation_failed", "latency_ms": 812},
                {"candidate": "fireworks/accounts/fireworks/models/deepseek-v4-pro", "outcome": "ok", "latency_ms": 1490} ],
  "validated": true,                     // null when no validator was declared
  "estimate": { "output_tokens_p50": 210, "output_tokens_p90": 640,
                "basis": "cold" },       // 'cold' | 'learned' | (later) 'quote' — guidance, never a quote
  "limited": "escalation_budget",        // present only when policy degraded the walk (per-user budget, Engine §)
  "savings": { "reference_model": "anthropic/claude-sonnet-5",
               "reference_cost_usd": "0.0412", "billed_cost_usd": "0.0104",
               "saved_usd": "0.0308",
               "basis": "measured" }     // 'measured' | 'rate_only' — see Billing §3; only when the key declares a reference
}
```

Headers: existing `x-request-id` / `x-zerorouter-provider` /
`x-zerorouter-model` (`api.rs:383-393`) keep naming the served candidate;
add `x-zerorouter-attempts` (count).

**Streaming:** SSE headers are sent when the response starts — before the
walk resolves (`streaming_response` returns at `api.rs:397-437` with only
`x-request-id`, `:435`) — so streaming metadata is in-band only: the same
`zerorouter` object rides the final usage chunk (`stream_usage_json`,
`openai.rs:563-574`) when the client opts in via
`stream_options.include_usage`. Clients that already parse usage chunks
tolerate an extra top-level field; clients without `include_usage` read
attempts and savings from the portal usage endpoint. Honest, and a strict
improvement over invisible.

> **Shipped in Stage 3a**, with the visibility rule this section implied
> made explicit: the block is attached **only when the request engaged the
> knob** through some carrier (typed object, suffix, or key default). A
> request that never mentioned the knob keeps a byte-identical legacy body —
> the testable backward-compatibility anchor — while its resolved `balanced`
> is still persisted. The 3a block is `priority` + `attempts` + `validated`
> (null exactly as defined: no validator was declared); `estimate`,
> `limited`, and `savings` are **absent until their stages ship them**, so
> each field's appearance is itself the capability signal. The attempts
> array is built from the same walk ledger the settle transaction drains,
> immediately before it drains, so the customer-visible story and the
> `request_attempts` rows cannot diverge; skips are included. The
> `x-zerorouter-attempts` header is additive and rides every served buffered
> response, engaged or not; both stream serve paths (live and synthetic)
> attach the block to the usage chunk. The portal-usage-endpoint read path
> for header-less streaming clients is **not** in 3a — it ships with a later
> portal pass.

> **Stage 3b added the `estimate` member** to the engaged block, with three
> decisions recorded. The display gate is **n ≥ 50 alone**: the p99/p50 ≤ 8
> tail gate guards reservation *money* (Stage 4) and deliberately does not
> hide guidance — a heavy-tailed segment shows its measured percentiles.
> The **cold shape echoes the request's own `max_tokens`** on both
> percentiles: an unmeasured segment's only honest bound, and legibly so.
> And the response basis is the **estimate's** provenance, not the
> reservation's: `usage_events.estimator_basis` keeps stamping `cold` until
> Stage 4 sizes reservations from these cells — a `learned` estimate shown
> beside a cold-sized reservation is exactly the
> visibility-before-financial-exposure split the rollout row names.


### Error semantics

**Validation exhaustion is a result, not an error.** When the success-mode
walk runs out of candidates, attempts, or budget without a validator pass, ZR
returns **200 with the best attempt**, `"validated": false`, and bills
actuals for the served attempt — failing the whole request because *our*
escalation ran out would make ZR strictly worse than a direct call, and no
per-success fee can ever attach to an unvalidated result. A customer opt-in
(`on_validation_failure: "error"` → 422) is deliberately deferred.

New 400 codes (added to `ApiError`, `error.rs:8-26`, at the stages that ship
them): `priority_conflict` (typed field vs suffix disagreement),
`invalid_validator` (unknown kind, malformed spec, unknown/disabled
registered reference, other user's validator), `budget_too_low` (budget below
the cheapest admissible rung's reservation-bound sell cost — see the
cap-and-block rule above).

## Engine architecture

### Task signature (the segmentation key)

`task_signature` = first 16 hex chars of
`sha256(user_id ∥ sorted tool names ∥ message-count bucket {1, 2-4, 5-16, 17+}
∥ log2 prompt-bytes bucket ∥ stream flag ∥ requested-max_tokens bucket)`,
computed at ingress beside `reservation_usage` (`openai.rs:237-269`), which
already walks exactly these fields. Keying **per user — not per API key** is
the anti-gaming segmentation: estimates are learned from *your* traffic, so
reshaping requests only poisons your own band, and — the review's correct
attack on the earlier per-key design — **minting a fresh key resets
nothing**, because keys are cheap (`disable_key` just flips a flag,
`portal.rs:350-369`, and the 20-key cap counts only non-disabled keys,
`portal.rs:283`, so disable-and-remint is unlimited). Every estimator cell,
clamp-loss aggregate, abuse monitor, and escalation budget in this design is
keyed on `user_id` for the same reason. Coarse buckets mean padding a prompt
to jump buckets raises the attacker's own input bill. Raw features
(`prompt_bytes`, `message_count`, `tool_count`, `requested_max_tokens`,
`stream`) are persisted alongside the hash so signatures can be re-bucketed —
or re-keyed to a pooled/hierarchical scheme — retroactively without a second
data system. Prompt *content* never feeds the estimator (retention contract
intact).

Key-churn is additionally throttled at the source: key **creation** (portal
`create_key` and the device-claim mint; the admin CLI is operator-only and
exempt) gains a per-user trailing-24h creation-count limit that counts
disabled keys too — closing the disable-and-remint loophole the active-key
cap (`portal.rs:33`) never covered. Ships with Stage 5a, where it starts
mattering.

### Cost estimator

**Input side stays countable pre-flight**: the byte bound
(`openai.rs:237-269`) is kept as v1's input reservation. A learned
bytes-per-token ratio per candidate (from `input_tokens / prompt_bytes` on
settled rows) is a later refinement, clamped above by the byte bound.

**Output side is a SQL aggregate — no ML, ever, in this design:**

```sql
SELECT percentile_cont(ARRAY[0.5, 0.9, 0.99]) WITHIN GROUP (ORDER BY output_tokens), COUNT(*)
FROM usage_events
WHERE task_signature = $1 AND candidate_id = $2 AND status = 200
  AND ts >= NOW() - INTERVAL '14 days';
```

served by `usage_events_signature_candidate_idx` (partial, migration 0004).
Selection uses per-`(signature, candidate)` cells; reservation sizing uses
the candidate-agnostic per-signature distribution (the reservation is
computed before a candidate is chosen, `api.rs:188-191`).

**Never on the request path.** An `EstimatorState` on `RouterServices`
(`api.rs:56-61`) joins the `KeyAuthenticator`'s auth cache
(`auth.rs:33-43`, 30s TTL) as the struct's second piece of cross-request
in-process state — and like the auth cache it is a cache, not a source of
truth. It holds
`RwLock<HashMap<(TaskSignature, CandidateId), CachedEstimate>>`. A request
that misses (or finds a stale >5 min entry) uses the **cold** estimate and
enqueues the cell; a background refresher on the existing `TaskTracker`
(`api.rs:63-76`) batches the percentile queries every 60s. Restart = cold
until warmed; cold is exactly today's behavior, so the failure mode of the
whole estimator is the status quo.

**Use — reservation sizing only, never billing.** At the admission seam
(`api.rs:188-191`), a request is *eligible* for learned sizing only when all
of the following hold; otherwise it reserves exactly today's byte bound
(`max_tokens` defaulted to `BASELINE_MAX_TOKENS`, `api.rs:185`):

- the signature cell is warm: n ≥ 50 settled 200-rows in the 14-day window;
- the signature's output distribution is not heavy-tailed: p99/p50 ≤ 8
  (judgment-set; a fat right tail is exactly where a percentile reservation
  under-covers, so such segments never leave `cold`);
- the request is not escalation-capable: priority is not `success` and no
  validator is declared. Escalating requests are the subset selected for
  hard, long-output work — the tail-correlated cohort where a whole-segment
  p99 under-reserves most — so in Stage 4 they keep the byte bound outright.

Eligible requests reserve
`output = min(requested_max_tokens, max(p99 × 1.25, 0.25 × requested_max_tokens))`.
The **floor at 25% of requested max_tokens** is the structural answer to the
dilution attack the review demonstrated (199 "reply hi" requests + 1
full-max generation inside one segment): it caps the per-row clamp loss at
`0.75 × requested_max × sell output rate` no matter how poisoned the
percentile is, and it makes dilution self-defeating — filler requests
carrying a huge `requested_max_tokens` (they must, to stay in the target
bucket) still reserve ≥ 25% of it, and reserved tokens project against the
velocity cap and credit gate at admission (`db.rs:216-273`), so the attacker
burns their own TPM and balance headroom to keep the segment diluted.

The payoff is verified mechanics: a tighter bound directly buys admissible
TPM and balance headroom (`db.rs:216-245`, `:247-273`). The billing fallback
that settles `reservation_usage` on missing provider usage
(`api.rs:339-352`, `:849`, `:994-1007`) then bills the *smaller* estimated
bound — erring in the customer's favor — and the debit clamp (`db.rs:387`)
guarantees under-reservation is **ZeroRouter's tail, not the customer's**:
`clamp_loss = max(0, cost_usd − reserved_cost_usd)` is a per-row dollar
metric (both terms now on the settled row — migration 0004 snapshots
`reserved_cost_usd`) that doubles as the calibration objective and the
primary gaming alarm.

**Auto-revert triggers on dollars, not rates.** The review is right that a
clamp-hit *rate* is dilutable to arbitrarily low values while per-row losses
stay large — 1-in-200 is exactly 0.5%. The automatic revert (evaluated by
the background refresher, no request-path work) fires per segment on any of:
trailing-7d clamp-loss **dollars** > $10; any **single row** clamp_loss >
$1; clamp-hit rate > 0.5% (kept as a secondary, distribution-shift signal).
Reverted segments go `cold` for ≥ 7 days. A per-**user** aggregate (trailing
30d clamp-loss > $50 across all the user's segments) reverts every segment
of that user — segments are user-scoped, so this cannot be escaped by
re-slicing traffic. All thresholds are judgment-set numbers to re-fit
against Stage-4 shadow telemetry before reservations actually shrink; the
mechanism (dollar-denominated, per-row, per-segment, per-user) is the
commitment.

**Why not simply bill actuals above the reservation instead?** Rejected, and
recorded here so it stays rejected: the clamp at `db.rs:387` is the
enforcement point of the non-negative-balance invariant (`0003:6-8`) —
admission verified, under the per-user advisory lock, that
`balance − other active reservations ≥ this reservation` (`db.rs:247-273`);
debiting *more* than reserved could overdraw a balance that concurrent
requests have legitimately reserved against, turning a pricing bug into a
broken ledger. The customer-side clamp stays; ZR's exposure is bounded by
the floor and alarmed in dollars instead.

**Maturity ladder, per segment:** `cold` (byte bound — the permanent floor)
→ `learned` (estimator-sized reservations + shown estimates) → `quote`
(banded quotes; far-future, gated). `estimator_basis` is stamped on every
row, so an audit can prove no quote-grade estimate ever shipped from an
ungated segment.

> **Shipped in Stage 3b** (`estimator.rs`, `db.rs`, `api.rs`) — the read
> path only, reservations untouched. Decisions recorded:
>
> - **The cell key carries the signature scheme** (migration 0007): scheme-1
>   and scheme-2 rows are different segments, and the scan filters
>   `task_signature_scheme` so pre-0007 NULL-scheme rows are invisible to a
>   current-scheme cell. Only `status = 200` rows train a cell — a failure
>   shape's output count is not what a completion of the segment costs.
> - **Both grains live in one cache**: `(signature, candidate)` selection
>   cells and the candidate-agnostic `(signature)` cell the estimate block
>   and Stage 4's sizing read. The per-signature cell is looked up — and its
>   miss enqueued — on **every** request, engaged or not, so the flywheel
>   warms on all traffic; per-candidate cells are touched only inside the
>   cost arm, so balanced traffic never churns selection cells.
> - **Fresh-but-thin cells wait for their TTL** rather than re-querying per
>   request: a segment under the n ≥ 50 gate re-measures at staleness
>   cadence, not request cadence. Cell map and pending queue are both
>   bounded, and both overflow toward cold — cold is the status quo.
> - **The refresher follows the settlement-recovery pattern**: opt-in,
>   spawned only by `serve`, exiting only on shutdown (a test harness that
>   started it could never drain `wait_for_background_tasks`); tests drive
>   the identical batch synchronously (`refresh_estimator_once`) and cross
>   the TTL by backdating cells (`age_estimator_cells`). A failed scan logs
>   and re-enqueues its cell rather than stranding it cold until TTL.

### Success estimator

`P(success | task_signature, candidate, validator_kind)` = Beta(1,1)-smoothed
empirical rate over `request_attempts` plus the day-one `shape_ok` labels:
`(passes + 1) / (n + 2)`, counting `validation_failed` as failure and
upstream errors as *health*, not success (they feed the health registry
instead). Signatures are user-scoped, so this too survives key churn. Cold:
the tiers.toml candidate order **is** the prior — it is the human-curated
quality ordering — so cold success mode degrades to walking the table
expensive-ward *within the margin-eligible escalation set* (below); the
per-user escalation budget bounds what a cold-start (or key-churning) user
can spend on that walk. Same background-refresh cache pattern as the cost
estimator. Policy thresholds (0.85 cost-mode floor, 0.6 success-mode floor)
are judgment-set numbers to be re-fit against telemetry before anything
prices on them.

### Selection policy

A pure function `order_candidates(priority, candidates, estimates, health)`
applied to `resolved.candidates` before `ProviderRoute::new` at `api.rs:186`
(the map's designated seam; `ProviderRoute` preserves given order,
`providers.rs:208-216`). One structural fact sharpens everything: **within a
tier, every candidate bills at the same tier sell rate**
(`resolved.sell_rates`; `config.rs:99-126` enforces this even for pinned
candidates), so in-tier reordering and escalation are *sell-price-invariant*
— they change ZeroRouter's COGS and the customer's success odds, never the
customer's bill. The honest corollary (Risks §4): sell-price invariance
means the reservation caps only the *customer's* money; ZR's walk COGS needs
— and now has — its own ceiling.

**Margin eligibility — the escalation set vs the availability chain.** A
candidate is a **negative-margin rung** when its cost basis exceeds its
owning tier's sell rate on any required dimension. When this design was
written the shipped table carried three: `anthropic/claude-opus-4-8` and
`bedrock/us.anthropic.claude-opus-4-8` (basis 5.00/0.50/25.00 vs high-end
sell 2.00/0.20/10.00) and — less obviously —
`anthropic/claude-haiku-4-5` in zero/balanced (output basis 5.00 vs sell
3.48).

**All three have since been removed from `tiers.toml`, and the class is now
closed at startup**: `validate_tier_catalog` (`config.rs`) rejects any
catalog containing a candidate whose basis exceeds its owning tier's sell
rate on any dimension, so a negative-margin rung can no longer be added by
editing the table. The eligibility machinery below is therefore a no-op
against today's table and is retained as the rule that keeps it that way if
the invariant is ever relaxed — and as the reasoning that decides what a
future tier is allowed to contain. The eligibility rule, applied in
every mode:

- The **availability chain** (a rung failed with a transport/upstream error
  or timeout → try the next) is the full tier-table order, negative-margin
  rungs included. This is bit-for-bit today's outage semantics: opus serves
  when sonnet is *down*, as a rare, accepted reliability cost.
- The **escalation set** (a rung completed but failed the governing
  validator/shape check → spend more for a better answer) excludes
  negative-margin rungs, always. Concretely: once any attempt in the walk
  has completed-but-failed-validation, subsequent negative-margin rungs are
  skipped (recorded `policy_skipped`); while every prior attempt failed for
  availability only, the walk is in outage-fallback mode and the full chain
  applies. A customer-declared validator can therefore *never* steer spend
  into a rung that loses money per token — the review's opus-farming attack
  (an unsatisfiable validator dispatching 2 sonnet + 2 opus attempts for
  ~60 USD/mtok of output COGS against a 10 USD/mtok bill) is structurally
  closed, not merely alarmed. The eligibility set is computed from the live
  tier table at request time, so a rate flip (e.g. the Sonnet-5 intro-price
  expiry, Risks §12) re-derives it with no code change.

Per mode:

- **balanced** — identity. Tiers.toml order, today's behavior exactly. The
  frozen control group.
- **cost** — order ascending by expected cost basis
  (`byte-bound input × candidate.rates.input + E[output | sig, cand] ×
  candidate.rates.output` — the parsed-but-unused rate table,
  `config.rs:29-36`, finally doing runtime work); filter out warm candidates
  with `P(success) < 0.85` when a validator is present, never below one
  candidate. In fixed tiers this is customer-price-neutral margin
  optimization; it becomes customer-facing $ optimization inside `zero/auto`.
- **success** — cheapest-likely-to-work first: escalation-set candidates with
  `P(success) ≥ floor` ordered by expected cost ascending, remainder
  expensive-ward. The escalation loop provides the guarantee; ordering
  minimizes expected spend on the way.

Health demotion applies last in all modes: unhealthy candidates sink to the
back, never disappear. Route-unbuildable errors
(`ProviderBuildError::NoAvailableCredentials`, `providers.rs:363-367`) stay
reachable only via missing credentials; `EmptyRoute` (`providers.rs:321-323`)
is the empty-input guard that catalog validation (`EmptyTier`,
`config.rs:170-174`) already makes unreachable — health demotion reorders and
never removes, so it can never manufacture either.

> **Shipped in Stage 3a** at the designated seam, with two decisions
> recorded. First, 3a's ordering is exactly the rollout row's bound: every
> mode's base order is the identity (no estimator exists to order by), and
> health demotion — sink to the back, table order preserved within each
> group — is the only permutation; the mode arms exist so 3b's cost/success
> orderings change `order_candidates`' body, not its callers. Second, the
> permutation is applied to the **built route** (`ProviderRoute::
> candidates_mut`) rather than to `resolved.candidates` before
> `ProviderRoute::new`: each rung's definition and its constructed transport
> move together, so a reorder cannot pair one candidate's definition with
> another's client — semantically identical (`ProviderRoute` preserves
> order), mechanically safer. Demotion's first line is now the ordering; the
> stage-2b walk-time skip remains as the backstop for a rung that cools
> between ordering and the walk reaching it, keeping its recorded
> `health_skipped` rows and its never-below-one-candidate floor. An
> all-demoted route partitions to itself, so ordering never manufactures an
> empty route.

> **Stage 3b flipped the cost arm** to expected-cost ordering, with one
> decision this section left open now recorded: **the cold-start circle is
> broken by a shared fallback.** A rung's expected output is its own warm
> `(signature, candidate)` p50 when one exists, else the segment's
> candidate-agnostic p50 — so a candidate that has never served (and so can
> never warm its own cell) is still orderable, and an all-thin route prices
> every rung at the same expected output, degenerating to rate order: the
> right cold-ish answer. Only when the segment itself is cold does the whole
> route fall through to the identity; the sort never runs on partial data.
> The sort is stable (ties keep the human-curated table order), runs on the
> built route like health demotion, and is f64 per-mtok arithmetic pricing
> an ORDERING, never a bill — sell-price invariance means it chooses
> ZeroRouter's COGS, and billing stays in `Decimal`. Balanced remains the
> frozen control group; success remains identity until 5a.

### Provider-health state

None exists today — the 429-cooldown map inside `ReliableModelProvider` dies
with each per-request construction (map §3.8). Add an in-process
`ProviderHealth` registry keyed `(provider, upstream_model)`:
`{ error_ewma (α = 0.3), cooldown_until }`. Updated at every attempt outcome;
a 429 sets a 60s cooldown; success decays the EWMA; demote when
`error_ewma > 0.5` or cooling. **In-process, single-instance, lost on restart
— deliberately** (today's deploy is one task; the failure mode of cold health
state is exactly today's behavior). Persisted/shared health is explicitly
deferred.

> **Deferred out of Stage 2 (shipped).** No `ProviderHealth` type, registry,
> demotion, or `'health_skipped'` row exists yet, for three reasons in the
> order that decided it. The streaming walk has no health state either, so
> adding one only to the buffered walk would open the exact cross-path
> divergence Stage 2 existed to close. Observe-only is already delivered, as
> data rather than as an in-process EWMA: `request_attempts` plus
> `request_attempts_candidate_ts_idx` is a queryable, durable,
> restart-surviving record of every attempt outcome and latency, which is a
> strictly better bake-week instrument than a registry that is lost on
> restart. And Stage 2's whole reviewability claim was "every pre-existing
> assertion still holds", which new mutable cross-request state would have
> made unauditable.
>
> The seam is one line each, and both sites exist in the shipped loop:
> `health.observe(candidate, outcome)` at the single `attempts.push(...)`
> funnel, and `health.should_skip(candidate)` at the top of the candidate
> loop, pushing a `'health_skipped'` row and continuing. Neither is written.
>
> The 429-cooldown map did not need replacing to be removed. Under ZeroRouter's
> wiring its key was the candidate id, each candidate was visited once per
> walk, and the map lived exactly one request — so no entry it wrote could ever
> be read back. Its only observable effect, the move-on-instead-of-wait break,
> is reproduced directly. Cross-request cooldown arrives with health.

> **Shipped in Stage 2b**, at exactly those two seams and on both walks in one
> change (`health.rs`; the funnel is `WalkLedger::push`, so recording an
> attempt row and observing it are the same act and no settle terminal can
> bypass health). Four points where this section was terse are now decided,
> recorded here so they are re-decided rather than re-derived:
>
> - **Demotion is the recorded skip**, not a reorder: `error_ewma > 0.5` or
>   cooling ⇒ a `'health_skipped'` row takes the rung's walk position and the
>   walk moves on — the shape migration 0004 already documents for that
>   outcome. Health-informed *ordering* ("sink to the back") belongs to the
>   knob's ordering modes and stays in Stage 3a.
> - **The skip is floored at one dispatch per walk** ("never below one
>   candidate", as in cost-mode filtering): a walk that has dispatched nothing
>   will not skip its last rung. A solo route therefore rides out a cooldown
>   the way it rides out the 429 itself, an all-cooling walk still dispatches
>   its final rung, and — on the streaming walk — every terminal keeps a
>   candidate to settle against, so an all-skipped walk cannot strand its
>   reservation.
> - **A 429 sets the cooldown and only the cooldown; availability errors move
>   only the EWMA; a success decays the EWMA and clears any cooldown.**
>   Pressure is time-bounded, brokenness is evidence-bounded: folding 429s
>   into the EWMA would ratchet a chronically busy rung past the threshold
>   where, skipped and therefore never observed, nothing could bring it back
>   before a restart. `validation_failed` and `aborted` are health-neutral —
>   the first is an answer-quality fact, the second is the router's doing.
> - **The streaming walk now labels an upstream 429 `'rate_limited'`** (it
>   recorded `stream_error`/`upstream_error` before), because the outcome
>   column is what feeds the cooldown and a mislabelled 429 on one path is
>   precisely the cross-path divergence this stage exists not to open. Labels
>   only; the streaming walk's control flow is unchanged.

### The escalation loop

**Non-streaming: unroll `ReliableModelProvider`** (map §5.4 — one change,
four payoffs). Replace `provider_route.chat(...)` (`api.rs:283-291` →
`providers.rs:243-264`) with a router-owned loop over
`ProviderCandidate::chat` (`providers.rs:172-180` — already used by the
synthetic-stream path, `api.rs:538`), mirroring the streaming walk's shape:

```
walk state: attempts_dispatched = 0, walk_cogs = 0, best_so_far = none,
            any_validation_failure = false
for candidate in ordered_candidates:
    # -- admissibility (skips are recorded, never silent) --
    skip 'policy_skipped'  if any_validation_failure and candidate is negative-margin
    skip 'policy_skipped'  if rung reservation-bound sell > min(budget_usd, reserved)   # bites only cross-tier
    skip 'policy_skipped'  if per-user escalation budget exhausted (serve best_so_far if one exists)
    skip 'health_skipped'  if health-cooled
    # -- hard ceilings --
    break unless attempts_dispatched < MAX_ATTEMPTS(4)
    break unless remaining_upstream_time (api.rs:790-794) covers the attempt
    break unless walk_cogs + attempt_cogs_bound <= walk_cogs_budget
    # -- dispatch --
    bump dispatch-time attempt-token buckets (per key + per user)     # velocity, below
    result = timeout(remaining, candidate.chat(request, temperature))  # 1 retry on 5xx/timeout
    attempts_dispatched += 1; record attempt {candidate_id, outcome, latency, usage?, cost_basis}
    walk_cogs += attempt cost basis (estimate lower-bound if usage unreported)
    transport error -> update health, continue          # availability chain: full table order
    completed:
        run governing check (declared validator, else shape check; label-only in balanced)
        pass -> served winner, break
        fail -> any_validation_failure = true; record 'validation_failed'; keep best-so-far; continue
settle served-or-best; attempts rows ride the settle transaction
```

This deletes the `scope_provider_fallback` / `take_last_provider_fallback`
attribution hack and the `ReliableModelProvider` construction. It lands the
attempts ledger, direct winner attribution, the validator hook, and health
instrumentation simultaneously, and is the first concrete step of the
thin-trait option, taken on its own merits. Existing attribution tests are
replaced by loop-level tests asserting the same observable.

> **Revised in Stage 2 (shipped): the retry cut was rejected.** Per-attempt
> retries stay at 2×500ms rather than dropping to one. Cutting them would have
> been a behavior change smuggled into a change whose entire warrant was that
> it changed no behavior — and an expensive one to review, since the retry
> budget is what a failing request costs in provider spend, and no test
> measured that budget before Stage 2 added one. There is no ladder to replace
> blind retry until Stage 5a ships one, so the cut would have traded
> availability for nothing in the interim. Revisit it with the ladder, where a
> retry and an escalation can be weighed against each other.
>
> Two further behaviors were reproduced rather than revised, both because
> dropping them would have moved money: the empty-completion re-roll (a blank
> turn returned instead of re-rolled settles as a billed 200), and
> context-window truncation including its abort of the whole walk. Whether an
> empty completion should escalate rather than re-roll is a Stage-5a question,
> not an availability one.

**The invariance claim, stated honestly:** the unroll is
**response-invariant on the no-failure path** — same candidate, same
response bytes, same billing on every request where the first dispatched
candidate succeeds. Failure-path behavior *intentionally changes*: a
transient error that today burns up to two 500ms-backoff retries on one
candidate will tomorrow fail over to the next candidate after one retry — a
different upstream may serve, visibly (the `x-zerorouter-provider/model`
headers, `api.rs:383-393`, are ZR's disclosure contract). The canary is
therefore two-sided: a byte-diff canary on `balanced` happy-path traffic,
**plus a fault-injection test** (candidate 1 returns one 5xx then would
succeed) asserting the new intended behavior — one retry, then ladder — so
the change is verified, not papered over. A third loop test pins the
flagship semantics the review showed a naive ceiling would break: a
success-mode walk in `zero/low-cost` (four margin-eligible rungs) against an
unsatisfiable validator dispatches **all four** rungs, bills exactly one
served attempt at tier sell rates, and records four attempts rows.

**Two money ceilings, never conflated.** The review correctly showed that a
single "walk sell-cost ≤ reserved" ceiling either kills escalation (read
cumulatively, attempt 2 always exceeds a one-attempt reservation) or bounds
nothing (read per-attempt, cumulative COGS is unbounded). So there are two
explicitly separate ceilings:

1. **Customer money** is capped per the *served attempt only*: its metered
   sell ≤ its reservation-bound sell ≤ `min(budget_usd, reserved sell cost)`
   (rung admissibility, above). In-tier this is automatic
   (sell-price-invariance); in `zero/auto` the top-rung reservation rule
   (below) keeps it true. The debit clamp (`db.rs:387`) remains the final
   backstop. Cumulative walk sell-cost is **not** a customer quantity —
   the customer never pays for losing attempts — and is therefore not a
   walk ceiling.
2. **ZeroRouter money** (walk COGS) is capped by three independent bounds:
   `MAX_ATTEMPTS = 4` dispatched attempts; a per-request
   `walk_cogs_budget = κ × reserved sell cost` (κ config-set, default 2.0 —
   projected per-attempt cost basis uses the warm estimate where available,
   the reservation bound where cold); and a **per-user escalation-COGS
   budget** (in-process daily bucket, config default judgment-set, e.g.
   min($5, 20% of trailing-30d spend)) — the proactive bound that holds from
   a user's very first request, which is what makes the reactive abuse
   monitors survivable against cold-start and key-churn. When the user
   budget is exhausted, success mode degrades to single-attempt with the
   `limited: "escalation_budget"` response field — never a hard failure.
   Margin eligibility (above) additionally guarantees every escalation
   dispatch has basis ≤ sell dimension-wise, so bound 2 is a bound on spend
   that revenue plus the orchestration fee is structured to cover.

Wall clock is capped by the shared 15-minute `UPSTREAM_REQUEST_TIMEOUT`
(`api.rs:47`, `remaining_upstream_time` `:790-794`); one reservation covers
the whole request — attempts never re-reserve.

**Streaming.** The walk already lives in the router (`api.rs:459-764`).
- *cost/balanced*: unchanged live flow; only upstream ordering differs, plus
  attempt records at the failure-continue (`api.rs:560`) and stream-error
  break (`api.rs:682`). Shape check = label only (Validators §).
- *success (or any declared validator)*: you cannot unsend streamed tokens,
  so these run **buffer-then-replay** — the escalation loop executes
  internally non-streamed, then the winning response replays through the
  machinery that already exists verbatim as `complete_synthetic_stream`
  (`api.rs:880-973`, built for non-streaming-capable candidates), with SSE
  keep-alives (`api.rs:428-433`) covering the buffering gap. TTFB becomes
  full walk latency; that is the honest price of a validated stream, and it
  is documented, not hidden. Abandoned-attempt partial COGS uses the
  per-chunk `token_count` estimate ZR already requests and ignores
  (`api.rs:583`) as a lower bound, flagged `tokens_estimated`.

**Attempt-token velocity accounting — dispatch-time buckets, not settle-time
SQL.** The review is right that a settle-time `request_attempts` SUM cannot
police the traffic it exists to cap: rows are inserted only at settle (up to
15 minutes after dispatch), their `ts` is the backdated attempt start, so
in-flight walks are invisible to any concurrent admission and most attempt
rows are born already outside a trailing-minute window — and the served
attempt's tokens would be double-counted against `usage_events`
(`db.rs:199-204`). The primary mechanism is therefore the review's own
suggested fix, promoted from fallback to design: **in-process per-key and
per-user minute buckets, bumped at attempt dispatch time** with the
attempt's reserved token bound — the only accounting that sees a walk while
it is happening. Admission consults the bucket as a pre-check beside the
existing SQL projection (`db.rs:190-215`); like the health registry and
estimator cache, buckets are restart-cold by design (failure mode = today's
behavior: attempt traffic uncounted). The SQL over `request_attempts`
(served by `request_attempts_key_ts_idx`) is the **audit and backfill view
only**, and it filters `WHERE NOT served` so the winner — already summed
from `usage_events` — is never counted twice.

### `zero/auto` (staged: the cross-tier ladder)

Cross-tier escalation exists only inside a new catalog construct, and the
review is right that the previous framing was structurally impossible:
re-declaring candidates under a `zero/auto` tier trips
`TierConfigError::DuplicateModelId` — `validate_tier_catalog` collects
candidate ids into one `BTreeSet` across **all** tiers (`config.rs:164,
193-195`) — and because the catalog is re-parsed per request
(`api.rs:179-181`), an invalid tiers.toml is a full-API 503. So `zero/auto`
is a **ladder tier**, a named catalog-schema change, not a candidate list:

```toml
[tiers."zero/auto"]
ladder = ["zero/low-cost", "zero/balanced", "zero/high-end"]  # tier ids, ascending
```

- `TierDefinition` gains an optional `ladder: Vec<String>`; validation
  requires exactly one of `candidates`/`ladder` non-empty, every ladder
  entry an existing non-ladder tier id, and the ladder non-decreasing in
  sell output rate (so "top rung" is well-defined as the last entry). A
  ladder tier declares no `rates` of its own — there is no zero/auto sell
  rate to leak or to pin against.
- **No candidate is re-declared and no alias exists**, so `DuplicateModelId`
  stays intact, `/v1/models` (`config.rs:128-141`) gains exactly one new
  row (`zero/auto`), and the concrete-candidate pinning path
  (`config.rs:112-125`) is untouched — a pinned candidate still resolves to
  its *owning* tier and bills its owning tier's sell rate, exactly as the
  regression test at `config.rs:261-292` already pins.
- `resolve("zero/auto")` returns a rung-structured route (per-rung
  candidates with their owning tier's sell rates); the winner→owning-tier→
  sell-rate lookup is total *by construction* because rungs never leave
  their owning tiers. Config tests to ship with the change: a spanning
  ladder loads; the lookup is total; duplicate candidate ids still fail;
  ladder entries referencing missing/ladder tiers fail.

Three rules make the ladder margin-safe by construction:

1. **The winner bills at its owning tier's sell rate** ("you pay the tier
   that answered"), disclosed per response via `tier_served`.
   Validator-forced escalation therefore costs the customer full freight at
   the expensive rung — there is no free upgrade to farm.
2. **Admission reserves at the top rung's sell rates.** This is the fix for
   the one place sell price is *not* escalation-invariant: reserved ≥ any
   possible winner's sell cost, so the debit clamp (`db.rs:387`) can never
   systematically bind. The flip side is honest too — you cannot enter the
   auto ladder without top-rung headroom in balance and caps.
3. **The ladder's escalation set contains only margin-eligible rungs** (the
   global rule, Selection policy §): with today's table every rung is
   margin-eligible — the negative-margin ones were removed and the catalog
   validator now refuses to load a table that reintroduces one — so the auto
   ladder escalates up to the sonnet-class rungs with nothing to exclude.
   The rule still binds any future tier. And `zero/auto` ships only at
   Stage 6, **after** the flat orchestration fee is live (Stage 5a), so the
   ladder is never a fee-less COGS faucet.

`zero/auto` is where `priority: cost` becomes customer-facing $ optimization
(cheap tier serves → cheap tier's rate bills), i.e. the MANAGED/AUTO flagship
lane of the pricing brief, entered with the ladder disclosed rather than
opaque. Auto-mode requests carry either a declared (floored — Billing §3)
reference model with savings-share, or the flat auto orchestration fee — the
lane never runs at bare 0% markup.

## Data model

Migration **0004** (`migrations/0004_estimate_and_select_substrate.sql`) is
the Stage-1 substrate: nullable insert-time `usage_events` columns (features,
selection/reservation provenance, outcome and validator-outcome labels, rate
and reserved-cost snapshots), the append-only `request_attempts` ledger, one
partial estimator index, and `api_keys.default_priority` (thesis-fixed enum,
inert until the knob stage). All `usage_events` additions respect the
append-only trigger (`0001_b0_schema.sql:106-124` blocks row mutation, not
DDL); every column is nullable, written at INSERT only, never backfilled —
pre-0004 rows read NULL = "not captured". Tables whose shape is still
stage-gated (validators registry, fee bookkeeping, savings columns,
estimator cell caches) are deliberately *not* created in 0004.

**Rust plumbing.** `UsageRecord` (`db.rs:17-26`) grows a single
`telemetry: RequestTelemetry` struct (all new columns) plus
`attempts: Vec<AttemptRecord>`; the settle INSERT (`db.rs:346-376`) extends
its bind list, and attempts rows are inserted **inside the same transaction,
after the event row** — the FK to the UNIQUE `usage_events.request_id`
(`0001:68`) holds, exactly-once is inherited from the reservation
`DELETE...RETURNING` (`db.rs:335-344`), and a failed settle drops attempts
with the event (consistent, self-healing). A request that never settles
(crash, reservation expiry released un-billed at next admission,
`db.rs:186-188`) loses attempt telemetry, never money; the roadmap's planned
settle-at-reserved expiry flip (map §4) shrinks that window. `UsageSession`
(`db.rs:36-42`) carries the task signature and reservation provenance from
admission to settle (reservation rows are destroyed at settle, so provenance
must be copied into the ledger). `persist_usage` (`api.rs:1116-1140`) takes
the telemetry struct; all 13 call sites update mechanically, passing
sentinels/NULL exactly where they pass sentinel strings today.

**Later migrations (shapes fixed here, shipped with their stages):**

- **0005 — fee substrate, Phase A (monthly aggregate), ships with Stage 5a**:
  extend the `credit_ledger` entry-type CHECK (`0002:27-28`) with `'fee'`
  (plus `fee entries are debits`, mirroring `0002:31-32`). A monthly fee row
  carries `request_id NULL`, which **satisfies** the existing usage↔request
  biconditional (`0002:35-36`) unchanged. Idempotency anchor: a
  `fee_statements` table, UNIQUE `(user_id, period_start, fee_type)`, written
  in the same advisory-locked transaction as the ledger row
  (`billing.rs:44-99` is the template — same per-user lock as
  admission/settle) by an admin biller command; a re-run is a no-op.
  **Insolvency semantics are specified, not accidental** (the review caught
  that a naive fee debit can violate the `0003:6-8` non-negative-balance
  CHECK and abort the invoice run): inside the locked transaction the biller
  debits `LEAST(fee_computed, current balance)`; the remainder is recorded as
  `fee_statements.fee_shortfall_usd` and **carries forward** into the next
  period's statement (opening-shortfall line), still subject to the
  cumulative never-worse-off cap; write-off happens only at account closure
  (a business decision, recorded as such). A $0 debit writes no ledger row
  (the ledger forbids zero amounts, `0002:29-30`), only the statement row.
- **0006 — validators registry + savings (Stage 5b/5c)**: `validators` table
  (immutable `slug@version` rows: `user_id`, `slug`, `version`,
  `kind IN ('json_schema','assert')`, `spec JSONB`, `spec_sha256`,
  disable-only, UNIQUE `(user_id, slug, version)`);
  `api_keys.reference_model` (customer-declared counterfactual; must resolve
  in the catalog, `config.rs:99-126`, **and satisfy the reference floor** —
  Billing §3 — both checked at PATCH time); `usage_events` gains
  `reference_cost_usd` and `reference_basis` (`'rate_only' | 'measured'`).
- **0007 — per-request fees, Phase B (Stage 7, only when per-success billing
  is earned)**: replace the biconditional with the OR-form that admits *both*
  fee shapes — `CHECK ((entry_type = 'usage' AND request_id IS NOT NULL) OR
  entry_type = 'fee' OR (entry_type NOT IN ('usage','fee') AND request_id IS
  NULL))` — because a fee⇒NOT-NULL constraint would be invalidated by Phase
  A's existing NULL rows, and the untouched original would reject per-request
  fee rows outright. Re-scope `credit_ledger_request_unique` (`0002:45-47`)
  to `(request_id, entry_type) WHERE request_id IS NOT NULL` so a usage debit
  and a fee debit for one request can coexist. Add `fee_events` (UNIQUE
  `(request_id, fee_type)`, `billed BOOLEAN` for the shadow phase,
  `fee_shortfall_usd`), append-only-triggered like `request_attempts`.

## Billing & savings accounting

**1. Customers pay the served attempt. Only the served attempt. In every
mode.** `cost_usd` remains the sell price of the served attempt's metered
usage at the resolved sell rates (`usage_cost(resolved.sell_rates, usage)`,
`api.rs:1134`; column contract `0001:104`) — unchanged formula, unchanged
meaning. Failed and escalated attempts' COGS is ZeroRouter's cost of goods —
that is precisely what the orchestration/value fee prices, and it keeps the
bill line intelligible ("one call, one result, one bill line") while the
entire settle machinery — advisory lock, `DELETE...RETURNING` exactly-once,
clamped debit (`db.rs:322-424`) — survives verbatim. Zero-charge release on
all-failed/undelivered paths (`api.rs:293-337`, `:766-787`) is untouched.
Gross margin per request is one expression:
`cost_usd − cost_basis_usd − attempts_cost_basis_usd`.

**2. COGS capture.** Per attempt: `usage_cost(candidate.rates, usage)` —
the same function with cost-basis rates; the rate table (`config.rs:29-36`)
has been parsed and validated all along. The served attempt's COGS lands on
`usage_events.cost_basis_usd`, losing attempts sum into
`attempts_cost_basis_usd` — making per-request gross margin a monitored
number from Stage 1. The three invisibly negative-margin candidates that
motivated this (opus at basis 5.00/0.50/25.00 vs high-end sell
2.00/0.20/10.00; haiku output at basis 5.00 vs balanced sell 3.48) have been
removed from the table and the class is now rejected at catalog load, so
what this measures is residual walk COGS rather than per-rung losses.
Unreported
usage stores NULL tokens (or the `token_count` lower bound with
`tokens_estimated = true`). Bedrock rows carry a known caveat — no
cached-token reporting at the pin while cachePoints are inserted (map §3.9)
— and are excluded from cache-adjusted margin alerting until the upstream
fix.

**3. Reference price & savings statement — a measured counterfactual, not an
assumed one.** `reference_model` is per-key, customer-declared, must resolve
in the catalog, and must satisfy the **reference floor**: its rates must be
≥, dimension-wise, the sell rates of the most expensive rung the key's route
can escalate to (for `zero/auto`, the ladder's top tier). The floor kills
the deflation exploit the review demonstrated (declare the cheapest
plausible reference → measured savings ≈ 0 → fee ≈ 0 while consuming the
full routing service at 0% markup); the existing never-worse-off cap kills
inflation harm; together they bracket the declaration honestly:
"the ceiling you asked us to beat."

The review's second savings attack also lands: pricing the reference at the
*served* model's token counts assumes the reference would have emitted the
same number of output tokens — an assumption, made by ZR, on the exact term
the fee turns on, i.e. the Decagon/Ada definition dispute reborn. So the fee
basis is measured, with provenance stamped per row:

- **Input side**: served input tokens priced at reference input rates. The
  prompt is byte-identical; tokenizer variance is second-order and the
  assumption is disclosed.
- **Output side**: the reference model's own observed output distribution
  for the same (user-scoped) task signature — its per-signature p50 from
  organic reference-model rows and from **shadow samples**: once a key opts
  into savings-share, ZR runs the declared reference on a small sample
  (~1-2%) of that key's cost-mode requests at ZR's expense (a COGS of the
  fee lane, priced into the share; the customer declared this exact model,
  so sending sampled prompts to it is within the declared counterfactual —
  disclosed alongside the escalation disclosure, Risks §11).
- Rows on segments with reference calibration (n ≥ 30 reference
  observations) carry `reference_basis = 'measured'` and are
  **fee-eligible**; rows without carry `'rate_only'` (same-token rate
  comparison — statement-only, labeled as such, $0 fee, shown as guidance).

At settle, `reference_cost_usd` is snapshotted into the row alongside
`sell_rates`/`basis_rates` JSONB — tiers.toml is re-parsed per request
(`api.rs:179-181`), so the snapshot is what makes every historical row
re-priceable and every savings dollar reconstructible from the row alone.
Every term is now customer-visible *and* measured: their declared model,
their actual inputs, the reference's observed outputs on their own traffic,
the published rate on the day. What ships first is the **statement**
(response `savings` block + portal report), not the fee.

**4. Fees — phase-gated, never from estimates.**
- *Success-optimizer, at escalation GA (Stage 5a): flat orchestration fee.*
  A per-request fee (cents-denominated; price point is a launch decision) on
  `priority: success` requests, extended to `zero/auto` requests at Stage 6;
  aggregated monthly into one `'fee'` ledger row via `fee_statements`
  (migration 0005, insolvency semantics above). This is the thesis's own
  "flat orchestration fee at first," moved to where the COGS starts: the
  escalation loop never runs fee-less at GA. (A fee-less pre-GA beta is
  invite-only, with the per-user escalation budgets doing the bounding.)
- *Cost-optimizer: savings-share (Stage 7).* `fee = 25% × max(0,
  Σ(reference_cost_usd − cost_usd))` over the calendar month, computed only
  over settled 200, `reference_basis = 'measured'` (and, where declared,
  validated) rows, **capped so cost + fee ≤ the reference total — the
  customer is never worse off than their own declared reference.** No
  reference declared → no savings-share; on `zero/auto` the flat fee applies
  instead (the lane never runs bare).
- *Per-validated-success (Stage 7, Phase B)*: only after its own shadow
  period, gated on deterministic validator kinds and per-key pass-rate
  monitoring.
- *Mechanics under the clamp* (Phase B): the usage debit settles first,
  clamped as today; the fee debits only remaining reservation headroom —
  `fee_debited = min(fee_computed, reserved − usage_debit)` — so each ledger
  row carries its actually-debited amount and the ledger-sum ≡ balance-delta
  invariant (`0002:13-14`) holds even when the clamp binds; any shortfall is
  recorded on `fee_events.fee_shortfall_usd`, and admission for fee-bearing
  modes reserves sell + maximum-possible-fee so shortfall is rare by
  construction.

**5. Money-path oddities are inherited, not smuggled.** The map's flagged
inconsistencies (timeout bills reservation at zero delivery, `api.rs:849` vs
the delivery-gated philosophy at `:735-737`; disconnect-between-candidates
charges full reservation, `api.rs:460-486`, vs all-failed releases free)
are *not* changed by any engine PR — normalizing billing semantics inside the
unroll would make both unreviewable. They get a dedicated normalization pass,
and the new columns record what happened either way.

## Rollout

Each stage is one independently shippable, revertible PR;
billable-actuals-first throughout. Map numbers refer to
`zeroclaw-integration-map.md` §5. (The review flagged the earlier Stage 3
and Stage 5 as secretly multi-PR; they are split below and the revert story
is real again.)

| Stage | Map # | Contents | Customer-visible? |
|---|---|---|---|
| 0 | 1, 2 | Prereqs, parallel: advance the ZC pin (builder rewrite `providers.rs:369-445`, `.timeout_secs(900)`; decide B0's fate); OpenRouter-shaped string pricing on `/v1/models` (`openai.rs:355-361`, `api.rs:147-152`) | pricing only |
| 1 | 3 | **Migration 0004** + telemetry at all 13 persist sites + user-scoped task signature + synthesized `finish_reason`(+`_source`) + `shape_ok` labeling + attempts rows from the already-router-owned streaming walk (`api.rs:459-764`). Zero behavior change, zero wire change; the margin dashboard (per-request gross margin and walk-COGS aggregates; the opus/haiku negative-margin rungs it was originally scoped to watch are gone from the table and now rejected at catalog load) and the data flywheel start here | no |
| 2 | 4 | **Unroll the non-streaming walk** into the router-owned loop; attempts rows everywhere; attribution hack deleted. `ProviderHealth` deferred to 2b — observation is delivered as `request_attempts` data instead, which is durable and restart-surviving where an in-process EWMA is not. Landed as four commits: characterization tests first, the unmetered-success overcharge on its own, the unroll with every pre-existing assertion unchanged, then the ledger and provenance | no |
| 3a | 6 | **The knob, visibility-only**: `zerorouter` request object + `:suffix` (resolve-first) + catalog colon-collision validation + per-key default threaded through auth/mints + `PATCH /api/keys/{id}` + the `zerorouter` response block (non-streaming + usage-chunk streaming). Ordering = identity or health-only; no estimator dependency; reservations byte-bound | yes |
| 3b | 6, 7a | **Estimator read path**: `EstimatorState` cache + background refresher + percentile SQL + n≥50/staleness gates; flip cost-mode ordering to estimator-backed (cold-fallback); `estimate` block appears. Reservations still byte-bound — **visibility before financial exposure** | yes |
| 4 | 7a | Estimator-informed **reservations** for eligible segments (floor at 0.25×requested_max; p99/p50 tail gate; escalation-capable requests stay byte-bound); provenance + `reserved_cost_usd` stamped; **dollar-denominated clamp-loss monitoring and per-segment/per-user auto-revert**. Reclaims velocity/credit headroom (`db.rs:216-273`) | throughput |
| 5a | 7b | **Validators + success mode + the fee**: inline validators, non-streaming escalation loop (margin-eligibility split, two money ceilings, per-user escalation budgets, dispatch-time velocity buckets), key-creation throttle, migration 0005 + **flat orchestration fee billed from GA**. Gated on the Stage-5 legal review (Risks §11) | yes |
| 5b | 7b | **Registered validators + buffered streaming**: migration 0006 (registry), `/v1/validators`, buffer-then-replay for validated streams | yes |
| 5c | 7b | **Savings statements at $0 fee**: `reference_model` (+ floor) via PATCH, shadow sampling, `reference_basis` provenance, response `savings` block + portal report | yes |
| 6 | — | **`zero/auto`**: ladder-tier catalog schema + config tests, top-rung reservation rule, pay-the-serving-tier billing, margin-eligible escalation set; flat fee extends to auto mode. Requires 5a (fee live) | yes |
| 7 | 7c | **Value fees**: savings-share activation (measured-basis rows only) and, after its own shadow period, per-request per-success (migration 0007). Gated per segment: decayed n ≥ 500 and fleet ≥ 5k settled priority-mode requests; trailing-30d p90 reservation coverage in [0.85, 0.97]; trailing-30d clamp-loss < $10/segment **and** hit-rate < 0.5%; reference calibration coverage ≥ 80% of fee-eligible rows; savings-statement spot-audit clean | yes |
| ∥ | 5, 8 | ZC preset + device-flow client (v0.9.0 milestone); upstream asks (`StreamEvent` finish-reason payload — retires the heuristic and flips `finish_reason_source`; publishable `zeroclaw-api`). None block the sequence | no |

## Risks & mitigations

1. **Estimate gaming / adverse selection.** Signatures, estimator cells,
   clamp-loss aggregation, and abuse monitors are **user-scoped** — minting
   or rotating keys resets nothing (the review's key-churn attack), and key
   creation is itself throttled counting disabled keys. Reservations sit at
   p99 × 1.25 with a floor at 25% of requested max_tokens, so distribution
   dilution both caps its per-row payoff and burns the attacker's own
   velocity/credit headroom; heavy-tailed segments (p99/p50 > 8) never leave
   `cold`; escalation-capable requests stay byte-bound in Stage 4 (the
   tail-correlated cohort). Billing is actuals so estimate error never
   mis-bills the customer; the automatic revert fires on **clamp-loss
   dollars** (per row, per segment 7d, per user 30d), with the dilutable
   hit-rate kept only as a secondary signal. `estimator_basis` +
   `reserved_cost_usd` make drift auditable per row.
2. **Rejected alternative, recorded: billing actuals above the
   reservation.** Suggested in review as an alternative to eating clamp
   loss; rejected because the clamp (`db.rs:387`) is the enforcement point
   of the non-negative-balance invariant (`0003:6-8`) under the per-user
   advisory lock — admission verified the balance against *reserved*, and
   concurrent requests have reserved against the remainder, so debiting past
   the reservation can overdraw a correctly-managed balance. Customer-side
   clamp stays; ZR-side loss is floored, dollar-alarmed, and auto-reverted.
   This question is settled.
3. **Validator gaming.** An always-pass validator (`regex ".*"`) farms
   nothing — no per-success fee exists until Stage 7, and when it arrives it
   is gated on deterministic kinds with pass-rate monitoring (a sustained
   ~100% first-rung pass rate moves the key to the flat fee). An
   unsatisfiable validator can no longer farm negative-margin attempts at
   all: validation failure **cannot** escalate into a rung whose basis
   exceeds its tier sell (Selection policy §), and as of the table cleanup
   no such rung exists to reach — the three that did (opus, bedrock-opus,
   haiku-output) were removed and the catalog validator now rejects any
   replacement at startup. What remains farmable is neutral-margin walk
   COGS, bounded by `MAX_ATTEMPTS`, the κ×reserved walk-COGS budget, and the
   per-user escalation budget — and priced by the orchestration fee that
   ships **with** the loop, not two stages later. The per-user
   `attempts_cost_basis / cost_usd` monitor degrades persistently abusive
   users to single-attempt with notice; the full-width spec hash on every
   row is the audit trail; content-addressed immutable versions kill
   retroactive weakening.
4. **Tail cost / customer-triggerable negative margin.** The review's
   sharpest attack, conceded and fixed structurally rather than by alerting:
   (a) the reservation caps only customer money — ZR walk COGS now has its
   own explicit ceilings (two-money-ceilings §); (b) negative-margin rungs
   are excluded from every escalation set by construction, verified against
   today's table (opus 25.00-basis output sold at 10.00; haiku 5.00-basis
   output sold at 3.48); (c) the escalation loop never ships fee-less at GA
   (flat orchestration fee in Stage 5a; `zero/auto` only after it); (d) the
   COGS columns still turn residual neutral-margin burn into an alertable
   aggregate. Never a quote before gates; alerts are the backstop, no longer
   the defense.
5. **Escalation-ceiling coherence.** The earlier single
   "walk sell-cost ≤ reserved" ceiling was ambiguous (cumulative reading
   kills escalation after one attempt; per-attempt reading bounds nothing) —
   resolved by the explicit two-ceiling split, and pinned by a loop test: a
   4-attempt success walk in `zero/low-cost` dispatches all four rungs and
   bills exactly one. This question is settled.
6. **Savings counterfactual.** Both directions are closed with mechanism,
   not contract language: deflation by the reference floor (reference ≥ the
   route's top escalation rung, checked at PATCH) plus the no-bare-lane rule
   (auto mode carries the flat fee when no reference is declared); inflation
   by the never-worse-off cap. The token-count basis is **measured**
   (reference model's own per-signature output distribution via organic rows
   and shadow sampling), with `reference_basis` provenance per row and fees
   restricted to `'measured'` rows — the disputed term is observed, not
   assumed, and every dollar is reconstructible from the row snapshot.
   Residual risk (a task mix the reference was never sampled on) is handled
   by the calibration gate, not by shipping anyway.
7. **Calibration failure / cold start.** Every degradation path lands on
   today's byte-bound behavior, never below it; the n ≥ 50 gate, 14-day
   window, tail-stability gate, provenance columns, and per-segment
   auto-revert make miscalibration measurable before money depends on it.
   Per-user cold start is bounded proactively by the escalation budget, so
   "the first cohort escalates before any alarm can act" is bounded in
   dollars from request one.
8. **`finish_reason` blindness** (map §3.4). Synthesized reason mislabels
   truncated-mid-tool-call as `tool_calls` and cannot see `content_filter`;
   persisted with `finish_reason_source = 'synthetic'` so training cohorts
   survive the upstream plumb without a break. Validators partially
   compensate (truncated JSON fails `json_schema` regardless of the label).
9. **Transparency vs. the price-comparison leak.** Attempts metadata names
   real candidates while billing tier sell rates — the comparison the pricing
   brief warns about (Copilot multiplier leak, rec #2). Position taken
   deliberately: per-request winner disclosure is *already* ZR's contract
   (`x-zerorouter-provider/model` headers, `api.rs:383-393`; candidate ids
   are public in `/v1/models`, `config.rs:128-141`), and auditability is the
   product — so the design keeps transparency and mitigates the comparison
   with the savings statement ("you pay for the outcome ladder, not the
   winning rung") rather than retreating to opacity it no longer has.
10. **Buffer-then-replay TTFB** (success-mode streaming): first token ≈ full
    walk latency. Disclosed; `balanced`/`cost` streaming stays live (the
    shape check never buffers a stream — Validators §); the 15-minute budget
    time-boxes it; a tee-with-late-abort design is explicitly deferred until
    demand proves it.
11. **ToS / provider-terms exposure is a legal gate, not a policy
    sentence.** The macro shape is the sanctioned one — value-added routing,
    validation, and escalation on commercial API keys, transparent metered
    pass-through plus orchestration/outcome fees, no consumer-subscription
    resale, no flat all-you-can-eat tier (pricing brief recs #3/#4) — but
    the escalation loop replicates one customer's prompt across competing
    providers within one logical request and uses one provider's output to
    decide whether to route to a competitor, and shadow sampling adds a
    second cross-provider replication path. Enforcement is active (Anthropic
    revoked OpenAI; the OpenClaw ban; Priority Tier closed). **Before Stage
    5a ships**: per-provider commercial-terms review confirming (a)
    cross-provider replication within a request and (b) output-gated
    competitive routing are permitted, with the escalation set constrained
    to confirmed providers until each clears; privacy-policy disclosure of
    multi-provider dispatch and shadow sampling in addition, not instead.
12. **In-process state amnesia.** Estimator cache, health registry,
    per-user escalation budgets, and dispatch-time velocity buckets are
    per-instance and restart-cold by design (single-task deploy today); a
    second instance degrades to cold-start behavior — approximate limits,
    never incorrect billing (money paths are all DB-transactional). Revisit
    at scale-out.
13. **Attempts telemetry loss on never-settled paths.** Attempts ride the
    settle transaction (FK-valid, exactly-once inherited, dispute-auditable);
    crash/expiry paths lose attempt rows but never money. Accepted; shrinks
    with the planned settle-at-reserved flip (map §4). This is also why
    settle-time attempt rows are *not* the velocity-enforcement mechanism
    (dispatch-time buckets are — Engine §), only its audit trail.
14. **Clock risk** (map §4): Sonnet-5 intro sell pricing expires 2026-08-31
    (`tiers.toml`); the rate-snapshot columns make the flip
    auditable, and the margin-eligibility set is recomputed from the live
    table — but note the consequence, now sharper than when this was
    written: the high-end sonnet rows sit at basis **==** sell, so the
    moment the intro basis lapses without a matching sell-rate raise they
    violate the catalog invariant and `validate_tier_catalog` **refuses to
    load the table**. Because the catalog is re-parsed per request, that is
    a full-API 503, not a quiet drop out of the escalation set. Loud beats
    bleeding, but it is an outage: the tier table needs an owner and a
    raised sell rate landed *before* that date.

## Deliberate v1 exclusions

`priority: latency` (no per-candidate latency telemetry until the attempts
table matures — the knob ships when the engine can honor it, and someone pays
for it); judge/rubric validators; webhook or customer-code validators
(declarative-only is the safety and auditability line); per-request
`reference_model` (per-key only — prevents cherry-picked counterfactuals);
`on_validation_failure: "error"`; per-request `max_attempts` / success
floors; mid-stream validation and tee-with-late-abort; speculative parallel
dispatch (doubles COGS and ToS exposure); persisted or cross-instance
provider health, budgets, or velocity buckets; estimator cell tables
(in-process cache + rebuildable SQL is the v1 store); any ML beyond SQL
percentiles and Beta counts; fixed quotes, success guarantees, and per-task
fixed pricing (the analytics are built first — pricing brief rec #5);
admission-time SQL over `request_attempts` (dispatch-time buckets are the
enforcement mechanism; the SQL is audit-only); billing actuals above the
reservation (settled — Risks §2); tokenizer integration; re-reservation on
escalation; `n > 1` choices; changes to the flagged money-path oddities
inside engine PRs; B0 back-port (decided in the pin-advance PR, map §5.1).

## Open questions

1. **Pooled vs. per-user signatures.** Per-user isolation wins v1 on
   anti-gaming simplicity, at the cost of per-customer cold start. The raw
   feature columns keep a pooled/hierarchical re-keying (shrinkage toward
   fleet marginals with per-user residual isolation) open without schema
   change — revisit when a user's cold-start measurably delays `learned`
   status.
2. **Numbers to fit before they bind.** The floor fraction (0.25), tail gate
   (p99/p50 ≤ 8), κ (2.0), clamp-loss thresholds ($1/$10/$50), per-user
   escalation budget, orchestration fee price point, and shadow-sampling
   rate (~1-2%) are all judgment-set; each has a shadow window in its stage
   before it gates money or throughput.
3. **Streaming metadata reach.** The `zerorouter` block requires
   `stream_options.include_usage`. Should ZR eventually append the block
   unconditionally (an extra empty-`choices` chunk before `[DONE]`)? Needs
   client-compat evidence first.
4. **Judge validators.** What dispute machinery (logged verdicts, pinned
   judge model, replayability) is sufficient before a judge outcome may gate
   escalation — and how much longer before it may gate a fee?
5. **Settle-at-reserved flip interplay** (map §4): when expiry inverts to
   settle-at-reserved, expired requests begin writing usage_events rows —
   decide then whether buffered attempts flush with them, and ensure the
   estimator-sized (smaller) reservation is what settles.
6. **Scale-out.** At >1 task, health, estimator, budget, and velocity-bucket
   state diverge per-instance. DB snapshots, gossip, or indifference —
   decide when the second task is real.
7. **Phase-A shortfall carry-forward horizon.** Carry-forward is the default;
   whether shortfall older than N months is written off (dunning vs
   write-off) is a business decision to make before the first real
   fee-bearing cohort, not a schema question (`fee_shortfall_usd` supports
   either).

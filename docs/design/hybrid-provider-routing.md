# Hybrid provider routing: zero-retention by default, addressable by design

Status: **phase 1 implemented, DARK (2026-09-02) — unified ids + retention
switch + fail-closed shipped behind `ZEROROUTER_UNIFIED_IDS` (default off) and
unlisted from `/v1/models`; decision 2 resolved (stage it). §9 has the settled
decisions and the probe result; phases 2–3 (the `provider` request object,
posture reporting) and cache affinity remain future work.**
Driver: the same model is sold by several providers with *different retention
postures* (claude-sonnet on anthropic / bedrock / vertex; gpt-5.6 on
openai-direct / bedrock). Give callers one id that "just does the right thing"
— routes to a zero-retention provider by default — without giving up the
transparency that is the whole product.

## The one-sentence version

`data_collection: deny` is the **default, not an opt-in**: a bare model id
routes to zero-retention providers first and **fails closed** rather than
silently sending a prompt to a retaining provider; the explicit `bedrock/…`
and `vertex/…` ids stay as addressable aliases for callers who want to pin one
provider. Everything below is the precise semantics of that sentence.

## Why this is the inversion, not a copy

OpenRouter solved same-model-many-providers first, so it is the reference —
but the default is the point of disagreement, not the mechanism.

- **OpenRouter:** one canonical id, a hidden provider pool, and a `provider`
  request object to steer it — `sort` (price/throughput/latency), `order`,
  `only`/`ignore`, `allow_fallbacks`, `max_price`, and
  `data_collection: "allow" | "deny"`. The **default** load-balances on
  price + uptime; retention is an **opt-in filter** you remember to set.
- **ZeroRouter:** the same machinery, with `data_collection` **defaulted to
  `deny`**. That single flipped default *is* the product. Everything else here
  exists to make that default honest under load, not just on the happy path.

A ZDR promise that silently degrades when a provider is down is not a ZDR
promise. So the design work is almost entirely in the failure modes, not the
happy path.

## Today's baseline (what we are extending, not replacing)

Two mechanisms already exist and the hybrid is their composition:

1. **Explicit per-provider ids ("twins").** `bedrock/claude-sonnet-5` vs
   `anthropic/claude-sonnet-5`; `vertex/gemini-*` vs `google/gemini-*`. These
   exist *because retention differs and must be addressable* — a customer must
   be able to say "the zero one" vs "the cheap one" rather than be silently
   assigned whichever we felt like. **These do not go away.** They become the
   *pin* primitive.
2. **Tiers with ordered candidates ("the walk").** A tier already dispatches
   across an ordered candidate list, skipping unhealthy rungs. The unified id
   is just a tier whose candidates are *the same model across providers*.

The hybrid adds a third thing on top of these two: a **unified id** with a
**retention-partitioned candidate order** and an **explicit, argued failure
policy**.

## The model

A **unified model id** (e.g. `claude-sonnet-5`, `gpt-5.6-luna`) is a tier
whose candidates are the same model on different providers. Each candidate
carries, beyond the existing fields:

- `provider` (bedrock / vertex / anthropic / openai / …),
- `retention` — the *posture* with its *basis* (`zero{contract|enforced-config|
  published-default}` or `standard`), the same pin the catalog already tracks,
- health / price / latency signals (already collected by the walk).

### Candidate ordering (the default policy)

1. **Partition by retention first.** All `zero` candidates rank above all
   `standard` candidates. This partition is prior to price/latency — retention
   is not a tiebreaker, it is the primary key.
2. **Within the zero partition,** order by health (available first), then the
   caller's `sort` (default `price`, options `throughput` / `latency`), then a
   stable provider order for determinism.
3. **The standard partition is not eligible at all** unless the caller has
   opted into it for this request (see the crux). When eligible, it is ordered
   the same way *below* the zero partition.

So the default eligible set is **only the zero candidates**, ordered by price.
Standard providers are invisible to a default request even as a fallback.

### Unified id vs explicit alias — precise semantics

| | Unified id (`claude-sonnet-5`) | Explicit alias (`bedrock/claude-sonnet-5`) |
|---|---|---|
| Candidates | all providers of the model, retention-partitioned | exactly one provider |
| Fallback | across the *eligible* set (see crux) | **none** — a pin is a pin |
| Default posture | zero-first; fails closed if no zero route | whatever that provider is (its own pin) |
| `/v1/models` badge | the posture of its **default route** | its own single posture |

A pin means pin: `bedrock/claude-sonnet-5` never silently becomes
`anthropic/…`, even under a Bedrock outage. That is the whole reason the
explicit ids exist and it must not be eroded by the unified layer.

## The crux: no zero route available — fail closed, or fall back?

This is the decision that must be **argued, not assumed**, because it is where
the label is either enforced or revealed to be decorative. Three options:

### Option A — Fail closed (the recommended default)

If every zero-retention candidate for a unified id is unavailable (down,
rate-limited, out of quota), the request **fails** with a specific,
actionable error — `503 no_zero_retention_provider_available` — and does
**not** route to a standard-retention provider.

**The argument for making this the default:** the customer reached for
ZeroRouter, and for the bare id, *because zero is the promise*. The single
worst thing the system can do is take a prompt that a customer believes is
going to a non-retaining provider and, invisibly, under load, send it to one
that keeps it. That is not a degraded experience; it is a broken guarantee,
and it breaks precisely when the customer is least able to notice (a transient
provider outage). A visible `503` is a failure the caller can *handle* — retry,
back off, or explicitly widen the pool. A silent downgrade is a failure the
caller *cannot* handle because they never learn it happened. For a product
whose entire pitch is "we don't retain, and we can prove it," fail-closed is
the only default that keeps the pitch true.

This is also **consistent with the enforcement the codebase already has.** The
xAI lane asserts its `x-zero-data-retention` attestation *before the first byte
is forwarded* and refuses otherwise; the Bedrock lane runs under an enforced
`data_retention_mode: none`. Both already fail closed on retention at the
**per-response** layer. If the **routing** layer silently fell back, it would
be the one weak link in an otherwise fail-closed chain — the cross-provider
policy must match the guarantee its own candidates already make, or it is the
place the promise leaks.

Cost of this default: a real, visible unavailability when *all* zero providers
for a model are simultaneously down. That is rare (it requires every zero
provider of a model to fail at once), it is honest, and it is the caller's to
resolve — which leads to Option B as the escape hatch, not the default.

### Option B — Explicit, per-request opt-in fallback

A caller may set `provider.data_collection: "allow"` (OpenRouter-compatible)
— or the sugar `provider.allow_standard_retention_fallback: true` — to widen
the eligible set to include standard-retention candidates for **this request
only**. Then, when no zero route is available, the request is served by a
standard provider *and the response stamps the posture that actually served
it* (see "what the response reports"). This is opt-in, per-request, never a
default, and never silent. It exists so that a caller who genuinely prefers
"answered-but-retained" over "failed" can say so explicitly and own that
choice.

### Option C — Silent fallback (rejected)

Route to standard when zero is down, without telling the caller. **Rejected.**
It is Option A's failure mode with the honesty removed. A retention label that
is not enforced is worse than no label, because it manufactures trust it does
not keep. This option is named here only so the rejection is on the record.

**Recommendation: A as the default, B as the explicit escape hatch, C never.**
Fail closed unless the caller opts in, and always report the posture that
served. **DECIDED (Jordan, 2026-08-25): a switch — ZDR-only by default, allow
others when flipped.** See the next section for where that switch lives.

## The switch: a persistent retention policy, not just a per-request flag

Option B above is per-request, but the durable default belongs on the
**key** (and, as a ceiling, the account). A `retention_policy` on the API key:

- `zdr_only` (**default**): the key's requests use the zero partition and
  **fail closed** when no zero route is available. This is the switch in its
  default position — a key created today is ZDR-only without anyone setting
  anything.
- `allow_non_zdr`: the key may fall back to (or, with `data_collection` /
  `only`, target) standard-retention providers. Flipping the switch is a
  deliberate, auditable act on the key.

Precedence, tightest wins: **per-request `provider.data_collection`** (one
call) is read *under* the **key `retention_policy`** (the standing default),
which is read *under* an optional **account ceiling** (`zdr_only` at the
account level forbids any key from flipping to `allow_non_zdr` — for orgs that
must guarantee ZDR tenancy-wide). A per-request `data_collection: "allow"` on a
`zdr_only` key is rejected, not honored: the standing switch is a floor the
request cannot undercut, only the key/account owner can lower it. This makes
"ZDR-only" a property someone can *set once and rely on*, rather than a flag
every caller must remember on every request.

## Failover triggers: what moves the walk, and the invariant

Failover is **not** in tension with fail-closed — fail-closed governs the
*retention boundary*, not whether we give up on the first try. Within the
eligible (switch-permitted) set, the walk moves on for three reasons:

1. **Unavailability** — 5xx, connection error, timeout. Try the next candidate.
2. **Rate limit / provider quota (429)** — the provider is up but capping *us*.
   Same-model, different-provider failover is the clean fix: identical model,
   different backend, no change to the answer. Try the next candidate.
3. **Cap / budget** — two sub-cases that must not be conflated:
   - a **provider-side quota** is case 2 above (429) → same-model failover;
   - a **customer's prepaid balance / spend cap** near-exhausted has no cheaper
     *same* model to fail to. The only "failover" is a **different, cheaper
     model**, which changes the answer — a quality downgrade. So it is
     **opt-in, never silent**: a per-key/route
     `on_insufficient_balance: fail | downgrade_to "<model>"` policy (default
     `fail`), and the fallback model is itself subject to the retention switch.

**The invariant across all three:** failover only ever moves *within the
retention partition the switch permits*. Under `zdr_only` the walk fails over
freely among zero-retention providers and, when they are exhausted, **fails
closed (503)** — an outage or a cap is never a license to silently drop to a
retaining provider. Crossing to standard requires `allow_non_zdr` (or a
per-request `data_collection: allow`, bounded by the switch); a model downgrade
requires its own opt-in. So: **same-model → other-provider failover is
retention-preserving and default-on; crossing the retention boundary and
swapping to a cheaper model are both explicit, separate opt-ins.**

## The `provider` request object

OpenRouter-compatible in shape so migrants can paste their config, with the one
inverted default:

```jsonc
"provider": {
  "sort": "price" | "throughput" | "latency",  // default "price"; retention partition is always primary
  "order": ["bedrock", "vertex"],              // explicit priority within the eligible set
  "only":  ["bedrock"],                         // allowlist of providers
  "ignore": ["anthropic"],                      // denylist of providers
  "allow_fallbacks": true,                      // fall back across the ELIGIBLE (same-posture) set; default true
  "data_collection": "deny" | "allow",          // DEFAULT "deny" — the inversion
  "max_price": { "prompt": "...", "completion": "..." }
}
```

Two semantics that must not blur:

- **`allow_fallbacks`** governs fallback *within the eligible set*. It does
  **not** cross the retention boundary. Turning it off means "don't try a
  second zero provider"; it never means "try a standard provider."
- **`data_collection`** is the *only* control that crosses the retention
  boundary. `deny` (default) → eligible set is the zero partition only, fail
  closed. `allow` → eligible set includes the standard partition, ordered
  strictly below zero, and the served posture is reported.

`only` / `ignore` / `order` operate *inside* whatever the retention partition
allows; they can never promote a standard provider ahead of a zero one, and
they can never make a standard provider eligible on their own (`data_collection`
does that).

## What the response reports (transparency is not optional)

Because the default can fail closed and the opt-in can downgrade, **every**
response must make the outcome legible:

- Response metadata + `usage_events` record: the **served provider**, the
  **served retention posture** (`zero{basis}` / `standard`), and a
  `retention_fallback: bool` when B kicked in.
- `/v1/models`: a unified id advertises the posture of its **default route**
  (zero if it has any zero candidate; standard only if it has *no* zero
  candidate — see open decision 6). Explicit ids advertise their own. The
  existing retention-drift check extends to unified ids: the advertised badge
  is *derived from the live candidate set*, never hand-pinned, so it cannot go
  stale against the providers that actually back it.

## The flagship case: what actually carries the hybrid (probe run 2026-08-25)

The `mode:none` probe ran on prod (161457899654) and the honest reading is that
the OpenAI-on-Bedrock story is **held behind AWS Sales**, not shippable today:

- **Proprietary GPT-5.6** (`openai.gpt-5.6-luna/terra/sol`) is **Sales-gated** —
  the invoke returns `AccessDenied, not available for this account, contact AWS
  Sales`, even though `get-foundation-model-availability` says AUTHORIZED. This
  is the marquee "ZDR GPT-5.6" case and it is real, but it is **blocked on the
  AWS Sales agreement** (already on the unlock list); its retention mode can't
  even be tested until access lands. When it does, `gpt-5.6-luna` becomes the
  unified `[ bedrock (zero, if mode:none holds) → openai-direct (standard,
  opt-in) ]` flagship. That is the OpenAI-lineage prize worth waiting for.
- **Open-weight `gpt-oss` on Bedrock is deliberately NOT built** (decision,
  2026-08-26). It invokes under `mode:none`, but it is a mid-tier model that is
  already served — and served ZDR — elsewhere in the catalog, so a Bedrock copy
  adds a redundant path to a model nobody is asking for. It is not a flagship;
  it is only *proof that the Bedrock ZDR path works for OpenAI-lineage models*,
  which is why the GPT-5.6 Sales unlock is worth pursuing.

So the hybrid does not lean on gpt-oss. Its **live** on-thesis examples are the
zero-retention lanes already serving: **Claude on Bedrock** (`mode:none`, live)
and **`fireworks/deepseek-v4-flash`** — the catalog's cheapest lane
($0.14/$0.28), a strong efficient model, zero-retention by Fireworks' published
open-model default. The **marquee** example (GPT-5.6 on Bedrock) waits on AWS
Sales. It also stands on claude-sonnet across providers (bedrock + anthropic +
vertex-once-the-exception-lands).

## Caching & provider affinity (added 2026-08-31)

Prompt caching and zero retention coexist, because the industry's short-TTL
caches are **volatile-memory, tenant-isolated, minutes-scale** — not data at
rest. The catalog already embodies this: Fireworks' re-verified page says
cached KV data lives "in volatile memory for several minutes … never written
to persistent storage"; Bedrock's account-scoped ~5-minute cache is
compatible with `data_retention_mode: none`; xAI and Together publish cached
rates under their ZDR postures. The one exception is Vertex, whose 24-hour
cache Google's own ZDR checklist requires disabled — so it is, and vertex
lanes are the one zero family without a cache.

Affinity (the OpenRouter mechanism, verified against their docs): after a
cache-bearing request, remember which provider served it and prefer that
candidate next time, so follow-ups hit the provider's warm cache at its
cache-read rate. The mechanism stores routing memory, not content, and we
adopt it with two constraints:

1. **Partition-bounded.** Affinity never crosses the retention boundary — we
   stick to the same zero provider to keep a cache warm, never to a standard
   one. Since cache hits require the same provider anyway, this costs nothing
   on cache efficiency; it only forbids a trade the product would never make.
2. **Content-free at the router.** The affinity key is a client-supplied
   `session_id` when present, else a non-invertible hash of the prompt
   prefix, held in memory with a short TTL and never persisted. We remember
   *where* we routed, never *what*.

Failover deliberately breaks a cache (availability beats cache economics);
the customer pays one cold prompt on the new provider, identical to
OpenRouter's behavior when stickiness cannot hold. Activation mirrors their
gate: affinity only engages when the provider's cache-read price is below
its input price.

Build item independent of the hybrid: the router currently refuses explicit
`cache_control` breakpoints (`400 cache_control_unsupported`). Passthrough on
the anthropic-wire lanes is what makes ZDR caching real for Bedrock-Claude,
the flagship zero lane.

## Rollout (each phase is shippable and reversible)

1. **Unified ids as multi-candidate tiers over existing zero lanes.** No
   behavior change for anyone pinning an explicit id. Bare ids appear and route
   zero-first, fail closed. (Depends on nothing new.) **IMPLEMENTED, dark
   (2026-09-02).** Notes on what shipped, and the conservatism it chose:
   - **Dark gate.** Unified ids resolve only when `ZEROROUTER_UNIFIED_IDS` is
     set (default off), and are never listed on `/v1/models` this phase — a
     separate `TierCatalog::unified` map that `resolve`, `model_listing`, and the
     drift checks do not read. So the storefront and every pinned route are
     byte-for-byte unchanged; the operator can drive routed traffic in prod and
     watch it with zero customer-visible change.
   - **Equivalence is doubly conservative.** Two pins unify only when their bare
     ids match EXACTLY (no cross-version merge) AND they sell at the IDENTICAL
     rate. The rate rule is stricter than the doc's same-model rule and exists
     to keep "one admission/money path" literally true: a unified route carries
     one sell schedule and settlement bills at it, so pins that price
     differently would make the charge depend on which provider served — a
     billing decision this layer must not make. Consequence in today's catalog:
     the three Google/Vertex Gemini twins unify (identical price); the
     Bedrock/Anthropic Claude **haiku** twin does NOT (the zero lane costs more).
     Unifying the haiku twin needs a deliberate rate-reconciliation policy
     (which rate the unified id bills, and the resulting margin), which is a
     pricing decision for the owner, not a silent config effect — deferred.
   - **The switch vs a pin.** The `retention_policy` switch governs a routed
     (unified) id only. An explicitly provider-pinned id is served as itself
     whatever the switch says — including a `zdr_only` key pinning a standard
     lane by name — because pinning is a deliberate single-lane address, not the
     routed default the switch is the floor for. The per-request
     `data_collection` floor (rejecting `allow` on a `zdr_only` key) is a
     phase-2 concern along with the `provider` object.
2. **The `provider` request object,** `data_collection` defaulting to `deny`.
3. **Posture reporting** in responses + `usage_events` + the derived
   `/v1/models` badge, and the drift check extended to unified ids.

## Decisions (§9)

**Settled (Jordan, 2026-08-25):**

1. **[CRUX] Fail-closed default + a switch to allow others.** Fail closed by
   default (Option A); standard-retention reachable only by flipping the key's
   `retention_policy` to `allow_non_zdr` (or per-request `data_collection`,
   bounded by that switch); silent fallback (C) never. See "The switch".
3. **Inverted default `data_collection: "deny"` — YES.** It is the product.
5. **Unified-id naming — bare ids.** Routed entries are bare (`claude-sonnet-5`,
   `deepseek-v4-flash`); the provider-prefixed ids (`bedrock/…`, `vertex/…`,
   `openai/…`) carry the pin semantics; no `zero/` prefix (a routed id's posture
   is derived and can be standard, so "zero" stays a badge, never a name).
6. **A model with only standard providers — publish with a `standard` badge.**
   Withholding hides a model we do serve; the honest badge is the ZeroRouter
   move.
9. **Failover is retention-bounded.** Same-model → other-provider failover on
   unavailability/429 is default-on and stays inside the switch's partition
   (fails closed under `zdr_only` when zero routes are exhausted); prepaid/spend
   exhaustion triggers a cheaper-model downgrade only as an explicit
   `on_insufficient_balance` opt-in. See "Failover triggers".

**Result of the `mode:none` probe (run 2026-08-25 on prod 161457899654):**

- Proprietary **GPT-5.6** (`openai.gpt-5.6-luna/terra/sol`) → `AccessDenied,
  not available for this account, contact AWS Sales`. So the flagship "ZDR
  GPT-5.6" case is **blocked on the AWS Sales agreement** (already on the
  unlock list), not on our code. Its retention mode can't be tested until
  access is granted. `get-foundation-model-availability` reporting AUTHORIZED
  is misleading — the invoke is the ground truth.
- **Open-weight `gpt-oss-120b` / `gpt-oss-20b`** → invoked successfully under
  `data_retention_mode: none`, but **deliberately NOT built as a Bedrock lane**
  (decision, 2026-08-26): mid-tier, already served ZDR elsewhere, redundant. It
  stands only as proof the Bedrock ZDR path works for OpenAI-lineage models.
  The live ZDR examples the hybrid actually leans on are Claude-on-Bedrock and
  `fireworks/deepseek-v4-flash`; the marquee OpenAI example waits on the AWS
  Sales unlock for GPT-5.6.

**Still open — one call left (decision 2):**

### Decision 2 — expose unified ids now, or stage behind the explicit ids?

*Pros of unified ids:* one obvious entry per model (`claude-sonnet-5`) instead
of asking users to know the twin they want; the zero-first default lives at the
id, so the ZDR promise is delivered without the caller doing anything; matches
the OpenRouter mental model migrants already have; lets us add/retire a backing
provider (add or drop a lane) without users rewriting configs; concentrates
health/price/latency routing in one place.

*Cons:* a routed id is less predictable than a pin — the same id can be served
by different providers run-to-run, which matters for reproducibility,
per-provider quirks (tokenizer, tool-format edge cases), and debugging ("which
backend answered?"); the `/v1/models` badge is *derived*, so it can change as
the candidate set changes; it adds routing/fallback logic that is a new place
for bugs on the money path; and it risks *diluting* the transparency pitch if
users stop thinking about which provider they hit. Mitigation: always report
the served provider+posture (already required), and keep the explicit ids as
first-class pins so anyone who wants determinism has it.

*Recommendation:* ship unified ids, but **stage** — Phase 1 introduces them
over existing zero lanes with explicit ids untouched, so nothing anyone relies
on changes and we can watch routing behavior before it is the default path.

### Decision 5 — unified id naming: bare vs prefixed — DECIDED: bare

*Bare* (`claude-sonnet-5`, `deepseek-v4-flash`): friendliest, exactly matches
OpenRouter and OpenAI-client expectations, lowest migration friction. Con: a
bare id gives no visual signal that it is *routed* rather than pinned — a user
may not realize `claude-sonnet-5` could be served by any of several providers.

*Prefixed* (`auto/claude-sonnet-5`, or `zero/claude-sonnet-5`): the prefix
makes "this is a policy-routed id, not a pin" explicit, and `zero/` doubles as
a retention signal. Con: it is non-standard, breaks copy-paste from other
tools, and `zero/` overpromises if a `zero/…` id ever has to publish as
standard (decision 6 says such ids can exist).

*Recommendation:* **bare** ids for the routed entries, with the explicit
`bedrock/…`, `vertex/…`, `openai/…` ids carrying the pin semantics — the
provider-prefixed form already reads as "a specific provider," so the absence
of a prefix reads as "let the router choose," which is the right split. Avoid
`zero/` as a prefix precisely because a routed id's posture is derived and can
be standard; keep "zero" a *badge*, not a *name*.

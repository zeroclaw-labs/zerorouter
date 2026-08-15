# Edge mode: a $0 local rung in the `zero/*` ladder

Status: **draft — approved direction, pre-announcement scope**
Driver: make ZeroRouter the all-in-one inference backend for agents — one
OpenAI-compatible endpoint that spans the user's local models and every cloud
provider, ZDR always, under one prepaid spend cap.

## Motivation

Agent workloads burn enormous token volume on mostly-easy steps (tool glue,
formatting, retries) punctuated by genuinely hard ones. Technical users already
run llama.cpp/vLLM/Ollama and will route easy traffic there with or without us
— the only question is whether that routing lives in *our* ladder (keeping the
whole workload, and the cloud burst where spend concentrates, in one funnel) or
in someone else's client-side switcher. Local inference is also the ZDR story
taken to its logical extreme: data on the local rung physically never leaves
the machine.

## Topology (the part that must not be fudged)

The **hosted** service cannot and must not reach into a user's LAN. Edge mode
is the **self-hosted** deployment of this AGPL router:

```
agent → edge router (user's box)
          ├── local models: llama.cpp / vLLM / Ollama  ($0, LAN, unmetered)
          └── cloud burst: hosted ZeroRouter /v1 (prepaid key, metered, ZDR)
                └── (or direct provider keys, user's choice)
```

Local traffic terminates on the user's own network. Cloud-bound traffic goes to
the hosted service exactly as any client's would. The hosted product is
unchanged; edge mode is packaging plus routing config for the open-source
router.

## Scope — Version A only (mechanical eligibility)

1. **Chat-completions outbound wire.** llama.cpp, vLLM, Ollama, and LM Studio
   all speak OpenAI chat completions. The same wire lets an edge router use the
   hosted ZeroRouter `/v1` as its metered upstream — one adapter serves both
   halves of the hybrid. Billing-grade usage extraction, same bar as the
   Responses and Anthropic wires.
2. **User-registered local candidates.** Config declares endpoint URL, model
   id, and capabilities (context length, tool support, modalities). The user
   declares what the local rung handles; the router never guesses.
3. **$0 rung in the `zero/*` ladder.** Zero-price candidates order first in
   cost-led tiers; existing candidate/fallback machinery handles
   local-down/overflow → cloud. Selection inputs are mechanical only: does the
   request fit context, need tools/modalities the candidate lacks, is the
   endpoint healthy.
4. **Metering seam: $0 routes skip reserve/settle.** Nothing to bill — no
   advisory lock, no reservation, no settlement on the hot path. Token usage is
   still recorded (asynchronously, off-path) so hybrid usage stays visible in
   one dashboard. Content-free discipline is identical on both lanes.

   **What "$0 route" means, precisely** (implemented as
   `api::free_lane_admissible`; this refines the looser wording above, which
   stage 3 found to be two different claims wearing one name):

   - **Every candidate in the route is free** — not the first, not the one
     expected to serve. The reservation is taken at ADMISSION, before anything
     is known about which rung will answer, so a route holding even one metered
     candidate must reserve for it. A fallback that dispatches to a paid
     upstream with no reservation behind it is paid inference delivered with
     nothing to charge it against, no exactly-once settle, and no record that
     anything was owed — the exact failure this seam exists to avoid.
   - **AND the tier sells at $0.** Candidate freeness is a claim about
     ZeroRouter's COST BASIS; the customer pays the owning TIER's sell rate
     whichever rung serves. Catalog validation forbids only a basis ABOVE the
     sell rate, so a $0 basis under a $3.00 tier is a legal, deliberate,
     100%-margin configuration. It is also what a MISSING CREDENTIAL produces:
     a candidate whose credential is absent is dropped from the route, so an
     unset cloud key collapses a mixed tier to an all-free route. Keying the
     skip on candidates alone would turn that deployment mistake into free
     paid-tier inference, silently, for as long as the variable stayed unset.

   Together these mean the skip engages only where the metered path would have
   priced `cost_usd = 0` and debited nothing, so **nothing a customer is
   charged changes on any route**.

   **The consequence, which is a scope statement and not a defect:** the
   latency win belongs to routes composed ENTIRELY of free rungs — an all-local
   tier, or a local model addressed as a pin. **The local-first/cloud-burst
   ladder keeps full metering on every request**, including the ones its local
   rung serves. Reserving at admission is what makes the burst billable, and
   the burst is the point of a hybrid ladder. Benchmark lane 1 must therefore
   be read as measuring the all-local shape, not the hybrid one.

   **Known limitation, deliberately unchanged:** the prepaid gate runs before
   the lane is chosen, so an account whose credit is fully encumbered, negative,
   or frozen is refused the free lane too. (A zero balance is NOT refused — the
   gate compares against the request's reserved cost, which on a $0 route is
   zero — so this lane is not a credit gate so much as an account-standing
   one.) Whether a wholly-free route should be servable regardless of account
   standing is a product decision, not an implementation detail, and is left
   open.
5. **Edge packaging.** Docker image + a documented config for
   router-on-your-box; a quickstart that goes from `docker run` to hybrid
   routing in minutes.

## Explicitly out of scope (the B-line)

**No quality prediction. No cascades. No judge models. No per-install
capability learning.** "Can the local model handle this?" is answered by user
config and mechanical constraints, never by the router's opinion of a model's
intelligence. Quality-aware routing is a research-grade problem (entire
companies exist for it) whose failure mode — silent quality degradation blamed
on the router — is the most trust-destroying behavior a gateway can have. Any
future crossing of this line gets its own design review; it does not creep in
through this feature.

## Security requirements for the free lane (audit treatment required)

The unmetered path is an attack surface against the money path. Hard rules:

- **Classification is server-side config only.** Whether a route is free is a
  property of the tier/candidate configuration on disk — never influenced by
  request contents, headers, model-id aliasing, or client hints.
- **No paid model may be reachable through the free lane *by accident*.** The
  skip predicate keys on server-side configuration at selection time, in one
  place (`api::free_lane_admissible`), and it takes TWO independent
  declarations in TWO files to enter the lane: the provider's
  `"settlement": "free"` and the candidate's $0 price. Neither a typo in a
  rate, nor picking the chat-completions adapter, nor a forgotten credential
  can produce one on its own.

  **What this does not achieve, stated plainly, because an earlier draft of
  this line overclaimed it.** A candidate with a nonzero price is *not*
  structurally incapable of reaching the lane, because nothing in the router
  can know what an arbitrary endpoint charges. `chat_completions` is dual-use
  by this document's own scope — it serves both the operator's local models and
  a hosted ZeroRouter taking metered burst traffic — and `settlement: free` on
  a credentialed `chat_completions` provider whose `base_url` points at a paid
  upstream will be believed. That is an operator lying to their own
  configuration, it is undetectable by any validation short of the invoice, and
  the harm lands on the operator who wrote it (their own bill, unrecorded)
  rather than on a customer or on ZeroRouter's ledger. The guarantee is the
  explicit double declaration, never inference.

  **Open question, flagged rather than closed:** whether a better discriminator
  than "the operator said so" exists. Keying on keyless-only was considered and
  rejected — it breaks the legitimate, common case of a local vLLM behind a
  bearer token — and no other candidate discriminator has been found that does
  not either break that case or amount to guessing. Tracked as an open finding;
  it is a hardening question, not a blocker, precisely because the failure mode
  is self-inflicted and self-absorbed.
- **Free-lane requests still authenticate.** Cached-key auth applies; the free
  rung is not an anonymous proxy.
- **Async usage recording must not become a billing input.** $0 usage rows are
  observability, and must be inert to settlement, autopay eligibility, and
  balance math. As implemented (`db::UsageSession::observe_free_usage`), that
  is structural rather than conventional: `cost_usd` is bound to a literal
  zero, so every spend aggregate sums it to nothing; autopay and balance math
  never read `usage_events` at all; and `task_signature` / `estimator_basis`
  are left NULL, which makes the row invisible to every estimator query. That
  last one is the sharp edge — the segment key omits the model, so a local
  rung's output lengths would otherwise train the percentiles that SIZE
  METERED RESERVATIONS.

  **One deliberate exception:** free usage DOES count toward the
  token-denominated velocity cap. The cap is abuse control, not accounting —
  free inference still burns the operator's own hardware — and counting is the
  safe direction to be wrong in, since it can refuse traffic but never
  authorize a charge. Without the per-user advisory lock the cap is
  approximate at the boundary (k simultaneous requests can each pass a check
  their sum would fail) and exact **over the rows that landed** — the next
  window sees every token of every row that was written, because
  `usage_events` is append-only. It is not exact over traffic served: the
  free-lane insert is warn-and-drop, so a row lost to a database blip or to
  the process dying is usage that was delivered and never counted against the
  cap. Both slippages are bounded and both cost the operator throughput rather
  than money, which is why the trade is acceptable on this lane and would not
  be on the metered one.
- The seam lands as its own PR with adversarial review, same bar as the
  reserve→settle work.

## Benchmark plan (two honest lanes)

Published comparisons against LiteLLM and Bifrost use the harness in
`benchmarks/` (migrating in from the workspace prototype), pinned versions,
one-command repro.

- **Lane 1 — free/local routes:** apples-to-apples stateless forwarding, the
  job LiteLLM and Bifrost actually do. Hot path after this design: cached auth
  → in-memory routing → transform → shared keep-alive clients. Target: overhead
  competitive with Bifrost (+0.43 ms measured), decisively ahead of LiteLLM
  (+3.30 ms); throughput without the per-user lock measured under a multi-user
  scenario. Targets are validated by measurement, never asserted.
- **Lane 2 — metered cloud routes:** presented as what it is — prepaid
  can't-overspend accounting that no competitor performs. Current measured
  overhead ~8 ms (≈half is metering). Not spun as proxy overhead.

Integrity items before any number is published: fix the harness so Bifrost
parses responses at full fidelity (the prototype's minimal body left
`choices: null`); add a multi-user metered-throughput scenario so the
single-key advisory-lock artifact (221 req/s) is not misrepresented as a
ceiling; document machine, configs, and caveats in the report.

**Benchmark gate:** the table is re-run after each stage (wire → seam →
packaging); regressions surface one commit after they land.

## Implementation order

1. Chat-completions wire (+ unit tests at the existing wires' bar)
2. Local-candidate config + $0 rung in the ladder
3. Metering seam ($0 skip) — **adversarial review gate**
4. Benchmark harness in-repo + integrity fixes; free-lane numbers
5. Edge packaging (docker + quickstart)
6. Announcement with the two-lane table

Each stage is a separate PR; nothing merges without green CI and, for stage 3,
an explicit review pass.

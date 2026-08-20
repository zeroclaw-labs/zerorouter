# Deploying ZeroRouter

## Infrastructure ownership

All Terraform for the live stack lives in
**`zeroclaw-labs/zeroclaw-infrastructure`**, under
`environments/zerorouter-beta`. That repository is the **sole IaC owner**:
VPC, ALB, ECS cluster/service, RDS, Secrets Manager containers, IAM, and the
GitHub-OIDC deploy role are all defined there and only there.

This repository ships exactly two deployment artifacts:

- the application image (the root `Dockerfile`: Rust router + built portal
  SPA, `linux/arm64`, distroless);
- the deploy workflow (`.github/workflows/deploy.yml`), which builds and
  pushes the image to the Terraform-owned ECR repository and rolls the
  Terraform-owned ECS service. The workflow discovers its ECR/ECS
  coordinates from the deploy role's inline IAM policy, so it carries no
  hardcoded account or resource names.

Do not add Terraform, task-definition JSON, or AWS resource names to this
repository.

## The live stack contract

The app is built to satisfy the `zerorouter-beta` environment as Terraform
defines it:

- **`ZEROROUTER_BIND=0.0.0.0:8080`** — the container listens on 8080
  (baked into the image as a default).
- **`GET /healthz`** is the ALB target-group health check (and the
  container `HEALTHCHECK`).
- **`ZEROROUTER_TIERS_PATH=/etc/zerorouter/tiers.toml`** — the image bakes
  the canonical `router/config/tiers.toml` at that path.
- **Database**: `DB_HOST`, `DB_NAME`, `DB_PORT`, `DB_USERNAME`,
  `DB_PASSWORD` plus `DB_SSL_ROOT_CERT`; this path always connects with
  `verify-full` TLS against the checksum-pinned RDS CA bundle shipped in
  the image at `/etc/zerorouter/rds-global-bundle.pem`.
- **Provider keys** are injected from AWS Secrets Manager via
  task-definition `secrets`, never plain env in Terraform or the task
  definition. The set the shipped `providers.json` can consume is whatever
  its entries' `credential_env` name — today `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `GEMINI_API_KEY`, and `BEDROCK_API_KEY`. A provider whose
  key is absent is simply not a candidate — set only what the catalog actually
  routes to.
- **`BEDROCK_REGION`** — plain env, not a secret; a region is not a credential.
  It is **required whenever `BEDROCK_API_KEY` is set**, because the Bedrock
  endpoint carries the region in its hostname
  (`bedrock-mantle.{region}.api.aws`). Unset, the Bedrock rungs drop out of
  every route exactly as a missing key would, and the rest of the catalog keeps
  serving; `/v1/models` still lists them, as it does for any credential-less
  lane. No region is defaulted on purpose: `us-east-1` is the plausible guess
  that would silently route an eu-west-1 deployment's prompts to Virginia.

  **Not every region serves every model.** The mantle endpoint exists in a
  subset of regions, and in-region Claude availability is narrower still —
  `anthropic.claude-sonnet-5` in-region is us-east-1, eu-west-1, and
  eu-north-1 (plus us-gov-west-1, priced 1.2x rather than 1.1x). Pointing
  `BEDROCK_REGION` at a mantle region that does not serve the model — us-west-2,
  say — produces failing calls rather than a fallback, because mantle is
  in-region only and cannot reach a cross-region inference profile. Beta runs
  us-east-1.
- **Platform**: ARM64 on Fargate; the workflow builds `linux/arm64` only.

The deploy workflow re-registers the task-definition family's latest ACTIVE
revision with only the image swapped, so Terraform-authored env/secret/role
changes are picked up on the next deploy without workflow edits.

> **Note — secret rotation:** ECS resolves Secrets Manager references **at
> task start**. Rotating a secret does nothing to running tasks; run the
> deploy workflow (or `aws ecs update-service --force-new-deployment`) to
> pick up rotated values.

## Cutover checklist: old `zerorouter-ts` repo → this repo

The beta environment currently deploys from the old TypeScript-era
repository, now `zeroclaw-labs/zerorouter-ts` (it held the `zerorouter`
name until this repository took it over). To cut it over:

1. **Infrastructure repo** (`zeroclaw-labs/zeroclaw-infrastructure`,
   `environments/zerorouter-beta`):
   - point the `sources/zerorouter` submodule at this repository;
   - extend the GitHub-OIDC deploy-role trust policy to accept
     `repo:zeroclaw-labs/zerorouter:ref:refs/heads/main` as a subject
     (keep or drop the old repo's subject as desired — the trust policy is
     the only thing binding "who may deploy").
2. **This repo**: set the repository variable `AWS_DEPLOY_ROLE_ARN` to the
   deploy role's ARN. The workflow validates the ARN shape and refuses to
   run without it.
3. **Task definition (Terraform)** — add the new web-plane configuration:
   - plain environment: `ZEROROUTER_PUBLIC_BASE_URL`,
     `ZEROROUTER_REQUIRE_CREDITS`, `ZEROROUTER_SIGNUP_CREDIT_USD`;
     **`ZEROROUTER_REQUIRE_CREDITS` now defaults to `true`** — see
     [Credit enforcement is on by default](#credit-enforcement-is-on-by-default)
     below before deploying a task definition that omits it;
   - new Secrets Manager containers, following the existing
     `<name>/providers/<secret>` naming convention, wired as task-definition
     `secrets`: `OIDC_CLIENT_SECRET`, `STRIPE_SECRET_KEY`,
     `STRIPE_WEBHOOK_SECRET`. (`OIDC_ISSUER_URL` and `OIDC_CLIENT_ID` are
     not secret and may be plain env; remember the OIDC group is
     all-or-nothing — a partial group aborts the task at startup, which the
     circuit breaker will surface as a rollback.)
   - **`STRIPE_PUBLISHABLE_KEY` (`pk_...`) — required, and new.** The Stripe
     group is all-or-nothing like the OIDC one, so a task definition that
     carries the secret key and the webhook secret but not this one **aborts
     at startup** and the circuit breaker rolls the deployment back. It is
     not a secret (the portal serves it to every signed-in browser over
     `/api/me`, and Stripe.js sends it from the client), so plain env is
     fine — but it IS environment-specific: use the sandbox `pk_test_...` in
     a sandbox and the live `pk_live_...` in production, matching whichever
     `STRIPE_SECRET_KEY` is set. A mismatched pair fails when a customer
     tries to pay, not at startup.
4. **Deploy**: run the `Deploy Router` workflow manually, or merge to
   `main` (the workflow triggers on push to `main`). Verify the run's
   deployment summary and that
   ECS stabilized on the requested task definition **without a
   circuit-breaker rollback** — the workflow fails loudly if the PRIMARY
   deployment did not complete.

> ## ⚠️ The beta ALB cannot receive Stripe webhooks or OIDC redirects yet
>
> The beta ALB listener is **HTTP on port 80** with a **/32 source-IP
> allowlist**. Stripe requires a publicly reachable **HTTPS** endpoint for
> webhooks, and any real IdP will refuse a plain-HTTP redirect URI on a
> non-loopback host. Until the environment gains a domain, an ACM
> certificate, and an HTTPS listener — with `/webhooks/stripe` reachable
> from Stripe's IP ranges — production-shaped billing/login **cannot work
> against the beta ALB**.
>
> Interim setup for beta testing:
>
> - **Stripe**: run `stripe listen --forward-to <allowlisted
>   address>/webhooks/stripe` from an allowlisted machine and use the CLI's
>   signing secret as `STRIPE_WEBHOOK_SECRET`.
> - **OIDC**: register the IdP redirect URI against the allowlisted
>   HTTP address (IdPs that permit `http://` redirect URIs only for
>   development tenants; use a dev tenant).
> - Set `ZEROROUTER_PUBLIC_BASE_URL` to that same allowlisted address so
>   generated URLs match. Note that on `http://` origins session cookies are
>   issued without the `Secure` attribute — acceptable for the allowlisted
>   beta only.
>
> Treat the HTTPS listener as a blocker for any external user traffic.

## Checkout is pinned to a Stripe API version

The two checkout calls — creating a Checkout Session and reading one back for
the return page — send `Stripe-Version: 2026-03-25.dahlia` explicitly. **Do not
remove that pin, and do not assume the account's dashboard version can satisfy
it.**

The embedded form is requested with `ui_mode=embedded_page`. That enum value
does not exist before Dahlia: the release renamed `hosted`/`embedded`/`custom`
to `hosted_page`/`embedded_page`/`elements`, and the changelog marks it a
breaking change. An unpinned request runs at whatever version the *account*
defaults to, so on an account created before Dahlia, Stripe rejects the session
outright — `POST /api/billing/checkout` returns 502 `checkout_failed` and
**nobody can buy credits**. It fails on the first real purchase, not at startup.

Two things make this easy to miss:

- **A green sandbox does not prove live works.** A sandbox defaults to the API
  version current when the sandbox was created, so a recently made sandbox
  silently passes while an older live account fails.
- **The client is already on Dahlia.** The portal loads Stripe.js from the
  `dahlia` bundle and calls `createEmbeddedCheckoutPage` (itself a Dahlia
  rename). Stripe's guidance is to keep Stripe.js and the server-side API
  version on the same release train, which the pin does.

Scope is deliberate: **only the two checkout calls are pinned.** The autopay
paths (PaymentIntents, Customers, the setup-mode session) keep the account
default, because they send nothing version-sensitive and Dahlia carries
breaking Payments changes that this integration has not audited. Upgrading the
account's default API version is therefore safe for checkout — it is pinned —
but is still an autopay decision, not a checkout one.

Webhook payloads are unaffected either way: Stripe renders events at the
account's (or the endpoint's) configured version, not at the version of the
request that created the object.

## Autopay: what deployment must provide

Autopay ships enabled in the binary — there is no feature flag. What it
needs from the environment:

- **The same Stripe secrets as checkout** (`STRIPE_SECRET_KEY`,
  `STRIPE_WEBHOOK_SECRET`). No new variables. (`STRIPE_API_BASE` exists as
  an override for pointing the client at a mock in tests; leave it unset in
  any real deployment.)
- **Webhook event subscriptions.** The Stripe webhook endpoint must be
  subscribed to **`payment_intent.succeeded`** and
  **`payment_intent.payment_failed`** in addition to the checkout events
  (`checkout.session.completed`,
  `checkout.session.async_payment_succeeded`). Without the payment-intent
  pair, autopay charges settle only through the 30-minute reconciliation
  sweep — credits still arrive exactly once, just late.
- **`charge.dispute.created` and `charge.refunded` (migration 0009).**
  These are not optional and they are not late-tolerant. A dispute freezes
  the account and reverses the credit; a refund reverses the credit. An
  endpoint that is not subscribed to them silently keeps the pre-0009
  behavior — a customer can charge back at Stripe and keep spending — with
  nothing in the logs to say so, because the events never arrive. **Check
  the subscription list when deploying this change**, and re-check it after
  any endpoint is recreated.
- **A single serving deployment per database** is assumed by the sweep's
  claim rows (one pending intent per user); the ECS service already runs
  one task. Scaling out is safe for correctness (claims are DB-enforced)
  but will multiply Stripe list/read traffic.

The sweep itself (charge candidates, reconcile stale intents, three-strikes
disable) starts with serve mode and needs no configuration.

## Abandoned checkout intents are deleted after 30 days (migration 0022)

A second, unrelated sweep starts with serve mode and also needs no
configuration: hourly, it deletes `stripe_checkout_intents` rows for Checkout
Sessions that were created, never paid, and are more than 30 days old. Most
Checkout Sessions are never paid — a customer who opens the payment modal,
closes it, and reopens it leaves a row behind each time — so without this the
table only grows.

**This is a data-retention change, so it is stated rather than left to be
discovered.** What it can never delete is the half that matters: a row whose
session was credited is corroboration for a `credit_ledger` purchase entry and
is permanent, and that is enforced by the database (the migration narrows
0005's DELETE prohibition rather than lifting it — a settled row, a
ledger-referenced row, and any row less than seven days old are all refused by
a trigger, independently of the sweeping query).

Two consequences worth knowing before it runs:

- **A customer returning to a checkout tab more than 30 days later** sees "We
  could not confirm that payment just now" instead of "that checkout expired",
  because the row the status endpoint looks them up in is gone. Safe by
  construction — a deleted row was never credited, so this can never hide a
  purchase that landed — but it is a deliberate trade of a precise sentence for
  a bounded table.
- **Reconciling a payment that was collected at Stripe but never credited**
  loses its local handle after 30 days. Stripe keeps the record: the session,
  its `metadata[user_id]` and `metadata[credit_usd]`, and the failed webhook
  delivery are all in the dashboard, which is where that reconciliation
  already starts. The retention window is far outside anything Stripe can do
  on its own (24h of session life plus three days of webhook retries), so a
  row reaching 30 days unpaid and uncredited has already exhausted every
  automatic path.

The sweep is bounded to 256 rows per pass and takes an advisory lock, so
scaling out does not multiply the work.

## Stripe Tax must be configured BEFORE this deploys

Checkout Sessions are created with `automatic_tax[enabled]=true` and
`tax_id_collection[enabled]=true`, and nothing else. The code sends no rate, no
jurisdiction, **no product tax code, and no
tax behavior** — all of it comes from Tax Settings, deliberately, so the
operator can revise a contested tax classification without a deploy. The
dashboard is therefore not optional configuration around this feature, it
**is** the feature. No environment variable and no code path checks any of
it, and no test can: the tests prove one parameter is sent, not that Stripe
was configured to act on it.

Three distinct things go wrong when it is missing, and only one is loud:

1. **Stripe Tax not activated on the account.** Stripe rejects the session
   creation (`stripe_tax_inactive`), so `POST /api/billing/checkout`
   returns 502 `checkout_failed` and **nobody can buy credits at all**.
   This is immediate and total on deploy — activate first.
2. **Default tax behavior left as `Inclusive`.** Stripe carves the tax out
   of ZeroRouter's price instead of adding it on top, so every purchase
   reaches the webhook as a short payment and **credits nothing**: the
   customer is charged, no balance appears, and `amount_mismatch` piles up
   in Stripe's webhook dashboard. The webhook is behaving correctly — it
   refuses to credit against money that did not arrive — but the effect is
   a total checkout outage with money moving. Since the request no longer
   pins the behavior, this setting is now the only thing preventing it.
3. **Activated, but no registration covering the buyer.** Stripe accepts
   the session and calculates zero tax. Checkout works, credits land, and
   nothing in the logs says tax is not being collected. This is the quiet
   failure, and Stripe cannot retroactively correct a sale that collected
   the wrong tax — so registrations must exist before the first live
   purchase, not after the first complaint.

Operator steps, in order, in **each** environment (a sandbox's tax
registrations do not carry to live mode; Tax Settings must be verified per
environment):

1. Dashboard → **Tax → Settings**: activate Stripe Tax and set the head
   office address (Cambridge, MA). `automatic_tax` calculates nothing while
   the settings status is `pending`.
2. Dashboard → **Tax → Settings → Include tax in prices**: set the default
   tax behavior to **Exclusive**. (`Automatic` is equivalent today — it
   resolves to exclusive for USD and CAD — but `Exclusive` stays correct if
   a second currency is ever priced.) **Do not leave this on `Inclusive`;
   see failure mode 2 above.** ZeroRouter's ToS says prices are exclusive of
   taxes, and the deposit-fee margin assumes the gross arrives intact.
3. Dashboard → **Tax → Settings → preset product tax code**: set it. The
   recommended starting selection is **`txcd_10105001`** (AIaaS – Cloud
   Based – Personal Use). The reasoning, the alternatives, and the
   **unresolved question of whether tax is due when credits are bought or
   when they are spent** are written up in the `# Sales tax` section of
   `router/src/stripe.rs`. That question is an open item for the operator's
   accountant — Massachusetts DOR issues letter rulings for exactly this
   situation — and nothing in the code settles it. Because the code no
   longer sends a tax code, changing this selection later is a dashboard
   edit with no deploy.
4. Dashboard → **Tax → Registrations**: add the Massachusetts registration
   and confirm it shows as *Collecting*. With a head office in
   Massachusetts the business is not a remote seller, so this registration
   is required on physical presence, not on a sales threshold.
5. Run one real purchase and confirm the session carries a non-zero tax
   line and that the credit still lands. A green test suite is not
   evidence.

### Tax IDs and reverse charge — what entering a VAT number does and does not do

The checkout form offers business buyers an **optional** VAT/tax-ID field
(`tax_id_collection[enabled]=true`; `required` is deliberately left at its
default `never`, because making it mandatory would stop EU consumers buying at
all). The purpose is **reverse charge**: on a cross-border B2B sale of services
into the EU or UK, a VAT-registered buyer accounts for the VAT themselves, the
seller collects zero, and the invoice must cite the buyer's VAT number.

**Reverse charge only shows up where you are registered.** Stripe applies it
against your *registrations*, and it already calculates zero tax for any
jurisdiction you are not registered in (failure mode 3 above). So with only the
Massachusetts registration in place:

- An **EU or UK buyer** collects **zero tax either way** — with or without a VAT
  number. The field is collected and recorded; the tax was already zero. Adding
  an EU OSS or UK VAT registration is what makes the distinction real, and at
  that point entering a VAT number is what stops a business being charged
  consumer VAT.
- A **US buyer** is taxed exactly as before. Reverse charge is a VAT mechanism
  and US sales tax has no equivalent, so a US business entering an EIN changes
  nothing about that sale today. It is collected so the buyer can self-identify
  for business-use treatment later.

Do not read "the buyer entered a tax ID" as "the tax changed". Those are
independent facts and only the registration list connects them.

**The tax ID is not stored in ZeroRouter.** Reverse-charge invoices must cite
it, but Stripe already holds it and no migration was added to duplicate it. To
retrieve one: `stripe checkout sessions retrieve <session_id>` and read
`customer_details.tax_ids[]`, or Dashboard → Payments → the session. For a VAT
return, Tax → Registrations → reports break out reverse-charged transactions
with the buyer's tax ID per row, alongside the rest of the filing figures. If
the accountant ever needs the ID inside ZeroRouter's own books rather than at
filing time, that is a migration and a deliberate decision.

Two consequences worth planning for:

- **Stripe Tax costs roughly 0.5% per transaction** where a registration
  applies, on top of card processing. The deposit fee has not been re-sized
  for it; the arithmetic is in the `DEPOSIT_FEE_FLOOR_USD` comment. Autopay
  now also pays for one tax *calculation* per top-up attempt (Stripe bills
  per calculation call), which a reconciliation replay does not repeat.

### Autopay is taxed too (migration 0021)

Autopay top-ups used to collect no tax at all, so the same credits bought two
ways collected two different amounts. They no longer do. A raw PaymentIntent
still takes no `automatic_tax` parameter, so autopay prices tax with the **Tax
Calculation API** and charges `gross + tax`, then records a tax transaction so
the sale reaches the filing report. The reasoning — including why the Invoices
route and Stripe's newer `hooks[inputs][tax][calculation]` PaymentIntent link
were both rejected — is in the autopay section of `router/src/stripe.rs`.

**Nothing here needs new configuration.** It uses the same Tax Settings the
checkout path does: no tax code and no tax behavior is sent, so the preset
governs both surfaces and they cannot drift into taxing the same product two
different ways. The five operator steps above are the whole setup.

**This ships inert, and that is expected.** With no tax registrations Stripe
calculates zero tax for every buyer, so today every autopay charge is priced,
comes back zero, and collects exactly what it collected before. The change
becomes visible the day a registration goes live — which is the point: it
means the first taxed autopay charge does not require a deploy.

Three things worth knowing when it stops being inert:

- **The buyer's location comes from the saved card's billing address**, and
  nowhere else. `ensure_stripe_customer` stores no address on the Stripe
  Customer, and the Tax API does not fall back to any other source. If Stripe
  captured no billing address when the card was saved, or the address cannot
  be rated (a US address needs a postal code), the top-up is charged
  **untaxed** rather than failing. That is deliberate: a degraded top-up beats
  a dead one. Every such charge logs at WARN with the field
  **`autopay_tax_fallback`**, whose value is one of `no_billing_address`,
  `incomplete_address`, `calculation_rejected`, `calculation_unavailable`.
  **Alert on that field** — it is the only signal that autopay is collecting
  no tax where it should. To fix `no_billing_address` at the source, turn on
  billing-address collection for the card-setup session in Stripe's checkout
  settings; existing saved cards need the customer to re-add the card.
- **Tax reversals on a refund are NOT automatic.** Stripe reverses tax
  automatically only for Checkout and for its own simplified PaymentIntent
  link, neither of which this path uses. A refunded or disputed autopay charge
  leaves its tax transaction standing, so the tax must be reversed by hand in
  Dashboard → Tax → Transactions. The same is true of an autopay charge whose
  credit was **withheld** (collected from a frozen or indebted account): no tax
  transaction is recorded for it, and the operator refund must be the **taxed**
  total — `withheld_autopay_intents` reports that figure, not the ex-tax gross.
- **A tax transaction that fails to record does not fail the charge.** The
  money is already correct by then, so the failure is logged at ERROR naming
  the `payment_intent` and the `tax_calculation`, and the sale is missing from
  the filing report until an operator creates the transaction from that
  calculation. Search the logs for `tax transaction was not recorded` before
  filing.

The ledger is unchanged by all of this: the buyer is credited exactly the
top-up, the recorded charge stays the ex-tax gross so fee revenue is still
`charge - credit`, and tax lives in its own column — never credited, never
counted as revenue.

**Rollout ordering.** During a deploy the old and new binaries can both be
processing webhooks. A pre-0021 intent (no tax metadata) is credited correctly
by the new binary. A 0021 intent carrying real tax would be read as a short
payment by an *old* binary and refused with `amount_mismatch` — Stripe retries
for days, so it credits once the rollout completes. With no registrations the
tax is zero and the two shapes are numerically identical, so this is a
non-event today; it matters only if a registration is added mid-deploy.

## Credit enforcement is on by default

`ZEROROUTER_REQUIRE_CREDITS` **defaults to `true`**. It previously defaulted
to `false`, so this is a deliberate behavior change for any deployment that
left the variable unset.

**Why it changed.** Credits are the only ceiling backed by money. With
enforcement off, nothing verifies that spend is funded: the per-key and
derived per-user spend/velocity caps on `api_keys` are the sole limit on what
a user can consume, and those caps are **self-service** — the portal lets a
user raise a key's own `spend_cap_usd`. A deployment that never set the
variable was therefore running with no enforced ceiling at all, which is not
a state anyone chooses on purpose. Unconfigured now lands on the safe side.

| value | behavior |
|---|---|
| unset, or set to a blank/whitespace string | credits **required** (the default) |
| `true` / `1` | credits required |
| `false` / `0` | cap-only; logs a startup warning naming what it gives up |
| anything else | **startup aborts** — never a silent fallback in either direction |

**Opting out.** Cap-only remains a supported shape for self-hosted
deployments that deliberately run without billing. Set
`ZEROROUTER_REQUIRE_CREDITS=false` (or `0`) explicitly. Do this knowing the
only remaining ceiling is a cap the user can raise themselves.

**Before deploying.** A task definition that omits the variable now runs with
credits required, so inference is refused for users with no funded balance.
Either fund balances / set `ZEROROUTER_SIGNUP_CREDIT_USD`, or set
`ZEROROUTER_REQUIRE_CREDITS=false` explicitly if cap-only is what you want.

## Retention posture: how to change a label, and when you may

ZeroRouter's catalog labels **every** lane with what its upstream does with a
request after answering it, and `/v1/models` lists zero-retention lanes first.
The labels are pinned in `router/config/tiers.toml` under `[retention.<provider>]`
and are never written by any tool — the same rule prices follow, for a sharper
reason: a retention label is a claim to a customer about their own data.

**Today two lanes are `zero` and the rest are `standard`.** `anthropic`,
`openai`, and `google` are ordinary API accounts. `bedrock` — the two
`bedrock/claude-*` lanes, added 2026-08-20 — is the first zero-retention
upstream, and it got there by configuration rather than by contract. The section
below on enforced configuration is why that counts.

### The rule for `posture = "zero"`

> A lane may be labelled zero-retention **only when a signed or confirmed
> zero-data-retention arrangement is in force with that provider, covering the
> account that lane dispatches on** — or when the provider **enforces** zero
> retention as a setting on that account, with published semantics for what the
> setting means.

Not because the vendor *offers* ZDR to somebody. Not because a policy page says
data is not used for training — **training and retention are different claims**,
and all three standard providers disclaim training while still retaining. When
in doubt, write `standard`. A wrong `standard` costs a little marketing; a wrong
`zero` is a false statement to a customer about their data, and the kind of
claim a regulator or a plaintiff reads literally.

#### Enforced configuration, and why it satisfies the rule

The rule was written for contracts, because that is how every major vendor sold
ZDR when it was written. Bedrock does it differently, and the difference is in
our favour rather than a loophole in it.

AWS exposes `data_retention_mode` as an account-level (or project-level) setting
with four values, and ZeroRouter's account is set to `none` on both control
planes. AWS publishes what that value means:

> No request or response data is written to durable storage by AWS or shared
> with the model provider… Chat Completions and Messages requests are never
> retained.

and what its scope is:

> the setting is enforced consistently across the Messages, Chat Completions,
> and Responses APIs

**A setting the platform enforces on every request is stronger evidence than a
contract, not weaker.** A contract is a promise a human honours; this is a
control that cannot be overridden per call. AWS also documents the failure
direction as closed: a model that *requires* retention is **blocked** under this
mode rather than silently downgraded, so a lane that could not honour the claim
returns an error instead of quietly retaining.

Two conditions come with accepting configuration as evidence, and both are
load-bearing:

1. **The setting must be verified live, not assumed.** A contract cannot be
   turned off by someone clicking through a console; a setting can. That is what
   `--bedrock-live` below exists for, and it is why the Bedrock pin is the only
   one with two re-verification steps instead of one.
2. **The published semantics must be pinned like any other evidence.** The
   `source_url` for a configuration-backed pin is the page defining what the
   value means, so `retention-drift` catches AWS *rewording* the guarantee. It
   is the same loop as every other pin, over a different kind of claim.

`inherit`, not `default`, is the value a never-configured AWS account reports —
it means "no opinion at this scope". Only a literal `none` backs a `zero` label.

The one exception that needs no vendor at all: a **local rung on your own
hardware** (see `examples/edge/tiers.toml`). Even there, confirm your inference
server is not writing prompts to a request log before you label it — several do
by default.

### Changing a posture

1. **Re-verify first.** Open the provider's policy page and read what it now
   says. If the posture is changing to `zero`, confirm the arrangement is
   actually executed — an email saying "we can offer that" is not an
   arrangement.
2. **Edit the pin** in `router/config/tiers.toml`: `posture`, `description`,
   `source_url`, and `verified` (today's date). Keep the description
   qualitative when the vendor publishes no window — Google's terms say prompts
   are logged "for a limited period of time" and state no number, so ours does
   not invent one.
3. **Re-pin the digest.** Run the drift check; it prints the digest it observed:

   ```bash
   cd router
   ./target/debug/zerorouter admin retention-drift --tiers config/tiers.toml
   ```

   Copy the `observed source_sha256` into the pin. Copy it only *after* step 1
   — pasting the new digest without reading the page is the one way to misuse
   this tool, and it converts the check into a rubber stamp.
4. **Confirm green.** Re-run the command; it should report every page unchanged
   and exit zero.

A tier that needs its own posture (one lane bought under a separate agreement)
declares a complete `[tiers."<id>".retention]` block. It **replaces** the
provider pin rather than patching it, so an overriding tier states its own
evidence and its own date.

### What the drift check does and does not mean

`admin retention-drift` fetches each pinned `source_url`, reduces it to visible
text, and compares the SHA-256 against the pin. It **never** compares postures —
no public source states what your contract with a provider says.

| verdict | meaning | exit |
|---|---|---|
| `UNCHANGED` | the page still reads as it did on `verified` | 0 |
| `PAGE CHANGED` | the wording moved — **a human must re-read it** | non-zero |
| `UNREACHABLE` | the page could not be fetched, so the claim has no re-verification loop | non-zero |

**A changed page does not mean the posture flipped.** It usually means the
vendor reworded or relaid-out something. The loop is: alert on change → a human
re-verifies → bump `verified` and `source_sha256`. `--allow-drift` reports and
exits zero when you need to unblock; `--source-dir` reads pages from disk for a
deterministic CI fixture.

`--corroborate` adds OpenRouter's provider directory as a second opinion. It is
**advisory and cannot change the exit code**, and it is doubly indirect: it
describes *OpenRouter's* account with each provider, so a private ZDR
arrangement of yours is invisible to it. Expect a `zero` pin to look like a
disagreement there. Note also that `google` corroborates against
`google-ai-studio`, not `google-vertex` — different products, different
policies, and the slug is pinned explicitly in the file for exactly that reason.
`bedrock` has the same trap: it joins `amazon-bedrock`, **not** `claude-on-aws`,
which is Anthropic's own managed capacity on AWS and reports 30-day retention.

### The live half of the Bedrock claim

The page hash cannot see the account. It catches AWS rewording what `none`
means; it cannot catch someone flipping the account to `default`, and after that
flip every check above still passes while `/v1/models` keeps telling customers
their prompts are never stored. So the Bedrock posture has a second check:

```bash
cd router
BEDROCK_API_KEY=... BEDROCK_REGION=us-east-1 \
  ./target/debug/zerorouter admin retention-drift \
    --tiers config/tiers.toml --bedrock-live
```

It calls `GET https://bedrock-mantle.$BEDROCK_REGION.api.aws/v1/data_retention`
(note the underscore — the classic control plane spells it `/data-retention`)
and expects `{"mode":"none"}`. Run **both** halves before re-pinning a Bedrock
`verified` date.

Three deliberate differences from `--corroborate`:

- **It is not advisory.** It reads ZeroRouter's own account, not a third party's
  opinion, so when asked for it decides the exit code.
- **It is opt-in** so the daily CI job, which holds no AWS credentials, stays
  deterministic and green without them.
- **`--allow-drift` does not cover it.** That flag means "the evidence moved and
  I accept that for now", which is a defensible call about a reworded page. It
  is not a defensible call about an account that reports it is retaining while
  the catalog publishes that it is not — fix the account or change the pin.

Asking for the check and being unable to run it (credential unset, rotated, or
AWS unreachable) is a **failure**, not a pass: a check that could not run has
not verified anything.

### If a provider's posture actually changes

Raise `standard` first. A lane labelled `standard` that is really zero costs
nothing but a missed selling point; a lane labelled `zero` that is really
standard is the failure this whole mechanism exists to prevent.

## Bedrock: confirm the billing SKU after the first real request

The two `bedrock/claude-*` lanes are pinned at AWS's **in-region** rates —
Sonnet 5 at 2.20/0.22/11.00 and Opus 5 at 5.50/0.55/27.50 per MTok, 10% above
the `anthropic/*` lanes serving the same weights. That premium is correct and
priced straight through: the mantle endpoint is in-region only, and AWS charges
in-region traffic 10% more than a global cross-region inference profile. Do not
"correct" it downward — that sells the lane below what AWS invoices, on every
token.

**One step of that reasoning is an inference and should be closed empirically.**
AWS publishes exactly two on-demand SKU classes per model — `_standard` and
`_global_standard` — but no sentence states which one a mantle call meters
against. It is forced by elimination (mantle cannot use a global inference
profile), and the invoice is what settles it. After the first real Bedrock
request, open the Cost and Usage Report and read the line item:

- `usagetype` ending `_input_tokens_standard-Units` — as pinned, nothing to do.
- `usagetype` ending `_input_tokens_global_standard-Units` — the pins are 10%
  **high**, and the lane is selling above cost. Correct both basis and sell.

These two tiers are also the only ones `admin catalog-drift` does not reconcile,
so nothing in CI will catch an AWS price move on them. The exemption, its
reasoning, and the re-verification command are declared in
`router/config/providers.json` under `unreconcilable_reason`, and printed on
every drift run. Re-read it by hand when AWS changes anything:

```bash
curl -s --compressed \
  https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/\
AmazonBedrockFoundationModels/current/us-east-1/index.json
```

## Rollback

The workflow deploys immutable per-commit image tags. To roll back, re-run
the deploy workflow from the last good commit (`workflow_dispatch` checks
out and verifies the ref before deploying); ECS's deployment circuit breaker
also rolls back automatically if new tasks fail to stabilize.

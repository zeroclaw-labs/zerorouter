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
  `OPENAI_API_KEY`, `GEMINI_API_KEY`, `BEDROCK_API_KEY`, `FIREWORKS_API_KEY`,
  `XAI_API_KEY`, and `VERTEX_SERVICE_ACCOUNT`. A provider whose
  key is absent is simply not a candidate — set only what the catalog actually
  routes to.
- **`VERTEX_SERVICE_ACCOUNT` is not an API key**, and it is the one credential
  in this list that is not a string the upstream accepts. It holds a Google
  **service-account key in JSON** — the whole blob, including an RSA private
  key — because Google issues no long-lived key for the surface the Vertex lane
  dispatches on ("Only Google Cloud Auth is supported using the OpenAI
  library"). ZeroRouter signs a JWT with it and exchanges that for a one-hour
  OAuth2 access token, cached in-process and refreshed five minutes before
  expiry (`router/src/gcp_auth.rs`). Two consequences for an operator:
  - The secret is **larger and more sensitive** than the others. Treat a leak of
    it as a compromise of the service account, and rotate by creating a new key
    in GCP and deleting the old one — not by editing the JSON.
  - The value may be either the JSON itself or a **path to a file** containing
    it; ZeroRouter tells them apart by whether the value starts with `{`. ECS
    injects the JSON inline; the path form is for running locally against a
    downloaded key.
- **`VERTEX_PROJECT_ID`** — plain env, not a secret; a project id is not a
  credential. It is **required whenever `VERTEX_SERVICE_ACCOUNT` is set**,
  because the endpoint carries the project in its path
  (`.../v1/projects/{project}/locations/global/endpoints/openapi/...`). Unset,
  the Vertex rungs drop out of every route exactly as a missing key would.
  Nothing is defaulted, and here that matters more than it does for a region: a
  project is an **account boundary**, and the zero-retention configuration
  (below) is applied per project. A guessed project would serve requests under
  a posture nobody verified.

  Note the endpoint is pinned to `locations/global` deliberately. Google prices
  non-global endpoints 10% above global for these models, and `tiers.toml`
  records the global rates as this lane's cost basis — so switching to a
  regional endpoint without repricing sells every Vertex token below cost. See
  the Vertex section of `router/config/tiers.toml`.
- **`BEDROCK_REGION`** — plain env, not a secret; a region is not a credential.
  It is **required whenever `BEDROCK_API_KEY` is set**, because BOTH Bedrock
  endpoints carry the region in their hostname
  (`bedrock-mantle.{region}.api.aws` and
  `bedrock-runtime.{region}.amazonaws.com`). Unset, the Bedrock rungs drop out
  of every route exactly as a missing key would, they disappear from
  `/v1/models` by the same filter, and the rest of the catalog keeps serving.
  No region is defaulted on purpose: `us-east-1` is the plausible guess that
  would silently route an eu-west-1 deployment's prompts to Virginia.

  **Not every region serves every model, and the two planes differ.** The
  mantle endpoint exists in a subset of regions and is in-region only, so it
  cannot reach a cross-region inference profile: `anthropic.claude-sonnet-5`
  in-region is us-east-1, eu-west-1, and eu-north-1 (plus us-gov-west-1, priced
  1.2x rather than 1.1x). The classic runtime plane the shipped lanes actually
  dispatch on is broader, because those lanes name `us.`-prefixed GEOGRAPHIC
  inference profiles, which route within the US geography rather than pinning
  one region — but `BEDROCK_REGION` still selects the source region the call is
  made from and priced in, so it must be a US region for a `us.` profile. Beta
  runs us-east-1.
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
- **The card-setup session is card-only, and deliberately ignores the
  Dashboard's payment-method list.** It sends `payment_method_types[]=card`,
  because the off-session charge can only ever charge a card — it lists the
  customer's payment methods with `type=card` and fails the attempt when there
  is none. Enabling a further payment method in the Dashboard therefore will
  NOT make it appear on the autopay card-setup form, and that is the intended
  behavior: a method a customer could save but the sweep could not charge would
  look like a successful setup and then burn one of the three strikes on every
  pass until autopay disabled itself. Apple Pay is unaffected — Stripe persists
  an Apple Pay enrolment as a `card`. If autopay should ever accept a genuinely
  non-card method, the charge path has to learn it first.

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
  no tax where it should.

  **The card-setup session now requires a full billing address**
  (`billing_address_collection=required`, the same parameter the manual
  checkout has sent since the California registration). This needs no
  configuration and replaces the Stripe Dashboard toggle this section used to
  recommend — the guarantee is in the code, versioned with it, and cannot be
  switched off in a dashboard. It also removes the reason the fallback was the
  common case rather than the rare one: a `setup`-mode session carries no
  `automatic_tax`, so Stripe's `auto` default had nothing to raise its minimum
  above whatever the card network wanted, which for a US card is a postal code
  or nothing.

  **Cards saved before this change are not retroactively fixed.** Stripe cannot
  add an address to a payment method after the fact, so an existing card keeps
  whatever it was saved with; the customer has to re-add it (portal → Credits →
  Autopay → **Save or replace card**). Expect `autopay_tax_fallback` to keep
  firing for those users until they do, and to stop appearing for cards saved
  from here on. Autopay also now keeps the card's address on the user's row
  when nothing is stored there yet, so an autopay-only account — one that never
  runs a manual checkout — finally has a location for the redemption-tax
  surface below. It never overwrites an address a checkout stored: a checkout
  address is a form the buyer completed, a card address is a byproduct, and
  letting the coarser one win would degrade every future rating.
- **Tax reversals on a refund are automatic (migration 0024).** Stripe
  reverses tax on its own only for Checkout and for its simplified
  PaymentIntent link, neither of which this path uses — so the autopay sweep
  does it instead: when the ledger shows a charge's credit reversed (a refund
  or covering chargeback) and its tax transaction id is stored, the sweep
  records a **full tax reversal** and stamps the row. The one case that stays
  manual is a row whose transaction id was never stored (rows settled before
  migration 0024, or an id lost to a network failure): the Tax API cannot look
  a transaction up by reference, so those are surfaced at ERROR on every sweep
  pass until an operator reverses them in Dashboard → Tax → Transactions.
  **Reversing at Stripe does not quiet the log by itself** — ZeroRouter's row
  still says unreversed, so close the loop by stamping it:

  ```sql
  UPDATE stripe_autopay_intents SET tax_reversed_at = NOW()
  WHERE payment_intent_id = 'pi_...';   -- after reversing it at Stripe
  ```

  The same applies to a recording that can never complete automatically (the
  transaction exists at Stripe but its id was lost, so every retry is refused
  as a duplicate — or the calculation passed its 90-day expiry): resolve it at
  Stripe first, then stamp `tax_recorded_at` the same way. Stamp only what
  Stripe's dashboard confirms is done; the stamp is the sweep's off-switch,
  not a way to silence a report that is genuinely missing. An
  autopay charge whose credit was **withheld** (collected from a frozen or
  indebted account) still records no tax transaction at all, and the operator
  refund must be the **taxed** total — `withheld_autopay_intents` reports that
  figure, not the ex-tax gross.
- **A tax transaction that fails to record does not fail the charge — and is
  no longer an operator task.** The money is already correct by then; the
  failure is logged at ERROR and the sweep retries the recording from the
  calculation id frozen on the intent row until it lands (the reference is the
  PaymentIntent id, unique across all transactions, so a retry can never
  double-report). A row that keeps failing keeps logging — a quiet log before
  filing means the report is complete. Note Tax Calculations expire after 90
  days; a recording stuck longer than that needs the operator path above.

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

### Redemption-time tax exists, and is OFF (migration 0025)

If the accountant or a Massachusetts DOR letter ruling decides prepaid
credits are stored value — excluded at sale, taxable when SPENT — the
mechanism for that answer is built and dormant: `ZEROROUTER_REDEMPTION_TAX`
(default `off`). It tiles each user's usage ledger into periods, prices each
period with one Tax Calculation against the billing address checkout now
stores on the user, debits the collected figure from the balance as a `tax`
ledger entry (clamped — a drained balance is absorbed, never overdrawn), and
records the FULL figure as a tax transaction. Design and policies are in
`router/src/redemption_tax.rs`; nothing below runs while the variable is
unset, and address capture from checkout runs regardless so the data exists
before any flip.

**The flip is one paired change, in this order, or the same dollar is taxed
twice:**

1. Confirm the determination in writing (this code does not settle the law).
2. Dashboard → Tax → Settings: change the preset product tax code to the
   multi-purpose stored-value code (`txcd_10502000`). Purchases — checkout
   AND autopay, which share the preset — start pricing zero tax.
3. Set `ZEROROUTER_REDEMPTION_TAX=collect` and deploy. Redemptions start
   being taxed. (`dry_run` first is cheaper courage: periods are built and
   priced, nothing is debited, nothing reaches filing reports.)
4. Never run step 2 without step 3 (defers tax to a point that collects
   nothing) or step 3 without step 2 (taxes purchase and redemption both).

What to know when it is on:

- **Pre-flip balances are exempt.** Each user's balance at enrollment was
  bought tax-paid; periods consume that exemption before anything is
  taxable. Users created after the flip get zero exemption. Promo credit at
  enrollment sits inside the exemption — an accountant question the code
  answers toward not collecting.
- **A user with no stored address cannot be priced.** Their periods wait,
  logging `redemption_tax_fallback` on every pass, and are priced correctly
  once their next checkout stores an address. Nothing is frozen wrong to
  quiet a log.
- **Shortfalls are absorbed and filed.** The debit is clamped to the
  balance; the vendor's liability is not. Watch for the clamped-debit WARN —
  it is money ZeroRouter is paying a jurisdiction out of margin.
- **Dry-run calculations still cost Stripe Tax API calls** once
  registrations exist; the hourly sweep prices at most one calculation per
  user per pass.

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

**Today fourteen lanes are `zero` and the rest are `standard`.** `anthropic`,
`openai`, and `google` are ordinary API accounts. `bedrock` — the four
`bedrock/claude-*` lanes, added 2026-08-20 — was the first zero-retention
upstream, and it got there by configuration rather than by contract.
`fireworks` — the five open-weight `fireworks/*` lanes, added the same day —
got there a third way again: neither a contract nor a setting, but the vendor's
published default for every customer. `xai` — the two `xai/grok-*` lanes, added
the same day again — rests on the same basis as Bedrock, an enforced account
setting, but is verified in a way nothing else here is: xAI restates the
guarantee in a response header on **every** response, and ZeroRouter asserts it
before serving. `vertex` — the three `vertex/gemini-*` lanes, added 2026-08-21 —
rests on the same basis as Bedrock and xAI, an enforced configuration on the
operator's own account, this time a Google Cloud **project**. The three sections
below on enforced configuration, published defaults, and per-response
attestation are why each counts.

**`google` and `vertex` publish opposite postures for the same three Gemini
models, and that is the product rather than a bug to reconcile.** They are two
different Google products under two different data policies: `google` is the
Gemini Developer API on an ordinary key, which logs prompts for an unstated
period; `vertex` is Vertex AI on a project configured for zero data retention.
Unlike the Bedrock/Anthropic twins — where the zero-retention lane costs 10%
more — Vertex's global-endpoint price for these models is identical to the
Developer API's, so the zero-retention lane is strictly better and
`/v1/models` sorts it first. See "What the operator must do in Google Cloud"
below before that lane is ever given a credential.

The posture is pinned per PROVIDER, and that is what lets one `[retention.bedrock]`
block cover both of Bedrock's API planes (see the next section):
`data_retention_mode` is a property of the AWS account, not of a model or an
endpoint, so one setting governs every request made on that key.

### The rule for `posture = "zero"`

> A lane may be labelled zero-retention **only** on one of three bases:
>
> 1. a signed or confirmed zero-data-retention **arrangement** is in force with
>    that provider, covering the account that lane dispatches on; or
> 2. the provider **enforces** zero retention as a setting on that account, with
>    published semantics for what the setting means; or
> 3. the provider's own security documentation states zero retention as the
>    **published default** for all customers, on the API surface that lane
>    dispatches on.

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
   `--bedrock-live` below exists for. Both configuration-backed pins therefore
   carry two re-verification steps rather than one — the page hash that catches
   a reworded guarantee, and a live check of the setting itself — but the live
   halves are not alike, and the difference is the subject of the third section
   below: Bedrock's is a credentialed GET a human remembers to run, while xAI's
   runs on every request and can refuse one.
2. **The published semantics must be pinned like any other evidence.** The
   `source_url` for a configuration-backed pin is the page defining what the
   value means, so `retention-drift` catches AWS *rewording* the guarantee. It
   is the same loop as every other pin, over a different kind of claim.

`inherit`, not `default`, is the value a never-configured AWS account reports —
it means "no opinion at this scope". Only a literal `none` backs a `zero` label.

#### Published defaults, and the narrow door they come through

The third basis was added on 2026-08-20 for Fireworks, and it is the weakest of
the three in one specific respect, so it comes with the tightest conditions.

Fireworks does not sell, negotiate, or expose a switch for zero retention. Its
security documentation states it as what the platform does for everyone:
"Fireworks has Zero Data Retention by default", and "prompt and generation data
exist only in volatile memory for the duration of the request". There is no
arrangement to confirm and no setting to read back — so bases 1 and 2 cannot be
satisfied, while the claim itself is public, specific, and quotable.

Four conditions, and none of them is optional:

1. **A specific retention statement, not a reassuring page.** "We do not train
   on your data" is not this. The documentation must say what happens to prompt
   and generation data, in terms that distinguish retention from training.
2. **Scoped to the surface you dispatch on.** Fireworks' own page carves out its
   Response API, which *does* store conversations by default for 30 days.
   ZeroRouter's lanes ride chat completions, which the zero-retention sentences
   cover. A pin is only as wide as the surface the sentence names — adding a
   differently-shaped surface to a provider entry means re-reading the pin.
3. **Scoped to the models you pin.** Fireworks' sentence says "for any open
   models". The five shipped lanes are all open-weights for exactly that
   reason; its closed-weight models are deliberately not pinned, because a
   provider-level posture would extend the label past its evidence.
4. **The page hash is the entire re-verification loop, and must be treated as
   such.** This is where the basis is weaker than the other two. A contract
   cannot be revoked silently. An account setting can be re-read live, which is
   what `--bedrock-live` does. A published default can change for everybody with
   one documentation edit and no notification, and there is nothing to query,
   because there is no per-account state to query. So `retention-drift` is not a
   supplement for these lanes — it is the only check there is, and a `PAGE
   CHANGED` verdict must be read by a human before the digest is re-pinned.
   Pasting a fresh digest without reading turns the sole evidence for a
   customer-facing data claim into a rubber stamp.

Corroboration is worth more here than elsewhere, and for a reason specific to
this basis: `--corroborate` reports what OpenRouter believes about *its own*
account with a provider, which is why a `zero` pin backed by a private
arrangement is expected to look like a disagreement. A published default governs
every account alike, so OpenRouter reading the same policy and reaching the same
answer is genuine second-party agreement rather than an accident. It is still
advisory and still cannot change an exit code.

#### Per-response attestation, and why it is the strongest evidence here

Added 2026-08-20 for xAI. This is **not a fourth basis** — the `xai` pin rests
on basis 2, an enforced account setting, exactly as Bedrock's does. What is
different is the re-verification, and the difference is large enough to be worth
its own section because it changes what a `zero` label *means* for these lanes.

xAI's Zero Data Retention is a team-level toggle in the xAI Console. Its
published semantics are specific: API request inputs and outputs "are never
persisted to disk", and the default 30-day encrypted audit retention "does not
apply to ZDR-enabled teams". That satisfies basis 2 on its own. But the toggle
is **self-serve**, team-wide, and reversible by any team admin in four clicks —
so of the three bases, this is the one whose evidence can evaporate fastest and
most quietly. Without ZDR the same account stores every prompt and completion
for 30 days.

What closes that gap is that xAI publishes the verdict on every response:

> every API response includes an `x-zero-data-retention` header set to `"true"`
> or `"false"`, so your application can programmatically confirm whether ZDR is
> active

**ZeroRouter asserts it and fails closed.** `attestation_header` and
`attestation_expect` in `router/config/providers.json` declare the pair; the
dispatch path reads it off every response from that upstream and refuses the
request unless it reads `true`. A missing header fails exactly as a `false` one
does — "the upstream did not say" and "the upstream said no" are the same state
as far as a customer's data is concerned. The customer gets a 502
`retention_attestation_failed` naming the guarantee, nothing is billed (the walk
settles the reservation at zero like any other failed upstream), and the
operator gets an ERROR log line carrying the provider, the header, what it
actually said, and the upstream status.

Three things to know before pinning another vendor this way:

1. **It is asserted before anything is forwarded, streaming included.** The
   header arrives in the initial HTTP response headers, ahead of the SSE body,
   so the check sits between the request and the first chunk. Note that xAI's
   documentation says "every API response" and does not discuss streaming
   specifically; if a streamed response ever omits the header this lane fails
   closed on every streamed request, which is the correct direction for an
   assumption the vendor has not confirmed in writing.
2. **A retention failure is never retried.** Retrying would deliver the prompt
   to the unattested upstream again. It ends the candidate immediately.
3. **Provisioning the key is not enough.** The key must belong to a team with
   ZDR *enabled*, and the Console refuses to enable it while the team holds any
   Files or Collections. A key from a non-ZDR team authenticates fine and then
   fails every request closed — correctly, but it reads as an outage rather than
   as a step nobody took.

Corroboration behaves the opposite way here from the Fireworks case, and that is
expected rather than alarming: OpenRouter's directory reports `xai` as
`retainsPrompts: true, retentionDays: 30`, because that is xAI's default and
OpenRouter cannot see a toggle on somebody else's team. `--corroborate` will
therefore flag this pin as appearing to disagree. The slug is `xai`; note that
OpenRouter namespaces xAI's *models* `x-ai/*`, which is a different string.

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

### What the operator must do in Google Cloud

**The `vertex` lanes ship dark and must stay dark until every step below is
done.** The posture is a precondition of the credential, not a consequence of
it: minting the key first would put three lanes that publish "zero retention" in
front of a project that does not have it. Nothing in the repository can detect
that, because a Google Cloud project reports no such summary — see the honest
weakness noted in step 3.

1. **Create a dedicated project** (for example `zerorouter-vertex-prod`) and
   attach a Cloud Billing account. A dedicated project rather than a shared one,
   because every control below is applied *per project* and a project shared
   with anything else invites someone to re-enable caching for an unrelated
   workload.

2. **Enable the Vertex AI API** on it:

   ```bash
   gcloud config set project PROJECT_ID
   gcloud services enable aiplatform.googleapis.com
   ```

3. **Get out of scope for abuse-monitoring prompt logging.** Google logs prompts
   for up to 90 days on a classifier hit for customers "whose use of Google
   Cloud is governed by the Google Cloud Platform Terms of Service", and states
   that "customers with a Google Cloud Master Agreement are exempt from prompt
   logging for this abuse monitoring by default". Either:
   - hold a **Google Cloud Master Agreement** covering this account, or
   - file Google's **abuse-monitoring exception form** and get it approved
     ("If approved, Google won't store any prompts associated with the approved
     Google Cloud account"), linked from the data-governance page pinned in
     `[retention.vertex]`.

   **Paying Google is not sufficient.** A self-serve credit-card project is
   governed by the Cloud Platform ToS and *is* in scope. This is the step with
   no machine check behind it — no API reports whether an account is exempt — so
   it rests on knowing which agreement the account is under, or on holding the
   approval. Record which of the two applies, and where the evidence lives,
   before continuing.

4. **Disable in-memory data caching** for the project:

   ```bash
   curl -X PATCH \
     -H "Authorization: Bearer $(gcloud auth print-access-token)" \
     -H "Content-Type: application/json" \
     https://us-central1-aiplatform.googleapis.com/v1/projects/PROJECT_ID/cacheConfig \
     -d '{"name":"projects/PROJECT_ID/cacheConfig","disableCache":true}'
   ```

   The change applies to all regions. Requires `roles/aiplatform.admin`.
   Google's own position is that this cache does not violate zero data retention
   — it is in-memory only, project-isolated, 24-hour TTL — and ZeroRouter
   disables it anyway, because "retained only in RAM for a day" is a sentence a
   customer is entitled to not want, and disabling costs nothing.

5. **Leave request-response logging off.** It is off by default and per-model
   per-project. Do not enable it for any model in this project.

6. **Create the service account and key.** Give it the narrowest role that can
   call the API:

   ```bash
   gcloud iam service-accounts create zerorouter-vertex \
     --display-name="ZeroRouter Vertex dispatch"
   gcloud projects add-iam-policy-binding PROJECT_ID \
     --member="serviceAccount:zerorouter-vertex@PROJECT_ID.iam.gserviceaccount.com" \
     --role="roles/aiplatform.user"
   gcloud iam service-accounts keys create vertex-key.json \
     --iam-account=zerorouter-vertex@PROJECT_ID.iam.gserviceaccount.com
   ```

   `roles/aiplatform.user` can invoke models; it deliberately cannot change
   `cacheConfig`, so a compromise of this key cannot silently turn caching back
   on. Do the cache change in step 4 as an admin, not as this account.

7. **Store the key and set the project.** The secret container follows the
   existing convention, `<env>/providers/<secret_name>`:

   ```bash
   aws secretsmanager create-secret \
     --name zerorouter-beta/providers/vertex-service-account \
     --secret-string file://vertex-key.json
   shred -u vertex-key.json   # it is a private key; do not leave it on disk
   ```

   Then wire `VERTEX_SERVICE_ACCOUNT` as a task-definition secret and
   `VERTEX_PROJECT_ID` as plain env, and add `vertex` to `enabled_provider_keys`
   in the environment's tfvars. Injection is opt-in per key: a secret present in
   Secrets Manager but absent from that list leaves these lanes dark with no
   error anywhere.

8. **Verify before the first customer request**, both halves:

   ```bash
   # the cache setting is really off
   curl -H "Authorization: Bearer $(gcloud auth print-access-token)" \
     https://us-central1-aiplatform.googleapis.com/v1/projects/PROJECT_ID/cacheConfig
   # expect {"name": "projects/PROJECT_ID/cacheConfig", "disableCache": true}
   ```

   **A response without `disableCache` means caching is ENABLED.** The field is
   simply absent at the default, so an operator reading a bare
   `{"name": "..."}` must not treat it as a pass. It is the same trap as
   Bedrock's `inherit`.

   Then confirm the lane actually dispatches, and that `/v1/models` lists the
   three `vertex/*` rows with `"posture": "zero"`.

If any step above cannot be completed — most likely step 3 — **the honest
outcome is to change `[retention.vertex]` to `standard` rather than to ship the
lane anyway**. A wrong `standard` costs a little marketing; a wrong `zero` is a
false statement to a customer about their data.

### If a provider's posture actually changes

Raise `standard` first. A lane labelled `standard` that is really zero costs
nothing but a missed selling point; a lane labelled `zero` that is really
standard is the failure this whole mechanism exists to prevent.

## Bedrock has two API planes, and only one of them currently answers

The `bedrock` provider entry in `router/config/providers.json` declares two
endpoints for one AWS account, because AWS exposes Claude on two unrelated APIs
and they host **different model generations**:

| Plane | Endpoint | API | Hosts | Status on this account |
| --- | --- | --- | --- | --- |
| Mantle | `bedrock-mantle.{region}.api.aws/anthropic/v1/messages` | Anthropic Messages, verbatim | 5-generation Claude | **Refused.** Every model 403s `not available for this account` |
| Classic runtime (`surfaces.classic_runtime`) | `bedrock-runtime.{region}.amazonaws.com` | AWS `InvokeModel` | 4.5- and 4.6-generation Claude | **Live.** Serves all four shipped lanes |

### Where the account gate actually cuts

The gate is **not** "the mantle plane" and **not** "the new API" — it is a
per-model-generation entitlement on the AWS account, and on this account it cuts
between 4.6 and 4.7. Probed model by model on 2026-08-20, on the classic runtime
plane, with the same bearer credential:

| Model | Result |
| --- | --- |
| `us.anthropic.claude-opus-4-5-20251101-v1:0` | 200 |
| `us.anthropic.claude-sonnet-4-5-20250929-v1:0` | 200 |
| `us.anthropic.claude-haiku-4-5-20251001-v1:0` | 200 |
| `us.anthropic.claude-opus-4-6-v1` | 200 |
| opus 4.7, opus 4.8 | 403 `not available for this account` |
| every 5-generation model | 403 `not available for this account` |

The 403 arrives before IAM, region, or retention mode matter — a valid key on a
correctly configured account still gets it. **Do not add a lane above that line
without probing it first**: the id shapes are guessable, so a plausible-looking
`us.anthropic.claude-opus-4-7-v1` will pass every check this repo has and 403 on
every customer request. When Sales grants the account more generations, re-probe
and move the line.

Note also that the profile id shapes are not uniform: the 4.5-generation
profiles carry a date and a `-v1:0` suffix, while opus 4.6 is plain
`us.anthropic.claude-opus-4-6-v1`. Both are what AWS publishes; neither should
be "regularised" into the other.

Both planes read the same `BEDROCK_API_KEY` and the same `BEDROCK_REGION`, so a
deployment either reaches both or neither — which is why `/v1/models` and route
construction still share one dispatchability answer (see `ProviderMetadata::dispatchable`).

**The mantle lanes are commented out in `router/config/tiers.toml`, not
deleted.** AWS gates 5-generation Claude per account behind a Sales conversation,
entirely separately from IAM: the credential is valid, the region is right,
`data_retention_mode` reads `none`, IAM grants invoke — and the request is still
refused. Nothing in the environment records an entitlement, so the credential
filter that keeps unservable lanes off the storefront (#89) cannot see it, and
would happily publish two flagship lanes that 403 on every call. The catalog file
is the only honest place to encode it.

**To re-enable them:** AWS Sales grants account `161457899654` access to
5-generation Claude on Bedrock; verify with a real invocation rather than a
console page; then uncomment the block in `tiers.toml` and re-check its prices.
Nothing else needs changing — the retention pin, the provider entry, and the
reconciliation exemption all stay in place while the lanes are dark.

**The runtime lanes are NOT temporary and do not retire when that happens.**
They were reached for because the mantle plane was gated, but they are not a
stand-in for it: the mantle plane does not host 4.5- or 4.6-generation Claude at
all. So ungating the account gives the catalog six Bedrock lanes rather than
replacing four with two, and a customer picks a model generation rather than an
API plane. Deleting the runtime lanes later would remove four models nothing else
serves.

One honest gap in the retention verification while this is the shape:
`admin retention-drift --bedrock-live` reads the **mantle** control plane's
`/v1/data_retention`, while traffic goes to the **runtime** plane. That is still
a valid check — `data_retention_mode` is an account-level setting, so the value
that endpoint reports is the value governing every request on the key — but the
check and the traffic do go through different hosts, and it is worth knowing that
if the mantle plane ever becomes unreachable for reasons beyond the model gate.
The classic control plane exposes the same setting at
`bedrock.{region}.amazonaws.com/data-retention` (hyphen, not underscore) as a
fallback read.

## Bedrock: confirm the billing SKU after the first real request

The four `bedrock/claude-*` lanes are pinned at AWS's **regional** rates —
Opus 4.6 and Opus 4.5 both at 5.50/0.55/27.50, Sonnet 4.5 at 3.30/0.33/16.50, and
Haiku 4.5 at 1.10/0.11/5.50 per MTok. Each is exactly 10% above Anthropic's
first-party rate
for the same weights. That premium is correct and priced straight through: these
lanes dispatch `us.`-prefixed **geographic** inference profiles, and AWS prices
geographic cross-region inference at the standard class while the global class
takes ~10% off ("Cost: Standard pricing" versus "approximately 10% savings", in
AWS's own cross-region-inference comparison). Do not "correct" them downward —
that sells the lane below what AWS invoices, on every token. Reaching the cheaper
class would mean dispatching `global.`-prefixed ids, which is a different routing
and data-residency decision, not a price fix.

Read the rates from the AWS Price List API, offer `AmazonBedrockFoundationModels`
(the older `AmazonBedrock` offer carries no Claude models at all). **The usagetype
naming differs by model generation and neither form is guessable from the other:**

| Generation | Regional (what we bill at) | Global |
| --- | --- | --- |
| 4.5 | `USE1-MP:USE1_InputTokenCount-Units` | `USE1-MP:USE1_InputTokenCount_Global-Units` |
| 5 | `USE1_input_tokens_standard-Units` | `USE1_input_tokens_global_standard-Units` |

On 4.5-generation SKUs the priceDimension descriptions end "Regional" and
"Global" respectively; take the non-global member of the pair in both schemes.

**One step of that reasoning is an inference and should be closed empirically.**
AWS documents the price *level* of each profile kind but never maps a profile
prefix to a Price List `usagetype`, and its Cost and Usage Report page uses a
third naming scheme again with no distinct geographic entry. "A `us.` call bills
the unqualified SKU" is forced by elimination, and the invoice settles it. After
the first real Bedrock request, open the Cost and Usage Report and read the line
item's routing column:

- **In-region** — as pinned, nothing to do.
- **Cross-region-global** — the pins are 10% **high** and the lane is selling
  above cost. Correct both basis and sell.

A cheap desk check in the meantime: every pinned rate should be exactly 1.10x
Anthropic's first-party rate for the same model.
`every_bedrock_rate_is_exactly_the_documented_premium_over_first_party` in
`router/tests/http.rs` asserts that, so an accidental edit toward the global
figures fails `cargo test` rather than surfacing on an invoice.

These tiers are also the only ones `admin catalog-drift` does not reconcile, so
nothing in CI will catch an AWS price move on them. The exemption, its
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

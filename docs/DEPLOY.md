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
  its entries' `credential_env` name — today `ANTHROPIC_API_KEY` and
  `OPENAI_API_KEY`. A provider whose key is absent is simply not a
  candidate — set only what the catalog actually routes to.
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
  for it; the arithmetic is in the `DEPOSIT_FEE_FLOOR_USD` comment.
- **Autopay is not taxed and cannot be**, because a raw PaymentIntent has
  no `automatic_tax` parameter. The same credits bought through autopay
  collect no tax. Closing that gap needs the Tax Calculation API and is a
  separate decision.

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

## Rollback

The workflow deploys immutable per-commit image tags. To roll back, re-run
the deploy workflow from the last good commit (`workflow_dispatch` checks
out and verifies the ref before deploying); ECS's deployment circuit breaker
also rolls back automatically if new tasks fail to stabilize.

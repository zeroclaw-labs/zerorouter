// The suite that would have caught both launch-day bugs: a real browser
// through the full OIDC loop (multi-audience id token, like Zitadel), then
// every data page against the real API — list envelopes included.
import { test, expect, type Page } from '@playwright/test'
import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const routerBin = path.resolve(here, '..', '..', 'router', 'target', 'debug', 'zerorouter')
const E2E_EMAIL = 'e2e@zerorouter.test'

async function signIn(page: Page) {
  await page.goto('/')
  await page.getByRole('link', { name: /sign in/i }).click()
  // The mock IdP auto-approves and bounces straight back through the
  // callback; the SPA then renders the authenticated shell.
  await expect(page.getByText(E2E_EMAIL)).toBeVisible({ timeout: 15_000 })
}

test('login lands in the portal via the multi-audience OIDC flow', async ({ page }) => {
  await signIn(page)
  await expect(page.getByRole('heading', { name: /overview/i })).toBeVisible()
})

test('keys page creates a key, reveals it once, and hands over a runnable request', async ({
  page,
}) => {
  await signIn(page)
  await page.getByRole('link', { name: /keys/i }).click()
  const keyName = `e2e-${Date.now()}`
  await page.getByPlaceholder(/key name/i).fill(keyName)
  await page.getByRole('button', { name: /create key/i }).click()
  // The plaintext is revealed once, in the one-shot panel, prefixed like a real
  // key. Scoped to the key box rather than matched loose across the page: the
  // same panel now also prints the key inside a ready-to-run curl, so a bare
  // text match would find two elements and fail on strictness rather than on
  // anything about the key.
  await expect(page.locator('.keybox')).toHaveText(/^zcr_[a-f0-9]{64}$/)
  await expect(page.getByText(keyName)).toBeVisible()

  // The snippet, asserted in the SAME test rather than a new one: nothing is
  // lost by asserting here — it is the same dialog, at the same moment.
  //
  // This used to be a budget as well as a convenience. A user may mint only 20
  // keys per 24 hours (`MAX_KEYS_CREATED_PER_WINDOW`), the suite reused one
  // account, and nothing ever reset the database — so every test that minted a
  // key shortened how many times the suite could run before creation started
  // failing for reasons unrelated to the code. The harness now recreates its
  // own database per run (`e2e/scratch-db.mjs`), so the throttle counts from
  // zero every time and a new minting test costs nothing.
  //
  // The gap this closes: the dialog used to hand over a secret and leave the
  // customer to guess the base URL and the auth scheme.
  const snippet = page.locator('.keybox-curl .code-body')
  await expect(snippet).toBeVisible()
  const command = (await snippet.innerText()).trim()
  expect(command).toContain('/v1/chat/completions')
  expect(command).toMatch(/Authorization: Bearer zcr_[a-f0-9]{64}/)

  // The example model must exist in the catalog this deployment serves. A
  // snippet naming a lane that has since been retired would 404 on the very
  // first request a customer ever makes, which is worse than shipping no
  // snippet at all — and it would fail silently, because nothing else on this
  // page reads the catalog.
  const named = /"model": "([^"]+)"/.exec(command)
  expect(named, `the snippet must name a model: ${command}`).not.toBeNull()
  const listed = await page.evaluate(async () => {
    const catalog = await (await fetch('/v1/models')).json()
    return (catalog.data as Array<{ id: string }>).map((model) => model.id)
  })
  expect(listed).toContain(named?.[1])

  // And it points at the reference for everything the snippet does not cover.
  await expect(page.getByRole('link', { name: /api docs/i })).toBeVisible()
})

test('a key can be minted with an expiry and a credit limit', async ({ page }) => {
  await signIn(page)
  await page.getByRole('link', { name: /keys/i }).click()

  // Defaults first: the form opens on the same key it has always minted —
  // never expires, no limit — so the added fields cost an existing user
  // nothing.
  await expect(page.getByText(/this key will not expire/i)).toBeVisible()

  // Choosing a preset resolves it to a concrete date in the dialog, which is
  // the whole reason the presets live client-side.
  await page.getByLabel(/expiration/i).selectOption({ label: '1 week' })
  await expect(page.getByText(/expires \w+ \d+, \d{2}:\d{2}/i)).toBeVisible()

  const keyName = `e2e-limited-${Date.now()}`
  await page.getByPlaceholder(/key name/i).fill(keyName)
  await page.getByLabel(/credit limit in dollars/i).fill('25')
  await page.getByLabel(/reset limit every/i).selectOption({ label: 'Weekly' })
  await page.getByRole('button', { name: /create key/i }).click()
  await expect(page.locator('.keybox')).toHaveText(/^zcr_[a-f0-9]{64}$/)
  await page.getByRole('button', { name: /stored it/i }).click()

  // The row carries both, and the usage half starts at zero.
  const row = page.getByRole('row', { name: new RegExp(keyName) })
  await expect(row).toContainText('$0.00 of $25.00/we')
  await expect(row).toContainText('active')

  // A reset cadence with no limit would mint an UNLIMITED key from a request
  // that plainly asked for a budget, so the form refuses to send it.
  await page.getByPlaceholder(/key name/i).fill(`e2e-bad-${Date.now()}`)
  await page.getByLabel(/credit limit in dollars/i).fill('')
  await page.getByLabel(/reset limit every/i).selectOption({ label: 'Daily' })
  await page.getByRole('button', { name: /create key/i }).click()
  await expect(page.getByText(/set a credit limit/i)).toBeVisible()

  // ── EDITING THE KEY THAT WAS JUST MINTED ─────────────────────────────────
  //
  // Asserted in this test rather than a new one for the reason stated above: a
  // fresh test would mint another key off the account's 24-hour creation
  // throttle, and this is the same row, in the same table, one interaction
  // later. Until the edit endpoint existed, changing any of these meant
  // revoke-and-remint — a new secret and every client updated with it.
  const dataRow = page.getByRole('row', { name: new RegExp(keyName) }).first()
  await dataRow.getByRole('button', { name: new RegExp(`edit limits for ${keyName}`, 'i') }).click()

  const editor = page.locator('.key-edit-row')
  // The editor opens seeded with what the key ALREADY carries, and states the
  // used-of-limit figure the server computed — so the customer is typing over a
  // number they can see rather than one they have to remember.
  await expect(editor).toContainText('Used $0.00 of $25.00')
  await expect(page.getByLabel(`Credit limit for ${keyName}`)).toHaveValue('25')
  await expect(page.getByLabel(`Reset limit for ${keyName}`)).toHaveValue('weekly')

  // A cadence with nothing to reset is refused on the edit path exactly as it is
  // on the mint path, and refusing does not close the editor or lose the rest of
  // the form.
  await page.getByLabel(`Credit limit for ${keyName}`).fill('')
  await editor.getByRole('button', { name: /^save$/i }).click()
  // `.last()`: the create form raised the same toast a moment ago and toasts
  // stack, so an unqualified match finds both and fails on strictness rather
  // than on anything about the edit.
  await expect(page.getByText(/set a credit limit/i).last()).toBeVisible()
  await expect(page.locator('.key-edit-row')).toHaveCount(1)

  // The round trip: a new limit, a new cadence, and the expiry cleared — all in
  // one PATCH, all reflected in the row afterwards.
  await page.getByLabel(`Credit limit for ${keyName}`).fill('40')
  await page.getByLabel(`Reset limit for ${keyName}`).selectOption({ label: 'Monthly' })
  await page.getByLabel(`Expiration for ${keyName}`).selectOption({ label: 'Expiry: never' })
  await editor.getByRole('button', { name: /^save$/i }).click()

  // The editor closes and the row carries the edit. The expiry column moving
  // from a date to "never" is the half that proves an ABSENT field and a NULL
  // field are different things on this wire: the cadence select was sent as a
  // value, the expiry as an explicit null.
  await expect(page.locator('.key-edit-row')).toHaveCount(0)
  await expect(dataRow).toContainText('$0.00 of $40.00/mo')
  await expect(dataRow).toContainText('never')
  await expect(dataRow).toContainText('active')

  // And the server agrees — the row is not merely re-rendered from what the form
  // hoped it sent.
  const stored = await page.evaluate(async (name) => {
    const keys = await (await fetch('/api/keys')).json()
    return (
      keys.keys as Array<{
        name: string
        credit_limit_usd: string | null
        credit_limit_window: string | null
        expires_at: string | null
      }>
    ).find((key) => key.name === name)
  }, keyName)
  expect(stored?.credit_limit_usd).toBe('40')
  expect(stored?.credit_limit_window).toBe('monthly')
  expect(stored?.expires_at).toBeNull()
})

test('the api documentation is readable without an account', async ({ page }) => {
  // Public like the catalog, and for a stronger reason: someone deciding
  // whether to sign up is exactly the reader who does not yet know the base URL
  // is OpenAI-compatible. If the signed-out shell stops giving /docs its public
  // treatment, this route falls through to the landing screen — so the absence
  // of the landing CTA is asserted, not merely the presence of the page.
  await page.goto('/docs')

  await expect(page.getByRole('heading', { name: /api documentation/i })).toBeVisible({
    timeout: 15_000,
  })
  await expect(page.getByRole('link', { name: /sign in with sso/i })).toHaveCount(0)
  // The public top bar, with its links to the other pages a reader without a
  // session can reach.
  await expect(page.getByRole('link', { name: /^sign in$/i })).toBeVisible()
  await expect(
    page.getByRole('navigation', { name: 'Public' }).getByRole('link', { name: 'Models' }),
  ).toBeVisible()

  // The three facts a customer previously had to guess, and the one claim the
  // whole page rests on.
  const curl = page.locator('.code-body').first()
  await expect(curl).toContainText('/v1/chat/completions')
  await expect(curl).toContainText('Authorization: Bearer')
  await expect(page.getByText(/OpenAI chat-completions wire/i)).toBeVisible()

  // Same catalog check as the Keys snippet: an example model id that has left
  // the catalog is a 404 on the reader's first attempt.
  const named = /"model": "([^"]+)"/.exec((await curl.innerText()).trim())
  expect(named).not.toBeNull()
  const listed = await page.evaluate(async () => {
    const catalog = await (await fetch('/v1/models')).json()
    return (catalog.data as Array<{ id: string }>).map((model) => model.id)
  })
  expect(listed).toContain(named?.[1])

  // The error codes are the reason an agent reads this page at all, and the two
  // 402s have to be distinguishable — "rotate the key" and "top up the account"
  // are different actions.
  const codes = page.locator('.docs-table-wrap .table tbody tr td:first-child')
  const rendered = (await codes.allTextContents()).map((text) => text.trim())
  for (const code of [
    'invalid_api_key',
    'insufficient_credits',
    'key_credit_limit_exceeded',
    'velocity_cap_exceeded',
    'model_unavailable',
    'retention_attestation_failed',
  ]) {
    expect(rendered).toContain(code)
  }
})

test('the docs page is in the signed-in navigation', async ({ page }) => {
  await signIn(page)
  const docs = page.getByRole('navigation', { name: 'Portal' }).getByRole('link', { name: 'Docs' })
  await expect(docs).toBeVisible()
  await docs.click()
  await expect(page).toHaveURL(/\/docs$/)
  await expect(page.getByRole('heading', { name: /api documentation/i })).toBeVisible()
  // Rendered inside the portal shell, not the public one: a signed-in reader
  // keeps their sidebar.
  await expect(page.getByText(E2E_EMAIL)).toBeVisible()
})

// Named for what it actually asserts. It used to say "…and the autopay panel",
// which it never touched: with Stripe unconfigured the panel collapses to the
// billing-off banner. The autopay panel is covered by its own spec below.
test('credits page renders balance and promo ledger, and hides checkout when billing is off', async ({
  page,
}) => {
  await signIn(page)
  // Fund through the same admin path production uses; the user exists
  // because the login upserted it.
  execFileSync(routerBin, ['admin', 'grant-credit', '--email', E2E_EMAIL, '--amount-usd', '5'], {
    env: process.env,
  })
  await page.getByRole('link', { name: /credits/i }).click()
  await expect(page.getByRole('heading', { name: 'Credits', exact: true })).toBeVisible()
  // The ledger table shows the promo row (the envelope-unwrap regression
  // guard: a bare object here crashed the page on launch day).
  await expect(page.getByText('promo').first()).toBeVisible()
  // Stripe is deliberately unconfigured in e2e. Since embedded checkout, the
  // portal learns that at page load — `/api/me` returns a null publishable
  // key — instead of discovering it from a failed checkout call. So the
  // banner is present immediately and there is no purchase button to press:
  // a deployment without Stripe must never offer a checkout that cannot
  // complete.
  await expect(page.getByText(/billing is not enabled/i).first()).toBeVisible()
  await expect(page.getByRole('button', { name: /add credits/i })).toHaveCount(0)
})

// The ledger renders the `byok` flag the API sends, and renders it per ROW.
//
// This is the one spec in the suite that serves a stubbed API response, and the
// reason is worth stating rather than leaving as an inconsistency. Producing a
// genuine BYOK ledger row end to end needs two things this harness cannot
// cheaply arrange: an attached provider credential that the mock upstream will
// answer for, and BYOK usage above the $5,000 monthly allowance — below it the
// fee is zero and a zero debit writes no ledger row at all. What is under test
// here is a RENDERING binding, not a money path, so the honest split is: the
// server half — that a BYOK settlement really surfaces `byok: true` out of the
// database — is pinned in Rust by `the_ledger_reports_byok_per_usage_row`, and
// this pins that the SPA reads that field rather than inventing the badge.
//
// Both rows in one response on purpose: a badge hardcoded onto every row would
// pass a test that only ever looked at a BYOK row.
test('the ledger badges only the rows the API marks as byok', async ({ page }) => {
  await page.route('**/api/billing/ledger*', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        limit: 50,
        entries: [
          {
            id: 2,
            created_at: '2026-08-21T12:00:00Z',
            entry_type: 'usage',
            amount_usd: '-0.0500',
            balance_after_usd: '4.95',
            note: null,
            byok: true,
          },
          {
            id: 1,
            created_at: '2026-08-21T11:00:00Z',
            entry_type: 'usage',
            amount_usd: '-1.0000',
            balance_after_usd: '5.00',
            note: null,
            byok: null,
          },
        ],
      }),
    })
  })
  await signIn(page)
  await page.getByRole('link', { name: /credits/i }).click()
  await expect(page.getByRole('heading', { name: 'Credits', exact: true })).toBeVisible()

  const rows = page.locator('.table tbody tr')
  await expect(rows).toHaveCount(2)
  // The badge follows the flag: present on the row the API marked, absent on
  // the one it did not.
  await expect(rows.nth(0).getByText('your own key')).toBeVisible()
  await expect(rows.nth(1).getByText('your own key')).toHaveCount(0)
  // And it is additional to the entry type, not a replacement for it — a reader
  // must still be able to see that both rows are usage debits.
  await expect(rows.nth(0).getByText('usage')).toBeVisible()
  await expect(rows.nth(1).getByText('usage')).toBeVisible()
})

test('the add-credits modal opens to our amount step when Stripe is configured', async ({
  page,
}) => {
  // The e2e router runs without Stripe, and standing a second one up just for
  // this would not test anything more: the portal decides whether to offer
  // checkout purely from `stripe_publishable_key` on /api/me. Injecting one
  // exercises the real component path — button, modal, amount step, and the
  // server-priced quote — without depending on js.stripe.com being reachable
  // from CI. The Stripe form itself is deliberately not reached: mounting it
  // is the one step that needs the network, and it is Stripe's code, not ours.
  await page.route('**/api/me', async (route) => {
    const response = await route.fetch()
    const body = await response.json()
    await route.fulfill({
      response,
      json: { ...body, stripe_publishable_key: 'pk_test_e2e_placeholder' },
    })
  })

  await signIn(page)
  await page.getByRole('link', { name: /credits/i }).click()
  await expect(page.getByRole('heading', { name: 'Credits', exact: true })).toBeVisible()

  // The banner is gone and the entry point is offered.
  await expect(page.getByText(/billing is not enabled/i)).toHaveCount(0)
  await page.getByRole('button', { name: /add credits/i }).click()

  // The amount step is ours, not Stripe's: presets, a custom field, and a
  // Continue that advances to the payment step.
  //
  // The priced quote line is deliberately NOT asserted here. It comes from
  // /api/billing/quote, which needs Stripe configured on the SERVER — this
  // test only injects a publishable key into the client. The fee arithmetic
  // behind that line is pinned in Rust (`deposit_fee_quote` unit tests and the
  // wire-contract test), which is where it belongs; re-mocking it here would
  // assert our own fixture rather than the server's answer.
  const modal = page.getByRole('dialog', { name: /add credits/i })
  await expect(modal).toBeVisible()
  await expect(modal.getByRole('button', { name: '$25', exact: true })).toBeVisible()
  await expect(modal.getByRole('button', { name: '$100', exact: true })).toBeVisible()
  await expect(modal.getByLabel(/custom amount in dollars/i)).toBeVisible()
  await expect(modal.getByRole('button', { name: /continue/i })).toBeVisible()

  // Selecting a preset is ours too, and must not navigate anywhere.
  await modal.getByRole('button', { name: '$100', exact: true }).click()
  await expect(page).toHaveURL(/\/credits$/)

  // Dismissing costs nothing: no Checkout Session is created until the
  // payment step is reached.
  await page.keyboard.press('Escape')
  await expect(modal).toHaveCount(0)
})

/**
 * Inject an /api/me that claims a given deployment capability set.
 *
 * The same technique the modal spec above uses, and for the same reason: the
 * portal decides what to offer purely from what /api/me reports, and the e2e
 * router runs without Stripe. Overriding the response exercises the real
 * component path for a capability this harness's router cannot itself have.
 */
async function withCapabilities(
  page: Page,
  extra: Record<string, unknown>,
): Promise<void> {
  await page.route('**/api/me', async (route) => {
    const response = await route.fetch()
    const body = await response.json()
    await route.fulfill({
      response,
      json: { ...body, stripe_publishable_key: 'pk_test_e2e_placeholder', ...extra },
    })
  })
}

test('the stablecoin option is absent when the deployment has not enabled it', async ({ page }) => {
  // The dark-ship contract, rendered. `crypto_rail: false` is what every
  // deployment reports until the operator sets ZEROROUTER_CRYPTO_RAIL, and the
  // Credits page must then look exactly as it did before the rail existed —
  // not a disabled control, not an explanatory note, nothing at all.
  await withCapabilities(page, { crypto_rail: false })
  await signIn(page)
  await page.getByRole('link', { name: /credits/i }).click()
  await page.getByRole('button', { name: /add credits/i }).click()

  const modal = page.getByRole('dialog', { name: /add credits/i })
  await expect(modal).toBeVisible()
  // The amount step is fully present...
  await expect(modal.getByRole('button', { name: '$25', exact: true })).toBeVisible()
  // ...and carries no trace of the other rail.
  await expect(modal.getByRole('group', { name: /payment method/i })).toHaveCount(0)
  await expect(modal.getByRole('radio', { name: /stablecoin/i })).toHaveCount(0)
  await expect(modal.getByText(/stablecoin/i)).toHaveCount(0)
})

test('an older router that never heard of the rail also renders no crypto option', async ({
  page,
}) => {
  // `crypto_rail` absent entirely, not false — the shape a router built before
  // this feature returns. The portal must read that as "off" rather than
  // crashing or defaulting to on.
  await withCapabilities(page, {})
  await signIn(page)
  await page.getByRole('link', { name: /credits/i }).click()
  await page.getByRole('button', { name: /add credits/i }).click()

  const modal = page.getByRole('dialog', { name: /add credits/i })
  await expect(modal).toBeVisible()
  await expect(modal.getByRole('button', { name: '$25', exact: true })).toBeVisible()
  await expect(modal.getByRole('radio', { name: /stablecoin/i })).toHaveCount(0)
})

test('with the rail enabled, card stays the default and stablecoin is the opt-in', async ({
  page,
}) => {
  await withCapabilities(page, { crypto_rail: true })
  await signIn(page)
  await page.getByRole('link', { name: /credits/i }).click()
  await page.getByRole('button', { name: /add credits/i }).click()

  const modal = page.getByRole('dialog', { name: /add credits/i })
  await expect(modal).toBeVisible()

  const card = modal.getByRole('radio', { name: /^card$/i })
  const crypto = modal.getByRole('radio', { name: /stablecoin/i })
  await expect(card).toBeVisible()
  await expect(crypto).toBeVisible()

  // Card is selected on open. The crypto rail is an opt-in per purchase, never
  // a remembered preference — a customer must not be silently returned to a
  // rail they used once.
  await expect(card).toBeChecked()
  await expect(crypto).not.toBeChecked()
  // Nothing crypto-specific is claimed until it is chosen.
  await expect(modal.getByText(/cannot be charged back/i)).toHaveCount(0)

  // Choosing it discloses the things a buyer needs before committing: the
  // different fee, that we settle in dollars rather than holding coin, the
  // per-transaction ceiling, and the absence of chargebacks.
  await crypto.check()
  await expect(crypto).toBeChecked()
  await expect(card).not.toBeChecked()
  await expect(modal.getByText(/5% on stablecoin instead of 5\.5%/i)).toBeVisible()
  await expect(modal.getByText(/settle in dollars and never hold the coin/i)).toBeVisible()
  await expect(modal.getByText(/\$10,000 per crypto payment/i)).toBeVisible()
  await expect(modal.getByText(/cannot be charged back/i)).toBeVisible()

  // Switching back retracts all of it, and selecting a rail never navigates.
  await card.check()
  await expect(modal.getByText(/cannot be charged back/i)).toHaveCount(0)
  await expect(page).toHaveURL(/\/credits$/)

  // Reopening resets to card even after choosing crypto.
  await crypto.check()
  await page.keyboard.press('Escape')
  await expect(modal).toHaveCount(0)
  await page.getByRole('button', { name: /add credits/i }).click()
  await expect(modal.getByRole('radio', { name: /^card$/i })).toBeChecked()
})

test('the autopay panel offers setup, arming, and its own status', async ({ page }) => {
  // Autopay had a complete UI and no e2e coverage at all: the credits spec
  // above used to be named "…and the autopay panel" but, with Stripe
  // unconfigured, the panel it claimed to cover collapses to the billing-off
  // banner and no locator in it ever touched the panel. Injecting the
  // publishable key the same way the modal spec does renders the real panel
  // against the real `GET /api/billing/autopay`.
  await page.route('**/api/me', async (route) => {
    const response = await route.fetch()
    const body = await response.json()
    await route.fulfill({
      response,
      json: { ...body, stripe_publishable_key: 'pk_test_e2e_placeholder' },
    })
  })

  await signIn(page)
  await page.getByRole('link', { name: /credits/i }).click()
  await expect(page.getByRole('heading', { name: 'Credits', exact: true })).toBeVisible()

  // The panel, its status badge, and the card-setup entry point. A fresh e2e
  // user has never saved a card, so autopay reads off and the copy says so —
  // this is the server's answer, not a fixture.
  const autopay = page.locator('section.panel').filter({ hasText: 'Autopay' })
  await expect(autopay).toBeVisible()
  await expect(autopay.getByText('off', { exact: true })).toBeVisible()
  await expect(autopay.getByText(/no card on file yet/i)).toBeVisible()
  await expect(autopay.getByRole('button', { name: /save a card/i })).toBeVisible()

  // Disarming is offered only when autopay is on, so it must be absent here —
  // the guard against showing a customer an off switch for something that is
  // already off.
  await expect(autopay.getByRole('button', { name: /turn off/i })).toHaveCount(0)

  // Client-side amount validation, which is ours and never reaches the server:
  // a top-up below the $5.00 minimum is refused in the browser.
  await autopay.getByLabel(/autopay threshold in dollars/i).fill('10.00')
  await autopay.getByLabel(/autopay top-up in dollars/i).fill('1.00')
  await autopay.getByRole('button', { name: /turn on autopay/i }).click()
  await expect(autopay.getByText(/top-up of at least \$5\.00/i)).toBeVisible()

  // A valid pair passes the client check and reaches the server. Only the
  // publishable key was injected, so the ROUTER still has no Stripe
  // configuration and `put_autopay` refuses with `billing_unavailable` before
  // it looks at anything else. The portal must degrade to the billing-off
  // banner rather than report success — a deployment that cannot charge a card
  // must never leave a customer believing autopay is armed.
  //
  // The sibling refusal — a 400 when Stripe IS configured but no card has been
  // saved — is deliberately not reached here. It needs a router with real
  // Stripe credentials, and it is already pinned server-side where the guard
  // lives (`put_autopay` verifies a card exists at Stripe before enabling, so
  // arming cannot burn the three-strikes budget on a card that was never
  // saved).
  await autopay.getByLabel(/autopay top-up in dollars/i).fill('25.00')
  await autopay.getByRole('button', { name: /turn on autopay/i }).click()
  await expect(page.getByText(/billing is not enabled/i).first()).toBeVisible()
  // And autopay was never reported as on.
  await expect(page.getByText(/autopay is on/i)).toHaveCount(0)
})

test('the api surface the SPA consumes returns the documented shapes', async ({ page }) => {
  await signIn(page)
  // Contract pins straight from the browser session: catches envelope
  // drift on endpoints whose pages might not render them immediately.
  const shapes = await page.evaluate(async () => {
    const keys = await (await fetch('/api/keys')).json()
    // The 0023 fields are ADDITIVE: present on every row, null where unset.
    // A missing key here means the SPA's limit column would render undefined.
    const firstKey = keys.keys[0] ?? {}
    const keyHasLimitFields =
      'expires_at' in firstKey &&
      'credit_limit_usd' in firstKey &&
      'credit_limit_window' in firstKey &&
      'credit_limit_used_usd' in firstKey
    const ledger = await (await fetch('/api/billing/ledger?limit=5')).json()
    const me = await (await fetch('/api/me')).json()
    const usage = await (await fetch('/api/usage?days=7')).json()
    return {
      keysIsEnvelope: Array.isArray(keys.keys),
      keyHasLimitFields,
      ledgerIsEnvelope: Array.isArray(ledger.entries),
      meHasEmail: typeof me.email === 'string',
      usageHasTotals: typeof usage.totals === 'object' && Array.isArray(usage.daily),
    }
  })
  expect(shapes).toEqual({
    keysIsEnvelope: true,
    keyHasLimitFields: true,
    ledgerIsEnvelope: true,
    meHasEmail: true,
    usageHasTotals: true,
  })
})

test('the models catalog labels every lane with its retention posture', async ({ page }) => {
  // The catalog is a storefront — reachable signed OUT — and the retention
  // label is a claim ZeroRouter makes to anyone who reads it, so this is
  // exercised without a session on purpose.
  await page.goto('/models')

  const header = page.getByRole('columnheader', { name: /^retention$/i })
  await expect(header).toBeVisible({ timeout: 15_000 })

  // EVERY row carries a label. A blank cell here is the failure the whole
  // feature exists to prevent: an unlabelled lane a reader has to guess about.
  const rows = page.locator('table.table tbody tr')
  const count = await rows.count()
  expect(count).toBeGreaterThan(0)
  for (let i = 0; i < count; i += 1) {
    await expect(rows.nth(i).locator('td').last()).toHaveText(
      /zero retention|provider retains data/,
    )
  }

  // And the page explains what the label means rather than leaving a bare word
  // in a column. The shipped catalog carries zero-retention lanes since
  // 2026-08-20, so the footnote must render its COUNTED branch — the
  // "no lane currently carries" copy would now be a false statement, and it is
  // asserted absent rather than merely not asserted present.
  await expect(page.getByText(/zero-retention lanes are listed first/i)).toBeVisible()
  await expect(page.getByText(/lanes are zero retention/i)).toBeVisible()
  await expect(page.getByText(/no lane currently carries a zero-retention label/i)).toHaveCount(0)

  // A zero-retention lane can be dearer than the same model on a standard
  // account, so the page has to say why — otherwise the storefront shows a
  // higher price for identical weights and reads as a markup.
  await expect(page.getByText(/passed through like every other rate/i)).toBeVisible()

  // Ordering, read off the rendered table: once a zero-retention lane appears,
  // no retaining lane may precede it. This was VACUOUS until the catalog had a
  // zero lane to sort; it is now a real assertion, and it fails if this page's
  // own sort (which re-sorts for vendor grouping and would otherwise undo the
  // router's order) drops its retention rank.
  const labels = (await rows.locator('td:last-child').allTextContents()).map((t) => t.trim())
  const lastZero = labels.lastIndexOf('zero retention')
  const firstStandard = labels.indexOf('provider retains data')
  expect(lastZero).toBeGreaterThanOrEqual(0)
  expect(firstStandard).toBeGreaterThanOrEqual(0)
  expect(lastZero).toBeLessThan(firstStandard)
})

test('a per-tier retention override renders on the storefront, not the provider pin', async ({
  page,
}) => {
  // The override's first real use, checked at the surface a CUSTOMER reads.
  //
  // `fireworks/qwen3.8-max` dispatches the same Fireworks account and the same
  // key as the five open-weight lanes beside it, and `[retention.fireworks]`
  // pins that provider `zero`. The lane is closed-weight, so the sentence
  // backing that pin does not reach it, and a per-tier override in `tiers.toml`
  // publishes `standard` instead. Every path between that block and this badge
  // — catalog load, `candidate_retention`, `/v1/models`, this page's own
  // re-sort — has to carry the override rather than the provider pin, and until
  // this lane shipped none of them had ever been asked to.
  //
  // This is deliberately NOT a re-test of the router's JSON (http.rs does that).
  // The portal re-sorts the rows for vendor grouping and picks the badge itself
  // from `m.retention?.posture`, so it is capable of reversing both the label
  // and the order on its own.
  await page.goto('/models')
  await expect(page.getByRole('columnheader', { name: /^retention$/i })).toBeVisible({
    timeout: 15_000,
  })

  const rows = page.locator('table.table tbody tr')
  const ids = (await rows.locator('td:nth-child(2)').allTextContents()).map((t) => t.trim())
  const labels = (await rows.locator('td:last-child').allTextContents()).map((t) => t.trim())

  const overridden = ids.indexOf('fireworks/qwen3.8-max')
  expect(overridden).toBeGreaterThanOrEqual(0)
  expect(labels[overridden]).toBe('provider retains data')

  // Its siblings on the same provider still read zero — which is what makes the
  // line above an override rather than the provider pin having been changed.
  for (const sibling of ['fireworks/kimi-k3', 'fireworks/minimax-m3']) {
    const at = ids.indexOf(sibling)
    expect(at).toBeGreaterThanOrEqual(0)
    expect(labels[at]).toBe('zero retention')
    // ...and the overridden lane sorts BELOW them, despite sharing a vendor
    // prefix that this page groups on. A retaining lane rendered inside the
    // zero block would misrepresent it by position alone.
    expect(overridden).toBeGreaterThan(at)
  }

  // The hover text must carry the REASON. A bare "provider retains data" badge
  // on a lane whose provider is advertised as zero-retention two rows above is
  // exactly the thing a reader would assume is a bug; the tooltip is where the
  // scope limit gets explained, so it has to actually be there.
  const title = await rows.nth(overridden).locator('td').last().getAttribute('title')
  expect(title).toMatch(/open models/i)
  expect(title).toMatch(/closed-weight/i)
})

test('a provider key can be attached, is shown only by fingerprint, and is never re-displayed', async ({
  page,
}) => {
  await signIn(page)
  await page.getByRole('link', { name: /keys/i }).click()

  // The section renders because the deployment provisioned an encryption key.
  // On a deployment that has not, nothing below exists at all — see
  // `byok_ships_dark_when_the_deployment_has_no_encryption_key` in
  // router/tests/portal.rs for the other half of that contract.
  await expect(page.getByRole('heading', { name: /your own provider keys/i })).toBeVisible()

  // The three things the copy must say, because they are what a customer is
  // agreeing to: what is free, what the fee is beyond it, and whose retention
  // agreement governs.
  await expect(page.getByText(/of catalog-equivalent usage each month is free/i)).toBeVisible()
  await expect(page.getByText(/5% of what the same usage would have cost/i)).toBeVisible()
  await expect(page.getByText(/governed by your agreement with that provider/i)).toBeVisible()

  // The allowance meter. A fresh e2e user has run no BYOK traffic, so the whole
  // $5,000 is still there — and the figures come from `/api/me` rather than
  // from the bundle, which is what this assertion is really pinning: a portal
  // that hardcoded the allowance would keep rendering it after the server
  // changed the number, and this panel is where a customer reads what they will
  // be billed.
  await expect(page.getByText(/of this month.s \$5,000\.00 allowance remaining/i)).toBeVisible()
  await expect(page.getByText(/Usage within the allowance is not charged at all/i)).toBeVisible()
  const allowance = await page.evaluate(async () => {
    const res = await fetch('/api/me')
    return (await res.json()) as { byok_allowance?: Record<string, string> }
  })
  expect(allowance.byok_allowance, 'the allowance must be reported').toBeTruthy()
  expect(allowance.byok_allowance?.allowance_usd).toBe('5000')
  expect(Number(allowance.byok_allowance?.remaining_usd)).toBe(5000)

  const providerKey = `sk-ant-e2e-${Date.now()}-0123456789abcdef`
  await page.getByLabel(/^provider$/i).selectOption({ label: 'Anthropic' })
  await page.getByLabel(/provider api key/i).fill(providerKey)
  await page.getByRole('button', { name: /attach key/i }).click()

  // The row shows the provider, the last four, and a 16-character fingerprint.
  const row = page.locator('tr', { hasText: 'Anthropic' })
  await expect(row).toBeVisible({ timeout: 10_000 })
  await expect(row).toContainText(`…${providerKey.slice(-4)}`)

  // Paste-once, asserted the only way that means anything: the full key must
  // not appear anywhere in the rendered page, and the input that held it must
  // be empty again.
  const body = await page.locator('body').innerText()
  expect(body).not.toContain(providerKey)
  expect(body).not.toContain(providerKey.slice(0, 20))
  await expect(page.getByLabel(/provider api key/i)).toHaveValue('')

  // Nor in the API response the page reloads from — a fingerprint and a last4,
  // and no field that could carry the key.
  const listed = await page.evaluate(async () => {
    const res = await fetch('/api/byok')
    return (await res.json()) as { keys: Array<Record<string, unknown>> }
  })
  const attached = listed.keys.find((k) => k.provider === 'anthropic')
  expect(attached, 'the key should be listed').toBeTruthy()
  expect(JSON.stringify(attached)).not.toContain(providerKey)
  expect(attached?.api_key).toBeUndefined()
  expect(String(attached?.fingerprint)).toHaveLength(16)
  expect(attached?.last4).toBe(providerKey.slice(-4))

  // The fallback opt-in renders OFF, and the control states the consequence on
  // itself. Both halves matter: a customer must be able to see that the default
  // is no-fallback, and must not have to go looking for what turning it on
  // costs.
  const fallback = row.getByRole('checkbox', { name: /use zerorouter's key if my anthropic key fails/i })
  await expect(fallback).toBeVisible()
  await expect(fallback).not.toBeChecked()
  await expect(row).toContainText(/those attempts bill at full catalog price, not 5%/i)
  await expect(row).toContainText(/do not use your monthly allowance/i)

  // Turning it on persists, and the server is the thing that says so — a
  // checkbox that only looked checked would be the failure worth catching.
  await fallback.check()
  await expect(fallback).toBeChecked()
  const attachedKeys = await page.evaluate(async () => {
    const res = await fetch('/api/byok')
    return (await res.json()) as { keys: Array<Record<string, unknown>> }
  })
  expect(attachedKeys.keys.find((k) => k.provider === 'anthropic')?.fallback_enabled).toBe(true)

  // And off again, because an opt-in you cannot withdraw is not one.
  await fallback.uncheck()
  await expect(fallback).not.toBeChecked()

  // And it can be taken away again, which is the promise that makes attaching
  // one reasonable in the first place.
  await row.getByRole('button', { name: /^remove$/i }).click()
  await row.getByRole('button', { name: /confirm remove/i }).click()
  await expect(page.getByText(/no provider keys attached/i)).toBeVisible({ timeout: 10_000 })
})

// The lane the playground round-trip drives. `google` is the upstream
// global-setup redirects to the mock, and this is its cheapest lane — the one
// whose reservation leaves the most of the e2e grant intact for the specs after
// it. Its posture is `standard`, which is what makes the badge assertion below
// worth making: the page shows the real claim, not a flattering default.
const MOCK_LANE = 'google/gemini-3.5-flash-lite'
// A lane that DECLARES text-only input, for the capability gate.
const TEXT_ONLY_LANE = 'fireworks/glm-5.2'

test('the playground groups its lane picker by retention posture, zero first', async ({ page }) => {
  await signIn(page)
  const nav = page
    .getByRole('navigation', { name: 'Portal' })
    .getByRole('link', { name: 'Playground' })
  await expect(nav).toBeVisible()
  await nav.click()
  await expect(page).toHaveURL(/\/playground$/)
  await expect(page.getByRole('heading', { name: 'Playground', exact: true })).toBeVisible()

  // The two groups exist and are in the catalog's order. This is the same claim
  // the storefront makes by sorting its table; a picker has room to name it, so
  // it does, and the naming has to stay honest.
  const groups = page.locator('.lane-group')
  await expect(groups).toHaveCount(2, { timeout: 15_000 })
  const postures = await groups.evaluateAll((nodes) =>
    nodes.map((node) => node.getAttribute('data-posture')),
  )
  expect(postures).toEqual(['zero', 'standard'])
  await expect(groups.nth(0).locator('.lane-group-title')).toContainText('Zero retention')
  await expect(groups.nth(1).locator('.lane-group-title')).toContainText('Provider retains data')

  // And every lane really sits in the group its own badge claims. Ordering the
  // headings correctly while filing lanes under the wrong one would be the
  // worse bug — a reader picks a lane by the heading above it — so this is
  // asserted per row rather than per group.
  for (const [index, label] of [
    [0, 'zero retention'],
    [1, 'provider retains data'],
  ] as const) {
    const lanes = groups.nth(index).locator('.lane')
    const count = await lanes.count()
    expect(count, `the ${label} group must not be empty`).toBeGreaterThan(0)
    for (let i = 0; i < count; i += 1) {
      await expect(lanes.nth(i).locator('.badge').first()).toHaveText(label)
    }
  }

  // The ZDR promise, stated where the question occurs to a reader.
  await expect(page.getByText(/conversations live in this tab only/i)).toBeVisible()

  // Search narrows the picker without breaking the grouping.
  await page.getByLabel(/search models/i).fill('deepseek')
  await expect(page.locator('.lane')).toHaveCount(
    await page.locator('.lane-id', { hasText: 'deepseek' }).count(),
  )
})

test('a completion round-trips through the real router and mints the playground key', async ({
  page,
}) => {
  await signIn(page)
  // The playground spends real credits, so the account needs some. Same admin
  // path production uses, same one the credits spec funds through.
  execFileSync(routerBin, ['admin', 'grant-credit', '--email', E2E_EMAIL, '--amount-usd', '5'], {
    env: process.env,
  })

  await page.getByRole('link', { name: /playground/i }).click()
  await expect(page.getByRole('heading', { name: 'Playground', exact: true })).toBeVisible()

  // Pick the lane the mock upstream stands behind.
  await page.getByLabel(/search models/i).fill(MOCK_LANE)
  await page.locator('.lane', { hasText: MOCK_LANE }).click()

  // No key has been minted for this browser yet — the mint happens on the first
  // send, which is the moment the credential is actually needed.
  await page.getByLabel('Prompt').fill('Say the thing.')
  await page.getByRole('button', { name: /^send$/i }).click()

  // The reply streamed back through the REAL router: real key auth, real
  // admission against a real balance, real dispatch over the chat-completions
  // wire, real settlement on the usage the far end reported. Only the model is
  // a stand-in.
  const assistant = page.locator('.turn-assistant').last()
  await expect(assistant.locator('.turn-body')).toHaveText(/Zero retention acknowledged\./, {
    timeout: 20_000,
  })

  // The token counts are the upstream's, carried on the final usage frame —
  // which arrives only because the page asks for `stream_options.include_usage`.
  await expect(assistant.locator('.turn-usage')).toContainText('41 in')
  await expect(assistant.locator('.turn-usage')).toContainText('7 out')

  // A cost, computed from the catalog's decimal-string rates, and labelled for
  // what it is. The figure is deliberately not pinned to the digit here: the
  // arithmetic is exact and tested by construction, while the rate it multiplies
  // is a catalog value that may legitimately be repriced.
  const cost = assistant.locator('.turn-cost')
  await expect(cost).toContainText('estimated')
  await expect(cost).toHaveText(/\$0\.\d+/)

  // THE RETENTION BADGE, ON THE RESPONSE. `google` is a retaining lane and the
  // page says so — the failure worth catching is a playground that renders the
  // brand's favourite label regardless of which lane answered.
  await expect(assistant.locator('.turn-lane')).toContainText(MOCK_LANE)
  await expect(assistant.locator('.turn-lane .badge')).toHaveText('provider retains data')

  // The balance moved, and the page re-read it from the server rather than
  // subtracting its own estimate.
  await expect(page.getByText(/credit balance/i)).toBeVisible()

  // THE KEY'S OWN BUDGET, shown beside the balance. The key this page just
  // minted lives in `localStorage`, so it carries a default credit limit the key
  // dialog does not impose — and a cap the customer cannot see is a mystery 402
  // waiting to happen, so the page names it and names its cadence.
  const budget = page.locator('.stat', { hasText: /playground key budget/i })
  await expect(budget).toContainText('of $5.00')
  await expect(budget).toContainText('resets monthly')

  // THE IMPLICIT KEY IS VISIBLE AND REVOCABLE. This is the whole reason the
  // design is an ordinary key rather than a hidden credential: a customer can
  // find it, and taking it away really does turn the page off.
  await page.getByRole('link', { name: /keys/i }).click()
  // Scoped to the LIVE row on purpose. Keys are disabled, never deleted, so a
  // database that has run this suite before carries the playground keys of
  // every previous run — and "exactly one of them is active" is the property
  // worth asserting anyway.
  const live = page.getByRole('row', { name: /playground/ }).filter({ hasText: 'active' })
  await expect(live).toHaveCount(1)
  await expect(live).toBeVisible()

  // And the server agrees, which is what actually makes the row above the key
  // this page is using rather than a leftover.
  const named = await page.evaluate(async () => {
    const keys = await (await fetch('/api/keys')).json()
    return (keys.keys as Array<{ name: string; disabled: boolean }>).filter(
      (key) => key.name === 'playground' && !key.disabled,
    ).length
  })
  expect(named).toBe(1)
})

test('the capability gate is rendered when a text-only lane is sent an image', async ({ page }) => {
  await signIn(page)
  execFileSync(routerBin, ['admin', 'grant-credit', '--email', E2E_EMAIL, '--amount-usd', '5'], {
    env: process.env,
  })
  await page.getByRole('link', { name: /playground/i }).click()
  await expect(page.getByRole('heading', { name: 'Playground', exact: true })).toBeVisible()

  // A one-pixel PNG is enough: the gate reads the request's content PARTS, not
  // the image, and refuses before anything is reserved or dialled.
  await page.setInputFiles('input[type="file"]', {
    name: 'pixel.png',
    mimeType: 'image/png',
    buffer: Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
      'base64',
    ),
  })
  await expect(page.locator('.attachment-thumb')).toBeVisible()

  // The picker is deliberately NOT filtered — an absent `input_modalities` means
  // unknown, and the router serves those, so hiding lanes here would make the
  // page stricter than the product. A lane that DECLARES text-only is warned
  // about instead, and the send still goes through to the server's verdict.
  await page.getByLabel(/search models/i).fill(TEXT_ONLY_LANE)
  await page.locator('.lane', { hasText: TEXT_ONLY_LANE }).click()
  await expect(page.locator('.attachment-warn')).toContainText(/lists no image input/i)

  await page.getByLabel('Prompt').fill('What is in this image?')
  await page.getByRole('button', { name: /^send$/i }).click()

  // The router's own words, rendered as a banner rather than swallowed: it names
  // what the lane accepts and states that nothing was reserved, which is the
  // difference between "try another model" and "am I being charged for this?"
  const banner = page.getByRole('alert')
  await expect(banner).toContainText(/does not accept image input/i, { timeout: 20_000 })
  await expect(banner).toContainText(/Nothing was reserved and no upstream was contacted/i)

  // And the prompt survived the refusal — a rejected send must not cost the
  // customer what they typed.
  await expect(page.getByLabel('Prompt')).toHaveValue('What is in this image?')
})

test('a spent playground budget is explained as the key’s own limit, and is raisable', async ({
  page,
}) => {
  await signIn(page)
  execFileSync(routerBin, ['admin', 'grant-credit', '--email', E2E_EMAIL, '--amount-usd', '5'], {
    env: process.env,
  })

  // A real completion first, so the key has settled spend to be measured
  // against. This also mints the key if an earlier spec has not.
  await page.getByRole('link', { name: /playground/i }).click()
  await page.getByLabel(/search models/i).fill(MOCK_LANE)
  await page.locator('.lane', { hasText: MOCK_LANE }).click()
  await page.getByLabel('Prompt').fill('Say the thing.')
  await page.getByRole('button', { name: /^send$/i }).click()
  await expect(page.locator('.turn-assistant').last().locator('.turn-body')).toHaveText(
    /Zero retention acknowledged\./,
    { timeout: 20_000 },
  )

  // Read what the key has actually spent and aim BELOW it. Picking a fixed
  // "small" limit would be a race against the catalog's rates and the
  // reservation's size; a limit strictly under the settled spend refuses on
  // `committed < cap` before the projection is even considered, whatever a
  // request happens to reserve. This is the browser-level statement of the same
  // property `lowering_a_limit_below_what_is_already_spent_refuses_immediately`
  // pins in the router suite.
  const playgroundKey = await page.evaluate(async () => {
    const keys = await (await fetch('/api/keys')).json()
    return (
      keys.keys as Array<{ name: string; disabled: boolean; credit_limit_used_usd: string | null }>
    ).find((key) => key.name === 'playground' && !key.disabled)
  })
  const used = Number(playgroundKey?.credit_limit_used_usd ?? '0')
  expect(used, 'the completion above must have settled some spend').toBeGreaterThan(0)
  const underwater = (used / 2).toFixed(8)
  expect(Number(underwater)).toBeGreaterThan(0)

  // Lower it from the Keys page — the customer's own affordance, not a fixture.
  await page.getByRole('link', { name: /keys/i }).click()
  const live = page.getByRole('row', { name: /playground/ }).filter({ hasText: 'active' }).first()
  await live.getByRole('button', { name: /edit limits for playground/i }).click()
  const editor = page.locator('.key-edit-row')
  await page.getByLabel('Credit limit for playground').fill(underwater)
  // The form warns before saving, because a limit under what is already spent
  // takes effect on the very next request rather than at the next reset.
  await expect(editor.locator('.key-edit-warn')).toContainText(/refuse its next request/i)
  await editor.getByRole('button', { name: /^save$/i }).click()
  await expect(page.locator('.key-edit-row')).toHaveCount(0)

  // THE REFUSAL, IN ITS OWN WORDS. The router answers a spent key budget with
  // `key_credit_limit_exceeded` rather than `insufficient_credits`, and the
  // whole point of that separate code is that a customer with $5 of credit must
  // not be told they are out of money.
  await page.getByRole('link', { name: /playground/i }).click()
  await page.getByLabel(/search models/i).fill(MOCK_LANE)
  await page.locator('.lane', { hasText: MOCK_LANE }).click()
  await page.getByLabel('Prompt').fill('Say it again.')
  await page.getByRole('button', { name: /^send$/i }).click()

  const banner = page.getByText(/used its whole credit limit/i)
  await expect(banner).toBeVisible({ timeout: 20_000 })
  await expect(page.getByText(/account balance of \$[\d.]+ is untouched/i)).toBeVisible()
  // And it points at the page that fixes it.
  await expect(
    page.locator('.banner').getByRole('link', { name: /keys/i }).first(),
  ).toBeVisible()
  // The prompt survived the refusal — a budget stop must not cost the customer
  // what they typed.
  await expect(page.getByLabel('Prompt')).toHaveValue('Say it again.')

  // Put the default back, so this spec leaves the shared e2e account as it found
  // it and the budget stays raisable rather than a wall.
  //
  // Scoped to the NAV: the banner still on screen carries its own link to the
  // same page, which is the point of the banner and a strictness collision for
  // an unqualified match.
  await page
    .getByRole('navigation', { name: 'Portal' })
    .getByRole('link', { name: 'Keys' })
    .click()
  const stillLive = page
    .getByRole('row', { name: /playground/ })
    .filter({ hasText: 'active' })
    .first()
  await stillLive.getByRole('button', { name: /edit limits for playground/i }).click()
  await page.getByLabel('Credit limit for playground').fill('5.00')
  await page.locator('.key-edit-row').getByRole('button', { name: /^save$/i }).click()
  await expect(page.locator('.key-edit-row')).toHaveCount(0)
  await expect(stillLive).toContainText('of $5.00/mo')
})

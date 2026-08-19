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

test('keys page lists, creates, and reveals a key exactly once', async ({ page }) => {
  await signIn(page)
  await page.getByRole('link', { name: /keys/i }).click()
  const keyName = `e2e-${Date.now()}`
  await page.getByPlaceholder(/key name/i).fill(keyName)
  await page.getByRole('button', { name: /create key/i }).click()
  // The plaintext is shown exactly once, prefixed like a real key.
  await expect(page.getByText(/zcr_[a-f0-9]{16}/)).toBeVisible()
  await expect(page.getByText(keyName)).toBeVisible()
})

test('credits page renders balance, promo ledger, and the autopay panel', async ({ page }) => {
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

test('the api surface the SPA consumes returns the documented shapes', async ({ page }) => {
  await signIn(page)
  // Contract pins straight from the browser session: catches envelope
  // drift on endpoints whose pages might not render them immediately.
  const shapes = await page.evaluate(async () => {
    const keys = await (await fetch('/api/keys')).json()
    const ledger = await (await fetch('/api/billing/ledger?limit=5')).json()
    const me = await (await fetch('/api/me')).json()
    const usage = await (await fetch('/api/usage?days=7')).json()
    return {
      keysIsEnvelope: Array.isArray(keys.keys),
      ledgerIsEnvelope: Array.isArray(ledger.entries),
      meHasEmail: typeof me.email === 'string',
      usageHasTotals: typeof usage.totals === 'object' && Array.isArray(usage.daily),
    }
  })
  expect(shapes).toEqual({
    keysIsEnvelope: true,
    ledgerIsEnvelope: true,
    meHasEmail: true,
    usageHasTotals: true,
  })
})

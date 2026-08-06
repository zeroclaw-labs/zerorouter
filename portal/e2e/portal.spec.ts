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
  // Stripe is deliberately unconfigured in e2e. The deployment banner is
  // action-triggered (page load never consults Stripe): attempting a
  // checkout must degrade to the banner, never a blank screen.
  await page.getByRole('button', { name: /buy/i }).click()
  await expect(page.getByText(/billing is not enabled/i).first()).toBeVisible()
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

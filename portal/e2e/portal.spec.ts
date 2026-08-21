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
  await expect(page.getByText(/zcr_[a-f0-9]{16}/)).toBeVisible()
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

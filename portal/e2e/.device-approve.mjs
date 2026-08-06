// Drives the portal's device-activation approval in a real browser:
// sign in through the mock IdP, enter the user code, approve.
import { chromium } from '@playwright/test'

const CODE = process.argv[2]
if (!CODE) throw new Error('usage: device-approve.mjs XXXX-XXXX')
const BASE = 'http://localhost:8080'

const browser = await chromium.launch()
const page = await browser.newPage()
await page.goto(`${BASE}/activate`)
await page.getByRole('link', { name: /sign in/i }).click()
await page.waitForURL(/localhost:8080/, { timeout: 15000 })
await page.goto(`${BASE}/activate`)
await page.getByLabel('Device code').fill(CODE)
await page.getByRole('button', { name: /continue|look up|submit/i }).click()
await page.getByRole('button', { name: /approve/i }).click()
await page.waitForTimeout(1500)
console.log('APPROVED')
await browser.close()

// Spawns the mock IdP and the real router (against real Postgres) for the
// Playwright suite. The router serves the built SPA itself, so the tests
// exercise the production serving path including the SPA fallback.
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '..', '..')

const IDP_PORT = 9400
const ROUTER_PORT = 9410
export const BASE_URL = `http://127.0.0.1:${ROUTER_PORT}`

async function waitFor(url, label, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try {
      const res = await fetch(url)
      if (res.ok) return
    } catch {}
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(`${label} did not come up at ${url}`)
}

export default async function globalSetup() {
  const dbUrl = process.env.DATABASE_URL
  if (!dbUrl) throw new Error('DATABASE_URL is required for the e2e suite')

  mkdirSync(path.join(here, '.state'), { recursive: true })

  const idp = spawn('node', [path.join(here, 'mock-idp.mjs')], {
    env: { ...process.env, IDP_PORT: String(IDP_PORT) },
    stdio: 'inherit',
  })
  await waitFor(`http://127.0.0.1:${IDP_PORT}/.well-known/openid-configuration`, 'mock idp')

  const router = spawn(path.join(repo, 'router', 'target', 'debug', 'zerorouter'), ['serve'], {
    env: {
      ...process.env,
      DATABASE_URL: dbUrl,
      ZEROROUTER_BIND: `127.0.0.1:${ROUTER_PORT}`,
      ZEROROUTER_PUBLIC_BASE_URL: BASE_URL,
      ZEROROUTER_PORTAL_DIST: path.resolve(here, '..', 'dist'),
      // Without this the router resolves the default `config/tiers.toml`
      // relative to ITS cwd — which is `portal/` here, where no such file
      // exists — so `/v1/models` answered "catalog unavailable" and the
      // storefront page rendered an error banner. Nothing asserted on that
      // page, so the gap was invisible until the catalog gained something
      // worth testing.
      ZEROROUTER_TIERS_PATH: path.resolve(repo, 'router', 'config', 'tiers.toml'),
      // `/v1/models` publishes only lanes whose provider credential is present,
      // so without these the storefront renders an EMPTY catalog — correctly,
      // and the retention assertions below would have nothing to read. These
      // stand for "the secret is provisioned"; the catalog route never dials an
      // upstream, so no value here leaves the process. They are deliberately
      // not plausible keys.
      //
      // This harness is therefore a FULLY credentialed deployment. The partial
      // case — the one that caused the incident, region set and
      // BEDROCK_API_KEY absent — is covered in `router/tests/http.rs`, where it
      // can be asserted precisely and without a browser.
      ANTHROPIC_API_KEY: 'not-a-real-key',
      OPENAI_API_KEY: 'not-a-real-key',
      GEMINI_API_KEY: 'not-a-real-key',
      BEDROCK_API_KEY: 'not-a-real-key',
      BEDROCK_REGION: 'us-east-1',
      OIDC_ISSUER_URL: `http://127.0.0.1:${IDP_PORT}`,
      OIDC_CLIENT_ID: 'e2e-portal',
      OIDC_CLIENT_SECRET: 'e2e-secret',
      RUST_LOG: 'warn',
    },
    stdio: 'inherit',
  })
  await waitFor(`${BASE_URL}/healthz`, 'router')

  writeFileSync(path.join(here, '.state', 'pids.json'), JSON.stringify({ idp: idp.pid, router: router.pid }))
}

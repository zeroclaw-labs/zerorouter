// Spawns the mock IdP and the real router (against real Postgres) for the
// Playwright suite. The router serves the built SPA itself, so the tests
// exercise the production serving path including the SPA fallback.
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import { recreateScratchDatabase } from './scratch-db.mjs'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '..', '..')

const IDP_PORT = 9400
const ROUTER_PORT = 9410
const MOCK_UPSTREAM_PORT = 9420
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

/// Readiness for a server that has no 200-answering route. The mock upstream
/// answers anything but POST with a 405, which is a perfectly good proof that
/// the socket is accepting — `waitFor` above would spin forever on it.
async function waitForSocket(url, label, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try {
      await fetch(url)
      return
    } catch {}
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(`${label} did not come up at ${url}`)
}

export default async function globalSetup() {
  const suppliedUrl = process.env.DATABASE_URL
  if (!suppliedUrl) throw new Error('DATABASE_URL is required for the e2e suite')

  // Every run starts from an empty database. `DATABASE_URL` names the server
  // and the database to derive a scratch name from; the scratch database beside
  // it is dropped and recreated here, and the supplied one is never written to.
  // See `scratch-db.mjs` for why the suite owns a database rather than
  // borrowing one — in short, the per-user key-mint throttle made an
  // accumulating database a hard limit on how many times the suite could run.
  const dbUrl = await recreateScratchDatabase(suppliedUrl)

  // Overwritten in this process so it is inherited by everything downstream:
  // the router spawned below, and — the part that is easy to miss — the
  // `zerorouter admin ...` invocations the specs make with `env: process.env`.
  // A spec that granted credit to the SUPPLIED database while the router read
  // the scratch one would fail in a thoroughly confusing way.
  process.env.DATABASE_URL = dbUrl

  mkdirSync(path.join(here, '.state'), { recursive: true })

  const idp = spawn('node', [path.join(here, 'mock-idp.mjs')], {
    env: { ...process.env, IDP_PORT: String(IDP_PORT) },
    stdio: 'inherit',
  })
  await waitFor(`http://127.0.0.1:${IDP_PORT}/.well-known/openid-configuration`, 'mock idp')

  // An OpenAI-compatible upstream the router can actually dispatch to, so the
  // playground spec can drive a completion end to end instead of asserting on a
  // page that never sends one. See `mock-upstream.mjs` for why the fake has to
  // sit on the wire rather than inside the process.
  const upstream = spawn('node', [path.join(here, 'mock-upstream.mjs')], {
    env: { ...process.env, MOCK_UPSTREAM_PORT: String(MOCK_UPSTREAM_PORT) },
    stdio: 'inherit',
  })
  await waitForSocket(`http://127.0.0.1:${MOCK_UPSTREAM_PORT}/`, 'mock upstream')

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
      // Added 2026-08-20 with the Qwen 3.8 Max lane, and it was MISSING before
      // that: the Fireworks lanes shipped the same day this list was last
      // described as "fully credentialed", so for the whole of that day the
      // storefront under test rendered six fewer rows than the product does and
      // the comment above was quietly false. Nothing failed, because the
      // retention assertions here only needed SOME zero-retention lane and the
      // Bedrock four supplied it. The per-tier override test below is the first
      // thing that names a Fireworks lane, and it could not pass without this.
      FIREWORKS_API_KEY: 'not-a-real-key',
      // Added 2026-08-20 with the two xAI Grok lanes, for exactly the reason
      // the paragraph above records — an upstream that ships without its key in
      // this list makes the "fully credentialed" claim false again and hides
      // its own rows from every assertion below.
      //
      // Note this key is a placeholder in a stronger sense than the others. The
      // xai lanes are the only ones whose dispatch asserts a per-response
      // retention attestation, so a real key from a team without ZDR enabled
      // would fail every request closed. Nothing here dials an upstream — the
      // catalog route is the whole surface under test — so the distinction does
      // not bite in this harness, but it is why this value must stay
      // implausible rather than becoming a real key someone pastes in.
      XAI_API_KEY: 'not-a-real-key',
      // Added 2026-08-21 with the three Vertex Gemini lanes, for the same
      // reason as the two paragraphs above.
      //
      // This pair is unlike every other credential here in a way worth
      // knowing before someone "fixes" it. `VERTEX_SERVICE_ACCOUNT` really
      // holds a Google service-account key in JSON, and this string is not
      // one — it could not be parsed, let alone exchanged for a token. That is
      // fine here and only here: `/v1/models` decides what to publish from
      // whether the variable is PRESENT, never from whether it works, so the
      // catalog surface under test renders all three rows. A harness that
      // dialled an upstream would need a real key, and would then be putting a
      // live GCP credential in a repository.
      //
      // `VERTEX_PROJECT_ID` is required alongside it — the endpoint carries the
      // project in its path, so without it these lanes drop out exactly as
      // Bedrock's do without `BEDROCK_REGION`.
      VERTEX_SERVICE_ACCOUNT: 'not-a-real-service-account-key',
      VERTEX_PROJECT_ID: 'zr-e2e-not-a-real-project',
      // Added 2026-08-22 with the three Groq and four Together lanes, for the
      // same reason as every paragraph above: an upstream missing from this
      // list publishes no rows and quietly narrows what the suite proves.
      //
      // These seven matter more to this harness than their count suggests.
      // They took the catalog's zero-retention lanes from a minority to a
      // majority (18 of 32), so `portal.spec.ts`'s footnote assertion is now
      // reading a sentence whose larger number comes mostly from providers
      // added here. A missing key would not fail anything — it would just make
      // the counted branch smaller and still true.
      GROQ_API_KEY: 'not-a-real-key',
      TOGETHER_API_KEY: 'not-a-real-key',
      // Bring-your-own-key. Present so the portal renders its BYOK section and
      // the attach flow can be driven; a fixed fixture value rather than a
      // generated one so a failing run is reproducible. Nothing here is
      // protecting a real secret — the keys the suite attaches are made up, and
      // no request in it dials an upstream.
      BYOK_ENCRYPTION_KEY: '1b'.repeat(32),
      // Point the `google` upstream at the mock above. This is the router's own
      // production test seam (`base_url_override`), and it is the ONLY thing in
      // this harness that changes the request path — the browser still presents
      // a real key, admission still reserves against a real balance, and
      // settlement still bills the usage the far end reported.
      //
      // `google` and not one of its neighbours, for two reasons worth keeping:
      // it rides the `chat_completions` adapter, which is the wire this mock
      // speaks, and it declares no per-response retention attestation. The xAI
      // lanes DO (`x-zero-data-retention: true`), so pointing this at them would
      // fail every request closed and the failure would look like a bug in the
      // playground rather than a mock that cannot make the claim.
      ZEROROUTER_PROVIDER_BASE_URL_GOOGLE: `http://127.0.0.1:${MOCK_UPSTREAM_PORT}/v1/chat/completions`,
      OIDC_ISSUER_URL: `http://127.0.0.1:${IDP_PORT}`,
      OIDC_CLIENT_ID: 'e2e-portal',
      OIDC_CLIENT_SECRET: 'e2e-secret',
      RUST_LOG: 'warn',
    },
    stdio: 'inherit',
  })
  await waitFor(`${BASE_URL}/healthz`, 'router')

  writeFileSync(
    path.join(here, '.state', 'pids.json'),
    JSON.stringify({ idp: idp.pid, router: router.pid, upstream: upstream.pid }),
  )
}

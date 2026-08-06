// Zero-dependency mock OIDC IdP speaking the Zitadel dialect the router
// meets in production: RS256 id tokens whose `aud` carries the client id
// AND a project id (the multi-audience shape that broke the first live
// login), email + email_verified embedded in the id token, auto-approving
// authorize endpoint (no UI). E2E-only; never reachable from outside.
import http from 'node:http'
import crypto from 'node:crypto'

const PORT = Number(process.env.IDP_PORT ?? 9400)
const CLIENT_ID = process.env.IDP_CLIENT_ID ?? 'e2e-portal'
const EMAIL = process.env.IDP_EMAIL ?? 'e2e@zerorouter.test'
const ISSUER = `http://127.0.0.1:${PORT}`
const PROJECT_AUD = 'e2e-project-1234567890'

const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 })
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'e2e-key', alg: 'RS256', use: 'sig' }

const b64u = (buf) => Buffer.from(buf).toString('base64url')
function signJwt(payload) {
  const header = b64u(JSON.stringify({ alg: 'RS256', kid: 'e2e-key', typ: 'JWT' }))
  const body = b64u(JSON.stringify(payload))
  const signature = crypto.createSign('RSA-SHA256').update(`${header}.${body}`).sign(privateKey)
  return `${header}.${body}.${b64u(signature)}`
}

const codes = new Map()

const server = http.createServer((req, res) => {
  const url = new URL(req.url, ISSUER)
  const json = (status, body) => {
    res.writeHead(status, { 'content-type': 'application/json' })
    res.end(JSON.stringify(body))
  }

  if (url.pathname === '/.well-known/openid-configuration') {
    return json(200, {
      issuer: ISSUER,
      authorization_endpoint: `${ISSUER}/authorize`,
      token_endpoint: `${ISSUER}/token`,
      jwks_uri: `${ISSUER}/jwks`,
      response_types_supported: ['code'],
      subject_types_supported: ['public'],
      id_token_signing_alg_values_supported: ['RS256'],
      scopes_supported: ['openid', 'email', 'profile'],
      token_endpoint_auth_methods_supported: ['client_secret_basic', 'client_secret_post'],
      claims_supported: ['sub', 'email', 'email_verified'],
      grant_types_supported: ['authorization_code'],
    })
  }
  if (url.pathname === '/jwks') return json(200, { keys: [jwk] })

  if (url.pathname === '/authorize') {
    const redirect = url.searchParams.get('redirect_uri')
    const state = url.searchParams.get('state')
    const nonce = url.searchParams.get('nonce') ?? ''
    const code = crypto.randomUUID()
    codes.set(code, { nonce })
    const target = new URL(redirect)
    target.searchParams.set('code', code)
    target.searchParams.set('state', state)
    res.writeHead(302, { location: target.toString() })
    return res.end()
  }

  if (url.pathname === '/token' && req.method === 'POST') {
    let raw = ''
    req.on('data', (chunk) => (raw += chunk))
    req.on('end', () => {
      const form = new URLSearchParams(raw)
      const stored = codes.get(form.get('code'))
      if (!stored) return json(400, { error: 'invalid_grant' })
      codes.delete(form.get('code'))
      const now = Math.floor(Date.now() / 1000)
      const idToken = signJwt({
        iss: ISSUER,
        sub: `e2e-sub-${EMAIL}`,
        aud: [CLIENT_ID, PROJECT_AUD],
        azp: CLIENT_ID,
        exp: now + 300,
        iat: now,
        nonce: stored.nonce,
        email: EMAIL,
        email_verified: true,
      })
      return json(200, {
        access_token: 'e2e-access-token',
        token_type: 'Bearer',
        expires_in: 300,
        id_token: idToken,
      })
    })
    return
  }

  json(404, { error: 'not_found' })
})

server.listen(PORT, '127.0.0.1', () => console.log(`mock-idp listening on ${ISSUER}`))

// Typed same-origin API client for the ZeroRouter portal.
//
// Every mutating request carries the `x-zerorouter-portal` CSRF header (a
// cross-site form post cannot set it). A 401 with code `session_required`
// flips the app into its signed-out state via the registered handler.

const CSRF_HEADER = 'x-zerorouter-portal'

export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

export interface Me {
  user_id: string
  email: string
  credit_balance_usd: string
  created_at: string
  /** Stripe publishable key for this deployment, or null when billing is off.
   * Server-provided rather than baked into the bundle: it differs between the
   * test and live Stripe accounts, so a hardcoded key would be wrong for one
   * of them. Publishable by design — Stripe.js sends it from the browser. */
  stripe_publishable_key: string | null
  /** Upstream providers this deployment will hold your own API key for.
   * EMPTY when bring-your-own-key is not configured here — which is how the
   * feature ships dark: the Keys page renders no BYOK section at all rather
   * than a form whose every submission would be refused. Optional in the type
   * so an older router cannot crash this page. */
  byok_providers?: string[]
  /** Where you stand against this month's free BYOK allowance. ABSENT when
   * bring-your-own-key is not configured here, on the same contract as the
   * empty provider list above.
   *
   * All three figures come from the server rather than being derived here. The
   * allowance is a number ZeroRouter chose and may revise, so a portal that
   * subtracted against its own hardcoded copy would keep showing the old
   * promise after a change — and this panel is where a customer reads what
   * they will be billed. */
  byok_allowance?: ByokAllowance
  /** Whether this deployment offers paying with stablecoins. FALSE (or absent,
   * on a router older than the feature) renders no crypto option at all — the
   * dark-ship contract, the same one `byok_providers` uses. Optional in the
   * type so an older router cannot crash this page. */
  crypto_rail?: boolean
}

/** This month's free BYOK allowance, as `/api/me` reports it. Amounts are
 * decimal strings for the same reason every other money field here is: a
 * JavaScript number cannot hold the router's exact `Decimal`. */
export interface ByokAllowance {
  allowance_usd: string
  /** Catalog-equivalent BYOK usage settled this UTC month. Keeps growing past
   * the allowance — it is a usage figure, not a countdown. */
  consumed_usd: string
  /** What is left, floored at zero. */
  remaining_usd: string
}

/** One attached provider key, as the portal is ever allowed to see it.
 *
 * There is deliberately no field for the key itself. The server returns the
 * plaintext at no point — not even in the response to the request that
 * attached it — so a customer who loses it re-pastes rather than recovers it. */
export interface ByokKey {
  provider: string
  /** A truncated SHA-256 of the key. An identifier for support and for your
   * own eyes; it cannot be turned back into the key. */
  fingerprint: string
  /** The last four characters, matching what the provider's dashboard shows. */
  last4: string
  created_at: string
  last_used_at: string | null
  /** Whether a failure on this key is retried on ZeroRouter's own credential.
   * FALSE by default and for every key attached before the option existed.
   * Those retries bill at the FULL catalog price, not the BYOK fee, and do not
   * draw on the monthly allowance — which is why the control that sets this
   * says so on itself. */
  fallback_enabled: boolean
}

/** The reset cadences a key's credit limit can use; `null` never resets. */
export type CreditLimitWindow = 'daily' | 'weekly' | 'monthly'

export interface ApiKey {
  id: string
  name: string
  disabled: boolean
  spend_cap_usd: string | null
  velocity_cap_tokens_per_min: number | null
  /** When the key stops working; null never expires. */
  expires_at: string | null
  /** The limit set on this key; null is unlimited. */
  credit_limit_usd: string | null
  /** How often the limit resets; null means it never does. */
  credit_limit_window: CreditLimitWindow | null
  /** Spend counted against the limit in the CURRENT window; null when there
   * is no limit. Server-computed from the same counters that enforce it, so
   * this page never shows a number that disagrees with the gate. */
  credit_limit_used_usd: string | null
  created_at: string
  last_used_at: string | null
}

/** What the create-key dialog sends. Everything but the name is optional, and
 * omitting all of it mints exactly the key this portal minted before limits
 * existed: no expiry, no cap. */
export interface NewKey {
  name: string
  /** An absolute instant, not a preset — the dialog resolves its presets
   * against the clock and sends the result, so what the customer was shown is
   * what the server records. */
  expires_at?: string
  credit_limit_usd?: string
  credit_limit_window?: CreditLimitWindow
}

export interface CreatedKey extends ApiKey {
  api_key: string
}

export interface UsageTotals {
  requests: number
  input_tokens: number
  output_tokens: number
  cost_usd: string
}

export interface UsageDay {
  date: string
  requests: number
  cost_usd: string
}

export interface UsageEvent {
  request_id: string
  ts: string
  tier: string | null
  upstream_provider: string
  upstream_model: string
  input_tokens: number
  output_tokens: number
  cost_usd: string
  latency_ms: number
  status: string
  key_name: string | null
}

export interface Usage {
  totals: UsageTotals
  daily: UsageDay[]
  recent: UsageEvent[]
}

export interface LedgerEntry {
  id: string
  created_at: string
  entry_type: string
  amount_usd: string
  balance_after_usd: string
  note: string | null
  /** True when the request behind a usage entry ran on your own provider key
   * and was therefore charged the 5% fee rather than the catalog price. Null
   * on entries that are not usage, and on usage from before BYOK existed. */
  byok?: boolean | null
}

/** A Stripe-hosted page to redirect to. Still the shape of the autopay card
 * setup, which remains a redirect; credit purchase no longer uses it. */
export interface RedirectSession {
  url: string
}

/** An embedded Checkout Session: the secret the browser mounts the payment
 * form with. There is no url — an `embedded_page` session has none. */
export interface CheckoutClientSecret {
  client_secret: string
}

/** Display-only status of a Checkout Session. `complete` means Stripe took the
 * payment; it does NOT mean credit has landed. Crediting is webhook-driven and
 * this value never causes it — see `router/src/stripe.rs`. */
export interface CheckoutStatus {
  status: 'complete' | 'open' | 'expired' | string
}

/** One row of the public catalog (`GET /v1/models`). Prices are decimal
 * strings in USD **per single token**; the UI renders them per-Mtok. Metadata
 * fields are optional — absent means the catalog does not publish one, never a
 * default. */
/** One repricing band from `/v1/models`, in OpenRouter's `pricing.overrides[]`
 * shape. Several vendors reprice long-context requests, and the repricing is a
 * STEP rather than a margin: at or above `min_prompt_tokens` these rates
 * replace the base table for the WHOLE request, input and output alike.
 *
 * The portal reads these because the playground quotes a cost back to the
 * customer, and quoting only the base rate would understate a long-prompt
 * request by half on four of the catalog's ten models. */
export interface PricingOverride {
  min_prompt_tokens: number
  prompt: string
  completion: string
  input_cache_read?: string
}

export interface Model {
  id: string
  owned_by: string
  pricing: {
    prompt: string
    completion: string
    input_cache_read?: string
    /** Absent (not empty) on a model that charges one price at every size. */
    overrides?: PricingOverride[]
  }
  context_length?: number | null
  max_output_tokens?: number | null
  input_modalities?: string[] | null
  tool_call?: boolean | null
  /** What the upstream serving this model does with the request afterwards.
   * Unlike the metadata fields above, this is never absent — the router refuses
   * to load a catalog with an unlabelled lane, so a row without it means the
   * response did not come from a ZeroRouter that understands retention. It is
   * typed optional anyway so an older router cannot crash this page; the
   * renderer shows "unstated" rather than guessing the favourable answer. */
  retention?: { posture: 'zero' | 'standard' | string; description: string; verified: string }
}

/** Which payment rail a deposit is priced on and payable with. The two are one
 * decision on the server: a crypto-priced session only accepts stablecoin and a
 * card-priced one never does. */
export type Rail = 'card' | 'crypto'

/** A server-priced deposit: the credit picked, the fee on top, and the gross
 * Stripe collects. All are decimal strings — the fee is never computed in TS.
 *
 * The fee DIFFERS by rail (5% flat for crypto, 5.5% with a $0.80 minimum for
 * card), which is exactly why this page must never derive it: the only correct
 * fee is the one the server quoted for the rail the customer chose. */
export interface Quote {
  credit: string
  fee: string
  gross: string
  /** Echoed back so a late-arriving quote can be matched to the rail it was
   * asked for, rather than being rendered against whichever rail is selected by
   * the time it lands. */
  rail?: Rail
}

export interface AutopayStatus {
  enabled: boolean
  threshold_usd: string | null
  topup_usd: string | null
  consecutive_failures: number
  card_setup_started: boolean
}

export interface AutopayUpdate {
  enabled: boolean
  threshold_usd?: string
  topup_usd?: string
}

export interface DeviceLookup {
  client_id: string
  key_name: string
  created_at: string
}

let sessionRequiredHandler: (() => void) | null = null

/** Register the app-level reaction to a lost session (signed-out flip). */
export function onSessionRequired(handler: () => void): void {
  sessionRequiredHandler = handler
}

function errorDetail(payload: unknown): { code?: string; message?: string } {
  if (typeof payload === 'object' && payload !== null && 'error' in payload) {
    const err = (payload as { error: unknown }).error
    if (typeof err === 'object' && err !== null) {
      const { code, message } = err as { code?: unknown; message?: unknown }
      return {
        code: typeof code === 'string' ? code : undefined,
        message: typeof message === 'string' ? message : undefined,
      }
    }
  }
  return {}
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {}
  if (method !== 'GET') headers[CSRF_HEADER] = '1'
  if (body !== undefined) headers['content-type'] = 'application/json'

  let res: Response
  try {
    res = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  } catch {
    throw new ApiError(0, 'network_error', 'Could not reach the server. Check your connection and try again.')
  }

  let payload: unknown = null
  const text = await res.text()
  if (text.length > 0) {
    try {
      payload = JSON.parse(text)
    } catch {
      payload = null
    }
  }

  if (!res.ok) {
    const detail = errorDetail(payload)
    const code = detail.code ?? `http_${res.status}`
    if (res.status === 401 && code === 'session_required') {
      sessionRequiredHandler?.()
    }
    throw new ApiError(res.status, code, detail.message ?? `Request failed (${res.status}).`)
  }
  return payload as T
}

export const api = {
  me: () => request<Me>('GET', '/api/me'),
  // The server wraps list responses in envelopes; unwrap here so pages
  // consume bare arrays (first caught live: the SPA's data pages were
  // untestable in a browser until OIDC existed).
  keys: () => request<{ keys: ApiKey[] }>('GET', '/api/keys').then((r) => r.keys),
  createKey: (key: NewKey) => request<CreatedKey>('POST', '/api/keys', key),
  deleteKey: (id: string) => request<void>('DELETE', `/api/keys/${encodeURIComponent(id)}`),
  // The public catalog — same endpoint any OpenAI-compatible client reads, so
  // the storefront shows exactly what callers get. No auth: a prospective
  // customer can see models and prices before signing in.
  models: () => request<{ data: Model[] }>('GET', '/v1/models').then((r) => r.data),
  usage: (days: number) => request<Usage>('GET', `/api/usage?days=${days}`),
  byokKeys: () => request<{ keys: ByokKey[] }>('GET', '/api/byok').then((r) => r.keys),
  attachByokKey: (provider: string, apiKey: string) =>
    request<ByokKey>('POST', '/api/byok', { provider, api_key: apiKey }),
  removeByokKey: (provider: string) =>
    request<void>('DELETE', `/api/byok/${encodeURIComponent(provider)}`),
  // Sends the state it wants rather than "flip it", so a retried request lands
  // on the same setting the customer clicked instead of its opposite.
  setByokFallback: (provider: string, enabled: boolean) =>
    request<void>('PATCH', `/api/byok/${encodeURIComponent(provider)}`, { enabled }),
  ledger: (limit: number) =>
    request<{ limit: number; entries: LedgerEntry[] }>(
      'GET',
      `/api/billing/ledger?limit=${limit}`,
    ).then((r) => r.entries),
  // `rail` is omitted for the card path so an unchanged request body reaches an
  // unchanged server path — the crypto rail is additive on the wire too.
  checkout: (amountUsd: string, rail: Rail = 'card') =>
    request<CheckoutClientSecret>(
      'POST',
      '/api/billing/checkout',
      rail === 'card' ? { amount_usd: amountUsd } : { amount_usd: amountUsd, rail },
    ),
  checkoutStatus: (sessionId: string) =>
    request<CheckoutStatus>(
      'GET',
      `/api/billing/checkout/status?session_id=${encodeURIComponent(sessionId)}`,
    ),
  quote: (creditUsd: string, rail: Rail = 'card') =>
    request<Quote>(
      'GET',
      `/api/billing/quote?credit=${encodeURIComponent(creditUsd)}&rail=${rail}`,
    ),
  autopay: () => request<AutopayStatus>('GET', '/api/billing/autopay'),
  putAutopay: (update: AutopayUpdate) =>
    request<AutopayStatus>('PUT', '/api/billing/autopay', update),
  autopaySetup: () => request<RedirectSession>('POST', '/api/billing/autopay/setup'),
  // Ensure this account holds exactly one live key named "playground" and hand
  // back its plaintext. Idempotent in STATE, not in secret: the server keeps
  // only a digest, so it cannot return a key it already minted, and each call
  // replaces the previous one. Call it when the browser finds it has no key —
  // never on every page load, or the account's creation throttle pays for it.
  ensurePlaygroundKey: () => request<CreatedKey>('POST', '/api/playground/key'),
  deviceLookup: (userCode: string) =>
    request<DeviceLookup>('POST', '/api/device/lookup', { user_code: userCode }),
  deviceApprove: (userCode: string) =>
    request<void>('POST', '/api/device/approve', { user_code: userCode }),
  deviceDeny: (userCode: string) => request<void>('POST', '/api/device/deny', { user_code: userCode }),
  logout: () => request<void>('POST', '/auth/logout'),
}

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
}

export interface ApiKey {
  id: string
  name: string
  disabled: boolean
  spend_cap_usd: string | null
  velocity_cap_tokens_per_min: number | null
  created_at: string
  last_used_at: string | null
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
}

export interface CheckoutSession {
  url: string
}

/** A server-priced deposit: the credit picked, the fee on top, and the gross
 * Stripe collects. All are decimal strings — the fee is never computed in TS. */
export interface Quote {
  credit: string
  fee: string
  gross: string
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
  createKey: (name: string) => request<CreatedKey>('POST', '/api/keys', { name }),
  deleteKey: (id: string) => request<void>('DELETE', `/api/keys/${encodeURIComponent(id)}`),
  usage: (days: number) => request<Usage>('GET', `/api/usage?days=${days}`),
  ledger: (limit: number) =>
    request<{ limit: number; entries: LedgerEntry[] }>(
      'GET',
      `/api/billing/ledger?limit=${limit}`,
    ).then((r) => r.entries),
  checkout: (amountUsd: string) =>
    request<CheckoutSession>('POST', '/api/billing/checkout', { amount_usd: amountUsd }),
  quote: (creditUsd: string) =>
    request<Quote>('GET', `/api/billing/quote?credit=${encodeURIComponent(creditUsd)}`),
  autopay: () => request<AutopayStatus>('GET', '/api/billing/autopay'),
  putAutopay: (update: AutopayUpdate) =>
    request<AutopayStatus>('PUT', '/api/billing/autopay', update),
  autopaySetup: () => request<CheckoutSession>('POST', '/api/billing/autopay/setup'),
  deviceLookup: (userCode: string) =>
    request<DeviceLookup>('POST', '/api/device/lookup', { user_code: userCode }),
  deviceApprove: (userCode: string) =>
    request<void>('POST', '/api/device/approve', { user_code: userCode }),
  deviceDeny: (userCode: string) => request<void>('POST', '/api/device/deny', { user_code: userCode }),
  logout: () => request<void>('POST', '/auth/logout'),
}

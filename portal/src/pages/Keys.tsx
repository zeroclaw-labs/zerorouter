import { useState } from 'react'
import type { FormEvent } from 'react'
import { api } from '../api'
import type { ApiKey, CreatedKey, CreditLimitWindow, NewKey } from '../api'
import { Link } from 'react-router-dom'
import { EXAMPLE_MODEL } from './Docs'
import {
  Badge,
  Banner,
  CodeBlock,
  CopyButton,
  EmptyState,
  Loading,
  Modal,
  formatInt,
  formatTime,
  formatUsd,
  useLoad,
  useToast,
} from '../ui'

/** The expiry presets, in minutes. `null` is "never" and sends no expiry at
 * all — the same request this form sent before expiry existed.
 *
 * Presets live here rather than on the server on purpose: "1 week" only means
 * something relative to a clock, and resolving it in the dialog is what lets
 * the customer see the exact date before they commit to it. The wire carries
 * the resolved instant. */
const EXPIRY_PRESETS: ReadonlyArray<{ label: string; minutes: number | null }> = [
  { label: 'Never', minutes: null },
  { label: '1 hour', minutes: 60 },
  { label: '1 day', minutes: 60 * 24 },
  { label: '1 week', minutes: 60 * 24 * 7 },
  { label: '1 month', minutes: 60 * 24 * 30 },
]

const RESET_WINDOWS: ReadonlyArray<{ label: string; value: CreditLimitWindow | '' }> = [
  { label: 'Never resets', value: '' },
  { label: 'Daily', value: 'daily' },
  { label: 'Weekly', value: 'weekly' },
  { label: 'Monthly', value: 'monthly' },
]

function expiryFromPreset(minutes: number | null): Date | null {
  return minutes === null ? null : new Date(Date.now() + minutes * 60_000)
}

/** The request this key is for, ready to paste.
 *
 * The moment a key exists is the moment its owner wants to try it, and until
 * now this dialog handed over a secret and left them to guess the base URL and
 * the auth scheme. The key is inlined rather than referenced by an environment
 * variable on purpose: this panel is the only time the plaintext is ever shown,
 * so a command that needs it exported first is a command that cannot be run
 * from here.
 *
 * The origin comes from the browser rather than a constant, so a self-hosted
 * deployment gets its own hostname instead of ours. */
function curlFor(apiKey: string): string {
  const origin = typeof window === 'undefined' ? '' : window.location.origin
  return `curl ${origin}/v1/chat/completions \\
  -H "Authorization: Bearer ${apiKey}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${EXAMPLE_MODEL}",
    "messages": [{"role": "user", "content": "Say hello in five words."}]
  }'`
}

/** How a key's expiry reads in the table: the date, or how it ended. */
function expiryLabel(key: ApiKey): string {
  if (key.expires_at === null) return 'never'
  return formatTime(key.expires_at)
}

function hasExpired(key: ApiKey): boolean {
  return key.expires_at !== null && new Date(key.expires_at).getTime() <= Date.now()
}

/** The limit column: what was set, how much of it is gone, and how it resets. */
function limitLabel(key: ApiKey): string {
  if (key.credit_limit_usd === null) return 'no limit'
  const used = formatUsd(key.credit_limit_used_usd ?? '0')
  const limit = formatUsd(key.credit_limit_usd)
  const cadence = key.credit_limit_window === null ? '' : `/${key.credit_limit_window.slice(0, 2)}`
  return `${used} of ${limit}${cadence}`
}

export function Keys() {
  const toast = useToast()
  const keys = useLoad(() => api.keys(), [])
  const [name, setName] = useState('')
  const [expiryMinutes, setExpiryMinutes] = useState<number | null>(null)
  const [creditLimit, setCreditLimit] = useState('')
  const [resetWindow, setResetWindow] = useState<CreditLimitWindow | ''>('')
  const [creating, setCreating] = useState(false)
  const [created, setCreated] = useState<CreatedKey | null>(null)
  const [confirming, setConfirming] = useState<string | null>(null)
  const [revoking, setRevoking] = useState<string | null>(null)

  const expiresAt = expiryFromPreset(expiryMinutes)

  async function create(event: FormEvent) {
    event.preventDefault()
    const trimmed = name.trim()
    if (trimmed === '') {
      toast('Name the key first — try the machine or project it belongs to.', 'error')
      return
    }
    const limit = creditLimit.trim()
    if (limit !== '' && !(Number(limit) > 0)) {
      toast('A credit limit must be a positive dollar amount, or blank for none.', 'error')
      return
    }
    // A reset cadence with nothing to reset would mint an unlimited key, which
    // is the opposite of what someone choosing a cadence wants. The server
    // refuses it too; catching it here says so without a round trip.
    if (limit === '' && resetWindow !== '') {
      toast('Set a credit limit, or change the reset to “Never resets”.', 'error')
      return
    }
    // Only the fields the customer actually set are sent. A blank form
    // therefore posts exactly `{ name }` — the request this page made before
    // any of these fields existed.
    const body: NewKey = { name: trimmed }
    if (expiresAt !== null) body.expires_at = expiresAt.toISOString()
    if (limit !== '') body.credit_limit_usd = limit
    if (resetWindow !== '') body.credit_limit_window = resetWindow
    setCreating(true)
    try {
      const key = await api.createKey(body)
      setCreated(key)
      setName('')
      setExpiryMinutes(null)
      setCreditLimit('')
      setResetWindow('')
      keys.reload()
    } catch (err) {
      toast(err instanceof Error ? err.message : 'Could not create the key.', 'error')
    } finally {
      setCreating(false)
    }
  }

  async function revoke(id: string) {
    setRevoking(id)
    try {
      await api.deleteKey(id)
      toast('Key revoked.', 'success')
      keys.reload()
    } catch (err) {
      toast(err instanceof Error ? err.message : 'Could not revoke the key.', 'error')
    } finally {
      setRevoking(null)
      setConfirming(null)
    }
  }

  return (
    <div className="page">
      <header className="page-head">
        <h1>API keys</h1>
        <p className="page-sub">Keys authenticate requests to the inference API and bill this account</p>
      </header>

      <section className="panel">
        <div className="panel-head">
          <h2>Keys</h2>
        </div>
        <form className="create-row create-key" onSubmit={create}>
          <input
            className="field"
            value={name}
            maxLength={64}
            placeholder="Key name — e.g. laptop, ci, prod-agent"
            aria-label="New key name"
            onChange={(e) => setName(e.target.value)}
          />
          <select
            className="field"
            aria-label="Expiration"
            value={expiryMinutes === null ? '' : String(expiryMinutes)}
            onChange={(e) => setExpiryMinutes(e.target.value === '' ? null : Number(e.target.value))}
          >
            {EXPIRY_PRESETS.map((preset) => (
              <option key={preset.label} value={preset.minutes === null ? '' : String(preset.minutes)}>
                {preset.label}
              </option>
            ))}
          </select>
          <input
            className="field"
            value={creditLimit}
            inputMode="decimal"
            placeholder="Credit limit (USD)"
            aria-label="Credit limit in dollars"
            onChange={(e) => setCreditLimit(e.target.value)}
          />
          <select
            className="field"
            aria-label="Reset limit every"
            value={resetWindow}
            onChange={(e) => setResetWindow(e.target.value as CreditLimitWindow | '')}
          >
            {RESET_WINDOWS.map((window) => (
              <option key={window.label} value={window.value}>
                {window.label}
              </option>
            ))}
          </select>
          <button className="btn btn-primary" type="submit" disabled={creating}>
            {creating ? 'Creating…' : 'Create key'}
          </button>
        </form>
        <p className="field-hint create-key-hint">
          {expiresAt === null
            ? 'This key will not expire.'
            : `Expires ${formatTime(expiresAt.toISOString())}.`}{' '}
          Leave the credit limit blank for unlimited.
        </p>

        {keys.loading ? (
          <Loading />
        ) : keys.error !== null ? (
          <div className="panel-body">
            <Banner kind="error">{keys.error}</Banner>
          </div>
        ) : keys.data === null || keys.data.length === 0 ? (
          <EmptyState
            title="No API keys yet"
            hint="Create a key above to authenticate requests to /v1/chat/completions."
          />
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Created</th>
                <th>Last used</th>
                <th>Expires</th>
                <th>Limit</th>
                <th>Caps</th>
                <th>Status</th>
                <th className="num" aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              {keys.data.map((k) => (
                <tr key={k.id}>
                  <td>{k.name}</td>
                  <td className="dim nowrap">{formatTime(k.created_at)}</td>
                  <td className="dim nowrap">{k.last_used_at !== null ? formatTime(k.last_used_at) : 'never'}</td>
                  <td className="dim mono nowrap">{expiryLabel(k)}</td>
                  <td className="dim mono nowrap">{limitLabel(k)}</td>
                  <td className="dim mono nowrap">
                    {k.spend_cap_usd !== null ? `${formatUsd(k.spend_cap_usd)}/mo` : 'no cap'}
                    {k.velocity_cap_tokens_per_min !== null
                      ? ` · ${formatInt(k.velocity_cap_tokens_per_min)} tok/min`
                      : ''}
                  </td>
                  <td>
                    {k.disabled ? (
                      <Badge tone="neutral">disabled</Badge>
                    ) : hasExpired(k) ? (
                      <Badge tone="neutral">expired</Badge>
                    ) : (
                      <Badge tone="good">active</Badge>
                    )}
                  </td>
                  <td className="num">
                    {confirming === k.id ? (
                      <span className="confirm-row">
                        <button
                          type="button"
                          className="btn btn-danger btn-sm"
                          disabled={revoking === k.id}
                          onClick={() => void revoke(k.id)}
                        >
                          {revoking === k.id ? 'Revoking…' : 'Confirm revoke'}
                        </button>
                        <button
                          type="button"
                          className="btn btn-ghost btn-sm"
                          disabled={revoking === k.id}
                          onClick={() => setConfirming(null)}
                        >
                          Cancel
                        </button>
                      </span>
                    ) : (
                      <button type="button" className="btn btn-ghost btn-sm" onClick={() => setConfirming(k.id)}>
                        Revoke
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      {created !== null && (
        <Modal title="API key created">
          <p className="modal-warn">
            This is the only time the full key is shown. Store it now — the server keeps only a hash.
          </p>
          <div className="keybox mono">{created.api_key}</div>
          <div className="keybox-curl">
            <CodeBlock label="Try it">{curlFor(created.api_key)}</CodeBlock>
          </div>
          <p className="field-hint">
            Any OpenAI-compatible client works the same way — see the{' '}
            <Link to="/docs">API docs</Link> for the SDK examples, streaming, and the error codes.
          </p>
          <div className="modal-actions">
            <CopyButton text={created.api_key} label="Copy key" />
            <button type="button" className="btn btn-primary" onClick={() => setCreated(null)}>
              I&rsquo;ve stored it
            </button>
          </div>
        </Modal>
      )}
    </div>
  )
}

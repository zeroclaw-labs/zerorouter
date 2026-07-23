import { useState } from 'react'
import type { FormEvent } from 'react'
import { api } from '../api'
import type { CreatedKey } from '../api'
import {
  Badge,
  Banner,
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

export function Keys() {
  const toast = useToast()
  const keys = useLoad(() => api.keys(), [])
  const [name, setName] = useState('')
  const [creating, setCreating] = useState(false)
  const [created, setCreated] = useState<CreatedKey | null>(null)
  const [confirming, setConfirming] = useState<string | null>(null)
  const [revoking, setRevoking] = useState<string | null>(null)

  async function create(event: FormEvent) {
    event.preventDefault()
    const trimmed = name.trim()
    if (trimmed === '') {
      toast('Name the key first — try the machine or project it belongs to.', 'error')
      return
    }
    setCreating(true)
    try {
      const key = await api.createKey(trimmed)
      setCreated(key)
      setName('')
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
        <form className="create-row" onSubmit={create}>
          <input
            className="field"
            value={name}
            maxLength={64}
            placeholder="Key name — e.g. laptop, ci, prod-agent"
            aria-label="New key name"
            onChange={(e) => setName(e.target.value)}
          />
          <button className="btn btn-primary" type="submit" disabled={creating}>
            {creating ? 'Creating…' : 'Create key'}
          </button>
        </form>

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
                <th>Limits</th>
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
                  <td className="dim mono nowrap">
                    {k.spend_cap_usd !== null ? `${formatUsd(k.spend_cap_usd)}/mo` : 'no cap'}
                    {k.velocity_cap_tokens_per_min !== null
                      ? ` · ${formatInt(k.velocity_cap_tokens_per_min)} tok/min`
                      : ''}
                  </td>
                  <td>
                    {k.disabled ? <Badge tone="neutral">disabled</Badge> : <Badge tone="good">active</Badge>}
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

import { useState } from 'react'
import type { FormEvent } from 'react'
import { useSearchParams } from 'react-router'
import { api } from '../api'
import type { DeviceLookup } from '../api'
import { Banner, formatTime } from '../ui'

type Stage =
  | { kind: 'enter' }
  | { kind: 'confirm'; device: DeviceLookup }
  | { kind: 'done'; approved: boolean }

/** Uppercase, strip non-alphanumerics, cap at 8 chars, dash after 4: XXXX-XXXX. */
function formatUserCode(raw: string): string {
  const chars = raw
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, '')
    .slice(0, 8)
  return chars.length > 4 ? `${chars.slice(0, 4)}-${chars.slice(4)}` : chars
}

export function Activate() {
  const [searchParams] = useSearchParams()
  const [code, setCode] = useState(() => formatUserCode(searchParams.get('code') ?? ''))
  const [stage, setStage] = useState<Stage>({ kind: 'enter' })
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function lookup(event: FormEvent) {
    event.preventDefault()
    if (code.length !== 9) {
      setError('Enter the 8-character code shown in your terminal.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      const device = await api.deviceLookup(code)
      setStage({ kind: 'confirm', device })
    } catch (err) {
      setError(err instanceof Error ? err.message : 'That code was not recognized.')
    } finally {
      setBusy(false)
    }
  }

  async function decide(approved: boolean) {
    setBusy(true)
    setError(null)
    try {
      if (approved) {
        await api.deviceApprove(code)
      } else {
        await api.deviceDeny(code)
      }
      setStage({ kind: 'done', approved })
    } catch (err) {
      setError(err instanceof Error ? err.message : 'The request could not be completed.')
    } finally {
      setBusy(false)
    }
  }

  function reset() {
    setStage({ kind: 'enter' })
    setCode('')
    setError(null)
  }

  return (
    <div className="activate">
      {stage.kind === 'enter' && (
        <div className="activate-card">
          <h1>Connect a device</h1>
          <p className="page-sub">Enter the code shown in your terminal.</p>
          <form onSubmit={lookup}>
            <input
              className="code-input"
              value={code}
              autoFocus
              spellCheck={false}
              autoComplete="off"
              placeholder="XXXX-XXXX"
              aria-label="Device code"
              onChange={(e) => {
                setCode(formatUserCode(e.target.value))
                setError(null)
              }}
            />
            {error !== null && <Banner kind="error">{error}</Banner>}
            <button className="btn btn-primary" type="submit" disabled={busy || code.length !== 9}>
              {busy ? 'Checking…' : 'Continue'}
            </button>
          </form>
        </div>
      )}

      {stage.kind === 'confirm' && (
        <div className="activate-card">
          <h1>Approve this device?</h1>
          <dl className="device-facts">
            <div>
              <dt>Client</dt>
              <dd className="mono">{stage.device.client_id}</dd>
            </div>
            <div>
              <dt>Key name</dt>
              <dd>{stage.device.key_name}</dd>
            </div>
            <div>
              <dt>Requested</dt>
              <dd>{formatTime(stage.device.created_at)}</dd>
            </div>
          </dl>
          <p className="caution">
            Approving mints a new API key for this device, billed to your credit balance. Only approve a request
            you started yourself.
          </p>
          {error !== null && <Banner kind="error">{error}</Banner>}
          <div className="modal-actions">
            <button type="button" className="btn btn-ghost" onClick={() => void decide(false)} disabled={busy}>
              Deny
            </button>
            <button type="button" className="btn btn-primary" onClick={() => void decide(true)} disabled={busy}>
              Approve
            </button>
          </div>
        </div>
      )}

      {stage.kind === 'done' && (
        <div className="activate-card">
          {stage.approved ? (
            <>
              <div className="done-mark" aria-hidden="true">
                ✓
              </div>
              <h1>Device connected</h1>
              <p className="page-sub">Return to your terminal — it now holds its own API key.</p>
            </>
          ) : (
            <>
              <h1>Request denied</h1>
              <p className="page-sub">The code has been invalidated. Nothing was authorized.</p>
            </>
          )}
          <button type="button" className="btn btn-ghost btn-sm" onClick={reset}>
            Activate another device
          </button>
        </div>
      )}
    </div>
  )
}

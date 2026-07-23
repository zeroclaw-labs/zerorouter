import { useEffect, useState } from 'react'
import type { FormEvent } from 'react'
import { useSearchParams } from 'react-router-dom'
import { api, ApiError } from '../api'
import {
  Badge,
  Banner,
  EmptyState,
  Loading,
  Stat,
  formatSignedUsd,
  formatTime,
  formatUsd,
  useAuth,
  useLoad,
  useToast,
  useUser,
} from '../ui'

const PRESETS = ['10.00', '25.00', '100.00']

/**
 * Validate and normalize a user-entered amount to a `"NN.NN"` decimal string.
 * String manipulation only — no float ever represents the money value.
 * Returns null when invalid or under the $5.00 minimum.
 */
function normalizeAmount(raw: string): string | null {
  const cleaned = raw.trim().replace(/^\$/, '').replace(/,/g, '')
  const match = /^(\d{1,6})(?:\.(\d{1,2}))?$/.exec(cleaned)
  if (match === null) return null
  const int = match[1].replace(/^0+(?=\d)/, '')
  const frac = (match[2] ?? '').padEnd(2, '0')
  if (parseInt(int, 10) < 5) return null
  return `${int}.${frac}`
}

export function Credits() {
  const user = useUser()
  const { refresh } = useAuth()
  const toast = useToast()
  const [searchParams, setSearchParams] = useSearchParams()

  const ledger = useLoad(() => api.ledger(50), [])
  const [notice, setNotice] = useState<'success' | 'cancelled' | null>(null)
  const [preset, setPreset] = useState<string | null>('25.00')
  const [custom, setCustom] = useState('')
  const [formError, setFormError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [unavailable, setUnavailable] = useState(false)

  // Absorb the ?checkout=success|cancelled return from Stripe exactly once.
  useEffect(() => {
    const flag = searchParams.get('checkout')
    if (flag === 'success' || flag === 'cancelled') {
      setNotice(flag)
      const next = new URLSearchParams(searchParams)
      next.delete('checkout')
      setSearchParams(next, { replace: true })
      if (flag === 'success') {
        void refresh()
        ledger.reload()
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [searchParams])

  if (user === null) return null

  const chosen = custom.trim() !== '' ? custom : (preset ?? '')
  const normalized = normalizeAmount(chosen)

  async function buy(event: FormEvent) {
    event.preventDefault()
    const amount = normalizeAmount(chosen)
    if (amount === null) {
      setFormError('Enter an amount of at least $5.00, with up to two decimals.')
      return
    }
    setFormError(null)
    setSubmitting(true)
    try {
      const session = await api.checkout(amount)
      window.location.assign(session.url)
    } catch (err) {
      setSubmitting(false)
      if (err instanceof ApiError && err.code === 'billing_unavailable') {
        setUnavailable(true)
      } else {
        toast(err instanceof Error ? err.message : 'Checkout failed.', 'error')
      }
    }
  }

  return (
    <div className="page">
      <header className="page-head">
        <h1>Credits</h1>
        <p className="page-sub">Prepaid balance, purchases, and the full ledger</p>
      </header>

      {notice === 'success' && (
        <Banner kind="success" onDismiss={() => setNotice(null)}>
          Checkout complete. Credits appear as soon as Stripe confirms the payment — usually within seconds.
        </Banner>
      )}
      {notice === 'cancelled' && (
        <Banner kind="info" onDismiss={() => setNotice(null)}>
          Checkout cancelled — you have not been charged.
        </Banner>
      )}

      <section className="stats">
        <Stat label="Credit balance" value={formatUsd(user.credit_balance_usd)} />
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Add credits</h2>
        </div>
        {unavailable ? (
          <div className="panel-body">
            <Banner kind="info">Billing is not enabled on this deployment.</Banner>
          </div>
        ) : (
          <form className="buy" onSubmit={buy}>
            <div className="preset-row" role="group" aria-label="Amount">
              {PRESETS.map((p) => (
                <button
                  key={p}
                  type="button"
                  className={`preset${preset === p && custom.trim() === '' ? ' selected' : ''}`}
                  onClick={() => {
                    setPreset(p)
                    setCustom('')
                    setFormError(null)
                  }}
                >
                  ${p.slice(0, -3)}
                </button>
              ))}
              <label className="custom-amount">
                <span className="currency" aria-hidden="true">
                  $
                </span>
                <input
                  inputMode="decimal"
                  placeholder="Custom"
                  aria-label="Custom amount in dollars"
                  value={custom}
                  onChange={(e) => {
                    setCustom(e.target.value)
                    setFormError(null)
                  }}
                />
              </label>
            </div>
            <p className="field-hint">Minimum $5.00. Credits are spent by usage at the listed tier rates.</p>
            {formError !== null && <Banner kind="error">{formError}</Banner>}
            <button className="btn btn-primary" type="submit" disabled={submitting}>
              {submitting
                ? 'Redirecting to checkout…'
                : normalized !== null
                  ? `Buy ${formatUsd(normalized)} of credits`
                  : 'Buy credits'}
            </button>
          </form>
        )}
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Ledger</h2>
        </div>
        {ledger.loading ? (
          <Loading />
        ) : ledger.error !== null ? (
          <div className="panel-body">
            <Banner kind="error">{ledger.error}</Banner>
          </div>
        ) : ledger.data === null || ledger.data.length === 0 ? (
          <EmptyState
            title="No ledger activity yet"
            hint="Purchases, promo credits, and usage debits will appear here."
          />
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Type</th>
                <th className="num">Amount</th>
                <th className="num">Balance after</th>
                <th>Note</th>
              </tr>
            </thead>
            <tbody>
              {ledger.data.map((e) => {
                const positive = !e.amount_usd.trim().startsWith('-')
                return (
                  <tr key={e.id}>
                    <td className="dim nowrap">{formatTime(e.created_at)}</td>
                    <td>
                      <Badge tone={toneFor(e.entry_type)}>{e.entry_type}</Badge>
                    </td>
                    <td className={`num mono${positive ? ' amount-pos' : ''}`}>
                      {formatSignedUsd(e.amount_usd)}
                    </td>
                    <td className="num mono dim">{formatUsd(e.balance_after_usd)}</td>
                    <td className="dim note" title={e.note ?? undefined}>
                      {e.note ?? ''}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        )}
      </section>
    </div>
  )
}

function toneFor(entryType: string): 'good' | 'accent' | 'neutral' {
  if (entryType === 'purchase') return 'good'
  if (entryType === 'promo') return 'accent'
  return 'neutral'
}

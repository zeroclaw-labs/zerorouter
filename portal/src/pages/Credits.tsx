import { useEffect, useState } from 'react'
import type { FormEvent } from 'react'
import { useSearchParams } from 'react-router-dom'
import { api, ApiError } from '../api'
import type { AutopayStatus, Quote } from '../api'
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

/**
 * Like `normalizeAmount`, but for the autopay threshold: any non-negative
 * amount is a valid trigger, including $0.00 (top up only once exhausted).
 */
function normalizeThreshold(raw: string): string | null {
  const cleaned = raw.trim().replace(/^\$/, '').replace(/,/g, '')
  const match = /^(\d{1,6})(?:\.(\d{1,2}))?$/.exec(cleaned)
  if (match === null) return null
  const int = match[1].replace(/^0+(?=\d)/, '')
  const frac = (match[2] ?? '').padEnd(2, '0')
  return `${int}.${frac}`
}

export function Credits() {
  const user = useUser()
  const { refresh } = useAuth()
  const toast = useToast()
  const [searchParams, setSearchParams] = useSearchParams()

  const ledger = useLoad(() => api.ledger(50), [])
  const autopay = useLoad(() => api.autopay(), [])
  const [notice, setNotice] = useState<'success' | 'cancelled' | null>(null)
  const [preset, setPreset] = useState<string | null>('25.00')
  const [custom, setCustom] = useState('')
  const [formError, setFormError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [unavailable, setUnavailable] = useState(false)
  // The server-priced deposit for the current amount. Null until it lands (or
  // when the amount is invalid / billing is off); the fee is never computed here.
  const [quote, setQuote] = useState<Quote | null>(null)

  const [autopayNotice, setAutopayNotice] = useState<'saved' | 'cancelled' | null>(null)
  // The PUT response is the authoritative status the moment it lands; a
  // fresh GET supersedes it (cleared in the seed effect below). Without
  // this, a successful PUT followed by a failed reload leaves the badge
  // showing the opposite of what the server just confirmed (sol review).
  const [autopayOverride, setAutopayOverride] = useState<AutopayStatus | null>(null)
  const [threshold, setThreshold] = useState('')
  const [topup, setTopup] = useState('')
  const [autopayError, setAutopayError] = useState<string | null>(null)
  const [autopaySubmitting, setAutopaySubmitting] = useState(false)
  const [cardSubmitting, setCardSubmitting] = useState(false)

  // Absorb the ?checkout=success|cancelled and ?autopay=saved|cancelled
  // returns from Stripe exactly once.
  useEffect(() => {
    const checkout = searchParams.get('checkout')
    const card = searchParams.get('autopay')
    if (checkout === null && card === null) return
    const next = new URLSearchParams(searchParams)
    if (checkout === 'success' || checkout === 'cancelled') {
      setNotice(checkout)
      if (checkout === 'success') {
        void refresh()
        ledger.reload()
      }
    }
    next.delete('checkout')
    if (card === 'saved' || card === 'cancelled') {
      setAutopayNotice(card)
      if (card === 'saved') autopay.reload()
    }
    next.delete('autopay')
    setSearchParams(next, { replace: true })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [searchParams])

  // Seed the autopay form from the saved settings whenever they (re)load,
  // and let the fresh GET supersede any PUT-response override.
  useEffect(() => {
    setAutopayOverride(null)
    if (autopay.data !== null) {
      setThreshold(autopay.data.threshold_usd ?? '')
      setTopup(autopay.data.topup_usd ?? '')
    }
  }, [autopay.data])

  const autopayStatus = autopayOverride ?? autopay.data

  const chosen = custom.trim() !== '' ? custom : (preset ?? '')
  const normalized = normalizeAmount(chosen)

  // Price the deposit on the server whenever the amount changes — the fee is
  // never recomputed in TypeScript. Reset first so a stale fee never shows
  // against a new amount; a failed quote (billing off, out of bounds) simply
  // omits the fee line, and the server is still the authority at checkout.
  useEffect(() => {
    setQuote(null)
    if (normalized === null) return
    let active = true
    api
      .quote(normalized)
      .then((q) => {
        if (active) setQuote(q)
      })
      .catch(() => {})
    return () => {
      active = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [normalized])

  if (user === null) return null

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

  async function saveCard() {
    setCardSubmitting(true)
    try {
      const session = await api.autopaySetup()
      window.location.assign(session.url)
    } catch (err) {
      setCardSubmitting(false)
      if (err instanceof ApiError && err.code === 'billing_unavailable') {
        setUnavailable(true)
      } else {
        toast(err instanceof Error ? err.message : 'Could not start the card setup.', 'error')
      }
    }
  }

  async function saveAutopay(event: FormEvent) {
    event.preventDefault()
    const trigger = normalizeThreshold(threshold)
    const amount = normalizeAmount(topup)
    if (trigger === null) {
      setAutopayError('Enter a threshold of $0.00 or more, with up to two decimals.')
      return
    }
    if (amount === null) {
      setAutopayError('Enter a top-up of at least $5.00, with up to two decimals.')
      return
    }
    setAutopayError(null)
    setAutopaySubmitting(true)
    try {
      const status = await api.putAutopay({
        enabled: true,
        threshold_usd: trigger,
        topup_usd: amount,
      })
      setAutopayOverride(status)
      toast('Autopay is on.', 'success')
    } catch (err) {
      if (err instanceof ApiError && err.code === 'billing_unavailable') {
        setUnavailable(true)
      } else if (err instanceof ApiError && err.status === 400) {
        // The server refuses to arm autopay without a saved card or with
        // out-of-bounds amounts; its generic 400 reads badly here, so name
        // the two real causes.
        setAutopayError(
          'Autopay needs a saved card and in-bounds amounts — save a card first, then check the threshold and top-up.',
        )
      } else {
        toast(err instanceof Error ? err.message : 'Could not update autopay.', 'error')
      }
    } finally {
      setAutopaySubmitting(false)
    }
  }

  async function disableAutopay() {
    setAutopaySubmitting(true)
    try {
      const status = await api.putAutopay({ enabled: false })
      setAutopayOverride(status)
      toast('Autopay is off.')
    } catch (err) {
      toast(err instanceof Error ? err.message : 'Could not update autopay.', 'error')
    } finally {
      setAutopaySubmitting(false)
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
      {autopayNotice === 'saved' && (
        <Banner kind="success" onDismiss={() => setAutopayNotice(null)}>
          Card saved. Turn on autopay below to put it to work.
        </Banner>
      )}
      {autopayNotice === 'cancelled' && (
        <Banner kind="info" onDismiss={() => setAutopayNotice(null)}>
          Card setup cancelled — nothing was saved.
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
            {quote !== null && (
              <p className="field-hint quote-line">
                You pay {formatUsd(quote.gross)} (includes {formatUsd(quote.fee)} processing fee) →
                receive {formatUsd(quote.credit)} credit.
              </p>
            )}
            {formError !== null && <Banner kind="error">{formError}</Banner>}
            <button className="btn btn-primary" type="submit" disabled={submitting}>
              {submitting
                ? 'Redirecting to checkout…'
                : quote !== null
                  ? `Pay ${formatUsd(quote.gross)} → get ${formatUsd(quote.credit)} credit`
                  : normalized !== null
                    ? `Buy ${formatUsd(normalized)} of credits`
                    : 'Buy credits'}
            </button>
          </form>
        )}
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Autopay</h2>
          {autopayStatus !== null && (
            <Badge tone={autopayStatus.enabled ? 'good' : 'neutral'}>
              {autopayStatus.enabled ? 'on' : 'off'}
            </Badge>
          )}
        </div>
        {unavailable ? (
          <div className="panel-body">
            <Banner kind="info">Billing is not enabled on this deployment.</Banner>
          </div>
        ) : autopayStatus === null && autopay.loading ? (
          <Loading />
        ) : autopayStatus === null && autopay.error !== null ? (
          <div className="panel-body">
            <Banner kind="error">{autopay.error}</Banner>
          </div>
        ) : autopayStatus === null ? null : (
          <div className="panel-body autopay-body">
            <p className="field-hint">
              When your balance falls below the threshold, ZeroRouter charges your saved card for
              the top-up plus the same processing fee as a manual purchase, and adds the top-up as
              credits — no interruption. Three failed charges in a row turn autopay off.
            </p>
            <div className="card-row">
              <span className="dim">
                {autopayStatus.card_setup_started
                  ? 'A card setup has been started or completed with Stripe.'
                  : 'No card on file yet.'}
              </span>
              <button
                className="btn btn-ghost"
                type="button"
                onClick={saveCard}
                disabled={cardSubmitting}
              >
                {cardSubmitting
                  ? 'Redirecting to Stripe…'
                  : autopayStatus.card_setup_started
                    ? 'Save or replace card'
                    : 'Save a card'}
              </button>
            </div>
            {autopayStatus.consecutive_failures >= 3 && !autopayStatus.enabled && (
              <Banner kind="error">
                Autopay turned itself off after three failed charges. Replace the card, then turn
                it back on.
              </Banner>
            )}
            {autopayStatus.consecutive_failures > 0 && autopayStatus.enabled && (
              <Banner kind="info">
                {autopayStatus.consecutive_failures} failed charge
                {autopayStatus.consecutive_failures === 1 ? '' : 's'} so far — autopay turns itself
                off after three in a row.
              </Banner>
            )}
            <form className="autopay-form" onSubmit={saveAutopay}>
              <div className="autopay-amounts">
                <label>
                  Top up when the balance falls below
                  <input
                    className="field"
                    inputMode="decimal"
                    placeholder="10.00"
                    aria-label="Autopay threshold in dollars"
                    value={threshold}
                    onChange={(e) => {
                      setThreshold(e.target.value)
                      setAutopayError(null)
                    }}
                  />
                </label>
                <label>
                  Top-up amount
                  <input
                    className="field"
                    inputMode="decimal"
                    placeholder="25.00"
                    aria-label="Autopay top-up in dollars"
                    value={topup}
                    onChange={(e) => {
                      setTopup(e.target.value)
                      setAutopayError(null)
                    }}
                  />
                </label>
              </div>
              {autopayError !== null && <Banner kind="error">{autopayError}</Banner>}
              <div className="autopay-actions">
                <button className="btn btn-primary" type="submit" disabled={autopaySubmitting}>
                  {autopaySubmitting
                    ? 'Saving…'
                    : autopayStatus.enabled
                      ? 'Save changes'
                      : 'Turn on autopay'}
                </button>
                {autopayStatus.enabled && (
                  <button
                    className="btn btn-ghost"
                    type="button"
                    onClick={disableAutopay}
                    disabled={autopaySubmitting}
                  >
                    Turn off
                  </button>
                )}
              </div>
            </form>
          </div>
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

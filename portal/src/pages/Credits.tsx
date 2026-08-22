import { useEffect, useMemo, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { useSearchParams } from 'react-router-dom'
import { EmbeddedCheckout, EmbeddedCheckoutProvider } from '@stripe/react-stripe-js'
import { loadStripe } from '@stripe/stripe-js'
import type { Stripe } from '@stripe/stripe-js'
import { api, ApiError } from '../api'
import type { AutopayStatus, Quote, Rail } from '../api'
import {
  Badge,
  Banner,
  EmptyState,
  Loading,
  Modal,
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
 * What the `/credits/return` route concluded about a finished payment attempt.
 *
 * `success` means Stripe reported the session `complete` — the payment was
 * taken. It does NOT mean the credit has landed: crediting is driven by the
 * signed `checkout.session.completed` webhook, so the banner promises the
 * money is coming, never that the balance has already moved.
 */
type CheckoutNotice = 'success' | 'retry' | 'expired' | 'unknown'

const CHECKOUT_NOTICES: readonly string[] = ['success', 'retry', 'expired', 'unknown']

function isCheckoutNotice(value: string | null): value is CheckoutNotice {
  return value !== null && CHECKOUT_NOTICES.includes(value)
}

/**
 * How long to wait for Stripe's iframe to appear before declaring the mount
 * failed. Generous — this is a slow-network allowance, not a latency budget —
 * but finite, because the alternative is an empty box with no explanation.
 */
const STRIPE_MOUNT_TIMEOUT_MS = 15_000

/** Where the embedded payment form is in its lifecycle. */
type MountState = {
  state: 'idle' | 'creating' | 'mounting' | 'ready' | 'failed'
  error: string | null
}

/**
 * `loadStripe` injects the js.stripe.com script and must not be called on
 * every render. The publishable key arrives from the server (`/api/me`) rather
 * than from the bundle, so the promise cannot simply be a module constant —
 * this memoizes it per key instead, which gives the same "called once" result
 * without hardcoding the account.
 */
let stripeCache: { key: string; promise: Promise<Stripe | null> } | null = null
function stripeFor(publishableKey: string): Promise<Stripe | null> {
  if (stripeCache === null || stripeCache.key !== publishableKey) {
    stripeCache = { key: publishableKey, promise: loadStripe(publishableKey) }
  }
  return stripeCache.promise
}

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
  const [notice, setNotice] = useState<CheckoutNotice | null>(null)
  const [preset, setPreset] = useState<string | null>('25.00')
  const [custom, setCustom] = useState('')
  const [formError, setFormError] = useState<string | null>(null)
  const [unavailable, setUnavailable] = useState(false)
  // Modal state. `stage` is 'amount' while the customer picks what to buy and
  // 'pay' once Stripe's form is mounted; `payingAmount` is the amount the
  // mounted session was created for, and keys the provider so that going back
  // and choosing a different amount tears the old session's iframe down.
  const [modalOpen, setModalOpen] = useState(false)
  // Which rail the customer is buying on. Card is the default and stays the
  // primary path; crypto is opt-in per purchase and resets every time the modal
  // opens, so nobody is silently returned to a rail they used once.
  const [rail, setRail] = useState<Rail>('card')
  const [stage, setStage] = useState<'amount' | 'pay'>('amount')
  const [payingAmount, setPayingAmount] = useState<string | null>(null)
  const [clientSecret, setClientSecret] = useState<string | null>(null)
  const [mount, setMount] = useState<MountState>({ state: 'idle', error: null })
  // Bumped by the retry button to re-run the session-creation effect.
  const [retryCount, setRetryCount] = useState(0)
  const embedRef = useRef<HTMLDivElement | null>(null)
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

  // Absorb the ?checkout=... and ?autopay=saved|cancelled returns exactly
  // once. `checkout` is now set by the /credits/return route (which reads the
  // session status) rather than by Stripe redirecting to a success/cancel url.
  useEffect(() => {
    const checkout = searchParams.get('checkout')
    const card = searchParams.get('autopay')
    if (checkout === null && card === null) return
    const next = new URLSearchParams(searchParams)
    if (isCheckoutNotice(checkout)) {
      setNotice(checkout)
      if (checkout === 'success') {
        void refresh()
        ledger.reload()
      }
      // The payment did not go through, so put the customer back where they
      // were — the modal, on the amount step — rather than making them find
      // the button again.
      if (checkout === 'retry') {
        setStage('amount')
        setPayingAmount(null)
        setModalOpen(true)
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
  // omits the sentence below, and the server is still the authority at
  // checkout. (Distinct from the "Processing fee" LINE ITEM Stripe renders
  // inside the embedded form, which the server sends and this file never sees.)
  useEffect(() => {
    setQuote(null)
    if (normalized === null) return
    let active = true
    api
      .quote(normalized, rail)
      .then((q) => {
        // Guard against an out-of-order response: switching rails fires a
        // second request, and the first can still land after it. Showing a card
        // fee against a crypto purchase would misstate the price the customer
        // is about to pay, so a quote for a rail we are no longer on is
        // discarded rather than rendered.
        if (active && (q.rail === undefined || q.rail === rail)) setQuote(q)
      })
      .catch(() => {})
    return () => {
      active = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [normalized, rail])

  // The key is server configuration, so Stripe.js can only be loaded once the
  // session has. `null` means this deployment has no Stripe billing at all.
  // Ships dark: absent or false renders no crypto option at all. `?? false`
  // rather than `?? true` because an older router that does not send the field
  // has no crypto rail to offer.
  const cryptoAvailable = user?.crypto_rail ?? false
  const publishableKey = user?.stripe_publishable_key ?? null
  const stripePromise = useMemo(
    () => (publishableKey === null ? null : stripeFor(publishableKey)),
    [publishableKey],
  )

  // Create the Checkout Session ourselves rather than handing Stripe a
  // `fetchClientSecret` callback.
  //
  // Two reasons, both about failure. A rejecting `fetchClientSecret` is
  // swallowed inside react-stripe-js — it stores the resulting promise in a
  // ref with no `.catch`, so the rejection is unhandled and we get no chance
  // to render anything. And with the callback form there is no state to
  // distinguish "still creating" from "created, waiting for the iframe".
  // Fetching it here gives both, and `clientSecret` is a first-class provider
  // option, so nothing is lost.
  useEffect(() => {
    if (stage !== 'pay' || payingAmount === null) return
    let active = true
    setMount({ state: 'creating', error: null })
    setClientSecret(null)
    api
      .checkout(payingAmount, rail)
      .then((session) => {
        if (!active) return
        setClientSecret(session.client_secret)
        setMount({ state: 'mounting', error: null })
      })
      .catch((err: unknown) => {
        if (!active) return
        if (err instanceof ApiError && err.code === 'billing_unavailable') {
          setUnavailable(true)
          setModalOpen(false)
          return
        }
        setMount({
          state: 'failed',
          error:
            err instanceof Error
              ? err.message
              : 'The payment form could not be started. Please try again.',
        })
      })
    return () => {
      active = false
    }
  }, [stage, payingAmount, rail, retryCount])

  // Stripe.js can fail to mount with no error we can observe: js.stripe.com
  // blocked by a network or an extension, or a publishable key from the wrong
  // account. The old redirect flow could not fail this way — the browser
  // either reached Stripe's page or visibly did not — so an empty box that
  // stays empty forever is a regression in failure VISIBILITY, not just
  // polish. Watch for the iframe Stripe injects and give up out loud.
  useEffect(() => {
    if (mount.state !== 'mounting') return
    const deadline = Date.now() + STRIPE_MOUNT_TIMEOUT_MS
    const timer = window.setInterval(() => {
      if (embedRef.current?.querySelector('iframe') != null) {
        window.clearInterval(timer)
        setMount({ state: 'ready', error: null })
      } else if (Date.now() > deadline) {
        window.clearInterval(timer)
        setMount({
          state: 'failed',
          error:
            'The secure payment form did not load. An ad blocker or network policy may be ' +
            'blocking js.stripe.com.',
        })
      }
    }, 250)
    return () => window.clearInterval(timer)
  }, [mount.state, clientSecret])

  if (user === null) return null

  const billingOff = unavailable || publishableKey === null

  // Move from picking an amount to paying for it. The Checkout Session is
  // created by the effect above, once, when this stage is entered — so a
  // customer who opens the modal and closes it again has cost nothing.
  function goToPayment(event: FormEvent) {
    event.preventDefault()
    const amount = normalizeAmount(chosen)
    if (amount === null) {
      setFormError('Enter an amount of at least $5.00, with up to two decimals.')
      return
    }
    setFormError(null)
    setPayingAmount(amount)
    setStage('pay')
  }

  function closeCheckout() {
    setModalOpen(false)
    setStage('amount')
    setPayingAmount(null)
    setClientSecret(null)
    setRail('card')
    setMount({ state: 'idle', error: null })
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
          Payment received. Credits appear as soon as Stripe confirms the payment — usually within
          seconds.
        </Banner>
      )}
      {notice === 'retry' && (
        <Banner kind="info" onDismiss={() => setNotice(null)}>
          That payment was not completed — you have not been charged. Pick an amount to try again.
        </Banner>
      )}
      {notice === 'expired' && (
        <Banner kind="info" onDismiss={() => setNotice(null)}>
          That checkout expired before it was paid — you have not been charged.
        </Banner>
      )}
      {notice === 'unknown' && (
        <Banner kind="info" onDismiss={() => setNotice(null)}>
          We could not confirm that payment just now. If it went through, the credits will still
          appear here — the ledger below is the record.
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
        <div className="panel-body">
          {billingOff ? (
            <Banner kind="info">Billing is not enabled on this deployment.</Banner>
          ) : (
            <>
              <p className="field-hint">
                Credits are spent by usage at the listed tier rates. Minimum $5.00.
              </p>
              <button
                className="btn btn-primary"
                type="button"
                style={{ marginTop: 12 }}
                onClick={() => {
                  setFormError(null)
                  setStage('amount')
                  setPayingAmount(null)
                  setRail('card')
                  setModalOpen(true)
                }}
              >
                Add credits
              </button>
            </>
          )}
        </div>
      </section>

      {modalOpen && !billingOff && (
        <Modal title="Add credits" onClose={closeCheckout} wide>
          {stage === 'amount' ? (
            <form className="buy" onSubmit={goToPayment}>
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
              {quote !== null && (
                <p className="field-hint quote-line">
                  You pay {formatUsd(quote.gross)} (includes {formatUsd(quote.fee)} processing fee)
                  plus any applicable tax → receive {formatUsd(quote.credit)} credit. The exact tax
                  appears on the next step, once you enter your billing address. Businesses can
                  enter a VAT or tax ID there for reverse charge.
                </p>
              )}

              {/* The crypto rail, deliberately secondary: a plain toggle under
                  the amount rather than a second button beside "Continue", so
                  the card path stays the obvious one. Rendered only when the
                  operator has enabled stablecoin acceptance — otherwise this
                  block does not exist and the page is exactly what it was. */}
              {cryptoAvailable && (
                <div className="rail-choice" role="group" aria-label="Payment method">
                  <label className="rail-option">
                    <input
                      type="radio"
                      name="rail"
                      value="card"
                      checked={rail === 'card'}
                      onChange={() => setRail('card')}
                    />
                    <span>Card</span>
                  </label>
                  <label className="rail-option">
                    <input
                      type="radio"
                      name="rail"
                      value="crypto"
                      checked={rail === 'crypto'}
                      onChange={() => setRail('crypto')}
                    />
                    <span>Stablecoin (USDC)</span>
                  </label>
                </div>
              )}
              {cryptoAvailable && rail === 'crypto' && (
                <p className="field-hint">
                  The processing fee is 5% on stablecoin instead of 5.5% with a $0.80 minimum, so
                  small top-ups cost noticeably less. You will be sent to Stripe to connect a wallet
                  and pay in USDC; we settle in dollars and never hold the coin. Sales tax is
                  calculated the same way as on a card. Stripe settles at most $10,000 per crypto
                  payment. Crypto payments cannot be charged back, so please check the amount before
                  you pay.
                </p>
              )}
              {formError !== null && <Banner kind="error">{formError}</Banner>}
              <div className="modal-actions">
                <button className="btn btn-ghost" type="button" onClick={closeCheckout}>
                  Cancel
                </button>
                <button className="btn btn-primary" type="submit">
                  {quote !== null
                    ? `Continue — ${formatUsd(quote.gross)} + tax`
                    : normalized !== null
                      ? `Continue — ${formatUsd(normalized)} of credits`
                      : 'Continue'}
                </button>
              </div>
            </form>
          ) : (
            <>
              <div className="checkout-summary">
                <p className="field-hint">
                  {quote !== null ? (
                    <>
                      {formatUsd(quote.gross)} (includes {formatUsd(quote.fee)} processing fee) plus
                      tax → {formatUsd(quote.credit)} credit
                      {rail === 'crypto' ? ', paid in stablecoin' : ''}.
                    </>
                  ) : (
                    <>Paying for {formatUsd(payingAmount)} of credits.</>
                  )}{' '}
                  <button className="linkish" type="button" onClick={() => setStage('amount')}>
                    Change amount
                  </button>
                </p>
              </div>
              {/* In ZeroRouter's own voice, above Stripe's form, because the
                  address field appears without explanation otherwise. */}
              <p className="checkout-note">
                A billing address is required to verify your identity, help prevent fraud, and
                calculate any sales tax due.{' '}
                {rail === 'crypto' ? (
                  <>
                    We never see your wallet or its keys — the form below is Stripe's, and it will
                    hand you to Stripe's wallet connection to complete the payment.
                  </>
                ) : (
                  <>We never see your card details — the form below is Stripe's.</>
                )}
              </p>
              {mount.state === 'failed' ? (
                <div className="checkout-embed">
                  <Banner kind="error">{mount.error}</Banner>
                  <div className="modal-actions">
                    <button className="btn btn-ghost" type="button" onClick={closeCheckout}>
                      Close
                    </button>
                    <button
                      className="btn btn-primary"
                      type="button"
                      onClick={() => setRetryCount((n) => n + 1)}
                    >
                      Try again
                    </button>
                  </div>
                </div>
              ) : (
                <div className="checkout-embed" ref={embedRef}>
                  {mount.state !== 'ready' && (
                    <Loading
                      label={
                        mount.state === 'creating'
                          ? 'Preparing your payment'
                          : 'Opening secure payment'
                      }
                    />
                  )}
                  {clientSecret !== null && (
                    <EmbeddedCheckoutProvider
                      // Keyed by the secret, so a new session always gets a
                      // fresh provider and a stale iframe is torn down rather
                      // than left showing the previous price.
                      key={clientSecret}
                      stripe={stripePromise}
                      options={{ clientSecret }}
                    >
                      <EmbeddedCheckout />
                    </EmbeddedCheckoutProvider>
                  )}
                </div>
              )}
            </>
          )}
        </Modal>
      )}

      <section className="panel">
        <div className="panel-head">
          <h2>Autopay</h2>
          {autopayStatus !== null && (
            <Badge tone={autopayStatus.enabled ? 'good' : 'neutral'}>
              {autopayStatus.enabled ? 'on' : 'off'}
            </Badge>
          )}
        </div>
        {billingOff ? (
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

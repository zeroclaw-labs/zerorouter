// Shared UI primitives, contexts, and formatting helpers.
//
// Money is formatted from decimal strings by string manipulation only —
// floats never touch an amount that gets displayed or sent to the server.

import { createContext, useCallback, useContext, useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import type { Me } from './api'

// ---------------------------------------------------------------------------
// Auth context

export type Auth =
  | { status: 'loading' }
  | { status: 'signed-out'; reason?: string }
  | { status: 'signed-in'; user: Me }

export interface AuthContextValue {
  auth: Auth
  refresh: () => Promise<void>
  signOut: () => Promise<void>
}

export const AuthContext = createContext<AuthContextValue | null>(null)

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext)
  if (ctx === null) {
    throw new Error('useAuth must be used inside AuthContext.Provider')
  }
  return ctx
}

/** The signed-in user, or null while the app is not signed in. */
export function useUser(): Me | null {
  const { auth } = useAuth()
  return auth.status === 'signed-in' ? auth.user : null
}

// ---------------------------------------------------------------------------
// Toast context

export type ToastKind = 'info' | 'error' | 'success'

export type ToastFn = (message: string, kind?: ToastKind) => void

export const ToastContext = createContext<ToastFn>(() => undefined)

export function useToast(): ToastFn {
  return useContext(ToastContext)
}

// ---------------------------------------------------------------------------
// Data loading

export interface Loaded<T> {
  data: T | null
  error: string | null
  loading: boolean
  reload: () => void
}

export function useLoad<T>(load: () => Promise<T>, deps: readonly unknown[]): Loaded<T> {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [tick, setTick] = useState(0)

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    load().then(
      (value) => {
        if (!cancelled) {
          setData(value)
          setLoading(false)
        }
      },
      (err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : 'Something went wrong.')
          setLoading(false)
        }
      },
    )
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick, ...deps])

  const reload = useCallback(() => setTick((t) => t + 1), [])
  return { data, error, loading, reload }
}

// ---------------------------------------------------------------------------
// Formatting

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/** Local time as `MMM D, HH:mm`. Falls back to the raw string if unparsable. */
export function formatTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const hh = String(d.getHours()).padStart(2, '0')
  const mm = String(d.getMinutes()).padStart(2, '0')
  return `${MONTHS[d.getMonth()]} ${d.getDate()}, ${hh}:${mm}`
}

export function formatInt(value: number): string {
  return Number.isFinite(value) ? Math.trunc(value).toLocaleString('en-US') : '—'
}

interface UsdParts {
  negative: boolean
  int: string
  frac: string
}

function usdParts(value: string | number | null | undefined): UsdParts | null {
  if (value === null || value === undefined) return null
  const raw = typeof value === 'number' ? value.toString() : value.trim()
  const match = /^([+-]?)(\d+)(?:\.(\d+))?$/.exec(raw)
  if (match === null) return null
  const digits = `${match[2]}${match[3] ?? ''}`
  const negative = match[1] === '-' && !/^0*$/.test(digits)
  let int = match[2].replace(/^0+(?=\d)/, '')
  int = int.replace(/\B(?=(\d{3})+(?!\d))/g, ',')
  // At least two fractional digits; keep up to four when the extra precision
  // is real (sub-cent usage charges), trimming trailing zeros beyond two.
  let frac = (match[3] ?? '').padEnd(2, '0')
  if (frac.length > 2) {
    frac = frac.slice(0, 4).replace(/0+$/, '')
    if (frac.length < 2) frac = frac.padEnd(2, '0')
  }
  return { negative, int, frac }
}

/** `"25.0000"` → `$25.00`, `"0.4371"` → `$0.4371`, junk → `—`. */
export function formatUsd(value: string | number | null | undefined): string {
  const parts = usdParts(value)
  if (parts === null) return '—'
  return `${parts.negative ? '-' : ''}$${parts.int}.${parts.frac}`
}

/** Like `formatUsd` but with an explicit sign for ledger deltas. */
export function formatSignedUsd(value: string | number | null | undefined): string {
  const parts = usdParts(value)
  if (parts === null) return '—'
  return `${parts.negative ? '-' : '+'}$${parts.int}.${parts.frac}`
}

// ---------------------------------------------------------------------------
// Components

export function Banner({
  kind,
  children,
  onDismiss,
}: {
  kind: ToastKind
  children: ReactNode
  onDismiss?: () => void
}) {
  return (
    <div className={`banner banner-${kind}`} role={kind === 'error' ? 'alert' : 'status'}>
      <span className="banner-body">{children}</span>
      {onDismiss && (
        <button type="button" className="banner-x" onClick={onDismiss} aria-label="Dismiss">
          ×
        </button>
      )}
    </div>
  )
}

export function Badge({ tone, children }: { tone: 'good' | 'accent' | 'neutral' | 'bad'; children: ReactNode }) {
  return <span className={`badge badge-${tone}`}>{children}</span>
}

export function Stat({ label, value, sub }: { label: string; value: ReactNode; sub?: string }) {
  return (
    <div className="stat">
      <div className="stat-label">{label}</div>
      <div className="stat-value">{value}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  )
}

export function EmptyState({ title, hint, action }: { title: string; hint?: string; action?: ReactNode }) {
  return (
    <div className="empty">
      <p className="empty-title">{title}</p>
      {hint && <p className="empty-hint">{hint}</p>}
      {action}
    </div>
  )
}

export function Loading({ label = 'Loading' }: { label?: string }) {
  return <p className="loading">{label}…</p>
}

/**
 * A centred modal dialog.
 *
 * `onClose` is optional: some modals (the one-shot "API key created" panel)
 * own their own dismissal through their action buttons and must NOT be
 * escapable, because closing them loses the only copy of a secret. When it is
 * supplied the modal gains the usual affordances — a close button, Escape, and
 * a click on the backdrop.
 *
 * `wide` gives the dialog room for content that cannot reflow below ~500px,
 * such as Stripe's embedded payment form.
 */
export function Modal({
  title,
  children,
  onClose,
  wide = false,
}: {
  title: string
  children: ReactNode
  onClose?: () => void
  wide?: boolean
}) {
  useEffect(() => {
    if (onClose === undefined) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [onClose])

  return (
    <div
      className="modal-overlay"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      // Backdrop-only: a click that started inside the dialog (a drag out of a
      // text field, say) must not dismiss it and lose entered payment details.
      onMouseDown={onClose === undefined ? undefined : (e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className={`modal${wide ? ' modal-wide' : ''}`}>
        <div className="modal-head">
          <h2 className="modal-title">{title}</h2>
          {onClose !== undefined && (
            <button type="button" className="modal-close" onClick={onClose} aria-label="Close">
              ×
            </button>
          )}
        </div>
        {children}
      </div>
    </div>
  )
}

/**
 * A labelled, copyable code sample.
 *
 * Shared by the docs page and the one-shot "key created" panel on purpose: the
 * curl a customer is handed with a new key and the curl on the docs page are
 * the same command, and two renderers would eventually disagree about it.
 *
 * `children` is the literal text — copied verbatim by the button, so it must be
 * exactly what a reader would paste, not a prettified version of it.
 */
export function CodeBlock({ label, children }: { label: string; children: string }) {
  return (
    <div className="code">
      <div className="code-head">
        <span className="code-label">{label}</span>
        <CopyButton text={children} />
      </div>
      <pre className="code-body">
        <code>{children}</code>
      </pre>
    </div>
  )
}

export function CopyButton({ text, label = 'Copy' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    if (!copied) return
    const timer = window.setTimeout(() => setCopied(false), 1600)
    return () => window.clearTimeout(timer)
  }, [copied])

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
    } catch {
      setCopied(false)
    }
  }

  return (
    <button type="button" className="btn btn-ghost" onClick={copy}>
      {copied ? 'Copied' : label}
    </button>
  )
}

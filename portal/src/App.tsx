import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { BrowserRouter, Navigate, NavLink, Route, Routes, useLocation } from 'react-router-dom'
import { api, ApiError, onSessionRequired } from './api'
import type { Me } from './api'
import { AuthContext, Banner, ToastContext, useAuth } from './ui'
import type { Auth, ToastKind } from './ui'
import { Activate } from './pages/Activate'
import { Credits } from './pages/Credits'
import { CreditsReturn } from './pages/CreditsReturn'
import { Docs } from './pages/Docs'
import { Keys } from './pages/Keys'
import { Models } from './pages/Models'
import { Playground } from './pages/Playground'
import { Overview } from './pages/Overview'
import { Privacy } from './pages/Privacy'
import { Terms } from './pages/Terms'

export function App() {
  return (
    <BrowserRouter>
      <ToastProvider>
        <AuthProvider>
          <Shell />
        </AuthProvider>
      </ToastProvider>
    </BrowserRouter>
  )
}

// ---------------------------------------------------------------------------
// Providers

interface ToastItem {
  id: number
  message: string
  kind: ToastKind
}

function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<ToastItem[]>([])
  const nextId = useRef(1)

  const push = useCallback((message: string, kind: ToastKind = 'info') => {
    const id = nextId.current
    nextId.current += 1
    setToasts((current) => [...current.slice(-3), { id, message, kind }])
    window.setTimeout(() => {
      setToasts((current) => current.filter((t) => t.id !== id))
    }, 5000)
  }, [])

  return (
    <ToastContext.Provider value={push}>
      {children}
      <div className="toasts" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={`toast toast-${t.kind}`}>
            {t.message}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  )
}

function AuthProvider({ children }: { children: ReactNode }) {
  const [auth, setAuth] = useState<Auth>({ status: 'loading' })

  const refresh = useCallback(async () => {
    try {
      const user = await api.me()
      setAuth({ status: 'signed-in', user })
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        setAuth({ status: 'signed-out' })
      } else {
        // Fail closed: an unreachable or erroring server is a signed-out
        // portal, with the reason surfaced on the landing screen.
        setAuth({
          status: 'signed-out',
          reason: err instanceof Error ? err.message : 'Could not reach the server.',
        })
      }
    }
  }, [])

  const signOut = useCallback(async () => {
    try {
      await api.logout()
    } catch {
      // The session may already be gone; either way we are signed out.
    }
    setAuth({ status: 'signed-out' })
  }, [])

  useEffect(() => {
    onSessionRequired(() => setAuth({ status: 'signed-out' }))
    void refresh()
  }, [refresh])

  const value = useMemo(() => ({ auth, refresh, signOut }), [auth, refresh, signOut])
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>
}

// ---------------------------------------------------------------------------
// Shell

function Shell() {
  const { auth } = useAuth()
  const location = useLocation()
  if (auth.status === 'loading') {
    return (
      <div className="boot">
        <span className="mark" aria-label="Loading" />
      </div>
    )
  }
  if (auth.status === 'signed-out') {
    if (location.pathname.startsWith('/activate')) return <SignedOutActivate />
    // The catalog is a storefront: viewable before signing in.
    if (location.pathname.startsWith('/models')) {
      return (
        <PublicPage>
          <Models />
        </PublicPage>
      )
    }
    // The API reference is public for the same reason the catalog is, and one
    // stronger: someone deciding whether to sign up is exactly the person who
    // needs to know the base URL is OpenAI-compatible and what a request costs.
    // Putting it behind the login would hide the answer from the only audience
    // that has not already found it.
    if (location.pathname.startsWith('/docs')) {
      return (
        <PublicPage>
          <Docs />
        </PublicPage>
      )
    }
    // Legal pages are public: readable without an account.
    if (location.pathname.startsWith('/terms')) {
      return (
        <PublicPage>
          <Terms />
        </PublicPage>
      )
    }
    if (location.pathname.startsWith('/privacy')) {
      return (
        <PublicPage>
          <Privacy />
        </PublicPage>
      )
    }
    return <Landing reason={auth.reason} />
  }
  return <SignedInLayout user={auth.user} />
}

function SignedInLayout({ user }: { user: Me }) {
  return (
    <div className="layout">
      <Sidebar user={user} />
      <main className="main">
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/models" element={<Models />} />
          <Route path="/playground" element={<Playground />} />
          <Route path="/credits" element={<Credits />} />
          {/* Stripe's return_url target. Must stay in sync with
              CHECKOUT_RETURN_PATH in router/src/stripe.rs. */}
          <Route path="/credits/return" element={<CreditsReturn />} />
          <Route path="/keys" element={<Keys />} />
          <Route path="/docs" element={<Docs />} />
          <Route path="/activate" element={<Activate />} />
          <Route path="/terms" element={<Terms />} />
          <Route path="/privacy" element={<Privacy />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </div>
  )
}

function Sidebar({ user }: { user: Me }) {
  const { signOut } = useAuth()
  const link = ({ isActive }: { isActive: boolean }) => `nav-link${isActive ? ' active' : ''}`
  return (
    <aside className="sidebar">
      <Wordmark />
      <nav aria-label="Portal">
        <NavLink to="/" end className={link}>
          Overview
        </NavLink>
        <NavLink to="/models" className={link}>
          Models
        </NavLink>
        {/* Signed-in only, deliberately: the playground spends the reader's own
            credits through their own key, so there is nothing to show a visitor
            without an account. The storefront at /models is the public half. */}
        <NavLink to="/playground" className={link}>
          Playground
        </NavLink>
        <NavLink to="/credits" className={link}>
          Credits
        </NavLink>
        <NavLink to="/keys" className={link}>
          Keys
        </NavLink>
        <NavLink to="/docs" className={link}>
          Docs
        </NavLink>
      </nav>
      <div className="sidebar-foot">
        <div className="sidebar-email" title={user.email}>
          {user.email}
        </div>
        <button type="button" className="btn btn-ghost btn-sm" onClick={() => void signOut()}>
          Sign out
        </button>
      </div>
    </aside>
  )
}

function Wordmark({ large = false }: { large?: boolean }) {
  return (
    <span className={large ? 'wordmark wordmark-lg' : 'wordmark'}>
      <span className="mark" aria-hidden="true" />
      ZeroRouter
    </span>
  )
}

// ---------------------------------------------------------------------------
// Signed-out screens

function Landing({ reason }: { reason?: string }) {
  return (
    <div className="landing">
      <div className="landing-inner">
        <Wordmark large />
        <p className="landing-tag">Zero data retention, by default.</p>
        <p className="landing-sub">
          ZeroRouter is structurally incapable of reading, logging, or training on your prompts and
          completions — content-free by design, and open source so anyone can verify it. Prepaid
          access to every major model, one balance, one key.
        </p>
        {reason && <Banner kind="error">{reason}</Banner>}
        <a className="btn btn-primary btn-lg" href="/auth/login">
          Sign in with SSO
        </a>
        {/* Beside the CTA, not buried in the footer. Someone landing here is
            deciding whether to sign up at all, and the answer to "what is this,
            concretely" is a base URL and a curl — which used to be reachable
            only by signing in first. */}
        <p className="landing-secondary">
          OpenAI-compatible — <a href="/docs">read the API docs</a> or{' '}
          <a href="/models">browse the models</a> before you sign in.
        </p>
        <nav className="landing-foot" aria-label="Legal">
          <a href="/terms">Terms</a>
          <span className="landing-foot-sep" aria-hidden="true">
            ·
          </span>
          <a href="/privacy">Privacy</a>
        </nav>
      </div>
    </div>
  )
}

// Any page readable before sign-in: a slim top bar with the wordmark, links to
// the other public pages, and a sign-in CTA, then the same page component the
// signed-in portal renders.
//
// One wrapper for all of them rather than one per page. The catalog and the
// legal pages had identical copies of this bar, and the docs page would have
// made three — at which point adding a link to the bar means remembering to add
// it three times, and a public page that cannot reach the other public pages is
// a dead end for the only reader who arrives without a session.
function PublicPage({ children }: { children: ReactNode }) {
  return (
    <div className="public">
      <header className="public-top">
        <Wordmark />
        <nav className="public-nav" aria-label="Public">
          <NavLink to="/models" className={publicLink}>
            Models
          </NavLink>
          <NavLink to="/docs" className={publicLink}>
            Docs
          </NavLink>
        </nav>
        <a className="btn btn-primary btn-sm" href="/auth/login">
          Sign in
        </a>
      </header>
      <main className="main">{children}</main>
    </div>
  )
}

function publicLink({ isActive }: { isActive: boolean }): string {
  return `public-nav-link${isActive ? ' active' : ''}`
}

function SignedOutActivate() {
  return (
    <div className="landing">
      <div className="landing-inner">
        <Wordmark large />
        <h1 className="landing-h">Connect a device</h1>
        <p className="landing-sub">
          Sign in to continue. Keep your terminal open — you will enter its code after signing in.
        </p>
        <a className="btn btn-primary btn-lg" href="/auth/login">
          Sign in with SSO
        </a>
      </div>
    </div>
  )
}

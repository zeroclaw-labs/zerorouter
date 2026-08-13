import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { BrowserRouter, Navigate, NavLink, Route, Routes, useLocation } from 'react-router-dom'
import { api, ApiError, onSessionRequired } from './api'
import type { Me } from './api'
import { AuthContext, Banner, ToastContext, useAuth } from './ui'
import type { Auth, ToastKind } from './ui'
import { Activate } from './pages/Activate'
import { Credits } from './pages/Credits'
import { Keys } from './pages/Keys'
import { Models } from './pages/Models'
import { Overview } from './pages/Overview'

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
    if (location.pathname.startsWith('/models')) return <PublicModels />
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
          <Route path="/credits" element={<Credits />} />
          <Route path="/keys" element={<Keys />} />
          <Route path="/activate" element={<Activate />} />
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
        <NavLink to="/credits" className={link}>
          Credits
        </NavLink>
        <NavLink to="/keys" className={link}>
          Keys
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
        <p className="landing-tag">Prepaid inference gateway</p>
        <p className="landing-sub">
          One balance, one key namespace, three tiers. Sign in to manage credits, API keys, and usage.
        </p>
        {reason && <Banner kind="error">{reason}</Banner>}
        <a className="btn btn-primary btn-lg" href="/auth/login">
          Sign in with SSO
        </a>
      </div>
    </div>
  )
}

// The catalog before sign-in: a slim top bar with the wordmark and a sign-in
// CTA, then the same Models page the signed-in portal renders.
function PublicModels() {
  return (
    <div className="public">
      <header className="public-top">
        <Wordmark />
        <a className="btn btn-primary btn-sm" href="/auth/login">
          Sign in
        </a>
      </header>
      <main className="main">
        <Models />
      </main>
    </div>
  )
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

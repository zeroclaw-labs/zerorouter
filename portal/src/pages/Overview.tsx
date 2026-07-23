import { Link } from 'react-router-dom'
import { api } from '../api'
import {
  Badge,
  Banner,
  EmptyState,
  Loading,
  Stat,
  formatInt,
  formatTime,
  formatUsd,
  useLoad,
  useUser,
} from '../ui'

export function Overview() {
  const user = useUser()
  const usage = useLoad(() => api.usage(30), [])
  if (user === null) return null

  const totals = usage.data?.totals

  return (
    <div className="page">
      <header className="page-head">
        <h1>Overview</h1>
        <p className="page-sub">Balance and activity for the last 30 days</p>
      </header>

      <section className="stats">
        <Stat label="Credit balance" value={formatUsd(user.credit_balance_usd)} />
        <Stat label="Spend · 30d" value={totals ? formatUsd(totals.cost_usd) : '—'} />
        <Stat label="Requests · 30d" value={totals ? formatInt(totals.requests) : '—'} />
        <Stat
          label="Tokens · 30d"
          value={totals ? formatInt(totals.input_tokens + totals.output_tokens) : '—'}
          sub={totals ? `${formatInt(totals.input_tokens)} in · ${formatInt(totals.output_tokens)} out` : undefined}
        />
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Recent activity</h2>
        </div>
        {usage.loading ? (
          <Loading />
        ) : usage.error !== null ? (
          <div className="panel-body">
            <Banner kind="error">{usage.error}</Banner>
          </div>
        ) : usage.data === null || usage.data.recent.length === 0 ? (
          <EmptyState
            title="No requests yet"
            hint="Create an API key, then point any OpenAI-compatible client at /v1/chat/completions."
            action={
              <Link className="btn btn-ghost btn-sm" to="/keys">
                Create a key
              </Link>
            }
          />
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Model</th>
                <th>Key</th>
                <th className="num">Tokens</th>
                <th className="num">Cost</th>
                <th className="num">Latency</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {usage.data.recent.map((r) => (
                <tr key={r.request_id}>
                  <td className="dim nowrap">{formatTime(r.ts)}</td>
                  <td>
                    <span className="mono">{r.upstream_model}</span>
                    <span className="table-sub">
                      {r.tier !== null && r.tier !== '' ? `${r.tier} · ` : ''}
                      {r.upstream_provider}
                    </span>
                  </td>
                  <td className="dim">{r.key_name ?? '—'}</td>
                  <td className="num mono">
                    {formatInt(r.input_tokens)} → {formatInt(r.output_tokens)}
                  </td>
                  <td className="num mono">{formatUsd(r.cost_usd)}</td>
                  <td className="num mono dim">{formatInt(r.latency_ms)} ms</td>
                  <td>
                    <StatusBadge status={r.status} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  )
}

function StatusBadge({ status }: { status: string }) {
  const ok = status === 'ok' || status === 'success' || /^2\d\d$/.test(status)
  return <Badge tone={ok ? 'good' : 'bad'}>{status}</Badge>
}

import { api } from '../api'
import type { Model } from '../api'
import { Badge, Banner, Loading, useLoad } from '../ui'

// Prices arrive per single token; humans read $/Mtok.
function perMtok(price: string): string {
  const n = Number.parseFloat(price)
  if (!Number.isFinite(n)) return '—'
  return `$${(n * 1_000_000).toFixed(2)}`
}

function formatContext(tokens: number): string {
  if (tokens >= 1_000_000) {
    const m = tokens / 1_000_000
    return `${Number.isInteger(m) ? m : m.toFixed(1)}M`
  }
  if (tokens >= 1_000) return `${Math.round(tokens / 1_000)}K`
  return `${tokens}`
}

/** The public model catalog. Reads `GET /v1/models` — the same catalog every
 * OpenAI-compatible client sees — and shows the vendor-named `{vendor}/{model}`
 * pins with the serving vendor, their exact prices, context windows, and
 * capabilities. Rows the router owns itself (`owned_by === 'zerorouter'`, the
 * reserved `zero/*` routing aliases) are not real models to buy, so they are
 * left out. */
export function Models() {
  const models = useLoad(() => api.models(), [])
  const tiers: Model[] = (models.data ?? [])
    .filter((m) => m.owned_by !== 'zerorouter')
    // Group by vendor, then cheapest -> premium by output price within each
    // vendor, so the catalog reads as tidy per-provider blocks rather than a
    // vendor-mixed price ladder.
    .sort((a, b) =>
      a.owned_by !== b.owned_by
        ? a.owned_by.localeCompare(b.owned_by)
        : Number.parseFloat(a.pricing.completion) - Number.parseFloat(b.pricing.completion),
    )

  return (
    <div className="page">
      <header className="page-head">
        <h1>Models</h1>
        <p className="page-sub">
          Every model at cost — the price you pay is the provider’s list rate, with no markup on
          inference. A 5.5% fee applies only when you add credits.
        </p>
      </header>

      <section className="panel">
        {models.loading ? (
          <Loading />
        ) : models.error !== null ? (
          <div className="panel-body">
            <Banner kind="error">{models.error}</Banner>
          </div>
        ) : tiers.length === 0 ? (
          <div className="panel-body">
            <Banner kind="info">No models are currently listed.</Banner>
          </div>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Vendor</th>
                <th>Model</th>
                <th className="num">Input</th>
                <th className="num">Output</th>
                <th className="num">Context</th>
                <th>Inputs</th>
                <th>Tools</th>
              </tr>
            </thead>
            <tbody>
              {tiers.map((m) => (
                <tr key={m.id}>
                  <td className="dim">{m.owned_by}</td>
                  <td>
                    <span className="mono">{m.id}</span>
                  </td>
                  <td className="num mono">
                    {perMtok(m.pricing.prompt)}
                    <span className="table-sub">/Mtok</span>
                  </td>
                  <td className="num mono">
                    {perMtok(m.pricing.completion)}
                    <span className="table-sub">/Mtok</span>
                  </td>
                  <td className="num mono dim">
                    {m.context_length ? formatContext(m.context_length) : '—'}
                  </td>
                  <td className="dim">{(m.input_modalities ?? ['text']).join(' · ')}</td>
                  <td>
                    {m.tool_call ? (
                      <Badge tone="good">yes</Badge>
                    ) : (
                      <Badge tone="neutral">no</Badge>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <p className="page-note">
        Prices are per million tokens. Point any OpenAI-compatible client at{' '}
        <span className="mono">/v1</span> and request a model by its id.
      </p>
    </div>
  )
}

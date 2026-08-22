import { api } from '../api'
import type { Model } from '../api'
import { Badge, Banner, Loading, useLoad } from '../ui'

// Prices arrive per single token; humans read $/Mtok.
function perMtok(price: string): string {
  const n = Number.parseFloat(price)
  if (!Number.isFinite(n)) return '—'
  return `$${(n * 1_000_000).toFixed(2)}`
}

/** Sort rank for the retention posture: zero-retention lanes come first.
 *
 * Mirrors `RetentionPosture::ordering_rank` in the router. An unknown or absent
 * posture ranks with the retaining lanes, never with the zero ones — a row this
 * page cannot read must not be promoted to the flattering half of the catalog.
 *
 * Exported because the playground groups its lane picker on the same rule. Two
 * copies of "which half of the catalog is this lane in" would eventually
 * disagree, and the direction they would disagree in is the flattering one. */
export function retentionRank(model: Model): number {
  return model.retention?.posture === 'zero' ? 0 : 1
}

/** The full retention statement, for a tooltip. Shared with the playground,
 * which puts the same claim on every response it renders. */
export function retentionTitle(model: Model): string {
  if (!model.retention) return 'This router did not publish a retention posture for this lane.'
  return `${model.retention.description} (verified ${model.retention.verified})`
}

/** The retention label as the storefront states it.
 *
 * One definition, used by the catalog table and by the badge the playground
 * stamps on every response. The wording is a claim ZeroRouter makes about a
 * customer's data, so it must read identically wherever it appears — a
 * playground that said "zero retention" where the catalog said something else
 * would be the worst kind of drift to ship. */
export function RetentionBadge({ model }: { model: Model }) {
  return model.retention?.posture === 'zero' ? (
    <Badge tone="good">zero retention</Badge>
  ) : model.retention?.posture === 'standard' ? (
    <Badge tone="neutral">provider retains data</Badge>
  ) : (
    <Badge tone="neutral">unstated</Badge>
  )
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
    // ZERO-RETENTION LANES FIRST, then the existing grouping within each
    // posture: by vendor, then cheapest -> premium by output price.
    //
    // The router already sends the rows in posture order, but this page re-sorts
    // for its own vendor grouping and would otherwise silently undo it — so the
    // ordering claim has to be restated here rather than inherited. Both sides
    // read `ordering_rank`'s rule: zero before standard.
    .sort((a, b) =>
      retentionRank(a) !== retentionRank(b)
        ? retentionRank(a) - retentionRank(b)
        : a.owned_by !== b.owned_by
          ? a.owned_by.localeCompare(b.owned_by)
          : Number.parseFloat(a.pricing.completion) - Number.parseFloat(b.pricing.completion),
    )
  const zeroLanes = tiers.filter((m) => m.retention?.posture === 'zero').length

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
                <th>Retention</th>
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
                  {/* The label is the claim; `title` carries the full statement
                      and the date it was verified, so the detail is one hover
                      away without widening the table. A row whose posture this
                      page cannot read says so plainly rather than defaulting to
                      the favourable answer. */}
                  <td title={retentionTitle(m)}>
                    <RetentionBadge model={m} />
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

      {/* The label needs a referent, and the tally is stated either way. It was
          zero until 2026-08-20 and the page said so plainly; it is now two, and
          the same sentence has to carry that without overclaiming. A catalog
          that labels every lane and then hides the count would be labelling for
          show — in whichever direction the count happens to run. */}
      <p className="page-note">
        <strong>Retention</strong> is what the upstream provider does with your prompt after it
        answers. Zero-retention lanes are listed first.{' '}
        {zeroLanes === 0
          ? 'No lane currently carries a zero-retention label: every model above runs on a standard API account, and those providers retain data under their own published policies. We only apply a zero-retention label once zero data retention is in force with that provider.'
          : `${zeroLanes} of ${tiers.length} lanes are zero retention: the provider writes nothing to durable storage. The rest run on standard API accounts and the provider retains data under its own published policy.`}{' '}
        Hover a label for the provider’s exact statement and the date we last verified it.
      </p>
      {/* Stated only when there is a premium to explain. Two lanes here serve
          the same models as cheaper ones above, and a storefront that shows the
          higher price without the reason reads as a markup — which it is not:
          every lane is sold at what it costs, and this one costs more. */}
      {zeroLanes > 0 && (
        <p className="page-note">
          A zero-retention lane can cost more than the same model on a standard account. That
          difference is the provider’s own price, passed through like every other rate on this
          page — ZeroRouter takes no margin on either.
        </p>
      )}
    </div>
  )
}

import { useState } from 'react'
import type { FormEvent } from 'react'
import { api } from '../api'
import type { ByokKey } from '../api'
import { Badge, Banner, EmptyState, Loading, formatTime, useLoad, useToast, useUser } from '../ui'

/** The fee ZeroRouter charges on traffic dispatched with your own key, written
 * as a string rather than computed: the number belongs to the server (it is
 * applied to the metered cost there), and a second definition in TypeScript
 * that could drift from it would be worse than no number at all. */
const BYOK_FEE_LABEL = '5%'

/** Provider aliases rendered as the names customers know them by.
 *
 * A lookup with a passthrough default, not an exhaustive map: the server
 * decides which providers are on offer, and a new lane must show up here as
 * its alias rather than disappearing because this file was not updated. */
const PROVIDER_LABELS: Readonly<Record<string, string>> = {
  anthropic: 'Anthropic',
  openai: 'OpenAI',
  google: 'Google Gemini',
  bedrock: 'AWS Bedrock',
  fireworks: 'Fireworks',
  xai: 'xAI',
}

function providerLabel(provider: string): string {
  return PROVIDER_LABELS[provider] ?? provider
}

/**
 * Bring-your-own-key: attach your own upstream provider credentials, and pay
 * ZeroRouter a fraction of catalog instead of the catalog price.
 *
 * Rendered only when the deployment offers it. `byok_providers` arrives on
 * `/api/me` and is empty when the operator has not provisioned the encryption
 * key, so an unconfigured deployment shows nothing here rather than a form that
 * could only ever be refused.
 */
export function ByokSection() {
  const toast = useToast()
  const user = useUser()
  const offered = user?.byok_providers ?? []
  const keys = useLoad(() => api.byokKeys(), [])
  const [provider, setProvider] = useState('')
  const [apiKey, setApiKey] = useState('')
  const [attaching, setAttaching] = useState(false)
  const [confirming, setConfirming] = useState<string | null>(null)
  const [removing, setRemoving] = useState<string | null>(null)

  if (offered.length === 0) return null

  const chosen = provider === '' ? offered[0] : provider
  const alreadyAttached = (keys.data ?? []).some((key) => key.provider === chosen)

  async function attach(event: FormEvent) {
    event.preventDefault()
    const pasted = apiKey.trim()
    if (pasted === '') {
      toast('Paste the provider API key first.', 'error')
      return
    }
    setAttaching(true)
    try {
      await api.attachByokKey(chosen, pasted)
      // Cleared immediately on success: this input is the only place the
      // plaintext exists in the browser, and leaving it sitting in a form
      // field is the kind of thing that ends up in a screenshot.
      setApiKey('')
      toast(
        alreadyAttached
          ? `Replaced your ${providerLabel(chosen)} key.`
          : `${providerLabel(chosen)} key attached. Requests to ${providerLabel(chosen)} models now use it.`,
        'success',
      )
      keys.reload()
    } catch (err) {
      toast(err instanceof Error ? err.message : 'Could not attach the key.', 'error')
    } finally {
      setAttaching(false)
    }
  }

  async function remove(key: ByokKey) {
    setRemoving(key.provider)
    try {
      await api.removeByokKey(key.provider)
      toast(`${providerLabel(key.provider)} key removed. Those models bill at catalog rates again.`, 'success')
      keys.reload()
    } catch (err) {
      toast(err instanceof Error ? err.message : 'Could not remove the key.', 'error')
    } finally {
      setRemoving(null)
      setConfirming(null)
    }
  }

  return (
    <section className="panel">
      <div className="panel-head">
        <h2>Your own provider keys</h2>
      </div>

      <div className="panel-body">
        <p className="page-sub">
          Attach your own API key for a provider and ZeroRouter will dispatch on it instead of ours.
          You pay the provider directly for the inference, and ZeroRouter charges{' '}
          <strong>{BYOK_FEE_LABEL} of what the same usage would have cost at our catalog rates</strong>,
          taken from your prepaid balance. Your spend caps and rate limits still apply, measured
          against that fee.
        </p>
        <Banner kind="info">
          Requests served on your key are governed by <strong>your</strong> agreement with that
          provider — not by ZeroRouter&rsquo;s. The retention labels on our model list describe our
          contracts, and they do not apply to traffic dispatched on your credentials. Responses to
          those requests are marked <code>byok: true</code> so you can tell them apart.
        </Banner>
      </div>

      <form className="create-row" onSubmit={attach}>
        <select
          className="field"
          aria-label="Provider"
          value={chosen}
          onChange={(e) => setProvider(e.target.value)}
        >
          {offered.map((name) => (
            <option key={name} value={name}>
              {providerLabel(name)}
            </option>
          ))}
        </select>
        <input
          className="field"
          type="password"
          value={apiKey}
          autoComplete="off"
          spellCheck={false}
          placeholder="Paste the provider API key"
          aria-label="Provider API key"
          onChange={(e) => setApiKey(e.target.value)}
        />
        <button className="btn btn-primary" type="submit" disabled={attaching}>
          {attaching ? 'Attaching…' : alreadyAttached ? 'Replace key' : 'Attach key'}
        </button>
      </form>
      <p className="field-hint create-key-hint">
        Paste once — we store it encrypted and never show it again, not even here. To rotate, paste
        the new key over the old one. {alreadyAttached ? `Attaching replaces your current ${providerLabel(chosen)} key.` : ''}
      </p>

      {keys.loading ? (
        <Loading />
      ) : keys.error !== null ? (
        <div className="panel-body">
          <Banner kind="error">{keys.error}</Banner>
        </div>
      ) : keys.data === null || keys.data.length === 0 ? (
        <EmptyState
          title="No provider keys attached"
          hint={`Attach one above to dispatch on your own account and pay ${BYOK_FEE_LABEL} of catalog instead.`}
        />
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Provider</th>
              <th>Key</th>
              <th>Fingerprint</th>
              <th>Attached</th>
              <th>Last used</th>
              <th className="num" aria-label="Actions" />
            </tr>
          </thead>
          <tbody>
            {keys.data.map((k) => (
              <tr key={k.provider}>
                <td>{providerLabel(k.provider)}</td>
                <td className="dim mono nowrap">{`…${k.last4}`}</td>
                <td className="dim mono nowrap">{k.fingerprint}</td>
                <td className="dim nowrap">{formatTime(k.created_at)}</td>
                <td className="dim nowrap">
                  {k.last_used_at !== null ? formatTime(k.last_used_at) : <Badge tone="neutral">never</Badge>}
                </td>
                <td className="num">
                  {confirming === k.provider ? (
                    <span className="confirm-row">
                      <button
                        type="button"
                        className="btn btn-danger btn-sm"
                        disabled={removing === k.provider}
                        onClick={() => void remove(k)}
                      >
                        {removing === k.provider ? 'Removing…' : 'Confirm remove'}
                      </button>
                      <button
                        type="button"
                        className="btn btn-ghost btn-sm"
                        disabled={removing === k.provider}
                        onClick={() => setConfirming(null)}
                      >
                        Cancel
                      </button>
                    </span>
                  ) : (
                    <button
                      type="button"
                      className="btn btn-ghost btn-sm"
                      onClick={() => setConfirming(k.provider)}
                    >
                      Remove
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}

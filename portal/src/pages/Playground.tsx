// The playground: run a prompt against any lane, from the portal, billed to
// the signed-in customer's balance exactly as an API call is.
//
// ---------------------------------------------------------------------------
// HOW THIS PAGE AUTHENTICATES, AND WHY IT MATTERS MORE THAN IT LOOKS
//
// It does NOT call the portal's session-authenticated `/api` plane to run
// inference. It calls `POST /v1/chat/completions` — the public endpoint every
// customer calls — presenting a real `zcr_` key in an `Authorization: Bearer`
// header, over `fetch`, with no CSRF header and no cookie reliance. **The
// playground is a client of the API, not a second server path.** That single
// choice is what makes every invariant apply to it unchanged: the reserve /
// settle pair, the four admission ceilings, BYOK, the retention attestation,
// the metered-actuals billing policy. None of them had to be re-implemented or
// even re-checked, because this page reaches them the same way curl does.
//
// The key comes from `POST /api/playground/key`, which mints an ordinary key
// named "playground" under the same throttle the Keys dialog obeys. See
// `PLAYGROUND_KEY_NAME` in `router/src/portal.rs` for why a session-authorized
// proxy was rejected: admission is keyed on a presenting API key, and a cookie
// carries no key row, no caps, and nothing to attribute the spend to.
//
// The key is held in this origin's `localStorage`. That is a real decision and
// not a shortcut, so it is worth stating the reasoning plainly: anyone who can
// run script on the portal origin can already call `POST /api/keys` with the
// session cookie and mint themselves a key, so storing one here grants an
// attacker nothing they did not already have. What it does buy the customer is
// that the credential is VISIBLE — it appears in their key list, carries the
// ordinary caps, and revoking it there genuinely turns this page off.
//
// Because it lives in a browser, it is also the account's most EXPOSED key, so
// the mint gives it a default credit limit the key dialog does not impose —
// $5.00 a month, `PLAYGROUND_KEY_CREDIT_LIMIT_USD` in `router/src/portal.rs`.
// This page shows that budget rather than leaving the customer to find it on the
// Keys page, and when it runs out it says so in those words and points at the
// edit that raises it. A cap the customer could not lift would be a footgun;
// one they cannot even see is a mystery 402.
// ---------------------------------------------------------------------------

import { useEffect, useMemo, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { Link } from 'react-router'
import { ApiError, api } from '../api'
import type { ApiKey, Model } from '../api'
import {
  Badge,
  Banner,
  EmptyState,
  Loading,
  Stat,
  formatUsd,
  useAuth,
  useLoad,
  useToast,
  useUser,
} from '../ui'
import { RetentionBadge, retentionRank, retentionTitle } from './Models'

// The plaintext playground key, and a note that we have ever held one. The
// second is what keeps a revoke meaningful: without it a customer who revoked
// the key on the Keys page would have it silently re-minted by their next
// keystroke here, and the revoke button would be decorative.
const KEY_STORAGE = 'zr.playground.key'
const MINTED_STORAGE = 'zr.playground.minted'
// Draft only. The conversation itself is deliberately NOT persisted anywhere —
// see the note this page prints for the customer.
const DRAFT_STORAGE = 'zr.playground.draft'
const SYSTEM_STORAGE = 'zr.playground.system'
const MODEL_STORAGE = 'zr.playground.model'

/** Every browser storage read is wrapped: a private window, or a browser set to
 * block site data, throws on access rather than returning null. */
function readStored(key: string): string | null {
  try {
    return window.localStorage.getItem(key)
  } catch {
    return null
  }
}

function writeStored(key: string, value: string | null): void {
  try {
    if (value === null) window.localStorage.removeItem(key)
    else window.localStorage.setItem(key, value)
  } catch {
    // Storage being unavailable costs the customer a remembered draft and a
    // re-mint per session. Neither is worth failing the page over.
  }
}

// ---------------------------------------------------------------------------
// Exact money
//
// The catalog publishes rates as decimal strings, per single token, because a
// JavaScript number cannot hold them. This page multiplies those rates by token
// counts to show a customer what a response cost, so the arithmetic is done on
// integers via BigInt and never touches a float. The result is still labelled an
// ESTIMATE — not because the multiplication is inexact, but because the ledger
// applies things this page cannot see (a BYOK fee multiplier, the free-lane
// skip, the allowance) and the ledger is the record.
// ---------------------------------------------------------------------------

interface Exact {
  mantissa: bigint
  scale: number
}

function parseDecimal(value: string): Exact | null {
  const match = /^(-?)(\d+)(?:\.(\d+))?$/.exec(value.trim())
  if (match === null) return null
  const frac = match[3] ?? ''
  const magnitude = BigInt(`${match[2]}${frac}`)
  return { mantissa: match[1] === '-' ? -magnitude : magnitude, scale: frac.length }
}

/** The rate table that actually applies at this prompt size.
 *
 * Mirrors `RateSchedule::at_prompt_tokens` in the router, including its
 * inclusive comparison: a request measuring exactly `min_prompt_tokens` is
 * priced in the band above. A band REPLACES the base table for the whole
 * request — input and output alike — rather than applying to the tokens past
 * the threshold, so this picks one table and uses it for everything.
 *
 * Reading only `pricing.prompt` / `pricing.completion` would quote half price
 * on a long-context request against a model that reprices, which is exactly the
 * request where the difference is largest. */
function ratesAt(model: Model, promptTokens: number): { prompt: string; completion: string } {
  let table = { prompt: model.pricing.prompt, completion: model.pricing.completion }
  for (const band of model.pricing.overrides ?? []) {
    if (promptTokens >= band.min_prompt_tokens) {
      table = { prompt: band.prompt, completion: band.completion }
    }
  }
  return table
}

/** Exact `Σ tokens × rate`, or null if any rate was unreadable. */
function exactCost(terms: Array<{ tokens: number; rate: string }>): Exact | null {
  const parsed: Array<{ tokens: bigint; rate: Exact }> = []
  for (const term of terms) {
    const rate = parseDecimal(term.rate)
    if (rate === null || !Number.isFinite(term.tokens)) return null
    parsed.push({ tokens: BigInt(Math.max(0, Math.trunc(term.tokens))), rate })
  }
  const scale = parsed.reduce((max, term) => Math.max(max, term.rate.scale), 0)
  let total = 0n
  for (const term of parsed) {
    total += term.tokens * term.rate.mantissa * 10n ** BigInt(scale - term.rate.scale)
  }
  return { mantissa: total, scale }
}

/** Render an exact amount as USD, keeping enough precision to be useful.
 *
 * A playground response often costs a small fraction of a cent, and `$0.00` is
 * not an answer — so this keeps up to six fractional digits, trimming trailing
 * zeros but never showing fewer than two. `formatUsd` in `ui.tsx` stops at four
 * because that is what the ledger and the balance need; this is the one surface
 * that routinely renders smaller numbers than those. */
function formatExactUsd(value: Exact): string {
  const digits = value.mantissa < 0n ? -value.mantissa : value.mantissa
  const text = digits.toString().padStart(value.scale + 1, '0')
  const cut = text.length - value.scale
  const whole = text.slice(0, cut)
  let frac = text.slice(cut).slice(0, 6).padEnd(2, '0')
  if (frac.length > 2) frac = frac.replace(/0+$/, '').padEnd(2, '0')
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',')
  return `${value.mantissa < 0n ? '-' : ''}$${grouped}.${frac}`
}

// ---------------------------------------------------------------------------
// The conversation, which lives in this component and nowhere else
// ---------------------------------------------------------------------------

interface Usage {
  prompt_tokens: number
  completion_tokens: number
}

interface Turn {
  role: 'user' | 'assistant'
  text: string
  /** The data-URI image sent with a user turn, if any. */
  image?: string
  /** Which lane answered — pinned per turn, so a customer who switches lanes
   * mid-conversation still reads the right claim against each response rather
   * than the picker's current value. */
  modelId?: string
  usage?: Usage
  /** ZeroRouter's response-side namespace, when it sent one. Absent on an
   * ordinary request, which is the contract: the block appears only when the
   * priority knob was engaged or the request was BYOK. */
  byok?: boolean
  byokFallback?: boolean
}

interface StreamOutcome {
  text: string
  usage?: Usage
  byok?: boolean
  byokFallback?: boolean
}

/** Read one SSE body to completion, calling `onDelta` as text arrives.
 *
 * The wire is the OpenAI chat-completions stream the router emits: `data:` JSON
 * frames carrying `choices[0].delta.content`, a final usage frame with an empty
 * `choices` array, and `data: [DONE]`. The `zerorouter` block rides that usage
 * frame, which is why this request asks for usage explicitly — without
 * `stream_options.include_usage` the router has nowhere to put either. */
async function readStream(body: ReadableStream<Uint8Array>, onDelta: (delta: string) => void): Promise<StreamOutcome> {
  const reader = body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  const outcome: StreamOutcome = { text: '' }

  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buffer += decoder.decode(value, { stream: true })
    // Frames are separated by a blank line. Anything after the last separator
    // is a partial frame and stays in the buffer.
    const frames = buffer.split('\n\n')
    buffer = frames.pop() ?? ''
    for (const frame of frames) {
      for (const line of frame.split('\n')) {
        if (!line.startsWith('data:')) continue
        const payload = line.slice(5).trim()
        if (payload === '' || payload === '[DONE]') continue
        let parsed: unknown
        try {
          parsed = JSON.parse(payload)
        } catch {
          continue
        }
        const chunk = parsed as {
          choices?: Array<{ delta?: { content?: string } }>
          // NULL on every interim frame, not absent: when the client asks for
          // usage the router stamps `usage: null` on each delta chunk and fills
          // it only on the final one (`stream_delta_json` / `stream_usage_json`
          // in `router/src/openai.rs`), which is the OpenAI convention. Testing
          // for `undefined` alone reads that null as a usage block and throws
          // on the first delta — which is exactly what it did.
          usage?: { prompt_tokens?: number; completion_tokens?: number } | null
          zerorouter?: { byok?: boolean; byok_fallback?: boolean } | null
          error?: { message?: string }
        }
        // An error frame mid-stream: the router shipped headers before the walk
        // resolved, so a failure after that point arrives in-band. Surfacing it
        // as a thrown error puts it in the same place as a pre-stream refusal.
        if (chunk.error?.message !== undefined) throw new Error(chunk.error.message)
        const delta = chunk.choices?.[0]?.delta?.content
        if (typeof delta === 'string' && delta.length > 0) {
          outcome.text += delta
          onDelta(delta)
        }
        if (chunk.usage != null) {
          outcome.usage = {
            prompt_tokens: chunk.usage.prompt_tokens ?? 0,
            completion_tokens: chunk.usage.completion_tokens ?? 0,
          }
        }
        if (chunk.zerorouter != null) {
          outcome.byok = chunk.zerorouter.byok === true
          outcome.byokFallback = chunk.zerorouter.byok_fallback === true
        }
      }
    }
  }
  return outcome
}

/** The message array sent upstream. An image rides the OpenAI content-parts
 * shape, which is what the router's modality gate reads (`image_url` parts are
 * what make a request need the `image` modality). */
function wireMessages(system: string, turns: Turn[], pending: { text: string; image: string | null }) {
  const messages: Array<{ role: string; content: unknown }> = []
  if (system.trim() !== '') messages.push({ role: 'system', content: system })
  for (const turn of turns) {
    if (turn.image !== undefined) {
      messages.push({
        role: turn.role,
        content: [
          { type: 'text', text: turn.text },
          { type: 'image_url', image_url: { url: turn.image } },
        ],
      })
    } else {
      messages.push({ role: turn.role, content: turn.text })
    }
  }
  if (pending.image !== null) {
    messages.push({
      role: 'user',
      content: [
        { type: 'text', text: pending.text },
        { type: 'image_url', image_url: { url: pending.image } },
      ],
    })
  } else {
    messages.push({ role: 'user', content: pending.text })
  }
  return messages
}

function takesImages(model: Model | undefined): boolean {
  // Absent means UNKNOWN, never "text only" — the same contract the router's
  // own gate keeps (`unservable_modality`: a lane that declares nothing serves
  // everything). Several shipped lanes omit the field deliberately and do take
  // images, so treating silence as a refusal here would make this page stricter
  // than the product.
  if (model?.input_modalities == null) return true
  return model.input_modalities.includes('image')
}

export function Playground() {
  const user = useUser()
  const { refresh } = useAuth()
  const toast = useToast()
  const models = useLoad(() => api.models(), [])

  const [search, setSearch] = useState('')
  const [modelId, setModelId] = useState(() => readStored(MODEL_STORAGE) ?? '')
  const [system, setSystem] = useState(() => readStored(SYSTEM_STORAGE) ?? '')
  const [draft, setDraft] = useState(() => readStored(DRAFT_STORAGE) ?? '')
  const [image, setImage] = useState<string | null>(null)
  const [imageName, setImageName] = useState<string | null>(null)
  const [turns, setTurns] = useState<Turn[]>([])
  const [streaming, setStreaming] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // Set when the stored key stopped authenticating. The customer revoked it (or
  // it lapsed), and the page must ask before minting another rather than
  // undoing what they just did on the Keys page.
  const [keyRevoked, setKeyRevoked] = useState(false)
  const [enabling, setEnabling] = useState(false)
  // Set when the router refused with `key_credit_limit_exceeded` — the key's own
  // budget, not the account balance. Kept apart from `error` because the two
  // want different words and different next steps: one is "add credits", the
  // other is "raise this key's limit", and telling a customer with $40 of credit
  // that they are out of money is the failure worth avoiding here.
  const [budgetSpent, setBudgetSpent] = useState(false)
  const transcriptRef = useRef<HTMLDivElement | null>(null)

  // The playground key's budget, read from the server rather than remembered
  // from the mint: the customer may have raised it on the Keys page since, and
  // the used figure only exists server-side anyway (it comes from the same
  // derived counters admission enforces against). Absent when this browser has
  // never minted one, which is the state before the first send.
  const keys = useLoad(() => api.keys(), [])
  const playgroundRow: ApiKey | null =
    keys.data?.find((key) => key.name === 'playground' && !key.disabled) ?? null

  useEffect(() => writeStored(DRAFT_STORAGE, draft === '' ? null : draft), [draft])
  useEffect(() => writeStored(SYSTEM_STORAGE, system === '' ? null : system), [system])
  useEffect(() => writeStored(MODEL_STORAGE, modelId === '' ? null : modelId), [modelId])

  const lanes: Model[] = useMemo(
    () =>
      (models.data ?? [])
        // The reserved `zero/*` routing aliases are not models to buy, exactly
        // as the storefront has it.
        .filter((m) => m.owned_by !== 'zerorouter')
        .sort((a, b) =>
          retentionRank(a) !== retentionRank(b)
            ? retentionRank(a) - retentionRank(b)
            : a.id.localeCompare(b.id),
        ),
    [models.data],
  )

  const matching = useMemo(() => {
    const needle = search.trim().toLowerCase()
    if (needle === '') return lanes
    return lanes.filter(
      (m) => m.id.toLowerCase().includes(needle) || m.owned_by.toLowerCase().includes(needle),
    )
  }, [lanes, search])

  const zeroLanes = matching.filter((m) => retentionRank(m) === 0)
  const standardLanes = matching.filter((m) => retentionRank(m) !== 0)
  const selected = lanes.find((m) => m.id === modelId)

  // Default to the first zero-retention lane once the catalog arrives: the
  // brand's own ordering, applied to the thing the page does rather than only
  // to how it lists.
  useEffect(() => {
    if (modelId === '' && lanes.length > 0) setModelId(lanes[0].id)
    // Selecting a lane that has since left the catalog would send a request
    // that 404s on the first try, so fall back rather than keep a dead id.
    else if (modelId !== '' && lanes.length > 0 && !lanes.some((m) => m.id === modelId)) {
      setModelId(lanes[0].id)
    }
  }, [lanes, modelId])

  useEffect(() => {
    const node = transcriptRef.current
    if (node !== null) node.scrollTop = node.scrollHeight
  }, [turns, streaming])

  const balance = user?.credit_balance_usd ?? '0'
  const hasBalance = (parseDecimal(balance)?.mantissa ?? 0n) > 0n

  /** The key this page presents, minting one if we have never held one.
   *
   * `allowMint` is false on the path where a stored key stopped working: that
   * is a revoke, and answering it with a fresh key would make the Keys page's
   * revoke button a no-op. */
  async function playgroundKey(allowMint: boolean): Promise<string | null> {
    const stored = readStored(KEY_STORAGE)
    if (stored !== null && stored !== '') return stored
    if (!allowMint) return null
    const minted = await api.ensurePlaygroundKey()
    writeStored(KEY_STORAGE, minted.api_key)
    writeStored(MINTED_STORAGE, '1')
    return minted.api_key
  }

  async function enable() {
    setEnabling(true)
    setError(null)
    try {
      const minted = await api.ensurePlaygroundKey()
      writeStored(KEY_STORAGE, minted.api_key)
      writeStored(MINTED_STORAGE, '1')
      setKeyRevoked(false)
      toast('The playground has a new key. The old one no longer works.', 'success')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not create a playground key.')
    } finally {
      setEnabling(false)
    }
  }

  async function send(event: FormEvent) {
    event.preventDefault()
    const prompt = draft.trim()
    if (prompt === '' || selected === undefined || streaming) return

    setError(null)
    setBudgetSpent(false)
    // The key was minted before this browser ever had one, so the FIRST send is
    // where it happens — the customer asked to run something, which is the
    // moment the credential is actually needed.
    let key: string | null
    try {
      key = await playgroundKey(!keyRevoked && readStored(MINTED_STORAGE) === null)
    } catch (err) {
      setError(
        err instanceof ApiError && err.code === 'key_limit_reached'
          ? `${err.message} The playground needs one of your key slots; revoke a key you no longer use and try again.`
          : err instanceof Error
            ? err.message
            : 'Could not create a playground key.',
      )
      return
    }
    if (key === null) {
      setKeyRevoked(true)
      return
    }

    const sent: Turn = {
      role: 'user',
      text: prompt,
      ...(image !== null ? { image } : {}),
    }
    const pending = { text: prompt, image }
    const history = turns
    setTurns([...history, sent, { role: 'assistant', text: '', modelId: selected.id }])
    setDraft('')
    setImage(null)
    setImageName(null)
    setStreaming(true)

    try {
      // Straight at the public API with a key. No CSRF header, no credentials —
      // this is the request a customer's own client would send, and sending it
      // any other way would be testing a path they do not have.
      const response = await fetch('/v1/chat/completions', {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          authorization: `Bearer ${key}`,
        },
        body: JSON.stringify({
          model: selected.id,
          messages: wireMessages(system, history, pending),
          stream: true,
          // Both halves of what this page shows after a response ride the final
          // usage frame: the token counts, and the `zerorouter` block carrying
          // BYOK disclosure. Without this the router has nowhere to put either.
          stream_options: { include_usage: true },
        }),
      })

      if (!response.ok || response.body === null) {
        const detail = (await response.json().catch(() => null)) as {
          error?: { message?: string; code?: string }
        } | null
        const code = detail?.error?.code ?? `http_${response.status}`
        // The router distinguishes "this key's own budget is gone" from "this
        // account is out of credit" with two different codes, and the whole
        // point of that distinction is that they need different sentences. A
        // customer whose $5 playground budget ran out has not run out of money.
        if (code === 'key_credit_limit_exceeded') {
          setBudgetSpent(true)
          setTurns(history)
          setDraft(prompt)
          keys.reload()
          return
        }
        if (code === 'invalid_api_key') {
          // The key in this browser no longer authenticates — revoked on the
          // Keys page, most likely. Drop it and ask; do not quietly mint.
          writeStored(KEY_STORAGE, null)
          setKeyRevoked(true)
          setTurns(history)
          setDraft(prompt)
          return
        }
        throw new Error(
          detail?.error?.message ?? `The request failed (${response.status}).`,
        )
      }

      const outcome = await readStream(response.body, (delta) => {
        setTurns((current) => {
          const next = [...current]
          const last = next[next.length - 1]
          if (last?.role === 'assistant') next[next.length - 1] = { ...last, text: last.text + delta }
          return next
        })
      })
      setTurns((current) => {
        const next = [...current]
        const last = next[next.length - 1]
        if (last?.role === 'assistant') {
          next[next.length - 1] = {
            ...last,
            text: outcome.text,
            usage: outcome.usage,
            byok: outcome.byok,
            byokFallback: outcome.byokFallback,
          }
        }
        return next
      })
      // The balance moved, and so did the key's budget. Re-read BOTH from the
      // server rather than subtracting an estimate here: what a request cost is
      // the ledger's answer, and this page must not invent a second one — least
      // of all for the number that decides whether the next send is refused.
      void refresh()
      keys.reload()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'The request failed.')
      // Drop the empty assistant turn; keep what the customer typed so a
      // refusal does not cost them their prompt.
      setTurns(history)
      setDraft(prompt)
    } finally {
      setStreaming(false)
    }
  }

  function attach(file: File | undefined) {
    if (file === undefined) return
    // 4 MB of base64 is comfortably inside the router's 8 MiB body limit while
    // leaving room for the conversation around it.
    if (file.size > 4 * 1024 * 1024) {
      toast('That image is larger than 4 MB. Try a smaller one.', 'error')
      return
    }
    const reader = new FileReader()
    reader.onload = () => {
      if (typeof reader.result === 'string') {
        setImage(reader.result)
        setImageName(file.name)
      }
    }
    reader.readAsDataURL(file)
  }

  if (user === null) return null

  return (
    <div className="page">
      <header className="page-head">
        <h1>Playground</h1>
        <p className="page-sub">
          Run a prompt against any lane and see what it costs. Requests go through the same{' '}
          <span className="mono">/v1/chat/completions</span> your own client calls, with a real key
          and your own credits — there is no free path here.
        </p>
      </header>

      {error !== null && (
        <Banner kind="error" onDismiss={() => setError(null)}>
          {error}
        </Banner>
      )}

      {/* The budget refusal, in its own words. Not an error banner: nothing went
          wrong, a limit the customer's own account set did exactly what it was
          set to do — and the remedy is one they own, on a page linked from
          here. Stating the balance is untouched matters, because a 402 on a page
          that spends credits reads as "out of money" unless it says otherwise. */}
      {budgetSpent && (
        <Banner kind="info">
          This playground key has used its whole credit limit
          {playgroundRow?.credit_limit_usd != null
            ? ` of ${formatUsd(playgroundRow.credit_limit_usd)}`
            : ''}
          {playgroundRow?.credit_limit_window === 'monthly' ? ' for this month' : ''}. Your account
          balance of {formatUsd(balance)} is untouched — this is the key&rsquo;s own budget, which
          the playground sets to a modest default because the key lives in this browser. Raise it on
          the <Link to="/keys">Keys</Link> page, or wait for the limit to reset.
        </Banner>
      )}

      {keyRevoked && (
        <Banner kind="info">
          Your playground key was revoked, so this page cannot run anything until it has a new one.
          Creating one mints a key named <span className="mono">playground</span> in your account —
          you can see it and revoke it again on the <Link to="/keys">Keys</Link> page.{' '}
          <button type="button" className="btn btn-primary btn-sm" onClick={() => void enable()} disabled={enabling}>
            {enabling ? 'Creating…' : 'Create a playground key'}
          </button>
        </Banner>
      )}

      <section className="stats">
        <Stat label="Credit balance" value={formatUsd(balance)} />
        {/* THE KEY'S OWN BUDGET, beside the account balance and distinct from
            it, because they are two different ceilings and only one of them is
            about money the customer has. Rendered from the server's
            `credit_limit_used_usd`, which is computed from the same counters
            admission enforces against — so this tile can never show a figure
            that disagrees with the one refusing a request.

            Absent before this browser has minted a key, and "no limit" for a
            playground key minted before the default existed: those are not
            retroactively capped, so the page must not claim they are. */}
        {playgroundRow !== null && (
          <Stat
            label="Playground key budget"
            value={
              playgroundRow.credit_limit_usd === null
                ? 'no limit'
                : `${formatUsd(playgroundRow.credit_limit_used_usd ?? '0')} of ${formatUsd(
                    playgroundRow.credit_limit_usd,
                  )}`
            }
            sub={
              playgroundRow.credit_limit_usd === null
                ? 'this key predates the default'
                : playgroundRow.credit_limit_window === null
                  ? 'does not reset'
                  : `resets ${playgroundRow.credit_limit_window}`
            }
          />
        )}
        <Stat
          label="Serving lane"
          value={selected === undefined ? '—' : <span className="mono">{selected.id}</span>}
          sub={selected?.owned_by}
        />
        <Stat
          label="Retention"
          value={selected === undefined ? '—' : <RetentionBadge model={selected} />}
          sub={selected?.retention?.verified ? `verified ${selected.retention.verified}` : undefined}
        />
      </section>

      {!hasBalance && (
        <section className="panel">
          <EmptyState
            title="You have no credits yet."
            hint="The playground spends real credits at the catalog rate, exactly as an API call does — so it needs a balance before it can run anything."
            action={
              <Link className="btn btn-primary" to="/credits">
                Add credits
              </Link>
            }
          />
        </section>
      )}

      <div className="playground">
        <section className="panel playground-lanes">
          <div className="panel-head">
            <h2>Lane</h2>
          </div>
          <div className="panel-body">
            <input
              className="field"
              value={search}
              placeholder="Search models — e.g. haiku, google, deepseek"
              aria-label="Search models"
              onChange={(e) => setSearch(e.target.value)}
            />
          </div>
          {models.loading ? (
            <Loading label="Loading the catalog" />
          ) : models.error !== null ? (
            <div className="panel-body">
              <Banner kind="error">{models.error}</Banner>
            </div>
          ) : matching.length === 0 ? (
            <EmptyState title="No lane matches that search." />
          ) : (
            <div className="lane-groups">
              {/* GROUPED BY RETENTION POSTURE, zero first — the same statement
                  the catalog makes by its ordering, made here as a heading
                  because a picker has room to say it outright. The group a lane
                  sits in is decided by `retentionRank`, shared with the models
                  page so the two can never disagree about which half a lane is
                  in. */}
              {[
                { key: 'zero', title: 'Zero retention', lanes: zeroLanes },
                { key: 'standard', title: 'Provider retains data', lanes: standardLanes },
              ].map((group) =>
                group.lanes.length === 0 ? null : (
                  <div className="lane-group" key={group.key} data-posture={group.key}>
                    <h3 className="lane-group-title">
                      {group.title}
                      <span className="lane-group-count">{group.lanes.length}</span>
                    </h3>
                    <ul className="lane-list">
                      {group.lanes.map((m) => (
                        <li key={m.id}>
                          <button
                            type="button"
                            className={`lane${m.id === modelId ? ' lane-active' : ''}`}
                            aria-pressed={m.id === modelId}
                            title={retentionTitle(m)}
                            onClick={() => setModelId(m.id)}
                          >
                            <span className="lane-id mono">{m.id}</span>
                            <span className="lane-meta">
                              <RetentionBadge model={m} />
                              {!takesImages(m) && <Badge tone="neutral">text only</Badge>}
                            </span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  </div>
                ),
              )}
            </div>
          )}
        </section>

        <section className="panel playground-chat">
          <div className="panel-head">
            <h2>Conversation</h2>
            {turns.length > 0 && (
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                onClick={() => setTurns([])}
                disabled={streaming}
              >
                Clear
              </button>
            )}
          </div>

          <div className="panel-body">
            <label className="field-label" htmlFor="playground-system">
              System message
            </label>
            <textarea
              id="playground-system"
              className="field playground-system"
              value={system}
              rows={2}
              placeholder="Optional. Sets the assistant's behaviour for every turn below."
              onChange={(e) => setSystem(e.target.value)}
            />
          </div>

          <div className="transcript" ref={transcriptRef}>
            {turns.length === 0 ? (
              <EmptyState
                title="Nothing sent yet."
                hint="Pick a lane, write a prompt, and send it. Everything below stays in this tab."
              />
            ) : (
              turns.map((turn, index) => (
                <Turnstile key={index} turn={turn} lanes={lanes} streaming={streaming && index === turns.length - 1} />
              ))
            )}
          </div>

          <form className="composer" onSubmit={send}>
            {image !== null && (
              <div className="attachment">
                <img src={image} alt="Attached" className="attachment-thumb" />
                <span className="attachment-name">{imageName}</span>
                {/* Not filtered out of the picker — see `takesImages`. The
                    catalog's silence about a lane's modalities means unknown,
                    and the router serves those requests, so hiding the lane here
                    would refuse something the product accepts. A lane that
                    DECLARES it takes only text gets this warning instead, and
                    the send still goes through: the server is the authority and
                    its refusal is rendered above. */}
                {!takesImages(selected) && (
                  <span className="attachment-warn">
                    {selected?.id} lists no image input. Sending this will be refused.
                  </span>
                )}
                <button
                  type="button"
                  className="btn btn-ghost btn-sm"
                  onClick={() => {
                    setImage(null)
                    setImageName(null)
                  }}
                >
                  Remove
                </button>
              </div>
            )}
            <textarea
              className="field composer-text"
              value={draft}
              rows={3}
              placeholder="Write a prompt. Enter sends; Shift+Enter adds a line."
              aria-label="Prompt"
              disabled={!hasBalance}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault()
                  void send(e as unknown as FormEvent)
                }
              }}
            />
            <div className="composer-actions">
              <label className="btn btn-ghost btn-sm attach-button">
                Attach image
                <input
                  type="file"
                  accept="image/*"
                  aria-label="Attach an image"
                  onChange={(e) => {
                    attach(e.target.files?.[0])
                    e.target.value = ''
                  }}
                />
              </label>
              <button
                type="submit"
                className="btn btn-primary"
                disabled={streaming || draft.trim() === '' || selected === undefined || !hasBalance}
              >
                {streaming ? 'Streaming…' : 'Send'}
              </button>
            </div>
          </form>
        </section>
      </div>

      {/* The one thing a customer of a zero-retention brand should not have to
          ask about. Stated on the page rather than in a policy document,
          because the page is where the question occurs to them. */}
      <p className="page-note">
        <strong>Conversations live in this tab only.</strong> Nothing here is saved to your account,
        shared, or readable by ZeroRouter after it answers — reload the page and it is gone. What
        persists is the metered usage every request writes, which you can read on{' '}
        <Link to="/credits">Credits</Link>.
      </p>
      <p className="page-note">
        Costs shown against each response are <strong>estimated</strong> from the catalog's published
        rates and the tokens the provider reported. The ledger is the record — a bring-your-own-key
        request bills at a fraction of catalog, and this page does not try to guess which figure
        yours landed on.
      </p>
    </div>
  )
}

/** One turn, with the claim ZeroRouter makes about it attached.
 *
 * The badge is read from the lane that ANSWERED — pinned on the turn when it
 * was sent — rather than from whatever the picker says now, so scrolling back
 * through a conversation where the customer switched lanes shows each response
 * under its own posture. */
function Turnstile({ turn, lanes, streaming }: { turn: Turn; lanes: Model[]; streaming: boolean }) {
  const model = lanes.find((m) => m.id === turn.modelId)
  const cost =
    turn.usage === undefined || model === undefined
      ? null
      : (() => {
          const rates = ratesAt(model, turn.usage.prompt_tokens)
          return exactCost([
            { tokens: turn.usage.prompt_tokens, rate: rates.prompt },
            { tokens: turn.usage.completion_tokens, rate: rates.completion },
          ])
        })()

  return (
    <div className={`turn turn-${turn.role}`}>
      <div className="turn-head">
        <span className="turn-role">{turn.role === 'user' ? 'You' : 'Assistant'}</span>
        {turn.role === 'assistant' && model !== undefined && (
          <span className="turn-lane" title={retentionTitle(model)}>
            <span className="mono dim">{model.id}</span>
            <RetentionBadge model={model} />
            {/* BYOK disclosure, surfaced only when the response carried it.
                Two separate facts, never both: `byok` means the request went out
                on the customer's own credential under their agreement with the
                provider — so ZeroRouter's retention label above describes
                ZeroRouter's contract, not theirs — while `byok_fallback` means
                their key failed and ZeroRouter's served it, at full catalog
                price and outside the monthly allowance. */}
            {turn.byok === true && <Badge tone="accent">your own key</Badge>}
            {turn.byokFallback === true && (
              <Badge tone="accent">fell back to ZeroRouter's key</Badge>
            )}
          </span>
        )}
      </div>
      {turn.image !== undefined && <img src={turn.image} alt="Attached" className="turn-image" />}
      <div className="turn-body">
        {turn.text}
        {streaming && turn.role === 'assistant' && <span className="caret" />}
      </div>
      {turn.usage !== undefined && (
        <div className="turn-usage">
          <span>
            {turn.usage.prompt_tokens.toLocaleString('en-US')} in ·{' '}
            {turn.usage.completion_tokens.toLocaleString('en-US')} out
          </span>
          {cost !== null && (
            <span className="turn-cost" title="Estimated from the catalog rate; the ledger is the record.">
              {formatExactUsd(cost)} <span className="dim">estimated</span>
            </span>
          )}
        </div>
      )}
      {turn.byok === true && (
        <p className="turn-note">
          Served on your own provider key. ZeroRouter's retention label describes ZeroRouter's
          agreement with that provider, not yours — this request is governed by your own.
        </p>
      )}
      {turn.byokFallback === true && (
        <p className="turn-note">
          Your provider key did not answer, so ZeroRouter's credential served this. It bills at the
          full catalog price and does not draw on your monthly allowance.
        </p>
      )}
    </div>
  )
}

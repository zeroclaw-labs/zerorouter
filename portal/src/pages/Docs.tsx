// The public API reference. Everything on this page is a claim ZeroRouter
// makes to anyone who reads it — signed in or not — so every endpoint, field,
// and error code here was checked against the router source rather than
// remembered:
//
//   routes                 router/src/api.rs        `app()`
//   error codes and shapes router/src/error.rs      `response_parts()`
//   accepted request body  router/src/openai.rs     `ChatCompletionRequest`
//   key format             router/src/auth.rs       `generate_api_key`
//   catalog row shape      router/src/openai.rs     `ModelObject`
//   CLI                    docs/USER-CLI.md
//
// Write timelessly. Postures, prices, and the model line-up move; the concepts
// do not. Nothing here hardcodes a count of lanes or a price — those live on
// the models page, which reads them from the live catalog. The one model id
// used in examples is pinned by a whole-row wire test
// (`router/tests/http.rs::models_are_materialized_from_tiers_toml`), so if it
// ever leaves the catalog that test fails first.

import { Link } from 'react-router-dom'
import { CodeBlock } from '../ui'

/** The base URL every example uses. Same origin as this portal in production. */
const BASE_URL = 'https://zerorouter.ai'

/** A model id that exists in the shipped catalog, used throughout the examples.
 * Examples with a made-up id are worse than no examples: the first thing a
 * reader does is paste one, and a 404 on step one reads as "the gateway is
 * broken" rather than "that was a placeholder".
 *
 * Exported so the curl the Keys page hands over with a freshly minted key names
 * the same model this page does — two copies would eventually name a lane that
 * had left the catalog, in whichever file nobody was looking at. */
export const EXAMPLE_MODEL = 'anthropic/claude-haiku-4-5'

const CURL = `curl ${BASE_URL}/v1/chat/completions \\
  -H "Authorization: Bearer $ZEROROUTER_API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${EXAMPLE_MODEL}",
    "messages": [{"role": "user", "content": "Say hello in five words."}]
  }'`

const PYTHON = `import os

from openai import OpenAI

client = OpenAI(
    base_url="${BASE_URL}/v1",
    api_key=os.environ["ZEROROUTER_API_KEY"],  # zcr_...
)

response = client.chat.completions.create(
    model="${EXAMPLE_MODEL}",
    messages=[{"role": "user", "content": "Say hello in five words."}],
)
print(response.choices[0].message.content)`

const TYPESCRIPT = `import OpenAI from 'openai'

const client = new OpenAI({
  baseURL: '${BASE_URL}/v1',
  apiKey: process.env.ZEROROUTER_API_KEY, // zcr_...
})

const response = await client.chat.completions.create({
  model: '${EXAMPLE_MODEL}',
  messages: [{ role: 'user', content: 'Say hello in five words.' }],
})
console.log(response.choices[0].message.content)`

const STREAM_REQUEST = `curl ${BASE_URL}/v1/chat/completions \\
  -H "Authorization: Bearer $ZEROROUTER_API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${EXAMPLE_MODEL}",
    "messages": [{"role": "user", "content": "Count to three."}],
    "stream": true,
    "stream_options": {"include_usage": true}
  }'`

const STREAM_RESPONSE = `data: {"id":"...","object":"chat.completion.chunk","created":1766000000,"model":"${EXAMPLE_MODEL}","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}

data: {"id":"...","object":"chat.completion.chunk","created":1766000000,"model":"${EXAMPLE_MODEL}","choices":[{"index":0,"delta":{"content":"One"},"finish_reason":null}]}

data: {"id":"...","object":"chat.completion.chunk","created":1766000000,"model":"${EXAMPLE_MODEL}","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"...","object":"chat.completion.chunk","created":1766000000,"model":"${EXAMPLE_MODEL}","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}}

data: [DONE]`

const ERROR_BODY = `{
  "error": {
    "message": "This API key has reached the credit limit set on it. The account balance is unaffected; the limit resets on the key's own schedule.",
    "type": "billing_error",
    "param": null,
    "code": "key_credit_limit_exceeded"
  }
}`

const STREAM_ERROR = `data: {"error":{"message":"...","type":"server_error","param":null,"code":"retention_attestation_failed"}}

data: [DONE]`

const MODELS_ROW = `{
  "id": "${EXAMPLE_MODEL}",
  "object": "model",
  "created": 0,
  "owned_by": "anthropic",
  "pricing": {
    "prompt": "0.000001",
    "completion": "0.000005",
    "input_cache_read": "0.0000001"
  },
  "context_length": 200000,
  "max_output_tokens": 64000,
  "input_modalities": ["text", "image", "pdf"],
  "tool_call": true,
  "retention": {
    "posture": "standard",
    "description": "Anthropic deletes API inputs and outputs from its backend within 30 days; ...",
    "verified": "2026-08-20"
  }
}`

const CLI = `zerorouter user login     # RFC 8628 device flow; stores the credential
zerorouter user whoami    # which router this machine is logged in to
zerorouter user models    # the catalog, with rates and retention postures
zerorouter user logout    # remove the credential

zerorouter user models --json | jq '.data[] | select(.retention.posture == "zero") | .id'`

/** One row of the error table: the code a client branches on, the status it
 * arrives with, and what to actually do about it. */
interface ErrorRow {
  code: string
  status: string
  meaning: string
}

/** Straight from `ApiError::response_parts` in `router/src/error.rs` — status,
 * `type`, and `code` are read off that match arm, not paraphrased. The list is
 * deliberately not exhaustive: it is the codes a caller can act on. */
const ERROR_ROWS: ReadonlyArray<ErrorRow> = [
  {
    code: 'invalid_api_key',
    status: '401 · authentication_error',
    meaning:
      'The key is unknown, revoked, or past its expiry. All three answer identically on purpose — a probe cannot learn which. Mint a fresh key in the portal.',
  },
  {
    code: 'insufficient_credits',
    status: '402 · billing_error',
    meaning:
      'The account balance cannot cover the request. Add credits. Nothing was dispatched and nothing was charged.',
  },
  {
    code: 'key_credit_limit_exceeded',
    status: '402 · billing_error',
    meaning:
      'This key spent the credit limit its owner set on it, for the current reset window. The account balance is untouched, so buying credit does not help — rotate to another key, raise this key’s limit, or wait for its window to reset.',
  },
  {
    code: 'spend_cap_exceeded',
    status: '402 · billing_error',
    meaning:
      'The operator ceiling on the key bound, not your own limit. Where both are set, the tighter one is the one reported.',
  },
  {
    code: 'account_frozen',
    status: '402 · billing_error',
    meaning:
      'The account is on hold, usually after a payment dispute. Adding credit will not lift it; contact support.',
  },
  {
    code: 'velocity_cap_exceeded',
    status: '429 · rate_limit_error',
    meaning:
      'The key passed its tokens-per-minute cap. Back off and retry; the window is a rolling minute.',
  },
  {
    code: 'model_not_found',
    status: '404 · invalid_request_error',
    meaning:
      'No such id in the catalog. Read GET /v1/models for the exact spellings — ids are {vendor}/{model} and the vendor prefix is part of the id.',
  },
  {
    code: 'model_unavailable',
    status: '503 · server_error',
    meaning:
      'The model is in the catalog but this deployment cannot serve that lane — a missing upstream credential, or a tier withheld because it is priced below its own cost. A ZeroRouter-side fault, and retrying will not clear it. Every other model is unaffected.',
  },
  {
    code: 'retention_attestation_failed',
    status: '502 · server_error',
    meaning:
      'The upstream answered without confirming the zero-data-retention guarantee the lane is sold under, so the request was refused rather than served. Nothing reached you and nothing was billed. Not retried, and not retryable — see below.',
  },
  {
    code: 'unsupported_request_fields',
    status: '400 · invalid_request_error',
    meaning:
      'The body carried a field or a structured content block the compat surface does not preserve. See “What a request body may contain”.',
  },
  {
    code: 'request_too_large',
    status: '413 · invalid_request_error',
    meaning: 'The body is over 8 MiB.',
  },
  {
    code: 'server_overloaded',
    status: '503 · server_error',
    meaning:
      'The router is already buffering as many bodies as it will hold. Shed deliberately rather than queued; retry shortly.',
  },
  {
    code: 'upstream_unavailable',
    status: '502 · server_error',
    meaning:
      'Every upstream candidate for the lane failed. Unlike the two above, this one is worth a retry.',
  },
]

/** The public API reference: what the base URL is, how to authenticate, what
 * the catalog publishes, and which errors a caller should branch on. Public
 * like the model catalog — a prospective customer can read the whole surface
 * before signing in, and an existing one should never have to guess it. */
export function Docs() {
  return (
    <article className="docs">
      <h1>API documentation</h1>
      <p className="docs-lede">
        ZeroRouter speaks the OpenAI chat-completions wire. Point a client at{' '}
        <span className="mono">{BASE_URL}/v1</span>, send a{' '}
        <span className="mono">zcr_</span> key as a bearer token, and request a model by its
        catalog id. Inference is billed at the provider’s own rate against a prepaid balance —
        there is no per-request markup and no invoice at the end of the month.
      </p>

      <nav className="docs-toc" aria-label="On this page">
        <a href="#quickstart">Quickstart</a>
        <a href="#models">Models &amp; retention</a>
        <a href="#streaming">Streaming</a>
        <a href="#keys">Keys</a>
        <a href="#errors">Errors worth knowing</a>
        <a href="#billing">How you are charged</a>
        <a href="#cli">Command line</a>
      </nav>

      {/* ── Quickstart ──────────────────────────────────────────────────── */}

      <h2 id="quickstart">Quickstart</h2>
      <p>Three steps, then a request.</p>
      <ol className="docs-steps">
        <li>
          <strong>Sign in with SSO.</strong> An account is created the first time you arrive; there
          is no separate sign-up.
        </li>
        <li>
          <strong>Add credits.</strong> Credits are prepaid and denominated in dollars. A deposit
          fee applies when you buy them; inference itself is passed through at cost.
        </li>
        <li>
          <strong>Mint a key</strong> on the <Link to="/keys">Keys</Link> page. The plaintext key
          is shown exactly once — the server keeps only a SHA-256 digest of it. Keys look like{' '}
          <span className="mono">zcr_</span> followed by 64 hex characters.
        </li>
      </ol>

      <CodeBlock label="curl">{CURL}</CodeBlock>

      <p>
        The same request from the official OpenAI SDKs. Only two settings change — the base URL and
        the key — and nothing else about the call site has to move.
      </p>

      <CodeBlock label="Python · openai">{PYTHON}</CodeBlock>
      <CodeBlock label="JavaScript / TypeScript · openai">{TYPESCRIPT}</CodeBlock>

      <p>
        Any client that already speaks the OpenAI chat-completions API works the same way: the LLM
        libraries, the agent frameworks, the editor plugins, anything that lets you set a base URL
        and a key. There is no ZeroRouter SDK to install and none is planned — a gateway that
        needed its own client library would not be compatible in the way that matters.
      </p>

      <h3>What a request body may contain</h3>
      <p>
        The compat surface is deliberately narrow, and it is strict: a field ZeroRouter cannot
        faithfully carry to every upstream on the lane is refused with{' '}
        <span className="mono">400 unsupported_request_fields</span> rather than accepted and
        quietly dropped. A silently ignored <span className="mono">temperature</span> is worse than
        an error, because you would never learn the request you tuned is not the request that ran.
      </p>
      <ul>
        <li>
          <strong>Accepted:</strong> <span className="mono">model</span>,{' '}
          <span className="mono">messages</span>, <span className="mono">stream</span>,{' '}
          <span className="mono">stream_options.include_usage</span>,{' '}
          <span className="mono">temperature</span>, <span className="mono">max_tokens</span>,{' '}
          <span className="mono">tools</span>, and{' '}
          <span className="mono">tool_choice: &quot;auto&quot;</span>.
        </li>
        <li>
          <strong>Messages</strong> carry a <span className="mono">role</span> of{' '}
          <span className="mono">system</span>, <span className="mono">user</span>,{' '}
          <span className="mono">assistant</span>, or <span className="mono">tool</span>, and{' '}
          <span className="mono">content</span> as a <em>string</em> —{' '}
          <span className="mono">tool_calls</span>, <span className="mono">tool_call_id</span>,{' '}
          <span className="mono">name</span>, and <span className="mono">reasoning_content</span>{' '}
          come along for multi-turn tool use. Structured content arrays are not accepted yet, so
          image and file parts are a 400 today even on lanes whose catalog row advertises those
          modalities.
        </li>
        <li>
          <strong>Anything else at the top level</strong> — <span className="mono">top_p</span>,{' '}
          <span className="mono">frequency_penalty</span>, <span className="mono">n</span>,{' '}
          <span className="mono">stop</span>, <span className="mono">seed</span>,{' '}
          <span className="mono">response_format</span>, <span className="mono">logprobs</span> —
          is a 400. If your client sets one of these by default, unset it.
        </li>
        <li>
          <span className="mono">cache_control</span> passthrough has its own code,{' '}
          <span className="mono">400 cache_control_unsupported</span>, so a prompt-caching client
          learns that specifically rather than reading a generic refusal.
        </li>
        <li>
          Bodies are capped at <strong>8 MiB</strong> and must arrive within 30 seconds.
        </li>
      </ul>

      {/* ── Models ──────────────────────────────────────────────────────── */}

      <h2 id="models">Models &amp; retention</h2>
      <p>
        <span className="mono">GET /v1/models</span> is <strong>public</strong> — no key, no
        account. It is the same document the <Link to="/models">catalog page</Link> renders, so
        what a prospective customer reads and what a client resolves are the same answer.
      </p>

      <CodeBlock label="One row of GET /v1/models">{MODELS_ROW}</CodeBlock>

      <p>
        Prices are decimal <em>strings</em>, in USD <strong>per single token</strong> — the
        OpenRouter convention, so a client that already normalizes that shape needs no special
        case. The metadata fields are omitted when unknown rather than defaulted: an absent{' '}
        <span className="mono">context_length</span> means the catalog does not publish one, never
        that it is small.
      </p>

      <h3>Retention postures</h3>
      <p>
        Every row carries a <span className="mono">retention</span> block, and that one is{' '}
        <em>never</em> omitted — the router refuses to load a catalog with an unlabelled lane. A
        customer reading a row with no posture would have to guess, and the guess a
        zero-retention brand invites is the flattering one.
      </p>
      <ul>
        <li>
          <span className="mono">posture: &quot;zero&quot;</span> — the upstream writes nothing to
          durable storage, abuse-monitoring logs included. This is only ever pinned against a
          signed arrangement, an enforced account setting with published semantics, or a vendor’s
          published default, verified against the vendor’s own documentation on the date in{' '}
          <span className="mono">verified</span>.
        </li>
        <li>
          <span className="mono">posture: &quot;standard&quot;</span> — the upstream retains
          prompts and completions for some period, commonly for abuse monitoring. This is the
          honest posture for an ordinary API account, and it is never dressed up as anything else.
          A no-training promise is not a retention promise; the two are separate claims and only
          one of them is this field.
        </li>
        <li>
          <span className="mono">description</span> is the provider’s statement in full, and{' '}
          <span className="mono">verified</span> is the date a human last read the source page.
        </li>
      </ul>
      <p>
        Rows arrive <strong>zero-retention first</strong>, then alphabetically within each posture.
        That order is a statement rather than a convenience, and it is worth relying on: the first
        rows of the list are the lanes that keep the promise the product is named for.
      </p>

      <h3>The same model can appear on more than one lane</h3>
      <p>
        A lane is a <em>configuration</em> — a model reached over one specific provider account —
        not just a model. The same weights from the same vendor can be reachable two ways, and the
        two ways can differ in exactly the things you care about: which company holds the data,
        what that company does with it afterwards, and what it costs.{' '}
        <span className="mono">bedrock/</span> and <span className="mono">anthropic/</span> serve
        overlapping Claude models; <span className="mono">vertex/</span> and{' '}
        <span className="mono">google/</span> serve overlapping Gemini models. Which posture and
        which price each lane currently carries is on the{' '}
        <Link to="/models">models page</Link>, read live from the catalog — deliberately not
        restated here, because postures change when the underlying arrangement changes and a page
        that hardcoded one would go quietly stale.
      </p>
      <p>
        <strong>You pick the configuration, by id.</strong> ZeroRouter does not silently move a
        request from the lane you named to a cheaper twin. A zero-retention lane can cost more than
        the same model on a standard account; that difference is the provider’s own price, not a
        margin.
      </p>

      <h3>Conditional pricing</h3>
      <p>
        Some lanes reprice above a prompt-size threshold. Where that happens the row carries a{' '}
        <span className="mono">pricing.overrides</span> array — each band naming the{' '}
        <span className="mono">min_prompt_tokens</span> it starts at and the rates that apply from
        there. The bands are <em>absolute replacements</em>, not surcharges: past the threshold the
        whole request bills at that band, input and output alike. A lane that charges one price at
        every size has no <span className="mono">overrides</span> key at all. The published band is
        the band settlement actually charges; the <Link to="/models">models page</Link> shows the
        numbers.
      </p>

      {/* ── Streaming ───────────────────────────────────────────────────── */}

      <h2 id="streaming">Streaming</h2>
      <p>
        Send <span className="mono">&quot;stream&quot;: true</span> for server-sent events. Add{' '}
        <span className="mono">stream_options: {'{'} &quot;include_usage&quot;: true {'}'}</span>{' '}
        to get a final chunk carrying the token counts the request was actually billed on.
      </p>

      <CodeBlock label="Streaming request">{STREAM_REQUEST}</CodeBlock>
      <CodeBlock label="Response body">{STREAM_RESPONSE}</CodeBlock>

      <p>
        Chunks are <span className="mono">chat.completion.chunk</span> objects, one per{' '}
        <span className="mono">data:</span> line, terminated by the literal sentinel{' '}
        <span className="mono">data: [DONE]</span>. Every OpenAI-compatible streaming client
        already knows this frame.
      </p>
      <p>
        <strong>Failures on a stream arrive in-band, under a 200.</strong> The response head is
        committed before the upstream is dialled, so once the stream has opened there is no status
        code left to change. A request that fails after that point delivers an error frame carrying
        the same body a non-streaming error would have had, followed by{' '}
        <span className="mono">[DONE]</span>. So: parse <span className="mono">error</span> on every
        chunk, and never read <span className="mono">200 OK</span> as proof the completion
        succeeded.
      </p>
      <p>
        An error frame can arrive <em>after</em> partial content — an upstream that dies mid-answer
        produces exactly that, and you keep what you were sent. The one case where it cannot is a{' '}
        <a href="#errors">retention refusal</a>, which is decided before any model output is
        released; there, nothing was delivered and nothing was billed.
      </p>

      <CodeBlock label="An error frame">{STREAM_ERROR}</CodeBlock>

      {/* ── Keys ────────────────────────────────────────────────────────── */}

      <h2 id="keys">Keys</h2>
      <p>
        Create keys on the <Link to="/keys">Keys</Link> page. A key is a bearer credential — anyone
        holding it can spend the account’s balance — and the plaintext is displayed exactly once,
        because the server stores only its digest. Lose it and you mint another; there is no
        recovery path, by design.
      </p>
      <p>A key carries a name, and three limits you can leave unset:</p>
      <ul>
        <li>
          <strong>Name</strong> — the one required field. Name it for the machine or project that
          holds it: it is what a usage row is attributed to, and what you will be reading when you
          decide which key to revoke.
        </li>
        <li>
          <strong>Expiration</strong> — an absolute instant, or never. A lapsed key stops
          authenticating at that moment and answers like a revoked one.
        </li>
        <li>
          <strong>Credit limit</strong> — a dollar ceiling on what this key may spend. Blank is
          unlimited. This is your own budget, not an operator ceiling.
        </li>
        <li>
          <strong>Reset window</strong> — <span className="mono">daily</span>,{' '}
          <span className="mono">weekly</span>, <span className="mono">monthly</span>, or never.
          Windows are UTC calendar windows: daily rolls over at UTC midnight, weekly on Monday,
          monthly on the first. With no window the limit is a lifetime total for that key.
        </li>
      </ul>
      <p>
        Revoking a key takes effect within 30 seconds — an authenticated key is cached for at most
        that long, and admission re-checks the row under a lock regardless, so a revoked key cannot
        dispatch one more inference on the strength of a stale cache.
      </p>

      <h3>Telling “rotate this key” from “top up the account”</h3>
      <p>
        Two different 402s, on purpose, because the remedy differs and an agent acts on the code.
        Collapsing them into one would send half of every audience down the wrong path.
      </p>
      <ul>
        <li>
          <span className="mono">key_credit_limit_exceeded</span> — <em>this key’s</em> own budget
          is spent for the current window. The account balance is untouched, so buying credit
          changes nothing. Use another key, raise this one’s limit, or wait for the window.
        </li>
        <li>
          <span className="mono">insufficient_credits</span> — the <em>account</em> balance cannot
          cover the request. Every key on the account is equally blocked; only adding credit
          helps.
        </li>
        <li>
          <span className="mono">spend_cap_exceeded</span> — an operator ceiling on the key bound
          instead. Raising your own limit will not help.
        </li>
      </ul>

      {/* ── Errors ──────────────────────────────────────────────────────── */}

      <h2 id="errors">Errors worth knowing</h2>
      <p>
        Errors are OpenAI-shaped: an <span className="mono">error</span> object with{' '}
        <span className="mono">message</span>, <span className="mono">type</span>,{' '}
        <span className="mono">param</span>, and <span className="mono">code</span>. Branch on{' '}
        <span className="mono">code</span> — it is the stable half. <span className="mono">type</span>{' '}
        groups codes into families (<span className="mono">invalid_request_error</span>,{' '}
        <span className="mono">authentication_error</span>,{' '}
        <span className="mono">billing_error</span>, <span className="mono">rate_limit_error</span>,{' '}
        <span className="mono">server_error</span>), and{' '}
        <span className="mono">param</span> names the offending field when there is one.
      </p>

      <CodeBlock label="Error body">{ERROR_BODY}</CodeBlock>

      <div className="docs-table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>code</th>
              <th>status · type</th>
              <th>what it means</th>
            </tr>
          </thead>
          <tbody>
            {ERROR_ROWS.map((row) => (
              <tr key={row.code}>
                <td>
                  <span className="mono">{row.code}</span>
                </td>
                <td className="dim nowrap">{row.status}</td>
                <td>{row.meaning}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <h3>
        <span className="mono">retention_attestation_failed</span>
      </h3>
      <p>
        This one is worth understanding rather than just handling, because it is the router working
        exactly as intended.
      </p>
      <p>
        Some lanes are sold under a zero-data-retention guarantee, and on those lanes the guarantee
        is <em>checked on every response</em> rather than assumed at configuration time. When an
        upstream answers without confirming it, ZeroRouter <strong>refuses the request</strong> — a
        502, no content delivered, nothing billed, and the balance untouched. It is not retried,
        and the refusal is emphatically not an invitation to retry: a retry would deliver your
        prompt again to an upstream that has just declined to say it will not keep it.
      </p>
      <p>
        A generic “upstream unavailable” would have been the easy answer and the wrong one. If you
        chose a zero-retention lane deliberately, you are owed the real reason: this is ZeroRouter
        declining to serve you under a weaker guarantee than the one you bought, which is a
        materially different event from a provider being down. The cause is a setting on
        ZeroRouter’s own account with the provider, so it is ours to fix — other lanes are
        unaffected in the meantime.
      </p>

      {/* ── Billing ─────────────────────────────────────────────────────── */}

      <h2 id="billing">How you are charged</h2>
      <p>
        <strong>Reserve, then settle.</strong> Before a request is dispatched, ZeroRouter reserves
        the most it could possibly cost — the worst-case price band applied to a bound on the
        prompt and the requested output. That reservation is what the balance and your key’s
        ceilings are checked against, under a per-account lock, so concurrent requests across your
        keys see each other and cannot jointly overdraw. When the upstream reports what it actually
        used, the request settles at the metered figure and the rest of the reservation is
        released, in the same transaction, exactly once.
      </p>
      <p>
        The consequences are the ones a prepaid product should have. <strong>There is no
        surprise bill</strong>: you cannot spend money you have not already deposited, and a
        request that would exceed your balance or a cap is refused up front rather than served and
        invoiced. Billing is on <strong>metered actuals only</strong> — if the upstream did not
        report usage, the request is not billed, because a guess about what you owe is not a
        number anyone should be charged. And a request that failed before delivering model output
        costs nothing at all.
      </p>
      <p>
        Prices are the provider’s own list rates with no markup on inference. The only fee
        ZeroRouter charges is the deposit fee when you add credits, and the exact amount is shown
        before you confirm. See the <Link to="/terms">Terms</Link> for the full statement.
      </p>

      {/* ── CLI ─────────────────────────────────────────────────────────── */}

      <h2 id="cli">Command line</h2>
      <p>
        <span className="mono">zerorouter user</span> logs a machine in with the RFC 8628 device
        flow — the same flow a TV app uses: the CLI prints a URL and a short code, you approve it
        in this portal, and the CLI receives an inference key. Handy for a laptop, a CI runner, or
        an agent that should not be handed a long-lived key by hand.
      </p>

      <CodeBlock label="zerorouter user">{CLI}</CodeBlock>

      <p>
        It is built to be driven by an agent as readily as by a person: every command takes{' '}
        <span className="mono">--json</span>, nothing prompts for input, and failure modes are
        distinguished by exit code (<span className="mono">3</span> not logged in,{' '}
        <span className="mono">4</span> the router failed or was unreachable,{' '}
        <span className="mono">5</span> the device grant was denied or expired). With{' '}
        <span className="mono">--json</span>, stdout carries exactly one JSON object and everything
        else goes to stderr, so piping into <span className="mono">jq</span> is safe. The
        credential lands in <span className="mono">~/.config/zerorouter/credentials</span> at mode{' '}
        <span className="mono">0600</span>.
      </p>
      <p>
        The device credential authorizes <span className="mono">POST /v1/chat/completions</span>{' '}
        and nothing else — it is an inference key, not a portal session, so it cannot mint keys,
        read your ledger, or touch billing. That is why the CLI has{' '}
        <span className="mono">models</span> but no <span className="mono">keys</span> subcommand.
      </p>
      <p className="page-note">
        The CLI ships as a subcommand of the router binary rather than as a separate download, and
        is built from source: ZeroRouter is AGPL-licensed and the whole thing — router, portal, and
        CLI — is public, which is what makes the zero-retention claim something you can check
        rather than something you have to take our word for.
      </p>

      <p className="page-note">
        Something here wrong, stale, or missing? Write to{' '}
        <span className="mono">support@zerorouter.ai</span>. A documentation page that quietly
        disagrees with the running service is worse than no page at all, so corrections are
        genuinely welcome.
      </p>
    </article>
  )
}

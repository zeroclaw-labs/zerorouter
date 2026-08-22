// Zero-dependency mock of an OpenAI-compatible upstream, so the e2e suite can
// drive a REAL completion through the REAL router process.
//
// The router reaches this through its own production test seam,
// `ZEROROUTER_PROVIDER_BASE_URL_<PROVIDER>` (`router/src/providers.rs`), which
// replaces one upstream's endpoint with a caller-supplied URL and logs a
// warning when it does. Nothing about the request path is faked: the browser
// presents a real `zcr_` key, the router authenticates it, reserves against the
// customer's balance, dispatches over its `chat_completions` wire, and settles
// on the usage this server reports. Only the model on the far end is a stand-in.
//
// This is what makes the playground's round-trip testable at all. The `testing`
// feature's `FakeModelProvider` is a Rust-level injection into `RouterState` and
// is deliberately absent from a production binary — which is the binary this
// suite runs — so the fake has to sit on the wire instead.
import http from 'node:http'

const PORT = Number(process.env.MOCK_UPSTREAM_PORT ?? 9420)

// Deterministic, and shaped so a test can assert on it without matching model
// prose. Split into several deltas so the browser genuinely exercises its SSE
// reassembly rather than receiving one whole frame.
const REPLY = ['Zero ', 'retention ', 'acknowledged.']
const PROMPT_TOKENS = 41
const COMPLETION_TOKENS = 7

function chunk(id, created, model, choices, usage) {
  const body = { id, object: 'chat.completion.chunk', created, model, choices }
  if (usage !== undefined) body.usage = usage
  return `data: ${JSON.stringify(body)}\n\n`
}

const server = http.createServer((req, res) => {
  if (req.method !== 'POST') {
    res.writeHead(405).end()
    return
  }
  let raw = ''
  req.on('data', (piece) => {
    raw += piece
  })
  req.on('end', () => {
    let request
    try {
      request = JSON.parse(raw)
    } catch {
      res.writeHead(400, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: { message: 'mock upstream: body was not JSON' } }))
      return
    }
    const id = `chatcmpl-mock-${Date.now()}`
    const created = Math.floor(Date.now() / 1000)
    const model = request.model ?? 'mock-model'
    const usage = {
      prompt_tokens: PROMPT_TOKENS,
      completion_tokens: COMPLETION_TOKENS,
      total_tokens: PROMPT_TOKENS + COMPLETION_TOKENS,
    }

    if (request.stream !== true) {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(
        JSON.stringify({
          id,
          object: 'chat.completion',
          created,
          model,
          choices: [
            {
              index: 0,
              message: { role: 'assistant', content: REPLY.join('') },
              finish_reason: 'stop',
            },
          ],
          usage,
        }),
      )
      return
    }

    res.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    })
    res.write(chunk(id, created, model, [{ index: 0, delta: { role: 'assistant' } }]))
    for (const piece of REPLY) {
      res.write(chunk(id, created, model, [{ index: 0, delta: { content: piece } }]))
    }
    // The terminal choice first, then the usage-bearing chunk, then `[DONE]` —
    // the order `ChatCompletionsStreamMachine` reads, and the reason it flushes
    // only at `[DONE]` rather than at the finish reason.
    res.write(chunk(id, created, model, [{ index: 0, delta: {}, finish_reason: 'stop' }]))
    res.write(chunk(id, created, model, [], usage))
    res.write('data: [DONE]\n\n')
    res.end()
  })
})

server.listen(PORT, '127.0.0.1', () => {
  // The readiness probe global-setup waits on. A GET is answered 405 by the
  // handler above, which is a response — enough to prove the socket is live —
  // so setup waits on the listen event through this line instead.
  process.stdout.write(`mock upstream listening on ${PORT}\n`)
})

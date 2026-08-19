# Getting started: hosted ZeroRouter

From nothing to a first API call against the hosted service at
[zerorouter.ai](https://zerorouter.ai). To run the router yourself instead,
see the [README quickstart](../README.md#quickstart) (from source) or
[`edge-quickstart.md`](edge-quickstart.md) (models on your own hardware).

## 1. Sign in

Go to [zerorouter.ai](https://zerorouter.ai) and click **Sign in with SSO**.
An account is created on first sign-in; there is no separate registration.

## 2. Add credits

Access is prepaid: a request is admitted only when the credit balance covers
it, so a fresh account cannot make calls yet.

On the **Credits** page, pick an amount ($5.00 minimum) — checkout happens on
Stripe. **Deposits carry a processing fee, charged on top of the credit:
5.5% of the amount, with a minimum of $0.80.** The page quotes that split —
you pay $X, you receive $Y of credit — before you commit; the quote is priced
by the server with the same arithmetic the charge uses. **Sales tax, where it
applies, is added on top of that figure**, so the card is charged more than
the quote shows; Stripe determines it from your billing address and displays
it on the checkout page before you confirm. Tax is never credited — you
receive the same $Y either way.

The same page can optionally arm autopay: a saved card tops the balance up
whenever it falls below a threshold you set. Three failed charges in a row
turn autopay off.

## 3. Create an API key

On the **Keys** page, name the key (the machine or project it belongs to)
and click **Create key**. The full key — `zcr_…` — is shown exactly once, in
the dialog that follows; the server stores only a hash. Store it before
closing. A lost key cannot be recovered, only revoked and replaced.

## 4. Make a request

The API is OpenAI-compatible at base URL `https://zerorouter.ai/v1`; the key
is the bearer token.

Model ids are OpenRouter-standard `{vendor}/{model}`. Pick one from the
[models page](https://zerorouter.ai/models) — public, with per-million-token
prices, context windows, and capabilities — or from `GET /v1/models` (no
auth required). Unknown ids are hard errors, not fuzzy matches.

curl:

```sh
curl https://zerorouter.ai/v1/chat/completions \
  -H 'Authorization: Bearer zcr_YOUR_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"model":"anthropic/claude-sonnet-5","messages":[{"role":"user","content":"Hello"}]}'
```

Python (`openai` SDK — only the base URL and key change):

```python
from openai import OpenAI

client = OpenAI(base_url="https://zerorouter.ai/v1", api_key="zcr_YOUR_KEY")

response = client.chat.completions.create(
    model="anthropic/claude-sonnet-5",
    messages=[{"role": "user", "content": "Hello"}],
)
print(response.choices[0].message.content)
```

Streaming is the standard OpenAI shape: `"stream": true` (curl) or
`stream=True` (SDK), SSE chunks back.

## Local models

A self-hosted ZeroRouter can route to models on your own hardware — llama.cpp,
Ollama, vLLM — and burst to the hosted service when they cannot take the
request: [`edge-quickstart.md`](edge-quickstart.md).

## Terms and privacy

[Terms of service](https://zerorouter.ai/terms) ·
[Privacy policy](https://zerorouter.ai/privacy)

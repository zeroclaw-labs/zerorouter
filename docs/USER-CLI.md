# `zerorouter user` — the end-user CLI

`zerorouter user` logs a machine in to a ZeroRouter deployment with the RFC 8628
device-authorization flow and stores the resulting credential. It is a
subcommand of the same binary that serves the router, alongside
`zerorouter admin`.

It is built to be driven by an AI agent as readily as by a person: every command
takes `--json`, nothing prompts for input, and failures are distinguished by
exit code.

```
zerorouter user login    # device-flow login; stores the credential
zerorouter user whoami   # which router this machine is logged in to
zerorouter user models   # what the router serves, with rates
zerorouter user logout   # remove the credential
```

## What this CLI can and cannot do

**Read this before wondering where `keys` went.**

`POST /auth/device/token` mints a **`zcr_` inference API key** and returns it as
`access_token` with `scope: "inference"`. It does not mint a portal session.

Every `/api/*` endpoint — `/api/me`, `/api/keys`, `/api/usage`,
`/api/billing/ledger`, and the Stripe billing routes — is guarded by the
`PortalUser` extractor, which resolves **only** the `zr_session` cookie (a
`zcs_` token stored as a SHA-256 digest in `portal_sessions`) and, on mutating
methods, additionally requires the `x-zerorouter-portal` CSRF header. There is
no bearer path into `/api/*`.

So the device-flow credential authorizes exactly one endpoint:

| endpoint                 | device credential | note                             |
|--------------------------|-------------------|----------------------------------|
| `POST /v1/chat/completions` | ✅ yes         | the only bearer-authenticated route |
| `GET /v1/models`         | ✅ (not needed)   | unauthenticated on the server    |
| `GET /healthz`           | ✅ (not needed)   | unauthenticated                  |
| `GET /api/me`            | ❌ no             | `PortalUser` — session cookie only |
| `GET/POST /api/keys`     | ❌ no             | `PortalUser`                     |
| `DELETE /api/keys/{id}`  | ❌ no             | `PortalUser` + CSRF header       |
| `GET /api/usage`         | ❌ no             | `PortalUser`                     |
| `GET /api/billing/ledger`| ❌ no             | `PortalUser`                     |

`keys list/create/revoke`, `balance`, `ledger`, and `usage` are therefore **not
implemented**. They are not stubbed either — a subcommand that always fails is
worse than an absent one for an agent reading `--help`. Shipping them needs a
server-side credential that carries portal scope; see
[Extending the scope](#extending-the-scope).

### CSRF

Not applicable to this CLI as it stands. The CSRF header is enforced by
`PortalUser`, and nothing this CLI can reach uses `PortalUser`. The one bearer
route, `/v1/chat/completions`, has no CSRF check — correctly, since a bearer
token is not ambient authority the way a cookie is. Any future portal-scoped CLI
credential delivered as a cookie would need to send `x-zerorouter-portal` on
mutating calls; delivered as a bearer header it would not.

## Commands

### `login`

```
zerorouter user login [--base-url URL] [--client-id ID] [--key-name NAME]
                      [--no-browser | --open] [--json]
```

Starts a device authorization, prints the verification URL and user code to
**stderr**, then polls until the grant is approved, denied, or expires (the
server's TTL is 15 minutes). On success the credential is written to
`~/.config/zerorouter/credentials` with mode `0600`.

The plaintext key is printed **exactly once**, on success. The server stores
only its SHA-256 digest, so this is the sole opportunity to capture it —
`whoami` deliberately prints a fingerprint instead.

A browser is **never** opened unless `--open` is passed. `--no-browser` states
that default explicitly for scripts.

`--client-id` defaults to `zeroclaw`, because that is the default value of the
router's `ZEROROUTER_DEVICE_CLIENT_IDS` allowlist — so login works against an
unmodified deployment. If an operator narrows that list, pass a matching id;
a mismatch is reported as such rather than as a generic 400.

### `whoami`

Reports the stored credential's router, scope, key name, fingerprint, and login
time. Purely local — it makes no request, because no endpoint exists that a
device credential could use to identify itself. Exits `3` when not logged in.

**It never prints the key**, in either output mode.

### `models`

`GET /v1/models`, which is unauthenticated, so this works before `login`. The
`--json` output is the server's document passed through unchanged (OpenAI /
OpenRouter shaped) rather than reshaped, so there is only one contract to keep
in step. The human table converts the wire's per-single-token rates to
per-million and flags models that reprice above a prompt-size threshold.

Each row also carries its **retention posture** — `zero` when a zero-data-retention
arrangement is in force with that upstream, `standard` when the provider retains
data under its own published policy. `--json` carries the full statement and the
date it was last verified; `docs/DEPLOY.md` covers how a posture is changed.

### `logout`

Removes the credential file. Idempotent: exits `0` whether or not one was
there, so cleanup scripts need not test first.

## Agent usage

### Output contract

With `--json`, **stdout carries exactly one JSON object per invocation** and
nothing else. Progress messages, the login prompt, and errors all go to stderr.
So this is safe:

```bash
zerorouter user login --json > credential.json    # prompt still visible on the terminal
API_KEY=$(zerorouter user login --json | jq -r .api_key)
```

On failure, stdout is **empty** and stderr's last line is a JSON error object:

```json
{"error": {"code": "not_authenticated", "message": "not logged in: no credential at /home/a/.config/zerorouter/credentials. Run `zerorouter user login`."}}
```

### Exit codes

| code | meaning                        | JSON `error.code`             |
|------|--------------------------------|-------------------------------|
| 0    | success                        | —                             |
| 1    | local/runtime error (I/O, bad `--base-url`) | `runtime_error`  |
| 2    | usage error (unknown flag or subcommand) | — (clap)            |
| 3    | not logged in                  | `not_authenticated`           |
| 4    | the router failed or was unreachable | `http_error`            |
| 5    | device grant denied, expired, or timed out | `device_authorization_failed` |

`2` is clap's own usage code; the CLI's codes deliberately avoid it so an agent
can tell "I called this wrong" from "it ran and failed".

### Non-interactive login

`login` never reads stdin. An agent that cannot render a browser prints the
prompt for a human and waits:

```bash
zerorouter user login --json --no-browser 2>prompt.txt >credential.json &
# prompt.txt now holds the verification URL and user code; relay it, then wait
wait $!
```

### Environment

| variable                      | effect                                                     |
|-------------------------------|------------------------------------------------------------|
| `ZEROROUTER_BASE_URL`         | default router base URL (overridden by `--base-url`)       |
| `ZEROROUTER_CONFIG_DIR`       | overrides the whole config directory — use it to sandbox an agent |
| `ZEROROUTER_DEVICE_CLIENT_ID` | default device-flow client id                              |
| `XDG_CONFIG_HOME`             | honored when `ZEROROUTER_CONFIG_DIR` is unset              |

## Example output

`zerorouter user login --json`:

```json
{
  "status": "logged_in",
  "base_url": "https://zerorouter.ai",
  "api_key": "zcr_4f2a…",
  "token_type": "bearer",
  "scope": "inference",
  "key_name": "zerorouter cli",
  "key_fingerprint": "sha256:9c1d4b7e2a08f351",
  "credentials_path": "/home/a/.config/zerorouter/credentials",
  "created_at": "2026-08-20T09:14:02.481Z"
}
```

`zerorouter user whoami --json` — note the absence of `api_key`:

```json
{
  "status": "authenticated",
  "base_url": "https://zerorouter.ai",
  "scope": "inference",
  "token_type": "bearer",
  "key_name": "zerorouter cli",
  "key_fingerprint": "sha256:9c1d4b7e2a08f351",
  "credentials_path": "/home/a/.config/zerorouter/credentials",
  "created_at": "2026-08-20T09:14:02.481Z"
}
```

`zerorouter user logout --json`:

```json
{
  "status": "logged_out",
  "removed": true,
  "credentials_path": "/home/a/.config/zerorouter/credentials"
}
```

`zerorouter user models` (human):

```
MODEL                             IN $/MTOK    OUT $/MTOK     CONTEXT  RETENTION
anthropic/claude-fable-5              10.00         50.00     1000000  standard
anthropic/claude-haiku-4-5             1.00          5.00      200000  standard
anthropic/claude-opus-5                5.00         25.00     1000000  standard
anthropic/claude-sonnet-5              2.00         10.00     1000000  standard
google/gemini-3.1-pro-preview          2.00         12.00     1048576  standard
openai/gpt-5.6-luna                    0.20          1.20     1050000  standard

4 model(s) reprice above a prompt-size threshold; the rates above are the base band. Use --json for the full schedule.

Retention: 18 lane(s) zero-retention, listed first; 14 where the provider retains data.
Use --json for each lane's full retention statement and the date it was verified.
```

Rows arrive in the server's order, which lists **zero-retention lanes first**,
then alphabetically within each posture. The counts in the footnote track the
live catalog, so treat the numbers in this example as a snapshot rather than a
promise — the split moves as lanes are added and as postures are re-verified.

## Credential file

`~/.config/zerorouter/credentials` (or `$ZEROROUTER_CONFIG_DIR/credentials`),
mode `0600`, inside a directory forced to `0700`. Written to a temporary file
and renamed, so an interrupted write cannot leave a partial credential in place.

```json
{
  "version": 1,
  "base_url": "https://zerorouter.ai",
  "api_key": "zcr_…",
  "token_type": "bearer",
  "scope": "inference",
  "key_name": "zerorouter cli",
  "created_at": "2026-08-20T09:14:02.481Z"
}
```

It holds a plaintext API key that can spend the account's balance. Keyring
integration is out of scope; the file mode is the whole protection, and it is
covered by a test.

## Extending the scope

To give the CLI `keys`, `balance`, `ledger`, and `usage`, the server must issue
the CLI something a portal endpoint accepts. Three shapes, in ascending order of
how much they should be trusted:

1. **Teach `PortalUser` to accept `Authorization: Bearer zcr_…`.** Smallest
   diff, worst idea: it promotes *every* inference key — keys handed to agents
   and CI runners — to full portal authority, including Stripe checkout and
   autopay. A leaked key would go from "can spend the balance" to "can mint
   keys, read the ledger, and change payment settings".
2. **Have the device claim also mint a `portal_sessions` row** and return it
   alongside the API key. Needs no migration and is confined to `device.rs`.
   But it silently widens what approving a device grant hands over, and the
   consent screen that says "this will create an API key" lives in the portal
   SPA — so the copy and the behavior must change together.
3. **A distinct CLI credential with its own scope** — its own table (or a
   `scope` column on `portal_sessions`), an allowlist of the endpoints it may
   reach, and consent copy that names them. More work, and the only option that
   lets a user see what they are approving.

(3) is the recommended shape. Whichever is chosen, it is a decision about what a
device approval grants, not a mechanical change.

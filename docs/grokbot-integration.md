# Grok Bot integration

`/bot` routes a pager session to a Grok Bot. It now has two providers behind one
trait (`BotHub` in `crates/codegen/xai-grok-pager/src/bot/hub.rs`):

| Provider | Selected by | Transport | Credential |
| --- | --- | --- | --- |
| `hub` (default) | nothing, or `provider = "hub"` | Computer hub WebSocket, `bot_*` tools | The grok.com login this CLI already owns (`/login`) |
| `gateway` | `GROK_BOT_PROVIDER=gateway` or `[bot] provider = "gateway"` | Grok Bot HTTP gateway, `POST {url}/api/<method>` | `GROK_BOT_GATEWAY_URL` + `GROK_BOT_GATEWAY_TOKEN`, supplied by the user |

The `hub` provider is unchanged. This document covers the `gateway` provider.

## Configure

```toml
# config.toml, the same file that holds [cli] backend
[bot]
provider = "gateway"
```

```sh
export GROK_BOT_GATEWAY_URL=https://<your-gateway-host>
export GROK_BOT_GATEWAY_TOKEN=<token>
```

`GROK_BOT_PROVIDER` beats `[bot] provider`. An unknown value falls through to the
next source, and the default is `hub`. The env var names match grok-bot-cli, so a
shell already set up for `gbot` works unchanged.

`GROK_BOT_ALLOW_LOCAL_GATEWAY=1` permits `http(s)://localhost`, `127.0.0.1`, and
`::1`, for a local gateway during development.

## Sources

Everything here comes from public, documented material.

- grok-bot-cli README, MIT licensed, `https://github.com/ScriptedAlchemy/grok-bot-cli`
  at commit `43499fa27f36807f6ac93b56365b3cae5200bc1e` (2026-10-05). The "Auth",
  "Gateway URL policy", and "Errors" sections define the env vars, host policy,
  and error shape used here.
- grok-bot-cli source at the same commit, read for wire shapes the README does
  not spell out: `src/core/gateway.js` (methods and bodies), `src/core/headers.js`
  (bearer header), `src/core/url-policy.js` (host checks), `src/core/transcript.js`
  and `src/core/relay/intake.js` (how a reply is tied to its prompt).
- npm registry metadata for `grok-bot-cli` 0.12.4 (license MIT, repository above).
- This repo's hub bot tool list in `crates/common/xai-computer-hub-core/src/bot_tools.rs`
  for the `hub` provider.

The README does not document the gateway's HTTP routes. The method names and bodies
below are read from the MIT source, so they are the CLI's working contract, not a
published API. A gateway change can break them.

## Credentials

The provider sends a credential only when the user hands it one.

grok-bot-cli resolves auth in this order: `GROK_BOT_GATEWAY_URL` +
`GROK_BOT_GATEWAY_TOKEN`, then the Grok Bot desktop app's stored session, then
`CURSOR_ACCESS_TOKEN` exchanged through `aiserver.v1.GrokBotService/EnsureSandBox`.
This provider implements only the first.

- **Desktop app session.** Not used. gbot reads it from `~/Library/Application Support/Grok Bot`,
  the OS keyring, or a DPAPI-wrapped key. Reading another app's encrypted session
  is credential extraction.
- **`CURSOR_ACCESS_TOKEN` and `EnsureSandBox`.** Not used. The token is Cursor's
  session token, not a key Cursor issues for API use (gbot's own error says a
  dashboard API key does not work), and `EnsureSandBox` is an internal Cursor RPC
  with no public documentation.
- **`GROK_BOT_GATEWAY_HEADERS`** and **`GROK_BOT_ALLOW_ANY_GATEWAY`.** Not
  supported. The first carries extra session headers. The second turns off the
  host check, which gbot itself labels unsafe.

**The live-call gap.** No live gateway call was made for this change. It needs
`GROK_BOT_GATEWAY_URL` and `GROK_BOT_GATEWAY_TOKEN`. Both are named in the
grok-bot-cli README "Auth" section. The README does not say how to obtain a
gateway token outside the desktop app or `EnsureSandBox`, so there is no
documented way to mint one. A user who has a gateway URL and token from Grok Bot
can set them and run `/bot`.

### Host policy

Ported from grok-bot-cli `url-policy.js`. The token is sent only to:

- `https` on `cursor.sh`, `cursor.com`, `*.cursor.sh`, `*.cursor.com`, or `*.cursorvm.com`.
- `http(s)` loopback, when `GROK_BOT_ALLOW_LOCAL_GATEWAY=1`.

A URL with userinfo is rejected. The HTTP client follows no redirects, so the
bearer token cannot be forwarded to another host. Error text replaces the token
with `[redacted]`. `GatewayCredentials`'s `Debug` omits it.

## Wire protocol

Every call is `POST {GROK_BOT_GATEWAY_URL}/api/<method>` with
`Authorization: Bearer <token>`, `Content-Type: application/json`, and a 30 s timeout.

| `/bot` operation | Gateway method | Request body | Response used |
| --- | --- | --- | --- |
| list roster | `listAgents` | `{}` | `agents[]` (or a bare array, or `agent`): `id`/`agentId`, `name`, `description`, `isGroup` |
| resolve by name | `listAgents` | `{}` | Exact name first, then name or id ignoring case, as grok-bot-cli resolves. The gateway has no search method. |
| `/bot new <name>` | `createAgent` | `{name, description:"", title:"", avatarShape:"", avatarColor:"", origin:"user"}` | `agent.id`, `agent.name` |
| send prompt | `sendPrompt` | `{agentId, prompt, clientNonce}` | Any 2xx, whatever the body. `clientNonce` is a fresh UUID. |
| wait for reply | `getAgentTranscriptTail` | `{id, limit: 200}` | `entries[]` |

### Tying a reply to its prompt

The prompt's transcript entry carries our `clientNonce` and a `requestId`. Each bot
reply is an entry with `kind: "send-message"`, the same `requestId`, and its text
in `message.content`. grok-bot-cli's relay correlates the same way
(`relay/intake.js`).

A pending approval card (`message.type` of `auto-review-approval` or
`local-tool-permission`) has no text. It counts as a reply that reads "Waiting
for your approval. Answer it in Grok Bot.", so a turn that stops on one still
finishes.

The wait polls the tail every second. It returns once the turn has at least one
reply and the replies have not changed for 3 s. Several replies join with a
blank line. Polling stops after 60 s of waiting. A single poll can take up to
the 30 s request timeout, so the worst case is about 90 s. Then the route keeps
the nonce and prints "`<name>` is still working. The next message waits for that
turn.". The next line resumes that wait instead of sending. A 5xx or transport
failure on a poll ends the wait the same way, because the prompt has already
landed. A 4xx on a poll is shown as an error.

The gateway exposes no "turn finished" signal that grok-bot-cli reads. The
3 s settle rule is a heuristic. A bot that replies, then works quietly for more
than 3 s and replies again, ends the turn at its first reply. The later reply is
not shown.

### Busy bots and `--replace`

The hub has `on_busy: reject | supersede`. The gateway's `sendPrompt` takes no
such field, so nothing is superseded. `--replace` still skips a pending wait and
sends the new prompt. Grok Bot decides what to do with a prompt sent to a busy
bot.

### Delivery certainty

Ported from grok-bot-cli `sendPrompt`. Only a 4xx means the gateway refused the
prompt. A 3xx, a 5xx, or a transport failure means it may have landed, so the
message gains `(delivery unknown; check the thread before resending)`. A 2xx
whose body is not JSON counts as accepted, as in grok-bot-cli. `/bot` never
resends on its own.

## Errors

### From Grok Bot, live

None recorded. No live call was made (see [Credentials](#credentials)).

### Shown by the gateway provider

The provider shows each failure verbatim in scrollback. HTTP failures use
grok-bot-cli's shape, `<method> failed: <status> <detail>`. `<detail>` is the
body's `message`, else its `error`, else the raw body, else the status reason,
cut to 300 characters with the token redacted. A JSON body with neither field
shows the status reason. Each line below is asserted in
`crates/codegen/xai-grok-pager/src/bot/gateway_tests.rs`, against the mock
gateway or `GatewayCredentials::parse`.

```text
Set GROK_BOT_GATEWAY_URL and GROK_BOT_GATEWAY_TOKEN to talk to Grok Bot over its gateway.
Rejected gateway URL: only https is allowed (got http:).
Rejected gateway URL host "cursor.sh.example.com". Expected *.cursor.sh / *.cursor.com / *.cursorvm.com.
Rejected gateway URL host "localhost". Set GROK_BOT_ALLOW_LOCAL_GATEWAY=1 to permit localhost/127.0.0.1 gateways.
Rejected gateway URL: userinfo is not allowed.
listAgents failed: 401 invalid gateway token [redacted]
sendPrompt failed: 400 prompt is empty
sendPrompt failed: 503 box is restarting (delivery unknown; check the thread before resending)
Ada is still working. The next message waits for that turn.
Waiting for your approval. Answer it in Grok Bot.
No bot matches "rust". The route is unchanged.
```

The `401`, `400`, and `503` bodies are mock fixtures, not text Grok Bot is known
to send.

Other messages the provider can show, not asserted by a test:

```text
Invalid gateway URL.
<method> failed: <reqwest transport error>
createAgent returned no agent id
No Grok Bot prompt is waiting for a reply.
gateway client: <reqwest builder error>
```

### grok-bot-cli's own messages, for reference

Verbatim from `src/core/gateway.js` at the commit above. Seeing these from `gbot`
explains what the gateway provider will hit with the same credentials.

```text
Set CURSOR_ACCESS_TOKEN, or GROK_BOT_GATEWAY_URL + GROK_BOT_GATEWAY_TOKEN. Do not use a Cursor dashboard API key.
EnsureSandBox returned no gatewayUrl/gatewayToken. Auth may be a dashboard API key (those do not work).
Gateway response too large (over 2097152 bytes)
No bot or group named "<ref>"
Ambiguous name "<ref>"
```

## Code

| File | Owns |
| --- | --- |
| `crates/codegen/xai-grok-pager/src/bot/gateway.rs` | `GatewayCredentials` (env parse and host policy) and `GatewayBotHub` (the `BotHub` impl) |
| `crates/codegen/xai-grok-pager/src/bot/provider.rs` | `BotProvider`, `resolve_provider`, and `run`, which connects the chosen provider for one `/bot` submit |
| `crates/codegen/xai-grok-pager/src/bot/gateway_tests.rs` | `/bot` end to end against a wiremock gateway that answers only the right bearer token |

`BotSession` (`session.rs`) is shared, so list, route, `/bot new`, `/bot off`,
and the resume-after-timeout flow behave the same on both providers.

## Open items

- **Gateway token.** Find a documented way to issue `GROK_BOT_GATEWAY_TOKEN`,
  then run `/bot` live and record the real error strings here.
- **Turn completion.** Replace the two-equal-polls heuristic if the gateway
  exposes a turn status.
- **Supersede.** Map `--replace` to a gateway interrupt if one is documented.
- **Groups.** Groups list with status `group` and can be routed to. Whether
  member replies carry the prompt's `requestId` is unverified, so a group turn
  may never finish. This path has no live check.
- **Turns with no reply.** A turn that ends without text or an approval card,
  or whose prompt row scrolls out of the 200-entry tail, never finishes. Each
  later line resumes that wait. `/bot off`, then `/bot <name>`, clears it.
- **Response size.** grok-bot-cli caps a gateway body at 2 MiB. This provider
  has no cap and trusts the allowlisted host.
- **Redaction.** Only the exact token string is redacted. grok-bot-cli's
  `redactSecrets` also scrubs other secret patterns.

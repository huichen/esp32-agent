# The model behind the agent

The board is the loop; the model is a setting. Everything here is `POST /api/agent/config`
(or the Agent panel in the console), stored in NVS.

| Field | Default | Meaning |
|---|---|---|
| `base_url` | empty | Full endpoint URL, `http://` or `https://`. Empty means nothing is configured and a run fails in milliseconds |
| `model` | empty | The provider's model id |
| `api_key` | empty | Sent as `Authorization: Bearer`; `__set__` on read means "a key is stored" |
| `protocol` | `chat` | `chat` (OpenAI chat completions), `responses` (OpenAI Responses API), `anthropic` (Messages API) |
| `max_steps` | `0` | Model calls per turn; `0` is no limit, capped at 200 |
| `temperature` | `1.0` | 0 to 2 |
| `reasoning` | empty | `off`/`low`/`medium`/`high`; empty sends nothing and leaves it to the provider |
| `vision` | `true` | Whether image attachments go out as content parts |
| `context_tokens` | `262144` | Transcript budget; compaction fires at 80% of it |
| `max_output_tokens` | `32768` | Reply ceiling, sent as `max_tokens` and reserved out of the window |
| `timeout_s` | `45` | Per model call |

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://example.invalid/v1/chat/completions","model":"m","api_key":"…"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"what is on the screen?"}'
```

## The three wire protocols differ where it hurts

A request built for one is a 400 on the others, so the choice is explicit:

| | `chat` | `responses` | `anthropic` |
|---|---|---|---|
| History | `messages[]` | `input[]`, items are typed | `messages[]` + `system` as a field |
| Tools | nested under `function` | flat | `input_schema` |
| Tool results | a `role: tool` message | a `function_call_output` item | a `tool_result` block in a user turn |
| Auth header | `Authorization: Bearer` | same | `x-api-key` + `anthropic-version` |
| `max_tokens` | optional | optional | required |

## When it fails

The error names the layer, because the three failures look identical from a distance:

- `connecting to … failed` — no route, wrong host, or the radio is not up.
- `sending the request over TLS failed` — usually the clock (SNTP has not synced, so a
  certificate looks not-yet-valid) or internal RAM for the handshake. The message carries
  `time.synced`, the clock and the two heap numbers for that reason.
- `reading the reply over TLS failed … (code -0x7200: MBEDTLS_ERR_SSL_INVALID_RECORD)` —
  the peer sent a TLS record larger than `CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN`. This board had
  4096 there (to save internal RAM), so every model reply longer than one 4 KB record failed
  while short ones passed; the buffer is 16384 now. mbedTLS only accepts a record up to that
  size and does not ask the other end to split — the length is the peer's choice.
- `reading the reply over TLS failed … (code -0x6800: MBEDTLS_ERR_SSL_TIMEOUT)` — a real
  timeout: nothing arrived within `timeout_s`. The message says how long the request had
  been open, which is the difference between that and a peer that reset the connection.

## Without a provider

Point `base_url` at the board's own `http://127.0.0.1/selftest/chat/completions` and the loop
still runs: it answers from the board's tool surface with no network and no key. That is what
`scripts/smoke.ps1` uses.

Related: [agent loop](agent-loop.md) · [HTTP API](http-api.md) · [troubleshooting](troubleshooting.md)

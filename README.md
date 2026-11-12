# esp32-agent

**English** · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

Firmware for an ESP32 that runs its own agent loop and serves an MCP endpoint, so one model
can drive the board directly and any external agent can use the same tools over HTTP.

Flash it, join WiFi, and it probes the buses it has drivers for. Each device that answers
becomes MCP tools. The agent on the board sees those same tools and answers with them; a new
sensor can be declared in JSON while the board runs.

> The agent loop follows [FutureOS](https://github.com/futuregene/future-os): transcript,
> tool journal, stream events, protocol adapters, memory index. 

| What | In detail |
|---|---|
| A model runs on the board | Loop, tools, memory and transcript are firmware; the model is any OpenAI-compatible or Anthropic endpoint. [Model](docs/model.md) |
| The tool set is a document | `manifest_apply` validates a JSON manifest, compiles it into tools, live in the same turn. [Manifest](docs/manifest.md) |
| It finds its own hardware | I2C, 1-Wire and declared pins are probed at boot; only what answers gets tools. [Capabilities](docs/capabilities.md) |
| One tool set, two callers | The MCP endpoint and the on-board agent see the same tools, from one description. [HTTP and MCP API](docs/http-api.md) |
| A console over HTTPS | Chat with the model and configure devices, tools, schedule, logs, WiFi and the model, from a phone; the board issues its own certificate. [Web console](docs/web-console.md) |
| It acts unattended | Timed prompts, and sensor rules that fire a prompt when a reading crosses a threshold. [Scheduling](docs/scheduling.md) |
| A shell and a script language | `shell` with 55 NuttX-named commands, and an interpreter for loops over tool calls. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| It remembers what it was told | Standing orders, `/AGENTS.md`, a `/MEMORY.md` index and skills, assembled into the prompt on every turn. [Context layers](docs/context.md) |
| No cloud in the path | HTTPS from the board, a certificate it generates, one PIN for three callers. [Security](docs/security.md) |
| Testable without hardware | The simulator runs the same `agent-core` against a virtual board. [Simulator](docs/simulator.md) |

## Flash and connect

Write the merged image from a [release](https://github.com/huichen/esp32-agent/releases) at offset 0 (or build it, see
[docs/flashing.md](docs/flashing.md)):

```bash
espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin
```

With no credentials stored the board starts an access point called `esp32-agent-setup`. Join
it from a phone, open **http://192.168.4.1/** and enter your WiFi name and password. They are
kept in NVS, never in the image, and they are what the board needs to reach anything beyond
the LAN — the model, a package mirror, `/agent.json` on someone else's desk.

## Use it

Two things it is for.

**A person with a phone.** Open `http://<board>/` (or the same over https on 443): chat with
the model, look at the devices the probe found, switch a tool off, read the log, set the PIN.

**An agent that was handed `http://<board>/agent.json`.** That document is the interface as
data — every route with method, body and auth, the MCP surface, the streaming endpoints, the
settings table with types and defaults, and the limits. An agent that can fetch one URL can
use this board without reading a line of this README; that is what the endpoint is for.

## Give it a model

Two ways, same three fields.

**In the console.** Open the page, Agent panel, fill in endpoint, model and key, save. The key
is write-only: it goes to NVS and reads back as `__set__`.

**By an agent.** `POST /api/agent/config` with the same fields:

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
```

Three wire protocols (`chat`, `responses`, `anthropic`), a thinking level, a context window and
an output ceiling are on the same panel and the same route. With no key at all, point
`base_url` at `http://127.0.0.1/selftest/chat/completions` and the loop still runs, so a board
can be tested before an account exists. [Model](docs/model.md) has the field list.

## Safety notes

- **Credentials stay on the board.** WiFi goes to NVS through `provision`, never into the
  image; `api_key` is write-only and reads back as `__set__`.
- **Set a PIN before the board leaves your desk.** `auth.portal_pin` guards every write and the
  reads that expose the transcript, the prompt, the config and the logs.
- **Compare the certificate.** The board's TLS certificate is self-signed; its fingerprint is at
  `GET /api/tls`. Port 80 stays open by default so a bad certificate costs a URL and not the
  board; `mcp.plain_http: false` closes it, and the USB cable is then the way back in.
- **`shell`, `script` and manifest tools move pins.** That is the point of the board; the PIN is
  what separates the agent doing its job from a passer-by doing it.

[Security](docs/security.md) states what is deliberately not defended.

## Status

| Part | State |
|---|---|
| `agent-core` | 427 unit tests and 14 integration tests pass; `scripts/smoke.ps1` passes end to end |
| Simulator | console, MCP and the agent loop run end to end |
| Firmware | flashed on an ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 tools, OLED 128×32, I²S mic and amp, WS2812, TLS console, model turns over HTTPS |
| Not implemented | OTA, SD card, DHT11/22, camera. See `ROADMAP.md` |

Bench numbers: a four-tool turn in 13.7 s; console over TLS in 0.55 s; I²C scan stable over
8 runs; internal SRAM 5–6 kB free after a TLS page, the tightest constraint here.

## Documentation

[Architecture](docs/architecture.md) · [Manifest](docs/manifest.md) · [Capabilities](docs/capabilities.md) ·
[HTTP and MCP](docs/http-api.md) · [Adding a device](docs/adding-a-device.md) · [Flashing](docs/flashing.md) ·
[Troubleshooting](docs/troubleshooting.md) · [Agent loop](docs/agent-loop.md) · [Stream](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Memory](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Performance](docs/performance.md)

## Contributing

A new peripheral starts as a manifest fragment under `examples/manifests/`; a Rust driver is
needed only for timing, calibration or state. `CATALOG` is where a driver registers itself.
Conventions are in [AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).

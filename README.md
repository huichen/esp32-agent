# esp32-agent

**English** · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

Firmware for an ESP32 that runs its own agent loop and serves an MCP endpoint, so one model
can drive the board directly and any external agent can use the same tools over HTTP.

Flash it, join WiFi, and it probes the buses it has drivers for. Each device that answers
becomes MCP tools. The agent on the board sees those same tools and answers with them; a new
sensor can be declared in JSON while the board runs.

> The agent loop follows [FutureOS](https://github.com/futuregene/future-os): transcript,
> tool journal, stream events, protocol adapters, memory index. Where the firmware differs
> (kilobytes instead of gigabytes of RAM, no model-written summaries) the code says why.

| What | In detail |
|---|---|
| A model runs on the board | Loop, tools, memory and transcript are firmware; the model is any OpenAI-compatible or Anthropic endpoint. [Model](docs/model.md) |
| The tool set is a document | `manifest_apply` validates a JSON manifest, compiles it into tools, live in the same turn. [Manifest](docs/manifest.md) |
| It finds its own hardware | I2C, 1-Wire and declared pins are probed at boot; only what answers gets tools. [Capabilities](docs/capabilities.md) |
| One tool set, two callers | The MCP endpoint and the on-board agent see the same tools, from one description. [HTTP and MCP API](docs/http-api.md) |
| A console over HTTPS | Chat with the model and configure devices, tools, schedule, logs, WiFi and the model, from a phone; the board issues its own certificate. [Web console](docs/web-console.md) |
| It acts unattended | Timed prompts, and sensor rules that fire a prompt when a reading crosses a threshold. [Scheduling](docs/scheduling.md) |
| A shell and a script language | `shell` with 26 NuttX-named commands, and an interpreter for loops over tool calls. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| It remembers what it was told | Standing orders, `/AGENTS.md`, a `/memory.md` index and skills, assembled into the prompt on every turn. [Context layers](docs/context.md) |
| No cloud in the path | HTTPS from the board, a certificate it generates, one PIN for three callers. [Security](docs/security.md) |
| Testable without hardware | The simulator runs the same `agent-core` against a virtual board. [Simulator](docs/simulator.md) |

## Flash and connect

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

With no stored credentials the board raises its own access point. Join it from a phone and
open `http://192.168.4.1/`, or send `provision <ssid> <password>` over USB. Credentials go
to NVS, never into the image.

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

Port 443 serves the same routes over TLS with a certificate generated on first boot; browsers
warn, so compare the fingerprint. Port 80 stays open, so a bad certificate costs a URL rather
than the board. `GET /agent.json` is the whole interface as data: 40 routes with method, body,
return and auth, plus every writable setting with its type and default.

## Repository layout

`crates/` builds and is tested on a host; `firmware/` is the ESP-IDF backend. `agent-core` is
the board behind one `Hardware` trait with no chip-specific code, so `cargo test` runs it on a
laptop; `agent-sim` implements that trait against a register file. Merging them would put the
simulator in every image and the firmware's dependencies in every test. `firmware/` opts out of
the workspace because it needs `cargo +esp`, `-Zbuild-std` and a checked-out ESP-IDF.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) · boards.rs ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

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

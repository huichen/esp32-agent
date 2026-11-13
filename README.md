# esp32-agent

**English** · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

Firmware for an ESP32 that runs its own agent loop and serves an MCP endpoint, so one model
can drive the board directly and any external agent can use the same tools over HTTP.

Flash it, join WiFi, and it probes the buses it has drivers for. Each device that answers
becomes MCP tools. The agent on the board sees those same tools and answers with them; a new
sensor can be declared in JSON while the board runs.

> The agent loop follows [FutureOS](https://github.com/futuregene/future-os): transcript,
> tool journal, stream events, protocol adapters, memory index. 

| What | In detail |
|---|---|
| On-board agent | Loop, tools, memory and transcript are firmware; the model is any OpenAI-compatible or Anthropic endpoint. [Model](docs/model.md) |
| Manifest-driven tools | `manifest` (`command: apply`) validates a JSON manifest, compiles it into tools, live in the same turn. [Manifest](docs/manifest.md) |
| Automatic device discovery | I2C, 1-Wire and declared pins are probed at boot; only what answers gets tools. [Capabilities](docs/capabilities.md) |
| One tool set, two callers | The MCP endpoint and the on-board agent see the same tools, from one description. [HTTP and MCP API](docs/http-api.md) |
| HTTPS console | Chat with the model and configure devices, tools, schedule, logs, WiFi and the model, from a phone; the board issues its own certificate. [Web console](docs/web-console.md) |
| Unattended operation | Timed prompts, and sensor rules that fire a prompt when a reading crosses a threshold. [Scheduling](docs/scheduling.md) |
| Shell and script | `shell` with 57 NuttX-named commands, and an interpreter for loops over tool calls. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Long-term memory | Standing orders, `/AGENTS.md`, a `/MEMORY.md` index and skills, assembled into the prompt on every turn. [Context layers](docs/context.md) |
| Built-in skills | One procedure ships in the firmware, `create-skill`, written with the standard `---` header — and it teaches the rest: what a skill is, how to write one. It lands in `/skills` on the first boot, and an edit stays yours. [Skills](docs/agent-skills.md) |
| The board is the server | HTTPS from the board, a certificate it generates, a portal PIN and a separate MCP token. [Security](docs/security.md) |
| Hardware-free testing | The simulator runs the same `agent-core` against a virtual board. [Simulator](docs/simulator.md) |

## Flash and connect

Build from source; no prebuilt release image is published. The default target is an
ESP32-S3 N16R8 with 16 MiB flash and 8 MiB octal PSRAM. Install the toolchain in
[Flashing](docs/flashing.md), then run:

```bash
bash scripts/build-firmware.sh --flash --port /dev/ttyACM0
```

On Windows, use `powershell -File scripts/build-firmware.ps1 -Flash -Port COM3`. The wrappers
build before flashing and explicitly pass the partition table and the `factory` app slot.

The board has no WiFi credentials, so it starts an access point named `esp32-agent-setup`.
Join it, open `http://192.168.4.1/`, type the network name and password, save. They go to
NVS and the board reconnects. Until that happens it reaches nothing outside the LAN: no
model endpoint, no package mirror.

## Use it

**From a browser**: `http://<board>/` (or `https://<board>/`) is the console. Chat with the
model, list the devices the probe found, turn tools on and off, read the log, set the PIN.

**From an agent**: `GET http://<board>/agent.json` returns the interface as JSON — every
route with its method, body and auth, the MCP surface, the settings table with types and
defaults, and the limits. An agent that can fetch a URL needs nothing else.

**A new board already knows how to learn.** `/skills/create-skill` is the one seeded procedure: it says what a skill is, the `---` header it must carry, and how to write one the next session will find. Everything else — the pinout of your bench, what to do when a board looks dead — is a skill the agent writes and then keeps. It is an ordinary workspace file, installed on the first boot wherever the path is empty; a factory reset restores it and `skills.seed_defaults=false` stops that. The first useful sentence to a freshly flashed board can be "write down your own pinout".

## Give it a model

Open `http://<board>/`, choose **Agent**, and fill in the endpoint, the model id and the key,
then save. The key is write-only: it goes to NVS, and reads report `api_key_set`, not the key.
The same panel
holds `protocol` (`chat`, `responses` or `anthropic`), `reasoning`, `context_tokens` and
`max_output_tokens`.

**Or let an agent do it.** Once an agent has fetched `agent.json` it knows these settings, their
types and their defaults, so a prompt is enough:

> point the board at `https://your-provider/v1/chat/completions`, model `…`, key `sk-…`,
> thinking `low`, context `64000`, output `4096`

The agent applies them with `POST /api/agent/config` — the same call the panel makes — and reads
the configuration back to confirm. No command line is needed on your side, and the same prompt
works for every board it can reach. Without a key at all, `base_url` set to
`http://127.0.0.1/selftest/chat/completions` runs the loop against the board itself.

## Safety notes

- **Credentials stay on the board.** WiFi goes to NVS through `provision`, never into the
  image; `api_key` is write-only and reads report only `api_key_set`.
- **Set both credentials before the board leaves your desk.** `auth.portal_pin` guards REST
  writes and config, schedule, transcript, prompt, workspace, attachment, log, run-journal and
  script-result reads. MCP uses `auth.mcp_token`; when that is unset, it falls back to the portal credential.
- **Compare the certificate.** The board's TLS certificate is self-signed; its fingerprint is at
  `GET /api/tls`. Port 80 stays open by default so a bad certificate costs a URL and not the
  board; `mcp.plain_http: false` closes it, and the USB cable is then the way back in.
- **`shell`, `script` and manifest tools move pins.** That is the point of the board; configure
  the credential for each interface to distinguish authorized callers from passers-by.

[Security](docs/security.md) states what is deliberately not defended.

## Status

| Part | State |
|---|---|
| `agent-core` | 839 unit tests and 5 integration tests pass; `scripts/smoke.ps1` passes end to end |
| Simulator | console, MCP and the agent loop run end to end |
| Firmware | historical bench firmware on an ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 tools, OLED 128×32, I²S mic and amp, WS2812, TLS console, model turns over HTTPS; audit changes not board-verified |
| Not implemented / unverified | OTA and firmware DVP camera capture are not implemented. SD raw-sector and DHT11/22 drivers have host tests, not board verification. See `ROADMAP.md` |

Historical bench numbers: a four-tool turn in 13.7 s; console over TLS in 0.55 s; I²C scan
stable over 8 runs. The early 5–6 kB internal-SRAM reading after a TLS page predates allocator
fixes (`10b16dd`) and is not a current limit. Later pool measurements and provenance are in
[Troubleshooting](docs/troubleshooting.md#how-much-memory-the-board-has). The audit firmware
has not been flashed or board-tested.

## Documentation

[Architecture](docs/architecture.md) · [Manifest](docs/manifest.md) · [Capabilities](docs/capabilities.md) ·
[HTTP and MCP](docs/http-api.md) · [Adding a device](docs/adding-a-device.md) · [Flashing](docs/flashing.md) ·
[Troubleshooting](docs/troubleshooting.md) · [Agent loop](docs/agent-loop.md) · [Stream](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Memory](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Audio](docs/audio.md) · [Audit report](docs/audit-2026-11.md)

## Contributing

A new peripheral starts as a manifest fragment under `examples/manifests/`; a Rust driver is
needed only for timing, calibration or state. `CATALOG` is where a driver registers itself.
Conventions are in [AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).

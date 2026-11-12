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
| A model runs on the board | Loop, tools, memory and transcript are firmware; the model is any OpenAI-compatible or Anthropic endpoint. [Give it a model](#give-it-a-model) |
| The tool set is a document | `manifest_apply` validates a JSON manifest, compiles it into tools, live in the same turn. [Manifest](#manifest) |
| It finds its own hardware | I2C, 1-Wire and declared pins are probed at boot; only what answers gets tools. [Capabilities](docs/capabilities.md) |
| One tool set, two callers | The MCP endpoint and the on-board agent see the same tools, from one description. [HTTP and MCP API](docs/http-api.md) |
| A console over HTTPS | Chat and configure devices, tools, schedule, logs, WiFi and the model from a phone; the board issues its own certificate. [Web console](#web-console) |
| It acts unattended | Timed prompts and sensor rules that fire one when a threshold crosses. [Scheduled work](#scheduled-work) |
| A shell and a script language | `shell` with 26 NuttX-named commands, and an interpreter for loops over tool calls. [Shell](#shell) · [Script](#script) |
| Standing orders | 2 KB in the system prompt for what this board is for, writable from the console, `shell` and HTTP. [Standing orders](#standing-orders) |
| No cloud in the path | HTTPS from the board, a certificate it generates, one PIN for three callers. [Authentication](#authentication) |
| Testable without hardware | The simulator runs the same `agent-core` against a virtual board. [Simulator](#simulator) |

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

## Web console

| Where | What you do there |
|---|---|
| Chat (the page) | talk to the model; tool calls and reasoning fold into one line per turn |
| Devices | what the probe found, pins, addresses, `Probe devices`, adoptable candidates |
| Tools | every tool with its input schema, and why one is hidden |
| Agent | endpoint, model, key, protocol, thinking level, temperature, context and output size |
| Schedule | timed prompts and sensor rules |
| Logs, Settings | the ring buffer; WiFi with gateway/DNS overrides, the PIN, reboot |

No build step and nothing loaded from the internet: about 58 KB of HTML, CSS and JavaScript
compiled in and served with `Cache-Control: no-store`, so a reflash shows on the next reload.

## Give it a model

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"write HELLO on the OLED, set the LED blue, then scan I2C"}'
```

Three protocols: `chat`, `responses`, `anthropic`. Images from the console go out as content
parts. `reasoning` sets the thinking level, `max_steps` caps model calls per turn (0 = no
cap), `context_tokens` and `max_output_tokens` set the window and the reply reserve, and the
transcript compacts at 80% of the window. Without a key, point `base_url` at the board's own
`/selftest/chat/completions` and the loop still runs.

## Standing orders

A slot for what this board is *for*, written by its owner and read on every turn:

```bash
curl -sS -X POST http://<board>/api/agent/prompt -H 'Content-Type: application/json' \
  -d '{"action":"set","text":"The pump relay is on GPIO12. Never run it past two minutes."}'
```

`action` is `set`, `append` or `clear`; `GET /api/agent/prompt` shows the prompt the model
will receive, custom part included, and the agent reaches it with
`shell {"command":"system_prompt set …"}`. 2 KB in NVS, so it survives a reboot.

## Shell

Every driver in the catalogue is documented in [docs/capabilities.md](docs/capabilities.md):
GPIO, ADC, PWM, I2C, 1-Wire, WS2812, SSD1306, BME280, SHT3x, INA219/226, MPU6050, ADS1115,
PCF8574, DS18B20, HC-SR04, servo, relay, I²S mic and amp.

One writable area, 8 KB of text in NVS. `read`, `write` and `edit` cover the common cases;
everything else is one `shell` command line, parsed with words, quotes and `--flags`. A tool
result is capped at 2 KB and a reply at 8 KB: the board has 8 MB of PSRAM and a few kilobytes
of internal SRAM, and the second number is the one that binds:

```text
shell {"command":"ls /notes"}            shell {"command":"grep GPIO /"}
shell {"command":"cp /a.txt /b.txt"}     shell {"command":"rm /a.txt --confirm"}
shell {"command":"curl https://example.com/"}          # GET, HTML reduced to text
shell {"command":"wget -o /dl/page.txt https://example.com/"}
shell {"command":"ping -c 3 192.168.3.1"}   shell {"command":"nslookup example.com"}
shell {"command":"ifconfig"}                # configured gateway and DNS, not the lease
shell {"command":"df | free | uptime | date | uname | ps"}
shell {"command":"sha256sum /notes/a.txt"}  shell {"command":"zip t.zip /a.txt"}
```

`edit` refuses an ambiguous match rather than rewriting the wrong place; `rm` and `mv` need
`confirm`. A wrong command answers with its usage line, taken from the same table the help
text comes from, so the two cannot disagree: `rm without --confirm` returns
`usage: rm <path> --confirm`. `shell {"command":"help"}` lists all 26.

## Script

For what fixed tools cannot express: a loop over readings, a branch on a result.

```text
let scan = tool("i2c_scan", {})
let i = 0
while i < len(scan.addresses) { log(scan.addresses[i])  i = i + 1 }
if i > 0 { "found " + str(i) } else { "nothing" }
```

Numbers, strings, booleans, nil, lists, maps; `let`, assignment, `if`/`else`, `while`,
`break`; built-ins `len`, `str`, `num`, `type`, `log` and `tool(name, args)`, which calls any
board tool with the caller's own permissions (`tool("script")` aside). `{"lint":true}` checks
syntax and unknown names with line and column and never executes; `{"help":true}` returns the
reference. Bounds: 30 s, five million steps, 32 nested blocks, 4 KB strings, 8 KB of source.
The clock is the real bound: the interpreter runs on the thread that answers the console.

## Scheduled work

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "tasks":[{"id":"t1","prompt":"read the BME280","every_s":300}],
  "rules":[{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,"prompt":"it is hot"}]}'
```

`at_hhmm` runs at a UTC wall-clock time instead. The agent can schedule itself with
`schedule {"action":"add", …}`. Tasks survive a reboot.

## Manifest

A device that needs no timing or calibration is declared, not compiled:

```json
{"devices":[{"id":"pump","kind":"actuator","pins":{"out":12},"note":"active-low relay"}],
 "tools":[{"name":"pump_pulse","description":"Energise the relay for `ms` ms",
   "params":[{"name":"ms","type":"integer","min":10,"max":5000,"required":true}],
   "exec":{"type":"sequence","steps":[
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":0}},
     {"ms":0,"exec":{"type":"delay","ms":{"param":"ms"}}},
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":1}}]},
   "safety":{"max_calls_per_min":20,"confirm":true}}]}
```

Three paths add hardware: a manifest (no timing or state machine, applies live), a compiled
driver (`drivers/<chip>.rs`, for state machines and strict timing), or a board profile (facts
that cannot be probed, such as which pins the LED and buttons use). A driver is listed only
when the probe finds the device, so a half-written one cannot report hardware that is not
there. [docs/adding-a-device.md](docs/adding-a-device.md) covers all three.

## Authentication

| Caller | Credential |
|---|---|
| Console | `POST /api/auth {"pin":"…"}` returns a bearer token the page keeps |
| Script | `X-Portal-Pin: <pin>`, or the same token |
| MCP client | `Authorization: Bearer <mcp.token>`; the PIN is accepted when no token is set |

With `auth.portal_pin` set, writes and the reads that expose transcript, prompt, config and
logs need it; status, devices, tools and `/agent.json` stay open. Tokens live in RAM, so a
reboot revokes them. The default is no PIN, and `/agent.json` says so in its `security` block.

## Simulator

```bash
cargo run -p esp32-agent-sim
```

The same `agent-core` against a virtual board on a real port. Only the `Hardware`
implementation differs, so the console, the MCP endpoint, the drivers, the manifest engine and
the agent loop are the code that runs on the chip.

## Repository layout

`crates/` builds and is tested on a host; `firmware/` is the ESP-IDF backend. `agent-core` is
the board behind one `Hardware` trait with no chip-specific code, so `cargo test` runs it on a
laptop; `agent-sim` implements that trait against a register file. Merging them would put the
simulator in every image and the firmware's dependencies in every test. `firmware/` opts out of
the workspace because it needs `cargo +esp`, `-Zbuild-std` and a checked-out ESP-IDF.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## Status

| Part | State |
|---|---|
| `agent-core` | 424 unit tests and 14 integration tests pass; `scripts/smoke.ps1` passes end to end |
| Simulator | console, MCP and the agent loop run end to end |
| Firmware | flashed on an ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 tools, OLED 128×32, I²S mic and amp, WS2812, TLS console, model turns over HTTPS |
| Not implemented | OTA, SD card, DHT11/22, camera. See `ROADMAP.md` |

Bench numbers: a four-tool turn in 13.7 s; console over TLS in 0.55 s; I²C scan stable over
8 runs; internal SRAM 5–6 kB free after a TLS page, the tightest constraint here.

## Documentation

[Architecture](docs/architecture.md) · [Manifest](docs/manifest.md) · [Capabilities](docs/capabilities.md) ·
[HTTP and MCP](docs/http-api.md) · [Adding a device](docs/adding-a-device.md) · [Flashing](docs/flashing.md) ·
[Troubleshooting](docs/troubleshooting.md) · [Agent loop](docs/agent-loop.md) · [Stream](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Memory](docs/agent-memory.md) · [Scripting](docs/scripting.md)

## Contributing

A new peripheral starts as a manifest fragment under `examples/manifests/`; a Rust driver is
needed only for timing, calibration or state. `CATALOG` is where a driver registers itself.
Conventions are in [AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).

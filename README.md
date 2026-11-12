# esp32-agent

**A board that is a body for a model, not a peripheral for one.**

Flash it, let it join WiFi, and it answers with its own tools: it looks at what is wired
up, turns the hardware into tool calls, and exposes them over MCP — to itself, and to any
agent that can reach it. Its pins, its settings, its files and its manifest are agent-
operable too, so the same interface that blinks an LED also reconfigures the board.

Nothing else needs to be written for a new sensor to become a tool. That is the point of
the project.

```text
flash ──▶ join WiFi ──▶ detect ──▶ MCP surface ──▶ a model that can use it
                                  ▲            ▲
                                  │            └── an external agent, over HTTP
                                  └─────────────── the board's own agent loop
```

> **The agent loop here is modelled on [FutureOS](https://github.com/futuregene/future-os)'s
> agent** — its transcript, tool-call journal, streaming event vocabulary, protocol
> adapters and memory index. Where this firmware had to differ (a few kB of RAM instead of
> gigabytes, no model-written summaries, deterministic compression) the code says why.

## Why this is different from "MCP on a microcontroller"

MCP servers usually expose a fixed set of functions someone compiled in. Here the tool
surface is a **document** that the board reads, validates, compiles and rewrites while it
runs — and the same document drives both the MCP endpoint and the board's own model loop.

| What you get | Why it matters |
|---|---|
| **Hardware it can find** | Built-in drivers probe I2C, 1-Wire and pins at boot; only what answered gets tools. Nothing is listed that the board cannot actually do. |
| **A tool surface it can rewrite** | `manifest_apply` validates and rebuilds the tool list **without a restart or a reflash**; a device the agent just discovered is callable in the same turn. |
| **Two kinds of caller, one surface** | 51 tools on the MCP endpoint — and the on-board agent sees that same list, `fs_*`/`net_*` included. No second description of the hardware to drift. |
| **A body, not just hands** | Pins and buses, the manifest, runtime configuration, `/AGENTS.md`, `/skills/*`, the memory index, the file workspace, timers, sensor rules and goals are all operations the agent can perform on itself. |
| **It can start with nothing and build its surface** | With `discovery: manual` the drivers probe but adopt nothing: the board boots with an empty device list and a candidate list that carries the exact `manifest_apply` body for each. A script — loops, conditions, `tool()` calls — finds the hardware and declares it, and a re-probe after a hot-plug follows reality. |
| **It acts while you are not looking** | `POST /api/sched` runs prompts on a clock or a sensor threshold; `POST /api/goal` gives it an objective and lets it iterate, one step per round, with notes that survive reboots. |
| **It remembers, and can forget** | `/memory.md` is an index the agent writes with `fs_write`, pointing at `/memory/<topic>.md`; the index is in every prompt, the bodies are opened only when needed. Old turns are compressed into a deterministic index (no summary, no model call), and `POST /api/agent/forget {"keep":N}` cuts the past away on purpose. |
| **Bring your own brain** | Any OpenAI-compatible `/chat/completions`, `http://` or `https://` (esp-tls + certificate bundle). Measured on the bench with `future-os.cn` and `deepseek-flash`: four tools in one turn, 13.7 s. |

Point an external agent at `GET /agent.json` and it can drive all of this without reading
this repository: every route, the MCP surface, the error shape, the limits, and a
`settings` table that says which field on which call changes what.

## The five minutes it takes

```bash
# once: Rust + Xtensa toolchain, and the flasher
cargo install espup espflash ldproxy
espup install

# every time: build and flash (ESP32-S3 by default)
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

The board comes up as an access point when it has no credentials:

```text
wifi: radio up: access point `esp32-agent-setup` on http://192.168.4.1/
http: listening on 0.0.0.0:80
```

Join it from a phone, open `http://192.168.4.1/`, pick your network. **No password is
compiled into the firmware** — credentials live in NVS, so moving networks costs nothing.
Prefer not to lose the host's network? The console takes
`provision <ssid> <password>` over the same USB cable you flashed with.

Then:

```bash
curl -sS http://<board>:80/agent.json | jq '.endpoints | length'   # 40 routes
curl -sS -X POST http://<board>:80/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
```

Port 443 answers too, with a certificate the board generated for itself on first boot:

```bash
curl -k -sS https://<board>/api/tls | jq -r .fingerprint   # SHA-256 of its own cert
```

A browser will warn about the certificate — that is what self-signed means. Compare the
fingerprint it shows with that line before clicking through; both HTTP and HTTPS stay up,
so a TLS problem costs a URL, never the board.

## Bring your own brain

The loop runs on the board: it is handed the board's own tools, calls them, and answers.

```bash
curl -sS -X POST http://<board>:80/api/agent/config \
  -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'

curl -sS -X POST http://<board>:80/api/agent/message \
  -H 'Content-Type: application/json' \
  -d '{"text":"清空屏幕，在第 0 行写 HELLO，把板载灯设成蓝色，再扫描 I2C"}'
```

A real run, on the bench board, answered by four tool calls in one turn:

| Step | Tool | Result |
|---|---|---|
| 1 | `board_status` | ESP32-S3, 6 devices, 192.168.3.7 |
| 2 | `oled_text {"line":0,"text":"HELLO"}` | `{"chars":5,"ok":true}` |
| 3 | `led_set {"color":"#0000ff"}` | `{"ok":true}` |
| 4 | `i2c_scan` | `0x3C` |

Model settings are themselves agent-operable: `base_url`, `model`, `api_key`,
`max_steps` (**0 = unlimited by default**), `temperature`, `timeout_s`. No key? Point
`base_url` at the board's own `/selftest/chat/completions` and the whole loop still runs.

## Hands, and the rest of the body

Everything below is an operation, not a fixed menu:

| Area | What an agent can do | Where |
|---|---|---|
| Hardware | GPIO, ADC, PWM, RGB, I2C, 1-Wire, OLED, sensors, servo, relay, I2S mic/amp | MCP tools |
| Discovery | `inventory`, `devices_list`, `i2c_scan`, `i2c_identify`, `devices_probe` | MCP tools |
| Manifest | `manifest_get/describe/validate/apply/reset` — live, no reboot | MCP tools |
| Files | `fs_ls/read/write/edit/rm/cp/mv/grep` over an NVS workspace | MCP tools |
| Network | `net_get`, `net_post` over `http://` and `https://` | MCP tools |
| Inbound TLS | `https://` on 443, self-signed certificate generated on the board (EC P-256, fingerprint in `/api/tls`); http on 80 stays | both ports |
| Config | WiFi, MCP, buses, pins, auth, log, UI, tick, discovery | `POST /api/config` |
| Scripts | loops, conditions and `tool()` calls over every tool above | `script` tool |
| Model | endpoint, model, key, steps, temperature, timeout | `POST /api/agent/config` |
| Time | cron-ish prompts (`every_s`, `at_hhmm`) and sensor rules | `POST /api/sched` |
| Purpose | a goal it iterates on across runs | `POST /api/goal` |
| Recovery | `reboot`, `factory-reset`, `devices_probe` | HTTP |

`GET /agent.json` carries this as data: a `settings` section with every writable field,
its type, its default, and the caveats a field name hides — "0 means no limit",
"write-only, `__set__` keeps the stored value", "needs a restart". Five groups, tested
against the route table so a group cannot outlive its endpoint.

## How self-configuration works

```
drivers      compiled in, when timing, calibration or a state machine is needed
       │ looked up by id
manifest     JSON in NVS: which devices exist, which tools they expose;
       │     primitives compose — the agent edits this and the tool list
       │     is rebuilt in place
       ▼
MCP layer    device independent, written once: JSON-RPC, HTTP, sessions,
             auth, capability filtering
```

| Step | Tool | What it gives |
|---|---|---|
| Discover | `inventory`, `i2c_scan`, `i2c_identify` | chip, buses, recognised devices, and addresses that answer with no driver |
| Model | `manifest_describe` | the reference in sections (`overview`, `schema`, `exec`, `examples`, `drivers`) — each sized to arrive whole inside a 2 KB tool result |
| Write | `manifest_apply` (`merge`/`replace`, `dry_run`) | validated and live immediately; failures return issues with a JSON Pointer |
| Verify | `tools/list`, `mcp://manifest` | the new tool is listed — call it |
| Iterate | edit and apply again | the tool list follows, no restart |

One fragment turns a relay nobody wrote code for into a tool:

```json
{
  "devices": [{ "id": "pump", "kind": "actuator", "pins": { "out": 12 }, "note": "active-low relay" }],
  "tools": [{
    "name": "pump_pulse",
    "description": "Energise the pump relay for `ms` milliseconds",
    "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "unit": "ms", "required": true }],
    "exec": { "type": "sequence", "steps": [
      { "ms": 0, "exec": { "type": "gpio_write", "pin": 12, "level": 0 } },
      { "ms": 0, "exec": { "type": "delay", "ms": { "param": "ms" } } },
      { "ms": 0, "exec": { "type": "gpio_write", "pin": 12, "level": 1 } }]},
    "safety": { "max_calls_per_min": 20, "confirm": true }
  }]
}
```

**Validation runs on the board.** `min`/`max`, `safety.min/max`, `max_calls_per_min` and
`confirm` are checked before a pin moves; an out-of-range call comes back `forbidden`
with the range, and tests call out of range on purpose.

## Drivers: compiled, declared, or written by you

Both paths exist, and the split is deliberate — this is the community's way in:

| Path | When | Where |
|---|---|---|
| **Manifest only** | the device speaks I2C or GPIO and needs no timing or state machine | `examples/manifests/*.json`, applies live, no reflash |
| **Compiled driver** | it needs a state machine, calibration or strict timing (SSD1306, INA226, DS18B20, I2S audio) | `crates/agent-core/src/drivers/<chip>.rs` |
| **Board profile** | facts that cannot be probed: which pins the LED/button/mic use | `crates/agent-core/src/boards.rs` |

`drivers/` is a directory of one-module-per-chip files, and adding one is a three-step
job documented in [docs/adding-a-device.md](docs/adding-a-device.md): write the module,
add it to `CATALOG`, add its probe to `detect()`. A driver gets its tools listed only
when the probe finds it, so a half-supported device cannot lie to the model. `planned`
entries in the catalogue are named so nobody duplicates work.

## Why the tool list does not grow without bound

- Hardware that is not present is not listed: `requires` is intersected with the
  capabilities the board detected.
- Two styles, switchable at runtime: `auto` (one tool per action) and `generic` (device
  tools collapse into `device_invoke`), plus `disabled_tools` by name.
- Tool results are capped at 2 KB and replies at 8 KB, so a runaway model cannot eat the
  board's RAM — the board has ~8 MB of PSRAM and a few dozen kB of internal SRAM.

## Security: three callers, one credential

The board is on a LAN with no cloud in the middle, so the credential is local and simple —
but it is the same one everywhere, because three different clients have to use it:

| Caller | How it authenticates |
|---|---|
| The portal (a person) | `POST /api/auth {"pin":"…"}` → a bearer token kept in `localStorage`; the page asks once and retries the request it was making |
| A script | `X-Portal-Pin: <pin>`, or the same bearer token |
| An MCP client | `Authorization: Bearer <mcp.token>`; if no MCP token is set but a portal PIN is, the PIN (or a token) is accepted |

With `auth.portal_pin` set, mutations **and** the reads that expose the transcript, the
prompt, the config and the logs all require it. Status, devices, tools, the schedule and
the discovery document stay open, so a monitor can watch the board without a credential.

Tokens live in RAM: a reboot revokes them, and changing the PIN invalidates the ones the
page holds. The default is no PIN and no token — `GET /agent.json` says so in its
`security` block rather than leaving it to be discovered, and the portal's Access panel
sets it in one field.

## 30 seconds without a board

```bash
cargo run -p esp32-agent-sim        # virtual board: OLED + BME280 + SHT3x + INA226
```

The simulator runs **the same `esp32-agent-core`**: same console, same MCP endpoint, same
drivers, same manifest engine, same agent loop. Only the `Hardware` implementation
differs — a register file in memory instead of ESP-IDF. Development, tests and demos need
no hardware.

## Repository layout

The split is a layer, not a duplicate: `crates/` is everything that runs on a host and is
tested there; `firmware/` is the ESP-IDF backend and nothing else.

```
crates/agent-core/          no chip needed (host-tested)
  hardware.rs               the one trait the firmware implements
  drivers/                  one module per chip (ssd1306 bme280 sht3x ina219 mpu6050
                            ads1115 pcf8574 ds18b20 hcsr04 servo relay led lcd audio)
  manifest.rs               validation, compilation into tools, merge, safety
  agent.rs, llm.rs          the loop: transcript, journal, dispatch, model client
  goal.rs, sched.rs         objectives across runs; prompts on a clock or a sensor
  tools/                    board, system, manifest, file, network tools
  http.rs                   HTTP/1.1: keep-alive, chunked, 100-continue, SSE
  app.rs                    state, JSON-RPC dispatch, /mcp and console routes
  sitemap.rs, settings.rs   the discovery document and what it lets you set
  virtual_board.rs          the same services on a fake chip
crates/agent-sim/           host process: the virtual board behind a real HTTP port
firmware/
  main.rs                   boot order: config → buses → radio → server → agent
  runner.rs                 the ESP-IDF agent thread and the TLS transport
  esp_hw.rs                 Hardware over ESP-IDF: GPIO/I2C/PWM/ADC/RMT/I2S
  console.rs                provisioning over USB-Serial-JTAG
  wifi.rs, dns.rs           AP+STA, captive DNS
  nvs_store.rs              the NVS key-value store
```

Two files used to be called `agent.rs` — one the loop, one the thread that drives it —
which made the layering look like a duplicate. They are `agent.rs` (core) and
`runner.rs` (firmware) now.

## Status (honest version)

| Part | Status |
|---|---|
| `agent-core` | ✅ 271 unit + 14 integration tests; `scripts/smoke.ps1` green end to end |
| Host simulator | ✅ console, MCP and the agent loop, end to end |
| ESP-IDF firmware | ✅ flashed and verified on an ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): boots, radio up, console + MCP, 51 tools, OLED 128×32, I²S mic/amplifier, WS2812 — **and a real model loop over TLS** |
| Bench measurements | model call over HTTPS end to end; a four-tool turn in 13.7 s; I²C scan stable over 8 runs; portal answers in ~0.1 s with a browser open |
| Not verified | OTA, SD card, DHT/RC522/VL53L0X drivers — see `ROADMAP.md` |

## Documentation

- [Architecture](docs/architecture.md) — the three layers and why they are cut this way
- [Manifest specification](docs/manifest.md) — fields, primitives, validation, examples
- [Capabilities and devices](docs/capabilities.md) — driver catalogue, identification, pins
- [HTTP / MCP API](docs/http-api.md) — routes, auth, sessions, and the `GET /agent.json` map
- [Adding a device](docs/adding-a-device.md) — manifest only, or a driver
- [Flashing and first setup](docs/flashing.md) — toolchain, partitions, both provisioning paths
- [Troubleshooting](docs/troubleshooting.md) — dark panel, empty I2C scan, portal that will not open
- [Agent loop](docs/agent-loop.md) and [agent stream](docs/agent-stream.md)
- [Instructions and skills](docs/agent-skills.md) — `/AGENTS.md` and `/skills/<name>.md`
- [Memory, compression and forgetting](docs/agent-memory.md) — `/memory.md` as an index, a deterministic transcript index instead of a summary, and `POST /api/agent/forget`
- [Manual discovery and scripting](docs/scripting.md) — boot with no devices, let a script find and declare them, then write loops over tool calls
- [Goal mode](docs/agent-goal.md) — objectives, notes, when it stops

## Contributing

A new peripheral should first be a `manifest` fragment under `examples/manifests/`; it
needs a Rust driver only when it needs timing or calibration. Both paths are welcome, and
`CATALOG` is where a new driver announces itself. See [CONTRIBUTING.md](CONTRIBUTING.md);
repository conventions live in [AGENTS.md](AGENTS.md).

## License

MIT, see [LICENSE](LICENSE).

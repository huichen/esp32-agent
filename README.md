# esp32-agent

Turn an ESP32 into an MCP server that **discovers what is wired to it and lets a model
reconfigure it while it runs**.

The firmware serves a Streamable HTTP MCP endpoint and a bilingual web console. It is
usable before it has any WiFi credentials: the board raises its own access point, a
phone joins it, fills in the home network at the fixed address `192.168.4.1`, and the
board joins it — **no password is ever compiled into the firmware**.

The point is not another gadget: it is the split between **drivers, the capability
description and the MCP glue**:

```
drivers      compiled into the firmware, written only when timing,
             calibration or a state machine is needed
      ↑ looked up by id
manifest     JSON in NVS: which devices exist, which tools they expose;
             primitives compose
      ↑ compiled to ToolDef
MCP layer    device independent, written once: JSON-RPC, HTTP,
             sessions, auth, capability filtering
```

Adding a peripheral therefore needs no reflash: the model runs `inventory` /
`i2c_identify` to see the wiring, writes a JSON manifest, `manifest_apply` rebuilds the
tool list on the spot — **no restart, no reflash** — calls the new tool, checks the
result, and adjusts. A session can tune the hardware it has.

---

## 30 seconds without a board

```bash
cargo run -p esp32-agent-sim          # virtual board: OLED + BME280 + SHT3x + INA219
# open  http://127.0.0.1:8080/      web console
# MCP   http://127.0.0.1:8080/mcp
```

```bash
curl -sS -X POST http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

The simulator runs **the same `esp32-agent-core`** as the firmware: same console, same
MCP endpoint, same drivers, same manifest engine, same validation. Only the `Hardware`
implementation differs — a register file in memory instead of ESP-IDF. So development,
tests and demos need no board.

## Flashing a board

```bash
# once: the toolchain (espup: Rust + Xtensa) and the ESP-IDF dependencies
cargo install espup espflash ldproxy
espup install

# every time: build and flash (ESP32-S3 by default; edit firmware/.cargo/config.toml
# to change the chip)
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

The serial console then prints:

```
I (...) esp32_mcp::wifi: radio up: access point `esp32-agent-setup` on http://192.168.4.1/, station joining `<ssid>`
I (...) esp32_mcp: http server listening on 0.0.0.0:8080
```

Join `esp32-agent-setup` from a phone, open `http://192.168.4.1/`, pick a network,
enter the password, save. The board joins it as a station and the console is reachable
at the new address too. The access point stays up by default (`wifi.ap_fallback`), so
the configuration page remains reachable when the network is not.

## How a model uses this

| Step | Tool | What it gives |
|---|---|---|
| Discover | `inventory`, `devices_list`, `i2c_scan`, `i2c_identify` | chip, buses, recognised devices, and **addresses that answer but have no driver** |
| Model | `manifest_describe` | the manifest JSON Schema, the eleven primitives, the driver catalogue, limits, examples |
| Write | `manifest_apply` (`mode: merge/replace`, optional `dry_run`) | validated and applied **immediately**; failures come back as issues with a JSON Pointer |
| Verify | `tools/list`, `mcp://manifest` | the new tool is listed; call it |
| Iterate | edit the JSON, `manifest_apply` again | the tool list is rebuilt without a restart or a reflash |

One manifest fragment turns a relay nobody wrote code for into a tool:

```json
{
  "devices": [{ "id": "pump", "kind": "actuator", "pins": { "out": 12 }, "note": "active-low relay" }],
  "tools": [{
    "name": "pump.pulse",
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

**Validation runs on the board, not only in the schema.** `min`/`max`,
`safety.min/max`, `max_calls_per_min` and `confirm` are all checked before a pin
moves; an out-of-range call comes back as `forbidden` with the range in the message,
and there are tests that call out of range on purpose.

## Why the tool list does not grow without bound

- Hardware that is not present **is not listed**: tools are declared by a driver or a
  manifest, and `requires` is intersected with the capabilities the board detected.
- Two styles, switchable from the console or `POST /api/tools`:
  - `auto`: one tool per action, clear names;
  - `generic`: device tools collapse into `device_invoke`, leaving a few dozen core
    tools — the answer once there are many peripherals.
- Individual tools can also be disabled by name.

## The agent on the board

The board does the work itself: a model loop runs on the ESP32 and is handed the
board's own tools.

- Send a message: `POST /api/agent/message {"text":"what is on this board?"}` → 202
  (queued); the loop can call tools across several steps, and the final text lands in
  `/api/agent/history?after=`
- Model: any OpenAI-compatible `/chat/completions` over plain `http://`; fill in
  `base_url` / `model` / `api_key` in the portal's agent panel (the key is stored in
  NVS and never echoed back)
- Tools: every MCP tool on the board, plus the file tools
  `fs_ls/fs_read/fs_write/fs_edit/fs_grep` over an NVS workspace, plus `net_get` /
  `net_post`. There is no shell — `ls` and `grep` are the parts of one that matter here
- Orchestration: `POST /api/sched` adds a scheduled prompt (`every_s` or `at_hhmm`
  plus the prompt) or a sensor rule (`tool` + `path` + `op` + `value`, rate limited);
  either way it enters the same one-slot queue as a user message
- No key needed to try it: `POST /selftest/chat/completions` on the board is a scripted
  model — point `base_url` at it and the whole loop runs; the host-side equivalent is
  `tools/stub_model.py` with `tools/agent_smoke.ps1`

Single-process trade-offs: one agent thread, one tick, one run at a time, a one-slot
queue that coalesces. The turn is split into phases so the model call happens **without
holding the app lock** — the portal keeps answering while the model thinks. The agent's
journal streams over `GET /api/agent/stream` (Server-Sent Events) with the host agent's
event names: `message_start`, `thinking_*`, `text_*`, `tool_use`, `tool_progress`,
`tool_result`, `usage`, `message_end`, `run_end`.

## Repository layout

```
crates/agent-core/      everything that does not need a chip (runs on the host, tested)
  hardware.rs         the one trait the firmware implements: GPIO/I2C/PWM/ADC/RGB/delay/WiFi
  drivers/            one module per chip: ssd1306 bme280 sht3x ina219 mpu6050
                      ads1115 pcf8574 ds18b20 hcsr04 servo relay led
  manifest.rs         manifest: validation, compilation into tools, merge, safety
  agent.rs, llm.rs    the agent loop: transcript, journal, tool dispatch, model client
  sched.rs            scheduled prompts and sensor rules
  tools/              board, system, manifest, file and network tools
  http.rs             blocking HTTP/1.1 (keep-alive, chunked, 100-continue, SSE)
  app.rs              state, JSON-RPC dispatch, /mcp and the console routes
  web/                the embedded console (no build step)
crates/agent-sim/       host simulator: virtual board, same services, same agent loop
firmware/               ESP-IDF: WiFi (AP+STA), NVS, buses and pins, entry point
docs/                   architecture, manifest, capabilities, HTTP API, devices, flashing
```

## Status (honest version)

| Part | Status |
|---|---|
| `agent-core` (protocol, HTTP, manifest, drivers, validation, console) | ✅ 184 unit/integration tests, `scripts/smoke.ps1` green end to end |
| Host simulator | ✅ runs, console and MCP usable end to end; the agent loop runs there too |
| ESP-IDF firmware | ✅ built, flashed and verified on an ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): boots without reset, radio up, console and MCP served, agent loop runs with tools |
| OTA, I2S audio, SD card, DHT/RC522/VL53L0X | 📋 see `ROADMAP.md` |

## Documentation

- [Architecture](docs/architecture.md) — the three layers and why they are cut this way
- [Manifest specification](docs/manifest.md) — fields, primitives, validation, examples
- [Capabilities and devices](docs/capabilities.md) — driver catalogue, identification, pins
- [HTTP / MCP API](docs/http-api.md) — routes, auth, sessions, tool conventions
- [Adding a device](docs/adding-a-device.md) — manifest only, or a driver
- [Flashing and first setup](docs/flashing.md) — toolchain, partitions, the setup flow
- [Troubleshooting](docs/troubleshooting.md) — dark panel, empty I2C scan, missing tools
- [Agent loop contract](docs/agent-loop.md) and [agent stream](docs/agent-stream.md) — the on-board loop and its API

## Contributing

The goal is community-added hardware: a new peripheral should first be a `manifest`
fragment under `examples/manifests/`, and only needs a Rust driver when it really needs
timing or calibration. See [CONTRIBUTING.md](CONTRIBUTING.md); the repository
conventions live in [AGENTS.md](AGENTS.md).

## License

MIT, see [LICENSE](LICENSE).

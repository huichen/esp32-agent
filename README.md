# esp32-agent

Firmware for an ESP32 that runs its own agent loop and serves an MCP endpoint, so one model
can drive the board directly and any external agent can use the same tools over HTTP.

Flash it, join WiFi, and it probes the buses and pins it has drivers for. Each device that
answers becomes MCP tools. The agent on the board sees those same tools, calls them, and
answers; a new sensor can be declared with a JSON manifest while the board runs.

> The agent loop follows [FutureOS](https://github.com/futuregene/future-os): transcript,
> tool journal, stream events, protocol adapters, memory index. Where the firmware differs
> (kilobytes instead of gigabytes of RAM, no model-written summaries) the code says why.

## Flash and connect

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

With no stored credentials the board raises its own access point:

```text
wifi: radio up: access point `esp32-agent-setup` on http://192.168.4.1/
http: listening on 0.0.0.0:80
```

Join it from a phone and open `http://192.168.4.1/`, or send
`provision <ssid> <password>` over the USB cable. WiFi credentials are kept in NVS, not
compiled in.

Check the board:

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

Port 443 serves the same routes over TLS with a certificate the board generates on first
boot. Browsers warn about it; compare the fingerprint in `/api/tls` with the one the
browser shows. Port 80 stays open, so a TLS misconfiguration costs a URL and not the board.

`GET /agent.json` is the whole interface as data: 40 routes with method, body, return and
auth, the MCP surface, error shape, limits, and a `settings` table listing every writable
field with its type and default.

## Give it a model

```bash
curl -sS -X POST http://<board>/api/agent/config \
  -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'

curl -sS -X POST http://<board>/api/agent/message \
  -H 'Content-Type: application/json' \
  -d '{"text":"清空屏幕，在第 0 行写 HELLO，把板载灯设成蓝色，再扫描 I2C"}'
```

The model client speaks three protocols: `chat` (OpenAI chat completions), `responses`
(OpenAI Responses API) and `anthropic` (Messages API). Images attached in the portal are
sent as content parts. `reasoning` selects the thinking level (`off`/`low`/`medium`/`high`);
`max_steps` caps model calls per turn and is unlimited by default.

Without a key, point `base_url` at the board's own `/selftest/chat/completions` and the loop
still runs.

## What the board can do

| Area | Tools |
|---|---|
| Hardware | GPIO, ADC, PWM, I2C, 1-Wire, WS2812, SSD1306, BME280, SHT3x, INA219/226, MPU6050, ADS1115, PCF8574, DS18B20, HC-SR04, servo, relay, I²S microphone and amplifier |
| Discovery | `inventory`, `devices_list`, `i2c_scan`, `i2c_identify`, `devices_probe` |
| Manifest | `manifest_get`, `manifest_describe`, `manifest_validate`, `manifest_apply`, `manifest_reset` |
| Shell | `shell` with 14 commands: `ls cat write edit rm cp mv grep` on the workspace, `get post dns tcp` on the network, `zip unzip` on archives |
| Model support | `script` (loops and conditions over any tool), `image_read` (asks the vision model about an attachment), `chip_temp` |
| Scheduling | `schedule` tool, `POST /api/sched` for timed prompts and sensor rules |
| Configuration | `config_set`, `POST /api/config` (WiFi, buses, pins, MCP, auth, log, UI, discovery) |

Tool results are capped at 2 KB and replies at 8 KB. The board has 8 MB of PSRAM and a few
dozen kilobytes of internal SRAM; the limits exist because of the second number.

## Shell

The board has one writable area, 8 KB of text in NVS, and two ways to use it: three tools
for the common operations, and a `shell` for the rest.

```text
read   {"path":"/notes/a.txt"}
write  {"path":"/notes/a.txt","text":"..."}
edit   {"path":"/notes/a.txt","find":"old","replace":"new"}

shell  {"op":"ls","path":"/notes"}
shell  {"op":"cp","from":"/a.txt","to":"/b.txt"}
shell  {"op":"mv","from":"/a.txt","to":"/b.txt","confirm":true}
shell  {"op":"rm","path":"/a.txt","confirm":true}
shell  {"op":"grep","pattern":"GPIO","path":"/"}
shell  {"op":"zip","name":"t.zip","files":{"a.txt":"hello"}}
shell  {"op":"unzip","archive_b64":"UEsDB..."}
```

`edit` refuses when `find` occurs zero or several times, so an ambiguous edit cannot rewrite
the wrong place. `rm` and `mv` need `confirm`. `zip` writes stored entries and returns the
archive as base64, because a tool result is capped at 2 KB; `unzip` reads stored and
deflated entries and checks each CRC.

Network commands use the names they have everywhere else:

```text
curl     {"url":"https://example.com/"}                       # GET, HTML reduced to text
curl     {"url":"...","data":"{\"a\":1}"}                    # POST
curl     {"url":"...","format":"raw"}                         # the document as it arrived
wget     {"url":"https://example.com/","path":"/dl/page.txt"} # save the body
nslookup {"host":"example.com"}
nc       {"host":"example.com","port":443}
ping     {"host":"192.168.3.1","count":3}
ifconfig {}                                                   # ip, gateway, DNS, SSID, RSSI, MAC
```

`ifconfig` reports the configured gateway and DNS rather than the lease, which answers
"did my override take effect". `ping` sends real ICMP echoes through ESP-IDF's ping session.

**When a command is wrong, the error says how to write it:**

```text
rm without confirm → "`confirm` must be true" ... usage: rm <path> --confirm
unknown op         → "`sudo` is not a shell command" ... followed by every usage line
{"op":"rm","help":true} → usage: rm <path> --confirm
{"help":true} on curl, read, ping, … → the same one-line usage
```

`shell {"op":"help"}` lists every command. The usage lines come from one table, so the
error suffix and the help output cannot disagree.

## Script

`script` is a small language for what the fixed tools cannot express: a loop over sensor
readings, a condition on a result, a sequence that depends on the previous answer.

```text
let scan = tool("i2c_scan", {})
let n = len(scan.addresses)
let i = 0
while i < n {
  log("address " + scan.addresses[i])
  i = i + 1
}
if n > 0 { "found " + str(n) } else { "nothing" }
```

Values are numbers, strings, booleans, nil, lists `[1,2]` and maps `{a: 1}`. Statements are
`let`, assignment (including `l[0] = 9` and `m.k = 1`), `if`/`else`, `while`, `break` and
blocks. Operators are `+ - * / %`, `== != < <= > >=`, `and or not`. Built-ins are `len`,
`str`, `num`, `type`, `log`, and `tool(name, args)`, which calls any board tool with the
caller's own permissions.

`tool` is the only way out, so a script can do exactly what the caller could do — and
`tool("script")` is refused. `script {"lint": true, "source": …}` checks syntax and unknown
function names with line and column, and never executes. `script {"help": true}` returns the
reference in three sections, each small enough to arrive whole in a 2 KB result.

Bounds: 30 seconds of wall clock, five million steps, 32 nested blocks, 4 KB strings, 64 log
lines, 8 KB of source. The clock matters because the interpreter runs on the thread that
answers the portal: a script that does not finish makes the board unreachable, so the time
limit is what protects it.

A full run on the bench, from a prompt asking for π to twenty decimal places: doubles reach
15 digits, so the model wrote a fixed-point big-number loop over lists and returned
`3.141592653589793238462648…` in 22 000 steps.

## Manifest

A device that needs no timing or calibration is declared, not compiled:

```json
{
  "devices": [{ "id": "pump", "kind": "actuator", "pins": { "out": 12 }, "note": "active-low relay" }],
  "tools": [{
    "name": "pump_pulse",
    "description": "Energise the pump relay for `ms` milliseconds",
    "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "required": true }],
    "exec": { "type": "sequence", "steps": [
      { "ms": 0, "exec": { "type": "gpio_write", "pin": 12, "level": 0 } },
      { "ms": 0, "exec": { "type": "delay", "ms": { "param": "ms" } } },
      { "ms": 0, "exec": { "type": "gpio_write", "pin": 12, "level": 1 } }]},
    "safety": { "max_calls_per_min": 20, "confirm": true }
  }]
}
```

`manifest_apply` validates the document, rebuilds the tool list and takes effect in the same
turn. Validation runs on the board: parameter ranges, `safety` limits and the per-minute cap
are checked before a pin moves, and an out-of-range call returns the allowed range.

Three paths add hardware:

| Path | Use | Where |
|---|---|---|
| Manifest only | I2C or GPIO devices with no timing or state machine | `examples/manifests/*.json`, applies live |
| Compiled driver | state machines, calibration, strict timing | `crates/agent-core/src/drivers/<chip>.rs` |
| Board profile | facts that cannot be probed, such as which pins the LED and buttons use | `crates/agent-core/src/boards.rs` |

Drivers are listed only when the probe finds the device, so a half-written driver cannot
report hardware that is not there. [docs/adding-a-device.md](docs/adding-a-device.md) covers
both paths. `drivers/` has one module per chip, and `CATALOG` is where a new driver
announces itself.

## Authentication

The board is on a LAN, so the credential is local. One PIN covers three callers:

| Caller | Credential |
|---|---|
| Portal | `POST /api/auth {"pin":"…"}` returns a bearer token the page keeps and retries with |
| Script | `X-Portal-Pin: <pin>`, or the same bearer token |
| MCP client | `Authorization: Bearer <mcp.token>`; a portal PIN is accepted when no MCP token is set |

With `auth.portal_pin` set, writes and the reads that expose the transcript, prompt, config
and logs require it. Status, devices, tools, schedule and `/agent.json` stay open. Tokens
live in RAM, so a reboot revokes them. The default is no PIN; `/agent.json` reports that in
its `security` block.

## Simulator

```bash
cargo run -p esp32-agent-sim
```

Runs the same `esp32-agent-core` against a virtual board (OLED, BME280, SHT3x, INA226)
with a real HTTP port. Only the `Hardware` implementation differs, so the console, the MCP
endpoint, the drivers, the manifest engine and the agent loop are the code that runs on the
chip.

## Repository layout

`crates/` is everything that builds and is tested on a host. `firmware/` is the ESP-IDF
backend.

```
crates/agent-core/          no chip needed, host-tested
  hardware.rs               the trait firmware implements
  drivers/                  one module per chip
  manifest.rs               validation, compilation into tools, merge, safety
  agent.rs, llm.rs          the loop and the model client (three protocols)
  goal.rs, sched.rs         objectives across runs; timed prompts and sensor rules
  tools/                    board, manifest, shell, script, image, knowledge
  http.rs                   HTTP/1.1: keep-alive, chunked, 100-continue, SSE
  app.rs                    state, JSON-RPC dispatch, /mcp and console routes
  sitemap.rs, settings.rs   the discovery document and the writable settings
  virtual_board.rs          the same services on a fake chip
crates/agent-sim/           host process: the virtual board behind an HTTP port
firmware/
  main.rs                   boot order: config → buses → radio → server → agent
  runner.rs                 the agent thread and the TLS transport
  esp_hw.rs                 Hardware over ESP-IDF: GPIO/I2C/PWM/ADC/RMT/I2S
  console.rs                provisioning over USB-Serial-JTAG
  wifi.rs, dns.rs           AP+STA, captive DNS, gateway and DNS overrides
  nvs_store.rs              the NVS key-value store
```

## Status

| Part | State |
|---|---|
| `agent-core` | 318 unit tests and 14 integration tests pass; `scripts/smoke.ps1` passes end to end |
| Simulator | console, MCP and the agent loop run end to end |
| Firmware | built and flashed on an ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 45 tools, OLED 128×32, I²S mic and amplifier, WS2812, TLS portal, model turns over HTTPS |
| Not implemented | OTA, SD card, DHT/RC522/VL53L0X drivers; video decoding. See `ROADMAP.md` |

Bench measurements: a four-tool model turn in 13.7 s; portal page 7 535 B in 0.55 s over
TLS; I²C scan stable across 8 runs; `net_get` of a web page reduced to 3 943 bytes of text.
Internal SRAM after a TLS page is 5–6 kB free, which is the tightest constraint in the
design.

## Documentation

- [Architecture](docs/architecture.md)
- [Manifest specification](docs/manifest.md)
- [Capabilities and devices](docs/capabilities.md)
- [HTTP / MCP API](docs/http-api.md)
- [Adding a device](docs/adding-a-device.md)
- [Flashing and first setup](docs/flashing.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Agent loop](docs/agent-loop.md) and [agent stream](docs/agent-stream.md)
- [Instructions and skills](docs/agent-skills.md)
- [Memory, compression and forgetting](docs/agent-memory.md)
- [Manual discovery and scripting](docs/scripting.md)
- [Goal mode](docs/agent-goal.md)

## Contributing

A new peripheral starts as a manifest fragment under `examples/manifests/`. A Rust driver is
needed only for timing, calibration or state. Both are welcome; `CATALOG` is where a driver
is registered. Conventions are in [AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).

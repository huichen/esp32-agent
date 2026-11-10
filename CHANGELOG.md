# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
version numbering follows semantic versioning.

## [Unreleased]

### Added

- **On-board agent**: a model loop that runs on the ESP32 (`llm.rs` + `agent.rs`) against
  an OpenAI-compatible `/chat/completions`; one turn may call tools in several steps, and
  history is persisted to NVS and visible in the portal
- **Agent tools**: every MCP tool the board already had, plus file tools
  (`fs_ls/fs_read/fs_write/fs_edit/fs_grep` over an NVS workspace) and network tools
  (`net_get/net_post`); there is no shell, because the device does not have one
- **Orchestration layer**: scheduled tasks (`every_s` or `at_hhmm`, once a day, never
  twice in the same minute) and sensor rules (a tool, a JSON path in its result, a
  comparison, and a rate limit); both simply put one prompt on the same queue
- **Agent API**: `/api/agent/message`, `/api/agent/history?after=`, `/api/agent/reset`,
  `/api/agent/config`, `/api/sched`, plus the "agent" panel in the portal (send,
  conversation, schedule table, model connection)
- **Live journal**: `GET /api/agent/stream` serves the run journal over SSE (replay from
  `after=<seq>`, then follow), with a heartbeat, an idle timeout, and at most two
  concurrent streams
- **`tool_progress`** events bracketing each tool call with `start`/`end` and
  `elapsed_ms`, completing the host agent's event vocabulary
- **`agent.tick_ms`** (200–5000, default 1000): how often the agent thread evaluates the
  schedule, adjustable from the console without a reboot
- **Keyless self-test**: `POST /selftest/chat/completions` on the board (a scripted model
  served before the App lock) plus `tools/stub_model.py` on the host and
  `tools/agent_smoke.ps1`, which together verify the whole loop without an API key
- Manifest layer (`manifest.rs`): devices and tools declared in JSON, validated and
  applied live on the board without a reboot or a reflash
- 11 primitives: `gpio_write/read`, `toggle`, `pwm`, `adc`, `i2c_read/write`, `rgb`,
  `delay`, `driver`, `sequence`
- `safety` enforced on the board: ranges, rate limits, `confirm`
- Drivers: SSD1306/SH1106, PCF8574+HD44780, BME280/BMP280, SHT3x, INA219/INA226, MPU6050,
  ADS1115, DS18B20, HC-SR04, servo, relay, RGB LED, buttons
- Auto-detection: I2C scan plus register-level identity; 1-Wire presence plus ROM CRC;
  per-board pin tables
- `i2c_identify`: for an unknown address, candidate drivers and a register snapshot
- MCP: `initialize` (instructions generated from the actual hardware), `tools/list`
  (capability-filtered), `tools/call`, `resources/*` (capabilities/devices/manifest/
  config/logs/catalog)
- Two tool styles: `auto` (one tool per action) and `generic` (`device_invoke` as a
  single entry point)
- HTTP console: WiFi provisioning, device table, tool table, manifest editor (validate +
  apply), logs, client configuration
- Setup access point: with no credentials the board brings up `esp32-agent-setup`, serves
  `192.168.4.1`, and answers DNS for everything
- Host simulator `agent-sim`: a virtual register board running the same services, and it
  drives the agent loop and the scheduler too
- Documentation: architecture, the manifest specification, the capability table, the HTTP
  API, adding a device, flashing, troubleshooting, the roadmap, AGENTS.md

### Changed

- **Phased turn**: the model call no longer holds the `App` lock. `Agent::begin` computes
  the request and `Agent::apply` consumes the reply, so the platform layer releases the
  lock across the network wait — the portal stays responsive while a turn is in flight
- **Transcript and journal follow the host agent's shape**: a `reasoning` block separate
  from `text`, `is_error` on tool results, per-call `usage`, and the event vocabulary
  `message_start / thinking_* / text_* / tool_use / tool_progress / tool_result / usage /
  message_end / run_end`
- The project is renamed **esp32-agent**: package names
  `esp32-agent-core/sim/firmware`, binary `esp32-agent`, setup AP `esp32-agent-setup`, MCP
  `serverInfo`, and the portal title all follow. The MCP protocol and the `/mcp` path are
  unchanged (it is the transport now), tool names are unchanged (renaming them would break
  stored manifests and prompts), and the storage namespace stays `esp32mcp` (renaming it
  would wipe the settings and history of an already-flashed board)

### Fixed

- `tools/list` serialises tool by tool instead of building one `Value` tree first: the
  tree for 28 tools exhausted the heap on an S3 and aborted (the board rebooted on a
  client's first `tools/list`)
- Firmware boot order: lwIP, the NVS partition and the event loop are initialised in
  order; `Peripherals`, the NVS partition and the system event loop are each taken once
  and handed to the radio thread (a second `take()` returns `ESP_ERR_INVALID_STATE` and
  the thread exits silently)
- I2C explicitly requests `I2C_CLK_SRC_XTAL` (the `zeroed()` default is not on the S3
  clock tree, so `i2c_new_master_bus` returned 258)
- Station mode calls `connect()` after `start()` and retries when it drops; SNTP, which
  `at_hhmm` depends on, starts alongside it
- The setup AP is pinned to `192.168.4.0/24` (esp-idf-svc hands out `192.168.71.0/24` by
  default, which disagreed with the documented address and the captive DNS answer)
- `sdkconfig.defaults` and `partitions.csv` actually take effect: `Project name:
  esp32-agent`, and the repository's seven-partition table (with `otadata`/`ota_0`/`ota_1`)
  ships with the image and enumerates through `esp_partition` at runtime
- Device detection: `board: auto` no longer commits to one I2C wiring — it opens a
  candidate bus too, and whichever answers decides the board profile
- `uptime_s` waits for a real wall clock before counting (it used to report decades after
  SNTP set the clock)
- Changing I2C pins or the board profile now really requires a restart: the comparison is
  against the **effective** configuration (`before.bus != cfg.bus || before.board !=
  cfg.board`), which sets `restart_requested` and returns `restart_required: true`. A
  driver that has opened its bus never changes pins, so it used to keep the old wiring
  while reporting success, and the next probe scanned pins nothing was connected to
- Panel height comes from `pins["oled.height"]` (default 64): the carrier is 128×32, so a
  hard-coded 64 put half the frame off the panel while the tool still returned ok.
  `crates/agent-core/tests/display_height.rs` pins both geometries and the path from
  configuration to driver
- The file workspace and the tools agree on one contract: `Workspace::list` returns child
  names (it returned full paths, so `fs_grep` walked `/bench//bench/x` and reported zero
  files while `fs_read` worked)
- A refused event stream answers 503 with `Retry-After: 5`, so a console retries instead
  of hammering
- Connection threads use a 10 KB stack: at 20 KB each, six of them plus the agent and
  radio threads exhausted internal RAM (`could not spawn connection thread`), which the
  portal's heap figure hid because it counts PSRAM
- The simulator actually drives the agent loop and the scheduler, so a queued message no
  longer sits `pending` forever

### Known gaps

- `net_*` pointing at one of the board's own routes deadlocks on the App lock
- The model interface supports `http://` only (`https://` is refused with a clear error);
  a LAN proxy is the expected deployment
- The file workspace lives in memory and persists as one NVS blob (≤ 2 KB per file, ≤ 8 KB
  total); SPIFFS is still to come
- `fs_grep` has host tests but has not been driven through the loop on hardware;
  `/api/ota` is not implemented (the partitions are ready); WS2812/RMT and the ADC/PWM
  values are unmeasured
- There is no skill mechanism and the board does not read `AGENTS.md`; the agent's
  behaviour comes from a fixed system prompt

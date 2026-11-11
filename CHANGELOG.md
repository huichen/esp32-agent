# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
version numbering follows semantic versioning.

## [Unreleased]

### Added

- **`board.auto_peripherals`**: the devkit's own RGB LED and BOOT button are no longer
  forced into the device list. With `false` the board comes up with the bus, GPIO and
  manifest tools (44 instead of 48 on an S3-DevKitC-1) and gets them back by declaring
  `led` / `buttons` in a manifest — the minimal start a different board can grow from.
  Set from the portal, `config_set`, or `POST /api/config`; it takes effect on the next
  probe, no reboot
- **`buttons` as a manifest driver**: `pins` is a name→GPIO map, e.g. `{"boot": 0}`, so a
  button set can be declared like any other device

- **Goal loop**: state one objective (`POST /api/goal`) and the board iterates on its own
  — each iteration is one small step, its result becomes a note that the next iteration
  reads, and a reply saying `GOAL MET` / `GOAL BLOCKED` ends the run; the goal lives in
  NVS across reboots, is capped by `max_iterations`, aborts after three consecutive
  failures, and is one at a time. Contract: `docs/agent-goal.md`
- **On-board agent**: a model loop that runs on the ESP32 (`llm.rs` + `agent.rs`) against
  an OpenAI-compatible `/chat/completions`; one turn may call tools in several steps, and
  history is persisted to NVS and visible in the portal
- **Agent tools**: every MCP tool the board already had, plus file tools
  (`fs_ls/fs_read/fs_write/fs_edit/fs_rm/fs_grep` over an NVS workspace) and network tools
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
- **`https://` for model and `net_*` calls**: the firmware's transport carries the scheme
  — plain `http://` over `std::net`, `https://` over esp-tls with the ESP-IDF
  certificate bundle (no skip-verification switch) — and the simulator uses rustls with
  the webpki roots, so the same endpoint can be tried on the desk before it is flashed
- **Instructions and skills on the board**: `/AGENTS.md` is injected into the system
  prompt (first 1 KB, the rest readable with `fs_read`), and each
  `/skills/<name>/SKILL.md` is indexed with its path — a skill may keep other files next
  to it. Deliberately no dedicated tools: the paths are ordinary files, and
  `fs_ls`/`fs_read`/`fs_write`/`fs_rm` reach them. Contract: `docs/agent-skills.md`
- Documentation: architecture, the manifest specification, the capability table, the HTTP
  API, adding a device, flashing, troubleshooting, the roadmap, AGENTS.md

### Changed

- **The model's tool list is ordered least-volatile-first**: board tools, then `fs_*` and
  `net_*`, then manifest tools — alphabetical inside each block. Providers cache a request
  by prefix, so a mid-turn `manifest_apply` used to shift every tool after the inserted
  name; now only the tail moves. Same board, same bytes: a test compares two identically
  configured boards, a repeated call, and a re-probe

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

- **`fs_rm`**: deleting one workspace file (`confirm: true`). The workspace could write
  and edit but never remove, so a wrong skill stayed in every future prompt

- **`fs_*` and `net_*` on the MCP surface**: an outside client can read and write the
  workspace, fetch a page and post JSON, through the same implementation the board's own
  agent uses. `mcp.disabled_tools` hides them like any other tool
- **`fs_cp` / `fs_mv`**: copy or move a file, or every file under a directory (`mv` needs
  `confirm: true`); the workspace had one-way writes only

### Added

- **I2S audio: microphone and amplifier tools.** `mic_level` (RMS/peak in dBFS over a
  window), `mic_record` (base64 PCM, at most 512 samples so the result fits one tool
  reply), `speaker_tone` (generated sine) and `speaker_play` (base64 PCM). Nothing here
  is *detected* — I2S has no addressing and no id register — so the pins come from the
  board profile or `config.pins`, the channels are brought up once, and the tools exist
  only if that succeeded. Formats are the ones measured on this board: microphone
  16 kHz / 32-bit slots / left channel, amplifier 24 kHz / 16-bit / Philips. The
  firmware backend is in `esp_hw.rs` and reads by `bytes_read` rather than by the return
  code, because `i2s_channel_read` times out while still handing back samples
- **WS2812 output actually works**: `ring_set` was a TODO answering "addressable RGB is
  not implemented", so a board listed an LED whose tool always failed. It now drives the
  chain through `rmt_new_bytes_encoder` (10 MHz, 3/9 and 9/3 tick symbols, GRB payload)
- **`examples/manifests/lamp-strip.json`**: the xiaozhi demo strip on GPIO18, so a board
  with two WS2812 outputs can carry both

### Fixed

- **Two devices of one kind no longer share a tool name.** The board's second WS2812
  output (a strip on GPIO18) registered its `led_set` over the devkit LED's, so the
  second device silently made the first unreachable while `devices_list` claimed both
  were callable — two BME280s had the same problem waiting. The first keeps the plain
  name and the rest become `_2`, `_3`, in both `tools/list` and `devices_list`
- **A manifest device keeps the id the manifest gave it.** `instantiate` returned the
  driver's default id (`led`), so `device_invoke` could only reach the first device of a
  kind; a `NamedDevice` wrapper now carries `spec.id`

- **`mic` / `speaker` were advertised as capabilities with nothing behind them.** A board
  profile can record the I2S pins (the xiaozhi wiring does), and `board_caps` turned that
  into `mic`/`speaker` caps — but there is no INMP441 or MAX98357A driver, so the model
  was told about a device that has no tool, and no amount of probing would find it: I2S
  has no addressing and no id register. The caps are gone until the drivers exist
- **`tools_mode = "generic"` never took effect.** Two independent faults: the config
  patch accepted only `auto`/`all` and ignored anything else in silence, and
  `Config::sanitized` rewrote every value except `all` back to `auto`, so the mode was
  discarded on the next config write. With both fixed, folding the per-device tools into
  `device_invoke` removes eight definitions on a populated S3-DevKitC-1
  (48 tools → 36 with `auto_peripherals=false`), and an unknown `tools_mode` is now an
  error instead of a no-op

- A manifest can no longer take a name the board already provides. `manifest_apply`
  registered manifest tools last, so a tool called `manifest_apply` (or `bme280_read`)
  simply replaced the board's own: applying it removed the very tool needed to fix the
  manifest, and a later probe could bring a driver tool back on top of the replacement.
  Validation now refuses the clash with a rename hint, and `rebuild` skips a clashing tool
  from a manifest stored by an older build (logging which names it ignored)

- `net_get` / `net_post` now work inside a turn. The model call borrowed the App's only
  HTTP client, so a tool call during a turn found none — and the simulator never attached
  one at all, which is why the same code worked on a board and not on the desk. The App
  owns a tool transport (`attach_tool_transport`), the driver loop owns the model one
- `manifest_describe` answers one section per call (`overview`, `schema`, `exec`,
  `examples`, `drivers`, with examples and drivers paged by name) instead of returning
  8 KB into a 2 KB tool result: the model used to receive a quarter of the reference and
  invent the rest
- Manifest tool names are checked against the wire charset (`^[a-zA-Z0-9_-]+$`) when the
  manifest is applied, not when the request is built: a dotted name such as `pump.pulse`
  was accepted by the board and then rejected by every OpenAI-compatible endpoint, and
  because each request carries the whole tool list it failed **every** call — including
  the one that would have repaired the manifest. A name already stored in NVS is dropped
  at load, with a log line, so an affected board recovers on reboot
- `manifest_validate` / `manifest_apply` name the document shape and a JSON Pointer when
  the body does not fit (`invalid type: sequence, expected a map` cost a model six steps
  of guessing), and refuse a mistyped top-level field instead of silently ignoring it
- `scripts/build-firmware.ps1` actually builds: it ran the default toolchain (`can't find
  crate for core`), pointed `LIBCLANG_PATH` at a file instead of its directory, kept
  artifacts under `firmware/target` (too long for esp-idf-sys' 88-character check on
  Windows), and stamped a BOM into `firmware/.cargo/config.toml` on every run
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
- `setup.needed` and the instructions handed to an MCP client follow the stored
  credentials: a joined board used to announce "setup mode … waiting for credentials"
  whenever `wifi.ap_fallback` kept the access point up, because the station reports its
  state as `ap+sta`; a board that had never been provisioned could report the opposite
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
- The self-test model matches its script keywords as whole words: it is fed back its own
  output, so a goal note quoting `board_profile` used to look like a request about files
  and burn the step budget on the wrong tool

### Known gaps

- `net_*` pointing at one of the board's own routes deadlocks on the App lock
- The file workspace lives in memory and persists as one NVS blob (≤ 2 KB per file, ≤ 8 KB
  total); SPIFFS is still to come
- `fs_grep` has host tests but has not been driven through the loop on hardware;
  `/api/ota` is not implemented (the partitions are ready); WS2812/RMT and the ADC/PWM
  values are unmeasured
- There is no skill mechanism and the board does not read `AGENTS.md`; the agent's
  behaviour comes from a fixed system prompt

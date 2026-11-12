# Troubleshooting

Start with `GET /api/logs` (or the console's log tab). Every boot-time line carries a
`target` (`boot`/`probe`/`wifi`/`http`/`tool`/`manifest`).

## The board will not come up

| Symptom | Check |
|---|---|
| Nothing on the serial console | Is the console not USB-Serial/JTAG? Change `CONFIG_ESP_CONSOLE_*`; baud rate 115200 |
| Reboots right after `rst:0x…` | Read the `panic` line; usually PSRAM settings that do not match the board (`CONFIG_SPIRAM_MODE_OCT`) |
| `i2c bus 0 (SDA 8, SCL 9) failed` | The pin does not exist or is taken; change `config.bus.i2c` or pick other pins in the console |
| `no stored config` in the log | Normal: first power-up, so it falls back to provisioning |

## Provisioning

| Symptom | Check |
|---|---|
| The phone cannot see `esp32-agent-setup` | The board still has old credentials → the serial log says `wifi: joining ...`; `POST /api/wifi/forget` then reboot |
| Joined the access point but no page | It must be `http://192.168.4.1:80/` — note the port; the captive DNS only makes HTTP pop up automatically |
| Credentials entered but it will not connect | 2.4 GHz and WPA2 are what works (the ESP32 has no 5 GHz radio); watch `wifi: status=…` in the log; check for trailing spaces in a pasted password |
| Connected but no IP found | Look for hostname `esp32-agent` in the router's DHCP list, or the `ip=` line in the serial log |

## A device is invisible

1. `POST /api/devices/probe`, or from the model call `devices_probe` then `devices_list`.
2. `bus i2c_scan` to see whether the address answers at all:
   - Nothing at all → wiring, pull-ups or power; are SDA/SCL swapped; is a 5 V module
     driving a 3.3 V logic board.
   - An address appears but is `unidentified` → `i2c_identify {addr}` for candidates, or
     read the ID register yourself with `bus i2c_read`.
3. Right address, wrong driver: address collisions are common (0x76/0x77, 0x3C/0x3D,
   0x44/0x45). The log carries the reason the driver gave.
4. A 1-Wire device never appears: 1-Wire is probed only on the pins listed in
   `bus.onewire_pins`, and it needs a 4.7 kΩ pull-up.

## A tool is missing from `tools/list`

- `GET /api/tools`: each entry has `visible`, and when it is not offered, `why` — the
  sentence, e.g. `no camera on this board` — plus `missing` for a registry tool whose
  capability list is not met.
- `/api/status` reports `mcp.tools_visible` (what the model actually receives, including
  `shell`, `script` and the file tools) and `mcp.tools_hidden` as `[{name, why}]`. A
  missing tool that is not listed there is a bug, not a setting.
- Did the user disable it? `mcp.disabled_tools`.
- With `tools_mode = "generic"` the device tools are collapsed into `device_invoke`; switch
  back to `auto` if you want them individually.
- Manifest tools: check that `enabled` is not `false` and the name is legal (`a-z A-Z
  0-9 _ -`, no `.`, at most 48 characters — the wire charset, see `docs/manifest.md`).

## An `https://` model endpoint fails

- `the TLS handshake with … failed` with a certificate error: the board's clock is far
  from real time, so the certificate looks expired or not-yet-valid. `wifi_status` says
  whether SNTP has synced; it happens within a minute of joining a network.
- `ESP_ERR_ESP_TLS_CANNOT_RESOLVE_HOSTNAME`: DNS, not TLS. Try the LAN address, or give
  `net_get` the same host to see whether name resolution is the problem.
- A private CA or a self-signed endpoint: the chain is verified against the ESP-IDF
  certificate bundle and there is no skip-verification switch, by design. Put the CA in
  the image instead of disabling the check.
- Out of memory during the handshake: a TLS session costs roughly 40 KB and the agent
  thread has 16 KB of stack plus the heap. Lower `log.ring`, or set `tools_mode` to
  `generic`, and run one turn at a time (which the queue already enforces).

## The portal opens over http but not https

`GET /api/tls` is the first thing to read: `{enabled, port, fingerprint, error}` says
whether a certificate exists and why not.

- `error: "crt_write_pem failed (mbedtls error -0x002e)"` — `MBEDTLS_ERR_OID_NOT_FOUND`:
  the certificate writer was asked to sign with "no hash". `mbedtls_x509write_crt_set_md_alg`
  fixes it; it is a missing line, not a missing mbedTLS feature, which is why the message
  reads like one.
- `error: "esp-aes: Failed to allocate memory"` on every connection: hardware AES-DMA wants
  a DMA-capable buffer in internal RAM, and with ~15 kB free the first TLS record dies with
  `esp-tls-mbedtls: write error :-0x0001`. The handshake succeeds, which makes it look like
  a certificate problem. `CONFIG_MBEDTLS_HARDWARE_AES=n` trades a few milliseconds per
  kilobyte for the memory.
- `could not spawn tls thread` in the log, `ERR_CONNECTION_RESET` in the browser: a thread
  per TLS connection needs its stack *before* the handshake starts, and a browser opens six
  sockets for one page. The listener now does the handshake inline on its one thread, and
  only streaming responses get a thread.
- `agent thread did not start: Not enough space` at boot: the TLS listener was given its own
  thread. It now shares the plaintext accept thread; adding a second one costs the agent,
  which costs the whole board.
- The browser warns `ERR_CERT_AUTHORITY_INVALID`: **expected**. The certificate is
  self-signed. Compare the fingerprint it shows with `/api/tls` and click through —
  a warning that can be verified is the point of generating the key on the device.
- `internal_kb` drops by about 11 kB the first time a TLS page is served and then stays
  put: 17 kB after boot becomes 5-6 kB, and twenty more page loads leave it there. Budget
  for that when raising `log.ring` or adding a listener; the agent still runs in what is
  left, but the margin is thin.
- The connection is served one at a time: a page that opens six sockets loads, just not in
  parallel. If a page seems to hang, check whether an event stream (`/api/agent/stream`) is
  open — streams run on their own thread, so this should not block, but that is the only
  long-lived response on this server.

## Applying a manifest fails

The returned `issues[].path` is a JSON Pointer, so it points straight at the problem:
- `/tools/0/exec/pin` → pin out of range
- `/tools/2/name` → illegal or duplicate name
- `/devices/1/driver` → driver id not in the table in `docs/capabilities.md`
- a `sequence` total-duration complaint → reduce `ms` or the number of steps

## An out-of-range call is refused

That is by design: `safety.min/max` and `params.min/max` are both checked on the board, and
the message carries the range, e.g. `` `ms` = 90000 is outside 10..=5000 ms ``. Change the
argument, not the firmware.

## The portal will not open

Three things produce the same symptom — a browser showing nothing — and the board's log
tells them apart:

- `***ERROR*** A stack overflow in task pthread` → a handler outgrew its stack. `/api/tools`
  serialising every tool descriptor is the heavy one; the accept thread runs with 16 kB for
  that reason.
- `could not spawn connection thread` → internal RAM, not PSRAM. Thread stacks come from
  internal SRAM; check the `mem:` lines the firmware logs at boot (`boot`, `after devices`,
  `after wifi`) and lower `log.ring` or the device count if `after wifi` is under 30 kB.
- Connections reset with no log line at all → the socket pool. Every closed connection sits
  in TIME_WAIT for 2x TCP MSL; `CONFIG_LWIP_MAX_SOCKETS=20` and a 5 s MSL keep the pool
  usable after repeated page loads.

`curl -sS -m 5 http://<board>:80/api/tools | head -c 200` is the fastest check: it is the
heaviest endpoint the portal calls, so if it answers, the page will load.

## The panel stays dark or shows garbage

- Confusing 128x32 with 128x64 garbles it: set `pins["oled.height"]`, or `options.height` in
  the manifest.
- SH1106 clones: `options.controller = "sh1106"` (column offset 2).
- Pure noise: `oled_contrast 0xCF` first, then check power and the 4.7 kΩ pull-ups.

## The microphone reads one level and never moves

- A line that sits at full scale (peak within a few counts of 2^31, dBFS around -3) is not
  audio: nothing is driving the data pin. Check VDD/GND, `L/R` strapped to GND for the left
  channel, and the data pin against `config.pins["mic.din"]`.
- Before blaming the wiring, check the slot alignment. `I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG`
  sets `bit_shift = true` and `left_align = true`; a zeroed struct right-aligns the sample
  and drops the Philips shift, which on an INMP441 reads as a stuck full-scale line. Both
  flags are set explicitly for that reason.
- A quiet room on this bench reads about -65 dBFS after the alignment is right; a 1 kHz tone
  from the amplifier puts it around -25 dBFS.

`mic_record` returns a **WAV** (`format: "wav"`, `wav_b64`: a 44-byte RIFF header over the same
16-bit mono PCM), because that is the format this board can produce honestly. There is no Opus
encoder and there will not be one: [Audio](audio.md) has the flash and RAM arithmetic that
says why, and what would have to change for that to stop being true.

## A tone keeps playing after the caller is gone

- The I2S DMA descriptor is replayed, so a `speaker_tone` whose HTTP connection drops mid-way
  loops that buffer forever and the amplifier keeps sounding. The TX channel now sets
  `auto_clear_after_cb`, which sends silence once the written buffer has gone out.
- If a board on older firmware is stuck this way, `speaker_play` with a block of zeros
  (1024 samples = 2048 bytes, the per-call limit) replaces the buffer.
- Keep `volume` modest: it is a percentage of full scale, and 90 against a 3 W amplifier at
  desk distance is genuinely loud.

## The microphone and the speaker vanish after `manifest_apply`

- Every apply rebuilds the device list, so `Hardware::i2s_setup` runs again on channels that
  are already open. `i2s_new_channel` refuses a port in use, and one failure there drops both
  audio devices until the next reboot. The firmware now leaves an open channel alone, and
  `devices_list` keeps `mic` and `speaker` across an apply.

## How much conversation the board can carry

`MAX_HISTORY_BYTES` bounds the transcript. It started at 24 KB, which was a guess made
before anything was measured; it is 512 KB today. Two measurements got it there, and the
second one stopped short of where the asking went.

**First ladder, against a real model.** One long reply per turn, `/api/status` read between
turns:

| Turn | Messages | History | Request body (history + ~18 KB of tool schemas) | Internal SRAM | Result |
|---|---|---|---|---|---|
| 3 | 5 | 4.7 KB | ~23 KB | 6 kB | ok |
| 9 | 17 | 35.3 KB | ~53 KB | 6 kB | ok |
| 12 | 23 | 59.8 KB | ~78 KB | 6 kB | ok |
| 13 | 21 | 63.5 KB | ~82 KB | 6 kB | compacted, then ok |

**Second ladder, against a local stub** (a stand-in chat endpoint on the LAN, so the run cost
no tokens; same transport, same serialisation, 8 KB replies):

| Turn | Messages | History | PSRAM free | Internal SRAM | Result |
|---|---|---|---|---|---|
| 60 | 119 | 249 KB | 7 111 kB | 6 kB | ok |
| 120 | 240 | 507 KB | 6 593 kB | 6 kB | ok |
| ~180 | — | — | — | — | **board rebooted**, cause not captured |

What that says:

1. **Internal SRAM never moves** (6 kB free throughout): transcript, JSON body and mbedTLS
   buffers are all in PSRAM, so a bigger transcript costs the resource that does not run out.
2. **Each turn costs about four times its text.** PSRAM fell ~95 kB per ten 4 KB turns: the
   message, a copy in the event journal, and the serialised request all exist at once.
3. **The 48-message cap was the first thing to bind**, not the byte cap: at 2 messages a turn
   it stopped at ~101 KB. Both caps have to move together, which is why
   `MAX_HISTORY_MESSAGES` is 256 now.
4. **1 MB has not been proved.** The board survived 507 KB and rebooted somewhere below 1 MB.
   The panic was not captured because nothing was holding the serial port. That capture is the
   next step, and until it exists 512 KB is the highest figure with evidence behind it.

The one failure in the first ladder was not memory: it was
`reading the reply over TLS failed: ERROR (the session times out after 45 s)` on a deliberately
long reply. Raise `llm.timeout_s` before raising the history cap.

## Performance and stability

| Symptom | Cause / fix |
|---|---|
| Connections refused (503) | The concurrent connection limit is 6; close stale browser tabs |
| A tool call takes > 500 ms | The log warns; the I2C device is slow, or it is a 1-Wire conversion (DS18B20 is fixed at 750 ms) |
| Out of memory | Lower `log.ring`, reduce concurrency, switch `tools_mode` to `generic` |
| Sessions pile up | `session_ttl_s` (900 s default) and `max_sessions` (4 default) reclaim them |
| `could not spawn connection thread` | Internal RAM exhausted (PSRAM cannot back a stack); close streams or lower the connection limit |

## The simulator

- `cargo run -p esp32-agent-sim -- --port 18080 --state sim-state/kv.json`
- `--empty`: a bare board (to verify that the tool list is empty with no hardware).
- To start over: delete `sim-state/kv.json`.

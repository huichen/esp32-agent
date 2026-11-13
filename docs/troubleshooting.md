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
- `internal_kb` used to drop by about 11 kB the first time a TLS page was served: 17 kB
  after boot became 5–6 kB, and twenty more page loads left it there. **It does not any
  more, and the old figures are not a budget.** Measured on the bench (N16R8, IDF v5.5.2,
  `internal_kb` read before and after four https requests): 161 kB before, 161 kB after.
  The two changes behind that are in `firmware/sdkconfig.defaults` —
  `CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC=y` keeps the session in PSRAM, and the 4 KB record
  buffers (`CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN` / `..._OUT_...`) replace the 16 KB default.
  The 5–6 kB reading dates from before commit `10b16dd`, whose message records the same
  numbers ("8 kB of internal heap, 6 kB largest block") as the reason it lowered
  `SPIRAM_MALLOC_ALWAYSINTERNAL`; see [how much memory the board has](#how-much-memory-the-board-has)
  for the current figures and how to read them.
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
  internal SRAM, and a stack needs one **contiguous** block: read `memory.largest_internal_kb`
  in `/api/status` (the boot log's `mem:` lines report the same pair at `boot`, `after
  devices` and `after wifi`) and lower `log.ring` or the device count when the largest free
  block is smaller than the 6 kB a new connection thread asks for.
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

## How much memory the board has

Four readings, and they answer different questions. `GET /api/memory` splits the heap by
capability; `/api/status` carries `memory` (the one number that decides whether a socket or
a handshake fits) and `threads` (each task and the stack it has left); `shell free` prints
the total, which includes PSRAM and can look healthy while the internal pool is empty.

Measured on the bench (ESP32-S3-DevKitC-1 N16R8, IDF v5.5.2, commit `93b0579`) after a boot,
a portal load, `/api/tools`, two scans, two https requests and ten minutes of uptime:

| Pool | Total | Free | Largest free block | What it is |
|---|---|---|---|---|
| `internal_8bit` | 370 kB | 168 kB | 98 kB | thread stacks, queues, anything that must work with the cache off |
| `dma` | 363 kB | 161 kB | 98 kB | descriptors and buffers a peripheral reads or writes |
| `spiram` | 8165 kB | 7901 kB | 7808 kB | PSRAM: allocation only, no stacks, no DMA descriptors |

The pools overlap on purpose: internal RAM is the DMA pool plus more, and PSRAM is a region
of its own. `memory.free_heap_kb` in `/api/status` is the sum of them, which is how "8 MB of
free heap" and "a thread stack did not fit" were both true at once.

That sum is also why `board status` now reports `internal_kb` and `largest_internal_kb` next
to `free_heap_kb`, and why `shell free` lists one line per pool. Asked "how much RAM is
free", a model read the only number there was — 7 864 kB, nearly all of it PSRAM — and
answered "7 919 KB of free internal heap". Both figures are named now, so the answer a model
gives and the answer `/api/memory` gives are the same one.

`/api/status.threads`, same run. The name is the FreeRTOS task name, which is what `ps`
prints — it is set by `firmware/src/threads.rs`, not by `std::thread::Builder::name` (that
one only sets the pthread name, which nothing here reads):

| Task in `ps` | Source | Stack | Free at its worst point | Peak use |
|---|---|---|---|---|
| `mcp-main` | `main.rs` | 20 kB | 13.5 kB | 6.5 kB |
| `pthread` (the http listener) | `ACCEPT_STACK`, `http.rs` | 16 kB | 10.2 kB | 5.8 kB |
| `mcp-agent` | `STACK_BYTES`, `runner.rs` | 16 kB | 8.3 kB | 7.7 kB |
| `radio` | `wifi.rs` | 12 kB | 6.5 kB | 5.5 kB |
| `provision` | `console.rs` | 6 kB | 4.7 kB | 1.3 kB |
| `dns` | `dns.rs` | 4 kB **in PSRAM** | 2.1 kB | 1.9 kB |
| `store-writer` | `nvs_writer.rs` | 6 kB | 4.7 kB | 1.3 kB |

A name is a fact about one thread and is never inherited. `esp_pthread_set_cfg` is read by
`pthread_create` from the **creating** thread, so the name, the size and the stack
capabilities are one decision made on the wrong side of the call: a config left behind
becomes the next thread's name and priority. That happened — the store writer set
`nvs-writer` at priority 2, and the console, radio and agent threads came up under it, so
`ps` showed four `nvs-writer`s and three were the wrong task at the wrong priority. Every
spawn now goes through `threads::spawn`, which sets its own name and capabilities and
restores the defaults (with `inherit_cfg = false`) afterwards. The one thread that still
prints as `pthread` is the HTTP listener: it is spawned in `agent-core/src/http.rs`, which
also builds for the host and cannot know about FreeRTOS task names.

74 kB of stack, about 29 kB of it ever touched — the largest single block of internal RAM
the firmware reserves, and the reason these constants are worth measuring rather than
guessing. IDF's own tasks (`main`, `esp_timer`, `wifi`, `tiT`, `Tmr Svc`, `sys_evt`, `ipc0`,
`ipc1`, two `IDLE`) add roughly 26 kB and are not ours to size.

The DNS responder is the only stack that could move, and it is in PSRAM: its whole body is
`recvfrom`, an A-record reply and `send_to`, so it can never run while the flash cache is
off. That is the condition — with the cache off, external RAM is not addressable, and a
thread that commits to NVS from a PSRAM stack asserts in
`esp_task_stack_is_sane_cache_disabled` and reboots the board. Every other thread here
writes NVS somewhere (`mcp-main` on a config save, the http listener on a manifest apply,
the agent on its transcript, the console on `provision`), which is why their stacks stay
internal; `esp_pthread_set_cfg` is set for exactly one thread and restored immediately,
because the listener is spawned from the same thread later.

**The rule a stack is judged by: `target >= max(2 x peak, peak + 4 kB)`.** Peak use is the
size in the source minus `high_water_kb`; FreeRTOS reports what is left of a stack, never
how big it is, so the size stays in the source. A stack that is too small asserts and
reboots, it does not slow down — which is why the readings are taken after the heavy paths
(`/api/tools`, the portal, a TLS handshake) have run, and why they are re-read after a
change: the WiFi thread read 1.4 kB of peak use on one boot and 5.5 kB on the next, so its
12 kB stays. The 4 kB DNS stack is the one entry that does not clear the rule (2.1 kB peak
needs 5.9 kB by it); it is a fixed `recvfrom` loop, and raising it would spend 2 kB of the
pool everything else allocates from, so it is a decision, not an oversight.

`SPIRAM_MALLOC_RESERVE_INTERNAL` (32 kB) is carved out of the internal pool for allocations
that *must* be internal. Measured, it is not what binds: the I²S transmit ring is created at
boot, a 400 ms tone plays with the `dma` pool still at 160 kB free, and lowering the reserve
would not add capacity — it would only let `malloc` reach into memory held for DMA.

What got the board here (each change one commit, each measured on the board):
the 4 KB sine table moved from `.data` to flash (`refactor(audio)`), `mcp-main` 32 → 20 kB
and the http listener 20 → 16 kB (`refactor(threads)`), and 14.6 kB of lwIP/net80211/pp/
wpa_supplicant `.bss` into PSRAM (`CONFIG_SPIRAM_ALLOW_BSS_SEG_EXTERNAL_MEMORY`,
`refactor(spiram)`). The `.bss` move is the one to be careful with: it touches the radio's
own state, and the failure it is proved against is WiFi init refusing to allocate its 16 rx
buffers. Ten minutes, a scan and an https request, with `internal_8bit` free unchanged
throughout, is what it took.

## How much conversation the board can carry

`MAX_HISTORY_BYTES` bounds the transcript. It started at 24 KB, which was a guess made
before anything was measured; it is 2 MB today, with `MAX_HISTORY_MESSAGES` raised to
1,024 alongside it. The two ladders below are what the board has actually been through:
the top rung they reach is 507 KB. **2 MB is a ceiling chosen ahead of the evidence**, not
a size that has been exercised — the arithmetic behind it is item 4.

**First ladder, against a real model.** One long reply per turn, `/api/status` read between
turns. Both ladders are historical in one respect: they were measured before commit
`10b16dd` lowered `SPIRAM_MALLOC_ALWAYSINTERNAL`, so the "Internal SRAM" column is that
firmware's 6 kB, not the 168 kB this one reports — the per-turn cost in PSRAM is what they
are about, and the byte ceiling has since been raised above them by hand.

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

What that says:

1. **Internal SRAM never moved in that firmware** (6 kB free throughout; today it is 168 kB
   and the figure to watch is in [how much memory the board has](#how-much-memory-the-board-has)):
   transcript, JSON body and mbedTLS buffers are all in PSRAM, so a bigger transcript costs
   the resource that does not run out.
2. **Each turn costs about four times its text.** PSRAM fell ~95 kB per ten 4 KB turns: the
   message, a copy in the event journal, and the serialised request all exist at once.
3. **The 48-message cap was the first thing to bind**, not the byte cap: at 2 messages a turn
   it stopped at ~101 KB. Both caps have to move together, which is why
   `MAX_HISTORY_MESSAGES` is 1,024 now.
4. **The 2 MB ceiling is arithmetic, not a measurement.** During a turn the transcript, its
   journal entry and the serialised request are live at once, so the peak is roughly twice
   the cap plus the 18 KB of tool schemas — about 4 MB against 7.9 MB free. What to read at a
   size larger than 507 KB is `spiram.free_kb` in `/api/memory` between turns and
   `largest_kb` inside one; a board that runs out does it as a failed allocation or a
   watchdog reset, not as a slower turn.

The one failure in the first ladder was not memory, and the message it printed named the
wrong cause: `reading the reply over TLS failed: ERROR (the session times out after 45 s)` on
a deliberately long reply. `ERROR` was `esp_err_to_name` of `-0x7200`, and `-0x7200` is
`MBEDTLS_ERR_SSL_INVALID_RECORD`: the reply arrived in a TLS record bigger than the 4 KB
inbound buffer this board was configured with, which mbedTLS rejects. It is fixed in
`firmware/sdkconfig.defaults` (16 KB inbound, the protocol maximum) — raising `timeout_s`
never had anything to do with it.

## A store write was refused

`/api/status.store.failed` counts operations the backing store refused after a caller was
told the change was accepted. It is not a warning: the value is in RAM and not on flash, so
the next boot reads the one before it. Measured on this bench, firmware `17a9358`, 195 s
after boot: `{"written": 4, "failed": 23}` — 23 changes the board believed it had saved.
Those counters restart at every boot and belong to the image that ran them: the capture
below came from `17a9358`, and the board has been reflashed to `f7bea4b` and then
`a47d0b1` since, neither of which carries this fix. A number in a report is only worth
something next to the `firmware.commit` that produced it.

The count is not the diagnosis. Two fields beside it say which key is in trouble and why, so
a board with no serial terminal attached can still be read:

| Field | What it says |
|---|---|
| `store.failed` | How many operations were refused since boot (unchanged) |
| `store.last_failure` | The newest refusal: `{key, op, err, err_name, at_ms}` — `op` is `set` or `remove`, `err` is the `esp_err_t` the board returned, `err_name` its `esp_err_to_name`, `at_ms` ms since the store queue was created (within a few ms of boot, so it lines up with a log line's `t_ms`). `null` before anything is refused |
| `store.refused_keys` | Refusals per key, most refused first, at most 16 keys |
| `store.refused_other` | Refusals for keys beyond those 16 (counted, so the map cannot grow without bound) |
| `store.space` | `{used_entries, free_entries, total_entries}` from `nvs_get_stats`, or `null` on a backend that cannot say |

The number and the name come from the same call, so they cannot drift apart. Each refusal is
also one `store` log line per key — key, operation, code, name and `at_ms` — written by the
`store-writer` task as it drains the queue, not one line for all of them: a count says
something is wrong, and only the key says what.

**The case that produced this section.** Every write of `agent.transcript`,
`agent.transcript.N` and `agent.run_in_flight` was refused, and the console showed why:

```
E (279355) esp32_agent::nvs_store: nvs write `agent.run_in_flight`: ESP_ERR_NVS_KEY_TOO_LONG
E (279357) esp32_agent::nvs_store: nvs write `agent.transcript`: ESP_ERR_NVS_KEY_TOO_LONG
E (279361) esp32_agent::nvs_store: nvs write `agent.transcript.1`: ESP_ERR_NVS_KEY_TOO_LONG
```

Not the partition: a key name is at most 15 bytes (`NVS_KEY_NAME_MAX_SIZE` is 16 with the
terminator, `nvs.h`), and those names are 16, 18 and 19. `ESP_ERR_NVS_KEY_TOO_LONG` is
`0x1109` = 4361. Nothing was ever stored, so the transcript and the run-in-flight marker
existed only in RAM: a power cut lost the conversation, and no reboot could ever find a
turn to resume. The names are now `transcript` (10), `transcript.N` (12) and
`run_in_flight` (13), and `StoreQueue` refuses an over-long key with that same code before
it reaches a backend, so the next one costs a log line instead of a flash round trip.

**Nothing to migrate, on purpose.** No build ever put a byte under `agent.transcript` or
`agent.run_in_flight` — NVS refused the value before writing any of it — so there is no old
value on flash, no migration step, and no code that reads the old names. If a later
investigation expects one, this paragraph is the answer. A simulator state file is the only
place an old name can still exist, and that is scratch state rather than the board's.

Every key name now lives in one table, `crates/agent-core/src/keys.rs`: the seven base
names plus `keys::chunk(base, index)`, which is the only place a chunked key is assembled
(both the workspace and the transcript go through it). `keys::all()` enumerates the shapes
that assembly can produce — indices `0..=99`, deliberately wider than any plan — and
`every_key_the_board_can_generate_fits_nvs` asserts each is within the ceiling. It fails on
a longer prefix (`agent.transcript` → red), on a longer separator (`::chunk` → red) and on
a wider index; `the_blob_plans_stay_inside_the_key_table` in `app.rs` keeps the two plans
inside what the table enumerated.

What the other codes mean, since they need different fixes:

| `err` | Name | What it means here |
|---|---|---|
| 4361 | `ESP_ERR_NVS_KEY_TOO_LONG` | The name is over 15 bytes; the value was never stored |
| 4357 | `ESP_ERR_NVS_NOT_ENOUGH_SPACE` | The partition is full: the entry would not fit |
| 4366 | `ESP_ERR_NVS_VALUE_TOO_LONG` | The value is over 4000 bytes — a chunking bug, not a full partition |
| 4354 | `ESP_ERR_NVS_NOT_FOUND` | A read asked for a key that is not there; a *removal* that finds nothing is not a failure |
| 4358 | `ESP_ERR_NVS_INVALID_NAME` | The namespace is over 15 bytes (`esp32mcp` is 8) |

Space is worth watching rather than guessing at: the `nvs` partition is `0x6000` = 24 KB
(`firmware/partitions.csv`), which is 6 pages of 126 entries of 32 bytes — 756 entries. A
3500-byte value takes 1 header entry plus 110 data entries, 111 of them. So the two blob
plans cannot both be full at once: `TRANSCRIPT_CHUNKS_MAX` 8 and `WORKSPACE_PLAN` 5 are
13 values of up to 3500 bytes, about 45 KB of values on a 24 KB partition. Whichever fills
first wins and the other is refused — visibly, with `ESP_ERR_NVS_NOT_ENOUGH_SPACE` and the
key named above — and `store.space` is the reading that says how close that is. What the
right split between the transcript and the file workspace is has not been measured; that is
in `ROADMAP.md`, not here.

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

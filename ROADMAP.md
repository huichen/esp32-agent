# Roadmap

What already works is documented in the README and under `docs/`; this file lists only
what is not done or not verified.

## Verified / unverified

- ✅ `agent-core`: 839 unit tests and 5 integration tests (protocol, HTTP, manifest
  validation, driver conversion, capability filtering, auth, hot reload, the agent loop,
  shell and script, the file tools, sched, SSE streams, a streamed request body)
- ✅ Simulator end to end: the PowerShell smoke gate; `scripts/smoke.sh` is the Unix
  counterpart, whose runtime must be checked on a Unix host
- ✅ Historical firmware bench evidence through 2026-11-12 (ESP32-S3-DevKitC-1 N16R8,
  ESP-IDF v5.5.2): boots without a reset loop, the I2C bus works, setup AP and station mode
  coexist, `/` and `/mcp` answer, `tools/list` is stable across five consecutive calls, and
  the 8 MiB PSRAM passes its memory test. Earlier boot logs enumerated the then-current
  seven-partition layout; the source now has four entries: `nvs`, `phy_init`, `factory`
  and `storage`
- ✅ Historical on-board agent loop: `board_status`, the file tools (`write` then `read`
  then `grep`) and `net_get` (against a real external host) ran through the model loop;
  a 45 s scheduled task fired twice on its own and a saved history survived a reboot.
  This does not verify arbitrary power cuts or the audit changes
- ✅ Model latency is outside the `App` lock: a turn in flight no longer queues the
  portal (historical bench: `/api/status` ≤ 253 ms while a 2.5 s model call runs)
- ✅ Live journal over SSE (`/api/agent/stream`), `tool_progress`, `agent.tick_ms`
- ✅ Audit firmware release compiled and linked at runtime revision `6e78043` with
  ESP-IDF v5.5.2 (2026-11-13); 35 warnings reported, 20 duplicates. This is a build check,
  not hardware validation. The [audit report](docs/audit-2026-11.md) records hashes and
  the 1,089-check live HTTP/MCP/internal-agent run
- ⚠️ Audit firmware has not been flashed or board-tested. Earlier flashes do not validate
  subsequent source changes
- ⚠️ Not measured: WS2812/RMT, ADC/PWM values, boot time and power draw under a full
  sdkconfig; the portal's live console has only been driven with `curl -N`, not in a
  browser
- ⚠️ Storage/runtime validation remains: transcript and workspace already use LittleFS on
  `storage` (`0xbf0000` = 12,517,376 bytes = 11.9375 MiB); configuration and secrets remain
  in 24 KiB NVS. Host tests cover bounded parsing/admission and refused restores. Ordinary
  configuration and file-tool replies acknowledge queue admission, not storage completion;
  workspace saved hashes advance only from completed publication receipts. Neither is a
  board power-cut measurement. See [storage guarantees](docs/storage.md)
- ⚠️ Historical media bench (2026-11-12, recorded in `e7a9623`): `POST /audio/pcm` and `POST /av`
  played on the board; a 30.00 s clip
  with 362 frames completed in 30.19 s with the sound judged correct by ear, and its audio
  alone in 30.03 s. What the 128×32 panel *shows* of a dithered video has not been judged
  yet — the picture is dense enough that "noisy content" and "a wrong panel setup" look
  alike — and the display dropped off the I²C scan once during those runs, returning after
  a power cycle

## Near term

1. **mDNS**: register `_mcp._tcp` so a client does not have to know the IP first.
2. **Workspace durability on the board**: the LittleFS per-file workspace is implemented;
   verify refusal under ENOSPC, queued dependency ordering, recovery and interrupted writes
   on real flash before claiming power-loss durability.
3. **Inbound TLS without the certificate the board made**: the board serves 443 itself with
   a self-signed certificate, so every client has to be taught the fingerprint. A
   provisioned certificate (a real CA for a name the LAN resolves) is the missing piece,
   not the listener.
4. **Manifest snapshots**: keep the previous version before each apply so a bad manifest
   is one click to undo.
5. **SSE on `/mcp`**: the long-lived `GET /mcp` stream (currently 405, which the spec
   allows).

## Medium term

6. **Audio beyond the current bounded calls**: change the I2S TX rate at runtime (only
   24000 Hz is wired), permit overlapping uploads, add long microphone file recording or
   background 60 s score playback. None is implemented. On-board Opus/ADPCM encoding is
   deferred pending an actual build and measurements; the old 5–6 kB SRAM reading is not
   evidence that a codec cannot fit. See [audio](docs/audio.md).
7. **A usable firmware camera capture path.** OV2640/OV3660 probing and camera host tests
   exist. `camera_capture` exposes metadata and a 160×120 luma thumbnail, not a full JPEG
   download. ESP32 DVP capture still uses the unsupported hardware default, so neither
   real capture nor 1080p operation is verified. DMA acquisition, lock duration and a
   binary frame-download route need implementation and board evidence.
8. **Manifest template library**: grow `examples/manifests/` into a set of device recipes
   that the console can import in one click.
9. **Capability registry**: make the table in `docs/capabilities.md` the single source a
   `scripts/` check validates (the repository ships without CI).
10. **Multi-bus / multi-device UI**: a second device of the same kind gets a `_2` suffix
    today, and the console cannot tell them apart.

## Long term

11. **Zero-code extension**: expressions in manifests (`scale`, `map`, `threshold`) for
    conversion, to remove more of the need for drivers.
12. **Resource subscriptions**: push sensor changes to clients (`resources/subscribe`).
13. **Security**: on-device OAuth (at least a token + rate limits + an allowlist for
    dangerous actions).
14. **Community**: extend the existing bug/new-device issue templates for driver and
    protocol reports; package `examples/manifests/` into a demo board every quarter.

SD raw-sector and DHT11/22 drivers already exist and have host tests. A FAT filesystem on
SD and board validation of those drivers remain unimplemented/unverified, respectively.

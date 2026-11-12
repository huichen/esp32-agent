# Roadmap

What already works is documented in the README and under `docs/`; this file lists only
what is not done or not verified.

## Verified / unverified

- ✅ `agent-core`: 731 unit tests and 4 integration tests (protocol, HTTP, manifest
  validation, driver conversion, capability filtering, auth, hot reload, the agent loop,
  shell and script, the file tools, sched, SSE streams, a streamed request body)
- ✅ Simulator end to end: `scripts/smoke.ps1` and `scripts/smoke.sh`
- ✅ Firmware runs on real hardware (ESP32-S3-DevKitC-1 N16R8, ESP-IDF v5.5.2):
  boots without a reset loop, the I2C bus works, setup AP and station mode coexist, `/`
  and `/mcp` answer, `tools/list` is stable across five consecutive calls, the 8 MB PSRAM
  passes its memory test, and the repository's seven-partition table ships with the image
  and enumerates at runtime
- ✅ On-board agent loop: `board_status`, the file tools (`write` then `read` then `grep`)
  and `net_get` (against a real external host) have all run through the model loop on the
  board; a 45 s scheduled task fired twice on its own; history survives a reboot
- ✅ Model latency is outside the `App` lock: a turn in flight no longer queues the
  portal (measured on the board: `/api/status` ≤ 253 ms while a 2.5 s model call runs)
- ✅ Live journal over SSE (`/api/agent/stream`), `tool_progress`, `agent.tick_ms`
- ⚠️ Not measured: WS2812/RMT, ADC/PWM values, boot time and power draw under a full
  sdkconfig; the portal's live console has only been driven with `curl -N`, not in a
  browser
- ⚠️ `POST /audio/pcm` and `POST /av` have been played on the board since: a 30.00 s clip
  with 362 frames completed in 30.19 s with the sound judged correct by ear, and its audio
  alone in 30.03 s. What the 128×32 panel *shows* of a dithered video has not been judged
  yet — the picture is dense enough that "noisy content" and "a wrong panel setup" look
  alike — and the display dropped off the I²C scan once during those runs, returning after
  a power cycle

## Near term

1. **OTA**: firmware upload over `/api/ota` with rollback between the two app slots; the
   partition table is ready (`otadata`/`ota_0`/`ota_1` are in the image).
2. **mDNS**: register `_mcp._tcp` so a client does not have to know the IP first.
3. **File workspace on SPIFFS** (the 1 MB `storage` partition); today the workspace is a
   single 8 KB NVS blob.
4. **Inbound TLS without the certificate the board made**: the board serves 443 itself with
   a self-signed certificate, so every client has to be taught the fingerprint. A
   provisioned certificate (a real CA for a name the LAN resolves) is the missing piece,
   not the listener.
5. **Manifest snapshots**: keep the previous version before each apply so a bad manifest
   is one click to undo.
6. **SSE on `/mcp`**: the long-lived `GET /mcp` stream (currently 405, which the spec
   allows).

## Medium term

7. **Audio streaming, the two things `/audio/pcm` does not do**: change the I2S TX rate at
   runtime (the clock is a constant, so only 24000 Hz is accepted) and hold two uploads at
   once. Both need a second thread or a reopenable channel, and internal RAM is the
   constraint — `docs/audio.md` §6 has the arithmetic.
8. **A camera frame that does not hold a lock.** `camera_capture` returns a frame as
   base64 through the same tool result a sensor reading uses, which is why the frame has to
   be small. A mode and frame-size negotiation over SCCB plus DMA frame buffers in PSRAM —
   writing the frame to the workspace, or streaming it — is a feature, not a driver, and it
   should land after the storage question (3) is answered.
9. **Manifest template library**: grow `examples/manifests/` into a set of device recipes
   that the console can import in one click.
10. **Capability registry**: make the table in `docs/capabilities.md` the single source a
    `scripts/` check validates (the repository ships without CI).
11. **Multi-bus / multi-device UI**: a second device of the same kind gets a `_2` suffix
    today, and the console cannot tell them apart.

## Long term

12. **Zero-code extension**: expressions in manifests (`scale`, `map`, `threshold`) for
    conversion, to remove more of the need for drivers.
13. **Resource subscriptions**: push sensor changes to clients (`resources/subscribe`).
14. **Security**: on-device OAuth (at least a token + rate limits + an allowlist for
    dangerous actions).
15. **Community**: issue templates split by "new device / new driver / protocol bug";
    package `examples/manifests/` into a demo board every quarter.

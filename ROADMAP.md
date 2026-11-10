# Roadmap

What already works is documented in the README and under `docs/`; this file lists only
what is not done or not verified.

## Verified / unverified

- ✅ `agent-core`: 218 tests (protocol, HTTP, manifest validation, driver conversion,
  capability filtering, auth, hot reload, the agent loop, fs/net/sched, SSE streams, the
  goal loop)
- ✅ Simulator end to end: `scripts/smoke.ps1`
- ✅ Firmware runs on real hardware (ESP32-S3-DevKitC-1 N16R8, ESP-IDF v5.5.2):
  boots without a reset loop, the I2C bus works, setup AP and station mode coexist, `/`
  and `/mcp` answer, `tools/list` is stable across five consecutive calls, the 8 MB PSRAM
  passes its memory test, and the repository's seven-partition table ships with the image
  and enumerates at runtime
- ✅ On-board agent loop: `board_status`, `fs_write→fs_read→fs_grep` and `net_get`
  (against a real external host) have all run through the model loop on the board; a 45 s
  scheduled task fired twice on its own; history survives a reboot
- ✅ Model latency is outside the `App` lock: a turn in flight no longer queues the
  portal (measured on the board: `/api/status` ≤ 253 ms while a 2.5 s model call runs)
- ✅ Live journal over SSE (`/api/agent/stream`), `tool_progress`, `agent.tick_ms`
- ✅ Goal loop (`/api/goal`, `docs/agent-goal.md`): on the board, a 3-iteration goal ran
  itself to its cap with a real note per iteration, a second goal ran to iteration 7 in
  five seconds and froze there when stopped, and the objective plus notes survived a
  reboot. `scripts/goal-smoke.ps1` covers the same path in the simulator, including the
  409 on a second start and the abort after three consecutive failures
- ⚠️ Not measured: WS2812/RMT, ADC/PWM values, `/api/ota`, boot time and power draw under
  a full sdkconfig; the portal's live console has only been driven with `curl -N`, not in
  a browser

## Near term

1. **OTA**: firmware upload over `/api/ota` with rollback between the two app slots; the
   partition table is ready (`otadata`/`ota_0`/`ota_1` are in the image).
2. **mDNS**: register `_mcp._tcp` so a client does not have to know the IP first.
3. **File workspace on SPIFFS** (the 1 MB `storage` partition); today the workspace is a
   single 8 KB NVS blob.
4. **TLS**: `https://` for model calls and `net_*` (currently refused with a clear error).
5. **Manifest snapshots**: keep the previous version before each apply so a bad manifest
   is one click to undo.
6. **SSE on `/mcp`**: the long-lived `GET /mcp` stream (currently 405, which the spec
   allows).

## Medium term

7. **I2S audio**: INMP441 microphone + MAX98357A amplifier → `mic_level`, `tone`,
   `record`.
8. **More drivers**: DHT11/22, RC522, VL53L0X, BH1750, BME680, SD card, stepper motor.
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
    dangerous actions), and TLS on the LAN.
15. **Community**: issue templates split by "new device / new driver / protocol bug";
    package `examples/manifests/` into a demo board every quarter.

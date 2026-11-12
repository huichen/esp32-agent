# Roadmap

What already works is documented in the README and under `docs/`; this file lists only
what is not done or not verified.

## Verified / unverified

- ✅ `agent-core`: 222 tests (protocol, HTTP, manifest validation, driver conversion,
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
4. **Inbound TLS**: the portal and `/mcp` on 8080 are HTTP and a reverse proxy terminates
   TLS today (`SECURITY.md`); serving them from the board itself needs a certificate
   story (per-device self-signed plus pinning, or a provisioned cert in NVS).
5. **Manifest snapshots**: keep the previous version before each apply so a bad manifest
   is one click to undo.
6. **SSE on `/mcp`**: the long-lived `GET /mcp` stream (currently 405, which the spec
   allows).

## Medium term

7. ~~**I2S audio**: INMP441 microphone + MAX98357A amplifier → `mic_level`, `speaker_tone`,
   `mic_record`.~~ Done and measured on the bench: 16 kHz/32-bit/left mic, 24 kHz/16-bit
   amplifier, -65 dBFS floor and a 1 kHz tone at -25 dBFS through the microphone.
8. **More drivers.** The catalogue in `drivers/mod.rs` marks what is planned and what is
   not; these are the ones worth writing next, ordered by how often they appear on an
   ESP32 bench, with the upstream component that already exists for each so nobody starts
   from a datasheet alone:

   | Driver | Bus | Probe | Upstream reference |
   |---|---|---|---|
   | Camera OV2640 / OV3660 | DVP + I2C (SCCB) | SCCB id register 0x0A/0x0B | `esp32-camera` (registry), `esp_video` |
   | BME680 (T/RH/P/gas) | I2C 0x76/0x77 | chip id 0x61 | `bme680`, `esp_bme680` |
   | AHT20 / AHT10 | I2C 0x38 | status word after a convert | `aht20`, `esp_ahtxx` |
   | BH1750 light | I2C 0x23/0x5C | one-byte command, 16-bit lux | `bh1750` in several ports |
   | VL53L0X / VL53L1X ToF | I2C 0x29 | model id 0xEE | `vl53l0x`, `vl53l1x_uld_esp_wrapper` |
   | HX711 load cell | 2 GPIO, bit-banged | presence by a stable 24-bit read | `hx711` |
   | MFRC522 RFID | SPI | version register 0x37 | `rc522` |
   | DS3231 RTC | I2C 0x68 | registers are BCD, no id — declare it | — |
   | Brushed motor L298N / TB6612 / DRV8833 | 2 PWM + 1 GPIO | not probeable, declare it | `bdc_motor`, `bdc_driver` |
   | Stepper A4988 / DRV8825 / TMC2208 | STEP/DIR + PWM | not probeable, declare it | `stepper_driver`, `tmc2208`, `soldered-drv8825-esp-idf-component` |
   | Rotary encoder | 2 GPIO | not probeable; needs a quadrature decoder | — |
   | PIR HC-SR501 | 1 GPIO | not probeable | — |
   | TM1637 7-segment | 2 GPIO | not I2C: its own clock/data protocol | — |
   | MAX7219 matrix | SPI | not probeable, declare it | — |
   | DHT11 / DHT22 | 1 GPIO, strict timing | not probeable | — |
   | SD card | SPI or SDMMC | mount attempt | `esp-idf` sdspi |

   A camera is the largest of these: it needs DMA frame buffers in PSRAM, a mode and
   frame-size negotiation over SCCB, and a result path that does not go through the 2 KB
   tool result limit — the frame has to land in the workspace or go out as a stream.
   That makes it a feature, not a driver, and it should land after the storage question
   (below) is answered.

   Two of the entries above are honest about not being probeable at all (motors, PIR,
   encoders). For those the manifest is the interface and the driver is only needed when
   timing or decoding demands it — a brushed motor over two PWM pins is a manifest today.
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

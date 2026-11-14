# Manifest specification v1

A manifest is a JSON document in the NVS `manifest` key. It decides which hardware this
board exposes, as which tools. Call `manifest` with `command: apply` to make it take
effect: `tools/list` changes immediately, without a reboot or a reflash.

The five manifest operations are one tool with five commands — `get`, `describe`,
`validate`, `apply`, `reset` — called as `{"command": "apply", "arguments": {...}}`.
`{"command": "help"}` lists them with their arguments in about a kilobyte, which is what
lets the board send one declaration instead of five. The name is `manifest` and nothing
else: one capability has one name, so the five old ones (`manifest_apply` and the rest) are
gone rather than hidden, and a stored script that used them gets `no tool` and a pointer to
the family.

## 0. Manifest or driver?

They are not alternatives to each other; they answer different questions.

| | Manifest | Driver |
|---|---|---|
| Question | "this board has X wired to pin 12 — what may a caller do with it?" | "how does a chip of type X actually work?" |
| Form | JSON in NVS, edited at runtime | Rust module in `drivers/`, compiled in |
| Changes how | which tools exist and what they compose | what the board can detect and how it talks to the chip |
| Needs a reflash | no — `manifest` with `command: apply` is live | yes |
| Typical case | a relay, a servo, a raw I2C register read, a sequence of GPIO steps | SSD1306, INA226, DS18B20, I2S audio, anything with a state machine, calibration or timing |

A manifest **tool** is built from primitives (`gpio_write`, `i2c_read`, `delay`, `sequence`,
`adc`, `rgb`, `pwm`, `servo`, `distance`, …). It can move a pin and read a register; it
cannot decode a CRC, wait for a conversion-done bit, or keep a bus in a state between
calls.

A driver **decodes** a chip and contributes its own tools (`bme280_read`, `oled_text`,
`mic_level`). It is detected once at boot (and again on `devices_probe`), and its tools
appear only when the probe finds the device — a board without the chip never advertises
the capability.

A device can have both: a driver instantiates it (so the board knows it is there and what
its pins are), and manifest tools add operations the driver does not ship.

So a new peripheral starts as a manifest fragment — no reflash, testable in one call — and
becomes a driver only when it needs something a JSON primitive cannot express. See
[adding-a-device](adding-a-device.md) for both paths.

## 1. Top level

```json
{ "version": 1, "devices": [ … ], "tools": [ … ] }
```

| Field | Meaning |
|---|---|
| `version` | Fixed at `1` for now, reserved for migration |
| `devices` | Device declarations. With a `driver` the compiled driver is instantiated; without one it only records pins/address |
| `tools` | Tool declarations. This is the interface the model sees |

Limits: `tools ≤ 48`, `sequence.steps ≤ 8`, cumulative `delay ≤ 5000 ms` per call.

## 2. devices

```json
{ "id": "bme0", "kind": "sensor", "driver": "bme280",
  "bus": 0, "address": 118, "pins": { "out": 12 },
  "options": { "shunt_mohm": "100" }, "enabled": true, "note": "soldered on the back" }
```

- Valid `driver` values are the "implemented" table in `docs/capabilities.md`.
- If a `driver` is given but probing fails, that device is skipped with a reason in the
  log; the rest of the manifest still applies.
- `options` is a string map for driver-specific parameters (OLED height, INA219 shunt
  resistance, …).

## 3. tools

```json
{ "name": "pump_pulse", "title": "Pump pulse", "description": "…",
  "device": "pump", "enabled": true,
  "params": [ … ], "exec": { … }, "safety": { … } }
```

- `name`: `a-z A-Z 0-9 _ -` (no `.`, which OpenAI-compatible endpoints reject), at most
  48 characters, must be unique. The restriction is the wire charset, not a style
  choice: `function.name` is validated against `^[a-zA-Z0-9_-]+$`, and every request
  carries the whole tool list — one name outside it fails **every** call, including the
  `manifest` call (`command: apply`) that would repair the manifest.
- `name` must not be one the board already provides: the built-in tools (`board`, `bus`,
  `manifest`, `display`, `read`, `shell`, …) and the driver tools of the detected hardware
  (`bme280_read`, `sensor_read_2`, …). Reusing one is refused with
  `` `board` is already a tool the board provides `` — a manifest tool that took `manifest`
  would leave the board unable to fix its own manifest. The `manifest_`, `board_` and
  `bus_` prefixes are not reserved by themselves: `manifest_helper` is fine, `manifest` is
  not.
- `description` is the only natural language the model sees; state **units and side
  effects**.
- `params`: `{name, type: integer|number|string|boolean, required, min, max, unit, enum, default, description}`.
  The ranges go into the JSON Schema **and are enforced on the board**.

## 4. Primitives (exec)

Any value field may be a literal or `{"param": "name"}` to read a call argument.

| type | Fields | Meaning |
|---|---|---|
| `gpio_write` | `pin`, `level` | level 0/1, or a boolean argument (true = 1) |
| `gpio_read` | `pin` | returns `{pin, level}` |
| `toggle` | `pin`, `times`, `gap_ms` | blink, times ≤ 20 |
| `pwm` | `pin`, `freq_hz`, `duty_pct` | servo at 50 Hz plus duty; LED 1–5 kHz |
| `adc` | `pin` | returns millivolts (uncalibrated but monotonic) |
| `i2c_read` | `addr`, `reg`, `length`, `bus?` | returns hex and the raw bytes |
| `i2c_write` | `addr`, `reg`, `data` (array/hex/`{"param":…}`), `bus?` | raw write |
| `rgb` | `pin`, `color` (`#rrggbb` or a param), `count?` | WS2812 |
| `delay` | `ms` | blocking wait |
| `driver` | `driver`, `action` | call one of the driver's own tools, e.g. `bme280_read` |
| `sequence` | `steps: [{ms, exec}]` | composition; `ms` is the wait after that step |

`sequence` is the entry point for composing basics: pulse a relay, blink then read a light
sensor, sweep a servo and take a reading.

One tool call has a **5000 ms cumulative explicit-wait budget**. It includes every
`delay`, both the high and low waits of each `toggle`, and every step's `ms`, including
nested sequences. Apply-time validation rejects a known total above that ceiling;
large step fields cannot overflow the accounting. Parameterized waits remain legal,
including a pump-on / `delay {"param":"ms"}` / pump-off sequence. At call time all wait
arguments are resolved and summed **before any hardware access**. Missing, non-finite
or invalid unsigned wait arguments and over-budget totals return `-32602`; `delay` is
no longer silently shortened or refused after energizing a relay. Toggle arguments
retain their existing coercion to 1–20 repetitions and 10–1000 ms gaps, and those coerced
high/low waits count toward the total. An over-budget error includes
`total_delay_ms` and `max_delay_ms` in its data. This bounds manifest waits, not the wall
clock time of driver calls or hardware I/O, which have their own limits.

An `i2c_write` data parameter may contain a hex string, e.g. `"01:02 ff"`. Spaces,
commas and colons are ignored; the remaining characters must be ASCII hex digits in
complete byte pairs. Invalid text, including non-ASCII characters, returns an argument
error before any I2C write.

## 5. safety (enforced on the board)

```json
"safety": { "min": 5, "max": 175, "max_calls_per_min": 30, "confirm": true }
```

- `min/max` apply to **every numeric argument**, independently of `params[].min/max`. If
  the schema says 0..180 and safety says 5..175, then 180 is rejected — this layer exists
  to catch a hallucinating model.
- `max_calls_per_min`: sliding-window rate limit, so a model in a loop cannot use a relay
  as a drum.
- `confirm: true`: the call must carry `"confirm": true`. A direct driver call, a family
  command and `device_invoke` share the manifest policy for the same device/action;
  choosing another entry point does not skip confirmation, ranges or rate limiting.
- `mcp.disabled_tools` applies at execution time, not just to `tools/list`. Manifest
  driver actions resolve the device's published name (including `_2` suffixes) before
  checking that switch and its family. Legacy raw names remain accepted by
  `device_invoke`, but cannot evade a disabled instance. GPIO/I2C/ADC primitives and
  explicit waits check their underlying command switches too. Every nested sequence
  is preflighted before hardware access or rate accounting, so a disabled late step
  cannot leave an earlier actuator energized.

Error codes: an out-of-range argument is `-32602`; a safety range, a missing confirmation
or rate limiting is `-32005` (forbidden), with the range in the message.

## 6. Validation and errors

`manifest` with `command: apply` returns:

```json
{ "applied": false,
  "issues": [ { "path": "/tools/0/exec/pin", "message": "pin 99 is outside 0..=48", "hint": "ESP32-S3 has GPIO0..GPIO48…" } ] }
```

`path` is a JSON Pointer and `hint` is an actionable suggestion. **A failed validation
changes nothing.** On success it returns
`{applied: true, revision, tools_added, tools_removed, restart_required: false}`.

Two kinds of pin are refused beyond the plain `0..=48` range: a pin the board profile
reserves, and a pin one of the SPI flash/PSRAM lines occupies. The second is not a matter of
taste — claiming one does not fail, it takes down the core executing from it, and because the
manifest is persisted the crash repeats at every boot until the board is erased over serial.
`inventory` lists what the profile reserves (`board_profile.reserved_pins`), and a device
stored from before this rule is skipped by the probe with its reason in `declared_errors`
rather than instantiated. See [security](security.md#what-a-tool-cannot-do).

## 7. Merge and rollback

- `mode: "merge"`: overwrite by `tools[].name` / `devices[].id`, keep everything else
  (the usual choice while iterating).
- `mode: "replace"`: replace the whole document (the usual choice when importing).
- `dry_run: true`: validate only, and return `would_add` / `would_remove`.
- `manifest` with `command: reset`: clear it, keeping only the built-in tools.
- The previous version is not backed up automatically: to roll back, apply an older
  document again (the current one can be read from the `mcp://manifest` resource).

## 8. Examples

See `examples/manifests/`: `relay-pump.json`, `servo-scan.json`, `i2c-sensor-raw.json`,
`led-indicator.json`. Every one can be passed directly as the `manifest` argument
(`mode: merge`).

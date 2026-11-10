# Manifest specification v1

A manifest is a JSON document in the NVS `manifest` key. It decides which hardware this
board exposes, as which tools. Call `manifest_apply` to make it take effect: `tools/list`
changes immediately, without a reboot or a reflash.

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
  `manifest_apply` call that would repair the manifest.
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

## 5. safety (enforced on the board)

```json
"safety": { "min": 5, "max": 175, "max_calls_per_min": 30, "confirm": true }
```

- `min/max` apply to **every numeric argument**, independently of `params[].min/max`. If
  the schema says 0..180 and safety says 5..175, then 180 is rejected — this layer exists
  to catch a hallucinating model.
- `max_calls_per_min`: sliding-window rate limit, so a model in a loop cannot use a relay
  as a drum.
- `confirm: true`: the call must carry `"confirm": true`.

Error codes: an out-of-range argument is `-32602`; a safety range, a missing confirmation
or rate limiting is `-32005` (forbidden), with the range in the message.

## 6. Validation and errors

`manifest_apply` returns:

```json
{ "applied": false,
  "issues": [ { "path": "/tools/0/exec/pin", "message": "pin 99 is outside 0..=48", "hint": "ESP32-S3 has GPIO0..GPIO48…" } ] }
```

`path` is a JSON Pointer and `hint` is an actionable suggestion. **A failed validation
changes nothing.** On success it returns
`{applied: true, revision, tools_added, tools_removed, restart_required: false}`.

## 7. Merge and rollback

- `mode: "merge"`: overwrite by `tools[].name` / `devices[].id`, keep everything else
  (the usual choice while iterating).
- `mode: "replace"`: replace the whole document (the usual choice when importing).
- `dry_run: true`: validate only, and return `would_add` / `would_remove`.
- `manifest_reset`: clear it, keeping only the built-in tools.
- The previous version is not backed up automatically: to roll back, apply an older
  document again (the current one can be read from the `mcp://manifest` resource).

## 8. Examples

See `examples/manifests/`: `relay-pump.json`, `servo-scan.json`, `i2c-sensor-raw.json`,
`led-indicator.json`. Every one can be passed directly as the `manifest` argument
(`mode: merge`).

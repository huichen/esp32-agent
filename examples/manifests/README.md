# Manifest examples

Each file is a **complete argument object** that can be handed straight to
`manifest {command: "apply"}` (`mode: merge`), or pasted into the console's manifest tab and applied
there.

| File | Hardware it needs | What it shows |
|---|---|---|
| `relay-pump.json` | a relay on GPIO12 (active low) | on/off plus a timed pulse; `confirm` and rate limiting are both on |
| `servo-scan.json` | a servo on GPIO7 | an angle setter plus one sweep; `safety` is narrower than `params` (5–175 degrees) |
| `i2c-sensor-raw.json` | a hypothetical light sensor at 0x40 | reads the raw value with `i2c_read` and puts the conversion in the description |
| `led-indicator.json` | the WS2812 on GPIO48 | a status light plus a blink, with an `enum` parameter over presets |

## Writing your own

1. Start with `inventory` / `i2c_scan` to confirm the real address and pins.
2. State a range twice: `params[].min/max` for the model, `safety.min/max` to enforce on
   the board.
3. Anything physical gets `max_calls_per_min`; anything irreversible gets `confirm: true`.
4. Put units in both `description` and `unit` — they are all the model can see.
5. Plain JSON, no comments; when you need to explain something use `devices[].note` or
   fold it into `description`.

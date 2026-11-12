# Contributing

The goal is that supporting new hardware needs nothing but configuration, and only
at worst some Rust. Read [AGENTS.md](AGENTS.md) first (branching, commits, the check
commands).

## Two paths — take the lighter one

### Path A: manifest only (no firmware changes)

For: GPIO switches, PWM, ADC, reading one I2C register, composed actions.

1. Write the manifest in the web console or through `manifest_apply`, e.g. reading an
   I2C sensor at a known address:

   ```json
   {"tools":[{"name":"soil.moisture","description":"Read the raw soil moisture value",
     "params":[],"exec":{"type":"i2c_read","addr":72,"reg":0,"length":2},
     "safety":{"max_calls_per_min":30}}]}
   ```

2. Once it works, put it in `examples/manifests/<device>.json` and record in a header
   comment: the chip part number, the address, the wiring, the datasheet page, and how
   the raw value converts to a physical quantity (a model needs that last part).
3. Add a row to the table in `docs/capabilities.md` with status `manifest`.

### Path B: a driver (needs timing, calibration or a state machine)

For: SSD1306, BME280, DS18B20, HC-SR04 — anything that has to run a protocol or the
compensation itself.

1. Add a module under `crates/agent-core/src/drivers/` implementing the `Device` trait
   (`info` / `tools` / `call`).
2. `probe()` must confirm identity from **register-level evidence** (chip id, id
   register, CRC), not from the address alone — addresses collide.
3. Compensation/conversion formulas need unit tests; when the datasheet gives worked
   examples, assert against those values (see `bme280.rs`).
4. Add a row to `CATALOG` in `drivers/mod.rs` and a probe call in `detect()` (order
   matters: specific before generic).
5. Add a branch in `instantiate()` so a manifest can declare the device explicitly
   (address/pins included).
6. Update `docs/capabilities.md` with how the device is identified and its known traps.

## Driver checklist

- [ ] Uses only the `Hardware` trait, never ESP-IDF types directly
- [ ] `probe` failure returns `None`/`Err` rather than panicking
- [ ] Every I2C read has a timeout or propagates the error
- [ ] Units appear in the tool description and in `detail` (°C, hPa, %RH, mV, cm)
- [ ] At least one unit test (parsing, conversion or CRC)
- [ ] A fake device in `virtual_board.rs` so the simulator can demonstrate it too

## Changing the manifest engine (`manifest.rs`)

- New primitive: add an `Exec` variant, a `run_exec` branch, a `validate_exec` check,
  the `primitives_doc()` entry, and `docs/manifest.md`.
- New field: `#[serde(default)]` is mandatory so old manifests still load.
- Every new capability that can move something physical needs a matching `safety`
  story; an unrestricted path is not the default.

## Documentation and examples

- Examples must run: at least one `scripts/smoke.ps1` (or `smoke.sh`) check must reference it.
- Never write "should work": unverified behaviour goes into `ROADMAP.md`.

## Before committing

```bash
cargo test -p esp32-agent-core
powershell -File scripts/smoke.ps1        # or: bash scripts/smoke.sh
cargo clippy -p esp32-agent-core -p esp32-agent-sim -- -D warnings
cargo fmt --all --check
```

## Code of conduct

See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md): argue about the work, bring evidence, and
do not attack people over which hardware they use.

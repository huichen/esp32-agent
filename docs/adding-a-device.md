# Adding a device

Decide first whether code is needed at all.

## A. Manifest only (the usual case)

Good for: GPIO switches, PWM, ADC, reading or writing I2C registers, and compositions of
those.

1. Confirm you can read the data, in the console's manifest tab or by calling a tool:

   ```json
   {"mode":"merge","manifest":{"tools":[{
     "name":"light_level","description":"Read the photoresistor on GPIO1 in millivolts",
     "params":[],"exec":{"type":"adc","pin":1}}]}}
   ```

2. Anything the manifest cannot express (say, converting to lux) either goes into
   `description` as a formula, or needs a small driver (see B).
3. Put it in `examples/manifests/<name>.json` with a header comment recording the chip, the
   address, the wiring, the datasheet page and the conversion formula.
4. Add a row to `docs/capabilities.md` with status `manifest`.
5. Add a call to `scripts/smoke.ps1` (proof that it really runs).

## B. Writing a driver

For chips that need timing (1-Wire, ultrasonic), calibration (BME280) or a state machine
(SSD1306).

### 1. A new module `crates/agent-core/src/drivers/<chip>.rs`

```rust
use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

pub struct MyChip { pub bus: u8, pub addr: u8, id: String }

impl MyChip {
    pub const ADDRESSES: &'static [u8] = &[0x40];

    /// None means "this address is not this chip". Use register evidence, never the
    /// address alone.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<u8> {
        let mut id = [0u8; 1];
        hw.i2c_read(bus, addr, Some(0x0F), &mut id).ok()?; // the ID register from the datasheet
        matches!(id[0], 0x21 | 0x22).then_some(id[0])
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, suffix: usize) -> Result<Self> {
        let _ = Self::identify(hw, bus, addr)
            .ok_or_else(|| McpError::not_found(format!("no MyChip at 0x{addr:02X}")))?;
        Ok(Self { bus, addr, id: if suffix > 1 { format!("mychip_{suffix}") } else { "mychip".into() } })
    }
}

impl Device for MyChip {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("mychip", "MyChip sensor", "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["mychip".into(), "i2c".into()];
        i.tools = vec!["mychip_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("mychip_read", "Read the MyChip measurement (unit: lux)")
            .category("sensor").requires(&["mychip"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| { let id = id.clone(); ctx.devices.call(&id, "mychip_read", args, ctx.hw) })]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "mychip_read" => {
                let mut b = [0u8; 2];
                hw.i2c_read(self.bus, self.addr, Some(0x10), &mut b)?;
                let lux = u16::from_be_bytes(b) as f32 / 1.2;
                Ok(json!({ "lux": (lux * 10.0).round() / 10.0 }))
            }
            other => Err(McpError::not_found(format!("mychip has no tool `{other}`"))),
        }
    }
}
```

### 2. Wire it into four tables

| Where | What to add |
|---|---|
| `pub mod` in `drivers/mod.rs` | `pub mod mychip;` |
| `CATALOG` in `drivers/mod.rs` | one row: kind / name / identification / `implemented` / caps / source file |
| `detect()` in `drivers/mod.rs` | in a sensible order, `for addr in MyChip::ADDRESSES { if present(...) { ... } }` |
| `instantiate()` in `drivers/mod.rs` | `"mychip" => Box::new(MyChip::probe(...)?)` so a manifest can declare it explicitly |

### 3. Add a fake device (so the simulator can show it too)

Add a variant to `FakeKind` in `crates/agent-core/src/virtual_board.rs`, fill in the
registers in `FakeI2c::new` (ID register, measurements, CRC), and hook it up in
`VirtualBoard::new()`.

### 4. Tests

- Identification: assert what `identify` returns against `NullHardware` or `VirtualBoard`.
- Conversion: assert against the datasheet's worked examples (see `drivers/bme280.rs`).
- Tool visibility: assert in an `app.rs` test that the tool does not appear when the device
  is absent.

### 5. Documentation

A row in `docs/capabilities.md`, and any trap you hit goes into `docs/troubleshooting.md`.

## C. The "semi-automatic" route: read a register, write no driver

Some chips are not worth a driver (low usage, one register to read). A manifest primitive
is enough, and the model can follow it:

```json
{"tools":[{"name":"mychip_raw","description":"Read 2 bytes at MyChip 0x10 (raw counts; divide by 1.2 for lux)",
  "params":[],"exec":{"type":"i2c_read","addr":64,"reg":16,"length":2}}]}
```

Writing the conversion into the description is far better than leaving the model to guess
at registers.

## D. A new board profile

Edit `PROFILES` in `crates/agent-core/src/boards.rs`: I2C pins, LED, buttons,
microphone/amplifier, ADC pins, reserved pins, and a one-line description. The test
`every_profile_has_a_unique_id_and_docs` checks that ids are unique. Add a row to the board
table in `docs/capabilities.md`.

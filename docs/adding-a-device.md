# 加一个设备

先判断需不需要写代码。

## A. 只写清单（大多数情况）

适合：GPIO 开关、PWM、ADC、读/写 I2C 寄存器、以及这些的组合。

1. 先在控制台"清单"页或直接调工具确认能读到数据：

   ```json
   {"mode":"merge","manifest":{"tools":[{
     "name":"light.level","description":"读光敏电阻（GPIO1）的毫伏值",
     "params":[],"exec":{"type":"adc","pin":1}}]}}
   ```

2. 换算成物理量也在清单里做不了的部分（比如光照 lux），要么在 `description` 里写清换算公式，
   要么写一个小驱动（见 B）。

3. 落到 `examples/manifests/<name>.json`，文件头注释写：芯片、地址、接线、数据手册页码、换算公式。
4. `docs/capabilities.md` 表格加一行，状态 `manifest`。
5. 在 `scripts/smoke.ps1` 里加一条调用（证明它真能跑）。

## B. 写驱动

适合：需要时序（1-Wire、超声波）、校准（BME280）、状态机（SSD1306）的芯片。

### 1. 新建模块 `crates/agent-core/src/drivers/<chip>.rs`

```rust
use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

pub struct MyChip { pub bus: u8, pub addr: u8, id: String }

impl MyChip {
    pub const ADDRESSES: &'static [u8] = &[0x40];

    /// 返回 None = 这个地址上不是它。必须用寄存器证据，不要只看地址。
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<u8> {
        let mut id = [0u8; 1];
        hw.i2c_read(bus, addr, Some(0x0F), &mut id).ok()?; // 数据手册里的 ID 寄存器
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
        let mut i = DeviceInfo::new("mychip", "MyChip 传感器", "i2c");
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.caps = vec!["mychip".into(), "i2c".into()];
        i.tools = vec!["mychip_read".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("mychip_read", "读 MyChip 的测量值（单位：lux）")
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

### 2. 接进四张表

| 位置 | 加什么 |
|---|---|
| `drivers/mod.rs` 的 `pub mod` | `pub mod mychip;` |
| `drivers/mod.rs` 的 `CATALOG` | 一行：kind / 名称 / 识别方式 / `implemented` / caps / 源文件 |
| `drivers/mod.rs` 的 `detect()` | 在合适顺序里加 `for addr in MyChip::ADDRESSES { if present(...) { ... } }` |
| `drivers/mod.rs` 的 `instantiate()` | 增加 `"mychip" => Box::new(MyChip::probe(...)?)`，让清单能显式声明 |

### 3. 加一个假设备（让模拟器也能演示）

`crates/agent-core/src/virtual_board.rs` 的 `FakeKind` 加变体，在 `FakeI2c::new` 里填寄存器
（ID 寄存器、测量数据、CRC），`VirtualBoard::new()` 里可以挂上它。

### 4. 测试

- id 识别：对 `NullHardware` 或 `VirtualBoard` 断言 `identify` 的返回。
- 换算：用数据手册示例值写断言（参考 `drivers/bme280.rs`）。
- 工具可见性：`app.rs` 的测试里断言"没有这个设备时工具不出现"。

### 5. 文档

`docs/capabilities.md` 加一行；有坑就写进 `docs/troubleshooting.md`。

## C. 只读寄存器、不写驱动的"半自动"路线

有些芯片不值得写驱动（用量小、只需要读一个寄存器）。用清单原语即可，模型侧也能理解：

```json
{"tools":[{"name":"mychip.raw","description":"读 MyChip 0x10 起的 2 字节（原始计数，除以 1.2 得 lux）",
  "params":[],"exec":{"type":"i2c_read","addr":64,"reg":16,"length":2}}]}
```

把"原始值怎么换算"写进 description，比让模型猜寄存器强得多。

## D. 新板型

编辑 `crates/agent-core/src/boards.rs` 的 `PROFILES`：I2C 引脚、LED、按键、麦克风/功放、ADC 引脚、
保留引脚、一行说明。测试 `every_profile_has_a_unique_id_and_docs` 会检查 id 唯一。
`docs/capabilities.md` 的板型表加一行。

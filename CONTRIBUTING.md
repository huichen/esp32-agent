# 贡献指南

目标是让"支持一块新硬件"尽量只需要写配置，最坏才写 Rust。请先读 [AGENTS.md](AGENTS.md)（分支、提交、检查命令）。

## 两条路，先选轻的那条

### 路线 A：只写清单（不需要碰固件）

适用于：GPIO 开关、PWM、ADC、读某个 I2C 寄存器、组合动作。

1. 在网页控制台或 `manifest_apply` 里写清单，例如读一个已知地址的 I2C 传感器：

   ```json
   {"tools":[{"name":"soil.moisture","description":"读土壤湿度原始值",
     "params":[],"exec":{"type":"i2c_read","addr":72,"reg":0,"length":2},
     "safety":{"max_calls_per_min":30}}]}
   ```

2. 跑通后把这段放进 `examples/manifests/<device>.json`，并在文件头注释里写：芯片型号、地址、接线、
   数据手册页码、以及"原始值怎么换算成物理量"（模型需要这条信息）。
3. 在 `docs/capabilities.md` 的表格里加一行，状态写 `manifest`。

### 路线 B：写驱动（需要时序、校准或状态机）

适用于：SSD1306、BME280、DS18B20、HC-SR04 这类必须自己跑协议/补偿的。

1. 在 `crates/agent-core/src/drivers/` 新建模块，实现 `Device` trait（`info` / `tools` / `call`）。
2. `probe()` 要用**寄存器级证据**确认身份（chip id、id register、CRC），不要只看地址——地址会撞。
3. 补偿/换算公式必须配单元测试；有数据手册示例值的直接用示例值（见 `bme280.rs`）。
4. 在 `drivers/mod.rs` 的 `CATALOG` 加一行，并在 `detect()` 里加探测调用（注意顺序：先具体后通用）。
5. 在 `instantiate()` 里加分支，让模型可以通过清单显式声明该设备（含地址/引脚）。
6. 补 `docs/capabilities.md`，写清识别方式与已知坑。

## 驱动检查清单

- [ ] 只用 `Hardware` trait，不直接引用 ESP-IDF 类型
- [ ] `probe` 失败返回 `None`/`Err` 而不是 panic
- [ ] 所有 I2C 读取都有超时或错误传播
- [ ] 单位写进工具描述与 `detail`（°C、hPa、%RH、mV、cm）
- [ ] 有至少一个单元测试（解析、换算或 CRC）
- [ ] 在 `virtual_board.rs` 里加一个假设备，让模拟器也能演示

## 改清单引擎（`manifest.rs`）

- 新增原语：加 `Exec` 变体、`run_exec` 分支、`validate_exec` 校验、`primitives_doc()` 文档、`docs/manifest.md`。
- 新增字段：`#[serde(default)]` 必加，保证旧清单仍能加载。
- 任何新的"能做物理动作"的能力都要有对应的 `safety` 说明；默认不提供无限制通路。

## 文档与示例

- 示例必须能跑：`scripts/smoke.ps1` 里至少有一条引用它。
- 不写"应该可以"：未验证的写进 `ROADMAP.md`。

## 提交前

```bash
cargo test -p esp32-agent-core
powershell -File scripts/smoke.ps1
cargo clippy -p esp32-agent-core -p esp32-agent-sim -- -D warnings
cargo fmt --all --check
```

## 行为准则

见 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)：就事论事、给证据、别让人身攻击硬件型号。

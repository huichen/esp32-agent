# 变更记录

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

## [Unreleased]

### Added

- **板载智能体**：模型循环跑在 ESP32 上（`llm.rs` + `agent.rs`），OpenAI 兼容
  `/chat/completions`，一轮对话内可多步调用工具，历史落 NVS、门户可看
- **智能体工具**：板子原有全部 MCP 工具 + 文件工具（`fs_ls/fs_read/fs_write/fs_edit/fs_grep`，
  NVS 工作区）+ 网络工具（`net_get/net_post`）；没有 shell——设备上本来就没有
- **编排层**：定时任务（`every_s` 或 `at_hhmm`，每天一次、同分钟不重跑）与传感器规则
  （工具 + 结果里的 JSON 路径 + 比较 + 限频）；两者都只是往同一个队列投一条 prompt
- **智能体接口**：`/api/agent/message`、`/api/agent/history?after=`、`/api/agent/reset`、
  `/api/agent/config`、`/api/sched`，以及门户里的「智能体」面板（发送 / 对话 / 调度表 / 模型连接）
- **无密钥自检**：板上 `POST /selftest/chat/completions`（脚本化模型，排在 App 锁之前）与
  主机侧 `tools/stub_model.py`，配合 `tools/agent_smoke.ps1` 可在没有 API key 的情况下验完整闭环
- 清单层（`manifest.rs`）：设备与工具用 JSON 声明，校验后在板上热生效，不重启、不重烧
- 11 种原语：`gpio_write/read`、`toggle`、`pwm`、`adc`、`i2c_read/write`、`rgb`、`delay`、`driver`、`sequence`
- 板侧强制的 `safety`：范围、限频、`confirm`
- 驱动：SSD1306/SH1106、PCF8574+HD44780、BME280/BMP280、SHT3x、INA219/INA226、MPU6050、
  ADS1115、DS18B20、HC-SR04、舵机、继电器、RGB LED、按键
- 自动探测：I2C 扫描 + 寄存器级身份确认；1-Wire presence + ROM CRC；板型引脚表
- `i2c_identify`：未识别地址给出候选驱动与寄存器快照
- MCP：`initialize`（按实际硬件生成 instructions）、`tools/list`（能力过滤）、`tools/call`、
  `resources/*`（能力/设备/清单/配置/日志/目录）
- 两种工具风格：`auto`（每动作一工具）与 `generic`（`device_invoke` 收口）
- HTTP 控制台：中英双语、WiFi 配网、设备表、工具表、清单编辑器（校验 + 应用）、日志、客户端配置
- 热点配网：无凭证时开 `esp32-agent-setup`，`192.168.4.1` 提供服务，捕获式 DNS
- 主机模拟器 `mcp-sim`：虚拟寄存器板子，跑同一套服务
- 文档：架构、清单规范、能力表、HTTP API、加设备、烧录、排错、路线图、AGENTS.md

### Changed

- 项目改名为 **esp32-agent**：包名 `esp32-agent-core/sim/firmware`、二进制 `esp32-agent`、
  配网热点 `esp32-agent-setup`、MCP `serverInfo`、门户标题全量跟随。MCP 协议与 `/mcp`
  路径不变（它现在是传输层），工具名不变（否则已存的清单与提示词会断），
  存储命名空间仍为 `esp32mcp`（改名会清空已烧板子的配置与历史）

### Fixed

- `tools/list` 改为逐工具序列化，不再先建整棵 `Value` 树：28 个工具的树在 S3 上会耗尽堆
  并 abort（板子会在客户端第一次 `tools/list` 时重启）
- 固件启动顺序：lwIP、NVS 分区、事件循环按序初始化；`Peripherals` / NVS 分区 / 系统事件循环
  各只取一次并传入电台线程（第二次 `take()` 返回 `ESP_ERR_INVALID_STATE`，线程静默退出）
- I2C 显式请求 `I2C_CLK_SRC_XTAL`（`zeroed()` 的默认值不在 S3 时钟树里，`i2c_new_master_bus` 报 258）
- 站模式在 `start()` 后补 `connect()` 并在断线时重试，`at_hhmm` 依赖的 SNTP 也一并启动
- 配网 AP 钉在 `192.168.4.0/24`（esp-idf-svc 默认给 `192.168.71.0/24`，与文案/捕获 DNS 不一致）
- `sdkconfig.defaults` 与 `partitions.csv` 真正生效：`Project name: esp32-agent`，
  仓库的七分区表（含 `otadata`/`ota_0`/`ota_1`）随镜像下发，`esp_partition` 运行时可枚举
- 设备探测：`board: auto` 不再只认一种 I2C 走线——多开一个候选总线，谁应答就采用哪个板型 profile
- `uptime_s` 等待真实墙钟再计时（之前 SNTP 校时后会报出几十年）
- 改 I2C 引脚或板型现在会真的要求重启：按**生效配置**比较（`before.bus != cfg.bus || before.board != cfg.board`），
  命中就置 `restart_requested` 并在响应里回 `restart_required: true`——驱动一旦开了总线就不再改引脚，
  以前它静默沿用旧走线却回“成功”，下一次探测扫的是没人接的引脚
- 面板高度改为读 `pins["oled.height"]`（默认 64）：载板是 128×32，写死 64 时下半个画面根本不在面板上，
  工具却回 ok；`crates/agent-core/tests/display_height.rs` 钉住 32/64 两种几何的裁切与配置到驱动的链路

### Known gaps

- 智能体一轮对话期间持有 App 锁，门户在这段时间只能排队（把模型网络等待移出锁的分阶段改造
  是下一步）；`net_*` 指向板子自己的路由会因同一把锁自锁
- 模型接口只支持 `http://`（`https://` 明确拒绝）；局域网代理是当前预期部署方式
- 文件工作区在内存里、整体存成一块 NVS blob（单文件 ≤ 2 KB，总体 ≤ 8 KB）；SPIFFS 待接
- `fs_grep` 只有主机单测；`/api/ota` 未实现（分区已就绪）；WS2812/RMT、ADC/PWM 未实测

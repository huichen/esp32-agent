# esp32-agent

把一块 ESP32 变成**能自己发现自己接了什么、并且允许大模型在线改配置**的 MCP 服务器。

固件里跑一个 Streamable HTTP 的 MCP 端点和一个中文/英文的网页控制台。首次上电没有 WiFi 密码也能用：
板子自己开一个热点，手机连上去在固定地址 `192.168.4.1` 填家里的 WiFi，保存即联网，**密码不烧进固件**。

真正的重点不是"又一个小智"，而是把 **驱动 / 能力描述 / MCP 胶水** 三层拆开：

```
驱动层（编译进固件，只在需要时序、校准、状态机时才写）
      ↑ 按 id 查表
清单层（manifest JSON，存在 NVS）  声明有哪些设备、暴露成哪些工具；原语可组合
      ↑ 编译成 ToolDef
MCP 层（与设备无关，只写一次）      JSON-RPC、HTTP、会话、鉴权、能力过滤
```

于是"加一个外设"不必重烧固件：大模型先 `inventory` / `i2c_identify` 摸清接线，再用 `manifest_apply`
写一段 JSON，工具列表当场重建，**不重启、不重烧**，调用新工具、看结果、不对再改——一次会话里把周边配到最好。

---

## 30 秒上手（不用板子）

```bash
cargo run -p esp32-agent-sim          # 起一个虚拟板子：OLED + BME280 + SHT3x + INA219
# 打开 http://127.0.0.1:8080/      网页控制台
# MCP 端点 http://127.0.0.1:8080/mcp
```

```bash
curl -sS -X POST http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

模拟器跑的是**和固件同一份 `esp32-agent-core`**：网页、MCP、驱动、清单引擎、校验逻辑全都一样，
只有 `Hardware` trait 的实现换成了内存里的寄存器文件。所以没有板子也能开发、测试、演示。

## 烧到板子上

```bash
# 一次性：工具链（espup：Rust + Xtensa 工具链），并装好 ESP-IDF 依赖
cargo install espup espflash ldproxy
espup install

# 每次：编译并烧录（默认 ESP32-S3；改 firmware/.cargo/config.toml 换型号）
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

烧完串口会打印：

```
wifi: setup mode: access point `esp32-agent-setup` at http://192.168.4.1/
http: listening on http://0.0.0.0:8080/
```

手机连 `esp32-agent-setup` → 打开 `http://192.168.4.1/` → 选 WiFi、输密码 → 保存。
板子随后以 STA 加入你的网络，控制台也能在新地址上打开。热点默认保留（`wifi.ap_fallback`），
断网时还能上去改配置。

## 大模型怎么用这套东西

| 阶段 | 工具 | 作用 |
|---|---|---|
| 发现 | `inventory`、`devices_list`、`i2c_scan`、`i2c_identify` | 看芯片、总线、已识别设备、**已应答但没有驱动的地址** |
| 建模 | `manifest_describe` | 拿到清单 JSON Schema、11 种原语、驱动目录、上限、示例 |
| 写入 | `manifest_apply`（`mode: merge/replace`，可 `dry_run`） | 校验并**立即生效**；出错返回带 JSON Pointer 的 issues |
| 验证 | `tools/list`、`mcp://manifest` | 新工具已在列表里，直接调用 |
| 迭代 | 改 JSON 再 `manifest_apply` | 工具列表重建，无需重启/重烧 |

清单里的一段就够让一块没写代码的继电器变成工具：

```json
{
  "devices": [{ "id": "pump", "kind": "actuator", "pins": { "out": 12 }, "note": "低电平吸合" }],
  "tools": [{
    "name": "pump.pulse",
    "description": "让水泵继电器吸合指定毫秒数",
    "params": [{ "name": "ms", "type": "integer", "min": 10, "max": 5000, "unit": "ms", "required": true }],
    "exec": { "type": "sequence", "steps": [
      { "ms": 0, "exec": { "type": "gpio_write", "pin": 12, "level": 0 } },
      { "ms": 0, "exec": { "type": "delay", "ms": { "param": "ms" } } },
      { "ms": 0, "exec": { "type": "gpio_write", "pin": 12, "level": 1 } }]},
    "safety": { "max_calls_per_min": 20, "confirm": true }
  }]
}
```

**校验在板子上执行，不只在 schema 里写着。** `min/max`、`safety.min/max`、`max_calls_per_min`、`confirm`
都在真正动引脚之前检查；越界调用返回 `forbidden` 并说明范围，测试里有专门的越界用例。

## 为什么工具列表不会变长到失控

- 没接的设备**不进工具列表**：工具由驱动/清单声明，`requires` 与检测到的能力求交集。
- 两个风格可切（网页或 `POST /api/tools`）：
  - `auto`：每个动作一个工具，语义清楚；
  - `generic`：把设备工具收口成 `device_invoke`，只留几十个核心工具——外设一多就靠它。
- 还可以按名字禁用单个工具。

## 智能体（Agent）

板子自己会做事：一个跑在 ESP32 上的模型循环，把板子的工具交给模型。

- 发一条消息：`POST /api/agent/message {"text":"看看板子上有什么"}` → 202（入队），
  循环里可连续调用工具，最终文本落在 `/api/agent/history?after=`
- 模型：OpenAI 兼容 `/chat/completions`，只走明文 `http://`；在门户「智能体」面板填
  `base_url` / `model` / `api_key`（key 只存 NVS，接口不回显）
- 工具：板子全部 MCP 工具 + 文件工具 `fs_ls/fs_read/fs_write/fs_edit/fs_grep`（NVS 工作区）
  + 网络工具 `net_get/net_post`；没有 shell，`ls`/`grep` 是其中真正有意义的部分
- 编排：`POST /api/sched` 增加定时任务（`every_s` 或 `at_hhmm` + `prompt`）或传感器规则
  （`tool` + `path` + `op` + `value`，带限频）；到点或触发时与用户消息走同一条队列
- 没有 key 也能验：板上的 `POST /selftest/chat/completions` 是脚本化模型（把 `base_url`
  指过去即可跑通闭环），主机侧等价物是 `tools/stub_model.py` + `tools/agent_smoke.ps1`

单进程下的取舍：一个 agent 线程、每秒一次 tick、同一时刻只跑一轮、队列深度 1（新触发合并）。
一轮对话期间门户请求会排队等待——这是已知限制，下一步是把模型网络等待移出 App 锁。
## 仓库结构

```
crates/agent-core/      与板卡无关的全部逻辑（可在主机上跑、被测试覆盖）
  hardware.rs         固件唯一要实现的特质：GPIO/I2C/PWM/ADC/RGB/延时/WiFi
  drivers/            每个芯片一个模块：ssd1306 bme280 sht3x ina219 mpu6050 ads1115 pcf8574 ds18b20 hcsr04 servo relay led
  manifest.rs         清单：校验 + 编译成工具 + 合并 + 安全限制
  tools/              板级/系统/清单工具
  http.rs             阻塞式 HTTP/1.1（keep-alive、chunked、100-continue）
  app.rs              状态、JSON-RPC 分发、/mcp 与网页路由
  web/                内嵌的控制台（无构建步骤）
crates/agent-sim/       主机模拟器：虚拟板子 + 同一套服务
firmware/             ESP-IDF 固件：WiFi(AP+STA)、NVS、总线/引脚、入口
docs/                 架构、清单、能力、HTTP API、加设备、烧录、排错
```

## 状态（诚实版）

| 部分 | 状态 |
|---|---|
| `mcp-core`（协议、HTTP、清单、驱动、校验、网页） | ✅ 94 个单元/集成测试通过，`scripts/smoke.ps1` 端到端通过 |
| 主机模拟器 | ✅ 可跑，网页与 MCP 全流程可用 |
| ESP-IDF 固件层 | ⚠️ 代码完整、结构对，但**尚未在本机编译烧录**（本机没装 ESP-IDF 工具链）；第一次 `build-firmware.ps1` 可能需要修若干 API 细节 |
| OTA、音频(I2S)、SD 卡、DHT/RC522/VL53L0X | 📋 见 `ROADMAP.md` |

## 文档

- [架构](docs/architecture.md) — 三层怎么分工，为什么这样切
- [清单规范](docs/manifest.md) — 字段、原语、校验规则、示例
- [能力与设备](docs/capabilities.md) — 驱动目录、识别方式、引脚约定
- [HTTP / MCP 接口](docs/http-api.md) — 路由、鉴权、会话、工具约定
- [加一个设备](docs/adding-a-device.md) — 只写清单 / 写驱动 两条路
- [烧录与首次配置](docs/flashing.md) — 工具链、分区、热点流程
- [排错](docs/troubleshooting.md) — 屏不亮、I2C 扫不到、工具没出现

## 贡献

这个项目的目标就是"社区加设备"：一个新外设，优先只写一份 `manifest` 片段（放进
`examples/manifests/`），只有真的需要时序/校准才写 Rust 驱动。
详见 [CONTRIBUTING.md](CONTRIBUTING.md)；仓库协作约定在 [AGENTS.md](AGENTS.md)。

## License

MIT，见 [LICENSE](LICENSE)。

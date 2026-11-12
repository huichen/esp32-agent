# esp32-agent

**简体中文** · [English](README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

运行自身智能体循环并提供 MCP 端点的 ESP32 固件，因此一个模型可以直接驱动这块板，任何
外部智能体也能通过 HTTP 使用同一批工具。

烧录、接入 WiFi，它就会探测自己有驱动的总线。每个应答的设备都成为 MCP 工具。板上的智能
体看到同一批工具并用它们作答；新的传感器可以在板子运行期间用 JSON 声明。

> 智能体循环遵循 [FutureOS](https://github.com/futuregene/future-os)：会话记录、工具
> 日志、流事件、协议适配器、记忆索引。

| 要点 | 细节 |
|---|---|
| 模型跑在板上 | 循环、工具、记忆与会话记录都是固件；模型可以是任何 OpenAI 兼容或 Anthropic 端点。[给它一个模型](docs/model.md) |
| 工具集就是一份文档 | `manifest_apply` 校验 JSON 清单，把它编译成工具，同一轮内即生效。[清单](docs/manifest.md) |
| 自己发现硬件 | 启动时探测 I2C、1-Wire 与已声明的引脚；只有应答的才得到工具。[能力](docs/capabilities.md) |
| 一套工具，两个调用方 | MCP 端点与板载智能体看到同一批工具，来自同一份描述。[HTTP 与 MCP API](docs/http-api.md) |
| 通过 HTTPS 的控制台 | 在手机上对话，并配置设备、工具、定时、日志、WiFi 与模型；证书由板子自己签发。[网页控制台](docs/web-console.md) |
| 无人值守也会动作 | 定时提示，以及读数越过阈值时触发提示的传感器规则。[定时任务](docs/scheduling.md) |
| 一个 shell 和一门脚本语言 | `shell` 有 26 个按 NuttX 命名的命令，还有一个对工具调用做循环的解释器。[Shell](docs/shell.md) · [脚本](docs/scripting.md) |
| 它会记住告诉过它的事 | 常驻指令、`/AGENTS.md`、`/MEMORY.md` 索引与技能，每轮组装进提示词。[模型每轮都能看到什么](docs/context.md) |
| 路径中没有云端 | 由板子提供 HTTPS，证书由它生成，三类调用方一个 PIN。[认证](docs/security.md) |
| 无需硬件即可测试 | 模拟器把同一份 `agent-core` 跑在虚拟板上。[模拟器](docs/simulator.md) |

## 烧录与连接

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

**不想装工具链？** 每个 tag 的 release 都附带用该 tag 源码构建好的合并镜像：
`espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin`。
`scripts/` 下的脚本是 PowerShell；macOS 或 Linux 上对应的是 `. $HOME/export-esp.sh`，
然后在 `firmware/` 里 `cargo build --release --target xtensa-esp32s3-espidf`
（[细节](docs/flashing.md)）。

没有已保存的凭据时，板子会自己起一个接入点。用手机接入它并打开 `http://192.168.4.1/`，
或者通过 USB 发送 `provision <ssid> <password>`。凭据写入 NVS，绝不进镜像。

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

443 端口以 TLS 提供同样的路由，证书在首次启动时生成；浏览器会报警告，因此要比较指纹。80
端口保持开放，因此证书有问题只损失一个 URL，而不是整块板子。`GET /agent.json` 把整个接口
作为数据给出：40 条路由及其方法、请求体、返回值与认证，外加每个可写设置及其类型与默认值。

## 给它一个模型

模型是一项配置，不是一次编译：

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"在 OLED 上写 HELLO，把灯设成蓝色"}'
```

三种协议（`chat`、`responses`、`anthropic`）、思考等级、上下文窗口与输出上限，都在控制台的
Agent 面板或同一个 `POST` 里。没有 key 时把 `base_url` 指向 `http://127.0.0.1/selftest/chat/completions`，
循环照样能跑。字段表与三种协议的并排对比见 [Model](docs/model.md)。

## 使用

| 从哪里 | 能得到什么 |
|---|---|
| 手机打开 `http://<board>/` | 与模型对话、查看设备与工具、改任何设置 |
| MCP 客户端发 `POST /mcp` | 与板上 agent 完全相同的工具 |
| `POST /api/script`、`POST /api/sched` | 立刻跑一个脚本，或让它按时间跑 |
| `GET /agent.json` | 整个接口即数据：路由、设置、上限 |

```bash
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"shell","arguments":{"command":"ls /"}}}'
```

板上的 agent 也能给自己加硬件（`manifest_apply`）、加记忆（`write /memory/…`）和 shell 权限——
用的是同一批调用，同一个 PIN。

## 安全须知

- **凭据只留在板上。** WiFi 通过 `provision` 写进 NVS，不烧进镜像；`api_key` 只写，读回来是 `__set__`。
- **板子离开你的桌子之前先设 PIN。** `auth.portal_pin` 守住所有写操作，以及会暴露会话记录、提示词、
  配置与日志的读操作。
- **核对证书。** 板子的 TLS 证书是自签的，指纹在 `GET /api/tls`。80 端口默认保持开着，证书坏了只损失一个 URL，不是整块板；把 `mcp.plain_http` 设为 false 就关掉它，那时唯一的退路是 USB 线。
- **`shell`、`script` 与清单工具会动引脚。** 这正是这块板的意义；PIN 是“agent 在干活”和“路人顺手”之间的界线。

[Security](docs/security.md) 写了哪些东西是故意不设防的。

## 仓库结构

`crates/` 在主机上构建并测试；`firmware/` 是 ESP-IDF 后端。`agent-core` 是这块板，藏在一
个 `Hardware` trait 之后，没有芯片相关代码，因此 `cargo test` 能在笔记本上运行它；
`agent-sim` 针对寄存器文件实现该 trait。合并两者会把模拟器塞进每个镜像，也会把固件的依
赖塞进每个测试。`firmware/` 退出 workspace，因为它需要 `cargo +esp`、`-Zbuild-std`，以
及检出好的 ESP-IDF。

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) · boards.rs ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## 状态

| 部分 | 状态 |
|---|---|
| `agent-core` | 427 个单元测试与 14 个集成测试通过；`scripts/smoke.ps1` 端到端通过 |
| 模拟器 | 控制台、MCP 与智能体循环端到端运行 |
| 固件 | 在 ESP32-S3-DevKitC-1 N16R8（IDF v5.5.2）上烧录：42 个工具、OLED 128×32、I²S 麦克风与功放、WS2812、TLS 控制台、通过 HTTPS 的模型轮次 |
| 未实现 | OTA、SD 卡、DHT11/22、摄像头。见 `ROADMAP.md` |

台架数字：一次用到四个工具的轮次 13.7 s；经 TLS 的控制台 0.55 s；I²C 扫描在 8 次运行中
稳定；加载一次 TLS 页面后内部 SRAM 空闲 5–6 kB，这是这里最紧的约束。

## 文档

[架构](docs/architecture.md) · [清单](docs/manifest.md) · [能力](docs/capabilities.md) ·
[HTTP 与 MCP](docs/http-api.md) · [添加设备](docs/adding-a-device.md) · [烧录](docs/flashing.md) ·
[故障排查](docs/troubleshooting.md) · [智能体循环](docs/agent-loop.md) · [流](docs/agent-stream.md) ·
[技能](docs/agent-skills.md) · [记忆](docs/agent-memory.md) · [脚本](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Performance](docs/performance.md)

## 贡献

新外设从 `examples/manifests/` 下的清单片段开始；只有在涉及时序、校准或状态时才需要 Rust
驱动。驱动在 `CATALOG` 中注册自己。约定见 [AGENTS.md](AGENTS.md) 与
[CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可

MIT。见 [LICENSE](LICENSE)。

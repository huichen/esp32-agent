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
| 一个 shell 和一门脚本语言 | `shell` 有 55 个按 NuttX 命名的命令，还有一个对工具调用做循环的解释器。[Shell](docs/shell.md) · [脚本](docs/scripting.md) |
| 它会记住告诉过它的事 | 常驻指令、`/AGENTS.md`、`/MEMORY.md` 索引与技能，每轮组装进提示词。[模型每轮都能看到什么](docs/context.md) |
| 路径中没有云端 | 由板子提供 HTTPS，证书由它生成，三类调用方一个 PIN。[认证](docs/security.md) |
| 无需硬件即可测试 | 模拟器把同一份 `agent-core` 跑在虚拟板上。[模拟器](docs/simulator.md) |

## 烧录与连接

把 release 里的合并镜像写到偏移 0（或自己编译，见 [docs/flashing.md](docs/flashing.md)）：

```bash
espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin
```

板子里没有凭据时会开一个叫 `esp32-agent-setup` 的热点。用手机连上它，打开
**http://192.168.4.1/**，填入 WiFi 名称和密码。它们存进 NVS，不烧进镜像；而板子要连到
局域网之外的东西（大模型、软件源、别人桌上的 `/agent.json`）就必须先有它们。

## 使用

它只服务两件事。

**人在手机上用。** 打开 `http://<board>/`（或 443 上的 https）：和模型对话、看探测到的设备、
关掉某个工具、翻日志、设 PIN。

**把 `http://<board>/agent.json` 交给一个 agent。** 那份文档就是接口本身：每条路由的方法、
请求体、鉴权，MCP 那一块，流式端点，带类型和默认值的设置表，以及各项上限。一个能取一个
URL 的 agent，不需要读这份 README 的任何一行就能用这块板——这个端点就是为此存在的。

## 给它一个模型

两种方式，同样三个字段。

**在网页上。** 打开页面，Agent 面板，填端点、模型、key，保存。key 只写不读：进 NVS，
读回来是 `__set__`。

**让 agent 填。** 用同样的字段 `POST /api/agent/config`：

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
```

三种协议（`chat`、`responses`、`anthropic`）、思考等级、上下文窗口与输出上限，都在同一个面板
和同一个路由上。完全没有 key 时，把 `base_url` 指向
`http://127.0.0.1/selftest/chat/completions`，循环照样跑，所以没账号也能先验板子。字段表见
[Model](docs/model.md)。

## 安全须知

- **凭据只留在板上。** WiFi 通过 `provision` 写进 NVS，不烧进镜像；`api_key` 只写，读回来是 `__set__`。
- **板子离开你的桌子之前先设 PIN。** `auth.portal_pin` 守住所有写操作，以及会暴露会话记录、提示词、
  配置与日志的读操作。
- **核对证书。** 板子的 TLS 证书是自签的，指纹在 `GET /api/tls`。80 端口默认保持开着，证书坏了只损失一个 URL，不是整块板；把 `mcp.plain_http` 设为 false 就关掉它，那时唯一的退路是 USB 线。
- **`shell`、`script` 与清单工具会动引脚。** 这正是这块板的意义；PIN 是“agent 在干活”和“路人顺手”之间的界线。

[Security](docs/security.md) 写了哪些东西是故意不设防的。

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

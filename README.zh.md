# esp32-agent

[English](README.md) · **简体中文** · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

运行自身智能体循环并提供 MCP 端点的 ESP32 固件，因此一个模型可以直接驱动这块板，任何
外部智能体也能通过 HTTP 使用同一批工具。

烧录、接入 WiFi，它就会探测自己有驱动的总线。每个应答的设备都成为 MCP 工具。板上的智能
体看到同一批工具并用它们作答；新的传感器可以在板子运行期间用 JSON 声明。

> 智能体循环遵循 [FutureOS](https://github.com/futuregene/future-os)：会话记录、工具
> 日志、流事件、协议适配器、记忆索引。

| 要点 | 细节 |
|---|---|
| 智能体跑在板上 | 循环、工具、记忆与会话记录都是固件；模型可以是任何 OpenAI 兼容或 Anthropic 端点。[给它一个模型](docs/model.md) |
| 工具集就是一份文档 | `manifest` (`command: apply`) 校验 JSON 清单，把它编译成工具，同一轮内即生效。[清单](docs/manifest.md) |
| 自己发现硬件 | 启动时探测 I2C、1-Wire 与已声明的引脚；只有应答的才得到工具。[能力](docs/capabilities.md) |
| 一套工具，两个调用方 | MCP 端点与板载智能体看到同一批工具，来自同一份描述。[HTTP 与 MCP API](docs/http-api.md) |
| 通过 HTTPS 的控制台 | 在手机上对话，并配置设备、工具、定时、日志、WiFi 与模型；证书由板子自己签发。[网页控制台](docs/web-console.md) |
| 无人值守也会动作 | 定时提示，以及读数越过阈值时触发提示的传感器规则。[定时任务](docs/scheduling.md) |
| 一个 shell 和一门脚本语言 | `shell` 有 57 个按 NuttX 命名的命令，还有一个对工具调用做循环的解释器。[Shell](docs/shell.md) · [脚本](docs/scripting.md) |
| 它会记住告诉过它的事 | 常驻指令、`/AGENTS.md`、`/MEMORY.md` 索引与技能，每轮组装进提示词。[模型每轮都能看到什么](docs/context.md) |
| 自带技能 | 固件里只带一个流程 `create-skill`，用标准的 `---` 头部书写，而它教的就是剩下的部分：技能是什么、怎么写。首次启动落在 `/skills`，你改过的那份会一直是你的。[技能](docs/agent-skills.md) |
| 板子自己就是服务器 | 由板子提供 HTTPS，证书由它生成，控制台 PIN 与独立的 MCP token 分别鉴权。[认证](docs/security.md) |
| 无需硬件即可测试 | 模拟器把同一份 `agent-core` 跑在虚拟板上。[模拟器](docs/simulator.md) |

## 烧录与连接

从源码构建；目前没有发布预编译的镜像。默认目标是 ESP32-S3 N16R8，带 16 MiB flash 和
8 MiB 八线 PSRAM。按[烧录](docs/flashing.md)安装工具链，然后运行：

```bash
bash scripts/build-firmware.sh --flash --port /dev/ttyACM0
```

Windows 上使用 `powershell -File scripts/build-firmware.ps1 -Flash -Port COM3`。脚本先构建再烧录，
并显式传入分区表和 `factory` 应用分区。

板子里没有 WiFi 凭据，所以它开一个叫 `esp32-agent-setup` 的热点。连上它，打开
`http://192.168.4.1/`，填网络名和密码，保存。凭据进 NVS，板子重连。在这一步完成之前，
它到不了局域网以外的任何地方：模型端点、软件源都到不了。

## 使用

**在浏览器里**：`http://<board>/`（或 `https://<board>/`）就是控制台。和模型对话、看探测到的
设备、开关工具、看日志、设 PIN。

**在 agent 里**：`GET http://<board>/agent.json` 返回接口本身，JSON 形式——每条路由的方法、
请求体、鉴权，MCP 那一块，带类型和默认值的设置表，以及各项上限。能取一个 URL 的 agent
不需要别的东西。

**一块新板子已经会自己学。** `/skills/create-skill` 是唯一内置的流程：它讲技能是什么、必须带什么样的 `---` 头部、怎么写下一个会话能找到的技能。其余的东西——你这张桌子的接线、板子看起来死了该怎么办——由 agent 自己写成技能并保留。它是普通的工作区文件，首次启动时装在路径还空着的地方；恢复出厂会把它装回来，`skills.seed_defaults=false` 可以关掉。对一块刚刷好的板子，第一句有用的话可以是「把你的接线写下来」。

## 给它一个模型

打开 `http://<board>/`，进 **Agent** 面板，填端点、模型 id 和密钥，保存。密钥只写：进 NVS，
读操作返回 `api_key_set`，不返回密钥。同一个面板里有 `protocol`（`chat`、`responses` 或 `anthropic`）、
`reasoning`、`context_tokens` 和 `max_output_tokens`。

**或者交给 agent 去做。** agent 取过 `agent.json` 之后就知道这些设置项、它们的类型和默认值，
所以一句话就够：

> 把板子的模型端点设成 `https://your-provider/v1/chat/completions`，模型 `…`，密钥
> `sk-…`，思考等级 `low`，上下文 `64000`，输出 `4096`

agent 用 `POST /api/agent/config` 写进去——和面板发的是同一个调用——再把配置读回来确认。
你这边不需要敲命令行，同一句话对它能连上的每一块板都成立。完全没有密钥时，把 `base_url`
设成 `http://127.0.0.1/selftest/chat/completions`，循环会拿板子自己当模型跑。

## 安全须知

- **凭据只留在板上。** WiFi 通过 `provision` 写进 NVS，不烧进镜像；`api_key` 只写，读操作只报告 `api_key_set`。
- **板子离开你的桌子之前设好两种凭据。** `auth.portal_pin` 守住 REST 写操作，以及配置、调度、会话记录、
  提示词、工作区、附件、日志、运行事件和脚本结果的读操作。MCP 使用 `auth.mcp_token`；未设置时，
  回退到控制台凭据。
- **核对证书。** 板子的 TLS 证书是自签的，指纹在 `GET /api/tls`。80 端口默认保持开着，证书坏了只损失一个 URL，不是整块板；把 `mcp.plain_http` 设为 false 就关掉它，那时唯一的退路是 USB 线。
- **`shell`、`script` 与清单工具会动引脚。** 这正是这块板的意义；为每种接口配置对应的凭据，才能区分授权调用方与路人。

[Security](docs/security.md) 写了哪些东西是故意不设防的。

## 状态

| 部分 | 状态 |
|---|---|
| `agent-core` | 840 个单元测试与 5 个集成测试通过；`scripts/smoke.ps1` 端到端通过 |
| 模拟器 | 控制台、MCP 与智能体循环端到端运行 |
| 固件 | 历史台架固件在 ESP32-S3-DevKitC-1 N16R8（IDF v5.5.2）上运行：42 个工具、OLED 128×32、I²S 麦克风与功放、WS2812、TLS 控制台、通过 HTTPS 的模型轮次；审计变更尚未通过实板验证 |
| 未实现 / 未验证 | 固件 DVP 摄像头采集尚未实现。SD 原始扇区与 DHT11/22 驱动有主机测试，尚无实板验证。见 `ROADMAP.md` |

历史台架数字：一次用到四个工具的轮次 13.7 s；经 TLS 的控制台 0.55 s；I²C 扫描在 8 次运行中
稳定。早期加载 TLS 页面后内部 SRAM 空闲 5–6 kB 的读数，早于分配器修复（`10b16dd`），不是当前上限。
后续分池测量与来源见[故障排查](docs/troubleshooting.md#how-much-memory-the-board-has)。本次审计固件
尚未烧录或在实板上测试。

## 文档

[架构](docs/architecture.md) · [清单](docs/manifest.md) · [能力](docs/capabilities.md) ·
[HTTP 与 MCP](docs/http-api.md) · [添加设备](docs/adding-a-device.md) · [烧录](docs/flashing.md) ·
[故障排查](docs/troubleshooting.md) · [智能体循环](docs/agent-loop.md) · [流](docs/agent-stream.md) ·
[技能](docs/agent-skills.md) · [记忆](docs/agent-memory.md) · [脚本](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Audio](docs/audio.md) · [审计报告](docs/audit-2026-11.md)

## 贡献

新外设从 `examples/manifests/` 下的清单片段开始；只有在涉及时序、校准或状态时才需要 Rust
驱动。驱动在 `CATALOG` 中注册自己。约定见 [AGENTS.md](AGENTS.md) 与
[CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可

MIT。见 [LICENSE](LICENSE)。

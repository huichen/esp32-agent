# esp32-agent

**简体中文** · [English](README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

运行自身智能体循环并提供 MCP 端点的 ESP32 固件，因此一个模型可以直接驱动这块板，任何
外部智能体也能通过 HTTP 使用同一批工具。

烧录、接入 WiFi，它就会探测自己有驱动的总线。每个应答的设备都成为 MCP 工具。板上的智能
体看到同一批工具并用它们作答；新的传感器可以在板子运行期间用 JSON 声明。

> 智能体循环遵循 [FutureOS](https://github.com/futuregene/future-os)：会话记录、工具
> 日志、流事件、协议适配器、记忆索引。凡固件与它不同之处（RAM 以千字节而非千兆字节计，
> 没有由模型撰写的摘要），代码中说明了原因。

| 要点 | 细节 |
|---|---|
| 模型跑在板上 | 循环、工具、记忆与会话记录都是固件；模型可以是任何 OpenAI 兼容或 Anthropic 端点。[给它一个模型](#给它一个模型) |
| 工具集就是一份文档 | `manifest_apply` 校验 JSON 清单，把它编译成工具，同一轮内即生效。[清单](#清单) |
| 自己发现硬件 | 启动时探测 I2C、1-Wire 与已声明的引脚；只有应答的才得到工具。[能力](docs/capabilities.md) |
| 一套工具，两个调用方 | MCP 端点与板载智能体看到同一批工具，来自同一份描述。[HTTP 与 MCP API](docs/http-api.md) |
| 通过 HTTPS 的控制台 | 在手机上对话，并配置设备、工具、定时、日志、WiFi 与模型；证书由板子自己签发。[网页控制台](#网页控制台) |
| 无人值守也会动作 | 定时提示，以及读数越过阈值时触发提示的传感器规则。[定时任务](#定时任务) |
| 一个 shell 和一门脚本语言 | `shell` 有 26 个按 NuttX 命名的命令，还有一个对工具调用做循环的解释器。[Shell](#shell) · [脚本](#脚本) |
| 它会记住告诉过它的事 | 常驻指令、`/AGENTS.md`、`/memory.md` 索引与技能，每轮组装进提示词。[模型每轮都能看到什么](#模型每轮都能看到什么) |
| 路径中没有云端 | 由板子提供 HTTPS，证书由它生成，三类调用方一个 PIN。[认证](#认证) |
| 无需硬件即可测试 | 模拟器把同一份 `agent-core` 跑在虚拟板上。[模拟器](#模拟器) |

## 烧录与连接

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

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

## 网页控制台

| 位置 | 在那里做什么 |
|---|---|
| 对话（页面） | 与模型交谈；工具调用与推理每轮折叠成一行 |
| 设备 | 探测发现了什么、引脚、地址、`Probe devices`、可采纳的候选 |
| 工具 | 每个工具及其输入 schema，以及某个工具为何被隐藏 |
| 智能体 | 端点、模型、密钥、协议、思考级别、温度、上下文与输出大小 |
| 定时 | 定时提示与传感器规则 |
| 日志、设置 | 环形缓冲区；WiFi 及网关/DNS 覆盖、PIN、重启 |

没有构建步骤，也不从互联网加载任何东西：约 58 KB 的 HTML、CSS 与 JavaScript 编译进固件，
带上 `Cache-Control: no-store` 提供，因此重新烧录在下次刷新时就能看到。

## 给它一个模型

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"write HELLO on the OLED, set the LED blue, then scan I2C"}'
```

三种协议：`chat`（OpenAI chat completions）、`responses`（OpenAI Responses API）与
`anthropic`（Messages API）。控制台附加的图像按 content parts 发出。
`reasoning` 设置思考级别，`max_steps` 限制每轮的模型调用次数（0 表示不限），
`context_tokens` 与 `max_output_tokens` 设置窗口与回复预留，会话记录在窗口用到 80% 时压
缩。没有密钥时，把 `base_url` 指向板子自身的 `/selftest/chat/completions`，循环仍然能跑。

## 模型每轮都能看到什么

系统提示词里有四层，按这个顺序、**每一轮**都重新组装：一部分是主人写下的，一部分是 agent 自己学到的。它们都不在会话记录里，所以对话只有一条消息还是有一千条，代价都一样。

| 层 | 位置 | 送给模型的部分 | 谁写 |
|---|---|---|---|
| 常驻指令 | `agent.system_prompt`（NVS） | 全文，上限 2 KB | 主人，通过 HTTP 或 `shell` |
| 项目指令 | `/AGENTS.md` | 前 1 KB，后面接一句“其余仍在文件里” | 主人，用 `fs_write` |
| 长期记忆 | `/memory.md`，一个指向 `/memory/<主题>.md` 的索引 | 只有索引，24 行 / 2 KB | agent 自己，边做边记 |
| 技能 | `/skills/<名字>/SKILL.md`，以及它旁边的其它文件 | 每个一行，名字与描述 | 两者皆可 |

```bash
# 主人的常驻指令：这块板为什么存在，以及必须先遵守的规矩。
curl -sS -X POST http://<board>/api/agent/prompt -H 'Content-Type: application/json' \
  -d '{"action":"set","text":"水泵继电器在 GPIO12。每次不要超过两分钟。"}'

# 模型究竟会收到什么，四层都在里面；这是唯一能核对的办法。
curl -sS http://<board>/api/agent/prompt | jq -r .system
```

`action` 取 `set`、`append` 或 `clear`；`system_prompt get` 从板上把它读回来，`system_prompt set …` 从 agent 一侧写入。记忆和技能都是普通文件：`fs_write /memory/wiring.md "GPIO18 = 灯带"` 再加 `/memory.md` 里一行，一条记忆就有了；删掉文件就等于忘掉。改完的文件下一轮生效，不必重启、不必重刷、也不必重写会话记录。

之所以分成四层，是因为尺寸：内部 SRAM 只有几 KB 的板子，不可能把“它知道的一切”塞进每个请求，所以每层要么是索引、要么是前缀，真正贵的内容等这一轮用得上时再用 `fs_read` 取。[docs/agent-memory.md](docs/agent-memory.md) 和 [docs/agent-skills.md](docs/agent-skills.md) 写了细节，包括索引如何限长。

## Shell

目录中的每个驱动都在 [docs/capabilities.md](docs/capabilities.md) 中有文档：GPIO、ADC、
PWM、I2C、1-Wire、WS2812、SSD1306、BME280、SHT3x、INA219/226、MPU6050、ADS1115、
PCF8574、DS18B20、HC-SR04、舵机、继电器、I²S 麦克风与功放。

一块可写区域，NVS 中 8 KB 文本。`read`、`write` 与 `edit` 覆盖常见情况；其余都是一个
`shell` 命令行，按词、引号与 `--flags` 解析。工具结果上限 2 KB，回复上限 8 KB：板上有
8 MB PSRAM 与几 KB 内部 SRAM，起约束作用的是第二个数字：

```text
shell {"command":"ls /notes"}            shell {"command":"grep GPIO /"}
shell {"command":"cp /a.txt /b.txt"}     shell {"command":"rm /a.txt --confirm"}
shell {"command":"curl https://example.com/"}          # GET, HTML reduced to text
shell {"command":"wget -o /dl/page.txt https://example.com/"}
shell {"command":"ping -c 3 192.168.3.1"}   shell {"command":"nslookup example.com"}
shell {"command":"ifconfig"}                # configured gateway and DNS, not the lease
shell {"command":"df | free | uptime | date | uname | ps"}
shell {"command":"sha256sum /notes/a.txt"}  shell {"command":"zip t.zip /a.txt"}
```

`edit` 遇到有歧义的匹配就拒绝，而不是改错位置；`rm` 与 `mv` 需要 `confirm`。写错的命令会
返回它的用法行，用法行取自帮助文本所用的同一张表，因此两者不可能互相矛盾：
`rm without --confirm` 返回 `usage: rm <path> --confirm`。`shell {"command":"help"}` 列出
全部 26 个。

## 脚本

用于固定工具无法表达的事情：对一组读数的循环、对结果的分支。

```text
let scan = tool("i2c_scan", {})
let i = 0
while i < len(scan.addresses) { log(scan.addresses[i])  i = i + 1 }
if i > 0 { "found " + str(i) } else { "nothing" }
```

数字、字符串、布尔、nil、列表、映射；`let`、赋值、`if`/`else`、`while`、`break`；内置函
数 `len`、`str`、`num`、`type`、`log` 与 `tool(name, args)`，后者以调用方自身的权限调用
任意板载工具（`tool("script")` 除外）。`{"lint":true}` 检查语法与未知名称，给出行与列，
且从不执行；`{"help":true}` 返回参考。边界：30 s、五百万步、32 层嵌套块、4 KB 字符串、
8 KB 源码。真正的边界是时间：解释器跑在应答控制台的那个线程上。

## 定时任务

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "tasks":[{"id":"t1","prompt":"read the BME280","every_s":300}],
  "rules":[{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,"prompt":"it is hot"}]}'
```

`at_hhmm` 改为按 UTC 挂钟时间运行，而每个回答都以 `source: "schedule:t1"` 标记落入会话
记录。智能体可以用 `schedule {"action":"add", …}` 给自己安排任务。任务在重启后保留。

## 清单

不需要时序或校准的设备靠声明，而不是靠编译：

```json
{"devices":[{"id":"pump","kind":"actuator","pins":{"out":12},"note":"active-low relay"}],
 "tools":[{"name":"pump_pulse","description":"Energise the relay for `ms` ms",
   "params":[{"name":"ms","type":"integer","min":10,"max":5000,"required":true}],
   "exec":{"type":"sequence","steps":[
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":0}},
     {"ms":0,"exec":{"type":"delay","ms":{"param":"ms"}}},
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":1}}]},
   "safety":{"max_calls_per_min":20,"confirm":true}}]}
```

添加硬件有三条路径：清单（不需要时序或状态机，在线生效）、编译驱动
（`drivers/<chip>.rs`，用于状态机与严格时序），或板级配置（无法探测的事实，例如 LED 与
按键用哪些引脚）。只有当探测到设备时才列出驱动，因此写了一半的驱动无法报告并不存在的硬
件。校验在板上运行：参数范围与 `safety` 的每分钟上限会在引脚动作之前检查，越界的调用会
返回它能接受的范围。[docs/adding-a-device.md](docs/adding-a-device.md) 覆盖这三条。

## 认证

| 调用方 | 凭据 |
|---|---|
| 控制台 | `POST /api/auth {"pin":"…"}` 返回页面保存的 bearer token |
| 脚本 | `X-Portal-Pin: <pin>`，或同一个 token |
| MCP 客户端 | `Authorization: Bearer <mcp.token>`；未设置 token 时接受 PIN |

设置了 `auth.portal_pin` 之后，写入以及暴露会话记录、提示词、配置与日志的读取都需要它；
状态、设备、工具与 `/agent.json` 保持开放。token 存放在 RAM 中，因此重启即吊销。默认没有
PIN，`/agent.json` 在其 `security` 块中说明了这一点。

## 模拟器

```bash
cargo run -p esp32-agent-sim
```

同一份 `agent-core` 对接真实端口上的虚拟板。只有 `Hardware` 实现不同，因此控制台、MCP
端点、驱动、清单引擎与智能体循环就是运行在芯片上的那份代码。

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
| `agent-core` | 424 个单元测试与 14 个集成测试通过；`scripts/smoke.ps1` 端到端通过 |
| 模拟器 | 控制台、MCP 与智能体循环端到端运行 |
| 固件 | 在 ESP32-S3-DevKitC-1 N16R8（IDF v5.5.2）上烧录：42 个工具、OLED 128×32、I²S 麦克风与功放、WS2812、TLS 控制台、通过 HTTPS 的模型轮次 |
| 未实现 | OTA、SD 卡、DHT11/22、摄像头。见 `ROADMAP.md` |

台架数字：一次用到四个工具的轮次 13.7 s；经 TLS 的控制台 0.55 s；I²C 扫描在 8 次运行中
稳定；加载一次 TLS 页面后内部 SRAM 空闲 5–6 kB，这是这里最紧的约束。

## 文档

[架构](docs/architecture.md) · [清单](docs/manifest.md) · [能力](docs/capabilities.md) ·
[HTTP 与 MCP](docs/http-api.md) · [添加设备](docs/adding-a-device.md) · [烧录](docs/flashing.md) ·
[故障排查](docs/troubleshooting.md) · [智能体循环](docs/agent-loop.md) · [流](docs/agent-stream.md) ·
[技能](docs/agent-skills.md) · [记忆](docs/agent-memory.md) · [脚本](docs/scripting.md)

## 贡献

新外设从 `examples/manifests/` 下的清单片段开始；只有在涉及时序、校准或状态时才需要 Rust
驱动。驱动在 `CATALOG` 中注册自己。约定见 [AGENTS.md](AGENTS.md) 与
[CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可

MIT。见 [LICENSE](LICENSE)。

# ESP32 固件语言选型：C++ / Rust / 其他语言的深度调研

- 调研日期：2026-11-09
- 范围：ESP32 系列（Xtensa 与 RISC-V）固件与片上应用层语言
- 方法：以厂商文档、官方仓库、同行评审论文与一线生产复盘为主；所有关键数字标注来源与证据强度
- 结论口径：语言选择不是"哪个更好"，而是"在哪个层次、由谁维护、承受多大风险"

---

## 0. 一页结论

| 你的处境 | 首选 | 说明 |
|---|---|---|
| 量产 IoT：Wi-Fi/BLE + 配网 + OTA + 认证 | **ESP-IDF（C/C++）** | 唯一"第一天就有全部功能"的路径 |
| 量产但重度使用 Rust，可接受少量缺口 | **Rust std（esp-idf-hal/svc）** | 复用 IDF 的 Wi-Fi/HTTP/OTA，Rust 写业务 |
| 新项目、重安全/可维护、团队会 Rust、可接受 beta | **Rust no_std（esp-hal + embassy）** | 资源最省、控制最强，但 Wi-Fi 仍 beta |
| 原型/PoC/教学/低频采集 | **MicroPython** | 迭代最快；性能差 2–3 个数量级 |
| Go 团队、简单 HTTP/MQTT 服务、C3/S3 | **TinyGo 0.41+** | 2026 年才补上 Wi-Fi，覆盖面仍窄 |
| 硬实时、极小资源、要确定性 | **C（IDF 或裸机）/ Rust no_std** | 避开 GC 与解释器 |
| 需要业务逻辑热更新 | **C/C++ 宿主 + Lua/JS/WASM 插件** | 或 MicroPython 整体脚本化 |
| 实验/研究 | **Zig** | 官方无 ESP32 支持，MicroZig 仍 WIP |

一句话：**C++ 赢在生态与"第一天可用"，Rust 赢在正确性与可维护性，MicroPython 赢在迭代速度，其他语言都还是小众选项。**

---

## 1. 先厘清层次：ESP32 上其实有四层"语言"

很多讨论把不同层混在一起，导致结论失真。ESP32 上至少存在四个可替换的层：

1. **芯片与 ROM**：BootROM、无线固件 blob、硬件加速器。所有语言最终都调同一套 blob。
2. **SDK/运行时**：ESP-IDF（C，FreeRTOS + newlib）、Arduino-ESP32（C++ 封装 IDF）、esp-hal（Rust，no_std，不用 FreeRTOS）、MicroPython 解释器（跑在 IDF 上）、TinyGo runtime（自带调度器与 GC）。
3. **HAL/驱动层**：IDF 驱动、esp-idf-hal、esp-hal、machine 模块、TinyGo machine 包。
4. **应用层**：业务逻辑、协议解析、HTTP/MQTT、OTA。

关键事实（都影响选型）：

- ESP-IDF **本身是 C 写的、对外提供 C API**；C++ 是"被支持"而非"原生"[1]。
- Arduino-ESP32 **是 IDF 之上的一层 C++ 封装**，当前 3.3.12 基于 IDF 5.5，而 IDF 稳定版已到 6.1 —— 封装层天然滞后[17]。
- Rust 有**两条互不相同的路线**：`esp-hal`（no_std、不用 IDF、Espressif 付费团队维护）与 `esp-idf-*`（std、包 IDF、社区维护）[2][5]。
- MicroPython 在 ESP32 上**跑在 ESP-IDF + FreeRTOS 之上**，不是裸机[10]。
- 所以"用 Rust"并不等于"摆脱 ESP-IDF"；"用 C++"也不等于"用 Arduino"。

---

## 2. C / C++（ESP-IDF 与 Arduino）

### 优势

1. **厂商一等公民，新芯片零等待。** C5、P4、S31 等新芯片、新外设、无线协议栈更新都先在 IDF 落地；Rust/TinyGo 需要跟进移植。
2. **生态体量最大。** ESP Component Registry 有官方 + 社区组件[18]，Arduino 库以万计，ESP-ADF（音频）、ESP-WHO（视觉）、ESP-SR（语音）、esp-matter、RainMaker、LVGL 示例都以 C/C++ 为首发语言。找现成代码、改现成工程的时间成本最低。
3. **工具链与调试最成熟。** OpenOCD/JTAG、GDB、core dump、heap tracing、app trace、`idf.py size/monitor`、Wokwi 仿真；出问题时有官方论坛与大量同构案例。
4. **C++ 能力完整度比想象中高。** IDF 默认 `-std=gnu++26`；`std::thread/mutex/condition_variable` 通过 pthread 封装 FreeRTOS 可用；`std::filesystem` 基本可用（符号链接、权限等少数例外）[1]。
5. **性能天花板。** 与 C 同级；本调研引用的基准里 C/C++ 在 5 个 DSP 核中 4 个最快[9]。
6. **可控与确定性。** 静态分配、任务优先级、直接寄存器访问、关闭动态分配都能做到，硬实时场景路径清晰。
7. **人才与资料。** 招聘、外包、StackOverflow/CSDN/视频/AI 语料都最多，团队交接成本最低。

### 劣势

1. **内存安全靠人。** 悬垂指针、越界、use-after-free、数据竞争在 520 KB SRAM、无进程隔离的芯片上是"一次错误 = 一次重启"，且现场难复现。
2. **异常与 RTTI 默认关闭**（可在 menuconfig 打开）：开启异常增加数 KB 二进制、需要 emergency pool、抛出时多用约 200 B 栈且展开"慢几个数量级"；RTTI 增加数十 KB[1]。结果是大量 C++ 代码被迫退回错误码风格，标准库与第三方库的假设经常对不上。
3. **Arduino 层质量参差。** `String` 堆碎片、库之间抢外设、核心版本与 IDF 脱节（3.3.12 / IDF 5.5 vs IDF 6.1）[17]；"能跑 demo"与"能过量产"差距大。
4. **构建系统重。** CMake + 组件依赖 + sdkconfig 全局配置耦合；组件间 ABI/配置漂移；全量编译慢。
5. **现代 C++ 在 MCU 上"能用不好用"。** STL 容器隐式分配、模板膨胀、编译错误难读；嵌入式实践普遍要裁剪。
6. **并发正确性无编译期保障。** FreeRTOS 原语 + C++ 对象 + 中断上下文混用，靠规范、code review、MISRA/clang-tidy 兜底，没有强制力。
7. **C++ 在 IDF 里是"客人"**：跨语言要 `extern "C"`，异常不能穿过 C 栈帧，很多官方组件只有 C 接口[1]。

### 适合
量产设备、需要完整无线/配网/OTA/协议栈、团队是 C/C++、要最快拿到可用固件。

---

## 3. Rust

### 两条路线必须先分清

| | `esp-hal`（no_std） | `esp-idf-hal/svc`（std） |
|---|---|---|
| 底座 | 不用 ESP-IDF，直接操作外设 | 包住 ESP-IDF，链接其 C 库 |
| 维护方 | **Espressif 付费 Rust 团队**，1.0 于 2025-10 发布[2] | **社区维护**，"Espressif 几乎不投入带薪人力"[5] |
| 稳定性 | 1.0 只稳定 init + 4 个核心驱动 + time + `#[main]`；其余在 `unstable` 后面[2] | 0.x，落后 IDF 版本，无 HIL 测试，文档偏薄[5] |
| 无线 | `esp-radio`：Wi-Fi/BLE/ESP-NOW/802.15.4，**需开 `unstable`**，1.0.0-beta.1（2026-09）[4] | 直接用 IDF 的 Wi-Fi/BLE/HTTP/MQTT/WS/NVS/OTA 封装[5] |
| 运行时 | 无 RTOS；async + embassy 或裸轮询 | FreeRTOS + pthread + std |
| 体积/内存 | 最省，自己管分配器与 panic | 明显更重（newlib、pthread、std） |
| 首次构建 | 快（只编 Rust） | 慢（下载并编译 IDF + `build-std`）[7] |
| 目标架构 | Xtensa 为 **Tier 3**，需 `espup` 装 Rust fork + LLVM fork + GCC 链接器[6][8]；RISC-V 为 Tier 2，官方 rustup 即可 | 同样 Tier 3，另需 `ldproxy` + `build-std`[7] |

### 优势

1. **编译期内存与线程安全。** 所有权/借用把 use-after-free、数据竞争、缓冲区越界挡在编译期；外设所有权用类型编码（type-state），重复初始化、引脚冲突直接编译失败。
2. **零成本抽象、无 GC。** 基准中 5 个核的几何平均为 C 的 **1.21 倍**（单核最好 0.81 倍，即更快）[9]。
3. **Cargo 带来的工程性。** 依赖解析、特性开关、锁文件、workspace、`clippy`、`rustfmt`、以及**在 PC 上跑单元测试**——协议解析这类纯逻辑可以完全不碰硬件地测。
4. **类型系统适合协议。** `enum` + `Result` + 模式匹配表达 JSON-RPC/MCP 的状态机与错误分支，比 C 的返回码 + 出参清晰得多。
5. **async 生态。** embassy + esp-hal 的 async 驱动，事件驱动、低功耗、无 RTOS 也能并发；`esp-radio` 的 async 接口与 Embassy executor 兼容[2]。
6. **驱动可复用。** `embedded-hal` / `embedded-hal-async` trait 让传感器驱动跨芯片复用。
7. **厂商背书在变强。** esp-hal 是"第一个厂商背书的 Rust SDK"，承诺核心 API 的 SemVer 稳定[2]；有生产案例：STABL Energy 在 ESP32 上用 Rust（std 路线）超过一年，称"比 C 版可靠得多，基本零调试时间"[15]。

### 劣势

1. **应用层积木缺口。** no_std 路线没有现成的 HTTP server / OTA / 配网 / MQTT 全家桶，要自己拼 `smoltcp`/`picoserve`/`edge-http`；std 路线虽然有 HTTP/WS/MQTT/OTA[5]，但那是社区 crate。
2. **稳定性分层。** 1.0 只覆盖核心；其余驱动标 `unstable`，**minor 版本可能破坏**，官方建议用 `~1.1` 这种约束并盯着迁移指南[3]。
3. **无线仍在 beta。** Wi-Fi/BLE 在 esp-hal 侧要求 `unstable`，esp-radio 才到 1.0.0-beta.1；BLE 官方推荐外部 `TrouBLE` 栈[4]。
4. **工具链摩擦。** Xtensa 是 Tier 3、无 rustup 分发，必须 `espup` 装 fork；`build-std`、`ldproxy`、链接脚本、flashing 配置都要理解；std 路线首次构建要编译 IDF，耗时以十分钟计[5][7][8]。
5. **学习曲线陡。** 借用检查 + 外设 move 语义 + async + no_std（分配器、panic handler）+ 链接脚本，嵌入式 Rust 比桌面 Rust 更难；生产复盘明确说"Rust 写起来比 C 慢"，且"把 C 程序员转成 Rust 不划算"[15]。
6. **招人与外包难。** 同一篇复盘的核心结论就是维护者难找[15]。
7. **安全收益有边界。** HAL、FFI、DMA 描述符里仍是 `unsafe`；学术研究发现嵌入式 Rust 生态支持不足、SAST 工具对嵌入式 Rust 失效、高级类型让互操作工程变难（2,836 个 crate + 225 名开发者调查）[16]。
8. **文档/示例相对少、API 变动快**，AI 语料明显少于 C++。

### 适合
新项目、对可靠性与长期维护有要求、团队已有 Rust、芯片选 RISC-V（C3/C6）或愿意接受 S3/C5 的 Tier 3 摩擦。

---

## 4. MicroPython / CircuitPython

### 优势
1. **迭代速度无对手**：REPL 即时执行、`mpremote` 推文件、改一行不用编译烧录。
2. **高层库齐全**：`machine`、`network`、`socket`、`ssl`、`aioble`(BLE)、`umqtt`、`requests`；Wi-Fi 直接复用 IDF。
3. **技能复用**：Python 开发者零门槛；教学与科研原型首选。
4. **部署简单**：`.py` 文件即应用，OTA/热更可以就是拷文件。
5. **可下沉**：热点用 native/viper emitter 或 C 模块（`.mpy`/frozen）加速[11]。

### 劣势
1. **慢 2–3 个数量级**：本调研基准中 5 个核为 C 的 302–1891 倍，几何平均 **539 倍**[9]。
2. **GC 停顿毫秒级**，且堆碎片会让长时间运行的程序越来越难分配[11]；不适合硬实时与高吞吐。
3. **内存紧**：解释器常驻 + 堆；TLS、大 JSON、多缓冲在 520 KB SRAM 上很容易触顶。
4. **错误运行时才暴露**，大型工程的可维护性与重构能力弱。
5. **调试弱**：主要靠 `print`/REPL，无断点、无内存剖析闭环。
6. **并发弱**：asyncio 单线程为主，`_thread` 能力有限。
7. **生产故事弱**：可靠性、认证、长期维护都要自己补。

### 适合
原型、教学、低频采集、网关胶水、把设备当"可编程传感器"的场景。

---

## 5. TinyGo

### 优势
1. **Go 语言与工具链**：goroutine/channel 表达并发自然，`go test`、`gofmt` 成熟。
2. **编译型、性能接近 C**：同一基准几何平均 **1.22 倍**，个别项 0.95 倍[9]。
3. **二进制小、GC 简单**（TinyGo 的 GC 面向 MCU 裁剪）。
4. **2026 年补上无线**：0.41（2026-04）通过 `espradio` 支持 C3/S3 的 Wi-Fi，`net/http`、MQTT 可直接跑；BLE 标注进行中[12][13]。
5. **可直接烧录**（`espflasher`），无需外部工具[12]。

### 劣势
1. **支持面窄且晚**：Wi-Fi 先覆盖 C3/S3；经典 ESP32 与 BLE 在 espradio 仓库里列出但官方发布说明更保守[12][13]。
2. **GC 停顿**仍在，硬实时不适用。
3. **标准库子集**，反射/`net` 等功能有限。
4. **外设与驱动覆盖不如 IDF**：ADC、USB、低功耗模式等常缺。
5. **调试体验一般**，GDB 支持有限。
6. **生态与人才池小**，遇到问题可求助的人少。

### 适合
Go 团队做中小型联网设备、以 HTTP/MQTT 为主的业务、C3/S3 平台。

---

## 6. 其他语言（简要）

| 语言 | 现状 | 结论 |
|---|---|---|
| **Zig** | 无官方 ESP32 支持；MicroZig 的 espressif port 目录里明确标注 `[WIP]`，整个项目自称"开发中、API 必坏"[14]；现实路径是把 Zig 当 C 编译器配合 ESP-IDF | 实验/研究可以，产品不建议 |
| **C#（.NET nanoFramework）** | 解释器方案，VS 调试、NuGet 生态；历史上长期卡在 IDF 3.3，跟进慢[19] | 小众，适合 .NET 团队做非关键设备 |
| **Lua（NodeMCU 等）** | 引擎小、可热更；生态萎缩、性能与内存差 | 存量/教学 |
| **JavaScript（Espruino / Moddable XS）** | 动态、可脚本化；性能与内存受限 | 小众，适合 UI/脚本化场景 |
| **Ada / SPARK** | 高保证语言，但无 ESP32 运行时/BSP | 不适用 |
| **WASM（WAMR 等）** | 可做沙箱插件与多语言扩展；宿主仍需 C/C++/Rust | 作为"插件层"而非主语言 |

---

## 7. 性能：数字怎么读

同一块 M5Stack（ESP32-D0WDQ6，160 MHz），5 个 DSP 核，每项 100 次取中位数；C 用 `-Os`，Rust/TinyGo 用 `-Oz`[9]（原始 CSV 见仓库）。

长度 1024 时（µs，括号为相对 C 的倍数）：

| 核 | C | Rust | TinyGo | MicroPython |
|---|---|---|---|---|
| CRC32 | 116 | 114 (0.98×) | 110 (0.95×) | 47,032 (**405×**) |
| SHA-256 | 705 | 999 (1.42×) | 1,039 (1.47×) | 1,332,934 (**1891×**) |
| FFT i16 | 1,823 | 3,512 (1.93×) | 3,331 (1.83×) | 844,856 (**463×**) |
| IIR biquad f32 | 212 | 252 (1.19×) | 225 (1.06×) | 90,172 (**425×**) |
| FIR f32 | 30,054 | 24,221 (0.81×) | 29,934 (1.00×) | 9,066,359 (**302×**) |
| **几何平均** | 1.00 | **1.21×** | **1.22×** | **539×** |

读法：

- **Rust/TinyGo 与 C 的差距在 20% 量级**，且方向不固定（FIR 上 Rust 更快）。这个差距在"Wi-Fi + TLS + JSON"的真实负载里通常被协议栈淹没。
- **MicroPython 的 300–1900 倍是"解释执行 + 每次操作装箱/分配"的必然结果**；如果业务是每秒几次传感器读取，根本感觉不到；如果是滤波、加密、图像，直接不可用。
- 基准的局限要说清：2022 年的工具链、手写移植（非各语言惯用写法）、只测 5 个 DSP 核、160 MHz、不含无线与网络、Rust 走的是当时的 esp-idf 路线。**不能把 1.21× 当作 esp-hal 的精确系数**，只能当作"同一量级"的证据。

---

## 8. 安全与可靠性：Rust 到底买到了什么

**买到的：**
- 编译期的所有权/借用检查，消灭一整类内存错误与数据竞争；外设与引脚的所有权模型把"配置错"提前到编译期。
- 无 GC、无隐藏分配（no_std 路线），时序更可预测。
- 重构友好：改 API 后编译器会把所有调用点揪出来。

**没买到的：**
- ESP32 没有应用处理器那种带权限隔离的 MMU，"一个野指针毁掉整个系统"的硬件前提没变。
- HAL/FFI/DMA 仍是 `unsafe`；驱动层 bug 依旧能造成内存不安全。
- esp-hal 的 `unstable` 驱动会随 minor 版本变化；esp-radio 还在 beta。**"API 不稳定"本身就是一种生产风险**。
- 学术研究显示嵌入式 Rust 的软件支持不足、静态分析工具对嵌入式 Rust 失效、互操作工程困难[16]。

**C++ 侧的对冲手段**：MISRA/AUTOSAR C++ 规范、静态分析、ASan（桌面侧）、防御性分配、看门狗、双分区 OTA 回滚。这些在 IDF 里都有现成支持，成熟度高于 Rust 侧。

---

## 9. 混合架构：不一定要"全押一种语言"

这是被低估的选项，且往往是工程上最优：

1. **IDF（C/C++）做底座 + Rust 做业务**：`esp-idf-sys` 明确支持混合 Rust/C 工程[5]；Wi-Fi/OTA/配网用 IDF，协议与状态机用 Rust，Rust 侧还能在 PC 上单测。
2. **C/C++ 宿主 + 脚本插件**：Lua/JS/WASM 做可热更的业务规则，宿主保持稳定。
3. **MicroPython + C 模块**：`native`/`viper` 或 `.mpy` 把热点下沉[11]。
4. **Rust no_std 核心 + C 驱动**：`bindgen` 包厂商 C 驱动，业务层保持安全。

代价是构建复杂度与两套工具链，收益是各取所长。

---

## 10. 针对本仓库（esp32-agent）的具体建议

现状：Rust workspace（`mcp-core`、`mcp-sim` 可在主机跑，`firmware` 尚未成文）。MCP 的本质是 **JSON-RPC 2.0 over HTTP/SSE（或 stdio）**，负载是文本 + JSON，QPS 很低。这意味着：

- **语言性能不是瓶颈**，TLS、内存、连接管理、OTA 才是。
- **协议逻辑应当与硬件解耦**——这一点比选语言更重要。

建议按优先级：

1. **保持 `mcp-core` 为纯逻辑、`no_std + alloc` 友好、可在主机 `cargo test`。** 这是当前仓库结构最大的资产：固件语言再怎么选，协议解析、工具路由、JSON 序列化都能在 PC 上回归测试。
2. **固件路线 A（推荐，若目标是尽快可用）：`esp-idf-hal` + `esp-idf-svc`（std）。** 直接获得 Wi-Fi、HTTP server、WebSocket、NVS、OTA 封装[5]；业务用 Rust 写；风险是这些 crate 是社区维护、落后 IDF、无 HIL 测试[5]，需要用锁版本 + 薄封装层隔离。
3. **固件路线 B（推荐，若追求最小体积与完全控制）：`esp-hal` + `embassy` + `esp-radio`（`unstable`）。** 自己拼 HTTP（`picoserve`/`edge-http`）与 OTA；芯片优先选 **C3/C6（RISC-V，Tier 2）**，避免 Xtensa 的 Tier 3 工具链摩擦。
4. **不建议**为 MCP 服务选 MicroPython 做量产固件：JSON + TLS + 并发连接会同时撞上性能与内存墙；可以做一个 MicroPython 版 demo 用于教学/演示。
5. **若必须用经典 ESP32（Xtensa）**：Rust 的工具链与生态摩擦最大，此时 C++/IDF 的性价比明显更高；或者换 S3/C3。
6. **TLS 策略**：在 520 KB SRAM 上做完整 TLS + 大 JSON 很紧，优先"网关终止 TLS、设备走局域网明文/预共享密钥"，或选带 PSRAM 的模组。

---

## 11. 证据强度与未解问题

**强证据（一手文档/代码仓库）**
- ESP-IDF 的 C++ 支持边界、异常/RTTI 默认值与成本[1]
- esp-hal 1.0 的稳定范围、esp-radio 的 beta 状态与 `unstable` 要求[2][4]
- esp-idf-* 为社区维护、落后 IDF、无 HIL 测试[5]
- Xtensa/ESP-IDF 目标为 Tier 3，需 `build-std`/`ldproxy`/`espup`[6][7][8]
- MicroPython 跑在 IDF/FreeRTOS 上、GC 停顿毫秒级[10][11]
- TinyGo 0.41 的 Wi-Fi 覆盖范围[12][13]
- MicroZig espressif port 的 `[WIP]`[14]

**中等证据**
- 基准数字（2022 工具链、5 个核、非惯用写法）[9]
- 生产复盘（单公司、单项目）[15]
- 学术调查（2023–2024 数据，生态已变化）[16]

**弱证据/未解**
- esp-hal/esp-radio 何时 1.0 稳定、路线图节奏（官方未给日期）
- 各语言在 ESP32 上的长期缺陷率/维护成本，没有公开对照数据
- Rust 嵌入式人才市场的量化数据（只有个案描述）
- 无线/网络/TLS 负载下的跨语言性能，没有找到严谨对照实验
- AI 辅助编码对各语言在 ESP32 上的实际效率差异（语料偏差明显，但缺量化研究）

**反证提示（避免结论过强）**
- "Rust 更安全"在驱动层会被 `unsafe` 与 C 底座稀释；Rust 生态自身也有稳定性与文档问题[16]。
- "C++ 更慢/更危险"并不成立于性能维度——它在多数基准里最快[9]；风险主要在工程纪律。
- "MicroPython 太慢"对低频 IoT 常常不成立；它的真实短板是内存与确定性，而非吞吐。

---

## 12. 参考资料

1. Espressif. *C++ Support*, ESP-IDF v6.1. https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-guides/cplusplus.html
2. Espressif. *Announcing esp-hal 1.0* (2025-10). https://developer.espressif.com/blog/2025/10/esp-hal-1/
3. esp-rs. *esp-hal README*（支持芯片、`unstable`、发布策略）. https://github.com/esp-rs/esp-hal
4. esp-rs. *esp-radio README*（Wi-Fi/BLE 支持矩阵、`unstable` 要求）. https://github.com/esp-rs/esp-hal/tree/main/esp-radio
5. esp-rs. *Rust on ESP-IDF README*（社区维护声明、std/HTTP/MQTT/OTA 能力）. https://github.com/esp-rs/esp-idf
6. Rust. *Platform Support: xtensa-\*-none-elf*（Tier 3）. https://doc.rust-lang.org/rustc/platform-support/xtensa.html
7. Rust. *Platform Support: ESP-IDF*（Tier 3、build-std、ldproxy）. https://doc.rust-lang.org/rustc/platform-support/esp-idf.html
8. esp-rs. *The Rust on ESP Book — Toolchain Installation*. https://docs.espressif.com/projects/rust/book/getting-started/toolchain.html
9. Plauska, I.; Liutkevičius, A.; Janavičiūtė, A. *Performance Evaluation of C/C++, MicroPython, Rust and TinyGo Programming Languages on ESP32 Microcontroller.* Electronics 2023, 12(1), 143. https://doi.org/10.3390/electronics12010143 ；原始数据 https://github.com/ignasp/ProgLangComp_onESP32
10. MicroPython. *General information about the ESP32 port*. https://docs.micropython.org/en/latest/esp32/general.html
11. MicroPython. *Maximising MicroPython speed*. https://docs.micropython.org/en/latest/reference/speed_python.html
12. TinyGo. *TinyGo 0.41 — The Big Release* (2026-04). https://tinygo.org/blog/2026/tinygo-0-41-the-big-release/
13. tinygo-org. *espradio*. https://github.com/tinygo-org/espradio
14. ZigEmbeddedGroup. *MicroZig*（含 `port/espressif/esp` 的 `[WIP]` 标注）. https://github.com/ZigEmbeddedGroup/microzig
15. Lohr, M. *Embedded Rust in Production ..?* https://blog.lohr.dev/embedded-rust
16. Sharma, A. et al. *Rust for Embedded Systems: Current State, Challenges and Open Problems.* arXiv:2311.05063. https://arxiv.org/abs/2311.05063
17. Espressif. *Arduino-ESP32 Documentation*（3.3.12 / IDF 5.5）. https://docs.espressif.com/projects/arduino-esp32/en/latest/
18. Espressif. *IDF Component Manager and ESP Component Registry*. https://docs.espressif.com/projects/idf-component-manager/en/latest/
19. .NET nanoFramework. https://nanoframework.net/ ；ESP32 支持历史 https://nanoframework.net/improved-support-for-esp32/
20. FOSDEM 2026. *From C to Rust on the ESP32: A Developer's Journey*. https://archive.fosdem.org/2026/schedule/event/GNNQFP-converting-c-firmware-to-rust/

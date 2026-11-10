# ESP32 firmware language choice: C++ / Rust / others

- Research date: 2026-11-09
- Scope: firmware and on-chip application languages for the ESP32 family (Xtensa and RISC-V)
- Method: vendor documentation, official repositories, peer-reviewed papers and first-hand
  production retrospectives; every key number carries its source and strength of evidence
- Stance: the question is not "which language is better" but "at which layer, maintained by
  whom, and carrying how much risk"

> Note added 2026-11: this is a dated decision record, kept as written. The decision it
> led to — the `std` route (`esp-idf-hal`/`esp-idf-svc`) — is now implemented and running
> on hardware; see `docs/flashing.md` and the ROADMAP. Only path names and the "current
> state" paragraph have been brought up to date.

---

## 0. One-page summary

| Your situation | First choice | Why |
|---|---|---|
| Production IoT: Wi-Fi/BLE + provisioning + OTA + certification | **ESP-IDF (C/C++)** | the only path where everything exists on day one |
| Production, heavily invested in Rust, small gaps acceptable | **Rust std (esp-idf-hal/svc)** | reuse IDF's Wi-Fi/HTTP/OTA, write the logic in Rust |
| New project, safety and maintainability first, team knows Rust, beta acceptable | **Rust no_std (esp-hal + embassy)** | smallest footprint, most control, but Wi-Fi is still beta |
| Prototype / PoC / teaching / low-rate sensing | **MicroPython** | fastest iteration; 2–3 orders of magnitude slower |
| Go team, simple HTTP/MQTT service, C3/S3 | **TinyGo 0.41+** | Wi-Fi only arrived in 2026, coverage still narrow |
| Hard real time, tiny footprint, determinism | **C (IDF or bare metal) / Rust no_std** | no GC, no interpreter |
| Business logic needs hot reload | **C/C++ host + Lua/JS/WASM plugins** | or script everything in MicroPython |
| Experimentation / research | **Zig** | no official ESP32 support, MicroZig is still WIP |

In one sentence: **C++ wins on ecosystem and day-one availability, Rust on correctness and
maintainability, MicroPython on iteration speed, and everything else is still a niche.**

---

## 1. Layers first: ESP32 actually has four "language" layers

Most discussions mix layers together, which distorts the conclusion. At least four layers
on an ESP32 are replaceable:

1. **Chip and ROM**: BootROM, the wireless firmware blobs, hardware accelerators. Every
   language ends up calling the same blobs.
2. **SDK/runtime**: ESP-IDF (C, FreeRTOS + newlib), Arduino-ESP32 (a C++ wrapper over IDF),
   esp-hal (Rust, no_std, no FreeRTOS), the MicroPython interpreter (running on IDF), the
   TinyGo runtime (its own scheduler and GC).
3. **HAL/driver layer**: IDF drivers, esp-idf-hal, esp-hal, the `machine` module, TinyGo's
   `machine` package.
4. **Application layer**: business logic, protocol parsing, HTTP/MQTT, OTA.

Facts that matter for the choice:

- ESP-IDF **is written in C and exposes a C API**; C++ is "supported", not native [1].
- Arduino-ESP32 **is a C++ wrapper on top of IDF**: 3.3.12 is based on IDF 5.5 while IDF
  stable is already at 6.1, so the wrapper inherently lags [17].
- Rust has **two distinct routes**: `esp-hal` (no_std, no IDF, maintained by Espressif's
  paid team) and `esp-idf-*` (std, wraps IDF, community maintained) [2][5].
- MicroPython on ESP32 **runs on ESP-IDF + FreeRTOS**, not on bare metal [10].
- So "using Rust" does not mean "escaping ESP-IDF", and "using C++" does not mean "using
  Arduino".

---

## 2. C / C++ (ESP-IDF and Arduino)

### Strengths

1. **Vendor first-class citizen, zero wait for new chips.** New chips such as C5, P4 and
   S31, new peripherals and wireless stack updates land in IDF first; Rust/TinyGo have to
   port afterwards.
2. **The largest ecosystem.** The ESP Component Registry carries official and community
   components [18], the Arduino library count is in the tens of thousands, and ESP-ADF
   (audio), ESP-WHO (vision), ESP-SR (speech), esp-matter, RainMaker and the LVGL examples
   all ship C/C++ first. Finding code and adapting an existing project is cheapest here.
3. **The most mature toolchain and debugging.** OpenOCD/JTAG, GDB, core dump, heap tracing,
   app trace, `idf.py size/monitor`, Wokwi simulation; when something breaks there is an
   official forum and a large body of similar cases.
4. **C++ support is more complete than people assume.** IDF defaults to `-std=gnu++26`;
   `std::thread/mutex/condition_variable` work through a pthread wrapper over FreeRTOS, and
   `std::filesystem` mostly works (symlinks and permissions are the exceptions) [1].
5. **Performance ceiling.** On par with C; in the benchmark cited here C/C++ is fastest in
   four of five DSP kernels [9].
6. **Control and determinism.** Static allocation, task priorities, direct register access
   and disabling dynamic allocation are all available, so the hard-real-time path is clear.
7. **People and material.** Hiring, outsourcing, StackOverflow, videos and AI training data
   are most plentiful, so handover is cheapest.

### Weaknesses

1. **Memory safety is a human discipline.** Dangling pointers, overruns, use-after-free and
   data races on a 520 KB SRAM chip without process isolation mean "one mistake, one
   reboot", and it is hard to reproduce in the field.
2. **Exceptions and RTTI are off by default** (they can be enabled in menuconfig): enabling
   exceptions costs several KB of binary, needs an emergency pool, takes about 200 B more
   stack per throw and unwinds "orders of magnitude slower"; RTTI costs tens of KB [1]. The
   result is that much C++ falls back to error codes, and standard-library assumptions
   often do not line up.
3. **Arduino quality varies.** `String` heap fragmentation, libraries fighting over
   peripherals, a core version detached from IDF (3.3.12 / IDF 5.5 vs IDF 6.1) [17]; the gap
   between "runs a demo" and "ships to production" is wide.
4. **A heavy build system.** CMake plus component dependencies plus global sdkconfig
   coupling; ABI and configuration drift between components; slow full builds.
5. **Modern C++ on an MCU is usable but not pleasant.** Implicit allocation in STL
   containers, template bloat, unreadable compile errors; embedded practice demands
   trimming.
6. **No compile-time guarantee of concurrency correctness.** FreeRTOS primitives, C++
   objects and interrupt context mixed together are held together by convention, review and
   MISRA/clang-tidy, none of which is enforced.
7. **C++ is a guest inside IDF**: crossing the boundary needs `extern "C"`, exceptions
   cannot cross C stack frames, and many official components expose only a C interface [1].

### Fits
Production devices that need the full wireless/provisioning/OTA/protocol stack, C/C++
teams, and getting to usable firmware fastest.

---

## 3. Rust

### The two routes must be kept apart

| | `esp-hal` (no_std) | `esp-idf-hal/svc` (std) |
|---|---|---|
| Base | no ESP-IDF; drives peripherals directly | wraps ESP-IDF and links its C libraries |
| Maintained by | **Espressif's paid Rust team**, 1.0 released 2025-10 [2] | **the community**; "Espressif invests almost no paid headcount" [5] |
| Stability | 1.0 stabilises only init plus four core drivers plus time plus `#[main]`; the rest sits behind `unstable` [2] | 0.x, lags IDF, no HIL testing, thin documentation [5] |
| Wireless | `esp-radio`: Wi-Fi/BLE/ESP-NOW/802.15.4, **requires `unstable`**, 1.0.0-beta.1 (2026-09) [4] | IDF's own Wi-Fi/BLE/HTTP/MQTT/WS/NVS/OTA wrappers [5] |
| Runtime | no RTOS; async with embassy, or bare polling | FreeRTOS + pthread + std |
| Size/memory | smallest; you own the allocator and the panic handler | noticeably heavier (newlib, pthread, std) |
| First build | fast (Rust only) | slow (downloads and compiles IDF, plus `build-std`) [7] |
| Target | Xtensa is **Tier 3**: `espup` installs a Rust fork, an LLVM fork and a GCC linker [6][8]; RISC-V is Tier 2 and works with stock rustup | same Tier 3, and additionally `ldproxy` + `build-std` [7] |

### Strengths

1. **Compile-time memory and thread safety.** Ownership and borrowing keep use-after-free,
   data races and buffer overruns out of the build; peripheral ownership is encoded in the
   type system (type-state), so double initialisation or a pin conflict fails to compile.
2. **Zero-cost abstraction, no GC.** Across five kernels the geometric mean is **1.21×** C
   (best single kernel 0.81×, i.e. faster) [9].
3. **Engineering that comes with Cargo.** Dependency resolution, feature flags, lock files,
   workspaces, `clippy`, `rustfmt`, and **unit tests on the PC** — pure logic such as
   protocol parsing can be tested without touching hardware.
4. **A type system that suits protocol work.** `enum` + `Result` + pattern matching express
   JSON-RPC/MCP state machines and error branches far more clearly than return codes with
   out-parameters.
5. **An async ecosystem.** embassy plus esp-hal async drivers give event-driven,
   low-power concurrency without an RTOS; esp-radio's async interface is compatible with
   the Embassy executor [2].
6. **Reusable drivers.** The `embedded-hal` / `embedded-hal-async` traits let sensor drivers
   cross chips.
7. **Vendor backing is growing.** esp-hal is "the first vendor-backed Rust SDK" and commits
   to SemVer stability for its core API [2]; there is a production case: STABL Energy has run
   Rust on ESP32 (the std route) for over a year and reports it as "far more reliable than
   the C version, essentially zero debugging time" [15].

### Weaknesses

1. **Missing application-layer building blocks.** The no_std route has no ready-made HTTP
   server, OTA, provisioning or MQTT suite; you assemble `smoltcp`/`picoserve`/`edge-http`
   yourself. The std route has HTTP/WS/MQTT/OTA [5], but those are community crates.
2. **Tiered stability.** 1.0 covers the core only; the remaining drivers are marked
   `unstable` and **a minor release may break them**, so the guidance is to pin something
   like `~1.1` and watch the migration guide [3].
3. **Wireless is still beta.** Wi-Fi/BLE on the esp-hal side require `unstable`; esp-radio
   is only at 1.0.0-beta.1, and BLE is officially recommended through the external
   `TrouBLE` stack [4].
4. **Toolchain friction.** Xtensa is Tier 3 with no rustup distribution, so `espup` and a
   fork are mandatory; `build-std`, `ldproxy`, linker scripts and flashing configuration all
   have to be understood; a first std-route build compiles IDF and takes tens of minutes
   [5][7][8].
5. **A steep learning curve.** Borrow checking plus peripheral move semantics plus async
   plus no_std (allocator, panic handler) plus linker scripts makes embedded Rust harder
   than desktop Rust; that production retrospective says outright that Rust is slower to
   write than C and that converting C programmers to Rust does not pay [15].
6. **Hard to hire and outsource.** The same retrospective's core conclusion is that
   maintainers are hard to find [15].
7. **The safety benefit has a boundary.** HAL, FFI and DMA descriptors are still `unsafe`;
   academic work finds the embedded Rust ecosystem under-supported, SAST tools ineffective
   on it, and advanced types making interoperability harder (2,836 crates and 225 developers
   surveyed) [16].
8. **Fewer docs and examples, fast-moving APIs**, and markedly less AI training material
   than C++.

### Fits
New projects that need reliability and long-term maintenance, teams that already know Rust,
and RISC-V chips (C3/C6) — or willingness to accept Tier 3 friction on S3/C5.

---

## 4. MicroPython / CircuitPython

### Strengths
1. **Unmatched iteration speed**: a REPL executes immediately, `mpremote` pushes files, and
   changing a line needs no compile or flash.
2. **Complete high-level libraries**: `machine`, `network`, `socket`, `ssl`, `aioble` (BLE),
   `umqtt`, `requests`; Wi-Fi reuses IDF directly.
3. **Skill reuse**: zero barrier for Python developers; the default for teaching and research
   prototypes.
4. **Simple deployment**: a `.py` file is the application, and OTA/hot reload can just be
   copying files.
5. **It can drop to native**: hot paths use the native/viper emitter or a C module
   (`.mpy`/frozen) [11].

### Weaknesses
1. **2–3 orders of magnitude slower**: across the five kernels in this benchmark it is
   302–1891× C, geometric mean **539×** [9].
2. **GC pauses are milliseconds long**, and heap fragmentation makes allocation progressively
   harder in long-running programs [11]; unsuitable for hard real time and high throughput.
3. **Tight memory**: a resident interpreter plus heap; TLS, large JSON and multiple buffers
   hit the ceiling easily on 520 KB SRAM.
4. **Errors surface at runtime**, so large projects are hard to maintain and refactor.
5. **Weak debugging**: mostly `print`/REPL, with no breakpoints and no memory profiling loop.
6. **Weak concurrency**: asyncio on one thread, with limited `_thread` capability.
7. **A thin production story**: reliability, certification and long-term maintenance are all
   on you.

### Fits
Prototypes, teaching, low-rate sensing, gateway glue, and treating the device as a
"programmable sensor".

---

## 5. TinyGo

### Strengths
1. **The Go language and toolchain**: goroutines and channels express concurrency naturally,
   and `go test` and `gofmt` are mature.
2. **Compiled, close to C**: geometric mean **1.22×** on the same benchmark, with one case at
   0.95× [9].
3. **Small binaries and a simple GC** (TinyGo's GC is trimmed for MCUs).
4. **Wireless arrived in 2026**: 0.41 (2026-04) supports Wi-Fi on C3/S3 through `espradio`,
   so `net/http` and MQTT run directly; BLE is marked in progress [12][13].
5. **It flashes directly** (`espflasher`) without external tools [12].

### Weaknesses
1. **Narrow and late support**: Wi-Fi covers C3/S3 first; classic ESP32 and BLE are listed in
   the espradio repository while the official release notes are more conservative [12][13].
2. **GC pauses remain**, so hard real time is out.
3. **A subset of the standard library**; reflection and parts of `net` are limited.
4. **Peripheral and driver coverage trails IDF**: ADC, USB and low-power modes are often
   missing.
5. **Mediocre debugging**, with limited GDB support.
6. **A small ecosystem and talent pool**, so there are few people to ask.

### Fits
Go teams building small or mid-size connected devices with HTTP/MQTT-heavy workloads on
C3/S3.

---

## 6. Other languages (briefly)

| Language | State | Conclusion |
|---|---|---|
| **Zig** | no official ESP32 support; MicroZig's espressif port directory is explicitly marked `[WIP]` and the project calls itself "in development, the API will break" [14]; the realistic path is Zig as a C compiler alongside ESP-IDF | fine for experiments, not for products |
| **C# (.NET nanoFramework)** | an interpreter approach with VS debugging and NuGet; historically stuck on IDF 3.3 and slow to follow [19] | niche; suits .NET teams building non-critical devices |
| **Lua (NodeMCU etc.)** | a small engine and hot reload; a shrinking ecosystem, poor performance and memory | legacy and teaching |
| **JavaScript (Espruino / Moddable XS)** | dynamic and scriptable; limited by performance and memory | niche; suits UI and scripting |
| **Ada / SPARK** | high-assurance language, but no ESP32 runtime or BSP | not applicable |
| **WASM (WAMR etc.)** | good for sandboxed plugins and multi-language extension; the host still needs C/C++/Rust | a plugin layer, not a primary language |

---

## 7. Performance: how to read the numbers

The same M5Stack board (ESP32-D0WDQ6 at 160 MHz), five DSP kernels, median of 100 runs each;
C at `-Os`, Rust/TinyGo at `-Oz` [9] (raw CSV in the repository).

At length 1024 (µs; the parenthesised figure is the multiple of C):

| Kernel | C | Rust | TinyGo | MicroPython |
|---|---|---|---|---|
| CRC32 | 116 | 114 (0.98×) | 110 (0.95×) | 47,032 (**405×**) |
| SHA-256 | 705 | 999 (1.42×) | 1,039 (1.47×) | 1,332,934 (**1891×**) |
| FFT i16 | 1,823 | 3,512 (1.93×) | 3,331 (1.83×) | 844,856 (**463×**) |
| IIR biquad f32 | 212 | 252 (1.19×) | 225 (1.06×) | 90,172 (**425×**) |
| FIR f32 | 30,054 | 24,221 (0.81×) | 29,934 (1.00×) | 9,066,359 (**302×**) |
| **Geometric mean** | 1.00 | **1.21×** | **1.22×** | **539×** |

How to read it:

- **Rust/TinyGo are within about 20% of C**, and not always slower (Rust wins on FIR). Under a
  real "Wi-Fi + TLS + JSON" workload that gap is usually buried by the protocol stack.
- **MicroPython's 300–1900× is the inevitable result of interpretation plus boxing and
  allocation per operation.** A workload that reads a sensor a few times a second will never
  notice; filtering, encryption or imaging is simply out of reach.
- The benchmark's limits must be stated: 2022 toolchains, hand-written ports (not idiomatic
  for each language), only five DSP kernels, 160 MHz, no wireless or networking, and Rust on
  the esp-idf route of that time. **1.21× is not a precise coefficient for esp-hal**; it is
  only evidence of "the same order of magnitude".

---

## 8. Safety and reliability: what Rust actually buys

**What you get:**
- Compile-time ownership and borrow checking, eliminating a whole class of memory errors and
  data races; the peripheral and pin ownership model moves configuration mistakes to compile
  time.
- No GC and no hidden allocation (no_std), so timing is more predictable.
- Refactoring is friendly: change an API and the compiler finds every call site.

**What you do not get:**
- The ESP32 has no application-processor MMU with privilege isolation, so "one stray pointer
  destroys the system" remains a hardware premise.
- HAL, FFI and DMA are still `unsafe`; a driver bug can still cause memory unsafety.
- esp-hal's `unstable` drivers change across minor versions, and esp-radio is still beta.
  **API instability is itself a production risk.**
- Academic work reports insufficient tooling support for embedded Rust, static analysis that
  does not work on it, and difficult interoperability [16].

**The C++ countermeasures**: MISRA/AUTOSAR C++ rules, static analysis, ASan (on the desktop
side), defensive allocation, watchdog, dual-slot OTA rollback. All of these are supported
out of the box in IDF, more maturely than on the Rust side.

---

## 9. Hybrid architectures: you need not commit to one language

This is an underrated option, and often the best engineering answer:

1. **IDF (C/C++) as the base + Rust for logic**: `esp-idf-sys` explicitly supports mixed
   Rust/C projects [5]; use IDF for Wi-Fi/OTA/provisioning and Rust for protocol and state
   machines, which can then be unit-tested on the PC.
2. **C/C++ host + scripted plugins**: Lua/JS/WASM for hot-reloadable rules while the host
   stays stable.
3. **MicroPython + C modules**: sink the hot path with `native`/`viper` or `.mpy` [11].
4. **Rust no_std core + C drivers**: wrap vendor C drivers with `bindgen` and keep the logic
   layer safe.

The cost is build complexity and two toolchains; the benefit is the best of each.

---

## 10. Recommendations for this repository (esp32-agent)

Current state: a Rust workspace where `agent-core` and `agent-sim` run on the host and
`firmware/` builds and runs on an ESP32-S3-DevKitC-1 N16R8 (ESP-IDF v5.5.2, the `std` route
described below). MCP is essentially **JSON-RPC 2.0 over HTTP** with text and JSON payloads
and very low QPS. That means:

- **Language performance is not the bottleneck**; TLS, memory, connection management and OTA
  are.
- **Protocol logic must be decoupled from hardware** — that matters more than the language.

Recommended in priority order:

1. **Keep `agent-core` pure logic, `no_std + alloc` friendly, and `cargo test`-able on the
   host.** That is this repository's biggest asset: whatever the firmware language, protocol
   parsing, tool routing and JSON serialisation can be regression-tested on a PC.
2. **Firmware route A (recommended when the goal is to be usable soon): `esp-idf-hal` +
   `esp-idf-svc` (std).** It gives Wi-Fi, an HTTP server, WebSocket, NVS and OTA wrappers out
   of the box [5]; write the logic in Rust. The risk is that those crates are community
   maintained, lag IDF and have no HIL tests [5], so pin versions and isolate them behind a
   thin layer.
3. **Firmware route B (recommended when minimal size and full control matter): `esp-hal` +
   `embassy` + `esp-radio` (`unstable`).** Assemble HTTP (`picoserve`/`edge-http`) and OTA
   yourself; prefer **C3/C6 (RISC-V, Tier 2)** to avoid Xtensa's Tier 3 toolchain friction.
4. **Do not** choose MicroPython for production MCP firmware: JSON, TLS and concurrent
   connections hit both the performance and the memory wall at once. A MicroPython demo for
   teaching is fine.
5. **If classic ESP32 (Xtensa) are mandatory**: Rust's toolchain and ecosystem friction is
   greatest there, and C++/IDF is clearly better value; or move to S3/C3.
6. **TLS strategy**: full TLS plus large JSON is tight in 520 KB of SRAM, so prefer
   terminating TLS at a gateway and letting the device speak plaintext or pre-shared keys on
   the LAN — or choose a module with PSRAM.

---

## 11. Evidence strength and open questions

**Strong evidence (primary documentation or repositories)**
- The boundaries of IDF's C++ support, and the default state and cost of exceptions/RTTI [1]
- The stable scope of esp-hal 1.0, and esp-radio's beta status and `unstable` requirement [2][4]
- esp-idf-* being community maintained, lagging IDF, and lacking HIL tests [5]
- Xtensa/ESP-IDF targets being Tier 3 and needing `build-std`/`ldproxy`/`espup` [6][7][8]
- MicroPython running on IDF/FreeRTOS, with millisecond GC pauses [10][11]
- TinyGo 0.41's Wi-Fi coverage [12][13]
- MicroZig's espressif port being `[WIP]` [14]

**Medium evidence**
- The benchmark numbers (2022 toolchains, five kernels, non-idiomatic ports) [9]
- The production retrospective (one company, one project) [15]
- The academic survey (2023–2024 data; the ecosystem has moved since) [16]

**Weak evidence / unresolved**
- When esp-hal/esp-radio will be fully stable, and the roadmap cadence (no dates published)
- Long-term defect rates and maintenance cost per language on ESP32: no public comparative data
- Quantified data on the embedded Rust talent market (only anecdotal descriptions)
- Cross-language performance under wireless/network/TLS load: no rigorous controlled
  experiment found
- How much AI-assisted coding actually helps per language on ESP32 (the training-data bias is
  obvious, but there is no quantitative study)

**Counter-arguments (to keep the conclusion from overreaching)**
- "Rust is safer" is diluted at the driver layer by `unsafe` and the C base, and the Rust
  ecosystem has its own stability and documentation problems [16].
- "C++ is slower or more dangerous" does not hold on the performance axis — it is fastest in
  most of the benchmark [9]; the risk is chiefly engineering discipline.
- "MicroPython is too slow" often does not hold for low-rate IoT; its real weaknesses are
  memory and determinism, not throughput.

---

## 12. References

1. Espressif. *C++ Support*, ESP-IDF v6.1. https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-guides/cplusplus.html
2. Espressif. *Announcing esp-hal 1.0* (2025-10). https://developer.espressif.com/blog/2025/10/esp-hal-1/
3. esp-rs. *esp-hal README* (supported chips, `unstable`, release policy). https://github.com/esp-rs/esp-hal
4. esp-rs. *esp-radio README* (Wi-Fi/BLE support matrix, `unstable` requirement). https://github.com/esp-rs/esp-hal/tree/main/esp-radio
5. esp-rs. *Rust on ESP-IDF README* (community-maintained notice, std/HTTP/MQTT/OTA support). https://github.com/esp-rs/esp-idf
6. Rust. *Platform Support: xtensa-\*-none-elf* (Tier 3). https://doc.rust-lang.org/rustc/platform-support/xtensa.html
7. Rust. *Platform Support: ESP-IDF* (Tier 3, build-std, ldproxy). https://doc.rust-lang.org/rustc/platform-support/esp-idf.html
8. esp-rs. *The Rust on ESP Book — Toolchain Installation*. https://docs.espressif.com/projects/rust/book/getting-started/toolchain.html
9. Plauska, I.; Liutkevičius, A.; Janavičiūtė, A. *Performance Evaluation of C/C++, MicroPython, Rust and TinyGo Programming Languages on ESP32 Microcontroller.* Electronics 2023, 12(1), 143. https://doi.org/10.3390/electronics12010143 ; raw data https://github.com/ignasp/ProgLangComp_onESP32
10. MicroPython. *General information about the ESP32 port*. https://docs.micropython.org/en/latest/esp32/general.html
11. MicroPython. *Maximising MicroPython speed*. https://docs.micropython.org/en/latest/reference/speed_python.html
12. TinyGo. *TinyGo 0.41 — The Big Release* (2026-04). https://tinygo.org/blog/2026/tinygo-0-41-the-big-release/
13. tinygo-org. *espradio*. https://github.com/tinygo-org/espradio
14. ZigEmbeddedGroup. *MicroZig* (including the `[WIP]` marker on `port/espressif/esp`). https://github.com/ZigEmbeddedGroup/microzig
15. Lohr, M. *Embedded Rust in Production ..?* https://blog.lohr.dev/embedded-rust
16. Sharma, A. et al. *Rust for Embedded Systems: Current State, Challenges and Open Problems.* arXiv:2311.05063. https://arxiv.org/abs/2311.05063
17. Espressif. *Arduino-ESP32 Documentation* (3.3.12 / IDF 5.5). https://docs.espressif.com/projects/arduino-esp32/en/latest/
18. Espressif. *IDF Component Manager and ESP Component Registry*. https://docs.espressif.com/projects/idf-component-manager/en/latest/
19. .NET nanoFramework. https://nanoframework.net/ ; ESP32 support history https://nanoframework.net/improved-support-for-esp32/
20. FOSDEM 2026. *From C to Rust on the ESP32: A Developer's Journey*. https://archive.fosdem.org/2026/schedule/event/GNNQFP-converting-c-firmware-to-rust/

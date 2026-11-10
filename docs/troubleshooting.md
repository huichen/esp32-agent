# 排错

先看 `GET /api/logs`（或网页"日志"页）。启动阶段的每一行都有 `target`（`boot`/`probe`/`wifi`/`http`/`tool`/`manifest`）。

## 板子起不来

| 现象 | 检查 |
|---|---|
| 串口没有任何输出 | 控制台不是 USB-Serial/JTAG？改 `CONFIG_ESP_CONSOLE_*`；波特率 115200 |
| `rst:0x…` 后立刻重启 | 看 `panic` 行；多半是 PSRAM 配置与板子不符（`CONFIG_SPIRAM_MODE_OCT`） |
| `i2c bus 0 (SDA 8, SCL 9) failed` | 引脚不存在或已被占用；改 `config.bus.i2c` 或在网页里换引脚 |
| 日志里 `no stored config` | 正常：第一次上电，走热点配网 |

## 配网

| 现象 | 检查 |
|---|---|
| 手机看不到 `esp32-agent-setup` | 板子有旧凭证 → 串口 `wifi: joining ...`；`POST /api/wifi/forget` 后再重启 |
| 连上热点但打不开页面 | 必须是 `http://192.168.4.1:8080/`，注意端口；捕获式 DNS 只帮 HTTP 自动弹窗 |
| 填完密码连不上 | 2.4 GHz + WPA2 才稳（ESP32 不支持 5 GHz）；看日志 `wifi: status=…`；检查密码是否被复制带入空格 |
| 连上了但 IP 找不到 | 路由器 DHCP 列表按 hostname `esp32-agent` 找；或串口日志里的 `ip=` |

## 设备看不见

1. `POST /api/devices/probe`，或模型侧调 `devices_probe` 再 `devices_list`。
2. `i2c_scan` 看地址在不在：
   - 一个都没有 → 接线/上拉/供电；SDA/SCL 是否接反；模块是否 5 V 供电而板子是 3.3 V 逻辑。
   - 有地址但 `unidentified` → `i2c_identify {addr}` 看候选；必要时用 `i2c_read` 自己读 ID 寄存器。
3. 地址对但驱动没认：地址冲突很常见（0x76/0x77、0x3C/0x3D、0x44/0x45）。看日志里驱动给出的失败原因。
4. 1-Wire 设备没出现：1-Wire 只在 `bus.onewire_pins` 里列出的引脚上探测，且必须有 4.7 kΩ 上拉。

## 工具没出现在 `tools/list`

- `GET /api/tools`：每一项有 `visible` 与 `missing`。`missing` 非空就是硬件没检测到。
- 用户禁用了？`mcp.disabled_tools`。
- `tools_mode = "generic"` 时设备工具被折叠，改用 `device_invoke`，或切回 `auto`。
- 清单工具：确认 `enabled` 不是 `false`，名字合法（`a-z0-9_-` 与 `.`）。

## 清单应用失败

返回的 `issues[].path` 是 JSON Pointer，直接定位：
- `/tools/0/exec/pin` → 引脚越界
- `/tools/2/name` → 名字非法或重复
- `/devices/1/driver` → 驱动 id 不在 `docs/capabilities.md` 的表里
- `sequence` 报总时长 → 减少 `ms` 或步数

## 越界调用被拒

这是设计：`safety.min/max` 与 `params.min/max` 都在板子上检查。
消息里带范围，例如 `` `ms` = 90000 is outside 10..=5000 ms ``。改参数，不要改固件。

## 屏幕不亮 / 花屏

- 128x32 与 128x64 认错会花屏：设 `pins["oled.height"]`，或清单里 `options.height`。
- SH1106 仿制品：`options.controller = "sh1106"`（列偏移 2）。
- 全是噪点：先 `oled_contrast 0xCF`，再确认供电与 4.7 kΩ 上拉。

## 性能与稳定性

| 现象 | 原因/处理 |
|---|---|
| 连接被拒（503） | 并发连接上限 6；关掉浏览器里的旧标签 |
| 一次工具调用 > 500 ms | 日志会 warn；I2C 设备慢或 1-Wire 转换（DS18B20 固定 750 ms） |
| 内存不足 | 降 `log.ring`、减少并发、把 `tools_mode` 切 `generic` |
| 会话越积越多 | `session_ttl_s`（默认 900 s）与 `max_sessions`（默认 4）自动回收 |

## 模拟器

- `cargo run -p esp32-agent-sim -- --port 18080 --state sim-state/kv.json`
- `--empty`：一块空板子（验证"没硬件时工具列表为空"）。
- 想回到初始状态：删 `sim-state/kv.json`。

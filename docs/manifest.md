# 清单（manifest）规范 v1

清单是存在 NVS `manifest` 键里的一段 JSON。它决定"这块板子把哪些硬件暴露成哪些工具"。
写完调用 `manifest_apply` 即生效；`tools/list` 立刻变化，不需要重启，也不需要重烧。

## 1. 顶层

```json
{ "version": 1, "devices": [ … ], "tools": [ … ] }
```

| 字段 | 说明 |
|---|---|
| `version` | 目前固定 `1`，用于将来迁移 |
| `devices` | 设备声明。给了 `driver` 就会实例化编译好的驱动；不给则只是记录引脚/地址 |
| `tools` | 工具声明。这是模型能看到的接口 |

上限：`tools ≤ 48`，`sequence.steps ≤ 8`，单次调用累计 `delay ≤ 5000 ms`。

## 2. devices

```json
{ "id": "bme0", "kind": "sensor", "driver": "bme280",
  "bus": 0, "address": 118, "pins": { "out": 12 },
  "options": { "shunt_mohm": "100" }, "enabled": true, "note": "焊在背面" }
```

- `driver` 可用值见 `docs/capabilities.md` 的"已实现"表。
- 给了 `driver` 但探测失败时，该设备跳过并在日志里说明原因（不会让整份清单失败）。
- `options` 是字符串表，驱动自己的参数（OLED 高度、INA219 分流电阻…）放这里。

## 3. tools

```json
{ "name": "pump.pulse", "title": "水泵脉冲", "description": "…",
  "device": "pump", "enabled": true,
  "params": [ … ], "exec": { … }, "safety": { … } }
```

- `name`：`a-z0-9_-` 与 `.`，最长 48，必须唯一。
- `description` 是模型唯一能看到的自然语言，写清**单位与副作用**。
- `params`：`{name, type: integer|number|string|boolean, required, min, max, unit, enum, default, description}`。
  这些范围会写进 JSON Schema，**同时在板子上强制**。

## 4. 原语（exec）

值字段可以是字面量，也可以是 `{"param": "名字"}` 从调用参数取值。

| type | 字段 | 说明 |
|---|---|---|
| `gpio_write` | `pin`, `level` | level 0/1 或布尔参数（true = 1） |
| `gpio_read` | `pin` | 返回 `{pin, level}` |
| `toggle` | `pin`, `times`, `gap_ms` | 闪烁，times ≤ 20 |
| `pwm` | `pin`, `freq_hz`, `duty_pct` | 舵机 50 Hz + 占空比；LED 1–5 kHz |
| `adc` | `pin` | 返回毫伏（未校准，单调可用） |
| `i2c_read` | `addr`, `reg`, `length`, `bus?` | 返回 hex 与原始字节 |
| `i2c_write` | `addr`, `reg`, `data`(数组/hex/`{"param":…}`), `bus?` | 原始写 |
| `rgb` | `pin`, `color`(`#rrggbb` 或 param), `count?` | WS2812 |
| `delay` | `ms` | 阻塞等待 |
| `driver` | `driver`, `action` | 调驱动自己的工具，例如 `bme280_read` |
| `sequence` | `steps: [{ms, exec}]` | 组合；`ms` 是本步之后的等待 |

`sequence` 是"基础能力组合"的入口：脉冲继电器、闪灯再读光敏、舵机扫一遍取值，都用它。

## 5. safety（在板子上强制）

```json
"safety": { "min": 5, "max": 175, "max_calls_per_min": 30, "confirm": true }
```

- `min/max`：对**任何数值参数**生效，与 `params[].min/max` 独立。schema 说 0..180，safety 说 5..175，
  那么 180 会被拒绝——这一层是给"模型幻觉"兜底的。
- `max_calls_per_min`：滑动窗口限频，防止模型循环里把继电器当鼓敲。
- `confirm: true`：调用必须带 `"confirm": true`。

错误码：参数越界 `-32602`；安全范围/确认缺失/限频 `-32005`（forbidden），消息里带范围。

## 6. 校验与错误

`manifest_apply` 返回：

```json
{ "applied": false,
  "issues": [ { "path": "/tools/0/exec/pin", "message": "pin 99 is outside 0..=48", "hint": "ESP32-S3 has GPIO0..GPIO48…" } ] }
```

`path` 是 JSON Pointer，`hint` 是可执行的修法建议。**校验不通过时什么都没改**。
合法则返回 `{applied: true, revision, tools_added, tools_removed, restart_required: false}`。

## 7. 合并与回滚

- `mode: "merge"`：按 `tools[].name` / `devices[].id` 覆盖同名项，其余保留（日常迭代用这个）。
- `mode: "replace"`：整份替换（从文件导入时用）。
- `dry_run: true`：只校验，返回 `would_add` / `would_remove`。
- `manifest_reset`：清空，仅保留内置工具。
- 上一版清单不会自动备份：需要回滚就重新 `manifest_apply` 一份旧的（从 `mcp://manifest` 资源可读取当前值）。

## 8. 例子

见 `examples/manifests/`：`relay-pump.json`、`servo-scan.json`、`i2c-sensor-raw.json`、`led-indicator.json`。
每个文件都能直接作为 `manifest` 参数提交（`mode: merge`）。

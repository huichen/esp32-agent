# 清单示例

每个文件都是可以直接提交给 `manifest_apply`（`mode: merge`）的**完整参数对象**，也可以复制到网页
"清单"页里应用。

| 文件 | 需要什么硬件 | 说明 |
|---|---|---|
| `relay-pump.json` | GPIO12 上的继电器（低电平吸合） | 开关 + 定时脉冲，`confirm` 与限频都开着 |
| `servo-scan.json` | GPIO7 上的舵机 | 角度设置 + 一次往返扫描；`safety` 比 `params` 更窄（5–175 度） |
| `i2c-sensor-raw.json` | 0x40 上一个假设的光照传感器 | 直接用 `i2c_read` 读原始值，换算写进 description |
| `led-indicator.json` | GPIO48 上的 WS2812 | 状态灯 + 闪烁提示，`enum` 参数限定预设 |

## 写自己的示例

1. 先 `inventory` / `i2c_scan` 确认真实地址与引脚。
2. 数值范围写两遍：`params[].min/max` 给模型看，`safety.min/max` 在板上强制。
3. 有物理动作的加 `max_calls_per_min`；不可逆的动作加 `confirm: true`。
4. 单位写进 `description` 与 `unit`——模型只能看到这些。
5. 纯 JSON，不能有注释；需要解释就用 `devices[].note` 或把说明写进 `description`。

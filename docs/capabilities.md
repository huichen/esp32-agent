# 能力、驱动与设备

两根轴：**能力（capability）** 是工具用来判断"能不能出现"的字符串；**驱动（driver）** 是能实例化的芯片代码。

## 1. 自动识别怎么发生

启动时（以及每次 `devices_probe` / `/api/devices/probe`）：

1. 按 `config.bus.i2c` 扫描每条 I2C 总线，得到应答地址表。
2. 按固定顺序尝试驱动，**用寄存器证据确认身份**，避免只凭地址撞车：
   - SSD1306/SH1106：0x3C/0x3D 应答 → 写初始化序列（这类屏不可回读）
   - BME280/BMP280：`0xD0` = 0x60/0x58
   - SHT3x：0x44/0x45 发单次测量并校验 CRC
   - INA219/INA226：`0xFE` = 0x5449（INA226）
   - MPU-6050/6500：`0x75` ∈ {0x68,0x69,0x70,0x71,0x73}
   - ADS1115：0x48–0x4B 读一次转换且在量程内
   - PCF8574：写 0xFF 再读回 0xFF（LCD 背包）
3. 1-Wire：只扫 `config.bus.onewire_pins` 里列出的引脚（在任意引脚上打复位脉冲不是无害操作），
   认 presence pulse + ROM CRC。
4. 声明式外设（继电器/舵机/HC-SR04/LED/按键）：来自板型或 `config.pins`，因为无法被动探测。
5. 没被任何驱动认领的地址 → 进 `unidentified`，`i2c_identify` 会去读常见 id 寄存器给候选。

## 2. 已实现驱动

| driver | 设备 | 识别 | 工具 | 备注 |
|---|---|---|---|---|
| `ssd1306` | SSD1306 / SH1106 128x32/128x64 | 地址 0x3C/0x3D | `oled_text` `oled_clear` `oled_rect` `oled_bitmap` `oled_contrast` | 6x8 内嵌字库；`options.controller=sh1106`、`options.height` |
| `pcf8574` | PCF8574 扩展 + HD44780 1602 | 写 0xFF 回读 | `pcf8574_read/write` `lcd_text` `lcd_clear` `lcd_backlight` | 4 位模式，P0=RS P2=EN P3=背光 |
| `bme280` | BME280 / BMP280 | chip id | `bme280_read` | 温度/气压/湿度/露点/海拔 |
| `sht3x` | SHT30/31/35 | 测量 + CRC | `sht3x_read` | 与 BME280 同址时优先 BME280 |
| `ina219` | INA219 / INA226 | 0x5449 / 地址 | `power_read` | `options.shunt_mohm`，默认 100 mΩ |
| `mpu6050` | MPU-6050/6500 | WHO_AM_I | `mpu6050_read` | 加速度/陀螺/倾角，可切量程 |
| `ads1115` | ADS1115 16-bit ADC | 转换在量程内 | `ads1115_read` | 单端 0–3 或差分 0-1/2-3，可选增益 |
| `ds18b20` | DS18B20/DS18S20 | presence + ROM CRC | `ds18b20_read` | 需 4.7 kΩ 上拉；`pins.data` |
| `hcsr04` | HC-SR04 / JSN-SR04T | 首次 ping | `distance_cm` | `pins.trig` / `pins.echo` |
| `servo` | 舵机 | 声明引脚 | `servo_angle` | 50 Hz，500–2500 µs |
| `relay` | 继电器 | 声明引脚 | `relay_set` `relay_pulse` | `options.active_low=true` |
| `led` | WS2812 / 普通 / PWM | 板型或声明 | `led_set` `led_blink` | `options.kind=ws2812|pwm|gpio`、`count` |

## 3. 计划中（欢迎认领）

`dht11/dht22`、`rc522`（RFID）、`vl53l0x`（ToF）、`sdcard`、`inmp441`（I2S 麦克风）、`max98357a`（I2S 功放）、
`bme680`（气体）、`sgp30`、`tsl2561`（光照）、`hmc5883`（磁力计）、`pn532`、`max6675`（热电偶）、
`aht20`、`bh1750`、`sg90` 多路、`step`（步进电机）。

## 4. 板型（`boards.rs`）

板型记录"探测不到的事实"：I2C 引脚、板载 LED 的 GPIO 与类型、按键、I2S 麦/放引脚、可用 ADC 引脚、保留引脚。

| id | 板子 | I2C | 板载 LED | 备注 |
|---|---|---|---|---|
| `auto` | 按 SoC + flash 推断 | — | — | 默认 |
| `esp32s3-devkitc1-n16r8` | ESP32-S3-DevKitC-1 N16R8 | SDA8 SCL9 | GPIO48 WS2812 | USB-Serial/JTAG 在 19/20 |
| `esp32s3-xiaozhi-bread` | 面包板 xiaozhi 接线 | SDA41 SCL42 | GPIO48 | 麦 4/5/6、放 7/15/16、按键 0/39/40 |
| `esp32dev` | ESP32 DevKit v1 | SDA21 SCL22 | GPIO2 普通 | 经典 WROOM-32 |
| `esp32c3-devkitm-1` | ESP32-C3-DevKitM-1 | SDA8 SCL9 | GPIO8 RGB | 单核 RISC-V |
| `esp32c6-devkitc-1` | ESP32-C6-DevKitC-1 | SDA6 SCL7 | GPIO8 RGB | WiFi6 + Thread |
| `generic` | 通用 | SDA8 SCL9 | 无 | 自己填引脚 |

`config.board` 可显式指定；`pins` 表可覆盖任意一项（`led`、`button.0`、`servo.0`、`relay.0`、
`hcsr04.trig`、`oled.height` …）。

## 5. 能力 id

`wifi gpio adc pwm i2c onewire led button oled lcd bme280 sht3x ina219 mpu6050 ads1115 pcf8574
ds18b20 hcsr04 servo relay mic speaker`

工具用 `requires` 声明所需 id；清单里给设备写 `caps` 时可以扩展新的 id，社区驱动自由命名即可。

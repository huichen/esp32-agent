# Capabilities, drivers and devices

Two axes: a **capability** is a string a tool uses to decide whether it may appear, and a
**driver** is chip code that can be instantiated.

## 1. How identification happens

At boot (and on every `devices_probe` / `/api/devices/probe`):

1. Scan each I2C bus listed in `config.bus.i2c` to get the set of addresses that answer.
2. Try drivers in a fixed order and **confirm identity from register evidence**, so an
   address collision cannot masquerade as a device:
   - SSD1306/SH1106: 0x3C/0x3D answers → write the init sequence (these panels cannot be
     read back)
   - BME280/BMP280: `0xD0` = 0x60/0x58
   - SHT3x: 0x44/0x45 takes a single-shot measurement that passes its CRC
   - INA219/INA226: `0xFE` = 0x5449 (INA226)
   - MPU-6050/6500: `0x75` ∈ {0x68,0x69,0x70,0x71,0x73}
   - ADS1115: 0x48–0x4B returns one conversion within range
   - PCF8574: write 0xFF and read 0xFF back (LCD backpack)
3. 1-Wire: probe only the pins in `config.bus.onewire_pins` (sending a reset pulse on an
   arbitrary pin is not a harmless operation), and require a presence pulse plus a ROM CRC.
4. Declarative peripherals (relay/servo/HC-SR04/LED/buttons): from the board profile or
   `config.pins`, because they cannot be probed passively. The devkit's own LED and BOOT
   button follow `config.auto_peripherals`: with `false` the board comes up without them
   and they exist once a manifest declares the device —
   `{"id":"rgb","kind":"led","driver":"led","pins":{"out":48},"options":{"kind":"ws2812"}}`
   and `{"id":"boot","kind":"buttons","driver":"buttons","pins":{"boot":0}}`. That is
   how a different devkit starts with a minimal surface (44 tools instead of 48 on an
   S3-DevKitC-1) and grows on purpose. A pin set in `config.pins` (`led`, `button.boot`)
   still wins either way, because an explicit pin is a decision.
5. I2S audio (microphone, amplifier): **never detectable** — I2S is point-to-point with no
   addressing and no id register, so nothing can answer a probe. The pins come from the
   board profile or `config.pins` (`mic.bclk`/`mic.ws`/`mic.din`,
   `amp.bclk`/`amp.ws`/`amp.dout`); when they are set and `Hardware::i2s_setup` brings
   the channels up, the `mic`/`speaker` devices and their tools exist. The capability
   rides on the device, so a board whose I2S does not come up never claims `mic` or
   `speaker`.
6. An address no driver claims goes into `unidentified`; `i2c_identify` then reads the
   common id registers to suggest candidates.

## 2. Implemented drivers

| driver | Device | Identification | Tools | Notes |
|---|---|---|---|---|
| `ssd1306` | SSD1306 / SH1106 128x32 or 128x64 | address 0x3C/0x3D | `oled_text` `oled_clear` `oled_rect` `oled_bitmap` `oled_contrast` | built-in 6x8 font; `options.controller=sh1106`, `options.height` |
| `pcf8574` | PCF8574 expander + HD44780 1602 | write 0xFF, read back | `pcf8574_read/write` `lcd_text` `lcd_clear` `lcd_backlight` | 4-bit mode, P0=RS P2=EN P3=backlight |
| `bme280` | BME280 / BMP280 | chip id | `bme280_read` | temperature/pressure/humidity/dew point/altitude |
| `sht3x` | SHT30/31/35 | measurement + CRC | `sht3x_read` | BME280 wins when both share an address |
| `ina219` | INA219 / INA226 | 0x5449 / address | `power_read` | `options.shunt_mohm`, 100 mΩ by default |
| `mpu6050` | MPU-6050/6500 | WHO_AM_I | `mpu6050_read` | accel/gyro/tilt, range selectable |
| `ads1115` | ADS1115 16-bit ADC | conversion within range | `ads1115_read` | single-ended 0–3 or differential 0-1/2-3, gain selectable |
| `ds18b20` | DS18B20/DS18S20 | presence + ROM CRC | `ds18b20_read` | needs a 4.7 kΩ pull-up; `pins.data` |
| `hcsr04` | HC-SR04 / JSN-SR04T | first ping | `distance_cm` | `pins.trig` / `pins.echo` |
| `servo` | servo | declared pin | `servo_angle` | 50 Hz, 500–2500 µs |
| `relay` | relay | declared pin | `relay_set` `relay_pulse` | `options.active_low=true` |
| `led` | WS2812 / plain GPIO / PWM | board profile or declared | `led_set` `led_blink` | `options.kind=ws2812\|pwm\|gpio`, `count` |

## 3. Planned (claim one)

`dht11/dht22`, `rc522` (RFID), `vl53l0x` (ToF), `sdcard`, `inmp441` (I2S microphone),
`max98357a` (I2S amplifier), `bme680` (gas), `sgp30`, `tsl2561` (light), `hmc5883`
(magnetometer), `pn532`, `max6675` (thermocouple), `aht20`, `bh1750`, multi-channel
`sg90`, `step` (stepper motor).

## 4. Board profiles (`boards.rs`)

A board profile records the facts that cannot be probed: I2C pins, the on-board LED's GPIO
and kind, buttons, I2S microphone/amplifier pins, usable ADC pins, reserved pins.

| id | Board | I2C | On-board LED | Notes |
|---|---|---|---|---|
| `auto` | inferred from SoC + flash | — | — | default |
| `esp32s3-devkitc1-n16r8` | ESP32-S3-DevKitC-1 N16R8 | SDA8 SCL9 | GPIO48 WS2812 | USB-Serial/JTAG on 19/20 |
| `esp32s3-xiaozhi-bread` | xiaozhi breadboard wiring | SDA41 SCL42 | GPIO48 | mic 4/5/6, amp 7/15/16, buttons 0/39/40 |
| `esp32dev` | ESP32 DevKit v1 | SDA21 SCL22 | GPIO2 plain | the classic WROOM-32 |
| `esp32c3-devkitm-1` | ESP32-C3-DevKitM-1 | SDA8 SCL9 | GPIO8 RGB | single-core RISC-V |
| `esp32c6-devkitc-1` | ESP32-C6-DevKitC-1 | SDA6 SCL7 | GPIO8 RGB | WiFi 6 + Thread |
| `generic` | generic | SDA8 SCL9 | none | fill in the pins yourself |

`config.board` selects one explicitly; the `pins` map overrides any entry (`led`,
`button.0`, `servo.0`, `relay.0`, `hcsr04.trig`, `oled.height`, …).

## 5. Capability ids

`wifi gpio adc pwm i2c onewire led button oled lcd bme280 sht3x ina219 mpu6050 ads1115
pcf8574 ds18b20 hcsr04 servo relay mic speaker`

A tool declares what it needs in `requires`; a manifest device may declare new ids in
`caps`, so a community driver can name its own.

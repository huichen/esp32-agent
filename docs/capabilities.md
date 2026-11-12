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
4. Declarative peripherals (relay/servo/HC-SR04/LED/buttons, and everything else nothing
   can recognise — motors, encoders, PIR, the SPI parts with no id register): from the
   board profile or `config.pins`, because they cannot be probed passively. The devkit's
   own LED and BOOT button follow `config.auto_peripherals`: with `false` the board comes
   up without them and they exist once a manifest declares the device —
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
   `speaker`. Formats are fixed per side and come from the bench measurements:
   microphone **16 kHz, 32-bit slots, left channel only** (an INMP441 sends 24 bits
   left-justified, right slot always zero); amplifier **24 kHz, 16-bit, Philips, mono**.
   On the read side the driver judges by `bytes_read`, not by the return code: this
   board's `i2s_channel_read` answers `ESP_ERR_TIMEOUT` while already holding samples, and
   counting only `ESP_OK` makes a working microphone read as silence.
6. An address no driver claims goes into `unidentified`; `i2c_identify` then reads the
   common id registers to suggest candidates.

## 2. Implemented drivers

`CATALOG` in `drivers/mod.rs` is the list; every row there has `status: "implemented"`,
and `GET /api/catalog` serves the same table to a client. Tools are named as the
`tools/list` names them.

| driver | Device | Identification | Tools | Notes |
|---|---|---|---|---|
| `ssd1306` | SSD1306 / SH1106 128x32 or 128x64 | address 0x3C/0x3D | `oled_text` `oled_clear` `oled_rect` `oled_bitmap` `oled_contrast` `oled_read` | built-in 6x8 font; `options.controller=sh1106`, `options.height` |
| `pcf8574` | PCF8574 expander + HD44780 1602 | write 0xFF, read back | `pcf8574_read` `pcf8574_write` `lcd_text` `lcd_clear` `lcd_backlight` | 4-bit mode, P0=RS P2=EN P3=backlight |
| `bme280` | BME280 / BMP280 | chip id 0x60/0x58 at 0x76/0x77 | `bme280_read` | temperature/pressure/humidity/dew point/altitude |
| `sht3x` | SHT30/31/35 | measurement + CRC at 0x44/0x45 | `sht3x_read` | BME280 wins when both share an address |
| `ina219` | INA219 / INA226 | 0x5449 / reset value at 0x40-0x41 | `power_read` | `options.shunt_mohm`, 100 mΩ by default |
| `mpu6050` | MPU-6050/6500 | WHO_AM_I | `mpu6050_read` | accel/gyro/tilt, range selectable |
| `ads1115` | ADS1115 16-bit ADC | conversion within range at 0x48-0x4B | `ads1115_read` | single-ended 0–3 or differential 0-1/2-3, gain selectable |
| `ds18b20` | DS18B20/DS18S20 | presence + ROM CRC | `ds18b20_read` | needs a 4.7 kΩ pull-up; `pins.data` |
| `hcsr04` | HC-SR04 / JSN-SR04T | first ping | `distance_cm` | `pins.trig` / `pins.echo` |
| `servo` | hobby servo | declared pin | `servo_angle` | 50 Hz, 500–2500 µs |
| `relay` | relay board channel | declared pin | `relay_set` `relay_pulse` | `options.active_low=true` |
| `led` | WS2812 / plain GPIO / PWM | board profile or declared | `led_set` `led_blink` | `options.kind=ws2812\|pwm\|gpio`, `count` |
| `buttons` | push buttons with pull-ups | board profile or declared pins | `buttons_read` `button_wait` | `elapsed_ms` in the readings |
| `dht11` | DHT11 / DHT22 | declared pin: a bit-banged 40-bit frame checked against its own checksum | `dht_read` | `min_interval_ms` per part; a failed CRC says so instead of inventing a reading |
| `rc522` | MFRC522 RFID reader | SPI + reset: version register 0x37 (0x91/0x92) | `rc522_uid` `rc522_version` | `pins.cs` / `pins.reset` |
| `vl53l0x` | VL53L0X time-of-flight distance | 0x29: model id 0xC0 = 0xEE | `vl53l0x_read` | `distance_mm`, `range_status`, tuning settings |
| `sdcard` | SD card over SPI | declared chip select: CMD0/CMD8/ACMD41/CMD58 handshake | `sdcard_info` `sdcard_read_block` `sdcard_status` | raw 512-byte blocks as base64; no filesystem |
| `bme680` | BME680 temperature / humidity / pressure / gas | chip id 0xD0 = 0x61 | `bme680_read` | `gas_counts`, `humidity_counts`, altitude |
| `aht20` | AHT20 / AHT10 | 0x38: the status byte after a convert reports calibrated | `aht20_read` | temperature/humidity/dew point |
| `bh1750` | BH1750 ambient light | 0x23/0x5C: one-byte command, 16 bits of lux back | `bh1750_read` | continuous mode |
| `hx711` | HX711 load cell amplifier | declared only: two GPIOs, bit-banged 24-bit read | `hx711_read` `hx711_tare` | `counts_per_gram` |
| `ds3231` | DS3231 real-time clock | no id register: a plausible BCD time and a valid status byte | `rtc_read` `rtc_set` | `oscillator_stopped` when the coin cell went flat |
| `bdc_motor` | L298N / TB6612 / DRV8833 | declared only: not probeable, an H-bridge has no id | `motor_speed` `motor_state` `motor_stop` | `two_pwm` or PWM + direction |
| `stepper` | A4988 / DRV8825 / TMC2208 | declared only: STEP/DIR pulses with a move time limit | `stepper_move` `stepper_goto` `stepper_release` | `steps_per_rev` |
| `encoder` | rotary encoder (KY-040 and bare) | declared only: two GPIOs, polled quadrature | `encoder_read` `encoder_reset` | a fast turn can drop a detent |
| `pir` | HC-SR501 PIR motion | declared only: one GPIO; a quiet sensor looks like an undriven pin | `motion_now` `motion_wait` `motion_since` | `active_high`, `timeout_ms` |
| `tm1637` | TM1637 four-digit display | declared only: two GPIOs, its own clocked protocol | `tm1637_show` `tm1637_brightness` `tm1637_clear` | identify reads the brightness echo |
| `max7219` | MAX7219 LED matrix | declared only: SPI with a chip select | `max7219_row` `max7219_bitmap` `max7219_text` `max7219_intensity` | identify writes and reads the intensity register |
| `camera` | OV2640 / OV3660 | declared only: SCCB ids 0x26/0x36, but a frame has to come through the platform, so a camera is never auto-adopted | `camera_capture` `camera_info` `camera_register` | 0x3C is also the OLED's, which is why it is never probed |
| `mic` | INMP441 / ICS-43434 I2S microphone | I2S pins from the profile or `config.pins` | `mic_level` `mic_record` | 16 kHz, 32-bit slots, left channel |
| `speaker` | MAX98357A I2S amplifier | I2S pins from the profile or `config.pins` | `speaker_tone` `speaker_play` | 24 kHz, 16-bit, mono |

## 3. What is not implemented

The catalogue is complete: every entry in `CATALOG` is implemented and probeable or
declarable. What is left is not another driver but the things around it — a camera
frame that does not go through the 2 KB tool result, an SD card with a filesystem on
it, manifest templates. `ROADMAP.md` holds that list, and nothing belongs here until
it has a driver.

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

`wifi gpio adc pwm i2c onewire` come from the SoC and the board profile.

`led button oled lcd bme280 sht3x ina219 mpu6050 ads1115 pcf8574 ds18b20 hcsr04 servo
relay dht rc522 vl53l0x sdcard bme680 aht20 bh1750 hx711 rtc motor stepper encoder pir
sevenseg matrix camera mic speaker` come from a detected or declared device.

A tool declares what it needs in `requires`; a manifest device may declare new ids in
`caps`, so a community driver can name its own.

## 6. The board's own tools

Two groups need no device and appear on every board. The first is what the hardware is
made of; the second is the board looking at itself. `GET /api/tools` and `/agent.json` are
authoritative when this table and a firmware disagree.

| Tool | Answers |
|---|---|
| `board` | the board about itself, in nine commands: `status` (chip, firmware, uptime, die temperature, heap, WiFi, device count, tool counters), `inventory` (profile, buses scanned, every device with pins and address, addresses nothing claimed), `config_get`/`config_set`, `chip_temp` (the die's own sensor, not a board thermometer), `logs_tail`, `reboot`, `factory_reset`, `delay`; `command: help` lists each |
| `devices_list`, `devices_probe` | the detected devices and their capabilities; re-scan the buses |
| `i2c_scan`, `i2c_read`, `i2c_write`, `i2c_identify` | bus access for chips without a driver, and "what answers at this address" |
| `gpio_mode`, `gpio_read`, `gpio_write`, `adc_read` | the pins a manifest composes, as tools of their own |
| `device_invoke` | one detected device's tool, by id — what `tools_mode: generic` collapses the device tools into |

The `manifest` (five commands) and `board` (nine) families are in [manifest](manifest.md) and here above; `shell`, `read`,
`write`, `edit` and `script` are in [shell](shell.md) and [scripting](scripting.md).

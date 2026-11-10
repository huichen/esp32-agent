# Flashing and first-time setup

## 1. Toolchain

```bash
cargo install espup espflash ldproxy
espup install
# then in every new shell, run the environment script espup generated (PowerShell):
#   . $HOME\export-esp.ps1        Windows: C:\Users\<you>\export-esp.ps1
```

ESP-IDF v5.4+ is required (`firmware/.cargo/config.toml` pins `v5.5.2`). On the first
build, `embuild`/`esp-idf-sys` downloads ESP-IDF and its tools itself (several GB,
10–30 minutes).

## 2. Build and flash

```powershell
# build (esp32s3 by default)
powershell scripts/build-firmware.ps1

# flash and watch the serial console
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

Another chip: change `target` and `MCU` in `firmware/.cargo/config.toml` and
`firmware/sdkconfig.defaults` (for example `esp32c3` → `riscv32imc-esp-espidf`,
`MCU=esp32c3`, and drop the PSRAM settings).

### Do not let the flasher generate its own partition table

Without `--partition-table`, `espflash flash <elf>` builds **its own single-partition
table** and writes it into flash, overwriting the one `partitions.csv` defines. The boot
log still looks normal, but `otadata`/`ota_0`/`ota_1` are gone. The correct command (the
one the scripts use):

```powershell
espflash flash --chip esp32s3 --port COM3 --baud 921600 --flash-size 16mb `
  --partition-table firmware/partitions.csv --target-app-partition factory `
  target/xtensa-esp32s3-espidf/release/esp32-agent
```

After flashing, check the serial output: the bootloader's `Partition Table:` must list
seven entries (`nvs`, `otadata`, `phy_init`, `factory`, `ota_0`, `ota_1`, `storage`), and
the firmware itself prints the real table again through `esp_partition_find`.

### On Windows, build from a short path

`esp-idf-sys` rejects long paths, so build from a short clone (e.g. `D:\e`) rather than a
deep directory: the path is canonicalised, so `subst` does not help. A machine-level
`rustc-wrapper = "sccache"` in the cargo config cannot spawn the ESP toolchain's rustc;
move that config aside for the build and restore it afterwards. `LIBCLANG_PATH` must point
at the **directory** holding `esp-clang\bin`.

## 3. Partition table

`firmware/partitions.csv`:

| Name | Type | Offset | Size |
|---|---|---|---|
| `nvs` | data/nvs | 0x9000 | 24 KB |
| `otadata` | data/ota | 0xF000 | 8 KB |
| `phy_init` | data/phy | 0x11000 | 4 KB |
| `factory` | app | 0x20000 | 3 MB |
| `ota_0` | app | 0x320000 | 3 MB |
| `ota_1` | app | 0x620000 | 3 MB |
| `storage` | data/spiffs | 0x920000 | 1 MB |

OTA is not implemented yet, but the slots are reserved. `storage` is held for future
static web assets and fonts.

## 4. First power-up: provisioning over the access point

With **no WiFi credentials** stored, the board comes up in setup mode:

```
wifi: setup mode: access point `esp32-agent-setup` at http://192.168.4.1/
http: listening on http://0.0.0.0:8080/
```

1. Join `esp32-agent-setup` from a phone (no password by default; set
   `config.wifi.ap_password` if you want one).
2. Open `http://192.168.4.1:8080/` (most phones open it automatically, because the captive
   DNS answers everything).
3. Scan → pick the SSID → enter the password → "save and connect".
4. The board writes it to NVS and reconnects — no reboot, no reflash. The access point
   stays up so you can still change settings from it later; turn `wifi.ap_fallback` off if
   you do not want that.

Credentials live only in NVS. **The firmware contains no password**, so moving networks
costs nothing.

## 5. Reaching it afterwards

- Look for `esp32-agent`'s DHCP lease on the router; the serial log prints the IP too.
- Console: `http://<board-ip>:8080/`
- MCP: `http://<board-ip>:8080/mcp`

## 6. Optional: console PIN and MCP token

```json
POST /api/config
{ "auth": { "portal_pin": "1234", "mcp_token": "<long random string>" } }
```

- `portal_pin`: write operations then require `X-Portal-Pin`.
- `mcp_token`: the MCP endpoint then requires `Authorization: Bearer …`.

## 7. Recovery

- Lost the PIN: if the console is out of reach, erase the NVS partition over serial:
  `python -m esptool -p COM3 erase-region 0x9000 0x6000`
- Full factory reset: `python -m esptool -p COM3 erase-flash`, then flash again.

## 8. Common build problems

| Symptom | Fix |
|---|---|
| `ldproxy` not found | `cargo install ldproxy`, and make sure `~/.cargo/bin` is on `PATH` |
| `error: linker 'ldproxy' not found` | same; the linker name in `.cargo/config.toml` must match what is installed |
| missing `riscv`/`xtensa` target | after `espup install` you need `source export-esp.sh/.ps1`, or build with `cargo +esp build` |
| `esp-idf-sys` download fails | point `IDF_PATH` at a local ESP-IDF, or retry behind a mirror |
| Builds, but panics on boot | check that the PSRAM settings in `sdkconfig.defaults` match the board (only N16R8 has octal PSRAM) and that the build used that file at all |

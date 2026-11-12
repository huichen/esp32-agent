# Flashing and first-time setup

## 0. If you do not want to build at all

Every tagged release carries a merged image built from that tag, so the whole section below
is optional:

```bash
# espflash (Rust, any OS)
espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin

# or the Python tooling
esptool.py --chip esp32s3 write_flash 0x0 esp32-agent-vX.Y.Z.bin
```

Then jump to section 4: the board raises the `esp32-agent-setup` access point when it has no
credentials, and provisioning is the same either way.

The image is the project's own build (`firmware/partitions.csv`, application partition
`factory`), produced by `.github/workflows/release.yml` on every tag. If you are on a
different flash size, or want a chip other than the esp32s3, build it yourself — the rest of
this page is that path.

## 1. Toolchain

The `scripts/` helpers are PowerShell, so they are the Windows path. The toolchain itself is
not Windows-specific; on macOS or Linux the three commands below are the same, and the only
difference is the file `espup` writes for you to source:

```bash
cargo install espup espflash ldproxy
espup install
. $HOME/export-esp.sh        # macOS/Linux; on Windows it is export-esp.ps1
```

With that environment sourced, the build is plain cargo:

```bash
cd firmware
cargo build --release --target xtensa-esp32s3-espidf
espflash flash --chip esp32s3 --flash-size 16mb \
  --partition-table partitions.csv --target-app-partition factory \
  target/xtensa-esp32s3-espidf/release/esp32-agent
```

The PowerShell script does exactly that, plus the two workarounds documented in section 8
(`LIBCLANG_PATH`, and the short output path esp-idf-sys insists on) — worth reading even
when you are not on Windows, because the second one bites on any path with a long name.



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

The path is `<clone>\target\…` because that is where the build script keeps it; with
`-TargetDir` elsewhere, use that directory instead.

After flashing, check the serial output: the bootloader's `Partition Table:` must list
seven entries (`nvs`, `otadata`, `phy_init`, `factory`, `ota_0`, `ota_1`, `storage`), and
the firmware itself prints the real table again through `esp_partition_find`.

### On Windows, build from a short path

`esp-idf-sys` rejects an output path longer than 88 characters, so build from a short
clone (e.g. `D:\e`) rather than a deep directory: the path is canonicalised, so `subst`
does not help. `scripts/build-firmware.ps1` handles the four traps this sets — it selects
the `esp` toolchain, points `LIBCLANG_PATH` at the **directory** holding `esp-clang\bin`,
keeps the artifacts in `<clone>\target` instead of `firmware\target` (pass
`-TargetDir D:\t` if the clone path is still too long), and rewrites
`firmware/.cargo/config.toml` only when a chip setting actually changed. A machine-level
`rustc-wrapper = "sccache"` in the cargo config cannot spawn the ESP toolchain's rustc;
move that config aside for the build and restore it afterwards.

What each of those looks like when it is wrong, so the next person recognises the error
instead of re-deriving the fix:

| Symptom | Cause | Fix |
|---|---|---|
| `error[E0463]: can't find crate for core`, plus "the target may not be installed" | `cargo build` on the default toolchain: it has neither the xtensa target nor `build-std` | build with `cargo +esp` (the script does) |
| `couldn't find libclang` / bindgen failing on the first build | `export-esp.ps1` sets `LIBCLANG_PATH` to `libclang.dll`; bindgen wants the directory that holds it | set `LIBCLANG_PATH` to `…\esp-clang\bin` (the script does) |
| `Too long output directory: … Shorten your project path down to no more than 10 characters` | `esp-idf-sys` refuses an output directory over 88 characters on Windows; `firmware\target\…` passes that in any clone with a name of its own | point `CARGO_TARGET_DIR` at a short path (`<clone>\target`, or `-TargetDir D:\t`) |
| A one-line diff in `firmware/.cargo/config.toml` after every build, or all of it rewritten | PowerShell 5.1's `Set-Content -Encoding utf8` writes a BOM, and the script rewrote the file unconditionally | write only when the text changed, through `[System.IO.File]::WriteAllText` with `UTF8Encoding($false)` (the script does) |

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
http: listening on http://0.0.0.0:80/
```

1. Join `esp32-agent-setup` from a phone (no password by default; set
   `config.wifi.ap_password` if you want one).
2. Open `http://192.168.4.1:80/` (most phones open it automatically, because the captive
   DNS answers everything).
3. Scan → pick the SSID → enter the password → "save and connect".
4. The board writes it to NVS and reconnects — no reboot, no reflash. The access point
   stays up so you can still change settings from it later; turn `wifi.ap_fallback` off if
   you do not want that.

Credentials live only in NVS. **The firmware contains no password**, so moving networks
costs nothing.

### Without leaving the network: the console

Joining the board's access point from a machine that is also talking to a model over the
network cuts that machine off — and if the model runs on this agent, the run dies with it.
The console is a way in that touches nothing else. With the board on its USB port:

```text
provision <ssid> <password>
```

one line, terminated by `\n`. The board answers on the console, writes NVS and restarts:

```
I (…) esp32_agent::provision: credentials for `my-network` received on the console
I (…) esp32_agent: wifi state: ap+sta, station ip Some("192.168.3.7")
```

Two details decide whether it works on a given bench:

- **Which port.** A DevKitC-1 has two USB sockets. The UART one is UART0 (GPIO43/44); the
  other is the chip's own **USB-Serial-JTAG**. The firmware reads the USB-Serial-JTAG port
  when it can and falls back to UART0, because writing to the wrong one just times out on
  the host. If the log says `console: reading USB-Serial-JTAG`, the port next to `PWR` is
  the one to talk to.
- **Do not reset on open.** Tools that assert DTR/RTS while opening the port pulse the
  chip's reset or park it in download mode, so the line lands on a booting device. Drop
  both lines first (`pyserial`: `sp.dtr = False; sp.rts = False` before `open()`).

## 5. Reaching it afterwards

- Look for `esp32-agent`'s DHCP lease on the router; the serial log prints the IP too.
- Console: `http://<board-ip>:80/`
- MCP: `http://<board-ip>:80/mcp`

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

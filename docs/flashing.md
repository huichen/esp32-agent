# 烧录与首次配置

## 1. 工具链

```bash
cargo install espup espflash ldproxy
espup install
# 之后每次开新终端：把 espup 生成的环境脚本跑一下（PowerShell）
#   . $HOME\export-esp.ps1        Windows：C:\Users\<you>\export-esp.ps1
```

需要 ESP-IDF v5.4+（`firmware/.cargo/config.toml` 里写的是 `v5.5.2`）。首次编译时
`embuild`/`esp-idf-sys` 会自己下载 ESP-IDF 与工具（数 GB，10–30 分钟）。

## 2. 编译与烧录

```powershell
# 编译（默认 esp32s3）
powershell scripts/build-firmware.ps1

# 烧录并看串口
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

换芯片：改 `firmware/.cargo/config.toml` 的 `target`、`MCU` 与 `firmware/sdkconfig.defaults`
（例如 `esp32c3` → `riscv32imc-esp-espidf`、`MCU=esp32c3`，并去掉 PSRAM 配置）。

### 烧录时别让 flasher 自己生成分区表

`espflash flash <elf>` 在没有 `--partition-table` 时会**自己造一张单分区表**并写进 flash，
把仓库 `partitions.csv` 定义的那张覆盖掉——启动日志看起来正常，但 `otadata`/`ota_0`/`ota_1`
就没了。正确的一行（也用在 `scripts/` 里的烧录流程）：

```powershell
espflash flash --chip esp32s3 --port COM3 --baud 921600 --flash-size 16mb `
  --partition-table firmware/partitions.csv --target-app-partition factory `
  target/xtensa-esp32s3-espidf/release/esp32-agent
```

刷完在串口里核对：bootloader 打印的 `Partition Table:` 应当有七项（`nvs`、`otadata`、
`phy_init`、`factory`、`ota_0`、`ota_1`、`storage`），固件自己也会用 `esp_partition_find`
把实际分区再打一遍。
## 3. 分区表

`firmware/partitions.csv`：`nvs 24 KB` / `factory 3 MB` / `ota_0 3 MB` / `spiffs 1 MB`。
OTA 还没实现，但预留了槽位；`storage` 分区留给将来的静态网页/字体。

## 4. 首次上电：热点配网

板子启动后**没有任何 WiFi 凭证**，于是进入 setup 模式：

```
wifi: setup mode: access point `esp32-agent-setup` at http://192.168.4.1/
http: listening on http://0.0.0.0:8080/
```

1. 手机连 `esp32-agent-setup`（默认无密码；可在 `config.wifi.ap_password` 设）。
2. 打开 `http://192.168.4.1:8080/`（多数手机会自动弹出，因为有捕获式 DNS 应答）。
3. 扫描 → 选 SSID → 输密码 → "保存并连接"。
4. 板子写入 NVS 并重连（不需要重启，也不需要重新烧录）。热点保留，方便以后从热点侧改配置；
   不需要就把 `wifi.ap_fallback` 关掉。

凭证只在 NVS 里。**固件里没有密码**，换网络随便换。

## 5. 之后怎么访问

- 路由器上看 `esp32-agent` 的 DHCP 地址；串口日志也会打印 IP。
- 控制台：`http://<board-ip>:8080/`
- MCP：`http://<board-ip>:8080/mcp`

## 6. 可选：控制台 PIN 与 MCP token

```json
POST /api/config
{ "auth": { "portal_pin": "1234", "mcp_token": "长随机串" } }
```

- `portal_pin`：写操作需要 `X-Portal-Pin`。
- `mcp_token`：MCP 端点要求 `Authorization: Bearer …`。

## 7. 恢复

- 忘记 PIN：串口里 `POST /api/factory-reset` 进不去时，用 esptool 擦 NVS：
  `python -m esptool -p COM3 erase-region 0x9000 0x6000`
- 完全恢复出厂：`python -m esptool -p COM3 erase-flash`，然后重新烧。

## 8. 常见编译问题

| 现象 | 处理 |
|---|---|
| `ldproxy` 找不到 | `cargo install ldproxy`，并确认 `PATH` 里有 `~/.cargo/bin` |
| `error: linker 'ldproxy' not found` | 同上；`.cargo/config.toml` 的 linker 名要与已装的一致 |
| `riscv`/`xtensa` target 缺失 | `espup install` 后需要 `source export-esp.sh/.ps1` 或 `cargo +esp build` |
| `esp-idf-sys` 下载失败 | 设 `IDF_PATH` 指向本地 ESP-IDF，或换镜像后重试 |
| 编译通过但启动即 panic | 检查 `sdkconfig.defaults` 的 PSRAM 配置与板子是否匹配（N16R8 才有 octal PSRAM） |

# 路线图

已完成的能力写在 README 与 `docs/` 里；这里只列还没做或没验证的。

## 已验证 / 未验证

- ✅ `agent-core`：164 个测试（协议、HTTP、清单校验、驱动换算、能力过滤、鉴权、热更新、agent 循环、fs/net/sched）
- ✅ 模拟器端到端：`scripts/smoke.ps1`
- ✅ 固件在实机跑通（ESP32-S3-DevKitC-1 N16R8，ESP-IDF v5.5.2）：
  不复位启动、I2C 总线可用、配网 AP 与站模式并存、`/` 与 `/mcp` 正常、`tools/list` 连续 5 次稳定，
  8 MB PSRAM 通过内存测试，仓库的七分区表随镜像下发并可在运行时枚举
- ✅ 板载智能体闭环：`board_status`、`fs_write→fs_read→fs_grep`、`net_get`（真实外部 HTTP）
  都经模型循环在板上跑过；45 s 定时任务自主触发两次，历史跨重启保留
- ⚠️ 未实测：WS2812/RMT、ADC/PWM 数值、`/api/ota`、以及全量 sdkconfig 下的启动时间与功耗

## 近期

1. **分阶段对话**：把模型网络等待移出 `App` 锁——现在一轮对话期间门户只能排队，这是最大的已知限制。
2. **OTA**：`/api/ota` 上传固件、双分区回滚；分区表已就绪（`otadata`/`ota_0`/`ota_1` 已在固件里）。
3. **mDNS**：注册 `_mcp._tcp`，客户端不必先知道 IP。
4. **文件工作区改用 SPIFFS**（`storage` 分区 1 MB），现在的工作区是 NVS 里一块 8 KB 的 blob。
5. **TLS**：模型调用与 `net_*` 的 `https://`（目前明确拒绝并报错）。
## 近期

1. **固件首次实机跑通**：编译 → 烧录 → 热点配网 → 网页改引脚 → 清单加工具 → 调用。记录到 `docs/flashing.md`。
2. **OTA**：`/api/ota` 上传固件、双分区回滚；分区表已预留。
3. **mDNS**：注册 `_mcp._tcp`，客户端不必先知道 IP。
4. **SSE**：`GET /mcp` 的长连接（现在返回 405，规范允许）。
5. **清单快照**：每次 apply 前存上一版，支持一键回滚。

## 中期

6. **I2S 音频**：INMP441 麦克风 + MAX98357A 功放 → `mic_level`、`tone`、`record`。
7. **驱动扩充**：DHT11/22、RC522、VL53L0X、BH1750、BME680、SD 卡、步进电机。
8. **清单模板库**：`examples/manifests/` 长成"设备配方"集合，网页里一键导入。
9. **能力注册表**：把 `docs/capabilities.md` 的表格变成 `scripts/` 里一条校验命令的单一来源（仓库不带 CI）。
10. **多总线/多设备 UI**：现在同一 kind 的第二台设备会有 `_2` 后缀，网页里还看不出区别。

## 长期

11. **零代码扩展**：清单里用表达式（`scale`、`map`、`threshold`）描述换算，进一步减少写驱动的需求。
12. **资源订阅**：传感器变化推给客户端（`resources/subscribe`）。
13. **安全**：设备端 OAuth（至少是 token + 速率限制 + 危险动作白名单），以及局域网内的 TLS。
14. **社区**：issue 模板按"新设备/新驱动/协议问题"分类；每季度把 `examples/manifests/` 打包成一块示例板。

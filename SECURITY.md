# 安全策略

这是一台**能动手的**设备：MCP 客户端可以让它合上继电器、转舵机、写 I2C。请把它当作局域网内的执行器，
而不是普通网页。

## 部署建议

1. **只在可信局域网**：默认无 TLS，别把 `8080` 直接暴露到公网；要远程访问请走 VPN 或反向代理（带 TLS）。
2. **开 token 与 PIN**：
   ```json
   { "auth": { "mcp_token": "<32 字节随机>", "portal_pin": "123456" } }
   ```
   开了之后 MCP 要求 `Authorization: Bearer …`，控制台写操作要求 `X-Portal-Pin`。
3. **限制危险动作**：给清单里的写操作加 `safety`，例如 `{"max_calls_per_min": 10}`、
   `{"min": 0, "max": 90}`、`{"confirm": true}`。这些在板子上强制，不依赖模型自觉。
4. **关掉不用的热点**：配网完成后设 `wifi.ap_fallback: false`，减少一个入口。
5. **及时改密码**：`POST /api/wifi/forget` 清除凭证，重新配网。

## 已知风险

| 风险 | 现状 | 缓解 |
|---|---|---|
| 提示注入导致物理动作 | 无审批环节 | `safety.confirm`、限频、局域网隔离 |
| 明文 HTTP | 无 TLS | 内网 + 反向代理；不要把端口映射到公网 |
| 凭证在 NVS 明文 | ESP-IDF NVS 默认不加密 | 可开 NVS 加密（`CONFIG_NVS_ENCRYPTION`），需要先烧密钥 |
| 任意 I2C 写 | `i2c_write` / 清单原语是设计的一部分 | 只给可信模型；必要时在清单里不暴露写原语 |
| 固件无签名校验 | 未实现 OTA | 见 ROADMAP；当前只能串口烧录 |

## 报告漏洞

请**不要**公开开 issue。用 GitHub 的私密漏洞报告（Security → Report a vulnerability），或直接联系维护者。
请附：影响的版本/commit、可复现步骤、影响面（能否触发物理动作）、以及你建议的修复方向。

我们会在 7 天内确认，修复后在该版本 `CHANGELOG.md` 里致谢（除非你要求匿名）。

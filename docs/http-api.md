# HTTP 与 MCP 接口

同一个端口同时提供网页控制台和 MCP 端点（默认 `8080`，`config.mcp.port` 可改）。

## 1. MCP 端点

`POST /mcp`（`config.mcp.path` 可改）。Streamable HTTP，无 SSE。

```bash
# 列工具
curl -sS -X POST http://<ip>:8080/mcp \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

# 调工具
curl -sS -X POST http://<ip>:8080/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"board_status","arguments":{}}}'
```

支持的 JSON-RPC 方法：`initialize`、`ping`、`tools/list`、`tools/call`、`resources/list`、
`resources/read`、`resources/templates/list`、`prompts/list`（空）、`logging/setLevel`、`completion/complete`（空）。
通知（无 `id`）返回 `202` 且无正文。批量数组按 JSON-RPC 处理。

| 头 | 行为 |
|---|---|
| `Mcp-Session-Id`（响应） | `initialize` 或首个请求时分配；客户端可复用 |
| `MCP-Protocol-Version` | `initialize` 的响应里带回服务端版本 |
| `Authorization: Bearer <token>` | 配置了 `auth.mcp_token` 时必需，否则 401 |
| `Connection: keep-alive` | 支持；单连接最多 32 个请求后关闭 |
| `Transfer-Encoding: chunked` | 支持（Node 的 fetch 会用） |
| `Expect: 100-continue` | 先回 100，再读正文 |

`GET /mcp` → `405`（说明用 POST）；`DELETE /mcp` → 结束该会话。

CORS 默认开启（`mcp.cors`），浏览器里的 MCP 客户端可直接连。

## 2. 网页/配置 REST

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/` | 控制台（内嵌 HTML/CSS/JS，无外链） |
| GET | `/healthz` | `{ok, uptime_s, devices}` |
| GET | `/api/status` | 板卡、内存、WiFi、MCP、设备数、计数器 |
| GET | `/api/config` | 配置（密码/token 显示为 `__set__`） |
| POST | `/api/config` | 部分更新（同形状 JSON） |
| GET | `/api/devices` | 设备表 + 未识别地址 + 总线 |
| POST | `/api/devices/probe` | 重新探测 |
| GET | `/api/tools` | 工具表（含隐藏原因） |
| POST | `/api/tools` | `{mode: auto|generic|all, disabled: [...]}` |
| GET | `/api/manifest` | 当前清单 + revision |
| POST | `/api/manifest` | `{manifest, mode?, dry_run?}` |
| GET | `/api/manifest/schema` | 与 `manifest_describe` 相同内容 |
| GET | `/api/wifi/scan` | 扫网 |
| POST | `/api/wifi/connect` | `{ssid, password}`，保存并重连 |
| POST | `/api/wifi/forget` | 清除凭证 |
| GET | `/api/logs?n=&since=` | 环形日志 |
| GET | `/api/catalog` | 驱动目录 |
| GET | `/api/sessions` | MCP 会话 |
| GET | `/api/clients` | 可直接复制的客户端配置 |
| POST | `/api/auth` | `{pin}`，校验控制台 PIN |
| POST | `/api/reboot` | 重启 |
| POST | `/api/factory-reset` | 清配置与清单并重启 |

写操作在设置了 `auth.portal_pin` 时要求 `X-Portal-Pin`（或 `?pin=`）；读操作不要求。

## 3. 资源（`resources/*`）

| URI | 内容 |
|---|---|
| `mcp://capabilities` | 当前能力、可见工具、被隐藏的工具及缺失能力 |
| `mcp://devices` | 设备表、未识别地址、总线扫描结果 |
| `mcp://devices/<id>` | 单个设备的详情 |
| `mcp://manifest` | 当前清单 + revision |
| `mcp://config` | 脱敏配置 |
| `mcp://logs` | 最近日志 |
| `mcp://catalog` | 驱动目录 |

## 4. 工具结果形状

```json
{ "jsonrpc":"2.0","id":2,
  "result": { "content":[{"type":"text","text":"{…pretty json…}"}],
              "structuredContent": { … },
              "isError": false } }
```

工具失败也返回 `result`，只是 `isError: true` 且正文里带错误消息与码——这样模型能看到原因并自己修参数，
而不是看到一个协议级错误。协议级错误（未知方法、坏 JSON）才用 `error` 对象。

## 5. 客户端接入

- **Claude Desktop / Cursor 等支持 HTTP 的**：`{"mcpServers":{"esp32-agent":{"type":"http","url":"http://<ip>:8080/mcp"}}}`
- **只支持 stdio 的**：用 `mcp-remote`、`mcp2serial` 一类桥。
- **自己写脚本**：见上面的 curl；`/api/clients` 会返回拼好的命令。

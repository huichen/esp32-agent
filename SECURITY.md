# Security policy

This is a device that **can act**: an MCP client can make it close a relay, turn a
servo, or write to I2C. Treat it as an actuator on your LAN, not as an ordinary web
page.

## Deployment recommendations

1. **Trusted LAN only**: there is no TLS by default; do not expose `8080` to the
   internet. For remote access use a VPN or a reverse proxy that terminates TLS.
2. **Turn on the token and the PIN**:
   ```json
   { "auth": { "mcp_token": "<32 random bytes>", "portal_pin": "123456" } }
   ```
   With these set, MCP requires `Authorization: Bearer …` and console write operations
   require `X-Portal-Pin`.
3. **Constrain dangerous actions**: give manifest write operations a `safety` block,
   e.g. `{"max_calls_per_min": 10}`, `{"min": 0, "max": 90}`, `{"confirm": true}`. These
   are enforced on the board, not left to the model's good behaviour.
4. **Turn off the access point you do not need**: after provisioning, set
   `wifi.ap_fallback: false` to remove one entry point.
5. **Rotate credentials**: `POST /api/wifi/forget` clears them; provision again.

## Known risks

| Risk | Current state | Mitigation |
|---|---|---|
| Prompt injection leading to physical action | no approval step | `safety.confirm`, rate limits, LAN isolation |
| Plaintext HTTP | no TLS | intranet plus a reverse proxy; never map the port to the internet |
| Credentials in plaintext NVS | ESP-IDF NVS is not encrypted by default | NVS encryption (`CONFIG_NVS_ENCRYPTION`) is available but needs keys flashed first |
| Arbitrary I2C writes | `i2c_write` and the manifest primitive are by design | point it at trusted models only; omit the write primitive when you do not need it |
| No firmware signature check | OTA is not implemented | see ROADMAP; flashing is over serial today |

## Reporting a vulnerability

**Do not** open a public issue. Use GitHub's private vulnerability reporting
(Security → Report a vulnerability), or contact the maintainers directly. Include the
affected version/commit, reproduction steps, the blast radius (can it trigger a
physical action), and the fix you would suggest.

We acknowledge within 7 days and credit you in that release's `CHANGELOG.md` unless you
ask to stay anonymous.

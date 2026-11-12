# The simulator

`agent-sim` is the same `agent-core` against a virtual board, on a real HTTP port, on a
laptop. Only the `Hardware` implementation differs, so the console, the MCP endpoint, the
drivers, the manifest engine, the agent loop and the scheduler are the code that runs on the
chip.

```bash
cargo run -p esp32-agent-sim                      # port 18080, state in sim-state/
cargo run -p esp32-agent-sim -- --port 18080 --state sim-state/kv.json
cargo run -p esp32-agent-sim -- --empty           # no I2C parts: an empty board
rm -rf sim-state                                  # start over
```

## What the virtual board has

| Part | Address |
|---|---|
| SSD1306 128×32 | 0x3C on bus 0 |
| BME280 | 0x76 |
| SHT3x | 0x44 |
| INA226 | 0x40 |
| Three buttons, one LED | the profile's pins |

`--empty` removes the I2C parts, which is how you check that discovery reports "nothing" and
that the tool list shrinks to the built-in set instead of inventing devices.

## What it cannot tell you

Timing, power, radio, the TLS handshake's appetite for internal SRAM, and anything about a
part that is not modelled. `--empty` still has the on-board LED and button, because those come
from the board profile and not from a probe.

`scripts/smoke.ps1` drives the simulator end to end (portal, `tools/list`, live manifest,
refusal paths); it is the one gate that exercises HTTP without hardware.

Related: [architecture](architecture.md) · [agent loop](agent-loop.md)

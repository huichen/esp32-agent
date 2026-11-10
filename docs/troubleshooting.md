# Troubleshooting

Start with `GET /api/logs` (or the console's log tab). Every boot-time line carries a
`target` (`boot`/`probe`/`wifi`/`http`/`tool`/`manifest`).

## The board will not come up

| Symptom | Check |
|---|---|
| Nothing on the serial console | Is the console not USB-Serial/JTAG? Change `CONFIG_ESP_CONSOLE_*`; baud rate 115200 |
| Reboots right after `rst:0x…` | Read the `panic` line; usually PSRAM settings that do not match the board (`CONFIG_SPIRAM_MODE_OCT`) |
| `i2c bus 0 (SDA 8, SCL 9) failed` | The pin does not exist or is taken; change `config.bus.i2c` or pick other pins in the console |
| `no stored config` in the log | Normal: first power-up, so it falls back to provisioning |

## Provisioning

| Symptom | Check |
|---|---|
| The phone cannot see `esp32-agent-setup` | The board still has old credentials → the serial log says `wifi: joining ...`; `POST /api/wifi/forget` then reboot |
| Joined the access point but no page | It must be `http://192.168.4.1:8080/` — note the port; the captive DNS only makes HTTP pop up automatically |
| Credentials entered but it will not connect | 2.4 GHz and WPA2 are what works (the ESP32 has no 5 GHz radio); watch `wifi: status=…` in the log; check for trailing spaces in a pasted password |
| Connected but no IP found | Look for hostname `esp32-agent` in the router's DHCP list, or the `ip=` line in the serial log |

## A device is invisible

1. `POST /api/devices/probe`, or from the model call `devices_probe` then `devices_list`.
2. `i2c_scan` to see whether the address answers at all:
   - Nothing at all → wiring, pull-ups or power; are SDA/SCL swapped; is a 5 V module
     driving a 3.3 V logic board.
   - An address appears but is `unidentified` → `i2c_identify {addr}` for candidates, or
     read the ID register yourself with `i2c_read`.
3. Right address, wrong driver: address collisions are common (0x76/0x77, 0x3C/0x3D,
   0x44/0x45). The log carries the reason the driver gave.
4. A 1-Wire device never appears: 1-Wire is probed only on the pins listed in
   `bus.onewire_pins`, and it needs a 4.7 kΩ pull-up.

## A tool is missing from `tools/list`

- `GET /api/tools`: each entry has `visible` and `missing`. A non-empty `missing` means the
  hardware was not detected.
- Did the user disable it? `mcp.disabled_tools`.
- With `tools_mode = "generic"` the device tools are collapsed into `device_invoke`; switch
  back to `auto` if you want them individually.
- Manifest tools: check that `enabled` is not `false` and the name is legal (`a-z0-9_-`
  and `.`).

## Applying a manifest fails

The returned `issues[].path` is a JSON Pointer, so it points straight at the problem:
- `/tools/0/exec/pin` → pin out of range
- `/tools/2/name` → illegal or duplicate name
- `/devices/1/driver` → driver id not in the table in `docs/capabilities.md`
- a `sequence` total-duration complaint → reduce `ms` or the number of steps

## An out-of-range call is refused

That is by design: `safety.min/max` and `params.min/max` are both checked on the board, and
the message carries the range, e.g. `` `ms` = 90000 is outside 10..=5000 ms ``. Change the
argument, not the firmware.

## The panel stays dark or shows garbage

- Confusing 128x32 with 128x64 garbles it: set `pins["oled.height"]`, or `options.height` in
  the manifest.
- SH1106 clones: `options.controller = "sh1106"` (column offset 2).
- Pure noise: `oled_contrast 0xCF` first, then check power and the 4.7 kΩ pull-ups.

## Performance and stability

| Symptom | Cause / fix |
|---|---|
| Connections refused (503) | The concurrent connection limit is 6; close stale browser tabs |
| A tool call takes > 500 ms | The log warns; the I2C device is slow, or it is a 1-Wire conversion (DS18B20 is fixed at 750 ms) |
| Out of memory | Lower `log.ring`, reduce concurrency, switch `tools_mode` to `generic` |
| Sessions pile up | `session_ttl_s` (900 s default) and `max_sessions` (4 default) reclaim them |
| `could not spawn connection thread` | Internal RAM exhausted (PSRAM cannot back a stack); close streams or lower the connection limit |

## The simulator

- `cargo run -p esp32-agent-sim -- --port 18080 --state sim-state/kv.json`
- `--empty`: a bare board (to verify that the tool list is empty with no hardware).
- To start over: delete `sim-state/kv.json`.

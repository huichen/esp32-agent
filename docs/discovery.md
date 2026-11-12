# Manual discovery: an empty board the agent fills in

The board can start with **no devices at all** and let the agent find and declare its own
hardware. This page covers that; the language a script is written in is
[scripting](scripting.md).

By default (`devices.discovery = auto`) a driver that recognises a device adopts it, and it
appears in `tools/list` straight away. Set it to `manual` and the drivers still probe — they
just stop deciding:
```bash
curl -sS -X POST http://<board>:80/mcp -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"tools/call",
  "params":{"name":"board","arguments":{"command":"config_set","arguments":{"patch":{"discovery":"manual"}}}}}'
```

`devices_list` then answers with an empty device list and a **candidate** list:

```json
{"count": 0, "devices": [], "candidates": [
  {"id":"ssd1306","kind":"ssd1306","bus":"i2c","address":60,"adopted":false,
   "adopt": {"devices":[{"id":"ssd1306","kind":"ssd1306","driver":"ssd1306",
                         "bus":0,"address":60,"options":{"height":"32"}}]},
   "note":"manifest apply: this is the body to send, with mode merge"},
  {"id":"mic","kind":"mic","bus":"i2s","adopted":false,"adopt":null,
   "note":"provided by the board profile; there is nothing to declare"}
]}
```

`adopt` is not a suggestion — it is the exact body `manifest` accepts with `command: apply`, because
`instantiate` reads `bus` as a number, omits a missing address rather than sending `null`,
and takes an OLED's height from `options`. `adopt: null` marks hardware that only the board
profile can provide (I2S audio): declaring it would be a no-op that reads like a success.

**Hot-plug**: `devices_probe` re-runs the scan. A device that was unplugged leaves the
candidate list on the next probe, and a manifest entry whose hardware is gone fails to
instantiate with `device \`x\` failed: …` in the log — the declaration stays (it is
configuration), the device simply is not live.

## The whole loop, measured

On the bench board, after `manifest_reset` with `discovery = manual`:

```text
1. empty the manifest:  devices: []                     tools: 38
   candidates: ssd1306/buttons/led/mic/speaker, all adopted=false

2. one script:
   adopted ssd1306 from i2c
   adopted buttons from gpio
   adopted led from gpio
   added 3 device(s) of 5 candidate(s)       steps: 244

3. after: [ssd1306, buttons, led]                       tools: 47
```

The five tools the OLED brought back are the five `oled_*` calls; `mic` and `speaker` are
profile-provided and were correctly left alone. Running the same script again adopts
nothing: the second run is visibly a no-op.

## Related

- [Scripting](scripting.md): the language a discovery script is written in, including the
  `tool()` call it uses to reach `manifest` (command `apply`).
- [Capabilities](capabilities.md): what the probe can and cannot recognise.
- [Manifest](manifest.md): the shape `adopt` is written in.

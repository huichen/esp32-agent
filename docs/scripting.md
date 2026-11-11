# Manual discovery and the scripting language

Two features that fit together: the board can start with **no devices at all**, and the
agent can write a **script** to find the hardware and declare it.

## 1. An empty board the agent fills in

By default (`devices.discovery = auto`) a driver that recognises a device adopts it, and it
appears in `tools/list` straight away. Set it to `manual` and the drivers still probe — they
just stop deciding:
```bash
curl -sS -X POST http://<board>:8080/mcp -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"tools/call",
  "params":{"name":"config_set","arguments":{"patch":{"discovery":"manual"}}}}'
```

`devices_list` then answers with an empty device list and a **candidate** list:

```json
{"count": 0, "devices": [], "candidates": [
  {"id":"ssd1306","kind":"ssd1306","bus":"i2c","address":60,"adopted":false,
   "adopt": {"devices":[{"id":"ssd1306","kind":"ssd1306","driver":"ssd1306",
                         "bus":0,"address":60,"options":{"height":"32"}}]},
   "note":"manifest_apply {mode: \"merge\", manifest: adopt}"},
  {"id":"mic","kind":"mic","bus":"i2s","adopted":false,"adopt":null,
   "note":"provided by the board profile; there is nothing to declare"}
]}
```

`adopt` is not a suggestion — it is the exact body `manifest_apply` accepts, because
`instantiate` reads `bus` as a number, omits a missing address rather than sending `null`,
and takes an OLED's height from `options`. `adopt: null` marks hardware that only the board
profile can provide (I2S audio): declaring it would be a no-op that reads like a success.

**Hot-plug**: `devices_probe` re-runs the scan. A device that was unplugged leaves the
candidate list on the next probe, and a manifest entry whose hardware is gone fails to
instantiate with `device \`x\` failed: …` in the log — the declaration stays (it is
configuration), the device simply is not live.

## 2. The language

Small on purpose: the board has ~40 kB of free internal RAM, and the language exists so the
model can express **loops and conditions over tool calls** that a fixed tool vocabulary
cannot. It is Turing-complete (assignment plus `while`), has no pointers and no user
functions, and cannot reach anything the caller cannot.

### How a model learns it

`script` describes itself, because a tool description is one paragraph and a language is
not:

| Call | Answer |
|---|---|
| `{"help": true}` (or `{}`) | the overview: values, statements, operators, access, limits |
| `{"section": "builtins"}` | the six functions and what `tool()` may reach |
| `{"section": "examples"}` | two runnable programs |
| `{"lint": true, "source": "…"}` | check it without running: syntax errors with line and column, and warnings for unknown function names |

The lint exists because `tool()` calls have side effects: "run it and see" is the worst way
to find a typo, and a syntax error at line 12 after three tool calls is worse still. Each
section is under 2 KB, the cap on a tool result.

```text
{"ok": false, "errors": ["line 1, column 5: expected a name after `let`, found Assign"]}
{"ok": true,  "warnings": ["unknown function `speaker_ton`; the builtins are len, str, …"]}
```

```text
let scan = tool("i2c_scan", {})          # any board tool, same permissions
let n = len(scan.addresses)
let i = 0
while i < n {
  log("address " + scan.addresses[i])    # up to 64 lines
  i = i + 1
}
if n > 0 { "found" } else { "nothing" }  # the last expression is the result
```

| Feature | Notes |
|---|---|
| Values | number, string, bool, nil, list, map |
| Statements | `let`, assignment (`x = …`, `l[0] = …`, `m.k = …`), `if`/`else`, `while`, `break`, blocks |
| Operators | `+ - * / %`, `== != < <= > >=`, `and or not`, unary `-` |
| Builtins | `len`, `str`, `num`, `type`, `log`, `tool(name, args)` |
| Comments | `#` and `//` |
| Maps | `{a: 1}` reads like JSON; `{a = 1}` also parses |

`tool(name, args)` is the only way out of the interpreter: it calls the same dispatcher an
MCP client would, so a script has exactly the caller's reach, `mcp.disabled_tools` included.
`tool("script", …)` is refused — recursion through the tool layer is how a 40 kB board runs
out of stack.

### Bounds

| Limit | Value | Why |
|---|---|---|
| Steps | 50 000 | a runaway `while` stops with a message instead of a reboot |
| Nesting | 32 blocks | the interpreter recurses on the call stack |
| String / list / map | 4 KB / 256 / 64 | these live in internal RAM |
| Log lines | 64 | the result travels as a tool result (2 KB) |
| Source | 8 KB | a model will not write more, and the check is free |

### Result shape

```json
{"ok": true, "result": <last expression as JSON>, "output": ["…"], "steps": 244}
```

Integers stay integers across the boundary (`address: 60`, never `60.0`), because a script
that copies a number out of a tool result and back into `manifest_apply` would otherwise be
refused by a schema that asks for a `u8` — a round-trip bug that reads like a validation bug.

## 3. The whole loop, measured

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

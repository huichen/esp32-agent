# The scripting language

`script` is for what the fixed tools cannot express: a loop over readings, a branch on a
result, a procedure called twice. It is Turing-complete (assignment plus `while`), has no
pointers, and cannot reach anything the caller cannot.

Discovery scripts — the ones that probe an empty board and declare what they find — are in
[discovery](discovery.md).

## Arguments and output

A script can take arguments, so one procedure serves several pins:

```json
{"source": "tool(\"led_set\", {color: args.colour})", "args": {"colour": "#00ff00"}}
```

`args` is a map of whatever the caller passed; `argv` is its values in declaration order.
Both exist even when nothing was passed (`args` is then empty and a missing key reads as
nil), so a script can be written before its arguments are decided.

Output is the `output` array on the result: `print`, `echo` and `log` all write to it, one
line each, and the caller gets the lines back alongside `result` and `steps`. It is capped at
64 lines — a script that prints more is stopped with a message rather than filling the 2 KB a
tool result is allowed.

```text
print("temperature " + str(t))     -> output: ["temperature 24.5"]
```

There is no stdin and no terminal: a script reads the board through `tool()`, and its stdout
is the array it hands back.

## How a model learns it

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
| Builtins | `len`, `str`, `num`, `type`, `log`, `print`, `echo`, `sleep(ms)`, `tool(name, args)` |
| Comments | `#` and `//` |
| Maps | `{a: 1}` reads like JSON; `{a = 1}` also parses |

`tool(name, args)` is the only way out of the interpreter: it calls the same dispatcher an
MCP client would, so a script has exactly the caller's reach, `mcp.disabled_tools` included.
`tool("script", …)` is refused — recursion through the tool layer is how a 40 kB board runs
out of stack.

## Bounds

| Limit | Value | Why |
|---|---|---|
| Wall clock | 30 s | a runaway `while` stops with a message instead of a hung board |
| `sleep(ms)` | 2 s per call, 5 s per script | it is a real wait on the thread that answers the portal |
| Steps | 5 000 000 | the backstop when a loop does nothing but compute |

`sleep` waits for real — the thread is parked in the scheduler, not spinning — but the caller
still waits for the script to return, so the portal does not answer until it does. That is why
the caps are seconds and not minutes: a job that has to wait longer is a scheduled task
(`every_s`), whose wait costs nothing because the board keeps serving in between.
| Nesting | 32 blocks | the interpreter recurses on the call stack |
| String / list / map | 4 KB / 256 / 64 | these live in internal RAM |
| Log lines | 64 | the result travels as a tool result (2 KB) |
| Source | 8 KB | a model will not write more, and the check is free |

## Result shape

```json
{"ok": true, "result": <last expression as JSON>, "output": ["…"], "steps": 244}
```

## Running one over HTTP

The same interpreter is a route, for a client that would rather not wrap it in JSON-RPC:

```bash
curl -sS -X POST http://<board>:80/api/script -H 'Content-Type: application/json' \
  -d '{"source":"1 + 2"}'
# {"id":"s1","state":"done","async":false,"ok":true,"result":3,"output":[],"steps":3,"ms":2}
```

`POST /api/script` requires the PIN, takes `{source, async?}` and uses the same 8 KB source
limit. Without `async` it waits for the run and answers with `result`, `output`, `steps`,
`ms` and `error`. With `"async": true` it answers at once with `{id, state, poll}` and the
work runs in the background; poll `GET /api/script/<id>` for `state`, the result and the
error. The tool (`script`) is the same code with a smaller wrapper — a script reaches the
same dispatcher, so nothing a script can do here is out of reach of the tool.

Integers stay integers across the boundary (`address: 60`, never `60.0`), because a script
that copies a number out of a tool result and back into `manifest_apply` would otherwise be
refused by a schema that asks for a `u8` — a round-trip bug that reads like a validation bug.

# The scripting language

`script` is for what the fixed tools cannot express: a loop over readings, a branch on a
result, a procedure called twice. It is Turing-complete (assignment plus `while`), has no
pointers, and cannot bypass the caller's tool permissions.

Discovery scripts — the ones that probe an empty board and declare what they find — are in
[discovery](discovery.md).

## Arguments and output

A script can take arguments, so one procedure serves several pins:

```json
{"source": "tool(\"led_set\", {color: args.colour})", "args": {"colour": "#00ff00"}}
```

`args` is the map of named arguments; `argv` contains those values in **key order**, not
JSON insertion order. A shell argument array keeps its positional order. Both globals exist
even when nothing was passed (`args` and `argv` are then empty, and a missing key reads as
nil), so a script can be written before its arguments are decided.

Output is the `output` array on the result: `print`, `echo` and `log` all write to it, one
line each, and the caller gets the lines back alongside `result` and `steps`. More than
64 lines stops the script with an error. That is an independent interpreter limit, not
the current 64 KiB agent tool-text ceiling; use file tools rather than log output for
bulk data.

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
| `{"section": "builtins"}` | builtin functions, output aliases and what `tool()` may reach |
| `{"section": "examples"}` | runnable programs |
| `{"lint": true, "source": "…"}` | check without executing: syntax errors with line/column, warnings for unknown function names and literal recursive `tool("script", …)` calls |

Linting executes no tools. It recognizes top-level user functions, including forward calls
and mutual recursion, in the same way the runtime hoists them. A definition hidden inside
a block is not hoisted. Each reference section retains a compact 2 KiB policy to avoid
repeatedly spending context on a manual; 2 KiB is not the global tool-result ceiling.

```text
{"ok": false, "errors": ["line 1, column 5: expected a name after `let`, found Assign"]}
{"ok": true,  "warnings": ["unknown function `speaker_ton`; the builtins are len, str, …"]}
```

```text
let scan = tool("i2c_scan", {})
for address in scan.addresses {
  log("address " + str(address))       # up to 64 lines
}
if len(scan.addresses) > 0 { "found" } else { "nothing" }
```

| Feature | Notes |
|---|---|
| Values | number, string, bool, nil, list, map |
| Statements | `let`, assignment (`x = …`, `l[0] = …`, `m.k = …`), `if`/`else`, `while`, `for`, `break`, `return`, blocks |
| Functions | `fn name(a, b) { … }`; top-level definitions are hoisted; bounded recursion is supported |
| Operators | `+ - * / %`, `== != < <= > >=`, `and or not`, unary `-` |
| Builtins | `len`, `str`, `num`, `type`, `log`, `print`, `echo`, `sleep(ms)`, `tool(name, args)` |
| Comments | `#` and `//` |
| Maps | `{a: 1}` reads like JSON; `{a = 1}` also parses |

`for` iterates list values, map keys or string characters. `return` exits the current
function (or the whole script at top level), including from a `while`, `for` or nested
conditional. It does not merely leave that loop:

```text
fn first_positive(values) {
  for value in values {
    if value > 0 { return value }
  }
  return nil
}
first_positive([-2, 0, 7])
```

`tool(name, args)` is the only way out of the interpreter. It uses the same execution
checks as MCP, including `mcp.disabled_tools`; listing profiles do not revoke an enabled
legacy name. `tool("script", …)` is refused to prevent recursion through the tool layer.
Use the argument names in the tool's description: `(required)` marks required fields and
`?` marks optional ones. A failed call with a parameter error lists the available arguments.

## Bounds

| Limit | Value | Why |
|---|---|---|
| Wall clock | 30 s, or less | stop a runaway program with a message; a script started by a tool call inherits that call's remaining budget, so the smaller of the two is the deadline |
| `sleep(ms)` | whatever the call has left | one budget instead of a second cap: a wait longer than the remaining milliseconds is refused with both numbers |
| Steps | 5 000 000 | backstop for a loop that only computes |
| Nesting | 32 blocks | bound recursive interpreter execution |
| Function recursion | 32 calls | bound stack frames and local maps |
| String / list / map | 4 KiB / 256 items / 64 keys | independent per-value growth policies, not the board's total heap |
| Log lines | 64 | bound run output independently of transport limits |
| Source | 8 KiB | bound lexing and parsing work |

`sleep` parks the thread rather than spinning, but the caller still waits. A script run by
a tool call shares that call's deadline (`agent.tool_timeout_s`, 30 s by default), so it
cannot outlive the call that started it, and a `sleep` is bounded by what is left rather
than by a constant of its own. Synchronous
scripts execute under the shared App lock, so other callers needing that lock are delayed;
a background job is not permission to monopolize hardware indefinitely. Longer waits belong
in scheduled work (`every_s`), whose idle interval does not hold the App lock.

## Result shape

`result` is the last expression or an explicit return value. For example:

```json
{"ok": true, "result": 3, "output": ["done"], "steps": 244}
```

When a script admits file edits, its MCP/internal-agent tool response also reports
`queued: true` / `saved: false`. Execution success is not storage completion. A later
command error can follow an earlier admitted edit; that error reports the partial effect.
See [storage acknowledgements](storage.md#workspace-publication-and-rollback).

## Running one over HTTP

The same interpreter is a route, for a client that would rather not wrap it in JSON-RPC:

```bash
curl -sS -X POST http://<board>:80/api/script -H 'Content-Type: application/json' \
  -d '{"source":"1 + 2"}'
# {"id":"s1","state":"done","async":false,"ok":true,"result":3,"output":[],"steps":3,"ms":2}
```

`POST /api/script` requires the configured portal credential, takes `{source, async?}`
and uses the same 8 KiB source limit. Without `async` it waits for the run and answers with
`result`, `output`, `steps`, `ms` and `error`. With `"async": true` it answers with
`{id, state, poll}`; the background worker runs it even without a pending model turn.
Poll `GET /api/script/<id>` for execution state/result/error. Only four job records are
retained. `state: "done"` means the program finished, not that its file edits reached flash;
check the `store` object in `GET /api/status` for persistence outcomes.

Integers stay integers across the tool boundary (`address: 60`, not `60.0`), so copying a
number from a tool result into `manifest_apply` does not create a spurious `u8` validation
failure.

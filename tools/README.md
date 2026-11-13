# `tools/` — what a bench verification needs

These two are a harness, not a gate: `scripts/` proves the code works on this machine,
`tools/` proves it works on the board in front of you. Nothing here is run by
[AGENTS.md](../AGENTS.md)'s gate, and nothing here can run in CI — a board has to be
plugged in and reachable.

| File | What it is | Needs |
|---|---|---|
| `bench-agent.ps1` | drives the board's agent loop end to end: point it at `stub_model.py`, send a message, watch a tool call come back, then wait for a scheduled task to fire on its own | a flashed board on the LAN, `powershell` |
| `stub_model.py` | a stand-in OpenAI-compatible chat endpoint, so the loop is provable with no API key and no provider: the first request answers with a `board {command: "status"}` tool call, the second quotes the tool result back | `python3` (standard library only) |

```bash
python tools/stub_model.py --port 8899
powershell -File tools/bench-agent.ps1 -Board 192.0.2.2 -Port 8080 `
  -ModelUrl http://192.0.2.10:8899/v1/chat/completions
```

The defaults are the addresses the bench uses; pass your own. The stub must be
reachable from the board, so put it on the same network and give `-ModelUrl` the
machine's LAN address, not `127.0.0.1` — from the board's side that is the board.

Only `bench-agent.ps1` exists in PowerShell; it needs a Windows machine on the same
network as the board, and there is no POSIX counterpart yet. The model side is already
portable (`stub_model.py`), so a `bench-agent.sh` would be a translation of the HTTP
calls and nothing else.

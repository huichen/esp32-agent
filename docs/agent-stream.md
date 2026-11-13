# Agent console: the live journal

The turn is phased (the model call runs without the app lock), and the transcript and
journal use the host agent's event names. This page is the endpoint contract for the run
journal and for `agent.tick_ms`.

## 1. `GET /api/agent/stream?after=<seq>`

Server-Sent Events, `Content-Type: text/event-stream`, `Cache-Control: no-cache`.

- replay every journal event with `seq > after`, newest last, then follow live;
- one event per frame:
  ```
  id: 42
  event: tool_use
  data: {"seq":42,"event":"tool_use","at_ms":...,"tool_call_id":"c1","name":"board","text":"{}"}
  ```
  `data` is exactly the JSON object `Event::json()` produces; `id` and `event` repeat `seq`
  and `kind` so a browser `EventSource` can use them;
- a comment heart-beat `: ping` every 15 s;
- **bounded**: the stream closes itself after 30 s with no new event, or after 120 s in
  total, whichever comes first. The portal closes the failed `EventSource` and reconnects
  explicitly with `after=<last seq>` while a run is busy. This server reads `after`, not
  `Last-Event-ID`; a generic SSE client must update the query on reconnect;
- at most **2** concurrent streams; a third gets `503 {"error":"too many event streams"}`.
  ordinary requests share one accept thread, and these streams use separate threads.
  `MAX_CONNECTIONS = 6` caps spawned response streams, not all open TCP sockets; parked
  sockets have a separate limit. A journal-gate refusal carries `Retry-After: 5`;
- the stream, polling journal and transcript require the portal credential when
  `auth.portal_pin` is configured. `EventSource` can use `?token=<minted portal token>`
  with the value URL-encoded; this accepts an issued, unexpired read token, not a raw PIN.
  The portal adds it when
  reconnecting. See [security](security.md).

## 2. `tool_progress`

One extra journal kind, emitted by the agent around each tool execution:

```
{"seq":43,"event":"tool_progress","at_ms":...,"tool_call_id":"c1","name":"board","phase":"start"}
{"seq":47,"event":"tool_progress","at_ms":...,"tool_call_id":"c1","name":"board","phase":"end","elapsed_ms":12}
```

Order inside one tool call: `tool_use` → `tool_progress(start)` → [`tool_result`] →
`tool_progress(end)`. The board cannot report progress *within* a tool, so the pair marks
the boundaries and carries the elapsed time — a client that ignores it loses nothing.

`GET /api/agent/events?after=<seq>` is the same journal as plain JSON, for a client that
cannot hold a connection open.

## 3. `agent.tick_ms`

`GET /api/agent/config` reports `tick_ms`; `POST` accepts `{"tick_ms":500}`, clamped to
`200..=5000`, default `1000`. It is the platform loop's sleep interval, not a maximum
queue-drain latency during a long model/tool call. Simulator and firmware ticks evaluate
the schedule and run a queued asynchronous script job before driving the pending agent
turn. The firmware reads the interval each tick, so an edit takes effect on a following
tick, not on reboot; a busy loop may reach that tick later.

## 4. What must not change

MCP method names, `/mcp` path, tool names, the journal event names and their order (the
test `the_run_journal_uses_the_host_agents_event_names_in_order` pins it), the
one-run-at-a-time queue, and the "no lock across the model call" property
(`a_run_in_flight_does_not_hold_the_app_lock`).

Related: [agent loop](agent-loop.md) · [web console](web-console.md) · [HTTP API](http-api.md)

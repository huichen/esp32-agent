# Concurrency and lock ownership

## What is separated

`App` still owns mutable device/tool/workspace state behind `Mutex<App>`.
Removing that mutex would permit concurrent mutation of a shared I2C bus, device
registry, manifest and workspace. This change does **not** promise parallel tools
on those resources, asynchronous media playback, or forced driver cancellation.

Three paths are independent of a tool holding App:

- `GET /api/status` and `GET /healthz` read an immutable `Arc<StatusView>`.
  The small view lock is held only for Arc clone/swap. JSON construction,
  serialization and destruction of the old view occur outside that lock.
- `POST /api/agent/cancel` checks the live run-control state and portal credentials
  under a separate short-lived mutex. It does not acquire App. A configured MCP
  path still takes precedence; no new credential bypass is introduced.
- HTTP request parsing/handling, TLS handshakes and SSE bodies run on bounded
  workers, not on the accept/readiness thread. Otherwise even a lock-independent
  handler would still queue behind an inline MCP tool.

The ESP-IDF request stack is 16 KiB. Four reusable workers are created at server
startup: three ordinary/TLS/stream workers and one reserved plaintext control
worker. Their stacks reserve 64 KiB in total, in addition to the existing 16 KiB
listener stack. Ready requests enter bounded queues; requests do not create or
destroy pthreads. Idle plaintext keep-alive sockets do not occupy a worker.
The intermediate per-request-thread design failed real-board sustained churn
with `pthread: Failed to create task`; it was not accepted. Reserving a slot alone
was not enough: the control stack must already exist. Six admitted
connections bound executing, idle and returned sockets together. Ordinary bursts
wait in this bounded pending set; pending sockets expire after 30 seconds.
TLS saturation closes the new connection without starting another handshake.
Connection-budget exhaustion also closes the new socket. This is resource
bounding, not immunity to connection exhaustion or denial of service.

The plaintext control lane classifies a complete request line, including one split
across TCP packets, then uses the ordinary parser, body limits and handler. It is
not an authentication shortcut. HTTPS handshakes use the ordinary lane because an
encrypted path is not known before handshake. A custom MCP path colliding with
`/api/agent/cancel` retains MCP semantics, not a cancellation guarantee.

## Publication and freshness

Only server construction **after radio startup** and the platform maintenance
loop build full status JSON. The firmware's 20 KiB `mcp-main` task publishes every
500 ms when it can acquire App. The simulator publishes on its configured tick.
The 6 KiB store writer, App constructor, nested tools and agent reply handlers do
not construct or publish a full status view.

Responses expose `snapshot: true`, `age_ms`, and `stale` (age over 1 second).
Heap, device and storage information can therefore be old during a long tool;
`age_ms` is not hidden. Uptime advances from the last monotonic reading plus elapsed
`Instant` time, independent of SNTP. `agent.run_active`, `agent.active_tool`
(`name` and `owner`: `agent` or `external`), and `mcp.sessions` are overlaid
from the live run-control state rather than reported from an old snapshot.
Nested tool activity restores its parent on completion or unwinding. A growing
`age_ms` alone is not proof that an Agent is executing a tool: model calls,
queued writes and other callers can also delay publication. The regression
waits for an Agent-owned `script`, after a startup idle window, before cancelling. Control-plane HTTP
and auth-failure counters are merged into App at the next control-state sync.

Portal PIN/token policy is synchronized immediately after route/tool changes,
not deferred to full status publication. Cancellation accepts the same direct PIN
and unexpired minted bearer token forms as the portal write gate. URL `token=`
does not authorize a POST. Failed authentication does not set the cancel flag.

## Lock order and cancellation

- A producer may hold App while briefly updating the status-view/run-control lock.
- A status/cancel reader never acquires App, hardware, backend or queue locks.
- A storage drain releases its backend locks before acknowledgment acquires App.
  `StoreQueue::space()` itself reads cached statistics, not the backend.
- Radio publication releases its WiFi status mutex before acquiring App for
  configuration. A claimed reverse WiFi-status-to-App edge was not found in the
  current source; previous boot crashes are not proof of that cycle.

Cancellation is cooperative. Script execution checks at bounded interpreter step
intervals; script and shell sleeps check every 25 ms, including when the configured
tool deadline is disabled. Existing synchronous socket/driver calls cannot be
forcibly withdrawn. Their next boundary observes the flag. A completed turn clears
its cancellation state so it does not leak into the next turn.

## Lifetime-safe task inspection

`uxTaskGetSystemState` copies scalar task statistics, but its returned `pcTaskName`
and task handle may become invalid when a request worker exits. The firmware never
dereferences those pointers after enumeration. Firmware-owned tasks register an
owned label and affinity from their own live task; unknown tasks use `task-<id>`
and `core: null`, `core_known: false`. A known `core` means affinity, **not** the core
executing a task at the instant of the HTTP request. Stack high-water marks and
numeric task IDs come from the scalar snapshot. Owned labels use the live task's
`vTaskGetInfo().xTaskNumber`, matching the system snapshot trace/creation ID.
`uxTaskGetTaskNumber()` is a separate application tag (normally zero), not this
lookup key.

## Reproduce the real-server checks

```bash
python scripts/test-concurrency.py --host http://192.168.3.7 --agent --soak 600
```

The script exercises shell sleep, script sleep plus a nested tool, two and three
serialized long tools, five seconds of buffered silent PCM, status/health
responsiveness, real Agent-loop interruption, and uptime during a soak with
repeated real script calls in flight. It creates and drops only
its own `_mutex_probe` clip. `--agent` adds one test turn and temporarily points
three public model settings at the **board's own** scripted endpoint, then restores
them in `finally`; it never reads a credential file or sends the existing model key
to a different host. Omit `--agent` when configuration must not change.

The default responsiveness assertion is 1 second; measured latency, not that
assertion, must be reported as board evidence. Warm serial resets, cold power cuts,
TLS stack headroom, real-provider cancellation and hardware sound/display quality
are separate checks. Passing host concurrency tests does not establish them.

# The agent loop on the board

An agent loop that runs **on the ESP32** and talks to an OpenAI-compatible, Responses or
Anthropic endpoint over HTTP or HTTPS, using the board's own tools as its tool set:

    user message / scheduled task / sensor rule
        -> queue (depth 1, coalesce)
        -> agent thread: model call -> tool calls -> tool results -> ... -> final text
        -> history (RAM + LittleFS), visible on the portal

A turn is **reactive**: something arrives, the agent answers it. Nothing on this board
works towards an objective on its own between turns.

## 1. Constraints that shape the design

- **One OS process, several FreeRTOS tasks.** HTTP server threads, the radio thread, one
  agent thread and one scheduler tick. No process isolation; every shared structure is
  behind the existing `App` mutex or a dedicated `Mutex`.
- **One agent run at a time.** The queue holds exactly one pending message; a trigger
  while a run is in flight *coalesces* into it (newest text wins, source list appended).
  Never two concurrent model calls.
- **TLS for `https://`, on the board.** The transport carries the scheme: plain `http://`
  over a socket (a LAN endpoint, and the board's own `/selftest/chat/completions`),
  `https://` over esp-tls with the ESP-IDF certificate bundle. There is no
  "skip verification" switch. Inbound traffic — the portal and `/mcp` — is a separate
  listener; port 80 stays open by default and 443 serves the same routes with the
  board's own certificate. See [security](security.md).
- **Bounded payloads.** History ring (at most 1,024 messages / 2 MiB), one user message
  32 KiB, reasoning 4 KiB, tool result 64 KiB (cut with `…(truncated)`), model reply 32 KiB,
  `agent.max_steps` model calls per turn (`0` = no limit, capped at 200 when set),
  `agent.tick_ms` clamped to 200-5000 (default 1000). The history byte budget includes
  serialized tool arguments, reasoning, attachment payloads/metadata, IDs and source,
  not just visible text. The token trigger
  and retained-turn selection use the same estimate for text, reasoning and tool
  arguments; image token costs remain provider-dependent. Trimming and turn selection
  scan the history linearly. The separate 96-entry journal stores only 512-byte text
  excerpts (truncation marker included), and the compressed index keeps at most 40 lines
  with role-specific excerpt limits, distinct from the journal's 512-byte cap.
  Byte trimming deliberately keeps one oversized newest message; the payload ceiling
  excludes allocator overhead, request copies and the archive index, so it is not a
  measured peak-RAM guarantee. Saved JSONL is serialized newest-first into one capped
  buffer (512 KiB, header included), stopping at the cap instead of serializing the whole
  history or joining a second full copy. A saved cut keeps complete user exchanges and
  call/result pairs; an exchange too large to fit is counted as dropped, never saved as
  an orphaned tail.
- **Growing data uses a filesystem.** Transcript and workspace use the 11.9375 MiB
  `storage` partition (LittleFS); configuration and secrets remain in NVS. A UTF-8 text
  file may be 256 KiB and aggregate file text 4 MiB, with separate filename/file-count
  budgets. The workspace still occupies RAM. See section 4 and [storage](storage.md).

## 2. The order of the tool list is part of the contract

A provider caches a request by its longest matching prefix, so the `tools` array has to be
byte-identical between two turns that changed nothing. The list is therefore **sorted by
name across every source** — board tools, driver tools, manifest tools and the agent-only
`read`/`write`/`edit`/`shell`/`script` group — which is stable for a given board, and the
same order an MCP client sees in `tools/list`.

`every_tool_reaches_the_model_in_one_shape_and_a_predictable_order` and
`the_tool_list_is_byte_identical_for_the_same_board` pin it. Manifest tools are not kept
in a block of their own, so a mid-turn `manifest_apply` moves only the entries it
touches.

## 3. Whose endpoint and whose key

All three of `base_url`, `model` and `api_key` are **empty in a build**, and the board
cannot reach a model until whoever owns it fills them in at the portal. Nothing is
compiled in and nothing ships in the image.

The endpoint and credential belong to the person running the board. The built-in
`/selftest/chat/completions` endpoint needs no key and never leaves the device, so it can
verify loop/tool plumbing without a provider account. A real endpoint uses the owner's
configuration; success against the stub does not verify a real provider.

`the_firmware_ships_with_no_endpoint_and_no_key` in `llm.rs` pins the default, so
hard-coding an endpoint has to be a deliberate act with a failing test to justify it.

## 4. Transcript persistence and interrupted runs

`agent.persist_transcript` (default on) saves a bounded tail, and `run_in_flight` records
that a turn started. They aid recovery; they do not establish exactly-once execution.

The transcript is `/storage/transcript`, a JSON Lines file on LittleFS. Its header carries
format version 1, a dropped-message count and a checksum over the body. Saved text is cut
at 4 KiB, reasoning at 1 KiB and tool arguments at 4 KiB; attachments are not saved.
The writer selects the newest complete exchanges/call-result pairs within **512 KiB,
header included**, and restores chronological order in place. It does not serialize the
whole history and then join a second full-size copy. A refused checksum/version/message
structure is logged and not half-adopted.

At boot, a surviving in-flight marker is consumed and the saved transcript's last user
message is queued with `source: "resume"`. Saved tool results are passed back to the model.
That is context for continuing a turn, not proof that the model will never repeat a call:
a hardware effect can happen before its result reaches storage. A power cut can also lose
the pending queue entry, model output, marker update or final transcript save. Model work
may be requested and billed again.

**Acknowledgement matters.** The firmware queues ordinary configuration/transcript
writes. A successful `set` or `queued: true` configuration response means admission, not
that flash has finished. A later writer refusal can leave RAM newer than the persisted
value; inspect `store.pending`, `failed` and `last_failure`. File-tool replies also report
`queued: true` / `saved: false`; completed publication receipts govern later saved-hash
acknowledgements, not a blocking tool response. Per-file replacement uses a synced
temporary file and rename, but the host-tested ordering/recovery code is not a board
power-cut measurement. The detailed backend guarantees and mount policy are in
[storage](storage.md).

### The file workspace

| Budget / layout | Current value |
|---|---|
| Physical index | `/storage/ws.index`, mapping logical paths to short physical keys |
| File values | `/storage/ws.<12 hex digits>`; one UTF-8 text value per file |
| Text per file | 256 KiB (`WORKSPACE_FILE_MAX`) |
| Aggregate text | 4 MiB (`WORKSPACE_TOTAL_MAX`) |
| Files | 1,024, including empty files |
| Canonical path / all path bytes | 256 UTF-8 bytes per path / 128 KiB aggregate |
| Raw path before normalization | 1,024 UTF-8 bytes |
| Restore | validate the complete index/document and contents before adoption; preserve refused data and block mutations |

A save submits changed file values and the index instead of rewriting all workspace text.
A successful file-tool response confirms the RAM edit and queue admission, with
`queued: true` / `saved: false`; it does not wait for the required values/index publication
receipt. Only a completed receipt advances the workspace's saved hashes. Legacy document/chunk formats remain
readable. This is not an all-files transaction or a power-cut guarantee; see
[storage](storage.md) for failed-publication and recovery boundaries.

### Historical NVS failures

The 2026-11-12 record at `43b9f92` introduced `agent.transcript` (16 bytes) and
`agent.run_in_flight` (19), beyond NVS's 15-byte key limit. NVS refused them before writing,
so there is no real-device value under those names to migrate; a simulator's old scratch
state is different. Current names and generated suffixes are checked in
`crates/agent-core/src/keys.rs`.

The old workspace was one NVS JSON string under `ws`. The historical 5.7 KB of three
seeded `SKILL.md` files exceeded its roughly 4 KiB string limit: `write` answered from RAM
while NVS logged `ESP_ERR_NVS_VALUE_TOO_LONG`. That describes the old layout and seed set,
not the current per-file LittleFS workspace or the single shipped default skill. See
[the original refusal](troubleshooting.md#a-store-write-was-refused) for provenance.

## 5. Where the code lives

| File | Holds |
|---|---|
| `crates/agent-core/src/llm.rs` | the three wire protocols, the request/response shapes, `LlmConfig` defaults |
| `crates/agent-core/src/agent.rs` | the loop, history and its caps, compaction, the stored transcript |
| `crates/agent-core/src/tools/` | `shell`, `read`/`write`/`edit`, `script`, and the handlers behind them |
| `crates/agent-core/src/app.rs` | `/api/agent/*`, the scheduler tick, `ToolHost` for `App` |
| `firmware/src/runner.rs` | the agent thread and the board HTTP client |

Related: [model](model.md) · [stream](agent-stream.md) · [memory](agent-memory.md) ·
[skills](agent-skills.md) · [scheduling](scheduling.md)

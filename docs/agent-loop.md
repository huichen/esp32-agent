# The agent loop on the board

An agent loop that runs **on the ESP32** and talks to an OpenAI-compatible, Responses or
Anthropic endpoint over HTTP or HTTPS, using the board's own tools as its tool set:

    user message / scheduled task / sensor rule
        -> queue (depth 1, coalesce)
        -> agent thread: model call -> tool calls -> tool results -> ... -> final text
        -> history (RAM + NVS), visible on the portal

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
- **Bounded everything.** History ring (at most 1,024 messages / 2 MB), one user message
  32 KB, reasoning 4 KB, tool result 64 KB (cut with `...(truncated)`), model reply 32 KB,
  `agent.max_steps` model calls per turn (`0` = no limit, capped at 200 when set),
  `agent.tick_ms` clamped to 200-5000 (default 1000). The history byte budget includes
  tool arguments, reasoning and image base64, not just visible text. The token trigger
  and retained-turn selection use the same estimate for text, reasoning and tool
  arguments; image token costs remain provider-dependent. Trimming and turn selection
  scan the history linearly. The separate 96-entry journal stores only 512-byte text
  excerpts (truncation marker included), and the compressed index keeps at most 40 lines.
  Byte trimming deliberately keeps one oversized newest message; the payload ceiling
  excludes allocator overhead, request copies and the archive index, so it is not a
  measured peak-RAM guarantee. Saved JSONL is serialized newest-first into one capped
  buffer (512 KiB, header included), stopping at the cap instead of serializing the whole
  history or joining a second full copy. A saved cut keeps complete user exchanges and
  call/result pairs; an exchange too large to fit is counted as dropped, never saved as
  an orphaned tail.
- **Storage is a filesystem.** The transcript and the file workspace are files on the 12 MB
  `storage` partition (LittleFS); NVS keeps the configuration and the secrets, and nothing
  that grows. A workspace file may be 256 KB and the whole workspace 4 MB. See section 4
  for the layout and what a reboot can and cannot lose.

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

That is a rule, not an accident of defaults: the endpoint and the credential belong to the
person running the board. This repository's own bench work never points a board at a
maintainer's or an assistant's model provider, and never puts that provider's key on the
device — a bench run uses the board's own `/selftest/chat/completions` endpoint, which
needs no key and never leaves the device. If a demonstration seems to need a real key,
the answer is a scripted endpoint on the board, not somebody else's credential in flash.

`the_firmware_ships_with_no_endpoint_and_no_key` in `llm.rs` pins the default, so
hard-coding an endpoint has to be a deliberate act with a failing test to justify it.

## 4. Surviving a power cut

Two mechanisms, one storage: `agent.persist_transcript` (default on) and the
`run_in_flight` marker.

The transcript is one JSON Lines file, `/storage/kv/transcript` on the `storage`
partition, behind a header line carrying the format version, the number of dropped
messages and a checksum over the body. It used to be five to eight NVS values of 3 500
bytes, chunked because ESP-IDF refuses a string value above about 4 000 bytes; the
filesystem has no such ceiling, so it is one value and the ceiling is `MAX_SAVED_BYTES`.

A key name is a limit too: NVS keeps 15 bytes (`NVS_KEY_NAME_MAX_SIZE` is 16 with the
terminator) and refuses a longer one with `ESP_ERR_NVS_KEY_TOO_LONG`, storing nothing.
The names used to be `agent.transcript` (16 bytes) and `agent.run_in_flight` (19), so
neither was ever written — see `docs/troubleshooting.md` §"a write was refused". Every key
name now lives in `crates/agent-core/src/keys.rs`, where `all()` enumerates the shapes the
assembly can produce (including a two-digit chunk number) and
`every_key_the_board_can_generate_fits_nvs` fails on a longer prefix, a longer separator or
a wider index.

**There is no migration, and that is not an omission.** Both names were introduced already
over the ceiling (`43b9f92`), and NVS refused the value before writing any of it, so no
board ever had a byte under `agent.transcript` or `agent.run_in_flight` to migrate. Nothing
reads the old names either: a key that never existed cannot hold a conversation, and a
fallback for it would only hide the next such mistake. A simulator's state file is the one
place an old name could exist — that is a developer's scratch state, not the board's.

The stored copy is lossy on purpose: text over 4 KB, reasoning over 1 KB and tool
arguments over 4 KB are cut with the truncation marker, images are not stored at all,
and only the newest messages that fit 512 KB are kept (`dropped: N` in the header,
the prompt as "Recovered after a reboot"). A blob with a bad checksum, an unknown version,
an edited body or an orphan tool result at the tail is discarded whole and logged, so a
torn write loses the transcript rather than half-adopting it.

Resume works at step granularity. The marker is written before a run and removed when it
ends, before the final save, so a crash can lose a reply but never repeat one. At boot a
marker that survived is consumed and the transcript's last user message is queued again
with `source: "resume"`; the request then carries the tool call and the result that already
landed, so the model answers instead of re-issuing the call.

What a power cut still costs: the model output in flight (the turn is re-asked and paid for
again), a queue entry that never started, and a cut inside the window between clearing the
marker and the final save (one turn lost, never duplicated).

### The file workspace, which had the bug the transcript never had

The workspace is the same problem and, until now, the opposite answer. It was one JSON blob
under `ws`, so once the files passed roughly 4 KB **every save failed** —
`nvs write 'ws': ESP_ERR_NVS_VALUE_TOO_LONG` in the boot log — while `write` answered
`ok: true` and `read` served the file out of RAM until the next boot took it away. Three
seeded `SKILL.md` files are 5.7 KB: the board was in that state every time it booted, and it
looked healthy from outside.

The layout is the transcript's, with one difference that matters when something goes wrong:

| | |
|---|---|
| where | `/storage/kv/ws` on the `storage` partition; NVS is not involved |
| per file | 256 KB of text (`WORKSPACE_FILE_MAX`) |
| whole workspace | 4 MB of text (`WORKSPACE_TOTAL_MAX`) |
| write order | one value, written whole: a filesystem has no chunks to order |
| read | the stored text is parsed; a torn file is dropped whole and logged |
| the old layout | `ws` + `ws.1`..`ws.5` in NVS, still read: a plain value under `ws` is migrated on the same boot |

Chunking still exists in `store::write_blob` and is still tested: it is what a backend
without `whole_value_bytes` gets, and its header (a count, the total length and one
checksum per chunk) is what makes a torn write *nameable*. The board's backend answers
`Some(..)` because a file has no 4 KB ceiling, so the workspace and the transcript are one
value each and the filesystem's own bookkeeping is the only copy.

What that costs is stated rather than implied: `std::fs::write` replaces a file, so a power
cut inside a save can lose the workspace instead of the previous version of it. SPIFFS
writes through its own pages and its own garbage collector, which narrows the window but
does not close it. What does not change is the guarantee the tools make: `write`, `edit`,
`rm`, `cp`, `mv`, a `>>` redirect, `tee`, `touch`, `base64 -o` and `wget` all go through
one save and, when it fails, **roll the change back** and answer with the numbers.

```text
the file workspace was not saved: it is 4194305 bytes and the store keeps at most
4194304 bytes, so it needs 2 values; nothing was written
The change was dropped, so `read` shows the file as it was before this call
```

So a write that cannot be stored no longer looks like one that can: `read` shows the old
file, the answer carries the numbers, and `du`/`df` report the stored ceiling next to the
text one.

## 5. Where the code lives

| File | Holds |
|---|---|
| `crates/agent-core/src/llm.rs` | the three wire protocols, the request/response shapes, `LlmConfig` defaults |
| `crates/agent-core/src/agent.rs` | the loop, history and its caps, compaction, the stored transcript |
| `crates/agent-core/src/tools/` | `shell`, `read`/`write`/`edit`, `script`, and the handlers behind them |
| `crates/agent-core/src/app.rs` | `/api/agent/*`, the scheduler tick, `ToolHost` for `App` |
| `firmware/src/runner.rs` | the agent thread, the HTTP client and the NVS workspace |

Related: [model](model.md) · [stream](agent-stream.md) · [memory](agent-memory.md) ·
[skills](agent-skills.md) · [scheduling](scheduling.md)

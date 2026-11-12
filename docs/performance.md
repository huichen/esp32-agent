# Where the time goes, and what to do about it

A line-level pass over the hot paths, ordered by what actually costs: a model step happens a
few times per turn, a tool call once, a status poll maybe once a second. Every item names the
file and the function, so the claim can be checked rather than trusted.

Measured context, from the bench: a four-tool turn takes 13.7 s and five model calls; the
console answers in 0.55 s over TLS; internal SRAM sits at 5–6 kB free after a TLS page. On this
chip the real cost of a hot-path allocation is not the microseconds, it is the fragmentation of
a heap that has 5 kB left.

## P0: paid on every model step

**1. `App::tool_specs` rebuilds the whole tool array every step** (`app.rs`, `tool_specs`).
51 tools × ~350 bytes of schema is about 18 KB of `serde_json::Value` built, walked and thrown
away on each of up to a dozen model calls in a turn. The list only changes when the manifest,
the visible set or the probe result changes.
*Fix:* keep a cached `Value` and a dirty flag, set by `manifest_apply`, `config_set`, `reboot`
and `devices_probe`. *Risk:* low; a missed invalidation shows up as a tool that does not appear.

**2. `is_visible` is O(n×m) and runs per tool per step** (`app.rs`).
Every entry does `cfg.mcp.disabled_tools.iter().any(...)` — string compares against a small
Vec, 51 times a step, plus a linear registry lookup before that.
*Fix:* build a `BTreeSet<&str>` of disabled names when the config changes; make the registry
lookup a map. *Risk:* none.

**3. `history_tokens` classifies every character of the transcript, every step**
(`agent.rs`). `needs_compaction` calls it, `trim` calls it again, and each call is
`chars().fold()` over the whole conversation: a 512 KB transcript is half a million branches
per step, so per turn it is millions.
*Fix:* keep the total as a field, updated on push/remove/compress — the only mutations.
*Risk:* a missed update drifts the trigger; a test that compacts at a boundary catches it.

**4. `Agent::emit` copies the whole reply into the event ring** (`agent.rs`).
A `text_delta` holds a `String` clone of an up-to-8 KB answer, and `MAX_EVENTS = 96` means the
ring can retain several hundred kilobytes of duplicates of things the transcript already has.
`events_json` then clones them again for the SSE response.
*Fix:* `Arc<str>` in the event, or cap event text at a few hundred bytes (the console renders
the answer from the transcript, not from the delta). *Risk:* low; SSE clients that replay
`text_delta` for the answer would need the history instead — they already poll it.

## P1: paid on every tool call, or per request

**5. `shell` serialises its own result and parses it back** (`app.rs`, `call_tool`).
`run_shell` returns a `String`, then `serde_json::from_str` turns it into a `Value`: one format
and one parse per call, for data that was built with `json!` macros in the first place.
*Fix:* return `Value` and let the tool layer decide. *Risk:* the few tests that assert on the
string.

**6. The script interpreter is quadratic on list writes** (`script.rs`, `get`/`set`).
`get` clones the value out of the scope map; `SetIndex` does get → clone → mutate → set →
clone, so filling an N-element list is O(N²) with N-sized copies. A `for` loop over 256 items
does tens of thousands of clones.
*Fix:* add `get_mut` and have `SetIndex` borrow mutably in place. *Risk:* borrow-checker
reshaping of one function; the loop tests cover it.

**7. The workspace rewrites everything on every write** (`workspace`, `AppWorkspace`).
It is one JSON blob: adding a file re-serialises all 8 KB and does an NVS write, which is both
CPU and flash wear. *Fix:* SPIFFS, or one NVS key per file. *Risk:* migration of the stored blob.

**8. Request bodies grow without a hint** (`http.rs`).
The chunked path `extend_from_slice`s into a fresh `Vec`; the Content-Length path could
`Vec::with_capacity(len)`. `Request::text()` copies through `from_utf8_lossy(..).to_string()`
per call. *Fix:* reserve, and borrow when the bytes are already UTF-8. *Risk:* none.

**9. `zip` builds its output byte by byte** (`zip.rs`).
Fine at 1 KB, but `Vec::push` per byte with no `with_capacity` is a free win while the code is
being touched.

## P2: cold, or a design note rather than a bug

**10. `Devices::infos()` clones every string on every call** — and the system prompt, the
status route and `devices_list` all call it. A borrowed slice would do.

**11. `sched_tick` polls every enabled rule every tick.** That is the feature (a sensor rule has
to read the sensor), but the cost scales with rule count × tick rate and is worth stating in
the scheduling doc rather than discovering on a board with ten rules.

**12. The model request body is built twice in memory**: a `Value` tree, then `.to_string()`.
On a 512 KB transcript that is the peak allocation of the turn, and the one most likely to fail
on a board with a fragmented PSRAM heap.

## What is already fine

The parser and the manifest compiler are cold paths (once per apply). `inflate` and the CRCs
work on borrowed slices. The HTTP server reads with a bounded buffer and rejects early instead
of buffering a hostile body. The scheduler does not scan the transcript. `tool_specs` is sorted
for provider cache stability, which costs one sort and buys a prefix cache — the right trade.

## Order I would do them in

1, 3, 4 (memory pressure per step), then 6 (a script that loops over a list), then 5 and 8
(free), then 2 and 10 while touching those files anyway. Nothing here needs a design change
except 7, which is the SPIFFS task already on the roadmap.

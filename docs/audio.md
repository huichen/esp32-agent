# Audio on this board: WAV, scores and streaming playback

The board records short microphone clips as WAV, synthesizes scores, and accepts streaming
PCM playback. There is **no on-board Opus or ADPCM encoder**. The earlier conclusion that
Opus could not fit used an obsolete internal-SRAM reading; codec feasibility remains
unverified, not disproved by that reading.

`[source]` means a configured limit or arithmetic from the implementation. `[measured]`
numbers are historical bench observations with provenance, not measurements of the audit
firmware. `[estimate]` means an unbuilt calculation. The final audit source has not been
flashed or board-tested.

## 1. What the board has

| Resource | Value | Source |
|---|---|---|
| Board | ESP32-S3 N16R8, 16 MiB flash, 8 MiB octal PSRAM, ESP-IDF 5.5.2 | `[source]` `firmware/.cargo/config.toml`, `sdkconfig.defaults` |
| App partition | `factory`: 4 MiB = 4,194,304 B; no OTA slots or OTA implementation | `[source]` `firmware/partitions.csv`, [flashing](flashing.md#3-partition-table) |
| Earlier firmware image | ~2,705,000 B, reported as 86% of an older 3 MiB app partition | `[measured]` historical report in `526c8d1` (2026-11-12); not current image size/headroom |
| Internal SRAM / DMA free | internal 168 kB, DMA 161 kB; largest block 98 kB in each | `[measured]` `93b0579` (2026-11-12), [pool readings](troubleshooting.md#how-much-memory-the-board-has) |
| PSRAM free | 7901 kB, largest block 7808 kB | `[measured]` same run, not a measurement during maximum audio/history load |
| Thread stacks | internal for flash/cache-off-sensitive paths; PSRAM allowed for constrained tasks | `[source]` `firmware/src/threads.rs`; DNS uses PSRAM |
| Microphone | 16 kHz, 32-bit slots, left channel, 24 bits used | `[source]` `crates/agent-core/src/drivers/audio.rs`, `firmware/src/esp_hw.rs` |
| One tool result | 65,536 B (`agent::MAX_TOOL_RESULT_BYTES`), truncated with a marker past that; `mic_record` sizes its 16,000 samples against it | `crates/agent-core/src/agent.rs` |

The earlier 5–6 kB internal-free and 6593 kB PSRAM-at-507-KB-history observations describe
firmware before allocator fixes (`10b16dd`, 2026-11-11), not current limits. Internal,
DMA and PSRAM readings answer different questions, and free total is not the largest
contiguous allocation. Historical TLS allocation failures remain useful diagnoses in
[troubleshooting](troubleshooting.md#the-portal-opens-over-http-but-not-https).

## 2. Codec estimates are not a build result

The original Opus discussion (`526c8d1`, 2026-11-12) estimated code, tables, state, stack
and CPU without compiling or profiling a codec for this target. It also asserted libopus
pipeline/subset details without a verified dependency build. Those estimates do not
establish that an encoder fits, fails to fit, or interoperates with a host decoder.

Opus and ADPCM remain deferred in [ROADMAP.md](../ROADMAP.md). WAV is the implemented
microphone format; host encoding avoids introducing an unverified encoder into firmware.

## 3. What would establish codec feasibility

A codec evaluation needs the exact dependency version/build flags, linked image size
against the **4 MiB** factory slot, peak heap by capability and largest block, stack
high-water marks, encode time under concurrent WiFi/TLS/I2S load, and host-decoder checks.
Cache-off/flash-writing paths must retain internal stacks; enabling PSRAM stacks does not
make every task safe there. None of these codec measurements has been completed here.

## 4. What the board does instead: WAV

`mic_record` returns a **complete RIFF/WAVE file**, base64-encoded, plus the fields needed to
read it without parsing the header:

```json
{
  "format": "wav",
  "samples": 512,
  "rate_hz": 16000,
  "bits": 16,
  "channels": 1,
  "ms": 32,
  "wav_bytes": 1068,
  "note": "a RIFF/WAVE file of 16-bit mono PCM taken from the top of the 24-bit samples; the board records WAV and a host encodes (docs/audio.md)",
  "wav_b64": "<base64 of the 1068 bytes: 44-byte header + 1024 bytes of samples>"
}
```

The header is the canonical 44 bytes (`RIFF`, `WAVE`, one 16-byte `fmt ` chunk for PCM mono
16-bit at 16 kHz, `data`), written by `drivers::audio::wav`. The payload it wraps is exactly
the PCM the tool already produced: the top 16 bits of each 24-bit I2S sample, little-endian.

This replaced the old `pcm_b64` field. One blob, not two: 16,000 samples is 32,000 B of PCM
plus a 44-byte header = **32,044 bytes of WAV / 42,728 bytes of base64**, before JSON
metadata. A second raw-PCM base64 field would add 42,668 bytes: 85,396 combined bytes
before metadata, beyond the **65,536-byte** tool-result cap. Truncating that result could
cut a base64 blob. The accepted maximum fits one result, as checked by
`the_largest_recording_still_fits_one_tool_result`.

The payload also occupies history and may be re-sent on later turns until trimming or
compaction. Provider token accounting for audio/base64 is not measured here. The schema's
*default* remains **256 samples** (16 ms); one second is a ceiling a caller opts into.

Why WAV and not "PCM, figure it out": a host that receives base64 has to guess what it is, and
the guess that matters is container vs. raw samples. A WAV is unambiguous, needs no codec, and
every tool that plays audio reads it.

### What that costs on the wire

| Format | Nominal payload bytes per second | Time within a 64 KiB result |
|---|---|---|
| WAV, 16 kHz, 16-bit mono | 32,000 PCM bytes, plus a 44-byte WAV header | implemented maximum 1 s (16,000 samples) |
| Opus, hypothetical 16 kbps | 2,000, excluding container overhead | about 24 s after base64, before metadata; not implemented |
| IMA ADPCM, hypothetical 4:1 | 8,000, excluding block/container overhead | about 6 s after base64, before metadata; not implemented |

The compressed rows are `[estimate]` wire arithmetic only, not encoder benchmarks or
supported `mic_record` modes. Compression could reduce bandwidth and history payload,
but would still need a verified encoder, framing and host-decoder contract.

## 5. Recommendation

1. Record WAV, 16 kHz mono, at most 16,000 samples per call (1 s); encode on the host.
   Longer microphone-to-file recording is **not implemented**. `POST /audio/pcm` (§6)
   is playback, not a longer recording route.
2. Treat on-board Opus/ADPCM as unverified work, not as impossible or already supported.
3. Play host-held audio through `POST /audio/pcm`, which reads the body as it arrives.
   `speaker_play.pcm_b64` has a **2048 decoded-byte argument cap**, distinct from the
   **64 KiB tool-result cap** and **1 MiB buffered HTTP body cap**.
4. Send a **tune** as `notes` (§7) when the caller would otherwise have to generate samples:
   a few hundred bytes of score rather than 240,000 bytes of five-second PCM.
5. Re-evaluate codecs using capability-specific pool/contiguous-block readings and a
   real target build, not combined free heap or historical allocator failures.

## 6. Streaming playback: `POST /audio/pcm`

`speaker_play.pcm_b64` accepts 2048 decoded bytes: 1024 mono samples, about 43 ms at
24 kHz. This is a tool-argument limit, not the 64 KiB result budget. Ordinary HTTP bodies
are buffered up to 1 MiB (`http::MAX_BODY_BYTES`); streamed media has separate limits.

`POST /audio/pcm` does not materialize the whole body. It takes raw PCM16LE,
mono, 24 kHz, framed with `Content-Length` or `Transfer-Encoding: chunked`, and hands each
piece straight to `i2s_out_write` as it arrives:

| | Value | Where |
|---|---|---|
| Sample format | PCM16LE, mono, 24000 Hz | `drivers::audio::AMP_RATE_HZ` |
| One upload | at most **30 s** = 1 440 000 bytes | `AUDIO_STREAM_MAX_BYTES` |
| Sink buffers | reused byte/sample `Vec`s, initially 1025 + 1024 bytes; socket/stereo/DMA buffers are additional | `[source]` `AudioSink` |
| New task thread | **none** — the request runs on the accept thread it already owns | `http.rs` |
| Lock held | one piece at a time (about 1 KB, 21 ms), never for the song | `AudioSink::push` |

The three numbers that matter, in the order they bite:

- **No new stack.** The upload runs inline on the accept thread. While it plays, that
  thread cannot serve a new ordinary request; other sockets wait. Existing event streams
  have their own threads. Releasing the App mutex does not make the accept loop concurrent.
- **No whole-body buffer.** PCM arrives at 48,000 bytes/s. The sink reuses piece buffers,
  and `i2s_channel_write` blocks until DMA accepts them. This is backpressure, not an
  unbounded song-sized allocation.
- **The lock is per piece.** The shared `App` mutex is released between I2S writes, so the
  agent and existing event threads can acquire it between pieces.

Two failures this path has to answer honestly, because both are easy to get wrong:

1. **An odd byte.** A socket read can end mid-sample. The byte is carried into the next piece,
   and if the body ends with one it is reported as `400` and dropped — never played as half a
   sample (which is a click).
2. **A body that ends early.** The client hung up or the amplifier refused. The answer is
   `ok: false` with the samples that *were* played, because half a song is not a song.

The amplifier itself needs no new handling: `i2s_out_write` already duplicates mono into both
slots and clears the TX descriptor afterwards, which is what keeps a finished stream from
replaying its last DMA buffer forever (the bench bug in `firmware/src/esp_hw.rs`).

The MIME essence must be exactly `audio/pcm`; parameters are allowed, prefix matches are
not. Odd final bytes and incomplete HTTP framing return `400` / `ok: false`; a hardware
write failure returns `500`. Successful responses count samples submitted to the driver,
not an independently measured acoustic waveform.

What this does **not** do: change the sample rate (`?rate=` accepts 24000 only), capture
a microphone stream, or serve two uploads concurrently. Those require additional
implementation/measurement, not a conclusion that no task could ever fit.

## 7. Playing a melody: send the score, not the samples

§6 streams PCM, and for audio a host already holds that is the right answer. A melody is the
other case: the bytes do not exist until the *caller* writes them, and here the caller is a
model. Five seconds at 24 kHz is 240,000 bytes — 320,000 as base64, far beyond this tool's
2048 decoded-byte argument cap even though below the buffered HTTP limit. A tune is
something the caller can *describe* without generating that payload.

| Form | What the caller sends | Audio it plays |
|---|---|---|
| `pcm_b64` | 2,732 B of base64 | 2048 B = 1024 samples = **43 ms** |
| `notes` | ~25 B per note — a few hundred bytes for a tune | as long as the notes add up to, at most **12 s** |
| `POST /audio/pcm` (§6) | 48,000 B for every second of audio | up to **30 s**, but the caller must already have the bytes |

So the score is not a way around the body cap — §6 already has that — it is a way around
having to produce the audio at all. `{hz, ms}` is ~25 bytes of JSON, so a fourteen-note tune
is a few hundred bytes, and the board synthesizes it into **one contiguous mono buffer**
for back-to-back playback. There is no separate network round trip between notes.

```json
{"notes": [{"hz": 523, "ms": 400}, {"hz": 523, "ms": 400}, {"hz": 784, "ms": 400}], "volume": 30}
```

### The limits, and where they come from

| Limit | Value | Why |
|---|---|---|
| Notes per call | 64 | bounded score parsing; exact JSON size depends on formatting |
| One note | 20–2000 ms, 0–8000 Hz | 20 ms keeps the 2 ms articulation ramp from swallowing the note; 8000 Hz is `speaker_tone`'s ceiling |
| Whole melody | 12000 ms | 288,000 samples = 576,000 B of mono PCM |
| Stereo staging per I2S write | 4 bytes × mono samples in that write | `[source]` `esp_hw.rs::i2s_out_write` duplicates each `i16` into two slots |
| Chunked-playback arithmetic | a 4096-sample write stages 16,384 bytes; mono + that staging = 592,384 bytes at 12 s | `[estimate]` conditional on the caller's chunk bound; excludes score, request, allocator and DMA overhead, not a measured peak |
| Arguments on the wire | a few hundred bytes | this is the point |
| Binary write needed? | **no** | the score is JSON; nothing has to be stored as a file |

`hz: 0` is a rest — the same envelope over silence — so a melody can hold a beat. Each note
starts and ends at exactly zero amplitude and the ramp is 2 ms at a note boundary, 5 ms at
the melody's ends: a note at a new frequency that starts at a non-zero value is a step, and
a step is a click. That ramp is a **dip in the envelope, not a gap in the stream** — no
samples are inserted or skipped, and the buffer is exactly the sum of the note lengths.

### The text workspace

This workspace stores text: `write` writes text files, so a rendered `.wav` cannot be kept
here, and "render a file, then play the file" is not a path this repository has (five
seconds at the amplifier rate is 240,000 PCM bytes plus a 44-byte WAV header, far beyond
the `pcm_b64` argument cap before base64). The score form needs no file at all — the
melody lives in the tool call and dies with it. A host that *does* hold bytes still has `pcm_b64` for the 43 ms it is good for.

### What it costs elsewhere

This tool holds the App lock across synthesis/playback, so `/api/status` and other tools
wait for it to return. The 12 s limit is score duration, not a wall-clock latency deadline.
§6 releases the lock between pieces; this tool does not, even if I2S is fed in pieces.
Chunking a write does not release the tool call's App lock.

The tool result carries `synth_ms` and `write_ms` so the split is measured rather than
guessed. `[measured]` historical bench timings recorded with `f95bbba` (2026-11-12),
not re-run on the audit source:

| Melody | Samples | `synth_ms` | `write_ms` | Wall clock of the call |
|---|---|---|---|---|
| 1 note, 1.2 s | 28,800 | 40 | 1,193 | 1,275 ms |
| 4 notes, 2.4 s | 57,600 | 81 | 2,399 | 2,527 ms |
| 14 notes, 4.8 s (Twinkle Twinkle) | 115,200 | 163 | 4,817 | 5,038 ms |

Two things are worth reading off that table:

- `write_ms` was close to the audio duration in those runs. That is evidence of pacing
  for those clips, not proof of gap-free output for every score or of current maximum-load
  behavior.
- The first version of this used `f32::sin` per sample and `synth_ms` was **1,915** for the
  same 14 notes, so the call took 6,779 ms to play 4,800 ms of music: the board spent 40%
  of the tune on float maths and answered nothing while it did. The table oscillator is
  11× cheaper, which is why it is there rather than for elegance. It costs 4 KB of flash
  (1,025 `f32` entries); the app partition went from 87.08% to 87.24% of 3 MiB with it.
  `[measured]` historical percentages of the then-3-MiB partition (`f95bbba`, 2026-11-12),
  not current factory-slot usage.

### Sound and picture in one tool call: `media_play`

Give `notes`, `frames`, or both. Notes use exactly `speaker_play`'s **64-note / 12,000 ms**
budget and 24 kHz mono synthesis; `hz: 0` is a rest. `volume` is 0–100, default 30. A frame
is `{at_ms, rects?, text?, line?}`; its slot is **0–12,000 ms** after playback starts,
excluding synthesis. At most **360 frames / 64 rectangles per frame** are accepted;
excess lists are refused with their counts. No `confirm` is needed.

```json
{"notes":[{"hz":523,"ms":400},{"hz":784,"ms":400}],"frames":[{"at_ms":0,"text":"START"},{"at_ms":400,"rects":[{"x":8,"y":8,"w":16,"h":8,"fill":true}],"text":"NEXT","line":0},{"at_ms":1000,"text":"END"}],"volume":30}
```

Each frame clears the framebuffer, draws rectangles using `oled_rect`'s filled/outlined
pixels (`fill` defaults to true), overlays `oled_text`'s 6×8 text and wrapping (`line`
defaults to 0), then flushes **once**. Origins, dimensions and text rows are checked
against the detected panel; legal rectangles crossing an edge are clipped. Every frame
needs non-empty rectangles or text. All parameters are validated before any audio/display
write; an invalid later frame names its index and field and plays nothing. Action/family
disables are respected; manifest-policy-wrapped actions are refused, not bypassed.

The melody is synthesized **once**, then played in **4096-sample chunks**. One monotonic
clock checks frame deadlines before the first write and between writes. Frames are
stably sorted by `at_ms`, drawn once even at equal timestamps, and never dropped for
being late. Remaining frames are waited for and drawn even after the sound ends. A full
chunk is about **171 ms** at 24 kHz `[source: arithmetic]`, so this is chunk-boundary
pacing, not exact audiovisual synchronisation. `fps` defaults to the rate implied by the
shortest positive slot spacing, clamped **1–30** (1 without spacing). It bounds idle
wakeups, not `at_ms` slots, and does not discard frames to enforce a rate.

The result is `{ok, notes, frames_drawn, frames_late, played_ms, synth_ms, write_ms,
max_late_ms}`: score-entry count, completed flushes, flushes completed after their slots
(in whole milliseconds), submitted audio duration, synthesis time, summed audio-write
time, and maximum lateness. Display work/waits are not `write_ms`; submitted samples are
not an acoustic measurement. Hardware errors carry completed-work counters in error
data. PCM/frames are not echoed, keeping the result inside **64 KiB**.

**Playback is synchronous:** the call holds its caller through synthesis, writes and
frame waits; chunking does not release its App lock. This fixes pacing, not concurrency.
Background playback needs a media worker, still on [the roadmap](../ROADMAP.md#medium-term).
Host simulator tests cover this tool; firmware not built/verified, and physical timing
has not been measured on the bench.

## 8. Sound with a picture: `POST /av`

The bench board played a 30 s clip with its animation on the 128x32 OLED: PCM down one TCP
port, one frame per MCP `display` call out of the other. The current server has no
background audio task and one ordinary accept thread, so this route carries the content
as **one body**; record order is the synchronisation.

| Record | Payload | What the board does |
|---|---|---|
| `A` | even-length PCM16LE mono at 24 kHz | blocking `i2s_out_write`; driver submission is the pacing source |
| `F` | 512 bytes (128x32) or 1024 (128x64), 1-bpp page format | `oled_bitmap`, so a 512-byte frame lands in the top half of a 64-row panel |
| `E` | nothing | end of the stream |

A host writes `F`, then the `A` for that frame's interval, then the next `F`. By the time
the board reads the next frame it has just finished playing the audio that belongs to the
one before it: there is no clock to agree on and no drift to correct, because the I2S write
**is** the clock. `tools/av_push.py` is the host side, and the probe's own strip is the
content:

```bash
python tools/av_push.py --audio clip.pcm --frames clip.1bpp --host <ip> --fps 12
# {"ok":true,"format":"av1","audio_bytes":1440000,"samples":720000,"ms":30000,"frames":362,...}
```

The costs are §6's costs: no new thread (the request runs where requests already run), no
body buffer (records are parsed as they arrive, into two reused buffers), and the App mutex
taken once per record rather than for the song. The limits differ in one place — audio is
still capped at **30 s / 1,440,000 bytes**, while the whole body is capped at
**1,964,288 bytes**: audio plus a **512 KiB** bitmap/framing allowance. A 12-fps,
30-second strip of 512-byte frames is 184,320 payload bytes, before record headers.

Use MIME essence `application/x-esp32-av` or `application/octet-stream`. Each record has
a three-byte `tag | u16le length` header. `E` must have length zero. An incomplete final
header/payload is `400` / `ok: false` even if HTTP framing completes; a hardware failure
is `500`. Ending on a complete record boundary without `E` is accepted (`ended: false`);
bytes following a valid `E` are ignored.

`[measured]` historical bench record in `e7a9623` (2026-11-12): 30.00 s of audio,
362 frames, 30.19 s wall clock; audio alone 30.03 s. These are not final-audit benchmarks.

Two things this does not do. Frames cannot run *ahead* of the audio: one way, in order, so
seeking, looping or overlaying means a second body. And the panel's row count is not guessed
— a board whose config says `oled.height = 64` on a 32-row panel gets the frame drawn into
the top half of a taller buffer, which is the same thing the register setup already got
wrong (see `docs/adding-a-device.md`, or set `pins: {"oled.height": 32}`).

## 9. How to check these numbers again

- Flash: `espflash flash --partition-table firmware/partitions.csv --target-app-partition
  factory <elf>` prints `App size … (N%)`; record bytes and percent against the current
  4 MiB slot, together with the source commit/profile. This command flashes a board.
- Internal SRAM at runtime: the `mem:` lines the firmware logs at boot, and `/api/status`
  (`docs/troubleshooting.md`).
- The WAV itself: call `mic_record` over `/mcp`, write `wav_b64` to a file after
  base64-decoding it, then `soxi`/`ffprobe` should say `16000 Hz, 1 channels, 16-bit` with the
  sample count the tool reported.
- The stream route (§6): `sox` a known 10 s clip to raw PCM16 mono at 24 kHz, `POST` it to
  `/audio/pcm` with `Transfer-Encoding: chunked`, and compare the answer's `ms` with 10000 —
  then record internal/DMA/PSRAM free and largest-block readings before, during and after
  repeated uploads. Combined free heap alone is not the allocation budget. A `samples`
  count short of `bytes / 2` means a piece was cut; `ok: false` names why.
- The melody route (§7): call `speaker_play` with `notes` over `/mcp` and time the call. Its
  `ms` field is the score duration; wall clock also includes synthesis, scheduling and
  request overhead. Record `synth_ms`, `write_ms`, total latency and pool/stack peaks.
  Check the actual output for completeness/gaps rather than inferring them from timing
  alone. Verify the largest I2S piece from the current caller before using the staging
  arithmetic above.
- The picture: `python tools/av_push.py --audio clip.pcm --frames clip.1bpp --dry-run` prints
  how many records and bytes a clip becomes; a real run answers with `frames` equal to the
  strip's count and `ms` equal to the audio's length. If the OLED snows, its `height` is
  wrong for the panel — `GET /api/devices` reports what the firmware thinks it is.
- The panel itself, with no media at all: `python tools/av_push.py --test-pattern --seconds 4`
  draws a one-pixel border, a filled block at (8,8) and a diagonal from (40,4) to (80,28)
  over four seconds of silence. A complete border, the block top-left and a diagonal that
  runs down-right means the driver and the panel agree; a pattern repeated or torn in
  stripes means addressing or row count, and half of it means `oled.height`. This needs no
  file on the bench, so it still works after a reflash or a wipe.

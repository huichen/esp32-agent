# Audio on this board: a WAV, and why not Opus

The owner asked whether a very small Opus encoder can be written for this board. This
document answers with arithmetic rather than enthusiasm. The short answer: **no**, and the
board records WAV while the host encodes. The arithmetic is below, and it says which budget
is the one that fails, so a future board can be checked against it in a minute.

Nothing here was measured on a fresh build for this document. Every figure is labelled
`[measured]` (recorded on this bench earlier, with where it came from) or `[estimate]`
(derived, not built here). The distinction is the point: an `[estimate]` is cheap to
disagree with, a `[measured]` number is not.

## 1. What the board has

| Resource | Value | Source |
|---|---|---|
| App partition | 3 MiB = 3,145,728 B (`factory`, and the same per `ota_0`/`ota_1`) | `firmware/partitions.csv`, `docs/flashing.md` §3 |
| Firmware image | 86% of it = ~2,705,000 B, so **~440,000 B free (~430 KiB, 13.6%)** | `[measured]` on this board; `espflash` prints `App size … (86%)` |
| Internal SRAM free at runtime | **5–6 kB**, and it does not move with the transcript | `[measured]`, `docs/troubleshooting.md`, `README.md` bench numbers |
| PSRAM free at runtime | ~6.5 MB (6,593 kB at 507 KB of history) | `[measured]`, `docs/troubleshooting.md` |
| Thread stacks | come from **internal SRAM**, not PSRAM | `docs/troubleshooting.md` ("could not spawn connection thread") |
| Microphone | 16 kHz, 32-bit slots, left channel, 24 bits used | `[measured]`, `crates/agent-core/src/drivers/audio.rs` |
| One tool result | 2048 B, truncated with a marker past that | `agent::MAX_TOOL_RESULT_BYTES` |

The 5–6 kB is not a soft number. TLS already lives close to the edge of it: with ~15 kB free
the hardware-AES path fails to get a DMA-capable buffer and every record dies with
`esp-tls-mbedtls: write error :-0x0001` (`docs/troubleshooting.md`, "The portal opens over
http but not https"). An audio encoder would be competing for the same few kilobytes as the
thing that talks to the model.

## 2. What an Opus encoder would cost

Opus (RFC 6716) is two codecs in one session: SILK for speech and CELT for everything else,
plus a hybrid mode. The encoder always runs an internal 48 kHz pipeline; a 16 kHz microphone
input is resampled up, and a 48 kHz-capable path is part of what "Opus" means. So the cheapest
honest subset is:

- **SILK only**, fixed point, mono, and either 8 kHz (narrowband) or 16 kHz (wideband);
- no CELT, no hybrid, no 48 kHz, no FEC/DTX/VBR knobs.

That subset still produces packets a normal `opus_decode` can read — which is exactly why it
is tempting. The costs:

| Item | Cost | Basis |
|---|---|---|
| Encoder code (SILK encoder, range coder, resampler) | ~30–40 KiB of text | `[estimate]`; libopus is ~70–80 kloc C, `silk/` is ~22 kloc of it |
| SILK codebooks and tables (NLSF/LSF, pitch lag, LTP, gains, pulse counts) | ~20–30 KiB of rodata | `[estimate]`; these are the tables nobody escapes by writing "a small encoder" |
| **Flash total** | **~50–70 KiB** | `[estimate]` |
| Encoder state (`OpusEncoder` + SILK control + resampler) | ~15–30 KiB of heap | `[estimate]`; libopus 1.3 fixed point, not built here |
| Task stack | ≥ 4 KiB, **internal** | a FreeRTOS task cannot run from PSRAM |

Two further facts about libopus that the table does not show:

1. There is **no supported "SILK-only" build**. `FIXED_POINT` exists, and float can be
   compiled out, but CELT's state is part of `OpusEncoder` and its tables are linked into the
   API. A subset means a fork you maintain, not a `-D` flag.
2. A fork is the maintenance cost this repository's rules are written to avoid: the driver is
   supposed to stay small enough to read in one sitting (`AGENTS.md` §1, §8).

## 3. Does the subset fit? Flash yes, RAM no

**Flash: it fits, and it is not the blocker.** ~50–70 KiB against ~440,000 B free takes the
image from 86% to ~88–89% of the partition. Uncomfortable, but inside the partition, and the
OTA slots are the same 3 MiB so there is no second opinion to consult.

**Internal SRAM: it does not fit, and this is the blocker.**

```
free internal SRAM           5,000 – 6,000 B
task stack (internal only)  ≥ 4,096 B          -> at most ~1,900 B left, before any state
encoder state             15,000 – 30,000 B    -> 3× the total free internal SRAM
```

The state could live in PSRAM — the transcript already does — but the stack cannot, and the
5–6 kB that is left is smaller than the stack plus the I2S staging buffers the DMA already
owns. That is a hard "no", not a "tight": there is no arrangement of the existing allocations
that finds 15–30 kB of internal RAM on a board that runs the model, TLS, HTTP and the portal
from the same pool.

CPU is *not* the problem: fixed-point SILK at 16 kHz mono is tens of MHz on a
Cortex-M4-class core `[estimate, order of magnitude]`, and the ESP32-S3 runs at 240 MHz. The
problem is that the encode has to be fed by I2S DMA at 32,000 B/s and serviced by a task, and
there is no task stack to give it.

So: **no encoder was written.** A subset that fits flash cannot be *run* here, and shipping
half of Opus would be a promise this board cannot keep.

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

This replaced the old `pcm_b64` field. One blob, not two: 512 samples is 1024 B of PCM, and
base64 makes that 1424 B; a second copy as raw PCM would be 2792 B and would be cut by the
2048-byte tool-result cap mid-base64, which is the one failure mode that hands a host
unreadable bytes. `the_largest_recording_still_fits_one_tool_result` pins that.

Why WAV and not "PCM, figure it out": a host that receives base64 has to guess what it is, and
the guess that matters is container vs. raw samples. A WAV is unambiguous, needs no codec, and
every tool that plays audio reads it.

### What that costs on the wire

| Format | Bytes per second | What one 2 KB tool result carries |
|---|---|---|
| WAV, 16 kHz, 16-bit mono | 32,000 | 32 ms (512 samples; the schema's maximum) |
| Opus, 16 kbps | 2,000 | 1 s |
| IMA ADPCM, 4:1 | 8,000 | 250 ms |

Raw PCM is 16× bigger than Opus at 16 kbps and 4× bigger than ADPCM — but the binding limit
here is not bandwidth, it is the 2048-byte tool result and the 5–6 kB of internal RAM. A
compressed stream inside the same cap would carry more *time*, not more audio quality, and it
would still have to be re-chunked by whoever holds the connection. So the codec is the wrong
place to spend the budget on this board.

If the pipe ever does bind before the RAM does, the cheap step is **IMA ADPCM** (~40 lines,
16 bytes of per-channel state, 4:1): it fits in what is left, needs no tables, and is in every
host. It is **not implemented** — this document proposes it, it does not claim it.

## 5. Recommendation

1. Record on the board as WAV, 16 kHz mono, at most 512 samples per call; encode on the host.
   That is what `mic_record` does today.
2. Do not attempt an Opus encoder for this board. Flash would fit; internal SRAM would not,
   by a factor of three, and a forked libopus is a maintenance burden for a chip that cannot
   run it.
3. Play a song by streaming PCM into `POST /audio/pcm` (§6), which reads the body as it
   arrives. Do not grow `speaker_play`: its 2 KB is the tool-result cap, not an oversight.
4. Revisit if a board arrives with ~40–50 kB of free internal SRAM *and* a PSRAM-capable task
   stack (ESP32-P4 class, or DMA that reaches PSRAM): the flash arithmetic above already says
   the code would fit there.

## 6. Streaming playback: `POST /audio/pcm`

The board could make a tone but not play a song: `speaker_play` carries 2048 bytes, which is
43 ms at 24 kHz, and every fix along that road runs into the same wall as the encoder — a
tool result is 2 KB and a request body is 128 KB (`agent::MAX_TOOL_RESULT_BYTES`,
`http::MAX_BODY_BYTES`), i.e. 1.3 s. Neither is the format's fault; both are the cost of
holding the bytes somewhere first.

So there is one route whose body is not held at all. `POST /audio/pcm` takes raw PCM16LE,
mono, 24 kHz, framed with `Content-Length` or `Transfer-Encoding: chunked`, and hands each
piece straight to `i2s_out_write` as it arrives:

| | Value | Where |
|---|---|---|
| Sample format | PCM16LE, mono, 24000 Hz | `drivers::audio::AMP_RATE_HZ` |
| One upload | at most **30 s** = 1 440 000 bytes | `AUDIO_STREAM_MAX_BYTES` |
| Buffer used | two reused `Vec`s, ~3 KB of capacity, no growth per piece | `AudioSink` |
| New task thread | **none** — the request runs on the accept thread it already owns | `http.rs` |
| Lock held | one piece at a time (about 1 KB, 21 ms), never for the song | `AudioSink::push` |

The three numbers that matter, in the order they bite:

- **No new stack.** A dedicated audio thread would want the same 6 kB `CONN_STACK` the event
  streams use, and the boot log already says internal free heap is around 7 kB. The upload
  therefore runs inline: while one plays, the accept thread is busy and other requests wait
  in the listen backlog. That is the same trade the sync-melody call makes on the bench
  firmware, and it is bounded here by the 30 s cap instead of by a task that might not fit.
- **No body buffer.** 48 KB/s into a `Vec` is not a stream, it is a memory leak with a
  deadline; the body never becomes one. The socket is throttled by the amplifier, because
  `i2s_channel_write` blocks until the DMA has taken the piece.
- **The lock is per piece.** `App` is one mutex, and a handler that held it for a song would
  stop the portal, `/api/status` and the board's own agent thread for its duration.

Two failures this path has to answer honestly, because both are easy to get wrong:

1. **An odd byte.** A socket read can end mid-sample. The byte is carried into the next piece,
   and if the body ends with one it is reported as `400` and dropped — never played as half a
   sample (which is a click).
2. **A body that ends early.** The client hung up or the amplifier refused. The answer is
   `ok: false` with the samples that *were* played, because half a song is not a song.

The amplifier itself needs no new handling: `i2s_out_write` already duplicates mono into both
slots and clears the TX descriptor afterwards, which is what keeps a finished stream from
replaying its last DMA buffer forever (the bench bug in `firmware/src/esp_hw.rs`).

What this does **not** do: change the sample rate (the I2S TX clock is the constant
`AMP_RATE_HZ`, so `?rate=` accepts 24000 only), and hold two uploads at once. Both need a
`i2s_open_tx(rate)` and a second thread, i.e. RAM this board does not have; they are
`ROADMAP.md` items, not promises.

## 7. How to check these numbers again

- Flash: `espflash flash --partition-table firmware/partitions.csv --target-app-partition
  factory <elf>` prints `App size … (N%)`; compare N with 86%.
- Internal SRAM at runtime: the `mem:` lines the firmware logs at boot, and `/api/status`
  (`docs/troubleshooting.md`).
- The WAV itself: call `mic_record` over `/mcp`, write `wav_b64` to a file after
  base64-decoding it, then `soxi`/`ffprobe` should say `16000 Hz, 1 channels, 16-bit` with the
  sample count the tool reported.
- The stream route: `sox` a known 10 s clip to raw PCM16 mono at 24 kHz, `POST` it to
  `/audio/pcm` with `Transfer-Encoding: chunked`, and compare the answer's `ms` with 10000 —
  then watch `/api/status`'s `free_heap_kb` before and after, which must not have moved. A
  `samples` count short of `bytes / 2` means a piece was cut; `ok: false` names why.

//! I2S audio: an INMP441-class microphone and a MAX98357A-class amplifier.
//!
//! Neither can be *detected*. I2S is a point-to-point bus with no addressing and no id
//! register, so there is nothing to interrogate: the board profile (or `config.pins`)
//! says which pins are wired, `Hardware::i2s_setup` brings the channels up, and only then
//! do the tools exist. That is also why the caps come from the devices rather than from
//! `board_caps` — a capability without a working channel would be a promise.
//!
//! The formats are the ones measured on this board (`D:\esp32s3-probe\README.md`):
//!
//! * microphone: 16 kHz, 32-bit slots, **left channel only** — the INMP441 sends 24 bits
//!   left-justified and the right slot is always zero;
//! * amplifier: 24 kHz, 16-bit, Philips standard, mono; `SD` tied high so it is always
//!   enabled, `GAIN` floating = 9 dB.
//!
//! The tools are deliberately small: a level meter, a bounded recording, a generated
//! tone, and a bounded playback. `mic_record` keeps its own result under 2 KB — well
//! inside `agent::MAX_TOOL_RESULT_BYTES` — because 512 samples is already 32 ms of audio
//! and a longer answer would spend a model's context on samples it cannot hear.
//!
//! Playback has the opposite budget and it is not a generous one. A request body is
//! 128 KB, so a `pcm_b64` argument could in principle carry 1.9 s of audio — but five
//! seconds is 240,000 bytes of PCM, which no single request holds, and splitting it over
//! three requests would put an HTTP round trip between the notes and be heard as a gap.
//! So the melody form does not send samples at all: it sends a *score* (`notes`, a few
//! hundred bytes) and the board synthesizes one contiguous buffer, which reaches I2S in
//! a single write. `docs/audio.md` §6 has the arithmetic.

use crate::device::DeviceInfo;
use crate::error::McpError;
use crate::hardware::Hardware;
use crate::json as j;
use crate::tool::{integer, object_schema, string, ToolDef};
use crate::util;
use serde_json::{json, Value};

/// Microphone rate: 16 kHz, 32-bit slots, left channel only.
pub const MIC_RATE_HZ: u32 = 16_000;

/// Amplifier rate: 24 kHz, 16-bit, Philips, mono.
pub const AMP_RATE_HZ: u32 = 24_000;

/// Largest recording a tool result can carry: 512 samples = 1024 bytes = ~1.4 KB of
/// base64, which leaves room for the envelope inside the 2 KB tool result.
pub const MAX_RECORD_SAMPLES: usize = 512;

/// Bytes in the RIFF/WAVE header this module writes: one `fmt ` chunk and the `data`
/// header, with no metadata chunks. 44 is what every decoder expects to skip.
pub const WAV_HEADER_BYTES: usize = 44;

/// Longest melody the board will synthesize, in milliseconds of audio: 6 s is 144,000
/// samples, 288,000 bytes of mono PCM (and twice that once the driver duplicates each
/// sample into a stereo frame). The cap is a memory budget, not a musical one: 8 s also
/// fits in PSRAM, but the peak is then over 1 MB for a buffer that exists only to be
/// handed to the DMA, and this board has 8 MB shared with everything else.
pub const MAX_MELODY_MS: u32 = 6_000;

/// Most notes in one melody. A 64-note score is still well under a kilobyte of JSON.
pub const MAX_MELODY_NOTES: usize = 64;

/// Shortest and longest single note. The lower bound keeps the articulation ramp (below)
/// from swallowing the note; the upper is the same 2 s `speaker_tone` allows.
pub const MIN_NOTE_MS: u32 = 20;
pub const MAX_NOTE_MS: u32 = 2_000;

/// Articulation ramp at a note boundary: 2 ms each side. Two notes at different
/// frequencies have to meet at zero amplitude or the step is a click, so this is the
/// shortest ramp a 44.1 kHz-able ear reads as smooth. It is a dip in the envelope, not a
/// gap in the stream: no samples are skipped, and the deepest attenuation lasts 4 ms.
const NOTE_FADE_MS: u32 = 2;

/// The same ramp at the melody's own start and end, where nothing follows and a longer
/// fade costs nothing.
const EDGE_FADE_MS: u32 = 5;

/// A level measurement reads at most this many samples, so a long window cannot park the
/// agent thread for seconds.
const MAX_LEVEL_SAMPLES: usize = 8_192;

pub struct Mic {
    pub id: String,
}

pub struct Speaker {
    pub id: String,
}

impl Mic {
    fn info_of(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("mic", "I2S microphone (INMP441)", "mic");
        i.id = self.id.clone();
        i.caps = vec!["mic".into()];
        i.detail = json!({ "rate_hz": MIC_RATE_HZ, "bits": 32, "channel": "left" });
        i.tools = vec!["mic_level".into(), "mic_record".into()];
        i
    }
}

impl Speaker {
    fn info_of(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new("speaker", "I2S amplifier (MAX98357A)", "speaker");
        i.id = self.id.clone();
        i.caps = vec!["speaker".into()];
        i.detail = json!({ "rate_hz": AMP_RATE_HZ, "bits": 16, "max_melody_ms": MAX_MELODY_MS });
        i.tools = vec!["speaker_tone".into(), "speaker_play".into()];
        i
    }
}

impl crate::device::Device for Mic {
    fn info(&self) -> DeviceInfo {
        self.info_of()
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let id2 = self.id.clone();
        vec![
            ToolDef::new(
                "mic_level",
                "Measure the microphone: RMS and peak level in dBFS over `ms` milliseconds. Use this to check that the mic hears anything at all before recording",
            )
            .title("Microphone level")
            .category("audio")
            .requires(&["mic"])
            .schema(object_schema(
                &[("ms", integer(10, 2000, "window in milliseconds, default 200"))],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "mic_level", args, ctx.hw)
            }),
            ToolDef::new(
                "mic_record",
                "Record from the microphone and return a complete WAV file as base64: 16-bit mono PCM at 16 kHz, taken from the top 16 bits of the 24-bit samples. At most 512 samples (32 ms) — a tool result is capped at 2 KB, so longer audio belongs on a host, which is also where it is encoded (the board records WAV, see docs/audio.md)",
            )
            .title("Record")
            .category("audio")
            .requires(&["mic"])
            .mutating()
            .schema(object_schema(
                &[("samples", integer(1, MAX_RECORD_SAMPLES as i64, "how many samples, default 256"))],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id2.clone();
                ctx.devices.call(&id, "mic_record", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> crate::error::Result<Value> {
        match tool {
            "mic_level" => {
                let ms = j::opt_i64(args, "ms").unwrap_or(200).clamp(10, 2000) as usize;
                let want = (MIC_RATE_HZ as usize / 1000 * ms).clamp(64, MAX_LEVEL_SAMPLES);
                let mut buf = vec![0i32; want];
                let got = hw.i2s_mic_read(&mut buf, ms as u32 + 200)?;
                if got == 0 {
                    return Ok(json!({
                        "samples": 0,
                        "dbfs": Value::Null,
                        "note": "the microphone returned no samples in the window",
                    }));
                }
                // Full scale is 2^31: the INMP441 sends 24 bits left-justified in a
                // 32-bit slot, and the bench firmware measures dBFS the same way. Shifting
                // down first would only move the rounding.
                let (rms, peak) = levels32(&buf[..got]);
                Ok(json!({
                    "samples": got,
                    "ms": ms,
                    "rate_hz": MIC_RATE_HZ,
                    "rms": rms as i64,
                    "peak": peak as i64,
                    "dbfs": if rms == 0.0 { Value::Null } else { json!(round1(20.0 * (rms / 2_147_483_648.0).log10())) },
                    "peak_dbfs": if peak == 0 { Value::Null } else { json!(round1(20.0 * (peak as f64 / 2_147_483_648.0).log10())) },
                }))
            }
            "mic_record" => {
                let n =
                    j::opt_i64(args, "samples").unwrap_or(256).clamp(1, MAX_RECORD_SAMPLES as i64) as usize;
                let mut buf = vec![0i32; n];
                let got = hw.i2s_mic_read(&mut buf, (n as u32 * 1000 / MIC_RATE_HZ) + 300)?;
                let bytes: Vec<u8> =
                    buf[..got].iter().flat_map(|s| ((*s >> 16) as i16).to_le_bytes()).collect();
                let file = wav(&bytes, MIC_RATE_HZ);
                Ok(json!({
                    // The format is stated rather than implied: a host that receives
                    // base64 without one has to guess, and the guess that matters here is
                    // whether the bytes are raw PCM or a container.
                    "format": "wav",
                    "samples": got,
                    "rate_hz": MIC_RATE_HZ,
                    "bits": 16,
                    "channels": 1,
                    "ms": (got as u32 * 1000 / MIC_RATE_HZ),
                    "wav_bytes": file.len(),
                    "note": "a RIFF/WAVE file of 16-bit mono PCM taken from the top of the 24-bit samples; the board records WAV and a host encodes (docs/audio.md)",
                    "wav_b64": util::b64_encode(&file),
                }))
            }
            other => Err(McpError::not_found(format!("mic has no tool `{other}`"))),
        }
    }
}

impl crate::device::Device for Speaker {
    fn info(&self) -> DeviceInfo {
        self.info_of()
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let id2 = self.id.clone();
        vec![
            ToolDef::new(
                "speaker_tone",
                "Play a sine tone on the amplifier: `freq_hz` for `ms` milliseconds at `volume` percent",
            )
            .title("Play tone")
            .category("audio")
            .requires(&["speaker"])
            .mutating()
            .schema(object_schema(
                &[
                    ("freq_hz", integer(20, 8000, "tone frequency, e.g. 440")),
                    ("ms", integer(10, 2000, "duration in milliseconds, default 200")),
                    ("volume", integer(0, 100, "amplitude as a percentage, default 30")),
                ],
                &["freq_hz"],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "speaker_tone", args, ctx.hw)
            }),
            ToolDef::new(
                "speaker_play",
                "Play audio on the amplifier. `notes` is a melody the board synthesizes into one continuous buffer (up to 64 notes / 6 s), so a tune costs one call and has no gap between its notes; a note is `{\"hz\": 523, \"ms\": 400}` and `hz` 0 is a rest. `pcm_b64` is pre-rendered samples (16-bit mono, 24 kHz), at most 2048 bytes = 43 ms — for audio you already hold, and longer than that, stream it into `POST /audio/pcm` instead of growing this argument (docs/audio.md §6, §7)",
            )
            .title("Play")
            .category("audio")
            .requires(&["speaker"])
            .mutating()
            .schema(object_schema(
                &[
                    (
                        "notes",
                        json!({
                            "type": "array",
                            "description": "a melody, played back to back in one buffer: [{hz, ms}, ...]",
                            "minItems": 1,
                            "maxItems": MAX_MELODY_NOTES,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "hz": { "type": "integer", "minimum": 0, "maximum": 8000,
                                            "description": "frequency in Hz; 0 is a rest" },
                                    "ms": { "type": "integer", "minimum": MIN_NOTE_MS, "maximum": MAX_NOTE_MS,
                                            "description": "how long this note sounds, in milliseconds" },
                                },
                                "required": ["hz", "ms"],
                            },
                        }),
                    ),
                    (
                        "pcm_b64",
                        string("base64 of i16 little-endian samples, at most 2048 bytes (1024 samples, 43 ms)"),
                    ),
                    ("volume", integer(0, 100, "amplitude as a percentage, default 30")),
                ],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id2.clone();
                ctx.devices.call(&id, "speaker_play", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> crate::error::Result<Value> {
        match tool {
            "speaker_tone" => {
                let freq = j::ranged_i64(args, "freq_hz", 0, 20, 8000)? as f32;
                let ms = j::opt_i64(args, "ms").unwrap_or(200).clamp(10, 2000) as u32;
                let volume = j::opt_i64(args, "volume").unwrap_or(30).clamp(0, 100) as f32 / 100.0;
                let samples = tone(freq, ms, volume);
                // In pieces the size of a ring's worth plus change, not one block for the whole
                // tone: a 2 s tone is 192 KB, which one `i2s_channel_write` can only part with
                // inside its 1000 ms timeout, and the part it does not send is silence in the
                // middle of the tone. Piecewise is also how a stream writes, so the pacing a
                // tone shows is the pacing a song gets.
                for piece in samples.chunks(4_096) {
                    hw.i2s_out_write(piece)?;
                }
                Ok(json!({
                    "ok": true,
                    "freq_hz": freq as i64,
                    "ms": ms,
                    "rate_hz": AMP_RATE_HZ,
                    "volume_pct": (volume * 100.0) as i64,
                    "samples": samples.len(),
                }))
            }
            "speaker_play" => {
                let melody_arg = j::arr(args, "notes");
                let pcm_arg = j::opt_str(args, "pcm_b64");
                match (melody_arg, pcm_arg) {
                    (Some(_), Some(_)) => Err(McpError::invalid_params(
                        "give either `notes` (a melody the board synthesizes) or `pcm_b64` (samples to play), not both",
                    )),
                    (None, None) => Err(McpError::invalid_params(
                        "one of `notes` (a melody) or `pcm_b64` (samples) is required",
                    )),
                    (Some(notes), None) => {
                        if notes.is_empty() {
                            return Err(McpError::invalid_params(
                                "`notes` is empty; a melody needs at least one note",
                            ));
                        }
                        if notes.len() > MAX_MELODY_NOTES {
                            return Err(McpError::invalid_params(format!(
                                "`notes` has {} entries; at most {MAX_MELODY_NOTES} are accepted",
                                notes.len()
                            )));
                        }
                        let mut score = Vec::with_capacity(notes.len());
                        let mut total_ms: u32 = 0;
                        for (i, note) in notes.iter().enumerate() {
                            let (hz, ms) = note_of(i, note)?;
                            total_ms = total_ms.saturating_add(ms);
                            score.push((hz, ms));
                        }
                        if total_ms > MAX_MELODY_MS {
                            return Err(McpError::invalid_params(format!(
                                "the melody is {total_ms} ms; at most {MAX_MELODY_MS} are accepted \
                                 (one contiguous buffer of {} bytes of PCM)",
                                samples_for(MAX_MELODY_MS) * 2
                            )));
                        }
                        let volume = j::opt_i64(args, "volume").unwrap_or(30).clamp(0, 100) as f32 / 100.0;
                        // Where the time goes, reported rather than guessed: `synth_ms` is
                        // the float maths, `write_ms` is how long the board held the
                        // amplifier. `samples` against `write_ms` is the only measurement
                        // of the real sample rate this board offers — see docs/audio.md §6.
                        let started = std::time::Instant::now();
                        let samples = melody(&score, volume);
                        let synth_ms = started.elapsed().as_millis() as u64;
                        // One buffer, one write: this is what makes the notes gapless. A
                        // per-note `i2s_out_write` would be equally cheap here and sound
                        // like it — the DMA ring holds 400 frames (16.7 ms) and the
                        // synthesis of the next note would have to fit in that.
                        let writing = std::time::Instant::now();
                        hw.i2s_out_write(&samples)?;
                        let write_ms = writing.elapsed().as_millis() as u64;
                        Ok(json!({
                            "ok": true,
                            "notes": score.len(),
                            "samples": samples.len(),
                            "ms": total_ms,
                            "rate_hz": AMP_RATE_HZ,
                            "volume_pct": (volume * 100.0) as i64,
                            "pcm_bytes": samples.len() * 2,
                            "synth_ms": synth_ms,
                            "write_ms": write_ms,
                        }))
                    }
                    (None, Some(b64)) => {
                        let mut bytes = Vec::new();
                        util::b64_decode(&b64, &mut bytes)?;
                        if bytes.len() % 2 != 0 {
                            return Err(McpError::invalid_params("`pcm_b64` must decode to whole 16-bit samples"));
                        }
                        if bytes.len() > 2048 {
                            return Err(McpError::invalid_params(format!(
                                "`pcm_b64` decodes to {} bytes; at most 2048 are accepted (1024 samples, 43 ms) — \
                                 for anything longer, send `notes` and let the board synthesize it",
                                bytes.len()
                            )));
                        }
                        let samples: Vec<i16> =
                            bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
                        hw.i2s_out_write(&samples)?;
                        Ok(json!({
                            "ok": true,
                            "samples": samples.len(),
                            "rate_hz": AMP_RATE_HZ,
                            "ms": (samples.len() as u32 * 1000 / AMP_RATE_HZ),
                        }))
                    }
                }
            }
            other => Err(McpError::not_found(format!("speaker has no tool `{other}`"))),
        }
    }
}

/// How many 16-bit samples `ms` milliseconds of amplifier audio is.
fn samples_for(ms: u32) -> usize {
    (AMP_RATE_HZ as u64 * ms as u64 / 1000) as usize
}

/// Points in one sine cycle, and the table is `SINE_STEPS + 1` long so the interpolation
/// inside the last step needs no wrap-around.
const SINE_STEPS: usize = 1024;

/// `sin(2π i / SINE_STEPS)`, built once.
///
/// `f32::sin` costs ~16 µs per sample on this chip — measured on the bench: 1,915 ms of
/// synthesis for a 4,800 ms melody, so the board spent 40% of the tune answering nothing
/// (the numbers are in the tool result as `synth_ms` and `write_ms`). A table plus a
/// linear interpolation replaces that with a shift and a lerp, and the phase is an
/// integer, so it cannot drift over a long melody either.
static SINE: std::sync::OnceLock<[f32; SINE_STEPS + 1]> = std::sync::OnceLock::new();

fn sine_table() -> &'static [f32; SINE_STEPS + 1] {
    SINE.get_or_init(|| {
        let mut t = [0.0f32; SINE_STEPS + 1];
        for (i, v) in t.iter_mut().enumerate() {
            *v = (2.0 * std::f32::consts::PI * i as f32 / SINE_STEPS as f32).sin();
        }
        t
    })
}

/// The sine at `phase`, where a full cycle is `u32::MAX` — 10 bits of table index and 12
/// bits of interpolation between two entries.
fn sine_at(table: &[f32; SINE_STEPS + 1], phase: u32) -> f32 {
    let idx = (phase >> 22) as usize;
    let frac = ((phase >> 10) & 0xFFF) as f32 / 4096.0;
    table[idx] + (table[idx + 1] - table[idx]) * frac
}

/// A melody as one buffer of 24 kHz 16-bit mono PCM: every note starts where the previous
/// one ended, so there is nothing between them that could become a gap.
///
/// `freq` 0 is a rest — the same envelope over silence — so a melody can hold a beat
/// without the score needing a special case. Each note starts and ends at zero amplitude:
/// the last sample of one note and the first of the next are exactly 0, because a note at
/// a new frequency starting at a non-zero value is a step, and a step is a click. That
/// ramp is a dip in the envelope, not a gap in the stream — no samples are skipped.
pub fn melody(notes: &[(f32, u32)], volume: f32) -> Vec<i16> {
    let amp = volume.clamp(0.0, 1.0) * 32767.0;
    let table = sine_table();
    let mut out = Vec::with_capacity(notes.iter().map(|(_, ms)| samples_for(*ms)).sum());
    let last = notes.len().saturating_sub(1);
    for (idx, (freq, ms)) in notes.iter().enumerate() {
        let n = samples_for(*ms);
        if n == 0 {
            continue;
        }
        let rise = samples_for(if idx == 0 { EDGE_FADE_MS } else { NOTE_FADE_MS }).clamp(1, (n / 2).max(1));
        let fall =
            samples_for(if idx == last { EDGE_FADE_MS } else { NOTE_FADE_MS }).clamp(1, (n / 2).max(1));
        // One cycle is 2^32 of phase, so a note advances `freq / rate` of a cycle per
        // sample. The phase is an integer that wraps: no accumulated rounding, so the
        // first and the last sample of a 6 s note are the same pitch.
        let step = ((*freq as f64 / AMP_RATE_HZ as f64).rem_euclid(1.0) * 4_294_967_296.0) as u64 as u32;
        let mut phase: u32 = 0;
        for i in 0..n {
            // Both ramps are 0 at i == 0 and at i == n-1, so the note joins the next at
            // silence without a discontinuity the amplifier would render as a click.
            let env = (i as f32 / rise as f32).min((n - 1 - i) as f32 / fall as f32).min(1.0);
            out.push((sine_at(table, phase) * amp * env) as i16);
            phase = phase.wrapping_add(step);
        }
    }
    out
}

/// A sine at `freq` for `ms`: one note, with the melody's own ramps at both ends.
fn tone(freq: f32, ms: u32, volume: f32) -> Vec<i16> {
    melody(&[(freq, ms)], volume)
}

/// One note of a score: `{ "hz": 523, "ms": 400 }`. `hz` 0 is a rest.
///
/// The index is in the error message because a score arrives as a list: "a note is
/// malformed" would leave a caller guessing which of fourteen it was.
fn note_of(index: usize, note: &Value) -> crate::error::Result<(f32, u32)> {
    let bad = |why: String| McpError::invalid_params(format!("`notes[{index}]` {why}"));
    let hz = j::opt_i64(note, "hz").ok_or_else(|| bad("needs `hz` (integer Hz, 0 = rest)".into()))?;
    let ms = j::opt_i64(note, "ms").ok_or_else(|| bad("needs `ms` (integer milliseconds)".into()))?;
    if !(0..=8000).contains(&hz) {
        return Err(bad(format!("`hz` must be within 0..=8000, got {hz}")));
    }
    if !(MIN_NOTE_MS as i64..=MAX_NOTE_MS as i64).contains(&ms) {
        return Err(bad(format!("`ms` must be within {MIN_NOTE_MS}..={MAX_NOTE_MS}, got {ms}")));
    }
    Ok((hz as f32, ms as u32))
}

/// `pcm` (16-bit little-endian, mono) as a complete RIFF/WAVE file.
///
/// The board cannot encode Opus: the flash would take a subset but internal SRAM cannot
/// carry an encoder's state and task stack, which is the arithmetic in `docs/audio.md`.
/// A WAV is what this chip can produce honestly, and no host needs a codec to read it.
///
/// The header is the canonical 44 bytes: `RIFF <size> WAVE`, one 16-byte `fmt ` chunk
/// (PCM, mono, `rate_hz`, 16 bits) and `data <size>`. No metadata chunks, so every
/// decoder that accepts a WAV accepts this one.
pub fn wav(pcm: &[u8], rate_hz: u32) -> Vec<u8> {
    let data = pcm.len().min(u32::MAX as usize) as u32;
    let mut out = Vec::with_capacity(WAV_HEADER_BYTES + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk: 16 bytes, PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // format 1 = PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
    out.extend_from_slice(&rate_hz.to_le_bytes());
    out.extend_from_slice(&(rate_hz.saturating_mul(2)).to_le_bytes()); // byte rate: rate * 1 ch * 2 B
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// RMS and peak magnitude of a 32-bit window (24 bits left-justified).
fn levels32(samples: &[i32]) -> (f64, i32) {
    let mut sum = 0f64;
    let mut peak = 0i32;
    for s in samples {
        let v = *s;
        sum += (v as f64) * (v as f64);
        peak = peak.max(v.saturating_abs());
    }
    let rms = if samples.is_empty() { 0.0 } else { (sum / samples.len() as f64).sqrt() };
    (rms, peak)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Device;

    /// The whole tone reaches the hardware: it goes out in pieces now, and the sum must still
    /// be exactly 24 kHz x ms, or a chunk boundary would shorten the note.
    #[test]
    fn a_played_tone_writes_every_sample_in_pieces() {
        use crate::device::Device;
        let mut hw = crate::virtual_board::VirtualBoard::new();
        hw.i2s_setup(None, Some(crate::boards::I2sPins { bclk: 15, ws: 16, din: -1, dout: 7 }))
            .expect("amplifier");
        let mut spk = Speaker { id: "speaker".into() };
        let out = spk
            .call("speaker_tone", &json!({ "freq_hz": 440, "ms": 2000, "volume": 10 }), &mut hw)
            .expect("a tone");
        assert_eq!(out["samples"], json!(48_000));
        assert_eq!(hw.i2s_written, 48_000, "every sample of the note, and only those");
    }

    #[test]
    fn a_tone_is_bounded_and_shaped() {
        let t = tone(440.0, 100, 0.5);
        assert_eq!(t.len(), 2400, "24 kHz for 100 ms");
        assert!(t.iter().any(|s| s.abs() > 1000), "a half-volume tone must move");
        assert!(t.iter().all(|s| s.abs() <= 32767), "no wrap-around");
        assert!(t[0].abs() < 200 && t[t.len() - 1].abs() < 200, "the ends click");
    }

    #[test]
    fn a_recording_is_a_complete_wav_file() {
        let pcm: Vec<u8> = vec![0x01, 0x00, 0xff, 0x7f, 0x00, 0x80];
        let file = wav(&pcm, MIC_RATE_HZ);

        assert_eq!(file.len(), WAV_HEADER_BYTES + pcm.len());
        assert_eq!(&file[0..4], b"RIFF");
        let riff_size = u32::from_le_bytes([file[4], file[5], file[6], file[7]]) as usize;
        assert_eq!(riff_size, file.len() - 8, "the RIFF size counts everything after it");
        assert_eq!(&file[8..12], b"WAVE");
        assert_eq!(&file[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes([file[16], file[17], file[18], file[19]]), 16, "PCM fmt chunk");
        assert_eq!(u16::from_le_bytes([file[20], file[21]]), 1, "format 1 = PCM");
        assert_eq!(u16::from_le_bytes([file[22], file[23]]), 1, "mono");
        assert_eq!(u32::from_le_bytes([file[24], file[25], file[26], file[27]]), MIC_RATE_HZ);
        assert_eq!(
            u32::from_le_bytes([file[28], file[29], file[30], file[31]]),
            MIC_RATE_HZ * 2,
            "byte rate: 16-bit mono at 16 kHz"
        );
        assert_eq!(u16::from_le_bytes([file[32], file[33]]), 2, "block align");
        assert_eq!(u16::from_le_bytes([file[34], file[35]]), 16, "bits per sample");
        assert_eq!(&file[36..40], b"data");
        assert_eq!(u32::from_le_bytes([file[40], file[41], file[42], file[43]]) as usize, pcm.len());
        assert_eq!(&file[WAV_HEADER_BYTES..], &pcm[..], "the samples are carried unchanged");
    }

    /// The largest recording the schema allows still has to fit one tool result, because
    /// that is the only way the audio reaches a host: truncation would cut a base64 blob
    /// and hand it over as unreadable bytes.
    #[test]
    fn the_largest_recording_still_fits_one_tool_result() {
        let longest = wav(&vec![0u8; MAX_RECORD_SAMPLES * 2], MIC_RATE_HZ);
        assert_eq!(longest.len(), WAV_HEADER_BYTES + MAX_RECORD_SAMPLES * 2);
        let encoded = util::b64_encode(&longest);
        // The envelope around the payload (keys, note, numbers) is under 300 bytes of JSON.
        assert!(
            encoded.len() + 300 < crate::agent::MAX_TOOL_RESULT_BYTES,
            "base64 is {} bytes, which does not leave the envelope room in {}",
            encoded.len(),
            crate::agent::MAX_TOOL_RESULT_BYTES
        );
    }

    #[test]
    fn levels_report_rms_and_peak() {
        let (rms, peak) = levels32(&[0, 65_536, -65_536, 0]);
        assert!((rms - 46_341.0).abs() < 10.0, "rms {rms}");
        assert_eq!(peak, 65_536);
        assert_eq!(levels32(&[]), (0.0, 0));
    }

    /// Stands in for the amplifier and records what it was handed, so a test can say how
    /// many writes a melody took rather than trusting the handler's own return value.
    #[derive(Default)]
    struct Ear {
        writes: Vec<usize>,
        samples: Vec<i16>,
    }

    impl crate::hardware::Hardware for Ear {
        fn board_info(&mut self) -> crate::hardware::BoardInfo {
            crate::hardware::BoardInfo::default()
        }

        fn i2s_out_write(&mut self, samples: &[i16]) -> crate::error::Result<()> {
            self.writes.push(samples.len());
            self.samples.extend_from_slice(samples);
            Ok(())
        }
    }

    /// Twinkle Twinkle's first phrase: 14 notes, 5.6 s — the case this path exists for.
    fn twinkle() -> Vec<(f32, u32)> {
        let mut n = Vec::new();
        for hz in [523.0, 523.0, 784.0, 784.0, 880.0, 880.0, 784.0] {
            n.push((hz, 400));
        }
        for hz in [698.0, 698.0, 659.0, 659.0, 587.0, 587.0, 523.0] {
            n.push((hz, 400));
        }
        n
    }

    #[test]
    fn a_melody_is_one_contiguous_buffer_with_no_silence_in_it() {
        let notes = twinkle();
        let pcm = melody(&notes, 0.3);
        let expected: usize = notes.iter().map(|(_, ms)| samples_for(*ms)).sum();
        assert_eq!(expected, 134_400, "5.6 s at 24 kHz");
        assert_eq!(pcm.len(), expected, "the buffer is the notes and nothing else: no padding");

        // A gap would be a stretch with no signal at all. The articulation ramp between
        // two notes bottoms out over 4 ms, so a 1 ms window is the finest gap this can
        // hide — and none of them is silent.
        let window = samples_for(1).max(1);
        let silent = pcm.windows(window).filter(|w| w.iter().all(|s| *s == 0)).count();
        assert_eq!(silent, 0, "{window}-sample windows with no signal are audible gaps");
    }

    #[test]
    fn a_note_joins_the_next_at_silence_so_there_is_no_click() {
        let pcm = melody(&[(440.0, 100), (523.0, 100)], 0.8);
        let n = samples_for(100);
        assert_eq!(pcm.len(), n * 2);
        // Different frequencies: a step at the join would be a click, a zero is not.
        assert_eq!(pcm[n - 1], 0, "the last sample of the first note");
        assert_eq!(pcm[n], 0, "the first sample of the second");
        assert_eq!(pcm[0], 0);
        assert_eq!(pcm[pcm.len() - 1], 0);
        // …and the ramp is short: the middle of a note is not mostly envelope. (The exact
        // centre sample is not usable — at 440 Hz, 50 ms in is a whole number of periods.)
        let peak = pcm[n / 4..n * 3 / 4].iter().map(|s| s.abs()).max().unwrap_or(0);
        assert!(peak > 10_000, "the body of a note is not attenuated: peak {peak}");
    }

    /// The oscillator is a table and an integer phase, which is fast but only as good as
    /// its arithmetic: this compares it with `sin` at the same rate. A wrong phase step
    /// (the pitch) or a wrong table index shows up immediately; the tolerance covers the
    /// f32 reference's own rounding, not a real error.
    #[test]
    fn the_table_oscillator_matches_the_analytic_sine() {
        let hz = 440.0f32;
        let n = samples_for(200);
        let pcm = melody(&[(hz, 200)], 1.0);
        // Past the 5 ms ramp at each end, the envelope is exactly 1.
        let ramp = samples_for(EDGE_FADE_MS);
        let mut worst = 0.0f32;
        for i in ramp..n - ramp {
            let want = (2.0 * std::f32::consts::PI * hz / AMP_RATE_HZ as f32 * i as f32).sin();
            let got = pcm[i] as f32 / 32767.0;
            worst = worst.max((got - want).abs());
        }
        assert!(worst < 0.005, "worst deviation {worst} (about {} LSB)", (worst * 32767.0) as i64);
    }

    #[test]
    fn a_rest_is_silence_of_the_score_length() {
        let pcm = melody(&[(0.0, 100), (880.0, 100)], 0.9);
        let n = samples_for(100);
        assert_eq!(pcm.len(), n * 2);
        assert!(pcm[..n - 1].iter().all(|s| *s == 0), "hz 0 is a rest");
        assert!(pcm[n..].iter().any(|s| s.abs() > 1000), "and the note after it still sounds");
    }

    /// The whole point of the score form: 5 s of music reaches the amplifier in **one**
    /// write, so no request boundary, no scheduling delay and no DMA underrun can fall
    /// between two notes. Reverting `speaker_play` to samples-only makes this test fail at
    /// the call, not at an assertion.
    #[test]
    fn a_melody_reaches_the_amplifier_in_one_write() {
        let mut speaker = Speaker { id: "speaker".into() };
        let mut ear = Ear::default();
        let score: Vec<Value> =
            twinkle().into_iter().map(|(hz, ms)| json!({ "hz": hz as i64, "ms": ms })).collect();

        let out = speaker
            .call("speaker_play", &json!({ "notes": score, "volume": 30 }), &mut ear)
            .expect("a melody is playable");

        assert_eq!(out["notes"], json!(14), "{out}");
        assert_eq!(out["ms"], json!(5600), "the notes are 5.6 s in total: {out}");
        assert_eq!(out["samples"], json!(134_400), "{out}");
        assert_eq!(ear.writes, vec![134_400], "one write of the whole melody, not fourteen");
        assert_eq!(ear.samples.len(), 134_400, "and every sample of it arrived");
    }

    #[test]
    fn a_melody_over_the_limits_is_refused_with_the_number() {
        let mut speaker = Speaker { id: "speaker".into() };
        let mut ear = Ear::default();

        let too_many: Vec<Value> = (0..MAX_MELODY_NOTES + 1).map(|_| json!({"hz": 440, "ms": 100})).collect();
        let e = speaker.call("speaker_play", &json!({ "notes": too_many }), &mut ear).unwrap_err();
        assert!(e.message.contains(&MAX_MELODY_NOTES.to_string()), "{}", e.message);

        // 7 notes of 1 s is past the 6 s cap even though each note is legal.
        let too_long: Vec<Value> = (0..7).map(|_| json!({"hz": 440, "ms": 1000})).collect();
        let e = speaker.call("speaker_play", &json!({ "notes": too_long }), &mut ear).unwrap_err();
        assert!(e.message.contains("7000 ms") && e.message.contains("6000"), "{}", e.message);

        // The score and the samples are alternatives; both at once is a caller mistake
        // worth naming rather than silently preferring one.
        let e = speaker
            .call("speaker_play", &json!({ "notes": [{"hz": 440, "ms": 100}], "pcm_b64": "AAA=" }), &mut ear)
            .unwrap_err();
        assert!(e.message.contains("not both"), "{}", e.message);
        assert!(ear.writes.is_empty(), "nothing was played");
    }

    #[test]
    fn a_malformed_note_names_its_index() {
        let mut speaker = Speaker { id: "speaker".into() };
        let mut ear = Ear::default();
        let score = json!([{ "hz": 440, "ms": 100 }, { "hz": 9000, "ms": 100 }]);
        let e = speaker.call("speaker_play", &json!({ "notes": score }), &mut ear).unwrap_err();
        assert!(e.message.contains("notes[1]") && e.message.contains("9000"), "{}", e.message);

        let score = json!([{ "hz": 440, "ms": 5 }]);
        let e = speaker.call("speaker_play", &json!({ "notes": score }), &mut ear).unwrap_err();
        assert!(e.message.contains("notes[0]") && e.message.contains("ms"), "{}", e.message);
    }

    /// The largest melody the schema allows still fits this board's memory: the mono
    /// buffer plus the stereo copy the I2S driver makes of it.
    #[test]
    fn the_largest_melody_fits_the_psram_budget() {
        let pcm = melody(&[(440.0, MAX_MELODY_MS)], 0.5);
        assert_eq!(pcm.len(), samples_for(MAX_MELODY_MS));
        let bytes = pcm.len() * 2;
        assert_eq!(bytes, 288_000, "6 s of 16-bit mono at 24 kHz");
        // 8 MB of PSRAM, ~7.8 MB free at runtime: the mono buffer plus the driver's stereo
        // duplicate is ~864 kB, which is 11% of what is free.
        assert!((bytes * 3) < 1_024 * 1_024, "mono + stereo copy = {} bytes", bytes * 3);
    }
}

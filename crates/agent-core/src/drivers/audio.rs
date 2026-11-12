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
//! tone, and a bounded playback. A tool result is capped at 2 KB, so anything longer
//! would be truncated rather than heard — 512 samples is the honest maximum for a tool
//! that returns samples.

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
        i.detail = json!({ "rate_hz": AMP_RATE_HZ, "bits": 16 });
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
                "Play base64 PCM on the amplifier (16-bit mono, 24 kHz), at most 2048 bytes of audio — 43 ms. A tool result cannot carry a song: stream a longer one into `POST /audio/pcm` instead (docs/audio.md §6)",
            )
            .title("Play samples")
            .category("audio")
            .requires(&["speaker"])
            .mutating()
            .schema(object_schema(
                &[("pcm_b64", string("base64 of i16 little-endian samples"))],
                &["pcm_b64"],
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
                hw.i2s_out_write(&samples)?;
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
                let b64 = j::str_arg(args, "pcm_b64")?;
                let mut bytes = Vec::new();
                util::b64_decode(&b64, &mut bytes)?;
                if bytes.len() % 2 != 0 {
                    return Err(McpError::invalid_params("`pcm_b64` must decode to whole 16-bit samples"));
                }
                if bytes.len() > 2048 {
                    return Err(McpError::invalid_params(format!(
                        "`pcm_b64` decodes to {} bytes; at most 2048 are accepted (1024 samples)",
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
            other => Err(McpError::not_found(format!("speaker has no tool `{other}`"))),
        }
    }
}

/// A sine at `freq` for `ms`, shaped with a short fade so the start and end do not click.
fn tone(freq: f32, ms: u32, volume: f32) -> Vec<i16> {
    let n = (AMP_RATE_HZ as u64 * ms as u64 / 1000) as usize;
    let amp = volume.clamp(0.0, 1.0) * 32767.0;
    let fade = (AMP_RATE_HZ as usize / 200).max(8); // 5 ms
    (0..n)
        .map(|i| {
            let phase = 2.0 * std::f32::consts::PI * freq * i as f32 / AMP_RATE_HZ as f32;
            let env = {
                let head = (i as f32 / fade as f32).min(1.0);
                let tail = ((n.saturating_sub(i)) as f32 / fade as f32).min(1.0);
                head.min(tail)
            };
            (phase.sin() * amp * env) as i16
        })
        .collect()
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
}

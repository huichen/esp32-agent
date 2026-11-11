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
                "Record from the microphone and return base64 PCM (16-bit mono, 16 kHz, taken from the top 16 bits of the 24-bit samples). At most 512 samples — a tool result is capped at 2 KB, so longer audio belongs on a host",
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
                // 24 bits left-justified in 32: shift down to 16 for dBFS arithmetic.
                let samples: Vec<i16> = buf[..got].iter().map(|s| (*s >> 16) as i16).collect();
                let (rms, peak) = levels(&samples);
                Ok(json!({
                    "samples": got,
                    "ms": ms,
                    "rate_hz": MIC_RATE_HZ,
                    "rms": rms as i64,
                    "peak": peak as i64,
                    "dbfs": if rms == 0.0 { Value::Null } else { json!(round1(20.0 * (rms / 32768.0).log10())) },
                    "peak_dbfs": if peak == 0 { Value::Null } else { json!(round1(20.0 * (peak as f32 / 32768.0).log10())) },
                }))
            }
            "mic_record" => {
                let n =
                    j::opt_i64(args, "samples").unwrap_or(256).clamp(1, MAX_RECORD_SAMPLES as i64) as usize;
                let mut buf = vec![0i32; n];
                let got = hw.i2s_mic_read(&mut buf, (n as u32 * 1000 / MIC_RATE_HZ) + 300)?;
                let bytes: Vec<u8> =
                    buf[..got].iter().flat_map(|s| ((*s >> 16) as i16).to_le_bytes()).collect();
                Ok(json!({
                    "samples": got,
                    "rate_hz": MIC_RATE_HZ,
                    "bits": 16,
                    "channels": 1,
                    "note": "16-bit values taken from the top of the 24-bit microphone samples",
                    "pcm_b64": util::b64_encode(&bytes),
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
                "Play base64 PCM on the amplifier (16-bit mono, 24 kHz), at most 2048 bytes of audio",
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

/// RMS and peak magnitude of a window.
fn levels(samples: &[i16]) -> (f32, i16) {
    let mut sum = 0f64;
    let mut peak = 0i16;
    for s in samples {
        let v = *s;
        sum += (v as f64) * (v as f64);
        peak = peak.max(v.saturating_abs());
    }
    let rms = if samples.is_empty() { 0.0 } else { (sum / samples.len() as f64).sqrt() as f32 };
    (rms, peak)
}

fn round1(v: f32) -> f64 {
    ((v * 10.0).round() / 10.0) as f64
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
    fn levels_report_rms_and_peak() {
        let (rms, peak) = levels(&[0, 1000, -1000, 0]);
        assert!((rms - 707.1).abs() < 1.0, "rms {rms}");
        assert_eq!(peak, 1000);
        assert_eq!(levels(&[]), (0.0, 0));
    }
}

//! Clips: the tools that put media in PSRAM and play it back later.
//!
//! The shape is forced by two ceilings that pull in opposite directions. A tool result is
//! capped at 64 KiB, so a recording or an animation cannot travel through the conversation;
//! a request body is far smaller still, so it cannot travel in one argument either. What is
//! left is a *place*: append pieces into a named buffer, then say "play it". The bytes never
//! enter the transcript, which is also why replaying an animation costs one line instead of
//! a thousand tool calls.
//!
//! Everything here is deliberately synchronous. A clip does not start a worker: the tool
//! call that says "play" blocks until the sound is out or the frames are drawn, and the
//! deadline it honours is the caller's (`agent.tool_timeout_s`). Background playback is not
//! implemented, and `docs/audio.md` says so rather than implying otherwise.

use crate::app::App;
use crate::clips;
use crate::clips::{ClipError, ClipKind, CLIP_APPEND_MAX, CLIP_DEFAULT_PCM_BYTES, CLIP_MAX_BYTES};
use crate::drivers::audio::{MAX_RECORD_SAMPLES, MIC_RATE_HZ};
use crate::error::{McpError, Result};
use crate::json as j;
use crate::tool::with_argument_descriptions;
use crate::util;
use serde_json::{json, Value};

/// Samples per I²S write, the same chunk `speaker_play` and `media_play` use: one DMA
/// buffer's worth, so a long clip feeds the ring in pieces instead of one huge call.
const WRITE_SAMPLES: usize = 4_096;
/// Default frames per second for a bitmap clip when the caller does not say.
const DEFAULT_FPS: u32 = 8;

/// The schemas, in the same OpenAI shape as the rest of `agent_extra_specs`.
pub fn specs() -> Value {
    with_argument_descriptions(json!([
        {
            "type": "function",
            "function": {
                "name": "clip_new",
                "description": format!(
                    "Open or reset a named buffer in PSRAM for media too large for a tool result. `kind` is `pcm` (16-bit mono samples: use `{PCM_RATE_HZ}` for playback, `{MIC_RATE_HZ}` for a recording) or `bitmap` (packed 1-bpp frames, each the panel's framebuffer, drawn with `clip_play`). `bytes` reserves space up front (it grows in {grow} KiB steps anyway). At most {max} bytes per clip, {total} bytes of clips, {count} clips; all of it is lost on reboot and never enters the conversation. Fill it with `clip_append` or `clip_record`, then `clip_play`.",
                    PCM_RATE_HZ = clips::PCM_RATE_HZ,
                    MIC_RATE_HZ = MIC_RATE_HZ,
                    grow = clips::CLIP_GROW_BYTES / 1024,
                    max = CLIP_MAX_BYTES,
                    total = clips::CLIPS_TOTAL_MAX,
                    count = clips::CLIPS_MAX,
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "clip name, up to 32 characters, e.g. `take1`" },
                        "kind": { "type": "string", "description": "`pcm` or `bitmap`" },
                        "bytes": { "type": "integer", "description": "reserve this many bytes now" },
                        "rate_hz": { "type": "integer", "description": "sample rate of the PCM that will be appended, default 24000" },
                        "fps": { "type": "integer", "description": "bitmap playback rate, 1-30, default 8" },
                    },
                    "required": ["name", "kind"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "clip_append",
                "description": format!("Append decoded bytes to a clip. `data_b64` is base64 of raw PCM (16-bit LE mono at the clip's rate) or of whole framebuffer-sized frames. One call takes at most {} bytes decoded (~{} base64 characters), so a long recording or animation is several calls; nothing is echoed back, only the new length. Appending past the clip's ceiling or the pool budget is refused before a byte is copied.", CLIP_APPEND_MAX, CLIP_APPEND_MAX / 3 * 4),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "clip name" },
                        "data_b64": { "type": "string", "description": "base64 of PCM or of whole frames" },
                    },
                    "required": ["name", "data_b64"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "clip_record",
                "description": format!("Record the microphone into a clip, in PSRAM, instead of into a tool result. `ms` is how long to record (default 1000). The clip is created at {MIC_RATE_HZ} Hz if it does not exist; `clip_play` resamples it to the speaker's rate, so recording then playing sounds right rather than 1.5x fast. Recording stops early if the call's tool budget runs out, and the result says how much was kept."),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "clip name to record into" },
                        "ms": { "type": "integer", "description": "record this long, 100-60000, default 1000" },
                    },
                    "required": ["name"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "clip_play",
                "description": "Play a clip: PCM goes to the detected I²S speaker in 4096-sample writes (resampled to the amplifier's rate when it was recorded), bitmap frames are drawn on the OLED at the clip's fps. `loops` replays it. This call holds until the clip has played, so keep a take under the tool budget (`agent.tool_timeout_s`, 30 s by default) rather than expecting it to play in the background.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "clip name" },
                        "volume": { "type": "integer", "description": "0-100, PCM only, default 100 (no change)" },
                        "loops": { "type": "integer", "description": "play it this many times, 1-10, default 1" },
                    },
                    "required": ["name"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "clip_list",
                "description": "List the clips and where their bytes live: kind, length, duration or frame count, the pool (`psram` on this board), and how much of the pool budget they reserve.",
                "parameters": { "type": "object", "properties": {} },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "clip_drop",
                "description": "Free a clip, or every clip when `name` is omitted, so its PSRAM goes back before the next take.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "clip name; omit to drop all" },
                    },
                },
            },
        },
    ]))
}

/// A clip error as the tool layer reports it: a missing clip is `not_found`, a refusal to
/// allocate is `hardware` (it is the pool, not the arguments), everything else is the
/// caller's numbers.
fn as_tool_error(error: ClipError) -> McpError {
    match error {
        ClipError::Unknown(name) => McpError::not_found(format!("no clip `{name}`")),
        ClipError::NoMemory { .. } => McpError::hardware(error.message()),
        ClipError::WrongKind { .. } => McpError::invalid_params(error.message()),
        _ => McpError::invalid_params(error.message()),
    }
}

fn clip_name(args: &Value) -> Result<String> {
    let name = j::str_arg(args, "name")?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(McpError::invalid_params("`name` must not be empty"));
    }
    if trimmed.len() > 32 {
        return Err(McpError::invalid_params(format!(
            "`name` is {} bytes; at most 32 are kept",
            trimmed.len()
        )));
    }
    Ok(trimmed.to_string())
}

impl App {
    /// Entry point for every `clip_*` tool, called from `call_tool_inner`.
    pub(crate) fn clip_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "clip_new" => self.clip_new(args),
            "clip_append" => self.clip_append(args),
            "clip_record" => self.clip_record(args),
            "clip_play" => self.clip_play(args),
            "clip_list" => Ok(self.clips.json(self.frame_bytes())),
            "clip_drop" => self.clip_drop(args),
            other => Err(McpError::not_found(format!("no clip tool `{other}`"))),
        }
    }

    /// Framebuffer size of the detected panel, or zero when there is none: the number that
    /// turns a bitmap clip's bytes into frames.
    pub(crate) fn frame_bytes(&self) -> usize {
        self.devices
            .infos()
            .iter()
            .find(|d| d.kind == "ssd1306")
            .and_then(|d| {
                let w = d.detail["width"].as_u64()?;
                let h = d.detail["height"].as_u64()?;
                Some((w * h / 8) as usize)
            })
            .unwrap_or(0)
    }

    fn clip_new(&mut self, args: &Value) -> Result<Value> {
        let name = clip_name(args)?;
        let kind_text = j::str_arg(args, "kind")?;
        let kind = ClipKind::parse(&kind_text)
            .ok_or_else(|| McpError::invalid_params("`kind` must be `pcm` or `bitmap`"))?;
        let fps = j::optional_ranged_u64(args, "fps", 1, 30)?.unwrap_or(DEFAULT_FPS as u64) as u32;
        let rate = j::optional_ranged_u64(args, "rate_hz", 1, 192_000)?
            .map(|v| v as u32)
            .unwrap_or(clips::PCM_RATE_HZ);
        let frame = self.frame_bytes();
        let default_reserve = match kind {
            ClipKind::Pcm => CLIP_DEFAULT_PCM_BYTES,
            // Eight frames is one second at the default rate: enough to see something on
            // the panel without reserving a megabyte for a clip nobody has filled yet.
            ClipKind::Bitmap => frame.max(1) * 8,
        };
        let reserve = j::optional_ranged_u64(args, "bytes", 0, CLIP_MAX_BYTES as u64)?
            .map(|v| v as usize)
            .unwrap_or(default_reserve);
        let largest = self.hw.psram_largest_free().unwrap_or(0);
        let (buffer, pool) = self.hw.media_buffer(reserve).ok_or_else(|| {
            McpError::hardware(format!(
                "could not reserve {reserve} bytes for a clip: clips live in PSRAM, whose largest free block is {largest} bytes"
            ))
        })?;
        let clip = self.clips.open(&name, kind, pool, buffer).map_err(as_tool_error)?;
        clip.rate_hz = if kind == ClipKind::Pcm { rate } else { 0 };
        clip.fps = if kind == ClipKind::Bitmap { fps } else { 0 };
        let (rate, fps, reserved) = (clip.rate_hz, clip.fps, clip.bytes.capacity());
        Ok(json!({
            "ok": true,
            "name": name,
            "kind": kind.as_str(),
            "pool": pool,
            "reserved": reserved,
            "rate_hz": rate,
            "fps": fps,
            "clips": self.clips.json(frame),
        }))
    }

    fn clip_append(&mut self, args: &Value) -> Result<Value> {
        let name = clip_name(args)?;
        let encoded = j::str_arg(args, "data_b64")?;
        let mut data = Vec::new();
        util::b64_decode_limit(&encoded, &mut data, CLIP_APPEND_MAX)?;
        {
            let App { clips, hw, .. } = self;
            clips.append_with(&name, &data, &mut |need| hw.media_buffer(need)).map_err(as_tool_error)?;
        }
        let clip = self.clips.get(&name).ok_or_else(|| McpError::not_found(format!("no clip `{name}`")))?;
        let frames = clip.frames(self.frame_bytes());
        Ok(json!({
            "ok": true,
            "name": name,
            "bytes": clip.bytes.len(),
            "reserved": clip.bytes.capacity(),
            "ms": clip.ms(),
            "frames": frames,
            "pool": clip.pool,
        }))
    }

    fn clip_record(&mut self, args: &Value) -> Result<Value> {
        let name = clip_name(args)?;
        let ms = j::optional_ranged_u64(args, "ms", 100, 60_000)?.unwrap_or(1000);
        let target = (ms * u64::from(MIC_RATE_HZ) / 1000) as usize;
        let Some(mic) = self.devices.infos().into_iter().find(|d| d.kind == "mic") else {
            return Err(McpError::unsupported("clip_record needs a detected I2S microphone"));
        };
        crate::tool::ensure_tool_enabled_in(&self.cfg, &self.family_members, "mic_record")?;
        // A clip that already holds PCM at another rate would mix two clocks in one buffer,
        // and the playback would be right for neither half.
        match self.clips.get(&name) {
            Some(clip) if clip.kind != ClipKind::Pcm => {
                return Err(as_tool_error(ClipError::WrongKind {
                    name: name.clone(),
                    expected: ClipKind::Pcm,
                }))
            }
            Some(clip) if clip.rate_hz != MIC_RATE_HZ && !clip.bytes.is_empty() => {
                return Err(McpError::invalid_params(format!(
                    "clip `{name}` holds {} Hz audio; a recording is {MIC_RATE_HZ} Hz — record into an empty or new clip",
                    clip.rate_hz
                )))
            }
            _ => {}
        }
        if self.clips.get(&name).is_none() {
            let reserve = (target * 2).min(CLIP_MAX_BYTES);
            let largest = self.hw.psram_largest_free().unwrap_or(0);
            let (buffer, pool) = self.hw.media_buffer(reserve).ok_or_else(|| {
                McpError::hardware(format!(
                    "could not reserve {reserve} bytes for the recording: the largest free PSRAM block is {largest} bytes"
                ))
            })?;
            let clip = self.clips.open(&name, ClipKind::Pcm, pool, buffer).map_err(as_tool_error)?;
            clip.rate_hz = MIC_RATE_HZ;
        }
        if let Some(clip) = self.clips.get_mut(&name) {
            clip.rate_hz = MIC_RATE_HZ;
        }
        let started = self.hw.millis();
        let mut recorded = 0usize;
        let mut chunks = 0usize;
        while recorded < target {
            let wanted = (target - recorded).min(MAX_RECORD_SAMPLES);
            let asked = json!({ "samples": wanted });
            let answer = {
                let App { devices, hw, .. } = self;
                devices.call(&mic.id, "mic_record", &asked, hw.as_mut())
            }?;
            let wav = answer["wav_b64"].as_str().unwrap_or("");
            if wav.is_empty() {
                break;
            }
            let mut file = Vec::new();
            util::b64_decode(wav, &mut file)?;
            let pcm = clips::pcm_of_wav(&file);
            if pcm.is_empty() {
                break;
            }
            let appended = {
                let App { clips, hw, .. } = self;
                clips.append_with(&name, pcm, &mut |need| hw.media_buffer(need))
            };
            match appended {
                Ok(_) => {}
                Err(error) => {
                    // Whatever the microphone managed before the pool said no is kept: it
                    // is audio that was already recorded, not a failed request.
                    return Err(as_tool_error(error).with_data(json!({
                        "clip": name,
                        "samples": recorded,
                        "ms": recorded as u64 * 1000 / u64::from(MIC_RATE_HZ),
                        "chunks": chunks,
                    })));
                }
            }
            recorded += pcm.len() / 2;
            chunks += 1;
            if self.remaining_tool_ms() == Some(0) {
                break;
            }
        }
        let clip = self.clips.get(&name).ok_or_else(|| McpError::not_found(format!("no clip `{name}`")))?;
        Ok(json!({
            "ok": true,
            "name": name,
            "samples": clip.bytes.len() / 2,
            "ms": clip.ms(),
            "rate_hz": clip.rate_hz,
            "bytes": clip.bytes.len(),
            "pool": clip.pool,
            "chunks": chunks,
            "record_ms": self.hw.millis().saturating_sub(started),
            "complete": recorded >= target,
        }))
    }

    fn clip_play(&mut self, args: &Value) -> Result<Value> {
        let name = clip_name(args)?;
        let volume = j::optional_ranged_i64(args, "volume", 0, 100)?.unwrap_or(100) as u8;
        let loops = j::optional_ranged_u64(args, "loops", 1, 10)?.unwrap_or(1) as u32;
        let kind =
            self.clips.get(&name).ok_or_else(|| McpError::not_found(format!("no clip `{name}`")))?.kind;
        match kind {
            ClipKind::Pcm => self.play_clip_pcm(&name, volume, loops),
            ClipKind::Bitmap => self.play_clip_frames(&name, loops),
        }
    }

    fn play_clip_pcm(&mut self, name: &str, volume: u8, loops: u32) -> Result<Value> {
        let Some(speaker) = self.devices.infos().into_iter().find(|d| d.kind == "speaker") else {
            return Err(McpError::unsupported("clip_play needs a detected I2S speaker for a pcm clip"));
        };
        crate::tool::ensure_tool_enabled_in(&self.cfg, &self.family_members, "speaker_play")?;
        let to = clips::PCM_RATE_HZ;
        let (from, in_frames) = {
            let clip =
                self.clips.get(name).ok_or_else(|| McpError::not_found(format!("no clip `{name}`")))?;
            let from = if clip.rate_hz == 0 { to } else { clip.rate_hz };
            (from, clip.bytes.len() / 2)
        };
        if in_frames == 0 {
            return Err(McpError::invalid_params(format!("clip `{name}` is empty; append audio first")));
        }
        let out_frames = (in_frames as u64 * u64::from(to) / u64::from(from)) as usize;
        let started = self.hw.millis();
        let mut out: Vec<i16> = Vec::new();
        let mut samples_out = 0u64;
        let mut write_ms = 0u64;
        for _ in 0..loops {
            let mut done = 0usize;
            while done < out_frames {
                if self.out_of_time() {
                    return Err(McpError::timeout(format!(
                        "clip `{name}` played {} of {} ms before the tool budget ran out",
                        samples_out * 1000 / u64::from(to),
                        out_frames as u64 * 1000 / u64::from(to),
                    ))
                    .with_data(json!({ "name": name, "played_ms": samples_out * 1000 / u64::from(to) })));
                }
                let this = (out_frames - done).min(WRITE_SAMPLES);
                // Input span for this output span, plus one sample of lookahead so the last
                // output interpolates instead of repeating the boundary sample.
                let from_frame = (done as u64 * u64::from(from) / u64::from(to)) as usize;
                let to_frame =
                    (((done + this) as u64 * u64::from(from) / u64::from(to)) as usize + 1).min(in_frames);
                let writing = {
                    let App { clips, hw, .. } = self;
                    let clip =
                        clips.get(name).ok_or_else(|| McpError::not_found(format!("no clip `{name}`")))?;
                    clips::resample(&clip.bytes[from_frame * 2..to_frame * 2], from, to, this, &mut out);
                    if volume < 100 {
                        let scale = f32::from(volume) / 100.0;
                        for sample in out.iter_mut() {
                            *sample = (f32::from(*sample) * scale) as i16;
                        }
                    }
                    let writing = hw.millis();
                    hw.i2s_out_write(&out).map_err(|e| {
                        McpError::hardware(format!(
                            "the speaker stopped after {} ms: {e}",
                            samples_out * 1000 / u64::from(to)
                        ))
                    })?;
                    writing
                };
                write_ms += self.hw.millis().saturating_sub(writing);
                samples_out += out.len() as u64;
                done += this;
            }
        }
        Ok(json!({
            "ok": true,
            "name": name,
            "kind": "pcm",
            "samples": samples_out,
            "played_ms": samples_out * 1000 / u64::from(to),
            "loops": loops,
            "resampled": from != to,
            "write_ms": write_ms,
            "elapsed_ms": self.hw.millis().saturating_sub(started),
            "speaker": speaker.id,
        }))
    }

    fn play_clip_frames(&mut self, name: &str, loops: u32) -> Result<Value> {
        let frame_bytes = self.frame_bytes();
        if frame_bytes == 0 {
            return Err(McpError::unsupported("clip_play needs a detected OLED to draw a bitmap clip"));
        }
        let Some(oled) = self.devices.infos().into_iter().find(|d| d.kind == "ssd1306") else {
            return Err(McpError::unsupported("clip_play needs a detected OLED to draw a bitmap clip"));
        };
        crate::tool::ensure_tool_enabled_in(&self.cfg, &self.family_members, "oled_bitmap")?;
        let started = self.hw.millis();
        let mut drawn = 0usize;
        let (fps, total) = {
            let clip =
                self.clips.get(name).ok_or_else(|| McpError::not_found(format!("no clip `{name}`")))?;
            if clip.bytes.len() % frame_bytes != 0 {
                return Err(McpError::invalid_params(format!(
                    "clip `{name}` holds {} bytes, which is not a whole number of {frame_bytes}-byte frames; append whole frames",
                    clip.bytes.len()
                )));
            }
            (clip.fps.max(1), clip.bytes.len() / frame_bytes)
        };
        if total == 0 {
            return Err(McpError::invalid_params(format!("clip `{name}` is empty; append frames first")));
        }
        let frame_ms = 1000 / u64::from(fps);
        for _ in 0..loops {
            for index in 0..total {
                if self.out_of_time() {
                    return Err(McpError::timeout(format!(
                        "clip `{name}` drew {drawn} of {} frames before the tool budget ran out",
                        total * loops as usize
                    ))
                    .with_data(json!({ "name": name, "frames_drawn": drawn })));
                }
                let args = {
                    let clip = self.clips.get(name).expect("checked above");
                    let start = index * frame_bytes;
                    json!({ "bitmap_b64": util::b64_encode(&clip.bytes[start..start + frame_bytes]) })
                };
                {
                    let App { devices, hw, .. } = self;
                    devices.call(&oled.id, crate::drivers::ssd1306::MEDIA_FRAME_TOOL, &args, hw.as_mut())?;
                }
                drawn += 1;
                // The panel is the clock: the same delay `media_play` settles into, so a
                // clip and a melody played together would not drift apart by fps rounding.
                let elapsed = self.hw.millis().saturating_sub(started);
                let due = drawn as u64 * frame_ms;
                if due > elapsed {
                    self.hw.delay_us(((due - elapsed) * 1000) as u32);
                }
            }
        }
        Ok(json!({
            "ok": true,
            "name": name,
            "kind": "bitmap",
            "frames_drawn": drawn,
            "frames": total,
            "fps": fps,
            "played_ms": total as u64 * loops as u64 * frame_ms,
            "elapsed_ms": self.hw.millis().saturating_sub(started),
            "panel": oled.id,
        }))
    }

    fn clip_drop(&mut self, args: &Value) -> Result<Value> {
        let before = self.clips.len();
        let dropped = match args.get("name") {
            Some(_) => {
                let name = clip_name(args)?;
                let dropped = self.clips.drop_clip(&name);
                if !dropped {
                    return Err(McpError::not_found(format!("no clip `{name}`")));
                }
                vec![name]
            }
            None => {
                let names = self.clips.names();
                for name in &names {
                    self.clips.drop_clip(name);
                }
                names
            }
        };
        Ok(json!({
            "ok": true,
            "dropped": dropped,
            "count": before.saturating_sub(dropped.len()),
            "clips": self.clips.json(self.frame_bytes()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FwMeta;
    use crate::logbuf::LogBuf;
    use crate::store::MemoryStore;
    use crate::virtual_board::VirtualBoard;

    /// The bench profile, so the fixture has the speaker, the OLED and the microphone the
    /// clip tools need. The virtual board reports its PSRAM pool by name.
    fn fixture() -> App {
        let mut app = App::new(
            Box::new(VirtualBoard::new()),
            Box::new(MemoryStore::new()),
            LogBuf::new(32),
            FwMeta::default(),
        );
        app.cfg.board = "esp32s3-xiaozhi-bread".into();
        app.cfg.pins.insert("oled.height".into(), 32);
        app.rebuild();
        assert!(app.devices.has_cap("speaker") && app.devices.has_cap("oled"));
        assert!(app.devices.has_cap("mic"));
        app
    }

    fn call(app: &mut App, name: &str, args: Value) -> Result<Value> {
        app.call_tool(name, &args)
    }

    fn pcm_b64(samples: usize, value: i16) -> String {
        let bytes: Vec<u8> = (0..samples).flat_map(|_| value.to_le_bytes()).collect();
        util::b64_encode(&bytes)
    }

    #[test]
    fn a_clip_reserves_the_psram_pool_and_says_so() {
        let mut app = fixture();
        let made = call(&mut app, "clip_new", json!({"name": "take1", "kind": "pcm", "bytes": 48000}))
            .expect("clip_new");
        assert_eq!(made["pool"], "psram", "the bench board's clips live in PSRAM");
        assert_eq!(made["reserved"], 48000);
        assert_eq!(made["rate_hz"], clips::PCM_RATE_HZ);
        // The same numbers reach `/api/status`, because a person watching the heap needs to
        // see what the clips reserved, not only what the allocator happened to leave.
        let status = app.status_json();
        assert_eq!(status["clips"]["count"], 1);
        assert_eq!(status["clips"]["clips"][0]["name"], "take1");
        assert_eq!(status["clips"]["reserved_bytes"], 48000);
        assert_eq!(status["clips"]["clips"][0]["pool"], "psram");
        // Dropping it gives the reservation back rather than leaving a hole in the budget.
        let dropped = call(&mut app, "clip_drop", json!({"name": "take1"})).expect("clip_drop");
        assert_eq!(dropped["count"], 0);
        assert_eq!(app.clips.total_bytes(), 0);
    }

    #[test]
    fn appended_audio_plays_at_its_own_rate_and_costs_one_call() {
        let mut app = fixture();
        call(&mut app, "clip_new", json!({"name": "tone", "kind": "pcm"})).expect("clip_new");
        // Half a second of 24 kHz mono, appended in 12 KiB pieces: the shape a model uses
        // because one `clip_append` cannot carry more than 48 KiB decoded.
        for _ in 0..2 {
            let appended =
                call(&mut app, "clip_append", json!({"name": "tone", "data_b64": pcm_b64(6000, 1000)}))
                    .expect("clip_append");
            assert_eq!(appended["pool"], "psram");
        }
        assert_eq!(app.clips.get("tone").unwrap().bytes.len(), 24_000, "24000 bytes of PCM");
        let played = call(&mut app, "clip_play", json!({"name": "tone"})).expect("clip_play");
        assert_eq!(played["played_ms"], 500, "24000 bytes is half a second at 24 kHz");
        assert_eq!(played["samples"], 12_000);
        assert_eq!(played["resampled"], false, "the clip is already at the amplifier's rate");
        assert_eq!(played["loops"], 1);
    }

    #[test]
    fn a_recording_is_stored_at_the_microphone_rate_and_resampled_on_the_way_out() {
        let mut app = fixture();
        let recorded = call(&mut app, "clip_record", json!({"name": "take", "ms": 250})).expect("record");
        assert_eq!(recorded["rate_hz"], MIC_RATE_HZ, "it is recorded at the microphone's rate");
        assert_eq!(recorded["pool"], "psram");
        assert!(recorded["samples"].as_u64().unwrap() > 0, "{recorded}");
        let samples = recorded["samples"].as_u64().unwrap() as usize;
        // WAV container stripped: the clip holds samples, so the playback has no header to
        // mistake for audio and no per-call decoding.
        assert_eq!(app.clips.get("take").unwrap().bytes.len(), samples * 2);
        let played = call(&mut app, "clip_play", json!({"name": "take"})).expect("clip_play");
        assert_eq!(played["resampled"], true, "16 kHz in, 24 kHz out");
        assert_eq!(
            played["played_ms"],
            recorded["ms"].as_u64().unwrap(),
            "resampling keeps the duration: the same audio, not a faster one"
        );
        // Recording into a clip that holds a different rate would mix two clocks.
        call(&mut app, "clip_new", json!({"name": "loud", "kind": "pcm"})).expect("clip_new");
        call(&mut app, "clip_append", json!({"name": "loud", "data_b64": pcm_b64(10, 0)})).expect("append");
        let err = call(&mut app, "clip_record", json!({"name": "loud", "ms": 100}))
            .expect_err("a 24 kHz clip cannot take a 16 kHz take");
        assert!(err.message.contains("16000"), "{}", err.message);
    }

    #[test]
    fn frames_are_drawn_at_the_clips_rate() {
        let mut app = fixture();
        let frame_bytes = app.frame_bytes();
        assert_eq!(frame_bytes, 128 * 32 / 8, "the panel's framebuffer");
        call(&mut app, "clip_new", json!({"name": "anim", "kind": "bitmap", "fps": 4})).expect("clip_new");
        let frames: Vec<u8> = (0..frame_bytes * 2).map(|i| (i % 251) as u8).collect();
        let appended =
            call(&mut app, "clip_append", json!({"name": "anim", "data_b64": util::b64_encode(&frames)}))
                .expect("append");
        assert_eq!(appended["frames"], 2);
        let played = call(&mut app, "clip_play", json!({"name": "anim"})).expect("play");
        assert_eq!(played["frames_drawn"], 2);
        assert_eq!(played["fps"], 4);
        assert_eq!(played["played_ms"], 500, "two frames at 4 fps");
        // A partial frame is a mistake worth naming, not a frame to draw half of.
        call(&mut app, "clip_append", json!({"name": "anim", "data_b64": util::b64_encode(&[0u8; 8])}))
            .expect("append a partial frame");
        let err = call(&mut app, "clip_play", json!({"name": "anim"}))
            .expect_err("a clip that is not whole frames cannot be played");
        assert!(err.message.contains("whole number"), "{}", err.message);
    }

    #[test]
    fn the_ceilings_are_named_in_the_refusal() {
        let mut app = fixture();
        call(&mut app, "clip_new", json!({"name": "big", "kind": "pcm"})).expect("clip_new");
        // 48 KiB + 1 decoded is over the per-call argument ceiling, and the refusal quotes
        // both numbers instead of silently dropping the request.
        let too_much = util::b64_encode(&vec![0u8; CLIP_APPEND_MAX + 1]);
        let err = call(&mut app, "clip_append", json!({"name": "big", "data_b64": too_much}))
            .expect_err("over the append ceiling");
        assert!(err.message.contains("49152"), "{}", err.message);
        // A name nobody has is `not_found`, not an empty success.
        let err = call(&mut app, "clip_play", json!({"name": "ghost"})).expect_err("no such clip");
        assert!(err.message.contains("no clip `ghost`"), "{}", err.message);
    }

    #[test]
    fn a_switched_off_clip_tool_cannot_be_reached() {
        let mut app = fixture();
        app.cfg.mcp.disabled_tools.push("clip_play".into());
        let err = call(&mut app, "clip_play", json!({"name": "x"})).expect_err("disabled");
        assert!(err.message.contains("disabled"), "{}", err.message);
        // The other clip tools stay reachable: one switch is one tool, not a family.
        call(&mut app, "clip_new", json!({"name": "x", "kind": "pcm"})).expect("clip_new");
    }
}

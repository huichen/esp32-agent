//! One synchronous timeline for a synthesized score and OLED frames.
//!
//! The complete plan is validated before any write, then the complete score is
//! synthesized once. I2S is fed in bounded chunks; frame deadlines are checked on
//! the same monotonic clock between them. This is pacing, not a media worker.

use crate::device::Devices;
use crate::drivers::audio::{self, AMP_RATE_HZ, MAX_MELODY_MS, MAX_MELODY_NOTES, MAX_NOTE_MS, MIN_NOTE_MS};
use crate::drivers::ssd1306::{MediaFrame, MAX_FRAME_RECTS, MEDIA_FRAME_TOOL};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::json as j;
use crate::tool::{boolean, integer, object_schema, string, ToolCtx, ToolDef};
use serde_json::{json, Value};
use std::time::Duration;

/// At most a 12 s, 30 fps strip. Duplicate timestamps still consume entries.
pub const MAX_MEDIA_FRAMES: usize = MAX_MELODY_MS as usize * 30 / 1000;
/// Tool-argument budget, independent of the 64 KiB tool-result ceiling.
pub const MAX_MEDIA_BITMAP_BYTES: usize = 32 * 1024;
const WRITE_SAMPLES: usize = 4_096;

/// The compound tool exists when either medium is available. Its handler checks
/// only the devices the supplied plan actually uses, so frame-only calls need no amp.
pub fn tools(devices: &Devices) -> Vec<ToolDef> {
    if !devices.has_cap("speaker") && !devices.has_cap("oled") {
        return Vec::new();
    }
    let mut schema = object_schema(
        &[
            ("notes", json!({
                "type": "array", "minItems": 1, "maxItems": MAX_MELODY_NOTES,
                "description": "the speaker_play score: [{hz, ms}, ...]; hz 0 is a rest, at most 12 s total",
                "items": {
                    "type": "object",
                    "properties": {
                        "hz": integer(0, 8000, "frequency in Hz; 0 is a rest"),
                        "ms": integer(MIN_NOTE_MS.into(), MAX_NOTE_MS.into(), "note duration in milliseconds"),
                    },
                    "required": ["hz", "ms"],
                },
            })),
            ("frames", json!({
                "type": "array", "minItems": 1, "maxItems": MAX_MEDIA_FRAMES,
                "description": "frames in any order, each drawn once at/after its at_ms slot; at most 360 entries and 32768 decoded bitmap bytes in the whole request",
                "items": {
                    "type": "object",
                    "properties": {
                        "at_ms": integer(0, MAX_MELODY_MS.into(), "offset from playback start, excluding synthesis"),
                        "bitmap_b64": string("base64 of a full-panel 1-bpp bitmap, oled_bitmap/POST /av page format, bit0 = top row; exactly 512 bytes (128x32) or 1024 (128x64); cannot be combined with rects, text or line"),
                        "rects": {
                            "type": "array", "maxItems": MAX_FRAME_RECTS,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "x": integer(0, 127, "left pixel, must be inside the detected panel"),
                                    "y": integer(0, 63, "top pixel, must be inside the detected panel"),
                                    "w": integer(1, 128, "width, clipped at the panel's right edge"),
                                    "h": integer(1, 64, "height, clipped at the panel's bottom edge"),
                                    "fill": boolean("filled (default true) or outlined"),
                                },
                                "required": ["x", "y", "w", "h"],
                                "additionalProperties": false,
                            },
                        },
                        "text": string("ASCII text using oled_text's 6x8 font and wrapping"),
                        "line": integer(0, 7, "text row, default 0; must be inside the detected panel"),
                    },
                    "required": ["at_ms"],
                    "oneOf": [
                        {"required": ["bitmap_b64"], "not": {"anyOf": [
                            {"required": ["rects"]}, {"required": ["text"]}, {"required": ["line"]},
                        ]}},
                        {"not": {"required": ["bitmap_b64"]}, "anyOf": [
                            {"required": ["rects"]}, {"required": ["text"]},
                        ]},
                    ],
                    "additionalProperties": false,
                },
            })),
            ("volume", integer(0, 100, "amplitude percent, default 30")),
            ("fps", integer(1, 30, "idle pacing hint, derived from timestamp spacing by default; clamped 1-30, at_ms stays authoritative")),
        ],
        &[],
    );
    schema["anyOf"] = json!([{"required": ["notes"]}, {"required": ["frames"]}]);
    vec![ToolDef::new(
        "media_play",
        "Play a score and OLED frames on one clock in one synchronous call. Optional notes use speaker_play's 64-note / 12 s budget; frames use at_ms slots (0-12000), replace the panel with a bitmap or rectangles plus text in one flush, and accept at most 32 KiB of decoded bitmap arguments per call. All frames are validated before playback. Audio is synthesized once and written in 4096-sample chunks; frames due between writes are drawn in timestamp order, including after the sound ends. The result reports frames_drawn, frames_late and max_late_ms rather than claiming exact audiovisual sync. This holds the caller; background playback is not implemented (docs/audio.md).",
    )
    .title("Play sound and picture")
    .category("audio")
    .mutating()
    .schema(schema)
    .bind(|ctx, args| play_with_wait(ctx, args, |_, ms| std::thread::sleep(Duration::from_millis(ms))))]
}

struct Frame {
    at_ms: u64,
    arguments: Value,
}

struct Plan {
    score: Vec<(f32, u32)>,
    frames: Vec<Frame>,
    panel: Option<String>,
    volume: f32,
    fps: u64,
}

/// The internal batch must not turn a disabled public action into a back door.
/// Manifest driver policies cannot be executed atomically by this compound tool;
/// refuse those plans before playback rather than silently bypass their safety rules.
fn require_action(ctx: &ToolCtx<'_>, id: &str, raw: &str) -> Result<()> {
    let (public, resolved) = ctx.devices.resolve_tool(id, raw)?;
    ctx.ensure_enabled(&public)?;
    if let Some(policy) =
        ctx.manifest.driver_tool(id, &public).or_else(|| ctx.manifest.driver_tool(id, &resolved))
    {
        return Err(McpError::forbidden(format!(
            "media_play cannot bypass manifest driver policy `{}` for `{public}`; use that action directly",
            policy.name
        )));
    }
    Ok(())
}

fn array<'a>(args: &'a Value, name: &str) -> Result<Option<&'a Vec<Value>>> {
    args.get(name)
        .map(|v| v.as_array().ok_or_else(|| McpError::invalid_params(format!("`{name}` must be an array"))))
        .transpose()
}

fn prepare(ctx: &ToolCtx<'_>, args: &Value) -> Result<Plan> {
    if !args.is_object() {
        return Err(McpError::invalid_params("tool arguments must be an object"));
    }
    let notes = array(args, "notes")?;
    let frames_arg = array(args, "frames")?;
    if notes.is_none() && frames_arg.is_none() {
        return Err(McpError::invalid_params("at least one of `notes` or `frames` is required"));
    }
    let score =
        notes.map(|notes| audio::score_of(notes).map(|(score, _)| score)).transpose()?.unwrap_or_default();
    let volume = j::optional_ranged_i64(args, "volume", 0, 100)?.unwrap_or(30) as f32 / 100.0;
    let fps = j::optional_ranged_i64(args, "fps", i64::MIN, i64::MAX)?.map(|v| v.clamp(1, 30) as u64);
    let infos = ctx.devices.infos();
    if notes.is_some() {
        let speaker = infos
            .iter()
            .find(|d| d.kind == "speaker")
            .ok_or_else(|| McpError::unsupported("media_play `notes` needs a detected I2S speaker"))?;
        require_action(ctx, &speaker.id, "speaker_play")?;
    }
    let mut panel = None;
    let mut frames = Vec::new();
    if let Some(entries) = frames_arg {
        if entries.is_empty() {
            return Err(McpError::invalid_params("`frames` is empty; give at least one drawable frame"));
        }
        if entries.len() > MAX_MEDIA_FRAMES {
            return Err(McpError::invalid_params(format!(
                "`frames` has {} entries; at most {MAX_MEDIA_FRAMES} are accepted",
                entries.len()
            )));
        }
        let oled = infos
            .iter()
            .find(|d| d.kind == "ssd1306")
            .ok_or_else(|| McpError::unsupported("media_play `frames` needs a detected OLED"))?;
        let width = oled.detail["width"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| McpError::internal("OLED width is missing"))?;
        let height = oled.detail["height"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| McpError::internal("OLED height is missing"))?;
        let mut bitmap_bytes = 0;
        for (index, frame) in entries.iter().enumerate() {
            let bad = |e: McpError| McpError::invalid_params(format!("`frames[{index}]` {}", e.message));
            let at_ms = j::optional_ranged_u64(frame, "at_ms", 0, MAX_MELODY_MS.into())
                .map_err(bad)?
                .ok_or_else(|| bad(McpError::invalid_params("`at_ms` is required")))?;
            let content = MediaFrame::parse(frame, width, height).map_err(bad)?;
            if let Some(bytes) = content.bitmap_bytes() {
                bitmap_bytes += bytes;
                if bitmap_bytes > MAX_MEDIA_BITMAP_BYTES {
                    return Err(bad(McpError::invalid_params(format!(
                        "brings decoded bitmap arguments to {bitmap_bytes} bytes; at most {MAX_MEDIA_BITMAP_BYTES} bytes are accepted per call",
                    ))));
                }
                require_action(ctx, &oled.id, "oled_bitmap")?;
            }
            if content.has_rects() {
                require_action(ctx, &oled.id, "oled_rect")?;
            }
            if content.has_text() {
                require_action(ctx, &oled.id, "oled_text")?;
            }
            frames.push(Frame { at_ms, arguments: content.arguments() });
        }
        // Stable: two frames sharing a slot retain their input order, and neither is lost.
        frames.sort_by_key(|frame| frame.at_ms);
        panel = Some(oled.id.clone());
    }
    let spacing = frames.windows(2).map(|pair| pair[1].at_ms - pair[0].at_ms).filter(|ms| *ms > 0).min();
    let fps = fps.unwrap_or_else(|| spacing.map(|ms| 1000u64.div_ceil(ms).clamp(1, 30)).unwrap_or(1));
    Ok(Plan { score, frames, panel, volume, fps })
}

#[derive(Default)]
struct Progress {
    notes: usize,
    frames_drawn: usize,
    frames_late: usize,
    samples: u64,
    synth_ms: u64,
    write_ms: u64,
    max_late_ms: u64,
}

impl Progress {
    /// Counts only, never a frame/PCM echo: even a maximum plan fits one 64 KiB result.
    fn result(&self, ok: bool) -> Value {
        json!({
            "ok": ok,
            "notes": self.notes,
            "frames_drawn": self.frames_drawn,
            "frames_late": self.frames_late,
            "played_ms": self.samples * 1000 / u64::from(AMP_RATE_HZ),
            "synth_ms": self.synth_ms,
            "write_ms": self.write_ms,
            "max_late_ms": self.max_late_ms,
        })
    }

    fn failure(&self, mut error: McpError) -> McpError {
        error.data = Some(json!({"media_play": self.result(false), "cause": error.data.take()}));
        error
    }
}

fn draw_due(
    ctx: &mut ToolCtx<'_>,
    plan: &Plan,
    started: u64,
    next: &mut usize,
    progress: &mut Progress,
) -> Result<()> {
    while let Some(frame) = plan.frames.get(*next) {
        if ctx.hw.millis().saturating_sub(started) < frame.at_ms {
            break;
        }
        ctx.devices.call(
            plan.panel.as_deref().expect("a frame has a panel"),
            MEDIA_FRAME_TOOL,
            &frame.arguments,
            ctx.hw,
        )?;
        // A frame is drawn when its flush completes, not when the first page starts.
        let late_ms = ctx.hw.millis().saturating_sub(started).saturating_sub(frame.at_ms);
        progress.frames_drawn += 1;
        progress.frames_late += usize::from(late_ms > 0);
        progress.max_late_ms = progress.max_late_ms.max(late_ms);
        *next += 1;
    }
    Ok(())
}

/// Waiting is injectable only to make the simulator's clock deterministic in tests.
/// Production sleeps on the calling thread; it creates no worker and releases no lock.
fn play_with_wait(
    ctx: &mut ToolCtx<'_>,
    args: &Value,
    mut wait: impl FnMut(&mut dyn Hardware, u64),
) -> Result<Value> {
    let plan = prepare(ctx, args)?;
    let mut progress = Progress { notes: plan.score.len(), ..Default::default() };
    let synthesis = ctx.hw.millis();
    let samples = audio::melody(&plan.score, plan.volume);
    progress.synth_ms = ctx.hw.millis().saturating_sub(synthesis);
    // Synthesis is preparation, not part of the shared playback timeline.
    let started = ctx.hw.millis();
    let mut next = 0;
    draw_due(ctx, &plan, started, &mut next, &mut progress).map_err(|e| progress.failure(e))?;
    for piece in samples.chunks(WRITE_SAMPLES) {
        let writing = ctx.hw.millis();
        let result = ctx.hw.i2s_out_write(piece);
        progress.write_ms = progress.write_ms.saturating_add(ctx.hw.millis().saturating_sub(writing));
        result.map_err(|e| progress.failure(e))?;
        progress.samples += piece.len() as u64;
        draw_due(ctx, &plan, started, &mut next, &mut progress).map_err(|e| progress.failure(e))?;
    }
    // A frame-only plan, or a picture outlasting its melody, still owns its deadlines.
    // fps limits idle wakeups, not frame slots; catching up never drops/coalesces frames.
    while let Some(frame) = plan.frames.get(next) {
        let elapsed = ctx.hw.millis().saturating_sub(started);
        let remaining = frame.at_ms.saturating_sub(elapsed);
        if remaining > 0 {
            wait(ctx.hw, remaining.min(1000 / plan.fps));
        }
        draw_due(ctx, &plan, started, &mut next, &mut progress).map_err(|e| progress.failure(e))?;
    }
    Ok(progress.result(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, FwMeta};
    use crate::boards::I2sPins;
    use crate::drivers::ssd1306::{Controller, Ssd1306};
    use crate::hardware::{BoardInfo, PinMode};
    use crate::logbuf::LogBuf;
    use crate::store::MemoryStore;
    use crate::virtual_board::VirtualBoard;
    use std::sync::{Arc, Mutex};

    struct DrawnFrame {
        at_ms: u64,
        samples: u64,
        pixels: Vec<u8>,
    }

    struct Recording {
        board: VirtualBoard,
        now_us: u64,
        writes: Vec<usize>,
        pcm: Vec<i16>,
        frames: Vec<DrawnFrame>,
        page: usize,
        pixels: Vec<u8>,
        frame_delay_ms: u64,
        write_delay_ms: u64,
        fail_write_after: Option<usize>,
    }

    /// The simulator's actual OLED and I2S, with a deterministic monotonic clock.
    /// Each full four-page flush is recorded independently of the tool's counters.
    #[derive(Clone)]
    struct RecordedBoard(Arc<Mutex<Recording>>);

    impl RecordedBoard {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Recording {
                board: VirtualBoard::new(),
                now_us: 0,
                writes: Vec::new(),
                pcm: Vec::new(),
                frames: Vec::new(),
                page: 0,
                pixels: vec![0; 512],
                frame_delay_ms: 0,
                write_delay_ms: 0,
                fail_write_after: None,
            })))
        }

        fn reset(&self) {
            let mut state = self.0.lock().unwrap();
            state.now_us = 0;
            state.board.i2s_written = 0;
            state.writes.clear();
            state.pcm.clear();
            state.frames.clear();
        }
    }

    impl Hardware for RecordedBoard {
        fn board_info(&mut self) -> BoardInfo {
            self.0.lock().unwrap().board.board_info()
        }

        fn stack_is_internal(&self) -> bool {
            false
        }

        fn i2c_scan(&mut self, bus: u8) -> Result<Vec<u8>> {
            self.0.lock().unwrap().board.i2c_scan(bus)
        }

        fn i2c_read(&mut self, bus: u8, addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
            self.0.lock().unwrap().board.i2c_read(bus, addr, reg, out)
        }

        fn i2c_write(&mut self, bus: u8, addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.board.i2c_write(bus, addr, reg, data)?;
            if addr == 0x3C && reg == Some(0) && data.len() == 3 && data[0] & 0xF8 == 0xB0 {
                state.page = usize::from(data[0] & 7);
            }
            if addr == 0x3C && reg == Some(0x40) && data.len() == 128 && state.page < 4 {
                let start = state.page * 128;
                state.pixels[start..start + 128].copy_from_slice(data);
                if state.page == 3 {
                    state.now_us += state.frame_delay_ms * 1000;
                    let frame = DrawnFrame {
                        at_ms: state.now_us / 1000,
                        samples: state.board.i2s_written,
                        pixels: state.pixels.clone(),
                    };
                    state.frames.push(frame);
                }
            }
            Ok(())
        }

        fn gpio_config(&mut self, pin: i32, mode: PinMode) -> Result<()> {
            self.0.lock().unwrap().board.gpio_config(pin, mode)
        }

        fn gpio_read(&mut self, pin: i32) -> Result<bool> {
            self.0.lock().unwrap().board.gpio_read(pin)
        }

        fn ring_set(&mut self, pin: i32, rgb: (u8, u8, u8), count: u16) -> Result<()> {
            self.0.lock().unwrap().board.ring_set(pin, rgb, count)
        }

        fn i2s_setup(&mut self, mic: Option<I2sPins>, amp: Option<I2sPins>) -> Result<()> {
            self.0.lock().unwrap().board.i2s_setup(mic, amp)
        }

        fn i2s_out_write(&mut self, samples: &[i16]) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            if state.fail_write_after == Some(state.writes.len()) {
                return Err(McpError::hardware("virtual amplifier stopped"));
            }
            let before = state.board.i2s_written;
            state.board.i2s_out_write(samples)?;
            let after = state.board.i2s_written;
            state.now_us +=
                after * 1_000_000 / u64::from(AMP_RATE_HZ) - before * 1_000_000 / u64::from(AMP_RATE_HZ);
            state.now_us += state.write_delay_ms * 1000;
            state.writes.push(samples.len());
            state.pcm.extend_from_slice(samples);
            Ok(())
        }

        fn delay_us(&mut self, us: u32) {
            self.0.lock().unwrap().now_us += u64::from(us);
        }

        fn millis(&mut self) -> u64 {
            self.0.lock().unwrap().now_us / 1000
        }
    }

    fn fixture() -> (App, RecordedBoard) {
        let board = RecordedBoard::new();
        let mut app = App::new(
            Box::new(board.clone()),
            Box::new(MemoryStore::new()),
            LogBuf::new(32),
            FwMeta::default(),
        );
        app.cfg.board = "esp32s3-xiaozhi-bread".into();
        app.cfg.pins.insert("oled.height".into(), 32);
        app.rebuild();
        assert!(app.devices.has_cap("speaker") && app.devices.has_cap("oled"));
        board.reset();
        (app, board)
    }

    fn run(app: &mut App, args: &Value) -> Result<Value> {
        let mut ctx = ToolCtx {
            hw: &mut *app.hw,
            devices: &mut app.devices,
            cfg: &mut app.cfg,
            runtime: &mut app.runtime,
            log: &app.log,
            caps: &app.caps,
            profile: app.profile,
            probe: &app.probe,
            manifest: &mut app.manifest,
            taken: &[],
            tasks: &mut app.tasks,
            rules: &mut app.rules,
            families: &app.family_members,
        };
        play_with_wait(&mut ctx, args, |hw, ms| hw.delay_us((ms * 1000) as u32))
    }

    fn planned_fps(app: &mut App, args: &Value) -> u64 {
        let ctx = ToolCtx {
            hw: &mut *app.hw,
            devices: &mut app.devices,
            cfg: &mut app.cfg,
            runtime: &mut app.runtime,
            log: &app.log,
            caps: &app.caps,
            profile: app.profile,
            probe: &app.probe,
            manifest: &mut app.manifest,
            taken: &[],
            tasks: &mut app.tasks,
            rules: &mut app.rules,
            families: &app.family_members,
        };
        prepare(&ctx, args).unwrap().fps
    }

    fn text_pixels(text: &str) -> Vec<u8> {
        let mut hw = VirtualBoard::new();
        let mut oled = Ssd1306::probe(&mut hw, 0, 0x3C, 32, Controller::Ssd1306, 1).unwrap();
        oled.clear();
        oled.text(0, 0, text, true);
        oled.framebuffer().to_vec()
    }

    fn bitmap_frame(at_ms: u64, bytes: &[u8]) -> Value {
        json!({"at_ms": at_ms, "bitmap_b64": crate::util::b64_encode(bytes)})
    }

    fn bitmap_pattern() -> Vec<u8> {
        (0..512).map(|index| if index / 128 % 2 == 0 { 0xAA } else { 0x55 }).collect()
    }

    fn no_output(board: &RecordedBoard) {
        let state = board.0.lock().unwrap();
        assert_eq!(state.board.i2s_written, 0, "invalid parameters must not play a partial melody");
        assert!(state.writes.is_empty());
        assert!(state.frames.is_empty(), "validation also precedes display writes");
    }

    #[test]
    fn frames_are_stably_ordered_and_drawn_once() {
        let (mut app, board) = fixture();
        let result = run(
            &mut app,
            &json!({
                "notes": [{"hz": 440, "ms": 200}, {"hz": 0, "ms": 200}],
                "frames": [
                    {"at_ms": 200, "text": "B"}, {"at_ms": 0, "text": "A"},
                    {"at_ms": 200, "text": "C"}, {"at_ms": 100, "text": "D"},
                ],
            }),
        )
        .unwrap();
        let state = board.0.lock().unwrap();
        let pixels: Vec<_> = state.frames.iter().map(|f| f.pixels.clone()).collect();
        assert_eq!(pixels, ["A", "D", "B", "C"].map(text_pixels).to_vec());
        assert_eq!(state.frames.iter().map(|f| f.at_ms).collect::<Vec<_>>(), [0, 170, 341, 341]);
        assert_eq!(state.writes, [4096, 4096, 1408]);
        assert_eq!(state.pcm, audio::melody(&[(440.0, 200), (0.0, 200)], 0.3));
        assert_eq!(result["notes"], 2);
        assert_eq!(result["frames_drawn"], state.frames.len());
        assert_eq!(result["frames_late"], 3);
        assert_eq!(result["max_late_ms"], 141);
        assert_eq!(result["played_ms"], 400);
    }

    #[test]
    fn bitmap_frames_preserve_the_exact_oled_bitmap_bytes() {
        let (mut app, board) = fixture();
        let bytes = bitmap_pattern();
        let result = run(&mut app, &json!({"frames": [bitmap_frame(0, &bytes)]})).unwrap();
        {
            let state = board.0.lock().unwrap();
            assert_eq!(state.frames.len(), 1);
            assert_eq!(state.frames[0].pixels, bytes, "every page/column byte reached the virtual panel");
            assert!(state.writes.is_empty(), "a bitmap-only plan needs no audio write");
        }
        assert_eq!(result["frames_drawn"], 1);
        assert_eq!(result["frames_late"], 0);
        assert_eq!(result["played_ms"], 0);

        let legacy = app
            .call_tool(
                "display",
                &json!({
                    "command": "oled_bitmap", "arguments": {"data_b64": crate::util::b64_encode(&bytes)},
                }),
            )
            .unwrap();
        assert_eq!(legacy["bytes"], bytes.len());
        let state = board.0.lock().unwrap();
        assert_eq!(state.frames.len(), 2);
        assert_eq!(state.frames[1].pixels, bytes, "existing oled_bitmap draws the same unmodified bytes");
    }

    #[test]
    fn bitmap_and_vector_frames_share_the_timestamp_order() {
        let (mut app, board) = fixture();
        let bytes = bitmap_pattern();
        let inverse: Vec<_> = bytes.iter().map(|byte| !byte).collect();
        let result = run(
            &mut app,
            &json!({
                "notes": [{"hz": 440, "ms": 60}],
                "frames": [
                    {"at_ms": 240, "text": "LAST"}, bitmap_frame(200, &inverse),
                    {"at_ms": 100, "text": "MID"}, bitmap_frame(0, &bytes),
                ],
            }),
        )
        .unwrap();
        let state = board.0.lock().unwrap();
        assert_eq!(
            state.frames.iter().map(|frame| frame.pixels.clone()).collect::<Vec<_>>(),
            [bytes, text_pixels("MID"), inverse, text_pixels("LAST")],
            "both content forms replace the whole panel without an intermediate flush"
        );
        assert_eq!(state.frames.iter().map(|frame| frame.at_ms).collect::<Vec<_>>(), [0, 100, 200, 240]);
        assert_eq!(state.writes, [1440]);
        assert_eq!(result["frames_drawn"], 4);
        assert_eq!(result["frames_late"], 0);
        assert_eq!(result["played_ms"], 60);
    }

    #[test]
    fn a_picture_can_outlast_the_sound() {
        let (mut app, board) = fixture();
        let result = run(
            &mut app,
            &json!({
                "notes": [{"hz": 440, "ms": 60}],
                "frames": [{"at_ms": 240, "text": "OUT"}, {"at_ms": 0, "text": "IN"}],
            }),
        )
        .unwrap();
        let state = board.0.lock().unwrap();
        assert_eq!(state.frames.len(), 2);
        assert_eq!(state.frames[1].at_ms, 240);
        assert_eq!(state.frames[1].samples, 1440, "all audio ended before the last frame");
        assert_eq!(state.frames[1].pixels, text_pixels("OUT"));
        assert_eq!(result["frames_drawn"], 2);
        assert_eq!(result["frames_late"], 0);
        assert_eq!(result["max_late_ms"], 0);
        assert_eq!(result["played_ms"], 60);
        assert_eq!(result["write_ms"], 60, "waiting for the picture is not an audio write");
    }

    #[test]
    fn lateness_agrees_with_completed_virtual_flushes() {
        let (mut app, board) = fixture();
        {
            let mut state = board.0.lock().unwrap();
            state.frame_delay_ms = 7;
            state.write_delay_ms = 12;
        }
        let slots = [0, 100, 101, 200];
        let result = run(
            &mut app,
            &json!({
                "notes": [{"hz": 523, "ms": 400}],
                "frames": slots.map(|at_ms| json!({"at_ms": at_ms, "text": "T"})),
            }),
        )
        .unwrap();
        let state = board.0.lock().unwrap();
        let lateness: Vec<_> =
            state.frames.iter().zip(slots).map(|(f, slot)| f.at_ms.saturating_sub(slot)).collect();
        assert_eq!(result["frames_drawn"], state.frames.len());
        assert_eq!(result["frames_late"], lateness.iter().filter(|ms| **ms > 0).count());
        assert_eq!(result["max_late_ms"], *lateness.iter().max().unwrap());
        assert_eq!(result["frames_late"], 4, "even the slot-zero flush took seven milliseconds");
        assert_eq!(result["played_ms"], 400);
        assert_eq!(result["write_ms"], 436);
        assert_eq!(result["synth_ms"], 0);
    }

    #[test]
    fn invalid_frames_refuse_everything_before_any_write() {
        for (frame, field) in [
            (json!({"at_ms": 20, "rects": [{"x": 128, "y": 0, "w": 1, "h": 1}]}), "x"),
            (json!({"at_ms": 20, "rects": [{"x": 0, "y": 32, "w": 1, "h": 1}]}), "y"),
            (json!({"at_ms": 20, "rects": [{"x": 0, "y": 0, "w": 129, "h": 1}]}), "w"),
            (json!({"at_ms": 20, "rects": [{"x": 0, "y": 0, "w": 1, "h": 33}]}), "h"),
            (json!({"at_ms": 20, "rects": [{"x": -1, "y": 0, "w": 1, "h": 1}]}), "x"),
            (json!({"at_ms": 20, "rects": [{"x": "bad", "y": 0, "w": 1, "h": 1}]}), "x"),
            (json!({"at_ms": 20, "rects": [{"x": 0, "y": 0, "h": 1}]}), "w"),
            (json!({"at_ms": 20, "rects": [{"x": 0, "y": 0, "w": 1, "h": 1, "fill": 3}]}), "fill"),
            (json!({"at_ms": 20, "text": "X", "line": 4}), "line"),
            (json!({"at_ms": 12001, "text": "X"}), "at_ms"),
            (json!({"at_ms": -1, "text": "X"}), "at_ms"),
            (json!({"at_ms": 1.5, "text": "X"}), "at_ms"),
            (json!({"text": "X"}), "at_ms"),
            (json!({"at_ms": 20, "rects": {}}), "rects"),
            (json!({"at_ms": 20, "text": null}), "text"),
            (json!({"at_ms": 20, "bitmap_b64": null}), "bitmap_b64"),
            (json!({"at_ms": 20, "bitmap_b64": 123}), "bitmap_b64"),
            (json!({"at_ms": 20, "bitmap_b64": "!!!!"}), "bitmap_b64"),
            (json!({"at_ms": 20, "bitmap_b64": "AA=A"}), "bitmap_b64"),
            (json!({"at_ms": 20, "rects": [], "text": ""}), "drawable"),
        ] {
            let (mut app, board) = fixture();
            let error = run(
                &mut app,
                &json!({
                    "notes": [{"hz": 440, "ms": 200}],
                    "frames": [{"at_ms": 0, "text": "VALID"}, frame],
                }),
            )
            .unwrap_err();
            assert_eq!(error.code, crate::error::code::INVALID_PARAMS);
            assert!(
                error.message.contains("frames[1]") && error.message.contains(field),
                "{}",
                error.message
            );
            no_output(&board);
        }
    }

    #[test]
    fn bitmap_lengths_match_the_detected_panel_before_playback() {
        for (height, expected) in [(32, 512), (64, 1024)] {
            for actual in [0, 10, 511, 512, 513, 1023, 1024, 1025, 65_536] {
                if actual == expected {
                    continue;
                }
                let (mut app, board) = fixture();
                app.cfg.pins.insert("oled.height".into(), height);
                app.rebuild();
                board.reset();
                let error = run(
                    &mut app,
                    &json!({
                        "notes": [{"hz": 440, "ms": 200}],
                        "frames": [{"at_ms": 0, "text": "VALID"}, bitmap_frame(20, &vec![0xAA; actual])],
                    }),
                )
                .unwrap_err();
                assert_eq!(error.code, crate::error::code::INVALID_PARAMS);
                assert!(error.message.contains("frames[1]"), "{}", error.message);
                assert!(error.message.contains(&format!("{actual} decoded bytes")), "{}", error.message);
                assert!(error.message.contains(&format!("expected {expected}")), "{}", error.message);
                no_output(&board);
            }
        }
    }

    #[test]
    fn bitmap_and_vector_fields_cannot_be_mixed() {
        for (field, value) in [
            ("rects", json!([{"x": 0, "y": 0, "w": 1, "h": 1}])),
            ("rects", json!([])),
            ("rects", json!(null)),
            ("text", json!("OVERLAY")),
            ("text", json!("")),
            ("text", json!(null)),
            ("line", json!(0)),
            ("line", json!(null)),
        ] {
            let (mut app, board) = fixture();
            let mut frame = bitmap_frame(20, &[0xAA; 512]);
            frame[field] = value;
            let error = run(
                &mut app,
                &json!({
                    "notes": [{"hz": 440, "ms": 200}],
                    "frames": [{"at_ms": 0, "text": "VALID"}, frame],
                }),
            )
            .unwrap_err();
            assert_eq!(error.code, crate::error::code::INVALID_PARAMS);
            assert!(
                error.message.contains("frames[1]")
                    && error.message.contains("bitmap_b64")
                    && error.message.contains(field),
                "{}",
                error.message
            );
            no_output(&board);
        }
    }

    #[test]
    fn bitmap_budget_is_for_the_entire_request_not_the_result() {
        let (mut app, board) = fixture();
        let bitmap = bitmap_frame(0, &[0xAA; 512]);
        let accepted = MAX_MEDIA_BITMAP_BYTES / 512;
        let mut frames = vec![json!({"at_ms": 0, "text": "VALID"})];
        frames.extend(vec![bitmap.clone(); accepted + 1]);
        let error = run(&mut app, &json!({"notes": [{"hz": 440, "ms": 200}], "frames": frames})).unwrap_err();
        assert_eq!(error.code, crate::error::code::INVALID_PARAMS);
        assert!(error.message.contains("frames[65]"), "{}", error.message);
        assert!(error.message.contains("33280") && error.message.contains("32768"), "{}", error.message);
        no_output(&board);

        let args = json!({"frames": vec![bitmap; accepted]});
        assert!(args.to_string().len() > MAX_MEDIA_BITMAP_BYTES, "base64/JSON is not the decoded budget");
        let result = run(&mut app, &args).unwrap();
        assert_eq!(result["frames_drawn"], 64);
        assert_eq!(board.0.lock().unwrap().frames.len(), 64);
        assert!(result.to_string().len() < crate::agent::MAX_TOOL_RESULT_BYTES);
    }

    #[test]
    fn malformed_top_level_arguments_do_not_play() {
        for args in [
            json!({}),
            json!({"notes": null}),
            json!({"notes": {}}),
            json!({"notes": []}),
            json!({"frames": null}),
            json!({"frames": {}}),
            json!({"frames": []}),
            json!({"notes": [{"hz": 440, "ms": 100}], "frames": []}),
            json!({"notes": [{"hz": 440, "ms": 100}], "volume": -1}),
            json!({"notes": [{"hz": 440, "ms": 100}], "volume": 101}),
            json!({"notes": [{"hz": 440, "ms": 100}], "volume": null}),
            json!({"notes": [{"hz": 440, "ms": 100}], "fps": null}),
            json!({"notes": [{"hz": 440, "ms": 100}], "fps": "fast"}),
        ] {
            let (mut app, board) = fixture();
            assert_eq!(run(&mut app, &args).unwrap_err().code, crate::error::code::INVALID_PARAMS, "{args}");
            no_output(&board);
        }
    }

    #[test]
    fn media_keeps_the_exact_score_limits() {
        let (mut app, board) = fixture();
        for notes in [
            json!(vec![json!({"hz": 440, "ms": 20}); MAX_MELODY_NOTES + 1]),
            json!(vec![json!({"hz": 440, "ms": 1000}); 13]),
            json!([{"hz": 8001, "ms": 20}]),
            json!([{"hz": 440, "ms": 19}]),
            json!([{"hz": 440, "ms": 2001}]),
        ] {
            assert!(run(&mut app, &json!({"notes": notes})).is_err());
            no_output(&board);
        }
        let notes = vec![json!({"hz": 440, "ms": 2000}); 6];
        let result = run(&mut app, &json!({"notes": notes})).unwrap();
        let state = board.0.lock().unwrap();
        assert_eq!(state.board.i2s_written, 288_000);
        assert_eq!(state.pcm.len() * 2, 576_000, "exactly speaker_play's maximum mono budget");
        assert!(state.writes.iter().all(|n| *n <= 4096));
        assert_eq!(result["played_ms"], MAX_MELODY_MS);
        assert_eq!(result["frames_drawn"], 0);
    }

    #[test]
    fn oversized_frame_and_rectangle_lists_are_refused_with_counts() {
        let (mut app, board) = fixture();
        let frames = vec![json!({"at_ms": 0, "text": "T"}); MAX_MEDIA_FRAMES + 1];
        let error = run(&mut app, &json!({"notes": [{"hz": 440, "ms": 100}], "frames": frames})).unwrap_err();
        assert!(error.message.contains("361") && error.message.contains("360"), "{}", error.message);
        no_output(&board);
        let rects = vec![json!({"x": 0, "y": 0, "w": 1, "h": 1}); MAX_FRAME_RECTS + 1];
        let error = run(
            &mut app,
            &json!({"notes": [{"hz": 440, "ms": 100}], "frames": [{"at_ms": 0, "rects": rects}]}),
        )
        .unwrap_err();
        assert!(
            error.message.contains("65") && error.message.contains("64") && error.message.contains("rects"),
            "{}",
            error.message
        );
        no_output(&board);
    }

    #[test]
    fn maximum_frame_plan_fits_one_tool_result_without_truncation() {
        let (mut app, board) = fixture();
        let frames = vec![json!({"at_ms": 0, "text": "T"}); MAX_MEDIA_FRAMES];
        let result = run(&mut app, &json!({"frames": frames})).unwrap();
        assert_eq!(result["frames_drawn"], MAX_MEDIA_FRAMES);
        assert_eq!(board.0.lock().unwrap().frames.len(), MAX_MEDIA_FRAMES);
        assert_eq!(result.as_object().unwrap().len(), 8, "counts, not an unbounded frame log");
        assert!(result.to_string().len() < crate::agent::MAX_TOOL_RESULT_BYTES);
    }

    #[test]
    fn frames_only_need_no_speaker_and_notes_only_need_no_panel() {
        let (mut app, board) = fixture();
        app.cfg.board = "esp32s3-devkitc1-n16r8".into();
        app.rebuild();
        board.reset();
        assert!(!app.devices.has_cap("speaker"));
        assert!(app.is_visible("media_play"));
        let result = run(&mut app, &json!({"frames": [{"at_ms": 300, "text": "PICTURE"}]})).unwrap();
        assert_eq!(result["played_ms"], 0);
        assert_eq!(result["frames_drawn"], 1);
        assert_eq!(board.0.lock().unwrap().frames[0].at_ms, 300);

        app.cfg.board = "esp32s3-xiaozhi-bread".into();
        board.0.lock().unwrap().board.remove_at(0x3C);
        app.rebuild();
        board.reset();
        assert!(!app.devices.has_cap("oled"));
        assert!(app.is_visible("media_play"));
        let result = run(&mut app, &json!({"notes": [{"hz": 440, "ms": 100}]})).unwrap();
        assert_eq!(result["played_ms"], 100);
        assert_eq!(result["frames_drawn"], 0);

        app.cfg.board = "esp32s3-devkitc1-n16r8".into();
        app.rebuild();
        assert!(app.tools.get("media_play").is_none(), "no compound tool when neither medium exists");
    }

    #[test]
    fn rectangles_clip_and_text_shares_one_frame_flush() {
        let (mut app, board) = fixture();
        let result = run(&mut app, &json!({"frames": [{
            "at_ms": 0,
            "rects": [{"x": 126, "y": 30, "w": 8, "h": 8}, {"x": 120, "y": 8, "w": 3, "h": 3, "fill": false}],
            "text": "OVERLAY", "line": 2,
        }]})).unwrap();
        let state = board.0.lock().unwrap();
        assert_eq!(state.frames.len(), 1, "rectangles and text never flush an intermediate frame");
        let pixels = &state.frames[0].pixels;
        assert_eq!(pixels[3 * 128 + 126], 0xC0);
        assert_eq!(pixels[3 * 128 + 127], 0xC0);
        assert_eq!(pixels[128 + 120], 7);
        assert_eq!(pixels[128 + 121], 5, "outline leaves the centre clear");
        assert_eq!(pixels[128 + 122], 7);
        assert!(pixels[2 * 128..2 * 128 + 42].iter().any(|b| *b != 0), "text overlays the same frame");
        assert_eq!(result["frames_drawn"], 1);
    }

    #[test]
    fn fps_defaults_and_clamps_without_retiming_slots() {
        let (mut app, board) = fixture();
        let mut args = json!({"frames": [{"at_ms": 40, "text": "B"}, {"at_ms": 0, "text": "A"}]});
        assert_eq!(planned_fps(&mut app, &args), 25);
        for (requested, effective) in [(0, 1), (500, 30), (12, 12)] {
            args["fps"] = json!(requested);
            assert_eq!(planned_fps(&mut app, &args), effective);
            board.reset();
            let result = run(&mut app, &args).unwrap();
            assert_eq!(board.0.lock().unwrap().frames.iter().map(|f| f.at_ms).collect::<Vec<_>>(), [0, 40]);
            assert_eq!(result["frames_late"], 0);
        }
        assert_eq!(
            planned_fps(&mut app, &json!({"frames": [{"at_ms": 0, "text": "A"}, {"at_ms": 0, "text": "B"}]})),
            1
        );
    }

    #[test]
    fn media_registration_and_existing_tools_keep_their_behavior() {
        let (mut app, board) = fixture();
        let list = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        let media =
            list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "media_play").unwrap();
        assert!(media["inputSchema"]["anyOf"].is_array());
        let frame_schema = &media["inputSchema"]["properties"]["frames"]["items"];
        assert_eq!(frame_schema["properties"]["bitmap_b64"]["type"], "string");
        assert_eq!(frame_schema["oneOf"].as_array().unwrap().len(), 2);
        assert!(media["inputSchema"]["properties"]["confirm"].is_null());
        assert_eq!(media["annotations"]["readOnlyHint"], false);
        let response = app.handle_rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"media_play","arguments":{"notes":[{"hz":440,"ms":20}],"frames":[{"at_ms":0,"text":"MCP"}]}}}"#, None).unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
        assert_eq!(response["result"]["structuredContent"]["frames_drawn"], 1);
        assert_eq!(board.0.lock().unwrap().writes, [480]);

        app.call_tool(
            "display",
            &json!({"command": "oled_text", "arguments": {"text": "LEGACY", "line": 1}}),
        )
        .unwrap();
        let read = app.call_tool("display", &json!({"command": "oled_read"})).unwrap();
        assert_eq!(read["lines"][1], "LEGACY");
        board.reset();
        let result = app
            .call_tool(
                "audio",
                &json!({
                    "command": "speaker_play",
                    "arguments": {"notes": [{"hz": 440, "ms": 200}, {"hz": 523, "ms": 200}]}
                }),
            )
            .unwrap();
        assert_eq!(result["notes"], 2);
        assert_eq!(result["ms"], 400);
        assert_eq!(
            board.0.lock().unwrap().writes,
            [9600],
            "speaker_play still uses one write, not media chunks"
        );
        let help = app.call_tool("audio", &json!({"command": "help"})).unwrap();
        assert!(
            !help["commands"].as_array().unwrap().iter().any(|v| v == "media_play"),
            "the existing audio family is unchanged"
        );
    }

    #[test]
    fn disabled_actions_and_manifest_policies_are_not_bypassed() {
        let args = json!({
            "notes": [{"hz": 440, "ms": 100}],
            "frames": [{"at_ms": 0, "text": "T", "rects": [{"x": 0, "y": 8, "w": 1, "h": 1}]}],
        });
        for disabled in ["display", "oled_text", "oled_rect", "speaker_play", "audio", "media_play"] {
            let (mut app, board) = fixture();
            app.cfg.mcp.disabled_tools.push(disabled.into());
            assert_eq!(
                app.call_tool("media_play", &args).unwrap_err().code,
                crate::error::code::FORBIDDEN,
                "{disabled}"
            );
            no_output(&board);
        }
        let bitmap_args = json!({
            "notes": [{"hz": 440, "ms": 100}],
            "frames": [bitmap_frame(0, &[0xAA; 512])],
        });
        for disabled in ["display", "oled_bitmap", "speaker_play", "audio", "media_play"] {
            let (mut app, board) = fixture();
            app.cfg.mcp.disabled_tools.push(disabled.into());
            assert_eq!(
                app.call_tool("media_play", &bitmap_args).unwrap_err().code,
                crate::error::code::FORBIDDEN,
                "{disabled}"
            );
            no_output(&board);
        }
        let (mut app, board) = fixture();
        app.manifest = crate::manifest::Manifest::from_json_str(r#"{"tools":[{"name":"locked_bitmap","params":[],"exec":{"type":"driver","driver":"ssd1306","action":"oled_bitmap"},"safety":{"confirm":true}}]}"#).unwrap();
        let error = run(&mut app, &bitmap_args).unwrap_err();
        assert!(error.message.contains("locked_bitmap"));
        no_output(&board);
        app.manifest = crate::manifest::Manifest::from_json_str(r#"{"tools":[{"name":"locked_audio","params":[],"exec":{"type":"driver","driver":"speaker","action":"speaker_play"},"safety":{"confirm":true}}]}"#).unwrap();
        let error = run(&mut app, &args).unwrap_err();
        assert!(error.message.contains("locked_audio"));
        no_output(&board);
        app.manifest = Default::default();
        assert!(app
            .call_tool(
                "device_invoke",
                &json!({"device": "ssd1306", "tool": MEDIA_FRAME_TOOL, "arguments": {"text": "T"}})
            )
            .is_err());
        no_output(&board);
    }

    #[test]
    fn hardware_failures_preserve_completed_work() {
        let (mut app, board) = fixture();
        board.0.lock().unwrap().fail_write_after = Some(1);
        let error = run(&mut app, &json!({
            "notes": [{"hz": 440, "ms": 400}],
            "frames": [{"at_ms": 0, "text": "A"}, {"at_ms": 100, "text": "B"}, {"at_ms": 200, "text": "C"}],
        })).unwrap_err();
        assert!(error.message.contains("virtual amplifier stopped"));
        let progress = &error.data.unwrap()["media_play"];
        let state = board.0.lock().unwrap();
        assert_eq!(progress["ok"], false);
        assert_eq!(progress["played_ms"], 170);
        assert_eq!(progress["frames_drawn"], state.frames.len());
        assert_eq!(progress["frames_drawn"], 2);
        assert_eq!(progress["frames_late"], 1);
        assert_eq!(state.board.i2s_written, 4096);
    }
}

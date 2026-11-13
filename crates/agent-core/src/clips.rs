//! Media bytes held in RAM on purpose: clips.
//!
//! The board has megabytes of PSRAM and a few hundred kilobytes of internal SRAM, and the
//! two are not interchangeable: stacks, DMA descriptors and every allocation that must
//! survive a flash write come from internal RAM. So a clip lives in **PSRAM when the
//! platform has any**, and the store says where each one landed instead of hoping.
//!
//! A clip is deliberately dumb: bytes, a kind, and the numbers needed to play it back
//! (a sample rate for PCM, a frame rate for bitmaps). It is not a filesystem: clips are lost
//! on reboot, are not in the transcript, and are capped so a model cannot fill the pool with
//! things nobody will play.
//!
//! Why this exists at all: a tool result is capped at 64 KiB, so a 20 s recording or a
//! 30-frame animation cannot travel through the conversation. Appending it in pieces into a
//! clip and then saying "play the clip" keeps the bytes out of the transcript and puts them
//! where there is room.

use serde_json::{json, Value};

use crate::util;

/// One clip's ceiling. Two megabytes is about 43 s of 24 kHz mono PCM, or 4096 frames of
/// 128×32 (512 bytes each); both are far more than a single playback should hold.
pub const CLIP_MAX_BYTES: usize = 2 * 1024 * 1024;
/// All clips together. The measured free PSRAM on the bench is ~7.8 MB, so this leaves room
/// for the model request, TLS buffers and a workspace save while it is full.
pub const CLIPS_TOTAL_MAX: usize = 4 * 1024 * 1024;
/// How many clips may exist at once: a person juggling more than this needs names, not slots.
pub const CLIPS_MAX: usize = 6;
/// One `clip_append` payload, decoded. A tool argument travels in the request body, and the
/// base64 of this is ~65 KB — small enough to stay well inside the request budget.
pub const CLIP_APPEND_MAX: usize = 48 * 1024;
/// How much a clip grows by when an append does not fit its reserved capacity. A power of
/// two keeps the number of copies down; 8 KiB is two frames and a sixth of a second of PCM,
/// so a 60-frame animation costs four reallocations, not sixty.
pub const CLIP_GROW_BYTES: usize = 8 * 1024;
/// Sample rate the amplifier is wired for. A PCM clip carries its own rate instead of
/// assuming this one, because the recorder does **not** run at this rate.
pub const PCM_RATE_HZ: u32 = 24_000;
/// What a new clip reserves when the caller does not say: half a second of amplifier audio,
/// or one second of recording. Small enough that six idle clips cannot crowd the pool.
pub const CLIP_DEFAULT_PCM_BYTES: usize = 12_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    /// Signed 16-bit little-endian mono PCM at the clip's own `rate_hz`.
    Pcm,
    /// Packed 1-bpp frames, back to back, each the panel's framebuffer size.
    Bitmap,
}

impl ClipKind {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "pcm" | "pcm16" => Some(Self::Pcm),
            "bitmap" | "bitmaps" | "frames" => Some(Self::Bitmap),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pcm => "pcm",
            Self::Bitmap => "bitmap",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Clip {
    pub name: String,
    pub kind: ClipKind,
    pub bytes: Vec<u8>,
    /// Where the buffer came from: `psram` on a board that has it, `internal` on a host.
    pub pool: &'static str,
    /// Frames per second for a bitmap clip; meaningless for PCM.
    pub fps: u32,
    /// Samples per second for a PCM clip; meaningless for a bitmap.
    pub rate_hz: u32,
}

impl Clip {
    /// Duration of a PCM clip, from its own byte count at its own rate.
    pub fn ms(&self) -> u64 {
        match self.kind {
            ClipKind::Pcm if self.rate_hz > 0 => {
                (self.bytes.len() as u64 * 1000) / (u64::from(self.rate_hz) * 2)
            }
            // A frame is the panel's framebuffer; without the geometry the honest answer is
            // the frame count, which `info` reports.
            _ => 0,
        }
    }

    pub fn frames(&self, frame_bytes: usize) -> usize {
        if self.kind == ClipKind::Bitmap && frame_bytes > 0 {
            self.bytes.len() / frame_bytes
        } else {
            0
        }
    }
}

/// The clips this `App` holds. Not a store: nothing here is persisted.
#[derive(Debug, Default)]
pub struct Clips {
    items: Vec<Clip>,
}

#[derive(Debug, PartialEq)]
pub enum ClipError {
    Full(usize),
    TooBig {
        wanted: usize,
        limit: usize,
    },
    TotalFull {
        wanted: usize,
        used: usize,
        limit: usize,
    },
    Unknown(String),
    WrongKind {
        name: String,
        expected: ClipKind,
    },
    /// The platform refused a buffer of this size. On a board this is the PSRAM answer, not
    /// a fallback to internal RAM: the bytes would not all fit together, and pretending
    /// otherwise would spend the pool the rest of the system runs on.
    NoMemory {
        wanted: usize,
    },
}

impl ClipError {
    pub fn message(&self) -> String {
        match self {
            Self::Full(limit) => format!("there are already {limit} clips; drop one first"),
            Self::TooBig { wanted, limit } => {
                format!("that clip would be {wanted} bytes; one clip may be at most {limit}")
            }
            Self::TotalFull { wanted, used, limit } => format!(
                "clips reserve {used} of {limit} bytes; {wanted} more does not fit — drop one first"
            ),
            Self::Unknown(name) => format!("no clip `{name}`"),
            Self::WrongKind { name, expected } => {
                format!("clip `{name}` is not a {} clip", expected.as_str())
            }
            Self::NoMemory { wanted } => format!(
                "the platform refused a {wanted}-byte buffer; a clip may only use PSRAM, and there is not that much free"
            ),
        }
    }
}

impl Clips {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The pool the clips have taken: capacity, not content.
    ///
    /// A clip reserves when it is opened or grown, so counting content would let six empty
    /// clips each hold a megabyte while the budget read zero.
    pub fn total_bytes(&self) -> usize {
        self.items.iter().map(|c| c.bytes.capacity()).sum()
    }

    /// The bytes a clip holds, as opposed to the pool it has reserved.
    pub fn content_bytes(&self) -> usize {
        self.items.iter().map(|c| c.bytes.len()).sum()
    }

    pub fn get(&self, name: &str) -> Option<&Clip> {
        self.items.iter().find(|c| c.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Clip> {
        self.items.iter_mut().find(|c| c.name == name)
    }

    pub fn names(&self) -> Vec<String> {
        self.items.iter().map(|c| c.name.clone()).collect()
    }

    /// Create (or reset) a clip. The buffer is allocated by the caller, so the platform's
    /// PSRAM preference is what decides where the bytes live; the returned clip is where the
    /// caller records its rate or frame count.
    pub fn open(
        &mut self,
        name: &str,
        kind: ClipKind,
        pool: &'static str,
        capacity: Vec<u8>,
    ) -> Result<&mut Clip, ClipError> {
        if self.get(name).is_none() && self.items.len() >= CLIPS_MAX {
            return Err(ClipError::Full(CLIPS_MAX));
        }
        let reserve = capacity.capacity();
        // Replacing a clip gives its reservation back first, so reopening the largest clip
        // does not depend on how much of the budget the old one held.
        let replaced = self.get(name).map(|c| c.bytes.capacity()).unwrap_or(0);
        let used = self.total_bytes() - replaced;
        if used + reserve > CLIPS_TOTAL_MAX {
            return Err(ClipError::TotalFull { wanted: reserve, used, limit: CLIPS_TOTAL_MAX });
        }
        let clip = Clip { name: name.to_string(), kind, bytes: capacity, pool, fps: 0, rate_hz: 0 };
        let index = match self.items.iter().position(|c| c.name == name) {
            Some(index) => index,
            None => {
                self.items.push(Clip {
                    name: String::new(),
                    kind,
                    bytes: Vec::new(),
                    pool,
                    fps: 0,
                    rate_hz: 0,
                });
                self.items.len() - 1
            }
        };
        self.items[index] = clip;
        Ok(&mut self.items[index])
    }

    /// Append decoded bytes, enforcing the per-clip and total ceilings before copying.
    ///
    /// Growth goes through `alloc` rather than the ambient allocator, so every reallocation
    /// asks the platform for the same kind of memory the clip was opened with: a board
    /// answers with PSRAM or refuses. A `Vec` growing on its own would quietly take whatever
    /// `malloc` handed back, which on this board is internal RAM when it can spare it.
    pub fn append_with(
        &mut self,
        name: &str,
        data: &[u8],
        alloc: &mut dyn FnMut(usize) -> Option<(Vec<u8>, &'static str)>,
    ) -> Result<usize, ClipError> {
        let used = self.total_bytes();
        let clip = self.get_mut(name).ok_or_else(|| ClipError::Unknown(name.to_string()))?;
        let want = clip.bytes.len() + data.len();
        if want > CLIP_MAX_BYTES {
            return Err(ClipError::TooBig { wanted: want, limit: CLIP_MAX_BYTES });
        }
        // The space this clip gives back when it grows, so the check is on what it will hold
        // afterwards rather than on the old reservation plus the new bytes.
        let held = used - clip.bytes.capacity();
        let reserve = if clip.bytes.capacity() < want {
            (want.div_ceil(CLIP_GROW_BYTES) * CLIP_GROW_BYTES).min(CLIP_MAX_BYTES)
        } else {
            clip.bytes.capacity()
        };
        if held + reserve > CLIPS_TOTAL_MAX {
            return Err(ClipError::TotalFull { wanted: reserve, used: held, limit: CLIPS_TOTAL_MAX });
        }
        if clip.bytes.capacity() < want {
            let (mut fresh, pool) = alloc(reserve).ok_or(ClipError::NoMemory { wanted: reserve })?;
            // The old buffer is freed when it drops, after the copy: peak usage is the two
            // buffers, which is why the per-clip ceiling is half the pool budget.
            fresh.extend_from_slice(&clip.bytes);
            clip.bytes = fresh;
            clip.pool = pool;
        }
        clip.bytes.extend_from_slice(data);
        Ok(clip.bytes.len())
    }

    /// `append_with` on a host, where every buffer is ordinary memory.
    pub fn append(&mut self, name: &str, data: &[u8]) -> Result<usize, ClipError> {
        self.append_with(name, data, &mut |len| Some((Vec::with_capacity(len), "internal")))
    }

    pub fn drop_clip(&mut self, name: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|c| c.name != name);
        self.items.len() != before
    }

    /// The listing `/api/status.clips` and `clip_list` share.
    pub fn json(&self, frame_bytes: usize) -> Value {
        let clips: Vec<Value> = self
            .items
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "kind": c.kind.as_str(),
                    "bytes": c.bytes.len(),
                    "reserved": c.bytes.capacity(),
                    "pool": c.pool,
                    // Only meaningful for the kind it belongs to; the other stays zero rather
                    // than pretending a frame rate is a duration.
                    "ms": c.ms(),
                    "rate_hz": c.rate_hz,
                    "frames": c.frames(frame_bytes),
                    "fps": c.fps,
                })
            })
            .collect();
        json!({
            "clips": clips,
            "count": self.items.len(),
            // What the clips hold, and what they have reserved in the pool: two different
            // numbers, and the second is the one that competes with everything else.
            "total_bytes": self.content_bytes(),
            "reserved_bytes": self.total_bytes(),
            "total_max": CLIPS_TOTAL_MAX,
            "clip_max": CLIP_MAX_BYTES,
            "count_max": CLIPS_MAX,
            "append_max": CLIP_APPEND_MAX,
            "growth_bytes": CLIP_GROW_BYTES,
        })
    }
}

/// The PCM inside a RIFF/WAVE file, located by its `data` chunk rather than by a 44-byte
/// assumption: the header `wav()` writes today is 44 bytes, and a recorder that adds a
/// `LIST` chunk tomorrow would otherwise shift every sample.
pub fn pcm_of_wav(wav: &[u8]) -> &[u8] {
    if wav.len() <= 44 || &wav[..4] != b"RIFF" {
        return wav;
    }
    let mut at = 12;
    while at + 8 <= wav.len() {
        let id = &wav[at..at + 4];
        let len = u32::from_le_bytes([wav[at + 4], wav[at + 5], wav[at + 6], wav[at + 7]]) as usize;
        let body = at + 8;
        if id == b"data" {
            return &wav[body..(body + len).min(wav.len())];
        }
        at = body + len + (len & 1);
    }
    wav
}

/// Resample 16-bit LE mono PCM by linear interpolation into `frames_out` samples.
///
/// The recorder and the amplifier do not share a rate — 16 kHz in, 24 kHz out — so a
/// recording played at the amplifier's rate without this step is 1.5× too fast, which is
/// audible as a chipmunk and easy to mistake for a hardware fault. `src` should hold one
/// extra sample past `frames_out` when it can, so the last output interpolates rather than
/// repeating.
pub fn resample(src: &[u8], from_hz: u32, to_hz: u32, frames_out: usize, out: &mut Vec<i16>) {
    out.clear();
    if from_hz == 0 || to_hz == 0 || src.len() < 2 {
        return;
    }
    let frames_in = src.len() / 2;
    let at = |i: usize| util::i16_le(src, i.min(frames_in - 1) * 2) as f32;
    out.reserve(frames_out);
    if from_hz == to_hz {
        for i in 0..frames_out.min(frames_in) {
            out.push(at(i) as i16);
        }
        return;
    }
    let step = f64::from(from_hz) / f64::from(to_hz);
    for j in 0..frames_out {
        let pos = j as f64 * step;
        let i = pos.floor() as usize;
        let frac = (pos - i as f64) as f32;
        let (a, b) = (at(i), at(i + 1));
        let value = a + (b - a) * frac;
        out.push(value.clamp(-32768.0, 32767.0) as i16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_is_capped_and_the_store_is_capped() {
        let mut clips = Clips::default();
        clips.open("a", ClipKind::Pcm, "internal", Vec::new()).expect("open");
        // Per clip: the ceiling is refused before the copy, with both numbers.
        let err = clips.append("a", &vec![0u8; CLIP_MAX_BYTES + 1]).unwrap_err();
        assert!(matches!(err, ClipError::TooBig { .. }), "{err:?}");
        assert_eq!(clips.get("a").unwrap().bytes.len(), 0, "nothing was copied");
        // More clips than slots.
        for i in 1..CLIPS_MAX {
            clips.open(&format!("c{i}"), ClipKind::Pcm, "internal", Vec::new()).expect("open");
        }
        let err = clips.open("one-too-many", ClipKind::Pcm, "internal", Vec::new()).unwrap_err();
        assert!(matches!(err, ClipError::Full(_)), "{err:?}");
    }

    #[test]
    fn appending_past_the_total_refuses_without_growing() {
        let mut clips = Clips::default();
        clips.open("big", ClipKind::Pcm, "internal", vec![0; CLIP_MAX_BYTES]).expect("open");
        clips.open("big2", ClipKind::Pcm, "internal", vec![0; CLIP_MAX_BYTES]).expect("open");
        // 4 MiB is the total; two 2 MiB reservations are already there.
        clips.open("more", ClipKind::Pcm, "internal", Vec::new()).expect("open");
        let err = clips.append("more", &vec![0u8; CLIP_APPEND_MAX]).unwrap_err();
        assert!(matches!(err, ClipError::TotalFull { .. }), "{err:?}");
        assert_eq!(clips.total_bytes(), CLIPS_TOTAL_MAX);
    }

    #[test]
    fn a_listing_says_where_the_bytes_live_and_what_kind_they_are() {
        let mut clips = Clips::default();
        clips.open("speech", ClipKind::Pcm, "psram", vec![0; 48_000]).expect("open").rate_hz = PCM_RATE_HZ;
        let anim = clips.open("anim", ClipKind::Bitmap, "psram", vec![0; 1024]).expect("open");
        anim.fps = 12;
        let json = clips.json(512);
        assert_eq!(json["count"], 2);
        assert_eq!(json["clips"][0]["pool"], "psram", "the listing has to say where they are");
        assert_eq!(json["clips"][0]["ms"], 1000, "48 000 bytes is one second of 24 kHz mono");
        assert_eq!(json["clips"][1]["frames"], 2, "1024 bytes is two 128x32 frames");
        assert_eq!(json["clips"][1]["fps"], 12);
        assert_eq!(json["total_bytes"], 49_024);
    }

    #[test]
    fn dropping_is_by_name_and_says_whether_it_existed() {
        let mut clips = Clips::default();
        clips.open("a", ClipKind::Pcm, "internal", Vec::new()).expect("open");
        assert!(clips.drop_clip("a"));
        assert!(!clips.drop_clip("a"));
        assert!(clips.is_empty());
    }

    #[test]
    fn growth_asks_the_platform_and_a_refusal_is_not_a_fallback() {
        let mut clips = Clips::default();
        clips.open("a", ClipKind::Pcm, "psram", Vec::new()).expect("open");
        // Every reallocation goes through the caller's allocator, which is what keeps a
        // clip in PSRAM: the ambient `malloc` would answer from internal RAM when it can.
        let asked = std::cell::RefCell::new(Vec::new());
        let mut alloc = |len: usize| {
            asked.borrow_mut().push(len);
            Some((Vec::with_capacity(len), "psram"))
        };
        clips.append_with("a", &vec![1u8; 1000], &mut alloc).expect("append");
        assert_eq!(*asked.borrow(), vec![CLIP_GROW_BYTES], "one 8 KiB reservation, not 1000 bytes");
        assert_eq!(clips.total_bytes(), CLIP_GROW_BYTES, "the pool holds the reservation");
        assert_eq!(clips.content_bytes(), 1000, "the clip holds 1000 bytes of audio");
        // Filling the reservation asks for nothing more.
        clips.append_with("a", &vec![1u8; CLIP_GROW_BYTES - 1000], &mut alloc).expect("append");
        assert_eq!(asked.borrow().len(), 1, "a write inside the reservation does not reallocate");
        // Past it, the next reservation is asked for and the old bytes survive the copy.
        clips.append_with("a", &vec![2u8; 10], &mut alloc).expect("append");
        assert_eq!(*asked.borrow(), vec![CLIP_GROW_BYTES, CLIP_GROW_BYTES * 2]);
        assert_eq!(clips.get("a").unwrap().bytes.len(), CLIP_GROW_BYTES + 10);
        assert_eq!(clips.get("a").unwrap().bytes[0], 1, "the copy kept the old content");
        assert_eq!(clips.get("a").unwrap().pool, "psram", "the clip stays where it was");
        // A platform that refuses says so instead of quietly moving the bytes.
        let mut refuse = |_: usize| None;
        let err = clips.append_with("a", &vec![3u8; CLIP_GROW_BYTES * 2], &mut refuse).unwrap_err();
        assert_eq!(err, ClipError::NoMemory { wanted: CLIP_GROW_BYTES * 4 });
        assert_eq!(clips.content_bytes(), CLIP_GROW_BYTES + 10, "nothing was appended");
    }

    #[test]
    fn a_recorded_wav_is_stored_as_samples_not_as_a_container() {
        let body: Vec<u8> = [1i16, 2, 3, 4].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(body.len() as u32 + 36).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 16]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wav.extend_from_slice(&body);
        assert_eq!(pcm_of_wav(&wav), &body[..], "the data chunk, not the 44-byte header");
        // A recording that starts with something else is returned whole rather than cut.
        assert_eq!(pcm_of_wav(&body), &body[..]);
    }

    #[test]
    fn resampling_changes_the_length_and_keeps_the_shape() {
        // A ramp at 16 kHz, played at 24 kHz, has to become 1.5× as many samples with the
        // same slope — that is the difference between a recording and a chipmunk.
        let ramp: Vec<u8> = (0..8i16).flat_map(|s| (s * 1000).to_le_bytes()).collect();
        let mut out = Vec::new();
        resample(&ramp, 16_000, 24_000, 12, &mut out);
        assert_eq!(out.len(), 12, "8 frames in, 12 out");
        assert_eq!(out[0], 0);
        assert_eq!(out[3], 2000, "every third output lands on an input sample");
        assert_eq!(out[1], 666, "and the samples between them are interpolated");
        assert_eq!(out[11], 7000, "and the ramp still ends where it should");
        // Resampling to the same rate is a copy, not an interpolation.
        resample(&ramp, 24_000, 24_000, 8, &mut out);
        assert_eq!(out, vec![0, 1000, 2000, 3000, 4000, 5000, 6000, 7000]);
    }
}

//! The background audio player.
//!
//! Playback used to be a blocking call: `speaker_play` synthesized the whole score and
//! wrote every 4 KiB chunk before it returned, so a 12 s tune owned the thread that also
//! answers the portal and draws the panel. That is what made audio stutter whenever
//! something else used the board, and it is why a script could only do one thing at a time.
//!
//! This module owns the I2S transmit channel instead: a task takes a job off a queue and
//! writes it at its own pace, and the caller is free the moment its buffer is accepted.
//!
//! Two rules keep it from becoming the deadlock this board has already paid for twice:
//!
//! * the player **never takes the App lock** — it touches the channel, this module's queue
//!   and its own state, nothing else;
//! * the handoff is one small mutex, and the only lock this module holds while writing is
//!   the channel itself, which the blocking path also takes (so two writers cannot
//!   interleave into one DMA stream).
//!
//! Video is deliberately not here. The OLED is written through `i2c_write` on a bus shared
//! with other chips, so a task drawing frames would need a second owner of that bus. What
//! this buys instead is the property that matters: audio no longer waits for the panel, and
//! a script can draw while a tune plays.

use esp32_agent_core::error::Result;
use esp32_agent_core::hardware::MediaState;
use esp_idf_svc::sys;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Frames per write. The DMA ring holds 400 stereo frames (16.7 ms at 24 kHz), so a chunk
/// this size gives the player something to do while the next one is prepared without
/// asking the driver to block for long.
const CHUNK: usize = 4_096;

/// How long one `i2s_channel_write` may block. Longer than a chunk takes to drain, so a
/// healthy write never trips it.
const WRITE_TIMEOUT_MS: u32 = 1_000;

struct Job {
    id: u64,
    samples: Vec<i16>,
    rate_hz: u32,
    /// Frames already rendered to page bytes, in deadline order.
    pictures: Vec<(u64, Vec<u8>)>,
    panel: Option<std::sync::Arc<Panel>>,
}

/// The OLED, as the player sees it: its own I2C device handle and nothing else.
///
/// Sharing the App lock is what made asynchronous video impossible: the panel was reachable
/// only through `EspHw`, which lives inside `Mutex<App>`, so a player that wanted to draw
/// would have had to wait for whatever tool call was running. This type is the separation —
/// one device handle plus the bus mutex that the blocking path also takes, so two writers
/// cannot interleave their transfers, and neither of them needs App.
pub struct Panel {
    dev: usize,
    bus: Mutex<()>,
}

impl Panel {
    pub fn new(dev: usize) -> Self {
        Self { dev, bus: Mutex::new(()) }
    }

    /// Write one whole frame buffer to the glass, page by page.
    ///
    /// The sequence is the driver's `flush`: for each page, the page/column command, then
    /// the 128 bytes of that page behind the data control byte. It is duplicated here on
    /// purpose — the driver cannot be reached from this task without the App lock, and a
    /// one-line "call the driver" would be the very coupling this exists to avoid.
    fn blit(&self, page: &[u8]) -> bool {
        let pages = page.len() / 128;
        if pages == 0 {
            return false;
        }
        let _guard = self.bus.lock().unwrap_or_else(|p| p.into_inner());
        let dev = self.dev as sys::i2c_master_dev_handle_t;
        for index in 0..pages {
            let command = [0x00u8, 0xB0 | index as u8, 0x00, 0x10];
            if unsafe { sys::i2c_master_transmit(dev, command.as_ptr(), command.len(), 50) } != 0 {
                return false;
            }
            let mut chunk = Vec::with_capacity(129);
            chunk.push(0x40);
            chunk.extend_from_slice(&page[index * 128..(index + 1) * 128]);
            if unsafe { sys::i2c_master_transmit(dev, chunk.as_ptr(), chunk.len(), 50) } != 0 {
                return false;
            }
        }
        true
    }
}

pub struct Player {
    /// When the job that is playing now started. `remaining_ms` is derived from this on
    /// every read instead of being left at whatever the last chunk published: a published
    /// number is stale the moment it is written, and a stale "259 ms left" on a 4 s tune is
    /// the kind of reading this board keeps having to apologise for.
    started: Mutex<Option<std::time::Instant>>,
    /// The channel, as the driver's opaque pointer. Held as a number so the queue and this
    /// field can live in one struct without `unsafe impl Send` on this module.
    tx: Mutex<usize>,
    queue: Mutex<VecDeque<Job>>,
    state: Mutex<MediaState>,
    /// Set by `cancel`; the player checks it between chunks so a stop lands within one.
    stop: AtomicBool,
    next_id: AtomicU32,
    jobs_done: AtomicU32,
}

impl Player {
    pub fn new() -> Arc<Self> {
        // A fresh player has nothing playing, so nothing has started either.
        let started = Mutex::new(None);
        Arc::new(Self {
            started,
            tx: Mutex::new(0),
            queue: Mutex::new(VecDeque::new()),
            state: Mutex::new(MediaState::default()),
            stop: AtomicBool::new(false),
            next_id: AtomicU32::new(0),
            jobs_done: AtomicU32::new(0),
        })
    }

    /// The I2S channel came up (or came back up): remember it for the player.
    pub fn set_channel(&self, tx: sys::i2s_chan_handle_t) {
        if let Ok(mut slot) = self.tx.lock() {
            *slot = tx as usize;
        }
    }

    fn channel(&self) -> sys::i2s_chan_handle_t {
        match self.tx.lock() {
            Ok(slot) => *slot as sys::i2s_chan_handle_t,
            Err(p) => *p.into_inner() as sys::i2s_chan_handle_t,
        }
    }

    /// Accept an audio/video job and answer immediately.
    pub fn submit_av(
        &self,
        samples: &[i16],
        rate_hz: u32,
        pictures: &[(u64, Vec<u8>)],
        panel: Option<Arc<Panel>>,
    ) -> Result<u64> {
        self.push_job(samples, rate_hz, pictures.to_vec(), panel)
    }

    /// Accept a buffer and answer immediately.
    pub fn submit(&self, samples: &[i16], rate_hz: u32) -> Result<u64> {
        self.push_job(samples, rate_hz, Vec::new(), None)
    }

    fn push_job(
        &self,
        samples: &[i16],
        rate_hz: u32,
        pictures: Vec<(u64, Vec<u8>)>,
        panel: Option<Arc<Panel>>,
    ) -> Result<u64> {
        if samples.is_empty() {
            return Err(esp32_agent_core::error::McpError::invalid_params(
                "nothing to play: the buffer is empty",
            ));
        }
        if self.channel().is_null() {
            return Err(esp32_agent_core::error::McpError::unsupported("no I2S speaker on this board"));
        }
        let id = u64::from(self.next_id.fetch_add(1, Ordering::Relaxed)) + 1;
        let duration_ms = (samples.len() as u64).saturating_mul(1000) / u64::from(rate_hz.max(1));
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            // Bounded: a queue that grows without limit is RAM a script can take away from
            // everything else, and nobody wants six tunes deep in the future.
            if queue.len() >= 4 {
                return Err(esp32_agent_core::error::McpError::invalid_params(
                    "the player already has four jobs queued; wait or cancel",
                ));
            }
            queue.push_back(Job { id, samples: samples.to_vec(), rate_hz, pictures, panel });
            if let Ok(mut state) = self.state.lock() {
                state.queued = queue.len() as u64 - u64::from(state.playing);
                // Only an idle player takes the new length: a queued job must not rewrite
                // the numbers belonging to the job that is playing right now, which is what
                // made the remaining time jump when a second tune was queued.
                if !state.playing {
                    state.duration_ms = duration_ms;
                    state.remaining_ms = duration_ms;
                }
            }
        }
        self.stop.store(false, Ordering::Relaxed);
        Ok(id)
    }

    /// How many jobs are still waiting behind the one that is playing.
    fn pending(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn state(&self) -> MediaState {
        let mut state = match self.state.lock() {
            Ok(state) => state.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if state.playing {
            if let Some(started) = *self.started.lock().unwrap_or_else(|p| p.into_inner()) {
                let elapsed = started.elapsed().as_millis() as u64;
                state.remaining_ms = state.duration_ms.saturating_sub(elapsed);
            }
        }
        state
    }

    pub fn cancel(&self) -> bool {
        let playing = self.state().playing;
        *self.started.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.stop.store(true, Ordering::Relaxed);
        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let queued = queue.len();
        queue.clear();
        if let Ok(mut state) = self.state.lock() {
            state.queued = 0;
            state.playing = false;
            state.remaining_ms = 0;
            state.cancelled = true;
        }
        playing || queued > 0
    }

    fn publish(&self, update: impl FnOnce(&mut MediaState)) {
        if let Ok(mut state) = self.state.lock() {
            update(&mut state);
        }
    }

    fn take(&self) -> Option<Job> {
        self.queue.lock().ok().and_then(|mut queue| queue.pop_front())
    }
}

/// Write one buffer as stereo frames, keeping going until every sample is out.
///
/// The driver returns as soon as the DMA has taken what it can, so one call can be short;
/// dropping the rest would be a gap in the music rather than "normal under load".
pub fn write_chunk(tx: sys::i2s_chan_handle_t, samples: &[i16]) -> Result<()> {
    let mut frames: Vec<i16> = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        frames.push(*s);
        frames.push(*s);
    }
    let total = frames.len();
    let mut done = 0usize;
    while done < total {
        let mut written: usize = 0;
        let rc = unsafe {
            sys::i2s_channel_write(
                tx,
                frames[done..].as_ptr() as *const core::ffi::c_void,
                (total - done) * std::mem::size_of::<i16>(),
                &mut written,
                WRITE_TIMEOUT_MS,
            )
        };
        if written == 0 || (rc != 0 && rc != sys::ESP_ERR_TIMEOUT) {
            return Err(esp32_agent_core::error::McpError::hardware(format!(
                "i2s write returned {rc} after {done} of {total} samples"
            )));
        }
        done += written / std::mem::size_of::<i16>();
    }
    Ok(())
}

static STARTED: OnceLock<()> = OnceLock::new();

/// Start the player task once. Called after the I2S channel exists; a second call is a no-op.
pub fn start(player: Arc<Player>) {
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("media".into())
        .stack_size(6 * 1024)
        .spawn(move || loop {
            let Some(job) = player.take() else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            };
            let tx = player.channel();
            let total_chunks = ((job.samples.len() + CHUNK - 1) / CHUNK).max(1) as u64;
            player.publish(|state| {
                state.job = job.id;
                state.playing = true;
                state.cancelled = false;
                state.rate_hz = job.rate_hz;
                state.duration_ms =
                    (job.samples.len() as u64).saturating_mul(1000) / u64::from(job.rate_hz.max(1));
                state.remaining_ms = state.duration_ms;
                state.chunks = 0;
            });
            let started = std::time::Instant::now();
            *player.started.lock().unwrap_or_else(|p| p.into_inner()) = Some(started);
            let mut completed = true;
            let total_frames = job.pictures.len() as u64;
            let mut drawn = 0u64;
            let mut worst_late = 0u64;
            player.publish(|state| {
                state.frames_total = total_frames;
                state.frames_done = 0;
                state.max_late_ms = 0;
            });
            // Write the audio, splitting a chunk at the next frame deadline. Writing a
            // whole 4096-sample chunk (170 ms at 24 kHz) and *then* looking at the clock is
            // what left a picture 162 ms late on the bench: by the time the write returned,
            // the deadline had already gone past. Splitting costs one extra write per frame
            // and puts the picture where the sound is.
            let per_ms = (job.rate_hz / 1000).max(1) as usize;
            let mut written = 0usize;
            let mut index = 0u64;
            while written < job.samples.len() {
                if player.stop.load(Ordering::Relaxed) {
                    completed = false;
                    break;
                }
                // Frames due by this point in the stream, checked *before* the write: the
                // deadline is the audio position, not the wall clock. `write_chunk` blocks
                // until the DMA ring has room, so wall time ran ahead of what had been sent
                // and every picture landed about one buffer late (169 ms on the bench).
                // `written` is the position after this write, so a frame is put on the glass
                // as the samples it belongs to enter the ring — early by the ring depth
                // (about 42 ms), which is inside what an eye tolerates, instead of late.
                let stream_ms = (written as u64).saturating_mul(1000) / u64::from(job.rate_hz.max(1));
                while let Some((at_ms, page)) = job.pictures.get(drawn as usize) {
                    if *at_ms > stream_ms {
                        break;
                    }
                    let late = stream_ms.saturating_sub(*at_ms);
                    if let Some(panel) = job.panel.as_ref() {
                        if panel.blit(page) {
                            worst_late = worst_late.max(late);
                        }
                    }
                    drawn += 1;
                    player.publish(|state| {
                        state.frames_done = drawn;
                        state.max_late_ms = worst_late;
                    });
                }
                let mut take = CHUNK.min(job.samples.len() - written);
                if let Some((at_ms, _)) = job.pictures.get(drawn as usize) {
                    let due = (*at_ms as usize).saturating_mul(per_ms);
                    if due > written && due - written < take {
                        take = due - written;
                    }
                }
                let piece = &job.samples[written..written + take];
                if let Err(e) = write_chunk(tx, piece) {
                    log::warn!("media: playback of job {} stopped: {}", job.id, e.message);
                    completed = false;
                    break;
                }
                written += take;
                index += 1;
                let done_ms = (started.elapsed().as_millis() as u64)
                    .min((job.samples.len() as u64).saturating_mul(1000) / u64::from(job.rate_hz.max(1)));
                player.publish(|state| {
                    state.chunks = index;
                    state.remaining_ms = state.duration_ms.saturating_sub(done_ms);
                });
            }
            *player.started.lock().unwrap_or_else(|p| p.into_inner()) = None;
            if completed {
                let done = u64::from(player.jobs_done.fetch_add(1, Ordering::Relaxed)) + 1;
                let waiting = player.pending() as u64;
                player.publish(|state| {
                    state.playing = false;
                    state.job = 0;
                    state.chunks = total_chunks;
                    state.remaining_ms = 0;
                    state.jobs_done = done;
                    // The queue is the authority: a job that finished leaves whatever is
                    // still waiting, and a submit that was counted before the player picked
                    // the job up must not survive it.
                    state.queued = waiting;
                });
            }
        })
        .expect("the media task could not be started");
}

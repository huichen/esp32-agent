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
}

pub struct Player {
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
        Arc::new(Self {
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

    /// Accept a buffer and answer immediately.
    pub fn submit(&self, samples: &[i16], rate_hz: u32) -> Result<u64> {
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
            queue.push_back(Job { id, samples: samples.to_vec(), rate_hz });
            if let Ok(mut state) = self.state.lock() {
                state.queued = queue.len() as u64 - u64::from(state.playing);
                state.duration_ms = duration_ms;
            }
        }
        self.stop.store(false, Ordering::Relaxed);
        Ok(id)
    }

    pub fn state(&self) -> MediaState {
        match self.state.lock() {
            Ok(state) => state.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    pub fn cancel(&self) -> bool {
        let playing = self.state().playing;
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
            let mut completed = true;
            for (index, piece) in job.samples.chunks(CHUNK).enumerate() {
                if player.stop.load(Ordering::Relaxed) {
                    completed = false;
                    break;
                }
                if let Err(e) = write_chunk(tx, piece) {
                    log::warn!("media: playback of job {} stopped: {}", job.id, e.message);
                    completed = false;
                    break;
                }
                let done_ms = (started.elapsed().as_millis() as u64)
                    .min((job.samples.len() as u64).saturating_mul(1000) / u64::from(job.rate_hz.max(1)));
                player.publish(|state| {
                    state.chunks = index as u64 + 1;
                    state.remaining_ms = state.duration_ms.saturating_sub(done_ms);
                });
            }
            if completed {
                let done = u64::from(player.jobs_done.fetch_add(1, Ordering::Relaxed)) + 1;
                player.publish(|state| {
                    state.playing = false;
                    state.job = 0;
                    state.chunks = total_chunks;
                    state.remaining_ms = 0;
                    state.jobs_done = done;
                });
            }
        })
        .expect("the media task could not be started");
}

//! The one task that writes NVS.
//!
//! Every durable change the `App` makes — config, manifest, schedule, file workspace,
//! transcript — is queued in `esp32_agent_core::store::StoreQueue` and performed here.
//! That indirection is the fix for a reboot loop, not tidiness:
//!
//! * An NVS write is an `spi_flash` operation, and `spi_flash` disables both caches (the
//!   flash one and the PSRAM one) for the length of the write, because the code that runs
//!   in that window must not need either.
//! * For that reason the stack the write runs on has to be in internal RAM, and ESP-IDF
//!   asserts exactly that: `assert(esp_task_stack_is_sane_cache_disabled())` in
//!   `spi_flash_disable_interrupts_caches_and_other_cpu`.
//! * The agent thread was *given* a PSRAM stack on purpose (`runner.rs`, to survive a boot
//!   where 16 KB of contiguous internal RAM was gone). Measured on this bench (2026-11-12):
//!   `mcp-agent` ran with an `sp` of `0x3c2e…`, and the first scheduled task that came due
//!   killed the board — `nvs_erase_key` ← `NvsStore::set` ← `App::persist_sched` ← the agent
//!   thread — on that assertion, roughly every 8 s, because the task was still due after
//!   every reboot. `runner.rs` now asks for internal RAM first and only falls back to PSRAM.
//!
//! So this task is not left to a default: it is spawned with
//! `esp_pthread_set_cfg(stack_alloc_caps = MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT)`, which
//! tells the pthread layer to take the stack from internal RAM, and then it *checks* that
//! its stack really is inside the DRAM window before it writes anything. If the check
//! fails it does not write at all and says so — in the log and in `/api/status.store`.
//!
//! It has to be a pthread rather than a plain FreeRTOS task: the queue and the log are
//! behind `std::sync::Mutex`, and ESP-IDF's `pthread_self()` asserts when the calling task
//! was never registered as a pthread (`Failed to find current thread ID!` in
//! `components/pthread/pthread.c`). A `xTaskCreateStatic` task here would assert on its
//! first lock instead of writing.

use std::ptr;
use std::sync::Arc;

use esp32_agent_core::app::{App, Shared};
use esp32_agent_core::logbuf::LogBuf;
use esp_idf_svc::sys;

/// Enough for the Rust thread entry, `NvsStore::set` and the NVS/flash frames under it:
/// the values themselves are built on the heap. The head-room left after the first write is
/// logged, so "enough" is a reading rather than a hope.
const STACK_BYTES: usize = 6 * 1024;

/// Below the agent thread and the HTTP server (both at the default pthread priority 5) and
/// above the idle task: a write is worth a millisecond of latency, not a preemption.
const PRIORITY: usize = 2;

/// How often the queue is drained. This is the window in which a change is durable in RAM
/// only, which is why the reboot path flushes synchronously first ([`flush_now`]).
const PERIOD_MS: u32 = 100;

/// `tskNO_AFFINITY` from `freertos/task.h`, which is a macro and therefore not in the
/// bindings. Unpinned on purpose: whichever core is idle should do the write.
const NO_AFFINITY: std::os::raw::c_int = 0x7FFF_FFFF;

/// The task's name, also what the log lines and `/api/status.store` say.
const NAME: &str = "nvs-writer";

/// Start the writer. Called once, before anything can queue a write.
pub fn start(shared: Shared, log: Arc<LogBuf>) -> Result<(), String> {
    // `pthread_create` reads this config from the *creating* thread, so it applies to every
    // thread this one spawns while it is set — the agent thread included, and that one must
    // keep its 16 KB and its priority 5. Save what is there and put it back the moment the
    // writer exists.
    let previous = current_pthread_config();

    // The one setting that matters. `inherit_cfg = false` on purpose: the writer's own
    // children (it has none) must not be born on a 6 KB internal stack.
    let cfg = sys::esp_pthread_cfg_t {
        thread_name: b"nvs-writer\0".as_ptr() as *const std::os::raw::c_char,
        stack_size: STACK_BYTES,
        prio: PRIORITY,
        inherit_cfg: false,
        pin_to_core: NO_AFFINITY,
        stack_alloc_caps: (sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT) as u32,
    };
    let rc = unsafe { sys::esp_pthread_set_cfg(&cfg) };
    if rc != sys::ESP_OK {
        let why = format!(
            "esp_pthread_set_cfg returned {rc}; the writer would get any stack, so it was not started"
        );
        with_app_mut(&shared, |app| app.attach_store_writer(NAME, false));
        log.error("store", why.clone());
        return Err(why);
    }

    let spawned = std::thread::Builder::new().name(NAME.to_string()).stack_size(STACK_BYTES).spawn({
        let shared = shared.clone();
        let log = log.clone();
        move || run(shared, log)
    });
    // Whatever happened, the next thread `main` spawns is not this one.
    let _ = unsafe { sys::esp_pthread_set_cfg(&previous) };

    match spawned {
        Ok(_) => Ok(()),
        Err(e) => {
            with_app_mut(&shared, |app| app.attach_store_writer(NAME, false));
            let why = format!("could not start the store writer thread: {e}");
            log.error("store", why.clone());
            Err(why)
        }
    }
}

/// The creating thread's pthread config, or the defaults when nothing has set one — which is
/// the same thing in effect.
fn current_pthread_config() -> sys::esp_pthread_cfg_t {
    let mut cfg = unsafe { sys::esp_pthread_get_default_config() };
    let _ = unsafe { sys::esp_pthread_get_cfg(&mut cfg) };
    cfg
}

/// Perform queued writes now, from whichever task is running.
///
/// Used by the reboot path, where waiting for the writer's next 100 ms tick would be a coin
/// flip: `esp_restart` does not wait for it. The check is the same one the writer makes —
/// this is a `spi_flash` window, so the stack has to be in internal RAM — and an unsafe
/// answer refuses instead of rebooting the board.
pub fn flush_now(shared: &Shared, log: &LogBuf, who: &str) -> bool {
    if !on_internal_stack() {
        log.warn(
            "store",
            format!(
                "`{who}` is not on an internal-RAM stack, so it did not flush; the queued writes stay queued"
            ),
        );
        return false;
    }
    with_app(shared, |app| app.flush_store(who));
    true
}

/// Is the calling task's stack in internal RAM?
///
/// `pxTaskGetStackStart(NULL)` is the current task's lowest stack address, and the DRAM
/// window comes from the SoC headers, so this cannot drift from what IDF's own
/// `esp_ptr_in_dram` decides. Shared with `runner.rs`, which reports it for the agent thread.
pub(crate) fn on_internal_stack() -> bool {
    let base = unsafe { sys::pxTaskGetStackStart(ptr::null_mut()) } as usize;
    (sys::SOC_DRAM_LOW as usize..sys::SOC_DRAM_HIGH as usize).contains(&base)
}

/// Run `f` under the app lock, tolerating a poisoned one: a thread that panicked mid-update
/// must not stop the writes from ever happening again.
fn with_app<R>(shared: &Shared, f: impl FnOnce(&App) -> R) -> R {
    match shared.lock() {
        Ok(guard) => f(&guard),
        Err(poisoned) => f(&poisoned.into_inner()),
    }
}

fn with_app_mut<R>(shared: &Shared, f: impl FnOnce(&mut App) -> R) -> R {
    match shared.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

fn run(shared: Shared, log: Arc<LogBuf>) {
    // The one check that makes this task worth having, and it runs on the stack that does the
    // writing rather than on the stack of whoever created the task.
    let safe = on_internal_stack();
    with_app_mut(&shared, |app| app.attach_store_writer(NAME, safe));
    if safe {
        let base = unsafe { sys::pxTaskGetStackStart(ptr::null_mut()) } as usize;
        log.info(
            "store",
            format!(
                "{NAME} up: {STACK_BYTES} byte stack at 0x{base:08x} (internal RAM), drains every {PERIOD_MS} ms"
            ),
        );
    } else {
        log.error(
            "store",
            "the writer thread is running on a stack outside internal RAM; it will not write NVS",
        );
        // Deliberately idle instead of writing: a broken stack is a report, not a reboot.
        loop {
            unsafe { sys::vTaskDelay(1_000) };
        }
    }

    let mut reported = false;
    loop {
        unsafe { sys::vTaskDelay(PERIOD_MS) };
        let (written_now, total) = with_app(&shared, |app| {
            let report = app.flush_store(NAME);
            (report.written, app.store_queue().written())
        });
        // One line after the first real write, saying how much of the stack it took: the
        // number a later change to this file has to beat is a measurement, not a guess.
        if written_now > 0 && !reported {
            reported = true;
            let head_room = unsafe { sys::uxTaskGetStackHighWaterMark(ptr::null_mut()) } as usize;
            log.info(
                "store",
                format!(
                    "{NAME} wrote {written_now} value(s); {head_room} of {STACK_BYTES} bytes of stack left ({total} write(s) since boot)"
                ),
            );
        }
    }
}

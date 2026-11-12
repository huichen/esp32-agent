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
//! * The agent thread does *not* always get an internal stack. Measured on this bench
//!   (2026-11-12): `mcp-agent` ran with an `sp` of `0x3c2e…`, i.e. in PSRAM, and the first
//!   scheduled task that came due killed the board — `nvs_erase_key` ← `NvsStore::set` ←
//!   `App::persist_sched` ← the agent thread — on that assertion, roughly every 8 s,
//!   because the task was still due after every reboot.
//!
//! So the writer here leaves nothing to chance: its stack is a `static` buffer in `.bss`
//! (internal DRAM by construction, never the heap), the task is created with
//! `xTaskCreateStaticPinnedToCore`, and it checks at start-up that the stack it is actually
//! running on is inside the DRAM window. If that check fails it does not write at all and
//! says so — in the log and in `/api/status.store`.

use std::os::raw::c_char;
use std::os::raw::c_void;
use std::ptr;
use std::sync::Arc;

use esp32_agent_core::app::{App, Shared};
use esp32_agent_core::logbuf::LogBuf;
use esp_idf_svc::sys;

/// Enough for `NvsStore::set` and the NVS/flash frames under it: the values themselves are
/// built on the heap, and what runs on this stack is `nvs_set_str`'s own frame plus
/// `esp_flash_write`'s chunking. Small on purpose — internal DRAM is the scarcest thing on
/// this board — and the head-room left after the first write is logged, so "enough" is a
/// reading rather than a hope.
const STACK_BYTES: usize = 4 * 1024;

/// Below the agent thread and the HTTP server (both at the default pthread priority 5) and
/// above the idle task: a write is worth a millisecond of latency, not a preemption.
const PRIORITY: u32 = 2;

/// How often the queue is drained. This is the window in which a change is durable in RAM
/// only, which is why the reboot path flushes synchronously first ([`flush_now`]).
const PERIOD_TICKS: u32 = 100;

/// `tskNO_AFFINITY` from `freertos/task.h`, which is a macro and therefore not in the
/// bindings. Unpinned on purpose: whichever core is idle should do the write.
const NO_AFFINITY: sys::BaseType_t = 0x7FFF_FFFF;

/// The task's stack: a `static`, so it is in the image's `.bss` and therefore in internal
/// RAM. Aligned to 16, which is what the Xtensa port wants of a task stack.
#[repr(align(16))]
struct Aligned<T>(T);

static mut STACK: Aligned<[u8; STACK_BYTES]> = Aligned([0; STACK_BYTES]);

/// FreeRTOS keeps the task control block here, for the same reason: no heap, no PSRAM.
static mut TCB: std::mem::MaybeUninit<sys::StaticTask_t> = std::mem::MaybeUninit::uninit();

/// The writers' task name, also what the log lines and `/api/status.store` say.
const NAME: &str = "nvs-writer";

/// What the task needs from the rest of the firmware. Boxed and handed over as the task's
/// parameter, which is the only way to give a C task function any state.
struct Writer {
    shared: Shared,
    log: Arc<LogBuf>,
}

/// The buffer's address.
///
/// `addr_of!`/`addr_of_mut!` and never `&STACK`: a reference to a `static mut` is what
/// edition 2024 turns into an error, and a stack buffer must not have one anyway — FreeRTOS
/// writes through it behind the language's back, so the only sound handle on it is a raw
/// pointer. These three accessors are the whole of the static's use.
fn stack_base() -> usize {
    // `addr_of!` is still an access of a `static mut`, so it is unsafe; what it is not is a
    // reference, which is the part that would be undefined behaviour here.
    unsafe { core::ptr::addr_of!(STACK.0) as *const u8 as usize }
}

fn stack_buffer() -> *mut u8 {
    unsafe { core::ptr::addr_of_mut!(STACK.0) as *mut u8 }
}

fn tcb_buffer() -> *mut sys::StaticTask_t {
    core::ptr::addr_of_mut!(TCB) as *mut sys::StaticTask_t
}

/// Start the writer. Called once, before anything can queue a write.
pub fn start(shared: Shared, log: Arc<LogBuf>) -> Result<(), String> {
    // The buffer is a `static`, so this is the linker's answer and not an assumption. A
    // stack outside DRAM would make every write that assertion, so refuse to start at all
    // rather than start a task that reboots the board the first time it is used.
    let base = stack_base();
    if !in_internal_ram(base) {
        let why = format!(
            "the writer's static stack is outside internal RAM (0x{base:08x}); refusing to start a task that would assert on its first write"
        );
        with_app_mut(&shared, |app| app.attach_store_writer(NAME, false));
        log.error("store", why.clone());
        return Err(why);
    }

    let params = Box::into_raw(Box::new(Writer { shared: shared.clone(), log: log.clone() }));
    let handle = unsafe {
        sys::xTaskCreateStaticPinnedToCore(
            Some(task_entry),
            b"nvs-writer\0".as_ptr() as *const c_char,
            STACK_BYTES as u32,
            params as *mut c_void,
            PRIORITY,
            stack_buffer(),
            tcb_buffer(),
            NO_AFFINITY,
        )
    };
    if handle.is_null() {
        // Give the box back: the task never started, so nothing else will free it.
        unsafe { drop(Box::from_raw(params)) };
        with_app_mut(&shared, |app| app.attach_store_writer(NAME, false));
        let why = "xTaskCreateStaticPinnedToCore returned no handle".to_string();
        log.error("store", why.clone());
        return Err(why);
    }
    Ok(())
}

/// Perform queued writes now, from whichever task is running.
///
/// Used by the reboot path, where waiting for the writer's next 100 ms tick would be a coin
/// flip: `esp_restart` does not wait for it. The check is the same one the writer task makes
/// — this is a `spi_flash` window, so the stack has to be in internal RAM — and an unsafe
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
/// `esp_ptr_in_dram` decides.
fn on_internal_stack() -> bool {
    let base = unsafe { sys::pxTaskGetStackStart(ptr::null_mut()) } as usize;
    in_internal_ram(base)
}

fn in_internal_ram(address: usize) -> bool {
    (sys::SOC_DRAM_LOW as usize..sys::SOC_DRAM_HIGH as usize).contains(&address)
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

unsafe extern "C" fn task_entry(params: *mut c_void) {
    let writer = &*(params as *const Writer);
    let app = &writer.shared;
    let log = &writer.log;

    // The one check that makes this task worth having, and it runs on the stack that does
    // the writing rather than on the stack of whoever created the task.
    let safe = on_internal_stack();
    with_app_mut(app, |app| app.attach_store_writer(NAME, safe));
    if safe {
        let base = unsafe { sys::pxTaskGetStackStart(ptr::null_mut()) } as usize;
        log.info(
            "store",
            format!(
                "{NAME} up: {STACK_BYTES} byte static stack at 0x{base:08x} (internal RAM), drains every {PERIOD_TICKS} ms"
            ),
        );
    } else {
        log.error(
            "store",
            "the writer task is running on a stack outside internal RAM; it will not write NVS",
        );
        // Deliberately idle instead of writing: a broken stack is a report, not a reboot.
        loop {
            sys::vTaskDelay(1_000);
        }
    }

    let mut reported = false;
    loop {
        sys::vTaskDelay(PERIOD_TICKS);
        let (written_now, total) = with_app(app, |app| {
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
                format!("{NAME} wrote {written_now} value(s); {head_room} of {STACK_BYTES} bytes of stack left ({total} write(s) since boot)"),
            );
        }
    }
}

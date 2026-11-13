//! Spawning a Rust thread with a name `ps` can show and a stack the caller chose.
//!
//! `std::thread::Builder::name` sets the *pthread* name. `ps` and `/api/status.threads`
//! use owned task labels (never post-snapshot `pcTaskName` pointers), while
//! `pthread_create` takes its FreeRTOS name from the **creating** thread's
//! `esp_pthread_cfg_t` — the same struct that decides where the stack comes from. So a
//! thread's visible name and its stack capabilities are one decision, made on the wrong
//! side of the call, and a config left behind by one spawn becomes the next thread's name
//! and priority. That is not hypothetical: the store writer set
//! `thread_name = "nvs-writer", prio = 2` for itself and the console, radio and agent
//! threads came up under it — `ps` showed four `nvs-writer`s, three of them the wrong task
//! at the wrong priority.
//!
//! Firmware-owned long-lived tasks go through [`spawn`]. Core HTTP workers use their
//! own bounded spawner. This helper sets the name, the size and the
//! capabilities; spawns; then restores the **default** config — never "whatever was there
//! before", because every `esp_pthread_set_cfg` here is scoped to one spawn, so a
//! remembered value is either the default or a leak, and restoring a leak is exactly the
//! bug above. Both calls are checked and a failed restore says so, since otherwise the next
//! thread silently inherits.
//!
//! A name is never inherited: `inherit_cfg = false`, so a thread that spawns something of
//! its own does not pass this name on.

use esp_idf_svc::sys;
use std::collections::BTreeMap;
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};
use std::thread::{Builder, JoinHandle};

// Own the labels independently of FreeRTOS task storage. TaskStatus_t's name
// pointer and handle may already be freed once uxTaskGetSystemState returns.
static LABELS: OnceLock<Mutex<BTreeMap<u32, (String, i32)>>> = OnceLock::new();

pub fn task_labels() -> BTreeMap<u32, (String, i32)> {
    LABELS.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner()).clone()
}

struct TaskLabel(u32);
impl TaskLabel {
    fn register(name: String) -> Self {
        // Safe: this thread owns its current task's lifetime until it exits.
        let task = unsafe { sys::xTaskGetCurrentTaskHandle() };
        // uxTaskGetTaskNumber reads the application-set tag (normally zero),
        // not the trace/creation number used by uxTaskGetSystemState. Read our
        // own live task's scalar info to use the same ID as status snapshots.
        let mut info: sys::TaskStatus_t = unsafe { std::mem::zeroed() };
        unsafe { sys::vTaskGetInfo(task, &mut info, 0, sys::eTaskState_eInvalid) };
        let id = info.xTaskNumber as u32;
        let affinity = unsafe { sys::xTaskGetCoreID(task) };
        LABELS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, (name, affinity));
        Self(id)
    }
}
impl Drop for TaskLabel {
    fn drop(&mut self) {
        LABELS.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner()).remove(&self.0);
    }
}

/// A stack that survives the flash cache being off — what any thread that may write NVS
/// needs (`nvs_writer.rs` has the arithmetic and the assert).
pub const INTERNAL: u32 = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;

/// A stack in PSRAM. Only for a thread that cannot reach flash while the cache is off.
pub const PSRAM: u32 = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;

/// Spawn `body` as a task named `task_name` (NUL-terminated, as FreeRTOS wants it).
///
/// `prio` is `None` for the configured default (5). `caps` is [`INTERNAL`] or [`PSRAM`].
/// The name must be a `'static` literal: the pthread layer keeps the pointer until the task
/// exists.
pub fn spawn<T, F>(
    task_name: &'static [u8],
    stack_bytes: usize,
    caps: u32,
    prio: Option<usize>,
    body: F,
) -> std::io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let name = rust_name(task_name);
    if task_name.last() != Some(&0) {
        return Err(std::io::Error::other(format!("thread `{name}`: the task name is not NUL-terminated")));
    }

    let mut cfg = unsafe { sys::esp_pthread_get_default_config() };
    cfg.thread_name = task_name.as_ptr() as *const c_char;
    cfg.stack_size = stack_bytes;
    // Not inherited: a name describes this thread, and a stack capability describes what
    // this thread is allowed to touch. Neither should leak into whatever it spawns.
    cfg.inherit_cfg = false;
    cfg.stack_alloc_caps = caps;
    // 0 is "use the configured default" in `pthread_create`.
    cfg.prio = prio.unwrap_or(0);

    let rc = unsafe { sys::esp_pthread_set_cfg(&cfg) };
    let spawned = if rc == sys::ESP_OK {
        let label = name.clone();
        Builder::new().name(name.clone()).stack_size(stack_bytes).spawn(move || {
            let _label = TaskLabel::register(label);
            body()
        })
    } else {
        Err(std::io::Error::other(format!("thread `{name}`: esp_pthread_set_cfg returned {rc}")))
    };

    // Restore the defaults, not "whatever was there before": every set_cfg in this firmware
    // is scoped to one spawn, so a remembered value is either the default or a leak — and
    // restoring a leak is how three threads came up as `nvs-writer`. `inherit_cfg = false`
    // on the way out so a thread spawned *outside* this helper — the HTTP listener lives in
    // `agent-core`, for the host too — does not inherit the name of whichever thread
    // spawned it; with no name and no inheritance it gets the pthread default, `pthread`.
    let mut back = unsafe { sys::esp_pthread_get_default_config() };
    back.inherit_cfg = false;
    let rc = unsafe { sys::esp_pthread_set_cfg(&back) };
    if rc != sys::ESP_OK {
        log::error!(
            "threads: could not restore the default pthread config after spawning `{name}` ({rc}); the next thread inherits its stack and name"
        );
    }
    spawned
}

/// The task name without its NUL, for `std::thread::Builder` (which takes a `str`).
fn rust_name(task_name: &'static [u8]) -> String {
    String::from_utf8_lossy(&task_name[..task_name.len().saturating_sub(1)]).into_owned()
}

//! Every key NVS has to hold, declared and assembled in one place.
//!
//! NVS keeps 15 bytes of key name 鈥?`NVS_KEY_NAME_MAX_SIZE` is 16 *including* the
//! terminator (`nvs.h`) 鈥?and refuses a longer one with `ESP_ERR_NVS_KEY_TOO_LONG`
//! (0x1109) **without storing anything**. That is not a style rule. This board spent its
//! whole life refusing every transcript write: `agent.transcript` is 16 bytes and
//! `agent.run_in_flight` 19, so the conversation and the "a run was in flight" marker were
//! never on flash, and the only trace was a number in `/api/status.store.failed`
//! (measured 2026-11-12, 23 refusals in 195 s 鈥?`docs/troubleshooting.md` 搂"a store write
//! was refused").
//!
//! So the names are not literals scattered over four modules any more. They are declared
//! here, every key is *assembled* by [`chunk`], and [`all`] enumerates the shapes that
//! assembly can produce 鈥?up to a two-digit chunk number, past anything a plan asks for
//! today. `every_key_the_board_can_generate_fits_nvs` is the assertion: it fails on a
//! longer prefix, on a longer separator, and on a wider index, which is the whole point of
//! not leaving this to a serial terminal.
//!
//! **There is nothing to migrate.** `agent.transcript` and `agent.run_in_flight` were
//! introduced already over the ceiling (`43b9f92`), so no build ever stored a value under
//! them on hardware 鈥?see `docs/agent-loop.md` 搂4. That is why this file has no legacy
//! names and no read-old-keys path: there is no old value on flash to read, and code that
//! looked for one would be searching for a key that never existed.

/// The NVS namespace the firmware opens (`firmware/src/main.rs`). A namespace name has the
/// same 15-byte ceiling as a key name, so it is covered by the same assertion.
pub const NAMESPACE: &str = "esp32mcp";

/// WiFi, MCP endpoint, buses, pins (`config.rs`).
pub const CONFIG: &str = "config";
/// Devices and tools (`manifest.rs`).
pub const MANIFEST: &str = "manifest";
/// The agent's own settings: model endpoint, model, persistence.
pub const AGENT: &str = "agent";
/// Scheduled tasks and sensor rules (`sched.rs`).
pub const SCHED: &str = "sched";
/// The file workspace's blob header and its first chunk (`WORKSPACE_PLAN`).
pub const WORKSPACE: &str = "ws";
/// The transcript's header and first part (`TRANSCRIPT_CHUNKS_MAX`).
pub const TRANSCRIPT: &str = "transcript";
/// Written before a run starts, removed when it ends.
pub const RUN_IN_FLIGHT: &str = "run_in_flight";

/// The one place a chunked key is assembled from a base and a chunk number.
///
/// Both chunked blobs go through it 鈥?the file workspace (`store::chunk_key`) and the
/// transcript (`App::transcript_key`) 鈥?so "what a key looks like" is one format string
/// rather than two that can drift, and changing the separator is a change both blobs and
/// the length assertion see at once.
pub fn chunk(base: &str, index: usize) -> String {
    format!("{base}.{index}")
}

/// A chunk of the file workspace. Chunk 1 is `ws.1`; the base key holds the header.
pub fn workspace_chunk(index: usize) -> String {
    chunk(WORKSPACE, index)
}

/// A chunk of the transcript. Chunk 0 *is* the base key, because a blob's first value
/// carries the header that says whether the other chunks belong together
/// ([`store::write_blob`](crate::store::write_blob)).
pub fn transcript(index: usize) -> String {
    if index == 0 {
        TRANSCRIPT.to_string()
    } else {
        chunk(TRANSCRIPT, index)
    }
}

/// Every key name the assembly in this module can produce: the seven base names plus both
/// chunked shapes for indices 0 through 99.
///
/// The index range is deliberately wider than today's plans (`TRANSCRIPT_CHUNKS_MAX` is 8,
/// `WORKSPACE_PLAN.chunks_max` is 5). A plan raised to 99 later must still be under the
/// ceiling, and this list is a length check on the *generator* 鈥?not on today's callers.
/// Nothing at runtime calls this; the assertion in `mod tests` and the plan-coverage test
/// in `app.rs` do.
pub fn all() -> Vec<String> {
    let mut keys: Vec<String> = [CONFIG, MANIFEST, AGENT, SCHED, WORKSPACE, TRANSCRIPT, RUN_IN_FLIGHT]
        .iter()
        .map(|key| key.to_string())
        .collect();
    for index in 0..=99usize {
        keys.push(workspace_chunk(index));
        keys.push(transcript(index));
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::KEY_MAX;

    /// The invariant, on the host: no name NVS has to hold is longer than it keeps.
    ///
    /// `agent.transcript` (16) and `agent.run_in_flight` (19) are the two names that hung
    /// the board's persistence for its whole life. This is what turns the next one into a
    /// failing test instead of a session with a serial cable.
    #[test]
    fn every_key_the_board_can_generate_fits_nvs() {
        assert!(NAMESPACE.len() <= KEY_MAX, "the namespace `{NAMESPACE}` is {} bytes", NAMESPACE.len());
        let keys = all();
        assert!(keys.len() > 200, "the table must include the chunked shapes: {}", keys.len());
        for key in &keys {
            assert!(!key.is_empty(), "NVS refuses an empty key too");
            assert!(
                key.len() <= KEY_MAX,
                "`{key}` is {} bytes and NVS keeps {KEY_MAX}: this key would be refused on \
                 every write, and the value would never reach flash",
                key.len()
            );
        }
    }

    /// The probe is wider than any plan, so raising `chunks_max` cannot step over the
    /// ceiling above without the list covering it.
    #[test]
    fn the_table_covers_a_two_digit_chunk_number() {
        for key in [workspace_chunk(99), transcript(99), transcript(0)] {
            assert!(all().contains(&key), "`{key}` is missing from keys::all()");
        }
        assert_eq!(transcript(0), TRANSCRIPT, "chunk 0 is the base key itself");
    }

    /// The names this replaced, kept as a fact rather than a memory: they do not fit,
    /// which is why no board ever stored them and why there is nothing to migrate.
    #[test]
    fn the_long_names_the_board_used_to_use_do_not_fit() {
        for old in ["agent.transcript", "agent.transcript.1", "agent.run_in_flight"] {
            assert!(old.len() > KEY_MAX, "`{old}` is {} bytes", old.len());
        }
    }
}

//! The skills a freshly flashed board starts with.
//!
//! A board comes out of the box with an empty workspace: no instructions, no skills, and
//! an agent that has to be told how to do everything. One skill is baked into the firmware
//! instead — how to write the next one — because that is the skill that cannot be looked up
//! and the one every other procedure needs first. `include_str!` is the whole mechanism: the
//! repository's `skills/` is the source, and the build fails if the file disappears.
//!
//! Nothing about it is special after that. It lands in `/skills/<name>/SKILL.md`, the same
//! place an owner's own skill goes, and [`crate::tools::knowledge::index`] picks it up like
//! any other file. A path that already holds text — a seeded skill the owner rewrote — is
//! never touched again, which is what makes the seed safe to run on every boot. A path the
//! owner *deleted* is empty, so the next boot writes it back: deleting a seeded skill means
//! removing it here, not on the board.
//!
//! The file carries the standard header (YAML front matter with `name` and `description`),
//! the same shape the host's skills use, so one skill written by hand and one written by the
//! agent look alike and the index reads them the same way.

use crate::app::AppWorkspace;
use crate::tools::fs::Workspace;
use crate::tools::knowledge::SKILLS_DIR;

/// The skills the firmware carries: directory name and the file itself.
pub const DEFAULT_SKILLS: &[(&str, &str)] =
    &[("create-skill", include_str!("../../../skills/create-skill/SKILL.md"))];

/// The workspace path of one of them, which is also what the prompt index prints.
pub fn skill_path(name: &str) -> String {
    format!("{SKILLS_DIR}/{name}/SKILL.md")
}

/// The default set for a board profile.
///
/// Every profile gets the same one today: writing a skill is not board-specific, and a
/// procedure that *is* board-specific belongs in a skill the owner writes, which is what
/// this one teaches. A profile that needs its own set adds itself here and nowhere else —
/// the board has one place for skills, and this function is the seam over it.
pub fn defaults_for(_profile: &str) -> &'static [(&'static str, &'static str)] {
    DEFAULT_SKILLS
}

/// Write the defaults into a workspace, and report how many were written.
///
/// Presence of the path is the whole test. There is no checksum of the shipped copy and
/// no revision counter: an owner who edits `pinout` into something that fits their bench
/// has made their version the real one, and a firmware upgrade must not argue with that.
///
/// Running out of room is not an error. The workspace is 8 KB, and three skills with a
/// memory beside them can exceed it; what did not fit is missing from the count, and the
/// log line in `App::seed_default_skills` is where that shows up.
pub fn seed(ws: &mut AppWorkspace, profile: &str) -> usize {
    let mut written = 0;
    for (name, text) in defaults_for(profile) {
        let path = skill_path(name);
        if ws.read(&path).is_ok() {
            continue;
        }
        if ws.write(&path, text).is_err() {
            break;
        }
        written += 1;
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, FwMeta, WORKSPACE_TOTAL_MAX};
    use crate::config::NVS_KEY_CONFIG;
    use crate::logbuf::LogBuf;
    use crate::store::{KvStore, MemoryStore};
    use crate::tools::fs::MAX_FILE_BYTES;
    use crate::tools::knowledge;
    use crate::virtual_board::VirtualBoard;

    /// An app over a store that holds exactly this config, the way the board boots.
    fn app_with_config(config: &str) -> App {
        let mut store = MemoryStore::new();
        store.set(NVS_KEY_CONFIG, config);
        App::new(Box::new(VirtualBoard::new()), Box::new(store), LogBuf::new(64), FwMeta::default())
    }

    fn seeded(profile: &str) -> AppWorkspace {
        let mut ws = AppWorkspace::default();
        assert_eq!(seed(&mut ws, profile), DEFAULT_SKILLS.len(), "a fresh workspace takes them all");
        ws
    }

    #[test]
    fn a_fresh_workspace_gets_the_skill_and_the_index_names_it() {
        let ws = seeded("esp32s3-xiaozhi-bread");
        let index = knowledge::index(&ws);
        let names: Vec<&str> = index.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["create-skill"]);
        for skill in &index {
            assert_eq!(skill.path, skill_path(&skill.name), "the index must carry the path to open");
            assert!(!skill.description.is_empty(), "{} reached the prompt without a line", skill.name);
            assert!(!skill.description.ends_with('…'), "{} is clipped: {}", skill.name, skill.description);
            assert_eq!(skill.bytes, ws.read(&skill.path).unwrap().len());
        }
    }

    #[test]
    fn a_second_seed_writes_nothing() {
        let mut ws = seeded("generic");
        assert_eq!(seed(&mut ws, "generic"), 0, "seeding is not a rewrite");
        assert_eq!(knowledge::index(&ws).len(), DEFAULT_SKILLS.len(), "and it added nothing");
    }

    #[test]
    fn a_skill_the_owner_rewrote_is_never_overwritten() {
        let mut ws = seeded("generic");
        let mine =
            "---\nname: create-skill\ndescription: our own house style\n---\n# create-skill\n12 relay\n";
        ws.write(&skill_path("create-skill"), mine).expect("the owner edits");

        assert_eq!(seed(&mut ws, "generic"), 0, "the shipped copy must not come back");
        assert_eq!(ws.read(&skill_path("create-skill")).unwrap(), mine);
    }

    /// The seed fills an empty path and is safe on every boot, so a skill deleted on the
    /// board comes back. Removing one for good is a change here, not a `rm`.
    #[test]
    fn a_skill_deleted_on_the_board_is_seeded_again() {
        let mut ws = seeded("generic");
        ws.remove(&skill_path("create-skill")).expect("the owner deletes it");
        assert_eq!(seed(&mut ws, "generic"), 1, "the next boot writes it back");
        assert!(ws.read(&skill_path("create-skill")).is_ok());
    }

    #[test]
    fn the_flag_stops_the_seed_and_is_on_by_default() {
        let fresh = app_with_config("{}");
        assert_eq!(fresh.workspace.len(), DEFAULT_SKILLS.len(), "a new board starts with it");
        for (name, _) in DEFAULT_SKILLS {
            assert!(fresh.workspace.read(&skill_path(name)).is_ok(), "{name} is not in the workspace");
        }

        let off = app_with_config(r#"{"skills":{"seed_defaults":false}}"#);
        assert_eq!(off.workspace.len(), 0, "the owner turned it off; nothing may be written");
        assert!(!off.cfg.skills.seed_defaults);
    }

    /// The guard that keeps this a build failure instead of a board that silently seeds a
    /// file it cannot write: the skill has to leave room for one full memory beside it, and
    /// it has to carry the standard header — the same one a hand-written skill uses.
    #[test]
    fn the_shipped_skills_fit_the_board_and_carry_a_description() {
        let mut total = 0;
        for (name, text) in DEFAULT_SKILLS {
            assert!(knowledge::is_skill_name(name), "`{name}` is not a name the index accepts");
            assert!(text.len() < MAX_FILE_BYTES, "{name} is {} bytes; a stored file is smaller", text.len());
            assert!(text.is_ascii(), "{name} carries non-ASCII text");

            // The standard header, not a `#` line with keys under it: front matter opened
            // and closed by `---`, carrying the name and the description.
            let (meta, body) = knowledge::split_front_matter(text);
            assert!(!meta.is_empty(), "{name} has no front matter");
            assert!(
                meta.lines().any(|l| l.trim() == format!("name: {name}")),
                "{name}: front matter must carry `name: {name}`\n{meta}"
            );
            assert!(body.trim_start().starts_with('#'), "{name}: the body starts with a heading");

            let line = meta
                .lines()
                .find_map(|l| l.trim().strip_prefix("description:"))
                .unwrap_or_else(|| panic!("{name} has no `description:` in its header"));
            let line = line.trim();
            assert!(!line.is_empty(), "{name} has an empty description");
            assert!(line.chars().count() <= 80, "{name} would be clipped in the prompt: {line}");

            // What the index reports is what the prompt shows: same line, unclipped.
            let ws = seeded("generic");
            let skill = knowledge::index(&ws).into_iter().find(|s| s.name == *name).expect("indexed");
            assert_eq!(skill.description, line);
            total += text.len();
        }
        assert!(
            total + MAX_FILE_BYTES <= WORKSPACE_TOTAL_MAX,
            "{total} bytes of skills leave no room for a memory file in {WORKSPACE_TOTAL_MAX}"
        );
    }
}

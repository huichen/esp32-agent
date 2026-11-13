//! Skills and project instructions: the two kinds of context an agent needs that are
//! neither tools nor transcript.
//!
//! Both live on the board's file workspace, and **no tool of their own**: `fs_ls`,
//! `fs_read` and `fs_write` already reach them, and a fifth tool that does the same
//! thing costs a definition in every request and one more choice for a small model.
//! What this module owns is the part the file tools cannot do — putting the right
//! amount of that content into the system prompt:
//!
//! ```text
//! /AGENTS.md          project instructions, always in the system prompt (bounded)
//! /skills/<name>.md   one skill per file: name, description, then the body
//! ```
//!
//! A skill file:
//!
//! ```text
//! # deploy
//! description: roll the portal forward without losing the transcript
//! <body — the procedure, the gotchas, the acceptance check>
//! ```
//!
//! The split matters on a board with ~300 KB of heap: the *index* (one line per skill)
//! is always in the prompt, and a skill's *body* only when the model reads the file.

use crate::tools::fs::Workspace;

/// Project instructions that always apply.
pub const AGENTS_MD: &str = "/AGENTS.md";
/// Where skills live.
pub const SKILLS_DIR: &str = "/skills";
/// The memory index the agent writes and the prompt reads.
pub const MEMORY_MD: &str = "/MEMORY.md";
/// Where the memories themselves live, one file per topic.
pub const MEMORY_DIR: &str = "/memory";

/// How much of the memory index reaches the system prompt, and how many lines.
///
/// The index is a table of contents: 24 lines at ~80 bytes is about 2 KB, the same order
/// as the skill index, and it leaves the rest of the prompt to the tool definitions.
pub const MEMORY_BUDGET: usize = 2048;
pub const MEMORY_INDEX_MAX: usize = 24;

/// How much of `/AGENTS.md` reaches the system prompt.
///
/// The rest stays on the workspace and is reachable with `fs_read`, which is the point:
/// a prompt that carries everything crowds out the tool descriptions, and the tool
/// descriptions are what make a small model useful.
pub const INSTRUCTIONS_BUDGET: usize = 1024;

/// How many skills the prompt names. Each line is ~100 bytes, so this bounds the index
/// at about 1.2 KB.
pub const SKILL_INDEX_MAX: usize = 12;

const DESCRIPTION_MAX: usize = 80;
const NAME_MAX: usize = 32;

/// One skill as the index sees it: enough to open it without guessing the shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub bytes: usize,
    /// `/skills/<name>/SKILL.md`, or `/skills/<name>.md` for a skill that is one file.
    pub path: String,
}

/// A name that can be printed and asked for without quoting. It is also the directory
/// (or file) name, so it is what a path segment looks like.
pub fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The skill index: name, description, size and path for everything under `/skills`.
///
/// Two shapes, because a skill is sometimes a procedure and sometimes a procedure with
/// files next to it:
///
/// * `/skills/<name>/SKILL.md` — the canonical one; anything else in that directory is
///   the skill's own material (a pinout, a table, an acceptance script the model runs by
///   hand with `gpio_write`).
/// * `/skills/<name>.md` — a skill that needs nothing else.
///
/// Unreadable or misnamed entries are skipped rather than reported as errors: the index
/// feeds the system prompt, and a prompt that fails is a turn that never happens.
pub fn index(ws: &dyn Workspace) -> Vec<Skill> {
    let names = match ws.list(SKILLS_DIR) {
        Ok(names) => names,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<Skill> = names
        .into_iter()
        .filter_map(|entry| {
            let (name, path) = match entry.strip_suffix(".md") {
                Some(stem) => (stem.to_string(), format!("{SKILLS_DIR}/{entry}")),
                None => (entry.clone(), format!("{SKILLS_DIR}/{entry}/SKILL.md")),
            };
            if !is_skill_name(&name) {
                return None;
            }
            let text = ws.read(&path).ok()?;
            Some(Skill { description: describe(&text), bytes: text.len(), name, path })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(SKILL_INDEX_MAX);
    out
}

/// Split a leading `---` block off a document.
///
/// The host's skills use YAML front matter; on the board this is read line by line rather
/// than parsed, because the only keys that matter are `name` and `description` and a YAML
/// parser is more heap than the whole feature is worth.
pub fn split_front_matter(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---") else { return ("", text) };
    let Some(end) = rest.find("\n---") else { return ("", text) };
    (rest[..end].trim_matches('\n'), rest[end + 4..].trim_start_matches(['\n', '\r']))
}

/// One `key: value` line out of a front-matter block.
fn meta_value<'a>(meta: &'a str, key: &str) -> Option<&'a str> {
    for line in meta.lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.trim().eq_ignore_ascii_case(key) {
            let value = value.trim().trim_matches(['"', '\'']).trim();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// The description: front matter first, then a `description:` line, then the first prose
/// line.
///
/// Forgiving on purpose: a skill that is useful but has no header should still appear in
/// the index, because the index is how the model finds it at all.
fn describe(text: &str) -> String {
    let (meta, body) = split_front_matter(text);
    if let Some(value) = meta_value(meta, "description") {
        return clip(value, DESCRIPTION_MAX);
    }
    for line in body.lines().take(8) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description:") {
            return clip(rest.trim(), DESCRIPTION_MAX);
        }
    }
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.ends_with(':') || line.starts_with("---") {
            continue;
        }
        return clip(line, DESCRIPTION_MAX);
    }
    String::new()
}

/// What the system prompt gets from `/AGENTS.md`: the head of the file, bounded.
pub fn instructions(ws: &dyn Workspace) -> Option<String> {
    let text = ws.read(AGENTS_MD).ok()?;
    let body = injections(&text);
    if body.trim().is_empty() {
        return None;
    }
    Some(body)
}

/// The memory index: `/memory.md`, one line per memory, each pointing at a file.
///
/// The shape is deliberate and mirrors how this project's own agent keeps its notes: the
/// index is cheap and always present, the memories are files and only opened when one is
/// relevant. Lines that are not pointers (prose, an empty bullet) are dropped, so a model
/// that writes a paragraph instead of an index does not spend the prompt budget on it.
pub fn memory(ws: &dyn Workspace) -> Option<String> {
    let text = ws.read(MEMORY_MD).ok()?;
    let mut out = String::new();
    let mut lines = 0;
    let truncated = format!("- (truncated; the rest is in {MEMORY_MD}, read it with fs_read)\n");
    let full = format!("- (index full: {MEMORY_INDEX_MAX} lines; replace an entry)\n");
    // Include the marker in the budget, and check before appending an untrusted line.
    let content_budget = MEMORY_BUDGET.saturating_sub(truncated.len().max(full.len()));
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        // A pointer: a bullet that names a path the reader can open.
        let is_pointer = line.starts_with("- ") && line.contains("/memory/");
        if !is_pointer {
            continue;
        }
        if lines >= MEMORY_INDEX_MAX {
            out.push_str(&full);
            break;
        }
        if line.len().saturating_add(1) > content_budget.saturating_sub(out.len()) {
            let mut end = content_budget.saturating_sub(out.len()).saturating_sub(1).min(line.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                out.push_str(&line[..end]);
                out.push('\n');
            }
            out.push_str(&truncated);
            break;
        }
        out.push_str(line);
        out.push('\n');
        lines += 1;
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The injected part: the first [`INSTRUCTIONS_BUDGET`] bytes, cut on a character
/// boundary, with a marker that says where the rest is.
fn injections(text: &str) -> String {
    let text = text.trim();
    if text.len() <= INSTRUCTIONS_BUDGET {
        return text.to_string();
    }
    let mut end = INSTRUCTIONS_BUDGET;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(truncated; the rest is in {AGENTS_MD}, read it with fs_read)", &text[..end])
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The smallest workspace that satisfies the trait: the real one lives in `app.rs`.
    #[derive(Default)]
    struct Mem {
        files: BTreeMap<String, String>,
    }

    impl Workspace for Mem {
        fn list(&self, dir: &str) -> Result<Vec<String>, String> {
            let prefix = format!("{}/", dir.trim_end_matches('/'));
            let mut out = Vec::new();
            for path in self.files.keys() {
                if let Some(rest) = path.strip_prefix(&prefix) {
                    let child = rest.split('/').next().unwrap_or_default().to_string();
                    if !child.is_empty() && !out.contains(&child) {
                        out.push(child);
                    }
                }
            }
            Ok(out)
        }

        fn read(&self, path: &str) -> Result<String, String> {
            self.files.get(path).cloned().ok_or_else(|| format!("no such file: {path}"))
        }

        fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
            self.files.insert(path.to_string(), text.to_string());
            Ok(())
        }

        fn remove(&mut self, path: &str) -> Result<(), String> {
            self.files.remove(path).map(|_| ()).ok_or_else(|| format!("no such file: {path}"))
        }
    }

    fn ws_with(files: &[(&str, &str)]) -> Mem {
        let mut ws = Mem::default();
        for (path, text) in files {
            ws.write(path, text).expect("write");
        }
        ws
    }

    #[test]
    fn the_index_reads_a_skill_directory_a_flat_file_and_both_header_styles() {
        let ws = ws_with(&[
            // The canonical shape: a directory with SKILL.md and the host's front matter.
            (
                "/skills/deploy/SKILL.md",
                "---\nname: deploy\ndescription: roll the portal forward\n---\n# Deploy\n1. snapshot\n",
            ),
            // Supporting material next to it: part of the skill, not a skill itself.
            ("/skills/deploy/pinout.md", "GPIO12 relay\n"),
            // A skill that needs nothing else.
            ("/skills/notes.md", "# notes\n\ndescription: wiring notes for the bench\n"),
            // No header at all: still indexed, from its first prose line.
            ("/skills/bare/SKILL.md", "# bare\nnothing but prose here\n"),
            // Not skills: a wrong name, and a directory with no SKILL.md.
            ("/skills/bad name/SKILL.md", "nope"),
            ("/skills/empty/readme.md", "nope"),
        ]);
        let index = index(&ws);
        let names: Vec<&str> = index.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["bare", "deploy", "notes"], "{index:?}");
        assert_eq!(index[1].description, "roll the portal forward");
        assert_eq!(index[1].path, "/skills/deploy/SKILL.md");
        assert_eq!(index[2].description, "wiring notes for the bench");
        assert_eq!(index[2].path, "/skills/notes.md");
        assert_eq!(index[0].description, "nothing but prose here");
    }

    #[test]
    fn the_index_is_bounded() {
        let mut ws = Mem::default();
        for i in 0..SKILL_INDEX_MAX + 5 {
            ws.write(&format!("/skills/s{i:02}/SKILL.md"), &format!("---\ndescription: d{i}\n---\nbody\n"))
                .unwrap();
        }
        assert_eq!(index(&ws).len(), SKILL_INDEX_MAX, "the prompt index must stay bounded");
    }

    #[test]
    fn the_prompt_gets_the_head_of_the_instructions_and_a_pointer_to_the_rest() {
        let long = "x".repeat(INSTRUCTIONS_BUDGET + 500);
        let ws = ws_with(&[(AGENTS_MD, &long)]);
        let injected = instructions(&ws).expect("instructions");
        assert!(injected.len() < long.len(), "the prompt must not carry the whole file");
        assert!(injected.contains("truncated") && injected.contains("fs_read"), "{injected}");
        assert!(injected.len() <= INSTRUCTIONS_BUDGET + 120, "{}", injected.len());
    }

    #[test]
    fn no_instructions_and_no_skills_is_a_normal_state() {
        let ws = ws_with(&[]);
        assert!(instructions(&ws).is_none());
        assert!(index(&ws).is_empty());
        assert!(memory(&ws).is_none(), "a board that has written nothing has no memory index");
    }

    #[test]
    fn the_memory_index_reaches_the_prompt_as_pointers() {
        let index = "# memories\n\n- [pins](/{}/pins.md) — GPIO18 is the strip, active high (2026-11-11)\n- [wifi](/{}/wifi.md) — the 5 GHz SSID is not visible (2026-11-10)\n";
        let text = index.replace("{}", "memory");
        let ws = ws_with(&[(MEMORY_MD, &text), ("/memory/pins.md", "GPIO18 = strip")]);
        let injected = memory(&ws).expect("index");
        assert!(injected.contains("memory/pins.md"), "{injected}");
        assert!(injected.contains("memory/wifi.md"), "{injected}");
        assert!(!injected.contains("# memories"), "headings are not pointers: {injected}");
    }

    #[test]
    fn prose_in_the_memory_file_does_not_spend_the_prompt_budget() {
        let text = "This paragraph is not an index entry and must be ignored.\n- not a pointer either\n";
        let ws = ws_with(&[(MEMORY_MD, text)]);
        assert!(memory(&ws).is_none(), "nothing that a reader could follow");
    }

    #[test]
    fn a_memory_index_that_grows_past_the_cap_says_so() {
        let mut text = String::new();
        for i in 0..MEMORY_INDEX_MAX + 3 {
            text.push_str(&format!("- [t{i}](/memory/t{i}.md) — hook\n"));
        }
        let ws = ws_with(&[(MEMORY_MD, &text)]);
        let injected = memory(&ws).expect("index");
        assert_eq!(injected.matches("/memory/t").count(), MEMORY_INDEX_MAX, "{injected}");
        assert!(injected.contains("index full"), "{injected}");

        // One pointer may itself be a whole workspace file. The byte budget is a
        // limit, not a trigger that fires only after that file has entered the prompt.
        let huge = format!("- [huge](/memory/huge.md) {}", "界".repeat(MEMORY_BUDGET));
        let ws = ws_with(&[(MEMORY_MD, &huge)]);
        let injected = memory(&ws).expect("bounded pointer");
        assert!(injected.len() <= MEMORY_BUDGET, "{} bytes", injected.len());
        assert!(injected.contains("truncated"), "{injected}");
    }
}

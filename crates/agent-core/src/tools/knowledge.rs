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

/// One skill as the index sees it: enough to decide whether to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub bytes: usize,
}

/// A name that can be printed and asked for without quoting. The file stem is the name,
/// so this is also what a path segment looks like.
pub fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The skill index: name, description and size for every `/skills/*.md`.
///
/// Unreadable or misnamed files are skipped rather than reported as errors: the index
/// feeds the system prompt, and a prompt that fails is a turn that never happens.
pub fn index(ws: &dyn Workspace) -> Vec<Skill> {
    let names = match ws.list(SKILLS_DIR) {
        Ok(names) => names,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<Skill> = names
        .into_iter()
        .filter(|n| n.ends_with(".md"))
        .filter_map(|file| {
            let name = file.trim_end_matches(".md").to_string();
            if !is_skill_name(&name) {
                return None;
            }
            let text = ws.read(&format!("{SKILLS_DIR}/{file}")).ok()?;
            Some(Skill { description: describe(&text), bytes: text.len(), name })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(SKILL_INDEX_MAX);
    out
}

/// The `description:` line of a skill, or its first prose line.
///
/// Forgiving on purpose: a skill that is useful but has no header should still appear in
/// the index, because the index is how the model finds it at all — and with `fs_write`
/// as the only way to create one, there is no writer to enforce a header.
fn describe(text: &str) -> String {
    for line in text.lines().take(8) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description:") {
            return clip(rest.trim(), DESCRIPTION_MAX);
        }
    }
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.ends_with(':') {
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
    fn the_index_reads_the_header_and_falls_back_to_the_first_prose_line() {
        let ws = ws_with(&[
            ("/skills/deploy.md", "# deploy\ndescription: roll the portal forward\nbody\n"),
            // Written with plain fs_write, so there is no header to parse.
            ("/skills/notes.md", "# notes\n\nWiring notes for the bench.\n"),
            ("/skills/ignored.txt", "not a skill"),
            ("/skills/bad name.md", "a space is not a skill name"),
        ]);
        let index = index(&ws);
        assert_eq!(index.len(), 2, "only .md files with legal names are skills: {index:?}");
        assert_eq!(index[0].name, "deploy");
        assert_eq!(index[0].description, "roll the portal forward");
        assert_eq!(index[1].description, "Wiring notes for the bench.");
    }

    #[test]
    fn the_index_is_bounded() {
        let files: Vec<(String, String)> = (0..SKILL_INDEX_MAX + 5)
            .map(|i| (format!("/skills/s{i:02}.md"), format!("# s{i:02}\ndescription: d{i}\n")))
            .collect();
        let mut ws = Mem::default();
        for (path, text) in &files {
            ws.write(path, text).unwrap();
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
    }
}

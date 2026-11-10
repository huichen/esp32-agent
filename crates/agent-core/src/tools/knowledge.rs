//! Skills and project instructions: the two kinds of context an agent needs that are
//! neither tools nor transcript.
//!
//! Both live on the board's file workspace, so they are writable from the portal, from
//! MCP, or by the agent itself — there is no second store to keep in sync:
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
//! is always in the prompt, and a skill's *body* only when the model asks for it. That
//! is the same progressive disclosure the host agent uses, scaled down to 2 KB files.

use crate::json as j;
use crate::tool::{object_schema, string};
use crate::tools::fs::Workspace;
use serde_json::{json, Value};

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

/// One skill as the index sees it: enough to decide whether to load it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub bytes: usize,
}

/// A legal skill name: the file stem, restricted so it can be printed and asked for
/// without quoting.
pub fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn specs() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "skill_list",
                "description": "List the board's skills: the name and one-line description of each `/skills/<name>.md`. The same index is already in your system prompt; call this when you need the byte sizes or think the list has changed",
                "parameters": object_schema(&[], &[]),
            }
        },
        {
            "type": "function",
            "function": {
                "name": "skill_read",
                "description": "Load one skill's full text from `/skills/<name>.md`. Read a skill before following it; do not reconstruct a procedure from its description",
                "parameters": object_schema(
                    &[("name", string("the skill to load, e.g. `deploy`"))],
                    &["name"],
                ),
            }
        },
        {
            "type": "function",
            "function": {
                "name": "agents_md",
                "description": "Read or replace the project instructions in `/AGENTS.md`. What is stored there is injected into your system prompt on every turn (the first 1024 bytes; the rest is read with fs_read)",
                "parameters": object_schema(
                    &[
                        ("action", json!({ "type": "string", "enum": ["get", "set"], "description": "get (default) or set" })),
                        ("text", string("with action=set: the whole new contents of /AGENTS.md")),
                    ],
                    &[],
                ),
            }
        }
    ])
}

/// Route one of the three tools, or `None` when the name is not ours.
pub fn call(ws: &mut dyn Workspace, name: &str, args: &Value) -> Option<Result<String, String>> {
    match name {
        "skill_list" => Some(list(ws)),
        "skill_read" => Some(read_skill(ws, args)),
        "agents_md" => Some(agents_md(ws, args)),
        _ => None,
    }
}

fn list(ws: &mut dyn Workspace) -> Result<String, String> {
    let skills = index(ws);
    Ok(to_text(json!({
        "count": skills.len(),
        "dir": SKILLS_DIR,
        "skills": skills.iter().map(|s| json!({
            "name": s.name, "description": s.description, "bytes": s.bytes,
        })).collect::<Vec<Value>>(),
        "note": "load one with skill_read {name}; a skill is a procedure, not a hint",
    })))
}

fn read_skill(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let name = j::field(args, "name").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
    if !is_skill_name(&name) {
        return Err(format!(
            "`{name}` is not a skill name (a-z, A-Z, 0-9, `_`, `-`, max {NAME_MAX}); the skills are: {}",
            names(ws).join(", ")
        ));
    }
    let path = format!("{SKILLS_DIR}/{name}.md");
    let text = ws
        .read(&path)
        .map_err(|e| format!("no skill `{name}` ({e}); the skills are: {}", names(ws).join(", ")))?;
    Ok(to_text(json!({
        "name": name,
        "path": path,
        "bytes": text.len(),
        "text": text,
    })))
}

fn agents_md(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let action = j::opt_str(args, "action").unwrap_or_else(|| "get".to_string());
    match action.trim() {
        "get" => match ws.read(AGENTS_MD) {
            Ok(text) => Ok(to_text(json!({
                "path": AGENTS_MD,
                "bytes": text.len(),
                "injected_bytes": injections(&text).len(),
                "text": text,
            }))),
            // No instructions is a normal state: the board ships without them.
            Err(_) => Ok(to_text(json!({
                "path": AGENTS_MD,
                "exists": false,
                "note": "no project instructions yet; write them with action=set",
            }))),
        },
        "set" => {
            let text = j::field(args, "text").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if text.trim().is_empty() {
                return Err(
                    "`text` is required with action=set (use an empty file to remove them)".to_string()
                );
            }
            ws.write(AGENTS_MD, &text)?;
            Ok(to_text(json!({
                "path": AGENTS_MD,
                "bytes": text.len(),
                "injected_bytes": injections(&text).len(),
                "note": "in the system prompt from the next turn; the rest is read with fs_read",
            })))
        }
        other => Err(format!("`{other}` is not an action; use get or set")),
    }
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

/// The names only, for error messages.
fn names(ws: &mut dyn Workspace) -> Vec<String> {
    let mut out: Vec<String> = index(ws).into_iter().map(|s| s.name).collect();
    if out.is_empty() {
        out.push("(none: write one to /skills/<name>.md)".to_string());
    }
    out
}

/// The `description:` line of a skill, or its first prose line.
///
/// Forgiving on purpose: a skill that is useful but has no header should still appear in
/// the index, because the index is how the model finds it at all.
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

fn to_text(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
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
            ("/skills/notes.md", "# notes\n\nWiring notes for the bench.\n"),
            ("/skills/ignored.txt", "not a skill"),
        ]);
        let index = index(&ws);
        assert_eq!(index.len(), 2, "only .md files are skills: {index:?}");
        assert_eq!(index[0].name, "deploy");
        assert_eq!(index[0].description, "roll the portal forward");
        assert_eq!(index[1].description, "Wiring notes for the bench.");
    }

    #[test]
    fn a_skill_body_is_loaded_whole_and_an_unknown_name_lists_the_rest() {
        let mut ws = ws_with(&[("/skills/deploy.md", "# deploy\ndescription: d\nstep one\nstep two\n")]);
        let text = call(&mut ws, "skill_read", &json!({ "name": "deploy" })).unwrap().unwrap();
        assert!(text.contains("step one") && text.contains("step two"), "{text}");
        let err = call(&mut ws, "skill_read", &json!({ "name": "nope" })).unwrap().unwrap_err();
        assert!(err.contains("deploy"), "the error must list what exists: {err}");
        let err = call(&mut ws, "skill_read", &json!({ "name": "../etc" })).unwrap().unwrap_err();
        assert!(err.contains("not a skill name"), "{err}");
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
        let mut ws = ws_with(&[]);
        let text = call(&mut ws, "agents_md", &json!({})).unwrap().unwrap();
        assert!(text.contains("\"exists\":false"), "{text}");
    }

    #[test]
    fn agents_md_round_trips_through_the_workspace() {
        let mut ws = ws_with(&[]);
        call(&mut ws, "agents_md", &json!({ "action": "set", "text": "Pin 12 is the pump." }))
            .unwrap()
            .unwrap();
        let text = call(&mut ws, "agents_md", &json!({ "action": "get" })).unwrap().unwrap();
        assert!(text.contains("Pin 12 is the pump."), "{text}");
        assert!(ws.read(AGENTS_MD).unwrap().contains("pump"));
    }
}

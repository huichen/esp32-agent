//! File tools for the on-board agent: `fs_ls`, `fs_read`, `fs_write`,
//! `fs_edit`, `fs_grep`.
//!
//! The storage itself is not this module's business. [`Workspace`] is the whole
//! surface the tools need, so the same tools run against the in-RAM map that the
//! firmware persists in NVS today and against a flash filesystem later.
//!
//! Paths are absolute-ish: `/` is the root, `/notes/a.txt` is a file in it, and a
//! path without a leading `/` is read as if it had one (`notes/a.txt` ==
//! `/notes/a.txt`). `..` is rejected rather than resolved — a model that sends it
//! is confused, and silently clamping it to the root would hide that. There are
//! no symlinks: the trait only knows flat keys, so there is nothing to follow.
//!
//! Every tool answer is JSON text, because that is what the agent loop feeds back
//! to the model as a tool message.

use serde_json::{json, Value};

/// Largest file this layer accepts. The board keeps the whole workspace in RAM
/// (<= 8 KB), so a single file may take a quarter of it.
/// The most text one workspace file may hold.
///
/// It was 2048 because the whole workspace was a 17.5 KB blob over five NVS values. The
/// workspace is a file on a 12 MB filesystem now, so this is the number a person would expect
/// from a note pad and not from a sticky note: 256 KB of Markdown, or a JSON log.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// How deep `fs_grep` walks before it gives up.
const MAX_DEPTH: usize = 8;
/// How many files `fs_grep` opens in one call.
const MAX_FILES: usize = 128;
/// How many matching lines `fs_grep` reports.
const MAX_MATCHES: usize = 40;

/// The storage behind the file tools.
///
/// Implementations are expected to be cheap and infallible for small stores.
/// Two contract points the tools rely on:
///
/// * `list` returns the **immediate** entry names under `dir` (not full paths,
///   not recursive), sorted or not; `dir` itself must exist, an empty directory
///   returns an empty vector.
/// * `read` on a directory (or on anything that is not a stored file) must return
///   `Err`, so the tools can tell files from directories with the four methods
///   above and nothing else.
pub trait Workspace {
    fn list(&self, dir: &str) -> Result<Vec<String>, String>;
    fn read(&self, path: &str) -> Result<String, String>;
    fn write(&mut self, path: &str, text: &str) -> Result<(), String>;
    fn remove(&mut self, path: &str) -> Result<(), String>;
}

/// Canonicalise a tool-supplied path: `/a/b`, leading slash added if missing,
/// empty and `.` components dropped, `..` rejected.
pub fn normalize(path: &str) -> Result<String, String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.trim().split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err("path traversal is not allowed (`..` in path)".to_string()),
            _ => {
                if part.contains('\\') {
                    return Err(format!("invalid path component `{part}`: use `/` as separator"));
                }
                if part.chars().any(|c| c.is_control()) {
                    return Err(format!("invalid path component `{part}`: control character"));
                }
                parts.push(part);
            }
        }
    }
    Ok(if parts.is_empty() { "/".to_string() } else { format!("/{}", parts.join("/")) })
}

pub fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn opt_str(args: &Value, name: &str) -> Option<String> {
    match args.get(name) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn req_str(args: &Value, name: &str) -> Result<String, String> {
    match opt_str(args, name) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(format!("`{name}` (non-empty string) is required")),
    }
}

/// Byte length of the text as it will be stored.
fn size(text: &str) -> usize {
    text.len()
}

/// The board has no trash: a destructive tool asks for a boolean, not a default.
fn confirmed(args: &Value) -> bool {
    args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false)
}

// ---------------------------------------------------------------- tool schemas

fn params(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        },
    })
}

/// The OpenAI `tools` array for the file tools.
pub fn specs() -> Value {
    json!([
        function(
            "fs_ls",
            "List one directory of the board workspace. `/` is the root.",
            params(
                json!({
                    "path": { "type": "string", "description": "directory to list, default `/`" },
                }),
                &[],
            ),
        ),
        function(
            "fs_read",
            "Read a text file from the board workspace.",
            params(
                json!({
                    "path": { "type": "string", "description": "file path, e.g. `/notes/a.txt`" },
                }),
                &["path"],
            ),
        ),
        function(
            "fs_write",
            "Create or overwrite a text file in the board workspace. Parent directories are created implicitly. Files are limited to 2048 bytes.",
            params(
                json!({
                    "path": { "type": "string", "description": "file path to write" },
                    "text": { "type": "string", "description": "complete new contents" },
                }),
                &["path", "text"],
            ),
        ),
        function(
            "fs_edit",
            "Replace exactly one occurrence of `find` in a file. Fails when `find` occurs zero times or more than once, so an ambiguous edit never silently rewrites the wrong place.",
            params(
                json!({
                    "path": { "type": "string", "description": "file path to edit" },
                    "find": { "type": "string", "description": "literal text, must occur exactly once" },
                    "replace": { "type": "string", "description": "replacement text" },
                }),
                &["path", "find", "replace"],
            ),
        ),
        function(
            "fs_rm",
            "Delete one file from the workspace. Needs confirm=true: there is no trash on the board. Deleting a skill's SKILL.md is how the skill leaves the prompt index.",
            params(
                json!({
                    "path": { "type": "string", "description": "file path to delete" },
                    "confirm": { "type": "boolean", "description": "must be true" },
                }),
                &["path", "confirm"],
            ),
        ),
        function(
            "fs_cp",
            "Copy a file, or every file under a directory, inside the workspace. `to` is the new path itself, not a directory to copy into.",
            params(
                json!({
                    "from": { "type": "string", "description": "existing file or directory" },
                    "to": { "type": "string", "description": "new path" },
                }),
                &["from", "to"],
            ),
        ),
        function(
            "fs_mv",
            "Move (rename) a file, or every file under a directory. Needs confirm=true: the source is removed.",
            params(
                json!({
                    "from": { "type": "string", "description": "existing file or directory" },
                    "to": { "type": "string", "description": "new path" },
                    "confirm": { "type": "boolean", "description": "must be true" },
                }),
                &["from", "to", "confirm"],
            ),
        ),
        function(
            "fs_grep",
            "Search the workspace for lines containing a literal substring. Walks files under `path` recursively.",
            params(
                json!({
                    "pattern": { "type": "string", "description": "literal substring to look for" },
                    "path": { "type": "string", "description": "file or directory to search, default `/`" },
                }),
                &["pattern"],
            ),
        ),
    ])
}

// ----------------------------------------------------------------- operations

/// Route one tool call. `None` means the name is not a file tool, so the caller
/// can try the next tool family.
pub fn call(ws: &mut dyn Workspace, name: &str, args: &Value) -> Option<Result<String, String>> {
    match name {
        "fs_ls" => Some(ls(ws, args)),
        "fs_read" => Some(read(ws, args)),
        "fs_write" => Some(write(ws, args)),
        "fs_edit" => Some(edit(ws, args)),
        "fs_rm" => Some(rm(ws, args)),
        "fs_cp" => Some(transfer(ws, args, false)),
        "fs_mv" => Some(transfer(ws, args, true)),
        "fs_grep" => Some(grep(ws, args)),
        _ => None,
    }
}

/// Copy (`move_it == false`) or move a file or a whole directory tree.
///
/// The workspace has no real directories — a path with slashes only exists because some
/// file lives under it — so "directory" here means "every file under this prefix", and
/// a transfer copies that set. A failure says how much already happened: this is a
/// filesystem with no transaction, and pretending otherwise would leave the caller
/// guessing what state it is in.
fn transfer(ws: &mut dyn Workspace, args: &Value, move_it: bool) -> Result<String, String> {
    let from = normalize(&req_str(args, "from")?)?;
    let to = normalize(&req_str(args, "to")?)?;
    if from == "/" {
        return Err("`from` must not be the workspace root".to_string());
    }
    if from == to {
        return Err(format!("`from` and `to` are the same path (`{from}`)"));
    }
    if to.starts_with(&format!("{from}/")) {
        return Err(format!("`{to}` is inside `{from}`; that would recurse"));
    }
    if move_it && !confirmed(args) {
        return Err(format!("set confirm=true to move `{from}` (the source is removed)"));
    }

    let mut sources: Vec<String> = Vec::new();
    let mut budget = MAX_FILES;
    if ws.read(&from).is_ok() {
        sources.push(from.clone());
    } else {
        walk(ws, &from, 1, &mut sources, &mut budget);
    }
    if sources.is_empty() {
        return Err(format!("nothing at `{from}`"));
    }

    let total = sources.len();
    let mut files = 0usize;
    let mut bytes = 0usize;
    for path in &sources {
        let text =
            ws.read(path).map_err(|e| format!("reading `{path}` failed: {e} ({files} of {total} done)"))?;
        let target = if path == &from { to.clone() } else { format!("{to}{}", &path[from.len()..]) };
        ws.write(&target, &text).map_err(|e| {
            format!("writing `{target}` failed: {e} ({files} of {total} done, the rest untouched)")
        })?;
        bytes += text.len();
        files += 1;
    }
    if move_it {
        for path in &sources {
            // The copy is already written, so a failed unlink leaves a duplicate rather
            // than losing data; say which one.
            ws.remove(path).map_err(|e| format!("copied to `{to}` but could not remove `{path}`: {e}"))?;
        }
    }
    Ok(to_text(json!({
        "ok": true,
        "from": from,
        "to": to,
        "files": files,
        "bytes": bytes,
        "moved": move_it,
    })))
}

/// Delete one file.
///
/// The workspace had no way to remove anything: a wrong skill or a stale note stayed in
/// every future prompt. It is `confirm: true` because there is no trash and no undo on a
/// board — and because a model that means to rewrite a file should say so.
fn rm(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let path = normalize(&req_str(args, "path")?)?;
    if !confirmed(args) {
        return Err(format!("set confirm=true to delete `{path}`"));
    }
    ws.remove(&path)?;
    Ok(to_text(json!({ "removed": path, "ok": true })))
}

fn ls(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let dir = normalize(&opt_str(args, "path").unwrap_or_else(|| "/".to_string()))?;
    let names = ws.list(&dir)?;
    let mut entries: Vec<Value> = Vec::new();
    for name in names {
        let child = join(&dir, &name);
        match ws.read(&child) {
            Ok(text) => entries.push(json!({
                "name": name, "path": child, "dir": false, "size_bytes": size(&text),
            })),
            // Not readable as a file: it is a directory (see the trait contract).
            Err(_) => entries.push(json!({
                "name": name, "path": child, "dir": true,
            })),
        }
    }
    entries.sort_by(|a, b| {
        let ad = a["dir"].as_bool().unwrap_or(false);
        let bd = b["dir"].as_bool().unwrap_or(false);
        // directories first, then by name
        bd.cmp(&ad).then_with(|| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")))
    });
    Ok(to_text(json!({
        "path": dir,
        "count": entries.len(),
        "entries": entries,
    })))
}

fn read(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let path = normalize(&req_str(args, "path")?)?;
    let text = ws.read(&path)?;
    Ok(to_text(json!({
        "path": path,
        "size_bytes": size(&text),
        "text": text,
    })))
}

fn write(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let path = normalize(&req_str(args, "path")?)?;
    let text = match args.get("text") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => {
            return Err(format!("`text` must be a string, got {}", kind_of(other)));
        }
    };
    if path == "/" {
        return Err("cannot write to `/`: that is the workspace root, not a file".to_string());
    }
    if size(&text) > MAX_FILE_BYTES {
        return Err(format!("file is {} bytes, limit is {MAX_FILE_BYTES} bytes", size(&text)));
    }
    ws.write(&path, &text)?;
    Ok(to_text(json!({
        "ok": true,
        "path": path,
        "size_bytes": size(&text),
    })))
}

fn edit(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let path = normalize(&req_str(args, "path")?)?;
    let find = match args.get("find") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::String(_)) => return Err("`find` must not be empty".to_string()),
        _ => return Err("`find` (non-empty string) is required".to_string()),
    };
    let replace = match args.get("replace") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => {
            return Err(format!("`replace` must be a string, got {}", kind_of(other)));
        }
    };
    let text = ws.read(&path)?;
    let hits = text.matches(&find).count();
    if hits != 1 {
        return Err(if hits == 0 {
            format!("`find` does not occur in {path}; nothing changed")
        } else {
            format!("`find` occurs {hits} times in {path}; make it unique so the edit is unambiguous")
        });
    }
    let updated = text.replacen(&find, &replace, 1);
    if size(&updated) > MAX_FILE_BYTES {
        return Err(format!(
            "edited file would be {} bytes, limit is {MAX_FILE_BYTES} bytes",
            size(&updated)
        ));
    }
    ws.write(&path, &updated)?;
    Ok(to_text(json!({
        "ok": true,
        "path": path,
        "occurrences": hits,
        "size_bytes": size(&updated),
    })))
}

fn grep(ws: &mut dyn Workspace, args: &Value) -> Result<String, String> {
    let pattern = match args.get("pattern") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::String(_)) => return Err("`pattern` must not be empty".to_string()),
        _ => return Err("`pattern` (non-empty string) is required".to_string()),
    };
    let root = normalize(&opt_str(args, "path").unwrap_or_else(|| "/".to_string()))?;

    // `grep -E`: the pattern is compiled once and matched per line. A pattern outside the
    // subset is refused here rather than silently matching nothing.
    let regex = match args.get("regex").and_then(Value::as_bool).unwrap_or(false) {
        true => Some(crate::tools::regex::Pattern::compile(&pattern)?),
        false => None,
    };

    // A file works as the start path too: grep it alone.
    let mut files: Vec<String> = Vec::new();
    let mut budget = MAX_FILES;
    if ws.read(&root).is_ok() {
        files.push(root.clone());
    } else {
        walk(ws, &root, 1, &mut files, &mut budget);
    }

    let mut matches: Vec<Value> = Vec::new();
    let mut files_scanned = 0usize;
    let mut truncated = false;
    for file in &files {
        let text = match ws.read(file) {
            Ok(t) => t,
            Err(_) => continue,
        };
        files_scanned += 1;
        for (idx, line) in text.lines().enumerate() {
            let hit = match regex.as_ref() {
                Some(compiled) => compiled.is_match(line),
                None => line.contains(&pattern),
            };
            if hit {
                if matches.len() >= MAX_MATCHES {
                    truncated = true;
                    break;
                }
                matches.push(json!({
                    "path": file,
                    "line": idx + 1,
                    "text": line,
                }));
            }
        }
        if truncated {
            break;
        }
    }
    Ok(to_text(json!({
        "pattern": pattern,
        "mode": if regex.is_some() { "regex" } else { "literal" },
        "path": root,
        "files_scanned": files_scanned,
        "count": matches.len(),
        "truncated": truncated,
        "matches": matches,
    })))
}

fn walk(ws: &dyn Workspace, dir: &str, depth: usize, out: &mut Vec<String>, budget: &mut usize) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let names = match ws.list(dir) {
        Ok(n) => n,
        Err(_) => return,
    };
    for name in names {
        if *budget == 0 {
            return;
        }
        let child = join(dir, &name);
        if ws.read(&child).is_ok() {
            *budget -= 1;
            out.push(child);
        } else {
            walk(ws, &child, depth + 1, out, budget);
        }
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn to_text(v: Value) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-RAM workspace: flat path -> text map, directories implicit. Mirrors what
    /// the firmware does in NVS, and doubles as the reference for the trait
    /// contract (`read` on a directory is an error).
    #[derive(Default)]
    struct Mem {
        files: BTreeMap<String, String>,
    }

    impl Workspace for Mem {
        fn list(&self, dir: &str) -> Result<Vec<String>, String> {
            let dir = normalize(dir)?;
            let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
            let mut names: Vec<String> = Vec::new();
            for key in self.files.keys() {
                let Some(rest) = key.strip_prefix(&prefix) else { continue };
                let name = rest.split('/').next().unwrap_or(rest);
                if !name.is_empty() && !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
            Ok(names)
        }

        fn read(&self, path: &str) -> Result<String, String> {
            let path = normalize(path)?;
            self.files.get(&path).cloned().ok_or_else(|| format!("no such file: {path}"))
        }

        fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
            let path = normalize(path)?;
            if path == "/" {
                return Err("cannot write to the root".to_string());
            }
            // Parents come for free: the key is the full path.
            self.files.insert(path, text.to_string());
            Ok(())
        }

        fn remove(&mut self, path: &str) -> Result<(), String> {
            let path = normalize(path)?;
            if self.files.remove(&path).is_some() {
                return Ok(());
            }
            let prefix = format!("{path}/");
            let before = self.files.len();
            self.files.retain(|k, _| !k.starts_with(&prefix));
            if self.files.len() == before {
                Err(format!("no such file or directory: {path}"))
            } else {
                Ok(())
            }
        }
    }

    fn run(ws: &mut Mem, name: &str, args: Value) -> Result<Value, String> {
        let text = call(ws, name, &args).ok_or_else(|| format!("not an fs tool: {name}"))??;
        serde_json::from_str(&text).map_err(|e| format!("tool result is not JSON: {e}"))
    }

    #[test]
    fn fs_rm_deletes_one_file_and_asks_first() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/notes/a.txt", "text": "hi"})).unwrap();

        let err = run(&mut ws, "fs_rm", json!({"path": "/notes/a.txt"})).unwrap_err();
        assert!(err.contains("confirm=true"), "{err}");
        assert!(ws.read("/notes/a.txt").is_ok(), "the file went without a confirmation");

        run(&mut ws, "fs_rm", json!({"path": "/notes/a.txt", "confirm": true})).unwrap();
        assert!(ws.read("/notes/a.txt").is_err(), "the file is still there");
        // A second delete says the file is gone rather than pretending it worked.
        assert!(run(&mut ws, "fs_rm", json!({"path": "/notes/a.txt", "confirm": true})).is_err());
        // Path rules still apply: no traversal out of the workspace.
        assert!(run(&mut ws, "fs_rm", json!({"path": "../escape", "confirm": true})).is_err());
    }

    #[test]
    fn fs_cp_copies_a_file_and_a_directory_tree() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/a.txt", "text": "alpha"})).unwrap();
        let out = run(&mut ws, "fs_cp", json!({"from": "/a.txt", "to": "/b.txt"})).unwrap();
        assert_eq!(out["files"], json!(1), "{out}");
        assert_eq!(ws.read("/b.txt").unwrap(), "alpha");
        assert!(ws.read("/a.txt").is_ok(), "a copy must leave the source alone");

        // A directory is "every file under this prefix".
        run(&mut ws, "fs_write", json!({"path": "/skills/pins/SKILL.md", "text": "pins"})).unwrap();
        run(&mut ws, "fs_write", json!({"path": "/skills/pins/notes.md", "text": "low"})).unwrap();
        let out = run(&mut ws, "fs_cp", json!({"from": "/skills/pins", "to": "/skills/copy"})).unwrap();
        assert_eq!(out["files"], json!(2), "{out}");
        assert_eq!(ws.read("/skills/copy/SKILL.md").unwrap(), "pins");
        assert_eq!(ws.read("/skills/copy/notes.md").unwrap(), "low");
    }

    #[test]
    fn fs_mv_moves_a_tree_only_with_a_confirmation() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/skills/pins/SKILL.md", "text": "pins"})).unwrap();

        let err = run(&mut ws, "fs_mv", json!({"from": "/skills/pins", "to": "/skills/moved"})).unwrap_err();
        assert!(err.contains("confirm=true"), "{err}");
        assert!(ws.read("/skills/pins/SKILL.md").is_ok(), "the move happened without a confirmation");

        let out =
            run(&mut ws, "fs_mv", json!({"from": "/skills/pins", "to": "/skills/moved", "confirm": true}))
                .unwrap();
        assert_eq!(out["moved"], json!(true), "{out}");
        assert!(ws.read("/skills/moved/SKILL.md").is_ok());
        assert!(ws.read("/skills/pins/SKILL.md").is_err(), "the source is still there");
    }

    #[test]
    fn a_transfer_refuses_to_recurse_or_to_invent_a_source() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/skills/pins/SKILL.md", "text": "pins"})).unwrap();

        let err =
            run(&mut ws, "fs_cp", json!({"from": "/skills/pins", "to": "/skills/pins/inner"})).unwrap_err();
        assert!(err.contains("recurse"), "{err}");
        let err = run(&mut ws, "fs_cp", json!({"from": "/nope", "to": "/x"})).unwrap_err();
        assert!(err.contains("nothing at"), "{err}");
        let err = run(&mut ws, "fs_cp", json!({"from": "/skills/pins", "to": "/skills/pins"})).unwrap_err();
        assert!(err.contains("same path"), "{err}");
        let err = run(&mut ws, "fs_cp", json!({"from": "/", "to": "/x"})).unwrap_err();
        assert!(err.contains("root"), "{err}");
    }

    // ------------------------------------------------------------ path rules

    #[test]
    fn normalize_adds_root_and_drops_noise() {
        assert_eq!(normalize("/").unwrap(), "/");
        assert_eq!(normalize("").unwrap(), "/");
        assert_eq!(normalize("notes/a.txt").unwrap(), "/notes/a.txt");
        assert_eq!(normalize("/notes//./a.txt").unwrap(), "/notes/a.txt");
        assert_eq!(normalize("  /notes/a.txt  ").unwrap(), "/notes/a.txt");
    }

    #[test]
    fn normalize_rejects_traversal() {
        for bad in ["/../etc/passwd", "..", "notes/../../x", "/a/../b", "/.."] {
            let err = normalize(bad).unwrap_err();
            assert!(err.contains("traversal"), "{bad} -> {err}");
        }
    }

    #[test]
    fn tools_reject_traversal_and_bad_separators() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/notes/a.txt", "text": "hi"})).unwrap();
        for args in [json!({"path": "/../etc/passwd"}), json!({"path": "/notes/../../etc/passwd"})] {
            let err = run(&mut ws, "fs_read", args).unwrap_err();
            assert!(err.contains("traversal"), "{err}");
        }
        let err = run(&mut ws, "fs_write", json!({"path": "../escape.txt", "text": "x"})).unwrap_err();
        assert!(err.contains("traversal"), "{err}");
        let err = run(&mut ws, "fs_ls", json!({"path": "/notes\\..\\x"})).unwrap_err();
        assert!(err.contains("separator"), "{err}");
        assert_eq!(
            ws.files.keys().cloned().collect::<Vec<_>>(),
            vec!["/notes/a.txt".to_string()],
            "a rejected path must not create or remove anything"
        );
    }

    // -------------------------------------------------------------- round trip

    #[test]
    fn write_read_list_remove_round_trip() {
        let mut ws = Mem::default();

        let w =
            run(&mut ws, "fs_write", json!({"path": "notes/a.txt", "text": "line one\nline two"})).unwrap();
        assert_eq!(w["ok"], true);
        assert_eq!(w["path"], "/notes/a.txt");
        assert_eq!(w["size_bytes"], 17);

        let r = run(&mut ws, "fs_read", json!({"path": "/notes/a.txt"})).unwrap();
        assert_eq!(r["text"], "line one\nline two");
        assert_eq!(r["size_bytes"], 17);

        // Parents are implicit, so the listing shows the directory at the root.
        let root = run(&mut ws, "fs_ls", json!({})).unwrap();
        assert_eq!(root["path"], "/");
        assert_eq!(root["count"], 1);
        assert_eq!(root["entries"][0]["name"], "notes");
        assert_eq!(root["entries"][0]["dir"], true);

        let dir = run(&mut ws, "fs_ls", json!({"path": "/notes"})).unwrap();
        assert_eq!(dir["entries"][0]["name"], "a.txt");
        assert_eq!(dir["entries"][0]["dir"], false);
        assert_eq!(dir["entries"][0]["size_bytes"], 17);

        ws.remove("/notes").unwrap();
        let err = run(&mut ws, "fs_read", json!({"path": "/notes/a.txt"})).unwrap_err();
        assert!(err.contains("no such file"), "{err}");
        assert_eq!(run(&mut ws, "fs_ls", json!({})).unwrap()["count"], 0);
    }

    #[test]
    fn write_enforces_the_size_limit() {
        let mut ws = Mem::default();
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        let err = run(&mut ws, "fs_write", json!({"path": "/big.txt", "text": big})).unwrap_err();
        assert!(err.contains(&format!("limit is {MAX_FILE_BYTES}")), "{err}");
        assert!(ws.files.is_empty());

        let ok = "x".repeat(MAX_FILE_BYTES);
        run(&mut ws, "fs_write", json!({"path": "/big.txt", "text": ok})).unwrap();
    }

    #[test]
    fn missing_arguments_name_the_field() {
        let mut ws = Mem::default();
        assert!(run(&mut ws, "fs_read", json!({})).unwrap_err().contains("`path`"));
        // `text` is optional and defaults to empty, so this creates an empty file.
        let empty = run(&mut ws, "fs_write", json!({"path": "/a.txt"})).unwrap();
        assert_eq!(empty["ok"], true);
        assert_eq!(empty["size_bytes"], 0);
        assert!(run(&mut ws, "fs_edit", json!({"path": "/a.txt", "replace": "b"}))
            .unwrap_err()
            .contains("`find`"));
    }

    // ------------------------------------------------------------------- edit

    #[test]
    fn edit_requires_exactly_one_match() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/n.txt", "text": "a=1\nb=2\na=3"})).unwrap();

        let err =
            run(&mut ws, "fs_edit", json!({"path": "/n.txt", "find": "a=", "replace": "c="})).unwrap_err();
        assert!(err.contains("2 times"), "{err}");
        // The file is untouched after a rejected edit.
        assert_eq!(ws.read("/n.txt").unwrap(), "a=1\nb=2\na=3");

        let ok = run(&mut ws, "fs_edit", json!({"path": "/n.txt", "find": "b=2", "replace": "b=9"})).unwrap();
        assert_eq!(ok["occurrences"], 1);
        assert_eq!(ws.read("/n.txt").unwrap(), "a=1\nb=9\na=3");

        let err =
            run(&mut ws, "fs_edit", json!({"path": "/n.txt", "find": "zzz", "replace": "y"})).unwrap_err();
        assert!(err.contains("does not occur"), "{err}");
        assert_eq!(ws.read("/n.txt").unwrap(), "a=1\nb=9\na=3");
    }

    #[test]
    fn edit_on_missing_file_is_an_error() {
        let mut ws = Mem::default();
        let err =
            run(&mut ws, "fs_edit", json!({"path": "/nope.txt", "find": "a", "replace": "b"})).unwrap_err();
        assert!(err.contains("no such file"), "{err}");
    }

    // ------------------------------------------------------------------- grep

    #[test]
    fn grep_walks_the_tree_with_a_literal_match() {
        let mut ws = Mem::default();
        run(&mut ws, "fs_write", json!({"path": "/notes/a.txt", "text": "alpha\nbeta"})).unwrap();
        run(&mut ws, "fs_write", json!({"path": "/notes/deep/b.txt", "text": "gamma alpha\nALPHA"})).unwrap();
        run(&mut ws, "fs_write", json!({"path": "/log.txt", "text": "nothing here"})).unwrap();

        let all = run(&mut ws, "fs_grep", json!({"pattern": "alpha"})).unwrap();
        assert_eq!(all["pattern"], "alpha");
        assert_eq!(all["count"], 2);
        assert_eq!(all["files_scanned"], 3);
        let mut hits: Vec<(String, u64)> = all["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["path"].as_str().unwrap().to_string(), m["line"].as_u64().unwrap()))
            .collect();
        hits.sort();
        assert_eq!(hits, vec![("/notes/a.txt".to_string(), 1), ("/notes/deep/b.txt".to_string(), 1)]);

        // Case-sensitive literal match: `ALPHA` is not `alpha`.
        let upper = run(&mut ws, "fs_grep", json!({"pattern": "ALPHA"})).unwrap();
        assert_eq!(upper["count"], 1);
        assert_eq!(upper["matches"][0]["line"], 2);

        // Scoped to a directory.
        let scoped = run(&mut ws, "fs_grep", json!({"pattern": "alpha", "path": "/notes/a.txt"})).unwrap();
        assert_eq!(scoped["count"], 1);
        assert_eq!(scoped["files_scanned"], 1);

        // A literal `.` is not a regex wildcard.
        let dot = run(&mut ws, "fs_grep", json!({"pattern": "a.txt"})).unwrap();
        assert_eq!(dot["count"], 0);
    }

    // ------------------------------------------------------------------ specs

    #[test]
    fn specs_are_openai_tools_and_call_rejects_foreign_names() {
        let specs = specs();
        let list = specs.as_array().expect("tools array");
        let names: Vec<&str> = list.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["fs_ls", "fs_read", "fs_write", "fs_edit", "fs_rm", "fs_cp", "fs_mv", "fs_grep"]
        );
        for t in list {
            assert_eq!(t["type"], "function");
            assert_eq!(t["function"]["parameters"]["type"], "object");
        }
        let mut ws = Mem::default();
        assert!(call(&mut ws, "net_get", &json!({})).is_none());
        assert!(call(&mut ws, "fs_read", &json!({"path": "/a"})).is_some());
    }
}

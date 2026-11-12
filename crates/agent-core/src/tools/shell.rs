//! One shell, many small commands.
//!
//! Twelve separate tools (`fs_ls`, `fs_read`, … `net_tcp`) is twelve schema blocks in every
//! request and twelve names for the model to keep straight, for a set of operations that
//! read as one command line. They are one tool now: `shell {op, …}`, where `op` picks the
//! command and the remaining fields are its arguments — the same shape as the filesystem
//! they act on.
//!
//! The commands themselves are unchanged: `shell` is a router in front of the handlers that
//! already existed, so a script that calls `tool("fs_read", …)` directly still works.

use crate::tool::{object_schema, string, ToolDef};
use serde_json::{json, Value};

/// Every `op`, with the handler it stands for.
///
/// Reading, writing and editing a file are their own tools, and the network commands are
/// `curl`/`wget`/`nslookup`/`nc`: those are the names people already use, and a model
/// knows what they mean without reading a description. What stays here is what has no
/// familiar single-command equivalent on a board with one storage area.
pub const OPS: &[(&str, &str)] = &[
    ("ls", "fs_ls"),
    ("rm", "fs_rm"),
    ("cp", "fs_cp"),
    ("mv", "fs_mv"),
    ("grep", "fs_grep"),
    ("zip", "shell_zip"),
    ("unzip", "shell_unzip"),
    // The commands a shell has for looking at itself. NuttX's `nsh` has all of these and
    // they cost nothing here: the numbers are already in the app.
    ("curl", "shell_curl"),
    ("wget", "shell_wget"),
    ("nslookup", "net_dns"),
    ("nc", "net_tcp"),
    ("ping", "shell_ping"),
    ("ifconfig", "shell_ifconfig"),
    ("df", "shell_df"),
    ("free", "shell_free"),
    ("uptime", "shell_uptime"),
    ("date", "shell_date"),
    ("uname", "shell_uname"),
    ("ps", "shell_ps"),
    ("hexdump", "shell_hexdump"),
    ("xd", "shell_hexdump"),
    ("base64enc", "shell_base64"),
    ("base64dec", "shell_base64"),
    ("sha256sum", "shell_sha256sum"),
    ("md5sum", "shell_md5sum"),
    ("system_prompt", "shell_system_prompt"),
];

/// One command line, split the way a shell splits it: words, with quotes respected.
pub struct Parsed {
    pub name: String,
    pub args: Vec<String>,
    /// `--flag`, `-f value` and `--flag=value` both land here.
    pub flags: std::collections::BTreeMap<String, String>,
}

/// Split a command line into a name, positional arguments and flags.
///
/// The grammar is the one everybody already types — `ls -la /notes`, `curl -f raw URL`,
/// `ping -c 3 1.1.1.1` — because the point of a shell is that the model does not have to
/// learn a new calling convention to use it. Quotes group a word; a backslash escapes the
/// next character.
pub fn parse(line: &str) -> Result<Parsed, String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in line.trim().chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' | '\'' => match quote {
                Some(open) if open == ch => quote = None,
                Some(_) => current.push(ch),
                None => quote = Some(ch),
            },
            c if c.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if quote.is_some() {
        return Err("unbalanced quote in the command".into());
    }
    if !current.is_empty() {
        words.push(current);
    }
    let mut it = words.into_iter();
    let name = it.next().ok_or("empty command")?.to_ascii_lowercase();
    let rest: Vec<String> = it.collect();

    let mut args = Vec::new();
    let mut flags = std::collections::BTreeMap::new();
    let mut index = 0;
    while index < rest.len() {
        let word = &rest[index];
        if let Some(stripped) = word.strip_prefix("--") {
            match stripped.split_once('=') {
                Some((key, value)) => {
                    flags.insert(key.to_ascii_lowercase(), value.to_string());
                }
                None => {
                    let key = stripped.to_ascii_lowercase();
                    // A flag takes the next word as its value unless that word is another flag.
                    if let Some(next) = rest.get(index + 1) {
                        if !next.starts_with('-') {
                            flags.insert(key, next.clone());
                            index += 1;
                        } else {
                            flags.insert(key, "true".into());
                        }
                    } else {
                        flags.insert(key, "true".into());
                    }
                }
            }
        } else if word.len() > 1 && word.starts_with('-') && word != "-" {
            // Short flags: `-d`, and `-o value` for the ones that take one.
            let key = word[1..].to_ascii_lowercase();
            let takes_value = matches!(key.as_str(), "o" | "p" | "c" | "w" | "f");
            if takes_value {
                if let Some(next) = rest.get(index + 1) {
                    flags.insert(key, next.clone());
                    index += 1;
                }
            } else {
                flags.insert(key, "true".into());
            }
        } else {
            args.push(word.clone());
        }
        index += 1;
    }
    Ok(Parsed { name, args, flags })
}

/// Turn a parsed line into the argument object the handler for that command takes.
///
/// One place, so the command line and the JSON form cannot mean different things.
pub fn arguments(parsed: &Parsed) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let arg = |n: usize| parsed.args.get(n).cloned();
    let need = |n: usize, what: &str| -> Result<String, String> {
        arg(n).ok_or_else(|| format!("{}: missing {what}", parsed.name))
    };
    let flag = |long: &str, short: &str| parsed.flags.get(long).or_else(|| parsed.flags.get(short)).cloned();
    let flag_bool = |long: &str, short: &str| flag(long, short).map(|v| v == "true").unwrap_or(false);

    Ok(match parsed.name.as_str() {
        "ls" => json!({ "path": arg(0).unwrap_or_else(|| "/".into()) }),
        "rm" => {
            json!({ "path": need(0, "<path>")?, "confirm": flag_bool("confirm", "y") || flag_bool("force", "f") })
        }
        "cp" => json!({ "from": need(0, "<from>")?, "to": need(1, "<to>")? }),
        "mv" => {
            json!({ "from": need(0, "<from>")?, "to": need(1, "<to>")?, "confirm": flag_bool("confirm", "y") })
        }
        "grep" => json!({ "pattern": need(0, "<pattern>")?, "path": arg(1).unwrap_or_else(|| "/".into()) }),
        "zip" => {
            let name = need(0, "<archive>")?;
            let mut files = serde_json::Map::new();
            for path in parsed.args.iter().skip(1) {
                files.insert(path.trim_start_matches('/').to_string(), json!(""));
            }
            json!({ "name": name, "paths": parsed.args.iter().skip(1).collect::<Vec<_>>(), "files": files })
        }
        "unzip" => json!({ "archive_b64": need(0, "<archive_b64>")? }),
        "hexdump" | "xd" => json!({ "path": need(0, "<path>")?, "width": flag("width", "w") }),
        "base64" | "base64enc" | "base64dec" => json!({
            "path": need(0, "<path>")?,
            "decode": parsed.name == "base64dec" || flag_bool("decode", "d"),
            "out": flag("out", "o"),
        }),
        "sha256sum" | "md5sum" => json!({ "path": need(0, "<path>")? }),
        "system_prompt" => {
            let action = arg(0).unwrap_or_else(|| "get".into());
            let text = parsed.args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            json!({ "action": action, "text": text })
        }
        "curl" => json!({
            "url": need(0, "<url>")?,
            "data": flag("data", "d"),
            "format": flag("format", "f"),
        }),
        "wget" => json!({ "url": need(0, "<url>")?, "path": flag("output", "o") }),
        "nslookup" => json!({ "host": need(0, "<host>")?, "port": flag("port", "p") }),
        "nc" => {
            json!({ "host": need(0, "<host>")?, "port": flag("port", "p"), "timeout_ms": flag("timeout", "t") })
        }
        "ping" => json!({ "host": need(0, "<host>")?, "count": flag("count", "c") }),
        "ifconfig" | "df" | "free" | "uptime" | "date" | "uname" | "ps" | "help" => json!({}),
        other => return Err(format!("`{other}` is not a shell command.\n\n{}", help_text())),
    })
}

/// How each command is written, in the order it takes its arguments.
///
/// This exists because a model that gets a command wrong needs the correction in the
/// error, not in a document it must go and fetch: `rm` without `confirm` answers with
/// `usage: rm <path> --confirm`, and `shell help` lists every line.
pub fn usage(op: &str) -> Option<&'static str> {
    Some(match op {
        "ls" => "ls [path]",
        "rm" => "rm <path> --confirm",
        "cp" => "cp <from> <to>",
        "mv" => "mv <from> <to> --confirm",
        "grep" => "grep <pattern> [path]",
        "zip" => "zip <archive> <path...>",
        "unzip" => "unzip <archive_b64>",
        "df" => "df",
        "free" => "free",
        "uptime" => "uptime",
        "date" => "date",
        "uname" => "uname",
        "ps" => "ps",
        "hexdump" => "hexdump <path> [-w <bytes per line>]",
        "xd" => "xd <path> [-w <bytes per line>]  (same as hexdump)",
        "base64enc" => "base64enc <path> [-o <out>]",
        "base64dec" => "base64dec <path> [-o <out>]",
        "sha256sum" => "sha256sum <path>",
        "md5sum" => "md5sum <path>",
        "system_prompt" => "system_prompt get | set <text> | append <text> | clear",
        // The commands that are tools of their own. They share the table so `--help` and
        // the error suffix say the same thing wherever the model reaches them.
        "read" => "read <path>",
        "write" => "write <path> <text>",
        "edit" => "edit <path> <find> <replace>",
        "curl" => "curl <url> [-d <body>] [-f text|raw]",
        "wget" => "wget <url> [-o <path>] [-f text|raw]",
        "nslookup" => "nslookup <host> [-p <port>]",
        "nc" => "nc <host> [-p <port>] [-t <ms>]",
        "ping" => "ping <host> [-c <count>]",
        "ifconfig" => "ifconfig",
        _ => return None,
    })
}

/// Every command with its usage, for `nsh {"command":"help"}`.
pub fn help_text() -> String {
    let mut out = String::from("shell commands:\n");
    for (op, _) in OPS {
        if let Some(line) = usage(op) {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("\nSeparate tools: read <path> · write <path> <text> · edit <path> <find> <replace>");
    out
}

/// The handler a command maps to, or `None` for an operator nobody has.
pub fn route(op: &str) -> Option<&'static str> {
    let op = op.trim().to_ascii_lowercase();
    OPS.iter().find(|(name, _)| *name == op).map(|(_, tool)| *tool)
}

/// The one tool definition the model and MCP clients see.
pub fn spec() -> ToolDef {
    ToolDef::new(
        "nsh",
        "Run a command on the board, as on any unix shell: one `command` string.\n\
         ls [path] · rm <path> --confirm · cp <from> <to> · mv <from> <to> --confirm · grep <pattern> [path]\n\
         curl <url> [-d body] [-f text|raw] · wget <url> [-o path] · nslookup <host> · nc <host> [-p port] · ping <host> [-c n] · ifconfig\n\
         zip <archive> <path...> · unzip <archive_b64> · hexdump <path> [-w n] · base64 <path> [-d] [-o out]\n\
         df · free · uptime · date · uname · ps · help\n\
         `curl` reduces an HTML page to its text unless -f raw. A wrong command answers with its usage; `shell help` lists everything.",
    )
    .title("NuttX shell")
    .category("system")
    .schema(object_schema(
        &[
            ("command", string("the command line, e.g. `ls /notes`, `curl https://example.com/`, `ping -c 3 1.1.1.1`")),
            ("path", string("file or directory, for ls/rm/grep/hexdump/base64")),
            ("decode", json!({ "type": "boolean", "description": "for base64: decode instead of encode" })),
            ("out", string("for base64: where to write the result, default is the return value")),
            ("width", json!({ "type": "integer", "minimum": 4, "maximum": 32, "description": "for hexdump: bytes per line, default 16" })),
            ("from", string("source path, for cp/mv")),
            ("to", string("destination path, for cp/mv")),
            ("confirm", json!({ "type": "boolean", "description": "must be true for rm and mv" })),
            ("pattern", string("literal substring, for grep")),
            ("name", string("archive name, for zip")),
            ("files", json!({ "type": "object", "description": "for zip: {name: text} entries to pack" })),
            ("archive_b64", string("for unzip: the archive, base64")),
            ("help", json!({ "type": "boolean", "description": "return the usage lines instead of running anything" })),
        ],
        &[],
    ))
}

/// The same tool in the OpenAI `tools` shape, which is what the model, the MCP list and
/// the portal all read.
pub fn openai_spec() -> Value {
    let def = spec();
    json!({
        "type": "function",
        "function": {
            "name": def.spec.name,
            "description": def.spec.description,
            "parameters": def.spec.input_schema,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_routes_and_unknown_ops_do_not() {
        for (op, tool) in OPS {
            assert_eq!(route(op), Some(*tool), "{op}");
            assert_eq!(route(&op.to_uppercase()), Some(*tool), "{op} is case-insensitive");
        }
        assert_eq!(route("sudo"), None);
        let spec = spec();
        assert_eq!(spec.spec.name, "nsh");
        assert!(spec.spec.input_schema["properties"]["command"].is_object());
    }
}

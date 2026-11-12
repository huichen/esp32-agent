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
            // Short flags: `-d`, and `-o value` for the ones that take one. Every short
            // flag the usage lines document with an argument after it has to be in this
            // list: `-d <body>` and `-t <ms>` were missing, so `curl url -d hello` set
            // `d` to "true", dropped `hello` into the positional arguments and left the
            // body empty.
            //
            // The list is a hint, not a rule: the same letter is a switch in one command
            // and a value in another (`base64 -d` decodes, `curl -d body` sends), so a
            // flag with no word after it is a boolean. A command line is read left to
            // right and only `--flag=value` is unambiguous, which is what a shell does too.
            let key = word[1..].to_ascii_lowercase();
            let takes_value = matches!(key.as_str(), "o" | "p" | "c" | "w" | "f" | "d" | "t");
            match (takes_value, rest.get(index + 1)) {
                (true, Some(next)) => {
                    flags.insert(key, next.clone());
                    index += 1;
                }
                _ => {
                    flags.insert(key, "true".into());
                }
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

/// Every command with its usage, for `shell {"command":"help"}`.
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
        "shell",
        "Run a command on the board, as on any unix shell: one `command` string.\n\
         ls [path] · rm <path> --confirm · cp <from> <to> · mv <from> <to> --confirm · grep <pattern> [path]\n\
         curl <url> [-d body] [-f text|raw] · wget <url> [-o path] · nslookup <host> · nc <host> [-p port] · ping <host> [-c n] · ifconfig\n\
         zip <archive> <path...> · unzip <archive_b64> · hexdump <path> [-w n] · base64 <path> [-d] [-o out]\n\
         df · free · uptime · date · uname · ps · help\n\
         `curl` reduces an HTML page to its text unless -f raw. A wrong command answers with its usage; `shell help` lists everything.",
    )
    .title("Shell")
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
        assert_eq!(route("  LS  "), Some("fs_ls"), "the router trims, because a model types spaces");
        assert_eq!(route("sudo"), None);
        let spec = spec();
        assert_eq!(spec.spec.name, "shell");
        assert!(spec.spec.input_schema["properties"]["command"].is_object());
    }

    fn parsed(line: &str) -> Parsed {
        parse(line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
    }

    fn args_of(line: &str) -> Value {
        arguments(&parsed(line)).unwrap_or_else(|e| panic!("{line:?}: {e}"))
    }

    fn err_of(line: &str) -> String {
        let p = parsed(line);
        match arguments(&p) {
            Ok(v) => panic!("{line:?} should have been refused, got {v}"),
            Err(e) => e,
        }
    }

    /// The grammar is the one a person already types: words, quotes that group, a backslash
    /// that escapes, and flags in either `--flag value` or `--flag=value` form.
    #[test]
    fn a_command_line_is_split_like_a_shell_splits_it() {
        let p = parsed("  ls -la /notes  ");
        assert_eq!(p.name, "ls");
        assert_eq!(p.args, vec!["/notes"]);
        assert_eq!(p.flags["la"], "true");

        let p = parsed("GREP \"hello world\" /notes");
        assert_eq!(p.name, "grep", "the command name is lowercased");
        assert_eq!(p.args, vec!["hello world", "/notes"], "a quoted word is one argument");

        let p = parsed(r#"grep 'say "hi"' /x"#);
        assert_eq!(p.args[0], r#"say "hi""#, "the other quote character is literal inside");

        let p = parsed(r"grep hello\ world /x");
        assert_eq!(p.args, vec!["hello world", "/x"], "a backslash escapes the space");

        let p = parsed(r"grep a\\b /x");
        assert_eq!(p.args[0], r"a\b", "an escaped backslash is one backslash");
    }

    #[test]
    fn flags_take_a_value_or_a_boolean_and_long_flags_accept_equals() {
        let p = parsed("curl --format=raw https://example.com/");
        assert_eq!(p.flags["format"], "raw");
        assert_eq!(p.args, vec!["https://example.com/"]);

        let p = parsed("ping --count 3 1.1.1.1");
        assert_eq!(p.flags["count"], "3");
        assert_eq!(p.args, vec!["1.1.1.1"], "a flag's value is not a positional argument");

        // A flag followed by another flag has no value of its own.
        let p = parsed("curl --raw --data hello url");
        assert_eq!(p.flags["raw"], "true");
        assert_eq!(p.flags["data"], "hello");

        // And a flag at the end of the line.
        let p = parsed("ls --all");
        assert_eq!(p.flags["all"], "true");

        // Short flags that take a value, per the usage lines.
        let p = parsed("hexdump /notes/a.txt -w 8");
        assert_eq!(p.flags["w"], "8");
        let p = parsed("curl https://example.com/ -d hello");
        assert_eq!(p.flags["d"], "hello", "-d takes the body");
        assert_eq!(p.args, vec!["https://example.com/"]);
        let p = parsed("nc 1.1.1.1 -p 80 -t 500");
        assert_eq!(p.flags["p"], "80");
        assert_eq!(p.flags["t"], "500", "-t takes the timeout");
        let p = parsed("base64enc /a.txt -o /a.b64");
        assert_eq!(p.flags["o"], "/a.b64");

        // A short flag with no value is a switch.
        let p = parsed("base64dec /a.b64 -d");
        assert_eq!(p.flags["d"], "true");

        // A bare dash is a word, not a flag.
        let p = parsed("grep - /notes");
        assert_eq!(p.args, vec!["-", "/notes"]);
    }

    #[test]
    fn an_unbalanced_quote_or_an_empty_line_is_refused() {
        let err = |line: &str| match parse(line) {
            Ok(_) => panic!("{line:?} should not have parsed"),
            Err(e) => e,
        };
        assert!(err("grep \"unterminated").contains("unbalanced quote"));
        assert!(err("grep 'unterminated").contains("unbalanced quote"));
        assert!(err("   ").contains("empty command"));
        assert!(err("").contains("empty command"));
    }

    /// Every command's usage line, because it is what the model is told when it gets the
    /// command wrong. A command without one is a command whose error message is empty.
    #[test]
    fn every_op_has_a_usage_line_and_help_lists_them_all() {
        for (op, _) in OPS {
            let line = usage(op).unwrap_or_else(|| panic!("{op} has no usage line"));
            assert!(!line.is_empty());
        }
        assert_eq!(usage("sudo"), None);
        // The commands that are tools of their own are in the table too, so the same
        // sentence comes back whichever entry point refuses.
        for tool in ["read", "write", "edit", "curl", "wget", "nslookup", "nc", "ping", "ifconfig"] {
            assert!(usage(tool).is_some(), "{tool}");
        }

        let help = help_text();
        for (op, _) in OPS {
            let line = usage(op).expect("checked above");
            assert!(help.contains(line), "help is missing `{line}`");
        }
        assert!(help.contains("Separate tools"), "help must point at read/write/edit: {help}");
        assert!(help.starts_with("shell commands:\n"));
    }

    /// Each command maps to the argument object its handler takes, in one place, so the
    /// command line and the JSON form cannot drift apart.
    #[test]
    fn each_command_builds_the_arguments_its_handler_expects() {
        assert_eq!(args_of("ls"), json!({ "path": "/" }), "ls defaults to the root");
        assert_eq!(args_of("ls /notes"), json!({ "path": "/notes" }));
        assert_eq!(args_of("grep TODO"), json!({ "pattern": "TODO", "path": "/" }));
        assert_eq!(args_of("grep TODO /notes"), json!({ "pattern": "TODO", "path": "/notes" }));
        assert_eq!(args_of("cp /a /b"), json!({ "from": "/a", "to": "/b" }));
        assert_eq!(args_of("mv /a /b --confirm"), json!({ "from": "/a", "to": "/b", "confirm": true }));
        assert_eq!(args_of("mv /a /b -y"), json!({ "from": "/a", "to": "/b", "confirm": true }));
        assert_eq!(
            args_of("rm /a"),
            json!({ "path": "/a", "confirm": false }),
            "rm needs no confirm to parse"
        );
        assert_eq!(args_of("rm /a -f"), json!({ "path": "/a", "confirm": true }));
        assert_eq!(args_of("unzip AAAA"), json!({ "archive_b64": "AAAA" }));
        assert_eq!(args_of("hexdump /a -w 8"), json!({ "path": "/a", "width": "8" }));
        assert_eq!(args_of("xd /a"), json!({ "path": "/a", "width": null }));
        assert_eq!(args_of("base64enc /a"), json!({ "path": "/a", "decode": false, "out": null }));
        assert_eq!(args_of("base64dec /a -o /b"), json!({ "path": "/a", "decode": true, "out": "/b" }));
        assert_eq!(args_of("base64 /a -d"), json!({ "path": "/a", "decode": true, "out": null }));
        assert_eq!(args_of("sha256sum /a"), json!({ "path": "/a" }));
        assert_eq!(args_of("md5sum /a"), json!({ "path": "/a" }));
        assert_eq!(
            args_of("curl https://e.com/ -d hi -f raw"),
            json!({
                "url": "https://e.com/", "data": "hi", "format": "raw"
            })
        );
        assert_eq!(args_of("wget https://e.com/ -o /a"), json!({ "url": "https://e.com/", "path": "/a" }));
        assert_eq!(args_of("nslookup example.com -p 53"), json!({ "host": "example.com", "port": "53" }));
        assert_eq!(
            args_of("nc 1.1.1.1 -p 80 -t 500"),
            json!({
                "host": "1.1.1.1", "port": "80", "timeout_ms": "500"
            })
        );
        assert_eq!(args_of("ping -c 3 1.1.1.1"), json!({ "host": "1.1.1.1", "count": "3" }));
        for op in ["ifconfig", "df", "free", "uptime", "date", "uname", "ps", "help"] {
            assert_eq!(args_of(op), json!({}), "{op}");
        }

        // A zip's entries are the paths after the archive, with the leading slash removed
        // because an archive stores names, not absolute paths.
        let v = args_of("zip /notes.zip /a.txt /dir/b.md");
        assert_eq!(v["name"], "/notes.zip");
        assert_eq!(v["paths"], json!(["/a.txt", "/dir/b.md"]));
        assert_eq!(v["files"], json!({ "a.txt": "", "dir/b.md": "" }));

        // The system prompt is the one command whose argument is free text.
        assert_eq!(args_of("system_prompt"), json!({ "action": "get", "text": "" }));
        assert_eq!(args_of("system_prompt set be brief"), json!({ "action": "set", "text": "be brief" }));
        assert_eq!(
            args_of("system_prompt append and precise"),
            json!({ "action": "append", "text": "and precise" })
        );
    }

    /// A command missing its argument names the command and the argument it wanted, and
    /// nothing else: that sentence is the whole correction the model gets.
    #[test]
    fn a_command_without_its_argument_says_which_one_is_missing() {
        for (line, want) in [
            ("rm", "rm: missing <path>"),
            ("cp /a", "cp: missing <to>"),
            ("mv /a", "mv: missing <to>"),
            ("grep", "grep: missing <pattern>"),
            ("zip", "zip: missing <archive>"),
            ("unzip", "unzip: missing <archive_b64>"),
            ("hexdump", "hexdump: missing <path>"),
            ("xd", "xd: missing <path>"),
            ("base64enc", "base64enc: missing <path>"),
            ("base64dec", "base64dec: missing <path>"),
            ("sha256sum", "sha256sum: missing <path>"),
            ("md5sum", "md5sum: missing <path>"),
            ("curl", "curl: missing <url>"),
            ("wget", "wget: missing <url>"),
            ("nslookup", "nslookup: missing <host>"),
            ("nc", "nc: missing <host>"),
            ("ping", "ping: missing <host>"),
            ("cp /a", "cp: missing <to>"),
        ] {
            assert_eq!(err_of(line), want);
        }
    }

    /// An unknown command answers with the whole table rather than with a guess, because a
    /// model that mistyped one needs the list to pick from.
    #[test]
    fn an_unknown_command_lists_what_there_is() {
        let err = err_of("sudo rm -rf /");
        assert!(err.starts_with("`sudo` is not a shell command."), "{err}");
        assert!(err.contains("ls [path]"), "the error must carry the table: {err}");
        assert!(err.contains("Separate tools"));
    }

    #[test]
    fn the_openai_shaped_spec_matches_the_tool_definition() {
        let def = spec();
        let v = openai_spec();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], def.spec.name);
        assert_eq!(v["function"]["description"], def.spec.description);
        assert_eq!(v["function"]["parameters"], def.spec.input_schema);
        assert!(v["function"]["parameters"]["properties"]["command"].is_object());
        assert_eq!(def.spec.title, "Shell");
        assert_eq!(def.spec.category, "system");
    }
}

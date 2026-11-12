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

/// Every `op`, in the order the description lists them, with the tool it stands for.
pub const OPS: &[(&str, &str)] = &[
    ("ls", "fs_ls"),
    ("cat", "fs_read"),
    ("write", "fs_write"),
    ("edit", "fs_edit"),
    ("rm", "fs_rm"),
    ("cp", "fs_cp"),
    ("mv", "fs_mv"),
    ("grep", "fs_grep"),
    ("get", "net_get"),
    ("post", "net_post"),
    ("dns", "net_dns"),
    ("tcp", "net_tcp"),
    ("zip", "shell_zip"),
    ("unzip", "shell_unzip"),
];

/// The handler a command maps to, or `None` for an operator nobody has.
pub fn route(op: &str) -> Option<&'static str> {
    let op = op.trim().to_ascii_lowercase();
    OPS.iter().find(|(name, _)| *name == op).map(|(_, tool)| *tool)
}

/// The one tool definition the model and MCP clients see.
pub fn spec() -> ToolDef {
    ToolDef::new(
        "shell",
        "The board's command line, as one tool. `op` picks the command and the other fields are its arguments:\n\
         ls{path} · cat{path} · write{path,text} · edit{path,find,replace} · rm{path,confirm} · cp{from,to} · mv{from,to,confirm}\n\
         grep{pattern,path} · get{url,format} · post{url,body} · dns{host,port} · tcp{host,port,timeout_ms}\n\
         zip{name,files} · unzip{archive_b64}\n\
         `get` reduces an HTML page to its text unless format=raw. `zip` writes a stored archive and returns it as base64; `unzip` reads stored or deflated archives.",
    )
    .title("Shell")
    .category("system")
    .schema(object_schema(
        &[
            ("op", string("the command: ls, cat, write, edit, rm, cp, mv, grep, get, post, dns, tcp, zip, unzip")),
            ("path", string("file or directory, for ls/cat/write/edit/rm/grep")),
            ("text", string("contents, for write")),
            ("find", string("literal text, must occur exactly once, for edit")),
            ("replace", string("replacement, for edit")),
            ("from", string("source path, for cp/mv")),
            ("to", string("destination path, for cp/mv")),
            ("confirm", json!({ "type": "boolean", "description": "must be true for rm and mv" })),
            ("pattern", string("literal substring, for grep")),
            ("url", string("http:// or https:// URL, for get/post")),
            ("format", string("text (default) or raw, for get")),
            ("body", string("request body, for post")),
            ("host", string("hostname or address, for dns/tcp")),
            ("port", json!({ "type": "integer", "minimum": 1, "maximum": 65535, "description": "port, for dns/tcp" })),
            ("timeout_ms", json!({ "type": "integer", "minimum": 100, "maximum": 20000, "description": "for tcp, default 3000" })),
            ("name", string("archive name, for zip")),
            ("files", json!({ "type": "object", "description": "for zip: {name: text} entries to pack" })),
            ("archive_b64", string("for unzip: the archive, base64")),
        ],
        &["op"],
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
        assert_eq!(spec.spec.name, "shell");
        assert!(spec.spec.input_schema["properties"]["op"].is_object());
    }
}

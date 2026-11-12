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
        "Workspace commands that have no tool of their own. `op` picks the command and the other fields are its arguments:\n\
         ls{path} · rm{path,confirm} · cp{from,to} · mv{from,to,confirm} · grep{pattern,path} · zip{name,files} · unzip{archive_b64}\n\
         Read, write and edit a file with the `read`, `write` and `edit` tools; fetch a URL with `curl` or `wget`. `zip` writes a stored archive and returns it as base64; `unzip` reads stored or deflated archives.",
    )
    .title("Shell")
    .category("system")
    .schema(object_schema(
        &[
            ("op", string("the command: ls, rm, cp, mv, grep, zip, unzip")),
            ("path", string("file or directory, for ls/rm/grep")),
            ("from", string("source path, for cp/mv")),
            ("to", string("destination path, for cp/mv")),
            ("confirm", json!({ "type": "boolean", "description": "must be true for rm and mv" })),
            ("pattern", string("literal substring, for grep")),
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

//! One tool, several commands: the shape `shell` uses, made available to a family.
//!
//! The problem this solves is the size of `tools` in every request. A board with five
//! devices had 48 tool declarations and 25 KB of schema, and most of that was five variants
//! of the same operation spelled out one after another. Declarations are paid for on every
//! step of a run, so the win is not "fewer names" — it is 4 KB of schema that is no longer
//! sent, and 4 KB fewer tokens to invalidate a provider's prefix cache with.
//!
//! The cost is one extra step the first time a model meets the family: it has to ask
//! `{"command": "help"}`. That is why everything here is generated from one list of inner
//! tools and nothing is written twice:
//!
//! - the declaration (`command` is an enum of exactly those tools),
//! - the `help` text and its `commands` array,
//! - the dispatch (a lookup in the same list).
//!
//! A hand-written help string is how `jq` ended up documented as a `shell` command that
//! dispatch could not run. With one list, "declared", "helped" and "dispatchable" are the
//! same set by construction; the tests compare them anyway, because a future refactor can
//! reintroduce the gap and only a test will notice.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::error::{McpError, Result};
use crate::tool::{enumeration, object_schema, ToolCtx, ToolDef};

/// A named group of tools that answers to one tool name and a `command` argument.
pub struct Family {
    /// The tool name the model sees, e.g. `manifest`.
    pub name: &'static str,
    /// One sentence: what the whole family is for. It reaches the schema, so it is short.
    pub summary: String,
    /// The commands, in the order `help` lists them. Each one keeps its own handler and
    /// schema; the family only changes how it is reached.
    pub subs: Vec<ToolDef>,
}

impl Family {
    pub fn new(name: &'static str, summary: impl Into<String>, subs: Vec<ToolDef>) -> Self {
        Self { name, summary: summary.into(), subs }
    }

    pub fn commands(&self) -> Vec<&str> {
        self.subs.iter().map(|d| d.spec.name.as_str()).collect()
    }

    /// The words a caller types inside the family, in the order `help` lists them.
    ///
    /// What an error message has to name: the schema enum, `help` and a refusal all have to
    /// speak the same language, and the inner names (`board_status`) are not in the schema.
    pub fn command_words(&self) -> Vec<String> {
        self.subs.iter().map(|d| Self::command_word(&d.spec.name, self.name)).collect()
    }

    /// The command words inside the family: `manifest apply` answers to `apply`.
    ///
    /// The inner tools keep their full names in the registry, so a script written before
    /// the family existed still calls the same handler; only the listing hides them.
    pub fn command_word(name: &str, family: &str) -> String {
        let prefix = format!("{family}_");
        name.strip_prefix(&prefix).unwrap_or(name).to_string()
    }

    /// The declaration, the help and the dispatch all read `subs`; this builds the tool.
    pub fn into_tool(self) -> ToolDef {
        let name = self.name;
        let mut words: Vec<String> =
            self.subs.iter().map(|d| Self::command_word(&d.spec.name, name)).collect();
        words.sort();
        let usage: Vec<(String, String, String)> = self
            .subs
            .iter()
            .map(|d| {
                (
                    Self::command_word(&d.spec.name, name),
                    first_sentence(d.spec.prose_description()),
                    arguments_of(&d.spec.input_schema),
                )
            })
            .collect();
        let mut variant = words.clone();
        variant.push("help".to_string());
        let schema = object_schema(
            &[
                (
                    "command",
                    enumeration(
                        &variant.iter().map(|s| s.as_str()).collect::<Vec<&str>>(),
                        "which operation, or `help` for what each one takes",
                    ),
                ),
                (
                    "arguments",
                    json!({
                        "type": "object",
                        "description": "arguments for that command; `command: help` lists them"
                    }),
                ),
            ],
            &["command"],
        );
        let hints: Vec<String> = self
            .subs
            .iter()
            .map(|d| {
                let word = Self::command_word(&d.spec.name, name);
                let takes = arguments_of(&d.spec.input_schema);
                if takes == "none" {
                    format!("{word}()")
                } else {
                    // The argument's own hint, when it has one: `led_set(color: string)` left the
                    // model passing `"green"`, which the board then refused with "must be
                    // #rrggbb, [r,g,b] or {r,g,b}" - one wasted call that the sub-tool's
                    // description already had the answer to (measured on the board, first turn
                    // of a battery over every tool). Cut short so a family of ten stays a
                    // paragraph.
                    format!("{word}({})", argument_hints(&d.spec.input_schema))
                }
            })
            .collect();
        let description = format!("{} Commands: {}.", self.summary, hints.join(", "));
        let shared = Arc::new(self);
        ToolDef::new(name, &description).title(name).category(name).core().schema(schema).bind(
            move |ctx, args| {
                let command =
                    args.get("command").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
                if command == "help" {
                    return Ok(json!({
                        "commands": words,
                        "usage": usage.iter().map(|(cmd, about, takes)| json!({
                            "command": format!("{name} {cmd}"),
                            "about": about,
                            "arguments": takes,
                        })).collect::<Vec<Value>>(),
                        "how_to_call": "{\"command\": \"<command>\", \"arguments\": {...}}",
                    }));
                }
                shared.dispatch(ctx, args, &command)
            },
        )
    }

    fn dispatch(&self, ctx: &mut ToolCtx<'_>, args: &Value, command: &str) -> Result<Value> {
        if command.is_empty() {
            return Err(McpError::invalid_params(format!(
                "`command` is required: one of {} (or `help`)",
                self.command_words().join(", ")
            )));
        }
        let Some(sub) = self.subs.iter().find(|d| Self::command_word(&d.spec.name, self.name) == command)
        else {
            return Err(McpError::invalid_params(format!(
                "no command `{command}` in `{}`; the commands are {} (or `help`)",
                self.name,
                self.command_words().join(", ")
            )));
        };
        let inner = match args.get("arguments") {
            Some(v) if v.is_object() => v.clone(),
            Some(Value::Null) | None => json!({}),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "`arguments` must be an object, one field per argument the command takes",
                ))
            }
        };
        ctx.ensure_enabled(self.name)?;
        ctx.ensure_enabled(&sub.spec.name)?;
        (sub.handler)(ctx, &inner)
    }
}

/// The first sentence of a description: `help` has to stay small, and the rest of a
/// description is what the model reads after it has decided to call the command.
fn first_sentence(text: &str) -> String {
    let cut = text.find(". ").map(|i| i + 1).unwrap_or(text.len());
    let line = text[..cut].trim();
    if line.len() <= 96 {
        line.to_string()
    } else {
        let mut end = 96;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &line[..end])
    }
}

/// `name: type` for each argument, without the prose — one line per command in `help`.
/// The same as [`arguments_of`] but carrying the note an argument can have about its *shape*.
///
/// A type is not a format: `color: string` is true of `"green"` and of `"#00ff00"`, and the
/// board refuses the first. The note is where `led_set` and `speaker_play` already say which one
/// they mean, so it belongs in the one line the model reads without a `help` call - cut short so
/// a family of ten commands stays a paragraph.
fn argument_hints(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return "none".to_string();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    for (name, spec) in props {
        let kind = spec.get("type").and_then(Value::as_str).unwrap_or("any");
        let mark = if required.contains(&name.as_str()) { "" } else { "?" };
        // Only a note that shows the value's *shape*: `#rrggbb`, `{hz: 523, ms: 400}`, `[r,g,b]`.
        // A sentence about what the field means belongs in `help`; a family that repeats every
        // sentence is bigger than the tools it replaced, which is the one thing grouping exists
        // to avoid (a test enforces that budget). The shape is found *before* clipping: a note
        // like "a melody, played back to back in one buffer: [{hz, ms}, ...]" opens with prose and
        // ends with the only part worth the bytes.
        match spec.get("description").and_then(Value::as_str).and_then(shape_of) {
            Some(shape) => parts.push(format!("{name}{mark}: {kind} ({shape})")),
            None => parts.push(format!("{name}{mark}: {kind}")),
        }
    }
    parts.join(", ")
}

/// The part of a note that says what the value looks like, cut short.
///
/// `#rrggbb, or an [r,g,b] array` and `a melody, played back to back in one buffer: [{hz, ms}, ...]`
/// both carry the shape after some prose, and the prose is what does not fit in a family line.
fn shape_of(note: &str) -> Option<String> {
    let start = note.find(['#', '[', '{'])?;
    let shape = clip_note(&note[start..]);
    if shape.is_empty() {
        None
    } else {
        Some(shape)
    }
}

/// An argument's note, cut to the part that says what the value looks like.
fn clip_note(note: &str) -> String {
    const MAX: usize = 24;
    let note = note.trim().trim_end_matches('.');
    if note.chars().count() <= MAX {
        return note.to_string();
    }
    let cut: String = note.chars().take(MAX).collect();
    match cut.rsplit_once(' ') {
        Some((head, _)) => head.to_string(),
        None => cut,
    }
}

fn arguments_of(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return "none".to_string();
    };
    if props.is_empty() {
        return "none".to_string();
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    for (name, spec) in props {
        let kind = spec.get("type").and_then(Value::as_str).unwrap_or("any");
        let mark = if required.contains(&name.as_str()) { "" } else { "?" };
        parts.push(format!("{name}{mark}: {kind}"));
    }
    parts.join(", ")
}

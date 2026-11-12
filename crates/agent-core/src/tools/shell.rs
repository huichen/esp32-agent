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
//!
//! A line may also carry a **pipeline**: `cmd1 | cmd2 | cmd3`, with `> file`, `>> file` and
//! `< file` redirection. One line, up to [`MAX_PIPE_STAGES`] stages, and every stage runs
//! through the same handler, the same table and the same credential the single-command form
//! does — a pipeline is a convenience, not a second way in. What is still missing is
//! everything the firmware has no model for: `&&`, `;`, `$(...)`, subshells, background jobs
//! and variable expansion. The tokenizer refuses the forms that mean "run two things" by
//! name, so `echo a && echo b` fails instead of printing `a && echo b`.

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
    // A JSON query, because `curl api | grep title` is what people actually do and it does
    // not survive nested objects or escaping.
    ("jq", "shell_jq"),
    ("md5sum", "shell_md5sum"),
    ("system_prompt", "shell_system_prompt"),
    // Reading and slicing text. `nsh` carries all of these and a person's fingers already
    // know them; none of them needs a directory tree, only a file and a few arguments.
    // `cat` is `read` under the other name — NuttX has both, and both get typed.
    ("echo", "shell_echo"),
    ("cat", "fs_read"),
    ("wc", "shell_wc"),
    ("head", "shell_head"),
    ("tail", "shell_head"),
    ("cut", "shell_cut"),
    ("sort", "shell_sort"),
    ("uniq", "shell_uniq"),
    ("tr", "shell_tr"),
    ("tee", "shell_tee"),
    ("touch", "shell_touch"),
    ("du", "shell_du"),
    ("strings", "shell_strings"),
    ("od", "shell_hexdump"),
    // Numbers and time. The board has one clock and one loop, so `sleep` is capped; `jot`
    // is how a shell produces a sequence without a `for`.
    ("sleep", "shell_sleep"),
    ("usleep", "shell_sleep"),
    ("jot", "shell_jot"),
    ("crc32", "shell_crc32"),
    // The board looking at itself. These answer from what the `App` already knows; none of
    // them invents a table it does not have.
    ("hostname", "shell_hostname"),
    ("whoami", "shell_whoami"),
    ("id", "shell_id"),
    ("env", "shell_env"),
    ("setenv", "shell_env"),
    ("unsetenv", "shell_env"),
    ("netstat", "shell_netstat"),
    ("route", "shell_route"),
    ("arp", "shell_arp"),
    // The registry: what a name means on this board.
    ("type", "shell_type"),
    ("which", "shell_type"),
    ("help", "shell_help"),
];

/// One command line, split the way a shell splits it: words, with quotes respected.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub name: String,
    pub args: Vec<String>,
    /// `--flag`, `-f value` and `--flag=value` both land here.
    pub flags: std::collections::BTreeMap<String, String>,
}

/// The most stages one line may carry.
///
/// A bound is what keeps a pipeline a shorthand and not a program: every stage copies the
/// text, and the answer has to fit in one tool result. Five is more than the examples a
/// person actually types (`ls | grep | wc -l` is three) and small enough to stay cheap.
pub const MAX_PIPE_STAGES: usize = 5;

/// What the pipeline syntax is, for the errors that have to teach it.
pub const PIPE_USAGE: &str =
    "cmd | cmd | …  ·  cmd > file  ·  cmd >> file  ·  cmd < file   (one line, up to 5 stages)";

/// A whole command line: the stages a `|` separates, each with its own redirections.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pipeline {
    pub stages: Vec<Stage>,
}

/// One stage: the command, plus the file it reads (`<`) and the file it writes (`>`, `>>`).
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub parsed: Parsed,
    pub stdin: Option<String>,
    pub stdout: Option<Redirect>,
}

/// `> file` or `>> file`.
#[derive(Debug, Clone, PartialEq)]
pub struct Redirect {
    pub path: String,
    pub append: bool,
}

impl Pipeline {
    /// The command as it was before pipelines: one stage, no pipe, no redirect. That form
    /// keeps the answer shape it always had, because every caller reads it.
    pub fn single(&self) -> Option<&Parsed> {
        match self.stages.as_slice() {
            [only] if only.stdin.is_none() && only.stdout.is_none() => Some(&only.parsed),
            _ => None,
        }
    }
}

/// One piece of a command line: a word, or one of the four operators this shell has.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Pipe,
    Out,
    Append,
    In,
}

impl Token {
    fn operator(&self) -> &'static str {
        match self {
            Token::Pipe => "|",
            Token::Out => ">",
            Token::Append => ">>",
            Token::In => "<",
            Token::Word(_) => "",
        }
    }
}

/// Split a line into words and operators, keeping quotes as one word.
///
/// This shell is small on purpose, and a person who types bash will reach for what is
/// missing. Those forms are refused *by name* here, because the alternative — let them
/// through as ordinary text — is a command that looks like it worked.
fn tokenize(line: &str) -> Result<Vec<Token>, String> {
    fn flush(tokens: &mut Vec<Token>, current: &mut String) -> Result<(), String> {
        if current.is_empty() {
            return Ok(());
        }
        let word = std::mem::take(current);
        if let Some(why) = compound_operator(&word) {
            return Err(why);
        }
        tokens.push(Token::Word(word));
        Ok(())
    }

    let mut tokens: Vec<Token> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = line.trim().chars().peekable();
    while let Some(ch) = chars.next() {
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
            // Inside quotes every operator is an ordinary character: `echo "a | b"`.
            c if quote.is_some() => current.push(c),
            '|' => {
                if chars.peek() == Some(&'|') {
                    return Err(compound_error("`||`", "no conditions, one pipeline per line"));
                }
                flush(&mut tokens, &mut current)?;
                tokens.push(Token::Pipe);
            }
            '>' => {
                flush(&mut tokens, &mut current)?;
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::Append);
                } else {
                    tokens.push(Token::Out);
                }
            }
            '<' => {
                flush(&mut tokens, &mut current)?;
                tokens.push(Token::In);
            }
            // `;`, `&`, `$` and the parentheses are ordinary characters: `system_prompt set
            // the pump is on GPIO12; never past two minutes` is a sentence. The forms that
            // mean "a second command" are caught per word by `compound_operator`.
            c if c.is_whitespace() => flush(&mut tokens, &mut current)?,
            c => current.push(c),
        }
    }
    if quote.is_some() {
        return Err("unbalanced quote in the command".into());
    }
    flush(&mut tokens, &mut current)?;
    Ok(tokens)
}

fn compound_error(what: &str, why: &str) -> String {
    format!("{what} is not a shell operator here: {why}\n\nusage: {PIPE_USAGE}")
}

/// A word that tries to be a second command.
///
/// `;`, `&`, `$` and the parentheses are ordinary characters inside text, but a word that
/// *is* one of the compound forms is a person asking for something this shell does not have,
/// and letting it through would print `a && echo b` and look like both halves ran.
fn compound_operator(word: &str) -> Option<String> {
    match word {
        ";" => Some(compound_error("`;`", "one line, one pipeline, no sequencing")),
        "&" => Some(compound_error("`&`", "there are no background jobs")),
        "&&" | "||" => Some(compound_error("`&&` / `||`", "no conditions, one pipeline per line")),
        _ if word.starts_with("$(") => Some(compound_error("`$(`", "there is no command substitution")),
        _ if word.contains('`') => Some(compound_error("backticks", "there is no command substitution")),
        _ => None,
    }
}

/// One command line, split the way a shell splits it: words, with quotes respected.
///
/// The single-command form of [`parse_pipeline`]: it refuses a pipe or a redirect rather than
/// guessing what was meant, so a caller that only speaks one command says so.
pub fn parse(line: &str) -> Result<Parsed, String> {
    let mut words: Vec<String> = Vec::new();
    for token in tokenize(line)? {
        match token {
            Token::Word(word) => words.push(word),
            other => {
                return Err(format!(
                    "`{}` belongs to a pipeline, not to a single command\n\nusage: {PIPE_USAGE}",
                    other.operator()
                ))
            }
        }
    }
    words_to_parsed(words)
}

/// A whole line: stages, pipes and the two redirections.
///
/// The grammar is the one everybody already types — `ls / | grep txt`, `cat < /a > /b`, `wc
/// -l < /log` — because the point of a shell is that the model does not have to learn a new
/// calling convention to use it. Quotes group a word; a backslash escapes the next
/// character; operators only count outside quotes.
/// `a; b` split into its commands, or `None` when the line is one command.
///
/// A semicolon is a character when it is inside a sentence — `system_prompt set the pump is
/// on GPIO12; never past it` is one argument, and the quoting rule already keeps it that
/// way. Four parts is the ceiling: beyond that the caller wants a script, and the script
/// language is where a real sequence belongs.
/// Is the text after a `;` the start of a command?
///
/// This is what keeps `system_prompt set the pump is on GPIO12; never past it` one argument
/// while `echo a > /f; cat /f` is two commands: `never` is not a command, `cat` is. A
/// semicolon that introduces a word the shell does not know is a character, which is the
/// behaviour the standing-orders sentence has relied on since it was written.
fn starts_a_command(after: &str) -> bool {
    let word = after.trim_start();
    let word: String = word.chars().take_while(|c| !c.is_whitespace() && *c != ';').collect();
    if word.is_empty() {
        return false;
    }
    route(&word).is_some()
}

pub fn split_sequence(line: &str) -> Result<Option<Vec<String>>, String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut rest = line.char_indices().peekable();
    while let Some((at, ch)) = rest.next() {
        let rest_after: &str = &line[at + ch.len_utf8()..];
        if let Some(q) = quote {
            current.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            current.push(ch);
        } else if ch == ';' && starts_a_command(&rest_after) {
            if current.trim().is_empty() {
                return Err("`;` needs a command on both sides\n\n".to_string() + PIPE_USAGE);
            }
            parts.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed\n\n".to_string() + PIPE_USAGE);
    }
    if parts.is_empty() {
        return Ok(None);
    }
    if current.trim().is_empty() {
        return Err("`;` needs a command on both sides\n\n".to_string() + PIPE_USAGE);
    }
    parts.push(current.trim().to_string());
    if parts.len() > 4 {
        return Err(format!(
            "at most 4 commands on one line with `;` (got {}): use a script for a sequence",
            parts.len()
        ));
    }
    Ok(Some(parts))
}

pub fn parse_pipeline(line: &str) -> Result<Pipeline, String> {
    let mut stages: Vec<Stage> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut stdin: Option<String> = None;
    let mut stdout: Option<Redirect> = None;
    // The operator that is waiting for its file name or its next stage.
    let mut pending: Option<Token> = None;

    for token in tokenize(line)? {
        match token {
            Token::Word(word) => match pending.take() {
                Some(Token::Out) => stdout = Some(Redirect { path: word, append: false }),
                Some(Token::Append) => stdout = Some(Redirect { path: word, append: true }),
                Some(Token::In) => stdin = Some(word),
                Some(Token::Pipe) | Some(Token::Word(_)) | None => words.push(word),
            },
            Token::Pipe => {
                if let Some(operator) = pending.take() {
                    return Err(missing_target(&operator));
                }
                stages.push(stage_from(std::mem::take(&mut words), stdin.take(), stdout.take())?);
                if stages.len() >= MAX_PIPE_STAGES {
                    return Err(format!(
                        "a pipeline carries at most {MAX_PIPE_STAGES} stages; this one already has {}\n\nusage: {PIPE_USAGE}",
                        stages.len()
                    ));
                }
            }
            operator @ (Token::Out | Token::Append | Token::In) => {
                if let Some(waiting) = pending.take() {
                    return Err(missing_target(&waiting));
                }
                pending = Some(operator);
            }
        }
    }
    if let Some(operator) = pending {
        return Err(missing_target(&operator));
    }
    if words.is_empty() {
        return Err(if stages.is_empty() {
            "empty command".to_string()
        } else {
            format!("a pipeline cannot end with `|`: the last stage is missing\n\nusage: {PIPE_USAGE}")
        });
    }
    stages.push(stage_from(words, stdin, stdout)?);
    Ok(Pipeline { stages })
}

fn missing_target(operator: &Token) -> String {
    format!("`{}` needs a file name\n\nusage: {PIPE_USAGE}", operator.operator())
}

fn stage_from(words: Vec<String>, stdin: Option<String>, stdout: Option<Redirect>) -> Result<Stage, String> {
    if words.is_empty() {
        return Err(format!("a pipeline stage is missing its command\n\nusage: {PIPE_USAGE}"));
    }
    Ok(Stage { parsed: words_to_parsed(words)?, stdin, stdout })
}

/// Whether a command reads the text a pipe hands it.
///
/// `None` means it is not a pipeline stage at all, which is what stops `curl` and `script`
/// from being smuggled into one: a pipeline is not a way round the command table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stdio {
    /// Reads stdin when it has no file of its own: `sort`, `grep`, `wc`, `cat`…
    Reads,
    /// Only produces: it may start a pipeline, but a later stage has nothing to read.
    Produces,
}

/// The stages a pipeline may carry, and what each one does with the text arriving on stdin.
///
/// `tee` and `xd` sit beside the list a person would write out: `tee` is the command a
/// pipeline exists for, and `xd` is `hexdump` under its other name.
pub fn pipe_stdio(op: &str) -> Option<Stdio> {
    Some(match op {
        // `jq` reads lines (a JSON document) and writes lines, which is what a stage does.
        "jq" | "cat" | "grep" | "sort" | "uniq" | "tr" | "cut" | "head" | "tail" | "wc" | "strings"
        | "od" | "xd" | "hexdump" | "base64enc" | "base64dec" | "sha256sum" | "md5sum" | "crc32" | "tee" => {
            Stdio::Reads
        }
        // A fetch is a producer: it has no stdin to read, and its body is exactly the text a
        // `jq`, `grep` or `head` downstream wants. Refusing it made `curl api | jq .x` — the
        // reason both commands exist — impossible.
        "curl" | "wget" | "ls" | "jot" | "du" | "df" | "free" | "ps" | "uname" | "date" | "uptime"
        | "echo" => Stdio::Produces,
        _ => return None,
    })
}

/// The stages a pipeline may carry, as one line, for the error that refuses another command.
pub fn pipe_commands() -> String {
    let mut names: Vec<&str> = [
        "curl",
        "wget",
        "jq",
        "cat",
        "grep",
        "sort",
        "uniq",
        "tr",
        "cut",
        "head",
        "tail",
        "wc",
        "strings",
        "od",
        "xd",
        "hexdump",
        "base64enc",
        "base64dec",
        "sha256sum",
        "md5sum",
        "crc32",
        "tee",
        "ls",
        "jot",
        "du",
        "df",
        "free",
        "ps",
        "uname",
        "date",
        "uptime",
        "echo",
    ]
    .to_vec();
    names.sort_unstable();
    names.join(" ")
}

/// Short flags that take a value, per command.
///
/// It used to be one list for every command, which cannot work once `-n` is `head`'s line
/// count and `echo`'s "no newline", or `-c` is `ping`'s count and `wc`'s bytes. A flag that
/// takes a value swallows the next word, so getting this wrong turns `wc -c /f` into a
/// command with no file at all.
fn takes_value(op: &str, key: &str) -> bool {
    match key {
        "c" => op == "ping",
        "d" | "f" => matches!(op, "curl" | "cut"),
        "o" => matches!(op, "wget" | "base64enc" | "base64dec"),
        "p" => matches!(op, "nslookup" | "nc"),
        "w" => matches!(op, "hexdump" | "xd" | "od"),
        "n" => matches!(op, "head" | "tail" | "strings"),
        "i" => op == "tee",
        _ => false,
    }
}

/// Turn the words of one command into a name, its positional arguments and its flags.
fn words_to_parsed(words: Vec<String>) -> Result<Parsed, String> {
    let mut it = words.into_iter();
    let name = it.next().ok_or("empty command")?.to_ascii_lowercase();
    let rest: Vec<String> = it.collect();

    let mut args = Vec::new();
    let mut flags = std::collections::BTreeMap::new();
    let mut index = 0;
    while index < rest.len() {
        let word = &rest[index];
        // A negative number is an argument, not a flag: `jot 5 10 -1` counts down. Nothing
        // in the table has a short flag made of digits, so there is nothing to shadow.
        let negative_number = word.len() > 1 && word.starts_with('-') && looks_numeric(&word[1..]);
        if negative_number {
            args.push(word.clone());
        } else if let Some(stripped) = word.strip_prefix("--") {
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
            // Short flags: `-r`, and `-o value` for the ones that take one — per command, so
            // `-n` can be `head`'s count and `echo`'s "no newline" at the same time.
            let key = word[1..].to_ascii_lowercase();
            if takes_value(&name, &key) {
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

/// `jq`, the subset this board needs: a path into a JSON document.
///
/// Queries are `.` (the document), `.name`, `.a.b`, `.[0]`, `.a[0].b`, and `[]` after any
/// step to iterate (`.[]`, `.hits[]`, `.hits[].title`). `length` returns a count. Iterating
/// prints one compact JSON value per line, which is what makes `jq .hits[].title | head`
/// work; any other result prints one value, pretty-printed unless `raw` is set and the
/// value is a string.
///
/// Not implemented, on purpose: pipes inside the query, functions, arithmetic, assignment.
/// A query language inside a shell inside a tool would be a third grammar to learn, and the
/// board has one job for this command — reach into a response.
pub fn jq(input: &str, query: &str, raw: bool) -> Result<String, String> {
    let doc: serde_json::Value =
        serde_json::from_str(input).map_err(|e| format!("jq: the input is not JSON ({e})"))?;
    let query = query.trim();
    let query = if query.is_empty() { "." } else { query };
    let mut current = doc;
    let mut iterated = false;

    // A leading `.` is the document; the rest is a sequence of names, `[n]` indices and `[]`
    // iterations. Scanned rather than split on `.`, because `hits[0]` is one step with two
    // parts and splitting there would lose the index.
    let body = query.strip_prefix('.').ok_or("jq: a query starts with `.`")?;
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' => i += 1,
            '[' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                let inner: String = chars[i + 1..j.min(chars.len())].iter().collect();
                if inner.trim().is_empty() {
                    iterated = true;
                } else {
                    let n: usize =
                        inner.trim().parse().map_err(|_| format!("jq: `{inner}` is not an index"))?;
                    current = current.get(n).cloned().ok_or_else(|| format!("jq: no element {n}"))?;
                }
                i = (j + 1).min(chars.len());
            }
            _ => {
                let mut j = i;
                while j < chars.len() && chars[j] != '.' && chars[j] != '[' {
                    j += 1;
                }
                let name: String = chars[i..j].iter().collect();
                if name == "length" {
                    let n = match &current {
                        serde_json::Value::Array(a) => a.len(),
                        serde_json::Value::Object(o) => o.len(),
                        serde_json::Value::String(s) => s.chars().count(),
                        _ => 0,
                    };
                    current = serde_json::json!(n);
                } else if iterated {
                    // After `[]` the value is an array and every further step applies to each
                    // element: `.hits[].title` is a projection, not a lookup on the array.
                    let items = match current {
                        serde_json::Value::Array(items) => items,
                        other => vec![other],
                    };
                    let mut out = Vec::new();
                    for item in items {
                        if let Some(found) = item.get(&name) {
                            out.push(found.clone());
                        }
                    }
                    current = serde_json::Value::Array(out);
                } else {
                    current = current.get(&name).cloned().ok_or_else(|| format!("jq: no field `{name}`"))?;
                }
                i = j;
            }
        }
    }

    if iterated {
        // One line per element of the final array, which is what a following `grep` or
        // `head` wants. A non-array under `[]` is one element, as in jq.
        let items: Vec<serde_json::Value> = match current {
            serde_json::Value::Array(a) => a,
            other => vec![other],
        };
        let mut lines = Vec::new();
        for item in items {
            let line = match (&item, raw) {
                (serde_json::Value::String(s), true) => s.clone(),
                (serde_json::Value::Object(_), _) | (serde_json::Value::Array(_), _) => {
                    serde_json::to_string(&item).unwrap_or_default()
                }
                _ => serde_json::to_string(&item).unwrap_or_default(),
            };
            lines.push(line);
        }
        return Ok(lines.join("\n"));
    }

    match (&current, raw) {
        (serde_json::Value::String(s), true) => Ok(s.clone()),
        _ => Ok(serde_json::to_string_pretty(&current).unwrap_or_default()),
    }
}

/// Digits and dots: what a negative number looks like once its sign is off.
fn looks_numeric(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|c| c.is_ascii_digit() || c == '.')
        && text.chars().any(|c| c.is_ascii_digit())
}

/// Turn a parsed line into the argument object the handler for that command takes.
///
/// One place, so the command line and the JSON form cannot mean different things.
pub fn arguments(parsed: &Parsed) -> Result<serde_json::Value, String> {
    arguments_inner(parsed, false)
}

/// The same translation for a stage in a pipeline, where a missing path is not an error: the
/// text comes from the pipe instead, and the handler finds it under `stdin`.
pub fn stage_arguments(parsed: &Parsed, piped: bool) -> Result<serde_json::Value, String> {
    arguments_inner(parsed, piped)
}

fn arguments_inner(parsed: &Parsed, piped: bool) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let arg = |n: usize| parsed.args.get(n).cloned();
    let need = |n: usize, what: &str| -> Result<String, String> {
        arg(n).ok_or_else(|| format!("{}: missing {what}", parsed.name))
    };
    // In a pipeline the file may be absent: `null` tells the handler to read `stdin`.
    let path_arg = |n: usize| -> Result<Value, String> {
        match arg(n) {
            Some(path) => Ok(json!(path)),
            None if piped => Ok(Value::Null),
            None => Err(format!("{}: missing <path>", parsed.name)),
        }
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
        "grep" => json!({
            "pattern": need(0, "<pattern>")?,
            // No path and a pipe: the text arrives on stdin. Without a pipe the tree is what
            // `grep` searches, as it always has.
            "path": arg(1).or(if piped { None } else { Some("/".to_string()) }),
        }),
        "zip" => {
            let name = need(0, "<archive>")?;
            let mut files = serde_json::Map::new();
            for path in parsed.args.iter().skip(1) {
                files.insert(path.trim_start_matches('/').to_string(), json!(""));
            }
            json!({ "name": name, "paths": parsed.args.iter().skip(1).collect::<Vec<_>>(), "files": files })
        }
        "unzip" => json!({ "archive_b64": need(0, "<archive_b64>")? }),
        "hexdump" | "xd" | "od" => json!({
            "path": path_arg(0)?,
            "width": flag("width", "w"),
            "octal": parsed.name == "od",
        }),
        "base64" | "base64enc" | "base64dec" => json!({
            "path": path_arg(0)?,
            "decode": parsed.name == "base64dec" || flag_bool("decode", "d"),
            "out": flag("out", "o"),
        }),
        "sha256sum" | "md5sum" => json!({ "path": path_arg(0)? }),
        "echo" => {
            // `-n` takes no value in this table (it is `head`'s flag elsewhere), so it only
            // ever means "no trailing newline" here.
            json!({ "words": parsed.args, "newline": !flag_bool("n", "n") })
        }
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
        "ifconfig" | "df" | "free" | "uptime" | "date" | "uname" | "ps" | "help" | "du" | "whoami" | "id"
        | "netstat" | "route" | "arp" => json!({}),
        "env" => json!({ "action": "list" }),
        "cat" | "uniq" | "crc32" | "touch" => json!({ "path": path_arg(0)? }),
        "wc" => {
            // One mode per call, as the usage line says; `-l`, `-w` and `-c` are plain flags
            // because they take no value for `wc` (see `takes_value`).
            let mode = [("l", "lines"), ("w", "words"), ("c", "bytes")]
                .iter()
                .find(|(key, _)| parsed.flags.contains_key(*key))
                .map(|(_, name)| *name)
                .unwrap_or("");
            json!({ "path": path_arg(0)?, "mode": mode })
        }
        "head" | "tail" => json!({
            "path": path_arg(0)?,
            "lines": parsed.flags.get("n").cloned(),
            "tail": parsed.name == "tail",
        }),
        "cut" => json!({
            "path": path_arg(0)?,
            "delimiter": parsed.flags.get("d").cloned(),
            "field": parsed.flags.get("f").cloned(),
        }),
        "sort" => json!({
            "path": path_arg(0)?,
            "reverse": flag_bool("r", "r"),
            "unique": flag_bool("u", "u"),
        }),
        "tr" => json!({
            "from": need(0, "<from>")?,
            "to": need(1, "<to>")?,
            "path": path_arg(2)?,
        }),
        "tee" => json!({ "path": need(0, "<path>")?, "input": parsed.flags.get("i").cloned() }),
        "strings" => json!({ "path": path_arg(0)?, "min": parsed.flags.get("n").cloned() }),
        "sleep" => json!({ "unit": "s", "value": need(0, "<seconds>")? }),
        "usleep" => json!({ "unit": "ms", "value": need(0, "<milliseconds>")? }),
        "jot" => json!({ "count": need(0, "<count>")?, "start": arg(1), "step": arg(2) }),
        "hostname" => json!({ "name": arg(0) }),
        "setenv" => json!({ "action": "set", "key": need(0, "<key>")?, "value": need(1, "<value>")? }),
        "unsetenv" => json!({ "action": "unset", "key": need(0, "<key>")? }),
        "type" | "which" => json!({ "name": need(0, "<name>")?, "which": parsed.name == "which" }),
        // The query is positional because `jq` has no flag for it; `-r` is the only flag.
        // Miss this arm and `jq` is a command with a usage line, a handler and no way in.
        "jq" => {
            // The query is the first word, the document is everything after it that is not a
            // flag. A command of `[path]` shape would read `jq .a.b` as the path.
            let mut words = parsed.args.iter();
            let query = words.next().cloned();
            let path = words.find(|w| !w.starts_with('-')).cloned();
            json!({
                "positional": parsed.args,
                "query": query,
                "path": path,
                "r": flag_bool("r", "r"),
            })
        }
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
        "jq" => "jq [query] <path>   query: . .name .a.b .[0] .a[0].b .[] .length (-r for strings)",
        "md5sum" => "md5sum <path>",
        "system_prompt" => "system_prompt get | set <text> | append <text> | clear",
        // Reading and slicing text. Each takes its input as a path, or as the pipe a stage
        // before it left behind.
        "echo" => "echo [-n] <text...>",
        "cat" => "cat [path]  (same as read; with no path it copies the pipe)",
        "wc" => "wc [-l|-w|-c] <path>",
        "head" => "head [-n <lines>] <path>",
        "tail" => "tail [-n <lines>] <path>",
        "cut" => "cut [-d <delimiter>] [-f <field>] <path>  (defaults: tab, field 1)",
        "sort" => "sort [-r] [-u] <path>",
        "uniq" => "uniq <path>",
        "tr" => "tr <from> <to> <path>",
        "tee" => "tee <path> [-i <text>]  (a pipe feeds it; -i carries the text without one)",
        "touch" => "touch <path>",
        "du" => "du",
        "strings" => "strings <path> [-n <min run>]",
        "od" => "od <path> [-w <bytes per line>]  (octal, where hexdump is hexadecimal)",
        // Numbers and time.
        "sleep" => "sleep <seconds>",
        "usleep" => "usleep <milliseconds>",
        "jot" => "jot <count> [start [step]]",
        "crc32" => "crc32 <path>",
        // The board looking at itself.
        "hostname" => "hostname [name]",
        "whoami" => "whoami",
        "id" => "id",
        "env" => "env  (session variables only; they are not saved)",
        "setenv" => "setenv <key> <value>  (session only, not saved to NVS)",
        "unsetenv" => "unsetenv <key>  (session only)",
        "netstat" => "netstat",
        "route" => "route",
        "arp" => "arp",
        // The registry.
        "type" => "type <name>",
        "which" => "which <name>  (same answer as type)",
        "help" => "help",
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
         zip <archive> <path...> · unzip <archive_b64> · hexdump <path> [-w n] · od <path> (octal) · base64 <path> [-d] [-o out]\n\
         cat [path] · wc [-l|-w|-c] <path> · head/tail [-n n] <path> · cut -d c -f n <path> · sort [-r] [-u] <path> · uniq <path> · tr <from> <to> <path> · tee <path> [-i text] · echo <text>\n\
         touch <path> · du · strings <path> · sleep <s> · usleep <ms> · jot <count> [start [step]] · crc32 <path>\n\
         hostname [name] · whoami · id · env · setenv <k> <v> · unsetenv <k> · netstat · route · arp · type <name> · which <name>\n\
         df · free · uptime · date · uname · ps · help\n\
         Pipes and redirection work on one line: `ls / | grep txt`, `cat < /a > /b`, `echo hi >> /log` — up to 5 stages.\n\
         No `&&`, no `;`, no `$(...)`, no subshell, no background job, no variable expansion.\n\
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
        assert_eq!(route("sudo"), None);
        let spec = spec();
        assert_eq!(spec.spec.name, "shell");
        assert!(spec.spec.input_schema["properties"]["command"].is_object());
    }

    /// `help` is generated from the table, so a command with no usage line would be a
    /// command nobody can find out how to write. One line per entry, in table order.
    #[test]
    fn every_command_has_a_usage_line_and_help_prints_it() {
        assert!(OPS.len() >= 45, "the board's shell has {} commands", OPS.len());
        let text = help_text();
        let printed: Vec<&str> = text.lines().filter(|line| line.starts_with("  ")).collect();
        assert_eq!(printed.len(), OPS.len(), "help must print one line per command:\n{text}");
        for (op, _) in OPS {
            let line = usage(op).unwrap_or_else(|| panic!("`{op}` has no usage line"));
            assert!(printed.contains(&format!("  {line}").as_str()), "`{op}` is missing from help:\n{text}");
        }
        // The three tools that stayed separate are named in the same text.
        for tool in ["read", "write", "edit"] {
            assert!(text.contains(&format!("{tool} ")), "{tool} is missing from help");
        }
    }

    /// A filter has to name its file, and the flags that take a value have to survive the
    /// shared flag table: `-c` is ping's count and `-w` is hexdump's width, so `wc -c /f`
    /// reaches the handler as `c=/f`.
    #[test]
    fn the_new_commands_parse_the_way_a_shell_writes_them() {
        let cut = arguments(&parse("cut -d , -f 2 /notes.csv").unwrap()).unwrap();
        assert_eq!(cut["delimiter"], json!(","));
        assert_eq!(cut["field"], json!("2"));
        assert_eq!(cut["path"], json!("/notes.csv"));

        let wc = arguments(&parse("wc -c /f").unwrap()).unwrap();
        assert_eq!(wc["path"], json!("/f"), "the value of -c is the path");
        assert_eq!(wc["mode"], json!("bytes"));
        let wc = arguments(&parse("wc -l /f").unwrap()).unwrap();
        assert_eq!(wc["mode"], json!("lines"));
        assert_eq!(wc["path"], json!("/f"));

        let head = arguments(&parse("head -n 2 /f").unwrap()).unwrap();
        assert_eq!(head["lines"], json!("2"));
        assert_eq!(head["tail"], json!(false));
        let tail = arguments(&parse("tail /f").unwrap()).unwrap();
        assert_eq!(tail["tail"], json!(true));

        let tee = arguments(&parse("tee /out.txt -i hello there").unwrap()).unwrap();
        assert_eq!(tee["path"], json!("/out.txt"));
        assert_eq!(tee["input"], json!("hello"));

        let sort = arguments(&parse("sort -r -u /f").unwrap()).unwrap();
        assert_eq!(sort["reverse"], json!(true));
        assert_eq!(sort["unique"], json!(true));

        let tr = arguments(&parse("tr a b /f").unwrap()).unwrap();
        assert_eq!(tr["from"], json!("a"));
        assert_eq!(tr["to"], json!("b"));
        assert_eq!(tr["path"], json!("/f"));

        let od = arguments(&parse("od /f").unwrap()).unwrap();
        assert_eq!(od["octal"], json!(true));

        let env = arguments(&parse("setenv MODE test").unwrap()).unwrap();
        assert_eq!(env["action"], json!("set"));
        assert_eq!(env["key"], json!("MODE"));
        assert_eq!(env["value"], json!("test"));
        let list = arguments(&parse("env").unwrap()).unwrap();
        assert_eq!(list["action"], json!("list"));

        // A negative number is an argument, not a flag: `jot` counts down.
        let jot = arguments(&parse("jot 5 10 -1").unwrap()).unwrap();
        assert_eq!(jot["count"], json!("5"));
        assert_eq!(jot["start"], json!("10"));
        assert_eq!(jot["step"], json!("-1"));

        let which = arguments(&parse("which ls").unwrap()).unwrap();
        assert_eq!(which["name"], json!("ls"));
        assert_eq!(which["which"], json!(true));
    }

    /// Every command a person would type is in the table under the NuttX name, and the
    /// ones that are aliases share a handler instead of copying it.
    #[test]
    fn the_table_names_the_commands_nuttx_has() {
        for op in [
            "cat", "wc", "head", "tail", "cut", "sort", "uniq", "tr", "tee", "touch", "du", "strings", "od",
            "sleep", "usleep", "jot", "crc32", "hostname", "whoami", "id", "env", "setenv", "unsetenv",
            "netstat", "route", "arp", "type", "which", "help", "echo",
        ] {
            assert!(route(op).is_some(), "`{op}` is not a shell command");
        }
        assert_eq!(route("cat"), Some("fs_read"), "cat is read under the other name");
        assert_eq!(route("head"), route("tail"), "one handler, two directions");
        assert_eq!(route("sleep"), route("usleep"));
        assert_eq!(route("env"), route("setenv"));
        assert_eq!(route("type"), route("which"));
        assert_eq!(route("od"), route("xd"), "od is the octal half of hexdump");
    }

    /// A line is stages, and each stage keeps its own redirections. A quoted pipe is text.
    #[test]
    fn a_line_splits_into_a_pipeline_with_its_redirections() {
        let two = parse_pipeline("ls / | grep txt").unwrap();
        assert_eq!(two.stages.len(), 2);
        assert_eq!(two.stages[0].parsed.name, "ls");
        assert_eq!(two.stages[1].parsed.args, vec!["txt".to_string()]);
        assert!(two.single().is_none(), "a pipe is not the plain form");

        let one = parse_pipeline("cat /a").unwrap();
        assert_eq!(one.single().map(|p| p.name.as_str()), Some("cat"));

        let written = parse_pipeline("echo hi > /f").unwrap();
        assert!(written.single().is_none(), "a redirect is not the plain form either");
        let redirect = written.stages[0].stdout.as_ref().unwrap();
        assert_eq!(redirect.path, "/f");
        assert!(!redirect.append);
        assert!(parse_pipeline("echo hi >> /f").unwrap().stages[0].stdout.as_ref().unwrap().append);

        assert_eq!(parse_pipeline("wc -l < /f").unwrap().stages[0].stdin.as_deref(), Some("/f"));

        // Quotes keep an operator as one word.
        let quoted = parse_pipeline("echo \"a | b\"").unwrap();
        assert_eq!(quoted.stages.len(), 1);
        assert_eq!(quoted.stages[0].parsed.args, vec!["a | b".to_string()]);
    }

    /// The bound is a decision, not an accident: five stages carry, six do not.
    #[test]
    fn jq_reaches_into_a_document() {
        let doc = r#"{"hits":[{"title":"a","points":3},{"title":"b","points":9}],"total":2}"#;

        assert!(jq(doc, ".", false).unwrap().contains("\"total\""));
        assert_eq!(jq(doc, ".total", false).unwrap(), "2");
        assert_eq!(jq(doc, ".hits[0].title", false).unwrap(), "\"a\"");
        assert_eq!(jq(doc, ".hits[0].title", true).unwrap(), "a", "-r drops the quotes");
        assert_eq!(jq(doc, ".hits.length", false).unwrap(), "2");

        // Iteration is one value per line, which is what a `head` or `grep` downstream reads.
        assert_eq!(jq(doc, ".hits[].title", true).unwrap(), "a\nb");

        // A bad step is named, not turned into null.
        assert!(jq(doc, ".missing", false).unwrap_err().contains("no field `missing`"));
        assert!(jq(doc, ".hits[7]", false).unwrap_err().contains("no element 7"));
        assert!(jq("not json", ".", false).unwrap_err().contains("not JSON"));
        assert!(jq(doc, "hits", false).unwrap_err().contains("starts with"));
    }

    #[test]
    fn a_pipeline_has_a_stage_limit_and_needs_both_ends() {
        let five = "echo a | cat | cat | cat | cat";
        assert_eq!(parse_pipeline(five).unwrap().stages.len(), MAX_PIPE_STAGES);
        let six = "echo a | cat | cat | cat | cat | cat";
        let err = parse_pipeline(six).unwrap_err();
        assert!(err.contains("at most 5"), "{err}");
        assert!(err.contains("usage:"), "{err}");

        for (line, needle) in [
            ("ls / |", "cannot end with `|`"),
            ("| ls", "missing its command"),
            ("echo hi >", "needs a file name"),
            ("echo hi >>", "needs a file name"),
            ("cat <", "needs a file name"),
        ] {
            let err = parse_pipeline(line).unwrap_err();
            assert!(err.contains(needle), "`{line}`: {err}");
            assert!(err.contains("usage:"), "`{line}` has no usage line: {err}");
        }
    }

    /// The forms this shell does not have are refused by name; the ones that are just text
    /// stay text, or `system_prompt set the pump is on GPIO12; never past it` would be two
    /// commands.
    #[test]
    fn the_parser_refuses_the_bash_forms_it_does_not_have() {
        for (line, needle) in [
            ("echo a && echo b", "&&"),
            ("echo a || echo b", "||"),
            ("ls ; ls", "`;`"),
            ("ls &", "`&`"),
            ("echo $(date)", "$("),
            ("echo `date`", "backticks"),
        ] {
            let err = parse_pipeline(line).unwrap_err();
            assert!(err.contains(needle), "`{line}`: {err}");
            assert!(err.contains("usage:"), "`{line}` has no usage line: {err}");
        }
        let sentence = parse_pipeline("system_prompt set the pump is on GPIO12; never past it").unwrap();
        assert_eq!(sentence.stages.len(), 1, "a semicolon inside a word is text");
        assert_eq!(
            parse_pipeline("echo $HOME").unwrap().stages[0].parsed.args,
            vec!["$HOME"],
            "no expansion"
        );
    }

    /// Only the line commands are stages, and the list the error prints is this list.
    #[test]
    fn only_the_line_commands_may_be_stages() {
        for op in [
            "cat", "grep", "sort", "uniq", "tr", "cut", "head", "tail", "wc", "echo", "ls", "jot", "tee",
            "curl", "wget",
        ] {
            assert!(pipe_stdio(op).is_some(), "`{op}` should be a stage");
        }
        // A fetch reads no stdin, so it may only start a pipeline.
        assert_eq!(pipe_stdio("curl"), Some(Stdio::Produces));
        assert_eq!(pipe_stdio("wget"), Some(Stdio::Produces));
        for op in ["rm", "touch", "sleep", "script", "hostname", "ping"] {
            assert_eq!(pipe_stdio(op), None, "`{op}` must not be a stage");
        }
        assert_eq!(pipe_stdio("ls"), Some(Stdio::Produces));
        assert_eq!(pipe_stdio("sort"), Some(Stdio::Reads));

        // Every command that is a stage is named in the refusal, and nothing else is.
        let listed = pipe_commands();
        let names: Vec<&str> = listed.split(' ').collect();
        for (op, _) in OPS {
            assert_eq!(names.contains(op), pipe_stdio(op).is_some(), "`{op}` in the list");
        }
    }
}

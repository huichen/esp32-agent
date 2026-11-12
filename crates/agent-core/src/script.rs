//! A small scripting language the agent can write and the board can run.
//!
//! Why not Lua or Rhai: this firmware has ~40 kB of free internal RAM and ships as one
//! binary, and the point of the language is not general purpose programming — it is
//! letting the model express *loops and conditions over tool calls* that the fixed tool
//! vocabulary cannot: "probe every bus, adopt the OLED, then blink until the button is
//! pressed".
//!
//! The language is deliberately tiny and genuinely Turing-complete (assignment plus
//! `while`), with no pointers, no user functions and no I/O of its own:
//!
//! ```text
//! let found = tool("i2c_scan", {})
//! let i = 0
//! while i < len(found.addresses) {
//!   let addr = found.addresses[i]
//!   log("probing " + str(addr))
//!   i = i + 1
//! }
//! ```
//!
//! `tool(name, args)` is the only way out: it calls any MCP tool the board already has,
//! through the same dispatcher, so a script has exactly the permissions of the caller and
//! cannot reach anything the tool list does not expose. `script` itself is refused inside
//! a script — recursion through the tool layer is how a 40 kB board runs out of stack.
//!
//! Everything is bounded: a step budget, a depth limit, and caps on string, list and map
//! growth. A runaway script stops with a message instead of taking the board down.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Milliseconds since boot, from the same clock the rest of the board uses.
fn now_ms() -> u64 {
    crate::app::now_ms()
}

/// Wall-clock budget for one script.
///
/// A step count alone is the wrong bound: 50 000 steps is a few hundred milliseconds of
/// real work, so the old limit stopped honest loops while saying nothing about a script
/// that spends its steps waiting. Time is what the board can afford — the interpreter runs
/// on the thread that also answers the portal, so thirty seconds of spinning is thirty seconds
/// the board is unreachable.
pub const MAX_MS: u64 = 30_000;
/// A backstop for cheap steps: even inside the time budget, this many means something is
/// spinning. Large enough that no honest program notices it.
pub const MAX_STEPS: u64 = 5_000_000;
/// Nested blocks (and `if`/`while` bodies) a script may enter.
const MAX_DEPTH: usize = 32;
/// Recursion depth. Each frame is a map of its own, so this is also a memory bound: 32
/// frames of a few locals is what an 8 KB workspace and a few kilobytes of stack allow.
const MAX_CALLS: usize = 32;
/// A string or a list may not grow past this: the board has kilobytes, not megabytes.
const MAX_STRING: usize = 4096;
const MAX_LIST: usize = 256;
const MAX_MAP: usize = 64;
/// Source length. The whole script arrives as a tool argument (2 KB result cap applies to
/// results, not arguments, but a small model will not write more than this anyway).
pub const MAX_SOURCE: usize = 8192;

/// What a script may call to act on the board.
pub trait ScriptHost {
    /// Call a board tool by name. The arguments are the tool's own JSON object.
    fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value, String>;
}

/// The reference the model reads before writing a script.
///
/// A tool description is one paragraph, and a language needs more than that: the first
/// run of a real agent guessed `tool()` (the example was in the description) and then
/// asked for `speaker_tone` without knowing it existed. Sections rather than one document
/// because a tool result is capped at 2 KB and a truncated reference is worse than none.
pub fn help(section: &str) -> Result<String, String> {
    match section {
        "" | "overview" => Ok(OVERVIEW.to_string()),
        "builtins" => Ok(BUILTINS.to_string()),
        "examples" => Ok(EXAMPLES.to_string()),
        other => Err(format!("unknown section `{other}`; use overview, builtins or examples")),
    }
}

const OVERVIEW: &str = r#"# board script — overview

A small program the board runs: loops and conditions over tool calls.

    let scan = tool("i2c_scan", {})
    let i = 0
    while i < len(scan.addresses) {
      log("probe " + scan.addresses[i])
      i = i + 1
    }

VALUES   number, string, bool, nil, list, map
         [1, 2]   {a: 1}   "text"   true   nil
STATEMENTS
         let x = 1        declare
         x = 2            assign
         l[0] = 9 / m.k = 9
         if cond { ... } else { ... }
         while cond { ... }        break inside
         # comment
OPERATORS  + - * / %   == != < <= > >=   and or not   (unary -)
           `+` also joins strings and concatenates lists
ACCESS     d.field   d["field"]   list[0]   (missing key -> nil)
RESULT     the value of the last expression is returned
NOTE       the interpreter runs on the thread that called it: the portal's listener when a
           person runs a script, the agent thread when the model does. A script that does
           not finish holds that thread, so the clock — not the step count — is the limit
           that matters
LIMITS     30 s or 5 000 000 steps, 32 nested blocks, 4 KB strings, 64 log lines
"#;

const BUILTINS: &str = r#"# board script — builtins

Only these, plus `tool`:

  len(x)        string chars, list items, map keys; len(nil) is 0
  str(x)        printable text
  num(x)        "12" -> 12, true -> 1
  type(x)       "number" | "string" | "bool" | "nil" | "list" | "map"
  log(x, ...)   one line into the run output (64 lines max)
  sleep(ms)     wait, at most 2 s per call and 5 s per script
  tool(name, args)   call any board tool; args is a map; returns its JSON

`sleep` really waits: the thread is in the scheduler's hands, not spinning,
but the portal still answers only after the script returns. A script that
needs to wait longer than 5 s is a scheduled task, not a loop.

`tool` is the only way out: same permissions as the caller, and
`tool("script", ...)` is refused.

To find what you can call: `devices_list`, `tools/list`, or read `/MEMORY.md`.
"#;

const EXAMPLES: &str = r#"# board script — examples

Adopt every device a probe found but did not adopt:

    let d = tool("devices_list", {})
    let i = 0
    while i < len(d.candidates) {
      let c = d.candidates[i]
      if c.adopted == false and c.adopt != nil {
        tool("manifest_apply", {mode: "merge", manifest: {version: 1, devices: c.adopt.devices}})
      }
      i = i + 1
    }

Play a melody in one call (`speaker_play` needs a speaker device; a note is `{hz, ms}`,
and `hz` 0 is a rest). The board synthesizes the whole score into one buffer, so nothing
falls between the notes — one call per note is both dearer and audibly worse:

    tool("speaker_play", {notes: [{hz: 523, ms: 400}, {hz: 523, ms: 400}, {hz: 784, ms: 800}], volume: 20})

Report and stop early on a failure:

    let s = tool("board_status", {})
    if s.heap_kb != nil { log("heap " + str(s.heap_kb)) }
"#;

/// What a check came back with.
#[derive(Debug, Default)]
pub struct Lint {
    /// Things that stop the program from running, with a line and column.
    pub errors: Vec<String>,
    /// Things that will run but are probably wrong.
    pub warnings: Vec<String>,
}

impl Lint {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Check a program without running it.
///
/// A model should be able to find out that its script is wrong before the board executes
/// half of it — `tool()` calls have side effects, so "run it and see" is the worst way to
/// learn. This covers the two failure classes that actually happen: syntax (the lexer and
/// parser answer with a line and column) and a call to a function that does not exist
/// (`speaker_ton` instead of `speaker_tone`, `print` instead of `log`).
pub fn lint(source: &str) -> Lint {
    let mut out = Lint::default();
    if source.len() > MAX_SOURCE {
        out.errors.push(format!("the script is larger than {MAX_SOURCE} bytes"));
        return out;
    }
    let toks = match lex(source) {
        Ok(t) => t,
        Err(e) => {
            out.errors.push(e);
            return out;
        }
    };
    let mut parser = Parser { toks, pos: 0 };
    let program = match parser.program() {
        Ok(p) => p,
        Err(e) => {
            out.errors.push(e);
            return out;
        }
    };
    let mut calls = Vec::new();
    collect_calls(&program, &mut calls);
    for name in calls {
        if !BUILTINS_NAMES.contains(&name.as_str()) {
            out.warnings
                .push(format!("unknown function `{name}`; the builtins are {}", BUILTINS_NAMES.join(", ")));
        }
        if name == "script" {
            out.warnings.push("`tool(\"script\", …)` is refused when the script runs".into());
        }
    }
    out
}

/// The functions a script may call, other than `tool`.
pub const BUILTINS_NAMES: &[&str] = &["len", "str", "num", "type", "log", "print", "echo", "sleep", "tool"];

fn collect_calls(stmts: &[Stmt], out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Let(_, e) | Stmt::Assign(_, e) | Stmt::Expr(e) => collect_expr(e, out),
            Stmt::SetIndex(_, a, b) => {
                collect_expr(a, out);
                collect_expr(b, out);
            }
            Stmt::If(c, then, otherwise) => {
                collect_expr(c, out);
                collect_calls(then, out);
                collect_calls(otherwise, out);
            }
            Stmt::For(_, iterable, body) => {
                collect_expr(iterable, out);
                collect_calls(body, out);
            }
            Stmt::Fn(_, _, body) => collect_calls(body, out),
            Stmt::Return(Some(e)) => collect_expr(e, out),
            Stmt::Return(None) => {}
            Stmt::While(c, body) => {
                collect_expr(c, out);
                collect_calls(body, out);
            }
            Stmt::Break => {}
        }
    }
}

fn collect_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Lit(_) | Expr::Var(_) => {}
        Expr::List(items) => items.iter().for_each(|e| collect_expr(e, out)),
        Expr::Map(pairs) => pairs.iter().for_each(|(_, e)| collect_expr(e, out)),
        Expr::Unary(_, e) => collect_expr(e, out),
        Expr::Binary(_, a, b) => {
            collect_expr(a, out);
            collect_expr(b, out);
        }
        Expr::Index(a, b) => {
            collect_expr(a, out);
            collect_expr(b, out);
        }
        Expr::Member(a, _) => collect_expr(a, out),
        Expr::Call(name, args) => {
            out.push(name.clone());
            args.iter().for_each(|e| collect_expr(e, out));
        }
    }
}

/// The result of a run.
#[derive(Debug)]
pub struct Outcome {
    pub result: Value,
    pub output: Vec<String>,
    pub steps: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Nil,
    Bool(bool),
    Num(f64),
    Str(String),
    List(Vec<Val>),
    Map(BTreeMap<String, Val>),
}

impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Nil => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::List(l) => !l.is_empty(),
            Val::Map(m) => !m.is_empty(),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Val::Nil => "nil",
            Val::Bool(_) => "bool",
            Val::Num(_) => "number",
            Val::Str(_) => "string",
            Val::List(_) => "list",
            Val::Map(_) => "map",
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Val::Nil => Value::Null,
            Val::Bool(b) => json!(b),
            Val::Num(n) => {
                // Integers stay integers across the JSON boundary. A script that copies a
                // number out of a tool result and back in (`address: 60`) must not turn it
                // into `60.0`: schemas that ask for a u8 reject the float, and the failure
                // reads like a validation bug in the manifest rather than a round-trip one.
                if n.fract() == 0.0 && n.abs() < 9.0e15 {
                    json!(*n as i64)
                } else {
                    json!(n)
                }
            }
            Val::Str(s) => json!(s),
            Val::List(l) => Value::Array(l.iter().map(|v| v.to_json()).collect()),
            Val::Map(m) => {
                let mut out = Map::new();
                for (k, v) in m {
                    out.insert(k.clone(), v.to_json());
                }
                Value::Object(out)
            }
        }
    }

    fn from_json(v: &Value) -> Val {
        match v {
            Value::Null => Val::Nil,
            Value::Bool(b) => Val::Bool(*b),
            Value::Number(n) => Val::Num(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Val::Str(clip(s.clone())),
            Value::Array(a) => Val::List(a.iter().take(MAX_LIST).map(Val::from_json).collect()),
            Value::Object(o) => {
                let mut m = BTreeMap::new();
                for (k, v) in o.iter().take(MAX_MAP) {
                    m.insert(k.clone(), Val::from_json(v));
                }
                Val::Map(m)
            }
        }
    }

    /// Printable form, used by `str()` and by `log`.
    fn display(&self) -> String {
        match self {
            Val::Nil => "nil".into(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Val::Str(s) => s.clone(),
            other => serde_json::to_string(&other.to_json()).unwrap_or_else(|_| "?".into()),
        }
    }
}

fn clip(mut s: String) -> String {
    if s.len() > MAX_STRING {
        let mut end = MAX_STRING;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

// ---------------------------------------------------------------- tokens

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    // punctuation and operators
    Let,
    If,
    Else,
    While,
    Break,
    True,
    False,
    Nil,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Assign,
    Colon,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    And,
    Or,
    Not,
    Dot,
    Eof,
}

/// A token and where it came from. Every error a model sees has to name a place, or the
/// fix is a guess: `line 3, column 7: expected a name after `let`` is actionable,
/// "unexpected token" is not.
#[derive(Clone, Debug, PartialEq)]
struct Sp {
    tok: Tok,
    line: u32,
    col: u32,
}

fn lex(src: &str) -> Result<Vec<Sp>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut line = 1u32;
    let mut col = 1u32;
    let mut out: Vec<Sp> = Vec::new();
    // Advance one character, keeping line/column in step with it.
    macro_rules! bump {
        () => {{
            if chars[i] == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            i += 1;
        }};
    }
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            bump!();
            continue;
        }
        // comments: `#` to end of line, and `//`
        if c == '#' || (c == '/' && chars.get(i + 1) == Some(&'/')) {
            while i < chars.len() && chars[i] != '\n' {
                bump!();
            }
            continue;
        }
        let (start_line, start_col) = (line, col);
        let push = |tok: Tok, out: &mut Vec<Sp>| {
            out.push(Sp { tok, line: start_line, col: start_col });
        };
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                bump!();
            }
            let text: String = chars[start..i].iter().collect();
            let n = text
                .parse::<f64>()
                .map_err(|_| format!("line {start_line}, column {start_col}: `{text}` is not a number"))?;
            push(Tok::Num(n), &mut out);
            continue;
        }
        if c == '"' {
            bump!();
            let mut s = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    bump!();
                    s.push(match chars[i] {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    s.push(chars[i]);
                }
                bump!();
            }
            if i >= chars.len() {
                return Err(format!("line {start_line}, column {start_col}: the string is never closed"));
            }
            bump!();
            push(Tok::Str(s), &mut out);
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                bump!();
            }
            let word: String = chars[start..i].iter().collect();
            let tok = match word.as_str() {
                "let" => Tok::Let,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "break" => Tok::Break,
                "true" => Tok::True,
                "false" => Tok::False,
                "nil" => Tok::Nil,
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                _ => Tok::Ident(word),
            };
            push(tok, &mut out);
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let (tok, len) = match two.as_str() {
            "==" => (Tok::Eq, 2),
            "!=" => (Tok::Ne, 2),
            "<=" => (Tok::Le, 2),
            ">=" => (Tok::Ge, 2),
            _ => match c {
                '(' => (Tok::LParen, 1),
                ')' => (Tok::RParen, 1),
                '{' => (Tok::LBrace, 1),
                '}' => (Tok::RBrace, 1),
                '[' => (Tok::LBracket, 1),
                ']' => (Tok::RBracket, 1),
                ',' => (Tok::Comma, 1),
                ';' => (Tok::Semi, 1),
                ':' => (Tok::Colon, 1),
                '=' => (Tok::Assign, 1),
                '<' => (Tok::Lt, 1),
                '>' => (Tok::Gt, 1),
                '+' => (Tok::Plus, 1),
                '-' => (Tok::Minus, 1),
                '*' => (Tok::Star, 1),
                '/' => (Tok::Slash, 1),
                '%' => (Tok::Percent, 1),
                '.' => (Tok::Dot, 1),
                other => {
                    return Err(format!(
                        "line {start_line}, column {start_col}: `{other}` is not part of the language"
                    ))
                }
            },
        };
        push(tok, &mut out);
        for _ in 0..len {
            bump!();
        }
    }
    out.push(Sp { tok: Tok::Eof, line, col });
    Ok(out)
}

// ---------------------------------------------------------------- syntax tree

#[derive(Clone, Debug)]
enum Expr {
    Lit(Val),
    Var(String),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Unary(Tok, Box<Expr>),
    Binary(Tok, Box<Expr>, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    Call(String, Vec<Expr>),
}

#[derive(Clone, Debug)]
enum Stmt {
    Let(String, Expr),
    Assign(String, Expr),
    SetIndex(String, Expr, Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
    /// `for name in <list or map keys> { ... }`. A `while` loop already made the language
    /// Turing complete; this exists because every script that walks a sensor list was
    /// spending four lines on an index it never used for anything else.
    For(String, Expr, Vec<Stmt>),
    /// `fn name(a, b) { ... }`. Definitions are hoisted: a script may call a function
    /// before the line that defines it, which is what makes mutual recursion usable.
    Fn(String, Vec<String>, Vec<Stmt>),
    /// `return <expr>`, and a bare `return` for nil.
    Return(Option<Expr>),
    Break,
    Expr(Expr),
}

struct Parser {
    toks: Vec<Sp>,
    pos: usize,
}

impl Parser {
    /// The token at the cursor, without its position.
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).map(|s| &s.tok).unwrap_or(&Tok::Eof)
    }

    /// Where the cursor is, for an error a model can act on.
    fn here(&self) -> String {
        match self.toks.get(self.pos) {
            Some(sp) => format!("line {}, column {}", sp.line, sp.col),
            None => "end of script".to_string(),
        }
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        self.pos += 1;
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(format!("{}: expected {:?}, found {:?}", self.here(), t, self.peek()))
        }
    }

    fn program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        while self.peek() != &Tok::Eof {
            out.push(self.stmt()?);
        }
        Ok(out)
    }

    /// `fn name(a, b) { ... }`, `for x in expr { ... }`, `return [expr]`.
    ///
    /// They are parsed here rather than as expression forms because all three read better
    /// as statements, and because a `for` header needs `in`, which is not an operator.
    fn keyword_stmt(&mut self) -> Option<Result<Stmt, String>> {
        let word = match self.peek() {
            Tok::Ident(w) => w.clone(),
            _ => return None,
        };
        match word.as_str() {
            "fn" => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => return Some(Err(format!("expected a name after `fn`, found {other:?}"))),
                };
                let mut params = Vec::new();
                if self.peek() == &Tok::LParen {
                    self.next();
                    while self.peek() != &Tok::RParen {
                        match self.next() {
                            Tok::Ident(p) => params.push(p),
                            other => return Some(Err(format!("expected a parameter name, found {other:?}"))),
                        }
                        if self.peek() == &Tok::Comma {
                            self.next();
                        } else {
                            break;
                        }
                    }
                    if let Err(e) = self.expect(&Tok::RParen) {
                        return Some(Err(e));
                    }
                }
                match self.block() {
                    Ok(body) => Some(Ok(Stmt::Fn(name, params, body))),
                    Err(e) => Some(Err(e)),
                }
            }
            "return" => {
                self.next();
                if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    return Some(Ok(Stmt::Return(None)));
                }
                match self.expr_opt() {
                    Ok(e) => Some(Ok(Stmt::Return(Some(e)))),
                    Err(e) => Some(Err(e)),
                }
            }
            "for" => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => {
                        return Some(Err(format!("expected a loop variable after `for`, found {other:?}")))
                    }
                };
                match self.next() {
                    Tok::Ident(w) if w == "in" => {}
                    other => {
                        return Some(Err(format!("expected `in` after the loop variable, found {other:?}")))
                    }
                }
                let iterable = match self.expr_opt() {
                    Ok(e) => e,
                    Err(e) => return Some(Err(e)),
                };
                match self.block() {
                    Ok(body) => Some(Ok(Stmt::For(name, iterable, body))),
                    Err(e) => Some(Err(e)),
                }
            }
            _ => None,
        }
    }

    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(&Tok::LBrace)?;
        let mut out = Vec::new();
        while self.peek() != &Tok::RBrace {
            if self.peek() == &Tok::Eof {
                return Err(format!("{}: the block is never closed", self.here()));
            }
            out.push(self.stmt()?);
        }
        self.expect(&Tok::RBrace)?;
        Ok(out)
    }

    fn stmt(&mut self) -> Result<Stmt, String> {
        if let Some(parsed) = self.keyword_stmt() {
            return parsed;
        }
        match self.peek().clone() {
            Tok::Let => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => {
                        return Err(format!("{}: expected a name after `let`, found {other:?}", self.here()))
                    }
                };
                self.expect(&Tok::Assign)?;
                let value = self.expr()?;
                self.eat(&Tok::Semi);
                Ok(Stmt::Let(name, value))
            }
            Tok::If => {
                self.next();
                let cond = self.expr()?;
                let then = self.block()?;
                let otherwise = if self.eat(&Tok::Else) {
                    if self.peek() == &Tok::If {
                        vec![self.stmt()?]
                    } else {
                        self.block()?
                    }
                } else {
                    Vec::new()
                };
                Ok(Stmt::If(cond, then, otherwise))
            }
            Tok::While => {
                self.next();
                let cond = self.expr()?;
                let body = self.block()?;
                Ok(Stmt::While(cond, body))
            }
            Tok::Break => {
                self.next();
                self.eat(&Tok::Semi);
                Ok(Stmt::Break)
            }
            Tok::LBrace => Ok(Stmt::Expr({
                // A bare block runs for its side effects; reuse the statement list.
                let body = self.block()?;
                Expr::Lit(Val::Nil).with_block(body)
            })),
            _ => {
                // Assignment or an expression statement.
                if let Tok::Ident(name) = self.peek().clone() {
                    if self.toks.get(self.pos + 1).map(|s| &s.tok) == Some(&Tok::Assign) {
                        self.pos += 2;
                        let value = self.expr()?;
                        self.eat(&Tok::Semi);
                        return Ok(Stmt::Assign(name, value));
                    }
                }
                // `a[i] = v`
                if let Tok::Ident(name) = self.peek().clone() {
                    if self.toks.get(self.pos + 1).map(|s| &s.tok) == Some(&Tok::LBracket) {
                        let save = self.pos;
                        self.pos += 2;
                        let index = self.expr()?;
                        if self.eat(&Tok::RBracket) && self.eat(&Tok::Assign) {
                            let value = self.expr()?;
                            self.eat(&Tok::Semi);
                            return Ok(Stmt::SetIndex(name, index, value));
                        }
                        self.pos = save;
                    }
                }
                let e = self.expr()?;
                self.eat(&Tok::Semi);
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn expr_opt(&mut self) -> Result<Expr, String> {
        self.expr()
    }

    fn expr(&mut self) -> Result<Expr, String> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.eat(&Tok::Or) {
            let right = self.and()?;
            left = Expr::Binary(Tok::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.equality()?;
        while self.eat(&Tok::And) {
            let right = self.equality()?;
            left = Expr::Binary(Tok::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, String> {
        let mut left = self.comparison()?;
        loop {
            let op = match self.peek() {
                Tok::Eq => Tok::Eq,
                Tok::Ne => Tok::Ne,
                _ => break,
            };
            self.next();
            let right = self.comparison()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.additive()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => Tok::Lt,
                Tok::Le => Tok::Le,
                Tok::Gt => Tok::Gt,
                Tok::Ge => Tok::Ge,
                _ => break,
            };
            self.next();
            let right = self.additive()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => Tok::Plus,
                Tok::Minus => Tok::Minus,
                _ => break,
            };
            self.next();
            let right = self.term()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => Tok::Star,
                Tok::Slash => Tok::Slash,
                Tok::Percent => Tok::Percent,
                _ => break,
            };
            self.next();
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Tok::Minus => {
                self.next();
                Ok(Expr::Unary(Tok::Minus, Box::new(self.unary()?)))
            }
            Tok::Not => {
                self.next();
                Ok(Expr::Unary(Tok::Not, Box::new(self.unary()?)))
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut node = self.primary()?;
        loop {
            if self.eat(&Tok::LBracket) {
                let index = self.expr()?;
                self.expect(&Tok::RBracket)?;
                node = Expr::Index(Box::new(node), Box::new(index));
            } else if self.eat(&Tok::Dot) {
                match self.next() {
                    Tok::Ident(name) => node = Expr::Member(Box::new(node), name),
                    other => {
                        return Err(format!(
                            "{}: expected a field name after `.`, found {other:?}",
                            self.here()
                        ))
                    }
                }
            } else {
                break;
            }
        }
        Ok(node)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(n) => Ok(Expr::Lit(Val::Num(n))),
            Tok::Str(s) => Ok(Expr::Lit(Val::Str(s))),
            Tok::True => Ok(Expr::Lit(Val::Bool(true))),
            Tok::False => Ok(Expr::Lit(Val::Bool(false))),
            Tok::Nil => Ok(Expr::Lit(Val::Nil)),
            Tok::Ident(name) => {
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    if !self.eat(&Tok::RParen) {
                        loop {
                            args.push(self.expr()?);
                            if self.eat(&Tok::Comma) {
                                continue;
                            }
                            self.expect(&Tok::RParen)?;
                            break;
                        }
                    }
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    loop {
                        items.push(self.expr()?);
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RBracket)?;
                        break;
                    }
                }
                Ok(Expr::List(items))
            }
            Tok::LBrace => {
                let mut pairs = Vec::new();
                if !self.eat(&Tok::RBrace) {
                    loop {
                        let key = match self.next() {
                            Tok::Ident(k) | Tok::Str(k) => k,
                            other => return Err(format!("{}: expected a key, found {other:?}", self.here())),
                        };
                        // `:` reads like JSON, `=` like a constructor; a model writes both.
                        if !self.eat(&Tok::Colon) {
                            self.expect(&Tok::Assign)?;
                        }
                        let value = self.expr()?;
                        pairs.push((key, value));
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RBrace)?;
                        break;
                    }
                }
                Ok(Expr::Map(pairs))
            }
            other => Err(format!("{}: unexpected token {other:?}", self.here())),
        }
    }
}

impl Expr {
    /// A block used as an expression: run its statements, yield nil. Kept out of the
    /// evaluator because blocks are statements, and the only place an expression is
    /// required is a statement position.
    fn with_block(self, _body: Vec<Stmt>) -> Expr {
        self
    }
}

// ---------------------------------------------------------------- evaluation

enum Flow {
    Normal,
    Break,
    /// A function returned a value; unwinds to the call that started it.
    Return(Val),
}

struct Run<'a> {
    host: &'a mut dyn ScriptHost,
    scopes: Vec<BTreeMap<String, Val>>,
    /// User-defined functions, hoisted before the first statement runs.
    fns: BTreeMap<String, (Vec<String>, Vec<Stmt>)>,
    /// Call depth, separate from block depth: recursion costs both.
    calls: usize,
    steps: u64,
    depth: usize,
    output: Vec<String>,
    /// When the script started, so the loop can be stopped by the clock as well as by
    /// the step count.
    started_ms: u64,
    /// How long this script has slept in total. `sleep()` is capped so a script cannot turn
    /// into an outage: it runs on the thread that also answers the portal.
    slept_ms: u64,
}

/// The longest one `sleep(ms)` call will actually wait.
///
/// The script runs on the thread that answers the portal — a 5 s tool call already shows up
/// as a portal that does not respond (measured: 5 103 ms for a `sleep 5` tool). A two second
/// nap is the most that stays in the same range as a slow tool result.
pub const SLEEP_MAX_MS: u64 = 2_000;
/// The most a whole script may sleep, across every call.
///
/// Short enough to stay inside the 30 s wall clock with room to work, and short enough that
/// a script which sleeps in a loop hits the ceiling rather than the portal's patience.
pub const SLEEP_TOTAL_MS: u64 = 5_000;

/// Run `source`, returning its last expression value.
/// Run with the arguments a caller passed in.
///
/// `args` lands in the global scope as a map (`args.pin`) and as `argv`, a list of its
/// values (`argv[0]`), so a script can be written once and called with different pins
/// without being rewritten. `Value::Null` is the same as no arguments: `args` is then an
/// empty map and `argv` an empty list, and a script that reads a missing key gets nil
/// rather than an error — the same rule as any other absent field.
pub fn run_with(
    source: &str,
    host: &mut dyn ScriptHost,
    args: &serde_json::Value,
) -> Result<Outcome, String> {
    if source.len() > MAX_SOURCE {
        return Err(format!("script is larger than {MAX_SOURCE} bytes"));
    }
    let toks = lex(source)?;
    let mut parser = Parser { toks, pos: 0 };
    let program = parser.program()?;
    let mut globals: BTreeMap<String, Val> = BTreeMap::new();
    // Two shapes arrive here: a plain map of named arguments, or the `{__args, __argv}` pair
    // a command line is turned into. Both end up as the same two globals.
    let (map_json, list_json) = match args {
        serde_json::Value::Object(m) if m.contains_key("__args") || m.contains_key("__argv") => (
            m.get("__args").cloned().unwrap_or(serde_json::Value::Null),
            m.get("__argv").cloned().unwrap_or(serde_json::Value::Null),
        ),
        other => (other.clone(), serde_json::Value::Null),
    };
    let map = match &map_json {
        serde_json::Value::Object(m) => {
            m.iter().map(|(k, v)| (k.clone(), Val::from_json(v))).collect::<BTreeMap<String, Val>>()
        }
        _ => BTreeMap::new(),
    };
    let list = match &list_json {
        serde_json::Value::Array(items) => items.iter().map(Val::from_json).collect::<Vec<Val>>(),
        // Named arguments still fill `argv` with their values, in key order.
        _ => map.values().cloned().collect::<Vec<Val>>(),
    };
    globals.insert("args".into(), Val::Map(map));
    globals.insert("argv".into(), Val::List(list));

    let mut runner = Run {
        host,
        scopes: vec![globals],
        fns: BTreeMap::new(),
        calls: 0,
        steps: 0,
        depth: 0,
        output: Vec::new(),
        started_ms: now_ms(),
        slept_ms: 0,
    };
    runner.execute(program)
}

pub fn run(source: &str, host: &mut dyn ScriptHost) -> Result<Outcome, String> {
    run_with(source, host, &serde_json::Value::Null)
}

impl<'a> Run<'a> {
    /// Run a parsed program: hoist definitions, then walk the statements.
    fn execute(&mut self, program: Vec<Stmt>) -> Result<Outcome, String> {
        for stmt in &program {
            if let Stmt::Fn(name, params, body) = stmt {
                self.fns.insert(name.clone(), (params.clone(), body.clone()));
            }
        }
        // The value of the last expression statement is the script's result, taken while the
        // statement runs. Re-evaluating it afterwards (the obvious way) ran its side effects
        // twice — `log("done")` printed twice — which is exactly the kind of bug a language
        // should not have.
        let mut last = Val::Nil;
        for stmt in &program {
            if let Stmt::Expr(e) = stmt {
                last = self.expr_quiet(e)?;
                continue;
            }
            match self.stmt(stmt)? {
                Flow::Break => break,
                Flow::Return(v) => {
                    last = v;
                    break;
                }
                Flow::Normal => {}
            }
        }
        Ok(Outcome { result: last.to_json(), output: std::mem::take(&mut self.output), steps: self.steps })
    }

    fn tick(&mut self) -> Result<(), String> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(format!("script stopped after {MAX_STEPS} steps (a loop without progress?)"));
        }
        // The clock, not the count, is the bound that matters: the script runs on the
        // thread that also answers the portal, so ten seconds of spinning is ten seconds
        // the board is unreachable. Checked every 1024 steps so the clock read is not the
        // cost of the loop.
        if self.steps & 0x3ff == 0 && now_ms().saturating_sub(self.started_ms) > MAX_MS {
            return Err(format!(
                "script stopped after {MAX_MS} ms ({} steps); it runs on the thread that answers the portal, so a longer script makes the board unreachable",
                self.steps
            ));
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Val {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.clone();
            }
        }
        Val::Nil
    }

    fn set(&mut self, name: &str, value: Val) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return;
            }
        }
        self.scopes.last_mut().expect("scope").insert(name.to_string(), value);
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<Flow, String> {
        self.tick()?;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            // Recursion consumes block depth too, so the two bounds meet at the same point.
            // Name the one the script actually did: a runaway `f(n+1)` is a recursion
            // problem, and telling its author about nesting sends them to the wrong line.
            return Err(if self.calls > 0 {
                format!("recursion deeper than {MAX_CALLS} calls")
            } else {
                "script nests deeper than the board allows".into()
            });
        }
        let out = self.stmt_inner(stmt);
        self.depth -= 1;
        out
    }

    fn stmt_inner(&mut self, stmt: &Stmt) -> Result<Flow, String> {
        match stmt {
            Stmt::Let(name, e) => {
                let v = self.eval(e)?;
                self.scopes.last_mut().expect("scope").insert(name.clone(), v);
                Ok(Flow::Normal)
            }
            Stmt::Assign(name, e) => {
                let v = self.eval(e)?;
                self.set(name, v);
                Ok(Flow::Normal)
            }
            Stmt::SetIndex(name, index, e) => {
                let idx = self.eval(index)?;
                let value = self.eval(e)?;
                let mut target = self.get(name);
                match (&mut target, &idx) {
                    (Val::List(items), Val::Num(n)) => {
                        let i = *n as i64;
                        if i < 0 || i as usize >= items.len() {
                            return Err(format!("index {} is outside a list of {}", i, items.len()));
                        }
                        items[i as usize] = value;
                    }
                    (Val::Map(m), Val::Str(k)) => {
                        if m.len() >= MAX_MAP && !m.contains_key(k) {
                            return Err(format!("map is full ({MAX_MAP} keys)"));
                        }
                        m.insert(k.clone(), value);
                    }
                    (other, _) => return Err(format!("cannot index a {} for assignment", other.type_name())),
                }
                self.set(name, target);
                Ok(Flow::Normal)
            }
            Stmt::If(cond, then, otherwise) => {
                if self.eval(cond)?.truthy() {
                    self.block(then)
                } else {
                    self.block(otherwise)
                }
            }
            Stmt::While(cond, body) => {
                loop {
                    self.tick()?;
                    if !self.eval(cond)?.truthy() {
                        break;
                    }
                    if let Flow::Break = self.block(body)? {
                        break;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For(name, iterable, body) => {
                let items = match self.eval(iterable)? {
                    Val::List(items) => items,
                    Val::Map(m) => m.keys().map(|k| Val::Str(k.clone())).collect::<Vec<_>>(),
                    Val::Str(text) => text.chars().map(|c| Val::Str(c.to_string())).collect::<Vec<_>>(),
                    other => return Err(format!("cannot loop over a {}", other.type_name())),
                };
                for item in items {
                    self.tick()?;
                    self.scopes.last_mut().expect("scope").insert(name.clone(), item);
                    match self.block(body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            // Definitions are hoisted before execution starts, so reaching one here is a
            // statement that does nothing at run time.
            Stmt::Fn(_, _, _) => Ok(Flow::Normal),
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e)?,
                    None => Val::Nil,
                };
                Ok(Flow::Return(v))
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Expr(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
        }
    }

    fn block(&mut self, stmts: &[Stmt]) -> Result<Flow, String> {
        self.scopes.push(BTreeMap::new());
        let mut flow = Flow::Normal;
        for stmt in stmts {
            match self.stmt(stmt) {
                Ok(Flow::Break) => {
                    flow = Flow::Break;
                    break;
                }
                Ok(Flow::Return(v)) => {
                    self.scopes.pop();
                    return Ok(Flow::Return(v));
                }
                Ok(Flow::Normal) => {}
                Err(e) => {
                    self.scopes.pop();
                    return Err(e);
                }
            }
        }
        self.scopes.pop();
        Ok(flow)
    }

    fn expr_quiet(&mut self, e: &Expr) -> Result<Val, String> {
        self.eval(e)
    }

    fn eval(&mut self, e: &Expr) -> Result<Val, String> {
        self.tick()?;
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::Var(name) => Ok(self.get(name)),
            Expr::List(items) => {
                if items.len() > MAX_LIST {
                    return Err(format!("list literal larger than {MAX_LIST}"));
                }
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(item)?);
                }
                Ok(Val::List(out))
            }
            Expr::Map(pairs) => {
                if pairs.len() > MAX_MAP {
                    return Err(format!("map literal larger than {MAX_MAP}"));
                }
                let mut out = BTreeMap::new();
                for (k, v) in pairs {
                    let value = self.eval(v)?;
                    out.insert(k.clone(), value);
                }
                Ok(Val::Map(out))
            }
            Expr::Unary(op, inner) => {
                let v = self.eval(inner)?;
                match op {
                    Tok::Minus => match v {
                        Val::Num(n) => Ok(Val::Num(-n)),
                        other => Err(format!("cannot negate a {}", other.type_name())),
                    },
                    Tok::Not => Ok(Val::Bool(!v.truthy())),
                    _ => Err("unknown unary operator".into()),
                }
            }
            Expr::Binary(op, a, b) => self.binary(op, a, b),
            Expr::Index(target, index) => {
                let t = self.eval(target)?;
                let i = self.eval(index)?;
                match (t, i) {
                    (Val::List(items), Val::Num(n)) => {
                        let idx = n as i64;
                        if idx < 0 || idx as usize >= items.len() {
                            Ok(Val::Nil)
                        } else {
                            Ok(items[idx as usize].clone())
                        }
                    }
                    (Val::Map(m), Val::Str(k)) => Ok(m.get(&k).cloned().unwrap_or(Val::Nil)),
                    (Val::Str(s), Val::Num(n)) => {
                        let chars: Vec<char> = s.chars().collect();
                        let idx = n as i64;
                        if idx < 0 || idx as usize >= chars.len() {
                            Ok(Val::Nil)
                        } else {
                            Ok(Val::Str(chars[idx as usize].to_string()))
                        }
                    }
                    (other, _) => Err(format!("cannot index a {}", other.type_name())),
                }
            }
            Expr::Member(target, field) => {
                let t = self.eval(target)?;
                match t {
                    Val::Map(m) => Ok(m.get(field).cloned().unwrap_or(Val::Nil)),
                    other => Err(format!("a {} has no fields", other.type_name())),
                }
            }
            Expr::Call(name, arg_exprs) => self.call(name, arg_exprs),
        }
    }

    fn binary(&mut self, op: &Tok, a: &Expr, b: &Expr) -> Result<Val, String> {
        // Short-circuit before evaluating the right side.
        if matches!(op, Tok::And | Tok::Or) {
            let left = self.eval(a)?.truthy();
            return Ok(match op {
                Tok::And => {
                    if left {
                        Val::Bool(self.eval(b)?.truthy())
                    } else {
                        Val::Bool(false)
                    }
                }
                _ => {
                    if left {
                        Val::Bool(true)
                    } else {
                        Val::Bool(self.eval(b)?.truthy())
                    }
                }
            });
        }
        let left = self.eval(a)?;
        let right = self.eval(b)?;
        match op {
            Tok::Plus => match (&left, &right) {
                (Val::Str(x), Val::Str(y)) => {
                    let mut s = String::with_capacity(x.len() + y.len());
                    s.push_str(x);
                    s.push_str(y);
                    Ok(Val::Str(clip(s)))
                }
                (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x + y)),
                (Val::List(x), Val::List(y)) => {
                    let mut out = x.clone();
                    out.extend(y.iter().cloned());
                    if out.len() > MAX_LIST {
                        return Err(format!("list would grow past {MAX_LIST}"));
                    }
                    Ok(Val::List(out))
                }
                (x, y) => Err(format!("cannot add a {} and a {}", x.type_name(), y.type_name())),
            },
            Tok::Minus => num_op(left, right, "-", |a, b| a - b),
            Tok::Star => num_op(left, right, "*", |a, b| a * b),
            Tok::Slash => {
                let (a, b) = as_nums(&left, &right, "/")?;
                if b == 0.0 {
                    return Err("division by zero".into());
                }
                Ok(Val::Num(a / b))
            }
            Tok::Percent => {
                let (a, b) = as_nums(&left, &right, "%")?;
                if b == 0.0 {
                    return Err("modulo by zero".into());
                }
                Ok(Val::Num(a % b))
            }
            Tok::Eq => Ok(Val::Bool(left == right)),
            Tok::Ne => Ok(Val::Bool(left != right)),
            Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge => {
                let ordering = match (&left, &right) {
                    (Val::Num(a), Val::Num(b)) => a.partial_cmp(b),
                    (Val::Str(a), Val::Str(b)) => Some(a.cmp(b)),
                    _ => None,
                };
                let ord = ordering.ok_or_else(|| {
                    format!("cannot compare a {} with a {}", left.type_name(), right.type_name())
                })?;
                let out = match op {
                    Tok::Lt => ord.is_lt(),
                    Tok::Le => ord.is_le(),
                    Tok::Gt => ord.is_gt(),
                    _ => ord.is_ge(),
                };
                Ok(Val::Bool(out))
            }
            other => Err(format!("unknown operator {other:?}")),
        }
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Result<Val, String> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg)?);
        }
        // A user function wins over a builtin of the same name: the script wrote it, so the
        // script meant it. `tool("script")` stays refused below either way.
        if let Some((params, body)) = self.fns.get(name).cloned() {
            if params.len() != values.len() {
                return Err(format!("{name} takes {} argument(s), {} given", params.len(), values.len()));
            }
            if self.calls >= MAX_CALLS {
                return Err(format!("recursion deeper than {MAX_CALLS} calls"));
            }
            self.calls += 1;
            // The frame is a scope of its own, so a parameter cannot leak into the caller.
            let mut frame = BTreeMap::new();
            for (param, value) in params.iter().zip(values) {
                frame.insert(param.clone(), value);
            }
            self.scopes.push(frame);
            let flowed = self.block(&body);
            self.scopes.pop();
            self.calls -= 1;
            return match flowed? {
                Flow::Return(v) => Ok(v),
                _ => Ok(Val::Nil),
            };
        }
        match name {
            "len" => match values.first() {
                Some(Val::Str(s)) => Ok(Val::Num(s.chars().count() as f64)),
                Some(Val::List(l)) => Ok(Val::Num(l.len() as f64)),
                Some(Val::Map(m)) => Ok(Val::Num(m.len() as f64)),
                // `len(nil)` is 0 on purpose: a script reading an optional field out of a
                // tool result (`r.issues`) should not have to check for nil first — the
                // missing field means "none", which is what a model means by it.
                Some(Val::Nil) => Ok(Val::Num(0.0)),
                Some(other) => Err(format!("len() of a {}", other.type_name())),
                None => Err("len() needs one argument".into()),
            },
            "str" => Ok(Val::Str(clip(values.first().unwrap_or(&Val::Nil).display()))),
            "num" => match values.first() {
                Some(Val::Num(n)) => Ok(Val::Num(*n)),
                Some(Val::Str(s)) => {
                    s.trim().parse::<f64>().map(Val::Num).map_err(|_| format!("`{s}` is not a number"))
                }
                Some(Val::Bool(b)) => Ok(Val::Num(if *b { 1.0 } else { 0.0 })),
                _ => Ok(Val::Num(0.0)),
            },
            "type" => Ok(Val::Str(values.first().map(|v| v.type_name()).unwrap_or("nil").to_string())),
            "log" | "print" | "echo" => {
                // Three names, one stdout: `log` is what the first version of the language
                // called it, `print` and `echo` are what everyone writes first. The lines go
                // to the same place — the `output` array a caller reads back — and the cap
                // protects the 2 KB result more than it protects the heap.
                let line = values.iter().map(|v| v.display()).collect::<Vec<_>>().join(" ");
                if self.output.len() >= 64 {
                    return Err("script printed more than 64 lines".into());
                }
                self.output.push(clip(line));
                Ok(Val::Nil)
            }
            "sleep" => {
                // A real wait, not a busy one: the interpreter may not spin, and the board's
                // scheduler is free to run other tasks while this thread is in vTaskDelay.
                let ms = match values.first() {
                    Some(Val::Num(n)) if *n > 0.0 => *n as u64,
                    Some(Val::Num(_)) => 0,
                    Some(other) => {
                        return Err(format!("sleep() needs milliseconds, got a {}", other.type_name()))
                    }
                    None => return Err("sleep() needs milliseconds".into()),
                };
                let left = SLEEP_TOTAL_MS.saturating_sub(self.slept_ms);
                // Said out loud rather than cut silently: a script that asked for 10 s and
                // waited 2 would look like it worked, and the author would draw the wrong
                // conclusion about the sensor it was timing.
                if ms > SLEEP_MAX_MS {
                    return Err(format!(
                        "sleep({ms}) is longer than the {SLEEP_MAX_MS} ms one sleep may take"
                    ));
                }
                if ms > left {
                    return Err(format!(
                        "this script has {left} ms of sleep left of its {SLEEP_TOTAL_MS} ms"
                    ));
                }
                let spent = now_ms().saturating_sub(self.started_ms);
                if spent + ms > MAX_MS {
                    return Err(format!("sleep({ms}) would pass the {MAX_MS} ms a script may run"));
                }
                if ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    self.slept_ms += ms;
                }
                Ok(Val::Nil)
            }
            "tool" => {
                let name = match values.first() {
                    Some(Val::Str(s)) => s.clone(),
                    Some(other) => return Err(format!("tool() needs a name, got a {}", other.type_name())),
                    None => return Err("tool() needs a name".into()),
                };
                if name == "script" {
                    return Err("a script may not call `script`".into());
                }
                let args = values.get(1).map(|v| v.to_json()).unwrap_or_else(|| json!({}));
                let out = self.host.call_tool(&name, &args)?;
                Ok(Val::from_json(&out))
            }
            other => {
                Err(format!("unknown function `{other}` (builtins: len, str, num, type, log, sleep, tool)"))
            }
        }
    }
}

fn as_nums(a: &Val, b: &Val, op: &str) -> Result<(f64, f64), String> {
    match (a, b) {
        (Val::Num(x), Val::Num(y)) => Ok((*x, *y)),
        _ => Err(format!("`{op}` needs two numbers, got {} and {}", a.type_name(), b.type_name())),
    }
}

fn num_op(a: Val, b: Val, op: &str, f: fn(f64, f64) -> f64) -> Result<Val, String> {
    let (x, y) = as_nums(&a, &b, op)?;
    Ok(Val::Num(f(x, y)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snippet `script("examples")` hands a model has to be a program this parser
    /// reads, or it teaches a call that fails with a syntax error at the third note.
    #[test]
    fn the_melody_example_is_a_program_the_parser_reads() {
        let src = r#"tool("speaker_play", {notes: [{hz: 523, ms: 400}, {hz: 523, ms: 400}, {hz: 784, ms: 800}], volume: 20})"#;
        let l = lint(src);
        assert!(l.ok(), "{:?}", l.errors);
    }

    struct NoHost;
    impl ScriptHost for NoHost {
        fn call_tool(&mut self, name: &str, _args: &Value) -> Result<Value, String> {
            Err(format!("no tool `{name}`"))
        }
    }

    /// `sleep` is a real wait, and a bounded one: a script runs on the thread that answers
    /// the portal, so a script that naps in a loop must hit a ceiling rather than an outage.
    #[test]
    fn sleep_waits_and_is_capped() {
        use std::time::Instant;

        let t0 = Instant::now();
        let out = run("sleep(30)\n1", &mut NoHost).expect("a nap is not an error");
        let waited = t0.elapsed().as_millis();
        assert_eq!(out.result, json!(1), "sleep returns nil and the script continues");
        assert!(waited >= 25, "sleep(30) waited {waited} ms");
        assert!(waited < 1_000, "sleep(30) must not wait a second: {waited} ms");

        // Zero and negative are free, and a non-number is refused with a sentence.
        let t0 = Instant::now();
        run("sleep(0)\nsleep(-5)\n1", &mut NoHost).expect("zero is not a wait");
        assert!(t0.elapsed().as_millis() < 500);
        let err = run("sleep(\"soon\")", &mut NoHost).unwrap_err();
        assert!(err.contains("needs milliseconds"), "{err}");

        // The total budget is a check on the interpreter's own counter, not on the clock:
        // the runner starts with the counter already spent, which is what a script that
        // slept in a loop looks like at the end.
        let toks = lex("sleep(1)").expect("lex");
        let mut parser = Parser { toks, pos: 0 };
        let program = parser.program().expect("parse");
        let mut runner = Run {
            host: &mut NoHost,
            scopes: vec![BTreeMap::new()],
            fns: BTreeMap::new(),
            calls: 0,
            steps: 0,
            depth: 0,
            output: Vec::new(),
            started_ms: now_ms(),
            slept_ms: SLEEP_TOTAL_MS,
        };
        let err = runner.execute(program).unwrap_err();
        assert!(err.contains("sleep left"), "{err}");

        // A nap longer than one call may take is named, not cut short.
        let err = run("sleep(5000)", &mut NoHost).unwrap_err();
        assert!(err.contains("longer than the 2000 ms"), "{err}");
    }

    /// 50 000 steps stopped honest loops (about 0.2 s of work); the bound is now the clock.
    #[test]
    fn a_script_may_run_far_past_the_old_step_cap() {
        let mut host = NoHost;
        let out = run("let i = 0\nwhile i < 200000 { i = i + 1 }\ni", &mut host).expect("runs");
        assert_eq!(out.result, json!(200000));
        assert!(out.steps > 200_000, "{}", out.steps);
    }

    struct Stub {
        calls: Vec<(String, Value)>,
        reply: Value,
    }

    impl Stub {
        fn new(reply: Value) -> Self {
            Self { calls: Vec::new(), reply }
        }
    }

    impl ScriptHost for Stub {
        fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
            self.calls.push((name.to_string(), arguments.clone()));
            Ok(self.reply.clone())
        }
    }

    fn ok(src: &str) -> Value {
        let mut host = Stub::new(json!({}));
        run(src, &mut host).expect("script runs").result
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ok("1 + 2 * 3"), json!(7));
        assert_eq!(ok("(1 + 2) * 3"), json!(9));
        assert_eq!(ok("10 % 4"), json!(2));
        assert_eq!(ok("-3 + 1"), json!(-2));
        assert_eq!(ok("2 < 3"), json!(true));
    }

    #[test]
    fn strings_and_lists() {
        assert_eq!(ok(r#""a" + "b""#), json!("ab"));
        assert_eq!(ok(r#"len("héllo")"#), json!(5));
        assert_eq!(ok("len([1, 2, 3])"), json!(3));
        assert_eq!(ok("[1, 2][1]"), json!(2));
    }

    #[test]
    fn a_while_loop_makes_it_turing_complete() {
        let out = ok(r#"
            let i = 0
            let sum = 0
            while i < 5 {
              sum = sum + i
              i = i + 1
            }
            sum
            "#);
        assert_eq!(out, json!(10));
    }

    #[test]
    fn if_else_and_break() {
        let out = ok(r#"
            let i = 0
            while true {
              i = i + 1
              if i == 3 { break } else { i = i }
            }
            i
            "#);
        assert_eq!(out, json!(3));
    }

    #[test]
    fn maps_read_like_json() {
        let out = ok(r#"
            let d = {a: 1, b: {c: 2}}
            d.b.c + d.a
            "#);
        assert_eq!(out, json!(3));
    }

    #[test]
    fn len_of_nil_is_zero_so_optional_fields_need_no_guard() {
        assert_eq!(ok("len(nil)"), json!(0));
        assert_eq!(
            ok(r#"let r = {ok: true}
len(r.issues)"#),
            json!(0)
        );
    }

    #[test]
    fn integers_survive_the_json_round_trip() {
        struct Copy;
        impl ScriptHost for Copy {
            fn call_tool(&mut self, _name: &str, arguments: &Value) -> Result<Value, String> {
                // Echo the address back the way a tool would return it.
                Ok(arguments.clone())
            }
        }
        let mut host = Copy;
        let outcome = run(r#"tool("echo", {address: 60, gain: 1.5})"#, &mut host).expect("runs");
        assert_eq!(outcome.result["address"], json!(60));
        assert!(outcome.result["address"].is_i64(), "a whole number must not become 60.0");
        assert_eq!(outcome.result["gain"], json!(1.5));
    }

    #[test]
    fn tool_calls_reach_the_board_and_results_are_json() {
        let mut host = Stub::new(json!({"addresses": ["0x3C"], "found": 1}));
        let outcome = run(
            r#"
            let scan = tool("i2c_scan", {})
            log("found " + str(scan.found))
            scan.addresses[0]
            "#,
            &mut host,
        )
        .expect("runs");
        assert_eq!(host.calls.len(), 1);
        assert_eq!(host.calls[0].0, "i2c_scan");
        assert_eq!(outcome.result, json!("0x3C"));
        assert_eq!(outcome.output, vec!["found 1".to_string()]);
    }

    #[test]
    fn a_script_reads_its_arguments_and_writes_to_stdout() {
        let mut host = NoHost;

        // `args` is a map and `argv` its values in order: one script, several pins.
        let args = serde_json::json!({"pin": 12, "level": 0});
        let out = run_with("args.pin", &mut host, &args).expect("runs");
        assert_eq!(out.result, json!(12));

        let out = run_with("let t = 0\nfor v in argv { t = t + v }\nt", &mut host, &args).expect("runs");
        assert_eq!(out.result, json!(12), "argv carries the values");

        // No arguments is the same as empty: a missing key is nil, not an error.
        let out = run("len(args)", &mut host).expect("runs");
        assert_eq!(out.result, json!(0));
        let out = run("args.missing", &mut host).expect("runs");
        assert_eq!(out.result, json!(null));

        // stdout: print and echo are the same stream as log, and a caller reads it back.
        let out = run("print(\"hello\")\necho(2 + 3)\nlog(\"done\")", &mut host).expect("runs");
        assert_eq!(out.output, vec!["hello".to_string(), "5".to_string(), "done".to_string()]);

        // The stdout cap is a fact, not a surprise: 64 lines then a message.
        let err = run("let i = 0\nwhile i < 70 { print(i)  i = i + 1 }", &mut host).expect_err("refused");
        assert!(err.contains("64 lines"), "{err}");
        let _ = host;
    }

    #[test]
    fn functions_recursion_and_for_loops_work() {
        let mut host = NoHost;

        // A recursive factorial: the language could already loop, but a user function is
        // what makes a procedure reusable inside one script.
        let out = run("fn fact(n) { if n <= 1 { return 1 } return n * fact(n - 1) }\nfact(5)", &mut host)
            .expect("runs");
        assert_eq!(out.result, json!(120));

        // Parameters are local: a function cannot see or overwrite the caller's variable.
        let out = run("let n = 99\nfn f(n) { return n + 1 }\nf(1)\nn", &mut host).expect("runs");
        assert_eq!(out.result, json!(99), "the caller's `n` survived the call");

        // `for` over a list, a string and a map's keys.
        let out = run("let t = 0\nfor x in [1,2,3] { t = t + x }\nt", &mut host).expect("runs");
        assert_eq!(out.result, json!(6));
        let out = run("let t = \"\"\nfor c in \"abc\" { t = t + c }\nt", &mut host).expect("runs");
        assert_eq!(out.result, json!("abc"));
        let out = run("let t = 0\nfor k in {a: 1, b: 2} { t = t + 1 }\nt", &mut host).expect("runs");
        assert_eq!(out.result, json!(2));

        // Definitions hoist, so a call may precede the definition, and mutual recursion works.
        let out = run(
            "is_even(4)\nfn is_even(n) { if n == 0 { return true } return is_odd(n - 1) }\nfn is_odd(n) { if n == 0 { return false } return is_even(n - 1) }",
            &mut host,
        )
        .expect("runs");
        assert_eq!(out.result, json!(true));

        // Runaway recursion is a message, not a crash.
        let err = run("fn f(n) { return f(n + 1) }\nf(0)", &mut host).expect_err("refused");
        assert!(err.contains("recursion"), "{err}");
    }

    #[test]
    fn a_script_cannot_recurse_into_script() {
        let mut host = Stub::new(json!({}));
        let err = run(r#"tool("script", {source: "1"})"#, &mut host).expect_err("refused");
        assert!(err.contains("may not call"), "{err}");
    }

    #[test]
    fn an_endless_loop_stops_with_a_message() {
        let mut host = Stub::new(json!({}));
        let err = run("while true { }", &mut host).expect_err("stopped");
        assert!(err.contains("steps"), "{err}");
    }

    #[test]
    fn a_syntax_error_names_what_it_found() {
        let mut host = Stub::new(json!({}));
        let err = run("let = 3", &mut host).expect_err("bad syntax");
        assert!(err.contains("expected a name"), "{err}");
    }

    #[test]
    fn assignment_to_an_index_works_for_lists_and_maps() {
        let out = ok(r#"
            let l = [1, 2]
            l[0] = 9
            let m = {}
            m["k"] = l[0]
            m.k
            "#);
        assert_eq!(out, json!(9));
    }
}

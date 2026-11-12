//! The agent loop: one queued user turn in, a bounded sequence of model calls and
//! board tool calls out, and a transcript the portal can render.
//!
//! The loop is deliberately small and single-threaded. Everything that touches
//! the board arrives through [`ToolHost`], everything that touches the network
//! through [`ChatTransport`], and time is passed in as a `fn() -> u64` so the
//! loop itself never reads a clock and stays testable on the host.
//!
//! Bounds, because the board has ~300 KB of usable heap: the history lives on the heap
//! while it is in use, and a bounded tail of it lives in NVS between boots
//! ([`MAX_SAVED_BYTES`]; [`transcript_jsonl`] and [`transcript_from_jsonl`] are the
//! format, `App` owns the store side).
//!
//! * at most [`MAX_HISTORY_MESSAGES`] messages and [`MAX_HISTORY_BYTES`] of text;
//! * a user message is cut to [`MAX_USER_BYTES`];
//! * a tool result is cut to [`MAX_TOOL_RESULT_BYTES`] with `…(truncated)`;
//! * an assistant text is cut to [`MAX_REPLY_BYTES`];
//! * at most `cfg.max_steps` model calls per run (never an unbounded ping-pong).

use crate::llm::{self, ChatTransport, LlmConfig, ToolCall};
use serde_json::{json, Value};

/// Oldest messages are dropped above this count.
pub const MAX_HISTORY_MESSAGES: usize = 256;
/// Oldest messages are dropped above this much text.
///
/// Not a physical limit of the chip: the transcript and the request built from it live
/// in PSRAM (8 MB here), and the two ceilings that used to bind, the internal-RAM
/// allocations and the mbedTLS buffers, were both moved off it. What this bounds is the
/// *request body*: tools (about 18 KB of JSON), the system prompt, and this text all go
/// out in one POST, and the board has to build and send that body while its internal
/// SRAM is down to a few kilobytes after TLS.
///
/// Measured on the bench, and then measured again at a larger size (see
/// `docs/troubleshooting.md`): 507 KB of transcript still served turns, and the board
/// rebooted somewhere below 1 MB with the cause not yet captured. 512 KB is therefore the
/// highest size with a measurement behind it, not a wish.
pub const MAX_HISTORY_BYTES: usize = 512 * 1024;
/// How many question-and-answer exchanges survive compression verbatim.
///
/// Four is enough to keep the thread of a task (what was asked, what the last tools did)
/// while leaving most of the budget for the retrieval work in progress.
pub const COMPACT_KEEP_TURNS: usize = 4;
/// The deterministic index of dropped exchanges is capped too: an index that grows without
/// limit would take the budget back through the system prompt.
pub const ARCHIVE_MAX_LINES: usize = 40;
/// A tool result larger than this is cut with [`llm::TRUNCATED_MARK`].
pub const MAX_TOOL_RESULT_BYTES: usize = 8192;
/// Assistant text larger than this is cut with [`llm::TRUNCATED_MARK`].
pub const MAX_REPLY_BYTES: usize = 8 * 1024;
/// A queued user message larger than this is cut before it enters the history.
pub const MAX_USER_BYTES: usize = 4096;

/// Reasoning is shown in the portal but never sent back as answer text, so it gets a
/// tighter cap than the reply itself.
pub const MAX_REASONING_BYTES: usize = 4096;

/// The most transcript bytes the board keeps between boots.
///
/// Not a storage limit: the tail is written through [`crate::store::KvStore`], and the
/// board's NVS holds far more than this. It is a RAM limit. Building the stored form
/// means holding a copy of the tail (this buffer) next to the history that is already on
/// the heap, on a board with kilobytes of internal SRAM left once TLS is up. Raising it
/// without a measurement of exactly that would trade a rare lost conversation for a
/// board that reboots under load, so the number stays where it is until somebody
/// measures it.
///
/// What the budget buys at the per-message caps below: roughly fifty messages of text,
/// which is the tail of a real task rather than a history.
pub const MAX_SAVED_BYTES: usize = 16 * 1024;
/// Text longer than this is cut when the message is stored, with
/// [`llm::TRUNCATED_MARK`]. The message in RAM keeps the whole text; only the copy that
/// survives a reboot is shortened.
pub const MAX_SAVED_TEXT_BYTES: usize = 300;
/// Reasoning gets less room than the answer: it is folded away in the portal and no
/// request needs it back in full.
pub const MAX_SAVED_REASONING_BYTES: usize = 200;
/// Tool arguments are echoed to the model when the transcript is rebuilt, so they are
/// stored; a `write` of a whole file is not worth the budget, so they are cut too.
pub const MAX_SAVED_ARG_BYTES: usize = 256;
/// The stored format's version. A blob written by a version this build does not know is
/// refused rather than parsed on a guess.
pub const TRANSCRIPT_VERSION: u32 = 1;
/// Room left for the header line when the tail is priced against [`MAX_SAVED_BYTES`].
/// The header carries five fields and a 16-hex-digit checksum; this is a ceiling, so the
/// assembled blob cannot exceed the budget by the length of its own header.
const SAVED_HEADER_RESERVE: usize = 128;

/// When compaction fires, as a fraction of the declared context window.
///
/// Same ratio as the host agent's `trigger_tokens` (four fifths of the window): the
/// remaining fifth is what a reply and the tool schemas get to use.
pub fn trigger_tokens(context_tokens: u32) -> u32 {
    let window = context_tokens.max(1) as u64;
    ((window * 4) / 5).max(1) as u32
}

/// How much of the tail survives compression verbatim, in tokens.
///
/// The host computes `min(RECENT_HISTORY, trigger / 4)`; the board has no 8000-token
/// history budget to spend, so its cap is a quarter of the trigger and never more than
/// what is left after the reply reserve. One exchange is always kept.
pub fn keep_recent_tokens(context_tokens: u32, max_output_tokens: u32) -> u32 {
    let trigger = trigger_tokens(context_tokens);
    let room = trigger.saturating_sub(max_output_tokens.min(trigger.saturating_sub(1)));
    (trigger / 4).min(room).max(1)
}

/// Whether `c` falls in a CJK range whose characters tokenise to about one token each,
/// rather than the ~4 characters per token of ASCII. Ported from the host agent so both
/// sides estimate the same text the same way.
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0x3400..=0x4DBF   // CJK Extension A
        | 0x3040..=0x30FF   // Hiragana + Katakana
        | 0xAC00..=0xD7AF   // Hangul Syllables
        | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
        | 0x20000..=0x2A6DF // CJK Extension B
    )
}

/// Quarters of a token per character: CJK 1.5, ASCII 0.25, everything else 0.5.
/// Rounded up by [`estimate_tokens`], so the estimate errs high and compaction fires
/// early rather than at the provider's 400.
fn char_quarters(c: char) -> u64 {
    if is_cjk(c) {
        6
    } else if c.is_ascii() {
        1
    } else {
        2
    }
}

/// Estimate the tokens in a piece of text, the same way the host agent does.
pub fn estimate_tokens(text: &str) -> u32 {
    let quarters = text.chars().fold(0_u64, |n, c| n.saturating_add(char_quarters(c)));
    quarters.saturating_add(3).div_euclid(4).min(u32::MAX as u64) as u32
}

/// The run journal is a ring: enough to render one run in the portal, small enough to
/// live in RAM next to the transcript on a board with a few hundred KB of internal heap.
pub const MAX_EVENTS: usize = 96;

/// Who produced a message. Serialised as the OpenAI role name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(text: &str) -> Role {
        match text {
            "system" => Role::System,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => Role::User,
        }
    }
}

/// One history entry. `text` is the assistant text or the tool result; the tool
/// call plumbing is only filled in for the roles that have it.
#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    /// The visible text: the assistant's answer, or the tool's output.
    pub text: String,
    /// The model's reasoning for this message, when it gave any. Kept as its own
    /// block (the host agent calls it `Reasoning`) so it can be shown, folded away or
    /// echoed back to a reasoner without polluting the answer.
    pub reasoning: String,
    /// Assistant messages only.
    pub tool_calls: Vec<ToolCall>,
    /// Tool messages only: which call this answers.
    pub tool_call_id: Option<String>,
    /// Tool messages only: did the call fail? (the host agent's `is_error`)
    pub is_error: bool,
    /// `""` | `"http"` | `"schedule:<id>"` | `"sensor:<id>"`; user messages only.
    pub source: String,
    pub at_ms: u64,
    /// Images the user attached, as base64 with their mime type. They ride on the
    /// message rather than in a side table so the request that carries them and the
    /// transcript that reports them cannot disagree.
    pub attachments: Vec<Attachment>,
}

/// One image attached to a user message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    /// Base64 of the bytes, exactly as it arrived: re-encoding on every request would
    /// cost more than the bytes themselves.
    pub data_b64: String,
}

impl Message {
    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    pub fn user(text: &str, source: &str, at_ms: u64) -> Self {
        Self {
            role: Role::User,
            text: llm::truncate_bytes(text, MAX_USER_BYTES),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
            source: source.to_string(),
            at_ms,
            attachments: Vec::new(),
        }
    }

    pub fn assistant(text: &str, tool_calls: Vec<ToolCall>, at_ms: u64) -> Self {
        Self::assistant_with_reasoning(text, "", tool_calls, at_ms)
    }

    /// An assistant turn that also carries the model's reasoning, the way the host
    /// agent stores a `Reasoning` block next to the `Text` one.
    pub fn assistant_with_reasoning(
        text: &str,
        reasoning: &str,
        tool_calls: Vec<ToolCall>,
        at_ms: u64,
    ) -> Self {
        Self {
            role: Role::Assistant,
            text: llm::truncate_bytes(text, MAX_REPLY_BYTES),
            reasoning: llm::truncate_bytes(reasoning, MAX_REASONING_BYTES),
            tool_calls,
            tool_call_id: None,
            is_error: false,
            source: String::new(),
            at_ms,
            attachments: Vec::new(),
        }
    }

    pub fn tool(tool_call_id: &str, text: &str, at_ms: u64) -> Self {
        Self::tool_result(tool_call_id, text, false, at_ms)
    }

    /// A tool result, with the host agent's `is_error` flag rather than inferring it
    /// from the payload.
    pub fn tool_result(tool_call_id: &str, text: &str, is_error: bool, at_ms: u64) -> Self {
        Self {
            role: Role::Tool,
            text: llm::truncate_bytes(text, MAX_TOOL_RESULT_BYTES),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.to_string()),
            is_error,
            source: String::new(),
            at_ms,
            attachments: Vec::new(),
        }
    }
}

/// Everything the loop needs from the board, implemented by `App`.
pub trait ToolHost {
    /// The owner's own system-prompt text, or `None` when they have not written any.
    fn custom_prompt(&self) -> Option<String> {
        None
    }

    /// OpenAI `tools` array.
    fn tool_specs(&self) -> Value;
    /// Run one tool call; the result is JSON text (<= 2 KB enforced here).
    fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String, String>;

    /// Project instructions that always apply, already bounded by the host.
    ///
    /// `None` is the normal state: a board with no `/AGENTS.md` behaves exactly as it
    /// did before the file existed.
    fn project_instructions(&self) -> Option<String> {
        None
    }

    /// Skills the model may open on demand. Only the index goes into the prompt; the
    /// body arrives when the model reads `path`.
    ///
    /// A host with no skills returns nothing, which is the state a fresh board is in.
    fn skills(&self) -> Vec<SkillRef> {
        Vec::new()
    }

    /// The memory index: `/MEMORY.md`, already bounded by the host.
    ///
    /// Memories live on the file workspace like everything else the agent can edit — the
    /// index is what makes them reachable without a tool of their own. `None` means the
    /// board has not written any yet, and the prompt then says how to start.
    fn memory_index(&self) -> Option<String> {
        None
    }
}

/// A skill as the prompt needs it: what it is called, the line that says when to open
/// it, and where its text lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillRef {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// One step of a turn: ask the model, or finish.
pub enum AgentStep {
    /// Carry these to the model, then hand the reply back to [`Agent::apply`].
    Ask { messages: Value, tools: Value },
    /// The turn ended, with the final text or the error that stopped it.
    Done(Result<String, String>),
}

/// The transcript plus the queue. Lives behind the app mutex.
pub struct Agent {
    pub history: Vec<Message>,
    /// The deterministic index of dropped exchanges: one line per message, taken from the
    /// messages themselves. It is **not** a model-written summary — nothing here costs a
    /// model call, and it survives as long as the transcript does.
    ///
    /// Two asymmetries follow the host agent's deterministic projection: a user question
    /// is kept in full (it is the statement of what was wanted), while an answer is
    /// reduced to its first line — and the count of those reductions is stated in the
    /// prompt rather than hidden.
    pub archive: Vec<String>,
    /// How many earlier answers were reduced to a line, so the prompt can say so.
    pub demoted_answers: usize,
    /// Bumped by every mutation of `history`, so the token estimate can be cached.
    history_version: u64,
    /// `(version, message count, tokens)`; `None` until the first estimate.
    token_cache: Option<(u64, usize, u32)>,
    /// How many of the newest messages a restored transcript had to leave out, because
    /// the stored tail is capped at [`MAX_SAVED_BYTES`]. Non-zero only on the boot that
    /// read it back, and stated in the prompt: a model that does not know the beginning
    /// is gone will confidently assume what it said.
    pub saved_dropped: usize,
    /// Depth-1 queue: a trigger while a run is in flight coalesces into this.
    pub pending: Option<Message>,
    pub last_error: String,
    pub runs: u32,
    /// The run journal: the same vocabulary the host agent streams, as a ring.
    pub events: std::collections::VecDeque<Event>,
    event_seq: u64,
    /// Token accounting, per run and cumulative.
    pub usage_run: llm::Usage,
    pub usage_total: llm::Usage,
    /// Per-turn bookkeeping, set by `begin` so `apply` can stop at the limit.
    turn_steps: u8,
    turn_limit: u8,
    turn_tools: Value,
    /// Whether image attachments go out as content parts. Mirrors `llm.vision`: a
    /// text-only endpoint rejects the multimodal shape, and the board cannot tell from a
    /// model name which kind it is talking to.
    pub vision: bool,
    /// The transcript budget, in tokens: `llm.context_tokens`, mirrored here so `trim`
    /// can run without the config in hand. `set_budget` refreshes it on every run, so a
    /// setting changed in the portal takes effect on the next turn.
    context_tokens: u32,
    /// The reply reserve, in tokens: `llm.max_output_tokens`, same reason.
    max_output_tokens: u32,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            history: Vec::new(),
            archive: Vec::new(),
            demoted_answers: 0,
            history_version: 0,
            token_cache: None,
            saved_dropped: 0,
            pending: None,
            last_error: String::new(),
            runs: 0,
            events: std::collections::VecDeque::new(),
            event_seq: 0,
            usage_run: llm::Usage::default(),
            usage_total: llm::Usage::default(),
            turn_steps: 0,
            turn_limit: 0,
            turn_tools: Value::Null,
            // On by default: the model this board ships configured for is multimodal, and
            // an image the user attached going nowhere would be the surprising outcome.
            vision: true,
            context_tokens: LlmConfig::default().context_tokens,
            max_output_tokens: LlmConfig::default().max_output_tokens,
        }
    }
}

/// One journal entry. The kinds are the host agent's event names on purpose
/// (`message_start`, `thinking_start|delta|end`, `text_start|delta|end`, `tool_use`,
/// `tool_progress`, `tool_result`, `usage`, `message_end`, `error`, `run_end`): a
/// client that reads one console can read the other. The board does not stream token
/// by token, so a delta carries the whole block; the names and the order are what
/// stay identical. `retried` is the one addition: it fires at most once per model
/// call, when the request behind the reply took more than one attempt, so a reader
/// learns that the answer was slow to arrive instead of seeing each failed attempt.
#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub kind: &'static str,
    pub at_ms: u64,
    pub text: String,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub is_error: bool,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// `tool_progress` only: which boundary of one tool call this entry marks.
    pub phase: Option<&'static str>,
    /// `tool_progress` on the end boundary only: how long that call took.
    pub elapsed_ms: Option<u64>,
}

impl Event {
    fn simple(seq: u64, kind: &'static str, at_ms: u64) -> Self {
        Self {
            seq,
            kind,
            at_ms,
            text: String::new(),
            tool_call_id: None,
            name: None,
            is_error: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            phase: None,
            elapsed_ms: None,
        }
    }

    pub fn json(&self) -> Value {
        let mut v = serde_json::Map::new();
        v.insert("seq".into(), json!(self.seq));
        v.insert("event".into(), json!(self.kind));
        v.insert("at_ms".into(), json!(self.at_ms));
        if !self.text.is_empty() {
            v.insert("text".into(), json!(llm::truncate_bytes(&self.text, 512)));
        }
        if let Some(id) = &self.tool_call_id {
            v.insert("tool_call_id".into(), json!(id));
        }
        if let Some(name) = &self.name {
            v.insert("name".into(), json!(name));
        }
        if self.is_error {
            v.insert("is_error".into(), json!(true));
        }
        if let Some(phase) = self.phase {
            v.insert("phase".into(), json!(phase));
        }
        if let Some(elapsed_ms) = self.elapsed_ms {
            v.insert("elapsed_ms".into(), json!(elapsed_ms));
        }
        if self.kind == "usage" {
            v.insert("prompt_tokens".into(), json!(self.prompt_tokens));
            v.insert("completion_tokens".into(), json!(self.completion_tokens));
            v.insert("total_tokens".into(), json!(self.prompt_tokens.saturating_add(self.completion_tokens)));
        }
        Value::Object(v)
    }
}

impl Agent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a user turn. Returns `false` (and queues nothing) for blank text.
    ///
    /// When one is already pending, the newest text wins and the source list is
    /// appended, so a 60 s schedule and a portal message cannot start two runs
    /// and cannot silently drop the second trigger's origin.
    pub fn push_user(&mut self, text: &str, source: &str, now_ms: u64) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        match self.pending.as_mut() {
            Some(pending) => {
                pending.text = llm::truncate_bytes(text, MAX_USER_BYTES);
                pending.source = merge_sources(&pending.source, source);
                pending.at_ms = now_ms;
            }
            None => self.pending = Some(Message::user(text, source, now_ms)),
        }
        true
    }

    /// History from index `after` on, plus the count to poll with next.
    pub fn messages_json(&self, after: usize) -> Value {
        let start = after.min(self.history.len());
        let messages: Vec<Value> = self
            .history
            .iter()
            .enumerate()
            .skip(start)
            .map(|(seq, message)| message_json(seq, message))
            .collect();
        json!({ "seq": self.history.len(), "messages": messages })
    }

    /// The whole transcript as the portal renders it.
    pub fn transcript_json(&self) -> Value {
        let messages: Vec<Value> =
            self.history.iter().enumerate().map(|(seq, message)| message_json(seq, message)).collect();
        json!({
            "count": self.history.len(),
            "runs": self.runs,
            "last_error": self.last_error,
            "pending": self.pending.is_some(),
            "pending_source": self.pending.as_ref().map(|p| p.source.clone()).unwrap_or_default(),
            "usage": {
                "last_run": {
                    "prompt_tokens": self.usage_run.prompt_tokens,
                    "completion_tokens": self.usage_run.completion_tokens,
                    "total_tokens": self.usage_run.total(),
                },
                "total": {
                    "prompt_tokens": self.usage_total.prompt_tokens,
                    "completion_tokens": self.usage_total.completion_tokens,
                    "total_tokens": self.usage_total.total(),
                },
            },
            "messages": messages,
        })
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.archive.clear();
        self.pending = None;
        self.last_error.clear();
        self.runs = 0;
        self.saved_dropped = 0;
    }

    /// The newest question in the transcript, which is what a resumed turn asks again.
    pub fn last_user(&self) -> Option<&str> {
        self.history.iter().rev().find(|m| m.role == Role::User).map(|m| m.text.as_str())
    }

    /// Whether the transcript ends on a tool result: a call landed and no assistant
    /// message followed it, which is exactly what an interrupted turn looks like.
    pub fn ends_on_a_tool_result(&self) -> bool {
        matches!(self.history.last().map(|m| m.role), Some(Role::Tool))
    }

    /// One step of a turn, as handed back to whoever drives it.
    ///
    /// `Ask` carries everything a model call needs and nothing that borrows the agent,
    /// which is the whole point: the caller can drop its lock, make the call, and come
    /// back with [`Agent::apply`].
    /// Set the transcript budget from the model configuration.
    ///
    /// Separate from `begin` so existing callers keep working with the defaults, and
    /// called before every run so a change made in the portal applies to the next turn
    /// rather than at the next reboot.
    pub fn set_budget(&mut self, context_tokens: u32, max_output_tokens: u32) {
        self.context_tokens = context_tokens.max(1);
        self.max_output_tokens = max_output_tokens.max(1);
    }

    /// The budget in force: `(context_tokens, max_output_tokens)`.
    pub fn budget(&self) -> (u32, u32) {
        (self.context_tokens, self.max_output_tokens)
    }

    pub fn begin(
        &mut self,
        host: &mut dyn ToolHost,
        now_ms: fn() -> u64,
        max_steps: u8,
    ) -> Option<AgentStep> {
        let user = self.pending.take()?;
        self.runs = self.runs.saturating_add(1);
        self.history.push(user);
        self.history_version += 1;
        self.trim();
        self.usage_run = llm::Usage::default();
        self.turn_steps = 0;
        self.turn_limit = max_steps;
        self.turn_tools = host.tool_specs();
        self.emit("message_start", now_ms(), |_| {});
        Some(self.ask(host))
    }

    /// Consume one model reply: journal it, run the tool calls it asked for, and say
    /// what to do next.
    pub fn apply(
        &mut self,
        host: &mut dyn ToolHost,
        reply: Result<llm::ModelReply, String>,
        now_ms: fn() -> u64,
    ) -> AgentStep {
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                self.last_error = e.clone();
                self.emit("error", now_ms(), |ev| ev.text = e.clone());
                self.emit("run_end", now_ms(), |ev| ev.is_error = true);
                return AgentStep::Done(Err(e));
            }
        };
        // A request that needed more than one attempt says so once, here, and `chat` has
        // already folded the attempts into one line. The failed attempts themselves are
        // not journaled: a console that shows two 503s before the answer reads as a
        // broken run even when the retry is exactly what kept it working.
        if !reply.retry_note.is_empty() {
            let note = reply.retry_note.clone();
            self.emit("retried", now_ms(), |ev| ev.text = note);
        }
        self.turn_steps = self.turn_steps.saturating_add(1);

        // Journal in the host agent's order: usage, reasoning, then the visible text,
        // then one `tool_use` per call it asked for.
        self.usage_run.prompt_tokens = self.usage_run.prompt_tokens.saturating_add(reply.usage.prompt_tokens);
        self.usage_run.completion_tokens =
            self.usage_run.completion_tokens.saturating_add(reply.usage.completion_tokens);
        self.usage_total.prompt_tokens =
            self.usage_total.prompt_tokens.saturating_add(reply.usage.prompt_tokens);
        self.usage_total.completion_tokens =
            self.usage_total.completion_tokens.saturating_add(reply.usage.completion_tokens);
        self.emit("usage", now_ms(), |ev| {
            ev.prompt_tokens = reply.usage.prompt_tokens;
            ev.completion_tokens = reply.usage.completion_tokens;
        });
        if !reply.reasoning.is_empty() {
            let reasoning = reply.reasoning.clone();
            self.emit("thinking_start", now_ms(), |_| {});
            self.emit("thinking_delta", now_ms(), |ev| ev.text = reasoning.clone());
            self.emit("thinking_end", now_ms(), |_| {});
        }
        if !reply.content.is_empty() {
            let text = reply.content.clone();
            self.emit("text_start", now_ms(), |_| {});
            self.emit("text_delta", now_ms(), |ev| ev.text = text.clone());
            self.emit("text_end", now_ms(), |_| {});
        }

        if reply.tool_calls.is_empty() {
            let text = llm::truncate_bytes(&reply.content, MAX_REPLY_BYTES);
            self.history_version += 1;
            self.history.push(Message::assistant_with_reasoning(
                &text,
                &reply.reasoning,
                Vec::new(),
                now_ms(),
            ));
            self.trim();
            self.last_error.clear();
            self.emit("message_end", now_ms(), |_| {});
            self.emit("run_end", now_ms(), |_| {});
            return AgentStep::Done(Ok(text));
        }

        for call in &reply.tool_calls {
            let call_id = call.id.clone();
            let name = call.name.clone();
            self.emit("tool_use", now_ms(), |ev| {
                ev.tool_call_id = Some(call_id.clone());
                ev.name = Some(name.clone());
                ev.text = call.arguments.to_string();
            });
        }
        self.history.push(Message::assistant_with_reasoning(
            &reply.content,
            &reply.reasoning,
            reply.tool_calls.clone(),
            now_ms(),
        ));
        for call in &reply.tool_calls {
            let at = now_ms();
            // The board cannot report progress *within* a tool, so the pair marks the
            // boundaries of one call and the end carries how long it took. A client
            // that ignores it loses nothing; one that pairs it gets a latency.
            self.emit("tool_progress", at, |ev| {
                ev.tool_call_id = Some(call.id.clone());
                ev.name = Some(call.name.clone());
                ev.phase = Some("start");
            });
            let mut is_error = false;
            let result = match llm::arguments_error(call) {
                // The model's argument string was not JSON: report it as a
                // tool error and do not guess at what it meant.
                Some(err) => {
                    is_error = true;
                    json!({ "error": format!("arguments: {err}") }).to_string()
                }
                None if call.name.trim().is_empty() => {
                    is_error = true;
                    json!({ "error": "tool call without a name" }).to_string()
                }
                None => match host.call_tool(&call.name, &call.arguments) {
                    Ok(text) => text,
                    Err(e) => {
                        is_error = true;
                        json!({ "error": e }).to_string()
                    }
                },
            };
            self.emit("tool_result", at, |ev| {
                ev.tool_call_id = Some(call.id.clone());
                ev.name = Some(call.name.clone());
                ev.text = result.clone();
                ev.is_error = is_error;
            });
            let ended = now_ms();
            self.emit("tool_progress", ended, |ev| {
                ev.tool_call_id = Some(call.id.clone());
                ev.name = Some(call.name.clone());
                ev.phase = Some("end");
                ev.elapsed_ms = Some(ended.saturating_sub(at));
            });
            // `Message::tool_result` owns the 2 KB cut; the marker tells the model
            // the result it sees is incomplete.
            self.history.push(Message::tool_result(&call.id, &result, is_error, at));
            self.history_version += 1;
        }
        self.trim();

        // 0 means no cap: the loop ends on a final answer, a transport error, or when the
        // history budget forces a trim. A fixed cap only truncates work that would have
        // finished, so it is something to switch on rather than to live with.
        if self.turn_limit > 0 && self.turn_steps >= self.turn_limit {
            let err = format!("agent: step limit {} reached before a final answer", self.turn_limit);
            self.last_error = err.clone();
            self.emit("error", now_ms(), |ev| ev.text = err.clone());
            self.emit("run_end", now_ms(), |ev| ev.is_error = true);
            return AgentStep::Done(Err(err));
        }
        // Rebuild the tool list for the next round, instead of reusing the one captured
        // at the start of the turn. The board's premise is that a model can apply a
        // manifest and then call the tool it just declared; a list frozen for the whole
        // turn makes that impossible until the next user message, and it also keeps
        // showing a tool that a manifest change removed.
        self.turn_tools = host.tool_specs();
        self.ask(host)
    }

    /// The request for the next model call, from the current transcript.
    fn ask(&self, host: &dyn ToolHost) -> AgentStep {
        AgentStep::Ask { messages: self.openai_messages(host), tools: self.turn_tools.clone() }
    }

    /// Drain `pending`, run the loop, append messages. Returns the final text.
    ///
    /// This holds the host for the whole turn, which is what a test or the simulator
    /// wants. A platform that has other things to answer (the firmware's HTTP server)
    /// uses [`Agent::begin`]/[`Agent::apply`] through `App::drive_agent` instead, so the
    /// model call happens with nothing locked.
    pub fn run_pending(
        &mut self,
        host: &mut dyn ToolHost,
        transport: &mut dyn ChatTransport,
        cfg: &LlmConfig,
        now_ms: fn() -> u64,
    ) -> Result<String, String> {
        self.run_pending_with(
            host,
            transport,
            cfg,
            now_ms,
            &llm::RetryPolicy::default(),
            &llm::WallClock::new(),
        )
    }

    /// [`run_pending`](Self::run_pending) with the retry policy and the clock spelled out.
    ///
    /// The seam is public for the same reason [`llm::chat_with`] is: the backoff and the
    /// deadline are numbers a test has to observe without sleeping, and a platform whose
    /// socket timeout differs from the default deserves to say what it wants.
    pub fn run_pending_with(
        &mut self,
        host: &mut dyn ToolHost,
        transport: &mut dyn ChatTransport,
        cfg: &LlmConfig,
        now_ms: fn() -> u64,
        policy: &llm::RetryPolicy,
        timer: &dyn llm::RetryTimer,
    ) -> Result<String, String> {
        let mut step = match self.begin(host, now_ms, cfg.max_steps) {
            Some(step) => step,
            None => return Ok(String::new()),
        };
        loop {
            match step {
                AgentStep::Ask { messages, tools } => {
                    let reply = llm::chat_with(transport, cfg, &messages, &tools, policy, timer);
                    step = self.apply(host, reply, now_ms);
                }
                AgentStep::Done(result) => return result,
            }
        }
    }

    /// Append one journal entry, keeping the ring bounded.
    fn emit(&mut self, kind: &'static str, at_ms: u64, edit: impl FnOnce(&mut Event)) {
        self.event_seq = self.event_seq.saturating_add(1);
        let seq = self.event_seq;
        let mut event = Event::simple(seq, kind, at_ms);
        edit(&mut event);
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// The journal, optionally only what came after `seq`. Shape matches the host
    /// agent's stream so one client can read both.
    pub fn events_json(&self, after: u64) -> Value {
        let events: Vec<Value> = self.events.iter().filter(|e| e.seq > after).map(|e| e.json()).collect();
        json!({ "seq": self.event_seq, "kind": "agent_events", "events": events })
    }

    /// Drop the oldest messages until the ring is within both bounds, keeping
    /// the history a valid OpenAI transcript (a tool result never leads it).
    ///
    /// The dropped part is not lost silently: :func:`compress` turns it into a
    /// deterministic index first, so the model still knows what it already tried.
    fn trim(&mut self) {
        if self.needs_compaction() {
            let keep = self.turns_within(keep_recent_tokens(self.context_tokens, self.max_output_tokens));
            self.compress(keep);
        }
        while self.history.len() > MAX_HISTORY_MESSAGES {
            self.history.remove(0);
            self.history_version += 1;
        }
        while self.history_bytes() > MAX_HISTORY_BYTES && self.history.len() > 1 {
            let dropped = self.history.remove(0);
            self.history_version += 1;
            self.archive_message(&dropped);
        }
        while matches!(self.history.first().map(|m| m.role), Some(Role::Tool)) {
            let dropped = self.history.remove(0);
            self.history_version += 1;
            self.archive_message(&dropped);
        }
        while self.archive.len() > ARCHIVE_MAX_LINES {
            self.archive.remove(0);
        }
    }

    /// Keep the last `keep_turns` question-and-answer groups and move everything older
    /// into the archive as one line per message.
    ///
    /// This is **compression without a summary**: the text is derived from the transcript,
    /// so it costs no model call, cannot invent a fact, and is reproducible. The index is
    /// what a model needs to avoid repeating itself — which tool ran with which arguments
    /// and what came back — not prose about how the conversation felt.
    pub fn compress(&mut self, keep_turns: usize) {
        let cut = turn_start(&self.history, keep_turns.min(self.history.len()));
        if cut == 0 {
            return;
        }
        let dropped: Vec<Message> = self.history.drain(..cut).collect();
        if !dropped.is_empty() {
            self.history_version += 1;
        }
        for message in &dropped {
            self.archive_message(message);
        }
    }

    /// One line for the archive, bounded so the index itself cannot grow without limit.
    fn archive_message(&mut self, message: &Message) {
        let line = match message.role {
            // A question is the one thing compression must not paraphrase: it is the
            // statement of what was wanted, and a clipped version of it is what makes a
            // model re-ask something it was already told. The host agent protects user
            // text the same way (its `protected_text` never demotes a user block), so the
            // whole line is kept here, bounded only by what one message may carry.
            Role::User => format!("user: {}", head(&message.text, MAX_USER_BYTES)),
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    // Answers are demoted, and the demotion is counted rather than silent:
                    // `retention_note` states how many were reduced to a line.
                    self.demoted_answers += 1;
                    format!("answer: {}", head(&message.text, 90))
                } else {
                    let calls: Vec<String> = message
                        .tool_calls
                        .iter()
                        .map(|c| format!("{}({})", c.name, head(&llm::arguments_request_json(c), 48)))
                        .collect();
                    format!("called: {}", head(&calls.join(", "), 100))
                }
            }
            Role::Tool => {
                // The result line joins the call above it by time: printing the id would
                // spend the budget on a string the model cannot use.
                let _ = message.tool_call_id.as_deref();
                format!("result: {}", head(&message.text, 90))
            }
            Role::System => format!("note: {}", head(&message.text, 90)),
        };
        if line.trim().len() > 6 {
            self.archive.push(line);
        }
    }

    /// Forget the past: keep the last `keep_turns` exchanges, drop everything else, and
    /// drop the archive with it — an index of what is no longer there is worse than none.
    ///
    /// `keep = 0` clears the conversation completely, which is what a client wants before
    /// switching task; `keep = 2` is what a long session wants when it starts repeating
    /// itself. Returns `(removed, kept)` message counts.
    pub fn forget(&mut self, keep_turns: usize) -> (usize, usize) {
        let cut = turn_start(&self.history, keep_turns.min(self.history.len()));
        let removed = self.history.drain(..cut).count();
        if removed > 0 {
            self.history_version += 1;
        }
        self.archive.clear();
        self.demoted_answers = 0;
        self.saved_dropped = 0;
        self.pending = None;
        self.last_error.clear();
        (removed, self.history.len())
    }

    /// Whether the transcript has outgrown its budget and compaction should run.
    ///
    /// Public because it is the whole answer to "when does this fire": the three bounds
    /// are the token budget from the configured window, the hard byte ceiling that keeps
    /// the ring buffer inside the board's RAM, and the message count.
    pub fn needs_compaction(&mut self) -> bool {
        self.history_tokens() > trigger_tokens(self.context_tokens)
            || self.history_bytes() > MAX_HISTORY_BYTES
            || self.history.len() > MAX_HISTORY_MESSAGES
    }

    /// Estimated tokens in the transcript, by the same classifier the host uses.
    ///
    /// Tool arguments count too: a `write` of two kilobytes is the largest thing a single
    /// message can carry, and leaving it out would let a run of them look free.
    ///
    /// Memoised on `history_version`: a 512 KB transcript is half a million character
    /// classifications, and `needs_compaction` used to pay for that on every step.
    pub fn history_tokens(&mut self) -> u32 {
        if let Some((version, len, tokens)) = self.token_cache {
            if version == self.history_version && len == self.history.len() {
                return tokens;
            }
        }
        let tokens = self.history_tokens_uncached();
        self.token_cache = Some((self.history_version, self.history.len(), tokens));
        tokens
    }

    /// The same number, always recomputed. Tests use it to prove the cache is not lying.
    pub fn history_tokens_uncached(&self) -> u32 {
        self.history
            .iter()
            .map(|m| {
                let calls: u32 = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        estimate_tokens(&c.name)
                            .saturating_add(estimate_tokens(&llm::arguments_request_json(c).to_string()))
                    })
                    .sum();
                estimate_tokens(&m.text).saturating_add(calls)
            })
            .fold(0u32, |a, b| a.saturating_add(b))
    }

    /// How many trailing exchanges fit in `budget` tokens; at least one, so a single
    /// oversized exchange still survives compression rather than emptying the thread.
    fn turns_within(&self, budget: u32) -> usize {
        let mut kept = 0usize;
        for turn in 1..=self.history.len().max(1) {
            let start = turn_start(&self.history, turn);
            let cost: u32 = self.history[start..]
                .iter()
                .map(|m| estimate_tokens(&m.text))
                .fold(0u32, |a, b| a.saturating_add(b));
            if cost > budget && kept > 0 {
                break;
            }
            kept = turn;
            if cost >= budget {
                break;
            }
        }
        kept.max(1)
    }

    fn history_bytes(&self) -> usize {
        self.history
            .iter()
            .map(|m| {
                m.text.len()
                    + m.source.len()
                    + m.tool_call_id.as_ref().map(|id| id.len()).unwrap_or(0)
                    + m.tool_calls.iter().map(|c| c.id.len() + c.name.len() + 48).sum::<usize>()
            })
            .sum()
    }

    /// The request payload: the generated system prompt, the host's own context, and
    /// then the history.
    fn openai_messages(&self, host: &dyn ToolHost) -> Value {
        let mut out = Vec::with_capacity(self.history.len() + 1);
        out.push(json!({ "role": Role::System.as_str(), "content": self.system_message(host) }));
        for message in &self.history {
            out.push(openai_message(message, self.vision));
        }
        Value::Array(out)
    }

    /// The static prompt plus whatever the host contributes: project instructions and
    /// the skill index.
    ///
    /// It is kept per run rather than in NVS because it must follow the files: editing
    /// `/AGENTS.md` or adding a skill takes effect on the next turn, with no restart and
    /// no transcript rewrite.
    /// The prompt the model receives, for `/api/agent/prompt`: the same text the next
    /// request will carry, memory index and compressed history included.
    pub fn prompt_text(&self, host: &dyn ToolHost) -> String {
        self.system_message(host)
    }

    fn system_message(&self, host: &dyn ToolHost) -> String {
        let mut text = String::from(system_prompt());
        // The owner's standing orders come before the machine-generated parts: they are
        // the reason this board exists, and a reader (or a model) should meet them first.
        if let Some(custom) = host.custom_prompt() {
            let custom = custom.trim();
            if !custom.is_empty() {
                text.push_str("\n\n## Standing orders for this board\n");
                text.push_str(custom);
            }
        }
        if let Some(instructions) = host.project_instructions() {
            text.push_str("\n\n## Project instructions (/AGENTS.md)\nThese are the rules for this installation; follow them.\n");
            text.push_str(&instructions);
        }
        let skills = host.skills();
        if !skills.is_empty() {
            text.push_str(
                "\n\n## Skills\nEach line is `name: description` followed by the file to read. \
                 When one matches the task, open that file with `read` before following it; a \
                 directory may hold other files next to SKILL.md (diagrams, tables, a pinout) and \
                 those are readable the same way. Do not reconstruct a procedure from its \
                 description.\n",
            );
            for skill in &skills {
                text.push_str(&format!("- {}: {} ({})\n", skill.name, skill.description, skill.path));
            }
        }
        // What earlier sessions wrote down. The index is all that goes in the prompt; a
        // memory is opened with `read` like any other file, so nothing is loaded twice
        // and nothing is spent on a topic this turn does not need.
        text.push_str(
            "\n\n## Memory (/MEMORY.md)\n\
             This is what you wrote down in earlier sessions, as an index. Open a topic with \
             `read` before relying on it; a fresh board has none.\n\
             To remember something: write the fact to `/memory/<topic>.md` (2 KB is the file \
             limit) and add **one line** to `/MEMORY.md` — `- [topic](path) — hook`. The line is a \
             pointer, never the memory itself, and the index is kept under 24 lines; replace an \
             outdated entry instead of appending.\n",
        );
        match host.memory_index() {
            Some(index) => text.push_str(&index),
            None => text.push_str("(nothing yet — write the first memory when something is worth keeping)\n"),
        }
        // What compression kept of the thread: a deterministic index, not a summary.
        if !self.archive.is_empty() {
            text.push_str(
                "\n## Earlier in this conversation (compressed)\n\
                 These exchanges were dropped from the transcript to stay inside the context \
                 budget. The lines are taken from the messages themselves — nothing was \
                 summarised — so treat them as an index: a repeat of a tool call listed here \
                 is wasted work.\n",
            );
            for line in &self.archive {
                text.push_str(&format!("- {line}\n"));
            }
            if self.demoted_answers > 0 {
                text.push_str(&format!(
                    "- ({count} earlier answer(s) are reduced to their first line; the questions \
                     above are kept in full)\n",
                    count = self.demoted_answers
                ));
            }
        }
        // What a reboot did not keep. The model has to know: a transcript that silently
        // starts in the middle reads as "we were always here", and the first thing a model
        // does with a gap is fill it in from memory.
        if self.saved_dropped > 0 {
            text.push_str(&format!(
                "\n## Recovered after a reboot\nThe board lost power and only the last {kept} \
                 message(s) of this conversation were stored; {dropped} older one(s) were not \
                 kept. Do not assume what they said — re-read a file or re-run a tool when the \
                 answer matters, and say so if the gap blocks the task.\n",
                kept = self.history.len(),
                dropped = self.saved_dropped
            ));
        }
        text
    }
}

/// The system prompt is built per run and never stored in the history, so it can
/// change with the firmware without rewriting NVS or the transcript.
pub fn system_prompt() -> &'static str {
    "You are the agent running on an esp32-agent board over a small LAN.\n\
     1. Answer the user's question in their language, briefly: replies are capped at 8 KB and shown on a small portal page.\n\
     2. When a fact about the board is needed, call a tool instead of guessing; one call at a time, then use its result.\n\
     3. Prefer the `board` tool's status command, then the narrower tool. Never repeat a call whose result you already have.\n\
     4. If a tool fails, say what failed and what you tried; do not invent readings.\n\
     5. You cannot browse the internet except through `shell`, with a command line like `curl <url>`; you cannot reboot the board."
}

/// Where the last `keep` question-and-answer exchanges begin.
///
/// A turn starts at a user message; everything from there on is kept. With `keep` larger
/// than the transcript, nothing is cut.
fn turn_start(history: &[Message], keep: usize) -> usize {
    if keep == 0 {
        return history.len();
    }
    let mut seen = 0;
    for i in (0..history.len()).rev() {
        if history[i].role == Role::User {
            seen += 1;
            if seen == keep {
                return i;
            }
        }
    }
    0
}

/// The first `max` characters, newlines flattened: an archive line must stay one line.
fn head(text: &str, max: usize) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let trimmed = flat.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}…")
}

fn openai_message(message: &Message, vision: bool) -> Value {
    match message.role {
        Role::Assistant if !message.tool_calls.is_empty() => {
            let calls: Vec<Value> = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": llm::arguments_request_json(call),
                        },
                    })
                })
                .collect();
            // `content` is null for a pure tool-call turn, which is what every
            // OpenAI-compatible server expects to see.
            let content = if message.text.is_empty() { Value::Null } else { json!(message.text) };
            let mut out =
                json!({ "role": Role::Assistant.as_str(), "content": content, "tool_calls": calls });
            // A reasoning model refuses its own turn back without the reasoning that
            // produced the calls, so it goes out again as the provider field.
            if !message.reasoning.is_empty() {
                out["reasoning_content"] = json!(message.reasoning);
            }
            out
        }
        Role::Assistant if !message.reasoning.is_empty() => {
            json!({
                "role": Role::Assistant.as_str(),
                "content": message.text,
                "reasoning_content": message.reasoning,
            })
        }
        Role::User if vision && !message.attachments.is_empty() => {
            // OpenAI's multimodal shape: one text part, then one image part per file. A
            // text-only endpoint refuses this outright, which is why `llm.vision` gates it.
            let mut parts: Vec<Value> = Vec::with_capacity(message.attachments.len() + 1);
            if !message.text.is_empty() {
                parts.push(json!({ "type": "text", "text": message.text }));
            }
            for a in &message.attachments {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{};base64,{}", a.mime, a.data_b64) },
                }));
            }
            json!({ "role": Role::User.as_str(), "content": parts })
        }
        Role::Tool => json!({
            "role": Role::Tool.as_str(),
            "tool_call_id": message.tool_call_id.as_deref().unwrap_or_default(),
            "content": message.text,
        }),
        role => json!({ "role": role.as_str(), "content": message.text }),
    }
}

/// What the portal and `GET /api/agent/history` see for one message.
fn message_json(seq: usize, message: &Message) -> Value {
    let mut out = json!({
        "seq": seq,
        "role": message.role.as_str(),
        "text": message.text,
        "at_ms": message.at_ms,
    });
    if !message.source.is_empty() {
        out["source"] = json!(message.source);
    }
    // A separate block, like the host agent's `Reasoning`: the portal renders it
    // dimmed and folded, and a client that ignores it still sees the answer.
    if !message.reasoning.is_empty() {
        out["reasoning"] = json!(message.reasoning);
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> =
            message.tool_calls.iter().map(|call| json!({ "id": call.id, "name": call.name })).collect();
        out["tool_calls"] = Value::Array(calls);
    }
    if let Some(id) = &message.tool_call_id {
        out["tool_call_id"] = json!(id);
    }
    if message.is_error {
        out["is_error"] = json!(true);
    }
    out
}

// ---------------------------------------------------------------- stored transcript

/// The transcript in the form it is stored between boots: JSON Lines, a header line
/// followed by one message per line.
///
/// Why this shape rather than one JSON array: a blob torn by a power cut loses its last
/// lines, and a line-oriented format plus a checksum in the header makes that detectable
/// instead of half-adopted. The per-message caps ([`MAX_SAVED_TEXT_BYTES`],
/// [`MAX_SAVED_REASONING_BYTES`], [`MAX_SAVED_ARG_BYTES`]) are what keep the whole thing
/// inside [`MAX_SAVED_BYTES`] instead of storing a 2 KB tool result fifty times over.
///
/// Attachments are not stored: a base64 screenshot is larger than the entire budget, and
/// the image had already been seen by the model when the turn ran.
pub fn transcript_jsonl(history: &[Message], at_ms: u64) -> String {
    let lines: Vec<String> = history.iter().map(stored_message_line).collect();
    // Keep the newest messages, dropping the oldest until the tail fits. The header is
    // priced in up front, so assembling it cannot push the blob over the budget.
    let mut start = 0usize;
    while start + 1 < lines.len() {
        let body: usize = lines[start..].iter().map(|line| line.len() + 1).sum();
        if body + SAVED_HEADER_RESERVE <= MAX_SAVED_BYTES {
            break;
        }
        start += 1;
    }
    // A tail that begins with a tool result carries a `tool_call_id` whose call was cut
    // with the message before it, and every provider rejects that. Those go too, and they
    // are counted as dropped: they are gone from what a boot restores.
    while start + 1 < lines.len() && history[start].role == Role::Tool {
        start += 1;
    }
    let body = if start < lines.len() { lines[start..].join("\n") + "\n" } else { String::new() };
    let header = stored_header(at_ms, start, checksum(body.as_bytes()));
    format!("{header}\n{body}")
}

/// Read back what [`transcript_jsonl`] wrote: the messages and how many older ones the
/// tail had to drop.
///
/// `None` is the answer for anything that does not check out: no header, an unknown
/// version, a checksum that does not match the body (a write torn by a power cut looks
/// exactly like this), a line that is not a message, or a tool result whose call is not
/// in the tail. The caller starts a new conversation instead, which is worse than a
/// complete history and far better than a transcript a provider rejects on every turn.
pub fn transcript_from_jsonl(text: &str) -> Option<(Vec<Message>, usize)> {
    let (header, body) = text.split_once('\n')?;
    let header: Value = serde_json::from_str(header).ok()?;
    if header.get("v").and_then(|v| v.as_u64())? != TRANSCRIPT_VERSION as u64 {
        return None;
    }
    let dropped = header.get("dropped").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let expected = u64::from_str_radix(header.get("hash").and_then(|v| v.as_str())?, 16).ok()?;
    if checksum(body.as_bytes()) != expected {
        return None;
    }
    let mut messages = Vec::new();
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        messages.push(stored_message(line)?);
    }
    // Structure: every result must answer a call that is still in the tail, or the next
    // request is one no endpoint will accept. Calls left open at the end are fine - that
    // is the interrupted turn this whole module exists to resume.
    let mut open: Vec<String> = Vec::new();
    for message in &messages {
        match message.role {
            Role::Assistant => open.extend(message.tool_calls.iter().map(|call| call.id.clone())),
            Role::Tool => {
                let id = message.tool_call_id.clone().unwrap_or_default();
                let at = open.iter().position(|seen| *seen == id)?;
                open.remove(at);
            }
            _ => {}
        }
    }
    Some((messages, dropped))
}

fn stored_header(at_ms: u64, dropped: usize, checksum: u64) -> String {
    json!({
        "v": TRANSCRIPT_VERSION,
        "at_ms": at_ms,
        "dropped": dropped,
        "hash": format!("{checksum:016x}"),
    })
    .to_string()
}

/// One stored message. Absent fields are absent rather than empty: the blob is read under
/// a 16 KB ceiling and an `"is_error":false` per line is bytes that buy nothing.
fn stored_message_line(message: &Message) -> String {
    let mut out = serde_json::Map::new();
    out.insert("role".into(), json!(message.role.as_str()));
    if !message.text.is_empty() {
        out.insert("text".into(), json!(llm::truncate_bytes(&message.text, MAX_SAVED_TEXT_BYTES)));
    }
    if !message.reasoning.is_empty() {
        out.insert(
            "reasoning".into(),
            json!(llm::truncate_bytes(&message.reasoning, MAX_SAVED_REASONING_BYTES)),
        );
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                let arguments = llm::arguments_request_json(call);
                json!({
                    "id": call.id,
                    "name": call.name,
                    "arguments": llm::truncate_bytes(&arguments, MAX_SAVED_ARG_BYTES),
                })
            })
            .collect();
        out.insert("tool_calls".into(), Value::Array(calls));
    }
    if let Some(id) = &message.tool_call_id {
        out.insert("tool_call_id".into(), json!(id));
    }
    if message.is_error {
        out.insert("is_error".into(), json!(true));
    }
    if !message.source.is_empty() {
        out.insert("source".into(), json!(message.source));
    }
    out.insert("at_ms".into(), json!(message.at_ms));
    Value::Object(out).to_string()
}

fn stored_message(line: &str) -> Option<Message> {
    let value: Value = serde_json::from_str(line).ok()?;
    let role = Role::parse(value.get("role")?.as_str()?);
    let text = value.get("text").and_then(|v| v.as_str()).unwrap_or_default();
    let reasoning = value.get("reasoning").and_then(|v| v.as_str()).unwrap_or_default();
    let mut tool_calls = Vec::new();
    for call in value.get("tool_calls").and_then(|v| v.as_array()).into_iter().flatten() {
        let id = call.get("id").and_then(|v| v.as_str())?.to_string();
        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        // A cut argument string is not JSON any more; the transcript must still parse, so
        // the call is re-sent with no arguments rather than dropped: its result names the
        // id, and a call without arguments is a request the endpoint still accepts.
        let arguments = call
            .get("arguments")
            .and_then(|v| v.as_str())
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or_else(|| json!({}));
        tool_calls.push(ToolCall { id, name, arguments });
    }
    Some(Message {
        role,
        // The stored copy is already cut; constructing through the role helpers would cut
        // it again against the larger in-RAM caps, so the fields are filled directly.
        text: text.to_string(),
        reasoning: reasoning.to_string(),
        tool_calls,
        tool_call_id: value.get("tool_call_id").and_then(|v| v.as_str()).map(|v| v.to_string()),
        is_error: value.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
        source: value.get("source").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        at_ms: value.get("at_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        attachments: Vec::new(),
    })
}

/// The transcript's checksum: FNV-1a over the body, one cheap pass, no dependency, and
/// enough to tell a torn or half-overwritten blob from an intact one. It is an integrity
/// check for a power cut, not a signature. The function itself is
/// [`crate::hash::fnv1a`], shared with the chunked storage in [`crate::store`], so the
/// constant is written once.
fn checksum(bytes: &[u8]) -> u64 {
    crate::hash::fnv1a(bytes)
}

fn merge_sources(a: &str, b: &str) -> String {
    let mut out = String::new();
    for source in [a, b] {
        for part in source.split(',') {
            let part = part.trim();
            if part.is_empty() || out.split(',').any(|seen| seen == part) {
                continue;
            }
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(part);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn an_image_goes_out_as_a_content_part() {
        let message = Message::user("what is this?", "http", 1).with_attachments(vec![Attachment {
            name: "shot.jpg".into(),
            mime: "image/jpeg".into(),
            data_b64: "aGVsbG8=".into(),
        }]);
        let wire = openai_message(&message, true);
        let parts = wire["content"].as_array().expect("content parts");
        assert_eq!(parts.len(), 2, "{wire}");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,aGVsbG8=");

        // A text-only model gets the text and nothing it cannot parse.
        let plain = openai_message(&message, false);
        assert!(plain["content"].is_string(), "{plain}");
        assert_eq!(plain["content"], "what is this?");
    }

    /// Canned transport: replies in order, requests recorded.
    struct StubTransport {
        replies: VecDeque<String>,
        bodies: Vec<String>,
    }

    impl StubTransport {
        fn new(replies: Vec<String>) -> Self {
            Self { replies: replies.into(), bodies: Vec::new() }
        }
    }

    impl ChatTransport for StubTransport {
        fn post_json(&mut self, _url: &str, _headers: &[(&str, &str)], body: &str) -> Result<String, String> {
            self.bodies.push(body.to_string());
            self.replies.pop_front().ok_or_else(|| "stub: no reply left".to_string())
        }
    }

    /// Records every call, answers with one fixed result.
    struct StubHost {
        calls: Vec<(String, Value)>,
        result: String,
    }

    impl StubHost {
        fn returning(result: &str) -> Self {
            Self { calls: Vec::new(), result: result.to_string() }
        }
    }

    impl ToolHost for StubHost {
        fn tool_specs(&self) -> Value {
            json!([{ "type": "function", "function": { "name": "board_status" } }])
        }

        fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String, String> {
            self.calls.push((name.to_string(), arguments.clone()));
            Ok(self.result.clone())
        }
    }

    fn tool_call_reply(id: &str, name: &str, arguments: &str) -> String {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                }
            }]
        })
        .to_string()
    }

    /// A reply carrying the host agent's two extra pieces: a reasoning block and token
    /// usage, both of which the wire puts beside the text.
    fn reasoned_reply(reasoning: &str, text: &str, prompt: u32, completion: u32) -> String {
        json!({
            "usage": { "prompt_tokens": prompt, "completion_tokens": completion },
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": text,
                    "reasoning_content": reasoning,
                }
            }]
        })
        .to_string()
    }

    /// The interesting case for a reasoner: it thinks, then asks for a tool. The next
    /// request has to carry that reasoning back or the provider rejects the turn.
    fn reasoned_tool_call_reply(id: &str, name: &str, arguments: &str, reasoning: &str) -> String {
        json!({
            "usage": { "prompt_tokens": 12, "completion_tokens": 5 },
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": reasoning,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                }
            }]
        })
        .to_string()
    }

    fn answer_reply(text: &str) -> String {
        json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }

    fn cfg() -> LlmConfig {
        LlmConfig {
            base_url: "http://127.0.0.1:8899/v1/chat/completions".to_string(),
            model: "stub".to_string(),
            max_steps: 4,
            ..LlmConfig::default()
        }
    }

    fn clock() -> u64 {
        1_000
    }

    thread_local! {
        /// A clock that advances by 1 ms per read, so a duration is observable in a
        /// test without sleeping. One per test thread, which is how cargo runs them.
        static TICKING_CLOCK: std::cell::Cell<u64> = const { std::cell::Cell::new(1_000) };
    }

    fn ticking_clock() -> u64 {
        TICKING_CLOCK.with(|cell| {
            let next = cell.get().saturating_add(1);
            cell.set(next);
            next
        })
    }

    /// The shape the host agent streams, in the same order: reasoning, text, tool use,
    /// tool result, usage. The board cannot stream token by token, so a delta carries
    /// the whole block - the vocabulary and the ordering are what stay identical.
    #[test]
    fn the_run_journal_uses_the_host_agents_event_names_in_order() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning(r#"{"ok":true}"#);
        let mut transport = StubTransport::new(vec![
            reasoned_tool_call_reply("c1", "board_status", "{}", "look at the board first"),
            reasoned_reply("the tool answered, now summarise", "the board is fine", 30, 7),
        ]);
        agent.push_user("is the board ok?", "http", 40);
        agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("answer");

        let kinds: Vec<&str> = agent.events.iter().map(|e| e.kind).collect();
        let first = |k: &str| {
            kinds.iter().position(|x| *x == k).unwrap_or_else(|| panic!("{k} missing in {kinds:?}"))
        };
        assert!(first("message_start") < first("thinking_start"), "{kinds:?}");
        assert!(first("thinking_end") < first("tool_use"), "reasoning precedes the call it explains");
        assert!(first("tool_use") < first("tool_result"), "{kinds:?}");
        assert!(first("tool_result") < first("text_start"), "the answer comes after the tool ran");
        assert!(first("text_end") < first("message_end"), "{kinds:?}");
        assert!(first("message_end") < first("run_end"), "{kinds:?}");
        assert!(kinds.iter().filter(|k| **k == "usage").count() >= 2, "one per model call: {kinds:?}");

        // Reasoning is its own block on the assistant turn that produced the call...
        let tool_turn = agent.history.iter().find(|m| !m.tool_calls.is_empty()).expect("the tool-call turn");
        assert_eq!(tool_turn.reasoning, "look at the board first");
        // ...and it goes back out on the next request as the provider field.
        let body = transport.bodies.last().expect("a request body");
        assert!(body.contains("reasoning_content"), "{body}");

        let last = agent.history.last().expect("assistant message");
        assert_eq!(last.text, "the board is fine");
        assert_eq!(last.reasoning, "the tool answered, now summarise");
        // Both model calls are accounted: 12+30 prompt, 5+7 completion.
        assert_eq!(agent.usage_run.prompt_tokens, 42);
        assert_eq!(agent.usage_run.completion_tokens, 12);
        assert_eq!(agent.usage_total.total(), 54);
    }

    /// `tool_progress` brackets exactly one tool call, in the order the frozen contract
    /// in `docs/agent-stream.md` asks for, with the elapsed time on the closing half.
    #[test]
    fn a_tool_call_is_bracketed_by_tool_progress_events() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning(r#"{"ok":true}"#);
        let mut transport = StubTransport::new(vec![
            tool_call_reply("c1", "board_status", "{}"),
            answer_reply("the board answered"),
        ]);
        agent.push_user("is the board ok?", "http", 1);
        agent.run_pending(&mut host, &mut transport, &cfg(), ticking_clock).expect("answer");

        let kinds: Vec<&str> = agent.events.iter().map(|e| e.kind).collect();
        let at = kinds
            .iter()
            .position(|k| *k == "tool_use")
            .unwrap_or_else(|| panic!("tool_use missing in {kinds:?}"));
        assert_eq!(
            &kinds[at..at + 4],
            &["tool_use", "tool_progress", "tool_result", "tool_progress"],
            "one tool call journals exactly this window: {kinds:?}"
        );

        let start = &agent.events[at + 1];
        let result = &agent.events[at + 2];
        let end = &agent.events[at + 3];
        assert!(start.seq < result.seq && result.seq < end.seq, "the journal is ordered by seq");
        assert_eq!(start.phase, Some("start"));
        assert_eq!(start.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(start.name.as_deref(), Some("board_status"));
        assert_eq!(start.elapsed_ms, None, "a start has no duration yet");
        assert_eq!(end.phase, Some("end"));
        assert_eq!(end.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(end.name.as_deref(), Some("board_status"));
        let elapsed = end.elapsed_ms.expect("the end half carries elapsed_ms");
        assert!(elapsed > 0, "the clock advanced during the call, so elapsed_ms cannot be 0");

        // A start and its end stamp the same call, and the pair carries no text: the
        // client reads the result from `tool_result`.
        assert!(start.text.is_empty() && end.text.is_empty());
        let start_json = start.json();
        assert_eq!(start_json["event"], "tool_progress");
        assert_eq!(start_json["seq"], json!(start.seq));
        assert_eq!(start_json["at_ms"], json!(start.at_ms));
        assert_eq!(start_json["tool_call_id"], "c1");
        assert_eq!(start_json["name"], "board_status");
        assert_eq!(start_json["phase"], "start");
        assert!(start_json.get("elapsed_ms").is_none(), "no duration before the call ran");
        let end_json = end.json();
        assert_eq!(end_json["phase"], "end");
        assert_eq!(end_json["elapsed_ms"], json!(elapsed));

        // `events_json(after)` is the frame source for the stream: both halves are in
        // the tail a polling client asks for.
        let tail_json = agent.events_json(start.seq);
        let tail: Vec<&str> = tail_json["events"]
            .as_array()
            .expect("events array")
            .iter()
            .map(|e| e["event"].as_str().unwrap_or_default())
            .collect();
        assert!(tail.contains(&"tool_progress"), "{tail:?}");
        assert_eq!(tail.iter().filter(|e| **e == "tool_progress").count(), 1, "{tail:?}");
    }

    /// A call that fails before it reaches the host (bad arguments, no name) is still
    /// bracketed, so a client pairing the two halves never sees an unmatched start.
    #[test]
    fn a_tool_call_that_fails_before_the_host_is_still_bracketed() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("should not run");
        let mut transport = StubTransport::new(vec![
            tool_call_reply("c1", "fs_write", "{\"path\": \"/a.txt\""),
            answer_reply("I could not write the file"),
        ]);
        agent.push_user("write a file", "http", 1);
        agent
            .run_pending(&mut host, &mut transport, &cfg(), ticking_clock)
            .expect("the loop still ends with an answer");
        assert!(host.calls.is_empty(), "the tool must not be called");

        let kinds: Vec<&str> = agent.events.iter().map(|e| e.kind).collect();
        let at = kinds.iter().position(|k| *k == "tool_use").expect("tool_use");
        assert_eq!(
            &kinds[at..at + 4],
            &["tool_use", "tool_progress", "tool_result", "tool_progress"],
            "{kinds:?}"
        );
        assert!(agent.events[at + 2].is_error, "the result is flagged as an error");
        assert!(agent.events[at + 3].elapsed_ms.is_some(), "the pair is closed anyway");
    }

    #[test]
    fn a_tool_call_runs_through_the_host_and_its_result_reaches_the_next_request() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning(r#"{"boot":true,"marker":"SENSOR-OK"}"#);
        let mut transport = StubTransport::new(vec![
            tool_call_reply("call_1", "board_status", "{\"detail\":true}"),
            answer_reply("the board is up"),
        ]);

        assert!(agent.push_user("is the board up?", "http", 10));
        let text = agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("a final answer");

        assert_eq!(text, "the board is up");
        // The tool really ran, with the decoded arguments.
        assert_eq!(host.calls.len(), 1);
        assert_eq!(host.calls[0].0, "board_status");
        assert_eq!(host.calls[0].1, json!({ "detail": true }));

        // Two model calls: the request carried the call, the result, and a
        // generated system prompt that is not part of the history.
        assert_eq!(transport.bodies.len(), 2);
        let first: Value = serde_json::from_str(&transport.bodies[0]).expect("json");
        assert_eq!(first["messages"][0]["role"], "system");
        assert!(first["messages"][0]["content"].as_str().unwrap_or_default().contains("esp32-agent"));
        assert_eq!(first["messages"][1]["role"], "user");
        assert_eq!(first["tools"][0]["function"]["name"], "board_status");

        let second: Value = serde_json::from_str(&transport.bodies[1]).expect("json");
        let tool_message = second["messages"]
            .as_array()
            .expect("array")
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("a tool message in the second request");
        assert_eq!(tool_message["tool_call_id"], "call_1");
        assert!(tool_message["content"].as_str().unwrap_or_default().contains("SENSOR-OK"));
        let assistant = &second["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"], "{\"detail\":true}",
            "arguments must be re-sent as a JSON string"
        );

        // History: user, assistant(tool call), tool, assistant(text).
        let roles: Vec<Role> = agent.history.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
        assert!(!agent
            .history
            .iter()
            .any(|m| m.role == Role::System || m.text.contains("You are the agent")));
        assert_eq!(agent.runs, 1);
        assert!(agent.last_error.is_empty());
        assert!(agent.pending.is_none());
    }

    #[test]
    fn an_idle_run_is_a_no_op_and_an_empty_prompt_is_not_queued() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let mut transport = StubTransport::new(Vec::new());

        assert!(!agent.push_user("   \n\t ", "http", 1));
        assert!(agent.pending.is_none());
        assert_eq!(agent.run_pending(&mut host, &mut transport, &cfg(), clock).as_deref(), Ok(""));
        assert_eq!(agent.runs, 0, "an idle tick is not a run");
        assert!(transport.bodies.is_empty());
        assert!(agent.history.is_empty());
    }

    #[test]
    fn a_second_trigger_coalesces_into_the_pending_message() {
        let mut agent = Agent::new();
        assert!(agent.push_user("first", "schedule:t1", 5));
        assert!(agent.push_user("second", "http", 6));
        let pending = agent.pending.as_ref().expect("still one pending message");
        assert_eq!(pending.text, "second", "newest text wins");
        assert_eq!(pending.source, "schedule:t1,http");
        assert_eq!(pending.at_ms, 6);
        // Merging is idempotent, not additive.
        assert!(agent.push_user("third", "http", 7));
        assert_eq!(agent.pending.as_ref().unwrap().source, "schedule:t1,http");
    }

    #[test]
    fn history_keeps_the_ring_bounded_and_the_transcript_valid() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let replies: Vec<String> = (0..40).map(|i| answer_reply(&format!("answer {i}"))).collect();
        let mut transport = StubTransport::new(replies);

        for i in 0..40 {
            assert!(agent.push_user(&format!("question {i}"), "http", i as u64));
            agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("answer");
        }

        assert!(
            agent.history.len() <= MAX_HISTORY_MESSAGES,
            "history grew to {} messages",
            agent.history.len()
        );
        // 80 messages were produced, so the oldest questions are gone and the
        // newest turn is intact.
        assert_eq!(agent.history[0].role, Role::User);
        assert_eq!(agent.history[0].text, format!("question {}", 40 - agent.history.len() / 2));
        assert_eq!(agent.history.last().map(|m| m.text.as_str()), Some("answer 39"));
        assert!(agent.history_bytes() <= MAX_HISTORY_BYTES);
        assert_eq!(agent.runs, 40);
    }

    #[test]
    fn a_tool_result_is_truncated_at_the_result_budget_with_a_marker() {
        let mut agent = Agent::new();
        let mut stomp = StubHost::returning(&"y".repeat(MAX_TOOL_RESULT_BYTES + 1000));
        let mut transport =
            StubTransport::new(vec![tool_call_reply("call_big", "board_status", "{}"), answer_reply("ok")]);

        agent.push_user("read everything", "http", 1);
        agent.run_pending(&mut stomp, &mut transport, &cfg(), clock).expect("answer");

        let tool_message = agent.history.iter().find(|m| m.role == Role::Tool).expect("a tool message");
        assert!(tool_message.text.ends_with(llm::TRUNCATED_MARK));
        assert!(
            tool_message.text.len() <= MAX_TOOL_RESULT_BYTES + llm::TRUNCATED_MARK.len(),
            "tool result is not bounded: {} bytes",
            tool_message.text.len()
        );
        // The truncated text is what the next request carries.
        assert!(!transport.bodies[1].contains(&"y".repeat(MAX_TOOL_RESULT_BYTES + 1)));
    }

    #[test]
    fn an_unparsable_argument_string_becomes_a_tool_error_not_a_tool_call() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("should not run");
        let mut transport = StubTransport::new(vec![
            tool_call_reply("call_bad", "fs_write", "{\"path\": \"/a.txt\""),
            answer_reply("I could not write the file"),
        ]);

        agent.push_user("write a file", "http", 1);
        let text = agent
            .run_pending(&mut host, &mut transport, &cfg(), clock)
            .expect("the loop still ends with an answer");
        assert_eq!(text, "I could not write the file");
        assert!(host.calls.is_empty(), "the tool must not be called");

        let tool_message = agent.history.iter().find(|m| m.role == Role::Tool).expect("a tool message");
        assert!(tool_message.text.contains("arguments"), "{}", tool_message.text);
        assert!(tool_message.text.contains("not JSON"), "{}", tool_message.text);
        // The model sees `{}` on the way back out, not our private error key.
        let second: Value = serde_json::from_str(&transport.bodies[1]).expect("json");
        assert_eq!(second["messages"][2]["tool_calls"][0]["function"]["arguments"], "{}");
    }

    /// Zero means no cap: the loop runs until the model stops asking for tools.
    ///
    /// The default is zero, and a fixed cap truncated work that would have finished — a
    /// five-step task died at step four with "step limit reached". A run that really has to
    /// be bounded sets the number, which is why the previous test still exists.
    #[test]
    fn a_zero_step_limit_lets_the_model_finish_its_work() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let mut transport = StubTransport::new(vec![
            tool_call_reply("c1", "board_status", "{}"),
            tool_call_reply("c2", "board_status", "{}"),
            tool_call_reply("c3", "board_status", "{}"),
            tool_call_reply("c4", "board_status", "{}"),
            answer_reply("done after six calls"),
        ]);
        let config = LlmConfig { max_steps: 0, ..cfg() };
        assert_eq!(LlmConfig::default().max_steps, 0, "unlimited is the default");

        agent.push_user("keep going", "http", 1);
        let text = agent.run_pending(&mut host, &mut transport, &config, clock).expect("finishes");
        assert_eq!(text, "done after six calls");
        assert_eq!(transport.bodies.len(), 5, "four tool rounds plus the answer");
        assert_eq!(host.calls.len(), 4);
        assert!(agent.last_error.is_empty(), "{}", agent.last_error);
    }

    /// A model that only calls tools must not survive a zero limit, but compression must:
    /// the transcript shrinks and the index tells the model what it already did.
    #[test]
    fn compression_keeps_the_last_exchanges_and_indexes_the_rest() {
        let mut agent = Agent::new();
        // Enough answered exchanges to exceed the byte budget on their own: each tool
        // result is capped at 2 KB, so this needs more than a dozen of them.
        for i in 0..14 {
            agent.history.push(Message::user(&format!("question {i}"), "http", 10 * i));
            let call = ToolCall { id: "c".into(), name: "board_status".into(), arguments: json!({}) };
            agent.history.push(Message::assistant("calling", vec![call], 10 * i + 1));
            agent.history.push(Message::tool("c", &"x".repeat(MAX_TOOL_RESULT_BYTES), 10 * i + 2));
        }
        let before = agent.history.len();
        agent.compress(1);
        assert_eq!(agent.history.len(), 3, "one exchange survives: question, call, result");
        assert_eq!(agent.history[0].text, "question 13");
        assert!(before > agent.history.len());
        assert!(!agent.archive.is_empty(), "dropped work must leave an index");
        assert!(
            agent.archive.iter().any(|line| line.starts_with("called: board_status"))
                && agent.archive.iter().any(|line| line.starts_with("result:")),
            "{:?}",
            agent.archive
        );

        // The index is what the prompt carries — no summary, just the lines.
        let mut host = StubHost::returning("{}");
        let prompt = agent.system_message(&mut host);
        assert!(prompt.contains("Earlier in this conversation"), "{prompt}");
        assert!(prompt.contains("board_status"), "{prompt}");
    }

    #[test]
    fn the_trigger_follows_the_configured_window_and_the_script_it_is_written_in() {
        // A window of 8192 fires at 80% of it, as the host agent's trigger_tokens does.
        assert_eq!(trigger_tokens(8192), 6553);
        assert_eq!(trigger_tokens(1), 1, "a degenerate window still has a trigger");
        assert_eq!(trigger_tokens(0), 1, "and a zero window does not divide by zero");

        // Same character count, different scripts: Chinese costs six quarters a character
        // against one for ASCII, so a mixed board compacts earlier than an ASCII one.
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100, "four ASCII characters to a token");
        assert_eq!(estimate_tokens(&"\u{5de5}\u{5177}".repeat(200)), 600, "1.5 tokens a character");

        // A budget set through set_budget is what trim uses, not the constants.
        let mut agent = Agent::new();
        agent.set_budget(4096, 512);
        assert_eq!(agent.budget(), (4096, 512));
        assert_eq!(agent.budget(), (agent.budget().0.max(1), agent.budget().1.max(1)));
        let small = keep_recent_tokens(4096, 512);
        let large = keep_recent_tokens(65536, 512);
        assert!(large >= small, "a bigger window keeps at least as much: {small} vs {large}");
        assert!(small >= 1, "one exchange always survives");
    }

    #[test]
    fn compaction_fires_on_the_token_budget_and_not_before_it() {
        // A window small enough that a handful of replies crosses 80% of it.
        let mut agent = Agent::new();
        agent.set_budget(600, 64);

        // Under the trigger: three short exchanges must not be compressed.
        for i in 0..3 {
            agent.history.push(Message::user(&format!("q{i}"), "http", 10 * i));
            agent.history.push(Message::assistant("a", Vec::new(), 10 * i + 1));
        }
        let before = agent.history.len();
        assert!(agent.history_tokens() < trigger_tokens(600), "{} tokens", agent.history_tokens());
        assert!(!agent.needs_compaction(), "under 80% of the window nothing fires");
        agent.trim();
        assert_eq!(agent.history.len(), before, "nothing to compress yet");

        // Over the trigger: the same history with one long reply does compress, and what
        // survives is the tail, priced in tokens rather than in messages.
        agent.history.push(Message::user("long", "http", 99));
        agent.history.push(Message::assistant(&"x".repeat(2400), Vec::new(), 100));
        assert!(agent.history_tokens() > trigger_tokens(600));
        assert!(agent.needs_compaction(), "over the window the trigger is armed");
        agent.trim();
        assert!(
            agent.history.len() < 8,
            "the oldest exchanges are gone: {:?}",
            agent.history.iter().map(|m| m.text.len()).collect::<Vec<_>>()
        );
        assert!(agent.archive.iter().any(|l| l.starts_with("user: q0")), "{:?}", agent.archive);
        assert_eq!(
            agent.history.last().map(|m| m.text.len()),
            Some(2400),
            "the exchange in flight is never the one compressed away"
        );
    }

    #[test]
    fn compression_never_orphans_a_tool_result_and_the_run_resumes() {
        // Two exchanges of work, then a third mid-flight: the assistant asked for a tool
        // and the result is in. Compressing here must not cut between the two, or the next
        // request carries a `tool_call_id` with no message and every provider rejects it.
        let mut agent = Agent::new();
        for i in 0..2 {
            agent.history.push(Message::user(&format!("task {i}"), "http", 10 * i));
            let call = ToolCall { id: format!("c{i}"), name: "board_status".into(), arguments: json!({}) };
            agent.history.push(Message::assistant("checking", vec![call], 10 * i + 1));
            agent.history.push(Message::tool(&format!("c{i}"), "{}", 10 * i + 2));
        }
        agent.history.push(Message::user("task 2", "http", 30));
        let call = ToolCall { id: "c2".into(), name: "i2c_scan".into(), arguments: json!({}) };
        agent.history.push(Message::assistant("scanning", vec![call], 31));
        agent.history.push(Message::tool("c2", "{\"addresses\":[\"0x3C\"]}", 32));

        agent.compress(1);

        assert_eq!(agent.history[0].role, Role::User, "compression never starts on a tool result");
        let mut open: Vec<String> = Vec::new();
        for message in &agent.history {
            match message.role {
                Role::Assistant => open.extend(message.tool_calls.iter().map(|c| c.id.clone())),
                Role::Tool => {
                    let id = message.tool_call_id.clone().unwrap_or_default();
                    let at = open.iter().position(|o| *o == id).expect("a result for an opened call");
                    open.remove(at);
                }
                _ => {}
            }
        }
        assert!(open.is_empty(), "every tool call kept its result: {open:?}");

        // And the run continues with the index in the prompt.
        let mut host = StubHost::returning("{}");
        let prompt = agent.system_message(&mut host);
        assert!(prompt.contains("Earlier in this conversation"), "the index is carried");
        assert!(agent.history_tokens() > 0);
    }

    #[test]
    fn a_long_conversation_compacts_mid_run_and_the_next_turn_still_goes_out() {
        // The whole loop, four turns deep, with a window small enough that a later turn
        // crosses it. This is the "does it resume" question: compaction happens between
        // turns, and the request built afterwards has to be one a provider will accept.
        let mut agent = Agent::new();
        agent.set_budget(900, 128);
        let mut host = StubHost::returning("{\"ok\":true}");

        for turn in 0..6u64 {
            assert!(agent.push_user(&format!("turn {turn}"), "portal", 10 * turn));
            let step = agent.begin(&mut host, || 0, 0).expect("a pending message starts a run");
            assert!(matches!(step, AgentStep::Ask { .. }), "turn {turn} asks the model");
            // A long reply with no tool call: it ends the turn and grows the transcript.
            let reply = llm::ModelReply { content: "y".repeat(600), ..Default::default() };
            agent.apply(&mut host, Ok(reply), || 0);
        }

        assert!(!agent.archive.is_empty(), "the conversation outgrew the window and compacted");
        assert_eq!(agent.history[0].role, Role::User, "the tail still starts with a question");

        // A fifth turn is accepted and produces a request: the run resumed rather than
        // wedging on a transcript the model would reject.
        assert!(agent.push_user("turn 4", "portal", 99));
        let step = agent.begin(&mut host, || 0, 0).expect("the run resumes");
        let messages = match step {
            AgentStep::Ask { messages, .. } => messages,
            _ => panic!("expected a request, not a finished run"),
        };
        assert!(messages.as_array().map(|m| !m.is_empty()).unwrap_or(false), "the request carries messages");
        assert!(agent.last_error.is_empty(), "{}", agent.last_error);
    }

    #[test]
    fn a_question_survives_compression_verbatim_and_a_demoted_answer_is_counted() {
        // The host agent's deterministic projection protects user text and demotes
        // assistant text, counting the demotions. This is that behaviour, and the reason
        // it matters: a model that reads a clipped question re-asks what it was told.
        let mut agent = Agent::new();
        let question = "rewire the pump relay to GPIO12 and keep the pulse under 2 s, \
                        the old pin is shorted to ground so do not touch it";
        agent.history.push(Message::user(question, "portal", 1));
        agent.history.push(Message::assistant("ok, moving it", Vec::new(), 2));
        for i in 0..3 {
            agent.history.push(Message::user(&format!("q{i}"), "portal", 10 + i));
            agent.history.push(Message::assistant(
                &format!("a{i} that is a long answer {}", "x".repeat(200)),
                Vec::new(),
                20 + i,
            ));
        }

        agent.compress(1);

        let user_line = agent
            .archive
            .iter()
            .find(|l| l.starts_with("user: rewire"))
            .expect("the question is in the index");
        assert_eq!(
            user_line.trim_start_matches("user: "),
            question,
            "a question is kept whole, not clipped to its first line"
        );
        assert!(
            agent.archive.iter().any(|l| l.starts_with("answer: ok, moving it")),
            "an answer is reduced: {:?}",
            agent.archive
        );
        assert_eq!(agent.demoted_answers, 3, "every dropped answer is counted");

        let mut host = StubHost::returning("{}");
        let prompt = agent.system_message(&mut host);
        assert!(
            prompt.contains("earlier answer(s) are reduced to their first line"),
            "the prompt states the demotion instead of hiding it: {prompt}"
        );
        assert!(prompt.contains("the old pin is shorted to ground"), "the whole question is carried");
    }

    #[test]
    fn forgetting_keeps_the_last_exchanges_or_none_of_them() {
        let mut agent = Agent::new();
        for i in 0..3 {
            agent.history.push(Message::user(&format!("q{i}"), "http", 10 * i));
            agent.history.push(Message::assistant(&format!("a{i}"), Vec::new(), 10 * i + 1));
        }
        agent.archive.push("user: old".into());

        let (removed, kept) = agent.forget(1);
        assert_eq!(removed, 4, "two exchanges, two messages each");
        assert_eq!(kept, 2);
        assert_eq!(agent.history[0].text, "q2");
        assert!(agent.archive.is_empty(), "an index of what is gone is worse than none");

        let (removed, kept) = agent.forget(0);
        assert_eq!(removed, 2);
        assert_eq!(kept, 0);
        assert!(agent.history.is_empty());
    }

    #[test]
    fn the_memory_index_reaches_the_system_prompt() {
        struct Remembering;
        impl ToolHost for Remembering {
            fn tool_specs(&self) -> Value {
                json!([])
            }
            fn call_tool(&mut self, _n: &str, _a: &Value) -> Result<String, String> {
                Ok("{}".into())
            }
            fn memory_index(&self) -> Option<String> {
                Some("- [pins](/memory/pins.md) — GPIO18 is the strip\n".into())
            }
        }
        let agent = Agent::new();
        let prompt = agent.system_message(&mut Remembering);
        assert!(prompt.contains("## Memory (/MEMORY.md)"), "{prompt}");
        assert!(prompt.contains("memory/pins.md"), "{prompt}");

        let prompt = agent.system_message(&mut StubHost::returning("{}"));
        assert!(prompt.contains("nothing yet"), "a fresh board says so: {prompt}");
    }

    /// A model that only calls tools must stop at the step limit.
    #[test]
    fn a_model_that_only_calls_tools_ends_at_the_step_limit() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let mut transport = StubTransport::new(vec![
            tool_call_reply("c1", "board_status", "{}"),
            tool_call_reply("c2", "board_status", "{}"),
            tool_call_reply("c3", "board_status", "{}"),
        ]);
        let mut config = cfg();
        config.max_steps = 2;

        agent.push_user("loop forever", "http", 1);
        let err = agent.run_pending(&mut host, &mut transport, &config, clock).expect_err("must stop");
        assert!(err.contains("step limit 2"), "unexpected error: {err}");
        assert_eq!(transport.bodies.len(), 2, "never more model calls than steps");
        assert_eq!(host.calls.len(), 2);
        // The gathered results are kept so the next turn can continue.
        assert_eq!(agent.history.iter().filter(|m| m.role == Role::Tool).count(), 2);
        assert_eq!(agent.last_error, err);
    }

    /// A manifest change inside one turn must show up in the very next request.
    ///
    /// The model applies a manifest and then calls the tool it just declared. While the
    /// tool list was captured at the start of the turn, that second call was rejected as
    /// "not on the tool list": the feature only worked across turns, which is not what
    /// the device promises. Found on the board, not by a test.
    #[test]
    fn a_tool_created_mid_turn_is_offered_in_the_next_request() {
        struct GrowingHost {
            specs: Value,
            calls: Vec<String>,
        }

        impl ToolHost for GrowingHost {
            fn tool_specs(&self) -> Value {
                self.specs.clone()
            }

            fn call_tool(&mut self, name: &str, _args: &Value) -> Result<String, String> {
                self.calls.push(name.to_string());
                if name == "manifest_apply" {
                    self.specs = json!([
                        { "type": "function", "function": { "name": "manifest_apply" } },
                        { "type": "function", "function": { "name": "bench_new" } },
                    ]);
                }
                Ok("{}".to_string())
            }
        }

        let mut host = GrowingHost {
            specs: json!([{ "type": "function", "function": { "name": "manifest_apply" } }]),
            calls: Vec::new(),
        };
        let mut transport = StubTransport::new(vec![
            tool_call_reply("c1", "manifest_apply", "{}"),
            tool_call_reply("c2", "bench_new", "{}"),
            answer_reply("both ran"),
        ]);
        let mut agent = Agent::new();
        let mut config = cfg();
        config.max_steps = 4;

        agent.push_user("add a tool and then use it", "http", 1);
        let text = agent.run_pending(&mut host, &mut transport, &config, clock).expect("answer");
        assert_eq!(text, "both ran");
        assert_eq!(host.calls, vec!["manifest_apply", "bench_new"]);

        let second: Value = serde_json::from_str(&transport.bodies[1]).expect("json");
        let names: Vec<&str> = second["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(
            names.contains(&"bench_new"),
            "the tool the model just declared must be advertised: {names:?}"
        );
    }

    #[test]
    fn a_transport_failure_is_recorded_as_the_last_error() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let mut transport = StubTransport::new(Vec::new());

        agent.push_user("hello", "http", 1);
        let err = agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect_err("must fail");
        assert!(err.contains("no reply left"), "unexpected error: {err}");
        assert_eq!(agent.last_error, err);
        assert_eq!(agent.runs, 1);
        // The user turn stays, so a retry can answer it.
        assert_eq!(agent.history.len(), 1);
        assert_eq!(agent.history[0].role, Role::User);
    }

    /// Fails `failures` times with `status` and then answers: the shape of a turn that a
    /// retry rescued.
    struct FlakyTransport {
        failures: usize,
        status: u16,
        calls: usize,
        reply: String,
    }

    impl ChatTransport for FlakyTransport {
        fn post_json(
            &mut self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &str,
        ) -> Result<String, String> {
            self.calls += 1;
            if self.calls <= self.failures {
                return Err(llm::http_error(self.status, "the model is restarting"));
            }
            Ok(self.reply.clone())
        }
    }

    /// A clock that records the waits instead of taking them.
    #[derive(Default)]
    struct FakeClock {
        now: std::cell::Cell<u64>,
        sleeps: std::cell::RefCell<Vec<u64>>,
    }
    impl llm::RetryTimer for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }

        fn sleep_ms(&self, ms: u64) {
            self.sleeps.borrow_mut().push(ms);
            self.now.set(self.now.get().saturating_add(ms));
        }
    }

    /// Answers from a script where `Err(status)` is a failure: the shape of a turn whose
    /// second model request had to be retried.
    struct ScriptedTransport {
        script: std::collections::VecDeque<Result<String, u16>>,
        calls: usize,
    }

    impl ChatTransport for ScriptedTransport {
        fn post_json(
            &mut self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &str,
        ) -> Result<String, String> {
            self.calls += 1;
            match self.script.pop_front() {
                Some(Ok(body)) => Ok(body),
                Some(Err(status)) => Err(llm::http_error(status, "the model is restarting")),
                None => Err("script: no reply left".to_string()),
            }
        }
    }

    /// A retried request is one note and one answer. A client reading the journal must not
    /// see the failed attempts — three failures and one success is what a broken run looks
    /// like — and the retry must not have run a tool twice.
    #[test]
    fn a_retried_model_request_is_said_once_and_still_answers() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("should not run");
        let mut transport =
            FlakyTransport { failures: 2, status: 503, calls: 0, reply: answer_reply("it is fine") };
        let timer = FakeClock::default();

        agent.push_user("is the board ok?", "http", 1);
        let text = agent
            .run_pending_with(&mut host, &mut transport, &cfg(), clock, &llm::RetryPolicy::default(), &timer)
            .expect("the third attempt answers");

        assert_eq!(text, "it is fine");
        assert_eq!(transport.calls, 3, "the request really was tried three times");
        assert_eq!(*timer.sleeps.borrow(), vec![llm::BASE_DELAY_MS, llm::BASE_DELAY_MS * 2]);
        let kinds: Vec<&str> = agent.events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "retried").count(), 1, "one note: {kinds:?}");
        assert_eq!(kinds.iter().filter(|k| **k == "error").count(), 0, "no failure: {kinds:?}");
        let note = agent.events.iter().find(|e| e.kind == "retried").expect("the note");
        assert!(note.text.contains("retried 2 times"), "{}", note.text);
        assert!(note.text.contains("HTTP 503"), "the cause is named: {}", note.text);

        // One answer in the transcript, and no trace of the attempts the model never saw.
        let answers: Vec<&str> =
            agent.history.iter().filter(|m| m.role == Role::Assistant).map(|m| m.text.as_str()).collect();
        assert_eq!(answers, vec!["it is fine"]);
        assert!(host.calls.is_empty(), "nothing but the model request was retried");
    }

    /// Exhausted attempts are one `error`, with the count and the last message, and the
    /// user's turn stays in the transcript so the next tick can try again.
    #[test]
    fn exhausted_attempts_journal_one_error_with_the_count_and_the_last_message() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let mut transport =
            FlakyTransport { failures: usize::MAX, status: 503, calls: 0, reply: String::new() };
        let timer = FakeClock::default();

        agent.push_user("hello", "http", 1);
        let err = agent
            .run_pending_with(&mut host, &mut transport, &cfg(), clock, &llm::RetryPolicy::default(), &timer)
            .expect_err("three failures is a failure");

        assert_eq!(transport.calls, llm::MAX_ATTEMPTS as usize);
        assert!(err.contains("gave up after 3 attempts"), "{err}");
        assert!(err.contains("HTTP 503"), "{err}");

        let errors: Vec<&Event> = agent.events.iter().filter(|e| e.kind == "error").collect();
        assert_eq!(errors.len(), 1, "one error event, not one per attempt");
        assert!(errors[0].text.contains("3 attempts"), "{}", errors[0].text);
        assert!(errors[0].text.contains("HTTP 503"), "{}", errors[0].text);
        assert_eq!(agent.events.iter().filter(|e| e.kind == "retried").count(), 0);
        assert!(
            matches!(agent.events.back(), Some(e) if e.kind == "run_end" && e.is_error),
            "the run is closed as an error"
        );
        assert_eq!(agent.last_error, err);
        assert_eq!(agent.history.len(), 1, "nothing was invented");
        assert_eq!(agent.history[0].role, Role::User);
    }

    /// A retry is of the model request, never of a tool call that already succeeded: the
    /// second attempt sends the same transcript, so the tool runs once and its result is
    /// carried into the retried request rather than produced again.
    #[test]
    fn a_retry_of_a_later_request_does_not_run_the_tool_again() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning(r#"{"boot":true}"#);
        let mut transport = ScriptedTransport {
            script: vec![
                Ok(tool_call_reply("c1", "board_status", "{}")),
                Err(503),
                Err(503),
                Ok(answer_reply("the board is up")),
            ]
            .into(),
            calls: 0,
        };
        let timer = FakeClock::default();

        agent.push_user("is the board up?", "http", 1);
        let text = agent
            .run_pending_with(&mut host, &mut transport, &cfg(), clock, &llm::RetryPolicy::default(), &timer)
            .expect("the fourth request answers");

        assert_eq!(text, "the board is up");
        assert_eq!(transport.calls, 4);
        assert_eq!(host.calls.len(), 1, "the tool ran once, not once per attempt");
        assert_eq!(host.calls[0].0, "board_status");
        assert_eq!(
            agent.events.iter().filter(|e| e.kind == "tool_result").count(),
            1,
            "one result, however many attempts the next request took"
        );
        assert_eq!(agent.events.iter().filter(|e| e.kind == "retried").count(), 1);
        // The retried request carries the result that already landed.
        let roles: Vec<Role> = agent.history.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
    }

    #[test]
    fn a_run_never_leads_its_history_with_a_tool_result() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        // Turn one ends in a tool call, turn two pushes it past the ring.
        let mut replies = vec![tool_call_reply("c1", "board_status", "{}"), answer_reply("done one")];
        for i in 0..24 {
            replies.push(answer_reply(&format!("filler {i}")));
        }
        let mut transport = StubTransport::new(replies);

        agent.push_user("one", "http", 1);
        agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("answer");
        for i in 0..24 {
            agent.push_user(&format!("filler {i}"), "http", 2);
            agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("answer");
        }
        assert!(!agent.history.is_empty(), "the newest messages must survive trimming");
        assert_ne!(agent.history[0].role, Role::Tool);
        // Every request the loop built is a transcript OpenAI would accept: the
        // first non-system message is never a tool result.
        for body in &transport.bodies {
            let value: Value = serde_json::from_str(body).expect("json");
            let msgs = value["messages"].as_array().expect("array");
            assert_eq!(msgs[0]["role"], "system");
            assert_ne!(msgs.get(1).map(|m| m["role"].clone()), Some(json!("tool")));
        }
    }

    #[test]
    fn messages_json_pages_from_a_sequence_number() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning("{}");
        let mut transport = StubTransport::new(vec![answer_reply("hi there")]);

        agent.push_user("hello", "http", 42);
        agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("answer");

        let all = agent.messages_json(0);
        assert_eq!(all["seq"], 2);
        assert_eq!(all["messages"].as_array().map(|m| m.len()), Some(2));
        assert_eq!(all["messages"][0]["role"], "user");
        assert_eq!(all["messages"][0]["source"], "http");
        assert_eq!(all["messages"][0]["at_ms"], 42);
        assert_eq!(all["messages"][1]["role"], "assistant");
        assert_eq!(all["messages"][1]["text"], "hi there");

        let tail = agent.messages_json(1);
        assert_eq!(tail["messages"].as_array().map(|m| m.len()), Some(1));
        assert_eq!(tail["messages"][0]["seq"], 1);
        // Polling past the end is empty, not an error.
        assert_eq!(agent.messages_json(99)["messages"].as_array().map(|m| m.len()), Some(0));

        let transcript = agent.transcript_json();
        assert_eq!(transcript["count"], 2);
        assert_eq!(transcript["runs"], 1);
        assert_eq!(transcript["pending"], false);
        assert_eq!(transcript["last_error"], "");

        agent.reset();
        assert!(agent.history.is_empty());
        assert!(agent.pending.is_none());
        assert_eq!(agent.runs, 0);
        assert_eq!(agent.transcript_json()["count"], 0);
    }

    // ------------------------------------------------ the transcript between boots

    /// Three turns of real work, through the stored form and back: roles, text, the
    /// reasoning that produced a call, the call itself and the result that answers it.
    #[test]
    fn a_transcript_round_trips_through_the_stored_form() {
        let mut agent = Agent::new();
        for turn in 0..3u64 {
            agent.history.push(Message::user(&format!("question {turn}"), "http", 10 * turn));
            let call = ToolCall {
                id: format!("c{turn}"),
                name: "board_status".into(),
                arguments: json!({ "detail": true }),
            };
            agent.history.push(Message::assistant_with_reasoning(
                "checking",
                &format!("because {turn}"),
                vec![call],
                10 * turn + 1,
            ));
            agent.history.push(Message::tool_result(
                &format!("c{turn}"),
                &format!("{{\"boot\":true,\"turn\":{turn}}}"),
                false,
                10 * turn + 2,
            ));
            agent.history.push(Message::assistant(&format!("answer {turn}"), Vec::new(), 10 * turn + 3));
        }

        let stored = transcript_jsonl(&agent.history, 999);
        let (restored, dropped) = transcript_from_jsonl(&stored).expect("a stored transcript parses");
        assert_eq!(dropped, 0, "everything fitted, so nothing was dropped");
        assert_eq!(restored.len(), agent.history.len());

        for (before, after) in agent.history.iter().zip(restored.iter()) {
            assert_eq!(before.role, after.role);
            assert_eq!(before.text, after.text);
            assert_eq!(before.reasoning, after.reasoning);
            assert_eq!(before.at_ms, after.at_ms);
            assert_eq!(before.tool_call_id, after.tool_call_id);
            assert_eq!(before.is_error, after.is_error);
            assert_eq!(before.source, after.source);
            assert_eq!(
                before.tool_calls.iter().map(|c| (&c.id, &c.name)).collect::<Vec<_>>(),
                after.tool_calls.iter().map(|c| (&c.id, &c.name)).collect::<Vec<_>>()
            );
        }
        // The arguments come back as JSON, not as a string that still has to be parsed.
        let call = &restored[1].tool_calls[0];
        assert_eq!(call.arguments, json!({ "detail": true }));
        // The tool results are intact, which is the whole point: the model must be able to
        // see what a call returned without the call running again.
        assert_eq!(restored[2].text, "{\"boot\":true,\"turn\":0}");
        assert_eq!(restored[2].tool_call_id.as_deref(), Some("c0"));

        // An image never reaches flash: it is larger than the budget and the model already
        // saw it during the turn that carried it.
        let with_image = vec![Message::user("look", "http", 1).with_attachments(vec![Attachment {
            name: "shot.jpg".into(),
            mime: "image/jpeg".into(),
            data_b64: "aGVsbG8=".into(),
        }])];
        let (restored, _) = transcript_from_jsonl(&transcript_jsonl(&with_image, 2)).expect("parses");
        assert!(restored[0].attachments.is_empty());
        assert_eq!(restored[0].text, "look");
    }

    #[test]
    fn the_stored_form_cuts_long_text_and_reasoning_with_the_repo_marker() {
        let history = vec![
            Message::user(&"q".repeat(400), "http", 1),
            Message::assistant_with_reasoning(
                &"a".repeat(400),
                &"r".repeat(400),
                vec![ToolCall {
                    id: "c1".into(),
                    name: "fs_read".into(),
                    arguments: json!({ "path": "/AGENTS.md" }),
                }],
                2,
            ),
            Message::tool("c1", &"y".repeat(600), 3),
        ];
        let stored = transcript_jsonl(&history, 4);
        assert!(!stored.contains(&"q".repeat(400)), "the whole question is not stored");
        let (restored, dropped) = transcript_from_jsonl(&stored).expect("parses");
        assert_eq!(dropped, 0, "cutting a message is not dropping it");
        assert!(restored[0].text.ends_with(llm::TRUNCATED_MARK));
        assert!(restored[0].text.len() <= MAX_SAVED_TEXT_BYTES + llm::TRUNCATED_MARK.len());
        assert!(restored[1].text.ends_with(llm::TRUNCATED_MARK));
        assert!(restored[1].reasoning.ends_with(llm::TRUNCATED_MARK));
        assert!(restored[1].reasoning.len() <= MAX_SAVED_REASONING_BYTES + llm::TRUNCATED_MARK.len());
        assert!(restored[2].text.ends_with(llm::TRUNCATED_MARK));
        // The cut does not break the pairing the next request depends on.
        assert_eq!(restored[1].tool_calls[0].id, "c1");
        assert_eq!(restored[2].tool_call_id.as_deref(), Some("c1"));
        // In RAM the message keeps everything; only the stored copy is cut.
        assert_eq!(history[0].text.len(), 400);
        // Every line is one message: a JSON Lines file is not a JSON document.
        let lines: Vec<&str> = stored.lines().collect();
        assert_eq!(lines.len(), 4, "a header and three messages");
        let header: Value = serde_json::from_str(lines[0]).expect("the header is JSON");
        assert_eq!(header["v"], json!(TRANSCRIPT_VERSION));
        assert_eq!(header["dropped"], json!(0));
    }

    #[test]
    fn a_transcript_over_the_budget_keeps_its_tail_and_counts_what_it_dropped() {
        let mut history = Vec::new();
        for i in 0..120u64 {
            history.push(Message::user(&format!("question {i} {}", "q".repeat(300)), "http", 10 * i));
            history.push(Message::assistant(
                &format!("answer {i} {}", "a".repeat(300)),
                Vec::new(),
                10 * i + 1,
            ));
        }
        let stored = transcript_jsonl(&history, 5_000);
        assert!(
            stored.len() <= MAX_SAVED_BYTES,
            "the stored tail is {} bytes, over the {MAX_SAVED_BYTES} budget",
            stored.len()
        );

        let (restored, dropped) = transcript_from_jsonl(&stored).expect("parses");
        assert!(dropped > 0, "a transcript this long must have dropped messages");
        assert_eq!(restored.len() + dropped, history.len(), "nothing appears or vanishes silently");
        assert!(restored.len() >= 10, "the tail is worth keeping: {} message(s) survived", restored.len());
        // What survived is the end of the conversation, in order, not a prefix.
        assert_eq!(
            restored.last().map(|m| m.text.clone()),
            Some(llm::truncate_bytes(&history.last().unwrap().text, MAX_SAVED_TEXT_BYTES))
        );
        assert_eq!(restored[0].text, llm::truncate_bytes(&history[dropped].text, MAX_SAVED_TEXT_BYTES));
        // And the header says how much is missing, which is what the prompt repeats.
        let header: Value = serde_json::from_str(stored.lines().next().unwrap()).expect("header");
        assert_eq!(header["dropped"], json!(dropped));
    }

    /// The case the whole checksum exists for: a power cut in the middle of a write leaves
    /// a blob whose tail is not the tail its header was built with.
    #[test]
    fn a_blob_torn_by_a_power_cut_is_refused_rather_than_half_adopted() {
        let mut agent = Agent::new();
        for i in 0..6u64 {
            agent.history.push(Message::user(&format!("question {i}"), "http", 10 * i));
            agent.history.push(Message::assistant(&format!("answer {i}"), Vec::new(), 10 * i + 1));
        }
        let stored = transcript_jsonl(&agent.history, 900);

        // Not JSON at all.
        assert!(transcript_from_jsonl("not json").is_none());
        // No header.
        assert!(transcript_from_jsonl("").is_none());
        assert!(transcript_from_jsonl("{\"role\":\"user\",\"text\":\"hi\"}\n{}").is_none());
        // Cut in the middle: the header is complete, the body is not.
        let cut = &stored[..stored.len() - 40];
        assert!(transcript_from_jsonl(cut).is_none(), "a torn body must not be adopted");
        // Cut exactly at a line boundary: still a different body than the header describes.
        let lines: Vec<&str> = stored.lines().collect();
        let short = format!("{}\n{}\n", lines[0], lines[1]);
        assert!(transcript_from_jsonl(&short).is_none(), "a shortened body must not be adopted");
        // A body edited in place.
        let edited = stored.replacen("answer 3", "answer 9", 1);
        assert!(transcript_from_jsonl(&edited).is_none(), "an edited body must not be adopted");
        // The intact blob still parses, so the checks above refused for the right reason.
        assert!(transcript_from_jsonl(&stored).is_some());
    }

    /// A tail whose first message is a tool result names a call that was cut with the
    /// message before it. Sending that to a provider is a 400 on every turn, so the tail
    /// is refused instead of being adopted with a hole in it.
    #[test]
    fn a_tool_result_whose_call_is_not_in_the_tail_is_refused() {
        let history = vec![
            Message::assistant(
                "checking",
                vec![ToolCall { id: "c1".into(), name: "board_status".into(), arguments: json!({}) }],
                1,
            ),
            Message::tool("c1", "{}", 2),
        ];
        let stored = transcript_jsonl(&history, 3);
        assert!(transcript_from_jsonl(&stored).is_some());

        // The same result with its call removed, as a bad tail would be.
        let orphan = json!({ "role": "tool", "tool_call_id": "c9", "text": "{}", "at_ms": 2 }).to_string();
        let body = format!("{orphan}\n");
        let header = stored_header(4, 0, checksum(body.as_bytes()));
        assert!(transcript_from_jsonl(&format!("{header}\n{body}")).is_none());

        // A call left open at the end is not an error: that is the interrupted turn.
        let interrupted = vec![
            Message::user("turn the pump on", "http", 1),
            Message::assistant(
                "",
                vec![ToolCall { id: "c1".into(), name: "gpio_write".into(), arguments: json!({}) }],
                2,
            ),
            Message::tool("c1", "{\"ok\":true}", 3),
        ];
        let (restored, _) = transcript_from_jsonl(&transcript_jsonl(&interrupted, 4)).expect("parses");
        assert!(restored.last().map(|m| m.role) == Some(Role::Tool));
        assert_eq!(restored.last().and_then(|m| m.tool_call_id.clone()).as_deref(), Some("c1"));
    }
}

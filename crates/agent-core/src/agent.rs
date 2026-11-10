//! The agent loop: one queued user turn in, a bounded sequence of model calls and
//! board tool calls out, and a transcript the portal can render.
//!
//! The loop is deliberately small and single-threaded. Everything that touches
//! the board arrives through [`ToolHost`], everything that touches the network
//! through [`ChatTransport`], and time is passed in as a `fn() -> u64` so the
//! loop itself never reads a clock and stays testable on the host.
//!
//! Bounds, because the board has ~300 KB of usable heap and this history is
//! persisted to NVS:
//!
//! * at most [`MAX_HISTORY_MESSAGES`] messages and [`MAX_HISTORY_BYTES`] of text;
//! * a user message is cut to [`MAX_USER_BYTES`];
//! * a tool result is cut to [`MAX_TOOL_RESULT_BYTES`] with `…(truncated)`;
//! * an assistant text is cut to [`MAX_REPLY_BYTES`];
//! * at most `cfg.max_steps` model calls per run (never an unbounded ping-pong).

use crate::llm::{self, ChatTransport, LlmConfig, ToolCall};
use serde_json::{json, Value};

/// Oldest messages are dropped above this count.
pub const MAX_HISTORY_MESSAGES: usize = 48;
/// Oldest messages are dropped above this much text.
pub const MAX_HISTORY_BYTES: usize = 24 * 1024;
/// A tool result larger than this is cut with [`llm::TRUNCATED_MARK`].
pub const MAX_TOOL_RESULT_BYTES: usize = 2048;
/// Assistant text larger than this is cut with [`llm::TRUNCATED_MARK`].
pub const MAX_REPLY_BYTES: usize = 8 * 1024;
/// A queued user message larger than this is cut before it enters the history.
pub const MAX_USER_BYTES: usize = 4096;

/// Reasoning is shown in the portal but never sent back as answer text, so it gets a
/// tighter cap than the reply itself.
pub const MAX_REASONING_BYTES: usize = 4096;

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
}

impl Message {
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
        }
    }
}

/// Everything the loop needs from the board, implemented by `App`.
pub trait ToolHost {
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
#[derive(Default)]
pub struct Agent {
    pub history: Vec<Message>,
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
}

/// One journal entry. The kinds are the host agent's event names on purpose
/// (`message_start`, `thinking_start|delta|end`, `text_start|delta|end`, `tool_use`,
/// `tool_progress`, `tool_result`, `usage`, `message_end`, `error`, `run_end`): a
/// client that reads one console can read the other. The board does not stream token
/// by token, so a delta carries the whole block; the names and the order are what
/// stay identical.
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
        self.pending = None;
        self.last_error.clear();
        self.runs = 0;
    }

    /// One step of a turn, as handed back to whoever drives it.
    ///
    /// `Ask` carries everything a model call needs and nothing that borrows the agent,
    /// which is the whole point: the caller can drop its lock, make the call, and come
    /// back with [`Agent::apply`].
    pub fn begin(
        &mut self,
        host: &mut dyn ToolHost,
        now_ms: fn() -> u64,
        max_steps: u8,
    ) -> Option<AgentStep> {
        let user = self.pending.take()?;
        self.runs = self.runs.saturating_add(1);
        self.history.push(user);
        self.trim();
        self.usage_run = llm::Usage::default();
        self.turn_steps = 0;
        self.turn_limit = max_steps.max(1);
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
        }
        self.trim();

        if self.turn_steps >= self.turn_limit {
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
        let mut step = match self.begin(host, now_ms, cfg.max_steps) {
            Some(step) => step,
            None => return Ok(String::new()),
        };
        loop {
            match step {
                AgentStep::Ask { messages, tools } => {
                    let reply = llm::chat(transport, cfg, &messages, &tools);
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
    fn trim(&mut self) {
        while self.history.len() > MAX_HISTORY_MESSAGES {
            self.history.remove(0);
        }
        while self.history_bytes() > MAX_HISTORY_BYTES && self.history.len() > 1 {
            self.history.remove(0);
        }
        while matches!(self.history.first().map(|m| m.role), Some(Role::Tool)) {
            self.history.remove(0);
        }
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
            out.push(openai_message(message));
        }
        Value::Array(out)
    }

    /// The static prompt plus whatever the host contributes: project instructions and
    /// the skill index.
    ///
    /// It is kept per run rather than in NVS because it must follow the files: editing
    /// `/AGENTS.md` or adding a skill takes effect on the next turn, with no restart and
    /// no transcript rewrite.
    fn system_message(&self, host: &dyn ToolHost) -> String {
        let mut text = String::from(system_prompt());
        if let Some(instructions) = host.project_instructions() {
            text.push_str("\n\n## Project instructions (/AGENTS.md)\nThese are the rules for this installation; follow them.\n");
            text.push_str(&instructions);
        }
        let skills = host.skills();
        if !skills.is_empty() {
            text.push_str(
                "\n\n## Skills\nEach line is `name: description` followed by the file to read. \
                 When one matches the task, open that file with fs_read before following it; a \
                 directory may hold other files next to SKILL.md (diagrams, tables, a pinout) and \
                 those are readable the same way. Do not reconstruct a procedure from its \
                 description.\n",
            );
            for skill in &skills {
                text.push_str(&format!("- {}: {} ({})\n", skill.name, skill.description, skill.path));
            }
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
     3. Prefer board_status, then the narrower tool. Never repeat a call whose result you already have.\n\
     4. If a tool fails, say what failed and what you tried; do not invent readings.\n\
     5. You cannot browse the internet except through the net_get/net_post tools, and you cannot reboot the board."
}

fn openai_message(message: &Message) -> Value {
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
    fn a_tool_result_is_truncated_to_two_kilobytes_with_a_marker() {
        let mut agent = Agent::new();
        let mut host = StubHost::returning(&"y".repeat(3000));
        let mut transport =
            StubTransport::new(vec![tool_call_reply("call_big", "board_status", "{}"), answer_reply("ok")]);

        agent.push_user("read everything", "http", 1);
        agent.run_pending(&mut host, &mut transport, &cfg(), clock).expect("answer");

        let tool_message = agent.history.iter().find(|m| m.role == Role::Tool).expect("a tool message");
        assert!(tool_message.text.ends_with(llm::TRUNCATED_MARK));
        assert!(
            tool_message.text.len() <= MAX_TOOL_RESULT_BYTES + llm::TRUNCATED_MARK.len(),
            "tool result is not bounded: {} bytes",
            tool_message.text.len()
        );
        // The truncated text is what the next request carries.
        assert!(!transport.bodies[1].contains(&"y".repeat(2100)));
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
}

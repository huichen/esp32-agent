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
    pub text: String,
    /// Assistant messages only.
    pub tool_calls: Vec<ToolCall>,
    /// Tool messages only: which call this answers.
    pub tool_call_id: Option<String>,
    /// `""` | `"http"` | `"schedule:<id>"` | `"sensor:<id>"`; user messages only.
    pub source: String,
    pub at_ms: u64,
}

impl Message {
    pub fn user(text: &str, source: &str, at_ms: u64) -> Self {
        Self {
            role: Role::User,
            text: llm::truncate_bytes(text, MAX_USER_BYTES),
            tool_calls: Vec::new(),
            tool_call_id: None,
            source: source.to_string(),
            at_ms,
        }
    }

    pub fn assistant(text: &str, tool_calls: Vec<ToolCall>, at_ms: u64) -> Self {
        Self {
            role: Role::Assistant,
            text: llm::truncate_bytes(text, MAX_REPLY_BYTES),
            tool_calls,
            tool_call_id: None,
            source: String::new(),
            at_ms,
        }
    }

    pub fn tool(tool_call_id: &str, text: &str, at_ms: u64) -> Self {
        Self {
            role: Role::Tool,
            text: llm::truncate_bytes(text, MAX_TOOL_RESULT_BYTES),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.to_string()),
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
}

/// The transcript plus the queue. Lives behind the app mutex.
#[derive(Default)]
pub struct Agent {
    pub history: Vec<Message>,
    /// Depth-1 queue: a trigger while a run is in flight coalesces into this.
    pub pending: Option<Message>,
    pub last_error: String,
    pub runs: u32,
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
            "messages": messages,
        })
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.pending = None;
        self.last_error.clear();
        self.runs = 0;
    }

    /// Drain `pending`, run the loop, append messages. Returns the final text.
    ///
    /// A run with nothing queued is `Ok("")`: the scheduler may tick every
    /// second without producing work. On error the tool results already gathered
    /// stay in the history, so the next turn can continue from them.
    pub fn run_pending(
        &mut self,
        host: &mut dyn ToolHost,
        transport: &mut dyn ChatTransport,
        cfg: &LlmConfig,
        now_ms: fn() -> u64,
    ) -> Result<String, String> {
        let Some(user) = self.pending.take() else {
            return Ok(String::new());
        };
        self.runs = self.runs.saturating_add(1);
        self.history.push(user);
        self.trim();

        let tools = host.tool_specs();
        // A zeroed config still deserves one model call rather than a silent
        // empty answer.
        let max_steps = cfg.max_steps.max(1);

        for _ in 0..max_steps {
            let messages = self.openai_messages();
            let reply = match llm::chat(transport, cfg, &messages, &tools) {
                Ok(reply) => reply,
                Err(e) => {
                    self.last_error = e.clone();
                    return Err(e);
                }
            };

            if reply.tool_calls.is_empty() {
                let text = llm::truncate_bytes(&reply.content, MAX_REPLY_BYTES);
                self.history.push(Message::assistant(&text, Vec::new(), now_ms()));
                self.trim();
                self.last_error.clear();
                return Ok(text);
            }

            self.history.push(Message::assistant(&reply.content, reply.tool_calls.clone(), now_ms()));
            for call in &reply.tool_calls {
                let at = now_ms();
                let result = match llm::arguments_error(call) {
                    // The model's argument string was not JSON: report it as a
                    // tool error and do not guess at what it meant.
                    Some(err) => json!({ "error": format!("arguments: {err}") }).to_string(),
                    None if call.name.trim().is_empty() => {
                        json!({ "error": "tool call without a name" }).to_string()
                    }
                    None => match host.call_tool(&call.name, &call.arguments) {
                        Ok(text) => text,
                        Err(e) => json!({ "error": e }).to_string(),
                    },
                };
                // `Message::tool` owns the 2 KB cut; the marker tells the model
                // the result it sees is incomplete.
                self.history.push(Message::tool(&call.id, &result, at));
            }
            self.trim();
        }

        let err = format!("agent: step limit {max_steps} reached before a final answer");
        self.last_error = err.clone();
        Err(err)
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

    /// The request payload: the generated system prompt followed by the history.
    fn openai_messages(&self) -> Value {
        let mut out = Vec::with_capacity(self.history.len() + 1);
        out.push(json!({ "role": Role::System.as_str(), "content": system_prompt() }));
        for message in &self.history {
            out.push(openai_message(message));
        }
        Value::Array(out)
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
            json!({ "role": Role::Assistant.as_str(), "content": content, "tool_calls": calls })
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
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> =
            message.tool_calls.iter().map(|call| json!({ "id": call.id, "name": call.name })).collect();
        out["tool_calls"] = Value::Array(calls);
    }
    if let Some(id) = &message.tool_call_id {
        out["tool_call_id"] = json!(id);
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

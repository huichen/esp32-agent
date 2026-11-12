//! OpenAI-compatible chat client: the single request shape the agent loop uses.
//!
//! The socket is injected as a [`ChatTransport`] because the board and the host
//! simulator do HTTP differently (the firmware's ESP-IDF client with esp-tls for
//! `https://`, the host's `std::net` with rustls), while everything above the
//! socket — the request body, the reply shape, the error text, the byte budget —
//! has to behave identically on both. `chat` never opens anything itself: it
//! serialises the request, hands it over, and turns the JSON body back into a
//! bounded [`ModelReply`].
//!
//! Two properties matter more than feature coverage here:
//!
//! * A model that answers with something unexpected must produce a `String`
//!   error or a degraded reply, never a panic: the agent thread has no supervisor.
//! * Every string that leaves this module is bounded, because the board has
//!   ~300 KB of usable heap and the history is persisted to NVS.

use serde_json::{json, Value};

/// Marker appended to any text this crate cuts at its byte budget.
pub const TRUNCATED_MARK: &str = "…(truncated)";

/// Key kept inside [`ToolCall::arguments`] when the model sent `arguments` that
/// is not valid JSON.
///
/// The brief requires an unusable argument string to become `{}` *and* to be
/// reported as a tool error instead of silently running the tool with no
/// arguments. A `ToolCall` has nowhere else to carry that message, so the error
/// text travels inside the empty object under this key; [`arguments_error`]
/// reads it, and the agent turns it into a tool-error message without calling
/// the tool at all. [`arguments_request_json`] scrubs it when the message is
/// echoed back to the model.
pub const ARGS_ERROR_KEY: &str = "__llm_arguments_error";

/// One HTTP POST, nothing more. Implementations enforce their own timeout and
/// must return non-2xx responses as `Err` built with [`http_error`], so the
/// status and the body prefix survive to the log without a second channel.
pub trait ChatTransport {
    /// POST `body` to `url` with `headers`; return the response body.
    ///
    /// `url` is `http://` or `https://`; the scheme is the implementation's business
    /// (plain socket or TLS), and the implementation enforces its own timeout.
    fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String>;

    /// Send one request with an explicit method and return the response body.
    ///
    /// The default keeps a transport that only knows POST working — it is still the right
    /// thing for the model call — but a real board transport overrides it, because a
    /// `net_get` that arrives as a POST is not a GET: any server that distinguishes the two
    /// answers 405, and the model learns the wrong thing about the request it made.
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<String, String> {
        if method.eq_ignore_ascii_case("POST") {
            self.post_json(url, headers, body)
        } else {
            Err(format!("this transport cannot send {method} requests"))
        }
    }
}

/// Where the model lives and how the loop should treat it.
///
/// `Default` is hand-written rather than derived: a zero timeout would make the first run
/// fail in a confusing way, so the documented defaults (0.2, 45 s) are what
/// `LlmConfig::default()` yields. `max_steps` is unlimited by default — the loop already
/// stops on its own (a final answer, a transport error, or the history budget), and a
/// fixed cap only truncates work that would have finished. Set it when a run has to be
/// bounded, e.g. on a metered endpoint.
#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// e.g. `http://192.0.2.10:8899/v1/chat/completions`
    pub base_url: String,
    /// e.g. `stub`
    pub model: String,
    /// Empty = no `Authorization` header.
    pub api_key: String,
    /// Model calls per run; **0 means no limit**.
    pub max_steps: u8,
    /// Default 1.0: this board asks for tool calls, not prose.
    pub temperature: f32,
    /// Default 45. Used by the transport, not by `chat`.
    pub timeout_s: u32,
    /// How hard the model should think: `off`, `low`, `medium`, `high`, or empty for
    /// "whatever the provider does by default". Sent as `reasoning_effort`, and only when
    /// set — a provider that does not know the field rejects the whole request, and the
    /// board cannot tell which ones know it.
    pub reasoning: String,
    /// Whether image attachments may be sent as content parts. The board cannot know
    /// that from the model name, so it is a setting: on for a multimodal model, off for
    /// a text-only one, which would reject the request outright.
    pub vision: bool,
    /// Which wire protocol the endpoint speaks: `chat` (OpenAI chat completions, the
    /// default), `responses` (OpenAI's Responses API) or `anthropic` (Messages API).
    /// Same routes as the host agent's three adapters — the shapes differ enough that a
    /// request built for one is a 400 on the others.
    pub protocol: String,
    /// Context window the transcript is allowed to occupy, in tokens.
    ///
    /// This is what decides when the board compacts: the trigger is 80% of it, exactly
    /// as the host agent's `trigger_tokens` does, and the reserve left over is what keeps
    /// room for the answer. Default 262144 (256K), the window the models this board is
    /// pointed at declare.
    ///
    /// Note the interaction with [`MAX_HISTORY_BYTES`]: the board has kilobytes of RAM,
    /// not megabytes, so the transcript is also capped at 24 KB of text. On a 256K window
    /// that byte ceiling binds first and the token trigger may never fire — which is the
    /// honest outcome, because a request large enough to need that window cannot be built
    /// on this chip anyway.
    pub context_tokens: u32,
    /// Ceiling on the reply, in tokens; sent as `max_tokens` on all three protocols.
    /// Also part of the compaction reserve.
    ///
    /// The board still truncates a reply at [`MAX_REPLY_BYTES`] (8 KB) to keep it out of
    /// the portal's way, so a 32K ceiling is a provider-side limit rather than a promise
    /// of a longer answer here.
    pub max_output_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            max_steps: 0,
            temperature: 1.0,
            timeout_s: 45,
            reasoning: String::new(),
            vision: true,
            protocol: "chat".into(),
            context_tokens: 262_144,
            max_output_tokens: 32_768,
        }
    }
}

/// One tool the model asked for. `arguments` is already a JSON value, or `{}`
/// carrying [`ARGS_ERROR_KEY`] when the model's argument string was unusable.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A model turn: text, tool calls, or both.
#[derive(Clone, Debug, Default)]
pub struct ModelReply {
    /// The assistant's visible answer. Empty on a pure tool-call turn.
    pub content: String,
    /// One line saying the request behind this reply was retried, or empty when it was
    /// answered on the first attempt.
    ///
    /// One note per reply, not one event per attempt: a client reading the journal
    /// should see "this answer took two tries", never three failures and one success.
    /// The agent turns it into a single `retried` entry. Attempts that run out never
    /// reach a reply; they are reported as one `error` instead.
    pub retry_note: String,
    /// The model's reasoning, when the provider exposes it (DeepSeek sends
    /// `reasoning_content`, some gateways send `reasoning`). It is kept as its own
    /// block rather than folded into `content`, and echoed back on the next request
    /// because a reasoner rejects the turn without it.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// Token accounting for one model call, the way the host agent reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl Usage {
    pub fn total(&self) -> u32 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }
}

/// Attempts one model request may make, the first included.
///
/// Three, and not more: the failures worth retrying (a refused connection, a 5xx, a body
/// that arrived short) are usually gone by the second try, while the board has a single
/// agent thread and a turn with a token budget of its own — a fourth copy of the same
/// request mostly buys a user staring at an empty console. Two retries also bound a
/// request at three times one attempt's cost, which [`RETRY_DEADLINE_MS`] caps in wall
/// time from the other side.
pub const MAX_ATTEMPTS: u8 = 3;

/// First wait between attempts, doubled per retry: 250 ms, then 500 ms.
pub const BASE_DELAY_MS: u64 = 250;

/// Ceiling on a single wait, so a longer ladder cannot park the agent thread.
pub const MAX_DELAY_MS: u64 = 1_000;

/// Hard ceiling on one model request: every attempt and every wait inside it.
///
/// Without it a retry silently doubles a turn's latency, because one attempt already
/// spends up to the transport's `timeout_s` (45 s by default). Once this much wall time
/// has gone the request is abandoned whether or not it looks retryable, and the error
/// says the budget ran out instead of blaming the model. 20 s is about how long a person
/// waits at a console before deciding the board is wedged.
pub const RETRY_DEADLINE_MS: u64 = 20_000;

/// How hard `chat` retries one request.
///
/// `Default` is the constants above. Tests — and a platform whose socket timeout is
/// configured very differently from the default 45 s — pass their own.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Attempts in total, the first included. `1` disables retrying.
    pub max_attempts: u8,
    /// Wait after the first failure, doubled per further failure.
    pub base_delay_ms: u64,
    /// Ceiling on one wait.
    pub max_delay_ms: u64,
    /// Ceiling on the whole request, waits included.
    pub deadline_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            base_delay_ms: BASE_DELAY_MS,
            max_delay_ms: MAX_DELAY_MS,
            deadline_ms: RETRY_DEADLINE_MS,
        }
    }
}

impl RetryPolicy {
    /// The wait after `failed` unsuccessful attempts: the base doubled per retry, capped.
    pub fn delay_after(&self, failed: u8) -> u64 {
        let shift = failed.saturating_sub(1).min(8) as u32;
        self.base_delay_ms.saturating_mul(1u64 << shift).min(self.max_delay_ms)
    }
}

/// The clock and the wait one request needs.
///
/// Injected for two reasons: a test has to watch the backoff without sleeping, and the
/// board's own clock is not the host's. Only differences of [`now_ms`](Self::now_ms)
/// mean anything.
pub trait RetryTimer {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64);
}

/// The real clock: a monotonic `Instant`, and a real sleep.
pub struct WallClock {
    origin: std::time::Instant,
}

impl WallClock {
    pub fn new() -> Self {
        Self { origin: std::time::Instant::now() }
    }
}

impl Default for WallClock {
    fn default() -> Self {
        Self::new()
    }
}

impl RetryTimer for WallClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// One request/response round trip. `messages` and `tools` are already serialised
/// by the caller as OpenAI `messages` / `tools` arrays.
///
/// A failure that another copy of the request could survive is retried under
/// [`RetryPolicy::default`]; [`chat_with`] takes the policy and the clock instead.
pub fn chat(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
) -> Result<ModelReply, String> {
    chat_with(transport, cfg, messages, tools, &RetryPolicy::default(), &WallClock::new())
}

/// [`chat`] with the retry policy and the clock spelled out.
///
/// One attempt is one HTTP request, and nothing else is repeated: a tool that already
/// ran stays run, because the second attempt sends the same transcript — tool results
/// included — rather than re-executing anything.
pub fn chat_with(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
    policy: &RetryPolicy,
    timer: &dyn RetryTimer,
) -> Result<ModelReply, String> {
    let url = cfg.base_url.trim();
    // Decided before the first byte leaves: a wrong scheme or a missing model name is the
    // caller's mistake, so it costs no attempt and no wait.
    check_url(url)?;
    if cfg.model.trim().is_empty() {
        return Err("llm: model is empty; set one in the agent config".to_string());
    }

    let max_attempts = policy.max_attempts.max(1);
    let started = timer.now_ms();
    let mut attempt: u8 = 0;
    let mut last = String::new();
    let mut waits = String::new();
    loop {
        attempt = attempt.saturating_add(1);
        match send_once(transport, cfg, messages, tools, url) {
            Ok(mut reply) => {
                if attempt > 1 {
                    reply.retry_note = format!(
                        "the model request was retried {} time{} (waited {waits} ms); the last \
                         failure was {last}",
                        attempt - 1,
                        if attempt == 2 { "" } else { "s" }
                    );
                }
                return Ok(reply);
            }
            Err(e) => last = e,
        }

        // A second copy of the same request cannot fix a rejected key or a body that
        // arrived and did not parse; it only spends the user's credits again.
        if !retryable_failure(&last) {
            return Err(last);
        }
        if attempt >= max_attempts {
            return Err(format!("{last} (gave up after {attempt} attempts)"));
        }
        let delay = policy.delay_after(attempt);
        let spent = timer.now_ms().saturating_sub(started);
        if spent.saturating_add(delay) > policy.deadline_ms {
            return Err(format!(
                "{last} (gave up after {} attempt{}: {spent} ms of the {} ms retry deadline was \
                 already spent)",
                attempt,
                if attempt == 1 { "" } else { "s" },
                policy.deadline_ms
            ));
        }
        timer.sleep_ms(delay);
        if !waits.is_empty() {
            waits.push_str(", ");
        }
        waits.push_str(&delay.to_string());
    }
}

/// One request, in the dialect the configuration names.
fn send_once(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
    url: &str,
) -> Result<ModelReply, String> {
    match Protocol::parse(&cfg.protocol) {
        Protocol::Chat => chat_completions(transport, cfg, messages, tools, url),
        Protocol::Responses => responses(transport, cfg, messages, tools, url),
        Protocol::Anthropic => anthropic(transport, cfg, messages, tools, url),
    }
}

/// Whether another copy of the same request could succeed where this one failed.
///
/// The transport speaks `String`, so this reads the wording [`http_error`] and the two
/// transports already produce — pinned by
/// `the_retry_decision_matches_what_the_transports_report`. Two families are worth
/// another try:
///
/// * a status: 5xx is the endpoint's problem and usually transient, and a missing status
///   line (`HTTP 0`) means the reply was not HTTP at all, which is what a half-closed
///   connection looks like. 401/403/400/404/422 are the caller's mistake: the same
///   request is the same refusal, and the copy costs credits for nothing. 429 asks for a
///   wait longer than [`RETRY_DEADLINE_MS`] allows, so a sub-second retry only adds noise;
/// * a transport-level failure: refused, reset, timed out, or a connection closed in the
///   middle of the body. A 200 whose body did not parse is *not* retried — that body is
///   the provider's answer to this exact request, and asking again is a second charge for
///   the same answer.
pub fn retryable_failure(err: &str) -> bool {
    if let Some(status) = http_status(err) {
        return status == 0 || (500..=599).contains(&status);
    }
    // Everything `chat` itself can complain about returned before the first attempt; what
    // is left under this prefix is the transport's own report.
    let Some(detail) =
        err.strip_prefix("llm: POST ").and_then(|rest| rest.split_once(" failed: ")).map(|(_, d)| d)
    else {
        return false;
    };
    let detail = detail.to_ascii_lowercase();
    const MARKERS: [&str; 10] = [
        "connection refused",
        "connecting to",
        "connect ",
        "closed the connection mid-reply",
        "the reply ended after",
        "did not answer within",
        "timed out",
        "connection reset",
        "broken pipe",
        "read failed",
    ];
    MARKERS.iter().any(|m| detail.contains(m))
}

/// The status in an error text built by [`http_error`], if it carries one.
fn http_status(err: &str) -> Option<u16> {
    let start = err.find("HTTP ")? + "HTTP ".len();
    let digits: String = err[start..].chars().take_while(char::is_ascii_digit).take(3).collect();
    digits.parse().ok()
}

/// The three wire dialects. Named the way the host agent names them; a value that is
/// not recognised falls back to the OpenAI chat shape, which is what every
/// OpenAI-compatible gateway speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

impl Protocol {
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "responses" | "openai-responses" => Self::Responses,
            "anthropic" | "anthropic-messages" => Self::Anthropic,
            _ => Self::Chat,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
        }
    }
}

fn chat_completions(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
    url: &str,
) -> Result<ModelReply, String> {
    // A NaN/inf temperature serialises to `null`, which some servers reject.
    let temperature =
        if cfg.temperature.is_finite() { cfg.temperature } else { LlmConfig::default().temperature };
    let mut body = json!({
        "model": cfg.model.trim(),
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "temperature": temperature,
        "max_tokens": cfg.max_output_tokens.max(1),
    });
    // Only when asked: `reasoning_effort` is an OpenAI extension, and an endpoint that
    // does not define it answers 400 rather than ignoring it.
    let reasoning = cfg.reasoning.trim();
    if !reasoning.is_empty() {
        body["reasoning_effort"] = json!(reasoning);
    }
    let body = body.to_string();

    let auth = format!("Bearer {}", cfg.api_key.trim());
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    if !cfg.api_key.trim().is_empty() {
        headers.push(("Authorization", &auth));
    }

    let response =
        transport.post_json(url, &headers, &body).map_err(|e| format!("llm: POST {url} failed: {e}"))?;
    parse_reply(&response)
}

/// OpenAI's Responses API.
///
/// The differences that matter: `input` replaces `messages`, tools are flat objects
/// (`{type:"function", name, …}` rather than `{type:"function", function:{…}}`), a tool
/// call comes back as its own output item rather than inside a message, and the tool
/// result goes back as a `function_call_output` item.
fn responses(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
    url: &str,
) -> Result<ModelReply, String> {
    let input = responses_input(messages);
    let mut body = json!({
        "model": cfg.model.trim(),
        "input": input,
        "max_output_tokens": cfg.max_output_tokens.max(1),
    });
    let tool_list = responses_tools(tools);
    if !tool_list.is_empty() {
        body["tools"] = Value::Array(tool_list);
    }
    let reasoning = cfg.reasoning.trim();
    if !reasoning.is_empty() {
        body["reasoning"] = json!({ "effort": reasoning });
    }

    let auth = format!("Bearer {}", cfg.api_key.trim());
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    if !cfg.api_key.trim().is_empty() {
        headers.push(("Authorization", &auth));
    }
    let response = transport
        .post_json(url, &headers, &body.to_string())
        .map_err(|e| format!("llm: POST {url} failed: {e}"))?;
    parse_responses_reply(&response)
}

/// `messages` (chat shape) to the Responses `input` array.
fn responses_input(messages: &Value) -> Value {
    let mut out = Vec::new();
    for message in messages.as_array().into_iter().flatten() {
        let role = message["role"].as_str().unwrap_or("user");
        match role {
            "tool" => out.push(json!({
                "type": "function_call_output",
                "call_id": message["tool_call_id"].as_str().unwrap_or_default(),
                "output": message["content"].as_str().unwrap_or_default(),
            })),
            "assistant" => {
                let text = message["content"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    out.push(json!({ "role": "assistant", "content": text }));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let f = &call["function"];
                    out.push(json!({
                        "type": "function_call",
                        "call_id": call["id"].as_str().unwrap_or_default(),
                        "name": f["name"].as_str().unwrap_or_default(),
                        "arguments": f["arguments"].as_str().unwrap_or_default(),
                    }));
                }
            }
            "system" => out.push(json!({ "role": "system", "content": message["content"] })),
            _ => out.push(json!({ "role": "user", "content": message["content"] })),
        }
    }
    Value::Array(out)
}

fn responses_tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = &t["function"];
            let name = f["name"].as_str()?;
            Some(json!({
                "type": "function",
                "name": name,
                "description": f["description"],
                "parameters": f["parameters"],
            }))
        })
        .collect()
}

fn parse_responses_reply(body: &str) -> Result<ModelReply, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("llm: reply is not JSON ({e}): {}", body_prefix(body, 200)))?;
    // An error object comes back with a 200 on some gateways.
    if let Some(err) = value.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        return Err(format!("llm: {err}"));
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in value["output"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if let Some(t) = part["text"].as_str() {
                        text.push_str(t);
                    }
                }
            }
            "function_call" => {
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                calls.push(ToolCall {
                    id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    arguments: serde_json::from_str(arguments).unwrap_or_else(|_| json!({})),
                });
            }
            _ => {}
        }
    }
    let usage = Usage {
        prompt_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
    };
    if text.is_empty() && calls.is_empty() {
        return Err(format!("llm: no output items in reply: {}", body_prefix(body, 200)));
    }
    Ok(ModelReply {
        content: text,
        reasoning: String::new(),
        tool_calls: calls,
        usage,
        retry_note: String::new(),
    })
}

/// Anthropic's Messages API.
///
/// The differences that matter: the system prompt is a field, not a message; a tool
/// result is a `tool_result` block inside a **user** turn; tools carry `input_schema`
/// rather than `parameters`; the reply is content blocks, not one message.
fn anthropic(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
    url: &str,
) -> Result<ModelReply, String> {
    let (system, lowered) = anthropic_messages(messages);
    let mut body = json!({
        "model": cfg.model.trim(),
        "max_tokens": cfg.max_output_tokens.max(1),
        "messages": lowered,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    let tool_list = anthropic_tools(tools);
    if !tool_list.is_empty() {
        body["tools"] = Value::Array(tool_list);
    }
    if (cfg.temperature - 1.0).abs() > f32::EPSILON && cfg.temperature.is_finite() {
        body["temperature"] = json!(cfg.temperature);
    }

    // Anthropic authenticates with a header of its own; a bearer token there is a 401.
    let mut headers: Vec<(&str, &str)> =
        vec![("Content-Type", "application/json"), ("anthropic-version", ANTHROPIC_VERSION)];
    if !cfg.api_key.trim().is_empty() {
        headers.push(("x-api-key", cfg.api_key.trim()));
    }
    let response = transport
        .post_json(url, &headers, &body.to_string())
        .map_err(|e| format!("llm: POST {url} failed: {e}"))?;
    parse_anthropic_reply(&response)
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

fn anthropic_messages(messages: &Value) -> (String, Value) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    for message in messages.as_array().into_iter().flatten() {
        let role = message["role"].as_str().unwrap_or("user");
        match role {
            "system" => {
                if let Some(text) = message["content"].as_str() {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(text);
                }
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": message["tool_call_id"].as_str().unwrap_or_default(),
                    "content": message["content"].as_str().unwrap_or_default(),
                });
                // Consecutive tool results belong in one user turn.
                match out.last_mut() {
                    Some(last) if last["role"] == "user" => {
                        if let Some(blocks) = last["content"].as_array_mut() {
                            blocks.push(block);
                        }
                    }
                    _ => out.push(json!({ "role": "user", "content": [block] })),
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = message["content"].as_str() {
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let f = &call["function"];
                    let input = f["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"].as_str().unwrap_or_default(),
                        "name": f["name"].as_str().unwrap_or_default(),
                        "input": input,
                    }));
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            _ => {
                let content = match &message["content"] {
                    Value::String(text) => json!([{ "type": "text", "text": text }]),
                    Value::Array(parts) => Value::Array(parts.iter().filter_map(anthropic_part).collect()),
                    other => json!([{ "type": "text", "text": other.to_string() }]),
                };
                out.push(json!({ "role": "user", "content": content }));
            }
        }
    }
    (system, Value::Array(out))
}

/// An OpenAI content part, in Anthropic's shape.
fn anthropic_part(part: &Value) -> Option<Value> {
    match part["type"].as_str()? {
        "text" => Some(json!({ "type": "text", "text": part["text"] })),
        "image_url" => {
            let url = part["image_url"]["url"].as_str()?;
            let rest = url.strip_prefix("data:")?;
            let (media, data) = rest.split_once(";base64,")?;
            Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media, "data": data },
            }))
        }
        _ => None,
    }
}

fn anthropic_tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = &t["function"];
            Some(json!({
                "name": f["name"].as_str()?,
                "description": f["description"],
                "input_schema": f["parameters"],
            }))
        })
        .collect()
}

fn parse_anthropic_reply(body: &str) -> Result<ModelReply, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("llm: reply is not JSON ({e}): {}", body_prefix(body, 200)))?;
    if let Some(err) = value.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        return Err(format!("llm: {err}"));
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for block in value["content"].as_array().into_iter().flatten() {
        match block["type"].as_str().unwrap_or("") {
            "text" => text.push_str(block["text"].as_str().unwrap_or("")),
            // Extended thinking arrives as its own block type.
            "thinking" => reasoning.push_str(block["thinking"].as_str().unwrap_or("")),
            "tool_use" => calls.push(ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: block["name"].as_str().unwrap_or_default().to_string(),
                arguments: block["input"].clone(),
            }),
            _ => {}
        }
    }
    let usage = Usage {
        prompt_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
    };
    if text.is_empty() && calls.is_empty() {
        return Err(format!("llm: no content blocks in reply: {}", body_prefix(body, 200)));
    }
    Ok(ModelReply { content: text, reasoning, tool_calls: calls, usage, retry_note: String::new() })
}

/// Split a reply body into the fields the loop needs.
fn parse_reply(body: &str) -> Result<ModelReply, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("llm: reply is not JSON ({e}): {}", body_prefix(body, 200)))?;
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| format!("llm: no choices[0].message in reply: {}", body_prefix(body, 200)))?;

    let content = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        // `null` is a legal content for a pure tool-call turn.
        Some(Value::Null) | None => String::new(),
        // Some servers send content parts; keep the JSON rather than dropping it.
        Some(other) => other.to_string(),
    };

    // Reasoning is a first-class block in the host agent's transcript; here it is the
    // provider field the wire actually carries. DeepSeek uses `reasoning_content`,
    // several gateways use `reasoning`, and an empty string means "none given".
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let usage = value.get("usage").map(|u| Usage {
        prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        completion_tokens: u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
    });
    let usage = usage.unwrap_or_default();

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let function = call.get("function");
            let name =
                function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default().to_string();
            let arguments = match function.and_then(|f| f.get("arguments")) {
                // The documented wire format: a JSON *string*.
                Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
                    Ok(Value::Object(map)) => Value::Object(map),
                    Ok(other) => {
                        return Err(format!(
                            "llm: tool call {name}: arguments parsed to {}, expected an object",
                            json_kind(&other)
                        ))
                    }
                    Err(e) => {
                        json!({ ARGS_ERROR_KEY: format!("arguments are not JSON ({e}): {}", body_prefix(raw, 120)) })
                    }
                },
                // Tolerated: a server that already decoded the object for us.
                Some(value @ Value::Object(_)) => value.clone(),
                Some(other) => {
                    json!({ ARGS_ERROR_KEY: format!("arguments have type {}, expected a JSON string", json_kind(other)) })
                }
                None => json!({}),
            };
            tool_calls.push(ToolCall { id, name, arguments });
        }
    }

    Ok(ModelReply { content, reasoning, tool_calls, usage, retry_note: String::new() })
}

/// The argument-parse error of `tc`, if `chat` degraded its arguments.
pub fn arguments_error(tc: &ToolCall) -> Option<&str> {
    tc.arguments.get(ARGS_ERROR_KEY).and_then(Value::as_str)
}

/// `arguments` as it must be echoed in an OpenAI `tool_calls` entry: a compact
/// JSON string, with a degraded argument object scrubbed back to `{}`.
pub fn arguments_request_json(tc: &ToolCall) -> String {
    if arguments_error(tc).is_some() {
        "{}".to_string()
    } else {
        tc.arguments.to_string()
    }
}

/// `Err` text for a non-2xx response: status plus the first 200 characters of
/// the body, on one line so it survives the log ring.
pub fn http_error(status: u16, body: &str) -> String {
    format!("HTTP {status}: {}", body_prefix(body, 200))
}

/// First `max_chars` characters of `body`, control characters folded to spaces.
///
/// Bodies are read line-by-line by humans in the portal and the serial log; a
/// multi-line HTML error page would otherwise flood both.
pub fn body_prefix(body: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(body.len()) + 1);
    let mut space_pending = false;
    for (index, ch) in body.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch == ' ' {
            if space_pending {
                continue;
            }
            space_pending = true;
        } else {
            space_pending = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// Cut `text` to at most `max` bytes, on a character boundary, with
/// [`TRUNCATED_MARK`] appended so the model and the reader can tell.
///
/// The result can exceed `max` by the length of the marker; the point is the
/// bound, not an exact count.
pub fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + TRUNCATED_MARK.len());
    out.push_str(&text[..end]);
    out.push_str(TRUNCATED_MARK);
    out
}

fn check_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if lower.is_empty() {
        return Err("llm: base_url is empty; set the chat endpoint in the agent config".to_string());
    }
    // Both schemes are allowed: `https://` is carried by the platform transport (TLS
    // on the board via esp-tls, on the host via rustls), so there is nothing to refuse
    // here. What is still refused is anything that is not HTTP at all — that would
    // otherwise surface as an obscure socket error.
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(format!(
            "llm: base_url must start with http:// or https://, got {}",
            body_prefix(url, 60)
        ));
    }
    Ok(())
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records the request and answers with a canned body.
    struct Stub {
        reply: String,
        seen: RefCell<Vec<(String, String)>>,
    }

    impl ChatTransport for Stub {
        fn post_json(&mut self, url: &str, _headers: &[(&str, &str)], body: &str) -> Result<String, String> {
            self.seen.borrow_mut().push((url.to_string(), body.to_string()));
            Ok(self.reply.clone())
        }
    }

    fn wire_messages() -> Value {
        json!([
            {"role": "system", "content": "you are a board"},
            {"role": "user", "content": "read the screen"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "oled_read", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "{\"text\":\"HI\"}"}
        ])
    }

    fn wire_tools() -> Value {
        json!([{"type": "function", "function": {
            "name": "oled_read", "description": "read it", "parameters": {"type": "object"}
        }}])
    }

    fn proto_cfg(protocol: &str) -> LlmConfig {
        LlmConfig {
            base_url: "https://example.test/v1/messages".into(),
            model: "m".into(),
            api_key: "k".into(),
            protocol: protocol.into(),
            ..LlmConfig::default()
        }
    }

    #[test]
    fn the_anthropic_request_uses_its_own_shape() {
        let mut stub = Stub {
            reply: json!({
                "content": [
                    {"type": "thinking", "thinking": "look first"},
                    {"type": "text", "text": "it says HI"},
                    {"type": "tool_use", "id": "t1", "name": "oled_read", "input": {"x": 1}}
                ],
                "usage": {"input_tokens": 11, "output_tokens": 7}
            })
            .to_string(),
            seen: RefCell::new(Vec::new()),
        };
        let reply = chat(&mut stub, &proto_cfg("anthropic"), &wire_messages(), &wire_tools()).expect("reply");
        assert_eq!(reply.content, "it says HI");
        assert_eq!(reply.reasoning, "look first");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].arguments, json!({"x": 1}));
        assert_eq!(reply.usage.prompt_tokens, 11);

        let body: Value = serde_json::from_str(&stub.seen.borrow()[0].1).expect("json");
        assert_eq!(body["system"], "you are a board", "the system prompt is a field, not a message");
        let msgs = body["messages"].as_array().expect("messages");
        // user, assistant(tool_use), user(tool_result) — no system message in the array.
        assert_eq!(msgs.len(), 3, "{body}");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        assert!(body["max_tokens"].is_number(), "the Messages API requires it");
    }

    #[test]
    fn the_responses_request_uses_its_own_shape() {
        let mut stub = Stub {
            reply: json!({
                "output": [
                    {"type": "message", "content": [{"type": "output_text", "text": "done"}]},
                    {"type": "function_call", "call_id": "c9", "name": "oled_read", "arguments": "{}"}
                ],
                "usage": {"input_tokens": 3, "output_tokens": 2}
            })
            .to_string(),
            seen: RefCell::new(Vec::new()),
        };
        let reply = chat(&mut stub, &proto_cfg("responses"), &wire_messages(), &wire_tools()).expect("reply");
        assert_eq!(reply.content, "done");
        assert_eq!(reply.tool_calls[0].id, "c9");
        assert_eq!(reply.usage.prompt_tokens, 3);

        let body: Value = serde_json::from_str(&stub.seen.borrow()[0].1).expect("json");
        let input = body["input"].as_array().expect("input");
        assert_eq!(input[0]["role"], "system");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "oled_read", "flat tool objects, not nested");
    }

    #[test]
    fn the_default_protocol_is_still_chat_completions() {
        assert_eq!(Protocol::parse(""), Protocol::Chat);
        assert_eq!(Protocol::parse("nonsense"), Protocol::Chat);
        assert_eq!(Protocol::parse("anthropic").as_str(), "anthropic");
    }

    /// A build must never carry somebody's model endpoint, and never their key.
    ///
    /// The owner configures `base_url`/`model`/`api_key` at the portal; the firmware
    /// ships with all three empty, so nothing leaves the board until a person decides
    /// where it should go. This is also the guard against a convenient "just point it at
    /// the endpoint I already have" — that endpoint and its credential belong to whoever
    /// runs the board, not to the assistant or maintainer building it.
    #[test]
    fn the_firmware_ships_with_no_endpoint_and_no_key() {
        let cfg = LlmConfig::default();
        assert!(cfg.base_url.is_empty(), "a build must not carry a model endpoint: {}", cfg.base_url);
        assert!(cfg.model.is_empty(), "a build must not carry a model name: {}", cfg.model);
        assert!(cfg.api_key.is_empty(), "a build must not carry an api key");
    }

    struct StubTransport {
        urls: Vec<String>,
        headers: Vec<Vec<(String, String)>>,
        bodies: Vec<String>,
        reply: Result<String, String>,
    }

    impl Default for StubTransport {
        fn default() -> Self {
            Self { urls: Vec::new(), headers: Vec::new(), bodies: Vec::new(), reply: Ok(String::new()) }
        }
    }

    impl ChatTransport for StubTransport {
        fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
            self.urls.push(url.to_string());
            self.headers.push(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
            self.bodies.push(body.to_string());
            self.reply.clone()
        }
    }

    impl StubTransport {
        fn answering(body: String) -> Self {
            Self { reply: Ok(body), ..Self::default() }
        }

        fn failing(status: u16, body: &str) -> Self {
            Self { reply: Err(http_error(status, body)), ..Self::default() }
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers.first()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    fn cfg() -> LlmConfig {
        LlmConfig {
            base_url: "http://127.0.0.1:8899/v1/chat/completions".to_string(),
            model: "stub".to_string(),
            api_key: "sekret".to_string(),
            ..LlmConfig::default()
        }
    }

    fn messages() -> Value {
        json!([{ "role": "user", "content": "hi" }])
    }

    fn tools() -> Value {
        json!([{ "type": "function", "function": { "name": "board_status", "parameters": { "type": "object" } } }])
    }

    fn reply_body(message: Value) -> String {
        json!({ "choices": [{ "message": message }] }).to_string()
    }

    #[test]
    fn config_defaults_match_the_brief() {
        let cfg = LlmConfig::default();
        // No step cap by default: a fixed one truncated work that would have finished.
        assert_eq!(cfg.max_steps, 0);
        assert!((cfg.temperature - 1.0).abs() < f32::EPSILON);
        assert_eq!(cfg.timeout_s, 45);
        assert!(cfg.api_key.is_empty());
    }

    #[test]
    fn builds_the_openai_request_and_sends_the_bearer_token() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "hello" })));
        let reply = chat(&mut t, &cfg(), &messages(), &tools()).expect("reply");
        assert_eq!(reply.content, "hello");
        assert!(reply.tool_calls.is_empty());

        assert_eq!(t.urls, vec![cfg().base_url.clone()]);
        assert_eq!(t.header("Content-Type"), Some("application/json"));
        assert_eq!(t.header("Authorization"), Some("Bearer sekret"));

        let body: Value = serde_json::from_str(&t.bodies[0]).expect("request body is JSON");
        assert_eq!(body["model"], "stub");
        assert_eq!(body["tool_choice"], "auto");
        let temperature = body["temperature"].as_f64().expect("a number");
        assert!((temperature - 1.0).abs() < 1e-6, "temperature is {temperature}");
        assert_eq!(body["messages"], messages());
        assert_eq!(body["tools"], tools());
    }

    #[test]
    fn an_empty_key_sends_no_authorization_header() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let mut c = cfg();
        c.api_key = String::new();
        chat(&mut t, &c, &wire_messages(), &wire_tools()).expect("reply");
        assert_eq!(t.header("Authorization"), None);
    }

    #[test]
    fn content_may_be_null_and_arguments_arrive_as_a_string() {
        let mut t = StubTransport::answering(reply_body(json!({
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "board_status", "arguments": "{\"detail\":true}" }
            }]
        })));
        let reply = chat(&mut t, &cfg(), &wire_messages(), &wire_tools()).expect("reply");
        assert_eq!(reply.content, "");
        assert_eq!(reply.tool_calls.len(), 1);
        let call = &reply.tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "board_status");
        assert_eq!(call.arguments, json!({ "detail": true }));
        assert_eq!(arguments_error(call), None);
        assert_eq!(arguments_request_json(call), "{\"detail\":true}");
    }

    #[test]
    fn unparsable_arguments_degrade_to_an_empty_object_plus_an_error() {
        let mut t = StubTransport::answering(reply_body(json!({
            "content": null,
            "tool_calls": [{
                "id": "call_bad",
                "function": { "name": "fs_write", "arguments": "{\"path\": \"/a.txt\"" }
            }]
        })));
        // No panic, and nothing is invented: the call survives with `{}`.
        let reply = chat(&mut t, &cfg(), &wire_messages(), &wire_tools()).expect("reply");
        let call = &reply.tool_calls[0];
        assert_eq!(call.arguments.get("path"), None);
        let err = arguments_error(call).expect("a degraded call carries its error");
        assert!(err.contains("not JSON"), "unexpected error text: {err}");
        // Echoed back to the model as `{}`, never as our private error key.
        assert_eq!(arguments_request_json(call), "{}");
    }

    #[test]
    fn a_non_2xx_from_the_transport_keeps_the_status_and_a_short_body() {
        let long = "upstream exploded ".repeat(60);
        let mut t = StubTransport::failing(503, &long);
        // On the injected clock: a 503 is retryable, so the real one would spend three
        // quarters of a second asleep to prove a point about error text.
        let clock = FakeClock::default();
        let err = chat_with(&mut t, &cfg(), &wire_messages(), &wire_tools(), &RetryPolicy::default(), &clock)
            .expect_err("must fail");
        assert!(err.contains("HTTP 503"), "unexpected error text: {err}");
        assert!(err.chars().count() <= 400, "error text is not bounded: {} chars", err.chars().count());
        assert!(!err.contains('\n'), "error text spans lines: {err:?}");
    }

    // ---------------------------------------------------------------- retry policy

    /// A clock that does not sleep: every wait is recorded and moves time forward, so a
    /// test can assert the backoff and the deadline without spending wall time.
    #[derive(Default)]
    struct FakeClock {
        now: std::cell::Cell<u64>,
        sleeps: std::cell::RefCell<Vec<u64>>,
    }

    impl FakeClock {
        fn advance(&self, ms: u64) {
            self.now.set(self.now.get().saturating_add(ms));
        }

        fn slept(&self) -> Vec<u64> {
            self.sleeps.borrow().clone()
        }

        fn total_slept(&self) -> u64 {
            self.sleeps.borrow().iter().sum()
        }
    }

    impl RetryTimer for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }

        fn sleep_ms(&self, ms: u64) {
            self.sleeps.borrow_mut().push(ms);
            self.advance(ms);
        }
    }

    /// Fails `failures` times with `status`, then answers. Every attempt costs `cost_ms`
    /// on the fake clock, which is how a test says "the socket timeout ate the budget".
    struct Flaky<'a> {
        failures: usize,
        status: u16,
        cost_ms: u64,
        calls: usize,
        clock: &'a FakeClock,
        reply: String,
    }

    impl<'a> Flaky<'a> {
        fn new(failures: usize, status: u16, cost_ms: u64, clock: &'a FakeClock, reply: String) -> Self {
            Self { failures, status, cost_ms, calls: 0, clock, reply }
        }
    }

    impl ChatTransport for Flaky<'_> {
        fn post_json(
            &mut self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &str,
        ) -> Result<String, String> {
            self.calls += 1;
            self.clock.advance(self.cost_ms);
            if self.calls <= self.failures {
                return Err(http_error(self.status, "the model is restarting"));
            }
            Ok(self.reply.clone())
        }
    }

    #[test]
    fn a_request_that_fails_twice_is_retried_once_more_and_answers_with_one_note() {
        let clock = FakeClock::default();
        let mut t = Flaky::new(2, 503, 0, &clock, reply_body(json!({ "content": "hello" })));
        let reply = chat_with(&mut t, &cfg(), &messages(), &tools(), &RetryPolicy::default(), &clock)
            .expect("the third attempt answers");

        assert_eq!(reply.content, "hello");
        assert_eq!(t.calls, 3);
        assert_eq!(clock.slept(), vec![BASE_DELAY_MS, BASE_DELAY_MS * 2], "the wait doubles per retry");
        assert!(reply.retry_note.contains("retried 2 times"), "{}", reply.retry_note);
        assert_eq!(
            reply.retry_note.matches("250, 500").count(),
            1,
            "the waits are named in ms, in the order they happened: {}",
            reply.retry_note
        );
        assert!(reply.retry_note.contains("HTTP 503"), "the last failure survives: {}", reply.retry_note);
        assert!(reply.retry_note.chars().count() <= 512, "the note stays inside a journal entry");
    }

    #[test]
    fn a_first_try_success_carries_no_note() {
        let clock = FakeClock::default();
        let mut t = Flaky::new(0, 503, 0, &clock, reply_body(json!({ "content": "hello" })));
        let reply =
            chat_with(&mut t, &cfg(), &messages(), &tools(), &RetryPolicy::default(), &clock).expect("reply");
        assert!(reply.retry_note.is_empty());
        assert!(clock.slept().is_empty());
    }

    /// A rejected key, a wrong model name or a malformed body is the caller's mistake:
    /// the same request is the same refusal, and every copy spends credits for nothing.
    #[test]
    fn a_failure_the_request_caused_is_not_retried() {
        for status in [400u16, 401, 403, 404, 408, 422, 429] {
            let clock = FakeClock::default();
            let mut t = Flaky::new(usize::MAX, status, 0, &clock, String::new());
            let err = chat_with(&mut t, &cfg(), &messages(), &tools(), &RetryPolicy::default(), &clock)
                .expect_err("must fail");
            assert!(err.contains(&format!("HTTP {status}")), "{err}");
            assert_eq!(t.calls, 1, "HTTP {status} must not be retried");
            assert!(clock.slept().is_empty(), "HTTP {status} must not wait");
            assert!(!err.contains("attempts"), "a final failure names no attempts: {err}");
        }
    }

    #[test]
    fn exhausted_attempts_report_how_many_and_the_last_message() {
        let clock = FakeClock::default();
        let mut t = Flaky::new(usize::MAX, 503, 0, &clock, String::new());
        let err = chat_with(&mut t, &cfg(), &messages(), &tools(), &RetryPolicy::default(), &clock)
            .expect_err("three failures is a failure");

        assert_eq!(t.calls, MAX_ATTEMPTS as usize, "the ladder stops at the policy's count");
        assert!(err.contains("gave up after 3 attempts"), "{err}");
        assert!(err.contains("HTTP 503"), "the last message survives: {err}");
        assert!(err.contains("the model is restarting"), "{err}");
        assert!(err.chars().count() <= 400, "still bounded: {} chars", err.chars().count());
        assert_eq!(clock.slept(), vec![BASE_DELAY_MS, BASE_DELAY_MS * 2]);
    }

    /// A transport cannot be made to wait for seconds in a unit test, so the budget is
    /// spent through the waits: a deadline that cannot afford the second backoff stops
    /// the ladder before the attempt count does.
    #[test]
    fn the_deadline_stops_the_ladder_before_the_attempts_run_out() {
        let clock = FakeClock::default();
        let mut t = Flaky::new(usize::MAX, 503, 0, &clock, String::new());
        let policy = RetryPolicy { deadline_ms: 300, ..RetryPolicy::default() };
        let err = chat_with(&mut t, &cfg(), &messages(), &tools(), &policy, &clock).expect_err("must fail");

        assert_eq!(t.calls, 2, "the 500 ms wait would overrun a 300 ms deadline: {err}");
        assert!(err.contains("retry deadline"), "{err}");
        assert!(err.contains("300"), "the budget that ran out is named: {err}");
        assert!(clock.total_slept() <= policy.deadline_ms, "slept {} of 300 ms", clock.total_slept());
    }

    /// The case that matters on the board: one attempt that spent nearly the whole budget
    /// (a 45 s socket timeout is the default) leaves no room for a second, so the request
    /// is given up instead of quietly doubling the turn's latency.
    #[test]
    fn a_slow_first_attempt_that_used_the_budget_is_not_retried() {
        let clock = FakeClock::default();
        let mut t = Flaky::new(usize::MAX, 503, 19_900, &clock, String::new());
        let err = chat_with(&mut t, &cfg(), &messages(), &tools(), &RetryPolicy::default(), &clock)
            .expect_err("must fail");

        assert_eq!(t.calls, 1, "a retry here would double a 19.9 s turn: {err}");
        assert!(err.contains("retry deadline"), "{err}");
        assert!(err.contains("19900"), "the time already spent is reported: {err}");
        assert!(clock.slept().is_empty());
    }

    #[test]
    fn a_policy_of_one_attempt_does_not_retry_at_all() {
        let clock = FakeClock::default();
        let mut t = Flaky::new(usize::MAX, 503, 0, &clock, String::new());
        let policy = RetryPolicy { max_attempts: 1, ..RetryPolicy::default() };
        let err = chat_with(&mut t, &cfg(), &messages(), &tools(), &policy, &clock).expect_err("must fail");
        assert_eq!(t.calls, 1);
        assert!(err.contains("gave up after 1 attempts"), "{err}");
        assert!(clock.slept().is_empty());
    }

    /// The decision is made from the wording the two transports and `http_error` produce,
    /// because a `String` is the only channel the trait has. Every phrase below is one the
    /// firmware's transport or the simulator's actually emits.
    #[test]
    fn the_retry_decision_matches_what_the_transports_report() {
        for status in [500u16, 502, 503, 504, 0] {
            assert!(retryable_failure(&http_error(status, "")), "HTTP {status} is worth a retry");
        }
        for status in [400u16, 401, 403, 404, 408, 422, 429] {
            assert!(!retryable_failure(&http_error(status, "")), "HTTP {status} is not");
        }

        let wrapped = |detail: &str| format!("llm: POST http://h/v1/chat/completions failed: {detail}");
        for detail in [
            "connecting to 192.0.2.10:80 failed: connection refused",
            "connect 127.0.0.1:8899 failed: Connection refused (os error 10061)",
            "the endpoint did not answer within 45 s",
            "reading the reply failed: timed out",
            "the server closed the connection mid-reply",
            "the reply ended after 12 of 900 bytes",
            "read failed: An existing connection was forcibly closed by the remote host. (os error 10054)",
        ] {
            assert!(retryable_failure(&wrapped(detail)), "should retry: {detail}");
        }
        for detail in [
            "the TLS handshake with api.example.com failed: mbedtls error -0x2700",
            "TLS setup for api.example.com failed: invalid server name",
            "could not resolve `api.example.com`: ESP_ERR_ESP_TLS_CANNOT_RESOLVE_HOSTNAME",
        ] {
            assert!(!retryable_failure(&wrapped(detail)), "should not retry: {detail}");
        }

        // A 200 whose body did not parse arrived: that body is the answer to this exact
        // request, so asking again is a second charge for the same answer.
        assert!(!retryable_failure("llm: reply is not JSON (expected value at line 1): boom"));
        assert!(!retryable_failure("llm: no choices[0].message in reply: {\"error\":\"timeout\"}"));
        assert!(!retryable_failure("llm: model is empty; set one in the agent config"));
    }

    #[test]
    fn a_2xx_body_that_does_not_parse_is_not_resent() {
        let clock = FakeClock::default();
        let mut t = StubTransport::answering("not json at all".to_string());
        let err = chat_with(&mut t, &cfg(), &messages(), &tools(), &RetryPolicy::default(), &clock)
            .expect_err("must fail");
        assert!(err.contains("not JSON"), "{err}");
        assert_eq!(t.urls.len(), 1, "the body arrived, so there is nothing to ask again");
        assert!(clock.slept().is_empty());
    }

    #[test]
    fn an_https_endpoint_reaches_the_transport_untouched() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let mut c = cfg();
        c.base_url = "https://api.example.com/v1/chat/completions".to_string();
        chat(&mut t, &c, &wire_messages(), &wire_tools()).expect("https must be carried to the transport");
        assert_eq!(t.urls, vec!["https://api.example.com/v1/chat/completions".to_string()]);
    }

    #[test]
    fn a_scheme_that_is_not_http_is_refused_before_the_transport() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let mut c = cfg();
        c.base_url = "ftp://example.invalid/v1/chat/completions".to_string();
        let err = chat(&mut t, &c, &wire_messages(), &wire_tools()).expect_err("ftp must fail");
        assert!(err.contains("http"), "unexpected error text: {err}");
        assert!(t.urls.is_empty(), "the transport must not be called");
    }

    #[test]
    fn empty_config_is_a_clear_error() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let err = chat(&mut t, &LlmConfig::default(), &wire_messages(), &wire_tools())
            .expect_err("empty base_url must fail");
        assert!(err.contains("base_url"), "unexpected error text: {err}");

        let mut c = cfg();
        c.model = "  ".to_string();
        let err = chat(&mut t, &c, &wire_messages(), &wire_tools()).expect_err("empty model must fail");
        assert!(err.contains("model"), "unexpected error text: {err}");
        assert!(t.urls.is_empty());
    }

    #[test]
    fn a_reply_without_choices_reports_the_body_prefix() {
        let mut t = StubTransport::answering(json!({ "error": "no such model" }).to_string());
        let err = chat(&mut t, &cfg(), &wire_messages(), &wire_tools()).expect_err("must fail");
        assert!(err.contains("choices[0].message"), "unexpected: {err}");
        assert!(err.contains("no such model"), "unexpected: {err}");
    }

    #[test]
    fn http_error_and_body_prefix_bound_what_reaches_the_log() {
        let text = http_error(500, "line one\nline two\r\n");
        assert_eq!(text, "HTTP 500: line one line two");
        let long = http_error(404, &"a".repeat(500));
        assert_eq!(long.chars().filter(|c| *c == 'a').count(), 200);
        assert!(body_prefix("\u{0}\u{1}ok", 10).starts_with("ok"));
    }

    #[test]
    fn truncate_bytes_stays_on_a_character_boundary() {
        let text = "é".repeat(2000); // 4000 bytes
        let cut = truncate_bytes(&text, 2049);
        assert!(cut.ends_with(TRUNCATED_MARK));
        assert!(cut.len() <= 2049 + TRUNCATED_MARK.len());
        // 2049 is not on a boundary (2 bytes per `é`), so 1024 whole characters.
        assert_eq!(cut[..cut.len() - TRUNCATED_MARK.len()].chars().count(), 1024);
        assert_eq!(truncate_bytes("short", 2048), "short");
    }
}

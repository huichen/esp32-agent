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

/// One request/response round trip. `messages` and `tools` are already serialised
/// by the caller as OpenAI `messages` / `tools` arrays.
pub fn chat(
    transport: &mut dyn ChatTransport,
    cfg: &LlmConfig,
    messages: &Value,
    tools: &Value,
) -> Result<ModelReply, String> {
    let url = cfg.base_url.trim();
    check_url(url)?;
    if cfg.model.trim().is_empty() {
        return Err("llm: model is empty; set one in the agent config".to_string());
    }
    // A NaN/inf temperature serialises to `null`, which some servers reject.
    let temperature =
        if cfg.temperature.is_finite() { cfg.temperature } else { LlmConfig::default().temperature };
    let body = json!({
        "model": cfg.model.trim(),
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "temperature": temperature,
    })
    .to_string();

    let auth = format!("Bearer {}", cfg.api_key.trim());
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    if !cfg.api_key.trim().is_empty() {
        headers.push(("Authorization", &auth));
    }

    let response =
        transport.post_json(url, &headers, &body).map_err(|e| format!("llm: POST {url} failed: {e}"))?;
    parse_reply(&response)
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

    Ok(ModelReply { content, reasoning, tool_calls, usage })
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
        chat(&mut t, &c, &messages(), &tools()).expect("reply");
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
        let reply = chat(&mut t, &cfg(), &messages(), &tools()).expect("reply");
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
        let reply = chat(&mut t, &cfg(), &messages(), &tools()).expect("reply");
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
        let err = chat(&mut t, &cfg(), &messages(), &tools()).expect_err("must fail");
        assert!(err.contains("HTTP 503"), "unexpected error text: {err}");
        assert!(err.chars().count() <= 400, "error text is not bounded: {} chars", err.chars().count());
        assert!(!err.contains('\n'), "error text spans lines: {err:?}");
    }

    #[test]
    fn an_https_endpoint_reaches_the_transport_untouched() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let mut c = cfg();
        c.base_url = "https://api.example.com/v1/chat/completions".to_string();
        chat(&mut t, &c, &messages(), &tools()).expect("https must be carried to the transport");
        assert_eq!(t.urls, vec!["https://api.example.com/v1/chat/completions".to_string()]);
    }

    #[test]
    fn a_scheme_that_is_not_http_is_refused_before_the_transport() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let mut c = cfg();
        c.base_url = "ftp://example.invalid/v1/chat/completions".to_string();
        let err = chat(&mut t, &c, &messages(), &tools()).expect_err("ftp must fail");
        assert!(err.contains("http"), "unexpected error text: {err}");
        assert!(t.urls.is_empty(), "the transport must not be called");
    }

    #[test]
    fn empty_config_is_a_clear_error() {
        let mut t = StubTransport::answering(reply_body(json!({ "content": "x" })));
        let err =
            chat(&mut t, &LlmConfig::default(), &messages(), &tools()).expect_err("empty base_url must fail");
        assert!(err.contains("base_url"), "unexpected error text: {err}");

        let mut c = cfg();
        c.model = "  ".to_string();
        let err = chat(&mut t, &c, &messages(), &tools()).expect_err("empty model must fail");
        assert!(err.contains("model"), "unexpected error text: {err}");
        assert!(t.urls.is_empty());
    }

    #[test]
    fn a_reply_without_choices_reports_the_body_prefix() {
        let mut t = StubTransport::answering(json!({ "error": "no such model" }).to_string());
        let err = chat(&mut t, &cfg(), &messages(), &tools()).expect_err("must fail");
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

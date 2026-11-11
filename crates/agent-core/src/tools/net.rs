//! Network tools for the on-board agent: `net_get`, `net_post`.
//!
//! Both go through [`crate::llm::ChatTransport`] — the same injected socket the
//! chat call uses. There is deliberately no second transport type here: the board
//! has one HTTP client, the host simulator has another, and a tool that opened its
//! own connection would be untestable on the host and unbounded on the board.
//!
//! What that costs: the trait has one verb, `post_json`, so both tools issue a
//! POST. `net_post` sends its body with `Content-Type: application/json`;
//! `net_get` sends an empty body as a cache-friendly request. A server that
//! insists on a real GET answers 405 and the model sees that as a tool error
//! rather than an empty body. Widening `ChatTransport` with a `get()` is the
//! follow-up in `llm.rs`; inventing a parallel struct here would be worse.

use crate::llm::{body_prefix, truncate_bytes, ChatTransport};
use serde_json::{json, Value};

/// Response bodies larger than this are cut, with [`TRUNCATED_MARK`] appended.
/// The agent truncates tool results at the same size; doing it here keeps the
/// marker and the byte count honest instead of losing the tail silently.
pub const MAX_BODY_BYTES: usize = 2048;

/// Longest URL a tool call may pass. The board's header buffer is small, and a
/// model pasting a data: URI must fail fast.
const MAX_URL_LEN: usize = 512;

/// How much of a failing body the error message keeps.
const ERROR_BODY_CHARS: usize = 200;

fn params(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        },
    })
}

/// The OpenAI `tools` array for the network tools.
pub fn specs() -> Value {
    json!([
        function(
            "net_get",
            "Fetch a URL from the board's network and return up to 2048 bytes of the response body. http:// and https:// are both carried; the certificate chain is checked against the board's bundle.",
            params(
                json!({
                    "url": { "type": "string", "description": "http:// URL" },
                }),
                &["url"],
            ),
        ),
        function(
            "net_post",
            "POST a JSON body to a URL from the board's network and return up to 2048 bytes of the response body. http:// and https:// are both carried.",
            params(
                json!({
                    "url": { "type": "string", "description": "http:// URL" },
                    "body": { "type": "string", "description": "request body, sent as application/json" },
                }),
                &["url", "body"],
            ),
        ),
    ])
}

/// Route one tool call. `None` means the name is not a network tool.
pub fn call(transport: &mut dyn ChatTransport, name: &str, args: &Value) -> Option<Result<String, String>> {
    match name {
        "net_get" => Some(get(transport, args)),
        "net_post" => Some(post(transport, args)),
        _ => None,
    }
}

fn get(transport: &mut dyn ChatTransport, args: &Value) -> Result<String, String> {
    let url = check_url(args)?;
    let body = transport.post_json(&url, &[], "").map_err(transport_error)?;
    Ok(result_json(
        json!({
            "url": url,
            "method": "POST",
            "request_bytes": 0,
        }),
        &body,
    ))
}

fn post(transport: &mut dyn ChatTransport, args: &Value) -> Result<String, String> {
    let url = check_url(args)?;
    let body_in = match args.get("body") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => {
            return Err(format!("`body` must be a string, got {}", kind_of(other)));
        }
    };
    let headers: [(&str, &str); 1] = [("Content-Type", "application/json")];
    let body = transport.post_json(&url, &headers, &body_in).map_err(transport_error)?;
    Ok(result_json(
        json!({
            "url": url,
            "method": "POST",
            "request_bytes": body_in.len(),
        }),
        &body,
    ))
}

/// A successful tool answer: the (bounded) body plus enough context that the
/// model knows whether it is looking at the whole thing.
fn result_json(meta: Value, body: &str) -> String {
    let bytes = body.len();
    let truncated = bytes > MAX_BODY_BYTES;
    let mut out = meta;
    out["bytes"] = json!(bytes);
    out["truncated"] = json!(truncated);
    out["body"] = json!(truncate_bytes(body, MAX_BODY_BYTES));
    serde_json::to_string(&out).unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

/// Non-2xx arrives as `Err` from the transport, already shaped by
/// [`crate::llm::http_error`] as `HTTP <status>: <body prefix>`. Keep that — the
/// status is the useful part — and only bound and label anything else.
fn transport_error(e: String) -> String {
    let e = e.trim();
    if e.starts_with("HTTP ") {
        e.to_string()
    } else if e.is_empty() {
        "request failed: the transport returned no detail".to_string()
    } else {
        format!("request failed: {}", body_prefix(e, ERROR_BODY_CHARS))
    }
}

/// `http://` or `https://`, and never empty or absurdly long.
///
/// The scheme is the transport's business (`net_get` runs on the same client the model
/// call uses), so this only refuses what is not HTTP at all — a `ftp://` or a bare host
/// would otherwise surface as an obscure socket error.
fn check_url(args: &Value) -> Result<String, String> {
    let url = match args.get("url") {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => return Err("`url` (string) is required".to_string()),
    };
    if url.is_empty() {
        return Err("`url` must not be empty".to_string());
    }
    if url.len() > MAX_URL_LEN {
        return Err(format!("`url` is {} bytes, limit is {MAX_URL_LEN}", url.len()));
    }
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(format!("`url` must start with http:// or https://, got `{}`", body_prefix(&url, 40)));
    }
    Ok(url)
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{http_error, TRUNCATED_MARK};
    use std::collections::VecDeque;

    /// One recorded request: url, headers, body.
    type Seen = (String, Vec<(String, String)>, String);

    /// Records every request and replays canned answers, so the tools can be
    /// tested without a socket and without a second transport abstraction.
    #[derive(Default)]
    struct Fake {
        replies: VecDeque<Result<String, String>>,
        seen: Vec<Seen>,
    }

    impl Fake {
        fn ok(body: &str) -> Self {
            let mut f = Fake::default();
            f.replies.push_back(Ok(body.to_string()));
            f
        }

        fn err(e: String) -> Self {
            let mut f = Fake::default();
            f.replies.push_back(Err(e));
            f
        }
    }

    impl ChatTransport for Fake {
        fn post_json(&mut self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
            self.seen.push((
                url.to_string(),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                body.to_string(),
            ));
            self.replies.pop_front().unwrap_or_else(|| Ok(String::new()))
        }
    }

    fn run(f: &mut Fake, name: &str, args: Value) -> Result<Value, String> {
        let text = call(f, name, &args).ok_or_else(|| format!("not a net tool: {name}"))??;
        serde_json::from_str(&text).map_err(|e| format!("tool result is not JSON: {e}"))
    }

    #[test]
    fn net_get_returns_the_body_and_shows_what_it_sent() {
        let mut f = Fake::ok(r#"{"temperature":21.5}"#);
        let v = run(&mut f, "net_get", json!({"url": "http://192.0.2.42/status"})).unwrap();
        assert_eq!(v["url"], "http://192.0.2.42/status");
        assert_eq!(v["body"], "{\"temperature\":21.5}");
        assert_eq!(v["bytes"], 20);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["request_bytes"], 0);

        assert_eq!(f.seen.len(), 1);
        assert_eq!(f.seen[0].0, "http://192.0.2.42/status");
        assert_eq!(f.seen[0].2, "", "net_get sends an empty body through post_json");
    }

    #[test]
    fn net_post_sends_json_content_type_and_the_body() {
        let mut f = Fake::ok("{\"ok\":true}");
        let v = run(&mut f, "net_post", json!({"url": "http://10.0.0.2/hook", "body": "{\"a\":1}"})).unwrap();
        assert_eq!(v["request_bytes"], 7);
        assert_eq!(v["body"], "{\"ok\":true}");
        let (url, headers, body) = &f.seen[0];
        assert_eq!(url, "http://10.0.0.2/hook");
        assert_eq!(body, "{\"a\":1}");
        assert!(
            headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == "application/json"),
            "headers were {headers:?}"
        );

        // A POST with no body is allowed and still labelled as JSON.
        let mut f = Fake::ok("");
        let v = run(&mut f, "net_post", json!({"url": "http://10.0.0.2/x"})).unwrap();
        assert_eq!(v["request_bytes"], 0);
        assert_eq!(v["body"], "");
    }

    #[test]
    fn the_response_body_is_truncated_to_2kb_with_a_marker() {
        let big = "x".repeat(MAX_BODY_BYTES + 500);
        let mut f = Fake::ok(&big);
        let v = run(&mut f, "net_get", json!({"url": "http://host/big"})).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["bytes"], MAX_BODY_BYTES + 500, "the real size is still reported");
        let body = v["body"].as_str().unwrap();
        assert!(body.len() <= MAX_BODY_BYTES + TRUNCATED_MARK.len(), "{}", body.len());
        assert!(body.ends_with(TRUNCATED_MARK), "the cut must be visible");

        // Exactly at the limit is not truncated.
        let mut f = Fake::ok(&"y".repeat(MAX_BODY_BYTES));
        let v = run(&mut f, "net_get", json!({"url": "http://host/edge"})).unwrap();
        assert_eq!(v["truncated"], false);
        assert!(!v["body"].as_str().unwrap().ends_with(TRUNCATED_MARK));
    }

    #[test]
    fn non_2xx_is_an_error_carrying_status_and_body_prefix() {
        let mut f = Fake::err(http_error(500, "{\"error\":\"model exploded\"}"));
        let err = run(&mut f, "net_get", json!({"url": "http://host/x"})).unwrap_err();
        assert!(err.starts_with("HTTP 500:"), "{err}");
        assert!(err.contains("model exploded"), "{err}");

        // A 404 with a long HTML body keeps only a prefix.
        let long = format!("<html>{}</html>", "z".repeat(4000));
        let mut f = Fake::err(http_error(404, &long));
        let err = run(&mut f, "net_post", json!({"url": "http://host/x", "body": ""})).unwrap_err();
        assert!(err.starts_with("HTTP 404:"), "{err}");
        assert!(err.chars().count() < 300, "error must stay short: {}", err.chars().count());
    }

    #[test]
    fn transport_failures_that_are_not_http_get_a_label() {
        let mut f = Fake::err("connect timeout after 5000 ms".to_string());
        let err = run(&mut f, "net_get", json!({"url": "http://host/x"})).unwrap_err();
        assert!(err.starts_with("request failed:"), "{err}");
        assert!(err.contains("timeout"), "{err}");

        let mut f = Fake::err(String::new());
        let err = run(&mut f, "net_get", json!({"url": "http://host/x"})).unwrap_err();
        assert!(err.contains("no detail"), "{err}");
    }

    #[test]
    fn https_is_carried_and_bad_urls_are_rejected_before_the_socket() {
        let mut f = Fake::ok("over tls");
        let value = run(&mut f, "net_get", json!({"url": "https://api.example.com/v1"})).unwrap();
        assert_eq!(value["body"], "over tls", "{value}");
        assert_eq!(f.seen.len(), 1, "the request never reached the transport");
        assert_eq!(f.seen[0].0, "https://api.example.com/v1", "the scheme must be carried, not rewritten");

        for (args, needle) in [
            (json!({}), "`url`"),
            (json!({"url": ""}), "must not be empty"),
            (json!({"url": "ftp://host/x"}), "must start with http:// or https://"),
            (json!({"url": 42}), "`url`"),
        ] {
            let err = run(&mut f, "net_get", args).unwrap_err();
            assert!(err.contains(needle), "{err} did not contain {needle}");
        }
        let too_long = format!("http://host/{}", "p".repeat(MAX_URL_LEN));
        let err = run(&mut f, "net_get", json!({"url": too_long})).unwrap_err();
        assert!(err.contains("limit is 512"), "{err}");
        // Only the one legal request reached the transport; every refusal above stopped
        // before the socket.
        assert_eq!(f.seen.len(), 1, "a refused URL still went out: {:?}", f.seen);
    }

    #[test]
    fn specs_are_openai_tools_and_foreign_names_are_none() {
        let specs = specs();
        let list = specs.as_array().expect("tools array");
        let names: Vec<&str> = list.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["net_get", "net_post"]);
        for t in list {
            assert_eq!(t["type"], "function");
            assert_eq!(t["function"]["parameters"]["type"], "object");
        }
        let mut f = Fake::default();
        assert!(call(&mut f, "fs_read", &json!({})).is_none());
        assert!(call(&mut f, "net_get", &json!({"url": "http://h/x"})).is_some());
    }
}

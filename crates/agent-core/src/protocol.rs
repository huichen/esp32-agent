//! JSON-RPC 2.0 / MCP framing.
//!
//! Only the parts of the MCP specification that a constrained device can honour
//! are implemented: `initialize`, `ping`, `tools/*`, `resources/*`, `prompts/*`
//! (empty) and `logging/setLevel`. A notification gets no body; the Streamable
//! HTTP transport returns 202 for it, as the specification requires.

use crate::error::code;
use serde_json::{json, Value};

/// The protocol revision this server speaks. A client asking for another
/// revision is answered with this one, which is what the spec asks for.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub const SERVER_NAME: &str = "esp32-agent";

#[derive(Debug, Clone)]
pub struct RpcIn {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
    /// A request without an `id` is a notification and must not be answered.
    pub is_notification: bool,
}

pub type RpcError = (Option<Value>, i32, String);

/// Retain the outer framing and each member's validation result. An invalid batch
/// member must not discard its valid peers or turn an array response into an object.
#[derive(Debug)]
pub struct RpcFrame {
    pub is_batch: bool,
    pub messages: Vec<Result<RpcIn, RpcError>>,
}

/// Compatibility seam for callers that need only fully valid requests (including
/// the single tools/list fast path). Dispatch uses [`parse_frame`] instead.
pub fn parse(body: &str) -> Result<Vec<RpcIn>, RpcError> {
    parse_frame(body)?.messages.into_iter().collect()
}

pub fn parse_frame(body: &str) -> Result<RpcFrame, RpcError> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| (None, code::PARSE_ERROR, format!("invalid JSON: {e}")))?;
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Err((None, code::INVALID_REQUEST, "empty batch".into()));
            }
            Ok(RpcFrame { is_batch: true, messages: items.into_iter().map(into_request).collect() })
        }
        other => Ok(RpcFrame { is_batch: false, messages: vec![into_request(other)] }),
    }
}

fn into_request(v: Value) -> Result<RpcIn, RpcError> {
    // Null is a legal, if discouraged, id. Only an absent id denotes a notification.
    // Invalid id shapes cannot be echoed as an id in the error response.
    let id = v.get("id").filter(|i| i.is_null() || i.is_string() || i.is_number()).cloned();
    let obj = v.as_object().ok_or((id.clone(), code::INVALID_REQUEST, "message is not an object".into()))?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((id, code::INVALID_REQUEST, "jsonrpc must be \"2.0\"".into()));
    }
    if obj.contains_key("id") && id.is_none() {
        return Err((None, code::INVALID_REQUEST, "`id` must be a string, number or null".into()));
    }
    let method = obj
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or((id.clone(), code::INVALID_REQUEST, "missing `method`".into()))?
        .to_string();
    if let Some(params) = obj.get("params") {
        if !params.is_object() && !params.is_array() {
            return Err((id, code::INVALID_REQUEST, "`params` must be an object or array".into()));
        }
    }
    Ok(RpcIn {
        is_notification: !obj.contains_key("id"),
        id,
        method,
        params: obj.get("params").cloned().unwrap_or_else(|| json!({})),
    })
}

pub fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

pub fn err(id: Option<Value>, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": e })
}

/// `initialize` result: capabilities, server info and the instructions the model
/// sees first. The instructions are generated from what the board has, so they
/// never mention hardware that is not there.
pub fn initialize_result(name: &str, version: &str, instructions: &str, resources: bool) -> Value {
    let mut caps = json!({
        "tools": { "listChanged": true },
        "logging": {},
        "prompts": { "listChanged": false },
    });
    if resources {
        caps["resources"] = json!({ "subscribe": false, "listChanged": true });
    }
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": caps,
        "serverInfo": { "name": name, "version": version },
        "instructions": instructions,
    })
}

/// A tool result carrying one text block (what every MCP client renders).
pub fn tool_text(text: impl Into<String>) -> Value {
    json!({
        "content": [ { "type": "text", "text": text.into() } ],
        "isError": false,
    })
}

/// A tool result carrying pretty-printed JSON plus MCP's structured field.
pub fn tool_json(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": value,
        "isError": false,
    })
}

pub fn tool_error(e: &crate::error::McpError) -> Value {
    json!({
        "content": [ { "type": "text", "text": format!("{} (code {})", e.message, e.code) } ],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_notifications_are_distinguished() {
        let msgs = parse(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].is_notification);
        assert_eq!(msgs[0].id, Some(json!(7)));

        let msgs = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(msgs[0].is_notification);
        assert_eq!(msgs[0].params, json!({}));
    }

    #[test]
    fn string_and_batch_ids_survive() {
        let msgs =
            parse(r#"[{"jsonrpc":"2.0","id":"a","method":"ping"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#)
                .unwrap();
        assert_eq!(msgs[0].id, Some(json!("a")));
        assert_eq!(msgs[1].id, Some(json!(2)));
    }

    #[test]
    fn bad_json_reports_a_parse_error() {
        let (id, code_n, _) = parse("{not json").unwrap_err();
        assert!(id.is_none());
        assert_eq!(code_n, code::PARSE_ERROR);
    }

    #[test]
    fn wrong_jsonrpc_version_is_rejected() {
        let e = parse(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).unwrap_err();
        assert_eq!(e.1, code::INVALID_REQUEST);
        assert_eq!(e.0, Some(json!(1)));
    }

    #[test]
    fn request_version_id_and_params_are_validated() {
        for (value, id) in [
            (json!({"id": 1, "method": "ping"}), Some(json!(1))),
            (json!({"jsonrpc": "2.0", "id": {}, "method": "ping"}), None),
            (json!({"jsonrpc": "2.0", "id": [], "method": "ping"}), None),
            (json!({"jsonrpc": "2.0", "id": true, "method": "ping"}), None),
            (json!({"jsonrpc": "2.0", "id": "p", "method": "ping", "params": 1}), Some(json!("p"))),
            (json!({"jsonrpc": "2.0", "id": null, "method": "ping", "params": null}), Some(Value::Null)),
        ] {
            let error = parse(&value.to_string()).unwrap_err();
            assert_eq!(error.0, id, "{value}");
            assert_eq!(error.1, code::INVALID_REQUEST, "{value}");
        }
        assert!(parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[]}"#).is_ok());
    }

    #[test]
    fn null_ids_and_batch_member_errors_retain_their_framing() {
        let frame = parse_frame(
            r#"[{"jsonrpc":"2.0","id":null,"method":"ping"},1,{"jsonrpc":"2.0","method":"ping"}]"#,
        )
        .unwrap();
        assert!(frame.is_batch);
        assert_eq!(frame.messages.len(), 3);
        let request = frame.messages[0].as_ref().unwrap();
        assert_eq!(request.id, Some(Value::Null));
        assert!(!request.is_notification);
        let error = frame.messages[1].as_ref().unwrap_err();
        assert_eq!(error.0, None);
        assert_eq!(error.1, code::INVALID_REQUEST);
        assert!(frame.messages[2].as_ref().unwrap().is_notification);
        assert!(!parse_frame(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap().is_batch);
    }

    #[test]
    fn initialize_advertises_the_implemented_capabilities() {
        let r = initialize_result("esp32-agent", "0.1.0", "hello", true);
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["capabilities"]["tools"]["listChanged"], true);
        assert!(r["capabilities"]["resources"].is_object());
        assert!(!r["instructions"].as_str().unwrap_or("").is_empty());
        assert_eq!(r["serverInfo"]["name"], "esp32-agent");
    }

    #[test]
    fn tool_results_are_rendered_as_text_blocks() {
        let v = tool_json(&json!({ "a": 1 }));
        assert!(v["content"][0]["text"].as_str().unwrap().contains("\"a\""));
        assert_eq!(v["isError"], false);
        let e = tool_error(&crate::error::McpError::hardware("nope"));
        assert_eq!(e["isError"], true);
    }
}

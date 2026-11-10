//! Error type shared by the protocol layer, the tool handlers and the drivers.
//!
//! Every error carries a JSON-RPC error code, so a tool failure reaches the
//! model as a structured error instead of a stringly-typed 500.

use serde_json::{json, Value};
use std::fmt;

/// Standard JSON-RPC 2.0 codes plus the ranges this firmware uses for its own
/// failures. The custom codes stay inside the implementation-defined range
/// (-32000..=-32099) reserved by the spec.
pub mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    /// A tool exists but the hardware or firmware cannot do what was asked.
    pub const UNSUPPORTED: i32 = -32001;
    /// The peripheral reported a failure (NAK, timeout, bad CRC, ...).
    pub const HARDWARE: i32 = -32002;
    /// Unknown device, tool or resource name.
    pub const NOT_FOUND: i32 = -32004;
    /// Refused by configuration (tool disabled, output not allowed, ...).
    pub const FORBIDDEN: i32 = -32005;
    /// Authenticated path required and the caller did not authenticate.
    pub const UNAUTHORIZED: i32 = -32006;
}

#[derive(Debug, Clone)]
pub struct McpError {
    pub code: i32,
    pub message: String,
    /// Optional machine-readable detail appended as `data`.
    pub data: Option<Value>,
}

impl McpError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(code::INTERNAL_ERROR, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(code::UNSUPPORTED, message)
    }

    pub fn hardware(message: impl Into<String>) -> Self {
        Self::new(code::HARDWARE, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(code::NOT_FOUND, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(code::FORBIDDEN, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(code::UNAUTHORIZED, message)
    }

    /// JSON-RPC error object (`{"code":..,"message":..,"data":..}`).
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.data {
            v["data"] = d.clone();
        }
        v
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for McpError {}

impl From<std::io::Error> for McpError {
    fn from(e: std::io::Error) -> Self {
        McpError::hardware(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for McpError {
    fn from(e: serde_json::Error) -> Self {
        McpError::invalid_params(format!("json: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, McpError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_carries_code_and_data() {
        let e = McpError::hardware("ds18b20 crc mismatch").with_data(json!({ "pin": 4 }));
        let v = e.to_json();
        assert_eq!(v["code"], code::HARDWARE);
        assert_eq!(v["message"], "ds18b20 crc mismatch");
        assert_eq!(v["data"]["pin"], 4);
    }
}

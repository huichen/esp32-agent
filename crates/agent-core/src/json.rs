//! Small helpers for pulling typed arguments out of a tool call.
//!
//! Tool schemas are advisory: a model will send `"3"` where an integer is
//! expected, or omit an optional field. These helpers keep the handlers free of
//! that boilerplate and produce an `INVALID_PARAMS` error naming the field.

use crate::error::{McpError, Result};
use serde_json::Value;

pub fn field<'a>(args: &'a Value, name: &str) -> Option<&'a Value> {
    args.get(name).filter(|v| !v.is_null())
}

pub fn opt_str(args: &Value, name: &str) -> Option<String> {
    match field(args, name) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

pub fn str_arg(args: &Value, name: &str) -> Result<String> {
    opt_str(args, name).ok_or_else(|| McpError::invalid_params(format!("`{name}` (string) is required")))
}

pub fn opt_i64(args: &Value, name: &str) -> Option<i64> {
    let v = field(args, name)?;
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    v.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f as i64)
}

pub fn opt_f64(args: &Value, name: &str) -> Option<f64> {
    let v = field(args, name)?;
    if let Some(f) = v.as_f64() {
        return Some(f);
    }
    v.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

pub fn i64_arg(args: &Value, name: &str) -> Result<i64> {
    opt_i64(args, name).ok_or_else(|| McpError::invalid_params(format!("`{name}` (integer) is required")))
}

pub fn f64_arg(args: &Value, name: &str) -> Result<f64> {
    opt_f64(args, name).ok_or_else(|| McpError::invalid_params(format!("`{name}` (number) is required")))
}

pub fn opt_bool(args: &Value, name: &str) -> Option<bool> {
    match field(args, name)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub fn bool_arg(args: &Value, name: &str) -> Result<bool> {
    opt_bool(args, name).ok_or_else(|| McpError::invalid_params(format!("`{name}` (boolean) is required")))
}

/// Integer with an inclusive range check, clamped only when `clamp` is set.
pub fn ranged_i64(args: &Value, name: &str, default: i64, min: i64, max: i64) -> Result<i64> {
    let v = opt_i64(args, name).unwrap_or(default);
    if v < min || v > max {
        return Err(McpError::invalid_params(format!("`{name}` must be within {min}..={max}, got {v}")));
    }
    Ok(v)
}

pub fn opt_u8(args: &Value, name: &str) -> Option<u8> {
    opt_i64(args, name).and_then(|v| u8::try_from(v).ok())
}

pub fn arr<'a>(args: &'a Value, name: &str) -> Option<&'a Vec<Value>> {
    field(args, name).and_then(|v| v.as_array())
}

/// 24-bit colour accepted as `#rrggbb`, `rrggbb`, `[r,g,b]` or `{r,g,b}`.
pub fn color(args: &Value, name: &str) -> Result<(u8, u8, u8)> {
    let v = field(args, name)
        .ok_or_else(|| McpError::invalid_params(format!("`{name}` (colour) is required")))?;
    parse_color(v)
        .ok_or_else(|| McpError::invalid_params(format!("`{name}` must be #rrggbb, [r,g,b] or {{r,g,b}}")))
}

pub fn parse_color(v: &Value) -> Option<(u8, u8, u8)> {
    match v {
        Value::String(s) => {
            let s = s.trim().trim_start_matches('#');
            if s.len() != 6 {
                return None;
            }
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some((r, g, b))
        }
        Value::Array(a) if a.len() >= 3 => {
            let r = a[0].as_u64()?.min(255) as u8;
            let g = a[1].as_u64()?.min(255) as u8;
            let b = a[2].as_u64()?.min(255) as u8;
            Some((r, g, b))
        }
        Value::Object(o) => {
            let r = o.get("r").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
            let g = o.get("g").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
            let b = o.get("b").and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8;
            Some((r, g, b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_numbers_are_accepted() {
        let a = json!({ "pin": "42", "ms": "12.7" });
        assert_eq!(opt_i64(&a, "pin"), Some(42));
        assert_eq!(opt_i64(&a, "ms"), Some(12));
    }

    #[test]
    fn range_errors_name_the_field() {
        let e = ranged_i64(&json!({ "pin": 99 }), "pin", 0, 0, 48).unwrap_err();
        assert!(e.message.contains("pin"));
    }

    #[test]
    fn colour_forms() {
        assert_eq!(parse_color(&json!("#00ff10")), Some((0, 255, 16)));
        assert_eq!(parse_color(&json!([1, 2, 3])), Some((1, 2, 3)));
        assert_eq!(parse_color(&json!({"r": 4, "g": 5, "b": 6})), Some((4, 5, 6)));
        assert_eq!(parse_color(&json!("nope")), None);
    }
}

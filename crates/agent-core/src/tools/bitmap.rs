//! Pure bitmap compression tools; display/storage remain the caller's next step.

use crate::bitmap::{self, MAX_PACKED_BYTES};
use crate::error::{McpError, Result};
use crate::json as j;
use crate::tool::{integer, object_schema, string, ToolDef};
use crate::util::{b64_decode_limit, b64_encode};
use serde_json::{json, Value};

/// With worst-case RLE overhead, 32 KiB raw becomes at most 33,024 packed bytes
/// (44,032 base64 characters), leaving ample room for JSON in a 64 KiB result.
/// This also matches media_play's existing decoded-bitmap request budget.
pub const MAX_TOOL_RAW_BYTES: usize = 32 * 1024;

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "bitmap_pack",
            "RLE-compress already bit-packed bitmap page bytes without changing their pixels. Returns packed_b64, raw_bytes, packed_bytes, ratio (packed/raw; null for empty input) and runs (emitted repeat pairs). Literal blocks bound worst-case expansion to ceil(raw_bytes/128) bytes; compression depends on content. Unpack before passing data to media_play: it still takes raw bitmap_b64.",
        )
        .title("Pack bitmap")
        .category("media")
        .core()
        .schema(object_schema(
            &[("data_b64", string("base64 of raw page-format bitmap bytes (8 vertical pixels per byte, bit 0 at top), at most 32768 decoded bytes; frames may be concatenated"))],
            &["data_b64"],
        ))
        .bind(|_, args| pack_bitmap(args)),
        ToolDef::new(
            "bitmap_unpack",
            "Decode bitmap-rle-v1 repeat/literal blocks and return data_b64 plus raw_bytes. Requires the exact raw length: truncated streams, trailing blocks and runs exceeding it are refused. No display writes or storage changes.",
        )
        .title("Unpack bitmap")
        .category("media")
        .core()
        .schema(object_schema(
            &[
                ("packed_b64", string("base64 of bitmap_pack's RLE stream, at most 65536 decoded packed bytes")),
                ("bytes", integer(0, MAX_TOOL_RAW_BYTES as i64, "exact decoded raw length in bytes, not base64 characters; at most 32768 so the base64 result fits one tool result")),
            ],
            &["packed_b64", "bytes"],
        ))
        .bind(|_, args| unpack_bitmap(args)),
    ]
}

fn decoded_arg(args: &Value, name: &str, max_bytes: usize) -> Result<Vec<u8>> {
    let encoded = args
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::invalid_params(format!("`{name}` must be a base64 string")))?;
    let mut data = Vec::new();
    b64_decode_limit(encoded, &mut data, max_bytes).map_err(|mut error| {
        error.message = format!("`{name}` {}", error.message);
        error
    })?;
    Ok(data)
}

fn pack_bitmap(args: &Value) -> Result<Value> {
    let raw = decoded_arg(args, "data_b64", MAX_TOOL_RAW_BYTES)?;
    let (packed, runs) = bitmap::pack_with_runs(&raw);
    Ok(json!({
        "packed_b64": b64_encode(&packed),
        "raw_bytes": raw.len(),
        "packed_bytes": packed.len(),
        "ratio": if raw.is_empty() { Value::Null } else { json!(packed.len() as f64 / raw.len() as f64) },
        "runs": runs,
    }))
}

fn unpack_bitmap(args: &Value) -> Result<Value> {
    let bytes = j::optional_ranged_u64(args, "bytes", 0, MAX_TOOL_RAW_BYTES as u64)?
        .ok_or_else(|| McpError::invalid_params("`bytes` (exact raw length) is required"))?
        as usize;
    let packed = decoded_arg(args, "packed_b64", MAX_PACKED_BYTES)?;
    let raw = bitmap::unpack(&packed, bytes)?;
    Ok(json!({ "data_b64": b64_encode(&raw), "raw_bytes": raw.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::MAX_TOOL_RESULT_BYTES;
    use crate::app::{App, FwMeta};
    use crate::error::code;
    use crate::logbuf::LogBuf;
    use crate::store::MemoryStore;
    use crate::virtual_board::VirtualBoard;

    fn fixture() -> App {
        App::new(
            Box::new(VirtualBoard::new()),
            Box::new(MemoryStore::new()),
            LogBuf::new(64),
            FwMeta::default(),
        )
    }

    fn rpc_call(app: &mut App, name: &str, args: &Value) -> Value {
        app.handle_rpc(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": name, "arguments": args,
            }})
            .to_string(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn tools_are_advertised_read_only_and_round_trip_over_mcp() {
        let mut app = fixture();
        app.cfg.pins.insert("oled.height".into(), 32);
        app.rebuild();
        let listed = app.handle_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, None).unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        for name in ["bitmap_pack", "bitmap_unpack"] {
            let spec = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(spec["annotations"]["readOnlyHint"], true);
            assert_eq!(spec["inputSchema"]["additionalProperties"], false);
        }
        let unpack_spec = tools.iter().find(|tool| tool["name"] == "bitmap_unpack").unwrap();
        assert_eq!(unpack_spec["inputSchema"]["properties"]["bytes"]["maximum"], MAX_TOOL_RAW_BYTES);
        assert_eq!(unpack_spec["inputSchema"]["required"], json!(["packed_b64", "bytes"]));

        let mut raw = vec![0; 512];
        raw[..5].copy_from_slice(&[0x80, 0xff, 0, 0xab, 0xab]);
        let encoded = b64_encode(&raw);
        let packed_response = rpc_call(&mut app, "bitmap_pack", &json!({"data_b64": encoded}));
        assert_eq!(packed_response["result"]["isError"], false, "{packed_response}");
        let packed = &packed_response["result"]["structuredContent"];
        assert_eq!(packed["raw_bytes"], 512);
        assert_eq!(packed["packed_b64"], b64_encode(&bitmap::pack(&raw)));
        assert_eq!(packed["packed_bytes"], bitmap::pack(&raw).len());
        let unpacked_response =
            rpc_call(&mut app, "bitmap_unpack", &json!({"packed_b64": packed["packed_b64"], "bytes": 512}));
        assert_eq!(unpacked_response["result"]["isError"], false, "{unpacked_response}");
        let unpacked = &unpacked_response["result"]["structuredContent"];
        assert_eq!(unpacked, &json!({"data_b64": encoded, "raw_bytes": 512}));
        let played = app
            .call_tool(
                "media_play",
                &json!({"frames": [{
                    "at_ms": 0, "bitmap_b64": unpacked["data_b64"],
                }]}),
            )
            .unwrap();
        assert_eq!(played["frames_drawn"], 1, "the existing raw bitmap input still works");
        assert!(
            app.call_tool(
                "media_play",
                &json!({"frames": [{
                    "at_ms": 0, "bitmap_b64": packed["packed_b64"],
                }]})
            )
            .is_err(),
            "media_play does not silently treat a packed stream as pixels"
        );
    }

    #[test]
    fn pack_reports_repeat_blocks_not_literal_blocks_and_empty_ratio() {
        let mut app = fixture();
        let solid = app.call_tool("bitmap_pack", &json!({"data_b64": b64_encode(&[0; 512])})).unwrap();
        assert_eq!(
            solid,
            json!({
                "packed_b64": b64_encode(&[0xff, 0, 0xff, 0, 0xff, 0, 0xff, 0]),
                "raw_bytes": 512, "packed_bytes": 8, "ratio": 0.015625, "runs": 4,
            })
        );
        let literal = app.call_tool("bitmap_pack", &json!({"data_b64": b64_encode(&[0, 1, 1, 2])})).unwrap();
        assert_eq!(literal["raw_bytes"], 4);
        assert_eq!(literal["packed_bytes"], 5);
        assert_eq!(literal["ratio"], 1.25);
        assert_eq!(literal["runs"], 0);
        assert_eq!(
            app.call_tool("bitmap_pack", &json!({"data_b64": ""})).unwrap(),
            json!({
                "packed_b64": "", "raw_bytes": 0, "packed_bytes": 0, "ratio": null, "runs": 0,
            })
        );
        assert_eq!(
            app.call_tool("bitmap_unpack", &json!({"packed_b64": "", "bytes": 0})).unwrap(),
            json!({"data_b64": "", "raw_bytes": 0})
        );
    }

    #[test]
    fn invalid_base64_types_and_exact_lengths_are_refused() {
        let mut app = fixture();
        for args in [
            json!({}),
            json!({"data_b64": null}),
            json!({"data_b64": 123}),
            json!({"data_b64": "!!!!"}),
            json!({"data_b64": "AA=A"}),
            json!({"data_b64": "AB=="}),
        ] {
            let error = app.call_tool("bitmap_pack", &args).unwrap_err();
            assert_eq!(error.code, code::INVALID_PARAMS);
            assert!(error.message.contains("data_b64"));
        }
        let packed = b64_encode(&[0x82, 0]);
        for bytes in [json!(null), json!("3"), json!(3.5), json!(true), json!(-1)] {
            let error =
                app.call_tool("bitmap_unpack", &json!({"packed_b64": packed, "bytes": bytes})).unwrap_err();
            assert_eq!(error.code, code::INVALID_PARAMS);
            assert!(error.message.contains("bytes"));
        }
        assert!(app.call_tool("bitmap_unpack", &json!({"packed_b64": packed})).is_err());
        for args in [
            json!({"packed_b64": null, "bytes": 3}),
            json!({"packed_b64": 123, "bytes": 3}),
            json!({"packed_b64": "!!!!", "bytes": 3}),
            json!({"packed_b64": "AA=A", "bytes": 3}),
            json!({"packed_b64": b64_encode(&[0x82]), "bytes": 3}),
            json!({"packed_b64": b64_encode(&[2, 0, 1]), "bytes": 3}),
            json!({"packed_b64": packed, "bytes": 2}),
            json!({"packed_b64": packed, "bytes": 4}),
        ] {
            let response = rpc_call(&mut app, "bitmap_unpack", &args);
            assert_eq!(response["result"]["isError"], true, "{response}");
        }
    }

    #[test]
    fn limits_and_base64_expansion_fit_the_tool_result_budget() {
        let mut app = fixture();
        let raw: Vec<u8> = (0..MAX_TOOL_RAW_BYTES).map(|i| i as u8).collect();
        let encoded = b64_encode(&raw);
        let packed = app.call_tool("bitmap_pack", &json!({"data_b64": encoded})).unwrap();
        assert_eq!(packed["packed_bytes"], MAX_TOOL_RAW_BYTES + MAX_TOOL_RAW_BYTES.div_ceil(128));
        assert_eq!(packed["packed_b64"].as_str().unwrap().len(), 44_032);
        assert!(packed.to_string().len() < MAX_TOOL_RESULT_BYTES);
        let unpacked = app
            .call_tool(
                "bitmap_unpack",
                &json!({
                    "packed_b64": packed["packed_b64"], "bytes": raw.len(),
                }),
            )
            .unwrap();
        assert_eq!(unpacked["data_b64"], encoded);
        assert!(unpacked.to_string().len() < MAX_TOOL_RESULT_BYTES);
        // The exact packed-input boundary is accepted, even with inefficient runs.
        let maximum_packed = [0x80, 0xab].repeat(MAX_PACKED_BYTES / 2);
        let unpacked = app
            .call_tool(
                "bitmap_unpack",
                &json!({
                    "packed_b64": b64_encode(&maximum_packed), "bytes": MAX_TOOL_RAW_BYTES,
                }),
            )
            .unwrap();
        assert_eq!(unpacked["raw_bytes"], MAX_TOOL_RAW_BYTES);
        assert!(unpacked.to_string().len() < MAX_TOOL_RESULT_BYTES);
        let mut oversized = maximum_packed;
        oversized.push(0);
        assert!(app
            .call_tool(
                "bitmap_unpack",
                &json!({
                    "packed_b64": b64_encode(&oversized), "bytes": MAX_TOOL_RAW_BYTES,
                })
            )
            .unwrap_err()
            .message
            .contains("65536"));
        assert!(app
            .call_tool(
                "bitmap_unpack",
                &json!({
                    "packed_b64": "", "bytes": MAX_TOOL_RAW_BYTES + 1,
                })
            )
            .unwrap_err()
            .message
            .contains("32768"));
        assert!(app
            .call_tool(
                "bitmap_pack",
                &json!({
                    "data_b64": b64_encode(&vec![0; MAX_TOOL_RAW_BYTES + 1]),
                })
            )
            .unwrap_err()
            .message
            .contains("32768"));
    }

    #[test]
    fn no_panel_is_needed_and_disabled_tools_stay_disabled() {
        use std::sync::atomic::Ordering;
        let mut board = VirtualBoard::new();
        board.i2c.clear();
        let calls = board.hw_calls.clone();
        let mut app =
            App::new(Box::new(board), Box::new(MemoryStore::new()), LogBuf::new(64), FwMeta::default());
        app.cfg.mcp.profile = "minimal".into();
        app.cfg.mcp.tools_mode = "generic".into();
        assert!(!app.devices.has_cap("oled"));
        let before = calls.load(Ordering::SeqCst);
        for name in ["bitmap_pack", "bitmap_unpack"] {
            assert!(app.is_visible(name), "{name} needs no device");
        }
        let args = json!({"data_b64": b64_encode(&[0xab; 512])});
        let packed = app.call_tool("bitmap_pack", &args).unwrap();
        let unpack_args = json!({"packed_b64": packed["packed_b64"], "bytes": 512});
        assert_eq!(app.call_tool("bitmap_unpack", &unpack_args).unwrap()["raw_bytes"], 512);
        for (name, args) in [("bitmap_pack", args), ("bitmap_unpack", unpack_args)] {
            app.cfg.mcp.disabled_tools.push(name.into());
            assert!(!app.is_visible(name));
            assert_eq!(app.call_tool(name, &args).unwrap_err().code, code::FORBIDDEN);
        }
        assert_eq!(calls.load(Ordering::SeqCst), before, "compression never accesses hardware");
    }
}

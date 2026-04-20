//! Machine-friendly control protocol for `srtla_send`.
//!
//! Speaks JSON-RPC 2.0 over stdin or a Unix socket, one request per line.
//! Requests that omit `id` are notifications and get no response; this is
//! the hot path for `mark_critical` where an upstream encoder fires a
//! hint-per-keyframe and never wants to block waiting for an ACK.
//!
//! Methods:
//! - `set_mode { mode: "classic"|"enhanced"|"rtt-threshold"|"edpf" }`
//! - `set_quality { enabled: bool }`
//! - `set_exploration { enabled: bool }`
//! - `set_rtt_delta { delta_ms: u32 }`
//! - `get_status` → current `ConfigSnapshot` + hint telemetry
//! - `get_stats` → per-link telemetry (same JSON as the old `stats` command)
//! - `mark_critical { count: u32 }` — notification (no response required)
//!
//! JSON-RPC error codes follow the spec:
//! `-32700` parse error, `-32600` invalid request, `-32601` method not found,
//! `-32602` invalid params, `-32603` internal error. Anything above `-32000`
//! is reserved for future app-specific errors.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::DynamicConfig;
use crate::mode::SchedulingMode;
use crate::stats::SharedStats;

const JSONRPC_VERSION: &str = "2.0";

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

#[derive(Debug, Deserialize)]
struct Request {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorObject>,
    id: Value,
}

#[derive(Debug, Serialize)]
struct ErrorObject {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl ErrorObject {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl Response {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            result: Some(result),
            error: None,
            id,
        }
    }

    fn err(id: Value, err: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            result: None,
            error: Some(err),
            id,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            // Serializing our own Response type cannot realistically fail,
            // but we never want to panic in the control plane.
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"response serialization failed"},"id":null}"#.to_string()
        })
    }
}

/// Dispatch one JSON-RPC request. Returns `None` for notifications (no
/// response to send).
pub fn dispatch(
    config: &DynamicConfig,
    stats: Option<&SharedStats>,
    line: &str,
) -> Option<Response> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            // No id available — reply with null per spec.
            return Some(Response::err(
                Value::Null,
                ErrorObject {
                    code: PARSE_ERROR,
                    message: "parse error".into(),
                    data: Some(Value::String(e.to_string())),
                },
            ));
        }
    };

    if req.jsonrpc != JSONRPC_VERSION {
        return req.id.map(|id| {
            Response::err(
                id,
                ErrorObject::new(INVALID_REQUEST, "jsonrpc version must be \"2.0\""),
            )
        });
    }

    let is_notification = req.id.is_none();
    let id_for_response = req.id.clone().unwrap_or(Value::Null);
    let result = handle_method(config, stats, &req.method, &req.params);

    if is_notification {
        return None;
    }

    Some(match result {
        Ok(value) => Response::ok(id_for_response, value),
        Err(err) => Response::err(id_for_response, err),
    })
}

fn handle_method(
    config: &DynamicConfig,
    stats: Option<&SharedStats>,
    method: &str,
    params: &Value,
) -> Result<Value, ErrorObject> {
    match method {
        "set_mode" => {
            let mode_str = params
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| ErrorObject::new(INVALID_PARAMS, "expected params.mode: string"))?;
            let mode = parse_mode(mode_str)?;
            config.set_mode(mode);
            Ok(json!({ "mode": mode.to_string() }))
        }

        "set_quality" => {
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| ErrorObject::new(INVALID_PARAMS, "expected params.enabled: bool"))?;
            config.set_quality_enabled(enabled);
            Ok(json!({ "enabled": enabled }))
        }

        "set_exploration" => {
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| ErrorObject::new(INVALID_PARAMS, "expected params.enabled: bool"))?;
            config.set_exploration_enabled(enabled);
            Ok(json!({ "enabled": enabled }))
        }

        "set_rtt_delta" => {
            let delta = params
                .get("delta_ms")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    ErrorObject::new(INVALID_PARAMS, "expected params.delta_ms: u32")
                })?;
            config.set_rtt_delta_ms(delta);
            Ok(json!({ "delta_ms": delta }))
        }

        "get_status" => {
            let snap = config.snapshot();
            Ok(json!({
                "mode": snap.mode.to_string(),
                "quality_enabled": snap.quality_enabled,
                "exploration_enabled": snap.exploration_enabled,
                "rtt_delta_ms": snap.rtt_delta_ms,
                "critical_hints_total": config.critical_hints_total(),
                "critical_hint_remaining": config.critical_hint_remaining(),
            }))
        }

        "get_stats" => {
            let stats = stats.ok_or_else(|| {
                ErrorObject::new(INTERNAL_ERROR, "stats provider not registered")
            })?;
            let json_str = stats.to_json();
            serde_json::from_str(&json_str).map_err(|e| ErrorObject {
                code: INTERNAL_ERROR,
                message: "failed to re-parse stats JSON".into(),
                data: Some(Value::String(e.to_string())),
            })
        }

        "mark_critical" => {
            let count = params
                .get("count")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| ErrorObject::new(INVALID_PARAMS, "expected params.count: u32"))?;
            config.add_critical_hint(count);
            Ok(json!({ "remaining": config.critical_hint_remaining() }))
        }

        // Reserved for the future streaming API. A subscription-capable
        // control plane will replace these returning METHOD_NOT_FOUND with
        // a persistent-connection impl. Reserving the names now so clients
        // written against the current protocol don't collide with built-in
        // methods when the streaming upgrade lands.
        "subscribe" | "unsubscribe" => Err(ErrorObject::new(
            METHOD_NOT_FOUND,
            format!("{method} is reserved for a future streaming protocol, not yet implemented"),
        )),

        other => Err(ErrorObject::new(
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        )),
    }
}

fn parse_mode(s: &str) -> Result<SchedulingMode, ErrorObject> {
    match s {
        "classic" => Ok(SchedulingMode::Classic),
        "enhanced" => Ok(SchedulingMode::Enhanced),
        "rtt-threshold" => Ok(SchedulingMode::RttThreshold),
        "edpf" => Ok(SchedulingMode::Edpf),
        other => Err(ErrorObject::new(
            INVALID_PARAMS,
            format!(
                "unknown mode '{other}': use classic, enhanced, rtt-threshold, or edpf"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_returns_jsonrpc_error() {
        let config = DynamicConfig::new();
        let resp = dispatch(&config, None, "not valid json").unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        assert_eq!(v["error"]["code"], PARSE_ERROR);
        assert_eq!(v["id"], Value::Null);
    }

    #[test]
    fn notification_returns_none() {
        let config = DynamicConfig::new();
        let req = r#"{"jsonrpc":"2.0","method":"mark_critical","params":{"count":5}}"#;
        assert!(dispatch(&config, None, req).is_none());
        assert_eq!(config.critical_hint_remaining(), 5);
    }

    #[test]
    fn set_mode_happy_path() {
        let config = DynamicConfig::new();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"set_mode","params":{"mode":"classic"}}"#;
        let resp = dispatch(&config, None, req).unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        assert_eq!(v["result"]["mode"], "classic");
        assert_eq!(v["id"], 1);
        assert_eq!(config.mode(), SchedulingMode::Classic);
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let config = DynamicConfig::new();
        let req = r#"{"jsonrpc":"2.0","id":"abc","method":"noop"}"#;
        let resp = dispatch(&config, None, req).unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["id"], "abc");
    }

    #[test]
    fn invalid_params_returns_invalid_params() {
        let config = DynamicConfig::new();
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"set_rtt_delta","params":{}}"#;
        let resp = dispatch(&config, None, req).unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn wrong_jsonrpc_version_rejects() {
        let config = DynamicConfig::new();
        let req = r#"{"jsonrpc":"1.0","id":1,"method":"get_status"}"#;
        let resp = dispatch(&config, None, req).unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn get_status_returns_all_fields() {
        let config = DynamicConfig::new();
        config.add_critical_hint(3);
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"get_status"}"#;
        let resp = dispatch(&config, None, req).unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        let result = &v["result"];
        assert!(result["mode"].is_string());
        assert!(result["quality_enabled"].is_boolean());
        assert!(result["exploration_enabled"].is_boolean());
        assert!(result["rtt_delta_ms"].is_number());
        assert_eq!(result["critical_hint_remaining"], 3);
        assert_eq!(result["critical_hints_total"], 1);
    }

    #[test]
    fn mark_critical_returns_remaining_when_called_with_id() {
        let config = DynamicConfig::new();
        let req = r#"{"jsonrpc":"2.0","id":9,"method":"mark_critical","params":{"count":4}}"#;
        let resp = dispatch(&config, None, req).unwrap();
        let v: Value = serde_json::from_str(&resp.to_json()).unwrap();
        assert_eq!(v["result"]["remaining"], 4);
    }
}

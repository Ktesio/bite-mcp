//! Wire protocol between the Rust control plane and the native Swift helper.
//!
//! Line-delimited JSON over the helper's stdio:
//!
//! ```text
//! helper -> host : {"method":"hello","params":{"protocol":1,"version":"0.1.0","capabilities":[...]}}
//! host -> helper : {"id":N,"method":"app.verb","params":{...}}
//! helper -> host : {"id":N,"result":{...}}
//!                | {"id":N,"error":{"code":"...","message":"...","app":"...","fix":"..."}}
//!                | {"method":"log","params":{...}}          (notification)
//! ```
//!
//! Spec: docs/protocol.md.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// App-level or transport-level error, normalized across the bridge.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct BridgeError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl BridgeError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            app: None,
            fix: None,
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new("timeout", message)
    }

    pub fn helper_exited() -> Self {
        Self::new(
            "helper_exited",
            "the native helper process exited unexpectedly",
        )
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new("protocol_error", message)
    }

    pub fn spawn(message: impl Into<String>) -> Self {
        Self::new("helper_spawn_failed", message)
    }

    /// True when the failure is a macOS permission denial the user can fix.
    pub fn is_permission(&self) -> bool {
        self.code == "permission_denied" || self.code == "fda_required"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hello {
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// One message arriving from the helper.
#[derive(Debug)]
pub enum FromHelper {
    Hello(Hello),
    Response {
        id: i64,
        result: Option<Value>,
        error: Option<BridgeError>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

pub fn encode_request(id: i64, method: &str, params: &Value) -> String {
    let line = serde_json::json!({ "id": id, "method": method, "params": params });
    let mut s = line.to_string();
    s.push('\n');
    s
}

/// Parse one NDJSON line from the helper. Returns `Ok(None)` for unparseable
/// lines (they are logged, never fatal).
pub fn decode_line(line: &str) -> Result<Option<FromHelper>, BridgeError> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| BridgeError::protocol(format!("unparseable helper line: {e}")))?;
    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        if method == "hello" {
            let hello: Hello =
                serde_json::from_value(v.get("params").cloned().unwrap_or(Value::Null))
                    .map_err(|e| BridgeError::protocol(format!("bad hello: {e}")))?;
            return Ok(Some(FromHelper::Hello(hello)));
        }
        return Ok(Some(FromHelper::Notification {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        }));
    }
    let id = v
        .get("id")
        .and_then(|i| i.as_i64())
        .ok_or_else(|| BridgeError::protocol("response without id"))?;
    let result = v.get("result").cloned().filter(|r| !r.is_null());
    let error = v
        .get("error")
        .and_then(|e| serde_json::from_value(e.clone()).ok());
    Ok(Some(FromHelper::Response { id, result, error }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_request_line() {
        let line = encode_request(3, "sys.ping", &serde_json::json!({}));
        assert!(line.starts_with(r#"{"id":3,"method":"sys.ping","params":{}"#));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn decodes_hello() {
        let line = r#"{"method":"hello","params":{"protocol":1,"version":"0.1.0","capabilities":["calendar"]}}"#;
        match decode_line(line).unwrap().unwrap() {
            FromHelper::Hello(h) => {
                assert_eq!(h.protocol, 1);
                assert_eq!(h.capabilities, vec!["calendar"]);
            }
            _ => panic!("expected hello"),
        }
    }

    #[test]
    fn decodes_response_and_error() {
        let ok = decode_line(r#"{"id":1,"result":{"pong":true}}"#)
            .unwrap()
            .unwrap();
        match ok {
            FromHelper::Response { id, result, error } => {
                assert_eq!(id, 1);
                assert_eq!(result.unwrap()["pong"], serde_json::json!(true));
                assert!(error.is_none());
            }
            _ => panic!(),
        }
        let err = decode_line(
            r#"{"id":2,"error":{"code":"permission_denied","message":"denied","app":"Calendar","fix":"open x"}}"#,
        )
        .unwrap()
        .unwrap();
        match err {
            FromHelper::Response { id, result, error } => {
                assert_eq!(id, 2);
                assert!(result.is_none());
                let e = error.unwrap();
                assert!(e.is_permission());
                assert_eq!(e.app.as_deref(), Some("Calendar"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn decodes_log_notification() {
        let n = decode_line(r#"{"method":"log","params":{"level":"info","message":"hi"}}"#)
            .unwrap()
            .unwrap();
        match n {
            FromHelper::Notification { method, .. } => assert_eq!(method, "log"),
            _ => panic!(),
        }
    }
}

//! Newline-delimited JSON-RPC 2.0 framing. One message per line, no batching.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC id. ACP peers number their own requests independently, so an id
/// only means something together with the direction it travelled.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Num(i64),
    Str(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub const METHOD_NOT_FOUND: i64 = -32601;

    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: message.into(),
            data: None,
        }
    }
}

/// Every inbound line decodes to one of these.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request {
        id: Id,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Id,
        result: Result<Value, RpcError>,
    },
}

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    id: Option<Id>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("message has neither method nor result/error")]
    Shape,
}

pub fn decode(line: &str) -> Result<Message, DecodeError> {
    let w: Wire = serde_json::from_str(line)?;
    match (w.id, w.method) {
        (Some(id), Some(method)) => Ok(Message::Request {
            id,
            method,
            params: w.params.unwrap_or(Value::Null),
        }),
        (None, Some(method)) => Ok(Message::Notification {
            method,
            params: w.params.unwrap_or(Value::Null),
        }),
        (Some(id), None) => match (w.result, w.error) {
            (_, Some(e)) => Ok(Message::Response { id, result: Err(e) }),
            (Some(r), None) => Ok(Message::Response { id, result: Ok(r) }),
            (None, None) => Ok(Message::Response {
                id,
                result: Ok(Value::Null),
            }),
        },
        (None, None) => Err(DecodeError::Shape),
    }
}

pub fn encode_request(id: &Id, method: &str, params: &impl Serialize) -> String {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params
    }))
    .expect("serializable request")
}

pub fn encode_notification(method: &str, params: &impl Serialize) -> String {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0", "method": method, "params": params
    }))
    .expect("serializable notification")
}

pub fn encode_response(id: &Id, result: Result<Value, RpcError>) -> String {
    let body = match result {
        Ok(v) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(e) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    };
    serde_json::to_string(&body).expect("serializable response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::types::{SessionUpdate, SessionUpdateParams};

    /// Strip the `C> ` / `S> ` prefixes the Phase 0 captures carry.
    fn capture_lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
        text.lines().filter_map(|l| {
            let l = l.trim();
            l.split_once("> ")
                .filter(|(dir, _)| *dir == "C" || *dir == "S")
        })
    }

    const CAPTURES: [&str; 2] = [
        include_str!("../../../docs/reference/acp-omp-18.0.4-session.jsonl"),
        include_str!("../../../docs/reference/acp-omp-restricted-session.jsonl"),
    ];

    #[test]
    fn every_captured_line_decodes() {
        let mut updates = 0;
        let mut requests = 0;
        for cap in CAPTURES {
            for (_dir, line) in capture_lines(cap) {
                let msg = decode(line).unwrap_or_else(|e| panic!("{e}: {line}"));
                match msg {
                    Message::Notification { method, params } if method == "session/update" => {
                        let p: SessionUpdateParams = serde_json::from_value(params).unwrap();
                        assert!(
                            !matches!(p.update, SessionUpdate::Other(_)),
                            "unmodelled update in capture: {line}"
                        );
                        updates += 1;
                    }
                    Message::Request { .. } => requests += 1,
                    _ => {}
                }
            }
        }
        assert!(updates > 200, "expected the captures to carry updates");
        assert!(requests > 5, "expected permission and fs requests");
    }

    #[test]
    fn unknown_update_variant_is_kept_whole() {
        let p: SessionUpdateParams = serde_json::from_value(serde_json::json!({
            "sessionId": "s", "update": {"sessionUpdate": "brand_new_thing", "x": 1}
        }))
        .unwrap();
        match p.update {
            SessionUpdate::Other(v) => assert_eq!(v["sessionUpdate"], "brand_new_thing"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn response_shapes() {
        assert_eq!(
            decode(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#).unwrap(),
            Message::Response {
                id: Id::Num(4),
                result: Ok(serde_json::json!({}))
            }
        );
        assert!(matches!(
            decode(r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"no"}}"#).unwrap(),
            Message::Response { result: Err(_), .. }
        ));
        assert!(decode(r#"{"jsonrpc":"2.0"}"#).is_err());
    }

    #[test]
    fn permission_outcome_nests_twice() {
        let s = encode_response(
            &Id::Num(0),
            Ok(
                serde_json::to_value(crate::acp::types::RequestPermissionResult {
                    outcome: crate::acp::types::PermissionOutcome::Selected {
                        option_id: "allow_once".into(),
                    },
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            s,
            r#"{"id":0,"jsonrpc":"2.0","result":{"outcome":{"optionId":"allow_once","outcome":"selected"}}}"#
        );
    }
}

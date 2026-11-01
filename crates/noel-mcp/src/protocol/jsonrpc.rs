//! JSON-RPC 2.0, as upstream's `jsonrpc.ts`: the message shapes, the
//! standard error codes, and the discriminators that tell a request,
//! notification and response apart — a response without `result` must
//! carry a well-formed `error`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC id: a string or a finite number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

/// One JSON-RPC 2.0 message: a request, a notification, or a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    Success {
        jsonrpc: String,
        id: JsonRpcId,
        result: Value,
    },
    Error {
        jsonrpc: String,
        id: JsonRpcId,
        error: JsonRpcErrorObject,
    },
}

impl JsonRpcResponse {
    /// The response's id, whichever shape it took.
    pub fn id(&self) -> &JsonRpcId {
        match self {
            Self::Success { id, .. } | Self::Error { id, .. } => id,
        }
    }
}

impl JsonRpcMessage {
    /// Parses one message, rejecting anything that is not a request,
    /// notification or well-formed response (upstream `parseJsonRpcMessage`).
    pub fn parse(value: Value) -> Result<Self, McpParseError> {
        let object = value.as_object().ok_or(McpParseError::InvalidMessage)?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(McpParseError::InvalidMessage);
        }
        let has_id = object.contains_key("id");
        let id = object.get("id").and_then(parse_id);
        let method = object.get("method").and_then(Value::as_str);
        match (has_id, id, method) {
            // A request: an id and a method.
            (true, Some(id), Some(method)) => Ok(Self::Request(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id,
                method: method.to_string(),
                params: object.get("params").cloned(),
            })),
            // A notification: a method and no id at all.
            (false, _, Some(method)) => Ok(Self::Notification(JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params: object.get("params").cloned(),
            })),
            // A response: an id, and either a result (and no error) or a
            // well-formed error.
            (true, Some(id), None) => {
                if let Some(result) = object.get("result") {
                    if object.contains_key("error") {
                        return Err(McpParseError::InvalidMessage);
                    }
                    return Ok(Self::Response(JsonRpcResponse::Success {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: result.clone(),
                    }));
                }
                if let Some(error) = object.get("error").and_then(Value::as_object) {
                    let (code, message) = (
                        error.get("code").and_then(Value::as_i64),
                        error.get("message").and_then(Value::as_str),
                    );
                    if let (Some(code), Some(message)) = (code, message) {
                        return Ok(Self::Response(JsonRpcResponse::Error {
                            jsonrpc: "2.0".to_string(),
                            id,
                            error: JsonRpcErrorObject {
                                code,
                                message: message.to_string(),
                                data: error.get("data").cloned(),
                            },
                        }));
                    }
                }
                Err(McpParseError::InvalidMessage)
            }
            _ => Err(McpParseError::InvalidMessage),
        }
    }
}

fn parse_id(value: &Value) -> Option<JsonRpcId> {
    match value {
        Value::Number(number) if number.is_i64() => {
            Some(JsonRpcId::Number(number.as_i64().unwrap_or_default()))
        }
        Value::String(text) => Some(JsonRpcId::String(text.clone())),
        _ => None,
    }
}

/// Why a value is not a JSON-RPC message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpParseError {
    #[error("invalid JSON-RPC message")]
    InvalidMessage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_notifications_and_responses_parse() {
        let request =
            JsonRpcMessage::parse(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
                .unwrap();
        assert!(matches!(request, JsonRpcMessage::Request(_)));
        let notification =
            JsonRpcMessage::parse(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .unwrap();
        assert!(matches!(notification, JsonRpcMessage::Notification(_)));
        let response =
            JsonRpcMessage::parse(json!({"jsonrpc": "2.0", "id": "abc", "result": {"tools": []}}))
                .unwrap();
        assert!(matches!(
            response,
            JsonRpcMessage::Response(JsonRpcResponse::Success { .. })
        ));
        let error = JsonRpcMessage::parse(json!({
            "jsonrpc": "2.0", "id": 2,
            "error": {"code": METHOD_NOT_FOUND, "message": "nope"}
        }))
        .unwrap();
        assert!(matches!(
            error,
            JsonRpcMessage::Response(JsonRpcResponse::Error { .. })
        ));
    }

    #[test]
    fn malformed_messages_are_rejected() {
        // No jsonrpc version.
        assert!(JsonRpcMessage::parse(json!({"id": 1, "method": "x"})).is_err());
        // A response with both result and error.
        assert!(JsonRpcMessage::parse(
            json!({"jsonrpc": "2.0", "id": 1, "result": {}, "error": {"code": 1, "message": "x"}})
        )
        .is_err());
        // An error without a message.
        assert!(
            JsonRpcMessage::parse(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": 1}}))
                .is_err()
        );
        // An id that is neither a string nor a number.
        assert!(
            JsonRpcMessage::parse(json!({"jsonrpc": "2.0", "id": null, "result": {}})).is_err()
        );
    }

    #[test]
    fn messages_round_trip_through_serde() {
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(7),
            method: "tools/call".to_string(),
            params: Some(json!({"name": "search"})),
        });
        let text = serde_json::to_string(&message).unwrap();
        assert_eq!(
            text,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search"}}"#
        );
        let back = JsonRpcMessage::parse(serde_json::from_str(&text).unwrap()).unwrap();
        assert_eq!(back, message);
    }
}

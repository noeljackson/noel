//! The initialize handshake's types and the tool listing, as upstream's
//! `types.ts`. Only what noel's client reads is typed; everything else a
//! server sends stays JSON it never inspects.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The client or server's name and version, from the handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// What a server says it can do; noel reads only `tools`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// The handshake's answer: the version the server chose, its
/// capabilities, its name, and optional instructions for the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// One tool a server lists: its name, JSON-schema input, and a
/// description the model reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_results_parse_with_camel_case() {
        let result: InitializeResult = serde_json::from_value(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "everything", "version": "1.0.0"},
            "instructions": "be careful"
        }))
        .unwrap();
        assert_eq!(result.protocol_version, "2025-06-18");
        assert_eq!(result.server_info.name, "everything");
        assert_eq!(
            result
                .capabilities
                .tools
                .and_then(|tools| tools.list_changed),
            Some(true)
        );
        assert_eq!(result.instructions.as_deref(), Some("be careful"));
    }

    #[test]
    fn tools_parse_the_minimum_shape() {
        let tool: Tool = serde_json::from_value(json!({
            "name": "search",
            "inputSchema": {"type": "object"}
        }))
        .unwrap();
        assert_eq!(tool.name, "search");
        assert!(tool.description.is_none());
    }
}

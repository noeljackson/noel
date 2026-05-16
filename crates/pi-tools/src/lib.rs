use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub read_only: bool,
}

pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    ["read", "bash", "edit", "write", "grep", "find", "ls"]
        .into_iter()
        .map(|name| ToolDefinition {
            name: name.to_string(),
            read_only: matches!(name, "read" | "grep" | "find" | "ls"),
        })
        .collect()
}

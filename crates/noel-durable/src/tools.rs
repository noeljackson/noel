//! In-process Rust tools: a program's own tools, offered to the model
//! beside noel's built-ins and executed in-process (the plugin-tool
//! surface, which runs a host's calls in the runtime itself).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use noel_core::plugins::{PluginTool, PluginToolHost, PluginToolOutput};

/// What a tool call runs with.
pub struct ToolContext<'a> {
    /// The arguments the model sent, parsed.
    pub arguments: &'a Value,
    /// The session's working directory.
    pub cwd: &'a std::path::Path,
}

/// A tool's result: `output` for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(output: impl Into<String>) -> ToolResult {
        ToolResult {
            output: output.into(),
            is_error: false,
        }
    }

    pub fn error(output: impl Into<String>) -> ToolResult {
        ToolResult {
            output: output.into(),
            is_error: true,
        }
    }
}

/// An in-process Rust tool (Durable's `defineTool`): a name, a JSON
/// schema for its parameters, and an async execute.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// The JSON schema of the tool's parameters.
    fn parameters(&self) -> Value;
    /// Whether a call cut off by a stop may run again when the turn
    /// continues. Default false, the safe choice: a tool that is not
    /// idempotent must not run twice.
    fn replay_safe(&self) -> bool {
        false
    }
    fn execute<'a>(
        &'a self,
        context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
}

/// The tools a harness offers.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> ToolRegistry {
        ToolRegistry::default()
    }

    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub(crate) fn install(self, runtime: &mut noel_core::Runtime) {
        if self.tools.is_empty() {
            return;
        }
        let host = Arc::new(RegistryHost { tools: self.tools });
        let tools: Vec<PluginTool> = host
            .tools
            .iter()
            .map(|tool| PluginTool {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters(),
                plugin: "sdk".to_owned(),
                replay_safe: tool.replay_safe(),
            })
            .collect();
        runtime.set_plugin_tools(host, tools);
    }
}

struct RegistryHost {
    tools: Vec<Arc<dyn Tool>>,
}

impl PluginToolHost for RegistryHost {
    fn call(
        &self,
        _plugin: &str,
        name: &str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = PluginToolOutput> + Send>> {
        let name = name.to_owned();
        let Some(tool) = self.tools.iter().find(|candidate| candidate.name() == name) else {
            return Box::pin(async move {
                PluginToolOutput {
                    output: format!("tool {name} is not registered"),
                    is_error: true,
                }
            });
        };
        let tool = tool.clone();
        Box::pin(async move {
            let result = tool
                .execute(ToolContext {
                    arguments: &args,
                    cwd: std::path::Path::new(""),
                })
                .await;
            PluginToolOutput {
                output: result.output,
                is_error: result.is_error,
            }
        })
    }
}

/// Names to keep alive for the docs.
#[allow(dead_code)]
type Unused = BTreeMap<String, Value>;

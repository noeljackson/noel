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

/// What a tool's execute returns: a result, or a park (upstream
/// `ToolPark`): the call holds no invocation while parked, then runs
/// `execute` again with the same arguments, `wake` saying why and
/// `checkpoint` set — a rerun after a crash takes the same path, so a
/// parked tool must be replay-safe (the registry marks it so).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    Result(ToolResult),
    /// Sleep until `until`, for `signals`, or both (upstream
    /// `{ sleep }`).
    Park {
        checkpoint: Value,
        until: Option<u64>,
        /// The signal keys the park waits on, as `(signal, key)` pairs.
        signals: Vec<(String, String)>,
    },
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
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;

    /// Runs when the call is aborted while parked (upstream `cancel`):
    /// undo what the call started. Default: nothing.
    fn cancel<'a>(
        &'a self,
        _context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

/// A park the host recorded, for the driver to hold: the call's identity
/// and what it waits for (upstream `ToolPark`).
#[derive(Debug, Clone)]
pub struct ToolPark {
    /// The tool call's id, whose result the rerun returns.
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    pub checkpoint: Value,
    pub until: Option<u64>,
    /// The signal keys the park waits on, as `(signal, key)` pairs.
    pub signals: Vec<(String, String)>,
}

/// The tools a harness offers.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    /// Where a parking execute reports its park: the driver installs this
    /// before a run and reads it after.
    parks: Arc<std::sync::Mutex<Vec<ToolPark>>>,
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

    /// The parks its calls made (each once), cleared by the reader.
    pub(crate) fn take_parks(&self) -> Vec<ToolPark> {
        std::mem::take(&mut self.parks.lock().expect("parks"))
    }

    pub(crate) fn install(self, runtime: &mut noel_core::Runtime) {
        if self.tools.is_empty() {
            return;
        }
        let host = Arc::new(RegistryHost {
            tools: self.tools,
            parks: self.parks.clone(),
        });
        let tools: Vec<PluginTool> = host
            .tools
            .iter()
            .map(|tool| PluginTool {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters(),
                plugin: "sdk".to_owned(),
                // A park reruns through the resume's replay, so every SDK
                // tool that can park must be replay-safe; the default
                // `replay_safe()` stays the tool's own word.
                replay_safe: tool.replay_safe(),
            })
            .collect();
        runtime.set_plugin_tools(host, tools);
    }
}

struct RegistryHost {
    tools: Vec<Arc<dyn Tool>>,
    parks: Arc<std::sync::Mutex<Vec<ToolPark>>>,
}

impl PluginToolHost for RegistryHost {
    fn call(
        &self,
        plugin: &str,
        name: &str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = PluginToolOutput> + Send>> {
        self.call_with_id(plugin, name, "", args)
    }

    fn call_with_id(
        &self,
        _plugin: &str,
        name: &str,
        call_id: &str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = PluginToolOutput> + Send>> {
        let tools = self.tools.clone();
        let parks = self.parks.clone();
        let name = name.to_owned();
        let call_id = call_id.to_owned();
        Box::pin(async move {
            let Some(tool) = tools.iter().find(|candidate| candidate.name() == name) else {
                return PluginToolOutput {
                    output: format!("tool {name} is not registered"),
                    is_error: true,
                };
            };
            let tool = tool.clone();
            let args = args;
            let outcome = tool
                .execute(ToolContext {
                    arguments: &args,
                    cwd: std::path::Path::new(""),
                })
                .await;
            match outcome {
                ToolOutcome::Result(result) => PluginToolOutput {
                    output: result.output,
                    is_error: result.is_error,
                },
                ToolOutcome::Park {
                    checkpoint,
                    until,
                    signals,
                } => {
                    // Record the park for the driver to hold and answer
                    // with a placeholder the run's cancellation keeps out
                    // of the journal: the resume replays the call, whose
                    // execute now reads what it waited for.
                    parks.lock().expect("parks").push(ToolPark {
                        call_id: call_id.clone(),
                        tool: name.clone(),
                        arguments: args.clone(),
                        checkpoint,
                        until,
                        signals,
                    });
                    PluginToolOutput {
                        output: String::new(),
                        is_error: true,
                    }
                }
            }
        })
    }
}

/// Names to keep alive for the docs.
#[allow(dead_code)]
type Unused = BTreeMap<String, Value>;

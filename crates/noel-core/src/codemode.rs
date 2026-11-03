//! The built-in `codemode` tool (upstream coding agent
//! `extensions/codemode`): the model writes JavaScript that calls the other
//! active tools, run on the djs engine by `noel-codemode`.
//!
//! Off by default, as upstream registers it inactive: it is on only when
//! selected (`--tools`, `enabledTools`, `defaultTools: ["+codemode"]`).
//! Nested calls go through the same permission gate as the model's own and
//! run in parallel; they are not added to the conversation. The script's
//! `store()` is saved with the session. Differences from upstream are in the
//! non-parity register ("Codemode").

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use noel_ai::{ChatToolCall, ToolDefinition as AiToolDefinition};
use noel_codemode::execution::{
    execute, CallFuture, CallStatus, DeclaredTool, ExecuteOptions, Executor,
};
use noel_codemode::format::{format_result, Block, CallSummary, OutputFiles};
use noel_codemode::vm::CallTarget;
use noel_codemode::{parse_codemode_source, to_codemode_identifier};
use noel_env::ExecutionEnv;
use noel_tools::{execute_tool_streaming_in, ToolImage, ToolRequest, ToolRuntimeOptions};
use serde_json::{json, Value};

use super::{
    execute_extension_tool, extensions, model_tool_requests, AgentError, ExtensionTool, Runtime,
};

/// The tool's name.
pub(crate) const CODEMODE: &str = "codemode";

/// The VM's heap limit: overruns throw `InternalError: out of memory` in the
/// script (upstream `CODEMODE_MEMORY_LIMIT_BYTES`).
const MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Upstream's `DESCRIPTION_INTRO`, word for word.
const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// Upstream's globals lines, without the discovery globals and `models`,
/// which noel does not have.
const GLOBALS: &str = "Globals:
- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.
- `store(key, value)` and `load(key)` keep JSON values across codemode calls.";

/// The tools a script may call: the model's active tools, except codemode
/// itself and `todo` (which changes the session; model-only here).
pub(crate) fn callable(definitions: &[AiToolDefinition]) -> Vec<AiToolDefinition> {
    definitions
        .iter()
        .filter(|definition| definition.name != CODEMODE && definition.name != "todo")
        .cloned()
        .collect()
}

/// The tool's definition, listing the tools scripts can call.
pub(crate) fn definition(callable: &[AiToolDefinition]) -> AiToolDefinition {
    let mut description = format!("{DESCRIPTION_INTRO}\n\n{GLOBALS}");
    if !callable.is_empty() {
        description.push_str("\n\nNested tools:");
        for tool in callable {
            let summary = tool.description.lines().next().unwrap_or_default();
            description.push_str(&format!(
                "\n- `tools.{}(args)`: {summary}",
                to_codemode_identifier(&tool.name)
            ));
        }
    }
    AiToolDefinition {
        name: CODEMODE.to_owned(),
        description,
        parameters: json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Raw JavaScript source." }
            },
            "required": ["code"]
        }),
    }
}

/// What nested calls need, taken from the runtime before the script starts,
/// so they run in parallel without it.
struct Nested {
    env: Result<Arc<dyn ExecutionEnv>, String>,
    options: ToolRuntimeOptions,
    hosts: Arc<tokio::sync::Mutex<extensions::ExtensionHosts>>,
    cwd: PathBuf,
    extension_tools: BTreeMap<String, ExtensionTool>,
    parent_id: String,
    state: Mutex<NestedState>,
}

#[derive(Default)]
struct NestedState {
    next: usize,
    edited: Vec<String>,
    registrations: Vec<extensions::ExtensionRegistrationSet>,
}

impl Nested {
    fn state(&self) -> std::sync::MutexGuard<'_, NestedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One nested call: the gate, then the tool. A string result, or the
    /// tool's error text as the script's `Error`.
    async fn run(&self, name: &str, args: Option<Value>) -> Result<Option<Value>, String> {
        let arguments = args.unwrap_or_else(|| json!({})).to_string();
        let id = {
            let mut state = self.state();
            state.next += 1;
            format!("{}/{}", self.parent_id, state.next)
        };
        let call = ChatToolCall {
            id,
            name: name.to_owned(),
            arguments,
        };
        let gate = {
            let mut hosts = self.hosts.lock().await;
            hosts
                .gate_tool_call(&call.name, &call.arguments, &self.cwd)
                .await
        };
        if let Ok(decision) = gate {
            if !decision.allow {
                return Err(decision.reason);
            }
        }
        let (output, is_error) = match self.extension_tools.get(name) {
            Some(tool) if tool.protocol == "2" => {
                let mut hosts = self.hosts.lock().await;
                match hosts
                    .call(
                        &tool.extension_name,
                        "tool",
                        &tool.name,
                        &call.arguments,
                        &self.cwd,
                        Some(&call.id),
                    )
                    .await
                {
                    Ok(outcome) => {
                        if let Some(set) = outcome.registrations {
                            self.state().registrations.push(set);
                        }
                        (outcome.output, outcome.is_error)
                    }
                    Err(error) => (error, true),
                }
            }
            Some(tool) => match execute_extension_tool(&self.cwd, tool, &call).await {
                Ok(result) => (result.output, result.is_error),
                Err(error) => (error.to_string(), true),
            },
            None => self.builtin(&call).await,
        };
        if is_error {
            Err(if output.is_empty() {
                format!("Tool \"{name}\" failed")
            } else {
                output
            })
        } else {
            Ok(Some(Value::String(output)))
        }
    }

    async fn builtin(&self, call: &ChatToolCall) -> (String, bool) {
        let requests = match model_tool_requests(call) {
            Ok(requests) => requests,
            Err(error) => return (error.to_string(), true),
        };
        let env = match &self.env {
            Ok(env) => env.clone(),
            Err(message) => return (message.clone(), true),
        };
        let mut outputs = Vec::new();
        let mut is_error = false;
        for request in requests {
            let edited = match &request {
                ToolRequest::Edit { path, .. } | ToolRequest::Write { path, .. } => {
                    Some(path.clone())
                }
                _ => None,
            };
            let mut ignore = |_: noel_tools::OutputSoFar<'_>| {};
            match execute_tool_streaming_in(env.as_ref(), request, &self.options, &mut ignore).await
            {
                Ok(result) => {
                    is_error |= result.is_error;
                    if let (Some(path), false) = (edited, result.is_error) {
                        self.state().edited.push(path);
                    }
                    outputs.push(result.output);
                }
                Err(error) => {
                    is_error = true;
                    outputs.push(error.to_string());
                }
            }
        }
        (outputs.join("\n"), is_error)
    }
}

/// Records each nested call's name and status for the failure summary.
struct Recording<'a> {
    nested: &'a Nested,
    calls: Mutex<Vec<CallSummary>>,
}

impl Executor for Recording<'_> {
    fn call<'a>(
        &'a self,
        target: CallTarget,
        name: &'a str,
        args: Option<Value>,
    ) -> CallFuture<'a> {
        Box::pin(async move {
            if target == CallTarget::Global {
                return Err(format!("Unknown global \"{name}\""));
            }
            let index = {
                let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
                calls.push(CallSummary {
                    name: name.to_owned(),
                    status: "cancelled".to_owned(),
                });
                calls.len() - 1
            };
            let result = self.nested.run(name, args).await;
            let status = if result.is_ok() { "ok" } else { "error" };
            self.calls.lock().unwrap_or_else(PoisonError::into_inner)[index].status =
                status.to_owned();
            result
        })
    }
}

/// Spill and image files in the temp directory, as noel's tools write them.
struct TempFiles;

fn temp_path(extension: &str) -> PathBuf {
    use std::hash::{BuildHasher, Hasher};
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    std::env::temp_dir().join(format!("noel-codemode-{random:016x}{extension}"))
}

impl OutputFiles for TempFiles {
    fn write_text(&self, text: &str) -> Result<PathBuf, String> {
        let path = temp_path(".txt");
        std::fs::write(&path, text).map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn write_image(&self, bytes: &[u8], extension: &str) -> Result<PathBuf, String> {
        let path = temp_path(extension);
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
        Ok(path)
    }
}

/// The tool's output: its text, images, and whether it failed.
pub(crate) struct CodemodeOutput {
    pub output: String,
    pub images: Vec<ToolImage>,
    pub is_error: bool,
}

/// Runs a `codemode` call: parses the source, runs it with the callable
/// tools, saves the store's writes and the files the script edited, and
/// formats the result.
pub(crate) async fn run(
    runtime: &mut Runtime,
    tool_call: &ChatToolCall,
    callable: &[AiToolDefinition],
) -> Result<CodemodeOutput, AgentError> {
    let started = Instant::now();
    let failed = |message: String| CodemodeOutput {
        output: message,
        images: Vec::new(),
        is_error: true,
    };
    let code = match serde_json::from_str::<Value>(&tool_call.arguments) {
        Ok(Value::Object(fields)) => match fields.get("code") {
            Some(Value::String(code)) => code.clone(),
            _ => {
                return Ok(failed(
                    "invalid codemode arguments: `code` must be a string".to_owned(),
                ))
            }
        },
        _ => {
            return Ok(failed(
                "invalid codemode arguments: expected an object".to_owned(),
            ))
        }
    };
    let source = match parse_codemode_source(&code) {
        Ok(source) => source,
        Err(error) => return Ok(failed(error.0)),
    };
    let nested = Nested {
        env: runtime.tool_env(),
        options: super::tool_runtime_options(runtime, super::bash_session_env(runtime)),
        hosts: runtime.extension_hosts(),
        cwd: runtime.session.cwd.clone(),
        extension_tools: runtime.systems.extension_tools.clone(),
        parent_id: tool_call.id.clone(),
        state: Mutex::default(),
    };
    let tools: Vec<DeclaredTool> = callable
        .iter()
        .map(|tool| DeclaredTool {
            name: tool.name.clone(),
            js_name: None,
            description: tool.description.clone(),
        })
        .collect();
    let recording = Recording {
        nested: &nested,
        calls: Mutex::default(),
    };
    let options = ExecuteOptions {
        timeout: source
            .options
            .timeout_ms
            .map(std::time::Duration::from_millis),
        memory_limit_bytes: Some(MEMORY_LIMIT_BYTES),
        store: runtime.session.codemode_store.clone(),
    };
    let result = execute(&source.code, &tools, &[], &recording, options, None).await;

    let NestedState {
        edited,
        registrations,
        ..
    } = std::mem::take(&mut *nested.state());
    if !edited.is_empty() {
        runtime.note_edited_files(edited)?;
    }
    for set in registrations {
        runtime.apply_extension_registrations(set).await;
    }
    if let Ok(completed) = &result.outcome {
        let writes = &completed.store_writes;
        if !writes.set.is_empty() || !writes.delete.is_empty() {
            let mut store = runtime.session.codemode_store.clone();
            for key in &writes.delete {
                store.remove(key);
            }
            for (key, value) in &writes.set {
                store.insert(key.clone(), value.clone());
            }
            runtime.set_codemode_store(store)?;
        }
    }
    let mut calls = recording
        .calls
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    // A call the sandbox cancelled stays `cancelled` in the summary.
    for (summary, record) in calls.iter_mut().zip(&result.calls) {
        if record.status == CallStatus::Cancelled {
            summary.status = "cancelled".to_owned();
        }
    }
    let formatted = format_result(
        &result,
        &calls,
        source.options.max_output_tokens,
        started.elapsed().as_secs_f64(),
        &TempFiles,
    );
    let mut output = String::new();
    let mut images = Vec::new();
    for block in formatted.blocks {
        match block {
            Block::Text(text) => output.push_str(&text),
            Block::Image { data, mime_type } => images.push(ToolImage {
                mime_type,
                data_base64: data,
            }),
        }
    }
    Ok(CodemodeOutput {
        output,
        images,
        is_error: formatted.is_error,
    })
}

/// Whether `name` is selected in the session's tool scope (codemode is
/// never on by default).
pub(crate) fn selected(available: &BTreeSet<String>, disabled: &BTreeSet<String>) -> bool {
    available.contains(CODEMODE) && !disabled.contains(CODEMODE)
}

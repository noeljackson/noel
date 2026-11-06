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
    execute, AbortSignal, CallFuture, DeclaredGlobal, DeclaredTool, ExecuteOptions, Executor,
};
use noel_codemode::format::{format_result, Block, CallSummary, OutputFiles};
use noel_codemode::search::{create_tool_search_document, Bm25Ranker, DEFAULT_TOOL_SEARCH_LIMIT};
use noel_codemode::vm::CallTarget;
use noel_codemode::{
    parse_codemode_source, render_tool_output_type, render_tool_sample, to_codemode_identifier,
    ToolDeclaration,
};
use noel_env::ExecutionEnv;
use noel_tools::{execute_tool_streaming_in, ToolImage, ToolRequest, ToolRuntimeOptions};
use serde::Serialize;
use serde_json::{json, Value};

use super::{
    execute_extension_tool, extensions, model_tool_requests, AgentError, ExtensionTool, Runtime,
};

/// The tool's name.
pub(crate) const CODEMODE: &str = "codemode";

/// The VM's heap limit: overruns throw `InternalError: out of memory` in the
/// script (upstream `CODEMODE_MEMORY_LIMIT_BYTES`).
const MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Scripts that may run at once in this process, unless
/// `NOEL_CODEMODE_MAX_WORKERS` (or upstream's `PI_CODEMODE_MAX_WORKERS`)
/// names another positive number (upstream `DEFAULT_MAX_CODEMODE_WORKERS`).
const DEFAULT_MAX_WORKERS: usize = 2;
/// How long a script waits for a free slot (upstream
/// `CODEMODE_WORKER_WAIT_MS`).
const WORKER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn max_workers() -> usize {
    ["NOEL_CODEMODE_MAX_WORKERS", "PI_CODEMODE_MAX_WORKERS"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()?
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
        })
        .unwrap_or(DEFAULT_MAX_WORKERS)
}

/// The slots, sized when the first script asks for one.
fn workers() -> &'static (Arc<tokio::sync::Semaphore>, usize) {
    static WORKERS: std::sync::OnceLock<(Arc<tokio::sync::Semaphore>, usize)> =
        std::sync::OnceLock::new();
    WORKERS.get_or_init(|| {
        let max = max_workers();
        (Arc::new(tokio::sync::Semaphore::new(max)), max)
    })
}

/// Why a script did not start: no slot became free in time (upstream's
/// texts).
fn admission_failure(deadline_ms: Option<u64>, max: usize, waited_seconds: f64) -> String {
    let error = match deadline_ms {
        Some(deadline) if deadline <= WORKER_WAIT.as_millis() as u64 => format!(
            "Script timed out: Execution timed out after {deadline} ms while waiting for a free codemode worker"
        ),
        _ => format!(
            "No codemode worker became free within {} seconds. All {max} workers, shared by every session in this process, are held by other scripts. Retry after they finish.",
            WORKER_WAIT.as_secs()
        ),
    };
    format!(
        "Script failed\nWall time {} seconds\nOutput:\nScript error:\n{error}\n\nThe script did not start.",
        noel_codemode::format::to_fixed_1(waited_seconds)
    )
}

/// The reason an aborted script names: the message of the `AbortError` an
/// `AbortController.abort()` without a reason gives in Node.
const ABORT_REASON: &str = "This operation was aborted";

/// Upstream's `DESCRIPTION_INTRO`, word for word.
const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// Upstream's globals lines (`describeGlobals`), without `models`, which
/// noel does not have.
const GLOBALS: &str = "Globals:
- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.
- `store(key, value)` and `load(key)` keep JSON values across codemode calls.
- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.";

/// Characters per token when estimating the cost of a tool section.
const CHARS_PER_TOKEN: usize = 4;

/// The tools a script may call: the model's active tools, except codemode
/// itself and `todo` (which changes the session; model-only here).
pub(crate) fn callable(definitions: &[AiToolDefinition]) -> Vec<AiToolDefinition> {
    definitions
        .iter()
        .filter(|definition| definition.name != CODEMODE && definition.name != "todo")
        .cloned()
        .collect()
}

/// How codemode presents the tools (upstream's `codemode` settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// `codemode.mode: "only"`: codemode lists every callable tool and the
    /// others are not declared. Otherwise (`"on"`) every tool stays
    /// declared and says how scripts call it.
    pub only: bool,
    /// `codemode.inlineBudget`: estimated tokens for the listed tools.
    pub inline_budget: u64,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            only: false,
            inline_budget: 3000,
        }
    }
}

impl Presentation {
    pub fn from_settings(settings: Option<&noel_config::CodemodeSettings>) -> Self {
        let only = settings.and_then(|s| s.mode.as_deref()) == Some("only");
        let inline_budget = settings
            .and_then(|s| s.inline_budget.as_ref())
            .and_then(serde_json::Number::as_f64)
            .filter(|budget| budget.is_finite() && *budget >= 0.0)
            .map_or(Self::default().inline_budget, |budget| {
                budget.floor() as u64
            });
        Self {
            only,
            inline_budget,
        }
    }
}

/// What a nested call resolves to, as upstream's tools declare it: `bash`
/// and `read` have output schemas; every other tool resolves to its text.
pub(crate) fn output_schema(name: &str) -> Value {
    match name {
        "bash" => json!({
            "type": "object",
            "properties": {
                "output": { "type": "string", "description": "Combined stdout and stderr, possibly truncated" },
                "truncated": { "type": "boolean" },
                "full_output_path": { "type": "string", "description": "Full output, when truncated" },
                "exit_code": { "type": "number" },
                "wall_time_seconds": { "type": "number" }
            },
            "required": ["output", "truncated", "exit_code", "wall_time_seconds"]
        }),
        "read" => json!({
            "anyOf": [
                { "type": "string" },
                {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "const": "image" },
                        "data": { "type": "string" },
                        "mimeType": { "type": "string" },
                        "note": { "type": "string" }
                    },
                    "required": ["type", "data", "mimeType", "note"]
                }
            ]
        }),
        _ => json!({ "type": "string" }),
    }
}

/// The order upstream declares an output schema's properties in, where
/// serde_json's map sorts them.
fn property_order(name: &str) -> &'static [&'static str] {
    match name {
        "bash" => &[
            "output",
            "truncated",
            "full_output_path",
            "exit_code",
            "wall_time_seconds",
        ],
        "read" => &["type", "data", "mimeType", "note"],
        _ => &[],
    }
}

/// What a script sees of a tool (upstream `toCodemodeDeclaration`): its
/// description followed by its prompt guidelines.
pub(crate) fn declaration(tool: &AiToolDefinition, guidelines: &[String]) -> ToolDeclaration {
    let bullets: Vec<String> = guidelines
        .iter()
        .map(|guideline| guideline.trim())
        .filter(|guideline| !guideline.is_empty())
        .map(|guideline| format!("- {guideline}"))
        .collect();
    let description = if bullets.is_empty() {
        tool.description.clone()
    } else {
        format!("{}\n\n{}", tool.description.trim(), bullets.join("\n"))
    };
    ToolDeclaration {
        name: tool.name.clone(),
        description: Some(description),
        input_schema: Some(tool.parameters.clone()),
        output_schema: Some(output_schema(&tool.name)),
        signature: None,
    }
}

/// `### `id` (`raw name`)` followed by the tool's sample.
fn tool_section(declaration: &ToolDeclaration) -> String {
    let id = to_codemode_identifier(&declaration.name);
    let heading = if id == declaration.name {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", declaration.name)
    };
    format!(
        "{heading}\n{}",
        render_tool_sample(declaration, None).trim()
    )
}

/// Upstream `createCodemodeDescription` for tools without namespaces: the
/// introduction, the globals, and the sections of the listed tools that fit
/// `inline_budget` (cheapest first; `None` lists all).
pub(crate) fn description(listed: &[ToolDeclaration], inline_budget: Option<u64>) -> String {
    let mut sections = vec![DESCRIPTION_INTRO.to_string(), GLOBALS.to_string()];
    if listed.is_empty() {
        return sections.join("\n\n");
    }
    let entries: Vec<(String, u64)> = listed
        .iter()
        .map(|declaration| {
            let section = tool_section(declaration);
            let cost = section.encode_utf16().count().div_ceil(CHARS_PER_TOKEN) as u64;
            (section, cost)
        })
        .collect();
    let shown: Vec<bool> = match inline_budget {
        None => vec![true; entries.len()],
        Some(budget) => {
            let mut order: Vec<usize> = (0..entries.len()).collect();
            order.sort_by_key(|&index| entries[index].1);
            let mut shown = vec![false; entries.len()];
            let mut remaining = budget;
            for index in order {
                if entries[index].1 > remaining {
                    break;
                }
                remaining -= entries[index].1;
                shown[index] = true;
            }
            shown
        }
    };
    let mut tool_sections = vec!["Nested tools:".to_string()];
    tool_sections.extend(
        entries
            .into_iter()
            .zip(shown)
            .filter(|(_, shown)| *shown)
            .map(|((section, _), _)| section),
    );
    sections.push(tool_sections.join("\n\n"));
    sections.join("\n\n")
}

/// The subschema the local `$ref` chain of `schema` names in `root`;
/// remote, missing and cyclic references stop at the schema holding them
/// (upstream `followRef`, 6d3e342b).
fn follow_ref<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    let mut current = schema;
    let mut seen: Vec<*const Value> = Vec::new();
    while let Some(Value::String(reference)) = current.get("$ref") {
        let pointer: *const Value = current;
        if seen.contains(&pointer) {
            break;
        }
        let Some(target) = noel_codemode::declarations::resolve_schema_ref(reference, root) else {
            break;
        };
        seen.push(pointer);
        current = target;
    }
    current
}

/// The field names of an object schema with the fields of object fields
/// `depth` levels deep and the value of `const` fields (upstream
/// `describeFields`); local `$ref`s resolve against `root`.
fn describe_fields(schema: &Value, root: &Value, depth: usize, order: &[&str]) -> Option<String> {
    let object = follow_ref(schema, root).as_object()?;
    if object.get("type") != Some(&Value::String("object".into())) {
        return None;
    }
    let properties = object.get("properties")?.as_object()?;
    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|required| required.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort_by_key(|name| order.iter().position(|o| o == name).unwrap_or(usize::MAX));
    let fields: Vec<String> = names
        .into_iter()
        .map(|name| {
            let field = &properties[name.as_str()];
            let nested = match follow_ref(field, root).get("const") {
                Some(constant) => Some(constant.to_string()),
                None if depth > 0 => describe_fields(field, root, depth - 1, &[]),
                None => None,
            };
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            match nested {
                Some(nested) => format!("{name}{optional}: {nested}"),
                None => format!("{name}{optional}"),
            }
        })
        .collect();
    Some(format!("{{ {} }}", fields.join(", ")))
}

/// The field names of each variant of a union whose variants are all objects.
fn describe_union_fields(schema: &Value, root: &Value, order: &[&str]) -> Option<String> {
    let schema = follow_ref(schema, root);
    let variants = match (schema.get("anyOf"), schema.get("oneOf")) {
        (Some(variants), _) | (None, Some(variants)) => variants.as_array()?,
        _ => return None,
    };
    if variants.is_empty() {
        return None;
    }
    variants
        .iter()
        .map(|variant| describe_fields(variant, root, 0, order))
        .collect::<Option<Vec<_>>>()
        .map(|described| described.join(" | "))
}

/// What a script call resolves to, in one line (upstream `describeOutput`).
fn describe_output(name: &str, schema: &Value) -> String {
    let rendered = render_tool_output_type(Some(schema));
    if rendered == "string" {
        return "a string".into();
    }
    let order = property_order(name);
    if let Some(fields) = describe_fields(schema, schema, 1, order)
        .or_else(|| describe_union_fields(schema, schema, order))
    {
        return format!("`{fields}`");
    }
    if !rendered.contains('\n') {
        return format!("`{rendered}`");
    }
    format!(
        "a value; `await describeTool({})` shows its type",
        Value::String(name.to_string())
    )
}

/// A declared tool's description followed by how scripts call it (upstream
/// `describeScriptCall`).
fn describe_script_call(tool: &AiToolDefinition) -> String {
    format!(
        "{}\n\nCodemode: `tools.{}(args)` resolves to {}.",
        tool.description.trim(),
        to_codemode_identifier(&tool.name),
        describe_output(&tool.name, &output_schema(&tool.name))
    )
}

/// The declarations sent to the model with codemode on (upstream
/// `prepareCodemodeLoadout`; every noel tool has `direct` exposure). With
/// `on`, callable tools say how scripts call them and codemode lists none;
/// with `only`, codemode lists them all and they are not declared.
pub(crate) fn present(
    definitions: Vec<AiToolDefinition>,
    guidelines: &BTreeMap<String, Vec<String>>,
    presentation: Presentation,
) -> Vec<AiToolDefinition> {
    let callable = callable(&definitions);
    let is_callable = |name: &str| callable.iter().any(|tool| tool.name == name);
    let no_guidelines = Vec::new();
    let codemode = if presentation.only {
        let listed: Vec<ToolDeclaration> = callable
            .iter()
            .map(|tool| declaration(tool, guidelines.get(&tool.name).unwrap_or(&no_guidelines)))
            .collect();
        definition(description(&listed, Some(presentation.inline_budget)))
    } else {
        definition(description(&[], Some(presentation.inline_budget)))
    };
    let mut presented: Vec<AiToolDefinition> = definitions
        .into_iter()
        .filter(|tool| !(presentation.only && is_callable(&tool.name)))
        .map(|mut tool| {
            if is_callable(&tool.name) {
                tool.description = describe_script_call(&tool);
            }
            tool
        })
        .collect();
    presented.push(codemode);
    presented
}

/// The tool's definition with `description`.
fn definition(description: String) -> AiToolDefinition {
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
    state: Mutex<NestedState>,
}

#[derive(Default)]
struct NestedState {
    edited: Vec<String>,
    registrations: Vec<extensions::ExtensionRegistrationSet>,
}

impl Nested {
    fn state(&self) -> std::sync::MutexGuard<'_, NestedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One nested call: the gate, then the tool. A string result, or the
    /// tool's error text as the script's `Error`.
    async fn run(
        &self,
        id: String,
        name: &str,
        args: Option<Value>,
    ) -> Result<Option<Value>, String> {
        let arguments = args.unwrap_or_else(|| json!({})).to_string();
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
            None => {
                let (output, is_error, value) = self.builtin(&call).await;
                // A tool with a structured result resolves to it, on a
                // failure too (upstream `toScriptValue`).
                if let Some(value) = value {
                    return Ok(Some(value));
                }
                (output, is_error)
            }
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

    /// A built-in tool's text, error flag, and the value a script gets
    /// instead of the text when the tool has one: `bash`'s structured
    /// output, or `read` of an image as an image block.
    async fn builtin(&self, call: &ChatToolCall) -> (String, bool, Option<Value>) {
        let requests = match model_tool_requests(call) {
            Ok(requests) => requests,
            Err(error) => return (error.to_string(), true, None),
        };
        let env = match &self.env {
            Ok(env) => env.clone(),
            Err(message) => return (message.clone(), true, None),
        };
        let mut outputs = Vec::new();
        let mut is_error = false;
        let mut value = None;
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
                    if let Some(structured) = result.structured {
                        value = Some(structured);
                    } else if let Some(image) = result.images.first() {
                        value = Some(json!({
                            "type": "image",
                            "data": image.data_base64,
                            "mimeType": image.mime_type,
                            "note": result.output,
                        }));
                    }
                    outputs.push(result.output);
                }
                Err(error) => {
                    is_error = true;
                    outputs.push(error.to_string());
                }
            }
        }
        (outputs.join("\n"), is_error, value)
    }
}

/// Characters of a nested call's arguments and error kept for display
/// (upstream `ARGS_PREVIEW_CHARS`, `ERROR_PREVIEW_CHARS`).
const ARGS_PREVIEW_CHARS: usize = 200;
const ERROR_PREVIEW_CHARS: usize = 500;

/// `text` cut to `max` characters (UTF-16 units, as upstream counts) with
/// `...` at the end, never splitting a character (upstream 9e870f6f).
fn truncate_text(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_owned();
    }
    format!("{}...", noel_tools::slice_head_utf16(text, max - 3))
}

/// What a nested call is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NestedCallStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

impl NestedCallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NestedCallStatus::Running => "running",
            NestedCallStatus::Ok => "ok",
            NestedCallStatus::Error => "error",
            NestedCallStatus::Cancelled => "cancelled",
        }
    }
}

/// A tool call a codemode script made (upstream `CodemodeNestedCall`, the
/// rows of the tool's `details.calls`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCall {
    /// `<codemode call id>/<n>`.
    pub id: String,
    pub name: String,
    /// Compact JSON of the arguments, cut for display.
    pub args: String,
    pub status: NestedCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Error text, cut for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Keeps the rows of the script's nested calls and sends a snapshot on
/// every change.
struct Recording<'a> {
    nested: &'a Nested,
    /// The callable tools and their samples (description and declaration),
    /// for `ALL_TOOLS` and the discovery globals.
    callable: &'a [AiToolDefinition],
    samples: &'a BTreeMap<String, String>,
    parent_id: String,
    calls: Mutex<Vec<NestedToolCall>>,
    updates: tokio::sync::mpsc::UnboundedSender<Vec<NestedToolCall>>,
}

impl Recording<'_> {
    fn rows(&self) -> std::sync::MutexGuard<'_, Vec<NestedToolCall>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, rows: &[NestedToolCall]) {
        let _ = self.updates.send(rows.to_vec());
    }

    /// `searchTools()`, `describeTool()` and `describeNamespace()` (upstream
    /// `createDiscoveryGlobals`). noel's tools have no namespaces, so
    /// `describeNamespace()` finds none and a `namespace` filter matches no
    /// tool.
    fn discovery(&self, name: &str, args: Option<Value>) -> Result<Option<Value>, String> {
        let args = match args {
            Some(Value::Array(args)) => args,
            _ => Vec::new(),
        };
        let entry = |tool: &str| {
            json!({
                "name": to_codemode_identifier(tool),
                "description": self.samples.get(tool).cloned().unwrap_or_default(),
            })
        };
        match name {
            "searchTools" => {
                let Some(Value::String(query)) = args.first() else {
                    return Err("searchTools() expects a query string".into());
                };
                let options = args.get(1).filter(|options| options.is_object());
                let limit = match options.and_then(|options| options.get("limit")) {
                    None | Some(Value::Null) => DEFAULT_TOOL_SEARCH_LIMIT,
                    Some(limit) => match limit.as_f64() {
                        Some(n) if n.fract() == 0.0 && n > 0.0 => n as usize,
                        _ => return Err("searchTools() limit must be a positive integer".into()),
                    },
                };
                let namespace = match options.and_then(|options| options.get("namespace")) {
                    None | Some(Value::Null) => None,
                    Some(Value::String(namespace)) => Some(namespace),
                    Some(_) => return Err("searchTools() namespace must be a string".into()),
                };
                if namespace.is_some_and(|namespace| !namespace.is_empty()) {
                    return Ok(Some(Value::Array(Vec::new())));
                }
                let documents: Vec<_> = self
                    .callable
                    .iter()
                    .map(|tool| {
                        create_tool_search_document(
                            &tool.name,
                            &tool.description,
                            Some(&tool.parameters),
                            None,
                        )
                    })
                    .collect();
                let matches = Bm25Ranker::default().rank(query, &documents, limit);
                Ok(Some(Value::Array(
                    matches.iter().map(|found| entry(&found.name)).collect(),
                )))
            }
            "describeTool" => {
                let Some(Value::String(wanted)) = args.first() else {
                    return Err("describeTool() expects a tool name".into());
                };
                Ok(self
                    .callable
                    .iter()
                    .find(|tool| {
                        &tool.name == wanted || &to_codemode_identifier(&tool.name) == wanted
                    })
                    .and_then(|tool| self.samples.get(&tool.name))
                    .map(|sample| Value::String(sample.clone())))
            }
            "describeNamespace" => match args.first() {
                Some(Value::String(_)) => Ok(None),
                _ => Err("describeNamespace() expects a namespace name".into()),
            },
            _ => Err(format!("Unknown global \"{name}\"")),
        }
    }
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
                return self.discovery(name, args);
            }
            let preview = args
                .as_ref()
                .map(|args| truncate_text(&args.to_string(), ARGS_PREVIEW_CHARS))
                .unwrap_or_default();
            let (index, id) = {
                let mut rows = self.rows();
                let id = format!("{}/{}", self.parent_id, rows.len() + 1);
                rows.push(NestedToolCall {
                    id: id.clone(),
                    name: name.to_owned(),
                    args: preview,
                    status: NestedCallStatus::Running,
                    duration_ms: None,
                    error: None,
                });
                self.publish(&rows);
                (rows.len() - 1, id)
            };
            let started = Instant::now();
            let result = self.nested.run(id, name, args).await;
            let mut rows = self.rows();
            let row = &mut rows[index];
            row.duration_ms = Some(started.elapsed().as_millis() as u64);
            match &result {
                Ok(_) => row.status = NestedCallStatus::Ok,
                Err(error) => {
                    row.status = NestedCallStatus::Error;
                    row.error = Some(truncate_text(error, ERROR_PREVIEW_CHARS));
                }
            }
            self.publish(&rows);
            drop(rows);
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
    guidelines: &BTreeMap<String, Vec<String>>,
    on_nested: &mut (dyn FnMut(Vec<NestedToolCall>) + Send),
    cancellation: Option<&noel_agent::CancellationToken>,
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
    // A slot first: the wait counts toward the script's deadline.
    let (slots, max) = workers();
    let wait = source.options.timeout_ms.map_or(WORKER_WAIT, |ms| {
        WORKER_WAIT.min(std::time::Duration::from_millis(ms))
    });
    let _slot = match tokio::time::timeout(wait, slots.clone().acquire_owned()).await {
        Ok(Ok(slot)) => slot,
        _ => {
            return Ok(failed(admission_failure(
                source.options.timeout_ms,
                *max,
                started.elapsed().as_secs_f64(),
            )))
        }
    };
    let waited = started.elapsed();
    let nested = Nested {
        env: runtime.tool_env(),
        options: super::tool_runtime_options(runtime, super::bash_session_env(runtime)),
        hosts: runtime.extension_hosts(),
        cwd: runtime.session.cwd.clone(),
        extension_tools: runtime.systems.extension_tools.clone(),
        state: Mutex::default(),
    };
    // ALL_TOOLS entries carry the declaration.
    let no_guidelines = Vec::new();
    let samples: BTreeMap<String, String> = callable
        .iter()
        .map(|tool| {
            let declaration =
                declaration(tool, guidelines.get(&tool.name).unwrap_or(&no_guidelines));
            (tool.name.clone(), render_tool_sample(&declaration, None))
        })
        .collect();
    let tools: Vec<DeclaredTool> = callable
        .iter()
        .map(|tool| DeclaredTool {
            name: tool.name.clone(),
            js_name: None,
            description: samples[&tool.name].clone(),
        })
        .collect();
    let globals: Vec<DeclaredGlobal> = ["searchTools", "describeTool", "describeNamespace"]
        .into_iter()
        .map(|name| DeclaredGlobal {
            name: name.to_owned(),
            spread: true,
        })
        .collect();
    let (updates, mut snapshots) = tokio::sync::mpsc::unbounded_channel();
    let recording = Recording {
        nested: &nested,
        callable,
        samples: &samples,
        parent_id: tool_call.id.clone(),
        calls: Mutex::default(),
        updates,
    };
    // Shows the script as running before its first nested call.
    on_nested(Vec::new());
    let options = ExecuteOptions {
        timeout: source.options.timeout_ms.map(|ms| {
            std::time::Duration::from_millis(ms)
                .saturating_sub(waited)
                .max(std::time::Duration::from_millis(1))
        }),
        memory_limit_bytes: Some(MEMORY_LIMIT_BYTES),
        store: runtime.session.codemode_store.clone(),
    };
    // The run's abort ends the script, which then reports `Script aborted`.
    let abort = AbortSignal::default();
    let result = {
        let execution = execute(
            &source.code,
            &tools,
            &globals,
            &recording,
            options,
            Some(&abort),
        );
        tokio::pin!(execution);
        let cancelled = async {
            match cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(cancelled);
        let mut watching = true;
        loop {
            tokio::select! {
                result = &mut execution => break result,
                Some(rows) = snapshots.recv() => on_nested(rows),
                () = &mut cancelled, if watching => {
                    watching = false;
                    abort.abort(ABORT_REASON);
                }
            }
        }
    };
    // Calls still running were cut off by the script ending, a timeout or
    // an abort.
    let rows = {
        let mut rows = recording.rows();
        for row in rows.iter_mut() {
            if row.status == NestedCallStatus::Running {
                row.status = NestedCallStatus::Cancelled;
            }
        }
        rows.clone()
    };
    on_nested(rows.clone());

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
    let calls: Vec<CallSummary> = rows
        .iter()
        .map(|row| CallSummary {
            name: row.name.clone(),
            status: row.status.as_str().to_owned(),
        })
        .collect();
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

#[cfg(test)]
mod tests {
    /// An output schema whose fields sit behind a local `$ref` still shows
    /// them (upstream 6d3e342b).
    #[test]
    fn the_output_summary_follows_local_refs() {
        let schema = serde_json::json!({
            "$ref": "#/$defs/result",
            "$defs": {
                "result": {
                    "type": "object",
                    "required": ["status"],
                    "properties": {
                        "status": {"$ref": "#/$defs/status"},
                        "detail": {"$ref": "#/$defs/detail"}
                    }
                },
                "status": {"const": "ok"},
                "detail": {"type": "object", "properties": {"rows": {"type": "integer"}}}
            }
        });
        assert_eq!(
            describe_fields(&schema, &schema, 1, &[]).as_deref(),
            Some("{ detail?: { rows? }, status: \"ok\" }")
        );
    }

    use super::*;

    #[test]
    fn a_script_that_finds_no_free_slot_says_why_as_upstream_does() {
        assert_eq!(
            admission_failure(Some(5000), 2, 5.04),
            "Script failed\nWall time 5.0 seconds\nOutput:\nScript error:\nScript timed out: Execution timed out after 5000 ms while waiting for a free codemode worker\n\nThe script did not start."
        );
        assert!(admission_failure(None, 3, 60.0).contains(
            "No codemode worker became free within 60 seconds. All 3 workers, shared by every session in this process, are held by other scripts. Retry after they finish."
        ));
    }

    fn tool(name: &str, description: &str) -> AiToolDefinition {
        AiToolDefinition {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    #[test]
    fn the_catalog_fills_the_budget_cheapest_first_as_upstream_does() {
        let tools = [
            tool("read_notes", "Read notes."),
            tool("mcp__docs__search", "Search docs."),
            tool("mcp__docs__long", &"Long ".repeat(200)),
        ];
        let listed: Vec<_> = tools.iter().map(|tool| declaration(tool, &[])).collect();
        let all = description(&listed, None);
        assert!(all.contains("Nested tools:"));
        assert!(all.contains("find unlisted tools, such as MCP tools"));
        assert!(all.contains("`await searchTools(query, { limit?, namespace? })`"));
        assert!(all.contains("### `mcp__docs__long`"));
        assert!(all.contains(
            "### `read_notes`\nRead notes.\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { read_notes(args: { [key: string]: unknown; }): Promise<string>; };\n```"
        ));
        let budgeted = description(&listed, Some(100));
        assert!(budgeted.contains("### `read_notes`"));
        assert!(budgeted.contains("### `mcp__docs__search`"));
        assert!(!budgeted.contains("### `mcp__docs__long`"));
        assert_eq!(description(&listed, Some(100)), budgeted);
        assert!(!description(&listed, Some(0)).contains("codemode tool declaration:"));
        // A section whose id differs names the raw tool.
        let dashed = description(&[declaration(&tool("my-tool", "x"), &[])], None);
        assert!(dashed.contains("### `my_tool` (`my-tool`)"));
    }

    #[test]
    fn declared_tools_say_what_scripts_get_as_upstream_words_it() {
        let bash = describe_script_call(&tool("bash", "Run a command."));
        assert_eq!(
            bash,
            "Run a command.\n\nCodemode: `tools.bash(args)` resolves to `{ output, truncated, full_output_path?, exit_code, wall_time_seconds }`."
        );
        let read = describe_script_call(&tool("read", "Read a file."));
        assert!(read.ends_with(
            "resolves to `string | { data: string; mimeType: string; note: string; type: \"image\"; }`."
        ));
        assert!(describe_script_call(&tool("ls", "List."))
            .ends_with("`tools.ls(args)` resolves to a string."));
    }

    #[test]
    fn mode_only_lists_callable_tools_and_hides_their_declarations() {
        let definitions = vec![tool("bash", "Run."), tool("todo", "Todos.")];
        let on = present(
            definitions.clone(),
            &BTreeMap::new(),
            Presentation::default(),
        );
        let names: Vec<&str> = on.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["bash", "todo", CODEMODE]);
        assert!(on[0].description.contains("Codemode: `tools.bash(args)`"));
        assert_eq!(on[1].description, "Todos.");
        assert!(!on[2].description.contains("Nested tools:"));
        let only = present(
            definitions,
            &BTreeMap::from([("bash".to_string(), vec!["Prefer rg.".to_string()])]),
            Presentation {
                only: true,
                inline_budget: 3000,
            },
        );
        let names: Vec<&str> = only.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["todo", CODEMODE]);
        assert!(only[1]
            .description
            .contains("### `bash`\nRun.\n\n- Prefer rg."));
    }

    #[test]
    fn settings_choose_the_presentation() {
        let settings = noel_config::CodemodeSettings {
            mode: Some("only".into()),
            inline_budget: serde_json::Number::from_f64(170.5),
        };
        assert_eq!(
            Presentation::from_settings(Some(&settings)),
            Presentation {
                only: true,
                inline_budget: 170
            }
        );
        let invalid = noel_config::CodemodeSettings {
            mode: Some("off".into()),
            inline_budget: serde_json::Number::from_f64(-1.0),
        };
        assert_eq!(
            Presentation::from_settings(Some(&invalid)),
            Presentation::default()
        );
    }

    #[test]
    fn previews_are_cut_with_dots() {
        assert_eq!(truncate_text("abcdef", 10), "abcdef");
        assert_eq!(truncate_text("abcdefghijkl", 10), "abcdefg...");
        // An emoji across the cut is left out whole, not split.
        assert_eq!(truncate_text("abcdef\u{1f600}xyz", 10), "abcdef...");
    }
}

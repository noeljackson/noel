//! The persistent extension host (JSON protocol v2).
//!
//! v1 (a manifest's `"protocol": "json"`) runs one process per request: the
//! request goes in on stdin, one response comes back, and the process exits.
//! v2 (a manifest's `"protocolVersion": 2`) keeps one process per extension
//! for the session's life: an `init` handshake carries the extension's
//! registered tools and commands (upstream's `registerTool` and
//! `registerCommand`), tools and slash commands travel as requests over the
//! same pipe, the host sends lifecycle notifications (`sessionStart`,
//! `agentSettled`, `reload`, `shutdown`), and the extension can send
//! `registerTools` at any time — the dynamic-tools pattern, where a command
//! or a gate decides what the model may call next.
//!
//! A v2 host holds a child process and its stdin, so it is not part of
//! [`crate::ReloadableSystems`](crate::ReloadableSystems): reloadable state
//! stays plain data, the host is runtime state, and `/reload` restarts the
//! hosts of the new extension set after notifying the old ones.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;

use noel_tools::spawn_tool_process;

use crate::{AgentError, ToolError};

/// The protocol version of the persistent host.
pub const EXTENSION_PROTOCOL_VERSION: u8 = 2;

/// A registered tool, from a manifest (v1) or the v2 `init` handshake and
/// `registerTools`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRegistration {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub replay_safe: bool,
    /// The system prompt's one-line entry for the tool; `None` keeps it out
    /// of the prompt's tool list (upstream `promptSnippet`).
    pub prompt_snippet: Option<String>,
    /// The system prompt's rules for the tool (upstream `promptGuidelines`).
    pub prompt_guidelines: Vec<String>,
}

/// A registered `/`-command (upstream `registerCommand`): invoked as
/// `/<command> [args]` from the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCommandRegistration {
    pub name: String,
    pub description: String,
}

/// What one loaded extension contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRegistrationSet {
    pub extension_name: String,
    pub tools: Vec<ExtensionRegistration>,
    pub commands: Vec<ExtensionCommandRegistration>,
    /// The extension wants `toolCall` gate requests: before a tool the
    /// model asked for runs, the host asks it to allow, modify, or block
    /// the call (upstream's `tool_call` handlers, a permission gate).
    pub tool_gate: bool,
}

/// One request's reply: the parsed response and the raw line it came in.
struct HostResponse {
    parsed: ExtensionResponse,
    raw: String,
}

/// A v2 request to the extension process.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostRequest<'a> {
    protocol_version: u8,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    input: &'a str,
    cwd: &'a str,
    /// `init` only: the session's id, like the environment a bash command
    /// gets.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

/// A v2 response from the extension process. `output`/`error` are v1's
/// command response; v2 adds the host-side effects.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionResponse {
    output: Option<String>,
    error: Option<String>,
    /// Replacement tool registrations from this extension. An empty array
    /// withdraws them all (upstream: tools cannot be unregistered, only
    /// re-registered `hidden`, so withdrawing means "none for the model").
    #[serde(default)]
    tools: Option<Vec<ExtensionRegistrationWire>>,
    /// Replacement command registrations from this extension.
    #[serde(default)]
    commands: Option<Vec<ExtensionCommandWire>>,
    /// Ask for `toolCall` gate requests (upstream's `tool_call` handlers).
    #[serde(default)]
    tool_gate: bool,
    /// A gate response: `false` (with `reason`) blocks the call.
    #[serde(default)]
    allow: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ExtensionRegistrationWire {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
    #[serde(default)]
    replay_safe: bool,
    prompt_snippet: Option<String>,
    #[serde(default)]
    prompt_guidelines: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ExtensionCommandWire {
    name: String,
    description: Option<String>,
}

impl From<ExtensionRegistrationWire> for ExtensionRegistration {
    fn from(wire: ExtensionRegistrationWire) -> Self {
        ExtensionRegistration {
            name: wire.name,
            description: wire.description.unwrap_or_default(),
            parameters: wire.parameters.unwrap_or_else(
                || serde_json::json!({"type": "object", "properties": {}, "required": []}),
            ),
            replay_safe: wire.replay_safe,
            prompt_snippet: wire.prompt_snippet,
            prompt_guidelines: wire.prompt_guidelines,
        }
    }
}

impl From<ExtensionCommandWire> for ExtensionCommandRegistration {
    fn from(wire: ExtensionCommandWire) -> Self {
        ExtensionCommandRegistration {
            name: wire.name,
            description: wire.description.unwrap_or_default(),
        }
    }
}

/// The outcome of one host request.
/// A `toolCall` gate's decision for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecision {
    /// `false` blocks the call; the model sees `reason` as its error.
    pub allow: bool,
    pub reason: String,
}

impl GateDecision {
    pub fn allow() -> Self {
        Self {
            allow: true,
            reason: String::new(),
        }
    }

    pub fn blocked(reason: String) -> Self {
        Self {
            allow: false,
            reason,
        }
    }
}

/// The outcome of one host request.
#[derive(Debug)]
pub struct HostOutcome {
    pub output: String,
    pub is_error: bool,
    /// A gate's structured decision, when the response carried one.
    pub allow: Option<bool>,
    pub reason: Option<String>,
    /// The raw response line, for kinds whose payload is the response
    /// itself (a gate's `allow`/`reason`) rather than its `output`.
    pub raw: String,
    /// Registrations as the extension now wants them, when its response
    /// carried any.
    pub registrations: Option<ExtensionRegistrationSet>,
}

/// One persistent extension process.
struct HostProcess {
    child: Child,
    stdin: ChildStdin,
}
impl HostProcess {
    async fn spawn(path: &Path, extension_name: &str, cwd: &Path) -> Result<Self, AgentError> {
        let mut command = host_command(path)?;
        command
            .current_dir(cwd)
            .env("NOEL_EXTENSION_NAME", extension_name)
            .env("NOEL_EXTENSION_PATH", path)
            .env("NOEL_EXTENSION_PROTOCOL", "2")
            // A TypeScript extension resolves its imports against the
            // directory noel found it in, not against the shim's own.
            .env("NOEL_EXTENSION_DIR", path.parent().unwrap_or(cwd))
            // Extensions written for pi read the same values under PI_ names.
            .env("PI_EXTENSION_NAME", extension_name)
            .env("PI_EXTENSION_PATH", path)
            .env("PI_EXTENSION_PROTOCOL", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Like a tool call: no controlling terminal, and a dropped host
        // kills the extension's whole process group. The group guard is
        // finished (released, not killed): the host outlives the call, and
        // the child's kill_on_drop still kills it when the host drops.
        let (mut child, group) = spawn_tool_process(&mut command).map_err(|source| {
            // Name what actually failed to start: for a `.ts` extension
            // that is the bun program, not the extension's own file.
            let program = command.as_std().get_program();
            let what = if program.to_string_lossy() == "bun" {
                format!(
                    "bun (needed for the TypeScript extension {}; install bun or set NOEL_BUN_HOST)",
                    path.display()
                )
            } else {
                path.display().to_string()
            };
            AgentError::Tool(ToolError::Io {
                path: what.into(),
                source,
            })
        })?;
        group.finish();
        let stdin = child.stdin.take().expect("piped stdin");
        Ok(Self { child, stdin })
    }

    async fn request(&mut self, request: &HostRequest<'_>) -> Result<HostResponse, String> {
        let mut line = serde_json::to_string(request).map_err(|error| error.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(io_error)?;
        self.stdin.flush().await.map_err(io_error)?;
        // One line of JSON back per request, like v1's single response.
        let mut stdout = self
            .child
            .stdout
            .take()
            .map(BufReader::new)
            .ok_or_else(|| "extension closed its stdout".to_string())?;
        let mut response = String::new();
        stdout.read_line(&mut response).await.map_err(io_error)?;
        if response.is_empty() {
            return Err("extension exited without a response".to_string());
        }
        self.child.stdout = Some(stdout.into_inner());
        let parsed: ExtensionResponse =
            serde_json::from_str(response.trim()).map_err(|error| error.to_string())?;
        Ok(HostResponse {
            parsed,
            raw: response.trim().to_string(),
        })
    }

    /// Fire-and-forget notification; the extension may exit afterwards
    /// (`shutdown`), so no response is read.
    async fn notify(&mut self, request: &HostRequest<'_>) -> Result<(), String> {
        let mut line = serde_json::to_string(request).map_err(|error| error.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(io_error)?;
        self.stdin.flush().await.map_err(io_error)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

fn io_error(error: io::Error) -> String {
    error.to_string()
}

/// The session's extension hosts: one persistent process per v2 extension,
/// plus the registrations the handshakes produced. v1 extensions keep their
/// one-shot behavior and are not held here.
#[derive(Default)]
pub struct ExtensionHosts {
    hosts: BTreeMap<String, Arc<Mutex<HostProcess>>>,
    /// Registrations by extension name, from `init` and later responses.
    registrations: BTreeMap<String, ExtensionRegistrationSet>,
    /// Failures to report instead of running tools: a host that could not
    /// start never runs, fail closed.
    failures: BTreeMap<String, String>,
    /// The one-shot v1 tools, by tool name, for dispatch.
    v1_tools: BTreeMap<String, V1Tool>,
}

/// A v1 manifest tool and the extension it belongs to.
#[derive(Debug, Clone)]
pub struct V1Tool {
    pub extension_name: String,
    pub extension_path: PathBuf,
    pub tool_name: String,
}

impl ExtensionHosts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a v2 host: spawn, send `init`, and adopt its registrations.
    pub async fn start(
        &mut self,
        extension_name: &str,
        path: &Path,
        cwd: &Path,
        session_id: &str,
    ) -> Result<(), String> {
        let mut process = match HostProcess::spawn(path, extension_name, cwd).await {
            Ok(process) => process,
            Err(error) => {
                // A spawn failure (bun missing, shim missing) is recorded
                // like an init failure: later calls report it instead of
                // "has no host", failing closed with the reason.
                let message = error.to_string();
                self.failures
                    .insert(extension_name.to_string(), message.clone());
                return Err(message);
            }
        };
        let response = process
            .request(&HostRequest {
                protocol_version: EXTENSION_PROTOCOL_VERSION,
                kind: "init",
                command: None,
                tool_call_id: None,
                input: "",
                cwd: &cwd.display().to_string(),
                session_id: Some(session_id),
            })
            .await;
        match response {
            Ok(response) => {
                let set = registrations_from_response(extension_name, &response.parsed);
                if let Some(set) = set {
                    self.registrations.insert(extension_name.to_string(), set);
                }
                self.hosts
                    .insert(extension_name.to_string(), Arc::new(Mutex::new(process)));
                Ok(())
            }
            Err(error) => {
                let _ = process.child.kill().await;
                let message = format!("extension {extension_name} init failed: {error}");
                self.failures
                    .insert(extension_name.to_string(), message.clone());
                Err(message)
            }
        }
    }

    /// Register the one-shot v1 tools for dispatch.
    pub fn set_v1_tools(&mut self, tools: Vec<V1Tool>) {
        self.v1_tools = tools
            .into_iter()
            .map(|tool| (tool.tool_name.clone(), tool))
            .collect();
    }

    /// The registrations of every live extension.
    pub fn registration_sets(&self) -> Vec<ExtensionRegistrationSet> {
        self.registrations.values().cloned().collect()
    }

    /// The extension that owns a tool name, if any (v2 first, then v1).
    pub fn owning_extension(&self, tool: &str) -> Option<String> {
        self.registrations
            .values()
            .find(|set| set.tools.iter().any(|tool_| tool_.name == tool))
            .map(|set| set.extension_name.clone())
            .or_else(|| {
                self.v1_tools
                    .get(tool)
                    .map(|tool_| tool_.extension_name.clone())
            })
    }

    /// Send a lifecycle notification to every live host. Returns the
    /// failures to surface as diagnostics.
    pub async fn notify(&mut self, kind: &str, cwd: &Path) -> Vec<String> {
        let mut diagnostics = Vec::new();
        for (name, host) in &self.hosts {
            let mut host = host.lock().await;
            if !host.is_alive() {
                continue;
            }
            if let Err(error) = host
                .notify(&HostRequest {
                    protocol_version: EXTENSION_PROTOCOL_VERSION,
                    kind,
                    command: None,
                    tool_call_id: None,
                    input: "",
                    cwd: &cwd.display().to_string(),
                    session_id: None,
                })
                .await
            {
                diagnostics.push(format!("extension {name} {kind} failed: {error}"));
            }
        }
        diagnostics
    }

    /// A v2 tool call or slash command. Returns the output and any
    /// registration changes.
    pub async fn call(
        &mut self,
        extension_name: &str,
        kind: &str,
        command: &str,
        input: &str,
        cwd: &Path,
        tool_call_id: Option<&str>,
    ) -> Result<HostOutcome, String> {
        if let Some(failure) = self.failures.get(extension_name) {
            return Err(failure.clone());
        }
        let host = self
            .hosts
            .get(extension_name)
            .ok_or_else(|| format!("extension {extension_name} has no host"))?;
        let mut host = host.lock().await;
        if !host.is_alive() {
            return Err(format!("extension {extension_name} exited"));
        }
        let response = host
            .request(&HostRequest {
                protocol_version: EXTENSION_PROTOCOL_VERSION,
                kind,
                command: Some(command),
                tool_call_id,
                input,
                cwd: &cwd.display().to_string(),
                session_id: None,
            })
            .await?;
        let HostResponse {
            parsed: response,
            raw: raw_line,
        } = response;
        let is_error = response.error.is_some();
        let allow = response.allow;
        let reason = response.reason.clone();
        let output = response
            .output
            .clone()
            .unwrap_or_else(|| format!("extension {extension_name} {kind} {command} completed"));
        Ok(HostOutcome {
            output,
            is_error,
            allow,
            reason,
            raw: raw_line,
            registrations: registrations_from_response(extension_name, &response),
        })
    }

    /// Ask every gate-registered host to allow, modify, or block a tool
    /// call (upstream's `tool_call` handlers, in registration order). A
    /// gate that fails closes: the call is blocked with the failure, never
    /// run, because a gate is a safety decision.
    pub async fn gate_tool_call(
        &mut self,
        tool_name: &str,
        input: &str,
        cwd: &Path,
    ) -> Result<GateDecision, String> {
        let gated: Vec<String> = self
            .registrations
            .values()
            .filter(|set| set.tool_gate)
            .map(|set| set.extension_name.clone())
            .collect();
        let mut decision = GateDecision::allow();
        for extension_name in gated {
            let outcome = self
                .call(&extension_name, "toolCall", tool_name, input, cwd, None)
                .await;
            match outcome {
                Ok(outcome) => {
                    if outcome.is_error {
                        return Ok(GateDecision::blocked(format!(
                            "extension {extension_name} gate failed: {}",
                            outcome.output
                        )));
                    }
                    if let Some(set) = outcome.registrations {
                        self.apply_registrations(set);
                    }
                    // The structured decision wins; a response without
                    // one allows, and a plain-text `output` is a block
                    // reason, so a script can gate without building JSON.
                    if let Some(allow) = outcome.allow {
                        if allow {
                            continue;
                        }
                        let reason = outcome
                            .reason
                            .or(Some(outcome.output))
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "blocked by extension gate".to_string());
                        decision = GateDecision::blocked(reason);
                        continue;
                    }
                    let payload = outcome.output.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    decision = GateDecision::blocked(payload.to_string());
                }
                Err(error) => {
                    // Fail closed: a host that cannot answer a safety
                    // question does not silently allow the call.
                    return Ok(GateDecision::blocked(format!(
                        "extension {extension_name} gate failed: {error}"
                    )));
                }
            }
        }
        Ok(decision)
    }

    /// A one-shot v1 tool call: spawn, ask, exit.
    pub async fn call_v1(
        &mut self,
        extension_name: &str,
        path: &Path,
        tool_name: &str,
        tool_call_id: &str,
        input: &str,
        cwd: &Path,
    ) -> Result<HostOutcome, String> {
        let mut process = HostProcess::spawn(path, extension_name, cwd)
            .await
            .map_err(|error| error.to_string())?;
        let outcome = process
            .request(&HostRequest {
                protocol_version: 1,
                kind: "tool",
                command: Some(tool_name),
                tool_call_id: Some(tool_call_id),
                input,
                cwd: &cwd.display().to_string(),
                session_id: None,
            })
            .await
            .map(|response| HostOutcome {
                output: response.parsed.output.unwrap_or_else(|| {
                    format!("extension {extension_name} tool {tool_name} completed")
                }),
                is_error: response.parsed.error.is_some(),
                allow: response.parsed.allow,
                reason: response.parsed.reason.clone(),
                raw: response.raw,
                registrations: None,
            })
            .map_err(|error| format!("extension {extension_name} failed: {error}"));
        let _ = process.child.kill().await;
        outcome
    }

    /// Adopt registration changes from a response.
    pub fn apply_registrations(&mut self, set: ExtensionRegistrationSet) {
        self.registrations.insert(set.extension_name.clone(), set);
    }

    /// Stop every host, notifying `shutdown` first.
    pub async fn shutdown(&mut self, cwd: &Path) {
        self.notify("shutdown", cwd).await;
        for host in self.hosts.values() {
            let mut host = host.lock().await;
            let _ = host.child.kill().await;
        }
        self.hosts.clear();
        self.registrations.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.v1_tools.is_empty()
    }

    /// Whether a host for `extension_name` is running (started, whatever
    /// its init returned).
    pub fn host_running(&self, extension_name: &str) -> bool {
        self.hosts.contains_key(extension_name)
    }

    /// Drop the hosts of extensions no longer loaded and their
    /// registrations, so a reload that removes a v2 extension stops
    /// offering its tools.
    pub fn retain_extensions(&mut self, keep: &[String]) {
        self.hosts.retain(|name, _| keep.contains(name));
        self.registrations.retain(|name, _| keep.contains(name));
        self.failures.retain(|name, _| keep.contains(name));
    }
}

fn registrations_from_response(
    extension_name: &str,
    response: &ExtensionResponse,
) -> Option<ExtensionRegistrationSet> {
    if response.tools.is_none() && response.commands.is_none() {
        return None;
    }
    Some(ExtensionRegistrationSet {
        extension_name: extension_name.to_string(),
        tools: response
            .tools
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(ExtensionRegistration::from)
            .collect(),
        commands: response
            .commands
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(ExtensionCommandRegistration::from)
            .collect(),
        tool_gate: response.tool_gate,
    })
}

/// Read a manifest's declared protocol version. `Some(2)` is the persistent
/// host; anything else is v1's one-shot behavior.
pub fn manifest_protocol_version(path: &Path) -> Option<u8> {
    let manifest_paths = crate::extension_manifest_paths(path);
    for manifest_path in manifest_paths {
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        // A manifest can name the persistent protocol either way.
        let protocol = value
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .or_else(|| {
                value
                    .get("protocol")
                    .and_then(Value::as_str)
                    .and_then(|protocol| protocol.strip_prefix("json").map(str::to_string))
                    .map(|suffix| {
                        let trimmed = suffix.trim();
                        if trimmed.is_empty() {
                            1
                        } else {
                            trimmed.parse::<u64>().unwrap_or(1)
                        }
                    })
            });
        return protocol.map(|version| version.clamp(1, 2) as u8);
    }
    None
}

/// Map a host error onto the agent error type used for tool failures.
pub fn host_error_as_agent_error(extension_name: &str, error: String) -> AgentError {
    AgentError::Tool(ToolError::Failed(format!(
        "extension {extension_name}: {error}"
    )))
}

/// The command that runs one extension. An executable file runs itself
/// (v1's one-shot programs and v2 hosts); a `.ts` file runs under the bun
/// host: `bun <noel-bun-host.ts> <extension.ts>`, discovered as
/// [`find_bun_host`] describes.
fn host_command(path: &Path) -> Result<tokio::process::Command, AgentError> {
    let is_typescript = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "ts");
    if !is_typescript {
        // An executable file is its own host, as before.
        return Ok(tokio::process::Command::new(path));
    }
    let shim = find_bun_host().ok_or_else(|| {
        AgentError::Tool(ToolError::Failed(
            "a TypeScript extension needs bun (found on PATH) and noel's shim \
             (shipped beside the noel binary or in the agent dir); \
             set NOEL_BUN_HOST to the shim's path"
                .to_string(),
        ))
    })?;
    let mut command = tokio::process::Command::new("bun");
    command.arg(shim).arg(path);
    Ok(command)
}

/// Where noel's bun shim lives: `NOEL_BUN_HOST`, else beside the running
/// binary, else the agent dir's `extension-hosts/noel-bun-host.ts`, the
/// same agent dir the config discovers.
fn find_bun_host() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("NOEL_BUN_HOST") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let beside = exe.parent().map(|dir| dir.join("noel-bun-host.ts"));
        if let Some(beside) = beside.filter(|path| path.is_file()) {
            return Some(beside);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    let (paths, _import) = noel_config::ConfigPaths::discover(cwd, None).ok()?;
    let shim = paths.agent_dir.join("extension-hosts/noel-bun-host.ts");
    shim.is_file().then_some(shim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod");
    }

    #[tokio::test]
    async fn v2_host_registers_tools_and_serves_tool_calls() {
        let cwd = std::env::temp_dir().join(format!("noel-ext-host-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("mkdir");
        let extension = cwd.join("hosted.sh");
        fs::write(
            &extension,
            concat!(
                "#!/bin/sh\n",
                "while read line; do\n",
                "  case \"$line\" in\n",
                "    *'\"kind\":\"init\"'*) printf '%s\\n' '{\"output\":\"ready\",\"tools\":[{\"name\":\"dyn\",\"parameters\":{\"type\":\"object\"}}],\"commands\":[{\"name\":\"hi\"}]}';;\n",
                "    *) printf '%s\\n' '{\"output\":\"ran\"}';;\n",
                "  esac\n",
                "done\n",
            ),
        )
        .expect("write");
        make_executable(&extension);
        let mut hosts = ExtensionHosts::new();
        hosts
            .start("hosted", &extension, &cwd, "session-1")
            .await
            .expect("start host");
        let sets = hosts.registration_sets();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].tools[0].name, "dyn");
        assert_eq!(sets[0].commands[0].name, "hi");
        let outcome = hosts
            .call("hosted", "tool", "dyn", "{}", &cwd, Some("call-1"))
            .await
            .expect("call tool");
        assert_eq!(outcome.output, "ran");
        hosts.shutdown(&cwd).await;
        let _ = fs::remove_dir_all(cwd);
    }

    /// A `.ts` extension runs under the bun host: the same handshake, tool
    /// call, and gate as the shell host above, through noel's shipped shim.
    /// Skipped when bun or the repo's shim is not available (CI without
    /// bun keeps passing; the drift image covers the full chain).
    #[tokio::test]
    async fn v2_typescript_extension_runs_under_bun_host() {
        let Ok(bun) = which_bun() else {
            eprintln!("skipping: bun not found");
            return;
        };
        let Some(shim) = repo_bun_host_shim() else {
            eprintln!("skipping: shim not found");
            return;
        };
        let cwd = std::env::temp_dir().join(format!("noel-ext-bun-{}", std::process::id()));
        let agent_dir = cwd.join("agent");
        let extensions_dir = agent_dir.join("extensions");
        fs::create_dir_all(&extensions_dir).expect("mkdir");
        let extension = extensions_dir.join("gate.ts");
        fs::write(
            &extension,
            r#"
            export default function (pi) {
              pi.registerTool({
                name: "echo_tool",
                description: "Echo its input back.",
                parameters: { type: "object", properties: { text: { type: "string" } } },
                // Upstream's execute signature; AgentToolResult content.
                execute: async (_id, params) => ({
                  content: [{ type: "text", text: "echo: " + params.text }],
                }),
              });
              pi.registerCommand("greet", { handler: async (args) => "hello, " + (args || "world") });
              pi.on("toolCall", async (event) => {
                if (String(event.input.command ?? "").includes("secret")) {
                  return { allow: false, reason: "secret path is protected" };
                }
                return { allow: true };
              });
            }
            "#,
        )
        .expect("write extension");
        fs::write(
            extensions_dir.join("gate.ts.noel-extension.json"),
            serde_json::json!({ "protocolVersion": 2 }).to_string(),
        )
        .expect("write manifest");

        // The shim lookup checks NOEL_BUN_HOST first; point it at the repo's
        // shim so the test does not depend on an installed agent dir.
        // SAFETY: single-threaded test; the guard restores the variable.
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("NOEL_BUN_HOST", value),
                    None => std::env::remove_var("NOEL_BUN_HOST"),
                }
            }
        }
        let _guard = EnvGuard(std::env::var_os("NOEL_BUN_HOST"));
        std::env::set_var("NOEL_BUN_HOST", &shim);

        let mut hosts = ExtensionHosts::new();
        hosts
            .start("gate", &extension, &cwd, "session-1")
            .await
            .expect("start bun host");
        let sets = hosts.registration_sets();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].tools[0].name, "echo_tool");
        assert_eq!(sets[0].commands[0].name, "greet");
        assert!(sets[0].tool_gate);
        let outcome = hosts
            .call(
                "gate",
                "tool",
                "echo_tool",
                r#"{"text":"hi"}"#,
                &cwd,
                Some("c1"),
            )
            .await
            .expect("call tool");
        assert_eq!(outcome.output, "echo: hi");
        let outcome = hosts
            .call("gate", "command", "greet", "noel", &cwd, None)
            .await
            .expect("call command");
        assert_eq!(outcome.output, "hello, noel");
        let gate = hosts
            .gate_tool_call("bash", r#"{"command":"cat secret.txt"}"#, &cwd)
            .await
            .expect("gate call");
        assert_eq!(
            gate,
            GateDecision::blocked("secret path is protected".to_string())
        );
        let gate = hosts
            .gate_tool_call("bash", r#"{"command":"ls"}"#, &cwd)
            .await
            .expect("gate call");
        assert_eq!(gate, GateDecision::allow());
        hosts.shutdown(&cwd).await;
        let _ = fs::remove_dir_all(cwd);
        drop(bun);
    }

    /// Upstream's own example extension, verbatim from the pinned
    /// reference checkout, runs under the bun host: its `tool_call`
    /// handler blocks dangerous bash commands with no UI (headless), the
    /// exact semantics upstream documents. Skipped without bun or the
    /// reference checkout.
    #[tokio::test]
    async fn upstream_permission_gate_example_runs_under_bun_host() {
        let Ok(_bun) = which_bun() else {
            return;
        };
        let shim = repo_bun_host_shim();
        let example = upstream_example("permission-gate.ts");
        let (shim, example) = match (shim, example) {
            (Some(shim), Some(example)) => (shim, example),
            _ => return,
        };
        let cwd = std::env::temp_dir().join(format!("noel-ext-bun-pi-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("mkdir");
        let extension = cwd.join("permission-gate.ts");
        fs::copy(&example, &extension).expect("copy example");
        fs::write(
            cwd.join("permission-gate.ts.noel-extension.json"),
            serde_json::json!({ "protocolVersion": 2 }).to_string(),
        )
        .expect("write manifest");

        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("NOEL_BUN_HOST", value),
                    None => std::env::remove_var("NOEL_BUN_HOST"),
                }
            }
        }
        let _guard = EnvGuard(std::env::var_os("NOEL_BUN_HOST"));
        std::env::set_var("NOEL_BUN_HOST", &shim);

        let mut hosts = ExtensionHosts::new();
        hosts
            .start("permission-gate", &extension, &cwd, "session-1")
            .await
            .expect("start bun host for upstream example");
        let sets = hosts.registration_sets();
        assert_eq!(sets.len(), 1);
        assert!(sets[0].tool_gate, "the example takes the tool_call gate");
        for command in ["rm -rf /tmp/x", "sudo reboot", "chmod 777 /etc"] {
            let gate = hosts
                .gate_tool_call("bash", &format!(r#"{{"command":"{command}"}}"#), &cwd)
                .await
                .expect("gate call");
            assert_eq!(
                gate,
                GateDecision::blocked(
                    "Dangerous command blocked (no UI for confirmation)".to_string()
                ),
                "the example blocks {command}"
            );
        }
        let gate = hosts
            .gate_tool_call("bash", r#"{"command":"ls"}"#, &cwd)
            .await
            .expect("gate call");
        assert_eq!(gate, GateDecision::allow());
        hosts.shutdown(&cwd).await;
        let _ = fs::remove_dir_all(cwd);
    }

    /// Upstream's `hello.ts`, the minimal `defineTool` example, verbatim:
    /// its TypeBox parameters reach the handshake as JSON schema and its
    /// `AgentToolResult` content flattens to the protocol's output.
    #[tokio::test]
    async fn upstream_hello_tool_example_runs_under_bun_host() {
        let Ok(_bun) = which_bun() else {
            return;
        };
        let shim = repo_bun_host_shim();
        let example = upstream_example("hello.ts");
        let (shim, example) = match (shim, example) {
            (Some(shim), Some(example)) => (shim, example),
            _ => return,
        };
        let cwd = std::env::temp_dir().join(format!("noel-ext-bun-hello-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("mkdir");
        let extension = cwd.join("hello.ts");
        fs::copy(&example, &extension).expect("copy example");
        fs::write(
            cwd.join("hello.ts.noel-extension.json"),
            serde_json::json!({ "protocolVersion": 2 }).to_string(),
        )
        .expect("write manifest");

        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("NOEL_BUN_HOST", value),
                    None => std::env::remove_var("NOEL_BUN_HOST"),
                }
            }
        }
        let _guard = EnvGuard(std::env::var_os("NOEL_BUN_HOST"));
        std::env::set_var("NOEL_BUN_HOST", &shim);

        let mut hosts = ExtensionHosts::new();
        hosts
            .start("hello", &extension, &cwd, "session-1")
            .await
            .expect("start bun host for upstream example");
        let sets = hosts.registration_sets();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].tools.len(), 1);
        let tool = &sets[0].tools[0];
        assert_eq!(tool.name, "hello");
        // TypeBox's schema crosses as JSON schema: required name, string.
        let parameters = tool.parameters.clone();
        assert_eq!(parameters["required"], serde_json::json!(["name"]));
        assert_eq!(parameters["properties"]["name"]["type"], "string");
        let outcome = hosts
            .call(
                "hello",
                "tool",
                "hello",
                r#"{"name":"noel"}"#,
                &cwd,
                Some("c1"),
            )
            .await
            .expect("call tool");
        assert_eq!(outcome.output, "Hello, noel!");
        hosts.shutdown(&cwd).await;
        let _ = fs::remove_dir_all(cwd);
    }

    /// A session can host all three extension kinds at once: an
    /// executable Rust program (this repo's example, compiled on the fly),
    /// a `.ts` extension under the bun host, and a shell one-shot host.
    /// Each keeps its own process and registrations; a tool call routes to
    /// the extension that registered it. Skipped without bun (the Rust
    /// host alone is covered by the shell-host test's mechanics).
    #[tokio::test]
    async fn rust_typescript_and_shell_extensions_host_together() {
        let Ok(_bun) = which_bun() else {
            return;
        };
        let Some(shim) = repo_bun_host_shim() else {
            return;
        };
        let cwd = std::env::temp_dir().join(format!("noel-ext-mixed-{}", std::process::id()));
        let extensions_dir = cwd.join("extensions");
        fs::create_dir_all(&extensions_dir).expect("mkdir");

        // The Rust host: the shipped example, compiled with rustc.
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/extensions/rust/host.rs");
        if !example.is_file() {
            return;
        }
        let rust_host = extensions_dir.join("rust-host");
        let compiled =
            std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
                .arg("-O")
                .arg(&example)
                .arg("-o")
                .arg(&rust_host)
                .output()
                .expect("spawn rustc");
        assert!(
            compiled.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        make_executable(&rust_host);
        fs::write(
            extensions_dir.join("rust-host.noel-extension.json"),
            serde_json::json!({ "protocolVersion": 2 }).to_string(),
        )
        .expect("write manifest");

        // The TypeScript host: a counter tool under the bun shim.
        let ts_host = extensions_dir.join("ts-host.ts");
        fs::write(
            &ts_host,
            r#"
            export default function (pi) {
              let calls = 0;
              pi.registerTool({
                name: "ts_echo",
                description: "Echo from the TypeScript host.",
                parameters: { type: "object", properties: { text: { type: "string" } } },
                execute: async (_id, params) => ({
                  content: [{ type: "text", text: "ts says " + params.text + " (call " + (++calls) + ")" }],
                }),
              });
            }
            "#,
        )
        .expect("write extension");
        fs::write(
            extensions_dir.join("ts-host.ts.noel-extension.json"),
            serde_json::json!({ "protocolVersion": 2 }).to_string(),
        )
        .expect("write manifest");

        // The shell host: a persistent responder, like the v2 shell test.
        let shell_host = extensions_dir.join("shell-host");
        fs::write(
            &shell_host,
            r#"#!/bin/sh
while read line; do
  case "$line" in
    *'"kind":"init"'*) printf '%s\n' '{"output":"ready","tools":[{"name":"sh_version","parameters":{"type":"object"}}]}';;
    *) printf '%s\n' '{"output":"sh host alive"}';;
  esac
done
"#,
        )
        .expect("write extension");
        make_executable(&shell_host);
        fs::write(
            extensions_dir.join("shell-host.noel-extension.json"),
            serde_json::json!({ "protocolVersion": 2 }).to_string(),
        )
        .expect("write manifest");

        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("NOEL_BUN_HOST", value),
                    None => std::env::remove_var("NOEL_BUN_HOST"),
                }
            }
        }
        let _guard = EnvGuard(std::env::var_os("NOEL_BUN_HOST"));
        std::env::set_var("NOEL_BUN_HOST", &shim);

        let mut hosts = ExtensionHosts::new();
        hosts
            .start("rust-host", &rust_host, &cwd, "session-1")
            .await
            .expect("start rust host");
        hosts
            .start("ts-host", &ts_host, &cwd, "session-1")
            .await
            .expect("start ts host");
        hosts
            .start("shell-host", &shell_host, &cwd, "session-1")
            .await
            .expect("start shell host");

        // Every host's registrations landed, each under its own name.
        let sets = hosts.registration_sets();
        let names: Vec<&str> = sets.iter().map(|set| set.extension_name.as_str()).collect();
        assert!(names.contains(&"rust-host"));
        assert!(names.contains(&"ts-host"));
        assert!(names.contains(&"shell-host"));

        // Tools route to the host that registered them, with state.
        let outcome = hosts
            .call(
                "rust-host",
                "tool",
                "word_count",
                r#"{"text":"one two three"}"#,
                &cwd,
                Some("c1"),
            )
            .await
            .expect("rust tool");
        assert_eq!(outcome.output, "3 words");
        let outcome = hosts
            .call("rust-host", "command", "stats", "", &cwd, None)
            .await
            .expect("rust command");
        assert!(outcome.output.contains("run 1 time(s)"));
        let outcome = hosts
            .call(
                "ts-host",
                "tool",
                "ts_echo",
                r#"{"text":"hi"}"#,
                &cwd,
                Some("c2"),
            )
            .await
            .expect("ts tool");
        assert_eq!(outcome.output, "ts says hi (call 1)");
        let outcome = hosts
            .call(
                "ts-host",
                "tool",
                "ts_echo",
                r#"{"text":"again"}"#,
                &cwd,
                Some("c3"),
            )
            .await
            .expect("ts tool again");
        assert_eq!(outcome.output, "ts says again (call 2)");
        let outcome = hosts
            .call("shell-host", "tool", "sh_version", "{}", &cwd, Some("c4"))
            .await
            .expect("shell tool");
        assert_eq!(outcome.output, "sh host alive");

        // The rust host's gate decides; the others registered none.
        let gate = hosts
            .gate_tool_call("write", r#"{"path":"/etc/passwd","content":"x"}"#, &cwd)
            .await
            .expect("gate");
        assert_eq!(
            gate,
            GateDecision::blocked("/etc is protected by rust-host".to_string())
        );

        hosts.shutdown(&cwd).await;
        let _ = fs::remove_dir_all(cwd);
    }

    fn which_bun() -> Result<PathBuf, ()> {
        let path = std::env::var_os("PATH").ok_or(())?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("bun");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(())
    }

    /// The repo's shipped shim: `crates/noel-cli/src/extension-hosts/`.
    fn repo_bun_host_shim() -> Option<PathBuf> {
        let shim = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../noel-cli/src/extension-hosts/noel-bun-host.ts");
        shim.is_file().then_some(shim)
    }

    /// An upstream example: the vendored copy in `examples/extensions/
    /// typescript/` (kept current by scripts/sync-upstream-examples.sh),
    /// else the local reference checkout.
    fn upstream_example(name: &str) -> Option<PathBuf> {
        let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/extensions/typescript")
            .join(name);
        if vendored.is_file() {
            return Some(vendored);
        }
        let reference = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/ts-reference-pi/packages/coding-agent/examples/extensions")
            .join(name);
        reference.is_file().then_some(reference)
    }
}

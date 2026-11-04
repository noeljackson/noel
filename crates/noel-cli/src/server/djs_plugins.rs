//! The djs plugin tier (roadmap phase 8, spec §13): a plugin that is a
//! script, run in process on the codemode VM (djs: a sandboxed
//! interpreter with fuel and memory limits and no JIT, so it also runs on
//! iOS) instead of as a process.
//!
//! The manifest names `script` instead of `command`. The script defines
//! `const plugin = {tools, commands, panels}`:
//!
//! ```js
//! const plugin = {
//!   tools: { reverse: { description: "...", parameters: {...},
//!                       run: async (args, ctx) => "text" } },
//!   commands: { count: { description: "...", run: async (args, ctx) => "text" } },
//!   panels: { counter: { title: "Counter", view: (state) => ({type: "text", text: "..."}),
//!                        action: async (action, value, ctx) => {} } },
//! };
//! ```
//!
//! Each call runs the script on a fresh VM with no host calls, at most
//! `limits.timeoutMs` (default 5000) and `limits.memoryBytes` (default
//! 64 MiB). `ctx.state` is the plugin's state and `ctx.save(state)` keeps a
//! new one: it is saved in `state.json` beside the manifest and survives
//! restarts. After each call the panels are drawn again from the state.
//! The plugin is served by the same `noel-plugin` host as a process plugin,
//! over a link in memory, so `noel.plugin`, grants and `noel.plugins` work
//! as for any plugin.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use noel_chord::v2::Link;
use noel_codemode::execution::{execute, CallFuture, ExecuteOptions, Executor};
use noel_codemode::vm::CallTarget;
use noel_plugin::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

/// The `limits` of a script plugin's manifest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Limits {
    pub timeout_ms: Option<u64>,
    pub memory_bytes: Option<usize>,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MEMORY: usize = 64 * 1024 * 1024;

/// Runs the requests of `noel.plugin` against the script; `request` is
/// `{op, ...}` and the script's `plugin` answers it.
const DISPATCH: &str = r#"
;return await (async (request) => {
  if (typeof plugin !== "object" || plugin === null) throw new Error("the script defines no `plugin` object");
  let state = load("state");
  const ctx = { get state() { return state; }, save(next) { state = next; store("state", next); } };
  const view = (panel) => (typeof panel.view === "function" ? panel.view(state) : panel.view);
  const entries = (object) => Object.entries(object ?? {});
  const describe = () => ({
    tools: entries(plugin.tools).map(([name, tool]) => ({
      name, description: String(tool.description ?? ""), parameters: tool.parameters ?? { type: "object" },
    })),
    commands: entries(plugin.commands).map(([name, command]) => ({ name, description: String(command.description ?? "") })),
    panels: entries(plugin.panels).map(([id, panel]) => ({ id, title: String(panel.title ?? id), view: view(panel) })),
  });
  const find = (kind, name) => {
    const found = (plugin[kind] ?? {})[name];
    if (!found) throw new Error(`no ${kind.slice(0, -1)} ${name}`);
    return found;
  };
  switch (request.op) {
    case "describe": return describe();
    case "tool": return { output: String(await find("tools", request.name).run(request.args, ctx)), describe: describe() };
    case "command": return { output: String(await find("commands", request.name).run(request.args, ctx)), describe: describe() };
    case "action": {
      const panel = find("panels", request.panel);
      if (typeof panel.action !== "function") throw new Error(`panel ${request.panel} takes no actions`);
      await panel.action(request.action, request.value, ctx);
      return { output: "", describe: describe() };
    }
    default: throw new Error(`no request ${request.op}`);
  }
})(__REQUEST__);
"#;

/// A script plugin's code and state.
pub(crate) struct Script {
    source: String,
    state_path: PathBuf,
    state: Mutex<Option<Value>>,
    limits: Limits,
}

/// The script makes no host calls.
struct NoCalls;

impl Executor for NoCalls {
    fn call<'a>(&'a self, _: CallTarget, name: &'a str, _: Option<Value>) -> CallFuture<'a> {
        Box::pin(async move { Err(format!("{name}: a plugin script makes no host calls")) })
    }
}

impl Script {
    pub(crate) fn load(
        dir: &std::path::Path,
        script: &str,
        limits: Limits,
    ) -> Result<Script, String> {
        let path = dir.join(script);
        let source = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let state_path = dir.join("state.json");
        let state = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        Ok(Script {
            source,
            state_path,
            state: Mutex::new(state),
            limits,
        })
    }

    /// Runs `request`; saves the state the script kept.
    async fn run(&self, request: Value) -> Result<Value, String> {
        let code = format!(
            "{}\n{}",
            self.source,
            DISPATCH.replace("__REQUEST__", &request.to_string())
        );
        let mut store = BTreeMap::new();
        if let Some(state) = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            store.insert("state".to_owned(), state);
        }
        let options = ExecuteOptions {
            timeout: Some(
                self.limits
                    .timeout_ms
                    .map_or(DEFAULT_TIMEOUT, Duration::from_millis),
            ),
            memory_limit_bytes: Some(self.limits.memory_bytes.unwrap_or(DEFAULT_MEMORY)),
            store,
        };
        let result = execute(&code, &[], &[], &NoCalls, options, None).await;
        let completed = result
            .outcome
            .map_err(|error| format!("{}: {}", error.kind.as_str(), error.message))?;
        if let Some(state) = completed.store_writes.set.get("state") {
            *self.state.lock().unwrap_or_else(PoisonError::into_inner) = Some(state.clone());
            if let Err(error) = std::fs::write(&self.state_path, state.to_string()) {
                eprintln!(
                    "noel server: plugin state {}: {error}",
                    self.state_path.display()
                );
            }
        }
        Ok(completed.value.unwrap_or(Value::Null))
    }
}

/// Draws the panels `describe` gave again.
fn redraw(context: &Context, result: &Value) {
    for panel in result["describe"]["panels"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(id) = panel["id"].as_str() {
            context.set_panel(id, panel["view"].clone());
        }
    }
}

/// Builds the plugin `name` from its script and serves it over `link`
/// until the link closes.
pub(crate) async fn serve(name: String, script: Arc<Script>, link: Link) -> Result<(), String> {
    let described = script.run(json!({"op": "describe"})).await?;
    let mut plugin = Plugin::new(name);
    for tool in described["tools"].as_array().into_iter().flatten() {
        let tool_name = tool["name"].as_str().unwrap_or_default().to_owned();
        let script = script.clone();
        let called = tool_name.clone();
        plugin = plugin.tool(
            tool_name,
            tool["description"].as_str().unwrap_or_default(),
            tool["parameters"].clone(),
            move |args, context| {
                let (script, name) = (script.clone(), called.clone());
                async move {
                    let result = script
                        .run(json!({"op": "tool", "name": name, "args": args}))
                        .await?;
                    redraw(&context, &result);
                    Ok(result["output"].as_str().unwrap_or_default().to_owned())
                }
            },
        );
    }
    for command in described["commands"].as_array().into_iter().flatten() {
        let command_name = command["name"].as_str().unwrap_or_default().to_owned();
        let script = script.clone();
        let called = command_name.clone();
        plugin = plugin.command(
            command_name,
            command["description"].as_str().unwrap_or_default(),
            move |args, context| {
                let (script, name) = (script.clone(), called.clone());
                async move {
                    let result = script
                        .run(json!({"op": "command", "name": name, "args": args}))
                        .await?;
                    redraw(&context, &result);
                    Ok(result["output"].as_str().unwrap_or_default().to_owned())
                }
            },
        );
    }
    for panel in described["panels"].as_array().into_iter().flatten() {
        let id = panel["id"].as_str().unwrap_or_default().to_owned();
        let script = script.clone();
        let acted = id.clone();
        plugin = plugin.panel(
            id,
            panel["title"].as_str().unwrap_or_default(),
            panel["view"].clone(),
            move |action, value, context| {
                let (script, panel) = (script.clone(), acted.clone());
                async move {
                    let result = script
                        .run(json!({"op": "action", "panel": panel, "action": action, "value": value}))
                        .await?;
                    redraw(&context, &result);
                    Ok(())
                }
            },
        );
    }
    plugin.serve(link).await;
    Ok(())
}

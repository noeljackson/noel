//! The Rust SDK for noel plugins (roadmap phase 7).
//!
//! A plugin is a process `noel server` starts from a manifest and keeps
//! running, restarting it when it exits. It is a Chord v2 host at
//! `Plugin({name})` that serves `noel.plugin`: its contributions (tools,
//! commands and panels) as state, and the methods that run them. Its parent
//! link is the server, over its stdin and stdout (framed messages); through
//! it the plugin can also hold the server's objects, as its granted
//! permissions allow.
//!
//! ```no_run
//! # async fn run() {
//! use noel_plugin::Plugin;
//! use serde_json::json;
//!
//! Plugin::new("sample")
//!     .tool("echo", "Says the text back.", json!({"type": "object"}), |args, _| async move {
//!         Ok(args["text"].as_str().unwrap_or_default().to_owned())
//!     })
//!     .command("hello", "Greets.", |_, _| async { Ok("hello".to_owned()) })
//!     .run_stdio()
//!     .await;
//! # }
//! ```
//!
//! Panels are views in a small declarative vocabulary every client renders
//! natively: `{type: "text", text}`, `{type: "stack", children: [view]}`,
//! `{type: "list", items: [view]}` and `{type: "button", label, action}`.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use noel_chord::v2::host::{Host, HostOptions, LinkOptions, RemoteRef};
use noel_chord::v2::object::{BoxFuture, Cell, Implementation};
use noel_chord::v2::{Contract, Link, WireError};
use serde_json::{json, Value};

type ToolFn = Arc<dyn Fn(Value, Context) -> BoxFuture<Result<String, String>> + Send + Sync>;
type CommandFn = Arc<dyn Fn(String, Context) -> BoxFuture<Result<String, String>> + Send + Sync>;
type ActionFn =
    Arc<dyn Fn(String, Option<String>, Context) -> BoxFuture<Result<(), String>> + Send + Sync>;

struct Tool {
    description: String,
    parameters: Value,
    replay_safe: bool,
    run: ToolFn,
}

struct Command {
    description: String,
    run: CommandFn,
}

struct Panel {
    title: String,
    view: Value,
    on_action: Option<ActionFn>,
}

/// A plugin being declared, then run.
pub struct Plugin {
    name: String,
    tools: BTreeMap<String, Tool>,
    commands: BTreeMap<String, Command>,
    panels: BTreeMap<String, Panel>,
}

/// What a handler runs with: the plugin's host, to reach the server's
/// objects, and its panels, to change their views.
#[derive(Clone)]
pub struct Context {
    host: Host,
    shared: Arc<Shared>,
}

struct Shared {
    contributions: Cell,
    panels: Mutex<BTreeMap<String, (String, Value)>>,
    order: Vec<String>,
}

impl Context {
    /// Holds a server object (as the plugin's principal: what the server
    /// grants it).
    pub fn server(&self, contract: Arc<Contract>, params: Value) -> RemoteRef {
        self.host.get(contract, params)
    }

    /// Shows `view` in the panel `id`.
    pub fn set_panel(&self, id: &str, view: Value) {
        let mut panels = self
            .shared
            .panels
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(panel) = panels.get_mut(id) {
            panel.1 = view;
        }
        let rendered = panels_json(&self.shared.order, &panels);
        drop(panels);
        let _ = self
            .shared
            .contributions
            .change(|value| vec![set_panels(value, rendered)]);
    }
}

fn panels_json(order: &[String], panels: &BTreeMap<String, (String, Value)>) -> Value {
    Value::Array(
        order
            .iter()
            .filter_map(|id| {
                let (title, view) = panels.get(id)?;
                Some(json!({"id": id, "title": title, "view": view}))
            })
            .collect(),
    )
}

fn set_panels(_value: &Value, panels: Value) -> noel_chord::delta::Op {
    noel_chord::delta::Op::Set(vec![noel_chord::delta::Seg::Key("panels".into())], panels)
}

impl Plugin {
    pub fn new(name: impl Into<String>) -> Plugin {
        Plugin {
            name: name.into(),
            tools: BTreeMap::new(),
            commands: BTreeMap::new(),
            panels: BTreeMap::new(),
        }
    }

    /// A tool the model can call: `run` gets its arguments and answers its
    /// output, or an error the model sees as the tool's failure.
    pub fn tool<F, Fut>(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        run: F,
    ) -> Plugin
    where
        F: Fn(Value, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        self.tools.insert(
            name.into(),
            Tool {
                description: description.into(),
                parameters,
                replay_safe: false,
                run: Arc::new(move |args, context| Box::pin(run(args, context))),
            },
        );
        self
    }

    /// A command a person runs (`noel client run <plugin> <name> [args]`).
    pub fn command<F, Fut>(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        run: F,
    ) -> Plugin
    where
        F: Fn(String, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        self.commands.insert(
            name.into(),
            Command {
                description: description.into(),
                run: Arc::new(move |args, context| Box::pin(run(args, context))),
            },
        );
        self
    }

    /// A panel showing `view`; `on_action` gets its buttons' actions.
    pub fn panel<F, Fut>(
        mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        view: Value,
        on_action: F,
    ) -> Plugin
    where
        F: Fn(String, Option<String>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.panels.insert(
            id.into(),
            Panel {
                title: title.into(),
                view,
                on_action: Some(Arc::new(move |action, value, context| {
                    Box::pin(on_action(action, value, context))
                })),
            },
        );
        self
    }

    /// Runs the plugin over its stdin and stdout, as `noel server` starts
    /// it, until the server closes the link.
    pub async fn run_stdio(self) {
        let (link, _) =
            noel_chord::v2::transport::framed(tokio::io::stdin(), tokio::io::stdout(), None);
        self.serve(link).await;
    }

    /// Runs the plugin over `link` to its server until the link closes.
    pub async fn serve(self, link: Link) {
        let contract = noel_client::contracts::plugin();
        let host = Host::new(HostOptions {
            address: Some(noel_client::plugin_address(&self.name)),
            ..HostOptions::default()
        });
        let order: Vec<String> = self.panels.keys().cloned().collect();
        let panels: BTreeMap<String, (String, Value)> = self
            .panels
            .iter()
            .map(|(id, panel)| (id.clone(), (panel.title.clone(), panel.view.clone())))
            .collect();
        let contributions = json!({
            "tools": self.tools.iter().map(|(name, tool)| json!({
                "name": name,
                "description": tool.description,
                "parameters": tool.parameters,
                "replaySafe": tool.replay_safe,
            })).collect::<Vec<_>>(),
            "commands": self.commands.iter().map(|(name, command)| json!({
                "name": name,
                "description": command.description,
            })).collect::<Vec<_>>(),
            "panels": panels_json(&order, &panels),
        });
        let schema = contract.declaration.variants["host"].state["contributions"].clone();
        let cell = Cell::new(schema, contributions).expect("the contributions fit the contract");
        let context = Context {
            host: host.clone(),
            shared: Arc::new(Shared {
                contributions: cell.clone(),
                panels: Mutex::new(panels),
                order,
            }),
        };
        let tools = Arc::new(self.tools);
        let commands = Arc::new(self.commands);
        let actions: Arc<BTreeMap<String, Option<ActionFn>>> = Arc::new(
            self.panels
                .into_iter()
                .map(|(id, panel)| (id, panel.on_action))
                .collect(),
        );
        host.serve(contract, move |_params, _principal| {
            let (tools, commands, actions) = (tools.clone(), commands.clone(), actions.clone());
            let (tool_context, command_context, action_context) =
                (context.clone(), context.clone(), context.clone());
            Ok(Implementation::new("host")
                .state("contributions", &cell)
                .method("callTool", move |args: Value, _| {
                    let tools = tools.clone();
                    let context = tool_context.clone();
                    async move {
                        let name = args["name"].as_str().unwrap_or_default();
                        let Some(tool) = tools.get(name) else {
                            return Ok(
                                json!({"output": format!("no tool {name}"), "isError": true}),
                            );
                        };
                        let input = args["args"]
                            .as_str()
                            .and_then(|text| serde_json::from_str(text).ok())
                            .unwrap_or_else(|| json!({}));
                        Ok(match (tool.run)(input, context).await {
                            Ok(output) => json!({"output": output, "isError": false}),
                            Err(output) => json!({"output": output, "isError": true}),
                        })
                    }
                })
                .method("runCommand", move |args: Value, _| {
                    let commands = commands.clone();
                    let context = command_context.clone();
                    async move {
                        let name = args["name"].as_str().unwrap_or_default();
                        let Some(command) = commands.get(name) else {
                            return Err(WireError::plugin("command", format!("no command {name}")));
                        };
                        let input = args["args"].as_str().unwrap_or_default().to_owned();
                        match (command.run)(input, context).await {
                            Ok(output) => Ok(json!({ "output": output })),
                            Err(error) => Err(WireError::plugin("failed", error)),
                        }
                    }
                })
                .method("panelAction", move |args: Value, _| {
                    let actions = actions.clone();
                    let context = action_context.clone();
                    async move {
                        let panel = args["panel"].as_str().unwrap_or_default();
                        let Some(Some(on_action)) = actions.get(panel) else {
                            return Err(WireError::plugin("panel", format!("no panel {panel}")));
                        };
                        let action = args["action"].as_str().unwrap_or_default().to_owned();
                        let value = args["value"].as_str().map(str::to_owned);
                        on_action(action, value, context)
                            .await
                            .map(|()| Value::Null)
                            .map_err(|error| WireError::plugin("failed", error))
                    }
                }))
        });
        host.connect(
            link.clone(),
            LinkOptions {
                trusted: true,
                ..LinkOptions::new(json!({"kind": "server"}))
            },
        );
        link.closed().await;
        host.close().await;
    }
}

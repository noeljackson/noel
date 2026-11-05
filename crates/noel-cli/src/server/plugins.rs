//! Plugins (roadmap phase 7, spec §13): processes the server starts from
//! manifests and keeps running, each a Chord v2 host at `Plugin({name})`
//! that serves `noel.plugin` (see noel-plugin, the Rust SDK).
//!
//! - **Manifests:** `<agent dir>/plugins/<dir>/plugin.json`:
//!   `{name, version?, description?, command: [program, args...],
//!   permissions: [...]}`, the program relative to the manifest's
//!   directory when it is there.
//! - **Grants:** `<agent dir>/plugins.json`: `{"<name>": {"permissions":
//!   [...]}}`. A plugin gets the permissions it requests that are granted:
//!   `tools`, `commands` and `panels` for its contributions to count,
//!   `sessions` to hold `noel.sessions`. Its link is stamped
//!   `{kind: "plugin", name, permissions}`; every object checks that
//!   principal.
//! - **Supervision:** each plugin runs with its stdin and stdout as a framed
//!   link (the `plugin` route's link for its address), its stderr in
//!   `plugins/<name>.log` under the runtime directory. One that exits is
//!   started again after a backoff (0.5 s, doubling to 30 s; back to 0.5 s
//!   after a minute up).
//! - **`noel.plugins`** (at `Server()`): the directory of plugins with their
//!   granted contributions; the owner runs commands and panel actions, a
//!   worker runs tools.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use noel_chord::v2::host::{Host, LinkOptions, RemoteRef, Resolved, Route, StateUpdate};
use noel_chord::v2::message::Address;
use noel_chord::v2::object::{BoxFuture, Cell, Implementation};
use noel_chord::v2::transport::framed;
use noel_chord::v2::{ChordError, Link, WireError};
use noel_client::{contracts, plugin_address};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::errors::{forbidden, unavailable};
use super::RuntimeDir;

const BACKOFF_FIRST: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Up this long, a plugin's backoff starts over.
const STABLE: Duration = Duration::from_secs(60);

/// A plugin's manifest.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The program and its arguments, for a process plugin.
    #[serde(default)]
    pub command: Vec<String>,
    /// The script, for a djs plugin run in process (phase 8).
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub limits: super::djs_plugins::Limits,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(skip)]
    pub dir: PathBuf,
}

/// The manifests under `<agent dir>/plugins`, by name; one that does not
/// parse is reported and skipped.
pub(crate) fn manifests(agent_dir: &Path) -> Vec<Manifest> {
    let Ok(entries) = fs::read_dir(agent_dir.join("plugins")) else {
        return Vec::new();
    };
    let mut found: BTreeMap<String, Manifest> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path().join("plugin.json");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<Manifest>(&text) {
            Ok(mut manifest)
                if valid_name(&manifest.name)
                    && (manifest.command.is_empty() != manifest.script.is_none()) =>
            {
                manifest.dir = entry.path();
                found.insert(manifest.name.clone(), manifest);
            }
            Ok(_) => eprintln!(
                "noel server: plugin {}: needs a name of [a-z0-9-] and a command or a script",
                path.display()
            ),
            Err(error) => eprintln!("noel server: plugin {}: {error}", path.display()),
        }
    }
    found.into_values().collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The permissions granted in `<agent dir>/plugins.json`, by plugin.
pub(crate) fn grants(agent_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let Ok(text) = fs::read_to_string(agent_dir.join("plugins.json")) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(plugins)) = serde_json::from_str::<Value>(&text) else {
        eprintln!("noel server: plugins.json is no JSON object; no permissions granted");
        return BTreeMap::new();
    };
    plugins
        .into_iter()
        .map(|(name, grant)| {
            let permissions = grant["permissions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            (name, permissions)
        })
        .collect()
}

/// A plugin principal's permissions, if `principal` is a plugin's.
pub(crate) fn plugin_permissions(principal: &Value) -> Option<Vec<&str>> {
    (principal.get("kind").and_then(Value::as_str) == Some("plugin")).then(|| {
        principal["permissions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    })
}

struct Supervised {
    manifest: Manifest,
    granted: Vec<String>,
    link: Option<Link>,
    running: bool,
    restarts: u64,
    error: Option<String>,
    contributions: Value,
    /// The server's hold on the plugin's `noel.plugin`.
    reference: Option<Arc<RemoteRef>>,
}

pub(crate) struct Plugins {
    host: Host,
    dir: RuntimeDir,
    state: Mutex<BTreeMap<String, Supervised>>,
    directory: Cell,
    linked: Notify,
    closing: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn empty_contributions() -> Value {
    json!({"tools": [], "commands": [], "panels": []})
}

impl Plugins {
    /// Starts every plugin of `agent_dir` and serves `noel.plugins`.
    pub(crate) fn start(
        host: &Host,
        dir: &RuntimeDir,
        agent_dir: &Path,
    ) -> anyhow::Result<Arc<Plugins>> {
        let schema = contracts::plugins().declaration.variants["owner"].state["directory"].clone();
        let directory = Cell::new(schema, json!({"plugins": []}))?;
        let grants = grants(agent_dir);
        let state = manifests(agent_dir)
            .into_iter()
            .map(|manifest| {
                let granted = manifest
                    .permissions
                    .iter()
                    .filter(|permission| {
                        grants
                            .get(&manifest.name)
                            .is_some_and(|granted| granted.contains(permission))
                    })
                    .cloned()
                    .collect();
                (
                    manifest.name.clone(),
                    Supervised {
                        manifest,
                        granted,
                        link: None,
                        running: false,
                        restarts: 0,
                        error: None,
                        contributions: empty_contributions(),
                        reference: None,
                    },
                )
            })
            .collect();
        let plugins = Arc::new(Plugins {
            host: host.clone(),
            dir: dir.clone(),
            state: Mutex::new(state),
            directory,
            linked: Notify::new(),
            closing: AtomicBool::new(false),
        });
        host.route("plugin", PluginRoute(plugins.clone()));
        serve(host, plugins.clone());
        let names: Vec<String> = lock(&plugins.state).keys().cloned().collect();
        for name in names {
            tokio::spawn(plugins.clone().supervise(name));
        }
        plugins.publish();
        Ok(plugins)
    }

    /// Stops starting plugins (the server exits).
    pub(crate) fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    /// Keeps the plugin `name` running.
    async fn supervise(self: Arc<Self>, name: String) {
        let mut backoff = BACKOFF_FIRST;
        loop {
            if self.closing.load(Ordering::SeqCst) {
                return;
            }
            let started = tokio::time::Instant::now();
            match self.spawn(&name) {
                Ok(running) => {
                    let stopped = running.await;
                    let mut state = lock(&self.state);
                    if let Some(plugin) = state.get_mut(&name) {
                        plugin.running = false;
                        plugin.link = None;
                        plugin.restarts += 1;
                        plugin.error = Some(stopped);
                    }
                    drop(state);
                    eprintln!("noel server: plugin {name} stopped; starting it again");
                }
                Err(error) => {
                    eprintln!("noel server: plugin {name}: {error}");
                    if let Some(plugin) = lock(&self.state).get_mut(&name) {
                        plugin.error = Some(error);
                    }
                }
            }
            self.publish();
            if started.elapsed() >= STABLE {
                backoff = BACKOFF_FIRST;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Starts the plugin and runs its link; the future resolves, with why,
    /// when it stops.
    fn spawn(self: &Arc<Self>, name: &str) -> Result<BoxFuture<String>, String> {
        let (manifest, granted) = {
            let state = lock(&self.state);
            let plugin = state.get(name).ok_or("no such plugin")?;
            (plugin.manifest.clone(), plugin.granted.clone())
        };
        if let Some(script) = &manifest.script {
            let script = Arc::new(super::djs_plugins::Script::load(
                &manifest.dir,
                script,
                manifest.limits.clone(),
            )?);
            let (outer, inner) = Link::memory_pair();
            let serving = tokio::spawn(super::djs_plugins::serve(name.to_owned(), script, inner));
            self.linked(name, outer, granted);
            eprintln!("noel server: started script plugin {name}");
            return Ok(Box::pin(async move {
                match serving.await {
                    Ok(Ok(())) => "its link closed".to_owned(),
                    Ok(Err(error)) => error,
                    Err(error) => format!("lost: {error}"),
                }
            }));
        }
        let program = {
            let local = manifest.dir.join(&manifest.command[0]);
            if local.is_file() {
                local
            } else {
                PathBuf::from(&manifest.command[0])
            }
        };
        let logs = self.dir.root().join("plugins");
        let _ = fs::create_dir_all(&logs);
        let log = File::options()
            .create(true)
            .append(true)
            .open(logs.join(format!("{name}.log")))
            .map_err(|error| format!("its log: {error}"))?;
        let mut child = tokio::process::Command::new(&program)
            .args(&manifest.command[1..])
            .current_dir(&manifest.dir)
            .env("NOEL_PLUGIN_NAME", name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("start {}: {error}", program.display()))?;
        let stdout = child.stdout.take().expect("piped");
        let stdin = child.stdin.take().expect("piped");
        let (link, _) = framed(stdout, stdin, None);
        self.linked(name, link, granted);
        eprintln!("noel server: started plugin {name} (pid {:?})", child.id());
        Ok(Box::pin(async move {
            match child.wait().await {
                Ok(status) => format!("exited: {status}"),
                Err(error) => format!("lost: {error}"),
            }
        }))
    }

    /// Runs `link` as the plugin's: the `plugin` route's link for its
    /// address, stamped with its granted permissions.
    fn linked(self: &Arc<Self>, name: &str, link: Link, granted: Vec<String>) {
        let principal = json!({"kind": "plugin", "name": name, "permissions": granted});
        self.host.route_link(
            plugin_address(name),
            link.clone(),
            LinkOptions::new(principal),
        );
        let first = {
            let mut state = lock(&self.state);
            let plugin = state.get_mut(name).expect("the plugin");
            plugin.link = Some(link);
            plugin.running = true;
            plugin.error = None;
            plugin.reference.is_none()
        };
        self.linked.notify_waiters();
        if first {
            self.follow(name);
        }
    }

    /// Holds the plugin's `noel.plugin` and follows its contributions.
    fn follow(self: &Arc<Self>, name: &str) {
        let reference = Arc::new(
            self.host
                .get(contracts::plugin(), json!({ "plugin": name })),
        );
        let mut updates = reference.subscribe("contributions");
        if let Some(plugin) = lock(&self.state).get_mut(name) {
            plugin.reference = Some(reference);
        }
        let (plugins, name) = (self.clone(), name.to_owned());
        tokio::spawn(async move {
            while let Some(update) = updates.next().await {
                let value = match update {
                    StateUpdate::Snapshot(value) | StateUpdate::Ops { value, .. } => value,
                    StateUpdate::Failed(_) => continue,
                };
                if let Some(plugin) = lock(&plugins.state).get_mut(&name) {
                    plugin.contributions = value;
                }
                plugins.publish();
            }
        });
    }

    /// The directory as clients see it: each plugin's contributions of the
    /// kinds it was granted.
    fn publish(&self) {
        let state = lock(&self.state);
        let entries: Vec<Value> = state
            .values()
            .map(|plugin| {
                let granted = |kind: &str| plugin.granted.iter().any(|g| g == kind);
                let part = |kind: &str| {
                    if granted(kind) {
                        plugin.contributions[kind].clone()
                    } else {
                        json!([])
                    }
                };
                let mut entry = json!({
                    "name": plugin.manifest.name,
                    "running": plugin.running,
                    "restarts": plugin.restarts,
                    "requested": plugin.manifest.permissions,
                    "granted": plugin.granted,
                    "contributions": {
                        "tools": part("tools"),
                        "commands": part("commands"),
                        "panels": part("panels"),
                    },
                });
                if let Some(version) = &plugin.manifest.version {
                    entry["version"] = json!(version);
                }
                if let Some(description) = &plugin.manifest.description {
                    entry["description"] = json!(description);
                }
                if let Some(error) = &plugin.error {
                    entry["error"] = json!(error);
                }
                entry
            })
            .collect();
        drop(state);
        if let Err(error) = self.directory.update(json!({ "plugins": entries })) {
            eprintln!("noel server: the plugin directory: {error}");
        }
    }

    /// The plugin's reference, if `kind` is granted to it.
    fn granted(&self, plugin: &str, kind: &str) -> Result<Arc<RemoteRef>, WireError> {
        let state = lock(&self.state);
        let Some(supervised) = state.get(plugin) else {
            return Err(WireError::plugin("plugin", format!("no plugin {plugin}")));
        };
        if !supervised.granted.iter().any(|granted| granted == kind) {
            return Err(forbidden(format!(
                "plugin {plugin} is not granted the permission {kind}"
            )));
        }
        supervised
            .reference
            .clone()
            .ok_or_else(|| unavailable(format!("plugin {plugin} has not started")).into())
    }

    /// Calls `method` of the plugin's `noel.plugin` when `kind` is granted.
    async fn forward(&self, kind: &str, method: &str, args: Value) -> Result<Value, WireError> {
        let plugin = args["plugin"].as_str().unwrap_or_default().to_owned();
        let reference = self.granted(&plugin, kind)?;
        let mut args = args;
        if let Some(fields) = args.as_object_mut() {
            fields.remove("plugin");
        }
        reference.call(method, args).await
    }
}

/// Serves `noel.plugins`: the owner runs commands and panel actions, a
/// worker runs tools; any other principal is refused.
fn serve(host: &Host, plugins: Arc<Plugins>) {
    host.serve(contracts::plugins(), move |_params, principal| {
        let kind = principal.get("kind").and_then(Value::as_str);
        let (command, action, tool) = (plugins.clone(), plugins.clone(), plugins.clone());
        match kind {
            Some("owner") => Ok(Implementation::new("owner")
                .state("directory", &plugins.directory)
                .method("runCommand", move |args: Value, _| {
                    let plugins = command.clone();
                    async move { plugins.forward("commands", "runCommand", args).await }
                })
                .method("panelAction", move |args: Value, _| {
                    let plugins = action.clone();
                    async move { plugins.forward("panels", "panelAction", args).await }
                })),
            Some("worker") => Ok(Implementation::new("worker")
                .state("directory", &plugins.directory)
                .method("callTool", move |args: Value, _| {
                    let plugins = tool.clone();
                    async move { plugins.forward("tools", "callTool", args).await }
                })),
            _ => Err(forbidden("noel.plugins is for the owner and workers")),
        }
    });
}

/// The `plugin` route: the supervisor's link to a plugin, waiting while it
/// starts. Only this server's own references reach plugins.
struct PluginRoute(Arc<Plugins>);

impl Route for PluginRoute {
    fn authorize(&self, principal: &Value, _address: &Address) -> bool {
        principal.is_null()
    }

    fn resolve(&self, address: Address) -> BoxFuture<Result<Resolved, ChordError>> {
        let plugins = self.0.clone();
        Box::pin(async move {
            let name = address.params["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let linked = plugins.linked.notified();
                {
                    let state = lock(&plugins.state);
                    let Some(plugin) = state.get(&name) else {
                        return Err(unavailable(format!("no plugin {name}")));
                    };
                    if let Some(link) = plugin.link.clone().filter(|link| !link.is_closed()) {
                        let principal = json!({
                            "kind": "plugin",
                            "name": name,
                            "permissions": plugin.granted,
                        });
                        return Ok(Resolved {
                            link,
                            options: LinkOptions::new(principal),
                        });
                    }
                }
                if tokio::time::timeout_at(deadline, linked).await.is_err() {
                    return Err(unavailable(format!("plugin {name} is not running")));
                }
            }
        })
    }
}

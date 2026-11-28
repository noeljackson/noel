//! In-process Rust tools, including Durable's suspendable tool calls.
//!
//! A model call is represented by a `pi.tool/*` durable task keyed by its
//! call id. A park commits that task before the invocation returns and the
//! next invocation receives its checkpoint and wake reason. The task record
//! is appended directly to the conversation journal because the session
//! controller lends its runtime to the active model run while a tool runs.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use noel_core::plugins::{PluginTool, PluginToolHost, PluginToolOutput};
use noel_core::storage::Storage;
use noel_core::DurableTaskRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

use crate::clock::Clock;
use crate::tasks::{SignalResult, SignalState, SignalToken};

/// Why a parked tool invocation runs again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum ToolWake {
    Time,
    Signal { signal: String, key: String },
    Tasks { task_ids: Vec<String> },
}

/// How a tool waits for owned tasks (slice 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    FailFast,
    AllSettled,
    Any,
}

/// A durable suspension returned by a tool instead of a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolPark {
    Sleep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        signals: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint: Option<Value>,
    },
    Wait {
        on: Vec<String>,
        policy: JoinPolicy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint: Option<Value>,
    },
}

/// What a tool call runs with.
pub struct ToolContext<'a> {
    /// The arguments the model sent, parsed.
    pub arguments: &'a Value,
    /// The session's working directory.
    pub cwd: &'a Path,
    pub task_id: &'a str,
    pub call_id: &'a str,
    pub wake: Option<&'a ToolWake>,
    pub checkpoint: Option<&'a Value>,
    engine: Weak<RegistryHost>,
}

impl ToolContext<'_> {
    pub fn now(&self) -> u64 {
        self.engine.upgrade().map_or(0, |engine| engine.clock.now())
    }

    /// First writer wins, and the memo is durable before this returns.
    pub async fn memo(&self, name: &str, candidate: Value) -> Value {
        let Some(engine) = self.engine.upgrade() else {
            return candidate;
        };
        engine.memo(self.task_id, name, candidate)
    }

    pub async fn read_signal<T: serde::de::DeserializeOwned>(
        &self,
        token: &SignalToken<T>,
        key: &str,
    ) -> Option<SignalState<T>> {
        let engine = self.engine.upgrade()?;
        engine.read_signal(self.task_id, token.name, key)
    }

    pub async fn read_document<T: serde::de::DeserializeOwned>(
        &self,
        document: &crate::Document<T>,
        key: &str,
    ) -> Option<T> {
        let engine = self.engine.upgrade()?;
        crate::documents::decode(engine.read_document(document.name, key))
    }

    pub async fn write_document<T: serde::Serialize>(
        &self,
        document: &crate::Document<T>,
        key: impl Into<String>,
        value: &T,
    ) -> Result<(), String> {
        let Some(engine) = self.engine.upgrade() else {
            return Err("the harness is closed".into());
        };
        engine.write_documents(vec![noel_core::DurableDocumentWrite {
            document: document.name.to_owned(),
            key: key.into(),
            value: Some(serde_json::to_value(value).unwrap_or(Value::Null)),
        }])
    }

    /// Executes another registered tool as a child call and waits for it.
    pub async fn execute_tool(&self, name: &str, arguments: Value, key: &str) -> ToolResult {
        let Some(engine) = self.engine.upgrade() else {
            return ToolResult::error("the harness is closed");
        };
        let call_id = format!("{}/{}", self.call_id, key);
        Box::pin(engine.call_tool(name, &call_id, arguments)).await
    }

    /// Starts a nested call without holding this invocation. Its task id
    /// can be used in a `ToolPark::Wait`; `result()` joins it directly.
    pub fn execute_tool_detached(&self, name: &str, arguments: Value, key: &str) -> NestedToolCall {
        let call_id = format!("{}/{}", self.call_id, key);
        let task_id = RegistryHost::task_id(&call_id);
        let (send, receive) = tokio::sync::oneshot::channel();
        if let Some(engine) = self.engine.upgrade() {
            let name = name.to_owned();
            tokio::spawn(async move {
                let result = engine.call_tool(&name, &call_id, arguments).await;
                let _ = send.send(result);
            });
        }
        NestedToolCall {
            task_id,
            result: Mutex::new(Some(receive)),
        }
    }
}

/// A nested call started without waiting for it.
pub struct NestedToolCall {
    pub task_id: String,
    result: Mutex<Option<tokio::sync::oneshot::Receiver<ToolResult>>>,
}

impl NestedToolCall {
    pub async fn result(&self) -> ToolResult {
        let receive = self
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match receive {
            Some(receive) => receive
                .await
                .unwrap_or_else(|_| ToolResult::error("the nested call ended")),
            None => ToolResult::error("the nested call result was already taken"),
        }
    }
}

/// A tool's result for the model, or a durable park.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
    pub park: Option<ToolPark>,
}

impl ToolResult {
    pub fn text(output: impl Into<String>) -> ToolResult {
        ToolResult {
            output: output.into(),
            is_error: false,
            park: None,
        }
    }

    pub fn error(output: impl Into<String>) -> ToolResult {
        ToolResult {
            output: output.into(),
            is_error: true,
            park: None,
        }
    }

    pub fn sleep(
        until: Option<u64>,
        signals: Vec<(String, String)>,
        checkpoint: Option<Value>,
    ) -> ToolResult {
        ToolResult {
            output: String::new(),
            is_error: false,
            park: Some(ToolPark::Sleep {
                until,
                signals,
                checkpoint,
            }),
        }
    }

    pub fn wait(on: Vec<String>, policy: JoinPolicy, checkpoint: Option<Value>) -> ToolResult {
        ToolResult {
            output: String::new(),
            is_error: false,
            park: Some(ToolPark::Wait {
                on,
                policy,
                checkpoint,
            }),
        }
    }
}

/// An in-process Rust tool (Durable's `defineTool`).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn replay_safe(&self) -> bool {
        false
    }
    fn execute<'a>(
        &'a self,
        context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

    /// Cleans up external work after an abort of a parked call. It may run
    /// more than once and therefore must be idempotent.
    fn cancel<'a>(
        &'a self,
        _context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

/// A decision made before a tool invocation. Gates run again after a wake.
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    Allow,
    Block(String),
    Park(ToolPark),
}

pub trait ToolGate: Send + Sync {
    fn before<'a>(
        &'a self,
        tool: &'a str,
        context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = GateDecision> + Send + 'a>>;
}

/// The tools and gates a harness offers.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    gates: Vec<Arc<dyn ToolGate>>,
}

impl ToolRegistry {
    pub fn new() -> ToolRegistry {
        ToolRegistry::default()
    }

    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_gate(mut self, gate: Arc<dyn ToolGate>) -> Self {
        self.gates.push(gate);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub(crate) fn install(
        self,
        runtime: &mut noel_core::Runtime,
        storage: Storage,
        clock: Arc<Clock>,
    ) -> Arc<RegistryHost> {
        let records = runtime.session().tasks.clone();
        let cwd = runtime.session().cwd.clone();
        let conversation_id = runtime.session().session_id.clone();
        let host = Arc::new_cyclic(|weak| RegistryHost {
            own: weak.clone(),
            tools: self.tools,
            gates: self.gates,
            storage,
            clock,
            cwd,
            conversation_id,
            records: Mutex::new(records),
            documents: Mutex::new(runtime.session().documents.clone()),
            notifies: Mutex::new(BTreeMap::new()),
        });
        let tools = host
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
        runtime.set_plugin_tools(host.clone(), tools);
        host
    }
}

pub(crate) struct RegistryHost {
    own: Weak<RegistryHost>,
    tools: Vec<Arc<dyn Tool>>,
    gates: Vec<Arc<dyn ToolGate>>,
    storage: Storage,
    clock: Arc<Clock>,
    cwd: PathBuf,
    #[allow(dead_code)]
    conversation_id: String,
    records: Mutex<BTreeMap<String, DurableTaskRecord>>,
    documents: Mutex<BTreeMap<String, BTreeMap<String, Value>>>,
    notifies: Mutex<BTreeMap<String, Arc<Notify>>>,
}

impl RegistryHost {
    fn task_id(call_id: &str) -> String {
        format!("tool:{call_id}")
    }

    fn notify(&self, id: &str) -> Arc<Notify> {
        self.notifies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    fn save(&self, record: DurableTaskRecord) -> Result<(), String> {
        noel_core::storage::append_task_record(&self.storage, record.clone())
            .map_err(|error| error.to_string())?;
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(record.id.clone(), record);
        Ok(())
    }

    fn record(&self, id: &str) -> Option<DurableTaskRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn memo(&self, id: &str, name: &str, candidate: Value) -> Value {
        let Some(mut record) = self.record(id) else {
            return candidate;
        };
        if let Some(value) = record.memos.get(name) {
            return value.clone();
        }
        record.memos.insert(name.to_owned(), candidate.clone());
        let _ = self.save(record);
        candidate
    }

    fn read_document(&self, document: &str, key: &str) -> Option<Value> {
        self.documents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(document)
            .and_then(|document| document.get(key))
            .cloned()
    }

    fn write_documents(&self, writes: Vec<noel_core::DurableDocumentWrite>) -> Result<(), String> {
        noel_core::storage::append_document_records(&self.storage, writes.clone())
            .map_err(|error| error.to_string())?;
        let mut documents = self
            .documents
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for write in writes {
            match write.value {
                Some(value) => {
                    documents
                        .entry(write.document)
                        .or_default()
                        .insert(write.key, value);
                }
                None => {
                    if let Some(document) = documents.get_mut(&write.document) {
                        document.remove(&write.key);
                    }
                }
            }
        }
        Ok(())
    }

    fn read_signal<T: serde::de::DeserializeOwned>(
        &self,
        id: &str,
        signal: &str,
        key: &str,
    ) -> Option<SignalState<T>> {
        let record = self.record(id)?;
        let state = record.memos.get(&format!("pi.signal.{signal}/{key}"))?;
        if state.get("expired") == Some(&Value::Bool(true)) {
            return Some(SignalState::Expired);
        }
        serde_json::from_value(state.get("value")?.clone())
            .ok()
            .map(SignalState::Value)
    }

    pub(crate) fn signal(&self, id: &str, signal: &str, key: &str, value: Value) -> SignalResult {
        let Some(mut record) = self.record(id) else {
            return SignalResult::NotFound;
        };
        if record.abort_requested || record.state["status"] == "terminal" {
            return SignalResult::Closed;
        }
        let memo = format!("pi.signal.{signal}/{key}");
        if record.memos.contains_key(&memo) {
            return SignalResult::Duplicate;
        }
        if let Some(until) = record.state.get("until").and_then(Value::as_u64)
            && self.clock.now() >= until
        {
            record
                .memos
                .insert(memo, serde_json::json!({"expired": true}));
            let _ = self.save(record);
            return SignalResult::Closed;
        }
        record
            .memos
            .insert(memo, serde_json::json!({"version": 1, "value": value}));
        if self.save(record).is_err() {
            return SignalResult::Closed;
        }
        self.notify(id).notify_one();
        SignalResult::Delivered
    }

    pub(crate) fn wake(&self) -> Option<u64> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|record| record.kind.starts_with("pi.tool/"))
            .filter_map(|record| record.state.get("until").and_then(Value::as_u64))
            .min()
    }

    pub(crate) fn resume(&self) {
        for notify in self
            .notifies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            notify.notify_one();
        }
    }

    #[allow(dead_code)] // slice 7's child joins read records through this.
    pub(crate) fn get(&self, id: &str) -> Option<DurableTaskRecord> {
        self.record(id)
    }

    pub(crate) fn abort(&self, id: &str) -> bool {
        let Some(mut record) = self.record(id) else {
            return false;
        };
        if record.state["status"] == "terminal" {
            return true;
        }
        record.abort_requested = true;
        let _ = self.save(record.clone());
        self.notify(id).notify_one();
        if let Some(engine) = self.own.upgrade()
            && let Some(name) = record.kind.strip_prefix("pi.tool/")
            && let Some(tool) = self.tools.iter().find(|tool| tool.name() == name).cloned()
        {
            let id = id.to_owned();
            let call_id = id.strip_prefix("tool:").unwrap_or(&id).to_owned();
            tokio::spawn(async move {
                let checkpoint = record
                    .state
                    .get("checkpoint")
                    .filter(|v| !v.is_null())
                    .cloned();
                tool.cancel(engine.context(
                    &record.input,
                    &id,
                    &call_id,
                    None,
                    checkpoint.as_ref(),
                ))
                .await;
            });
        }
        true
    }

    pub(crate) fn abort_all(&self) {
        let ids = self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|record| {
                record.kind.starts_with("pi.tool/") && record.state["status"] != "terminal"
            })
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.abort(&id);
        }
    }

    fn context<'a>(
        &'a self,
        args: &'a Value,
        id: &'a str,
        call_id: &'a str,
        wake: Option<&'a ToolWake>,
        checkpoint: Option<&'a Value>,
    ) -> ToolContext<'a> {
        ToolContext {
            arguments: args,
            cwd: &self.cwd,
            task_id: id,
            call_id,
            wake,
            checkpoint,
            engine: self.own.clone(),
        }
    }

    async fn call_tool(&self, name: &str, call_id: &str, args: Value) -> ToolResult {
        let Some(tool) = self
            .tools
            .iter()
            .find(|candidate| candidate.name() == name)
            .cloned()
        else {
            return ToolResult::error(format!("tool {name} is not registered"));
        };
        let id = Self::task_id(call_id);
        let mut record = self.record(&id).unwrap_or_else(|| DurableTaskRecord {
            id: id.clone(),
            kind: format!("pi.tool/{name}"),
            version: 1,
            input: args.clone(),
            owner_task_id: call_id
                .rsplit_once('/')
                .map(|(parent, _)| Self::task_id(parent)),
            state: serde_json::json!({"status": "pending", "checkpoint": null}),
            background: false,
            abort_requested: false,
            restarts: 0,
            memos: BTreeMap::new(),
        });
        if self.record(&id).is_none() && self.save(record.clone()).is_err() {
            return ToolResult::error("could not save the tool call");
        }

        'run: loop {
            if record.abort_requested {
                let checkpoint = record
                    .state
                    .get("checkpoint")
                    .filter(|v| !v.is_null())
                    .cloned();
                tool.cancel(self.context(&args, &id, call_id, None, checkpoint.as_ref()))
                    .await;
                record.state = serde_json::json!({"status": "terminal", "outcome": {"output": "Tool call aborted", "is_error": true}});
                let _ = self.save(record);
                return ToolResult::error("Tool call aborted");
            }
            if record.state["status"] == "terminal" {
                return serde_json::from_value::<StoredResult>(record.state["outcome"].clone())
                    .map(ToolResult::from)
                    .unwrap_or_else(|_| ToolResult::error("tool call ended without a result"));
            }

            let checkpoint = record
                .state
                .get("checkpoint")
                .filter(|v| !v.is_null())
                .cloned();
            let wake = match self.park_wake(&mut record) {
                ParkState::Ready(wake) => wake,
                ParkState::Invalid(error) => {
                    let result = ToolResult::error(error);
                    self.finish(&mut record, &result);
                    return result;
                }
                ParkState::Waiting => {
                    self.notify(&id).notified().await;
                    record = self.record(&id).unwrap_or(record);
                    continue;
                }
            };

            for gate in &self.gates {
                match gate
                    .before(
                        name,
                        self.context(&args, &id, call_id, wake.as_ref(), checkpoint.as_ref()),
                    )
                    .await
                {
                    GateDecision::Allow => {}
                    GateDecision::Block(reason) => {
                        let result = ToolResult::error(reason);
                        self.finish(&mut record, &result);
                        return result;
                    }
                    GateDecision::Park(park) => {
                        if let Err(error) = self.park(&mut record, park) {
                            let result = ToolResult::error(error);
                            self.finish(&mut record, &result);
                            return result;
                        }
                        record = self.record(&id).unwrap_or(record);
                        continue 'run;
                    }
                }
            }

            record.state = serde_json::json!({
                "status": "running",
                "checkpoint": checkpoint,
                "wake": wake,
            });
            if self.save(record.clone()).is_err() {
                return ToolResult::error("could not save the running tool call");
            }
            let result = tool
                .execute(self.context(&args, &id, call_id, wake.as_ref(), checkpoint.as_ref()))
                .await;
            if let Some(park) = result.park.clone() {
                if let Err(error) = self.park(&mut record, park) {
                    let result = ToolResult::error(error);
                    self.finish(&mut record, &result);
                    return result;
                }
                record = self.record(&id).unwrap_or(record);
                continue;
            }
            self.finish(&mut record, &result);
            return result;
        }
    }

    fn finish(&self, record: &mut DurableTaskRecord, result: &ToolResult) {
        let stored = StoredResult {
            output: result.output.clone(),
            is_error: result.is_error,
        };
        record.state = serde_json::json!({"status": "terminal", "outcome": stored});
        let _ = self.save(record.clone());
        self.notify(&record.id).notify_one();
        self.notify_task_waiters(&record.id);
    }

    fn notify_task_waiters(&self, ended: &str) {
        let waiting = self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|record| {
                record.state["status"] == "waiting"
                    && record.state["on"]
                        .as_array()
                        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(ended)))
            })
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        for id in waiting {
            self.notify(&id).notify_one();
        }
    }

    fn park(&self, record: &mut DurableTaskRecord, park: ToolPark) -> Result<(), String> {
        match park {
            ToolPark::Sleep {
                until,
                signals,
                checkpoint,
            } => {
                if until.is_none() && signals.is_empty() {
                    return Err("a tool cannot sleep without a deadline or signals".into());
                }
                record.state = serde_json::json!({
                    "status": "sleeping", "checkpoint": checkpoint,
                    "until": until, "signals": signals.iter().map(|(signal, key)| serde_json::json!({"signal": signal, "key": key})).collect::<Vec<_>>()
                });
            }
            ToolPark::Wait {
                on,
                policy,
                checkpoint,
            } => {
                if on.is_empty() {
                    return Err("a tool cannot wait for no tasks".into());
                }
                record.state = serde_json::json!({"status": "waiting", "checkpoint": checkpoint, "on": on, "policy": policy});
            }
        }
        self.save(record.clone())
    }

    fn park_wake(&self, record: &mut DurableTaskRecord) -> ParkState {
        match record.state["status"].as_str() {
            Some("pending") | Some("running") => ParkState::Ready(None),
            Some("sleeping") => {
                let signals = record.state["signals"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for signal in &signals {
                    let (Some(kind), Some(key)) =
                        (signal["signal"].as_str(), signal["key"].as_str())
                    else {
                        continue;
                    };
                    if record
                        .memos
                        .get(&format!("pi.signal.{kind}/{key}"))
                        .is_some_and(|value| value.get("value").is_some())
                    {
                        return ParkState::Ready(Some(ToolWake::Signal {
                            signal: kind.to_owned(),
                            key: key.to_owned(),
                        }));
                    }
                }
                if record
                    .state
                    .get("until")
                    .and_then(Value::as_u64)
                    .is_some_and(|until| self.clock.now() >= until)
                {
                    for signal in signals {
                        if let (Some(kind), Some(key)) =
                            (signal["signal"].as_str(), signal["key"].as_str())
                        {
                            record
                                .memos
                                .entry(format!("pi.signal.{kind}/{key}"))
                                .or_insert_with(|| serde_json::json!({"expired": true}));
                        }
                    }
                    let _ = self.save(record.clone());
                    ParkState::Ready(Some(ToolWake::Time))
                } else {
                    ParkState::Waiting
                }
            }
            Some("waiting") => {
                let ids = record.state["on"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                let ended = ids
                    .iter()
                    .filter(|id| {
                        self.record(id)
                            .is_some_and(|task| task.state["status"] == "terminal")
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let policy = serde_json::from_value::<JoinPolicy>(record.state["policy"].clone())
                    .unwrap_or(JoinPolicy::AllSettled);
                let failed = ended.iter().any(|id| {
                    self.record(id).is_some_and(|task| {
                        task.state["outcome"]["is_error"] == Value::Bool(true)
                            || task.state["outcome"]["status"] == "failed"
                            || task.state["outcome"]["status"] == "aborted"
                    })
                });
                let ready = match policy {
                    JoinPolicy::Any => !ended.is_empty(),
                    JoinPolicy::AllSettled => ended.len() == ids.len(),
                    JoinPolicy::FailFast => failed || ended.len() == ids.len(),
                };
                if ready {
                    if policy == JoinPolicy::FailFast && failed {
                        for id in ids.iter().filter(|id| !ended.contains(id)) {
                            self.abort(id);
                        }
                    }
                    ParkState::Ready(Some(ToolWake::Tasks { task_ids: ended }))
                } else {
                    ParkState::Waiting
                }
            }
            Some("terminal") => ParkState::Ready(None),
            _ => ParkState::Invalid("invalid durable tool state".into()),
        }
    }
}

enum ParkState {
    Ready(Option<ToolWake>),
    Waiting,
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredResult {
    output: String,
    #[serde(default)]
    is_error: bool,
}

impl From<StoredResult> for ToolResult {
    fn from(value: StoredResult) -> Self {
        ToolResult {
            output: value.output,
            is_error: value.is_error,
            park: None,
        }
    }
}

impl PluginToolHost for RegistryHost {
    fn call(
        &self,
        _plugin: &str,
        name: &str,
        call_id: &str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = PluginToolOutput> + Send>> {
        let engine = self.own.upgrade();
        let name = name.to_owned();
        let call_id = call_id.to_owned();
        Box::pin(async move {
            let result = match engine {
                Some(engine) => engine.call_tool(&name, &call_id, args).await,
                None => ToolResult::error("the harness is closed"),
            };
            PluginToolOutput {
                output: result.output,
                is_error: result.is_error,
            }
        })
    }
}

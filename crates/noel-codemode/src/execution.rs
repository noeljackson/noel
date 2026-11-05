//! The host side of one execution (upstream `runtime/execution.ts`): the tool
//! table, call records, output, the deadline, the caller's abort, and the
//! decoding of what the VM reports.
//!
//! The VM runs on its own thread ([`crate::vm::VmSession`]); this side is one
//! async loop. Tool calls are futures polled together in that loop, so a
//! script's `Promise.all` runs them in parallel, and ending the execution
//! drops (cancels) the ones still running. The first terminal event wins:
//! the script's end, a crash, the deadline or the abort.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc as async_mpsc;

use crate::identifier::to_codemode_identifier;
use crate::vm::{
    CallTarget, OutputItem, VmGlobal, VmMessage, VmResult, VmSession, VmStart, VmTool,
};

/// A boxed future borrowed for `'a`.
pub type CallFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<Value>, String>> + Send + 'a>>;

/// Runs the script's calls. `args` is the call's argument (the first one,
/// or every argument as an array for a `spread` global); `Ok` is the JSON
/// value the script gets, `Err` the message of the `Error` it gets.
pub trait Executor: Sync {
    fn call<'a>(&'a self, target: CallTarget, name: &'a str, args: Option<Value>)
        -> CallFuture<'a>;
}

/// A tool the script sees: `tools[js_name]` (default: the name as an
/// identifier) and `tools[name]`, listed in `ALL_TOOLS` with `description`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredTool {
    pub name: String,
    pub js_name: Option<String>,
    pub description: String,
}

/// A global function; `a.b` names group into a frozen `a` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredGlobal {
    pub name: String,
    pub spread: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteOptions {
    /// The deadline for the whole script, tool calls included; `None` runs
    /// until it settles or is aborted (the coding agent's default).
    pub timeout: Option<Duration>,
    pub memory_limit_bytes: Option<usize>,
    /// What `load()` reads.
    pub store: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The script threw or failed to parse.
    Script,
    /// The deadline expired.
    Timeout,
    /// The caller aborted.
    Aborted,
    /// The VM or the bridge failed outside the script's control.
    Sandbox,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Script => "script",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Aborted => "aborted",
            ErrorKind::Sandbox => "sandbox",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodemodeError {
    pub kind: ErrorKind,
    pub name: Option<String>,
    pub message: String,
    pub stack: Option<String>,
}

impl CodemodeError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        CodemodeError {
            kind,
            name: None,
            message: message.into(),
            stack: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    Ok,
    Error,
    /// Still running when the execution ended.
    Cancelled,
}

/// A tool call the script made (globals are not recorded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub name: String,
    pub status: CallStatus,
    pub duration: Duration,
}

/// Keys the script changed with `store()`; only successful executions
/// report them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreWrites {
    pub set: BTreeMap<String, Value>,
    /// Keys stored as `undefined`.
    pub delete: Vec<String>,
}

/// The outcome. `output` is kept for failed executions too, up to the
/// failure; `exit()` completes with no value.
#[derive(Debug, Clone, PartialEq)]
pub struct CodemodeResult {
    pub outcome: Result<Completed, CodemodeError>,
    pub output: Vec<OutputItem>,
    pub calls: Vec<CallRecord>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completed {
    pub value: Option<Value>,
    /// The value's JSON as the script serialized it, keys in its order
    /// (a `Value` sorts them).
    pub value_json: Option<String>,
    pub store_writes: StoreWrites,
}

/// A caller's abort: set it, and the execution ends `aborted` with `reason`.
#[derive(Clone, Default)]
pub struct AbortSignal {
    inner: Arc<AbortInner>,
}

#[derive(Default)]
struct AbortInner {
    reason: std::sync::Mutex<Option<String>>,
    notify: tokio::sync::Notify,
}

impl AbortSignal {
    pub fn abort(&self, reason: impl Into<String>) {
        let mut slot = self
            .inner
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(reason.into());
        }
        drop(slot);
        self.inner.notify.notify_waiters();
    }

    pub fn reason(&self) -> Option<String> {
        self.inner
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn aborted(&self) -> String {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(reason) = self.reason() {
                return reason;
            }
            notified.await;
        }
    }
}

const IDENTIFIER_RESERVED: [&str; 9] = [
    "tools",
    "ALL_TOOLS",
    "console",
    "text",
    "image",
    "exit",
    "globalThis",
    "store",
    "load",
];

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Checks global names: identifiers or `namespace.member`, not shadowing a
/// built-in global or each other (upstream `checkGlobals`).
pub fn check_globals(globals: &[DeclaredGlobal]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    let mut namespaces = std::collections::HashSet::new();
    for global in globals {
        let parts: Vec<&str> = global.name.split('.').collect();
        if parts.len() > 2
            || !parts.iter().all(|part| is_identifier(part))
            || IDENTIFIER_RESERVED.contains(&parts[0])
        {
            return Err(format!("Invalid global name \"{}\"", global.name));
        }
        if !names.insert(global.name.clone()) {
            return Err(format!("Global \"{}\" is already registered", global.name));
        }
        if parts.len() == 2 {
            namespaces.insert(parts[0].to_owned());
        }
    }
    for name in namespaces {
        if names.contains(&name) {
            return Err(format!(
                "Global \"{name}\" conflicts with the namespace \"{name}\""
            ));
        }
    }
    Ok(())
}

/// What the VM sent that does not decode: the script may have patched a
/// built-in the prelude uses to serialize.
struct BridgeError(String);

fn parse_bridge_json(json: &str, what: &str) -> Result<Value, BridgeError> {
    serde_json::from_str(json).map_err(|_| BridgeError(format!("{what} is not valid JSON")))
}

fn parse_store_writes(json: &str) -> Result<StoreWrites, BridgeError> {
    let Value::Array(entries) = parse_bridge_json(json, "store writes")? else {
        return Err(BridgeError("store writes are not an array".to_owned()));
    };
    let mut writes = StoreWrites::default();
    for entry in entries {
        let malformed = || BridgeError("store writes contain a malformed entry".to_owned());
        let Value::Array(entry) = entry else {
            return Err(malformed());
        };
        let Some(Value::String(key)) = entry.first() else {
            return Err(malformed());
        };
        match entry.get(1) {
            None if entry.len() == 1 => writes.delete.push(key.clone()),
            Some(Value::String(value)) if entry.len() == 2 => {
                let parsed = parse_bridge_json(
                    value,
                    &format!("store value for {}", Value::String(key.clone())),
                )?;
                writes.set.insert(key.clone(), parsed);
            }
            _ => return Err(malformed()),
        }
    }
    Ok(writes)
}

fn parse_script_error(json: &str) -> Result<CodemodeError, BridgeError> {
    let Value::Object(fields) = parse_bridge_json(json, "script error")? else {
        return Err(BridgeError("script error is not an object".to_owned()));
    };
    let malformed = || BridgeError("script error is malformed".to_owned());
    let text = |key: &str| -> Result<Option<String>, BridgeError> {
        match fields.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(malformed()),
        }
    };
    let name = text("name")?;
    let stack = text("stack")?;
    let message = text("message")?.ok_or_else(malformed)?;
    Ok(CodemodeError {
        kind: ErrorKind::Script,
        name,
        message,
        stack,
    })
}

/// What the host tells the VM thread.
enum Command {
    Deliver(VmResult),
}

/// Runs `code` in a new VM with `tools` and `globals` answered by
/// `executor`. Never fails for script failures: they are the result's
/// error. `abort` ends it `aborted`.
pub async fn execute(
    code: &str,
    tools: &[DeclaredTool],
    globals: &[DeclaredGlobal],
    executor: &dyn Executor,
    options: ExecuteOptions,
    abort: Option<&AbortSignal>,
) -> CodemodeResult {
    let mut output = Vec::new();
    let mut calls: Vec<CallRecord> = Vec::new();
    let finish = |outcome, output, calls| CodemodeResult {
        outcome,
        output,
        calls,
    };
    if let Err(message) = check_globals(globals) {
        return finish(
            Err(CodemodeError::new(ErrorKind::Sandbox, message)),
            output,
            calls,
        );
    }
    if let Some(reason) = abort.and_then(AbortSignal::reason) {
        return finish(
            Err(CodemodeError::new(ErrorKind::Aborted, reason)),
            output,
            calls,
        );
    }
    let start = VmStart {
        code: code.to_owned(),
        tools: tools
            .iter()
            .map(|tool| VmTool {
                name: tool.name.clone(),
                js_name: tool
                    .js_name
                    .clone()
                    .unwrap_or_else(|| to_codemode_identifier(&tool.name)),
                description: tool.description.clone(),
            })
            .collect(),
        globals: globals
            .iter()
            .map(|global| VmGlobal {
                name: global.name.clone(),
                spread: global.spread,
            })
            .collect(),
        memory_limit_bytes: options.memory_limit_bytes,
        store: options
            .store
            .iter()
            .map(|(key, value)| (key.clone(), value.to_string()))
            .collect(),
    };
    let tool_names: std::collections::HashSet<&str> =
        tools.iter().map(|tool| tool.name.as_str()).collect();
    let global_names: std::collections::HashSet<&str> =
        globals.iter().map(|global| global.name.as_str()).collect();

    // The VM thread: starts the VM, then applies results until told to stop.
    let interrupt = Arc::new(AtomicBool::new(false));
    let (commands, command_rx) = mpsc::channel::<Command>();
    let (message_tx, mut messages) =
        async_mpsc::unbounded_channel::<Result<Vec<VmMessage>, String>>();
    {
        let interrupt = interrupt.clone();
        std::thread::Builder::new()
            .name("noel-codemode-vm".to_owned())
            .spawn(move || {
                let sink = {
                    let message_tx = message_tx.clone();
                    Box::new(move |message| {
                        let _ = message_tx.send(Ok(vec![message]));
                    })
                };
                let mut session = match VmSession::start_streaming(&start, interrupt, sink) {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = message_tx.send(Err(error));
                        return;
                    }
                };
                while let Ok(Command::Deliver(result)) = command_rx.recv() {
                    session.deliver(&result);
                    if session.ended() {
                        return;
                    }
                }
            })
            .expect("spawn the codemode VM thread");
    }
    // Stops the VM thread however this function ends: the flag stops a
    // spinning script, and the closed channel ends the loop.
    struct Stop(Arc<AtomicBool>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _stop = Stop(interrupt);

    let deadline = options
        .timeout
        .map(|timeout| (timeout, tokio::time::sleep(timeout)));
    let (timeout, sleep) = match deadline {
        Some((timeout, sleep)) => (Some(timeout), Some(sleep)),
        None => (None, None),
    };
    tokio::pin!(sleep);
    let aborted = async {
        match abort {
            Some(abort) => abort.aborted().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(aborted);

    // Running calls: (id, index of the record, started, future).
    let mut running = FuturesUnordered::new();
    let mut pending: HashMap<u64, (Option<usize>, Instant)> = HashMap::new();
    let outcome: Result<Completed, CodemodeError> = 'run: loop {
        tokio::select! {
            batch = messages.recv() => {
                let batch = match batch {
                    Some(Ok(batch)) => batch,
                    Some(Err(error)) => break 'run Err(CodemodeError::new(ErrorKind::Sandbox, error)),
                    None => break 'run Err(CodemodeError::new(ErrorKind::Sandbox, "The VM stopped")),
                };
                for message in batch {
                    match message {
                        VmMessage::Output(item) => output.push(item),
                        VmMessage::Call { id, target, name, args } => {
                            if pending.contains_key(&id) {
                                break 'run Err(bridge_broken(&format!("duplicate call id {id}")));
                            }
                            let record = (target == CallTarget::Tool).then(|| {
                                calls.push(CallRecord { name: name.clone(), status: CallStatus::Cancelled, duration: Duration::ZERO });
                                calls.len() - 1
                            });
                            pending.insert(id, (record, Instant::now()));
                            let known = match target {
                                CallTarget::Tool => tool_names.contains(name.as_str()),
                                CallTarget::Global => global_names.contains(name.as_str()),
                            };
                            let args = match args.as_deref().map(serde_json::from_str::<Value>).transpose() {
                                Ok(args) => args,
                                Err(_) => break 'run Err(bridge_broken("call arguments is not valid JSON")),
                            };
                            running.push(async move {
                                let outcome = if known {
                                    executor.call(target, &name, args).await
                                } else {
                                    let what = if target == CallTarget::Tool { "tool" } else { "global" };
                                    Err(format!("Unknown {what} \"{name}\""))
                                };
                                (id, outcome)
                            });
                        }
                        VmMessage::Done { value, writes } => {
                            let value_json = value.clone();
                            let value = match value.as_deref().map(|value| parse_bridge_json(value, "return value")).transpose() {
                                Ok(value) => value,
                                Err(BridgeError(message)) => break 'run Err(bridge_broken(&message)),
                            };
                            match parse_store_writes(&writes) {
                                Ok(store_writes) => break 'run Ok(Completed { value, value_json, store_writes }),
                                Err(BridgeError(message)) => break 'run Err(bridge_broken(&message)),
                            }
                        }
                        VmMessage::Failed { error } => match parse_script_error(&error) {
                            Ok(error) => break 'run Err(error),
                            Err(BridgeError(message)) => break 'run Err(bridge_broken(&message)),
                        },
                        VmMessage::Crash { message } => {
                            break 'run Err(CodemodeError::new(ErrorKind::Sandbox, message))
                        }
                    }
                }
            }
            Some((id, result)) = running.next(), if !running.is_empty() => {
                let Some((record, started)) = pending.remove(&id) else { continue };
                if let Some(index) = record {
                    calls[index].status = if result.is_ok() { CallStatus::Ok } else { CallStatus::Error };
                    calls[index].duration = started.elapsed();
                }
                let reply = match result {
                    Ok(value) => VmResult { id, ok: true, payload: value.map(|value| value.to_string()), properties: None },
                    Err(message) => VmResult { id, ok: false, payload: Some(message), properties: None },
                };
                if commands.send(Command::Deliver(reply)).is_err() {
                    break 'run Err(CodemodeError::new(ErrorKind::Sandbox, "The VM stopped"));
                }
            }
            () = async { sleep.as_mut().as_pin_mut().expect("a deadline").await }, if sleep.is_some() => {
                let ms = timeout.map_or(0, |timeout| timeout.as_millis());
                break 'run Err(CodemodeError::new(ErrorKind::Timeout, format!("Execution timed out after {ms} ms")));
            }
            reason = &mut aborted => {
                break 'run Err(CodemodeError::new(ErrorKind::Aborted, reason));
            }
        }
    };
    // Calls still running are cancelled: their records keep `cancelled`.
    for (record, started) in pending.into_values() {
        if let Some(index) = record {
            calls[index].duration = started.elapsed();
        }
    }
    drop(running);
    finish(outcome, output, calls)
}

fn bridge_broken(message: &str) -> CodemodeError {
    CodemodeError::new(
        ErrorKind::Sandbox,
        format!(
            "Sandbox bridge broken: {message}. The script may have modified built-ins such as a prototype's toJSON."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    /// `add` sums, `slow` never answers (and aborts the run when it has an
    /// abort to trigger), `fail` fails; globals echo. Counts the calls in
    /// flight at once.
    #[derive(Default)]
    struct Fake {
        in_flight: AtomicUsize,
        most: AtomicUsize,
        abort: Option<AbortSignal>,
    }

    impl Executor for Fake {
        fn call<'a>(
            &'a self,
            target: CallTarget,
            name: &'a str,
            args: Option<Value>,
        ) -> CallFuture<'a> {
            Box::pin(async move {
                let args = args.unwrap_or(Value::Null);
                match (target, name) {
                    (CallTarget::Tool, "add") => {
                        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        self.most.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        self.in_flight.fetch_sub(1, Ordering::SeqCst);
                        let sum = args["x"].as_i64().unwrap_or(0) + args["y"].as_i64().unwrap_or(0);
                        Ok(Some(json!(sum)))
                    }
                    (CallTarget::Tool, "slow") => {
                        if let Some(abort) = &self.abort {
                            abort.abort("stopped by the user");
                        }
                        std::future::pending().await
                    }
                    (CallTarget::Tool, _) => Err(format!("{name} broke")),
                    (CallTarget::Global, _) => Ok(Some(json!({"global": name, "args": args}))),
                }
            })
        }
    }

    fn tools() -> Vec<DeclaredTool> {
        ["add", "slow", "fail"]
            .iter()
            .map(|name| DeclaredTool {
                name: (*name).to_owned(),
                js_name: None,
                description: String::new(),
            })
            .collect()
    }

    async fn run_on(
        fake: &Fake,
        code: &str,
        options: ExecuteOptions,
        abort: Option<&AbortSignal>,
    ) -> CodemodeResult {
        let globals = [
            DeclaredGlobal {
                name: "lookup".to_owned(),
                spread: false,
            },
            DeclaredGlobal {
                name: "models.pick".to_owned(),
                spread: true,
            },
        ];
        execute(code, &tools(), &globals, fake, options, abort).await
    }

    async fn run(code: &str, options: ExecuteOptions) -> CodemodeResult {
        run_on(&Fake::default(), code, options, None).await
    }

    #[tokio::test]
    async fn calls_run_in_parallel_and_are_recorded() {
        let fake = Fake::default();
        let result = run_on(
            &fake,
            "const sums = await Promise.all([1, 2, 3, 4].map((x) => tools.add({ x, y: 10 })));\n\
             let error; try { await tools.fail(); } catch (e) { error = e.message; }\n\
             text('done');\n\
             return { sums, error, g: await lookup(1), m: await models.pick(1, 2) };",
            ExecuteOptions::default(),
            None,
        )
        .await;
        let completed = result.outcome.expect("completed");
        assert_eq!(
            completed.value,
            Some(json!({
                "sums": [11, 12, 13, 14],
                "error": "fail broke",
                "g": {"global": "lookup", "args": 1},
                "m": {"global": "models.pick", "args": [1, 2]},
            }))
        );
        // The four calls were in flight together.
        assert_eq!(fake.most.load(Ordering::SeqCst), 4);
        let statuses: Vec<_> = result
            .calls
            .iter()
            .map(|call| (call.name.as_str(), call.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("add", CallStatus::Ok),
                ("add", CallStatus::Ok),
                ("add", CallStatus::Ok),
                ("add", CallStatus::Ok),
                ("fail", CallStatus::Error),
            ]
        );
        assert_eq!(
            result.output,
            [OutputItem::Text {
                text: "done".to_owned(),
                console: false
            }]
        );
    }

    #[tokio::test]
    async fn the_deadline_stops_a_spinning_script_and_keeps_its_output() {
        let options = ExecuteOptions {
            // Longer than a debug build takes to start the VM.
            timeout: Some(Duration::from_millis(1500)),
            ..ExecuteOptions::default()
        };
        let result = run("text('start'); while (true) {}", options).await;
        let error = result.outcome.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Timeout);
        assert_eq!(error.message, "Execution timed out after 1500 ms");
        assert_eq!(result.output.len(), 1);
    }

    #[tokio::test]
    async fn an_abort_ends_the_run_and_cancels_pending_calls() {
        let abort = AbortSignal::default();
        let fake = Fake {
            abort: Some(abort.clone()),
            ..Fake::default()
        };
        let result = run_on(
            &fake,
            "await tools.slow({}); return 1;",
            ExecuteOptions::default(),
            Some(&abort),
        )
        .await;
        let error = result.outcome.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Aborted);
        assert_eq!(error.message, "stopped by the user");
        assert_eq!(result.calls.len(), 1);
        assert_eq!(result.calls[0].status, CallStatus::Cancelled);
    }

    #[tokio::test]
    async fn the_store_reads_its_values_and_reports_writes() {
        let mut store = BTreeMap::new();
        store.insert("count".to_owned(), json!(2));
        store.insert("old".to_owned(), json!("x"));
        let options = ExecuteOptions {
            store,
            ..ExecuteOptions::default()
        };
        let result = run(
            "store('count', load('count') + 1); store('old', undefined); store('list', [1]); return load('count');",
            options,
        )
        .await;
        let completed = result.outcome.expect("completed");
        assert_eq!(completed.value, Some(json!(3)));
        assert_eq!(completed.store_writes.set.get("count"), Some(&json!(3)));
        assert_eq!(completed.store_writes.set.get("list"), Some(&json!([1])));
        assert_eq!(completed.store_writes.delete, ["old"]);
    }

    #[tokio::test]
    async fn script_errors_and_memory_limits_are_script_failures() {
        let thrown = run("throw new RangeError('nope')", ExecuteOptions::default()).await;
        let error = thrown.outcome.unwrap_err();
        assert_eq!(
            (error.kind, error.name.as_deref(), error.message.as_str()),
            (ErrorKind::Script, Some("RangeError"), "nope")
        );

        let options = ExecuteOptions {
            memory_limit_bytes: Some(4 << 20),
            ..ExecuteOptions::default()
        };
        let hungry = run(
            "const a = []; while (true) a.push('x'.repeat(4096) + a.length);",
            options,
        )
        .await;
        let error = hungry.outcome.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Script, "{error:?}");
        assert!(error.message.contains("memory"), "{error:?}");
    }

    #[test]
    fn global_names_are_checked() {
        let global = |name: &str| DeclaredGlobal {
            name: name.to_owned(),
            spread: false,
        };
        assert!(check_globals(&[global("a"), global("b.c")]).is_ok());
        assert_eq!(
            check_globals(&[global("tools")]),
            Err("Invalid global name \"tools\"".to_owned())
        );
        assert_eq!(
            check_globals(&[global("a.b.c")]),
            Err("Invalid global name \"a.b.c\"".to_owned())
        );
        assert_eq!(
            check_globals(&[global("a"), global("a")]),
            Err("Global \"a\" is already registered".to_owned())
        );
        assert_eq!(
            check_globals(&[global("a"), global("a.b")]),
            Err("Global \"a\" conflicts with the namespace \"a\"".to_owned())
        );
    }
}

//! A watcher running in the daemon (upstream `packages/env/src/watch.ts`): Durable's `NodeFileWatcher` next to the
//! files. When the connection is lost, the watch is opened again once the connection can be started again, and
//! `Overflow` is reported: changes in between were not seen.
//!
//! Ported from upstream pi's `packages/env/src/watch.ts` (<https://github.com/earendil-works/pi>, commit `1692bba`),
//! Copyright (c) 2025 Mario Zechner, MIT License (see LICENSE at the repository root).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;

use crate::connection::{lost, Connection, Json, RemoteError, Reply, RequestOptions};
use crate::remote::remote_file_error;
use crate::{
    AbortController, Context, FileError, FileErrorCode, FileWatcher, WatchCallback, WatchChange,
    WatchMode, WatchTarget,
};

const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How the daemon watches, like `NodeExecutionEnv`'s watch options (upstream `RemoteWatchOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteWatchOptions {
    /// Force a mode; by default the daemon polls only where native events are unreliable.
    pub mode: Option<WatchMode>,
    /// Interval between snapshots in polling mode; the daemon's default is 2000 ms.
    pub poll_interval: Option<Duration>,
    /// Most directories one watcher covers; the daemon's default is 10 000.
    pub max_directories: Option<u64>,
}

struct Shared {
    on_change: WatchCallback,
    closed: AtomicBool,
    mode: Mutex<WatchMode>,
    /// Aborted by `close`: cancels the running watch request.
    stop: AbortController,
    /// Wakes a reconnect that waits.
    wake: Notify,
}

impl Shared {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn set_mode(&self, mode: WatchMode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    fn deliver(&self, change: WatchChange) {
        if self.is_closed() {
            return;
        }
        // A callback that panics must not stop watching.
        let _ = catch_unwind(AssertUnwindSafe(|| (self.on_change)(change)));
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        self.stop.abort();
    }
}

pub(crate) struct RemoteWatcher {
    shared: Arc<Shared>,
    task: Option<JoinHandle<()>>,
}

impl RemoteWatcher {
    /// Watches `targets` (resolved paths); fails as `NodeFileWatcher.open` does if coverage cannot be established.
    pub(crate) async fn open(
        connection: Connection,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        options: &RemoteWatchOptions,
    ) -> Result<RemoteWatcher, RemoteError> {
        let targets: Vec<Value> = targets
            .iter()
            .map(|target| {
                let exclude = target.exclude.clone().unwrap_or_default();
                json!({
                    "path": target.path,
                    "recursive": target.recursive,
                    "exclude": { "hidden": exclude.hidden, "names": exclude.names },
                })
            })
            .collect();
        let mut request = Json::new();
        request.insert("targets".into(), Value::from(targets));
        if let Some(mode) = options.mode {
            let mode = match mode {
                WatchMode::Native => "native",
                WatchMode::Polling => "polling",
            };
            request.insert("mode".into(), Value::from(mode));
        }
        if let Some(interval) = options.poll_interval {
            request.insert(
                "pollIntervalMs".into(),
                Value::from(interval.as_millis() as u64),
            );
        }
        if let Some(max_directories) = options.max_directories {
            request.insert("maxDirectories".into(), Value::from(max_directories));
        }
        let shared = Arc::new(Shared {
            on_change,
            closed: AtomicBool::new(false),
            mode: Mutex::new(WatchMode::Native),
            stop: AbortController::new(),
            wake: Notify::new(),
        });
        let (ready, opened) = oneshot::channel();
        let task = tokio::spawn(run(connection, request, shared.clone(), ready));
        match opened.await {
            Ok(Ok(())) => Ok(RemoteWatcher {
                shared,
                task: Some(task),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(lost("pi-env connection lost")),
        }
    }
}

/// Runs the watch request to its end. Its `ready` event sends `Ok` on `opened` or, after a reconnect, reports an
/// overflow.
async fn watch_once(
    connection: &Connection,
    request: &Json,
    shared: &Shared,
    mut opened: Option<oneshot::Sender<Result<(), RemoteError>>>,
) -> (bool, Result<Reply, RemoteError>) {
    let reconnecting = opened.is_none();
    let mut ready = false;
    let mut on_event = |event: &Json, _payload: &[u8]| -> bool {
        match event.get("kind").and_then(Value::as_str) {
            Some("ready") => {
                ready = true;
                shared.set_mode(if event.get("mode") == Some(&Value::from("polling")) {
                    WatchMode::Polling
                } else {
                    WatchMode::Native
                });
                match opened.take() {
                    Some(opened) => {
                        let _ = opened.send(Ok(()));
                    }
                    // Changes while disconnected were not seen.
                    None if reconnecting => shared.deliver(WatchChange::Overflow),
                    None => {}
                }
            }
            Some("change") => {
                if event.get("mode") == Some(&Value::from("polling")) {
                    shared.set_mode(WatchMode::Polling);
                }
                if event.get("overflow") == Some(&Value::Bool(true)) {
                    shared.deliver(WatchChange::Overflow);
                } else {
                    let paths = event
                        .get("paths")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|path| path.as_str().map(str::to_string))
                        .collect();
                    shared.deliver(WatchChange::Paths(paths));
                }
            }
            Some("error") => {
                let code = if event.get("code") == Some(&Value::from("permission_denied")) {
                    FileErrorCode::PermissionDenied
                } else {
                    FileErrorCode::Invalid
                };
                let message = match event.get("message") {
                    Some(Value::String(message)) => message.clone(),
                    Some(other) => other.to_string(),
                    None => "undefined".to_string(),
                };
                shared.deliver(WatchChange::Error(FileError::new(code, message, None)));
                shared.closed.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
        true
    };
    let cx = Context::with_abort_signal(shared.stop.signal());
    let result = connection
        .request(
            "watch",
            request.clone(),
            RequestOptions {
                cx: Some(&cx),
                on_event: Some(&mut on_event),
                cancel_on_drop: true,
                ..RequestOptions::default()
            },
        )
        .await;
    if let (Some(opened), Err(error)) = (opened.take(), &result) {
        let _ = opened.send(Err(error.clone()));
    }
    (ready, result)
}

fn file_error(error: RemoteError) -> FileError {
    remote_file_error(error, None)
}

async fn run(
    connection: Connection,
    request: Json,
    shared: Arc<Shared>,
    opened: oneshot::Sender<Result<(), RemoteError>>,
) {
    let (ready, mut result) = watch_once(&connection, &request, &shared, Some(opened)).await;
    if !ready {
        return;
    }
    loop {
        // The watch request ended.
        if shared.is_closed() {
            return;
        }
        let error = match result {
            Ok(_) => return,
            Err(error) if error.is_connection_lost() => error,
            Err(error) => {
                shared.deliver(WatchChange::Error(file_error(error)));
                shared.closed.store(true, Ordering::SeqCst);
                return;
            }
        };
        drop(error);
        // Open it again once a new daemon starts, waiting longer after each failed start.
        let mut delay = RECONNECT_FIRST;
        loop {
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shared.wake.notified() => {}
            }
            if shared.is_closed() {
                return;
            }
            let attempt = tokio::select! {
                attempt = watch_once(&connection, &request, &shared, None) => attempt,
                () = shared.wake.notified() => return,
            };
            let (ready, next) = attempt;
            if ready {
                result = next;
                break;
            }
            match next {
                Err(error) if error.is_connection_lost() => {
                    delay = (delay * 2).min(RECONNECT_MAX);
                }
                Err(error) => {
                    shared.deliver(WatchChange::Error(file_error(error)));
                    shared.closed.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(_) => return,
            }
        }
    }
}

#[async_trait]
impl FileWatcher for RemoteWatcher {
    fn mode(&self) -> WatchMode {
        *self
            .shared
            .mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    async fn close(&mut self, _cx: &Context) {
        self.shared.close();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        self.shared.close();
    }
}

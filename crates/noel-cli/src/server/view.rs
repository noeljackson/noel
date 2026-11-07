//! The view a worker publishes as `noel.chat`'s `view` (spec §7.3): Durable's
//! shape `{conversation, entries, docs, lastRun}` (`durable/docs/spec.md`
//! §9.3) with the documents noel has data for.
//!
//! - `conversation`: `{id, cwd, name?}`.
//! - `entries`: a window over the session's history
//!   ([`noel_core::history`]): Durable's entry records `{id, kind, model?,
//!   data?, head?, timestamp?}`, each message as upstream pi's `Message`
//!   JSON. A view starts with the newest 50 entries; new ones are appended,
//!   and past 100 the oldest leave in one splice down to 50. `before` names
//!   the entry just before the window; `noel.chat`'s `entries` method pages
//!   back from it.
//! - `docs["pi.agent"]`: `{model?, thinkingLevel, cwd, compaction?}`.
//! - `docs["pi.live"]`: a subset of Durable's `pi.live` v3. While a run is
//!   active, `run` (`{runId, requestId?, inputs, turns}`), `generation`
//!   (`{attempt, message?, retry?}`, where `message` is the reply's
//!   committed partial and `retry` is `{at, error, delayMs, maxAttempts}`,
//!   the last two noel's) and `tools` (the round's calls, `{callId, name,
//!   status, output?, truncated?, entry?}`, with their committed output and,
//!   once saved, their result's entry); `compactions` (`[{trigger}]`) while
//!   one runs and `compactionFailure` (`{trigger, error}`) after one failed;
//!   `runFailure` (`{reason, detail?, inputs}`) after a run that ended
//!   without an answer, until the next starts. Otherwise `{}`.
//! - `docs["pi.inbox"]`: `{items: [{id, mode, content, requestId?}]}`.
//! - `docs["pi.usage"]`: `{total: {input, output, requests}}`.
//! - `docs["noel.todos"]`: `{items}`, the session's todo list.
//! - `docs["noel.turn"]`: `{unfinished}`, whether the saved context ends
//!   mid-turn; a client opening the session continues it, as `noel` does.
//! - `docs["noel.notices"]`: `{items: [{id, text}]}`, the newest notes
//!   (an account failover's switch); a client shows each new one once.
//! - `docs["noel.clients"]`: `{count}`, the clients holding the session (a
//!   local `noel` that is the only one ends the session with itself).
//! - `lastRun`: `get_state.lastRun`.
//!
//! Only committed progress is shown: partial replies and tool output come
//! from the journal's own records as they are saved
//! ([`noel_core::SavedProgress`]), so a client sees what a crash would keep.
//! A subscription gets the current value, then ops: appends to a streaming
//! reply or a tool's output, a splice at the end of `entries` for a new
//! message, and small diffs of the documents.

use std::collections::BTreeMap;
use std::path::PathBuf;

use noel_chord::delta::{diff, Op, Seg};
use noel_chord::v2::object::Cell;
use noel_core::controller::{CallInput, Event, Outcome};
use noel_core::history::{History, HistoryRecord};
use noel_core::{
    interrupted_output, MessageRole, QueueItem, QueueMode, Runtime, SavedProgress, SessionState,
    TurnEvent,
};
use serde_json::{json, Map, Value};

use crate::rpc_wire::{LastRun, Message};

/// The view's schema, as `noel.chat` declares it.
pub(crate) fn schema() -> Value {
    super::chat_contract().declaration.variants["participant"].state["view"].clone()
}

/// How many notes `noel.notices` keeps.
const NOTICES_KEPT: usize = 20;

/// Entries a view starts with, and the most it holds before the oldest
/// leave (Durable's `ConversationWindow` defaults).
pub(crate) const WINDOW_MIN: usize = 50;
pub(crate) const WINDOW_MAX: usize = 2 * WINDOW_MIN;

/// The view of one session and what of it the cell holds.
pub(crate) struct View {
    cell: Cell,
    conversation: Value,
    history: History,
    /// The window's entries, as the cell holds them once published.
    entries: Vec<Value>,
    /// The entry just before the window.
    before: Option<u64>,
    published_before: Option<u64>,
    /// Entries that left the front of the window since the last publish.
    trimmed: usize,
    agent: Value,
    usage: Value,
    /// `docs["noel.todos"]`: the session's todo list, as the TUI's panel
    /// shows it.
    todos: Value,
    /// `docs["noel.turn"]`: `{unfinished}`, whether the saved context ends
    /// mid-turn (a client opening the session continues it).
    turn: Value,
    /// `docs["noel.notices"]`: `{items: [{id, text}]}`, the newest notes
    /// for clients to show once (an account failover).
    notices: Vec<Value>,
    notices_next: u64,
    /// `docs["noel.clients"]`: `{count}`, the clients holding the session.
    clients: usize,
    inbox: Value,
    live: Live,
    last_run: Option<Value>,
    /// Entries from this index on differ from what the cell holds.
    dirty_from: Option<usize>,
    /// What the cell holds, but for entries, of which it holds this many.
    published_entries: usize,
    published_conversation: Value,
    published_docs: BTreeMap<&'static str, Value>,
    published_last_run: Option<Value>,
    /// The calls an interrupted run owes results for, as it continues:
    /// `(call, replay-safe, interrupted result)`.
    resuming: Vec<(noel_ai::ChatToolCall, bool, String)>,
    empty: SessionState,
    /// Every batch applied to the cell, for tests to replay.
    #[cfg(test)]
    applied: Vec<Vec<Op>>,
}

#[derive(Default)]
struct Live {
    run: Option<RunDoc>,
    generation: Option<Generation>,
    tools: Vec<ToolSlot>,
    /// The running compaction's trigger (Durable's
    /// `CompactionStatus.trigger`).
    compaction: Option<CompactionTrigger>,
    /// The last compaction failed: its trigger and error.
    compaction_failure: Option<(CompactionTrigger, String)>,
    /// The reply ran out of context: the next compaction is an overflow's.
    overflowed: bool,
    /// Why the last run ended without an answer; kept until a run starts.
    run_failure: Option<Value>,
}

struct RunDoc {
    run_id: String,
    request_id: Option<String>,
    inputs: Vec<String>,
    /// The run's tool rounds so far.
    turns: u64,
}

#[derive(Default)]
struct Generation {
    attempt: u64,
    text: String,
    thinking: String,
    /// When the next attempt goes out (ms since the epoch), why, the
    /// backoff, and the most attempts.
    retry: Option<(u64, String, u64, u64)>,
}

/// What started a compaction (#145).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionTrigger {
    Manual,
    Threshold,
    Overflow,
}

impl CompactionTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// Where a tool call of the round is (#145): `pi.live`'s tool `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPhase {
    /// Asked for, not started.
    Pending,
    Running,
    /// Its result is saved: output belongs to the entry now.
    Done,
    /// Cut off by a stop and not run again.
    Interrupted,
}

impl ToolPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Interrupted => "interrupted",
        }
    }
}

struct ToolSlot {
    call_id: String,
    name: String,
    status: ToolPhase,
    output: String,
    truncated: bool,
    /// The history entry of the call's result, once saved.
    entry: Option<u64>,
}

impl View {
    /// A view of `session` with its saved history (from
    /// `SessionStore::read_history`); [`View::refresh`] publishes it.
    pub fn new(session: &SessionState, saved: &[HistoryRecord]) -> Self {
        let mut history = History::default();
        let empty = SessionState::new(String::new(), PathBuf::new());
        for record in saved {
            history.push(record, |message, failed| render(message, failed, &empty));
        }
        let (window, before) = history.window(None, WINDOW_MIN, WINDOW_MAX);
        let entries = window.iter().map(|entry| entry.to_json()).collect();
        let conversation = conversation(session);
        let initial = json!({"conversation": conversation, "entries": [], "docs": {}});
        let cell = Cell::new(schema(), initial).expect("the empty view fits its schema");
        Self {
            cell,
            published_conversation: conversation.clone(),
            conversation,
            history,
            entries,
            before,
            published_before: None,
            trimmed: 0,
            agent: json!({}),
            usage: json!({}),
            todos: json!({"items": session.todos}),
            turn: turn(session),
            notices: Vec::new(),
            notices_next: 1,
            clients: 0,
            inbox: json!({"items": []}),
            live: Live::default(),
            last_run: None,
            dirty_from: Some(0),
            published_entries: 0,
            published_docs: BTreeMap::new(),
            published_last_run: None,
            resuming: Vec::new(),
            empty,
            #[cfg(test)]
            applied: Vec::new(),
        }
    }

    pub fn cell(&self) -> &Cell {
        &self.cell
    }

    /// The whole view as the cell holds it.
    #[cfg(test)]
    pub fn value(&self) -> Value {
        (*self.cell.value()).clone()
    }

    /// Reads everything from the session, while no run is active (or as one
    /// starts): the transcript, the documents and the last run.
    pub fn refresh(&mut self, runtime: &Runtime, agent: Value, queue: &[QueueItem]) {
        let session = runtime.session();
        self.conversation = conversation(session);
        self.agent = agent;
        self.usage = usage(session);
        self.todos = json!({"items": session.todos});
        self.turn = turn(session);
        self.inbox = inbox(queue);
        self.last_run = LastRun::of(runtime).map(|last| to_json(&last));
        // A failed run keeps its error, which the journal saved with it.
        if let (Some(last), Some(run)) = (self.last_run.as_mut(), session.runs.last()) {
            if let Some(failure) = run.failure.as_ref().filter(|f| !f.detail.is_empty()) {
                last["error"] = json!(failure.detail);
            }
        }
        self.publish();
    }

    /// How many clients hold the session.
    pub fn set_clients(&mut self, count: usize) {
        if count != self.clients {
            self.clients = count;
            self.publish();
        }
    }

    /// Adds a note for clients to show (the newest few are kept).
    pub fn notice(&mut self, text: String) {
        self.notices
            .push(json!({"id": self.notices_next, "text": text}));
        self.notices_next += 1;
        let excess = self.notices.len().saturating_sub(NOTICES_KEPT);
        self.notices.drain(..excess);
        self.publish();
    }

    /// Sets `pi.agent` (after a model or thinking-level change).
    pub fn set_agent(&mut self, agent: Value) {
        self.agent = agent;
        self.publish();
    }

    /// What the session controller reports.
    pub fn event(&mut self, event: &Event<'_>) {
        let changed = match event {
            Event::RunStarted { run_id, runtime } => {
                self.run_started(run_id, runtime);
                true
            }
            Event::CallStarted { input } => self.call_started(*input),
            Event::Turn { event, .. } => self.turn(event),
            Event::CallReturned { session, queue, .. } => {
                self.usage = usage(session);
                self.todos = json!({"items": session.todos});
                self.turn = turn(session);
                self.inbox = inbox(&queue.items());
                true
            }
            Event::Placed { submissions, .. } => {
                if let Some(run) = &mut self.live.run {
                    let ids = submissions.iter().map(|id| id.submission_id.clone());
                    run.inputs.extend(ids);
                }
                true
            }
            Event::Withdrawn { .. } => false,
            Event::QueueChanged { queue } => {
                self.inbox = inbox(&queue.items());
                true
            }
            Event::CompactionStarted => {
                self.live.compaction = Some(CompactionTrigger::Manual);
                self.live.compaction_failure = None;
                true
            }
            Event::CompactionEnded { result } => {
                // A manual compaction's failure is shown as a threshold's
                // or an overflow's is (#145: it was dropped).
                if let (Some(trigger), Err(error)) = (self.live.compaction.take(), result) {
                    self.live.compaction_failure = Some((trigger, (*error).to_owned()));
                }
                true
            }
            Event::SaveFailed { .. } => false,
            Event::RunSettled { run_id, outcome } => {
                let run = self.live.run.take();
                let inputs = run
                    .as_ref()
                    .map(|run| run.inputs.clone())
                    .unwrap_or_default();
                let request_id = run.and_then(|run| run.request_id);
                self.live = Live::default();
                // Why the run ended without an answer (Durable's
                // `runFailure`), until another run starts.
                self.live.run_failure = match outcome {
                    Outcome::Completed => None,
                    Outcome::Aborted => Some(json!({"reason": "aborted", "inputs": inputs})),
                    failed => failed.failure().map(|failure| {
                        let mut value =
                            json!({"reason": failure.reason.as_str(), "inputs": inputs});
                        if !failure.detail.is_empty() {
                            value["detail"] = json!(failure.detail);
                        }
                        value
                    }),
                };
                self.resuming.clear();
                let mut last = json!({"runId": run_id, "status": to_json(&outcome.status())});
                if let Some(request_id) = request_id {
                    last["requestId"] = json!(request_id);
                }
                if let Some(error) = outcome.error() {
                    last["error"] = json!(error);
                }
                self.last_run = Some(last);
                true
            }
        };
        if changed {
            self.publish();
        }
    }

    /// Progress the run committed to the journal.
    pub fn saved(&mut self, progress: &SavedProgress) {
        match progress {
            SavedProgress::Partial {
                attempt,
                text,
                thinking,
                append,
                ..
            } => {
                let generation = self.live.generation.get_or_insert_with(Generation::default);
                if *append && generation.attempt == *attempt {
                    generation.text.push_str(text);
                    generation.thinking.push_str(thinking);
                } else {
                    generation.attempt = *attempt;
                    generation.text.clone_from(text);
                    generation.thinking.clone_from(thinking);
                }
            }
            SavedProgress::ToolOutput {
                call_id,
                output,
                truncated,
                append,
                ..
            } => {
                // Progress for a call already done is late: its output is
                // the saved result's now (#145).
                let Some(slot) = self.slot(call_id, None) else {
                    return;
                };
                if *append {
                    slot.output.push_str(output);
                } else {
                    slot.output.clone_from(output);
                }
                slot.truncated = *truncated;
            }
            SavedProgress::History(record) => {
                let empty = &self.empty;
                let added = self
                    .history
                    .push(record, |message, failed| render(message, failed, empty))
                    .map(|entry| entry.to_json());
                if let Some(entry) = added {
                    // A call's result names its entry in the call's slot.
                    let call_id = entry["model"][0]["toolCallId"].as_str().map(str::to_owned);
                    let id = entry["id"].as_u64();
                    if let Some(slot) = call_id.and_then(|call_id| {
                        self.live
                            .tools
                            .iter_mut()
                            .find(|slot| slot.call_id == call_id)
                    }) {
                        slot.entry = id;
                    }
                    self.push_entry(entry);
                }
            }
        }
        self.publish();
    }

    /// Up to `limit` entries just before the entry `before`, oldest first,
    /// and the entry before them: `noel.chat`'s `entries`.
    pub fn page(&self, before: u64, limit: usize) -> Value {
        let (entries, earlier) = self.history.page(before, limit);
        let mut page =
            json!({"entries": entries.iter().map(|entry| entry.to_json()).collect::<Vec<_>>()});
        if let Some(earlier) = earlier {
            page["before"] = json!(earlier);
        }
        page
    }

    fn run_started(&mut self, run_id: &str, runtime: &Runtime) {
        let session = runtime.session();
        self.conversation = conversation(session);
        self.usage = usage(session);
        let request_id = session
            .runs
            .iter()
            .rev()
            .find(|run| run.id == run_id)
            .and_then(|run| run.request_id.clone());
        self.live = Live {
            run: Some(RunDoc {
                run_id: run_id.to_owned(),
                request_id,
                inputs: vec![run_id.to_owned()],
                turns: 0,
            }),
            ..Live::default()
        };
        let mut last = json!({"runId": run_id, "status": "running"});
        if let Some(request_id) = self
            .live
            .run
            .as_ref()
            .and_then(|run| run.request_id.clone())
        {
            last["requestId"] = json!(request_id);
        }
        self.last_run = Some(last);
        // An interrupted run owes results for the calls its last reply
        // made: replay-safe ones run again, the others get their saved
        // output as an interrupted result (spec §4.1, §4.2).
        self.resuming = session
            .pending_tool_calls()
            .map(|(_, calls)| {
                calls
                    .into_iter()
                    .map(|call| {
                        let saved = session.tool_outputs.get(&call.id);
                        let result = interrupted_output(&call.name, saved);
                        (call.clone(), runtime.replay_safe(&call.name), result)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (call, replay_safe, _) in &self.resuming {
            let saved = session.tool_outputs.get(&call.id);
            self.live.tools.push(ToolSlot {
                call_id: call.id.clone(),
                name: call.name.clone(),
                status: if *replay_safe {
                    ToolPhase::Pending
                } else {
                    ToolPhase::Interrupted
                },
                output: saved.map(|saved| saved.output.clone()).unwrap_or_default(),
                truncated: saved.is_some_and(|saved| saved.truncated),
                entry: None,
            });
        }
    }

    fn call_started(&mut self, input: CallInput) -> bool {
        match input {
            CallInput::Prompt => false,
            CallInput::Resume => {
                // The calls that do not run again get their interrupted
                // result first thing (saved, it is an entry).
                let resuming = std::mem::take(&mut self.resuming);
                for (call, replay_safe, _) in resuming {
                    if !replay_safe {
                        self.live.tools.retain(|slot| slot.call_id != call.id);
                    }
                }
                true
            }
            CallInput::FollowUps => false,
        }
    }

    fn turn(&mut self, event: &TurnEvent) -> bool {
        match event {
            TurnEvent::RoundStarted => {
                self.live.generation = Some(Generation {
                    attempt: 1,
                    ..Generation::default()
                });
                self.live
                    .tools
                    .retain(|slot| slot.status == ToolPhase::Pending);
                true
            }
            TurnEvent::RoundResumed(_) => false,
            // Saved messages are entries through the history.
            TurnEvent::MessageAdded(message) => {
                if message.role == MessageRole::Assistant {
                    // The reply supersedes its partial; its calls are next.
                    if let Some(generation) = &mut self.live.generation {
                        generation.text.clear();
                        generation.thinking.clear();
                    }
                    self.live.tools = message
                        .tool_calls
                        .iter()
                        .map(|call| ToolSlot {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            status: ToolPhase::Pending,
                            output: String::new(),
                            truncated: false,
                            entry: None,
                        })
                        .collect();
                }
                true
            }
            TurnEvent::Provider(_)
            | TurnEvent::ToolUpdate { .. }
            | TurnEvent::NestedToolCalls { .. }
            | TurnEvent::TodosUpdated(_) => false,
            TurnEvent::RetryStarted(notice) => {
                self.live.generation = Some(Generation {
                    attempt: notice.attempt + 1,
                    retry: Some((
                        super::now_ms() + notice.delay_ms,
                        notice.error_message.clone(),
                        notice.delay_ms,
                        notice.max_attempts,
                    )),
                    ..Generation::default()
                });
                true
            }
            TurnEvent::RetryFinished { .. } => {
                if let Some(generation) = &mut self.live.generation {
                    generation.retry = None;
                }
                true
            }
            TurnEvent::Overflowed { .. } => {
                self.live.overflowed = true;
                if let Some(generation) = &mut self.live.generation {
                    generation.text.clear();
                    generation.thinking.clear();
                }
                true
            }
            TurnEvent::ToolStarted { call } => {
                if let Some(slot) = self.slot(&call.id, Some(&call.name)) {
                    slot.status = ToolPhase::Running;
                }
                true
            }
            TurnEvent::ToolFinished { .. } => false,
            TurnEvent::ToolOutput { call, .. } => {
                if let Some(slot) = self.slot(&call.id, Some(&call.name)) {
                    slot.status = ToolPhase::Done;
                    slot.output.clear();
                    slot.truncated = false;
                }
                true
            }
            TurnEvent::CompactionStarted => {
                let trigger = if std::mem::take(&mut self.live.overflowed) {
                    CompactionTrigger::Overflow
                } else {
                    CompactionTrigger::Threshold
                };
                self.live.compaction = Some(trigger);
                self.live.compaction_failure = None;
                true
            }
            TurnEvent::CompactionFinished(_) => {
                self.live.compaction = None;
                true
            }
            TurnEvent::CompactionFailed(error) => {
                if let Some(trigger) = self.live.compaction.take() {
                    self.live.compaction_failure = Some((trigger, error.clone()));
                }
                true
            }
            TurnEvent::RoundFinished => {
                if !self.live.tools.is_empty() {
                    if let Some(run) = &mut self.live.run {
                        run.turns += 1;
                    }
                }
                self.live.generation = None;
                self.live.tools.clear();
                true
            }
        }
    }

    /// The round's slot for call `call_id`, made for a call the round did
    /// not list (it is running, then), or `None` once the call is done:
    /// what arrives for it after its result is late.
    fn slot(&mut self, call_id: &str, name: Option<&str>) -> Option<&mut ToolSlot> {
        let index = match self
            .live
            .tools
            .iter()
            .position(|slot| slot.call_id == call_id)
        {
            Some(index) => index,
            None => {
                self.live.tools.push(ToolSlot {
                    call_id: call_id.to_owned(),
                    name: String::new(),
                    status: ToolPhase::Running,
                    output: String::new(),
                    truncated: false,
                    entry: None,
                });
                self.live.tools.len() - 1
            }
        };
        let slot = &mut self.live.tools[index];
        if slot.status == ToolPhase::Done {
            return None;
        }
        if let Some(name) = name {
            name.clone_into(&mut slot.name);
        }
        Some(slot)
    }

    /// Appends a new entry to the window; past [`WINDOW_MAX`] the oldest
    /// leave, down to [`WINDOW_MIN`].
    fn push_entry(&mut self, entry: Value) {
        let index = self.entries.len();
        self.dirty_from = Some(self.dirty_from.map_or(index, |from| from.min(index)));
        self.entries.push(entry);
        if self.entries.len() > WINDOW_MAX {
            let leave = self.entries.len() - WINDOW_MIN;
            self.entries.drain(..leave);
            self.before = self.entries[0]["id"]
                .as_u64()
                .map(|id| id - 1)
                .filter(|id| *id > 0);
            // What the cell holds of the leaving entries goes in one splice;
            // the rest of the pending change moves with the window.
            let published_leaving = leave.min(self.published_entries);
            self.trimmed += published_leaving;
            self.published_entries -= published_leaving;
            self.dirty_from = self.dirty_from.map(|from| from.saturating_sub(leave));
        }
    }

    fn live_json(&self) -> Value {
        let mut live = Map::new();
        if let Some(run) = &self.live.run {
            let mut doc = json!({"runId": run.run_id, "inputs": run.inputs, "turns": run.turns});
            if let Some(request_id) = &run.request_id {
                doc["requestId"] = json!(request_id);
            }
            live.insert("run".into(), doc);
        }
        if let Some(generation) = &self.live.generation {
            let mut doc = json!({"attempt": generation.attempt});
            let mut content = Vec::new();
            if !generation.thinking.is_empty() {
                content.push(json!({"type": "thinking", "thinking": generation.thinking}));
            }
            if !generation.text.is_empty() {
                content.push(json!({"type": "text", "text": generation.text}));
            }
            if !content.is_empty() {
                doc["message"] = json!({"role": "assistant", "content": content});
            }
            if let Some((at, error, delay, most)) = &generation.retry {
                doc["retry"] = json!({
                    "at": at,
                    "error": error,
                    "delayMs": delay,
                    "maxAttempts": most,
                });
            }
            live.insert("generation".into(), doc);
        }
        if !self.live.tools.is_empty() {
            let tools = self
                .live
                .tools
                .iter()
                .map(|slot| {
                    let mut doc =
                        json!({"callId": slot.call_id, "name": slot.name, "status": slot.status.as_str()});
                    if !slot.output.is_empty() {
                        doc["output"] = json!(slot.output);
                    }
                    if slot.truncated {
                        doc["truncated"] = json!(true);
                    }
                    if let Some(entry) = slot.entry {
                        doc["entry"] = json!(entry);
                    }
                    doc
                })
                .collect::<Vec<_>>();
            live.insert("tools".into(), Value::Array(tools));
        }
        if let Some(trigger) = self.live.compaction {
            live.insert("compactions".into(), json!([{"trigger": trigger.as_str()}]));
        }
        if let Some((trigger, error)) = &self.live.compaction_failure {
            live.insert(
                "compactionFailure".into(),
                json!({"trigger": trigger.as_str(), "error": error}),
            );
        }
        if let Some(failure) = &self.live.run_failure {
            live.insert("runFailure".into(), failure.clone());
        }
        Value::Object(live)
    }

    /// Applies what changed since the last publish to the cell, as ops.
    fn publish(&mut self) {
        let mut ops = Vec::new();
        if self.conversation != self.published_conversation {
            ops.push(Op::Set(
                vec![key("conversation")],
                self.conversation.clone(),
            ));
            self.published_conversation = self.conversation.clone();
        }
        if self.trimmed > 0 {
            ops.push(Op::Splice(
                vec![key("entries")],
                0,
                std::mem::take(&mut self.trimmed) as u64,
                Vec::new(),
            ));
        }
        if self.before != self.published_before {
            match self.before {
                Some(before) => ops.push(Op::Set(vec![key("before")], json!(before))),
                None => ops.push(Op::Delete(vec![key("before")])),
            }
            self.published_before = self.before;
        }
        if let Some(from) = self.dirty_from.take() {
            let from = from.min(self.published_entries);
            let removed = self.published_entries - from;
            let added = self.entries[from.min(self.entries.len())..].to_vec();
            if removed > 0 || !added.is_empty() {
                ops.push(Op::Splice(
                    vec![key("entries")],
                    from as u64,
                    removed as u64,
                    added,
                ));
            }
            self.published_entries = self.entries.len();
        }
        let docs: [(&'static str, Value); 8] = [
            ("pi.agent", self.agent.clone()),
            ("pi.live", self.live_json()),
            ("pi.inbox", self.inbox.clone()),
            ("pi.usage", self.usage.clone()),
            ("noel.todos", self.todos.clone()),
            ("noel.turn", self.turn.clone()),
            ("noel.notices", json!({"items": self.notices})),
            ("noel.clients", json!({"count": self.clients})),
        ];
        for (name, doc) in docs {
            let path = vec![key("docs"), key(name)];
            match self.published_docs.get(name) {
                None => ops.push(Op::Set(path, doc.clone())),
                Some(old) if *old != doc => ops.extend(prefixed(diff(old, &doc), &path)),
                Some(_) => continue,
            }
            self.published_docs.insert(name, doc);
        }
        if self.last_run != self.published_last_run {
            match &self.last_run {
                Some(last) => ops.push(Op::Set(vec![key("lastRun")], last.clone())),
                None => ops.push(Op::Delete(vec![key("lastRun")])),
            }
            self.published_last_run = self.last_run.clone();
        }
        if ops.is_empty() {
            return;
        }
        #[cfg(test)]
        self.applied.push(ops.clone());
        if let Err(error) = self.cell.apply(ops) {
            // The ops were made from what the cell holds; should they not
            // apply, the whole value replaces it.
            eprintln!("noel worker: view ops failed ({error}); replacing the view");
            let _ = self.cell.replace(self.full());
        }
    }

    fn full(&self) -> Value {
        let mut view = json!({
            "conversation": self.conversation,
            "entries": self.entries,
            "docs": self.published_docs.iter().map(|(name, doc)| ((*name).to_owned(), doc.clone())).collect::<Map<_, _>>(),
        });
        if let Some(last) = &self.last_run {
            view["lastRun"] = last.clone();
        }
        if let Some(before) = self.published_before {
            view["before"] = json!(before);
        }
        view
    }
}

/// A saved message as clients see it: upstream pi's `Message` JSON.
fn render(message: &noel_core::ConversationMessage, failed: bool, empty: &SessionState) -> Value {
    let mut json = to_json(&Message::saved(message, empty));
    if message.role == MessageRole::Tool {
        json["isError"] = json!(failed);
    }
    json
}

fn key(name: &str) -> Seg {
    Seg::Key(name.to_owned())
}

/// `ops` made for the value at `prefix`, for the whole view.
fn prefixed(ops: Vec<Op>, prefix: &[Seg]) -> Vec<Op> {
    let at = |path: Vec<Seg>| {
        let mut full = prefix.to_vec();
        full.extend(path);
        full
    };
    ops.into_iter()
        .map(|op| match op {
            Op::Replace(value) => Op::Set(prefix.to_vec(), value),
            Op::Set(path, value) => Op::Set(at(path), value),
            Op::Delete(path) => Op::Delete(at(path)),
            Op::Append(path, text) => Op::Append(at(path), text),
            Op::Trim(path, count) => Op::Trim(at(path), count),
            Op::Splice(path, index, remove, items) => Op::Splice(at(path), index, remove, items),
            Op::Permute(path, order) => Op::Permute(at(path), order),
        })
        .collect()
}

fn to_json(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("view values serialize")
}

fn conversation(session: &SessionState) -> Value {
    let mut doc = json!({"id": session.session_id, "cwd": session.cwd.display().to_string()});
    if let Some(name) = &session.name {
        doc["name"] = json!(name);
    }
    doc
}

fn turn(session: &SessionState) -> Value {
    json!({"unfinished": session.has_unfinished_turn()})
}

fn usage(session: &SessionState) -> Value {
    let usage = &session.usage;
    json!({"total": {
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "requests": usage.requests,
    }})
}

fn inbox(items: &[QueueItem]) -> Value {
    let items = items
        .iter()
        .map(|item| {
            let mode = match item.id.mode {
                QueueMode::Steer => "steer",
                QueueMode::FollowUp => "followUp",
            };
            let mut doc =
                json!({"id": item.id.submission_id, "mode": mode, "content": item.content});
            if let Some(request_id) = &item.id.request_id {
                doc["requestId"] = json!(request_id);
            }
            doc
        })
        .collect::<Vec<_>>();
    json!({ "items": items })
}

/// `pi.agent`: the session's model, its thinking level in effect, its
/// working directory, and its own compaction settings when it has them.
pub(crate) fn agent(runtime: &Runtime, config: &noel_config::LoadedConfig) -> Value {
    let session = runtime.session();
    let mut doc = json!({
        "thinkingLevel": crate::effective_thinking_level(runtime, config),
        "cwd": session.cwd.display().to_string(),
    });
    if let Some(model) = &session.active_model {
        doc["model"] = json!(format!("{}/{}", model.provider, model.id));
    }
    if let Some(compaction) = &session.compaction {
        doc["compaction"] = json!(compaction);
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use noel_chord::delta::apply_owned;
    use noel_core::{ConversationMessage, ReloadableSystems};

    fn message(role: MessageRole, content: &str) -> ConversationMessage {
        ConversationMessage {
            role,
            content: content.into(),
            thinking: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        }
    }

    /// A saved message, as the store reports it.
    fn saved(message: ConversationMessage) -> SavedProgress {
        SavedProgress::History(HistoryRecord::Message {
            message,
            timestamp: Some(5),
            queued: None,
        })
    }

    /// What a subscriber that took the snapshot at `from` (a count of
    /// batches applied) would hold after the ops since.
    fn replay(view: &View, snapshot: Value, from: usize) -> Value {
        view.applied[from..]
            .iter()
            .fold(snapshot, |value, ops| apply_owned(value, ops).unwrap())
    }

    #[test]
    fn a_run_streams_its_committed_partial_then_the_reply_as_an_entry() {
        let mut session = SessionState::new("s1".to_owned(), PathBuf::from("/w"));
        session.messages.push(message(MessageRole::User, "hello"));
        let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
        let history = [HistoryRecord::Message {
            message: message(MessageRole::User, "hello"),
            timestamp: Some(1),
            queued: None,
        }];
        let mut view = View::new(&session, &history);
        view.refresh(&runtime, json!({"thinkingLevel": "off", "cwd": "/w"}), &[]);
        let (snapshot, from) = (view.value(), view.applied.len());
        assert_eq!(
            view.value()["entries"],
            json!([{"id": 1, "kind": "pi.user", "timestamp": 1,
                    "model": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]}])
        );
        assert_eq!(view.value()["docs"]["pi.live"], json!({}));

        view.event(&Event::RunStarted {
            run_id: "r1",
            runtime: &runtime,
        });
        view.event(&Event::CallStarted {
            input: CallInput::Prompt,
        });
        let turn = |event: TurnEvent| event;
        let queue = noel_core::controller::Queue::default();
        let report = |view: &mut View, event: TurnEvent| {
            view.event(&Event::Turn {
                event: &event,
                queue: &queue,
            })
        };
        report(&mut view, turn(TurnEvent::RoundStarted));
        // Entries come from what is saved, not from what the engine reports.
        report(
            &mut view,
            TurnEvent::MessageAdded(message(MessageRole::User, "hello")),
        );
        assert_eq!(view.value()["entries"].as_array().unwrap().len(), 1);
        view.saved(&SavedProgress::Partial {
            run_id: Some("r1".into()),
            attempt: 1,
            text: "Hel".into(),
            thinking: String::new(),
            append: false,
        });
        view.saved(&SavedProgress::Partial {
            run_id: Some("r1".into()),
            attempt: 1,
            text: "lo".into(),
            thinking: String::new(),
            append: true,
        });
        let live = view.value()["docs"]["pi.live"].clone();
        assert_eq!(
            live["run"],
            json!({"runId": "r1", "inputs": ["r1"], "turns": 0})
        );
        assert_eq!(
            live["generation"]["message"],
            json!({"role": "assistant", "content": [{"type": "text", "text": "Hello"}]})
        );
        // The second partial went out as an append to the text.
        let last = view.applied.last().unwrap();
        assert!(
            matches!(last.as_slice(), [Op::Append(path, text)] if text == "lo" && path.len() == 7),
            "{last:?}"
        );
        view.saved(&saved(message(MessageRole::Assistant, "Hello")));
        report(
            &mut view,
            TurnEvent::MessageAdded(message(MessageRole::Assistant, "Hello")),
        );
        report(&mut view, TurnEvent::RoundFinished);
        view.event(&Event::RunSettled {
            run_id: "r1",
            outcome: &Outcome::Completed,
        });
        let value = view.value();
        assert_eq!(value["entries"].as_array().unwrap().len(), 2);
        assert_eq!(value["entries"][1]["id"], 2);
        assert_eq!(value["entries"][1]["kind"], "pi.assistant");
        assert_eq!(value["entries"][1]["model"][0]["role"], "assistant");
        assert_eq!(value["docs"]["pi.live"], json!({}));
        assert_eq!(
            value["lastRun"],
            json!({"runId": "r1", "status": "completed"})
        );
        // A subscriber that followed the ops has the same value.
        assert_eq!(replay(&view, snapshot, from), value);
    }

    #[test]
    fn a_resumed_run_shows_interrupted_results_with_their_saved_output() {
        let mut session = SessionState::new("s1".to_owned(), PathBuf::from("/w"));
        session.messages.push(message(MessageRole::User, "run it"));
        let mut reply = message(MessageRole::Assistant, "");
        reply.tool_calls.push(noel_ai::ChatToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        session.messages.push(reply);
        session.tool_outputs.insert(
            "c1".into(),
            noel_core::SavedToolOutput {
                run_id: Some("r1".into()),
                output: "started".into(),
                truncated: false,
            },
        );
        let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
        let history: Vec<_> = session
            .messages
            .iter()
            .map(|message| HistoryRecord::Message {
                message: message.clone(),
                timestamp: None,
                queued: None,
            })
            .collect();
        let mut view = View::new(&session, &history);
        view.refresh(&runtime, json!({}), &[]);
        let (snapshot, from) = (view.value(), view.applied.len());
        view.event(&Event::RunStarted {
            run_id: "r1",
            runtime: &runtime,
        });
        let tools = view.value()["docs"]["pi.live"]["tools"].clone();
        assert_eq!(
            tools,
            json!([{"callId": "c1", "name": "bash", "status": "interrupted", "output": "started"}])
        );
        view.event(&Event::CallStarted {
            input: CallInput::Resume,
        });
        // The engine saves the interrupted result: its outcome, then it.
        let output = interrupted_output("bash", session.tool_outputs.get("c1"));
        view.saved(&SavedProgress::History(HistoryRecord::ToolEvent {
            call_id: "c1".into(),
            is_error: true,
        }));
        let mut result = message(MessageRole::Tool, &output);
        result.tool_call_id = Some("c1".into());
        result.tool_name = Some("bash".into());
        view.saved(&saved(result));
        let value = view.value();
        assert_eq!(value["entries"][2]["kind"], "pi.tool-result");
        let result = &value["entries"][2]["model"][0];
        assert_eq!(result["role"], "toolResult");
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("started\n\n<harness>"), "{text}");
        assert!(text.contains("[error] Tool bash was interrupted"), "{text}");
        assert!(value["docs"]["pi.live"].get("tools").is_none());
        assert_eq!(replay(&view, snapshot, from), value);
    }

    #[test]
    fn the_window_keeps_the_newest_entries_and_pages_back() {
        let session = SessionState::new("s1".to_owned(), PathBuf::from("/w"));
        let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
        let history: Vec<_> = (0..120)
            .map(|index| HistoryRecord::Message {
                message: message(MessageRole::User, &index.to_string()),
                timestamp: None,
                queued: None,
            })
            .collect();
        let mut view = View::new(&session, &history);
        view.refresh(&runtime, json!({}), &[]);
        let ids = |value: &Value| -> Vec<u64> {
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_u64().unwrap())
                .collect()
        };
        let value = view.value();
        assert_eq!(ids(&value), (71..=120).collect::<Vec<_>>());
        assert_eq!(value["before"], 70);
        let (snapshot, from) = (view.value(), view.applied.len());
        // Fifty more fill the window to 100; the next makes the oldest leave
        // down to 50, in one splice.
        for index in 0..51 {
            view.saved(&saved(message(MessageRole::User, &format!("new {index}"))));
        }
        let value = view.value();
        assert_eq!(ids(&value), (122..=171).collect::<Vec<_>>());
        assert_eq!(value["before"], 121);
        assert_eq!(replay(&view, snapshot, from), value);
        // Paging back from the window's first entry.
        let page = view.page(122, 30);
        let page_ids: Vec<u64> = page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_u64().unwrap())
            .collect();
        assert_eq!(page_ids, (92..=121).collect::<Vec<_>>());
        assert_eq!(page["before"], 91);
        assert!(view.page(1, 30).get("before").is_none());
    }

    #[test]
    fn live_shows_turns_compactions_and_why_a_run_ended() {
        let session = SessionState::new("s1".to_owned(), PathBuf::from("/w"));
        let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
        let mut view = View::new(&session, &[]);
        view.refresh(&runtime, json!({}), &[]);
        let (snapshot, from) = (view.value(), view.applied.len());
        let queue = noel_core::controller::Queue::default();
        let report = |view: &mut View, event: TurnEvent| {
            view.event(&Event::Turn {
                event: &event,
                queue: &queue,
            })
        };
        let live = |view: &View| view.value()["docs"]["pi.live"].clone();
        view.event(&Event::RunStarted {
            run_id: "r1",
            runtime: &runtime,
        });
        // A round with a tool call is a turn; its result names its entry.
        report(&mut view, TurnEvent::RoundStarted);
        let mut reply = message(MessageRole::Assistant, "");
        reply.tool_calls.push(noel_ai::ChatToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        report(&mut view, TurnEvent::MessageAdded(reply));
        let mut result = message(MessageRole::Tool, "ok");
        result.tool_call_id = Some("c1".into());
        view.saved(&saved(result));
        assert_eq!(live(&view)["tools"][0]["entry"], 1);
        report(&mut view, TurnEvent::RoundFinished);
        assert_eq!(live(&view)["run"]["turns"], 1);
        // An overflow's compaction, which fails.
        report(&mut view, TurnEvent::Overflowed { error: None });
        report(&mut view, TurnEvent::CompactionStarted);
        assert_eq!(live(&view)["compactions"], json!([{"trigger": "overflow"}]));
        report(&mut view, TurnEvent::CompactionFailed("too big".into()));
        assert!(live(&view).get("compactions").is_none());
        assert_eq!(
            live(&view)["compactionFailure"],
            json!({"trigger": "overflow", "error": "too big"})
        );
        // The run fails; the failure stays until the next run.
        view.event(&Event::RunSettled {
            run_id: "r1",
            outcome: &Outcome::Failed("provider down".into()),
        });
        assert_eq!(
            live(&view),
            json!({"runFailure": {"reason": "model_error", "detail": "provider down", "inputs": ["r1"]}})
        );
        assert_eq!(replay(&view, snapshot, from), view.value());
        view.event(&Event::RunStarted {
            run_id: "r2",
            runtime: &runtime,
        });
        assert!(live(&view).get("runFailure").is_none());
    }

    /// #145: progress for a tool already done does not refill its output,
    /// and a manual compaction's failure is shown.
    #[test]
    fn late_tool_progress_is_ignored_and_a_manual_compaction_failure_shows() {
        let session = SessionState::new("s1".to_owned(), PathBuf::from("/w"));
        let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
        let mut view = View::new(&session, &[]);
        view.refresh(&runtime, json!({}), &[]);
        let queue = noel_core::controller::Queue::default();
        let report = |view: &mut View, event: TurnEvent| {
            view.event(&Event::Turn {
                event: &event,
                queue: &queue,
            })
        };
        let live = |view: &View| view.value()["docs"]["pi.live"].clone();
        view.event(&Event::RunStarted {
            run_id: "r1",
            runtime: &runtime,
        });
        report(&mut view, TurnEvent::RoundStarted);
        let call = noel_ai::ChatToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        report(&mut view, TurnEvent::ToolStarted { call: call.clone() });
        let progress = |output: &str| SavedProgress::ToolOutput {
            run_id: Some("r1".into()),
            call_id: "c1".into(),
            output: output.into(),
            truncated: false,
            append: true,
        };
        view.saved(&progress("partial"));
        assert_eq!(live(&view)["tools"][0]["output"], "partial");
        report(
            &mut view,
            TurnEvent::ToolOutput {
                call: call.clone(),
                output: "done".into(),
                images: Vec::new(),
                is_error: false,
                duration: None,
            },
        );
        assert_eq!(live(&view)["tools"][0]["status"], "done");
        // Late: the call is done, so its slot keeps no output.
        view.saved(&progress(" more"));
        assert!(live(&view)["tools"][0].get("output").is_none());
        report(&mut view, TurnEvent::RoundFinished);
        view.event(&Event::RunSettled {
            run_id: "r1",
            outcome: &Outcome::Completed,
        });
        view.event(&Event::CompactionStarted);
        assert_eq!(live(&view)["compactions"], json!([{"trigger": "manual"}]));
        view.event(&Event::CompactionEnded {
            result: Err("nothing to compact"),
        });
        assert_eq!(
            live(&view)["compactionFailure"],
            json!({"trigger": "manual", "error": "nothing to compact"})
        );
    }
}

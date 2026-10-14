//! The view a worker publishes as `noel.chat`'s `view` (spec §7.3): Durable's
//! shape `{conversation, entries, docs, lastRun}` (`durable/docs/spec.md`
//! §9.3) with the documents noel has data for.
//!
//! - `conversation`: `{id, cwd, name?}`.
//! - `entries`: the transcript as upstream pi's `Message` JSON, as RPC
//!   mode's `get_messages` reports it.
//! - `docs["pi.agent"]`: `{model?, thinkingLevel, cwd}`.
//! - `docs["pi.live"]`: `{}` while idle; while a run is active, `run`
//!   (`{runId, requestId?, inputs}`), `generation` (`{attempt, message?,
//!   retry?}`, where `message` is the reply's committed partial), `tools`
//!   (the round's calls, `{callId, name, status, output?, truncated?}`, with
//!   their committed output) and `compaction` while one runs.
//! - `docs["pi.inbox"]`: `{items: [{id, mode, content, requestId?}]}`.
//! - `docs["pi.usage"]`: `{total: {input, output, requests}}`.
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
use noel_chord::Cell;
use noel_core::controller::{CallInput, Event, Outcome};
use noel_core::{
    interrupted_output, MessageRole, QueueItem, QueueMode, Runtime, SavedProgress, SessionState,
    TurnEvent,
};
use serde_json::{json, Map, Value};

use crate::rpc_wire::{LastRun, Message};

/// The view's schema, as `noel.chat` declares it.
pub(crate) fn schema() -> Value {
    let contract = super::chat_contract();
    let member = contract
        .variants
        .get("participant")
        .and_then(|members| members.get("view"))
        .expect("noel.chat has a view");
    match member {
        noel_chord::MemberDescription::State { schema } => schema.clone(),
        _ => unreachable!("view is state"),
    }
}

/// The view of one session and what of it the cell holds.
pub(crate) struct View {
    cell: Cell,
    conversation: Value,
    entries: Vec<Value>,
    agent: Value,
    usage: Value,
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
    /// The saved prompt the run's first engine call reports again.
    skip_prompt: Option<Value>,
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
    compaction: Option<&'static str>,
}

struct RunDoc {
    run_id: String,
    request_id: Option<String>,
    inputs: Vec<String>,
}

#[derive(Default)]
struct Generation {
    attempt: u64,
    text: String,
    thinking: String,
    retry: Option<(u64, String)>,
}

struct ToolSlot {
    call_id: String,
    name: String,
    status: &'static str,
    output: String,
    truncated: bool,
}

impl View {
    /// An empty view of `session`; [`View::refresh`] fills it.
    pub fn new(session: &SessionState) -> Self {
        let conversation = conversation(session);
        let initial = json!({"conversation": conversation, "entries": [], "docs": {}});
        let cell = Cell::new(schema(), initial).expect("the empty view fits its schema");
        Self {
            cell,
            published_conversation: conversation.clone(),
            conversation,
            entries: Vec::new(),
            agent: json!({}),
            usage: json!({}),
            inbox: json!({"items": []}),
            live: Live::default(),
            last_run: None,
            dirty_from: None,
            published_entries: 0,
            published_docs: BTreeMap::new(),
            published_last_run: None,
            skip_prompt: None,
            resuming: Vec::new(),
            empty: SessionState::new(String::new(), PathBuf::new()),
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
        self.set_entries(session);
        self.agent = agent;
        self.usage = usage(session);
        self.inbox = inbox(queue);
        self.last_run = LastRun::of(runtime).map(|last| to_json(&last));
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
                self.set_entries(session);
                self.usage = usage(session);
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
                self.live.compaction = Some("manual");
                true
            }
            Event::CompactionEnded { .. } => {
                self.live.compaction = None;
                true
            }
            Event::SaveFailed { .. } => false,
            Event::RunSettled { run_id, outcome } => {
                let request_id = self.live.run.take().and_then(|run| run.request_id);
                self.live = Live::default();
                self.resuming.clear();
                self.skip_prompt = None;
                let mut last = json!({"runId": run_id, "status": to_json(&outcome.status())});
                if let Some(request_id) = request_id {
                    last["requestId"] = json!(request_id);
                }
                if let Outcome::Failed(error) = outcome {
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
                let slot = self.slot(call_id, None);
                if *append {
                    slot.output.push_str(output);
                } else {
                    slot.output.clone_from(output);
                }
                slot.truncated = *truncated;
            }
        }
        self.publish();
    }

    fn run_started(&mut self, run_id: &str, runtime: &Runtime) {
        let session = runtime.session();
        self.conversation = conversation(session);
        self.set_entries(session);
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
                    "pending"
                } else {
                    "interrupted"
                },
                output: saved.map(|saved| saved.output.clone()).unwrap_or_default(),
                truncated: saved.is_some_and(|saved| saved.truncated),
            });
        }
    }

    fn call_started(&mut self, input: CallInput) -> bool {
        match input {
            CallInput::Prompt => {
                // The run's prompt is saved with its start, and the engine
                // reports it again as its first round opens.
                self.skip_prompt = self.entries.last().cloned();
                false
            }
            CallInput::Resume => {
                // The calls that do not run again get their interrupted
                // result first thing; show it now.
                let resuming = std::mem::take(&mut self.resuming);
                for (call, replay_safe, result) in resuming {
                    if replay_safe {
                        continue;
                    }
                    self.live.tools.retain(|slot| slot.call_id != call.id);
                    self.push_entry(to_json(&Message::tool_result(&call, &result, &[], true)));
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
                self.live.tools.retain(|slot| slot.status == "pending");
                true
            }
            TurnEvent::RoundResumed(_) => false,
            TurnEvent::MessageAdded(message) => {
                let entry = to_json(&Message::saved(message, &self.empty));
                if self.skip_prompt.take().as_ref() == Some(&entry)
                    && message.role == MessageRole::User
                {
                    return false;
                }
                self.push_entry(entry);
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
                            status: "pending",
                            output: String::new(),
                            truncated: false,
                        })
                        .collect();
                }
                true
            }
            TurnEvent::Provider(_) | TurnEvent::ToolUpdate { .. } | TurnEvent::TodosUpdated(_) => {
                false
            }
            TurnEvent::RetryStarted(notice) => {
                self.live.generation = Some(Generation {
                    attempt: notice.attempt + 1,
                    retry: Some((
                        super::now_ms() + notice.delay_ms,
                        notice.error_message.clone(),
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
                if let Some(generation) = &mut self.live.generation {
                    generation.text.clear();
                    generation.thinking.clear();
                }
                true
            }
            TurnEvent::ToolStarted { call } => {
                let slot = self.slot(&call.id, Some(&call.name));
                slot.status = "running";
                true
            }
            TurnEvent::ToolFinished { .. } => false,
            TurnEvent::ToolOutput {
                call,
                output,
                images,
                is_error,
                ..
            } => {
                self.push_entry(to_json(&Message::tool_result(
                    call, output, images, *is_error,
                )));
                let slot = self.slot(&call.id, Some(&call.name));
                slot.status = "done";
                slot.output.clear();
                slot.truncated = false;
                true
            }
            TurnEvent::CompactionStarted => {
                self.live.compaction = Some("auto");
                true
            }
            TurnEvent::CompactionFinished(_) | TurnEvent::CompactionFailed(_) => {
                self.live.compaction = None;
                true
            }
            TurnEvent::RoundFinished => {
                self.live.generation = None;
                self.live.tools.clear();
                true
            }
        }
    }

    fn slot(&mut self, call_id: &str, name: Option<&str>) -> &mut ToolSlot {
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
                    status: "running",
                    output: String::new(),
                    truncated: false,
                });
                self.live.tools.len() - 1
            }
        };
        let slot = &mut self.live.tools[index];
        if let Some(name) = name {
            name.clone_into(&mut slot.name);
        }
        slot
    }

    fn push_entry(&mut self, entry: Value) {
        let index = self.entries.len();
        self.dirty_from = Some(self.dirty_from.map_or(index, |from| from.min(index)));
        self.entries.push(entry);
    }

    /// Makes the session's messages the entries, marking where they differ.
    fn set_entries(&mut self, session: &SessionState) {
        let entries: Vec<Value> = session
            .messages
            .iter()
            .map(|message| to_json(&Message::saved(message, session)))
            .collect();
        let same = self
            .entries
            .iter()
            .zip(&entries)
            .take_while(|(old, new)| old == new)
            .count();
        if same < self.entries.len() || same < entries.len() {
            self.dirty_from = Some(self.dirty_from.map_or(same, |from| from.min(same)));
        }
        self.entries = entries;
    }

    fn live_json(&self) -> Value {
        let mut live = Map::new();
        if let Some(run) = &self.live.run {
            let mut doc = json!({"runId": run.run_id, "inputs": run.inputs});
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
            if let Some((at, error)) = &generation.retry {
                doc["retry"] = json!({"at": at, "error": error});
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
                        json!({"callId": slot.call_id, "name": slot.name, "status": slot.status});
                    if !slot.output.is_empty() {
                        doc["output"] = json!(slot.output);
                    }
                    if slot.truncated {
                        doc["truncated"] = json!(true);
                    }
                    doc
                })
                .collect::<Vec<_>>();
            live.insert("tools".into(), Value::Array(tools));
        }
        if let Some(reason) = self.live.compaction {
            live.insert("compaction".into(), json!({"reason": reason}));
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
        let docs: [(&'static str, Value); 4] = [
            ("pi.agent", self.agent.clone()),
            ("pi.live", self.live_json()),
            ("pi.inbox", self.inbox.clone()),
            ("pi.usage", self.usage.clone()),
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
        view
    }
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

/// `pi.agent`: the session's model, its thinking level in effect, and its
/// working directory.
pub(crate) fn agent(runtime: &Runtime, config: &noel_config::LoadedConfig) -> Value {
    let session = runtime.session();
    let mut doc = json!({
        "thinkingLevel": crate::effective_thinking_level(runtime, config),
        "cwd": session.cwd.display().to_string(),
    });
    if let Some(model) = &session.active_model {
        doc["model"] = json!(format!("{}/{}", model.provider, model.id));
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
        let mut view = View::new(&session);
        view.refresh(&runtime, json!({"thinkingLevel": "off", "cwd": "/w"}), &[]);
        let (snapshot, from) = (view.value(), view.applied.len());
        assert_eq!(view.value()["entries"].as_array().unwrap().len(), 1);
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
        // The prompt reported again is not a second entry.
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
        assert_eq!(live["run"], json!({"runId": "r1", "inputs": ["r1"]}));
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
        assert_eq!(value["entries"][1]["role"], "assistant");
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
        let mut view = View::new(&session);
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
        let value = view.value();
        let result = &value["entries"][2];
        assert_eq!(result["role"], "toolResult");
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("started\n\n<harness>"), "{text}");
        assert!(text.contains("[error] Tool bash was interrupted"), "{text}");
        assert!(value["docs"]["pi.live"].get("tools").is_none());
        assert_eq!(replay(&view, snapshot, from), value);
    }
}

//! Durable partial output (spec §4.2, following upstream Durable): what a
//! streaming reply and a running tool call have produced so far is saved to
//! the journal at most every `progress.partialIntervalMs` and
//! `progress.outputIntervalMs`, so a crash loses at most the last interval.
//! The reply, or the call's result, supersedes what was saved of it; what a
//! stop leaves is recovered as an aborted reply ([`SessionStore::open`]) and
//! an interrupted result ([`interrupted_output`]).
//!
//! The journal only grows, so neither kind of record rewrites what it saved
//! while it can append instead. A reply's first record holds what it has
//! streamed so far and each later one only what streamed since, so a long
//! reply adds about its own size to the journal. A tool's output is the end
//! of what it printed, within the limits of its result: its records append
//! while it grows, and once it is longer than those limits the kept end
//! moves, so each record rewrites it whole, at intervals that double from
//! one rewrite to the next. A command that prints for an hour rewrites its
//! output about fifteen times. Compaction drops what is superseded.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use noel_ai::StreamEvent;
use noel_tools::OutputSoFar;
use tokio::time::Instant;

use crate::{ConversationMessage, SessionError, SessionRecord, SessionStore};

/// How often a run saves what it has produced so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeProgressSettings {
    /// `progress.partialIntervalMs`: a streaming reply's text and thinking.
    pub partial_interval: Duration,
    /// `progress.outputIntervalMs`: a running tool's output.
    pub output_interval: Duration,
}

/// Both intervals' default (upstream's `DEFAULT_PROGRESS_POLICY`).
pub const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

impl Default for RuntimeProgressSettings {
    fn default() -> Self {
        Self {
            partial_interval: DEFAULT_PROGRESS_INTERVAL,
            output_interval: DEFAULT_PROGRESS_INTERVAL,
        }
    }
}

/// Saves the first change after a quiet `interval` at once, and changes
/// that arrive sooner together once the interval since the last save is
/// over: at most one save per interval, and none without a change.
#[derive(Debug)]
pub(crate) struct Throttle {
    interval: Duration,
    last: Option<Instant>,
    /// A change arrived that is not saved yet.
    pending: bool,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            pending: false,
        }
    }

    /// Notes a change at `now`; `true` if it is to be saved now, else it
    /// waits for [`Throttle::due`].
    pub fn change(&mut self, now: Instant) -> bool {
        self.pending = true;
        self.last.is_none_or(|last| now >= last + self.interval)
    }

    /// When the change that waits is to be saved.
    pub fn due(&self) -> Option<Instant> {
        self.last
            .filter(|_| self.pending)
            .map(|last| last + self.interval)
    }

    /// Everything noted is saved, at `now`.
    pub fn saved(&mut self, now: Instant) {
        self.pending = false;
        self.last = Some(now);
    }

    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }
}

/// What the reply being streamed has produced so far, and how much of it
/// the journal holds. Without a journal nothing is saved.
#[derive(Debug)]
pub(crate) struct ReplyProgress {
    store: Option<SessionStore>,
    run_id: Option<String>,
    attempt: u64,
    text: String,
    thinking: String,
    /// The bytes of `text` and `thinking` saved.
    saved_text: usize,
    saved_thinking: usize,
    /// A record of this attempt is saved: later ones append to it.
    started: bool,
    throttle: Throttle,
    /// A save that failed. Nothing more is saved, and the request ends
    /// with it.
    error: Option<SessionError>,
}

impl ReplyProgress {
    pub fn new(store: Option<SessionStore>, run_id: Option<String>, interval: Duration) -> Self {
        Self {
            store,
            run_id,
            attempt: 1,
            text: String::new(),
            thinking: String::new(),
            saved_text: 0,
            saved_thinking: 0,
            started: false,
            throttle: Throttle::new(interval),
            error: None,
        }
    }

    /// Takes in a stream event at `now`, saving if the throttle allows;
    /// `true` when a save waits for [`ReplyProgress::due`].
    pub fn observe(&mut self, event: &StreamEvent, now: Instant) -> bool {
        let delta = match event {
            StreamEvent::Text(delta) => {
                self.text.push_str(delta);
                delta
            }
            StreamEvent::Thinking(delta) => {
                self.thinking.push_str(delta);
                delta
            }
            _ => return false,
        };
        if delta.is_empty() {
            return false;
        }
        if self.throttle.change(now) {
            self.save(now);
            return false;
        }
        true
    }

    pub fn due(&self) -> Option<Instant> {
        self.throttle.due()
    }

    /// Saves what waits once it is due.
    pub fn save_due(&mut self, now: Instant) {
        if self.due().is_some_and(|due| due <= now) {
            self.save(now);
        }
    }

    pub fn take_error(&mut self) -> Option<SessionError> {
        self.error.take()
    }

    /// The record that saves what streamed since the last one, if
    /// anything did.
    pub(crate) fn next_record(&self) -> Option<SessionRecord> {
        let text = &self.text[self.saved_text..];
        let thinking = &self.thinking[self.saved_thinking..];
        if text.is_empty() && thinking.is_empty() {
            return None;
        }
        Some(SessionRecord::Partial {
            run_id: self.run_id.clone(),
            attempt: self.attempt,
            text: text.to_owned(),
            thinking: thinking.to_owned(),
            append: self.started,
        })
    }

    fn save(&mut self, now: Instant) {
        self.throttle.saved(now);
        let Some(record) = self.next_record() else {
            return;
        };
        (self.saved_text, self.saved_thinking) = (self.text.len(), self.thinking.len());
        self.started = true;
        self.write(&record);
    }

    fn write(&mut self, record: &SessionRecord) {
        if self.error.is_some() {
            return;
        }
        if let Some(store) = &self.store {
            if let Err(error) = store.record_progress(record) {
                self.error = Some(error);
            }
        }
    }

    /// The reply is dropped and asked for again (a retry, or a compaction
    /// after an overflow): what was saved of it no longer stands. The next
    /// attempt starts empty.
    pub fn dropped(&mut self) -> Result<(), SessionError> {
        if self.started {
            // An empty record supersedes the dropped one: a stop now
            // leaves nothing to recover.
            self.write(&SessionRecord::Partial {
                run_id: self.run_id.clone(),
                attempt: self.attempt,
                text: String::new(),
                thinking: String::new(),
                append: false,
            });
        }
        self.reset(self.attempt + 1);
        self.take_error().map_or(Ok(()), Err)
    }

    /// The reply stands and is saved, which supersedes what was saved of
    /// it; the next reply is a first attempt.
    pub fn kept(&mut self) {
        self.reset(1);
    }

    /// The reply was cut off, by an abort or by the failure that ends the
    /// run: what it streamed, as an aborted reply. Saving it supersedes
    /// what was saved of it.
    pub fn aborted(&mut self) -> Option<ConversationMessage> {
        let partial = crate::PartialReply {
            run_id: self.run_id.clone(),
            attempt: self.attempt,
            text: std::mem::take(&mut self.text),
            thinking: std::mem::take(&mut self.thinking),
        };
        self.reset(1);
        partial.aborted_message()
    }

    fn reset(&mut self, attempt: u64) {
        self.attempt = attempt;
        self.text.clear();
        self.thinking.clear();
        (self.saved_text, self.saved_thinking, self.started) = (0, 0, false);
        self.throttle.pending = false;
    }
}

/// The longest wait between two rewrites of a tool's output, in doublings
/// of `progress.outputIntervalMs`.
const MAX_REWRITE_DOUBLINGS: u32 = 16;

/// What a running tool call has printed so far, and what the journal holds.
#[derive(Debug)]
pub(crate) struct ToolProgress {
    store: Option<SessionStore>,
    run_id: Option<String>,
    call_id: String,
    text: String,
    truncated: bool,
    /// The output as saved, once a record of the call is.
    saved: Option<(String, bool)>,
    interval: Duration,
    rewrites: u32,
    throttle: Throttle,
    error: Option<SessionError>,
}

impl ToolProgress {
    pub fn new(
        store: Option<SessionStore>,
        run_id: Option<String>,
        call_id: String,
        interval: Duration,
    ) -> Self {
        Self {
            store,
            run_id,
            call_id,
            text: String::new(),
            truncated: false,
            saved: None,
            interval,
            rewrites: 0,
            throttle: Throttle::new(interval),
            error: None,
        }
    }

    /// Takes in the output so far at `now`, saving if the throttle allows;
    /// `true` when a save waits for [`ToolProgress::due`].
    pub fn observe(&mut self, output: OutputSoFar<'_>, now: Instant) -> bool {
        if output.text == self.text && output.truncated == self.truncated {
            return false;
        }
        output.text.clone_into(&mut self.text);
        self.truncated = output.truncated;
        if self.throttle.change(now) {
            self.save(now);
            return false;
        }
        true
    }

    pub fn due(&self) -> Option<Instant> {
        self.throttle.due()
    }

    pub fn save_due(&mut self, now: Instant) {
        if self.due().is_some_and(|due| due <= now) {
            self.save(now);
        }
    }

    pub fn take_error(&mut self) -> Option<SessionError> {
        self.error.take()
    }

    /// The output so far, and whether its beginning was left out.
    pub fn output(&self) -> (&str, bool) {
        (&self.text, self.truncated)
    }

    /// The record that saves the output as it is now, if it changed, and
    /// whether it rewrites the saved output rather than extending it.
    pub(crate) fn next_record(&self) -> Option<(SessionRecord, bool)> {
        let record = |output: &str, append| SessionRecord::ToolOutput {
            run_id: self.run_id.clone(),
            call_id: self.call_id.clone(),
            output: output.to_owned(),
            truncated: self.truncated,
            append,
        };
        match &self.saved {
            None if self.text.is_empty() => None,
            None => Some((record(&self.text, false), false)),
            Some((text, truncated)) if *text == self.text && *truncated == self.truncated => None,
            Some((text, false)) if !self.truncated && self.text.starts_with(text.as_str()) => {
                Some((record(&self.text[text.len()..], true), false))
            }
            Some(_) => Some((record(&self.text, false), true)),
        }
    }

    fn save(&mut self, now: Instant) {
        self.throttle.saved(now);
        let Some((record, rewrite)) = self.next_record() else {
            return;
        };
        self.saved = Some((self.text.clone(), self.truncated));
        if rewrite {
            // The kept end moved: rewrites wait twice as long each time.
            self.rewrites = (self.rewrites + 1).min(MAX_REWRITE_DOUBLINGS);
            self.throttle
                .set_interval(self.interval.saturating_mul(1 << self.rewrites));
        }
        if self.error.is_some() {
            return;
        }
        if let Some(store) = &self.store {
            if let Err(error) = store.record_progress(&record) {
                self.error = Some(error);
            }
        }
    }
}

/// The result of a tool call that a stop cut off (upstream Durable's
/// `interrupted` result): the output it had saved, then a block saying so.
pub(crate) fn interrupted_output(tool: &str, saved: Option<&crate::SavedToolOutput>) -> String {
    let mut notes = Vec::new();
    if saved.is_some_and(|saved| saved.truncated) {
        notes.push("[warn] Output truncated to its end".to_owned());
    }
    notes.push(format!(
        "[error] Tool {tool} was interrupted and may have partially run"
    ));
    let block = format!("<harness>\n{}\n</harness>", notes.join("\n"));
    let output = saved.map_or("", |saved| saved.output.as_str());
    with_status(output, &block)
}

/// The result of a tool call an abort stopped: what it had printed, then
/// upstream pi's text for an aborted call.
pub(crate) fn aborted_output(output: &str) -> String {
    with_status(output, ABORTED_TOOL)
}

/// Upstream pi's result for a tool call an abort stopped.
pub(crate) const ABORTED_TOOL: &str = "Operation aborted";

/// `output`, then `status` after a blank line (upstream bash's
/// `appendStatus`).
fn with_status(output: &str, status: &str) -> String {
    if output.is_empty() {
        status.to_owned()
    } else {
        format!("{output}\n\n{status}")
    }
}

/// `journal` without the progress records a compaction checkpoint makes
/// stale: every partial reply (a compaction never runs while a reply
/// streams, and the checkpoint supersedes one), and each tool call's saved
/// output except the current one of a call in `pending`, which an
/// interrupted result still needs. Lines that do not parse are kept.
pub(crate) fn drop_superseded(journal: &[u8], pending: &BTreeSet<String>) -> Vec<u8> {
    let lines = journal
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    let mut keep = vec![true; lines.len()];
    // The records of each call's current output.
    let mut outputs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let discard = |indexes: Vec<usize>, keep: &mut Vec<bool>| {
        for index in indexes {
            keep[index] = false;
        }
    };
    for (index, line) in lines.iter().enumerate() {
        let Ok(record) = serde_json::from_slice::<SessionRecord>(line) else {
            continue;
        };
        match record {
            SessionRecord::Partial { .. } => keep[index] = false,
            SessionRecord::ToolOutput {
                call_id, append, ..
            } => {
                let chain = outputs.entry(call_id).or_default();
                if !append {
                    discard(std::mem::take(chain), &mut keep);
                }
                chain.push(index);
            }
            SessionRecord::Message { message, .. } => {
                if let Some(chain) = message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| outputs.remove(id))
                {
                    discard(chain, &mut keep);
                }
            }
            SessionRecord::RunFinished { .. } => {
                for (_, chain) in std::mem::take(&mut outputs) {
                    discard(chain, &mut keep);
                }
            }
            _ => {}
        }
    }
    for (call_id, chain) in outputs {
        if !pending.contains(&call_id) {
            discard(chain, &mut keep);
        }
    }
    lines
        .into_iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .flat_map(|(line, _)| line.iter().copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::{json, Value};

    use super::*;
    use crate::{
        new_session_id, provider_messages, MessageRole, PartialReply, SavedToolOutput, SessionState,
    };

    const INTERVAL: Duration = Duration::from_millis(100);

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// A new session in its own directory.
    fn store(name: &str) -> (SessionStore, SessionState, PathBuf) {
        let base = std::env::temp_dir().join(format!("noel-progress-{name}-{}", new_session_id()));
        let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
        (store, state, base)
    }

    /// The journal's records after `started`, without `type`'s companions
    /// that do not matter here.
    fn records(store: &SessionStore) -> Vec<Value> {
        let journal = fs::read_to_string(store.path()).unwrap();
        journal
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn text(delta: &str) -> StreamEvent {
        StreamEvent::Text(delta.into())
    }

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

    #[test]
    fn the_throttle_saves_the_first_change_at_once_and_the_rest_together() {
        let start = Instant::now();
        let mut throttle = Throttle::new(INTERVAL);
        assert_eq!(throttle.due(), None);
        assert!(throttle.change(start));
        throttle.saved(start);
        assert_eq!(throttle.due(), None);
        // Changes within the interval wait for its end, together.
        assert!(!throttle.change(start + ms(30)));
        assert!(!throttle.change(start + ms(60)));
        assert_eq!(throttle.due(), Some(start + INTERVAL));
        throttle.saved(start + INTERVAL);
        // Nothing changed: nothing is due.
        assert_eq!(throttle.due(), None);
        // A change after a quiet interval is saved at once again.
        assert!(throttle.change(start + ms(250)));
    }

    #[test]
    fn a_reply_saves_what_streamed_since_its_last_record_at_most_every_interval() {
        let (store, _, base) = store("reply");
        let start = Instant::now();
        let mut reply = ReplyProgress::new(Some(store.clone()), Some("run-1".into()), INTERVAL);
        // The first delta is saved at once; the next ones wait.
        assert!(!reply.observe(&text("Hel"), start));
        assert!(reply.observe(&text("lo"), start + ms(10)));
        assert!(reply.observe(&StreamEvent::Thinking("hmm".into()), start + ms(20)));
        // Events without text change nothing.
        let usage = StreamEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
        };
        assert!(!reply.observe(&usage, start + ms(30)));
        reply.save_due(start + ms(50));
        assert_eq!(records(&store).len(), 1);
        reply.save_due(start + INTERVAL);
        reply.save_due(start + ms(300));
        assert_eq!(
            records(&store),
            [
                json!({"type": "partial", "run_id": "run-1", "attempt": 1, "text": "Hel"}),
                json!({"type": "partial", "run_id": "run-1", "attempt": 1, "text": "lo", "thinking": "hmm", "append": true}),
            ]
        );
        // Together the records are the reply so far.
        let partial = SessionStore::read(store.path()).unwrap().partial;
        let expected = PartialReply {
            run_id: Some("run-1".into()),
            attempt: 1,
            text: "Hello".into(),
            thinking: "hmm".into(),
        };
        assert_eq!(partial, Some(expected));

        // A reply dropped to ask again is superseded by an empty record, and
        // the next attempt starts over.
        reply.dropped().unwrap();
        let partial = SessionStore::read(store.path()).unwrap().partial.unwrap();
        assert_eq!(partial.aborted_message(), None);
        assert!(!reply.observe(&text("Again"), start + ms(400)));
        let partial = SessionStore::read(store.path()).unwrap().partial.unwrap();
        assert_eq!((partial.attempt, partial.text.as_str()), (2, "Again"));
        // The reply, once saved, supersedes them all.
        store
            .record_message(message(MessageRole::Assistant, "Again and again"))
            .unwrap();
        assert_eq!(SessionStore::read(store.path()).unwrap().partial, None);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn opening_a_session_turns_its_saved_partial_into_an_aborted_reply_once() {
        let (store, mut state, base) = store("recover");
        let prompt = message(MessageRole::User, "tell a story");
        store.record_message(prompt.clone()).unwrap();
        state.messages.push(prompt);
        let start = Instant::now();
        let mut reply = ReplyProgress::new(Some(store.clone()), None, INTERVAL);
        reply.observe(&StreamEvent::Thinking("plot".into()), start);
        reply.observe(&text("Once upon"), start + ms(10));
        reply.save_due(start + INTERVAL);
        // A stop: the process ends here, and its lock with it.
        drop((reply, store));

        let (store, reopened) =
            SessionStore::open(base.join(format!("{}.jsonl", state.session_id))).unwrap();
        let aborted = reopened.messages.last().unwrap();
        assert!(aborted.aborted);
        assert_eq!(aborted.role, MessageRole::Assistant);
        assert_eq!(
            (aborted.content.as_str(), aborted.thinking.as_str()),
            ("Once upon", "plot")
        );
        assert_eq!(reopened.partial, None);
        // The aborted reply stays in the transcript, out of requests, and
        // the turn is still unfinished.
        assert_eq!(reopened.messages.len(), 2);
        assert_eq!(provider_messages(&reopened.messages).len(), 1);
        assert!(reopened.has_unfinished_turn());
        assert_eq!(reopened.pending_tool_calls(), None);
        // Reopening again finds nothing more to recover.
        let path = store.path().to_path_buf();
        drop(store);
        let (_, again) = SessionStore::open(path).unwrap();
        assert_eq!(again, reopened);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_partial_that_streamed_nothing_recovers_as_nothing() {
        let (store, _, base) = store("empty");
        store
            .record_message(message(MessageRole::User, "hi"))
            .unwrap();
        let mut reply = ReplyProgress::new(Some(store.clone()), None, INTERVAL);
        reply.observe(&text(""), Instant::now());
        assert_eq!(records(&store).len(), 1);
        let path = store.path().to_path_buf();
        drop((reply, store));
        let (_, state) = SessionStore::open(path).unwrap();
        assert_eq!(state.messages.len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    fn output(text: &str, truncated: bool) -> OutputSoFar<'_> {
        OutputSoFar { text, truncated }
    }

    #[test]
    fn tool_output_appends_while_it_grows_and_rewrites_at_doubling_intervals_once_truncated() {
        let (store, _, base) = store("tool");
        let start = Instant::now();
        let mut tool = ToolProgress::new(
            Some(store.clone()),
            Some("run-1".into()),
            "b1".into(),
            INTERVAL,
        );
        // The report that the command starts has nothing to save.
        assert!(!tool.observe(output("", false), start));
        assert!(records(&store).is_empty());
        assert!(!tool.observe(output("one\n", false), start));
        assert!(tool.observe(output("one\ntwo\n", false), start + ms(50)));
        tool.save_due(start + INTERVAL);
        // Past the limits, the kept end moves: each record rewrites it, and
        // the wait before the next rewrite doubles.
        assert!(!tool.observe(output("two\nthree\n", true), start + ms(200)));
        assert!(tool.observe(output("three\nfour\n", true), start + ms(300)));
        assert_eq!(tool.due(), Some(start + ms(400)));
        tool.save_due(start + ms(400));
        assert!(tool.observe(output("four\nfive\n", true), start + ms(500)));
        assert_eq!(tool.due(), Some(start + ms(800)));
        assert_eq!(
            records(&store),
            [
                json!({"type": "tool_output", "run_id": "run-1", "call_id": "b1", "output": "one\n"}),
                json!({"type": "tool_output", "run_id": "run-1", "call_id": "b1", "output": "two\n", "append": true}),
                json!({"type": "tool_output", "run_id": "run-1", "call_id": "b1", "output": "two\nthree\n", "truncated": true}),
                json!({"type": "tool_output", "run_id": "run-1", "call_id": "b1", "output": "three\nfour\n", "truncated": true}),
            ]
        );
        let saved = SessionStore::read(store.path()).unwrap().tool_outputs;
        let expected = SavedToolOutput {
            run_id: Some("run-1".into()),
            output: "three\nfour\n".into(),
            truncated: true,
        };
        assert_eq!(saved.get("b1"), Some(&expected));
        // The call's result supersedes them.
        let mut result = message(MessageRole::Tool, "done");
        result.tool_call_id = Some("b1".into());
        store.record_message(result).unwrap();
        assert!(SessionStore::read(store.path())
            .unwrap()
            .tool_outputs
            .is_empty());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn an_interrupted_result_is_the_saved_output_and_says_it_was_interrupted() {
        let block =
            "<harness>\n[error] Tool bash was interrupted and may have partially run\n</harness>";
        assert_eq!(interrupted_output("bash", None), block);
        let saved = SavedToolOutput {
            run_id: None,
            output: "line 1\nline 2\n".into(),
            truncated: false,
        };
        assert_eq!(
            interrupted_output("bash", Some(&saved)),
            format!("line 1\nline 2\n\n\n{block}")
        );
        let truncated = SavedToolOutput {
            truncated: true,
            ..saved
        };
        assert_eq!(
            interrupted_output("bash", Some(&truncated)),
            "line 1\nline 2\n\n\n<harness>\n[warn] Output truncated to its end\n[error] Tool bash was interrupted and may have partially run\n</harness>"
        );
        assert_eq!(aborted_output(""), "Operation aborted");
        assert_eq!(aborted_output("so far"), "so far\n\nOperation aborted");
    }

    #[test]
    fn compaction_drops_progress_that_is_superseded() {
        let lines = [
            json!({"type": "started", "session_id": "s", "cwd": "."}),
            json!({"type": "message", "message": {"role": "user", "content": "go"}}),
            json!({"type": "partial", "attempt": 1, "text": "Hel"}),
            json!({"type": "partial", "attempt": 1, "text": "lo", "append": true}),
            json!({"type": "message", "message": {"role": "assistant", "content": "Hello", "tool_calls": [
                {"id": "done", "name": "bash", "arguments": "{}"},
                {"id": "open", "name": "bash", "arguments": "{}"}
            ]}}),
            json!({"type": "tool_output", "call_id": "done", "output": "a\n"}),
            json!({"type": "message", "message": {"role": "tool", "content": "a\n", "tool_call_id": "done"}}),
            json!({"type": "tool_output", "call_id": "open", "output": "x\n"}),
            json!({"type": "tool_output", "call_id": "gone", "output": "y\n"}),
            json!({"type": "tool_output", "call_id": "open", "output": "y\n", "truncated": true}),
            json!({"type": "tool_output", "call_id": "open", "output": "z\n", "append": true}),
        ];
        let mut journal = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>()
            .into_bytes();
        // A line this version cannot read is kept as it is.
        journal.extend(b"{\"type\":\"message\",\"message\":7}\n");
        let pending = BTreeSet::from(["open".to_owned()]);
        let kept = drop_superseded(&journal, &pending);
        let kept = String::from_utf8(kept).unwrap();
        let expected = [0, 1, 4, 6, 9, 10]
            .iter()
            .map(|index| format!("{}\n", lines[*index]))
            .collect::<String>()
            + "{\"type\":\"message\",\"message\":7}\n";
        assert_eq!(kept, expected);
        // A finished run supersedes every call's output.
        let finished = format!(
            "{}{}\n",
            String::from_utf8(journal.clone()).unwrap(),
            json!({"type": "run_finished", "run_id": "r", "status": "aborted"})
        );
        let kept = drop_superseded(finished.as_bytes(), &pending);
        assert!(!String::from_utf8(kept).unwrap().contains("tool_output"));
    }

    #[test]
    fn a_journal_from_before_partials_opens_as_it_did() {
        // Written by the previous version: no partial records, no aborted
        // replies, and records this version does not know are ignored.
        let journal = [
            json!({"type": "started", "session_id": "old", "cwd": "."}),
            json!({"type": "message", "message": {"role": "user", "content": "work"}, "run": {"id": "run-1", "request_id": "m-1"}}),
            json!({"type": "message", "message": {"role": "assistant", "content": "", "tool_calls": [{"id": "b1", "name": "bash", "arguments": "{}"}]}}),
            json!({"type": "tool", "event": {"id": "b1", "name": "bash", "result": "ok", "is_error": false}}),
            json!({"type": "message", "message": {"role": "tool", "content": "ok", "tool_call_id": "b1", "tool_name": "bash"}}),
            json!({"type": "message", "message": {"role": "assistant", "content": "done"}}),
            json!({"type": "run_finished", "run_id": "run-1", "status": "completed"}),
            json!({"type": "from_a_newer_version", "anything": 1}),
        ];
        let base = std::env::temp_dir().join(format!("noel-progress-old-{}", new_session_id()));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("old.jsonl");
        let text = journal
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(&path, &text).unwrap();
        let (_, state) = SessionStore::open(path.clone()).unwrap();
        assert_eq!(state.session_id, "old");
        assert_eq!(state.messages.len(), 4);
        assert!(state.messages.iter().all(|message| !message.aborted));
        assert_eq!((state.partial.clone(), state.tool_outputs.len()), (None, 0));
        assert_eq!(state.runs.len(), 1);
        assert!(!state.has_unfinished_turn());
        assert_eq!(provider_messages(&state.messages).len(), 4);
        // Opening wrote nothing.
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_dir_all(base).unwrap();
    }
}

mod codemode;
pub mod history;
pub use codemode::{NestedCallStatus, NestedToolCall};
mod compaction;
pub mod controller;
#[cfg(test)]
mod controller_tests;
pub mod extensions;
#[cfg(test)]
mod goal_tests;
mod goals;
mod hooks;
mod input;
mod journal;
mod mcp;
mod model_tools;
#[cfg(test)]
mod plan_tests;
pub mod plugins;
mod progress;
#[cfg(test)]
mod recovery_tests;
mod rewind;
#[cfg(test)]
mod rewind_tests;
mod session_export;
mod session_lock;
pub mod storage;
#[cfg(test)]
mod subagent_tests;
mod subagents;
mod system_prompt;
mod tool_calls;
#[cfg(test)]
mod tool_parity_tests;
#[cfg(test)]
mod tool_tests;
mod turn;
mod workspace;
pub use compaction::RuntimeCompactionSettings;
pub use goals::{
    continuation_prompt, evaluate_goal, kickoff_prompt, parse_verdict, GoalVerdict, SessionGoal,
    GOAL_MAX_TURNS,
};
pub use hooks::{run_post_hooks, run_pre_hooks, HookDecision};
pub use input::expand_input;
#[allow(unused_imports)]
pub use journal::*;
pub use mcp::{read_mcp_auth, write_mcp_auth, McpAuthEntry, McpHosts, McpTool, SharedMcpHosts};
#[allow(unused_imports)]
pub use model_tools::*;
pub use noel_mcp::{McpClient, McpError, StdioOptions};
pub use progress::{interrupted_output, RuntimeProgressSettings};
use progress::{ReplyProgress, ToolProgress};
pub use rewind::Undone;
#[allow(unused_imports)]
pub use session_export::*;
#[cfg(test)]
use session_lock::SessionLock;
pub use subagents::{run_task_tool, SubagentEvent, TaskToolArgs};
pub use system_prompt::PRODUCT_NAME;
use system_prompt::{PromptTool, SystemPromptInput};
#[allow(unused_imports)]
pub use tool_calls::*;
#[allow(unused_imports)]
pub use turn::*;
pub use workspace::SessionWorkspace;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use noel_agent::RetryPolicy;
pub use noel_agent::{CancellationToken, RetryNotice};
use noel_ai::{
    ChatMessage, ChatRole, ChatToolCall, MediaInput, ModelRef, Provider, ProviderError,
    ProviderRequest, StreamEvent, ToolDefinition as AiToolDefinition,
};

/// Builds a provider for one model, as a subagent's override asks.
pub type SubagentProviderFactory = Arc<dyn Fn(&ModelRef) -> BoxProviderFuture + Send + Sync>;

/// A boxed, shared future, as a provider factory returns.
pub type BoxProviderFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Box<dyn Provider>, String>> + Send + 'static>,
>;
use noel_config::{
    has_auth_for_provider, listed_accounts_for_provider, AgentDefinition, ContextFile,
    HooksSettings, LoadedConfig, ResourceFile, Skill,
};
use noel_env::LocalEnvOptions;
pub use noel_env::{ExecutionEnv, LocalEnv};
/// Kills every running tool subprocess tree; for noel's fatal-signal handling.
pub use noel_tools::kill_running_tool_groups;
use noel_tools::{
    builtin_tool_definitions, cap_tool_output, execute_tool_in, execute_tool_streaming_in,
    spawn_tool_process, OutputSoFar, ToolError, ToolRequest, ToolResult, ToolRuntimeOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
    /// Provider-supplied thinking, kept separate from conversational context.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(default)]
    pub media: Vec<MediaInput>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ChatToolCall>,
    /// A reply that was cut off: by an abort, by the failure that ended its
    /// run, or by a stop, after which recovery saved what it had streamed
    /// (spec §4.2). It stays in the transcript and is left out of model
    /// requests. Older versions ignore it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub aborted: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    pub id: String,
    pub name: String,
    pub result: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

pub fn format_todo_list(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "no todos".to_string();
    }
    todos
        .iter()
        .enumerate()
        .map(|(index, todo)| {
            let marker = match todo.status {
                TodoStatus::Completed => "[x]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Pending => "[ ]",
            };
            format!("{}. {} {}", index + 1, marker, todo.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Provider-reported token totals, independent of context trimming and undo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
}

/// A run's identity, saved on the record of the prompt that starts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStart {
    pub id: String,
    /// The client's id for the prompt; a prompt that repeats it gets this
    /// run instead of starting another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// What the client called the submission that started the run, so a
    /// `requestId` reused for another kind is refused. Older versions
    /// started runs only with prompts, and leave it out.
    #[serde(default, skip_serializing_if = "SubmissionKind::is_prompt")]
    pub kind: SubmissionKind,
}

/// A run: a prompt and the work it caused, until it settled. RPC mode
/// starts runs; the journal keeps their identity apart from the messages,
/// which compaction and `/undo` rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "SubmissionKind::is_prompt")]
    pub kind: SubmissionKind,
    pub status: RunStatus,
    /// Why a failed run ended without an answer (Durable's `runFailure`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<RunFailure>,
    /// Crash restarts since the run last made progress (Durable's task
    /// `restarts`): a resume after a stop that was not a close counts one,
    /// and a message the run saves clears them.
    #[serde(default, skip_serializing_if = "no_restarts")]
    pub restarts: u32,
    /// A close stopped the run (not a crash): its resume is not counted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed: bool,
}

fn no_restarts(value: &u32) -> bool {
    *value == 0
}

impl Run {
    fn started(start: RunStart, status: RunStatus) -> Self {
        Self {
            id: start.id,
            request_id: start.request_id,
            kind: start.kind,
            status,
            failure: None,
            restarts: 0,
            closed: false,
        }
    }

    // The run's transitions, the same live and on replay (#130). `closed`
    // means only "a close stopped this run": a resume or an end clears it,
    // so a later stop is classified by what happens then.

    /// A close stopped the run: its next resume is not a restart.
    fn close(&mut self) {
        self.closed = true;
    }

    /// The run continues after a stop that was not a close: a restart.
    fn restart(&mut self) -> u32 {
        self.closed = false;
        self.restarts += 1;
        self.restarts
    }

    /// The run continues after a close: no restart, but it is open again.
    fn reopen(&mut self) {
        self.closed = false;
    }

    /// The run ended; only a failed run keeps a reason.
    fn finish(&mut self, status: RunStatus, failure: Option<RunFailure>) {
        self.status = status;
        self.closed = false;
        self.failure = failure.filter(|_| status == RunStatus::Failed);
    }
}

/// Why a run ended without an answer, as Durable's `pi.live.runFailure`
/// names it. An aborted run is [`RunStatus::Aborted`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    /// The model request failed, retries included.
    ModelError,
    /// The host died while the model was answering, and the retry policy
    /// allowed no other attempt.
    Interrupted,
    /// The run was restarted `restartLimit` times in a row without
    /// progress.
    RestartLimit,
    /// A continue found nothing to answer.
    NothingToAnswer,
    /// The run's tool rounds reached `agent.maxToolRounds`.
    TurnLimit,
}

impl FailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelError => "model_error",
            Self::Interrupted => "interrupted",
            Self::RestartLimit => "restart_limit",
            Self::NothingToAnswer => "nothing_to_answer",
            Self::TurnLimit => "turn_limit",
        }
    }
}

/// A failed run's reason and detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFailure {
    pub reason: FailureReason,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// What a client called a submission (spec §3.1): a prompt, a steer or a
/// follow-up. A `requestId` names one submission of one kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionKind {
    #[default]
    Prompt,
    Steer,
    FollowUp,
    /// Durable's `{type: "continue"}`: a run on the context as it is.
    Continue,
}

impl SubmissionKind {
    pub fn is_prompt(&self) -> bool {
        *self == Self::Prompt
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
            Self::Continue => "continue",
        }
    }
}

impl From<QueueMode> for SubmissionKind {
    fn from(mode: QueueMode) -> Self {
        match mode {
            QueueMode::Steer => Self::Steer,
            QueueMode::FollowUp => Self::FollowUp,
        }
    }
}

/// Where a queued submission waits: steering joins the active run at its
/// next provider round, after the current tool round; a follow-up starts
/// the run's next engine call when the run would answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    Steer,
    FollowUp,
}

/// A queued submission's identity, as the journal saves it: in the queue
/// record while it waits, then with the message that places it into a run,
/// or in the queue record that withdraws it. Only a submission with a
/// `requestId` is saved this way: nothing can ask about another one after
/// the process that queued it ends, so its id lasts only that long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItemId {
    pub submission_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub mode: QueueMode,
}

/// One saved queued submission: its mode, its text, and its saved identity
/// when it has one (see [`QueueItemId`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedEntry {
    pub mode: QueueMode,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<QueueItemId>,
}

impl QueuedEntry {
    /// A follow-up saved without an identity.
    pub fn follow_up(content: impl Into<String>) -> Self {
        Self {
            mode: QueueMode::FollowUp,
            content: content.into(),
            id: None,
        }
    }
}

impl QueueItemId {
    /// Whether the journal keeps this identity (see [`QueueItemId`]).
    pub fn is_durable(&self) -> bool {
        self.request_id.is_some()
    }
}

/// A queued submission: its identity, and its text as it will be placed
/// (a skill command or a prompt template already expanded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: QueueItemId,
    pub content: String,
}

/// A saved submission that left the queue (see [`QueueItemId`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PastSubmission {
    pub id: QueueItemId,
    /// The run it was placed into; `None` if it was withdrawn unanswered.
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Started by this process, which has not finished it yet.
    Running,
    /// Started by a process that stopped before finishing it.
    Interrupted,
    Completed,
    Failed,
    Aborted,
}

impl RunStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Aborted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub labels: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub tool_history: Vec<ToolEvent>,
    /// The saved queue, steering first (#138: three parallel fields, whose
    /// ids counted only while their lengths matched). Only RPC mode queues
    /// steering: the TUI turns leftover steering into follow-ups.
    #[serde(default)]
    pub queue: Vec<QueuedEntry>,
    /// Saved submissions that left the queue, oldest first, as the journal
    /// had them when the session was loaded. The session controller keeps
    /// its own copy current.
    #[serde(default)]
    pub past_submissions: Vec<PastSubmission>,
    #[serde(default)]
    pub compactions: Vec<CompactionRecord>,
    #[serde(default)]
    pub branch_summaries: Vec<BranchSummary>,
    pub active_model: Option<ModelRef>,
    #[serde(default)]
    pub active_thinking_level: Option<String>,
    #[serde(default)]
    pub active_account: Option<String>,
    /// This conversation's compaction settings over the settings file's
    /// (Durable's `AgentState.compaction`); forks inherit them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<noel_config::CompactionSettings>,
    pub active_tool_names: BTreeSet<String>,
    #[serde(default)]
    pub disabled_tools: BTreeSet<String>,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub edited_files: Vec<String>,
    /// What codemode scripts keep with `store()` (key to JSON value).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub codemode_store: BTreeMap<String, Value>,
    #[serde(default)]
    pub usage: SessionUsage,
    /// The session's runs, oldest first.
    #[serde(default)]
    pub runs: Vec<Run>,
    /// What a reply had streamed when it was last saved, while nothing has
    /// superseded it: the reply a stop cut off. Opening the session for
    /// writing saves it as an aborted reply (spec §4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<PartialReply>,
    /// What tool calls without a result had printed when it was last saved,
    /// by call id. An interrupted call's result starts with it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_outputs: BTreeMap<String, SavedToolOutput>,
    /// Where the session's tools run, when that is not this machine (spec
    /// §10); `None` runs them in `cwd` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<SessionWorkspace>,
    /// File snapshots this session took before an `edit`/`write` changed a
    /// file, oldest first: what rewind can restore. Not part of the
    /// export (the bytes live beside the journal, not in it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<SnapshotRef>,
    /// The session goal, when one is active (spec §9.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SessionGoal>,
    /// Plan mode (spec §9.4): research only, until a plan is approved.
    #[serde(default, skip_serializing_if = "is_false")]
    pub plan_mode: bool,
}

/// One file snapshot: its id, the file it holds, and what the bytes were
/// when it was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    pub id: String,
    pub path: String,
    pub before_sha: String,
    pub created: bool,
    /// How many messages the session held when the snapshot was taken (so
    /// more than the index of its turn's prompt); a rewind restores, per
    /// path, the oldest snapshot the dropped range covers.
    pub message_index: usize,
    /// What the file held once the edit this snapshot was taken for had
    /// run: a rewind leaves a file that no longer holds what its last edit
    /// left. None when the edit left no file, or was not seen to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha: Option<String>,
}

/// A streaming reply as far as the journal saved it (spec §4.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The request for the reply, from 1: a retry, or asking again after
    /// a compaction, is the next attempt.
    pub attempt: u64,
    pub text: String,
    pub thinking: String,
}

impl PartialReply {
    /// The reply as an aborted assistant message; `None` if it streamed
    /// nothing.
    pub fn aborted_message(&self) -> Option<ConversationMessage> {
        (!self.text.is_empty() || !self.thinking.is_empty()).then(|| ConversationMessage {
            role: MessageRole::Assistant,
            content: self.text.clone(),
            thinking: self.thinking.clone(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: true,
        })
    }
}

/// What a running tool call had printed as far as the journal saved it: the
/// end of its output, within the limits of its result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedToolOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub output: String,
    /// Whether output before `output` was left out.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl SessionState {
    /// Rewinds to the first `messages` messages (spec §9.2): the rest are
    /// dropped with the progress saved for them. Runs, past submissions,
    /// the queue, the tool audit and usage stay, as after any undo: they
    /// record what was asked and billed, and what a request id answers.
    ///
    /// `restore` names the snapshots the caller wrote back. With any, the
    /// files are as they were before the dropped range, so its snapshots
    /// are spent. With none the files keep their changes: the dropped
    /// range's snapshots then belong to the last turn kept, where a later
    /// rewind to an earlier prompt still finds them, or are forgotten when
    /// no turn is kept.
    pub fn rewind_to(&mut self, messages: usize, restore: &[String]) {
        self.messages.truncate(messages);
        self.partial = None;
        self.tool_outputs.clear();
        if !restore.is_empty() || messages == 0 {
            self.snapshots
                .retain(|snapshot| snapshot.message_index < messages);
        } else {
            for snapshot in &mut self.snapshots {
                snapshot.message_index = snapshot.message_index.min(messages - 1);
            }
        }
    }

    /// Replaces the message list. File snapshots are placed by message
    /// count, which only keeps its meaning while the new list is the start
    /// of the old one (an undo, a rewind). Any other replacement, such as
    /// a compaction, forgets them: a rewind does not reach past it.
    pub(crate) fn set_messages(&mut self, messages: Vec<ConversationMessage>) {
        if !self.messages.starts_with(&messages) {
            self.snapshots.clear();
        }
        self.messages = messages;
    }

    /// The saved queue: its steering, then its follow-ups.
    pub fn saved_queue(&self) -> (Vec<String>, Vec<String>) {
        let texts = |mode: QueueMode| {
            self.queue
                .iter()
                .filter(|entry| entry.mode == mode)
                .map(|entry| entry.content.clone())
                .collect()
        };
        (texts(QueueMode::Steer), texts(QueueMode::FollowUp))
    }

    /// The saved queue's texts, steering first.
    pub fn queued_messages(&self) -> Vec<String> {
        self.queue
            .iter()
            .map(|entry| entry.content.clone())
            .collect()
    }

    /// The saved queue's items, steering first, with their saved
    /// identities. An item saved without one (by an older version, by the
    /// TUI, or without a `requestId`) has `None`.
    pub fn saved_queue_items(&self) -> Vec<(QueueMode, String, Option<QueueItemId>)> {
        self.queue
            .iter()
            .map(|entry| {
                let id = entry.id.clone().filter(|id| id.mode == entry.mode);
                (entry.mode, entry.content.clone(), id)
            })
            .collect()
    }

    /// Takes the queued submissions `placed` out of the saved queue: the
    /// message that placed them into run `run_id` is saved.
    fn place_queued(&mut self, placed: Vec<QueueItemId>, run_id: Option<&str>) {
        for id in placed {
            let index = self.queue.iter().position(|entry| {
                entry
                    .id
                    .as_ref()
                    .is_some_and(|queued| queued.submission_id == id.submission_id)
            });
            if let Some(index) = index {
                self.queue.remove(index);
            }
            self.past_submissions.push(PastSubmission {
                id,
                run_id: run_id.map(str::to_owned),
            });
        }
    }

    /// Whether the tool call `call_id` of the last reply has a result.
    fn has_result(&self, call_id: &str) -> bool {
        let results = self.messages.iter().rev();
        let mut results = results.take_while(|message| message.role == MessageRole::Tool);
        results.any(|message| message.tool_call_id.as_deref() == Some(call_id))
    }

    /// `message` is saved: it supersedes a partial reply, and a tool
    /// result supersedes its call's saved output.
    fn supersede_progress(&mut self, message: &ConversationMessage) {
        self.partial = None;
        if let Some(call_id) = &message.tool_call_id {
            self.tool_outputs.remove(call_id);
        }
    }

    /// The run a prompt with `request_id` started, if one did.
    pub fn run_for_request(&self, request_id: &str) -> Option<&Run> {
        self.runs
            .iter()
            .rev()
            .find(|run| run.request_id.as_deref() == Some(request_id))
    }

    fn run_mut(&mut self, run_id: &str) -> Option<&mut Run> {
        self.runs.iter_mut().rev().find(|run| run.id == run_id)
    }

    /// The tool calls of the last reply that have no saved result, with
    /// that reply, when only tool results follow it: what a turn stopped
    /// during its tool calls still owes.
    pub fn pending_tool_calls(&self) -> Option<(&ConversationMessage, Vec<&ChatToolCall>)> {
        pending_tool_calls(&self.messages)
    }

    /// Model work awaiting a reply. Direct slash-tool results complete their
    /// turn without an assistant reply and have no model tool-call id. A
    /// reply cut off (`aborted`) answered nothing: the work it was for is
    /// still unfinished.
    pub fn has_unfinished_turn(&self) -> bool {
        self.messages
            .iter()
            .rfind(|message| !message.aborted)
            .is_some_and(|message| match message.role {
                MessageRole::User => true,
                MessageRole::Assistant => !message.tool_calls.is_empty(),
                MessageRole::Tool => message.tool_call_id.is_some(),
                MessageRole::System => false,
            })
    }

    pub fn new(session_id: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            session_id: session_id.into(),
            cwd,
            name: None,
            labels: BTreeSet::new(),
            parent_session_id: None,
            messages: Vec::new(),
            tool_history: Vec::new(),
            queue: Vec::new(),
            past_submissions: Vec::new(),
            compactions: Vec::new(),
            branch_summaries: Vec::new(),
            active_model: None,
            active_thinking_level: None,
            active_account: None,
            compaction: None,
            active_tool_names: default_active_tool_names(),
            disabled_tools: BTreeSet::new(),
            todos: Vec::new(),
            edited_files: Vec::new(),
            codemode_store: BTreeMap::new(),
            usage: SessionUsage::default(),
            runs: Vec::new(),
            partial: None,
            tool_outputs: BTreeMap::new(),
            workspace: None,
            snapshots: Vec::new(),
            goal: None,
            plan_mode: false,
        }
    }
}

/// The tool calls of the last reply that have no result in `messages`,
/// with that reply, when only tool results follow it (aborted replies
/// aside): what a turn stopped during its tool calls still owes.
fn pending_tool_calls(
    messages: &[ConversationMessage],
) -> Option<(&ConversationMessage, Vec<&ChatToolCall>)> {
    let aborted = messages
        .iter()
        .rev()
        .take_while(|message| message.aborted)
        .count();
    let messages = &messages[..messages.len() - aborted];
    let results = messages
        .iter()
        .rev()
        .take_while(|message| message.role == MessageRole::Tool)
        .count();
    let reply_index = messages.len().checked_sub(results + 1)?;
    let (reply, results) = (&messages[reply_index], &messages[reply_index + 1..]);
    if reply.role != MessageRole::Assistant {
        return None;
    }
    let pending = reply
        .tool_calls
        .iter()
        .filter(|call| {
            !results
                .iter()
                .any(|result| result.tool_call_id.as_deref() == Some(call.id.as_str()))
        })
        .collect::<Vec<_>>();
    (!pending.is_empty()).then_some((reply, pending))
}

pub fn default_active_tool_names() -> BTreeSet<String> {
    builtin_tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

fn active_tools_with(disabled_tools: &BTreeSet<String>) -> BTreeSet<String> {
    default_active_tool_names()
        .difference(disabled_tools)
        .cloned()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub labels: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub branch_summary: Option<String>,
    pub modified: Option<SystemTime>,
    /// Number of conversation messages (user/assistant/tool) in the journal,
    /// shown in the resume picker like Kimi/Claude's message counts.
    pub message_count: usize,
    /// First user prompt (truncated) — the picker's one-line topic preview.
    pub first_prompt: Option<String>,
    /// Last active model, shown like Claude's model badge per row.
    pub active_model: Option<String>,
    /// A turn was mid-flight when the session ended: the picker marks it so
    /// resuming offers the one-shot continuation.
    pub has_unfinished_turn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExport {
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub labels: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub tool_history: Vec<ToolEvent>,
    pub queued_messages: Vec<String>,
    #[serde(default)]
    pub compactions: Vec<CompactionRecord>,
    #[serde(default)]
    pub branch_summaries: Vec<BranchSummary>,
    pub active_model: Option<ModelRef>,
    #[serde(default)]
    pub active_thinking_level: Option<String>,
    #[serde(default)]
    pub active_account: Option<String>,
    #[serde(default)]
    pub disabled_tools: BTreeSet<String>,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub edited_files: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub codemode_store: BTreeMap<String, Value>,
    #[serde(default)]
    pub usage: SessionUsage,
}

impl From<&SessionState> for SessionExport {
    fn from(state: &SessionState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
            name: state.name.clone(),
            labels: state.labels.clone(),
            parent_session_id: state.parent_session_id.clone(),
            messages: state.messages.clone(),
            tool_history: state.tool_history.clone(),
            queued_messages: state.queued_messages(),
            compactions: state.compactions.clone(),
            branch_summaries: state.branch_summaries.clone(),
            active_model: state.active_model.clone(),
            active_thinking_level: state.active_thinking_level.clone(),
            active_account: state.active_account.clone(),
            disabled_tools: state.disabled_tools.clone(),
            todos: state.todos.clone(),
            edited_files: state.edited_files.clone(),
            codemode_store: state.codemode_store.clone(),
            usage: state.usage.clone(),
        }
    }
}

fn session_state_from_export(export: SessionExport, session_id: String) -> SessionState {
    let active_tool_names = active_tools_with(&export.disabled_tools);
    SessionState {
        session_id,
        cwd: export.cwd,
        name: export.name,
        labels: export.labels,
        parent_session_id: export.parent_session_id,
        messages: export.messages,
        tool_history: export.tool_history,
        // An imported session has no run for steering to join.
        queue: export
            .queued_messages
            .into_iter()
            .map(QueuedEntry::follow_up)
            .collect(),
        past_submissions: Vec::new(),
        compactions: export.compactions,
        branch_summaries: export.branch_summaries,
        active_model: export.active_model,
        active_thinking_level: export.active_thinking_level,
        active_account: export.active_account,
        // An export does not carry them.
        compaction: None,
        active_tool_names,
        disabled_tools: export.disabled_tools,
        todos: export.todos,
        edited_files: export.edited_files,
        codemode_store: export.codemode_store,
        usage: export.usage,
        // Runs belong to the session that ran them.
        runs: Vec::new(),
        partial: None,
        tool_outputs: BTreeMap::new(),
        // An export carries no workspace: an imported session is local.
        workspace: None,
        snapshots: Vec::new(),
        goal: None,
        plan_mode: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub kind: CompactionKind,
    pub omitted_messages: usize,
    pub retained_messages: usize,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    Manual,
    Automatic,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSummary {
    pub from_session_id: String,
    pub to_session_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadableSystems {
    pub config_generation: u64,
    /// Replaces the default prompt's role, tool list, and rules.
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub context_files: Vec<ContextFile>,
    /// Those of `context_files` from noel's agent dir, the user's own: a
    /// session in a remote workspace keeps them and reads the project's
    /// from the workspace.
    pub agent_context_files: Vec<ContextFile>,
    /// The loaded skills, which the system prompt lists.
    pub skills: Vec<Skill>,
    /// The subagent definitions, which the `task` tool offers.
    pub agents: Vec<AgentDefinition>,
    pub available_models: Vec<ModelRef>,
    pub configured_providers: BTreeSet<String>,
    pub available_accounts: BTreeMap<String, Vec<String>>,
    pub available_tool_names: BTreeSet<String>,
    pub tool_scope_configured: bool,
    /// The settings' `enabledTools` and `excludedTools`, which choose the
    /// tools plugins add as they choose extension tools.
    pub tool_selection: (Option<Vec<String>>, Vec<String>),
    pub extension_tools: BTreeMap<String, ExtensionTool>,
    /// The loaded extension files, for the v2 persistent hosts: which
    /// extensions exist this generation and where their programs are.
    pub extension_files: Vec<noel_config::ResourceFile>,
    pub keybinding_generation: u64,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    /// The `images.autoResize` and `images.blockImages` settings, for images
    /// the `read` tool returns.
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub retry: RuntimeRetrySettings,
    pub offline: bool,
    pub compaction: RuntimeCompactionSettings,
    /// How codemode presents the tools (`codemode` settings).
    pub codemode: codemode::Presentation,
    /// How often a run saves what it has produced so far (spec §4.2).
    pub progress: RuntimeProgressSettings,
    /// User hooks around tool calls (spec §9.5).
    pub hooks: HooksSettings,
    /// MCP servers from the config (spec §9.7).
    pub mcp_servers: Vec<noel_config::mcp::McpServer>,
    /// The agent directory, where `mcp-auth.json` lives (spec §9.7).
    pub mcp_agent_dir: Option<std::path::PathBuf>,
    /// The MCP tools the last run's connections offer, as the run's
    /// tool surface holds them.
    pub mcp_tools: Vec<McpTool>,
    pub model_context_windows: BTreeMap<String, u64>,
    /// Safety cap on provider rounds per run, including steering. Manual
    /// `Default` keeps the compiled cap; a derived zero would stop every
    /// turn after zero rounds.
    pub max_tool_rounds: usize,
    /// Crash restarts in a row after which a run ends `restart_limit`
    /// (Durable's `settings.restartLimit`, default 5).
    pub restart_limit: u32,
}

impl Default for ReloadableSystems {
    fn default() -> Self {
        Self {
            config_generation: 0,
            system_prompt: None,
            append_system_prompt: Vec::new(),
            context_files: Vec::new(),
            agent_context_files: Vec::new(),
            skills: Vec::new(),
            agents: Vec::new(),
            available_models: Vec::new(),
            configured_providers: BTreeSet::new(),
            available_accounts: BTreeMap::new(),
            available_tool_names: BTreeSet::new(),
            tool_scope_configured: false,
            tool_selection: (None, Vec::new()),
            extension_tools: BTreeMap::new(),
            extension_files: Vec::new(),
            keybinding_generation: 0,
            shell_path: None,
            shell_command_prefix: None,
            auto_resize_images: true,
            block_images: false,
            retry: RuntimeRetrySettings::default(),
            offline: false,
            compaction: RuntimeCompactionSettings::default(),
            codemode: codemode::Presentation::default(),
            progress: RuntimeProgressSettings::default(),
            hooks: HooksSettings::default(),
            mcp_servers: Vec::new(),
            mcp_agent_dir: None,
            mcp_tools: Vec::new(),
            model_context_windows: BTreeMap::new(),
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
            restart_limit: DEFAULT_RESTART_LIMIT,
        }
    }
}

/// The tools a run may use, from the tool settings and the command-line
/// flags folded into them (upstream `createAgentSession`):
///
/// - `enabledTools` (`--tools` with plain names) is an allowlist of names
///   and `*` patterns over every built-in and extension tool.
/// - Otherwise the built-in tools are those `defaultTools` selects (all of
///   them when it is unset), and every extension tool is available.
/// - `--exclude-tools` then removes the tools it names or matches.
fn available_tool_names(
    settings: &noel_config::Settings,
    extension_tools: &BTreeMap<String, ExtensionTool>,
) -> BTreeSet<String> {
    let builtin = builtin_tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    // Built-in tools that are off unless selected: codemode, as upstream
    // registers it inactive.
    let selectable = builtin
        .iter()
        .cloned()
        .chain([codemode::CODEMODE.to_owned()])
        .collect::<Vec<_>>();
    let selected: BTreeSet<String> = match (&settings.enabled_tools, &settings.default_tools) {
        (Some(allowed), _) => selectable
            .iter()
            .chain(extension_tools.keys())
            .filter(|name| {
                // Off unless selected by name: a pattern never turns it on.
                if name.as_str() == codemode::CODEMODE {
                    allowed.iter().any(|entry| entry == codemode::CODEMODE)
                } else {
                    noel_config::tool_name_matches(allowed, name)
                }
            })
            .cloned()
            .collect(),
        (None, Some(entries)) => noel_config::resolve_default_tools(entries, &builtin)
            .into_iter()
            .filter(|name| selectable.contains(name))
            .chain(extension_tools.keys().cloned())
            .collect(),
        (None, None) => builtin
            .into_iter()
            .chain(extension_tools.keys().cloned())
            .collect(),
    };
    selected
        .into_iter()
        .filter(|name| !noel_config::tool_name_matches(&settings.excluded_tools, name))
        .collect()
}

fn image_setting(
    config: &LoadedConfig,
    setting: impl Fn(&noel_config::ImageSettings) -> Option<bool>,
    default: bool,
) -> bool {
    config
        .settings
        .images
        .as_ref()
        .and_then(setting)
        .unwrap_or(default)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionToolProtocol {
    /// A v1 manifest tool: a one-shot process per call, JSON on stdin.
    Json,
    /// A tool a v2 host registered; its calls go to the persistent host.
    V2,
    /// A tool a server plugin contributes.
    Plugin,
}

impl ExtensionToolProtocol {
    /// The name extensions are told (`NOEL_EXTENSION_PROTOCOL`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::V2 => "2",
            Self::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub extension_name: String,
    pub extension_path: PathBuf,
    pub protocol: ExtensionToolProtocol,
    /// The manifest's `replaySafe`: a call cut off by a stop may run again
    /// when the turn continues.
    pub replay_safe: bool,
    /// The manifest's `promptSnippet` as one line: the tool's entry in the
    /// system prompt's tool list. A tool without one is not listed there.
    pub prompt_snippet: Option<String>,
    /// The manifest's `promptGuidelines`: the tool's system prompt rules.
    pub prompt_guidelines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRetrySettings {
    pub enabled: bool,
    pub max_retries: u64,
    pub base_delay_ms: u64,
    pub max_agent_delay_ms: u64,
}

impl Default for RuntimeRetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
            max_agent_delay_ms: noel_agent::DEFAULT_MAX_RETRY_DELAY_MS,
        }
    }
}

impl ReloadableSystems {
    pub fn from_config(config: &LoadedConfig, generation: u64) -> Self {
        let available_models = config
            .models
            .iter()
            .map(|model| ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            })
            .collect();
        let configured_providers = config
            .models
            .iter()
            .filter(|model| {
                model.provider == "faux"
                    || has_auth_for_provider(&config.auth, &model.provider, None)
            })
            .map(|model| model.provider.clone())
            .collect();
        let available_accounts = config
            .models
            .iter()
            .map(|model| model.provider.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|provider| {
                let accounts = listed_accounts_for_provider(&config.auth, &provider)
                    .into_iter()
                    .map(|(account, _)| account)
                    .collect();
                (provider, accounts)
            })
            .collect();
        let extension_tools = extension_tools_from_resources(&config.extensions);
        let available_tool_names = available_tool_names(&config.settings, &extension_tools);
        Self {
            config_generation: generation,
            system_prompt: config.system_prompt.clone(),
            append_system_prompt: config.append_system_prompt.clone(),
            context_files: config.context_files.clone(),
            agent_context_files: config
                .context_files
                .iter()
                .filter(|file| file.path.parent() == Some(config.paths.agent_dir.as_path()))
                .cloned()
                .collect(),
            skills: config.skills.clone(),
            agents: config.agents.clone(),
            available_models,
            configured_providers,
            model_context_windows: config
                .models
                .iter()
                .map(|model| {
                    (
                        format!("{}/{}", model.provider, model.id),
                        noel_config::model_context_window(model),
                    )
                })
                .collect(),
            codemode: codemode::Presentation::from_settings(config.settings.codemode.as_ref()),
            compaction: {
                let settings = config.settings.compaction.clone().unwrap_or_default();
                RuntimeCompactionSettings {
                    enabled: settings.enabled.unwrap_or(true),
                    reserve_tokens: settings.reserve_tokens.unwrap_or(16_384),
                    keep_recent_tokens: settings.keep_recent_tokens.unwrap_or(20_000),
                    trigger_percent: settings.trigger_percent.unwrap_or(80).clamp(1, 99),
                    model_overrides: settings.model_overrides.clone().unwrap_or_default(),
                }
            },
            hooks: config.settings.hooks.clone().unwrap_or_default(),
            mcp_servers: config.mcp_servers.clone(),
            mcp_agent_dir: Some(config.paths.agent_dir.clone()),
            mcp_tools: Vec::new(),
            progress: {
                let settings = config.settings.progress.clone().unwrap_or_default();
                let interval = |ms: Option<u64>| {
                    ms.map_or(progress::DEFAULT_PROGRESS_INTERVAL, Duration::from_millis)
                };
                RuntimeProgressSettings {
                    partial_interval: interval(settings.partial_interval_ms),
                    output_interval: interval(settings.output_interval_ms),
                }
            },
            available_accounts,
            available_tool_names,
            tool_scope_configured: true,
            tool_selection: (
                config.settings.enabled_tools.clone(),
                config.settings.excluded_tools.clone(),
            ),
            extension_tools,
            extension_files: config.extensions.clone(),
            keybinding_generation: generation,
            shell_path: config.settings.shell_path.clone(),
            shell_command_prefix: config.settings.shell_command_prefix.clone(),
            auto_resize_images: image_setting(config, |images| images.auto_resize, true),
            block_images: image_setting(config, |images| images.block_images, false),
            offline: noel_config::env::offline(),
            retry: RuntimeRetrySettings {
                enabled: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.enabled)
                    .unwrap_or(true),
                max_retries: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.max_retries)
                    .unwrap_or(3),
                base_delay_ms: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.base_delay_ms)
                    .unwrap_or(2_000),
                max_agent_delay_ms: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.max_agent_delay_ms)
                    .unwrap_or(noel_agent::DEFAULT_MAX_RETRY_DELAY_MS),
            },
            max_tool_rounds: config
                .settings
                .agent
                .as_ref()
                .and_then(|agent| agent.max_tool_rounds)
                .unwrap_or(DEFAULT_MAX_TOOL_ROUNDS)
                .max(1),
            restart_limit: config
                .settings
                .restart_limit
                .unwrap_or(DEFAULT_RESTART_LIMIT),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    pub active_model_valid: bool,
    pub active_account_valid: bool,
    pub removed_active_tools: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReloadError {
    #[error("reloadable systems are invalid: {0}")]
    Invalid(String),
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Config(#[from] noel_config::ConfigError),
    #[error(transparent)]
    Journal(#[from] noel_session::JournalError),
    #[error("failed to create session directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open session {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read session {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write session {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse session line in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Another process holds the session for writing; `pid` names the holder
    /// when its lock file says so.
    #[error("session {path} is in use by another noel process{}", lock_holder(.pid))]
    Locked { path: PathBuf, pid: Option<u32> },
    #[error("failed to lock session {path}: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The session's workspace could not be opened for an operation on
    /// its files outside a tool call (a rewind).
    #[error("the workspace is not available: {0}")]
    Workspace(String),
}

fn lock_holder(pid: &Option<u32>) -> String {
    pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("compaction failed: {0}")]
    Compaction(String),
    /// The host died while the model was answering, and the retry policy
    /// allows no other attempt (Durable's `interrupted` run failure).
    #[error("{0}")]
    Interrupted(String),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("missing argument for slash command: {0}")]
    MissingCommandArgument(String),
    #[error("tool is disabled: {0}")]
    DisabledTool(String),
    #[error("invalid model tool call for {tool}: {message}")]
    InvalidToolCall { tool: String, message: String },
    #[error("turn reached the {max_rounds} provider-round limit (including steering); work is saved; send a prompt to continue, or set agent.maxToolRounds in settings.json to raise the limit")]
    RoundLimitReached { max_rounds: usize },
    #[error("turn cancelled")]
    Cancelled,
}

impl From<noel_agent::AgentError> for AgentError {
    fn from(error: noel_agent::AgentError) -> Self {
        match error {
            noel_agent::AgentError::Provider(error) => Self::Provider(error),
            noel_agent::AgentError::Cancelled => Self::Cancelled,
            other => Self::Provider(ProviderError::Config(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum SessionRecord {
    Started {
        session_id: String,
        cwd: PathBuf,
    },
    /// Where the session's tools run (spec §10), written with `started` in
    /// one atomic file, so a session that has one never existed without
    /// it. Older versions ignore it, and run such a session's tools here.
    Workspace(SessionWorkspace),
    Metadata {
        name: Option<String>,
        labels: Vec<String>,
        parent_session_id: Option<String>,
    },
    Message {
        message: ConversationMessage,
        /// Set on the prompt that starts a run: one record, so a run is
        /// never saved without its prompt. Older versions ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<RunStart>,
        /// When it was saved, in milliseconds since the epoch: the history
        /// entry's timestamp. Older versions ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<u64>,
        /// The queued submissions this message places into the run, which
        /// leave the queue with it: one record, so a submission is never
        /// both placed and still queued. Older versions ignore it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        placed: Vec<QueueItemId>,
    },
    MessagesSnapshot {
        messages: Vec<ConversationMessage>,
    },
    Tool {
        event: ToolEvent,
    },
    ActiveModel {
        model: Option<ModelRef>,
    },
    ActiveAccount {
        account: Option<String>,
    },
    ActiveThinkingLevel {
        level: Option<String>,
    },
    /// The conversation's compaction settings, replaced whole; `null`
    /// clears them.
    CompactionSettings {
        settings: Option<noel_config::CompactionSettings>,
    },
    DisabledTools {
        tools: Vec<String>,
    },
    Todos {
        todos: Vec<TodoItem>,
    },
    EditedFiles {
        paths: Vec<String>,
    },
    /// The codemode store as it is after a script wrote to it. Older
    /// versions ignore it.
    CodemodeStore {
        store: BTreeMap<String, Value>,
    },
    Usage {
        usage: SessionUsage,
    },
    QueuedMessage {
        message: String,
    },
    /// The whole queue. The first `steering` messages are steering; older
    /// versions read them all as follow-ups, as the TUI does.
    QueuedMessagesSnapshot {
        messages: Vec<String>,
        #[serde(default, skip_serializing_if = "is_zero")]
        steering: usize,
        /// Each message's identity, when any has a `requestId` (see
        /// [`QueueItemId`]); older versions ignore it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<Option<QueueItemId>>,
        /// The saved submissions this record withdraws from the queue
        /// unanswered; older versions ignore it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        withdrawn: Vec<QueueItemId>,
    },
    Compaction {
        record: CompactionRecord,
    },
    CompactionCheckpoint {
        record: CompactionRecord,
        messages: Vec<ConversationMessage>,
    },
    BranchSummary {
        summary: BranchSummary,
    },
    /// How a run ended: completed, failed, or aborted. A run that started
    /// without one was interrupted.
    RunFinished {
        run_id: String,
        status: RunStatus,
        /// A failed run's reason; older versions leave it out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<RunFailure>,
    },
    /// A run that saves no message to start it: a continue.
    RunStarted {
        run: RunStart,
    },
    /// A close stopped the run, which a later resume continues without
    /// counting a restart.
    RunClosed {
        run_id: String,
    },
    /// The run resumed after a stop that was not a close (a crash).
    RunRestarted {
        run_id: String,
    },
    /// The run resumed after a close: no restart, and a later stop is no
    /// longer that close.
    RunReopened {
        run_id: String,
    },
    /// A reply's request failed and the turn waits out a retry's backoff
    /// before attempt `attempt + 1`, or (`attempt` 0) compacts after an
    /// overflow: a crash now cut off no request.
    RetryWait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        attempt: u64,
    },
    /// What a streaming reply has produced so far (spec §4.2), saved at
    /// most every `progress.partialIntervalMs`. A reply's first record
    /// holds its text and thinking so far, and supersedes any earlier
    /// partial; each later one (`append`) holds what streamed since. The
    /// reply, or any message after it, supersedes them. Older versions
    /// ignore it.
    Partial {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        attempt: u64,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        thinking: String,
        #[serde(default, skip_serializing_if = "is_false")]
        append: bool,
    },
    /// What a running tool call has printed so far, within the limits of
    /// its result, saved at most every `progress.outputIntervalMs`. A
    /// record supersedes the call's earlier ones, or with `append` extends
    /// them; the call's result supersedes them all. Older versions ignore
    /// it.
    ToolOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        call_id: String,
        output: String,
        /// Output before `output` was left out.
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        append: bool,
    },
    /// Plan mode entered or left (spec §9.4).
    PlanMode {
        on: bool,
    },
    /// The session goal set (spec §9.3); replaces any earlier one.
    Goal {
        goal: SessionGoal,
    },
    /// The session goal cleared: by the evaluator (met), by the turn cap,
    /// or by `/goal clear`.
    GoalCleared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A file snapshot saved before an `edit`/`write` changed it: the
    /// snapshot's id and where its bytes are. Rewind restores by id.
    /// Older versions ignore it.
    FileSnapshot {
        id: String,
        /// The file as the session saw it, relative or absolute as the
        /// tool wrote it.
        path: String,
        /// SHA-256 of the snapshotted bytes, to notice a file that changed
        /// since (a snapshot restores only what it saved).
        before_sha: String,
        /// Whether the file did not exist (a snapshot of nothing).
        #[serde(default, skip_serializing_if = "is_false")]
        created: bool,
    },
    /// What the file of snapshot `id` held after its edit ran, for the
    /// check a rewind makes before it writes a file back. Older versions
    /// ignore it.
    FileEdited {
        id: String,
        after_sha: String,
    },
    /// The conversation rewound: everything after `messages` (user and
    /// assistant) is dropped, and the files named by `restore` — per
    /// path, the oldest snapshot the dropped turns took, so what the file
    /// held before them — were written back where the file still held
    /// what the session left. The messages snapshot before this record is
    /// what the history shows; this record settles the snapshots (see
    /// [`SessionState::rewind_to`]). The journal keeps everything; this is
    /// the state that loads. Older versions ignore it.
    Rewound {
        messages: usize,
        /// The `FileSnapshot.id`s written back, one per path; none when
        /// the files were left as they are.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restore: Vec<String>,
    },
    /// Records written by other binary versions are ignored on load.
    #[serde(other)]
    Unknown,
}

/// The SHA-256 of `bytes` as lowercase hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Progress a run committed to the journal (spec §4.2), as a
/// [`ProgressObserver`] hears it: only what is saved, in the order it is
/// saved, so whoever shows it shows what a crash would keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedProgress {
    /// What a streaming reply produced. A record without `append` replaces
    /// what was saved of the reply before (an empty one drops it); with
    /// `append` it extends attempt `attempt`'s.
    Partial {
        run_id: Option<String>,
        attempt: u64,
        text: String,
        thinking: String,
        append: bool,
    },
    /// What a running tool call printed: with `append` more of it, else all
    /// of it that is kept (`truncated`: its start was left out).
    ToolOutput {
        run_id: Option<String>,
        call_id: String,
        output: String,
        truncated: bool,
        append: bool,
    },
    /// A record that adds to the session's history (see [`history`]).
    History(history::HistoryRecord),
}

/// What `record` adds to the session's history, if anything.
fn history_record(record: &SessionRecord) -> Option<history::HistoryRecord> {
    Some(match record {
        SessionRecord::Message {
            message,
            ts,
            placed,
            ..
        } => history::HistoryRecord::Message {
            message: message.clone(),
            timestamp: *ts,
            queued: placed.first().map(|id| match id.mode {
                QueueMode::Steer => "steer",
                QueueMode::FollowUp => "followUp",
            }),
        },
        SessionRecord::MessagesSnapshot { messages } => history::HistoryRecord::Reset {
            messages: messages.len(),
        },
        SessionRecord::CompactionCheckpoint { record, .. }
        | SessionRecord::Compaction { record } => {
            history::HistoryRecord::Compaction(record.clone())
        }
        SessionRecord::Tool { event } => history::HistoryRecord::ToolEvent {
            call_id: event.id.clone(),
            is_error: event.is_error,
        },
        _ => return None,
    })
}

/// Hears each [`SavedProgress`] once it is in the journal, on the task that
/// saved it.
pub type ProgressObserver = Arc<dyn Fn(&SavedProgress) + Send + Sync>;

#[derive(Clone)]
struct ObserverSlot(ProgressObserver);

impl std::fmt::Debug for ObserverSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProgressObserver")
    }
}

/// A session journal opened for writing, over a [`storage::JournalStorage`]:
/// the journal file by default, whose constructors take the session lock
/// (see [`SessionError::Locked`]) and repair a torn tail before the first
/// append; clones share the storage and serialize their writes. Read-only
/// access goes through [`SessionStore::read`], which never locks or repairs.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    storage: storage::Storage,
    /// Hears the progress this store and its clones save.
    progress: Option<ObserverSlot>,
    /// What opening the journal found about how its last run stopped.
    hint: Arc<Mutex<ResumeHint>>,
}

/// How the journal's last run stopped, as opening it found: for a resume
/// to charge a model request a crash cut off (Durable section 8.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResumeHint {
    /// Nothing says how it stopped (or a resume already took the hint).
    #[default]
    Unknown,
    /// A request was in flight: the attempt of the reply a stop cut off,
    /// recovered as an aborted reply.
    InRequest(u64),
    /// The run was waiting out a retry's backoff or a compaction (its last
    /// record is a `retry_wait`): no request was in flight.
    InBackoff,
}

fn lock_hint(hint: &Mutex<ResumeHint>) -> std::sync::MutexGuard<'_, ResumeHint> {
    hint.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SessionStore {
    pub fn create(session_dir: &Path, cwd: PathBuf) -> Result<(Self, SessionState), SessionError> {
        Self::create_with_id(session_dir, cwd, new_session_id())
    }

    pub fn create_with_id(
        session_dir: &Path,
        cwd: PathBuf,
        session_id: impl Into<String>,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = session_id.into();
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self::writer(path)?;
        let state = SessionState::new(session_id.clone(), cwd.clone());
        store.append(&SessionRecord::Started { session_id, cwd })?;
        Ok((store, state))
    }

    /// A new session, local ([`SessionStore::create`]) or in `workspace`
    /// ([`SessionStore::create_in_workspace`]).
    pub fn create_with_workspace(
        session_dir: &Path,
        cwd: PathBuf,
        workspace: Option<SessionWorkspace>,
    ) -> Result<(Self, SessionState), SessionError> {
        match workspace {
            Some(workspace) => {
                Self::create_in_workspace(session_dir, cwd, new_session_id(), workspace)
            }
            None => Self::create(session_dir, cwd),
        }
    }

    /// [`SessionStore::create_with_id`] for a session whose tools run in
    /// `workspace`. The journal starts with its `started` and `workspace`
    /// records in one atomic write, so it never exists without the second:
    /// a session must never fall back to running its tools here.
    pub fn create_in_workspace(
        session_dir: &Path,
        cwd: PathBuf,
        session_id: impl Into<String>,
        workspace: SessionWorkspace,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = session_id.into();
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self::writer(path)?;
        let mut state = SessionState::new(session_id, cwd);
        state.workspace = Some(workspace);
        store.write_full_state(&state)?;
        Ok((store, state))
    }

    /// A new session that starts as `state` (its model, thinking level,
    /// account, workspace), written in one atomic write, so it never
    /// exists half set up (#175).
    pub fn create_with_state(
        session_dir: &Path,
        state: SessionState,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let path = session_dir.join(format!("{}.jsonl", state.session_id));
        let store = Self::writer(path)?;
        store.write_full_state(&state)?;
        Ok((store, state))
    }

    pub fn fork(
        session_dir: &Path,
        source: &SessionState,
        clone_parent: bool,
    ) -> Result<(Self, SessionState), SessionError> {
        Self::fork_with_id(session_dir, source, clone_parent, new_session_id())
    }

    /// [`SessionStore::fork`] into a session named `session_id`.
    pub fn fork_with_id(
        session_dir: &Path,
        source: &SessionState,
        clone_parent: bool,
        session_id: impl Into<String>,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = session_id.into();
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self::writer(path)?;
        let mut state = source.clone();
        state.session_id = session_id.clone();
        // A fork is a new session: the source's runs, and what they left
        // unfinished, stay with the source.
        state.runs.clear();
        state.partial = None;
        state.tool_outputs.clear();
        state.parent_session_id = if clone_parent {
            source.parent_session_id.clone()
        } else {
            Some(source.session_id.clone())
        };
        state.branch_summaries.push(BranchSummary {
            from_session_id: source.session_id.clone(),
            to_session_id: session_id,
            summary: summarize_branch(source),
        });
        store.write_full_state(&state)?;
        Ok((store, state))
    }

    pub fn import(
        session_dir: &Path,
        export: SessionExport,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = if export.session_id.trim().is_empty() {
            new_session_id()
        } else {
            export.session_id.clone()
        };
        let path = session_dir.join(format!("{session_id}.jsonl"));
        // Importing over an existing id rewrites that journal, so it takes the
        // lock first and refuses while another process holds the session.
        let store = Self::writer(path)?;
        let state = session_state_from_export(export, session_id);
        store.write_full_state(&state)?;
        Ok((store, state))
    }

    /// Opens an existing session for writing: takes the session lock, repairs
    /// a torn tail, then loads it. A reply a stop cut off is then saved as
    /// an aborted reply ([`SessionStore::recover_partial`]).
    pub fn open(path: PathBuf) -> Result<(Self, SessionState), SessionError> {
        fs::metadata(&path).map_err(|source| SessionError::Open {
            path: path.clone(),
            source,
        })?;
        let store = Self::writer(path)?;
        let mut state = store.load()?;
        let waiting = waiting_retry(&store.path);
        let attempt = store.recover_partial(&mut state)?;
        *lock_hint(&store.hint) = match (waiting, attempt) {
            (true, _) => ResumeHint::InBackoff,
            (false, Some(attempt)) => ResumeHint::InRequest(attempt),
            (false, None) => ResumeHint::Unknown,
        };
        Ok((store, state))
    }

    /// Recovery of a reply a stop cut off (spec §4.2): what the journal
    /// saved of it becomes an aborted reply in the transcript, which later
    /// requests leave out, and supersedes the partial. A reply that saved
    /// nothing leaves nothing.
    /// Returns the attempt of the reply recovered, if one was cut off.
    pub fn recover_partial(&self, state: &mut SessionState) -> Result<Option<u64>, SessionError> {
        let Some(partial) = state.partial.take() else {
            return Ok(None);
        };
        let attempt = partial.attempt;
        if let Some(message) = partial.aborted_message() {
            self.record_message(message.clone())?;
            state.messages.push(message);
        }
        Ok(Some(attempt))
    }

    /// Loads a session without locking or repairing it, for listing, pickers,
    /// fork and clone sources, and import sources. A torn final segment left
    /// by an interrupted append is ignored; a bad line before it is an error.
    pub fn read(path: &Path) -> Result<SessionState, SessionError> {
        session_state_from_bytes(path, &read_journal_bytes(path)?)
    }

    /// Deletes a session journal. Refuses with [`SessionError::Locked`] while
    /// another process holds the session; this process may delete a session
    /// it holds itself, whose lock file goes away with its last handle.
    pub fn delete(path: &Path) -> Result<(), SessionError> {
        let lock = session_lock::acquire(path)?;
        let _writer = lock.writer();
        fs::remove_file(path).map_err(|source| SessionError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    fn writer(path: PathBuf) -> Result<Self, SessionError> {
        Ok(Self::on(Arc::new(storage::JsonlFile::open(path)?)))
    }

    fn on(storage: storage::Storage) -> Self {
        Self {
            path: storage.path().to_path_buf(),
            storage,
            progress: None,
            hint: Arc::default(),
        }
    }

    /// How the journal's last run stopped, as opening it found. Taken
    /// once: the resume it informs is the only one it describes, so a
    /// later call (and every clone of this store) finds `Unknown`.
    pub fn take_resume_hint(&self) -> ResumeHint {
        std::mem::take(&mut *lock_hint(&self.hint))
    }

    /// Opens the session a storage holds (another backend than the journal
    /// file): its state, with a reply a stop cut off recovered as for
    /// [`SessionStore::open`].
    pub fn open_storage(storage: storage::Storage) -> Result<(Self, SessionState), SessionError> {
        let store = Self::on(storage);
        let mut state = store.load()?;
        if let Some(attempt) = store.recover_partial(&mut state)? {
            *lock_hint(&store.hint) = ResumeHint::InRequest(attempt);
        }
        Ok((store, state))
    }

    /// Starts a new session in an empty storage.
    pub fn create_in_storage(
        storage: storage::Storage,
        cwd: PathBuf,
        session_id: impl Into<String>,
    ) -> Result<(Self, SessionState), SessionError> {
        let store = Self::on(storage);
        let session_id = session_id.into();
        let state = SessionState::new(session_id.clone(), cwd.clone());
        store.append(&SessionRecord::Started { session_id, cwd })?;
        Ok((store, state))
    }

    #[cfg(test)]
    fn lock(&self) -> &Arc<SessionLock> {
        self.storage
            .as_any()
            .downcast_ref::<storage::JsonlFile>()
            .expect("a journal file")
            .lock()
    }

    /// This store, and the clones made of it from now on, tell `observer`
    /// about each partial and tool output they save, once it is saved.
    /// Mode 2's worker shows them to clients (spec §7.3).
    pub fn observe_progress(mut self, observer: ProgressObserver) -> Self {
        self.progress = Some(ObserverSlot(observer));
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_message(&self, message: ConversationMessage) -> Result<(), SessionError> {
        self.record_placed_message(message, Vec::new())
    }

    /// Saves `message` with the queued submissions it places into the run;
    /// only those the journal keeps (see [`QueueItemId`]) are named.
    pub fn record_placed_message(
        &self,
        message: ConversationMessage,
        placed: Vec<QueueItemId>,
    ) -> Result<(), SessionError> {
        let placed = placed.into_iter().filter(QueueItemId::is_durable).collect();
        self.append(&SessionRecord::Message {
            message,
            run: None,
            ts: Some(history::now_ms()),
            placed,
        })
    }

    pub fn record_messages_snapshot(
        &self,
        messages: Vec<ConversationMessage>,
    ) -> Result<(), SessionError> {
        self.append(&SessionRecord::MessagesSnapshot { messages })
    }

    pub fn record_tool(&self, event: ToolEvent) -> Result<(), SessionError> {
        self.append(&SessionRecord::Tool { event })
    }

    fn record_progress(&self, record: &SessionRecord) -> Result<(), SessionError> {
        self.append(record)?;
        let Some(ObserverSlot(observer)) = &self.progress else {
            return Ok(());
        };
        let saved = match record {
            SessionRecord::Partial {
                run_id,
                attempt,
                text,
                thinking,
                append,
            } => SavedProgress::Partial {
                run_id: run_id.clone(),
                attempt: *attempt,
                text: text.clone(),
                thinking: thinking.clone(),
                append: *append,
            },
            SessionRecord::ToolOutput {
                run_id,
                call_id,
                output,
                truncated,
                append,
            } => SavedProgress::ToolOutput {
                run_id: run_id.clone(),
                call_id: call_id.clone(),
                output: output.clone(),
                truncated: *truncated,
                append: *append,
            },
            _ => return Ok(()),
        };
        observer(&saved);
        Ok(())
    }

    pub fn record_active_model(&self, model: Option<ModelRef>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveModel { model })
    }

    pub fn record_active_account(&self, account: Option<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveAccount { account })
    }

    pub fn record_compaction_settings(
        &self,
        settings: Option<noel_config::CompactionSettings>,
    ) -> Result<(), SessionError> {
        self.append(&SessionRecord::CompactionSettings { settings })
    }

    pub fn record_active_thinking_level(&self, level: Option<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveThinkingLevel { level })
    }

    pub fn record_disabled_tools(&self, tools: Vec<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::DisabledTools { tools })
    }

    pub fn record_todos(&self, todos: Vec<TodoItem>) -> Result<(), SessionError> {
        self.append(&SessionRecord::Todos { todos })
    }

    pub fn record_codemode_store(
        &self,
        store: BTreeMap<String, Value>,
    ) -> Result<(), SessionError> {
        self.append(&SessionRecord::CodemodeStore { store })
    }

    pub fn record_edited_files(&self, paths: Vec<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::EditedFiles { paths })
    }

    pub fn record_goal(&self, goal: SessionGoal) -> Result<(), SessionError> {
        self.append(&SessionRecord::Goal { goal })
    }

    pub fn record_plan_mode(&self, on: bool) -> Result<(), SessionError> {
        self.append(&SessionRecord::PlanMode { on })
    }

    pub fn record_goal_cleared(&self, reason: Option<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::GoalCleared { reason })
    }

    /// Where this session's file snapshots live: `<journal>.files/`, a
    /// directory beside the journal.
    pub fn snapshot_dir(&self) -> PathBuf {
        self.path.with_extension("files")
    }

    /// Saves the file's current bytes as snapshot `id` and journals the
    /// snapshot. `created` says the file did not exist (an empty snapshot,
    /// restoring deletes). Returns its reference for rewind.
    pub fn record_file_snapshot(
        &self,
        id: &str,
        path: &str,
        bytes: Option<Vec<u8>>,
    ) -> Result<SnapshotRef, SessionError> {
        let dir = self.snapshot_dir();
        std::fs::create_dir_all(&dir).map_err(|source| SessionError::Write {
            path: dir.clone(),
            source,
        })?;
        let (bytes, created, before_sha) = match bytes {
            Some(bytes) => {
                let sha = sha256_hex(&bytes);
                (bytes, false, sha)
            }
            None => (Vec::new(), true, String::new()),
        };
        // Write-atomic: a temp file, renamed into place (PiImport's rule).
        let temp = dir.join(format!(".{id}.tmp"));
        std::fs::write(&temp, &bytes)
            .and_then(|_| std::fs::rename(&temp, dir.join(id)))
            .map_err(|source| SessionError::Write {
                path: dir.join(id),
                source,
            })?;
        self.append(&SessionRecord::FileSnapshot {
            id: id.to_string(),
            path: path.to_string(),
            before_sha: before_sha.clone(),
            created,
        })?;
        Ok(SnapshotRef {
            id: id.to_string(),
            path: path.to_string(),
            before_sha,
            created,
            // The caller records with the message count in effect; a load
            // sets it from the journal's position.
            message_index: 0,
            after_sha: None,
        })
    }

    /// Reads snapshot `id`'s bytes (None for a created-file snapshot).
    pub fn read_snapshot(&self, id: &str) -> Result<Option<Vec<u8>>, SessionError> {
        let path = self.snapshot_dir().join(id);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SessionError::Read { path, source }),
        }
    }

    /// Journals what the file of snapshot `id` held after its edit.
    pub fn record_file_edited(&self, id: &str, after_sha: &str) -> Result<(), SessionError> {
        self.append(&SessionRecord::FileEdited {
            id: id.to_string(),
            after_sha: after_sha.to_string(),
        })
    }

    /// Journals a rewind and applies it to `state` (see
    /// [`SessionState::rewind_to`]). The caller restores the files.
    pub fn record_rewind(
        &self,
        state: &mut SessionState,
        messages: usize,
        restore: Vec<String>,
    ) -> Result<(), SessionError> {
        // The history a client is shown follows the messages snapshot, as
        // it does for an undo; the rewind record drops what else belonged
        // to the dropped range.
        let kept = messages.min(state.messages.len());
        self.record_messages_snapshot(state.messages[..kept].to_vec())?;
        self.append(&SessionRecord::Rewound {
            messages,
            restore: restore.clone(),
        })?;
        state.rewind_to(messages, &restore);
        Ok(())
    }

    pub fn record_usage(&self, usage: SessionUsage) -> Result<(), SessionError> {
        self.append(&SessionRecord::Usage { usage })
    }

    pub fn record_metadata(&self, state: &SessionState) -> Result<(), SessionError> {
        self.append(&metadata_record(state))
    }

    pub fn record_queued_message(&self, message: String) -> Result<(), SessionError> {
        self.append(&SessionRecord::QueuedMessage { message })
    }

    /// Saves the whole queue as follow-ups.
    pub fn record_queued_messages_snapshot(
        &self,
        messages: Vec<String>,
    ) -> Result<(), SessionError> {
        self.record_queue(Vec::new(), messages)
    }

    /// Saves the whole queue: steering for a run's next provider round,
    /// then follow-ups.
    pub fn record_queue(
        &self,
        steering: Vec<String>,
        follow_ups: Vec<String>,
    ) -> Result<(), SessionError> {
        self.append(&queue_record(steering, follow_ups))
    }

    /// Saves the whole queue with its items' identities, steering first,
    /// and the submissions it withdraws. Identities are saved only when the
    /// journal keeps them (see [`QueueItemId`]), so a queue without any is
    /// saved as [`SessionStore::record_queue`] saves it.
    pub fn record_queue_items(
        &self,
        items: &[QueueItem],
        withdrawn: &[QueueItemId],
    ) -> Result<(), SessionError> {
        self.append(&queue_items_record(items, withdrawn))
    }

    pub fn record_compaction(&self, record: CompactionRecord) -> Result<(), SessionError> {
        self.append(&SessionRecord::Compaction { record })
    }

    fn record_compaction_checkpoint(
        &self,
        record: CompactionRecord,
        messages: Vec<ConversationMessage>,
    ) -> Result<(), SessionError> {
        // Tool calls whose saved output still stands after the checkpoint.
        let pending: BTreeSet<String> = pending_tool_calls(&messages)
            .map(|(_, calls)| calls.into_iter().map(|call| call.id.clone()).collect())
            .unwrap_or_default();
        let marker = SessionRecord::Compaction {
            record: record.clone(),
        };
        let checkpoint =
            serde_json::to_vec(&SessionRecord::CompactionCheckpoint { record, messages }).map_err(
                |source| SessionError::Parse {
                    path: self.path.clone(),
                    source,
                },
            )?;
        // No append from another handle lands between the read and the
        // replacement, and a failure leaves the old journal and active
        // context unchanged, with no partial checkpoint. A torn final
        // segment is dropped, never patched into a corrupt interior line.
        self.storage.rewrite(&mut |journal| {
            // The rewrite drops what the checkpoint and the rest of the
            // journal supersede of what replies and tool calls saved as they
            // ran.
            let mut journal = progress::drop_superseded(&journal, &pending);
            journal.extend_from_slice(&checkpoint);
            journal.push(b'\n');
            Ok(journal)
        })?;
        self.observe_history(&marker);
        Ok(())
    }

    pub fn record_branch_summary(&self, summary: BranchSummary) -> Result<(), SessionError> {
        self.append(&SessionRecord::BranchSummary { summary })
    }

    pub fn export_state(&self, state: &SessionState, path: &Path) -> Result<(), SessionError> {
        write_session_export(state, path)
    }

    pub fn import_path(
        session_dir: &Path,
        path: &Path,
    ) -> Result<(Self, SessionState), SessionError> {
        let export = if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            // The source is only read: importing never locks or repairs it.
            let content = read_journal_bytes(path)?;
            if jsonl_is_ts_session_export(&content) {
                read_ts_jsonl_export(path, &content)?
            } else {
                SessionExport::from(&read_rust_journal(path, &content)?)
            }
        } else {
            let content = fs::read_to_string(path).map_err(|source| SessionError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            serde_json::from_str::<SessionExport>(&content).map_err(|source| {
                SessionError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?
        };
        Self::import(session_dir, export)
    }

    /// Summarizes every readable session, oldest first. A journal that fails
    /// to load is skipped, so one damaged file never hides the others from
    /// the resume picker, `--continue`, `--resume`, `/tree`, or `resolve`.
    pub fn list(session_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        if !session_dir.exists() {
            return Ok(Vec::new());
        }
        let mut summaries = Vec::new();
        for entry in fs::read_dir(session_dir).map_err(|source| SessionError::Read {
            path: session_dir.to_path_buf(),
            source,
        })? {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            let Ok(state) = Self::read(&path) else {
                continue;
            };
            let branch_summary = state
                .branch_summaries
                .iter()
                .rev()
                .find(|summary| summary.to_session_id == state.session_id)
                .map(|summary| summary.summary.clone());
            let first_prompt = state
                .messages
                .iter()
                .find(|message| message.role == MessageRole::User)
                .map(|message| message.content.clone());
            let active_model = state
                .active_model
                .as_ref()
                .map(|model| format!("{}/{}", model.provider, model.id));
            let has_unfinished_turn = state.has_unfinished_turn();
            summaries.push(SessionSummary {
                path,
                session_id: state.session_id,
                cwd: state.cwd,
                name: state.name,
                labels: state.labels,
                parent_session_id: state.parent_session_id,
                branch_summary,
                modified,
                message_count: state.messages.len(),
                first_prompt,
                active_model,
                has_unfinished_turn,
            });
        }
        summaries.sort_by_key(|summary| summary.modified.unwrap_or(UNIX_EPOCH));
        Ok(summaries)
    }

    pub fn resolve(session_dir: &Path, reference: &str) -> Result<Option<PathBuf>, SessionError> {
        let reference_path = PathBuf::from(reference);
        if reference_path.exists() {
            return Ok(Some(reference_path));
        }
        let candidate = session_dir.join(reference);
        if candidate.exists() {
            return Ok(Some(candidate));
        }
        let jsonl_candidate = session_dir.join(format!("{reference}.jsonl"));
        if jsonl_candidate.exists() {
            return Ok(Some(jsonl_candidate));
        }
        let summaries = Self::list(session_dir)?;
        if let Ok(index) = reference.parse::<usize>()
            && index > 0
            && let Some(summary) = summaries.get(index - 1)
        {
            return Ok(Some(summary.path.clone()));
        }
        let matches = summaries
            .into_iter()
            .filter(|summary| {
                summary.session_id.starts_with(reference)
                    || summary.name.as_deref() == Some(reference)
            })
            .map(|summary| summary.path)
            .collect::<Vec<_>>();
        Ok(if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        })
    }

    fn load(&self) -> Result<SessionState, SessionError> {
        session_state_from_bytes(&self.path, &self.storage.read()?)
    }

    /// Replaces the journal with the full state in one atomic rename, so a
    /// crash leaves either the previous journal or the complete new one.
    fn write_full_state(&self, state: &SessionState) -> Result<(), SessionError> {
        let mut records = vec![SessionRecord::Started {
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
        }];
        records.extend(state.workspace.clone().map(SessionRecord::Workspace));
        records.extend([
            metadata_record(state),
            SessionRecord::ActiveModel {
                model: state.active_model.clone(),
            },
            SessionRecord::ActiveAccount {
                account: state.active_account.clone(),
            },
            SessionRecord::ActiveThinkingLevel {
                level: state.active_thinking_level.clone(),
            },
            SessionRecord::CompactionSettings {
                settings: state.compaction.clone(),
            },
            SessionRecord::DisabledTools {
                tools: state.disabled_tools.iter().cloned().collect(),
            },
            SessionRecord::Todos {
                todos: state.todos.clone(),
            },
            SessionRecord::EditedFiles {
                paths: state.edited_files.clone(),
            },
            SessionRecord::CodemodeStore {
                store: state.codemode_store.clone(),
            },
            SessionRecord::Usage {
                usage: state.usage.clone(),
            },
        ]);
        // Fork and import start new sessions, which have no runs yet.
        records.extend(
            state
                .messages
                .iter()
                .cloned()
                .map(|message| SessionRecord::Message {
                    message,
                    run: None,
                    ts: None,
                    placed: Vec::new(),
                }),
        );
        records.extend(
            state
                .tool_history
                .iter()
                .cloned()
                .map(|event| SessionRecord::Tool { event }),
        );
        if !state.queue.is_empty() {
            records.push(entries_record(&state.queue));
        }
        records.extend(
            state
                .compactions
                .iter()
                .cloned()
                .map(|record| SessionRecord::Compaction { record }),
        );
        records.extend(
            state
                .branch_summaries
                .iter()
                .cloned()
                .map(|summary| SessionRecord::BranchSummary { summary }),
        );
        let mut journal = Vec::new();
        for record in &records {
            serde_json::to_writer(&mut journal, record).map_err(|source| SessionError::Parse {
                path: self.path.clone(),
                source,
            })?;
            journal.push(b'\n');
        }
        self.storage.rewrite(&mut |_| Ok(journal.clone()))
    }

    /// Appends `records` in one write: the journal holds all of them or
    /// none (#175).
    fn append_all(&self, records: &[SessionRecord]) -> Result<(), SessionError> {
        let mut lines = Vec::new();
        for record in records {
            if !lines.is_empty() {
                lines.push(b'\n');
            }
            let line = serde_json::to_vec(record).map_err(|source| SessionError::Parse {
                path: self.path.clone(),
                source,
            })?;
            lines.extend_from_slice(&line);
        }
        self.storage.append(&lines)?;
        for record in records {
            self.observe_history(record);
        }
        Ok(())
    }

    fn append(&self, record: &SessionRecord) -> Result<(), SessionError> {
        let line = serde_json::to_vec(record).map_err(|source| SessionError::Parse {
            path: self.path.clone(),
            source,
        })?;
        self.storage.append(&line)?;
        self.observe_history(record);
        Ok(())
    }

    /// Tells the observer what `record`, now saved, adds to the history.
    fn observe_history(&self, record: &SessionRecord) {
        if let (Some(ObserverSlot(observer)), Some(added)) =
            (&self.progress, history_record(record))
        {
            observer(&SavedProgress::History(added));
        }
    }

    /// The session's history as its journal holds it, oldest first (see
    /// [`history`]). A session imported from pi's format is its messages.
    pub fn read_history(path: &Path) -> Result<Vec<history::HistoryRecord>, SessionError> {
        let content = read_journal_bytes(path)?;
        if jsonl_is_ts_session_export(&content) {
            let state = Self::read(path)?;
            return Ok(state
                .messages
                .into_iter()
                .map(|message| history::HistoryRecord::Message {
                    message,
                    timestamp: None,
                    queued: None,
                })
                .collect());
        }
        let (records, tail) = noel_session::split_jsonl_tail(&content);
        let mut history = Vec::new();
        let lines = String::from_utf8_lossy(records);
        let tail = (!is_blank_line(tail)).then(|| String::from_utf8_lossy(tail).into_owned());
        for line in lines.lines().map(str::to_owned).chain(tail) {
            if line.trim().is_empty() {
                continue;
            }
            // What the session does not read (a torn tail) is no history.
            if let Some(added) = serde_json::from_str::<SessionRecord>(&line)
                .ok()
                .as_ref()
                .and_then(history_record)
            {
                history.push(added);
            }
        }
        Ok(history)
    }
}

fn metadata_record(state: &SessionState) -> SessionRecord {
    SessionRecord::Metadata {
        name: state.name.clone(),
        labels: state.labels.iter().cloned().collect(),
        parent_session_id: state.parent_session_id.clone(),
    }
}

fn queue_record(steering: Vec<String>, follow_ups: Vec<String>) -> SessionRecord {
    let count = steering.len();
    let mut messages = steering;
    messages.extend(follow_ups);
    SessionRecord::QueuedMessagesSnapshot {
        messages,
        steering: count,
        items: Vec::new(),
        withdrawn: Vec::new(),
    }
}

/// The queue record for saved `entries` (a checkpoint): their identities
/// kept where they have one.
fn entries_record(entries: &[QueuedEntry]) -> SessionRecord {
    let durable = entries
        .iter()
        .any(|entry| entry.id.as_ref().is_some_and(QueueItemId::is_durable));
    SessionRecord::QueuedMessagesSnapshot {
        messages: entries.iter().map(|entry| entry.content.clone()).collect(),
        steering: entries
            .iter()
            .take_while(|entry| entry.mode == QueueMode::Steer)
            .count(),
        items: if durable {
            entries.iter().map(|entry| entry.id.clone()).collect()
        } else {
            Vec::new()
        },
        withdrawn: Vec::new(),
    }
}

/// The queue record for `items`, steering first.
fn queue_items_record(items: &[QueueItem], withdrawn: &[QueueItemId]) -> SessionRecord {
    let steering = items
        .iter()
        .take_while(|item| item.id.mode == QueueMode::Steer)
        .count();
    let durable = items.iter().any(|item| item.id.is_durable());
    SessionRecord::QueuedMessagesSnapshot {
        messages: items.iter().map(|item| item.content.clone()).collect(),
        steering,
        items: if durable {
            items.iter().map(|item| Some(item.id.clone())).collect()
        } else {
            Vec::new()
        },
        withdrawn: withdrawn
            .iter()
            .filter(|id| id.is_durable())
            .cloned()
            .collect(),
    }
}

/// What a workspace is chosen for (upstream Durable's `EnvTarget`).
#[derive(Debug, Clone, Copy)]
pub struct EnvTarget<'a> {
    /// The session (upstream's conversation).
    pub session_id: &'a str,
    /// The session's working directory.
    pub cwd: &'a Path,
}

/// Builds the workspace one tool call runs in; an `Err` is that call's
/// error result.
pub type EnvHook =
    Arc<dyn Fn(&EnvTarget<'_>) -> Result<Arc<dyn ExecutionEnv>, String> + Send + Sync>;

/// Opens the workspace a session's journal names ([`SessionWorkspace`]) for
/// one tool call; an `Err` is that call's error result.
pub type WorkspaceHook = Arc<
    dyn Fn(&EnvTarget<'_>, &SessionWorkspace) -> Result<Arc<dyn ExecutionEnv>, String>
        + Send
        + Sync,
>;

/// Chooses the workspace of each tool call (upstream Durable's harness
/// `env(target)` hook), asked anew for every call.
#[derive(Clone, Default)]
pub enum EnvProvider {
    /// A [`LocalEnv`] at the session's working directory, with the
    /// `shellPath` setting: every mode's default. A session whose journal
    /// names a workspace gets an error instead, never this machine.
    #[default]
    Local,
    Hook(EnvHook),
    /// [`EnvProvider::Local`] for a session without a workspace record; the
    /// hook opens the workspace of one that has it.
    Workspaces(WorkspaceHook),
}

impl std::fmt::Debug for EnvProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Local => "EnvProvider::Local",
            Self::Hook(_) => "EnvProvider::Hook",
            Self::Workspaces(_) => "EnvProvider::Workspaces",
        })
    }
}

/// How long a run waits for a remote workspace's context files before it
/// goes on without them; it tries again at the next run.
const WORKSPACE_CONTEXT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Runtime {
    session: SessionState,
    systems: ReloadableSystems,
    store: Option<SessionStore>,
    /// The thinking level of the current turn's provider, which bash
    /// commands see as `NOEL_REASONING_LEVEL`.
    thinking_level: Option<String>,
    env_provider: EnvProvider,
    /// The project context files read from the session's workspace, once a
    /// run loaded them (spec §10); `None` until then.
    workspace_context: Option<Vec<ContextFile>>,
    /// Whether a remote workspace's project context files are read
    /// (`--no-context-files` turns it off).
    workspace_context_enabled: bool,
    /// The persistent JSON-protocol v2 extension hosts and their
    /// registrations. Durable session state stays plain data; the hosts
    /// are processes, owned here and dropped with the runtime.
    extension_hosts: Arc<tokio::sync::Mutex<extensions::ExtensionHosts>>,
    /// Plugin tools (roadmap phase 7) and what runs them: offered with the
    /// extension tools, and kept across reloads.
    plugin_tools: Vec<plugins::PluginTool>,
    plugin_host: Option<Arc<dyn plugins::PluginToolHost>>,
    /// Builds a provider for a subagent's model override (the `model`
    /// frontmatter of an agent definition); `None` means the mode cannot
    /// (a test runtime, say), and an override then fails with a message.
    subagent_provider_factory: Option<SubagentProviderFactory>,
    /// The MCP server connections, started with the run that first
    /// needs them (as extension hosts are).
    mcp_hosts: SharedMcpHosts,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("session", &self.session)
            .field("systems", &self.systems)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    pub fn new(session: SessionState, systems: ReloadableSystems) -> Self {
        Self {
            session,
            systems,
            store: None,
            thinking_level: None,
            env_provider: EnvProvider::default(),
            workspace_context: None,
            workspace_context_enabled: true,
            extension_hosts: Arc::new(tokio::sync::Mutex::new(extensions::ExtensionHosts::new())),
            plugin_tools: Vec::new(),
            plugin_host: None,
            subagent_provider_factory: None,
            mcp_hosts: Arc::new(tokio::sync::Mutex::new(McpHosts::default())),
        }
    }

    /// Sets the factory a subagent's `model` override builds its provider
    /// with (see the field). Each mode sets its own at startup.
    pub fn set_subagent_provider_factory(&mut self, factory: SubagentProviderFactory) {
        self.subagent_provider_factory = Some(factory);
    }

    /// The plugin tools the run offers from now on, run by `host`; they
    /// replace the ones set before.
    pub fn set_plugin_tools(
        &mut self,
        host: Arc<dyn plugins::PluginToolHost>,
        tools: Vec<plugins::PluginTool>,
    ) {
        self.plugin_host = Some(host);
        self.plugin_tools = tools;
        overlay_plugin_tools(&mut self.systems, &self.plugin_tools);
    }

    pub fn with_store(
        session: SessionState,
        systems: ReloadableSystems,
        store: SessionStore,
    ) -> Self {
        Self {
            store: Some(store),
            ..Self::new(session, systems)
        }
    }

    /// A subagent's session, for tests of the `task` tool's scoping.
    #[cfg(test)]
    pub(crate) fn child_session(&self) -> &SessionState {
        &self.session
    }

    /// Whether the system prompt of a session with a remote workspace lists
    /// the project context files found there; default true.
    pub fn set_workspace_context_enabled(&mut self, enabled: bool) {
        self.workspace_context_enabled = enabled;
        self.workspace_context = None;
    }

    /// Reads the session workspace's project context files for the system
    /// prompt, unless they were read already. The workspace may be out of
    /// reach: then the prompt goes without them, and the next run tries
    /// again, while its tool calls report the failure.
    async fn load_workspace_context(&mut self, run: &RunControl) -> Result<(), AgentError> {
        if self.session.workspace.is_none()
            || self.workspace_context.is_some()
            || !self.workspace_context_enabled
        {
            return Ok(());
        }
        let Ok(env) = self.tool_env() else {
            return Ok(());
        };
        let cx = noel_env::Context::background();
        let loaded = run
            .until_cancelled(tokio::time::timeout(
                WORKSPACE_CONTEXT_TIMEOUT,
                workspace::workspace_context_files(env.as_ref(), &cx),
            ))
            .await?;
        if let Ok(Ok(files)) = loaded {
            self.workspace_context = Some(files);
        }
        Ok(())
    }

    pub fn env_provider(&self) -> &EnvProvider {
        &self.env_provider
    }

    /// Chooses the workspace of later tool calls.
    pub fn set_env_provider(&mut self, provider: EnvProvider) {
        self.env_provider = provider;
    }

    /// The workspace for one tool call, from the env provider. A session
    /// whose journal names a workspace runs nowhere else: without a provider
    /// that opens it, its tool calls fail.
    pub fn tool_env(&self) -> Result<Arc<dyn ExecutionEnv>, String> {
        let target = EnvTarget {
            session_id: &self.session.session_id,
            cwd: &self.session.cwd,
        };
        match (&self.env_provider, &self.session.workspace) {
            (EnvProvider::Hook(hook), _) => hook(&target),
            (EnvProvider::Local | EnvProvider::Workspaces(_), None) => {
                Ok(Arc::new(LocalEnv::with_options(
                    &self.session.cwd,
                    LocalEnvOptions {
                        shell_path: self.systems.shell_path.clone(),
                    },
                )))
            }
            (EnvProvider::Workspaces(open), Some(workspace)) => open(&target, workspace),
            (EnvProvider::Local, Some(workspace)) => Err(format!(
                "This session's tools run in its {} workspace {}, which this process cannot open",
                workspace.adapter, workspace.id
            )),
        }
    }

    pub fn session(&self) -> &SessionState {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SessionState {
        &mut self.session
    }

    pub fn systems(&self) -> &ReloadableSystems {
        &self.systems
    }

    /// The persistent extension hosts: the v2 extensions of the loaded
    /// extension set, started on first use, with the registrations their
    /// handshakes produced overlaid on the manifest tools.
    pub fn extension_hosts(&self) -> Arc<tokio::sync::Mutex<extensions::ExtensionHosts>> {
        Arc::clone(&self.extension_hosts)
    }

    /// Start the v2 hosts of the loaded extensions that have none yet.
    /// Idempotent: hosts already running keep their state. Returns the
    /// diagnostics of extensions that failed to start; their tools then
    /// fail closed when called.
    pub async fn start_extension_hosts(&mut self) -> Vec<String> {
        let cwd = self.session.cwd.clone();
        let session_id = self.session.session_id.clone();
        let mut hosts = self.extension_hosts.lock().await;
        let mut diagnostics = Vec::new();
        // The extension files of this generation, v2 ones first so a tool
        // name a v2 and a v1 extension both claim goes to the v2 host.
        let mut started = Vec::new();
        for extension in &self.systems.extension_files {
            if extensions::manifest_protocol_version(&extension.path) != Some(2) {
                continue;
            }
            started.push(extension.name.clone());
            if hosts.started_as(&extension.name, &extension.path, &cwd, &session_id) {
                continue;
            }
            if let Err(error) = hosts
                .start(&extension.name, &extension.path, &cwd, &session_id)
                .await
            {
                diagnostics.push(error);
            }
        }
        // A reload may have removed a v2 extension: stop its host so its
        // tools stop being offered.
        hosts.retain_extensions(&started);
        // Overlay the live registrations on the manifest tools the run
        // presents to the model and the prompt.
        overlay_live_registrations(&mut self.systems, &hosts);
        overlay_plugin_tools(&mut self.systems, &self.plugin_tools);
        diagnostics
    }

    /// Starts the MCP connections of the configured servers (spec §9.7),
    /// as the run's extension hosts start, and overlays their tools on
    /// the run's tool surface. One system line per newly-failed server.
    pub async fn start_mcp_hosts(&mut self) -> Vec<String> {
        let servers = self.systems.mcp_servers.clone();
        let cwd = self.session.cwd.clone();
        let agent_dir = self.systems.mcp_agent_dir.clone();
        let owned = self.mcp_hosts_arc();
        let mut hosts = owned.lock().await;
        let mut overlay = std::mem::take(&mut self.systems.mcp_tools);
        let failures = hosts
            .start(&servers, &cwd, agent_dir.as_deref(), &mut overlay)
            .await;
        self.systems.mcp_tools = overlay;
        failures
    }

    /// The shared MCP hosts handle.
    pub fn mcp_hosts_arc(&self) -> SharedMcpHosts {
        self.mcp_hosts.clone()
    }

    /// Drops the configured MCP servers (`--no-mcp`): this run connects
    /// none, and offers none of their tools.
    pub fn clear_mcp_servers(&mut self) {
        self.systems.mcp_servers.clear();
        self.systems.mcp_tools.clear();
    }

    /// Refresh the overlay of live v2 registrations on the reloadable
    /// systems after a host response changed them.
    pub async fn refresh_extension_registrations(&mut self) {
        let hosts = self.extension_hosts.lock().await;
        overlay_live_registrations(&mut self.systems, &hosts);
        overlay_plugin_tools(&mut self.systems, &self.plugin_tools);
    }

    /// Adopt a registration change a host response carried: the host
    /// registry takes the set, and the overlay the run presents follows.
    pub async fn apply_extension_registrations(
        &mut self,
        set: extensions::ExtensionRegistrationSet,
    ) {
        {
            let owned = self.extension_hosts();
            let mut hosts = owned.lock().await;
            hosts.apply_registrations(set);
        }
        self.refresh_extension_registrations().await;
    }
    pub fn store(&self) -> Option<&SessionStore> {
        self.store.as_ref()
    }

    pub fn push_message(&mut self, message: ConversationMessage) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_message(message.clone())?;
        }
        self.session.supersede_progress(&message);
        self.session.messages.push(message);
        Ok(())
    }

    pub fn replace_messages(
        &mut self,
        messages: Vec<ConversationMessage>,
    ) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_messages_snapshot(messages.clone())?;
        }
        self.session.partial = None;
        self.session.set_messages(messages);
        Ok(())
    }

    /// Saves `output` as the result of the model's tool call `call`.
    fn push_tool_result(
        &mut self,
        call: &ChatToolCall,
        output: String,
        media: Vec<MediaInput>,
        is_error: bool,
    ) -> Result<(), SessionError> {
        self.push_tool_event(ToolEvent {
            id: call.id.clone(),
            name: call.name.clone(),
            result: output.clone(),
            is_error,
        })?;
        self.push_message(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Tool,
            content: output,
            media,
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
            tool_calls: Vec::new(),
            aborted: false,
        })
    }

    /// Saves the result of `call`, which a stop cut off (spec §4.2): the
    /// output it had saved, and that it was interrupted.
    fn push_interrupted_result(&mut self, call: &ChatToolCall) -> Result<(), SessionError> {
        let output =
            progress::interrupted_output(&call.name, self.session.tool_outputs.get(&call.id));
        self.push_tool_result(call, output, Vec::new(), true)
    }

    /// Gives every tool call the last reply still owes a result an
    /// interrupted one: nothing will run them, since the turn they belong
    /// to is over.
    fn settle_unanswered_calls(&mut self) -> Result<(), SessionError> {
        let calls = match self.session.pending_tool_calls() {
            Some((_, calls)) => calls.into_iter().cloned().collect::<Vec<_>>(),
            None => return Ok(()),
        };
        for call in &calls {
            self.push_interrupted_result(call)?;
        }
        Ok(())
    }

    pub fn push_tool_event(&mut self, event: ToolEvent) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_tool(event.clone())?;
        }
        self.session.tool_history.push(event);
        Ok(())
    }

    pub fn set_active_model(&mut self, model: Option<ModelRef>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_active_model(model.clone())?;
        }
        self.session.active_model = model;
        Ok(())
    }

    pub fn set_active_account(&mut self, account: Option<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_active_account(account.clone())?;
        }
        self.session.active_account = account;
        Ok(())
    }

    /// Replaces this conversation's compaction settings (`None` clears
    /// them); Durable's `configure({compaction})`.
    pub fn set_compaction_settings(
        &mut self,
        settings: Option<noel_config::CompactionSettings>,
    ) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_compaction_settings(settings.clone())?;
        }
        self.session.compaction = settings;
        Ok(())
    }

    /// Sets the model and the thinking level together, in one journal
    /// write, so a failure leaves neither changed (#175).
    pub fn set_active_model_and_thinking_level(
        &mut self,
        model: Option<ModelRef>,
        level: Option<String>,
    ) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.append_all(&[
                SessionRecord::ActiveModel {
                    model: model.clone(),
                },
                SessionRecord::ActiveThinkingLevel {
                    level: level.clone(),
                },
            ])?;
        }
        self.session.active_model = model;
        self.session.active_thinking_level = level;
        Ok(())
    }

    pub fn set_active_thinking_level(&mut self, level: Option<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_active_thinking_level(level.clone())?;
        }
        self.session.active_thinking_level = level;
        Ok(())
    }

    pub fn set_disabled_tools(&mut self, tools: BTreeSet<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_disabled_tools(tools.iter().cloned().collect())?;
        }
        self.session.disabled_tools = tools;
        self.session.active_tool_names = active_tools_with(&self.session.disabled_tools);
        Ok(())
    }

    /// Enters or leaves plan mode (spec §9.4), journaled so a resumed
    /// session keeps the mode.
    pub fn set_plan_mode(&mut self, on: bool) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_plan_mode(on)?;
        }
        self.session.plan_mode = on;
        Ok(())
    }

    /// Sets the session goal (spec §9.3), replacing any earlier one, and
    /// journals it. A goal persists across resume.
    pub fn set_goal(&mut self, condition: impl Into<String>) -> Result<(), SessionError> {
        let goal = SessionGoal {
            condition: condition.into(),
            turns: 0,
            last_status: String::new(),
        };
        if let Some(store) = &self.store {
            store.record_goal(goal.clone())?;
        }
        self.session.goal = Some(goal);
        Ok(())
    }

    /// Clears the session goal, journaling why (`None` for `/goal clear`).
    pub fn clear_goal(&mut self, reason: Option<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_goal_cleared(reason)?;
        }
        self.session.goal = None;
        Ok(())
    }

    /// Advances the goal after the evaluator answered: the turn count and
    /// the evaluator's sentence, journaled so a resume shows both.
    pub fn advance_goal(&mut self, status: &str) -> Result<(), SessionError> {
        let Some(goal) = self.session.goal.as_mut() else {
            return Ok(());
        };
        goal.turns += 1;
        goal.last_status = status.to_string();
        let goal = goal.clone();
        if let Some(store) = &self.store {
            store.record_goal(goal)?;
        }
        Ok(())
    }

    pub fn set_todos(&mut self, todos: Vec<TodoItem>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_todos(todos.clone())?;
        }
        self.session.todos = todos;
        Ok(())
    }

    /// Saves the codemode store a script changed.
    pub fn set_codemode_store(
        &mut self,
        store: BTreeMap<String, Value>,
    ) -> Result<(), SessionError> {
        if let Some(session_store) = &self.store {
            session_store.record_codemode_store(store.clone())?;
        }
        self.session.codemode_store = store;
        Ok(())
    }

    pub fn note_edited_files(&mut self, paths: Vec<String>) -> Result<(), SessionError> {
        let mut changed = false;
        for path in paths {
            if !self.session.edited_files.contains(&path) {
                self.session.edited_files.push(path);
                changed = true;
            }
        }
        if changed && let Some(store) = &self.store {
            store.record_edited_files(self.session.edited_files.clone())?;
        }
        Ok(())
    }

    pub fn set_store(&mut self, store: SessionStore) {
        self.store = Some(store);
    }

    pub fn replace_session(&mut self, session: SessionState, store: Option<SessionStore>) {
        self.session = session;
        self.store = store;
        self.workspace_context = None;
    }

    pub fn rename_session(&mut self, name: Option<String>) -> Result<(), SessionError> {
        self.session.name = name;
        if let Some(store) = &self.store {
            store.record_metadata(&self.session)?;
        }
        Ok(())
    }

    pub fn set_labels(&mut self, labels: BTreeSet<String>) -> Result<(), SessionError> {
        self.session.labels = labels;
        if let Some(store) = &self.store {
            store.record_metadata(&self.session)?;
        }
        Ok(())
    }

    pub fn queue_message(&mut self, message: String) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_queued_message(message.clone())?;
        }
        // The other entries keep their identities (#138: all were wiped).
        self.session.queue.push(QueuedEntry::follow_up(message));
        Ok(())
    }

    /// Replaces the queue with `messages`, all follow-ups.
    pub fn replace_queued_messages(&mut self, messages: Vec<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_queued_messages_snapshot(messages.clone())?;
        }
        self.session.queue = messages.into_iter().map(QueuedEntry::follow_up).collect();
        Ok(())
    }

    pub fn clear_queued_messages(&mut self) -> Result<usize, SessionError> {
        let count = self.session.queue.len();
        self.replace_queued_messages(Vec::new())?;
        Ok(count)
    }

    /// Starts run `start`: saves its prompt as a user message, together
    /// with the run's start, and returns the message for
    /// [`run_saved_prompt_streaming_events`]. An interrupted last run is
    /// recorded as aborted first.
    pub fn start_run(
        &mut self,
        start: RunStart,
        prompt: String,
        media: Vec<MediaInput>,
    ) -> Result<ConversationMessage, SessionError> {
        tracing::debug!(run = %start.id, "run started");
        self.settle_unanswered_calls()?;
        let interrupted = self
            .session
            .runs
            .last()
            .filter(|run| run.status == RunStatus::Interrupted)
            .map(|run| run.id.clone());
        if let Some(run_id) = interrupted {
            self.finish_run(&run_id, RunStatus::Aborted)?;
        }
        let message = user_message(prompt, media);
        if let Some(store) = &self.store {
            store.append(&SessionRecord::Message {
                message: message.clone(),
                run: Some(start.clone()),
                ts: Some(history::now_ms()),
                placed: Vec::new(),
            })?;
        }
        self.session.messages.push(message.clone());
        self.session
            .runs
            .push(Run::started(start, RunStatus::Running));
        Ok(message)
    }

    /// Saves `message`, a user message that places the queued submissions
    /// `placed` into the active run, in one record with them (see
    /// [`SessionStore::record_placed_message`]).
    pub fn push_placed_message(
        &mut self,
        message: ConversationMessage,
        placed: Vec<QueueItemId>,
    ) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_placed_message(message.clone(), placed)?;
        }
        self.session.messages.push(message);
        Ok(())
    }

    /// This process continues the interrupted run `run_id`; if this
    /// process stops too, the run is interrupted again. A run a close
    /// stopped is saved as open again, so that stop is a crash (#130).
    pub fn resume_run(&mut self, run_id: &str) -> Result<(), SessionError> {
        tracing::debug!(run = run_id, "run resumed");
        let Some(run) = self.session.run_mut(run_id) else {
            return Ok(());
        };
        run.status = RunStatus::Running;
        if run.closed {
            run.reopen();
            if let Some(store) = &self.store {
                store.append(&SessionRecord::RunReopened {
                    run_id: run_id.to_owned(),
                })?;
            }
        }
        Ok(())
    }

    /// Run `run_id` was accepted and never run (its start was dropped): it
    /// is interrupted, as the journal already reads it, so a resume or the
    /// next start ends it (#140). Nothing is saved.
    pub fn interrupt_unrun(&mut self, run_id: &str) {
        if let Some(run) = self.session.run_mut(run_id)
            && run.status == RunStatus::Running
        {
            run.status = RunStatus::Interrupted;
        }
    }

    /// Records how run `run_id` ended. The status changes even if saving
    /// it fails, since this process is no longer running the run.
    pub fn finish_run(&mut self, run_id: &str, status: RunStatus) -> Result<(), SessionError> {
        self.end_run(run_id, status, None)
    }

    /// Records how run `run_id` ended, with a failed run's reason.
    pub fn end_run(
        &mut self,
        run_id: &str,
        status: RunStatus,
        failure: Option<RunFailure>,
    ) -> Result<(), SessionError> {
        tracing::debug!(run = run_id, ?status, "run ended");
        if let Some(run) = self.session.run_mut(run_id) {
            run.finish(status, failure.clone());
        }
        self.session.partial = None;
        self.session.tool_outputs.clear();
        if let Some(store) = &self.store {
            store.append(&SessionRecord::RunFinished {
                run_id: run_id.to_owned(),
                status,
                failure,
            })?;
        }
        Ok(())
    }

    /// Starts a run that saves no message of its own (a continue), ending
    /// an interrupted last run first, as a prompt's start does.
    pub fn start_bare_run(&mut self, start: RunStart) -> Result<(), SessionError> {
        let interrupted = self
            .session
            .runs
            .last()
            .filter(|run| run.status == RunStatus::Interrupted)
            .map(|run| run.id.clone());
        if let Some(run_id) = interrupted {
            self.finish_run(&run_id, RunStatus::Aborted)?;
        }
        if let Some(store) = &self.store {
            store.append(&SessionRecord::RunStarted { run: start.clone() })?;
        }
        self.session
            .runs
            .push(Run::started(start, RunStatus::Running));
        Ok(())
    }

    /// A close stops run `run_id`: its next resume is not a restart.
    pub fn close_run(&mut self, run_id: &str) -> Result<(), SessionError> {
        tracing::debug!(run = run_id, "run closed");
        if let Some(run) = self.session.run_mut(run_id) {
            run.close();
        }
        if let Some(store) = &self.store {
            store.append(&SessionRecord::RunClosed {
                run_id: run_id.to_owned(),
            })?;
        }
        Ok(())
    }

    /// Counts a crash restart of run `run_id`, and returns its restarts.
    pub fn count_restart(&mut self, run_id: &str) -> Result<u32, SessionError> {
        tracing::debug!(run = run_id, "run crashed; counting a restart");
        if let Some(store) = &self.store {
            store.append(&SessionRecord::RunRestarted {
                run_id: run_id.to_owned(),
            })?;
        }
        Ok(self.session.run_mut(run_id).map(Run::restart).unwrap_or(0))
    }

    /// Turns automatic retries on or off for this runtime's turns; the
    /// settings file keeps its value.
    pub fn set_retry_enabled(&mut self, enabled: bool) {
        self.systems.retry.enabled = enabled;
    }

    /// Whether a tool call cut off by a stop may run again when its turn
    /// continues: `read`, `grep`, `find`, `ls`, and extension tools whose
    /// manifest declares `"replaySafe": true`.
    pub fn replay_safe(&self, tool: &str) -> bool {
        match self.systems.extension_tools.get(tool) {
            Some(extension) => extension.replay_safe,
            None => matches!(tool, "read" | "grep" | "find" | "ls"),
        }
    }

    fn record_usage(&mut self, input_tokens: u64, output_tokens: u64) -> Result<(), SessionError> {
        let mut usage = self.session.usage.clone();
        usage.input_tokens = usage.input_tokens.saturating_add(input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(output_tokens);
        usage.requests = usage.requests.saturating_add(1);
        if let Some(store) = &self.store {
            store.record_usage(usage.clone())?;
        }
        self.session.usage = usage;
        Ok(())
    }

    pub fn reload(&mut self, next: ReloadableSystems) -> Result<ReloadReport, ReloadError> {
        if next.config_generation < self.systems.config_generation {
            return Err(ReloadError::Invalid(
                "config generation cannot move backwards".to_string(),
            ));
        }

        let active_model_valid = self
            .session
            .active_model
            .as_ref()
            .map(|model| {
                next.available_models
                    .iter()
                    .any(|candidate| candidate == model)
            })
            .unwrap_or(true);
        let active_account_valid = match (&self.session.active_account, &self.session.active_model)
        {
            (Some(account), Some(model)) => next
                .available_accounts
                .get(&model.provider)
                .map(|accounts| accounts.contains(account))
                .unwrap_or(false),
            _ => true,
        };
        // Tools the session could use before the reload and no longer can.
        let was_available = |tool_name: &String| {
            !self.systems.tool_scope_configured
                || self.systems.available_tool_names.contains(tool_name)
        };
        let removed_active_tools = self
            .session
            .active_tool_names
            .iter()
            .filter(|tool_name| was_available(tool_name))
            .filter(|tool_name| !next.available_tool_names.contains(*tool_name))
            .cloned()
            .collect();

        let mut next = next;
        next.offline |= self.systems.offline;
        self.systems = next;
        // The live v2 registrations stay overlaid (#159), for the
        // extensions still loaded; the next run starts or stops hosts.
        if let Ok(hosts) = self.extension_hosts.try_lock() {
            overlay_live_registrations(&mut self.systems, &hosts);
        }
        overlay_plugin_tools(&mut self.systems, &self.plugin_tools);
        // A reload reads context files again, a workspace's at the next run.
        self.workspace_context = None;
        Ok(ReloadReport {
            active_model_valid,
            active_account_valid,
            removed_active_tools,
        })
    }
}

/// Default safety cap on provider rounds per user run, including steering.
/// Each round can issue several tool calls at once, but real coding work
/// (read, edit, run tests, iterate) still needs far more than a handful of
/// rounds, so keep this generous while still bounding a runaway tool loop.
pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 200;

/// Durable's default `settings.restartLimit`.
pub const DEFAULT_RESTART_LIMIT: u32 = 5;

/// Resolves the effective per-run round cap from `agent.maxToolRounds`.
pub fn effective_max_tool_rounds(configured: Option<usize>) -> usize {
    configured.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS).max(1)
}

pub async fn run_user_turn(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
) -> Result<String, AgentError> {
    run_user_turn_streaming(runtime, provider, prompt, |_| {}).await
}

pub async fn run_user_turn_streaming(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
    on_text: impl FnMut(&str) + Send,
) -> Result<String, AgentError> {
    run_user_turn_streaming_with_media(
        runtime,
        provider,
        prompt,
        Vec::new(),
        &SteeringMailbox::default(),
        on_text,
    )
    .await
}

/// Mailbox for mid-turn steering messages. Senders (for example the TUI
/// key handler) push while a turn is streaming; the turn loop drains pending
/// messages into the conversation before the next provider request.
///
/// Everything queued at an injection point is combined into a single user
/// message (joined with [`STEERING_JOIN`]) so a burst of steering reads as
/// one instruction rather than several interruptions.
///
/// A message may carry its queued submission's identity
/// ([`SteeringMailbox::push`]): the turn saves it with the message that
/// places it, and keeps it for [`SteeringMailbox::take_delivered`].
#[derive(Debug, Clone, Default)]
pub struct SteeringMailbox {
    inner: Arc<Mutex<Mailbox>>,
}

#[derive(Debug, Default)]
struct Mailbox {
    messages: VecDeque<(String, Option<QueueItemId>)>,
    /// The identities of messages a turn has placed, until taken.
    delivered: Vec<QueueItemId>,
}

impl SteeringMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.inner.lock().expect("steering mailbox")
    }

    pub fn send(&self, message: String) {
        self.lock().messages.push_back((message, None));
    }

    /// Queues a submission with its identity.
    pub fn push(&self, item: QueueItem) {
        self.lock()
            .messages
            .push_back((item.content, Some(item.id)));
    }

    pub fn pending(&self) -> usize {
        self.lock().messages.len()
    }

    pub fn list(&self) -> Vec<String> {
        let mailbox = self.lock();
        mailbox
            .messages
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }

    /// The queued messages that carry an identity, in order.
    pub fn items(&self) -> Vec<QueueItem> {
        let mailbox = self.lock();
        let items = mailbox.messages.iter().filter_map(|(content, id)| {
            id.clone().map(|id| QueueItem {
                id,
                content: content.clone(),
            })
        });
        items.collect()
    }

    pub fn drain(&self) -> Vec<String> {
        let mut mailbox = self.lock();
        mailbox.messages.drain(..).map(|(text, _)| text).collect()
    }

    /// Takes the queued message of submission `submission_id`, if it is
    /// still queued.
    pub fn remove(&self, submission_id: &str) -> Option<QueueItem> {
        let mut mailbox = self.lock();
        let index = mailbox.messages.iter().position(|(_, id)| {
            id.as_ref()
                .is_some_and(|id| id.submission_id == submission_id)
        })?;
        let (content, id) = mailbox.messages.remove(index)?;
        id.map(|id| QueueItem { id, content })
    }

    /// The identities of the messages turns have placed since the last
    /// call, oldest first.
    pub fn take_delivered(&self) -> Vec<QueueItemId> {
        std::mem::take(&mut self.lock().delivered)
    }

    /// Takes everything queued for a turn to place: the texts, and the
    /// identities, which are also kept for [`SteeringMailbox::take_delivered`].
    fn take_batch(&self) -> (Vec<String>, Vec<QueueItemId>) {
        let mut mailbox = self.lock();
        let (texts, ids): (Vec<_>, Vec<_>) = mailbox.messages.drain(..).unzip();
        let ids = ids.into_iter().flatten().collect::<Vec<_>>();
        mailbox.delivered.extend(ids.iter().cloned());
        (texts, ids)
    }
}

/// Separator used when multiple queued steering messages are combined into a
/// single user message so the model reads them as one instruction burst, not
/// separate interruptions.
pub const STEERING_JOIN: &str = "\n---\n";

/// A caller's handles on a running turn: steering for its next provider
/// round, cancellation, and the backoff of a retry. Cancelling ends the turn
/// with [`AgentError::Cancelled`] at once, dropping an in-flight request,
/// backoff, compaction or tool call (a dropped tool takes its process group
/// with it). What the turn saved stays saved, and what it was producing is
/// saved as it ends (spec §4.2): a reply as an aborted reply, a tool call's
/// output as its aborted result. Clones share all of them.
#[derive(Debug, Clone, Default)]
pub struct RunControl {
    pub steering: SteeringMailbox,
    pub cancellation: CancellationToken,
    pub retry: RetryAbort,
    /// The run the turn belongs to, which names what it saves as it goes
    /// (spec §4.2); a mode without runs has none.
    pub run_id: Option<String>,
    /// A continued turn whose model request was in flight when the host
    /// died, on this attempt: the attempt failed, and is charged to the
    /// retry policy before the request is sent again (Durable).
    pub interrupted_attempt: Option<u64>,
}

/// Ends the backoff a turn is waiting out before a retry (upstream
/// `abortRetry`): the turn then ends with the failure it would have retried.
/// Outside a backoff there is nothing to end. Clones share the backoff.
#[derive(Debug, Clone, Default)]
pub struct RetryAbort(Arc<Mutex<Option<CancellationToken>>>);

impl RetryAbort {
    /// Ends the backoff in progress; `false` if there is none.
    pub fn abort(&self) -> bool {
        let backoff = self.0.lock().expect("retry backoff");
        backoff.as_ref().map(CancellationToken::cancel).is_some()
    }

    /// Sleeps for `delay` unless [`RetryAbort::abort`] ends the backoff
    /// first; `false` if it did.
    async fn backoff(&self, delay: Duration) -> bool {
        /// The backoff is over however it ends, a dropped turn included.
        struct Over<'a>(&'a RetryAbort);
        impl Drop for Over<'_> {
            fn drop(&mut self) {
                *self.0 .0.lock().expect("retry backoff") = None;
            }
        }
        let aborted = CancellationToken::default();
        *self.0.lock().expect("retry backoff") = Some(aborted.clone());
        let _over = Over(self);
        tokio::select! {
            biased;
            () = aborted.cancelled() => false,
            () = tokio::time::sleep(delay) => true,
        }
    }
}

/// How a retry ends when its backoff is ended early, as upstream words it.
pub const RETRY_CANCELLED: &str = "Retry cancelled";

impl RunControl {
    /// Runs `step` unless the turn is cancelled first; cancelling drops it.
    async fn until_cancelled<T>(&self, step: impl Future<Output = T>) -> Result<T, AgentError> {
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(AgentError::Cancelled),
            output = step => Ok(output),
        }
    }
}

/// A turn that reports only text deltas; it can be steered, not cancelled.
pub async fn run_user_turn_streaming_with_media(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
    media: Vec<MediaInput>,
    steering: &SteeringMailbox,
    mut on_text: impl FnMut(&str) + Send,
) -> Result<String, AgentError> {
    let run = RunControl {
        steering: steering.clone(),
        ..RunControl::default()
    };
    run_user_turn_streaming_events_with_media(runtime, provider, prompt, media, &run, |event| {
        if let TurnEvent::Provider(StreamEvent::Text(delta)) = event {
            on_text(delta);
        }
    })
    .await
}

/// Streams assistant text and provider-supplied thinking separately. Thinking is
/// persisted separately and never added to the next provider request.
/// Live progress of a streaming turn: provider deltas plus tool lifecycle,
/// so a UI can show what the agent is doing between text blocks.
///
/// Each provider round of a turn reports `RoundStarted`, its user messages
/// (the prompt or steering), any compaction and the reply's `Provider`
/// deltas, the reply, then `ToolStarted`, `ToolUpdate`, `ToolOutput` and
/// `ToolFinished` for each tool call, and `RoundFinished`. A turn that fails
/// or is cancelled stops mid-round without further events, except the
/// `ToolOutput` and `ToolFinished` of the aborted result a cancelled tool
/// call saves. A continued turn whose saved reply has replay-safe calls
/// without results first finishes that round: `RoundResumed`, those calls,
/// `RoundFinished`; the interrupted results of its other calls are saved
/// unreported.
///
/// A round whose reply is dropped ends there instead, and the turn asks
/// again in a new round: after `RetryStarted`, once the backoff is over;
/// after `Overflowed`, once its compaction (`CompactionStarted`, then
/// `CompactionFinished`) is done. A dropped reply is never kept, so its
/// `Provider` deltas are all that was reported of it. A reply an abort or a
/// failure cuts off is saved as an aborted reply, unreported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// A provider round started: one reply and the tool calls it asks for.
    RoundStarted,
    /// A continued turn finishes the round a stop cut off: this is the
    /// round's saved reply, whose replay-safe calls run next.
    RoundResumed(ConversationMessage),
    /// A message was saved: the prompt or steering that opens a round, or
    /// the round's reply after its deltas. Tool results are `ToolOutput`.
    MessageAdded(ConversationMessage),
    /// Everything the provider streams for the round's reply.
    Provider(StreamEvent),
    /// The round's request failed, before or after part of its reply, with
    /// an error a retry can fix. The reply is dropped and the request is
    /// sent again in a new round after the delay.
    RetryStarted(RetryNotice),
    /// The retries of a reply ended after `attempt` of them: with the reply
    /// (reported after it), or with the `error` that ends the turn
    /// ([`RETRY_CANCELLED`] if the backoff was ended early).
    RetryFinished {
        attempt: u64,
        error: Option<String>,
    },
    /// The round's reply ran out of context: the provider refused the
    /// request as too large (`error`), or cut the final reply off at the
    /// output limit (`None`). The reply is dropped. A compaction follows; if
    /// it succeeds the turn asks again in a new round, and otherwise the
    /// turn ends: with the error, or with the cut-off reply saved after all.
    Overflowed {
        error: Option<String>,
    },
    ToolStarted {
        call: ChatToolCall,
    },
    /// What a running tool has printed so far: a `bash` command's output,
    /// within the limits of its result. Empty when the command starts.
    ToolUpdate {
        call: ChatToolCall,
        output: String,
    },
    ToolFinished {
        name: String,
        is_error: bool,
    },
    /// The tool calls a running `codemode` script has made so far, each
    /// with its status: sent when the script starts and on every change
    /// (upstream's `details.calls`). Nested calls are not saved as results.
    NestedToolCalls {
        call: ChatToolCall,
        calls: Vec<NestedToolCall>,
    },
    /// A tool call's result as saved, and how long the tool ran (`None` for
    /// a call of a reply cut off at the output limit, which is not run).
    /// `images` are the attachments of the result (`read` of an image).
    ToolOutput {
        call: ChatToolCall,
        output: String,
        images: Vec<MediaInput>,
        is_error: bool,
        duration: Option<Duration>,
    },
    TodosUpdated(Vec<TodoItem>),
    /// A compaction started: because the context estimate crossed the
    /// threshold, or after `Overflowed`.
    CompactionStarted,
    /// A compaction replaced the history: the messages as saved now, sent
    /// just before `CompactionFinished`, so a copy of the session kept from
    /// the events stays the same as the saved one.
    HistoryRewritten(Vec<ConversationMessage>),
    CompactionFinished(CompactionRecord),
    /// Automatic compaction could not run (nothing compactable, or the
    /// retained tail alone exceeds the budget). The turn proceeds with the
    /// original history so the session stays usable; the provider surfaces
    /// a real error only if the request genuinely exceeds its window.
    CompactionFailed(String),
    /// The round's reply and its tool calls' results are saved.
    RoundFinished,
    /// A user hook's non-blocking notice (spec §9.5): shown, changing
    /// nothing.
    Notice(String),
}

/// Follow-up prompts typed while a turn streams. Shared between the key
/// handler (which appends and lists) and the turn runner (which moves them
/// into the session queue once the turn ends).
#[derive(Debug, Clone, Default)]
pub struct FollowUpQueue {
    messages: Arc<Mutex<Vec<String>>>,
}

impl FollowUpQueue {
    pub fn push(&self, message: String) {
        self.messages.lock().expect("follow-up queue").push(message);
    }

    pub fn pop(&self) -> Option<String> {
        self.messages.lock().expect("follow-up queue").pop()
    }

    pub fn list(&self) -> Vec<String> {
        self.messages.lock().expect("follow-up queue").clone()
    }

    pub fn clear(&self) -> usize {
        std::mem::take(&mut *self.messages.lock().expect("follow-up queue")).len()
    }

    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.messages.lock().expect("follow-up queue"))
    }

    pub fn pending(&self) -> usize {
        self.messages.lock().expect("follow-up queue").len()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The system prompt for the session as it is now: its tools, prompt
/// sources, context files, and working directory.
fn runtime_system_prompt(runtime: &Runtime) -> String {
    // Plan mode (spec §9.4) adds its rules as an addendum: research and
    // present a plan; the mutating tools are refused anyway, and the
    // instruction keeps `bash` read-only.
    let plan_addendum = if runtime.session.plan_mode {
        "\n\nYou are in plan mode. Research the codebase with read-only \
             tools (bash is for inspection only: no file changes, installs, \
             or side effects) and present a concrete plan: the steps, the \
             files each touches, and the risks. Do not present a plan until \
             you understand the problem; do not start implementing."
            .to_string()
    } else {
        String::new()
    };
    let Some(workspace) = &runtime.session.workspace else {
        return system_prompt::build_system_prompt(&SystemPromptInput {
            custom_prompt: runtime.systems.system_prompt.as_deref(),
            tools: &prompt_tools(runtime),
            append_system_prompt: &format!(
                "{}{}",
                runtime.systems.append_system_prompt.join("\n\n"),
                plan_addendum
            ),
            context_files: &runtime.systems.context_files,
            skills: &runtime.systems.skills,
            // Other sessions are reachable through `bash` (`noel client
            // ...`); a journaled session can be reached back, so its
            // prompt says so (spec §9.9). Ephemeral runs, subagent
            // children and the parity fixtures (no store) say nothing.
            sessions: runtime.store.is_some()
                && runtime.session.session_id != "ephemeral"
                && !runtime.session.session_id.ends_with("-task"),
            cwd: &runtime.session.cwd,
        });
    };
    // A remote workspace (spec §10): its directory, the user's own context
    // file from noel's agent dir, then the project's, read there. Skills are
    // files on this machine, which the workspace's `read` cannot reach, so
    // the prompt does not list them; `/skill:<name>` still expands here.
    let context_files = runtime
        .systems
        .agent_context_files
        .iter()
        .chain(runtime.workspace_context.iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    system_prompt::build_system_prompt(&SystemPromptInput {
        custom_prompt: runtime.systems.system_prompt.as_deref(),
        tools: &prompt_tools(runtime),
        append_system_prompt: &format!(
            "{}{}",
            runtime.systems.append_system_prompt.join("\n\n"),
            plan_addendum
        ),
        context_files: &context_files,
        skills: &[],
        sessions: false,
        cwd: Path::new(&workspace.cwd),
    })
}

/// The tools [`active_tool_definitions`] declares, as the prompt presents
/// them. Like there, an extension tool with a built-in tool's name replaces
/// it, so each tool is presented once.
fn prompt_tools(runtime: &Runtime) -> Vec<PromptTool> {
    let builtin = builtin_tool_definitions()
        .into_iter()
        .filter(|tool| !runtime.systems.extension_tools.contains_key(&tool.name))
        .filter(|tool| model_tool_enabled(runtime, &tool.name))
        // The `task` tool exists only when a subagent definition is loaded
        // (its description names them); without one it is not offered.
        .filter(|tool| tool.name != "task" || !runtime.systems.agents.is_empty())
        .map(|tool| PromptTool::builtin(&tool.name));
    let extension = runtime
        .systems
        .extension_tools
        .values()
        .filter(|tool| model_tool_enabled(runtime, &tool.name))
        .map(|tool| PromptTool {
            name: tool.name.clone(),
            snippet: tool.prompt_snippet.clone(),
            guidelines: tool.prompt_guidelines.clone(),
        });
    builtin.chain(extension).collect()
}

fn provider_request(runtime: &Runtime, system_prompt: String) -> ProviderRequest {
    let mut instructions = vec![system_prompt];
    instructions.extend(
        runtime
            .session
            .messages
            .iter()
            .filter(|message| {
                message.role == MessageRole::System && !compaction::is_checkpoint(message)
            })
            .map(|message| message.content.clone()),
    );
    ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some(instructions.join("\n\n")),
        messages: provider_messages(&runtime.session.messages),
        tools: active_tool_definitions(runtime),
    }
}

/// The messages of a request: the transcript without system messages
/// (other than compaction checkpoints) and without aborted replies, which
/// stay in the transcript only. Each tool call is followed by its result.
fn provider_messages(messages: &[ConversationMessage]) -> Vec<ChatMessage> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.aborted {
            index += 1;
            continue;
        }
        if message.role == MessageRole::Assistant && !message.tool_calls.is_empty() {
            output.push(conversation_to_chat_message(message));
            index += 1;

            let mut tool_outputs = BTreeMap::new();
            while index < messages.len() && messages[index].role == MessageRole::Tool {
                let tool_message = conversation_to_chat_message(&messages[index]);
                if let Some(tool_call_id) = tool_message.tool_call_id.clone() {
                    tool_outputs.entry(tool_call_id).or_insert(tool_message);
                }
                index += 1;
            }

            for tool_call in &message.tool_calls {
                output.push(
                    tool_outputs
                        .remove(&tool_call.id)
                        .unwrap_or_else(|| missing_tool_output(tool_call)),
                );
            }
            continue;
        }

        if message.role != MessageRole::Tool
            && (message.role != MessageRole::System || compaction::is_checkpoint(message))
        {
            output.push(conversation_to_chat_message(message));
        }
        index += 1;
    }
    output
}

fn missing_tool_output(tool_call: &ChatToolCall) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Tool,
        content: format!(
            "tool call {} did not complete before the session continued",
            tool_call.name
        ),
        media: Vec::new(),
        tool_call_id: Some(tool_call.id.clone()),
        tool_name: Some(tool_call.name.clone()),
        tool_calls: Vec::new(),
    }
}

fn conversation_to_chat_message(message: &ConversationMessage) -> ChatMessage {
    ChatMessage {
        role: match message.role {
            _ if compaction::is_checkpoint(message) => ChatRole::User,
            MessageRole::System => ChatRole::System,
            MessageRole::User => ChatRole::User,
            MessageRole::Assistant => ChatRole::Assistant,
            MessageRole::Tool => ChatRole::Tool,
        },
        content: message.content.clone(),
        media: message.media.clone(),
        tool_call_id: message.tool_call_id.clone(),
        tool_name: message.tool_name.clone(),
        tool_calls: message.tool_calls.clone(),
    }
}

async fn complete_with_retry_streaming(
    provider: &dyn Provider,
    request: ProviderRequest,
    retry: &RuntimeRetrySettings,
    cancellation: &CancellationToken,
    mut on_event: impl FnMut(&StreamEvent) + Send,
) -> Result<Vec<StreamEvent>, ProviderError> {
    noel_agent::complete_with_retry(
        provider,
        request,
        &retry_policy(retry),
        cancellation,
        |event| on_event(event),
    )
    .await
    .map_err(|error| match error {
        noel_agent::AgentError::Provider(error) => error,
        other => ProviderError::Config(other.to_string()),
    })
}

fn retry_policy(retry: &RuntimeRetrySettings) -> RetryPolicy {
    RetryPolicy {
        max_retries: if retry.enabled { retry.max_retries } else { 0 },
        base_delay_ms: retry.base_delay_ms,
        max_delay_ms: retry.max_agent_delay_ms,
    }
}

/// The options every tool runs with. `bash_env` is what a bash command
/// learns about its session: [`bash_session_env`] for a call the model
/// makes, nothing for a command the user types (`/bash`, `!`, `!!`).
fn tool_runtime_options(
    runtime: &Runtime,
    bash_env: Vec<(String, Option<String>)>,
) -> ToolRuntimeOptions {
    ToolRuntimeOptions {
        shell_path: runtime.systems.shell_path.clone(),
        shell_command_prefix: runtime.systems.shell_command_prefix.clone(),
        offline: runtime.systems.offline,
        bash_env,
        auto_resize_images: runtime.systems.auto_resize_images,
        block_images: runtime.systems.block_images,
    }
}

/// The images of a tool result as message attachments.
fn tool_result_media(images: Vec<noel_tools::ToolImage>) -> Vec<MediaInput> {
    images
        .into_iter()
        .map(|image| MediaInput {
            mime_type: image.mime_type,
            data_base64: image.data_base64,
            path: None,
            width: None,
            height: None,
        })
        .collect()
}

/// The tools offered to the model. An extension tool with a built-in tool's
/// name replaces it: the model is sent one definition per name, the one
/// that runs.
fn active_tool_definitions(runtime: &Runtime) -> Vec<AiToolDefinition> {
    let definitions = enabled_tool_definitions(runtime);
    if !model_tool_enabled(runtime, codemode::CODEMODE) {
        return definitions;
    }
    codemode::present(
        definitions,
        &tool_guidelines(runtime),
        runtime.systems.codemode,
    )
}

/// The prompt guidelines of each active tool, by name.
fn tool_guidelines(runtime: &Runtime) -> BTreeMap<String, Vec<String>> {
    prompt_tools(runtime)
        .into_iter()
        .map(|tool| (tool.name, tool.guidelines))
        .collect()
}

/// The enabled tools as they are, before codemode's presentation.
fn enabled_tool_definitions(runtime: &Runtime) -> Vec<AiToolDefinition> {
    let agents = runtime.systems.agents.clone();
    let mut definitions = builtin_tool_definitions()
        .into_iter()
        .filter(|tool| !runtime.systems.extension_tools.contains_key(&tool.name))
        .filter(|tool| model_tool_enabled(runtime, &tool.name))
        .filter_map(|tool| model_tool_definition(&tool.name, &agents))
        .collect::<Vec<_>>();
    definitions.extend(
        runtime
            .systems
            .extension_tools
            .values()
            .filter(|tool| model_tool_enabled(runtime, &tool.name))
            .map(|tool| AiToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            }),
    );
    definitions.extend(
        mcp::tool_definitions(&runtime.systems.mcp_tools)
            .into_iter()
            .filter(|tool| model_tool_enabled(runtime, &tool.name)),
    );
    definitions
}

/// `model_tool_enabled` for tests of plan mode's tool blocking.
#[cfg(test)]
pub(crate) fn model_tool_enabled_for_tests(runtime: &Runtime, name: &str) -> bool {
    model_tool_enabled(runtime, name)
}

fn model_tool_enabled(runtime: &Runtime, name: &str) -> bool {
    let mcp_offered = name.starts_with("mcp__")
        && runtime
            .systems
            .mcp_tools
            .iter()
            .any(|tool| tool.name == name);
    if name == codemode::CODEMODE {
        return codemode::selected(
            &runtime.systems.available_tool_names,
            &runtime.session.disabled_tools,
        );
    }
    (runtime.session.active_tool_names.contains(name)
        || runtime.systems.extension_tools.contains_key(name)
        // MCP tools exist only through the overlay; the session's active
        // tools (the built-ins) never name them. `--tools`/`enabledTools`
        // scoping still applies through the available-name checks below.
        || mcp_offered)
        && !runtime.session.disabled_tools.contains(name)
        && !(runtime.systems.offline && matches!(name, "web_search" | "web_fetch"))
        // Plan mode (spec §9.4): research only, so the tools that change
        // files never run; `bash` stays (the system prompt says read-only)
        // and every read tool stays.
        && !(runtime.session.plan_mode && matches!(name, "edit" | "write"))
        && ((runtime.systems.available_tool_names.is_empty()
            && !runtime.systems.tool_scope_configured)
            || runtime.systems.available_tool_names.contains(name))
}

/// How long a command the user runs (`!` and `/bash`) may take. Unlike a
/// model's bash call, which has no default timeout, these have no way to
/// set one and cannot be interrupted individually.
const USER_BASH_TIMEOUT_MS: u64 = 120_000;

pub async fn run_excluded_bash(runtime: &Runtime, command: String) -> Result<String, AgentError> {
    Ok(run_excluded_bash_result(runtime, command).await?.output)
}

pub async fn run_excluded_bash_result(
    runtime: &Runtime,
    command: String,
) -> Result<noel_tools::ToolResult, AgentError> {
    if !model_tool_enabled(runtime, "bash") {
        return Err(AgentError::DisabledTool("bash".to_string()));
    }
    let env = runtime
        .tool_env()
        .map_err(|message| AgentError::Tool(ToolError::Failed(message)))?;
    execute_tool_in(
        env.as_ref(),
        ToolRequest::Bash {
            command,
            timeout_ms: Some(USER_BASH_TIMEOUT_MS),
        },
        &tool_runtime_options(runtime, Vec::new()),
    )
    .await
    .map_err(Into::into)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedToolCommand {
    name: String,
    request: ToolRequest,
}

/// Whether `prompt` is a slash-tool command (`/read path`, ...), which runs
/// the tool directly instead of asking the model; a malformed one counts.
pub fn is_tool_command(prompt: &str) -> bool {
    !matches!(parse_tool_command(prompt), Ok(None))
}

fn parse_tool_command(prompt: &str) -> Result<Option<ParsedToolCommand>, AgentError> {
    let trimmed = prompt.trim();
    if !trimmed.starts_with('/') {
        return Ok(None);
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match command {
        "/web-search" => Ok(Some(ParsedToolCommand {
            name: "web_search".to_string(),
            request: ToolRequest::WebSearch {
                query: required_arg(command, rest)?.to_string(),
                limit: None,
            },
        })),
        "/web-fetch" => Ok(Some(ParsedToolCommand {
            name: "web_fetch".to_string(),
            request: ToolRequest::WebFetch {
                url: required_arg(command, rest)?.to_string(),
                max_chars: None,
            },
        })),
        "/read" => {
            // "/read <path> [offset=N] [limit=N]": the resume syntax printed
            // by the read truncation notice.
            let (path, rest) = split_optional_arg(rest);
            let mut offset = None;
            let mut limit = None;
            for part in rest.unwrap_or_default().split_whitespace() {
                if let Some(value) = part.strip_prefix("offset=") {
                    offset = value.parse::<u64>().ok();
                } else if let Some(value) = part.strip_prefix("limit=") {
                    limit = value.parse::<u64>().ok();
                }
            }
            Ok(Some(ParsedToolCommand {
                name: "read".to_string(),
                request: ToolRequest::Read {
                    path: required_arg(command, path)?.to_string(),
                    offset,
                    limit,
                },
            }))
        }
        "/bash" => Ok(Some(ParsedToolCommand {
            name: "bash".to_string(),
            request: ToolRequest::Bash {
                command: required_arg(command, rest)?.to_string(),
                timeout_ms: Some(USER_BASH_TIMEOUT_MS),
            },
        })),
        "/write" => {
            let (path, content) = split_once_arg(command, rest)?;
            Ok(Some(ParsedToolCommand {
                name: "write".to_string(),
                request: ToolRequest::Write {
                    path: path.to_string(),
                    content: content.to_string(),
                },
            }))
        }
        "/edit" => {
            let (path, rest) = split_once_arg(command, rest)?;
            let (find, replace) = split_once_arg(command, rest)?;
            Ok(Some(ParsedToolCommand {
                name: "edit".to_string(),
                request: ToolRequest::Edit {
                    path: path.to_string(),
                    edits: vec![noel_tools::TextEdit {
                        find: find.to_string(),
                        replace: replace.to_string(),
                    }],
                },
            }))
        }
        "/grep" => {
            let (pattern, path) = split_optional_arg(rest);
            let pattern = required_arg(command, pattern)?;
            Ok(Some(ParsedToolCommand {
                name: "grep".to_string(),
                request: ToolRequest::Grep {
                    path: path.map(ToString::to_string),
                    pattern: pattern.to_string(),
                    glob: None,
                    literal: None,
                    ignore_case: None,
                    context: None,
                    limit: None,
                },
            }))
        }
        "/find" => Ok(Some(ParsedToolCommand {
            name: "find".to_string(),
            request: ToolRequest::Find {
                pattern: required_arg(command, rest)?.to_string(),
                path: None,
                limit: None,
            },
        })),
        "/ls" => Ok(Some(ParsedToolCommand {
            name: "ls".to_string(),
            request: ToolRequest::Ls {
                path: if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                },
                limit: None,
            },
        })),
        // As upstream, any other text is for the model, whatever it starts
        // with: a path, or a command that names nothing.
        _ => Ok(None),
    }
}

fn required_arg<'a>(command: &str, value: &'a str) -> Result<&'a str, AgentError> {
    if value.is_empty() {
        Err(AgentError::MissingCommandArgument(command.to_string()))
    } else {
        Ok(value)
    }
}

fn split_once_arg<'a>(command: &str, value: &'a str) -> Result<(&'a str, &'a str), AgentError> {
    let mut parts = value.splitn(2, char::is_whitespace);
    let first = required_arg(command, parts.next().unwrap_or_default())?;
    let rest = required_arg(command, parts.next().unwrap_or_default().trim())?;
    Ok((first, rest))
}

fn split_optional_arg(value: &str) -> (&str, Option<&str>) {
    let mut parts = value.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default();
    let rest = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    (first, rest)
}

pub fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis}-{}-{counter}", std::process::id())
}

#[cfg(test)]
pub(crate) mod tests;

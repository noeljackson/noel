//! Upstream pi's RPC records (`packages/coding-agent/src/modes/rpc` and
//! `modes/json-event.ts`): the commands read from stdin, the responses and
//! session events written to stdout, and how pi's messages and turn events
//! map onto them. Fields keep upstream's order.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pi_ai::{ChatToolCall, ModelRef, StreamEvent};
use pi_config::{model_context_window, LoadedConfig, ProviderApi};
use pi_core::{
    CompactionRecord, ConversationMessage, MessageRole, Run, RunStatus, Runtime, SessionState,
    SteeringMailbox, ToolEvent, TurnEvent, STEERING_JOIN,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The upstream additions this RPC mode speaks, in `get_state`.
pub(crate) const RPC_EXTENSIONS: [&str; 4] =
    ["run-id", "expected-run-id", "request-id", "resume-run"];

/// Upstream's reply to a prompt while a run is active.
pub(crate) const BUSY: &str = "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.";

/// The reply to a queued prompt with a `requestId`, which only names runs.
pub(crate) const QUEUED_REQUEST: &str =
    "a prompt with requestId starts a run; send it when no run is active";

/// Upstream's error text for an aborted model request.
const ABORTED: &str = "Request was aborted";

/// Upstream commands pi does not implement; any other type is unknown.
const UNSUPPORTED: &[&str] = &[
    "abort_bash",
    "abort_retry",
    "bash",
    "clone",
    "cycle_model",
    "cycle_thinking_level",
    "export_html",
    "fork",
    "get_available_models",
    "get_available_thinking_levels",
    "get_commands",
    "get_entries",
    "get_fork_messages",
    "get_session_stats",
    "get_tree",
    "set_auto_compaction",
    "set_auto_retry",
];

// Commands.

/// One stdin line that parsed as JSON.
pub(crate) struct Request {
    /// Echoed in the response whatever its JSON type, as upstream does.
    pub id: Option<Value>,
    /// The command's `type`, which names it in the response.
    pub kind: Option<String>,
    /// The command, or why it is not one pi can run.
    pub command: Result<Command, String>,
}

impl Request {
    /// Parses one line; the error is upstream's parse response text.
    pub fn parse(line: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("Failed to parse command: {error}"))?;
        let id = value.get("id").cloned();
        let kind = value.get("type").and_then(Value::as_str).map(str::to_owned);
        let command = serde_json::from_value(value).map_err(|error| match &kind {
            Some(kind) => format!("Invalid {kind} command: {error}"),
            None => format!("Invalid command: {error}"),
        });
        Ok(Self { id, kind, command })
    }

    /// The reply to a type pi does not handle.
    pub fn unknown(&self) -> Reply {
        let kind = self.kind.as_deref().unwrap_or_default();
        Err(if UNSUPPORTED.contains(&kind) {
            format!("Unsupported command: {kind}")
        } else {
            format!("Unknown command: {kind}")
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Command {
    Prompt(Input),
    Steer(Input),
    FollowUp(Input),
    Abort {
        expected_run_id: Option<String>,
    },
    ResumeRun {
        expected_run_id: String,
    },
    ClearQueue,
    NewSession {
        parent_session: Option<String>,
    },
    SwitchSession {
        session_path: String,
    },
    GetState,
    GetMessages,
    GetLastAssistantText,
    SetModel {
        provider: String,
        model_id: String,
    },
    SetThinkingLevel {
        level: String,
    },
    SetSteeringMode {
        mode: QueueMode,
    },
    SetFollowUpMode {
        mode: QueueMode,
    },
    Compact {
        custom_instructions: Option<String>,
    },
    SetSessionName {
        name: String,
    },
    #[serde(other)]
    Other,
}

/// The message of a `prompt`, `steer` or `follow_up`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Input {
    pub message: String,
    #[serde(default)]
    pub images: Vec<Image>,
    /// How a prompt sent during a run is queued.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// The run a queued message is meant for.
    pub expected_run_id: Option<String>,
    /// The client's id for a prompt: a prompt that repeats it gets the run
    /// the first one started.
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Image {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum QueueMode {
    All,
    OneAtATime,
}

impl QueueMode {
    /// pi delivers everything queued at once (see the non-parity register's
    /// "Batched Queue and Steering Dispatch"), so only `all` is accepted.
    pub fn reply(self) -> Reply {
        match self {
            QueueMode::All => Ok(None),
            QueueMode::OneAtATime => {
                Err("pi delivers queued messages together; only \"all\" is supported".into())
            }
        }
    }
}

// Responses.

/// A command's outcome: its data (some commands have none) or an error.
pub(crate) type Reply = Result<Option<Data>, String>;

#[derive(Serialize)]
pub(crate) struct Response<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a Value>,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Data>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl<'a> Response<'a> {
    pub fn new(id: Option<&'a Value>, command: Option<&'a str>, reply: Reply) -> Self {
        let (success, data, error) = match reply {
            Ok(data) => (true, data, None),
            Err(error) => (false, None, Some(error)),
        };
        Self {
            id,
            kind: "response",
            command,
            success,
            data,
            error,
        }
    }

    pub fn to(request: &'a Request, reply: Reply) -> Self {
        Self::new(request.id.as_ref(), request.kind.as_deref(), reply)
    }

    /// A line that is not JSON: no id, and the command `parse`.
    pub fn parse_error(error: String) -> Self {
        Self::new(None, Some("parse"), Err(error))
    }
}

#[derive(Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub(crate) enum Data {
    Disposition {
        disposition: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    State(Box<State>),
    Messages {
        messages: Vec<Message>,
    },
    Text {
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Queue {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    Model(Box<Model>),
    Cancelled {
        cancelled: bool,
    },
    Compaction(Compaction),
}

impl Data {
    pub fn started(run_id: &str) -> Self {
        Data::Disposition {
            disposition: "started",
            run_id: Some(run_id.to_owned()),
        }
    }

    pub fn queued() -> Self {
        Data::Disposition {
            disposition: "queued",
            run_id: None,
        }
    }

    /// The reply to a prompt that repeats the `requestId` of `run`, which
    /// starts nothing: `started` until the run has finished, `handled`
    /// after.
    pub fn repeated(run: &Run) -> Self {
        Data::Disposition {
            disposition: if run.status.is_finished() {
                "handled"
            } else {
                "started"
            },
            run_id: Some(run.id.clone()),
        }
    }

    /// `get_messages`: the conversation as saved.
    pub fn messages(session: &SessionState) -> Self {
        Data::Messages {
            messages: session
                .messages
                .iter()
                .map(|message| Message::saved(message, session))
                .collect(),
        }
    }

    /// `get_last_assistant_text`: the latest reply's text, if it has any.
    pub fn last_assistant_text(session: &SessionState) -> Self {
        let text = session
            .messages
            .iter()
            .rfind(|message| message.role == MessageRole::Assistant)
            .map(|message| message.content.trim())
            .filter(|text| !text.is_empty());
        Data::Text {
            text: text.map(str::to_owned),
        }
    }
}

/// `get_state`'s data, with the extensions after upstream's fields.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub thinking_level: String,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: &'static str,
    pub follow_up_mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
    pub rpc_extensions: [&'static str; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
}

/// `get_state.lastRun`: the session's last run, and for an interrupted run
/// what continuing it starts with.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastRun {
    run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    status: RunStatus,
    /// `model` when the saved context needs a model request, `tools` when
    /// the last reply has tool calls without results.
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pending_tools: Vec<PendingTool>,
}

/// A tool call an interrupted run has no result for. Continuing the run
/// runs it again only if it is `replaySafe`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingTool {
    id: String,
    name: String,
    replay_safe: bool,
}

impl LastRun {
    pub fn of(runtime: &Runtime) -> Option<Self> {
        let session = runtime.session();
        let run = session.runs.last()?;
        let mut last = Self {
            run_id: run.id.clone(),
            request_id: run.request_id.clone(),
            status: run.status,
            phase: None,
            pending_tools: Vec::new(),
        };
        if run.status == RunStatus::Interrupted {
            if let Some((_, calls)) = session.pending_tool_calls() {
                last.phase = Some("tools");
                last.pending_tools = calls
                    .into_iter()
                    .map(|call| PendingTool {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        replay_safe: runtime.replay_safe(&call.name),
                    })
                    .collect();
            } else if session.has_unfinished_turn() {
                last.phase = Some("model");
            }
        }
        Some(last)
    }
}

/// Upstream's model object, with the fields pi's model config has.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Model {
    pub id: String,
    pub name: String,
    pub api: ProviderApi,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub reasoning: bool,
    pub context_window: u64,
}

impl Model {
    /// A configured model; `None` if `model` is not in the config.
    pub fn configured(config: &LoadedConfig, model: &ModelRef) -> Option<Self> {
        let definition = config
            .models
            .iter()
            .find(|candidate| candidate.provider == model.provider && candidate.id == model.id)?;
        Some(Self {
            id: definition.id.clone(),
            name: definition
                .name
                .clone()
                .unwrap_or_else(|| definition.id.clone()),
            api: definition.api.clone(),
            provider: definition.provider.clone(),
            base_url: definition.base_url.clone(),
            reasoning: !crate::model_thinking_levels(model).is_empty(),
            context_window: model_context_window(definition),
        })
    }
}

/// A compaction's result: pi's checkpoint summary and what it replaced.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Compaction {
    summary: String,
    details: CompactionDetails,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactionDetails {
    omitted_messages: usize,
    retained_messages: usize,
}

impl From<&CompactionRecord> for Compaction {
    fn from(record: &CompactionRecord) -> Self {
        Self {
            summary: record.summary.clone(),
            details: CompactionDetails {
                omitted_messages: record.omitted_messages,
                retained_messages: record.retained_messages,
            },
        }
    }
}

// Messages.

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "role",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Message {
    System {
        content: String,
    },
    User {
        content: Vec<Content>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
    Assistant(Assistant),
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<Content>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
}

impl Message {
    /// A user message: its text, then its images.
    fn user(message: &ConversationMessage, timestamp: Option<u64>) -> Self {
        let mut content = vec![Content::text(&message.content)];
        content.extend(message.media.iter().map(|media| Content::Image {
            data: media.data_base64.clone(),
            mime_type: media.mime_type.clone(),
        }));
        Message::User { content, timestamp }
    }

    /// A saved reply: its thinking, text and tool calls, and how it stopped.
    fn reply(message: &ConversationMessage) -> Self {
        let mut content = Vec::new();
        if !message.thinking.is_empty() {
            content.push(Content::Thinking {
                thinking: message.thinking.clone(),
            });
        }
        if !message.content.is_empty() {
            content.push(Content::text(&message.content));
        }
        content.extend(message.tool_calls.iter().map(Content::tool_call));
        Message::Assistant(Assistant {
            content,
            stop_reason: stop_reason(None, !message.tool_calls.is_empty()),
            ..Assistant::default()
        })
    }

    /// A message as the session saved it. The journal keeps no timestamps,
    /// usage, model or timings, so those fields are left out.
    pub fn saved(message: &ConversationMessage, session: &SessionState) -> Self {
        match message.role {
            MessageRole::System => Message::System {
                content: message.content.clone(),
            },
            MessageRole::User => Message::user(message, None),
            MessageRole::Assistant => Message::reply(message),
            MessageRole::Tool => {
                let event = tool_event(session, message);
                Message::ToolResult {
                    tool_call_id: message
                        .tool_call_id
                        .clone()
                        .or_else(|| event.map(|event| event.id.clone()))
                        .unwrap_or_default(),
                    tool_name: message.tool_name.clone().unwrap_or_default(),
                    content: vec![Content::text(&message.content)],
                    is_error: event.is_some_and(|event| event.is_error),
                    duration_ms: None,
                    timestamp: None,
                }
            }
        }
    }
}

/// The run of a saved tool result: by call id, or for a tool command run
/// without the model (`/bash ls`), the latest run of that tool with this
/// output.
fn tool_event<'a>(
    session: &'a SessionState,
    message: &ConversationMessage,
) -> Option<&'a ToolEvent> {
    session
        .tool_history
        .iter()
        .rev()
        .find(|event| match &message.tool_call_id {
            Some(id) => &event.id == id,
            None => {
                message.tool_name.as_ref() == Some(&event.name) && event.result == message.content
            }
        })
}

/// An assistant message. Streamed replies carry the run's model and usage;
/// a saved reply only its content and how it stopped.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Assistant {
    content: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api: Option<ProviderApi>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
    stop_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Content {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

impl Content {
    fn text(text: &str) -> Self {
        Content::Text {
            text: text.to_owned(),
        }
    }

    fn tool_call(call: &ChatToolCall) -> Self {
        Content::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: parse_arguments(&call.arguments),
        }
    }

    /// The kind and text of a text or thinking block.
    fn streamed(&self) -> Option<(Block, &str)> {
        match self {
            Content::Text { text } => Some((Block::Text, text)),
            Content::Thinking { thinking } => Some((Block::Thinking, thinking)),
            Content::Image { .. } | Content::ToolCall { .. } => None,
        }
    }
}

/// A content block that streams in deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Text,
    Thinking,
}

impl Block {
    fn open(self) -> Content {
        match self {
            Block::Text => Content::text(""),
            Block::Thinking => Content::Thinking {
                thinking: String::new(),
            },
        }
    }

    fn start(self, content_index: usize) -> AssistantEvent<'static> {
        match self {
            Block::Text => AssistantEvent::TextStart { content_index },
            Block::Thinking => AssistantEvent::ThinkingStart { content_index },
        }
    }

    fn delta(self, content_index: usize, delta: &str) -> AssistantEvent<'_> {
        match self {
            Block::Text => AssistantEvent::TextDelta {
                content_index,
                delta,
            },
            Block::Thinking => AssistantEvent::ThinkingDelta {
                content_index,
                delta,
            },
        }
    }

    fn end(self, content_index: usize, content: &str) -> AssistantEvent<'_> {
        match self {
            Block::Text => AssistantEvent::TextEnd {
                content_index,
                content,
            },
            Block::Thinking => AssistantEvent::ThinkingEnd {
                content_index,
                content,
            },
        }
    }
}

/// Token usage. Providers report input and output tokens to pi; cache
/// tokens and costs are not tracked and stay zero.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total_tokens: u64,
    cost: Cost,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Cost {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total: u64,
}

impl Usage {
    fn new(input: u64, output: u64) -> Self {
        Self {
            input,
            output,
            total_tokens: input.saturating_add(output),
            ..Self::default()
        }
    }
}

// Events.

#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Event<'a> {
    AgentStart {
        run_id: &'a str,
    },
    AgentEnd {
        messages: &'a [Message],
        will_retry: bool,
    },
    AgentSettled {
        aborted: bool,
        run_id: &'a str,
    },
    TurnStart,
    TurnEnd {
        message: &'a Message,
        tool_results: &'a [Message],
    },
    MessageStart {
        message: &'a Message,
    },
    MessageUpdate {
        usage: Usage,
        assistant_message_event: AssistantEvent<'a>,
    },
    MessageEnd {
        message: &'a Message,
    },
    ToolExecutionStart {
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: Value,
    },
    ToolExecutionEnd {
        tool_call_id: &'a str,
        tool_name: &'a str,
        result: ExecutionResult<'a>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: &'a [String],
    },
    AutoRetryStart {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error_message: &'a str,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_error: Option<&'a str>,
    },
    CompactionStart {
        reason: &'static str,
    },
    CompactionEnd {
        reason: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Compaction>,
        aborted: bool,
        will_retry: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<&'a str>,
    },
    SessionInfoChanged {
        name: &'a str,
    },
    ThinkingLevelChanged {
        level: &'a str,
    },
}

/// `message_update`'s event: a content block's start, delta or end.
#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum AssistantEvent<'a> {
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: &'a str,
    },
    TextEnd {
        content_index: usize,
        content: &'a str,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: &'a str,
    },
    ThinkingEnd {
        content_index: usize,
        content: &'a str,
    },
    ToolcallStart {
        content_index: usize,
        id: &'a str,
        tool_name: &'a str,
    },
    ToolcallDelta {
        content_index: usize,
        delta: &'a str,
    },
    ToolcallEnd {
        content_index: usize,
        tool_call: &'a Content,
    },
}

/// `tool_execution_end`'s result: the tool's output as text.
#[derive(Serialize)]
pub(crate) struct ExecutionResult<'a> {
    content: &'a [Content],
}

/// Where records go, in order.
pub(crate) type Sink<'s> = &'s mut dyn FnMut(Event<'_>);

// The queue.

/// Messages waiting for the agent. Steering joins a run's next provider
/// round; follow-ups start another turn when the run would end. Both are
/// delivered batched, like the TUI's queue.
#[derive(Default)]
pub(crate) struct Queue {
    pub steering: SteeringMailbox,
    pub follow_ups: Vec<String>,
}

/// A queue's steering and follow-ups.
pub(crate) type Contents = (Vec<String>, Vec<String>);

impl Queue {
    pub fn pending(&self) -> usize {
        self.steering.pending() + self.follow_ups.len()
    }

    pub fn update(&self) -> Event<'_> {
        Event::QueueUpdate {
            steering: self.steering.list(),
            follow_up: &self.follow_ups,
        }
    }

    pub fn contents(&self) -> Contents {
        (self.steering.list(), self.follow_ups.clone())
    }

    /// Makes `contents` the queue. The steering mailbox stays the one the
    /// active run reads.
    pub fn replace(&mut self, (steering, follow_ups): Contents) {
        self.steering.drain();
        for message in steering {
            self.steering.send(message);
        }
        self.follow_ups = follow_ups;
    }

    /// The queued follow-ups as one prompt.
    pub fn take_follow_ups(&mut self) -> Option<String> {
        (!self.follow_ups.is_empty())
            .then(|| std::mem::take(&mut self.follow_ups).join(STEERING_JOIN))
    }
}

// Runs.

/// What a run's assistant messages name.
pub(crate) struct Source {
    pub api: ProviderApi,
    pub provider: String,
    pub model: String,
    pub thinking_level: String,
}

/// What the next engine call of a run reports first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prompt {
    /// The prompt that started the run.
    Initial,
    /// Follow-ups taken from the queue, announced by a queue_update.
    FollowUps,
}

/// How a run ended.
pub(crate) enum Outcome {
    Completed,
    Aborted,
    Failed(String),
}

impl Outcome {
    pub fn status(&self) -> RunStatus {
        match self {
            Outcome::Completed => RunStatus::Completed,
            Outcome::Aborted => RunStatus::Aborted,
            Outcome::Failed(_) => RunStatus::Failed,
        }
    }
}

/// Writes one run's records from the engine's events. A run is one upstream
/// run whatever its engine calls (one per batch of follow-ups): one
/// agent_start, its turns, then one agent_end and one agent_settled.
pub(crate) struct Recorder {
    run_id: String,
    source: Source,
    /// The run's messages, for agent_end.
    messages: Vec<Message>,
    prompt: Option<Prompt>,
    /// Whether the current engine call reported a round.
    reported_round: bool,
    round: Option<Round>,
    reply: Option<Streaming>,
    tool: Option<ChatToolCall>,
    retry: Option<u64>,
    compacting: bool,
}

/// An open turn: when it started and where its reply is in the run's
/// messages. The reply's tool results follow it there.
struct Round {
    started: Instant,
    reply: Option<usize>,
}

/// The assistant message streaming in, and its block taking deltas.
struct Streaming {
    assistant: Assistant,
    open: Option<usize>,
}

impl Recorder {
    pub fn start(run_id: String, source: Source, sink: Sink) -> Self {
        sink(Event::AgentStart { run_id: &run_id });
        Self {
            run_id,
            source,
            messages: Vec::new(),
            prompt: None,
            reported_round: false,
            round: None,
            reply: None,
            tool: None,
            retry: None,
            compacting: false,
        }
    }

    pub fn id(&self) -> &str {
        &self.run_id
    }

    pub fn compacting(&self) -> bool {
        self.compacting
    }

    /// An engine call starts with `prompt`, or continues a stopped run.
    pub fn call(&mut self, prompt: Option<Prompt>) {
        self.prompt = prompt;
        self.reported_round = false;
    }

    pub fn record(&mut self, event: &TurnEvent, queue: &Queue, sink: Sink) {
        match event {
            TurnEvent::RoundStarted => {
                self.reported_round = true;
                self.start_round(sink);
            }
            // The saved reply is reported again, as saved, so the turn's
            // records end with messages they reported.
            TurnEvent::RoundResumed(reply) => {
                self.reported_round = true;
                self.start_round(sink);
                self.add(Message::reply(reply), sink);
                if let Some(round) = &mut self.round {
                    round.reply = Some(self.messages.len() - 1);
                }
            }
            TurnEvent::MessageAdded(message) if message.role == MessageRole::Assistant => {
                let raw = self
                    .reply
                    .as_ref()
                    .and_then(|reply| reply.assistant.raw_stop_reason.as_deref());
                let stop = stop_reason(raw, !message.tool_calls.is_empty());
                self.end_reply(stop, None, sink);
            }
            TurnEvent::MessageAdded(message) => self.add_user(message, queue, sink),
            TurnEvent::Provider(event) => self.stream(event, sink),
            TurnEvent::RetryStarted(notice) => {
                self.retry = Some(notice.attempt);
                sink(Event::AutoRetryStart {
                    attempt: notice.attempt,
                    max_attempts: notice.max_attempts,
                    delay_ms: notice.delay_ms,
                    error_message: &notice.error_message,
                });
            }
            TurnEvent::RetryFinished { attempt, error } => {
                self.retry = None;
                sink(Event::AutoRetryEnd {
                    success: error.is_none(),
                    attempt: *attempt,
                    final_error: error.as_deref(),
                });
            }
            TurnEvent::ToolStarted { call } => {
                sink(Event::ToolExecutionStart {
                    tool_call_id: &call.id,
                    tool_name: &call.name,
                    args: parse_arguments(&call.arguments),
                });
                self.tool = Some(call.clone());
            }
            TurnEvent::ToolOutput {
                call,
                output,
                is_error,
                duration,
            } => self.tool_output(call, output, *is_error, Some(millis(*duration)), sink),
            TurnEvent::CompactionStarted => {
                self.compacting = true;
                sink(Event::CompactionStart {
                    reason: "threshold",
                });
            }
            TurnEvent::CompactionFinished(record) => self.end_compaction(Ok(record), sink),
            TurnEvent::CompactionFailed(error) => self.end_compaction(Err(error), sink),
            TurnEvent::RoundFinished => self.end_round(sink),
            TurnEvent::ToolFinished { .. } | TurnEvent::TodosUpdated(_) => {}
        }
    }

    /// Reports what an engine call saved without reporting: a tool command
    /// (`/bash ls`) runs its tool without the model and reports nothing.
    /// `messages` are those the call added to `session`.
    pub fn unreported(
        &mut self,
        messages: &[ConversationMessage],
        session: &SessionState,
        queue: &Queue,
        sink: Sink,
    ) {
        if self.reported_round {
            return;
        }
        for message in messages {
            match message.role {
                MessageRole::User => self.add_user(message, queue, sink),
                _ => self.add(Message::saved(message, session), sink),
            }
        }
    }

    /// Ends the run: closes what an abort or a failure left open, then
    /// agent_end and agent_settled.
    pub fn finish(mut self, outcome: Outcome, sink: Sink) {
        let aborted = matches!(outcome, Outcome::Aborted);
        match outcome {
            Outcome::Completed => {}
            Outcome::Aborted => self.stop(None, sink),
            Outcome::Failed(error) => self.stop(Some(&error), sink),
        }
        sink(Event::AgentEnd {
            messages: &self.messages,
            will_retry: false,
        });
        sink(Event::AgentSettled {
            aborted,
            run_id: &self.run_id,
        });
    }

    /// Closes what the engine left open when it stopped (`error` is `None`
    /// for an abort), and reports the stop as upstream does: as the reply of
    /// the turn awaiting one, or of a new turn, with no further delivery.
    fn stop(&mut self, error: Option<&str>, sink: Sink) {
        let message = error.unwrap_or(ABORTED);
        if let Some(call) = self.tool.take() {
            self.tool_output(
                &call,
                error.unwrap_or("Operation aborted"),
                true,
                None,
                sink,
            );
        }
        if let Some(attempt) = self.retry.take() {
            sink(Event::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(message),
            });
        }
        if std::mem::take(&mut self.compacting) {
            sink(Event::CompactionEnd {
                reason: "threshold",
                result: None,
                aborted: error.is_none(),
                will_retry: false,
                error_message: error,
            });
        }
        let awaiting_reply = self.reply.is_some()
            || self
                .round
                .as_ref()
                .is_some_and(|round| round.reply.is_none());
        if !awaiting_reply {
            self.end_round(sink);
            self.start_round(sink);
        }
        let stop = if error.is_some() { "error" } else { "aborted" };
        self.end_reply(stop, Some(message), sink);
        self.end_round(sink);
    }

    fn add(&mut self, message: Message, sink: Sink) {
        sink(Event::MessageStart { message: &message });
        sink(Event::MessageEnd { message: &message });
        self.messages.push(message);
    }

    fn add_user(&mut self, message: &ConversationMessage, queue: &Queue, sink: Sink) {
        // Steering and follow-ups leave the queue as they are delivered.
        if self.prompt.take() != Some(Prompt::Initial) {
            sink(queue.update());
        }
        self.add(Message::user(message, Some(now())), sink);
    }

    fn start_round(&mut self, sink: Sink) {
        sink(Event::TurnStart);
        self.round = Some(Round {
            started: Instant::now(),
            reply: None,
        });
    }

    fn end_round(&mut self, sink: Sink) {
        if let Some(Round {
            reply: Some(reply), ..
        }) = self.round.take()
        {
            sink(Event::TurnEnd {
                message: &self.messages[reply],
                tool_results: &self.messages[reply + 1..],
            });
        }
    }

    fn stream(&mut self, event: &StreamEvent, sink: Sink) {
        let reply = match &mut self.reply {
            Some(reply) => reply,
            None => {
                let reply = Streaming::new(&self.source);
                sink(Event::MessageStart {
                    message: &Message::Assistant(reply.assistant.clone()),
                });
                self.reply.insert(reply)
            }
        };
        match event {
            StreamEvent::Text(delta) => reply.delta(Block::Text, delta, sink),
            StreamEvent::Thinking(delta) => reply.delta(Block::Thinking, delta, sink),
            StreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => reply.tool_call(id, name, arguments, sink),
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
            } => reply.assistant.usage = Some(Usage::new(*input_tokens, *output_tokens)),
            StreamEvent::Stop { reason } => reply.assistant.raw_stop_reason = Some(reason.clone()),
        }
    }

    /// Ends the streaming reply, or reports one that streamed nothing.
    fn end_reply(&mut self, stop_reason: &'static str, error: Option<&str>, sink: Sink) {
        let streamed = self.reply.is_some();
        let mut reply = self
            .reply
            .take()
            .unwrap_or_else(|| Streaming::new(&self.source));
        reply.close(sink);
        let assistant = &mut reply.assistant;
        assistant.stop_reason = stop_reason;
        assistant.error_message = error.map(str::to_owned);
        assistant.duration_ms = self
            .round
            .as_ref()
            .map(|round| millis(round.started.elapsed()));
        assistant.thinking_level = Some(self.source.thinking_level.clone());
        let message = Message::Assistant(reply.assistant);
        if !streamed {
            sink(Event::MessageStart { message: &message });
        }
        sink(Event::MessageEnd { message: &message });
        self.messages.push(message);
        if let Some(round) = &mut self.round {
            round.reply = Some(self.messages.len() - 1);
        }
    }

    fn tool_output(
        &mut self,
        call: &ChatToolCall,
        output: &str,
        is_error: bool,
        duration_ms: Option<u64>,
        sink: Sink,
    ) {
        self.tool = None;
        let content = vec![Content::text(output)];
        sink(Event::ToolExecutionEnd {
            tool_call_id: &call.id,
            tool_name: &call.name,
            result: ExecutionResult { content: &content },
            is_error,
            duration_ms,
        });
        self.add(
            Message::ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content,
                is_error,
                duration_ms,
                timestamp: Some(now()),
            },
            sink,
        );
    }

    fn end_compaction(&mut self, result: Result<&CompactionRecord, &String>, sink: Sink) {
        self.compacting = false;
        sink(Event::CompactionEnd {
            reason: "threshold",
            result: result.ok().map(Compaction::from),
            aborted: false,
            will_retry: false,
            error_message: result.err().map(String::as_str),
        });
    }
}

impl Streaming {
    fn new(source: &Source) -> Self {
        Self {
            assistant: Assistant {
                api: Some(source.api.clone()),
                provider: Some(source.provider.clone()),
                model: Some(source.model.clone()),
                usage: Some(Usage::default()),
                stop_reason: "pending",
                timestamp: Some(now()),
                ..Assistant::default()
            },
            open: None,
        }
    }

    fn update(&self, event: AssistantEvent<'_>, sink: Sink) {
        sink(Event::MessageUpdate {
            usage: self.assistant.usage.unwrap_or_default(),
            assistant_message_event: event,
        });
    }

    fn delta(&mut self, block: Block, delta: &str, sink: Sink) {
        let open = self.open.filter(|index| {
            self.assistant.content[*index]
                .streamed()
                .map(|(kind, _)| kind)
                == Some(block)
        });
        let index = match open {
            Some(index) => index,
            None => {
                self.close(sink);
                self.assistant.content.push(block.open());
                let index = self.assistant.content.len() - 1;
                self.open = Some(index);
                self.update(block.start(index), sink);
                index
            }
        };
        if let Content::Text { text } | Content::Thinking { thinking: text } =
            &mut self.assistant.content[index]
        {
            text.push_str(delta);
        }
        self.update(block.delta(index, delta), sink);
    }

    /// Ends the block taking deltas with its whole text.
    fn close(&mut self, sink: Sink) {
        let Some(index) = self.open.take() else {
            return;
        };
        if let Some((block, text)) = self.assistant.content[index].streamed() {
            self.update(block.end(index, text), sink);
        }
    }

    /// Tool calls arrive whole: their start, their arguments, their end.
    fn tool_call(&mut self, id: &str, name: &str, arguments: &str, sink: Sink) {
        self.close(sink);
        let index = self.assistant.content.len();
        self.assistant.content.push(Content::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: parse_arguments(arguments),
        });
        self.update(
            AssistantEvent::ToolcallStart {
                content_index: index,
                id,
                tool_name: name,
            },
            sink,
        );
        self.update(
            AssistantEvent::ToolcallDelta {
                content_index: index,
                delta: arguments,
            },
            sink,
        );
        self.update(
            AssistantEvent::ToolcallEnd {
                content_index: index,
                tool_call: &self.assistant.content[index],
            },
            sink,
        );
    }
}

/// Upstream's stop reason for a reply. A reply with tool calls is `toolUse`
/// whatever the provider's reason: one cut off at a token limit too, whose
/// calls get error results instead of running. Without tool calls, every
/// reason except a token limit is a normal stop. The provider's reason stays
/// in `rawStopReason`.
fn stop_reason(raw: Option<&str>, tool_calls: bool) -> &'static str {
    match raw {
        _ if tool_calls => "toolUse",
        Some("length" | "max_tokens" | "MAX_TOKENS" | "model_context_window_exceeded") => "length",
        _ => "stop",
    }
}

/// Tool-call arguments as JSON; arguments that do not parse read as `{}`,
/// as upstream's streaming parser gives them.
fn parse_arguments(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis)
        .unwrap_or_default()
}

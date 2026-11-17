//! The harness's owned events: what a program can observe, in order,
//! serialisable so it can cross tasks (the plan's "owned, serialisable
//! events").

use serde::{Deserialize, Serialize};

use noel_core::controller::Event;
use noel_core::{ConversationMessage, MessageRole, TurnEvent};

/// How a run ended, as the events carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: RunOutcome,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Whether a run was answered, as its own serialisable form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed,
}

/// One observable event, owned. Durable's hosts watch its session's
/// commits; noel's hosts watch the controller's `Event` stream, and this
/// is its owned form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OwnedEvent {
    /// A run starts, or an interrupted one continues.
    RunStarted {
        run_id: String,
    },
    /// An engine call of the active run starts.
    CallStarted {
        input: CallInput,
    },
    /// A message was saved (user, assistant, tool result).
    MessageAdded {
        role: String,
    },
    /// The model produced output: streamed text or thinking.
    Output {
        kind: &'static str,
    },
    /// A retry of a failed round starts, after its delay.
    RetryStarted,
    /// A tool call started; `tool_ended` reports its result.
    ToolStarted {
        name: String,
    },
    ToolEnded {
        name: String,
        is_error: bool,
    },
    /// The round's reply and its tool results are saved.
    RoundFinished,
    /// A compaction started or finished.
    Compaction {
        ended: bool,
    },
    /// Queued submissions were placed into the active run.
    Placed {
        run_id: String,
        submissions: Vec<String>,
    },
    /// Queued submissions were withdrawn, unanswered.
    Withdrawn {
        submissions: Vec<String>,
    },
    /// The queue changed other than by placing into the run.
    QueueChanged {
        steering: Vec<String>,
        follow_ups: Vec<String>,
    },
    /// A save the controller could not report as a refusal failed.
    SaveFailed {
        error: String,
    },
    /// The run settled; how it ended is saved.
    RunSettled {
        run_id: String,
        outcome: Outcome,
    },
}

/// Which engine call started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallInput {
    Prompt,
    FollowUps,
    Resume,
}

impl OwnedEvent {
    pub(crate) fn save_failed(error: &noel_core::SessionError) -> OwnedEvent {
        OwnedEvent::SaveFailed {
            error: error.to_string(),
        }
    }

    /// A stable name, as goldens record events.
    pub fn name(&self) -> &'static str {
        match self {
            OwnedEvent::RunStarted { .. } => "run_started",
            OwnedEvent::CallStarted { .. } => "call_started",
            OwnedEvent::MessageAdded { .. } => "message_added",
            OwnedEvent::Output { .. } => "output",
            OwnedEvent::RetryStarted => "retry_started",
            OwnedEvent::ToolStarted { .. } => "tool_started",
            OwnedEvent::ToolEnded { .. } => "tool_ended",
            OwnedEvent::RoundFinished => "round_finished",
            OwnedEvent::Compaction { .. } => "compaction",
            OwnedEvent::Placed { .. } => "placed",
            OwnedEvent::Withdrawn { .. } => "withdrawn",
            OwnedEvent::QueueChanged { .. } => "queue_changed",
            OwnedEvent::SaveFailed { .. } => "save_failed",
            OwnedEvent::RunSettled { .. } => "run_settled",
        }
    }
}

/// The events of one harness: a host takes them in order after each
/// operation, or subscribes and receives them as they happen.
pub struct Events {
    log: std::sync::Mutex<Vec<OwnedEvent>>,
    subscribers: std::sync::Mutex<Vec<tokio::sync::mpsc::UnboundedSender<OwnedEvent>>>,
}

impl Events {
    pub(crate) fn new() -> Events {
        Events {
            log: std::sync::Mutex::new(Vec::new()),
            subscribers: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn record(&self, event: OwnedEvent) {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event.clone());
        self.subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|sender| sender.send(event.clone()).is_ok());
    }

    pub(crate) fn take(&self) -> Vec<OwnedEvent> {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *log)
    }

    /// The log as it is, without taking it.
    pub(crate) fn peek(&self) -> Vec<OwnedEvent> {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn subscribe(&self) -> Subscription {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sender);
        Subscription { receiver }
    }
}

/// A live feed of a harness's events.
pub struct Subscription {
    receiver: tokio::sync::mpsc::UnboundedReceiver<OwnedEvent>,
}

impl Subscription {
    /// The next event already recorded, without waiting.
    pub fn try_next(&mut self) -> Result<OwnedEvent, ()> {
        self.receiver.try_recv().map_err(|_| ())
    }

    /// The next event, or `None` when the harness is closed.
    pub async fn next(&mut self) -> Option<OwnedEvent> {
        self.receiver.recv().await
    }
}

/// Records the controller's borrowed `Event`s as owned ones.
pub(crate) fn record(events: &Events, event: Event<'_>) {
    let owned = match event {
        Event::RunStarted { run_id, .. } => OwnedEvent::RunStarted {
            run_id: run_id.to_owned(),
        },
        Event::CallStarted { input } => OwnedEvent::CallStarted {
            input: match input {
                noel_core::controller::CallInput::Prompt => CallInput::Prompt,
                noel_core::controller::CallInput::FollowUps => CallInput::FollowUps,
                noel_core::controller::CallInput::Resume => CallInput::Resume,
            },
        },
        Event::Turn { event, .. } => turn_event(event),
        // Messages the call saved without reporting them (a saved prompt).
        Event::CallReturned { added, .. } => last_message(added),
        Event::Placed {
            run_id,
            submissions,
        } => OwnedEvent::Placed {
            run_id: run_id.to_owned(),
            submissions: submissions
                .iter()
                .map(|id| id.submission_id.clone())
                .collect(),
        },
        Event::Withdrawn { submissions } => OwnedEvent::Withdrawn {
            submissions: submissions
                .iter()
                .map(|item| item.id.submission_id.clone())
                .collect(),
        },
        Event::QueueChanged { queue } => OwnedEvent::QueueChanged {
            steering: queue.steering(),
            follow_ups: queue.follow_ups(),
        },
        Event::CompactionStarted => OwnedEvent::Compaction { ended: false },
        Event::CompactionEnded { .. } => OwnedEvent::Compaction { ended: true },
        Event::SaveFailed { error } => OwnedEvent::save_failed(error),
        Event::RunSettled { run_id, outcome } => OwnedEvent::RunSettled {
            run_id: run_id.to_owned(),
            outcome: Outcome {
                status: match outcome.status() {
                    noel_core::RunStatus::Completed => RunOutcome::Completed,
                    noel_core::RunStatus::Aborted => RunOutcome::Aborted,
                    _ => RunOutcome::Failed,
                },
                detail: match outcome {
                    noel_core::controller::Outcome::Failed(detail) => detail.clone(),
                    noel_core::controller::Outcome::Unanswered { detail, .. } => detail.clone(),
                    _ => String::new(),
                },
            },
        },
    };
    events.record(owned);
}

fn last_message(added: &[ConversationMessage]) -> OwnedEvent {
    match added.last() {
        Some(message) => OwnedEvent::MessageAdded {
            role: role(&message.role),
        },
        None => OwnedEvent::RoundFinished,
    }
}

fn role(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".into(),
        MessageRole::Assistant => "assistant".into(),
        MessageRole::Tool => "toolResult".into(),
        MessageRole::System => "system".into(),
    }
}

fn turn_event(event: &TurnEvent) -> OwnedEvent {
    match event {
        TurnEvent::RoundStarted | TurnEvent::RoundResumed(_) => OwnedEvent::RoundFinished,
        TurnEvent::MessageAdded(message) => OwnedEvent::MessageAdded {
            role: role(&message.role),
        },
        TurnEvent::Provider(noel_ai::StreamEvent::Text(text)) => OwnedEvent::Output {
            kind: if text.is_empty() { "empty" } else { "text" },
        },
        TurnEvent::Provider(noel_ai::StreamEvent::Thinking(_)) => {
            OwnedEvent::Output { kind: "thinking" }
        }
        TurnEvent::Provider(_) => OwnedEvent::Output { kind: "other" },
        TurnEvent::RetryStarted(_) => OwnedEvent::RetryStarted,
        TurnEvent::RetryFinished { .. } | TurnEvent::Overflowed { .. } => OwnedEvent::RetryStarted,
        TurnEvent::ToolStarted { call } => OwnedEvent::ToolStarted {
            name: call.name.clone(),
        },
        TurnEvent::ToolUpdate { .. } => OwnedEvent::RoundFinished,
        TurnEvent::ToolFinished { name, is_error } => OwnedEvent::ToolEnded {
            name: name.clone(),
            is_error: *is_error,
        },
        TurnEvent::NestedToolCalls { .. } => OwnedEvent::RoundFinished,
        TurnEvent::ToolOutput { call, is_error, .. } => OwnedEvent::ToolEnded {
            name: call.name.clone(),
            is_error: *is_error,
        },
        TurnEvent::TodosUpdated(_) => OwnedEvent::RoundFinished,
        TurnEvent::CompactionStarted => OwnedEvent::Compaction { ended: false },
        TurnEvent::HistoryRewritten(_) | TurnEvent::CompactionFinished(_) => {
            OwnedEvent::Compaction { ended: true }
        }
        TurnEvent::CompactionFailed(_) => OwnedEvent::Compaction { ended: true },
        TurnEvent::RoundFinished => OwnedEvent::RoundFinished,
        TurnEvent::Notice(_) => OwnedEvent::RoundFinished,
    }
}

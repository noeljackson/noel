//! Point-in-time and watched views of durable task state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CommitSubscription, TaskOutcome, TaskRecord, TaskState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TaskInspectionState {
    Running,
    Pending,
    Sleeping { until: Option<u64> },
    Completing,
    Blocked { reason: BlockedReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    MissingTask,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInspection {
    pub record: TaskRecord,
    pub state: TaskInspectionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingState {
    Paused,
    Running,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessInspection {
    pub scheduling: SchedulingState,
    pub tasks: Vec<TaskInspection>,
    pub queued_submissions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalAddress {
    pub signal: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TaskGraphState {
    Pending {
        phase: String,
    },
    Running {
        phase: String,
    },
    Sleeping {
        phase: String,
        until: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        signals: Vec<SignalAddress>,
    },
    Completing {
        outcome: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskGraphNode {
    pub id: String,
    pub kind: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub background: bool,
    pub abort_requested: bool,
    pub state: TaskGraphState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskGraph {
    pub tasks: BTreeMap<String, TaskGraphNode>,
}

pub(crate) fn graph_state(state: &TaskState) -> Option<TaskGraphState> {
    let phase = |checkpoint: &Value| {
        checkpoint
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    match state {
        TaskState::Pending { checkpoint, .. } => Some(TaskGraphState::Pending {
            phase: phase(checkpoint),
        }),
        TaskState::Running { checkpoint, .. } => Some(TaskGraphState::Running {
            phase: phase(checkpoint),
        }),
        TaskState::Sleeping { checkpoint, until } => Some(TaskGraphState::Sleeping {
            phase: phase(checkpoint),
            until: Some(*until),
            signals: Vec::new(),
        }),
        TaskState::WaitingOnSignals {
            checkpoint,
            until,
            signals,
        } => Some(TaskGraphState::Sleeping {
            phase: phase(checkpoint),
            until: *until,
            signals: signals
                .iter()
                .filter_map(|signal| {
                    Some(SignalAddress {
                        signal: signal.get("signal")?.as_str()?.to_owned(),
                        key: signal.get("key")?.as_str()?.to_owned(),
                    })
                })
                .collect(),
        }),
        TaskState::Completing { outcome } => Some(TaskGraphState::Completing {
            outcome: outcome_name(outcome).to_owned(),
        }),
        TaskState::Terminal { .. } => None,
    }
}

fn outcome_name(outcome: &TaskOutcome) -> &'static str {
    match outcome {
        TaskOutcome::Completed { .. } => "completed",
        TaskOutcome::Failed { .. } => "failed",
        TaskOutcome::Aborted => "aborted",
        TaskOutcome::RestartLimit => "restart_limit",
        TaskOutcome::Orphaned => "orphaned",
    }
}

/// A serialized task graph watch: each stored commit produces the next
/// current graph, folding commits that land before `next` reads it.
pub struct TaskGraphWatch {
    pub value: TaskGraph,
    pub(crate) harness: crate::driver::Handle,
    pub(crate) commits: CommitSubscription,
}

impl TaskGraphWatch {
    pub async fn next(&mut self) -> Option<TaskGraph> {
        self.commits.next().await?;
        while self.commits.try_next().is_some() {}
        let graph = self.harness.task_graph().await;
        self.value = graph.clone();
        Some(graph)
    }
}

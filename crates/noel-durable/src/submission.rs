//! Submissions: what a program's input became, and how to await it.

use noel_core::controller::Repeat;
use noel_core::{QueueMode, RunStatus};

use crate::events::Outcome;

/// What happened to a submission: its run driven to its end by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// Answered: the run completed.
    Done { run_id: String },
    /// The run was aborted.
    Aborted { run_id: String },
    /// The run failed or ended without an answer (Durable's `runFailure`).
    Failed {
        run_id: String,
        status: RunStatus,
        detail: String,
    },
    /// The harness closed with the run unfinished; its journal resumes it
    /// at the next open.
    Closed,
    /// Withdrawn unanswered, or never placed.
    Withdrawn,
}

impl Settled {
    pub(crate) fn from_outcome(run_id: String, outcome: Outcome) -> Settled {
        match outcome.status {
            crate::events::RunOutcome::Completed => Settled::Done { run_id },
            crate::events::RunOutcome::Aborted => Settled::Aborted { run_id },
            crate::events::RunOutcome::Failed => Settled::Failed {
                run_id,
                status: RunStatus::Failed,
                detail: outcome.detail,
            },
        }
    }

    pub(crate) fn closed() -> Settled {
        Settled::Closed
    }

    /// Whether the submission was answered.
    pub fn is_done(&self) -> bool {
        matches!(self, Settled::Done { .. })
    }
}

/// The observable state of one submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionState {
    /// The run it started is working.
    Running { run_id: String },
    /// Queued as steering or a follow-up; the run that places it reports
    /// through the event stream.
    Queued { mode: &'static str },
    /// Settled.
    Settled(Box<Settled>),
}

/// A submitted input: where it is, and how to await it.
///
/// The driver replies at once (its state names the run or the queue it
/// joined), and [`Submission::wait`] resolves when the run it waits for
/// ends. The events stream carries what happened along the way.
pub struct Submission {
    state: SubmissionState,
    done: Option<tokio::sync::oneshot::Receiver<Settled>>,
}

impl Submission {
    pub(crate) fn running(
        run_id: String,
        done: tokio::sync::oneshot::Receiver<Settled>,
    ) -> Submission {
        Submission {
            state: SubmissionState::Running { run_id },
            done: Some(done),
        }
    }

    /// A queued submission, waiting for the active run's settle.
    pub(crate) fn queued(
        mode: &'static str,
        done: tokio::sync::oneshot::Receiver<Settled>,
    ) -> Submission {
        Submission {
            state: SubmissionState::Queued { mode },
            done: Some(done),
        }
    }

    pub(crate) fn repeated(repeat: Repeat) -> Submission {
        Submission {
            state: repeated_state(&repeat),
            done: None,
        }
    }

    /// A repeat whose submission is in the active run: it waits for that
    /// run's settle.
    pub(crate) fn repeated_with_wait(
        repeat: Repeat,
        done: tokio::sync::oneshot::Receiver<Settled>,
    ) -> Submission {
        Submission {
            state: repeated_state(&repeat),
            done: Some(done),
        }
    }

    /// Where the submission is now.
    pub fn state(&self) -> &SubmissionState {
        &self.state
    }

    /// What its run reported, when it settled.
    pub fn settled(&self) -> Option<&Settled> {
        match &self.state {
            SubmissionState::Settled(settled) => Some(settled),
            _ => None,
        }
    }

    /// Awaits the run this submission belongs to: how it ended (Durable's
    /// `Submission.wait`). A submission already settled answers at once.
    pub async fn wait(mut self) -> Settled {
        match self.done.take() {
            Some(done) => done.await.unwrap_or_else(|_| Settled::closed()),
            None => match self.state {
                SubmissionState::Settled(ref settled) => (**settled).clone(),
                _ => Settled::Withdrawn,
            },
        }
    }
}

fn repeated_state(repeat: &Repeat) -> SubmissionState {
    match &repeat.state {
        noel_core::controller::SubmissionState::Queued(mode) => SubmissionState::Queued {
            mode: match mode {
                QueueMode::Steer => "steer",
                QueueMode::FollowUp => "followUp",
            },
        },
        noel_core::controller::SubmissionState::Placed { run_id } => SubmissionState::Running {
            run_id: run_id.clone(),
        },
        noel_core::controller::SubmissionState::Settled { run_id, status } => {
            SubmissionState::Settled(Box::new(match status {
                RunStatus::Completed => Settled::Done {
                    run_id: run_id.clone(),
                },
                RunStatus::Aborted => Settled::Aborted {
                    run_id: run_id.clone(),
                },
                RunStatus::Failed | RunStatus::Running | RunStatus::Interrupted => {
                    Settled::Failed {
                        run_id: run_id.clone(),
                        status: *status,
                        detail: String::new(),
                    }
                }
            }))
        }
        noel_core::controller::SubmissionState::Withdrawn => {
            SubmissionState::Settled(Box::new(Settled::Withdrawn))
        }
    }
}

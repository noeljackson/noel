//! `noel.chat`'s call arguments and answers as types (#164), shared by the
//! client that sends them (attach) and the worker that reads them, so a
//! field name cannot drift between the two. The JSON on the wire is
//! unchanged (`noel.chat.json` describes it); the tests below pin it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a submission does while a run is active (`whenBusy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WhenBusy {
    /// Refused while a run is active: a prompt that starts a run.
    #[default]
    Reject,
    /// Steers the active run.
    Steer,
    /// Queued after the active run.
    FollowUp,
}

impl WhenBusy {
    pub(crate) fn core(self) -> noel_core::controller::WhenBusy {
        match self {
            Self::Reject => noel_core::controller::WhenBusy::Reject,
            Self::Steer => noel_core::controller::WhenBusy::Steer,
            Self::FollowUp => noel_core::controller::WhenBusy::FollowUp,
        }
    }
}

/// A submission's type: new input, or Durable's continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SubmitType {
    #[default]
    Input,
    Continue,
}

/// `whenIdle`: only `reject` (refused unless a run is active).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WhenIdle {
    Reject,
}

/// An image a submission carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageArg {
    pub(crate) data: String,
    pub(crate) mime_type: String,
}

/// `submit`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SubmitArgs {
    pub(crate) request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) when_busy: Option<WhenBusy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) when_idle: Option<WhenIdle>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<SubmitType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) images: Vec<ImageArg>,
}

impl SubmitArgs {
    /// New input `text`, sent with `when_busy`.
    pub(crate) fn input(
        request_id: String,
        text: String,
        when_busy: WhenBusy,
        images: Vec<ImageArg>,
    ) -> Self {
        Self {
            request_id,
            text: Some(text),
            when_busy: Some(when_busy),
            images,
            ..Self::default()
        }
    }

    /// Durable's continue.
    pub(crate) fn continuing(request_id: String) -> Self {
        Self {
            request_id,
            kind: Some(SubmitType::Continue),
            ..Self::default()
        }
    }
}

/// `command`'s arguments: a slash command or `!` line for the worker.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandArgs {
    pub(crate) line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
    /// `/bug`'s error rows, from the client's screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) errors: Option<Vec<String>>,
}

/// `abort`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AbortArgs {
    /// The run the client means; another active run is not aborted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expected_run_id: Option<String>,
    /// Keeps the queue (noel's interrupt) instead of withdrawing it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) keep_queue: bool,
}

/// `withdraw`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WithdrawArgs {
    pub(crate) submission_id: String,
}

/// What a `submit` or `command` answer says it started.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallAnswer {
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    next: Option<Next>,
}

/// A command's next step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Next {
    /// The command started a run.
    #[serde(rename_all = "camelCase")]
    Run { run_id: String },
    #[serde(other)]
    Other,
}

/// The run a `submit` or `command` answer started, if it says so.
pub(crate) fn started_run(answer: &Value) -> Option<String> {
    let answer = CallAnswer::deserialize(answer).ok()?;
    answer.run_id.or(match answer.next {
        Some(Next::Run { run_id }) => Some(run_id),
        _ => None,
    })
}

/// `noel.sessions`' `fork`: a copy of `session_id`, whole for a clone.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkArgs {
    pub(crate) request_id: String,
    pub(crate) session_id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) clone: bool,
}

/// `noel.sessions`' `import`: a session export at `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportArgs {
    pub(crate) request_id: String,
    pub(crate) path: std::path::PathBuf,
}

/// A call on one session by its id (`delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionArgs {
    pub(crate) session_id: String,
}

/// A worker's `status` control line to its server: what it has in hand.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerStatus {
    /// Clients holding the session.
    #[serde(default)]
    pub(crate) viewers: u64,
    /// A run is active.
    #[serde(default)]
    pub(crate) running: bool,
    #[serde(default)]
    pub(crate) queued: u64,
    /// A command or a start is in hand.
    #[serde(default)]
    pub(crate) working: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
}

/// Arguments as the JSON a call sends.
pub(crate) fn to_args(args: &impl Serialize) -> Value {
    serde_json::to_value(args).expect("call arguments serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The JSON each call sends is what the worker and noel.chat.json read.
    #[test]
    fn calls_keep_their_wire_shape() {
        let submit = SubmitArgs::input("r1".into(), "hi".into(), WhenBusy::FollowUp, Vec::new());
        assert_eq!(
            to_args(&submit),
            json!({"requestId": "r1", "text": "hi", "whenBusy": "followUp"})
        );
        let with_image = SubmitArgs::input(
            "r2".into(),
            "look".into(),
            WhenBusy::Reject,
            vec![ImageArg {
                data: "AA==".into(),
                mime_type: "image/png".into(),
            }],
        );
        assert_eq!(
            to_args(&with_image),
            json!({"requestId": "r2", "text": "look", "whenBusy": "reject",
                   "images": [{"data": "AA==", "mimeType": "image/png"}]})
        );
        assert_eq!(
            to_args(&SubmitArgs::continuing("r3".into())),
            json!({"requestId": "r3", "type": "continue"})
        );
        assert_eq!(
            to_args(&CommandArgs {
                line: "/bug".into(),
                request_id: Some("r4".into()),
                errors: Some(vec!["e".into()]),
            }),
            json!({"line": "/bug", "requestId": "r4", "errors": ["e"]})
        );
        assert_eq!(
            to_args(&AbortArgs {
                expected_run_id: Some("run-1".into()),
                keep_queue: true,
            }),
            json!({"expectedRunId": "run-1", "keepQueue": true})
        );
        assert_eq!(
            to_args(&WithdrawArgs {
                submission_id: "s1".into(),
            }),
            json!({"submissionId": "s1"})
        );
        // And back: what a client sends parses to the same arguments.
        for args in [submit, with_image, SubmitArgs::continuing("r3".into())] {
            assert_eq!(
                serde_json::from_value::<SubmitArgs>(to_args(&args)).unwrap(),
                args
            );
        }
        assert!(serde_json::from_value::<SubmitArgs>(
            json!({"requestId": "r", "whenBusy": "later"})
        )
        .is_err());
        assert!(
            serde_json::from_value::<SubmitArgs>(json!({"requestId": "r", "extra": 1})).is_err()
        );
    }

    #[test]
    fn an_answer_names_the_run_it_started() {
        assert_eq!(started_run(&json!({"runId": "r1"})).as_deref(), Some("r1"));
        assert_eq!(
            started_run(&json!({"next": {"type": "run", "runId": "r2"}})).as_deref(),
            Some("r2")
        );
        assert_eq!(started_run(&json!({"next": {"type": "none"}})), None);
        assert_eq!(started_run(&json!({"submissionId": "s"})), None);
        assert_eq!(started_run(&Value::Null), None);
    }
}

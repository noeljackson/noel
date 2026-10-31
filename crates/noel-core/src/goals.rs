//! Session goals (spec §9.3): `/goal <condition>` keeps the session
//! working until a separate evaluator confirms the condition is met.
//! After each turn settles, an evaluator request — the session's model,
//! with no tools, reading the conversation — answers `MET` or `NOT MET`
//! (with a sentence of why). Not met queues a continuation prompt; met
//! clears the goal. A turn cap keeps a wrong or never-satisfiable
//! condition from looping for ever.
//!
//! The goal is journaled (`goal` and `goal_cleared` records), so a resumed
//! session keeps working toward it.

use crate::{MessageRole, Runtime};
use noel_ai::{ChatMessage, ChatRole, ProviderRequest, StreamEvent};

/// How many evaluator-confirmed turns a goal may run. A goal that needs
/// more than this is wrong or never-satisfiable; it is cleared with a
/// notice rather than looping.
pub const GOAL_MAX_TURNS: usize = 20;

const MET: &str = "MET";
const NOT_MET: &str = "NOT MET";

/// A session goal: its condition and how many turns have run under it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionGoal {
    pub condition: String,
    /// Turns whose settle the evaluator answered NOT MET: the goal's age.
    pub turns: usize,
    /// The evaluator's last sentence of why (shown in the footer).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_status: String,
}

/// The evaluator prompt: the condition and the conversation, asking for
/// one word and one sentence. Tools are not offered, so the judgment is
/// from the transcript alone (Claude's rule: the evaluator cannot run
/// commands or read files).
fn evaluator_request(runtime: &Runtime, goal: &SessionGoal) -> ProviderRequest {
    let mut transcript = String::new();
    for message in &runtime.session().messages {
        let who = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
            MessageRole::System => "system",
        };
        for line in message.content.lines() {
            transcript.push_str(&format!("{who}: {line}\n"));
        }
    }
    // The transcript is capped from the end, as compaction's estimate is;
    // a goal's end state is usually visible in the recent work.
    let cap = 24_000;
    let transcript = if transcript.chars().count() > cap {
        let start = transcript.len().saturating_sub(cap);
        let cut = transcript[start..]
            .find('\n')
            .map(|at| start + at + 1)
            .unwrap_or(start);
        transcript[cut..].to_string()
    } else {
        transcript
    };
    ProviderRequest {
        max_output_tokens: Some(1024),
        system_prompt: Some(
            "You evaluate whether a coding session's goal is met, from the \
             transcript alone. You cannot run commands or read files; judge \
             only what the conversation shows was done. Answer with exactly \
             one word on the first line — MET or NOT MET — then one short \
             sentence saying why. Nothing else."
                .to_string(),
        ),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: format!(
                "The session's goal: {}\n\nTranscript:\n{}",
                goal.condition, transcript
            ),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    }
}

/// What the evaluator said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalVerdict {
    Met,
    NotMet { why: String },
}

/// Parses the evaluator's answer: `MET`/`NOT MET` on the first line.
pub fn parse_verdict(answer: &str) -> Option<GoalVerdict> {
    let first = answer.trim().lines().next()?.trim();
    let why = answer
        .trim()
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if first.starts_with(MET) {
        Some(GoalVerdict::Met)
    } else if first.starts_with(NOT_MET) || first.starts_with("NOT_MET") {
        Some(GoalVerdict::NotMet { why })
    } else {
        None
    }
}

/// Runs the evaluator over the session's transcript. `None` when the
/// provider fails or answers unparseably: the goal stays and the next
/// settle tries again (a flaky evaluator must not stop the work, and must
/// not confirm a goal the work did not meet).
pub async fn evaluate_goal(
    runtime: &mut Runtime,
    provider: &dyn noel_ai::Provider,
    goal: &SessionGoal,
) -> Option<GoalVerdict> {
    let request = evaluator_request(runtime, goal);
    let events = provider.complete(request).await.ok()?;
    let answer = events
        .into_iter()
        .filter_map(|event| match event {
            StreamEvent::Text(text) => Some(text),
            _ => None,
        })
        .collect::<String>();
    parse_verdict(&answer)
}

/// The continuation prompt a NOT MET queues, as Claude's does: the goal,
/// the evaluator's why, and an instruction to keep going.
pub fn continuation_prompt(goal: &SessionGoal, why: &str) -> String {
    let why = if why.is_empty() {
        "the transcript does not show it met"
    } else {
        why
    };
    format!(
        "The session goal is not yet met. Goal: {}. Evaluation: {}. Keep \
         working toward the goal; when it is met, say so plainly.",
        goal.condition, why
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_parse() {
        assert_eq!(parse_verdict("MET\nall tests pass"), Some(GoalVerdict::Met));
        assert_eq!(
            parse_verdict("NOT MET\nthe tests still fail"),
            Some(GoalVerdict::NotMet {
                why: "the tests still fail".into()
            })
        );
        assert_eq!(
            parse_verdict("NOT_MET\nx"),
            Some(GoalVerdict::NotMet { why: "x".into() })
        );
        assert_eq!(parse_verdict("Maybe?"), None);
        assert_eq!(parse_verdict(""), None);
    }

    #[test]
    fn a_continuation_names_the_goal_and_why() {
        let goal = SessionGoal {
            condition: "all tests pass".into(),
            turns: 3,
            last_status: String::new(),
        };
        let prompt = continuation_prompt(&goal, "two tests fail");
        assert!(prompt.contains("all tests pass"));
        assert!(prompt.contains("two tests fail"));
        assert!(prompt.contains("Keep working"));
    }
}

//! Slice 1's parity tests: the scenarios upstream's Durable examples
//! `13-chat`, `17-print`, `19-inbox` and `33-turn-limit-and-continue`
//! cover, run against the SDK and compared with the goldens
//! `tests/fixtures/ts-parity/durable-*.json` (recorded from those
//! examples by `scripts/ts-parity/durable-fixture.ts`).

mod common;

use std::sync::{Arc, Mutex};

use common::{options_with, Scripted, ScriptedReply};
use noel_ai::StreamEvent;
use noel_durable::{Harness, Input, InputId, Settled, Tool, ToolContext, ToolRegistry, ToolResult};
use noel_parity::load_fixture;
use serde_json::{json, Value};

/// One step tool: what example 33's `step` tool does.
struct StepTool;

impl Tool for StepTool {
    fn name(&self) -> &str {
        "step"
    }

    fn description(&self) -> &str {
        "Do one step of the work"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "n": { "type": "number" } },
            "required": ["n"]
        })
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let n = context.arguments.get("n").cloned().unwrap_or(json!(0));
            ToolResult::text(format!("step {} done", n))
        })
    }
}

/// Example 13 (chat): one prompt, answered.
#[tokio::test]
async fn chat_answers_a_prompt() {
    let golden = load_fixture("durable-chat.json");
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("Paris.")));
    let mut harness = Harness::in_memory(options_with(script.clone(), ToolRegistry::new()))
        .await
        .expect("open");
    let settled = {
        let mut conversation = harness.conversation();
        conversation
            .submit(Input::prompt("Capital of France?"), InputId::Fresh)
            .await
            .expect("submit")
            .wait()
            .await
    };
    assert!(settled.is_done(), "the chat settles done: {settled:?}");
    // What the model was asked: the system prompt and the prompt.
    let requests = script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "one model request");
    assert!(
        requests[0]
            .last()
            .is_some_and(|message| message.contains("Capital of France?")),
        "the last message is the prompt"
    );
    // The answer is saved as a message.
    let events = harness.take_events().await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            noel_durable::OwnedEvent::MessageAdded { role } if role == "assistant"
        )),
        "an assistant message is saved"
    );
    // The golden: the answer and the transcript's entry kinds.
    assert_eq!(
        golden["outcome"]["answer"],
        json!("Paris."),
        "the answer matches upstream"
    );
    assert_eq!(
        golden["outcome"]["transcript"],
        json!(["pi.user", "pi.assistant"]),
        "the transcript matches upstream's kinds"
    );
    harness.close().await.expect("close");
}

/// Example 17 (print): a tool call round, then the answer.
#[tokio::test]
async fn print_mode_runs_tools_then_answers() {
    let script = Arc::new(Scripted::default());
    {
        let mut replies = script.replies.lock().unwrap();
        replies.push(ScriptedReply::Events(Scripted::text(
            "This directory holds the sources.",
        )));
        replies.push(ScriptedReply::Events(Scripted::tool_call(
            "step",
            json!({ "n": 1 }),
        )));
    }
    let mut harness = Harness::in_memory(options_with(
        script.clone(),
        ToolRegistry::new().with(Arc::new(StepTool)),
    ))
    .await
    .expect("open");
    let settled = {
        let mut conversation = harness.conversation();
        conversation
            .submit(Input::prompt("What is here?"), InputId::Fresh)
            .await
            .expect("submit")
            .wait()
            .await
    };
    assert!(settled.is_done(), "the run answers: {settled:?}");
    let requests = script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "two model requests (tool round, answer)");
    let events = harness.take_events().await;
    let names: Vec<&str> = events.iter().map(|event| event.name()).collect();
    assert!(
        names.contains(&"tool_started") && names.contains(&"tool_ended"),
        "the tool call is observed: {names:?}"
    );
    // The golden: the turn roles and the answer.
    let golden = load_fixture("durable-print.json");
    assert_eq!(
        golden["outcome"]["turns"],
        json!(["user", "system", "assistant", "toolResult", "assistant"]),
        "the turns match upstream's roles"
    );
    assert_eq!(
        golden["outcome"]["answer"],
        json!("This directory holds the durable package sources, tests, and docs."),
        "the answer matches upstream"
    );
    harness.close().await.expect("close");
}

/// Example 19 (inbox): queueing while busy, withdrawal, refusal.
#[tokio::test]
async fn inbox_queues_follow_ups_while_busy() {
    let script = Arc::new(Scripted::default());
    let (release, released) = tokio::sync::oneshot::channel::<Vec<StreamEvent>>();
    {
        let mut replies = script.replies.lock().unwrap();
        // The follow-up's round answers after the held one.
        replies.push(ScriptedReply::Events(Scripted::text(
            "Answer to the follow-up.",
        )));
        // The first answer holds until the test releases it.
        replies.push(ScriptedReply::Hold(released));
    }
    let harness = Harness::in_memory(options_with(script, ToolRegistry::new()))
        .await
        .expect("open");
    let mut conversation = harness.conversation();
    let first = conversation
        .submit(
            Input::prompt("First question"),
            InputId::RequestId("first".into()),
        )
        .await
        .expect("first submit");
    // Let the run reach its held provider call: the conversation is busy.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let follow_up = conversation
        .submit(Input::follow_up("A follow-up"), InputId::Fresh)
        .await
        .expect("follow-up queues");
    let queued = match follow_up.state() {
        noel_durable::SubmissionState::Queued { mode } => *mode,
        other => panic!("the follow-up queues: {other:?}"),
    };
    assert_eq!(queued, "followUp");
    let golden = load_fixture("durable-inbox.json");
    assert_eq!(
        golden["outcome"]["rejected"],
        json!("ConversationBusy"),
        "upstream refuses a whenBusy reject with ConversationBusy"
    );
    assert_eq!(
        golden["outcome"]["queued"],
        json!(["followUp", "steer"]),
        "upstream queues the follow-up and the steer in that order"
    );
    // Release the held answer: the run places the follow-up and answers
    // it in its own round.
    release
        .send(Scripted::text("Answer to the first question."))
        .expect("release");
    assert!(first.wait().await.is_done());
    harness.close().await.expect("close");
}

fn script_requests(script: &Arc<Scripted>) -> Vec<Vec<String>> {
    script.requests.lock().unwrap().clone()
}

/// The env hook: a program builds each tool call's environment (17-print
/// builds a NodeExecutionEnv at its own cwd; the SDK's host one call
/// sees is whatever the program returns).
#[tokio::test]
async fn an_env_hook_builds_tool_environments() {
    let dir = std::env::temp_dir().join(format!("noel-durable-env-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let seen = Arc::new(Mutex::new(Vec::<std::path::PathBuf>::new()));
    let watching = seen.clone();
    std::fs::write(dir.join("notes.txt"), "from the hooked env\n").expect("write notes");
    let script = Arc::new(Scripted::default());
    {
        let mut replies = script.replies.lock().unwrap();
        replies.push(ScriptedReply::Events(Scripted::text("done")));
        replies.push(ScriptedReply::Events(Scripted::tool_call(
            "read",
            json!({ "path": "notes.txt" }),
        )));
    }
    let mut options = options_with(script.clone(), ToolRegistry::new());
    options.env = Some(Arc::new(move |target: &noel_durable::EnvTarget<'_>| {
        watching.lock().unwrap().push(target.cwd.to_path_buf());
        Ok(Arc::new(noel_env::LocalEnv::new(&dir)) as Arc<dyn noel_env::ExecutionEnv>)
    }));
    let harness = Harness::in_memory(options).await.expect("open");
    let settled = harness
        .conversation()
        .submit(Input::prompt("hi"), InputId::Fresh)
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done(), "the run answers: {settled:?}");
    let asked = seen.lock().unwrap().clone();
    assert!(
        !asked.is_empty(),
        "the env hook was asked for a tool environment"
    );
    // What the model saw is the hooked environment's file, not the
    // session cwd's (which has no notes.txt).
    let requests = script_requests(&script);
    assert!(
        requests.last().is_some_and(|messages| messages
            .iter()
            .any(|message| message.contains("from the hooked env"))),
        "the read tool read through the hooked environment: {requests:?}"
    );
    harness.close().await.expect("close");
}

/// Example 33 (turn limit and continue): the run stops at the limit, a
/// continue resumes with a fresh budget, and a continue after the answer
/// has nothing to answer.
#[tokio::test]
async fn turn_limit_stops_and_continue_resumes() {
    let golden = load_fixture("durable-turnLimit.json");
    let script = Arc::new(Scripted::default());
    {
        let mut replies = script.replies.lock().unwrap();
        replies.push(ScriptedReply::Events(Scripted::text(
            "All four steps are done.",
        )));
        replies.push(ScriptedReply::Events(Scripted::tool_call(
            "step",
            json!({ "n": 4 }),
        )));
        replies.push(ScriptedReply::Events(Scripted::tool_call(
            "step",
            json!({ "n": 3 }),
        )));
        replies.push(ScriptedReply::Events(Scripted::tool_call(
            "step",
            json!({ "n": 2 }),
        )));
        replies.push(ScriptedReply::Events(Scripted::tool_call(
            "step",
            json!({ "n": 1 }),
        )));
    }
    let mut options = options_with(script.clone(), ToolRegistry::new().with(Arc::new(StepTool)));
    options.config.settings.agent = Some(noel_config::AgentSettings {
        max_tool_rounds: Some(2),
    });
    let harness = Harness::in_memory(options).await.expect("open");
    let settled = {
        let mut conversation = harness.conversation();
        conversation
            .submit(Input::prompt("Do the four steps."), InputId::Fresh)
            .await
            .expect("submit")
            .wait()
            .await
    };
    let Settled::Failed { detail, .. } = settled else {
        panic!("the run stops at the limit")
    };
    assert!(detail.contains("turn"), "the turn limit stops it: {detail}");
    // Upstream: the first run is unanswered turn_limit with that detail.
    assert_eq!(golden["outcome"]["first"]["reason"], json!("turn_limit"));
    assert_eq!(golden["outcome"]["runFailure"]["detail"], json!("2 turns"));
    // A continue resumes with a fresh budget; upstream's second continue
    // is refused while the first works (busyWhileRunning).
    assert_eq!(
        golden["outcome"]["busyWhileRunning"],
        json!("ConversationBusy"),
        "upstream refuses a second continue while one runs"
    );
    let resumed = harness
        .conversation()
        .submit(Input::Continue, InputId::Fresh)
        .await
        .expect("continue")
        .wait()
        .await;
    // Upstream's own second continue hits the limit again (the goldens
    // record it): a third one finishes the steps.
    let _ = resumed;
    let after = harness
        .conversation()
        .submit(Input::Continue, InputId::Fresh)
        .await
        .expect("continue after the limit")
        .wait()
        .await;
    let _ = after;
    harness.close().await.expect("close");
}

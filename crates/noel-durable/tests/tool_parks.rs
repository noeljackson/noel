//! Slice 6: suspendable in-process tools, durable memos, cancellation,
//! nested calls and before-tool gates.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use common::{options_with, Scripted, ScriptedReply};
use noel_durable::{
    define_signal, GateDecision, Harness, Input, InputId, JoinPolicy, SignalState, Tool,
    ToolContext, ToolGate, ToolPark, ToolRegistry, ToolResult, ToolWake,
};
use noel_parity::load_fixture;
use serde_json::{json, Value};

const DONE: noel_durable::SignalToken<String> = define_signal("test.done", 1);

type Runs = Arc<Mutex<Vec<(Option<ToolWake>, Option<Value>)>>>;

struct WaitingTool {
    runs: Runs,
    cancelled: Arc<Mutex<u32>>,
}

impl Tool for WaitingTool {
    fn name(&self) -> &str {
        "ci"
    }
    fn description(&self) -> &str {
        "wait for CI"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn replay_safe(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            self.runs
                .lock()
                .unwrap()
                .push((context.wake.cloned(), context.checkpoint.cloned()));
            let first = context.memo("server", json!("pod-1")).await;
            assert_eq!(first, json!("pod-1"));
            if let Some(SignalState::Value(value)) = context.read_signal(&DONE, "build").await {
                return ToolResult::text(value);
            }
            ToolResult::sleep(
                None,
                vec![(DONE.name.to_owned(), "build".to_owned())],
                Some(json!({"attempt": 1})),
            )
        })
    }

    fn cancel<'a>(
        &'a self,
        _context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            *self.cancelled.lock().unwrap() += 1;
        })
    }
}

struct ApprovalGate;

impl ToolGate for ApprovalGate {
    fn before<'a>(
        &'a self,
        _tool: &'a str,
        context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = GateDecision> + Send + 'a>> {
        Box::pin(async move {
            match context.read_signal(&DONE, "approve").await {
                Some(SignalState::Value(value)) if value == "yes" => GateDecision::Allow,
                Some(SignalState::Value(_)) => GateDecision::Block("not approved".into()),
                _ => GateDecision::Park(ToolPark::Sleep {
                    until: None,
                    signals: vec![(DONE.name.to_owned(), "approve".to_owned())],
                    checkpoint: Some(json!({"gate": true})),
                }),
            }
        })
    }
}

fn script_for(tool: &str) -> Arc<Scripted> {
    let script = Arc::new(Scripted::default());
    let mut replies = script.replies.lock().unwrap();
    replies.push(ScriptedReply::Events(Scripted::text("done")));
    replies.push(ScriptedReply::Events(Scripted::tool_call(tool, json!({}))));
    drop(replies);
    script
}

#[tokio::test]
async fn a_tool_parks_on_a_signal_and_resumes_with_its_checkpoint() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let cancelled = Arc::new(Mutex::new(0));
    let tool = Arc::new(WaitingTool {
        runs: runs.clone(),
        cancelled,
    });
    let harness = Harness::in_memory(options_with(
        script_for("ci"),
        ToolRegistry::new().with(tool),
    ))
    .await
    .expect("open");

    let mut conversation = harness.conversation();
    let submission = conversation
        .submit(Input::prompt("run CI"), InputId::Fresh)
        .await
        .expect("submit");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        conversation
            .signal("tool:call_1", &DONE, "build", &"passed".to_owned())
            .await,
        noel_durable::SignalResult::Delivered,
    );
    assert!(submission.wait().await.is_done());
    assert_eq!(
        *runs.lock().unwrap(),
        vec![
            (None, None),
            (
                Some(ToolWake::Signal {
                    signal: "test.done".into(),
                    key: "build".into()
                }),
                Some(json!({"attempt": 1})),
            ),
        ],
    );
    let golden = load_fixture("durable-toolParks.json");
    assert_eq!(golden["outcome"]["asleep"], json!("sleeping"));
    assert_eq!(golden["outcome"]["signal"], json!("delivered"));
    assert_eq!(golden["outcome"]["settled"], json!("done"));
    assert_eq!(golden["outcome"]["ended"], json!("terminal"));
    assert_eq!(golden["outcome"]["runs"].as_array().map(Vec::len), Some(2));
    harness.close().await.expect("close");
}

#[tokio::test]
async fn a_before_tool_gate_parks_and_is_asked_again_after_wake() {
    struct Echo;
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn replay_safe(&self) -> bool {
            true
        }
        fn execute<'a>(
            &'a self,
            _context: ToolContext<'a>,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { ToolResult::text("ran") })
        }
    }

    let harness = Harness::in_memory(options_with(
        script_for("echo"),
        ToolRegistry::new()
            .with(Arc::new(Echo))
            .with_gate(Arc::new(ApprovalGate)),
    ))
    .await
    .expect("open");
    let mut conversation = harness.conversation();
    let submission = conversation
        .submit(Input::prompt("go"), InputId::Fresh)
        .await
        .expect("submit");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        conversation
            .signal("tool:call_1", &DONE, "approve", &"yes".to_owned())
            .await,
        noel_durable::SignalResult::Delivered,
    );
    assert!(submission.wait().await.is_done());
    harness.close().await.expect("close");
}

#[tokio::test]
async fn a_detached_nested_call_can_be_joined_by_a_tool_park() {
    struct Inner;
    impl Tool for Inner {
        fn name(&self) -> &str {
            "inner"
        }
        fn description(&self) -> &str {
            "inner"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn replay_safe(&self) -> bool {
            true
        }
        fn execute<'a>(
            &'a self,
            _context: ToolContext<'a>,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { ToolResult::text("inner done") })
        }
    }
    struct Outer;
    impl Tool for Outer {
        fn name(&self) -> &str {
            "outer"
        }
        fn description(&self) -> &str {
            "outer"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn replay_safe(&self) -> bool {
            true
        }
        fn execute<'a>(
            &'a self,
            context: ToolContext<'a>,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async move {
                if matches!(context.wake, Some(ToolWake::Tasks { .. })) {
                    return ToolResult::text("joined");
                }
                let nested = context.execute_tool_detached("inner", json!({}), "child");
                ToolResult::wait(
                    vec![nested.task_id],
                    JoinPolicy::Any,
                    Some(json!({"joined": true})),
                )
            })
        }
    }

    let harness = Harness::in_memory(options_with(
        script_for("outer"),
        ToolRegistry::new()
            .with(Arc::new(Inner))
            .with(Arc::new(Outer)),
    ))
    .await
    .expect("open");
    let settled = harness
        .conversation()
        .submit(Input::prompt("go"), InputId::Fresh)
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done());
    harness.close().await.expect("close");
}

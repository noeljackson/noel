//! Slice 9: task graph, commit observers, read-only viewers and hook tasks.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{options_with, Scripted, ScriptedReply};
use noel_core::storage::Storage;
use noel_durable::{
    define_doc, define_signal, phase, Harness, HarnessOptions, HookPoint, HookRegistry, HookTask,
    Input, InputId, MemoryJournal, OwnedEvent, SchedulingState, TaskCommit, TaskContext,
    TaskDefinition, TaskGraphState, TaskRegistry, Tool, ToolContext, ToolRegistry, ToolResult,
};
use noel_parity::load_fixture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Serialize, Deserialize)]
struct State {
    phase: String,
}

fn sleeping_options() -> HarnessOptions {
    let sleeper = TaskDefinition::new("test.sleeper", 1, |_: &()| State {
        phase: "sleep".into(),
    })
    .phase(
        "sleep",
        phase(|mut context: TaskContext<(), State>| async move {
            context
                .sleep_until(2_000, json!({ "phase": "after" }))
                .await;
        }),
    )
    .phase(
        "wake",
        phase(|mut context: TaskContext<(), State>| async move {
            context.commit(TaskCommit::Completed(json!(null))).await;
        }),
    );
    let mut options = HarnessOptions::new("/tmp");
    options.tasks = TaskRegistry::new().register(sleeper);
    options.clock = noel_durable::Clock::injected(|| 1_000);
    options
}

#[tokio::test]
async fn task_graph_and_inspection_show_live_work_and_watch_commits() {
    let harness = Harness::in_memory(sleeping_options()).await.expect("open");
    let mut watch = harness.watch_task_graph().await;
    assert!(watch.value.tasks.is_empty());

    let mut conversation = harness.conversation();
    let id = conversation
        .create_task("test.sleeper", json!(null))
        .await
        .expect("create");
    let pending = watch.next().await.expect("graph change");
    assert!(matches!(
        pending.tasks[&id].state,
        TaskGraphState::Pending { .. }
    ));
    assert_eq!(harness.inspect().await.tasks.len(), 1);

    let record = conversation.wait_task(&id).await;
    assert!(matches!(
        record.state,
        noel_durable::TaskState::Sleeping { .. }
    ));
    let graph = harness.task_graph().await;
    assert!(matches!(
        graph.tasks[&id].state,
        TaskGraphState::Sleeping {
            until: Some(2_000),
            ..
        }
    ));
    let golden = load_fixture("durable-taskGraph.json");
    assert_eq!(
        golden["outcome"]["inspection"],
        json!({ "kind": "sleeping", "until": 2_000 })
    );
    assert_eq!(
        golden["outcome"]["graph"],
        json!({ "status": "sleeping", "phase": "after", "until": 2_000 })
    );
    harness.close().await.expect("close");
}

const COUNTER: noel_durable::Document<u64> = define_doc("test.viewer", 1);

#[tokio::test]
async fn commit_observers_publish_stored_records_and_read_only_never_writes() {
    let memory = Arc::new(MemoryJournal::new("memory://viewer"));
    let storage: Storage = memory.clone();
    let harness = Harness::open(storage.clone(), HarnessOptions::new("/tmp"))
        .await
        .expect("writer");
    let mut commits = harness.observe_commits();
    harness
        .conversation()
        .write_document(&COUNTER, "value", &7)
        .await
        .expect("write");
    let publication = commits.next().await.expect("publication");
    assert_eq!(publication.kind, "documents");
    let observers = load_fixture("durable-observers.json");
    assert_eq!(observers["outcome"]["count"], json!(2));
    assert_eq!(
        observers["outcome"]["changes"],
        json!([["conversation"], ["entry"]])
    );
    harness.close().await.expect("close writer");

    let before = memory.bytes();
    let viewer = Harness::open_read_only(storage, HarnessOptions::new("/tmp"))
        .await
        .expect("viewer");
    assert_eq!(viewer.inspect().await.scheduling, SchedulingState::Paused);
    let read_only = load_fixture("durable-readOnly.json");
    assert_eq!(read_only["outcome"]["scheduling"], json!("paused"));
    assert_eq!(read_only["outcome"]["wake"], json!({ "active": false }));
    assert_eq!(read_only["outcome"]["refused"], json!("ReadOnlySession"));
    assert_eq!(
        viewer.conversation().read_document(&COUNTER, "value").await,
        Some(7)
    );
    let refused = viewer
        .conversation()
        .write_document(&COUNTER, "value", &8)
        .await;
    assert_eq!(refused, Err("the harness is read-only".to_owned()));
    viewer.resume();
    viewer.close().await.expect("close viewer");
    assert_eq!(memory.bytes(), before);
}

struct Step;

const REVIEWED: noel_durable::SignalToken<bool> = define_signal("test.reviewed", 1);

impl Tool for Step {
    fn name(&self) -> &str {
        "step"
    }
    fn description(&self) -> &str {
        "one step"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute<'a>(
        &'a self,
        _context: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async { ToolResult::text("done") })
    }
}

#[tokio::test]
async fn generation_hook_tasks_run_at_their_boundaries() {
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_in_task = completed.clone();
    let hook = TaskDefinition::new("test.hook", 1, |_: &Value| State {
        phase: "run".into(),
    })
    .phase(
        "run",
        phase(move |mut context: TaskContext<Value, State>| {
            let completed = completed_in_task.clone();
            async move {
                completed.fetch_add(1, Ordering::SeqCst);
                context.commit(TaskCommit::Completed(json!(null))).await;
            }
        }),
    );

    let script = Arc::new(Scripted::default());
    script.replies.lock().unwrap().extend([
        ScriptedReply::Events(Scripted::text("answer")),
        ScriptedReply::Events(Scripted::tool_call("step", json!({}))),
    ]);
    let request = || vec![HookTask::new("test.hook", json!(null))];
    let mut options = options_with(script, ToolRegistry::new().with(Arc::new(Step)));
    options.tasks = TaskRegistry::new().register(hook);
    options.hooks = HookRegistry::new()
        .after_tools(move |_| request())
        .on_yield(move |_| request())
        .after_run(move |_| request());
    let mut harness = Harness::in_memory(options).await.expect("open");
    let settled = harness
        .conversation()
        .submit(Input::prompt("work"), InputId::Fresh)
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done());
    // The answer settles first; a following request waits behind afterRun.
    let _ = harness.inspect().await;
    assert_eq!(completed.load(Ordering::SeqCst), 3);
    let golden = load_fixture("durable-afterRun.json");
    assert_eq!(golden["outcome"], json!({ "status": "done", "runs": 1 }));
    let on_yield = load_fixture("durable-onYield.json");
    assert_eq!(on_yield["outcome"], json!({ "status": "done", "runs": 1 }));
    let after_tools = load_fixture("durable-afterTools.json");
    assert_eq!(
        after_tools["outcome"],
        json!({ "status": "done", "runs": 1, "requests": 2 })
    );
    let points = harness
        .take_events()
        .await
        .into_iter()
        .filter_map(|event| match event {
            OwnedEvent::HookTasksCompleted { point, .. } => Some(point),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        points,
        vec![
            HookPoint::AfterTools,
            HookPoint::OnYield,
            HookPoint::AfterRun
        ]
    );
    harness.close().await.expect("close");
}

#[tokio::test]
async fn a_parked_on_yield_task_holds_submission_settlement_until_signalled() {
    let review = TaskDefinition::new("test.parked-review", 1, |_: &()| State {
        phase: "wait".into(),
    })
    .phase(
        "wait",
        phase(|mut context: TaskContext<(), State>| async move {
            context
                .commit(TaskCommit::Wait {
                    until: None,
                    signals: vec![(REVIEWED.name.to_owned(), "answer".to_owned())],
                    checkpoint: json!({ "phase": "done" }),
                })
                .await;
        }),
    )
    .phase(
        "done",
        phase(|mut context: TaskContext<(), State>| async move {
            context.commit(TaskCommit::Completed(json!({}))).await;
        }),
    );
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("answer")));
    let mut options = options_with(script, ToolRegistry::new());
    options.tasks = TaskRegistry::new().register(review);
    options.hooks =
        HookRegistry::new().on_yield(|_| vec![HookTask::new("test.parked-review", json!(null))]);
    let harness = Harness::in_memory(options).await.expect("open");
    let submission = harness
        .conversation()
        .submit(Input::prompt("review this"), InputId::Fresh)
        .await
        .expect("submit");
    let waiter = tokio::spawn(submission.wait());
    let review_id = loop {
        if let Some(node) = harness
            .task_graph()
            .await
            .tasks
            .values()
            .find(|node| node.kind == "test.parked-review")
        {
            break node.id.clone();
        }
        tokio::task::yield_now().await;
    };
    assert!(!waiter.is_finished());
    assert_eq!(
        harness
            .conversation()
            .signal(&review_id, &REVIEWED, "answer", &true)
            .await,
        noel_durable::SignalResult::Delivered
    );
    assert!(waiter.await.expect("wait task").is_done());
    harness.close().await.expect("close");
}

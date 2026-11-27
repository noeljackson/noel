//! Slice 7: owned child tasks, joins, completing and bottom-up abort.

use noel_durable::{
    phase, Harness, HarnessOptions, JoinPolicy, TaskCommit, TaskContext, TaskDefinition,
    TaskOutcome, TaskRegistry, TaskState,
};
use noel_parity::load_fixture;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize)]
struct State {
    phase: String,
}

fn child() -> TaskDefinition<bool, State> {
    TaskDefinition::new("test.child", 1, |_| State {
        phase: "run".into(),
    })
    .phase(
        "run",
        phase(|mut ctx: TaskContext<bool, State>| async move {
            if *ctx.input {
                ctx.commit(TaskCommit::Failed("child failed".into())).await;
            } else {
                ctx.commit(TaskCommit::Completed(json!("child done"))).await;
            }
        }),
    )
    .abort(phase(|mut ctx: TaskContext<bool, State>| async move {
        ctx.commit(TaskCommit::Aborted).await;
    }))
}

fn parent() -> TaskDefinition<(), State> {
    TaskDefinition::new("test.parent", 1, |_| State {
        phase: "run".into(),
    })
    .phase(
        "run",
        phase(|mut ctx: TaskContext<(), State>| async move {
            let _child = ctx
                .create_child_task("test.child", json!(false), false)
                .await
                .expect("create child");
            // The outcome is held as `completing` until the child ends.
            ctx.commit(TaskCommit::Completed(json!("parent done")))
                .await;
        }),
    )
    .abort(phase(|mut ctx: TaskContext<(), State>| async move {
        ctx.commit(TaskCommit::Aborted).await;
    }))
}

fn options() -> HarnessOptions {
    let mut options = HarnessOptions::new("/tmp");
    options.tasks = TaskRegistry::new().register(child()).register(parent());
    options
}

#[tokio::test]
async fn a_parent_completes_only_after_its_owned_child() {
    let harness = Harness::in_memory(options()).await.expect("open");
    let mut conversation = harness.conversation();
    let parent = conversation
        .create_task("test.parent", json!(null))
        .await
        .expect("create parent");
    let ended = conversation.wait_task(&parent).await;
    assert!(matches!(
        ended.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    let golden = load_fixture("durable-childTasks.json");
    assert_eq!(golden["outcome"]["outcome"], json!("completed"));
    assert_eq!(golden["outcome"]["result"], json!("parent done"));
    assert_eq!(golden["outcome"]["ownedChildren"], json!(0));
    harness.close().await.expect("close");
}

#[tokio::test]
async fn joins_apply_any_all_settled_and_fail_fast() {
    let harness = Harness::in_memory(options()).await.expect("open");
    let mut conversation = harness.conversation();
    let first = conversation
        .create_task("test.child", json!(false))
        .await
        .unwrap();
    let second = conversation
        .create_task("test.child", json!(false))
        .await
        .unwrap();
    assert_eq!(
        conversation
            .join_tasks(vec![first.clone(), second.clone()], JoinPolicy::Any)
            .await
            .len(),
        1
    );
    assert_eq!(
        conversation
            .join_tasks(vec![first, second], JoinPolicy::AllSettled)
            .await
            .len(),
        2
    );

    let failed = conversation
        .create_task("test.child", json!(true))
        .await
        .unwrap();
    let cancelled = conversation
        .create_task("test.child", json!(false))
        .await
        .unwrap();
    let records = conversation
        .join_tasks(vec![failed, cancelled], JoinPolicy::FailFast)
        .await;
    assert!(matches!(
        records[0].state,
        TaskState::Terminal {
            outcome: TaskOutcome::Failed { .. }
        }
    ));
    assert!(matches!(
        records[1].state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted
        }
    ));
    harness.close().await.expect("close");
}

#[tokio::test]
async fn abort_cascades_to_the_owned_subtree() {
    let harness = Harness::in_memory(options()).await.expect("open");
    let mut conversation = harness.conversation();
    let root = conversation
        .create_task("test.child", json!(false))
        .await
        .unwrap();
    let child = conversation
        .create_child_task(&root, "test.child", json!(false), false)
        .await
        .unwrap();
    let grandchild = conversation
        .create_child_task(&child, "test.child", json!(false), false)
        .await
        .unwrap();
    conversation.abort_task(&root).await.expect("abort tree");
    for id in [root, child, grandchild] {
        assert!(matches!(
            conversation.wait_task(&id).await.state,
            TaskState::Terminal {
                outcome: TaskOutcome::Aborted
            }
        ));
    }
    harness.close().await.expect("close");
}

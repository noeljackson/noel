//! Slice 8: typed documents, task-atomic writes, deferred batches and the
//! conversation-created event.

use std::sync::Arc;

use noel_core::storage::Storage;
use noel_durable::{
    define_doc, phase, DocumentBatch, Harness, HarnessOptions, MemoryJournal, OwnedEvent,
    TaskCommit, TaskContext, TaskDefinition, TaskRegistry,
};
use noel_parity::load_fixture;
use serde::{Deserialize, Serialize};
use serde_json::json;

const COUNTERS: noel_durable::Document<u64> = define_doc("test.counters", 1);

#[derive(Serialize, Deserialize)]
struct State {
    phase: String,
}

fn options() -> HarnessOptions {
    let count = TaskDefinition::new("test.count", 1, |_: &()| State {
        phase: "run".into(),
    })
    .phase(
        "run",
        phase(|mut ctx: TaskContext<(), State>| async move {
            let value = ctx.read_doc(&COUNTERS, "runs").await.unwrap_or_default() + 1;
            ctx.write_doc(&COUNTERS, "runs", &value);
            ctx.commit(TaskCommit::Completed(json!(value))).await;
        }),
    );
    let mut options = HarnessOptions::new("/tmp");
    options.tasks = TaskRegistry::new().register(count);
    options
}

#[tokio::test]
async fn a_task_checkpoint_and_its_document_write_share_one_record() {
    let memory = Arc::new(MemoryJournal::new("memory://documents"));
    let storage: Storage = memory.clone();
    let mut harness = Harness::open(storage.clone(), options())
        .await
        .expect("open");
    assert!(harness
        .take_events()
        .await
        .iter()
        .any(|event| matches!(event, OwnedEvent::ConversationCreated { .. })));
    let mut conversation = harness.conversation();
    let id = conversation
        .create_task("test.count", json!(null))
        .await
        .unwrap();
    conversation.wait_task(&id).await;
    assert_eq!(conversation.read_document(&COUNTERS, "runs").await, Some(1));
    let journal = String::from_utf8(memory.bytes()).expect("journal utf8");
    assert!(journal
        .lines()
        .any(|line| { line.contains("\"type\":\"task\"") && line.contains("\"documents\":[") }));
    harness.close().await.expect("close");

    let reopened = Harness::open(storage, options()).await.expect("reopen");
    assert_eq!(
        reopened
            .conversation()
            .read_document(&COUNTERS, "runs")
            .await,
        Some(1)
    );
    reopened.close().await.expect("close");
}

#[tokio::test]
async fn a_deferred_batch_is_one_atomic_document_commit() {
    let harness = Harness::in_memory(options()).await.expect("open");
    let conversation = harness.conversation();
    let batch = DocumentBatch::new()
        .set(&COUNTERS, "a", &1)
        .set(&COUNTERS, "b", &2);
    conversation
        .commit_documents(batch)
        .await
        .expect("commit batch");
    assert_eq!(conversation.read_document(&COUNTERS, "a").await, Some(1));
    assert_eq!(conversation.read_document(&COUNTERS, "b").await, Some(2));

    let golden = load_fixture("durable-documents.json");
    assert_eq!(
        golden["outcome"],
        json!({
            "latest": { "text": "after goodbye" },
            "first": { "text": "after hello" },
            "second": { "text": "after goodbye" },
        })
    );
    harness.close().await.expect("close");
}

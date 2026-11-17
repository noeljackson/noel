//! The SDK's durability premise (the plan's slice 1 acceptance): a test
//! outside `noel-cli` runs a session to its answer, and opening the same
//! journal again resumes a run a stop cut off, with the same `requestId`
//! returning the first submission.

mod common;

use std::sync::Arc;

use common::{options_with, Scripted, ScriptedReply};
use noel_durable::{Harness, Input, InputId};

fn journal(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("noel-durable-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    dir.join("session.jsonl")
}

/// A prompt answered, then the same journal reopened: the session and its
/// answer are there, and the same `requestId` returns the first
/// submission without a model request.
#[tokio::test]
async fn reopening_a_journal_keeps_the_session() {
    let journal = journal("reopen");
    let _ = std::fs::remove_file(&journal);
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("Paris.")));
    let storage: noel_core::storage::Storage =
        Arc::new(noel_core::storage::JsonlFile::open(journal.clone()).expect("journal"));
    let harness = Harness::open(storage, options_with(script, Default::default()))
        .await
        .expect("open");
    let settled = harness
        .conversation()
        .submit(
            Input::prompt("Capital of France?"),
            InputId::RequestId("capital".into()),
        )
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done());
    harness.close().await.expect("close");

    // Open the same journal again: the answered submission is settled, and
    // the same requestId repeats it without writing.
    let script = Arc::new(Scripted::default());
    let storage: noel_core::storage::Storage =
        Arc::new(noel_core::storage::JsonlFile::open(journal).expect("journal"));
    let harness = Harness::open(storage, options_with(script.clone(), Default::default()))
        .await
        .expect("reopen");
    let repeated = harness
        .conversation()
        .submit(
            Input::prompt("Capital of France?"),
            InputId::RequestId("capital".into()),
        )
        .await
        .expect("repeat");
    assert!(
        matches!(repeated.state(), noel_durable::SubmissionState::Settled(_)),
        "the repeat is settled as the first was: {:?}",
        repeated.state()
    );
    assert!(
        script.requests.lock().unwrap().is_empty(),
        "a repeat makes no model request"
    );
    harness.close().await.expect("close");
}

/// A run a stop cut off resumes at the next open (the plan's premise: the
/// same storage gets the same work back). The first reply holds; the
/// harness closes mid-run; reopening continues it to its answer.
#[tokio::test]
async fn an_interrupted_run_resumes_at_reopen() {
    let journal = journal("resume");
    let _ = std::fs::remove_file(&journal);
    let script = Arc::new(Scripted::default());
    let (_release, released) = tokio::sync::oneshot::channel::<Vec<noel_ai::StreamEvent>>();
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Hold(released));
    let storage: noel_core::storage::Storage =
        Arc::new(noel_core::storage::JsonlFile::open(journal.clone()).expect("journal"));
    let harness = Harness::open(storage, options_with(script, Default::default()))
        .await
        .expect("open");
    let submitted = harness
        .conversation()
        .submit(
            Input::prompt("Tell me something."),
            InputId::RequestId("tell".into()),
        )
        .await
        .expect("submit");
    // The provider call holds. Closing the harness stops the run as a
    // close (no restart counted), the journal keeping its state.
    let _ = submitted;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), harness.close()).await;
    assert!(outcome.is_ok(), "close waits out the held run");

    // Reopen: the run resumes, and the same requestId returns its first
    // submission (still placed in that run).
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("Something.")));
    let storage: noel_core::storage::Storage =
        Arc::new(noel_core::storage::JsonlFile::open(journal).expect("journal"));
    let harness = Harness::open(storage, options_with(script.clone(), Default::default()))
        .await
        .expect("reopen");
    // The resumed run drives at open; the same requestId repeats as the
    // submission placed in it, and waiting for it gives its answer.
    let repeated = harness
        .conversation()
        .submit(
            Input::prompt("Tell me something."),
            InputId::RequestId("tell".into()),
        )
        .await
        .expect("repeat");
    let settled = repeated.wait().await;
    assert!(settled.is_done(), "the resumed run answers: {settled:?}");
    harness.close().await.expect("close");
}

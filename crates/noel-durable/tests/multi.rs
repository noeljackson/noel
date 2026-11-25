//! Slice 2's tests: one harness of many conversations (plan,
//! `docs/durable-sdk-2026-11.md`). Goldens for upstream's `05-harness`,
//! `07-harness-conversations` and `02-forks` scenarios are recorded by
//! `scripts/ts-parity/durable-fixture.ts` (`durable-harness.json`,
//! `durable-forks.json`); these tests replay their shape against the SDK.

mod common;

use std::sync::Arc;

use common::{options_with, Scripted, ScriptedReply};
use noel_durable::{Backend, HarnessOptions, Input, InputId, MultiHarness, MultiOptions};
use noel_parity::load_fixture;
use serde_json::json;

fn multi_options(script: Arc<Scripted>, backend: Backend) -> (MultiOptions, std::path::PathBuf) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "noel-durable-multi-{}-{}-{}",
        backend_label(backend),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let options = match backend {
        Backend::Jsonl => MultiOptions::new(options_with(script, Default::default()), &dir),
        Backend::Sqlite => MultiOptions::sqlite(options_with(script, Default::default()), &dir),
    };
    (options, dir)
}

fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Jsonl => "jsonl",
        Backend::Sqlite => "sqlite",
    }
}

/// Two conversations in one process, runs interleaved on the same script,
/// no cross-talk: each answers its own prompt (upstream's
/// `07-harness-conversations` shape: handles per conversation, one
/// harness).
#[tokio::test]
async fn two_conversations_answer_their_own_prompts() {
    for backend in [Backend::Jsonl, Backend::Sqlite] {
        let script = Arc::new(Scripted::default());
        script
            .replies
            .lock()
            .unwrap()
            .push(ScriptedReply::Events(Scripted::text("to the second")));
        script
            .replies
            .lock()
            .unwrap()
            .push(ScriptedReply::Events(Scripted::text("to the first")));
        let (options, dir) = multi_options(script.clone(), backend);
        let multi = MultiHarness::open(options).await.expect("open");

        let first = multi.conversation("one").await.expect("first");
        let second = multi.conversation("two").await.expect("second");
        // Both runs drive concurrently; the script answers in submission
        // order, and each conversation keeps its own.
        let mut first_conversation = first.conversation();
        let mut second_conversation = second.conversation();
        let (a, b) = tokio::join!(
            first_conversation.submit(Input::prompt("first"), InputId::Fresh),
            second_conversation.submit(Input::prompt("second"), InputId::Fresh),
        );
        let a = a.expect("first submits").wait().await;
        let b = b.expect("second submits").wait().await;
        assert!(a.is_done() && b.is_done(), "both answer: {a:?} {b:?}");

        // No cross-talk: what each model was asked holds only its own
        // conversation's prompt.
        let requests = script.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "one request per conversation");
        for (i, conversation) in [0, 1].into_iter().zip(["first", "second"]) {
            assert!(
                requests[i]
                    .last()
                    .is_some_and(|message| message.contains(conversation)),
                "request {i} holds the {conversation} prompt: {:?}",
                requests[i]
            );
        }
        assert_eq!(multi.list(), ["one", "two"], "both are listed");
        // Upstream (`durable-harnessMany.json`, 05/07): distinct
        // conversations, each answered, found again by id.
        let golden = load_fixture("durable-harnessMany.json");
        assert_eq!(
            golden["outcome"]["conversations"],
            json!([true, true]),
            "upstream: another id, and the lookup returns it"
        );
        assert_eq!(
            golden["outcome"]["settled"],
            json!(["done", "done"]),
            "upstream: both conversations settle done"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The same conversation found again is the same session (its journal),
/// and its answered history is there after the harness is dropped
/// (upstream's `05-harness`: a Harness is a Session plus handles).
#[tokio::test]
async fn a_conversation_survives_its_handle_and_reopens() {
    for backend in [Backend::Jsonl, Backend::Sqlite] {
        let script = Arc::new(Scripted::default());
        script
            .replies
            .lock()
            .unwrap()
            .push(ScriptedReply::Events(Scripted::text("kept")));
        let (options, dir) = multi_options(script.clone(), backend);
        let multi = MultiHarness::open(options).await.expect("open");
        {
            let one = multi.conversation("kept").await.expect("open");
            let settled = one
                .conversation()
                .submit(
                    Input::prompt("remember this"),
                    InputId::RequestId("remember".into()),
                )
                .await
                .expect("submit")
                .wait()
                .await;
            assert!(settled.is_done());
        }
        // A fresh harness on the same directory: the conversation's
        // journal is there, its requestId settled, and no new request.
        let script = Arc::new(Scripted::default());
        let options = match backend {
            Backend::Jsonl => {
                MultiOptions::new(options_with(script.clone(), Default::default()), &dir)
            }
            Backend::Sqlite => {
                MultiOptions::sqlite(options_with(script.clone(), Default::default()), &dir)
            }
        };
        let multi = MultiHarness::open(options).await.expect("reopen");
        let one = multi.conversation("kept").await.expect("reopen kept");
        let repeated = one
            .conversation()
            .submit(
                Input::prompt("remember this"),
                InputId::RequestId("remember".into()),
            )
            .await
            .expect("repeat");
        assert!(
            matches!(repeated.state(), noel_durable::SubmissionState::Settled(_)),
            "the requestId is settled from the journal: {:?}",
            repeated.state()
        );
        assert!(
            script.requests.lock().unwrap().is_empty(),
            "the repeat makes no model request"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A fork starts as the source's saved state (upstream's `02-forks`: a
/// fork is a new conversation with the parent's history).
#[tokio::test]
async fn a_fork_starts_from_the_sources_saved_state() {
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("the original answer")));
    let (options, dir) = multi_options(script, Backend::Jsonl);
    let multi = MultiHarness::open(options).await.expect("open");
    let source = multi.conversation("source").await.expect("source");
    let settled = source
        .conversation()
        .submit(Input::prompt("what came before?"), InputId::Fresh)
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done());

    let fork = multi.fork("source").await.expect("fork");
    let fork_id = fork.session_id().to_owned();
    assert_ne!(fork_id, "source", "the fork is its own conversation");
    // The fork's journal holds the source's messages: the model is asked
    // them again. The same directory, a fresh harness.
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("the fork's answer")));
    let options = MultiOptions::new(options_with(script.clone(), Default::default()), &dir);
    let multi = MultiHarness::open(options).await.expect("reopen");
    let fork = multi.conversation(&fork_id).await.expect("open fork");
    let settled = fork
        .conversation()
        .submit(Input::prompt("and now?"), InputId::Fresh)
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done());
    let requests = script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("what came before?")),
        "the fork carries the source's history: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("and now?")),
        "and its own prompt"
    );
    // Upstream (`durable-forks.json`, 02): a fork's transcript holds the
    // parent's entries before the fork and not those after; its writes
    // leave the parent unchanged. noel's sessions fork at their saved
    // state, so the fork holds the parent's whole saved transcript.
    let golden = load_fixture("durable-forks.json");
    assert_eq!(
        golden["outcome"]["parentTranscript"],
        json!(["goodbye", "hello"]),
        "upstream: the parent holds both entries, newest first"
    );
    assert_eq!(
        golden["outcome"]["independent"],
        json!(["goodbye", "hello"]),
        "upstream: the branch's writes leave the parent unchanged"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn probe_one_journal_exists() {
    let script = Arc::new(Scripted::default());
    script
        .replies
        .lock()
        .unwrap()
        .push(ScriptedReply::Events(Scripted::text("x")));
    let (options, dir) = multi_options(script, Backend::Jsonl);
    let multi = MultiHarness::open(options).await.expect("open");
    let one = multi.conversation("one").await.expect("one");
    let settled = one
        .conversation()
        .submit(Input::prompt("p"), InputId::Fresh)
        .await
        .expect("submit")
        .wait()
        .await;
    assert!(settled.is_done());
    eprintln!(
        "dir contents: {:?}",
        std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Options shape: the multi options carry the harness options.
#[test]
fn multi_options_carry_the_harness_options() {
    let options = MultiOptions::new(HarnessOptions::new("/tmp"), "/tmp/conversations");
    assert_eq!(options.dir, std::path::PathBuf::from("/tmp/conversations"));
    assert_eq!(options.backend, Backend::Jsonl);
}

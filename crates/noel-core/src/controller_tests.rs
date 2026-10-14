//! The session controller: submissions while busy, `requestId`s, withdraw,
//! abort under each policy, resuming after a crash, and journals written by
//! noel before the controller.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use noel_ai::{MediaInput, Provider, ProviderError, ProviderRequest, StreamEvent};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::controller::{
    AbortPolicy, Busy, Enqueued, Event, Host, Image, Outcome, Repeat, SessionController,
    Submission, SubmissionState, SubmitError, Submitted, WhenBusy, Withdrawal,
};
use crate::{
    new_session_id, QueueItem, QueueItemId, QueueMode, ReloadableSystems, RunStart, RunStatus,
    Runtime, SessionStore, SubmissionKind, TurnEvent,
};

/// One scripted reply: optionally wait for a gate, or never answer, then
/// say `text`.
struct Step {
    gate: Option<Arc<Notify>>,
    hangs: bool,
    text: &'static str,
}

fn says(text: &'static str) -> Step {
    Step {
        gate: None,
        hangs: false,
        text,
    }
}

fn after(gate: &Arc<Notify>, text: &'static str) -> Step {
    Step {
        gate: Some(gate.clone()),
        hangs: false,
        text,
    }
}

fn hangs() -> Step {
    Step {
        gate: None,
        hangs: true,
        text: "",
    }
}

/// Answers each request with the next step, and keeps each request's user
/// and assistant texts.
#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Vec<String>>>,
}

struct Scripted(Arc<Script>);

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        unreachable!("runs stream their replies")
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let texts = request
            .messages
            .iter()
            .map(|message| message.content.clone());
        self.0.requests.lock().unwrap().push(texts.collect());
        let step = self.0.steps.lock().unwrap().pop_front();
        let step = step.expect("a request the script did not expect");
        if let Some(gate) = step.gate {
            gate.notified().await;
        }
        if step.hangs {
            std::future::pending::<()>().await;
        }
        on_event(StreamEvent::Text(step.text.into()))
    }
}

/// The controller's events, as short lines.
type Lines = Arc<Mutex<Vec<String>>>;

struct TestHost {
    script: Arc<Script>,
    events: Lines,
}

impl Host for TestHost {
    async fn provider(&mut self, _runtime: &Runtime) -> Result<Box<dyn Provider>, String> {
        Ok(Box::new(Scripted(self.script.clone())))
    }

    fn media(&self, images: &[Image]) -> Result<Vec<MediaInput>, String> {
        match images {
            [] => Ok(Vec::new()),
            _ => Err("no images here".into()),
        }
    }

    fn expand(&self, text: &str) -> String {
        text.to_owned()
    }

    fn event(&mut self, event: Event<'_>) {
        let line = match event {
            Event::RunStarted { .. } => "run started".to_owned(),
            Event::Turn {
                event: TurnEvent::MessageAdded(message),
                ..
            } => format!("{:?} {}", message.role, message.content).to_lowercase(),
            Event::Placed { submissions, .. } => format!("placed {}", requests(submissions)),
            Event::Withdrawn { submissions } => {
                let ids = submissions.iter().map(|item| item.id.clone());
                format!("withdrawn {}", requests(&ids.collect::<Vec<_>>()))
            }
            Event::QueueChanged { queue } => {
                format!("queue {:?} {:?}", queue.steering(), queue.follow_ups())
            }
            Event::RunSettled { outcome, .. } => format!("settled {outcome:?}"),
            _ => return,
        };
        self.events.lock().unwrap().push(line);
    }
}

/// The request ids of `ids`, or `-` for none.
fn requests(ids: &[QueueItemId]) -> String {
    let ids = ids.iter().map(|id| id.request_id.as_deref().unwrap_or("-"));
    ids.collect::<Vec<_>>().join(",")
}

/// A session in its own directory, with a controller over it.
struct Fixture {
    controller: SessionController<TestHost>,
    script: Arc<Script>,
    events: Lines,
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str, policy: AbortPolicy) -> Self {
        let base =
            std::env::temp_dir().join(format!("noel-controller-{name}-{}", new_session_id()));
        let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
        let path = store.path().to_path_buf();
        Self::open(Runtime::with_store(state, systems(), store), path, policy)
    }

    /// The session at `path`, opened as a new process would.
    fn reopen(path: &Path, policy: AbortPolicy) -> Self {
        let (store, state) = SessionStore::open(path.to_path_buf()).unwrap();
        Self::open(
            Runtime::with_store(state, systems(), store),
            path.to_path_buf(),
            policy,
        )
    }

    fn open(runtime: Runtime, path: PathBuf, policy: AbortPolicy) -> Self {
        let script = Arc::new(Script::default());
        let events = Lines::default();
        let host = TestHost {
            script: script.clone(),
            events: events.clone(),
        };
        Self {
            controller: SessionController::new(runtime, host, policy),
            script,
            events,
            path,
        }
    }

    fn script(&self, steps: impl IntoIterator<Item = Step>) {
        self.script.steps.lock().unwrap().extend(steps);
    }

    /// Starts a run with `prompt`; its run id.
    async fn start(&mut self, prompt: &str, request_id: &str, actions: Vec<Action>) -> String {
        let submission = input(SubmissionKind::Prompt, prompt, Some(request_id));
        let Ok(Submitted::Started(start)) = self.controller.submit(submission).await else {
            panic!("{prompt} starts a run");
        };
        let run_id = start.run_id().to_owned();
        let (sender, mut inbox) = mpsc::unbounded_channel();
        for action in actions {
            sender.send(action).unwrap();
        }
        let run = self
            .controller
            .run(start, &mut inbox, |action, busy| action(busy));
        let ran = tokio::time::timeout(Duration::from_secs(10), run).await;
        assert!(ran.expect("the run settles"));
        run_id
    }

    fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    fn requests(&self) -> Vec<Vec<String>> {
        self.script.requests.lock().unwrap().clone()
    }

    /// The journal, one record per line.
    fn journal(&self) -> Vec<Value> {
        journal(&self.path)
    }

    /// The queue records, without their type.
    fn queue_records(&self) -> Vec<Value> {
        let records = self.journal().into_iter();
        let mut queues = records.filter(|record| record["type"] == "queued_messages_snapshot");
        let queues = queues.by_ref().map(|mut record| {
            record.as_object_mut().unwrap().remove("type");
            record
        });
        queues.collect()
    }

    /// The `placed` field of each user message, by its text.
    fn placements(&self) -> Vec<(String, Value)> {
        let records = self.journal().into_iter();
        let messages = records
            .filter(|record| record["type"] == "message" && record["message"]["role"] == "user");
        let placements = messages.map(|record| {
            let text = record["message"]["content"].as_str().unwrap().to_owned();
            (text, record.get("placed").cloned().unwrap_or(Value::Null))
        });
        placements.collect()
    }
}

/// Something the test does while a run is active.
type Action = Box<dyn FnOnce(&mut Busy<'_, TestHost>) + Send>;

fn act(action: impl FnOnce(&mut Busy<'_, TestHost>) + Send + 'static) -> Action {
    Box::new(action)
}

fn release(gate: &Arc<Notify>) -> Action {
    let gate = gate.clone();
    act(move |_| gate.notify_one())
}

fn systems() -> ReloadableSystems {
    let mut systems = ReloadableSystems::default();
    systems.retry.enabled = false;
    systems
}

fn input(kind: SubmissionKind, text: &str, request_id: Option<&str>) -> Submission {
    Submission {
        kind,
        request_id: request_id.map(str::to_owned),
        content: text.into(),
        images: Vec::new(),
        expected_run_id: None,
    }
}

fn steer(text: &str, request_id: &str) -> Submission {
    input(SubmissionKind::Steer, text, Some(request_id))
}

fn follow_up(text: &str, request_id: &str) -> Submission {
    input(SubmissionKind::FollowUp, text, Some(request_id))
}

fn queued(result: Result<Enqueued, SubmitError>) -> QueueItemId {
    match result {
        Ok(Enqueued::Queued(id)) => id,
        other => panic!("not queued: {other:?}"),
    }
}

fn repeated(result: Result<Enqueued, SubmitError>) -> Repeat {
    match result {
        Ok(Enqueued::Repeated(repeat)) => repeat,
        other => panic!("not repeated: {other:?}"),
    }
}

async fn repeated_prompt(fixture: &mut Fixture, request_id: &str) -> Repeat {
    let submission = input(SubmissionKind::Prompt, "again", Some(request_id));
    match fixture.controller.submit(submission).await {
        Ok(Submitted::Repeated(repeat)) => repeat,
        Ok(Submitted::Started(_)) => panic!("{request_id} started another run"),
        Err(error) => panic!("{request_id} refused: {error}"),
    }
}

fn journal(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn settled(run_id: &str, status: RunStatus) -> SubmissionState {
    SubmissionState::Settled {
        run_id: run_id.into(),
        status,
    }
}

#[tokio::test]
async fn while_busy_a_submission_is_rejected_or_queued_as_steering_or_a_follow_up() {
    let mut fixture = Fixture::new("when-busy", AbortPolicy::KeepQueue);
    let gate = Arc::new(Notify::new());
    fixture.script([after(&gate, "first"), says("second"), says("third")]);
    let run_id = fixture
        .start(
            "work",
            "r-1",
            vec![
                act(|busy| {
                    let nope = input(SubmissionKind::Prompt, "nope", Some("r-2"));
                    let result = busy.submit(nope, WhenBusy::Reject);
                    assert_eq!(result, Err(SubmitError::Busy));
                    assert_eq!(busy.queue().pending(), 0);
                }),
                act(|busy| {
                    let id = queued(busy.submit(steer("look", "s-1"), WhenBusy::Steer));
                    assert_eq!(id.mode, QueueMode::Steer);
                    let id = queued(busy.submit(follow_up("then", "f-1"), WhenBusy::FollowUp));
                    assert_eq!(id.mode, QueueMode::FollowUp);
                    // A prompt that would queue cannot keep its requestId.
                    let prompt = input(SubmissionKind::Prompt, "also", Some("r-3"));
                    let result = busy.submit(prompt, WhenBusy::FollowUp);
                    assert_eq!(result, Err(SubmitError::QueuedPrompt));
                }),
                release(&gate),
            ],
        )
        .await;

    // The steer joins the run's next round, and the follow-up starts its
    // next engine call.
    assert_eq!(
        fixture.take_events(),
        [
            "run started",
            "user work",
            "queue [\"look\"] []",
            "queue [\"look\"] [\"then\"]",
            "assistant first",
            "user look",
            "placed s-1",
            "assistant second",
            "placed f-1",
            "user then",
            "assistant third",
            "settled Completed",
        ]
    );
    assert_eq!(
        fixture.requests(),
        [
            vec!["work"],
            vec!["work", "first", "look"],
            vec!["work", "first", "look", "second", "then"],
        ]
    );
    // Each enqueue is saved with its identity before it is acknowledged;
    // each placement leaves the queue with the message that places it.
    let records = fixture.queue_records();
    assert_eq!(records.len(), 4, "{records:#?}");
    assert_eq!(records[0]["messages"], json!(["look"]));
    assert_eq!(records[0]["items"][0]["request_id"], "s-1");
    assert_eq!(records[0]["items"][0]["mode"], "steer");
    assert_eq!(records[1]["messages"], json!(["look", "then"]));
    assert_eq!(records[1]["steering"], 1);
    assert_eq!(records[1]["items"][1]["mode"], "follow_up");
    assert_eq!(records[2]["messages"], json!(["then"]));
    assert_eq!(records[3], json!({"messages": []}));
    let placements = fixture.placements();
    assert_eq!(placements[0], ("work".into(), Value::Null));
    assert_eq!(placements[1].0, "look");
    assert_eq!(placements[1].1[0]["request_id"], "s-1");
    assert_eq!(placements[2].0, "then");
    assert_eq!(placements[2].1[0]["request_id"], "f-1");
    // Nothing of the rejected prompt was written.
    let text = fs::read_to_string(&fixture.path).unwrap();
    assert!(!text.contains("nope") && !text.contains("r-2"), "{text}");
    assert_eq!(
        repeated_prompt(&mut fixture, "r-1").await.state,
        settled(&run_id, RunStatus::Completed)
    );
}

#[tokio::test]
async fn a_repeated_request_id_returns_the_first_submission_and_another_kind_is_refused() {
    let mut fixture = Fixture::new("request-id", AbortPolicy::KeepQueue);
    let gate = Arc::new(Notify::new());
    fixture.script([after(&gate, "first"), says("second")]);
    let journal_lines = Arc::new(Mutex::new(0));
    let path = fixture.path.clone();
    let lines = journal_lines.clone();
    let run_id = fixture
        .start(
            "work",
            "r-1",
            vec![
                act(move |busy| {
                    // The prompt that started the run, while it runs.
                    let again = input(SubmissionKind::Prompt, "work", Some("r-1"));
                    let repeat = repeated(busy.submit(again, WhenBusy::Reject));
                    assert_eq!(repeat.kind, SubmissionKind::Prompt);
                    let run_id = busy.run_id().to_owned();
                    assert_eq!(repeat.submission_id, run_id);
                    assert_eq!(repeat.state, SubmissionState::Placed { run_id });

                    let first = queued(busy.submit(steer("look", "s-1"), WhenBusy::Steer));
                    *lines.lock().unwrap() = journal(&path).len();
                    let repeat = repeated(busy.submit(steer("look", "s-1"), WhenBusy::Steer));
                    assert_eq!(repeat.submission_id, first.submission_id);
                    assert_eq!(repeat.state, SubmissionState::Queued(QueueMode::Steer));
                    // Another kind, through any door, is refused.
                    let reused = SubmitError::Reused {
                        request_id: "s-1".into(),
                        kind: SubmissionKind::Steer,
                    };
                    let result = busy.submit(follow_up("look", "s-1"), WhenBusy::FollowUp);
                    assert_eq!(result, Err(reused.clone()));
                    let prompt = input(SubmissionKind::Prompt, "look", Some("s-1"));
                    assert_eq!(busy.submit(prompt, WhenBusy::Reject), Err(reused.clone()));
                    let result = busy.enqueue(QueueMode::FollowUp, follow_up("x", "s-1"));
                    assert_eq!(result, Err(reused));
                    // Repeats and refusals write nothing.
                    assert_eq!(journal(&path).len(), *lines.lock().unwrap());
                    assert_eq!(busy.queue().pending(), 1);
                }),
                release(&gate),
            ],
        )
        .await;

    let completed = settled(&run_id, RunStatus::Completed);
    assert_eq!(repeated_prompt(&mut fixture, "r-1").await.state, completed);
    // The steer was placed into the run, which has ended.
    let repeat = repeated(
        fixture
            .controller
            .enqueue(QueueMode::Steer, steer("look", "s-1")),
    );
    assert_eq!(repeat.state, completed);
    let result = fixture
        .controller
        .enqueue(QueueMode::Steer, steer("x", "r-1"));
    let reused = SubmitError::Reused {
        request_id: "r-1".into(),
        kind: SubmissionKind::Prompt,
    };
    assert_eq!(result.unwrap_err(), reused);

    // A submission of another kind may start a run; it keeps its kind.
    fixture.script([says("steered")]);
    let submission = steer("go", "s-2");
    let Ok(Submitted::Started(start)) = fixture.controller.submit(submission).await else {
        panic!("an idle steer starts a run");
    };
    let steered = start.run_id().to_owned();
    let (_sender, mut inbox) = mpsc::unbounded_channel::<Action>();
    assert!(fixture.controller.run(start, &mut inbox, |a, b| a(b)).await);

    // Every requestId is still known after a restart.
    let path = fixture.path.clone();
    drop(fixture);
    let mut fixture = Fixture::reopen(&path, AbortPolicy::KeepQueue);
    assert_eq!(repeated_prompt(&mut fixture, "r-1").await.state, completed);
    let repeat = repeated(
        fixture
            .controller
            .enqueue(QueueMode::Steer, steer("look", "s-1")),
    );
    assert_eq!(repeat.state, completed);
    let repeat = fixture
        .controller
        .enqueue(QueueMode::Steer, steer("go", "s-2"));
    assert_eq!(
        repeated(repeat).state,
        settled(&steered, RunStatus::Completed)
    );
    let prompt = input(SubmissionKind::Prompt, "go", Some("s-2"));
    let Err(error) = fixture.controller.submit(prompt).await else {
        panic!("a prompt reusing a steer's requestId is refused");
    };
    assert_eq!(
        error,
        SubmitError::Reused {
            request_id: "s-2".into(),
            kind: SubmissionKind::Steer
        }
    );
}

#[tokio::test]
async fn withdraw_reports_withdrawn_already_placed_and_settled() {
    let mut fixture = Fixture::new("withdraw", AbortPolicy::KeepQueue);
    let look = queued(
        fixture
            .controller
            .enqueue(QueueMode::Steer, steer("look", "s-1")),
    );
    let later = queued(
        fixture
            .controller
            .enqueue(QueueMode::FollowUp, follow_up("later", "f-1")),
    );
    fixture.take_events();
    let withdrawal = fixture.controller.withdraw(&later.submission_id);
    assert_eq!(withdrawal, Ok(Withdrawal::Withdrawn));
    assert_eq!(
        fixture.take_events(),
        ["withdrawn f-1", "queue [\"look\"] []"]
    );
    let records = fixture.queue_records();
    let last = records.last().unwrap();
    assert_eq!(last["messages"], json!(["look"]));
    assert_eq!(last["withdrawn"][0]["request_id"], "f-1");
    assert_eq!(
        fixture.controller.withdraw(&later.submission_id),
        Ok(Withdrawal::Settled)
    );
    let repeat = fixture
        .controller
        .enqueue(QueueMode::FollowUp, follow_up("later", "f-1"));
    assert_eq!(repeated(repeat).state, SubmissionState::Withdrawn);
    assert!(fixture.controller.withdraw("nothing").is_err());

    // The steer joins the next run's first round.
    let gate = Arc::new(Notify::new());
    fixture.script([after(&gate, "done")]);
    let id = look.submission_id.clone();
    let run_id = fixture
        .start(
            "work",
            "r-1",
            vec![
                act(move |busy| {
                    assert_eq!(busy.withdraw(&id), Ok(Withdrawal::AlreadyPlaced));
                    let run_id = busy.run_id().to_owned();
                    assert_eq!(busy.withdraw(&run_id), Ok(Withdrawal::AlreadyPlaced));
                }),
                release(&gate),
            ],
        )
        .await;
    assert_eq!(fixture.requests(), [vec!["work", "look"]]);
    for id in [&look.submission_id, &later.submission_id, &run_id] {
        assert_eq!(fixture.controller.withdraw(id), Ok(Withdrawal::Settled));
    }

    // After a restart too.
    let path = fixture.path.clone();
    drop(fixture);
    let mut fixture = Fixture::reopen(&path, AbortPolicy::KeepQueue);
    for id in [&look.submission_id, &later.submission_id, &run_id] {
        assert_eq!(fixture.controller.withdraw(id), Ok(Withdrawal::Settled));
    }
}

/// A run stopped by an abort while a steer and a follow-up wait; what is
/// queued afterwards.
async fn abort_with_a_queue(policy: AbortPolicy) -> (Fixture, Vec<QueueItem>) {
    let mut fixture = Fixture::new("abort", policy);
    fixture.script([hangs()]);
    let run_id = fixture
        .start(
            "work",
            "r-1",
            vec![act(|busy| {
                queued(busy.submit(steer("look", "s-1"), WhenBusy::Steer));
                queued(busy.submit(follow_up("later", "f-1"), WhenBusy::FollowUp));
                assert!(busy.abort(Some("another run")).is_err());
                assert_eq!(busy.queue().pending(), 2);
                assert_eq!(busy.abort(None), Ok(()));
            })],
        )
        .await;
    let events = fixture.take_events();
    assert_eq!(events.last().unwrap(), "settled Aborted");
    let status = fixture.controller.runtime().session().runs[0].status;
    assert_eq!(status, RunStatus::Aborted);
    let ended = journal(&fixture.path).into_iter().last().unwrap();
    assert_eq!(
        ended,
        json!({"type": "run_finished", "run_id": run_id, "status": "aborted"})
    );
    let items = fixture.controller.queue().items();
    (fixture, items)
}

#[tokio::test]
async fn an_abort_keeps_the_queue_under_published_pis_rule() {
    let (mut fixture, items) = abort_with_a_queue(AbortPolicy::KeepQueue).await;
    let texts = items.iter().map(|item| item.content.as_str());
    assert_eq!(texts.collect::<Vec<_>>(), ["look", "later"]);
    let saved = fixture.queue_records().pop().unwrap();
    assert_eq!(saved["messages"], json!(["look", "later"]));
    let repeat = fixture
        .controller
        .enqueue(QueueMode::Steer, steer("look", "s-1"));
    assert_eq!(
        repeated(repeat).state,
        SubmissionState::Queued(QueueMode::Steer)
    );
    // The next prompt starts with them.
    fixture.script([says("done"), says("after")]);
    fixture.start("next", "r-2", Vec::new()).await;
    assert_eq!(
        fixture.requests().last().unwrap(),
        &["work", "next", "look", "done", "later"]
    );
}

#[tokio::test]
async fn an_abort_withdraws_the_queue_under_durables_rule() {
    let (mut fixture, items) = abort_with_a_queue(AbortPolicy::WithdrawQueue).await;
    assert!(items.is_empty());
    let saved = fixture.queue_records().pop().unwrap();
    assert_eq!(saved["messages"], json!([]));
    assert_eq!(
        saved["withdrawn"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id["request_id"].clone())
            .collect::<Vec<_>>(),
        ["s-1", "f-1"]
    );
    for (submission, mode) in [
        (steer("look", "s-1"), QueueMode::Steer),
        (follow_up("later", "f-1"), QueueMode::FollowUp),
    ] {
        let repeat = repeated(fixture.controller.enqueue(mode, submission));
        assert_eq!(repeat.state, SubmissionState::Withdrawn);
        let withdrawal = fixture.controller.withdraw(&repeat.submission_id);
        assert_eq!(withdrawal, Ok(Withdrawal::Settled));
    }
    // The next prompt starts with an empty queue.
    fixture.script([says("done")]);
    fixture.start("next", "r-2", Vec::new()).await;
    assert_eq!(fixture.requests().last().unwrap(), &["work", "next"]);

    // So does an abort with no run active.
    queued(
        fixture
            .controller
            .enqueue(QueueMode::FollowUp, follow_up("more", "f-2")),
    );
    assert_eq!(fixture.controller.abort(None), Ok(()));
    assert_eq!(fixture.controller.queue().pending(), 0);
}

#[tokio::test]
async fn an_interrupted_run_resumes_after_a_crash() {
    // A process saved a run's start, queued a steer for it, and died.
    let base = std::env::temp_dir().join(format!("noel-controller-crash-{}", new_session_id()));
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, systems(), store.clone());
    let start = RunStart {
        id: "run-1".into(),
        request_id: Some("m-1".into()),
        kind: SubmissionKind::Prompt,
    };
    runtime.start_run(start, "work".into(), Vec::new()).unwrap();
    let look = QueueItem {
        id: QueueItemId {
            submission_id: "sub-1".into(),
            request_id: Some("s-1".into()),
            mode: QueueMode::Steer,
        },
        content: "look".into(),
    };
    store
        .record_queue_items(std::slice::from_ref(&look), &[])
        .unwrap();
    drop((runtime, store));

    let mut fixture = Fixture::reopen(&path, AbortPolicy::KeepQueue);
    assert_eq!(fixture.controller.queue().items(), [look]);
    let interrupted = SubmissionState::Placed {
        run_id: "run-1".into(),
    };
    assert_eq!(
        repeated_prompt(&mut fixture, "m-1").await.state,
        interrupted
    );
    let repeat = repeated(
        fixture
            .controller
            .enqueue(QueueMode::Steer, steer("look", "s-1")),
    );
    assert_eq!(repeat.submission_id, "sub-1");
    assert_eq!(
        fixture.controller.withdraw("run-1"),
        Ok(Withdrawal::AlreadyPlaced)
    );
    assert!(fixture.controller.resume("run-2").await.is_err());

    fixture.script([says("done")]);
    let start = fixture.controller.resume("run-1").await.unwrap();
    assert_eq!(start.run_id(), "run-1");
    let (_sender, mut inbox) = mpsc::unbounded_channel::<Action>();
    assert!(fixture.controller.run(start, &mut inbox, |a, b| a(b)).await);
    assert_eq!(
        fixture.take_events(),
        [
            "run started",
            "user look",
            "placed s-1",
            "assistant done",
            "settled Completed"
        ]
    );
    assert_eq!(fixture.requests(), [vec!["work", "look"]]);
    let completed = settled("run-1", RunStatus::Completed);
    assert_eq!(repeated_prompt(&mut fixture, "m-1").await.state, completed);
    let repeat = repeated(
        fixture
            .controller
            .enqueue(QueueMode::Steer, steer("look", "s-1")),
    );
    assert_eq!(repeat.state, completed);
    let ended = journal(&path).into_iter().last().unwrap();
    assert_eq!(
        ended,
        json!({"type": "run_finished", "run_id": "run-1", "status": "completed"})
    );
}

#[tokio::test]
async fn a_journal_written_by_noel_main_opens_and_resumes_unchanged() {
    // `noel --mode rpc` at a0aad41e (before the controller): a completed
    // run for `m-1`, a run for `m-2` killed mid model request, and a steer
    // and a follow-up queued during it.
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/journals/main-a0aad41e.jsonl");
    let base = std::env::temp_dir().join(format!("noel-controller-main-{}", new_session_id()));
    fs::create_dir_all(&base).unwrap();
    let path = base.join("main-journal.jsonl");
    fs::copy(&fixture_path, &path).unwrap();
    let written = fs::read_to_string(&path).unwrap();

    let state = SessionStore::read(&path).unwrap();
    let runs = state
        .runs
        .iter()
        .map(|run| (run.request_id.as_deref().unwrap(), run.kind, run.status))
        .collect::<Vec<_>>();
    assert_eq!(
        runs,
        [
            ("m-1", SubmissionKind::Prompt, RunStatus::Completed),
            ("m-2", SubmissionKind::Prompt, RunStatus::Interrupted),
        ]
    );
    let (steering, follow_ups) = state.saved_queue();
    assert_eq!(
        (steering, follow_ups),
        (
            &["look again".to_owned()][..],
            &["then summarize".to_owned()][..]
        )
    );
    assert!(state.queued_ids.is_empty() && state.past_submissions.is_empty());
    let (first, second) = (state.runs[0].id.clone(), state.runs[1].id.clone());

    let mut fixture = Fixture::reopen(&path, AbortPolicy::KeepQueue);
    // Opening it wrote nothing.
    assert_eq!(fs::read_to_string(&path).unwrap(), written);
    let items = fixture.controller.queue().items();
    let ids = items
        .iter()
        .map(|item| (item.id.mode, item.id.request_id.clone()));
    assert_eq!(
        ids.collect::<Vec<_>>(),
        [(QueueMode::Steer, None), (QueueMode::FollowUp, None)]
    );
    assert_eq!(
        repeated_prompt(&mut fixture, "m-1").await.state,
        settled(&first, RunStatus::Completed)
    );
    assert_eq!(
        repeated_prompt(&mut fixture, "m-2").await.state,
        SubmissionState::Placed {
            run_id: second.clone()
        }
    );

    fixture.script([says("looked"), says("summary")]);
    let start = fixture.controller.resume(&second).await.unwrap();
    let (_sender, mut inbox) = mpsc::unbounded_channel::<Action>();
    assert!(fixture.controller.run(start, &mut inbox, |a, b| a(b)).await);
    assert_eq!(
        fixture.requests(),
        [
            vec!["first", "first answer", "second", "look again"],
            vec![
                "first",
                "first answer",
                "second",
                "look again",
                "looked",
                "then summarize"
            ],
        ]
    );
    // What this version appended keeps the shapes main wrote: submissions
    // without a requestId have no saved identity.
    let appended = &journal(&path)[written.lines().count()..];
    for record in appended {
        assert!(record.get("placed").is_none(), "{record}");
        assert!(record.get("items").is_none(), "{record}");
        assert!(record.get("withdrawn").is_none(), "{record}");
    }
    let state = SessionStore::read(&path).unwrap();
    assert_eq!(state.runs[1].status, RunStatus::Completed);
    assert!(state.queued_messages.is_empty());
    assert_eq!(
        fixture.controller.runtime().session().messages.len(),
        state.messages.len()
    );
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outcomes_name_the_status_they_save() {
    assert_eq!(Outcome::Completed.status(), RunStatus::Completed);
    assert_eq!(Outcome::Aborted.status(), RunStatus::Aborted);
    assert_eq!(Outcome::Failed("x".into()).status(), RunStatus::Failed);
}

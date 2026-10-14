//! How a turn recovers the reply it is asking for: a transient failure is
//! retried after a backoff, and a context overflow is answered by one
//! compaction. Either way the failed reply is dropped: never kept, and
//! absent from the request that asks again. A reply an abort or a failure
//! cuts off for good is kept as an aborted reply (spec §4.2).

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use noel_ai::{ChatRole, Provider, ProviderError, ProviderRequest, StreamEvent};
use serde_json::json;

use crate::tests::trace;
use crate::{
    continue_turn_streaming_events, new_session_id, run_user_turn_streaming_events_with_media,
    AgentError, ConversationMessage, MessageRole, ReloadableSystems, RunControl, Runtime,
    RuntimeRetrySettings, SessionState, SessionStore, TurnEvent, RETRY_CANCELLED,
};

/// One scripted answer to a request: the events it streams, then the error
/// that breaks it off, if any.
struct Step {
    events: Vec<StreamEvent>,
    error: Option<ProviderError>,
    /// Never answers after its events: the request stays in flight.
    hangs: bool,
}

fn reply(events: Vec<StreamEvent>) -> Step {
    Step {
        events,
        error: None,
        hangs: false,
    }
}

fn says(text: &str) -> Step {
    reply(vec![StreamEvent::Text(text.into())])
}

fn broken(events: Vec<StreamEvent>, error: &str) -> Step {
    Step {
        events,
        error: Some(ProviderError::InvalidResponse(error.into())),
        hangs: false,
    }
}

fn hangs() -> Step {
    Step {
        events: Vec::new(),
        error: None,
        hangs: true,
    }
}

/// A final reply cut off at the output limit after `output` tokens.
fn cut_off(text: &str, output: u64) -> Step {
    reply(vec![
        StreamEvent::Text(text.into()),
        StreamEvent::Usage {
            input_tokens: 10,
            output_tokens: output,
        },
        StreamEvent::Stop {
            reason: "length".into(),
        },
    ])
}

const OVERLOADED: &str = "overloaded";
const OVERFLOW: &str = "prompt is too long: 213462 tokens > 200000 maximum";

/// Answers each request with the next step and keeps the requests.
struct Steps {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ProviderRequest>>,
    output_limit: Option<u64>,
}

impl Steps {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            output_limit: None,
        }
    }

    /// The text of each message of request `index`, with its role.
    fn request(&self, index: usize) -> Vec<(ChatRole, String)> {
        let requests = self.requests.lock().unwrap();
        let messages = requests[index].messages.iter();
        messages
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl Provider for Steps {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        unreachable!("turns stream their replies")
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        self.requests.lock().unwrap().push(request);
        let step = self.steps.lock().unwrap().pop_front();
        let step = step.expect("a request the script did not expect");
        for event in step.events {
            on_event(event)?;
        }
        if step.hangs {
            std::future::pending::<()>().await;
        }
        step.error.map_or(Ok(()), Err)
    }

    fn output_limit(&self) -> Option<u64> {
        self.output_limit
    }
}

fn message(role: MessageRole, content: &str) -> ConversationMessage {
    ConversationMessage {
        thinking: String::new(),
        role,
        content: content.into(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    }
}

/// Retries without a real wait, and a compaction that keeps only the
/// latest turn.
fn systems() -> ReloadableSystems {
    let mut systems = ReloadableSystems {
        retry: RuntimeRetrySettings {
            base_delay_ms: 1,
            ..RuntimeRetrySettings::default()
        },
        ..ReloadableSystems::default()
    };
    systems.compaction.keep_recent_tokens = 1;
    systems
}

/// A saved session in its own directory, holding `history`.
fn session(name: &str, history: &[(MessageRole, &str)]) -> (Runtime, SessionStore, PathBuf) {
    let base = std::env::temp_dir().join(format!("noel-recovery-{name}-{}", new_session_id()));
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let mut runtime = Runtime::with_store(state, systems(), store.clone());
    for (role, content) in history {
        runtime
            .push_message(message(role.clone(), content))
            .unwrap();
    }
    (runtime, store, base)
}

/// One finished turn, long enough that a checkpoint is shorter than it.
const ONE_TURN: &[(MessageRole, &str)] = &[
    (MessageRole::User, "first"),
    (MessageRole::Assistant, FIRST_ANSWER),
];

const FIRST_ANSWER: &str = "First answer: the migration adds a nullable column, backfills it in \
batches of a thousand rows, then makes it required; the rollback drops the column again, and \
both directions were run against a copy of the production schema before this was written.";

/// Runs `prompt` as a turn; its result, and its events as trace lines.
async fn turn(
    runtime: &mut Runtime,
    provider: &Steps,
    prompt: &str,
    run: &RunControl,
) -> (Result<String, AgentError>, Vec<String>) {
    let mut events = Vec::new();
    let turn = run_user_turn_streaming_events_with_media(
        runtime,
        provider,
        prompt.into(),
        Vec::new(),
        run,
        |event: &TurnEvent| events.push(event.clone()),
    );
    let result = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn ends");
    (result, trace(&events))
}

/// The saved conversation: roles and texts.
fn saved(runtime: &Runtime) -> Vec<(MessageRole, &str)> {
    let messages = runtime.session().messages.iter();
    messages
        .map(|message| (message.role.clone(), message.content.as_str()))
        .collect()
}

/// The journal holds exactly the conversation the session has: a dropped
/// reply left no record, and nothing is recorded twice.
fn assert_journal_matches(store: &SessionStore, runtime: &Runtime) {
    assert_eq!(&store.load().unwrap(), runtime.session());
}

#[tokio::test]
async fn a_reply_that_breaks_off_is_retried_and_left_out_of_the_next_request() {
    let (mut runtime, store, base) = session("mid-stream", &[]);
    let provider = Steps::new([
        broken(vec![StreamEvent::Text("Hel".into())], "terminated"),
        says("Hello again."),
    ]);
    let (result, lines) = turn(&mut runtime, &provider, "hi", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "Hello again.");
    assert_eq!(
        lines,
        [
            "round started",
            r#"user "hi""#,
            r#"text "Hel""#,
            "retry 1 of 3 in 1ms: provider returned an invalid response: terminated",
            "round started",
            r#"text "Hello again.""#,
            r#"assistant "Hello again.""#,
            "retried 1: succeeded",
            "round finished",
        ]
    );
    // The request that asks again is the first one: the broken reply is
    // not part of it, nor of the session or its journal.
    assert_eq!(provider.request(0), [(ChatRole::User, "hi".into())]);
    assert_eq!(provider.request(1), provider.request(0));
    assert_eq!(
        saved(&runtime),
        [
            (MessageRole::User, "hi"),
            (MessageRole::Assistant, "Hello again.")
        ]
    );
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn an_error_no_retry_can_fix_ends_the_turn_at_once() {
    for error in [
        r#"{"error":{"message":"Incorrect API key provided.","code":"invalid_api_key"}}"#,
        r#"{"error":{"message":"You exceeded your current quota.","code":"insufficient_quota"}}"#,
        r#"{"error":{"message":"Unsupported parameter."}}"#,
    ] {
        let mut runtime = Runtime::new(
            SessionState::new("no-retry", PathBuf::from(".")),
            // The default policy: a retry would wait two seconds.
            ReloadableSystems::default(),
        );
        let provider = Steps::new([broken(Vec::new(), error), says("never asked")]);
        let started = Instant::now();
        let (result, lines) = turn(&mut runtime, &provider, "hi", &RunControl::default()).await;
        assert!(matches!(result, Err(AgentError::Provider(_))), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1), "{error}");
        assert_eq!(lines, ["round started", r#"user "hi""#], "{error}");
        assert_eq!(provider.request_count(), 1, "{error}");
    }
}

#[tokio::test]
async fn retries_do_not_use_up_the_round_limit() {
    let mut systems = systems();
    systems.max_tool_rounds = 1;
    let mut runtime = Runtime::new(SessionState::new("rounds", PathBuf::from(".")), systems);
    let provider = Steps::new([
        broken(Vec::new(), OVERLOADED),
        broken(Vec::new(), OVERLOADED),
        says("ok"),
    ]);
    let (result, _) = turn(&mut runtime, &provider, "hi", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "ok");
}

#[tokio::test]
async fn ending_a_backoff_early_ends_the_turn_with_the_failure_it_would_have_retried() {
    let mut systems = systems();
    systems.retry.base_delay_ms = 60_000;
    let mut runtime = Runtime::new(
        SessionState::new("abort-retry", PathBuf::from(".")),
        systems,
    );
    let provider = Steps::new([broken(Vec::new(), OVERLOADED)]);
    let run = RunControl::default();
    // Outside a backoff there is nothing to end.
    assert!(!run.retry.abort());
    let aborted = {
        let retry = run.retry.clone();
        tokio::spawn(async move {
            while !retry.abort() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Instant::now()
        })
    };
    let (result, lines) = turn(&mut runtime, &provider, "hi", &run).await;
    assert!(aborted.await.unwrap().elapsed() < Duration::from_secs(1));
    let error = result.unwrap_err();
    assert_eq!(
        error.to_string(),
        "provider returned an invalid response: overloaded"
    );
    assert_eq!(
        lines,
        [
            "round started".to_string(),
            r#"user "hi""#.to_string(),
            "retry 1 of 3 in 60000ms: provider returned an invalid response: overloaded".into(),
            format!("retried 1: {RETRY_CANCELLED}"),
        ]
    );
    assert_eq!(provider.request_count(), 1);
    // The backoff is over: a later abort ends nothing.
    assert!(!run.retry.abort());
}

#[tokio::test]
async fn a_context_overflow_is_answered_by_one_compaction_and_one_retry() {
    let (mut runtime, store, base) = session("overflow", ONE_TURN);
    let provider = Steps::new([
        broken(Vec::new(), OVERFLOW),
        says("Summary of the first turn."),
        says("Second answer."),
    ]);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "Second answer.");
    assert_eq!(
        lines,
        [
            "round started",
            r#"user "second""#,
            &format!("overflowed: provider returned an invalid response: {OVERFLOW}"),
            "compaction started",
            "compacted 2 messages",
            "round started",
            r#"text "Second answer.""#,
            r#"assistant "Second answer.""#,
            "round finished",
        ]
    );
    // The refused request, the summary, then the request again with the
    // checkpoint in place of the first turn.
    assert_eq!(provider.request_count(), 3);
    assert_eq!(provider.request(0).len(), 3);
    let again = provider.request(2);
    assert_eq!(again.len(), 2);
    assert!(
        again[0].1.ends_with("Summary of the first turn."),
        "{again:?}"
    );
    assert_eq!(again[1], (ChatRole::User, "second".into()));
    let conversation = saved(&runtime);
    assert_eq!(conversation.len(), 3);
    assert_eq!(conversation[1], (MessageRole::User, "second"));
    assert_eq!(conversation[2], (MessageRole::Assistant, "Second answer."));
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn an_overflow_without_anything_to_compact_ends_the_turn_with_its_error() {
    let (mut runtime, store, base) = session("overflow-empty", &[]);
    let provider = Steps::new([broken(Vec::new(), OVERFLOW)]);
    let (result, lines) = turn(&mut runtime, &provider, "hi", &RunControl::default()).await;
    assert!(result.unwrap_err().to_string().ends_with(OVERFLOW));
    assert_eq!(lines, ["round started", r#"user "hi""#]);
    assert_eq!(saved(&runtime), [(MessageRole::User, "hi")]);
    assert_journal_matches(&store, &runtime);

    // Nor is it compacted when automatic compaction is off.
    let (mut runtime, _, other) = session("overflow-off", ONE_TURN);
    runtime.systems.compaction.enabled = false;
    let provider = Steps::new([broken(Vec::new(), OVERFLOW)]);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert!(result.is_err());
    assert_eq!(lines, ["round started", r#"user "second""#]);
    for base in [base, other] {
        fs::remove_dir_all(base).unwrap();
    }
}

#[tokio::test]
async fn a_second_overflow_is_not_compacted_again() {
    // One turn of tool rounds, so that completed rounds can be summarized
    // more than once: only the one attempt keeps the second overflow from
    // another compaction.
    let (mut runtime, store, base) = session("overflow-once", &[]);
    let call = |id: &str| {
        reply(vec![StreamEvent::ToolCall {
            id: id.into(),
            name: "no_such_tool".into(),
            arguments: json!({}).to_string(),
        }])
    };
    let mut cut_off_call = call("c3");
    cut_off_call.events.push(StreamEvent::Stop {
        reason: "length".into(),
    });
    let provider = Steps::new([
        call("c1"),
        call("c2"),
        broken(Vec::new(), OVERFLOW),
        says("Summary of the first round."),
        // The retry is cut off while calling a tool: its call gets an error
        // result and the turn goes on, without a reply that was not cut off.
        cut_off_call,
        broken(Vec::new(), OVERFLOW),
    ]);
    let (result, lines) = turn(&mut runtime, &provider, "work", &RunControl::default()).await;
    assert!(result.unwrap_err().to_string().ends_with(OVERFLOW));
    assert_eq!(provider.request_count(), 6);
    let count = |prefix: &str| lines.iter().filter(|line| line.starts_with(prefix)).count();
    assert_eq!(count("overflowed"), 1, "{lines:#?}");
    assert_eq!(count("compacted"), 1, "{lines:#?}");
    // The second overflow had rounds to summarize, had it been allowed to.
    assert!(runtime.can_compact());
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn a_final_reply_cut_off_at_the_output_limit_is_dropped_compacted_and_asked_again() {
    let (mut runtime, store, base) = session("length", ONE_TURN);
    let provider = Steps::new([
        cut_off("This answer is cut", 5),
        says("Summary of the first turn."),
        says("The whole answer."),
    ]);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    // The cut-off reply is not part of the turn's text either.
    assert_eq!(result.unwrap(), "The whole answer.");
    assert_eq!(
        lines,
        [
            "round started",
            r#"user "second""#,
            r#"text "This answer is cut""#,
            "usage 10/5",
            "stop length",
            "overflowed: reply cut off",
            "compaction started",
            "compacted 2 messages",
            "round started",
            r#"text "The whole answer.""#,
            r#"assistant "The whole answer.""#,
            "round finished",
        ]
    );
    let again = provider.request(2);
    assert_eq!(again.len(), 2);
    assert_eq!(again[1], (ChatRole::User, "second".into()));
    let conversation = saved(&runtime);
    assert_eq!(conversation.len(), 3);
    assert_eq!(
        conversation[2],
        (MessageRole::Assistant, "The whole answer.")
    );
    assert_journal_matches(&store, &runtime);
    // The dropped reply was a request the provider answered: it is counted.
    assert_eq!(runtime.session().usage.requests, 1);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn a_cut_off_reply_stands_when_nothing_replaces_it() {
    let kept = |runtime: &Runtime| {
        let conversation = saved(runtime);
        assert_eq!(
            conversation.last(),
            Some(&(MessageRole::Assistant, "This answer is cut"))
        );
    };
    let finished = [
        "round started",
        r#"user "second""#,
        r#"text "This answer is cut""#,
        "usage 10/5",
        "stop length",
        r#"assistant "This answer is cut""#,
        "round finished",
    ];

    // Nothing to compact: the session has no older turn.
    let (mut runtime, store, base) = session("length-empty", &[]);
    let provider = Steps::new([cut_off("This answer is cut", 5)]);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "This answer is cut");
    assert_eq!(lines, finished);
    kept(&runtime);
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();

    // The model wrote all it can: its own limit cut the reply off, not the
    // context, so a smaller context would not help.
    let (mut runtime, _, base) = session("length-limit", ONE_TURN);
    let mut provider = Steps::new([cut_off("This answer is cut", 5)]);
    provider.output_limit = Some(5);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "This answer is cut");
    assert_eq!(lines, finished);
    kept(&runtime);
    fs::remove_dir_all(base).unwrap();

    // Below that limit it is recovered, as without a known limit.
    let (mut runtime, _, base) = session("length-below-limit", ONE_TURN);
    let mut provider = Steps::new([
        cut_off("This answer is cut", 4),
        says("Summary."),
        says("Whole."),
    ]);
    provider.output_limit = Some(5);
    let (result, _) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "Whole.");
    fs::remove_dir_all(base).unwrap();

    // The compaction fails (an empty summary): the reply is saved after
    // all, though its round ended when it was dropped.
    let (mut runtime, store, base) = session("length-failed", ONE_TURN);
    let provider = Steps::new([cut_off("This answer is cut", 5), says("")]);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "This answer is cut");
    assert_eq!(
        lines[5..],
        [
            "overflowed: reply cut off",
            "compaction started",
            "compaction failed"
        ]
    );
    kept(&runtime);
    assert_eq!(saved(&runtime).len(), 4);
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();

    // Asked again, the reply is cut off again: the second one stands.
    let (mut runtime, store, base) = session("length-twice", ONE_TURN);
    let provider = Steps::new([
        cut_off("First try is cut", 5),
        says("Summary."),
        cut_off("This answer is cut", 5),
    ]);
    let (result, lines) = turn(&mut runtime, &provider, "second", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "This answer is cut");
    assert_eq!(
        lines
            .iter()
            .filter(|line| *line == "overflowed: reply cut off")
            .count(),
        1
    );
    kept(&runtime);
    assert_eq!(provider.request_count(), 3);
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn cancelling_an_overflow_compaction_ends_the_turn_and_leaves_the_history_untouched() {
    let (mut runtime, store, base) = session("overflow-cancel", ONE_TURN);
    // The summary request never answers.
    let provider = Steps::new([broken(Vec::new(), OVERFLOW), hangs()]);
    let run = RunControl::default();
    let cancelled = {
        let cancellation = run.cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancellation.cancel();
            Instant::now()
        })
    };
    let (result, lines) = turn(&mut runtime, &provider, "second", &run).await;
    assert!(cancelled.await.unwrap().elapsed() < Duration::from_secs(1));
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert_eq!(lines.last().unwrap(), "compaction started");
    assert_eq!(provider.request_count(), 2);
    let history = [
        (MessageRole::User, "first"),
        (MessageRole::Assistant, FIRST_ANSWER),
        (MessageRole::User, "second"),
    ];
    assert_eq!(saved(&runtime), history);
    assert_journal_matches(&store, &runtime);

    // The saved prompt is a turn the next run continues, and recovers.
    let provider = Steps::new([
        broken(Vec::new(), OVERFLOW),
        says("Summary of the first turn."),
        says("Second answer."),
    ]);
    let run = RunControl::default();
    let continued = continue_turn_streaming_events(&mut runtime, &provider, &run, |_| {});
    assert_eq!(continued.await.unwrap(), "Second answer.");
    assert_eq!(saved(&runtime).len(), 3);
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn a_running_command_reports_its_output_before_its_result() {
    let mut runtime = Runtime::new(SessionState::new("update", PathBuf::from(".")), systems());
    let provider = Steps::new([
        reply(vec![StreamEvent::ToolCall {
            id: "b1".into(),
            name: "bash".into(),
            arguments: json!({"command": "printf 'one\\n'; sleep 0.3; printf 'two\\n'"})
                .to_string(),
        }]),
        says("done"),
    ]);
    let (result, lines) = turn(&mut runtime, &provider, "run it", &RunControl::default()).await;
    assert_eq!(result.unwrap(), "done");
    let start = lines
        .iter()
        .position(|line| line.starts_with("tool b1 bash"))
        .unwrap();
    assert_eq!(
        lines[start + 1..start + 6],
        [
            r#"update b1 """#,
            r#"update b1 "one\n""#,
            r#"update b1 "one\ntwo\n""#,
            r#"output b1 "one\ntwo\n""#,
            "finished bash",
        ]
    );
}

// What a reply that does not stand leaves (spec §4.2).

/// The saved conversation with each message's aborted mark.
fn saved_marked(runtime: &Runtime) -> Vec<(MessageRole, &str, bool)> {
    let messages = runtime.session().messages.iter();
    messages
        .map(|message| {
            (
                message.role.clone(),
                message.content.as_str(),
                message.aborted,
            )
        })
        .collect()
}

#[tokio::test]
async fn an_aborted_reply_is_kept_and_left_out_of_later_requests() {
    let (mut runtime, store, base) = session("aborted", &[]);
    let provider = Steps::new([
        Step {
            events: vec![
                StreamEvent::Thinking("plot".into()),
                StreamEvent::Text("Once".into()),
            ],
            error: None,
            hangs: true,
        },
        says("Shorter."),
    ]);
    let run = RunControl::default();
    let cancelled = {
        let cancellation = run.cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancellation.cancel();
        })
    };
    let (result, _) = turn(&mut runtime, &provider, "tell a long story", &run).await;
    cancelled.await.unwrap();
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert_eq!(
        saved_marked(&runtime),
        [
            (MessageRole::User, "tell a long story", false),
            (MessageRole::Assistant, "Once", true),
        ]
    );
    assert_eq!(runtime.session().messages[1].thinking, "plot");
    assert_journal_matches(&store, &runtime);

    let (result, _) = turn(
        &mut runtime,
        &provider,
        "shorter please",
        &RunControl::default(),
    )
    .await;
    assert_eq!(result.unwrap(), "Shorter.");
    assert_eq!(
        provider.request(1),
        [
            (ChatRole::User, "tell a long story".into()),
            (ChatRole::User, "shorter please".into())
        ]
    );
    assert_eq!(saved_marked(&runtime).len(), 4);
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn a_failure_that_ends_the_turn_keeps_what_the_reply_streamed_as_aborted() {
    let (mut runtime, store, base) = session("failed", &[]);
    let error = r#"{"error":{"message":"Unsupported parameter."}}"#;
    let provider = Steps::new([broken(vec![StreamEvent::Text("Hel".into())], error)]);
    let (result, _) = turn(&mut runtime, &provider, "hi", &RunControl::default()).await;
    assert!(matches!(result, Err(AgentError::Provider(_))));
    assert_eq!(
        saved_marked(&runtime),
        [
            (MessageRole::User, "hi", false),
            (MessageRole::Assistant, "Hel", true),
        ]
    );
    assert!(runtime.session().has_unfinished_turn());
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn a_stop_during_a_retry_backoff_leaves_no_partial_to_recover() {
    let (mut runtime, store, base) = session("backoff-stop", &[]);
    runtime
        .reload(ReloadableSystems {
            retry: RuntimeRetrySettings {
                base_delay_ms: 60_000,
                ..RuntimeRetrySettings::default()
            },
            ..systems()
        })
        .unwrap();
    let provider = Steps::new([broken(vec![StreamEvent::Text("Hel".into())], OVERLOADED)]);
    let run = RunControl::default();
    let turn = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "hi".into(),
        Vec::new(),
        &run,
        |_| {},
    );
    // The process stops during the backoff.
    assert!(tokio::time::timeout(Duration::from_millis(500), turn)
        .await
        .is_err());
    let path = store.path().to_path_buf();
    let journal = fs::read_to_string(&path).unwrap();
    assert!(
        journal.contains(r#""type":"partial","attempt":1,"text":"Hel""#),
        "{journal}"
    );
    drop((runtime, store));
    // The retry dropped the reply: nothing of it is recovered.
    let (_, state) = SessionStore::open(path).unwrap();
    assert_eq!(state.messages.len(), 1);
    assert!(state.has_unfinished_turn());
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn an_abort_during_a_round_answers_the_running_call_and_the_ones_after_it() {
    let (mut runtime, store, base) = session("aborted-round", &[]);
    let marker = base.join("ran.txt");
    let call = |id: &str, command: String| StreamEvent::ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments: json!({ "command": command }).to_string(),
    };
    let provider = Steps::new([reply(vec![
        call("b1", "echo so far; exec sleep 30".into()),
        call("b2", format!("touch '{}'", marker.display())),
    ])]);
    let run = RunControl::default();
    let cancelled = {
        let cancellation = run.cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancellation.cancel();
        })
    };
    let (result, lines) = turn(&mut runtime, &provider, "run both", &run).await;
    cancelled.await.unwrap();
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert!(!marker.exists(), "the second call ran");
    // Only the running call is reported.
    assert_eq!(
        lines[lines.len() - 2..],
        [
            r#"output b1 "so far\n\n\nOperation aborted" error"#,
            "finished bash error"
        ]
    );
    let results = saved(&runtime)[2..].to_vec();
    assert_eq!(
        results,
        [
            (MessageRole::Tool, "so far\n\n\nOperation aborted"),
            (MessageRole::Tool, "Operation aborted"),
        ]
    );
    assert!(runtime.session().pending_tool_calls().is_none());
    assert_journal_matches(&store, &runtime);
    fs::remove_dir_all(base).unwrap();
}

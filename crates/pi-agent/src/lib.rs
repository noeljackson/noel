//! A provider-neutral streaming agent and typed tool loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::{
    ChatMessage, ChatRole, ChatToolCall, Provider, ProviderError, ProviderRequest, StreamEvent,
    ToolDefinition,
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Notify;
use tokio::time::sleep;

/// Default cap for one backoff delay, as upstream's `DEFAULT_MAX_AGENT_RETRY_DELAY_MS`.
pub const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u64,
    pub base_delay_ms: u64,
    /// Caps each computed backoff delay (settings `retry.maxAgentDelayMs`).
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2_000,
            max_delay_ms: DEFAULT_MAX_RETRY_DELAY_MS,
        }
    }
}

/// Backoff before retry `attempt` (0-based): `base_delay_ms * 2^attempt`,
/// capped at `max_delay_ms`, matching upstream `retryDelayMs`.
pub fn retry_delay_ms(retry: &RetryPolicy, attempt: u64) -> u64 {
    let shift = attempt.min(16) as u32;
    retry
        .base_delay_ms
        .saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX))
        .min(retry.max_delay_ms)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FinalOutputPolicy {
    #[default]
    Text,
    RequiredTool {
        name: String,
    },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub retry: RetryPolicy,
    pub max_tool_rounds: usize,
    pub final_output: FinalOutputPolicy,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            tools: Vec::new(),
            retry: RetryPolicy::default(),
            max_tool_rounds: 50,
            final_output: FinalOutputPolicy::Text,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentState {
    pub messages: Vec<ChatMessage>,
}

/// Cooperative cancellation shared by every clone. Cancelling is permanent:
/// the flag stays set and every current or later `cancelled()` wait finishes.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<CancellationState>);

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Completes once the token is cancelled, immediately if it already is.
    pub async fn cancelled(&self) {
        // A `Notified` receives `notify_waiters` from the moment it is
        // created, so a cancel between this check and the await is not lost.
        let notified = self.0.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Reported by [`complete_with_retry_observed`] before each backoff, with the
/// fields of upstream's `auto_retry_start` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryNotice {
    /// The retry about to run, starting at 1.
    pub attempt: u64,
    /// The configured retry limit (`RetryPolicy::max_retries`).
    pub max_attempts: u64,
    /// The backoff before the retry, already capped by the policy.
    pub delay_ms: u64,
    /// The provider error that caused the retry.
    pub error_message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted,
    Provider(StreamEvent),
    ToolStarted(ChatToolCall),
    ToolFinished {
        call: ChatToolCall,
        output: String,
        is_error: bool,
    },
    TurnCompleted,
    TurnCancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutput {
    Text(String),
    Tool { name: String, arguments: Value },
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ChatToolCall) -> Result<String, String>;
}

pub struct NoTools;

#[async_trait]
impl ToolExecutor for NoTools {
    async fn execute(&self, call: &ChatToolCall) -> Result<String, String> {
        Err(format!("tool {} is not available", call.name))
    }
}

/// Optional hook for token-aware applications. It runs before every provider
/// request and may summarize or trim the normalized local history.
pub trait CompactionHook: Send + Sync {
    fn compact_if_needed(&self, messages: &mut Vec<ChatMessage>) -> Result<(), String>;
}

#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    provider: &dyn Provider,
    state: &mut AgentState,
    prompt: String,
    config: &AgentConfig,
    executor: &dyn ToolExecutor,
    cancellation: &CancellationToken,
    compaction: Option<&dyn CompactionHook>,
    mut on_event: impl FnMut(&AgentEvent) + Send,
) -> Result<AgentOutput, AgentError> {
    on_event(&AgentEvent::TurnStarted);
    state.messages.push(ChatMessage {
        role: ChatRole::User,
        content: prompt,
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
    });

    let mut final_text = String::new();
    for _ in 0..config.max_tool_rounds {
        if cancellation.is_cancelled() {
            on_event(&AgentEvent::TurnCancelled);
            return Err(AgentError::Cancelled);
        }
        if let Some(compaction) = compaction {
            compaction
                .compact_if_needed(&mut state.messages)
                .map_err(AgentError::Compaction)?;
        }
        let events = complete_with_retry(
            provider,
            ProviderRequest {
                max_output_tokens: None,
                system_prompt: config.system_prompt.clone(),
                messages: state.messages.clone(),
                tools: config.tools.clone(),
            },
            &config.retry,
            cancellation,
            |event| on_event(&AgentEvent::Provider(event.clone())),
        )
        .await?;
        if cancellation.is_cancelled() {
            on_event(&AgentEvent::TurnCancelled);
            return Err(AgentError::Cancelled);
        }

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        // A reply cut off by the output limit may carry truncated arguments
        // that still parse; none of its tool calls run.
        let mut truncated = false;
        for event in events {
            match event {
                StreamEvent::Text(delta) => text.push_str(&delta),
                StreamEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_calls.push(ChatToolCall {
                    id,
                    name,
                    arguments,
                }),
                StreamEvent::Stop { reason } => truncated |= pi_ai::is_output_limit_stop(&reason),
                _ => {}
            }
        }
        final_text.push_str(&text);
        state.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: text,
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: tool_calls.clone(),
        });

        if let FinalOutputPolicy::RequiredTool { name } = &config.final_output {
            if let Some(call) = tool_calls
                .iter()
                .find(|call| &call.name == name)
                .filter(|_| !truncated)
            {
                let arguments = serde_json::from_str(&call.arguments).map_err(|source| {
                    AgentError::InvalidToolArguments {
                        tool: call.name.clone(),
                        source,
                    }
                })?;
                on_event(&AgentEvent::TurnCompleted);
                return Ok(AgentOutput::Tool {
                    name: call.name.clone(),
                    arguments,
                });
            }
        } else if tool_calls.is_empty() {
            on_event(&AgentEvent::TurnCompleted);
            return Ok(AgentOutput::Text(final_text));
        }

        if tool_calls.is_empty() {
            return Err(match &config.final_output {
                FinalOutputPolicy::RequiredTool { name } => {
                    AgentError::RequiredFinalToolMissing(name.clone())
                }
                FinalOutputPolicy::Text => AgentError::ToolLoopExhausted,
            });
        }

        for call in tool_calls {
            if cancellation.is_cancelled() {
                on_event(&AgentEvent::TurnCancelled);
                return Err(AgentError::Cancelled);
            }
            on_event(&AgentEvent::ToolStarted(call.clone()));
            let result = if truncated {
                Err(pi_ai::truncated_tool_call_error(&call.name))
            } else {
                executor.execute(&call).await
            };
            let (output, is_error) = match result {
                Ok(output) => (output, false),
                Err(error) => (error, true),
            };
            on_event(&AgentEvent::ToolFinished {
                call: call.clone(),
                output: output.clone(),
                is_error,
            });
            state.messages.push(ChatMessage {
                role: ChatRole::Tool,
                content: output,
                media: Vec::new(),
                tool_call_id: Some(call.id),
                tool_name: Some(call.name),
                tool_calls: Vec::new(),
            });
        }
    }
    Err(AgentError::ToolLoopExhausted)
}

/// Requests a reply, retrying failures a retry can fix
/// ([`ProviderError::is_retryable`]) after a backoff. A failed attempt's
/// events were already reported to `on_event`; the returned events are those
/// of the attempt that succeeded, so a reply that broke off is never mixed
/// into the one that replaces it.
pub async fn complete_with_retry(
    provider: &dyn Provider,
    request: ProviderRequest,
    retry: &RetryPolicy,
    cancellation: &CancellationToken,
    on_event: impl FnMut(&StreamEvent) + Send,
) -> Result<Vec<StreamEvent>, AgentError> {
    complete_with_retry_observed(provider, request, retry, cancellation, on_event, |_| {}).await
}

/// [`complete_with_retry`] that also reports each retry before its backoff.
/// Cancellation interrupts an in-flight request or a backoff sleep at once.
pub async fn complete_with_retry_observed(
    provider: &dyn Provider,
    request: ProviderRequest,
    retry: &RetryPolicy,
    cancellation: &CancellationToken,
    mut on_event: impl FnMut(&StreamEvent) + Send,
    mut on_retry: impl FnMut(RetryNotice) + Send,
) -> Result<Vec<StreamEvent>, AgentError> {
    let mut attempt = 0u64;
    loop {
        let error =
            match complete_once(provider, request.clone(), cancellation, &mut on_event).await {
                Err(AgentError::Provider(error)) => error,
                other => return other,
            };
        if attempt >= retry.max_retries || !error.is_retryable() {
            return Err(error.into());
        }
        let delay = retry_delay_ms(retry, attempt);
        attempt += 1;
        on_retry(RetryNotice {
            attempt,
            max_attempts: retry.max_retries,
            delay_ms: delay,
            error_message: error.to_string(),
        });
        if delay > 0 {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(AgentError::Cancelled),
                () = sleep(Duration::from_millis(delay)) => {}
            }
        }
    }
}

/// Requests a reply once, reporting its events as they arrive. Cancellation
/// drops the request. A failure leaves the events of the broken reply
/// reported but not returned.
pub async fn complete_once(
    provider: &dyn Provider,
    request: ProviderRequest,
    cancellation: &CancellationToken,
    mut on_event: impl FnMut(&StreamEvent) + Send,
) -> Result<Vec<StreamEvent>, AgentError> {
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled);
    }
    let attempt = async {
        let mut events = Vec::new();
        let result = provider
            .complete_streaming(request, &mut |event| {
                on_event(&event);
                events.push(event);
                Ok(())
            })
            .await;
        result.map(|()| events)
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(AgentError::Cancelled),
        outcome = attempt => Ok(outcome?),
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("turn cancelled")]
    Cancelled,
    #[error("tool loop exceeded its configured round limit")]
    ToolLoopExhausted,
    #[error("model did not call required final tool {0}")]
    RequiredFinalToolMissing(String),
    #[error("invalid arguments for final tool {tool}: {source}")]
    InvalidToolArguments {
        tool: String,
        source: serde_json::Error,
    },
    #[error("compaction failed: {0}")]
    Compaction(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_doubles_and_is_capped() {
        let retry = RetryPolicy {
            max_retries: 10,
            base_delay_ms: 2_000,
            max_delay_ms: 10_000,
        };
        let delays: Vec<u64> = (0..5)
            .map(|attempt| retry_delay_ms(&retry, attempt))
            .collect();
        assert_eq!(delays, vec![2_000, 4_000, 8_000, 10_000, 10_000]);
        assert_eq!(
            RetryPolicy::default().max_delay_ms,
            DEFAULT_MAX_RETRY_DELAY_MS
        );
    }

    struct FixedProvider(Vec<StreamEvent>);

    #[async_trait]
    impl Provider for FixedProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(self.0.clone())
        }
    }

    struct CancellingProvider(CancellationToken);

    #[async_trait]
    impl Provider for CancellingProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            self.0.cancel();
            Ok(vec![StreamEvent::Text("too late".into())])
        }
    }

    #[tokio::test]
    async fn returns_text_and_keeps_normalized_history() {
        let provider = FixedProvider(vec![StreamEvent::Text("hello".to_string())]);
        let mut state = AgentState::default();
        let output = run_turn(
            &provider,
            &mut state,
            "hi".to_string(),
            &AgentConfig::default(),
            &NoTools,
            &CancellationToken::default(),
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(output, AgentOutput::Text("hello".to_string()));
        assert_eq!(state.messages.len(), 2);
    }

    #[tokio::test]
    async fn structured_final_tool_is_not_executed() {
        let provider = FixedProvider(vec![StreamEvent::ToolCall {
            id: "review".to_string(),
            name: "submit_review".to_string(),
            arguments: r#"{"decision":"deny"}"#.to_string(),
        }]);
        let config = AgentConfig {
            final_output: FinalOutputPolicy::RequiredTool {
                name: "submit_review".to_string(),
            },
            ..Default::default()
        };
        let output = run_turn(
            &provider,
            &mut AgentState::default(),
            "review".to_string(),
            &config,
            &NoTools,
            &CancellationToken::default(),
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            output,
            AgentOutput::Tool {
                name: "submit_review".to_string(),
                arguments: serde_json::json!({"decision": "deny"}),
            }
        );
    }

    fn empty_request() -> ProviderRequest {
        ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Cancels `cancellation` after `after` and returns when that happened.
    fn cancel_later(
        cancellation: &CancellationToken,
        after: Duration,
    ) -> tokio::task::JoinHandle<std::time::Instant> {
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            sleep(after).await;
            cancellation.cancel();
            std::time::Instant::now()
        })
    }

    struct PendingProvider;

    #[async_trait]
    impl Provider for PendingProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            std::future::pending().await
        }
    }

    /// Fails every request with an error a retry can fix.
    struct FailingProvider(std::sync::atomic::AtomicU64);

    #[async_trait]
    impl Provider for FailingProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let call = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            Err(ProviderError::InvalidResponse(format!("overloaded {call}")))
        }
    }

    /// Answers each request with the next scripted reply: the events it
    /// streams, then the error that breaks it off, if any.
    struct ScriptedProvider(std::sync::Mutex<Vec<(Vec<StreamEvent>, Option<ProviderError>)>>);

    impl ScriptedProvider {
        fn new(mut replies: Vec<(Vec<StreamEvent>, Option<ProviderError>)>) -> Self {
            replies.reverse();
            Self(std::sync::Mutex::new(replies))
        }

        fn remaining(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            unreachable!("the retry loop streams")
        }

        async fn complete_streaming(
            &self,
            _request: ProviderRequest,
            on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
        ) -> Result<(), ProviderError> {
            let (events, error) = self.0.lock().unwrap().pop().expect("a scripted reply");
            for event in events {
                on_event(event)?;
            }
            error.map_or(Ok(()), Err)
        }
    }

    fn invalid(response: &str) -> ProviderError {
        ProviderError::InvalidResponse(response.to_string())
    }

    fn text(text: &str) -> StreamEvent {
        StreamEvent::Text(text.to_string())
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_once_cancelled() {
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(100), cancellation.cancelled())
            .await
            .expect("an already-cancelled token must not wait");
        assert!(cancellation.clone().is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_request_that_never_completes() {
        let cancellation = CancellationToken::default();
        let cancelled_at = cancel_later(&cancellation, Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            complete_with_retry(
                &PendingProvider,
                empty_request(),
                &RetryPolicy::default(),
                &cancellation,
                |_| {},
            ),
        )
        .await
        .expect("cancellation must end the in-flight request");
        let latency = cancelled_at.await.unwrap().elapsed();
        assert!(matches!(result, Err(AgentError::Cancelled)));
        assert!(
            latency < Duration::from_millis(100),
            "cancellation took {latency:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_during_backoff_returns_promptly() {
        let provider = FailingProvider(Default::default());
        let cancellation = CancellationToken::default();
        // A one-minute backoff would hold the turn without cancellation.
        let retry = RetryPolicy {
            max_retries: 3,
            base_delay_ms: 60_000,
            max_delay_ms: 60_000,
        };
        let mut notices = Vec::new();
        let cancelled_at = cancel_later(&cancellation, Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            complete_with_retry_observed(
                &provider,
                empty_request(),
                &retry,
                &cancellation,
                |_| {},
                |notice| notices.push(notice),
            ),
        )
        .await
        .expect("cancellation must end the backoff");
        let latency = cancelled_at.await.unwrap().elapsed();
        assert!(matches!(result, Err(AgentError::Cancelled)));
        assert!(
            latency < Duration::from_millis(100),
            "cancellation took {latency:?}"
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].delay_ms, 60_000);
    }

    #[tokio::test]
    async fn retry_notices_report_attempts_and_capped_delays() {
        let provider = FailingProvider(Default::default());
        let retry = RetryPolicy {
            max_retries: 3,
            base_delay_ms: 10,
            max_delay_ms: 25,
        };
        let mut notices = Vec::new();
        let error = complete_with_retry_observed(
            &provider,
            empty_request(),
            &retry,
            &CancellationToken::default(),
            |_| {},
            |notice| notices.push(notice),
        )
        .await
        .unwrap_err();
        // The final attempt's error is returned, not followed by a notice.
        assert_eq!(
            error.to_string(),
            "provider returned an invalid response: overloaded 4"
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 4);
        let notice = |attempt, delay_ms, call: u64| RetryNotice {
            attempt,
            max_attempts: 3,
            delay_ms,
            error_message: format!("provider returned an invalid response: overloaded {call}"),
        };
        assert_eq!(
            notices,
            vec![notice(1, 10, 1), notice(2, 20, 2), notice(3, 25, 3)]
        );
    }

    #[tokio::test]
    async fn errors_no_retry_can_fix_fail_at_once() {
        for error in [
            invalid(r#"{"error":{"message":"Incorrect API key provided."}}"#),
            // A quota is not a rate limit: waiting does not lift it.
            invalid(r#"{"error":{"message":"Rate limit","code":"insufficient_quota"}}"#),
            ProviderError::MissingApiKey {
                provider: "anthropic".into(),
            },
            ProviderError::Config("no credentials".into()),
            // Compaction answers a context overflow, not a retry.
            invalid(r#"{"error":{"code":"context_length_exceeded"}}"#),
            invalid("overloaded: prompt is too long: 9 tokens > 8 maximum"),
        ] {
            let expected = error.to_string();
            let provider = ScriptedProvider::new(vec![
                (Vec::new(), Some(error)),
                (vec![text("never asked")], None),
            ]);
            let mut notices = Vec::new();
            let error = complete_with_retry_observed(
                &provider,
                empty_request(),
                &RetryPolicy {
                    base_delay_ms: 0,
                    ..RetryPolicy::default()
                },
                &CancellationToken::default(),
                |_| {},
                |notice| notices.push(notice),
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert_eq!(notices, Vec::new(), "{expected}");
            assert_eq!(provider.remaining(), 1, "{expected} was retried");
        }
    }

    #[tokio::test]
    async fn a_reply_that_breaks_off_is_retried_and_left_out_of_the_result() {
        let provider = ScriptedProvider::new(vec![
            (vec![text("Hel"), text("lo")], Some(invalid("terminated"))),
            (vec![text("Hello again.")], None),
        ]);
        let mut streamed = Vec::new();
        let mut notices = Vec::new();
        let events = complete_with_retry_observed(
            &provider,
            empty_request(),
            &RetryPolicy {
                base_delay_ms: 0,
                ..RetryPolicy::default()
            },
            &CancellationToken::default(),
            |event| streamed.push(event.clone()),
            |notice| notices.push(notice),
        )
        .await
        .unwrap();
        // The broken reply was streamed, but only its replacement is returned.
        assert_eq!(streamed, [text("Hel"), text("lo"), text("Hello again.")]);
        assert_eq!(events, [text("Hello again.")]);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].attempt, 1);
        assert_eq!(
            notices[0].error_message,
            "provider returned an invalid response: terminated"
        );
    }

    #[tokio::test]
    async fn cancellation_during_a_provider_request_wins() {
        let cancellation = CancellationToken::default();
        let provider = CancellingProvider(cancellation.clone());
        let error = run_turn(
            &provider,
            &mut AgentState::default(),
            "hi".into(),
            &AgentConfig::default(),
            &NoTools,
            &cancellation,
            None,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, AgentError::Cancelled));
    }

    #[tokio::test]
    async fn tool_calls_from_a_reply_cut_off_at_the_output_limit_are_not_run() {
        struct TruncatedThenDone(std::sync::Mutex<usize>);

        #[async_trait]
        impl Provider for TruncatedThenDone {
            async fn complete(
                &self,
                _request: ProviderRequest,
            ) -> Result<Vec<StreamEvent>, ProviderError> {
                let mut calls = self.0.lock().unwrap();
                *calls += 1;
                if *calls > 1 {
                    return Ok(vec![StreamEvent::Text("done".to_string())]);
                }
                Ok(vec![
                    StreamEvent::ToolCall {
                        id: "call_1".to_string(),
                        name: "bash".to_string(),
                        arguments: r#"{"command":"rm -rf build"}"#.to_string(),
                    },
                    StreamEvent::Stop {
                        reason: "length".to_string(),
                    },
                ])
            }
        }

        struct RecordingExecutor(std::sync::Mutex<Vec<String>>);

        #[async_trait]
        impl ToolExecutor for RecordingExecutor {
            async fn execute(&self, call: &ChatToolCall) -> Result<String, String> {
                self.0.lock().unwrap().push(call.name.clone());
                Ok("ran".to_string())
            }
        }

        let executor = RecordingExecutor(std::sync::Mutex::new(Vec::new()));
        let mut state = AgentState::default();
        let output = run_turn(
            &TruncatedThenDone(std::sync::Mutex::new(0)),
            &mut state,
            "clean up".into(),
            &AgentConfig::default(),
            &executor,
            &CancellationToken::default(),
            None,
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(output, AgentOutput::Text("done".to_string()));
        assert!(executor.0.lock().unwrap().is_empty());
        let result = &state.messages[2];
        assert_eq!(result.role, ChatRole::Tool);
        assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
        assert!(
            result.content.contains("output token limit"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_request_that_stalls_before_any_output_is_retried() {
        use std::io::{Read, Write};
        // The first connection never answers; the second one does.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let url = format!("http://{}", listener.local_addr().expect("stub addr"));
        std::thread::spawn(move || {
            let mut stalled = Vec::new();
            for answer in [false, true] {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                // Read the whole request so closing the socket cannot reset it.
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let line = line.to_ascii_lowercase();
                                line.strip_prefix("content-length:")?.trim().parse().ok()
                            })
                            .unwrap_or(0usize);
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                if answer {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                          data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    );
                } else {
                    stalled.push(stream);
                }
            }
            std::thread::sleep(Duration::from_secs(30));
        });
        let provider = pi_ai::create_provider_with_client(
            pi_ai::ProviderConfig {
                model: pi_ai::ModelRef {
                    provider: "local".to_string(),
                    id: "test".to_string(),
                },
                api: pi_ai::ProviderApi::OpenAi,
                base_url: Some(url),
                auth: pi_ai::ProviderAuth::ApiKey("test-key".to_string()),
                thinking_level: None,
                thinking_budget_tokens: None,
                session_id: None,
            },
            pi_ai::provider_http_client_with_idle_timeout(Duration::from_millis(300)),
        );
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hi".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };

        let events = tokio::time::timeout(
            Duration::from_secs(20),
            complete_with_retry(
                provider.as_ref(),
                request,
                &RetryPolicy {
                    max_retries: 1,
                    base_delay_ms: 0,
                    ..RetryPolicy::default()
                },
                &CancellationToken::default(),
                |_| {},
            ),
        )
        .await
        .expect("the idle timeout must end the stalled attempt")
        .expect("the retry succeeds");

        assert!(events.contains(&StreamEvent::Text("ok".to_string())));
    }
}

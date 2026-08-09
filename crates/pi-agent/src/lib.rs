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
use tokio::time::sleep;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u64,
    pub base_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalOutputPolicy {
    Text,
    RequiredTool { name: String },
}

impl Default for FinalOutputPolicy {
    fn default() -> Self {
        Self::Text
    }
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

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
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
                system_prompt: config.system_prompt.clone(),
                messages: state.messages.clone(),
                tools: config.tools.clone(),
            },
            &config.retry,
            cancellation,
            |event| on_event(&AgentEvent::Provider(event.clone())),
        )
        .await?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
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
            if let Some(call) = tool_calls.iter().find(|call| &call.name == name) {
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
            let result = executor.execute(&call).await;
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

/// Shared retry behavior. A request is retried only before any output or tool
/// call has been observed, preventing duplicate side effects and text.
pub async fn complete_with_retry(
    provider: &dyn Provider,
    request: ProviderRequest,
    retry: &RetryPolicy,
    cancellation: &CancellationToken,
    mut on_event: impl FnMut(&StreamEvent) + Send,
) -> Result<Vec<StreamEvent>, AgentError> {
    let max_attempts = retry.max_retries.saturating_add(1);
    let mut attempt = 0u64;
    loop {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let mut events = Vec::new();
        let result = provider
            .complete_streaming(request.clone(), &mut |event| {
                on_event(&event);
                events.push(event);
                Ok(())
            })
            .await;
        match result {
            Ok(()) => return Ok(events),
            Err(error) if !events.is_empty() => return Err(error.into()),
            Err(error) if is_context_overflow(&error) => return Err(error.into()),
            Err(error) if attempt + 1 >= max_attempts => return Err(error.into()),
            Err(_) => {
                let shift = attempt.min(16) as u32;
                let delay = retry
                    .base_delay_ms
                    .saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX));
                if delay > 0 {
                    sleep(Duration::from_millis(delay)).await;
                }
                attempt += 1;
            }
        }
    }
}

fn is_context_overflow(error: &ProviderError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("context_length")
        || message.contains("context window")
        || message.contains("too many tokens")
        || message.contains("maximum context")
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
        let mut config = AgentConfig::default();
        config.final_output = FinalOutputPolicy::RequiredTool {
            name: "submit_review".to_string(),
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
}

use std::{
    collections::BTreeMap,
    env, fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, PoisonError},
    time::Duration,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use regex::{Regex, RegexBuilder};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

mod anthropic;
mod bedrock;
pub mod catalog;
mod faux;
mod google;
mod http;
mod images;
mod mistral;
mod openai_chat;
mod openai_responses;
mod thinking;
pub mod validation;

pub use anthropic::*;
pub(crate) use bedrock::*;
pub(crate) use faux::*;
pub(crate) use google::*;
pub use http::*;
pub use images::*;
pub(crate) use mistral::*;
pub use openai_chat::*;
pub(crate) use openai_responses::*;
pub(crate) use thinking::*;

pub use catalog::{ModelTraits, ThinkingLevelMap};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Text(String),
    Thinking(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Stop {
        reason: String,
    },
}

/// True when a stop reason means the reply was cut off by the output-token
/// or context limit, so tool-call arguments in it may be truncated:
/// Anthropic and Bedrock `max_tokens`, OpenAI-compatible `length`, Gemini
/// `MAX_TOKENS`, and Anthropic `model_context_window_exceeded`.
pub fn is_output_limit_stop(reason: &str) -> bool {
    matches!(
        reason.to_ascii_lowercase().as_str(),
        "max_tokens"
            | "length"
            | "model_length"
            | "max_output_tokens"
            | "incomplete.max_output_tokens"
            | "model_context_window_exceeded"
    )
}

/// Tool result for a call that came in a reply cut off by the output limit
/// (upstream `failToolCallsFromTruncatedMessage`). The call is not run, so
/// the model can re-issue it with complete arguments.
pub fn truncated_tool_call_error(tool_name: &str) -> String {
    format!(
        "Tool call \"{tool_name}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments."
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderApi {
    #[default]
    Faux,
    OpenAi,
    OpenAiResponses,
    OpenAiCodexResponses,
    AzureOpenAiResponses,
    Anthropic,
    Google,
    GoogleVertex,
    Bedrock,
    Mistral,
}

impl ProviderApi {
    /// Upstream's name for the API, as model catalogs spell it.
    pub fn upstream_name(&self) -> &'static str {
        match self {
            Self::Faux => "faux",
            Self::OpenAi => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiCodexResponses => "openai-codex-responses",
            Self::AzureOpenAiResponses => "azure-openai-responses",
            Self::Anthropic => "anthropic-messages",
            Self::Google => "google-generative-ai",
            Self::GoogleVertex => "google-vertex",
            Self::Bedrock => "bedrock-converse-stream",
            Self::Mistral => "mistral-conversations",
        }
    }

    /// The API with that upstream name; `None` for one noel has no adapter
    /// for.
    pub fn from_upstream_name(name: &str) -> Option<Self> {
        [
            Self::Faux,
            Self::OpenAi,
            Self::OpenAiResponses,
            Self::OpenAiCodexResponses,
            Self::AzureOpenAiResponses,
            Self::Anthropic,
            Self::Google,
            Self::GoogleVertex,
            Self::Bedrock,
            Self::Mistral,
        ]
        .into_iter()
        .find(|api| api.upstream_name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageProviderApi {
    OpenRouterImages,
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub model: ModelRef,
    pub api: ProviderApi,
    pub base_url: Option<String>,
    pub auth: ProviderAuth,
    /// The thinking level requests ask for, already clamped to the model:
    /// `off` turns thinking off where the model allows that, and `None`
    /// sends no thinking parameter (a model without reasoning).
    pub thinking_level: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
    pub session_id: Option<String>,
    /// The model's output cap, context window, and names for the thinking
    /// levels; empty for a model nothing is known about.
    pub traits: ModelTraits,
}

#[derive(Debug, Clone)]
pub struct ImageProviderConfig {
    pub model: ModelRef,
    pub api: ImageProviderApi,
    pub base_url: Option<String>,
    pub auth: ProviderAuth,
    pub output_modalities: Vec<String>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub enum ProviderAuth {
    #[default]
    None,
    ApiKey(String),
    ClaudeCodeOAuth {
        access_token: String,
    },
    ChatGptOAuth {
        access_token: String,
        account_id: Option<String>,
    },
}

impl std::fmt::Debug for ProviderAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::ApiKey(_) => formatter.write_str("ApiKey([REDACTED])"),
            Self::ClaudeCodeOAuth { .. } => {
                formatter.write_str("ClaudeCodeOAuth { access_token: [REDACTED] }")
            }
            Self::ChatGptOAuth { account_id, .. } => formatter
                .debug_struct("ChatGptOAuth")
                .field("access_token", &"[REDACTED]")
                .field("account_id", account_id)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(default)]
    pub media: Vec<MediaInput>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ChatToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInput {
    pub mime_type: String,
    pub data_base64: String,
    pub path: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ImageGenerationInput {
    Text { text: String },
    Image { media: MediaInput },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageGenerationOutput {
    Text(String),
    Image {
        mime_type: String,
        data_base64: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenerationResult {
    pub response_id: Option<String>,
    pub output: Vec<ImageGenerationOutput>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    /// Optional request-local output budget (includes provider reasoning).
    pub max_output_tokens: Option<u64>,
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider {provider} requires an API key")]
    MissingApiKey { provider: String },
    #[error(
        "request failed: {detail}{timed_out}",
        detail = with_cause_detail(.0),
        timed_out = if .0.is_timeout() { " (timed out)" } else { "" }
    )]
    Request(#[from] reqwest::Error),
    #[error("request failed with status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
        /// How long the answer asks the client to wait before asking again,
        /// from its `retry-after-ms` or `retry-after` header.
        retry_after: Option<Duration>,
    },
    #[error("invalid header value: {0}")]
    Header(#[from] reqwest::header::InvalidHeaderValue),
    #[error("provider config error: {0}")]
    Config(String),
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(String),
}

/// An error's message with the deepest cause in its chain appended, so a
/// connection that failed says why (refused, DNS, TLS) rather than only that
/// the request could not be sent (upstream 4c992154 `withCauseDetail`).
pub fn with_cause_detail(error: &(dyn std::error::Error + 'static)) -> String {
    let message = error.to_string();
    let mut deepest = None;
    let mut source = error.source();
    for _ in 0..5 {
        let Some(cause) = source else { break };
        deepest = Some(cause);
        source = cause.source();
    }
    match deepest.map(|cause| cause.to_string()) {
        Some(cause) if !cause.is_empty() && !message.contains(&cause) => {
            format!("{message}: {cause}")
        }
        _ => message,
    }
}

impl ProviderError {
    /// Whether the same request may succeed if it is sent again: a transport
    /// failure, or a provider's answer that upstream reads as transient
    /// ([`is_retryable_error_text`]). A context overflow never is: compaction
    /// answers it, not a retry.
    pub fn is_retryable(&self) -> bool {
        match self.provider_answer() {
            Some(answer) => !is_context_overflow_text(&answer) && is_retryable_error_text(&answer),
            // The connection could not be made, or broke before or during
            // the response. A request noel could not build never succeeds.
            None => {
                matches!(self, Self::Request(error) if !error.is_builder() && !error.is_redirect())
            }
        }
    }

    /// Whether `provider` refused the request because its input exceeds the
    /// model's context window ([`is_context_overflow_text`]). Cerebras
    /// answers an overflow with a 400 or 413 without a body.
    pub fn is_context_overflow(&self, provider: &str) -> bool {
        if let Self::Status { status, body, .. } = self
            && provider == "cerebras"
            && matches!(status.as_u16(), 400 | 413)
            && body.trim().is_empty()
        {
            return true;
        }
        self.provider_answer()
            .is_some_and(|answer| is_context_overflow_text(&answer))
    }

    /// The error as text, if it is what the provider answered: an HTTP
    /// status with its body, or a response noel could not use. Upstream's
    /// patterns read only this. noel's own errors (no credential, a bad
    /// header, its configuration) may name paths and numbers that would
    /// match them by accident.
    fn provider_answer(&self) -> Option<String> {
        match self {
            Self::Status { .. } | Self::InvalidResponse(_) => Some(self.to_string()),
            Self::Request(error) if error.is_status() => Some(self.to_string()),
            Self::Request(_) | Self::MissingApiKey { .. } | Self::Header(_) | Self::Config(_) => {
                None
            }
        }
    }
}

/// Upstream's `RETRYABLE_PROVIDER_ERROR_PATTERN` (`packages/ai/src/utils/retry.ts`):
/// provider load, HTTP statuses, and transport failures, as they appear in
/// error text. `error-classification.json` holds upstream's list.
const RETRYABLE_ERROR_PATTERNS: &[&str] = &[
    "overloaded",
    "server_busy",
    "servers are currently busy",
    "currently experiencing high demand",
    "model is at capacity",
    "rate.?limit",
    "too many requests",
    // Status codes with word boundaries, so "50342" or "gpt-500" in an
    // id is not a status.
    r"(?<![a-z0-9])(?:429|500|502|503|504|520|524)(?![a-z0-9])",
    "service.?(?:is.?)?(?:temporarily.?)?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?(?:an.?)?error",
    "exceeded request buffer limit while retrying upstream",
    "unable to forward upstream",
    "upstream.?request.?failed",
    "upstream_stream_read_error",
    "stream was interrupted",
    "http/?2 stream failed",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    r"request to \S+ failed, reason:",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "stream disconnected before completion",
    "cut short after the reply had begun",
    "pending stream has been canceled",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "ResourceExhausted",
    "subscription_sharing_usage_unavailable",
    "subscription_sharing_user_unavailable",
];

/// Upstream's `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN`: quota, budget and
/// subscription limits that a retry cannot lift, whatever else the text says.
const NON_RETRYABLE_ERROR_PATTERNS: &[&str] = &[
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
    "subscription_sharing_usage_limit_exceeded",
    "subscription_sharing_workplace_individual_usage_limit_exceeded",
];

/// Upstream's `OVERFLOW_PATTERNS` (`packages/ai/src/utils/overflow.ts`): how
/// each provider words an input that exceeds the context window.
const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt (?:is )?too long",
    r"prompt exceeds max length",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"context window exceeded",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
    r"^(?:[\w -]+ API error \(413\):|413(?:\s|:|$))",
];

/// Upstream's `NON_OVERFLOW_PATTERNS`: throttling errors that would otherwise
/// match an overflow pattern ("Too many tokens, please wait").
const NON_OVERFLOW_PATTERNS: &[&str] = &[
    r"^(Throttling error|Service unavailable):",
    r"rate limit",
    r"too many requests",
];

/// One regular expression matching any of `patterns`, ignoring case.
fn any_pattern(patterns: &[&str]) -> Regex {
    let alternatives = patterns
        .iter()
        .map(|pattern| format!("(?:{})", translate_js_lookaround(pattern)))
        .collect::<Vec<_>>();
    RegexBuilder::new(&alternatives.join("|"))
        .case_insensitive(true)
        .build()
        .expect("error patterns are valid regular expressions")
}

/// JavaScript look-around that the Rust regex crate cannot compile, as an
/// equivalent without it. Upstream's error patterns use look-around only
/// for word boundaries around HTTP status codes; a consuming boundary
/// (with the string's edges) matches the same texts for `is_match`, where
/// nothing after the pattern depends on what it consumed.
fn translate_js_lookaround(pattern: &str) -> String {
    pattern
        .replace("(?<![a-z0-9])", "(?:^|[^a-z0-9])")
        .replace("(?![a-z0-9])", "(?:[^a-z0-9]|$)")
}

/// Upstream's `isRetryableAssistantError` for an error with this text: it
/// looks transient, and is not a quota or billing limit.
pub fn is_retryable_error_text(text: &str) -> bool {
    static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| any_pattern(RETRYABLE_ERROR_PATTERNS));
    static NON_RETRYABLE: LazyLock<Regex> =
        LazyLock::new(|| any_pattern(NON_RETRYABLE_ERROR_PATTERNS));
    !text.is_empty() && !NON_RETRYABLE.is_match(text) && RETRYABLE.is_match(text)
}

/// Upstream's `isContextOverflow` for an error with this text.
pub fn is_context_overflow_text(text: &str) -> bool {
    static OVERFLOW: LazyLock<Regex> = LazyLock::new(|| any_pattern(OVERFLOW_PATTERNS));
    static NON_OVERFLOW: LazyLock<Regex> = LazyLock::new(|| any_pattern(NON_OVERFLOW_PATTERNS));
    !NON_OVERFLOW.is_match(text) && OVERFLOW.is_match(text)
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError>;

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        for event in self.complete(request).await? {
            on_event(event)?;
        }
        Ok(())
    }

    /// The thinking level this provider's requests ask for; `None` when
    /// thinking is off or the model has none.
    fn thinking_level(&self) -> Option<String> {
        None
    }

    /// The most output tokens the model writes in one reply, when that is
    /// known ([`model_output_limit`]). A reply cut off below it was cut off
    /// by the context window, not by the model's own limit.
    fn output_limit(&self) -> Option<u64> {
        None
    }
}

/// The model's output limit before any clamp to the context window: its
/// catalog `maxTokens` ([`ModelTraits::max_tokens`]), which upstream compares
/// a cut-off reply with (`isRecoverableLength`). A model without one that is
/// served over the Anthropic API has the cap its requests carry; any other
/// has no known limit.
pub fn model_output_limit(config: &ProviderConfig) -> Option<u64> {
    config.traits.max_tokens.or_else(|| {
        (config.api == ProviderApi::Anthropic).then_some(ANTHROPIC_UNCATALOGED_MAX_TOKENS)
    })
}

pub fn create_provider(config: ProviderConfig) -> Box<dyn Provider> {
    create_provider_with_client(config, provider_http_client())
}

/// Upstream's HTTP idle timeout (`DEFAULT_HTTP_IDLE_TIMEOUT_MS`): the longest
/// a provider request waits for response headers, or for the next read of
/// the body or stream.
pub const PROVIDER_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The client [`create_provider`] uses: a stalled request fails after
/// [`PROVIDER_IDLE_TIMEOUT`] without progress with a timed-out
/// [`ProviderError::Request`], which callers retry like any network error,
/// instead of hanging the turn forever.
pub fn provider_http_client() -> reqwest::Client {
    provider_http_client_with_idle_timeout(PROVIDER_IDLE_TIMEOUT)
}

/// [`provider_http_client`] with another idle bound (tests use a short one).
pub fn provider_http_client_with_idle_timeout(idle_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(idle_timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Builds a provider with a caller-owned client, allowing applications to set
/// proxy, timeout, certificate, and connection-pool policy in one place.
pub fn create_provider_with_client(
    config: ProviderConfig,
    client: reqwest::Client,
) -> Box<dyn Provider> {
    match config.api {
        ProviderApi::Faux => Box::new(FauxProvider { config }),
        ProviderApi::OpenAi => Box::new(OpenAiProvider { config, client }),
        ProviderApi::OpenAiResponses => Box::new(OpenAiResponsesProvider {
            config,
            endpoint: OpenAiResponsesEndpoint::OpenAi,
            client,
        }),
        ProviderApi::OpenAiCodexResponses => Box::new(OpenAiResponsesProvider {
            config,
            endpoint: OpenAiResponsesEndpoint::Codex,
            client,
        }),
        ProviderApi::AzureOpenAiResponses => Box::new(OpenAiResponsesProvider {
            config,
            endpoint: OpenAiResponsesEndpoint::Azure,
            client,
        }),
        ProviderApi::Anthropic => Box::new(AnthropicProvider { config, client }),
        ProviderApi::Google => Box::new(GoogleProvider { config, client }),
        ProviderApi::GoogleVertex => Box::new(GoogleVertexProvider { config, client }),
        ProviderApi::Bedrock => Box::new(BedrockProvider { config, client }),
        ProviderApi::Mistral => Box::new(MistralProvider { config, client }),
    }
}

#[cfg(test)]
mod tests;

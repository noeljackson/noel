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

pub mod catalog;

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
        "max_tokens" | "length" | "max_output_tokens" | "model_context_window_exceeded"
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
        if let Self::Status { status, body, .. } = self {
            if provider == "cerebras"
                && matches!(status.as_u16(), 400 | 413)
                && body.trim().is_empty()
            {
                return true;
            }
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

pub async fn generate_images(
    config: ImageProviderConfig,
    input: Vec<ImageGenerationInput>,
) -> Result<ImageGenerationResult, ProviderError> {
    match config.api {
        ImageProviderApi::OpenRouterImages => generate_openrouter_images(config, input).await,
    }
}

struct FauxProvider {
    config: ProviderConfig,
}

async fn generate_openrouter_images(
    config: ImageProviderConfig,
    input: Vec<ImageGenerationInput>,
) -> Result<ImageGenerationResult, ProviderError> {
    let api_key = match &config.auth {
        ProviderAuth::ApiKey(api_key) => api_key.clone(),
        _ => {
            return Err(ProviderError::MissingApiKey {
                provider: config.model.provider.clone(),
            });
        }
    };
    let base_url = resolve_env_placeholders(
        config
            .base_url
            .as_deref()
            .unwrap_or("https://openrouter.ai/api/v1"),
    )?;
    let response = provider_http_client()
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .headers(bearer_headers(&api_key)?)
        .json(&openrouter_images_body(&config, &input))
        .send()
        .await?;
    parse_openrouter_images_response(
        error_for_status_with_body(response)
            .await?
            .json::<Value>()
            .await?,
    )
}

fn openrouter_images_body(config: &ImageProviderConfig, input: &[ImageGenerationInput]) -> Value {
    let modalities = if config.output_modalities.is_empty() {
        vec!["image".to_string(), "text".to_string()]
    } else {
        config.output_modalities.clone()
    };
    let content = input
        .iter()
        .map(|item| match item {
            ImageGenerationInput::Text { text } => json!({
                "type": "text",
                "text": sanitize_surrogates(text),
            }),
            ImageGenerationInput::Image { media } => json!({
                "type": "image_url",
                "image_url": {
                    "url": media_data_url(media),
                },
            }),
        })
        .collect::<Vec<_>>();
    json!({
        "model": config.model.id,
        "messages": [{
            "role": "user",
            "content": content,
        }],
        "stream": false,
        "modalities": modalities,
    })
}

fn parse_openrouter_images_response(
    response: Value,
) -> Result<ImageGenerationResult, ProviderError> {
    let mut output = Vec::new();
    if let Some(text) = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        output.push(ImageGenerationOutput::Text(text.to_string()));
    }
    if let Some(images) = response
        .pointer("/choices/0/message/images")
        .and_then(Value::as_array)
    {
        for image in images {
            let image_url = image
                .get("image_url")
                .and_then(|value| {
                    value
                        .as_str()
                        .or_else(|| value.get("url").and_then(Value::as_str))
                })
                .unwrap_or_default();
            let Some((mime_type, data_base64)) = parse_data_url(image_url) else {
                continue;
            };
            output.push(ImageGenerationOutput::Image {
                mime_type,
                data_base64,
            });
        }
    }
    if output.is_empty() {
        return Err(ProviderError::InvalidResponse(response.to_string()));
    }
    let input_tokens = response
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = response
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(ImageGenerationResult {
        response_id: response
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        output,
        input_tokens,
        output_tokens,
    })
}

fn parse_data_url(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime_type.to_string(), data.to_string()))
}

fn sanitize_surrogates(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '\u{FFFD}')
        .collect()
}

impl FauxProvider {
    fn is_script(&self) -> bool {
        self.config.model.id == "script"
    }
}

#[async_trait]
impl Provider for FauxProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.is_script() {
            let mut events = Vec::new();
            FauxScript::from_env()?
                .respond(&request, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .await?;
            return Ok(events);
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == ChatRole::User)
            .map(|message| {
                if message.media.is_empty() {
                    message.content.clone()
                } else {
                    format!("{} [media:{}]", message.content, message.media.len())
                }
            })
            .unwrap_or_default();
        Ok(vec![
            StreamEvent::Text(format!(
                "[{}/{}] ",
                self.config.model.provider, self.config.model.id
            )),
            StreamEvent::Text(last_user),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        if self.is_script() {
            return FauxScript::from_env()?.respond(&request, on_event).await;
        }
        let delay_ms = env::var("NOEL_FAUX_STREAM_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        let split_chars = env::var("NOEL_FAUX_STREAM_CHARS").ok().as_deref() == Some("1");
        for event in self.complete(request).await? {
            if split_chars {
                if let StreamEvent::Text(text) = event {
                    for ch in text.chars() {
                        if delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        on_event(StreamEvent::Text(ch.to_string()))?;
                    }
                    continue;
                }
            }
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            on_event(event)?;
        }
        Ok(())
    }
}

/// `faux/script`: replays the responses in the JSON file named by
/// `NOEL_FAUX_SCRIPT`, an array of responses that are each an array of steps.
/// With `NOEL_FAUX_SCRIPT_LOG`, every request is appended to that file as one
/// JSON line and fsynced before the response starts. The number of requests
/// already logged selects the response, so a restarted process resumes the
/// script where the previous one stopped. Without a log, an in-process counter
/// per script file selects it.
#[derive(Debug, Clone)]
struct FauxScript {
    path: PathBuf,
    log: Option<PathBuf>,
}

/// One step of a scripted response, written as a single-key JSON object.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum FauxScriptStep {
    Text(String),
    Thinking(String),
    ToolCall(FauxScriptToolCall),
    Usage {
        input: u64,
        output: u64,
    },
    Stop(String),
    /// Fails the request here, before or after other steps' output.
    Error(FauxScriptError),
    DelayMs(u64),
    /// Holds the response until the file exists; relative to the script.
    WaitForFile(PathBuf),
}

/// How a scripted request fails: a message is the body of an HTTP 503,
/// which the agent retries; `status` and `body` are any other response,
/// with the wait a `retry-after-ms` header would ask for.
#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum FauxScriptError {
    Overloaded(String),
    Status {
        status: u16,
        body: String,
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FauxScriptToolCall {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    arguments: serde_json::Map<String, Value>,
}

/// Next response per script file, for scripts run without a request log.
static FAUX_SCRIPT_POSITIONS: Mutex<BTreeMap<PathBuf, usize>> = Mutex::new(BTreeMap::new());

impl FauxScript {
    fn from_env() -> Result<Self, ProviderError> {
        let path = env::var_os("NOEL_FAUX_SCRIPT")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ProviderError::Config("faux/script needs NOEL_FAUX_SCRIPT to name a script".into())
            })?;
        Ok(Self {
            path: PathBuf::from(path),
            log: env::var_os("NOEL_FAUX_SCRIPT_LOG")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        })
    }

    async fn respond(
        &self,
        request: &ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let responses = self.load()?;
        let position = self.next_position(request)?;
        let steps = responses.get(position).ok_or_else(|| {
            ProviderError::Config(format!(
                "faux script {} is exhausted: request {} has no response (the script has {})",
                self.path.display(),
                position + 1,
                responses.len()
            ))
        })?;
        let mut tool_calls = 0;
        let mut stopped = false;
        for step in steps {
            match step {
                FauxScriptStep::Text(text) => on_event(StreamEvent::Text(text.clone()))?,
                FauxScriptStep::Thinking(text) => on_event(StreamEvent::Thinking(text.clone()))?,
                FauxScriptStep::ToolCall(call) => {
                    tool_calls += 1;
                    on_event(StreamEvent::ToolCall {
                        id: call
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}_{tool_calls}", position + 1)),
                        name: call.name.clone(),
                        arguments: Value::Object(call.arguments.clone()).to_string(),
                    })?;
                }
                FauxScriptStep::Usage { input, output } => on_event(StreamEvent::Usage {
                    input_tokens: *input,
                    output_tokens: *output,
                })?,
                FauxScriptStep::Stop(reason) => {
                    stopped = true;
                    on_event(StreamEvent::Stop {
                        reason: reason.clone(),
                    })?;
                }
                FauxScriptStep::Error(error) => {
                    let (status, body, retry_after) = match error {
                        FauxScriptError::Overloaded(message) => {
                            (reqwest::StatusCode::SERVICE_UNAVAILABLE, message, None)
                        }
                        FauxScriptError::Status {
                            status,
                            body,
                            retry_after_ms,
                        } => {
                            let status = reqwest::StatusCode::from_u16(*status).map_err(|_| {
                                ProviderError::Config(format!(
                                    "faux script {}: {status} is not an HTTP status",
                                    self.path.display()
                                ))
                            })?;
                            (status, body, retry_after_ms.map(Duration::from_millis))
                        }
                    };
                    return Err(ProviderError::Status {
                        status,
                        body: body.clone(),
                        retry_after,
                    });
                }
                FauxScriptStep::DelayMs(delay) => {
                    tokio::time::sleep(Duration::from_millis(*delay)).await;
                }
                FauxScriptStep::WaitForFile(file) => {
                    let file = self.path.parent().unwrap_or(Path::new("")).join(file);
                    while !file.exists() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
        if !stopped {
            let reason = if tool_calls > 0 { "tool_use" } else { "stop" };
            on_event(StreamEvent::Stop {
                reason: reason.to_string(),
            })?;
        }
        Ok(())
    }

    fn load(&self) -> Result<Vec<Vec<FauxScriptStep>>, ProviderError> {
        let text = fs::read_to_string(&self.path).map_err(|error| {
            ProviderError::Config(format!("faux script {}: {error}", self.path.display()))
        })?;
        serde_json::from_str(&text).map_err(|error| {
            ProviderError::Config(format!(
                "invalid faux script {}: {error}",
                self.path.display()
            ))
        })
    }

    /// Index of the response for this request: the number of requests logged
    /// before it (it is appended and fsynced first), or the in-process count.
    fn next_position(&self, request: &ProviderRequest) -> Result<usize, ProviderError> {
        let Some(log) = &self.log else {
            let mut positions = FAUX_SCRIPT_POSITIONS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let next = positions.entry(self.path.clone()).or_default();
            *next += 1;
            return Ok(*next - 1);
        };
        let log_error = |error: std::io::Error| {
            ProviderError::Config(format!("faux script log {}: {error}", log.display()))
        };
        let position = match fs::read(log) {
            Ok(bytes) => bytes.iter().filter(|byte| **byte == b'\n').count(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(log_error(error)),
        };
        let mut line = faux_script_log_entry(request).to_string();
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(log_error)?;
        file.write_all(line.as_bytes()).map_err(log_error)?;
        file.sync_all().map_err(log_error)?;
        Ok(position)
    }
}

/// What one request sent to the model, as a `NOEL_FAUX_SCRIPT_LOG` line.
fn faux_script_log_entry(request: &ProviderRequest) -> Value {
    let messages = request
        .messages
        .iter()
        .map(|message| {
            let mut entry = json!({ "role": message.role, "content": message.content });
            if !message.tool_calls.is_empty() {
                entry["tool_calls"] = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "name": call.name,
                            "arguments": serde_json::from_str::<Value>(&call.arguments)
                                .unwrap_or_else(|_| Value::String(call.arguments.clone())),
                        })
                    })
                    .collect();
            }
            if let Some(id) = &message.tool_call_id {
                entry["tool_call_id"] = json!(id);
            }
            if let Some(name) = &message.tool_name {
                entry["tool_name"] = json!(name);
            }
            if !message.media.is_empty() {
                entry["media"] = json!(message.media.len());
            }
            entry
        })
        .collect::<Vec<_>>();
    json!({
        "system_prompt": request.system_prompt,
        "messages": messages,
        "tools": request.tools.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
    })
}

struct OpenAiProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        if let ProviderAuth::ChatGptOAuth { .. } = &self.config.auth {
            return self.complete_with_chatgpt(request).await;
        }
        let api_key = self.api_key()?;
        let url = format!(
            "{}/chat/completions",
            self.chat_base_url()?.trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .headers(openai_compatible_headers(
                &self.config,
                &api_key,
                request_has_media(&request),
            )?)
            .json(&openai_chat_completions_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        if let Some(mut events) = parse_openai_chat_completions_sse_events(&body) {
            events.push(StreamEvent::Stop {
                reason: "stop".to_string(),
            });
            return Ok(events);
        }
        let response = serde_json::from_str::<Value>(&body)
            .map_err(|_| ProviderError::InvalidResponse(body.clone()))?;
        let mut events = parse_openai_chat_completions_events(&response)
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        events.push(StreamEvent::Stop {
            reason: response
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        });
        Ok(events)
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        if let ProviderAuth::ChatGptOAuth { .. } = &self.config.auth {
            return self
                .complete_with_chatgpt_streaming(request, on_event)
                .await;
        }
        let api_key = self.api_key()?;
        let url = format!(
            "{}/chat/completions",
            self.chat_base_url()?.trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .headers(openai_compatible_headers(
                &self.config,
                &api_key,
                request_has_media(&request),
            )?)
            .json(&openai_chat_completions_body(&self.config, &request))
            .send()
            .await?;
        let response = error_for_status_with_body(response).await?;
        let mut parser = OpenAiChatCompletionsSseParser::default();
        let stream =
            stream_sse_response(response, |data| parser.parse_data(data, on_event)).await?;
        if stream.saw_data {
            let reason = parser
                .stop_reason
                .clone()
                .unwrap_or_else(|| "stop".to_string());
            parser.finish(on_event)?;
            on_event(StreamEvent::Stop { reason })?;
            return Ok(());
        }
        let response = serde_json::from_str::<Value>(&stream.body)
            .map_err(|_| ProviderError::InvalidResponse(stream.body.clone()))?;
        let events = parse_openai_chat_completions_events(&response)
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        emit_events(events, on_event)?;
        on_event(StreamEvent::Stop {
            reason: response
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        })
    }
}

impl OpenAiProvider {
    async fn complete_with_chatgpt(
        &self,
        request: ProviderRequest,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let url = format!(
            "{}/responses",
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://chatgpt.com/backend-api/codex")
                .trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .headers(chatgpt_headers(&self.config.auth)?)
            .json(&openai_responses_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        let mut events = parse_openai_responses_sse_events(&body)
            .or_else(|| {
                serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|response| parse_openai_responses_events(&response))
            })
            .ok_or_else(|| ProviderError::InvalidResponse(body.clone()))?;
        events.push(StreamEvent::Stop {
            reason: "completed".to_string(),
        });
        Ok(events)
    }

    async fn complete_with_chatgpt_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let url = format!(
            "{}/responses",
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://chatgpt.com/backend-api/codex")
                .trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .headers(chatgpt_headers(&self.config.auth)?)
            .json(&openai_responses_body(&self.config, &request))
            .send()
            .await?;
        let response = error_for_status_with_body(response).await?;
        let mut parser = OpenAiResponsesSseParser::default();
        let stream =
            stream_sse_response(response, |data| parser.parse_data(data, on_event)).await?;
        if stream.saw_data {
            on_event(StreamEvent::Stop {
                reason: "completed".to_string(),
            })?;
            return Ok(());
        }
        let events = serde_json::from_str::<Value>(&stream.body)
            .ok()
            .and_then(|response| parse_openai_responses_events(&response))
            .ok_or_else(|| ProviderError::InvalidResponse(stream.body.clone()))?;
        emit_events(events, on_event)?;
        on_event(StreamEvent::Stop {
            reason: "completed".to_string(),
        })
    }

    fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }

    fn chat_base_url(&self) -> Result<String, ProviderError> {
        // Azure's catalog models carry no URL: the resource comes from the
        // environment, and without one the request must fail rather than
        // take an Azure key to OpenAI's endpoint.
        if is_azure_provider(&self.config.model.provider) {
            return azure_openai_base_url(self.config.base_url.as_deref());
        }
        resolve_env_placeholders(
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://api.openai.com/v1"),
        )
    }
}

/// Upstream's Azure provider id, and the id this port used before it.
pub const AZURE_PROVIDER: &str = "azure";
pub const LEGACY_AZURE_PROVIDER: &str = "azure-openai-responses";

fn is_azure_provider(provider: &str) -> bool {
    provider == AZURE_PROVIDER || provider == LEGACY_AZURE_PROVIDER
}

fn openai_chat_completions_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = json!({
        "model": if is_azure_provider(&config.model.provider) {
            azure_deployment_name(&config.model.id)
        } else {
            config.model.id.clone()
        },
        "messages": openai_chat_messages(config, request),
        "stream": true,
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(openai_chat_tools(&request.tools));
        body["tool_choice"] = json!("auto");
    }
    if config.model.provider == "openai" {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if config.model.provider == "openrouter" {
        body["stream_options"] = json!({ "include_usage": true });
        body["store"] = json!(false);
        if let Some(effort) = reasoning_effort(config) {
            body["reasoning"] = json!({ "effort": effort });
        }
    } else if matches!(
        config.model.provider.as_str(),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai"
    ) {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(max_tokens) = provider_max_tokens(config, request) {
        let field = if config.model.provider == "openrouter" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[field] = json!(max_tokens);
    }
    body
}

fn openai_chat_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "strict": false,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect()
}

fn openai_chat_messages(config: &ProviderConfig, request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    // A tool result's images, where the model accepts them, follow their
    // tool round in a user message of their own (upstream's shape): after
    // the last tool result of the run of tool messages they belong to.
    let pending_images = openai_chat_tool_image_groups(config, request);
    let mut image_group = 0usize;
    let mut messages_iter = request.messages.iter().peekable();
    while let Some(message) = messages_iter.next() {
        if message.role == ChatRole::Tool {
            let group_has_images =
                !message.media.is_empty() && matches!(config.traits.accepts_images(), Some(true));
            let group_continues = messages_iter
                .peek()
                .is_some_and(|next| next.role == ChatRole::Tool);
            messages.push(openai_chat_tool_message(config, message));
            if group_has_images && !group_continues {
                if let Some(image_message) = pending_images.get(image_group) {
                    messages.push(image_message.clone());
                    image_group += 1;
                }
            }
            continue;
        }
        if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
            messages.push(json!({
                "role": "assistant",
                "content": if message.content.is_empty() { Value::Null } else { json!(message.content) },
                "tool_calls": message.tool_calls.iter().map(|tool_call| {
                    json!({
                        "id": normalize_openai_chat_tool_call_id(config, &tool_call.id),
                        "type": "function",
                        "function": {
                            "name": tool_call.name,
                            "arguments": tool_call.arguments,
                        },
                    })
                }).collect::<Vec<_>>(),
            }));
            continue;
        }
        let content = if message.media.is_empty() {
            json!(message.content)
        } else if matches!(config.traits.accepts_images(), Some(false)) {
            // A model whose catalog entry lists no image input: the parts
            // would be refused by the endpoint, so the message keeps its
            // text and says the images were left out.
            let note = if message.media.len() == 1 {
                "(1 attached image not sent: the current model does not support image input)"
            } else {
                "(attached images not sent: the current model does not support image input)"
            };
            let mut parts = vec![json!({
                "type": "text",
                "text": message.content,
            })];
            parts.push(json!({
                "type": "text",
                "text": note,
            }));
            Value::Array(parts)
        } else {
            let mut parts = vec![json!({
                "type": "text",
                "text": message.content,
            })];
            parts.extend(message.media.iter().map(|media| {
                json!({
                    "type": "image_url",
                    "image_url": {
                        "url": media_data_url(media),
                    },
                })
            }));
            Value::Array(parts)
        };
        messages.push(json!({
            "role": role_name(&message.role),
            "content": content,
        }));
    }
    messages
}

/// What a tool result's images are replaced by where they cannot be sent.
const TOOL_IMAGE_OMITTED: &str =
    "(tool image omitted: this provider API does not accept images in tool results)";

/// What a tool result's images are replaced by for a model whose catalog
/// entry does not list image input.
const MODEL_IMAGE_OMITTED: &str =
    "(tool image omitted: the current model does not support image input)";

/// A tool result for the chat completions API, whose tool messages are text
/// only: the text alone, or with the omission note the model's input
/// modalities call for. The images themselves, when the model accepts them,
/// are sent in the user message [`openai_chat_messages`] adds after the tool
/// results, as upstream does for the models its catalog lists.
fn tool_result_text_without_images(config: &ProviderConfig, message: &ChatMessage) -> String {
    if message.media.is_empty() {
        return message.content.clone();
    }
    let note = match config.traits.accepts_images() {
        Some(false) => MODEL_IMAGE_OMITTED,
        _ => TOOL_IMAGE_OMITTED,
    };
    if message.content.is_empty() {
        note.to_string()
    } else {
        format!("{}\n{note}", message.content)
    }
}

/// The user messages that carry tool-result images where a tool message
/// cannot: one per run of tool messages whose results hold images, each
/// naming its tool call and then the images as `image_url` parts. Sent only
/// when the model's catalog entry lists image input (upstream's rule).
fn openai_chat_tool_image_groups(config: &ProviderConfig, request: &ProviderRequest) -> Vec<Value> {
    if !matches!(config.traits.accepts_images(), Some(true)) {
        return Vec::new();
    }
    let mut groups = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    for message in &request.messages {
        if message.role != ChatRole::Tool {
            if !current.is_empty() {
                groups.push(json!({ "role": "user", "content": current }));
                current = Vec::new();
            }
            continue;
        }
        if message.media.is_empty() {
            continue;
        }
        current.push(json!({
            "type": "text",
            "text": format!(
                "(images returned by the {} tool call {})",
                message.tool_name.as_deref().unwrap_or("tool"),
                message.tool_call_id.as_deref().unwrap_or("-"),
            ),
        }));
        current.extend(message.media.iter().map(|media| {
            json!({
                "type": "image_url",
                "image_url": { "url": media_data_url(media) },
            })
        }));
    }
    if !current.is_empty() {
        groups.push(json!({ "role": "user", "content": current }));
    }
    groups
}

/// One chat completions tool message: the result text, with its images'
/// omission note when they travel in a user message instead.
fn openai_chat_tool_message(config: &ProviderConfig, message: &ChatMessage) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": message
            .tool_call_id
            .as_deref()
            .map(|id| normalize_openai_chat_tool_call_id(config, id)),
        "content": tool_result_text_without_images(config, message),
    })
}

fn normalize_openai_chat_tool_call_id(config: &ProviderConfig, id: &str) -> String {
    if config.api == ProviderApi::Mistral {
        return derive_mistral_tool_call_id(id);
    }
    if let Some((call_id, _item_id)) = id.split_once('|') {
        return call_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
    }
    if config.model.provider == "openai" && id.len() > 40 {
        id.chars().take(40).collect()
    } else {
        id.to_string()
    }
}

fn derive_mistral_tool_call_id(id: &str) -> String {
    const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
    let normalized = id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    if normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed = if normalized.is_empty() {
        id
    } else {
        normalized.as_str()
    };
    short_hash(seed)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

/// Tokens upstream keeps free for the request beyond its estimate
/// (`CONTEXT_SAFETY_TOKENS`).
const CONTEXT_SAFETY_TOKENS: u64 = 4_096;

/// Upstream `clampMaxTokensToContext` (`packages/ai/src/api/simple-options.ts`):
/// the model's `maxTokens`, lowered so the estimated input plus the answer
/// fit its context window. `None` for a model without a known output cap.
fn provider_max_tokens(config: &ProviderConfig, request: &ProviderRequest) -> Option<u64> {
    if let Some(limit) = request.max_output_tokens {
        return Some(limit.max(1));
    }
    let max_tokens = config.traits.max_tokens?;
    let Some(context_window) = config.traits.context_window else {
        return Some(max_tokens);
    };
    let available = context_window
        .saturating_sub(estimate_request_tokens(request))
        .saturating_sub(CONTEXT_SAFETY_TOKENS)
        .max(1);
    Some(max_tokens.min(available))
}

/// Upstream `estimateContextTokens` (`packages/ai/src/utils/estimate.ts`)
/// without its usage shortcut: `ceil(chars / 3.5)` per message, where the
/// leading system message carries the prompt and the tool definitions' JSON
/// and each image counts as 4800 characters.
fn estimate_request_tokens(request: &ProviderRequest) -> u64 {
    const ESTIMATED_IMAGE_CHARS: usize = 4_800;
    fn tokens(chars: usize) -> u64 {
        (chars as u64 * 2).div_ceil(7)
    }
    let mut total = 0;
    if let Some(system_prompt) = &request.system_prompt {
        total += tokens(system_prompt.chars().count());
    }
    if !request.tools.is_empty() {
        let tools = serde_json::to_string(&request.tools).unwrap_or_default();
        total += tokens(tools.chars().count());
    }
    for message in &request.messages {
        let tool_call_chars: usize = message
            .tool_calls
            .iter()
            .map(|call| call.name.chars().count() + call.arguments.chars().count())
            .sum();
        total += tokens(
            message.content.chars().count()
                + message.media.len() * ESTIMATED_IMAGE_CHARS
                + tool_call_chars,
        );
    }
    total
}

/// Where the request's tools go for a Responses transport
/// (`getResponsesToolSupport`): in place as `additional_tools` input items
/// when the model can anchor them mid-conversation, otherwise the
/// top-level `tools` field.
fn responses_tools_in_place(config: &ProviderConfig) -> bool {
    config
        .traits
        .supports_mid_convo_system_messages
        .unwrap_or(false)
        && config.traits.supports_additional_tools.unwrap_or(false)
}

/// The leading `additional_tools` item declaring the request's tools in
/// place, or nothing when the transport keeps them top-level.
fn additional_tools_item(tools: &[ToolDefinition], strict: Value) -> Option<Value> {
    (!tools.is_empty()).then(|| {
        json!({
            "type": "additional_tools",
            "role": "developer",
            "tools": openai_responses_tools(tools, strict),
        })
    })
}

fn openai_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    if config.model.provider == "openai-codex" {
        return openai_codex_responses_body(config, request);
    }
    if config.model.provider == "github-copilot" {
        return github_copilot_responses_body(config, request);
    }
    let mut input = openai_responses_input_with_developer_system(request);
    if responses_tools_in_place(config) {
        if let Some(item) = additional_tools_item(&request.tools, json!(false)) {
            input.insert(0, item);
        }
    }
    let mut body = json!({
        "model": config.model.id,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if !responses_tools_in_place(config) && !request.tools.is_empty() {
        body["tools"] = json!(openai_responses_tools(&request.tools, json!(false)));
    }
    if let Some(effort) = reasoning_effort(config) {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }
    if let Some(max_tokens) = provider_max_tokens(config, request) {
        body["max_output_tokens"] = json!(max_tokens);
    }
    body
}

fn github_copilot_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut input = openai_responses_input_with_developer_system(request);
    if responses_tools_in_place(config) {
        if let Some(item) = additional_tools_item(&request.tools, Value::Null) {
            input.insert(0, item);
        }
    }
    let mut body = json!({
        "model": config.model.id,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    // Copilot is sent no reasoning parameter when thinking is off
    // (upstream `openai-responses.ts`).
    if let Some(effort) = reasoning_effort(config).filter(|_| !thinking_is_off(config)) {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }
    if let Some(max_tokens) = provider_max_tokens(config, request) {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if !responses_tools_in_place(config) && !request.tools.is_empty() {
        body["tools"] = json!(openai_responses_tools(&request.tools, json!(false)));
    }
    body
}

fn openai_codex_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut input = openai_responses_input(request);
    if responses_tools_in_place(config) {
        if let Some(item) = additional_tools_item(&request.tools, Value::Null) {
            input.insert(0, item);
        }
    }
    let mut body = json!({
        "model": config.model.id,
        "store": false,
        "stream": true,
        // The Codex backend rejects a request without instructions, so a
        // caller that sends no system prompt gets upstream's placeholder
        // (`openai-codex-responses.ts`). noel's sessions always send one.
        "instructions": request
            .system_prompt
            .clone()
            .filter(|prompt| !prompt.trim().is_empty())
            .unwrap_or_else(|| "You are a helpful assistant.".to_string()),
        "input": input,
        "text": {
            "verbosity": "low",
        },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });
    if let Some(effort) = reasoning_effort(config) {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }
    if !responses_tools_in_place(config) && !request.tools.is_empty() {
        body["tools"] = json!(openai_responses_tools(&request.tools, Value::Null));
    }
    body
}

fn openai_responses_tools(tools: &[ToolDefinition], strict: Value) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters.clone(),
                "strict": strict.clone(),
            })
        })
        .collect()
}

fn openai_responses_input_with_developer_system(request: &ProviderRequest) -> Vec<Value> {
    let mut input = Vec::new();
    if let Some(system_prompt) = request.system_prompt.as_deref() {
        if !system_prompt.trim().is_empty() {
            input.push(json!({
                "role": "developer",
                "content": system_prompt,
            }));
        }
    }
    input.extend(openai_responses_input(request));
    input
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiResponsesEndpoint {
    OpenAi,
    Codex,
    Azure,
}

struct OpenAiResponsesProvider {
    config: ProviderConfig,
    endpoint: OpenAiResponsesEndpoint,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let (url, headers, model) = self.request_parts(request_has_media(&request))?;
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&{
                let mut body = openai_responses_body(&self.config, &request);
                body["model"] = json!(model);
                body
            })
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        let mut events = parse_openai_responses_sse_events(&body)
            .or_else(|| {
                serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|response| parse_openai_responses_events(&response))
            })
            .ok_or_else(|| ProviderError::InvalidResponse(body.clone()))?;
        events.push(StreamEvent::Stop {
            reason: "completed".to_string(),
        });
        Ok(events)
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let (url, headers, model) = self.request_parts(request_has_media(&request))?;
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&{
                let mut body = openai_responses_body(&self.config, &request);
                body["model"] = json!(model);
                body
            })
            .send()
            .await?;
        let response = error_for_status_with_body(response).await?;
        let mut parser = OpenAiResponsesSseParser::default();
        let stream =
            stream_sse_response(response, |data| parser.parse_data(data, on_event)).await?;
        if stream.saw_data {
            on_event(StreamEvent::Stop {
                reason: "completed".to_string(),
            })?;
            return Ok(());
        }
        let events = serde_json::from_str::<Value>(&stream.body)
            .ok()
            .and_then(|response| parse_openai_responses_events(&response))
            .ok_or_else(|| ProviderError::InvalidResponse(stream.body.clone()))?;
        emit_events(events, on_event)?;
        on_event(StreamEvent::Stop {
            reason: "completed".to_string(),
        })
    }
}

impl OpenAiResponsesProvider {
    fn request_parts(&self, has_media: bool) -> Result<(String, HeaderMap, String), ProviderError> {
        match self.endpoint {
            OpenAiResponsesEndpoint::OpenAi => {
                let api_key = self.api_key()?;
                let base_url = resolve_env_placeholders(
                    self.config
                        .base_url
                        .as_deref()
                        .unwrap_or("https://api.openai.com/v1"),
                )?;
                let headers = if self.config.model.provider == "github-copilot" {
                    openai_compatible_headers(&self.config, &api_key, has_media)?
                } else {
                    bearer_headers(&api_key)?
                };
                Ok((
                    format!("{}/responses", base_url.trim_end_matches('/')),
                    headers,
                    self.config.model.id.clone(),
                ))
            }
            OpenAiResponsesEndpoint::Codex => {
                let url = codex_responses_url(
                    self.config
                        .base_url
                        .as_deref()
                        .unwrap_or("https://chatgpt.com/backend-api"),
                )?;
                Ok((
                    url,
                    codex_headers(&self.config.auth)?,
                    self.config.model.id.clone(),
                ))
            }
            OpenAiResponsesEndpoint::Azure => {
                let api_key = self.api_key()?;
                let base_url = azure_openai_base_url(self.config.base_url.as_deref())?;
                let url = azure_openai_responses_url(&base_url);
                Ok((
                    url,
                    azure_openai_headers(&api_key)?,
                    azure_deployment_name(&self.config.model.id),
                ))
            }
        }
    }

    fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

struct AnthropicProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let url = anthropic_messages_url(&self.config)?;

        let response = self
            .client
            .post(url)
            .headers(anthropic_headers(&self.config)?)
            .json(&anthropic_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        if let Some(mut events) = parse_anthropic_sse_events(&body) {
            events.push(StreamEvent::Stop {
                reason: "stop".to_string(),
            });
            return Ok(events);
        }
        let response = serde_json::from_str::<Value>(&body)
            .map_err(|_| ProviderError::InvalidResponse(body.clone()))?;
        let mut events = parse_anthropic_response_events(&response)
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        events.push(StreamEvent::Stop {
            reason: response
                .get("stop_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        });
        Ok(events)
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let url = anthropic_messages_url(&self.config)?;
        let response = self
            .client
            .post(url)
            .headers(anthropic_headers(&self.config)?)
            .json(&anthropic_body(&self.config, &request))
            .send()
            .await?;
        let response = error_for_status_with_body(response).await?;
        let mut parser = AnthropicSseParser::default();
        let stream =
            stream_sse_response(response, |data| parser.parse_data(data, on_event)).await?;
        if stream.saw_data {
            on_event(StreamEvent::Stop {
                reason: parser.stop_reason.unwrap_or_else(|| "stop".to_string()),
            })?;
            return Ok(());
        }
        let response = serde_json::from_str::<Value>(&stream.body)
            .map_err(|_| ProviderError::InvalidResponse(stream.body.clone()))?;
        let events = parse_anthropic_response_events(&response)
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        emit_events(events, on_event)?;
        on_event(StreamEvent::Stop {
            reason: response
                .get("stop_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        })
    }
}

fn anthropic_messages_url(config: &ProviderConfig) -> Result<String, ProviderError> {
    let base_url = resolve_env_placeholders(
        config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com/v1"),
    )?;
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/messages") {
        return Ok(normalized.to_string());
    }
    if normalized.ends_with("/v1") {
        return Ok(format!("{normalized}/messages"));
    }
    // A base URL names the API root, as it does for upstream's SDK client:
    // the catalog's `https://api.anthropic.com`, a gateway's
    // `https://api.kimi.com/coding`.
    Ok(format!("{normalized}/v1/messages"))
}

fn anthropic_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let max_tokens = provider_max_tokens(config, request)
        .unwrap_or(ANTHROPIC_UNCATALOGED_MAX_TOKENS)
        .max(1);
    let mut body = json!({
        "model": config.model.id,
        "max_tokens": max_tokens,
        "stream": true,
        "system": anthropic_system(config, request),
        "messages": anthropic_messages_with_cache_control(&request.messages, true),
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(anthropic_tools(&request.tools));
    }
    apply_anthropic_thinking(&mut body, config);
    // A thinking budget must stay below max_tokens. An explicit cap keeps half
    // of it for the answer; the model default keeps upstream's 1024 tokens.
    if body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled") {
        let answer_room = if request.max_output_tokens.is_some() {
            max_tokens / 2
        } else {
            max_tokens.saturating_sub(1024)
        };
        if max_tokens <= 1024 {
            body["thinking"] = json!({"type":"disabled"});
        } else if let Some(budget) = body
            .pointer("/thinking/budget_tokens")
            .and_then(Value::as_u64)
        {
            body["thinking"]["budget_tokens"] = json!(budget.min(answer_room.max(1024)));
        }
    }
    body
}

/// The output cap of an Anthropic Messages request for a model without a
/// catalog limit: Anthropic requires one, and this is upstream's default.
const ANTHROPIC_UNCATALOGED_MAX_TOKENS: u64 = 16_384;

fn anthropic_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    // Anthropic allows at most 4 cache_control breakpoints per request, and
    // caching is prefix-based: a single breakpoint on the last tool caches all
    // preceding tool definitions. Marking every tool would blow the budget once
    // system blocks and the last user message add their own breakpoints.
    let last_index = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut value = json!({
                "name": tool.name,
                "description": tool.description,
                "eager_input_streaming": true,
                "input_schema": tool.parameters,
            });
            if index == last_index {
                value["cache_control"] = json!({ "type": "ephemeral" });
            }
            value
        })
        .collect()
}

/// Minimum version accepted for current OAuth-gated models; the CLI replaces
/// this with the latest published Claude Code version whenever it can reach
/// the npm registry. Matches the `claudeCodeVersion` upstream pins in
/// `packages/ai/src/api/anthropic-messages.ts`.
pub const DEFAULT_CLAUDE_CODE_VERSION: &str = "2.1.280";
pub const CLAUDE_CODE_VERSION_ENV: &str = "NOEL_CLAUDE_CODE_VERSION";

static CLAUDE_CODE_VERSION_OVERRIDE: std::sync::RwLock<Option<String>> =
    std::sync::RwLock::new(None);

pub fn set_claude_code_version(version: impl Into<String>) {
    *CLAUDE_CODE_VERSION_OVERRIDE
        .write()
        .expect("claude code version lock") = Some(version.into());
}

pub fn claude_code_version() -> String {
    resolve_claude_code_version(
        env::var(CLAUDE_CODE_VERSION_ENV).ok().as_deref(),
        CLAUDE_CODE_VERSION_OVERRIDE
            .read()
            .expect("claude code version lock")
            .as_deref(),
    )
}

fn resolve_claude_code_version(env_value: Option<&str>, override_value: Option<&str>) -> String {
    [env_value, override_value]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|version| !version.is_empty())
        .unwrap_or(DEFAULT_CLAUDE_CODE_VERSION)
        .to_string()
}

fn anthropic_headers(config: &ProviderConfig) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    match &config.auth {
        ProviderAuth::ApiKey(api_key) => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
        }
        ProviderAuth::ClaudeCodeOAuth { access_token } => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {access_token}"))?,
            );
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("claude-code-20250219,oauth-2025-04-20"),
            );
            headers.insert(
                "anthropic-dangerous-direct-browser-access",
                HeaderValue::from_static("true"),
            );
            headers.insert(
                USER_AGENT,
                HeaderValue::from_str(&format!("claude-cli/{}", claude_code_version()))?,
            );
            headers.insert("x-app", HeaderValue::from_static("cli"));
        }
        _ => {
            return Err(ProviderError::MissingApiKey {
                provider: config.model.provider.clone(),
            });
        }
    }
    if config.model.provider == "kimi-coding" {
        headers.insert(USER_AGENT, HeaderValue::from_static("KimiCLI/1.5"));
    }
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn anthropic_system(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    if matches!(&config.auth, ProviderAuth::ClaudeCodeOAuth { .. }) {
        let mut blocks = vec![anthropic_cached_text_block(
            "You are Claude Code, Anthropic's official CLI for Claude.",
        )];
        if let Some(system_prompt) = request.system_prompt.as_deref() {
            if !system_prompt.trim().is_empty() {
                blocks.push(anthropic_cached_text_block(system_prompt));
            }
        }
        Value::Array(blocks)
    } else if let Some(system_prompt) = request.system_prompt.as_deref() {
        if system_prompt.trim().is_empty() {
            Value::String(String::new())
        } else {
            Value::Array(vec![anthropic_cached_text_block(system_prompt)])
        }
    } else {
        Value::String(String::new())
    }
}

fn anthropic_cached_text_block(text: &str) -> Value {
    json!({
        "type": "text",
        "text": text,
        "cache_control": {
            "type": "ephemeral",
        },
    })
}

fn apply_anthropic_thinking(body: &mut Value, config: &ProviderConfig) {
    let Some(level) = unclamped_thinking_level(config) else {
        return;
    };
    if level == "off" {
        body["thinking"] = json!({ "type": "disabled" });
        return;
    }
    if anthropic_supports_adaptive_thinking(&config.model.id) {
        body["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
        });
        body["output_config"] = json!({
            "effort": mapped_thinking_level(config, level)
                .unwrap_or_else(|| anthropic_adaptive_effort(&config.model.id, level)),
        });
    } else {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": config.thinking_budget_tokens.unwrap_or(1024).max(1024),
            "display": "summarized",
        });
    }
}

/// Parses the `(major, minor)` version of a Claude model id for a family
/// ("opus", "sonnet", ...), tolerating `-`, `.`, `_`, `:` separators and
/// prefixes/suffixes such as `us.anthropic.claude-opus-4-6-v1`.
pub fn claude_model_version(model_id: &str, family: &str) -> Option<(u32, u32)> {
    let normalized = model_id.to_ascii_lowercase().replace(['.', '_', ':'], "-");
    let rest = normalized.split(&format!("{family}-")).nth(1)?;
    let mut parts = rest.split('-');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Adaptive thinking (`thinking.type = "adaptive"` + `output_config.effort`)
/// is supported by Opus >= 4.6 and Sonnet >= 4.6. Version-parsed so new
/// models work without a code change.
pub fn anthropic_supports_adaptive_thinking(model_id: &str) -> bool {
    claude_model_version(model_id, "opus").is_some_and(|v| v >= (4, 6))
        || claude_model_version(model_id, "sonnet").is_some_and(|v| v >= (4, 6))
}

fn anthropic_adaptive_effort(model_id: &str, level: &str) -> &'static str {
    match level {
        "minimal" | "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" if model_id.contains("opus") => "max",
        "max" => "xhigh",
        _ => "high",
    }
}

/// The reasoning effort a request names for the configured thinking level:
/// the model's own name for the level where its catalog entry has one
/// (upstream `model.thinkingLevelMap?.[level] ?? level`), `none` for `off`,
/// and otherwise the level itself. `None` without a thinking level.
fn reasoning_effort(config: &ProviderConfig) -> Option<String> {
    let level = request_thinking_level(config)?;
    if let Some(mapped) = mapped_thinking_level(config, level) {
        return Some(mapped.to_string());
    }
    Some(
        match level {
            "off" => "none",
            // Only a model whose entry names `max` is set to it; a caller
            // that asks anyway gets the highest effort the API defines.
            "max" => "xhigh",
            level => level,
        }
        .to_string(),
    )
}

/// The thinking level a request is built for. A model known to have no
/// reasoning gets none, and a level a known model does not support becomes
/// the nearest one it does (upstream `clampThinkingLevel`, which its request
/// builders apply too). A model nothing is known about keeps the level as
/// configured.
fn request_thinking_level(config: &ProviderConfig) -> Option<&'static str> {
    let level = normalized_thinking_level(config.thinking_level.as_deref())?;
    match config.traits.reasoning {
        Some(false) => None,
        Some(true) => Some(catalog::clamp_thinking_level(
            true,
            config.traits.thinking_level_map.as_ref(),
            level,
        )),
        None => Some(level),
    }
}

/// The level for the request builders that do not clamp (Anthropic and
/// Bedrock, as upstream's): the configured one, or none for a model known
/// to have no reasoning or that cannot turn thinking off when asked to.
fn unclamped_thinking_level(config: &ProviderConfig) -> Option<&'static str> {
    let level = normalized_thinking_level(config.thinking_level.as_deref())?;
    let cannot_turn_off = || {
        config
            .traits
            .thinking_level_map
            .as_ref()
            .is_some_and(|map| map.get("off") == Some(&None))
    };
    if config.traits.reasoning == Some(false) || (level == "off" && cannot_turn_off()) {
        return None;
    }
    Some(level)
}

/// The model's own name for `level`, if its catalog entry gives one.
fn mapped_thinking_level<'a>(config: &'a ProviderConfig, level: &str) -> Option<&'a str> {
    config
        .traits
        .thinking_level_map
        .as_ref()?
        .get(level)?
        .as_deref()
}

fn thinking_is_off(config: &ProviderConfig) -> bool {
    request_thinking_level(config) == Some("off")
}

fn normalized_thinking_level(level: Option<&str>) -> Option<&'static str> {
    match level?.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => Some("off"),
        "minimal" | "min" => Some("minimal"),
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "extra-high" | "extra_high" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

struct GoogleProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for GoogleProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let response = self
            .client
            .post(google_url(&self.config)?)
            .headers(google_headers(&api_key)?)
            .json(&google_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        if let Some(mut events) = parse_google_sse_events(&body) {
            events.push(StreamEvent::Stop {
                reason: "STOP".to_string(),
            });
            return Ok(events);
        }
        parse_google_response(
            serde_json::from_str::<Value>(&body)
                .map_err(|_| ProviderError::InvalidResponse(body.clone()))?,
        )
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let api_key = self.api_key()?;
        let response = self
            .client
            .post(google_url(&self.config)?)
            .headers(google_headers(&api_key)?)
            .json(&google_body(&self.config, &request))
            .send()
            .await?;
        let response = error_for_status_with_body(response).await?;
        let stream =
            stream_sse_response(response, |data| parse_google_sse_data(data, on_event)).await?;
        if stream.saw_data {
            on_event(StreamEvent::Stop {
                reason: "STOP".to_string(),
            })?;
            return Ok(());
        }
        let events = parse_google_response(
            serde_json::from_str::<Value>(&stream.body)
                .map_err(|_| ProviderError::InvalidResponse(stream.body.clone()))?,
        )?;
        emit_events(events, on_event)
    }
}

impl GoogleProvider {
    fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

fn google_url(config: &ProviderConfig) -> Result<String, ProviderError> {
    let base_url = resolve_env_placeholders(
        config
            .base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com/v1beta"),
    )?;
    Ok(format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        base_url.trim_end_matches('/'),
        encode_path_segment(&config.model.id)
    ))
}

fn google_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert("x-goog-api-key", HeaderValue::from_str(api_key)?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn google_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut generation_config = json!({});
    if let Some(max_tokens) = provider_max_tokens(config, request) {
        generation_config["maxOutputTokens"] = json!(max_tokens);
    }
    if let Some(thinking_config) = google_thinking_config(config) {
        generation_config["thinkingConfig"] = thinking_config;
    }
    let mut body = json!({
        "contents": google_messages(config, &request.messages),
        "systemInstruction": request.system_prompt.as_ref().map(|text| {
            json!({
                "parts": [{ "text": text }],
                "role": "user",
            })
        }),
        "generationConfig": generation_config,
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(google_tools(&request.tools));
    }
    body
}

fn google_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    vec![json!({
        "functionDeclarations": tools.iter().map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parametersJsonSchema": tool.parameters,
            })
        }).collect::<Vec<_>>(),
    })]
}

fn google_thinking_config(config: &ProviderConfig) -> Option<Value> {
    let level = request_thinking_level(config)?;
    if level == "off" {
        return Some(json!({ "thinkingBudget": 0 }));
    }
    let budget = config
        .thinking_budget_tokens
        .map(|budget| budget as i64)
        .unwrap_or_else(|| google_default_thinking_budget(&config.model.id, level));
    Some(json!({
        "includeThoughts": true,
        "thinkingBudget": budget,
    }))
}

fn google_default_thinking_budget(model_id: &str, level: &str) -> i64 {
    if model_id.contains("2.5-pro") {
        return match level {
            "minimal" => 128,
            "low" => 2048,
            "medium" => 8192,
            _ => 32768,
        };
    }
    if model_id.contains("2.5-flash-lite") {
        return match level {
            "minimal" => 512,
            "low" => 2048,
            "medium" => 8192,
            _ => 24576,
        };
    }
    if model_id.contains("2.5-flash") {
        return match level {
            "minimal" => 128,
            "low" => 2048,
            "medium" => 8192,
            _ => 24576,
        };
    }
    -1
}

struct GoogleVertexProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for GoogleVertexProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let url = google_vertex_url(&self.config)?;
        let response = self
            .client
            .post(format!("{url}?key={api_key}"))
            .json(&google_body(&self.config, &request))
            .send()
            .await?;
        parse_google_response(
            error_for_status_with_body(response)
                .await?
                .json::<Value>()
                .await?,
        )
    }
}

impl GoogleVertexProvider {
    fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

struct MistralProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for MistralProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let response = self
            .client
            .post(mistral_chat_url(&self.config)?)
            .headers(mistral_headers(&self.config, &api_key)?)
            .json(&mistral_chat_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        if let Some(mut events) = parse_openai_chat_completions_sse_events(&body) {
            events.push(StreamEvent::Stop {
                reason: "stop".to_string(),
            });
            return Ok(events);
        }
        let response = serde_json::from_str::<Value>(&body)
            .map_err(|_| ProviderError::InvalidResponse(body.clone()))?;
        if let Some(mut events) = parse_openai_chat_completions_events(&response) {
            events.push(StreamEvent::Stop {
                reason: response
                    .pointer("/choices/0/finish_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("stop")
                    .to_string(),
            });
            return Ok(events);
        }
        Err(ProviderError::InvalidResponse(response.to_string()))
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let api_key = self.api_key()?;
        let response = self
            .client
            .post(mistral_chat_url(&self.config)?)
            .headers(mistral_headers(&self.config, &api_key)?)
            .json(&mistral_chat_body(&self.config, &request))
            .send()
            .await?;
        let response = error_for_status_with_body(response).await?;
        let mut parser = OpenAiChatCompletionsSseParser::default();
        let stream =
            stream_sse_response(response, |data| parser.parse_data(data, on_event)).await?;
        if stream.saw_data {
            let reason = parser
                .stop_reason
                .clone()
                .unwrap_or_else(|| "stop".to_string());
            parser.finish(on_event)?;
            on_event(StreamEvent::Stop { reason })?;
            return Ok(());
        }
        let response = serde_json::from_str::<Value>(&stream.body)
            .map_err(|_| ProviderError::InvalidResponse(stream.body.clone()))?;
        let events = parse_openai_chat_completions_events(&response)
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        emit_events(events, on_event)?;
        on_event(StreamEvent::Stop {
            reason: response
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        })
    }
}

impl MistralProvider {
    fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

/// Mistral's chat completions URL. A base URL names the API root, as it does
/// for upstream's SDK client (the catalog's `https://api.mistral.ai`); one
/// that already ends in `/v1` is used as it is.
fn mistral_chat_url(config: &ProviderConfig) -> Result<String, ProviderError> {
    let base_url = resolve_env_placeholders(
        config
            .base_url
            .as_deref()
            .unwrap_or("https://api.mistral.ai"),
    )?;
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/v1") {
        Ok(format!("{base_url}/chat/completions"))
    } else {
        Ok(format!("{base_url}/v1/chat/completions"))
    }
}

fn mistral_headers(config: &ProviderConfig, api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = bearer_headers(api_key)?;
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(USER_AGENT, HeaderValue::from_str(pi_user_agent())?);
    if let Some(session_id) = &config.session_id {
        headers.insert("x-affinity", HeaderValue::from_str(session_id)?);
    }
    Ok(headers)
}

fn mistral_chat_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = json!({
        "model": config.model.id,
        "stream": true,
        "messages": mistral_messages(config, request),
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(openai_chat_tools(&request.tools));
    }
    if let Some(max_tokens) = provider_max_tokens(config, request) {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(session_id) = &config.session_id {
        body["prompt_cache_key"] = json!(session_id);
    }
    body
}

fn mistral_messages(config: &ProviderConfig, request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.extend(request.messages.iter().map(|message| {
        if message.role == ChatRole::Tool {
            return json!({
                "role": "tool",
                "content": [{
                    "type": "text",
                    "text": message.content,
                }],
                "tool_call_id": message.tool_call_id.as_deref().map(|id| normalize_openai_chat_tool_call_id(config, id)),
                "name": message.tool_name,
            });
        }
        if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
            return json!({
                "role": "assistant",
                "tool_calls": message.tool_calls.iter().enumerate().map(|(index, tool_call)| {
                    json!({
                        "id": normalize_openai_chat_tool_call_id(config, &tool_call.id),
                        "type": "function",
                        "function": {
                            "name": tool_call.name,
                            "arguments": tool_call.arguments,
                        },
                        "index": index,
                    })
                }).collect::<Vec<_>>(),
                "prefix": false,
            });
        }
        json!({
            "role": role_name(&message.role),
            "content": message.content,
        })
    }));
    messages
}

struct BedrockProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let bearer_token = self.bearer_token()?;
        let base_url = resolve_env_placeholders(
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://bedrock-runtime.us-east-1.amazonaws.com"),
        )?;
        let response = self
            .client
            .post(format!(
                "{}/model/{}/converse",
                base_url.trim_end_matches('/'),
                encode_path_segment(&self.config.model.id)
            ))
            .headers(bearer_headers(&bearer_token)?)
            .json(&bedrock_body(&self.config, &request))
            .send()
            .await?;
        parse_bedrock_response(
            error_for_status_with_body(response)
                .await?
                .json::<Value>()
                .await?,
        )
    }
}

fn bedrock_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = json!({
        "modelId": config.model.id,
        "messages": bedrock_messages_with_cache(config, &request.messages),
        "inferenceConfig": provider_max_tokens(config, request)
            .map(|max_tokens| json!({ "maxTokens": max_tokens }))
            .unwrap_or_else(|| json!({})),
    });
    if !request.tools.is_empty() {
        body["toolConfig"] = json!({
            "tools": bedrock_tools(&request.tools),
        });
    }
    if let Some(system) = bedrock_system(config, request.system_prompt.as_deref()) {
        body["system"] = system;
    }
    if let Some(fields) = bedrock_additional_model_request_fields(config) {
        body["additionalModelRequestFields"] = fields;
    }
    body
}

fn bedrock_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": {
                        "json": tool.parameters,
                    },
                },
            })
        })
        .collect()
}

fn bedrock_system(config: &ProviderConfig, system_prompt: Option<&str>) -> Option<Value> {
    let system_prompt = system_prompt?;
    let mut blocks = vec![json!({ "text": system_prompt })];
    if bedrock_supports_prompt_cache(&config.model.id) {
        blocks.push(json!({ "cachePoint": { "type": "default" } }));
    }
    Some(Value::Array(blocks))
}

fn bedrock_messages_with_cache(config: &ProviderConfig, messages: &[ChatMessage]) -> Vec<Value> {
    let mut messages = bedrock_messages(messages);
    if bedrock_supports_prompt_cache(&config.model.id) {
        if let Some(last_user) = messages
            .iter_mut()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        {
            if let Some(content) = last_user.get_mut("content").and_then(Value::as_array_mut) {
                content.push(json!({ "cachePoint": { "type": "default" } }));
            }
        }
    }
    messages
}

fn bedrock_additional_model_request_fields(config: &ProviderConfig) -> Option<Value> {
    let level = unclamped_thinking_level(config)?;
    if !bedrock_is_anthropic_claude(&config.model.id) || level == "off" {
        return None;
    }
    if bedrock_supports_adaptive_thinking(&config.model.id) {
        return Some(json!({
            "thinking": {
                "type": "adaptive",
                "display": "summarized",
            },
            "output_config": {
                "effort": mapped_thinking_level(config, level)
                    .unwrap_or_else(|| bedrock_adaptive_effort(&config.model.id, level)),
            },
        }));
    }
    Some(json!({
        "thinking": {
            "type": "enabled",
            "budget_tokens": config.thinking_budget_tokens.unwrap_or(1024).max(1024),
            "display": "summarized",
        },
        "anthropic_beta": ["interleaved-thinking-2025-05-14"],
    }))
}

fn bedrock_is_anthropic_claude(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("claude")
}

fn bedrock_supports_prompt_cache(model_id: &str) -> bool {
    let normalized = model_id.to_ascii_lowercase();
    normalized.contains("-4-")
        || normalized.contains("claude-3-7-sonnet")
        || normalized.contains("claude-3-5-haiku")
}

fn bedrock_supports_adaptive_thinking(model_id: &str) -> bool {
    anthropic_supports_adaptive_thinking(model_id)
}

fn bedrock_adaptive_effort(model_id: &str, level: &str) -> &'static str {
    if level == "max" {
        return "max";
    }
    if level == "xhigh" && claude_model_version(model_id, "opus").is_some_and(|v| v >= (4, 7)) {
        return "xhigh";
    }
    match level {
        "minimal" | "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
}

impl BedrockProvider {
    fn bearer_token(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

/// Decode provider-reported request totals; streaming events are cumulative
/// snapshots, not deltas. Never invent estimates when usage is absent.
fn token_usage(usage: &Value) -> Option<StreamEvent> {
    let input = [
        "input_tokens",
        "prompt_tokens",
        "promptTokenCount",
        "inputTokens",
    ]
    .iter()
    .find_map(|key| usage.get(key).and_then(Value::as_u64));
    let output = [
        "output_tokens",
        "completion_tokens",
        "candidatesTokenCount",
        "outputTokens",
    ]
    .iter()
    .find_map(|key| usage.get(key).and_then(Value::as_u64));
    if input.is_none() && output.is_none() {
        return None;
    }
    let cached = ["cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .filter_map(|key| usage.get(key).and_then(Value::as_u64))
        .fold(0u64, u64::saturating_add);
    Some(StreamEvent::Usage {
        input_tokens: input.unwrap_or_default().saturating_add(cached),
        output_tokens: output.unwrap_or_default().saturating_add(
            usage
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        ),
    })
}

fn response_usage(response: &Value) -> Option<StreamEvent> {
    response
        .get("usage")
        .or_else(|| response.get("usageMetadata"))
        .and_then(token_usage)
}

fn parse_google_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut events = parse_google_response_events(&response)
        .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
    events.push(StreamEvent::Stop {
        reason: response
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            .unwrap_or("STOP")
            .to_string(),
    });
    Ok(events)
}

fn parse_google_response_events(response: &Value) -> Option<Vec<StreamEvent>> {
    let mut events = response
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .and_then(|parts| google_part_events(parts))
        .unwrap_or_default();
    if let Some(usage) = response_usage(response) {
        events.push(usage);
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_google_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    let mut events = Vec::new();
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(response) = serde_json::from_str::<Value>(data) {
                if let Some(mut next) = parse_google_response_events(&response) {
                    events.append(&mut next);
                }
            }
        }
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_google_sse_data(
    data: &str,
    on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
) -> Result<(), ProviderError> {
    if let Ok(response) = serde_json::from_str::<Value>(data) {
        if response
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            == Some("MAX_TOKENS")
        {
            on_event(StreamEvent::Stop {
                reason: "max_tokens".into(),
            })?;
        }
        if let Some(events) = parse_google_response_events(&response) {
            emit_events(events, on_event)?;
        }
    }
    Ok(())
}

fn google_part_events(parts: &[Value]) -> Option<Vec<StreamEvent>> {
    let mut events = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            if let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                events.push(StreamEvent::Thinking(text.to_string()));
            }
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                events.push(StreamEvent::Text(text.to_string()));
            }
        }
        if let Some(function_call) = part.get("functionCall") {
            let Some(name) = function_call.get("name").and_then(Value::as_str) else {
                continue;
            };
            events.push(StreamEvent::ToolCall {
                id: function_call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("{name}_{index}")),
                name: name.to_string(),
                arguments: function_call
                    .get("args")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "{}".to_string()),
            });
        }
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_bedrock_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut events = parse_bedrock_response_events(&response)
        .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
    events.push(StreamEvent::Stop {
        reason: response
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
            .to_string(),
    });
    Ok(events)
}

fn parse_bedrock_response_events(response: &Value) -> Option<Vec<StreamEvent>> {
    let content = response
        .pointer("/output/message/content")
        .and_then(Value::as_array)?;
    let mut events = Vec::new();
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                events.push(StreamEvent::Text(text.to_string()));
            }
        }
        if let Some(tool_use) = item.get("toolUse") {
            let Some(id) = tool_use.get("toolUseId").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = tool_use.get("name").and_then(Value::as_str) else {
                continue;
            };
            events.push(StreamEvent::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: tool_use
                    .get("input")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "{}".to_string()),
            });
        }
    }
    if let Some(usage) = response_usage(response) {
        events.push(usage);
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_anthropic_response_events(response: &Value) -> Option<Vec<StreamEvent>> {
    let mut events = Vec::new();
    let content = response.get("content").and_then(Value::as_array)?;
    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.push(StreamEvent::Text(text.to_string()));
                    }
                }
            }
            Some("thinking") => {
                if let Some(text) = item
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    events.push(StreamEvent::Thinking(text.to_string()));
                }
            }
            Some("tool_use") => {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let arguments = item
                    .get("input")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "{}".to_string());
                events.push(StreamEvent::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = response_usage(response) {
        events.push(usage);
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

#[derive(Debug)]
struct PendingAnthropicToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct AnthropicSseParser {
    input_tokens: u64,
    output_tokens: u64,
    pending_tool_calls: BTreeMap<u64, PendingAnthropicToolCall>,
    stop_reason: Option<String>,
}

impl AnthropicSseParser {
    fn parse_data(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        let usage = event
            .pointer("/message/usage")
            .or_else(|| event.get("usage"));
        if let Some(usage) = usage {
            if let Some(StreamEvent::Usage {
                input_tokens,
                output_tokens,
            }) = token_usage(usage)
            {
                if usage.get("input_tokens").is_some() {
                    self.input_tokens = input_tokens;
                }
                if usage.get("output_tokens").is_some() {
                    self.output_tokens = output_tokens;
                }
                on_event(StreamEvent::Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                })?;
            }
        }
        if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.into());
        }
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    return Ok(());
                };
                let Some(block) = event.get("content_block") else {
                    return Ok(());
                };
                if block.get("type").and_then(Value::as_str) == Some("thinking") {
                    if let Some(text) = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        on_event(StreamEvent::Thinking(text.to_string()))?;
                    }
                }
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        return Ok(());
                    };
                    let Some(name) = block.get("name").and_then(Value::as_str) else {
                        return Ok(());
                    };
                    let arguments = block
                        .get("input")
                        .filter(|input| !input.is_null())
                        .filter(|input| input.as_object().is_none_or(|object| !object.is_empty()))
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    self.pending_tool_calls.insert(
                        index,
                        PendingAnthropicToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments,
                        },
                    );
                }
            }
            Some("content_block_delta") => {
                let Some(delta) = event.get("delta") else {
                    return Ok(());
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                on_event(StreamEvent::Text(text.to_string()))?;
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            on_event(StreamEvent::Thinking(text.to_string()))?;
                        }
                    }
                    Some("input_json_delta") => {
                        let Some(index) = event.get("index").and_then(Value::as_u64) else {
                            return Ok(());
                        };
                        let Some(partial_json) = delta.get("partial_json").and_then(Value::as_str)
                        else {
                            return Ok(());
                        };
                        if let Some(tool_call) = self.pending_tool_calls.get_mut(&index) {
                            tool_call.arguments.push_str(partial_json);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    return Ok(());
                };
                let Some(tool_call) = self.pending_tool_calls.remove(&index) else {
                    return Ok(());
                };
                on_event(StreamEvent::ToolCall {
                    id: tool_call.id,
                    name: tool_call.name,
                    arguments: if tool_call.arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        tool_call.arguments
                    },
                })?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn parse_anthropic_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    let mut events = Vec::new();
    let mut parser = AnthropicSseParser::default();
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            parser
                .parse_data(data, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .ok()?;
        }
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn bearer_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}"))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Upstream `formatUserAgent()` (`packages/ai/src/utils/identity.ts`) in
/// its default detailed form: `pi/<pi-ai version> (<platform>; node/<node
/// version>; <arch>)`. The version and runtime are pinned to the TS
/// parity reference (`packages/ai/src/utils/version.ts` and the reference
/// image's Node), because they are properties of upstream's build, not of
/// the host running noel; fixtures carry the same values. No kernel release
/// appears anymore, so nothing is host-specific in the detailed form.
fn pi_user_agent() -> &'static str {
    static PI_USER_AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PI_USER_AGENT.get_or_init(|| {
        format_pi_user_agent(node_platform(env::consts::OS), node_arch(env::consts::ARCH))
    })
}

/// The `@earendil-works/pi-ai` version of the TS parity reference, which
/// upstream embeds in the detailed `User-Agent`.
const REFERENCE_PI_AI_VERSION: &str = "1.5.0";
/// The Node version of the TS parity reference image, which upstream
/// embeds as the runtime in the detailed `User-Agent`.
const REFERENCE_NODE_VERSION: &str = "v24.22.2";

fn format_pi_user_agent(platform: &str, arch: &str) -> String {
    format!("pi/{REFERENCE_PI_AI_VERSION} ({platform}; node/{REFERENCE_NODE_VERSION}; {arch})")
}

/// Node's `os.platform()` name for a Rust target OS.
fn node_platform(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" | "illumos" => "sunos",
        other => other,
    }
}

/// Node's `os.arch()` name for a Rust target architecture.
fn node_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "loongarch64" => "loong64",
        "powerpc" => "ppc",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// Node's `os.release()`: the kernel release `uname -r` reports, limited to
/// header-safe characters, or "unknown".
pub fn os_release() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .or_else(|| {
            std::process::Command::new("uname")
                .arg("-r")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
        })
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(char::is_ascii_graphic)
        .collect::<String>();
    if release.is_empty() {
        "unknown".to_string()
    } else {
        release
    }
}

fn openai_compatible_headers(
    config: &ProviderConfig,
    api_key: &str,
    has_media: bool,
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    if config.model.provider == "cloudflare-ai-gateway" {
        headers.insert(
            "cf-aig-authorization",
            HeaderValue::from_str(&format!("Bearer {api_key}"))?,
        );
        if let Some(session_id) = &config.session_id {
            headers.insert("session_id", HeaderValue::from_str(session_id)?);
            headers.insert("x-client-request-id", HeaderValue::from_str(session_id)?);
            headers.insert("x-session-affinity", HeaderValue::from_str(session_id)?);
        }
    } else {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))?,
        );
    }
    if config.model.provider == "github-copilot" {
        headers.insert(
            "User-Agent",
            HeaderValue::from_static("GitHubCopilotChat/0.35.0"),
        );
        headers.insert("Editor-Version", HeaderValue::from_static("vscode/1.107.0"));
        headers.insert(
            "Editor-Plugin-Version",
            HeaderValue::from_static("copilot-chat/0.35.0"),
        );
        headers.insert(
            "Copilot-Integration-Id",
            HeaderValue::from_static("vscode-chat"),
        );
        headers.insert("X-Initiator", HeaderValue::from_static("user"));
        headers.insert(
            "Openai-Intent",
            HeaderValue::from_static("conversation-edits"),
        );
        if has_media {
            headers.insert("Copilot-Vision-Request", HeaderValue::from_static("true"));
        }
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(headers)
}

async fn error_for_status_with_body(
    response: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Err(ProviderError::Status {
        status,
        body,
        retry_after,
    })
}

/// How long an answer asks the client to wait, read as upstream's Codex
/// provider reads it: `retry-after-ms`, else `retry-after` in seconds. A
/// date in `retry-after` is not read.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let (value, unit) = match header("retry-after-ms") {
        Some(millis) => (millis, 1000.0),
        None => (header("retry-after")?, 1.0),
    };
    let seconds = value.parse::<f64>().ok()? / unit;
    Duration::try_from_secs_f64(seconds.max(0.0)).ok()
}

struct SseStreamResult {
    body: String,
    saw_data: bool,
}

async fn stream_sse_response(
    mut response: reqwest::Response,
    mut on_data: impl FnMut(&str) -> Result<(), ProviderError>,
) -> Result<SseStreamResult, ProviderError> {
    let mut pending = Vec::new();
    let mut body = Vec::new();
    let mut saw_data = false;
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        pending.extend_from_slice(&chunk);
        process_complete_sse_blocks(&mut pending, &mut saw_data, &mut on_data)?;
    }
    process_remaining_sse_block(&mut pending, &mut saw_data, &mut on_data)?;
    let body = String::from_utf8(body)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(SseStreamResult { body, saw_data })
}

fn process_complete_sse_blocks(
    pending: &mut Vec<u8>,
    saw_data: &mut bool,
    on_data: &mut impl FnMut(&str) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    while let Some((end, delimiter_len)) = find_sse_delimiter(pending) {
        let block = pending.drain(..end).collect::<Vec<_>>();
        pending.drain(..delimiter_len);
        process_sse_block(&block, saw_data, on_data)?;
    }
    Ok(())
}

fn process_remaining_sse_block(
    pending: &mut Vec<u8>,
    saw_data: &mut bool,
    on_data: &mut impl FnMut(&str) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    if pending.is_empty() {
        return Ok(());
    }
    let block = std::mem::take(pending);
    process_sse_block(&block, saw_data, on_data)
}

fn process_sse_block(
    block: &[u8],
    saw_data: &mut bool,
    on_data: &mut impl FnMut(&str) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    let block = String::from_utf8(block.to_vec())
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    let Some(data) = sse_block_data(&block) else {
        return Ok(());
    };
    *saw_data = true;
    if data == "[DONE]" {
        return Ok(());
    }
    on_data(&data)
}

fn sse_block_data(block: &str) -> Option<String> {
    let mut parts = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        parts.push(data.strip_prefix(' ').unwrap_or(data));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subsequence(buffer, b"\n\n").map(|index| (index, 2));
    let crlf = find_subsequence(buffer, b"\r\n\r\n").map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

fn find_subsequence(buffer: &[u8], needle: &[u8]) -> Option<usize> {
    buffer
        .windows(needle.len())
        .position(|window| window == needle)
}

fn emit_events(
    events: Vec<StreamEvent>,
    on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
) -> Result<(), ProviderError> {
    for event in events {
        on_event(event)?;
    }
    Ok(())
}

fn request_has_media(request: &ProviderRequest) -> bool {
    request
        .messages
        .iter()
        .any(|message| !message.media.is_empty())
}

fn chatgpt_headers(auth: &ProviderAuth) -> Result<HeaderMap, ProviderError> {
    let ProviderAuth::ChatGptOAuth {
        access_token,
        account_id,
    } = auth
    else {
        return Err(ProviderError::MissingApiKey {
            provider: "openai".to_string(),
        });
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}"))?,
    );
    if let Some(account_id) = account_id {
        headers.insert("ChatGPT-Account-ID", HeaderValue::from_str(account_id)?);
    } else if let Some(account_id) = extract_chatgpt_account_id(access_token)? {
        headers.insert("ChatGPT-Account-ID", HeaderValue::from_str(&account_id)?);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    Ok(headers)
}

fn extract_chatgpt_account_id(token: &str) -> Result<Option<String>, ProviderError> {
    let parts = token.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Ok(None);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| ProviderError::Config("failed to extract ChatGPT account id".to_string()))?;
    let payload = serde_json::from_slice::<Value>(&payload)
        .map_err(|_| ProviderError::Config("failed to extract ChatGPT account id".to_string()))?;
    Ok(payload
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string))
}

fn codex_headers(auth: &ProviderAuth) -> Result<HeaderMap, ProviderError> {
    let mut headers = match auth {
        ProviderAuth::ApiKey(api_key) => bearer_headers(api_key)?,
        ProviderAuth::ChatGptOAuth { .. } => chatgpt_headers(auth)?,
        _ => {
            return Err(ProviderError::MissingApiKey {
                provider: "openai-codex".to_string(),
            });
        }
    };
    headers.insert(
        "OpenAI-Beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert("User-Agent", HeaderValue::from_static("pi"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    Ok(headers)
}

fn azure_openai_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert("api-key", HeaderValue::from_str(api_key)?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    Ok(headers)
}

fn codex_responses_url(base_url: &str) -> Result<String, ProviderError> {
    let normalized = resolve_env_placeholders(base_url)?
        .trim_end_matches('/')
        .to_string();
    if normalized.ends_with("/codex/responses") {
        Ok(normalized)
    } else if normalized.ends_with("/codex") {
        Ok(format!("{normalized}/responses"))
    } else {
        Ok(format!("{normalized}/codex/responses"))
    }
}

fn azure_openai_base_url(config_base_url: Option<&str>) -> Result<String, ProviderError> {
    let raw = config_base_url
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| env::var("AZURE_OPENAI_BASE_URL").ok())
        .or_else(|| {
            env::var("AZURE_OPENAI_RESOURCE_NAME")
                .ok()
                .map(|name| format!("https://{name}.openai.azure.com/openai/v1"))
        })
        .ok_or_else(|| {
            ProviderError::Config(
                "Azure OpenAI base URL is required; set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME"
                    .to_string(),
            )
        })?;
    let normalized = resolve_env_placeholders(&raw)?
        .trim_end_matches('/')
        .to_string();
    if (normalized.contains(".openai.azure.com")
        || normalized.contains(".cognitiveservices.azure.com"))
        && !normalized.ends_with("/openai/v1")
    {
        if normalized.ends_with("/openai") {
            Ok(format!("{normalized}/v1"))
        } else {
            Ok(format!("{normalized}/openai/v1"))
        }
    } else {
        Ok(normalized)
    }
}

fn azure_openai_responses_url(base_url: &str) -> String {
    let api_version = env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "v1".to_string());
    format!(
        "{}/responses?api-version={}",
        base_url.trim_end_matches('/'),
        api_version
    )
}

fn azure_deployment_name(model_id: &str) -> String {
    if let Ok(deployment) = env::var("AZURE_OPENAI_DEPLOYMENT_NAME") {
        if !deployment.trim().is_empty() {
            return deployment;
        }
    }
    if let Ok(map) = env::var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP") {
        for entry in map
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            if let Some((model, deployment)) = entry.split_once('=') {
                if model.trim() == model_id && !deployment.trim().is_empty() {
                    return deployment.trim().to_string();
                }
            }
        }
    }
    model_id.to_string()
}

fn google_vertex_url(config: &ProviderConfig) -> Result<String, ProviderError> {
    let project = env::var("GOOGLE_CLOUD_PROJECT")
        .or_else(|_| env::var("GCLOUD_PROJECT"))
        .map_err(|_| {
            ProviderError::Config(
                "GOOGLE_CLOUD_PROJECT or GCLOUD_PROJECT is required for google-vertex".to_string(),
            )
        })?;
    let location = env::var("GOOGLE_CLOUD_LOCATION").map_err(|_| {
        ProviderError::Config("GOOGLE_CLOUD_LOCATION is required for google-vertex".to_string())
    })?;
    // The catalog writes the location as `{location}`.
    let base_url = resolve_env_placeholders(
        &config
            .base_url
            .as_deref()
            .unwrap_or("https://{location}-aiplatform.googleapis.com")
            .replace("{location}", &location),
    )?;
    Ok(format!(
        "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
        base_url.trim_end_matches('/'),
        encode_path_segment(&project),
        encode_path_segment(&location),
        encode_path_segment(&config.model.id)
    ))
}

fn resolve_env_placeholders(value: &str) -> Result<String, ProviderError> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('}') else {
            return Err(ProviderError::Config(format!(
                "unclosed environment placeholder in {value}"
            )));
        };
        let name = &after_start[..end];
        if !name.chars().all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        }) {
            return Err(ProviderError::Config(format!(
                "invalid environment placeholder {{{name}}}"
            )));
        }
        let replacement = env::var(name)
            .map_err(|_| ProviderError::Config(format!("{name} is required but is not set")))?;
        output.push_str(&replacement);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn bedrock_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.role != ChatRole::System)
        .map(|message| {
            if message.role == ChatRole::Tool {
                return json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": message.tool_call_id,
                            "content": bedrock_content(message),
                        },
                    }],
                });
            }
            if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
                let mut content = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({ "text": message.content }));
                }
                content.extend(message.tool_calls.iter().filter_map(|tool_call| {
                    let input = serde_json::from_str::<Value>(&tool_call.arguments).ok()?;
                    Some(json!({
                        "toolUse": {
                            "toolUseId": tool_call.id,
                            "name": tool_call.name,
                            "input": input,
                        },
                    }))
                }));
                return json!({
                    "role": "assistant",
                    "content": content,
                });
            }
            json!({
                "role": if message.role == ChatRole::Assistant { "assistant" } else { "user" },
                "content": bedrock_content(message),
            })
        })
        .collect()
}

/// A message's text and images as Converse content blocks, for a user
/// message or inside a tool result.
fn bedrock_content(message: &ChatMessage) -> Vec<Value> {
    let mut content = vec![json!({ "text": message.content })];
    content.extend(message.media.iter().filter_map(|media| {
        let format = media.mime_type.strip_prefix("image/")?;
        Some(json!({
            "image": {
                "format": format,
                "source": {
                    "bytes": media.data_base64,
                },
            }
        }))
    }));
    content
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn openai_responses_input(request: &ProviderRequest) -> Vec<Value> {
    let mut input = Vec::new();
    for message in &request.messages {
        if message.role == ChatRole::Tool {
            let Some(call_id) = message.tool_call_id.as_deref() else {
                continue;
            };
            let (call_id, _item_id) = openai_responses_tool_call_ids(call_id);
            let output = openai_responses_tool_output(message);
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
            continue;
        }

        if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
            if !message.content.is_empty() {
                input.push(json!({
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": message.content,
                    }],
                }));
            }
            input.extend(message.tool_calls.iter().map(|tool_call| {
                let (call_id, item_id) = openai_responses_tool_call_ids(&tool_call.id);
                let mut item = json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": tool_call.name,
                    "arguments": tool_call.arguments,
                });
                if let Some(item_id) = item_id {
                    item["id"] = json!(item_id);
                }
                item
            }));
            continue;
        }

        input.push({
            let mut content = vec![json!({
                "type": if message.role == ChatRole::Assistant { "output_text" } else { "input_text" },
                "text": message.content,
            })];
            content.extend(message.media.iter().map(|media| {
                json!({
                    "type": "input_image",
                    "image_url": media_data_url(media),
                })
            }));
            json!({
                "role": role_name(&message.role),
                "content": content,
            })
        });
    }
    input
}

fn openai_responses_tool_output(message: &ChatMessage) -> Value {
    if message.media.is_empty() {
        return json!(message.content);
    }
    let mut output = Vec::new();
    if !message.content.is_empty() {
        output.push(json!({
            "type": "input_text",
            "text": message.content,
        }));
    }
    output.extend(message.media.iter().map(|media| {
        json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": media_data_url(media),
        })
    }));
    Value::Array(output)
}

fn openai_responses_tool_call_ids(id: &str) -> (String, Option<String>) {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return (normalize_openai_responses_id_part(id), None);
    };
    (
        normalize_openai_responses_id_part(call_id),
        Some(normalize_openai_responses_item_id(item_id)),
    )
}

fn normalize_openai_responses_item_id(id: &str) -> String {
    let normalized = normalize_openai_responses_id_part(id);
    if normalized.starts_with("fc_") && normalized.len() <= 64 && normalized.len() == id.len() {
        return normalized;
    }
    let hashed = format!("fc_{}", short_hash(id));
    if hashed.len() > 64 {
        hashed.chars().take(64).collect()
    } else {
        hashed
    }
}

fn normalize_openai_responses_id_part(id: &str) -> String {
    let mut normalized = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beefu32;
    let mut h2 = 0x41c6_ce57u32;
    for unit in value.encode_utf16() {
        let ch = u32::from(unit);
        h1 = (h1 ^ ch).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ ch).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", to_base36(h2), to_base36(h1))
}

fn to_base36(mut value: u32) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u8;
        let ch = if digit < 10 {
            char::from(b'0' + digit)
        } else {
            char::from(b'a' + digit - 10)
        };
        digits.push(ch);
        value /= 36;
    }
    digits.iter().rev().collect()
}

fn parse_openai_responses_events(response: &Value) -> Option<Vec<StreamEvent>> {
    if matches!(
        response.get("status").and_then(Value::as_str),
        Some("incomplete" | "failed" | "cancelled")
    ) {
        return None;
    }
    let mut events = Vec::new();
    let has_output_text = response
        .get("output_text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    if let Some(text) = has_output_text {
        events.push(StreamEvent::Text(text.to_string()));
    }
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if has_output_text.is_none() {
                if let Some(text) = parse_openai_responses_item_text(item) {
                    events.push(StreamEvent::Text(text));
                }
            }
            if let Some(tool_call) = parse_openai_responses_function_call(item) {
                events.push(tool_call);
            }
        }
    }
    if let Some(usage) = response_usage(response) {
        events.push(usage);
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

#[derive(Default)]
struct OpenAiResponsesSseParser {
    saw_text_delta: bool,
}

impl OpenAiResponsesSseParser {
    fn parse_data(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some("response.incomplete" | "response.failed" | "error")
        ) {
            return Err(ProviderError::InvalidResponse(format!(
                "incomplete provider response: {event}"
            )));
        }
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            if let Some(usage) = event.get("response").and_then(response_usage) {
                on_event(usage)?;
            }
        }
        match event.get("type").and_then(Value::as_str) {
            Some("response.reasoning_summary_text.delta")
            | Some("response.reasoning_text.delta") => {
                if let Some(delta) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    on_event(StreamEvent::Thinking(delta.to_string()))?;
                }
            }
            Some("response.output_text.delta") | Some("output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.saw_text_delta = true;
                        on_event(StreamEvent::Text(delta.to_string()))?;
                    }
                }
            }
            Some("response.output_item.done") | Some("output_item.done") => {
                if let Some(item) = event.get("item") {
                    if !self.saw_text_delta {
                        if let Some(text) = parse_openai_responses_item_text(item) {
                            on_event(StreamEvent::Text(text))?;
                        }
                    }
                    if let Some(tool_call) = parse_openai_responses_function_call(item) {
                        on_event(tool_call)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn parse_openai_responses_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    let mut events = Vec::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            if let Some(usage) = event.get("response").and_then(response_usage) {
                events.push(usage);
            }
        }
        match event.get("type").and_then(Value::as_str) {
            Some("response.reasoning_summary_text.delta")
            | Some("response.reasoning_text.delta") => {
                if let Some(delta) = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    events.push(StreamEvent::Thinking(delta.to_string()));
                }
            }
            Some("response.output_text.delta") | Some("output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Text(delta.to_string()));
                }
            }
            Some("response.output_item.done") | Some("output_item.done") => {
                if let Some(item) = event.get("item") {
                    if let Some(text) = parse_openai_responses_item_text(item) {
                        events.push(StreamEvent::Text(text));
                    }
                    if let Some(tool_call) = parse_openai_responses_function_call(item) {
                        events.push(tool_call);
                    }
                }
            }
            _ => {}
        }
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_openai_responses_function_call(item: &Value) -> Option<StreamEvent> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    Some(StreamEvent::ToolCall {
        id: item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)?
            .to_string(),
        name: item.get("name").and_then(Value::as_str)?.to_string(),
        arguments: item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string(),
    })
}

#[derive(Default)]
struct OpenAiChatCompletionsSseParser {
    tool_calls: Vec<(String, String, String)>,
    stop_reason: Option<String>,
}

impl OpenAiChatCompletionsSseParser {
    fn parse_data(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        if let Some(reason) = event
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            self.stop_reason = Some(reason.into());
        }
        if let Some(usage) = response_usage(&event) {
            on_event(usage)?;
        }
        let Some(delta) = event.pointer("/choices/0/delta") else {
            return Ok(());
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                on_event(StreamEvent::Text(text.to_string()))?;
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.tool_calls.len() <= index {
                    self.tool_calls
                        .push((String::new(), String::new(), String::new()));
                }
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    self.tool_calls[index].0.push_str(id);
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    self.tool_calls[index].1.push_str(name);
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    self.tool_calls[index].2.push_str(arguments);
                }
            }
        }
        Ok(())
    }

    fn finish(
        self,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        for (id, name, arguments) in self.tool_calls {
            if id.is_empty() || name.is_empty() {
                continue;
            }
            on_event(StreamEvent::ToolCall {
                id,
                name,
                arguments,
            })?;
        }
        Ok(())
    }
}

fn parse_openai_chat_completions_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    let mut events = Vec::new();
    let mut tool_calls: Vec<(String, String, String)> = Vec::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(usage) = response_usage(&event) {
            events.push(usage);
        }
        let Some(delta) = event.pointer("/choices/0/delta") else {
            continue;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            events.push(StreamEvent::Text(text.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while tool_calls.len() <= index {
                    tool_calls.push((String::new(), String::new(), String::new()));
                }
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    tool_calls[index].0.push_str(id);
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    tool_calls[index].1.push_str(name);
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    tool_calls[index].2.push_str(arguments);
                }
            }
        }
    }
    events.extend(tool_calls.into_iter().filter_map(|(id, name, arguments)| {
        if id.is_empty() || name.is_empty() {
            return None;
        }
        Some(StreamEvent::ToolCall {
            id,
            name,
            arguments,
        })
    }));
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_openai_chat_completions_events(response: &Value) -> Option<Vec<StreamEvent>> {
    let message = response.pointer("/choices/0/message")?;
    let mut events = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            events.push(StreamEvent::Text(text.to_string()));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
                continue;
            };
            events.push(StreamEvent::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
            });
        }
    }
    if let Some(usage) = response_usage(response) {
        events.push(usage);
    }
    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn parse_openai_responses_item_text(item: &Value) -> Option<String> {
    let text = item
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn anthropic_messages_with_cache_control(
    messages: &[ChatMessage],
    cache_last_user_message: bool,
) -> Vec<Value> {
    let mut output: Vec<Value> = messages
        .iter()
        .filter(|message| message.role != ChatRole::System)
        .map(|message| {
            if message.role == ChatRole::Tool {
                if let Some(tool_call_id) = message.tool_call_id.as_deref() {
                    return json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": anthropic_tool_result_content(message),
                        }],
                    });
                }
            }
            if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
                let mut content = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({
                        "type": "text",
                        "text": message.content,
                    }));
                }
                content.extend(message.tool_calls.iter().filter_map(|tool_call| {
                    let input = serde_json::from_str::<Value>(&tool_call.arguments).ok()?;
                    Some(json!({
                        "type": "tool_use",
                        "id": tool_call.id,
                        "name": tool_call.name,
                        "input": input,
                    }))
                }));
                return json!({
                    "role": "assistant",
                    "content": content,
                });
            }
            let content = if message.media.is_empty() {
                json!(message.content)
            } else {
                let mut parts = vec![json!({
                    "type": "text",
                    "text": message.content,
                })];
                parts.extend(message.media.iter().map(anthropic_image_block));
                Value::Array(parts)
            };
            json!({
                "role": if message.role == ChatRole::Assistant { "assistant" } else { "user" },
                "content": content,
            })
        })
        .collect();
    if cache_last_user_message {
        apply_anthropic_cache_control_to_last_user_message(&mut output);
    }
    output
}

fn anthropic_image_block(media: &MediaInput) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media.mime_type,
            "data": media.data_base64,
        },
    })
}

/// A tool result's content (upstream `convertContentBlocks`): its text, or
/// with images a text block followed by the images.
fn anthropic_tool_result_content(message: &ChatMessage) -> Value {
    if message.media.is_empty() {
        return json!(message.content);
    }
    let text = if message.content.is_empty() {
        "(see attached image)"
    } else {
        &message.content
    };
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    blocks.extend(message.media.iter().map(anthropic_image_block));
    Value::Array(blocks)
}

fn apply_anthropic_cache_control_to_last_user_message(messages: &mut [Value]) {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message["role"] == "user")
    else {
        return;
    };
    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(block) = content.last_mut() {
            if block["type"] == "text" || block["type"] == "image" {
                block["cache_control"] = json!({ "type": "ephemeral" });
            }
        }
        return;
    }
    let Some(text) = message.get("content").and_then(Value::as_str) else {
        return;
    };
    message["content"] = json!([anthropic_cached_text_block(text)]);
}

fn google_messages(config: &ProviderConfig, messages: &[ChatMessage]) -> Vec<Value> {
    let mut contents = Vec::new();
    for message in messages
        .iter()
        .filter(|message| message.role != ChatRole::System)
    {
        if message.role == ChatRole::Tool {
            let mut image_parts = message
                .media
                .iter()
                .map(google_inline_data_part)
                .collect::<Vec<_>>();
            let has_images = !image_parts.is_empty();
            let response_value = if !message.content.is_empty() {
                message.content.clone()
            } else if has_images {
                "(see attached image)".to_string()
            } else {
                String::new()
            };
            let mut function_response = json!({
                "name": message.tool_name.as_deref().unwrap_or("tool"),
                "response": {
                    "output": response_value,
                },
            });
            if has_images && google_supports_multimodal_function_response(&config.model.id) {
                function_response["parts"] = Value::Array(image_parts.clone());
            }
            if google_requires_tool_call_id(&config.model.id) {
                if let Some(tool_call_id) = message.tool_call_id.as_deref() {
                    function_response["id"] = json!(tool_call_id);
                }
            }
            let function_response_part = json!({ "functionResponse": function_response });
            let should_merge = contents
                .last()
                .and_then(|content: &Value| content.get("role"))
                .and_then(Value::as_str)
                == Some("user")
                && contents
                    .last()
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .any(|part| part.get("functionResponse").is_some())
                    })
                    .unwrap_or(false);
            if should_merge {
                if let Some(parts) = contents
                    .last_mut()
                    .and_then(|content| content.get_mut("parts"))
                    .and_then(Value::as_array_mut)
                {
                    parts.push(function_response_part);
                }
            } else {
                contents.push(json!({
                    "role": "user",
                    "parts": [function_response_part],
                }));
            }
            if has_images && !google_supports_multimodal_function_response(&config.model.id) {
                let mut parts = vec![json!({ "text": "Tool result image:" })];
                parts.append(&mut image_parts);
                contents.push(json!({
                    "role": "user",
                    "parts": parts,
                }));
            }
            continue;
        }
        if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
            let mut parts = Vec::new();
            if !message.content.is_empty() {
                parts.push(json!({ "text": message.content }));
            }
            parts.extend(message.tool_calls.iter().filter_map(|tool_call| {
                let args = serde_json::from_str::<Value>(&tool_call.arguments).ok()?;
                let mut function_call = json!({
                    "name": tool_call.name,
                    "args": args,
                });
                if google_requires_tool_call_id(&config.model.id) {
                    function_call["id"] = json!(tool_call.id);
                }
                Some(json!({ "functionCall": function_call }))
            }));
            contents.push(json!({
                "role": "model",
                "parts": parts,
            }));
            continue;
        }
        let mut parts = vec![json!({ "text": message.content })];
        parts.extend(message.media.iter().map(google_inline_data_part));
        contents.push(json!({
            "role": if message.role == ChatRole::Assistant { "model" } else { "user" },
            "parts": parts,
        }));
    }
    contents
}

fn google_inline_data_part(media: &MediaInput) -> Value {
    json!({
        "inlineData": {
            "mimeType": media.mime_type,
            "data": media.data_base64,
        },
    })
}

fn google_requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || google_gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

fn google_supports_multimodal_function_response(model_id: &str) -> bool {
    google_gemini_major_version(model_id)
        .map(|major| major >= 3)
        .unwrap_or(true)
}

fn google_gemini_major_version(model_id: &str) -> Option<u64> {
    let lower = model_id.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("gemini-live-")
        .or_else(|| lower.strip_prefix("gemini-"))?;
    let version = rest.split('-').next()?;
    let major = version.split('.').next()?;
    major.parse().ok()
}

fn media_data_url(media: &MediaInput) -> String {
    format!("data:{};base64,{}", media.mime_type, media.data_base64)
}

fn role_name(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

#[cfg(test)]
mod tests;

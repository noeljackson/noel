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
        "request failed: {0}{timed_out}",
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
    "429",
    "500",
    "502",
    "503",
    "504",
    "520",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?error",
    "exceeded request buffer limit while retrying upstream",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
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
        .map(|pattern| format!("(?:{pattern})"))
        .collect::<Vec<_>>();
    RegexBuilder::new(&alternatives.join("|"))
        .case_insensitive(true)
        .build()
        .expect("error patterns are valid regular expressions")
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
    messages.extend(request.messages.iter().map(|message| {
        if message.role == ChatRole::Tool {
            return json!({
                "role": "tool",
                "tool_call_id": message.tool_call_id.as_deref().map(|id| normalize_openai_chat_tool_call_id(config, id)),
                "content": tool_result_text_without_images(message),
            });
        }
        if message.role == ChatRole::Assistant && !message.tool_calls.is_empty() {
            return json!({
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
            });
        }
        let content = if message.media.is_empty() {
            json!(message.content)
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
        json!({
            "role": role_name(&message.role),
            "content": content,
        })
    }));
    messages
}

/// What a tool result's images are replaced by where they cannot be sent.
const TOOL_IMAGE_OMITTED: &str =
    "(tool image omitted: this provider API does not accept images in tool results)";

/// A tool result for the chat completions API, whose tool messages are text
/// only. Upstream sends the images in a user message that follows, for
/// models its catalog lists as accepting images; noel's catalog does not say
/// which models do, so the images are left out and the model is told.
fn tool_result_text_without_images(message: &ChatMessage) -> String {
    if message.media.is_empty() {
        message.content.clone()
    } else if message.content.is_empty() {
        TOOL_IMAGE_OMITTED.to_string()
    } else {
        format!("{}\n{TOOL_IMAGE_OMITTED}", message.content)
    }
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

fn openai_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    if config.model.provider == "openai-codex" {
        return openai_codex_responses_body(config, request);
    }
    if config.model.provider == "github-copilot" {
        return github_copilot_responses_body(config, request);
    }
    let mut body = json!({
        "model": config.model.id,
        "input": openai_responses_input_with_developer_system(request),
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if !request.tools.is_empty() {
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
    let mut body = json!({
        "model": config.model.id,
        "input": openai_responses_input_with_developer_system(request),
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
    body
}

fn openai_codex_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
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
        "input": openai_responses_input(request),
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
    if !request.tools.is_empty() {
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

#[cfg(test)]
fn parse_google_sse_text(body: &str) -> Option<String> {
    let events = parse_google_sse_events(body)?;
    let text = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
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

/// Upstream `getPiUserAgent()` (`packages/ai/src/utils/pi-user-agent.ts`):
/// `pi (<platform> <release>; <arch>)` using Node's `os` names.
fn pi_user_agent() -> &'static str {
    static PI_USER_AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PI_USER_AGENT.get_or_init(|| {
        format_pi_user_agent(
            node_platform(env::consts::OS),
            &os_release(),
            node_arch(env::consts::ARCH),
        )
    })
}

fn format_pi_user_agent(platform: &str, release: &str, arch: &str) -> String {
    format!("pi ({platform} {release}; {arch})")
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

#[cfg(test)]
fn openai_messages(request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.extend(request.messages.iter().map(|message| {
        let content = if message.media.is_empty() {
            json!(message.content)
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
        json!({
            "role": role_name(&message.role),
            "content": content,
        })
    }));
    messages
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

#[cfg(test)]
fn parse_openai_responses_text(response: &Value) -> Option<String> {
    if let Some(text) = response.get("output_text").and_then(Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let text = response
        .get("output")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
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

#[cfg(test)]
fn parse_openai_responses_sse_text(body: &str) -> Option<String> {
    let mut text = String::new();
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
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") | Some("output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            Some("response.output_item.done") | Some("output_item.done") if text.is_empty() => {
                if let Some(item_text) =
                    event.get("item").and_then(parse_openai_responses_item_text)
                {
                    text.push_str(&item_text);
                }
            }
            _ => {}
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
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

#[cfg(test)]
fn parse_openai_chat_completions_sse_text(body: &str) -> Option<String> {
    let mut text = String::new();
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
        let Some(delta) = event
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        else {
            continue;
        };
        text.push_str(delta);
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
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

#[cfg(test)]
fn anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    anthropic_messages_with_cache_control(messages, false)
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
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn request_local_output_budget_overrides_provider_defaults_without_changing_regular_requests() {
        let request = ProviderRequest {
            max_output_tokens: Some(512),
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "openai".into(),
                id: "gpt-5.4".into(),
            },
            api: ProviderApi::OpenAi,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits {
                context_window: Some(272_000),
                max_tokens: Some(128_000),
                ..ModelTraits::default()
            },
        };
        assert_eq!(
            openai_chat_completions_body(&config, &request)["max_tokens"],
            512
        );
        assert_eq!(
            openai_responses_body(&config, &request)["max_output_tokens"],
            512
        );
        assert_eq!(anthropic_body(&config, &request)["max_tokens"], 512);
        assert_eq!(
            google_body(&config, &request)["generationConfig"]["maxOutputTokens"],
            512
        );
        assert_eq!(mistral_chat_body(&config, &request)["max_tokens"], 512);
        assert_eq!(
            bedrock_body(&config, &request)["inferenceConfig"]["maxTokens"],
            512
        );
        let regular = ProviderRequest {
            max_output_tokens: None,
            ..request
        };
        assert_eq!(provider_max_tokens(&config, &regular), Some(128_000));
    }

    #[test]
    fn provider_max_tokens_leaves_room_for_the_input() {
        let config = ProviderConfig {
            model: ModelRef {
                provider: "cloudflare-ai-gateway".to_string(),
                id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits {
                context_window: Some(262_144),
                max_tokens: Some(256_000),
                ..ModelTraits::default()
            },
        };
        let request = |content: &str| ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: content.to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        // Small input: the model's cap.
        assert_eq!(
            provider_max_tokens(&config, &request("hello")),
            Some(256_000)
        );
        // No known window: the cap as it is. No known cap: none is sent.
        let mut unknown = config.clone();
        unknown.traits.context_window = None;
        assert_eq!(
            provider_max_tokens(&unknown, &request("hello")),
            Some(256_000)
        );
        unknown.traits.max_tokens = None;
        assert_eq!(provider_max_tokens(&unknown, &request("hello")), None);
        // 300k characters estimate as 85_715 tokens; with the 4 for the system
        // prompt and the 4096 safety margin, 172_329 remain for the answer.
        let long = "x".repeat(300_000);
        assert_eq!(provider_max_tokens(&config, &request(&long)), Some(172_329));
        // An input past the window still asks for one token, as upstream does.
        let huge = "x".repeat(1_000_000);
        assert_eq!(provider_max_tokens(&config, &request(&huge)), Some(1));
    }

    #[test]
    fn streaming_parsers_preserve_truncation_instead_of_accepting_partial_checkpoints() {
        let mut chat = OpenAiChatCompletionsSseParser::default();
        chat.parse_data(
            r#"{"choices":[{"finish_reason":"length","delta":{}}]}"#,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(chat.stop_reason.as_deref(), Some("length"));
        let mut anthropic = AnthropicSseParser::default();
        anthropic
            .parse_data(
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
                &mut |_| Ok(()),
            )
            .unwrap();
        assert_eq!(anthropic.stop_reason.as_deref(), Some("max_tokens"));
        let mut google = Vec::new();
        parse_google_sse_data(
            r#"{"candidates":[{"finishReason":"MAX_TOKENS"}]}"#,
            &mut |event| {
                google.push(event);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            google,
            [StreamEvent::Stop {
                reason: "max_tokens".into()
            }]
        );
        for status in ["incomplete", "failed", "cancelled"] {
            assert!(parse_openai_responses_events(
                &json!({"status":status,"output_text":"partial"})
            )
            .is_none());
        }
        for kind in ["response.incomplete", "response.failed", "error"] {
            assert!(OpenAiResponsesSseParser::default()
                .parse_data(&json!({"type":kind}).to_string(), &mut |_| Ok(()))
                .is_err());
        }
    }

    #[test]
    fn output_limit_stop_reasons_cover_every_provider_spelling() {
        for reason in [
            "max_tokens",
            "MAX_TOKENS",
            "length",
            "max_output_tokens",
            "model_context_window_exceeded",
        ] {
            assert!(is_output_limit_stop(reason), "{reason}");
        }
        for reason in [
            "stop",
            "STOP",
            "end_turn",
            "tool_use",
            "toolUse",
            "completed",
        ] {
            assert!(!is_output_limit_stop(reason), "{reason}");
        }
    }

    /// Accepts one connection per reply, reads the request, writes the reply
    /// (possibly nothing), and then keeps the connection open without
    /// sending anything else.
    fn spawn_stalling_server(replies: Vec<&'static str>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let url = format!("http://{}", listener.local_addr().expect("stub addr"));
        std::thread::spawn(move || {
            let mut stalled = Vec::new();
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
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
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                let _ = stream.write_all(reply.as_bytes());
                stalled.push(stream);
            }
            std::thread::sleep(Duration::from_secs(30));
        });
        url
    }

    #[tokio::test]
    async fn stalled_provider_requests_fail_after_the_idle_timeout() {
        let url = spawn_stalling_server(vec![
            // No response headers at all.
            "",
            // Headers and one event, then nothing.
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        ]);
        let provider = create_provider_with_client(
            ProviderConfig {
                model: ModelRef {
                    provider: "local".to_string(),
                    id: "test".to_string(),
                },
                api: ProviderApi::OpenAi,
                base_url: Some(url),
                auth: ProviderAuth::ApiKey("test-key".to_string()),
                thinking_level: None,
                thinking_budget_tokens: None,
                session_id: None,
                traits: ModelTraits::default(),
            },
            provider_http_client_with_idle_timeout(Duration::from_millis(300)),
        );

        for expected_texts in [vec![], vec!["partial".to_string()]] {
            let mut texts = Vec::new();
            let result = tokio::time::timeout(
                Duration::from_secs(20),
                provider.complete_streaming(uncapped_request("system"), &mut |event| {
                    if let StreamEvent::Text(text) = event {
                        texts.push(text);
                    }
                    Ok(())
                }),
            )
            .await
            .expect("the idle timeout must end a stalled request");
            let error = result.expect_err("stalled request");
            assert!(
                matches!(&error, ProviderError::Request(source) if source.is_timeout()),
                "{error:?}"
            );
            assert!(error.to_string().ends_with("(timed out)"), "{error}");
            assert_eq!(texts, expected_texts);
        }
        assert_eq!(PROVIDER_IDLE_TIMEOUT, Duration::from_secs(300));
    }

    #[tokio::test]
    async fn faux_provider_echoes_last_user_message() {
        let provider = create_provider(ProviderConfig {
            model: ModelRef {
                provider: "faux".to_string(),
                id: "echo".to_string(),
            },
            api: ProviderApi::Faux,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });

        let events = provider
            .complete(ProviderRequest {
                max_output_tokens: None,
                system_prompt: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: "hello".to_string(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                }],
                tools: Vec::new(),
            })
            .await
            .expect("faux provider should complete");

        assert_eq!(events[0], StreamEvent::Text("[faux/echo] ".to_string()));
        assert_eq!(events[1], StreamEvent::Text("hello".to_string()));
    }

    fn faux_script_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "noel-faux-script-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user_request(text: &str) -> ProviderRequest {
        ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("system".into()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: text.into(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object"}),
            }],
        }
    }

    async fn run_faux_script(
        script: &FauxScript,
        request: &ProviderRequest,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut events = Vec::new();
        script
            .respond(request, &mut |event| {
                events.push(event);
                Ok(())
            })
            .await
            .map(|()| events)
    }

    #[tokio::test]
    async fn faux_script_replays_responses_and_logs_each_request() {
        let dir = faux_script_dir("replay");
        fs::write(
            dir.join("script.json"),
            r#"[
                [{"thinking": "plan"}, {"tool_call": {"name": "read", "arguments": {"path": "a.txt"}}}],
                [{"text": "done"}, {"usage": {"input": 3, "output": 2}}, {"stop": "end_turn"}]
            ]"#,
        )
        .unwrap();
        let script = FauxScript {
            path: dir.join("script.json"),
            log: Some(dir.join("requests.jsonl")),
        };

        let first = run_faux_script(&script, &user_request("read a.txt"))
            .await
            .unwrap();
        assert_eq!(
            first,
            [
                StreamEvent::Thinking("plan".into()),
                StreamEvent::ToolCall {
                    id: "call_1_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                },
                StreamEvent::Stop {
                    reason: "tool_use".into()
                },
            ]
        );

        let mut second_request = user_request("read a.txt");
        second_request.messages.extend([
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_1_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "alpha".into(),
                media: Vec::new(),
                tool_call_id: Some("call_1_1".into()),
                tool_name: Some("read".into()),
                tool_calls: Vec::new(),
            },
        ]);
        let second = run_faux_script(&script, &second_request).await.unwrap();
        assert_eq!(
            second,
            [
                StreamEvent::Text("done".into()),
                StreamEvent::Usage {
                    input_tokens: 3,
                    output_tokens: 2
                },
                StreamEvent::Stop {
                    reason: "end_turn".into()
                },
            ]
        );

        let log = fs::read_to_string(dir.join("requests.jsonl")).unwrap();
        let entries = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1],
            json!({
                "system_prompt": "system",
                "messages": [
                    {"role": "user", "content": "read a.txt"},
                    {"role": "assistant", "content": "", "tool_calls": [
                        {"id": "call_1_1", "name": "read", "arguments": {"path": "a.txt"}}
                    ]},
                    {"role": "tool", "content": "alpha", "tool_call_id": "call_1_1", "tool_name": "read"}
                ],
                "tools": ["read"]
            })
        );

        let exhausted = run_faux_script(&script, &user_request("more"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            exhausted.contains("is exhausted: request 3 has no response (the script has 2)"),
            "{exhausted}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn faux_script_log_position_survives_a_new_provider() {
        let dir = faux_script_dir("resume");
        fs::write(
            dir.join("script.json"),
            r#"[[{"text": "one"}], [{"text": "two"}], [{"text": "three"}]]"#,
        )
        .unwrap();
        let script = FauxScript {
            path: dir.join("script.json"),
            log: Some(dir.join("requests.jsonl")),
        };
        run_faux_script(&script, &user_request("a")).await.unwrap();
        run_faux_script(&script, &user_request("b")).await.unwrap();

        // A restarted process holds no state beyond the log.
        let restarted = script.clone();
        let events = run_faux_script(&restarted, &user_request("c"))
            .await
            .unwrap();
        assert_eq!(events[0], StreamEvent::Text("three".into()));
        assert_eq!(
            fs::read_to_string(dir.join("requests.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn faux_script_without_log_counts_requests_per_script() {
        let dir = faux_script_dir("counter");
        for name in ["a.json", "b.json"] {
            fs::write(
                dir.join(name),
                format!(r#"[[{{"text": "{name} 1"}}], [{{"text": "{name} 2"}}]]"#),
            )
            .unwrap();
        }
        let a = FauxScript {
            path: dir.join("a.json"),
            log: None,
        };
        let b = FauxScript {
            path: dir.join("b.json"),
            log: None,
        };
        let request = user_request("hi");
        assert_eq!(
            run_faux_script(&a, &request).await.unwrap()[0],
            StreamEvent::Text("a.json 1".into())
        );
        assert_eq!(
            run_faux_script(&b, &request).await.unwrap()[0],
            StreamEvent::Text("b.json 1".into())
        );
        assert_eq!(
            run_faux_script(&a, &request).await.unwrap()[0],
            StreamEvent::Text("a.json 2".into())
        );
        assert!(run_faux_script(&a, &request).await.is_err());
        assert!(!dir.join("requests.jsonl").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn faux_script_error_fails_before_output_and_the_retry_gets_the_next_response() {
        let dir = faux_script_dir("error");
        fs::write(
            dir.join("script.json"),
            r#"[[{"error": "overloaded"}], [{"text": "recovered"}]]"#,
        )
        .unwrap();
        let script = FauxScript {
            path: dir.join("script.json"),
            log: Some(dir.join("requests.jsonl")),
        };
        let mut events = Vec::new();
        let error = script
            .respond(&user_request("hi"), &mut |event| {
                events.push(event);
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(events.is_empty());
        assert!(error.to_string().contains("overloaded"), "{error}");
        assert_eq!(
            run_faux_script(&script, &user_request("hi")).await.unwrap()[0],
            StreamEvent::Text("recovered".into())
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn faux_script_error_can_name_its_status_and_follow_output() {
        let dir = faux_script_dir("status-error");
        fs::write(
            dir.join("script.json"),
            r#"[
                [{"error": {"status": 401, "body": "bad key"}}],
                [{"text": "Hel"}, {"error": "terminated"}],
                [{"error": {"status": 99, "body": ""}}]
            ]"#,
        )
        .unwrap();
        let script = FauxScript {
            path: dir.join("script.json"),
            log: None,
        };
        let request = user_request("hi");
        let error = run_faux_script(&script, &request).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "request failed with status 401 Unauthorized: bad key"
        );
        assert!(!error.is_retryable());
        // An error after other steps breaks the reply off where it is.
        let mut events = Vec::new();
        let error = script
            .respond(&request, &mut |event| {
                events.push(event);
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(events, [StreamEvent::Text("Hel".into())]);
        assert_eq!(
            error.to_string(),
            "request failed with status 503 Service Unavailable: terminated"
        );
        assert!(error.is_retryable());
        let error = run_faux_script(&script, &request).await.unwrap_err();
        assert!(
            error.to_string().contains("99 is not an HTTP status"),
            "{error}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn error_classification_matches_ts_patterns_and_verdicts() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/error-classification.json"
        ))
        .unwrap();
        let patterns = |name: &str| -> Vec<String> {
            let patterns = fixture["patterns"][name].as_array().unwrap();
            let sources = patterns.iter().map(|pattern| pattern.as_str().unwrap());
            sources.map(str::to_owned).collect()
        };
        assert_eq!(RETRYABLE_ERROR_PATTERNS, patterns("retryable"));
        assert_eq!(NON_RETRYABLE_ERROR_PATTERNS, patterns("nonRetryable"));
        assert_eq!(OVERFLOW_PATTERNS, patterns("overflow"));
        assert_eq!(NON_OVERFLOW_PATTERNS, patterns("nonOverflow"));

        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() > 100, "{} cases", cases.len());
        for case in cases {
            let message = case["message"].as_str().unwrap();
            assert_eq!(
                is_retryable_error_text(message),
                case["retryable"].as_bool().unwrap(),
                "retryable: {message:?}"
            );
            assert_eq!(
                is_context_overflow_text(message),
                case["overflow"].as_bool().unwrap(),
                "overflow: {message:?}"
            );
        }
    }

    #[test]
    fn provider_errors_are_retryable_by_kind_and_text() {
        let status = |status: u16, body: &str| ProviderError::Status {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            body: body.to_string(),
            retry_after: None,
        };
        let invalid = |text: &str| ProviderError::InvalidResponse(text.to_string());
        for (error, retryable) in [
            (status(429, "slow down"), true),
            (status(500, ""), true),
            (status(502, ""), true),
            (status(503, "overloaded"), true),
            (status(504, ""), true),
            (
                status(529, r#"{"error":{"type":"overloaded_error"}}"#),
                true,
            ),
            (status(408, ""), true),
            (invalid("stream ended before message_stop"), true),
            (
                status(400, r#"{"error":{"message":"bad parameter"}}"#),
                false,
            ),
            (status(401, r#"{"error":{"message":"bad key"}}"#), false),
            (status(403, ""), false),
            (
                status(404, r#"{"error":{"message":"no such model"}}"#),
                false,
            ),
            (status(422, ""), false),
            // A quota or billing limit, though the status says rate limit.
            (
                status(429, r#"{"error":{"code":"insufficient_quota"}}"#),
                false,
            ),
            (invalid("{}"), false),
            (ProviderError::Config("no credentials".into()), false),
            // noel's own errors are never the provider's answer, whatever
            // their text happens to contain.
            (
                ProviderError::Config("faux script /tmp/run-503/script.json is exhausted".into()),
                false,
            ),
            (
                ProviderError::MissingApiKey {
                    provider: "anthropic".into(),
                },
                false,
            ),
            // Compaction answers a context overflow, whatever else it says.
            (
                status(400, r#"{"error":{"code":"context_length_exceeded"}}"#),
                false,
            ),
            (
                status(413, r#"{"error":{"type":"request_too_large"}}"#),
                false,
            ),
            (
                status(503, "prompt is too long: 9 tokens > 8 maximum"),
                false,
            ),
        ] {
            assert_eq!(error.is_retryable(), retryable, "{error}");
        }
    }

    #[test]
    fn retry_after_reads_milliseconds_then_seconds() {
        let wait = |headers: &[(&'static str, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in headers {
                map.insert(*name, HeaderValue::from_str(value).unwrap());
            }
            retry_after(&map)
        };
        assert_eq!(wait(&[]), None);
        assert_eq!(
            wait(&[("retry-after", "30")]),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            wait(&[("retry-after", " 1.5 ")]),
            Some(Duration::from_millis(1500))
        );
        // Milliseconds win, as upstream reads them first.
        assert_eq!(
            wait(&[("retry-after-ms", "250"), ("retry-after", "30")]),
            Some(Duration::from_millis(250))
        );
        assert_eq!(wait(&[("retry-after", "-5")]), Some(Duration::ZERO));
        // A date, and values no wait can hold, are not read.
        assert_eq!(
            wait(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
            None
        );
        assert_eq!(wait(&[("retry-after", "inf")]), None);
        assert_eq!(
            wait(&[("retry-after-ms", "soon"), ("retry-after", "30")]),
            None
        );
    }

    #[test]
    fn context_overflow_is_read_from_the_error_text_and_cerebras_statuses() {
        let status = |status: u16, body: &str| ProviderError::Status {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            body: body.to_string(),
            retry_after: None,
        };
        let overflow = status(400, r#"{"error":{"code":"context_length_exceeded"}}"#);
        assert!(overflow.is_context_overflow("openai"));
        assert!(status(413, r#"{"error":{"type":"request_too_large"}}"#)
            .is_context_overflow("anthropic"));
        // Throttling that mentions tokens is not an overflow.
        let throttled = status(429, r#"{"message":"Too many tokens, please wait."}"#);
        assert!(!throttled.is_context_overflow("amazon-bedrock"));
        assert!(throttled.is_retryable());
        assert!(!status(400, "bad parameter").is_context_overflow("openai"));
        // Cerebras answers an overflow without a body.
        for code in [400, 413] {
            assert!(status(code, "").is_context_overflow("cerebras"));
            assert!(!status(code, "").is_context_overflow("openai"));
        }
        assert!(!status(400, "bad parameter").is_context_overflow("cerebras"));
        assert!(!status(500, "").is_context_overflow("cerebras"));
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_made_is_retryable() {
        // Nothing listens on the port, so the request fails in transport.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error = reqwest::Client::new().get(url).send().await.unwrap_err();
        // Without the URL: its port is random, and a port such as 35033
        // contains one of upstream's patterns ("503").
        let error = ProviderError::Request(error.without_url());
        // The text names no cause upstream's patterns know; the kind does.
        assert!(!is_retryable_error_text(&error.to_string()), "{error}");
        assert!(error.is_retryable());
        assert!(!error.is_context_overflow("openai"));
    }

    /// The limit a cut-off reply is compared with is the catalog's
    /// `maxTokens`, for every built-in model.
    #[test]
    fn output_limits_are_the_catalogs_for_every_built_in_model() {
        let models = catalog::builtin_models();
        assert!(models.len() > 1000, "{} models", models.len());
        for model in models {
            assert!(model.max_tokens > 0, "{}/{}", model.provider, model.id);
            assert_eq!(
                model_output_limit(&builtin_config(model)),
                Some(model.max_tokens),
                "{}/{}",
                model.provider,
                model.id
            );
        }

        // A model nothing is known about, as a `models.json` entry without
        // `maxTokens` is: the cap its Anthropic requests carry, else none.
        let unknown = |api: ProviderApi| ProviderConfig {
            model: ModelRef {
                provider: "local".to_string(),
                id: "llama".to_string(),
            },
            api,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        assert_eq!(
            model_output_limit(&unknown(ProviderApi::Anthropic)),
            Some(16_384)
        );
        assert_eq!(model_output_limit(&unknown(ProviderApi::OpenAi)), None);
        assert_eq!(
            model_output_limit(&ProviderConfig {
                traits: ModelTraits {
                    max_tokens: Some(4_096),
                    ..ModelTraits::default()
                },
                ..unknown(ProviderApi::OpenAi)
            }),
            Some(4_096)
        );
    }

    #[tokio::test]
    async fn faux_script_delays_and_holds_a_response_until_a_file_exists() {
        let dir = faux_script_dir("wait");
        fs::write(
            dir.join("script.json"),
            r#"[[{"delay_ms": 50}, {"text": "before"}, {"wait_for_file": "go"}, {"text": "after"}]]"#,
        )
        .unwrap();
        let script = FauxScript {
            path: dir.join("script.json"),
            log: None,
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let request = user_request("hi");
        let started = std::time::Instant::now();
        let run = async {
            script
                .respond(&request, &mut |event| {
                    sender.send(event).unwrap();
                    Ok(())
                })
                .await
        };
        let release = async {
            assert_eq!(
                receiver.recv().await,
                Some(StreamEvent::Text("before".into()))
            );
            assert!(started.elapsed() >= Duration::from_millis(50));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(receiver.try_recv().is_err(), "response ran past the wait");
            fs::write(dir.join("go"), "").unwrap();
        };
        let (result, ()) = tokio::join!(run, release);
        result.unwrap();
        assert_eq!(
            receiver.recv().await,
            Some(StreamEvent::Text("after".into()))
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn faux_script_reports_unreadable_and_invalid_scripts() {
        let dir = faux_script_dir("invalid");
        let missing = FauxScript {
            path: dir.join("missing.json"),
            log: None,
        };
        let error = run_faux_script(&missing, &user_request("hi"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing.json"), "{error}");

        fs::write(dir.join("script.json"), r#"[[{"txt": "typo"}]]"#).unwrap();
        let invalid = FauxScript {
            path: dir.join("script.json"),
            log: Some(dir.join("requests.jsonl")),
        };
        let error = run_faux_script(&invalid, &user_request("hi"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid faux script"), "{error}");
        assert!(error.contains("txt"), "{error}");
        assert!(!dir.join("requests.jsonl").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn usage_is_decoded_for_all_response_apis_and_streams() {
        let expected = StreamEvent::Usage {
            input_tokens: 12,
            output_tokens: 5,
        };
        let responses = [
            parse_openai_chat_completions_events(
                &json!({"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":12,"completion_tokens":5}}),
            ),
            parse_openai_responses_events(
                &json!({"output_text":"ok","usage":{"input_tokens":12,"output_tokens":5}}),
            ),
            parse_anthropic_response_events(
                &json!({"content":[],"usage":{"input_tokens":2,"cache_read_input_tokens":8,"cache_creation_input_tokens":2,"output_tokens":5}}),
            ),
            parse_google_response_events(
                &json!({"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3,"thoughtsTokenCount":2}}),
            ),
            parse_bedrock_response_events(
                &json!({"output":{"message":{"content":[]}},"usage":{"inputTokens":12,"outputTokens":5}}),
            ),
        ];
        for events in responses {
            assert_eq!(events.unwrap().last(), Some(&expected));
        }
        assert!(response_usage(&json!({"usage": null})).is_none());
        let chat = r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5}}"#;
        let responses = r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":5}}}"#;
        let google = r#"{"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5}}"#;
        for (data, buffered) in [
            (
                chat,
                parse_openai_chat_completions_sse_events(&format!("data: {chat}\n")),
            ),
            (
                responses,
                parse_openai_responses_sse_events(&format!("data: {responses}\n")),
            ),
            (
                google,
                parse_google_sse_events(&format!("data: {google}\n")),
            ),
        ] {
            assert_eq!(buffered.unwrap(), std::slice::from_ref(&expected));
            let mut events = Vec::new();
            let mut callback = |event| {
                events.push(event);
                Ok(())
            };
            if data == chat {
                OpenAiChatCompletionsSseParser::default()
                    .parse_data(data, &mut callback)
                    .unwrap();
            } else if data == responses {
                OpenAiResponsesSseParser::default()
                    .parse_data(data, &mut callback)
                    .unwrap();
            } else {
                parse_google_sse_data(data, &mut callback).unwrap();
            }
            assert_eq!(events, std::slice::from_ref(&expected));
        }
        let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":10,"output_tokens":1}}}"#;
        let delta = r#"{"type":"message_delta","usage":{"output_tokens":5}}"#;
        let body = format!("data: {start}\ndata: {delta}\n");
        let buffered = parse_anthropic_sse_events(&body).unwrap();
        assert_eq!(buffered.last(), Some(&expected));
        let mut parser = AnthropicSseParser::default();
        let mut events = Vec::new();
        for data in [start, delta] {
            parser
                .parse_data(data, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(events, buffered);
    }

    #[test]
    fn thinking_events_are_separate_from_answer_text() {
        let anthropic = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"first\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" second\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"private-signature\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
        );
        let expected = vec![
            StreamEvent::Thinking("first".into()),
            StreamEvent::Thinking(" second".into()),
            StreamEvent::Text("answer".into()),
        ];
        assert_eq!(
            parse_anthropic_sse_events(anthropic),
            Some(expected.clone())
        );
        let mut streamed = Vec::new();
        let mut parser = AnthropicSseParser::default();
        for line in anthropic
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
        {
            parser
                .parse_data(line, &mut |event| {
                    streamed.push(event);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(streamed, expected);

        let openai = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"first\"}\n\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\" second\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n",
        );
        assert_eq!(
            parse_openai_responses_sse_events(openai),
            Some(expected.clone())
        );
        let mut streamed = Vec::new();
        let mut parser = OpenAiResponsesSseParser::default();
        for line in openai
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
        {
            parser
                .parse_data(line, &mut |event| {
                    streamed.push(event);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(streamed, expected);
        assert_eq!(
            google_part_events(&[
                json!({"thought": true, "text": "first"}),
                json!({"text": "answer"})
            ]),
            Some(vec![
                StreamEvent::Thinking("first".into()),
                StreamEvent::Text("answer".into())
            ])
        );
    }

    #[test]
    fn chatgpt_oauth_headers_include_bearer_and_account_id() {
        let headers = chatgpt_headers(&ProviderAuth::ChatGptOAuth {
            access_token: "access-token".to_string(),
            account_id: Some("account-id".to_string()),
        })
        .expect("headers");

        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer access-token")
        );
        assert_eq!(
            headers
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("account-id")
        );
    }

    #[test]
    fn parses_openai_responses_text_shapes() {
        let direct = json!({ "output_text": "hello" });
        assert_eq!(
            parse_openai_responses_text(&direct),
            Some("hello".to_string())
        );

        let nested = json!({
            "output": [{
                "content": [
                    { "type": "output_text", "text": "hel" },
                    { "type": "output_text", "text": "lo" }
                ]
            }]
        });
        assert_eq!(
            parse_openai_responses_text(&nested),
            Some("hello".to_string())
        );
    }

    #[test]
    fn parses_openai_responses_sse_delta_text() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "data: [DONE]\n"
        );

        assert_eq!(
            parse_openai_responses_sse_text(body),
            Some("hello".to_string())
        );
    }

    #[test]
    fn parses_openai_responses_function_call_events() {
        let body = r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#;
        let events = parse_openai_responses_sse_events(body).expect("events");

        assert_eq!(
            events,
            vec![StreamEvent::ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }]
        );
    }

    #[test]
    fn parses_openai_chat_completions_sse_delta_text() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: [DONE]\n"
        );

        assert_eq!(
            parse_openai_chat_completions_sse_text(body),
            Some("hello".to_string())
        );
    }

    #[test]
    fn parses_openai_chat_completions_tool_call_events() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n"
        );

        assert_eq!(
            parse_openai_chat_completions_sse_events(body),
            Some(vec![StreamEvent::ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }])
        );
    }

    #[test]
    fn openai_chat_stream_parser_emits_text_before_finish() {
        let mut parser = OpenAiChatCompletionsSseParser::default();
        let mut events = Vec::new();
        parser
            .parse_data(
                r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("parse text");
        assert_eq!(events, [StreamEvent::Text("hel".to_string())]);

        parser
            .parse_data(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("parse tool start");
        parser
            .parse_data(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("parse tool end");
        assert_eq!(events, [StreamEvent::Text("hel".to_string())]);

        parser
            .finish(&mut |event| {
                events.push(event);
                Ok(())
            })
            .expect("finish");
        assert_eq!(
            events,
            [
                StreamEvent::Text("hel".to_string()),
                StreamEvent::ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }
            ]
        );
    }

    #[test]
    fn openai_responses_stream_parser_does_not_duplicate_done_text_after_delta() {
        let mut parser = OpenAiResponsesSseParser::default();
        let mut events = Vec::new();
        parser
            .parse_data(
                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("parse delta");
        parser
            .parse_data(
                r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"hello"}]}}"#,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("parse done");

        assert_eq!(events, [StreamEvent::Text("hello".to_string())]);
    }

    #[test]
    fn parses_google_sse_text_deltas() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"skip\"},{\"text\":\"lo\"}]}}]}\n\n",
        );

        assert_eq!(parse_google_sse_text(body), Some("hello".to_string()));
    }

    #[test]
    fn provider_message_converters_include_image_parts() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "describe".to_string(),
                media: vec![MediaInput {
                    mime_type: "image/png".to_string(),
                    data_base64: "aW1hZ2U=".to_string(),
                    path: Some("fixture.png".to_string()),
                    width: Some(1),
                    height: Some(1),
                }],
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };

        let openai = openai_messages(&request);
        assert_eq!(
            openai[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1hZ2U="
        );
        let anthropic = anthropic_messages(&request.messages);
        assert_eq!(
            anthropic[0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        let google_config = ProviderConfig {
            model: ModelRef {
                provider: "google".to_string(),
                id: "gemini-2.5-pro".to_string(),
            },
            api: ProviderApi::Google,
            base_url: None,
            auth: ProviderAuth::ApiKey("key".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        let google = google_messages(&google_config, &request.messages);
        assert_eq!(google[0]["parts"][1]["inlineData"]["mimeType"], "image/png");
    }

    #[test]
    fn openai_responses_input_preserves_tool_calls_and_results() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::User,
                    content: "use a tool".to_string(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                },
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: "call_1".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some("call_1".to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: Vec::new(),
        };

        let input = openai_responses_input(&request);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "read");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "file contents");
    }

    #[test]
    fn openai_responses_input_normalizes_pipe_separated_tool_call_ids() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/openai-responses-tool-id.json"
        ))
        .expect("parse TS parity fixture");
        let raw_id = fixture["conversion"]["rawToolCallId"]
            .as_str()
            .expect("raw tool call id");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: raw_id.to_string(),
                        name: "edit".to_string(),
                        arguments: r#"{"path":"src/styles/app.css"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "ok".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some(raw_id.to_string()),
                    tool_name: Some("edit".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: Vec::new(),
        };

        let input = openai_responses_input(&request);

        assert_eq!(input[0], fixture["conversion"]["functionCall"]);
        assert!(input[0]["id"].as_str().expect("id").len() <= 64);
        assert_eq!(input[1], fixture["conversion"]["toolOutput"]);
    }

    #[test]
    fn openai_responses_tool_result_keeps_images_inside_function_output() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![ChatMessage {
                role: ChatRole::Tool,
                content: "A red circle".to_string(),
                media: vec![MediaInput {
                    mime_type: "image/png".to_string(),
                    data_base64: "cmVkLWNpcmNsZQ==".to_string(),
                    path: Some("circle.png".to_string()),
                    width: Some(100),
                    height: Some(100),
                }],
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("get_image".to_string()),
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };

        let input = openai_responses_input(&request);

        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["output"][0]["type"], "input_text");
        assert_eq!(input[0]["output"][0]["text"], "A red circle");
        assert_eq!(input[0]["output"][1]["type"], "input_image");
        assert_eq!(input[0]["output"][1]["detail"], "auto");
        assert_eq!(
            input[0]["output"][1]["image_url"],
            "data:image/png;base64,cmVkLWNpcmNsZQ=="
        );
    }

    #[test]
    fn openai_chat_body_preserves_tools_calls_and_results() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: "call_1".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some("call_1".to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "openrouter".to_string(),
                id: "model".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let mut openai = config.clone();
        openai.model.provider = "openai".into();
        assert_eq!(
            openai_chat_completions_body(&openai, &request)["stream_options"]["include_usage"],
            true
        );
        let body = openai_chat_completions_body(&config, &request);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
    }

    #[test]
    fn openai_chat_body_normalizes_responses_tool_call_ids() {
        let raw_id = "call_1234567890abcdefghijklmnopqrstuvwxyzEXTRA|fc_foreign_item";
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: raw_id.to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some(raw_id.to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: Vec::new(),
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "openai".to_string(),
                id: "gpt-4o-mini".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: None,
            auth: ProviderAuth::ApiKey("sk".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let body = openai_chat_completions_body(&config, &request);

        assert_eq!(
            body["messages"][0]["tool_calls"][0]["id"],
            "call_1234567890abcdefghijklmnopqrstuvwxy"
        );
        assert_eq!(
            body["messages"][1]["tool_call_id"],
            "call_1234567890abcdefghijklmnopqrstuvwxy"
        );
    }

    #[test]
    fn anthropic_body_preserves_tools_calls_and_results() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: "toolu_1".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some("toolu_1".to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-sonnet-4-6".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let body = anthropic_body(&config, &request);
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(
            body["tools"][0]["input_schema"],
            request.tools[0].parameters
        );
        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][0]["content"][0]["id"], "toolu_1");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn parses_anthropic_tool_use_events() {
        let response = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "read",
                "input": { "path": "a.txt" }
            }],
            "stop_reason": "tool_use"
        });

        assert_eq!(
            parse_anthropic_response_events(&response),
            Some(vec![StreamEvent::ToolCall {
                id: "toolu_1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }])
        );
    }

    #[test]
    fn parses_anthropic_sse_tool_use_events() {
        let body = concat!(
            r#"event: message_start"#,
            "\n",
            r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#,
            "\n\n",
            r#"event: content_block_start"#,
            "\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"event: content_block_delta"#,
            "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reading "}}"#,
            "\n\n",
            r#"event: content_block_start"#,
            "\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            "\n\n",
            r#"event: content_block_delta"#,
            "\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}}"#,
            "\n\n",
            r#"event: content_block_delta"#,
            "\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":":\"a.txt\"}"}}"#,
            "\n\n",
            r#"event: content_block_stop"#,
            "\n",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "\n\n",
        );

        assert_eq!(
            parse_anthropic_sse_events(body),
            Some(vec![
                StreamEvent::Text("reading ".to_string()),
                StreamEvent::ToolCall {
                    id: "toolu_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }
            ])
        );
    }

    #[test]
    fn google_body_preserves_tools_calls_and_results() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: "read_0".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some("read_0".to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "google".to_string(),
                id: "gemini-2.5-pro".to_string(),
            },
            api: ProviderApi::Google,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let body = google_body(&config, &request);
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "read");
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["name"],
            "read"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "read"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["response"]["output"],
            "file contents"
        );
    }

    #[test]
    fn google_tool_result_images_follow_gemini_version_routing() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/google-tool-image-routing.json"
        ))
        .expect("parse TS parity fixture");
        let messages = vec![ChatMessage {
            role: ChatRole::Tool,
            content: "alpha text".to_string(),
            media: vec![MediaInput {
                mime_type: "image/png".to_string(),
                data_base64: "YWJj".to_string(),
                path: Some("image.png".to_string()),
                width: Some(1),
                height: Some(1),
            }],
            tool_call_id: Some("call_img".to_string()),
            tool_name: Some("read".to_string()),
            tool_calls: Vec::new(),
        }];
        let mut config = ProviderConfig {
            model: ModelRef {
                provider: "google".to_string(),
                id: "gemini-2.5-flash".to_string(),
            },
            api: ProviderApi::Google,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let gemini_2 = google_messages(&config, &messages);
        assert_eq!(json!(gemini_2), fixture["routing"]["gemini2"]);

        config.model.id = "gemini-3-pro-preview".to_string();
        let gemini_3 = google_messages(&config, &messages);
        assert_eq!(json!(gemini_3), fixture["routing"]["gemini3"]);
    }

    #[test]
    fn parses_google_function_call_events() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[",
            "{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a.txt\"}}}",
            "]}}]}\n\n"
        );

        assert_eq!(
            parse_google_sse_events(body),
            Some(vec![StreamEvent::ToolCall {
                id: "read_0".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }])
        );
    }

    #[test]
    fn mistral_body_preserves_tools_calls_and_results() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: "call_1".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some("call_1".to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "mistral".to_string(),
                id: "devstral-medium-latest".to_string(),
            },
            api: ProviderApi::Mistral,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let body = mistral_chat_body(&config, &request);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/mistral-tool-id.json"
        ))
        .expect("parse TS parity fixture");
        assert_eq!(body["messages"], fixture["request"]["body"]["messages"]);
    }

    #[test]
    fn bedrock_body_preserves_tools_calls_and_results() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ChatToolCall {
                        id: "tooluse_1".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    }],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: "file contents".to_string(),
                    media: Vec::new(),
                    tool_call_id: Some("tooluse_1".to_string()),
                    tool_name: Some("read".to_string()),
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
        };
        let config = ProviderConfig {
            model: ModelRef {
                provider: "amazon-bedrock".to_string(),
                id: "us.anthropic.claude-opus-4-6-20260112-v1:0".to_string(),
            },
            api: ProviderApi::Bedrock,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };

        let body = bedrock_body(&config, &request);
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "read");
        assert_eq!(
            body["messages"][0]["content"][0]["toolUse"]["toolUseId"],
            "tooluse_1"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["toolResult"]["toolUseId"],
            "tooluse_1"
        );
    }

    #[test]
    fn parses_bedrock_tool_use_events() {
        let response = json!({
            "output": {
                "message": {
                    "content": [{
                        "toolUse": {
                            "toolUseId": "tooluse_1",
                            "name": "read",
                            "input": { "path": "a.txt" }
                        }
                    }]
                }
            },
            "stopReason": "tool_use"
        });

        assert_eq!(
            parse_bedrock_response_events(&response),
            Some(vec![StreamEvent::ToolCall {
                id: "tooluse_1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }])
        );
    }

    #[test]
    fn openrouter_images_body_matches_ts_shape() {
        let media = MediaInput {
            mime_type: "image/png".to_string(),
            data_base64: "aW1hZ2U=".to_string(),
            path: Some("fixture.png".to_string()),
            width: Some(1),
            height: Some(1),
        };
        let config = ImageProviderConfig {
            model: ModelRef {
                provider: "openrouter".to_string(),
                id: "google/gemini-3.1-flash-image-preview".to_string(),
            },
            api: ImageProviderApi::OpenRouterImages,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            auth: ProviderAuth::ApiKey("token".to_string()),
            output_modalities: vec!["image".to_string(), "text".to_string()],
        };
        let body = openrouter_images_body(
            &config,
            &[
                ImageGenerationInput::Text {
                    text: "Generate a dog".to_string(),
                },
                ImageGenerationInput::Image { media },
            ],
        );

        assert_eq!(body["model"], "google/gemini-3.1-flash-image-preview");
        assert_eq!(body["stream"], false);
        assert_eq!(body["modalities"], json!(["image", "text"]));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "Generate a dog");
        assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1hZ2U="
        );
    }

    #[test]
    fn parses_openrouter_images_response() {
        let response = json!({
            "id": "img-1",
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 34
            },
            "choices": [{
                "message": {
                    "content": "Here is your image.",
                    "images": [
                        { "image_url": "data:image/png;base64,ZmFrZS1wbmc=" },
                        { "image_url": { "url": "data:image/jpeg;base64,ZmFrZS1qcGc=" } }
                    ]
                }
            }]
        });

        let parsed = parse_openrouter_images_response(response).expect("parse image response");
        assert_eq!(parsed.response_id.as_deref(), Some("img-1"));
        assert_eq!(parsed.input_tokens, 12);
        assert_eq!(parsed.output_tokens, 34);
        assert_eq!(
            parsed.output[0],
            ImageGenerationOutput::Text("Here is your image.".to_string())
        );
        assert_eq!(
            parsed.output[1],
            ImageGenerationOutput::Image {
                mime_type: "image/png".to_string(),
                data_base64: "ZmFrZS1wbmc=".to_string(),
            }
        );
        assert_eq!(
            parsed.output[2],
            ImageGenerationOutput::Image {
                mime_type: "image/jpeg".to_string(),
                data_base64: "ZmFrZS1qcGc=".to_string(),
            }
        );
    }

    #[test]
    fn provider_parity_request_helpers_match_expected_shapes() {
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api").expect("codex url"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            anthropic_messages_url(&ProviderConfig {
                model: ModelRef {
                    provider: "fireworks".to_string(),
                    id: "accounts/fireworks/models/deepseek-v4-pro".to_string(),
                },
                api: ProviderApi::Anthropic,
                base_url: Some("https://api.fireworks.ai/inference".to_string()),
                auth: ProviderAuth::ApiKey("token".to_string()),
                thinking_level: None,
                thinking_budget_tokens: None,
                session_id: None,
                traits: ModelTraits::default(),
            })
            .expect("anthropic compatible url"),
            "https://api.fireworks.ai/inference/v1/messages"
        );
        assert_eq!(
            azure_openai_responses_url("https://example.openai.azure.com/openai/v1"),
            "https://example.openai.azure.com/openai/v1/responses?api-version=v1"
        );
        assert_eq!(
            encode_path_segment("us.anthropic.claude-opus-4-6-v1"),
            "us.anthropic.claude-opus-4-6-v1"
        );
        assert_eq!(
            encode_path_segment("workers-ai/@cf/model"),
            "workers-ai%2F%40cf%2Fmodel"
        );
    }

    /// A request config for a built-in model as noel builds one.
    fn builtin_config(model: &catalog::CatalogModel) -> ProviderConfig {
        ProviderConfig {
            model: ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            },
            api: model.api.clone(),
            base_url: model.base_url.clone(),
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::from(model),
        }
    }

    /// A request config for the model `provider` starts on.
    fn provider_config(provider: &str) -> ProviderConfig {
        builtin_config(
            catalog::provider_base_model(provider)
                .unwrap_or_else(|| panic!("{provider} has no built-in model")),
        )
    }

    /// The catalog's base URLs are API roots, as upstream's SDK clients take
    /// them. Every built-in model's request goes to its own base URL plus
    /// the path its API defines; no provider is special.
    #[test]
    fn every_catalog_base_url_gets_its_apis_path() {
        let mut checked = BTreeMap::new();
        for model in catalog::builtin_models() {
            // A URL from the environment (Azure, Cloudflare) is covered by
            // its own test.
            let Some(base) = model.base_url.as_deref().filter(|base| !base.contains('{')) else {
                continue;
            };
            let config = builtin_config(model);
            let (url, path) = match model.api {
                ProviderApi::Faux => continue,
                ProviderApi::OpenAi => {
                    let provider = OpenAiProvider {
                        config,
                        client: reqwest::Client::new(),
                    };
                    (
                        format!("{}/chat/completions", provider.chat_base_url().unwrap()),
                        "/chat/completions".to_string(),
                    )
                }
                ProviderApi::OpenAiResponses => {
                    let provider = OpenAiResponsesProvider {
                        config,
                        endpoint: OpenAiResponsesEndpoint::OpenAi,
                        client: reqwest::Client::new(),
                    };
                    (
                        provider.request_parts(false).unwrap().0,
                        "/responses".to_string(),
                    )
                }
                ProviderApi::OpenAiCodexResponses => (
                    codex_responses_url(base).unwrap(),
                    "/codex/responses".to_string(),
                ),
                ProviderApi::Anthropic => (
                    anthropic_messages_url(&config).unwrap(),
                    "/v1/messages".to_string(),
                ),
                ProviderApi::Google => (
                    google_url(&config).unwrap(),
                    format!(
                        "/models/{}:streamGenerateContent?alt=sse",
                        encode_path_segment(&model.id)
                    ),
                ),
                ProviderApi::Mistral => (
                    mistral_chat_url(&config).unwrap(),
                    "/v1/chat/completions".to_string(),
                ),
                // Bedrock and Vertex build their URLs from the region and
                // project; Azure has no catalog URL.
                ProviderApi::Bedrock
                | ProviderApi::GoogleVertex
                | ProviderApi::AzureOpenAiResponses => continue,
            };
            assert_eq!(
                url,
                format!("{base}{path}"),
                "{}/{}",
                model.provider,
                model.id
            );
            *checked.entry(model.api.upstream_name()).or_insert(0) += 1;
        }
        for api in [
            "openai-completions",
            "openai-responses",
            "openai-codex-responses",
            "anthropic-messages",
            "google-generative-ai",
            "mistral-conversations",
        ] {
            assert!(
                checked.get(api).is_some_and(|count| *count > 5),
                "{api}: {checked:?}"
            );
        }
    }

    #[test]
    fn base_urls_that_already_name_the_api_version_are_kept() {
        let anthropic = |base: &str| {
            let mut config = provider_config("anthropic");
            config.base_url = Some(base.to_string());
            anthropic_messages_url(&config).unwrap()
        };
        assert_eq!(
            anthropic("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic("https://proxy.example/anthropic/"),
            "https://proxy.example/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic("https://proxy.example/v1/messages"),
            "https://proxy.example/v1/messages"
        );
        let mut mistral = provider_config("mistral");
        mistral.base_url = Some("https://api.mistral.ai/v1".to_string());
        assert_eq!(
            mistral_chat_url(&mistral).unwrap(),
            "https://api.mistral.ai/v1/chat/completions"
        );
        mistral.base_url = None;
        assert_eq!(
            mistral_chat_url(&mistral).unwrap(),
            "https://api.mistral.ai/v1/chat/completions"
        );
    }

    /// Azure's models carry no URL. A chat-completions model of Azure must
    /// go to the Azure resource or fail, never to OpenAI's endpoint with an
    /// Azure key.
    #[test]
    fn an_azure_chat_model_never_falls_back_to_openais_endpoint() {
        let azure_chat = catalog::builtin_models()
            .iter()
            .find(|model| model.provider == AZURE_PROVIDER && model.api == ProviderApi::OpenAi)
            .expect("an Azure chat-completions model");
        assert_eq!(azure_chat.base_url, None);
        for provider in [AZURE_PROVIDER, LEGACY_AZURE_PROVIDER] {
            let mut config = builtin_config(azure_chat);
            config.model.provider = provider.to_string();
            let resolved = OpenAiProvider {
                config: config.clone(),
                client: reqwest::Client::new(),
            }
            .chat_base_url();
            // Without AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME
            // this is an error; with one, that resource.
            assert!(
                !resolved.is_ok_and(|url| url.contains("api.openai.com")),
                "{provider}"
            );
            config.base_url = Some("https://example.openai.azure.com".to_string());
            assert_eq!(
                OpenAiProvider {
                    config,
                    client: reqwest::Client::new(),
                }
                .chat_base_url()
                .unwrap(),
                "https://example.openai.azure.com/openai/v1"
            );
        }
        // Any other provider without a URL keeps OpenAI's default.
        let mut openai = provider_config("openai");
        openai.api = ProviderApi::OpenAi;
        openai.base_url = None;
        assert_eq!(
            OpenAiProvider {
                config: openai,
                client: reqwest::Client::new(),
            }
            .chat_base_url()
            .unwrap(),
            "https://api.openai.com/v1"
        );
    }

    fn effort_request() -> ProviderRequest {
        ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// A request config for a reasoning model with this thinking level map.
    fn thinker(
        provider: &str,
        id: &str,
        api: ProviderApi,
        map: Value,
        level: &str,
    ) -> ProviderConfig {
        ProviderConfig {
            model: ModelRef {
                provider: provider.to_string(),
                id: id.to_string(),
            },
            api,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: Some(level.to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits {
                reasoning: Some(true),
                thinking_level_map: Some(serde_json::from_value(map).expect("level map")),
                ..ModelTraits::default()
            },
        }
    }

    /// The effort a request names is the level clamped to the model and
    /// spelled as the model's catalog entry spells it.
    #[test]
    fn reasoning_effort_follows_the_models_thinking_level_map() {
        let map = json!({ "off": "none", "minimal": null, "xhigh": "xhigh" });
        let responses = |level: &str| {
            let config = thinker(
                "openai",
                "gpt-x",
                ProviderApi::OpenAiResponses,
                map.clone(),
                level,
            );
            openai_responses_body(&config, &effort_request())["reasoning"]["effort"].clone()
        };
        // `off` as the model spells it; a level the model lacks becomes the
        // nearest one it has, above first.
        assert_eq!(responses("off"), "none");
        assert_eq!(responses("minimal"), "low");
        assert_eq!(responses("high"), "high");
        assert_eq!(responses("xhigh"), "xhigh");
        assert_eq!(responses("max"), "xhigh");
        // A model may spell a level differently.
        let codex = thinker(
            "openai-codex",
            "gpt-x",
            ProviderApi::OpenAiCodexResponses,
            json!({ "minimal": "low" }),
            "minimal",
        );
        assert_eq!(
            openai_codex_responses_body(&codex, &effort_request())["reasoning"]["effort"],
            "low"
        );
        // OpenRouter is sent the model's own names too, not a collapsed set.
        let openrouter = |level: &str| {
            let config = thinker(
                "openrouter",
                "lab/model",
                ProviderApi::OpenAi,
                json!({ "off": null, "medium": null, "xhigh": "xhigh", "max": "ultra" }),
                level,
            );
            openai_chat_completions_body(&config, &effort_request())["reasoning"]["effort"].clone()
        };
        assert_eq!(openrouter("off"), "minimal");
        assert_eq!(openrouter("medium"), "high");
        assert_eq!(openrouter("xhigh"), "xhigh");
        assert_eq!(openrouter("max"), "ultra");
        // A model known to have no reasoning is sent nothing.
        let mut plain = thinker(
            "openai",
            "gpt-x",
            ProviderApi::OpenAiResponses,
            json!({}),
            "high",
        );
        plain.traits.reasoning = Some(false);
        assert_eq!(
            openai_responses_body(&plain, &effort_request()).get("reasoning"),
            None
        );
        // Copilot is sent nothing for `off`.
        let copilot = |level: &str| {
            let config = thinker(
                "github-copilot",
                "gpt-x",
                ProviderApi::OpenAiResponses,
                json!({ "off": "none" }),
                level,
            );
            github_copilot_responses_body(&config, &effort_request())
        };
        assert_eq!(copilot("off").get("reasoning"), None);
        assert_eq!(copilot("high")["reasoning"]["effort"], "high");
        // Nothing known of the model: the level as configured.
        let mut unknown = thinker(
            "openai",
            "gpt-x",
            ProviderApi::OpenAiResponses,
            json!({}),
            "minimal",
        );
        unknown.traits = ModelTraits::default();
        assert_eq!(
            openai_responses_body(&unknown, &effort_request())["reasoning"]["effort"],
            "minimal"
        );
        unknown.thinking_level = Some("max".to_string());
        assert_eq!(
            openai_responses_body(&unknown, &effort_request())["reasoning"]["effort"],
            "xhigh"
        );
    }

    #[test]
    fn anthropic_and_bedrock_thinking_follow_the_models_thinking_level_map() {
        let anthropic = |id: &str, map: Value, level: &str| {
            let config = thinker("anthropic", id, ProviderApi::Anthropic, map, level);
            anthropic_body(&config, &effort_request())
        };
        // The model's own names for the top levels.
        assert_eq!(
            anthropic("claude-sonnet-4-6", json!({ "max": "max" }), "max")["output_config"]
                ["effort"],
            "max"
        );
        assert_eq!(
            anthropic("claude-opus-4-8", json!({ "xhigh": "xhigh" }), "xhigh")["output_config"]
                ["effort"],
            "xhigh"
        );
        // Off is said where the model can turn thinking off, and left
        // unsaid where it cannot.
        assert_eq!(
            anthropic("claude-opus-4-8", json!({}), "off")["thinking"]["type"],
            "disabled"
        );
        assert_eq!(
            anthropic("claude-opus-5", json!({ "off": null }), "off").get("thinking"),
            None
        );
        // A model known to have no reasoning is sent no thinking at all.
        let mut plain = thinker(
            "anthropic",
            "claude-opus-4-8",
            ProviderApi::Anthropic,
            json!({}),
            "high",
        );
        plain.traits.reasoning = Some(false);
        assert_eq!(
            anthropic_body(&plain, &effort_request()).get("thinking"),
            None
        );

        let bedrock = thinker(
            "amazon-bedrock",
            "us.anthropic.claude-sonnet-4-6",
            ProviderApi::Bedrock,
            json!({ "max": "max" }),
            "max",
        );
        assert_eq!(
            bedrock_body(&bedrock, &effort_request())["additionalModelRequestFields"]
                ["output_config"]["effort"],
            "max"
        );
    }

    #[test]
    fn anthropic_compatible_headers_cover_kimi_static_headers() {
        let headers = anthropic_headers(&ProviderConfig {
            model: ModelRef {
                provider: "kimi-coding".to_string(),
                id: "kimi-for-coding".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.kimi.com/coding".to_string()),
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        })
        .expect("headers");

        assert_eq!(
            headers
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("KimiCLI/1.5")
        );
    }

    #[test]
    fn thinking_levels_map_to_provider_payloads() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("system".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let opus = ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-opus-4-7".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: Some("max".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        let opus_body = anthropic_body(&opus, &request);
        assert_eq!(opus_body["thinking"]["type"], "adaptive");
        assert_eq!(opus_body["output_config"]["effort"], "max");

        let codex = ProviderConfig {
            model: ModelRef {
                provider: "openai-codex".to_string(),
                id: "gpt-5.5".to_string(),
            },
            api: ProviderApi::OpenAiCodexResponses,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        let codex_body = openai_responses_body(&codex, &request);
        assert_eq!(codex_body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn adaptive_thinking_support_is_version_parsed() {
        assert_eq!(
            claude_model_version("claude-opus-5-5", "opus"),
            Some((5, 5))
        );
        assert_eq!(
            claude_model_version("us.anthropic.claude-opus-4-6-v1", "opus"),
            Some((4, 6))
        );
        assert_eq!(
            claude_model_version("claude-opus-4.7", "opus"),
            Some((4, 7))
        );
        assert_eq!(claude_model_version("claude-3-opus-20240229", "opus"), None);
        assert_eq!(claude_model_version("gpt-5.5", "opus"), None);

        for id in [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-5-5",
            "claude-sonnet-4-6",
            "us.anthropic.claude-opus-5-5-v1",
        ] {
            assert!(anthropic_supports_adaptive_thinking(id), "{id}");
        }
        for id in ["claude-sonnet-4-5", "claude-3-5-haiku", "claude-opus"] {
            assert!(!anthropic_supports_adaptive_thinking(id), "{id}");
        }
    }

    /// An Anthropic request config for a model nothing is known about
    /// beyond its id.
    fn anthropic_test_config(
        id: &str,
        thinking: Option<&str>,
        budget: Option<u64>,
    ) -> ProviderConfig {
        ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: id.to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: thinking.map(str::to_string),
            thinking_budget_tokens: budget,
            session_id: None,
            traits: ModelTraits::default(),
        }
    }

    /// The config with the catalog's traits for its model, as noel builds
    /// one: the output cap, the context window, and what the model supports
    /// of thinking. The thinking level stays as the fixture asked for it;
    /// request builders clamp it, as upstream's do.
    fn with_catalog(mut config: ProviderConfig) -> ProviderConfig {
        let model = catalog::builtin_model(&config.model.provider, &config.model.id)
            .unwrap_or_else(|| panic!("{:?} is not a built-in model", config.model));
        config.traits = ModelTraits::from(model);
        config
    }

    fn uncapped_request(system_prompt: &str) -> ProviderRequest {
        ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some(system_prompt.to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "write the file".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        }
    }

    #[test]
    fn anthropic_requests_without_a_cap_use_the_model_output_limit() {
        let request = uncapped_request("system");
        // Every Anthropic model of the catalog is capped at its `maxTokens`.
        let mut caps = BTreeSet::new();
        for model in catalog::builtin_models() {
            if model.provider != "anthropic" {
                continue;
            }
            let config = with_catalog(anthropic_test_config(&model.id, None, None));
            let body = anthropic_body(&config, &request);
            assert_eq!(body["max_tokens"], model.max_tokens, "{}", model.id);
            caps.insert(model.max_tokens);
        }
        // The catalog tells models apart; this is not one cap for all.
        assert!(caps.len() > 1, "{caps:?}");
        // A model nothing is known about gets upstream's default for a
        // model without a cap.
        let body = anthropic_body(
            &anthropic_test_config("claude-unlisted", None, None),
            &request,
        );
        assert_eq!(body["max_tokens"], 16_384);
    }

    #[test]
    fn anthropic_default_cap_leaves_room_in_the_context_window() {
        // The input estimates as 160_004 tokens (560k + 14 characters at 3.5
        // per token); 200k minus that and the 4096 safety margin remain.
        let request = uncapped_request(&"x".repeat(560_000));
        let with_limits = |mut config: ProviderConfig| {
            config.traits = ModelTraits {
                context_window: Some(200_000),
                max_tokens: Some(64_000),
                ..ModelTraits::default()
            };
            config
        };
        let config = with_limits(anthropic_test_config("claude-sonnet-4-5", None, None));
        assert_eq!(anthropic_body(&config, &request)["max_tokens"], 35_900);

        // A thinking budget stays below the cap and leaves 1024 for the answer.
        let thinking = with_limits(anthropic_test_config(
            "claude-sonnet-4-5",
            Some("high"),
            Some(50_000),
        ));
        let body = anthropic_body(&thinking, &request);
        assert_eq!(body["thinking"]["budget_tokens"], 34_876);
        let body = anthropic_body(&thinking, &uncapped_request("system"));
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["thinking"]["budget_tokens"], 50_000);

        // An explicit cap is still sent as given.
        let capped = ProviderRequest {
            max_output_tokens: Some(2048),
            ..request
        };
        let body = anthropic_body(&thinking, &capped);
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn opus_5_5_uses_adaptive_thinking_with_xhigh() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let opus = ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-opus-5-5".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        let body = anthropic_body(&opus, &request);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(
            bedrock_adaptive_effort("us.anthropic.claude-opus-5-5-v1", "xhigh"),
            "xhigh"
        );
        assert_eq!(
            bedrock_adaptive_effort("us.anthropic.claude-opus-4-6-v1", "xhigh"),
            "high"
        );
    }

    #[test]
    fn claude_code_version_resolution_prefers_env_then_override() {
        assert_eq!(
            resolve_claude_code_version(None, None),
            DEFAULT_CLAUDE_CODE_VERSION
        );
        assert_eq!(
            resolve_claude_code_version(None, Some("2.1.300")),
            "2.1.300"
        );
        assert_eq!(
            resolve_claude_code_version(Some("9.9.9"), Some("2.1.300")),
            "9.9.9"
        );
        assert_eq!(
            resolve_claude_code_version(Some("   "), Some("")),
            DEFAULT_CLAUDE_CODE_VERSION
        );
    }

    #[test]
    fn anthropic_claude_code_oauth_matches_ts_identity_shape() {
        // The runtime version tracks the latest published Claude Code release;
        // pin it to the compiled fallback, which must equal upstream's pinned
        // version, to compare the identity shape.
        set_claude_code_version(DEFAULT_CLAUDE_CODE_VERSION);
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/anthropic-claude-code-oauth.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            // The TS fixture script passes an explicit `maxTokens: 4096`.
            max_output_tokens: Some(4096),
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-sonnet-4-6".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ClaudeCodeOAuth {
                access_token: "access-token".to_string(),
            },
            thinking_level: Some("off".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];

        let headers = anthropic_headers(&config).expect("anthropic headers");
        for name in [
            "accept",
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version",
            "content-type",
            "user-agent",
            "x-app",
        ] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }

        let body = anthropic_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);
    }

    #[test]
    fn anthropic_api_key_matches_ts_body_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/anthropic-api-key.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            // The TS fixture script passes an explicit `maxTokens: 4096`.
            max_output_tokens: Some(4096),
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-sonnet-4-6".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ApiKey("api-key".to_string()),
            thinking_level: Some("off".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];

        let headers = anthropic_headers(&config).expect("anthropic headers");
        for name in ["accept", "anthropic-version", "content-type"] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
        assert!(headers.get("x-api-key").is_some());
        assert!(headers.get(AUTHORIZATION).is_none());
        assert!(headers.get("anthropic-beta").is_none());
        assert!(headers.get("x-app").is_none());

        let body = anthropic_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);
    }

    #[test]
    fn openai_compatible_headers_cover_copilot_and_cloudflare() {
        let copilot = ProviderConfig {
            model: ModelRef {
                provider: "github-copilot".to_string(),
                id: "gpt-5.4".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.individual.githubcopilot.com".to_string()),
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        let headers = openai_compatible_headers(&copilot, "token", true).expect("copilot headers");
        assert_eq!(
            headers
                .get("Copilot-Integration-Id")
                .and_then(|value| value.to_str().ok()),
            Some("vscode-chat")
        );
        assert_eq!(
            headers
                .get("Copilot-Vision-Request")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );

        let cloudflare = ProviderConfig {
            model: ModelRef {
                provider: "cloudflare-ai-gateway".to_string(),
                id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: None,
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: Some("session-test".to_string()),
            traits: ModelTraits::default(),
        };
        let headers =
            openai_compatible_headers(&cloudflare, "token", false).expect("cloudflare headers");
        assert_eq!(
            headers
                .get("cf-aig-authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer token")
        );
        assert_eq!(
            headers
                .get("x-session-affinity")
                .and_then(|value| value.to_str().ok()),
            Some("session-test")
        );
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn openai_codex_oauth_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/openai-codex-chatgpt-oauth.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "openai-codex".to_string(),
                id: "gpt-5.5".to_string(),
            },
            api: ProviderApi::OpenAiCodexResponses,
            base_url: None,
            auth: ProviderAuth::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: Some("acct_ts_parity".to_string()),
            },
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];

        let headers = codex_headers(&config.auth).expect("codex headers");
        for name in [
            "accept",
            "chatgpt-account-id",
            "content-type",
            "openai-beta",
            "originator",
        ] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
        assert!(headers.get(AUTHORIZATION).is_some());

        let body = openai_responses_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);

        // Without a system prompt, upstream's placeholder stands in.
        for system_prompt in [None, Some(String::new())] {
            let request = ProviderRequest {
                system_prompt,
                ..request.clone()
            };
            assert_eq!(
                openai_responses_body(&config, &request)["instructions"],
                "You are a helpful assistant."
            );
        }
    }

    #[test]
    fn openai_responses_tools_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/openai-responses-tools.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![ToolDefinition {
                name: "fixture_echo".to_string(),
                description: "Echo text for the parity fixture.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                        },
                    },
                    "required": ["text"],
                }),
            }],
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "openai".to_string(),
                id: "gpt-5.4".to_string(),
            },
            api: ProviderApi::OpenAiResponses,
            base_url: None,
            auth: ProviderAuth::ApiKey("sk-ts-parity-token".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let body = openai_responses_body(&config, &request);
        assert_eq!(body, fixture["request"]["body"]);
    }

    #[test]
    fn anthropic_tools_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/anthropic-tools.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            // The TS fixture script passes an explicit `maxTokens: 4096`.
            max_output_tokens: Some(4096),
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![fixture_echo_tool()],
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-sonnet-4-6".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ApiKey("api-key".to_string()),
            thinking_level: Some("off".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let body = anthropic_body(&config, &request);

        assert_eq!(body, fixture["request"]["body"]);
    }

    #[test]
    fn anthropic_body_stays_within_cache_control_budget() {
        fn count_cache_control(value: &Value) -> usize {
            match value {
                Value::Object(map) => {
                    let here = usize::from(map.contains_key("cache_control"));
                    here + map.values().map(count_cache_control).sum::<usize>()
                }
                Value::Array(items) => items.iter().map(count_cache_control).sum(),
                _ => 0,
            }
        }

        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            // More tools than the 4-breakpoint cap so a per-tool breakpoint would overflow.
            tools: vec![
                fixture_echo_tool(),
                fixture_echo_tool(),
                fixture_echo_tool(),
                fixture_echo_tool(),
                fixture_echo_tool(),
                fixture_echo_tool(),
                fixture_echo_tool(),
            ],
        };
        // OAuth path emits two system breakpoints, the most a request can carry.
        let config = ProviderConfig {
            model: ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-opus-4-8".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: None,
            auth: ProviderAuth::ClaudeCodeOAuth {
                access_token: "token".to_string(),
            },
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        };
        let body = anthropic_body(&config, &request);

        assert!(
            count_cache_control(&body) <= 4,
            "expected <= 4 cache_control breakpoints, found {} in {body}",
            count_cache_control(&body)
        );

        let tools = body["tools"].as_array().expect("tools array");
        for tool in &tools[..tools.len() - 1] {
            assert!(
                tool.get("cache_control").is_none(),
                "only the last tool should carry cache_control"
            );
        }
        assert!(
            tools.last().unwrap().get("cache_control").is_some(),
            "last tool should carry cache_control"
        );
    }

    #[test]
    fn google_tools_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/google-tools.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![fixture_echo_tool()],
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "google".to_string(),
                id: "gemini-2.5-pro".to_string(),
            },
            api: ProviderApi::Google,
            base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
            auth: ProviderAuth::ApiKey("google-key".to_string()),
            thinking_level: Some("high".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });

        assert_eq!(google_body(&config, &request), fixture["request"]["body"]);
    }

    #[test]
    fn mistral_tools_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/mistral-tools.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![fixture_echo_tool()],
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "mistral".to_string(),
                id: "devstral-medium-latest".to_string(),
            },
            api: ProviderApi::Mistral,
            base_url: Some("https://api.mistral.ai/v1".to_string()),
            auth: ProviderAuth::ApiKey("mistral-key".to_string()),
            thinking_level: Some("high".to_string()),
            thinking_budget_tokens: None,
            session_id: Some("session_ts_parity".to_string()),
            traits: ModelTraits::default(),
        });

        assert_eq!(
            mistral_chat_body(&config, &request),
            fixture["request"]["body"]
        );
    }

    #[test]
    fn bedrock_tools_matches_ts_payload_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/bedrock-tools.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![fixture_echo_tool()],
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "amazon-bedrock".to_string(),
                id: "us.anthropic.claude-opus-4-6-v1".to_string(),
            },
            api: ProviderApi::Bedrock,
            base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string()),
            auth: ProviderAuth::ApiKey("bedrock-key".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });

        assert_eq!(bedrock_body(&config, &request), fixture["payload"]);
    }

    fn fixture_echo_tool() -> ToolDefinition {
        ToolDefinition {
            name: "fixture_echo".to_string(),
            description: "Echo text for the parity fixture.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                    },
                },
                "required": ["text"],
            }),
        }
    }

    #[test]
    fn openrouter_kimi_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/openrouter-kimi.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "openrouter".to_string(),
                id: "moonshotai/kimi-k2.6".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            auth: ProviderAuth::ApiKey("openrouter-key".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];

        let headers = openai_compatible_headers(&config, "openrouter-key", false).expect("headers");
        for name in ["accept", "content-type"] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.starts_with("Bearer ")),
            Some(true)
        );

        let body = openai_chat_completions_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);
    }

    #[test]
    fn github_copilot_matches_ts_responses_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/github-copilot-gpt-5.4.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "github-copilot".to_string(),
                id: "gpt-5.4".to_string(),
            },
            api: ProviderApi::OpenAiResponses,
            base_url: Some("https://api.individual.githubcopilot.com".to_string()),
            auth: ProviderAuth::ApiKey("copilot-key".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];
        let provider = OpenAiResponsesProvider {
            config: config.clone(),
            endpoint: OpenAiResponsesEndpoint::OpenAi,
            client: reqwest::Client::new(),
        };
        let (url, headers, _) = provider.request_parts(false).expect("request parts");

        assert_eq!(url, fixture_request["url"].as_str().expect("fixture url"));
        for name in [
            "accept",
            "content-type",
            "copilot-integration-id",
            "editor-plugin-version",
            "editor-version",
            "openai-intent",
            "user-agent",
            "x-initiator",
        ] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
        assert!(headers.get(AUTHORIZATION).is_some());

        let body = openai_responses_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);
    }

    #[test]
    fn google_gemini_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/google-gemini-2.5-pro.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "google".to_string(),
                id: "gemini-2.5-pro".to_string(),
            },
            api: ProviderApi::Google,
            base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
            auth: ProviderAuth::ApiKey("google-key".to_string()),
            thinking_level: Some("high".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];
        let headers = google_headers("google-key").expect("headers");

        assert_eq!(
            google_url(&config).expect("url"),
            fixture_request["url"].as_str().expect("fixture url")
        );
        assert_eq!(
            headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            fixture_request["headers"]["content-type"].as_str()
        );
        assert!(headers.get("x-goog-api-key").is_some());

        assert_eq!(google_body(&config, &request), fixture_request["body"]);
    }

    #[test]
    fn bedrock_claude_opus_matches_ts_payload_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/bedrock-claude-opus-4.6.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "amazon-bedrock".to_string(),
                id: "us.anthropic.claude-opus-4-6-v1".to_string(),
            },
            api: ProviderApi::Bedrock,
            base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string()),
            auth: ProviderAuth::ApiKey("bedrock-key".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        });

        assert_eq!(bedrock_body(&config, &request), fixture["payload"]);
    }

    #[test]
    fn cloudflare_ai_gateway_matches_ts_request_shape() {
        std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acct_ts_parity");
        std::env::set_var("CLOUDFLARE_GATEWAY_ID", "gateway_ts_parity");
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/cloudflare-ai-gateway-kimi.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "cloudflare-ai-gateway".to_string(),
                id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: Some(
                "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
                    .to_string(),
            ),
            auth: ProviderAuth::ApiKey("cloudflare-key".to_string()),
            thinking_level: Some("xhigh".to_string()),
            thinking_budget_tokens: None,
            session_id: Some("session_ts_parity".to_string()),
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];
        let provider = OpenAiProvider {
            config: config.clone(),
            client: reqwest::Client::new(),
        };

        assert_eq!(
            format!(
                "{}/chat/completions",
                provider.chat_base_url().expect("base url")
            ),
            fixture_request["url"].as_str().expect("fixture url")
        );
        let headers = openai_compatible_headers(&config, "cloudflare-key", false).expect("headers");
        for name in [
            "accept",
            "cf-aig-authorization",
            "content-type",
            "session_id",
            "x-client-request-id",
            "x-session-affinity",
        ] {
            if name == "cf-aig-authorization" {
                assert_eq!(
                    headers
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(|value| value.starts_with("Bearer ")),
                    Some(true)
                );
            } else {
                assert_eq!(
                    headers.get(name).and_then(|value| value.to_str().ok()),
                    fixture_request["headers"][name].as_str(),
                    "header {name}"
                );
            }
        }
        assert!(headers.get(AUTHORIZATION).is_none());

        let body = openai_chat_completions_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);
    }

    #[test]
    fn mistral_devstral_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/mistral-devstral.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        };
        let config = with_catalog(ProviderConfig {
            model: ModelRef {
                provider: "mistral".to_string(),
                id: "devstral-medium-latest".to_string(),
            },
            api: ProviderApi::Mistral,
            base_url: Some("https://api.mistral.ai/v1".to_string()),
            auth: ProviderAuth::ApiKey("mistral-key".to_string()),
            thinking_level: Some("high".to_string()),
            thinking_budget_tokens: None,
            session_id: Some("session_ts_parity".to_string()),
            traits: ModelTraits::default(),
        });
        let fixture_request = &fixture["request"];
        let headers = mistral_headers(&config, "mistral-key").expect("headers");

        for name in ["accept", "content-type", "x-affinity"] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
        // The fixture was captured on Linux x86_64 with the kernel release
        // pinned to `<kernel>`; Rust sends the same identity for its own host.
        assert_eq!(
            headers
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some(pi_user_agent())
        );
        assert_eq!(
            Some(
                format_pi_user_agent(node_platform("linux"), "<kernel>", node_arch("x86_64"))
                    .as_str()
            ),
            fixture_request["headers"]["user-agent"].as_str()
        );
        assert!(headers.get(AUTHORIZATION).is_some());

        let body = mistral_chat_body(&config, &request);
        assert_eq!(body, fixture_request["body"]);
    }

    #[test]
    fn codex_headers_accept_login_tokens_and_api_keys() {
        let login_headers = codex_headers(&ProviderAuth::ChatGptOAuth {
            access_token: "access-token".to_string(),
            account_id: Some("account-id".to_string()),
        })
        .expect("login headers");
        assert_eq!(
            login_headers
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("account-id")
        );

        let api_key_headers =
            codex_headers(&ProviderAuth::ApiKey("api-key".to_string())).expect("api key headers");
        assert_eq!(
            api_key_headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer api-key")
        );
        assert!(api_key_headers.get("ChatGPT-Account-ID").is_none());
    }

    #[test]
    fn codex_headers_extract_account_id_from_chatgpt_jwt() {
        let token = concat!(
            "eyJhbGciOiJub25lIn0.",
            "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF9mcm9tX2p3dCJ9fQ.",
            "signature"
        );
        let headers = codex_headers(&ProviderAuth::ChatGptOAuth {
            access_token: token.to_string(),
            account_id: None,
        })
        .expect("codex headers");

        assert_eq!(
            headers
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("acct_from_jwt")
        );
    }

    fn image_tool_result(content: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: content.to_string(),
            media: vec![MediaInput {
                mime_type: "image/png".to_string(),
                data_base64: "aW1hZ2U=".to_string(),
                path: None,
                width: None,
                height: None,
            }],
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("read".to_string()),
            tool_calls: Vec::new(),
        }
    }

    #[test]
    fn anthropic_tool_results_carry_images_as_content_blocks() {
        let messages = anthropic_messages(&[
            image_tool_result("Read image file [image/png]"),
            image_tool_result(""),
        ]);
        assert_eq!(
            messages[0]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": [
                    { "type": "text", "text": "Read image file [image/png]" },
                    {
                        "type": "image",
                        "source": { "type": "base64", "media_type": "image/png", "data": "aW1hZ2U=" },
                    },
                ],
            })
        );
        // An image alone still needs a text block.
        assert_eq!(
            messages[1]["content"][0]["content"][0],
            json!({ "type": "text", "text": "(see attached image)" })
        );
    }

    #[test]
    fn bedrock_tool_results_carry_images_as_content_blocks() {
        let messages = bedrock_messages(&[image_tool_result("Read image file [image/png]")]);
        assert_eq!(
            messages[0]["content"][0]["toolResult"]["content"],
            json!([
                { "text": "Read image file [image/png]" },
                { "image": { "format": "png", "source": { "bytes": "aW1hZ2U=" } } },
            ])
        );
    }

    #[test]
    fn chat_completions_tool_results_say_their_images_were_left_out() {
        let request = ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![image_tool_result("Read image file [image/png]")],
            tools: Vec::new(),
        };
        for api in [ProviderApi::OpenAi, ProviderApi::Mistral] {
            let config = ProviderConfig {
                model: ModelRef {
                    provider: "local".to_string(),
                    id: "test".to_string(),
                },
                api,
                base_url: None,
                auth: ProviderAuth::None,
                thinking_level: None,
                thinking_budget_tokens: None,
                session_id: None,
                traits: ModelTraits::default(),
            };
            let messages = openai_chat_messages(&config, &request);
            assert_eq!(messages.len(), 1);
            assert_eq!(
                messages[0]["content"],
                "Read image file [image/png]\n(tool image omitted: this provider API does not accept images in tool results)"
            );
        }
    }

    #[test]
    fn bedrock_messages_include_image_payloads() {
        let messages = bedrock_messages(&[ChatMessage {
            role: ChatRole::User,
            content: "describe".to_string(),
            media: vec![MediaInput {
                mime_type: "image/png".to_string(),
                data_base64: "aW1hZ2U=".to_string(),
                path: None,
                width: None,
                height: None,
            }],
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }]);

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][1]["image"]["format"], "png");
        assert_eq!(
            messages[0]["content"][1]["image"]["source"]["bytes"],
            "aW1hZ2U="
        );
    }
}

use std::env;

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderApi {
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

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub model: ModelRef,
    pub api: ProviderApi,
    pub base_url: Option<String>,
    pub auth: ProviderAuth,
    pub thinking_level: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(default)]
    pub media: Vec<MediaInput>,
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
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider {provider} requires an API key")]
    MissingApiKey { provider: String },
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("request failed with status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("invalid header value: {0}")]
    Header(#[from] reqwest::header::InvalidHeaderValue),
    #[error("provider config error: {0}")]
    Config(String),
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError>;
}

pub fn create_provider(config: ProviderConfig) -> Box<dyn Provider> {
    match config.api {
        ProviderApi::Faux => Box::new(FauxProvider { config }),
        ProviderApi::OpenAi => Box::new(OpenAiProvider { config }),
        ProviderApi::OpenAiResponses => Box::new(OpenAiResponsesProvider {
            config,
            endpoint: OpenAiResponsesEndpoint::OpenAi,
        }),
        ProviderApi::OpenAiCodexResponses => Box::new(OpenAiResponsesProvider {
            config,
            endpoint: OpenAiResponsesEndpoint::Codex,
        }),
        ProviderApi::AzureOpenAiResponses => Box::new(OpenAiResponsesProvider {
            config,
            endpoint: OpenAiResponsesEndpoint::Azure,
        }),
        ProviderApi::Anthropic => Box::new(AnthropicProvider { config }),
        ProviderApi::Google => Box::new(GoogleProvider { config }),
        ProviderApi::GoogleVertex => Box::new(GoogleVertexProvider { config }),
        ProviderApi::Bedrock => Box::new(BedrockProvider { config }),
        ProviderApi::Mistral => Box::new(MistralProvider { config }),
    }
}

struct FauxProvider {
    config: ProviderConfig,
}

#[async_trait]
impl Provider for FauxProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
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
}

struct OpenAiProvider {
    config: ProviderConfig,
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
        let response = reqwest::Client::new()
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
        if let Some(text) = parse_openai_chat_completions_sse_text(&body) {
            return Ok(vec![
                StreamEvent::Text(text),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ]);
        }
        let response = serde_json::from_str::<Value>(&body)
            .map_err(|_| ProviderError::InvalidResponse(body.clone()))?;
        let text = response
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        Ok(vec![
            StreamEvent::Text(text.to_string()),
            StreamEvent::Stop {
                reason: response
                    .pointer("/choices/0/finish_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("stop")
                    .to_string(),
            },
        ])
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
        let response = reqwest::Client::new()
            .post(url)
            .headers(chatgpt_headers(&self.config.auth)?)
            .json(&openai_responses_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        let text = parse_openai_responses_sse_text(&body)
            .or_else(|| {
                serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|response| parse_openai_responses_text(&response))
            })
            .ok_or_else(|| ProviderError::InvalidResponse(body.clone()))?;
        Ok(vec![
            StreamEvent::Text(text),
            StreamEvent::Stop {
                reason: "completed".to_string(),
            },
        ])
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
        resolve_env_placeholders(
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://api.openai.com/v1"),
        )
    }
}

fn openai_chat_completions_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = json!({
        "model": config.model.id,
        "messages": openai_chat_messages(config, request),
    });
    if config.model.provider == "openrouter" {
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        body["store"] = json!(false);
        body["max_completion_tokens"] = json!(16384);
        if let Some(effort) = openrouter_reasoning_effort(config.thinking_level.as_deref()) {
            body["reasoning"] = json!({ "effort": effort });
        }
    } else if matches!(
        config.model.provider.as_str(),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai"
    ) {
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        body["max_tokens"] = json!(32000);
    }
    body
}

fn openai_chat_messages(config: &ProviderConfig, request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        let role = if config.model.provider == "openrouter" {
            "developer"
        } else {
            "system"
        };
        messages.push(json!({ "role": role, "content": system_prompt }));
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

fn openrouter_reasoning_effort(level: Option<&str>) -> Option<&'static str> {
    match normalized_thinking_level(level)? {
        "off" => Some("none"),
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
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
        "instructions": request.system_prompt.clone().unwrap_or_default(),
        "input": openai_responses_input(request),
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "store": false,
        "stream": true,
        "include": [],
    });
    if let Some(effort) = openai_reasoning_effort(config.thinking_level.as_deref()) {
        body["reasoning"] = json!({ "effort": effort });
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
    if let Some(effort) = openai_reasoning_effort(config.thinking_level.as_deref()) {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }
    body
}

fn openai_codex_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = json!({
        "model": config.model.id,
        "store": false,
        "stream": true,
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
    if let Some(effort) = openai_reasoning_effort(config.thinking_level.as_deref()) {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto",
        });
    }
    body
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
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let (url, headers, model) = self.request_parts(request_has_media(&request))?;
        let response = reqwest::Client::new()
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
        let text = parse_openai_responses_sse_text(&body)
            .or_else(|| {
                serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|response| parse_openai_responses_text(&response))
            })
            .ok_or_else(|| ProviderError::InvalidResponse(body.clone()))?;
        Ok(vec![
            StreamEvent::Text(text),
            StreamEvent::Stop {
                reason: "completed".to_string(),
            },
        ])
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
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let url = format!(
            "{}/messages",
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://api.anthropic.com/v1")
                .trim_end_matches('/')
        );

        let response = reqwest::Client::new()
            .post(url)
            .headers(anthropic_headers(&self.config)?)
            .json(&anthropic_body(&self.config, &request))
            .send()
            .await?;
        let response = error_for_status_with_body(response)
            .await?
            .json::<Value>()
            .await?;
        let text = response
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .filter(|text| !text.is_empty())
            .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
        Ok(vec![
            StreamEvent::Text(text),
            StreamEvent::Stop {
                reason: response
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("stop")
                    .to_string(),
            },
        ])
    }
}

fn anthropic_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    let mut body = json!({
        "model": config.model.id,
        "max_tokens": 4096,
        "system": anthropic_system(config, request),
        "messages": anthropic_messages_with_cache_control(&request.messages, true),
    });
    apply_anthropic_thinking(&mut body, config);
    body
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
            headers.insert(USER_AGENT, HeaderValue::from_static("claude-cli/2.1.75"));
            headers.insert("x-app", HeaderValue::from_static("cli"));
        }
        _ => {
            return Err(ProviderError::MissingApiKey {
                provider: config.model.provider.clone(),
            });
        }
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
    let Some(level) = normalized_thinking_level(config.thinking_level.as_deref()) else {
        return;
    };
    if level == "off" {
        body["thinking"] = json!({ "type": "disabled" });
        return;
    }
    if supports_anthropic_adaptive_thinking(&config.model.id) {
        body["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
        });
        body["output_config"] = json!({
            "effort": anthropic_adaptive_effort(&config.model.id, level),
        });
    } else {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": config.thinking_budget_tokens.unwrap_or(1024).max(1024),
            "display": "summarized",
        });
    }
}

fn supports_anthropic_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
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

fn openai_reasoning_effort(level: Option<&str>) -> Option<&'static str> {
    match normalized_thinking_level(level)? {
        "off" => Some("none"),
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("xhigh"),
        _ => None,
    }
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
}

#[async_trait]
impl Provider for GoogleProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let base_url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com/v1beta");
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            base_url.trim_end_matches('/'),
            self.config.model.id,
            api_key
        );
        let response = reqwest::Client::new()
            .post(url)
            .json(&json!({
                "systemInstruction": request.system_prompt.map(|text| json!({ "parts": [{ "text": text }] })),
                "contents": google_messages(&request.messages),
            }))
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

struct GoogleVertexProvider {
    config: ProviderConfig,
}

#[async_trait]
impl Provider for GoogleVertexProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let url = google_vertex_url(&self.config)?;
        let response = reqwest::Client::new()
            .post(format!("{url}?key={api_key}"))
            .json(&json!({
                "systemInstruction": request.system_prompt.map(|text| json!({ "parts": [{ "text": text }] })),
                "contents": google_messages(&request.messages),
            }))
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
}

#[async_trait]
impl Provider for MistralProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let base_url = resolve_env_placeholders(
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://api.mistral.ai/v1"),
        )?;
        let response = reqwest::Client::new()
            .post(format!(
                "{}/chat/completions",
                base_url.trim_end_matches('/')
            ))
            .headers(mistral_headers(&self.config, &api_key)?)
            .json(&mistral_chat_body(&self.config, &request))
            .send()
            .await?;
        let body = error_for_status_with_body(response).await?.text().await?;
        if let Some(text) = parse_openai_chat_completions_sse_text(&body) {
            return Ok(vec![
                StreamEvent::Text(text),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ]);
        }
        parse_openai_chat_response(
            serde_json::from_str::<Value>(&body)
                .map_err(|_| ProviderError::InvalidResponse(body.clone()))?,
        )
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

fn mistral_headers(config: &ProviderConfig, api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = bearer_headers(api_key)?;
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("mistral-client-typescript/2.2.1"),
    );
    if let Some(session_id) = &config.session_id {
        headers.insert("x-affinity", HeaderValue::from_str(session_id)?);
    }
    Ok(headers)
}

fn mistral_chat_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    json!({
        "model": config.model.id,
        "max_tokens": 32000,
        "stream": true,
        "messages": openai_messages(request),
    })
}

struct BedrockProvider {
    config: ProviderConfig,
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
        let response = reqwest::Client::new()
            .post(format!(
                "{}/model/{}/converse",
                base_url.trim_end_matches('/'),
                encode_path_segment(&self.config.model.id)
            ))
            .headers(bearer_headers(&bearer_token)?)
            .json(&json!({
                "modelId": self.config.model.id,
                "system": request.system_prompt.map(|text| vec![json!({ "text": text })]).unwrap_or_default(),
                "messages": bedrock_messages(&request.messages),
                "inferenceConfig": {
                    "maxTokens": 4096,
                },
            }))
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

fn parse_openai_chat_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
    let text = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
    Ok(vec![
        StreamEvent::Text(text.to_string()),
        StreamEvent::Stop {
            reason: response
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        },
    ])
}

fn parse_google_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
    let text = response
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
    Ok(vec![
        StreamEvent::Text(text),
        StreamEvent::Stop {
            reason: response
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str)
                .unwrap_or("STOP")
                .to_string(),
        },
    ])
}

fn parse_bedrock_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
    let text = response
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ProviderError::InvalidResponse(response.to_string()))?;
    Ok(vec![
        StreamEvent::Text(text),
        StreamEvent::Stop {
            reason: response
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string(),
        },
    ])
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
    let body = response.text().await.unwrap_or_default();
    Err(ProviderError::Status { status, body })
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
    let base_url = resolve_env_placeholders(
        config
            .base_url
            .as_deref()
            .unwrap_or("https://{GOOGLE_CLOUD_LOCATION}-aiplatform.googleapis.com"),
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
            json!({
                "role": if message.role == ChatRole::Assistant { "assistant" } else { "user" },
                "content": content,
            })
        })
        .collect()
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
    request
        .messages
        .iter()
        .map(|message| {
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
        })
        .collect()
}

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
            let content = if message.media.is_empty() {
                json!(message.content)
            } else {
                let mut parts = vec![json!({
                    "type": "text",
                    "text": message.content,
                })];
                parts.extend(message.media.iter().map(|media| {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media.mime_type,
                            "data": media.data_base64,
                        },
                    })
                }));
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

fn google_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.role != ChatRole::System)
        .map(|message| {
            let mut parts = vec![json!({ "text": message.content })];
            parts.extend(message.media.iter().map(|media| {
                json!({
                    "inline_data": {
                        "mime_type": media.mime_type,
                        "data": media.data_base64,
                    },
                })
            }));
            json!({
                "role": if message.role == ChatRole::Assistant { "model" } else { "user" },
                "parts": parts,
            })
        })
        .collect()
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
        });

        let events = provider
            .complete(ProviderRequest {
                system_prompt: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: "hello".to_string(),
                    media: Vec::new(),
                }],
            })
            .await
            .expect("faux provider should complete");

        assert_eq!(events[0], StreamEvent::Text("[faux/echo] ".to_string()));
        assert_eq!(events[1], StreamEvent::Text("hello".to_string()));
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
    fn provider_message_converters_include_image_parts() {
        let request = ProviderRequest {
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
            }],
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
        let google = google_messages(&request.messages);
        assert_eq!(
            google[0]["parts"][1]["inline_data"]["mime_type"],
            "image/png"
        );
    }

    #[test]
    fn provider_parity_request_helpers_match_expected_shapes() {
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api").expect("codex url"),
            "https://chatgpt.com/backend-api/codex/responses"
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

    #[test]
    fn thinking_levels_map_to_provider_payloads() {
        let request = ProviderRequest {
            system_prompt: Some("system".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
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
        };
        let codex_body = openai_responses_body(&codex, &request);
        assert_eq!(codex_body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn anthropic_claude_code_oauth_matches_ts_identity_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/anthropic-claude-code-oauth.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
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
        assert_eq!(body["model"], fixture_request["body"]["model"]);
        assert_eq!(body["max_tokens"], fixture_request["body"]["max_tokens"]);
        assert_eq!(body["system"], fixture_request["body"]["system"]);
        assert_eq!(body["messages"], fixture_request["body"]["messages"]);
        assert_eq!(body["thinking"], fixture_request["body"]["thinking"]);
    }

    #[test]
    fn anthropic_api_key_matches_ts_body_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/anthropic-api-key.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
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
        assert_eq!(body["model"], fixture_request["body"]["model"]);
        assert_eq!(body["max_tokens"], fixture_request["body"]["max_tokens"]);
        assert_eq!(body["system"], fixture_request["body"]["system"]);
        assert_eq!(body["messages"], fixture_request["body"]["messages"]);
        assert_eq!(body["thinking"], fixture_request["body"]["thinking"]);
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
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
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
    }

    #[test]
    fn openrouter_kimi_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/openrouter-kimi.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
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
        assert_eq!(body["model"], fixture_request["body"]["model"]);
        assert_eq!(body["messages"], fixture_request["body"]["messages"]);
        assert_eq!(body["stream"], fixture_request["body"]["stream"]);
        assert_eq!(
            body["stream_options"],
            fixture_request["body"]["stream_options"]
        );
        assert_eq!(body["store"], fixture_request["body"]["store"]);
        assert_eq!(
            body["max_completion_tokens"],
            fixture_request["body"]["max_completion_tokens"]
        );
        assert_eq!(body["reasoning"], fixture_request["body"]["reasoning"]);
    }

    #[test]
    fn github_copilot_matches_ts_responses_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/github-copilot-gpt-5.4.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
        let fixture_request = &fixture["request"];
        let provider = OpenAiResponsesProvider {
            config: config.clone(),
            endpoint: OpenAiResponsesEndpoint::OpenAi,
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
        assert_eq!(body["model"], fixture_request["body"]["model"]);
        assert_eq!(body["input"], fixture_request["body"]["input"]);
        assert_eq!(body["store"], fixture_request["body"]["store"]);
        assert_eq!(body["stream"], fixture_request["body"]["stream"]);
        assert_eq!(body["include"], fixture_request["body"]["include"]);
        assert_eq!(body["reasoning"], fixture_request["body"]["reasoning"]);
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
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
        let fixture_request = &fixture["request"];
        let provider = OpenAiProvider {
            config: config.clone(),
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
        assert_eq!(body["model"], fixture_request["body"]["model"]);
        assert_eq!(body["messages"], fixture_request["body"]["messages"]);
        assert_eq!(body["stream"], fixture_request["body"]["stream"]);
        assert_eq!(
            body["stream_options"],
            fixture_request["body"]["stream_options"]
        );
        assert_eq!(body["max_tokens"], fixture_request["body"]["max_tokens"]);
    }

    #[test]
    fn mistral_devstral_matches_ts_request_shape() {
        let fixture = serde_json::from_str::<Value>(include_str!(
            "../../../tests/fixtures/ts-parity/mistral-devstral.json"
        ))
        .expect("parse TS parity fixture");
        let request = ProviderRequest {
            system_prompt: Some("pi rust cli".to_string()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
            }],
        };
        let config = ProviderConfig {
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
        };
        let fixture_request = &fixture["request"];
        let headers = mistral_headers(&config, "mistral-key").expect("headers");

        for name in ["accept", "content-type", "user-agent", "x-affinity"] {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
        assert!(headers.get(AUTHORIZATION).is_some());

        let body = mistral_chat_body(&config, &request);
        assert_eq!(body["model"], fixture_request["body"]["model"]);
        assert_eq!(body["max_tokens"], fixture_request["body"]["max_tokens"]);
        assert_eq!(body["stream"], fixture_request["body"]["stream"]);
        assert_eq!(body["messages"], fixture_request["body"]["messages"]);
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
        }]);

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][1]["image"]["format"], "png");
        assert_eq!(
            messages[0]["content"][1]["image"]["source"]["bytes"],
            "aW1hZ2U="
        );
    }
}

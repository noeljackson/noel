use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
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
    Anthropic,
    Google,
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub model: ModelRef,
    pub api: ProviderApi,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
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
    #[error("invalid header value: {0}")]
    Header(#[from] reqwest::header::InvalidHeaderValue),
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
        ProviderApi::Anthropic => Box::new(AnthropicProvider { config }),
        ProviderApi::Google => Box::new(GoogleProvider { config }),
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
            .map(|message| message.content.as_str())
            .unwrap_or("");
        Ok(vec![
            StreamEvent::Text(format!(
                "[{}/{}] {last_user}",
                self.config.model.provider, self.config.model.id
            )),
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
        let api_key = self.api_key()?;
        let url = format!(
            "{}/chat/completions",
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/')
        );
        let messages = openai_messages(&request);
        let response = reqwest::Client::new()
            .post(url)
            .headers(bearer_headers(&api_key)?)
            .json(&json!({
                "model": self.config.model.id,
                "messages": messages,
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
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
    fn api_key(&self) -> Result<String, ProviderError> {
        self.config
            .api_key
            .clone()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            })
    }
}

struct AnthropicProvider {
    config: ProviderConfig,
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let api_key = self.api_key()?;
        let url = format!(
            "{}/messages",
            self.config
                .base_url
                .as_deref()
                .unwrap_or("https://api.anthropic.com/v1")
                .trim_end_matches('/')
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_str(&api_key)?);
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let response = reqwest::Client::new()
            .post(url)
            .headers(headers)
            .json(&json!({
                "model": self.config.model.id,
                "max_tokens": 4096,
                "system": request.system_prompt.unwrap_or_default(),
                "messages": anthropic_messages(&request.messages),
            }))
            .send()
            .await?
            .error_for_status()?
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

impl AnthropicProvider {
    fn api_key(&self) -> Result<String, ProviderError> {
        self.config
            .api_key
            .clone()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            })
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
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
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
}

impl GoogleProvider {
    fn api_key(&self) -> Result<String, ProviderError> {
        self.config
            .api_key
            .clone()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            })
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

fn openai_messages(request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.extend(request.messages.iter().map(|message| {
        json!({
            "role": role_name(&message.role),
            "content": message.content,
        })
    }));
    messages
}

fn anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.role != ChatRole::System)
        .map(|message| {
            json!({
                "role": if message.role == ChatRole::Assistant { "assistant" } else { "user" },
                "content": message.content,
            })
        })
        .collect()
}

fn google_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.role != ChatRole::System)
        .map(|message| {
            json!({
                "role": if message.role == ChatRole::Assistant { "model" } else { "user" },
                "parts": [{ "text": message.content }],
            })
        })
        .collect()
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
            api_key: None,
        });

        let events = provider
            .complete(ProviderRequest {
                system_prompt: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: "hello".to_string(),
                }],
            })
            .await
            .expect("faux provider should complete");

        assert_eq!(
            events[0],
            StreamEvent::Text("[faux/echo] hello".to_string())
        );
    }
}

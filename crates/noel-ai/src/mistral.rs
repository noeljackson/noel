//! Mistral chat.

#[allow(unused_imports)]
use super::*;

pub(crate) fn derive_mistral_tool_call_id(id: &str) -> String {
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

pub(crate) struct MistralProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
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
        if let Some(events) = parse_sse_body::<OpenAiChatCompletionsSseParser>(&body)? {
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
        let Some(body) = stream_sse::<OpenAiChatCompletionsSseParser>(response, on_event).await?
        else {
            return Ok(());
        };
        let response = serde_json::from_str::<Value>(&body)
            .map_err(|_| ProviderError::InvalidResponse(body.clone()))?;
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
    pub(crate) fn api_key(&self) -> Result<String, ProviderError> {
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
pub(crate) fn mistral_chat_url(config: &ProviderConfig) -> Result<String, ProviderError> {
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

pub(crate) fn mistral_headers(
    config: &ProviderConfig,
    api_key: &str,
) -> Result<HeaderMap, ProviderError> {
    let mut headers = bearer_headers(api_key)?;
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(USER_AGENT, HeaderValue::from_str(pi_user_agent())?);
    if let Some(session_id) = &config.session_id {
        headers.insert("x-affinity", HeaderValue::from_str(session_id)?);
    }
    Ok(headers)
}

pub(crate) fn mistral_chat_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
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

pub(crate) fn mistral_messages(config: &ProviderConfig, request: &ProviderRequest) -> Vec<Value> {
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

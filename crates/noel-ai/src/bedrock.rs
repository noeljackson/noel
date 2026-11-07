//! Amazon Bedrock Converse.

#[allow(unused_imports)]
use super::*;

pub(crate) struct BedrockProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
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

pub(crate) fn bedrock_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
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

pub(crate) fn bedrock_tools(tools: &[ToolDefinition]) -> Vec<Value> {
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

pub(crate) fn bedrock_system(
    config: &ProviderConfig,
    system_prompt: Option<&str>,
) -> Option<Value> {
    let system_prompt = system_prompt?;
    let mut blocks = vec![json!({ "text": system_prompt })];
    if bedrock_supports_prompt_cache(&config.model.id) {
        blocks.push(json!({ "cachePoint": { "type": "default" } }));
    }
    Some(Value::Array(blocks))
}

pub(crate) fn bedrock_messages_with_cache(
    config: &ProviderConfig,
    messages: &[ChatMessage],
) -> Vec<Value> {
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

pub(crate) fn bedrock_additional_model_request_fields(config: &ProviderConfig) -> Option<Value> {
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

pub(crate) fn bedrock_is_anthropic_claude(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("claude")
}

pub(crate) fn bedrock_supports_prompt_cache(model_id: &str) -> bool {
    let normalized = model_id.to_ascii_lowercase();
    normalized.contains("-4-")
        || normalized.contains("claude-3-7-sonnet")
        || normalized.contains("claude-3-5-haiku")
}

pub(crate) fn bedrock_supports_adaptive_thinking(model_id: &str) -> bool {
    anthropic_supports_adaptive_thinking(model_id)
}

pub(crate) fn bedrock_adaptive_effort(model_id: &str, level: &str) -> &'static str {
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
    pub(crate) fn bearer_token(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

pub(crate) fn parse_bedrock_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
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

pub(crate) fn parse_bedrock_response_events(response: &Value) -> Option<Vec<StreamEvent>> {
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

pub(crate) fn bedrock_messages(messages: &[ChatMessage]) -> Vec<Value> {
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
pub(crate) fn bedrock_content(message: &ChatMessage) -> Vec<Value> {
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

pub(crate) fn encode_path_segment(value: &str) -> String {
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

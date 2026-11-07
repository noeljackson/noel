//! Anthropic Messages.

#[allow(unused_imports)]
use super::*;

pub(crate) struct AnthropicProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
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

pub(crate) fn anthropic_messages_url(config: &ProviderConfig) -> Result<String, ProviderError> {
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

pub(crate) fn anthropic_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
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
pub(crate) const ANTHROPIC_UNCATALOGED_MAX_TOKENS: u64 = 16_384;

pub(crate) fn anthropic_tools(tools: &[ToolDefinition]) -> Vec<Value> {
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

pub(crate) static CLAUDE_CODE_VERSION_OVERRIDE: std::sync::RwLock<Option<String>> =
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

pub(crate) fn resolve_claude_code_version(
    env_value: Option<&str>,
    override_value: Option<&str>,
) -> String {
    [env_value, override_value]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|version| !version.is_empty())
        .unwrap_or(DEFAULT_CLAUDE_CODE_VERSION)
        .to_string()
}

pub(crate) fn anthropic_headers(config: &ProviderConfig) -> Result<HeaderMap, ProviderError> {
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

pub(crate) fn anthropic_system(config: &ProviderConfig, request: &ProviderRequest) -> Value {
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

pub(crate) fn anthropic_cached_text_block(text: &str) -> Value {
    json!({
        "type": "text",
        "text": text,
        "cache_control": {
            "type": "ephemeral",
        },
    })
}

pub(crate) fn apply_anthropic_thinking(body: &mut Value, config: &ProviderConfig) {
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

pub(crate) fn anthropic_adaptive_effort(model_id: &str, level: &str) -> &'static str {
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

pub(crate) fn parse_anthropic_response_events(response: &Value) -> Option<Vec<StreamEvent>> {
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
pub(crate) struct PendingAnthropicToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Default)]
pub(crate) struct AnthropicSseParser {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) pending_tool_calls: BTreeMap<u64, PendingAnthropicToolCall>,
    pub(crate) stop_reason: Option<String>,
}

impl AnthropicSseParser {
    pub(crate) fn parse_data(
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

pub(crate) fn parse_anthropic_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
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

pub(crate) fn anthropic_messages_with_cache_control(
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

pub(crate) fn anthropic_image_block(media: &MediaInput) -> Value {
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
pub(crate) fn anthropic_tool_result_content(message: &ChatMessage) -> Value {
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

pub(crate) fn apply_anthropic_cache_control_to_last_user_message(messages: &mut [Value]) {
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

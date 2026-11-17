//! OpenAI-style chat completions: OpenAI, Azure, Copilot and compatible APIs.

#[allow(unused_imports)]
use super::*;

pub(crate) struct OpenAiProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
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
        if let Some(events) = parse_sse_body::<OpenAiChatCompletionsSseParser>(&body)? {
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

impl OpenAiProvider {
    pub(crate) async fn complete_with_chatgpt(
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
        if let Some(events) = parse_sse_body::<OpenAiResponsesSseParser>(&body)? {
            return Ok(events);
        }
        openai_responses_json_events(&body)
    }

    pub(crate) async fn complete_with_chatgpt_streaming(
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
        let Some(body) = stream_sse::<OpenAiResponsesSseParser>(response, on_event).await? else {
            return Ok(());
        };
        emit_events(openai_responses_json_events(&body)?, on_event)
    }

    pub(crate) fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }

    pub(crate) fn chat_base_url(&self) -> Result<String, ProviderError> {
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

pub(crate) fn is_azure_provider(provider: &str) -> bool {
    provider == AZURE_PROVIDER || provider == LEGACY_AZURE_PROVIDER
}

pub(crate) fn openai_chat_completions_body(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Value {
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

pub(crate) fn openai_chat_tools(tools: &[ToolDefinition]) -> Vec<Value> {
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

pub(crate) fn openai_chat_messages(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Vec<Value> {
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
            if group_has_images
                && !group_continues
                && let Some(image_message) = pending_images.get(image_group)
            {
                messages.push(image_message.clone());
                image_group += 1;
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
pub(crate) const TOOL_IMAGE_OMITTED: &str =
    "(tool image omitted: this provider API does not accept images in tool results)";

/// What a tool result's images are replaced by for a model whose catalog
/// entry does not list image input.
pub(crate) const MODEL_IMAGE_OMITTED: &str =
    "(tool image omitted: the current model does not support image input)";

/// A tool result for the chat completions API, whose tool messages are text
/// only: the text alone, or with the omission note the model's input
/// modalities call for. The images themselves, when the model accepts them,
/// are sent in the user message [`openai_chat_messages`] adds after the tool
/// results, as upstream does for the models its catalog lists.
pub(crate) fn tool_result_text_without_images(
    config: &ProviderConfig,
    message: &ChatMessage,
) -> String {
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
pub(crate) fn openai_chat_tool_image_groups(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Vec<Value> {
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
pub(crate) fn openai_chat_tool_message(config: &ProviderConfig, message: &ChatMessage) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": message
            .tool_call_id
            .as_deref()
            .map(|id| normalize_openai_chat_tool_call_id(config, id)),
        "content": tool_result_text_without_images(config, message),
    })
}

pub(crate) fn normalize_openai_chat_tool_call_id(config: &ProviderConfig, id: &str) -> String {
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

#[derive(Default)]
pub(crate) struct OpenAiChatCompletionsSseParser {
    pub(crate) tool_calls: Vec<(String, String, String)>,
    pub(crate) stop_reason: Option<String>,
}

/// A chat completion's finish reason as noel's raw stop reason, or the
/// error upstream makes of it (`mapStopReason` in openai-completions.ts):
/// a content filter or a provider error is a failed reply, not a stop.
pub(crate) fn chat_finish_reason(reason: Option<&str>) -> Result<String, ProviderError> {
    match reason {
        None => Err(ProviderError::InvalidResponse(
            "Stream ended without finish_reason".into(),
        )),
        Some(
            reason @ ("stop" | "end" | "length" | "tool_calls" | "function_call" | "model_length"),
        ) => Ok(reason.to_owned()),
        Some(reason) => Err(ProviderError::InvalidResponse(format!(
            "Provider finish_reason: {reason}"
        ))),
    }
}

impl SseParser for OpenAiChatCompletionsSseParser {
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
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            on_event(StreamEvent::Text(text.to_string()))?;
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.tool_calls.len() <= index {
                    self.tool_calls
                        .push((String::new(), String::new(), String::new()));
                }
                // The first id and name stand, as upstream keeps them: a
                // provider may send them again with every chunk.
                let entry = &mut self.tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str)
                    && entry.0.is_empty()
                {
                    entry.0 = id.to_owned();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str)
                    && entry.1.is_empty()
                {
                    entry.1 = name.to_owned();
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
    ) -> Result<String, ProviderError> {
        let reason = chat_finish_reason(self.stop_reason.as_deref())?;
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
        Ok(reason)
    }
}

pub(crate) fn parse_openai_chat_completions_events(response: &Value) -> Option<Vec<StreamEvent>> {
    let message = response.pointer("/choices/0/message")?;
    let mut events = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        events.push(StreamEvent::Text(text.to_string()));
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

//! Google Gemini and Vertex AI.

#[allow(unused_imports)]
use super::*;

pub(crate) struct GoogleProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
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
    pub(crate) fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

pub(crate) fn google_url(config: &ProviderConfig) -> Result<String, ProviderError> {
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

pub(crate) fn google_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert("x-goog-api-key", HeaderValue::from_str(api_key)?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

pub(crate) fn google_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
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

pub(crate) fn google_tools(tools: &[ToolDefinition]) -> Vec<Value> {
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

pub(crate) fn google_thinking_config(config: &ProviderConfig) -> Option<Value> {
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

pub(crate) fn google_default_thinking_budget(model_id: &str, level: &str) -> i64 {
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

pub(crate) struct GoogleVertexProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
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
    pub(crate) fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

pub(crate) fn parse_google_response(response: Value) -> Result<Vec<StreamEvent>, ProviderError> {
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

pub(crate) fn parse_google_response_events(response: &Value) -> Option<Vec<StreamEvent>> {
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

pub(crate) fn parse_google_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
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

pub(crate) fn parse_google_sse_data(
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

pub(crate) fn google_part_events(parts: &[Value]) -> Option<Vec<StreamEvent>> {
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

pub(crate) fn google_vertex_url(config: &ProviderConfig) -> Result<String, ProviderError> {
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

pub(crate) fn google_messages(config: &ProviderConfig, messages: &[ChatMessage]) -> Vec<Value> {
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

pub(crate) fn google_inline_data_part(media: &MediaInput) -> Value {
    json!({
        "inlineData": {
            "mimeType": media.mime_type,
            "data": media.data_base64,
        },
    })
}

pub(crate) fn google_requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || google_gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

pub(crate) fn google_supports_multimodal_function_response(model_id: &str) -> bool {
    google_gemini_major_version(model_id)
        .map(|major| major >= 3)
        .unwrap_or(true)
}

pub(crate) fn google_gemini_major_version(model_id: &str) -> Option<u64> {
    let lower = model_id.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("gemini-live-")
        .or_else(|| lower.strip_prefix("gemini-"))?;
    let version = rest.split('-').next()?;
    let major = version.split('.').next()?;
    major.parse().ok()
}

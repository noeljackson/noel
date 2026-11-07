//! OpenAI Responses: OpenAI, Codex (ChatGPT sign-in), Copilot and Azure.

#[allow(unused_imports)]
use super::*;

/// Where the request's tools go for a Responses transport
/// (`getResponsesToolSupport`): in place as `additional_tools` input items
/// when the model can anchor them mid-conversation, otherwise the
/// top-level `tools` field.
pub(crate) fn responses_tools_in_place(config: &ProviderConfig) -> bool {
    config
        .traits
        .supports_mid_convo_system_messages
        .unwrap_or(false)
        && config.traits.supports_additional_tools.unwrap_or(false)
}

/// The leading `additional_tools` item declaring the request's tools in
/// place, or nothing when the transport keeps them top-level.
pub(crate) fn additional_tools_item(tools: &[ToolDefinition], strict: Value) -> Option<Value> {
    (!tools.is_empty()).then(|| {
        json!({
            "type": "additional_tools",
            "role": "developer",
            "tools": openai_responses_tools(tools, strict),
        })
    })
}

pub(crate) fn openai_responses_body(config: &ProviderConfig, request: &ProviderRequest) -> Value {
    if config.model.provider == "openai-codex" {
        return openai_codex_responses_body(config, request);
    }
    if config.model.provider == "github-copilot" {
        return github_copilot_responses_body(config, request);
    }
    let mut input = openai_responses_input_with_developer_system(request);
    if responses_tools_in_place(config)
        && let Some(item) = additional_tools_item(&request.tools, json!(false))
    {
        input.insert(0, item);
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

pub(crate) fn github_copilot_responses_body(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Value {
    let mut input = openai_responses_input_with_developer_system(request);
    if responses_tools_in_place(config)
        && let Some(item) = additional_tools_item(&request.tools, Value::Null)
    {
        input.insert(0, item);
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

pub(crate) fn openai_codex_responses_body(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Value {
    let mut input = openai_responses_input(request);
    if responses_tools_in_place(config)
        && let Some(item) = additional_tools_item(&request.tools, Value::Null)
    {
        input.insert(0, item);
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

pub(crate) fn openai_responses_tools(tools: &[ToolDefinition], strict: Value) -> Vec<Value> {
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

pub(crate) fn openai_responses_input_with_developer_system(
    request: &ProviderRequest,
) -> Vec<Value> {
    let mut input = Vec::new();
    if let Some(system_prompt) = request.system_prompt.as_deref()
        && !system_prompt.trim().is_empty()
    {
        input.push(json!({
            "role": "developer",
            "content": system_prompt,
        }));
    }
    input.extend(openai_responses_input(request));
    input
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAiResponsesEndpoint {
    OpenAi,
    Codex,
    Azure,
}

pub(crate) struct OpenAiResponsesProvider {
    pub(crate) config: ProviderConfig,
    pub(crate) endpoint: OpenAiResponsesEndpoint,
    pub(crate) client: reqwest::Client,
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
        if let Some(events) = parse_sse_body::<OpenAiResponsesSseParser>(&body)? {
            return Ok(events);
        }
        openai_responses_json_events(&body)
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
        let Some(body) = stream_sse::<OpenAiResponsesSseParser>(response, on_event).await? else {
            return Ok(());
        };
        emit_events(openai_responses_json_events(&body)?, on_event)
    }
}

impl OpenAiResponsesProvider {
    pub(crate) fn request_parts(
        &self,
        has_media: bool,
    ) -> Result<(String, HeaderMap, String), ProviderError> {
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

    pub(crate) fn api_key(&self) -> Result<String, ProviderError> {
        match &self.config.auth {
            ProviderAuth::ApiKey(api_key) => Ok(api_key.clone()),
            _ => Err(ProviderError::MissingApiKey {
                provider: self.config.model.provider.clone(),
            }),
        }
    }
}

pub(crate) fn chatgpt_headers(auth: &ProviderAuth) -> Result<HeaderMap, ProviderError> {
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

pub(crate) fn extract_chatgpt_account_id(token: &str) -> Result<Option<String>, ProviderError> {
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

pub(crate) fn codex_headers(auth: &ProviderAuth) -> Result<HeaderMap, ProviderError> {
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

pub(crate) fn azure_openai_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert("api-key", HeaderValue::from_str(api_key)?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    Ok(headers)
}

pub(crate) fn codex_responses_url(base_url: &str) -> Result<String, ProviderError> {
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

pub(crate) fn azure_openai_base_url(
    config_base_url: Option<&str>,
) -> Result<String, ProviderError> {
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

pub(crate) fn azure_openai_responses_url(base_url: &str) -> String {
    let api_version = env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "v1".to_string());
    format!(
        "{}/responses?api-version={}",
        base_url.trim_end_matches('/'),
        api_version
    )
}

pub(crate) fn azure_deployment_name(model_id: &str) -> String {
    if let Ok(deployment) = env::var("AZURE_OPENAI_DEPLOYMENT_NAME")
        && !deployment.trim().is_empty()
    {
        return deployment;
    }
    if let Ok(map) = env::var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP") {
        for entry in map
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            if let Some((model, deployment)) = entry.split_once('=')
                && model.trim() == model_id
                && !deployment.trim().is_empty()
            {
                return deployment.trim().to_string();
            }
        }
    }
    model_id.to_string()
}

pub(crate) fn openai_responses_input(request: &ProviderRequest) -> Vec<Value> {
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

pub(crate) fn openai_responses_tool_output(message: &ChatMessage) -> Value {
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

pub(crate) fn openai_responses_tool_call_ids(id: &str) -> (String, Option<String>) {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return (normalize_openai_responses_id_part(id), None);
    };
    (
        normalize_openai_responses_id_part(call_id),
        Some(normalize_openai_responses_item_id(item_id)),
    )
}

pub(crate) fn normalize_openai_responses_item_id(id: &str) -> String {
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

pub(crate) fn normalize_openai_responses_id_part(id: &str) -> String {
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

/// A response's end as noel's raw stop reason (upstream's `rawStopReason`,
/// `status` or `status.reason`), or the error upstream makes of it: an
/// incomplete response is an output limit only for `max_output_tokens`.
pub(crate) fn responses_terminal(response: &Value) -> Result<String, ProviderError> {
    let reason = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    match response.get("status").and_then(Value::as_str) {
        None | Some("completed") => Ok("completed".to_owned()),
        Some("incomplete") => match reason {
            Some("max_output_tokens") => Ok("incomplete.max_output_tokens".to_owned()),
            Some(reason) => Err(ProviderError::InvalidResponse(format!(
                "Response incomplete: {reason}"
            ))),
            None => Err(ProviderError::InvalidResponse(
                "Response incomplete without a provider reason".into(),
            )),
        },
        Some(_) => {
            let error = response.get("error").filter(|error| !error.is_null());
            Err(ProviderError::InvalidResponse(match (error, reason) {
                (Some(error), _) => format!(
                    "{}: {}",
                    error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message")
                ),
                (None, Some(reason)) => format!("incomplete: {reason}"),
                (None, None) => "Unknown error (no error details in response)".to_owned(),
            }))
        }
    }
}

/// A plain JSON Responses answer: its events, then its `Stop`.
pub(crate) fn openai_responses_json_events(body: &str) -> Result<Vec<StreamEvent>, ProviderError> {
    let response = serde_json::from_str::<Value>(body)
        .map_err(|_| ProviderError::InvalidResponse(body.to_owned()))?;
    let reason = responses_terminal(&response)?;
    let mut events = parse_openai_responses_events(&response)
        .ok_or_else(|| ProviderError::InvalidResponse(body.to_owned()))?;
    events.push(StreamEvent::Stop { reason });
    Ok(events)
}

pub(crate) fn parse_openai_responses_events(response: &Value) -> Option<Vec<StreamEvent>> {
    if matches!(
        response.get("status").and_then(Value::as_str),
        Some("incomplete" | "failed" | "cancelled")
    ) && responses_terminal(response).is_err()
    {
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
            if has_output_text.is_none()
                && let Some(text) = parse_openai_responses_item_text(item)
            {
                events.push(StreamEvent::Text(text));
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
pub(crate) struct OpenAiResponsesSseParser {
    /// The output items (by `output_index`) whose text streamed as deltas:
    /// their `output_item.done` text is not sent again (#160: per item, as
    /// upstream's output slots, not per response).
    pub(crate) streamed_items: std::collections::BTreeSet<Option<u64>>,
    /// The raw stop reason of the terminal event, once it arrived.
    pub(crate) terminal: Option<String>,
}

impl SseParser for OpenAiResponsesSseParser {
    fn finish(
        self,
        _on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<String, ProviderError> {
        self.terminal.ok_or_else(|| {
            ProviderError::InvalidResponse(
                "OpenAI Responses stream ended before a terminal response event".into(),
            )
        })
    }

    fn parse_data(
        &mut self,
        data: &str,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        match event.get("type").and_then(Value::as_str) {
            Some("error") => {
                return Err(ProviderError::InvalidResponse(format!(
                    "Error Code {}: {}",
                    event
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown error")
                )))
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let response = event.get("response").unwrap_or(&Value::Null);
                let mut response = response.clone();
                if response.get("status").is_none() {
                    // The event's type names the end when its response does not.
                    let status = match event.get("type").and_then(Value::as_str) {
                        Some("response.incomplete") => "incomplete",
                        Some("response.failed") => "failed",
                        _ => "completed",
                    };
                    if let Some(object) = response.as_object_mut() {
                        object.insert("status".into(), Value::from(status));
                    } else {
                        response = serde_json::json!({ "status": status });
                    }
                }
                self.terminal = Some(responses_terminal(&response)?);
                if let Some(usage) = response_usage(&response) {
                    on_event(usage)?;
                }
                return Ok(());
            }
            _ => {}
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
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.streamed_items.insert(output_index(&event));
                    on_event(StreamEvent::Text(delta.to_string()))?;
                }
            }
            Some("response.output_item.done") | Some("output_item.done") => {
                if let Some(item) = event.get("item") {
                    if !self.streamed_items.contains(&output_index(&event))
                        && let Some(text) = parse_openai_responses_item_text(item)
                    {
                        on_event(StreamEvent::Text(text))?;
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

/// The output item an event is about.
fn output_index(event: &Value) -> Option<u64> {
    event.get("output_index").and_then(Value::as_u64)
}

pub(crate) fn parse_openai_responses_function_call(item: &Value) -> Option<StreamEvent> {
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

pub(crate) fn parse_openai_responses_item_text(item: &Value) -> Option<String> {
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

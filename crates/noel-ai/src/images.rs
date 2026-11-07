//! Image generation (OpenRouter's images API).

#[allow(unused_imports)]
use super::*;

pub async fn generate_images(
    config: ImageProviderConfig,
    input: Vec<ImageGenerationInput>,
) -> Result<ImageGenerationResult, ProviderError> {
    match config.api {
        ImageProviderApi::OpenRouterImages => generate_openrouter_images(config, input).await,
    }
}

pub(crate) async fn generate_openrouter_images(
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

pub(crate) fn openrouter_images_body(
    config: &ImageProviderConfig,
    input: &[ImageGenerationInput],
) -> Value {
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

pub(crate) fn parse_openrouter_images_response(
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

pub(crate) fn parse_data_url(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime_type.to_string(), data.to_string()))
}

pub(crate) fn sanitize_surrogates(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '\u{FFFD}')
        .collect()
}

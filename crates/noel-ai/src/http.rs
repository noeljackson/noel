//! What every provider shares: headers, SSE framing, usage, limits.

#[allow(unused_imports)]
use super::*;

/// Tokens upstream keeps free for the request beyond its estimate
/// (`CONTEXT_SAFETY_TOKENS`).
pub(crate) const CONTEXT_SAFETY_TOKENS: u64 = 4_096;

/// Upstream `clampMaxTokensToContext` (`packages/ai/src/api/simple-options.ts`):
/// the model's `maxTokens`, lowered so the estimated input plus the answer
/// fit its context window. `None` for a model without a known output cap.
pub(crate) fn provider_max_tokens(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Option<u64> {
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
pub(crate) fn estimate_request_tokens(request: &ProviderRequest) -> u64 {
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

/// Decode provider-reported request totals; streaming events are cumulative
/// snapshots, not deltas. Never invent estimates when usage is absent.
pub(crate) fn token_usage(usage: &Value) -> Option<StreamEvent> {
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

pub(crate) fn response_usage(response: &Value) -> Option<StreamEvent> {
    response
        .get("usage")
        .or_else(|| response.get("usageMetadata"))
        .and_then(token_usage)
}

pub(crate) fn bearer_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}"))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// Upstream `formatUserAgent()` (`packages/ai/src/utils/identity.ts`) in
/// its default detailed form: `pi/<pi-ai version> (<platform>; node/<node
/// version>; <arch>)`. The version and runtime are pinned to the TS
/// parity reference (`packages/ai/src/utils/version.ts` and the reference
/// image's Node), because they are properties of upstream's build, not of
/// the host running noel; fixtures carry the same values. No kernel release
/// appears anymore, so nothing is host-specific in the detailed form.
pub(crate) fn pi_user_agent() -> &'static str {
    static PI_USER_AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PI_USER_AGENT.get_or_init(|| {
        format_pi_user_agent(node_platform(env::consts::OS), node_arch(env::consts::ARCH))
    })
}

/// The `@earendil-works/pi-ai` version of the TS parity reference, which
/// upstream embeds in the detailed `User-Agent`.
pub(crate) const REFERENCE_PI_AI_VERSION: &str = "1.5.0";
/// The Node version of the TS parity reference image, which upstream
/// embeds as the runtime in the detailed `User-Agent`.
pub(crate) const REFERENCE_NODE_VERSION: &str = "v24.22.2";

pub(crate) fn format_pi_user_agent(platform: &str, arch: &str) -> String {
    format!("pi/{REFERENCE_PI_AI_VERSION} ({platform}; node/{REFERENCE_NODE_VERSION}; {arch})")
}

/// Node's `os.platform()` name for a Rust target OS.
pub(crate) fn node_platform(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" | "illumos" => "sunos",
        other => other,
    }
}

/// Node's `os.arch()` name for a Rust target architecture.
pub(crate) fn node_arch(arch: &str) -> &str {
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

pub(crate) fn openai_compatible_headers(
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

pub(crate) async fn error_for_status_with_body(
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
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
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

pub(crate) struct SseStreamResult {
    pub(crate) body: String,
    pub(crate) saw_data: bool,
}

pub(crate) async fn stream_sse_response(
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

pub(crate) fn process_complete_sse_blocks(
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

pub(crate) fn process_remaining_sse_block(
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

pub(crate) fn process_sse_block(
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

pub(crate) fn sse_block_data(block: &str) -> Option<String> {
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

pub(crate) fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subsequence(buffer, b"\n\n").map(|index| (index, 2));
    let crlf = find_subsequence(buffer, b"\r\n\r\n").map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

pub(crate) fn find_subsequence(buffer: &[u8], needle: &[u8]) -> Option<usize> {
    buffer
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) fn emit_events(
    events: Vec<StreamEvent>,
    on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
) -> Result<(), ProviderError> {
    for event in events {
        on_event(event)?;
    }
    Ok(())
}

pub(crate) fn request_has_media(request: &ProviderRequest) -> bool {
    request
        .messages
        .iter()
        .any(|message| !message.media.is_empty())
}

pub(crate) fn resolve_env_placeholders(value: &str) -> Result<String, ProviderError> {
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

pub(crate) fn short_hash(value: &str) -> String {
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

pub(crate) fn to_base36(mut value: u32) -> String {
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

pub(crate) fn media_data_url(media: &MediaInput) -> String {
    format!("data:{};base64,{}", media.mime_type, media.data_base64)
}

pub(crate) fn role_name(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

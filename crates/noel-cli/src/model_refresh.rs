//! The model list: refreshed from the providers' model APIs into the cache, in the background or on request.

use super::*;

pub(crate) fn start_model_refresh(config: &LoadedConfig, offline: bool, verbose: bool) {
    if !background_refresh_allowed(&config.settings, offline, noel_config::test_isolation()) {
        return;
    }
    let ttl_hours = model_refresh_ttl_hours(&config.settings);
    if !model_cache_needs_refresh(&config.paths.model_cache_path, ttl_hours) {
        return;
    }
    let paths = config.paths.clone();
    let auth = config.auth.clone();
    tokio::spawn(async move {
        if let Err(error) = refresh_model_cache(paths, auth).await {
            if verbose {
                eprintln!("model refresh failed: {error}");
            }
        }
    });
}

/// Background model refresh runs only online and when enabled, and never
/// under the test harness (`NOEL_TEST_ISOLATION`), so tests never call
/// providers with whatever credentials they can see.
pub(crate) fn background_refresh_allowed(
    settings: &Settings,
    offline: bool,
    test_isolation: bool,
) -> bool {
    !offline && !test_isolation && model_refresh_enabled(settings)
}

pub(crate) fn model_refresh_enabled(settings: &Settings) -> bool {
    settings
        .model_refresh
        .as_ref()
        .and_then(|refresh| refresh.enabled)
        .unwrap_or(true)
}

pub(crate) fn model_refresh_ttl_hours(settings: &Settings) -> u64 {
    settings
        .model_refresh
        .as_ref()
        .and_then(|refresh| refresh.ttl_hours)
        .unwrap_or(24)
}

pub(crate) fn model_cache_needs_refresh(path: &Path, ttl_hours: u64) -> bool {
    let Ok(Some(cache)) = read_model_cache(path) else {
        return true;
    };
    model_cache_is_stale(&cache, unix_seconds(), ttl_hours)
}

pub(crate) fn model_cache_is_stale(cache: &ModelCache, now: Option<u64>, ttl_hours: u64) -> bool {
    if cache.version < MODEL_CACHE_VERSION {
        return true;
    }
    let Some(now) = now else {
        return true;
    };
    let ttl_seconds = ttl_hours.saturating_mul(60 * 60);
    now.saturating_sub(cache.refreshed_at) >= ttl_seconds
}

/// Outcome of one model-cache refresh pass. Fetch results are recorded per
/// provider so callers can report exactly what happened instead of guessing
/// from the cache contents.
#[derive(Debug, Default, Clone)]
pub(crate) struct ModelRefreshReport {
    /// Providers whose cached lists were replaced, with fetched model counts.
    pub(crate) refreshed: Vec<(String, usize)>,
    pub(crate) refreshed_providers: BTreeSet<String>,
    pub(crate) refreshed_models: Vec<ModelDefinition>,
    /// Providers that returned an empty list while cached models exist; the
    /// cached models were deliberately kept instead of being wiped.
    pub(crate) kept_cached: Vec<String>,
    /// Providers (or aux fetches) that failed, with the error text.
    pub(crate) failed: Vec<(String, String)>,
    pub(crate) claude_code_version: Option<String>,
}

impl ModelRefreshReport {
    pub(crate) fn record(
        &mut self,
        provider: &str,
        existing: Option<&ModelCache>,
        result: Result<Vec<ModelDefinition>>,
    ) {
        let models = match result {
            Ok(models) => models,
            Err(error) => {
                self.failed
                    .push((provider.to_string(), format!("{error:#}")));
                return;
            }
        };
        // An empty "success" is almost always a truncated or degraded
        // listing. Replacing the provider's cached models with nothing is how
        // newer models silently disappear, so keep the cache as-is.
        if models.is_empty() {
            let cached = existing
                .map(|cache| {
                    cache
                        .models
                        .iter()
                        .filter(|model| model.provider == provider)
                        .count()
                })
                .unwrap_or(0);
            if cached > 0 {
                self.kept_cached.push(provider.to_string());
            } else {
                self.failed
                    .push((provider.to_string(), "returned no models".to_string()));
            }
            return;
        }
        self.refreshed.push((provider.to_string(), models.len()));
        self.refreshed_providers.insert(provider.to_string());
        self.refreshed_models.extend(models);
    }

    pub(crate) fn diagnostics(&self) -> Vec<String> {
        let mut diagnostics = self
            .kept_cached
            .iter()
            .map(|provider| {
                format!("{provider} model refresh returned an empty list; kept cached models")
            })
            .collect::<Vec<_>>();
        diagnostics.extend(
            self.failed
                .iter()
                .map(|(provider, error)| format!("{provider} model refresh failed: {error}")),
        );
        diagnostics
    }

    pub(crate) fn nothing_happened(&self) -> bool {
        self.refreshed.is_empty()
            && self.kept_cached.is_empty()
            && self.failed.is_empty()
            && self.claude_code_version.is_none()
    }
}

pub(crate) async fn refresh_model_cache(
    paths: ConfigPaths,
    auth: noel_config::AuthData,
) -> Result<ModelRefreshReport> {
    let mut report = ModelRefreshReport::default();
    let cached_at_start = read_model_cache(&paths.model_cache_path).ok().flatten();

    if let Some(auth) = auth_for_provider(&auth, "anthropic", None) {
        let result = fetch_anthropic_models(auth).await;
        report.record("anthropic", cached_at_start.as_ref(), result);
    }

    // `openai` lists with its API key and `openai-codex` with its ChatGPT
    // login; neither provider has the other's credential.
    if let Some(ResolvedAuth::ApiKey(api_key)) = auth_for_provider(&auth, "openai", None) {
        let result = fetch_openai_api_models(
            "openai",
            ConfigProviderApi::OpenAiResponses,
            None,
            OPENAI_MODELS_URL,
            &api_key,
        )
        .await;
        report.record("openai", cached_at_start.as_ref(), result);
    }

    if let Some(ResolvedAuth::ChatGptOAuth {
        access_token,
        account_id,
        ..
    }) = auth_for_provider(&auth, "openai-codex", None)
    {
        let result = fetch_chatgpt_backend_models(
            "openai-codex",
            ConfigProviderApi::OpenAiCodexResponses,
            Some("https://chatgpt.com/backend-api".to_string()),
            &access_token,
            account_id.as_deref(),
        )
        .await;
        report.record("openai-codex", cached_at_start.as_ref(), result);
    }

    // The OpenAI-compatible providers whose listings are fetched, each at
    // the base URL the catalog gives its models.
    for provider in ["zai", "zai-coding", "moonshotai", "kimi-coding-openai"] {
        let Some(base_url) = noel_ai::catalog::provider_base_model(provider)
            .and_then(|model| model.base_url.clone())
        else {
            continue;
        };
        let Some(ResolvedAuth::ApiKey(api_key)) = auth_for_provider(&auth, provider, None) else {
            continue;
        };
        let models_url = format!("{base_url}/models");
        let result = fetch_openai_api_models(
            provider,
            ConfigProviderApi::OpenAi,
            Some(base_url),
            &models_url,
            &api_key,
        )
        .await;
        report.record(provider, cached_at_start.as_ref(), result);
    }

    if matches!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ClaudeCodeOAuth { .. })
    ) {
        match fetch_latest_claude_code_version().await {
            Ok(version) => {
                set_claude_code_version(&version);
                report.claude_code_version = Some(version);
            }
            Err(error) => {
                report.failed.push((
                    "claude-code".to_string(),
                    format!("version check failed: {error}"),
                ));
            }
        }
    }

    report.refreshed.sort();
    // Re-read right before writing so a refresh that finished concurrently
    // does not clobber models another process just cached.
    let existing = read_model_cache(&paths.model_cache_path).ok().flatten();
    let Some(next) = next_model_cache(existing.as_ref(), &report, unix_seconds()) else {
        return Ok(report);
    };
    write_model_cache(&paths.model_cache_path, &next)?;
    Ok(report)
}

/// Builds the next cache contents from a refresh report. Returning `None`
/// means there is nothing worth writing (no auth for any refreshable
/// provider). When every fetch failed, the previous `refreshedAt` is kept so
/// the cache stays stale and the next startup retries instead of waiting out
/// another full TTL.
pub(crate) fn next_model_cache(
    existing: Option<&ModelCache>,
    report: &ModelRefreshReport,
    now: Option<u64>,
) -> Option<ModelCache> {
    let existing_refreshed_at = existing.map(|cache| cache.refreshed_at).unwrap_or(0);
    let existing_version = existing.map(|cache| cache.version).unwrap_or(0);
    if report.refreshed_providers.is_empty() {
        if report.nothing_happened() {
            return None;
        }
        return Some(ModelCache {
            // Keep refreshedAt and version stale so the next startup retries
            // instead of waiting out another full TTL.
            refreshed_at: existing_refreshed_at,
            version: existing_version,
            models: existing
                .map(|cache| cache.models.clone())
                .unwrap_or_default(),
            diagnostics: report.diagnostics(),
            claude_code_version: report
                .claude_code_version
                .clone()
                .or_else(|| existing.and_then(|cache| cache.claude_code_version.clone())),
        });
    }
    let mut models = existing
        .map(|cache| cache.models.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|model| !report.refreshed_providers.contains(&model.provider))
        .collect::<Vec<_>>();
    models.extend(report.refreshed_models.iter().cloned());
    Some(ModelCache {
        refreshed_at: now.unwrap_or(existing_refreshed_at),
        version: MODEL_CACHE_VERSION,
        models,
        diagnostics: report.diagnostics(),
        claude_code_version: report
            .claude_code_version
            .clone()
            .or_else(|| existing.and_then(|cache| cache.claude_code_version.clone())),
    })
}

/// Removes the model cache so the next refresh starts from built-in models
/// plus explicit `models.json` entries only. Durable session state is
/// untouched; a bad cache entry disappears immediately.
pub(crate) fn reset_model_cache(path: &Path) -> Result<String> {
    match fs::remove_file(path) {
        Ok(()) => Ok("model cache cleared".to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok("model cache already clear".to_string())
        }
        Err(error) => Err(anyhow!("remove {}: {error}", path.display())),
    }
}

pub(crate) fn format_model_refresh_report(
    report: &ModelRefreshReport,
    config: &LoadedConfig,
) -> String {
    let mut lines = Vec::new();
    if report.refreshed.is_empty() {
        lines.push("no provider model lists refreshed".to_string());
    } else {
        let refreshed = report
            .refreshed
            .iter()
            .map(|(provider, count)| format!("{provider} ({count} models)"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("refreshed: {refreshed}"));
    }
    if !report.kept_cached.is_empty() {
        lines.push(format!(
            "kept cached models after empty response: {}",
            report.kept_cached.join(", ")
        ));
    }
    if !report.failed.is_empty() {
        let failed = report
            .failed
            .iter()
            .map(|(provider, error)| format!("{provider}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(format!("failed: {failed}"));
    }
    lines.push(format!("{} models available", config.models.len()));
    lines.join("\n")
}

pub(crate) fn format_model_refresh_after_reload(
    report: &ModelRefreshReport,
    config: &LoadedConfig,
    reload_report: &ReloadReport,
) -> String {
    let mut output = format_model_refresh_report(report, config);
    if !reload_report.active_model_valid {
        output.push_str("\nactive model is no longer available; use /model <provider/model>");
    }
    output
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicModelsResponse {
    pub(crate) data: Vec<AnthropicModel>,
    #[serde(default)]
    pub(crate) has_more: bool,
    #[serde(default)]
    pub(crate) last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicModel {
    pub(crate) id: String,
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) max_input_tokens: Option<u64>,
}

pub(crate) const ANTHROPIC_MODELS_MAX_PAGES: usize = 16;

pub(crate) fn anthropic_models_url(starting_after: Option<&str>) -> String {
    match starting_after {
        Some(cursor) => {
            format!("https://api.anthropic.com/v1/models?limit=1000&starting_after={cursor}")
        }
        None => "https://api.anthropic.com/v1/models?limit=1000".to_string(),
    }
}

pub(crate) async fn fetch_anthropic_models(auth: ResolvedAuth) -> Result<Vec<ModelDefinition>> {
    let client = reqwest::Client::new();
    let headers = anthropic_model_headers(&auth)?;
    let mut models = Vec::new();
    let mut cursor = None;
    for _ in 0..ANTHROPIC_MODELS_MAX_PAGES {
        let response = client
            .get(anthropic_models_url(cursor.as_deref()))
            .headers(headers.clone())
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("status {status}: {body}"));
        }
        let page = response.json::<AnthropicModelsResponse>().await?;
        let next = if page.has_more {
            page.last_id.clone()
        } else {
            None
        };
        models.extend(page.data.into_iter().map(|model| ModelDefinition {
            context_window: model.max_input_tokens,
            provider: "anthropic".to_string(),
            id: model.id,
            name: Some(model.display_name),
            api: ConfigProviderApi::Anthropic,
            ..ModelDefinition::default()
        }));
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(models),
        }
    }
    // Returning the partial list would replace the cache with a truncated
    // set, so treat an exhausted page budget as a failure and keep the cache.
    Err(anyhow!(
        "pagination exceeded {ANTHROPIC_MODELS_MAX_PAGES} pages"
    ))
}

pub(crate) const CLAUDE_CODE_NPM_LATEST_URL: &str =
    "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";

/// Claude Code publishes to npm; the registry's `latest` dist-tag tells us the
/// version string Anthropic's API expects in the `claude-cli/<version>`
/// user-agent for OAuth-gated models.
pub(crate) async fn fetch_latest_claude_code_version() -> Result<String> {
    let response = reqwest::Client::new()
        .get(CLAUDE_CODE_NPM_LATEST_URL)
        .header(ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status}: {body}"));
    }
    let document = response.json::<serde_json::Value>().await?;
    let version = document
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|version| is_valid_claude_code_version(version))
        .ok_or_else(|| anyhow!("missing version in npm registry response"))?;
    Ok(version.to_string())
}

pub(crate) fn is_valid_claude_code_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 32
        && version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
}

pub(crate) fn anthropic_model_headers(auth: &ResolvedAuth) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    match auth {
        ResolvedAuth::ApiKey(api_key) => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
        }
        ResolvedAuth::ClaudeCodeOAuth { access_token, .. } => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {access_token}"))?,
            );
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("oauth-2025-04-20"),
            );
        }
        ResolvedAuth::ChatGptOAuth { .. } => {
            return Err(anyhow!("unsupported Anthropic auth type"));
        }
    }
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(headers)
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiModelsResponse {
    pub(crate) data: Vec<OpenAiModel>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiModel {
    pub(crate) id: String,
}

pub(crate) const OPENAI_MODELS_URL: &str = "https://api.openai.com/v1/models";

pub(crate) async fn fetch_openai_api_models(
    provider: &str,
    api: ConfigProviderApi,
    base_url: Option<String>,
    models_url: &str,
    api_key: &str,
) -> Result<Vec<ModelDefinition>> {
    let response = reqwest::Client::new()
        .get(models_url)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status}: {body}"));
    }
    let response = response.json::<OpenAiModelsResponse>().await?;
    Ok(response
        .data
        .into_iter()
        .filter(|model| model_supported_for_provider(provider, &model.id))
        .map(|model| ModelDefinition {
            provider: provider.to_string(),
            id: model.id.clone(),
            name: Some(model_display_name(&model.id)),
            api: api.clone(),
            base_url: base_url.clone(),
            ..ModelDefinition::default()
        })
        .collect())
}

pub(crate) async fn fetch_chatgpt_backend_models(
    provider: &str,
    api: ConfigProviderApi,
    base_url: Option<String>,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<ModelDefinition>> {
    let endpoint = format!(
        "https://chatgpt.com/backend-api/codex/models?client_version={}",
        codex_client_version().unwrap_or_else(|| "0.145.0".to_string())
    );
    let response = reqwest::Client::new()
        .get(&endpoint)
        .headers(chatgpt_model_headers(access_token, account_id)?)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status} from {endpoint}: {body}"));
    }
    let document = response.json::<serde_json::Value>().await?;
    let ids = collect_codex_model_ids(&document, provider);
    if ids.is_empty() {
        return Err(anyhow!(
            "no selectable {provider} model IDs in response from {endpoint}"
        ));
    }
    Ok(ids
        .into_iter()
        .map(|id| ModelDefinition {
            context_window: document
                .get("models")
                .and_then(serde_json::Value::as_array)
                .and_then(|models| {
                    models.iter().find(|model| {
                        model
                            .get("slug")
                            .or_else(|| model.get("id"))
                            .and_then(serde_json::Value::as_str)
                            == Some(id.as_str())
                    })
                })
                .and_then(|model| {
                    model
                        .get("context_window")
                        .or_else(|| model.get("contextWindow"))
                })
                .and_then(serde_json::Value::as_u64),
            provider: provider.to_string(),
            name: Some(model_display_name(&id)),
            id,
            api: api.clone(),
            base_url: base_url.clone(),
            ..ModelDefinition::default()
        })
        .collect())
}

pub(crate) fn chatgpt_model_headers(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}"))?,
    );
    if let Some(account_id) = account_id {
        headers.insert("chatgpt-account-id", HeaderValue::from_str(account_id)?);
    }
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert("User-Agent", HeaderValue::from_static("pi"));
    headers.insert(
        "OpenAI-Beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

pub(crate) fn collect_codex_model_ids(
    value: &serde_json::Value,
    provider: &str,
) -> BTreeSet<String> {
    let models = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array());
    models
        .into_iter()
        .flatten()
        .filter(|model| {
            model
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .map(|visibility| visibility == "list")
                .unwrap_or(true)
        })
        .filter_map(|model| {
            ["slug", "id", "model"]
                .into_iter()
                .find_map(|key| model.get(key).and_then(serde_json::Value::as_str))
        })
        .filter(|id| model_supported_for_provider(provider, id))
        .map(ToString::to_string)
        .collect()
}

pub(crate) fn model_supported_for_provider(provider: &str, id: &str) -> bool {
    let excludes_non_chat = || {
        id.contains("audio")
            || id.contains("realtime")
            || id.contains("transcribe")
            || id.contains("tts")
            || id.contains("image")
            || id.contains("moderation")
            || id.contains("embedding")
    };
    match provider {
        "openai" => {
            (id.starts_with("gpt-")
                || id.starts_with("o1")
                || id.starts_with("o3")
                || id.starts_with("o4"))
                && !excludes_non_chat()
        }
        "openai-codex" => (id.starts_with("gpt-") || id.contains("codex")) && !excludes_non_chat(),
        "zai" | "zai-coding" | "moonshotai" | "kimi-coding-openai" => !excludes_non_chat(),
        _ => false,
    }
}

pub(crate) fn model_display_name(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if part.chars().all(|ch| ch.is_ascii_digit() || ch == '.') => {
                    format!("{first}{}", chars.collect::<String>())
                }
                Some(first) => format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    chars.collect::<String>()
                ),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const APP_NAME: &str = "pi";
pub const CONFIG_DIR_NAME: &str = ".pi";
pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("home directory is not available")]
    HomeUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigPaths {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_dir: PathBuf,
    pub settings_path: PathBuf,
    pub project_settings_path: PathBuf,
    pub auth_path: PathBuf,
    pub models_path: PathBuf,
    pub keybindings_path: PathBuf,
}

impl ConfigPaths {
    pub fn discover(
        cwd: impl Into<PathBuf>,
        session_dir_override: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        let cwd = cwd.into();
        let agent_dir = match env::var_os(ENV_AGENT_DIR) {
            Some(value) => expand_tilde(PathBuf::from(value))?,
            None => home_dir()?.join(CONFIG_DIR_NAME).join("agent"),
        };
        let session_dir = match session_dir_override {
            Some(value) => expand_tilde(value)?,
            None => match env::var_os(ENV_SESSION_DIR) {
                Some(value) => expand_tilde(PathBuf::from(value))?,
                None => agent_dir.join("sessions"),
            },
        };

        Ok(Self {
            cwd: cwd.clone(),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(CONFIG_DIR_NAME).join("settings.json"),
            auth_path: agent_dir.join("auth.json"),
            models_path: agent_dir.join("models.json"),
            keybindings_path: agent_dir.join("keybindings.json"),
            agent_dir,
            session_dir,
        })
    }

    pub fn with_session_dir(&self, session_dir: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let mut next = self.clone();
        let path = expand_tilde(session_dir.into())?;
        next.session_dir = if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        };
        Ok(next)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    pub transport: Option<String>,
    pub steering_mode: Option<String>,
    pub follow_up_mode: Option<String>,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub append_system_prompt: Vec<String>,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub hide_thinking_block: Option<bool>,
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub enabled_models: Option<Vec<String>>,
    pub theme: Option<String>,
    #[serde(default)]
    pub quiet_startup: Option<bool>,
    #[serde(default)]
    pub session_dir: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub themes: Vec<String>,
    #[serde(default)]
    pub compaction: Option<CompactionSettings>,
    #[serde(default)]
    pub branch_summary: Option<BranchSummarySettings>,
    #[serde(default)]
    pub retry: Option<RetrySettings>,
    #[serde(default)]
    pub terminal: Option<TerminalSettings>,
    #[serde(default)]
    pub images: Option<ImageSettings>,
    pub double_escape_action: Option<String>,
    pub tree_filter_mode: Option<String>,
    #[serde(default)]
    pub thinking_budgets: Option<ThinkingBudgetsSettings>,
    pub editor_padding_x: Option<u16>,
    pub autocomplete_max_visible: Option<u16>,
    pub show_hardware_cursor: Option<bool>,
    #[serde(default)]
    pub markdown: Option<MarkdownSettings>,
    #[serde(default)]
    pub warnings: Option<WarningSettings>,
}

impl Settings {
    fn merge(self, overrides: Settings) -> Settings {
        Settings {
            default_provider: overrides.default_provider.or(self.default_provider),
            default_model: overrides.default_model.or(self.default_model),
            default_thinking_level: overrides
                .default_thinking_level
                .or(self.default_thinking_level),
            transport: overrides.transport.or(self.transport),
            steering_mode: overrides.steering_mode.or(self.steering_mode),
            follow_up_mode: overrides.follow_up_mode.or(self.follow_up_mode),
            system_prompt: overrides.system_prompt.or(self.system_prompt),
            append_system_prompt: if overrides.append_system_prompt.is_empty() {
                self.append_system_prompt
            } else {
                overrides.append_system_prompt
            },
            shell_path: overrides.shell_path.or(self.shell_path),
            shell_command_prefix: overrides.shell_command_prefix.or(self.shell_command_prefix),
            hide_thinking_block: overrides.hide_thinking_block.or(self.hide_thinking_block),
            enabled_tools: overrides.enabled_tools.or(self.enabled_tools),
            enabled_models: overrides.enabled_models.or(self.enabled_models),
            theme: overrides.theme.or(self.theme),
            quiet_startup: overrides.quiet_startup.or(self.quiet_startup),
            session_dir: overrides.session_dir.or(self.session_dir),
            packages: if overrides.packages.is_empty() {
                self.packages
            } else {
                overrides.packages
            },
            extensions: if overrides.extensions.is_empty() {
                self.extensions
            } else {
                overrides.extensions
            },
            skills: if overrides.skills.is_empty() {
                self.skills
            } else {
                overrides.skills
            },
            prompts: if overrides.prompts.is_empty() {
                self.prompts
            } else {
                overrides.prompts
            },
            themes: if overrides.themes.is_empty() {
                self.themes
            } else {
                overrides.themes
            },
            compaction: merge_optional_settings(self.compaction, overrides.compaction),
            branch_summary: merge_optional_settings(self.branch_summary, overrides.branch_summary),
            retry: merge_optional_settings(self.retry, overrides.retry),
            terminal: merge_optional_settings(self.terminal, overrides.terminal),
            images: merge_optional_settings(self.images, overrides.images),
            double_escape_action: overrides.double_escape_action.or(self.double_escape_action),
            tree_filter_mode: overrides.tree_filter_mode.or(self.tree_filter_mode),
            thinking_budgets: merge_optional_settings(
                self.thinking_budgets,
                overrides.thinking_budgets,
            ),
            editor_padding_x: overrides.editor_padding_x.or(self.editor_padding_x),
            autocomplete_max_visible: overrides
                .autocomplete_max_visible
                .or(self.autocomplete_max_visible),
            show_hardware_cursor: overrides.show_hardware_cursor.or(self.show_hardware_cursor),
            markdown: merge_optional_settings(self.markdown, overrides.markdown),
            warnings: merge_optional_settings(self.warnings, overrides.warnings),
        }
    }
}

trait MergeSettings {
    fn merge(self, overrides: Self) -> Self;
}

fn merge_optional_settings<T: MergeSettings>(base: Option<T>, overrides: Option<T>) -> Option<T> {
    match (base, overrides) {
        (Some(base), Some(overrides)) => Some(base.merge(overrides)),
        (None, Some(overrides)) => Some(overrides),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    pub enabled: Option<bool>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
}

impl MergeSettings for CompactionSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            enabled: overrides.enabled.or(self.enabled),
            reserve_tokens: overrides.reserve_tokens.or(self.reserve_tokens),
            keep_recent_tokens: overrides.keep_recent_tokens.or(self.keep_recent_tokens),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    pub reserve_tokens: Option<u64>,
    pub skip_prompt: Option<bool>,
}

impl MergeSettings for BranchSummarySettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            reserve_tokens: overrides.reserve_tokens.or(self.reserve_tokens),
            skip_prompt: overrides.skip_prompt.or(self.skip_prompt),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetrySettings {
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u64>,
    pub max_retry_delay_ms: Option<u64>,
}

impl MergeSettings for ProviderRetrySettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            timeout_ms: overrides.timeout_ms.or(self.timeout_ms),
            max_retries: overrides.max_retries.or(self.max_retries),
            max_retry_delay_ms: overrides.max_retry_delay_ms.or(self.max_retry_delay_ms),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    pub enabled: Option<bool>,
    pub max_retries: Option<u64>,
    pub base_delay_ms: Option<u64>,
    pub provider: Option<ProviderRetrySettings>,
}

impl MergeSettings for RetrySettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            enabled: overrides.enabled.or(self.enabled),
            max_retries: overrides.max_retries.or(self.max_retries),
            base_delay_ms: overrides.base_delay_ms.or(self.base_delay_ms),
            provider: merge_optional_settings(self.provider, overrides.provider),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    pub show_images: Option<bool>,
    pub image_width_cells: Option<u16>,
    pub clear_on_shrink: Option<bool>,
    pub show_terminal_progress: Option<bool>,
}

impl MergeSettings for TerminalSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            show_images: overrides.show_images.or(self.show_images),
            image_width_cells: overrides.image_width_cells.or(self.image_width_cells),
            clear_on_shrink: overrides.clear_on_shrink.or(self.clear_on_shrink),
            show_terminal_progress: overrides
                .show_terminal_progress
                .or(self.show_terminal_progress),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    pub auto_resize: Option<bool>,
    pub block_images: Option<bool>,
}

impl MergeSettings for ImageSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            auto_resize: overrides.auto_resize.or(self.auto_resize),
            block_images: overrides.block_images.or(self.block_images),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgetsSettings {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

impl MergeSettings for ThinkingBudgetsSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            minimal: overrides.minimal.or(self.minimal),
            low: overrides.low.or(self.low),
            medium: overrides.medium.or(self.medium),
            high: overrides.high.or(self.high),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSettings {
    pub code_block_indent: Option<String>,
}

impl MergeSettings for MarkdownSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            code_block_indent: overrides.code_block_indent.or(self.code_block_indent),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningSettings {
    pub anthropic_extra_usage: Option<bool>,
}

impl MergeSettings for WarningSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            anthropic_extra_usage: overrides
                .anthropic_extra_usage
                .or(self.anthropic_extra_usage),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthCredential {
    ApiKey {
        key: String,
    },
    OAuth {
        access_token: String,
        expires: u64,
        #[serde(default)]
        account_id: Option<String>,
    },
}

pub type AuthData = BTreeMap<String, AuthCredential>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAuth {
    ApiKey(String),
    ClaudeCodeOAuth {
        access_token: String,
    },
    ChatGptOAuth {
        access_token: String,
        account_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api: ProviderApi,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderApi {
    #[serde(rename = "openai-completions", alias = "open-ai")]
    #[default]
    OpenAi,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-codex-responses")]
    OpenAiCodexResponses,
    #[serde(rename = "azure-openai-responses")]
    AzureOpenAiResponses,
    #[serde(rename = "anthropic-messages", alias = "anthropic")]
    Anthropic,
    #[serde(rename = "google-generative-ai", alias = "google")]
    Google,
    #[serde(rename = "google-vertex")]
    GoogleVertex,
    #[serde(rename = "bedrock-converse-stream", alias = "amazon-bedrock")]
    Bedrock,
    #[serde(rename = "mistral-conversations", alias = "mistral")]
    Mistral,
    #[serde(rename = "faux")]
    Faux,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingConfig {
    pub action: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum KeybindingsFile {
    List(Vec<KeybindingConfig>),
    Map(BTreeMap<String, Vec<String>>),
}

impl KeybindingsFile {
    fn into_keybindings(self) -> Vec<KeybindingConfig> {
        match self {
            KeybindingsFile::List(bindings) => bindings,
            KeybindingsFile::Map(bindings) => bindings
                .into_iter()
                .map(|(action, keys)| KeybindingConfig { action, keys })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub paths: ConfigPaths,
    pub settings: Settings,
    pub auth: AuthData,
    pub models: Vec<ModelDefinition>,
    pub keybindings: Vec<KeybindingConfig>,
    pub context_files: Vec<ContextFile>,
    pub extensions: Vec<ResourceFile>,
    pub skills: Vec<ResourceFile>,
    pub prompt_templates: Vec<ResourceFile>,
    pub themes: Vec<ResourceFile>,
    pub diagnostics: Vec<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceFile {
    pub name: String,
    pub path: PathBuf,
    pub content: String,
}

pub fn load_config(paths: ConfigPaths) -> Result<LoadedConfig, ConfigError> {
    let global_settings = read_optional_json::<Settings>(&paths.settings_path)?;
    let project_settings = read_optional_json::<Settings>(&paths.project_settings_path)?;
    let global_settings = global_settings.unwrap_or_default();
    let project_settings = project_settings.unwrap_or_default();
    let settings = global_settings.clone().merge(project_settings.clone());
    let auth = read_optional_json::<AuthData>(&paths.auth_path)?.unwrap_or_default();
    let models = filter_enabled_models(
        read_optional_json::<Vec<ModelDefinition>>(&paths.models_path)?
            .unwrap_or_else(default_models),
        settings.enabled_models.as_deref(),
    );
    let keybindings = read_optional_json::<KeybindingsFile>(&paths.keybindings_path)?
        .map(KeybindingsFile::into_keybindings)
        .unwrap_or_default();
    let mut diagnostics = Vec::new();
    let context_files = load_context_files(&paths.cwd, &paths.agent_dir)?;
    let mut extensions = load_resource_files(&paths, "extensions", &mut diagnostics);
    let mut skills = load_resource_files(&paths, "skills", &mut diagnostics);
    let mut prompt_templates = load_resource_files(&paths, "prompts", &mut diagnostics);
    let mut themes = load_resource_files(&paths, "themes", &mut diagnostics);
    extend_resource_files(
        &mut extensions,
        &paths.agent_dir,
        &global_settings.extensions,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut skills,
        &paths.agent_dir,
        &global_settings.skills,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut prompt_templates,
        &paths.agent_dir,
        &global_settings.prompts,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut themes,
        &paths.agent_dir,
        &global_settings.themes,
        &mut diagnostics,
    );
    let project_base = paths.cwd.join(CONFIG_DIR_NAME);
    extend_resource_files(
        &mut extensions,
        &project_base,
        &project_settings.extensions,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut skills,
        &project_base,
        &project_settings.skills,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut prompt_templates,
        &project_base,
        &project_settings.prompts,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut themes,
        &project_base,
        &project_settings.themes,
        &mut diagnostics,
    );
    load_package_resources(
        &paths.cwd,
        &settings.packages,
        &mut extensions,
        &mut skills,
        &mut prompt_templates,
        &mut themes,
        &mut diagnostics,
    );
    let system_prompt = resolve_prompt_input(settings.system_prompt.as_deref())?;
    let append_system_prompt = settings
        .append_system_prompt
        .iter()
        .map(|source| {
            resolve_prompt_input(Some(source))
                .map(|content| content.unwrap_or_else(|| source.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(LoadedConfig {
        paths,
        settings,
        auth,
        models,
        keybindings,
        context_files,
        extensions,
        skills,
        prompt_templates,
        themes,
        diagnostics,
        system_prompt,
        append_system_prompt,
    })
}

fn filter_enabled_models(
    models: Vec<ModelDefinition>,
    enabled_models: Option<&[String]>,
) -> Vec<ModelDefinition> {
    let Some(enabled_models) = enabled_models else {
        return models;
    };
    models
        .into_iter()
        .filter(|model| {
            enabled_models.iter().any(|enabled| {
                enabled == &model.id
                    || enabled == &format!("{}/{}", model.provider, model.id)
                    || model.name.as_deref() == Some(enabled.as_str())
            })
        })
        .collect()
}

pub fn api_key_for_provider(auth: &AuthData, provider: &str) -> Option<String> {
    match auth_for_provider(auth, provider) {
        Some(ResolvedAuth::ApiKey(api_key)) => Some(api_key),
        _ => None,
    }
}

pub fn auth_for_provider(auth: &AuthData, provider: &str) -> Option<ResolvedAuth> {
    if let Some(credential) = auth.get(provider) {
        return match credential {
            AuthCredential::ApiKey { key } => Some(ResolvedAuth::ApiKey(resolve_config_value(key))),
            AuthCredential::OAuth {
                access_token,
                account_id,
                ..
            } => Some(oauth_for_provider(
                provider,
                resolve_config_value(access_token),
                account_id.clone(),
            )),
        };
    }
    env_auth(provider).or_else(|| login_auth(provider))
}

pub fn has_auth_for_provider(auth: &AuthData, provider: &str) -> bool {
    auth_for_provider(auth, provider).is_some()
}

fn env_api_key(provider: &str) -> Option<String> {
    let names = match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"][..],
        "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"][..],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"][..],
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => &["CLOUDFLARE_API_KEY"][..],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"][..],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"][..],
        "openai" => &["OPENAI_API_KEY"],
        "openai-codex" => &["CODEX_API_KEY"][..],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        _ => &[],
    };
    names.iter().find_map(|name| env::var(name).ok())
}

fn env_auth(provider: &str) -> Option<ResolvedAuth> {
    if let Some(api_key) = env_api_key(provider) {
        return Some(ResolvedAuth::ApiKey(api_key));
    }
    match provider {
        "anthropic" => read_non_empty_env("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| read_non_empty_env("CLAUDE_CODE_OAUTH_TOKEN"))
            .map(|access_token| ResolvedAuth::ClaudeCodeOAuth { access_token }),
        "openai" => read_non_empty_env("CODEX_API_KEY")
            .map(ResolvedAuth::ApiKey)
            .or_else(|| {
                read_non_empty_env("CODEX_ACCESS_TOKEN").map(|access_token| {
                    ResolvedAuth::ChatGptOAuth {
                        access_token,
                        account_id: read_non_empty_env("CHATGPT_ACCOUNT_ID"),
                    }
                })
            }),
        "openai-codex" => read_non_empty_env("CODEX_ACCESS_TOKEN").map(|access_token| {
            ResolvedAuth::ChatGptOAuth {
                access_token,
                account_id: read_non_empty_env("CHATGPT_ACCOUNT_ID"),
            }
        }),
        _ => None,
    }
}

fn login_auth(provider: &str) -> Option<ResolvedAuth> {
    match provider {
        "anthropic" => read_claude_code_oauth(),
        "openai" | "openai-codex" => read_codex_chatgpt_oauth(),
        _ => None,
    }
}

fn oauth_for_provider(
    provider: &str,
    access_token: String,
    account_id: Option<String>,
) -> ResolvedAuth {
    match provider {
        "anthropic" => ResolvedAuth::ClaudeCodeOAuth { access_token },
        "openai" => ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
        },
        "openai-codex" => ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
        },
        _ => ResolvedAuth::ApiKey(access_token),
    }
}

fn resolve_config_value(value: &str) -> String {
    if let Some(name) = value.strip_prefix("env:") {
        return env::var(name).unwrap_or_default();
    }
    value.to_string()
}

fn read_non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCredentialsFile {
    claude_ai_oauth: Option<ClaudeAiOAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAiOAuth {
    access_token: String,
}

fn read_claude_code_oauth() -> Option<ResolvedAuth> {
    let path = home_dir().ok()?.join(".claude").join(".credentials.json");
    read_claude_code_oauth_from_path(&path)
}

fn read_claude_code_oauth_from_path(path: &Path) -> Option<ResolvedAuth> {
    let credentials = read_optional_json::<ClaudeCredentialsFile>(path)
        .ok()
        .flatten()?;
    let access_token = credentials.claude_ai_oauth?.access_token;
    if access_token.trim().is_empty() {
        return None;
    }
    Some(ResolvedAuth::ClaudeCodeOAuth { access_token })
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexAuthTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

fn read_codex_chatgpt_oauth() -> Option<ResolvedAuth> {
    let path = home_dir().ok()?.join(".codex").join("auth.json");
    read_codex_chatgpt_oauth_from_path(&path)
}

fn read_codex_chatgpt_oauth_from_path(path: &Path) -> Option<ResolvedAuth> {
    let auth = read_optional_json::<CodexAuthFile>(path).ok().flatten()?;
    let tokens = auth.tokens?;
    if tokens.access_token.trim().is_empty() {
        return None;
    }
    Some(ResolvedAuth::ChatGptOAuth {
        access_token: tokens.access_token,
        account_id: tokens.account_id,
    })
}

fn read_optional_json<T>(path: &Path) -> Result<Option<T>, ConfigError>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

fn resolve_prompt_input(input: Option<&str>) -> Result<Option<String>, ConfigError> {
    let Some(input) = input else {
        return Ok(None);
    };
    let path = Path::new(input);
    if path.exists() {
        return fs::read_to_string(path)
            .map(Some)
            .map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
    }
    Ok(Some(input.to_string()))
}

fn load_context_files(cwd: &Path, agent_dir: &Path) -> Result<Vec<ContextFile>, ConfigError> {
    let mut result = Vec::new();
    if let Some(file) = load_context_file_from_dir(agent_dir)? {
        result.push(file);
    }

    let mut ancestors = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if let Some(file) = load_context_file_from_dir(dir)? {
            ancestors.push(file);
        }
        current = dir.parent();
    }
    ancestors.reverse();
    result.extend(ancestors);
    Ok(result)
}

fn load_context_file_from_dir(dir: &Path) -> Result<Option<ContextFile>, ConfigError> {
    for filename in ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"] {
        let path = dir.join(filename);
        if path.exists() {
            let content = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            return Ok(Some(ContextFile { path, content }));
        }
    }
    Ok(None)
}

fn load_resource_files(
    paths: &ConfigPaths,
    resource_dir_name: &str,
    diagnostics: &mut Vec<String>,
) -> Vec<ResourceFile> {
    let dirs = [
        paths.agent_dir.join(resource_dir_name),
        paths.cwd.join(CONFIG_DIR_NAME).join(resource_dir_name),
    ];
    let mut resources = BTreeMap::<String, ResourceFile>::new();
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(format!("failed to read {}: {error}", dir.display()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    diagnostics.push(format!(
                        "failed to read entry in {}: {error}",
                        dir.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            match fs::read_to_string(&path) {
                Ok(content) => {
                    resources.insert(
                        stem.to_string(),
                        ResourceFile {
                            name: stem.to_string(),
                            path,
                            content,
                        },
                    );
                }
                Err(error) => {
                    diagnostics.push(format!("failed to read {}: {error}", path.display()));
                }
            }
        }
    }
    resources.into_values().collect()
}

fn extend_resource_files(
    resources: &mut Vec<ResourceFile>,
    base_dir: &Path,
    entries: &[String],
    diagnostics: &mut Vec<String>,
) {
    let mut map = resources
        .drain(..)
        .map(|resource| (resource.name.clone(), resource))
        .collect::<BTreeMap<_, _>>();
    for entry in entries {
        let path = match resolve_resource_path(base_dir, entry) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!("failed to resolve resource path {entry}: {error}"));
                continue;
            }
        };
        collect_resource_path(&mut map, &path, diagnostics);
    }
    resources.extend(map.into_values());
}

fn load_package_resources(
    cwd: &Path,
    packages: &[String],
    extensions: &mut Vec<ResourceFile>,
    skills: &mut Vec<ResourceFile>,
    prompts: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) {
    for package in packages {
        let package_root = match resolve_resource_path(cwd, package) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!("failed to resolve package {package}: {error}"));
                continue;
            }
        };
        if package_root.is_file() {
            extend_resource_files(extensions, cwd, std::slice::from_ref(package), diagnostics);
            continue;
        }
        if !package_root.is_dir() {
            diagnostics.push(format!(
                "package path not found: {}",
                package_root.display()
            ));
            continue;
        }
        if load_manifest_package_resources(
            &package_root,
            extensions,
            skills,
            prompts,
            themes,
            diagnostics,
        ) {
            continue;
        }
        extend_resource_files(
            extensions,
            &package_root,
            &["extensions".to_string()],
            diagnostics,
        );
        extend_resource_files(skills, &package_root, &["skills".to_string()], diagnostics);
        extend_resource_files(
            prompts,
            &package_root,
            &["prompts".to_string()],
            diagnostics,
        );
        extend_resource_files(themes, &package_root, &["themes".to_string()], diagnostics);
    }
}

fn load_manifest_package_resources(
    package_root: &Path,
    extensions: &mut Vec<ResourceFile>,
    skills: &mut Vec<ResourceFile>,
    prompts: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) -> bool {
    let path = package_root.join("package.json");
    if !path.exists() {
        return false;
    }
    let manifest = match read_optional_json::<PackageJson>(&path) {
        Ok(Some(package_json)) => package_json.pi,
        Ok(None) => None,
        Err(error) => {
            diagnostics.push(format!(
                "failed to read package manifest {}: {error}",
                path.display()
            ));
            None
        }
    };
    let Some(manifest) = manifest else {
        return false;
    };
    extend_resource_files(extensions, package_root, &manifest.extensions, diagnostics);
    extend_resource_files(skills, package_root, &manifest.skills, diagnostics);
    extend_resource_files(prompts, package_root, &manifest.prompts, diagnostics);
    extend_resource_files(themes, package_root, &manifest.themes, diagnostics);
    true
}

#[derive(Debug, Default, Deserialize)]
struct PackageJson {
    pi: Option<PackageManifest>,
}

#[derive(Debug, Default, Deserialize)]
struct PackageManifest {
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    prompts: Vec<String>,
    #[serde(default)]
    themes: Vec<String>,
}

fn resolve_resource_path(base_dir: &Path, entry: &str) -> Result<PathBuf, ConfigError> {
    let path = expand_tilde(PathBuf::from(entry))?;
    Ok(if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    })
}

fn collect_resource_path(
    resources: &mut BTreeMap<String, ResourceFile>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    if path.is_file() {
        push_resource_file(resources, path, diagnostics);
        return;
    }
    if !path.is_dir() {
        diagnostics.push(format!("resource path not found: {}", path.display()));
        return;
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(format!("failed to read {}: {error}", path.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(format!(
                    "failed to read entry in {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let child = entry.path();
        if child.is_dir() {
            collect_resource_path(resources, &child, diagnostics);
        } else if child.is_file() {
            push_resource_file(resources, &child, diagnostics);
        }
    }
}

fn push_resource_file(
    resources: &mut BTreeMap<String, ResourceFile>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
        return;
    };
    match fs::read_to_string(path) {
        Ok(content) => {
            resources.insert(
                stem.to_string(),
                ResourceFile {
                    name: stem.to_string(),
                    path: path.to_path_buf(),
                    content,
                },
            );
        }
        Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
    }
}

fn default_models() -> Vec<ModelDefinition> {
    vec![
        ModelDefinition {
            provider: "faux".to_string(),
            id: "echo".to_string(),
            name: Some("Faux Echo".to_string()),
            api: ProviderApi::Faux,
            base_url: None,
        },
        ModelDefinition {
            provider: "openai".to_string(),
            id: "gpt-5.4".to_string(),
            name: Some("OpenAI GPT 5.4".to_string()),
            api: ProviderApi::OpenAiResponses,
            base_url: None,
        },
        ModelDefinition {
            provider: "openai".to_string(),
            id: "gpt-4.1".to_string(),
            name: Some("OpenAI GPT 4.1".to_string()),
            api: ProviderApi::OpenAi,
            base_url: None,
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.2-codex".to_string(),
            name: Some("OpenAI Codex GPT 5.2".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "azure-openai-responses".to_string(),
            id: "gpt-5.2".to_string(),
            name: Some("Azure OpenAI GPT 5.2".to_string()),
            api: ProviderApi::AzureOpenAiResponses,
            base_url: None,
        },
        ModelDefinition {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-5".to_string(),
            name: Some("Claude Sonnet".to_string()),
            api: ProviderApi::Anthropic,
            base_url: None,
        },
        ModelDefinition {
            provider: "github-copilot".to_string(),
            id: "gpt-5.4".to_string(),
            name: Some("GitHub Copilot GPT 5.4".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.individual.githubcopilot.com".to_string()),
        },
        ModelDefinition {
            provider: "openrouter".to_string(),
            id: "moonshotai/kimi-k2.6".to_string(),
            name: Some("OpenRouter Kimi K2.6".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
        },
        ModelDefinition {
            provider: "google".to_string(),
            id: "gemini-2.5-pro".to_string(),
            name: Some("Gemini Pro".to_string()),
            api: ProviderApi::Google,
            base_url: None,
        },
        ModelDefinition {
            provider: "google-vertex".to_string(),
            id: "gemini-2.5-pro".to_string(),
            name: Some("Gemini Pro Vertex".to_string()),
            api: ProviderApi::GoogleVertex,
            base_url: Some("https://{GOOGLE_CLOUD_LOCATION}-aiplatform.googleapis.com".to_string()),
        },
        ModelDefinition {
            provider: "amazon-bedrock".to_string(),
            id: "us.anthropic.claude-opus-4-6-v1".to_string(),
            name: Some("Bedrock Claude Opus 4.6".to_string()),
            api: ProviderApi::Bedrock,
            base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string()),
        },
        ModelDefinition {
            provider: "mistral".to_string(),
            id: "devstral-medium-latest".to_string(),
            name: Some("Mistral Devstral Medium".to_string()),
            api: ProviderApi::Mistral,
            base_url: Some("https://api.mistral.ai/v1".to_string()),
        },
        ModelDefinition {
            provider: "cloudflare-workers-ai".to_string(),
            id: "@cf/moonshotai/kimi-k2.6".to_string(),
            name: Some("Cloudflare Workers AI Kimi K2.6".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some(
                "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1"
                    .to_string(),
            ),
        },
        ModelDefinition {
            provider: "cloudflare-ai-gateway".to_string(),
            id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
            name: Some("Cloudflare AI Gateway Kimi K2.6".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some(
                "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
                    .to_string(),
            ),
        },
    ]
}

fn expand_tilde(path: PathBuf) -> Result<PathBuf, ConfigError> {
    let value = path.to_string_lossy();
    if value == "~" {
        return home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(path)
}

fn home_dir() -> Result<PathBuf, ConfigError> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(ConfigError::HomeUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn load_config_accepts_keybinding_map_and_filters_enabled_models() {
        let root = test_dir("pi-config-load");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"enabledModels":["faux/echo","named"],"systemPrompt":"prompt"}"#,
        )
        .expect("write settings");
        fs::write(
            agent_dir.join("models.json"),
            r#"[
                {"provider":"faux","id":"echo","api":"faux"},
                {"provider":"openai","id":"gpt-test","name":"named","api":"open-ai"},
                {"provider":"anthropic","id":"claude-test","api":"anthropic"}
            ]"#,
        )
        .expect("write models");
        fs::write(
            agent_dir.join("keybindings.json"),
            r#"{"submit":["enter"],"cancel":["escape"]}"#,
        )
        .expect("write keybindings");
        fs::create_dir_all(agent_dir.join("skills")).expect("create skills");
        fs::create_dir_all(agent_dir.join("prompts")).expect("create prompts");
        fs::create_dir_all(agent_dir.join("extensions")).expect("create extensions");
        fs::create_dir_all(cwd.join(".pi/themes")).expect("create project themes");
        fs::write(agent_dir.join("skills/review.md"), "review skill").expect("write skill");
        fs::write(agent_dir.join("prompts/fix.md"), "fix {{input}}").expect("write prompt");
        fs::write(agent_dir.join("extensions/plan.md"), "plan extension").expect("write extension");
        fs::write(cwd.join(".pi/themes/dark.json"), r#"{"name":"dark"}"#).expect("write theme");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: root.join("missing-project-settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: agent_dir.join("models.json"),
            keybindings_path: agent_dir.join("keybindings.json"),
        })
        .expect("load config");

        assert_eq!(config.system_prompt, Some("prompt".to_string()));
        assert_eq!(
            config
                .models
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>(),
            ["faux/echo", "openai/gpt-test"]
        );
        assert_eq!(config.keybindings.len(), 2);
        assert!(config
            .keybindings
            .iter()
            .any(|binding| binding.action == "submit" && binding.keys == ["enter"]));
        assert_eq!(config.extensions[0].name, "plan");
        assert_eq!(config.extensions[0].content, "plan extension");
        assert_eq!(config.skills[0].name, "review");
        assert_eq!(config.skills[0].content, "review skill");
        assert_eq!(config.prompt_templates[0].name, "fix");
        assert_eq!(config.themes[0].name, "dark");
        assert!(config.diagnostics.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn settings_and_local_packages_extend_resource_paths() {
        let root = test_dir("pi-config-package-resources");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        let package_dir = cwd.join("package");
        fs::create_dir_all(agent_dir.join("custom-extensions")).expect("create extensions");
        fs::create_dir_all(cwd.join(".pi/custom-prompts")).expect("create prompts");
        fs::create_dir_all(package_dir.join("extensions")).expect("create package extensions");
        fs::create_dir_all(package_dir.join("skills")).expect("create package skills");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"extensions":["custom-extensions"],"packages":["package"]}"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".pi/settings.json"),
            r#"{"prompts":["custom-prompts"]}"#,
        )
        .expect("write project settings");
        fs::write(
            agent_dir.join("custom-extensions/global.md"),
            "global extension",
        )
        .expect("write global extension");
        fs::write(cwd.join(".pi/custom-prompts/local.md"), "local prompt")
            .expect("write local prompt");
        fs::write(
            package_dir.join("package.json"),
            r#"{"pi":{"extensions":["extensions/pkg.md"],"skills":["skills/pkg.md"]}}"#,
        )
        .expect("write package manifest");
        fs::write(package_dir.join("extensions/pkg.md"), "package extension")
            .expect("write package extension");
        fs::write(package_dir.join("skills/pkg.md"), "package skill").expect("write package skill");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert!(config
            .extensions
            .iter()
            .any(|resource| resource.name == "global" && resource.content == "global extension"));
        assert!(config
            .extensions
            .iter()
            .any(|resource| resource.name == "pkg" && resource.content == "package extension"));
        assert!(config
            .skills
            .iter()
            .any(|resource| resource.name == "pkg" && resource.content == "package skill"));
        assert!(config
            .prompt_templates
            .iter()
            .any(|resource| resource.name == "local" && resource.content == "local prompt"));
        assert!(config.diagnostics.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn with_session_dir_expands_relative_paths_from_cwd() {
        let cwd = test_dir("pi-config-session-dir");
        let paths = ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: cwd.join("agent"),
            session_dir: cwd.join("agent/sessions"),
            settings_path: cwd.join("agent/settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: cwd.join("agent/auth.json"),
            models_path: cwd.join("agent/models.json"),
            keybindings_path: cwd.join("agent/keybindings.json"),
        };

        let next = paths
            .with_session_dir("local-sessions")
            .expect("apply session dir");

        assert_eq!(next.session_dir, cwd.join("local-sessions"));
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn nested_settings_deep_merge_like_ts_reference() {
        let root = test_dir("pi-config-nested-settings");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(cwd.join(".pi")).expect("create project settings dir");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{
                "transport":"auto",
                "steeringMode":"all",
                "compaction":{"enabled":true,"reserveTokens":100},
                "retry":{"enabled":true,"provider":{"timeoutMs":1000}},
                "terminal":{"showImages":true},
                "images":{"autoResize":true},
                "thinkingBudgets":{"minimal":1000},
                "markdown":{"codeBlockIndent":"  "},
                "warnings":{"anthropicExtraUsage":true}
            }"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".pi/settings.json"),
            r#"{
                "followUpMode":"one-at-a-time",
                "compaction":{"keepRecentTokens":200},
                "retry":{"provider":{"maxRetries":2}},
                "terminal":{"imageWidthCells":40},
                "images":{"blockImages":true},
                "thinkingBudgets":{"high":4000},
                "warnings":{"anthropicExtraUsage":false}
            }"#,
        )
        .expect("write project settings");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert_eq!(config.settings.transport.as_deref(), Some("auto"));
        assert_eq!(config.settings.steering_mode.as_deref(), Some("all"));
        assert_eq!(
            config.settings.follow_up_mode.as_deref(),
            Some("one-at-a-time")
        );
        let compaction = config.settings.compaction.expect("compaction");
        assert_eq!(compaction.enabled, Some(true));
        assert_eq!(compaction.reserve_tokens, Some(100));
        assert_eq!(compaction.keep_recent_tokens, Some(200));
        let retry = config.settings.retry.expect("retry");
        let provider = retry.provider.expect("provider retry");
        assert_eq!(provider.timeout_ms, Some(1000));
        assert_eq!(provider.max_retries, Some(2));
        let terminal = config.settings.terminal.expect("terminal");
        assert_eq!(terminal.show_images, Some(true));
        assert_eq!(terminal.image_width_cells, Some(40));
        let images = config.settings.images.expect("images");
        assert_eq!(images.auto_resize, Some(true));
        assert_eq!(images.block_images, Some(true));
        let thinking = config.settings.thinking_budgets.expect("thinking budgets");
        assert_eq!(thinking.minimal, Some(1000));
        assert_eq!(thinking.high, Some(4000));
        assert_eq!(
            config
                .settings
                .markdown
                .expect("markdown")
                .code_block_indent
                .as_deref(),
            Some("  ")
        );
        assert_eq!(
            config
                .settings
                .warnings
                .expect("warnings")
                .anthropic_extra_usage,
            Some(false)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_oauth_maps_to_provider_specific_auth() {
        let auth = BTreeMap::from([
            (
                "anthropic".to_string(),
                AuthCredential::OAuth {
                    access_token: "claude-token".to_string(),
                    expires: 0,
                    account_id: None,
                },
            ),
            (
                "openai".to_string(),
                AuthCredential::OAuth {
                    access_token: "codex-token".to_string(),
                    expires: 0,
                    account_id: Some("account-id".to_string()),
                },
            ),
        ]);

        assert_eq!(
            auth_for_provider(&auth, "anthropic"),
            Some(ResolvedAuth::ClaudeCodeOAuth {
                access_token: "claude-token".to_string()
            })
        );
        assert_eq!(
            auth_for_provider(&auth, "openai"),
            Some(ResolvedAuth::ChatGptOAuth {
                access_token: "codex-token".to_string(),
                account_id: Some("account-id".to_string())
            })
        );
    }

    #[test]
    fn provider_api_names_match_ts_reference_names() {
        let models = serde_json::from_str::<Vec<ModelDefinition>>(
            r#"[
                {"provider":"openai","id":"gpt","api":"openai-responses"},
                {"provider":"openai-codex","id":"codex","api":"openai-codex-responses"},
                {"provider":"azure-openai-responses","id":"gpt","api":"azure-openai-responses"},
                {"provider":"anthropic","id":"claude","api":"anthropic-messages"},
                {"provider":"google","id":"gemini","api":"google-generative-ai"},
                {"provider":"google-vertex","id":"gemini","api":"google-vertex"},
                {"provider":"amazon-bedrock","id":"claude","api":"bedrock-converse-stream"},
                {"provider":"mistral","id":"devstral","api":"mistral-conversations"}
            ]"#,
        )
        .expect("parse models");

        assert_eq!(models[0].api, ProviderApi::OpenAiResponses);
        assert_eq!(models[1].api, ProviderApi::OpenAiCodexResponses);
        assert_eq!(models[2].api, ProviderApi::AzureOpenAiResponses);
        assert_eq!(models[3].api, ProviderApi::Anthropic);
        assert_eq!(models[4].api, ProviderApi::Google);
        assert_eq!(models[5].api, ProviderApi::GoogleVertex);
        assert_eq!(models[6].api, ProviderApi::Bedrock);
        assert_eq!(models[7].api, ProviderApi::Mistral);
    }

    #[test]
    fn default_models_include_provider_parity_targets() {
        let providers = default_models()
            .into_iter()
            .map(|model| model.provider)
            .collect::<Vec<_>>();

        for provider in [
            "openai-codex",
            "azure-openai-responses",
            "github-copilot",
            "openrouter",
            "google-vertex",
            "amazon-bedrock",
            "mistral",
            "cloudflare-workers-ai",
            "cloudflare-ai-gateway",
        ] {
            assert!(providers.iter().any(|candidate| candidate == provider));
        }
    }

    #[test]
    fn env_auth_detects_provider_parity_targets() {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let empty_auth = BTreeMap::new();
        let saved = [
            save_env("AZURE_OPENAI_API_KEY"),
            save_env("COPILOT_GITHUB_TOKEN"),
            save_env("OPENROUTER_API_KEY"),
            save_env("GOOGLE_CLOUD_API_KEY"),
            save_env("AWS_BEARER_TOKEN_BEDROCK"),
            save_env("MISTRAL_API_KEY"),
            save_env("CLOUDFLARE_API_KEY"),
            save_env("CODEX_ACCESS_TOKEN"),
            save_env("CHATGPT_ACCOUNT_ID"),
        ];

        env::set_var("AZURE_OPENAI_API_KEY", "azure-key");
        env::set_var("COPILOT_GITHUB_TOKEN", "copilot-key");
        env::set_var("OPENROUTER_API_KEY", "openrouter-key");
        env::set_var("GOOGLE_CLOUD_API_KEY", "vertex-key");
        env::set_var("AWS_BEARER_TOKEN_BEDROCK", "bedrock-token");
        env::set_var("MISTRAL_API_KEY", "mistral-key");
        env::set_var("CLOUDFLARE_API_KEY", "cloudflare-key");
        env::set_var("CODEX_ACCESS_TOKEN", "codex-token");
        env::set_var("CHATGPT_ACCOUNT_ID", "account-id");

        assert_eq!(
            auth_for_provider(&empty_auth, "azure-openai-responses"),
            Some(ResolvedAuth::ApiKey("azure-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "github-copilot"),
            Some(ResolvedAuth::ApiKey("copilot-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "openrouter"),
            Some(ResolvedAuth::ApiKey("openrouter-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "google-vertex"),
            Some(ResolvedAuth::ApiKey("vertex-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "amazon-bedrock"),
            Some(ResolvedAuth::ApiKey("bedrock-token".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "mistral"),
            Some(ResolvedAuth::ApiKey("mistral-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "cloudflare-ai-gateway"),
            Some(ResolvedAuth::ApiKey("cloudflare-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "openai-codex"),
            Some(ResolvedAuth::ChatGptOAuth {
                access_token: "codex-token".to_string(),
                account_id: Some("account-id".to_string())
            })
        );

        for (name, value) in saved {
            restore_env(name, value);
        }
    }

    #[test]
    fn reads_claude_code_oauth_credentials_file() {
        let root = test_dir("pi-config-claude-oauth");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".credentials.json");
        fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"claude-access","refreshToken":"redacted","expiresAt":1}}"#,
        )
        .expect("write credentials");

        assert_eq!(
            read_claude_code_oauth_from_path(&path),
            Some(ResolvedAuth::ClaudeCodeOAuth {
                access_token: "claude-access".to_string()
            })
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_codex_chatgpt_oauth_auth_file() {
        let root = test_dir("pi-config-codex-oauth");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("auth.json");
        fs::write(
            &path,
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"codex-access","refresh_token":"redacted","account_id":"account-id"}}"#,
        )
        .expect("write auth");

        assert_eq!(
            read_codex_chatgpt_oauth_from_path(&path),
            Some(ResolvedAuth::ChatGptOAuth {
                access_token: "codex-access".to_string(),
                account_id: Some("account-id".to_string())
            })
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}-{}", std::process::id(), unique_suffix()))
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    }

    fn save_env(name: &'static str) -> (&'static str, Option<String>) {
        (name, env::var(name).ok())
    }

    fn restore_env(name: &str, value: Option<String>) {
        match value {
            Some(value) => env::set_var(name, value),
            None => env::remove_var(name),
        }
    }
}

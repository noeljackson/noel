use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const APP_NAME: &str = "pi";
pub const CONFIG_DIR_NAME: &str = ".pi";
pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";
const RESOURCE_IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
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
    pub model_cache_path: PathBuf,
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
            model_cache_path: agent_dir.join("model-cache.json"),
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
    pub packages: Vec<PackageSource>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub themes: Vec<String>,
    #[serde(default)]
    pub disabled_resources: Option<ResourceStateSettings>,
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
    #[serde(default)]
    pub model_refresh: Option<ModelRefreshSettings>,
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
            disabled_resources: merge_optional_settings(
                self.disabled_resources,
                overrides.disabled_resources,
            ),
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
            model_refresh: merge_optional_settings(self.model_refresh, overrides.model_refresh),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceStateSettings {
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub themes: Vec<String>,
}

impl MergeSettings for ResourceStateSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            extensions: merge_resource_state_patterns(self.extensions, overrides.extensions),
            skills: merge_resource_state_patterns(self.skills, overrides.skills),
            prompts: merge_resource_state_patterns(self.prompts, overrides.prompts),
            themes: merge_resource_state_patterns(self.themes, overrides.themes),
        }
    }
}

fn merge_resource_state_patterns(base: Vec<String>, overrides: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut merged = Vec::new();
    for pattern in base.into_iter().chain(overrides) {
        if seen.insert(pattern.clone()) {
            merged.push(pattern);
        }
    }
    merged
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    Simple(String),
    Filtered(PackageSourceConfig),
}

impl PackageSource {
    pub fn source(&self) -> &str {
        match self {
            PackageSource::Simple(source) => source,
            PackageSource::Filtered(config) => &config.source,
        }
    }

    fn filters(&self, resource_type: PackageResourceType) -> Option<&[String]> {
        let PackageSource::Filtered(config) = self else {
            return None;
        };
        match resource_type {
            PackageResourceType::Extensions => config.extensions.as_deref(),
            PackageResourceType::Skills => config.skills.as_deref(),
            PackageResourceType::Prompts => config.prompts.as_deref(),
            PackageResourceType::Themes => config.themes.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageSourceConfig {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
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
            provider: overrides.provider.or(self.provider),
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRefreshSettings {
    pub enabled: Option<bool>,
    pub ttl_hours: Option<u64>,
}

impl MergeSettings for ModelRefreshSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            enabled: overrides.enabled.or(self.enabled),
            ttl_hours: overrides.ttl_hours.or(self.ttl_hours),
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
#[serde(rename_all = "camelCase")]
pub struct ModelCache {
    pub refreshed_at: u64,
    #[serde(default)]
    pub models: Vec<ModelDefinition>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageProviderApi {
    #[serde(rename = "openrouter-images")]
    #[default]
    OpenRouterImages,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModelDefinition {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api: ImageProviderApi,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
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
    pub image_models: Vec<ImageModelDefinition>,
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
    let global_settings = read_optional_settings(&paths.settings_path)?;
    let project_settings = read_optional_settings(&paths.project_settings_path)?;
    let global_settings = global_settings.unwrap_or_default();
    let project_settings = project_settings.unwrap_or_default();
    let settings = global_settings.clone().merge(project_settings.clone());
    let auth = read_optional_json::<AuthData>(&paths.auth_path)?.unwrap_or_default();
    let mut diagnostics = Vec::new();
    let explicit_models =
        read_optional_json::<Vec<ModelDefinition>>(&paths.models_path)?.unwrap_or_default();
    let cache = match read_model_cache(&paths.model_cache_path) {
        Ok(cache) => cache,
        Err(error) => {
            diagnostics.push(format!(
                "failed to read model cache {}: {error}",
                paths.model_cache_path.display()
            ));
            None
        }
    };
    if let Some(cache) = &cache {
        diagnostics.extend(cache.diagnostics.iter().cloned());
    }
    let models = filter_enabled_models(
        merge_model_definitions(
            default_models(),
            cache
                .as_ref()
                .map(|cache| cache.models.clone())
                .unwrap_or_default(),
            explicit_models,
        ),
        settings.enabled_models.as_deref(),
    );
    let image_models = default_image_models();
    let keybindings = read_optional_json::<KeybindingsFile>(&paths.keybindings_path)?
        .map(KeybindingsFile::into_keybindings)
        .unwrap_or_default();
    let context_files = load_context_files(&paths.cwd, &paths.agent_dir)?;
    let mut extensions = load_resource_files(&paths, "extensions", &mut diagnostics);
    let mut skills = load_skill_resource_files(&paths, &mut diagnostics);
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
    filter_disabled_resource_files(
        &mut extensions,
        PackageResourceType::Extensions,
        settings.disabled_resources.as_ref(),
    );
    filter_disabled_resource_files(
        &mut skills,
        PackageResourceType::Skills,
        settings.disabled_resources.as_ref(),
    );
    filter_disabled_resource_files(
        &mut prompt_templates,
        PackageResourceType::Prompts,
        settings.disabled_resources.as_ref(),
    );
    filter_disabled_resource_files(
        &mut themes,
        PackageResourceType::Themes,
        settings.disabled_resources.as_ref(),
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
        image_models,
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

pub fn read_model_cache(path: &Path) -> Result<Option<ModelCache>, ConfigError> {
    read_optional_json::<ModelCache>(path)
}

pub fn write_model_cache(path: &Path, cache: &ModelCache) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content = serde_json::to_string_pretty(cache).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    fs::write(path, format!("{content}\n")).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn merge_model_definitions(
    base: Vec<ModelDefinition>,
    cache: Vec<ModelDefinition>,
    explicit: Vec<ModelDefinition>,
) -> Vec<ModelDefinition> {
    let mut models = Vec::new();
    for model in base.into_iter().chain(cache).chain(explicit) {
        if let Some(index) = models.iter().position(|existing: &ModelDefinition| {
            existing.provider == model.provider && existing.id == model.id
        }) {
            models[index] = model;
        } else {
            models.push(model);
        }
    }
    models
}

fn env_api_key(provider: &str) -> Option<String> {
    let names = match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"][..],
        "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"][..],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"][..],
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => &["CLOUDFLARE_API_KEY"][..],
        "cerebras" => &["CEREBRAS_API_KEY"][..],
        "deepseek" => &["DEEPSEEK_API_KEY"][..],
        "fireworks" => &["FIREWORKS_API_KEY"][..],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"][..],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"][..],
        "groq" => &["GROQ_API_KEY"][..],
        "huggingface" => &["HF_TOKEN"][..],
        "kimi-coding" => &["KIMI_API_KEY"][..],
        "minimax" => &["MINIMAX_API_KEY"][..],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"][..],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"][..],
        "openai" => &["OPENAI_API_KEY"],
        "openai-codex" => &["CODEX_API_KEY"][..],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"][..],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "together" => &["TOGETHER_API_KEY"][..],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"][..],
        "xai" => &["XAI_API_KEY"][..],
        "xiaomi" => &["XIAOMI_API_KEY"][..],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"][..],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"][..],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"][..],
        "zai" => &["ZAI_API_KEY"][..],
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

fn read_optional_settings(path: &Path) -> Result<Option<Settings>, ConfigError> {
    let Some(mut value) = read_optional_json::<serde_json::Value>(path)? else {
        return Ok(None);
    };
    migrate_settings_value(&mut value);
    serde_json::from_value(value)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

fn migrate_settings_value(value: &mut serde_json::Value) {
    let Some(settings) = value.as_object_mut() else {
        return;
    };

    if !settings.contains_key("steeringMode") {
        if let Some(queue_mode) = settings.remove("queueMode") {
            settings.insert("steeringMode".to_string(), queue_mode);
        }
    }

    if !settings.contains_key("transport") {
        if let Some(websockets) = settings
            .get("websockets")
            .and_then(serde_json::Value::as_bool)
        {
            settings.insert(
                "transport".to_string(),
                serde_json::Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
            );
            settings.remove("websockets");
        }
    }

    let Some(retry) = settings
        .get_mut("retry")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if let Some(max_delay) = retry
        .get("maxDelayMs")
        .filter(|value| value.is_number())
        .cloned()
    {
        let provider = retry
            .entry("provider".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(provider) = provider.as_object_mut() {
            let should_set = match provider.get("maxRetryDelayMs") {
                Some(existing) => existing.is_null(),
                None => true,
            };
            if should_set {
                provider.insert("maxRetryDelayMs".to_string(), max_delay);
            }
        }
    }
    retry.remove("maxDelayMs");
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
            if is_resource_metadata_file(&path) {
                continue;
            }
            push_resource_file(&mut resources, &path, diagnostics);
        }
    }
    resources.into_values().collect()
}

fn load_skill_resource_files(
    paths: &ConfigPaths,
    diagnostics: &mut Vec<String>,
) -> Vec<ResourceFile> {
    let resources = load_resource_files(paths, "skills", diagnostics);
    let mut map = resources
        .into_iter()
        .map(|resource| (resource.name.clone(), resource))
        .collect::<BTreeMap<_, _>>();
    for dir in ancestor_agents_skill_dirs(&paths.cwd) {
        collect_agent_skill_path(&mut map, &dir, diagnostics);
    }
    map.into_values().collect()
}

fn ancestor_agents_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let git_root = find_git_root(cwd);
    let mut dirs = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        dirs.push(dir.join(".agents").join("skills"));
        if git_root.as_deref() == Some(dir) {
            break;
        }
        current = dir.parent();
    }
    dirs.reverse();
    dirs
}

fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn collect_agent_skill_path(
    resources: &mut BTreeMap<String, ResourceFile>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    if !path.exists() {
        return;
    }
    if path.is_file() {
        if path.file_name().and_then(|value| value.to_str()) == Some("SKILL.md") {
            push_resource_file(resources, path, diagnostics);
        }
        return;
    }
    if !path.is_dir() {
        return;
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(format!("failed to read {}: {error}", path.display()));
            return;
        }
    };
    let ignore_patterns = read_resource_ignore_patterns(path, diagnostics);
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
        if resource_path_ignored(&child, path, &ignore_patterns) {
            continue;
        }
        if child.is_dir() {
            collect_agent_skill_path(resources, &child, diagnostics);
        } else if child.file_name().and_then(|value| value.to_str()) == Some("SKILL.md") {
            push_resource_file(resources, &child, diagnostics);
        }
    }
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
    packages: &[PackageSource],
    extensions: &mut Vec<ResourceFile>,
    skills: &mut Vec<ResourceFile>,
    prompts: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) {
    for package in packages {
        let package_root = match resolve_resource_path(cwd, package.source()) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!(
                    "failed to resolve package {}: {error}",
                    package.source()
                ));
                continue;
            }
        };
        if package_root.is_file() {
            extend_resource_files(
                extensions,
                cwd,
                &[package.source().to_string()],
                diagnostics,
            );
            continue;
        }
        if !package_root.is_dir() {
            diagnostics.push(format!(
                "package path not found: {}",
                package_root.display()
            ));
            continue;
        }
        load_package_root_resources(
            &package_root,
            package,
            extensions,
            skills,
            prompts,
            themes,
            diagnostics,
        );
    }
}

fn load_package_root_resources(
    package_root: &Path,
    package: &PackageSource,
    extensions: &mut Vec<ResourceFile>,
    skills: &mut Vec<ResourceFile>,
    prompts: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) {
    let manifest = read_package_manifest(package_root, diagnostics);
    let extension_entries = package_resource_entries(
        manifest.as_ref().map(|manifest| &manifest.extensions),
        "extensions",
    );
    let skill_entries =
        package_resource_entries(manifest.as_ref().map(|manifest| &manifest.skills), "skills");
    let prompt_entries = package_resource_entries(
        manifest.as_ref().map(|manifest| &manifest.prompts),
        "prompts",
    );
    let theme_entries =
        package_resource_entries(manifest.as_ref().map(|manifest| &manifest.themes), "themes");
    extend_package_resource_files(
        extensions,
        package_root,
        &extension_entries,
        package.filters(PackageResourceType::Extensions),
        diagnostics,
    );
    extend_package_resource_files(
        skills,
        package_root,
        &skill_entries,
        package.filters(PackageResourceType::Skills),
        diagnostics,
    );
    extend_package_resource_files(
        prompts,
        package_root,
        &prompt_entries,
        package.filters(PackageResourceType::Prompts),
        diagnostics,
    );
    extend_package_resource_files(
        themes,
        package_root,
        &theme_entries,
        package.filters(PackageResourceType::Themes),
        diagnostics,
    );
}

fn read_package_manifest(
    package_root: &Path,
    diagnostics: &mut Vec<String>,
) -> Option<PackageManifest> {
    let path = package_root.join("package.json");
    if !path.exists() {
        return None;
    }
    match read_optional_json::<PackageJson>(&path) {
        Ok(Some(package_json)) => package_json.pi,
        Ok(None) => None,
        Err(error) => {
            diagnostics.push(format!(
                "failed to read package manifest {}: {error}",
                path.display()
            ));
            None
        }
    }
}

fn package_resource_entries(
    manifest_entries: Option<&Vec<String>>,
    default_dir: &str,
) -> Vec<String> {
    manifest_entries
        .cloned()
        .unwrap_or_else(|| vec![default_dir.to_string()])
}

fn extend_package_resource_files(
    resources: &mut Vec<ResourceFile>,
    base_dir: &Path,
    entries: &[String],
    filters: Option<&[String]>,
    diagnostics: &mut Vec<String>,
) {
    let Some(filtered_paths) =
        collect_filtered_package_paths(base_dir, entries, filters, diagnostics)
    else {
        return;
    };
    let mut map = resources
        .drain(..)
        .map(|resource| (resource.name.clone(), resource))
        .collect::<BTreeMap<_, _>>();
    for path in filtered_paths {
        push_resource_file(&mut map, &path, diagnostics);
    }
    resources.extend(map.into_values());
}

fn collect_filtered_package_paths(
    base_dir: &Path,
    entries: &[String],
    filters: Option<&[String]>,
    diagnostics: &mut Vec<String>,
) -> Option<Vec<PathBuf>> {
    if matches!(filters, Some(filters) if filters.is_empty()) {
        return None;
    }
    let mut paths = Vec::new();
    for entry in entries {
        let path = match resolve_resource_path(base_dir, entry) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!("failed to resolve resource path {entry}: {error}"));
                continue;
            }
        };
        collect_resource_paths(&mut paths, &path, diagnostics);
    }
    paths.sort();
    paths.dedup();
    Some(match filters {
        Some(filters) => apply_resource_patterns(&paths, filters, base_dir),
        None => paths,
    })
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

#[derive(Debug, Clone, Copy)]
enum PackageResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl PackageResourceType {
    fn key(self) -> &'static str {
        match self {
            PackageResourceType::Extensions => "extensions",
            PackageResourceType::Skills => "skills",
            PackageResourceType::Prompts => "prompts",
            PackageResourceType::Themes => "themes",
        }
    }

    fn singular(self) -> &'static str {
        match self {
            PackageResourceType::Extensions => "extension",
            PackageResourceType::Skills => "skill",
            PackageResourceType::Prompts => "prompt",
            PackageResourceType::Themes => "theme",
        }
    }
}

fn filter_disabled_resource_files(
    resources: &mut Vec<ResourceFile>,
    resource_type: PackageResourceType,
    disabled_resources: Option<&ResourceStateSettings>,
) {
    let Some(disabled_resources) = disabled_resources else {
        return;
    };
    let patterns = match resource_type {
        PackageResourceType::Extensions => &disabled_resources.extensions,
        PackageResourceType::Skills => &disabled_resources.skills,
        PackageResourceType::Prompts => &disabled_resources.prompts,
        PackageResourceType::Themes => &disabled_resources.themes,
    };
    if patterns.is_empty() {
        return;
    }
    resources.retain(|resource| {
        !patterns
            .iter()
            .any(|pattern| resource_state_pattern_matches(resource, resource_type, pattern))
    });
}

fn resource_state_pattern_matches(
    resource: &ResourceFile,
    resource_type: PackageResourceType,
    pattern: &str,
) -> bool {
    let pattern = normalize_pattern(pattern);
    let path = normalize_resource_path(&resource.path);
    let key = resource_type.key();
    let singular = resource_type.singular();
    wildcard_matches(&pattern, &resource.name)
        || wildcard_matches(&pattern, &format!("{key}/{}", resource.name))
        || wildcard_matches(&pattern, &format!("{singular}:{}", resource.name))
        || wildcard_matches(&pattern, &path)
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
    let mut paths = Vec::new();
    collect_resource_paths(&mut paths, path, diagnostics);
    for path in paths {
        push_resource_file(resources, &path, diagnostics);
    }
}

fn collect_resource_paths(paths: &mut Vec<PathBuf>, path: &Path, diagnostics: &mut Vec<String>) {
    if is_resource_metadata_file(path) {
        return;
    }
    if path.is_file() {
        paths.push(path.to_path_buf());
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
    let ignore_patterns = read_resource_ignore_patterns(path, diagnostics);
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
        if resource_path_ignored(&child, path, &ignore_patterns) {
            continue;
        }
        if is_resource_metadata_file(&child) {
            continue;
        }
        if child.is_dir() {
            collect_resource_paths(paths, &child, diagnostics);
        } else if child.is_file() {
            paths.push(child);
        }
    }
}

fn apply_resource_patterns(
    paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> Vec<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    for pattern in patterns {
        if let Some(pattern) = pattern.strip_prefix('+') {
            force_includes.push(pattern);
        } else if let Some(pattern) = pattern.strip_prefix('-') {
            force_excludes.push(pattern);
        } else if let Some(pattern) = pattern.strip_prefix('!') {
            excludes.push(pattern);
        } else {
            includes.push(pattern.as_str());
        }
    }

    let mut selected = paths
        .iter()
        .filter(|path| {
            includes.is_empty()
                || includes
                    .iter()
                    .any(|pattern| resource_pattern_matches(path, pattern, base_dir))
        })
        .filter(|path| {
            !excludes
                .iter()
                .any(|pattern| resource_pattern_matches(path, pattern, base_dir))
        })
        .cloned()
        .collect::<Vec<_>>();

    for path in paths {
        if !selected.contains(path)
            && force_includes
                .iter()
                .any(|pattern| exact_resource_pattern_matches(path, pattern, base_dir))
        {
            selected.push(path.clone());
        }
    }
    selected.retain(|path| {
        !force_excludes
            .iter()
            .any(|pattern| exact_resource_pattern_matches(path, pattern, base_dir))
    });
    selected
}

fn resource_pattern_matches(path: &Path, pattern: &str, base_dir: &Path) -> bool {
    let pattern = normalize_pattern(pattern);
    let rel = normalize_resource_path(path.strip_prefix(base_dir).unwrap_or(path));
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_string();
    let full = normalize_resource_path(path);
    wildcard_matches(&pattern, &rel)
        || wildcard_matches(&pattern, &name)
        || wildcard_matches(&pattern, &full)
}

fn exact_resource_pattern_matches(path: &Path, pattern: &str, base_dir: &Path) -> bool {
    let pattern = normalize_pattern(pattern);
    let rel = normalize_resource_path(path.strip_prefix(base_dir).unwrap_or(path));
    let full = normalize_resource_path(path);
    pattern == rel || pattern == full
}

fn normalize_pattern(pattern: &str) -> String {
    pattern
        .strip_prefix("./")
        .unwrap_or(pattern)
        .replace('\\', "/")
}

fn normalize_resource_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut matched = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    matched[0][0] = true;
    for index in 1..=pattern.len() {
        if pattern[index - 1] == b'*' {
            matched[index][0] = matched[index - 1][0];
        }
    }
    for pattern_index in 1..=pattern.len() {
        for value_index in 1..=value.len() {
            matched[pattern_index][value_index] = match pattern[pattern_index - 1] {
                b'*' => {
                    matched[pattern_index - 1][value_index]
                        || matched[pattern_index][value_index - 1]
                }
                b'?' => matched[pattern_index - 1][value_index - 1],
                byte => {
                    matched[pattern_index - 1][value_index - 1] && byte == value[value_index - 1]
                }
            };
        }
    }
    matched[pattern.len()][value.len()]
}

fn is_resource_metadata_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|name| name.ends_with(".pi-extension.json") || RESOURCE_IGNORE_FILES.contains(&name))
        .unwrap_or(false)
}

fn read_resource_ignore_patterns(dir: &Path, diagnostics: &mut Vec<String>) -> Vec<String> {
    let mut patterns = Vec::new();
    for filename in RESOURCE_IGNORE_FILES {
        let path = dir.join(filename);
        if !path.exists() {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(content) => patterns.extend(content.lines().filter_map(parse_ignore_line)),
            Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
        }
    }
    patterns
}

fn parse_ignore_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    Some(line.to_string())
}

fn resource_path_ignored(path: &Path, base_dir: &Path, patterns: &[String]) -> bool {
    let mut ignored = false;
    for pattern in patterns {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map(|pattern| (true, pattern))
            .unwrap_or((false, pattern.as_str()));
        if resource_ignore_pattern_matches(path, base_dir, pattern) {
            ignored = !negated;
        }
    }
    ignored
}

fn resource_ignore_pattern_matches(path: &Path, base_dir: &Path, pattern: &str) -> bool {
    let is_dir = path.is_dir();
    let pattern = normalize_pattern(pattern);
    let directory_only = pattern.ends_with('/');
    if directory_only && !is_dir {
        return false;
    }
    let pattern = pattern.trim_end_matches('/');
    let rel = normalize_resource_path(path.strip_prefix(base_dir).unwrap_or(path));
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_string();
    if pattern.contains('/') {
        wildcard_matches(pattern, &rel)
    } else {
        wildcard_matches(pattern, &name)
    }
}

fn push_resource_file(
    resources: &mut BTreeMap<String, ResourceFile>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    let Some(name) = resource_name(path) else {
        return;
    };
    match fs::read_to_string(path) {
        Ok(content) => {
            let previous = resources.insert(
                name.clone(),
                ResourceFile {
                    name: name.clone(),
                    path: path.to_path_buf(),
                    content,
                },
            );
            if let Some(previous) = previous {
                diagnostics.push(format!(
                    "resource name collision for {name}: {} replaced by {}",
                    previous.path.display(),
                    path.display()
                ));
            }
        }
        Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
    }
}

fn resource_name(path: &Path) -> Option<String> {
    if path.file_name().and_then(|value| value.to_str()) == Some("SKILL.md") {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .map(ToString::to_string);
    }
    path.file_stem()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
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
            id: "gpt-5.1".to_string(),
            name: Some("GPT 5.1".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.1-codex-max".to_string(),
            name: Some("GPT 5.1 Codex Max".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.1-codex-mini".to_string(),
            name: Some("GPT 5.1 Codex Mini".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.2".to_string(),
            name: Some("GPT 5.2".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.2-codex".to_string(),
            name: Some("GPT 5.2 Codex".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.3-codex".to_string(),
            name: Some("GPT 5.3 Codex".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.3-codex-spark".to_string(),
            name: Some("GPT 5.3 Codex Spark".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.4".to_string(),
            name: Some("GPT 5.4".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.4-mini".to_string(),
            name: Some("GPT 5.4 Mini".to_string()),
            api: ProviderApi::OpenAiCodexResponses,
            base_url: Some("https://chatgpt.com/backend-api".to_string()),
        },
        ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "gpt-5.5".to_string(),
            name: Some("GPT 5.5".to_string()),
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
            id: "claude-opus-4-1-20250805".to_string(),
            name: Some("Claude Opus 4.1".to_string()),
            api: ProviderApi::Anthropic,
            base_url: None,
        },
        ModelDefinition {
            provider: "anthropic".to_string(),
            id: "claude-opus-4-7".to_string(),
            name: Some("Claude Opus 4.7".to_string()),
            api: ProviderApi::Anthropic,
            base_url: None,
        },
        ModelDefinition {
            provider: "anthropic".to_string(),
            id: "claude-opus-4-20250514".to_string(),
            name: Some("Claude Opus 4".to_string()),
            api: ProviderApi::Anthropic,
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
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
            name: Some("Claude Sonnet 4.6".to_string()),
            api: ProviderApi::Anthropic,
            base_url: None,
        },
        ModelDefinition {
            provider: "github-copilot".to_string(),
            id: "gpt-5.4".to_string(),
            name: Some("GitHub Copilot GPT 5.4".to_string()),
            api: ProviderApi::OpenAiResponses,
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
            provider: "deepseek".to_string(),
            id: "deepseek-v4-flash".to_string(),
            name: Some("DeepSeek V4 Flash".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.deepseek.com".to_string()),
        },
        ModelDefinition {
            provider: "groq".to_string(),
            id: "openai/gpt-oss-120b".to_string(),
            name: Some("Groq GPT OSS 120B".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.groq.com/openai/v1".to_string()),
        },
        ModelDefinition {
            provider: "cerebras".to_string(),
            id: "gpt-oss-120b".to_string(),
            name: Some("Cerebras GPT OSS 120B".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.cerebras.ai/v1".to_string()),
        },
        ModelDefinition {
            provider: "xai".to_string(),
            id: "grok-code-fast-1".to_string(),
            name: Some("Grok Code Fast 1".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.x.ai/v1".to_string()),
        },
        ModelDefinition {
            provider: "zai".to_string(),
            id: "glm-5.1".to_string(),
            name: Some("GLM 5.1".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.z.ai/api/coding/paas/v4".to_string()),
        },
        ModelDefinition {
            provider: "huggingface".to_string(),
            id: "MiniMaxAI/MiniMax-M2.7".to_string(),
            name: Some("Hugging Face MiniMax M2.7".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://router.huggingface.co/v1".to_string()),
        },
        ModelDefinition {
            provider: "together".to_string(),
            id: "Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8".to_string(),
            name: Some("Together Qwen3 Coder 480B".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.together.ai/v1".to_string()),
        },
        ModelDefinition {
            provider: "moonshotai".to_string(),
            id: "kimi-k2-thinking".to_string(),
            name: Some("Kimi K2 Thinking".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.moonshot.ai/v1".to_string()),
        },
        ModelDefinition {
            provider: "moonshotai-cn".to_string(),
            id: "kimi-k2-thinking".to_string(),
            name: Some("Kimi K2 Thinking CN".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://api.moonshot.cn/v1".to_string()),
        },
        ModelDefinition {
            provider: "opencode".to_string(),
            id: "big-pickle".to_string(),
            name: Some("OpenCode Big Pickle".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://opencode.ai/zen/v1".to_string()),
        },
        ModelDefinition {
            provider: "opencode-go".to_string(),
            id: "deepseek-v4-flash".to_string(),
            name: Some("OpenCode Go DeepSeek V4 Flash".to_string()),
            api: ProviderApi::OpenAi,
            base_url: Some("https://opencode.ai/zen/go/v1".to_string()),
        },
        ModelDefinition {
            provider: "fireworks".to_string(),
            id: "accounts/fireworks/models/deepseek-v4-pro".to_string(),
            name: Some("Fireworks DeepSeek V4 Pro".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.fireworks.ai/inference".to_string()),
        },
        ModelDefinition {
            provider: "minimax".to_string(),
            id: "MiniMax-M2.7".to_string(),
            name: Some("MiniMax M2.7".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.minimax.io/anthropic".to_string()),
        },
        ModelDefinition {
            provider: "minimax-cn".to_string(),
            id: "MiniMax-M2.7".to_string(),
            name: Some("MiniMax M2.7 CN".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.minimaxi.com/anthropic".to_string()),
        },
        ModelDefinition {
            provider: "kimi-coding".to_string(),
            id: "kimi-for-coding".to_string(),
            name: Some("Kimi For Coding".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.kimi.com/coding".to_string()),
        },
        ModelDefinition {
            provider: "xiaomi".to_string(),
            id: "mimo-v2.5-pro".to_string(),
            name: Some("MiMo V2.5 Pro".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.xiaomimimo.com/anthropic".to_string()),
        },
        ModelDefinition {
            provider: "xiaomi-token-plan-cn".to_string(),
            id: "mimo-v2.5-pro".to_string(),
            name: Some("MiMo V2.5 Pro Token Plan CN".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://token-plan-cn.xiaomimimo.com/anthropic".to_string()),
        },
        ModelDefinition {
            provider: "xiaomi-token-plan-ams".to_string(),
            id: "mimo-v2.5-pro".to_string(),
            name: Some("MiMo V2.5 Pro Token Plan AMS".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://token-plan-ams.xiaomimimo.com/anthropic".to_string()),
        },
        ModelDefinition {
            provider: "xiaomi-token-plan-sgp".to_string(),
            id: "mimo-v2.5-pro".to_string(),
            name: Some("MiMo V2.5 Pro Token Plan SGP".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://token-plan-sgp.xiaomimimo.com/anthropic".to_string()),
        },
        ModelDefinition {
            provider: "vercel-ai-gateway".to_string(),
            id: "alibaba/qwen3-coder".to_string(),
            name: Some("Vercel AI Gateway Qwen3 Coder".to_string()),
            api: ProviderApi::Anthropic,
            base_url: Some("https://ai-gateway.vercel.sh".to_string()),
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

fn default_image_models() -> Vec<ImageModelDefinition> {
    vec![
        ImageModelDefinition {
            provider: "openrouter".to_string(),
            id: "google/gemini-3.1-flash-image-preview".to_string(),
            name: Some("Google Nano Banana 2".to_string()),
            api: ImageProviderApi::OpenRouterImages,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            input: vec!["image".to_string(), "text".to_string()],
            output: vec!["image".to_string(), "text".to_string()],
        },
        ImageModelDefinition {
            provider: "openrouter".to_string(),
            id: "openai/gpt-5-image".to_string(),
            name: Some("OpenAI GPT-5 Image".to_string()),
            api: ImageProviderApi::OpenRouterImages,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            input: vec!["image".to_string(), "text".to_string()],
            output: vec!["image".to_string(), "text".to_string()],
        },
        ImageModelDefinition {
            provider: "openrouter".to_string(),
            id: "black-forest-labs/flux.2-pro".to_string(),
            name: Some("FLUX.2 Pro".to_string()),
            api: ImageProviderApi::OpenRouterImages,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            input: vec!["text".to_string(), "image".to_string()],
            output: vec!["image".to_string()],
        },
        ImageModelDefinition {
            provider: "openrouter".to_string(),
            id: "openrouter/auto".to_string(),
            name: Some("OpenRouter Auto Image".to_string()),
            api: ImageProviderApi::OpenRouterImages,
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            input: vec!["text".to_string(), "image".to_string()],
            output: vec!["text".to_string(), "image".to_string()],
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

    fn optional_u64_object(fields: &[(&str, Option<u64>)]) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        for (key, value) in fields {
            if let Some(value) = value {
                object.insert((*key).to_string(), serde_json::json!(value));
            }
        }
        serde_json::Value::Object(object)
    }

    fn provider_retry_summary(settings: &Settings) -> serde_json::Value {
        let provider = settings
            .retry
            .as_ref()
            .and_then(|retry| retry.provider.as_ref());
        let mut object = serde_json::Map::new();
        if let Some(timeout_ms) = provider.and_then(|provider| provider.timeout_ms) {
            object.insert("timeoutMs".to_string(), serde_json::json!(timeout_ms));
        }
        if let Some(max_retries) = provider.and_then(|provider| provider.max_retries) {
            object.insert("maxRetries".to_string(), serde_json::json!(max_retries));
        }
        object.insert(
            "maxRetryDelayMs".to_string(),
            serde_json::json!(provider
                .and_then(|provider| provider.max_retry_delay_ms)
                .unwrap_or(60000)),
        );
        serde_json::Value::Object(object)
    }

    fn thinking_budgets_summary(settings: &Settings) -> serde_json::Value {
        let Some(thinking) = settings.thinking_budgets.as_ref() else {
            return serde_json::Value::Null;
        };
        optional_u64_object(&[
            ("minimal", thinking.minimal),
            ("low", thinking.low),
            ("medium", thinking.medium),
            ("high", thinking.high),
        ])
    }

    fn settings_summary(settings: &Settings) -> serde_json::Value {
        let compaction = settings.compaction.as_ref();
        let retry = settings.retry.as_ref();
        let terminal = settings.terminal.as_ref();
        let images = settings.images.as_ref();
        serde_json::json!({
            "defaultProvider": settings.default_provider.as_deref(),
            "defaultModel": settings.default_model.as_deref(),
            "defaultThinkingLevel": settings.default_thinking_level.as_deref(),
            "transport": settings.transport.as_deref().unwrap_or("auto"),
            "steeringMode": settings.steering_mode.as_deref().unwrap_or("one-at-a-time"),
            "followUpMode": settings.follow_up_mode.as_deref().unwrap_or("one-at-a-time"),
            "enabledModels": settings.enabled_models.as_ref(),
            "compaction": {
                "enabled": compaction.and_then(|compaction| compaction.enabled).unwrap_or(true),
                "reserveTokens": compaction.and_then(|compaction| compaction.reserve_tokens).unwrap_or(16384),
                "keepRecentTokens": compaction.and_then(|compaction| compaction.keep_recent_tokens).unwrap_or(20000),
            },
            "retry": {
                "enabled": retry.and_then(|retry| retry.enabled).unwrap_or(true),
                "maxRetries": retry.and_then(|retry| retry.max_retries).unwrap_or(3),
                "baseDelayMs": retry.and_then(|retry| retry.base_delay_ms).unwrap_or(2000),
            },
            "providerRetry": provider_retry_summary(settings),
            "terminal": {
                "showImages": terminal.and_then(|terminal| terminal.show_images).unwrap_or(true),
                "imageWidthCells": terminal.and_then(|terminal| terminal.image_width_cells).unwrap_or(60),
                "clearOnShrink": terminal.and_then(|terminal| terminal.clear_on_shrink).unwrap_or(false),
                "showTerminalProgress": terminal.and_then(|terminal| terminal.show_terminal_progress).unwrap_or(false),
            },
            "images": {
                "autoResize": images.and_then(|images| images.auto_resize).unwrap_or(true),
                "blockImages": images.and_then(|images| images.block_images).unwrap_or(false),
            },
            "thinkingBudgets": thinking_budgets_summary(settings),
            "doubleEscapeAction": settings.double_escape_action.as_deref().unwrap_or("tree"),
            "treeFilterMode": settings.tree_filter_mode.as_deref().unwrap_or("default"),
            "quietStartup": settings.quiet_startup.unwrap_or(false),
            "hideThinkingBlock": settings.hide_thinking_block.unwrap_or(false),
        })
    }

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
            model_cache_path: agent_dir.join("model-cache.json"),
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
        assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

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
            model_cache_path: root.join("missing-model-cache.json"),
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
        assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_object_filters_resource_paths() {
        let root = test_dir("pi-config-package-filters");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        let package_dir = cwd.join("package");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(package_dir.join("extensions")).expect("create package extensions");
        fs::create_dir_all(package_dir.join("skills")).expect("create package skills");
        fs::create_dir_all(package_dir.join("prompts")).expect("create package prompts");
        fs::create_dir_all(package_dir.join("themes")).expect("create package themes");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{
                "packages":[{
                    "source":"package",
                    "extensions":[
                        "extensions/*.md",
                        "!extensions/skip.md",
                        "+extensions/force.txt",
                        "-extensions/drop.md"
                    ],
                    "skills":[],
                    "prompts":["prompts/review.md"],
                    "themes":[]
                }]
            }"#,
        )
        .expect("write settings");
        fs::write(package_dir.join("extensions/keep.md"), "keep extension")
            .expect("write keep extension");
        fs::write(package_dir.join("extensions/skip.md"), "skip extension")
            .expect("write skip extension");
        fs::write(package_dir.join("extensions/drop.md"), "drop extension")
            .expect("write drop extension");
        fs::write(package_dir.join("extensions/force.txt"), "force extension")
            .expect("write force extension");
        fs::write(package_dir.join("extensions/other.txt"), "other extension")
            .expect("write other extension");
        fs::write(package_dir.join("skills/pkg.md"), "package skill").expect("write skill");
        fs::write(package_dir.join("prompts/review.md"), "review prompt").expect("write prompt");
        fs::write(package_dir.join("prompts/other.md"), "other prompt").expect("write prompt");
        fs::write(package_dir.join("themes/dark.json"), r#"{"name":"dark"}"#).expect("write theme");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert!(config
            .extensions
            .iter()
            .any(|resource| resource.name == "keep" && resource.content == "keep extension"));
        assert!(config
            .extensions
            .iter()
            .any(|resource| resource.name == "force" && resource.content == "force extension"));
        assert!(!config
            .extensions
            .iter()
            .any(|resource| resource.name == "skip"));
        assert!(!config
            .extensions
            .iter()
            .any(|resource| resource.name == "drop"));
        assert!(!config
            .extensions
            .iter()
            .any(|resource| resource.name == "other"));
        assert!(config.skills.is_empty());
        assert_eq!(config.prompt_templates.len(), 1);
        assert_eq!(config.prompt_templates[0].name, "review");
        assert!(config.themes.is_empty());
        assert!(config.diagnostics.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn agents_skill_dirs_load_skill_md_by_directory_name() {
        let root = test_dir("pi-config-agents-skills");
        let agent_dir = root.join("agent");
        let repo = root.join("repo");
        let cwd = repo.join("nested");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(cwd.join(".agents/skills/local")).expect("create local skill");
        fs::create_dir_all(repo.join(".agents/skills/repo")).expect("create repo skill");
        fs::write(repo.join(".git"), "").expect("mark git root");
        fs::write(
            cwd.join(".agents/skills/local/SKILL.md"),
            "local skill body",
        )
        .expect("write local skill");
        fs::write(repo.join(".agents/skills/repo/SKILL.md"), "repo skill body")
            .expect("write repo skill");
        fs::write(
            cwd.join(".agents/skills/ignored.md"),
            "ignored root markdown",
        )
        .expect("write ignored root markdown");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert!(config
            .skills
            .iter()
            .any(|resource| resource.name == "repo" && resource.content == "repo skill body"));
        assert!(config
            .skills
            .iter()
            .any(|resource| resource.name == "local" && resource.content == "local skill body"));
        assert!(!config
            .skills
            .iter()
            .any(|resource| resource.name == "ignored"));
        assert!(config.diagnostics.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resource_discovery_honors_ignore_files() {
        let root = test_dir("pi-config-resource-ignore");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        let package_dir = cwd.join("package");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(package_dir.join("extensions")).expect("create extensions");
        fs::create_dir_all(package_dir.join("skills")).expect("create skills");
        fs::create_dir_all(package_dir.join("prompts/ignored-dir")).expect("create prompts");
        fs::create_dir_all(package_dir.join("themes")).expect("create themes");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"packages":["package"]}"#,
        )
        .expect("write settings");
        fs::write(
            package_dir.join("prompts/.ignore"),
            "skip.md\nignored-dir/\n!keep.md\n",
        )
        .expect("write ignore");
        fs::write(package_dir.join("prompts/keep.md"), "keep prompt").expect("write keep prompt");
        fs::write(package_dir.join("prompts/skip.md"), "skip prompt").expect("write skip prompt");
        fs::write(
            package_dir.join("prompts/ignored-dir/nested.md"),
            "nested prompt",
        )
        .expect("write nested prompt");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert!(config
            .prompt_templates
            .iter()
            .any(|resource| resource.name == "keep"));
        assert!(!config
            .prompt_templates
            .iter()
            .any(|resource| resource.name == "skip"));
        assert!(!config
            .prompt_templates
            .iter()
            .any(|resource| resource.name == "nested"));
        assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_resources_are_filtered_from_loaded_config() {
        let root = test_dir("pi-config-disabled-resources");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        fs::create_dir_all(agent_dir.join("extensions")).expect("create extensions");
        fs::create_dir_all(agent_dir.join("prompts")).expect("create prompts");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"disabledResources":{"extensions":["assist"],"prompts":["prompt:fix"]}}"#,
        )
        .expect("write settings");
        fs::write(agent_dir.join("extensions/assist.md"), "assist extension")
            .expect("write assist");
        fs::write(agent_dir.join("extensions/keep.md"), "keep extension").expect("write keep");
        fs::write(agent_dir.join("prompts/fix.md"), "fix prompt").expect("write prompt");

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert!(!config
            .extensions
            .iter()
            .any(|resource| resource.name == "assist"));
        assert!(config
            .extensions
            .iter()
            .any(|resource| resource.name == "keep"));
        assert!(config.prompt_templates.is_empty());
        assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resource_name_collisions_emit_diagnostics() {
        let root = test_dir("pi-config-resource-collisions");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        fs::create_dir_all(agent_dir.join("extensions")).expect("create user extensions");
        fs::create_dir_all(cwd.join(".pi/extensions")).expect("create project extensions");
        fs::write(agent_dir.join("extensions/dup.md"), "user extension")
            .expect("write user extension");
        fs::write(cwd.join(".pi/extensions/dup.md"), "project extension")
            .expect("write project extension");

        let config = load_config(ConfigPaths {
            cwd,
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: root.join("repo/.pi/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        assert!(config
            .extensions
            .iter()
            .any(|resource| { resource.name == "dup" && resource.content == "project extension" }));
        assert!(
            config
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("resource name collision for dup")),
            "{:?}",
            config.diagnostics
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn model_cache_extends_builtin_and_explicit_models() {
        let root = test_dir("pi-config-model-cache");
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::write(
            agent_dir.join("models.json"),
            r#"[
                {"provider":"anthropic","id":"claude-custom","name":"Custom Claude","api":"anthropic"}
            ]"#,
        )
        .expect("write explicit models");
        write_model_cache(
            &agent_dir.join("model-cache.json"),
            &ModelCache {
                refreshed_at: 123,
                models: vec![ModelDefinition {
                    provider: "anthropic".to_string(),
                    id: "claude-opus-4-1-20250805".to_string(),
                    name: Some("Claude Opus 4.1".to_string()),
                    api: ProviderApi::Anthropic,
                    base_url: None,
                }],
                diagnostics: vec!["model refresh warning".to_string()],
            },
        )
        .expect("write model cache");

        let config = load_config(ConfigPaths {
            cwd,
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: root.join("missing-settings.json"),
            project_settings_path: root.join("missing-project-settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: agent_dir.join("models.json"),
            model_cache_path: agent_dir.join("model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config");

        let model_ids = config
            .models
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect::<Vec<_>>();
        assert!(model_ids.contains(&"faux/echo".to_string()));
        assert!(model_ids.contains(&"anthropic/claude-opus-4-1-20250805".to_string()));
        assert!(model_ids.contains(&"anthropic/claude-custom".to_string()));
        assert_eq!(config.diagnostics, ["model refresh warning"]);

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
            model_cache_path: cwd.join("agent/model-cache.json"),
            keybindings_path: cwd.join("agent/keybindings.json"),
        };

        let next = paths
            .with_session_dir("local-sessions")
            .expect("apply session dir");

        assert_eq!(next.session_dir, cwd.join("local-sessions"));
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn settings_behavior_matches_upstream_ts_fixture() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/settings.json"
        ))
        .expect("parse settings fixture");

        for case in fixture["cases"].as_array().expect("settings cases") {
            let name = case["name"].as_str().expect("case name");
            let root = test_dir(&format!("pi-config-settings-parity-{name}"));
            let agent_dir = root.join("agent");
            let cwd = root.join("repo");
            fs::create_dir_all(&agent_dir).expect("create agent dir");
            fs::create_dir_all(cwd.join(".pi")).expect("create project settings dir");

            let global = &case["globalSettings"];
            if !global.as_object().expect("global settings").is_empty() {
                fs::write(
                    agent_dir.join("settings.json"),
                    format!(
                        "{}\n",
                        serde_json::to_string_pretty(global).expect("serialize global")
                    ),
                )
                .expect("write global settings");
            }
            let project = &case["projectSettings"];
            if !project.as_object().expect("project settings").is_empty() {
                fs::write(
                    cwd.join(".pi/settings.json"),
                    format!(
                        "{}\n",
                        serde_json::to_string_pretty(project).expect("serialize project")
                    ),
                )
                .expect("write project settings");
            }

            let config = load_config(ConfigPaths {
                cwd: cwd.clone(),
                agent_dir: agent_dir.clone(),
                session_dir: agent_dir.join("sessions"),
                settings_path: agent_dir.join("settings.json"),
                project_settings_path: cwd.join(".pi/settings.json"),
                auth_path: root.join("missing-auth.json"),
                models_path: root.join("missing-models.json"),
                model_cache_path: root.join("missing-model-cache.json"),
                keybindings_path: root.join("missing-keybindings.json"),
            })
            .unwrap_or_else(|error| panic!("load config for {name}: {error}"));

            assert_eq!(
                settings_summary(&config.settings),
                case["expected"],
                "settings parity case {name}"
            );

            let _ = fs::remove_dir_all(root);
        }
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
                "warnings":{"anthropicExtraUsage":true},
                "modelRefresh":{"enabled":true,"ttlHours":24}
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
                "warnings":{"anthropicExtraUsage":false},
                "modelRefresh":{"ttlHours":12}
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
            model_cache_path: root.join("missing-model-cache.json"),
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
        assert_eq!(provider.timeout_ms, None);
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
        let model_refresh = config.settings.model_refresh.expect("model refresh");
        assert_eq!(model_refresh.enabled, Some(true));
        assert_eq!(model_refresh.ttl_hours, Some(12));

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
            "deepseek",
            "groq",
            "cerebras",
            "xai",
            "zai",
            "huggingface",
            "together",
            "moonshotai",
            "moonshotai-cn",
            "opencode",
            "opencode-go",
            "fireworks",
            "minimax",
            "minimax-cn",
            "kimi-coding",
            "xiaomi",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
            "xiaomi-token-plan-sgp",
            "vercel-ai-gateway",
            "google-vertex",
            "amazon-bedrock",
            "mistral",
            "cloudflare-workers-ai",
            "cloudflare-ai-gateway",
        ] {
            assert!(providers.iter().any(|candidate| candidate == provider));
        }

        let models = default_models()
            .into_iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect::<Vec<_>>();
        for model in [
            "anthropic/claude-opus-4-1-20250805",
            "anthropic/claude-opus-4-7",
            "anthropic/claude-sonnet-4-6",
            "openai-codex/gpt-5.4",
            "openai-codex/gpt-5.4-mini",
            "openai-codex/gpt-5.5",
            "deepseek/deepseek-v4-flash",
            "groq/openai/gpt-oss-120b",
            "cerebras/gpt-oss-120b",
            "xai/grok-code-fast-1",
            "zai/glm-5.1",
            "huggingface/MiniMaxAI/MiniMax-M2.7",
            "together/Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8",
            "moonshotai/kimi-k2-thinking",
            "moonshotai-cn/kimi-k2-thinking",
            "opencode/big-pickle",
            "opencode-go/deepseek-v4-flash",
            "fireworks/accounts/fireworks/models/deepseek-v4-pro",
            "minimax/MiniMax-M2.7",
            "minimax-cn/MiniMax-M2.7",
            "kimi-coding/kimi-for-coding",
            "xiaomi/mimo-v2.5-pro",
            "xiaomi-token-plan-cn/mimo-v2.5-pro",
            "xiaomi-token-plan-ams/mimo-v2.5-pro",
            "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            "vercel-ai-gateway/alibaba/qwen3-coder",
        ] {
            assert!(models.iter().any(|candidate| candidate == model));
        }
    }

    #[test]
    fn default_models_cover_upstream_ts_catalog_targets() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/model-catalog.json"
        ))
        .expect("parse model catalog fixture");
        assert!(
            fixture["providerCount"].as_u64().expect("provider count") >= 1,
            "upstream model catalog should expose providers"
        );
        let rust_models = default_models()
            .into_iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect::<BTreeSet<_>>();
        for target in fixture["targets"].as_array().expect("catalog targets") {
            assert_eq!(target["found"], true, "upstream target missing: {target}");
            let provider = target["provider"].as_str().expect("provider");
            let id = target["id"].as_str().expect("id");
            let model_ref = format!("{provider}/{id}");
            assert!(
                rust_models.contains(&model_ref),
                "missing upstream model catalog target {model_ref}"
            );
        }

        let opus = fixture["targets"]
            .as_array()
            .expect("catalog targets")
            .iter()
            .find(|target| target["provider"] == "anthropic" && target["id"] == "claude-opus-4-7")
            .expect("opus target");
        assert!(opus["thinkingLevels"]
            .as_array()
            .expect("opus thinking levels")
            .iter()
            .any(|level| level == "xhigh"));
    }

    #[test]
    fn default_image_models_include_openrouter_targets() {
        let models = default_image_models()
            .into_iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect::<Vec<_>>();

        for model in [
            "openrouter/google/gemini-3.1-flash-image-preview",
            "openrouter/openai/gpt-5-image",
            "openrouter/black-forest-labs/flux.2-pro",
            "openrouter/openrouter/auto",
        ] {
            assert!(models.iter().any(|candidate| candidate == model));
        }
    }

    #[test]
    fn env_auth_detects_provider_parity_targets() {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let empty_auth = BTreeMap::new();
        let saved = [
            save_env("AZURE_OPENAI_API_KEY"),
            save_env("CEREBRAS_API_KEY"),
            save_env("COPILOT_GITHUB_TOKEN"),
            save_env("DEEPSEEK_API_KEY"),
            save_env("FIREWORKS_API_KEY"),
            save_env("GROQ_API_KEY"),
            save_env("HF_TOKEN"),
            save_env("KIMI_API_KEY"),
            save_env("MINIMAX_API_KEY"),
            save_env("MINIMAX_CN_API_KEY"),
            save_env("MOONSHOT_API_KEY"),
            save_env("OPENCODE_API_KEY"),
            save_env("OPENROUTER_API_KEY"),
            save_env("GOOGLE_CLOUD_API_KEY"),
            save_env("AWS_BEARER_TOKEN_BEDROCK"),
            save_env("MISTRAL_API_KEY"),
            save_env("CLOUDFLARE_API_KEY"),
            save_env("TOGETHER_API_KEY"),
            save_env("AI_GATEWAY_API_KEY"),
            save_env("XAI_API_KEY"),
            save_env("XIAOMI_API_KEY"),
            save_env("XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
            save_env("XIAOMI_TOKEN_PLAN_CN_API_KEY"),
            save_env("XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
            save_env("ZAI_API_KEY"),
            save_env("CODEX_ACCESS_TOKEN"),
            save_env("CHATGPT_ACCOUNT_ID"),
        ];

        env::set_var("AZURE_OPENAI_API_KEY", "azure-key");
        env::set_var("CEREBRAS_API_KEY", "cerebras-key");
        env::set_var("COPILOT_GITHUB_TOKEN", "copilot-key");
        env::set_var("DEEPSEEK_API_KEY", "deepseek-key");
        env::set_var("FIREWORKS_API_KEY", "fireworks-key");
        env::set_var("GROQ_API_KEY", "groq-key");
        env::set_var("HF_TOKEN", "hf-key");
        env::set_var("KIMI_API_KEY", "kimi-key");
        env::set_var("MINIMAX_API_KEY", "minimax-key");
        env::set_var("MINIMAX_CN_API_KEY", "minimax-cn-key");
        env::set_var("MOONSHOT_API_KEY", "moonshot-key");
        env::set_var("OPENCODE_API_KEY", "opencode-key");
        env::set_var("OPENROUTER_API_KEY", "openrouter-key");
        env::set_var("GOOGLE_CLOUD_API_KEY", "vertex-key");
        env::set_var("AWS_BEARER_TOKEN_BEDROCK", "bedrock-token");
        env::set_var("MISTRAL_API_KEY", "mistral-key");
        env::set_var("CLOUDFLARE_API_KEY", "cloudflare-key");
        env::set_var("TOGETHER_API_KEY", "together-key");
        env::set_var("AI_GATEWAY_API_KEY", "vercel-key");
        env::set_var("XAI_API_KEY", "xai-key");
        env::set_var("XIAOMI_API_KEY", "xiaomi-key");
        env::set_var("XIAOMI_TOKEN_PLAN_AMS_API_KEY", "xiaomi-ams-key");
        env::set_var("XIAOMI_TOKEN_PLAN_CN_API_KEY", "xiaomi-cn-key");
        env::set_var("XIAOMI_TOKEN_PLAN_SGP_API_KEY", "xiaomi-sgp-key");
        env::set_var("ZAI_API_KEY", "zai-key");
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
            auth_for_provider(&empty_auth, "deepseek"),
            Some(ResolvedAuth::ApiKey("deepseek-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "groq"),
            Some(ResolvedAuth::ApiKey("groq-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "cerebras"),
            Some(ResolvedAuth::ApiKey("cerebras-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "xai"),
            Some(ResolvedAuth::ApiKey("xai-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "zai"),
            Some(ResolvedAuth::ApiKey("zai-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "huggingface"),
            Some(ResolvedAuth::ApiKey("hf-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "together"),
            Some(ResolvedAuth::ApiKey("together-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "moonshotai"),
            Some(ResolvedAuth::ApiKey("moonshot-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "moonshotai-cn"),
            Some(ResolvedAuth::ApiKey("moonshot-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "opencode"),
            Some(ResolvedAuth::ApiKey("opencode-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "opencode-go"),
            Some(ResolvedAuth::ApiKey("opencode-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "fireworks"),
            Some(ResolvedAuth::ApiKey("fireworks-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "minimax"),
            Some(ResolvedAuth::ApiKey("minimax-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "minimax-cn"),
            Some(ResolvedAuth::ApiKey("minimax-cn-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "kimi-coding"),
            Some(ResolvedAuth::ApiKey("kimi-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "xiaomi"),
            Some(ResolvedAuth::ApiKey("xiaomi-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "xiaomi-token-plan-cn"),
            Some(ResolvedAuth::ApiKey("xiaomi-cn-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "xiaomi-token-plan-ams"),
            Some(ResolvedAuth::ApiKey("xiaomi-ams-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "xiaomi-token-plan-sgp"),
            Some(ResolvedAuth::ApiKey("xiaomi-sgp-key".to_string()))
        );
        assert_eq!(
            auth_for_provider(&empty_auth, "vercel-ai-gateway"),
            Some(ResolvedAuth::ApiKey("vercel-key".to_string()))
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

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
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub append_system_prompt: Vec<String>,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub enabled_models: Option<Vec<String>>,
    pub theme: Option<String>,
    #[serde(default)]
    pub quiet_startup: Option<bool>,
    #[serde(default)]
    pub session_dir: Option<String>,
}

impl Settings {
    fn merge(self, overrides: Settings) -> Settings {
        Settings {
            default_provider: overrides.default_provider.or(self.default_provider),
            default_model: overrides.default_model.or(self.default_model),
            default_thinking_level: overrides
                .default_thinking_level
                .or(self.default_thinking_level),
            system_prompt: overrides.system_prompt.or(self.system_prompt),
            append_system_prompt: if overrides.append_system_prompt.is_empty() {
                self.append_system_prompt
            } else {
                overrides.append_system_prompt
            },
            shell_path: overrides.shell_path.or(self.shell_path),
            shell_command_prefix: overrides.shell_command_prefix.or(self.shell_command_prefix),
            enabled_tools: overrides.enabled_tools.or(self.enabled_tools),
            enabled_models: overrides.enabled_models.or(self.enabled_models),
            theme: overrides.theme.or(self.theme),
            quiet_startup: overrides.quiet_startup.or(self.quiet_startup),
            session_dir: overrides.session_dir.or(self.session_dir),
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
    let settings = global_settings
        .unwrap_or_default()
        .merge(project_settings.unwrap_or_default());
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
    let skills = load_resource_files(&paths, "skills", &mut diagnostics);
    let prompt_templates = load_resource_files(&paths, "prompts", &mut diagnostics);
    let themes = load_resource_files(&paths, "themes", &mut diagnostics);
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
        fs::create_dir_all(cwd.join(".pi/themes")).expect("create project themes");
        fs::write(agent_dir.join("skills/review.md"), "review skill").expect("write skill");
        fs::write(agent_dir.join("prompts/fix.md"), "fix {{input}}").expect("write prompt");
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
        assert_eq!(config.skills[0].name, "review");
        assert_eq!(config.skills[0].content, "review skill");
        assert_eq!(config.prompt_templates[0].name, "fix");
        assert_eq!(config.themes[0].name, "dark");
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

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
    ApiKey { key: String },
    OAuth { access_token: String, expires: u64 },
}

pub type AuthData = BTreeMap<String, AuthCredential>;

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
#[serde(rename_all = "kebab-case")]
pub enum ProviderApi {
    #[default]
    OpenAi,
    Anthropic,
    Google,
    Faux,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingConfig {
    pub action: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub paths: ConfigPaths,
    pub settings: Settings,
    pub auth: AuthData,
    pub models: Vec<ModelDefinition>,
    pub keybindings: Vec<KeybindingConfig>,
    pub context_files: Vec<ContextFile>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
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
    let models = read_optional_json::<Vec<ModelDefinition>>(&paths.models_path)?
        .unwrap_or_else(default_models);
    let keybindings =
        read_optional_json::<Vec<KeybindingConfig>>(&paths.keybindings_path)?.unwrap_or_default();
    let context_files = load_context_files(&paths.cwd, &paths.agent_dir)?;
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
        system_prompt,
        append_system_prompt,
    })
}

pub fn api_key_for_provider(auth: &AuthData, provider: &str) -> Option<String> {
    if let Some(AuthCredential::ApiKey { key }) = auth.get(provider) {
        return Some(resolve_config_value(key));
    }
    env_api_key(provider).or_else(|| {
        env::var("OPENAI_API_KEY")
            .ok()
            .filter(|_| provider == "openai")
    })
}

fn env_api_key(provider: &str) -> Option<String> {
    let names = match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"][..],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        _ => &[],
    };
    names.iter().find_map(|name| env::var(name).ok())
}

fn resolve_config_value(value: &str) -> String {
    if let Some(name) = value.strip_prefix("env:") {
        return env::var(name).unwrap_or_default();
    }
    value.to_string()
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
            id: "gpt-4.1".to_string(),
            name: Some("OpenAI GPT".to_string()),
            api: ProviderApi::OpenAi,
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
            provider: "google".to_string(),
            id: "gemini-2.5-pro".to_string(),
            name: Some("Gemini Pro".to_string()),
            api: ProviderApi::Google,
            base_url: None,
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

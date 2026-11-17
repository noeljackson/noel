mod agents;
mod frontmatter;
pub mod mcp;
mod prompt_templates;
mod resources;
mod skills;

pub use agents::AgentDefinition;
pub use frontmatter::{js_trim, strip_frontmatter};
pub use prompt_templates::{expand_prompt_template, PromptTemplate};
pub use resources::{ResourceOptions, SourceInfo, SourceScope};
pub use skills::{format_skills_for_prompt, Skill, SkillReader};

use std::collections::{BTreeMap, BTreeSet};

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

mod auth;
mod context_files;
pub mod env;
mod model_resolver;
mod resource_files;
mod settings;
#[allow(unused_imports)]
pub use auth::*;
#[allow(unused_imports)]
pub use context_files::*;
#[allow(unused_imports)]
pub use resource_files::*;
#[allow(unused_imports)]
pub use settings::*;

pub use model_resolver::{
    custom_model, find_exact_model_reference_match, find_initial_model, resolve_cli_model,
    resolve_model_scope, CliModel, ModelScope, ScopedModel,
};
pub use noel_ai::catalog::{ModelCost, ThinkingLevelMap, DEFAULT_THINKING_LEVEL, THINKING_LEVELS};

pub const APP_NAME: &str = "noel";
pub const CONFIG_DIR_NAME: &str = ".noel";
/// pi's directory: projects written for pi track `.pi/`, and pi kept its
/// state in `~/.pi/agent`.
pub const PI_CONFIG_DIR_NAME: &str = ".pi";
pub const ENV_AGENT_DIR: &str = env::AGENT_DIR;
pub const ENV_SESSION_DIR: &str = env::SESSION_DIR;
/// Set by the test harness (`make test`, and every `noel` the CLI tests run).
/// noel then refuses to fall back to `~/.noel`, so a test must name its
/// agent dir with `NOEL_DIR`, imports no Claude Code or Codex
/// login from `$HOME` and no `~/.pi/agent`, reads no `~/.agents/skills`, and
/// starts no background model refresh.
pub const ENV_TEST_ISOLATION: &str = env::TEST_ISOLATION;
pub const PROJECT_TRUST_FILE_NAME: &str = "trust.json";

/// True under the test harness; see [`ENV_TEST_ISOLATION`].
pub fn test_isolation() -> bool {
    std::env::var_os(ENV_TEST_ISOLATION).is_some()
}
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
    #[error(
        "NOEL_TEST_ISOLATION is set: refusing the default agent dir ~/.noel; set NOEL_DIR to a test directory"
    )]
    DefaultAgentDirInTest,
    #[error("failed to import {path}: {source}")]
    Import {
        path: PathBuf,
        source: std::io::Error,
    },
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
    /// The paths of noel's state and of `cwd`'s project. The agent dir is
    /// `NOEL_DIR`, else `~/.noel`.
    ///
    /// The default directory is the only one pi's state is imported into:
    /// the first time it is asked for, when it does not exist and pi's
    /// `~/.pi/agent` does, it is created from pi's (see [`PiImport`]), and
    /// the import is returned for the caller to report. So no command can
    /// create `~/.noel` without the import. Nothing else here writes, and a
    /// command asks for its paths only once it has parsed its arguments:
    /// `--version`, `--help`, and a usage error leave the disk as it was.
    pub fn discover(
        cwd: impl Into<PathBuf>,
        session_dir_override: Option<PathBuf>,
    ) -> Result<(Self, Option<PiImport>), ConfigError> {
        let cwd = cwd.into();
        let mut import = None;
        let agent_dir = match std::env::var_os(ENV_AGENT_DIR) {
            Some(value) => expand_tilde(PathBuf::from(value))?,
            None if test_isolation() => return Err(ConfigError::DefaultAgentDirInTest),
            None => {
                let home = home_dir()?;
                let agent_dir = home.join(CONFIG_DIR_NAME);
                import =
                    import_agent_dir(&home.join(PI_CONFIG_DIR_NAME).join("agent"), &agent_dir)?;
                agent_dir
            }
        };
        let session_dir = match session_dir_override {
            Some(value) => expand_tilde(value)?,
            None => match std::env::var_os(ENV_SESSION_DIR) {
                Some(value) => expand_tilde(PathBuf::from(value))?,
                None => agent_dir.join("sessions"),
            },
        };

        let paths = Self {
            cwd: cwd.clone(),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: project_config_dir(&cwd).join("settings.json"),
            auth_path: agent_dir.join("auth.json"),
            models_path: agent_dir.join("models.json"),
            model_cache_path: agent_dir.join("model-cache.json"),
            keybindings_path: agent_dir.join("keybindings.json"),
            agent_dir,
            session_dir,
        };
        Ok((paths, import))
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

/// A project's config directory: `.noel/`, or the `.pi/` of a project written
/// for pi when it has no `.noel/`. Everything a project configures is read
/// from this one directory (settings, `SYSTEM.md`, `APPEND_SYSTEM.md`,
/// extensions, skills, prompts and themes) and written there, so the two
/// directories are never mixed.
pub fn project_config_dir(cwd: &Path) -> PathBuf {
    let dir = cwd.join(CONFIG_DIR_NAME);
    let pi_dir = cwd.join(PI_CONFIG_DIR_NAME);
    if !dir.exists() && pi_dir.is_dir() {
        pi_dir
    } else {
        dir
    }
}

/// The names noel changed, as upstream's fixtures (`tests/fixtures/ts-parity`)
/// spell them, each with noel's: the product in the system prompt's first
/// sentence (a placeholder in the fixture), the variables its bash rule
/// names, and a project's directory. The fixtures are upstream's captured
/// output and stay as they are, so the parity tests read them through
/// [`with_noel_names`]; "Named noel, With Its State in `~/.noel`" in
/// `docs/rust-rewrite/non-parity-register.md` records the same list.
pub const UPSTREAM_NAMES: [(&str, &str); 3] = [
    ("<product>", APP_NAME),
    ("PI_*", "NOEL_*"),
    (PI_CONFIG_DIR_NAME, CONFIG_DIR_NAME),
];

/// Upstream's text with noel's names: each of [`UPSTREAM_NAMES`], the
/// directory only where it is a whole component of a path (`repo/.pi/skills`,
/// `repo/.pi`), never inside a word or a sentence.
pub fn with_noel_names(upstream: &str) -> String {
    let [product, variables, (pi_dir, dir)] = UPSTREAM_NAMES;
    let text = upstream
        .replace(product.0, product.1)
        .replace(variables.0, variables.1);
    let components = text.split('/');
    components
        .map(|component| if component == pi_dir { dir } else { component })
        .collect::<Vec<_>>()
        .join("/")
}

/// [`with_noel_names`] for every string of a fixture value, keys included.
pub fn value_with_noel_names(upstream: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match upstream {
        Value::String(text) => Value::String(with_noel_names(text)),
        Value::Array(items) => Value::Array(items.iter().map(value_with_noel_names).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (with_noel_names(key), value_with_noel_names(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// What is imported from pi's `~/.pi/agent`: everything noel reads there
/// except the caches, which it rebuilds.
const PI_IMPORTED_ENTRIES: [&str; 17] = [
    "settings.json",
    "trust.json",
    "keybindings.json",
    "models.json",
    "auth.json",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
    "skills",
    "prompts",
    "themes",
    "extensions",
    "sessions",
];

/// What the first use of noel's default directory imported from pi's
/// `~/.pi/agent`: see [`ConfigPaths::discover`], the only place that imports.
/// `NOEL_DIR` names another directory and the test harness
/// ([`ENV_TEST_ISOLATION`]) refuses the default one, so nothing is imported
/// under either. `~/.pi` is never modified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiImport {
    pub from: PathBuf,
    pub to: PathBuf,
    pub imported: Vec<String>,
    /// Files left in `from` because noel cannot load them, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// Copies the imported entries of `from` into a new `to`. The copy is staged
/// in a sibling directory and renamed into place, so an interrupted import
/// leaves no `to` behind and the next run imports again.
fn import_agent_dir(from: &Path, to: &Path) -> Result<Option<PiImport>, ConfigError> {
    if fs::symlink_metadata(to).is_ok() || !from.is_dir() {
        return Ok(None);
    }
    let mut staging = to.as_os_str().to_owned();
    staging.push(format!(".import-{}", std::process::id()));
    let staging = PathBuf::from(staging);
    let _ = fs::remove_dir_all(&staging);
    let mut import = PiImport {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        imported: Vec::new(),
        skipped: Vec::new(),
    };
    let result = stage_import(&staging, &mut import)
        .and_then(|()| fs::rename(&staging, to).map_err(import_error(to)));
    match result {
        Ok(()) => Ok(Some(import)),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            // Another noel process finished the import first.
            if fs::symlink_metadata(to).is_ok() {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

fn stage_import(staging: &Path, import: &mut PiImport) -> Result<(), ConfigError> {
    fs::create_dir(staging).map_err(import_error(staging))?;
    for entry in PI_IMPORTED_ENTRIES {
        let path = import.from.join(entry);
        if fs::symlink_metadata(&path).is_err() {
            continue;
        }
        // noel refuses to start with a models.json or keybindings.json it
        // cannot load, so the import leaves such a file behind.
        let loadable = match entry {
            "models.json" => read_optional_json::<Vec<ModelDefinition>>(&path).map(drop),
            "keybindings.json" => read_optional_json::<KeybindingsFile>(&path).map(drop),
            _ => Ok(()),
        };
        if let Err(error) = loadable {
            import.skipped.push((entry.to_string(), error.to_string()));
            continue;
        }
        copy_tree(&path, &staging.join(entry))?;
        import.imported.push(entry.to_string());
    }
    let permissions = fs::metadata(&import.from)
        .map_err(import_error(&import.from))?
        .permissions();
    fs::set_permissions(staging, permissions).map_err(import_error(staging))
}

fn import_error(path: &Path) -> impl FnOnce(std::io::Error) -> ConfigError + '_ {
    move |source| ConfigError::Import {
        path: path.to_path_buf(),
        source,
    }
}

/// Copies a directory tree with its permissions.
fn copy_tree(from: &Path, to: &Path) -> Result<(), ConfigError> {
    let metadata = fs::symlink_metadata(from).map_err(import_error(from))?;
    if !metadata.is_dir() {
        return copy_file(from, to, &metadata).map_err(import_error(from));
    }
    fs::create_dir(to).map_err(import_error(from))?;
    for entry in fs::read_dir(from).map_err(import_error(from))? {
        let entry = entry.map_err(import_error(from))?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    fs::set_permissions(to, metadata.permissions()).map_err(import_error(from))
}

/// Copies a file with its permissions, synced before the import is renamed
/// into place, or a symlink as a symlink. Sockets and fifos are not state.
fn copy_file(from: &Path, to: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return fs::copy(from, to).map(drop);
    }
    if metadata.is_file() {
        fs::copy(from, to)?;
        fs::File::open(to)?.sync_all()?;
    }
    Ok(())
}

pub fn save_project_trust(
    agent_dir: &Path,
    cwd: &Path,
    trusted: bool,
) -> Result<PathBuf, ConfigError> {
    let trust_path = agent_dir.join(PROJECT_TRUST_FILE_NAME);
    let mut decisions =
        read_optional_json::<BTreeMap<String, bool>>(&trust_path)?.unwrap_or_default();
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    decisions.insert(cwd.to_string_lossy().into_owned(), trusted);
    let content =
        serde_json::to_string_pretty(&decisions).expect("serialize project trust decisions");
    fs::create_dir_all(agent_dir).map_err(|source| ConfigError::Write {
        path: agent_dir.to_path_buf(),
        source,
    })?;
    write_file_atomic(&trust_path, format!("{content}\n").as_bytes())?;
    Ok(trust_path)
}

/// A per-call unique id for test temp directories.
#[cfg(test)]
pub(crate) fn unique_test_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn project_is_trusted(agent_dir: &Path, cwd: &Path) -> Result<bool, ConfigError> {
    let trust_path = agent_dir.join(PROJECT_TRUST_FILE_NAME);
    let decisions = read_optional_json::<BTreeMap<String, bool>>(&trust_path)?.unwrap_or_default();
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Ok(decisions
        .get(&cwd.to_string_lossy().into_owned())
        .copied()
        .unwrap_or(false))
}

pub fn codex_client_version() -> Option<String> {
    #[derive(Deserialize)]
    struct CodexModelsCache {
        client_version: Option<String>,
    }

    let path = home_dir().ok()?.join(".codex").join("models_cache.json");
    read_optional_json::<CodexModelsCache>(&path)
        .ok()
        .flatten()?
        .client_version
        .filter(|version| !version.trim().is_empty())
}

/// A model noel can be pointed at. The fields are upstream's model object's
/// and keep its names, so a built-in model, a `models.json` entry, and a
/// cached listing all describe a model the same way. Everything past the
/// endpoint is optional: a loaded config fills what an entry leaves out
/// from the built-in catalog (see `merge_model_definitions`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Context size in tokens. Custom models can override the compiled catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The most output tokens one reply may have. Without it requests carry
    /// no output cap, except where the API requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Whether the model has thinking levels. Absent means it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Which thinking levels a reasoning model supports and what the
    /// provider is sent for each (upstream `thinkingLevelMap`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    /// What the model accepts: `text`, `image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    /// Prices in US dollars per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl ModelDefinition {
    pub fn model_ref(&self) -> noel_ai::ModelRef {
        noel_ai::ModelRef {
            provider: self.provider.clone(),
            id: self.id.clone(),
        }
    }

    pub fn reasoning(&self) -> bool {
        self.reasoning.unwrap_or(false)
    }

    /// The thinking levels the model can be set to, lowest first; `off`
    /// alone for a model without reasoning.
    pub fn supported_thinking_levels(&self) -> Vec<&'static str> {
        noel_ai::catalog::supported_thinking_levels(
            self.reasoning(),
            self.thinking_level_map.as_ref(),
        )
    }

    /// `level` if the model supports it, else the nearest level it does
    /// (upstream `clampThinkingLevel`).
    pub fn clamp_thinking_level(&self, level: &str) -> &'static str {
        noel_ai::catalog::clamp_thinking_level(
            self.reasoning(),
            self.thinking_level_map.as_ref(),
            level,
        )
    }

    /// What request builders read of the model.
    pub fn traits(&self) -> noel_ai::ModelTraits {
        noel_ai::ModelTraits {
            context_window: self.context_window.filter(|window| *window > 0),
            max_tokens: self.max_tokens.filter(|tokens| *tokens > 0),
            reasoning: Some(self.reasoning()),
            thinking_level_map: self.thinking_level_map.clone(),
            input_modalities: self.input.clone(),
            // A model outside the catalog has no compat flags: its Responses
            // requests keep tools in the top-level field.
            supports_mid_convo_system_messages: None,
            supports_additional_tools: None,
        }
    }
}

impl From<&noel_ai::catalog::CatalogModel> for ModelDefinition {
    fn from(model: &noel_ai::catalog::CatalogModel) -> Self {
        Self {
            provider: model.provider.clone(),
            id: model.id.clone(),
            name: Some(model.name.clone()),
            api: ProviderApi::from(&model.api),
            base_url: model.base_url.clone(),
            context_window: Some(model.context_window),
            max_tokens: Some(model.max_tokens),
            reasoning: Some(model.reasoning),
            thinking_level_map: model.thinking_level_map.clone(),
            input: Some(model.input.clone()),
            cost: Some(model.cost.clone()),
        }
    }
}

/// The context size compaction and the output cap work with: the model's
/// own, else a conservative fallback for a model nothing is known about.
pub fn model_context_window(model: &ModelDefinition) -> u64 {
    model
        .context_window
        .filter(|window| *window > 0)
        .or_else(|| {
            noel_ai::catalog::builtin_model(&model.provider, &model.id)
                .map(|builtin| builtin.context_window)
        })
        .unwrap_or(128_000)
}

/// Upstream's Azure provider id. Before the built-in catalog came from
/// upstream this port called the provider `azure-openai-responses`; saved
/// settings, `auth.json` entries, sessions, and `models.json` may still.
pub const LEGACY_AZURE_PROVIDER: &str = noel_ai::LEGACY_AZURE_PROVIDER;

/// `provider` as the catalog names it: the Azure id this port used to use
/// becomes upstream's.
pub fn canonical_provider(provider: &str) -> &str {
    if provider == LEGACY_AZURE_PROVIDER {
        noel_ai::AZURE_PROVIDER
    } else {
        provider
    }
}

/// Caches written by older builds (or without this field) are treated as stale
/// so refreshed model lists pick up fetcher fixes immediately.
pub const MODEL_CACHE_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCache {
    pub refreshed_at: u64,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub models: Vec<ModelDefinition>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    /// Latest published Claude Code version, fetched from the npm registry
    /// during refresh and used as the `claude-cli/<version>` user-agent.
    #[serde(default)]
    pub claude_code_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default)]
    pub balance: Option<Balance>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub label: String,
    pub used_pct: f64,
    #[serde(default)]
    pub resets_at: Option<i64>,
    /// Whether the provider marks this limit as the binding one right now
    /// (Anthropic's `limits[].is_active`); `None` when the provider does
    /// not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    /// The provider's severity for this limit (`normal`/`critical`);
    /// `None` when not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub amount: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCache {
    pub refreshed_at: u64,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountUsage>,
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

impl From<&noel_ai::ProviderApi> for ProviderApi {
    fn from(api: &noel_ai::ProviderApi) -> Self {
        match api {
            noel_ai::ProviderApi::Faux => Self::Faux,
            noel_ai::ProviderApi::OpenAi => Self::OpenAi,
            noel_ai::ProviderApi::OpenAiResponses => Self::OpenAiResponses,
            noel_ai::ProviderApi::OpenAiCodexResponses => Self::OpenAiCodexResponses,
            noel_ai::ProviderApi::AzureOpenAiResponses => Self::AzureOpenAiResponses,
            noel_ai::ProviderApi::Anthropic => Self::Anthropic,
            noel_ai::ProviderApi::Google => Self::Google,
            noel_ai::ProviderApi::GoogleVertex => Self::GoogleVertex,
            noel_ai::ProviderApi::Bedrock => Self::Bedrock,
            noel_ai::ProviderApi::Mistral => Self::Mistral,
        }
    }
}

impl From<&ProviderApi> for noel_ai::ProviderApi {
    fn from(api: &ProviderApi) -> Self {
        match api {
            ProviderApi::Faux => Self::Faux,
            ProviderApi::OpenAi => Self::OpenAi,
            ProviderApi::OpenAiResponses => Self::OpenAiResponses,
            ProviderApi::OpenAiCodexResponses => Self::OpenAiCodexResponses,
            ProviderApi::AzureOpenAiResponses => Self::AzureOpenAiResponses,
            ProviderApi::Anthropic => Self::Anthropic,
            ProviderApi::Google => Self::Google,
            ProviderApi::GoogleVertex => Self::GoogleVertex,
            ProviderApi::Bedrock => Self::Bedrock,
            ProviderApi::Mistral => Self::Mistral,
        }
    }
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
    /// Every model the config knows: built-in, listed by a provider, or
    /// from `models.json`.
    pub models: Vec<ModelDefinition>,
    /// What `--models` or the `enabledModels` setting selects of them: the
    /// models startup and the model picker start from. Empty when there is
    /// no scope. `load_config` leaves it empty: which models a scope
    /// selects depends on credentials and the command line, so the caller
    /// resolves it ([`resolve_model_scope`]).
    pub model_scope: Vec<ScopedModel>,
    pub image_models: Vec<ImageModelDefinition>,
    pub keybindings: Vec<KeybindingConfig>,
    pub context_files: Vec<ContextFile>,
    pub extensions: Vec<ResourceFile>,
    /// In the order they loaded; the first of a name won.
    pub skills: Vec<Skill>,
    /// Subagent definitions, in load order; the first of a name won.
    pub agents: Vec<AgentDefinition>,
    /// MCP servers, in load order; the first of a name won.
    pub mcp_servers: Vec<mcp::McpServer>,
    pub prompt_templates: Vec<PromptTemplate>,
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
    load_config_with_project_trust(paths, true)
}

pub fn load_config_with_project_trust(
    paths: ConfigPaths,
    project_trusted: bool,
) -> Result<LoadedConfig, ConfigError> {
    load_config_with_resources(paths, project_trusted, &ResourceOptions::from_env())
}

/// Loads the configuration with the command line's skill and prompt
/// template flags, which decide what is discovered and what loads after it.
pub fn load_config_with_resources(
    paths: ConfigPaths,
    project_trusted: bool,
    resource_options: &ResourceOptions,
) -> Result<LoadedConfig, ConfigError> {
    let global_settings = read_optional_settings(&paths.settings_path)?;
    let project_settings = if project_trusted {
        read_optional_settings(&paths.project_settings_path)?
    } else {
        None
    };
    let global_settings = global_settings.unwrap_or_default();
    let project_settings = project_settings.unwrap_or_default();
    let settings = global_settings
        .clone()
        .merge(project_settings.clone())
        .with_canonical_providers();
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
    // Every known model. `enabledModels` scopes model selection without
    // removing models (`resolve_model_scope`).
    let models = merge_model_definitions(
        with_base_url_env(default_models(), true, read_non_empty_env),
        cache
            .as_ref()
            .map(|cache| cache.models.clone())
            .unwrap_or_default(),
        with_base_url_env(explicit_models, false, read_non_empty_env),
    );
    let image_models = default_image_models();
    let keybindings = read_optional_json::<KeybindingsFile>(&paths.keybindings_path)?
        .map(KeybindingsFile::into_keybindings)
        .unwrap_or_default();
    let context_files = load_context_files(&paths.cwd, &paths.agent_dir, &mut diagnostics);
    let mut extensions =
        load_resource_files(&paths, "extensions", project_trusted, &mut diagnostics);
    let mut themes = load_resource_files(&paths, "themes", project_trusted, &mut diagnostics);
    extend_resource_files(
        &mut extensions,
        &paths.agent_dir,
        &global_settings.extensions,
        &mut diagnostics,
    );
    extend_resource_files(
        &mut themes,
        &paths.agent_dir,
        &global_settings.themes,
        &mut diagnostics,
    );
    if project_trusted {
        let project_base = project_config_dir(&paths.cwd);
        extend_resource_files(
            &mut extensions,
            &project_base,
            &project_settings.extensions,
            &mut diagnostics,
        );
        extend_resource_files(
            &mut themes,
            &project_base,
            &project_settings.themes,
            &mut diagnostics,
        );
    }
    load_package_resources(
        &paths.cwd,
        &settings.packages,
        &mut extensions,
        &mut themes,
        &mut diagnostics,
    );
    let resources::LoadedResources {
        mut skills,
        skill_diagnostics,
        mut prompt_templates,
        prompt_diagnostics,
    } = resources::load_resources(
        &resources::ResourceSources {
            paths: &paths,
            project_trusted,
            user_settings: &global_settings,
            project_settings: &project_settings,
            packages: &settings.packages,
            options: resource_options,
        },
        &mut diagnostics,
    );
    diagnostics.extend(skill_diagnostics.iter().map(|d| d.describe("skill")));
    diagnostics.extend(
        prompt_diagnostics
            .iter()
            .map(|d| d.describe("prompt template")),
    );
    let (agents, agent_diagnostics) = agents::load_agents(&paths);
    diagnostics.extend(agent_diagnostics.iter().map(|d| d.describe("agent")));
    let project_mcp_dir = project_trusted.then(|| project_config_dir(&paths.cwd));
    let (mcp_servers, mcp_diagnostics) =
        mcp::load_mcp_servers(&paths.agent_dir, project_mcp_dir.as_deref());
    diagnostics.extend(mcp_diagnostics);
    let disabled = settings.disabled_resources.as_ref();
    let enabled = |resource_type, name: &str, path: &Path| {
        !resource_disabled(disabled, resource_type, name, path)
    };
    extensions.retain(|file| enabled(PackageResourceType::Extensions, &file.name, &file.path));
    skills.retain(|skill| enabled(PackageResourceType::Skills, &skill.name, &skill.file_path));
    prompt_templates.retain(|template| {
        enabled(
            PackageResourceType::Prompts,
            &template.name,
            &template.file_path,
        )
    });
    themes.retain(|file| enabled(PackageResourceType::Themes, &file.name, &file.path));
    // The `systemPrompt` and `appendSystemPrompt` settings stand where
    // upstream's flags do: a discovered file applies only without them.
    let system_prompt = match resolve_prompt_input(settings.system_prompt.as_deref())? {
        Some(prompt) => Some(prompt),
        None => discovered_prompt_file(&paths, "SYSTEM.md", project_trusted, &mut diagnostics),
    };
    let mut append_system_prompt = settings
        .append_system_prompt
        .iter()
        .map(|source| {
            resolve_prompt_input(Some(source))
                .map(|content| content.unwrap_or_else(|| source.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if append_system_prompt.is_empty() {
        append_system_prompt.extend(discovered_prompt_file(
            &paths,
            "APPEND_SYSTEM.md",
            project_trusted,
            &mut diagnostics,
        ));
    }

    Ok(LoadedConfig {
        paths,
        settings,
        auth,
        models,
        model_scope: Vec::new(),
        image_models,
        keybindings,
        context_files,
        extensions,
        skills,
        agents,
        mcp_servers,
        prompt_templates,
        themes,
        diagnostics,
        system_prompt,
        append_system_prompt,
    })
}

pub fn read_model_cache(path: &Path) -> Result<Option<ModelCache>, ConfigError> {
    read_optional_json::<ModelCache>(path)
}

pub fn read_usage_cache(path: &Path) -> Result<Option<UsageCache>, ConfigError> {
    read_optional_json::<UsageCache>(path)
}

pub fn write_usage_cache(path: &Path, cache: &UsageCache) -> Result<(), ConfigError> {
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
    write_file_atomic(path, format!("{content}\n").as_bytes())
}

/// Creates `dir` and any missing parents owner-only; an existing directory
/// keeps its permissions.
pub fn create_private_dir_all(dir: &Path) -> Result<(), ConfigError> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|source| ConfigError::Write {
        path: dir.to_path_buf(),
        source,
    })
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    write_file_atomic_with_mode(path, contents, None)
}

/// Writes a file only its owner may read (`0600` from before its first
/// byte), atomically: for credentials kept beside `auth.json`.
pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    write_file_atomic_with_mode(path, contents, Some(0o600))
}

/// `mode` holds Unix permission bits for the new file, set before any
/// content is written.
fn write_file_atomic_with_mode(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
) -> Result<(), ConfigError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let temp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp_path)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp_path, path)
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(ConfigError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
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
    write_file_atomic(path, format!("{content}\n").as_bytes())
}

/// The models a config knows: the built-in catalog, then the cached
/// provider listings, then `models.json`, later entries replacing earlier
/// ones for the same provider and id.
///
/// A cached listing says which models a provider serves, not how to reach
/// them or what they can do. For a provider the catalog has, a listed id
/// the catalog also has changes nothing, and one it lacks is built onto the
/// provider's default model, as `--model provider/new-id` is
/// (`custom_model`), keeping the listing's name and context window. A
/// `models.json` entry is used as written (provider, id, name, API, and
/// base URL), and takes what it leaves out (context window, output cap,
/// reasoning, thinking levels, input, prices) from the built-in model of the
/// same id, else from the provider's default model.
fn merge_model_definitions(
    base: Vec<ModelDefinition>,
    cache: Vec<ModelDefinition>,
    explicit: Vec<ModelDefinition>,
) -> Vec<ModelDefinition> {
    let builtin = base.clone();
    let known = |model: &ModelDefinition| {
        builtin
            .iter()
            .find(|candidate| candidate.provider == model.provider && candidate.id == model.id)
    };
    let mut models = base;
    let mut upsert = |model: ModelDefinition| match models
        .iter_mut()
        .find(|existing| existing.provider == model.provider && existing.id == model.id)
    {
        Some(existing) => *existing = model,
        None => models.push(model),
    };
    for mut model in cache {
        model.provider = canonical_provider(&model.provider).to_string();
        if known(&model).is_some() {
            continue;
        }
        upsert(match custom_model(&builtin, &model.provider, &model.id) {
            Some(custom) => ModelDefinition {
                name: model.name.or(custom.name),
                context_window: model.context_window.or(custom.context_window),
                ..custom
            },
            None => model,
        });
    }
    for mut model in explicit {
        model.provider = canonical_provider(&model.provider).to_string();
        let same_id = known(&model).cloned();
        let name = model
            .name
            .clone()
            .or_else(|| same_id.as_ref().and_then(|builtin| builtin.name.clone()));
        let traits = same_id.or_else(|| custom_model(&builtin, &model.provider, &model.id));
        upsert(match traits {
            Some(traits) => ModelDefinition {
                name,
                context_window: model.context_window.or(traits.context_window),
                max_tokens: model.max_tokens.or(traits.max_tokens),
                reasoning: model.reasoning.or(traits.reasoning),
                thinking_level_map: model
                    .thinking_level_map
                    .clone()
                    .or(traits.thinking_level_map),
                input: model.input.clone().or(traits.input),
                cost: model.cost.clone().or(traits.cost),
                ..model
            },
            None => model,
        });
    }
    models
}

fn resolve_config_value(value: &str) -> String {
    if let Some(name) = value.strip_prefix("env:") {
        return std::env::var(name).unwrap_or_default();
    }
    value.to_string()
}

/// Each provider's standard base-URL variable, read as Claude Code and the
/// providers' SDKs read it. In a Codewire Sandbox it names the local
/// credential proxy, which replaces a placeholder credential with the real
/// one, so requests that ignored it would reach the provider without one.
const BASE_URL_ENV: [(&str, &str); 2] = [
    ("anthropic", "ANTHROPIC_BASE_URL"),
    ("openai", "OPENAI_BASE_URL"),
];

/// Points the provider's models at its base-URL variable when that is set:
/// every built-in model (`replace`), but only those `models.json` entries
/// that name no `baseUrl` of their own. Listed models derived from a
/// built-in one inherit the built-in's address.
fn with_base_url_env(
    mut models: Vec<ModelDefinition>,
    replace: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<ModelDefinition> {
    for (provider, name) in BASE_URL_ENV {
        let Some(url) = env(name) else {
            continue;
        };
        for model in models.iter_mut().filter(|model| {
            canonical_provider(&model.provider) == provider && (replace || model.base_url.is_none())
        }) {
            model.base_url = Some(url.trim().to_string());
        }
    }
    models
}

fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
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

    if !settings.contains_key("steeringMode")
        && let Some(queue_mode) = settings.remove("queueMode")
    {
        settings.insert("steeringMode".to_string(), queue_mode);
    }

    if !settings.contains_key("transport")
        && let Some(websockets) = settings
            .get("websockets")
            .and_then(serde_json::Value::as_bool)
    {
        settings.insert(
            "transport".to_string(),
            serde_json::Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
        );
        settings.remove("websockets");
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
        return read_text(path)
            .map(Some)
            .map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
    }
    Ok(Some(input.to_string()))
}

/// A prompt or context file's text, as upstream reads one: invalid UTF-8 is
/// replaced and a leading byte order mark is dropped.
fn read_text(path: &Path) -> std::io::Result<String> {
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    Ok(match text.strip_prefix('\u{feff}') {
        Some(text) => text.to_string(),
        None => text,
    })
}

/// `SYSTEM.md` or `APPEND_SYSTEM.md`: the trusted project's `.noel/<name>`
/// (see [`project_config_dir`]), else the agent dir's. As upstream,
/// same-named files are not combined.
fn discovered_prompt_file(
    paths: &ConfigPaths,
    name: &str,
    project_trusted: bool,
    diagnostics: &mut Vec<String>,
) -> Option<String> {
    let project = project_config_dir(&paths.cwd).join(name);
    let path = if project_trusted && project.exists() {
        project
    } else {
        paths.agent_dir.join(name)
    };
    if !path.exists() {
        return None;
    }
    match read_text(&path) {
        Ok(text) => Some(text),
        Err(error) => {
            diagnostics.push(format!("failed to read {}: {error}", path.display()));
            None
        }
    }
}

/// The built-in catalog (`noel_ai::catalog`): this port's own providers, then
/// upstream's.
fn default_models() -> Vec<ModelDefinition> {
    noel_ai::catalog::builtin_models()
        .iter()
        .map(ModelDefinition::from)
        .collect()
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
    env::home_dir().ok_or(ConfigError::HomeUnavailable)
}

#[cfg(test)]
mod tests;

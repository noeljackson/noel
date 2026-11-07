mod frontmatter;
mod prompt_templates;
mod resources;
mod skills;

pub use frontmatter::{js_trim, strip_frontmatter};
pub use prompt_templates::{expand_prompt_template, PromptTemplate};
pub use resources::{ResourceOptions, SourceInfo, SourceScope};
pub use skills::{format_skills_for_prompt, Skill, SkillReader};

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

mod model_resolver;

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
pub const ENV_AGENT_DIR: &str = "NOEL_DIR";
pub const ENV_SESSION_DIR: &str = "NOEL_SESSION_DIR";
/// Set by the test harness (`make test`, and every `noel` the CLI tests run).
/// noel then refuses to fall back to `~/.noel`, so a test must name its
/// agent dir with `NOEL_DIR`, imports no Claude Code or Codex
/// login from `$HOME` and no `~/.pi/agent`, reads no `~/.agents/skills`, and
/// starts no background model refresh.
pub const ENV_TEST_ISOLATION: &str = "NOEL_TEST_ISOLATION";
pub const PROJECT_TRUST_FILE_NAME: &str = "trust.json";

/// True under the test harness; see [`ENV_TEST_ISOLATION`].
pub fn test_isolation() -> bool {
    env::var_os(ENV_TEST_ISOLATION).is_some()
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
        let agent_dir = match env::var_os(ENV_AGENT_DIR) {
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
            None => match env::var_os(ENV_SESSION_DIR) {
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    /// The thinking level a model starts on, by `provider/id`; it ranks
    /// above `defaultThinkingLevel`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_thinking_levels: BTreeMap<String, String>,
    pub transport: Option<String>,
    pub steering_mode: Option<String>,
    pub follow_up_mode: Option<String>,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub append_system_prompt: Vec<String>,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub hide_thinking_block: Option<bool>,
    /// An allowlist of tool names and `*` patterns: only matching tools are
    /// available. `--tools` with plain names sets it for one run.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    /// The built-in tools enabled at startup; see [`resolve_default_tools`].
    #[serde(default)]
    pub default_tools: Option<Vec<String>>,
    /// Tool names and `*` patterns `--exclude-tools` removes for one run.
    /// Not a settings file key.
    #[serde(skip)]
    pub excluded_tools: Vec<String>,
    #[serde(default)]
    pub enabled_models: Option<Vec<String>>,
    pub theme: Option<String>,
    pub accent_color: Option<String>,
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
    pub agent: Option<AgentSettings>,
    #[serde(default)]
    pub progress: Option<ProgressSettings>,
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
    /// Crash restarts in a row after which a Mode 2 run ends
    /// `restart_limit` (Durable's `settings.restartLimit`, default 5).
    #[serde(default)]
    pub restart_limit: Option<u32>,
    /// How codemode presents tools (upstream `codemode`).
    #[serde(default)]
    pub codemode: Option<CodemodeSettings>,
    #[serde(default)]
    pub model_refresh: Option<ModelRefreshSettings>,
    /// Failover-only multi-account rotation. Disabled by default.
    #[serde(default)]
    pub failover: Option<FailoverSettings>,
    /// Mode 2's `noel server` and its workers.
    #[serde(default)]
    pub server: Option<ServerSettings>,
}

impl Settings {
    fn merge(self, overrides: Settings) -> Settings {
        Settings {
            default_provider: overrides.default_provider.or(self.default_provider),
            default_model: overrides.default_model.or(self.default_model),
            default_thinking_level: overrides
                .default_thinking_level
                .or(self.default_thinking_level),
            model_thinking_levels: {
                let mut levels = self.model_thinking_levels;
                levels.extend(overrides.model_thinking_levels);
                levels
            },
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
            default_tools: merge_default_tools(self.default_tools, overrides.default_tools),
            excluded_tools: if overrides.excluded_tools.is_empty() {
                self.excluded_tools
            } else {
                overrides.excluded_tools
            },
            enabled_models: overrides.enabled_models.or(self.enabled_models),
            theme: overrides.theme.or(self.theme),
            accent_color: overrides.accent_color.or(self.accent_color),
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
            agent: merge_optional_settings(self.agent, overrides.agent),
            progress: merge_optional_settings(self.progress, overrides.progress),
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
            codemode: merge_optional_settings(self.codemode, overrides.codemode),
            restart_limit: overrides.restart_limit.or(self.restart_limit),
            model_refresh: merge_optional_settings(self.model_refresh, overrides.model_refresh),
            failover: merge_optional_settings(self.failover, overrides.failover),
            server: merge_optional_settings(self.server, overrides.server),
        }
    }
}

impl Settings {
    /// Rewrites the Azure provider id this port used to use, wherever a
    /// saved setting names a provider (see [`canonical_provider`]).
    fn with_canonical_providers(mut self) -> Self {
        let legacy_prefix = format!("{LEGACY_AZURE_PROVIDER}/");
        let rename = |reference: String| match reference.strip_prefix(&legacy_prefix) {
            Some(id) => format!("{}/{id}", noel_ai::AZURE_PROVIDER),
            None => reference,
        };
        self.default_provider = self
            .default_provider
            .map(|provider| canonical_provider(&provider).to_string());
        self.enabled_models = self
            .enabled_models
            .map(|patterns| patterns.into_iter().map(rename).collect());
        self.model_thinking_levels = self
            .model_thinking_levels
            .into_iter()
            .map(|(model, level)| (rename(model), level))
            .collect();
        self
    }

    /// The level `provider/id` starts on, if `modelThinkingLevels` sets one.
    pub fn model_thinking_level(&self, provider: &str, id: &str) -> Option<&str> {
        self.model_thinking_levels
            .get(&format!("{provider}/{id}"))
            .map(String::as_str)
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

/// Whether a tool selection entry is a `+name` or `-name` modifier.
pub fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with(['+', '-'])
}

/// What is wrong with a `--tools` list, if anything. It is either an
/// allowlist of names and `*` patterns, or only `+name`/`-name` entries with
/// exact names.
pub fn tool_list_error(entries: &[String]) -> Option<String> {
    let modifiers = entries
        .iter()
        .filter(|entry| is_tool_modifier(entry))
        .collect::<Vec<_>>();
    if modifiers.is_empty() {
        return None;
    }
    if modifiers.len() < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_string());
    }
    modifiers
        .into_iter()
        .find(|entry| entry.contains('*'))
        .map(|pattern| {
            format!("+name and -name entries take exact tool names, not patterns: {pattern}")
        })
}

/// Applies the `+name` and `-name` entries to `base` in order: `+name` adds
/// a tool and `-name` removes one. Other entries are ignored.
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries.iter().filter(|entry| is_tool_modifier(entry)) {
        let name = &entry[1..];
        let position = tools.iter().position(|tool| tool == name);
        match (entry.starts_with('+'), position) {
            (true, None) if !name.is_empty() => tools.push(name.to_string()),
            (false, Some(position)) => {
                tools.remove(position);
            }
            _ => {}
        }
    }
    tools
}

/// The tools a merged `defaultTools` list selects: its plain names replace
/// `defaults`, then `+name` adds and `-name` removes a tool, in list order.
/// An empty list selects nothing.
pub fn resolve_default_tools(entries: &[String], defaults: &[String]) -> Vec<String> {
    let plain = entries
        .iter()
        .filter(|entry| !is_tool_modifier(entry))
        .cloned()
        .collect::<Vec<_>>();
    let base = if plain.is_empty() && !entries.is_empty() {
        defaults
    } else {
        &plain
    };
    apply_tool_modifiers(base, entries)
}

/// Whether a tool name is one of `entries`, each an exact name or a pattern
/// where `*` matches any characters.
pub fn tool_name_matches(entries: &[String], name: &str) -> bool {
    entries.iter().any(|entry| {
        let mut parts = entry.split('*');
        let Some(rest) = parts.next().and_then(|first| name.strip_prefix(first)) else {
            return false;
        };
        let Some(last) = parts.next_back() else {
            // No `*`: an exact name.
            return rest.is_empty();
        };
        let mut rest = rest;
        for part in parts {
            let Some(found) = rest.find(part) else {
                return false;
            };
            rest = &rest[found + part.len()..];
        }
        rest.ends_with(last)
    })
}

/// Merges `defaultTools` of two settings layers. A list with plain tool
/// names replaces the inherited one; a list of only `+name`/`-name` entries
/// is appended, so it modifies the inherited selection.
fn merge_default_tools(
    base: Option<Vec<String>>,
    overrides: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (base, overrides) {
        (base, None) => base,
        (Some(mut base), Some(overrides))
            if overrides.iter().all(|entry| is_tool_modifier(entry)) =>
        {
            base.extend(overrides);
            Some(base)
        }
        (_, overrides) => overrides,
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

/// Compaction settings: of the settings file, or of one conversation
/// (Durable's `CompactionSettings`, upstream 922c3ff2). `modelOverrides`
/// is keyed by exact `"provider/modelId"`; each field it sets replaces the
/// general one for that model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Percentage of the model context window that triggers automatic compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<BTreeMap<String, CompactionPolicyFields>>,
}

/// The fields of a compaction policy, each optional: what a settings
/// layer or a model override sets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPolicyFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_percent: Option<u8>,
}

impl CompactionSettings {
    /// This layer's general fields.
    pub fn general(&self) -> CompactionPolicyFields {
        CompactionPolicyFields {
            enabled: self.enabled,
            reserve_tokens: self.reserve_tokens,
            keep_recent_tokens: self.keep_recent_tokens,
            trigger_percent: self.trigger_percent,
        }
    }

    /// This layer's override for `model` (`"provider/modelId"`).
    pub fn for_model(&self, model: &str) -> CompactionPolicyFields {
        self.model_overrides
            .as_ref()
            .and_then(|overrides| overrides.get(model))
            .cloned()
            .unwrap_or_default()
    }
}

/// The compaction policy for a conversation and the model it acts for,
/// field by field from the first layer that sets it (Durable section 8.7):
/// the conversation's override for the model, the settings' override for
/// it, the conversation's own fields, then the settings'. A model-specific
/// value beats a general one, and the conversation beats the settings at
/// the same level. One exception: when the conversation's override leaves
/// `enabled` unset, the conversation's own `enabled: false` wins over the
/// settings' override.
pub fn resolve_compaction(
    settings: &CompactionSettings,
    conversation: Option<&CompactionSettings>,
    model: &str,
) -> CompactionPolicyFields {
    let none = CompactionSettings::default();
    let conversation = conversation.unwrap_or(&none);
    let layers = [
        conversation.for_model(model),
        settings.for_model(model),
        conversation.general(),
        settings.general(),
    ];
    let first =
        |pick: &dyn Fn(&CompactionPolicyFields) -> Option<u64>| layers.iter().find_map(pick);
    let enabled = match (layers[0].enabled, conversation.enabled) {
        (Some(enabled), _) => Some(enabled),
        (None, Some(false)) => Some(false),
        _ => layers.iter().find_map(|layer| layer.enabled),
    };
    CompactionPolicyFields {
        enabled,
        reserve_tokens: first(&|layer| layer.reserve_tokens),
        keep_recent_tokens: first(&|layer| layer.keep_recent_tokens),
        trigger_percent: layers.iter().find_map(|layer| layer.trigger_percent),
    }
}

impl MergeSettings for CompactionSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            enabled: overrides.enabled.or(self.enabled),
            reserve_tokens: overrides.reserve_tokens.or(self.reserve_tokens),
            keep_recent_tokens: overrides.keep_recent_tokens.or(self.keep_recent_tokens),
            trigger_percent: overrides.trigger_percent.or(self.trigger_percent),
            model_overrides: overrides.model_overrides.or(self.model_overrides),
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
pub struct AgentSettings {
    /// Safety cap on provider rounds per run, including steering. `None`
    /// keeps the compiled default.
    pub max_tool_rounds: Option<usize>,
}

impl MergeSettings for AgentSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            max_tool_rounds: overrides.max_tool_rounds.or(self.max_tool_rounds),
        }
    }
}

/// How long Mode 2's processes stay without work (`settings.server`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    /// `noel server` exits after this long with no client link and no
    /// worker. Default 600000 (10 minutes).
    pub idle_exit_ms: Option<u64>,
    /// A worker exits after this long with no holds, no run and nothing
    /// queued; one that lost its server waits this long to be adopted.
    /// Default 60000.
    pub worker_idle_exit_ms: Option<u64>,
    /// Where the server also listens for paired devices over TLS
    /// (`host:port`); unset, it does not (spec §16).
    pub listen: Option<String>,
}

impl MergeSettings for ServerSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            idle_exit_ms: overrides.idle_exit_ms.or(self.idle_exit_ms),
            worker_idle_exit_ms: overrides.worker_idle_exit_ms.or(self.worker_idle_exit_ms),
            listen: overrides.listen.or(self.listen),
        }
    }
}

/// How often a run saves what it has produced so far (upstream Durable's
/// `settings.progress`): a streaming reply's text and thinking, and a
/// running tool's output. A crash loses at most the last interval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSettings {
    /// Default 100.
    pub partial_interval_ms: Option<u64>,
    /// Default 100.
    pub output_interval_ms: Option<u64>,
}

impl MergeSettings for ProgressSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            partial_interval_ms: overrides.partial_interval_ms.or(self.partial_interval_ms),
            output_interval_ms: overrides.output_interval_ms.or(self.output_interval_ms),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    pub enabled: Option<bool>,
    pub max_retries: Option<u64>,
    pub base_delay_ms: Option<u64>,
    /// Cap on each agent-level retry backoff delay; upstream defaults to 60s.
    pub max_agent_delay_ms: Option<u64>,
    pub provider: Option<ProviderRetrySettings>,
}

impl MergeSettings for RetrySettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            enabled: overrides.enabled.or(self.enabled),
            max_retries: overrides.max_retries.or(self.max_retries),
            base_delay_ms: overrides.base_delay_ms.or(self.base_delay_ms),
            max_agent_delay_ms: overrides.max_agent_delay_ms.or(self.max_agent_delay_ms),
            // Nested settings merge recursively, like upstream deepMergeObjects.
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

/// Upstream's `codemode` settings: `mode` is `"on"` (the default: tools stay
/// declared and their descriptions say how scripts call them) or `"only"`
/// (codemode lists every script-callable tool and the others are not
/// declared); `inlineBudget` is the estimated tokens codemode's description
/// may spend on tool declarations (default 3000).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeSettings {
    pub mode: Option<String>,
    pub inline_budget: Option<serde_json::Number>,
}

impl MergeSettings for CodemodeSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            mode: overrides.mode.or(self.mode),
            inline_budget: overrides.inline_budget.or(self.inline_budget),
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

/// Failover-only multi-account rotation settings.
///
/// One sticky account carries the session; other stored accounts are used
/// only when the sticky one is rate limited (429) or its 5h usage
/// window is at the exhaustion threshold. This is availability failover,
/// not load balancing: requests are never spread across accounts to
/// maximize throughput.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverSettings {
    pub enabled: Option<bool>,
    /// Providers that participate in failover. An empty or missing map
    /// means every provider with two or more stored accounts.
    #[serde(default)]
    pub providers: BTreeMap<String, FailoverProviderSettings>,
}

impl MergeSettings for FailoverSettings {
    fn merge(self, overrides: Self) -> Self {
        Self {
            enabled: overrides.enabled.or(self.enabled),
            // Per-provider maps replace rather than deep-merge, matching the
            // extensions/skills resource-list semantics.
            providers: if overrides.providers.is_empty() {
                self.providers
            } else {
                overrides.providers
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverProviderSettings {
    /// Usage percentage at which an account is skipped until its 5h window
    /// resets. Default 90.
    pub exhaustion_threshold_pct: Option<u64>,
    /// Minimum milliseconds between voluntary account switches. Default
    /// 10000. Switches away from a rate-limited account never wait.
    pub min_switch_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthCredential {
    ApiKey {
        key: String,
    },
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth {
        #[serde(alias = "access")]
        access_token: String,
        #[serde(default, alias = "refresh")]
        refresh_token: Option<String>,
        expires: u64,
        #[serde(default, alias = "accountId")]
        account_id: Option<String>,
    },
}

pub const DEFAULT_ACCOUNT_NAME: &str = "default";

/// Whether `provider`'s requests can carry `credential`. Upstream keeps
/// credentials per provider, and each provider names what it takes:
/// `openai-codex` a ChatGPT login and no API key, `openai` an API key. A
/// stored credential of the other kind is no credential, so nothing offers
/// the provider's models on the strength of it.
///
/// Upstream's `openai` also has a ChatGPT sign-in of its own, whose tokens
/// its Responses API takes. noel has no such flow: a login stored under
/// `openai` is the Codex login `noel login` used to save there as well, which
/// that API does not take.
fn provider_takes(provider: &str, credential: &AuthCredential) -> bool {
    !matches!(
        (provider, credential),
        ("openai", AuthCredential::OAuth { .. }) | ("openai-codex", AuthCredential::ApiKey { .. })
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthData {
    pub providers: BTreeMap<String, BTreeMap<String, AuthCredential>>,
}

impl AuthData {
    /// The key `provider`'s accounts are stored under: its id, or for Azure
    /// the id this port used to use, when the file still holds that one
    /// (see [`canonical_provider`]). Either id names the same accounts.
    fn stored_key<'a>(&self, provider: &'a str) -> &'a str {
        let provider = canonical_provider(provider);
        if provider == noel_ai::AZURE_PROVIDER
            && !self.providers.contains_key(provider)
            && self.providers.contains_key(LEGACY_AZURE_PROVIDER)
        {
            return LEGACY_AZURE_PROVIDER;
        }
        provider
    }

    /// `provider`'s stored accounts, less those whose credential it does
    /// not take (see [`provider_takes`]).
    fn accounts<'a, 'p>(
        &'a self,
        provider: &'p str,
    ) -> impl Iterator<Item = (&'a str, &'a AuthCredential)> + 'p
    where
        'a: 'p,
    {
        self.providers
            .get(self.stored_key(provider))
            .into_iter()
            .flatten()
            .filter(move |(_, credential)| provider_takes(provider, credential))
            .map(|(account, credential)| (account.as_str(), credential))
    }

    pub fn accounts_for_provider(&self, provider: &str) -> Vec<String> {
        self.accounts(provider)
            .map(|(account, _)| account.to_string())
            .collect()
    }

    pub fn credential(&self, provider: &str, account: &str) -> Option<&AuthCredential> {
        self.accounts(provider)
            .find(|(name, _)| *name == account)
            .map(|(_, credential)| credential)
    }

    pub fn insert(&mut self, provider: &str, account: &str, credential: AuthCredential) {
        self.providers
            .entry(self.stored_key(provider).to_string())
            .or_default()
            .insert(account.to_string(), credential);
    }

    pub fn remove(&mut self, provider: &str, account: &str) -> Option<AuthCredential> {
        let key = self.stored_key(provider);
        let accounts = self.providers.get_mut(key)?;
        let removed = accounts.remove(account);
        if accounts.is_empty() {
            self.providers.remove(key);
        }
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn account_for_resolution(&self, provider: &str) -> Option<&str> {
        let accounts = self
            .accounts(provider)
            .map(|(account, _)| account)
            .collect::<Vec<_>>();
        if accounts.contains(&DEFAULT_ACCOUNT_NAME) {
            return Some(DEFAULT_ACCOUNT_NAME);
        }
        match accounts[..] {
            [only] => Some(only),
            _ => None,
        }
    }

    fn default_credential(&self, provider: &str) -> Option<&AuthCredential> {
        self.account_for_resolution(provider)
            .and_then(|account| self.credential(provider, account))
    }
}

impl From<BTreeMap<String, AuthCredential>> for AuthData {
    fn from(flat: BTreeMap<String, AuthCredential>) -> Self {
        flat.into_iter()
            .map(|(provider, credential)| {
                (
                    provider,
                    BTreeMap::from([(DEFAULT_ACCOUNT_NAME.to_string(), credential)]),
                )
            })
            .collect::<BTreeMap<_, _>>()
            .into()
    }
}

impl From<BTreeMap<String, BTreeMap<String, AuthCredential>>> for AuthData {
    fn from(providers: BTreeMap<String, BTreeMap<String, AuthCredential>>) -> Self {
        Self { providers }
    }
}

impl Serialize for AuthData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.providers.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AuthData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum AuthDataFile {
            V2(BTreeMap<String, BTreeMap<String, AuthCredential>>),
            V1(BTreeMap<String, AuthCredential>),
        }
        match AuthDataFile::deserialize(deserializer)? {
            AuthDataFile::V2(providers) => Ok(providers.into()),
            AuthDataFile::V1(flat) => Ok(flat.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAuth {
    ApiKey(String),
    ClaudeCodeOAuth {
        access_token: String,
        refresh_token: Option<String>,
        expires: u64,
    },
    ChatGptOAuth {
        access_token: String,
        refresh_token: Option<String>,
        expires: u64,
        account_id: Option<String>,
    },
}

impl ResolvedAuth {
    pub fn refresh_token(&self) -> Option<&str> {
        match self {
            ResolvedAuth::ClaudeCodeOAuth { refresh_token, .. }
            | ResolvedAuth::ChatGptOAuth { refresh_token, .. } => refresh_token.as_deref(),
            ResolvedAuth::ApiKey(_) => None,
        }
    }

    pub fn expires(&self) -> u64 {
        match self {
            ResolvedAuth::ClaudeCodeOAuth { expires, .. }
            | ResolvedAuth::ChatGptOAuth { expires, .. } => *expires,
            ResolvedAuth::ApiKey(_) => 0,
        }
    }
}

pub fn is_expired(expires: u64, now: u64) -> bool {
    expires != 0 && now >= expires
}

pub fn expires_within(expires: u64, now: u64, within_seconds: u64) -> bool {
    expires != 0 && now.saturating_add(within_seconds) >= expires
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
        prompt_templates,
        themes,
        diagnostics,
        system_prompt,
        append_system_prompt,
    })
}

pub fn api_key_for_provider(
    auth: &AuthData,
    provider: &str,
    account: Option<&str>,
) -> Option<String> {
    match auth_for_provider(auth, provider, account) {
        Some(ResolvedAuth::ApiKey(api_key)) => Some(api_key),
        _ => None,
    }
}

pub fn auth_for_provider(
    auth: &AuthData,
    provider: &str,
    account: Option<&str>,
) -> Option<ResolvedAuth> {
    let credential = match account {
        Some(account) => auth.credential(provider, account),
        None => auth.default_credential(provider),
    };
    if let Some(credential) = credential {
        return match credential {
            AuthCredential::ApiKey { key } => Some(ResolvedAuth::ApiKey(resolve_config_value(key))),
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                account_id,
            } => Some(oauth_for_provider(
                provider,
                resolve_config_value(access_token),
                refresh_token.as_deref().map(resolve_config_value),
                *expires,
                account_id.clone(),
            )),
        };
    }
    env_auth(provider).or_else(|| login_auth(provider))
}

/// Whether `provider`'s models can be offered: it has a credential, and, for
/// the Cloudflare providers, the ids their URLs are built from. Upstream
/// counts neither on the key alone (`cloudflare-auth.ts`), and one key
/// serves both.
pub fn has_auth_for_provider(auth: &AuthData, provider: &str, account: Option<&str>) -> bool {
    let ids: &[&str] = match provider {
        "cloudflare-workers-ai" => &["CLOUDFLARE_ACCOUNT_ID"],
        "cloudflare-ai-gateway" => &["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"],
        _ => &[],
    };
    ids.iter().all(|name| read_non_empty_env(name).is_some())
        && auth_for_provider(auth, provider, account).is_some()
}

pub fn import_account_name(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-import",
        _ => "codex-import",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSource {
    Stored,
    Environment,
    ImportedFile,
}

pub fn listed_accounts_for_provider(
    auth: &AuthData,
    provider: &str,
) -> Vec<(String, AccountSource)> {
    listed_accounts(
        auth,
        provider,
        env_auth(provider).is_some(),
        login_auth(provider).is_some(),
    )
}

fn listed_accounts(
    auth: &AuthData,
    provider: &str,
    env_present: bool,
    import_present: bool,
) -> Vec<(String, AccountSource)> {
    let mut accounts: Vec<(String, AccountSource)> = auth
        .accounts_for_provider(provider)
        .into_iter()
        .map(|account| (account, AccountSource::Stored))
        .collect();
    if env_present && !accounts.iter().any(|(account, _)| account == "env") {
        accounts.push(("env".to_string(), AccountSource::Environment));
    }
    if import_present {
        let import = import_account_name(provider);
        if !accounts.iter().any(|(account, _)| account == import) {
            accounts.push((import.to_string(), AccountSource::ImportedFile));
        }
    }
    accounts
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

/// How long an `auth.json` update waits for another noel process to finish its
/// own (a login, logout, or OAuth refresh).
pub const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Cross-process advisory lock serializing every read-modify-write of
/// `auth.json`. The file itself is replaced by rename, so the lock lives in a
/// sibling `auth.json.flock` that is never replaced. Dropping the lock
/// releases it, and so does the process exiting.
#[derive(Debug)]
pub struct AuthFileLock {
    _file: fs::File,
}

/// `auth_path` with its links resolved: the file's own path when it
/// exists, else its directory's real path joined with its name.
fn real_auth_path(auth_path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(auth_path) {
        return real;
    }
    match (auth_path.parent(), auth_path.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| auth_path.to_path_buf()),
        _ => auth_path.to_path_buf(),
    }
}

impl AuthFileLock {
    /// Takes the lock unless another holder has it.
    pub fn try_acquire(auth_path: &Path) -> Result<Option<Self>, ConfigError> {
        // Every path to the same file shares one lock: a symlinked
        // `auth.json` locks its target (upstream 081a6e87).
        let real = real_auth_path(auth_path);
        let auth_path = real.as_path();
        let file_name = auth_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "auth.json".to_string());
        let lock_path = auth_path.with_file_name(format!("{file_name}.flock"));
        if let Some(parent) = lock_path.parent() {
            create_private_dir_all(parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock_error = |source| ConfigError::Write {
            path: lock_path.clone(),
            source,
        };
        let file = options.open(&lock_path).map_err(lock_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(source)) => Err(lock_error(source)),
        }
    }

    /// Waits up to [`AUTH_LOCK_TIMEOUT`] for the lock.
    pub fn acquire(auth_path: &Path) -> Result<Self, ConfigError> {
        let deadline = Instant::now() + AUTH_LOCK_TIMEOUT;
        loop {
            if let Some(lock) = Self::try_acquire(auth_path)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(ConfigError::Write {
                    path: auth_path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "another noel process holds the auth.json lock",
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Reads `auth.json` as it is on disk now; a missing file holds no
/// credentials.
pub fn read_auth_file(auth_path: &Path) -> Result<AuthData, ConfigError> {
    Ok(read_optional_json::<AuthData>(auth_path)?.unwrap_or_default())
}

/// Writes `auth.json` atomically (temporary file, then rename) and owner-only
/// (0600), creating its directory owner-only (0700) when missing. Callers
/// hold [`AuthFileLock`].
pub fn write_auth_file(auth_path: &Path, auth: &AuthData) -> Result<(), ConfigError> {
    if let Some(parent) = auth_path.parent() {
        create_private_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(auth).map_err(|source| ConfigError::Parse {
        path: auth_path.to_path_buf(),
        source,
    })?;
    write_file_atomic_with_mode(auth_path, format!("{content}\n").as_bytes(), Some(0o600))
}

/// Applies `mutate` to `auth.json` as it is now, under [`AuthFileLock`], and
/// writes the result back only when it changed. Logins and logouts go through
/// here and OAuth refreshes hold the same lock, so a change only touches its
/// own entry and never restores another process's stale copy.
pub fn update_auth_file<T>(
    auth_path: &Path,
    mutate: impl FnOnce(&mut AuthData) -> T,
) -> Result<T, ConfigError> {
    let _lock = AuthFileLock::acquire(auth_path)?;
    let mut auth = read_auth_file(auth_path)?;
    let before = auth.clone();
    let result = mutate(&mut auth);
    if auth != before {
        write_auth_file(auth_path, &auth)?;
    }
    Ok(result)
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

fn env_api_key(provider: &str) -> Option<String> {
    let names = match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"][..],
        "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"][..],
        "ant-ling" => &["ANT_LING_API_KEY"][..],
        "azure" | LEGACY_AZURE_PROVIDER => &["AZURE_OPENAI_API_KEY"][..],
        "baseten" => &["BASETEN_API_KEY"][..],
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => &["CLOUDFLARE_API_KEY"][..],
        "cerebras" => &["CEREBRAS_API_KEY"][..],
        "deepseek" => &["DEEPSEEK_API_KEY"][..],
        "fireworks" => &["FIREWORKS_API_KEY"][..],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"][..],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"][..],
        "groq" => &["GROQ_API_KEY"][..],
        "huggingface" => &["HF_TOKEN"][..],
        "kimi-coding" | "kimi-coding-openai" => &["KIMI_API_KEY"][..],
        "meta" => &["META_API_KEY"][..],
        "minimax" => &["MINIMAX_API_KEY"][..],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"][..],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"][..],
        "nvidia" => &["NVIDIA_API_KEY"][..],
        "openai" => &["OPENAI_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"][..],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "qwen-token-plan" | "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"][..],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"][..],
        "together" => &["TOGETHER_API_KEY"][..],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"][..],
        "xai" => &["XAI_API_KEY"][..],
        "xiaomi" => &["XIAOMI_API_KEY"][..],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"][..],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"][..],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"][..],
        "zai" | "zai-coding" | "zai-anthropic" => &["ZAI_API_KEY"][..],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"][..],
        _ => &[],
    };
    names.iter().find_map(|name| env::var(name).ok())
}

fn env_auth(provider: &str) -> Option<ResolvedAuth> {
    match provider {
        "anthropic" => read_non_empty_env("ANTHROPIC_OAUTH_TOKEN")
            .or_else(|| read_non_empty_env("ANTHROPIC_AUTH_TOKEN"))
            .or_else(|| read_non_empty_env("CLAUDE_CODE_OAUTH_TOKEN"))
            .map(|access_token| ResolvedAuth::ClaudeCodeOAuth {
                access_token,
                refresh_token: None,
                expires: 0,
            })
            .or_else(|| env_api_key(provider).map(ResolvedAuth::ApiKey)),
        // An API key, under OpenAI's name for it or the Codex CLI's. A
        // ChatGPT login is `openai-codex`'s credential (see `provider_takes`).
        "openai" => env_api_key(provider)
            .or_else(|| read_non_empty_env("CODEX_API_KEY"))
            .map(ResolvedAuth::ApiKey),
        "openai-codex" => read_non_empty_env("CODEX_ACCESS_TOKEN").map(|access_token| {
            ResolvedAuth::ChatGptOAuth {
                access_token,
                refresh_token: None,
                expires: 0,
                account_id: read_non_empty_env("CHATGPT_ACCOUNT_ID"),
            }
        }),
        _ => env_api_key(provider).map(ResolvedAuth::ApiKey),
    }
}

fn login_auth(provider: &str) -> Option<ResolvedAuth> {
    // Tests never read the developer's Claude Code or Codex login files.
    if test_isolation() {
        return None;
    }
    match provider {
        "anthropic" => read_claude_code_oauth(),
        "openai-codex" => read_codex_chatgpt_oauth(),
        _ => None,
    }
}

fn oauth_for_provider(
    provider: &str,
    access_token: String,
    refresh_token: Option<String>,
    expires: u64,
    account_id: Option<String>,
) -> ResolvedAuth {
    match provider {
        "anthropic" => ResolvedAuth::ClaudeCodeOAuth {
            access_token,
            refresh_token,
            expires,
        },
        "openai-codex" => ResolvedAuth::ChatGptOAuth {
            access_token,
            refresh_token,
            expires,
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
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
}

fn read_claude_code_oauth() -> Option<ResolvedAuth> {
    let path = home_dir().ok()?.join(".claude").join(".credentials.json");
    read_claude_code_oauth_from_path(&path)
}

fn read_claude_code_oauth_from_path(path: &Path) -> Option<ResolvedAuth> {
    let credentials = read_optional_json::<ClaudeCredentialsFile>(path)
        .ok()
        .flatten()?;
    let oauth = credentials.claude_ai_oauth?;
    if oauth.access_token.trim().is_empty() {
        return None;
    }
    Some(ResolvedAuth::ClaudeCodeOAuth {
        access_token: oauth.access_token,
        refresh_token: non_empty(oauth.refresh_token),
        expires: oauth.expires_at.map(|millis| millis / 1000).unwrap_or(0),
    })
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexAuthTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
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
    let expires = jwt_exp_claim(&tokens.access_token).unwrap_or(0);
    Some(ResolvedAuth::ChatGptOAuth {
        access_token: tokens.access_token,
        refresh_token: non_empty(tokens.refresh_token),
        expires,
        account_id: tokens.account_id,
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

pub fn jwt_exp_claim(token: &str) -> Option<u64> {
    let parts = token.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload = serde_json::from_slice::<serde_json::Value>(&payload).ok()?;
    payload.get("exp").and_then(serde_json::Value::as_u64)
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

/// Context file names, first match wins: one file per directory.
/// `AGENTS.override.md` replaces only its own directory's file.
pub const CONTEXT_FILE_NAMES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// Context files from the agent dir, then from the outermost ancestor of
/// `cwd` down to `cwd`. Like upstream, these load whatever the project trust
/// decision: trust gates only `.noel` resources and `.agents/skills`.
fn load_context_files(
    cwd: &Path,
    agent_dir: &Path,
    diagnostics: &mut Vec<String>,
) -> Vec<ContextFile> {
    let global = load_context_file_from_dir(agent_dir, diagnostics);
    let shadowed = shadowed_context_file(cwd);
    let mut ancestors = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if let Some(file) = load_context_file_from_dir(dir, diagnostics) {
            // Compare the directory's real path, not the file's: a worktree
            // file that links to the main checkout's would resolve to the
            // shadowed file itself.
            let is_shadowed = file.path.file_name().is_some_and(|name| {
                shadowed.as_deref() == Some(canonical_path(dir).join(name).as_path())
            });
            // The agent dir can also be an ancestor of `cwd`.
            let is_global = global
                .as_ref()
                .is_some_and(|global| global.path == file.path);
            if !is_shadowed && !is_global {
                ancestors.push(file);
            }
        }
        current = dir.parent();
    }
    ancestors.reverse();
    global.into_iter().chain(ancestors).collect()
}

fn load_context_file_from_dir(dir: &Path, diagnostics: &mut Vec<String>) -> Option<ContextFile> {
    for filename in CONTEXT_FILE_NAMES {
        let path = dir.join(filename);
        if !path.is_file() {
            continue;
        }
        match read_text(&path) {
            Ok(content) => return Some(ContextFile { path, content }),
            Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
        }
    }
    None
}

/// The main checkout's context file that a linked worktree nested inside that
/// checkout replaces with its own: both describe the same repository, so
/// loading both would apply its instructions twice. `None` for an ordinary
/// repository, a worktree beside its main checkout, a bare layout, and a
/// submodule, where ancestors load as usual.
fn shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let (worktree_root, common_git_dir) = git_paths(cwd)?;
    let worktree_root = canonical_path(&worktree_root);
    let common_git_dir = canonical_path(&common_git_dir);
    let main_root = common_git_dir.parent()?;
    if worktree_root == main_root || !worktree_root.starts_with(main_root) {
        return None;
    }
    // The common git dir's parent is the main checkout only when that dir is
    // its `.git`: not `proj/.bare`, and not a submodule's `.git/modules/x`.
    if canonical_path(&main_root.join(".git")) != common_git_dir {
        return None;
    }
    let file = load_context_file_from_dir(&worktree_root, &mut Vec::new())?;
    Some(main_root.join(file.path.file_name()?))
}

/// The work tree root and common git dir of the repository holding `cwd`:
/// `.git` is a directory, or a linked worktree's `gitdir:` file.
fn git_paths(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let git = dir.join(".git");
        if git.is_dir() {
            return git.join("HEAD").exists().then(|| (dir.to_path_buf(), git));
        }
        let pointer = fs::read_to_string(&git).unwrap_or_default();
        if let Some(target) = pointer.trim().strip_prefix("gitdir: ") {
            let git_dir = dir.join(target.trim());
            if !git_dir.join("HEAD").exists() {
                return None;
            }
            let common_git_dir = match fs::read_to_string(git_dir.join("commondir")) {
                Ok(common) => git_dir.join(common.trim()),
                Err(_) => git_dir,
            };
            return Some((dir.to_path_buf(), common_git_dir));
        }
        current = dir.parent();
    }
    None
}

/// `path` with symlinks resolved, or as given when it cannot be resolved.
fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn load_resource_files(
    paths: &ConfigPaths,
    resource_dir_name: &str,
    project_trusted: bool,
    diagnostics: &mut Vec<String>,
) -> Vec<ResourceFile> {
    let mut dirs = vec![paths.agent_dir.join(resource_dir_name)];
    if project_trusted {
        dirs.push(project_config_dir(&paths.cwd).join(resource_dir_name));
    }
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

/// The extensions and themes of the packages. Their skills and prompt
/// templates load with the others, in `resources`.
fn load_package_resources(
    cwd: &Path,
    packages: &[PackageSource],
    extensions: &mut Vec<ResourceFile>,
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
        load_package_root_resources(&package_root, package, extensions, themes, diagnostics);
    }
}

fn load_package_root_resources(
    package_root: &Path,
    package: &PackageSource,
    extensions: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) {
    let manifest = read_package_manifest(package_root, diagnostics);
    let extension_entries = package_resource_entries(
        manifest.as_ref().map(|manifest| &manifest.extensions),
        "extensions",
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
        Ok(Some(package_json)) => package_json.noel.or(package_json.pi),
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
    noel: Option<PackageManifest>,
    /// The key of packages written for pi, read when `noel` is absent.
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

/// Whether `disabledResources` in settings names this resource: by name,
/// as `skills/<name>` or `skill:<name>`, or by path. Wildcards apply.
fn resource_disabled(
    disabled_resources: Option<&ResourceStateSettings>,
    resource_type: PackageResourceType,
    name: &str,
    path: &Path,
) -> bool {
    let Some(disabled_resources) = disabled_resources else {
        return false;
    };
    let patterns = match resource_type {
        PackageResourceType::Extensions => &disabled_resources.extensions,
        PackageResourceType::Skills => &disabled_resources.skills,
        PackageResourceType::Prompts => &disabled_resources.prompts,
        PackageResourceType::Themes => &disabled_resources.themes,
    };
    let path = normalize_resource_path(path);
    let key = resource_type.key();
    let singular = resource_type.singular();
    patterns.iter().any(|pattern| {
        let pattern = normalize_pattern(pattern);
        wildcard_matches(&pattern, name)
            || wildcard_matches(&pattern, &format!("{key}/{name}"))
            || wildcard_matches(&pattern, &format!("{singular}:{name}"))
            || wildcard_matches(&pattern, &path)
    })
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
        .map(|name| {
            name.ends_with(".noel-extension.json")
                || name.ends_with(".pi-extension.json")
                || RESOURCE_IGNORE_FILES.contains(&name)
        })
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
    path.file_stem()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
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
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(ConfigError::HomeUnavailable)
}

#[cfg(test)]
mod tests;

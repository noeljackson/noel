//! Settings: the settings files merged global then project, and the tool list modifiers.

use super::*;

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
    pub(crate) fn merge(self, overrides: Settings) -> Settings {
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
    pub(crate) fn with_canonical_providers(mut self) -> Self {
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

pub(crate) fn merge_resource_state_patterns(
    base: Vec<String>,
    overrides: Vec<String>,
) -> Vec<String> {
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

    pub(crate) fn filters(&self, resource_type: PackageResourceType) -> Option<&[String]> {
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
pub(crate) fn merge_default_tools(
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

pub(crate) trait MergeSettings {
    fn merge(self, overrides: Self) -> Self;
}

pub(crate) fn merge_optional_settings<T: MergeSettings>(
    base: Option<T>,
    overrides: Option<T>,
) -> Option<T> {
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

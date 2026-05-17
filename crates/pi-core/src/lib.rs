use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pi_ai::{
    ChatMessage, ChatRole, MediaInput, ModelRef, Provider, ProviderError, ProviderRequest,
    StreamEvent,
};
use pi_config::{has_auth_for_provider, LoadedConfig};
use pi_tools::{
    builtin_tool_definitions, execute_tool, ToolError, ToolRequest, ToolRuntimeOptions,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default)]
    pub media: Vec<MediaInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    pub id: String,
    pub name: String,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub labels: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub tool_history: Vec<ToolEvent>,
    pub queued_messages: Vec<String>,
    #[serde(default)]
    pub compactions: Vec<CompactionRecord>,
    #[serde(default)]
    pub branch_summaries: Vec<BranchSummary>,
    pub active_model: Option<ModelRef>,
    #[serde(default)]
    pub active_thinking_level: Option<String>,
    pub active_tool_names: BTreeSet<String>,
}

impl SessionState {
    pub fn new(session_id: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            session_id: session_id.into(),
            cwd,
            name: None,
            labels: BTreeSet::new(),
            parent_session_id: None,
            messages: Vec::new(),
            tool_history: Vec::new(),
            queued_messages: Vec::new(),
            compactions: Vec::new(),
            branch_summaries: Vec::new(),
            active_model: None,
            active_thinking_level: None,
            active_tool_names: builtin_tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub labels: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub branch_summary: Option<String>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExport {
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub labels: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub tool_history: Vec<ToolEvent>,
    pub queued_messages: Vec<String>,
    #[serde(default)]
    pub compactions: Vec<CompactionRecord>,
    #[serde(default)]
    pub branch_summaries: Vec<BranchSummary>,
    pub active_model: Option<ModelRef>,
    #[serde(default)]
    pub active_thinking_level: Option<String>,
    pub active_tool_names: BTreeSet<String>,
}

impl From<&SessionState> for SessionExport {
    fn from(state: &SessionState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
            name: state.name.clone(),
            labels: state.labels.clone(),
            parent_session_id: state.parent_session_id.clone(),
            messages: state.messages.clone(),
            tool_history: state.tool_history.clone(),
            queued_messages: state.queued_messages.clone(),
            compactions: state.compactions.clone(),
            branch_summaries: state.branch_summaries.clone(),
            active_model: state.active_model.clone(),
            active_thinking_level: state.active_thinking_level.clone(),
            active_tool_names: state.active_tool_names.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub kind: CompactionKind,
    pub omitted_messages: usize,
    pub retained_messages: usize,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    Manual,
    Automatic,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSummary {
    pub from_session_id: String,
    pub to_session_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadableSystems {
    pub config_generation: u64,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub context_messages: Vec<String>,
    pub available_models: Vec<ModelRef>,
    pub configured_providers: BTreeSet<String>,
    pub available_tool_names: BTreeSet<String>,
    pub keybinding_generation: u64,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub retry: RuntimeRetrySettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRetrySettings {
    pub enabled: bool,
    pub max_retries: u64,
    pub base_delay_ms: u64,
}

impl Default for RuntimeRetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}

impl ReloadableSystems {
    pub fn from_config(config: &LoadedConfig, generation: u64) -> Self {
        let available_models = config
            .models
            .iter()
            .map(|model| ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            })
            .collect();
        let configured_providers = config
            .models
            .iter()
            .filter(|model| {
                model.provider == "faux" || has_auth_for_provider(&config.auth, &model.provider)
            })
            .map(|model| model.provider.clone())
            .collect();
        let available_tool_names = match &config.settings.enabled_tools {
            Some(enabled_tools) => enabled_tools.iter().cloned().collect(),
            None => builtin_tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect(),
        };
        let context_messages = config
            .context_files
            .iter()
            .map(|file| format!("{}:\n{}", file.path.display(), file.content))
            .collect();

        Self {
            config_generation: generation,
            system_prompt: config.system_prompt.clone(),
            append_system_prompt: config.append_system_prompt.clone(),
            context_messages,
            available_models,
            configured_providers,
            available_tool_names,
            keybinding_generation: generation,
            shell_path: config.settings.shell_path.clone(),
            shell_command_prefix: config.settings.shell_command_prefix.clone(),
            retry: RuntimeRetrySettings {
                enabled: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.enabled)
                    .unwrap_or(true),
                max_retries: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.max_retries)
                    .unwrap_or(3),
                base_delay_ms: config
                    .settings
                    .retry
                    .as_ref()
                    .and_then(|retry| retry.base_delay_ms)
                    .unwrap_or(2_000),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    pub active_model_valid: bool,
    pub removed_active_tools: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReloadError {
    #[error("reloadable systems are invalid: {0}")]
    Invalid(String),
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("failed to create session directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open session {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read session {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write session {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse session line in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("unknown slash command: {0}")]
    UnknownCommand(String),
    #[error("missing argument for slash command: {0}")]
    MissingCommandArgument(String),
    #[error("tool is disabled: {0}")]
    DisabledTool(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum SessionRecord {
    Started {
        session_id: String,
        cwd: PathBuf,
    },
    Metadata {
        name: Option<String>,
        labels: Vec<String>,
        parent_session_id: Option<String>,
    },
    Message {
        message: ConversationMessage,
    },
    MessagesSnapshot {
        messages: Vec<ConversationMessage>,
    },
    Tool {
        event: ToolEvent,
    },
    ActiveModel {
        model: Option<ModelRef>,
    },
    ActiveThinkingLevel {
        level: Option<String>,
    },
    ActiveTools {
        tools: Vec<String>,
    },
    QueuedMessage {
        message: String,
    },
    QueuedMessagesSnapshot {
        messages: Vec<String>,
    },
    Compaction {
        record: CompactionRecord,
    },
    BranchSummary {
        summary: BranchSummary,
    },
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn create(session_dir: &Path, cwd: PathBuf) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = new_session_id();
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self { path };
        let state = SessionState::new(session_id.clone(), cwd.clone());
        store.append(&SessionRecord::Started { session_id, cwd })?;
        store.append(&SessionRecord::ActiveTools {
            tools: state.active_tool_names.iter().cloned().collect(),
        })?;
        Ok((store, state))
    }

    pub fn fork(
        session_dir: &Path,
        source: &SessionState,
        clone_parent: bool,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = new_session_id();
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self { path };
        let mut state = source.clone();
        state.session_id = session_id.clone();
        state.parent_session_id = if clone_parent {
            source.parent_session_id.clone()
        } else {
            Some(source.session_id.clone())
        };
        state.branch_summaries.push(BranchSummary {
            from_session_id: source.session_id.clone(),
            to_session_id: session_id,
            summary: summarize_branch(source),
        });
        store.write_full_state(&state)?;
        Ok((store, state))
    }

    pub fn import(
        session_dir: &Path,
        export: SessionExport,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = if export.session_id.trim().is_empty() {
            new_session_id()
        } else {
            export.session_id
        };
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self { path };
        let state = SessionState {
            session_id,
            cwd: export.cwd,
            name: export.name,
            labels: export.labels,
            parent_session_id: export.parent_session_id,
            messages: export.messages,
            tool_history: export.tool_history,
            queued_messages: export.queued_messages,
            compactions: export.compactions,
            branch_summaries: export.branch_summaries,
            active_model: export.active_model,
            active_thinking_level: export.active_thinking_level,
            active_tool_names: export.active_tool_names,
        };
        store.write_full_state(&state)?;
        Ok((store, state))
    }

    pub fn open(path: PathBuf) -> Result<(Self, SessionState), SessionError> {
        let store = Self { path };
        let state = store.load()?;
        Ok((store, state))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_message(&self, message: ConversationMessage) -> Result<(), SessionError> {
        self.append(&SessionRecord::Message { message })
    }

    pub fn record_messages_snapshot(
        &self,
        messages: Vec<ConversationMessage>,
    ) -> Result<(), SessionError> {
        self.append(&SessionRecord::MessagesSnapshot { messages })
    }

    pub fn record_tool(&self, event: ToolEvent) -> Result<(), SessionError> {
        self.append(&SessionRecord::Tool { event })
    }

    pub fn record_active_model(&self, model: Option<ModelRef>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveModel { model })
    }

    pub fn record_active_thinking_level(&self, level: Option<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveThinkingLevel { level })
    }

    pub fn record_active_tools(&self, tools: Vec<String>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveTools { tools })
    }

    pub fn record_metadata(&self, state: &SessionState) -> Result<(), SessionError> {
        self.append(&SessionRecord::Metadata {
            name: state.name.clone(),
            labels: state.labels.iter().cloned().collect(),
            parent_session_id: state.parent_session_id.clone(),
        })
    }

    pub fn record_queued_message(&self, message: String) -> Result<(), SessionError> {
        self.append(&SessionRecord::QueuedMessage { message })
    }

    pub fn record_queued_messages_snapshot(
        &self,
        messages: Vec<String>,
    ) -> Result<(), SessionError> {
        self.append(&SessionRecord::QueuedMessagesSnapshot { messages })
    }

    pub fn record_compaction(&self, record: CompactionRecord) -> Result<(), SessionError> {
        self.append(&SessionRecord::Compaction { record })
    }

    pub fn record_branch_summary(&self, summary: BranchSummary) -> Result<(), SessionError> {
        self.append(&SessionRecord::BranchSummary { summary })
    }

    pub fn export_state(&self, state: &SessionState, path: &Path) -> Result<(), SessionError> {
        write_session_export(state, path)
    }

    pub fn import_path(
        session_dir: &Path,
        path: &Path,
    ) -> Result<(Self, SessionState), SessionError> {
        let export = if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            let (_store, state) = Self::open(path.to_path_buf())?;
            SessionExport::from(&state)
        } else {
            let content = fs::read_to_string(path).map_err(|source| SessionError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            serde_json::from_str::<SessionExport>(&content).map_err(|source| {
                SessionError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
            })?
        };
        Self::import(session_dir, export)
    }

    pub fn list(session_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        if !session_dir.exists() {
            return Ok(Vec::new());
        }
        let mut summaries = Vec::new();
        for entry in fs::read_dir(session_dir).map_err(|source| SessionError::Read {
            path: session_dir.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionError::Read {
                path: session_dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            let store = Self { path: path.clone() };
            let state = store.load()?;
            let branch_summary = state
                .branch_summaries
                .iter()
                .rev()
                .find(|summary| summary.to_session_id == state.session_id)
                .map(|summary| summary.summary.clone());
            summaries.push(SessionSummary {
                path,
                session_id: state.session_id,
                cwd: state.cwd,
                name: state.name,
                labels: state.labels,
                parent_session_id: state.parent_session_id,
                branch_summary,
                modified,
            });
        }
        summaries.sort_by_key(|summary| summary.modified.unwrap_or(UNIX_EPOCH));
        Ok(summaries)
    }

    pub fn resolve(session_dir: &Path, reference: &str) -> Result<Option<PathBuf>, SessionError> {
        let reference_path = PathBuf::from(reference);
        if reference_path.exists() {
            return Ok(Some(reference_path));
        }
        let candidate = session_dir.join(reference);
        if candidate.exists() {
            return Ok(Some(candidate));
        }
        let jsonl_candidate = session_dir.join(format!("{reference}.jsonl"));
        if jsonl_candidate.exists() {
            return Ok(Some(jsonl_candidate));
        }
        let summaries = Self::list(session_dir)?;
        if let Ok(index) = reference.parse::<usize>() {
            if index > 0 {
                if let Some(summary) = summaries.get(index - 1) {
                    return Ok(Some(summary.path.clone()));
                }
            }
        }
        let matches = summaries
            .into_iter()
            .filter(|summary| {
                summary.session_id.starts_with(reference)
                    || summary.name.as_deref() == Some(reference)
            })
            .map(|summary| summary.path)
            .collect::<Vec<_>>();
        Ok(if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        })
    }

    fn load(&self) -> Result<SessionState, SessionError> {
        let file = File::open(&self.path).map_err(|source| SessionError::Open {
            path: self.path.clone(),
            source,
        })?;
        let mut state: Option<SessionState> = None;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|source| SessionError::Read {
                path: self.path.clone(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let record = serde_json::from_str::<SessionRecord>(&line).map_err(|source| {
                SessionError::Parse {
                    path: self.path.clone(),
                    source,
                }
            })?;
            match record {
                SessionRecord::Started { session_id, cwd } => {
                    state = Some(SessionState::new(session_id, cwd));
                }
                SessionRecord::Metadata {
                    name,
                    labels,
                    parent_session_id,
                } => {
                    if let Some(state) = &mut state {
                        state.name = name;
                        state.labels = labels.into_iter().collect();
                        state.parent_session_id = parent_session_id;
                    }
                }
                SessionRecord::Message { message } => {
                    if let Some(state) = &mut state {
                        state.messages.push(message);
                    }
                }
                SessionRecord::MessagesSnapshot { messages } => {
                    if let Some(state) = &mut state {
                        state.messages = messages;
                    }
                }
                SessionRecord::Tool { event } => {
                    if let Some(state) = &mut state {
                        state.tool_history.push(event);
                    }
                }
                SessionRecord::ActiveModel { model } => {
                    if let Some(state) = &mut state {
                        state.active_model = model;
                    }
                }
                SessionRecord::ActiveThinkingLevel { level } => {
                    if let Some(state) = &mut state {
                        state.active_thinking_level = level;
                    }
                }
                SessionRecord::ActiveTools { tools } => {
                    if let Some(state) = &mut state {
                        state.active_tool_names = tools.into_iter().collect();
                    }
                }
                SessionRecord::QueuedMessage { message } => {
                    if let Some(state) = &mut state {
                        state.queued_messages.push(message);
                    }
                }
                SessionRecord::QueuedMessagesSnapshot { messages } => {
                    if let Some(state) = &mut state {
                        state.queued_messages = messages;
                    }
                }
                SessionRecord::Compaction { record } => {
                    if let Some(state) = &mut state {
                        state.compactions.push(record);
                    }
                }
                SessionRecord::BranchSummary { summary } => {
                    if let Some(state) = &mut state {
                        state.branch_summaries.push(summary);
                    }
                }
            }
        }
        Ok(state.unwrap_or_else(|| SessionState::new("recovered", PathBuf::from("."))))
    }

    fn write_full_state(&self, state: &SessionState) -> Result<(), SessionError> {
        File::create(&self.path).map_err(|source| SessionError::Write {
            path: self.path.clone(),
            source,
        })?;
        self.append(&SessionRecord::Started {
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
        })?;
        self.record_metadata(state)?;
        self.record_active_model(state.active_model.clone())?;
        self.record_active_thinking_level(state.active_thinking_level.clone())?;
        self.record_active_tools(state.active_tool_names.iter().cloned().collect())?;
        for message in &state.messages {
            self.record_message(message.clone())?;
        }
        for event in &state.tool_history {
            self.record_tool(event.clone())?;
        }
        for message in &state.queued_messages {
            self.append(&SessionRecord::QueuedMessage {
                message: message.clone(),
            })?;
        }
        for record in &state.compactions {
            self.record_compaction(record.clone())?;
        }
        for summary in &state.branch_summaries {
            self.record_branch_summary(summary.clone())?;
        }
        Ok(())
    }

    fn append(&self, record: &SessionRecord) -> Result<(), SessionError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| SessionError::Open {
                path: self.path.clone(),
                source,
            })?;
        let line = serde_json::to_string(record).map_err(|source| SessionError::Parse {
            path: self.path.clone(),
            source,
        })?;
        writeln!(file, "{line}").map_err(|source| SessionError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

pub fn write_session_export(state: &SessionState, path: &Path) -> Result<(), SessionError> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("html") | Some("htm") => write_html_export(state, path),
        Some("jsonl") => write_jsonl_export(state, path),
        _ => {
            let content =
                serde_json::to_string_pretty(&SessionExport::from(state)).map_err(|source| {
                    SessionError::Parse {
                        path: path.to_path_buf(),
                        source,
                    }
                })?;
            fs::write(path, content).map_err(|source| SessionError::Write {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

fn write_jsonl_export(state: &SessionState, path: &Path) -> Result<(), SessionError> {
    let mut file = File::create(path).map_err(|source| SessionError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    let mut records = vec![
        SessionRecord::Started {
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
        },
        SessionRecord::Metadata {
            name: state.name.clone(),
            labels: state.labels.iter().cloned().collect(),
            parent_session_id: state.parent_session_id.clone(),
        },
        SessionRecord::ActiveModel {
            model: state.active_model.clone(),
        },
        SessionRecord::ActiveTools {
            tools: state.active_tool_names.iter().cloned().collect(),
        },
    ];
    records.extend(
        state
            .messages
            .iter()
            .cloned()
            .map(|message| SessionRecord::Message { message }),
    );
    records.extend(
        state
            .tool_history
            .iter()
            .cloned()
            .map(|event| SessionRecord::Tool { event }),
    );
    records.extend(
        state
            .queued_messages
            .iter()
            .cloned()
            .map(|message| SessionRecord::QueuedMessage { message }),
    );
    records.extend(
        state
            .compactions
            .iter()
            .cloned()
            .map(|record| SessionRecord::Compaction { record }),
    );
    records.extend(
        state
            .branch_summaries
            .iter()
            .cloned()
            .map(|summary| SessionRecord::BranchSummary { summary }),
    );
    for record in records {
        let line = serde_json::to_string(&record).map_err(|source| SessionError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        writeln!(file, "{line}").map_err(|source| SessionError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn write_html_export(state: &SessionState, path: &Path) -> Result<(), SessionError> {
    let mut content = String::new();
    content.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    content.push_str("<title>pi session export</title>");
    content.push_str("<style>body{font-family:system-ui,sans-serif;line-height:1.45;margin:2rem;max-width:960px}pre{white-space:pre-wrap;background:#f5f5f5;padding:1rem;border-radius:6px}.message,.tool{border-top:1px solid #ddd;padding:1rem 0}.role{font-weight:700;text-transform:uppercase;font-size:.8rem;color:#555}.meta{color:#555}</style>");
    content.push_str("</head><body>");
    content.push_str("<h1>pi session export</h1>");
    content.push_str(&format!(
        "<p class=\"meta\">session: {}<br>cwd: {}</p>",
        escape_html(&state.session_id),
        escape_html(&state.cwd.display().to_string())
    ));
    if let Some(name) = &state.name {
        content.push_str(&format!(
            "<p class=\"meta\">name: {}</p>",
            escape_html(name)
        ));
    }
    if !state.labels.is_empty() {
        content.push_str(&format!(
            "<p class=\"meta\">labels: {}</p>",
            escape_html(&state.labels.iter().cloned().collect::<Vec<_>>().join(", "))
        ));
    }
    for message in &state.messages {
        content.push_str("<section class=\"message\">");
        content.push_str(&format!(
            "<div class=\"role\">{:?}</div><pre>{}</pre>",
            message.role,
            escape_html(&message.content)
        ));
        content.push_str("</section>");
    }
    if !state.tool_history.is_empty() {
        content.push_str("<h2>tool history</h2>");
        for event in &state.tool_history {
            content.push_str("<section class=\"tool\">");
            content.push_str(&format!(
                "<div class=\"role\">{}</div><pre>{}</pre>",
                escape_html(&event.name),
                escape_html(&event.result)
            ));
            content.push_str("</section>");
        }
    }
    if !state.compactions.is_empty() {
        content.push_str("<h2>compactions</h2>");
        for record in &state.compactions {
            content.push_str("<section class=\"message\">");
            content.push_str(&format!(
                "<div class=\"role\">{:?}</div><pre>{}</pre>",
                record.kind,
                escape_html(&record.summary)
            ));
            content.push_str("</section>");
        }
    }
    if !state.branch_summaries.is_empty() {
        content.push_str("<h2>branch summaries</h2>");
        for summary in &state.branch_summaries {
            content.push_str("<section class=\"message\">");
            content.push_str(&format!(
                "<div class=\"role\">{} to {}</div><pre>{}</pre>",
                escape_html(&summary.from_session_id),
                escape_html(&summary.to_session_id),
                escape_html(&summary.summary)
            ));
            content.push_str("</section>");
        }
    }
    content.push_str("</body></html>");
    fs::write(path, content).map_err(|source| SessionError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn summarize_messages(messages: &[ConversationMessage], omitted_messages: usize) -> String {
    if omitted_messages == 0 {
        return "No compaction was needed.".to_string();
    }
    let role_counts = messages
        .iter()
        .take(omitted_messages)
        .fold(BTreeSet::new(), |mut roles, message| {
            roles.insert(format!("{:?}", message.role).to_lowercase());
            roles
        })
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let first = messages
        .first()
        .map(|message| trim_summary_text(&message.content))
        .unwrap_or_else(|| "-".to_string());
    let last_omitted = messages
        .get(omitted_messages.saturating_sub(1))
        .map(|message| trim_summary_text(&message.content))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "Compacted {omitted_messages} earlier message(s). Omitted roles: {role_counts}. First omitted: {first}. Last omitted: {last_omitted}."
    )
}

fn summarize_branch(source: &SessionState) -> String {
    let name = source.name.as_deref().unwrap_or("-");
    let labels = if source.labels.is_empty() {
        "-".to_string()
    } else {
        source.labels.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    let last_message = source
        .messages
        .last()
        .map(|message| trim_summary_text(&message.content))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "Branched from {} with name {name}, labels {labels}, {} message(s), and last message: {last_message}.",
        source.session_id,
        source.messages.len()
    )
}

fn trim_summary_text(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 160 {
        return collapsed;
    }
    let mut trimmed = collapsed.chars().take(157).collect::<String>();
    trimmed.push_str("...");
    trimmed
}

#[derive(Debug, Clone)]
pub struct Runtime {
    session: SessionState,
    systems: ReloadableSystems,
    store: Option<SessionStore>,
}

impl Runtime {
    pub fn new(session: SessionState, systems: ReloadableSystems) -> Self {
        Self {
            session,
            systems,
            store: None,
        }
    }

    pub fn with_store(
        session: SessionState,
        systems: ReloadableSystems,
        store: SessionStore,
    ) -> Self {
        Self {
            session,
            systems,
            store: Some(store),
        }
    }

    pub fn session(&self) -> &SessionState {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SessionState {
        &mut self.session
    }

    pub fn systems(&self) -> &ReloadableSystems {
        &self.systems
    }

    pub fn store(&self) -> Option<&SessionStore> {
        self.store.as_ref()
    }

    pub fn push_message(&mut self, message: ConversationMessage) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_message(message.clone())?;
        }
        self.session.messages.push(message);
        Ok(())
    }

    pub fn replace_messages(
        &mut self,
        messages: Vec<ConversationMessage>,
    ) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_messages_snapshot(messages.clone())?;
        }
        self.session.messages = messages;
        Ok(())
    }

    pub fn push_tool_event(&mut self, event: ToolEvent) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_tool(event.clone())?;
        }
        self.session.tool_history.push(event);
        Ok(())
    }

    pub fn set_active_model(&mut self, model: Option<ModelRef>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_active_model(model.clone())?;
        }
        self.session.active_model = model;
        Ok(())
    }

    pub fn set_active_thinking_level(&mut self, level: Option<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_active_thinking_level(level.clone())?;
        }
        self.session.active_thinking_level = level;
        Ok(())
    }

    pub fn set_active_tools(&mut self, tools: BTreeSet<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_active_tools(tools.iter().cloned().collect())?;
        }
        self.session.active_tool_names = tools;
        Ok(())
    }

    pub fn set_store(&mut self, store: SessionStore) {
        self.store = Some(store);
    }

    pub fn replace_session(&mut self, session: SessionState, store: Option<SessionStore>) {
        self.session = session;
        self.store = store;
    }

    pub fn rename_session(&mut self, name: Option<String>) -> Result<(), SessionError> {
        self.session.name = name;
        if let Some(store) = &self.store {
            store.record_metadata(&self.session)?;
        }
        Ok(())
    }

    pub fn set_labels(&mut self, labels: BTreeSet<String>) -> Result<(), SessionError> {
        self.session.labels = labels;
        if let Some(store) = &self.store {
            store.record_metadata(&self.session)?;
        }
        Ok(())
    }

    pub fn queue_message(&mut self, message: String) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_queued_message(message.clone())?;
        }
        self.session.queued_messages.push(message);
        Ok(())
    }

    pub fn replace_queued_messages(&mut self, messages: Vec<String>) -> Result<(), SessionError> {
        if let Some(store) = &self.store {
            store.record_queued_messages_snapshot(messages.clone())?;
        }
        self.session.queued_messages = messages;
        Ok(())
    }

    pub fn clear_queued_messages(&mut self) -> Result<usize, SessionError> {
        let count = self.session.queued_messages.len();
        self.replace_queued_messages(Vec::new())?;
        Ok(count)
    }

    pub fn compact_messages(
        &mut self,
        kind: CompactionKind,
    ) -> Result<CompactionRecord, SessionError> {
        let original_count = self.session.messages.len();
        let retained_messages = self
            .session
            .messages
            .iter()
            .rev()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        let omitted_messages = original_count.saturating_sub(retained_messages.len());
        let summary = summarize_messages(&self.session.messages, omitted_messages);
        let record = CompactionRecord {
            kind,
            omitted_messages,
            retained_messages: retained_messages.len(),
            summary: summary.clone(),
        };
        if omitted_messages > 0 {
            let mut messages = vec![ConversationMessage {
                role: MessageRole::System,
                content: summary,
                media: Vec::new(),
            }];
            messages.extend(retained_messages);
            self.replace_messages(messages)?;
        }
        if let Some(store) = &self.store {
            store.record_compaction(record.clone())?;
        }
        self.session.compactions.push(record.clone());
        Ok(record)
    }

    pub fn reload(&mut self, next: ReloadableSystems) -> Result<ReloadReport, ReloadError> {
        if next.config_generation < self.systems.config_generation {
            return Err(ReloadError::Invalid(
                "config generation cannot move backwards".to_string(),
            ));
        }

        let active_model_valid = self
            .session
            .active_model
            .as_ref()
            .map(|model| {
                next.available_models
                    .iter()
                    .any(|candidate| candidate == model)
            })
            .unwrap_or(true);
        let removed_active_tools = self
            .session
            .active_tool_names
            .iter()
            .filter(|tool_name| !next.available_tool_names.contains(*tool_name))
            .cloned()
            .collect();

        self.systems = next;
        Ok(ReloadReport {
            active_model_valid,
            removed_active_tools,
        })
    }
}

pub async fn run_user_turn(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
) -> Result<String, AgentError> {
    run_user_turn_streaming(runtime, provider, prompt, |_| {}).await
}

pub async fn run_user_turn_streaming(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
    on_text: impl FnMut(&str),
) -> Result<String, AgentError> {
    run_user_turn_streaming_with_media(runtime, provider, prompt, Vec::new(), on_text).await
}

pub async fn run_user_turn_streaming_with_media(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
    media: Vec<MediaInput>,
    mut on_text: impl FnMut(&str),
) -> Result<String, AgentError> {
    runtime.push_message(ConversationMessage {
        role: MessageRole::User,
        content: prompt.clone(),
        media,
    })?;

    if let Some(command) = parse_tool_command(&prompt)? {
        if !runtime.session.active_tool_names.contains(&command.name) {
            return Err(AgentError::DisabledTool(command.name));
        }
        let result = execute_tool(
            &runtime.session.cwd,
            command.request,
            &ToolRuntimeOptions {
                shell_path: runtime.systems.shell_path.clone(),
                shell_command_prefix: runtime.systems.shell_command_prefix.clone(),
            },
        )
        .await?;
        runtime.push_tool_event(ToolEvent {
            id: format!("tool-{}", runtime.session.tool_history.len() + 1),
            name: command.name,
            result: result.output.clone(),
        })?;
        runtime.push_message(ConversationMessage {
            role: MessageRole::Tool,
            content: result.output.clone(),
            media: Vec::new(),
        })?;
        return Ok(result.output);
    }

    let mut system_prompt = runtime.systems.system_prompt.clone();
    let mut context = runtime.systems.context_messages.clone();
    context.extend(runtime.systems.append_system_prompt.clone());
    if !context.is_empty() {
        let context_text = context.join("\n\n");
        system_prompt = Some(match system_prompt {
            Some(prompt) => format!("{prompt}\n\n{context_text}"),
            None => context_text,
        });
    }

    let request = ProviderRequest {
        system_prompt,
        messages: runtime
            .session
            .messages
            .iter()
            .map(|message| match message.role {
                MessageRole::System => ChatMessage {
                    role: ChatRole::System,
                    content: message.content.clone(),
                    media: message.media.clone(),
                },
                MessageRole::User => ChatMessage {
                    role: ChatRole::User,
                    content: message.content.clone(),
                    media: message.media.clone(),
                },
                MessageRole::Assistant => ChatMessage {
                    role: ChatRole::Assistant,
                    content: message.content.clone(),
                    media: message.media.clone(),
                },
                MessageRole::Tool => ChatMessage {
                    role: ChatRole::Tool,
                    content: message.content.clone(),
                    media: message.media.clone(),
                },
            })
            .collect(),
    };
    let events = complete_with_retry(provider, request, &runtime.systems.retry).await?;
    let mut text = String::new();
    for event in events {
        if let StreamEvent::Text(delta) = event {
            on_text(&delta);
            text.push_str(&delta);
        }
    }
    runtime.push_message(ConversationMessage {
        role: MessageRole::Assistant,
        content: text.clone(),
        media: Vec::new(),
    })?;
    Ok(text)
}

async fn complete_with_retry(
    provider: &dyn Provider,
    request: ProviderRequest,
    retry: &RuntimeRetrySettings,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let max_attempts = if retry.enabled {
        retry.max_retries.saturating_add(1)
    } else {
        1
    };
    let mut attempt = 0;
    loop {
        match provider.complete(request.clone()).await {
            Ok(events) => return Ok(events),
            Err(error) if is_context_overflow_error(&error) => return Err(error),
            Err(error) if attempt + 1 >= max_attempts => return Err(error),
            Err(_) => {
                let delay_ms = retry_delay_ms(retry.base_delay_ms, attempt);
                if delay_ms > 0 {
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                attempt += 1;
            }
        }
    }
}

fn is_context_overflow_error(error: &ProviderError) -> bool {
    let message = error.to_string().to_lowercase();
    let non_overflow = [
        "throttling error:",
        "service unavailable:",
        "rate limit",
        "too many requests",
    ];
    if non_overflow.iter().any(|pattern| message.contains(pattern)) {
        return false;
    }
    [
        "prompt is too long",
        "request_too_large",
        "input is too long for requested model",
        "exceeds the context window",
        "input token count",
        "maximum prompt length is",
        "reduce the length of the messages",
        "maximum context length is",
        "longer than the model's context length",
        "longer than the models context length",
        "exceeds the limit of",
        "exceeds the available context size",
        "greater than the context length",
        "context window exceeds limit",
        "exceeded model token limit",
        "too large for model with",
        "model_context_window_exceeded",
        "prompt too long; exceeded context length",
        "prompt too long; exceeded max context length",
        "context_length_exceeded",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "413",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn retry_delay_ms(base_delay_ms: u64, attempt: u64) -> u64 {
    let multiplier = 1_u64
        .checked_shl(attempt.min(16) as u32)
        .unwrap_or(u64::MAX);
    base_delay_ms.saturating_mul(multiplier)
}

pub async fn run_excluded_bash(runtime: &Runtime, command: String) -> Result<String, AgentError> {
    if !runtime.session.active_tool_names.contains("bash") {
        return Err(AgentError::DisabledTool("bash".to_string()));
    }
    let result = execute_tool(
        &runtime.session.cwd,
        ToolRequest::Bash {
            command,
            timeout_ms: Some(120_000),
        },
        &ToolRuntimeOptions {
            shell_path: runtime.systems.shell_path.clone(),
            shell_command_prefix: runtime.systems.shell_command_prefix.clone(),
        },
    )
    .await?;
    Ok(result.output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedToolCommand {
    name: String,
    request: ToolRequest,
}

fn parse_tool_command(prompt: &str) -> Result<Option<ParsedToolCommand>, AgentError> {
    let trimmed = prompt.trim();
    if !trimmed.starts_with('/') {
        return Ok(None);
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match command {
        "/read" => Ok(Some(ParsedToolCommand {
            name: "read".to_string(),
            request: ToolRequest::Read {
                path: required_arg(command, rest)?.to_string(),
            },
        })),
        "/bash" => Ok(Some(ParsedToolCommand {
            name: "bash".to_string(),
            request: ToolRequest::Bash {
                command: required_arg(command, rest)?.to_string(),
                timeout_ms: Some(120_000),
            },
        })),
        "/write" => {
            let (path, content) = split_once_arg(command, rest)?;
            Ok(Some(ParsedToolCommand {
                name: "write".to_string(),
                request: ToolRequest::Write {
                    path: path.to_string(),
                    content: content.to_string(),
                },
            }))
        }
        "/edit" => {
            let (path, rest) = split_once_arg(command, rest)?;
            let (find, replace) = split_once_arg(command, rest)?;
            Ok(Some(ParsedToolCommand {
                name: "edit".to_string(),
                request: ToolRequest::Edit {
                    path: path.to_string(),
                    find: find.to_string(),
                    replace: replace.to_string(),
                },
            }))
        }
        "/grep" => {
            let (pattern, path) = split_optional_arg(rest);
            let pattern = required_arg(command, pattern)?;
            Ok(Some(ParsedToolCommand {
                name: "grep".to_string(),
                request: ToolRequest::Grep {
                    path: path.map(ToString::to_string),
                    pattern: pattern.to_string(),
                },
            }))
        }
        "/find" => Ok(Some(ParsedToolCommand {
            name: "find".to_string(),
            request: ToolRequest::Find {
                pattern: required_arg(command, rest)?.to_string(),
            },
        })),
        "/ls" => Ok(Some(ParsedToolCommand {
            name: "ls".to_string(),
            request: ToolRequest::Ls {
                path: if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                },
            },
        })),
        "/reload" | "/quit" | "/help" => Ok(None),
        _ => Err(AgentError::UnknownCommand(command.to_string())),
    }
}

fn required_arg<'a>(command: &str, value: &'a str) -> Result<&'a str, AgentError> {
    if value.is_empty() {
        Err(AgentError::MissingCommandArgument(command.to_string()))
    } else {
        Ok(value)
    }
}

fn split_once_arg<'a>(command: &str, value: &'a str) -> Result<(&'a str, &'a str), AgentError> {
    let mut parts = value.splitn(2, char::is_whitespace);
    let first = required_arg(command, parts.next().unwrap_or_default())?;
    let rest = required_arg(command, parts.next().unwrap_or_default().trim())?;
    Ok((first, rest))
}

fn split_optional_arg(value: &str) -> (&str, Option<&str>) {
    let mut parts = value.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default();
    let rest = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    (first, rest)
}

fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis}-{}-{counter}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        Arc,
    };

    use pi_ai::{create_provider, ProviderApi, ProviderAuth, ProviderConfig};

    use super::*;

    #[test]
    fn reload_preserves_session_context() {
        let cwd = PathBuf::from("/repo");
        let model = ModelRef {
            provider: "openai".to_string(),
            id: "gpt-test".to_string(),
        };
        let mut session = SessionState::new("session-1", cwd.clone());
        session.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: "keep this".to_string(),
            media: Vec::new(),
        });
        session.tool_history.push(ToolEvent {
            id: "tool-1".to_string(),
            name: "read".to_string(),
            result: "ok".to_string(),
        });
        session.queued_messages.push("next".to_string());
        session.active_model = Some(model.clone());
        session.active_tool_names = BTreeSet::from(["read".to_string()]);

        let mut runtime = Runtime::new(session, ReloadableSystems::default());
        let next = ReloadableSystems {
            config_generation: 1,
            system_prompt: Some("new prompt".to_string()),
            available_models: vec![model],
            available_tool_names: BTreeSet::from(["read".to_string(), "bash".to_string()]),
            keybinding_generation: 1,
            ..ReloadableSystems::default()
        };

        let report = runtime.reload(next).expect("reload should succeed");

        assert!(report.active_model_valid);
        assert!(report.removed_active_tools.is_empty());
        assert_eq!(runtime.session().session_id, "session-1");
        assert_eq!(runtime.session().cwd, cwd);
        assert_eq!(runtime.session().messages.len(), 1);
        assert_eq!(runtime.session().tool_history.len(), 1);
        assert_eq!(runtime.session().queued_messages, ["next"]);
    }

    #[test]
    fn invalid_reload_keeps_existing_systems() {
        let session = SessionState::new("session-1", PathBuf::from("/repo"));
        let mut runtime = Runtime::new(
            session,
            ReloadableSystems {
                config_generation: 2,
                ..ReloadableSystems::default()
            },
        );

        let result = runtime.reload(ReloadableSystems {
            config_generation: 1,
            ..ReloadableSystems::default()
        });

        assert_eq!(
            result,
            Err(ReloadError::Invalid(
                "config generation cannot move backwards".to_string()
            ))
        );
        assert_eq!(runtime.systems().config_generation, 2);
    }

    #[test]
    fn reload_reports_invalid_active_model_without_clearing_it() {
        let mut session = SessionState::new("session-1", PathBuf::from("/repo"));
        session.active_model = Some(ModelRef {
            provider: "openai".to_string(),
            id: "removed".to_string(),
        });

        let mut runtime = Runtime::new(session, ReloadableSystems::default());
        let report = runtime
            .reload(ReloadableSystems {
                config_generation: 1,
                available_models: vec![ModelRef {
                    provider: "anthropic".to_string(),
                    id: "claude-test".to_string(),
                }],
                ..ReloadableSystems::default()
            })
            .expect("reload should keep the session and report invalid model");

        assert!(!report.active_model_valid);
        assert_eq!(
            runtime.session().active_model,
            Some(ModelRef {
                provider: "openai".to_string(),
                id: "removed".to_string(),
            })
        );
    }

    #[test]
    fn session_store_round_trips_messages() {
        let base = std::env::temp_dir().join(format!("pi-session-test-{}", new_session_id()));
        let (store, mut state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        state.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
        });
        state.active_thinking_level = Some("xhigh".to_string());
        store
            .record_message(state.messages[0].clone())
            .expect("record message");
        store
            .record_active_thinking_level(state.active_thinking_level.clone())
            .expect("record thinking");

        let (_store, loaded) =
            SessionStore::open(store.path().to_path_buf()).expect("open session");

        assert_eq!(loaded.cwd, PathBuf::from("/repo"));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");
        assert_eq!(loaded.active_thinking_level, Some("xhigh".to_string()));

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn session_exports_jsonl_and_html_and_imports_jsonl() {
        let base =
            std::env::temp_dir().join(format!("pi-session-export-test-{}", new_session_id()));
        let export_dir = base.join("exports");
        fs::create_dir_all(&export_dir).expect("create export dir");
        let (store, mut state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        state.name = Some("exported".to_string());
        state.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: "hello <world>".to_string(),
            media: Vec::new(),
        });
        state.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            content: "done".to_string(),
            media: Vec::new(),
        });
        state.tool_history.push(ToolEvent {
            id: "tool-1".to_string(),
            name: "read".to_string(),
            result: "content".to_string(),
        });
        store.record_metadata(&state).expect("record metadata");
        store
            .record_message(state.messages[0].clone())
            .expect("record user");
        store
            .record_message(state.messages[1].clone())
            .expect("record assistant");
        store
            .record_tool(state.tool_history[0].clone())
            .expect("record tool");

        let jsonl = export_dir.join("session.jsonl");
        let html = export_dir.join("session.html");
        store.export_state(&state, &jsonl).expect("export jsonl");
        store.export_state(&state, &html).expect("export html");

        let jsonl_content = fs::read_to_string(&jsonl).expect("read jsonl");
        assert!(jsonl_content.contains("\"type\":\"message\""));
        let html_content = fs::read_to_string(&html).expect("read html");
        assert!(html_content.contains("hello &lt;world&gt;"));
        assert!(html_content.contains("tool history"));

        let import_dir = base.join("imported");
        let (_import_store, imported) =
            SessionStore::import_path(&import_dir, &jsonl).expect("import jsonl");
        assert_eq!(imported.session_id, state.session_id);
        assert_eq!(imported.messages, state.messages);
        assert_eq!(imported.tool_history, state.tool_history);

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn session_store_forks_with_metadata_and_parent() {
        let base = std::env::temp_dir().join(format!("pi-session-fork-test-{}", new_session_id()));
        let (store, mut state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        state.name = Some("main".to_string());
        state.labels = BTreeSet::from(["feature".to_string()]);
        state.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: "parent prompt".to_string(),
            media: Vec::new(),
        });
        store.record_metadata(&state).expect("record metadata");
        store
            .record_message(state.messages[0].clone())
            .expect("record message");

        let (fork_store, forked) = SessionStore::fork(&base, &state, false).expect("fork session");
        let (_opened_store, opened) =
            SessionStore::open(fork_store.path().to_path_buf()).expect("open fork");

        assert_ne!(forked.session_id, state.session_id);
        assert_eq!(opened.parent_session_id, Some(state.session_id.clone()));
        assert_eq!(opened.name, Some("main".to_string()));
        assert_eq!(opened.labels, BTreeSet::from(["feature".to_string()]));
        assert_eq!(opened.messages[0].content, "parent prompt");
        assert_eq!(opened.branch_summaries.len(), 1);
        assert_eq!(opened.branch_summaries[0].from_session_id, state.session_id);
        assert_eq!(opened.branch_summaries[0].to_session_id, forked.session_id);

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn compact_messages_persists_summary_record_and_snapshot() {
        let base = std::env::temp_dir().join(format!("pi-compact-test-{}", new_session_id()));
        let (store, mut state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        for index in 0..8 {
            state.messages.push(ConversationMessage {
                role: MessageRole::User,
                content: format!("message {index}"),
                media: Vec::new(),
            });
            store
                .record_message(state.messages[index].clone())
                .expect("record message");
        }
        let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());

        let record = runtime
            .compact_messages(CompactionKind::Manual)
            .expect("compact messages");

        assert_eq!(record.omitted_messages, 4);
        assert_eq!(record.retained_messages, 4);
        assert_eq!(runtime.session().messages.len(), 5);
        assert_eq!(runtime.session().compactions, [record]);
        let (_store, loaded) =
            SessionStore::open(store.path().to_path_buf()).expect("open compacted");
        assert_eq!(loaded.compactions.len(), 1);
        assert_eq!(loaded.messages[0].role, MessageRole::System);
        assert!(loaded.messages[0].content.contains("Compacted 4 earlier"));

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn session_store_resolves_id_prefix_and_name() {
        let base =
            std::env::temp_dir().join(format!("pi-session-resolve-test-{}", new_session_id()));
        let (store, mut state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        state.name = Some("named-session".to_string());
        store.record_metadata(&state).expect("record metadata");

        let prefix = &state.session_id[..8];
        assert_eq!(
            SessionStore::resolve(&base, prefix).expect("resolve prefix"),
            Some(store.path().to_path_buf())
        );
        assert_eq!(
            SessionStore::resolve(&base, "named-session").expect("resolve name"),
            Some(store.path().to_path_buf())
        );
        assert_eq!(
            SessionStore::resolve(&base, "1").expect("resolve index"),
            Some(store.path().to_path_buf())
        );

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn messages_snapshot_replaces_loaded_messages() {
        let base =
            std::env::temp_dir().join(format!("pi-session-snapshot-test-{}", new_session_id()));
        let (store, _state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        store
            .record_message(ConversationMessage {
                role: MessageRole::User,
                content: "old".to_string(),
                media: Vec::new(),
            })
            .expect("record message");
        store
            .record_messages_snapshot(vec![ConversationMessage {
                role: MessageRole::System,
                content: "summary".to_string(),
                media: Vec::new(),
            }])
            .expect("record snapshot");

        let (_store, loaded) =
            SessionStore::open(store.path().to_path_buf()).expect("open session");

        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "summary");

        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn run_user_turn_records_user_and_assistant_messages() {
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        let provider = create_provider(ProviderConfig {
            model: ModelRef {
                provider: "faux".to_string(),
                id: "echo".to_string(),
            },
            api: ProviderApi::Faux,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
        });

        let response = run_user_turn(&mut runtime, provider.as_ref(), "hello".to_string())
            .await
            .expect("run turn");

        assert_eq!(response, "[faux/echo] hello");
        assert_eq!(runtime.session().messages.len(), 2);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
        assert_eq!(runtime.session().messages[1].role, MessageRole::Assistant);
    }

    #[tokio::test]
    async fn run_user_turn_streams_text_deltas() {
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        let provider = create_provider(ProviderConfig {
            model: ModelRef {
                provider: "faux".to_string(),
                id: "echo".to_string(),
            },
            api: ProviderApi::Faux,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
        });
        let mut deltas = Vec::new();

        let response = run_user_turn_streaming(
            &mut runtime,
            provider.as_ref(),
            "hello".to_string(),
            |delta| deltas.push(delta.to_string()),
        )
        .await
        .expect("run streaming turn");

        assert_eq!(deltas, ["[faux/echo] ", "hello"]);
        assert_eq!(response, "[faux/echo] hello");
        assert_eq!(runtime.session().messages[1].content, response);
    }

    #[tokio::test]
    async fn provider_retry_does_not_duplicate_user_message() {
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems {
                retry: RuntimeRetrySettings {
                    enabled: true,
                    max_retries: 2,
                    base_delay_ms: 0,
                },
                ..ReloadableSystems::default()
            },
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = FlakyProvider {
            attempts: Arc::clone(&attempts),
            fail_before_success: 1,
        };

        let response = run_user_turn(&mut runtime, &provider, "hello".to_string())
            .await
            .expect("retry should recover");

        assert_eq!(response, "retried");
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(runtime.session().messages.len(), 2);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
        assert_eq!(runtime.session().messages[1].role, MessageRole::Assistant);
    }

    #[tokio::test]
    async fn disabled_provider_retry_returns_first_attempt_failure() {
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems {
                retry: RuntimeRetrySettings {
                    enabled: false,
                    max_retries: 2,
                    base_delay_ms: 0,
                },
                ..ReloadableSystems::default()
            },
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = FlakyProvider {
            attempts: Arc::clone(&attempts),
            fail_before_success: 1,
        };

        let error = run_user_turn(&mut runtime, &provider, "hello".to_string())
            .await
            .expect_err("retry should be disabled");

        assert!(matches!(
            error,
            AgentError::Provider(ProviderError::InvalidResponse(_))
        ));
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(runtime.session().messages.len(), 1);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn provider_retry_skips_context_overflow_errors() {
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems {
                retry: RuntimeRetrySettings {
                    enabled: true,
                    max_retries: 3,
                    base_delay_ms: 0,
                },
                ..ReloadableSystems::default()
            },
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = OverflowProvider {
            attempts: Arc::clone(&attempts),
        };

        let error = run_user_turn(&mut runtime, &provider, "hello".to_string())
            .await
            .expect_err("context overflow should not retry");

        assert!(matches!(
            error,
            AgentError::Provider(ProviderError::InvalidResponse(_))
        ));
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(runtime.session().messages.len(), 1);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    }

    #[test]
    fn queued_messages_are_persisted_and_clearable() {
        let base = std::env::temp_dir().join(format!("pi-queue-test-{}", new_session_id()));
        let (store, state) =
            SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
        let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());

        runtime
            .queue_message("first follow-up".to_string())
            .expect("queue message");
        runtime
            .queue_message("second follow-up".to_string())
            .expect("queue message");
        let cleared = runtime.clear_queued_messages().expect("clear queue");

        assert_eq!(cleared, 2);
        assert!(runtime.session().queued_messages.is_empty());
        let (_store, loaded) = SessionStore::open(store.path().to_path_buf()).expect("open queue");
        assert!(loaded.queued_messages.is_empty());

        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn excluded_bash_does_not_enter_context() {
        let cwd = std::env::temp_dir().join(format!("pi-excluded-bash-test-{}", new_session_id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let runtime = Runtime::new(
            SessionState::new("session-1", cwd.clone()),
            ReloadableSystems::default(),
        );

        let output = run_excluded_bash(&runtime, "printf shell-ok".to_string())
            .await
            .expect("run excluded bash");

        assert_eq!(output, "shell-ok");
        assert!(runtime.session().messages.is_empty());
        assert!(runtime.session().tool_history.is_empty());
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn disabled_tool_command_is_rejected_before_execution() {
        let cwd = std::env::temp_dir().join(format!("pi-disabled-tool-test-{}", new_session_id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let mut session = SessionState::new("session-1", cwd.clone());
        session.active_tool_names.remove("write");
        let mut runtime = Runtime::new(session, ReloadableSystems::default());
        let provider = create_provider(ProviderConfig {
            model: ModelRef {
                provider: "faux".to_string(),
                id: "echo".to_string(),
            },
            api: ProviderApi::Faux,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
        });

        let error = run_user_turn(
            &mut runtime,
            provider.as_ref(),
            "/write blocked.txt nope".to_string(),
        )
        .await
        .expect_err("write tool should be disabled");

        assert!(matches!(error, AgentError::DisabledTool(tool) if tool == "write"));
        assert!(!cwd.join("blocked.txt").exists());
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn session_ids_are_unique_inside_one_process() {
        let first = new_session_id();
        let second = new_session_id();

        assert_ne!(first, second);
    }

    struct FlakyProvider {
        attempts: Arc<AtomicUsize>,
        fail_before_success: usize,
    }

    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let attempt = self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
            if attempt < self.fail_before_success {
                return Err(ProviderError::InvalidResponse("temporary".to_string()));
            }
            Ok(vec![
                StreamEvent::Text("retried".to_string()),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ])
        }
    }

    struct OverflowProvider {
        attempts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for OverflowProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
            Err(ProviderError::InvalidResponse(
                "Your input exceeds the context window of this model".to_string(),
            ))
        }
    }
}

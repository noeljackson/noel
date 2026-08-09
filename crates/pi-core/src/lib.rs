use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pi_agent::{CancellationToken, RetryPolicy};
use pi_ai::{
    ChatMessage, ChatRole, ChatToolCall, MediaInput, ModelRef, Provider, ProviderError,
    ProviderRequest, StreamEvent, ToolDefinition as AiToolDefinition,
};
use pi_config::{has_auth_for_provider, LoadedConfig, ResourceFile};
use pi_tools::{
    builtin_tool_definitions, execute_tool, ToolError, ToolRequest, ToolRuntimeOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default)]
    pub media: Vec<MediaInput>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ChatToolCall>,
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

fn session_state_from_export(export: SessionExport, session_id: String) -> SessionState {
    SessionState {
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
    pub extension_tools: BTreeMap<String, ExtensionTool>,
    pub keybinding_generation: u64,
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub retry: RuntimeRetrySettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub extension_name: String,
    pub extension_path: PathBuf,
    pub protocol: String,
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
        let extension_tools = extension_tools_from_resources(&config.extensions);
        let available_tool_names = match &config.settings.enabled_tools {
            Some(enabled_tools) => enabled_tools.iter().cloned().collect(),
            None => builtin_tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .chain(extension_tools.keys().cloned())
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
            extension_tools,
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
    #[error(transparent)]
    Journal(#[from] pi_session::JournalError),
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
    #[error("invalid model tool call for {tool}: {message}")]
    InvalidToolCall { tool: String, message: String },
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
        Self::create_with_id(session_dir, cwd, new_session_id())
    }

    pub fn create_with_id(
        session_dir: &Path,
        cwd: PathBuf,
        session_id: impl Into<String>,
    ) -> Result<(Self, SessionState), SessionError> {
        fs::create_dir_all(session_dir).map_err(|source| SessionError::CreateDir {
            path: session_dir.to_path_buf(),
            source,
        })?;
        let session_id = session_id.into();
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
            export.session_id.clone()
        };
        let path = session_dir.join(format!("{session_id}.jsonl"));
        let store = Self { path };
        let state = session_state_from_export(export, session_id);
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
            let content = fs::read_to_string(path).map_err(|source| SessionError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            if jsonl_is_ts_session_export(&content) {
                read_ts_jsonl_export(path, &content)?
            } else {
                let (_store, state) = Self::open(path.to_path_buf())?;
                SessionExport::from(&state)
            }
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
        let mut file = File::open(&self.path).map_err(|source| SessionError::Open {
            path: self.path.clone(),
            source,
        })?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|source| SessionError::Read {
                path: self.path.clone(),
                source,
            })?;
        if jsonl_is_ts_session_export(&content) {
            let export = read_ts_jsonl_export(&self.path, &content)?;
            let session_id = if export.session_id.trim().is_empty() {
                "recovered".to_string()
            } else {
                export.session_id.clone()
            };
            return Ok(session_state_from_export(export, session_id));
        }
        let mut state: Option<SessionState> = None;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let record = serde_json::from_str::<SessionRecord>(line).map_err(|source| {
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
        pi_session::append_jsonl_record(&self.path, record)?;
        Ok(())
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
    for record in ts_jsonl_export_records(state) {
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

fn ts_jsonl_export_records(state: &SessionState) -> Vec<Value> {
    let timestamp = ts_jsonl_timestamp();
    let mut records = vec![json!({
        "type": "session",
        "version": 3,
        "id": state.session_id,
        "timestamp": timestamp.clone(),
        "cwd": state.cwd,
        "parentSession": state.parent_session_id,
    })];
    let mut parent_id: Option<String> = None;
    macro_rules! next_entry {
        ($entry_type:expr, $fields:expr $(,)?) => {
            push_ts_jsonl_entry(
                &mut records,
                &mut parent_id,
                &timestamp,
                $entry_type,
                $fields,
            );
        };
    }

    next_entry!(
        "custom",
        json!({
            "customType": "rust_session_state",
            "data": {
                "labels": state.labels,
                "active_tool_names": state.active_tool_names,
                "queued_messages": state.queued_messages,
                "tool_history": state.tool_history,
            },
        }),
    );
    if let Some(name) = &state.name {
        next_entry!("session_info", json!({ "name": name }));
    }
    if let Some(model) = &state.active_model {
        next_entry!(
            "model_change",
            json!({
                "provider": model.provider,
                "modelId": model.id,
            }),
        );
    }
    if let Some(level) = &state.active_thinking_level {
        next_entry!("thinking_level_change", json!({ "thinkingLevel": level }),);
    }
    for message in &state.messages {
        next_entry!("message", json!({ "message": ts_jsonl_message(message) }),);
    }
    for label in &state.labels {
        next_entry!("label", json!({ "label": label }),);
    }
    for record in &state.compactions {
        let first_kept_entry_id = parent_id.clone();
        next_entry!(
            "compaction",
            json!({
                "summary": record.summary,
                "firstKeptEntryId": first_kept_entry_id,
                "tokensBefore": record.omitted_messages,
                "details": {
                    "kind": record.kind,
                    "retained_messages": record.retained_messages,
                },
            }),
        );
    }
    for summary in &state.branch_summaries {
        next_entry!(
            "branch_summary",
            json!({
                "fromId": summary.from_session_id,
                "summary": summary.summary,
                "details": {
                    "to_session_id": summary.to_session_id,
                },
            }),
        );
    }
    records
}

fn ts_jsonl_timestamp() -> String {
    time::OffsetDateTime::from(SystemTime::now())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn push_ts_jsonl_entry(
    records: &mut Vec<Value>,
    parent_id: &mut Option<String>,
    timestamp: &str,
    entry_type: &str,
    mut fields: Value,
) {
    let id = format!("e{}", records.len());
    let object = fields
        .as_object_mut()
        .expect("session export fields must be an object");
    object.insert("type".to_string(), json!(entry_type));
    object.insert("id".to_string(), json!(id.clone()));
    object.insert("parentId".to_string(), json!(parent_id));
    object.insert("timestamp".to_string(), json!(timestamp));
    *parent_id = Some(id);
    records.push(fields);
}

fn ts_jsonl_message(message: &ConversationMessage) -> Value {
    let role = match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "toolResult",
        MessageRole::System => "system",
    };
    json!({
        "role": role,
        "content": message.content,
        "media": message.media,
        "toolCallId": message.tool_call_id,
        "toolName": message.tool_name,
        "toolCalls": message.tool_calls,
    })
}

fn jsonl_is_ts_session_export(content: &str) -> bool {
    content
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some("session")
}

fn read_ts_jsonl_export(path: &Path, content: &str) -> Result<SessionExport, SessionError> {
    let records = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).map_err(|source| SessionError::Parse {
                path: path.to_path_buf(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let header = records.first().ok_or_else(|| SessionError::Open {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, "missing session header"),
    })?;
    let session_id = header
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("imported")
        .to_string();
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let parent_session_id = header
        .get("parentSession")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut state = SessionState::new(session_id, cwd);
    state.parent_session_id = parent_session_id;

    for entry in ts_jsonl_active_path(&records) {
        reduce_ts_jsonl_entry(&mut state, entry);
    }
    Ok(SessionExport::from(&state))
}

fn ts_jsonl_active_path(records: &[Value]) -> Vec<&Value> {
    let entries = records
        .iter()
        .skip(1)
        .filter(|entry| entry.get("id").and_then(Value::as_str).is_some())
        .collect::<Vec<_>>();
    let Some(leaf_id) = entries.iter().rev().find_map(|entry| {
        if entry.get("type").and_then(Value::as_str) == Some("leaf") {
            return entry
                .get("targetId")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        entry
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    }) else {
        return Vec::new();
    };
    let mut path = Vec::new();
    let mut current = Some(leaf_id);
    while let Some(id) = current {
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
        else {
            break;
        };
        path.push(*entry);
        current = entry
            .get("parentId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }
    path.reverse();
    path
}

fn reduce_ts_jsonl_entry(state: &mut SessionState, entry: &Value) {
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => {
            if let Some(message) = entry.get("message").and_then(ts_jsonl_conversation_message) {
                state.messages.push(message);
            }
        }
        Some("session_info") => {
            state.name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToString::to_string);
        }
        Some("model_change") => {
            if let (Some(provider), Some(id)) = (
                entry.get("provider").and_then(Value::as_str),
                entry.get("modelId").and_then(Value::as_str),
            ) {
                state.active_model = Some(ModelRef {
                    provider: provider.to_string(),
                    id: id.to_string(),
                });
            }
        }
        Some("thinking_level_change") => {
            state.active_thinking_level = entry
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        Some("custom")
            if entry.get("customType").and_then(Value::as_str) == Some("rust_session_state") =>
        {
            restore_rust_session_state(state, entry.get("data").unwrap_or(&Value::Null));
        }
        Some("compaction") => {
            state.compactions.push(CompactionRecord {
                kind: entry
                    .pointer("/details/kind")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or(CompactionKind::Manual),
                omitted_messages: entry
                    .get("tokensBefore")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize,
                retained_messages: entry
                    .pointer("/details/retained_messages")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize,
                summary: entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Some("branch_summary") => {
            state.branch_summaries.push(BranchSummary {
                from_session_id: entry
                    .get("fromId")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                to_session_id: entry
                    .pointer("/details/to_session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&state.session_id)
                    .to_string(),
                summary: entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Some("label") => {
            if let Some(label) = entry.get("label").and_then(Value::as_str) {
                if !label.trim().is_empty() {
                    state.labels.insert(label.trim().to_string());
                }
            }
        }
        _ => {}
    }
}

fn restore_rust_session_state(state: &mut SessionState, data: &Value) {
    if let Some(labels) = data.get("labels").and_then(Value::as_array) {
        state.labels = labels
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
    }
    if let Some(tools) = data.get("active_tool_names").and_then(Value::as_array) {
        state.active_tool_names = tools
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
    }
    if let Some(messages) = data.get("queued_messages").and_then(Value::as_array) {
        state.queued_messages = messages
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
    }
    if let Some(history) = data.get("tool_history").and_then(Value::as_array) {
        state.tool_history = history
            .iter()
            .filter_map(|event| serde_json::from_value(event.clone()).ok())
            .collect();
    }
}

fn ts_jsonl_conversation_message(value: &Value) -> Option<ConversationMessage> {
    if let Ok(message) = serde_json::from_value::<ConversationMessage>(value.clone()) {
        return Some(message);
    }
    let role = match value.get("role").and_then(Value::as_str)? {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "toolResult" | "tool" => MessageRole::Tool,
        "system" => MessageRole::System,
        _ => return None,
    };
    Some(ConversationMessage {
        role,
        content: ts_jsonl_text_content(value.get("content").unwrap_or(&Value::Null)),
        media: value
            .get("media")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        tool_call_id: value
            .get("toolCallId")
            .or_else(|| value.get("tool_call_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        tool_name: value
            .get("toolName")
            .or_else(|| value.get("tool_name"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        tool_calls: value
            .get("toolCalls")
            .or_else(|| value.get("tool_calls"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
    })
}

fn ts_jsonl_text_content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("content"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
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
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
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

/// Maximum number of model responses that may contain tool calls before the
/// agent gives up on a single user turn. Each iteration can issue several tool
/// calls at once, but real coding work (read, edit, run tests, iterate) still
/// needs far more than a handful of rounds, so keep this generous while still
/// bounding a runaway tool loop.
const MAX_TOOL_CALL_TURNS: usize = 50;

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
    on_text: impl FnMut(&str) + Send,
) -> Result<String, AgentError> {
    run_user_turn_streaming_with_media(runtime, provider, prompt, Vec::new(), on_text).await
}

pub async fn run_user_turn_streaming_with_media(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
    media: Vec<MediaInput>,
    mut on_text: impl FnMut(&str) + Send,
) -> Result<String, AgentError> {
    runtime.push_message(ConversationMessage {
        role: MessageRole::User,
        content: prompt.clone(),
        media,
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
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
            name: command.name.clone(),
            result: result.output.clone(),
        })?;
        runtime.push_message(ConversationMessage {
            role: MessageRole::Tool,
            content: result.output.clone(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: Some(command.name.clone()),
            tool_calls: Vec::new(),
        })?;
        return Ok(result.output);
    }

    let system_prompt = runtime_system_prompt(runtime);
    let mut final_text = String::new();
    for _ in 0..MAX_TOOL_CALL_TURNS {
        let request = provider_request(runtime, system_prompt.clone());
        let events =
            complete_with_retry_streaming(provider, request, &runtime.systems.retry, |event| {
                if let StreamEvent::Text(delta) = event {
                    on_text(delta);
                }
            })
            .await?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for event in events {
            match event {
                StreamEvent::Text(delta) => {
                    text.push_str(&delta);
                }
                StreamEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_calls.push(ChatToolCall {
                    id,
                    name,
                    arguments,
                }),
                StreamEvent::Thinking(_) | StreamEvent::Usage { .. } | StreamEvent::Stop { .. } => {
                }
            }
        }

        final_text.push_str(&text);
        runtime.push_message(ConversationMessage {
            role: MessageRole::Assistant,
            content: text,
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: tool_calls.clone(),
        })?;

        if tool_calls.is_empty() {
            return Ok(final_text);
        }

        for tool_call in tool_calls {
            execute_model_tool_call(runtime, &tool_call).await?;
        }
    }

    Err(AgentError::InvalidToolCall {
        tool: "agent".to_string(),
        message: "model exceeded maximum tool-call turns".to_string(),
    })
}

fn runtime_system_prompt(runtime: &Runtime) -> Option<String> {
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
    system_prompt
}

fn provider_request(runtime: &Runtime, system_prompt: Option<String>) -> ProviderRequest {
    ProviderRequest {
        system_prompt,
        messages: provider_messages(&runtime.session.messages),
        tools: active_tool_definitions(runtime),
    }
}

fn provider_messages(messages: &[ConversationMessage]) -> Vec<ChatMessage> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == MessageRole::Assistant && !message.tool_calls.is_empty() {
            output.push(conversation_to_chat_message(message));
            index += 1;

            let mut tool_outputs = BTreeMap::new();
            while index < messages.len() && messages[index].role == MessageRole::Tool {
                let tool_message = conversation_to_chat_message(&messages[index]);
                if let Some(tool_call_id) = tool_message.tool_call_id.clone() {
                    tool_outputs.entry(tool_call_id).or_insert(tool_message);
                }
                index += 1;
            }

            for tool_call in &message.tool_calls {
                output.push(
                    tool_outputs
                        .remove(&tool_call.id)
                        .unwrap_or_else(|| missing_tool_output(tool_call)),
                );
            }
            continue;
        }

        if message.role != MessageRole::Tool {
            output.push(conversation_to_chat_message(message));
        }
        index += 1;
    }
    output
}

fn missing_tool_output(tool_call: &ChatToolCall) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Tool,
        content: format!(
            "tool call {} did not complete before the session continued",
            tool_call.name
        ),
        media: Vec::new(),
        tool_call_id: Some(tool_call.id.clone()),
        tool_name: Some(tool_call.name.clone()),
        tool_calls: Vec::new(),
    }
}

fn conversation_to_chat_message(message: &ConversationMessage) -> ChatMessage {
    ChatMessage {
        role: match message.role {
            MessageRole::System => ChatRole::System,
            MessageRole::User => ChatRole::User,
            MessageRole::Assistant => ChatRole::Assistant,
            MessageRole::Tool => ChatRole::Tool,
        },
        content: message.content.clone(),
        media: message.media.clone(),
        tool_call_id: message.tool_call_id.clone(),
        tool_name: message.tool_name.clone(),
        tool_calls: message.tool_calls.clone(),
    }
}

async fn complete_with_retry_streaming(
    provider: &dyn Provider,
    request: ProviderRequest,
    retry: &RuntimeRetrySettings,
    mut on_event: impl FnMut(&StreamEvent) + Send,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let retry = RetryPolicy {
        max_retries: if retry.enabled { retry.max_retries } else { 0 },
        base_delay_ms: retry.base_delay_ms,
    };
    pi_agent::complete_with_retry(
        provider,
        request,
        &retry,
        &CancellationToken::default(),
        |event| on_event(event),
    )
    .await
    .map_err(|error| match error {
        pi_agent::AgentError::Provider(error) => error,
        other => ProviderError::Config(other.to_string()),
    })
}

fn active_tool_definitions(runtime: &Runtime) -> Vec<AiToolDefinition> {
    let mut definitions = builtin_tool_definitions()
        .into_iter()
        .filter(|tool| model_tool_enabled(runtime, &tool.name))
        .filter_map(|tool| model_tool_definition(&tool.name))
        .collect::<Vec<_>>();
    definitions.extend(
        runtime
            .systems
            .extension_tools
            .values()
            .filter(|tool| model_tool_enabled(runtime, &tool.name))
            .map(|tool| AiToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            }),
    );
    definitions
}

fn model_tool_enabled(runtime: &Runtime, name: &str) -> bool {
    (runtime.session.active_tool_names.contains(name)
        || runtime.systems.extension_tools.contains_key(name))
        && (runtime.systems.available_tool_names.is_empty()
            || runtime.systems.available_tool_names.contains(name))
}

fn model_tool_definition(name: &str) -> Option<AiToolDefinition> {
    let definition = match name {
        "read" => AiToolDefinition {
            name: "read".to_string(),
            description: "Read a UTF-8 text file from the current workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "number" },
                    "limit": { "type": "number" }
                },
                "required": ["path"]
            }),
        },
        "bash" => AiToolDefinition {
            name: "bash".to_string(),
            description: "Execute a shell command in the current workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "number" }
                },
                "required": ["command"]
            }),
        },
        "edit" => AiToolDefinition {
            name: "edit".to_string(),
            description: "Apply exact text replacements to a file.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string" },
                                "newText": { "type": "string" }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        },
        "write" => AiToolDefinition {
            name: "write".to_string(),
            description: "Write UTF-8 text to a file in the current workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        "grep" => AiToolDefinition {
            name: "grep".to_string(),
            description: "Search files in the current workspace for a pattern.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "literal": { "type": "boolean" },
                    "ignoreCase": { "type": "boolean" },
                    "context": { "type": "number" },
                    "limit": { "type": "number" }
                },
                "required": ["pattern"]
            }),
        },
        "find" => AiToolDefinition {
            name: "find".to_string(),
            description: "Find file paths in the current workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "limit": { "type": "number" }
                },
                "required": ["pattern"]
            }),
        },
        "ls" => AiToolDefinition {
            name: "ls".to_string(),
            description: "List directory entries in the current workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "number" }
                },
                "required": []
            }),
        },
        _ => return None,
    };
    Some(definition)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionManifest {
    protocol: Option<String>,
    #[serde(default)]
    tools: Vec<ExtensionToolManifest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionToolManifest {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionProtocolRequest<'a> {
    protocol_version: u8,
    kind: &'static str,
    command: &'a str,
    input: &'a str,
    cwd: &'a str,
    tool_call_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionCommandResponse {
    output: Option<String>,
    error: Option<String>,
}

fn extension_tools_from_resources(extensions: &[ResourceFile]) -> BTreeMap<String, ExtensionTool> {
    let mut tools = BTreeMap::new();
    for extension in extensions {
        let Some(manifest) = read_extension_manifest(&extension.path) else {
            continue;
        };
        let protocol = manifest.protocol.unwrap_or_else(|| "json".to_string());
        if protocol != "json" {
            continue;
        }
        for tool in manifest.tools {
            let name = tool.name.trim();
            if name.is_empty() {
                continue;
            }
            tools.insert(
                name.to_string(),
                ExtensionTool {
                    name: name.to_string(),
                    description: tool
                        .description
                        .unwrap_or_else(|| format!("Run extension tool {name}.")),
                    parameters: tool.parameters.unwrap_or_else(|| {
                        serde_json::json!({
                            "type": "object",
                            "properties": {},
                            "required": []
                        })
                    }),
                    extension_name: extension.name.clone(),
                    extension_path: extension.path.clone(),
                    protocol: protocol.clone(),
                },
            );
        }
    }
    tools
}

fn read_extension_manifest(path: &Path) -> Option<ExtensionManifest> {
    extension_manifest_paths(path).into_iter().find_map(|path| {
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str::<ExtensionManifest>(&content).ok()
    })
}

fn extension_manifest_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(file_name) = path.file_name() {
        paths.push(
            path.with_file_name(format!("{}.pi-extension.json", file_name.to_string_lossy())),
        );
        paths.push(path.with_file_name(format!("{}.json", file_name.to_string_lossy())));
    }
    if path.extension().is_some() {
        paths.push(path.with_extension("json"));
    }
    paths
}

async fn execute_model_tool_call(
    runtime: &mut Runtime,
    tool_call: &ChatToolCall,
) -> Result<(), AgentError> {
    let extension_tool = runtime
        .systems
        .extension_tools
        .get(&tool_call.name)
        .cloned();
    let outputs = if let Some(extension_tool) = extension_tool {
        match execute_extension_tool(&runtime.session.cwd, &extension_tool, tool_call).await {
            Ok(output) => vec![output],
            Err(error) => vec![error.to_string()],
        }
    } else {
        match model_tool_requests(tool_call) {
            Ok(requests) if model_tool_enabled(runtime, &tool_call.name) => {
                let mut outputs = Vec::new();
                for request in requests {
                    let output = match execute_tool(
                        &runtime.session.cwd,
                        request,
                        &ToolRuntimeOptions {
                            shell_path: runtime.systems.shell_path.clone(),
                            shell_command_prefix: runtime.systems.shell_command_prefix.clone(),
                        },
                    )
                    .await
                    {
                        Ok(result) => result.output,
                        Err(error) => error.to_string(),
                    };
                    outputs.push(output);
                }
                outputs
            }
            Ok(_) => vec![format!("Tool {} not found", tool_call.name)],
            Err(error) => vec![error.to_string()],
        }
    };
    let output = outputs.join("\n");
    runtime.push_tool_event(ToolEvent {
        id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        result: output.clone(),
    })?;
    runtime.push_message(ConversationMessage {
        role: MessageRole::Tool,
        content: output,
        media: Vec::new(),
        tool_call_id: Some(tool_call.id.clone()),
        tool_name: Some(tool_call.name.clone()),
        tool_calls: Vec::new(),
    })?;
    Ok(())
}

async fn execute_extension_tool(
    cwd: &Path,
    extension: &ExtensionTool,
    tool_call: &ChatToolCall,
) -> Result<String, AgentError> {
    let cwd_string = cwd.display().to_string();
    let request = ExtensionProtocolRequest {
        protocol_version: 1,
        kind: "tool",
        command: &tool_call.name,
        input: &tool_call.arguments,
        cwd: &cwd_string,
        tool_call_id: Some(&tool_call.id),
    };
    let mut child = Command::new(&extension.extension_path)
        .current_dir(cwd)
        .env("PI_EXTENSION_NAME", &extension.extension_name)
        .env("PI_EXTENSION_PATH", &extension.extension_path)
        .env("PI_EXTENSION_PROTOCOL", &extension.protocol)
        .env("PI_EXTENSION_TOOL", &extension.name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| {
            AgentError::Tool(ToolError::Io {
                path: extension.extension_path.clone(),
                source,
            })
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = format!(
            "{}\n",
            serde_json::to_string(&request).map_err(|source| AgentError::Session(
                SessionError::Parse {
                    path: extension.extension_path.clone(),
                    source,
                }
            ))?
        );
        stdin.write_all(input.as_bytes()).await.map_err(|source| {
            AgentError::Tool(ToolError::Io {
                path: extension.extension_path.clone(),
                source,
            })
        })?;
    }
    let output = child.wait_with_output().await.map_err(|source| {
        AgentError::Tool(ToolError::Io {
            path: extension.extension_path.clone(),
            source,
        })
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Ok(format!(
            "extension {} failed:\n{}{}",
            extension.extension_name, stdout, stderr
        ));
    }
    let response =
        serde_json::from_str::<ExtensionCommandResponse>(stdout.trim()).map_err(|source| {
            AgentError::Session(SessionError::Parse {
                path: extension.extension_path.clone(),
                source,
            })
        })?;
    if let Some(error) = response.error {
        return Ok(format!(
            "extension {} failed: {error}",
            extension.extension_name
        ));
    }
    Ok(response.output.unwrap_or_else(|| {
        format!(
            "extension {} tool {} completed",
            extension.extension_name, extension.name
        )
    }))
}

fn model_tool_requests(tool_call: &ChatToolCall) -> Result<Vec<ToolRequest>, AgentError> {
    let args = parse_tool_arguments(tool_call)?;
    let requests = match tool_call.name.as_str() {
        "read" => vec![ToolRequest::Read {
            path: string_tool_arg(&args, &["path"], &tool_call.name)?,
        }],
        "bash" => vec![ToolRequest::Bash {
            command: string_tool_arg(&args, &["command"], &tool_call.name)?,
            timeout_ms: optional_number_tool_arg(&args, &["timeout", "timeout_ms"])
                .map(|timeout| timeout.saturating_mul(1_000))
                .or(Some(120_000)),
        }],
        "write" => vec![ToolRequest::Write {
            path: string_tool_arg(&args, &["path"], &tool_call.name)?,
            content: string_tool_arg(&args, &["content"], &tool_call.name)?,
        }],
        "edit" => edit_tool_requests(&args, &tool_call.name)?,
        "grep" => vec![ToolRequest::Grep {
            path: optional_string_tool_arg(&args, &["path", "glob"]),
            pattern: string_tool_arg(&args, &["pattern"], &tool_call.name)?,
        }],
        "find" => vec![ToolRequest::Find {
            pattern: string_tool_arg(&args, &["pattern"], &tool_call.name)?,
        }],
        "ls" => vec![ToolRequest::Ls {
            path: optional_string_tool_arg(&args, &["path"]),
        }],
        _ => {
            return Err(AgentError::InvalidToolCall {
                tool: tool_call.name.clone(),
                message: "unknown tool".to_string(),
            });
        }
    };
    Ok(requests)
}

fn parse_tool_arguments(tool_call: &ChatToolCall) -> Result<serde_json::Value, AgentError> {
    if tool_call.arguments.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&tool_call.arguments).map_err(|source| AgentError::InvalidToolCall {
        tool: tool_call.name.clone(),
        message: source.to_string(),
    })
}

fn edit_tool_requests(
    args: &serde_json::Value,
    tool: &str,
) -> Result<Vec<ToolRequest>, AgentError> {
    let path = string_tool_arg(args, &["path"], tool)?;
    if let Some(edits) = args.get("edits").and_then(serde_json::Value::as_array) {
        if edits.is_empty() {
            return Err(AgentError::InvalidToolCall {
                tool: tool.to_string(),
                message: "edits must not be empty".to_string(),
            });
        }
        return edits
            .iter()
            .map(|edit| {
                Ok(ToolRequest::Edit {
                    path: path.clone(),
                    find: string_tool_arg(edit, &["oldText", "find"], tool)?,
                    replace: string_tool_arg(edit, &["newText", "replace"], tool)?,
                })
            })
            .collect();
    }
    Ok(vec![ToolRequest::Edit {
        path,
        find: string_tool_arg(args, &["oldText", "find"], tool)?,
        replace: string_tool_arg(args, &["newText", "replace"], tool)?,
    }])
}

fn string_tool_arg(
    args: &serde_json::Value,
    names: &[&str],
    tool: &str,
) -> Result<String, AgentError> {
    optional_string_tool_arg(args, names).ok_or_else(|| AgentError::InvalidToolCall {
        tool: tool.to_string(),
        message: format!("missing string argument {}", names[0]),
    })
}

fn optional_string_tool_arg(args: &serde_json::Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| args.get(*name).and_then(serde_json::Value::as_str))
        .find(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn optional_number_tool_arg(args: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| args.get(*name).and_then(serde_json::Value::as_u64))
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
        Arc, Mutex,
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
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
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
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
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
        state.labels = BTreeSet::from(["important".to_string()]);
        state.parent_session_id = Some("parent-session".to_string());
        state.active_model = Some(ModelRef {
            provider: "anthropic".to_string(),
            id: "claude".to_string(),
        });
        state.active_thinking_level = Some("xhigh".to_string());
        state.active_tool_names = BTreeSet::from(["read".to_string()]);
        state.queued_messages = vec!["next prompt".to_string()];
        state.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: "hello <world>".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
        state.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            content: "done".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
        state.tool_history.push(ToolEvent {
            id: "tool-1".to_string(),
            name: "read".to_string(),
            result: "content".to_string(),
        });
        state.compactions.push(CompactionRecord {
            kind: CompactionKind::Manual,
            omitted_messages: 1,
            retained_messages: 1,
            summary: "compacted".to_string(),
        });
        state.branch_summaries.push(BranchSummary {
            from_session_id: state.session_id.clone(),
            to_session_id: "branch-session".to_string(),
            summary: "branched".to_string(),
        });
        store.record_metadata(&state).expect("record metadata");
        store
            .record_active_model(state.active_model.clone())
            .expect("record model");
        store
            .record_active_thinking_level(state.active_thinking_level.clone())
            .expect("record thinking");
        store
            .record_active_tools(state.active_tool_names.iter().cloned().collect())
            .expect("record tools");
        store
            .record_message(state.messages[0].clone())
            .expect("record user");
        store
            .record_message(state.messages[1].clone())
            .expect("record assistant");
        store
            .record_tool(state.tool_history[0].clone())
            .expect("record tool");
        store
            .record_queued_message(state.queued_messages[0].clone())
            .expect("record queued");
        store
            .record_compaction(state.compactions[0].clone())
            .expect("record compaction");
        store
            .record_branch_summary(state.branch_summaries[0].clone())
            .expect("record branch summary");

        let json = export_dir.join("session.json");
        let jsonl = export_dir.join("session.jsonl");
        let html = export_dir.join("session.html");
        store.export_state(&state, &json).expect("export json");
        store.export_state(&state, &jsonl).expect("export jsonl");
        store.export_state(&state, &html).expect("export html");

        let json_content = fs::read_to_string(&json).expect("read json");
        let exported =
            serde_json::from_str::<serde_json::Value>(&json_content).expect("parse json export");
        assert_eq!(exported["name"], "exported");
        assert_eq!(exported["parent_session_id"], "parent-session");
        assert_eq!(exported["active_thinking_level"], "xhigh");
        assert_eq!(exported["active_model"]["provider"], "anthropic");
        assert_eq!(exported["queued_messages"][0], "next prompt");

        let jsonl_content = fs::read_to_string(&jsonl).expect("read jsonl");
        assert!(jsonl_content.contains("\"type\":\"message\""));
        let jsonl_records = jsonl_content
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse jsonl line"))
            .collect::<Vec<_>>();
        let jsonl_types = jsonl_records
            .iter()
            .map(|record| record["type"].as_str().expect("record type"))
            .collect::<Vec<_>>();
        assert_eq!(jsonl_types.first(), Some(&"session"));
        assert_eq!(jsonl_records[0]["version"], 3);
        assert_eq!(jsonl_records[0]["cwd"], "/repo");
        let timestamp = jsonl_records[0]["timestamp"]
            .as_str()
            .expect("session timestamp");
        assert_ne!(timestamp, "1970-01-01T00:00:00.000Z");
        assert!(timestamp.contains('T'));
        for record in jsonl_records.iter().skip(1) {
            assert_eq!(record["timestamp"], timestamp);
        }
        for expected_type in [
            "custom",
            "session_info",
            "model_change",
            "thinking_level_change",
            "message",
            "label",
            "compaction",
            "branch_summary",
        ] {
            assert!(
                jsonl_types.contains(&expected_type),
                "missing jsonl record type {expected_type}"
            );
        }
        assert_eq!(jsonl_records[1]["parentId"], serde_json::Value::Null);
        assert_eq!(jsonl_records[2]["parentId"], "e1");
        let html_content = fs::read_to_string(&html).expect("read html");
        assert!(html_content.contains("<!doctype html>"));
        assert!(html_content.contains("hello &lt;world&gt;"));
        assert!(html_content.contains("tool history"));

        let import_dir = base.join("imported");
        let (_import_store, imported) =
            SessionStore::import_path(&import_dir, &jsonl).expect("import jsonl");
        assert_eq!(imported.session_id, state.session_id);
        assert_eq!(imported.messages, state.messages);
        assert_eq!(imported.tool_history, state.tool_history);
        assert_eq!(imported.active_thinking_level, state.active_thinking_level);
        assert_eq!(imported.labels, state.labels);

        let (_opened_store, opened) =
            SessionStore::open(jsonl.clone()).expect("open ts-style jsonl directly");
        assert_eq!(opened.session_id, state.session_id);
        assert_eq!(opened.messages, state.messages);
        assert_eq!(opened.active_model, state.active_model);
        assert_eq!(opened.active_tool_names, state.active_tool_names);
        assert_eq!(opened.labels, state.labels);

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn upstream_session_export_fixture_documents_ts_session_surface() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/session-export.json"
        ))
        .expect("parse session export fixture");

        assert_eq!(fixture["header"]["type"], "session");
        assert_eq!(fixture["header"]["version"], 3);
        assert_eq!(fixture["treeRootCount"], 1);
        assert_eq!(fixture["sessionName"], "demo session");
        assert_eq!(fixture["labelForFirstMessage"], "important");
        let entry_types = fixture["entryTypes"]
            .as_array()
            .expect("entry types")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<BTreeSet<_>>();
        for expected_type in [
            "message",
            "session_info",
            "model_change",
            "thinking_level_change",
            "custom",
            "compaction",
            "label",
        ] {
            assert!(
                entry_types.contains(expected_type),
                "missing upstream session entry type {expected_type}"
            );
        }
        assert_eq!(fixture["jsonlBranchExport"]["recordTypes"][0], "session");
        assert_eq!(fixture["jsonlBranchExport"]["firstRecordVersion"], 3);
        assert_eq!(
            fixture["jsonlBranchExport"]["parentChain"],
            serde_json::json!([null, "set", "set"])
        );

        let base = std::env::temp_dir().join(format!(
            "pi-ts-session-active-branch-test-{}",
            new_session_id()
        ));
        fs::create_dir_all(&base).expect("create branch fixture dir");
        let path = base.join("ts-branch.jsonl");
        let records = fixture["fullTreeJsonlExport"]["records"]
            .as_array()
            .expect("full tree records");
        let content = records
            .iter()
            .map(|record| serde_json::to_string(record).expect("serialize record"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, format!("{content}\n")).expect("write branch fixture");

        let (_store, opened) = SessionStore::open(path).expect("open TS active branch");
        let messages = opened
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            fixture["fullTreeJsonlExport"]["activeMessageTexts"]
                .as_array()
                .expect("active texts")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
        );
        for text in fixture["fullTreeJsonlExport"]["abandonedMessageTexts"]
            .as_array()
            .expect("abandoned texts")
            .iter()
            .filter_map(serde_json::Value::as_str)
        {
            assert!(
                !messages.contains(&text),
                "abandoned TS branch message should not load: {text}"
            );
        }
        assert_eq!(opened.labels, BTreeSet::from(["important".to_string()]));

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn upstream_agent_tool_loop_fixture_documents_model_callable_tools() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/agent-tool-loop.json"
        ))
        .expect("parse agent tool loop fixture");
        let tool_loop = &fixture["toolLoop"];

        assert_eq!(
            tool_loop["streamCalls"]
                .as_array()
                .expect("stream calls")
                .len(),
            2
        );
        assert_eq!(
            tool_loop["streamCalls"][0]["toolNames"],
            serde_json::json!(["fixture_echo"])
        );
        assert_eq!(
            tool_loop["streamCalls"][1]["lastRole"],
            serde_json::json!("toolResult")
        );
        assert!(tool_loop["events"]
            .as_array()
            .expect("events")
            .iter()
            .any(|event| event == "tool_execution_start"));
        assert!(tool_loop["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["toolResultFor"] == "fixture_echo"));
    }

    #[test]
    fn model_tool_definitions_match_ts_local_tool_schema_keys() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/local-tools.json"
        ))
        .expect("parse local tools fixture");
        let expected = fixture["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| {
                (
                    tool["name"].as_str().expect("tool name").to_string(),
                    (
                        tool["parameters"]["properties"]
                            .as_array()
                            .expect("properties")
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                        tool["parameters"]["required"]
                            .as_array()
                            .expect("required")
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let actual = builtin_tool_definitions()
            .into_iter()
            .map(|tool| {
                let definition = model_tool_definition(&tool.name).expect("model tool definition");
                let mut properties = definition.parameters["properties"]
                    .as_object()
                    .expect("properties object")
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                properties.sort();
                let mut required = definition.parameters["required"]
                    .as_array()
                    .expect("required array")
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                required.sort();
                (definition.name, (properties, required))
            })
            .collect::<BTreeMap<_, _>>();

        assert_eq!(actual, expected);
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
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
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
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
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
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            })
            .expect("record message");
        store
            .record_messages_snapshot(vec![ConversationMessage {
                role: MessageRole::System,
                content: "summary".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
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
            session_id: None,
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
            session_id: None,
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
    async fn run_user_turn_emits_provider_events_before_completion() {
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        let saw_delta = Arc::new(AtomicBool::new(false));
        let provider = ObservingStreamingProvider {
            saw_delta: Arc::clone(&saw_delta),
        };

        let response =
            run_user_turn_streaming(&mut runtime, &provider, "hello".to_string(), |delta| {
                if delta == "early" {
                    saw_delta.store(true, AtomicOrdering::SeqCst);
                }
            })
            .await
            .expect("run streaming turn");

        assert_eq!(response, "early done");
        assert_eq!(runtime.session().messages[1].content, response);
    }

    #[tokio::test]
    async fn run_user_turn_executes_model_tool_calls_and_continues() {
        let cwd =
            std::env::temp_dir().join(format!("pi-model-tool-loop-test-{}", new_session_id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
        let mut runtime = Runtime::new(
            SessionState::new("session-1", cwd.clone()),
            ReloadableSystems::default(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = ToolLoopProvider {
            requests: requests.clone(),
        };

        let response = run_user_turn(&mut runtime, &provider, "read the file".to_string())
            .await
            .expect("run tool loop");

        assert_eq!(response, "done");
        assert_eq!(runtime.session().tool_history.len(), 1);
        assert_eq!(runtime.session().tool_history[0].id, "call_read_1");
        assert_eq!(runtime.session().tool_history[0].name, "read");
        assert_eq!(runtime.session().tool_history[0].result, "file contents");
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(runtime.session().messages[1].tool_calls[0].name, "read");
        assert_eq!(
            runtime.session().messages[2].tool_call_id.as_deref(),
            Some("call_read_1")
        );

        let captured = requests.lock().expect("requests").clone();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].tools.iter().any(|tool| tool.name == "read"));
        assert_eq!(captured[1].messages[1].tool_calls[0].id, "call_read_1");
        assert_eq!(captured[1].messages[2].role, ChatRole::Tool);
        assert_eq!(
            captured[1].messages[2].tool_call_id.as_deref(),
            Some("call_read_1")
        );

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn unavailable_model_tool_call_returns_tool_error_without_execution() {
        let cwd = std::env::temp_dir().join(format!(
            "pi-unavailable-model-tool-test-{}",
            new_session_id()
        ));
        fs::create_dir_all(&cwd).expect("create temp dir");
        fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
        let mut runtime = Runtime::new(
            SessionState::new("session-1", cwd.clone()),
            ReloadableSystems {
                available_tool_names: BTreeSet::from(["bash".to_string()]),
                ..ReloadableSystems::default()
            },
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = ToolLoopProvider {
            requests: requests.clone(),
        };

        let response = run_user_turn(&mut runtime, &provider, "read the file".to_string())
            .await
            .expect("run tool loop");

        assert_eq!(response, "done");
        assert_eq!(
            runtime.session().tool_history[0].result,
            "Tool read not found"
        );
        assert_eq!(runtime.session().messages[2].content, "Tool read not found");
        let captured = requests.lock().expect("requests").clone();
        assert!(!captured[0].tools.iter().any(|tool| tool.name == "read"));
        assert!(captured[0].tools.iter().any(|tool| tool.name == "bash"));

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn failing_model_tool_call_is_recorded_as_tool_output() {
        let cwd =
            std::env::temp_dir().join(format!("pi-failing-model-tool-test-{}", new_session_id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let mut runtime = Runtime::new(
            SessionState::new("session-1", cwd.clone()),
            ReloadableSystems::default(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = FailingToolLoopProvider {
            requests: requests.clone(),
        };

        let response = run_user_turn(&mut runtime, &provider, "read outside".to_string())
            .await
            .expect("run tool loop");

        assert_eq!(response, "done");
        assert_eq!(runtime.session().tool_history.len(), 1);
        assert_eq!(runtime.session().tool_history[0].id, "call_read_bad");
        assert_eq!(runtime.session().tool_history[0].name, "read");
        assert_eq!(
            runtime.session().tool_history[0].result,
            "path escapes cwd: ../outside.txt"
        );
        assert_eq!(
            runtime.session().messages[2].tool_call_id.as_deref(),
            Some("call_read_bad")
        );
        assert_eq!(
            runtime.session().messages[2].content,
            "path escapes cwd: ../outside.txt"
        );

        let captured = requests.lock().expect("requests").clone();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[1].messages[2].role, ChatRole::Tool);
        assert_eq!(
            captured[1].messages[2].tool_call_id.as_deref(),
            Some("call_read_bad")
        );

        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn provider_messages_repair_missing_tool_outputs_from_saved_sessions() {
        let messages = vec![
            ConversationMessage {
                role: MessageRole::User,
                content: "question".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            },
            ConversationMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_missing".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"."}"#.to_string(),
                }],
            },
            ConversationMessage {
                role: MessageRole::User,
                content: "continue".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            },
        ];

        let request_messages = provider_messages(&messages);

        assert_eq!(request_messages.len(), 4);
        assert_eq!(request_messages[1].role, ChatRole::Assistant);
        assert_eq!(request_messages[2].role, ChatRole::Tool);
        assert_eq!(
            request_messages[2].tool_call_id.as_deref(),
            Some("call_missing")
        );
        assert!(request_messages[2].content.contains("did not complete"));
        assert_eq!(request_messages[3].content, "continue");
    }

    #[tokio::test]
    async fn run_user_turn_executes_json_extension_model_tools() {
        let cwd = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join(format!("pi-extension-tool-loop-test-{}", new_session_id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let extension_path = cwd.join("ext-tool.sh");
        fs::write(
            &extension_path,
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"output\":\"extension output\"}\\n'\n",
        )
        .expect("write extension");
        make_executable(&extension_path);
        fs::write(
            cwd.join("ext-tool.sh.pi-extension.json"),
            serde_json::json!({
                "protocol": "json",
                "tools": [{
                    "name": "ext_echo",
                    "description": "Echo through an extension.",
                    "parameters": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }]
            })
            .to_string(),
        )
        .expect("write manifest");
        let extensions = vec![ResourceFile {
            name: "ext-tool".to_string(),
            path: extension_path,
            content: String::new(),
        }];
        let extension_tools = extension_tools_from_resources(&extensions);
        let mut runtime = Runtime::new(
            SessionState::new("session-1", cwd.clone()),
            ReloadableSystems {
                available_tool_names: BTreeSet::from(["ext_echo".to_string()]),
                extension_tools,
                ..ReloadableSystems::default()
            },
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = ExtensionToolLoopProvider {
            requests: Arc::clone(&requests),
        };

        let response = run_user_turn(&mut runtime, &provider, "use extension".to_string())
            .await
            .expect("run extension tool loop");

        assert_eq!(response, "done");
        assert_eq!(runtime.session().tool_history[0].name, "ext_echo");
        assert_eq!(runtime.session().tool_history[0].result, "extension output");
        assert_eq!(runtime.session().messages[2].content, "extension output");
        let captured = requests.lock().expect("requests");
        assert!(captured[0].tools.iter().any(|tool| tool.name == "ext_echo"));
        assert_eq!(
            captured[1].messages[2].tool_name.as_deref(),
            Some("ext_echo")
        );

        let _ = fs::remove_dir_all(cwd);
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
            session_id: None,
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

    struct ToolLoopProvider {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    struct FailingToolLoopProvider {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    struct ExtensionToolLoopProvider {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    struct ObservingStreamingProvider {
        saw_delta: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Provider for ObservingStreamingProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            unreachable!("streaming provider should use complete_streaming")
        }

        async fn complete_streaming(
            &self,
            _request: ProviderRequest,
            on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
        ) -> Result<(), ProviderError> {
            on_event(StreamEvent::Text("early".to_string()))?;
            assert!(
                self.saw_delta.load(AtomicOrdering::SeqCst),
                "text callback was not invoked before provider completion"
            );
            on_event(StreamEvent::Text(" done".to_string()))?;
            on_event(StreamEvent::Stop {
                reason: "stop".to_string(),
            })?;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Provider for ToolLoopProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut requests = self.requests.lock().expect("requests");
            let call_index = requests.len();
            requests.push(request);
            if call_index == 0 {
                return Ok(vec![
                    StreamEvent::ToolCall {
                        id: "call_read_1".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"a.txt"}"#.to_string(),
                    },
                    StreamEvent::Stop {
                        reason: "toolUse".to_string(),
                    },
                ]);
            }
            Ok(vec![
                StreamEvent::Text("done".to_string()),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ])
        }
    }

    #[async_trait::async_trait]
    impl Provider for FailingToolLoopProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut requests = self.requests.lock().expect("requests");
            let call_index = requests.len();
            requests.push(request);
            if call_index == 0 {
                return Ok(vec![
                    StreamEvent::ToolCall {
                        id: "call_read_bad".to_string(),
                        name: "read".to_string(),
                        arguments: r#"{"path":"../outside.txt"}"#.to_string(),
                    },
                    StreamEvent::Stop {
                        reason: "toolUse".to_string(),
                    },
                ]);
            }
            Ok(vec![
                StreamEvent::Text("done".to_string()),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ])
        }
    }

    #[async_trait::async_trait]
    impl Provider for ExtensionToolLoopProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut requests = self.requests.lock().expect("requests");
            let call_index = requests.len();
            requests.push(request);
            if call_index == 0 {
                return Ok(vec![
                    StreamEvent::ToolCall {
                        id: "call_ext_1".to_string(),
                        name: "ext_echo".to_string(),
                        arguments: r#"{"text":"hello"}"#.to_string(),
                    },
                    StreamEvent::Stop {
                        reason: "toolUse".to_string(),
                    },
                ]);
            }
            Ok(vec![
                StreamEvent::Text("done".to_string()),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ])
        }
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod extension");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

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

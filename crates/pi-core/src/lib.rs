use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pi_ai::{
    ChatMessage, ChatRole, ModelRef, Provider, ProviderError, ProviderRequest, StreamEvent,
};
use pi_config::{api_key_for_provider, LoadedConfig};
use pi_tools::{
    builtin_tool_definitions, execute_tool, ToolError, ToolRequest, ToolRuntimeOptions,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
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
    pub messages: Vec<ConversationMessage>,
    pub tool_history: Vec<ToolEvent>,
    pub queued_messages: Vec<String>,
    pub active_model: Option<ModelRef>,
    pub active_tool_names: BTreeSet<String>,
}

impl SessionState {
    pub fn new(session_id: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            session_id: session_id.into(),
            cwd,
            messages: Vec::new(),
            tool_history: Vec::new(),
            queued_messages: Vec::new(),
            active_model: None,
            active_tool_names: builtin_tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect(),
        }
    }
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
                model.provider == "faux"
                    || api_key_for_provider(&config.auth, &model.provider).is_some()
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
    Started { session_id: String, cwd: PathBuf },
    Message { message: ConversationMessage },
    Tool { event: ToolEvent },
    ActiveModel { model: Option<ModelRef> },
    ActiveTools { tools: Vec<String> },
    QueuedMessage { message: String },
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

    pub fn record_tool(&self, event: ToolEvent) -> Result<(), SessionError> {
        self.append(&SessionRecord::Tool { event })
    }

    pub fn record_active_model(&self, model: Option<ModelRef>) -> Result<(), SessionError> {
        self.append(&SessionRecord::ActiveModel { model })
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
                SessionRecord::Message { message } => {
                    if let Some(state) = &mut state {
                        state.messages.push(message);
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
            }
        }
        Ok(state.unwrap_or_else(|| SessionState::new("recovered", PathBuf::from("."))))
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
    runtime.push_message(ConversationMessage {
        role: MessageRole::User,
        content: prompt.clone(),
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
                },
                MessageRole::User => ChatMessage {
                    role: ChatRole::User,
                    content: message.content.clone(),
                },
                MessageRole::Assistant => ChatMessage {
                    role: ChatRole::Assistant,
                    content: message.content.clone(),
                },
                MessageRole::Tool => ChatMessage {
                    role: ChatRole::Tool,
                    content: message.content.clone(),
                },
            })
            .collect(),
    };
    let events = provider.complete(request).await?;
    let mut text = String::new();
    for event in events {
        if let StreamEvent::Text(delta) = event {
            text.push_str(&delta);
        }
    }
    runtime.push_message(ConversationMessage {
        role: MessageRole::Assistant,
        content: text.clone(),
    })?;
    Ok(text)
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

    use pi_ai::{create_provider, ProviderApi, ProviderConfig};

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
        });
        store
            .record_message(state.messages[0].clone())
            .expect("record message");

        let (_store, loaded) =
            SessionStore::open(store.path().to_path_buf()).expect("open session");

        assert_eq!(loaded.cwd, PathBuf::from("/repo"));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");

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
            api_key: None,
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
            api_key: None,
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
}

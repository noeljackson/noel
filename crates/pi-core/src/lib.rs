use std::collections::BTreeSet;
use std::path::PathBuf;

use pi_ai::ModelRef;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub id: String,
    pub name: String,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
            active_tool_names: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadableSystems {
    pub config_generation: u64,
    pub system_prompt: Option<String>,
    pub available_models: Vec<ModelRef>,
    pub available_tool_names: BTreeSet<String>,
    pub keybinding_generation: u64,
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

#[derive(Debug, Clone)]
pub struct Runtime {
    session: SessionState,
    systems: ReloadableSystems,
}

impl Runtime {
    pub fn new(session: SessionState, systems: ReloadableSystems) -> Self {
        Self { session, systems }
    }

    pub fn session(&self) -> &SessionState {
        &self.session
    }

    pub fn systems(&self) -> &ReloadableSystems {
        &self.systems
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
}

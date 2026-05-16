use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybinding {
    pub action: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingMap {
    pub bindings: Vec<Keybinding>,
}

impl KeybindingMap {
    pub fn with_overrides(overrides: Vec<Keybinding>) -> Self {
        let mut bindings = default_keybindings().bindings;
        for override_binding in overrides {
            if let Some(existing) = bindings
                .iter_mut()
                .find(|binding| binding.action == override_binding.action)
            {
                existing.keys = override_binding.keys;
            } else {
                bindings.push(override_binding);
            }
        }
        bindings.sort_by(|left, right| left.action.cmp(&right.action));
        Self { bindings }
    }

    pub fn keys_for(&self, action: &str) -> Option<&[String]> {
        self.bindings
            .iter()
            .find(|binding| binding.action == action)
            .map(|binding| binding.keys.as_slice())
    }

    pub fn matches(&self, action: &str, key: &str) -> bool {
        self.keys_for(action)
            .map(|keys| keys.iter().any(|candidate| candidate == key))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTheme {
    pub name: String,
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub labels: Vec<String>,
    pub parent: Option<String>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelView {
    pub provider: String,
    pub id: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsView {
    pub agent_dir: String,
    pub session_dir: String,
    pub config_generation: u64,
    pub active_model: Option<String>,
    pub theme: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHelp {
    pub command: &'static str,
    pub description: &'static str,
}

pub const COMMAND_HELP: &[CommandHelp] = &[
    CommandHelp {
        command: "/help",
        description: "show commands",
    },
    CommandHelp {
        command: "/settings",
        description: "show active settings",
    },
    CommandHelp {
        command: "/diagnostics",
        description: "show resource reload diagnostics",
    },
    CommandHelp {
        command: "/hotkeys",
        description: "show keybindings",
    },
    CommandHelp {
        command: "/skills",
        description: "list loaded skills",
    },
    CommandHelp {
        command: "/skill:<name> [input]",
        description: "invoke a loaded skill",
    },
    CommandHelp {
        command: "/prompts",
        description: "list prompt templates",
    },
    CommandHelp {
        command: "/prompt <name> [input]",
        description: "invoke a prompt template",
    },
    CommandHelp {
        command: "/themes",
        description: "list themes",
    },
    CommandHelp {
        command: "/queue [prompt]",
        description: "list or queue a follow-up prompt",
    },
    CommandHelp {
        command: "/queue-clear",
        description: "clear queued follow-ups",
    },
    CommandHelp {
        command: "/interrupt",
        description: "cancel queued follow-ups",
    },
    CommandHelp {
        command: "/theme <name>",
        description: "switch theme",
    },
    CommandHelp {
        command: "/models",
        description: "list configured models",
    },
    CommandHelp {
        command: "/scoped-models",
        description: "list scoped models",
    },
    CommandHelp {
        command: "/model <provider/id>",
        description: "switch model by id or number",
    },
    CommandHelp {
        command: "/multiline",
        description: "enter a multiline prompt",
    },
    CommandHelp {
        command: "/session",
        description: "show session info",
    },
    CommandHelp {
        command: "/new",
        description: "start a new session",
    },
    CommandHelp {
        command: "/resume [id|name|path]",
        description: "resume or list sessions",
    },
    CommandHelp {
        command: "/fork [id|name|path]",
        description: "fork a session",
    },
    CommandHelp {
        command: "/clone [id|name|path]",
        description: "clone a session without changing parent",
    },
    CommandHelp {
        command: "/tree",
        description: "list session tree",
    },
    CommandHelp {
        command: "/summaries",
        description: "show compaction and branch summaries",
    },
    CommandHelp {
        command: "/delete [id|name|path]",
        description: "delete a session",
    },
    CommandHelp {
        command: "/name <name>",
        description: "rename current session",
    },
    CommandHelp {
        command: "/labels <labels...>",
        description: "replace current labels",
    },
    CommandHelp {
        command: "/export <file>",
        description: "export current session",
    },
    CommandHelp {
        command: "/import <file>",
        description: "import a session export",
    },
    CommandHelp {
        command: "/copy",
        description: "copy last assistant message",
    },
    CommandHelp {
        command: "/share [file]",
        description: "export a local HTML session share",
    },
    CommandHelp {
        command: "/compact",
        description: "compact older messages with a summary",
    },
    CommandHelp {
        command: "/login [provider]",
        description: "show auth status",
    },
    CommandHelp {
        command: "/logout <provider>",
        description: "remove stored auth",
    },
    CommandHelp {
        command: "/reload",
        description: "reload config without clearing context",
    },
    CommandHelp {
        command: "/read <path>",
        description: "read file",
    },
    CommandHelp {
        command: "/write <path> <text>",
        description: "write file",
    },
    CommandHelp {
        command: "/edit <path> <a> <b>",
        description: "replace text",
    },
    CommandHelp {
        command: "/grep <text> [path]",
        description: "search files",
    },
    CommandHelp {
        command: "/find <text>",
        description: "find files by substring",
    },
    CommandHelp {
        command: "/ls [path]",
        description: "list directory",
    },
    CommandHelp {
        command: "/bash <command>",
        description: "run shell command",
    },
    CommandHelp {
        command: "! <command>",
        description: "run shell command outside context",
    },
    CommandHelp {
        command: "!!",
        description: "repeat last outside-context shell command",
    },
    CommandHelp {
        command: "/quit",
        description: "exit",
    },
];

#[derive(Debug, Clone, Default)]
pub struct TerminalRenderer {
    theme: TerminalTheme,
}

impl TerminalRenderer {
    pub fn new(theme: TerminalTheme) -> Self {
        Self { theme }
    }

    pub fn banner(&self) -> String {
        format!("pi rust cli ({})", self.theme.name)
    }

    pub fn prompt(&self) -> &'static str {
        "pi> "
    }

    pub fn help(&self) -> String {
        COMMAND_HELP
            .iter()
            .map(|entry| format!("{:<24} {}", entry.command, entry.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn keybindings(&self, bindings: &KeybindingMap) -> String {
        bindings
            .bindings
            .iter()
            .map(|binding| format!("{}\t{}", binding.action, binding.keys.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn session(&self, session: &SessionView) -> String {
        let mut lines = vec![
            format!("session: {}", session.id),
            format!("cwd: {}", session.cwd),
        ];
        if let Some(name) = &session.name {
            lines.push(format!("name: {name}"));
        }
        if !session.labels.is_empty() {
            lines.push(format!("labels: {}", session.labels.join(", ")));
        }
        if let Some(parent) = &session.parent {
            lines.push(format!("parent: {parent}"));
        }
        if let Some(file) = &session.file {
            lines.push(format!("file: {file}"));
        }
        lines.join("\n")
    }

    pub fn models(&self, models: &[ModelView]) -> String {
        models
            .iter()
            .enumerate()
            .map(|(index, model)| {
                let marker = if model.active { "*" } else { " " };
                format!("{:>2}. {marker} {}/{}", index + 1, model.provider, model.id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn settings(&self, settings: &SettingsView) -> String {
        [
            format!("agent dir: {}", settings.agent_dir),
            format!("session dir: {}", settings.session_dir),
            format!("config generation: {}", settings.config_generation),
            format!(
                "active model: {}",
                settings
                    .active_model
                    .clone()
                    .unwrap_or_else(|| "-".to_string())
            ),
            format!(
                "theme: {}",
                settings.theme.clone().unwrap_or_else(|| "-".to_string())
            ),
        ]
        .join("\n")
    }
}

pub fn default_keybindings() -> KeybindingMap {
    KeybindingMap {
        bindings: vec![
            Keybinding {
                action: "submit".to_string(),
                keys: vec!["enter".to_string()],
            },
            Keybinding {
                action: "cancel".to_string(),
                keys: vec!["escape".to_string()],
            },
            Keybinding {
                action: "interrupt".to_string(),
                keys: vec!["ctrl+c".to_string()],
            },
            Keybinding {
                action: "reload".to_string(),
                keys: vec!["ctrl+r".to_string()],
            },
            Keybinding {
                action: "model".to_string(),
                keys: vec!["ctrl+m".to_string()],
            },
            Keybinding {
                action: "session".to_string(),
                keys: vec!["ctrl+s".to_string()],
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybinding_overrides_replace_defaults() {
        let bindings = KeybindingMap::with_overrides(vec![Keybinding {
            action: "submit".to_string(),
            keys: vec!["ctrl+j".to_string()],
        }]);

        assert!(bindings.matches("submit", "ctrl+j"));
        assert!(!bindings.matches("submit", "enter"));
        assert!(bindings.matches("reload", "ctrl+r"));
    }

    #[test]
    fn renderer_formats_session_and_help() {
        let renderer = TerminalRenderer::default();
        let session = renderer.session(&SessionView {
            id: "s1".to_string(),
            cwd: "/repo".to_string(),
            name: Some("work".to_string()),
            labels: vec!["a".to_string(), "b".to_string()],
            parent: Some("root".to_string()),
            file: Some("/sessions/s1.jsonl".to_string()),
        });

        assert!(session.contains("session: s1"));
        assert!(session.contains("labels: a, b"));
        assert!(renderer.help().contains("/reload"));
    }
}

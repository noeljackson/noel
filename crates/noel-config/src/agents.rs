//! Subagent definitions: a Markdown file whose frontmatter names the
//! agent, describes when to use it, and may scope its tools, model and
//! thinking level; the body is the subagent's system prompt. Files load
//! from the project's `agents/` directory and the user's
//! `<agent dir>/agents/`, project before user, the first of a name
//! winning, as skills load.

use std::path::{Path, PathBuf};

use crate::frontmatter::{parse_frontmatter, strip_frontmatter};
use crate::resources::{ResourceDiagnostic, SourceInfo, SourceScope};
use crate::{canonical_path, project_config_dir, ConfigPaths};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    /// The body of the file: the subagent's system prompt.
    pub system_prompt: String,
    /// The `tools` frontmatter: the built-in tools the subagent may call.
    /// `None` gives it the parent's active tools, without `task`.
    pub tools: Option<Vec<String>>,
    /// The `model` frontmatter, `provider/id`, overriding the session's.
    pub model: Option<String>,
    /// The `thinking` frontmatter, overriding the session's level.
    pub thinking: Option<String>,
    pub source_info: SourceInfo,
}

/// The `agents/` directories of `paths`: the project's, then the user's.
pub(crate) fn agent_dirs(paths: &ConfigPaths) -> Vec<(PathBuf, SourceScope)> {
    let mut dirs = Vec::new();
    let project = project_config_dir(&paths.cwd).join("agents");
    if project.is_dir() {
        dirs.push((project, SourceScope::Project));
    }
    let user = paths.agent_dir.join("agents");
    if user.is_dir() {
        dirs.push((user, SourceScope::User));
    }
    dirs
}

/// Loads every agent definition of `paths`, project before user, the first
/// of a name winning and each later one reported.
pub(crate) fn load_agents(paths: &ConfigPaths) -> (Vec<AgentDefinition>, Vec<ResourceDiagnostic>) {
    let mut agents: Vec<AgentDefinition> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut loaded_files = std::collections::BTreeSet::new();
    for (dir, scope) in agent_dirs(paths) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file() && path.extension().is_some_and(|extension| extension == "md")
            })
            .collect::<Vec<_>>();
        files.sort();
        for file in files {
            let source = SourceInfo::local(&file, scope, &dir);
            let Some(agent) = load_agent_file(&file, source, &mut diagnostics) else {
                continue;
            };
            let real_path = canonical_path(&agent.file_path);
            if loaded_files.contains(&real_path) {
                continue;
            }
            match agents.iter().find(|loaded| loaded.name == agent.name) {
                Some(winner) => diagnostics.push(ResourceDiagnostic::collision(
                    &agent.name,
                    &winner.file_path,
                    &agent.file_path,
                )),
                None => {
                    loaded_files.insert(real_path);
                    agents.push(agent);
                }
            }
        }
    }
    (agents, diagnostics)
}

/// Loads one agent file. Frontmatter without a description is a
/// diagnostic, not an agent: the description is how the model chooses
/// among them.
fn load_agent_file(
    path: &Path,
    source_info: SourceInfo,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Option<AgentDefinition> {
    let content = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::new(error.to_string(), path));
            return None;
        }
    };
    let (frontmatter, body) = match parse_frontmatter(&content) {
        Ok(parts) => parts,
        Err(message) => {
            diagnostics.push(ResourceDiagnostic::new(message, path));
            return None;
        }
    };
    let mut warn = |message: String| diagnostics.push(ResourceDiagnostic::new(message, path));
    let description = frontmatter
        .string("description")
        .map(str::to_owned)
        .unwrap_or_default();
    if description.trim().is_empty() {
        warn("description is required".into());
        return None;
    }
    if description.encode_utf16().count() > MAX_DESCRIPTION_LENGTH {
        warn(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters"
        ));
    }
    let file_stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = frontmatter.string("name").map(str::to_owned);
    let name = name.filter(|name| !name.trim().is_empty());
    let name = name.unwrap_or(file_stem);
    if name.encode_utf16().count() > MAX_NAME_LENGTH {
        warn(format!("name exceeds {MAX_NAME_LENGTH} characters"));
    }
    let system_prompt = strip_frontmatter(&content)
        .ok()
        .map(|body| body.trim().to_string())
        .unwrap_or_else(|| body.trim().to_string());
    if system_prompt.is_empty() {
        warn("agent file has no prompt body".into());
        return None;
    }
    let tools = frontmatter.string("tools").map(|tools| {
        tools
            .split(',')
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    let model = frontmatter
        .string("model")
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned);
    let thinking = frontmatter
        .string("thinking")
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(str::to_owned);
    Some(AgentDefinition {
        name,
        description,
        file_path: path.to_path_buf(),
        system_prompt,
        tools,
        model,
        thinking,
        source_info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(paths_dir: &Path) -> ConfigPaths {
        ConfigPaths {
            cwd: paths_dir.to_path_buf(),
            settings_path: paths_dir.join("settings.json"),
            project_settings_path: paths_dir.join(".noel/settings.json"),
            auth_path: paths_dir.join("auth.json"),
            models_path: paths_dir.join("models.json"),
            model_cache_path: paths_dir.join("model-cache.json"),
            keybindings_path: paths_dir.join("keybindings.json"),
            agent_dir: paths_dir.join(".noel-user"),
            session_dir: paths_dir.join("sessions"),
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn loads_frontmatter_and_body() {
        let root = std::env::temp_dir().join("noel-agents-load");
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join(".noel/agents/researcher.md"),
            "---\ndescription: Finds facts.\ntools: read, grep\nmodel: faux/echo\nthinking: high\n---\nYou research.",
        );
        let paths = config(&root);
        let (agents, diagnostics) = load_agents(&paths);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert_eq!(agent.name, "researcher");
        assert_eq!(agent.description, "Finds facts.");
        assert_eq!(agent.system_prompt, "You research.");
        assert_eq!(
            agent.tools,
            Some(vec!["read".to_string(), "grep".to_string()])
        );
        assert_eq!(agent.model.as_deref(), Some("faux/echo"));
        assert_eq!(agent.thinking.as_deref(), Some("high"));
    }

    #[test]
    fn project_wins_and_collisions_report() {
        let root = std::env::temp_dir().join("noel-agents-collide");
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join(".noel/agents/a.md"),
            "---\ndescription: Project.\n---\nProject body.",
        );
        write(
            &root.join(".noel-user/agents/a.md"),
            "---\ndescription: User.\n---\nUser body.",
        );
        let paths = config(&root);
        let (agents, diagnostics) = load_agents(&paths);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].description, "Project.");
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn missing_description_is_a_diagnostic() {
        let root = std::env::temp_dir().join("noel-agodes-desc");
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join(".noel/agents/bad.md"),
            "---\nname: bad\n---\nBody.",
        );
        let paths = config(&root);
        let (agents, diagnostics) = load_agents(&paths);
        assert!(agents.is_empty());
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn tools_frontmatter_parses_lists() {
        let root = std::env::temp_dir().join("noel-agents-tools");
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join(".noel/agents/t.md"),
            "---\ndescription: t.\ntools: read , ls ,grep\n---\nBody.",
        );
        let paths = config(&root);
        let (agents, _) = load_agents(&paths);
        assert_eq!(
            agents[0].tools,
            Some(vec!["read".into(), "ls".into(), "grep".into()])
        );
    }
}

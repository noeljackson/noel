//! The `task` tool: a subagent run inside a tool call. An agent definition
//! (`.noel/agents/` or `<agent dir>/agents/`, [`noel_config::AgentDefinition`])
//! names a system prompt and may scope the subagent's tools, model and
//! thinking level. The child runs its own provider rounds with its own
//! session state — its messages are not the parent's — and the parent
//! receives only the child's final reply, as the tool's result.
//!
//! A child never gets the `task` tool itself: subagents do not nest. An
//! abort of the parent run cancels the child (the shared
//! [`CancellationToken`]); what the child had done is kept, as for any
//! interrupted tool call.

use std::path::PathBuf;

use noel_ai::Provider;
use serde::Deserialize;

use crate::{
    run_user_turn_streaming_events_with_media, AgentError, RunControl, Runtime, SessionState,
};

/// The agents named in the `task` tool's description: `- name: description`
/// per line, empty when none is loaded (the tool is not offered then).
pub fn available_agents_description(agents: &[noel_config::AgentDefinition]) -> String {
    agents
        .iter()
        .map(|agent| format!("- {}: {}", agent.name, agent.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `provider/id`, as an agent definition's `model` frontmatter names one.
fn parse_model_reference(reference: &str) -> Option<noel_ai::ModelRef> {
    let (provider, id) = reference.split_once('/')?;
    let (provider, id) = (provider.trim(), id.trim());
    (!provider.is_empty() && !id.is_empty()).then(|| noel_ai::ModelRef {
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

#[derive(Debug, Deserialize)]
pub struct TaskToolArgs {
    pub subagent: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
}

/// What a running subagent reports as it goes, for the UI.
pub enum SubagentEvent<'a> {
    Started {
        agent: &'a str,
        description: &'a str,
    },
    /// The child produced a final reply.
    Finished { agent: &'a str, is_error: bool },
}

/// Runs the `task` tool's call: the subagent's own turn loop over the
/// parent's provider. The result is what the tool call reports.
pub async fn run_task_tool(
    runtime: &mut Runtime,
    parent_provider: &dyn Provider,
    run: &RunControl,
    args: &TaskToolArgs,
    on_event: &mut (dyn FnMut(&SubagentEvent<'_>) + Send),
) -> Result<String, AgentError> {
    let Some(definition) = runtime
        .systems
        .agents
        .iter()
        .find(|agent| agent.name == args.subagent)
        .cloned()
    else {
        return Ok(format!(
            "unknown subagent '{}'; the loaded agents are: {}",
            args.subagent,
            runtime
                .systems
                .agents
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let provider: Option<Box<dyn Provider>> =
        match &definition.model {
            Some(reference) => {
                let model = parse_model_reference(reference).ok_or_else(|| {
                    AgentError::InvalidToolCall {
                        tool: "task".to_string(),
                        message: format!(
                            "subagent '{}' names a model that is not provider/id: {reference}",
                            definition.name
                        ),
                    }
                })?;
                match &runtime.subagent_provider_factory {
                    Some(factory) => Some(factory(&model).await.map_err(|message| {
                        AgentError::InvalidToolCall {
                            tool: "task".to_string(),
                            message: format!(
                            "subagent '{}' could not build its provider for {reference}: {message}",
                            definition.name
                        ),
                        }
                    })?),
                    None => {
                        return Err(AgentError::InvalidToolCall {
                            tool: "task".to_string(),
                            message: format!(
                        "subagent '{}' names a model ({reference}) but this mode cannot build one",
                        definition.name
                    ),
                        })
                    }
                }
            }
            None => None,
        };
    let provider = provider.as_deref().unwrap_or(parent_provider);
    on_event(&SubagentEvent::Started {
        agent: definition.name.as_str(),
        description: args.description.as_str(),
    });
    let mut child = child_runtime(runtime, &definition)?;
    let description = args.description.trim();
    let prompt = if description.is_empty() {
        args.prompt.clone()
    } else {
        format!("{}\n\n{}", args.description.trim(), args.prompt)
    };
    let control = RunControl {
        cancellation: run.cancellation.clone(),
        ..RunControl::default()
    };
    // Boxed: the child's turn calls `execute_model_tool_call` again (its
    // own tools), which is what makes this future recursive.
    let turn = Box::pin(run_user_turn_streaming_events_with_media(
        &mut child,
        provider,
        prompt,
        Vec::new(),
        &control,
        |_event| {},
    ));
    let result = turn.await;
    match result {
        Ok(text) => {
            on_event(&SubagentEvent::Finished {
                agent: definition.name.as_str(),
                is_error: false,
            });
            Ok(text)
        }
        Err(error) => {
            on_event(&SubagentEvent::Finished {
                agent: definition.name.as_str(),
                is_error: true,
            });
            Err(error)
        }
    }
}

/// The runtime a subagent runs in: the parent's systems (extension tools,
/// retry, compaction, progress) with a fresh session whose active tools are
/// the definition's scope and whose model is the definition's override, and
/// whose system prompt is the definition's body.
pub(crate) fn child_runtime(
    parent: &Runtime,
    definition: &noel_config::AgentDefinition,
) -> Result<Runtime, AgentError> {
    use std::collections::BTreeSet;

    let mut systems = parent.systems.clone();
    systems.system_prompt = Some(definition.system_prompt.clone());
    systems.append_system_prompt = Vec::new();
    // A subagent lists no skills: the skill list is the parent's surface.
    // It may still read a skill's file if the parent's prompt pointed one
    // out.
    systems.skills = Vec::new();
    // Tools: the definition's scope, or the parent's actives without
    // `task`. A named tool the parent does not have is refused, as Claude
    // refuses a subagent whose tools resolve to nothing.
    let parent_tools: BTreeSet<String> = parent
        .session
        .active_tool_names
        .iter()
        .filter(|name| name.as_str() != "task")
        .cloned()
        .collect();
    let scoped = match &definition.tools {
        Some(tools) => {
            let mut scope = BTreeSet::new();
            let mut unknown = Vec::new();
            for tool in tools {
                if tool == "task" {
                    return Err(AgentError::InvalidToolCall {
                        tool: "task".to_string(),
                        message: "subagents cannot nest: a subagent cannot have the task tool"
                            .to_string(),
                    });
                }
                if parent_tools.contains(tool) {
                    scope.insert(tool.clone());
                } else {
                    unknown.push(tool.clone());
                }
            }
            if scope.is_empty() {
                let reason = if unknown.is_empty() {
                    "its tools list is empty".to_string()
                } else {
                    format!(
                        "its tools list resolved to nothing: unrecognized [{}]",
                        unknown.join(", ")
                    )
                };
                return Err(AgentError::InvalidToolCall {
                    tool: "task".to_string(),
                    message: format!(
                        "subagent '{}' would be spawned with zero tools — refusing. {}. Fix the agent's tools frontmatter.",
                        definition.name, reason
                    ),
                });
            }
            scope
        }
        None => parent_tools,
    };
    let session = SessionState {
        session_id: format!("{}-task", parent.session.session_id),
        cwd: parent.session.cwd.clone(),
        parent_session_id: Some(parent.session.session_id.clone()),
        active_model: parent.session.active_model.clone(),
        active_thinking_level: definition
            .thinking
            .clone()
            .or_else(|| parent.session.active_thinking_level.clone()),
        active_account: parent.session.active_account.clone(),
        active_tool_names: scoped,
        disabled_tools: BTreeSet::new(),
        workspace: parent.session.workspace.clone(),
        ..empty_session()
    };
    Ok(Runtime::new(session, systems))
}

/// A `SessionState` with the fields a subagent does not carry.
fn empty_session() -> SessionState {
    use std::collections::{BTreeMap, BTreeSet};
    SessionState {
        session_id: String::new(),
        cwd: PathBuf::new(),
        name: None,
        labels: BTreeSet::new(),
        parent_session_id: None,
        messages: Vec::new(),
        tool_history: Vec::new(),
        queued_messages: Vec::new(),
        queued_steering: 0,
        queued_ids: Vec::new(),
        past_submissions: Vec::new(),
        compactions: Vec::new(),
        branch_summaries: Vec::new(),
        active_model: None,
        active_thinking_level: None,
        active_account: None,
        active_tool_names: BTreeSet::new(),
        disabled_tools: BTreeSet::new(),
        todos: Vec::new(),
        edited_files: Vec::new(),
        usage: crate::SessionUsage::default(),
        runs: Vec::new(),
        partial: None,
        tool_outputs: BTreeMap::new(),
        workspace: None,
        snapshots: Vec::new(),
        rewinds: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse() {
        let args: TaskToolArgs = serde_json::from_str(
            r#"{"subagent":"researcher","description":"Count files","prompt":"count them"}"#,
        )
        .unwrap();
        assert_eq!(args.subagent, "researcher");
        assert_eq!(args.description, "Count files");
    }
}

//! Subagent (`task` tool) tests, on faux providers: a parent whose scripted
//! reply calls `task`, the child's own turn over `faux/echo`, and the
//! scoping and refusal rules of an agent definition.

use std::collections::BTreeSet;
use std::path::PathBuf;

use noel_ai::{create_provider, ProviderApi, ProviderAuth, ProviderConfig};

use crate::subagents::{run_task_tool, TaskToolArgs};
use crate::{AgentDefinition, RunControl, Runtime, SessionState};

fn agent(name: &str, tools: Option<&[&str]>) -> AgentDefinition {
    AgentDefinition {
        name: name.to_string(),
        description: format!("The {name} agent."),
        file_path: PathBuf::from(format!("/agents/{name}.md")),
        system_prompt: format!("You are the {name} agent."),
        tools: tools.map(|tools| tools.iter().map(|tool| tool.to_string()).collect()),
        model: None,
        thinking: None,
        source_info: noel_config::SourceInfo {
            path: PathBuf::from(format!("/agents/{name}.md")),
            source: "local".to_string(),
            scope: noel_config::SourceScope::Project,
            from_package: false,
            base_dir: Some(PathBuf::from("/agents")),
        },
    }
}

fn runtime_with(agent: AgentDefinition) -> Runtime {
    let session = SessionState::new("parent", PathBuf::from("."));
    let mut runtime = Runtime::new(session, crate::ReloadableSystems::default());
    runtime.systems.agents = vec![agent];
    runtime
}

fn echo_provider() -> Box<dyn noel_ai::Provider> {
    create_provider(ProviderConfig {
        model: noel_ai::ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    })
}

#[tokio::test]
async fn a_task_run_returns_the_childs_final_reply() {
    let mut runtime = runtime_with(agent("researcher", Some(&["read"])));
    let provider = echo_provider();
    let args = TaskToolArgs {
        subagent: "researcher".to_string(),
        description: "Count files".to_string(),
        prompt: "count the files".to_string(),
    };
    let run = RunControl::default();
    let result = run_task_tool(&mut runtime, provider.as_ref(), &run, &args, &mut |_| {})
        .await
        .expect("task run");
    // The child's prompt carries the description and the task; faux/echo
    // answers with the last user message it was sent.
    assert_eq!(result, "[faux/echo] Count files\n\ncount the files");
    // The parent's session is untouched: the child ran in its own.
    assert!(runtime.session().messages.is_empty());
}

#[tokio::test]
async fn an_unknown_subagent_names_the_loaded_ones() {
    let mut runtime = runtime_with(agent("researcher", Some(&["read"])));
    let provider = echo_provider();
    let args = TaskToolArgs {
        subagent: "nope".to_string(),
        description: String::new(),
        prompt: "anything".to_string(),
    };
    let run = RunControl::default();
    let result = run_task_tool(&mut runtime, provider.as_ref(), &run, &args, &mut |_| {})
        .await
        .expect("task run");
    assert!(result.contains("unknown subagent 'nope'"));
    assert!(result.contains("researcher"));
}

#[tokio::test]
async fn a_definition_scopes_the_childs_tools_without_task() {
    let runtime = runtime_with(agent("researcher", Some(&["read", "grep"])));
    let parent = {
        let mut session = SessionState::new("parent", PathBuf::from("."));
        session.active_tool_names = BTreeSet::from([
            "read".to_string(),
            "grep".to_string(),
            "bash".to_string(),
            "task".to_string(),
        ]);
        Runtime::new(session, crate::ReloadableSystems::default())
    };
    let definition = runtime.systems.agents[0].clone();
    let child = crate::subagents::child_runtime(&parent, &definition).expect("child runtime");
    assert_eq!(
        child.child_session().active_tool_names,
        BTreeSet::from(["read".to_string(), "grep".to_string()])
    );
    // The child's system prompt is the definition's body.
    assert!(child.systems.system_prompt.as_deref() == Some("You are the researcher agent."));
}

#[tokio::test]
async fn a_tools_list_that_resolves_to_nothing_is_refused() {
    let runtime = runtime_with(agent("broken", Some(&["read", "not_a_tool"])));
    let parent = {
        let mut session = SessionState::new("parent", PathBuf::from("."));
        session.active_tool_names = BTreeSet::from(["bash".to_string()]);
        Runtime::new(session, crate::ReloadableSystems::default())
    };
    let definition = runtime.systems.agents[0].clone();
    let error = crate::subagents::child_runtime(&parent, &definition)
        .expect_err("zero tools refused")
        .to_string();
    assert!(error.contains("zero tools"), "{error}");
    assert!(error.contains("not_a_tool"), "{error}");
}

#[tokio::test]
async fn task_in_a_tools_list_is_refused() {
    let runtime = runtime_with(agent("nester", Some(&["task"])));
    let parent = {
        let mut session = SessionState::new("parent", PathBuf::from("."));
        session.active_tool_names = BTreeSet::from(["task".to_string(), "read".to_string()]);
        Runtime::new(session, crate::ReloadableSystems::default())
    };
    let definition = runtime.systems.agents[0].clone();
    let error = crate::subagents::child_runtime(&parent, &definition)
        .expect_err("nesting refused")
        .to_string();
    assert!(error.contains("cannot nest"), "{error}");
}

#[test]
fn available_agents_description_lists_names() {
    let description = crate::subagents::available_agents_description(&[
        agent("researcher", None),
        agent("reviewer", None),
    ]);
    assert_eq!(
        description,
        "- researcher: The researcher agent.\n- reviewer: The reviewer agent."
    );
}

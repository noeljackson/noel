//! Plan mode tests (spec §9.4): mutating tools are refused, the system
//! prompt carries the rules, and the mode journals across a reload.

use std::fs;
use std::path::PathBuf;

use crate::{runtime_system_prompt, ReloadableSystems, Runtime, SessionState, SessionStore};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noel-plan-{}-{}", name, crate::new_session_id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn plan_mode_refuses_mutating_tools_and_keeps_read_tools() {
    let mut runtime = Runtime::new(
        SessionState::new("plan", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    for tool in ["read", "grep", "find", "ls", "bash", "todo"] {
        assert!(
            crate::model_tool_enabled_for_tests(&runtime, tool),
            "{tool} stays on in plan mode"
        );
    }
    runtime.session.plan_mode = true;
    for tool in ["edit", "write"] {
        assert!(
            !crate::model_tool_enabled_for_tests(&runtime, tool),
            "{tool} is refused in plan mode"
        );
    }
}

#[test]
fn plan_mode_adds_its_rules_to_the_system_prompt() {
    let mut runtime = Runtime::new(
        SessionState::new("plan", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let without = runtime_system_prompt(&runtime);
    runtime.session.plan_mode = true;
    let with = runtime_system_prompt(&runtime);
    assert!(!without.contains("plan mode"));
    assert!(with.contains("plan mode"));
    assert!(with.contains("read-only"));
    assert!(with.contains("present a concrete plan"));
}

#[test]
fn plan_mode_is_journaled_and_reloads() {
    let dir = temp_dir("journal");
    let (store, state) = SessionStore::create(&dir.join("sessions"), dir.clone()).unwrap();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    runtime.set_plan_mode(true).unwrap();
    runtime.set_plan_mode(false).unwrap();
    let path = dir
        .join("sessions")
        .join(format!("{}.jsonl", runtime.session().session_id));
    let (_, state) = SessionStore::open(path).unwrap();
    assert!(!state.plan_mode);
    fs::remove_dir_all(&dir).unwrap();
}

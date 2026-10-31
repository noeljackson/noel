//! User hooks (spec §9.5): plain shell commands around tool calls, run
//! from the session's settings — no host program, as extensions need.
//!
//! A hook command runs in the session's cwd with one JSON object on
//! stdin: `tool`, `arguments` (the raw JSON string the model sent),
//! `cwd`, `sessionId`. A **pre**-hook that exits 2 blocks the call, its
//! stderr the reason the model sees (Claude's exit-2 rule); any other
//! exit — including failure to run at all — allows the call (a broken
//! hook must not take the session down, but a blocking one fails closed
//! only by its explicit decision). A **post**-hook's output is ignored;
//! a non-zero exit is a system line.

use std::path::Path;
use std::process::Stdio;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::Runtime;

/// What a pre-hook decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    /// Block, with the reason the model sees.
    Block(String),
}

/// Runs the **pre**-hooks matching `tool` and folds their decisions: the
/// first exit-2 block wins. Any other exit allows (Claude's rule: a
/// failing hook is a non-blocking error, its stderr shown as a notice);
/// only a hook that cannot run at all blocks, fail-closed like the
/// extension gate.
pub async fn run_pre_hooks(
    runtime: &Runtime,
    tool: &str,
    arguments: &str,
) -> (HookDecision, Vec<String>) {
    let hooks = runtime.systems.hooks.matching("pre", tool).to_vec();
    let mut notices = Vec::new();
    for hook in hooks {
        match run_hook(
            runtime,
            tool,
            arguments,
            &hook.command,
            hook.matcher.as_str(),
        )
        .await
        {
            Ok((2, stderr)) => {
                let reason = stderr.trim();
                return (
                    HookDecision::Block(if reason.is_empty() {
                        format!("blocked by hook: {}", hook.command)
                    } else {
                        reason.to_string()
                    }),
                    notices,
                );
            }
            Ok((code, stderr)) if code != 0 => notices.push(format!(
                "pre-hook {}: exit {code}{}",
                hook.command,
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            )),
            Ok(_) => {}
            Err(error) => {
                return (
                    HookDecision::Block(format!("hook failed to run: {error}")),
                    notices,
                )
            }
        }
    }
    (HookDecision::Allow, notices)
}

/// Runs the **post**-hooks matching `tool`; each non-zero exit is one
/// system line (`None` when all are clean).
pub async fn run_post_hooks(runtime: &Runtime, tool: &str, arguments: &str) -> Vec<String> {
    let hooks = runtime.systems.hooks.matching("post", tool).to_vec();
    let mut notices = Vec::new();
    for hook in hooks {
        match run_hook(
            runtime,
            tool,
            arguments,
            &hook.command,
            hook.matcher.as_str(),
        )
        .await
        {
            Ok((code, stderr)) if code != 0 => {
                notices.push(format!(
                    "post-hook {}: exit {code}{}",
                    hook.command,
                    if stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", stderr.trim())
                    }
                ));
            }
            Ok(_) => {}
            Err(error) => {
                notices.push(format!("post-hook {} failed to run: {error}", hook.command))
            }
        }
    }
    notices
}

/// Runs one hook command with its stdin JSON. Returns the exit code and
/// stderr.
async fn run_hook(
    runtime: &Runtime,
    tool: &str,
    arguments: &str,
    command: &str,
    matcher: &str,
) -> Result<(i32, String), String> {
    let cwd: &Path = &runtime.session().cwd;
    let payload = json!({
        "tool": tool,
        "arguments": arguments,
        "matcher": matcher,
        "cwd": cwd,
        "sessionId": runtime.session().session_id,
    });
    // The shell the session's tools use, so a hook written for one shell
    // runs in it; a hook is a one-liner in settings, not a program.
    let mut process = Command::new(
        runtime
            .systems
            .shell_path
            .clone()
            .unwrap_or_else(|| "/bin/sh".to_string()),
    );
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NOEL_SESSION_ID", &runtime.session().session_id)
        .env("NOEL_TOOL", tool);
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("{payload}\n").as_bytes())
            .await
            .map_err(|error| error.to_string())?;
    }
    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReloadableSystems, Runtime, SessionState};
    use std::path::PathBuf;

    fn runtime_with(hooks: noel_config::HooksSettings) -> Runtime {
        let mut runtime = Runtime::new(
            SessionState::new("hooks-test", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        runtime.systems.hooks = hooks;
        runtime
    }

    fn hook(matcher: &str, command: &str) -> noel_config::HookCommand {
        noel_config::HookCommand {
            matcher: matcher.to_string(),
            command: command.to_string(),
        }
    }

    #[tokio::test]
    async fn an_exit_two_pre_hook_blocks_with_its_stderr() {
        let hooks = noel_config::HooksSettings {
            pre_tool_use: vec![hook("edit|write", "echo 'not allowed here' >&2; exit 2")],
            ..Default::default()
        };
        let runtime = runtime_with(hooks);
        let (decision, notices) = run_pre_hooks(&runtime, "edit", "{}").await;
        assert_eq!(decision, HookDecision::Block("not allowed here".into()));
        assert!(notices.is_empty());
        // A non-matching tool passes.
        let (decision, _) = run_pre_hooks(&runtime, "read", "{}").await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn a_zero_exit_allows_a_failing_one_notices_and_a_broken_command_allows() {
        let allow = noel_config::HooksSettings {
            pre_tool_use: vec![hook("", "exit 0")],
            ..Default::default()
        };
        let (decision, notices) = run_pre_hooks(&runtime_with(allow), "bash", "{}").await;
        assert_eq!(decision, HookDecision::Allow);
        assert!(notices.is_empty());
        // A failing hook is a non-blocking error: allow, with a notice.
        let failing = noel_config::HooksSettings {
            pre_tool_use: vec![hook("", "echo oops >&2; exit 1")],
            ..Default::default()
        };
        let (decision, notices) = run_pre_hooks(&runtime_with(failing), "bash", "{}").await;
        assert_eq!(decision, HookDecision::Allow);
        assert_eq!(notices.len(), 1);
        // A command the shell cannot even find is the same: 127, allow.
        let missing = noel_config::HooksSettings {
            pre_tool_use: vec![hook("", "definitely-not-a-command-xyz")],
            ..Default::default()
        };
        let (decision, _) = run_pre_hooks(&runtime_with(missing), "bash", "{}").await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn the_hook_reads_its_stdin_json() {
        let hooks = noel_config::HooksSettings {
            pre_tool_use: vec![hook("edit", r#"grep -q '"tool":"edit"' || exit 2"#)],
            ..Default::default()
        };
        let runtime = runtime_with(hooks);
        let (decision, _) = run_pre_hooks(&runtime, "edit", "{\"path\":\"a\"}").await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn post_hook_notices_report_nonzero_exits() {
        let hooks = noel_config::HooksSettings {
            post_tool_use: vec![hook("bash", "echo done >&2; exit 1")],
            ..Default::default()
        };
        let notices = run_post_hooks(&runtime_with(hooks), "bash", "{}").await;
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("exit 1"));
    }
}

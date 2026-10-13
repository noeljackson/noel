//! `kill_running_tool_groups` is process-wide, so it runs in its own test
//! binary where it cannot kill the tools of unrelated tests.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use noel_tools::{execute_tool, kill_running_tool_groups, ToolRequest, ToolRuntimeOptions};

async fn read_pid_file(pid_file: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(pid) = std::fs::read_to_string(pid_file) {
            return pid.trim().parse().expect("pid file holds a pid");
        }
        assert!(
            Instant::now() < deadline,
            "no pid in {}",
            pid_file.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A killed process stays a zombie (state Z) until reaped; that is exited.
fn process_is_running(pid: i32) -> bool {
    if cfg!(target_os = "linux") {
        return std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
        });
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[tokio::test]
async fn kill_running_tool_groups_kills_every_running_tool_tree() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("noel-tool-groups-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let options = ToolRuntimeOptions {
        shell_path: Some("/bin/sh".into()),
        ..Default::default()
    };
    let tools = (0..2)
        .map(|index| {
            let pid_file = dir.join(format!("{index}.pid"));
            let command = format!(
                "sleep 30 & echo $! > '{0}.tmp' && mv '{0}.tmp' '{0}'; wait",
                pid_file.display()
            );
            let options = options.clone();
            let tool = tokio::spawn(async move {
                execute_tool(
                    Path::new("."),
                    ToolRequest::Bash {
                        command,
                        timeout_ms: Some(60_000),
                    },
                    &options,
                )
                .await
            });
            (pid_file, tool)
        })
        .collect::<Vec<_>>();
    let mut pids = Vec::new();
    for (pid_file, _) in &tools {
        pids.push(read_pid_file(pid_file).await);
    }

    kill_running_tool_groups();

    for (_, tool) in tools {
        let result = tokio::time::timeout(Duration::from_secs(5), tool)
            .await
            .expect("a killed tool finishes")
            .unwrap()
            .unwrap();
        assert!(result.is_error, "{}", result.output);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|&pid| process_is_running(pid)) {
        if Instant::now() >= deadline {
            for &pid in &pids {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            panic!("background children outlived kill_running_tool_groups");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::remove_dir_all(dir).unwrap();
}

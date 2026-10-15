//! noel's built-in tools in a workspace on another machine (spec §10): through `RemoteEnv` and `noel-env serve`
//! (started here, without ssh, so "remote" files are this machine's), every tool gives what it gives through
//! `LocalEnv` on the same files. `grep` and `find` there list with the daemon's `listFiles` and read through the
//! workspace; the corners where that differs (`.git`, ignore files other than git's) are asserted too, as the
//! non-parity register lists them. A lost connection fails the call in flight, and the next call starts a new
//! daemon.

mod support;

use std::fs;
use std::time::Duration;

use noel_env::{ExecutionEnv, LocalEnv};
use noel_tools::{execute_tool_in, TextEdit, ToolRequest, ToolRuntimeOptions};
use support::{connection, remote_env, Scratch};

/// A tool's result as text: its output and whether it is an error, or the error it failed with.
async fn run(env: &dyn ExecutionEnv, request: ToolRequest) -> String {
    match execute_tool_in(env, request, &ToolRuntimeOptions::default()).await {
        Ok(result) => format!(
            "{}{}",
            if result.is_error { "error: " } else { "" },
            result.output
        ),
        Err(error) => format!("failed: {error}"),
    }
}

fn grep(pattern: &str) -> ToolRequest {
    ToolRequest::Grep {
        path: None,
        pattern: pattern.to_string(),
        glob: None,
        literal: None,
        ignore_case: None,
        context: None,
        limit: None,
    }
}

fn find(pattern: &str) -> ToolRequest {
    ToolRequest::Find {
        pattern: pattern.to_string(),
        path: None,
        limit: None,
    }
}

/// A tree with nested, hidden, binary and linked files, outside any git repository.
fn tree(scratch: &Scratch) {
    let write = |path: &str, content: &[u8]| {
        let path = scratch.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    write(
        "src/lib.rs",
        b"pub fn needle() {}\n// Needle in a comment\nfn other() {}\n",
    );
    write(
        "src/deep/inner/mod.rs",
        b"// nothing here\nconst NEEDLE: u8 = 1;\n",
    );
    write(
        "docs/guide.md",
        b"# Guide\n\nThe needle is here.\nAnd a second needle line.\n",
    );
    write(".hidden/secret.txt", b"a hidden needle\n");
    write("data.bin", b"needle before\n\0needle after\n");
    write("crlf.txt", b"one\r\nneedle two\r\nthree\r\n");
    write(
        "long.txt",
        format!("needle {}\n", "x".repeat(600)).as_bytes(),
    );
    std::os::unix::fs::symlink(scratch.join("docs"), scratch.join("docs-link")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn every_tool_gives_what_it_gives_locally() {
    let scratch = Scratch::new("tools");
    tree(&scratch);
    let connection = connection();
    let remote = remote_env(&connection, &scratch.text());
    let local = LocalEnv::new(&scratch.0);
    let requests = vec![
        ToolRequest::Read {
            path: "docs/guide.md".into(),
            offset: Some(2),
            limit: Some(2),
        },
        ToolRequest::Read {
            path: "missing.txt".into(),
            offset: None,
            limit: None,
        },
        ToolRequest::Ls {
            path: None,
            limit: None,
        },
        ToolRequest::Ls {
            path: Some("src".into()),
            limit: Some(1),
        },
        ToolRequest::Bash {
            command: "printf 'one\\ntwo\\n'; pwd; exit 3".into(),
            timeout_ms: None,
        },
        ToolRequest::Bash {
            command: "sleep 5".into(),
            timeout_ms: Some(200),
        },
        grep("needle"),
        ToolRequest::Grep {
            path: Some("src".into()),
            pattern: "NEEDLE".into(),
            glob: None,
            literal: Some(true),
            ignore_case: Some(true),
            context: Some(1),
            limit: Some(2),
        },
        ToolRequest::Grep {
            path: None,
            pattern: "needle".into(),
            glob: Some("*.md".into()),
            literal: None,
            ignore_case: None,
            context: None,
            limit: None,
        },
        ToolRequest::Grep {
            path: None,
            pattern: "needle".into(),
            glob: Some("!src/deep/**".into()),
            literal: None,
            ignore_case: Some(true),
            context: None,
            limit: None,
        },
        ToolRequest::Grep {
            path: Some("data.bin".into()),
            pattern: "after".into(),
            glob: None,
            literal: None,
            ignore_case: None,
            context: None,
            limit: None,
        },
        ToolRequest::Grep {
            path: Some("crlf.txt".into()),
            pattern: "two".into(),
            glob: None,
            literal: None,
            ignore_case: None,
            context: Some(1),
            limit: None,
        },
        grep("nowhere"),
        grep("("),
        ToolRequest::Grep {
            path: Some("missing".into()),
            pattern: "x".into(),
            glob: None,
            literal: None,
            ignore_case: None,
            context: None,
            limit: None,
        },
        find("*.rs"),
        find("*"),
        find("**/inner/*"),
        find("GUIDE.md"),
        find("guide.md"),
        ToolRequest::Find {
            pattern: "*".into(),
            path: Some("src".into()),
            limit: Some(2),
        },
        ToolRequest::Find {
            pattern: "*".into(),
            path: Some("data.bin".into()),
            limit: None,
        },
        ToolRequest::Find {
            pattern: "*".into(),
            path: Some("missing".into()),
            limit: None,
        },
    ];
    for request in requests {
        let described = format!("{request:?}");
        let here = run(&local, request.clone()).await;
        let there = run(&remote, request).await;
        assert_eq!(there, here, "{described}");
    }

    // Writes and edits land in the workspace's files, with the same results.
    let write = ToolRequest::Write {
        path: "new/dir/file.txt".into(),
        content: "alpha\nbeta\n".into(),
    };
    assert_eq!(
        run(&remote, write).await,
        "Successfully wrote to new/dir/file.txt"
    );
    let edit = |find: &str, replace: &str| ToolRequest::Edit {
        path: "new/dir/file.txt".into(),
        edits: vec![TextEdit {
            find: find.into(),
            replace: replace.into(),
        }],
    };
    assert_eq!(
        run(&remote, edit("beta", "gamma")).await,
        "Successfully replaced 1 block(s) in new/dir/file.txt."
    );
    assert_eq!(
        fs::read_to_string(scratch.join("new/dir/file.txt")).unwrap(),
        "alpha\ngamma\n"
    );
    assert_eq!(
        run(&remote, edit("missing", "x")).await,
        run(&local, edit("missing", "x")).await
    );
    connection.close();
}

/// Long output keeps its end for the model, as here, and the complete output is saved in the workspace, where its
/// `read` finds it.
#[tokio::test(flavor = "multi_thread")]
async fn long_bash_output_is_saved_in_the_workspace() {
    let scratch = Scratch::new("tools-spill");
    let connection = connection();
    let remote = remote_env(&connection, &scratch.text());
    let local = LocalEnv::new(&scratch.0);
    let request = ToolRequest::Bash {
        command: "seq 1 5000".into(),
        timeout_ms: None,
    };
    let here = run(&local, request.clone()).await;
    let there = run(&remote, request).await;
    let (here_text, here_path) = here.split_once("Full output: ").unwrap();
    let (there_text, there_path) = there.split_once("Full output: ").unwrap();
    assert_eq!(there_text, here_text);
    let (here_path, there_path) = (
        here_path.trim_end_matches(']'),
        there_path.trim_end_matches(']'),
    );
    assert_ne!(here_path, there_path);
    // The workspace's copy, read back through the workspace.
    let read = ToolRequest::Read {
        path: there_path.into(),
        offset: Some(5000),
        limit: None,
    };
    assert_eq!(run(&remote, read).await, "5000\n");
    let full = (1..=5000).map(|n| format!("{n}\n")).collect::<String>();
    for path in [here_path, there_path] {
        assert_eq!(fs::read_to_string(path).unwrap(), full, "{path}");
        let _ = fs::remove_file(path);
    }
    connection.close();
}

/// Where a remote `grep` and `find` differ from local ones: they list with the daemon, which never lists `.git` and
/// reads only git's ignore rules, inside a git work tree.
#[tokio::test(flavor = "multi_thread")]
async fn remote_searches_skip_git_and_read_only_gits_ignore_rules() {
    let scratch = Scratch::new("tools-git");
    let write = |path: &str, content: &str| {
        let path = scratch.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    write(".git/HEAD", "needle: refs/heads/main\n");
    write(".gitignore", "ignored.txt\n");
    write(".ignore", "dot-ignored.txt\n");
    write("ignored.txt", "needle\n");
    write("dot-ignored.txt", "needle\n");
    write("kept.txt", "needle\n");
    let connection = connection();
    let remote = remote_env(&connection, &scratch.text());
    let local = LocalEnv::new(&scratch.0);
    assert_eq!(
        run(&local, grep("needle")).await,
        ".git/HEAD:1: needle: refs/heads/main\nkept.txt:1: needle"
    );
    assert_eq!(
        run(&remote, grep("needle")).await,
        "dot-ignored.txt:1: needle\nkept.txt:1: needle"
    );
    assert_eq!(
        run(&remote, find("*.txt")).await,
        "dot-ignored.txt\nkept.txt"
    );
    connection.close();
}

/// A lost connection fails the call in flight; the next call starts a new daemon.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_daemon_fails_the_call_and_the_next_starts_another() {
    let scratch = Scratch::new("tools-lost");
    let connection = connection();
    let remote = remote_env(&connection, &scratch.text());
    let first = connection.info().await.unwrap().pid;
    let sleeping = run(
        &remote,
        ToolRequest::Bash {
            command: "sleep 30".into(),
            timeout_ms: None,
        },
    );
    let killer = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // SAFETY: kill(2) on the daemon this test started.
        unsafe { libc::kill(first as i32, libc::SIGKILL) };
    };
    let (result, ()) = tokio::join!(sleeping, killer);
    assert_eq!(result, "failed: pi-env connection lost");
    let after = run(
        &remote,
        ToolRequest::Bash {
            command: "echo again".into(),
            timeout_ms: None,
        },
    )
    .await;
    assert_eq!(after, "again\n");
    let second = connection.info().await.unwrap().pid;
    assert_ne!(first, second);
    connection.close();
}

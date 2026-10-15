//! The client's `Connection` and `RemoteEnv` over a daemon started directly (no ssh): upstream's `remote.test.ts`
//! and `differential.test.ts` behaviours, ported, plus the connection's lifetime (restart after loss, noise before
//! the sync line, closing, dropping a command). The daemon is `noel-env serve`, or upstream's `pi-env` with
//! `NOEL_ENV_TEST_DAEMON`.
//!
//! Ported in part from upstream pi's `packages/env/test/remote.test.ts` (<https://github.com/earendil-works/pi>,
//! commit `1692bba`), Copyright (c) 2025 Mario Zechner, MIT License (see LICENSE at the repository root).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use noel_env::frame::{encode_frame, RESULT};
use noel_env::{
    node_platform, AbortController, AbortSignal, BinaryReaderOptions, Connection,
    ConnectionOptions, ConnectionStatus, Context, DaemonCommand, ExecOptions, FileErrorCode,
    FileSystem, ListFilesOptions, LocalEnv, OutputChunk, OutputWindow, RequestOptions, Shell,
    ShellCommand,
};
use serde_json::{json, Value};
use support::{connection, daemon, remote_env, Scratch};

fn cx() -> Context {
    Context::background()
}

fn sh(script: &str) -> ShellCommand {
    ShellCommand::Argv(vec!["sh".into(), "-c".into(), script.into()])
}

fn kill(pid: u64, signal: i32) {
    // SAFETY: kill(2) on a daemon this test started.
    unsafe { libc::kill(pid as i32, signal) };
}

fn object(value: Value) -> noel_env::Json {
    value.as_object().cloned().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn transfers_only_about_a_window_of_output_a_caller_keeps_with_exact_counts() {
    let dir = Scratch::new("window");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let total = 5_000_000u64;
    let (mut delivered, mut skipped, mut tail) = (0u64, 0u64, String::new());
    let mut on_output = |chunk: OutputChunk<'_>| {
        delivered += chunk.text.len() as u64;
        if let Some(skip) = chunk.skipped {
            skipped += skip.bytes;
            tail.clear();
        }
        tail.push_str(chunk.text);
        Ok(())
    };
    let result = env
        .exec(
            sh(&format!("yes 0123456789abcdef | head -c {total}")),
            ExecOptions {
                window: Some(OutputWindow {
                    max_bytes: 1000,
                    max_lines: 20,
                    min_interval: Duration::from_millis(20),
                    bytes_per_second: 1_000_000.0,
                }),
                on_output: Some(&mut on_output),
                ..ExecOptions::default()
            },
            &cx(),
        )
        .await
        .unwrap();
    connection.close();
    assert_eq!(result.exit_code, 0);
    assert_eq!(delivered + skipped, total);
    assert!(skipped > 0);
    assert!(delivered < total / 10, "delivered {delivered}");
    // After the last skip comes everything to the end, more than the window.
    let full = "0123456789abcdef\n".repeat(total as usize / 17 + 1);
    assert!(full[..total as usize].ends_with(&tail));
    assert!(tail.len() > 1000 || tail.matches('\n').count() > 20);
}

#[tokio::test(flavor = "multi_thread")]
async fn aborts_commands_cancelled_right_after_they_are_sent() {
    let dir = Scratch::new("abort");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    connection.info().await.unwrap();
    let started = Instant::now();
    for attempt in 0..20u64 {
        let controller = AbortController::new();
        let context = Context::with_abort_signal(controller.signal());
        let running = env.exec(sh("sleep 5"), ExecOptions::default(), &context);
        let abort = async {
            if attempt % 2 == 1 {
                tokio::time::sleep(Duration::from_millis(attempt % 5)).await;
            }
            controller.abort();
        };
        let (result, ()) = tokio::join!(running, abort);
        assert_eq!(
            result.unwrap_err().code.as_str(),
            "aborted",
            "attempt {attempt}"
        );
    }
    connection.close();
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_the_remote_system_in_nodes_names() {
    let connection = connection();
    let info = connection.info().await.unwrap();
    connection.close();
    assert_eq!(
        node_platform(&info.os, &info.arch),
        node_platform(std::env::consts::OS, std::env::consts::ARCH)
    );
    assert_eq!(info.protocol, 1);
    assert_eq!(info.separator, "/");
}

/// A stand-in for a daemon released before `hello.features`: it answers `hello` without `features` (upstream's
/// `test/old-daemon.ts`). The client checks `features` before it would send `listFiles`, so `hello` is all it reads:
/// one frame of 40 bytes, `{"op":"hello","protocol":1}` after the header.
fn old_daemon() -> Vec<String> {
    let hello = json!({
        "protocol": 1, "version": "1.1.0", "os": "linux", "arch": "x86_64", "home": "/", "separator": "/",
        "tmpdir": "/tmp", "cwd": "/", "driveCwds": {}, "pid": 1,
    });
    let reply = encode_frame(RESULT, 1, &hello, &[]).unwrap();
    let escaped: String = reply.iter().map(|byte| format!("\\{byte:03o}")).collect();
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            r#"printf 'PI-ENV %s\n' "$3"; head -c 40 >/dev/null; printf '{escaped}'; exec cat >/dev/null"#
        ),
        "old-daemon".into(),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_list_files_as_not_supported_by_daemons_released_before_it() {
    let dir = Scratch::new("old");
    let connection = connection();
    let features = connection.info().await.unwrap().features.clone();
    connection.close();
    assert!(features.unwrap().contains(&"listFiles".to_string()));
    let old = Connection::command(old_daemon());
    let env = remote_env(&old, &dir.text());
    let listed = env
        .list_files(".", &ListFilesOptions::default(), &cx())
        .await;
    old.close();
    assert_eq!(listed.unwrap_err().code, FileErrorCode::NotSupported);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancels_a_listing_in_the_daemon() {
    let dir = Scratch::new("list");
    for directory in 0..100 {
        std::fs::create_dir(dir.join(&format!("d{directory}"))).unwrap();
        for file in 0..100 {
            std::fs::write(dir.join(&format!("d{directory}/f{file}")), "").unwrap();
        }
    }
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    // The cancel follows the request at once, bypassing the client's own abort check, so the daemon stops the walk.
    let aborted = Context::with_abort_signal(AbortSignal::already_aborted());
    let cancelled = connection
        .request(
            "listFiles",
            object(json!({ "path": dir.text() })),
            RequestOptions {
                cx: Some(&aborted),
                ..RequestOptions::default()
            },
        )
        .await;
    assert_eq!(cancelled.unwrap_err().code, "aborted");
    let listed = env
        .list_files(".", &ListFilesOptions::default(), &cx())
        .await
        .unwrap();
    assert_eq!((listed.entries.len(), listed.truncated), (10_100, false));
    let bounded = ListFilesOptions {
        max_entries: Some(5000),
        ..ListFilesOptions::default()
    };
    let listed = env.list_files(".", &bounded, &cx()).await.unwrap();
    connection.close();
    assert_eq!((listed.entries.len(), listed.truncated), (5000, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn times_out_a_command_that_produces_no_output() {
    let dir = Scratch::new("timeout");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let started = Instant::now();
    let result = env
        .exec(
            sh("sleep 5"),
            ExecOptions {
                timeout: Some(Duration::from_millis(300)),
                ..ExecOptions::default()
            },
            &cx(),
        )
        .await;
    connection.close();
    let error = result.unwrap_err();
    assert_eq!(
        (error.code.as_str(), error.message.as_str()),
        ("timeout", "timeout:0.3")
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn fails_requests_on_handles_of_a_lost_connection_instead_of_reaching_the_new_daemon() {
    let dir = Scratch::new("handles");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    env.write_file("a.txt", b"AAA", &cx()).await.unwrap();
    env.write_file("b.txt", b"BBB", &cx()).await.unwrap();
    let options = BinaryReaderOptions::default();
    let mut a = env
        .open_binary_reader("a.txt", options, &cx())
        .await
        .unwrap();
    let pid = connection.info().await.unwrap().pid;
    kill(pid, libc::SIGKILL);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The new daemon numbers its handles from 1 again.
    let mut b = env
        .open_binary_reader("b.txt", options, &cx())
        .await
        .unwrap();
    assert_ne!(connection.info().await.unwrap().pid, pid);
    let stale = a.read(0, 3, &cx()).await.unwrap_err();
    assert_eq!(stale.code, FileErrorCode::Unknown);
    assert_eq!(b.read(0, 3, &cx()).await.unwrap(), b"BBB");
    a.close(&cx()).await;
    b.close(&cx()).await;
    connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn computes_the_command_at_each_start_and_retries_a_start_that_failed() {
    let dir = Scratch::new("starts");
    let starts = Arc::new(AtomicUsize::new(0));
    let counted = starts.clone();
    let connection = Connection::new(ConnectionOptions::new(DaemonCommand::Computed(Arc::new(
        move || {
            let start = counted.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                if start == 1 {
                    return Err("ssh gpu-box failed: Network is unreachable".to_string());
                }
                Ok(vec![daemon()])
            })
        },
    ))));
    let env = remote_env(&connection, &dir.text());
    let offline = env.read_text_file("missing.txt", &cx()).await.unwrap_err();
    assert_eq!(
        (offline.code, offline.message.as_str()),
        (
            FileErrorCode::Unknown,
            "ssh gpu-box failed: Network is unreachable"
        )
    );
    let command = env
        .exec(sh("exit 0"), ExecOptions::default(), &cx())
        .await
        .unwrap();
    connection.close();
    assert_eq!(command.exit_code, 0);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_and_reads_files_larger_than_one_transfer_chunk_in_order() {
    let dir = Scratch::new("large");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let content: Vec<u8> = (0..3_500_017u64)
        .map(|index| ((index * 7919) % 251) as u8)
        .collect();
    env.write_file("big.bin", &content, &cx()).await.unwrap();
    env.append_file("big.bin", &content, &cx()).await.unwrap();
    let read = env.read_binary_file("big.bin", &cx()).await.unwrap();
    assert_eq!(read.len(), content.len() * 2);
    assert!(read[..content.len()] == content[..] && read[content.len()..] == content[..]);
    let mut reader = env
        .open_binary_reader("big.bin", BinaryReaderOptions::default(), &cx())
        .await
        .unwrap();
    let range = reader.read(1_000_003, 2_000_000, &cx()).await.unwrap();
    reader.close(&cx()).await;
    connection.close();
    assert!(range[..] == read[1_000_003..3_000_003]);
}

/// Contents that cross write chunks, read chunks and multi-byte characters split between them, from upstream's
/// `differential.test.ts`; `RemoteEnv` reads them back as `LocalEnv` reads the same files.
#[tokio::test(flavor = "multi_thread")]
async fn reads_what_local_env_reads_across_chunks_and_characters() {
    let dir = Scratch::new("differential");
    let connection = connection();
    let remote = remote_env(&connection, &dir.text());
    let local = LocalEnv::new(dir.text());
    let contents: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"hello\n".to_vec(),
        "\u{feff}bom\r\nline".as_bytes().to_vec(),
        vec![0xe2, 0x82, 0x0a, 0xff, 0xef, 0xbb, 0xbf],
        "x".repeat(70_000).into_bytes(),
        "é".repeat(300_001).into_bytes(),
    ];
    for (index, content) in contents.iter().enumerate() {
        let name = format!("f{index}");
        remote.write_file(&name, content, &cx()).await.unwrap();
        assert_eq!(
            remote.read_binary_file(&name, &cx()).await.unwrap(),
            *content
        );
        assert_eq!(
            remote.read_text_file(&name, &cx()).await.unwrap(),
            local.read_text_file(&name, &cx()).await.unwrap(),
            "{name}"
        );
        assert_eq!(
            remote.read_text_lines(&name, None, &cx()).await.unwrap(),
            local.read_text_lines(&name, None, &cx()).await.unwrap(),
            "{name}"
        );
        let infos = |infos: Vec<noel_env::FileInfo>| {
            infos
                .into_iter()
                .map(|info| (info.name, info.kind, info.size))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            infos(remote.list_dir(".", &cx()).await.unwrap()),
            infos(local.list_dir(".", &cx()).await.unwrap())
        );
    }
    connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_daemon_fails_requests_in_flight_and_the_next_request_starts_a_new_one() {
    let dir = Scratch::new("lost");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let pid = connection.info().await.unwrap().pid;
    let background = cx();
    let running = env.exec(sh("sleep 30"), ExecOptions::default(), &background);
    let killer = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        kill(pid, libc::SIGKILL);
    };
    let (result, ()) = tokio::join!(running, killer);
    let error = result.unwrap_err();
    assert_eq!(
        (error.code.as_str(), error.message.as_str()),
        ("unknown", "pi-env connection lost")
    );
    // By the time a request fails, the connection has forgotten the lost daemon.
    assert!(
        matches!(connection.status(), ConnectionStatus::Lost(_)),
        "{:?}",
        connection.status()
    );
    let next = env
        .exec(sh("exit 4"), ExecOptions::default(), &cx())
        .await
        .unwrap();
    assert_eq!(next.exit_code, 4);
    assert_ne!(connection.info().await.unwrap().pid, pid);
    connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn discards_and_logs_what_a_shell_prints_before_the_sync_line() {
    let logs = Arc::new(Mutex::new(String::new()));
    let sink = logs.clone();
    let connection = Connection::new(ConnectionOptions {
        command: DaemonCommand::Fixed(vec![
            "sh".into(),
            "-c".into(),
            r#"echo "a banner from a login shell"; exec "$0" "$@""#.into(),
            daemon(),
        ]),
        on_log: Some(Arc::new(move |text: &str| {
            sink.lock().unwrap().push_str(text)
        })),
    });
    let info = connection.info().await.unwrap();
    connection.close();
    assert_eq!(info.protocol, 1);
    assert!(logs.lock().unwrap().contains("a banner from a login shell"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_command_stops_it_on_the_remote_machine() {
    let dir = Scratch::new("drop");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let background = cx();
    let running = env.exec(
        sh("sleep 1; touch late"),
        ExecOptions::default(),
        &background,
    );
    assert!(tokio::time::timeout(Duration::from_millis(300), running)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!dir.join("late").exists(), "the dropped command ran on");
    // The connection is still usable.
    let next = env
        .exec(sh("exit 0"), ExecOptions::default(), &cx())
        .await
        .unwrap();
    connection.close();
    assert_eq!(next.exit_code, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_kills_commands_which_settle_with_their_status() {
    let dir = Scratch::new("cleanup");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let background = cx();
    let running = env.exec(sh("sleep 30"), ExecOptions::default(), &background);
    let cleanup = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        env.cleanup(&cx()).await;
    };
    let (result, ()) = tokio::join!(running, cleanup);
    connection.close();
    assert_eq!(result.unwrap().exit_code, 137);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_stops_the_daemon_and_later_requests_fail() {
    let connection = connection();
    let pid = connection.info().await.unwrap().pid;
    connection.close();
    let error = connection.info().await.unwrap_err();
    assert_eq!(error.message, "Connection closed");
    assert!(!error.is_connection_lost());
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only asks whether the process exists.
    while unsafe { libc::kill(pid as i32, 0) } == 0 {
        assert!(Instant::now() < deadline, "the daemon still runs");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_whose_output_callback_fails_is_killed_and_reports_it() {
    let dir = Scratch::new("callback");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let started = Instant::now();
    let mut on_output = |_chunk: OutputChunk<'_>| Err("no more, thanks".to_string());
    let result = env
        .exec(
            sh("echo first; sleep 30"),
            ExecOptions {
                on_output: Some(&mut on_output),
                ..ExecOptions::default()
            },
            &cx(),
        )
        .await;
    connection.close();
    let error = result.unwrap_err();
    assert_eq!(
        (error.code.as_str(), error.message.as_str()),
        ("callback_error", "no more, thanks")
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_above_the_frame_limit_fails_alone() {
    let dir = Scratch::new("oversized");
    let connection = connection();
    let env = remote_env(&connection, &dir.text());
    let huge = "x".repeat(noel_env::frame::MAX_FRAME);
    let refused = env
        .exec(
            ShellCommand::Argv(vec!["true".into(), huge]),
            ExecOptions::default(),
            &cx(),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code.as_str(), "unknown");
    let next = env
        .exec(sh("exit 0"), ExecOptions::default(), &cx())
        .await
        .unwrap();
    connection.close();
    assert_eq!(next.exit_code, 0);
}

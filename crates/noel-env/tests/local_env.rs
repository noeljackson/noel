//! `LocalEnv` behaviour the conformance goldens do not reach: noel's
//! additions to `exec` (removed variables, merged output, raw bytes), spill,
//! `cleanup`, and the shell and working-directory failures.

use std::time::Duration;

use noel_env::{
    Context, ExecOptions, ExecResult, ExecutionError, ExecutionErrorCode, FileErrorCode,
    FileSystem, LocalEnv, LocalEnvOptions, OutputStream, Shell, ShellCommand, SpillOptions,
};

fn cx() -> Context {
    Context::background()
}

fn sh(script: &str) -> ShellCommand {
    ShellCommand::Argv(vec!["sh".into(), "-c".into(), script.into()])
}

/// Runs `command`, returning its result and every chunk as (stream, text,
/// raw bytes).
async fn run(
    env: &LocalEnv,
    command: ShellCommand,
    options: ExecOptions<'_>,
) -> (
    Result<ExecResult, ExecutionError>,
    Vec<(OutputStream, String, Vec<u8>)>,
) {
    let mut chunks = Vec::new();
    let mut on_output = |chunk: noel_env::OutputChunk<'_>| {
        chunks.push((
            chunk.stream,
            chunk.text.to_string(),
            chunk.raw.unwrap_or_default().to_vec(),
        ));
        Ok(())
    };
    let result = env
        .exec(
            command,
            ExecOptions {
                on_output: Some(&mut on_output),
                ..options
            },
            &cx(),
        )
        .await;
    (result, chunks)
}

fn text(chunks: &[(OutputStream, String, Vec<u8>)]) -> String {
    chunks.iter().map(|(_, text, _)| text.as_str()).collect()
}

#[tokio::test]
async fn merged_output_keeps_the_order_written_and_raw_bytes() {
    let env = LocalEnv::new(".");
    let (result, chunks) = run(
        &env,
        sh("printf 'a\\n'; printf 'b\\n' >&2; printf 'c\\377\\n'"),
        ExecOptions {
            merge_stderr: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.unwrap().exit_code, 0);
    assert!(chunks
        .iter()
        .all(|(stream, ..)| *stream == OutputStream::Stdout));
    assert_eq!(text(&chunks), "a\nb\nc\u{fffd}\n");
    let raw = chunks
        .iter()
        .flat_map(|(_, _, raw)| raw.clone())
        .collect::<Vec<_>>();
    assert_eq!(raw, b"a\nb\nc\xff\n");
}

#[tokio::test]
async fn variables_can_be_set_removed_or_the_only_ones() {
    let env = LocalEnv::new(".");
    let (result, chunks) = run(
        &env,
        sh("echo \"${HOME-unset} ${NOEL_ENV_TEST-unset}\""),
        ExecOptions {
            env: vec![
                ("HOME".into(), None),
                ("NOEL_ENV_TEST".into(), Some("set".into())),
            ],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.unwrap().exit_code, 0);
    assert_eq!(text(&chunks), "unset set\n");

    let (result, chunks) = run(
        &env,
        ShellCommand::Argv(vec!["/usr/bin/env".into()]),
        ExecOptions {
            env: vec![("ONLY".into(), Some("1".into()))],
            inherit_env: false,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.unwrap().exit_code, 0);
    assert_eq!(text(&chunks), "ONLY=1\n");
}

#[tokio::test]
async fn a_failing_callback_stops_the_command() {
    let env = LocalEnv::new(".");
    let started = std::time::Instant::now();
    let mut on_output = |_: noel_env::OutputChunk<'_>| Err("no more".to_string());
    let result = env
        .exec(
            sh("echo first; sleep 30"),
            ExecOptions {
                on_output: Some(&mut on_output),
                ..Default::default()
            },
            &cx(),
        )
        .await;
    let error = result.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (ExecutionErrorCode::CallbackError, "no more")
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn output_past_the_thresholds_spills_whole() {
    let env = LocalEnv::new(".");
    let (result, chunks) = run(
        &env,
        sh("seq 1 5000"),
        ExecOptions {
            spill: Some(SpillOptions {
                after_bytes: 1000,
                after_lines: 1_000_000,
            }),
            ..Default::default()
        },
    )
    .await;
    let result = result.unwrap();
    let spilled = result.spill_path.expect("spilled");
    let expected = (1..=5000).map(|n| format!("{n}\n")).collect::<String>();
    assert_eq!(text(&chunks), expected);
    assert_eq!(std::fs::read_to_string(&spilled).unwrap(), expected);
    assert!(spilled.contains("/tmp-") && spilled.ends_with(".log"));
    let name = spilled.rsplit('/').next().unwrap();
    assert!(name.starts_with("pi-output-"), "{name}");
    std::fs::remove_dir_all(std::path::Path::new(&spilled).parent().unwrap()).unwrap();

    let (result, _) = run(
        &env,
        sh("echo short"),
        ExecOptions {
            spill: Some(SpillOptions {
                after_bytes: 1000,
                after_lines: 10,
            }),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.unwrap().spill_path, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_kills_running_commands_which_settle_with_137() {
    let env = std::sync::Arc::new(LocalEnv::new("."));
    let running = tokio::spawn({
        let env = env.clone();
        async move {
            env.exec(sh("sleep 30"), ExecOptions::default(), &cx())
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    env.cleanup(&cx()).await;
    let result = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the command settles")
        .unwrap();
    assert_eq!(result.unwrap().exit_code, 137);
}

#[tokio::test]
async fn shells_working_directories_and_timeouts_are_checked_first() {
    let missing_shell = LocalEnv::with_options(
        ".",
        LocalEnvOptions {
            shell_path: Some("/nonexistent/sh".into()),
        },
    );
    let error = missing_shell
        .exec("echo".into(), ExecOptions::default(), &cx())
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::ShellUnavailable);
    assert_eq!(
        error.message,
        "Custom shell path not found: /nonexistent/sh"
    );

    let missing_cwd = LocalEnv::new("/nonexistent/noel-env-cwd");
    let error = missing_cwd
        .exec("echo".into(), ExecOptions::default(), &cx())
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
    assert_eq!(error.errno, None);
    assert_eq!(
        error.message,
        "Working directory does not exist: /nonexistent/noel-env-cwd\nCannot execute bash commands."
    );

    let env = LocalEnv::new(".");
    let error = env
        .exec(
            "echo".into(),
            ExecOptions {
                timeout: Some(Duration::ZERO),
                ..Default::default()
            },
            &cx(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Timeout);
    // A string runs through the shell.
    let (result, chunks) = run(&env, "echo $((1 + 2))".into(), ExecOptions::default()).await;
    assert_eq!(result.unwrap().exit_code, 0);
    assert_eq!(text(&chunks), "3\n");
}

#[tokio::test]
async fn watch_and_list_files_are_not_supported_yet() {
    let env = LocalEnv::new(".");
    let listed = env
        .list_files(".", &Default::default(), &cx())
        .await
        .unwrap_err();
    assert_eq!(listed.code, FileErrorCode::NotSupported);
    let watched = env
        .watch(&[], std::sync::Arc::new(|_| {}), &cx())
        .await
        .err()
        .unwrap();
    assert_eq!(watched.code, FileErrorCode::NotSupported);
}

use std::cell::RefCell;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::{self, Cursor, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod account_failover;
mod attach;
mod auth_command;
mod auth_commands;
mod bug_report;
mod bun_install;
mod clipboard;
mod image_command;
mod markdown;
mod media;
mod model_refresh;
mod model_select;
mod oauth_login;
mod oauth_refresh;
mod package_command;
mod provider;
mod resource_commands;
mod rpc;
mod rpc_wire;
mod selectors;
mod server;
mod session_commands;
mod signals;
mod transcript_view;
mod tui;
mod usage;
mod workspace;
#[allow(unused_imports)]
pub use auth_commands::*;
#[allow(unused_imports)]
pub use image_command::*;
#[allow(unused_imports)]
pub use media::*;
#[allow(unused_imports)]
pub use model_refresh::*;
use model_select::{
    configured_model, effective_thinking_level, find_selectable_model, model_definition,
    normalized_thinking_level, provider_usable, register_cli_model, resolve_cli,
    resolve_cli_selection, resolve_scope, select_initial_model, set_thinking_level, switch_model,
    thinking_label,
};
#[allow(unused_imports)]
pub use package_command::*;
#[allow(unused_imports)]
pub use provider::*;
#[allow(unused_imports)]
pub use selectors::*;
use transcript_view::TranscriptView;
#[allow(unused_imports)]
pub use tui::*;

use anyhow::{anyhow, Result};
use base64::Engine;
use clap::{CommandFactory, Parser, ValueEnum};
use crossterm::{
    cursor::MoveTo,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear as TerminalClear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use noel_ai::{
    create_provider_with_client, generate_images, set_claude_code_version, ImageGenerationInput,
    ImageGenerationOutput, ImageProviderApi as AiImageProviderApi, ImageProviderConfig, MediaInput,
    ModelRef, ProviderApi as AiProviderApi, ProviderAuth, ProviderConfig, ProviderError,
    ProviderRequest, StreamEvent,
};
use noel_config::{
    apply_tool_modifiers, auth_for_provider, codex_client_version, is_tool_modifier,
    listed_accounts_for_provider, load_config, load_config_with_project_trust,
    load_config_with_resources, project_is_trusted, read_model_cache, resolve_default_tools,
    save_project_trust, tool_list_error, write_file_atomic, write_model_cache, AccountSource,
    AuthCredential, AuthData, CompactionSettings, ConfigPaths, ImageModelDefinition,
    ImageProviderApi as ConfigImageProviderApi, ImageSettings, LoadedConfig, ModelCache,
    ModelDefinition, ModelRefreshSettings, PackageSource, ProviderApi as ConfigProviderApi,
    ResolvedAuth, ResourceFile, RetrySettings, Settings, TerminalSettings, WarningSettings,
    DEFAULT_ACCOUNT_NAME, ENV_SESSION_DIR, MODEL_CACHE_VERSION,
};
use noel_core::{
    default_active_tool_names, expand_input, format_todo_list, run_excluded_bash_result,
    run_user_turn, run_user_turn_streaming_with_media, write_session_export, ReloadReport,
    ReloadableSystems, Runtime, SessionError, SessionState, SessionStore, SessionWorkspace,
    SteeringMailbox, TodoItem, TodoStatus, PRODUCT_NAME,
};
use noel_tui::{
    parse_theme_color, EditorState, Keybinding as TuiKeybinding, KeybindingMap, Selector,
    SelectorItem, SessionView, SettingsView, TerminalRenderer, TerminalTheme, ThemePalette,
    BUILTIN_THEMES,
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget, Wrap},
    Frame, Terminal, TerminalOptions, Viewport,
};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, ValueEnum)]
enum OutputMode {
    Text,
    Json,
    Rpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TuiMode {
    /// Owned alternate screen with a fixed composer (default).
    Fullscreen,
    /// Native inline scrollback owned by the terminal.
    Regular,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "noel")]
#[command(version)]
#[command(about = "Native Rust CLI for noel")]
struct Cli {
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    mode: OutputMode,

    #[arg(short = 'p', long)]
    print: bool,

    #[arg(short = 'c', long)]
    r#continue: bool,

    #[arg(short = 'r', long)]
    resume: bool,

    #[arg(long)]
    fork: Option<String>,

    #[arg(long)]
    no_session: bool,

    #[arg(long)]
    session: Option<String>,

    #[arg(long)]
    session_id: Option<String>,

    #[arg(long)]
    session_dir: Option<PathBuf>,

    /// Open the TUI on a session that `noel server` holds, starting the
    /// server when none listens. The session runs with its own settings, so
    /// the options that shape a local session are refused with it.
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with_all = [
            "print", "continue", "resume", "fork", "no_session", "session",
            "session_id", "export", "messages", "session_dir", "provider",
            "model", "models", "api_key", "thinking", "system_prompt",
            "append_system_prompt", "no_tools", "tools", "exclude_tools",
            "no_builtin_tools", "skill", "no_skills", "prompt_template",
            "no_prompt_templates", "no_context_files", "extension",
            "no_extensions", "no_mcp", "image", "name", "approve", "no_approve",
            "offline", "workspace",
        ]
    )]
    attach: Option<String>,

    /// With --attach: the session is on another machine's noel server,
    /// reached through `ssh <DEST> noel bridge`.
    #[arg(long, value_name = "DEST")]
    remote: Option<String>,

    /// With --remote: the noel program on that machine (default: `noel` on
    /// its PATH; a leading ~/ is its home).
    #[arg(long, value_name = "PATH", requires = "remote")]
    remote_noel: Option<String>,

    /// Run the session's tools in a directory on another machine, over ssh:
    /// ssh://[user@]host[:port]/absolute/path (add ?login=1 to start them
    /// through the remote login shell). The session keeps it; trust the host
    /// first with `noel workspace trust ssh://host[:port]`.
    #[arg(long, value_name = "URL")]
    workspace: Option<String>,

    #[arg(long)]
    provider: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long, value_delimiter = ',')]
    models: Vec<String>,

    #[arg(long)]
    api_key: Option<String>,

    #[arg(long)]
    thinking: Option<String>,

    #[arg(long, num_args = 0..=1, value_name = "SEARCH")]
    list_models: Option<Option<String>>,

    #[arg(long)]
    system_prompt: Option<String>,

    #[arg(long)]
    append_system_prompt: Vec<String>,

    #[arg(long)]
    no_tools: bool,

    /// An allowlist of tool names and `*` patterns, or only `+name` and
    /// `-name` entries that change the default selection.
    #[arg(short = 't', long, value_delimiter = ',', allow_hyphen_values = true)]
    tools: Vec<String>,

    #[arg(long, value_delimiter = ',')]
    exclude_tools: Vec<String>,

    #[arg(long)]
    no_builtin_tools: bool,

    /// Load a skill file or directory (can be used multiple times).
    #[arg(long)]
    skill: Vec<PathBuf>,

    /// Discover no skills; --skill paths still load.
    #[arg(long)]
    no_skills: bool,

    /// Skip the startup splash (the logo); transcript-mechanics tests and
    /// scripted callers want the plain banner.
    #[arg(long)]
    no_splash: bool,

    /// Load a prompt template file or directory (can be used multiple times).
    #[arg(long)]
    prompt_template: Vec<PathBuf>,

    /// Discover no prompt templates; --prompt-template paths still load.
    #[arg(long)]
    no_prompt_templates: bool,

    #[arg(long)]
    theme: Option<String>,

    /// Select the initial interactive theme for this run without saving it.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["theme", "no_themes"])]
    use_theme: Option<String>,

    #[arg(long)]
    no_themes: bool,

    #[arg(long)]
    no_context_files: bool,

    #[arg(short = 'e', long = "extension")]
    extension: Vec<PathBuf>,

    #[arg(long)]
    no_extensions: bool,

    /// Accepted for upstream compatibility; noel has no MCP support, so this is a no-op.
    #[arg(long)]
    no_mcp: bool,

    #[arg(long)]
    image: Vec<PathBuf>,

    #[arg(long)]
    export: Option<PathBuf>,

    #[arg(long)]
    verbose: bool,

    #[arg(short = 'n', long)]
    name: Option<String>,

    #[arg(short = 'a', long)]
    approve: bool,

    #[arg(long)]
    no_approve: bool,

    /// Use native inline scrollback instead of the fixed-composer owned screen.
    #[arg(long)]
    no_alt_screen: bool,

    /// TUI mode: fullscreen (default) or regular; regular equals --no-alt-screen.
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        conflicts_with = "no_alt_screen"
    )]
    tui_mode: Option<TuiMode>,

    #[arg(long)]
    offline: bool,

    #[arg()]
    messages: Vec<String>,
}

/// The paths a command works with. The first command that asks for noel's
/// default directory imports pi's state into it (`ConfigPaths::discover`),
/// which is reported here. Every command asks only once its arguments are
/// parsed, so `--version`, `--help`, and a usage error write nothing.
pub(crate) fn config_paths(cwd: PathBuf, session_dir: Option<PathBuf>) -> Result<ConfigPaths> {
    let (paths, import) = ConfigPaths::discover(cwd, session_dir)?;
    if let Some(import) = import {
        let imported = if import.imported.is_empty() {
            "nothing".to_string()
        } else {
            import.imported.join(", ")
        };
        eprintln!(
            "noel: imported {imported} from {} into {}",
            import.from.display(),
            import.to.display()
        );
        for (entry, reason) in &import.skipped {
            eprintln!("noel: did not import {entry}: {reason}");
        }
    }
    Ok(paths)
}

fn main() -> std::process::ExitCode {
    // Upstream's process markers, inherited by every command and tool noel
    // starts: a generic one naming the agent, and its own, which noel sets
    // under its name and under pi's for what was written for pi. Set before
    // any other thread exists, because the environment is process-wide.
    std::env::set_var("AI_AGENT", PRODUCT_NAME);
    std::env::set_var("NOEL_CODING_AGENT", "true");
    std::env::set_var("PI_CODING_AGENT", "true");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let result = match runtime {
        Ok(runtime) => {
            let result = runtime.block_on(run());
            // A blocking task may wait on input that never comes (a reader
            // of stdin): the process does not wait for it long.
            runtime.shutdown_timeout(Duration::from_millis(500));
            result
        }
        Err(error) => Err(error.into()),
    };
    // Every exit comes back here (#179): what `run` holds is dropped first,
    // so journals are written, children reaped and the terminal restored.
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => match error.downcast_ref::<ExitStatus>() {
            Some(ExitStatus(code)) => exit_code(*code),
            None => {
                eprintln!("Error: {error:?}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

/// An exit with `code`, carried up to `main` as an error so everything on
/// the way is dropped (#179) instead of `std::process::exit` skipping it.
#[derive(Debug)]
pub(crate) struct ExitStatus(pub(crate) i32);

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "exit status {}", self.0)
    }
}

impl std::error::Error for ExitStatus {}

/// `Ok` for status 0, else the status carried up to `main`.
pub(crate) fn exit_with(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(ExitStatus(code).into())
    }
}

fn exit_code(code: i32) -> std::process::ExitCode {
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn run() -> Result<()> {
    // `noel server`, `noel worker` and `noel client` (Mode 2) handle their
    // own signals.
    if let Some(code) = server::try_run(&std::env::args().collect::<Vec<_>>()).await {
        return exit_with(code);
    }
    if let Some(code) = workspace::try_run(&std::env::args().collect::<Vec<_>>()).await {
        return exit_with(code);
    }
    signals::install();
    if try_run_image_command().await? {
        return Ok(());
    }
    if try_run_accounts_command().await? {
        return Ok(());
    }
    if try_run_package_command().await? {
        return Ok(());
    }
    let cli = Cli::parse_from(normalized_cli_args(std::env::args()));
    // Not `requires`: clap waives it when an option --attach conflicts with
    // is given, and --remote would then be ignored.
    if cli.remote.is_some() && cli.attach.is_none() {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--remote needs --attach <SESSION_ID> (or use `noel client --remote`)",
            )
            .exit();
    }
    validate_session_id_flags(&cli)?;
    if matches!(cli.mode, OutputMode::Rpc) && (cli.print || !cli.messages.is_empty()) {
        return Err(anyhow!(
            "--mode rpc reads its prompts from stdin and takes no prompt arguments"
        ));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd.clone(), cli.session_dir.clone())?;
    let project_trusted = if cli.approve {
        true
    } else if cli.no_approve {
        false
    } else {
        project_is_trusted(&paths.agent_dir, &cwd)?
    };
    let resources = resource_commands::resource_options(&cli);
    let mut config = load_config_with_resources(paths, project_trusted, &resources)?;
    if cli.session_dir.is_none() && std::env::var_os(ENV_SESSION_DIR).is_none() {
        if let Some(session_dir) = &config.settings.session_dir {
            config.paths = config.paths.with_session_dir(session_dir)?;
        }
    }
    apply_cli_overrides(&cli, &cwd, &mut config)?;
    let _ = CONFIG_SOURCE.set(ConfigSource {
        cli: cli.clone(),
        cwd: cwd.clone(),
        project_trusted,
    });
    if cli.verbose {
        eprintln!("agent dir: {}", config.paths.agent_dir.display());
        eprintln!("session dir: {}", config.paths.session_dir.display());
    }
    // The remote backend: the session runs in its worker, so nothing below
    // (a local runtime, its model, its session) applies.
    if let Some(session) = &cli.attach {
        if !matches!(cli.mode, OutputMode::Text) || !io::stdin().is_terminal() {
            return Err(anyhow!("--attach opens the TUI and needs a terminal"));
        }
        let endpoint = server::client::Endpoint::new(cli.remote.clone(), cli.remote_noel.clone())?;
        if attach::run(session, endpoint, config, cli.owned_screen(), false, true)
            .await?
            .is_some()
        {
            restart_current_process(None)?;
        }
        return Ok(());
    }
    let offline = offline_enabled(cli.offline);
    apply_cached_claude_code_version(&config);
    start_model_refresh(&config, offline, cli.verbose);

    if let Some(search) = &cli.list_models {
        let search = search.as_deref().map(str::to_lowercase);
        for model in sorted_models(&config.models) {
            if let Some(search) = &search {
                let display = format!(
                    "{}/{} {} {:?}",
                    model.provider,
                    model.id,
                    model.name.as_deref().unwrap_or_default(),
                    model.api
                )
                .to_lowercase();
                if !display.contains(search) {
                    continue;
                }
            }
            println!("{}/{}\t{:?}", model.provider, model.id, model.api);
        }
        return Ok(());
    }

    // A workspace on another machine: checked (a valid URL, a trusted host)
    // before any session is made for it.
    let requested_workspace = cli
        .workspace
        .as_deref()
        .map(|url| workspace::from_flag(url, &config.paths.agent_dir))
        .transpose()?;
    let selection = resolve_cli_selection(&cli, &mut config, offline)?;
    let mut systems = ReloadableSystems::from_config(&config, 1);
    systems.offline = offline;
    // Interactive `noel` as a client: a session its worker already holds
    // (a client gone without quitting) is attached to, not waited for.
    let mut opened = None;
    if io::stdin().is_terminal()
        && !cli.print
        && cli.messages.is_empty()
        && !matches!(cli.mode, OutputMode::Rpc)
    {
        // The server reads the same sessions: a --session-dir is its too
        // (and names its runtime directory).
        if cli.session_dir.is_some() {
            std::env::set_var(ENV_SESSION_DIR, &config.paths.session_dir);
        }
        match create_runtime(
            &cli,
            &cwd,
            &config,
            systems.clone(),
            requested_workspace.as_ref(),
        ) {
            Ok(runtime) => opened = Some(runtime),
            Err(error) => {
                if let Some(SessionError::Locked {
                    path,
                    pid: Some(pid),
                }) = error.downcast_ref::<SessionError>()
                {
                    if let Some(session) = held_by_worker(path, *pid) {
                        offer_bun_install(&config).await?;
                        return run_client_tui(session, Some(path.clone()), config, &cli).await;
                    }
                }
            }
        }
    }
    let mut runtime = match opened {
        Some(runtime) => runtime,
        None => {
            create_runtime_when_unlocked(&cli, &cwd, &config, systems, requested_workspace.as_ref())
                .await?
        }
    };
    workspace::check_saved(runtime.session(), &config.paths.agent_dir)?;
    workspace::install(&mut runtime, &config.paths.agent_dir, !cli.no_context_files);
    select_initial_model(&mut runtime, &config, &selection, offline)?;
    // The tool flags apply to this run (`apply_tool_flags`). Earlier
    // versions saved them in the session as disabled tools; a resumed
    // session follows this run's flags instead.
    if !runtime.session().disabled_tools.is_empty() {
        runtime.set_disabled_tools(BTreeSet::new())?;
    }
    if let Some(name) = &cli.name {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("--name requires a non-empty value"));
        }
        runtime.rename_session(Some(name.to_string()))?;
    }

    let stdin_is_terminal = io::stdin().is_terminal();
    if matches!(cli.mode, OutputMode::Rpc) && !stdin_is_terminal {
        return rpc::run(runtime, config, offline).await;
    }

    let mut initial_prompt = expand_message_inputs(&cwd, &cli.messages)?;
    let initial_media = load_media_inputs(&cwd, &cli.image, &config)?;
    if !stdin_is_terminal && !matches!(cli.mode, OutputMode::Rpc) {
        let mut stdin = String::new();
        io::stdin().read_to_string(&mut stdin)?;
        initial_prompt = [initial_prompt, stdin.trim().to_string()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    if cli.print || !initial_prompt.is_empty() || !stdin_is_terminal {
        if initial_prompt.is_empty() {
            return Err(anyhow!("print mode requires a prompt"));
        }
        let prompt = expand_input(&config, &initial_prompt);
        let response =
            run_prompt_media(&mut runtime, &config, prompt, initial_media, offline).await?;
        if let Some(path) = &cli.export {
            export_session(&runtime, path)?;
        }
        print_response(&cli.mode, &response);
        return Ok(());
    }

    offer_bun_install(&config).await?;

    // --no-session: the worker still needs a journal; this one is kept
    // in the runtime directory only while this `noel` runs.
    let ephemeral = runtime.store().is_none();
    let (session, journal) = match runtime.store() {
        Some(store) => (
            runtime.session().session_id.clone(),
            store.path().to_path_buf(),
        ),
        None => ephemeral_journal(&runtime)?,
    };
    // The worker opens the journal: this process lets go of it first.
    drop(runtime);
    let result = run_client_tui(session.clone(), Some(journal.clone()), config, &cli).await;
    if ephemeral {
        let _ = fs::remove_file(&journal);
        if let Ok(dir) = server::RuntimeDir::check(server::RuntimeDir::locate()) {
            let _ = fs::remove_file(dir.worker_journal(&session));
        }
    }
    result
}

/// Interactive `noel` as a client: the session chosen and prepared as for
/// the in-process TUI, then left to its worker under the local server, and
/// the TUI attached to it.
async fn run_client_tui(
    session: String,
    journal: Option<PathBuf>,
    config: LoadedConfig,
    cli: &Cli,
) -> Result<()> {
    let dir = server::RuntimeDir::ensure(server::RuntimeDir::locate())?;
    dir.ensure_workers()?;
    // A journal outside the session directory (--session <path>) is
    // registered with the server, which otherwise looks there.
    let registered = dir.worker_journal(&session);
    match journal.and_then(|path| fs::canonicalize(path).ok()) {
        Some(path)
            if fs::canonicalize(config.paths.session_dir.join(format!("{session}.jsonl")))
                .ok()
                .as_ref()
                != Some(&path) =>
        {
            fs::write(&registered, format!("{}\n", path.display()))?;
        }
        _ => {
            let _ = fs::remove_file(&registered);
        }
    }
    let notice = hand_over_flags(&session, false)?;
    if let Some(notice) = notice {
        eprintln!("noel: {notice}");
    }
    let endpoint = server::client::Endpoint::new(None, None)?;
    let session_dir = config.paths.session_dir.clone();
    if let Some(session) = attach::run(
        &session,
        endpoint,
        config,
        cli.owned_screen(),
        true,
        cli.no_splash,
    )
    .await?
    {
        // The new image reopens the session shown last, by its journal.
        let journal = fs::read_to_string(dir.worker_journal(&session))
            .ok()
            .map(|text| PathBuf::from(text.trim_end()))
            .unwrap_or_else(|| session_dir.join(format!("{session}.jsonl")));
        restart_current_process(Some(&journal))?;
    }
    Ok(())
}

/// Leaves this run's flags and environment for `session`'s worker when it
/// starts now; a running one keeps its own, and the notice says so
/// (docs/client-tui-plan.md). A session `switched` to (not the one this
/// run opened) keeps its own model and thinking level, as `/resume` does.
pub(crate) fn hand_over_flags(session: &str, switched: bool) -> Result<Option<String>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = server::RuntimeDir::ensure(server::RuntimeDir::locate())?;
    dir.ensure_workers()?;
    let running = fs::read_to_string(dir.worker_applied_args(session))
        .ok()
        .filter(|_| dir.worker_socket(session).exists())
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok());
    Ok(match running {
        Some(running) if running != args => Some(format!(
            "this session's worker runs with other flags ({}); quit every client to start it with yours",
            if running.is_empty() { "none".to_owned() } else { running.join(" ") }
        )),
        Some(_) => None,
        None => {
            // With this process's environment, which the worker takes on;
            // the file is the user's alone, as the directory is.
            let env: serde_json::Map<String, serde_json::Value> = std::env::vars()
                .map(|(key, value)| (key, serde_json::Value::String(value)))
                .collect();
            let path = dir.worker_args(session);
            let cwd = std::env::current_dir().unwrap_or_default();
            let launch =
                serde_json::json!({"args": args, "env": env, "cwd": cwd, "switched": switched});
            let mut file = {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&path)?
            };
            file.write_all(launch.to_string().as_bytes())?;
            None
        }
    })
}

/// A journal for a `--no-session` run in client mode, in the runtime
/// directory: a new session with `runtime`'s cwd, workspace and model.
fn ephemeral_journal(runtime: &Runtime) -> Result<(String, PathBuf)> {
    let dir = server::RuntimeDir::ensure(server::RuntimeDir::locate())?;
    let base = dir.root().join("ephemeral");
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)?;
    }
    let session = runtime.session();
    let (store, state) =
        SessionStore::create_with_workspace(&base, session.cwd.clone(), session.workspace.clone())?;
    store.record_active_model(session.active_model.clone())?;
    store.record_active_thinking_level(session.active_thinking_level.clone())?;
    store.record_active_account(session.active_account.clone())?;
    Ok((state.session_id, store.path().to_path_buf()))
}

/// The session whose journal at `path` its running worker holds, if one
/// does: the lock's holder `pid` is the worker in the session's record.
pub(crate) fn held_by_worker(path: &Path, pid: u32) -> Option<String> {
    let session = SessionStore::read(path).ok()?.session_id;
    let dir = server::RuntimeDir::check(server::RuntimeDir::locate()).ok()?;
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.worker_record(&session)).ok()?).ok()?;
    (record["pid"].as_u64() == Some(u64::from(pid))).then_some(session)
}

/// Before the TUI opens (the terminal is still plain), offer to install
/// the bun runtime when a TypeScript extension is loaded and no bun can be
/// found: one y/n, one download, and the extensions work. Refusing keeps
/// the fail-closed behavior - the extensions' diagnostics say how to
/// install later. Non-interactive modes never reach this.
async fn offer_bun_install(config: &LoadedConfig) -> Result<()> {
    let typescript = config.extensions.iter().any(|extension| {
        extension.path.extension().and_then(|e| e.to_str()) == Some("ts")
            && noel_core::extensions::manifest_protocol_version(&extension.path) == Some(2)
    });
    if !typescript || noel_core::extensions::find_bun_binary().is_some() {
        return Ok(());
    }
    println!("a TypeScript extension is loaded but bun was not found.",);
    println!(
        "noel can download bun (~50 MB) into {} — the extensions run under it.",
        config.paths.agent_dir.join("bin").display()
    );
    print!("download bun now? [y/N] ");
    use std::io::Write as _;
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        println!("skipped; run `noel bun install` any time");
        return Ok(());
    }
    bun_install::install_bun(&config.paths.agent_dir, |_| Ok(true), true).await?;
    Ok(())
}

/// How startup built this process's config: the project trust decision and
/// the command line. `/reload` and `/models refresh|reset` rebuild the config
/// the same way, so a reload neither trusts a project the session did not
/// trust nor drops `--system-prompt`, `-e`, `--api-key`, and the other
/// command-line overrides. RPC mode never reloads: it keeps the config
/// startup built.
#[derive(Debug, Clone)]
struct ConfigSource {
    cli: Cli,
    cwd: PathBuf,
    project_trusted: bool,
}

static CONFIG_SOURCE: std::sync::OnceLock<ConfigSource> = std::sync::OnceLock::new();

impl ConfigSource {
    fn load(&self, paths: ConfigPaths) -> Result<LoadedConfig> {
        let resources = resource_commands::resource_options(&self.cli);
        let mut config = load_config_with_resources(paths, self.project_trusted, &resources)?;
        apply_cli_overrides(&self.cli, &self.cwd, &mut config)?;
        // A custom model `--model provider/new-id` named stays known.
        register_cli_model(&self.cli, &mut config, offline_enabled(self.cli.offline));
        Ok(config)
    }
}

/// Reloads the config the way startup built it (see [`ConfigSource`]).
fn reload_config(config: &LoadedConfig) -> Result<LoadedConfig> {
    match CONFIG_SOURCE.get() {
        Some(source) => source.load(config.paths.clone()),
        // Only startup records a source. Without one there are no overrides
        // to keep, and a project counts as trusted only by a saved decision.
        None => Ok(load_config_with_project_trust(
            config.paths.clone(),
            project_is_trusted(&config.paths.agent_dir, &config.paths.cwd)?,
        )?),
    }
}

/// Folds the tool flags into the tool settings, as upstream resolves them.
/// `--tools` with plain names is an allowlist of names and `*` patterns;
/// with only `+name`/`-name` entries it changes the default selection.
/// `--no-builtin-tools` empties the default selection, which leaves the
/// extension tools; `--no-tools` leaves only what `--tools` names. Any of
/// the three replaces the `enabledTools` setting for the run.
fn apply_tool_flags(cli: &Cli, settings: &mut Settings) -> Result<()> {
    let entries = |values: &[String]| {
        values
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
    };
    let tools = entries(&cli.tools);
    if let Some(error) = tool_list_error(&tools) {
        return Err(anyhow!("--tools: {error}"));
    }
    if cli.no_tools || cli.no_builtin_tools || !tools.is_empty() {
        // The flags decide for this run, not the saved allowlist.
        settings.enabled_tools = None;
    }
    if cli.no_tools || cli.no_builtin_tools {
        settings.default_tools = Some(Vec::new());
    }
    if tools.iter().any(|entry| is_tool_modifier(entry)) {
        let builtin = default_active_tool_names().into_iter().collect::<Vec<_>>();
        let defaults = match &settings.default_tools {
            Some(entries) => resolve_default_tools(entries, &builtin),
            None => builtin,
        };
        let selected = apply_tool_modifiers(&defaults, &tools);
        if cli.no_tools {
            settings.enabled_tools = Some(selected.clone());
        }
        settings.default_tools = Some(selected);
    } else if !tools.is_empty() {
        settings.enabled_tools = Some(tools);
    } else if cli.no_tools {
        settings.enabled_tools = Some(Vec::new());
    }
    settings.excluded_tools = entries(&cli.exclude_tools);
    Ok(())
}

fn apply_cli_overrides(cli: &Cli, cwd: &Path, config: &mut LoadedConfig) -> Result<()> {
    if let Some(system_prompt) = &cli.system_prompt {
        config.system_prompt = Some(resolve_prompt_flag(cwd, system_prompt)?);
    }
    // As upstream, the flags replace a discovered APPEND_SYSTEM.md; they add
    // to the `appendSystemPrompt` setting, which the config loaded instead
    // of that file.
    if !cli.append_system_prompt.is_empty() && config.settings.append_system_prompt.is_empty() {
        config.append_system_prompt.clear();
    }
    for prompt in &cli.append_system_prompt {
        config
            .append_system_prompt
            .push(resolve_prompt_flag(cwd, prompt)?);
    }
    if cli.no_context_files {
        config.context_files.clear();
    }
    apply_tool_flags(cli, &mut config.settings)?;
    if cli.no_themes {
        config.settings.theme = None;
        config.settings.accent_color = None;
        config.themes.clear();
    } else if let Some(theme) = cli.theme.as_ref().or(cli.use_theme.as_ref()) {
        // Either flag selects the theme for this run only (clap rejects both
        // together); --use-theme takes one name, not upstream's light/dark pair.
        if cli.use_theme.is_some() && theme.contains('/') {
            return Err(anyhow!(
                "--use-theme {theme}: light/dark theme pairs are not supported; pass one theme name"
            ));
        }
        let resolved = resolve_terminal_theme(config, theme)?;
        config.settings.theme = Some(resolved.name.clone());
        resolved
            .with_accent(config.settings.accent_color.as_deref())
            .map_err(|error| anyhow!(error))?;
    }
    if cli.no_extensions {
        config.extensions.clear();
    }
    for extension in &cli.extension {
        if extension.is_file() {
            config.extensions.push(ResourceFile {
                name: resource_name(extension),
                path: extension.clone(),
                content: fs::read_to_string(extension)?,
            });
        }
    }
    if let Some(api_key) = &cli.api_key {
        let provider = infer_cli_provider(cli, config).ok_or_else(|| {
            anyhow!("--api-key requires --provider, --model, or configured default provider")
        })?;
        // A key given on the command line serves this run; failover would
        // pool it with the stored accounts and could carry on with another.
        config.settings.failover = None;
        config.auth.insert(
            &provider,
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::ApiKey {
                key: api_key.clone(),
            },
        );
    }
    // `--models`, else `enabledModels`: a scope over the models that can
    // run with the credentials now known. It removes no model.
    config.model_scope = resolve_scope(cli, config, offline_enabled(cli.offline)).models;
    Ok(())
}

fn normalized_cli_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    args.into_iter()
        .map(|arg| match arg.as_str() {
            "-nt" => "--no-tools".to_string(),
            "-nbt" => "--no-builtin-tools".to_string(),
            "-ne" => "--no-extensions".to_string(),
            "-ns" => "--no-skills".to_string(),
            "-np" => "--no-prompt-templates".to_string(),
            "-nc" => "--no-context-files".to_string(),
            "-xt" => "--exclude-tools".to_string(),
            "-na" => "--no-approve".to_string(),
            "-v" => "--version".to_string(),
            _ => arg,
        })
        .collect()
}

fn validate_session_id_flags(cli: &Cli) -> Result<()> {
    let Some(session_id) = &cli.session_id else {
        return Ok(());
    };
    if cli.session.is_some() || cli.r#continue || cli.resume {
        return Err(anyhow!(
            "--session-id cannot be combined with --session, --continue, or --resume"
        ));
    }
    let bytes = session_id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(anyhow!(
            "session id must be non-empty, use alphanumeric characters, '-', '_', or '.', and start and end with an alphanumeric character"
        ));
    }
    Ok(())
}

fn offline_enabled(cli_offline: bool) -> bool {
    cli_offline
        || std::env::var("NOEL_OFFLINE")
            .ok()
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
}

fn apply_cached_claude_code_version(config: &LoadedConfig) {
    if let Ok(Some(cache)) = read_model_cache(&config.paths.model_cache_path) {
        if let Some(version) = cache.claude_code_version {
            set_claude_code_version(version);
        }
    }
}

fn unix_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

/// The provider `--api-key` is for: the one `--model` resolves to, else
/// `--provider`, else the saved default provider.
fn infer_cli_provider(cli: &Cli, config: &LoadedConfig) -> Option<String> {
    resolve_cli(cli, config, offline_enabled(cli.offline))
        .and_then(|resolved| resolved.model().cloned())
        .map(|model| model.provider)
        .or_else(|| {
            cli.provider
                .as_deref()
                .map(|provider| noel_config::canonical_provider(provider).to_string())
        })
        .or_else(|| config.settings.default_provider.clone())
}

fn resource_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("resource")
        .to_string()
}

fn resolve_text_or_file(cwd: &Path, value: &str) -> Result<String> {
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if path.exists() {
        return fs::read_to_string(path).map_err(Into::into);
    }
    Ok(value.to_string())
}

/// A prompt flag's value: the text itself, or the file it names, read as
/// upstream reads a prompt file (without a leading byte order mark).
fn resolve_prompt_flag(cwd: &Path, value: &str) -> Result<String> {
    let text = resolve_text_or_file(cwd, value)?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(text) => text.to_string(),
        None => text,
    })
}

/// How long startup waits for another noel process to release its session.
const SESSION_LOCK_WAIT: Duration = Duration::from_secs(5);
const SESSION_LOCK_POLL: Duration = Duration::from_millis(100);
/// `EX_TEMPFAIL`: the session is busy, so running again later can succeed.
const EXIT_SESSION_LOCKED: i32 = 75;

/// Opens the startup session, waiting up to [`SESSION_LOCK_WAIT`] while
/// another noel process holds it. Print, json, rpc, and interactive modes all
/// start here, before any terminal setup. A session that stays locked exits
/// with [`EXIT_SESSION_LOCKED`] and a message naming the path and holder pid.
async fn create_runtime_when_unlocked(
    cli: &Cli,
    cwd: &Path,
    config: &LoadedConfig,
    systems: ReloadableSystems,
    workspace: Option<&SessionWorkspace>,
) -> Result<Runtime> {
    let deadline = Instant::now() + SESSION_LOCK_WAIT;
    loop {
        let error = match create_runtime(cli, cwd, config, systems.clone(), workspace) {
            Ok(runtime) => return Ok(runtime),
            Err(error) => error,
        };
        if !matches!(
            error.downcast_ref::<SessionError>(),
            Some(SessionError::Locked { .. })
        ) {
            return Err(error);
        }
        if Instant::now() >= deadline {
            eprintln!(
                "noel: {error}; gave up after waiting {}s",
                SESSION_LOCK_WAIT.as_secs()
            );
            return Err(ExitStatus(EXIT_SESSION_LOCKED).into());
        }
        tokio::time::sleep(SESSION_LOCK_POLL).await;
    }
}

/// Opens or creates the startup session. A new session runs its tools in
/// `workspace` (`--workspace`), saved in its journal; an existing one keeps
/// its own, and `--workspace` naming another is refused.
fn create_runtime(
    cli: &Cli,
    cwd: &Path,
    config: &LoadedConfig,
    systems: ReloadableSystems,
    workspace: Option<&SessionWorkspace>,
) -> Result<Runtime> {
    if cli.no_session {
        let mut state = SessionState::new("ephemeral", cwd.to_path_buf());
        state.workspace = workspace.cloned();
        return Ok(Runtime::new(state, systems));
    }
    let opened = |store: SessionStore, state: SessionState| -> Result<Runtime> {
        workspace::check_reopened(&state, workspace)?;
        Ok(Runtime::with_store(state, systems.clone(), store))
    };

    if let Some(reference) = &cli.fork {
        let path = resolve_session_reference(&config.paths.session_dir, reference)?;
        let source_state = SessionStore::read(&path)?;
        // A fork keeps its source's workspace.
        workspace::check_reopened(&source_state, workspace)?;
        let (store, state) = SessionStore::fork(&config.paths.session_dir, &source_state, false)?;
        return Ok(Runtime::with_store(state, systems, store));
    }

    if let Some(session_id) = &cli.session_id {
        let path = config.paths.session_dir.join(format!("{session_id}.jsonl"));
        if path.exists() {
            let (store, state) = SessionStore::open(path)?;
            return opened(store, state);
        }
        let (store, state) = match workspace {
            Some(workspace) => SessionStore::create_in_workspace(
                &config.paths.session_dir,
                cwd.to_path_buf(),
                session_id,
                workspace.clone(),
            )?,
            None => SessionStore::create_with_id(
                &config.paths.session_dir,
                cwd.to_path_buf(),
                session_id,
            )?,
        };
        return Ok(Runtime::with_store(state, systems, store));
    }

    if let Some(reference) = &cli.session {
        let path = resolve_session_reference(&config.paths.session_dir, reference)?;
        let (store, state) = SessionStore::open(path)?;
        return opened(store, state);
    }

    if cli.r#continue {
        if let Some(path) = most_recent_session(&config.paths.session_dir, Some(cwd))? {
            let (store, state) = SessionStore::open(path)?;
            return opened(store, state);
        }
    }

    if cli.resume {
        if let Some(path) = most_recent_session(&config.paths.session_dir, None)? {
            let (store, state) = SessionStore::open(path)?;
            return opened(store, state);
        }
    }

    let (store, state) = SessionStore::create_with_workspace(
        &config.paths.session_dir,
        cwd.to_path_buf(),
        workspace.cloned(),
    )?;
    Ok(Runtime::with_store(state, systems, store))
}

/// Start fresh conversation state without changing the current selections.
/// Persist them before replacing the runtime so reopen/resume agrees with the UI.
fn start_new_session(runtime: &mut Runtime, config: &LoadedConfig) -> Result<()> {
    let (store, state) = session_commands::new_session_like(runtime, config)?;
    runtime.replace_session(state, Some(store));
    Ok(())
}

fn resolve_session_reference(session_dir: &Path, reference: &str) -> Result<PathBuf> {
    SessionStore::resolve(session_dir, reference)?
        .ok_or_else(|| anyhow!("session not found or ambiguous: {reference}"))
}

fn most_recent_session(session_dir: &Path, cwd: Option<&Path>) -> Result<Option<PathBuf>> {
    let mut sessions = SessionStore::list(session_dir)?;
    if let Some(cwd) = cwd {
        sessions.retain(|session| session.cwd == cwd);
    }
    Ok(sessions.pop().map(|summary| summary.path))
}

/// The model a TUI command names: a 1-based position in the model list, or
/// an exact `provider/id` or id among the selectable models, ignoring case.
fn resolve_model_reference(
    config: &LoadedConfig,
    reference: &str,
    offline: bool,
) -> Option<ModelRef> {
    if let Ok(index) = reference.parse::<usize>() {
        if index > 0 {
            return config.models.get(index - 1).map(ModelDefinition::model_ref);
        }
    }
    find_selectable_model(config, reference, offline)
}

/// The configured model a selector row names by its `provider/id` value.
fn selector_model(config: &LoadedConfig, value: &str) -> Option<ModelRef> {
    configured_model(config, &model_ref_from_value(value)?).map(ModelDefinition::model_ref)
}

/// A bare `noel` with no session flags used to open the resume picker here,
/// but that traps typed startup prompts inside the selector (scripting and
/// e2e send their first prompt immediately). Kimi/Claude/Codex all open the
/// picker only on an explicit resume action, so noel keeps `/resume`,
/// `--resume`, and the startup hint pointing there. Kept for the startup
/// hint decision: skip the hint when a picker would be redundant.
#[allow(dead_code)]
fn auto_open_resume_picker(cli: &Cli, runtime: &Runtime, config: &LoadedConfig) -> Result<bool> {
    if cli.r#continue
        || cli.resume
        || cli.no_session
        || cli.session.is_some()
        || cli.fork.is_some()
        || cli.session_id.is_some()
    {
        return Ok(false);
    }
    // Only offer sessions from this cwd so the picker matches the project.
    let sessions = SessionStore::list(&config.paths.session_dir)?;
    let any_for_cwd = sessions
        .iter()
        .any(|session| session.cwd == runtime.session().cwd);
    let _ = &runtime;
    Ok(any_for_cwd)
}

/// Takes the terminal for the TUI: raw mode, the alternate screen and mouse
/// capture when it owns the screen, bracketed paste and escape-code
/// disambiguation. Restoration is claimed first, so a fatal signal during
/// setup restores the terminal too.
fn open_tui_terminal(owned_screen: bool) -> Result<(TerminalRestore, TuiTerminal, u16)> {
    let restore = TerminalRestore::claim(owned_screen);
    enable_raw_mode()?;
    if owned_screen {
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    }
    execute!(io::stdout(), EnableBracketedPaste)?;
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let viewport_height = terminal_viewport_height();
    let mut terminal = new_tui_terminal(viewport_height, owned_screen)?;
    terminal.clear()?;
    Ok((restore, terminal, viewport_height))
}

/// Suspends the TUI's terminal modes so an interactive flow can use the
/// plain terminal (line input, its own output), then takes them back.
/// `interact` runs with the terminal restored; its output is the message
/// the TUI shows afterwards. The same claim/release lock the signal path
/// uses covers the gap, so a fatal signal during the flow still restores
/// the terminal.
pub(crate) async fn with_suspended_tui<R>(
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    config: &LoadedConfig,
    interact: impl Future<Output = R>,
) -> Result<R> {
    // Leave the modes the TUI enabled, in reverse, and show the cursor so
    // the flow's line input looks right.
    let mut out = io::stdout();
    let _ = execute!(
        &mut out,
        crossterm::cursor::Show,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste
    );
    if app.owned_screen {
        let _ = execute!(&mut out, DisableMouseCapture, LeaveAlternateScreen);
    }
    disable_raw_mode()?;
    let result = interact.await;
    // Take the terminal back exactly as startup did.
    signals::claim_tui_terminal(app.owned_screen);
    enable_raw_mode()?;
    let mut out = io::stdout();
    if app.owned_screen {
        execute!(&mut out, EnterAlternateScreen, EnableMouseCapture)?;
    }
    execute!(&mut out, EnableBracketedPaste)?;
    let _ = execute!(
        &mut out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    surface.terminal.clear()?;
    *surface.viewport_height = terminal_viewport_height();
    redraw_tui(surface.terminal, app, config)?;
    Ok(result)
}

/// Gives the terminal back, then prints the transcript for the e2e suites
/// when `NOEL_TUI_E2E_DUMP=1`, or writes it to `NOEL_TUI_E2E_DUMP_FILE`.
fn close_tui_terminal(
    mut terminal: TuiTerminal,
    restore: TerminalRestore,
    app: &TuiApp,
) -> Result<()> {
    let dump_transcript = std::env::var("NOEL_TUI_E2E_DUMP").ok().as_deref() == Some("1");
    if dump_transcript {
        terminal.clear()?;
    }
    drop(terminal);
    drop(restore);
    if dump_transcript {
        println!();
        println!("{}", app.transcript_text());
    }
    if let Some(path) = std::env::var_os("NOEL_TUI_E2E_DUMP_FILE").filter(|path| !path.is_empty()) {
        fs::write(path, format!("{}\n", app.transcript_text()))?;
    }
    Ok(())
}

/// What opening a session says of its saved work: a turn the saved context
/// left unfinished (direct slash-tool results have no call id and complete
/// their turn without an assistant reply) continues, with what is queued
/// placed first (Durable's `continue`).
pub(crate) fn resume_hint(mid_turn: bool, queued: usize) -> Option<String> {
    match (mid_turn, queued) {
        (false, 0) => None,
        (true, 0) => {
            Some("resumed mid-turn; continuing automatically from saved context".to_string())
        }
        (false, n) => Some(format!(
            "{n} queued message(s) pending; they run after your next prompt (/queue to list, /queue-clear to clear)"
        )),
        (true, n) => Some(format!(
            "resumed mid-turn; continuing automatically from saved context with {n} queued message(s) first"
        )),
    }
}

pub(crate) struct AutoRestart {
    executable: PathBuf,
    modified: Option<SystemTime>,
    pending: Option<(SystemTime, Instant)>,
}

/// A changed binary must stay untouched this long before the restart fires,
/// so a watcher that rebuilds continuously does not trap the UI in a
/// restart loop.
const RESTART_DEBOUNCE: Duration = Duration::from_secs(2);

impl AutoRestart {
    pub(crate) fn from_env() -> Self {
        let enabled = std::env::var("NOEL_DOGFOOD_AUTO_RESTART").ok().as_deref() == Some("1");
        if !enabled {
            return Self {
                executable: PathBuf::new(),
                modified: None,
                pending: None,
            };
        }
        let executable = std::env::var_os("NOEL_DOGFOOD_RESTART_EXE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_default();
        let modified = executable
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        Self {
            executable,
            modified,
            pending: None,
        }
    }

    pub(crate) fn should_restart(&mut self) -> Result<bool> {
        let Some(modified) = self.modified else {
            return Ok(false);
        };
        let current = self
            .executable
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        let Some(current) = current else {
            self.pending = None;
            return Ok(false);
        };
        if current == modified {
            self.pending = None;
            return Ok(false);
        }
        match self.pending {
            Some((mtime, since)) if mtime == current => Ok(since.elapsed() >= RESTART_DEBOUNCE),
            _ => {
                self.pending = Some((current, Instant::now()));
                Ok(false)
            }
        }
    }
}

#[cfg(unix)]
fn restart_current_process(session_file: Option<&Path>) -> Result<()> {
    let executable = std::env::var_os("NOEL_DOGFOOD_RESTART_EXE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| anyhow!("failed to resolve restart executable"))?;
    let session_file =
        session_file.map(|path| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
    let error = Command::new(executable)
        .args(restart_args(
            std::env::args_os().skip(1),
            session_file.as_deref(),
        ))
        .exec();
    Err(error.into())
}

#[cfg(not(unix))]
fn restart_current_process(_session_file: Option<&Path>) -> Result<()> {
    Err(anyhow!("dogfood auto-restart is only supported on Unix"))
}

/// Arguments for the dogfood exec-restart. The model selection is dropped,
/// since the session remembers its model. With a session file, the original
/// session selection (`--continue`, `--resume`, `--fork`, `--session`,
/// `--session-id`, `--no-session`) is replaced by `--session <file>`: running
/// `--continue` again could open a newer session, and `--fork` would fork
/// again. Other arguments are kept as given.
fn restart_args(
    args: impl IntoIterator<Item = OsString>,
    session_file: Option<&Path>,
) -> Vec<OsString> {
    const MODEL_OPTIONS: [&str; 2] = ["--model", "--provider"];
    const SESSION_OPTIONS: [&str; 3] = ["--fork", "--session", "--session-id"];
    const SESSION_FLAGS: [&str; 3] = ["--continue", "--resume", "--no-session"];
    let replace_session = session_file.is_some();
    let mut output = Vec::new();
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            output.push(arg);
            continue;
        };
        if text == "--" {
            positional.push(arg);
            positional.extend(args.by_ref());
            break;
        }
        let (name, inline_value) = match text.split_once('=') {
            Some((name, _)) if name.starts_with("--") => (name, true),
            _ => (text, false),
        };
        if MODEL_OPTIONS.contains(&name) || (replace_session && SESSION_OPTIONS.contains(&name)) {
            if !inline_value {
                let _ = args.next();
            }
            continue;
        }
        if replace_session && SESSION_FLAGS.contains(&text) {
            continue;
        }
        if replace_session && text.len() > 1 && text.starts_with('-') && !text.starts_with("--") {
            match without_session_short_flags(text) {
                Some(cluster) if cluster == "-" => {}
                Some(cluster) => output.push(cluster.into()),
                None => output.push(arg),
            }
            continue;
        }
        output.push(arg);
    }
    if let Some(path) = session_file {
        output.push("--session".into());
        output.push(path.into());
    }
    output.extend(positional);
    output
}

/// Removes `-c` and `-r` from a short-flag cluster such as `-cp`; `None` when
/// the cluster has neither. A short option that takes a value (`-t`, `-e`,
/// `-n`) consumes the rest of its cluster, which is kept verbatim.
fn without_session_short_flags(cluster: &str) -> Option<String> {
    let flags = cluster.strip_prefix('-')?;
    let mut kept = String::from("-");
    for (index, flag) in flags.char_indices() {
        match flag {
            'c' | 'r' => {}
            't' | 'e' | 'n' => {
                kept.push_str(&flags[index..]);
                break;
            }
            _ => kept.push(flag),
        }
    }
    (kept != cluster).then_some(kept)
}

/// The model picker's second layer for every provider: `{provider:
/// [{label, value, active}]}`, as the provider pick shows it.
fn provider_model_layers(config: &LoadedConfig, runtime: &Runtime) -> serde_json::Value {
    let mut layers = serde_json::Map::new();
    for model in picker_models(config) {
        let value = format!("{}/{}", model.provider, model.id);
        let active = runtime
            .session()
            .active_model
            .as_ref()
            .is_some_and(|active| active.provider == model.provider && active.id == model.id);
        let entry = serde_json::json!({"label": value, "value": value, "active": active});
        match layers
            .entry(model.provider.clone())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        {
            serde_json::Value::Array(items) => items.push(entry),
            _ => unreachable!("layers hold arrays"),
        }
    }
    serde_json::Value::Object(layers)
}

/// A key on the diff panel, with `detail` for the file opened.
fn diff_panel_key(
    key: &KeyEvent,
    app: &mut TuiApp,
    detail: impl Fn(&DiffFileEntry) -> DiffDetail,
) -> bool {
    let (input_empty, owned_screen) = (app.input.trim().is_empty(), app.owned_screen);
    let esc_closes = owned_screen || app.input.is_empty();
    let Some(panel) = app.diff_panel_mut() else {
        return false;
    };
    if let Some(detail) = panel.detail.as_mut() {
        match key.code {
            KeyCode::Esc => panel.detail = None,
            KeyCode::Up => detail.scroll = detail.scroll.saturating_sub(1),
            KeyCode::Down => {
                detail.scroll = (detail.scroll + 1).min(detail.lines.len().saturating_sub(1));
            }
            KeyCode::PageUp => detail.scroll = detail.scroll.saturating_sub(20),
            KeyCode::PageDown => {
                detail.scroll = (detail.scroll + 20).min(detail.lines.len().saturating_sub(1));
            }
            _ => return false,
        }
        return true;
    }
    match key.code {
        KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
        KeyCode::Down => {
            panel.selected = (panel.selected + 1).min(panel.files.len().saturating_sub(1));
        }
        KeyCode::Enter if key.modifiers.is_empty() && input_empty => {
            let detail = panel.files.get(panel.selected).map(&detail);
            if let Some(detail) = detail {
                panel.detail = Some(detail);
            }
        }
        KeyCode::Esc if esc_closes => app.close_overlay(|o| matches!(o, Overlay::DiffPanel(_))),
        _ => return false,
    }
    true
}

/// What the running turn is doing right now, shown in the footer spinner.
enum Activity {
    Waiting,
    Thinking,
    Writing,
    Tool(String),
    Compacting,
}

const SPINNER_FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

fn activity_status(started: Instant, activity: &Activity, pending_followups: usize) -> String {
    let elapsed = started.elapsed();
    let frame = SPINNER_FRAMES[(elapsed.as_millis() / 100) as usize % SPINNER_FRAMES.len()];
    let phase = match activity {
        Activity::Waiting => "waiting".to_string(),
        Activity::Thinking => "thinking".to_string(),
        Activity::Writing => "writing".to_string(),
        Activity::Compacting => "compacting".to_string(),
        Activity::Tool(name) => format!("running {name}"),
    };
    let queued = if pending_followups > 0 {
        format!(" · +{pending_followups} queued")
    } else {
        String::new()
    };
    format!(
        "{frame} {phase} · {}s{queued} · esc to interrupt",
        elapsed.as_secs()
    )
}

fn apply_settings_item(
    config: &mut LoadedConfig,
    runtime: &mut Runtime,
    key: &str,
) -> Result<String> {
    let message = match key {
        "compaction.enabled" => {
            let next = !auto_compaction_enabled(config);
            config
                .settings
                .compaction
                .get_or_insert_with(CompactionSettings::default)
                .enabled = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["compaction", "enabled"],
                next.into(),
            )?;
            format!("setting compaction.enabled: {}", on_off(next))
        }
        "terminal.showImages" => {
            let next = !config
                .settings
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.show_images)
                .unwrap_or(false);
            config
                .settings
                .terminal
                .get_or_insert_with(TerminalSettings::default)
                .show_images = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["terminal", "showImages"],
                next.into(),
            )?;
            format!("setting terminal.showImages: {}", on_off(next))
        }
        "terminal.showTerminalProgress" => {
            let next = !terminal_progress_enabled(config);
            config
                .settings
                .terminal
                .get_or_insert_with(TerminalSettings::default)
                .show_terminal_progress = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["terminal", "showTerminalProgress"],
                next.into(),
            )?;
            format!("setting terminal.showTerminalProgress: {}", on_off(next))
        }
        "images.autoResize" => {
            let next = !images_auto_resize(config);
            config
                .settings
                .images
                .get_or_insert_with(ImageSettings::default)
                .auto_resize = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["images", "autoResize"],
                next.into(),
            )?;
            format!("setting images.autoResize: {}", on_off(next))
        }
        "images.blockImages" => {
            let next = !images_blocked(config);
            config
                .settings
                .images
                .get_or_insert_with(ImageSettings::default)
                .block_images = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["images", "blockImages"],
                next.into(),
            )?;
            format!("setting images.blockImages: {}", on_off(next))
        }
        "retry.enabled" => {
            let next = !config
                .settings
                .retry
                .as_ref()
                .and_then(|retry| retry.enabled)
                .unwrap_or(true);
            config
                .settings
                .retry
                .get_or_insert_with(RetrySettings::default)
                .enabled = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["retry", "enabled"],
                next.into(),
            )?;
            format!("setting retry.enabled: {}", on_off(next))
        }
        "modelRefresh.enabled" => {
            let next = !model_refresh_enabled(&config.settings);
            config
                .settings
                .model_refresh
                .get_or_insert_with(ModelRefreshSettings::default)
                .enabled = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["modelRefresh", "enabled"],
                next.into(),
            )?;
            format!("setting modelRefresh.enabled: {}", on_off(next))
        }
        "hideThinkingBlock" => {
            let next = !config.settings.hide_thinking_block.unwrap_or(false);
            config.settings.hide_thinking_block = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["hideThinkingBlock"],
                next.into(),
            )?;
            format!("setting hideThinkingBlock: {}", on_off(next))
        }
        "warnings.anthropicExtraUsage" => {
            let next = !config
                .settings
                .warnings
                .as_ref()
                .and_then(|warnings| warnings.anthropic_extra_usage)
                .unwrap_or(true);
            config
                .settings
                .warnings
                .get_or_insert_with(WarningSettings::default)
                .anthropic_extra_usage = Some(next);
            write_user_setting(
                &config.paths.settings_path,
                &["warnings", "anthropicExtraUsage"],
                next.into(),
            )?;
            format!("setting warnings.anthropicExtraUsage: {}", on_off(next))
        }
        // followUpMode/steeringMode removed: queued bursts always dispatch
        // as one combined message (one turn), so there is no mode to toggle.
        _ => return Err(anyhow!("unknown setting: {key}")),
    };
    let next_generation = runtime.systems().config_generation + 1;
    runtime.reload(ReloadableSystems::from_config(config, next_generation))?;
    Ok(message)
}

fn write_user_setting(path: &Path, keys: &[&str], value: serde_json::Value) -> Result<()> {
    if keys.is_empty() {
        return Err(anyhow!("settings key path is empty"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut settings = if path.exists() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(path)?)?
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        return Err(anyhow!(
            "settings file must contain a JSON object: {}",
            path.display()
        ));
    }
    let mut current = &mut settings;
    for key in &keys[..keys.len() - 1] {
        if !current
            .get(*key)
            .map(|value| value.is_object())
            .unwrap_or(false)
        {
            current[*key] = serde_json::json!({});
        }
        current = current
            .get_mut(*key)
            .ok_or_else(|| anyhow!("failed to create settings object: {key}"))?;
    }
    current[keys[keys.len() - 1]] = value;
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )?;
    Ok(())
}

fn persist_default_model(config: &mut LoadedConfig, model: &ModelRef) -> Result<()> {
    config.settings.default_provider = Some(model.provider.clone());
    config.settings.default_model = Some(model.id.clone());
    write_user_setting(
        &config.paths.settings_path,
        &["defaultProvider"],
        model.provider.clone().into(),
    )?;
    write_user_setting(
        &config.paths.settings_path,
        &["defaultModel"],
        model.id.clone().into(),
    )?;
    Ok(())
}

fn clear_terminal_screen(terminal: &mut TuiTerminal, owned_screen: bool) -> Result<()> {
    execute!(
        terminal.backend_mut(),
        MoveTo(0, 0),
        TerminalClear(ClearType::All)
    )?;
    if !owned_screen {
        execute!(terminal.backend_mut(), TerminalClear(ClearType::Purge))?;
    }
    terminal.clear()?;
    Ok(())
}

/// A prompt to submit, expanded from what was typed.
pub(crate) struct TypedPrompt {
    prompt: String,
}

impl TypedPrompt {
    /// Typed input: a skill command or a prompt template expands.
    fn expand(config: &LoadedConfig, typed: String) -> Self {
        Self {
            prompt: expand_input(config, &typed),
        }
    }
}

/// A prompt a command has built, which is sent as it is.
impl From<String> for TypedPrompt {
    fn from(prompt: String) -> Self {
        Self { prompt }
    }
}

fn tool_call_detail(call: &noel_ai::ChatToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    if let Some(command) = args["command"].as_str() {
        return format!("$ {command}");
    }
    let detail = ["path", "url", "query", "pattern"]
        .iter()
        .find_map(|key| args[*key].as_str())
        .unwrap_or("");
    if call.name == "edit" {
        if let Some(edits) = args["edits"].as_array() {
            let added: usize = edits
                .iter()
                .filter_map(|edit| edit["newText"].as_str())
                .map(|text| text.lines().count())
                .sum();
            let removed: usize = edits
                .iter()
                .filter_map(|edit| edit["oldText"].as_str())
                .map(|text| text.lines().count())
                .sum();
            return format!("{detail} · +{added} -{removed}");
        }
    }
    detail.to_string()
}

fn collect_diff_file_entries(cwd: &Path, paths: &[String]) -> Vec<DiffFileEntry> {
    // git reports paths relative to cwd, so files under it are listed (and
    // looked up in git's output) the same way; that also keeps names visible
    // in the narrow panel.
    let paths = paths
        .iter()
        .map(|path| match Path::new(path).strip_prefix(cwd) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                relative.to_string_lossy().into_owned()
            }
            _ => path.clone(),
        })
        .collect::<Vec<_>>();
    let stats = git_numstat(cwd, &paths);
    let untracked = git_untracked_paths(cwd);
    paths
        .into_iter()
        .map(|path| {
            let (added, removed) = stats.get(path.as_str()).copied().unwrap_or((None, None));
            DiffFileEntry {
                added,
                removed,
                untracked: untracked.contains(&path),
                path,
            }
        })
        .collect()
}

fn git_numstat(cwd: &Path, paths: &[String]) -> BTreeMap<String, (Option<u64>, Option<u64>)> {
    if paths.is_empty() {
        return BTreeMap::new();
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--numstat", "--relative", "--"])
        .args(paths)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            parse_numstat(&String::from_utf8_lossy(&output.stdout))
        }
        _ => BTreeMap::new(),
    }
}

fn parse_numstat(text: &str) -> BTreeMap<String, (Option<u64>, Option<u64>)> {
    text.lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let added = columns.next()?.parse::<u64>().ok();
            let removed = columns.next()?.parse::<u64>().ok();
            let path = columns.next()?.to_string();
            Some((path, (added, removed)))
        })
        .collect()
}

fn git_untracked_paths(cwd: &Path) -> BTreeSet<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(ToString::to_string)
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn build_diff_detail(cwd: &Path, entry: &DiffFileEntry) -> DiffDetail {
    let lines = if entry.untracked {
        match fs::read_to_string(cwd.join(&entry.path)) {
            Ok(content) => content
                .lines()
                .take(500)
                .map(|line| format!("+{line}"))
                .collect(),
            Err(_) => vec!["unable to read file".to_string()],
        }
    } else {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["diff", "--relative", "--"])
            .arg(&entry.path)
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.trim().is_empty() {
                    vec!["no uncommitted changes".to_string()]
                } else {
                    text.lines().map(ToString::to_string).collect()
                }
            }
            _ => vec!["git diff unavailable".to_string()],
        }
    };
    DiffDetail {
        path: entry.path.clone(),
        lines,
        scroll: 0,
    }
}

fn response_kind_for_prompt(prompt: &str) -> TuiEntryKind {
    if matches!(
        prompt.split_whitespace().next(),
        Some(
            "/read"
                | "/write"
                | "/edit"
                | "/grep"
                | "/find"
                | "/ls"
                | "/bash"
                | "/web-search"
                | "/web-fetch"
        )
    ) {
        TuiEntryKind::Tool
    } else {
        TuiEntryKind::Assistant
    }
}

fn terminal_progress_enabled(config: &LoadedConfig) -> bool {
    config
        .settings
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.show_terminal_progress)
        .unwrap_or(true)
}

fn format_bash_running(command: &str) -> String {
    format!("running bash\n$ {command}")
}

fn format_bash_completed(command: &str, output: &str) -> String {
    format!("completed bash\n$ {command}\n{output}")
}

fn resolve_bang_command(line: &str, last_shell_command: &Option<String>) -> Option<String> {
    if line == "!!" {
        return last_shell_command.clone();
    }
    let command = line.strip_prefix('!')?.trim();
    Some(command.to_string())
}

fn print_response(mode: &OutputMode, response: &str) {
    match mode {
        OutputMode::Json => println!("{}", serde_json::json!({ "message": response })),
        OutputMode::Text | OutputMode::Rpc => println!("{response}"),
    }
}

fn terminal_renderer(config: &LoadedConfig) -> TerminalRenderer {
    TerminalRenderer::new(terminal_theme(config).unwrap_or_default())
}

fn terminal_theme(config: &LoadedConfig) -> Result<TerminalTheme> {
    resolve_terminal_theme(config, config.settings.theme.as_deref().unwrap_or("system"))?
        .with_accent(config.settings.accent_color.as_deref())
        .map_err(|error| anyhow!(error))
}

fn resolve_terminal_theme(config: &LoadedConfig, name: &str) -> Result<TerminalTheme> {
    // Loaded resources can customize built-ins; /theme system always restores native colors.
    if name != "system" && name != "default" {
        if let Some(resource) = find_resource(&config.themes, name) {
            return TerminalTheme::from_json(&resource.name, &resource.content)
                .map_err(|error| anyhow!(error));
        }
    }
    if let Some(theme) = TerminalTheme::builtin(name) {
        return Ok(theme);
    }
    let path = Path::new(name);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config.paths.cwd.join(path)
    };
    if path.is_file() {
        let mut theme =
            TerminalTheme::from_json(&resource_name(&path), &fs::read_to_string(&path)?)
                .map_err(|error| anyhow!(error))?;
        theme.name = fs::canonicalize(path)?.display().to_string();
        return Ok(theme);
    }
    Err(anyhow!("theme not found: {name}"))
}

fn persist_theme(config: &mut LoadedConfig, name: &str) -> Result<String> {
    let theme = resolve_terminal_theme(config, name)?;
    // Validate before persisting, so malformed themes never replace a working selection.
    theme
        .clone()
        .with_accent(config.settings.accent_color.as_deref())
        .map_err(|error| anyhow!(error))?;
    write_user_setting(
        &config.paths.settings_path,
        &["theme"],
        theme.name.clone().into(),
    )?;
    config.settings.theme = Some(theme.name.clone());
    Ok(theme.name)
}

fn persist_accent(config: &mut LoadedConfig, color: &str) -> Result<()> {
    let accent = if color == "auto" {
        None
    } else {
        parse_theme_color(color).map_err(|error| anyhow!(error))?;
        Some(color.to_string())
    };
    write_user_setting(
        &config.paths.settings_path,
        &["accentColor"],
        serde_json::to_value(&accent)?,
    )?;
    config.settings.accent_color = accent;
    Ok(())
}

fn theme_names(config: &LoadedConfig) -> Vec<String> {
    let mut names = BUILTIN_THEMES
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    for resource in &config.themes {
        if !names.contains(&resource.name) {
            names.push(resource.name.clone());
        }
    }
    names
}

fn format_themes(config: &LoadedConfig) -> String {
    format!("themes\n{}", theme_names(config).join("\n"))
}

fn keybinding_map(config: &LoadedConfig) -> KeybindingMap {
    KeybindingMap::with_overrides(
        config
            .keybindings
            .iter()
            .map(|binding| TuiKeybinding {
                action: binding.action.clone(),
                keys: binding.keys.clone(),
            })
            .collect(),
    )
}

/// `/app`: the desktop app (`noel app`, spec §14) on this session, started
/// apart from the terminal. A session this process holds opens there once
/// it lets the session go (one writer at a time).
pub(crate) fn open_desktop_app(session: &str) -> String {
    let started = std::env::current_exe().and_then(|noel| {
        std::process::Command::new(noel)
            .args(["app", "--session", session])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    });
    match started {
        Ok(_) => format!("opening session {session} in the desktop app"),
        Err(error) => format!("could not start the desktop app: {error}"),
    }
}

fn format_session(runtime: &Runtime) -> String {
    TerminalRenderer::default().session(&SessionView {
        id: runtime.session().session_id.clone(),
        cwd: runtime.session().cwd.display().to_string(),
        name: runtime.session().name.clone(),
        labels: runtime.session().labels.iter().cloned().collect(),
        parent: runtime.session().parent_session_id.clone(),
        file: runtime
            .store()
            .map(|store| store.path().display().to_string()),
    })
}

fn format_changelog() -> String {
    let entries = changelog_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|content| parse_changelog_entries(&content))
        .unwrap_or_default();
    if entries.is_empty() {
        return "What's New\n\nNo changelog entries found.".to_string();
    }
    format!(
        "What's New\n\n{}",
        entries.into_iter().rev().collect::<Vec<_>>().join("\n\n")
    )
}

fn changelog_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("CHANGELOG.md"));
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../CHANGELOG.md"));
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            candidates.push(ancestor.join("CHANGELOG.md"));
        }
    }
    candidates.into_iter().find(|path| path.exists())
}

fn parse_changelog_entries(content: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = Vec::new();
    let mut in_version = false;
    for line in content.lines() {
        if line.starts_with("## ") {
            if in_version && !current.is_empty() {
                entries.push(current.join("\n").trim().to_string());
            }
            in_version = changelog_header_has_version(line);
            current.clear();
            if in_version {
                current.push(line.to_string());
            }
        } else if in_version {
            current.push(line.to_string());
        }
    }
    if in_version && !current.is_empty() {
        entries.push(current.join("\n").trim().to_string());
    }
    entries.retain(|entry| !entry.is_empty());
    entries
}

fn changelog_header_has_version(line: &str) -> bool {
    let version = line
        .trim_start_matches("## ")
        .trim()
        .trim_start_matches('[')
        .split([']', ' '])
        .next()
        .unwrap_or_default();
    let parts = version.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|value| value.is_ascii_digit()))
}

/// The tree's rows as selector items: value is the session id, label the
/// tree row (connector + picker label), the current session active.
fn session_tree_items(config: &LoadedConfig, current: Option<&str>) -> Result<Vec<SelectorItem>> {
    let sessions = SessionStore::list(&config.paths.session_dir)?;
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let by_id: BTreeMap<&str, &noel_core::SessionSummary> = sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session))
        .collect();
    let mut children: BTreeMap<String, Vec<&noel_core::SessionSummary>> = BTreeMap::new();
    let mut roots: Vec<&noel_core::SessionSummary> = Vec::new();
    for session in &sessions {
        match session
            .parent_session_id
            .as_deref()
            .filter(|parent| by_id.contains_key(parent))
        {
            Some(parent) => children.entry(parent.to_owned()).or_default().push(session),
            None => roots.push(session),
        }
    }
    let mut items = Vec::new();
    for root in &roots {
        session_tree_row(root, &children, current, "", false, &mut items);
    }
    Ok(items)
}

/// Each listed session's parent in the tree (one that is listed) and
/// whether it is named: what the branch navigator folds and filters by.
pub(crate) fn session_tree_rows(
    config: &LoadedConfig,
) -> Result<BTreeMap<String, (Option<String>, bool)>> {
    let sessions = SessionStore::list(&config.paths.session_dir)?;
    let listed: BTreeSet<&str> = sessions
        .iter()
        .map(|session| session.session_id.as_str())
        .collect();
    Ok(sessions
        .iter()
        .map(|session| {
            let parent = session
                .parent_session_id
                .clone()
                .filter(|parent| listed.contains(parent.as_str()));
            (session.session_id.clone(), (parent, session.name.is_some()))
        })
        .collect())
}

/// One row of the tree selector: the label /tree shows, with the session id
/// as the value Enter picks.
fn session_tree_row(
    session: &noel_core::SessionSummary,
    children: &BTreeMap<String, Vec<&noel_core::SessionSummary>>,
    current: Option<&str>,
    indent: &str,
    last: bool,
    items: &mut Vec<SelectorItem>,
) {
    let here = current == Some(session.session_id.as_str());
    let own = children
        .get(&session.session_id)
        .map(|kids| kids.len())
        .unwrap_or(0);
    let branch_note = if own > 0 {
        format!(" ({own} branch{})", if own > 1 { "es" } else { "" })
    } else {
        String::new()
    };
    items.push(SelectorItem {
        label: format!(
            "{indent}{}{}{branch_note}",
            if here { "• " } else { "" },
            format_session_picker_label(session)
        ),
        value: session.session_id.clone(),
        active: here,
    });
    if let Some(kids) = children.get(&session.session_id) {
        for (index, child) in kids.iter().enumerate() {
            let child_last = index + 1 == kids.len();
            let stem = if indent.is_empty() || last {
                "  "
            } else {
                "│ "
            };
            let connector = if child_last { "└─ " } else { "├─ " };
            session_tree_row(
                child,
                children,
                current,
                &format!("{indent}{stem}{connector}"),
                child_last,
                items,
            );
        }
    }
}

fn format_session_tree(session_dir: &Path, current: Option<&str>) -> Result<String> {
    let sessions = SessionStore::list(session_dir)?;
    if sessions.is_empty() {
        return Ok("no sessions".to_string());
    }
    // Children by parent, in listing order; roots are sessions without a
    // parent we know (an unforked session, or one whose parent is gone).
    let by_id: BTreeMap<&str, &noel_core::SessionSummary> = sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session))
        .collect();
    let mut children: BTreeMap<String, Vec<&noel_core::SessionSummary>> = BTreeMap::new();
    let mut roots: Vec<&noel_core::SessionSummary> = Vec::new();
    for session in &sessions {
        match session
            .parent_session_id
            .as_deref()
            .filter(|parent| by_id.contains_key(parent))
        {
            Some(parent) => children.entry(parent.to_owned()).or_default().push(session),
            None => roots.push(session),
        }
    }
    // A child listed before its parent (a newer fork of an older id) still
    // renders under it; every session appears exactly once.
    let mut lines = Vec::new();
    for root in &roots {
        render_session_tree_row(root, &children, current, "", false, &mut lines);
    }
    Ok(lines.join("\n"))
}

/// One row of the session tree, upstream's `/tree` style: connectors,
/// the current session marked, and the resume picker's label so a row is
/// recognizable at a glance.
fn render_session_tree_row(
    session: &noel_core::SessionSummary,
    children: &BTreeMap<String, Vec<&noel_core::SessionSummary>>,
    current: Option<&str>,
    indent: &str,
    last: bool,
    lines: &mut Vec<String>,
) {
    let here = current == Some(session.session_id.as_str());
    let marker = if here { "• " } else { "  " };
    let own = children
        .get(session.session_id.as_str())
        .map(|kids| kids.len())
        .unwrap_or(0);
    let branch_note = if own > 0 {
        format!(" ({own} branch{})", if own > 1 { "es" } else { "" })
    } else {
        String::new()
    };
    let summary_note = session
        .branch_summary
        .as_deref()
        .map(|summary| {
            let collapsed: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
            let truncated: String = collapsed.chars().take(80).collect();
            if collapsed.chars().count() > 80 {
                format!("\n    {truncated}…")
            } else if collapsed.is_empty() {
                String::new()
            } else {
                format!("\n    {truncated}")
            }
        })
        .unwrap_or_default();
    lines.push(format!(
        "{indent}{marker}{}{branch_note}{}",
        format_session_picker_label(session),
        summary_note
    ));
    if let Some(kids) = children.get(&session.session_id) {
        for (index, child) in kids.iter().enumerate() {
            let child_last = index + 1 == kids.len();
            let stem = if indent.is_empty() || last {
                "  "
            } else {
                "│ "
            };
            let connector = if child_last { "└─ " } else { "├─ " };
            let child_indent = format!("{indent}{stem}{connector}");
            render_session_tree_row(child, children, current, &child_indent, child_last, lines);
        }
    }
}

fn format_session_usage(runtime: &Runtime) -> String {
    let usage = &runtime.session().usage;
    if usage.requests == 0 {
        return "session usage: no provider token usage reported yet".into();
    }
    format!("session usage: {} input tokens, {} output tokens, {} total · {} reported request(s)\ncosts are not available", usage.input_tokens, usage.output_tokens, usage.input_tokens.saturating_add(usage.output_tokens), usage.requests)
}

fn format_summaries(runtime: &Runtime) -> String {
    if runtime.session().compactions.is_empty() && runtime.session().branch_summaries.is_empty() {
        return "no summaries".to_string();
    }
    let mut lines = Vec::new();
    for record in &runtime.session().compactions {
        lines.push(format!(
            "compaction {:?}: omitted {}, retained {}",
            record.kind, record.omitted_messages, record.retained_messages
        ));
        lines.push(record.summary.clone());
    }
    for summary in &runtime.session().branch_summaries {
        lines.push(format!(
            "branch {} -> {}",
            summary.from_session_id, summary.to_session_id
        ));
        lines.push(summary.summary.clone());
    }
    lines.join("\n")
}

fn format_settings(config: &LoadedConfig, runtime: &Runtime) -> String {
    terminal_renderer(config).settings(&SettingsView {
        agent_dir: config.paths.agent_dir.display().to_string(),
        session_dir: config.paths.session_dir.display().to_string(),
        config_generation: runtime.systems().config_generation,
        active_model: runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| format!("{}/{}", model.provider, model.id)),
        theme: Some(
            config
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "system".to_string()),
        ),
    })
}

fn format_hotkeys(config: &LoadedConfig) -> String {
    let mut output = terminal_renderer(config).keybindings(&keybinding_map(config));
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str("transcript\tmouse wheel, PageUp/PageDown; Esc returns to bottom\nselection\tdrag to select/copy; ctrl+c or /copy copies selection; Esc clears; --no-alt-screen for native scrollback");
    output
}

fn format_media_fallback(media: &MediaInput) -> String {
    let dimensions = match (media.width, media.height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => "unknown-size".to_string(),
    };
    format!(
        "image: {}\t{}\t{}\tterminal display fallback: attached to provider message",
        media.path.as_deref().unwrap_or("-"),
        media.mime_type,
        dimensions
    )
}

fn format_history(editor_state: &EditorState) -> String {
    if editor_state.history().is_empty() {
        return "history is empty".to_string();
    }
    editor_state
        .history()
        .iter()
        .enumerate()
        .map(|(index, entry)| format!("{}.\t{}", index + 1, entry))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_image_models(config: &LoadedConfig, search: &str) -> String {
    let search = search.to_ascii_lowercase();
    let models = config
        .image_models
        .iter()
        .filter(|model| {
            if search.is_empty() {
                return true;
            }
            format!(
                "{}/{} {} {:?}",
                model.provider,
                model.id,
                model.name.as_deref().unwrap_or_default(),
                model.api
            )
            .to_ascii_lowercase()
            .contains(&search)
        })
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect::<Vec<_>>();
    if models.is_empty() {
        "no image models".to_string()
    } else {
        models.join("\n")
    }
}

fn command_completions(config: &LoadedConfig, prefix: &str) -> Vec<String> {
    let mut completions = EditorState::command_completions(prefix)
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    completions.extend(resource_command_completions(
        prefix,
        "/extension:",
        &config.extensions,
    ));
    completions.extend(resource_commands::completions(config, prefix));
    let templates = config.prompt_templates.iter();
    completions.extend(
        templates
            .map(|template| format!("/prompt {}", template.name))
            .filter(|command| command.starts_with(prefix)),
    );
    completions.extend(
        theme_names(config)
            .into_iter()
            .map(|name| format!("/theme {name}"))
            .filter(|command| command.starts_with(prefix)),
    );
    completions.extend(
        ["refresh", "reset"]
            .into_iter()
            .map(|sub| format!("/models {sub}"))
            .filter(|command| command.starts_with(prefix)),
    );
    completions.extend(
        [
            "auto", "cyan", "blue", "magenta", "green", "yellow", "red", "default",
        ]
        .into_iter()
        .map(|color| format!("/accent {color}"))
        .filter(|command| command.starts_with(prefix)),
    );
    completions.sort();
    completions.dedup();
    completions
}

fn resource_command_completions(
    prefix: &str,
    command_prefix: &str,
    resources: &[ResourceFile],
) -> Vec<String> {
    resources
        .iter()
        .map(|resource| format!("{command_prefix}{}", resource.name))
        .filter(|command| command.starts_with(prefix))
        .collect()
}

fn read_external_editor_prompt(initial: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!(
        "noel-editor-{}-{}.txt",
        std::process::id(),
        unique_temp_suffix()
    ));
    fs::write(&path, initial)?;
    if let Ok(command) = std::env::var("NOEL_EDITOR_COMMAND") {
        let command = command.replace("{file}", &shell_quote(&path.display().to_string()));
        run_editor_command(&command)?;
    } else {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .map_err(|_| anyhow!("set NOEL_EDITOR_COMMAND, VISUAL, or EDITOR"))?;
        run_editor_command(&format!(
            "{editor} {}",
            shell_quote(&path.display().to_string())
        ))?;
    }
    let content = fs::read_to_string(&path)?;
    let _ = fs::remove_file(path);
    Ok(content.trim().to_string())
}

fn run_editor_command(command: &str) -> Result<()> {
    let status = Command::new("sh").arg("-c").arg(command).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("editor command failed: {command}"))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unique_temp_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn format_diagnostics(config: &LoadedConfig) -> String {
    let mut diagnostics = config.diagnostics.clone();
    diagnostics.extend(extension_manifest_diagnostics(&config.extensions));
    if let Err(error) = terminal_theme(config) {
        diagnostics.push(format!("{error}; using system theme"));
    }
    if diagnostics.is_empty() {
        return "no diagnostics".to_string();
    }
    diagnostics
        .iter()
        .map(|diagnostic| format!("diagnostic: {diagnostic}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn extension_manifest_diagnostics(extensions: &[ResourceFile]) -> Vec<String> {
    extensions
        .iter()
        .filter(|extension| is_executable_extension(&extension.path))
        .filter_map(|extension| {
            extension_protocol(extension)
                .err()
                .map(|error| format!("extension {}: {error}", extension.name))
        })
        .collect()
}

fn select_from_selector_message(
    config: &mut LoadedConfig,
    runtime: &mut Runtime,
    line: &str,
) -> Result<String> {
    let rest = line.trim_start_matches("/select ").trim();
    let (kind, query) = split_once_text(rest);
    if query.is_empty() {
        return Err(anyhow!("usage: /select <kind> <query>"));
    }
    let selector = selector_for_kind(config, runtime, kind)?;
    let item = selector
        .select_query(query)
        .ok_or_else(|| anyhow!("selector item not found: {query}"))?;
    match kind {
        "model" | "models" | "scoped-models" => {
            let model = selector_model(config, &item.value)
                .ok_or_else(|| anyhow!("model not found: {}", item.value))?;
            switch_model(runtime, config, &model, None)?;
            persist_default_model(config, &model)?;
            Ok(format_model_selection(
                &model,
                thinking_label(runtime, config).as_deref(),
            ))
        }
        "theme" | "themes" => {
            let name = persist_theme(config, &item.value)?;
            Ok(format!("theme: {name}"))
        }
        "session" | "sessions" | "resume" | "tree" => {
            let path = resolve_session_reference(&config.paths.session_dir, &item.value)?;
            let (store, state) = SessionStore::open(path)?;
            runtime.replace_session(state, Some(store));
            Ok(format_session(runtime))
        }
        "auth" | "login" => Ok(format_login_status(config, &item.value)),
        "account" => {
            let account = if item.value.is_empty() {
                None
            } else {
                Some(item.value.clone())
            };
            runtime.set_active_account(account)?;
            Ok(format!(
                "account: {}",
                if item.value.is_empty() {
                    "auto"
                } else {
                    item.value.as_str()
                }
            ))
        }
        "logout" => {
            if remove_stored_auth(config, &item.value, DEFAULT_ACCOUNT_NAME)? {
                Ok(format!("removed stored auth for {}", item.value))
            } else {
                Ok(format!("no stored auth for {}", item.value))
            }
        }
        _ => Err(anyhow!("unknown selector: {kind}")),
    }
}

fn format_resources(kind: &str, resources: &[ResourceFile]) -> String {
    if resources.is_empty() {
        return format!("no {kind}");
    }
    resources
        .iter()
        .map(|resource| format_resource_line(kind, resource))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_resource_line(kind: &str, resource: &ResourceFile) -> String {
    if kind != "extensions" || !is_executable_extension(&resource.path) {
        return format!("{}\t{}", resource.name, resource.path.display());
    }
    let protocol = match extension_protocol(resource) {
        Ok(Some(protocol)) => protocol,
        Ok(None) => "stdio".to_string(),
        Err(error) => format!("invalid: {error}"),
    };
    format!(
        "{}\t{}\tprotocol:{}",
        resource.name,
        resource.path.display(),
        protocol
    )
}

fn format_queue(runtime: &Runtime) -> String {
    if runtime.session().queue.is_empty() {
        return "queue is empty".to_string();
    }
    runtime
        .session()
        .queued_messages()
        .iter()
        .enumerate()
        .map(|(index, message)| format!("{}.\t{}", index + 1, message))
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_resource<'a>(resources: &'a [ResourceFile], name: &str) -> Option<&'a ResourceFile> {
    resources.iter().find(|resource| resource.name == name)
}

fn is_executable_extension(path: &Path) -> bool {
    // A `.ts` file is not executable on disk; it runs under the bun host
    // (see `host_command` in noel-core), so it counts as runnable here.
    if path.extension().and_then(|e| e.to_str()) == Some("ts") {
        return path.is_file();
    }
    path.is_file() && is_executable_file(path)
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("exe"))
        .unwrap_or(false)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionManifest {
    protocol: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionProtocolRequest<'a> {
    protocol_version: u8,
    kind: &'static str,
    command: &'a str,
    input: &'a str,
    cwd: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionCommandResponse {
    output: Option<String>,
    error: Option<String>,
}

fn run_executable_extension(extension: &ResourceFile, input: &str, cwd: &Path) -> Result<String> {
    let protocol = extension_protocol(extension)?;
    run_extension_process(extension, protocol.as_deref(), "command", input, cwd)
}

fn notify_extension_lifecycle(
    extensions: &[ResourceFile],
    kind: &'static str,
    cwd: &Path,
) -> Vec<String> {
    extensions
        .iter()
        .filter(|extension| is_executable_extension(&extension.path))
        .filter_map(|extension| match extension_protocol(extension) {
            Ok(Some(protocol)) if protocol == "json" => {
                run_extension_process(extension, Some("json"), kind, "", cwd)
                    .err()
                    .map(|error| format!("extension {} {kind} failed: {error}", extension.name))
            }
            Ok(_) => None,
            Err(error) => Some(format!(
                "extension {} {kind} failed: {error}",
                extension.name
            )),
        })
        .collect()
}

fn run_extension_process(
    extension: &ResourceFile,
    protocol: Option<&str>,
    kind: &'static str,
    input: &str,
    cwd: &Path,
) -> Result<String> {
    let mut child = Command::new(&extension.path)
        .current_dir(cwd)
        .env("NOEL_EXTENSION_NAME", &extension.name)
        .env("NOEL_EXTENSION_PATH", &extension.path)
        .env("NOEL_EXTENSION_PROTOCOL", protocol.unwrap_or("stdio"))
        // Extensions written for pi read the same values under PI_ names.
        .env("PI_EXTENSION_NAME", &extension.name)
        .env("PI_EXTENSION_PATH", &extension.path)
        .env("PI_EXTENSION_PROTOCOL", protocol.unwrap_or("stdio"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        match protocol {
            Some("json") => {
                let cwd_string = cwd.display().to_string();
                let request = ExtensionProtocolRequest {
                    protocol_version: 1,
                    kind,
                    command: &extension.name,
                    input,
                    cwd: &cwd_string,
                };
                serde_json::to_writer(&mut stdin, &request)?;
                stdin.write_all(b"\n")?;
            }
            _ if !input.is_empty() => stdin.write_all(input.as_bytes())?,
            _ => {}
        }
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(anyhow!(
            "extension {} failed:\n{}{}",
            extension.name,
            stdout,
            stderr
        ));
    }
    let mut text = match protocol {
        Some("json") => parse_extension_json_response(&stdout, &extension.name)?,
        _ => stdout.trim_end().to_string(),
    };
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr.trim_end());
    }
    if text.is_empty() {
        Ok(format!("extension {} completed", extension.name))
    } else {
        Ok(text)
    }
}

fn extension_protocol(extension: &ResourceFile) -> Result<Option<String>> {
    for manifest_path in extension_manifest_paths(&extension.path) {
        if !manifest_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&manifest_path)?;
        let manifest = serde_json::from_str::<ExtensionManifest>(&content).map_err(|error| {
            anyhow!(
                "failed to parse extension manifest {}: {error}",
                manifest_path.display()
            )
        })?;
        if let Some(protocol) = manifest.protocol.as_deref() {
            if !matches!(protocol, "json" | "stdio") {
                return Err(anyhow!(
                    "unsupported protocol {protocol} in {}",
                    manifest_path.display()
                ));
            }
        }
        return Ok(manifest.protocol);
    }
    Ok(None)
}

fn extension_manifest_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(file_name) = path.file_name() {
        paths.push(path.with_file_name(format!(
            "{}.noel-extension.json",
            file_name.to_string_lossy()
        )));
        // Extensions written for pi ship a .pi-extension.json manifest.
        paths.push(
            path.with_file_name(format!("{}.pi-extension.json", file_name.to_string_lossy())),
        );
        paths.push(path.with_file_name(format!("{}.json", file_name.to_string_lossy())));
    }
    if path.extension().is_some() {
        paths.push(path.with_extension("json"));
    }
    paths
}

fn parse_extension_json_response(stdout: &str, name: &str) -> Result<String> {
    let response =
        serde_json::from_str::<ExtensionCommandResponse>(stdout.trim()).map_err(|error| {
            anyhow!("extension {name} returned invalid JSON protocol response: {error}")
        })?;
    if let Some(error) = response.error {
        return Err(anyhow!("extension {name} failed: {error}"));
    }
    Ok(response
        .output
        .unwrap_or_else(|| format!("extension {name} completed")))
}

fn split_resource_command<'a>(line: &'a str, prefix: &str) -> (&'a str, &'a str) {
    let rest = line.trim_start_matches(prefix).trim();
    split_once_text(rest)
}

fn split_once_text(value: &str) -> (&str, &str) {
    let mut parts = value.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    (first, rest)
}

fn format_login_status(config: &LoadedConfig, provider: &str) -> String {
    let providers = if provider.is_empty() {
        config
            .models
            .iter()
            .map(|model| model.provider.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        vec![provider.to_string()]
    };
    providers
        .into_iter()
        .map(|provider| {
            let accounts = config.auth.accounts_for_provider(&provider);
            let status = if accounts.is_empty() {
                if auth_for_provider(&config.auth, &provider, None).is_some() {
                    "available".to_string()
                } else {
                    "missing".to_string()
                }
            } else {
                format!("accounts: {}", accounts.join(", "))
            };
            format!("{provider}: {status}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolve_source_session(
    session_dir: &Path,
    runtime: &Runtime,
    line: &str,
    command: &str,
) -> Result<SessionState> {
    let reference = line.trim_start_matches(command).trim();
    if reference.is_empty() {
        return Ok(runtime.session().clone());
    }
    let path = resolve_session_reference(session_dir, reference)?;
    // A fork or clone source is only read, so it may be open elsewhere.
    Ok(SessionStore::read(&path)?)
}

fn export_session(runtime: &Runtime, path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    if let Some(store) = runtime.store() {
        store.export_state(runtime.session(), path)?;
    } else {
        write_session_export(runtime.session(), path)?;
    }
    Ok(())
}

fn delete_session_message(
    config: &LoadedConfig,
    runtime: &mut Runtime,
    line: &str,
) -> Result<String> {
    let reference = line.trim_start_matches("/delete").trim();
    if reference.is_empty() {
        // Deleting is never implicit: name the session, even the current one.
        return Ok(format!(
            "usage: /delete <id|name|path> (the current session is {})",
            runtime.session().session_id
        ));
    }
    let target = resolve_session_reference(&config.paths.session_dir, reference)?;
    let deleting_current = runtime
        .store()
        .map(|store| store.path() == target)
        .unwrap_or(false);
    // Refuses while another noel process has the session open.
    SessionStore::delete(&target)?;
    let mut output = format!("deleted {}", target.display());
    if deleting_current {
        let (store, state) =
            SessionStore::create(&config.paths.session_dir, runtime.session().cwd.clone())?;
        runtime.replace_session(state, Some(store));
        output.push('\n');
        output.push_str(&format_session(runtime));
    }
    Ok(output)
}

async fn copy_pending_selection(app: &mut TuiApp) {
    let Some(text) = app.transcript_view.get_mut().take_pending_copy() else {
        return;
    };
    let status = match clipboard::copy_text(&text).await {
        Ok(status) if status.starts_with("copied") => "copied selection".into(),
        Ok(_) => "clipboard requested (OSC 52)".into(),
        Err(error) => format!("copy failed: {error}"),
    };
    // Copy feedback is chrome, not a new transcript entry that changes reading.
    app.transcript_view.get_mut().set_copy_status(status);
}

/// Logs one account out: removes it from `auth.json` as the file is now (so
/// tokens refreshed since startup survive) and from this session's config.
/// The in-memory auth is never written back: it may hold a `--api-key`
/// value, which must not be persisted.
fn remove_stored_auth(config: &mut LoadedConfig, provider: &str, account: &str) -> Result<bool> {
    let removed = noel_config::update_auth_file(&config.paths.auth_path, |auth: &mut AuthData| {
        auth.remove(provider, account).is_some()
    })?;
    Ok(config.auth.remove(provider, account).is_some() || removed)
}

#[cfg(test)]
mod tests;

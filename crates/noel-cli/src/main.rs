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
mod bug_report;
mod bun_install;
mod clipboard;
mod markdown;
mod model_select;
mod oauth_login;
mod oauth_refresh;
mod resource_commands;
mod rpc;
mod rpc_wire;
mod server;
mod session_commands;
mod signals;
mod transcript_view;
mod usage;
mod workspace;
use model_select::{
    configured_model, effective_thinking_level, find_selectable_model, model_definition,
    normalized_thinking_level, provider_usable, register_cli_model, resolve_cli,
    resolve_cli_selection, resolve_scope, select_initial_model, set_thinking_level, switch_model,
    thinking_label,
};
use transcript_view::TranscriptView;

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

impl Cli {
    fn owned_screen(&self) -> bool {
        match self.tui_mode {
            Some(mode) => mode == TuiMode::Fullscreen,
            None => !self.no_alt_screen,
        }
    }
}

async fn try_run_image_command() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    match command {
        "images" | "image-models" => {
            run_image_model_list(&args[1..])?;
            Ok(true)
        }
        "generate-image" | "image-generate" => {
            run_image_generate(&args[1..]).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn run_image_model_list(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("usage: noel images [search]\n\nList image-generation models.");
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    let config = load_config(config_paths(cwd, None)?)?;
    let search = args.first().map(|value| value.to_ascii_lowercase());
    for model in &config.image_models {
        if let Some(search) = &search {
            let display = format!(
                "{}/{} {} {:?}",
                model.provider,
                model.id,
                model.name.as_deref().unwrap_or_default(),
                model.api
            )
            .to_ascii_lowercase();
            if !display.contains(search) {
                continue;
            }
        }
        println!("{}/{}\t{:?}", model.provider, model.id, model.api);
    }
    Ok(())
}

async fn run_image_generate(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_image_generate_help();
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    let mut model_ref = "openrouter/google/gemini-3.1-flash-image-preview".to_string();
    let mut output_path = None::<PathBuf>;
    let mut image_paths = Vec::<PathBuf>::new();
    let mut prompt_parts = Vec::<String>::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--model" | "-m" => {
                index += 1;
                model_ref = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--model requires a value"))?
                    .clone();
            }
            "--output" | "-o" => {
                index += 1;
                output_path = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--output requires a value"))?,
                ));
            }
            "--image" | "-i" => {
                index += 1;
                image_paths.push(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--image requires a value"))?,
                ));
            }
            value => prompt_parts.push(value.to_string()),
        }
        index += 1;
    }
    let output_path = output_path.ok_or_else(|| anyhow!("--output is required"))?;
    let prompt = prompt_parts.join(" ");
    if prompt.trim().is_empty() {
        return Err(anyhow!("image generation requires a prompt"));
    }
    let config = load_config(config_paths(cwd.clone(), None)?)?;
    println!(
        "{}",
        generate_image_to_path(
            &config,
            &cwd,
            &model_ref,
            &output_path,
            &prompt,
            &image_paths
        )
        .await?
    );
    Ok(())
}

fn print_image_generate_help() {
    println!(
        "usage: noel generate-image --output <file> [--model <provider/model>] [--image <file>] <prompt>\n\nGenerate images through a configured image model and write output files locally."
    );
}

fn resolve_image_model_reference(
    config: &LoadedConfig,
    reference: &str,
) -> Option<ImageModelDefinition> {
    let reference = reference.trim();
    config
        .image_models
        .iter()
        .find(|model| {
            reference == model.id
                || reference == format!("{}/{}", model.provider, model.id)
                || model.name.as_deref() == Some(reference)
        })
        .cloned()
}

fn map_image_provider_api(api: &ConfigImageProviderApi) -> AiImageProviderApi {
    match api {
        ConfigImageProviderApi::OpenRouterImages => AiImageProviderApi::OpenRouterImages,
    }
}

async fn generate_image_to_path(
    config: &LoadedConfig,
    cwd: &Path,
    model_ref: &str,
    output_path: &Path,
    prompt: &str,
    image_paths: &[PathBuf],
) -> Result<String> {
    if images_blocked(config) {
        return Err(anyhow!("images are blocked by settings"));
    }
    let model = resolve_image_model_reference(config, model_ref)
        .ok_or_else(|| anyhow!("image model not found: {model_ref}"))?;
    let auth = auth_for_provider(&config.auth, &model.provider, None).ok_or_else(|| {
        anyhow!(
            "provider {} requires auth; set auth.json or {}",
            model.provider,
            default_api_key_env(&model.provider).unwrap_or("provider API key env")
        )
    })?;
    let mut input = vec![ImageGenerationInput::Text {
        text: prompt.to_string(),
    }];
    for media in load_media_inputs(cwd, image_paths, config)? {
        input.push(ImageGenerationInput::Image { media });
    }
    let result = generate_images(
        ImageProviderConfig {
            model: ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            },
            api: map_image_provider_api(&model.api),
            base_url: model.base_url.clone(),
            auth: map_provider_auth(Some(auth)),
            output_modalities: model.output.clone(),
        },
        input,
    )
    .await?;
    let mut lines = write_generated_image_outputs(output_path, &result.output)?;
    if let Some(response_id) = result.response_id {
        lines.push(format!("response: {response_id}"));
    }
    lines.push(format!(
        "usage: input_tokens={} output_tokens={}",
        result.input_tokens, result.output_tokens
    ));
    Ok(lines.join("\n"))
}

fn write_generated_image_outputs(
    path: &Path,
    output: &[ImageGenerationOutput],
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut image_index = 0usize;
    for item in output {
        match item {
            ImageGenerationOutput::Text(text) => {
                if !text.trim().is_empty() {
                    lines.push(text.to_string());
                }
            }
            ImageGenerationOutput::Image {
                mime_type,
                data_base64,
            } => {
                image_index += 1;
                let bytes = base64::engine::general_purpose::STANDARD.decode(data_base64)?;
                let target = generated_image_output_path(path, image_index, mime_type);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, bytes)?;
                lines.push(format!("image: {}", target.display()));
            }
        }
    }
    if image_index == 0 {
        return Err(anyhow!("image provider returned no image output"));
    }
    Ok(lines)
}

fn generated_image_output_path(path: &Path, image_index: usize, mime_type: &str) -> PathBuf {
    if image_index == 1 {
        return path.to_path_buf();
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .or_else(|| image_extension_for_mime_type(mime_type).map(ToString::to_string))
        .unwrap_or_else(|| "img".to_string());
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    path.with_file_name(format!("{stem}-{image_index}.{extension}"))
}

fn image_extension_for_mime_type(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

async fn try_run_package_command() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    match command {
        "install" => {
            run_package_install(&args[1..])?;
            Ok(true)
        }
        "remove" | "uninstall" => {
            run_package_remove(&args[1..])?;
            Ok(true)
        }
        "update" => {
            run_package_update(&args[1..])?;
            Ok(true)
        }
        "list" => {
            run_package_list(&args[1..])?;
            Ok(true)
        }
        "config" => {
            run_package_config(&args[1..])?;
            Ok(true)
        }
        "login" => {
            run_auth_login(&args[1..]).await?;
            Ok(true)
        }
        "logout" => {
            run_auth_logout(&args[1..])?;
            Ok(true)
        }
        "auth" => {
            let status = auth_command::run(&args[1..]).await;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(true)
        }
        // Upstream's MCP server manager; refuse instead of prompting the model.
        "mcp" => Err(anyhow!(
            "noel mcp is not available: this port has no MCP support (see docs/rust-rewrite/non-parity-register.md)"
        )),
        // noel's own: install the bun runtime for TypeScript extension
        // hosts into the agent dir (`bin/bun`). Non-interactive.
        "bun" => {
            let rest = &args[1..];
            match rest.first().map(String::as_str) {
                Some("install") | None => {
                    let assume_yes = rest.iter().any(|arg| arg == "-y" || arg == "--yes");
                    let (paths, _import) = noel_config::ConfigPaths::discover(
                        std::env::current_dir()?,
                        None,
                    )?;
                    if !assume_yes {
                        println!(
                            "this downloads the bun runtime (~50 MB) into {}",
                            paths.agent_dir.join("bin").display()
                        );
                        print!("proceed? [y/N] ");
                        use std::io::Write as _;
                        std::io::stdout().flush()?;
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer)?;
                        if !answer.trim().eq_ignore_ascii_case("y") {
                            return Err(anyhow!("declined: bun was not installed"));
                        }
                    }
                    bun_install::install_bun(&paths.agent_dir, |_| Ok(true), true).await?;
                    Ok(true)
                }
                Some(other) => Err(anyhow!("noel bun: unknown subcommand {other} (expected install)")),
            }
        }
        _ => Ok(false),
    }
}

async fn run_auth_login(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("login");
        return Ok(());
    }
    let mut provider = None;
    let mut account = None;
    let mut api_key = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--api-key" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(anyhow!("--api-key requires a value"));
                };
                api_key = Some(value.clone());
            }
            "--account" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(anyhow!("--account requires a value"));
                };
                account = Some(value.clone());
            }
            value if value.starts_with('-') => {
                return Err(anyhow!("unknown login option: {value}"))
            }
            value => {
                if provider.replace(value.to_string()).is_some() {
                    return Err(anyhow!("unexpected login argument: {value}"));
                }
            }
        }
        index += 1;
    }
    let provider = provider.ok_or_else(|| {
        anyhow!("usage: noel login <provider> [--account <name>] [--api-key <key|env:VAR|->]")
    })?;
    let account = account.unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let cwd = std::env::current_dir()?;
    // A ChatGPT login is `openai-codex`'s credential, and its only one; an
    // API key is `openai`'s. The paths are asked for only once the arguments
    // are known to be usable (see `config_paths`).
    match (provider.as_str(), &api_key) {
        ("openai-codex", None) => {
            return run_codex_login(&config_paths(cwd, None)?, &account).await
        }
        ("openai-codex", Some(_)) => {
            return Err(anyhow!(
                "openai-codex signs in with ChatGPT and takes no API key: run `noel login openai-codex`, or store the key with `noel login openai --api-key <key|env:VAR|->`"
            ))
        }
        ("anthropic", None) => {
            return run_claude_login(&config_paths(cwd, None)?, &account).await
        }
        _ => {}
    }
    let key = match api_key {
        Some(value) if value == "-" => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input)?;
            input.trim().to_string()
        }
        Some(value) => value,
        None => default_api_key_env(&provider)
            .and_then(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .map(|_| format!("env:{name}"))
            })
            .ok_or_else(|| {
                anyhow!(
                    "usage: noel login {provider} --api-key <key|env:VAR|-> (OAuth login is available for: openai-codex, anthropic)"
                )
            })?,
    };
    if key.trim().is_empty() {
        return Err(anyhow!("api key is empty"));
    }
    let paths = config_paths(cwd, None)?;
    noel_config::update_auth_file(&paths.auth_path, |auth| {
        auth.insert(&provider, &account, AuthCredential::ApiKey { key })
    })?;
    println!(
        "stored API-key auth for {provider}{} in {}",
        account_suffix(&account),
        paths.auth_path.display()
    );
    Ok(())
}

/// Stores an API key for `provider`/`account` (the tail of the CLI login).
fn run_store_api_key(auth_path: &Path, provider: &str, account: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(anyhow!("api key is empty"));
    }
    noel_config::update_auth_file(auth_path, |auth| {
        auth.insert(
            provider,
            account,
            AuthCredential::ApiKey {
                key: key.to_owned(),
            },
        )
    })?;
    println!(
        "stored API-key auth for {provider}{} in {}",
        account_suffix(account),
        auth_path.display()
    );
    Ok(())
}

fn account_suffix(account: &str) -> String {
    if account == DEFAULT_ACCOUNT_NAME {
        String::new()
    } else {
        format!(" (account {account})")
    }
}

/// Opens a URL in the platform browser, upstream's `openBrowser`: spawn
/// the platform launcher directly, never a shell (a URL with shell
/// metacharacters must not be able to inject). Best effort: a missing
/// launcher is not an error, the URL is printed for copy regardless.
fn open_browser(url: &str) {
    #[cfg(unix)]
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    #[cfg(windows)]
    let (program, args): (&str, Vec<&str>) = ("rundll32", vec!["url.dll,FileProtocolHandler", url]);
    let _ = std::process::Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|child| {
            // The launcher must not hold noel's process group hostage: it
            // never becomes a zombie we wait on.
            let _ = child.id();
        });
}

pub(crate) fn login_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The TUI's `/login`: suspends the TUI, runs the provider's real login
/// flow on the plain terminal (the CLI command's own flow, opened browser
/// included), then reloads the config so the new account is selectable at
/// once. Returns the line to show in the transcript.
pub(crate) async fn run_tui_login(
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    config: &mut LoadedConfig,
    provider: &str,
    account: &str,
) -> Result<String> {
    let cwd = config.paths.cwd.clone();
    let auth_path = config.paths.auth_path.clone();
    let existing = noel_config::read_auth_file(&auth_path)
        .map(|auth| auth.credential(provider, account).is_some())
        .unwrap_or(false);
    let flow: std::pin::Pin<Box<dyn Future<Output = Result<String>> + '_>> = Box::pin(async {
        if existing {
            println!("account {account} of {provider} already has stored auth; logging in replaces it with whichever account the browser signs in as");
            let mut answer = String::new();
            print!("continue? [y/N] ");
            let _ = io::Write::flush(&mut io::stdout());
            io::stdin().read_line(&mut answer)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                return Ok("login cancelled".to_string());
            }
        }
        match provider {
            "openai-codex" => {
                run_codex_login_opened(&config_paths(cwd, None)?, account).await?;
            }
            "anthropic" => {
                run_claude_login_opened(&config_paths(cwd, None)?, account).await?;
            }
            _ => run_api_key_login_interactive(&auth_path, provider, account).await?,
        }
        Ok(format!("logged in {provider} as account {account}"))
    });
    let outcome = with_suspended_tui(surface, app, config, flow).await??;
    // The config reloads so the account appears in /account and the model
    // pickers without a restart, the same way /reload does.
    *config = reload_config(config)?;
    Ok(outcome)
}

/// [`run_codex_login`] with the browser opened on the device URL.
async fn run_codex_login_opened(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = oauth_login::run_codex_device_login(
        &client,
        oauth_login::CODEX_ISSUER,
        oauth_refresh::CODEX_TOKEN_URL,
        |device| {
            open_browser(&device.verification_url);
            println!(
                "opening {} (copy it if no browser opened) and enter code {}",
                device.verification_url, device.user_code
            );
            println!("waiting for authorization (the code expires in 15 minutes)...");
        },
    )
    .await?;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("openai-codex", account).is_some();
        auth.insert(
            "openai-codex",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires: tokens.expires,
                account_id: tokens.account_id,
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

/// [`run_claude_login`] with the browser opened on the authorize URL.
async fn run_claude_login_opened(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = tokio::time::timeout(
        oauth_login::LOGIN_TIMEOUT,
        oauth_login::run_claude_pkce_login(
            &client,
            oauth_login::CLAUDE_AUTHORIZE_URL,
            oauth_refresh::CLAUDE_TOKEN_URL,
            |url| {
                open_browser(url);
                println!("opening the sign-in page (copy it if no browser opened):");
                println!("{url}");
                println!("after signing in, paste the full code shown (code#state): ");
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                Ok(input)
            },
        ),
    )
    .await
    .map_err(|_| anyhow!("login timed out after 15 minutes"))??;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("anthropic", account).is_some();
        auth.insert(
            "anthropic",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires: tokens.expires,
                account_id: tokens.account_id.clone(),
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

/// An API-key login read from the plain terminal (the `--api-key -` path).
async fn run_api_key_login_interactive(
    auth_path: &Path,
    provider: &str,
    account: &str,
) -> Result<()> {
    println!("paste the {provider} API key (or an empty line to cancel):");
    let mut key = String::new();
    io::stdin().read_line(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("login cancelled"));
    }
    run_store_api_key(auth_path, provider, account, key)
}

async fn run_codex_login(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = oauth_login::run_codex_device_login(
        &client,
        oauth_login::CODEX_ISSUER,
        oauth_refresh::CODEX_TOKEN_URL,
        |device| {
            println!(
                "open {} in your browser and enter code {}",
                device.verification_url, device.user_code
            );
            println!("waiting for authorization (the code expires in 15 minutes)...");
        },
    )
    .await?;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("openai-codex", account).is_some();
        auth.insert(
            "openai-codex",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires: tokens.expires,
                account_id: tokens.account_id,
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

async fn run_claude_login(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = tokio::time::timeout(
        oauth_login::LOGIN_TIMEOUT,
        oauth_login::run_claude_pkce_login(
            &client,
            oauth_login::CLAUDE_AUTHORIZE_URL,
            oauth_refresh::CLAUDE_TOKEN_URL,
            |url| {
                println!("open this URL and sign in:\n{url}");
                println!("after signing in, paste the full code shown (code#state): ");
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                Ok(input)
            },
        ),
    )
    .await
    .map_err(|_| anyhow!("login timed out after 15 minutes"))??;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("anthropic", account).is_some();
        auth.insert(
            "anthropic",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires: tokens.expires,
                account_id: tokens.account_id.clone(),
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

fn run_auth_logout(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("logout");
        return Ok(());
    }
    let mut provider = None;
    let mut account = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--account" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(anyhow!("--account requires a value"));
                };
                account = Some(value.clone());
            }
            value if value.starts_with('-') => {
                return Err(anyhow!("unknown logout option: {value}"))
            }
            value => {
                if provider.replace(value.to_string()).is_some() {
                    return Err(anyhow!("unexpected logout argument: {value}"));
                }
            }
        }
        index += 1;
    }
    let provider =
        provider.ok_or_else(|| anyhow!("usage: noel logout <provider> [--account <name>]"))?;
    let account = account.unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    let removed = noel_config::update_auth_file(&paths.auth_path, |auth| {
        auth.remove(&provider, &account).is_some()
    })?;
    if removed {
        println!(
            "removed stored auth for {provider}{}",
            account_suffix(&account)
        );
    } else {
        println!("no stored auth for {provider}{}", account_suffix(&account));
    }
    Ok(())
}

async fn try_run_accounts_command() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) != Some("accounts") {
        return Ok(false);
    }
    run_accounts_status(&args[1..]).await?;
    Ok(true)
}

async fn run_accounts_status(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("accounts");
        return Ok(());
    }
    let mut action = None;
    let mut refresh = false;
    for arg in args {
        match arg.as_str() {
            "status" => {
                if action.replace(()).is_some() {
                    return Err(anyhow!("unexpected accounts argument: status"));
                }
            }
            "--refresh" => refresh = true,
            value => return Err(anyhow!("unknown accounts argument: {value}")),
        }
    }
    if action.is_none() {
        return Err(anyhow!("usage: noel accounts status [--refresh]"));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    let config = load_config(paths)?;
    let now = unix_seconds().unwrap_or(0) as i64;
    let rows = usage::collect_account_status(&config, refresh, None, None, now).await;
    println!("{}", usage::format_account_status(&rows, now));
    Ok(())
}

fn run_package_install(args: &[String]) -> Result<()> {
    let (local, rest) = parse_package_scope_args(args)?;
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("install");
        return Ok(());
    }
    let source = single_package_source("install", &rest)?;
    let path = package_settings_path(local)?;
    mutate_settings_packages(&path, |packages| {
        if !packages.iter().any(|package| package.source() == source) {
            packages.push(PackageSource::Simple(source.clone()));
        }
    })?;
    println!(
        "recorded package source in {} settings: {source}",
        if local { "project" } else { "user" }
    );
    Ok(())
}

fn run_package_remove(args: &[String]) -> Result<()> {
    let (local, rest) = parse_package_scope_args(args)?;
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("remove");
        return Ok(());
    }
    let source = single_package_source("remove", &rest)?;
    let path = package_settings_path(local)?;
    let mut removed = false;
    mutate_settings_packages(&path, |packages| {
        let before = packages.len();
        packages.retain(|package| package.source() != source);
        removed = packages.len() != before;
    })?;
    if removed {
        println!(
            "removed package source from {} settings: {source}",
            if local { "project" } else { "user" }
        );
    } else {
        println!(
            "package source not present in {} settings: {source}",
            if local { "project" } else { "user" }
        );
    }
    Ok(())
}

fn run_package_update(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("update");
        return Ok(());
    }
    if let Some(invalid) = args.iter().find(|arg| arg.starts_with('-')) {
        return Err(anyhow!("unknown update option: {invalid}"));
    }
    if args.len() > 1 {
        return Err(anyhow!("usage: noel update [source|self|noel]"));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd.clone(), None)?;
    let target = args.first().map(String::as_str).unwrap_or("all");
    let sources = if target == "all" {
        configured_package_sources(&paths)?
    } else if matches!(target, "self" | "noel") {
        println!("self update is not managed by the Rust no-npm package updater");
        return Ok(());
    } else {
        vec![target.to_string()]
    };
    if sources.is_empty() {
        println!("no package sources configured");
        return Ok(());
    }
    for source in sources {
        update_package_source(&cwd, &source)?;
    }
    Ok(())
}

fn configured_package_sources(paths: &ConfigPaths) -> Result<Vec<String>> {
    let mut sources = BTreeSet::new();
    for source in read_settings_packages(&paths.settings_path)? {
        sources.insert(source.source().to_string());
    }
    for source in read_settings_packages(&paths.project_settings_path)? {
        sources.insert(source.source().to_string());
    }
    Ok(sources.into_iter().collect())
}

fn update_package_source(cwd: &Path, source: &str) -> Result<()> {
    if source.contains("://") || source.starts_with("git@") {
        println!("git source is recorded but not installed locally: {source}");
        return Ok(());
    }
    let path = resolve_package_source_path(cwd, source)?;
    if !path.exists() {
        return Err(anyhow!("package path not found: {}", path.display()));
    }
    if path.join(".git").is_dir() {
        let output = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["pull", "--ff-only"])
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to update package {}:\n{}{}",
                path.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let details = String::from_utf8_lossy(&output.stdout);
        let details = details.trim();
        if details.is_empty() {
            println!("updated package {} from git", path.display());
        } else {
            println!("updated package {} from git: {details}", path.display());
        }
    } else {
        println!(
            "local package {} is not a git repository; resources reload at startup",
            path.display()
        );
    }
    Ok(())
}

fn resolve_package_source_path(cwd: &Path, source: &str) -> Result<PathBuf> {
    let path = if let Some(rest) = source.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("home directory is not available"))?
            .join(rest)
    } else {
        PathBuf::from(source)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn run_package_list(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("list");
        return Ok(());
    }
    if let Some(invalid) = args.first() {
        return Err(anyhow!("unknown list argument: {invalid}"));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    print_package_sources("user", &paths.settings_path)?;
    print_package_sources("project", &paths.project_settings_path)?;
    Ok(())
}

fn run_package_config(args: &[String]) -> Result<()> {
    let (local, rest) = parse_package_scope_args(args)?;
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("config");
        return Ok(());
    }
    match rest.as_slice() {
        [] => print_package_config_summary(),
        [summary] if summary == "show" || summary == "list" => print_package_config_summary(),
        [action, kind, name] if action == "disable" || action == "enable" => {
            let kind = normalize_resource_kind(kind)?;
            let path = package_settings_path(local)?;
            let disabled = action == "disable";
            mutate_settings_resource_state(&path, kind, name, disabled)?;
            println!(
                "{} {kind} resource in {} settings: {name}",
                if disabled { "disabled" } else { "enabled" },
                if local { "project" } else { "user" }
            );
            Ok(())
        }
        [action, ..] if action == "disable" || action == "enable" => Err(anyhow!(
            "usage: noel config {action} <extension|skill|prompt|theme> <name> [-l]"
        )),
        [invalid, ..] => Err(anyhow!("unknown config argument: {invalid}")),
    }
}

fn print_package_config_summary() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    let config = load_config(paths)?;
    println!("agent dir: {}", config.paths.agent_dir.display());
    println!(
        "project settings: {}",
        config.paths.project_settings_path.display()
    );
    println!("user settings: {}", config.paths.settings_path.display());
    println!(
        "packages: {}",
        if config.settings.packages.is_empty() {
            "-".to_string()
        } else {
            config
                .settings
                .packages
                .iter()
                .map(|package| package.source())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("skills: {}", config.skills.len());
    println!("prompts: {}", config.prompt_templates.len());
    println!("themes: {}", config.themes.len());
    println!("extensions: {}", config.extensions.len());
    println!(
        "disabled extensions: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.extensions.as_slice())
        )
    );
    println!(
        "disabled skills: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.skills.as_slice())
        )
    );
    println!(
        "disabled prompts: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.prompts.as_slice())
        )
    );
    println!(
        "disabled themes: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.themes.as_slice())
        )
    );
    Ok(())
}

fn format_disabled_resource_patterns(patterns: Option<&[String]>) -> String {
    patterns
        .filter(|patterns| !patterns.is_empty())
        .map(|patterns| patterns.join(", "))
        .unwrap_or_else(|| "-".to_string())
}

fn parse_package_scope_args(args: &[String]) -> Result<(bool, Vec<String>)> {
    let mut local = false;
    let mut rest = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            _ if arg.starts_with('-') && arg != "-h" && arg != "--help" => {
                return Err(anyhow!("unknown package option: {arg}"));
            }
            _ => rest.push(arg.clone()),
        }
    }
    Ok((local, rest))
}

fn single_package_source(command: &str, args: &[String]) -> Result<String> {
    match args {
        [source] => Ok(source.clone()),
        [] => Err(anyhow!("usage: noel {command} <source> [-l]")),
        [_, extra, ..] => Err(anyhow!("unexpected package argument: {extra}")),
    }
}

fn package_settings_path(local: bool) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    Ok(if local {
        paths.project_settings_path
    } else {
        paths.settings_path
    })
}

fn mutate_settings_packages(
    path: &Path,
    mutate: impl FnOnce(&mut Vec<PackageSource>),
) -> Result<()> {
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
    let current = settings
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| serde_json::from_value::<PackageSource>(value.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut packages = current;
    mutate(&mut packages);
    packages.sort_by(|left, right| left.source().cmp(right.source()));
    settings["packages"] = serde_json::to_value(packages)?;
    write_file_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )?;
    Ok(())
}

fn mutate_settings_resource_state(
    path: &Path,
    kind: &str,
    name: &str,
    disabled: bool,
) -> Result<()> {
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
    if !settings
        .get("disabledResources")
        .map(|value| value.is_object())
        .unwrap_or(false)
    {
        settings["disabledResources"] = serde_json::json!({});
    }
    let mut values = settings["disabledResources"]
        .get(kind)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if disabled {
        if !values.iter().any(|value| value == name) {
            values.push(name.to_string());
        }
    } else {
        values.retain(|value| value != name);
    }
    values.sort();
    values.dedup();
    settings["disabledResources"][kind] = serde_json::to_value(values)?;
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )?;
    Ok(())
}

fn normalize_resource_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "extension" | "extensions" => Ok("extensions"),
        "skill" | "skills" => Ok("skills"),
        "prompt" | "prompts" => Ok("prompts"),
        "theme" | "themes" => Ok("themes"),
        _ => Err(anyhow!(
            "unknown resource kind: {kind}; expected extension, skill, prompt, or theme"
        )),
    }
}

fn print_package_sources(scope: &str, path: &Path) -> Result<()> {
    let packages = read_settings_packages(path)?;
    if packages.is_empty() {
        println!("{scope}: no packages");
    } else {
        for package in packages {
            println!("{scope}: {}", package.source());
        }
    }
    Ok(())
}

fn read_settings_packages(path: &Path) -> Result<Vec<PackageSource>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let settings = serde_json::from_str::<Settings>(&fs::read_to_string(path)?)?;
    Ok(settings.packages)
}

fn default_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "github-copilot" => Some("COPILOT_GITHUB_TOKEN"),
        "azure" | noel_config::LEGACY_AZURE_PROVIDER => Some("AZURE_OPENAI_API_KEY"),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => Some("CLOUDFLARE_API_KEY"),
        "amazon-bedrock" => Some("AWS_BEARER_TOKEN_BEDROCK"),
        _ => None,
    }
}

fn print_package_help(command: &str) {
    match command {
        "install" => {
            println!("usage: noel install <source> [-l]\n\nRecord a package source in settings.")
        }
        "remove" => {
            println!("usage: noel remove <source> [-l]\n\nRemove a package source from settings.")
        }
        "update" => println!(
            "usage: noel update [source|self|noel]\n\nUpdate local git package sources without npm."
        ),
        "list" => {
            println!("usage: noel list\n\nList package sources from user and project settings.")
        }
        "config" => println!(
            "usage: noel config [show|list|disable <kind> <name>|enable <kind> <name>] [-l]\n\nShow active resource configuration or enable/disable resources."
        ),
        _ => {}
    }
}

fn print_auth_help(command: &str) {
    match command {
        "login" => println!(
            "usage: noel login <provider> [--account <name>] [--api-key <key|env:VAR|->]\n\nStore API-key auth in ~/.noel/auth.json. Without --api-key, noel starts the OAuth login flow for providers that support it (openai-codex, anthropic); other providers store env:<provider default> when that environment variable is present. Without --account, the account is \"default\"."
        ),
        "logout" => println!(
            "usage: noel logout <provider> [--account <name>]\n\nRemove stored provider auth. Without --account, removes the \"default\" account."
        ),
        "accounts" => println!(
            "usage: noel accounts status [--refresh]\n\nShow auth and quota status for every configured account. --refresh bypasses the 15-minute usage cache.\n\nAccount failover, in settings.json: {{ \"failover\": {{ \"enabled\": true }} }}\nWith two or more stored accounts for a provider, one sticky account carries its requests. Another takes over when the provider answers 429 before any output, until the time the provider gives for the limit to reset (else 5 minutes), or when the sticky account's 5h usage window is at failover.providers.<provider>.exhaustionThresholdPct (default 90) by the usage cache. A switch away from an account near its usage limit waits minSwitchIntervalMs (default 10000) after the last switch. What failover learns is kept in failover-state.json. Pinning an account with /account, or --api-key, turns failover off."
        ),
        _ => {}
    }
}

fn main() -> Result<()> {
    // Upstream's process markers, inherited by every command and tool noel
    // starts: a generic one naming the agent, and its own, which noel sets
    // under its name and under pi's for what was written for pi. Set before
    // any other thread exists, because the environment is process-wide.
    std::env::set_var("AI_AGENT", PRODUCT_NAME);
    std::env::set_var("NOEL_CODING_AGENT", "true");
    std::env::set_var("PI_CODING_AGENT", "true");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
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

async fn run() -> Result<()> {
    // `noel server`, `noel worker` and `noel client` (Mode 2) handle their
    // own signals.
    if let Some(code) = server::try_run(&std::env::args().collect::<Vec<_>>()).await {
        std::process::exit(code);
    }
    if let Some(code) = workspace::try_run(&std::env::args().collect::<Vec<_>>()).await {
        std::process::exit(code);
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

fn start_model_refresh(config: &LoadedConfig, offline: bool, verbose: bool) {
    if !background_refresh_allowed(&config.settings, offline, noel_config::test_isolation()) {
        return;
    }
    let ttl_hours = model_refresh_ttl_hours(&config.settings);
    if !model_cache_needs_refresh(&config.paths.model_cache_path, ttl_hours) {
        return;
    }
    let paths = config.paths.clone();
    let auth = config.auth.clone();
    tokio::spawn(async move {
        if let Err(error) = refresh_model_cache(paths, auth).await {
            if verbose {
                eprintln!("model refresh failed: {error}");
            }
        }
    });
}

/// Background model refresh runs only online and when enabled, and never
/// under the test harness (`NOEL_TEST_ISOLATION`), so tests never call
/// providers with whatever credentials they can see.
fn background_refresh_allowed(settings: &Settings, offline: bool, test_isolation: bool) -> bool {
    !offline && !test_isolation && model_refresh_enabled(settings)
}

fn model_refresh_enabled(settings: &Settings) -> bool {
    settings
        .model_refresh
        .as_ref()
        .and_then(|refresh| refresh.enabled)
        .unwrap_or(true)
}

fn model_refresh_ttl_hours(settings: &Settings) -> u64 {
    settings
        .model_refresh
        .as_ref()
        .and_then(|refresh| refresh.ttl_hours)
        .unwrap_or(24)
}

fn model_cache_needs_refresh(path: &Path, ttl_hours: u64) -> bool {
    let Ok(Some(cache)) = read_model_cache(path) else {
        return true;
    };
    model_cache_is_stale(&cache, unix_seconds(), ttl_hours)
}

fn model_cache_is_stale(cache: &ModelCache, now: Option<u64>, ttl_hours: u64) -> bool {
    if cache.version < MODEL_CACHE_VERSION {
        return true;
    }
    let Some(now) = now else {
        return true;
    };
    let ttl_seconds = ttl_hours.saturating_mul(60 * 60);
    now.saturating_sub(cache.refreshed_at) >= ttl_seconds
}

/// Outcome of one model-cache refresh pass. Fetch results are recorded per
/// provider so callers can report exactly what happened instead of guessing
/// from the cache contents.
#[derive(Debug, Default, Clone)]
struct ModelRefreshReport {
    /// Providers whose cached lists were replaced, with fetched model counts.
    refreshed: Vec<(String, usize)>,
    refreshed_providers: BTreeSet<String>,
    refreshed_models: Vec<ModelDefinition>,
    /// Providers that returned an empty list while cached models exist; the
    /// cached models were deliberately kept instead of being wiped.
    kept_cached: Vec<String>,
    /// Providers (or aux fetches) that failed, with the error text.
    failed: Vec<(String, String)>,
    claude_code_version: Option<String>,
}

impl ModelRefreshReport {
    fn record(
        &mut self,
        provider: &str,
        existing: Option<&ModelCache>,
        result: Result<Vec<ModelDefinition>>,
    ) {
        let models = match result {
            Ok(models) => models,
            Err(error) => {
                self.failed
                    .push((provider.to_string(), format!("{error:#}")));
                return;
            }
        };
        // An empty "success" is almost always a truncated or degraded
        // listing. Replacing the provider's cached models with nothing is how
        // newer models silently disappear, so keep the cache as-is.
        if models.is_empty() {
            let cached = existing
                .map(|cache| {
                    cache
                        .models
                        .iter()
                        .filter(|model| model.provider == provider)
                        .count()
                })
                .unwrap_or(0);
            if cached > 0 {
                self.kept_cached.push(provider.to_string());
            } else {
                self.failed
                    .push((provider.to_string(), "returned no models".to_string()));
            }
            return;
        }
        self.refreshed.push((provider.to_string(), models.len()));
        self.refreshed_providers.insert(provider.to_string());
        self.refreshed_models.extend(models);
    }

    fn diagnostics(&self) -> Vec<String> {
        let mut diagnostics = self
            .kept_cached
            .iter()
            .map(|provider| {
                format!("{provider} model refresh returned an empty list; kept cached models")
            })
            .collect::<Vec<_>>();
        diagnostics.extend(
            self.failed
                .iter()
                .map(|(provider, error)| format!("{provider} model refresh failed: {error}")),
        );
        diagnostics
    }

    fn nothing_happened(&self) -> bool {
        self.refreshed.is_empty()
            && self.kept_cached.is_empty()
            && self.failed.is_empty()
            && self.claude_code_version.is_none()
    }
}

async fn refresh_model_cache(
    paths: ConfigPaths,
    auth: noel_config::AuthData,
) -> Result<ModelRefreshReport> {
    let mut report = ModelRefreshReport::default();
    let cached_at_start = read_model_cache(&paths.model_cache_path).ok().flatten();

    if let Some(auth) = auth_for_provider(&auth, "anthropic", None) {
        let result = fetch_anthropic_models(auth).await;
        report.record("anthropic", cached_at_start.as_ref(), result);
    }

    // `openai` lists with its API key and `openai-codex` with its ChatGPT
    // login; neither provider has the other's credential.
    if let Some(ResolvedAuth::ApiKey(api_key)) = auth_for_provider(&auth, "openai", None) {
        let result = fetch_openai_api_models(
            "openai",
            ConfigProviderApi::OpenAiResponses,
            None,
            OPENAI_MODELS_URL,
            &api_key,
        )
        .await;
        report.record("openai", cached_at_start.as_ref(), result);
    }

    if let Some(ResolvedAuth::ChatGptOAuth {
        access_token,
        account_id,
        ..
    }) = auth_for_provider(&auth, "openai-codex", None)
    {
        let result = fetch_chatgpt_backend_models(
            "openai-codex",
            ConfigProviderApi::OpenAiCodexResponses,
            Some("https://chatgpt.com/backend-api".to_string()),
            &access_token,
            account_id.as_deref(),
        )
        .await;
        report.record("openai-codex", cached_at_start.as_ref(), result);
    }

    // The OpenAI-compatible providers whose listings are fetched, each at
    // the base URL the catalog gives its models.
    for provider in ["zai", "zai-coding", "moonshotai", "kimi-coding-openai"] {
        let Some(base_url) = noel_ai::catalog::provider_base_model(provider)
            .and_then(|model| model.base_url.clone())
        else {
            continue;
        };
        let Some(ResolvedAuth::ApiKey(api_key)) = auth_for_provider(&auth, provider, None) else {
            continue;
        };
        let models_url = format!("{base_url}/models");
        let result = fetch_openai_api_models(
            provider,
            ConfigProviderApi::OpenAi,
            Some(base_url),
            &models_url,
            &api_key,
        )
        .await;
        report.record(provider, cached_at_start.as_ref(), result);
    }

    if matches!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ClaudeCodeOAuth { .. })
    ) {
        match fetch_latest_claude_code_version().await {
            Ok(version) => {
                set_claude_code_version(&version);
                report.claude_code_version = Some(version);
            }
            Err(error) => {
                report.failed.push((
                    "claude-code".to_string(),
                    format!("version check failed: {error}"),
                ));
            }
        }
    }

    report.refreshed.sort();
    // Re-read right before writing so a refresh that finished concurrently
    // does not clobber models another process just cached.
    let existing = read_model_cache(&paths.model_cache_path).ok().flatten();
    let Some(next) = next_model_cache(existing.as_ref(), &report, unix_seconds()) else {
        return Ok(report);
    };
    write_model_cache(&paths.model_cache_path, &next)?;
    Ok(report)
}

/// Builds the next cache contents from a refresh report. Returning `None`
/// means there is nothing worth writing (no auth for any refreshable
/// provider). When every fetch failed, the previous `refreshedAt` is kept so
/// the cache stays stale and the next startup retries instead of waiting out
/// another full TTL.
fn next_model_cache(
    existing: Option<&ModelCache>,
    report: &ModelRefreshReport,
    now: Option<u64>,
) -> Option<ModelCache> {
    let existing_refreshed_at = existing.map(|cache| cache.refreshed_at).unwrap_or(0);
    let existing_version = existing.map(|cache| cache.version).unwrap_or(0);
    if report.refreshed_providers.is_empty() {
        if report.nothing_happened() {
            return None;
        }
        return Some(ModelCache {
            // Keep refreshedAt and version stale so the next startup retries
            // instead of waiting out another full TTL.
            refreshed_at: existing_refreshed_at,
            version: existing_version,
            models: existing
                .map(|cache| cache.models.clone())
                .unwrap_or_default(),
            diagnostics: report.diagnostics(),
            claude_code_version: report
                .claude_code_version
                .clone()
                .or_else(|| existing.and_then(|cache| cache.claude_code_version.clone())),
        });
    }
    let mut models = existing
        .map(|cache| cache.models.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|model| !report.refreshed_providers.contains(&model.provider))
        .collect::<Vec<_>>();
    models.extend(report.refreshed_models.iter().cloned());
    Some(ModelCache {
        refreshed_at: now.unwrap_or(existing_refreshed_at),
        version: MODEL_CACHE_VERSION,
        models,
        diagnostics: report.diagnostics(),
        claude_code_version: report
            .claude_code_version
            .clone()
            .or_else(|| existing.and_then(|cache| cache.claude_code_version.clone())),
    })
}

/// Removes the model cache so the next refresh starts from built-in models
/// plus explicit `models.json` entries only. Durable session state is
/// untouched; a bad cache entry disappears immediately.
fn reset_model_cache(path: &Path) -> Result<String> {
    match fs::remove_file(path) {
        Ok(()) => Ok("model cache cleared".to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok("model cache already clear".to_string())
        }
        Err(error) => Err(anyhow!("remove {}: {error}", path.display())),
    }
}

fn format_model_refresh_report(report: &ModelRefreshReport, config: &LoadedConfig) -> String {
    let mut lines = Vec::new();
    if report.refreshed.is_empty() {
        lines.push("no provider model lists refreshed".to_string());
    } else {
        let refreshed = report
            .refreshed
            .iter()
            .map(|(provider, count)| format!("{provider} ({count} models)"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("refreshed: {refreshed}"));
    }
    if !report.kept_cached.is_empty() {
        lines.push(format!(
            "kept cached models after empty response: {}",
            report.kept_cached.join(", ")
        ));
    }
    if !report.failed.is_empty() {
        let failed = report
            .failed
            .iter()
            .map(|(provider, error)| format!("{provider}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(format!("failed: {failed}"));
    }
    lines.push(format!("{} models available", config.models.len()));
    lines.join("\n")
}

fn format_model_refresh_after_reload(
    report: &ModelRefreshReport,
    config: &LoadedConfig,
    reload_report: &ReloadReport,
) -> String {
    let mut output = format_model_refresh_report(report, config);
    if !reload_report.active_model_valid {
        output.push_str("\nactive model is no longer available; use /model <provider/model>");
    }
    output
}

#[derive(Debug, Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModel>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModel {
    id: String,
    display_name: String,
    #[serde(default)]
    max_input_tokens: Option<u64>,
}

const ANTHROPIC_MODELS_MAX_PAGES: usize = 16;

fn anthropic_models_url(starting_after: Option<&str>) -> String {
    match starting_after {
        Some(cursor) => {
            format!("https://api.anthropic.com/v1/models?limit=1000&starting_after={cursor}")
        }
        None => "https://api.anthropic.com/v1/models?limit=1000".to_string(),
    }
}

async fn fetch_anthropic_models(auth: ResolvedAuth) -> Result<Vec<ModelDefinition>> {
    let client = reqwest::Client::new();
    let headers = anthropic_model_headers(&auth)?;
    let mut models = Vec::new();
    let mut cursor = None;
    for _ in 0..ANTHROPIC_MODELS_MAX_PAGES {
        let response = client
            .get(anthropic_models_url(cursor.as_deref()))
            .headers(headers.clone())
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("status {status}: {body}"));
        }
        let page = response.json::<AnthropicModelsResponse>().await?;
        let next = if page.has_more {
            page.last_id.clone()
        } else {
            None
        };
        models.extend(page.data.into_iter().map(|model| ModelDefinition {
            context_window: model.max_input_tokens,
            provider: "anthropic".to_string(),
            id: model.id,
            name: Some(model.display_name),
            api: ConfigProviderApi::Anthropic,
            ..ModelDefinition::default()
        }));
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(models),
        }
    }
    // Returning the partial list would replace the cache with a truncated
    // set, so treat an exhausted page budget as a failure and keep the cache.
    Err(anyhow!(
        "pagination exceeded {ANTHROPIC_MODELS_MAX_PAGES} pages"
    ))
}

const CLAUDE_CODE_NPM_LATEST_URL: &str =
    "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";

/// Claude Code publishes to npm; the registry's `latest` dist-tag tells us the
/// version string Anthropic's API expects in the `claude-cli/<version>`
/// user-agent for OAuth-gated models.
async fn fetch_latest_claude_code_version() -> Result<String> {
    let response = reqwest::Client::new()
        .get(CLAUDE_CODE_NPM_LATEST_URL)
        .header(ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status}: {body}"));
    }
    let document = response.json::<serde_json::Value>().await?;
    let version = document
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|version| is_valid_claude_code_version(version))
        .ok_or_else(|| anyhow!("missing version in npm registry response"))?;
    Ok(version.to_string())
}

fn is_valid_claude_code_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 32
        && version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
}

fn anthropic_model_headers(auth: &ResolvedAuth) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    match auth {
        ResolvedAuth::ApiKey(api_key) => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
        }
        ResolvedAuth::ClaudeCodeOAuth { access_token, .. } => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {access_token}"))?,
            );
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("oauth-2025-04-20"),
            );
        }
        ResolvedAuth::ChatGptOAuth { .. } => {
            return Err(anyhow!("unsupported Anthropic auth type"));
        }
    }
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(headers)
}

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModel {
    id: String,
}

const OPENAI_MODELS_URL: &str = "https://api.openai.com/v1/models";

async fn fetch_openai_api_models(
    provider: &str,
    api: ConfigProviderApi,
    base_url: Option<String>,
    models_url: &str,
    api_key: &str,
) -> Result<Vec<ModelDefinition>> {
    let response = reqwest::Client::new()
        .get(models_url)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status}: {body}"));
    }
    let response = response.json::<OpenAiModelsResponse>().await?;
    Ok(response
        .data
        .into_iter()
        .filter(|model| model_supported_for_provider(provider, &model.id))
        .map(|model| ModelDefinition {
            provider: provider.to_string(),
            id: model.id.clone(),
            name: Some(model_display_name(&model.id)),
            api: api.clone(),
            base_url: base_url.clone(),
            ..ModelDefinition::default()
        })
        .collect())
}

async fn fetch_chatgpt_backend_models(
    provider: &str,
    api: ConfigProviderApi,
    base_url: Option<String>,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<ModelDefinition>> {
    let endpoint = format!(
        "https://chatgpt.com/backend-api/codex/models?client_version={}",
        codex_client_version().unwrap_or_else(|| "0.145.0".to_string())
    );
    let response = reqwest::Client::new()
        .get(&endpoint)
        .headers(chatgpt_model_headers(access_token, account_id)?)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status} from {endpoint}: {body}"));
    }
    let document = response.json::<serde_json::Value>().await?;
    let ids = collect_codex_model_ids(&document, provider);
    if ids.is_empty() {
        return Err(anyhow!(
            "no selectable {provider} model IDs in response from {endpoint}"
        ));
    }
    Ok(ids
        .into_iter()
        .map(|id| ModelDefinition {
            context_window: document
                .get("models")
                .and_then(serde_json::Value::as_array)
                .and_then(|models| {
                    models.iter().find(|model| {
                        model
                            .get("slug")
                            .or_else(|| model.get("id"))
                            .and_then(serde_json::Value::as_str)
                            == Some(id.as_str())
                    })
                })
                .and_then(|model| {
                    model
                        .get("context_window")
                        .or_else(|| model.get("contextWindow"))
                })
                .and_then(serde_json::Value::as_u64),
            provider: provider.to_string(),
            name: Some(model_display_name(&id)),
            id,
            api: api.clone(),
            base_url: base_url.clone(),
            ..ModelDefinition::default()
        })
        .collect())
}

fn chatgpt_model_headers(access_token: &str, account_id: Option<&str>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}"))?,
    );
    if let Some(account_id) = account_id {
        headers.insert("chatgpt-account-id", HeaderValue::from_str(account_id)?);
    }
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert("User-Agent", HeaderValue::from_static("pi"));
    headers.insert(
        "OpenAI-Beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn collect_codex_model_ids(value: &serde_json::Value, provider: &str) -> BTreeSet<String> {
    let models = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array());
    models
        .into_iter()
        .flatten()
        .filter(|model| {
            model
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .map(|visibility| visibility == "list")
                .unwrap_or(true)
        })
        .filter_map(|model| {
            ["slug", "id", "model"]
                .into_iter()
                .find_map(|key| model.get(key).and_then(serde_json::Value::as_str))
        })
        .filter(|id| model_supported_for_provider(provider, id))
        .map(ToString::to_string)
        .collect()
}

fn model_supported_for_provider(provider: &str, id: &str) -> bool {
    let excludes_non_chat = || {
        id.contains("audio")
            || id.contains("realtime")
            || id.contains("transcribe")
            || id.contains("tts")
            || id.contains("image")
            || id.contains("moderation")
            || id.contains("embedding")
    };
    match provider {
        "openai" => {
            (id.starts_with("gpt-")
                || id.starts_with("o1")
                || id.starts_with("o3")
                || id.starts_with("o4"))
                && !excludes_non_chat()
        }
        "openai-codex" => (id.starts_with("gpt-") || id.contains("codex")) && !excludes_non_chat(),
        "zai" | "zai-coding" | "moonshotai" | "kimi-coding-openai" => !excludes_non_chat(),
        _ => false,
    }
}

fn model_display_name(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if part.chars().all(|ch| ch.is_ascii_digit() || ch == '.') => {
                    format!("{first}{}", chars.collect::<String>())
                }
                Some(first) => format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    chars.collect::<String>()
                ),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

fn expand_message_inputs(cwd: &Path, messages: &[String]) -> Result<String> {
    let mut parts = Vec::new();
    for message in messages {
        if let Some(path) = message.strip_prefix('@').filter(|path| !path.is_empty()) {
            parts.push(resolve_text_or_file(cwd, path)?);
        } else {
            parts.push(message.clone());
        }
    }
    Ok(parts.join(" "))
}

fn load_media_inputs(
    cwd: &Path,
    paths: &[PathBuf],
    config: &LoadedConfig,
) -> Result<Vec<MediaInput>> {
    if images_blocked(config) && !paths.is_empty() {
        return Err(anyhow!("images are blocked by settings"));
    }
    paths
        .iter()
        .map(|path| load_media_input(cwd, path, config))
        .collect()
}

fn images_auto_resize(config: &LoadedConfig) -> bool {
    config
        .settings
        .images
        .as_ref()
        .and_then(|images| images.auto_resize)
        .unwrap_or(true)
}

fn images_blocked(config: &LoadedConfig) -> bool {
    config
        .settings
        .images
        .as_ref()
        .and_then(|images| images.block_images)
        .unwrap_or(false)
}

fn load_media_input(cwd: &Path, path: &Path, config: &LoadedConfig) -> Result<MediaInput> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let bytes = fs::read(&path)?;
    media_input_from_bytes(
        bytes,
        media_mime_type(&path)?,
        Some(path.display().to_string()),
        config,
    )
}

fn media_input_from_bytes(
    mut bytes: Vec<u8>,
    mut mime_type: String,
    path: Option<String>,
    config: &LoadedConfig,
) -> Result<MediaInput> {
    if images_auto_resize(config) {
        if let Some(resized) = resize_image_if_needed(&bytes, &mime_type)? {
            bytes = resized.bytes;
            mime_type = resized.mime_type;
        }
    }
    let (width, height) = image_dimensions(&bytes, &mime_type).unwrap_or((None, None));
    Ok(MediaInput {
        mime_type,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        path,
        width,
        height,
    })
}

fn clipboard_media(bytes: Vec<u8>, config: &LoadedConfig) -> Result<MediaInput> {
    if images_blocked(config) {
        return Err(anyhow!("images are blocked by settings"));
    }
    let format = image::guess_format(&bytes)?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return Err(anyhow!("unsupported clipboard image format")),
    };
    // Validate dimensions/pixels before attaching arbitrary clipboard bytes.
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode()?;
    media_input_from_bytes(bytes, mime.into(), None, config)
}

async fn paste_clipboard_image(app: &mut TuiApp, config: &LoadedConfig) {
    if images_blocked(config) {
        app.push(TuiEntryKind::Error, "images are blocked by settings");
        return;
    }
    match clipboard::image_bytes()
        .await
        .and_then(|bytes| clipboard_media(bytes, config))
    {
        Ok(media) => app.input_media.push(media),
        Err(error) => app.push(TuiEntryKind::Error, format!("clipboard paste: {error}")),
    }
    app.quit_armed_since = None;
}

const MAX_AUTO_RESIZE_IMAGE_DIMENSION: u32 = 2000;

struct ResizedImage {
    bytes: Vec<u8>,
    mime_type: String,
}

fn resize_image_if_needed(bytes: &[u8], mime_type: &str) -> Result<Option<ResizedImage>> {
    let Some(input_format) = image_format_for_mime_type(mime_type) else {
        return Ok(None);
    };
    let image = match image::load_from_memory_with_format(bytes, input_format) {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    if image.width() <= MAX_AUTO_RESIZE_IMAGE_DIMENSION
        && image.height() <= MAX_AUTO_RESIZE_IMAGE_DIMENSION
    {
        return Ok(None);
    }
    let resized = image.thumbnail(
        MAX_AUTO_RESIZE_IMAGE_DIMENSION,
        MAX_AUTO_RESIZE_IMAGE_DIMENSION,
    );
    let output_format = if mime_type == "image/jpeg" {
        image::ImageFormat::Jpeg
    } else {
        image::ImageFormat::Png
    };
    let mut output = Cursor::new(Vec::new());
    resized.write_to(&mut output, output_format)?;
    Ok(Some(ResizedImage {
        bytes: output.into_inner(),
        mime_type: if output_format == image::ImageFormat::Jpeg {
            "image/jpeg".to_string()
        } else {
            "image/png".to_string()
        },
    }))
}

fn image_format_for_mime_type(mime_type: &str) -> Option<image::ImageFormat> {
    match mime_type {
        "image/png" => Some(image::ImageFormat::Png),
        "image/jpeg" => Some(image::ImageFormat::Jpeg),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/webp" => Some(image::ImageFormat::WebP),
        _ => None,
    }
}

fn media_mime_type(path: &Path) -> Result<String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_lowercase())
        .as_deref()
    {
        Some("png") => Ok("image/png".to_string()),
        Some("jpg") | Some("jpeg") => Ok("image/jpeg".to_string()),
        Some("gif") => Ok("image/gif".to_string()),
        Some("webp") => Ok("image/webp".to_string()),
        Some(extension) => Err(anyhow!("unsupported image extension: {extension}")),
        None => Err(anyhow!("image path has no extension: {}", path.display())),
    }
}

fn image_dimensions(bytes: &[u8], mime_type: &str) -> Option<(Option<u32>, Option<u32>)> {
    match mime_type {
        "image/png" => png_dimensions(bytes).map(|(width, height)| (Some(width), Some(height))),
        "image/jpeg" => jpeg_dimensions(bytes).map(|(width, height)| (Some(width), Some(height))),
        _ => Some((None, None)),
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    ))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut index = 2;
    while index + 9 < bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32;
            return Some((width, height));
        }
        if length < 2 {
            return None;
        }
        index += length + 2;
    }
    None
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
            std::process::exit(EXIT_SESSION_LOCKED);
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

/// The TUI's hold on the terminal. Dropping it restores the terminal unless a
/// fatal signal already did (see `signals`).
struct TerminalRestore;

impl TerminalRestore {
    fn claim(owned_screen: bool) -> Self {
        signals::claim_tui_terminal(owned_screen);
        Self
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        signals::release_tui_terminal();
    }
}

/// Leaves every terminal mode the TUI enabled. The cursor is shown too: on a
/// signal exit ratatui's own drop, which normally does it, never runs.
fn restore_terminal_modes(out: &mut impl Write, owned_screen: bool) {
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableBracketedPaste);
    if owned_screen {
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
    }
    let _ = execute!(out, crossterm::cursor::Show);
    let _ = disable_raw_mode();
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiEntryKind {
    Thinking,
    System,
    User,
    Assistant,
    Tool,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiEntry {
    kind: TuiEntryKind,
    text: String,
}

/// A listed model's thinking levels, lowest first, and the level selecting
/// it gives the session when the user adjusts nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelThinking {
    levels: Vec<&'static str>,
    switch_level: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiSelectorState {
    kind: String,
    title: String,
    items: Vec<SelectorItem>,
    filtered_indices: Vec<usize>,
    selected: usize,
    query: String,
    /// The thinking level the user adjusted to with left/right; `None`
    /// until then, when selecting a model gives the session the level a
    /// model switch does.
    thinking_level: Option<String>,
    /// What each listed model supports of thinking, by row value.
    model_thinking: BTreeMap<String, ModelThinking>,
    /// Codex-style two-layer model picker: selecting a provider pushes this
    /// selector here and shows the provider's models; leaving the model layer
    /// (esc) pops back instead of closing the whole selector.
    parent: Option<Box<TuiSelectorState>>,
    /// Provider filter for the model layer (e.g. "zai-coding").
    provider_filter: Option<String>,
}

impl TuiSelectorState {
    /// A selector a worker sent ([`TuiSelectorState::to_json`]).
    fn from_json(value: &serde_json::Value) -> Option<Self> {
        let items = value["items"].as_array()?.iter().map(|item| SelectorItem {
            label: item["label"].as_str().unwrap_or_default().to_owned(),
            value: item["value"].as_str().unwrap_or_default().to_owned(),
            active: item["active"] == true,
        });
        Some(Self::new(
            value["kind"].as_str()?,
            Selector::new(value["title"].as_str().unwrap_or_default(), items.collect()),
            value["query"].as_str().unwrap_or_default(),
            None,
        ))
    }

    /// The selector as a worker sends it to a client: its kind, title,
    /// query and rows; a pick comes back as `/select <kind> <value>`.
    fn to_json(&self) -> serde_json::Value {
        let items = self.items.iter().map(|item| {
            serde_json::json!({"label": item.label, "value": item.value, "active": item.active})
        });
        serde_json::json!({
            "kind": self.kind,
            "title": self.title,
            "query": self.query,
            "items": items.collect::<Vec<_>>(),
        })
    }
}

impl TuiSelectorState {
    fn new(
        kind: impl Into<String>,
        selector: Selector,
        query: impl Into<String>,
        thinking_level: Option<String>,
    ) -> Self {
        let mut state = Self {
            kind: kind.into(),
            title: selector.title,
            items: selector.items,
            filtered_indices: Vec::new(),
            selected: 0,
            query: query.into(),
            thinking_level,
            model_thinking: BTreeMap::new(),
            parent: None,
            provider_filter: None,
        };
        state.refresh_filter();
        state
    }

    /// Records what the listed models support of thinking, so the picker
    /// can show and adjust the level a selection applies.
    fn with_model_thinking(mut self, config: &LoadedConfig, runtime: &Runtime) -> Self {
        let before = effective_thinking_level(runtime, config);
        let by_value = config
            .models
            .iter()
            .map(|model| (format!("{}/{}", model.provider, model.id), model))
            .collect::<BTreeMap<_, _>>();
        self.model_thinking = self
            .items
            .iter()
            .filter_map(|item| {
                let model = by_value.get(&item.value)?;
                // The level a switch to this model gives the session
                // (`switch_model`).
                let requested = [
                    config
                        .settings
                        .model_thinking_level(&model.provider, &model.id),
                    config.settings.default_thinking_level.as_deref(),
                ]
                .into_iter()
                .flatten()
                .find_map(normalized_thinking_level)
                .unwrap_or(before);
                Some((
                    item.value.clone(),
                    ModelThinking {
                        levels: model.supported_thinking_levels(),
                        switch_level: model.clamp_thinking_level(requested),
                    },
                ))
            })
            .collect();
        self
    }

    /// The selected model's thinking, if it has levels to choose from.
    fn selected_model_thinking(&self) -> Option<&ModelThinking> {
        self.model_thinking
            .get(&self.selected_item()?.value)
            .filter(|thinking| thinking.levels.len() > 1)
    }

    fn refresh_filter(&mut self) {
        let query = self.query.to_lowercase();
        self.filtered_indices = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if query.is_empty()
                    || item.label.to_lowercase().contains(&query)
                    || item.value.to_lowercase().contains(&query)
                {
                    Some(index)
                } else {
                    None
                }
            })
            .collect();
        if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len().saturating_sub(1);
        }
    }

    fn selected_item(&self) -> Option<&SelectorItem> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(len - 1)
        };
    }

    fn jump_to_start(&mut self) {
        self.selected = 0;
    }

    fn jump_to_end(&mut self) {
        let len = self.filtered_indices.len();
        self.selected = len.saturating_sub(1);
    }

    fn push_query_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
        self.refresh_filter();
    }

    fn pop_query_char(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.refresh_filter();
    }

    /// The level selecting the highlighted model applies: the one the user
    /// adjusted to, clamped to the model, else the one a model switch
    /// gives. `None` for a model without thinking levels.
    fn selected_thinking_level(&self) -> Option<String> {
        let thinking = self.selected_model_thinking()?;
        let level = match self
            .thinking_level
            .as_deref()
            .and_then(normalized_thinking_level)
        {
            Some(adjusted) => noel_ai::catalog::clamp_to_supported(&thinking.levels, adjusted),
            None => thinking.switch_level,
        };
        Some(level.to_string())
    }
}

fn model_ref_from_value(value: &str) -> Option<ModelRef> {
    let (provider, id) = value.split_once('/')?;
    Some(ModelRef {
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

#[derive(Debug, Default)]
struct TuiApp {
    entries: Vec<TuiEntry>,
    transcript_scrollback: TranscriptScrollback,
    owned_screen: bool,
    transcript_view: RefCell<TranscriptView>,
    input: String,
    input_media: Vec<MediaInput>,
    input_cursor: usize,
    quit_armed_since: Option<Instant>,
    editor_state: EditorState,
    last_shell_command: Option<String>,
    multiline: Option<Vec<String>>,
    selector: Option<TuiSelectorState>,
    header_line: String,
    /// The session's workspace on another machine, which the footer shows
    /// with its connection state in place of the directory.
    workspace: Option<SessionWorkspace>,
    status: String,
    show_hardware_cursor: bool,
    live_entry_index: Option<usize>,
    history_cursor: Option<usize>,
    history_draft: Option<InputDraft>,
    input_pastes: Vec<CollapsedPaste>,
    next_paste_id: usize,
    todos: Vec<TodoItem>,
    todos_expanded: bool,
    previews_expanded: bool,
    live_thinking_index: Option<usize>,
    live_tool_indices: Vec<usize>,
    /// The running `codemode` call's nested calls as rows, kept above its
    /// result once it ends.
    nested_rows: Option<String>,
    queued_messages: Vec<String>,
    turn_running: bool,
    /// True once the running turn has made a tool call; a prose-only turn
    /// has no interior round boundary, so ctrl+s steering only lands after
    /// the response finishes and the hint must not promise a boundary.
    turn_saw_tools: bool,
    diff_panel: Option<DiffPanelState>,
}

// Paste ranges refer to the full input, never to marker text. Submission,
// queues, history, and external editors therefore always receive real text.
#[derive(Debug, Clone)]
struct CollapsedPaste {
    start: usize,
    end: usize,
    id: usize,
    lines: usize,
}

impl CollapsedPaste {
    fn marker(&self) -> String {
        format!(
            "[paste #{} +{} {}]",
            self.id,
            self.lines,
            if self.lines == 1 { "line" } else { "lines" }
        )
    }
}

#[derive(Debug)]
struct InputDraft {
    text: String,
    cursor: usize,
    pastes: Vec<CollapsedPaste>,
}

const LARGE_PASTE_CHARS: usize = 1_000;

#[derive(Debug)]
struct DiffPanelState {
    files: Vec<DiffFileEntry>,
    selected: usize,
    detail: Option<DiffDetail>,
}

#[derive(Debug)]
struct DiffFileEntry {
    path: String,
    added: Option<u64>,
    removed: Option<u64>,
    untracked: bool,
}

#[derive(Debug)]
struct DiffDetail {
    path: String,
    lines: Vec<String>,
    scroll: usize,
}

impl TuiApp {
    fn push(&mut self, kind: TuiEntryKind, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            // Expansion is for inspecting existing output, not an opt-in to
            // dump every tool result from subsequent prompts onto the screen.
            if kind == TuiEntryKind::User {
                self.previews_expanded = false;
            }
            self.transcript_view
                .get_mut()
                .invalidate(self.entries.len());
            self.entries.push(TuiEntry { kind, text });
        }
    }

    fn history_previous(&mut self) {
        let history = self.editor_state.history();
        if history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            Some(cursor) => cursor.saturating_sub(1),
            None => {
                self.history_draft = Some(InputDraft {
                    text: self.input.clone(),
                    cursor: self.input_cursor,
                    pastes: self.input_pastes.clone(),
                });
                history.len().saturating_sub(1)
            }
        };
        self.history_cursor = Some(next);
        self.input = history[next].clone();
        self.input_cursor = self.input.len();
        self.input_pastes.clear();
    }

    fn history_next(&mut self) {
        let Some(cursor) = self.history_cursor else {
            return;
        };
        let history = self.editor_state.history();
        if cursor + 1 < history.len() {
            let next = cursor + 1;
            self.history_cursor = Some(next);
            self.input = history[next].clone();
            self.input_pastes.clear();
        } else {
            self.history_cursor = None;
            if let Some(draft) = self.history_draft.take() {
                self.input = draft.text;
                self.input_cursor = draft.cursor;
                self.input_pastes = draft.pastes;
                return;
            }
            self.input.clear();
            self.input_pastes.clear();
        }
        self.input_cursor = self.input.len();
    }

    fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft = None;
    }

    fn paste_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let start = self.input_cursor;
        self.insert_input_str(&text);
        if text.contains('\n') || text.chars().count() > LARGE_PASTE_CHARS {
            self.next_paste_id += 1;
            self.input_pastes.push(CollapsedPaste {
                start,
                end: self.input_cursor,
                id: self.next_paste_id,
                lines: text.lines().count().max(1),
            });
            self.input_pastes.sort_by_key(|paste| paste.start);
        }
    }

    fn display_input(&self) -> (String, usize) {
        let mut display = String::new();
        let mut source_start = 0;
        let mut cursor = None;
        for paste in &self.input_pastes {
            if cursor.is_none() && self.input_cursor <= paste.start {
                cursor = Some(display.len() + self.input_cursor - source_start);
            }
            display.push_str(&self.input[source_start..paste.start]);
            display.push_str(&paste.marker());
            if cursor.is_none() && self.input_cursor < paste.end {
                cursor = Some(display.len());
            }
            source_start = paste.end;
        }
        let cursor = cursor.unwrap_or_else(|| display.len() + self.input_cursor - source_start);
        display.push_str(&self.input[source_start..]);
        (display, cursor)
    }

    fn insert_input_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let cursor = self.input_cursor;
        self.input.insert_str(cursor, text);
        let inserted = text.len();
        self.input_pastes.retain_mut(|paste| {
            if paste.start < cursor && cursor < paste.end {
                return false;
            }
            if paste.start >= cursor {
                paste.start += inserted;
                paste.end += inserted;
            }
            true
        });
        self.input_cursor += inserted;
        self.reset_history_navigation();
    }

    #[cfg(test)]
    fn set_input(&mut self, value: impl Into<String>) {
        self.input = value.into();
        self.input_cursor = self.input.len();
        self.input_pastes.clear();
        self.reset_history_navigation();
    }

    fn push_input_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.insert_input_str(ch.encode_utf8(&mut buf));
    }

    fn pop_input_char(&mut self) {
        let start = self
            .input_pastes
            .iter()
            .find(|paste| paste.end == self.input_cursor)
            .map(|paste| paste.start)
            .or_else(|| {
                self.input[..self.input_cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map(|(offset, _)| offset)
            });
        if let Some(start) = start {
            self.remove_input_range(start, self.input_cursor);
        }
    }

    fn delete_input_char(&mut self) {
        let end = self
            .input_pastes
            .iter()
            .find(|paste| paste.start == self.input_cursor)
            .map(|paste| paste.end)
            .or_else(|| {
                self.input[self.input_cursor..]
                    .graphemes(true)
                    .next()
                    .map(|grapheme| self.input_cursor + grapheme.len())
            });
        if let Some(end) = end {
            self.remove_input_range(self.input_cursor, end);
        }
    }

    fn remove_input_range(&mut self, start: usize, end: usize) {
        self.input.drain(start..end);
        self.input_cursor = start;
        self.input_pastes.retain_mut(|paste| {
            if paste.start < end && paste.end > start {
                return false;
            }
            if paste.start >= end {
                paste.start -= end - start;
                paste.end -= end - start;
            }
            true
        });
        self.reset_history_navigation();
    }

    fn move_cursor_left(&mut self) {
        if let Some(paste) = self
            .input_pastes
            .iter()
            .find(|paste| paste.end == self.input_cursor)
        {
            self.input_cursor = paste.start;
        } else if let Some((offset, _)) = self.input[..self.input_cursor]
            .grapheme_indices(true)
            .next_back()
        {
            self.input_cursor = offset;
        }
    }

    fn move_cursor_right(&mut self) {
        if let Some(paste) = self
            .input_pastes
            .iter()
            .find(|paste| paste.start == self.input_cursor)
        {
            self.input_cursor = paste.end;
        } else if let Some(grapheme) = self.input[self.input_cursor..].graphemes(true).next() {
            self.input_cursor += grapheme.len();
        }
    }

    fn input_newline_is_visible(&self, offset: usize) -> bool {
        !self
            .input_pastes
            .iter()
            .any(|paste| paste.start <= offset && offset < paste.end)
    }

    fn cursor_to_line_start(&mut self) {
        self.input_cursor = self.input[..self.input_cursor]
            .match_indices('\n')
            .rfind(|(offset, _)| self.input_newline_is_visible(*offset))
            .map(|(index, _)| index + 1)
            .unwrap_or(0);
    }

    fn cursor_to_line_end(&mut self) {
        self.input_cursor = self.input[self.input_cursor..]
            .match_indices('\n')
            .find(|(offset, _)| self.input_newline_is_visible(self.input_cursor + *offset))
            .map(|(index, _)| self.input_cursor + index)
            .unwrap_or(self.input.len());
    }

    fn clear_input(&mut self) {
        self.input_media.clear();
        self.input.clear();
        self.input_cursor = 0;
        self.input_pastes.clear();
        self.reset_history_navigation();
    }

    fn clear_visible(&mut self) {
        self.entries.clear();
        self.transcript_scrollback = TranscriptScrollback::default();
        self.transcript_view.get_mut().reset();
        self.clear_input();
        self.multiline = None;
        self.selector = None;
        self.live_entry_index = None;
        self.live_thinking_index = None;
        self.live_tool_indices.clear();
        self.nested_rows = None;
        self.previews_expanded = false;
    }

    fn insert_input_newline(&mut self) {
        self.insert_input_str("\n");
    }

    /// Double-press gate for quit keys on an empty input: the first press
    /// arms a confirmation window; a second press inside the window quits.
    fn quit_requested(&mut self, now: Instant) -> bool {
        let armed = self
            .quit_armed_since
            .map(|since| now.saturating_duration_since(since) < QUIT_CONFIRM_WINDOW)
            .unwrap_or(false);
        if armed {
            return true;
        }
        self.quit_armed_since = Some(now);
        false
    }

    fn push_placeholder(&mut self, kind: TuiEntryKind, text: impl Into<String>) -> usize {
        self.transcript_view
            .get_mut()
            .invalidate(self.entries.len());
        self.entries.push(TuiEntry {
            kind,
            text: text.into(),
        });
        let index = self.entries.len() - 1;
        self.live_entry_index = Some(index);
        index
    }

    fn replace_entry(&mut self, index: usize, text: impl Into<String>) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.text = text.into();
            self.transcript_view.get_mut().replaced(index);
            if let Some(progress) = self.transcript_scrollback.entries.get_mut(index) {
                *progress = EntryScrollback::default();
            }
        }
    }

    fn append_entry(&mut self, index: usize, text: &str) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.text.push_str(text);
            self.transcript_view.get_mut().invalidate(index);
        }
    }

    fn remove_entry(&mut self, index: usize) {
        self.entries.remove(index);
        self.transcript_scrollback.remove(index);
        self.transcript_view.get_mut().remove(index);
    }

    fn finish_live_entry(&mut self) {
        self.live_thinking_index = None;
        self.turn_running = false;
        self.turn_saw_tools = false;
        self.live_entry_index = None;
    }

    #[cfg(test)]
    fn finalized_entry_count(&self) -> usize {
        self.live_entry_index.unwrap_or(self.entries.len())
    }

    fn transcript_text(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                let label = match entry.kind {
                    TuiEntryKind::Thinking => "thinking",
                    TuiEntryKind::System => "system",
                    TuiEntryKind::User => "user",
                    TuiEntryKind::Assistant => "assistant",
                    TuiEntryKind::Tool => "tool",
                    TuiEntryKind::Error => "error",
                };
                format!("{label}> {}", entry.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// Scrollback is a rendering watermark, not conversation state. Logical-line
// and UTF-8 offsets survive rewrapping at a different terminal width. Tracking
// only a count of painted rows would duplicate or skip text after a resize.
#[derive(Debug, Default)]
struct TranscriptScrollback {
    entries: Vec<EntryScrollback>,
}

#[derive(Debug, Default, Clone, Copy)]
struct EntryScrollback {
    position: TranscriptPosition,
    complete: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TranscriptPosition {
    line: usize,
    byte: usize,
}

impl TranscriptScrollback {
    fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }
}

#[derive(Debug)]
struct TranscriptRow {
    line: Line<'static>,
    start: TranscriptPosition,
    end: TranscriptPosition,
    // Preview text can stand for a larger source range, not a literal slice.
    synthetic: bool,
}

fn wrap_positioned_line(line: Line<'static>, index: usize, width: usize) -> Vec<TranscriptRow> {
    let width = width.max(1);
    let line_style = line.style;
    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut cells = 0;
    let mut byte = 0;
    let mut start = TranscriptPosition {
        line: index,
        byte: 0,
    };
    for span in line.spans {
        let mut segment = String::new();
        for grapheme in span.content.graphemes(true) {
            // Expand tabs, and keep terminal control bytes out of painted text.
            let text = if grapheme == "\t" { "    " } else { grapheme };
            let text = text
                .chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>();
            let used = text.width();
            if cells > 0 && cells + used > width {
                push_transcript_segment(&mut current, &mut segment, span.style);
                let end = TranscriptPosition { line: index, byte };
                rows.push(TranscriptRow {
                    line: Line::from(std::mem::take(&mut current)).style(line_style),
                    start,
                    end,
                    synthetic: false,
                });
                start = end;
                cells = 0;
            }
            segment.push_str(&text);
            byte += grapheme.len();
            cells += used;
        }
        push_transcript_segment(&mut current, &mut segment, span.style);
    }
    rows.push(TranscriptRow {
        line: Line::from(current).style(line_style),
        start,
        end: if byte == 0 {
            TranscriptPosition {
                line: index + 1,
                byte: 0,
            }
        } else {
            TranscriptPosition { line: index, byte }
        },
        synthetic: false,
    });
    rows
}

fn collapsed_tool_rows(
    logical: &[Line<'static>],
    rows: Vec<TranscriptRow>,
    width: usize,
    palette: &ThemePalette,
) -> Vec<TranscriptRow> {
    // Keep command/path headers on one row so a long command cannot consume
    // the preview. On large results show the final output row, like Kimi.
    let mut header = wrap_positioned_line(logical[0].clone(), 0, usize::MAX)
        .remove(0)
        .line;
    let output = rows
        .into_iter()
        .filter(|row| row.start.line > 0)
        .collect::<Vec<_>>();
    let folded = output.len() > 2;
    if folded {
        let suffix = format!(" · {} more lines", output.len() - 1);
        header = clip_line_width(header, width.saturating_sub(suffix.width()));
        header.spans.push(Span::styled(suffix, palette.secondary()));
    }
    let mut preview = vec![TranscriptRow {
        line: clip_line_width(header, width),
        start: TranscriptPosition::default(),
        end: TranscriptPosition { line: 1, byte: 0 },
        synthetic: true,
    }];
    if folded {
        let tail = logical.last().expect("tool output");
        let text = tail
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let line = wrap_positioned_line(
            Line::from(Span::styled(
                format!("  … {}", text.trim()),
                palette.secondary(),
            )),
            0,
            usize::MAX,
        )
        .remove(0)
        .line;
        preview.push(TranscriptRow {
            line: clip_line_width(line, width),
            start: TranscriptPosition { line: 1, byte: 0 },
            end: output.last().expect("folded output").end,
            synthetic: true,
        });
    } else {
        preview.extend(output);
    }
    preview
}

fn pending_entry_rows(
    entry: &TuiEntry,
    index: usize,
    width: usize,
    palette: &ThemePalette,
    state: &PreviewState,
    progress: EntryScrollback,
) -> Vec<TranscriptRow> {
    if progress.complete {
        return Vec::new();
    }
    let mut logical = render_entry_lines(std::slice::from_ref(entry), palette);
    if logical.is_empty() {
        return Vec::new();
    }
    logical.pop(); // spacer is added after preview selection
    let mut rows = Vec::new();
    for (line_index, line) in logical.iter().cloned().enumerate() {
        let mut wrapped = wrap_positioned_line(line, line_index, width);
        if line_index + 1 < logical.len() || entry.text.ends_with('\n') {
            wrapped.last_mut().expect("logical row").end = TranscriptPosition {
                line: line_index + 1,
                byte: 0,
            };
        }
        rows.extend(wrapped);
    }
    let collapsed = state.expanded_from.is_none_or(|start| index < start);
    if collapsed && entry.kind == TuiEntryKind::Tool {
        rows = collapsed_tool_rows(&logical, rows, width, palette);
    }
    // Markdown emphasis restyles wrapped assistant rows in place. Selection
    // geometry stays anchored to the plain logical lines; only span styles
    // (and link display text) differ.
    {
        let logical_lines: Vec<String> = logical
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let base = Style::default().fg(palette.foreground);
        restyle_rows_with_markdown(
            &mut rows,
            entry,
            |line| logical_lines.get(line).cloned(),
            base,
            palette.accent,
        );
    }
    let limit = if entry.kind == TuiEntryKind::Thinking {
        2
    } else {
        usize::MAX
    };
    if collapsed && rows.len() > limit {
        let hidden = rows.len() - limit;
        if state.live_thinking == Some(index) {
            rows = rows.split_off(hidden);
        } else {
            let start = rows[limit].start;
            let end = rows.last().expect("nonempty preview").end;
            rows.truncate(limit);
            rows.push(TranscriptRow {
                line: clip_line_width(
                    Line::from(Span::styled(
                        format!("  … ({hidden} more lines, ctrl+o to expand)"),
                        palette.secondary(),
                    )),
                    width,
                ),
                start,
                end,
                synthetic: true,
            });
        }
    }
    rows.push(TranscriptRow {
        line: Line::from(""),
        start: TranscriptPosition {
            line: logical.len(),
            byte: 0,
        },
        end: TranscriptPosition {
            line: logical.len() + 1,
            byte: 0,
        },
        synthetic: false,
    });
    // A new wrap can straddle the watermark after a resize. Trim only the
    // archived prefix, using source bytes (not tab-expanded painted bytes).
    let mut pending = Vec::new();
    for mut row in rows {
        if row.end <= progress.position {
            continue;
        }
        if !row.synthetic && row.start < progress.position && progress.position.line < logical.len()
        {
            let source = &logical[progress.position.line];
            let end = if row.end.line == progress.position.line {
                row.end.byte
            } else {
                source.spans.iter().map(|span| span.content.len()).sum()
            };
            let mut offset = 0;
            let spans = source
                .spans
                .iter()
                .filter_map(|span| {
                    let from = progress
                        .position
                        .byte
                        .saturating_sub(offset)
                        .min(span.content.len());
                    let to = end.saturating_sub(offset).min(span.content.len());
                    offset += span.content.len();
                    (from < to)
                        .then(|| Span::styled(span.content[from..to].to_string(), span.style))
                })
                .collect::<Vec<_>>();
            row.line = wrap_positioned_line(Line::from(spans).style(source.style), 0, width)
                .into_iter()
                .map(|row| row.line)
                .next()
                .unwrap_or_default();
            row.start = progress.position;
        }
        pending.push(row);
    }
    pending
}

fn transcript_preview_state(app: &TuiApp) -> PreviewState {
    PreviewState {
        expanded_from: app
            .previews_expanded
            .then(|| recent_turn_start(&app.entries)),
        live_thinking: app.live_thinking_index,
    }
}

fn transcript_overflow(
    app: &mut TuiApp,
    config: &LoadedConfig,
    area: Rect,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    // Overlays and narrower diff views are temporary: do not irreversibly
    // archive transcript text merely because a panel covers it.
    if area.width == 0 || area.height == 0 || app.selector.is_some() || app.diff_panel.is_some() {
        return Vec::new();
    }
    app.transcript_scrollback
        .entries
        .resize(app.entries.len(), EntryScrollback::default());
    let state = transcript_preview_state(app);
    let mut chunks = Vec::new();
    let mut height = 0usize;
    // Include all pending entries, not only overscan: a provider can deliver
    // several screenfuls in one delta, and session replay is a single paint.
    for (index, entry) in app.entries.iter().enumerate() {
        if config.settings.hide_thinking_block.unwrap_or(false)
            && entry.kind == TuiEntryKind::Thinking
        {
            continue;
        }
        let progress = app.transcript_scrollback.entries[index];
        let rows = pending_entry_rows(entry, index, area.width as usize, palette, &state, progress);
        height += rows.len();
        if !rows.is_empty() {
            chunks.push((index, rows));
        }
    }
    if let Some((_, rows)) = chunks.last_mut() {
        while rows.last().is_some_and(|row| row.line.width() == 0) {
            rows.pop();
            height = height.saturating_sub(1);
        }
    }
    let mut remaining = height.saturating_sub(area.height as usize);
    let mut overflow = Vec::new();
    for (index, rows) in chunks {
        let live = app.live_entry_index == Some(index)
            || app.live_thinking_index == Some(index)
            || app.live_tool_indices.contains(&index);
        // A live entry's spacer is not final. Do not consume it or any later
        // entry until this one finishes, preserving chronological scrollback.
        let available = rows.len().saturating_sub(usize::from(live));
        let count = remaining.min(available);
        if count == 0 {
            break;
        }
        let progress = &mut app.transcript_scrollback.entries[index];
        progress.position = rows[count - 1].end;
        // Never seal a live entry: future deltas must still be paintable.
        progress.complete = count == rows.len() && !live;
        overflow.extend(rows.into_iter().take(count).map(|row| row.line));
        remaining -= count;
        if live {
            break;
        }
    }
    overflow
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub(crate) struct TuiSurface<'a> {
    pub(crate) terminal: &'a mut TuiTerminal,
    pub(crate) viewport_height: &'a mut u16,
}

fn new_tui_terminal(height: u16, owned_screen: bool) -> Result<TuiTerminal> {
    let backend = CrosstermBackend::new(io::stdout());
    Ok(Terminal::with_options(
        backend,
        TerminalOptions {
            // Leave a valid two-row native scroll region above the live UI.
            // A full-screen inline viewport makes ratatui borrow a one-row
            // region, which tmux (and VT terminals) rejects and scrolls the
            // entire editor instead. The reserved rows hold transcript text.
            viewport: if owned_screen {
                Viewport::Fullscreen
            } else {
                Viewport::Inline(height.saturating_sub(2).max(1))
            },
        },
    )?)
}

fn terminal_viewport_height() -> u16 {
    crossterm::terminal::size()
        .map(|(_, height)| height.max(1))
        .unwrap_or(24)
}

fn resize_tui_viewport(
    terminal: &mut TuiTerminal,
    viewport_height: &mut u16,
    owned_screen: bool,
) -> Result<()> {
    let desired = terminal_viewport_height();
    if owned_screen || desired == *viewport_height {
        *viewport_height = desired;
        terminal.autoresize()?;
        return Ok(());
    }
    *terminal = new_tui_terminal(desired, owned_screen)?;
    terminal.clear()?;
    *viewport_height = desired;
    Ok(())
}

fn redraw_tui<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut TuiApp,
    config: &LoadedConfig,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal.autoresize()?;
    let area = tui_layout(terminal.get_frame().area(), app, config)[0];
    let palette = terminal_theme(config).unwrap_or_default().palette;
    let overflow = if app.owned_screen {
        Vec::new()
    } else {
        transcript_overflow(app, config, area, &palette)
    };
    // Bound temporary terminal buffers even when a resumed message is huge.
    for rows in overflow.chunks(128) {
        terminal.insert_before(rows.len() as u16, |buffer| {
            Paragraph::new(rows.to_vec())
                .style(palette.base())
                .render(buffer.area, buffer);
        })?;
    }
    terminal.draw(|frame| draw_tui(frame, app, config))?;
    Ok(())
}

#[cfg(test)]
fn rendered_lines_height(lines: &[Line<'_>], width: usize) -> u16 {
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

const TRANSCRIPT_RENDER_OVERSCAN_ROWS: u16 = 4;
const STREAM_RENDER_INTERVAL_MS: u64 = 16;

// Entry marker glyph. A terminal cell is a fixed size, so a glyph cannot be
// literally scaled; use a larger filled circle instead of the small `•`
// (U+2022). `●` (U+25CF) is single-width and reliably bigger. Swap to `⬤`
// (U+2B24) for a still-larger circle, but that one is wide/font-dependent and
// can misalign the two-space continuation indent.
const BULLET: &str = "●";

struct VisibleTranscript {
    lines: Vec<Line<'static>>,
    visual_height: u16,
    #[cfg(test)]
    entries_used: usize,
}

#[cfg(test)]
fn visible_transcript(entries: &[TuiEntry], width: usize, height: u16) -> VisibleTranscript {
    visible_transcript_with_thinking(entries, width, height, false, &ThemePalette::default())
}

#[cfg(test)]
fn visible_transcript_with_thinking(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
) -> VisibleTranscript {
    visible_transcript_with_previews(
        entries,
        width,
        height,
        hide_thinking,
        palette,
        PreviewState::default(),
    )
}

#[derive(Default)]
struct PreviewState {
    expanded_from: Option<usize>,
    live_thinking: Option<usize>,
}

fn recent_turn_start(entries: &[TuiEntry]) -> usize {
    entries
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry.kind == TuiEntryKind::User)
        .nth(2)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

#[cfg(test)]
fn preview_entry(
    entry: &TuiEntry,
    index: usize,
    width: usize,
    palette: &ThemePalette,
    state: &PreviewState,
) -> Vec<Line<'static>> {
    pending_entry_rows(
        entry,
        index,
        width,
        palette,
        state,
        EntryScrollback::default(),
    )
    .into_iter()
    .map(|row| row.line)
    .collect()
}

#[cfg(test)]
fn visible_transcript_with_previews(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
    state: PreviewState,
) -> VisibleTranscript {
    visible_pending_transcript(entries, width, height, hide_thinking, palette, state, &[])
}

fn visible_pending_transcript(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
    state: PreviewState,
    archived: &[EntryScrollback],
) -> VisibleTranscript {
    let width = width.max(1);
    let target_height = height.saturating_add(TRANSCRIPT_RENDER_OVERSCAN_ROWS) as usize;
    let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut scan_height: usize = 0;
    #[cfg(test)]
    let mut entries_used = 0;

    for (index, entry) in entries.iter().enumerate().rev() {
        if hide_thinking && entry.kind == TuiEntryKind::Thinking {
            continue;
        }
        let progress = archived.get(index).copied().unwrap_or_default();
        if progress.complete {
            continue;
        }
        let mut chunk = pending_entry_rows(entry, index, width, palette, &state, progress)
            .into_iter()
            .map(|row| row.line)
            .collect::<Vec<_>>();
        if chunks.is_empty() {
            while chunk.last().map(Line::width) == Some(0) {
                chunk.pop();
            }
        }
        if chunk.is_empty() {
            continue;
        }

        scan_height = scan_height.saturating_add(chunk.len());
        chunks.push(chunk);
        #[cfg(test)]
        {
            entries_used += 1;
        }
        if scan_height >= target_height {
            break;
        }
    }

    chunks.reverse();
    let mut lines = chunks.into_iter().flatten().collect::<Vec<_>>();
    if lines.len() > height as usize {
        lines = lines[lines.len() - height as usize..].to_vec();
    }
    let visual_height = lines.len().min(u16::MAX as usize) as u16;
    VisibleTranscript {
        lines,
        visual_height,
        #[cfg(test)]
        entries_used,
    }
}

fn push_transcript_segment(current: &mut Vec<Span<'static>>, segment: &mut String, style: Style) {
    if !segment.is_empty() {
        current.push(Span::styled(std::mem::take(segment), style));
    }
}

fn tui_layout(frame_area: Rect, app: &TuiApp, config: &LoadedConfig) -> [Rect; 7] {
    let slash_matches = slash_command_matches(config, app);
    let slash_match_height = (slash_matches.len().min(SLASH_MATCH_LIMIT) as u16)
        .min(frame_area.height.saturating_sub(4));
    let spacer_height = u16::from(app.queued_messages.is_empty());
    let input_height = input_area_height(app, frame_area, slash_match_height);
    let panel_budget = frame_area
        .height
        .saturating_sub(input_height + slash_match_height + spacer_height + 1);
    let queue_height = queue_panel_height(app).min(panel_budget);
    let todo_budget = panel_budget.saturating_sub(queue_height) / 2;
    let todo_height = if todo_budget >= 4 {
        todo_panel_height(app).min(todo_budget)
    } else {
        0
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(todo_height),
            Constraint::Length(queue_height),
            Constraint::Length(spacer_height),
            Constraint::Length(input_height),
            Constraint::Length(slash_match_height),
            Constraint::Length(1),
        ])
        .areas::<7>(frame_area)
}

fn transcript_area(area: Rect, app: &TuiApp) -> Rect {
    if app.diff_panel.is_some() {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(DIFF_PANEL_WIDTH)])
            .split(area)[0]
    } else {
        area
    }
}

fn draw_tui(frame: &mut Frame<'_>, app: &TuiApp, config: &LoadedConfig) {
    let palette = terminal_theme(config).unwrap_or_default().palette;
    frame.render_widget(Block::default().style(palette.base()), frame.area());
    let root = tui_layout(frame.area(), app, config);
    let slash_matches = slash_command_matches(config, app);
    let transcript_area = transcript_area(root[0], app);
    if app.diff_panel.is_some() {
        let diff_area = Rect::new(
            transcript_area.right(),
            root[0].y,
            root[0].right().saturating_sub(transcript_area.right()),
            root[0].height,
        );
        draw_diff_panel(frame, diff_area, app, &palette);
    }
    draw_transcript(frame, transcript_area, app, config, &palette);
    draw_todo_panel(frame, root[1], app, &palette);
    draw_queue_panel(frame, root[2], app, &palette);
    // Queue hints sit directly against the editor; otherwise leave a blank row.
    draw_input(frame, root[4], app, &palette);
    draw_slash_matches(frame, root[5], &slash_matches, &palette);
    draw_footer(frame, root[6], app, &palette);
    draw_selector_overlay(frame, app, &palette);
    set_tui_cursor(frame, root[4], app);
}

const DIFF_PANEL_WIDTH: u16 = 48;
const TODO_COLLAPSED_ROWS: usize = 5;
const TODO_EXPANDED_ROW_LIMIT: usize = 15;

fn todo_row_limit(expanded: bool) -> usize {
    if expanded {
        TODO_EXPANDED_ROW_LIMIT
    } else {
        TODO_COLLAPSED_ROWS
    }
}

fn selected_todos_with_limit(todos: &[TodoItem], expanded: bool, limit: usize) -> Vec<usize> {
    if expanded {
        return (0..todos.len().min(limit)).collect();
    }
    let mut selected = todos
        .iter()
        .enumerate()
        .filter(|(_, todo)| todo.status == TodoStatus::InProgress)
        .map(|(index, _)| index)
        .take(limit)
        .collect::<Vec<_>>();
    let recent_done = todos
        .iter()
        .rposition(|todo| todo.status == TodoStatus::Completed);
    let pending_slots = limit.saturating_sub(selected.len() + usize::from(recent_done.is_some()));
    selected.extend(
        todos
            .iter()
            .enumerate()
            .filter(|(_, todo)| todo.status == TodoStatus::Pending)
            .map(|(index, _)| index)
            .take(pending_slots),
    );
    if let Some(index) = recent_done {
        if selected.len() < limit {
            selected.push(index);
        }
    }
    for index in (0..todos.len()).rev() {
        if selected.len() >= limit {
            break;
        }
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    selected.sort_unstable();
    selected
}

fn todo_panel_lines(app: &TuiApp, palette: &ThemePalette) -> Vec<Line<'static>> {
    todo_panel_lines_with_limit(app, palette, todo_row_limit(app.todos_expanded))
}

fn todo_panel_lines_with_limit(
    app: &TuiApp,
    palette: &ThemePalette,
    limit: usize,
) -> Vec<Line<'static>> {
    if app.todos.is_empty() {
        return Vec::new();
    }
    let selected = selected_todos_with_limit(&app.todos, app.todos_expanded, limit);
    let mut lines = vec![Line::from(Span::styled(
        "  Todo",
        palette.secondary().add_modifier(Modifier::BOLD),
    ))];
    for index in &selected {
        let todo = &app.todos[*index];
        let (marker, marker_style, title_style) = match todo.status {
            TodoStatus::Completed => (
                "✓",
                Style::default().fg(palette.success),
                palette.secondary().add_modifier(Modifier::CROSSED_OUT),
            ),
            TodoStatus::InProgress => (
                "●",
                Style::default().fg(palette.accent),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Pending => ("○", palette.secondary(), Style::default()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {marker} "), marker_style),
            Span::styled(single_line(&todo.content), title_style),
        ]));
    }
    let hidden = app
        .todos
        .iter()
        .enumerate()
        .filter(|(index, _)| !selected.contains(index))
        .map(|(_, todo)| todo)
        .collect::<Vec<_>>();
    if !hidden.is_empty() {
        let mut counts = Vec::new();
        for (status, label) in [
            (TodoStatus::Pending, "pending"),
            (TodoStatus::InProgress, "active"),
            (TodoStatus::Completed, "done"),
        ] {
            let count = hidden.iter().filter(|todo| todo.status == status).count();
            if count > 0 {
                counts.push(format!("{count} {label}"));
            }
        }
        lines.push(Line::from(Span::styled(
            format!(
                "  … +{} more ({}) · ctrl+t to {}",
                hidden.len(),
                counts.join(", "),
                if app.todos_expanded {
                    "collapse"
                } else {
                    "expand"
                }
            ),
            palette.secondary(),
        )));
    } else if app.todos_expanded && !app.todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "  ctrl+t to collapse",
            palette.secondary(),
        )));
    }
    lines
}

fn todo_panel_borders(app: &TuiApp) -> Borders {
    if app.queued_messages.is_empty() {
        Borders::TOP | Borders::BOTTOM
    } else {
        Borders::TOP
    }
}

fn todo_panel_height(app: &TuiApp) -> u16 {
    if app.todos.is_empty() {
        0
    } else {
        todo_panel_lines(app, &ThemePalette::default()).len() as u16
            + 1
            + u16::from(app.queued_messages.is_empty())
    }
}

fn draw_todo_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    if area.height == 0 || app.todos.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(todo_panel_borders(app))
        .border_style(palette.secondary().fg(palette.border));
    let inner = block.inner(area);
    let capacity = (inner.height as usize).saturating_sub(1);
    let needs_hint =
        app.todos_expanded || app.todos.len() > capacity.min(todo_row_limit(app.todos_expanded));
    let limit = capacity
        .saturating_sub(usize::from(needs_hint))
        .min(todo_row_limit(app.todos_expanded));
    let lines = todo_panel_lines_with_limit(app, palette, limit)
        .into_iter()
        .map(|line| clip_line_width(line, inner.width as usize))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

const QUEUE_VISIBLE_ROWS: usize = 4;

fn queue_panel_height(app: &TuiApp) -> u16 {
    if app.queued_messages.is_empty() {
        0
    } else {
        (app.queued_messages.len().min(QUEUE_VISIBLE_ROWS) + 2) as u16
    }
}

fn draw_queue_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    if area.height == 0 || app.queued_messages.is_empty() {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        "─".repeat(area.width as usize),
        palette.secondary().fg(palette.border),
    ))];
    let visible_rows = QUEUE_VISIBLE_ROWS.min(area.height.saturating_sub(2) as usize);
    let overflow = app.queued_messages.len().saturating_sub(visible_rows);
    for message in app.queued_messages.iter().skip(overflow) {
        let text = single_line(message);
        lines.push(Line::from(vec![
            Span::styled("  ❯ ", Style::default().fg(palette.accent)),
            Span::styled(
                truncate_width(&text, area.width.saturating_sub(4) as usize),
                Style::default().fg(palette.foreground),
            ),
        ]));
    }
    let hint = if app.turn_running {
        if app.turn_saw_tools {
            "↑ to edit · ctrl+s to steer at next boundary"
        } else {
            // A prose-only response has no tool-call round boundary to inject
            // into: steering is delivered only after this response finishes.
            "↑ to edit · ctrl+s queues after this response"
        }
    } else {
        "↑ to edit · runs after next turn"
    };
    lines.push(Line::from(Span::styled(
        if overflow > 0 {
            format!("  … +{overflow} more · {hint}")
        } else {
            format!("  {hint}")
        },
        palette.secondary(),
    )));
    let lines = lines
        .into_iter()
        .map(|line| clip_line_width(line, area.width as usize))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn recall_queued_message(app: &mut TuiApp, message: String) {
    app.clear_input();
    app.input = message;
    app.input_cursor = app.input.len();
}

fn draw_diff_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let Some(panel) = app.diff_panel.as_ref() else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(palette.border));
    if let Some(detail) = panel.detail.as_ref() {
        let block = block.title(format!(" {} ", detail.path));
        let visible_height = area.height.saturating_sub(1) as usize;
        let lines = detail
            .lines
            .iter()
            .skip(detail.scroll)
            .take(visible_height)
            .map(|line| colorized_diff_line(line, palette))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let block = block.title(format!(" edited files ({}) ", panel.files.len()));
    let mut lines = Vec::new();
    for (index, entry) in panel.files.iter().enumerate() {
        let stats = match (entry.added, entry.removed) {
            (Some(added), Some(removed)) => format!(" +{added} -{removed}"),
            _ if entry.untracked => " new".to_string(),
            _ => String::new(),
        };
        let line = Line::from(format!("{}{stats}", entry.path));
        lines.push(if index == panel.selected {
            line.style(palette.selected())
        } else {
            line
        });
    }
    if panel.files.is_empty() {
        lines.push(Line::from("no files edited this session").style(palette.secondary()));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn colorized_diff_line(line: &str, palette: &ThemePalette) -> Line<'static> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        palette.secondary()
    } else if line.starts_with('+') {
        Style::default().fg(palette.success)
    } else if line.starts_with('-') {
        Style::default().fg(palette.error)
    } else if line.starts_with("@@") {
        Style::default().fg(palette.accent)
    } else {
        Style::default()
    };
    Line::from(line.to_string()).style(style)
}

const MAX_INPUT_ROWS: usize = 10;

fn input_area_height(app: &TuiApp, frame_area: Rect, slash_match_height: u16) -> u16 {
    let desired = input_desired_height(app, frame_area.width);
    let available = frame_area
        .height
        .saturating_sub(slash_match_height)
        .saturating_sub(1 + u16::from(app.queued_messages.is_empty()));
    desired.min(available)
}

fn input_desired_height(app: &TuiApp, width: u16) -> u16 {
    let inner = input_block(&ThemePalette::default()).inner(Rect::new(0, 0, width, u16::MAX));
    (input_layout(app, inner.width as usize)
        .rows
        .len()
        .min(MAX_INPUT_ROWS)
        + 2
        + usize::from(app.multiline.is_some())
        + usize::from(!app.input_media.is_empty())) as u16
}

fn draw_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    config: &LoadedConfig,
    palette: &ThemePalette,
) {
    if app.owned_screen {
        let mut view = app.transcript_view.borrow_mut();
        view.sync(
            &app.entries,
            area,
            palette,
            config.settings.hide_thinking_block.unwrap_or(false),
            &transcript_preview_state(app),
        );
        let lines = view.visible_lines();
        let body = view.area;
        let y = view.visible_top();
        frame.render_widget(
            Paragraph::new(lines).style(palette.base()),
            Rect {
                y,
                height: body.bottom().saturating_sub(y),
                ..body
            },
        );
        if !view.hint().is_empty() {
            frame.render_widget(
                Paragraph::new(view.hint()).style(palette.secondary()),
                view.hint_area,
            );
        }
        return;
    }
    if area.height == 0 || area.width == 0 {
        return;
    }
    let visible = visible_pending_transcript(
        &app.entries,
        area.width as usize,
        area.height,
        config.settings.hide_thinking_block.unwrap_or(false),
        palette,
        transcript_preview_state(app),
        &app.transcript_scrollback.entries,
    );
    if visible.lines.is_empty() {
        return;
    }
    let y = area.y + area.height.saturating_sub(visible.visual_height);
    let height = visible.visual_height.min(area.height);
    let render_area = Rect { y, height, ..area };
    let paragraph = Paragraph::new(visible.lines).style(Style::default().fg(palette.foreground));
    frame.render_widget(paragraph, render_area);
}

fn render_entry_lines(entries: &[TuiEntry], palette: &ThemePalette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry.kind {
            TuiEntryKind::Thinking if entry.text.is_empty() => continue,
            TuiEntryKind::Thinking => push_marked_lines(
                &mut lines,
                "thinking",
                &entry.text,
                palette.secondary(),
                palette.secondary().add_modifier(Modifier::ITALIC),
            ),
            TuiEntryKind::Tool => push_tool_lines(&mut lines, &entry.text, palette),
            TuiEntryKind::Error => push_marked_lines(
                &mut lines,
                "error",
                &entry.text,
                Style::default()
                    .fg(palette.error)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(palette.error),
            ),
            TuiEntryKind::System => push_marked_lines(
                &mut lines,
                "",
                &entry.text,
                palette.secondary(),
                palette.secondary(),
            ),
            TuiEntryKind::User | TuiEntryKind::Assistant => {
                push_marked_lines(
                    &mut lines,
                    "",
                    &entry.text,
                    palette.secondary(),
                    Style::default().fg(palette.foreground),
                );
            }
        }
        lines.push(Line::from(""));
    }
    lines
}

fn push_marked_lines(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    text: &str,
    marker_style: Style,
    text_style: Style,
) {
    let mut text_lines = text.lines();
    let first = text_lines.next().unwrap_or_default();
    let prefix = if label.is_empty() {
        format!("{BULLET} ")
    } else {
        format!("{BULLET} {label} ")
    };
    lines.push(Line::from(vec![
        Span::styled(prefix, marker_style),
        Span::styled(first.to_string(), text_style),
    ]));
    for line in text_lines {
        lines.push(Line::from(Span::styled(format!("  {line}"), text_style)));
    }
}

/// Restyle wrapped assistant rows with inline markdown emphasis. Runs after
/// wrapping so selection geometry (built from the plain logical lines) is
/// untouched: the same graphemes keep the same byte positions, only span
/// styles change. Links are the exception — their display text is the link
/// label only — so a link row's display differs from its source slice and
/// selection copies the raw markdown, like folded tool previews.
fn restyle_rows_with_markdown(
    rows: &mut [TranscriptRow],
    entry: &TuiEntry,
    logical_source_line: impl Fn(usize) -> Option<String>,
    base: Style,
    accent: ratatui::style::Color,
) {
    if entry.kind != TuiEntryKind::Assistant {
        return;
    }
    // Fence state walks rows in display order: emphasis is suppressed and
    // rows are dimmed between opening and closing fences. The fence marker
    // lines themselves collapse to a dim language label.
    // The marker of the code block a row is in, which alone closes it.
    let mut in_fence: Option<&'static str> = None;
    // Ordered-list state: an ordered item numbers itself from the list's
    // start marker plus its position (upstream's list renderer), so a
    // source that numbers every item `1.` still displays `1. 2. 3.`. A
    // plain line or a bullet ends the sequence.
    let mut ordered_list: Option<(u64, u64)> = None;
    // Inside an HTML comment that began on an earlier row: rows show
    // nothing until it closes (upstream tui 54280c4f).
    let mut in_comment = false;
    for row in rows.iter_mut() {
        if row.synthetic {
            continue;
        }
        let Some(source_line) = logical_source_line(row.start.line) else {
            continue;
        };
        // Only the first row of a logical line applies block transforms;
        // wrapped continuations render as plain content of that line.
        let starts_line = row.start.byte == 0;
        let end = if row.end.line == row.start.line {
            row.end.byte
        } else {
            source_line.len()
        };
        let Some(row_text) = source_line.get(row.start.byte..end) else {
            continue;
        };
        if let Some(marker) = in_fence {
            // Inside a code block: no emphasis, dim accent bar, indent.
            if source_line.trim_start().starts_with(marker) && starts_line {
                in_fence = None;
                row.line = Line::from(Span::styled(String::new(), base));
                continue;
            }
            row.line = Line::from(vec![
                Span::styled("  ┆ ", Style::default().fg(accent)),
                Span::styled(row_text.to_string(), base),
            ]);
            continue;
        }
        // HTML comments are not shown (upstream tui 54280c4f): what is
        // left of the row is plain text, and a row that was only a comment
        // is blank.
        if in_comment || row_text.contains("<!--") {
            let (visible, open) = without_html_comments(row_text, in_comment);
            in_comment = open;
            row.line = if visible.trim().is_empty() {
                Line::from(Span::styled(String::new(), base))
            } else {
                Line::from(styled_spans(&visible, base, accent))
            };
            continue;
        }
        // The first logical line carries the "[provider/model] " echo
        // prefix (and the bullet marker before that); classify on the
        // markdown content only.
        let classify_target: &str = if row.start.line == 0 {
            source_line
                .strip_prefix("● ")
                .and_then(|rest| rest.find("] ").map(|i| &rest[i + 2..]))
                .unwrap_or(&source_line)
        } else {
            &source_line
        };
        let mut kind = if starts_line {
            markdown::classify_line(classify_target)
        } else {
            markdown::BlockKind::Plain
        };
        if starts_line {
            match &mut kind {
                markdown::BlockKind::ListItem {
                    ordered: true,
                    marker,
                } => {
                    let (start, position) = match ordered_list {
                        Some(state) => (state.0, state.1 + 1),
                        None => (marker.unwrap_or(1), 1),
                    };
                    *marker = Some(start + position - 1);
                    ordered_list = Some((start, position));
                }
                markdown::BlockKind::ListItem {
                    ordered: false,
                    marker,
                } => {
                    *marker = None;
                    ordered_list = None;
                }
                _ => ordered_list = None,
            }
        } else if matches!(kind, markdown::BlockKind::ListItem { .. }) {
            ordered_list = None;
        }
        // Headings render their content after the markers; compute from the
        // classification target, not the raw source line.
        let heading_content = |kind: &markdown::BlockKind, target: &str| -> String {
            match kind {
                markdown::BlockKind::Heading(level) => {
                    let hashes = *level as usize + 1;
                    target
                        .get(hashes..)
                        .unwrap_or(target)
                        .trim_start()
                        .to_string()
                }
                _ => String::new(),
            }
        };
        match &kind {
            markdown::BlockKind::Fence { language, .. } => {
                in_fence = markdown::fence_marker(classify_target);
                let label = if language.is_empty() {
                    "  ┆ code".to_string()
                } else {
                    format!("  ┆ {language}")
                };
                row.line = Line::from(vec![Span::styled(label, Style::default().fg(accent))]);
            }
            markdown::BlockKind::Heading(level) => {
                let content = heading_content(&kind, classify_target);
                let style = base.add_modifier(Modifier::BOLD).fg(accent);
                row.line = Line::from(vec![Span::styled(
                    format!("{}{content}", " ".repeat(*level as usize)),
                    style,
                )]);
            }
            markdown::BlockKind::ListItem { .. } | markdown::BlockKind::Blockquote => {
                let render = markdown::block_render(&kind, classify_target);
                let content = classify_target
                    .get(render.content_byte..)
                    .unwrap_or(row_text);
                let marker_style = if matches!(kind, markdown::BlockKind::Blockquote) {
                    base.add_modifier(Modifier::DIM)
                } else {
                    base
                };
                let mut spans = vec![Span::styled(render.prefix.clone(), marker_style)];
                spans.extend(styled_spans(content.trim_start(), marker_style, accent));
                row.line = Line::from(spans);
            }
            markdown::BlockKind::Plain => {
                let spans = styled_spans(row_text, base, accent);
                row.line = Line::from(spans);
            }
        }
    }
}

/// `text` without its HTML comments, `<!-- ... -->`, starting inside one
/// when `in_comment`; and whether a comment is still open at its end.
fn without_html_comments(text: &str, in_comment: bool) -> (String, bool) {
    let mut visible = String::new();
    let mut rest = text;
    if in_comment {
        match rest.find("-->") {
            Some(end) => rest = &rest[end + 3..],
            None => return (visible, true),
        }
    }
    loop {
        let Some(start) = rest.find("<!--") else {
            visible.push_str(rest);
            return (visible, false);
        };
        visible.push_str(&rest[..start]);
        rest = &rest[start + 4..];
        match rest.find("-->") {
            Some(end) => rest = &rest[end + 3..],
            None => return (visible, true),
        }
    }
}

/// Convert one markdown line to styled spans over its plain base style.
fn styled_spans(line: &str, base: Style, accent: ratatui::style::Color) -> Vec<Span<'static>> {
    markdown::parse_inline_line(line)
        .into_iter()
        .map(|span| {
            let style = match span.style {
                markdown::SpanStyle::Plain => base,
                // base first: the markdown style's fg/modifiers must win over
                // the plain base (base.fg is Some(Reset) and would otherwise
                // mask the accent color via patch).
                other => base.patch(markdown::ratatui_style(other, accent)),
            };
            Span::styled(span.display, style)
        })
        .collect()
}

fn push_tool_lines(lines: &mut Vec<Line<'static>>, text: &str, palette: &ThemePalette) {
    if !text.starts_with("running ")
        && !text.starts_with("completed ")
        && !text.starts_with("failed ")
    {
        push_marked_lines(
            lines,
            "tool",
            text,
            palette.secondary(),
            palette.secondary(),
        );
        return;
    }
    let mut parts = text.splitn(3, '\n');
    let state = parts.next().unwrap_or_default();
    let detail = parts.next().unwrap_or_default();
    let output = parts.next().unwrap_or_default();
    let (phase, name) = state.split_once(' ').unwrap_or((state, "tool"));
    let name = name.trim_start_matches('/');
    let failed = phase == "failed";
    let running = phase == "running";
    let highlight = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(
        if failed { "✗ " } else { "● " },
        if failed {
            Style::default().fg(palette.error)
        } else if running {
            palette.secondary()
        } else {
            Style::default().fg(palette.success)
        },
    )];
    if name == "bash" {
        spans.push(Span::styled(
            if failed {
                "Command failed"
            } else if running {
                "Running a command"
            } else {
                "Ran a command"
            },
            highlight,
        ));
    } else {
        spans.push(Span::raw(if failed {
            "Failed "
        } else if running {
            "Using "
        } else {
            "Used "
        }));
        let label = match name {
            "read" => "Read",
            "edit" => "Edit",
            "write" => "Write",
            "grep" => "Grep",
            "find" => "Find",
            "ls" => "Ls",
            "todo" => "Todo",
            "web_search" => "Web Search",
            "web_fetch" => "Web Fetch",
            _ => name,
        };
        spans.push(Span::styled(label.to_string(), highlight));
    }
    if !detail.is_empty() {
        spans.push(Span::styled(format!(" · {detail}"), palette.secondary()));
    }
    if name == "read" && !running && !output.is_empty() {
        spans.push(Span::styled(
            format!(" · {} lines", output.lines().count()),
            palette.secondary(),
        ));
    }
    lines.push(Line::from(spans));
    for line in output.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            palette.secondary(),
        )));
    }
    if output.is_empty() && !running {
        lines.push(Line::from(Span::styled(
            "  (no output)",
            palette.secondary(),
        )));
    }
}

fn input_block(palette: &ThemePalette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(palette.secondary().fg(palette.border))
        .padding(Padding::horizontal(1))
        .style(Style::default().fg(palette.foreground).bg(palette.surface))
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let block = input_block(palette);
    let inner = block.inner(area);
    let input_style = Style::default().fg(palette.foreground).bg(palette.surface);
    let prompt_style = Style::default().fg(palette.accent).bg(palette.surface);
    let lines = render_input_lines(app, inner, input_style, prompt_style);
    frame.render_widget(Paragraph::new(lines).style(input_style).block(block), area);
}

fn input_prompt(app: &TuiApp) -> &'static str {
    if app.multiline.is_some() {
        "multi> "
    } else {
        "> "
    }
}

fn render_input_lines(
    app: &TuiApp,
    area: Rect,
    input_style: Style,
    prompt_style: Style,
) -> Vec<Line<'static>> {
    if area.height == 0 || area.width == 0 {
        return Vec::new();
    }
    let prompt = input_prompt(app);
    let layout = input_layout(app, area.width as usize);
    let visible_rows = input_visible_rows(app, area.height as usize);
    let start = layout.visible_start(visible_rows);
    let mut lines = layout
        .rows
        .into_iter()
        .skip(start)
        .take(visible_rows)
        .enumerate()
        .map(|(offset, text)| {
            let prefix = if offset == 0 {
                prompt.to_string()
            } else {
                " ".repeat(prompt.len())
            };
            Line::from(vec![
                Span::styled(clip_width(&prefix, area.width as usize), prompt_style),
                Span::styled(text, input_style),
            ])
        })
        .collect::<Vec<_>>();
    if !app.input_media.is_empty() && lines.len() < area.height as usize {
        lines.push(Line::from(Span::styled(
            clip_width(
                &format!(
                    "[{} image{} attached · esc clears draft when idle]",
                    app.input_media.len(),
                    if app.input_media.len() == 1 { "" } else { "s" }
                ),
                area.width as usize,
            ),
            prompt_style,
        )));
    }
    if app.multiline.is_some() && lines.len() < area.height as usize {
        lines.push(Line::from(Span::styled(
            clip_width("submit with . on its own line", area.width as usize),
            prompt_style,
        )));
    }
    lines
}

struct InputLayout {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

impl InputLayout {
    fn visible_start(&self, capacity: usize) -> usize {
        self.rows
            .len()
            .saturating_sub(capacity)
            .min(self.cursor_row)
    }
}

/// The editor's height, rendering, and cursor use the same grapheme/cell wrap.
/// Only the display copy folds pastes; the underlying draft stays untouched.
fn input_layout(app: &TuiApp, inner_width: usize) -> InputLayout {
    let (display, cursor) = app.display_input();
    let width = inner_width.saturating_sub(input_prompt(app).len()).max(1);
    let mut layout = InputLayout {
        rows: Vec::new(),
        cursor_row: 0,
        cursor_column: 0,
    };
    let mut source_offset = 0;
    for part in display.split('\n') {
        let mut row = String::new();
        let mut column = 0;
        for (offset, grapheme) in part.grapheme_indices(true) {
            let text = if grapheme == "\t" { "    " } else { grapheme };
            let cells = text.width();
            if column > 0 && column + cells > width {
                layout.rows.push(std::mem::take(&mut row));
                column = 0;
            }
            if source_offset + offset == cursor {
                layout.cursor_row = layout.rows.len();
                layout.cursor_column = column;
            }
            row.push_str(text);
            column += cells;
        }
        // A full final row needs an insertion cell on the next row.
        if column >= width
            && (source_offset + part.len() == cursor || source_offset + part.len() == display.len())
        {
            layout.rows.push(std::mem::take(&mut row));
            column = 0;
        }
        if source_offset + part.len() == cursor {
            layout.cursor_row = layout.rows.len();
            layout.cursor_column = column;
        }
        layout.rows.push(row);
        source_offset += part.len() + 1;
    }
    layout
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip_width(text: &str, width: usize) -> String {
    let mut used = 0;
    text.graphemes(true)
        .take_while(|grapheme| {
            used += grapheme.width();
            used <= width
        })
        .collect()
}

fn truncate_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", clip_width(text, width - 1))
}

fn clip_line_width(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut remaining = width.saturating_sub(1);
    let mut spans = Vec::new();
    for span in line.spans {
        let clipped = span.width() > remaining;
        let content = clip_width(&span.content, remaining);
        remaining = remaining.saturating_sub(content.width());
        spans.push(Span::styled(content, span.style));
        if clipped {
            break;
        }
    }
    if width > 0 {
        spans.push(Span::styled("…", line.style));
    }
    Line::from(spans).style(line.style)
}

fn input_visible_rows(app: &TuiApp, area_height: usize) -> usize {
    if area_height == 0 {
        0
    } else {
        area_height
            .saturating_sub(
                usize::from(app.multiline.is_some()) + usize::from(!app.input_media.is_empty()),
            )
            .max(1)
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let status = if app.owned_screen && app.transcript_view.borrow().has_selection() {
        app.status
            .replace("esc to interrupt", "ctrl+c copies selection; esc clears")
    } else if app.owned_screen && app.transcript_view.borrow().is_reading() {
        app.status
            .replace("esc to interrupt", "ctrl+c to interrupt")
    } else {
        app.status.clone()
    };
    let line = match &app.workspace {
        Some(record) => format!(
            "{}  {}  {}",
            status,
            workspace::footer_label(record),
            app.header_line
        ),
        None if app.header_line.is_empty() => status,
        None => format!("{}  {}", status, app.header_line),
    };
    let footer = Paragraph::new(line).style(palette.secondary());
    frame.render_widget(footer, area);
}

const SLASH_MATCH_LIMIT: usize = 6;
const SELECTOR_PAGE_STEP: usize = 10;

fn slash_command_matches(config: &LoadedConfig, app: &TuiApp) -> Vec<String> {
    if app.selector.is_some() || app.multiline.is_some() || app.input.contains('\n') {
        return Vec::new();
    }
    let input = app.input.trim();
    if !input.starts_with('/') {
        return Vec::new();
    }
    command_completions(config, input)
        .into_iter()
        .take(SLASH_MATCH_LIMIT)
        .collect()
}

fn draw_slash_matches(
    frame: &mut Frame<'_>,
    area: Rect,
    matches: &[String],
    palette: &ThemePalette,
) {
    if area.height == 0 || matches.is_empty() {
        return;
    }
    let lines = matches
        .iter()
        .take(area.height as usize)
        .map(|command| {
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(command.clone(), palette.secondary()),
            ])
        })
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines).style(palette.secondary());
    frame.render_widget(paragraph, area);
}

fn draw_selector_overlay(frame: &mut Frame<'_>, app: &TuiApp, palette: &ThemePalette) {
    let Some(selector) = &app.selector else {
        return;
    };
    let area = centered_rect(frame.area(), 82, 68);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(palette.base().bg(palette.surface)),
        area,
    );
    let available = area.height.saturating_sub(5) as usize;
    let start = selector
        .selected
        .saturating_sub(available.saturating_sub(1));
    let mut lines = vec![
        Line::from(vec![
            Span::styled("filter ", palette.secondary()),
            Span::raw(selector.query.as_str()),
        ]),
        Line::from(""),
    ];
    for (position, item_index) in selector
        .filtered_indices
        .iter()
        .enumerate()
        .skip(start)
        .take(available)
    {
        let item = &selector.items[*item_index];
        let marker = if position == selector.selected {
            ">"
        } else {
            " "
        };
        let active = if item.active { "*" } else { " " };
        let style = if position == selector.selected {
            palette.selected()
        } else if item.active {
            Style::default().fg(palette.accent)
        } else {
            Style::default()
        };
        let prefix = format!("{:>2}. {marker}{active} ", position + 1);
        let text_width = area
            .width
            .saturating_sub(4)
            .saturating_sub(prefix.chars().count() as u16) as usize;
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", truncate_chars(&item.label, text_width)),
            style,
        )));
    }
    if selector.filtered_indices.is_empty() {
        lines.push(Line::from(Span::styled("no matches", palette.secondary())));
    }
    if let Some(level) = selector.selected_thinking_level() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("thinking ", palette.secondary()),
            Span::styled(level, Style::default().fg(palette.accent)),
            Span::styled("  left/right to adjust", palette.secondary()),
        ]));
    }
    let action = if selector.kind == "settings" {
        "enter toggle"
    } else {
        "enter select"
    };
    let total = selector.filtered_indices.len();
    let position = if total == 0 { 0 } else { selector.selected + 1 };
    let title = format!(
        " {} selector  {position}/{total}  {action}  pgup/pgdn home/end  esc cancel ",
        selector.title
    );
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.border))
            .title(title),
    );
    frame.render_widget(paragraph, area);
}

fn set_tui_cursor(frame: &mut Frame<'_>, input_area: Rect, app: &TuiApp) {
    if !app.show_hardware_cursor {
        return;
    }
    if let Some(selector) = &app.selector {
        let area = centered_rect(frame.area(), 82, 68);
        let filter_prefix_width = 7;
        let max_x = area.right().saturating_sub(2);
        let x = area
            .x
            .saturating_add(1)
            .saturating_add(filter_prefix_width)
            .saturating_add(selector.query.chars().count() as u16)
            .min(max_x);
        let y = area
            .y
            .saturating_add(1)
            .min(area.bottom().saturating_sub(2));
        frame.set_cursor_position(Position::new(x, y));
        return;
    }
    let inner = input_block(&ThemePalette::default()).inner(input_area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let prompt_width = input_prompt(app).len() as u16;
    let layout = input_layout(app, inner.width as usize);
    let start = layout.visible_start(input_visible_rows(app, inner.height as usize));
    let x = inner
        .x
        .saturating_add(prompt_width)
        .saturating_add(layout.cursor_column.min(u16::MAX as usize) as u16)
        .min(inner.right().saturating_sub(1));
    let y = inner
        .y
        .saturating_add(
            layout
                .cursor_row
                .saturating_sub(start)
                .min(u16::MAX as usize) as u16,
        )
        .min(inner.bottom().saturating_sub(1));
    frame.set_cursor_position(Position::new(x, y));
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() && max_chars > 0 {
        output.pop();
        output.push('~');
    }
    output
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn handle_transcript_key(key: &KeyEvent, app: &mut TuiApp) -> bool {
    if !app.owned_screen {
        return false;
    }
    let view = app.transcript_view.get_mut();
    match key.code {
        KeyCode::Char('c')
            if key.modifiers.contains(KeyModifiers::CONTROL) && view.has_selection() =>
        {
            view.request_copy()
        }
        KeyCode::Esc if view.is_reading() => {
            view.clear_selection();
            view.jump_to_latest();
        }
        KeyCode::Esc if view.clear_selection() => {}
        KeyCode::PageUp => view.page(false),
        KeyCode::PageDown => view.page(true),
        _ => return false,
    }
    true
}

fn handle_transcript_mouse(mouse: MouseEvent, app: &mut TuiApp) -> bool {
    if !app.owned_screen || app.selector.is_some() {
        return false;
    }
    let view = app.transcript_view.get_mut();
    let position = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp if view.area.contains(position) => view.scroll(-3),
        MouseEventKind::ScrollDown if view.area.contains(position) => view.scroll(3),
        MouseEventKind::Down(MouseButton::Left)
            if view.is_reading() && view.hint_area.contains(position) =>
        {
            view.clear_selection();
            view.jump_to_latest()
        }
        MouseEventKind::Down(MouseButton::Left) if view.area.contains(position) => {
            return view.begin_selection(position);
        }
        MouseEventKind::Drag(MouseButton::Left) => return view.drag_selection(position),
        MouseEventKind::Up(MouseButton::Left) => {
            if view.area.contains(position) {
                view.drag_selection(position);
            }
            view.finish_selection();
        }
        MouseEventKind::Down(_) => return view.clear_selection(),
        _ => return false,
    }
    true
}

/// Opens the external editor seeded with the current draft and replaces the
/// input with the edited content (ctrl+g).
fn edit_input_in_external_editor(app: &mut TuiApp) {
    match read_external_editor_prompt(&app.input) {
        Ok(content) => {
            let media = std::mem::take(&mut app.input_media);
            app.clear_input();
            app.input_media = media;
            app.insert_input_str(content.trim_end_matches('\n'));
        }
        Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
    }
}

const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

fn key_event_name(key: &KeyEvent) -> Option<String> {
    let base = match key.code {
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "escape".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        _ => return None,
    };
    let mut name = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        name.push_str("ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        name.push_str("super+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        name.push_str("alt+");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        name.push_str("shift+");
    }
    name.push_str(&base);
    Some(name)
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
    let Some(panel) = app.diff_panel.as_mut() else {
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
        KeyCode::Enter if key.modifiers.is_empty() && app.input.trim().is_empty() => {
            let detail = panel.files.get(panel.selected).map(&detail);
            if let Some(detail) = detail {
                panel.detail = Some(detail);
            }
        }
        KeyCode::Esc if app.owned_screen || app.input.is_empty() => app.diff_panel = None,
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

async fn run_prompt_media(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<String> {
    let provider = provider_for_runtime(runtime, config, offline).await?;
    let response = if media.is_empty() {
        run_user_turn(runtime, provider.as_ref(), prompt).await
    } else {
        run_user_turn_streaming_with_media(
            runtime,
            provider.as_ref(),
            prompt,
            media,
            &SteeringMailbox::default(),
            |_| {},
        )
        .await
    };
    // An account switch is reported whether or not the turn succeeded.
    for notice in account_failover::drain_notices() {
        eprintln!("noel: {notice}");
    }
    response.map_err(Into::into)
}

fn auto_compaction_enabled(config: &LoadedConfig) -> bool {
    config
        .settings
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.enabled)
        .unwrap_or(true)
}

async fn provider_for_runtime(
    runtime: &Runtime,
    config: &LoadedConfig,
    offline: bool,
) -> Result<Box<dyn noel_ai::Provider>> {
    let model = runtime
        .session()
        .active_model
        .clone()
        .ok_or_else(|| anyhow!("no active model; configure auth or use --model faux/echo"))?;
    if offline && model.provider != "faux" {
        return Err(anyhow!(
            "offline mode only allows local faux models; active model is {}/{}",
            model.provider,
            model.id
        ));
    }
    let definition = model_definition(config, &model).ok_or_else(|| {
        anyhow!(
            "active model is not in models config: {}/{}",
            model.provider,
            model.id
        )
    })?;
    // The level in effect, clamped to the model; a model without reasoning
    // is sent no thinking parameter.
    let thinking_level = definition
        .reasoning()
        .then(|| effective_thinking_level(runtime, config).to_string());
    let provider = FreshAuthProvider {
        config: ProviderConfig {
            thinking_budget_tokens: thinking_budget_tokens(config, thinking_level.as_deref()),
            thinking_level,
            model: definition.model_ref(),
            api: AiProviderApi::from(&definition.api),
            base_url: definition.base_url.clone(),
            auth: ProviderAuth::None,
            session_id: Some(runtime.session().session_id.clone()),
            traits: definition.traits(),
        },
        // The client `noel_ai::create_provider` uses: its idle timeout ends a
        // stalled request instead of hanging the turn.
        client: noel_ai::provider_http_client(),
        refresh_client: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        auth_path: config.paths.auth_path.clone(),
        auth: config.auth.clone(),
        account: runtime.session().active_account.clone(),
        failover: failover_for(runtime, config, &model.provider),
    };
    // Fail before the turn starts when there is no usable credential.
    if provider.failover_pool().await?.is_none() {
        provider.auth().await?;
    }
    Ok(Box::new(provider))
}

/// Whether failover rotation is active for a provider: globally enabled,
/// provider allowed (empty map means all), and more than one stored account.
/// An explicitly pinned session account always disables rotation for that
/// session.
fn failover_enabled_for(config: &LoadedConfig, provider: &str) -> bool {
    let Some(settings) = config.settings.failover.as_ref() else {
        return false;
    };
    if !settings.enabled.unwrap_or(false) {
        return false;
    }
    if !settings.providers.is_empty() && !settings.providers.contains_key(provider) {
        return false;
    }
    config.auth.accounts_for_provider(provider).len() > 1
}

/// What a turn's requests need to be served by the provider's account pool
/// instead of one account.
struct FailoverPoolSettings {
    min_switch_interval_ms: u64,
    exhaustion_threshold_pct: f64,
    usage_cache_path: PathBuf,
    state_path: PathBuf,
}

/// The pool settings of a turn when failover is enabled for the provider and
/// no explicit account is pinned for the session. `None` when the
/// single-account path should be used.
fn failover_for(
    runtime: &Runtime,
    config: &LoadedConfig,
    provider: &str,
) -> Option<FailoverPoolSettings> {
    let settings = config.settings.failover.as_ref();
    if !failover_enabled_for(config, provider) || runtime.session().active_account.is_some() {
        return None;
    }
    let provider_settings = settings
        .and_then(|settings| settings.providers.get(provider))
        .cloned()
        .unwrap_or_default();
    Some(FailoverPoolSettings {
        min_switch_interval_ms: provider_settings
            .min_switch_interval_ms
            .unwrap_or(account_failover::MIN_SWITCH_INTERVAL_MS),
        exhaustion_threshold_pct: provider_settings
            .exhaustion_threshold_pct
            .map_or(account_failover::EXHAUSTION_THRESHOLD_PCT, |pct| pct as f64),
        usage_cache_path: config.paths.agent_dir.join("usage-cache.json"),
        state_path: config.paths.agent_dir.join(account_failover::STATE_FILE),
    })
}

/// A provider that resolves its credentials for every request, so a turn
/// that outlives an OAuth access token refreshes it instead of sending the
/// token the turn started with.
///
/// With failover on, every request gets its own account pool, built from
/// the credentials valid then. The pool does not outlive the request: the
/// sticky account and the cooldowns are `account_failover`'s process-wide
/// memory, which each pool starts from and reports to.
struct FreshAuthProvider {
    /// The config of every request; `auth` is resolved per request.
    config: ProviderConfig,
    client: reqwest::Client,
    refresh_client: reqwest::Client,
    auth_path: PathBuf,
    auth: AuthData,
    account: Option<String>,
    /// Set when the turn's requests fail over between the stored accounts.
    failover: Option<FailoverPoolSettings>,
}

impl FreshAuthProvider {
    async fn auth(&self) -> Result<ProviderAuth> {
        let resolved = oauth_refresh::refresh_expiring_auth(
            &self.refresh_client,
            &self.auth_path,
            &self.auth,
            &self.config.model.provider,
            self.account.as_deref(),
            None,
            unix_seconds().unwrap_or(0),
        )
        .await?;
        Ok(map_provider_auth(resolved))
    }

    /// Build a failover pool for one request when the turn fails over and
    /// more than one stored account has a credential. Returns `None` when
    /// the single-account path should be used.
    ///
    /// An account whose credential cannot be refreshed is left out, once
    /// with a notice, and the others serve; only when none is left does the
    /// request fail, with the first account's error.
    async fn failover_pool(&self) -> Result<Option<Box<dyn noel_ai::Provider>>> {
        let Some(failover) = &self.failover else {
            return Ok(None);
        };
        let provider = self.config.model.provider.as_str();
        let now = unix_seconds().unwrap_or(0);
        let usage_cache = noel_config::read_usage_cache(&failover.usage_cache_path)
            .ok()
            .flatten();
        let candidates = account_failover::failover_candidates(
            &self.auth,
            usage_cache.as_ref(),
            provider,
            failover.exhaustion_threshold_pct,
            now as i64,
        );
        if candidates.len() < 2 {
            return Ok(None);
        }
        let mut pool = account_failover::FailoverProviderBuilder::new(provider)
            .min_switch_interval_ms(failover.min_switch_interval_ms)
            .saved_in(failover.state_path.clone());
        let saved_sticky = account_failover::saved_state(&failover.state_path, provider).current;
        let (mut accounts, mut first_error, mut sticky) = (0, None, None);
        for candidate in candidates {
            let refreshed = oauth_refresh::refresh_expiring_auth(
                &self.refresh_client,
                &self.auth_path,
                &self.auth,
                provider,
                Some(&candidate.account),
                None,
                now,
            )
            .await;
            let auth = match refreshed {
                Ok(Some(auth)) => auth,
                Ok(None) => continue,
                Err(error) => {
                    account_failover::report_left_out(
                        provider,
                        &candidate.account,
                        &format!("{error:#}"),
                    );
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            accounts += 1;
            if sticky.is_none() || saved_sticky.as_ref() == Some(&candidate.account) {
                sticky = Some((candidate.account.clone(), auth.clone()));
            }
            pool = pool
                .account(candidate.account.clone(), map_provider_auth(Some(auth)))
                .exhausted_until(candidate.account, candidate.exhausted_until);
        }
        if accounts == 0 {
            return first_error.map_or(Ok(None), Err);
        }
        // The 5h window only helps while the usage cache is fresh, so keep
        // the sticky account's fresh; the others are read as last cached.
        if let Some((account, auth)) = sticky {
            usage::refresh_stale_in_background(
                failover.usage_cache_path.clone(),
                provider,
                &account,
                auth,
                self.config.base_url.clone(),
                usage_cache
                    .as_ref()
                    .and_then(|cache| cache.accounts.get(&format!("{provider}:{account}"))),
                now as i64,
            );
        }
        let pool = pool.build(|auth| {
            create_provider_with_client(
                ProviderConfig {
                    auth,
                    ..self.config.clone()
                },
                self.client.clone(),
            )
        });
        Ok(Some(Box::new(pool)))
    }

    /// The provider for one request, with the credentials valid now: the
    /// account pool when the turn fails over, else the session's account.
    async fn provider(&self) -> std::result::Result<Box<dyn noel_ai::Provider>, ProviderError> {
        let config_error = |error: anyhow::Error| ProviderError::Config(error.to_string());
        if let Some(pool) = self.failover_pool().await.map_err(config_error)? {
            return Ok(pool);
        }
        let auth = self.auth().await.map_err(config_error)?;
        Ok(create_provider_with_client(
            ProviderConfig {
                auth,
                ..self.config.clone()
            },
            self.client.clone(),
        ))
    }
}

#[async_trait::async_trait]
impl noel_ai::Provider for FreshAuthProvider {
    async fn complete(
        &self,
        request: ProviderRequest,
    ) -> std::result::Result<Vec<StreamEvent>, ProviderError> {
        self.provider().await?.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> std::result::Result<(), ProviderError> + Send),
    ) -> std::result::Result<(), ProviderError> {
        self.provider()
            .await?
            .complete_streaming(request, on_event)
            .await
    }

    fn thinking_level(&self) -> Option<String> {
        self.config.thinking_level.clone()
    }

    fn output_limit(&self) -> Option<u64> {
        noel_ai::model_output_limit(&self.config)
    }
}

fn thinking_budget_tokens(config: &LoadedConfig, level: Option<&str>) -> Option<u64> {
    let budgets = config.settings.thinking_budgets.as_ref()?;
    match level {
        Some("minimal") => budgets.minimal,
        Some("low") => budgets.low,
        Some("medium") => budgets.medium,
        Some("high") | Some("xhigh") | Some("max") => budgets.high,
        _ => None,
    }
}

fn format_model_selection(model: &ModelRef, thinking: Option<&str>) -> String {
    match thinking {
        Some(level) => format!("{}/{} {level}", model.provider, model.id),
        None => format!("{}/{}", model.provider, model.id),
    }
}

fn map_provider_auth(auth: Option<ResolvedAuth>) -> ProviderAuth {
    match auth {
        Some(ResolvedAuth::ApiKey(api_key)) => ProviderAuth::ApiKey(api_key),
        Some(ResolvedAuth::ClaudeCodeOAuth { access_token, .. }) => {
            ProviderAuth::ClaudeCodeOAuth { access_token }
        }
        Some(ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
            ..
        }) => ProviderAuth::ChatGptOAuth {
            access_token,
            account_id,
        },
        None => ProviderAuth::None,
    }
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

/// One-line session row for the resume picker, in the Kimi/Claude/Codex
/// style: topic preview first, then relative age, message count, model
/// badge, and an unfinished-turn marker so a crashed session stands out.
fn format_session_picker_label(session: &noel_core::SessionSummary) -> String {
    let topic = session
        .first_prompt
        .as_deref()
        .map(|prompt| {
            let line = prompt.lines().next().unwrap_or("").to_string();
            let truncated: String = line.chars().take(60).collect();
            if truncated.chars().count() < line.chars().count() {
                format!("{truncated}…")
            } else {
                truncated
            }
        })
        .filter(|topic| !topic.trim().is_empty())
        .unwrap_or_else(|| {
            session
                .name
                .clone()
                .unwrap_or_else(|| session.session_id.clone())
        });
    let age = session
        .modified
        .map(relative_time_label)
        .unwrap_or_else(|| "-".to_string());
    let model = session.active_model.clone().unwrap_or_else(|| "-".into());
    let unfinished = if session.has_unfinished_turn {
        " · unfinished"
    } else {
        ""
    };
    format!(
        "{topic} · {age} · {} msg · {model}{unfinished}",
        session.message_count
    )
}

/// Compact relative age like Claude's picker: "2m", "3h", "5d".
fn relative_time_label(modified: std::time::SystemTime) -> String {
    let Ok(elapsed) = modified.elapsed() else {
        return "now".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn selector_for_kind(config: &LoadedConfig, runtime: &Runtime, kind: &str) -> Result<Selector> {
    match kind {
        "model" | "models" | "scoped-models" => Ok(Selector::new(
            "model",
            picker_models(config)
                .into_iter()
                .map(|model| {
                    let value = format!("{}/{}", model.provider, model.id);
                    SelectorItem {
                        label: value.clone(),
                        value,
                        active: runtime
                            .session()
                            .active_model
                            .as_ref()
                            .map(|active| {
                                active.provider == model.provider && active.id == model.id
                            })
                            .unwrap_or(false),
                    }
                })
                .collect(),
        )),
        // Codex-style first layer: pick the provider, then its models.
        "provider" => Ok(Selector::new(
            "provider",
            provider_selector_items(
                &picker_models(config),
                runtime
                    .session()
                    .active_model
                    .as_ref()
                    .map(|model| model.provider.as_str()),
            ),
        )),
        "theme" | "themes" => Ok(Selector::new(
            "theme",
            theme_names(config)
                .into_iter()
                .map(|name| SelectorItem {
                    active: config.settings.theme.as_deref().unwrap_or("system") == name
                        || (name == "system"
                            && config.settings.theme.as_deref() == Some("default")),
                    label: name.clone(),
                    value: name,
                })
                .collect(),
        )),
        // The branch navigator: /tree's rows as pickable items; Enter
        // switches to the picked branch (upstream's tree selector).
        "branches" => Ok(Selector::new(
            "branches",
            session_tree_items(config, Some(&runtime.session().session_id))?,
        )),
        "session" | "sessions" | "resume" | "tree" => Ok(Selector::new(
            "session",
            SessionStore::list(&config.paths.session_dir)?
                .into_iter()
                // Journal listing is oldest-first; the picker should open on
                // the most recent session like Kimi/Claude/Codex.
                .rev()
                .map(|session| SelectorItem {
                    label: format_session_picker_label(&session),
                    value: session.session_id.clone(),
                    active: runtime.session().session_id == session.session_id,
                })
                .collect(),
        )),
        "auth" | "login" | "logout" => Ok(Selector::new(
            if kind == "logout" { "logout" } else { "auth" },
            config
                .models
                .iter()
                .map(|model| model.provider.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|provider| SelectorItem {
                    label: match (
                        kind == "logout",
                        auth_for_provider(&config.auth, &provider, None).is_some(),
                    ) {
                        // /logout removes what is there; /login runs the
                        // provider's flow, so the row says which it has.
                        (false, true) => format!("{provider}: available (login replaces)"),
                        (false, false) => format!("{provider}: missing (login)"),
                        (true, true) => format!("{provider}: available"),
                        (true, false) => format!("{provider}: missing"),
                    },
                    value: provider.clone(),
                    active: auth_for_provider(&config.auth, &provider, None).is_some(),
                })
                .collect(),
        )),
        "account" => Ok(Selector::new(
            "account",
            account_selector_items(config, runtime),
        )),
        "settings" => Ok(Selector::new("settings", settings_selector_items(config))),
        _ => Err(anyhow!("unknown selector: {kind}")),
    }
}

fn sorted_models(models: &[ModelDefinition]) -> Vec<&ModelDefinition> {
    let mut models = models.iter().collect::<Vec<_>>();
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

/// The models a model picker lists, by provider and id: the scope's when
/// `--models` or `enabledModels` sets one, else every known model.
fn picker_models(config: &LoadedConfig) -> Vec<&ModelDefinition> {
    if config.model_scope.is_empty() {
        return sorted_models(&config.models);
    }
    let mut models = config
        .model_scope
        .iter()
        .filter_map(|scoped| configured_model(config, &scoped.model))
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

/// Provider rows for the first layer of the model picker: provider name,
/// model count, and whether the active model belongs to it.
fn provider_selector_items(
    models: &[&ModelDefinition],
    active_provider: Option<&str>,
) -> Vec<SelectorItem> {
    let mut providers: BTreeMap<String, usize> = BTreeMap::new();
    for model in models {
        *providers.entry(model.provider.clone()).or_default() += 1;
    }
    providers
        .into_iter()
        .map(|(provider, count)| {
            let active_provider = active_provider == Some(provider.as_str());
            SelectorItem {
                label: format!(
                    "{provider} · {count} model{}{}",
                    if count == 1 { "" } else { "s" },
                    if active_provider { " · active" } else { "" }
                ),
                value: provider,
                active: active_provider,
            }
        })
        .collect()
}

fn account_selector_items(config: &LoadedConfig, runtime: &Runtime) -> Vec<SelectorItem> {
    let Some(model) = runtime.session().active_model.clone() else {
        return Vec::new();
    };
    account_selector_items_for(
        config,
        &model.provider,
        runtime.session().active_account.as_deref(),
    )
}

/// [`account_selector_items`] without the runtime, for the picker opened
/// mid-turn while a streaming turn borrows it: the provider of the active
/// model and the session's pinned account come from the app's chrome.
fn account_selector_items_for(
    config: &LoadedConfig,
    provider: &str,
    active_account: Option<&str>,
) -> Vec<SelectorItem> {
    let failover = failover_enabled_for(config, provider);
    let pinned_note = if failover {
        " (pins account; failover disabled while pinned)"
    } else {
        ""
    };
    let active = active_account;
    // Auth is read from disk, not the startup snapshot: an account logged
    // in or refreshed by another process (the CLI, the TUI's own /login)
    // appears and reads current without a /reload. A missing file keeps
    // the snapshot (nothing was ever stored; the snapshot may hold
    // process-only state a unit harness built).
    let auth = if config.paths.auth_path.exists() {
        noel_config::read_auth_file(&config.paths.auth_path).unwrap_or_else(|_| config.auth.clone())
    } else {
        config.auth.clone()
    };
    let mut items = vec![SelectorItem {
        label: if failover {
            "auto (failover: sticky account, switches on limits)".to_string()
        } else {
            "auto (default resolution)".to_string()
        },
        value: String::new(),
        active: active.is_none(),
    }];
    items.extend(
        listed_accounts_for_provider(&auth, provider)
            .into_iter()
            .map(|(account, source)| {
                // The token's own expiry state, so a stale snapshot cannot
                // call a refreshed account expired (the request path
                // refreshes within its window before use anyway).
                let state = match &source {
                    AccountSource::Stored => {
                        let expiring = auth
                            .credential(provider, &account)
                            .and_then(|credential| match credential {
                                AuthCredential::OAuth { expires, .. } => Some(*expires),
                                _ => None,
                            })
                            .map(|expires| {
                                expires
                                    <= unix_seconds().unwrap_or_default()
                                        + crate::oauth_refresh::REFRESH_WINDOW_SECONDS
                            })
                            .unwrap_or(false);
                        if expiring {
                            format!("{account} (expires soon; refreshed before use){pinned_note}")
                        } else {
                            format!("{account}{pinned_note}")
                        }
                    }
                    AccountSource::Environment => {
                        format!("{account} (environment, read-only)")
                    }
                    AccountSource::ImportedFile => {
                        format!("{account} (imported login file, read-only)")
                    }
                };
                SelectorItem {
                    label: state,
                    active: active == Some(account.as_str()),
                    value: account,
                }
            }),
    );
    items
}

fn settings_selector_items(config: &LoadedConfig) -> Vec<SelectorItem> {
    vec![
        bool_setting_item(
            "auto compact",
            "compaction.enabled",
            auto_compaction_enabled(config),
        ),
        bool_setting_item(
            "show images",
            "terminal.showImages",
            config
                .settings
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.show_images)
                .unwrap_or(false),
        ),
        bool_setting_item(
            "terminal progress",
            "terminal.showTerminalProgress",
            terminal_progress_enabled(config),
        ),
        bool_setting_item(
            "auto resize images",
            "images.autoResize",
            images_auto_resize(config),
        ),
        bool_setting_item("block images", "images.blockImages", images_blocked(config)),
        bool_setting_item(
            "provider retry",
            "retry.enabled",
            config
                .settings
                .retry
                .as_ref()
                .and_then(|retry| retry.enabled)
                .unwrap_or(true),
        ),
        bool_setting_item(
            "background model refresh",
            "modelRefresh.enabled",
            model_refresh_enabled(&config.settings),
        ),
        bool_setting_item(
            "hide thinking block",
            "hideThinkingBlock",
            config.settings.hide_thinking_block.unwrap_or(false),
        ),
        bool_setting_item(
            "Anthropic extra usage warning",
            "warnings.anthropicExtraUsage",
            config
                .settings
                .warnings
                .as_ref()
                .and_then(|warnings| warnings.anthropic_extra_usage)
                .unwrap_or(true),
        ),
        // follow-up mode selector removed: queued bursts always dispatch as
        // one combined turn; there is no mode to choose anymore.
    ]
}

fn bool_setting_item(label: &str, value: &str, active: bool) -> SelectorItem {
    SelectorItem {
        label: format!("{label}: {}", on_off(active)),
        value: value.to_string(),
        active,
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
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

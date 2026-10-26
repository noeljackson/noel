use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
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
mod clipboard;
mod markdown;
mod model_select;
mod oauth_login;
mod oauth_refresh;
mod resource_commands;
mod rpc;
mod rpc_wire;
mod server;
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
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
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
    continue_turn_streaming_events, default_active_tool_names, expand_input, format_todo_list,
    run_excluded_bash_result, run_user_turn, run_user_turn_streaming_events_with_media,
    run_user_turn_streaming_with_media, write_session_export, AgentError, CompactionKind,
    ConversationMessage, FollowUpQueue, MessageRole, ReloadReport, ReloadableSystems, RunControl,
    Runtime, SessionError, SessionState, SessionStore, SessionWorkspace, SteeringMailbox, TodoItem,
    TodoStatus, TurnEvent, PRODUCT_NAME, STEERING_JOIN,
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

fn account_suffix(account: &str) -> String {
    if account == DEFAULT_ACCOUNT_NAME {
        String::new()
    } else {
        format!(" (account {account})")
    }
}

fn login_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
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
        return attach::run(session, endpoint, config, cli.owned_screen()).await;
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
    let mut runtime =
        create_runtime_when_unlocked(&cli, &cwd, &config, systems, requested_workspace.as_ref())
            .await?;
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

    run_interactive(runtime, config, offline, cli.owned_screen()).await
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
        .and_then(|resolved| resolved.model)
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
    // The new session keeps the workspace, as it keeps the model.
    let (store, mut state) = SessionStore::create_with_workspace(
        &config.paths.session_dir,
        runtime.session().cwd.clone(),
        runtime.session().workspace.clone(),
    )?;
    state.active_model = runtime.session().active_model.clone();
    state.active_thinking_level = runtime.session().active_thinking_level.clone();
    state.active_account = runtime.session().active_account.clone();
    store.record_active_model(state.active_model.clone())?;
    store.record_active_thinking_level(state.active_thinking_level.clone())?;
    store.record_active_account(state.active_account.clone())?;
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

async fn run_interactive(
    mut runtime: Runtime,
    mut config: LoadedConfig,
    offline: bool,
    owned_screen: bool,
) -> Result<()> {
    let mut app = TuiApp::new(&config, &runtime);
    app.owned_screen = owned_screen;
    if let Some(hint) = resume_hint(&runtime) {
        app.push(TuiEntryKind::System, hint);
    }
    let mut auto_restart = AutoRestart::from_env();
    let (_restore, mut terminal, mut viewport_height) = open_tui_terminal(owned_screen)?;

    let mut restart = false;
    loop {
        app.refresh_chrome(&config, &runtime);
        // A mid-turn /accounts probe that outlived its turn still lands.
        settle_account_status_task(&mut app).await;
        redraw_tui(&mut terminal, &mut app, &config)?;
        // Only schedule once per open/resume, never on every idle tick after
        // an error or explicit interruption. Use the ordinary live turn UI.
        if std::mem::take(&mut app.resume_pending) && runtime.session().has_unfinished_turn() {
            let result = resume_turn_with_queue_tui(
                &mut app,
                &mut TuiSurface {
                    terminal: &mut terminal,
                    viewport_height: &mut viewport_height,
                },
                &mut runtime,
                &config,
                offline,
            )
            .await;
            match result {
                Ok(TurnControl::Quit) => break,
                Ok(_) => {}
                Err(error) => {
                    app.drop_live_entry();
                    app.push(
                        TuiEntryKind::Error,
                        format_tui_error(&error, &runtime, &config),
                    );
                }
            }
            app.refresh_diff_panel(&runtime);
            if let Some(session_id) = app.pending_session_resume.take() {
                // A locked or unreadable pick is reported; the TUI stays up.
                if let Err(error) =
                    apply_midturn_session_resume(&mut app, &mut runtime, &config, &session_id)
                {
                    app.push(TuiEntryKind::Error, format!("{error:#}"));
                }
            }
            continue;
        }
        // Input wins over a pending restart so quit keys are honored even
        // while the dogfood watcher is rebuilding continuously.
        if event::poll(Duration::from_millis(100))? {
            let quit = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_tui_key(
                        key,
                        &mut TuiSurface {
                            terminal: &mut terminal,
                            viewport_height: &mut viewport_height,
                        },
                        &mut app,
                        &mut runtime,
                        &mut config,
                        offline,
                    )
                    .await?
                }
                Event::Mouse(mouse) => {
                    handle_transcript_mouse(mouse, &mut app);
                    copy_pending_selection(&mut app).await;
                    false
                }
                Event::Paste(text) => {
                    app.paste_text(&text);
                    false
                }
                Event::Resize(_, _) => {
                    resize_tui_viewport(&mut terminal, &mut viewport_height, app.owned_screen)?;
                    false
                }
                _ => false,
            };
            if quit {
                break;
            }
            continue;
        }
        // An error/interruption must not become an automatic retry loop just
        // because a rebuild is pending. Restart only when the session has no
        // unfinished turn AND the TUI is not streaming: app.turn_running is
        // the live signal, and has_unfinished_turn distinguishes "stopped by
        // the user this process" (keep waiting) from "between user prompts"
        // (safe to restart). A resume-driven restart re-enters the same
        // session and would otherwise re-dispatch a stopped turn.
        if !app.turn_running
            && !runtime.session().has_unfinished_turn()
            && auto_restart.should_restart()?
        {
            app.push(TuiEntryKind::System, "rebuilt; restarting");
            redraw_tui(&mut terminal, &mut app, &config)?;
            restart = true;
            break;
        }
    }
    close_tui_terminal(terminal, _restore, &app)?;
    if restart {
        // The session lock is close-on-exec: the new image reopens this
        // session as soon as it starts.
        restart_current_process(runtime.store().map(SessionStore::path))?;
    }
    Ok(())
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

/// Gives the terminal back, then prints the transcript for the e2e suites
/// when `NOEL_TUI_E2E_DUMP=1`.
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
    Ok(())
}

/// Infer unfinished model work, not its stop reason. Direct slash-tool
/// results have no call id and complete their turn without an assistant reply.
fn resume_hint(runtime: &Runtime) -> Option<String> {
    let queued = runtime.session().queued_messages.len();
    let mid_turn = runtime.session().has_unfinished_turn();
    match (mid_turn, queued) {
        (false, 0) => None,
        (true, 0) => {
            Some("resumed mid-turn; continuing automatically from saved context".to_string())
        }
        (false, n) => Some(format!(
            "{n} queued message(s) pending; they run after your next prompt (/queue to list, /queue-clear to clear)"
        )),
        (true, n) => Some(format!(
            "resumed mid-turn; continuing automatically from saved context, then {n} queued message(s)"
        )),
    }
}

struct AutoRestart {
    executable: PathBuf,
    modified: Option<SystemTime>,
    pending: Option<(SystemTime, Instant)>,
}

/// A changed binary must stay untouched this long before the restart fires,
/// so a watcher that rebuilds continuously does not trap the UI in a
/// restart loop.
const RESTART_DEBOUNCE: Duration = Duration::from_secs(2);

impl AutoRestart {
    fn from_env() -> Self {
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

    fn should_restart(&mut self) -> Result<bool> {
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

    fn cycle_thinking(&mut self, delta: isize) {
        let Some(current) = self.selected_thinking_level() else {
            return;
        };
        let Some(levels) = self
            .selected_model_thinking()
            .map(|thinking| &thinking.levels)
        else {
            return;
        };
        let current_index = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        let step = delta.unsigned_abs() % levels.len();
        let next_index = if delta.is_negative() {
            (current_index + levels.len() - step) % levels.len()
        } else {
            (current_index + step) % levels.len()
        };
        self.thinking_level = Some(levels[next_index].to_string());
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
    header_title: String,
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
    queued_messages: Vec<String>,
    turn_running: bool,
    /// True once the running turn has made a tool call; a prose-only turn
    /// has no interior round boundary, so ctrl+s steering only lands after
    /// the response finishes and the hint must not promise a boundary.
    turn_saw_tools: bool,
    /// True once the reply of the current provider round has streamed text.
    /// If the turn drops that reply to ask again, the text stays where it is
    /// and the next reply starts a new entry below it.
    round_saw_text: bool,
    /// True while a compaction answers a context overflow: if it fails, the
    /// turn does not go on with the full history, it stops asking.
    overflow_compaction: bool,
    /// Provider name of the active model, kept in sync for pickers that open
    /// while the runtime is borrowed by a streaming turn.
    active_model_provider: Option<String>,
    /// The session's pinned account, kept in sync the same way for the
    /// account picker opened mid-turn.
    active_session_account: Option<String>,
    /// A mid-turn /model <ref> resolved; apply when the turn ends.
    pending_model_switch: Option<String>,
    /// A mid-turn /account selection; apply when the turn ends.
    /// `Some(None)` is a queued switch to auto.
    pending_account_switch: Option<Option<String>>,
    /// A mid-turn /accounts probe: the transcript placeholder it fills and
    /// the background task collecting the status rows.
    account_status_task: Option<(usize, tokio::task::JoinHandle<String>)>,
    /// A mid-turn /resume selection; swap sessions once the turn is torn
    /// down (the picker already closed).
    pending_session_resume: Option<String>,
    resume_pending: bool,
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
    /// Provider of the session's active model, for picker highlights when
    /// the runtime itself is borrowed by a streaming turn.
    fn active_provider(&self) -> Option<&str> {
        self.active_model_provider.as_deref()
    }

    fn new(config: &LoadedConfig, runtime: &Runtime) -> Self {
        let mut app = Self {
            resume_pending: runtime.session().has_unfinished_turn(),
            status: "Ready".to_string(),
            ..Self::default()
        };
        app.refresh_chrome(config, runtime);
        app.push(
            TuiEntryKind::System,
            format!(
                "{}\ntype /help for commands, /reload to reload config, /quit to exit",
                terminal_renderer(config).banner()
            ),
        );
        app.restore_session_messages(runtime);
        if let Err(error) = terminal_theme(config) {
            app.push(TuiEntryKind::Error, format!("{error}; using system theme"));
        }
        app.status = footer_status(config, runtime, &app.editor_state);
        app
    }

    fn refresh_chrome(&mut self, config: &LoadedConfig, runtime: &Runtime) {
        self.active_model_provider = runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| model.provider.clone());
        self.active_session_account = runtime.session().active_account.clone();
        let model = runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .unwrap_or_else(|| "no model".to_string());
        self.header_title = format!(
            " noel  {}  {} ",
            config
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "system".to_string()),
            model
        );
        let session = runtime.session();
        let name = session
            .name
            .clone()
            .unwrap_or_else(|| session.session_id.chars().take(8).collect());
        self.workspace = session.workspace.clone();
        self.header_line = match &self.workspace {
            // The footer puts the workspace first, as it draws.
            Some(_) => format!("{name}  queue:{}", session.queued_messages.len()),
            None => format!(
                "{}  {name}  queue:{}",
                session.cwd.display(),
                session.queued_messages.len()
            ),
        };
        self.status = footer_status(config, runtime, &self.editor_state);
        if let Some(since) = self.quit_armed_since {
            if since.elapsed() < QUIT_CONFIRM_WINDOW {
                self.status = "press ctrl+c/ctrl+d again to quit".to_string();
            } else {
                self.quit_armed_since = None;
            }
        }
        self.show_hardware_cursor = config.settings.show_hardware_cursor.unwrap_or(true);
        self.todos = runtime.session().todos.clone();
        self.queued_messages = runtime.session().queued_messages.clone();
    }

    fn refresh_diff_panel(&mut self, runtime: &Runtime) {
        if let Some(panel) = self.diff_panel.as_mut() {
            let selected = panel.selected;
            panel.files =
                collect_diff_file_entries(&runtime.session().cwd, &runtime.session().edited_files);
            panel.selected = selected.min(panel.files.len().saturating_sub(1));
            let open_detail = panel.detail.as_ref().map(|detail| detail.path.clone());
            if let Some(detail_path) = open_detail {
                panel.detail = panel
                    .files
                    .iter()
                    .find(|entry| entry.path == detail_path)
                    .map(|entry| build_diff_detail(&runtime.session().cwd, entry));
            }
        }
    }

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

    fn restore_session_messages(&mut self, runtime: &Runtime) {
        for message in &runtime.session().messages {
            if message.role == MessageRole::Assistant {
                self.push(TuiEntryKind::Thinking, message.thinking.clone());
            }
            let kind = match message.role {
                MessageRole::User => TuiEntryKind::User,
                MessageRole::Assistant => TuiEntryKind::Assistant,
                MessageRole::Tool => TuiEntryKind::Tool,
                MessageRole::System => TuiEntryKind::System,
            };
            let text = if message.role == MessageRole::Tool {
                format_session_tool_message(message, runtime)
            } else {
                message.content.clone()
            };
            self.push(kind, text);
            if message.aborted {
                self.push(TuiEntryKind::System, "aborted");
            }
            for media in &message.media {
                self.push(TuiEntryKind::System, format_media_fallback(media));
            }
            if message.role == MessageRole::User {
                self.editor_state.record_history(message.content.clone());
            }
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

    fn insert_entry(&mut self, index: usize, kind: TuiEntryKind, text: impl Into<String>) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        let index = index.min(self.entries.len());
        self.entries.insert(index, TuiEntry { kind, text });
        self.transcript_scrollback.insert(index);
        self.transcript_view.get_mut().insert(index);
        if let Some(live_index) = self.live_entry_index.as_mut() {
            if index <= *live_index {
                *live_index += 1;
            }
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

    fn drop_live_entry(&mut self) {
        self.live_thinking_index = None;
        self.turn_running = false;
        self.turn_saw_tools = false;
        self.remove_live_entry();
    }

    /// Removes the entry a turn was filling, while the turn goes on or as
    /// it ends: a placeholder that nothing filled.
    fn remove_live_entry(&mut self) {
        let Some(index) = self.live_entry_index.take() else {
            return;
        };
        if index < self.entries.len() {
            self.remove_entry(index);
        }
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
    fn insert(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.insert(index, EntryScrollback::default());
        }
    }

    fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }

    fn truncate(&mut self, len: usize) {
        self.entries.truncate(len);
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

struct TuiSurface<'a> {
    terminal: &'a mut TuiTerminal,
    viewport_height: &'a mut u16,
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

#[cfg(test)]
fn selected_todos(todos: &[TodoItem], expanded: bool) -> Vec<usize> {
    selected_todos_with_limit(todos, expanded, todo_row_limit(expanded))
}

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
    let mut in_fence = false;
    // Ordered-list state: an ordered item numbers itself from the list's
    // start marker plus its position (upstream's list renderer), so a
    // source that numbers every item `1.` still displays `1. 2. 3.`. A
    // plain line or a bullet ends the sequence.
    let mut ordered_list: Option<(u64, u64)> = None;
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
        if in_fence {
            // Inside a code block: no emphasis, dim accent bar, indent.
            if source_line.trim_start().starts_with("```") && starts_line {
                in_fence = false;
                row.line = Line::from(Span::styled(String::new(), base));
                continue;
            }
            row.line = Line::from(vec![
                Span::styled("  ┆ ", Style::default().fg(accent)),
                Span::styled(row_text.to_string(), base),
            ]);
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
                in_fence = true;
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

async fn handle_tui_key(
    key: KeyEvent,
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    offline: bool,
) -> Result<bool> {
    if app.selector.is_some() {
        // A failed pick (for example a session locked by another process) is
        // an error line, never a reason to leave the TUI.
        if let Err(error) = handle_tui_selector_key(key, app, runtime, config) {
            app.push(
                TuiEntryKind::Error,
                format_tui_error(&error, runtime, config),
            );
        }
        app.status = footer_status(config, runtime, &app.editor_state);
        return Ok(false);
    }
    let key_name = key_event_name(&key);
    let bindings = keybinding_map(config);
    // Editing a newline must win over panel Enter actions, including an
    // empty draft with the diff panel open.
    if key_name
        .as_deref()
        .is_some_and(|name| bindings.matches("newline", name))
    {
        app.insert_input_newline();
        app.quit_armed_since = None;
        app.refresh_chrome(config, runtime);
        return Ok(false);
    }
    if app.diff_panel.is_some() && handle_diff_panel_key(&key, app, runtime) {
        app.refresh_chrome(config, runtime);
        return Ok(false);
    }
    if handle_transcript_key(&key, app) {
        copy_pending_selection(app).await;
        return Ok(false);
    }
    if let Some(name) = key_name {
        if bindings.matches("interrupt", &name) {
            if !app.input.is_empty() || !app.input_media.is_empty() {
                // With a draft present, ctrl+c clears it and ctrl+d does
                // nothing; neither arms quit.
                if name != "ctrl+d" {
                    app.clear_input();
                }
                app.quit_armed_since = None;
                app.refresh_chrome(config, runtime);
                return Ok(false);
            }
            if app.quit_requested(Instant::now()) {
                return Ok(true);
            }
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        app.quit_armed_since = None;
        if bindings.matches("paste-image", &name) {
            paste_clipboard_image(app, config).await;
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("expand", &name) {
            app.previews_expanded = !app.previews_expanded;
            return Ok(false);
        }
        if bindings.matches("todos", &name) {
            app.todos_expanded = !app.todos_expanded;
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("editor", &name) {
            edit_input_in_external_editor(app);
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("line-start", &name) {
            app.cursor_to_line_start();
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("line-end", &name) {
            app.cursor_to_line_end();
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("cursor-left", &name) {
            app.move_cursor_left();
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("cursor-right", &name) {
            app.move_cursor_right();
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        if bindings.matches("steer", &name) {
            return submit_tui_input(surface, app, runtime, config, offline).await;
        }
    }
    match key.code {
        KeyCode::Esc => {
            app.clear_input();
            app.multiline = None;
        }
        KeyCode::Backspace => {
            app.pop_input_char();
        }
        KeyCode::Delete => {
            app.delete_input_char();
        }
        KeyCode::Up if app.multiline.is_none() => {
            if app.input.is_empty() && !runtime.session().queued_messages.is_empty() {
                let mut queue = runtime.session().queued_messages.clone();
                let message = queue.pop().expect("nonempty queue");
                runtime.replace_queued_messages(queue)?;
                recall_queued_message(app, message);
            } else {
                app.history_previous();
            }
        }
        KeyCode::Down if app.multiline.is_none() => {
            app.history_next();
        }
        KeyCode::Enter => {
            return submit_tui_input(surface, app, runtime, config, offline).await;
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_input_char(ch);
        }
        _ => {}
    }
    app.refresh_chrome(config, runtime);
    Ok(false)
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

/// Submits the current input as a prompt. Shared by Enter and the steer
/// binding, which sends immediately when no turn is streaming.
async fn submit_tui_input(
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    offline: bool,
) -> Result<bool> {
    let line = app.input.trim().to_string();
    let media = std::mem::take(&mut app.input_media);
    app.clear_input();
    app.input_media = media;
    if !line.is_empty() || !app.input_media.is_empty() {
        let line = if line.is_empty() {
            "Describe this image.".to_string()
        } else {
            line
        };
        let quit = match handle_tui_submission(app, surface, runtime, config, offline, line).await {
            Ok(quit) => quit,
            Err(error) => {
                app.push(
                    TuiEntryKind::Error,
                    format_tui_error(&error, runtime, config),
                );
                false
            }
        };
        app.refresh_chrome(config, runtime);
        return Ok(quit);
    }
    app.refresh_chrome(config, runtime);
    Ok(false)
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

fn handle_diff_panel_key(key: &KeyEvent, app: &mut TuiApp, runtime: &Runtime) -> bool {
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
            let detail = panel
                .files
                .get(panel.selected)
                .map(|entry| build_diff_detail(&runtime.session().cwd, entry));
            if let Some(detail) = detail {
                panel.detail = Some(detail);
            }
        }
        KeyCode::Esc if app.owned_screen || app.input.is_empty() => app.diff_panel = None,
        _ => return false,
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StreamingKeyOutcome {
    Ignored,
    Changed,
    Interrupt,
    Quit,
    /// A mid-turn /model <ref> resolved; apply after the turn ends.
    SwitchModel(String),
    /// A mid-turn /account selection; apply after the turn ends.
    SwitchAccount(Option<String>),
    /// A mid-turn /resume selection: kill the turn and swap sessions.
    ResumeSession(String),
}

/// How a streaming turn ended: the provider finished, the user interrupted,
/// or the user asked to quit mid-turn.
enum StreamEnd {
    Finished(Result<String, AgentError>),
    Interrupted,
    Quit,
}

fn handle_streaming_tui_key(
    key: KeyEvent,
    app: &mut TuiApp,
    followups: &FollowUpQueue,
    steering: &SteeringMailbox,
    config: &LoadedConfig,
) -> StreamingKeyOutcome {
    if let Some(panel) = &mut app.diff_panel {
        if let Some(detail) = &mut panel.detail {
            match key.code {
                KeyCode::Esc => {
                    panel.detail = None;
                    return StreamingKeyOutcome::Changed;
                }
                KeyCode::PageUp => {
                    detail.scroll = detail.scroll.saturating_sub(20);
                    return StreamingKeyOutcome::Changed;
                }
                KeyCode::PageDown => {
                    detail.scroll = (detail.scroll + 20).min(detail.lines.len().saturating_sub(1));
                    return StreamingKeyOutcome::Changed;
                }
                _ => {}
            }
        } else if key.code == KeyCode::Esc {
            app.diff_panel = None;
            return StreamingKeyOutcome::Changed;
        }
    }
    if handle_transcript_key(&key, app) {
        return StreamingKeyOutcome::Changed;
    }
    if let Some(name) = key_event_name(&key) {
        let bindings = keybinding_map(config);
        if bindings.matches("interrupt", &name) {
            // A selector open mid-turn still honors the interrupt binding.
            if app.selector.is_some() {
                app.selector = None;
                return StreamingKeyOutcome::Changed;
            }
            return StreamingKeyOutcome::Interrupt;
        }
        if bindings.matches("steer", &name) {
            if !app.input_media.is_empty() {
                app.push(
                    TuiEntryKind::System,
                    "image draft kept; submit after the running turn finishes",
                );
                return StreamingKeyOutcome::Changed;
            }
            let line = app.input.trim().to_string();
            app.clear_input();
            for queued in followups.drain() {
                steering.send(queued.clone());
                app.push(TuiEntryKind::System, format!("steering> {queued}"));
            }
            if !line.is_empty() {
                steering.send(expand_input(config, &line));
                app.push(TuiEntryKind::System, format!("steering> {line}"));
            }
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("expand", &name) {
            app.previews_expanded = !app.previews_expanded;
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("todos", &name) {
            app.todos_expanded = !app.todos_expanded;
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("newline", &name) {
            app.insert_input_newline();
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("line-start", &name) {
            app.cursor_to_line_start();
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("line-end", &name) {
            app.cursor_to_line_end();
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("cursor-left", &name) {
            app.move_cursor_left();
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("cursor-right", &name) {
            app.move_cursor_right();
            return StreamingKeyOutcome::Changed;
        }
    }
    match key.code {
        KeyCode::Up if app.input.is_empty() => {
            if app.selector.is_some() {
                if let Some(selector) = app.selector.as_mut() {
                    selector.move_selection(-1);
                }
                return StreamingKeyOutcome::Changed;
            }
            if let Some(message) = followups.pop() {
                recall_queued_message(app, message);
            }
            StreamingKeyOutcome::Changed
        }
        KeyCode::Down if app.selector.is_some() => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(1);
            }
            StreamingKeyOutcome::Changed
        }
        KeyCode::PageUp if app.selector.is_some() => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(-(SELECTOR_PAGE_STEP as isize));
            }
            StreamingKeyOutcome::Changed
        }
        KeyCode::PageDown if app.selector.is_some() => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(SELECTOR_PAGE_STEP as isize);
            }
            StreamingKeyOutcome::Changed
        }
        KeyCode::Backspace if app.selector.is_some() => {
            if let Some(selector) = app.selector.as_mut() {
                selector.pop_query_char();
            }
            StreamingKeyOutcome::Changed
        }
        KeyCode::Esc => {
            if app.selector.is_some() {
                app.selector = None;
                return StreamingKeyOutcome::Changed;
            }
            StreamingKeyOutcome::Interrupt
        }
        KeyCode::Backspace => {
            app.pop_input_char();
            StreamingKeyOutcome::Changed
        }
        KeyCode::Delete => {
            app.delete_input_char();
            StreamingKeyOutcome::Changed
        }
        KeyCode::Enter => {
            if app.selector.is_some() {
                // Selecting from a mid-turn picker: session picks kill the
                // turn and swap; model picks switch at turn end; provider
                // picks drill in (two-layer) without touching the turn.
                let selector = app.selector.take().expect("selector");
                match selector.kind.as_str() {
                    "session" => {
                        if let Some(item) = selector.selected_item() {
                            return StreamingKeyOutcome::ResumeSession(item.value.clone());
                        }
                        return StreamingKeyOutcome::Changed;
                    }
                    "provider" => {
                        // Drill into the provider's models; keep streaming.
                        let provider = selector
                            .selected_item()
                            .map(|item| item.value.clone())
                            .unwrap_or_default();
                        let models: Vec<SelectorItem> = picker_models(config)
                            .into_iter()
                            .filter(|model| model.provider == provider)
                            .map(|model| {
                                let value = format!("{}/{}", model.provider, model.id);
                                SelectorItem {
                                    label: value.clone(),
                                    value,
                                    active: false,
                                }
                            })
                            .collect();
                        if models.is_empty() {
                            app.push(TuiEntryKind::System, format!("no models for {provider}"));
                            return StreamingKeyOutcome::Changed;
                        }
                        let mut state = TuiSelectorState::new(
                            "model",
                            Selector::new(format!("model · {provider}"), models),
                            "",
                            None,
                        );
                        state.provider_filter = Some(provider);
                        app.selector = Some(state);
                        return StreamingKeyOutcome::Changed;
                    }
                    "model" => {
                        if let Some(item) = selector.selected_item() {
                            return StreamingKeyOutcome::SwitchModel(item.value.clone());
                        }
                        return StreamingKeyOutcome::Changed;
                    }
                    "account" => {
                        if let Some(item) = selector.selected_item() {
                            return StreamingKeyOutcome::SwitchAccount(if item.value.is_empty() {
                                None
                            } else {
                                Some(item.value.clone())
                            });
                        }
                        return StreamingKeyOutcome::Changed;
                    }
                    _ => return StreamingKeyOutcome::Changed,
                }
            }
            if !app.input_media.is_empty() {
                app.push(
                    TuiEntryKind::System,
                    "image draft kept; submit after the running turn finishes",
                );
                return StreamingKeyOutcome::Changed;
            }
            let line = app.input.trim().to_string();
            app.clear_input();
            if let Some(command) = line.strip_prefix('/') {
                return handle_streaming_command(command, app, followups, config);
            }
            if !line.is_empty() {
                followups.push(line);
            }
            StreamingKeyOutcome::Changed
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_input_char(ch);
            StreamingKeyOutcome::Changed
        }
        _ => StreamingKeyOutcome::Ignored,
    }
}

/// Whether account status is unavailable mid-turn: offline mode (the CLI
/// flag or `NOEL_OFFLINE`), read without the runtime, which the running
/// turn borrows.
fn runtime_offline(_app: &TuiApp, _config: &LoadedConfig) -> bool {
    offline_enabled(false)
}

/// Collects account status rows without the runtime, for the mid-turn
/// `/accounts`: a background task runs it and the transcript placeholder
/// it filled is replaced with the result on a later render tick.
async fn collect_account_status_text(config: LoadedConfig) -> String {
    let now = unix_seconds().unwrap_or(0) as i64;
    let rows = usage::collect_account_status(&config, false, None, None, now).await;
    usage::format_account_status(&rows, now)
}

/// Replaces the transcript placeholder of a finished mid-turn `/accounts`
/// probe with its rows. Called from every render path while it runs: the
/// streaming drain during the turn, the idle loop after it, so a probe
/// that outlives its turn still lands. Returns whether the transcript
/// changed.
async fn settle_account_status_task(app: &mut TuiApp) -> bool {
    let Some((index, task)) = app.account_status_task.take() else {
        return false;
    };
    if !task.is_finished() {
        app.account_status_task = Some((index, task));
        return false;
    }
    match task.await {
        Ok(text) => {
            app.replace_entry(index, text);
            true
        }
        Err(error) => {
            app.replace_entry(index, format!("account status failed: {error}"));
            true
        }
    }
}

/// Slash commands typed mid-turn. Only turn-local actions work here: the
/// session itself is borrowed by the running turn, so anything that needs
/// it is rejected with a note instead of being swallowed as prompt text.
fn handle_streaming_command(
    command: &str,
    app: &mut TuiApp,
    followups: &FollowUpQueue,
    config: &LoadedConfig,
) -> StreamingKeyOutcome {
    match command {
        "queue" => {
            let pending = followups.list();
            if pending.is_empty() {
                app.push(TuiEntryKind::System, "no pending follow-ups");
            } else {
                let list = pending
                    .iter()
                    .enumerate()
                    .map(|(index, message)| format!("{}. {}", index + 1, message))
                    .collect::<Vec<_>>()
                    .join("\n");
                app.push(TuiEntryKind::System, format!("pending follow-ups:\n{list}"));
            }
            StreamingKeyOutcome::Changed
        }
        command if command.starts_with("queue ") => {
            let message = command.trim_start_matches("queue ").trim();
            if !message.is_empty() {
                followups.push(expand_input(config, message));
            }
            StreamingKeyOutcome::Changed
        }
        "queue-clear" => {
            let cleared = followups.clear();
            app.push(
                TuiEntryKind::System,
                format!("cleared {cleared} pending follow-up(s)"),
            );
            StreamingKeyOutcome::Changed
        }
        "copy" if app.transcript_view.borrow().has_selection() => {
            app.transcript_view.get_mut().request_copy();
            StreamingKeyOutcome::Changed
        }
        "todos" => {
            app.todos_expanded = !app.todos_expanded;
            StreamingKeyOutcome::Changed
        }
        "help" => {
            app.push(TuiEntryKind::System, terminal_renderer(config).help());
            StreamingKeyOutcome::Changed
        }
        "interrupt" => StreamingKeyOutcome::Interrupt,
        "quit" | "exit" | "q" => StreamingKeyOutcome::Quit,
        // Mid-turn model switching: a reference resolves against the static
        // model list (no runtime borrow) and applies at the next turn; a bare
        // /model opens the two-layer picker over the running turn.
        command if command == "model" || command == "models" || command == "scoped-models" => {
            let selector = Selector::new(
                "provider",
                provider_selector_items(&picker_models(config), app.active_provider()),
            );
            if selector.items.is_empty() {
                app.push(TuiEntryKind::System, "no models".to_string());
            } else {
                app.push(TuiEntryKind::System, "model selector".to_string());
                app.selector = Some(TuiSelectorState::new("provider", selector, "", None));
            }
            StreamingKeyOutcome::Changed
        }
        command
            if command.starts_with("model ")
                || command.starts_with("models ")
                || command.starts_with("scoped-models ") =>
        {
            let reference = command.split_once(' ').map(|x| x.1).unwrap_or("").trim();
            if resolve_model_reference(config, reference, false).is_some() {
                app.push(
                    TuiEntryKind::System,
                    format!("model switch queued: {reference} (applies after this turn)"),
                );
                StreamingKeyOutcome::SwitchModel(reference.to_string())
            } else {
                app.push(TuiEntryKind::Error, format!("model not found: {reference}"));
                StreamingKeyOutcome::Changed
            }
        }
        // Mid-turn account status: the probes run in a background task and
        // fill the transcript placeholder when they answer; the turn keeps
        // streaming while they run.
        "accounts" | "account-status" | "usage" => {
            if runtime_offline(app, config) {
                app.push(
                    TuiEntryKind::System,
                    "account quotas unavailable in offline mode",
                );
                return StreamingKeyOutcome::Changed;
            }
            if app.account_status_task.is_some() {
                app.push(TuiEntryKind::System, "account status already being checked");
                return StreamingKeyOutcome::Changed;
            }
            let checking = app.push_placeholder(TuiEntryKind::System, "checking account status...");
            let task = tokio::spawn(collect_account_status_text(config.clone()));
            app.account_status_task = Some((checking, task));
            StreamingKeyOutcome::Changed
        }
        // Mid-turn account switching: the picker opens over the running
        // turn; the selection applies when the turn ends, like /model.
        "account" => {
            let Some(provider) = app.active_provider().map(str::to_string) else {
                app.push(TuiEntryKind::System, "no model".to_string());
                return StreamingKeyOutcome::Changed;
            };
            let items = account_selector_items_for(
                config,
                &provider,
                app.active_session_account.as_deref(),
            );
            if items.is_empty() {
                app.push(TuiEntryKind::System, "no accounts".to_string());
            } else {
                app.push(TuiEntryKind::System, "account selector".to_string());
                app.selector = Some(TuiSelectorState::new(
                    "account",
                    Selector::new("account", items),
                    "",
                    None,
                ));
            }
            StreamingKeyOutcome::Changed
        }
        command if command.starts_with("account ") => {
            let account = command.trim_start_matches("account ").trim();
            let auto = account.is_empty() || account == "auto";
            let provider = app.active_provider();
            let known = provider.is_some_and(|provider| {
                auto || listed_accounts_for_provider(&config.auth, provider)
                    .into_iter()
                    .any(|(name, _)| name == account)
            });
            if !known {
                let shown = if account.is_empty() { "auto" } else { account };
                app.push(TuiEntryKind::Error, format!("account not found: {shown}"));
            } else {
                app.push(
                    TuiEntryKind::System,
                    format!(
                        "account switch queued: {} (applies after this turn)",
                        if auto { "auto" } else { account }
                    ),
                );
                return StreamingKeyOutcome::SwitchAccount(if auto {
                    None
                } else {
                    Some(account.to_string())
                });
            }
            StreamingKeyOutcome::Changed
        }
        // Mid-turn resume opens the session picker; picking kills the turn.
        "resume" => {
            match session_selector_items(config) {
                Ok(items) if items.is_empty() => {
                    app.push(TuiEntryKind::System, "no sessions".to_string());
                }
                Ok(items) => {
                    let selector = Selector::new("session", items);
                    app.push(
                        TuiEntryKind::System,
                        "resume session: enter to resume · type to filter · esc cancel",
                    );
                    app.selector = Some(TuiSelectorState::new("session", selector, "", None));
                }
                Err(error) => {
                    app.push(TuiEntryKind::Error, format!("{error:#}"));
                }
            }
            StreamingKeyOutcome::Changed
        }
        _ => {
            // A skill command or a prompt template queues as a follow-up,
            // like any text typed mid-turn.
            let typed = format!("/{command}");
            let prompt = expand_input(config, &typed);
            if prompt == typed {
                app.push(
                    TuiEntryKind::System,
                    format!("{typed} is not available while a turn is running"),
                );
            } else {
                followups.push(prompt);
            }
            StreamingKeyOutcome::Changed
        }
    }
}

async fn drain_streaming_tui_events(
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    followups: &FollowUpQueue,
    steering: &SteeringMailbox,
    config: &LoadedConfig,
) -> Result<(bool, Option<TurnControl>)> {
    if app.owned_screen {
        let root = tui_layout(surface.terminal.get_frame().area(), app, config);
        let area = transcript_area(root[0], app);
        let palette = terminal_theme(config).unwrap_or_default().palette;
        app.transcript_view.borrow_mut().sync(
            &app.entries,
            area,
            &palette,
            config.settings.hide_thinking_block.unwrap_or(false),
            &transcript_preview_state(app),
        );
    }
    let mut changed = false;
    let mut control = None;
    // A mid-turn /accounts probe that finished replaces its placeholder.
    changed |= settle_account_status_task(app).await;
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key_event_name(&key)
                    .is_some_and(|name| keybinding_map(config).matches("paste-image", &name))
                {
                    paste_clipboard_image(app, config).await;
                    changed = true;
                    continue;
                }
                match handle_streaming_tui_key(key, app, followups, steering, config) {
                    StreamingKeyOutcome::Ignored => {}
                    StreamingKeyOutcome::Changed => changed = true,
                    StreamingKeyOutcome::Interrupt => control = Some(TurnControl::Interrupted),
                    StreamingKeyOutcome::Quit => {
                        control = Some(TurnControl::Quit);
                    }
                    StreamingKeyOutcome::SwitchModel(reference) => {
                        app.pending_model_switch = Some(reference);
                        changed = true;
                    }
                    StreamingKeyOutcome::SwitchAccount(account) => {
                        app.pending_account_switch = Some(account);
                        changed = true;
                    }
                    StreamingKeyOutcome::ResumeSession(session_id) => {
                        app.pending_session_resume = Some(session_id);
                        control = Some(TurnControl::Interrupted);
                    }
                }
            }
            Event::Mouse(mouse) => {
                changed |= handle_transcript_mouse(mouse, app);
            }
            Event::Paste(text) => {
                app.paste_text(&text);
                changed = true;
            }
            Event::Resize(_, _) => {
                resize_tui_viewport(surface.terminal, surface.viewport_height, app.owned_screen)?;
                changed = true;
            }
            _ => {}
        }
    }
    copy_pending_selection(app).await;
    Ok((changed, control))
}

fn apply_stream_delta(app: &mut TuiApp, entry_index: usize, saw_delta: &mut bool, delta: &str) {
    if !*saw_delta {
        app.replace_entry(entry_index, "");
        *saw_delta = true;
    }
    app.append_entry(entry_index, delta);
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

fn apply_turn_event(
    app: &mut TuiApp,
    live_text_index: &mut usize,
    text_after_tool: &mut bool,
    thinking_index: usize,
    saw_delta: &mut bool,
    activity: &mut Activity,
    event: &TurnEvent,
) {
    match event {
        TurnEvent::RoundStarted => app.round_saw_text = false,
        TurnEvent::Provider(StreamEvent::Text(text)) => {
            *activity = Activity::Writing;
            app.round_saw_text = true;
            app.live_thinking_index = None;
            // Text that follows a tool round starts a new assistant entry at
            // the bottom of the transcript instead of growing the first one
            // above the tool calls, so the newest text stays in view while
            // earlier rounds scroll up past it.
            if *text_after_tool {
                let index = app.push_placeholder(TuiEntryKind::Assistant, "");
                *live_text_index = index;
                *text_after_tool = false;
                *saw_delta = true;
            }
            apply_stream_delta(app, *live_text_index, saw_delta, text);
        }
        TurnEvent::Provider(StreamEvent::Thinking(text)) => {
            *activity = Activity::Thinking;
            app.live_thinking_index = Some(thinking_index);
            app.append_entry(thinking_index, text);
        }
        TurnEvent::ToolStarted { call } => {
            *activity = Activity::Tool(call.name.clone());
            app.live_thinking_index = None;
            app.turn_saw_tools = true;
            // A reply that starts with a tool call writes nothing into the
            // turn's placeholder: take it away, or it stays above the calls
            // once the turn is over.
            if !*saw_delta {
                app.remove_live_entry();
                *saw_delta = true;
            }
            // Close the current text entry: the next delta opens a fresh one
            // below this tool call.
            *text_after_tool = true;
            app.live_tool_indices.push(app.entries.len());
            app.push(TuiEntryKind::Tool, format!("running {}\n\n", call.name));
        }
        // A running command's output so far. The collapsed preview shows
        // its last line; ctrl+o shows all of it.
        TurnEvent::ToolUpdate { call, output } => {
            if let Some(index) = app.live_tool_indices.last().copied() {
                app.replace_entry(
                    index,
                    format!(
                        "running {}\n{}\n{output}",
                        call.name,
                        tool_call_detail(call)
                    ),
                );
            }
        }
        TurnEvent::ToolOutput {
            call,
            output,
            is_error,
            ..
        } => {
            if let Some(index) = app.live_tool_indices.last().copied() {
                app.replace_entry(
                    index,
                    format!(
                        "{} {}\n{}\n{}",
                        if *is_error { "failed" } else { "completed" },
                        call.name,
                        tool_call_detail(call),
                        output
                    ),
                );
            }
        }
        TurnEvent::RetryStarted(notice) => {
            *activity = Activity::Waiting;
            let delay = match notice.delay_ms {
                delay if delay >= 1000 => format!("{}s", delay / 1000),
                delay => format!("{delay}ms"),
            };
            app.push(
                TuiEntryKind::System,
                format!(
                    "retrying in {delay} (attempt {} of {}): {}",
                    notice.attempt, notice.max_attempts, notice.error_message
                ),
            );
            *text_after_tool |= std::mem::take(&mut app.round_saw_text);
        }
        TurnEvent::Overflowed { error } => {
            app.push(
                TuiEntryKind::System,
                match error {
                    Some(error) => format!("context overflow; compacting to ask again: {error}"),
                    None => "reply cut off at the output limit; compacting to ask again".into(),
                },
            );
            app.overflow_compaction = true;
            *text_after_tool |= std::mem::take(&mut app.round_saw_text);
        }
        TurnEvent::CompactionStarted => *activity = Activity::Compacting,
        TurnEvent::CompactionFailed(reason) => {
            *activity = Activity::Waiting;
            let notice = if std::mem::take(&mut app.overflow_compaction) {
                format!("compaction failed, not asking again: {reason}")
            } else {
                format!("auto-compaction skipped: {reason}; continuing with full history")
            };
            app.push(TuiEntryKind::System, notice);
        }
        TurnEvent::CompactionFinished(record) => {
            app.overflow_compaction = false;
            *activity = Activity::Waiting;
            app.push(
                TuiEntryKind::System,
                format!(
                    "auto-compacted: omitted {} message(s)",
                    record.omitted_messages
                ),
            );
        }
        TurnEvent::TodosUpdated(todos) => app.todos = todos.clone(),
        TurnEvent::ToolFinished { .. } => {
            *activity = Activity::Waiting;
        }
        _ => {}
    }
}

fn handle_tui_selector_key(
    key: KeyEvent,
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            // In a two-layer picker, esc leaves the model layer back to the
            // provider layer instead of closing the selector (Codex-style).
            let pop_parent = app
                .selector
                .as_ref()
                .map(|selector| selector.parent.is_some())
                .unwrap_or(false);
            if pop_parent {
                let mut selector = app.selector.take().expect("selector");
                let parent = *selector.parent.take().expect("parent selector");
                app.selector = Some(parent);
            } else {
                app.selector = None;
            }
        }
        KeyCode::Up => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(-1);
            }
        }
        KeyCode::Down => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(1);
            }
        }
        KeyCode::PageUp => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(-(SELECTOR_PAGE_STEP as isize));
            }
        }
        KeyCode::PageDown => {
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(SELECTOR_PAGE_STEP as isize);
            }
        }
        KeyCode::Home => {
            if let Some(selector) = app.selector.as_mut() {
                selector.jump_to_start();
            }
        }
        KeyCode::End => {
            if let Some(selector) = app.selector.as_mut() {
                selector.jump_to_end();
            }
        }
        KeyCode::Left => {
            if let Some(selector) = app.selector.as_mut() {
                selector.cycle_thinking(-1);
            }
        }
        KeyCode::Right => {
            if let Some(selector) = app.selector.as_mut() {
                selector.cycle_thinking(1);
            }
        }
        KeyCode::Backspace => {
            if let Some(selector) = app.selector.as_mut() {
                selector.pop_query_char();
            }
        }
        KeyCode::Enter => {
            let is_settings = app
                .selector
                .as_ref()
                .map(|selector| selector.kind == "settings")
                .unwrap_or(false);
            let is_provider = app
                .selector
                .as_ref()
                .map(|selector| selector.kind == "provider")
                .unwrap_or(false);
            if is_provider {
                // First layer of the model picker: drill into the selected
                // provider's models, keeping this selector as the parent so
                // esc returns here.
                let selector = app.selector.take().expect("provider selector");
                let provider = selector
                    .selected_item()
                    .map(|item| item.value.clone())
                    .unwrap_or_default();
                let models: Vec<SelectorItem> = picker_models(config)
                    .into_iter()
                    .filter(|model| model.provider == provider)
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
                    .collect();
                if models.is_empty() {
                    app.push(TuiEntryKind::System, format!("no models for {provider}"));
                    return Ok(());
                }
                let mut model_selector = TuiSelectorState::new(
                    "model",
                    Selector::new(format!("model · {provider}"), models),
                    "",
                    None,
                )
                .with_model_thinking(config, runtime);
                model_selector.provider_filter = Some(provider);
                let mut model_state = model_selector;
                model_state.parent = Some(Box::new(selector));
                app.selector = Some(model_state);
            } else if is_settings {
                apply_tui_settings_selection(app, runtime, config)?;
            } else if let Some(selector) = app.selector.take() {
                apply_tui_selector_selection(app, runtime, config, selector)?;
            }
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(selector) = app.selector.as_mut() {
                selector.push_query_char(ch);
            }
        }
        _ => {}
    }
    Ok(())
}

fn open_tui_selector(
    app: &mut TuiApp,
    config: &LoadedConfig,
    runtime: &Runtime,
    kind: &str,
    query: &str,
) -> Result<()> {
    let selector = selector_for_kind(config, runtime, kind)?;
    if selector.items.is_empty() {
        app.push(TuiEntryKind::System, format!("no {kind}"));
        return Ok(());
    }
    let state =
        TuiSelectorState::new(kind, selector, query, None).with_model_thinking(config, runtime);
    app.push(TuiEntryKind::System, format!("{} selector", state.title));
    app.selector = Some(state);
    Ok(())
}

fn apply_tui_selector_selection(
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    selector: TuiSelectorState,
) -> Result<()> {
    let Some(item) = selector.selected_item().cloned() else {
        app.push(TuiEntryKind::System, "no selector match");
        return Ok(());
    };
    match selector.kind.as_str() {
        "model" | "models" | "scoped-models" => {
            let model = selector_model(config, &item.value)
                .ok_or_else(|| anyhow!("model not found: {}", item.value))?;
            // A level adjusted in the picker is the one to apply.
            switch_model(runtime, config, &model, selector.thinking_level.as_deref())?;
            persist_default_model(config, &model)?;
            app.push(
                TuiEntryKind::System,
                format_model_selection(&model, thinking_label(runtime, config).as_deref()),
            );
        }
        "theme" | "themes" => {
            let name = persist_theme(config, &item.value)?;
            app.push(TuiEntryKind::System, format!("theme: {name}"));
        }
        "session" | "sessions" | "resume" | "tree" => {
            let path = resolve_session_reference(&config.paths.session_dir, &item.value)?;
            let (store, state) = SessionStore::open(path)?;
            runtime.replace_session(state, Some(store));
            app.resume_pending = runtime.session().has_unfinished_turn();
            if let Some(hint) = resume_hint(runtime) {
                app.push(TuiEntryKind::System, hint);
            }
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        "auth" | "login" => {
            app.push(
                TuiEntryKind::System,
                format_login_status(config, &item.value),
            );
        }
        "account" => {
            let account = if item.value.is_empty() {
                None
            } else {
                Some(item.value.clone())
            };
            runtime.set_active_account(account)?;
            app.push(
                TuiEntryKind::System,
                format!(
                    "account: {}",
                    if item.value.is_empty() {
                        "auto"
                    } else {
                        item.value.as_str()
                    }
                ),
            );
        }
        "logout" => {
            if remove_stored_auth(config, &item.value, DEFAULT_ACCOUNT_NAME)? {
                app.push(
                    TuiEntryKind::System,
                    format!("removed stored auth for {}", item.value),
                );
            } else {
                app.push(
                    TuiEntryKind::System,
                    format!("no stored auth for {}", item.value),
                );
            }
        }
        _ => app.push(
            TuiEntryKind::System,
            format!("selected {}: {}", selector.title, item.value),
        ),
    }
    Ok(())
}

fn apply_tui_settings_selection(
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
) -> Result<()> {
    let Some(selector) = app.selector.take() else {
        return Ok(());
    };
    let selected = selector.selected;
    let query = selector.query.clone();
    let Some(item) = selector.selected_item().cloned() else {
        app.selector = Some(selector);
        app.push(TuiEntryKind::System, "no selector match");
        return Ok(());
    };
    let message = apply_settings_item(config, runtime, &item.value)?;
    let mut next = TuiSelectorState::new(
        "settings",
        selector_for_kind(config, runtime, "settings")?,
        query,
        None,
    );
    if !next.filtered_indices.is_empty() {
        next.selected = selected.min(next.filtered_indices.len() - 1);
    }
    app.selector = Some(next);
    app.push(TuiEntryKind::System, message);
    Ok(())
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

async fn handle_tui_submission(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    offline: bool,
    line: String,
) -> Result<bool> {
    if let Some(lines) = app.multiline.as_mut() {
        if line == "." {
            let prompt = TypedPrompt::expand(config, lines.join("\n"));
            app.multiline = None;
            let quit =
                submit_tui_prompt(app, surface, runtime, config, prompt, Vec::new(), offline).await;
            return Ok(quit);
        }
        lines.push(line);
        return Ok(false);
    }
    if matches!(line.as_str(), "/quit" | "/exit" | "/q") {
        for diagnostic in
            notify_extension_lifecycle(&config.extensions, "shutdown", &runtime.session().cwd)
        {
            app.push(TuiEntryKind::Error, diagnostic);
        }
        // The v2 hosts get their shutdown notification, then stop.
        {
            let hosts = runtime.extension_hosts();
            let mut hosts = hosts.lock().await;
            for diagnostic in hosts.notify("shutdown", &runtime.session().cwd).await {
                app.push(TuiEntryKind::Error, diagnostic);
            }
            hosts.shutdown(&runtime.session().cwd).await;
        }
        return Ok(true);
    }
    if handle_tui_bang(app, surface, runtime, config, &line).await? {
        return Ok(false);
    }
    match line.as_str() {
        "/help" => app.push(TuiEntryKind::System, terminal_renderer(config).help()),
        "/clear" => {
            app.clear_visible();
            clear_terminal_screen(surface.terminal, app.owned_screen)?;
        }
        "/image-models" => app.push(TuiEntryKind::System, format_image_models(config, "")),
        "/thinking" => app.push(
            TuiEntryKind::System,
            format!(
                "thinking: {}",
                thinking_label(runtime, config).unwrap_or_else(|| "-".to_string())
            ),
        ),
        "/model" | "/models" | "/scoped-models" => {
            // Codex-style two-layer picker: provider first, then its models.
            open_tui_selector(app, config, runtime, "provider", "")?
        }
        "/account" => open_tui_selector(app, config, runtime, "account", "")?,
        "/todos" => {
            app.push(
                TuiEntryKind::System,
                format_todo_list(&runtime.session().todos),
            );
        }
        "/diff" => {
            if app.diff_panel.is_some() {
                app.diff_panel = None;
            } else {
                app.diff_panel = Some(DiffPanelState {
                    files: collect_diff_file_entries(
                        &runtime.session().cwd,
                        &runtime.session().edited_files,
                    ),
                    selected: 0,
                    detail: None,
                });
            }
        }
        "/accounts" | "/usage" => {
            if line == "/usage" {
                app.push(TuiEntryKind::System, format_session_usage(runtime));
            }
            if offline || runtime.systems().offline {
                app.push(
                    TuiEntryKind::System,
                    "account quotas unavailable in offline mode",
                );
                return Ok(false);
            }
            let checking = app.push_placeholder(
                TuiEntryKind::System,
                "checking account status...".to_string(),
            );
            redraw_tui(surface.terminal, app, config)?;
            let now = unix_seconds().unwrap_or(0) as i64;
            let rows = usage::collect_account_status(config, false, None, None, now).await;
            app.replace_entry(checking, usage::format_account_status(&rows, now));
        }
        "/session" => app.push(TuiEntryKind::System, format_session(runtime)),
        "/changelog" => app.push(TuiEntryKind::System, format_changelog()),
        "/settings" => open_tui_selector(app, config, runtime, "settings", "")?,
        "/settings show" => app.push(TuiEntryKind::System, format_settings(config, runtime)),
        "/status" => app.push(
            TuiEntryKind::System,
            format_status(config, runtime, &app.editor_state),
        ),
        "/diagnostics" => app.push(TuiEntryKind::System, format_diagnostics(config)),
        "/hotkeys" => app.push(TuiEntryKind::System, format_hotkeys(config)),
        "/history" => app.push(TuiEntryKind::System, format_history(&app.editor_state)),
        "/skills" => app.push(
            TuiEntryKind::System,
            resource_commands::format_skills(config),
        ),
        "/prompts" => app.push(
            TuiEntryKind::System,
            resource_commands::format_prompts(config),
        ),
        "/themes" => app.push(TuiEntryKind::System, format_themes(config)),
        "/extensions" => app.push(
            TuiEntryKind::System,
            format_resources("extensions", &config.extensions),
        ),
        "/queue" => app.push(TuiEntryKind::System, format_queue(runtime)),
        "/queue-clear" => {
            let cleared = runtime.clear_queued_messages()?;
            app.push(
                TuiEntryKind::System,
                format!("cleared {cleared} queued message(s)"),
            );
        }
        "/interrupt" => {
            let cleared = runtime.clear_queued_messages()?;
            app.push(
                TuiEntryKind::System,
                format!("interrupted; cleared {cleared} queued message(s)"),
            );
        }
        "/tree" => app.push(
            TuiEntryKind::System,
            format_session_tree(
                &config.paths.session_dir,
                Some(&runtime.session().session_id),
            )?,
        ),
        "/summaries" => app.push(TuiEntryKind::System, format_summaries(runtime)),
        "/undo" => undo_tui_turn(app, runtime)?,
        _ if line.split_whitespace().next() == Some("/compact") => {
            let focus = line.strip_prefix("/compact").unwrap_or_default().trim();
            if compact_tui_context(app, surface, runtime, config, focus, offline).await? {
                return Ok(true);
            }
        }
        "/copy" => {
            if app.transcript_view.borrow().has_selection() {
                app.transcript_view.get_mut().request_copy();
                copy_pending_selection(app).await;
            } else {
                app.push(
                    TuiEntryKind::System,
                    copy_last_assistant_message(runtime).await?,
                );
            }
        }
        "/theme" => open_tui_selector(app, config, runtime, "theme", "")?,
        "/multiline" => {
            app.multiline = Some(Vec::new());
            app.push(
                TuiEntryKind::System,
                "enter multiline prompt; submit . on its own line",
            );
        }
        "/reload" => {
            // v1 one-shot extensions get their reload notification; the v2
            // hosts get it too, then are replaced by the next generation's
            // (a removed extension's host is stopped, a new one starts with
            // its next run or command).
            let mut lifecycle_diagnostics =
                notify_extension_lifecycle(&config.extensions, "reload", &runtime.session().cwd);
            {
                let hosts = runtime.extension_hosts();
                let mut hosts = hosts.lock().await;
                lifecycle_diagnostics.extend(hosts.notify("reload", &runtime.session().cwd).await);
            }
            *config = reload_config(config)?;
            start_model_refresh(config, offline, false);
            let next_generation = runtime.systems().config_generation + 1;
            let report = runtime.reload(ReloadableSystems::from_config(config, next_generation))?;
            let mut output = format_diagnostics(config);
            for diagnostic in lifecycle_diagnostics {
                output.push_str(&format!("\n{diagnostic}"));
            }
            if !report.active_model_valid {
                output
                    .push_str("\nactive model is no longer available; use /model <provider/model>");
            }
            if !report.active_account_valid {
                output.push_str("\nactive account is no longer available; use /account");
            }
            if !report.removed_active_tools.is_empty() {
                output.push_str(&format!(
                    "\nremoved active tools: {}",
                    report.removed_active_tools.join(", ")
                ));
            }
            output.push_str("\nreloaded");
            app.push(TuiEntryKind::System, output);
        }
        "/trust" => {
            let cwd = &runtime.session().cwd;
            let trust_path = save_project_trust(&config.paths.agent_dir, cwd, true)?;
            app.push(
                TuiEntryKind::System,
                format!(
                    "saved trust decision: trusted {} ({})",
                    cwd.display(),
                    trust_path.display()
                ),
            );
        }
        _ if line.starts_with("/complete ") => {
            let prefix = line.trim_start_matches("/complete ").trim();
            let completions = command_completions(config, prefix);
            app.push(
                TuiEntryKind::System,
                if completions.is_empty() {
                    "no completions".to_string()
                } else {
                    completions.join("\n")
                },
            );
        }
        _ if line.starts_with("/editor") => {
            let initial = line.trim_start_matches("/editor").trim();
            match read_external_editor_prompt(initial) {
                Ok(prompt) if !prompt.trim().is_empty() => {
                    let prompt = TypedPrompt::expand(config, prompt);
                    return Ok(submit_tui_prompt(
                        app,
                        surface,
                        runtime,
                        config,
                        prompt,
                        Vec::new(),
                        offline,
                    )
                    .await);
                }
                Ok(_) => app.push(TuiEntryKind::System, "editor returned an empty prompt"),
                Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
            }
        }
        "/image" => app.push(TuiEntryKind::System, "usage: /image <path> [prompt]"),
        _ if line.starts_with("/image ") => {
            let rest = line.trim_start_matches("/image ").trim();
            let (path, prompt) = split_once_text(rest);
            let media_result = if images_blocked(config) {
                Err(anyhow!("images are blocked by settings"))
            } else {
                load_media_input(&runtime.session().cwd, Path::new(path), config)
            };
            match media_result {
                Ok(media) => {
                    app.push(TuiEntryKind::System, format_media_fallback(&media));
                    if !prompt.is_empty() {
                        return Ok(submit_tui_prompt(
                            app,
                            surface,
                            runtime,
                            config,
                            prompt.to_string(),
                            vec![media],
                            offline,
                        )
                        .await);
                    }
                    app.input_media.push(media);
                }
                Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
            }
        }
        _ if line.starts_with("/image-models ") => {
            let search = line.trim_start_matches("/image-models ").trim();
            app.push(TuiEntryKind::System, format_image_models(config, search));
        }
        _ if line.starts_with("/models ") => {
            let arg = line.trim_start_matches("/models ").trim();
            match arg {
                "refresh" => {
                    if offline {
                        app.push(
                            TuiEntryKind::Error,
                            "model refresh is disabled in offline mode",
                        );
                    } else if !model_refresh_enabled(&config.settings) {
                        app.push(
                            TuiEntryKind::Error,
                            "model refresh is disabled by modelRefresh.enabled",
                        );
                    } else {
                        let report =
                            refresh_model_cache(config.paths.clone(), config.auth.clone()).await?;
                        *config = reload_config(config)?;
                        let next_generation = runtime.systems().config_generation + 1;
                        let reload_report = runtime
                            .reload(ReloadableSystems::from_config(config, next_generation))?;
                        app.push(
                            TuiEntryKind::System,
                            format_model_refresh_after_reload(&report, config, &reload_report),
                        );
                    }
                }
                "reset" => {
                    let message = reset_model_cache(&config.paths.model_cache_path)?;
                    *config = reload_config(config)?;
                    let next_generation = runtime.systems().config_generation + 1;
                    let reload_report =
                        runtime.reload(ReloadableSystems::from_config(config, next_generation))?;
                    let mut output = message;
                    if !reload_report.active_model_valid {
                        output.push_str(
                            "\nactive model is no longer available; use /model <provider/model>",
                        );
                    }
                    output.push_str(&format!(
                        "\n{} models available; run /models refresh to repopulate",
                        config.models.len()
                    ));
                    app.push(TuiEntryKind::System, output);
                }
                "help" | "usage" => app.push(
                    TuiEntryKind::System,
                    "usage: /models [refresh|reset|<query>]",
                ),
                query => open_tui_selector(app, config, runtime, "model", query)?,
            }
        }
        _ if line.starts_with("/scoped-models ") => {
            let query = line.trim_start_matches("/scoped-models ").trim();
            open_tui_selector(app, config, runtime, "model", query)?;
        }
        _ if line.starts_with("/generate-image ") => {
            let rest = line.trim_start_matches("/generate-image ").trim();
            let (path, prompt) = split_once_text(rest);
            if path.is_empty() || prompt.is_empty() {
                app.push(
                    TuiEntryKind::Error,
                    "usage: /generate-image <output> <prompt>",
                );
            } else {
                match generate_image_to_path(
                    config,
                    &runtime.session().cwd,
                    "openrouter/google/gemini-3.1-flash-image-preview",
                    Path::new(path),
                    prompt,
                    &[],
                )
                .await
                {
                    Ok(output) => app.push(TuiEntryKind::System, output),
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
            }
        }
        _ if line.starts_with("/queue ") => {
            let message = expand_input(config, line.trim_start_matches("/queue ").trim());
            runtime.queue_message(message)?;
            app.push(
                TuiEntryKind::System,
                format!("queued: {}", runtime.session().queued_messages.len()),
            );
        }
        _ if line.starts_with("/selector ") => {
            let kind = line.trim_start_matches("/selector ").trim();
            open_tui_selector(app, config, runtime, kind, "")?;
        }
        _ if line.starts_with("/select ") => {
            let (kind, _) = split_once_text(line.trim_start_matches("/select ").trim());
            let resuming = matches!(kind, "session" | "sessions" | "resume" | "tree");
            app.push(
                TuiEntryKind::System,
                select_from_selector_message(config, runtime, &line)?,
            );
            if resuming {
                app.resume_pending = runtime.session().has_unfinished_turn();
                if let Some(hint) = resume_hint(runtime) {
                    app.push(TuiEntryKind::System, hint);
                }
            }
        }
        _ if line.starts_with("/model ") => {
            // As upstream: an exact reference switches, and anything else
            // opens the picker with the text as its search.
            let reference = line.trim_start_matches("/model ").trim();
            match resolve_model_reference(config, reference, offline) {
                Some(model) => {
                    switch_model(runtime, config, &model, None)?;
                    persist_default_model(config, &model)?;
                    app.push(
                        TuiEntryKind::System,
                        format_model_selection(&model, thinking_label(runtime, config).as_deref()),
                    );
                }
                None => open_tui_selector(app, config, runtime, "model", reference)?,
            }
        }
        _ if line.starts_with("/thinking ") => {
            // As upstream: one of the levels the model supports, by name.
            let requested = line.trim_start_matches("/thinking ").trim();
            let available = runtime
                .session()
                .active_model
                .as_ref()
                .and_then(|model| model_definition(config, model))
                .map(|model| model.supported_thinking_levels())
                .unwrap_or_else(|| noel_config::THINKING_LEVELS.to_vec());
            let level = normalized_thinking_level(requested)
                .filter(|level| available.contains(level))
                .ok_or_else(|| {
                    anyhow!(
                        "Unknown thinking level \"{requested}\". Available levels: {}.",
                        available.join(", ")
                    )
                })?;
            set_thinking_level(runtime, config, level)?;
            app.push(TuiEntryKind::System, format!("thinking: {level}"));
        }
        _ if line.starts_with("/extension:") => {
            let (name, input) = split_resource_command(&line, "/extension:");
            let extension = find_resource(&config.extensions, name)
                .ok_or_else(|| anyhow!("extension not found: {name}"))?;
            if is_executable_extension(&extension.path) {
                // A v2 extension runs its `/extension:` command on the
                // persistent host, so its state carries between commands
                // and tools; v1 keeps its one-shot run.
                if noel_core::extensions::manifest_protocol_version(&extension.path) == Some(2) {
                    runtime.start_extension_hosts().await;
                    let hosts = runtime.extension_hosts();
                    let mut hosts = hosts.lock().await;
                    let outcome = hosts
                        .call(
                            extension.name.as_str(),
                            "command",
                            extension.name.as_str(),
                            input,
                            &runtime.session().cwd.clone(),
                            None,
                        )
                        .await;
                    match outcome {
                        Ok(outcome) => {
                            if let Some(set) = outcome.registrations {
                                drop(hosts);
                                runtime.apply_extension_registrations(set).await;
                            } else {
                                drop(hosts);
                            }
                            let kind = if outcome.is_error {
                                TuiEntryKind::Error
                            } else {
                                TuiEntryKind::System
                            };
                            app.push(kind, outcome.output);
                        }
                        Err(error) => {
                            drop(hosts);
                            app.push(TuiEntryKind::Error, error);
                        }
                    }
                    return Ok(false);
                }
                match run_executable_extension(extension, input, &runtime.session().cwd) {
                    Ok(output) => app.push(TuiEntryKind::System, output),
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
                return Ok(false);
            }
            let prompt = if input.is_empty() {
                extension.content.clone()
            } else {
                format!("{}\n\n{}", extension.content, input)
            };
            return Ok(submit_tui_prompt(
                app,
                surface,
                runtime,
                config,
                prompt,
                Vec::new(),
                offline,
            )
            .await);
        }
        "/prompt" => app.push(TuiEntryKind::System, "usage: /prompt <name> [input]"),
        _ if line.starts_with("/prompt ") => {
            let rest = line.trim_start_matches("/prompt ").trim();
            let (name, input) = split_once_text(rest);
            let template = resource_commands::find_template(config, name)
                .ok_or_else(|| anyhow!("prompt template not found: {name}"))?;
            let prompt = resource_commands::fill_prompt_template(&template.content, input);
            return Ok(submit_tui_prompt(
                app,
                surface,
                runtime,
                config,
                prompt,
                Vec::new(),
                offline,
            )
            .await);
        }
        "/accent" => app.push(
            TuiEntryKind::System,
            format!(
                "accent: {}",
                config.settings.accent_color.as_deref().unwrap_or("auto")
            ),
        ),
        _ if line.starts_with("/accent ") => {
            let color = line.trim_start_matches("/accent ").trim();
            persist_accent(config, color)?;
            app.push(TuiEntryKind::System, format!("accent: {color}"));
        }
        _ if line.starts_with("/theme ") => {
            let name = line.trim_start_matches("/theme ").trim();
            let name = persist_theme(config, name)?;
            app.push(TuiEntryKind::System, format!("theme: {name}"));
        }
        _ if line.starts_with("/new") => {
            start_new_session(runtime, config)?;
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line.starts_with("/resume") => {
            let reference = line.trim_start_matches("/resume").trim();
            if reference.is_empty() {
                // Kimi/Claude-style picker: interactive selector with topic,
                // age, message count, model badge, and unfinished marker.
                open_tui_selector(app, config, runtime, "session", "")?;
            } else {
                let path = resolve_session_reference(&config.paths.session_dir, reference)?;
                let (store, state) = SessionStore::open(path)?;
                runtime.replace_session(state, Some(store));
                app.resume_pending = runtime.session().has_unfinished_turn();
                if let Some(hint) = resume_hint(runtime) {
                    app.push(TuiEntryKind::System, hint);
                }
                app.push(TuiEntryKind::System, format_session(runtime));
            }
        }
        _ if line.starts_with("/fork") => {
            let source =
                resolve_source_session(&config.paths.session_dir, runtime, &line, "/fork")?;
            let (store, state) = SessionStore::fork(&config.paths.session_dir, &source, false)?;
            runtime.replace_session(state, Some(store));
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line.starts_with("/clone") => {
            let source =
                resolve_source_session(&config.paths.session_dir, runtime, &line, "/clone")?;
            let (store, state) = SessionStore::fork(&config.paths.session_dir, &source, true)?;
            runtime.replace_session(state, Some(store));
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line.starts_with("/delete") => {
            app.push(
                TuiEntryKind::System,
                delete_session_message(config, runtime, &line)?,
            );
        }
        _ if line.starts_with("/name") => {
            let name = line.trim_start_matches("/name").trim();
            if name.is_empty() {
                // Upstream: a bare /name shows the current name; it never
                // clears it.
                app.push(
                    TuiEntryKind::System,
                    match &runtime.session().name {
                        Some(name) => format!("session name: {name}"),
                        None => "usage: /name <name>".to_string(),
                    },
                );
            } else {
                runtime.rename_session(Some(name.to_string()))?;
                app.push(TuiEntryKind::System, format_session(runtime));
            }
        }
        _ if line.starts_with("/labels") => {
            let labels = line
                .trim_start_matches("/labels")
                .split_whitespace()
                .map(ToString::to_string)
                .collect();
            runtime.set_labels(labels)?;
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line == "/export" || line.starts_with("/export ") => {
            let requested = line.trim_start_matches("/export").trim();
            let path = if requested.is_empty() {
                // Upstream's default: pi-session-<session>.html in the cwd.
                runtime.session().cwd.join(format!(
                    "noel-session-{}.html",
                    runtime.session().session_id
                ))
            } else {
                PathBuf::from(requested)
            };
            export_session(runtime, &path)?;
            app.push(TuiEntryKind::System, format!("exported {}", path.display()));
        }
        "/import" => app.push(TuiEntryKind::System, "usage: /import <path.jsonl>"),
        _ if line.starts_with("/import ") => {
            let path = PathBuf::from(line.trim_start_matches("/import ").trim());
            let (store, state) = SessionStore::import_path(&config.paths.session_dir, &path)?;
            runtime.replace_session(state, Some(store));
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line.starts_with("/account ") => {
            let account = line.trim_start_matches("/account ").trim();
            runtime.set_active_account(if account.is_empty() {
                None
            } else {
                Some(account.to_string())
            })?;
            app.push(
                TuiEntryKind::System,
                format!(
                    "account: {}",
                    if account.is_empty() { "auto" } else { account }
                ),
            );
        }
        _ if line.starts_with("/login") => {
            let provider = line.trim_start_matches("/login").trim();
            if provider.is_empty() {
                open_tui_selector(app, config, runtime, "login", "")?;
            } else {
                app.push(TuiEntryKind::System, format_login_status(config, provider));
            }
        }
        _ if line.starts_with("/logout") => {
            let args = line.trim_start_matches("/logout").trim();
            if args.is_empty() {
                open_tui_selector(app, config, runtime, "logout", "")?;
            } else {
                let mut parts = args.split_whitespace();
                let provider = parts.next().unwrap_or_default();
                let account = parts.next().unwrap_or(DEFAULT_ACCOUNT_NAME);
                if remove_stored_auth(config, provider, account)? {
                    app.push(
                        TuiEntryKind::System,
                        format!(
                            "removed stored auth for {provider}{}",
                            account_suffix(account)
                        ),
                    );
                } else {
                    app.push(
                        TuiEntryKind::System,
                        format!("no stored auth for {provider}{}", account_suffix(account)),
                    );
                }
            }
        }
        _ if line.split_whitespace().next() == Some("/bug") => {
            bug_report::run_tui_command(app, runtime, config, &line)
        }
        _ if line.starts_with("/share") => {
            let requested = line.trim_start_matches("/share").trim();
            let path = if requested.is_empty() {
                config
                    .paths
                    .session_dir
                    .join(format!("{}.html", runtime.session().session_id))
            } else {
                PathBuf::from(requested)
            };
            export_session(runtime, &path)?;
            app.push(
                TuiEntryKind::System,
                format!("share exported {}", path.display()),
            );
        }
        // An extension-registered command: `/<name> [args]` runs the v2
        // host's registered command (upstream `registerCommand`). Built-ins
        // and resources above win, as upstream's own commands do.
        _ if line.starts_with('/') && !line.starts_with("//") => {
            let (name, input) = split_resource_command(&line, "/");
            let (name, input) = (name.to_string(), input.to_string());
            let registered = {
                let hosts = runtime.extension_hosts();
                let hosts = hosts.lock().await;
                hosts
                    .registration_sets()
                    .iter()
                    .any(|set| set.commands.iter().any(|command| command.name == name))
            };
            if registered {
                let extension = {
                    let hosts = runtime.extension_hosts();
                    let hosts = hosts.lock().await;
                    hosts
                        .registration_sets()
                        .iter()
                        .find(|set| set.commands.iter().any(|command| command.name == name))
                        .map(|set| set.extension_name.clone())
                };
                if let Some(extension) = extension {
                    let cwd = runtime.session().cwd.clone();
                    let hosts = runtime.extension_hosts();
                    let mut hosts = hosts.lock().await;
                    let outcome = hosts
                        .call(&extension, "command", &name, &input, &cwd, None)
                        .await;
                    drop(hosts);
                    match outcome {
                        Ok(outcome) => {
                            if let Some(set) = outcome.registrations {
                                runtime.apply_extension_registrations(set).await;
                            }
                            let kind = if outcome.is_error {
                                TuiEntryKind::Error
                            } else {
                                TuiEntryKind::System
                            };
                            app.push(kind, outcome.output);
                        }
                        Err(error) => app.push(TuiEntryKind::Error, error),
                    }
                    return Ok(false);
                }
            }
            let prompt = TypedPrompt::expand(config, line);
            return Ok(submit_tui_prompt(
                app,
                surface,
                runtime,
                config,
                prompt,
                Vec::new(),
                offline,
            )
            .await);
        }
        // Anything else is a prompt: a skill command or a prompt template
        // expands, and other text goes to the model as typed.
        _ => {
            let prompt = TypedPrompt::expand(config, line);
            return Ok(submit_tui_prompt(
                app,
                surface,
                runtime,
                config,
                prompt,
                Vec::new(),
                offline,
            )
            .await);
        }
    }
    Ok(false)
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

async fn handle_tui_bang(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    line: &str,
) -> Result<bool> {
    if line == "!!" && app.last_shell_command.is_none() {
        app.push(TuiEntryKind::System, "no previous shell command");
        return Ok(true);
    }
    if line == "!" {
        app.push(TuiEntryKind::System, "usage: ! <command>");
        return Ok(true);
    }
    let Some(command) = resolve_bang_command(line, &app.last_shell_command) else {
        return Ok(false);
    };
    app.previews_expanded = false;
    let entry_index = if terminal_progress_enabled(config) {
        let index = app.push_placeholder(TuiEntryKind::Tool, format_bash_running(&command));
        redraw_tui(surface.terminal, app, config)?;
        Some(index)
    } else {
        None
    };
    match run_excluded_bash_result(runtime, command.clone()).await {
        Ok(result) => {
            let output = result.output;
            if let Some(index) = entry_index {
                let text = format_bash_completed(&command, &output);
                app.replace_entry(
                    index,
                    if result.is_error {
                        text.replacen("completed", "failed", 1)
                    } else {
                        text
                    },
                );
                app.finish_live_entry();
            } else {
                app.push(TuiEntryKind::Tool, output);
            }
            app.last_shell_command = Some(command);
        }
        Err(error) => {
            if let Some(index) = entry_index {
                app.replace_entry(index, format!("failed bash\n$ {command}\n{error}"));
                app.finish_live_entry();
            } else {
                app.push(TuiEntryKind::Error, format!("{error}"));
            }
        }
    }
    Ok(true)
}

/// Swap to the session picked mid-turn. The turn that was running has been
/// interrupted and torn down by the time this runs; replace the session,
/// reset the transcript, and surface the same resume hint the idle picker
/// produces.
fn apply_midturn_session_resume(
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    session_id: &str,
) -> Result<()> {
    let path = resolve_session_reference(&config.paths.session_dir, session_id)?;
    let (store, state) = SessionStore::open(path)?;
    runtime.replace_session(state, Some(store));
    app.clear_visible();
    app.resume_pending = runtime.session().has_unfinished_turn();
    if let Some(hint) = resume_hint(runtime) {
        app.push(TuiEntryKind::System, hint);
    }
    app.push(TuiEntryKind::System, format_session(runtime));
    app.refresh_chrome(config, runtime);
    Ok(())
}

/// A prompt to submit, and the input that stands for it in history.
struct TypedPrompt {
    typed: String,
    prompt: String,
}

impl TypedPrompt {
    /// Typed input: a skill command or a prompt template expands.
    fn expand(config: &LoadedConfig, typed: String) -> Self {
        let prompt = expand_input(config, &typed);
        Self { typed, prompt }
    }
}

/// A prompt a command has built, which is sent as it is.
impl From<String> for TypedPrompt {
    fn from(prompt: String) -> Self {
        Self {
            typed: prompt.clone(),
            prompt,
        }
    }
}

async fn submit_tui_prompt(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: impl Into<TypedPrompt>,
    media: Vec<MediaInput>,
    offline: bool,
) -> bool {
    let TypedPrompt { typed, prompt } = prompt.into();
    if images_blocked(config) && (!media.is_empty() || !app.input_media.is_empty()) {
        app.push(
            TuiEntryKind::Error,
            "images are blocked by settings; clear the image draft before submitting",
        );
        return false;
    }
    let mut media = media;
    media.append(&mut app.input_media);
    for attachment in &media {
        app.push(TuiEntryKind::System, format_media_fallback(attachment));
    }
    // A template shows as the prompt it became. A skill's prompt is its
    // whole file, so its command stands for it.
    let shown = if typed.starts_with("/skill:") {
        typed.clone()
    } else {
        prompt.clone()
    };
    app.editor_state.record_history(typed);
    app.push(TuiEntryKind::User, shown);
    let control = match run_prompt_with_queue_tui(
        app, surface, runtime, config, prompt, media, offline,
    )
    .await
    {
        Ok(control) => control,
        Err(error) => {
            app.drop_live_entry();
            app.push(
                TuiEntryKind::Error,
                format_tui_error(&error, runtime, config),
            );
            TurnControl::Completed
        }
    };
    // A mid-turn /resume selection swaps the session once the turn is torn
    // down; submit_tui_prompt runs where the runtime is free again.
    if let Some(session_id) = app.pending_session_resume.take() {
        if let Err(error) = apply_midturn_session_resume(app, runtime, config, &session_id) {
            app.push(TuiEntryKind::Error, format!("{error:#}"));
        }
    }
    app.refresh_diff_panel(runtime);
    matches!(control, TurnControl::Quit)
}

/// How a submitted turn ended: normally, interrupted by the user, or with a
/// quit request that the interactive loop should honor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnControl {
    Completed,
    Interrupted,
    Quit,
}

fn format_tui_error(error: &anyhow::Error, runtime: &Runtime, config: &LoadedConfig) -> String {
    let message = error.to_string();
    // Transport-level failures reach the UI as "error sending request for
    // url …" after retries are exhausted. Name the likely causes so the
    // recovery path is discoverable: a local firewall/VPN prompt (denied
    // connections look identical to outages), or genuine network loss.
    let message = if message.contains("error sending request") {
        format!(
            "{message}\n\nconnection failed after retries: check network, \
VPN/firewall approval for this binary (an unapproved prompt denies by default \
on this host), provider status, then retry; /model switches providers"
        )
    } else {
        message
    };
    if !message.contains("429 Too Many Requests") {
        return message;
    }
    let model = runtime
        .session()
        .active_model
        .as_ref()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .unwrap_or_else(|| "-".to_string());
    let thinking = thinking_label(runtime, config).unwrap_or_else(|| "-".to_string());
    format!(
        "{message}\n\nAnthropic rate-limited this request for model {model} with thinking {thinking}. Try /thinking high, /thinking off, or /model anthropic/claude-sonnet-4-6. Claude Code may still work elsewhere if it is using a different model, thinking level, or quota pool."
    )
}

async fn compact_tui_context(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    focus: &str,
    offline: bool,
) -> Result<bool> {
    let provider = provider_for_runtime(runtime, config, offline).await?;
    let started = Instant::now();
    let followups = FollowUpQueue::default();
    for message in runtime.session().queued_messages.clone() {
        followups.push(message);
    }
    let steering = steering_mailbox(config);
    let queue_store = runtime.store().cloned();
    let mut saved_queue = followups.list();
    app.turn_running = true;
    app.turn_saw_tools = false;
    let mut quit = false;
    let result = {
        let compact = runtime.compact_messages(provider.as_ref(), CompactionKind::Manual, focus);
        tokio::pin!(compact);
        let mut tick = tokio::time::interval(Duration::from_millis(STREAM_RENDER_INTERVAL_MS));
        loop {
            tokio::select! {
                result = &mut compact => break Ok(Some(result)),
                _ = tick.tick() => {
                    app.status = activity_status(started, &Activity::Compacting, followups.pending());
                    if let Err(error) = redraw_tui(surface.terminal, app, config) { break Err(error); }
                    let control = match drain_streaming_tui_events(surface, app, &followups, &steering, config).await {
                        Ok((_, control)) => control,
                        Err(error) => break Err(error),
                    };
                    let pending = steering.list().into_iter().chain(followups.list()).collect::<Vec<_>>();
                    if pending != saved_queue {
                        if let Some(store) = &queue_store {
                            if let Err(error) = store.record_queued_messages_snapshot(pending.clone()) { break Err(error.into()); }
                        }
                        saved_queue = pending;
                    }
                    if let Some(control) = control {
                        quit = matches!(control, TurnControl::Quit);
                        break Ok(None);
                    }
                }
            }
        }
    };
    app.turn_running = false;
    runtime.replace_queued_messages(
        steering
            .drain()
            .into_iter()
            .chain(followups.drain())
            .collect(),
    )?;
    app.queued_messages = runtime.session().queued_messages.clone();
    app.status = footer_status(config, runtime, &app.editor_state);
    match result? {
        Some(Ok(record)) => app.push(
            TuiEntryKind::System,
            format!(
                "compacted: omitted {} message(s), retained {} message(s)",
                record.omitted_messages, record.retained_messages
            ),
        ),
        Some(Err(error)) => app.push(TuiEntryKind::Error, error.to_string()),
        None => app.push(
            TuiEntryKind::System,
            "compaction interrupted; original history retained",
        ),
    }
    redraw_tui(surface.terminal, app, config)?;
    Ok(quit)
}

async fn run_prompt_with_queue_tui(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<TurnControl> {
    match run_prompt_once_tui(app, surface, runtime, config, prompt, media, offline).await? {
        TurnControl::Completed => {}
        control => return Ok(control),
    }
    drain_followups_tui(app, surface, runtime, config, offline).await
}

async fn resume_turn_with_queue_tui(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    offline: bool,
) -> Result<TurnControl> {
    match run_turn_tui(app, surface, runtime, config, None, offline).await? {
        TurnControl::Completed => drain_followups_tui(app, surface, runtime, config, offline).await,
        control => Ok(control),
    }
}

async fn drain_followups_tui(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    offline: bool,
) -> Result<TurnControl> {
    while !runtime.session().queued_messages.is_empty() {
        // Tool-command entries (for example `/read path`) are positional and
        // dispatch one at a time; prose follow-ups batch into a single
        // combined turn so a queued burst reads as one instruction.
        let queue: Vec<String> = runtime.session().queued_messages.to_vec();
        let batch: Vec<String> = if response_kind_for_prompt(&queue[0]) == TuiEntryKind::Tool {
            vec![queue[0].clone()]
        } else {
            queue
                .iter()
                .take_while(|message| response_kind_for_prompt(message) != TuiEntryKind::Tool)
                .cloned()
                .collect()
        };
        let prompt = batch.join(STEERING_JOIN);
        let remaining = queue[batch.len()..].to_vec();
        runtime.replace_queued_messages(remaining)?;
        for queued in &batch {
            app.push(TuiEntryKind::User, format!("queued> {queued}"));
        }
        match run_prompt_once_tui(app, surface, runtime, config, prompt, Vec::new(), offline)
            .await?
        {
            TurnControl::Completed => {}
            control => return Ok(control),
        }
        // Batching describes delivery, not a cap on the chain. Each completed
        // follow-up turn may have queued more work, drained the same way.
    }
    Ok(TurnControl::Completed)
}

fn steering_mailbox(_config: &LoadedConfig) -> SteeringMailbox {
    SteeringMailbox::new()
}

async fn run_prompt_once_tui(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<TurnControl> {
    run_turn_tui(
        app,
        surface,
        runtime,
        config,
        Some((prompt, media)),
        offline,
    )
    .await
}

/// None continues existing context without a synthetic user message.
async fn run_turn_tui(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    submission: Option<(String, Vec<MediaInput>)>,
    offline: bool,
) -> Result<TurnControl> {
    let kind = submission
        .as_ref()
        .map(|(prompt, _)| response_kind_for_prompt(prompt))
        .unwrap_or(TuiEntryKind::Assistant);
    let progress_enabled = terminal_progress_enabled(config);
    let thinking_index = app.entries.len();
    app.turn_running = true;
    app.turn_saw_tools = false;
    if kind != TuiEntryKind::Tool {
        app.live_thinking_index = Some(thinking_index);
        app.entries.push(TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: String::new(),
        });
    }
    let entry_index = app.push_placeholder(
        kind.clone(),
        if kind == TuiEntryKind::Tool && progress_enabled {
            format_tool_running(&submission.as_ref().expect("tool prompt").0)
        } else if kind == TuiEntryKind::Tool {
            "running...".to_string()
        } else {
            "Working...".to_string()
        },
    );
    redraw_tui(surface.terminal, app, config)?;
    let provider = provider_for_runtime(runtime, config, offline).await?;
    let tool_start = runtime.session().tool_history.len();
    let steering = steering_mailbox(config);
    if kind == TuiEntryKind::Tool {
        let (prompt, media) = submission.expect("tool prompt");
        let tool_prompt = prompt.clone();
        let response = run_user_turn_streaming_with_media(
            runtime,
            provider.as_ref(),
            prompt,
            media,
            &steering,
            |_| {},
        )
        .await?;
        if progress_enabled {
            let text = format_tool_completed(&tool_prompt, &response);
            let text = if runtime
                .session()
                .tool_history
                .last()
                .is_some_and(|event| event.is_error)
            {
                text.replacen("completed", "failed", 1)
            } else {
                text
            };
            app.replace_entry(entry_index, text);
        } else {
            app.replace_entry(entry_index, response);
        }
        app.finish_live_entry();
        redraw_tui(surface.terminal, app, config)?;
        return Ok(TurnControl::Completed);
    }

    let mut saw_delta = false;
    // Index of the entry currently receiving text deltas; advances as tool
    // rounds split the response into interleaved assistant entries.
    let mut live_text_index = entry_index;
    let mut text_after_tool = false;
    let followups = FollowUpQueue::default();
    for message in runtime.session().queued_messages.clone() {
        followups.push(message);
    }
    let queue_store = runtime.store().cloned();
    let mut saved_queue = followups.list();
    let activity_started = Instant::now();
    let mut activity = Activity::Waiting;
    let (delta_tx, delta_rx) = std::sync::mpsc::channel::<TurnEvent>();
    // An interrupt cancels the turn and waits for it to stop, which takes
    // no longer than its next await: it saves what it produced so far (an
    // aborted reply, a tool call's output) and ends.
    let run = RunControl {
        steering: steering.clone(),
        ..RunControl::default()
    };
    let cancellation = run.cancellation.clone();
    let stream_end = {
        let turn = async {
            let on_event = move |event: &TurnEvent| {
                let _ = delta_tx.send(event.clone());
            };
            match submission {
                Some((prompt, media)) => {
                    run_user_turn_streaming_events_with_media(
                        runtime,
                        provider.as_ref(),
                        prompt,
                        media,
                        &run,
                        on_event,
                    )
                    .await
                }
                None => {
                    continue_turn_streaming_events(runtime, provider.as_ref(), &run, on_event).await
                }
            }
        };
        tokio::pin!(turn);
        let mut tick = tokio::time::interval(Duration::from_millis(STREAM_RENDER_INTERVAL_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                result = &mut turn => {
                    while let Ok(event) = delta_rx.try_recv() {
                        apply_turn_event(app, &mut live_text_index, &mut text_after_tool, thinking_index, &mut saw_delta, &mut activity, &event);
                    }
                    // Consume input already pending at completion while it
                    // still belongs to this turn (Enter queues, Ctrl+S steers).
                    let (_, control) =
                        drain_streaming_tui_events(surface, app, &followups, &steering, config).await?;
                    app.queued_messages = followups.list();
                    app.status = activity_status(activity_started, &activity, followups.pending());
                    redraw_tui(surface.terminal, app, config)?;
                    break match control {
                        Some(TurnControl::Quit) => StreamEnd::Quit,
                        Some(TurnControl::Interrupted) => StreamEnd::Interrupted,
                        _ => StreamEnd::Finished(result),
                    };
                }
                _ = tick.tick() => {
                    while let Ok(event) = delta_rx.try_recv() {
                        apply_turn_event(app, &mut live_text_index, &mut text_after_tool, thinking_index, &mut saw_delta, &mut activity, &event);
                    }
                    let (_, control) =
                        drain_streaming_tui_events(surface, app, &followups, &steering, config).await?;
                    let pending = steering.list().into_iter().chain(followups.list()).collect::<Vec<_>>();
                    if pending != saved_queue {
                        if let Some(store) = &queue_store { store.record_queued_messages_snapshot(pending.clone())?; }
                        saved_queue = pending;
                    }
                    if let Some(control) = control {
                        cancellation.cancel();
                        let _ = (&mut turn).await;
                        while let Ok(event) = delta_rx.try_recv() {
                            apply_turn_event(app, &mut live_text_index, &mut text_after_tool, thinking_index, &mut saw_delta, &mut activity, &event);
                        }
                        break match control {
                            TurnControl::Quit => StreamEnd::Quit,
                            _ => StreamEnd::Interrupted,
                        };
                    }
                    app.queued_messages = followups.list();
                    app.status = activity_status(activity_started, &activity, followups.pending());
                    redraw_tui(surface.terminal, app, config)?;
                }
            }
        }
    };
    app.turn_running = false;
    app.live_thinking_index = None;
    settle_tool_entries(app, runtime, tool_start, entry_index);
    app.status = footer_status(config, runtime, &app.editor_state);
    // Steering input that arrived after the last injection point becomes a
    // regular follow-up rather than being dropped, ahead of newer follow-ups.
    runtime.replace_queued_messages(
        steering
            .drain()
            .into_iter()
            .chain(followups.drain())
            .collect(),
    )?;

    app.refresh_chrome(config, runtime);
    // Failover notices (account switches and cooldowns) are surfaced as
    // system entries rather than injected into the assistant stream.
    for notice in account_failover::drain_notices() {
        app.push(TuiEntryKind::System, notice);
    }
    // Interrupt and quit cancel the turn, which cancels the provider
    // request; whatever already streamed stays visible, and is saved as an
    // aborted reply.
    let turn_result = match stream_end {
        StreamEnd::Finished(result) => result,
        StreamEnd::Interrupted | StreamEnd::Quit => {
            if saw_delta {
                app.finish_live_entry();
            } else {
                app.drop_live_entry();
            }
            let control = if matches!(stream_end, StreamEnd::Interrupted) {
                app.push(TuiEntryKind::System, "interrupted");
                TurnControl::Interrupted
            } else {
                TurnControl::Quit
            };
            redraw_tui(surface.terminal, app, config)?;
            return Ok(control);
        }
    };

    // On a mid-turn failure (e.g. exceeding the tool-call turn limit, a network
    // drop, or a rate limit), keep whatever was already streamed instead of
    // discarding the live entry. The error itself is surfaced as a separate
    // entry by the caller.
    let response = match turn_result {
        Ok(response) => response,
        Err(error) => {
            if saw_delta {
                app.finish_live_entry();
            } else {
                app.drop_live_entry();
            }
            redraw_tui(surface.terminal, app, config)?;
            return Err(error.into());
        }
    };

    if !saw_delta {
        app.replace_entry(live_text_index, response);
    }
    app.finish_live_entry();
    // A mid-turn /model <ref> applies now, before the next turn starts.
    // Persistence as default happens on the next explicit /model; here we
    // only switch the session's active model.
    if let Some(reference) = app.pending_model_switch.take() {
        // A picker row names a configured model; a typed reference one of
        // the selectable models.
        let model = selector_model(config, &reference)
            .or_else(|| resolve_model_reference(config, &reference, false));
        if let Some(model) = model {
            switch_model(runtime, config, &model, None)?;
            app.push(
                TuiEntryKind::System,
                format_model_selection(&model, thinking_label(runtime, config).as_deref()),
            );
        }
    }
    // A mid-turn /account selection applies now, before the next turn:
    // `pending_account_switch` holds `Some(None)` for a queued switch to
    // auto, and `None` when no switch is queued.
    if let Some(account) = app.pending_account_switch.take() {
        runtime.set_active_account(account)?;
        app.refresh_chrome(config, runtime);
        app.push(
            TuiEntryKind::System,
            format!(
                "account: {}",
                runtime
                    .session()
                    .active_account
                    .as_deref()
                    .unwrap_or("auto")
            ),
        );
    }
    redraw_tui(surface.terminal, app, config)?;
    Ok(TurnControl::Completed)
}

/// Finalize live tool previews once the turn ends, however it ends. Previews
/// are already interleaved between the per-round assistant text entries, so
/// each is replaced in place with its durable transcript text instead of
/// being re-inserted above the response (which would bury the newest text
/// under the tool history). Previews without a durable record (a tool dropped
/// by an interrupt) are removed; durable records without a preview fall back
/// to insertion above the first text entry. This is the only place a turn's
/// tool entries are settled: inserting them again would show each twice.
fn settle_tool_entries(
    app: &mut TuiApp,
    runtime: &Runtime,
    tool_start: usize,
    assistant_entry_index: usize,
) {
    let durable = durable_tool_texts(runtime, tool_start);
    let live = std::mem::take(&mut app.live_tool_indices);
    let pairs = live.len().min(durable.len());
    for (index, text) in live[..pairs].iter().zip(durable[..pairs].iter()) {
        app.replace_entry(*index, text.clone());
    }
    for index in live[pairs..].iter().rev() {
        app.remove_entry(*index);
    }
    for (offset, text) in durable[pairs..].iter().enumerate() {
        app.insert_entry(
            assistant_entry_index + offset,
            TuiEntryKind::Tool,
            text.clone(),
        );
    }
}

fn durable_tool_texts(runtime: &Runtime, tool_start: usize) -> Vec<String> {
    runtime
        .session()
        .tool_history
        .iter()
        .skip(tool_start)
        .map(|event| {
            if let Some(message) = runtime.session().messages.iter().find(|message| {
                message.role == MessageRole::Tool
                    && message.tool_call_id.as_deref() == Some(event.id.as_str())
            }) {
                format_session_tool_message(message, runtime)
            } else {
                format!(
                    "{} {}\n\n{}",
                    if event.is_error {
                        "failed"
                    } else {
                        "completed"
                    },
                    event.name,
                    event.result
                )
            }
        })
        .collect()
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

fn format_session_tool_message(message: &ConversationMessage, runtime: &Runtime) -> String {
    let name = message.tool_name.as_deref().unwrap_or("tool");
    let call = runtime
        .session()
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .find(|call| Some(call.id.as_str()) == message.tool_call_id.as_deref());
    let detail = call.map(tool_call_detail).unwrap_or_default();
    let failed = runtime
        .session()
        .tool_history
        .iter()
        .find(|event| Some(event.id.as_str()) == message.tool_call_id.as_deref())
        .is_some_and(|event| event.is_error);
    format!(
        "{} {name}\n{detail}\n{}",
        if failed { "failed" } else { "completed" },
        message.content
    )
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

fn format_tool_running(prompt: &str) -> String {
    let (command, detail) = split_once_text(prompt.trim());
    if detail.is_empty() {
        return format!("running {command}");
    }
    format!(
        "running {}\n{detail}",
        command.trim_start_matches('/').replace('-', "_")
    )
}

fn format_tool_completed(prompt: &str, output: &str) -> String {
    let (command, detail) = split_once_text(prompt.trim());
    if detail.is_empty() {
        return format!(
            "completed {}\n\n{output}",
            command.trim_start_matches('/').replace('-', "_")
        );
    }
    format!(
        "completed {}\n{detail}\n{output}",
        command.trim_start_matches('/').replace('-', "_")
    )
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

fn undo_tui_turn(app: &mut TuiApp, runtime: &mut Runtime) -> Result<()> {
    if !app.input_media.is_empty() {
        app.push(TuiEntryKind::System, "clear the image draft before /undo");
        return Ok(());
    }
    match runtime.undo_last_turn()? {
        Some(prompt) => {
            let visible_prompt = app
                .entries
                .iter()
                .rposition(|entry| entry.kind == TuiEntryKind::User);
            if let Some(index) = visible_prompt.filter(|index| {
                app.entries[*index].text == prompt.content
                    || app.entries[*index].text == format!("queued> {}", prompt.content)
            }) {
                app.transcript_view.get_mut().reset();
                app.entries.truncate(index);
                app.transcript_scrollback.truncate(index);
            } else {
                // Steering messages and resumed sessions may not have a matching
                // visible user row. Rebuild from actual context, not an unrelated
                // earlier turn, while keeping diagnostics and command output.
                app.transcript_view.get_mut().reset();
                app.entries.retain(|entry| {
                    matches!(entry.kind, TuiEntryKind::System | TuiEntryKind::Error)
                });
                app.transcript_scrollback = TranscriptScrollback::default();
                app.restore_session_messages(runtime);
            }
            app.clear_input();
            app.multiline = None;
            app.live_entry_index = None;
            app.live_thinking_index = None;
            app.live_tool_indices.clear();
            app.insert_input_str(&prompt.content);
            app.input_media = prompt.media;
            app.push(
                TuiEntryKind::System,
                "undid last conversation turn; prompt restored (files unchanged)",
            );
        }
        None => app.push(TuiEntryKind::System, "nothing to undo"),
    }
    Ok(())
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

fn format_status(config: &LoadedConfig, runtime: &Runtime, editor_state: &EditorState) -> String {
    format!("status\t{}", compact_status(config, runtime, editor_state))
}

fn footer_status(config: &LoadedConfig, runtime: &Runtime, editor_state: &EditorState) -> String {
    compact_status(config, runtime, editor_state)
}

fn compact_status(config: &LoadedConfig, runtime: &Runtime, editor_state: &EditorState) -> String {
    let mut output = format!(
        "{} {} {} ≡ {} ↺ {}",
        runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| match &runtime.session().active_account {
                Some(account) => format!("{}/{}@{account}", model.provider, model.id),
                None => format!("{}/{}", model.provider, model.id),
            })
            .unwrap_or_else(|| "-".to_string()),
        thinking_label(runtime, config).unwrap_or_else(|| "-".to_string()),
        config
            .settings
            .theme
            .clone()
            .unwrap_or_else(|| "system".to_string()),
        runtime.session().queued_messages.len(),
        editor_state.history().len()
    );
    if !config.diagnostics.is_empty() {
        output.push_str(&format!(" d:{}", config.diagnostics.len()));
    }
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

/// Session picker rows, shared by the idle and mid-turn resume paths.
fn session_selector_items(config: &LoadedConfig) -> Result<Vec<SelectorItem>> {
    Ok(SessionStore::list(&config.paths.session_dir)?
        .into_iter()
        .rev()
        .map(|session| SelectorItem {
            label: format_session_picker_label(&session),
            value: session.session_id.clone(),
            active: false,
        })
        .collect())
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
                    label: if auth_for_provider(&config.auth, &provider, None).is_some() {
                        format!("{provider}: available")
                    } else {
                        format!("{provider}: missing")
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
        listed_accounts_for_provider(&config.auth, provider)
            .into_iter()
            .map(|(account, source)| SelectorItem {
                label: match source {
                    AccountSource::Stored => format!("{account}{pinned_note}"),
                    AccountSource::Environment => {
                        format!("{account} (environment, read-only)")
                    }
                    AccountSource::ImportedFile => {
                        format!("{account} (imported login file, read-only)")
                    }
                },
                active: active == Some(account.as_str()),
                value: account,
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
    if runtime.session().queued_messages.is_empty() {
        return "queue is empty".to_string();
    }
    runtime
        .session()
        .queued_messages
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

async fn copy_last_assistant_message(runtime: &Runtime) -> Result<String> {
    let Some(message) = runtime
        .session()
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
    else {
        return Ok("no assistant message".to_string());
    };
    let status = clipboard::copy_text(&message.content).await?;
    Ok(format!("{}\n{status}", message.content))
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
mod tests {
    use super::*;
    use noel_config::{PromptTemplate, Skill, SourceInfo, SourceScope};
    use std::io::BufRead;

    /// The tool settings a command line leaves, starting from `settings`.
    fn tool_flags(arguments: &[&str], settings: serde_json::Value) -> Result<Settings> {
        let arguments = std::iter::once("noel").chain(arguments.iter().copied());
        let cli = Cli::try_parse_from(normalized_cli_args(arguments.map(String::from)))?;
        let mut settings: Settings = serde_json::from_value(settings)?;
        apply_tool_flags(&cli, &mut settings)?;
        Ok(settings)
    }

    #[test]
    fn tool_flags_select_tools_as_upstream_does() {
        let names =
            |names: &[&str]| Some(names.iter().map(ToString::to_string).collect::<Vec<_>>());
        let none = serde_json::json!({});

        // Plain names and patterns are an allowlist.
        let settings = tool_flags(&["--tools", "read, grep,mcp_*"], none.clone()).unwrap();
        assert_eq!(settings.enabled_tools, names(&["read", "grep", "mcp_*"]));
        assert_eq!(settings.default_tools, None);

        // Only +name/-name entries change the default selection instead.
        let settings = tool_flags(&["-t", "-bash,+grep,-write"], none.clone()).unwrap();
        assert_eq!(settings.enabled_tools, None);
        let defaults = settings.default_tools.expect("default selection");
        assert!(defaults.contains(&"grep".to_string()) && defaults.contains(&"read".to_string()));
        assert!(
            !defaults.contains(&"bash".to_string()) && !defaults.contains(&"write".to_string())
        );
        let settings = tool_flags(
            &["--tools", "+ls"],
            serde_json::json!({ "defaultTools": ["read", "-read", "grep"] }),
        )
        .unwrap();
        assert_eq!(settings.default_tools, names(&["grep", "ls"]));

        // --no-builtin-tools empties the default selection; extension tools
        // are not part of it, so they stay.
        let settings =
            tool_flags(&["-nbt"], serde_json::json!({ "defaultTools": ["read"] })).unwrap();
        assert_eq!(
            (settings.default_tools, settings.enabled_tools),
            (names(&[]), None)
        );
        let settings = tool_flags(&["-nbt"], serde_json::json!({ "enabledTools": ["read"] }));
        assert_eq!(settings.unwrap().enabled_tools, None);
        let settings =
            tool_flags(&["--no-builtin-tools", "--tools", "+read"], none.clone()).unwrap();
        assert_eq!(
            (settings.default_tools, settings.enabled_tools),
            (names(&["read"]), None)
        );

        // --no-tools leaves only what --tools names.
        let settings = tool_flags(&["--no-tools"], none.clone()).unwrap();
        assert_eq!(settings.enabled_tools, names(&[]));
        let settings = tool_flags(&["-nt", "--tools", "+read"], none.clone()).unwrap();
        assert_eq!(settings.enabled_tools, names(&["read"]));
        let settings = tool_flags(&["--no-tools", "--tools", "read,ext_*"], none.clone()).unwrap();
        assert_eq!(settings.enabled_tools, names(&["read", "ext_*"]));

        let settings = tool_flags(&["-xt", "web_*,todo"], none.clone()).unwrap();
        assert_eq!(settings.excluded_tools, ["web_*", "todo"]);

        for (tools, error) in [
            (
                "read,+grep",
                "--tools: tool names cannot be mixed with +name or -name entries",
            ),
            (
                "+re*",
                "--tools: +name and -name entries take exact tool names, not patterns: +re*",
            ),
        ] {
            let failure = tool_flags(&["--tools", tools], none.clone()).unwrap_err();
            assert_eq!(failure.to_string(), error);
        }
    }

    #[test]
    fn diff_panel_lists_files_under_cwd_relative_to_it() {
        let entries = collect_diff_file_entries(
            Path::new("/no-such-dir/work/cwd"),
            &[
                "/no-such-dir/work/cwd/src/file.txt".to_string(),
                "/elsewhere/notes.md".to_string(),
                "already/relative.rs".to_string(),
            ],
        );
        let paths = entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec!["src/file.txt", "/elsewhere/notes.md", "already/relative.rs"]
        );
    }

    fn picker_summary(first_prompt: Option<&str>, unfinished: bool) -> noel_core::SessionSummary {
        noel_core::SessionSummary {
            path: PathBuf::from("/tmp/ignored.jsonl"),
            session_id: "1791668953663-3417381-0".into(),
            cwd: PathBuf::from("/repo"),
            name: None,
            labels: Default::default(),
            parent_session_id: None,
            branch_summary: None,
            modified: Some(std::time::SystemTime::now()),
            message_count: 12,
            first_prompt: first_prompt.map(str::to_string),
            active_model: Some("zai-coding/glm-5.3".into()),
            has_unfinished_turn: unfinished,
        }
    }

    #[test]
    fn assistant_markdown_renders_styled_spans_but_plain_logical_lines() {
        let entry = TuiEntry {
            kind: TuiEntryKind::Assistant,
            text: "run `cargo build` then **commit**\nsee [docs](https://example.org)".to_string(),
        };
        let palette = ThemePalette::default();
        // Logical lines (selection/copy source) stay plain and complete,
        // including the markdown syntax and link URL.
        let logical = render_entry_lines(std::slice::from_ref(&entry), &palette);
        let logical_text: Vec<String> = logical
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(
            logical_text,
            vec![
                format!("{BULLET} run `cargo build` then **commit**"),
                "  see [docs](https://example.org)".to_string(),
                "".to_string()
            ]
        );
        // Display rows get styles after wrapping; anchors unchanged.
        let mut rows = pending_entry_rows(
            &entry,
            0,
            80,
            &palette,
            &PreviewState::default(),
            EntryScrollback::default(),
        );
        let anchor_starts: Vec<TranscriptPosition> = rows.iter().map(|row| row.start).collect();
        restyle_rows_with_markdown(
            &mut rows,
            &entry,
            |line| logical_text.get(line).cloned(),
            Style::default().fg(palette.foreground),
            palette.accent,
        );
        // Anchors identical after restyling.
        assert_eq!(
            anchor_starts,
            rows.iter().map(|row| row.start).collect::<Vec<_>>()
        );
        let display: Vec<String> = rows
            .iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Bold span carries the modifier; link row displays only "docs".
        let bold = rows[0].line.spans.iter().any(|span| {
            span.content.contains("commit")
                && !span.content.contains("**")
                && span.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(bold, "bold marker not styled: {display:?}");
        let code = rows[0].line.spans.iter().any(|span| {
            span.content.contains("cargo build") && span.style.fg == Some(palette.accent)
        });
        assert!(code, "code span not accented: {display:?}");
        assert_eq!(display[1], "  see docs");
        let link = rows[1]
            .line
            .spans
            .iter()
            .find(|span| span.content == "docs")
            .expect("link span");
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn ordered_lists_display_sequential_numbers() {
        // A source that numbers every item 1. (or restarts numbers) still
        // displays 1. 2. 3., matching upstream's list renderer; a plain
        // line ends the sequence and a later list starts over at its own
        // first marker.
        let entry = TuiEntry {
            kind: TuiEntryKind::Assistant,
            text:
                "intro:\n1. first\n1. second\n3. third skipped number\n\nplain\n\n5. five\n6. six"
                    .to_string(),
        };
        let palette = ThemePalette::default();
        let logical = render_entry_lines(std::slice::from_ref(&entry), &palette);
        let logical_text: Vec<String> = logical
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let mut rows = pending_entry_rows(
            &entry,
            0,
            80,
            &palette,
            &PreviewState::default(),
            EntryScrollback::default(),
        );
        restyle_rows_with_markdown(
            &mut rows,
            &entry,
            |line| logical_text.get(line).cloned(),
            Style::default().fg(palette.foreground),
            palette.accent,
        );
        let display: Vec<String> = rows
            .iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let item_text = |needle: &str| {
            display
                .iter()
                .find(|line| line.contains(needle))
                .cloned()
                .unwrap_or_else(|| panic!("missing {needle}: {display:?}"))
        };
        // The first logical line carries the "● " echo prefix, so the
        // list starts on the second line.
        assert_eq!(item_text("intro"), "● intro:");
        assert_eq!(item_text("first"), "  1. first");
        assert_eq!(item_text("second"), "  2. second");
        assert_eq!(
            item_text("third skipped number"),
            "  3. third skipped number"
        );
        assert_eq!(item_text("plain"), "  plain");
        // The blank line ended the list, so this one restarts at its own 5.
        assert_eq!(item_text("five"), "  5. five");
        assert_eq!(item_text("six"), "  6. six");
    }

    #[test]
    fn session_tree_renders_branches_with_connectors_and_marks_the_current() {
        let root = std::env::temp_dir().join(format!("noel-cli-tree-{}", unique_temp_suffix()));
        use serde_json::json;
        fs::create_dir_all(&root).unwrap();
        let session_dir = root.join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let write = |id: &str, prompt: &str, parent: Option<&str>| {
            let path = session_dir.join(format!("{id}.jsonl"));
            let mut records =
                vec![json!({"type": "started", "session_id": id, "cwd": root.join("cwd")})];
            if let Some(parent) = parent {
                records.push(json!({"type": "metadata", "name": null, "labels": [], "parent_session_id": parent}));
            }
            records.push(json!({"type": "message", "message": {"role": "user", "content": prompt, "media": [], "tool_call_id": null, "tool_name": null, "tool_calls": [], "aborted": false, "thinking": ""}}));
            let mut text = String::new();
            for record in records {
                text.push_str(&record.to_string());
                text.push('\n');
            }
            fs::write(path, text).unwrap();
        };
        write("parent", "fix the login flow", None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write("fork-a", "try oauth instead", Some("parent"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        write("fork-b", "try device code", Some("parent"));
        let tree = format_session_tree(&session_dir, Some("fork-a")).unwrap();
        let parent_row = tree
            .lines()
            .find(|line| line.contains("fix the login flow"))
            .unwrap();
        let fork_a_row = tree
            .lines()
            .find(|line| line.contains("try oauth instead"))
            .unwrap();
        let fork_b_row = tree
            .lines()
            .find(|line| line.contains("try device code"))
            .unwrap();
        // The parent leads with no bullet; both forks are its children with
        // connectors, the last one a corner.
        assert!(fork_a_row.contains("• "), "current session marked: {tree}");
        assert!(fork_b_row.contains("└─ "), "last child corners: {tree}");
        assert!(
            tree.lines().any(|line| line.contains("├─ ")),
            "a non-last child branches: {tree}"
        );
        assert!(
            parent_row.contains("(2 branches)"),
            "branch count noted: {tree}"
        );
        // Every session appears exactly once.
        assert_eq!(tree.lines().count(), 3, "{tree}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_picker_label_shows_topic_count_model_and_unfinished() {
        let label = format_session_picker_label(&picker_summary(
            Some("fix the retry loop\nmore details"),
            true,
        ));
        assert_eq!(
            label,
            "fix the retry loop · 0s · 12 msg · zai-coding/glm-5.3 · unfinished"
        );
        // Long topics truncate with an ellipsis, one line only.
        let long = "x".repeat(80);
        let label = format_session_picker_label(&picker_summary(Some(&long), false));
        assert!(label.starts_with(&format!("{}… · ", "x".repeat(60))));
        assert!(!label.contains('\n'))
    }

    #[test]
    fn session_picker_label_falls_back_to_session_id() {
        let label = format_session_picker_label(&picker_summary(None, false));
        assert_eq!(
            label,
            "1791668953663-3417381-0 · 0s · 12 msg · zai-coding/glm-5.3"
        );
    }

    #[test]
    fn relative_time_label_uses_compact_units() {
        let label = |secs: u64| {
            relative_time_label(std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
        };
        assert_eq!(label(5), "5s");
        assert_eq!(label(90), "1m");
        assert_eq!(label(7_200), "2h");
        assert_eq!(label(3 * 86_400), "3d");
    }

    fn clipboard_test_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::new_rgba8(width, height);
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn clipboard_images_validate_resize_and_honor_block_setting() {
        let mut config = minimal_test_config();
        let png = clipboard_test_png(2100, 10);
        let media = clipboard_media(png.clone(), &config).unwrap();
        assert_eq!(media.width, Some(2000));
        assert!(media.path.is_none());
        assert_eq!(media.mime_type, "image/png");
        assert!(!media.data_base64.is_empty());
        config.settings.images = Some(ImageSettings {
            auto_resize: Some(false),
            block_images: Some(false),
        });
        assert_eq!(
            clipboard_media(png.clone(), &config).unwrap().width,
            Some(2100)
        );
        config.settings.images.as_mut().unwrap().block_images = Some(true);
        assert!(clipboard_media(png, &config)
            .unwrap_err()
            .to_string()
            .contains("blocked"));
        assert!(clipboard_media(b"not an image".to_vec(), &minimal_test_config()).is_err());
    }

    #[test]
    fn image_draft_is_visible_cleared_and_never_discarded_by_stream_submission() {
        let config = minimal_test_config();
        let mut app = TuiApp::default();
        app.input_media
            .push(clipboard_media(clipboard_test_png(1, 1), &config).unwrap());
        app.set_input("describe");
        assert_eq!(input_desired_height(&app, 80), 4);
        let lines = render_input_lines(
            &app,
            Rect::new(0, 0, 76, 2),
            Style::default(),
            Style::default(),
        );
        assert!(lines
            .iter()
            .any(|line| line.to_string().contains("1 image attached")));
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(
                handle_streaming_tui_key(key, &mut app, &followups, &steering, &config),
                StreamingKeyOutcome::Changed
            ));
            assert_eq!(app.input, "describe");
            assert_eq!(app.input_media.len(), 1);
            assert_eq!(followups.pending(), 0);
            assert_eq!(steering.pending(), 0);
        }
        assert!(keybinding_map(&config).matches("paste-image", "ctrl+v"));
        app.clear_input();
        assert!(app.input_media.is_empty());
    }

    #[test]
    fn undo_restores_text_and_images_and_usage_has_no_invented_estimates() {
        let config = minimal_test_config();
        let mut runtime = account_test_runtime("faux", None);
        let media = clipboard_media(clipboard_test_png(1, 1), &config).unwrap();
        runtime
            .push_message(ConversationMessage {
                role: MessageRole::User,
                content: "image question".into(),
                thinking: String::new(),
                media: vec![media.clone()],
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                aborted: false,
            })
            .unwrap();
        let mut app = TuiApp::new(&config, &runtime);
        undo_tui_turn(&mut app, &mut runtime).unwrap();
        assert_eq!(app.input, "image question");
        assert_eq!(app.input_media, [media]);
        assert!(runtime.session().messages.is_empty());
        assert!(app
            .entries
            .iter()
            .any(|entry| entry.text.contains("files unchanged")));
        assert!(format_session_usage(&runtime).contains("no provider token usage reported"));
        runtime.session_mut().usage = noel_core::SessionUsage {
            input_tokens: 10,
            output_tokens: 5,
            requests: 1,
        };
        assert!(format_session_usage(&runtime).contains("15 total"));
    }

    #[test]
    fn undo_of_nonvisible_steering_prompt_keeps_the_earlier_turn_visible() {
        let config = minimal_test_config();
        let mut runtime = account_test_runtime("faux", None);
        let message = |content: &str| ConversationMessage {
            role: MessageRole::User,
            content: content.into(),
            thinking: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        };
        runtime.push_message(message("first prompt")).unwrap();
        let mut app = TuiApp::new(&config, &runtime);
        runtime.push_message(message("steering prompt")).unwrap();
        undo_tui_turn(&mut app, &mut runtime).unwrap();
        assert_eq!(app.input, "steering prompt");
        assert!(app
            .entries
            .iter()
            .any(|entry| entry.kind == TuiEntryKind::User && entry.text == "first prompt"));
        assert_eq!(runtime.session().messages.len(), 1);
    }

    #[test]
    fn auto_resize_downscales_large_png() {
        let original = png_bytes(3000, 1000);
        let resized = resize_image_if_needed(&original, "image/png")
            .expect("resize image")
            .expect("resized");

        assert_eq!(resized.mime_type, "image/png");
        let (width, height) = png_dimensions(&resized.bytes).expect("png dimensions");
        assert_eq!((width, height), (2000, 667));
        assert!(resized.bytes.len() < original.len());
    }

    #[test]
    fn auto_resize_leaves_small_png_unchanged() {
        let original = png_bytes(10, 10);
        let resized = resize_image_if_needed(&original, "image/png").expect("resize image");

        assert!(resized.is_none());
    }

    #[test]
    fn auto_resize_ignores_decode_failures() {
        let invalid_png_header = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x04\0\0\0";
        let resized =
            resize_image_if_needed(invalid_png_header, "image/png").expect("resize image");

        assert!(resized.is_none());
    }

    #[test]
    fn extension_protocol_rejects_unknown_manifest_protocol() {
        let root = std::env::temp_dir().join(format!(
            "noel-cli-extension-protocol-{}",
            unique_temp_suffix()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("bad-ext");
        fs::write(&path, "").expect("write extension");
        fs::write(
            root.join("bad-ext.noel-extension.json"),
            r#"{"protocol":"bogus"}"#,
        )
        .expect("write manifest");
        let extension = ResourceFile {
            name: "bad-ext".to_string(),
            path,
            content: String::new(),
        };

        let error = extension_protocol(&extension).expect_err("reject protocol");

        assert!(error.to_string().contains("unsupported protocol bogus"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn config_resource_state_enable_disable_updates_settings() {
        let root = std::env::temp_dir().join(format!(
            "noel-cli-config-resource-state-{}",
            unique_temp_suffix()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("settings.json");

        mutate_settings_resource_state(&path, "extensions", "assist", true)
            .expect("disable resource");
        mutate_settings_resource_state(&path, "extensions", "assist", false)
            .expect("enable resource");
        mutate_settings_resource_state(&path, "prompts", "fix", true).expect("disable prompt");

        let settings = serde_json::from_str::<serde_json::Value>(
            &fs::read_to_string(&path).expect("read settings"),
        )
        .expect("parse settings");

        assert_eq!(
            settings["disabledResources"]["extensions"],
            serde_json::json!([])
        );
        assert_eq!(
            settings["disabledResources"]["prompts"],
            serde_json::json!(["fix"])
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_completions_include_loaded_resources() {
        let config = LoadedConfig {
            paths: test_config_paths(),
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
            model_scope: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: vec![test_resource("json-ext")],
            skills: vec![test_skill("review", "Review code.\nCarefully.")],
            prompt_templates: vec![test_template(
                "fix",
                "Fix a bug",
                Some("<issue>"),
                "Fix $1.",
            )],
            themes: vec![test_resource("dark")],
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        };

        assert_eq!(
            command_completions(&config, "/extension:j"),
            ["/extension:json-ext"]
        );
        // A skill and a template complete with what upstream shows: the
        // argument hint, the scope, and the description on one line.
        let skill = "/skill:review  [u] Review code. Carefully.";
        assert_eq!(command_completions(&config, "/skill:r"), [skill]);
        assert_eq!(command_completions(&config, "/rev"), [skill]);
        assert_eq!(
            command_completions(&config, "/fix"),
            ["/fix  <issue> \u{2014} [u] Fix a bug"]
        );
        assert_eq!(command_completions(&config, "/prompt f"), ["/prompt fix"]);
        assert!(command_completions(&config, "/theme d").contains(&"/theme dark".to_string()));

        assert_eq!(
            resource_commands::format_skills(&config),
            "/skill:review\t[u] Review code. Carefully.\t/skills/review/SKILL.md"
        );
        assert_eq!(
            resource_commands::format_prompts(&config),
            "/fix\t<issue> \u{2014} [u] Fix a bug\t/prompts/fix.md"
        );
        let empty = minimal_test_config();
        assert_eq!(resource_commands::format_skills(&empty), "no skills");
        assert_eq!(resource_commands::format_prompts(&empty), "no prompts");
    }

    /// noel's own `/prompt <name> [input]`: `{{input}}` stands for the input,
    /// which otherwise follows the template.
    #[test]
    fn prompt_command_fills_the_input_placeholder() {
        let fill = resource_commands::fill_prompt_template;
        assert_eq!(fill("fix {{input}} now", "the bug"), "fix the bug now");
        assert_eq!(fill("fix {{input}}", ""), "fix ");
        assert_eq!(fill("review", "the diff"), "review\n\nthe diff");
        assert_eq!(fill("review $1", ""), "review $1");
    }

    /// Typed input expands before it is submitted: history keeps what was
    /// typed, and a skill's command stands for its file in the transcript.
    #[test]
    fn typed_prompts_expand_skills_and_templates() {
        let root = std::env::temp_dir().join(format!("noel-cli-typed-{}", unique_temp_suffix()));
        fs::create_dir_all(root.join("review")).unwrap();
        let skill_file = root.join("review/SKILL.md");
        fs::write(
            &skill_file,
            "---\ndescription: Review code.\n---\nCheck it.\n",
        )
        .unwrap();
        let mut config = minimal_test_config();
        config.skills = vec![Skill {
            file_path: skill_file.clone(),
            base_dir: root.join("review"),
            ..test_skill("review", "Review code.")
        }];
        config.prompt_templates = vec![test_template("fix", "Fix a bug", None, "Fix $1.")];

        let template = TypedPrompt::expand(&config, "/fix 42".into());
        assert_eq!(
            (template.typed.as_str(), template.prompt.as_str()),
            ("/fix 42", "Fix 42.")
        );
        let skill = TypedPrompt::expand(&config, "/skill:review the parser".into());
        assert_eq!(skill.typed, "/skill:review the parser");
        assert_eq!(
            skill.prompt,
            format!(
                "<skill name=\"review\" location=\"{}\">\nReferences are relative to {}.\n\nCheck it.\n</skill>\n\nthe parser",
                skill_file.display(),
                root.join("review").display()
            )
        );
        for text in ["/skill:nope x", "/nope x", "plain", "/read notes.txt"] {
            assert_eq!(TypedPrompt::expand(&config, text.into()).prompt, text);
        }
        // A prompt a command built is sent as it is.
        assert_eq!(TypedPrompt::from("/fix 42".to_string()).prompt, "/fix 42");
        let _ = fs::remove_dir_all(root);
    }

    /// Mid-turn, a skill command or a template queues expanded: steering
    /// with the steer key, a follow-up with Enter or `/queue`.
    #[test]
    fn streaming_input_expands_templates_into_the_queues() {
        let mut config = minimal_test_config();
        config.prompt_templates = vec![test_template("fix", "Fix a bug", None, "Fix $1.")];
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let mut app = TuiApp::default();
        let press = |app: &mut TuiApp, input: &str, key: KeyEvent| {
            app.set_input(input);
            handle_streaming_tui_key(key, app, &followups, &steering, &config)
        };
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let steer = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);

        press(&mut app, "/fix 1", steer);
        assert_eq!(steering.drain(), ["Fix 1."]);
        assert_eq!(app.entries[0].text, "steering> /fix 1");
        press(&mut app, "/fix 2", enter);
        press(&mut app, "/queue /fix 3", enter);
        assert_eq!(followups.list(), ["Fix 2.", "Fix 3."]);
        // A command that names no template is still refused mid-turn.
        press(&mut app, "/nope", enter);
        assert_eq!(followups.pending(), 2);
        assert_eq!(
            app.entries.last().unwrap().text,
            "/nope is not available while a turn is running"
        );
    }

    #[test]
    fn themes_are_available_without_resources_and_do_not_duplicate_builtins() {
        let mut config = account_test_config(AuthData::default());
        let runtime = account_test_runtime("faux", None);
        config.themes = vec![test_resource("dark"), test_resource("custom")];
        assert_eq!(
            theme_names(&config),
            ["system", "light", "dark", "kimi", "custom"]
        );
        let selector = selector_for_kind(&config, &runtime, "theme").unwrap();
        assert!(selector.items[0].active);
        assert!(command_completions(&config, "/theme k").contains(&"/theme kimi".to_string()));
        assert!(command_completions(&config, "/accent m").contains(&"/accent magenta".to_string()));
        assert!(format_themes(&config).contains("system\nlight\ndark\nkimi"));
    }

    #[test]
    fn theme_and_accent_persist_validate_and_reload_without_session_loss() {
        use ratatui::style::Color;
        let root = std::env::temp_dir().join(format!("noel-cli-theme-{}", unique_temp_suffix()));
        fs::create_dir_all(root.join("themes")).unwrap();
        let mut config = account_test_config(AuthData::default());
        config.paths = ConfigPaths {
            cwd: root.clone(),
            agent_dir: root.clone(),
            settings_path: root.join("settings.json"),
            project_settings_path: root.join(".noel/settings.json"),
            ..test_config_paths()
        };
        // Only the faux model is enabled, so building the runtime looks up no
        // provider credentials in the environment or in $HOME login files.
        fs::write(
            &config.paths.settings_path,
            r#"{"unknownSetting":123,"defaultProvider":"faux","defaultModel":"echo","enabledModels":["faux/echo"]}"#,
        )
        .unwrap();
        let theme_path = root.join("themes/custom.json");
        fs::write(
            &theme_path,
            r##"{"base":"dark","colors":{"accent":"#123456"}}"##,
        )
        .unwrap();
        config = load_config(config.paths).unwrap();
        let mut session = SessionState::new("keep-session", root.clone());
        session.active_model = Some(ModelRef {
            provider: "faux".into(),
            id: "echo".into(),
        });
        session.queued_messages.push("followup".into());
        session.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            content: "answer".into(),
            thinking: "summary".into(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        session.tool_history.push(noel_core::ToolEvent {
            id: "tool-1".into(),
            name: "read".into(),
            result: "contents".into(),
            is_error: false,
        });
        let mut runtime = Runtime::new(session, ReloadableSystems::from_config(&config, 1));
        let before = runtime.session().clone();
        let mut app = TuiApp::new(&config, &runtime);
        app.set_input("draft");
        let selector = TuiSelectorState::new(
            "theme",
            selector_for_kind(&config, &runtime, "theme").unwrap(),
            "custom",
            None,
        );
        apply_tui_selector_selection(&mut app, &mut runtime, &mut config, selector).unwrap();
        assert_eq!(config.settings.theme.as_deref(), Some("custom"));
        assert_eq!(
            terminal_theme(&config).unwrap().palette.accent,
            Color::Rgb(0x12, 0x34, 0x56)
        );
        persist_accent(&mut config, "magenta").unwrap();
        assert_eq!(
            terminal_theme(&config).unwrap().palette.accent,
            Color::Magenta
        );
        let saved = fs::read_to_string(&config.paths.settings_path).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap()["unknownSetting"],
            123
        );
        assert!(persist_theme(&mut config, "missing").is_err());
        assert!(persist_accent(&mut config, "bad-color").is_err());
        assert_eq!(
            fs::read_to_string(&config.paths.settings_path).unwrap(),
            saved
        );
        fs::write(
            &theme_path,
            r##"{"base":"light","colors":{"accent":"#654321"}}"##,
        )
        .unwrap();
        config = load_config(config.paths.clone()).unwrap();
        runtime
            .reload(ReloadableSystems::from_config(&config, 2))
            .unwrap();
        app.refresh_chrome(&config, &runtime);
        assert_eq!(runtime.session(), &before);
        assert_eq!(app.input, "draft");
        assert!(app.entries.iter().any(|entry| entry.text == "answer"));
        assert_eq!(
            terminal_theme(&config).unwrap().palette.background,
            TerminalTheme::builtin("light").unwrap().palette.background
        );
        assert_eq!(
            terminal_theme(&config).unwrap().palette.accent,
            Color::Magenta
        );
        persist_accent(&mut config, "auto").unwrap();
        assert_eq!(
            terminal_theme(&config).unwrap().palette.accent,
            Color::Rgb(0x65, 0x43, 0x21)
        );
        assert_eq!(
            select_from_selector_message(&mut config, &mut runtime, "/select theme kimi").unwrap(),
            "theme: kimi"
        );
        assert_eq!(persist_theme(&mut config, "default").unwrap(), "system");
        assert_eq!(
            terminal_theme(&load_config(config.paths.clone()).unwrap()).unwrap(),
            TerminalTheme::default()
        );
        // --theme accepts JSON paths as well as built-in and resource names.
        assert_eq!(
            resolve_terminal_theme(&config, "themes/custom.json")
                .unwrap()
                .palette
                .accent,
            Color::Rgb(0x65, 0x43, 0x21)
        );
        fs::write(&theme_path, "bad JSON").unwrap();
        config = load_config(config.paths.clone()).unwrap();
        let saved = fs::read_to_string(&config.paths.settings_path).unwrap();
        assert!(persist_theme(&mut config, "custom").is_err());
        assert_eq!(
            fs::read_to_string(&config.paths.settings_path).unwrap(),
            saved
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frame_palette_covers_input_transcript_footer_and_selector() {
        use ratatui::{backend::TestBackend, buffer::Cell, style::Color};
        let mut config = account_test_config(AuthData::default());
        let runtime = account_test_runtime("faux", None);
        let mut app = TuiApp::new(&config, &runtime);
        app.entries.clear();
        app.push(TuiEntryKind::Assistant, "neutral-answer");
        app.push(TuiEntryKind::Thinking, "secondary-thinking");
        app.push(
            TuiEntryKind::Tool,
            "completed bash\ncommand-detail\nsecondary-output",
        );
        app.push(TuiEntryKind::Error, "error-output");
        app.set_input("draft");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        for name in BUILTIN_THEMES {
            config.settings.theme = Some(name.to_string());
            app.refresh_chrome(&config, &runtime);
            let palette = terminal_theme(&config).unwrap().palette;
            terminal
                .draw(|frame| draw_tui(frame, &app, &config))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let cell_for = |text: &str| -> &Cell {
                for y in 0..buffer.area.height {
                    let row = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>();
                    if let Some(x) = row.find(text) {
                        return &buffer[(row[..x].width() as u16, y)];
                    }
                }
                panic!("missing rendered text {text}");
            };
            assert_eq!(cell_for("neutral-answer").fg, palette.foreground);
            assert_eq!(cell_for("secondary-thinking").fg, palette.muted);
            assert_eq!(cell_for("secondary-output").fg, palette.muted);
            assert_eq!(cell_for("error-output").fg, palette.error);
            assert_eq!(cell_for("> draft").fg, palette.accent);
            assert_eq!(cell_for("draft").fg, palette.foreground);
            assert_eq!(cell_for("draft").bg, palette.surface);
            assert_eq!(cell_for("faux/echo").fg, palette.muted);
            if *name == "system" {
                assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
                assert!(buffer
                    .content
                    .iter()
                    .all(|cell| !matches!(cell.fg, Color::Rgb(_, _, _))));
            }
            app.selector = Some(TuiSelectorState::new(
                "theme",
                selector_for_kind(&config, &runtime, "theme").unwrap(),
                "",
                None,
            ));
            terminal
                .draw(|frame| draw_tui(frame, &app, &config))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let selected = buffer
                .content
                .iter()
                .find(|cell| cell.modifier.contains(Modifier::REVERSED))
                .unwrap();
            assert_eq!(selected.fg, palette.accent);
            assert_eq!(selected.bg, palette.surface);
            let area = centered_rect(buffer.area, 82, 68);
            assert_eq!(buffer[(area.x, area.y)].fg, palette.border);
            app.selector = None;
        }
    }

    #[test]
    fn invalid_configured_theme_reports_diagnostic_and_renders_native_fallback() {
        use ratatui::{backend::TestBackend, style::Color};
        let mut config = account_test_config(AuthData::default());
        let runtime = account_test_runtime("faux", None);
        for (theme, accent) in [("missing", None), ("kimi", Some("bad-color"))] {
            config.settings.theme = Some(theme.into());
            config.settings.accent_color = accent.map(str::to_string);
            assert!(terminal_theme(&config).is_err());
            assert!(format_diagnostics(&config).contains("using system theme"));
            let app = TuiApp::new(&config, &runtime);
            assert!(app
                .entries
                .iter()
                .any(|entry| entry.kind == TuiEntryKind::Error
                    && entry.text.contains("using system theme")));
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| draw_tui(frame, &app, &config))
                .unwrap();
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset));
        }
    }

    #[test]
    fn diff_and_todo_widgets_use_semantic_colors() {
        use ratatui::{backend::TestBackend, style::Color};
        for name in BUILTIN_THEMES {
            let palette = TerminalTheme::builtin(name).unwrap().palette;
            assert_eq!(
                colorized_diff_line("+added", &palette).style.fg,
                Some(palette.success)
            );
            assert_eq!(
                colorized_diff_line("-removed", &palette).style.fg,
                Some(palette.error)
            );
            assert_eq!(
                colorized_diff_line("@@ hunk", &palette).style.fg,
                Some(palette.accent)
            );
            let app = TuiApp {
                todos: vec![
                    TodoItem {
                        content: "completed".into(),
                        status: TodoStatus::Completed,
                    },
                    TodoItem {
                        content: "active".into(),
                        status: TodoStatus::InProgress,
                    },
                    TodoItem {
                        content: "pending".into(),
                        status: TodoStatus::Pending,
                    },
                ],
                ..TuiApp::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
            terminal
                .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &palette))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(2, 2)].fg, palette.success);
            assert_eq!(buffer[(2, 3)].fg, palette.accent);
            assert_eq!(buffer[(2, 4)].fg, palette.muted);
            assert_eq!(buffer[(4, 4)].fg, Color::Reset);
        }
    }

    #[test]
    fn status_format_is_compact_and_unlabeled() {
        let settings = Settings {
            theme: Some("solar".to_string()),
            ..Settings::default()
        };
        let config = LoadedConfig {
            paths: test_config_paths(),
            settings,
            auth: AuthData::default(),
            models: vec![ModelDefinition {
                provider: "openai-codex".to_string(),
                id: "gpt-5.5".to_string(),
                reasoning: Some(true),
                thinking_level_map: Some(
                    serde_json::from_value(serde_json::json!({ "xhigh": "xhigh" })).unwrap(),
                ),
                ..ModelDefinition::default()
            }],
            model_scope: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: Vec::new(),
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            themes: Vec::new(),
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        };
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        session.active_model = Some(ModelRef {
            provider: "openai-codex".to_string(),
            id: "gpt-5.5".to_string(),
        });
        session.active_thinking_level = Some("xhigh".to_string());
        session.queued_messages.push("queued".to_string());
        let runtime = Runtime::new(session, ReloadableSystems::default());
        let mut editor = EditorState::default();
        editor.record_history("old prompt");

        let footer = footer_status(&config, &runtime, &editor);
        assert_eq!(footer, "openai-codex/gpt-5.5 xhigh solar ≡ 1 ↺ 1");
        assert_eq!(
            format_status(&config, &runtime, &editor),
            "status\topenai-codex/gpt-5.5 xhigh solar ≡ 1 ↺ 1"
        );
        for verbose in ["model:", "thinking:", "theme:", "queue:", "history:"] {
            assert!(!footer.contains(verbose));
        }
    }

    fn account_test_config(auth: AuthData) -> LoadedConfig {
        LoadedConfig {
            paths: test_config_paths(),
            settings: Settings::default(),
            auth,
            models: Vec::new(),
            model_scope: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: Vec::new(),
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            themes: Vec::new(),
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        }
    }

    fn account_test_runtime(provider: &str, active_account: Option<&str>) -> Runtime {
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        session.active_model = Some(ModelRef {
            provider: provider.to_string(),
            id: "echo".to_string(),
        });
        session.active_account = active_account.map(str::to_string);
        Runtime::new(session, ReloadableSystems::default())
    }

    #[test]
    fn account_selector_lists_stored_accounts_with_auto_entry() {
        let mut auth = AuthData::default();
        auth.insert(
            "faux",
            "default",
            AuthCredential::ApiKey {
                key: "default-key".to_string(),
            },
        );
        auth.insert(
            "faux",
            "work",
            AuthCredential::ApiKey {
                key: "work-key".to_string(),
            },
        );
        let config = account_test_config(auth);
        let runtime = account_test_runtime("faux", Some("work"));

        let items = account_selector_items(&config, &runtime);
        assert_eq!(
            items
                .iter()
                .map(|item| (item.label.as_str(), item.value.as_str(), item.active))
                .collect::<Vec<_>>(),
            vec![
                ("auto (default resolution)", "", false),
                ("default", "default", false),
                ("work", "work", true),
            ]
        );
    }

    /// Mid-turn, /account opens the picker from the app's chrome (no
    /// runtime borrow), a typed /account <name> queues for turn end, and
    /// /accounts starts a background probe that fills its placeholder.
    #[tokio::test]
    async fn streaming_account_commands_work_during_a_turn() {
        let mut auth = AuthData::default();
        auth.insert(
            "faux",
            "work",
            AuthCredential::ApiKey {
                key: "work-key".to_string(),
            },
        );
        let config = account_test_config(auth);
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let mut app = TuiApp::default();
        let runtime = account_test_runtime("faux", Some("work"));
        app.refresh_chrome(&config, &runtime);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        // A bare /account opens the picker over the running turn.
        app.set_input("/account");
        match handle_streaming_tui_key(enter, &mut app, &followups, &steering, &config) {
            StreamingKeyOutcome::Changed => {}
            other => panic!("bare /account: {other:?}"),
        }
        let selector = app.selector.take().expect("account picker");
        assert_eq!(selector.kind, "account");
        assert!(selector
            .items
            .iter()
            .any(|item| item.value == "work" && item.active));

        // Picking an account queues the switch for turn end.
        let mut picked = TuiSelectorState::new(
            "account",
            Selector::new("account", account_selector_items_for(&config, "faux", None)),
            "",
            None,
        );
        picked.selected = picked
            .filtered_indices
            .iter()
            .position(|&index| picked.items[index].value == "work")
            .expect("work row in the picker");
        app.selector = Some(picked);
        match handle_streaming_tui_key(enter, &mut app, &followups, &steering, &config) {
            StreamingKeyOutcome::SwitchAccount(Some(account)) => assert_eq!(account, "work"),
            other => panic!("picker pick: {other:?}"),
        }

        // A typed reference queues too; an unknown one is refused.
        app.set_input("/account work");
        match handle_streaming_tui_key(enter, &mut app, &followups, &steering, &config) {
            StreamingKeyOutcome::SwitchAccount(Some(account)) => assert_eq!(account, "work"),
            other => panic!("typed switch: {other:?}"),
        }
        app.set_input("/account auto");
        match handle_streaming_tui_key(enter, &mut app, &followups, &steering, &config) {
            StreamingKeyOutcome::SwitchAccount(None) => {}
            other => panic!("typed switch to auto: {other:?}"),
        }
        app.set_input("/account nope");
        match handle_streaming_tui_key(enter, &mut app, &followups, &steering, &config) {
            StreamingKeyOutcome::Changed => assert!(app
                .entries
                .last()
                .unwrap()
                .text
                .contains("account not found: nope")),
            other => panic!("unknown account: {other:?}"),
        }

        // /accounts starts the background probe without touching the turn.
        app.set_input("/accounts");
        match handle_streaming_tui_key(enter, &mut app, &followups, &steering, &config) {
            StreamingKeyOutcome::Changed => {}
            other => panic!("/accounts: {other:?}"),
        }
        assert!(app.account_status_task.is_some());
        assert!(app
            .entries
            .last()
            .unwrap()
            .text
            .contains("checking account status"));
        if let Some((_, task)) = app.account_status_task.take() {
            task.abort();
        }
    }

    #[test]
    fn account_selector_labels_env_pseudo_account() {
        let saved = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OPENROUTER_API_KEY", "test-openrouter-key");
        let config = account_test_config(AuthData::default());
        let runtime = account_test_runtime("openrouter", None);

        let items = account_selector_items(&config, &runtime);
        assert!(items.iter().any(|item| {
            item.value == "env" && item.label == "env (environment, read-only)" && !item.active
        }));
        assert!(items[0].active);

        match saved {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }

    #[test]
    fn status_line_marks_bound_account() {
        let config = account_test_config(AuthData::default());
        let runtime = account_test_runtime("openai-codex", Some("work"));
        let editor = EditorState::default();

        let footer = footer_status(&config, &runtime, &editor);
        assert!(footer.starts_with("openai-codex/echo@work "), "{footer}");
    }

    #[test]
    fn select_account_message_binds_and_clears_account() {
        let mut auth = AuthData::default();
        auth.insert(
            "faux",
            "work",
            AuthCredential::ApiKey {
                key: "work-key".to_string(),
            },
        );
        let mut config = account_test_config(auth);
        let mut runtime = account_test_runtime("faux", None);

        let message =
            select_from_selector_message(&mut config, &mut runtime, "/select account work")
                .expect("select account");
        assert_eq!(message, "account: work");
        assert_eq!(runtime.session().active_account, Some("work".to_string()));

        let message =
            select_from_selector_message(&mut config, &mut runtime, "/select account auto")
                .expect("clear account");
        assert_eq!(message, "account: auto");
        assert_eq!(runtime.session().active_account, None);
    }

    #[test]
    fn logout_rewrites_only_its_entry_and_never_persists_runtime_keys() {
        let root = std::env::temp_dir().join(format!("noel-cli-logout-{}", unique_temp_suffix()));
        let auth_path = root.join("agent").join("auth.json");
        let oauth = |token: &str, expires| AuthCredential::OAuth {
            access_token: format!("{token}-access"),
            refresh_token: Some(format!("{token}-refresh")),
            expires,
            account_id: None,
        };
        let openai = AuthCredential::ApiKey {
            key: "openai-key".to_string(),
        };
        // auth.json now holds tokens another process refreshed since startup.
        let mut current = AuthData::default();
        current.insert(
            "anthropic",
            DEFAULT_ACCOUNT_NAME,
            oauth("rotated", 4_000_000_000),
        );
        current.insert("openai", DEFAULT_ACCOUNT_NAME, openai.clone());
        noel_config::write_auth_file(&auth_path, &current).expect("seed auth.json");
        // This session's startup snapshot: the stale tokens plus a value that
        // came from `--api-key` and must stay in memory.
        let mut snapshot = AuthData::default();
        snapshot.insert("anthropic", DEFAULT_ACCOUNT_NAME, oauth("stale", 1));
        snapshot.insert("openai", DEFAULT_ACCOUNT_NAME, openai);
        snapshot.insert(
            "google",
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::ApiKey {
                key: "cli-only-key".to_string(),
            },
        );
        let mut config = account_test_config(snapshot);
        config.paths.auth_path = auth_path.clone();

        assert!(remove_stored_auth(&mut config, "openai", DEFAULT_ACCOUNT_NAME).unwrap());

        let content = fs::read_to_string(&auth_path).expect("read auth.json");
        assert!(!content.contains("cli-only-key"), "{content}");
        assert!(!content.contains("stale-"), "{content}");
        let stored = noel_config::read_auth_file(&auth_path).expect("parse auth.json");
        assert_eq!(
            stored.credential("anthropic", DEFAULT_ACCOUNT_NAME),
            Some(&oauth("rotated", 4_000_000_000))
        );
        assert!(stored.credential("openai", DEFAULT_ACCOUNT_NAME).is_none());
        assert!(config
            .auth
            .credential("openai", DEFAULT_ACCOUNT_NAME)
            .is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reload_keeps_the_trust_decision_and_cli_overrides() {
        // The developer's own `~/.agents/skills` must not load into the
        // test's config: isolate like the CLI test harness does, and put
        // the isolation back afterwards.
        let saved_isolation = std::env::var_os(noel_config::ENV_TEST_ISOLATION);
        std::env::set_var(noel_config::ENV_TEST_ISOLATION, "1");
        let root =
            std::env::temp_dir().join(format!("noel-cli-config-source-{}", unique_temp_suffix()));
        let agent = root.join("agent");
        let project = root.join("project");
        fs::create_dir_all(&agent).expect("create agent dir");
        fs::create_dir_all(project.join(".noel/extensions")).expect("create project extensions");
        fs::write(
            project.join(".noel/settings.json"),
            r#"{"shellCommandPrefix":"echo untrusted"}"#,
        )
        .expect("write project settings");
        fs::write(
            project.join(".noel/extensions/project.md"),
            "project extension",
        )
        .expect("write project extension");
        fs::write(project.join("AGENTS.md"), "project rules").expect("write context file");
        let resource = |name: &str, content: &str| {
            let path = root.join(format!("{name}.md"));
            fs::write(&path, content).expect("write cli resource");
            path.display().to_string()
        };
        let extension = resource("ext", "cli ext");
        let skill = resource(
            "skill",
            "---\nname: cli-skill\ndescription: From a flag.\n---\n",
        );
        let template = resource("tpl", "cli tpl");
        let cli = Cli::try_parse_from([
            "noel",
            "--system-prompt",
            "cli system",
            "--append-system-prompt",
            "cli append",
            "--no-context-files",
            "--tools",
            "read",
            "--provider",
            "anthropic",
            "--api-key",
            "cli-key",
            "-e",
            &extension,
            "--skill",
            &skill,
            "--prompt-template",
            &template,
        ])
        .expect("parse cli");
        let source = ConfigSource {
            cli,
            cwd: project.clone(),
            project_trusted: false,
        };
        let paths = ConfigPaths {
            cwd: project.clone(),
            agent_dir: agent.clone(),
            session_dir: agent.join("sessions"),
            settings_path: agent.join("settings.json"),
            project_settings_path: project.join(".noel/settings.json"),
            auth_path: agent.join("auth.json"),
            models_path: agent.join("models.json"),
            model_cache_path: agent.join("model-cache.json"),
            keybindings_path: agent.join("keybindings.json"),
        };

        let config = source.load(paths).expect("reload config");

        assert_eq!(config.settings.shell_command_prefix, None);
        let contents = |resources: &[ResourceFile]| {
            resources
                .iter()
                .map(|resource| resource.content.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(contents(&config.extensions), ["cli ext"]);
        assert_eq!(config.skills.len(), 1);
        assert_eq!(config.skills[0].name, "cli-skill");
        assert_eq!(config.prompt_templates.len(), 1);
        assert_eq!(config.prompt_templates[0].name, "tpl");
        assert_eq!(config.prompt_templates[0].content, "cli tpl");
        assert_eq!(config.system_prompt.as_deref(), Some("cli system"));
        assert_eq!(config.append_system_prompt, ["cli append"]);
        assert!(config.context_files.is_empty());
        assert_eq!(
            config.settings.enabled_tools,
            Some(vec!["read".to_string()])
        );
        assert_eq!(
            auth_for_provider(&config.auth, "anthropic", None),
            Some(ResolvedAuth::ApiKey("cli-key".to_string()))
        );
        match saved_isolation {
            Some(value) => std::env::set_var(noel_config::ENV_TEST_ISOLATION, value),
            None => std::env::remove_var(noel_config::ENV_TEST_ISOLATION),
        }
        let _ = fs::remove_dir_all(root);
    }

    /// Every `discovery` case of `system-prompt.json`: from the same files,
    /// trust decision, and flags, startup gives the prompt builder what
    /// upstream's resource loader gives its own. Each case runs twice: on
    /// the tree as upstream wrote it, which is a project written for pi
    /// (`.pi/`), and on the same tree under noel's names (`.noel/`).
    #[test]
    fn prompt_and_context_files_match_ts_discovery() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/system-prompt.json"
        ))
        .expect("parse TS parity fixture");
        let cases = fixture["discovery"].as_array().expect("discovery cases");
        assert_eq!(cases.len(), 15);
        let layouts = cases.iter().flat_map(|case| {
            [
                ("as upstream wrote it", case.clone()),
                (
                    "with noel's names",
                    noel_config::value_with_noel_names(case),
                ),
            ]
        });
        for (layout, case) in layouts {
            let name = format!("{} ({layout})", case["name"].as_str().expect("case name"));
            let root =
                std::env::temp_dir().join(format!("noel-cli-discovery-{}", unique_temp_suffix()));
            let text = |value: &serde_json::Value| {
                let text = value.as_str().expect("fixture string");
                text.replace("<tmp>", &root.display().to_string())
                    .replace("<bom>", "\u{feff}")
            };
            let (agent, cwd) = (root.join("agent"), root.join(text(&case["cwd"])));
            let dirs = case["dirs"].as_array().expect("dirs").iter().map(text);
            for dir in dirs.chain(["agent".to_string(), text(&case["cwd"])]) {
                fs::create_dir_all(root.join(dir)).expect("create dir");
            }
            for (path, content) in case["files"].as_object().expect("files") {
                let path = root.join(path);
                fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
                fs::write(path, text(content)).expect("write file");
            }

            let flags = &case["flags"];
            let mut args = vec!["noel".to_string()];
            if !flags["systemPrompt"].is_null() {
                args.extend(["--system-prompt".to_string(), text(&flags["systemPrompt"])]);
            }
            for value in flags["appendSystemPrompt"].as_array().expect("flags") {
                args.extend(["--append-system-prompt".to_string(), text(value)]);
            }
            if flags["noContextFiles"] == true {
                args.push("--no-context-files".to_string());
            }
            let source = ConfigSource {
                cli: Cli::try_parse_from(args).expect("parse cli"),
                cwd: cwd.clone(),
                project_trusted: case["trusted"].as_bool().expect("trusted"),
            };
            let config = source
                .load(ConfigPaths {
                    cwd: cwd.clone(),
                    agent_dir: agent.clone(),
                    session_dir: agent.join("sessions"),
                    settings_path: agent.join("settings.json"),
                    project_settings_path: noel_config::project_config_dir(&cwd)
                        .join("settings.json"),
                    auth_path: agent.join("auth.json"),
                    models_path: agent.join("models.json"),
                    model_cache_path: agent.join("model-cache.json"),
                    keybindings_path: agent.join("keybindings.json"),
                })
                .expect("load config");

            let result = &case["result"];
            assert_eq!(
                config.system_prompt.as_deref(),
                result["customPrompt"].as_str(),
                "{name}: custom prompt"
            );
            assert_eq!(
                config.append_system_prompt.join("\n\n"),
                text(&result["appendSystemPrompt"]),
                "{name}: appended prompt"
            );
            // Directories above the case's tree are not part of the case.
            let context_files = config
                .context_files
                .iter()
                .filter(|file| file.path.starts_with(&root))
                .map(|file| (file.path.display().to_string(), file.content.clone()))
                .collect::<Vec<_>>();
            let expected = result["contextFiles"].as_array().expect("context files");
            let expected = expected
                .iter()
                .map(|file| (text(&file["path"]), text(&file["content"])))
                .collect::<Vec<_>>();
            assert_eq!(context_files, expected, "{name}: context files");
            assert_eq!(config.diagnostics, Vec::<String>::new(), "{name}");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn append_flags_add_to_the_setting() {
        // The fixture above covers upstream: flags replace APPEND_SYSTEM.md.
        // The `appendSystemPrompt` setting is noel's own, and flags add to it.
        let root =
            std::env::temp_dir().join(format!("noel-cli-append-setting-{}", unique_temp_suffix()));
        let agent = root.join("agent");
        fs::create_dir_all(&agent).expect("create agent dir");
        fs::write(agent.join("APPEND_SYSTEM.md"), "from the file").expect("write append file");
        fs::write(
            agent.join("settings.json"),
            r#"{"appendSystemPrompt":["from the setting"]}"#,
        )
        .expect("write settings");
        let source = ConfigSource {
            cli: Cli::try_parse_from(["noel", "--append-system-prompt", "from the flag"])
                .expect("parse cli"),
            cwd: root.clone(),
            project_trusted: false,
        };
        let config = source
            .load(ConfigPaths {
                cwd: root.clone(),
                agent_dir: agent.clone(),
                session_dir: agent.join("sessions"),
                settings_path: agent.join("settings.json"),
                project_settings_path: root.join(".noel/settings.json"),
                auth_path: agent.join("auth.json"),
                models_path: agent.join("models.json"),
                model_cache_path: agent.join("model-cache.json"),
                keybindings_path: agent.join("keybindings.json"),
            })
            .expect("load config");
        assert_eq!(
            config.append_system_prompt,
            ["from the setting", "from the flag"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_prefers_models_with_credentials_over_faux() {
        // Made-up providers have no environment or login-file credentials,
        // so only `config.auth` decides which ones are usable.
        let model = |provider: &str, id: &str| ModelDefinition {
            context_window: None,
            provider: provider.to_string(),
            id: id.to_string(),
            name: None,
            api: if provider == "faux" {
                ConfigProviderApi::Faux
            } else {
                ConfigProviderApi::Anthropic
            },
            base_url: None,
            ..ModelDefinition::default()
        };
        let key = |key: &str| AuthCredential::ApiKey {
            key: key.to_string(),
        };
        let mut config = account_test_config(AuthData::default());
        config.models = vec![
            model("faux", "echo"),
            model("keyed", "first"),
            model("unkeyed", "saved"),
        ];
        let pick = |config: &LoadedConfig, args: &[&str], offline: bool| {
            let mut config = config.clone();
            let mut runtime = account_test_runtime("faux", None);
            runtime.set_active_model(None).expect("clear model");
            let cli = Cli::try_parse_from(std::iter::once("noel").chain(args.iter().copied()))
                .expect("parse cli");
            let selection =
                resolve_cli_selection(&cli, &mut config, offline).expect("read the command line");
            select_initial_model(&mut runtime, &config, &selection, offline).expect("select model");
            runtime
                .session()
                .active_model
                .as_ref()
                .map(|model| format!("{}/{}", model.provider, model.id))
        };

        // Nothing has credentials: faux is the last resort.
        assert_eq!(pick(&config, &[], false).as_deref(), Some("faux/echo"));

        // A model with credentials wins over faux; a provider's usual model
        // (upstream defaultModelPerProvider) wins over catalog order.
        config
            .auth
            .insert("keyed", DEFAULT_ACCOUNT_NAME, key("keyed-key"));
        assert_eq!(pick(&config, &[], false).as_deref(), Some("keyed/first"));
        config
            .auth
            .insert("anthropic", DEFAULT_ACCOUNT_NAME, key("anthropic-key"));
        config.models.extend([
            model("anthropic", "claude-opus-4-1-20250805"),
            model("anthropic", "claude-opus-4-8"),
        ]);
        assert_eq!(
            pick(&config, &[], false).as_deref(),
            Some("anthropic/claude-opus-4-8")
        );

        // A saved default counts only when its provider has credentials.
        config.settings.default_provider = Some("unkeyed".to_string());
        config.settings.default_model = Some("saved".to_string());
        assert_eq!(
            pick(&config, &[], false).as_deref(),
            Some("anthropic/claude-opus-4-8")
        );
        config.settings.default_provider = Some("keyed".to_string());
        config.settings.default_model = Some("first".to_string());
        assert_eq!(pick(&config, &[], false).as_deref(), Some("keyed/first"));

        // Offline only faux can run; an explicit --model still wins.
        assert_eq!(pick(&config, &[], true).as_deref(), Some("faux/echo"));
        assert_eq!(
            pick(&config, &["--model", "faux/echo"], false).as_deref(),
            Some("faux/echo")
        );
        assert_eq!(
            pick(&config, &["--model", "unkeyed/saved"], true).as_deref(),
            Some("unkeyed/saved")
        );
    }

    #[tokio::test]
    async fn provider_construction_tolerates_bound_account_without_matching_credential() {
        let mut config = account_test_config(AuthData::default());
        config.models.push(ModelDefinition {
            context_window: None,
            provider: "faux".to_string(),
            id: "echo".to_string(),
            name: None,
            api: ConfigProviderApi::Faux,
            base_url: None,
            ..ModelDefinition::default()
        });
        let runtime = account_test_runtime("faux", Some("work"));

        provider_for_runtime(&runtime, &config, false)
            .await
            .expect("faux provider builds regardless of account binding");
    }

    #[tokio::test]
    async fn every_model_request_sends_the_credential_current_when_it_is_sent() {
        use std::io::BufReader;
        let root =
            std::env::temp_dir().join(format!("noel-cli-fresh-auth-{}", unique_temp_suffix()));
        fs::create_dir_all(&root).unwrap();
        let auth_path = root.join("auth.json");
        let credential = |access_token: &str, expires| AuthCredential::OAuth {
            access_token: access_token.to_string(),
            refresh_token: None,
            expires,
            account_id: Some("account".to_string()),
        };
        let save = |access_token: &str| {
            let mut auth = AuthData::default();
            auth.insert(
                "openai-codex",
                "default",
                credential(access_token, 4_000_000_000),
            );
            fs::write(&auth_path, serde_json::to_string(&auth).unwrap()).unwrap();
        };
        // The model endpoint records the bearer token of each request.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            (0..2)
                .map(|_| {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream);
                    let (mut authorization, mut length) = (String::new(), 0);
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let Some((name, value)) = line.trim_end().split_once(": ") else {
                            if line.trim_end().is_empty() {
                                break;
                            }
                            continue;
                        };
                        match name.to_ascii_lowercase().as_str() {
                            "authorization" => authorization = value.to_string(),
                            "content-length" => length = value.parse().unwrap(),
                            _ => {}
                        }
                    }
                    reader.read_exact(&mut vec![0; length]).unwrap();
                    let body = r#"{"output_text":"ok"}"#;
                    write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    authorization
                })
                .collect::<Vec<_>>()
        });
        // The startup snapshot predates a refresh: its token long expired.
        let mut snapshot = AuthData::default();
        snapshot.insert("openai-codex", "default", credential("startup-access", 1));
        let mut config = account_test_config(snapshot);
        config.paths.auth_path = auth_path.clone();
        config.models.push(ModelDefinition {
            context_window: None,
            provider: "openai-codex".to_string(),
            id: "echo".to_string(),
            name: None,
            api: ConfigProviderApi::OpenAi,
            base_url: Some(url),
            ..ModelDefinition::default()
        });
        let runtime = account_test_runtime("openai-codex", None);
        let request = || ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };

        save("first-access");
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .unwrap();
        provider.complete(request()).await.unwrap();
        save("second-access");
        provider.complete(request()).await.unwrap();
        assert_eq!(
            server.join().unwrap(),
            ["Bearer first-access", "Bearer second-access"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// With failover on, the `/account` selector says what `auto` does and
    /// that choosing an account pins it.
    #[test]
    fn account_selector_names_failover_and_pinning_when_failover_is_on() {
        let (config, runtime) = failover_fixture(
            "failover-selector",
            "http://127.0.0.1:9",
            &["first", "second"],
            1,
        );
        let labels = |config: &LoadedConfig| {
            account_selector_items(config, &runtime)
                .into_iter()
                .map(|item| item.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&config),
            [
                "auto (failover: sticky account, switches on limits)",
                "first (pins account; failover disabled while pinned)",
                "second (pins account; failover disabled while pinned)",
            ]
        );
        let mut off = config;
        off.settings.failover = None;
        assert_eq!(
            labels(&off),
            ["auto (default resolution)", "first", "second"]
        );
    }

    /// What the failover endpoint answers a request with.
    enum Answer {
        /// A whole reply with this text.
        Says(&'static str),
        /// This HTTP status.
        Status(u16),
        /// A 429 whose `retry-after` header holds this.
        Limited(&'static str),
        /// Anthropic's 529: the provider is overloaded.
        Overloaded,
        /// The start of a reply, then the connection drops.
        BreaksOff,
    }

    /// A local chat-completions endpoint for the failover tests. It answers
    /// a request as `answer` says for the credential the request carries and
    /// the number of requests that credential made before. Returns its URL
    /// and the credentials of the requests it got, in order.
    fn failover_endpoint(
        answer: impl Fn(&str, usize) -> Answer + Send + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::BufReader;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut reader = BufReader::new(stream.unwrap());
                let (mut credential, mut length) = (String::new(), 0);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let Some((name, value)) = line.trim_end().split_once(": ") else {
                        if line.trim_end().is_empty() {
                            break;
                        }
                        continue;
                    };
                    match name.to_ascii_lowercase().as_str() {
                        "authorization" => {
                            credential = value.trim_start_matches("Bearer ").to_string()
                        }
                        "content-length" => length = value.parse().unwrap(),
                        _ => {}
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                let earlier = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(credential.clone());
                    seen.iter().filter(|seen| **seen == credential).count() - 1
                };
                let json = |status: &str, headers: &str, body: String| {
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let response = match answer(&credential, earlier) {
                    // A chat completion, and the same text as the ChatGPT
                    // endpoint of an `openai-codex` login words it.
                    Answer::Says(text) => json(
                        "200 OK",
                        "",
                        serde_json::json!({
                            "choices": [{"message": {"content": text}, "finish_reason": "stop"}],
                            "output_text": text,
                        })
                        .to_string(),
                    ),
                    Answer::Status(status) => json(
                        &format!("{status} Synthetic"),
                        "",
                        r#"{"error":"synthetic"}"#.to_string(),
                    ),
                    Answer::Limited(retry_after) => json(
                        "429 Too Many Requests",
                        &format!("retry-after: {retry_after}\r\n"),
                        r#"{"error":"synthetic"}"#.to_string(),
                    ),
                    Answer::Overloaded => json(
                        "529 Overloaded",
                        "",
                        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                            .to_string(),
                    ),
                    // One chunk of a chunked stream and no end of it.
                    Answer::BreaksOff => {
                        let event =
                            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n",
                            event.len()
                        )
                    }
                };
                // The client may have gone: a dropped request is not a failure here.
                let _ = reader.into_inner().write_all(response.as_bytes());
            }
        });
        (url, requests)
    }

    /// A config whose provider `provider` has one model at `url` and the
    /// API-key accounts `accounts` (each with the key `<account>-key`), with
    /// failover enabled and an agent directory of its own, and a runtime on
    /// that model that retries twice after `backoff_ms`. Every failover test
    /// has a provider id of its own, which its notices name: notices are
    /// the process's.
    fn failover_fixture(
        provider: &str,
        url: &str,
        accounts: &[&str],
        backoff_ms: u64,
    ) -> (LoadedConfig, Runtime) {
        let mut auth = AuthData::default();
        for account in accounts {
            auth.insert(
                provider,
                account,
                AuthCredential::ApiKey {
                    key: format!("{account}-key"),
                },
            );
        }
        let mut config = account_test_config(auth);
        fs::create_dir_all(&config.paths.agent_dir).unwrap();
        config.settings.failover = Some(noel_config::FailoverSettings {
            enabled: Some(true),
            ..Default::default()
        });
        config.models.push(ModelDefinition {
            provider: provider.to_string(),
            id: "echo".to_string(),
            api: ConfigProviderApi::OpenAi,
            base_url: Some(url.to_string()),
            ..ModelDefinition::default()
        });
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        session.active_model = Some(ModelRef {
            provider: provider.to_string(),
            id: "echo".to_string(),
        });
        let mut systems = ReloadableSystems::default();
        systems.retry.max_retries = 2;
        systems.retry.base_delay_ms = backoff_ms;
        (config, Runtime::new(session, systems))
    }

    /// What the pools of `provider` saved in the fixture's agent directory.
    fn saved_failover(config: &LoadedConfig, provider: &str) -> account_failover::SavedProvider {
        account_failover::saved_state(
            &config.paths.agent_dir.join(account_failover::STATE_FILE),
            provider,
        )
    }

    /// One turn on a provider built for it, as every mode runs one: how it
    /// ended, and how many retries the run loop started.
    async fn failover_turn(
        runtime: &mut Runtime,
        config: &LoadedConfig,
    ) -> (std::result::Result<String, String>, usize) {
        let provider = provider_for_runtime(runtime, config, false).await.unwrap();
        let mut retries = 0;
        let run = RunControl::default();
        let turn = run_user_turn_streaming_events_with_media(
            runtime,
            provider.as_ref(),
            "hello".into(),
            Vec::new(),
            &run,
            |event| retries += usize::from(matches!(event, TurnEvent::RetryStarted(_))),
        );
        let result = tokio::time::timeout(Duration::from_secs(30), turn)
            .await
            .expect("the turn waits for no long backoff");
        (result.map_err(|error| error.to_string()), retries)
    }

    /// Failover acts before the run loop's retry: a 429 with no output
    /// moves the request to the next account inside the provider, so the
    /// run loop sees a reply, starts no retry and waits for no backoff. The
    /// choice outlives the pool, which every request builds anew.
    #[tokio::test]
    async fn a_rate_limited_account_fails_over_inside_the_request_without_a_retry() {
        let provider = "failover-switch";
        let (url, requests) = failover_endpoint(|credential, _| match credential {
            "first-key" => Answer::Status(429),
            _ => Answer::Says("from the second account"),
        });
        // A backoff would outlast the turn's time limit.
        let (config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 60_000);

        let (reply, retries) = failover_turn(&mut runtime, &config).await;
        assert_eq!(reply.as_deref(), Ok("from the second account"));
        assert_eq!(retries, 0);
        assert_eq!(*requests.lock().unwrap(), ["first-key", "second-key"]);
        // What the TUI shows after the turn: both accounts, and until when.
        let notices = account_failover::notices_given(&format!("{provider} account "));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("failover-switch account first is rate limited until ")
                && notices[0].ends_with(" UTC; switched to second"),
            "{notices:?}"
        );

        // The next turn has a new provider and its request a new pool: the
        // second account still carries the session and the first is still
        // limited, so it is not asked again.
        let (reply, retries) = failover_turn(&mut runtime, &config).await;
        assert_eq!(reply.as_deref(), Ok("from the second account"));
        assert_eq!(retries, 0);
        assert_eq!(
            *requests.lock().unwrap(),
            ["first-key", "second-key", "second-key"]
        );
        let saved = saved_failover(&config, provider);
        assert_eq!(saved.current.as_deref(), Some("second"));
        assert_eq!(saved.limited_until.keys().collect::<Vec<_>>(), ["first"]);
        assert_eq!(
            account_failover::notices_given(&format!("{provider} account ")).len(),
            1
        );
    }

    /// A rate limit lasts until the time the provider's `retry-after` gives,
    /// and five minutes when the answer gives none.
    #[tokio::test]
    async fn a_rate_limit_lasts_until_the_reset_the_provider_gives() {
        for (provider, answer) in [
            ("failover-retry-after", Some("7200")),
            ("failover-no-retry-after", None),
        ] {
            let (url, _) = failover_endpoint(move |credential, _| match (credential, answer) {
                ("first-key", Some(seconds)) => Answer::Limited(seconds),
                ("first-key", None) => Answer::Status(429),
                _ => Answer::Says("ok"),
            });
            let (config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 1);
            let before = unix_seconds().unwrap() as i64;
            let (reply, _) = failover_turn(&mut runtime, &config).await;
            assert_eq!(reply.as_deref(), Ok("ok"));
            let wait = if answer.is_some() {
                7_200
            } else {
                account_failover::DEFAULT_COOLDOWN_SECONDS
            };
            let until = saved_failover(&config, provider).limited_until["first"];
            assert!(
                (before + wait..=before + wait + 2).contains(&until),
                "{provider}: {until} is not {wait} s after {before}"
            );
        }
    }

    /// With no account left the turn ends at once. The pool's error is noel's
    /// own, not a provider's answer, so the run loop does not retry it: each
    /// account is asked once, and none again while they are limited.
    #[tokio::test]
    async fn an_exhausted_pool_ends_the_turn_and_is_not_retried() {
        let (url, requests) = failover_endpoint(|_, _| Answer::Status(429));
        let (config, mut runtime) =
            failover_fixture("failover-exhausted", &url, &["first", "second"], 60_000);

        let (reply, retries) = failover_turn(&mut runtime, &config).await;
        let error = reply.expect_err("no account is left");
        assert!(
            error.contains(
                "every failover-exhausted account is rate limited; the first may be asked again at "
            ) && error.contains("first: request failed with status 429 Too Many Requests")
                && error.contains("second: request failed with status 429 Too Many Requests"),
            "{error}"
        );
        assert_eq!(retries, 0);
        assert_eq!(*requests.lock().unwrap(), ["first-key", "second-key"]);

        // Both accounts are limited: the next turn sends nothing.
        let (reply, retries) = failover_turn(&mut runtime, &config).await;
        let error = reply.expect_err("both accounts are limited");
        assert!(
            error.contains("every failover-exhausted account is rate limited")
                && error.ends_with(" UTC"),
            "{error}"
        );
        assert_eq!(retries, 0);
        assert_eq!(*requests.lock().unwrap(), ["first-key", "second-key"]);
    }

    /// Failures that are not a rate limit before any output stay with the
    /// account in use and are the run loop's to retry: an overloaded
    /// provider (529), and a reply that breaks off. Neither moves to the
    /// other account or sets one aside.
    #[tokio::test]
    async fn other_failures_are_retried_by_the_run_loop_on_the_same_account() {
        let provider = "failover-retry";
        let (url, requests) = failover_endpoint(|_, earlier| match earlier {
            0 => Answer::Overloaded,
            1 => Answer::BreaksOff,
            _ => Answer::Says("the whole reply"),
        });
        let (config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 1);

        let (reply, retries) = failover_turn(&mut runtime, &config).await;
        assert_eq!(reply.as_deref(), Ok("the whole reply"));
        assert_eq!(retries, 2);
        assert_eq!(
            *requests.lock().unwrap(),
            ["first-key", "first-key", "first-key"]
        );
        let saved = saved_failover(&config, provider);
        assert_eq!(saved.current.as_deref(), Some("first"));
        assert!(saved.limited_until.is_empty(), "{saved:?}");
        assert!(account_failover::notices_given(&format!("{provider} account ")).is_empty());
    }

    /// An account whose 5h usage window the usage cache shows at the
    /// threshold is passed over before the provider limits it, and the
    /// provider's own `exhaustionThresholdPct` moves the threshold.
    #[tokio::test]
    async fn an_account_at_its_usage_threshold_is_passed_over() {
        for (provider, threshold, first_serves) in [
            ("failover-usage", None, false),
            ("failover-usage-high", Some(99), true),
        ] {
            let (url, requests) = failover_endpoint(|credential, _| match credential {
                "first-key" => Answer::Says("first"),
                _ => Answer::Says("second"),
            });
            let (mut config, mut runtime) =
                failover_fixture(provider, &url, &["first", "second"], 1);
            if let Some(threshold) = threshold {
                config.settings.failover.as_mut().unwrap().providers.insert(
                    provider.to_string(),
                    noel_config::FailoverProviderSettings {
                        exhaustion_threshold_pct: Some(threshold),
                        ..Default::default()
                    },
                );
            }
            let now = unix_seconds().unwrap() as i64;
            let mut cache = noel_config::UsageCache::default();
            for (account, used_pct) in [("first", 95.0), ("second", 10.0)] {
                cache.accounts.insert(
                    format!("{provider}:{account}"),
                    noel_config::AccountUsage {
                        plan: None,
                        windows: vec![noel_config::UsageWindow {
                            label: "5h".to_string(),
                            used_pct,
                            resets_at: Some(now + 3_600),
                        }],
                        balance: None,
                        fetched_at: now,
                    },
                );
            }
            noel_config::write_usage_cache(
                &config.paths.agent_dir.join("usage-cache.json"),
                &cache,
            )
            .unwrap();

            let (reply, _) = failover_turn(&mut runtime, &config).await;
            let expected = if first_serves { "first" } else { "second" };
            assert_eq!(reply.as_deref(), Ok(expected), "{provider}");
            assert_eq!(*requests.lock().unwrap(), [format!("{expected}-key")]);
        }
    }

    /// An account whose credential cannot be refreshed is left out, with one
    /// notice however many turns it is left out of, and the others serve.
    /// Only a provider with a login of its own expires one: `anthropic`.
    #[tokio::test]
    async fn an_account_that_cannot_refresh_is_left_out_of_the_pool() {
        let provider = "anthropic";
        let (url, requests) = failover_endpoint(|_, _| Answer::Says("from the second account"));
        let (mut config, mut runtime) = failover_fixture(provider, &url, &["second"], 1);
        // Expired, and nothing to refresh it with.
        config.auth.insert(
            provider,
            "first",
            AuthCredential::OAuth {
                access_token: "first-access".to_string(),
                refresh_token: None,
                expires: 1,
                account_id: None,
            },
        );
        for _ in 0..2 {
            let (reply, _) = failover_turn(&mut runtime, &config).await;
            assert_eq!(reply.as_deref(), Ok("from the second account"));
        }
        assert_eq!(*requests.lock().unwrap(), ["second-key", "second-key"]);
        let notices = account_failover::notices_given(&format!("{provider} account "));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("anthropic account first is left out of failover: "),
            "{notices:?}"
        );
    }

    /// Without failover a rate limit is the run loop's, as on a build
    /// without this feature: the session's account is asked again after
    /// each backoff and no other account is tried. That holds when failover
    /// is off (the default) and when `/account` pins an account.
    #[tokio::test]
    async fn a_pinned_account_or_disabled_failover_does_not_rotate() {
        for pinned in [true, false] {
            let provider = if pinned {
                "failover-pinned"
            } else {
                "failover-off"
            };
            let (url, requests) = failover_endpoint(|_, _| Answer::Status(429));
            let (mut config, mut runtime) =
                failover_fixture(provider, &url, &["default", "second"], 1);
            if pinned {
                runtime
                    .set_active_account(Some("second".to_string()))
                    .unwrap();
            } else {
                config.settings.failover = None;
            }
            let account = if pinned { "second-key" } else { "default-key" };

            let (reply, retries) = failover_turn(&mut runtime, &config).await;
            let error = reply.expect_err("the account stays rate limited");
            assert!(
                error.starts_with("request failed with status 429 Too Many Requests"),
                "{error}"
            );
            assert_eq!(retries, 2);
            assert_eq!(*requests.lock().unwrap(), [account, account, account]);
            assert_eq!(
                saved_failover(&config, provider),
                account_failover::SavedProvider::default()
            );
        }
    }

    /// Named accounts without a `default` make a provider's models
    /// available when failover rotates between them; without failover the
    /// session has to pin one.
    #[test]
    fn named_accounts_make_a_provider_usable_when_failover_rotates() {
        let (mut config, _) = failover_fixture(
            "failover-usable",
            "http://127.0.0.1:9",
            &["first", "second"],
            1,
        );
        assert!(provider_usable(&config, "failover-usable", false));
        assert!(!provider_usable(&config, "failover-usable", true));
        config.settings.failover = None;
        assert!(!provider_usable(&config, "failover-usable", false));
    }

    /// A key given with `--api-key` serves the run: failover is off, so the
    /// stored accounts cannot take its place.
    #[test]
    fn an_api_key_on_the_command_line_turns_failover_off() {
        let (mut config, _) = failover_fixture(
            "failover-cli-key",
            "http://127.0.0.1:9",
            &["first", "second"],
            1,
        );
        let cli = Cli::try_parse_from([
            "noel",
            "--model",
            "failover-cli-key/echo",
            "--api-key",
            "cli-key",
        ])
        .unwrap();
        apply_cli_overrides(&cli, Path::new("."), &mut config).unwrap();
        assert_eq!(config.settings.failover, None);
        assert!(!failover_enabled_for(&config, "failover-cli-key"));
        assert_eq!(
            auth_for_provider(&config.auth, "failover-cli-key", None),
            Some(ResolvedAuth::ApiKey("cli-key".to_string()))
        );
    }

    /// What the run loop asks the provider about the model does not depend
    /// on failover: the thinking level it records, and the output limit it
    /// measures a cut-off reply against.
    #[tokio::test]
    async fn failover_keeps_the_thinking_level_and_the_output_limit() {
        let (mut config, runtime) = failover_fixture(
            "failover-traits",
            "http://127.0.0.1:9",
            &["first", "second"],
            1,
        );
        config.settings.default_thinking_level = Some("high".to_string());
        config.models[0].reasoning = Some(true);
        config.models[0].max_tokens = Some(4_096);
        let with_failover = provider_for_runtime(&runtime, &config, false)
            .await
            .unwrap();
        config.settings.failover = None;
        let without = provider_for_runtime(&runtime, &config, false)
            .await
            .unwrap();
        assert_eq!(with_failover.thinking_level().as_deref(), Some("high"));
        assert_eq!(with_failover.output_limit(), Some(4_096));
        assert_eq!(with_failover.thinking_level(), without.thinking_level());
        assert_eq!(with_failover.output_limit(), without.output_limit());
    }

    /// The pool holds only the accounts the provider can use, each with the
    /// credential that is current when the request is sent. `openai-codex`
    /// takes ChatGPT logins: its stored API key is no candidate. A login
    /// that expired in the startup snapshot is read again from auth.json,
    /// under its lock, as a single account's is.
    #[tokio::test]
    async fn the_pool_has_the_providers_own_accounts_with_current_credentials() {
        let root =
            std::env::temp_dir().join(format!("noel-cli-failover-auth-{}", unique_temp_suffix()));
        fs::create_dir_all(&root).unwrap();
        let auth_path = root.join("auth.json");
        let stored = |generation: &str, expires| {
            let mut auth = AuthData::default();
            for account in ["alpha", "beta"] {
                auth.insert(
                    "openai-codex",
                    account,
                    AuthCredential::OAuth {
                        access_token: format!("{account}-{generation}"),
                        refresh_token: None,
                        expires,
                        account_id: Some(account.to_string()),
                    },
                );
            }
            // Sorts between the two logins, where a candidate would be
            // tried after the first.
            auth.insert(
                "openai-codex",
                "apikey",
                AuthCredential::ApiKey {
                    key: "stray-key".to_string(),
                },
            );
            auth
        };
        let save = |generation: &str| {
            let auth = stored(generation, 4_000_000_000);
            fs::write(&auth_path, serde_json::to_string(&auth).unwrap()).unwrap();
        };
        let (url, requests) = failover_endpoint(|credential, _| {
            if credential.starts_with("alpha") {
                Answer::Status(429)
            } else {
                Answer::Says("ok")
            }
        });
        // The startup snapshot predates a refresh: its tokens long expired.
        let mut config = account_test_config(stored("startup", 1));
        config.paths.auth_path = auth_path.clone();
        config.settings.failover = Some(noel_config::FailoverSettings {
            enabled: Some(true),
            ..Default::default()
        });
        config.models.push(ModelDefinition {
            provider: "openai-codex".to_string(),
            id: "echo".to_string(),
            api: ConfigProviderApi::OpenAi,
            base_url: Some(url),
            ..ModelDefinition::default()
        });
        assert!(failover_enabled_for(&config, "openai-codex"));
        // `openai` takes API keys: of these accounts only one is its own.
        assert!(!failover_enabled_for(&config, "openai"));
        let runtime = account_test_runtime("openai-codex", None);
        let request = || ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };

        save("first");
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .unwrap();
        provider.complete(request()).await.unwrap();
        save("second");
        provider.complete(request()).await.unwrap();
        assert_eq!(
            *requests.lock().unwrap(),
            ["alpha-first", "beta-first", "beta-second"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A final reply cut off below the model's own output limit ran out of
    /// context: it is dropped, the session is compacted, and the request is
    /// sent again. One cut off at that limit stands, because a smaller
    /// context would not make it longer. The limit is the catalog's
    /// `maxTokens`, as for upstream's `isRecoverableLength`.
    #[tokio::test]
    async fn a_reply_cut_off_below_the_catalogs_output_limit_is_an_overflow() {
        use serde_json::json;
        use std::io::BufReader;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // A built-in model served over chat completions, here by a local
        // endpoint that answers each request with the next reply.
        let model = noel_ai::catalog::provider_base_model("deepseek").expect("a built-in model");
        assert_eq!(model.api, AiProviderApi::OpenAi);
        let limit = model.max_tokens;
        let says = |text: &str| json!({"choices": [{"message": {"content": text}, "finish_reason": "stop"}]});
        let cut_off = |output: u64| {
            json!({
                "choices": [{"message": {"content": "This answer is cut"}, "finish_reason": "length"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": output},
            })
        };
        let serve = |replies: Vec<serde_json::Value>| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let served = Arc::new(AtomicUsize::new(0));
            let count = served.clone();
            std::thread::spawn(move || {
                for (stream, reply) in listener.incoming().zip(replies) {
                    let mut reader = BufReader::new(stream.unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line.trim_end().is_empty() {
                            break;
                        }
                        let line = line.to_ascii_lowercase();
                        if let Some(value) = line.strip_prefix("content-length: ") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    reader.read_exact(&mut vec![0; length]).unwrap();
                    count.fetch_add(1, Ordering::SeqCst);
                    let body = reply.to_string();
                    write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            (url, served)
        };

        // What the turn answers and how many requests it took, when its
        // first reply stops at `length` after `output` tokens.
        let turn = |output: u64| async move {
            let (url, served) = serve(vec![
                cut_off(output),
                says("Summary of the first turn."),
                says("The whole answer."),
            ]);
            let mut auth = AuthData::default();
            auth.insert(
                &model.provider,
                DEFAULT_ACCOUNT_NAME,
                AuthCredential::ApiKey {
                    key: "test-key".to_string(),
                },
            );
            let mut config = account_test_config(auth);
            config.models.push(ModelDefinition {
                base_url: Some(url),
                ..ModelDefinition::from(model)
            });
            let mut session = SessionState::new("session-1", PathBuf::from("."));
            session.active_model = Some(ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            });
            // An older turn, long enough that its summary is shorter.
            let mut systems = ReloadableSystems::default();
            systems.compaction.keep_recent_tokens = 1;
            let mut runtime = Runtime::new(session, systems);
            let first_answer = "The migration adds a nullable column, backfills it in batches of \
                a thousand rows, then makes it required; the rollback drops the column again, \
                and both directions were run against a copy of the production schema.";
            for (role, content) in [
                (MessageRole::User, "first"),
                (MessageRole::Assistant, first_answer),
            ] {
                runtime
                    .push_message(ConversationMessage {
                        role,
                        content: content.into(),
                        thinking: String::new(),
                        media: Vec::new(),
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Vec::new(),
                        aborted: false,
                    })
                    .unwrap();
            }
            let provider = provider_for_runtime(&runtime, &config, false)
                .await
                .unwrap();
            assert_eq!(provider.output_limit(), Some(limit));
            let answer = run_user_turn(&mut runtime, provider.as_ref(), "second".into())
                .await
                .unwrap();
            (answer, served.load(Ordering::SeqCst))
        };

        // Below the limit: the reply, the summary, and the request again.
        assert_eq!(turn(limit - 1).await, ("The whole answer.".to_string(), 3));
        // At the limit the model wrote all it can.
        assert_eq!(turn(limit).await, ("This answer is cut".to_string(), 1));
    }

    #[test]
    fn model_selection_format_is_compact_and_unlabeled() {
        let model = ModelRef {
            provider: "openai-codex".to_string(),
            id: "gpt-5.5".to_string(),
        };

        assert_eq!(
            format_model_selection(&model, Some("xhigh")),
            "openai-codex/gpt-5.5 xhigh"
        );
        assert_eq!(format_model_selection(&model, None), "openai-codex/gpt-5.5");
    }

    #[tokio::test]
    async fn new_session_keeps_persisted_selections_but_starts_fresh_history() {
        let root = std::env::temp_dir().join(format!("noel-cli-new-{}", unique_temp_suffix()));
        let mut config = minimal_test_config();
        config.paths.session_dir = root.join("sessions");
        config.settings.default_thinking_level = Some("medium".into());
        config.models.push(ModelDefinition {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
            reasoning: Some(true),
            ..ModelDefinition::default()
        });
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut viewport_height = 24;
        for level in [Some("high"), Some("off"), None] {
            let (store, state) =
                SessionStore::create(&config.paths.session_dir, root.clone()).unwrap();
            let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
            runtime
                .set_active_model(Some(ModelRef {
                    provider: "anthropic".into(),
                    id: "claude-sonnet-4-6".into(),
                }))
                .unwrap();
            runtime.set_active_account(Some("work".into())).unwrap();
            runtime
                .rename_session(Some("old conversation".into()))
                .unwrap();
            runtime.set_labels(["old".into()].into()).unwrap();
            runtime.queue_message("old queue".into()).unwrap();
            runtime
                .push_message(ConversationMessage {
                    role: MessageRole::User,
                    content: "old prompt".into(),
                    thinking: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    aborted: false,
                })
                .unwrap();
            runtime
                .push_tool_event(noel_core::ToolEvent {
                    id: "old-tool".into(),
                    name: "read".into(),
                    result: "old result".into(),
                    is_error: false,
                })
                .unwrap();
            runtime.note_edited_files(vec!["old.txt".into()]).unwrap();
            runtime
                .set_todos(vec![TodoItem {
                    content: "old task".into(),
                    status: TodoStatus::Completed,
                }])
                .unwrap();
            let usage = noel_core::SessionUsage {
                input_tokens: 10,
                output_tokens: 20,
                requests: 1,
            };
            runtime
                .store()
                .unwrap()
                .record_usage(usage.clone())
                .unwrap();
            runtime.session_mut().usage = usage;
            let mut app = TuiApp::new(&config, &runtime);
            let mut surface = TuiSurface {
                terminal: &mut terminal,
                viewport_height: &mut viewport_height,
            };
            if let Some(level) = level {
                handle_tui_submission(
                    &mut app,
                    &mut surface,
                    &mut runtime,
                    &mut config,
                    true,
                    format!("/thinking {level}"),
                )
                .await
                .unwrap();
            }
            assert_eq!(runtime.session().active_thinking_level.as_deref(), level);
            let thinking_before = thinking_label(&runtime, &config);
            // An explicit off must not inherit the configured medium.
            assert_eq!(thinking_before.as_deref(), level.or(Some("medium")));
            let before = runtime.session().clone();
            let old_store = runtime.store().unwrap().clone();
            let systems = runtime.systems().clone();
            handle_tui_submission(
                &mut app,
                &mut surface,
                &mut runtime,
                &mut config,
                true,
                "/new".into(),
            )
            .await
            .unwrap();
            assert_ne!(runtime.session().session_id, before.session_id);
            let mut expected = SessionState::new(&runtime.session().session_id, root.clone());
            expected.active_model = before.active_model.clone();
            expected.active_thinking_level = before.active_thinking_level.clone();
            expected.active_account = before.active_account.clone();
            assert_eq!(runtime.session(), &expected);
            assert_eq!(runtime.systems(), &systems);
            assert_eq!(thinking_label(&runtime, &config), thinking_before);
            assert_eq!(
                SessionStore::open(old_store.path().to_path_buf())
                    .unwrap()
                    .1,
                before
            );
            let (_, reopened) =
                SessionStore::open(runtime.store().unwrap().path().to_path_buf()).unwrap();
            assert_eq!(reopened, expected);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn bare_session_commands_never_reach_the_model_or_destroy_state() {
        let root =
            std::env::temp_dir().join(format!("noel-cli-bare-commands-{}", unique_temp_suffix()));
        let mut config = minimal_test_config();
        config.paths.session_dir = root.join("sessions");
        config.models.push(ModelDefinition {
            context_window: None,
            provider: "faux".to_string(),
            id: "echo".to_string(),
            name: None,
            api: ConfigProviderApi::Faux,
            base_url: None,
            ..ModelDefinition::default()
        });
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut viewport_height = 24;
        let (store, state) = SessionStore::create(&config.paths.session_dir, root.clone()).unwrap();
        let session_file = store.path().to_path_buf();
        let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
        runtime
            .set_active_model(Some(ModelRef {
                provider: "faux".into(),
                id: "echo".into(),
            }))
            .unwrap();
        runtime.rename_session(Some("keep me".into())).unwrap();
        let mut app = TuiApp::new(&config, &runtime);
        let mut surface = TuiSurface {
            terminal: &mut terminal,
            viewport_height: &mut viewport_height,
        };

        for command in [
            "/name", "/delete", "/export", "/import", "/image", "/prompt",
        ] {
            handle_tui_submission(
                &mut app,
                &mut surface,
                &mut runtime,
                &mut config,
                true,
                command.to_string(),
            )
            .await
            .unwrap();
        }

        // None of them became a prompt, cleared the name, or deleted the
        // session; a bare /export wrote upstream's default file.
        assert!(runtime.session().messages.is_empty());
        assert_eq!(runtime.session().name.as_deref(), Some("keep me"));
        assert!(session_file.exists());
        let export = root.join(format!(
            "noel-session-{}.html",
            runtime.session().session_id
        ));
        assert!(export.exists(), "{}", export.display());
        let transcript = app.transcript_text();
        for expected in [
            "session name: keep me",
            "usage: /delete <id|name|path>",
            "exported",
            "usage: /import",
            "usage: /image",
            "usage: /prompt",
        ] {
            assert!(transcript.contains(expected), "{expected}\n{transcript}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    /// A reasoning model with this thinking level map (upstream
    /// `thinkingLevelMap`); without entries it supports `off` to `high`.
    fn thinking_test_model(provider: &str, id: &str, map: serde_json::Value) -> ModelDefinition {
        ModelDefinition {
            provider: provider.to_string(),
            id: id.to_string(),
            api: ConfigProviderApi::Anthropic,
            reasoning: Some(true),
            thinking_level_map: Some(serde_json::from_value(map).expect("level map")),
            ..ModelDefinition::default()
        }
    }

    /// A config that knows `models`, with a stored key for each of their
    /// providers. The providers are made up, so the models can run and no
    /// other can, whatever keys the test run's environment holds.
    fn thinking_test_config(models: Vec<ModelDefinition>) -> LoadedConfig {
        let mut config = minimal_test_config();
        for model in &models {
            config.auth.insert(
                &model.provider,
                DEFAULT_ACCOUNT_NAME,
                AuthCredential::ApiKey {
                    key: "test-key".to_string(),
                },
            );
        }
        config.models = models;
        config
    }

    fn test_model_ref(provider: &str, id: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_string(),
            id: id.to_string(),
        }
    }

    fn top_levels() -> serde_json::Value {
        serde_json::json!({ "xhigh": "xhigh", "max": "max" })
    }

    #[tokio::test]
    async fn thinking_and_model_commands_follow_the_models_levels() {
        let mut config = thinking_test_config(vec![
            thinking_test_model("alpha", "flash", serde_json::json!({})),
            thinking_test_model("beta", "haiku", serde_json::json!({})),
            thinking_test_model("beta", "opus", top_levels()),
            thinking_test_model(
                "beta",
                "opus-next",
                serde_json::json!({ "off": null, "xhigh": "xhigh", "max": "max" }),
            ),
        ]);
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        runtime
            .set_active_model(Some(test_model_ref("alpha", "flash")))
            .unwrap();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut viewport_height = 24;
        let mut app = TuiApp::new(&config, &runtime);
        let mut surface = TuiSurface {
            terminal: &mut terminal,
            viewport_height: &mut viewport_height,
        };
        macro_rules! submit {
            ($line:expr) => {
                handle_tui_submission(
                    &mut app,
                    &mut surface,
                    &mut runtime,
                    &mut config,
                    false,
                    $line.to_string(),
                )
                .await
            };
        }

        // A model starts on upstream's default level.
        submit!("/thinking").unwrap();
        assert!(app.transcript_text().ends_with("thinking: medium"));
        submit!("/thinking HIGH").unwrap();
        assert_eq!(
            runtime.session().active_thinking_level.as_deref(),
            Some("high")
        );
        // A level the model lacks is refused by name, as upstream refuses it.
        let error = submit!("/thinking xhigh").unwrap_err().to_string();
        assert_eq!(
            error,
            "Unknown thinking level \"xhigh\". Available levels: off, minimal, low, medium, high."
        );
        assert_eq!(
            runtime.session().active_thinking_level.as_deref(),
            Some("high")
        );

        // An exact reference switches, ignoring case, and the level in
        // effect moves with the session, clamped to the new model.
        submit!("/model Beta/Opus").unwrap();
        assert_eq!(
            runtime.session().active_model,
            Some(test_model_ref("beta", "opus"))
        );
        assert!(app.transcript_text().ends_with("beta/opus high"));
        submit!("/thinking max").unwrap();
        submit!("/model haiku").unwrap();
        assert!(app.transcript_text().ends_with("beta/haiku high"));
        // This model cannot turn thinking off.
        submit!("/model beta/opus-next").unwrap();
        let error = submit!("/thinking off").unwrap_err().to_string();
        assert!(
            error.ends_with("Available levels: minimal, low, medium, high, xhigh, max."),
            "{error}"
        );

        // Anything else opens the picker with the text as its search; the
        // session keeps its model.
        submit!("/model opu").unwrap();
        let selector = app.selector.take().expect("the model picker");
        assert_eq!(
            (selector.kind.as_str(), selector.query.as_str()),
            ("model", "opu")
        );
        assert_eq!(
            selector
                .filtered_indices
                .iter()
                .map(|index| selector.items[*index].value.as_str())
                .collect::<Vec<_>>(),
            ["beta/opus", "beta/opus-next"]
        );
        assert_eq!(
            runtime.session().active_model,
            Some(test_model_ref("beta", "opus-next"))
        );
        let _ = fs::remove_dir_all(config.paths.agent_dir.parent().unwrap());
    }

    #[test]
    fn the_model_picker_shows_and_applies_the_levels_a_model_supports() {
        let mut config = thinking_test_config(vec![
            ModelDefinition {
                reasoning: Some(false),
                ..thinking_test_model("alpha", "plain", serde_json::json!({}))
            },
            thinking_test_model("beta", "haiku", serde_json::json!({})),
            thinking_test_model("beta", "opus", top_levels()),
        ]);
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        runtime
            .set_active_model(Some(test_model_ref("beta", "opus")))
            .unwrap();
        set_thinking_level(&mut runtime, &config, "max").unwrap();
        let mut app = TuiApp::new(&config, &runtime);
        let picker = |config: &LoadedConfig, runtime: &Runtime, query: &str| {
            TuiSelectorState::new(
                "model",
                selector_for_kind(config, runtime, "model").unwrap(),
                query,
                None,
            )
            .with_model_thinking(config, runtime)
        };

        // Untouched, a row shows the level a switch to it gives: the level
        // in effect, clamped to that model.
        let mut selector = picker(&config, &runtime, "haiku");
        assert_eq!(selector.selected_thinking_level().as_deref(), Some("high"));
        // Left and right walk the model's own levels and wrap.
        selector.cycle_thinking(1);
        assert_eq!(selector.selected_thinking_level().as_deref(), Some("off"));
        selector.cycle_thinking(-2);
        assert_eq!(
            selector.selected_thinking_level().as_deref(),
            Some("medium")
        );
        apply_tui_selector_selection(&mut app, &mut runtime, &mut config, selector).unwrap();
        assert_eq!(
            runtime.session().active_model,
            Some(test_model_ref("beta", "haiku"))
        );
        assert_eq!(thinking_label(&runtime, &config).as_deref(), Some("medium"));
        assert!(app.transcript_text().ends_with("beta/haiku medium"));

        // A model without reasoning offers no level.
        let mut selector = picker(&config, &runtime, "plain");
        assert_eq!(selector.selected_thinking_level(), None);
        selector.cycle_thinking(1);
        assert_eq!(selector.thinking_level, None);
        apply_tui_selector_selection(&mut app, &mut runtime, &mut config, selector).unwrap();
        assert_eq!(thinking_label(&runtime, &config), None);
        assert!(app.transcript_text().ends_with("alpha/plain"));

        // `modelThinkingLevels` decides the level an untouched row shows.
        config
            .settings
            .model_thinking_levels
            .insert("beta/opus".to_string(), "xhigh".to_string());
        let selector = picker(&config, &runtime, "opus");
        assert_eq!(selector.selected_thinking_level().as_deref(), Some("xhigh"));
        assert_eq!(
            select_from_selector_message(&mut config, &mut runtime, "/select model opus").unwrap(),
            "beta/opus xhigh"
        );
        let _ = fs::remove_dir_all(config.paths.agent_dir.parent().unwrap());
    }

    #[test]
    fn a_model_scope_narrows_the_pickers_and_keeps_every_model_known() {
        let mut config = thinking_test_config(vec![
            thinking_test_model("alpha", "flash", serde_json::json!({})),
            thinking_test_model("beta", "haiku", serde_json::json!({})),
            thinking_test_model("beta", "opus", top_levels()),
        ]);
        let runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        let rows = |config: &LoadedConfig, kind: &str| {
            selector_for_kind(config, &runtime, kind)
                .unwrap()
                .items
                .into_iter()
                .map(|item| item.value)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(&config, "model").len(), 3);
        let cli =
            Cli::try_parse_from(["noel", "--models", "beta/op*,flash:low"]).expect("parse cli");
        apply_cli_overrides(&cli, Path::new("."), &mut config).expect("apply the scope");
        assert_eq!(rows(&config, "model"), ["alpha/flash", "beta/opus"]);
        assert_eq!(rows(&config, "provider"), ["alpha", "beta"]);
        assert_eq!(config.models.len(), 3);
        assert_eq!(config.model_scope[1].thinking_level.as_deref(), Some("low"));
        // A model outside the scope can still be named in full.
        assert_eq!(
            resolve_cli(
                &Cli::try_parse_from(["noel", "--model", "beta/haiku"]).unwrap(),
                &config,
                false
            )
            .and_then(|resolved| resolved.model)
            .map(|model| model.id),
            Some("haiku".to_string())
        );
    }

    #[tokio::test]
    async fn providers_are_built_with_the_clamped_thinking_level() {
        let config = thinking_test_config(vec![
            ModelDefinition {
                reasoning: Some(false),
                ..thinking_test_model("alpha", "plain", serde_json::json!({}))
            },
            thinking_test_model("beta", "haiku", serde_json::json!({})),
        ]);
        let mut runtime = Runtime::new(
            SessionState::new("session-1", PathBuf::from(".")),
            ReloadableSystems::default(),
        );
        runtime
            .set_active_model(Some(test_model_ref("beta", "haiku")))
            .unwrap();
        // A saved level the model lacks reaches the provider clamped.
        runtime
            .set_active_thinking_level(Some("max".to_string()))
            .unwrap();
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .expect("provider");
        assert_eq!(provider.thinking_level().as_deref(), Some("high"));
        // Off is a level too, where the model can turn thinking off.
        runtime
            .set_active_thinking_level(Some("off".to_string()))
            .unwrap();
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .expect("provider");
        assert_eq!(provider.thinking_level().as_deref(), Some("off"));
        // A model without reasoning is sent no level.
        runtime
            .set_active_model(Some(test_model_ref("alpha", "plain")))
            .unwrap();
        runtime
            .set_active_thinking_level(Some("high".to_string()))
            .unwrap();
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .expect("provider");
        assert_eq!(provider.thinking_level(), None);
    }

    /// From the session and the model definition to the request on the
    /// wire: the level clamped to the model and named as the model names
    /// it, and the model's output cap.
    #[tokio::test]
    async fn a_request_carries_the_models_thinking_level_and_output_cap() {
        use std::io::BufReader;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            (0..2)
                .map(|_| {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line.trim_end().is_empty() {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut request = vec![0; length];
                    reader.read_exact(&mut request).unwrap();
                    let body = r#"{"output_text":"ok"}"#;
                    write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    serde_json::from_slice::<serde_json::Value>(&request).unwrap()
                })
                .collect::<Vec<_>>()
        });
        let mut auth = AuthData::default();
        auth.insert(
            "local",
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::ApiKey {
                key: "local-key".to_string(),
            },
        );
        let mut config = account_test_config(auth);
        config.paths.auth_path = std::env::temp_dir().join(format!(
            "noel-cli-request-traits-{}/auth.json",
            unique_temp_suffix()
        ));
        let definition = |id: &str, extra: serde_json::Value| {
            let mut model = serde_json::json!({
                "provider": "local", "id": id, "api": "openai-responses", "baseUrl": url,
            });
            model
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<ModelDefinition>(model).unwrap()
        };
        config.models = vec![
            definition(
                "thinker",
                serde_json::json!({
                    "reasoning": true,
                    "thinkingLevelMap": { "minimal": null, "xhigh": "extra-hard" },
                    "contextWindow": 32000,
                    "maxTokens": 4000,
                }),
            ),
            definition("plain", serde_json::json!({})),
        ];
        let mut runtime = account_test_runtime("local", None);
        let request = || ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        // `max` is above what the model supports.
        runtime
            .set_active_thinking_level(Some("max".to_string()))
            .unwrap();
        for id in ["thinker", "plain"] {
            runtime
                .set_active_model(Some(test_model_ref("local", id)))
                .unwrap();
            let provider = provider_for_runtime(&runtime, &config, false)
                .await
                .unwrap();
            provider.complete(request()).await.unwrap();
        }
        let requests = server.join().unwrap();
        assert_eq!(requests[0]["model"], "thinker");
        assert_eq!(requests[0]["reasoning"]["effort"], "extra-hard");
        assert_eq!(requests[0]["max_output_tokens"], 4000);
        // Nothing is said of thinking or a cap to a model that has neither.
        assert_eq!(requests[1]["model"], "plain");
        assert_eq!(requests[1].get("reasoning"), None);
        assert_eq!(requests[1].get("max_output_tokens"), None);
    }

    #[test]
    fn reload_keeps_the_custom_model_the_command_line_named() {
        let root = std::env::temp_dir().join(format!("noel-cli-custom-{}", unique_temp_suffix()));
        let agent = root.join("agent");
        fs::create_dir_all(&agent).unwrap();
        let cli = Cli::try_parse_from(["noel", "--model", "anthropic/claude-next:high"]).unwrap();
        let source = ConfigSource {
            cli,
            cwd: root.clone(),
            project_trusted: false,
        };
        let config = source
            .load(ConfigPaths {
                cwd: root.clone(),
                agent_dir: agent.clone(),
                session_dir: agent.join("sessions"),
                settings_path: agent.join("settings.json"),
                project_settings_path: root.join(".noel/settings.json"),
                auth_path: agent.join("auth.json"),
                models_path: agent.join("models.json"),
                model_cache_path: agent.join("model-cache.json"),
                keybindings_path: agent.join("keybindings.json"),
            })
            .expect("reload config");
        let custom = configured_model(&config, &test_model_ref("anthropic", "claude-next"))
            .expect("the custom model");
        let base = noel_ai::catalog::provider_base_model("anthropic").unwrap();
        assert_eq!(custom.max_tokens, Some(base.max_tokens));
        assert_eq!(custom.name.as_deref(), Some("claude-next"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_api_key_goes_to_the_provider_the_model_resolves_to() {
        let mut config = minimal_test_config();
        config.models = noel_ai::catalog::builtin_models()
            .iter()
            .map(ModelDefinition::from)
            .collect();
        let provider = |args: &[&str]| {
            let cli =
                Cli::try_parse_from(["noel"].into_iter().chain(args.iter().copied())).unwrap();
            infer_cli_provider(&cli, &config)
        };
        assert_eq!(
            provider(&["--model", "anthropic/claude-opus-4-8"]).as_deref(),
            Some("anthropic")
        );
        // A custom id still names its provider, and so does the former
        // Azure id.
        assert_eq!(
            provider(&["--model", "mistral/next"]).as_deref(),
            Some("mistral")
        );
        assert_eq!(
            provider(&["--provider", "azure-openai-responses", "--model", "gpt-5.4"]).as_deref(),
            Some("azure")
        );
        assert_eq!(provider(&[]), None);
    }

    #[test]
    fn live_entries_are_not_finalized_until_complete() {
        let mut app = TuiApp::default();
        app.push(TuiEntryKind::User, "hello");
        let live = app.push_placeholder(TuiEntryKind::Assistant, "Working...");
        assert_eq!(live, 1);
        assert_eq!(app.finalized_entry_count(), 1);

        app.replace_entry(live, "done");
        app.finish_live_entry();
        assert_eq!(app.finalized_entry_count(), 2);
    }

    #[test]
    fn thinking_visibility_can_be_toggled_without_losing_entries() {
        let entries = vec![
            TuiEntry {
                kind: TuiEntryKind::Thinking,
                text: "summary".into(),
            },
            TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "answer".into(),
            },
        ];
        let shown =
            visible_transcript_with_thinking(&entries, 80, 24, false, &ThemePalette::default());
        let hidden =
            visible_transcript_with_thinking(&entries, 80, 24, true, &ThemePalette::default());
        assert_eq!(shown.entries_used, 2);
        assert_eq!(hidden.entries_used, 1);
        assert!(!hidden
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains("summary")));
        assert_eq!(
            visible_transcript_with_thinking(&entries, 80, 24, false, &ThemePalette::default())
                .entries_used,
            2
        );
    }

    #[test]
    fn thinking_is_dimmed_and_separate_from_answer() {
        let lines = render_entry_lines(
            &[
                TuiEntry {
                    kind: TuiEntryKind::Thinking,
                    text: "summary".into(),
                },
                TuiEntry {
                    kind: TuiEntryKind::Assistant,
                    text: "answer".into(),
                },
            ],
            &ThemePalette::default(),
        );
        let summary = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("summary"))
            .unwrap();
        assert_eq!(summary.style.fg, Some(ThemePalette::default().muted));
        assert!(summary.style.add_modifier.contains(Modifier::ITALIC));
        assert!(render_entry_lines(
            &[TuiEntry {
                kind: TuiEntryKind::Thinking,
                text: String::new()
            }],
            &ThemePalette::default()
        )
        .is_empty());
    }

    #[test]
    fn transcript_height_accounts_for_wrapped_streaming_text() {
        let lines = vec![Line::from("123456"), Line::from("ab")];
        assert_eq!(rendered_lines_height(&lines, 3), 3);
    }

    #[test]
    fn transcript_lines_drop_trailing_entry_spacer_for_viewport() {
        let mut lines = render_entry_lines(
            &[TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "done".to_string(),
            }],
            &ThemePalette::default(),
        );
        while lines.last().map(Line::width) == Some(0) {
            lines.pop();
        }

        assert_eq!(lines.last().map(Line::width), Some(6));
    }

    fn scrollback_test_terminal(
        width: u16,
        height: u16,
    ) -> Terminal<ratatui::backend::TestBackend> {
        Terminal::with_options(
            ratatui::backend::TestBackend::new(width, height),
            TerminalOptions {
                viewport: Viewport::Inline(height.saturating_sub(2).max(1)),
            },
        )
        .unwrap()
    }

    fn painted_transcript(terminal: &Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        buffer_rows(terminal.backend().scrollback())
            .into_iter()
            .chain(buffer_rows(terminal.backend().buffer()))
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    #[test]
    fn long_resumed_transcript_is_in_native_scrollback_without_a_dump() {
        let config = minimal_test_config();
        let mut app = TuiApp::default();
        let answer = (0..80)
            .map(|index| format!("answer-{index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push(TuiEntryKind::Assistant, answer.clone());
        let mut terminal = scrollback_test_terminal(40, 20);
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        let rows = painted_transcript(&terminal);
        for index in 0..80 {
            let marker = format!("answer-{index:03}");
            assert_eq!(
                rows.iter().filter(|line| line.contains(&marker)).count(),
                1,
                "{marker}"
            );
        }
        assert_eq!(app.entries[0].text, answer);
        let scrollback = terminal.backend().scrollback().clone();
        for _ in 0..5 {
            redraw_tui(&mut terminal, &mut app, &config).unwrap();
        }
        assert_eq!(terminal.backend().scrollback(), &scrollback);
        // Entry indices remain aligned after /undo-style truncation and tool insertion/removal.
        app.insert_entry(0, TuiEntryKind::Tool, "completed read\nfile\ncontents");
        app.remove_entry(0);
        assert_eq!(app.transcript_scrollback.entries.len(), 1);
        app.clear_visible();
        assert!(app.transcript_scrollback.entries.is_empty());
    }

    #[test]
    fn streamed_screenfuls_preserve_every_row_once_and_leave_the_draft_intact() {
        let config = minimal_test_config();
        let mut app = TuiApp {
            turn_running: true,
            ..Default::default()
        };
        let entry = app.push_placeholder(TuiEntryKind::Assistant, "");
        app.set_input("my draft");
        let mut terminal = scrollback_test_terminal(40, 18);
        // Include a single event larger than the viewport and several follow-on paints.
        for range in [0..70, 70..85, 85..100] {
            let delta = range
                .map(|index| format!("stream-{index:03}\n"))
                .collect::<String>();
            app.append_entry(entry, &delta);
            redraw_tui(&mut terminal, &mut app, &config).unwrap();
        }
        app.finish_live_entry();
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        let rows = painted_transcript(&terminal);
        for index in 0..100 {
            let marker = format!("stream-{index:03}");
            assert_eq!(
                rows.iter().filter(|line| line.contains(&marker)).count(),
                1,
                "{marker}"
            );
        }
        assert_eq!(app.input, "my draft");
        assert_eq!(app.input_cursor, 8);
        assert!(rows.iter().any(|line| line.contains("│ > my draft")));
    }

    #[test]
    fn scrollback_watermarks_survive_rewrap_and_appending_to_a_partial_line() {
        let config = minimal_test_config();
        let mut app = TuiApp {
            turn_running: true,
            ..Default::default()
        };
        let entry = app.push_placeholder(
            TuiEntryKind::Assistant,
            "abcdefghijklmnopqrstuvwxyz0123456789".repeat(8),
        );
        let mut terminal = scrollback_test_terminal(20, 10);
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        let before = app.transcript_scrollback.entries[entry].position;
        assert!(before.byte > 0);
        let previous = buffer_rows(terminal.backend().scrollback())
            .into_iter()
            .map(|row| row.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(!previous.is_empty());
        for (width, height) in [(33, 16), (17, 8), (50, 25)] {
            terminal.backend_mut().resize(width, height);
            terminal.resize(Rect::new(0, 0, width, height)).unwrap();
            app.append_entry(entry, "-next-界e\u{301}-");
            redraw_tui(&mut terminal, &mut app, &config).unwrap();
            // Native history is terminal-owned and may reflow differently. Check
            // logical painted text using the watermark rather than row counts.
            let watermark = app.transcript_scrollback.entries[entry].position;
            assert!(watermark >= before);
            let all = render_entry_lines(&app.entries, &ThemePalette::default());
            let pending = pending_entry_rows(
                &app.entries[entry],
                entry,
                width as usize,
                &ThemePalette::default(),
                &PreviewState::default(),
                app.transcript_scrollback.entries[entry],
            );
            assert_eq!(
                pending
                    .iter()
                    .map(|row| line_text(&row.line))
                    .collect::<String>(),
                all[0]
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()[watermark.byte..]
            );
        }
    }

    #[test]
    fn transcript_wrap_uses_terminal_cells_and_keeps_graphemes_and_styles() {
        use ratatui::style::Color;
        let style = Style::default().fg(Color::Cyan);
        let line = Line::from(vec![
            Span::raw("● "),
            Span::styled("界界e\u{301}👩‍💻\tend", style),
        ]);
        let rows = wrap_positioned_line(line, 0, 8);
        let text = rows
            .iter()
            .map(|row| line_text(&row.line))
            .collect::<String>();
        assert_eq!(text, "● 界界e\u{301}👩‍💻    end");
        assert!(rows.iter().all(|row| row.line.width() <= 8));
        assert!(rows
            .iter()
            .flat_map(|row| &row.line.spans)
            .filter(|span| !span.content.contains('●'))
            .all(|span| span.style == style));
    }

    #[test]
    fn temporary_panels_do_not_commit_hidden_transcript_rows() {
        let config = minimal_test_config();
        let mut app = TuiApp::default();
        app.push(TuiEntryKind::Assistant, "long answer\n".repeat(50));
        let mut terminal = scrollback_test_terminal(40, 20);
        app.selector = Some(TuiSelectorState::new(
            "model",
            Selector::new("model", vec![]),
            "",
            None,
        ));
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        terminal.backend().assert_scrollback_empty();
        app.selector = None;
        app.diff_panel = Some(DiffPanelState {
            files: vec![],
            selected: 0,
            detail: None,
        });
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        terminal.backend().assert_scrollback_empty();
        app.diff_panel = None;
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        assert!(terminal.backend().scrollback().area.height > 0);
    }

    #[test]
    fn transcript_render_is_bounded_while_streaming() {
        let mut app = TuiApp::default();
        for index in 0..1_000 {
            app.push(TuiEntryKind::Assistant, format!("old response {index}"));
        }
        app.push_placeholder(TuiEntryKind::Assistant, "word ".repeat(300));

        let visible = visible_transcript(&app.entries, 20, 12);

        assert_eq!(visible.entries_used, 1);
        assert!(visible.visual_height >= 12);
    }

    #[test]
    fn transcript_render_is_bounded_after_streaming() {
        let mut app = TuiApp::default();
        for index in 0..1_000 {
            app.push(TuiEntryKind::Assistant, format!("old response {index}"));
        }

        let visible = visible_transcript(&app.entries, 80, 20);

        assert!(visible.entries_used < app.entries.len());
        assert!(visible.entries_used <= 24);
        assert!(visible.visual_height >= 20);
    }

    #[test]
    fn transcript_render_keeps_latest_entry_after_wrapped_history() {
        let entries = vec![
            TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "stream".repeat(120),
            },
            TuiEntry {
                kind: TuiEntryKind::System,
                text: "queued> draft while streaming".to_string(),
            },
            TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "[faux/echo] draft while streaming".to_string(),
            },
        ];

        let visible = visible_transcript(&entries, 80, 8);
        let text = visible
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("[faux/echo] draft while streaming"));
    }

    #[test]
    fn streaming_input_key_updates_draft_while_response_is_live() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        for ch in "next prompt".chars() {
            let changed = handle_streaming_tui_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut app,
                &followups,
                &steering,
                &config,
            );
            assert_eq!(changed, StreamingKeyOutcome::Changed);
        }

        assert_eq!(app.input, "next prompt");
        assert_eq!(followups.pending(), 0);
        assert_eq!(app.entries[0].text, "streaming");
        assert_eq!(app.live_entry_index, Some(0));
    }

    #[test]
    fn streaming_enter_queues_draft_without_touching_live_response() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        app.set_input("next prompt");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        let changed = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );

        assert_eq!(changed, StreamingKeyOutcome::Changed);
        assert!(app.input.is_empty());
        assert_eq!(followups.list(), ["next prompt"]);
        assert_eq!(app.entries[0].text, "streaming");
        assert_eq!(app.live_entry_index, Some(0));
    }

    #[test]
    fn streaming_steer_sends_draft_to_the_mailbox() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        app.set_input("change course");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        let changed = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &followups,
            &steering,
            &config,
        );

        assert_eq!(changed, StreamingKeyOutcome::Changed);
        assert!(app.input.is_empty());
        assert_eq!(followups.pending(), 0);
        assert_eq!(steering.drain(), ["change course"]);
        assert_eq!(app.entries[1].text, "steering> change course");
        assert_eq!(app.live_entry_index, Some(0));
    }

    #[test]
    fn streaming_steer_with_empty_input_does_nothing() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        let changed = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &followups,
            &steering,
            &config,
        );

        assert_eq!(changed, StreamingKeyOutcome::Changed);
        assert_eq!(steering.pending(), 0);
        assert_eq!(app.entries.len(), 1);
    }

    #[test]
    fn streaming_ctrl_c_interrupts_without_arming_quit() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        for _ in 0..2 {
            let outcome = handle_streaming_tui_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut app,
                &followups,
                &steering,
                &config,
            );
            assert_eq!(outcome, StreamingKeyOutcome::Interrupt);
        }
    }

    #[test]
    fn auto_restart_debounces_binary_changes() {
        let dir =
            std::env::temp_dir().join(format!("noel-autorestart-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let exe = dir.join("noel");
        fs::write(&exe, b"v1").expect("write binary");
        let original = fs::metadata(&exe)
            .expect("metadata")
            .modified()
            .expect("mtime");
        let mut restart = AutoRestart {
            executable: exe.clone(),
            modified: Some(original),
            pending: None,
        };

        assert!(!restart.should_restart().expect("check"));

        let rebuilt = original + Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(&exe)
            .expect("open")
            .set_modified(rebuilt)
            .expect("set mtime");
        assert!(!restart.should_restart().expect("first sighting"));
        assert!(!restart.should_restart().expect("within debounce"));

        restart.pending = restart
            .pending
            .map(|(mtime, since)| (mtime, since - RESTART_DEBOUNCE));
        assert!(restart.should_restart().expect("after debounce"));

        let rebuilt_again = rebuilt + Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(&exe)
            .expect("open")
            .set_modified(rebuilt_again)
            .expect("set mtime");
        assert!(!restart.should_restart().expect("new build resets debounce"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resume_hint_flags_interrupted_turns_and_pending_queue() {
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        assert!(
            resume_hint(&Runtime::new(session.clone(), ReloadableSystems::default())).is_none()
        );

        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Tool,
            content: "partial".to_string(),
            media: Vec::new(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("bash".to_string()),
            tool_calls: Vec::new(),
            aborted: false,
        });
        let hint = resume_hint(&Runtime::new(session.clone(), ReloadableSystems::default()))
            .expect("mid-turn hint");
        assert!(hint.contains("mid-turn"));
        assert!(hint.contains("continuing automatically"));

        session.queued_messages = vec!["follow up".to_string()];
        let hint = resume_hint(&Runtime::new(session, ReloadableSystems::default()))
            .expect("mid-turn and queue hint");
        assert!(hint.contains("mid-turn"));
        assert!(hint.contains("1 queued"));
    }

    #[test]
    fn resume_hint_distinguishes_model_work_from_completed_direct_tools() {
        let mut session = SessionState::new("resume", PathBuf::from("."));
        let mut message = ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "prompt".into(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        };
        for (role, call_id, calls, unfinished) in [
            (MessageRole::User, None, Vec::new(), true),
            (MessageRole::Assistant, None, Vec::new(), false),
            (
                MessageRole::Assistant,
                None,
                vec![noel_ai::ChatToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"fixture.txt"}"#.into(),
                }],
                true,
            ),
            (MessageRole::Tool, Some("call-1".into()), Vec::new(), true),
            (MessageRole::Tool, None, Vec::new(), false),
            (MessageRole::System, None, Vec::new(), false),
        ] {
            message.role = role;
            message.tool_call_id = call_id;
            message.tool_calls = calls;
            session.messages = vec![message.clone()];
            let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
            let hint = resume_hint(&runtime);
            assert_eq!(hint.is_some(), unfinished);
            let config = minimal_test_config();
            let mut app = TuiApp::new(&config, &runtime);
            assert_eq!(app.resume_pending, unfinished);
            app.resume_pending = false;
            app.refresh_chrome(&config, &runtime);
            assert!(!app.resume_pending); // Idle refresh must never re-arm a stopped run.
        }
        session.messages[0].role = MessageRole::Tool;
        session.queued_messages = vec!["pending".into()];
        let hint = resume_hint(&Runtime::new(session, ReloadableSystems::default())).unwrap();
        assert!(hint.contains("1 queued"));
        assert!(!hint.contains("mid-turn"));
    }

    #[tokio::test]
    async fn completed_slash_read_reopens_without_mid_turn_warning() {
        let root =
            std::env::temp_dir().join(format!("noel-direct-resume-{}", unique_temp_suffix()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("fixture.txt"), "complete output").unwrap();
        let (store, state) = SessionStore::create(&root.join("sessions"), root.clone()).unwrap();
        let path = store.path().to_path_buf();
        let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
        let provider = noel_ai::create_provider(ProviderConfig {
            model: ModelRef {
                provider: "faux".into(),
                id: "echo".into(),
            },
            api: AiProviderApi::Faux,
            base_url: None,
            auth: ProviderAuth::None,
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: noel_ai::ModelTraits::default(),
        });
        let output = run_user_turn(&mut runtime, provider.as_ref(), "/read fixture.txt".into())
            .await
            .unwrap();
        assert_eq!(output, "complete output");
        assert_eq!(
            runtime.session().messages.last().unwrap().role,
            MessageRole::Tool
        );
        let (store, state) = SessionStore::open(path).unwrap();
        assert!(resume_hint(&Runtime::with_store(
            state,
            ReloadableSystems::default(),
            store
        ))
        .is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn streaming_esc_interrupts_the_turn() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );

        assert_eq!(outcome, StreamingKeyOutcome::Interrupt);
    }

    #[test]
    fn streaming_slash_commands_act_on_the_pending_followups() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let mut config = minimal_test_config();
        config.models.push(ModelDefinition {
            provider: "faux".into(),
            id: "echo".into(),
            name: None,
            api: noel_config::ProviderApi::Faux,
            base_url: None,
            context_window: None,
            ..ModelDefinition::default()
        });
        followups.push("first follow-up".to_string());

        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(outcome, StreamingKeyOutcome::Changed);

        app.set_input("/queue");
        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(outcome, StreamingKeyOutcome::Changed);
        assert!(app
            .entries
            .iter()
            .any(|entry| entry.text.contains("1. first follow-up")));

        app.set_input("/queue-clear");
        handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(followups.pending(), 0);

        app.set_input("/model");
        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        // /model now opens the two-layer provider picker over the running
        // turn instead of being rejected.
        assert_eq!(outcome, StreamingKeyOutcome::Changed);
        assert!(app.selector.is_some());
        assert_eq!(app.selector.as_ref().unwrap().kind, "provider");

        // A direct reference queues a switch that applies after the turn.
        app.selector = None;
        app.set_input("/model faux/echo");
        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(
            outcome,
            StreamingKeyOutcome::SwitchModel("faux/echo".into())
        );

        app.set_input("/interrupt");
        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(outcome, StreamingKeyOutcome::Interrupt);

        app.set_input("/quit");
        let outcome = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(outcome, StreamingKeyOutcome::Quit);
    }

    #[test]
    fn compaction_progress_keeps_live_entries_and_uses_audit_history_after_context_shrinks() {
        let mut app = TuiApp::default();
        let thinking = app.entries.len();
        app.entries.push(TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: String::new(),
        });
        let entry = app.push_placeholder(TuiEntryKind::Assistant, "Working...");
        let mut activity = Activity::Waiting;
        let mut saw_delta = false;
        let mut live_text_index = entry;
        let mut text_after_tool = false;
        apply_turn_event(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            thinking,
            &mut saw_delta,
            &mut activity,
            &TurnEvent::CompactionStarted,
        );
        assert!(activity_status(Instant::now(), &activity, 0).contains("compacting"));
        apply_turn_event(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            thinking,
            &mut saw_delta,
            &mut activity,
            &TurnEvent::CompactionFinished(noel_core::CompactionRecord {
                kind: CompactionKind::Automatic,
                omitted_messages: 100,
                retained_messages: 2,
                summary: "checkpoint".into(),
            }),
        );
        apply_turn_event(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            thinking,
            &mut saw_delta,
            &mut activity,
            &TurnEvent::Provider(StreamEvent::Text("done".into())),
        );
        assert_eq!(app.entries[entry].text, "done");
        assert_eq!(app.live_entry_index, Some(entry));
        let mut session = SessionState::new("audit", PathBuf::from("."));
        session.tool_history.push(noel_core::ToolEvent {
            id: "old".into(),
            name: "read".into(),
            result: "old".into(),
            is_error: false,
        });
        session.tool_history.push(noel_core::ToolEvent {
            id: "new".into(),
            name: "bash".into(),
            result: "test failed".into(),
            is_error: true,
        });
        let runtime = Runtime::new(session, ReloadableSystems::default());
        settle_tool_entries(&mut app, &runtime, 1, entry);
        assert_eq!(app.entries[entry].text, "failed bash\n\ntest failed");
        assert_eq!(app.entries[entry + 1].text, "done");
    }

    #[test]
    fn activity_status_shows_phase_and_pending_followups() {
        let started = Instant::now();
        let waiting = activity_status(started, &Activity::Waiting, 0);
        assert!(waiting.contains("waiting"));
        assert!(waiting.contains("esc to interrupt"));
        assert!(!waiting.contains("queued"));

        let tool = activity_status(started, &Activity::Tool("bash".to_string()), 2);
        assert!(tool.contains("running bash"));
        assert!(tool.contains("+2 queued"));
    }

    #[test]
    fn prompt_history_navigates_with_draft_restore() {
        let mut app = TuiApp::default();
        app.editor_state.record_history("first");
        app.editor_state.record_history("second");
        app.set_input("draft");

        app.history_previous();
        assert_eq!(app.input, "second");
        app.history_previous();
        assert_eq!(app.input, "first");
        app.history_previous();
        assert_eq!(app.input, "first");

        app.history_next();
        assert_eq!(app.input, "second");
        app.history_next();
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn restored_session_user_messages_populate_prompt_history() {
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "first prompt".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        session.messages.push(ConversationMessage {
            thinking: "saved summary".into(),
            role: MessageRole::Assistant,
            content: "first response".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "second prompt".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        let runtime = Runtime::new(session, ReloadableSystems::default());
        let mut app = TuiApp::default();

        app.restore_session_messages(&runtime);
        assert_eq!(app.entries[1].kind, TuiEntryKind::Thinking);
        assert_eq!(app.entries[1].text, "saved summary");
        assert_eq!(app.entries[2].text, "first response");
        app.history_previous();
        assert_eq!(app.input, "second prompt");
        app.history_previous();
        assert_eq!(app.input, "first prompt");
    }

    #[test]
    fn bracketed_paste_appends_to_input_and_exits_history_navigation() {
        let mut app = TuiApp::default();
        app.editor_state.record_history("old prompt");
        app.history_previous();
        assert_eq!(app.input, "old prompt");

        app.paste_text(" pasted");
        assert_eq!(app.input, "old prompt pasted");
        assert_eq!(input_area_height(&app, Rect::new(0, 0, 80, 20), 0), 3);
        app.history_next();
        assert_eq!(app.input, "old prompt pasted");
    }

    #[test]
    fn shift_enter_adds_newline_to_input() {
        let mut app = TuiApp::default();
        app.set_input("first");
        app.editor_state.record_history("old prompt");
        app.history_previous();
        assert_eq!(app.input, "old prompt");

        app.set_input("first");
        app.insert_input_newline();
        app.insert_input_str("second");

        assert_eq!(app.input, "first\nsecond");
        let bindings = keybinding_map(&minimal_test_config());
        assert!(bindings.matches("newline", "shift+enter"));
        assert!(bindings.matches("newline", "ctrl+j"));
        assert!(!bindings.matches("newline", "enter"));
        app.history_next();
        assert_eq!(app.input, "first\nsecond");
    }

    #[tokio::test]
    async fn idle_newline_keys_insert_at_cursor_without_submitting_or_opening_diff() {
        let mut config = minimal_test_config();
        let mut runtime = account_test_runtime("faux", None);
        let before = runtime.session().clone();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let mut viewport_height = 24;
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            for draft in ["", "firstsecond"] {
                let mut app = TuiApp {
                    diff_panel: Some(DiffPanelState {
                        files: vec![DiffFileEntry {
                            path: "unused.rs".into(),
                            added: None,
                            removed: None,
                            untracked: true,
                        }],
                        selected: 0,
                        detail: None,
                    }),
                    quit_armed_since: Some(Instant::now()),
                    ..Default::default()
                };
                app.set_input(draft);
                app.input_cursor = draft.len().min(5);
                let quit = handle_tui_key(
                    key,
                    &mut TuiSurface {
                        terminal: &mut terminal,
                        viewport_height: &mut viewport_height,
                    },
                    &mut app,
                    &mut runtime,
                    &mut config,
                    true,
                )
                .await
                .unwrap();
                assert!(!quit);
                assert_eq!(
                    app.input,
                    if draft.is_empty() {
                        "\n"
                    } else {
                        "first\nsecond"
                    }
                );
                assert_eq!(app.input_cursor, if draft.is_empty() { 1 } else { 6 });
                assert_eq!(input_desired_height(&app, 80), 4);
                assert!(app.quit_armed_since.is_none());
                assert!(app.diff_panel.as_ref().unwrap().detail.is_none());
                assert!(app.entries.is_empty());
                assert!(app.editor_state.history().is_empty());
                assert_eq!(runtime.session(), &before);
            }
        }
    }

    #[test]
    fn streaming_shift_enter_and_ctrl_j_add_newlines_without_queueing() {
        let config = minimal_test_config();
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            let mut app = TuiApp::default();
            app.push_placeholder(TuiEntryKind::Assistant, "streaming answer");
            app.set_input("firstsecond");
            app.input_cursor = 5;
            let followups = FollowUpQueue::default();
            let steering = SteeringMailbox::default();
            assert_eq!(
                handle_streaming_tui_key(key, &mut app, &followups, &steering, &config),
                StreamingKeyOutcome::Changed
            );
            assert_eq!(app.input, "first\nsecond");
            assert_eq!(app.input_cursor, 6);
            assert_eq!(input_desired_height(&app, 80), 4);
            assert_eq!(followups.pending(), 0);
            assert_eq!(steering.pending(), 0);
            assert!(app.editor_state.history().is_empty());
            assert_eq!(app.entries[0].text, "streaming answer");
            assert_eq!(app.live_entry_index, Some(0));
            handle_streaming_tui_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut app,
                &followups,
                &steering,
                &config,
            );
            assert!(app.input.is_empty());
            assert_eq!(followups.list(), ["first\nsecond"]);
        }
    }

    #[test]
    fn single_line_input_is_centered_between_rounded_borders() {
        use ratatui::backend::TestBackend;
        let mut app = TuiApp {
            show_hardware_cursor: true,
            ..Default::default()
        };
        app.set_input("draft");
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| {
                draw_input(frame, frame.area(), &app, &ThemePalette::default());
                set_tui_cursor(frame, frame.area(), &app);
            })
            .unwrap();
        assert_eq!(
            buffer_rows(terminal.backend().buffer()),
            [
                "╭──────────────────╮",
                "│ > draft          │",
                "╰──────────────────╯",
            ]
        );
        terminal.backend_mut().assert_cursor_position((9, 1));
    }

    #[test]
    fn typed_newlines_expand_input_area_and_pastes_show_numbered_markers() {
        let mut app = TuiApp::default();
        app.set_input("one");
        for line in ["two", "three", "four"] {
            app.insert_input_newline();
            for ch in line.chars() {
                app.push_input_char(ch);
            }
        }
        assert_eq!(input_layout(&app, 76).rows.len(), 4);
        assert_eq!(input_area_height(&app, Rect::new(0, 0, 80, 20), 0), 6);
        let layout = input_layout(&app, 76);
        let lines = render_input_lines(
            &app,
            Rect::new(0, 0, 76, 4),
            Style::default(),
            Style::default(),
        );
        assert_eq!(layout.cursor_row, 3);
        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            ["> one", "  two", "  three", "  four",]
        );

        let mut pasted = TuiApp::default();
        pasted.paste_text("one\ntwo\nthree\nfour");
        assert_eq!(input_layout(&pasted, 76).rows.len(), 1);
        assert_eq!(input_area_height(&pasted, Rect::new(0, 0, 80, 20), 0), 3);
        let lines = render_input_lines(
            &pasted,
            Rect::new(0, 0, 76, 1),
            Style::default(),
            Style::default(),
        );
        assert_eq!(line_text(&lines[0]), "> [paste #1 +4 lines]");
        assert_eq!(pasted.input, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn boxed_editor_wraps_and_grows_upward_then_shrinks_on_resize() {
        use ratatui::backend::TestBackend;
        let mut app = TuiApp {
            show_hardware_cursor: true,
            ..Default::default()
        };
        app.set_input("abcdefghijklmnop");
        let config = minimal_test_config();
        for (width, height, expected_rows, cursor) in [
            (
                12,
                5,
                vec!["│ > abcdef │", "│   ghijkl │", "│   mnop   │"],
                (8, 7),
            ),
            (24, 3, vec!["│ > abcdefghijklmnop   │"], (20, 7)),
        ] {
            assert_eq!(input_desired_height(&app, width), height);
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            terminal
                .draw(|frame| draw_tui(frame, &app, &config))
                .unwrap();
            let rows = buffer_rows(terminal.backend().buffer());
            assert_eq!(&rows[(10 - height) as usize..8], expected_rows.as_slice());
            assert!(rows[(9 - height) as usize].starts_with('╭'));
            assert!(rows[8].starts_with('╰'));
            terminal.backend_mut().assert_cursor_position(cursor);
        }
        app.cursor_to_line_start();
        let lines = render_input_lines(
            &app,
            Rect::new(0, 0, 8, 1),
            Style::default(),
            Style::default(),
        );
        assert_eq!(line_text(&lines[0]), "> abcdef");
        app.set_input("abc界界界e\u{301}");
        let mut terminal = Terminal::new(TestBackend::new(12, 4)).unwrap();
        terminal
            .draw(|frame| {
                draw_input(frame, frame.area(), &app, &ThemePalette::default());
                set_tui_cursor(frame, frame.area(), &app);
            })
            .unwrap();
        assert_eq!(
            buffer_rows(terminal.backend().buffer()),
            [
                "╭──────────╮",
                "│ > abc界  │",
                "│   界界e\u{301}  │",
                "╰──────────╯"
            ]
        );
        terminal.backend_mut().assert_cursor_position((9, 2));
    }

    #[test]
    fn editor_wrap_reserves_an_insertion_cell_and_preserves_graphemes() {
        let mut app = TuiApp::default();
        app.set_input("abcdef");
        let layout = input_layout(&app, 8);
        assert_eq!(layout.rows, ["abcdef", ""]);
        assert_eq!((layout.cursor_row, layout.cursor_column), (1, 0));
        app.move_cursor_left();
        let layout = input_layout(&app, 8);
        assert_eq!((layout.cursor_row, layout.cursor_column), (0, 5));
        app.set_input("ab界e\u{301}界\u{301}xy");
        let layout = input_layout(&app, 8);
        assert_eq!(layout.rows, ["ab界e\u{301}", "界\u{301}xy"]);
        app.move_cursor_left();
        app.move_cursor_left();
        app.move_cursor_left();
        assert_eq!(&app.input[app.input_cursor..], "界\u{301}xy");
        app.move_cursor_right();
        app.pop_input_char();
        assert_eq!(app.input, "ab界e\u{301}xy");
        assert_eq!(input_layout(&app, 8).rows, ["ab界e\u{301}x", "y"]);
    }

    #[test]
    fn tall_drafts_are_bounded_and_scroll_to_the_cursor() {
        let mut app = TuiApp::default();
        app.set_input(
            (0..50)
                .map(|index| format!("row {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(input_desired_height(&app, 80), MAX_INPUT_ROWS as u16 + 2);
        assert_eq!(input_area_height(&app, Rect::new(0, 0, 80, 6), 0), 4);
        let visible = |app: &TuiApp| {
            render_input_lines(
                app,
                Rect::new(0, 0, 76, 2),
                Style::default(),
                Style::default(),
            )
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
        };
        assert_eq!(visible(&app), ["> row 48", "  row 49"]);
        app.input_cursor = 0;
        assert_eq!(visible(&app), ["> row 0", "  row 1"]);
        assert_eq!(app.input.lines().count(), 50);
    }

    #[test]
    fn large_paste_markers_keep_full_text_and_allow_surrounding_edits() {
        let mut app = TuiApp::default();
        let paste = (0..500)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        app.insert_input_str("before  after");
        app.input_cursor = 7;
        app.paste_text(&paste);
        assert_eq!(app.input, format!("before {paste} after"));
        assert_eq!(app.display_input().0, "before [paste #1 +500 lines] after");
        assert_eq!(input_desired_height(&app, 80), 3);
        app.insert_input_str("!");
        app.move_cursor_left();
        app.move_cursor_left();
        assert_eq!(app.input_cursor, 7);
        assert_eq!(app.display_input().1, 7);
        app.insert_input_str("prefix ");
        assert_eq!(
            app.display_input().0,
            "before prefix [paste #1 +500 lines]! after"
        );
        app.cursor_to_line_end();
        assert_eq!(app.input_cursor, app.input.len());
        app.cursor_to_line_start();
        assert_eq!(app.input_cursor, 0);
        app.input_cursor = app.input_pastes[0].start;
        app.move_cursor_right();
        app.pop_input_char();
        assert_eq!(app.input, "before prefix ! after");
        assert!(app.input_pastes.is_empty());
    }

    #[test]
    fn multiple_pastes_keep_ids_and_ranges_when_deleted_or_cleared() {
        let mut app = TuiApp::default();
        app.paste_text("one\ntwo");
        app.insert_input_str(" / ");
        app.paste_text("three\nfour\nfive");
        app.cursor_to_line_start();
        app.paste_text("first\nsecond");
        assert_eq!(
            app.display_input().0,
            "[paste #3 +2 lines][paste #1 +2 lines] / [paste #2 +3 lines]"
        );
        app.delete_input_char();
        assert_eq!(
            app.display_input().0,
            "[paste #3 +2 lines] / [paste #2 +3 lines]"
        );
        app.move_cursor_left();
        app.delete_input_char();
        assert_eq!(app.display_input().0, " / [paste #2 +3 lines]");
        app.clear_input();
        app.paste_text("new\npaste");
        assert_eq!(app.display_input().0, "[paste #4 +2 lines]");
        app.clear_input();
        assert_eq!(app.display_input(), (String::new(), 0));
    }

    #[test]
    fn paste_normalizes_terminal_newlines_and_compacts_long_single_lines() {
        let mut app = TuiApp::default();
        app.paste_text("first\r\nsecond\rthird");
        assert_eq!(app.input, "first\nsecond\nthird");
        assert_eq!(app.display_input().0, "[paste #1 +3 lines]");
        app.clear_input();
        let text = "界".repeat(LARGE_PASTE_CHARS + 1);
        app.paste_text(&text);
        assert_eq!(app.input, text);
        assert_eq!(app.display_input().0, "[paste #2 +1 line]");
        app.clear_input();
        app.paste_text("");
        app.paste_text("short paste");
        assert!(app.input_pastes.is_empty());
        assert_eq!(app.display_input().0, "short paste");
    }

    #[test]
    fn history_navigation_restores_paste_markers_and_the_draft_cursor() {
        let mut app = TuiApp::default();
        app.editor_state.record_history("old prompt");
        app.insert_input_str("draft ");
        app.paste_text("one\ntwo");
        app.move_cursor_left();
        let before = app.display_input();
        let cursor = app.input_cursor;
        app.history_previous();
        assert_eq!(app.display_input().0, "old prompt");
        app.history_next();
        assert_eq!(app.display_input(), before);
        assert_eq!(app.input_cursor, cursor);
        assert_eq!(app.input, "draft one\ntwo");
    }

    #[test]
    fn streaming_paste_submissions_and_queue_recall_use_full_text() {
        let config = minimal_test_config();
        for key in [KeyCode::Enter, KeyCode::Char('s')] {
            let mut app = TuiApp::default();
            let followups = FollowUpQueue::default();
            let steering = SteeringMailbox::default();
            let paste = "full pasted text\n".repeat(500);
            app.insert_input_str("prefix ");
            app.paste_text(&paste);
            app.insert_input_str(" suffix");
            let expected = app.input.trim().to_string();
            let modifiers = if key == KeyCode::Enter {
                KeyModifiers::NONE
            } else {
                KeyModifiers::CONTROL
            };
            handle_streaming_tui_key(
                KeyEvent::new(key, modifiers),
                &mut app,
                &followups,
                &steering,
                &config,
            );
            let submitted = if key == KeyCode::Enter {
                followups.list()
            } else {
                steering.list()
            };
            assert_eq!(submitted.as_slice(), std::slice::from_ref(&expected));
            assert!(app.input.is_empty());
            assert!(app.input_pastes.is_empty());
            recall_queued_message(&mut app, submitted[0].clone());
            assert_eq!(app.input, expected);
            assert!(!app.display_input().0.contains("[paste"));
        }
    }

    fn buffer_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| {
                        // Wide graphemes own the following cell, which is a blank
                        // placeholder in ratatui's buffer, not an extra character.
                        let cell = &buffer[(x, y)];
                        if x > 0 && buffer[(x - 1, y)].symbol().width() > 1 {
                            None
                        } else {
                            Some(cell.symbol())
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn clear_visible_resets_transient_tui_state_only() {
        let mut app = TuiApp {
            multiline: Some(vec!["one".to_string()]),
            live_entry_index: Some(0),
            ..TuiApp::default()
        };
        app.set_input("draft");
        app.push(TuiEntryKind::User, "hello");
        app.editor_state.record_history("old prompt");
        app.history_previous();

        app.clear_visible();

        assert!(app.entries.is_empty());
        assert!(app.input.is_empty());
        assert!(app.multiline.is_none());
        assert!(app.selector.is_none());
        assert!(app.live_entry_index.is_none());
        assert_eq!(input_layout(&app, 76).rows.len(), 1);
        assert_eq!(app.editor_state.history(), ["old prompt"]);
    }

    #[test]
    fn slash_command_matches_follow_current_input() {
        let config = LoadedConfig {
            paths: test_config_paths(),
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
            model_scope: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: vec![test_resource("json-ext")],
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            themes: Vec::new(),
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        };
        let mut app = TuiApp::default();
        app.set_input("/cl");

        let matches = slash_command_matches(&config, &app);
        assert!(matches.contains(&"/clear".to_string()));
        assert!(matches.contains(&"/clone [id|name|path]".to_string()));

        app.set_input("not slash");
        assert!(slash_command_matches(&config, &app).is_empty());

        app.set_input("/extension:j");
        assert_eq!(
            slash_command_matches(&config, &app),
            vec!["/extension:json-ext".to_string()]
        );
    }

    #[test]
    fn model_tool_messages_render_before_final_assistant_entry() {
        let mut app = TuiApp::default();
        app.push(TuiEntryKind::User, "read a file");
        let assistant_index = app.push_placeholder(TuiEntryKind::Assistant, "done");
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "read a file".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Tool,
            content: "file contents".to_string(),
            media: Vec::new(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("read".to_string()),
            tool_calls: Vec::new(),
            aborted: false,
        });
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Assistant,
            content: "done".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        session.tool_history.push(noel_core::ToolEvent {
            id: "call_1".into(),
            name: "read".into(),
            result: "file contents".into(),
            is_error: false,
        });
        let runtime = Runtime::new(session, ReloadableSystems::default());

        settle_tool_entries(&mut app, &runtime, 0, assistant_index);

        assert_eq!(app.entries[1].kind, TuiEntryKind::Tool);
        assert_eq!(app.entries[1].text, "completed read\n\nfile contents");
        assert_eq!(app.entries[2].kind, TuiEntryKind::Assistant);
        assert_eq!(app.entries[2].text, "done");
    }

    #[test]
    fn dogfood_restart_drops_model_cli_overrides() {
        let args = [
            "--continue",
            "--model",
            "faux/echo",
            "--provider=openai",
            "--theme",
            "dark",
        ]
        .into_iter()
        .map(OsString::from);

        let filtered = restart_args(args, None);

        assert_eq!(
            filtered,
            ["--continue", "--theme", "dark"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn dogfood_restart_reopens_the_session_it_had_open() {
        let rewrite = |args: &[&str]| {
            restart_args(
                args.iter().map(OsString::from),
                Some(Path::new("/sessions/current.jsonl")),
            )
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect::<Vec<_>>()
        };
        let reopen = ["--session", "/sessions/current.jsonl"];

        // `make dogfood` runs `--continue --model faux/echo`.
        assert_eq!(rewrite(&["--continue", "--model", "faux/echo"]), reopen);
        for selection in [
            &["-c"][..],
            &["-r"],
            &["--resume"],
            &["--fork", "abc"],
            &["--fork=abc"],
            &["--session", "older.jsonl"],
            &["--session=older.jsonl"],
            &["--session-id", "named"],
            &["--session-id=named"],
            &["--no-session"],
        ] {
            assert_eq!(rewrite(selection), reopen, "{selection:?}");
        }
        assert_eq!(
            rewrite(&[
                "-cpn",
                "label",
                "--session-dir",
                "sessions",
                "--theme",
                "dark",
                "-rt",
                "read",
                "--",
                "-c",
            ]),
            [
                "-pn",
                "label",
                "--session-dir",
                "sessions",
                "--theme",
                "dark",
                "-t",
                "read",
                "--session",
                "/sessions/current.jsonl",
                "--",
                "-c",
            ]
        );
        // Without a session file (`--no-session`) the selection stays.
        assert_eq!(
            restart_args(
                ["--no-session", "--model", "faux/echo"].map(OsString::from),
                None
            ),
            [OsString::from("--no-session")]
        );
    }

    /// Paths in a fresh directory under the system temp dir, so a test never
    /// reads or writes the real `~/.noel` or the repository (nothing is
    /// created until a test writes).
    fn test_config_paths() -> ConfigPaths {
        let root = std::env::temp_dir().join(format!("noel-cli-unit-{}", unique_temp_suffix()));
        let agent_dir = root.join("agent");
        ConfigPaths {
            cwd: PathBuf::from("."),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: root.join("project/.noel/settings.json"),
            auth_path: agent_dir.join("auth.json"),
            models_path: agent_dir.join("models.json"),
            model_cache_path: agent_dir.join("model-cache.json"),
            keybindings_path: agent_dir.join("keybindings.json"),
            agent_dir,
        }
    }

    pub(super) fn minimal_test_config() -> LoadedConfig {
        LoadedConfig {
            paths: test_config_paths(),
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
            model_scope: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: Vec::new(),
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            themes: Vec::new(),
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        }
    }

    #[test]
    fn input_cursor_inserts_and_deletes_at_cursor() {
        let mut app = TuiApp::default();
        for ch in "helo".chars() {
            app.push_input_char(ch);
        }
        app.move_cursor_left();
        app.push_input_char('l');
        assert_eq!(app.input, "hello");
        assert_eq!(app.input_cursor, 4);

        app.cursor_to_line_start();
        app.push_input_char('>');
        assert_eq!(app.input, ">hello");

        app.cursor_to_line_end();
        app.move_cursor_left();
        app.pop_input_char();
        assert_eq!(app.input, ">helo");
        assert_eq!(app.input_cursor, 4);
    }

    #[test]
    fn line_navigation_respects_newlines() {
        let mut app = TuiApp::default();
        app.set_input("first\nsecond");

        app.cursor_to_line_start();
        assert_eq!(app.input_cursor, 6);
        app.move_cursor_left();
        assert_eq!(app.input_cursor, 5);
        app.cursor_to_line_start();
        assert_eq!(app.input_cursor, 0);
        app.cursor_to_line_end();
        assert_eq!(app.input_cursor, 5);
        app.move_cursor_right();
        app.cursor_to_line_end();
        assert_eq!(app.input_cursor, 12);
    }

    #[test]
    fn quit_requires_double_press_within_window() {
        let mut app = TuiApp::default();
        let now = Instant::now();

        assert!(!app.quit_requested(now));
        assert!(app.quit_requested(now + Duration::from_secs(1)));
    }

    #[test]
    fn quit_arm_does_not_touch_the_draft() {
        let mut app = TuiApp::default();
        app.set_input("draft");

        assert!(!app.quit_requested(Instant::now()));
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn quit_arm_expires() {
        let mut app = TuiApp::default();
        let now = Instant::now();

        assert!(!app.quit_requested(now));
        assert!(!app.quit_requested(now + QUIT_CONFIRM_WINDOW));
    }

    #[test]
    fn key_event_name_formats_modifiers() {
        assert_eq!(
            key_event_name(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some("ctrl+a".to_string())
        );
        assert_eq!(
            key_event_name(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            Some("home".to_string())
        );
        assert_eq!(
            key_event_name(&KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER)),
            Some("super+left".to_string())
        );
        assert_eq!(
            key_event_name(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some("shift+enter".to_string())
        );
    }

    #[test]
    fn persist_default_model_writes_settings_and_preserves_other_keys() {
        let root = std::env::temp_dir().join(format!(
            "noel-persist-model-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let settings_path = root.join("settings.json");
        fs::write(&settings_path, "{\n  \"theme\": \"dark\"\n}\n").expect("seed settings");
        let mut config = LoadedConfig {
            paths: ConfigPaths {
                settings_path: settings_path.clone(),
                ..test_config_paths()
            },
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
            model_scope: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: Vec::new(),
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            themes: Vec::new(),
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        };
        let model = ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
        };

        persist_default_model(&mut config, &model).expect("persist default model");

        assert_eq!(
            config.settings.default_provider.as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            config.settings.default_model.as_deref(),
            Some("claude-sonnet-4-6")
        );
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");
        assert_eq!(written["defaultProvider"], "anthropic");
        assert_eq!(written["defaultModel"], "claude-sonnet-4-6");
        assert_eq!(written["theme"], "dark");
        let _ = fs::remove_dir_all(root);
    }

    pub(super) fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn model_lists_are_grouped_by_provider_and_id() {
        let models = vec![
            test_model("openai-codex", "gpt-5.6-terra-wm"),
            test_model("anthropic", "claude-sonnet-4-6"),
            test_model("openai-codex", "gpt-5.6-luna-wm"),
        ];

        let sorted = sorted_models(&models)
            .into_iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect::<Vec<_>>();

        assert_eq!(
            sorted,
            [
                "anthropic/claude-sonnet-4-6",
                "openai-codex/gpt-5.6-luna-wm",
                "openai-codex/gpt-5.6-terra-wm",
            ]
        );
    }

    #[test]
    fn codex_discovery_uses_selectable_model_slugs_only() {
        let response = serde_json::json!({
            "models": [
                {"slug": "gpt-5.6-terra", "visibility": "list"},
                {"slug": "gpt-6-sol", "visibility": "list"},
                {"slug": "gpt-5.6-luna", "visibility": "list"},
                {"slug": "codex-auto-review", "visibility": "hide"},
                {"slug": "gpt-5.6-sol-wm", "visibility": "hide"},
            ],
            "unrelated": {"id": "gpt-5.6-terra-wm"},
        });

        assert_eq!(
            collect_codex_model_ids(&response, "openai-codex"),
            BTreeSet::from([
                "gpt-5.6-luna".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-6-sol".to_string(),
            ])
        );
    }

    #[test]
    fn model_filter_admits_new_generation_ids() {
        assert!(model_supported_for_provider("openai", "gpt-6-sol"));
        assert!(model_supported_for_provider("openai-codex", "gpt-6-sol"));
        assert!(model_supported_for_provider(
            "openai-codex",
            "gpt-5.3-codex"
        ));
        assert!(!model_supported_for_provider(
            "openai-codex",
            "gpt-6-sol-realtime"
        ));
        assert!(model_supported_for_provider("zai", "glm-5.1"));
        assert!(model_supported_for_provider("zai-coding", "glm-5.1"));
        assert!(!model_supported_for_provider("zai", "embedding-3"));
        assert!(model_supported_for_provider(
            "moonshotai",
            "kimi-k3-0905-preview"
        ));
        assert!(model_supported_for_provider(
            "kimi-coding-openai",
            "kimi-for-coding"
        ));
        assert!(!model_supported_for_provider("github-copilot", "gpt-6-sol"));
    }

    #[test]
    fn anthropic_models_url_paginates_with_cursor() {
        assert_eq!(
            anthropic_models_url(None),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        assert_eq!(
            anthropic_models_url(Some("claude-opus-4-8")),
            "https://api.anthropic.com/v1/models?limit=1000&starting_after=claude-opus-4-8"
        );
    }

    #[test]
    fn anthropic_models_response_reads_pagination_fields() {
        let page: AnthropicModelsResponse = serde_json::from_value(serde_json::json!({
            "data": [{"id": "claude-opus-4-8", "display_name": "Claude Opus 4.8"}],
            "has_more": true,
            "first_id": "claude-opus-4-8",
            "last_id": "claude-opus-4-8",
        }))
        .expect("parse page");
        assert!(page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("claude-opus-4-8"));

        let legacy: AnthropicModelsResponse = serde_json::from_value(serde_json::json!({
            "data": [{"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6"}],
        }))
        .expect("parse legacy page");
        assert!(!legacy.has_more);
        assert_eq!(legacy.last_id, None);
    }

    #[test]
    fn model_cache_version_forces_refresh_for_old_caches() {
        let root = std::env::temp_dir().join(format!(
            "noel-cli-model-cache-version-{}",
            unique_temp_suffix()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("model-cache.json");
        let now = unix_seconds().expect("unix seconds");

        write_model_cache(
            &path,
            &ModelCache {
                refreshed_at: now,
                ..ModelCache::default()
            },
        )
        .expect("write legacy cache");
        assert!(model_cache_needs_refresh(&path, 24));

        write_model_cache(
            &path,
            &ModelCache {
                refreshed_at: now,
                version: MODEL_CACHE_VERSION,
                models: Vec::new(),
                diagnostics: Vec::new(),
                claude_code_version: None,
            },
        )
        .expect("write current cache");
        assert!(!model_cache_needs_refresh(&path, 24));
        assert!(model_cache_needs_refresh(&path, 0));

        let _ = fs::remove_dir_all(root);
    }

    fn refresh_test_model(provider: &str, id: &str) -> ModelDefinition {
        ModelDefinition {
            context_window: None,
            provider: provider.to_string(),
            id: id.to_string(),
            name: Some(id.to_string()),
            api: ConfigProviderApi::OpenAi,
            base_url: None,
            ..ModelDefinition::default()
        }
    }

    fn refresh_test_cache(models: Vec<ModelDefinition>) -> ModelCache {
        ModelCache {
            refreshed_at: 100,
            version: MODEL_CACHE_VERSION,
            models,
            diagnostics: Vec::new(),
            claude_code_version: None,
        }
    }

    #[test]
    fn model_refresh_replaces_provider_but_keeps_other_providers() {
        let existing = refresh_test_cache(vec![
            refresh_test_model("zai-coding", "glm-5.3"),
            refresh_test_model("openai", "gpt-5"),
        ]);
        let mut report = ModelRefreshReport::default();
        report.record(
            "openai",
            Some(&existing),
            Ok(vec![refresh_test_model("openai", "gpt-6")]),
        );
        let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
        assert_eq!(next.refreshed_at, 200);
        assert_eq!(next.version, MODEL_CACHE_VERSION);
        let ids: Vec<&str> = next.models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["glm-5.3", "gpt-6"]);
        assert!(next.diagnostics.is_empty());
    }

    #[test]
    fn model_refresh_empty_fetch_keeps_cached_models_and_reports() {
        let existing = refresh_test_cache(vec![refresh_test_model("zai-coding", "glm-5.3")]);
        let mut report = ModelRefreshReport::default();
        report.record("zai-coding", Some(&existing), Ok(Vec::new()));
        let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
        // The cache contents are untouched; only diagnostics record the event.
        assert_eq!(next.models, existing.models);
        assert_eq!(next.refreshed_at, existing.refreshed_at);
        assert_eq!(
            next.diagnostics,
            ["zai-coding model refresh returned an empty list; kept cached models".to_string()]
        );
        assert_eq!(report.diagnostics(), next.diagnostics);
        assert_eq!(report.kept_cached, ["zai-coding".to_string()]);
        assert_eq!(report.refreshed.len(), 0);
        // The formatted report explains the outcome.
        let config = account_test_config(AuthData::default());
        let output = format_model_refresh_report(&report, &config);
        assert!(output.contains("kept cached models after empty response: zai-coding"));
    }

    #[test]
    fn model_refresh_failure_keeps_cache_stale_and_persists_diagnostics() {
        // Cache is old enough that the TTL already expired, then refresh fails.
        let mut existing = refresh_test_cache(vec![refresh_test_model("zai-coding", "glm-5.3")]);
        existing.refreshed_at = 0;
        let mut report = ModelRefreshReport::default();
        report.record("zai-coding", Some(&existing), Err(anyhow!("status 503")));
        let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
        // TTL is not reset: next startup (past the original TTL) retries
        // instead of waiting out another full TTL.
        assert_eq!(next.refreshed_at, existing.refreshed_at);
        assert!(model_cache_is_stale(&next, Some(90_000), 24));
        assert_eq!(next.models, existing.models);
        assert_eq!(
            next.diagnostics,
            ["zai-coding model refresh failed: status 503".to_string()]
        );
    }

    #[test]
    fn model_refresh_all_failures_without_cache_still_writes_diagnostics() {
        let mut report = ModelRefreshReport::default();
        report.record("openai", None, Err(anyhow!("network down")));
        let next = next_model_cache(None, &report, Some(200)).unwrap();
        assert_eq!(next.refreshed_at, 0);
        assert!(next.models.is_empty());
        assert_eq!(
            next.diagnostics,
            ["openai model refresh failed: network down".to_string()]
        );
    }

    #[test]
    fn model_refresh_without_auth_writes_nothing() {
        let report = ModelRefreshReport::default();
        assert!(next_model_cache(None, &report, Some(200)).is_none());
        assert!(next_model_cache(None, &report, None).is_none());
    }

    #[test]
    fn tests_never_refresh_models_in_the_background() {
        let settings = Settings::default();
        assert!(background_refresh_allowed(&settings, false, false));
        assert!(!background_refresh_allowed(&settings, false, true));
        assert!(!background_refresh_allowed(&settings, true, false));
    }

    #[test]
    fn claude_code_version_from_refresh_survives_merge() {
        let existing = refresh_test_cache(vec![]);
        let report = ModelRefreshReport {
            claude_code_version: Some("2.1.280".to_string()),
            ..Default::default()
        };
        let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
        assert_eq!(next.claude_code_version.as_deref(), Some("2.1.280"));

        // Version-check-only refresh keeps the previously cached version.
        let kept = refresh_test_cache(vec![]);
        let version_only = ModelRefreshReport {
            claude_code_version: Some("9.9.9".to_string()),
            ..Default::default()
        };
        let next = next_model_cache(Some(&kept), &version_only, Some(200)).unwrap();
        assert_eq!(next.claude_code_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn model_cache_reset_removes_file_and_tolerates_missing_file() {
        let root = std::env::temp_dir().join(format!(
            "noel-cli-model-cache-reset-{}",
            unique_temp_suffix()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("model-cache.json");
        assert_eq!(
            reset_model_cache(&path).unwrap(),
            "model cache already clear"
        );
        write_model_cache(&path, &refresh_test_cache(vec![])).expect("write cache");
        assert_eq!(reset_model_cache(&path).unwrap(), "model cache cleared");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn claude_code_version_validation_rejects_header_unsafe_values() {
        assert!(is_valid_claude_code_version("2.1.280"));
        assert!(is_valid_claude_code_version("2.2.0-beta.1+build"));
        assert!(!is_valid_claude_code_version(""));
        assert!(!is_valid_claude_code_version("2.1.280\r\nx-injected: yes"));
        assert!(!is_valid_claude_code_version(&"1".repeat(33)));
    }

    #[test]
    fn numstat_parses_text_and_binary_entries() {
        let stats = parse_numstat("3\t1\tsrc/a.rs\n-\t-\tassets/logo.png\n");
        assert_eq!(stats["src/a.rs"], (Some(3), Some(1)));
        assert_eq!(stats["assets/logo.png"], (None, None));
        assert!(parse_numstat("").is_empty());
    }

    #[test]
    fn collapsed_todos_select_active_upcoming_and_recent_done_in_original_order() {
        let mut app = TuiApp {
            todos: [
                TodoStatus::Completed,
                TodoStatus::Completed,
                TodoStatus::Completed,
                TodoStatus::Completed,
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Pending,
                TodoStatus::Pending,
                TodoStatus::Pending,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, status)| TodoItem {
                content: format!("task {index}"),
                status,
            })
            .collect(),
            ..Default::default()
        };
        assert_eq!(selected_todos(&app.todos, false), [3, 4, 5, 6, 7]);
        let lines = todo_panel_lines(&app, &ThemePalette::default());
        assert_eq!(
            line_text(lines.last().unwrap()),
            "  … +4 more (1 pending, 3 done) · ctrl+t to expand"
        );
        assert!(lines[1].spans[1]
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
        assert!(lines[3].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        app.todos_expanded = true;
        let config = minimal_test_config();
        let mut runtime = account_test_runtime("faux", None);
        runtime.set_todos(app.todos.clone()).unwrap();
        app.refresh_chrome(&config, &runtime);
        assert!(app.todos_expanded);
        assert!(line_text(
            todo_panel_lines(&app, &ThemePalette::default())
                .last()
                .unwrap()
        )
        .contains("collapse"));
    }

    #[test]
    fn thinking_previews_wrap_and_switch_between_streaming_tail_and_finished_head() {
        let entry = TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: "first thought\nsecond thought\nthird thought\nlast thought".into(),
        };
        let palette = ThemePalette::default();
        let head = preview_entry(&entry, 0, 80, &palette, &PreviewState::default());
        assert_eq!(head.len(), 4); // two content rows, one hint, spacer
        assert!(head
            .iter()
            .take(2)
            .map(line_text)
            .collect::<String>()
            .contains("thinking"));
        assert!(head
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("ctrl+o"));
        let tail = preview_entry(
            &entry,
            0,
            80,
            &palette,
            &PreviewState {
                live_thinking: Some(0),
                expanded_from: None,
            },
        );
        assert!(tail
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("last thought"));
        assert!(!tail
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("ctrl+o"));
        assert_eq!(tail.len(), 3);
        let expanded = preview_entry(
            &entry,
            0,
            8,
            &palette,
            &PreviewState {
                expanded_from: Some(0),
                live_thinking: None,
            },
        );
        assert!(expanded.len() > tail.len());
        assert!(!expanded
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("more lines"));
        assert_eq!(
            entry.text,
            "first thought\nsecond thought\nthird thought\nlast thought"
        );
    }

    #[test]
    fn collapsed_blocks_never_exceed_three_rows_even_on_narrow_screens() {
        let palette = ThemePalette::default();
        for kind in [TuiEntryKind::Thinking, TuiEntryKind::Tool] {
            let entry = TuiEntry {
                text: if kind == TuiEntryKind::Tool {
                    format!(
                        "completed bash\n$ {}\n{}",
                        "long command 👩‍💻界 ".repeat(30),
                        "output 界e\u{301}\t".repeat(100)
                    )
                } else {
                    "thought 界e\u{301}👩‍💻\t".repeat(100)
                },
                kind,
            };
            for width in [1, 2, 4, 8, 20, 80] {
                let rows = preview_entry(&entry, 0, width, &palette, &PreviewState::default());
                assert!(rows.len() <= 4, "three content rows plus spacer: {width}");
                assert!(rows.iter().all(|row| row.width() <= width), "width {width}");
                let expanded = preview_entry(
                    &entry,
                    0,
                    width,
                    &palette,
                    &PreviewState {
                        expanded_from: Some(0),
                        live_thinking: None,
                    },
                );
                assert!(expanded.len() > rows.len());
            }
        }
    }

    #[test]
    fn compact_command_header_keeps_line_count_and_full_command_is_expandable() {
        let command = format!(
            "git status --short; echo ---; git diff --stat {} end-of-command",
            "README.md crates/noel-tui/src/lib.rs ".repeat(20)
        );
        let entry = TuiEntry {
            kind: TuiEntryKind::Tool,
            text: format!("completed bash\n$ {command}\nfirst\nsecond\nthird\nfinal result"),
        };
        let palette = ThemePalette::default();
        let collapsed = preview_entry(&entry, 0, 80, &palette, &PreviewState::default());
        assert_eq!(collapsed.len(), 3);
        let header = line_text(&collapsed[0]);
        assert!(header.starts_with("● Ran a command · $ git status --short"));
        assert!(header.ends_with("… · 3 more lines"));
        assert_eq!(line_text(&collapsed[1]), "  … final result");
        let expanded = preview_entry(
            &entry,
            0,
            80,
            &palette,
            &PreviewState {
                expanded_from: Some(0),
                live_thinking: None,
            },
        );
        assert!(expanded
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("end-of-command"));
        assert!(expanded
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("first"));
        // A watermark inside a folded range must not reconstruct hidden output.
        let pending = pending_entry_rows(
            &entry,
            0,
            80,
            &palette,
            &PreviewState::default(),
            EntryScrollback {
                position: TranscriptPosition { line: 2, byte: 1 },
                complete: false,
            },
        );
        assert_eq!(line_text(&pending[0].line), "  … final result");
    }

    #[test]
    fn short_tool_results_stay_visible_without_an_expansion_count() {
        let entry = TuiEntry {
            kind: TuiEntryKind::Tool,
            text: "completed todo\n\none\ntwo".into(),
        };
        let rows = preview_entry(
            &entry,
            0,
            80,
            &ThemePalette::default(),
            &PreviewState::default(),
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(line_text(&rows[0]), "● Used Todo");
        assert_eq!(line_text(&rows[1]), "  one");
        assert_eq!(line_text(&rows[2]), "  two");
        assert!(!rows
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("more lines"));
    }

    #[test]
    fn ctrl_o_expands_only_the_recent_three_turns_and_works_mid_turn() {
        let mut app = TuiApp::default();
        for _ in 0..4 {
            app.push(TuiEntryKind::User, "question");
            app.push(TuiEntryKind::Thinking, "one\ntwo\nthree\nfour");
            app.push(TuiEntryKind::Assistant, "answer");
        }
        assert_eq!(recent_turn_start(&app.entries), 3);
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();
        handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert!(app.previews_expanded);
        let state = PreviewState {
            expanded_from: Some(recent_turn_start(&app.entries)),
            live_thinking: None,
        };
        let palette = ThemePalette::default();
        assert!(preview_entry(&app.entries[1], 1, 100, &palette, &state)
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("more lines"));
        assert!(preview_entry(&app.entries[4], 4, 100, &palette, &state)
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("four"));
    }

    #[test]
    fn tool_output_previews_bound_large_results_and_ctrl_o_round_trips() {
        let output = (0..5_000)
            .map(|line| format!("file-line-{line:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        let config = minimal_test_config();
        let palette = ThemePalette::default();
        for text in [
            format!("completed read\nsrc/large.rs\n{output}"),
            format!("failed bash\n$ command\n{output}"),
            output.clone(), // terminal progress disabled
        ] {
            let mut app = TuiApp::default();
            app.push(TuiEntryKind::User, "read the file");
            app.push(TuiEntryKind::Tool, text.clone());
            let render = |app: &TuiApp| {
                preview_entry(
                    &app.entries[1],
                    1,
                    80,
                    &palette,
                    &transcript_preview_state(app),
                )
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
            };
            let collapsed = render(&app);
            assert_eq!(collapsed.len(), 3); // header, final output row, spacer
            assert!(collapsed.join("\n").contains("more lines"));
            assert!(collapsed.join("\n").contains("… file-line-4999"));
            assert!(!collapsed.join("\n").contains("file-line-1000"));
            for expanded in [true, false] {
                assert_eq!(
                    handle_streaming_tui_key(
                        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                        &mut app,
                        &FollowUpQueue::default(),
                        &SteeringMailbox::default(),
                        &config,
                    ),
                    StreamingKeyOutcome::Changed
                );
                assert_eq!(app.previews_expanded, expanded);
                let rows = render(&app);
                if expanded {
                    assert!(rows.join("\n").contains("file-line-1000"));
                    assert!(!rows.join("\n").contains("more lines"));
                } else {
                    assert_eq!(rows, collapsed);
                }
                assert_eq!(app.entries[1].text, text);
            }
        }
    }

    #[test]
    fn owned_tool_output_is_collapsed_live_and_after_completion() {
        use ratatui::backend::TestBackend;
        let mut app = TuiApp {
            owned_screen: true,
            turn_running: true,
            ..Default::default()
        };
        app.push(TuiEntryKind::User, "read the file");
        let thinking_index = app.push_placeholder(TuiEntryKind::Thinking, "");
        let assistant_index = app.push_placeholder(TuiEntryKind::Assistant, "Working...");
        let output = (0..12)
            .map(|line| format!("live-file-line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut saw_delta = false;
        let mut activity = Activity::Waiting;
        let call = noel_ai::ChatToolCall {
            id: "large-read".into(),
            name: "read".into(),
            arguments: r#"{"path":"large.rs"}"#.into(),
        };
        for event in [
            TurnEvent::ToolStarted { call: call.clone() },
            TurnEvent::ToolOutput {
                call,
                output: output.clone(),
                images: Vec::new(),
                is_error: false,
                duration: Some(Duration::ZERO),
            },
        ] {
            let mut live_text_index = assistant_index;
            let mut text_after_tool = false;
            apply_turn_event(
                &mut app,
                &mut live_text_index,
                &mut text_after_tool,
                thinking_index,
                &mut saw_delta,
                &mut activity,
                &event,
            );
        }
        let config = minimal_test_config();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        for live in [true, false] {
            if !live {
                app.live_tool_indices.clear();
                app.finish_live_entry();
            }
            for expanded in [false, true, false] {
                if app.previews_expanded != expanded {
                    handle_streaming_tui_key(
                        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                        &mut app,
                        &FollowUpQueue::default(),
                        &SteeringMailbox::default(),
                        &config,
                    );
                }
                redraw_tui(&mut terminal, &mut app, &config).unwrap();
                let screen = buffer_rows(terminal.backend().buffer()).join("\n");
                assert!(screen.contains("Used Read · large.rs · 12 lines"));
                assert!(screen.contains("live-file-line-11"));
                assert_eq!(screen.contains("live-file-line-00"), expanded);
                assert_eq!(screen.contains("11 more lines"), !expanded);
                // The tool entry took the place of the turn's placeholder.
                assert_eq!(
                    app.entries[assistant_index].text,
                    format!("completed read\nlarge.rs\n{output}")
                );
            }
        }
    }

    #[test]
    fn restored_tool_output_is_collapsed_without_changing_context() {
        let mut session = SessionState::new("collapsed-read", PathBuf::from("."));
        session.messages.push(ConversationMessage {
            role: MessageRole::Tool,
            content: "restored-file-line\n".repeat(5_000),
            thinking: String::new(),
            media: Vec::new(),
            tool_call_id: Some("read-1".into()),
            tool_name: Some("read".into()),
            tool_calls: Vec::new(),
            aborted: false,
        });
        let runtime = Runtime::new(session.clone(), ReloadableSystems::default());
        let mut app = TuiApp::default();
        app.restore_session_messages(&runtime);
        let rows = preview_entry(
            &app.entries[0],
            0,
            80,
            &ThemePalette::default(),
            &transcript_preview_state(&app),
        );
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .map(line_text)
            .collect::<String>()
            .contains("4999 more lines"));
        assert!(app.entries[0].text.ends_with(&session.messages[0].content));
        assert_eq!(runtime.session(), &session);
    }

    #[test]
    fn new_prompts_and_clear_restore_collapsed_previews() {
        let mut app = TuiApp::default();
        app.push(TuiEntryKind::User, "first prompt");
        app.previews_expanded = true;
        app.push(TuiEntryKind::Tool, "first result");
        app.push(TuiEntryKind::System, "reloaded");
        assert!(app.previews_expanded);
        app.push(TuiEntryKind::User, "next prompt");
        assert!(!app.previews_expanded);
        app.previews_expanded = true;
        app.clear_visible();
        assert!(!app.previews_expanded);
    }

    #[test]
    fn tool_label_styles_are_segmented_and_previews_keep_final_output_line() {
        let palette = ThemePalette::default();
        let entry = TuiEntry {
            kind: TuiEntryKind::Tool,
            text: "completed read\nsrc/main.rs\nfirst\nsecond\nthird\nfourth\nfifth".into(),
        };
        let lines = render_entry_lines(std::slice::from_ref(&entry), &palette);
        let spans = &lines[0].spans;
        assert_eq!(spans[0].style.fg, Some(palette.success));
        assert_eq!(spans[1].content, "Used ");
        assert_eq!(spans[1].style.fg, None);
        assert_eq!(spans[2].content, "Read");
        assert_eq!(spans[2].style.fg, Some(palette.accent));
        assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[3].style.fg, Some(palette.muted));
        let preview = preview_entry(&entry, 0, 100, &palette, &PreviewState::default());
        let text = preview.iter().map(line_text).collect::<String>();
        assert!(!text.contains("first"));
        assert!(text.contains("4 more lines"));
        assert!(text.contains("… fifth"));
        let mut failed = Vec::new();
        push_tool_lines(
            &mut failed,
            "failed bash\n$ false\n(no output)\n\nCommand exited with code 1",
            &palette,
        );
        assert_eq!(failed[0].spans[0].content, "✗ ");
        assert_eq!(failed[0].spans[0].style.fg, Some(palette.error));
    }

    #[test]
    fn live_tool_events_show_results_and_todo_updates_without_waiting_for_final_answer() {
        let mut app = TuiApp::default();
        let thinking_index = app.push_placeholder(TuiEntryKind::Thinking, "");
        let index = app.push_placeholder(TuiEntryKind::Assistant, "");
        let call = noel_ai::ChatToolCall {
            id: "call-1".into(),
            name: "web_fetch".into(),
            arguments: r#"{"url":"https://example.org"}"#.into(),
        };
        let mut activity = Activity::Waiting;
        let mut saw_delta = false;
        let mut live_text_index = index;
        let mut text_after_tool = false;
        apply_turn_event(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            thinking_index,
            &mut saw_delta,
            &mut activity,
            &TurnEvent::ToolStarted { call: call.clone() },
        );
        let mut live_text_index = index;
        let mut text_after_tool = false;
        apply_turn_event(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            thinking_index,
            &mut saw_delta,
            &mut activity,
            &TurnEvent::ToolOutput {
                call,
                output: "HTTP 403".into(),
                images: Vec::new(),
                is_error: true,
                duration: Some(Duration::ZERO),
            },
        );
        // The reply began with the call: the tool entry took the place of
        // the turn's placeholder.
        assert_eq!(app.entries.len(), 2);
        assert_eq!(
            app.entries[index].text,
            "failed web_fetch\nhttps://example.org\nHTTP 403"
        );
        let todos = vec![TodoItem {
            content: "verify source".into(),
            status: TodoStatus::InProgress,
        }];
        let mut live_text_index = index;
        let mut text_after_tool = false;
        apply_turn_event(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            thinking_index,
            &mut saw_delta,
            &mut activity,
            &TurnEvent::TodosUpdated(todos.clone()),
        );
        assert_eq!(app.todos, todos);
    }

    #[test]
    fn queue_up_recalls_newest_and_ctrl_s_steers_queue_then_draft() {
        let mut app = TuiApp::default();
        let followups = FollowUpQueue::default();
        followups.push("first".into());
        followups.push("second\nline".into());
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();
        handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(app.input, "second\nline");
        assert_eq!(input_layout(&app, 76).rows.len(), 2);
        assert_eq!(followups.list(), ["first"]);
        handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(steering.drain(), ["first", "second\nline"]);
        assert_eq!(followups.pending(), 0);
        assert!(app.input.is_empty());
    }

    #[test]
    fn queue_pane_is_bounded_and_keeps_one_row_per_message() {
        use ratatui::backend::TestBackend;
        let app = TuiApp {
            queued_messages: vec!["queued\nmessage".into(); 7],
            turn_running: true,
            ..Default::default()
        };
        assert_eq!(queue_panel_height(&app), 6);
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal
            .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(text.matches("queued message").count(), 4);
        assert!(text.contains("+3 more"));
        assert!(text.contains("ctrl+s"));
    }

    #[test]
    fn text_after_tool_rounds_opens_new_interleaved_entries() {
        // A tool round must split the streamed response: text before the
        // tool stays in the first entry, text after it lands in a new entry
        // below the tool preview, so the newest text is always at the
        // bottom instead of growing above the tool calls.
        let mut app = TuiApp::default();
        let thinking = app.entries.len();
        app.entries.push(TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: String::new(),
        });
        let entry = app.push_placeholder(TuiEntryKind::Assistant, "Working...");
        let mut saw_delta = false;
        let mut live_text_index = entry;
        let mut text_after_tool = false;
        let read = noel_ai::ChatToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        };
        let send = |app: &mut TuiApp,
                    live_text_index: &mut usize,
                    text_after_tool: &mut bool,
                    saw_delta: &mut bool,
                    event: &TurnEvent| {
            apply_turn_event(
                app,
                live_text_index,
                text_after_tool,
                thinking,
                saw_delta,
                &mut Activity::Waiting,
                event,
            );
        };
        send(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            &mut saw_delta,
            &TurnEvent::Provider(StreamEvent::Text("before tool".into())),
        );
        send(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            &mut saw_delta,
            &TurnEvent::ToolStarted { call: read.clone() },
        );
        send(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            &mut saw_delta,
            &TurnEvent::ToolOutput {
                call: read,
                output: "file body".into(),
                images: Vec::new(),
                is_error: false,
                duration: Some(Duration::ZERO),
            },
        );
        send(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            &mut saw_delta,
            &TurnEvent::Provider(StreamEvent::Text("after tool".into())),
        );
        send(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            &mut saw_delta,
            &TurnEvent::ToolStarted {
                call: noel_ai::ChatToolCall {
                    id: "call-2".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            },
        );
        send(
            &mut app,
            &mut live_text_index,
            &mut text_after_tool,
            &mut saw_delta,
            &TurnEvent::Provider(StreamEvent::Text("final".into())),
        );
        assert!(saw_delta);
        let layout: Vec<(TuiEntryKind, &str)> = app
            .entries
            .iter()
            .map(|entry| (entry.kind.clone(), entry.text.as_str()))
            .collect();
        assert_eq!(
            layout,
            vec![
                (TuiEntryKind::Thinking, ""),
                (TuiEntryKind::Assistant, "before tool"),
                (TuiEntryKind::Tool, "completed read\na.txt\nfile body"),
                (TuiEntryKind::Assistant, "after tool"),
                (TuiEntryKind::Tool, "running bash\n\n"),
                (TuiEntryKind::Assistant, "final"),
            ],
            "tool rounds must interleave with per-round assistant entries"
        );
        // The live pointer tracks the newest text entry, not the original.
        assert_eq!(live_text_index, app.entries.len() - 1);
        assert_eq!(app.live_entry_index, Some(app.entries.len() - 1));
    }

    /// Applies `events` to a new turn's entries (its thinking entry, then
    /// its reply's placeholder) and returns them.
    fn entries_after(events: &[TurnEvent]) -> Vec<(TuiEntryKind, String)> {
        let mut app = TuiApp::default();
        let thinking = app.push_placeholder(TuiEntryKind::Thinking, "");
        let mut live_text_index = app.push_placeholder(TuiEntryKind::Assistant, "Working...");
        let (mut saw_delta, mut text_after_tool) = (false, false);
        for event in events {
            apply_turn_event(
                &mut app,
                &mut live_text_index,
                &mut text_after_tool,
                thinking,
                &mut saw_delta,
                &mut Activity::Waiting,
                event,
            );
        }
        let entries = app.entries.into_iter();
        entries.map(|entry| (entry.kind, entry.text)).collect()
    }

    #[test]
    fn a_reply_that_starts_with_a_tool_call_leaves_no_placeholder() {
        let call = |id: &str| noel_ai::ChatToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        };
        let started = |id: &str| TurnEvent::ToolStarted { call: call(id) };
        let output = |id: &str| TurnEvent::ToolOutput {
            call: call(id),
            output: "file body".into(),
            images: Vec::new(),
            is_error: false,
            duration: Some(Duration::ZERO),
        };
        let text = |text: &str| TurnEvent::Provider(StreamEvent::Text(text.into()));
        let thinking = (TuiEntryKind::Thinking, String::new());
        let tool = (
            TuiEntryKind::Tool,
            "completed read\na.txt\nfile body".to_string(),
        );
        let assistant = |text: &str| (TuiEntryKind::Assistant, text.to_string());

        // Two calls, then the answer: nothing is left of "Working...".
        let events = [
            TurnEvent::RoundStarted,
            started("1"),
            output("1"),
            started("2"),
            output("2"),
            TurnEvent::RoundStarted,
            text("done"),
        ];
        assert_eq!(
            entries_after(&events),
            [
                thinking.clone(),
                tool.clone(),
                tool.clone(),
                assistant("done")
            ]
        );
        // Text before the call took the placeholder's place, as before.
        let events = [TurnEvent::RoundStarted, text("Let me look."), started("1")];
        assert_eq!(
            entries_after(&events)[..2],
            [thinking.clone(), assistant("Let me look.")]
        );
        // Until a reply says or calls something, the placeholder stands.
        assert_eq!(
            entries_after(&[TurnEvent::RoundStarted]),
            [thinking, assistant("Working...")]
        );
    }

    #[test]
    fn a_running_command_shows_its_output_so_far() {
        use ratatui::backend::TestBackend;
        let call = noel_ai::ChatToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"make test"}"#.into(),
        };
        let update = |output: &str| TurnEvent::ToolUpdate {
            call: call.clone(),
            output: output.into(),
        };
        let started = TurnEvent::ToolStarted { call: call.clone() };
        let tool = |events: &[TurnEvent]| entries_after(events)[1].clone().1;
        // The command starts, prints, prints more, and ends.
        assert_eq!(tool(std::slice::from_ref(&started)), "running bash\n\n");
        assert_eq!(
            tool(&[started.clone(), update("")]),
            "running bash\n$ make test\n"
        );
        let running = [
            started.clone(),
            update(""),
            update("one\n"),
            update("one\ntwo\nthree\n"),
        ];
        assert_eq!(
            tool(&running),
            "running bash\n$ make test\none\ntwo\nthree\n"
        );
        let mut finished = running.to_vec();
        finished.push(TurnEvent::ToolOutput {
            call: call.clone(),
            output: "one\ntwo\nthree\nfour\n".into(),
            images: Vec::new(),
            is_error: false,
            duration: Some(Duration::ZERO),
        });
        assert_eq!(
            tool(&finished),
            "completed bash\n$ make test\none\ntwo\nthree\nfour\n"
        );

        // On screen the running command shows its latest line, and all of
        // its output once previews are expanded.
        let config = minimal_test_config();
        let mut app = TuiApp {
            owned_screen: true,
            turn_running: true,
            ..Default::default()
        };
        app.push(TuiEntryKind::Tool, tool(&running));
        app.live_tool_indices.push(0);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        let screen = buffer_rows(terminal.backend().buffer()).join("\n");
        assert!(
            screen.contains("Running a command · $ make test · 2 more lines"),
            "{screen}"
        );
        assert!(screen.contains("… three"), "{screen}");
        assert!(!screen.contains("one"), "{screen}");
        app.previews_expanded = true;
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        let screen = buffer_rows(terminal.backend().buffer()).join("\n");
        for line in ["  one", "  two", "  three"] {
            assert!(screen.contains(line), "{line}: {screen}");
        }
    }

    #[test]
    fn a_dropped_reply_stays_above_a_notice_and_the_next_reply_starts_below() {
        let text = |text: &str| TurnEvent::Provider(StreamEvent::Text(text.into()));
        let retry = TurnEvent::RetryStarted(noel_core::RetryNotice {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2_000,
            error_message: "request failed with status 503 Service Unavailable: overloaded".into(),
        });
        let notice = (
            TuiEntryKind::System,
            "retrying in 2s (attempt 1 of 3): request failed with status 503 Service Unavailable: overloaded".to_string(),
        );
        let thinking = (TuiEntryKind::Thinking, String::new());
        let assistant = |text: &str| (TuiEntryKind::Assistant, text.to_string());

        // The reply broke off after some text: the text stays, and the
        // reply that replaces it is a new entry below the notice.
        let events = [
            TurnEvent::RoundStarted,
            text("Hel"),
            retry.clone(),
            TurnEvent::RoundStarted,
            text("Hello "),
            text("again."),
        ];
        assert_eq!(
            entries_after(&events),
            [
                thinking.clone(),
                assistant("Hel"),
                notice.clone(),
                assistant("Hello again.")
            ]
        );
        // The request failed before any text: the placeholder still waits
        // for the reply.
        let events = [
            TurnEvent::RoundStarted,
            retry.clone(),
            TurnEvent::RoundStarted,
            text("Hello again."),
        ];
        assert_eq!(
            entries_after(&events),
            [thinking.clone(), assistant("Hello again."), notice]
        );
        // A reply cut off at the output limit is dropped the same way once
        // a compaction makes room to ask again.
        let events = [
            TurnEvent::RoundStarted,
            text("This answer is cut"),
            TurnEvent::Overflowed { error: None },
            TurnEvent::CompactionStarted,
            TurnEvent::CompactionFinished(noel_core::CompactionRecord {
                kind: noel_core::CompactionKind::Automatic,
                omitted_messages: 4,
                retained_messages: 1,
                summary: "Summary.".into(),
            }),
            TurnEvent::RoundStarted,
            text("The whole answer."),
        ];
        assert_eq!(
            entries_after(&events),
            [
                thinking,
                assistant("This answer is cut"),
                (
                    TuiEntryKind::System,
                    "reply cut off at the output limit; compacting to ask again".to_string()
                ),
                (
                    TuiEntryKind::System,
                    "auto-compacted: omitted 4 message(s)".to_string()
                ),
                assistant("The whole answer.")
            ]
        );
        // If that compaction fails, the turn does not ask again.
        let events = [
            TurnEvent::RoundStarted,
            TurnEvent::Overflowed {
                error: Some("too large".into()),
            },
            TurnEvent::CompactionStarted,
            TurnEvent::CompactionFailed("nothing to compact".into()),
        ];
        assert_eq!(
            entries_after(&events)[2..],
            [
                (
                    TuiEntryKind::System,
                    "context overflow; compacting to ask again: too large".to_string()
                ),
                (
                    TuiEntryKind::System,
                    "compaction failed, not asking again: nothing to compact".to_string()
                ),
            ]
        );
    }

    #[test]
    fn queue_hint_distinguishes_prose_only_turns_from_tool_rounds() {
        use ratatui::backend::TestBackend;
        let render = |saw_tools: bool| {
            let app = TuiApp {
                queued_messages: vec!["queued".into()],
                turn_running: true,
                turn_saw_tools: saw_tools,
                ..Default::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
            terminal
                .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        // With tool rounds in the turn, steering lands at the next boundary.
        assert!(render(true).contains("ctrl+s to steer at next boundary"));
        // A prose-only response has no boundary: promise post-response
        // delivery instead.
        assert!(render(false).contains("ctrl+s queues after this response"));
    }

    #[test]
    fn harness_frame_matches_todo_queue_and_boxed_prompt_example() {
        use ratatui::backend::TestBackend;
        let config = minimal_test_config();
        let mut app = TuiApp {
            todos: (0..6)
                .map(|index| TodoItem {
                    content: format!("task {index}"),
                    status: if index == 0 {
                        TodoStatus::InProgress
                    } else {
                        TodoStatus::Pending
                    },
                })
                .collect(),
            queued_messages: vec!["make the harness standard".into()],
            turn_running: true,
            turn_saw_tools: true,
            show_hardware_cursor: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|frame| draw_tui(frame, &app, &config))
            .unwrap();
        let rows = buffer_rows(terminal.backend().buffer());
        let expected = [
            "─".repeat(80),
            "  Todo".into(),
            "  ● task 0".into(),
            "  ○ task 1".into(),
            "  ○ task 2".into(),
            "  ○ task 3".into(),
            "  ○ task 4".into(),
            "  … +1 more (1 pending) · ctrl+t to expand".into(),
            "─".repeat(80),
            "  ❯ make the harness standard".into(),
            "  ↑ to edit · ctrl+s to steer at next boundary".into(),
            format!("╭{}╮", "─".repeat(78)),
            format!("│ > {}│", " ".repeat(75)),
            format!("╰{}╯", "─".repeat(78)),
        ];
        assert_eq!(
            rows[15..29]
                .iter()
                .map(|row| row.trim_end())
                .collect::<Vec<_>>(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        terminal.backend_mut().assert_cursor_position((4, 27));
        // The queue owns the shared separator, with no duplicate rule or
        // blank row between the hints and editor. Both sections can vanish.
        app.todos.clear();
        app.queued_messages.clear();
        terminal
            .draw(|frame| draw_tui(frame, &app, &config))
            .unwrap();
        let rows = buffer_rows(terminal.backend().buffer());
        assert!(rows[..26].iter().all(|row| row.trim().is_empty()));
        assert!(rows[27].starts_with("│ > "));
    }

    #[test]
    fn height_limited_todos_report_actual_hidden_counts_and_keep_active_task() {
        use ratatui::backend::TestBackend;
        let mut app = TuiApp {
            todos: (0..8)
                .map(|index| TodoItem {
                    content: format!("task {index}"),
                    status: if index == 6 {
                        TodoStatus::InProgress
                    } else {
                        TodoStatus::Pending
                    },
                })
                .collect(),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 7)).unwrap();
        terminal
            .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        let text = buffer_rows(terminal.backend().buffer()).join("\n");
        assert!(text.contains("● task 6"));
        assert!(text.contains("+5 more (5 pending) · ctrl+t to expand"));
        assert!(!text.contains("task 2"));
        app.todos_expanded = true;
        terminal
            .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        let text = buffer_rows(terminal.backend().buffer()).join("\n");
        assert!(text.contains("+5 more (4 pending, 1 active) · ctrl+t to collapse"));
        assert!(text.contains("task 2"));
        assert!(!text.contains("task 3"));
    }

    #[test]
    fn queue_overflow_shows_newest_editable_message_and_cell_width_truncation() {
        use ratatui::backend::TestBackend;
        let mut app = TuiApp {
            queued_messages: (0..7).map(|index| format!("queued {index}")).collect(),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
        terminal
            .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        let text = buffer_rows(terminal.backend().buffer()).join("\n");
        assert!(text.contains("❯ queued 5"));
        assert!(text.contains("❯ queued 6"));
        assert!(!text.contains("queued 4"));
        assert!(text.contains("+5 more · ↑ to edit · runs after next turn"));
        app.queued_messages = vec!["界界界界界界界界界界".into()];
        let mut terminal = Terminal::new(TestBackend::new(16, 3)).unwrap();
        terminal
            .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        assert_eq!(
            buffer_rows(terminal.backend().buffer())[1].trim_end(),
            "  ❯ 界界界界界…"
        );
        assert_eq!(truncate_width("e\u{301}xy", 2), "e\u{301}…");
    }

    #[test]
    fn tiny_frames_and_multiline_drafts_never_place_cursor_on_editor_border() {
        use ratatui::backend::TestBackend;
        let config = minimal_test_config();
        let mut app = TuiApp {
            show_hardware_cursor: true,
            queued_messages: vec!["pending".into()],
            todos: vec![TodoItem {
                content: "active".into(),
                status: TodoStatus::InProgress,
            }],
            ..Default::default()
        };
        app.set_input("/help");
        for width in [1, 4, 8, 20, 80] {
            for height in 1..=12 {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| draw_tui(frame, &app, &config))
                    .unwrap();
            }
        }
        app.set_input("first\nsecond\nthird");
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|frame| {
                draw_input(frame, frame.area(), &app, &ThemePalette::default());
                set_tui_cursor(frame, frame.area(), &app);
            })
            .unwrap();
        let rows = buffer_rows(terminal.backend().buffer());
        assert!(rows[1].starts_with("│ > first"));
        assert!(rows[2].starts_with("│   second"));
        assert!(rows[3].starts_with("│   third"));
        terminal.backend_mut().assert_cursor_position((9, 3));
        app.multiline = Some(Vec::new());
        app.clear_input();
        assert_eq!(input_desired_height(&app, 40), 4);
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        let rows = buffer_rows(terminal.backend().buffer());
        assert!(rows[1].starts_with("│ multi> "));
        assert!(rows[2].contains("submit with . on its own line"));
    }

    #[test]
    fn todo_panel_height_collapses_and_expands() {
        let mut app = TuiApp::default();
        assert_eq!(todo_panel_height(&app), 0);
        app.todos = (0..8)
            .map(|index| TodoItem {
                content: format!("task {index}"),
                status: TodoStatus::Pending,
            })
            .collect();
        assert_eq!(todo_panel_height(&app), 9);
        app.todos_expanded = true;
        assert_eq!(todo_panel_height(&app), 12);
        app.todos = (0..3)
            .map(|index| TodoItem {
                content: format!("task {index}"),
                status: TodoStatus::Pending,
            })
            .collect();
        assert_eq!(todo_panel_height(&app), 7);
    }

    #[test]
    fn diff_panel_keys_navigate_and_open_detail() {
        let cwd =
            std::env::temp_dir().join(format!("noel-cli-diff-panel-test-{}", unique_temp_suffix()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        fs::write(cwd.join("new.rs"), "fn main() {}\n").expect("write file");
        let runtime = Runtime::new(
            SessionState::new("session-1", cwd.clone()),
            ReloadableSystems::default(),
        );
        let mut app = TuiApp {
            diff_panel: Some(DiffPanelState {
                files: vec![
                    DiffFileEntry {
                        path: "new.rs".to_string(),
                        added: None,
                        removed: None,
                        untracked: true,
                    },
                    DiffFileEntry {
                        path: "other.rs".to_string(),
                        added: None,
                        removed: None,
                        untracked: true,
                    },
                ],
                selected: 0,
                detail: None,
            }),
            ..TuiApp::default()
        };

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(handle_diff_panel_key(&down, &mut app, &runtime));
        assert_eq!(app.diff_panel.as_ref().expect("panel").selected, 1);
        assert!(handle_diff_panel_key(&down, &mut app, &runtime));
        assert_eq!(app.diff_panel.as_ref().expect("panel").selected, 1);

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(handle_diff_panel_key(&enter, &mut app, &runtime));
        let panel = app.diff_panel.as_ref().expect("panel");
        let detail = panel.detail.as_ref().expect("detail");
        assert_eq!(detail.path, "other.rs");
        assert_eq!(detail.lines, ["unable to read file".to_string()]);

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(handle_diff_panel_key(&esc, &mut app, &runtime));
        assert!(app.diff_panel.as_ref().expect("panel").detail.is_none());
        assert!(handle_diff_panel_key(&esc, &mut app, &runtime));
        assert!(app.diff_panel.is_none());

        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn diff_detail_renders_untracked_file_as_additions() {
        let cwd = std::env::temp_dir().join(format!(
            "noel-cli-diff-detail-test-{}",
            unique_temp_suffix()
        ));
        fs::create_dir_all(&cwd).expect("create temp dir");
        fs::write(cwd.join("new.rs"), "fn main() {}\n").expect("write file");
        let entry = DiffFileEntry {
            path: "new.rs".to_string(),
            added: None,
            removed: None,
            untracked: true,
        };

        let detail = build_diff_detail(&cwd, &entry);

        assert_eq!(detail.lines, ["+fn main() {}".to_string()]);
        let _ = fs::remove_dir_all(cwd);
    }

    fn test_model(provider: &str, id: &str) -> ModelDefinition {
        ModelDefinition {
            context_window: None,
            provider: provider.to_string(),
            id: id.to_string(),
            name: None,
            api: ConfigProviderApi::default(),
            base_url: None,
            ..ModelDefinition::default()
        }
    }

    #[test]
    fn cli_contract_covers_upstream_ts_options_and_commands() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/cli-contract.json"
        ))
        .expect("parse cli contract fixture");
        let rust_options = rust_cli_option_contract();
        let rust_commands = rust_cli_command_contract();

        for option in fixture["options"].as_array().expect("fixture options") {
            let long = option["long"].as_str().expect("option long");
            assert!(
                rust_options.contains(long),
                "missing upstream CLI option --{long}"
            );
            if let Some(short) = option["short"].as_str() {
                assert!(
                    rust_options.contains(short),
                    "missing upstream CLI option -{short}"
                );
            }
        }

        let fixture_commands = fixture["commands"]
            .as_array()
            .expect("fixture commands")
            .iter()
            .map(|command| command.as_str().expect("command"))
            .collect::<BTreeSet<_>>();
        let register = include_str!("../../../docs/rust-rewrite/non-parity-register.md");
        for (command, heading) in UNSUPPORTED_UPSTREAM_CLI_COMMANDS {
            assert!(
                fixture_commands.contains(command),
                "stale exception: upstream no longer has CLI command {command}"
            );
            assert!(
                !rust_commands.contains(*command),
                "{command} is implemented; drop its exception"
            );
            assert!(
                register.contains(heading),
                "non-parity register must record CLI command {command} under {heading:?}"
            );
        }
        for command in fixture_commands {
            if UNSUPPORTED_UPSTREAM_CLI_COMMANDS
                .iter()
                .any(|(unsupported, _)| *unsupported == command)
            {
                continue;
            }
            assert!(
                rust_commands.contains(command),
                "missing upstream CLI command {command}"
            );
        }
    }

    /// Upstream CLI commands this port deliberately leaves out, each with
    /// its docs/rust-rewrite/non-parity-register.md heading.
    const UNSUPPORTED_UPSTREAM_CLI_COMMANDS: &[(&str, &str)] = &[("mcp", "### No MCP Support")];

    #[test]
    fn tui_mode_flag_selects_owned_or_native_screen() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["noel"][..], args].concat());
        let owned = |args: &[&str]| parse(args).expect("parse cli").owned_screen();
        assert!(owned(&[]));
        assert!(owned(&["--tui-mode", "fullscreen"]));
        assert!(!owned(&["--tui-mode", "regular"]));
        assert!(!owned(&["--no-alt-screen"]));
        assert!(parse(&["--tui-mode", "fullscreen", "--no-alt-screen"]).is_err());
        assert!(parse(&["--tui-mode", "split"]).is_err());
        assert!(parse(&["--no-mcp"]).expect("parse cli").no_mcp);
    }

    #[test]
    fn use_theme_conflicts_with_other_theme_flags_and_applies_for_one_run() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["noel"][..], args].concat());
        for args in [
            &["--use-theme", "light", "--no-themes"][..],
            &["--use-theme", "light", "--theme", "dark"],
        ] {
            let error = parse(args).expect_err("conflicting theme flags");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{args:?}"
            );
        }

        let mut config = minimal_test_config();
        let no_themes = parse(&["--theme", "missing", "--no-themes"]).expect("parse cli");
        apply_cli_overrides(&no_themes, Path::new("."), &mut config).expect("no themes");
        assert_eq!(config.settings.theme, None);
        let use_theme = parse(&["--use-theme", "light"]).expect("parse cli");
        apply_cli_overrides(&use_theme, Path::new("."), &mut config).expect("use theme");
        assert_eq!(config.settings.theme.as_deref(), Some("light"));
        let pair = parse(&["--use-theme", "light/dark"]).expect("parse cli");
        let error = apply_cli_overrides(&pair, Path::new("."), &mut config).expect_err("pair");
        assert!(
            error.to_string().contains("theme pairs are not supported"),
            "{error}"
        );
    }

    fn rust_cli_option_contract() -> BTreeSet<String> {
        let mut options = BTreeSet::new();
        for argument in Cli::command().get_arguments() {
            if let Some(long) = argument.get_long() {
                options.insert(long.to_string());
            }
            if let Some(short) = argument.get_short() {
                options.insert(short.to_string());
            }
        }
        for option in [
            "help", "h", "version", "v", "nt", "nbt", "ne", "ns", "np", "nc", "xt", "na",
        ] {
            options.insert(option.to_string());
        }
        options
    }

    fn rust_cli_command_contract() -> BTreeSet<String> {
        [
            "auth",
            "config",
            "install",
            "list",
            "remove",
            "uninstall",
            "update",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect()
    }

    fn test_resource(name: &str) -> ResourceFile {
        ResourceFile {
            name: name.to_string(),
            path: PathBuf::from(name),
            content: String::new(),
        }
    }

    fn test_source(path: &Path) -> SourceInfo {
        SourceInfo {
            path: path.to_path_buf(),
            source: "auto".into(),
            scope: SourceScope::User,
            from_package: false,
            base_dir: None,
        }
    }

    /// A user skill at `/skills/<name>/SKILL.md`.
    fn test_skill(name: &str, description: &str) -> Skill {
        let base_dir = PathBuf::from(format!("/skills/{name}"));
        let file_path = base_dir.join("SKILL.md");
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            source_info: test_source(&file_path),
            file_path,
            base_dir,
            disable_model_invocation: false,
        }
    }

    /// A user prompt template at `/prompts/<name>.md`.
    fn test_template(
        name: &str,
        description: &str,
        argument_hint: Option<&str>,
        content: &str,
    ) -> PromptTemplate {
        let file_path = PathBuf::from(format!("/prompts/{name}.md"));
        PromptTemplate {
            name: name.to_string(),
            description: description.to_string(),
            argument_hint: argument_hint.map(str::to_string),
            content: content.to_string(),
            source_info: test_source(&file_path),
            file_path,
        }
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .expect("encode png");
        output.into_inner()
    }
}

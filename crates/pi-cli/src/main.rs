use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Cursor, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod oauth_login;
mod oauth_refresh;
mod usage;

use anyhow::{anyhow, Result};
use base64::Engine;
#[cfg(test)]
use clap::CommandFactory;
use clap::{Parser, ValueEnum};
use crossterm::{
    cursor::MoveTo,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, Clear as TerminalClear, ClearType},
};
use pi_ai::{
    anthropic_supports_adaptive_thinking, create_provider, generate_images,
    set_claude_code_version, ImageGenerationInput, ImageGenerationOutput,
    ImageProviderApi as AiImageProviderApi, ImageProviderConfig, MediaInput, ModelRef,
    ProviderApi as AiProviderApi, ProviderAuth, ProviderConfig, StreamEvent,
};
use pi_config::{
    auth_for_provider, codex_client_version, has_auth_for_provider, listed_accounts_for_provider,
    load_config, load_config_with_project_trust, project_is_trusted, read_model_cache,
    save_project_trust, write_file_atomic, write_model_cache, AccountSource, AuthCredential,
    AuthData, CompactionSettings, ConfigPaths, ImageModelDefinition,
    ImageProviderApi as ConfigImageProviderApi, ImageSettings, LoadedConfig, ModelCache,
    ModelDefinition, ModelRefreshSettings, PackageSource, ProviderApi as ConfigProviderApi,
    ResolvedAuth, ResourceFile, RetrySettings, Settings, TerminalSettings, WarningSettings,
    DEFAULT_ACCOUNT_NAME, ENV_SESSION_DIR, MODEL_CACHE_VERSION,
};
use pi_core::{
    default_active_tool_names, format_todo_list, run_excluded_bash, run_user_turn,
    run_user_turn_streaming, run_user_turn_streaming_events_with_media,
    run_user_turn_streaming_with_media, write_session_export, AgentError, CompactionKind,
    ConversationMessage, FollowUpQueue, MessageRole, ReloadableSystems, Runtime, SessionState,
    SessionStore, SteeringMailbox, SteeringMode, TodoItem, TodoStatus, TurnEvent,
};
use pi_tui::{
    parse_theme_color, EditorState, Keybinding as TuiKeybinding, KeybindingMap, Selector,
    SelectorItem, SessionView, SettingsView, TerminalRenderer, TerminalTheme, ThemePalette,
    BUILTIN_THEMES,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal, TerminalOptions, Viewport,
};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, ValueEnum)]
enum OutputMode {
    Text,
    Json,
    Rpc,
}

#[derive(Debug, Parser)]
#[command(name = "pi")]
#[command(version)]
#[command(about = "Native Rust CLI for pi")]
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

    #[arg(short = 't', long, value_delimiter = ',')]
    tools: Vec<String>,

    #[arg(long, value_delimiter = ',')]
    exclude_tools: Vec<String>,

    #[arg(long)]
    no_builtin_tools: bool,

    #[arg(long)]
    skill: Vec<PathBuf>,

    #[arg(long)]
    no_skills: bool,

    #[arg(long)]
    prompt_template: Vec<PathBuf>,

    #[arg(long)]
    no_prompt_templates: bool,

    #[arg(long)]
    theme: Option<String>,

    #[arg(long)]
    no_themes: bool,

    #[arg(long)]
    no_context_files: bool,

    #[arg(short = 'e', long = "extension")]
    extension: Vec<PathBuf>,

    #[arg(long)]
    no_extensions: bool,

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

    #[arg(long)]
    offline: bool,

    #[arg()]
    messages: Vec<String>,
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
        println!("usage: pi images [search]\n\nList image-generation models.");
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    let config = load_config(ConfigPaths::discover(cwd, None)?)?;
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
    let config = load_config(ConfigPaths::discover(cwd.clone(), None)?)?;
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
        "usage: pi generate-image --output <file> [--model <provider/model>] [--image <file>] <prompt>\n\nGenerate images through a configured image model and write output files locally."
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
        anyhow!("usage: pi login <provider> [--account <name>] [--api-key <key|env:VAR|->]")
    })?;
    let account = account.unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
    if api_key.is_none() {
        match provider.as_str() {
            "openai" | "openai-codex" => return run_codex_login(&paths, &account).await,
            "anthropic" => return run_claude_login(&paths, &account).await,
            _ => {}
        }
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
                    "usage: pi login {provider} --api-key <key|env:VAR|-> (OAuth login is available for: openai-codex, anthropic)"
                )
            })?,
    };
    if key.trim().is_empty() {
        return Err(anyhow!("api key is empty"));
    }
    let mut auth = read_auth_data(&paths.auth_path)?;
    auth.insert(&provider, &account, AuthCredential::ApiKey { key });
    write_auth_data(&paths.auth_path, &auth)?;
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
    let mut auth = read_auth_data(&paths.auth_path)?;
    let replaced = auth.credential("openai-codex", account).is_some();
    for provider in ["openai", "openai-codex"] {
        auth.insert(
            provider,
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires: tokens.expires,
                account_id: tokens.account_id.clone(),
            },
        );
    }
    write_auth_data(&paths.auth_path, &auth)?;
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
    let mut auth = read_auth_data(&paths.auth_path)?;
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
    write_auth_data(&paths.auth_path, &auth)?;
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
        provider.ok_or_else(|| anyhow!("usage: pi logout <provider> [--account <name>]"))?;
    let account = account.unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
    let mut auth = read_auth_data(&paths.auth_path)?;
    if auth.remove(&provider, &account).is_some() {
        write_auth_data(&paths.auth_path, &auth)?;
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
        return Err(anyhow!("usage: pi accounts status [--refresh]"));
    }
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
    let config = load_config(paths)?;
    let now = unix_seconds().unwrap_or(0) as i64;
    let rows = usage::collect_account_status(&config, refresh, None, now).await;
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
        return Err(anyhow!("usage: pi update [source|self|pi]"));
    }
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd.clone(), None)?;
    let target = args.first().map(String::as_str).unwrap_or("all");
    let sources = if target == "all" {
        configured_package_sources(&paths)?
    } else if matches!(target, "self" | "pi") {
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
    let paths = ConfigPaths::discover(cwd, None)?;
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
            "usage: pi config {action} <extension|skill|prompt|theme> <name> [-l]"
        )),
        [invalid, ..] => Err(anyhow!("unknown config argument: {invalid}")),
    }
}

fn print_package_config_summary() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
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
        [] => Err(anyhow!("usage: pi {command} <source> [-l]")),
        [_, extra, ..] => Err(anyhow!("unexpected package argument: {extra}")),
    }
}

fn package_settings_path(local: bool) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
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

fn read_auth_data(path: &Path) -> Result<AuthData> {
    if !path.exists() {
        return Ok(AuthData::default());
    }
    Ok(serde_json::from_str::<AuthData>(&fs::read_to_string(
        path,
    )?)?)
}

fn write_auth_data(path: &Path, auth: &AuthData) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_file_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(auth)?).as_bytes(),
    )?;
    Ok(())
}

fn default_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "openai-codex" => Some("CODEX_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "github-copilot" => Some("COPILOT_GITHUB_TOKEN"),
        "azure-openai-responses" => Some("AZURE_OPENAI_API_KEY"),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => Some("CLOUDFLARE_API_KEY"),
        "amazon-bedrock" => Some("AWS_BEARER_TOKEN_BEDROCK"),
        _ => None,
    }
}

fn print_package_help(command: &str) {
    match command {
        "install" => {
            println!("usage: pi install <source> [-l]\n\nRecord a package source in settings.")
        }
        "remove" => {
            println!("usage: pi remove <source> [-l]\n\nRemove a package source from settings.")
        }
        "update" => println!(
            "usage: pi update [source|self|pi]\n\nUpdate local git package sources without npm."
        ),
        "list" => {
            println!("usage: pi list\n\nList package sources from user and project settings.")
        }
        "config" => println!(
            "usage: pi config [show|list|disable <kind> <name>|enable <kind> <name>] [-l]\n\nShow active resource configuration or enable/disable resources."
        ),
        _ => {}
    }
}

fn print_auth_help(command: &str) {
    match command {
        "login" => println!(
            "usage: pi login <provider> [--account <name>] [--api-key <key|env:VAR|->]\n\nStore API-key auth in ~/.pi/agent/auth.json. Without --api-key, pi starts the OAuth login flow for providers that support it (openai-codex, anthropic); other providers store env:<provider default> when that environment variable is present. Without --account, the account is \"default\"."
        ),
        "logout" => println!(
            "usage: pi logout <provider> [--account <name>]\n\nRemove stored provider auth. Without --account, removes the \"default\" account."
        ),
        "accounts" => println!(
            "usage: pi accounts status [--refresh]\n\nShow auth and quota status for every configured account. --refresh bypasses the 15-minute usage cache."
        ),
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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
    validate_session_id_flags(&cli)?;
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd.clone(), cli.session_dir.clone())?;
    let project_trusted = if cli.approve {
        true
    } else if cli.no_approve {
        false
    } else {
        project_is_trusted(&paths.agent_dir, &cwd)?
    };
    let mut config = load_config_with_project_trust(paths, project_trusted)?;
    if cli.session_dir.is_none() && std::env::var_os(ENV_SESSION_DIR).is_none() {
        if let Some(session_dir) = &config.settings.session_dir {
            config.paths = config.paths.with_session_dir(session_dir)?;
        }
    }
    apply_cli_overrides(&cli, &cwd, &mut config)?;
    if cli.verbose {
        eprintln!("agent dir: {}", config.paths.agent_dir.display());
        eprintln!("session dir: {}", config.paths.session_dir.display());
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

    let systems = ReloadableSystems::from_config(&config, 1);
    let mut runtime = create_runtime(&cli, &cwd, &config, systems)?;
    select_initial_model(&mut runtime, &config, &cli)?;
    if cli.no_tools || cli.no_builtin_tools {
        runtime.set_disabled_tools(default_active_tool_names())?;
    } else {
        let mut disabled: BTreeSet<String> = cli.exclude_tools.iter().cloned().collect();
        if !cli.tools.is_empty() {
            for name in default_active_tool_names() {
                if !cli.tools.contains(&name) {
                    disabled.insert(name);
                }
            }
        }
        if !disabled.is_empty() {
            runtime.set_disabled_tools(disabled)?;
        }
    }
    if let Some(name) = &cli.name {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("--name requires a non-empty value"));
        }
        runtime.rename_session(Some(name.to_string()))?;
    }

    let stdin_is_terminal = io::stdin().is_terminal();
    if matches!(cli.mode, OutputMode::Rpc)
        && !stdin_is_terminal
        && !cli.print
        && cli.messages.is_empty()
    {
        return run_rpc(runtime, config, offline).await;
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
        let response = run_prompt_media(
            &mut runtime,
            &config,
            initial_prompt,
            initial_media,
            offline,
        )
        .await?;
        if let Some(path) = &cli.export {
            export_session(&runtime, path)?;
        }
        print_response(&cli.mode, &response);
        return Ok(());
    }

    run_interactive(runtime, config, offline).await
}

async fn run_rpc(mut runtime: Runtime, mut config: LoadedConfig, offline: bool) -> Result<()> {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(request) => request,
            Err(error) => {
                println!(
                    "{}",
                    rpc_error(serde_json::Value::Null, -32700, &error.to_string())
                );
                continue;
            }
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
            println!("{}", rpc_error(id, -32600, "missing method"));
            continue;
        };
        let result = match method {
            "prompt" => match rpc_prompt(&request) {
                Ok(prompt) => match run_prompt(&mut runtime, &config, prompt, offline).await {
                    Ok(message) => Ok(serde_json::json!({ "message": message })),
                    Err(error) => Err((1, error.to_string())),
                },
                Err(error) => Err((-32602, error)),
            },
            "reload" => {
                config = load_config(config.paths.clone())?;
                start_model_refresh(&config, offline, false);
                let next_generation = runtime.systems().config_generation + 1;
                match runtime.reload(ReloadableSystems::from_config(&config, next_generation)) {
                    Ok(report) => Ok(serde_json::json!({
                        "activeModelValid": report.active_model_valid,
                        "activeAccountValid": report.active_account_valid,
                        "removedActiveTools": report.removed_active_tools,
                    })),
                    Err(error) => Err((1, error.to_string())),
                }
            }
            "session" => Ok(serde_json::json!({
                "id": runtime.session().session_id,
                "cwd": runtime.session().cwd.display().to_string(),
                "file": runtime.store().map(|store| store.path().display().to_string()),
            })),
            "model" => match rpc_model(&request) {
                Ok(reference) => match resolve_model_reference(&config, &reference) {
                    Some(model) => {
                        runtime.set_active_model(Some(model.clone()))?;
                        persist_default_model(&mut config, &model)?;
                        Ok(serde_json::json!({
                            "provider": model.provider,
                            "id": model.id,
                        }))
                    }
                    None => Err((1, format!("model not found: {reference}"))),
                },
                Err(error) => Err((-32602, error)),
            },
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        match result {
            Ok(result) => println!("{}", rpc_result(id, result)),
            Err((code, message)) => println!("{}", rpc_error(id, code, &message)),
        }
    }
    Ok(())
}

fn rpc_prompt(request: &serde_json::Value) -> std::result::Result<String, String> {
    let params = request
        .get("params")
        .ok_or_else(|| "missing params".to_string())?;
    if let Some(prompt) = params.as_str() {
        return Ok(prompt.to_string());
    }
    params
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| "missing prompt".to_string())
}

fn rpc_model(request: &serde_json::Value) -> std::result::Result<String, String> {
    let params = request
        .get("params")
        .ok_or_else(|| "missing params".to_string())?;
    if let Some(model) = params.as_str() {
        return Ok(model.to_string());
    }
    params
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| "missing model".to_string())
}

fn rpc_result(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn rpc_error(id: serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}

fn apply_cli_overrides(cli: &Cli, cwd: &Path, config: &mut LoadedConfig) -> Result<()> {
    if let Some(system_prompt) = &cli.system_prompt {
        config.system_prompt = Some(resolve_text_or_file(cwd, system_prompt)?);
    }
    for prompt in &cli.append_system_prompt {
        config
            .append_system_prompt
            .push(resolve_text_or_file(cwd, prompt)?);
    }
    if cli.no_context_files {
        config.context_files.clear();
    }
    if !cli.models.is_empty() {
        config.models.retain(|model| {
            cli.models.iter().any(|pattern| {
                pattern == &model.id
                    || pattern == &model.provider
                    || pattern == &format!("{}/{}", model.provider, model.id)
                    || model.name.as_deref() == Some(pattern.as_str())
            })
        });
    }
    if cli.no_tools || cli.no_builtin_tools {
        config.settings.enabled_tools = Some(Vec::new());
    }
    if !cli.tools.is_empty() {
        config.settings.enabled_tools = Some(cli.tools.clone());
    }
    if cli.no_themes {
        config.settings.theme = None;
        config.settings.accent_color = None;
        config.themes.clear();
    } else if let Some(theme) = &cli.theme {
        let resolved = resolve_terminal_theme(config, theme)?;
        config.settings.theme = Some(resolved.name.clone());
        resolved
            .with_accent(config.settings.accent_color.as_deref())
            .map_err(|error| anyhow!(error))?;
    }
    if let Some(thinking) = &cli.thinking {
        config.settings.default_thinking_level = Some(thinking.clone());
    }
    if !cli.no_skills {
        for skill in &cli.skill {
            if skill.is_file() {
                config.skills.push(ResourceFile {
                    name: resource_name(skill),
                    path: skill.clone(),
                    content: fs::read_to_string(skill)?,
                });
            }
        }
    }
    if !cli.no_prompt_templates {
        for prompt_template in &cli.prompt_template {
            if prompt_template.is_file() {
                config.prompt_templates.push(ResourceFile {
                    name: resource_name(prompt_template),
                    path: prompt_template.clone(),
                    content: fs::read_to_string(prompt_template)?,
                });
            }
        }
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
        config.auth.insert(
            &provider,
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::ApiKey {
                key: api_key.clone(),
            },
        );
    }
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
        || std::env::var("PI_OFFLINE")
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
    if offline || !model_refresh_enabled(&config.settings) {
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
    if cache.version < MODEL_CACHE_VERSION {
        return true;
    }
    let Some(now) = unix_seconds() else {
        return true;
    };
    let ttl_seconds = ttl_hours.saturating_mul(60 * 60);
    now.saturating_sub(cache.refreshed_at) >= ttl_seconds
}

async fn refresh_model_cache(paths: ConfigPaths, auth: pi_config::AuthData) -> Result<()> {
    let mut refreshed_providers = BTreeSet::new();
    let mut refreshed_models = Vec::new();
    let mut diagnostics = Vec::new();

    if let Some(auth) = auth_for_provider(&auth, "anthropic", None) {
        match fetch_anthropic_models(auth).await {
            Ok(models) => {
                refreshed_providers.insert("anthropic".to_string());
                refreshed_models.extend(models);
            }
            Err(error) => diagnostics.push(format!("anthropic model refresh failed: {error}")),
        }
    }

    match auth_for_provider(&auth, "openai", None) {
        Some(ResolvedAuth::ApiKey(api_key)) => {
            match fetch_openai_api_models(
                "openai",
                ConfigProviderApi::OpenAiResponses,
                None,
                OPENAI_MODELS_URL,
                &api_key,
            )
            .await
            {
                Ok(models) => {
                    refreshed_providers.insert("openai".to_string());
                    refreshed_models.extend(models);
                }
                Err(error) => diagnostics.push(format!("openai model refresh failed: {error}")),
            }
        }
        Some(ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
            ..
        }) => {
            match fetch_chatgpt_backend_models(
                "openai",
                ConfigProviderApi::OpenAiResponses,
                None,
                &access_token,
                account_id.as_deref(),
            )
            .await
            {
                Ok(models) => {
                    refreshed_providers.insert("openai".to_string());
                    refreshed_models.extend(models);
                }
                Err(error) => diagnostics.push(format!("openai model refresh failed: {error}")),
            }
        }
        _ => {}
    }

    if let Some(auth) = auth_for_provider(&auth, "openai-codex", None) {
        match fetch_codex_models(auth).await {
            Ok(models) => {
                refreshed_providers.insert("openai-codex".to_string());
                refreshed_models.extend(models);
            }
            Err(error) => diagnostics.push(format!("openai-codex model refresh failed: {error}")),
        }
    }

    for (provider, base_url) in [
        ("zai", "https://api.z.ai/api/paas/v4"),
        ("zai-coding", "https://api.z.ai/api/coding/paas/v4"),
        ("moonshotai", "https://api.moonshot.ai/v1"),
        ("kimi-coding-openai", "https://api.kimi.com/coding/v1"),
    ] {
        let Some(ResolvedAuth::ApiKey(api_key)) = auth_for_provider(&auth, provider, None) else {
            continue;
        };
        let models_url = format!("{base_url}/models");
        match fetch_openai_api_models(
            provider,
            ConfigProviderApi::OpenAi,
            Some(base_url.to_string()),
            &models_url,
            &api_key,
        )
        .await
        {
            Ok(models) => {
                refreshed_providers.insert(provider.to_string());
                refreshed_models.extend(models);
            }
            Err(error) => diagnostics.push(format!("{provider} model refresh failed: {error}")),
        }
    }

    let mut claude_code_version = None;
    if matches!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ClaudeCodeOAuth { .. })
    ) {
        match fetch_latest_claude_code_version().await {
            Ok(version) => {
                set_claude_code_version(&version);
                claude_code_version = Some(version);
            }
            Err(error) => {
                diagnostics.push(format!("claude code version check failed: {error}"));
            }
        }
    }

    if refreshed_providers.is_empty() && diagnostics.is_empty() {
        return Ok(());
    }

    let existing = read_model_cache(&paths.model_cache_path)
        .ok()
        .flatten()
        .unwrap_or_default();
    let existing_refreshed_at = existing.refreshed_at;
    let existing_claude_code_version = existing.claude_code_version.clone();
    let mut models = existing
        .models
        .into_iter()
        .filter(|model| !refreshed_providers.contains(&model.provider))
        .collect::<Vec<_>>();
    models.extend(refreshed_models);
    write_model_cache(
        &paths.model_cache_path,
        &ModelCache {
            refreshed_at: unix_seconds().unwrap_or(existing_refreshed_at),
            version: MODEL_CACHE_VERSION,
            models,
            diagnostics: Vec::new(),
            claude_code_version: claude_code_version.or(existing_claude_code_version),
        },
    )?;
    if !diagnostics.is_empty() {
        return Err(anyhow!(diagnostics.join("; ")));
    }
    Ok(())
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
            provider: "anthropic".to_string(),
            id: model.id,
            name: Some(model.display_name),
            api: ConfigProviderApi::Anthropic,
            base_url: None,
        }));
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(models),
        }
    }
    Ok(models)
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
        })
        .collect())
}

async fn fetch_codex_models(auth: ResolvedAuth) -> Result<Vec<ModelDefinition>> {
    match auth {
        ResolvedAuth::ApiKey(api_key) => {
            fetch_openai_api_models(
                "openai-codex",
                ConfigProviderApi::OpenAiCodexResponses,
                Some("https://chatgpt.com/backend-api".to_string()),
                OPENAI_MODELS_URL,
                &api_key,
            )
            .await
        }
        ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
            ..
        } => {
            fetch_chatgpt_backend_models(
                "openai-codex",
                ConfigProviderApi::OpenAiCodexResponses,
                Some("https://chatgpt.com/backend-api".to_string()),
                &access_token,
                account_id.as_deref(),
            )
            .await
        }
        ResolvedAuth::ClaudeCodeOAuth { .. } => Err(anyhow!("unsupported Codex auth type")),
    }
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
    let ids = collect_codex_model_ids(&response.json::<serde_json::Value>().await?, provider);
    if ids.is_empty() {
        return Err(anyhow!(
            "no selectable {provider} model IDs in response from {endpoint}"
        ));
    }
    Ok(ids
        .into_iter()
        .map(|id| ModelDefinition {
            provider: provider.to_string(),
            name: Some(model_display_name(&id)),
            id,
            api: api.clone(),
            base_url: base_url.clone(),
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

fn infer_cli_provider(cli: &Cli, config: &LoadedConfig) -> Option<String> {
    cli.provider
        .clone()
        .or_else(|| {
            cli.model.as_deref().and_then(|model| {
                model
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string())
            })
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
    let mut bytes = fs::read(&path)?;
    let mut mime_type = media_mime_type(&path)?;
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
        path: Some(path.display().to_string()),
        width,
        height,
    })
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

fn create_runtime(
    cli: &Cli,
    cwd: &Path,
    config: &LoadedConfig,
    systems: ReloadableSystems,
) -> Result<Runtime> {
    if cli.no_session {
        return Ok(Runtime::new(
            SessionState::new("ephemeral", cwd.to_path_buf()),
            systems,
        ));
    }

    if let Some(reference) = &cli.fork {
        let path = resolve_session_reference(&config.paths.session_dir, reference)?;
        let (_store, source_state) = SessionStore::open(path)?;
        let (store, state) = SessionStore::fork(&config.paths.session_dir, &source_state, false)?;
        return Ok(Runtime::with_store(state, systems, store));
    }

    if let Some(session_id) = &cli.session_id {
        let path = config.paths.session_dir.join(format!("{session_id}.jsonl"));
        if path.exists() {
            let (store, state) = SessionStore::open(path)?;
            return Ok(Runtime::with_store(state, systems, store));
        }
        let (store, state) =
            SessionStore::create_with_id(&config.paths.session_dir, cwd.to_path_buf(), session_id)?;
        return Ok(Runtime::with_store(state, systems, store));
    }

    if let Some(reference) = &cli.session {
        let path = resolve_session_reference(&config.paths.session_dir, reference)?;
        let (store, state) = SessionStore::open(path)?;
        return Ok(Runtime::with_store(state, systems, store));
    }

    if cli.r#continue {
        if let Some(path) = most_recent_session(&config.paths.session_dir, Some(cwd))? {
            let (store, state) = SessionStore::open(path)?;
            return Ok(Runtime::with_store(state, systems, store));
        }
    }

    if cli.resume {
        if let Some(path) = most_recent_session(&config.paths.session_dir, None)? {
            let (store, state) = SessionStore::open(path)?;
            return Ok(Runtime::with_store(state, systems, store));
        }
    }

    let (store, state) = SessionStore::create(&config.paths.session_dir, cwd.to_path_buf())?;
    Ok(Runtime::with_store(state, systems, store))
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

fn select_initial_model(runtime: &mut Runtime, config: &LoadedConfig, cli: &Cli) -> Result<()> {
    if runtime.session().active_model.is_some() && cli.provider.is_none() && cli.model.is_none() {
        return Ok(());
    }
    let model = if let (Some(provider), Some(id)) = (&cli.provider, &cli.model) {
        Some(ModelRef {
            provider: provider.clone(),
            id: id.clone(),
        })
    } else if let Some(model) = &cli.model {
        resolve_model_reference(config, model)
    } else if let (Some(provider), Some(id)) = (
        &config.settings.default_provider,
        &config.settings.default_model,
    ) {
        Some(ModelRef {
            provider: provider.clone(),
            id: id.clone(),
        })
    } else {
        config
            .models
            .iter()
            .find(|model| model.provider == "faux")
            .or_else(|| {
                config
                    .models
                    .iter()
                    .find(|model| has_auth_for_provider(&config.auth, &model.provider, None))
            })
            .map(|model| ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            })
    };
    runtime.set_active_model(model)?;
    Ok(())
}

fn resolve_model_reference(config: &LoadedConfig, reference: &str) -> Option<ModelRef> {
    if let Ok(index) = reference.parse::<usize>() {
        if index > 0 {
            return config.models.get(index - 1).map(|model| ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            });
        }
    }
    if let Some((provider, id)) = reference.split_once('/') {
        return Some(ModelRef {
            provider: provider.to_string(),
            id: id.to_string(),
        });
    }
    config
        .models
        .iter()
        .find(|model| model.id == reference || model.name.as_deref() == Some(reference))
        .map(|model| ModelRef {
            provider: model.provider.clone(),
            id: model.id.clone(),
        })
}

async fn run_interactive(
    mut runtime: Runtime,
    mut config: LoadedConfig,
    offline: bool,
) -> Result<()> {
    let mut app = TuiApp::new(&config, &runtime);
    if let Some(hint) = resume_hint(&runtime) {
        app.push(TuiEntryKind::System, hint);
    }
    let mut auto_restart = AutoRestart::from_env();
    enable_raw_mode()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let _restore = TerminalRestore;
    let mut viewport_height = terminal_viewport_height();
    let mut terminal = new_tui_terminal(viewport_height)?;
    terminal.clear()?;

    let mut restart = false;
    loop {
        app.refresh_chrome(&config, &runtime);
        redraw_tui(&mut terminal, &mut app, &config)?;
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
                    let _ = mouse;
                    false
                }
                Event::Paste(text) => {
                    app.paste_text(&text);
                    false
                }
                Event::Resize(_, _) => {
                    resize_tui_viewport(&mut terminal, &mut viewport_height)?;
                    false
                }
                _ => false,
            };
            if quit {
                break;
            }
            continue;
        }
        if auto_restart.should_restart()? {
            app.push(TuiEntryKind::System, "rebuilt; restarting");
            redraw_tui(&mut terminal, &mut app, &config)?;
            restart = true;
            break;
        }
    }
    let dump_transcript = std::env::var("PI_TUI_E2E_DUMP").ok().as_deref() == Some("1");
    if dump_transcript {
        terminal.clear()?;
    }
    drop(terminal);
    drop(_restore);
    if dump_transcript {
        println!();
        println!("{}", app.transcript_text());
    }
    if restart {
        restart_current_process()?;
    }
    Ok(())
}

/// Startup note for resumed sessions whose last turn was cut off (the
/// conversation ends at a tool result) or that still hold queued follow-ups.
fn resume_hint(runtime: &Runtime) -> Option<String> {
    let queued = runtime.session().queued_messages.len();
    let mid_turn = matches!(
        runtime.session().messages.last(),
        Some(message) if message.role == MessageRole::Tool
    );
    match (mid_turn, queued) {
        (false, 0) => None,
        (true, 0) => {
            Some("resumed mid-turn; send a prompt to pick up where it stopped".to_string())
        }
        (false, n) => Some(format!(
            "{n} queued message(s) pending; they run after your next prompt (/queue to list, /queue-clear to clear)"
        )),
        (true, n) => Some(format!(
            "resumed mid-turn with {n} queued message(s) pending; send a prompt to continue"
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
        let enabled = std::env::var("PI_DOGFOOD_AUTO_RESTART").ok().as_deref() == Some("1");
        if !enabled {
            return Self {
                executable: PathBuf::new(),
                modified: None,
                pending: None,
            };
        }
        let executable = std::env::var_os("PI_DOGFOOD_RESTART_EXE")
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
fn restart_current_process() -> Result<()> {
    let executable = std::env::var_os("PI_DOGFOOD_RESTART_EXE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| anyhow!("failed to resolve restart executable"))?;
    let error = Command::new(executable)
        .args(restart_args_preserving_session_model())
        .exec();
    Err(error.into())
}

#[cfg(not(unix))]
fn restart_current_process() -> Result<()> {
    Err(anyhow!("dogfood auto-restart is only supported on Unix"))
}

fn restart_args_preserving_session_model() -> Vec<OsString> {
    strip_restart_model_args(std::env::args_os().skip(1))
}

fn strip_restart_model_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut output = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--model" || arg == "--provider" {
            let _ = args.next();
            continue;
        }
        let text = arg.to_string_lossy();
        if text.starts_with("--model=") || text.starts_with("--provider=") {
            continue;
        }
        output.push(arg);
    }
    output
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(io::stdout(), DisableBracketedPaste);
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiSelectorState {
    kind: String,
    title: String,
    items: Vec<SelectorItem>,
    filtered_indices: Vec<usize>,
    selected: usize,
    query: String,
    thinking_level: Option<String>,
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
        };
        state.refresh_filter();
        state
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
        let Some(item) = self.selected_item() else {
            return;
        };
        let Some(model) = model_ref_from_value(&item.value) else {
            return;
        };
        let levels = model_thinking_levels(&model);
        if levels.is_empty() {
            return;
        }
        let current = self
            .thinking_level
            .as_deref()
            .and_then(normalized_thinking_level)
            .filter(|level| levels.contains(level))
            .or_else(|| default_thinking_for_model(&model))
            .unwrap_or("off");
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

    fn selected_thinking_level(&self) -> Option<String> {
        let item = self.selected_item()?;
        let model = model_ref_from_value(&item.value)?;
        let levels = model_thinking_levels(&model);
        if levels.is_empty() {
            return None;
        }
        let level = self
            .thinking_level
            .as_deref()
            .and_then(normalized_thinking_level)
            .filter(|level| levels.contains(level))
            .or_else(|| default_thinking_for_model(&model))?;
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

fn model_thinking_levels(model: &ModelRef) -> &'static [&'static str] {
    match model.provider.as_str() {
        "anthropic" => {
            if model.id.contains("opus") && anthropic_supports_adaptive_thinking(&model.id) {
                &["off", "high", "xhigh", "max"]
            } else if anthropic_supports_adaptive_thinking(&model.id) {
                &["off", "low", "medium", "high", "xhigh"]
            } else if model.id.contains("claude-") {
                &["off", "minimal", "low", "medium", "high"]
            } else {
                &[]
            }
        }
        "openai" | "openai-codex" | "azure-openai-responses" => {
            if model.id.starts_with("gpt-5")
                || model.id.starts_with("gpt-6")
                || model.id.contains("codex")
            {
                &["off", "minimal", "low", "medium", "high", "xhigh"]
            } else {
                &[]
            }
        }
        _ => &[],
    }
}

fn default_thinking_for_model(model: &ModelRef) -> Option<&'static str> {
    let levels = model_thinking_levels(model);
    if levels.contains(&"xhigh") {
        Some("xhigh")
    } else if levels.contains(&"high") {
        Some("high")
    } else {
        None
    }
}

fn normalized_thinking_level(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => Some("off"),
        "minimal" | "min" => Some("minimal"),
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "extra-high" | "extra_high" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct TuiApp {
    entries: Vec<TuiEntry>,
    input: String,
    input_cursor: usize,
    quit_armed_since: Option<Instant>,
    editor_state: EditorState,
    last_shell_command: Option<String>,
    multiline: Option<Vec<String>>,
    selector: Option<TuiSelectorState>,
    header_title: String,
    header_line: String,
    status: String,
    show_hardware_cursor: bool,
    live_entry_index: Option<usize>,
    history_cursor: Option<usize>,
    history_draft: Option<String>,
    history_draft_typed_newlines: Option<Vec<usize>>,
    typed_input_newlines: Vec<usize>,
    todos: Vec<TodoItem>,
    todos_expanded: bool,
    diff_panel: Option<DiffPanelState>,
}

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
    fn new(config: &LoadedConfig, runtime: &Runtime) -> Self {
        let mut app = Self {
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
        let model = runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .unwrap_or_else(|| "no model".to_string());
        self.header_title = format!(
            " pi  {}  {} ",
            config
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "system".to_string()),
            model
        );
        let session = runtime.session();
        self.header_line = format!(
            "{}  {}  queue:{}",
            session.cwd.display(),
            session
                .name
                .clone()
                .unwrap_or_else(|| session.session_id.chars().take(8).collect()),
            session.queued_messages.len()
        );
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
                format_model_tool_message(message)
            } else {
                message.content.clone()
            };
            self.push(kind, text);
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
                self.history_draft = Some(self.input.clone());
                self.history_draft_typed_newlines = Some(self.typed_input_newlines.clone());
                history.len().saturating_sub(1)
            }
        };
        self.history_cursor = Some(next);
        self.input = history[next].clone();
        self.input_cursor = self.input.len();
        self.typed_input_newlines = newline_offsets(&self.input);
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
            self.typed_input_newlines = newline_offsets(&self.input);
        } else {
            self.history_cursor = None;
            self.input = self.history_draft.take().unwrap_or_default();
            self.typed_input_newlines =
                self.history_draft_typed_newlines.take().unwrap_or_default();
        }
        self.input_cursor = self.input.len();
    }

    fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft = None;
        self.history_draft_typed_newlines = None;
    }

    fn paste_text(&mut self, text: &str) {
        self.insert_input_str(text);
    }

    fn insert_input_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let cursor = self.input_cursor;
        self.input.insert_str(cursor, text);
        let inserted = text.len();
        for offset in &mut self.typed_input_newlines {
            if *offset >= cursor {
                *offset += inserted;
            }
        }
        self.input_cursor += inserted;
        self.reset_history_navigation();
    }

    #[cfg(test)]
    fn set_input(&mut self, value: impl Into<String>) {
        self.input = value.into();
        self.input_cursor = self.input.len();
        self.typed_input_newlines.clear();
        self.reset_history_navigation();
    }

    fn push_input_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.insert_input_str(ch.encode_utf8(&mut buf));
    }

    fn pop_input_char(&mut self) {
        let Some((offset, ch)) = self.input[..self.input_cursor].char_indices().next_back() else {
            self.reset_history_navigation();
            return;
        };
        let removed = ch.len_utf8();
        self.input.drain(offset..offset + removed);
        self.input_cursor = offset;
        self.typed_input_newlines.retain_mut(|newline| {
            if ch == '\n' && *newline == offset {
                return false;
            }
            if *newline > offset {
                *newline -= removed;
            }
            true
        });
        self.reset_history_navigation();
    }

    fn move_cursor_left(&mut self) {
        if let Some((offset, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input_cursor = offset;
        }
    }

    fn move_cursor_right(&mut self) {
        if let Some(ch) = self.input[self.input_cursor..].chars().next() {
            self.input_cursor += ch.len_utf8();
        }
    }

    fn cursor_to_line_start(&mut self) {
        self.input_cursor = self.input[..self.input_cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    fn cursor_to_line_end(&mut self) {
        self.input_cursor = self.input[self.input_cursor..]
            .find('\n')
            .map(|index| self.input_cursor + index)
            .unwrap_or(self.input.len());
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.typed_input_newlines.clear();
        self.reset_history_navigation();
    }

    fn clear_visible(&mut self) {
        self.entries.clear();
        self.clear_input();
        self.multiline = None;
        self.selector = None;
        self.live_entry_index = None;
    }

    fn insert_input_newline(&mut self) {
        let cursor = self.input_cursor;
        self.input.insert(cursor, '\n');
        for offset in &mut self.typed_input_newlines {
            if *offset >= cursor {
                *offset += 1;
            }
        }
        self.typed_input_newlines.push(cursor);
        self.typed_input_newlines.sort_unstable();
        self.input_cursor = cursor + 1;
        self.reset_history_navigation();
    }

    /// Double-press gate for quit keys: the first press clears any draft and
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
        if !self.input.is_empty() {
            self.clear_input();
        }
        false
    }

    fn typed_input_rows(&self) -> usize {
        self.typed_input_newlines.len() + 1
    }

    fn push_placeholder(&mut self, kind: TuiEntryKind, text: impl Into<String>) -> usize {
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
        }
    }

    fn append_entry(&mut self, index: usize, text: &str) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.text.push_str(text);
        }
    }

    fn insert_entry(&mut self, index: usize, kind: TuiEntryKind, text: impl Into<String>) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        let index = index.min(self.entries.len());
        self.entries.insert(index, TuiEntry { kind, text });
        if let Some(live_index) = self.live_entry_index.as_mut() {
            if index <= *live_index {
                *live_index += 1;
            }
        }
    }

    fn finish_live_entry(&mut self) {
        self.live_entry_index = None;
    }

    fn drop_live_entry(&mut self) {
        let Some(index) = self.live_entry_index.take() else {
            return;
        };
        if index < self.entries.len() {
            self.entries.remove(index);
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

fn newline_offsets(input: &str) -> Vec<usize> {
    input
        .char_indices()
        .filter_map(|(offset, ch)| (ch == '\n').then_some(offset))
        .collect()
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

struct TuiSurface<'a> {
    terminal: &'a mut TuiTerminal,
    viewport_height: &'a mut u16,
}

fn new_tui_terminal(height: u16) -> Result<TuiTerminal> {
    let backend = CrosstermBackend::new(io::stdout());
    Ok(Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height.max(1)),
        },
    )?)
}

fn terminal_viewport_height() -> u16 {
    crossterm::terminal::size()
        .map(|(_, height)| height.max(1))
        .unwrap_or(24)
}

fn resize_tui_viewport(terminal: &mut TuiTerminal, viewport_height: &mut u16) -> Result<()> {
    let desired = terminal_viewport_height();
    if desired == *viewport_height {
        terminal.autoresize()?;
        return Ok(());
    }
    *terminal = new_tui_terminal(desired)?;
    terminal.clear()?;
    *viewport_height = desired;
    Ok(())
}

fn redraw_tui(terminal: &mut TuiTerminal, app: &mut TuiApp, config: &LoadedConfig) -> Result<()> {
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

fn visible_transcript_with_thinking(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
) -> VisibleTranscript {
    let width = width.max(1);
    let target_height = height.saturating_add(TRANSCRIPT_RENDER_OVERSCAN_ROWS) as usize;
    let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut scan_height: usize = 0;
    #[cfg(test)]
    let mut entries_used = 0;

    for entry in entries.iter().rev() {
        if hide_thinking && entry.kind == TuiEntryKind::Thinking {
            continue;
        }
        let mut chunk = render_entry_lines(std::slice::from_ref(entry), palette);
        if chunks.is_empty() {
            while chunk.last().map(Line::width) == Some(0) {
                chunk.pop();
            }
        }
        if chunk.is_empty() {
            continue;
        }

        let chunk = wrap_transcript_lines(chunk, width);
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

fn wrap_transcript_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in lines {
        wrap_transcript_line(line, width.max(1), &mut rows);
    }
    rows
}

fn wrap_transcript_line(line: Line<'static>, width: usize, rows: &mut Vec<Line<'static>>) {
    let mut current = Vec::new();
    let mut current_width: usize = 0;
    let mut row_has_content = false;
    let mut emitted_row = false;

    for span in line.spans {
        let style = span.style;
        let mut segment = String::new();
        for ch in span.content.chars() {
            let ch_width = transcript_char_width(ch);
            if ch_width > 0 && current_width > 0 && current_width.saturating_add(ch_width) > width {
                push_transcript_segment(&mut current, &mut segment, style);
                rows.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
                emitted_row = true;
            }
            segment.push(ch);
            current_width = current_width.saturating_add(ch_width);
            row_has_content = true;
            if current_width >= width {
                push_transcript_segment(&mut current, &mut segment, style);
                rows.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
                row_has_content = false;
                emitted_row = true;
            }
        }
        push_transcript_segment(&mut current, &mut segment, style);
    }

    if row_has_content || !emitted_row {
        rows.push(Line::from(current));
    }
}

fn push_transcript_segment(current: &mut Vec<Span<'static>>, segment: &mut String, style: Style) {
    if !segment.is_empty() {
        current.push(Span::styled(std::mem::take(segment), style));
    }
}

fn transcript_char_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else if ch.is_control() {
        0
    } else {
        1
    }
}

fn draw_tui(frame: &mut Frame<'_>, app: &TuiApp, config: &LoadedConfig) {
    let palette = terminal_theme(config).unwrap_or_default().palette;
    frame.render_widget(Block::default().style(palette.base()), frame.area());
    let slash_matches = slash_command_matches(config, app);
    let slash_match_height = slash_matches.len().min(SLASH_MATCH_LIMIT) as u16;
    let input_height = input_area_height(app, frame.area().height, slash_match_height);
    let todo_height = todo_panel_height(app);
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(todo_height),
            Constraint::Length(1),
            Constraint::Length(input_height),
            Constraint::Length(slash_match_height),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let transcript_area = if app.diff_panel.is_some() {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(DIFF_PANEL_WIDTH)])
            .split(root[0]);
        draw_diff_panel(frame, columns[1], app, &palette);
        columns[0]
    } else {
        root[0]
    };
    draw_transcript(frame, transcript_area, app, config, &palette);
    draw_todo_panel(frame, root[1], app, &palette);
    // root[2] is an intentional blank spacer so the input surface never butts
    // directly against the chat transcript.
    draw_input(frame, root[3], app, &palette);
    draw_slash_matches(frame, root[4], &slash_matches, &palette);
    draw_footer(frame, root[5], app, &palette);
    draw_selector_overlay(frame, app, &palette);
    set_tui_cursor(frame, root[3], app);
}

const DIFF_PANEL_WIDTH: u16 = 48;
const TODO_COLLAPSED_ROWS: usize = 5;
const TODO_EXPANDED_ROW_LIMIT: usize = 15;

fn todo_panel_height(app: &TuiApp) -> u16 {
    if app.todos.is_empty() {
        return 0;
    }
    if app.todos_expanded {
        return app.todos.len().min(TODO_EXPANDED_ROW_LIMIT) as u16;
    }
    let rows = app.todos.len().min(TODO_COLLAPSED_ROWS);
    let overflow = usize::from(app.todos.len() > TODO_COLLAPSED_ROWS);
    (rows + overflow) as u16
}

fn draw_todo_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    if area.height == 0 || app.todos.is_empty() {
        return;
    }
    let visible = if app.todos_expanded {
        app.todos.len().min(TODO_EXPANDED_ROW_LIMIT)
    } else {
        app.todos.len().min(TODO_COLLAPSED_ROWS)
    };
    let mut lines = Vec::new();
    for todo in app.todos.iter().take(visible) {
        let (marker, style) = match todo.status {
            TodoStatus::Completed => ("✓", Style::default().fg(palette.success)),
            TodoStatus::InProgress => ("●", Style::default().fg(palette.accent)),
            TodoStatus::Pending => ("○", palette.secondary()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::raw(todo.content.clone()),
        ]));
    }
    if !app.todos_expanded && app.todos.len() > visible {
        let done = app
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count();
        lines.push(Line::from(Span::styled(
            format!(
                "… +{} more ({} done) · ctrl+t to expand",
                app.todos.len() - visible,
                done
            ),
            palette.secondary(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
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

fn input_area_height(app: &TuiApp, frame_height: u16, slash_match_height: u16) -> u16 {
    let desired = input_desired_height(app);
    let available = frame_height
        .saturating_sub(slash_match_height)
        .saturating_sub(2)
        .max(1);
    desired.min(available)
}

fn input_desired_height(app: &TuiApp) -> u16 {
    if app.multiline.is_some() {
        3
    } else {
        app.typed_input_rows()
            .saturating_add(1)
            .max(3)
            .min(u16::MAX as usize) as u16
    }
}

fn draw_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    config: &LoadedConfig,
    palette: &ThemePalette,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let visible = visible_transcript_with_thinking(
        &app.entries,
        area.width as usize,
        area.height,
        config.settings.hide_thinking_block.unwrap_or(false),
        palette,
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

fn push_tool_lines(lines: &mut Vec<Line<'static>>, text: &str, palette: &ThemePalette) {
    let mut parts = text.lines();
    let state = parts.next().unwrap_or_default();
    let detail = parts.next().unwrap_or_default();
    let title = match state {
        "running bash" => "Running bash".to_string(),
        "completed bash" => "Ran bash".to_string(),
        "failed bash" => "Failed bash".to_string(),
        "running" => format!("Running {detail}"),
        "completed" => format!("Ran {detail}"),
        "failed" => format!("Failed {detail}"),
        _ if state.starts_with("running ") => {
            format!("Running {}", state.trim_start_matches("running "))
        }
        _ if state.starts_with("completed ") => {
            format!("Ran {}", state.trim_start_matches("completed "))
        }
        _ => "Ran tool".to_string(),
    };
    let marker_style = if state.starts_with("failed") {
        Style::default().fg(palette.error)
    } else if state.starts_with("running") {
        Style::default().fg(palette.accent)
    } else {
        palette.secondary()
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{BULLET} "), marker_style),
        Span::styled(
            title,
            Style::default()
                .fg(palette.foreground)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if !detail.is_empty() && !matches!(state, "running" | "completed" | "failed") {
        lines.push(Line::from(Span::styled(
            format!("  {detail}"),
            palette.secondary().add_modifier(Modifier::BOLD),
        )));
    }
    let output = parts.collect::<Vec<_>>().join("\n");
    if !output.is_empty() {
        for line in output.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                palette.secondary(),
            )));
        }
    } else if state.starts_with("completed") {
        lines.push(Line::from(Span::styled(
            "  (no output)",
            palette.secondary(),
        )));
    }
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let prompt = input_prompt(app);
    let input_style = Style::default().fg(palette.foreground).bg(palette.surface);
    let muted_style = Style::default().fg(palette.accent).bg(palette.surface);
    let mut lines = render_input_lines(app, area.height as usize, input_style, muted_style);
    if lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(prompt, muted_style),
            Span::styled("", input_style),
        ]));
    }
    frame.render_widget(Block::default().style(input_style), area);
    let paragraph = Paragraph::new(lines)
        .style(input_style)
        .wrap(Wrap { trim: false })
        .block(Block::default().style(input_style));
    frame.render_widget(paragraph, area);
}

fn input_prompt(app: &TuiApp) -> &'static str {
    if app.multiline.is_some() {
        "multi> "
    } else {
        "pi> "
    }
}

fn render_input_lines(
    app: &TuiApp,
    area_height: usize,
    input_style: Style,
    muted_style: Style,
) -> Vec<Line<'static>> {
    if area_height == 0 {
        return Vec::new();
    }
    let prompt = input_prompt(app);
    let parts = app.input.split('\n').collect::<Vec<_>>();
    let start = input_visible_start(app, area_height);
    let metrics = input_metrics(app, area_height);
    let mut lines = Vec::new();
    for _ in 0..metrics.top_padding {
        lines.push(Line::from(Span::styled("", input_style)));
    }
    for (visible_offset, part) in parts.iter().enumerate().skip(start) {
        let prefix = if visible_offset == start {
            prompt.to_string()
        } else {
            " ".repeat(prompt.len())
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, muted_style),
            Span::styled((*part).to_string(), input_style),
        ]));
    }
    if app.multiline.is_some() && lines.len() < area_height {
        lines.push(Line::from(Span::styled(
            "submit with . on its own line",
            muted_style,
        )));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputMetrics {
    top_padding: usize,
    visible_index: usize,
    current_width: usize,
}

fn input_metrics(app: &TuiApp, area_height: usize) -> InputMetrics {
    if area_height == 0 {
        return InputMetrics {
            top_padding: 0,
            visible_index: 0,
            current_width: 0,
        };
    }
    let input_rows = input_visible_rows(app, area_height);
    let parts_count = app.input.split('\n').count();
    let visible_count = parts_count.min(input_rows);
    let top_padding = if app.multiline.is_some() {
        input_rows.saturating_sub(visible_count)
    } else if area_height > visible_count {
        1
    } else {
        0
    };
    let cursor_line = app.input[..app.input_cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let line_start = app.input[..app.input_cursor]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let current_width = app.input[line_start..app.input_cursor].chars().count();
    InputMetrics {
        top_padding,
        visible_index: cursor_line.saturating_sub(input_visible_start(app, area_height)),
        current_width,
    }
}

fn input_visible_start(app: &TuiApp, area_height: usize) -> usize {
    let parts_count = app.input.split('\n').count();
    let visible_count = parts_count.min(input_visible_rows(app, area_height));
    let end_anchored = parts_count.saturating_sub(visible_count);
    let cursor_line = app.input[..app.input_cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    end_anchored.min(cursor_line)
}

fn input_visible_rows(app: &TuiApp, area_height: usize) -> usize {
    if area_height == 0 {
        0
    } else if app.multiline.is_some() {
        area_height.saturating_sub(1).max(1)
    } else {
        area_height
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let line = if app.header_line.is_empty() {
        app.status.clone()
    } else {
        format!("{}  {}", app.status, app.header_line)
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
    let prompt_width = input_prompt(app).chars().count() as u16;
    let metrics = input_metrics(app, input_area.height as usize);
    let max_x = input_area.right().saturating_sub(2);
    let x = input_area
        .x
        .saturating_add(prompt_width)
        .saturating_add(metrics.current_width as u16)
        .min(max_x);
    let y = input_area
        .y
        .saturating_add(metrics.top_padding as u16)
        .saturating_add(metrics.visible_index as u16)
        .min(input_area.bottom().saturating_sub(1));
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

async fn handle_tui_key(
    key: KeyEvent,
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    offline: bool,
) -> Result<bool> {
    if app.selector.is_some() {
        handle_tui_selector_key(key, app, runtime, config)?;
        app.status = footer_status(config, runtime, &app.editor_state);
        return Ok(false);
    }
    if app.diff_panel.is_some() && handle_diff_panel_key(&key, app, runtime) {
        app.refresh_chrome(config, runtime);
        return Ok(false);
    }
    if let Some(name) = key_event_name(&key) {
        let bindings = keybinding_map(config);
        if bindings.matches("interrupt", &name) {
            if app.quit_requested(Instant::now()) {
                return Ok(true);
            }
            app.refresh_chrome(config, runtime);
            return Ok(false);
        }
        app.quit_armed_since = None;
        if bindings.matches("todos", &name) {
            app.todos_expanded = !app.todos_expanded;
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
        KeyCode::Up if app.multiline.is_none() => {
            app.history_previous();
        }
        KeyCode::Down if app.multiline.is_none() => {
            app.history_next();
        }
        KeyCode::Enter if is_shift_enter(&key) => {
            app.insert_input_newline();
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
    app.clear_input();
    if !line.is_empty() {
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
        KeyCode::Enter if app.input.trim().is_empty() => {
            let detail = panel
                .files
                .get(panel.selected)
                .map(|entry| build_diff_detail(&runtime.session().cwd, entry));
            if let Some(detail) = detail {
                panel.detail = Some(detail);
            }
        }
        KeyCode::Esc if app.input.is_empty() => app.diff_panel = None,
        _ => return false,
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingKeyOutcome {
    Ignored,
    Changed,
    Interrupt,
    Quit,
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
    if let Some(name) = key_event_name(&key) {
        let bindings = keybinding_map(config);
        if bindings.matches("interrupt", &name) {
            if app.quit_requested(Instant::now()) {
                return StreamingKeyOutcome::Quit;
            }
            return StreamingKeyOutcome::Interrupt;
        }
        if bindings.matches("steer", &name) {
            let line = app.input.trim().to_string();
            app.clear_input();
            if !line.is_empty() {
                steering.send(line.clone());
                app.push(TuiEntryKind::System, format!("steering> {line}"));
            }
            return StreamingKeyOutcome::Changed;
        }
        if bindings.matches("todos", &name) {
            app.todos_expanded = !app.todos_expanded;
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
        KeyCode::Esc => StreamingKeyOutcome::Interrupt,
        KeyCode::Backspace => {
            app.pop_input_char();
            StreamingKeyOutcome::Changed
        }
        KeyCode::Enter if is_shift_enter(&key) => {
            app.insert_input_newline();
            StreamingKeyOutcome::Changed
        }
        KeyCode::Enter => {
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
        "queue-clear" => {
            let cleared = followups.clear();
            app.push(
                TuiEntryKind::System,
                format!("cleared {cleared} pending follow-up(s)"),
            );
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
        "quit" => StreamingKeyOutcome::Quit,
        _ => {
            app.push(
                TuiEntryKind::System,
                format!("/{command} is not available while a turn is running"),
            );
            StreamingKeyOutcome::Changed
        }
    }
}

fn drain_streaming_tui_events(
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    followups: &FollowUpQueue,
    steering: &SteeringMailbox,
    config: &LoadedConfig,
) -> Result<(bool, Option<TurnControl>)> {
    let mut changed = false;
    let mut control = None;
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match handle_streaming_tui_key(key, app, followups, steering, config) {
                    StreamingKeyOutcome::Ignored => {}
                    StreamingKeyOutcome::Changed => changed = true,
                    StreamingKeyOutcome::Interrupt => control = Some(TurnControl::Interrupted),
                    StreamingKeyOutcome::Quit => {
                        control = Some(TurnControl::Quit);
                    }
                }
            }
            Event::Paste(text) => {
                app.paste_text(&text);
                changed = true;
            }
            Event::Resize(_, _) => {
                resize_tui_viewport(surface.terminal, surface.viewport_height)?;
                changed = true;
            }
            _ => {}
        }
    }
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
}

const SPINNER_FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

fn activity_status(started: Instant, activity: &Activity, pending_followups: usize) -> String {
    let elapsed = started.elapsed();
    let frame = SPINNER_FRAMES[(elapsed.as_millis() / 100) as usize % SPINNER_FRAMES.len()];
    let phase = match activity {
        Activity::Waiting => "waiting".to_string(),
        Activity::Thinking => "thinking".to_string(),
        Activity::Writing => "writing".to_string(),
        Activity::Tool(name) => format!("running {name}"),
    };
    let queued = if pending_followups > 0 {
        format!(" · +{pending_followups} queued")
    } else {
        String::new()
    };
    format!(
        "{frame} {phase} · {}s{queued} · esc interrupt",
        elapsed.as_secs()
    )
}

fn apply_turn_event(
    app: &mut TuiApp,
    entry_index: usize,
    thinking_index: usize,
    saw_delta: &mut bool,
    activity: &mut Activity,
    event: &TurnEvent,
) {
    match event {
        TurnEvent::Provider(StreamEvent::Text(text)) => {
            *activity = Activity::Writing;
            apply_stream_delta(app, entry_index, saw_delta, text);
        }
        TurnEvent::Provider(StreamEvent::Thinking(text)) => {
            *activity = Activity::Thinking;
            app.append_entry(thinking_index, text);
        }
        TurnEvent::ToolStarted { name } => {
            *activity = Activity::Tool(name.clone());
        }
        TurnEvent::ToolFinished { .. } => {
            *activity = Activity::Waiting;
        }
        _ => {}
    }
}

fn is_shift_enter(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter) && key.modifiers.contains(KeyModifiers::SHIFT)
}

fn handle_tui_selector_key(
    key: KeyEvent,
    app: &mut TuiApp,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.selector = None;
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
            if is_settings {
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
    let thinking_level = if matches!(kind, "model" | "models" | "scoped-models") {
        runtime
            .session()
            .active_thinking_level
            .clone()
            .or_else(|| config.settings.default_thinking_level.clone())
    } else {
        None
    };
    let state = TuiSelectorState::new(kind, selector, query, thinking_level);
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
            let model = resolve_model_reference(config, &item.value)
                .ok_or_else(|| anyhow!("model not found: {}", item.value))?;
            let thinking = selector.selected_thinking_level();
            runtime.set_active_model(Some(model.clone()))?;
            runtime.set_active_thinking_level(thinking.clone())?;
            persist_default_model(config, &model)?;
            app.push(
                TuiEntryKind::System,
                format_model_selection(&model, thinking.as_deref()),
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
            if config
                .auth
                .remove(&item.value, DEFAULT_ACCOUNT_NAME)
                .is_some()
            {
                write_auth_file(config)?;
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
        "followUpMode" => {
            let next = if follow_up_mode(config) == "one-at-a-time" {
                "all"
            } else {
                "one-at-a-time"
            };
            config.settings.follow_up_mode = Some(next.to_string());
            write_user_setting(&config.paths.settings_path, &["followUpMode"], next.into())?;
            format!("setting followUpMode: {next}")
        }
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
            let prompt = lines.join("\n");
            app.multiline = None;
            let quit =
                submit_tui_prompt(app, surface, runtime, config, prompt, Vec::new(), offline).await;
            return Ok(quit);
        }
        lines.push(line);
        return Ok(false);
    }
    if line == "/quit" {
        for diagnostic in
            notify_extension_lifecycle(&config.extensions, "shutdown", &runtime.session().cwd)
        {
            app.push(TuiEntryKind::Error, diagnostic);
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
            clear_terminal_screen(surface.terminal)?;
        }
        "/image-models" => app.push(TuiEntryKind::System, format_image_models(config, "")),
        "/thinking" => app.push(
            TuiEntryKind::System,
            format!(
                "thinking: {}",
                active_thinking_label(runtime, config).unwrap_or_else(|| "-".to_string())
            ),
        ),
        "/model" | "/models" | "/scoped-models" => {
            open_tui_selector(app, config, runtime, "model", "")?
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
        "/accounts" => {
            let checking = app.push_placeholder(
                TuiEntryKind::System,
                "checking account status...".to_string(),
            );
            redraw_tui(surface.terminal, app, config)?;
            let now = unix_seconds().unwrap_or(0) as i64;
            let rows = usage::collect_account_status(config, false, None, now).await;
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
            format_resources("skills", &config.skills),
        ),
        "/prompts" => app.push(
            TuiEntryKind::System,
            format_resources("prompts", &config.prompt_templates),
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
            format_session_tree(&config.paths.session_dir)?,
        ),
        "/summaries" => app.push(TuiEntryKind::System, format_summaries(runtime)),
        "/compact" => {
            let record = runtime.compact_messages(CompactionKind::Manual)?;
            app.push(
                TuiEntryKind::System,
                format!(
                    "compacted: omitted {} message(s), retained {} message(s)",
                    record.omitted_messages, record.retained_messages
                ),
            );
        }
        "/copy" => app.push(TuiEntryKind::System, copy_last_assistant_message(runtime)?),
        "/theme" => open_tui_selector(app, config, runtime, "theme", "")?,
        "/multiline" => {
            app.multiline = Some(Vec::new());
            app.push(
                TuiEntryKind::System,
                "enter multiline prompt; submit . on its own line",
            );
        }
        "/reload" => {
            let lifecycle_diagnostics =
                notify_extension_lifecycle(&config.extensions, "reload", &runtime.session().cwd);
            *config = load_config(config.paths.clone())?;
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
                }
                Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
            }
        }
        _ if line.starts_with("/image-models ") => {
            let search = line.trim_start_matches("/image-models ").trim();
            app.push(TuiEntryKind::System, format_image_models(config, search));
        }
        _ if line.starts_with("/models ") => {
            let query = line.trim_start_matches("/models ").trim();
            open_tui_selector(app, config, runtime, "model", query)?;
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
            let message = line.trim_start_matches("/queue ").trim().to_string();
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
            app.push(
                TuiEntryKind::System,
                select_from_selector_message(config, runtime, &line)?,
            );
        }
        _ if line.starts_with("/model ") => {
            let reference = line.trim_start_matches("/model ").trim();
            let model = resolve_model_reference(config, reference)
                .ok_or_else(|| anyhow!("model not found: {reference}"))?;
            let thinking = active_thinking_level(runtime, config, &model);
            runtime.set_active_model(Some(model.clone()))?;
            runtime.set_active_thinking_level(thinking.clone())?;
            persist_default_model(config, &model)?;
            app.push(
                TuiEntryKind::System,
                format_model_selection(&model, thinking.as_deref()),
            );
        }
        _ if line.starts_with("/thinking ") => {
            let level = line.trim_start_matches("/thinking ").trim();
            let level = normalized_thinking_level(level)
                .ok_or_else(|| anyhow!("unknown thinking level: {level}"))?;
            if let Some(model) = runtime.session().active_model.clone() {
                if !model_thinking_levels(&model).contains(&level) {
                    return Err(anyhow!(
                        "thinking level {level} is not supported by {}/{}",
                        model.provider,
                        model.id
                    ));
                }
            }
            runtime.set_active_thinking_level(if level == "off" {
                None
            } else {
                Some(level.to_string())
            })?;
            app.push(TuiEntryKind::System, format!("thinking: {level}"));
        }
        _ if line.starts_with("/skill:") => {
            let (name, input) = split_resource_command(&line, "/skill:");
            let skill = find_resource(&config.skills, name)
                .ok_or_else(|| anyhow!("skill not found: {name}"))?;
            let prompt = if input.is_empty() {
                skill.content.clone()
            } else {
                format!("{}\n\n{}", skill.content, input)
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
        _ if line.starts_with("/extension:") => {
            let (name, input) = split_resource_command(&line, "/extension:");
            let extension = find_resource(&config.extensions, name)
                .ok_or_else(|| anyhow!("extension not found: {name}"))?;
            if is_executable_extension(&extension.path) {
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
        _ if line.starts_with("/prompt ") => {
            let rest = line.trim_start_matches("/prompt ").trim();
            let (name, input) = split_once_text(rest);
            let template = find_resource(&config.prompt_templates, name)
                .ok_or_else(|| anyhow!("prompt template not found: {name}"))?;
            let prompt = expand_prompt_template(&template.content, input);
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
            let (store, state) =
                SessionStore::create(&config.paths.session_dir, runtime.session().cwd.clone())?;
            runtime.replace_session(state, Some(store));
            app.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line.starts_with("/resume") => {
            let reference = line.trim_start_matches("/resume").trim();
            if reference.is_empty() {
                app.push(
                    TuiEntryKind::System,
                    format_sessions(&config.paths.session_dir)?,
                );
            } else {
                let path = resolve_session_reference(&config.paths.session_dir, reference)?;
                let (store, state) = SessionStore::open(path)?;
                runtime.replace_session(state, Some(store));
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
            runtime.rename_session((!name.is_empty()).then(|| name.to_string()))?;
            app.push(TuiEntryKind::System, format_session(runtime));
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
        _ if line.starts_with("/export ") => {
            let path = PathBuf::from(line.trim_start_matches("/export ").trim());
            export_session(runtime, &path)?;
            app.push(TuiEntryKind::System, format!("exported {}", path.display()));
        }
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
                if config.auth.remove(provider, account).is_some() {
                    write_auth_file(config)?;
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
        _ => {
            return Ok(
                submit_tui_prompt(app, surface, runtime, config, line, Vec::new(), offline).await,
            )
        }
    }
    Ok(false)
}

fn clear_terminal_screen(terminal: &mut TuiTerminal) -> Result<()> {
    execute!(
        terminal.backend_mut(),
        MoveTo(0, 0),
        TerminalClear(ClearType::All),
        TerminalClear(ClearType::Purge)
    )?;
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
    let entry_index = if terminal_progress_enabled(config) {
        let index = app.push_placeholder(TuiEntryKind::Tool, format_bash_running(&command));
        redraw_tui(surface.terminal, app, config)?;
        Some(index)
    } else {
        None
    };
    match run_excluded_bash(runtime, command.clone()).await {
        Ok(output) => {
            if let Some(index) = entry_index {
                app.replace_entry(index, format_bash_completed(&command, &output));
                app.finish_live_entry();
            } else {
                app.push(TuiEntryKind::Tool, output);
            }
            app.last_shell_command = Some(command);
        }
        Err(error) => {
            if let Some(index) = entry_index {
                app.replace_entry(index, format!("failed bash\n{error}"));
                app.finish_live_entry();
            } else {
                app.push(TuiEntryKind::Error, format!("{error}"));
            }
        }
    }
    Ok(true)
}

async fn submit_tui_prompt(
    app: &mut TuiApp,
    surface: &mut TuiSurface<'_>,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> bool {
    app.editor_state.record_history(prompt.clone());
    app.push(TuiEntryKind::User, prompt.clone());
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
    if !message.contains("429 Too Many Requests") {
        return message;
    }
    let model = runtime
        .session()
        .active_model
        .as_ref()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .unwrap_or_else(|| "-".to_string());
    let thinking = active_thinking_label(runtime, config).unwrap_or_else(|| "-".to_string());
    format!(
        "{message}\n\nAnthropic rate-limited this request for model {model} with thinking {thinking}. Try /thinking high, /thinking off, or /model anthropic/claude-sonnet-4-6. Claude Code may still work elsewhere if it is using a different model, thinking level, or quota pool."
    )
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
    maybe_auto_compact(runtime, config, false)?;
    match run_prompt_once_tui(app, surface, runtime, config, prompt, media, offline).await? {
        TurnControl::Completed => {}
        control => return Ok(control),
    }
    while let Some(prompt) = runtime.session().queued_messages.first().cloned() {
        let remaining = runtime
            .session()
            .queued_messages
            .iter()
            .skip(1)
            .cloned()
            .collect();
        runtime.replace_queued_messages(remaining)?;
        app.push(TuiEntryKind::System, format!("queued> {prompt}"));
        match run_prompt_once_tui(app, surface, runtime, config, prompt, Vec::new(), offline)
            .await?
        {
            TurnControl::Completed => {}
            control => return Ok(control),
        }
        if follow_up_mode(config) == "one-at-a-time" {
            break;
        }
    }
    Ok(TurnControl::Completed)
}

fn follow_up_mode(config: &LoadedConfig) -> &str {
    config
        .settings
        .follow_up_mode
        .as_deref()
        .unwrap_or("one-at-a-time")
}

fn steering_mailbox(config: &LoadedConfig) -> SteeringMailbox {
    let mode = match config.settings.steering_mode.as_deref() {
        Some("all") => SteeringMode::All,
        _ => SteeringMode::OneAtATime,
    };
    SteeringMailbox::new(mode)
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
    let kind = response_kind_for_prompt(&prompt);
    let progress_enabled = terminal_progress_enabled(config);
    let thinking_index = app.entries.len();
    if kind != TuiEntryKind::Tool {
        app.entries.push(TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: String::new(),
        });
    }
    let entry_index = app.push_placeholder(
        kind.clone(),
        if kind == TuiEntryKind::Tool && progress_enabled {
            format_tool_running(&prompt)
        } else if kind == TuiEntryKind::Tool {
            "running...".to_string()
        } else {
            "Working...".to_string()
        },
    );
    redraw_tui(surface.terminal, app, config)?;
    let provider = provider_for_runtime(runtime, config, offline).await?;
    let message_start = runtime.session().messages.len();
    let steering = steering_mailbox(config);
    if kind == TuiEntryKind::Tool {
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
            app.replace_entry(entry_index, format_tool_completed(&tool_prompt, &response));
        } else {
            app.replace_entry(entry_index, response);
        }
        app.finish_live_entry();
        redraw_tui(surface.terminal, app, config)?;
        return Ok(TurnControl::Completed);
    }

    let mut saw_delta = false;
    let followups = FollowUpQueue::default();
    let activity_started = Instant::now();
    let mut activity = Activity::Waiting;
    let (delta_tx, delta_rx) = std::sync::mpsc::channel::<TurnEvent>();
    let stream_end = {
        let turn = run_user_turn_streaming_events_with_media(
            runtime,
            provider.as_ref(),
            prompt,
            media,
            &steering,
            move |event| {
                let _ = delta_tx.send(event.clone());
            },
        );
        tokio::pin!(turn);
        let mut tick = tokio::time::interval(Duration::from_millis(STREAM_RENDER_INTERVAL_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                result = &mut turn => {
                    while let Ok(event) = delta_rx.try_recv() {
                        apply_turn_event(app, entry_index, thinking_index, &mut saw_delta, &mut activity, &event);
                    }
                    app.status = activity_status(activity_started, &activity, followups.pending());
                    redraw_tui(surface.terminal, app, config)?;
                    break StreamEnd::Finished(result);
                }
                _ = tick.tick() => {
                    while let Ok(event) = delta_rx.try_recv() {
                        apply_turn_event(app, entry_index, thinking_index, &mut saw_delta, &mut activity, &event);
                    }
                    let (_, control) =
                        drain_streaming_tui_events(surface, app, &followups, &steering, config)?;
                    if let Some(control) = control {
                        break match control {
                            TurnControl::Quit => StreamEnd::Quit,
                            _ => StreamEnd::Interrupted,
                        };
                    }
                    app.status = activity_status(activity_started, &activity, followups.pending());
                    redraw_tui(surface.terminal, app, config)?;
                }
            }
        }
    };
    app.status = footer_status(config, runtime, &app.editor_state);
    for followup in followups.drain() {
        runtime.queue_message(followup)?;
    }
    // Steering input that arrived after the turn's last injection point
    // becomes a regular follow-up instead of being dropped.
    for leftover in steering.drain() {
        runtime.queue_message(leftover)?;
    }

    // Interrupt and quit drop the in-flight turn future, which cancels the
    // provider request; whatever already streamed stays visible.
    let turn_result = match stream_end {
        StreamEnd::Finished(result) => result,
        StreamEnd::Interrupted | StreamEnd::Quit => {
            if saw_delta {
                app.finish_live_entry();
            } else {
                app.drop_live_entry();
            }
            insert_new_tool_messages(app, runtime, message_start, entry_index);
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
            insert_new_tool_messages(app, runtime, message_start, entry_index);
            redraw_tui(surface.terminal, app, config)?;
            return Err(error.into());
        }
    };

    if !saw_delta {
        app.replace_entry(entry_index, response);
    }
    insert_new_tool_messages(app, runtime, message_start, entry_index);
    app.finish_live_entry();
    redraw_tui(surface.terminal, app, config)?;
    Ok(TurnControl::Completed)
}

fn insert_new_tool_messages(
    app: &mut TuiApp,
    runtime: &Runtime,
    message_start: usize,
    assistant_entry_index: usize,
) {
    let tool_messages = runtime
        .session()
        .messages
        .iter()
        .skip(message_start)
        .filter(|message| message.role == MessageRole::Tool)
        .map(format_model_tool_message)
        .collect::<Vec<_>>();
    for (offset, text) in tool_messages.into_iter().enumerate() {
        app.insert_entry(assistant_entry_index + offset, TuiEntryKind::Tool, text);
    }
}

fn format_model_tool_message(message: &ConversationMessage) -> String {
    let tool_name = message.tool_name.as_deref().unwrap_or("tool");
    if message.content.trim().is_empty() {
        return format!("completed {tool_name}");
    }
    format!("completed {tool_name}\n{}", message.content)
}

fn collect_diff_file_entries(cwd: &Path, paths: &[String]) -> Vec<DiffFileEntry> {
    let stats = git_numstat(cwd, paths);
    let untracked = git_untracked_paths(cwd);
    paths
        .iter()
        .map(|path| {
            let (added, removed) = stats.get(path.as_str()).copied().unwrap_or((None, None));
            DiffFileEntry {
                path: path.clone(),
                added,
                removed,
                untracked: untracked.contains(path),
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
        Some("/read" | "/write" | "/edit" | "/grep" | "/find" | "/ls" | "/bash")
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
    format!("running {command}\n{detail}")
}

fn format_tool_completed(prompt: &str, output: &str) -> String {
    let (command, detail) = split_once_text(prompt.trim());
    if detail.is_empty() {
        return format!("completed {command}\n{output}");
    }
    format!("completed {command}\n{detail}\n{output}")
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

async fn run_prompt(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    offline: bool,
) -> Result<String> {
    run_prompt_media(runtime, config, prompt, Vec::new(), offline).await
}

async fn run_prompt_media(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<String> {
    run_prompt_once(runtime, config, prompt, media, offline, false).await
}

fn maybe_auto_compact(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    stream_output: bool,
) -> Result<()> {
    const AUTO_COMPACT_MESSAGE_LIMIT: usize = 24;
    if !auto_compaction_enabled(config) {
        return Ok(());
    }
    if runtime.session().messages.len() <= AUTO_COMPACT_MESSAGE_LIMIT {
        return Ok(());
    }
    let record = runtime.compact_messages(CompactionKind::Automatic)?;
    if stream_output && record.omitted_messages > 0 {
        println!(
            "auto-compacted: omitted {} message(s)",
            record.omitted_messages
        );
    }
    Ok(())
}

fn auto_compaction_enabled(config: &LoadedConfig) -> bool {
    config
        .settings
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.enabled)
        .unwrap_or(true)
}

async fn run_prompt_once(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
    stream_output: bool,
) -> Result<String> {
    let provider = provider_for_runtime(runtime, config, offline).await?;
    if !stream_output {
        if media.is_empty() {
            return run_user_turn(runtime, provider.as_ref(), prompt)
                .await
                .map_err(Into::into);
        }
        return run_user_turn_streaming_with_media(
            runtime,
            provider.as_ref(),
            prompt,
            media,
            &SteeringMailbox::default(),
            |_| {},
        )
        .await
        .map_err(Into::into);
    }
    let mut printed = false;
    let response = if media.is_empty() {
        run_user_turn_streaming(runtime, provider.as_ref(), prompt, |delta| {
            printed = true;
            print!("{delta}");
            let _ = io::stdout().flush();
        })
        .await?
    } else {
        run_user_turn_streaming_with_media(
            runtime,
            provider.as_ref(),
            prompt,
            media,
            &SteeringMailbox::default(),
            |delta| {
                printed = true;
                print!("{delta}");
                let _ = io::stdout().flush();
            },
        )
        .await?
    };
    if printed {
        println!();
    } else if !response.is_empty() {
        println!("{response}");
    }
    Ok(response)
}

async fn provider_for_runtime(
    runtime: &Runtime,
    config: &LoadedConfig,
    offline: bool,
) -> Result<Box<dyn pi_ai::Provider>> {
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
    let definition = config
        .models
        .iter()
        .find(|candidate| candidate.provider == model.provider && candidate.id == model.id)
        .ok_or_else(|| {
            anyhow!(
                "active model is not in models config: {}/{}",
                model.provider,
                model.id
            )
        })?;
    let thinking_level = active_thinking_level(runtime, config, &model);
    let resolved_auth = oauth_refresh::refresh_expiring_auth(
        &reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        &config.paths.auth_path,
        &config.auth,
        &definition.provider,
        runtime.session().active_account.as_deref(),
        None,
        unix_seconds().unwrap_or(0),
    )
    .await?;
    Ok(create_provider(ProviderConfig {
        thinking_budget_tokens: thinking_budget_tokens(config, thinking_level.as_deref()),
        thinking_level,
        model,
        api: map_provider_api(&definition.api),
        base_url: definition.base_url.clone(),
        auth: map_provider_auth(resolved_auth),
        session_id: Some(runtime.session().session_id.clone()),
    }))
}

fn active_thinking_level(
    runtime: &Runtime,
    config: &LoadedConfig,
    model: &ModelRef,
) -> Option<String> {
    let level = runtime
        .session()
        .active_thinking_level
        .as_deref()
        .or(config.settings.default_thinking_level.as_deref())
        .and_then(normalized_thinking_level)
        .or_else(|| default_thinking_for_model(model))?;
    if model_thinking_levels(model).contains(&level) && level != "off" {
        Some(level.to_string())
    } else {
        None
    }
}

fn active_thinking_label(runtime: &Runtime, config: &LoadedConfig) -> Option<String> {
    let model = runtime.session().active_model.as_ref()?;
    active_thinking_level(runtime, config, model)
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

fn map_provider_api(api: &ConfigProviderApi) -> AiProviderApi {
    match api {
        ConfigProviderApi::Faux => AiProviderApi::Faux,
        ConfigProviderApi::OpenAi => AiProviderApi::OpenAi,
        ConfigProviderApi::OpenAiResponses => AiProviderApi::OpenAiResponses,
        ConfigProviderApi::OpenAiCodexResponses => AiProviderApi::OpenAiCodexResponses,
        ConfigProviderApi::AzureOpenAiResponses => AiProviderApi::AzureOpenAiResponses,
        ConfigProviderApi::Anthropic => AiProviderApi::Anthropic,
        ConfigProviderApi::Google => AiProviderApi::Google,
        ConfigProviderApi::GoogleVertex => AiProviderApi::GoogleVertex,
        ConfigProviderApi::Bedrock => AiProviderApi::Bedrock,
        ConfigProviderApi::Mistral => AiProviderApi::Mistral,
    }
}

fn print_response(mode: &OutputMode, response: &str) {
    match mode {
        OutputMode::Text => println!("{response}"),
        OutputMode::Json | OutputMode::Rpc => {
            println!("{}", serde_json::json!({ "message": response }))
        }
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

fn format_sessions(session_dir: &Path) -> Result<String> {
    let mut lines = Vec::new();
    for (index, session) in SessionStore::list(session_dir)?.into_iter().enumerate() {
        let name = session.name.unwrap_or_else(|| "-".to_string());
        lines.push(format!(
            "{}.\t{}\t{}\t{}",
            index + 1,
            session.session_id,
            name,
            session.cwd.display()
        ));
    }
    Ok(if lines.is_empty() {
        "no sessions".to_string()
    } else {
        lines.join("\n")
    })
}

fn format_session_tree(session_dir: &Path) -> Result<String> {
    let mut lines = Vec::new();
    for session in SessionStore::list(session_dir)? {
        let parent = session.parent_session_id.unwrap_or_else(|| "-".to_string());
        let summary = session.branch_summary.unwrap_or_else(|| "-".to_string());
        lines.push(format!(
            "{}\tparent:{parent}\tsummary:{summary}\t{}",
            session.session_id,
            session.cwd.display()
        ));
    }
    Ok(if lines.is_empty() {
        "no sessions".to_string()
    } else {
        lines.join("\n")
    })
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
    output.push_str("scrollback\tmouse wheel, terminal selection");
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
        active_thinking_label(runtime, config).unwrap_or_else(|| "-".to_string()),
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
    completions.extend(resource_command_completions(
        prefix,
        "/skill:",
        &config.skills,
    ));
    completions.extend(resource_command_completions(
        prefix,
        "/prompt ",
        &config.prompt_templates,
    ));
    completions.extend(
        theme_names(config)
            .into_iter()
            .map(|name| format!("/theme {name}"))
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
        "pi-editor-{}-{}.txt",
        std::process::id(),
        unique_temp_suffix()
    ));
    fs::write(&path, initial)?;
    if let Ok(command) = std::env::var("PI_EDITOR_COMMAND") {
        let command = command.replace("{file}", &shell_quote(&path.display().to_string()));
        run_editor_command(&command)?;
    } else {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .map_err(|_| anyhow!("set PI_EDITOR_COMMAND, VISUAL, or EDITOR"))?;
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
            let model = resolve_model_reference(config, &item.value)
                .ok_or_else(|| anyhow!("model not found: {}", item.value))?;
            let thinking = active_thinking_level(runtime, config, &model);
            runtime.set_active_model(Some(model.clone()))?;
            runtime.set_active_thinking_level(thinking.clone())?;
            persist_default_model(config, &model)?;
            Ok(format_model_selection(&model, thinking.as_deref()))
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
            if config
                .auth
                .remove(&item.value, DEFAULT_ACCOUNT_NAME)
                .is_some()
            {
                write_auth_file(config)?;
                Ok(format!("removed stored auth for {}", item.value))
            } else {
                Ok(format!("no stored auth for {}", item.value))
            }
        }
        _ => Err(anyhow!("unknown selector: {kind}")),
    }
}

fn selector_for_kind(config: &LoadedConfig, runtime: &Runtime, kind: &str) -> Result<Selector> {
    match kind {
        "model" | "models" | "scoped-models" => Ok(Selector::new(
            "model",
            sorted_models(&config.models)
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
                .map(|session| SelectorItem {
                    label: session.name.unwrap_or_else(|| session.session_id.clone()),
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

fn account_selector_items(config: &LoadedConfig, runtime: &Runtime) -> Vec<SelectorItem> {
    let Some(model) = runtime.session().active_model.clone() else {
        return Vec::new();
    };
    let active = runtime.session().active_account.as_deref();
    let mut items = vec![SelectorItem {
        label: "auto (default resolution)".to_string(),
        value: String::new(),
        active: active.is_none(),
    }];
    items.extend(
        listed_accounts_for_provider(&config.auth, &model.provider)
            .into_iter()
            .map(|(account, source)| SelectorItem {
                label: match source {
                    AccountSource::Stored => account.clone(),
                    AccountSource::Environment => format!("{account} (environment, read-only)"),
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
        SelectorItem {
            label: format!("follow-up mode: {}", follow_up_mode(config)),
            value: "followUpMode".to_string(),
            active: follow_up_mode(config) == "one-at-a-time",
        },
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

fn expand_prompt_template(template: &str, input: &str) -> String {
    if template.contains("{{input}}") {
        return template.replace("{{input}}", input);
    }
    if input.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n{input}")
    }
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
    Ok(SessionStore::open(path)?.1)
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
    let target = if reference.is_empty() {
        runtime
            .store()
            .map(|store| store.path().to_path_buf())
            .ok_or_else(|| anyhow!("ephemeral session cannot be deleted"))?
    } else {
        resolve_session_reference(&config.paths.session_dir, reference)?
    };
    let deleting_current = runtime
        .store()
        .map(|store| store.path() == target)
        .unwrap_or(false);
    fs::remove_file(&target)?;
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

fn copy_last_assistant_message(runtime: &Runtime) -> Result<String> {
    let Some(message) = runtime
        .session()
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
    else {
        return Ok("no assistant message".to_string());
    };
    let mut output = message.content.clone();
    if let Some(command) = copy_to_clipboard(&message.content)? {
        output.push_str(&format!("\ncopied to clipboard via {command}"));
    } else {
        output.push_str("\nclipboard unavailable");
    }
    Ok(output)
}

fn copy_to_clipboard(text: &str) -> Result<Option<String>> {
    if let Ok(command) = std::env::var("PI_CLIPBOARD_COMMAND") {
        run_clipboard_command(&command, text)?;
        return Ok(Some(command));
    }
    for command in clipboard_commands() {
        if run_clipboard_command(&command, text).is_ok() {
            return Ok(Some(command));
        }
    }
    Ok(None)
}

fn clipboard_commands() -> Vec<String> {
    let mut commands = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push("pbcopy".to_string());
    }
    if cfg!(target_os = "windows") {
        commands.push("clip.exe".to_string());
    }
    commands.extend([
        "wl-copy".to_string(),
        "xclip -selection clipboard".to_string(),
        "xsel --clipboard --input".to_string(),
    ]);
    commands
}

fn run_clipboard_command(command: &str, text: &str) -> Result<()> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(stdin) = child.stdin.as_mut() else {
        return Err(anyhow!("clipboard command did not open stdin"));
    };
    stdin.write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("clipboard command failed: {command}"))
    }
}

fn write_auth_file(config: &LoadedConfig) -> Result<()> {
    if let Some(parent) = config.paths.auth_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_file_atomic(
        &config.paths.auth_path,
        serde_json::to_string_pretty(&config.auth)?.as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "pi-cli-extension-protocol-{}",
            unique_temp_suffix()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("bad-ext");
        fs::write(&path, "").expect("write extension");
        fs::write(
            root.join("bad-ext.pi-extension.json"),
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
            "pi-cli-config-resource-state-{}",
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
            paths: ConfigPaths {
                cwd: PathBuf::from("."),
                agent_dir: PathBuf::from(".pi/agent"),
                session_dir: PathBuf::from(".pi/agent/sessions"),
                settings_path: PathBuf::from(".pi/agent/settings.json"),
                project_settings_path: PathBuf::from(".pi/settings.json"),
                auth_path: PathBuf::from(".pi/agent/auth.json"),
                models_path: PathBuf::from(".pi/agent/models.json"),
                model_cache_path: PathBuf::from(".pi/agent/model-cache.json"),
                keybindings_path: PathBuf::from(".pi/agent/keybindings.json"),
            },
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
            image_models: Vec::new(),
            keybindings: Vec::new(),
            context_files: Vec::new(),
            extensions: vec![test_resource("json-ext")],
            skills: vec![test_resource("review")],
            prompt_templates: vec![test_resource("fix")],
            themes: vec![test_resource("dark")],
            diagnostics: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
        };

        assert_eq!(
            command_completions(&config, "/extension:j"),
            ["/extension:json-ext"]
        );
        assert!(command_completions(&config, "/skill:r").contains(&"/skill:review".to_string()));
        assert!(command_completions(&config, "/prompt f").contains(&"/prompt fix".to_string()));
        assert!(command_completions(&config, "/theme d").contains(&"/theme dark".to_string()));
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
        let root = std::env::temp_dir().join(format!("pi-cli-theme-{}", unique_temp_suffix()));
        fs::create_dir_all(root.join("themes")).unwrap();
        let mut config = account_test_config(AuthData::default());
        config.paths = ConfigPaths {
            cwd: root.clone(),
            agent_dir: root.clone(),
            settings_path: root.join("settings.json"),
            project_settings_path: root.join(".pi/settings.json"),
            ..test_config_paths()
        };
        fs::write(
            &config.paths.settings_path,
            r#"{"unknownSetting":123,"defaultProvider":"faux","defaultModel":"echo"}"#,
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
        });
        session.tool_history.push(pi_core::ToolEvent {
            id: "tool-1".into(),
            name: "read".into(),
            result: "contents".into(),
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
                        return &buffer[(x as u16, y)];
                    }
                }
                panic!("missing rendered text {text}");
            };
            assert_eq!(cell_for("neutral-answer").fg, palette.foreground);
            assert_eq!(cell_for("secondary-thinking").fg, palette.muted);
            assert_eq!(cell_for("secondary-output").fg, palette.muted);
            assert_eq!(cell_for("error-output").fg, palette.error);
            assert_eq!(cell_for("pi> ").fg, palette.accent);
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
            let mut terminal = Terminal::new(TestBackend::new(30, 3)).unwrap();
            terminal
                .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &palette))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(0, 0)].fg, palette.success);
            assert_eq!(buffer[(0, 1)].fg, palette.accent);
            assert_eq!(buffer[(0, 2)].fg, palette.muted);
            assert_eq!(buffer[(2, 2)].fg, Color::Reset);
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
            models: Vec::new(),
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

    #[tokio::test]
    async fn provider_construction_tolerates_bound_account_without_matching_credential() {
        let mut config = account_test_config(AuthData::default());
        config.models.push(ModelDefinition {
            provider: "faux".to_string(),
            id: "echo".to_string(),
            name: None,
            api: ConfigProviderApi::Faux,
            base_url: None,
        });
        let runtime = account_test_runtime("faux", Some("work"));

        provider_for_runtime(&runtime, &config, false)
            .await
            .expect("faux provider builds regardless of account binding");
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

    #[test]
    fn gpt6_thinking_levels_are_available() {
        for provider in ["openai", "openai-codex", "azure-openai-responses"] {
            let model = ModelRef {
                provider: provider.to_string(),
                id: "gpt-6.1-sol".to_string(),
            };
            assert!(model_thinking_levels(&model).contains(&"high"));
            assert!(model_thinking_levels(&model).contains(&"xhigh"));
            assert_eq!(default_thinking_for_model(&model), Some("xhigh"));
        }
    }

    #[test]
    fn anthropic_thinking_levels_follow_model_version() {
        let opus_5 = ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-opus-5-5".to_string(),
        };
        assert_eq!(
            model_thinking_levels(&opus_5),
            &["off", "high", "xhigh", "max"]
        );

        let sonnet_4_6 = ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
        };
        assert_eq!(
            model_thinking_levels(&sonnet_4_6),
            &["off", "low", "medium", "high", "xhigh"]
        );

        let sonnet_4_5 = ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-5".to_string(),
        };
        assert_eq!(
            model_thinking_levels(&sonnet_4_5),
            &["off", "minimal", "low", "medium", "high"]
        );
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
    fn streaming_ctrl_c_interrupts_then_second_press_quits() {
        let mut app = TuiApp::default();
        app.push_placeholder(TuiEntryKind::Assistant, "streaming");
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();

        let first = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(first, StreamingKeyOutcome::Interrupt);

        let second = handle_streaming_tui_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
            &followups,
            &steering,
            &config,
        );
        assert_eq!(second, StreamingKeyOutcome::Quit);
    }

    #[test]
    fn auto_restart_debounces_binary_changes() {
        let dir = std::env::temp_dir().join(format!("pi-autorestart-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let exe = dir.join("pi");
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
        });
        let hint = resume_hint(&Runtime::new(session.clone(), ReloadableSystems::default()))
            .expect("mid-turn hint");
        assert!(hint.contains("mid-turn"));

        session.queued_messages = vec!["follow up".to_string()];
        let hint = resume_hint(&Runtime::new(session, ReloadableSystems::default()))
            .expect("mid-turn and queue hint");
        assert!(hint.contains("mid-turn"));
        assert!(hint.contains("1 queued"));
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
        let config = minimal_test_config();
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
        assert_eq!(outcome, StreamingKeyOutcome::Changed);
        assert!(app.entries.iter().any(|entry| entry
            .text
            .contains("/model is not available while a turn is running")));

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
    fn activity_status_shows_phase_and_pending_followups() {
        let started = Instant::now();
        let waiting = activity_status(started, &Activity::Waiting, 0);
        assert!(waiting.contains("waiting"));
        assert!(waiting.contains("esc interrupt"));
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
        });
        session.messages.push(ConversationMessage {
            thinking: "saved summary".into(),
            role: MessageRole::Assistant,
            content: "first response".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "second prompt".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
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
        assert_eq!(input_area_height(&app, 20, 0), 3);
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
        assert!(is_shift_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        assert!(!is_shift_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        app.history_next();
        assert_eq!(app.input, "first\nsecond");
    }

    #[test]
    fn single_line_input_stays_vertically_centered() {
        let mut app = TuiApp::default();
        app.set_input("draft");

        let metrics = input_metrics(&app, 3);
        let lines = render_input_lines(&app, 3, Style::default(), Style::default());

        assert_eq!(metrics.top_padding, 1);
        assert_eq!(metrics.visible_index, 0);
        assert_eq!(line_text(&lines[0]), "");
        assert_eq!(line_text(&lines[1]), "pi> draft");
    }

    #[test]
    fn typed_newlines_expand_input_area_but_paste_newlines_do_not() {
        let mut app = TuiApp::default();
        app.set_input("one");
        for line in ["two", "three", "four"] {
            app.insert_input_newline();
            for ch in line.chars() {
                app.push_input_char(ch);
            }
        }

        assert_eq!(app.typed_input_rows(), 4);
        assert_eq!(input_area_height(&app, 20, 0), 5);
        let metrics = input_metrics(&app, 5);
        let lines = render_input_lines(&app, 5, Style::default(), Style::default());
        assert_eq!(metrics.top_padding, 1);
        assert_eq!(line_text(&lines[0]), "");
        assert_eq!(line_text(&lines[1]), "pi> one");
        assert_eq!(line_text(&lines[2]), "    two");
        assert_eq!(line_text(&lines[3]), "    three");
        assert_eq!(line_text(&lines[4]), "    four");

        let mut pasted = TuiApp::default();
        pasted.paste_text("one\ntwo\nthree\nfour");
        assert_eq!(pasted.typed_input_rows(), 1);
        assert_eq!(input_area_height(&pasted, 20, 0), 3);
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
        assert_eq!(app.typed_input_rows(), 1);
        assert_eq!(app.editor_state.history(), ["old prompt"]);
    }

    #[test]
    fn slash_command_matches_follow_current_input() {
        let config = LoadedConfig {
            paths: test_config_paths(),
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
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
        });
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Tool,
            content: "file contents".to_string(),
            media: Vec::new(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("read".to_string()),
            tool_calls: Vec::new(),
        });
        session.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Assistant,
            content: "done".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
        let runtime = Runtime::new(session, ReloadableSystems::default());

        insert_new_tool_messages(&mut app, &runtime, 1, assistant_index);

        assert_eq!(app.entries[1].kind, TuiEntryKind::Tool);
        assert_eq!(app.entries[1].text, "completed read\nfile contents");
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

        let filtered = strip_restart_model_args(args);

        assert_eq!(
            filtered,
            ["--continue", "--theme", "dark"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    fn test_config_paths() -> ConfigPaths {
        ConfigPaths {
            cwd: PathBuf::from("."),
            agent_dir: PathBuf::from(".pi/agent"),
            session_dir: PathBuf::from(".pi/agent/sessions"),
            settings_path: PathBuf::from(".pi/agent/settings.json"),
            project_settings_path: PathBuf::from(".pi/settings.json"),
            auth_path: PathBuf::from(".pi/agent/auth.json"),
            models_path: PathBuf::from(".pi/agent/models.json"),
            model_cache_path: PathBuf::from(".pi/agent/model-cache.json"),
            keybindings_path: PathBuf::from(".pi/agent/keybindings.json"),
        }
    }

    fn minimal_test_config() -> LoadedConfig {
        LoadedConfig {
            paths: test_config_paths(),
            settings: Settings::default(),
            auth: AuthData::default(),
            models: Vec::new(),
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
        app.set_input("draft");
        let now = Instant::now();

        assert!(!app.quit_requested(now));
        assert!(app.input.is_empty(), "first press clears the draft");
        assert!(app.quit_requested(now + Duration::from_secs(1)));
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
            "pi-persist-model-test-{}-{}",
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

    fn line_text(line: &Line<'_>) -> String {
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
            "pi-cli-model-cache-version-{}",
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
    fn todo_panel_height_collapses_and_expands() {
        let mut app = TuiApp::default();
        assert_eq!(todo_panel_height(&app), 0);
        app.todos = (0..8)
            .map(|index| TodoItem {
                content: format!("task {index}"),
                status: TodoStatus::Pending,
            })
            .collect();
        assert_eq!(todo_panel_height(&app), 6);
        app.todos_expanded = true;
        assert_eq!(todo_panel_height(&app), 8);
        app.todos = (0..3)
            .map(|index| TodoItem {
                content: format!("task {index}"),
                status: TodoStatus::Pending,
            })
            .collect();
        assert_eq!(todo_panel_height(&app), 3);
    }

    #[test]
    fn diff_panel_keys_navigate_and_open_detail() {
        let cwd =
            std::env::temp_dir().join(format!("pi-cli-diff-panel-test-{}", unique_temp_suffix()));
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
        let cwd =
            std::env::temp_dir().join(format!("pi-cli-diff-detail-test-{}", unique_temp_suffix()));
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
            provider: provider.to_string(),
            id: id.to_string(),
            name: None,
            api: ConfigProviderApi::default(),
            base_url: None,
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

        for command in fixture["commands"].as_array().expect("fixture commands") {
            let command = command.as_str().expect("command");
            assert!(
                rust_commands.contains(command),
                "missing upstream CLI command {command}"
            );
        }
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
        ["config", "install", "list", "remove", "uninstall", "update"]
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

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .expect("encode png");
        output.into_inner()
    }
}

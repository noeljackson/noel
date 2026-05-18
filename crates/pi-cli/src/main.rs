use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Cursor, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use base64::Engine;
use clap::{Parser, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use pi_ai::{
    create_provider, MediaInput, ModelRef, ProviderApi as AiProviderApi, ProviderAuth,
    ProviderConfig,
};
use pi_config::{
    auth_for_provider, has_auth_for_provider, load_config, read_model_cache, write_model_cache,
    AuthCredential, AuthData, CompactionSettings, ConfigPaths, ImageSettings, LoadedConfig,
    ModelCache, ModelDefinition, ModelRefreshSettings, PackageSource,
    ProviderApi as ConfigProviderApi, ResolvedAuth, ResourceFile, RetrySettings, Settings,
    TerminalSettings, WarningSettings, ENV_SESSION_DIR,
};
use pi_core::{
    run_excluded_bash, run_user_turn, run_user_turn_streaming, run_user_turn_streaming_with_media,
    write_session_export, CompactionKind, MessageRole, ReloadableSystems, Runtime, SessionState,
    SessionStore,
};
use pi_tui::{
    EditorState, Keybinding as TuiKeybinding, KeybindingMap, Selector, SelectorItem, SessionView,
    SettingsView, TerminalRenderer, TerminalTheme,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
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

    #[arg(long)]
    offline: bool,

    #[arg()]
    messages: Vec<String>,
}

fn try_run_package_command() -> Result<bool> {
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
            run_auth_login(&args[1..])?;
            Ok(true)
        }
        "logout" => {
            run_auth_logout(&args[1..])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn run_auth_login(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("login");
        return Ok(());
    }
    let mut provider = None;
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
    let provider =
        provider.ok_or_else(|| anyhow!("usage: pi login <provider> --api-key <key|env:VAR|->"))?;
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
            .ok_or_else(|| anyhow!("usage: pi login {provider} --api-key <key|env:VAR|->"))?,
    };
    if key.trim().is_empty() {
        return Err(anyhow!("api key is empty"));
    }
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
    let mut auth = read_auth_data(&paths.auth_path)?;
    auth.insert(provider.clone(), AuthCredential::ApiKey { key });
    write_auth_data(&paths.auth_path, &auth)?;
    println!(
        "stored API-key auth for {provider} in {}",
        paths.auth_path.display()
    );
    Ok(())
}

fn run_auth_logout(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("logout");
        return Ok(());
    }
    let [provider] = args else {
        return Err(anyhow!("usage: pi logout <provider>"));
    };
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd, None)?;
    let mut auth = read_auth_data(&paths.auth_path)?;
    if auth.remove(provider).is_some() {
        write_auth_data(&paths.auth_path, &auth)?;
        println!("removed stored auth for {provider}");
    } else {
        println!("no stored auth for {provider}");
    }
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
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
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
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(auth)?))?;
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
            "usage: pi login <provider> [--api-key <key|env:VAR|->]\n\nStore API-key auth in ~/.pi/agent/auth.json. Without --api-key, pi stores env:<provider default> when that environment variable is present."
        ),
        "logout" => println!("usage: pi logout <provider>\n\nRemove stored provider auth."),
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if try_run_package_command()? {
        return Ok(());
    }
    let cli = Cli::parse_from(normalized_cli_args(std::env::args()));
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd.clone(), cli.session_dir.clone())?;
    let mut config = load_config(paths)?;
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
    start_model_refresh(&config, offline, cli.verbose);

    if let Some(search) = &cli.list_models {
        let search = search.as_deref().map(str::to_lowercase);
        for model in &config.models {
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
    if cli.no_tools || cli.no_builtin_tools || !cli.tools.is_empty() {
        let active_tools = runtime.systems().available_tool_names.clone();
        runtime.set_active_tools(active_tools)?;
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
                let next_generation = runtime.systems().config_generation + 1;
                match runtime.reload(ReloadableSystems::from_config(&config, next_generation)) {
                    Ok(report) => Ok(serde_json::json!({
                        "activeModelValid": report.active_model_valid,
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
    if let Some(theme) = &cli.theme {
        config.settings.theme = Some(theme.clone());
    }
    if cli.no_themes {
        config.settings.theme = None;
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
            provider,
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
            "-v" => "--version".to_string(),
            _ => arg,
        })
        .collect()
}

fn offline_enabled(cli_offline: bool) -> bool {
    cli_offline
        || std::env::var("PI_OFFLINE")
            .ok()
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
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

    if let Some(auth) = auth_for_provider(&auth, "anthropic") {
        match fetch_anthropic_models(auth).await {
            Ok(models) => {
                refreshed_providers.insert("anthropic".to_string());
                refreshed_models.extend(models);
            }
            Err(error) => diagnostics.push(format!("anthropic model refresh failed: {error}")),
        }
    }

    if let Some(ResolvedAuth::ApiKey(api_key)) = auth_for_provider(&auth, "openai") {
        match fetch_openai_api_models("openai", ConfigProviderApi::OpenAiResponses, None, &api_key)
            .await
        {
            Ok(models) => {
                refreshed_providers.insert("openai".to_string());
                refreshed_models.extend(models);
            }
            Err(error) => diagnostics.push(format!("openai model refresh failed: {error}")),
        }
    }

    if let Some(auth) = auth_for_provider(&auth, "openai-codex") {
        match fetch_codex_models(auth).await {
            Ok(models) => {
                refreshed_providers.insert("openai-codex".to_string());
                refreshed_models.extend(models);
            }
            Err(error) => diagnostics.push(format!("openai-codex model refresh failed: {error}")),
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
            models,
            diagnostics: Vec::new(),
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
}

#[derive(Debug, Deserialize)]
struct AnthropicModel {
    id: String,
    display_name: String,
}

async fn fetch_anthropic_models(auth: ResolvedAuth) -> Result<Vec<ModelDefinition>> {
    let response = reqwest::Client::new()
        .get("https://api.anthropic.com/v1/models")
        .headers(anthropic_model_headers(&auth)?)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("status {status}: {body}"));
    }
    let response = response.json::<AnthropicModelsResponse>().await?;
    Ok(response
        .data
        .into_iter()
        .map(|model| ModelDefinition {
            provider: "anthropic".to_string(),
            id: model.id,
            name: Some(model.display_name),
            api: ConfigProviderApi::Anthropic,
            base_url: None,
        })
        .collect())
}

fn anthropic_model_headers(auth: &ResolvedAuth) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    match auth {
        ResolvedAuth::ApiKey(api_key) => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
        }
        ResolvedAuth::ClaudeCodeOAuth { access_token } => {
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

async fn fetch_openai_api_models(
    provider: &str,
    api: ConfigProviderApi,
    base_url: Option<String>,
    api_key: &str,
) -> Result<Vec<ModelDefinition>> {
    let response = reqwest::Client::new()
        .get("https://api.openai.com/v1/models")
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
                &api_key,
            )
            .await
        }
        ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
        } => fetch_chatgpt_codex_models(&access_token, account_id.as_deref()).await,
        ResolvedAuth::ClaudeCodeOAuth { .. } => Err(anyhow!("unsupported Codex auth type")),
    }
}

async fn fetch_chatgpt_codex_models(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<ModelDefinition>> {
    let client = reqwest::Client::new();
    let mut last_error = None;
    for endpoint in [
        "https://chatgpt.com/backend-api/codex/models",
        "https://chatgpt.com/backend-api/models",
    ] {
        let response = client
            .get(endpoint)
            .headers(chatgpt_model_headers(access_token, account_id)?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            last_error = Some(anyhow!("status {status} from {endpoint}: {body}"));
            continue;
        }
        let value = response.json::<serde_json::Value>().await?;
        let ids = collect_codex_model_ids(&value);
        if ids.is_empty() {
            last_error = Some(anyhow!("no Codex model IDs in response from {endpoint}"));
            continue;
        }
        return Ok(ids
            .into_iter()
            .map(|id| ModelDefinition {
                provider: "openai-codex".to_string(),
                name: Some(model_display_name(&id)),
                id,
                api: ConfigProviderApi::OpenAiCodexResponses,
                base_url: Some("https://chatgpt.com/backend-api".to_string()),
            })
            .collect());
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no Codex model refresh endpoint succeeded")))
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

fn collect_codex_model_ids(value: &serde_json::Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    collect_codex_model_ids_from_value(value, &mut ids);
    ids
}

fn collect_codex_model_ids_from_value(value: &serde_json::Value, ids: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_codex_model_ids_from_value(item, ids);
            }
        }
        serde_json::Value::Object(object) => {
            for key in ["id", "slug", "model", "name"] {
                if let Some(id) = object
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| model_supported_for_provider("openai-codex", id))
                {
                    ids.insert(id.to_string());
                }
            }
            for nested in object.values() {
                collect_codex_model_ids_from_value(nested, ids);
            }
        }
        _ => {}
    }
}

fn model_supported_for_provider(provider: &str, id: &str) -> bool {
    match provider {
        "openai" => {
            (id.starts_with("gpt-")
                || id.starts_with("o1")
                || id.starts_with("o3")
                || id.starts_with("o4"))
                && !id.contains("audio")
                && !id.contains("realtime")
                && !id.contains("transcribe")
                && !id.contains("tts")
                && !id.contains("image")
                && !id.contains("moderation")
        }
        "openai-codex" => id.starts_with("gpt-5.") || id.contains("codex"),
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
                    .find(|model| has_auth_for_provider(&config.auth, &model.provider))
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
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let _restore = TerminalRestore;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    loop {
        app.refresh_chrome(&config, &runtime);
        terminal.draw(|frame| draw_tui(frame, &app))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if handle_tui_key(
            key,
            &mut terminal,
            &mut app,
            &mut runtime,
            &mut config,
            offline,
        )
        .await?
        {
            break;
        }
    }
    drop(terminal);
    drop(_restore);
    if std::env::var("PI_TUI_E2E_DUMP").ok().as_deref() == Some("1") {
        println!("{}", app.transcript_text());
    }
    Ok(())
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiEntryKind {
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
            if model.id.contains("opus") && supports_anthropic_adaptive_thinking(&model.id) {
                &["off", "high", "xhigh", "max"]
            } else if supports_anthropic_adaptive_thinking(&model.id) {
                &["off", "low", "medium", "high", "xhigh"]
            } else if model.id.contains("claude-") {
                &["off", "minimal", "low", "medium", "high"]
            } else {
                &[]
            }
        }
        "openai" | "openai-codex" | "azure-openai-responses" => {
            if model.id.starts_with("gpt-5") || model.id.contains("codex") {
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

fn supports_anthropic_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
}

#[derive(Debug, Default)]
struct TuiApp {
    entries: Vec<TuiEntry>,
    input: String,
    editor_state: EditorState,
    last_shell_command: Option<String>,
    multiline: Option<Vec<String>>,
    selector: Option<TuiSelectorState>,
    header_title: String,
    header_line: String,
    status: String,
    show_hardware_cursor: bool,
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
                .unwrap_or_else(|| "default".to_string()),
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
        self.show_hardware_cursor = config.settings.show_hardware_cursor.unwrap_or(true);
    }

    fn push(&mut self, kind: TuiEntryKind, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            self.entries.push(TuiEntry { kind, text });
        }
    }

    fn push_placeholder(&mut self, kind: TuiEntryKind, text: impl Into<String>) -> usize {
        self.entries.push(TuiEntry {
            kind,
            text: text.into(),
        });
        self.entries.len() - 1
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

    fn transcript_text(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                let label = match entry.kind {
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

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn draw_tui(frame: &mut Frame<'_>, app: &TuiApp) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_header(frame, root[0], app);
    draw_chat(frame, root[1], app);
    draw_input(frame, root[2], app);
    draw_footer(frame, root[3], app);
    draw_selector_overlay(frame, app);
    set_tui_cursor(frame, root[2], app);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let paragraph = Paragraph::new(app.header_line.as_str())
        .style(Style::default().fg(Color::Gray))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.header_title.as_str()),
        );
    frame.render_widget(paragraph, area);
}

fn draw_chat(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut lines = Vec::new();
    for entry in &app.entries {
        let (label, style) = match entry.kind {
            TuiEntryKind::System => ("system", Style::default().fg(Color::DarkGray)),
            TuiEntryKind::User => (
                "you",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TuiEntryKind::Assistant => (
                "assistant",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            TuiEntryKind::Tool => ("tool", Style::default().fg(Color::Green)),
            TuiEntryKind::Error => ("error", Style::default().fg(Color::Red)),
        };
        lines.push(Line::from(vec![Span::styled(format!("{label} "), style)]));
        for line in entry.text.lines() {
            lines.push(Line::from(Span::raw(format!("  {line}"))));
        }
        lines.push(Line::from(""));
    }
    let available = area.height.saturating_sub(2) as usize;
    let start = lines.len().saturating_sub(available);
    let paragraph = Paragraph::new(lines[start..].to_vec())
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" conversation "),
        );
    frame.render_widget(paragraph, area);
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = if app.multiline.is_some() {
        " multiline: enter . to submit "
    } else {
        " pi> "
    };
    let paragraph = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let footer = Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

fn draw_selector_overlay(frame: &mut Frame<'_>, app: &TuiApp) {
    let Some(selector) = &app.selector else {
        return;
    };
    let area = centered_rect(frame.area(), 82, 68);
    frame.render_widget(Clear, area);
    let available = area.height.saturating_sub(5) as usize;
    let start = selector
        .selected
        .saturating_sub(available.saturating_sub(1));
    let mut lines = vec![
        Line::from(vec![
            Span::styled("filter ", Style::default().fg(Color::DarkGray)),
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
            Style::default().add_modifier(Modifier::REVERSED)
        } else if item.active {
            Style::default().fg(Color::Cyan)
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
        lines.push(Line::from(Span::styled(
            "no matches",
            Style::default().fg(Color::DarkGray),
        )));
    }
    if let Some(level) = selector.selected_thinking_level() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("thinking ", Style::default().fg(Color::DarkGray)),
            Span::styled(level, Style::default().fg(Color::Cyan)),
            Span::styled(
                "  left/right to adjust",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    let action = if selector.kind == "settings" {
        "enter toggle"
    } else {
        "enter select"
    };
    let title = format!(" {} selector  {action}  esc cancel ", selector.title);
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(title));
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
    let max_x = input_area.right().saturating_sub(2);
    let x = input_area
        .x
        .saturating_add(1)
        .saturating_add(app.input.chars().count() as u16)
        .min(max_x);
    let y = input_area
        .y
        .saturating_add(1)
        .min(input_area.bottom().saturating_sub(2));
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
    terminal: &mut TuiTerminal,
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
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Esc => {
            app.input.clear();
            app.multiline = None;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Enter => {
            let line = app.input.trim().to_string();
            app.input.clear();
            if !line.is_empty() {
                let quit = match handle_tui_submission(
                    app, terminal, runtime, config, offline, line,
                )
                .await
                {
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
        }
        KeyCode::Char(ch) => {
            app.input.push(ch);
        }
        _ => {}
    }
    app.refresh_chrome(config, runtime);
    Ok(false)
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
            app.push(
                TuiEntryKind::System,
                format_model_selection(&model, thinking.as_deref()),
            );
        }
        "theme" | "themes" => {
            config.settings.theme = Some(item.value.clone());
            app.push(TuiEntryKind::System, format!("theme: {}", item.value));
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
        "logout" => {
            if config.auth.remove(&item.value).is_some() {
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

async fn handle_tui_submission(
    app: &mut TuiApp,
    terminal: &mut TuiTerminal,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    offline: bool,
    line: String,
) -> Result<bool> {
    if let Some(lines) = app.multiline.as_mut() {
        if line == "." {
            let prompt = lines.join("\n");
            app.multiline = None;
            submit_tui_prompt(app, terminal, runtime, config, prompt, Vec::new(), offline).await;
        } else {
            lines.push(line);
        }
        return Ok(false);
    }
    if line == "/quit" {
        return Ok(true);
    }
    if handle_tui_bang(app, terminal, runtime, config, &line).await? {
        return Ok(false);
    }
    match line.as_str() {
        "/help" => app.push(TuiEntryKind::System, terminal_renderer(config).help()),
        "/models" => app.push(
            TuiEntryKind::System,
            config
                .models
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "/thinking" => app.push(
            TuiEntryKind::System,
            format!(
                "thinking: {}",
                active_thinking_label(runtime, config).unwrap_or_else(|| "-".to_string())
            ),
        ),
        "/model" | "/scoped-models" => open_tui_selector(app, config, runtime, "model", "")?,
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
        "/themes" => app.push(
            TuiEntryKind::System,
            format_resources("themes", &config.themes),
        ),
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
            *config = load_config(config.paths.clone())?;
            let next_generation = runtime.systems().config_generation + 1;
            let report = runtime.reload(ReloadableSystems::from_config(config, next_generation))?;
            let mut output = format_diagnostics(config);
            if !report.active_model_valid {
                output
                    .push_str("\nactive model is no longer available; use /model <provider/model>");
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
        _ if line.starts_with("/complete ") => {
            let prefix = line.trim_start_matches("/complete ").trim();
            let completions = EditorState::command_completions(prefix);
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
                    submit_tui_prompt(app, terminal, runtime, config, prompt, Vec::new(), offline)
                        .await;
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
                        submit_tui_prompt(
                            app,
                            terminal,
                            runtime,
                            config,
                            prompt.to_string(),
                            vec![media],
                            offline,
                        )
                        .await;
                    }
                }
                Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
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
            submit_tui_prompt(app, terminal, runtime, config, prompt, Vec::new(), offline).await;
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
            submit_tui_prompt(app, terminal, runtime, config, prompt, Vec::new(), offline).await;
        }
        _ if line.starts_with("/prompt ") => {
            let rest = line.trim_start_matches("/prompt ").trim();
            let (name, input) = split_once_text(rest);
            let template = find_resource(&config.prompt_templates, name)
                .ok_or_else(|| anyhow!("prompt template not found: {name}"))?;
            let prompt = expand_prompt_template(&template.content, input);
            submit_tui_prompt(app, terminal, runtime, config, prompt, Vec::new(), offline).await;
        }
        _ if line.starts_with("/theme ") => {
            let name = line.trim_start_matches("/theme ").trim();
            let theme = find_resource(&config.themes, name)
                .ok_or_else(|| anyhow!("theme not found: {name}"))?;
            config.settings.theme = Some(theme.name.clone());
            app.push(TuiEntryKind::System, format!("theme: {}", theme.name));
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
        _ if line.starts_with("/login") => {
            let provider = line.trim_start_matches("/login").trim();
            if provider.is_empty() {
                open_tui_selector(app, config, runtime, "login", "")?;
            } else {
                app.push(TuiEntryKind::System, format_login_status(config, provider));
            }
        }
        _ if line.starts_with("/logout") => {
            let provider = line.trim_start_matches("/logout").trim();
            if provider.is_empty() {
                open_tui_selector(app, config, runtime, "logout", "")?;
            } else if config.auth.remove(provider).is_some() {
                write_auth_file(config)?;
                app.push(
                    TuiEntryKind::System,
                    format!("removed stored auth for {provider}"),
                );
            } else {
                app.push(
                    TuiEntryKind::System,
                    format!("no stored auth for {provider}"),
                );
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
        _ => submit_tui_prompt(app, terminal, runtime, config, line, Vec::new(), offline).await,
    }
    Ok(false)
}

async fn handle_tui_bang(
    app: &mut TuiApp,
    terminal: &mut TuiTerminal,
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
        terminal.draw(|frame| draw_tui(frame, app))?;
        Some(index)
    } else {
        None
    };
    match run_excluded_bash(runtime, command.clone()).await {
        Ok(output) => {
            if let Some(index) = entry_index {
                app.replace_entry(index, format_bash_completed(&command, &output));
            } else {
                app.push(TuiEntryKind::Tool, output);
            }
            app.last_shell_command = Some(command);
        }
        Err(error) => {
            if let Some(index) = entry_index {
                app.replace_entry(index, format!("failed bash\n{error}"));
            } else {
                app.push(TuiEntryKind::Error, format!("{error}"));
            }
        }
    }
    Ok(true)
}

async fn submit_tui_prompt(
    app: &mut TuiApp,
    terminal: &mut TuiTerminal,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) {
    app.editor_state.record_history(prompt.clone());
    app.push(TuiEntryKind::User, prompt.clone());
    if let Err(error) =
        run_prompt_with_queue_tui(app, terminal, runtime, config, prompt, media, offline).await
    {
        app.push(
            TuiEntryKind::Error,
            format_tui_error(&error, runtime, config),
        );
    }
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
    terminal: &mut TuiTerminal,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<()> {
    maybe_auto_compact(runtime, config, false)?;
    run_prompt_once_tui(app, terminal, runtime, config, prompt, media, offline).await?;
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
        run_prompt_once_tui(app, terminal, runtime, config, prompt, Vec::new(), offline).await?;
        if follow_up_mode(config) == "one-at-a-time" {
            break;
        }
    }
    Ok(())
}

fn follow_up_mode(config: &LoadedConfig) -> &str {
    config
        .settings
        .follow_up_mode
        .as_deref()
        .unwrap_or("one-at-a-time")
}

async fn run_prompt_once_tui(
    app: &mut TuiApp,
    terminal: &mut TuiTerminal,
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<()> {
    let kind = response_kind_for_prompt(&prompt);
    let progress_enabled = terminal_progress_enabled(config);
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
    terminal.draw(|frame| draw_tui(frame, app))?;
    let provider = provider_for_runtime(runtime, config, offline)?;
    if kind == TuiEntryKind::Tool {
        let response =
            run_user_turn_streaming_with_media(runtime, provider.as_ref(), prompt, media, |_| {})
                .await?;
        if progress_enabled {
            app.replace_entry(entry_index, format_tool_completed(&response));
        } else {
            app.replace_entry(entry_index, response);
        }
        terminal.draw(|frame| draw_tui(frame, app))?;
        return Ok(());
    }

    let mut saw_delta = false;
    let response =
        run_user_turn_streaming_with_media(runtime, provider.as_ref(), prompt, media, |delta| {
            if !saw_delta {
                app.replace_entry(entry_index, "");
                saw_delta = true;
            }
            app.append_entry(entry_index, delta);
            let _ = terminal.draw(|frame| draw_tui(frame, app));
        })
        .await?;
    if !saw_delta {
        app.replace_entry(entry_index, response);
    }
    terminal.draw(|frame| draw_tui(frame, app))?;
    Ok(())
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

fn format_tool_completed(output: &str) -> String {
    format!("completed\n{output}")
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
    let provider = provider_for_runtime(runtime, config, offline)?;
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
        run_user_turn_streaming_with_media(runtime, provider.as_ref(), prompt, media, |delta| {
            printed = true;
            print!("{delta}");
            let _ = io::stdout().flush();
        })
        .await?
    };
    if printed {
        println!();
    } else if !response.is_empty() {
        println!("{response}");
    }
    Ok(response)
}

fn provider_for_runtime(
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
    Ok(create_provider(ProviderConfig {
        thinking_budget_tokens: thinking_budget_tokens(config, thinking_level.as_deref()),
        thinking_level,
        model,
        api: map_provider_api(&definition.api),
        base_url: definition.base_url.clone(),
        auth: map_provider_auth(auth_for_provider(&config.auth, &definition.provider)),
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
        Some(level) => format!("model: {}/{}\nthinking: {level}", model.provider, model.id),
        None => format!("model: {}/{}", model.provider, model.id),
    }
}

fn map_provider_auth(auth: Option<ResolvedAuth>) -> ProviderAuth {
    match auth {
        Some(ResolvedAuth::ApiKey(api_key)) => ProviderAuth::ApiKey(api_key),
        Some(ResolvedAuth::ClaudeCodeOAuth { access_token }) => {
            ProviderAuth::ClaudeCodeOAuth { access_token }
        }
        Some(ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
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
    TerminalRenderer::new(TerminalTheme {
        name: config
            .settings
            .theme
            .clone()
            .unwrap_or_else(|| "default".to_string()),
    })
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
        theme: config.settings.theme.clone(),
    })
}

fn format_hotkeys(config: &LoadedConfig) -> String {
    terminal_renderer(config).keybindings(&keybinding_map(config))
}

fn format_status(config: &LoadedConfig, runtime: &Runtime, editor_state: &EditorState) -> String {
    format!(
        "status\tmodel:{}\tthinking:{}\ttheme:{}\tqueue:{}\thistory:{}\tdiagnostics:{}",
        runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .unwrap_or_else(|| "-".to_string()),
        active_thinking_label(runtime, config).unwrap_or_else(|| "-".to_string()),
        config
            .settings
            .theme
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        runtime.session().queued_messages.len(),
        editor_state.history().len(),
        config.diagnostics.len()
    )
}

fn footer_status(config: &LoadedConfig, runtime: &Runtime, editor_state: &EditorState) -> String {
    format_status(config, runtime, editor_state)
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
            Ok(format_model_selection(&model, thinking.as_deref()))
        }
        "theme" | "themes" => {
            config.settings.theme = Some(item.value.clone());
            Ok(format!("theme: {}", item.value))
        }
        "session" | "sessions" | "resume" | "tree" => {
            let path = resolve_session_reference(&config.paths.session_dir, &item.value)?;
            let (store, state) = SessionStore::open(path)?;
            runtime.replace_session(state, Some(store));
            Ok(format_session(runtime))
        }
        "auth" | "login" => Ok(format_login_status(config, &item.value)),
        "logout" => {
            if config.auth.remove(&item.value).is_some() {
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
            config
                .models
                .iter()
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
            config
                .themes
                .iter()
                .map(|theme| SelectorItem {
                    label: theme.name.clone(),
                    value: theme.name.clone(),
                    active: config.settings.theme.as_deref() == Some(theme.name.as_str()),
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
                    label: if auth_for_provider(&config.auth, &provider).is_some() {
                        format!("{provider}: available")
                    } else {
                        format!("{provider}: missing")
                    },
                    value: provider.clone(),
                    active: auth_for_provider(&config.auth, &provider).is_some(),
                })
                .collect(),
        )),
        "settings" => Ok(Selector::new("settings", settings_selector_items(config))),
        _ => Err(anyhow!("unknown selector: {kind}")),
    }
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
struct ExtensionCommandRequest<'a> {
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
    let mut child = Command::new(&extension.path)
        .current_dir(cwd)
        .env("PI_EXTENSION_NAME", &extension.name)
        .env("PI_EXTENSION_PATH", &extension.path)
        .env(
            "PI_EXTENSION_PROTOCOL",
            protocol.as_deref().unwrap_or("stdio"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        match protocol.as_deref() {
            Some("json") => {
                let cwd_string = cwd.display().to_string();
                let request = ExtensionCommandRequest {
                    protocol_version: 1,
                    kind: "command",
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
    let mut text = match protocol.as_deref() {
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
            let status = if auth_for_provider(&config.auth, &provider).is_some() {
                "available"
            } else {
                "missing"
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
    fs::write(
        &config.paths.auth_path,
        serde_json::to_string_pretty(&config.auth)?,
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

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .expect("encode png");
        output.into_inner()
    }
}

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};
use pi_ai::{
    create_provider, ModelRef, ProviderApi as AiProviderApi, ProviderAuth, ProviderConfig,
};
use pi_config::{
    auth_for_provider, has_auth_for_provider, load_config, AuthCredential, ConfigPaths,
    LoadedConfig, ProviderApi as ConfigProviderApi, ResolvedAuth, ResourceFile, ENV_SESSION_DIR,
};
use pi_core::{
    run_excluded_bash, run_user_turn, run_user_turn_streaming, write_session_export,
    CompactionKind, MessageRole, ReloadableSystems, Runtime, SessionState, SessionStore,
};
use pi_tui::{
    Keybinding as TuiKeybinding, KeybindingMap, ModelView, SessionView, SettingsView,
    TerminalRenderer, TerminalTheme,
};

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

    #[arg(long)]
    list_models: bool,

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

    #[arg(long)]
    export: Option<PathBuf>,

    #[arg(long)]
    verbose: bool,

    #[arg(long)]
    offline: bool,

    #[arg()]
    messages: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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

    if cli.list_models {
        for model in &config.models {
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
        return run_rpc(runtime, config, cli.offline).await;
    }

    let mut initial_prompt = expand_message_inputs(&cwd, &cli.messages)?;
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
        let response = run_prompt(&mut runtime, &config, initial_prompt, cli.offline).await?;
        if let Some(path) = &cli.export {
            export_session(&runtime, path)?;
        }
        print_response(&cli.mode, &response);
        return Ok(());
    }

    run_interactive(runtime, config, cli.offline).await
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
    let renderer = terminal_renderer(&config);
    println!("{}", renderer.banner());
    println!("type /help for commands, /reload to reload config, /quit to exit");
    let stdin = io::stdin();
    let mut last_shell_command: Option<String> = None;
    loop {
        print!("{}", renderer.prompt());
        io::stdout().flush()?;
        let mut line = String::new();
        let read = stdin.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if handle_bang_command(&mut runtime, &line, &mut last_shell_command).await? {
            continue;
        }
        match line.as_str() {
            "/quit" => break,
            "/help" => {
                println!("{}", renderer.help());
                continue;
            }
            "/models" => {
                for model in &config.models {
                    println!("{}/{}", model.provider, model.id);
                }
                continue;
            }
            "/model" => {
                print_scoped_models(&config, &runtime);
                continue;
            }
            "/session" => {
                print_session(&runtime);
                continue;
            }
            "/reload" => {
                config = load_config(config.paths.clone())?;
                let next_generation = runtime.systems().config_generation + 1;
                let report =
                    runtime.reload(ReloadableSystems::from_config(&config, next_generation))?;
                print_diagnostics(&config);
                if !report.active_model_valid {
                    println!("active model is no longer available; use /model <provider/model>");
                }
                if !report.removed_active_tools.is_empty() {
                    println!(
                        "removed active tools: {}",
                        report.removed_active_tools.join(", ")
                    );
                }
                println!("reloaded");
                continue;
            }
            "/settings" => {
                print_settings(&config, &runtime);
                continue;
            }
            "/diagnostics" => {
                print_diagnostics(&config);
                continue;
            }
            "/hotkeys" => {
                print_hotkeys(&config);
                continue;
            }
            "/skills" => {
                print_resources("skills", &config.skills);
                continue;
            }
            "/prompts" => {
                print_resources("prompts", &config.prompt_templates);
                continue;
            }
            "/themes" => {
                print_resources("themes", &config.themes);
                continue;
            }
            "/queue" => {
                print_queue(&runtime);
                continue;
            }
            _ if line.starts_with("/queue ") => {
                let message = line.trim_start_matches("/queue ").trim().to_string();
                runtime.queue_message(message)?;
                println!("queued: {}", runtime.session().queued_messages.len());
                continue;
            }
            "/queue-clear" => {
                let cleared = runtime.clear_queued_messages()?;
                println!("cleared {cleared} queued message(s)");
                continue;
            }
            "/interrupt" => {
                let cleared = runtime.clear_queued_messages()?;
                println!("interrupted; cleared {cleared} queued message(s)");
                continue;
            }
            "/scoped-models" => {
                print_scoped_models(&config, &runtime);
                continue;
            }
            _ if line.starts_with("/model ") => {
                let reference = line.trim_start_matches("/model ").trim();
                let model = resolve_model_reference(&config, reference)
                    .ok_or_else(|| anyhow!("model not found: {reference}"))?;
                runtime.set_active_model(Some(model.clone()))?;
                println!("model: {}/{}", model.provider, model.id);
                continue;
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
                run_prompt_and_print(&mut runtime, &config, prompt, offline).await;
                continue;
            }
            _ if line.starts_with("/prompt ") => {
                let rest = line.trim_start_matches("/prompt ").trim();
                let (name, input) = split_once_text(rest);
                let template = find_resource(&config.prompt_templates, name)
                    .ok_or_else(|| anyhow!("prompt template not found: {name}"))?;
                let prompt = expand_prompt_template(&template.content, input);
                run_prompt_and_print(&mut runtime, &config, prompt, offline).await;
                continue;
            }
            _ if line.starts_with("/theme ") => {
                let name = line.trim_start_matches("/theme ").trim();
                let theme = find_resource(&config.themes, name)
                    .ok_or_else(|| anyhow!("theme not found: {name}"))?;
                config.settings.theme = Some(theme.name.clone());
                println!("theme: {}", theme.name);
                continue;
            }
            "/multiline" => {
                let prompt = read_multiline_prompt(&stdin)?;
                run_prompt_and_print(&mut runtime, &config, prompt, offline).await;
                continue;
            }
            _ if line.starts_with("/new") => {
                let (store, state) =
                    SessionStore::create(&config.paths.session_dir, runtime.session().cwd.clone())?;
                runtime.replace_session(state, Some(store));
                print_session(&runtime);
                continue;
            }
            _ if line.starts_with("/resume") => {
                let reference = line.trim_start_matches("/resume").trim();
                if reference.is_empty() {
                    print_sessions(&config.paths.session_dir)?;
                } else {
                    let path = resolve_session_reference(&config.paths.session_dir, reference)?;
                    let (store, state) = SessionStore::open(path)?;
                    runtime.replace_session(state, Some(store));
                    print_session(&runtime);
                }
                continue;
            }
            _ if line.starts_with("/fork") => {
                let source =
                    resolve_source_session(&config.paths.session_dir, &runtime, &line, "/fork")?;
                let (store, state) = SessionStore::fork(&config.paths.session_dir, &source, false)?;
                runtime.replace_session(state, Some(store));
                print_session(&runtime);
                continue;
            }
            _ if line.starts_with("/clone") => {
                let source =
                    resolve_source_session(&config.paths.session_dir, &runtime, &line, "/clone")?;
                let (store, state) = SessionStore::fork(&config.paths.session_dir, &source, true)?;
                runtime.replace_session(state, Some(store));
                print_session(&runtime);
                continue;
            }
            "/tree" => {
                print_session_tree(&config.paths.session_dir)?;
                continue;
            }
            "/summaries" => {
                print_summaries(&runtime);
                continue;
            }
            _ if line.starts_with("/delete") => {
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
                println!("deleted {}", target.display());
                if deleting_current {
                    let (store, state) = SessionStore::create(
                        &config.paths.session_dir,
                        runtime.session().cwd.clone(),
                    )?;
                    runtime.replace_session(state, Some(store));
                    print_session(&runtime);
                }
                continue;
            }
            _ if line.starts_with("/name") => {
                let name = line.trim_start_matches("/name").trim();
                runtime.rename_session((!name.is_empty()).then(|| name.to_string()))?;
                print_session(&runtime);
                continue;
            }
            _ if line.starts_with("/labels") => {
                let labels = line
                    .trim_start_matches("/labels")
                    .split_whitespace()
                    .map(ToString::to_string)
                    .collect();
                runtime.set_labels(labels)?;
                print_session(&runtime);
                continue;
            }
            _ if line.starts_with("/export ") => {
                let path = PathBuf::from(line.trim_start_matches("/export ").trim());
                export_session(&runtime, &path)?;
                println!("exported {}", path.display());
                continue;
            }
            _ if line.starts_with("/import ") => {
                let path = PathBuf::from(line.trim_start_matches("/import ").trim());
                let (store, state) = SessionStore::import_path(&config.paths.session_dir, &path)?;
                runtime.replace_session(state, Some(store));
                print_session(&runtime);
                continue;
            }
            "/copy" => {
                if let Some(message) = runtime
                    .session()
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == MessageRole::Assistant)
                {
                    println!("{}", message.content);
                    if let Some(command) = copy_to_clipboard(&message.content)? {
                        println!("copied to clipboard via {command}");
                    } else {
                        println!("clipboard unavailable");
                    }
                } else {
                    println!("no assistant message");
                }
                continue;
            }
            "/compact" => {
                let record = runtime.compact_messages(CompactionKind::Manual)?;
                println!(
                    "compacted: omitted {} message(s), retained {} message(s)",
                    record.omitted_messages, record.retained_messages
                );
                continue;
            }
            _ if line.starts_with("/login") => {
                let provider = line.trim_start_matches("/login").trim();
                print_login_status(&config, provider);
                continue;
            }
            _ if line.starts_with("/logout") => {
                let provider = line.trim_start_matches("/logout").trim();
                if provider.is_empty() {
                    println!("usage: /logout <provider>");
                } else if config.auth.remove(provider).is_some() {
                    write_auth_file(&config)?;
                    println!("removed stored auth for {provider}");
                } else {
                    println!("no stored auth for {provider}");
                }
                continue;
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
                export_session(&runtime, &path)?;
                println!("share exported {}", path.display());
                continue;
            }
            _ => {}
        }

        run_prompt_and_print(&mut runtime, &config, line, offline).await;
    }
    Ok(())
}

async fn handle_bang_command(
    runtime: &mut Runtime,
    line: &str,
    last_shell_command: &mut Option<String>,
) -> Result<bool> {
    if line == "!!" && last_shell_command.is_none() {
        println!("no previous shell command");
        return Ok(true);
    }
    if line == "!" {
        println!("usage: ! <command>");
        return Ok(true);
    }
    let Some(command) = resolve_bang_command(line, last_shell_command) else {
        return Ok(false);
    };
    match run_excluded_bash(runtime, command.clone()).await {
        Ok(output) => {
            print!("{output}");
            if !output.ends_with('\n') {
                println!();
            }
            *last_shell_command = Some(command);
        }
        Err(error) => eprintln!("error: {error}"),
    }
    Ok(true)
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
    run_prompt_once(runtime, config, prompt, offline, false).await
}

async fn run_prompt_and_print(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    offline: bool,
) {
    if let Err(error) = run_prompt_with_queue(runtime, config, prompt, offline, true).await {
        eprintln!("error: {error}");
    }
}

async fn run_prompt_with_queue(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    offline: bool,
    stream_output: bool,
) -> Result<String> {
    maybe_auto_compact(runtime, stream_output)?;
    let first_response = run_prompt_once(runtime, config, prompt, offline, stream_output).await?;
    while let Some(prompt) = runtime.session().queued_messages.first().cloned() {
        let remaining = runtime
            .session()
            .queued_messages
            .iter()
            .skip(1)
            .cloned()
            .collect();
        runtime.replace_queued_messages(remaining)?;
        if stream_output {
            println!("queued> {prompt}");
        }
        run_prompt_once(runtime, config, prompt, offline, stream_output).await?;
    }
    Ok(first_response)
}

fn maybe_auto_compact(runtime: &mut Runtime, stream_output: bool) -> Result<()> {
    const AUTO_COMPACT_MESSAGE_LIMIT: usize = 24;
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

async fn run_prompt_once(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    offline: bool,
    stream_output: bool,
) -> Result<String> {
    let provider = provider_for_runtime(runtime, config, offline)?;
    if !stream_output {
        return run_user_turn(runtime, provider.as_ref(), prompt)
            .await
            .map_err(Into::into);
    }
    let mut printed = false;
    let response = run_user_turn_streaming(runtime, provider.as_ref(), prompt, |delta| {
        printed = true;
        print!("{delta}");
        let _ = io::stdout().flush();
    })
    .await?;
    if printed {
        println!();
    } else if !response.is_empty() {
        println!("{response}");
    }
    Ok(response)
}

fn read_multiline_prompt(stdin: &io::Stdin) -> Result<String> {
    println!("enter multiline prompt; finish with a single .");
    let mut lines = Vec::new();
    loop {
        print!("...> ");
        io::stdout().flush()?;
        let mut line = String::new();
        let read = stdin.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line == "." {
            break;
        }
        lines.push(line.to_string());
    }
    Ok(lines.join("\n"))
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
    Ok(create_provider(ProviderConfig {
        model,
        api: map_provider_api(&definition.api),
        base_url: definition.base_url.clone(),
        auth: map_provider_auth(auth_for_provider(&config.auth, &definition.provider)),
    }))
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
        ConfigProviderApi::Anthropic => AiProviderApi::Anthropic,
        ConfigProviderApi::Google => AiProviderApi::Google,
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

fn print_session(runtime: &Runtime) {
    println!(
        "{}",
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
    );
}

fn print_sessions(session_dir: &Path) -> Result<()> {
    for (index, session) in SessionStore::list(session_dir)?.into_iter().enumerate() {
        let name = session.name.unwrap_or_else(|| "-".to_string());
        println!(
            "{}.\t{}\t{}\t{}",
            index + 1,
            session.session_id,
            name,
            session.cwd.display()
        );
    }
    Ok(())
}

fn print_session_tree(session_dir: &Path) -> Result<()> {
    for session in SessionStore::list(session_dir)? {
        let parent = session.parent_session_id.unwrap_or_else(|| "-".to_string());
        let summary = session.branch_summary.unwrap_or_else(|| "-".to_string());
        println!(
            "{}\tparent:{parent}\tsummary:{summary}\t{}",
            session.session_id,
            session.cwd.display()
        );
    }
    Ok(())
}

fn print_summaries(runtime: &Runtime) {
    if runtime.session().compactions.is_empty() && runtime.session().branch_summaries.is_empty() {
        println!("no summaries");
        return;
    }
    for record in &runtime.session().compactions {
        println!(
            "compaction {:?}: omitted {}, retained {}",
            record.kind, record.omitted_messages, record.retained_messages
        );
        println!("{}", record.summary);
    }
    for summary in &runtime.session().branch_summaries {
        println!(
            "branch {} -> {}",
            summary.from_session_id, summary.to_session_id
        );
        println!("{}", summary.summary);
    }
}

fn print_settings(config: &LoadedConfig, runtime: &Runtime) {
    println!(
        "{}",
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
    );
}

fn print_hotkeys(config: &LoadedConfig) {
    println!(
        "{}",
        terminal_renderer(config).keybindings(&keybinding_map(config))
    );
}

fn print_diagnostics(config: &LoadedConfig) {
    if config.diagnostics.is_empty() {
        println!("no diagnostics");
        return;
    }
    for diagnostic in &config.diagnostics {
        println!("diagnostic: {diagnostic}");
    }
}

fn print_resources(kind: &str, resources: &[ResourceFile]) {
    if resources.is_empty() {
        println!("no {kind}");
        return;
    }
    for resource in resources {
        println!("{}\t{}", resource.name, resource.path.display());
    }
}

fn print_queue(runtime: &Runtime) {
    if runtime.session().queued_messages.is_empty() {
        println!("queue is empty");
        return;
    }
    for (index, message) in runtime.session().queued_messages.iter().enumerate() {
        println!("{}.\t{}", index + 1, message);
    }
}

fn find_resource<'a>(resources: &'a [ResourceFile], name: &str) -> Option<&'a ResourceFile> {
    resources.iter().find(|resource| resource.name == name)
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

fn print_scoped_models(config: &LoadedConfig, runtime: &Runtime) {
    let models = config
        .models
        .iter()
        .map(|model| ModelView {
            provider: model.provider.clone(),
            id: model.id.clone(),
            active: runtime
                .session()
                .active_model
                .as_ref()
                .map(|active| active.provider == model.provider && active.id == model.id)
                .unwrap_or(false),
        })
        .collect::<Vec<_>>();
    println!("{}", terminal_renderer(config).models(&models));
}

fn print_login_status(config: &LoadedConfig, provider: &str) {
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
    for provider in providers {
        let status = if auth_for_provider(&config.auth, &provider).is_some() {
            "available"
        } else {
            "missing"
        };
        println!("{provider}: {status}");
    }
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

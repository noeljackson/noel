use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

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
    auth_for_provider, has_auth_for_provider, load_config, AuthCredential, ConfigPaths,
    LoadedConfig, ProviderApi as ConfigProviderApi, ResolvedAuth, ResourceFile, ENV_SESSION_DIR,
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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
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
    let initial_media = load_media_inputs(&cwd, &cli.image)?;
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
            cli.offline,
        )
        .await?;
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

fn load_media_inputs(cwd: &Path, paths: &[PathBuf]) -> Result<Vec<MediaInput>> {
    paths
        .iter()
        .map(|path| load_media_input(cwd, path))
        .collect()
}

fn load_media_input(cwd: &Path, path: &Path) -> Result<MediaInput> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let bytes = fs::read(&path)?;
    let mime_type = media_mime_type(&path)?;
    let (width, height) = image_dimensions(&bytes, &mime_type).unwrap_or((None, None));
    Ok(MediaInput {
        mime_type,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        path: Some(path.display().to_string()),
        width,
        height,
    })
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
}

impl TuiSelectorState {
    fn new(kind: impl Into<String>, selector: Selector, query: impl Into<String>) -> Self {
        let mut state = Self {
            kind: kind.into(),
            title: selector.title,
            items: selector.items,
            filtered_indices: Vec::new(),
            selected: 0,
            query: query.into(),
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
        lines.push(Line::from(Span::styled(
            format!(
                "{:>2}. {marker}{active} {}\t{}",
                position + 1,
                item.label,
                item.value
            ),
            style,
        )));
    }
    if selector.filtered_indices.is_empty() {
        lines.push(Line::from(Span::styled(
            "no matches",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let title = format!(" {} selector  enter select  esc cancel ", selector.title);
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(paragraph, area);
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
                        app.push(TuiEntryKind::Error, format!("{error}"));
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
        KeyCode::Backspace => {
            if let Some(selector) = app.selector.as_mut() {
                selector.pop_query_char();
            }
        }
        KeyCode::Enter => {
            if let Some(selector) = app.selector.take() {
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
    let state = TuiSelectorState::new(kind, selector, query);
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
            runtime.set_active_model(Some(model.clone()))?;
            app.push(
                TuiEntryKind::System,
                format!("model: {}/{}", model.provider, model.id),
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
    if handle_tui_bang(app, runtime, &line).await? {
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
        "/model" | "/scoped-models" => open_tui_selector(app, config, runtime, "model", "")?,
        "/session" => app.push(TuiEntryKind::System, format_session(runtime)),
        "/settings" => app.push(TuiEntryKind::System, format_settings(config, runtime)),
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
            match load_media_input(&runtime.session().cwd, Path::new(path)) {
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
            runtime.set_active_model(Some(model.clone()))?;
            app.push(
                TuiEntryKind::System,
                format!("model: {}/{}", model.provider, model.id),
            );
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

async fn handle_tui_bang(app: &mut TuiApp, runtime: &mut Runtime, line: &str) -> Result<bool> {
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
    match run_excluded_bash(runtime, command.clone()).await {
        Ok(output) => {
            app.push(TuiEntryKind::Tool, output);
            app.last_shell_command = Some(command);
        }
        Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
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
        app.push(TuiEntryKind::Error, format!("{error}"));
    }
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
    maybe_auto_compact(runtime, false)?;
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
    }
    Ok(())
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
    let entry_index = app.push_placeholder(
        kind.clone(),
        if kind == TuiEntryKind::Tool {
            "running..."
        } else {
            "Working..."
        },
    );
    terminal.draw(|frame| draw_tui(frame, app))?;
    let provider = provider_for_runtime(runtime, config, offline)?;
    if kind == TuiEntryKind::Tool {
        let response =
            run_user_turn_streaming_with_media(runtime, provider.as_ref(), prompt, media, |_| {})
                .await?;
        app.replace_entry(entry_index, response);
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
        "status\tmodel:{}\ttheme:{}\tqueue:{}\thistory:{}\tdiagnostics:{}",
        runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .unwrap_or_else(|| "-".to_string()),
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
    if config.diagnostics.is_empty() {
        return "no diagnostics".to_string();
    }
    config
        .diagnostics
        .iter()
        .map(|diagnostic| format!("diagnostic: {diagnostic}"))
        .collect::<Vec<_>>()
        .join("\n")
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
            runtime.set_active_model(Some(model.clone()))?;
            Ok(format!("model: {}/{}", model.provider, model.id))
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
        _ => Err(anyhow!("unknown selector: {kind}")),
    }
}

fn format_resources(kind: &str, resources: &[ResourceFile]) -> String {
    if resources.is_empty() {
        return format!("no {kind}");
    }
    resources
        .iter()
        .map(|resource| format!("{}\t{}", resource.name, resource.path.display()))
        .collect::<Vec<_>>()
        .join("\n")
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

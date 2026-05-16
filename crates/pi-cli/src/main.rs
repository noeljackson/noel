use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};
use pi_ai::{create_provider, ModelRef, ProviderApi as AiProviderApi, ProviderConfig};
use pi_config::{
    api_key_for_provider, load_config, ConfigPaths, LoadedConfig, ProviderApi as ConfigProviderApi,
};
use pi_core::{run_user_turn, ReloadableSystems, Runtime, SessionState, SessionStore};

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
    no_session: bool,

    #[arg(long)]
    session: Option<PathBuf>,

    #[arg(long)]
    session_dir: Option<PathBuf>,

    #[arg(long)]
    provider: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    list_models: bool,

    #[arg(long)]
    system_prompt: Option<String>,

    #[arg()]
    messages: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd.clone(), cli.session_dir.clone())?;
    let mut config = load_config(paths)?;
    if let Some(system_prompt) = &cli.system_prompt {
        config.system_prompt = Some(system_prompt.clone());
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

    let mut initial_prompt = cli.messages.join(" ");
    if !io::stdin().is_terminal() && !matches!(cli.mode, OutputMode::Rpc) {
        let mut stdin = String::new();
        io::stdin().read_to_string(&mut stdin)?;
        initial_prompt = [initial_prompt, stdin.trim().to_string()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    if cli.print || !initial_prompt.is_empty() || !io::stdin().is_terminal() {
        if initial_prompt.is_empty() {
            return Err(anyhow!("print mode requires a prompt"));
        }
        let response = run_prompt(&mut runtime, &config, initial_prompt).await?;
        print_response(&cli.mode, &response);
        return Ok(());
    }

    run_interactive(runtime, config).await
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

    if let Some(path) = &cli.session {
        let (store, state) = SessionStore::open(path.clone())?;
        return Ok(Runtime::with_store(state, systems, store));
    }

    if cli.r#continue || cli.resume {
        if let Some(path) = most_recent_session(&config.paths.session_dir)? {
            let (store, state) = SessionStore::open(path)?;
            return Ok(Runtime::with_store(state, systems, store));
        }
    }

    let (store, state) = SessionStore::create(&config.paths.session_dir, cwd.to_path_buf())?;
    Ok(Runtime::with_store(state, systems, store))
}

fn most_recent_session(session_dir: &PathBuf) -> Result<Option<PathBuf>> {
    if !session_dir.exists() {
        return Ok(None);
    }
    let mut sessions = std::fs::read_dir(session_dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified = metadata.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|(modified, _)| *modified);
    Ok(sessions.pop().map(|(_, path)| path))
}

fn select_initial_model(runtime: &mut Runtime, config: &LoadedConfig, cli: &Cli) -> Result<()> {
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
                    .find(|model| api_key_for_provider(&config.auth, &model.provider).is_some())
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

async fn run_interactive(mut runtime: Runtime, mut config: LoadedConfig) -> Result<()> {
    println!("pi rust cli");
    println!("type /help for commands, /reload to reload config, /quit to exit");
    let stdin = io::stdin();
    loop {
        print!("pi> ");
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
        match line.as_str() {
            "/quit" => break,
            "/help" => {
                print_help();
                continue;
            }
            "/models" => {
                for model in &config.models {
                    println!("{}/{}", model.provider, model.id);
                }
                continue;
            }
            "/session" => {
                println!("session: {}", runtime.session().session_id);
                println!("cwd: {}", runtime.session().cwd.display());
                if let Some(store) = runtime.store() {
                    println!("file: {}", store.path().display());
                }
                continue;
            }
            "/reload" => {
                config = load_config(config.paths.clone())?;
                let next_generation = runtime.systems().config_generation + 1;
                let report =
                    runtime.reload(ReloadableSystems::from_config(&config, next_generation))?;
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
            _ if line.starts_with("/model ") => {
                let reference = line.trim_start_matches("/model ").trim();
                let model = resolve_model_reference(&config, reference)
                    .ok_or_else(|| anyhow!("model not found: {reference}"))?;
                runtime.set_active_model(Some(model.clone()))?;
                println!("model: {}/{}", model.provider, model.id);
                continue;
            }
            _ => {}
        }

        match run_prompt(&mut runtime, &config, line).await {
            Ok(response) => println!("{response}"),
            Err(error) => eprintln!("error: {error}"),
        }
    }
    Ok(())
}

async fn run_prompt(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
) -> Result<String> {
    let provider = provider_for_runtime(runtime, config)?;
    run_user_turn(runtime, provider.as_ref(), prompt)
        .await
        .map_err(Into::into)
}

fn provider_for_runtime(
    runtime: &Runtime,
    config: &LoadedConfig,
) -> Result<Box<dyn pi_ai::Provider>> {
    let model = runtime
        .session()
        .active_model
        .clone()
        .ok_or_else(|| anyhow!("no active model; configure auth or use --model faux/echo"))?;
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
        api_key: api_key_for_provider(&config.auth, &definition.provider),
    }))
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

fn print_help() {
    println!("/help                  show commands");
    println!("/models                list configured models");
    println!("/model <provider/id>   switch model");
    println!("/session               show session info");
    println!("/reload                reload config without clearing context");
    println!("/read <path>           read file");
    println!("/write <path> <text>   write file");
    println!("/edit <path> <a> <b>   replace text");
    println!("/grep <text> [path]    search files");
    println!("/find <text>           find files by substring");
    println!("/ls [path]             list directory");
    println!("/bash <command>        run shell command");
    println!("/quit                  exit");
}

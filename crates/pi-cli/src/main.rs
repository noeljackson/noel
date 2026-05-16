use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use pi_core::{ReloadableSystems, Runtime, SessionState};

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

    #[arg(long)]
    no_session: bool,

    #[arg(long)]
    session_dir: Option<PathBuf>,

    #[arg(long)]
    provider: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg()]
    messages: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let mut session = SessionState::new("local", cwd);
    session.active_model = cli
        .provider
        .zip(cli.model)
        .map(|(provider, id)| pi_ai::ModelRef { provider, id });

    let systems = ReloadableSystems::default();
    let _runtime = Runtime::new(session, systems);

    if cli.print || !cli.messages.is_empty() {
        println!("{}", cli.messages.join(" "));
        return Ok(());
    }

    let session_mode = if cli.no_session {
        "ephemeral"
    } else {
        "persistent"
    };
    let session_dir = cli
        .session_dir
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "default".to_string());
    println!(
        "pi rust cli skeleton ({:?}, {session_mode}, session-dir: {session_dir})",
        cli.mode
    );
    Ok(())
}

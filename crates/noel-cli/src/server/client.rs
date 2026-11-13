//! `noel client`: line mode over the server (spec §5). It connects to
//! `server.sock`, starting a detached `noel server` when none listens, and
//! holds the server's objects:
//!
//! - `ls`: `noel.sessions`' directory;
//! - `new [--cwd DIR] [--model M] [--name N] [--thinking L] [--workspace URL]`:
//!   `create`;
//! - `send <id> <text> [--steer|--follow-up] [--request-id R]`: `noel.chat`'s
//!   `submit`. A send that fails `unavailable` (its worker was lost, or is
//!   starting) is sent again with the same `requestId`, which reports the
//!   first submission if it got through (spec §8.1);
//! - `watch <id>`: the view, its snapshot then each change, as text (or
//!   JSON lines with `--json`). It holds again when the worker is lost, and
//!   reconnects (starting a server) when the server is;
//! - `abort <id>`, `resume <id> [<runId>]`, `withdraw <id> <submissionId>`;
//! - `plugins`, `run <plugin> <command> [args]`, `panel <plugin> <panel>
//!   <action> [value]`: `noel.plugins` (spec §13).
//!
//! With `--remote <ssh-destination>`, the server is another machine's: the
//! client runs `ssh -T <destination> noel bridge` ([`super::bridge`]) and
//! speaks the same link over its stdin and stdout. ssh authenticates, with
//! the user's own ssh configuration, keys and known hosts; the server stamps
//! the link `{kind: "owner", via: "tunnel"}` and starts when none listens. A
//! lost ssh is a lost link, reconnected as a lost local server is, with ssh
//! in batch mode so it never asks on the terminal again.
//!
//! The TUI attached to a session (`noel --attach`, [`crate::attach`]) uses
//! the same connection: [`Client::follow`] and [`Client::chat_call`].

use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use noel_chord::delta::{Op, Seg};
use noel_chord::v2::host::StateUpdate;
use noel_chord::v2::object::BoxFuture;
use noel_client::{websocket_link, Connector};
pub(crate) use noel_client::{Client, Followed, Report};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, Join, ReadBuf};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::serve::{start_detached, Started};
use super::{new_request_id, peer_check, RuntimeDir};

#[derive(Debug, Parser)]
#[command(
    name = "noel client",
    about = "Use noel server's sessions from the command line"
)]
pub(crate) struct ClientArgs {
    /// Use the server on another machine, through `ssh <DEST> noel bridge`
    /// (DEST as ssh takes it: a host, user@host, or a Host from
    /// ~/.ssh/config).
    #[arg(long, value_name = "DEST", global = true)]
    remote: Option<String>,
    /// The noel program on the remote machine (default: `noel` on its
    /// PATH; a leading ~/ is its home).
    #[arg(long, value_name = "PATH", requires = "remote", global = true)]
    remote_noel: Option<String>,
    /// Use a server this device paired with (`noel client pair`), over TLS
    /// (spec §16).
    #[arg(long, value_name = "URL", global = true, conflicts_with = "remote")]
    server: Option<String>,
    #[command(subcommand)]
    command: ClientCommand,
}

#[derive(Debug, Subcommand)]
enum ClientCommand {
    /// List the sessions.
    Ls {
        /// Print the directory as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create a session and print its id.
    New {
        /// Its working directory (default: this one; with --remote, an
        /// absolute path on that machine, which it needs).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Its model, `provider/id`.
        #[arg(long)]
        model: Option<String>,
        /// Its name.
        #[arg(long)]
        name: Option<String>,
        /// Its thinking level.
        #[arg(long)]
        thinking: Option<String>,
        /// Run its tools on another machine:
        /// ssh://[user@]host[:port]/absolute/path, on a host trusted with
        /// `noel workspace trust`.
        #[arg(long, value_name = "URL")]
        workspace: Option<String>,
        /// The create's id; a repeat returns the same session.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Send a prompt (or with --steer or --follow-up, queue it while a run
    /// is active).
    Send {
        session: String,
        text: String,
        #[arg(long, conflicts_with = "follow_up")]
        steer: bool,
        #[arg(long)]
        follow_up: bool,
        /// The submission's id; a repeat reports the first submission.
        #[arg(long)]
        request_id: Option<String>,
        /// `reject`: refuse it while no run is active, so it never starts
        /// a run (Durable's `whenIdle`).
        #[arg(long, value_name = "reject")]
        when_idle: Option<String>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start a run on the session's context as it is, without a prompt
    /// (Durable's continue): after a run that ended without an answer.
    Continue {
        session: String,
        /// The submission's id; a repeat reports the first submission.
        #[arg(long)]
        request_id: Option<String>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a slash command (or a prompt) on the session as the TUI would,
    /// and print its output.
    Command {
        session: String,
        line: String,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the session's view, then its changes, until interrupted.
    Watch {
        session: String,
        /// Print each update as a JSON line.
        #[arg(long)]
        json: bool,
    },
    /// Abort the active run; queued submissions are withdrawn.
    Abort {
        session: String,
        #[arg(long)]
        expected_run_id: Option<String>,
    },
    /// Continue the session's interrupted last run.
    Resume {
        session: String,
        /// The run (default: the last run).
        run_id: Option<String>,
    },
    /// Withdraw a queued submission.
    Withdraw {
        session: String,
        submission_id: String,
    },
    /// List the plugins: their permissions, contributions and panels.
    Plugins {
        /// Print the directory as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a plugin's command and print its output.
    Run {
        plugin: String,
        command: String,
        /// The command's arguments, as one string.
        args: Vec<String>,
    },
    /// Pair this device with a server that listens for devices
    /// (`server.listen`): show a code to approve there, then keep the token.
    Pair {
        url: String,
        /// How the server lists this device.
        #[arg(long, default_value = "device")]
        label: String,
    },
    /// The devices paired with this machine's server: list, `approve
    /// <code>`, `revoke <id>`.
    Devices {
        action: Option<String>,
        argument: Option<String>,
    },
    /// Rename a session (an empty name clears it).
    Rename { session: String, name: String },
    /// Delete a session that no worker holds.
    Delete { session: String },
    /// Fork a session into a new one (with --clone, a whole copy).
    Fork {
        session: String,
        #[arg(long)]
        clone: bool,
    },
    /// Import an exported session (a file on the server's machine), or
    /// with `claude` a Claude Code session of this directory: alone it
    /// lists them, with a number or an id it imports that one.
    Import {
        path: String,
        session: Option<String>,
    },
    /// The models the server's sessions can use, and the scoped ones.
    Models {
        #[arg(long)]
        json: bool,
    },
    /// The server machine's provider accounts.
    Accounts {
        #[arg(long)]
        json: bool,
    },
    /// Log in to a provider on the server's machine: a device code or a
    /// browser sign-in shown here, or an API key read from stdin.
    Login {
        provider: String,
        #[arg(long, default_value = noel_config::DEFAULT_ACCOUNT_NAME)]
        account: String,
    },
    /// Remove a provider account from the server's machine.
    Logout {
        provider: String,
        #[arg(long, default_value = noel_config::DEFAULT_ACCOUNT_NAME)]
        account: String,
    },
    /// The server machine's settings; with a key and a JSON value, set it
    /// (`null` removes it).
    Settings {
        key: Option<String>,
        value: Option<String>,
    },
    /// Send a plugin panel an action (a button's).
    Panel {
        plugin: String,
        panel: String,
        action: String,
        value: Option<String>,
    },
}

pub(crate) async fn run(args: ClientArgs) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let agent_dir = match noel_config::ConfigPaths::discover(cwd, None) {
        Ok((paths, _)) => paths.agent_dir,
        Err(error) => {
            eprintln!("noel client: {error}");
            return 2;
        }
    };
    if let ClientCommand::Pair { url, label } = &args.command {
        return match super::remote::pair(&agent_dir, url, label).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("noel client: pair: {error:#}");
                1
            }
        };
    }
    let endpoint = match &args.server {
        Some(url) => super::remote::paired(&agent_dir, url).map(Endpoint::Paired),
        None => Endpoint::new(args.remote, args.remote_noel),
    };
    let endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("noel client: {error:#}");
            return 2;
        }
    };
    match command(endpoint, args.command).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("noel client: {error:#}");
            1
        }
    }
}

async fn command(endpoint: Endpoint, command: ClientCommand) -> Result<i32> {
    // The working directory of `new`: this one only when the server is here.
    if let ClientCommand::New { cwd, .. } = &command
        && let Endpoint::Ssh { destination, .. } = &endpoint
        && !cwd.as_ref().is_some_and(|cwd| cwd.is_absolute())
    {
        return Err(anyhow!(
            "with --remote, pass --cwd: an absolute path on {destination}"
        ));
    }
    let client = connect(
        endpoint,
        Arc::new(|message| eprintln!("noel client: {message}")),
    )
    .await?;
    match command {
        ClientCommand::Ls { json } => ls(&client, json).await,
        ClientCommand::New {
            cwd,
            model,
            name,
            thinking,
            workspace,
            request_id,
        } => {
            let cwd = match cwd {
                Some(cwd) if cwd.is_absolute() => cwd,
                Some(cwd) => std::path::absolute(cwd)?,
                None => std::env::current_dir()?,
            };
            let mut args = json!({
                "requestId": request_id.unwrap_or_else(new_request_id),
                "cwd": cwd.display().to_string(),
            });
            for (key, value) in [
                ("model", model),
                ("name", name),
                ("thinkingLevel", thinking),
                ("workspace", workspace),
            ] {
                if let Some(value) = value {
                    args[key] = json!(value);
                }
            }
            let result = client.sessions_call("create", args).await?;
            println!("{}", result["sessionId"].as_str().unwrap_or_default());
            Ok(0)
        }
        ClientCommand::Send {
            session,
            text,
            steer,
            follow_up,
            request_id,
            when_idle,
            json,
        } => {
            let when_busy = if steer {
                "steer"
            } else if follow_up {
                "followUp"
            } else {
                "reject"
            };
            let request_id = request_id.unwrap_or_else(new_request_id);
            let mut args = json!({"requestId": request_id, "text": text, "whenBusy": when_busy});
            if let Some(when_idle) = when_idle {
                args["whenIdle"] = json!(when_idle);
            }
            let result = client.chat_call(&session, "submit", args, true).await?;
            if json {
                println!("{}", json!({"requestId": request_id, "result": result}));
            } else {
                println!("{}", describe_submit(&result, &request_id));
            }
            Ok(0)
        }
        ClientCommand::Continue {
            session,
            request_id,
            json,
        } => {
            let request_id = request_id.unwrap_or_else(new_request_id);
            let args = json!({"requestId": request_id, "type": "continue"});
            let result = client.chat_call(&session, "submit", args, true).await?;
            if json {
                println!("{}", json!({"requestId": request_id, "result": result}));
            } else {
                println!("{}", describe_submit(&result, &request_id));
            }
            Ok(0)
        }
        ClientCommand::Command {
            session,
            line,
            json,
        } => {
            let args = json!({"line": line, "requestId": new_request_id()});
            let result = client.chat_call(&session, "command", args, true).await?;
            if json {
                println!("{result}");
                return Ok(0);
            }
            let mut failed = false;
            for entry in result["entries"].as_array().into_iter().flatten() {
                let text = entry["text"].as_str().unwrap_or_default();
                if entry["kind"] == "error" {
                    failed = true;
                    eprintln!("{text}");
                } else {
                    println!("{text}");
                }
            }
            match result["next"]["type"].as_str() {
                Some("run") => println!(
                    "started run {}",
                    result["next"]["runId"].as_str().unwrap_or("")
                ),
                Some("session") => println!(
                    "session {}",
                    result["next"]["session"].as_str().unwrap_or("")
                ),
                Some("selector") => println!("{}", describe_selector(&result["next"]["selector"])),
                Some("login") => println!(
                    "login {} needs a terminal on the server's machine",
                    result["next"]["provider"].as_str().unwrap_or("")
                ),
                _ => {}
            }
            Ok(if failed { 1 } else { 0 })
        }
        ClientCommand::Watch { session, json } => watch(&client, &session, json).await,
        ClientCommand::Abort {
            session,
            expected_run_id,
        } => {
            let mut args = json!({});
            if let Some(run_id) = expected_run_id {
                args["expectedRunId"] = json!(run_id);
            }
            let result = client.chat_call(&session, "abort", args, false).await?;
            let withdrawn = result["withdrawn"].as_u64().unwrap_or(0);
            match result["runId"].as_str() {
                Some(run_id) => println!("aborted run {run_id}; withdrew {withdrawn} queued"),
                None => println!("no run was active; withdrew {withdrawn} queued"),
            }
            Ok(0)
        }
        ClientCommand::Resume { session, run_id } => {
            let run_id = match run_id {
                Some(run_id) => run_id,
                None => {
                    let view = client.view(&session).await?;
                    view["lastRun"]["runId"]
                        .as_str()
                        .ok_or_else(|| anyhow!("session {session} has no run"))?
                        .to_owned()
                }
            };
            let args = json!({"expectedRunId": run_id});
            let result = client.chat_call(&session, "resume", args, false).await?;
            println!(
                "resumed run {}",
                result["runId"].as_str().unwrap_or_default()
            );
            Ok(0)
        }
        ClientCommand::Withdraw {
            session,
            submission_id,
        } => {
            let args = json!({"submissionId": submission_id});
            let result = client.chat_call(&session, "withdraw", args, false).await?;
            println!("{}", result["outcome"].as_str().unwrap_or_default());
            Ok(0)
        }
        ClientCommand::Pair { .. } => unreachable!("handled before connecting"),
        ClientCommand::Devices { action, argument } => {
            let call = |method: &'static str, key: &'static str| {
                let client = client.clone();
                let argument = argument.clone();
                async move {
                    let value =
                        argument.ok_or_else(|| anyhow!("devices {method} needs its {key}"))?;
                    client.devices_call(method, json!({ key: value })).await
                }
            };
            match action.as_deref() {
                None | Some("ls") => {
                    let devices = client.device_directory().await?;
                    println!(
                        "certificate: {}",
                        devices["fingerprint"].as_str().unwrap_or_default()
                    );
                    match devices["listening"].as_str() {
                        Some(address) => println!("listening: {address}"),
                        None => println!("listening: no (set server.listen)"),
                    }
                    for device in devices["devices"].as_array().into_iter().flatten() {
                        println!(
                            "device {} {}",
                            device["id"].as_str().unwrap_or_default(),
                            device["label"].as_str().unwrap_or_default()
                        );
                    }
                    for pending in devices["pending"].as_array().into_iter().flatten() {
                        println!(
                            "pending {} {}",
                            pending["code"].as_str().unwrap_or_default(),
                            pending["label"].as_str().unwrap_or_default()
                        );
                    }
                }
                Some("approve") => {
                    let approved = call("approve", "code").await?;
                    println!(
                        "approved: device {}",
                        approved["id"].as_str().unwrap_or_default()
                    );
                }
                Some("revoke") => {
                    call("revoke", "id").await?;
                    println!("revoked");
                }
                Some(other) => {
                    return Err(anyhow!("devices: no action {other} (ls, approve, revoke)"))
                }
            }
            Ok(0)
        }
        ClientCommand::Rename { session, name } => {
            let args = json!({"sessionId": session, "name": name});
            client.sessions_call("rename", args).await?;
            println!("renamed {session}");
            Ok(0)
        }
        ClientCommand::Delete { session } => {
            client
                .sessions_call("delete", json!({"sessionId": session}))
                .await?;
            println!("deleted {session}");
            Ok(0)
        }
        ClientCommand::Fork { session, clone } => {
            let args = json!({"requestId": new_request_id(), "sessionId": session, "clone": clone});
            let made = client.sessions_call("fork", args).await?;
            println!("{}", made["sessionId"].as_str().unwrap_or_default());
            Ok(0)
        }
        ClientCommand::Import { path, session } => {
            let cwd = std::env::current_dir()?;
            let words = format!("{path} {}", session.unwrap_or_default());
            let path = match crate::import_source(&cwd, &words)? {
                crate::ImportSource::File(path) => path,
                crate::ImportSource::ClaudeCode(sessions) => {
                    println!("{}", crate::format_claude_code_sessions(&cwd, &sessions));
                    return Ok(0);
                }
            };
            let path = std::path::absolute(&path).unwrap_or(path);
            let args = json!({"requestId": new_request_id(), "path": path});
            let made = client.sessions_call("import", args).await?;
            println!("{}", made["sessionId"].as_str().unwrap_or_default());
            Ok(0)
        }
        ClientCommand::Models { json } => {
            let catalog = client
                .server_state(noel_client::contracts::models(), "catalog")
                .await?;
            if json {
                println!("{catalog}");
            } else {
                for model in catalog["models"].as_array().into_iter().flatten() {
                    let mut notes = Vec::new();
                    if model["scoped"] == true {
                        notes.push("scoped");
                    }
                    if model["available"] != true {
                        notes.push("no auth");
                    }
                    println!(
                        "{}/{}{}",
                        model["provider"].as_str().unwrap_or_default(),
                        model["modelId"].as_str().unwrap_or_default(),
                        if notes.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", notes.join(", "))
                        }
                    );
                }
            }
            Ok(0)
        }
        ClientCommand::Accounts { json } => {
            let accounts = client
                .server_state(noel_client::contracts::accounts(), "accounts")
                .await?;
            if json {
                println!("{accounts}");
            } else {
                for line in render_accounts(&accounts) {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        ClientCommand::Login { provider, account } => {
            let line = relay_login(&client, &provider, &account, |kind| {
                use std::io::BufRead;
                match kind {
                    "key" => eprintln!("paste the {provider} API key:"),
                    _ => eprintln!("after signing in, paste the full code shown (code#state):"),
                }
                let mut input = String::new();
                std::io::stdin().lock().read_line(&mut input)?;
                Ok(input.trim().to_owned())
            })
            .await?;
            println!("{line}");
            Ok(0)
        }
        ClientCommand::Logout { provider, account } => {
            let args = json!({"provider": provider, "account": account});
            let removed = client
                .server_call(noel_client::contracts::accounts(), "logout", args)
                .await?;
            if removed["removed"] == true {
                println!("logged out {provider} account {account}");
            } else {
                println!("no account {account} of {provider}");
            }
            Ok(0)
        }
        ClientCommand::Settings { key, value } => {
            let contract = noel_client::contracts::settings;
            match (key, value) {
                (Some(key), Some(value)) => {
                    let value: Value = serde_json::from_str(&value)
                        .map_err(|error| anyhow!("{value} is not JSON: {error}"))?;
                    let set = client
                        .server_call(contract(), "set", json!({"key": key, "value": value}))
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&set["values"])?);
                }
                (Some(key), None) => {
                    let settings = client.server_state(contract(), "settings").await?;
                    let mut value = &settings["values"];
                    for part in key.split('.') {
                        value = &value[part];
                    }
                    println!("{value}");
                }
                (None, _) => {
                    let settings = client.server_state(contract(), "settings").await?;
                    println!("{}", settings["path"].as_str().unwrap_or_default());
                    println!("{}", serde_json::to_string_pretty(&settings["values"])?);
                }
            }
            Ok(0)
        }
        ClientCommand::Plugins { json } => {
            let directory = client.plugin_directory().await?;
            if json {
                println!("{directory}");
            } else {
                for line in render_plugins(&directory) {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        ClientCommand::Run {
            plugin,
            command,
            args,
        } => {
            let args = json!({"plugin": plugin, "name": command, "args": args.join(" ")});
            let result = client.plugins_call("runCommand", args).await?;
            println!("{}", result["output"].as_str().unwrap_or_default());
            Ok(0)
        }
        ClientCommand::Panel {
            plugin,
            panel,
            action,
            value,
        } => {
            let mut args = json!({"plugin": plugin, "panel": panel, "action": action});
            if let Some(value) = value {
                args["value"] = json!(value);
            }
            client.plugins_call("panelAction", args).await?;
            Ok(0)
        }
    }
}

/// The plugin directory as text: each plugin, its permissions, tools,
/// commands and panels, rendered.
fn render_plugins(directory: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let plugins = directory["plugins"].as_array().cloned().unwrap_or_default();
    if plugins.is_empty() {
        lines.push("no plugins".to_owned());
    }
    let names = |list: &Value| {
        list.as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["name"].as_str().or(entry.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for plugin in plugins {
        let state = if plugin["running"] == json!(true) {
            "running".to_owned()
        } else {
            format!(
                "stopped{}",
                plugin["error"]
                    .as_str()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            )
        };
        lines.push(format!(
            "{} {} restarts {}",
            plugin["name"].as_str().unwrap_or_default(),
            state,
            plugin["restarts"]
        ));
        lines.push(format!(
            "  permissions: {} of {}",
            names(&plugin["granted"]),
            names(&plugin["requested"])
        ));
        let contributions = &plugin["contributions"];
        lines.push(format!("  tools: {}", names(&contributions["tools"])));
        lines.push(format!("  commands: {}", names(&contributions["commands"])));
        for panel in contributions["panels"].as_array().into_iter().flatten() {
            lines.push(format!(
                "  panel {} ({}):",
                panel["id"].as_str().unwrap_or_default(),
                panel["title"].as_str().unwrap_or_default()
            ));
            render_panel(&panel["view"], 2, &mut lines);
        }
    }
    lines
}

/// A panel's view (spec §13's widget vocabulary) as indented text.
fn render_panel(view: &Value, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    match view["type"].as_str() {
        Some("text") => lines.push(format!(
            "{indent}{}",
            view["text"].as_str().unwrap_or_default()
        )),
        Some("button") => lines.push(format!(
            "{indent}[{}] ({})",
            view["label"].as_str().unwrap_or_default(),
            view["action"].as_str().unwrap_or_default()
        )),
        Some("list") => {
            for item in view["items"].as_array().into_iter().flatten() {
                let mut nested = Vec::new();
                render_panel(item, 0, &mut nested);
                for (index, line) in nested.into_iter().enumerate() {
                    let mark = if index == 0 { "- " } else { "  " };
                    lines.push(format!("{indent}{mark}{line}"));
                }
            }
        }
        Some("stack") => {
            for child in view["children"].as_array().into_iter().flatten() {
                render_panel(child, depth, lines);
            }
        }
        _ => lines.push(format!("{indent}{view}")),
    }
}

fn describe_submit(result: &Value, request_id: &str) -> String {
    let id = result["submissionId"].as_str().unwrap_or_default();
    match result["disposition"].as_str() {
        Some("started") => format!("started run {id} (request {request_id})"),
        Some("queued") => format!("queued submission {id} (request {request_id})"),
        _ => {
            let status = result["status"].as_str().unwrap_or("unknown");
            let run = result["runId"]
                .as_str()
                .map(|run| format!(" run {run}"))
                .unwrap_or_default();
            format!("repeated submission {id}: {status}{run} (request {request_id})")
        }
    }
}

/// Where the server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// This machine's, on `server.sock`.
    Local,
    /// Another machine's, through `ssh -T <destination> <program> bridge`.
    Ssh {
        destination: String,
        program: String,
    },
    /// A server this device paired with, over TLS.
    Paired(super::remote::Paired),
}

impl Endpoint {
    /// From `--remote` and `--remote-noel`.
    pub(crate) fn new(remote: Option<String>, program: Option<String>) -> Result<Self> {
        let Some(destination) = remote else {
            return Ok(Self::Local);
        };
        // ssh would take a leading `-` as an option; a space or a control
        // character is no destination.
        if destination.is_empty()
            || destination.starts_with('-')
            || destination
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(anyhow!(
                "--remote: {destination:?} is not an ssh destination"
            ));
        }
        let program = program.unwrap_or_else(|| "noel".to_owned());
        if program.is_empty() || program.chars().any(char::is_control) {
            return Err(anyhow!("--remote-noel: {program:?} is not a program"));
        }
        Ok(Self::Ssh {
            destination,
            program,
        })
    }
}

impl Connector for Endpoint {
    fn open(&self, interactive: bool, report: Report) -> BoxFuture<Result<noel_chord::v2::Link>> {
        let endpoint = self.clone();
        Box::pin(async move { open(&endpoint, interactive, &report).await })
    }
}

/// Connects to the server at `endpoint`, starting a local one when none
/// listens.
pub(crate) async fn connect(endpoint: Endpoint, report: Report) -> Result<Client> {
    Client::connect(Arc::new(endpoint), report).await
}

/// The accounts, one line each: `provider account kind`, with failover's
/// current account and a rate limit noted.
fn render_accounts(accounts: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for provider in accounts["providers"].as_array().into_iter().flatten() {
        let name = provider["provider"].as_str().unwrap_or_default();
        for account in provider["accounts"].as_array().into_iter().flatten() {
            let account_name = account["name"].as_str().unwrap_or_default();
            let mut line = format!(
                "{name} {account_name} {}",
                account["kind"].as_str().unwrap_or_default()
            );
            if provider["current"].as_str() == Some(account_name) {
                line.push_str(" (current)");
            }
            if let Some(until) = account["limitedUntil"].as_i64() {
                line.push_str(&format!(" (rate limited until {until})"));
            }
            lines.push(line);
        }
    }
    if lines.is_empty() {
        lines.push("no accounts".to_owned());
    }
    lines
}

/// Runs a login on the server's machine, relaying it: a device code or a
/// sign-in URL is shown (on stderr), and `read` gives the pasted code or
/// the API key. Returns the line to show when it is done.
pub(crate) async fn relay_login(
    client: &Client,
    provider: &str,
    account: &str,
    read: impl FnOnce(&str) -> Result<String>,
) -> Result<String> {
    let contract = noel_client::contracts::accounts;
    let args = json!({"provider": provider, "account": account});
    let started = client.server_call(contract(), "login", args).await?;
    let login_id = started["loginId"].as_str().unwrap_or_default().to_owned();
    let kind = started["kind"].as_str().unwrap_or_default();
    let mut finish = json!({ "loginId": login_id });
    match kind {
        "device" => {
            eprintln!(
                "open {} and enter code {}",
                started["url"].as_str().unwrap_or_default(),
                started["userCode"].as_str().unwrap_or_default()
            );
            eprintln!("waiting for authorization (the code expires in 15 minutes)...");
        }
        "code" => {
            eprintln!(
                "open this URL and sign in:\n{}",
                started["url"].as_str().unwrap_or_default()
            );
            finish["code"] = json!(read("code")?);
        }
        _ => finish["key"] = json!(read("key")?),
    }
    let done = client
        .server_call(contract(), "finishLogin", finish)
        .await?;
    Ok(format!(
        "logged in {provider} as account {account}{}",
        if done["replaced"] == true {
            " (replaced existing login)"
        } else {
            ""
        }
    ))
}

async fn ls(client: &Client, json: bool) -> Result<i32> {
    let directory = client.directory().await?;
    if json {
        println!("{directory}");
        return Ok(0);
    }
    let list = directory["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if list.is_empty() {
        println!("no sessions");
    }
    for session in list {
        println!("{}", describe_session(&session));
    }
    Ok(0)
}

/// Prints the view's snapshot, then each change, holding again when the
/// worker is lost and reconnecting when the server is.
async fn watch(client: &Client, session: &str, json: bool) -> Result<i32> {
    Err(client
        .follow(session, |followed| match followed {
            Followed::Update(update) => print_update(&update, json),
            Followed::ServerLost => print_note("server link lost; reconnecting", json),
            Followed::Reconnected => print_note("reconnected; holding again", json),
            Followed::ViewEnded(why) => {
                print_note(&format!("view ended ({why}); holding again"), json)
            }
        })
        .await)
}

/// A link to the server at `endpoint`. ssh asks on the terminal (a password,
/// an unknown host key) only when `interactive`.
async fn open(
    endpoint: &Endpoint,
    interactive: bool,
    report: &Report,
) -> Result<noel_chord::v2::Link> {
    match endpoint {
        Endpoint::Paired(paired) => super::remote::connect(paired).await,
        Endpoint::Local => websocket_link(dial(true, report).await?).await,
        Endpoint::Ssh {
            destination,
            program,
        } => open_ssh(destination, program, interactive, report).await,
    }
}

/// The server's socket, checked: the runtime directory is this user's and
/// so is the process listening. Starts a server when none listens.
pub(crate) async fn dial(autostart: bool, report: &Report) -> Result<UnixStream> {
    let root = RuntimeDir::locate();
    let connect = |root: PathBuf| async move {
        let dir = RuntimeDir::check(root)?;
        let stream = UnixStream::connect(dir.server_socket()).await?;
        peer_check(&stream).map_err(|reason| anyhow!("the server's socket: {reason}"))?;
        anyhow::Ok(stream)
    };
    match connect(root.clone()).await {
        Ok(stream) => Ok(stream),
        Err(error) if !autostart => Err(error),
        Err(_) => {
            match start_detached().await? {
                Started::Running(pid) => report(format!("started noel server (pid {pid})")),
                Started::AlreadyRunning(_) => {}
            }
            connect(root).await
        }
    }
}

/// The remote command: `<program> bridge`, the program quoted for the
/// remote shell, a leading `~/` left for it to expand to its home.
fn bridge_command(program: &str) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    match program.strip_prefix("~/") {
        Some(rest) => format!("~/{} bridge", quote(rest)),
        None => format!("{} bridge", quote(program)),
    }
}

/// A link through `ssh -T <destination> <program> bridge`. What ssh (and the
/// bridge) print while it connects is kept: on success it is reported, and
/// on failure it is the error. Later lines are reported as they come.
async fn open_ssh(
    destination: &str,
    program: &str,
    interactive: bool,
    report: &Report,
) -> Result<noel_chord::v2::Link> {
    let mut command = tokio::process::Command::new("ssh");
    command.args(["-T", "-e", "none"]);
    if !interactive {
        command.args(["-o", "BatchMode=yes"]);
    }
    command
        .arg("--")
        .arg(destination)
        .arg(bridge_command(program))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| anyhow!("run ssh: {error}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(anyhow!("ssh has no pipes"));
    };
    // Until the link is up, ssh's lines are kept; then they are reported.
    let kept = Arc::new(Mutex::new(Some(Vec::<String>::new())));
    let lines = {
        let (kept, report, destination) = (kept.clone(), report.clone(), destination.to_owned());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
                match kept.as_mut() {
                    Some(kept) => kept.push(line),
                    None => report(format!("{destination}: {line}")),
                }
            }
        })
    };
    let stream = SshStream {
        io: Some(tokio::io::join(stdout, stdin)),
        child: Some(child),
    };
    match websocket_link(stream).await {
        Ok(link) => {
            let said = kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or_default();
            for line in said {
                report(format!("{destination}: {line}"));
            }
            Ok(link)
        }
        Err(error) => {
            // ssh is gone, or ends with its stream: its last words are the
            // reason.
            let _ = tokio::time::timeout(Duration::from_secs(6), lines).await;
            let said = kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or_default();
            if said.is_empty() {
                Err(anyhow!("ssh {destination}: no server answered ({error})"))
            } else {
                Err(anyhow!("ssh {destination}: {}", said.join("; ")))
            }
        }
    }
}

/// ssh's stdout and stdin as one stream. When it drops, ssh's stdin closes
/// and ssh has a few seconds to pass that on and exit, so the remote side
/// ends cleanly; then it is killed.
struct SshStream {
    io: Option<Join<ChildStdout, ChildStdin>>,
    child: Option<Child>,
}

impl SshStream {
    fn io(&mut self) -> Pin<&mut Join<ChildStdout, ChildStdin>> {
        Pin::new(self.io.as_mut().expect("open until dropped"))
    }
}

impl Drop for SshStream {
    fn drop(&mut self) {
        drop(self.io.take());
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Without a runtime, `kill_on_drop` kills it at once.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            });
        }
    }
}

impl AsyncRead for SshStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.io().poll_read(cx, buf)
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.io().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.io().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.io().poll_shutdown(cx)
    }
}

pub(crate) fn describe_session(session: &Value) -> String {
    let live = &session["live"];
    let state = if live["busy"] == true {
        "running"
    } else if live["worker"] == true {
        "open"
    } else {
        "-"
    };
    let title = session["name"]
        .as_str()
        .or(session["firstPrompt"].as_str())
        .map(|text| one_line(text, 60))
        .unwrap_or_default();
    let last = session["lastRun"]["status"].as_str().unwrap_or("-");
    format!(
        "{}  {state:7}  {:>4} msgs  last run {last:11}  {}  {}",
        session["sessionId"].as_str().unwrap_or_default(),
        session["messageCount"],
        // Where its tools run: a workspace on another machine, else here.
        session["workspace"]
            .as_str()
            .or(session["cwd"].as_str())
            .unwrap_or_default(),
        title
    )
}

fn print_note(note: &str, json: bool) {
    if json {
        println!("{}", json!({ "note": note }));
    } else {
        println!("== {note}");
    }
}

fn print_update(update: &StateUpdate, json: bool) {
    if json {
        let line = match update {
            StateUpdate::Snapshot(value) => json!({ "snapshot": value }),
            StateUpdate::Ops { ops, value } => json!({
                "ops": ops.iter().map(Op::to_json).collect::<Vec<_>>(),
                "value": value,
            }),
            StateUpdate::Failed(error) => json!({ "error": error.to_string() }),
        };
        println!("{line}");
        return;
    }
    match update {
        StateUpdate::Snapshot(value) => {
            println!("== snapshot");
            for line in render_view(value) {
                println!("{line}");
            }
            println!("== end of snapshot");
        }
        StateUpdate::Ops { ops, .. } => {
            for op in ops {
                println!("{}", render_op(op));
            }
        }
        StateUpdate::Failed(error) => println!("== error: {error}"),
    }
}

/// The view as text: the conversation, its entries, what runs and what
/// waits.
fn render_view(view: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let conversation = &view["conversation"];
    let agent = &view["docs"]["pi.agent"];
    lines.push(format!(
        "session {} in {} (model {}, thinking {})",
        conversation["id"].as_str().unwrap_or_default(),
        conversation["cwd"].as_str().unwrap_or_default(),
        agent["model"].as_str().unwrap_or("-"),
        agent["thinkingLevel"].as_str().unwrap_or("-"),
    ));
    if let Some(before) = view["before"].as_u64() {
        lines.push(format!("(entries 1-{before} not shown)"));
    }
    for entry in view["entries"].as_array().into_iter().flatten() {
        lines.push(format!("[{}] {}", entry["id"], render_entry(entry)));
    }
    let live = &view["docs"]["pi.live"];
    if let Some(run) = live.get("run") {
        lines.push(format!(
            "live: run {} (inputs {})",
            run["runId"].as_str().unwrap_or_default(),
            run["inputs"].as_array().map_or(0, Vec::len)
        ));
    }
    if let Some(generation) = live.get("generation") {
        let partial = message_text(&generation["message"]);
        lines.push(format!(
            "live: reply attempt {}: {}",
            generation["attempt"],
            if partial.is_empty() {
                "(nothing yet)".to_owned()
            } else {
                format!("partial {:?}", one_line(&partial, 200))
            }
        ));
        if let Some(retry) = generation.get("retry") {
            lines.push(format!(
                "live: retrying: {}",
                retry["error"].as_str().unwrap_or_default()
            ));
        }
    }
    for tool in live["tools"].as_array().into_iter().flatten() {
        let output = tool["output"].as_str().unwrap_or_default();
        lines.push(format!(
            "live: tool {} {} {}{}",
            tool["name"].as_str().unwrap_or_default(),
            tool["callId"].as_str().unwrap_or_default(),
            tool["status"].as_str().unwrap_or_default(),
            if output.is_empty() {
                String::new()
            } else {
                format!(": output {:?}", one_line(output, 200))
            }
        ));
    }
    for compaction in live["compactions"].as_array().into_iter().flatten() {
        lines.push(format!(
            "live: compacting ({})",
            compaction["trigger"].as_str().unwrap_or_default()
        ));
    }
    if let Some(failure) = live.get("compactionFailure") {
        lines.push(format!(
            "live: compaction failed ({}): {}",
            failure["trigger"].as_str().unwrap_or_default(),
            failure["error"].as_str().unwrap_or_default()
        ));
    }
    if let Some(failure) = live.get("runFailure") {
        lines.push(format!(
            "live: last run ended without an answer ({}){}",
            failure["reason"].as_str().unwrap_or_default(),
            failure["detail"]
                .as_str()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        ));
    }
    for item in view["docs"]["pi.inbox"]["items"]
        .as_array()
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "inbox: {} {}: {}",
            item["mode"].as_str().unwrap_or_default(),
            item["id"].as_str().unwrap_or_default(),
            one_line(item["content"].as_str().unwrap_or_default(), 120)
        ));
    }
    if let Some(last) = view.get("lastRun") {
        lines.push(format!(
            "last run: {} {}",
            last["runId"].as_str().unwrap_or_default(),
            last["status"].as_str().unwrap_or_default()
        ));
    }
    lines
}

/// A history entry as a line: its message, or what a marker did.
fn render_entry(record: &Value) -> String {
    match record["kind"].as_str() {
        Some("pi.compaction") => {
            let data = &record["data"];
            return format!(
                "compaction ({}): {} messages summarized, {} kept: {}",
                data["reason"].as_str().unwrap_or_default(),
                data["omittedMessages"],
                data["retainedMessages"],
                one_line(data["summary"].as_str().unwrap_or_default(), 120)
            );
        }
        Some("pi.reset") => return format!("reset to {} messages", record["data"]["messages"]),
        _ => {}
    }
    let entry = record
        .get("model")
        .and_then(|model| model.get(0))
        .unwrap_or(record);
    let role = entry["role"].as_str().unwrap_or_default();
    match role {
        "toolResult" => format!(
            "tool {} ({}){}: {}",
            entry["toolName"].as_str().unwrap_or_default(),
            entry["toolCallId"].as_str().unwrap_or_default(),
            if entry["isError"] == true {
                " error"
            } else {
                ""
            },
            one_line(&message_text(entry), 200)
        ),
        "assistant" => {
            let calls = entry["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "toolCall")
                .map(|block| {
                    format!(
                        " -> {}({})",
                        block["name"].as_str().unwrap_or_default(),
                        block["arguments"]
                    )
                })
                .collect::<String>();
            let stop = match entry["stopReason"].as_str() {
                Some("aborted") => " [aborted]",
                _ => "",
            };
            format!(
                "assistant{stop}: {}{calls}",
                one_line(&message_text(entry), 200)
            )
        }
        role => format!("{role}: {}", one_line(&message_text(entry), 200)),
    }
}

/// The text and thinking of a message's content.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().map(str::to_owned),
                Some("thinking") => block["thinking"]
                    .as_str()
                    .map(|thinking| format!("(thinking: {thinking})")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// An op as a line: its verb, its path, and what it carries.
fn render_op(op: &Op) -> String {
    let path = op
        .path()
        .map(|path| {
            path.iter()
                .map(|seg| match seg {
                    Seg::Key(key) => key.clone(),
                    Seg::Index(index) => index.to_string(),
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();
    let value = |value: &Value| one_line(&value.to_string(), 160);
    match op {
        Op::Replace(value_) => format!("~ replace {}", value(value_)),
        Op::Set(_, value_) => format!("~ set {path} = {}", value(value_)),
        Op::Delete(_) => format!("~ delete {path}"),
        Op::Append(_, text) => format!("~ append {path} += {:?}", one_line(text, 160)),
        Op::Trim(_, count) => format!("~ trim {path} -{count}"),
        Op::Splice(_, index, remove, items) => {
            let mut line = format!("~ splice {path} at {index} remove {remove}");
            if path == "entries" {
                for item in items {
                    line.push_str(&format!("\n~   + {}", render_entry(item)));
                }
            } else if !items.is_empty() {
                line.push_str(&format!(
                    " insert {}",
                    one_line(&Value::Array(items.clone()).to_string(), 160)
                ));
            }
            line
        }
        Op::Permute(_, order) => format!("~ permute {path} {order:?}"),
    }
}

/// `text` on one line, at most `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', "\\n");
    if flat.chars().count() <= limit {
        return flat;
    }
    let kept: String = flat.chars().take(limit).collect();
    format!("{kept}…")
}

/// A selector a command opened, as `noel client command` prints it.
fn describe_selector(selector: &Value) -> String {
    let mut lines = vec![selector["title"].as_str().unwrap_or("selector").to_owned()];
    for item in selector["items"].as_array().into_iter().flatten() {
        let marker = if item["active"] == true { "*" } else { " " };
        lines.push(format!(
            "{marker} {}\t{}",
            item["value"].as_str().unwrap_or_default(),
            item["label"].as_str().unwrap_or_default()
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_is_an_ssh_destination_and_its_program_is_quoted() {
        assert_eq!(Endpoint::new(None, None).unwrap(), Endpoint::Local);
        assert_eq!(
            Endpoint::new(Some("me@box".into()), None).unwrap(),
            Endpoint::Ssh {
                destination: "me@box".into(),
                program: "noel".into()
            }
        );
        for bad in ["", "-oProxyCommand=x", "a b", "a\nb"] {
            assert!(Endpoint::new(Some(bad.into()), None).is_err(), "{bad:?}");
        }
        assert!(Endpoint::new(Some("box".into()), Some(String::new())).is_err());
        assert_eq!(bridge_command("noel"), "'noel' bridge");
        assert_eq!(
            bridge_command("/opt/it's/noel"),
            r"'/opt/it'\''s/noel' bridge"
        );
        assert_eq!(bridge_command("~/bin/noel"), "~/'bin/noel' bridge");
        assert_eq!(bridge_command("~me/noel"), "'~me/noel' bridge");
    }

    #[test]
    fn a_view_renders_its_entries_partial_tools_and_queue() {
        let view = json!({
            "conversation": {"id": "s1", "cwd": "/w"},
            "before": 3,
            "entries": [
                {"id": 4, "kind": "pi.user", "model": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]},
                {"id": 5, "kind": "pi.assistant", "model": [{"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "ls"}}], "stopReason": "toolUse"}]},
                {"id": 6, "kind": "pi.compaction", "head": 6, "data": {"reason": "threshold", "summary": "did things", "omittedMessages": 2, "retainedMessages": 1}},
            ],
            "docs": {
                "pi.agent": {"model": "faux/script", "thinkingLevel": "off", "cwd": "/w"},
                "pi.live": {
                    "run": {"runId": "r1", "inputs": ["r1"]},
                    "generation": {"attempt": 1, "message": {"role": "assistant", "content": [{"type": "text", "text": "Hel"}]}},
                    "tools": [{"callId": "c1", "name": "bash", "status": "running", "output": "a\nb"}],
                },
                "pi.inbox": {"items": [{"id": "q1", "mode": "steer", "content": "faster"}]},
            },
            "lastRun": {"runId": "r1", "status": "running"},
        });
        let lines = render_view(&view);
        assert_eq!(
            lines,
            [
                "session s1 in /w (model faux/script, thinking off)",
                "(entries 1-3 not shown)",
                "[4] user: hi",
                "[5] assistant:  -> bash({\"command\":\"ls\"})",
                "[6] compaction (threshold): 2 messages summarized, 1 kept: did things",
                "live: run r1 (inputs 1)",
                "live: reply attempt 1: partial \"Hel\"",
                "live: tool bash c1 running: output \"a\\\\nb\"",
                "inbox: steer q1: faster",
                "last run: r1 running",
            ]
        );
        let op = Op::Append(
            vec![
                Seg::Key("docs".into()),
                Seg::Key("pi.live".into()),
                Seg::Key("generation".into()),
            ],
            "lo".into(),
        );
        assert_eq!(render_op(&op), "~ append docs/pi.live/generation += \"lo\"");
    }
}

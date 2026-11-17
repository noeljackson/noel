//! `noel mcp`: upstream's MCP server manager (spec §9.7). `list` names the
//! servers of `mcp.json`; `add` and `remove` edit it; `login` and `logout`
//! sign in to and out of an HTTP server with OAuth.

use serde_json::{json, Value};

use super::*;

/// The servers of `mcp.json`, the agent directory's and, for a trusted
/// project, the project's, with the paths they came from. `--approve` and
/// `--no-approve` decide the trust, as the session startup does.
fn configured() -> Result<(
    noel_config::ConfigPaths,
    Vec<noel_config::mcp::McpServer>,
    Vec<String>,
)> {
    let (paths, _) = noel_config::ConfigPaths::discover(std::env::current_dir()?, None)?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trusted = if args.iter().any(|arg| arg == "--approve") {
        true
    } else if args.iter().any(|arg| arg == "--no-approve") {
        false
    } else {
        noel_config::project_is_trusted(&paths.agent_dir, &paths.cwd)?
    };
    let (servers, diagnostics) = noel_config::mcp::load_mcp_servers(
        &paths.agent_dir,
        trusted
            .then(|| noel_config::project_config_dir(&paths.cwd))
            .as_deref(),
    );
    Ok((paths, servers, diagnostics))
}

/// `noel mcp <args>`.
pub(crate) async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") | None => list(),
        Some(command @ ("login" | "logout")) => {
            let Some(name) = args.get(1) else {
                return Err(anyhow!("usage: noel mcp {command} <server>"));
            };
            let (paths, servers, _) = configured()?;
            let Some(server) = servers.iter().find(|server| &server.name == name) else {
                return Err(anyhow!(
                    "no MCP server named {name}; noel mcp list names them"
                ));
            };
            let noel_config::mcp::McpServerConfig::Http { url, .. } = &server.config else {
                return Err(anyhow!(
                    "mcp {name} is a stdio server; {command} is for HTTP servers"
                ));
            };
            if command == "logout" {
                let mut auth = noel_core::read_mcp_auth(&paths.agent_dir);
                match auth.remove(name) {
                    Some(_) => {
                        noel_core::write_mcp_auth(&paths.agent_dir, &auth)
                            .map_err(|error| anyhow!("{error}"))?;
                        println!("signed out of mcp {name}");
                    }
                    None => println!("not signed in to mcp {name}"),
                }
                return Ok(());
            }
            mcp_login(&paths.agent_dir, name, url).await
        }
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some(other) => Err(anyhow!(
            "noel mcp {other} is not a command: list, add, remove, login, logout"
        )),
    }
}

const ADD_USAGE: &str = "usage: noel mcp add <name> (--command <cmd> [args...] | --url <url>) [-l]; put server args that start with - after --";

/// `noel mcp add <name> --command <cmd> [args...]` or `--url <url>`
/// writes the entry to `mcp.json`: the user's, or with `-l` the project's.
fn add(rest: &[String]) -> Result<()> {
    let name = rest
        .first()
        .filter(|name| !name.starts_with('-'))
        .ok_or_else(|| anyhow!(ADD_USAGE))?;
    // Flags are read before a `--`: what follows it is the server's.
    let own = rest
        .iter()
        .position(|arg| arg == "--")
        .map_or(rest, |at| &rest[..at]);
    let local = own.iter().any(|arg| arg == "-l" || arg == "--local");
    let entry = if let Some(at) = own.iter().position(|arg| arg == "--command") {
        // The command and its args: up to the next flag, and everything
        // after a `--` (a server arg that starts with `-`).
        let mut words: Vec<String> = own[at + 1..]
            .iter()
            .take_while(|arg| !arg.starts_with('-'))
            .cloned()
            .collect();
        if let Some(split) = rest.iter().position(|arg| arg == "--") {
            words.extend(rest[split + 1..].iter().cloned());
        }
        if words.is_empty() {
            return Err(anyhow!("mcp add: --command needs a value"));
        }
        let command = words.remove(0);
        println!("adding mcp {name} (stdio: {command})");
        json!({"command": command, "args": words})
    } else if let Some(at) = own.iter().position(|arg| arg == "--url") {
        let url = own
            .get(at + 1)
            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
            .ok_or_else(|| anyhow!("mcp add: --url needs an http or https URL"))?;
        println!("adding mcp {name} (http: {url})");
        json!({"url": url})
    } else {
        return Err(anyhow!(ADD_USAGE));
    };
    let (path, what) = mcp_json(local)?;
    let mut servers = read_servers(&path)?;
    servers.insert(name.clone(), entry);
    write_servers(&path, servers)?;
    println!("added mcp {name} to {what}");
    Ok(())
}

/// `noel mcp remove <name> [-l]`.
fn remove(rest: &[String]) -> Result<()> {
    let name = rest
        .first()
        .filter(|name| !name.starts_with('-'))
        .ok_or_else(|| anyhow!("usage: noel mcp remove <name> [-l]"))?;
    let local = rest.iter().any(|arg| arg == "-l" || arg == "--local");
    let (path, what) = mcp_json(local)?;
    let mut servers = read_servers(&path)?;
    match servers.remove(name) {
        Some(_) => {
            write_servers(&path, servers)?;
            println!("removed mcp {name} from {what}");
        }
        None => println!("no server named {name} in {what}"),
    }
    Ok(())
}

/// The `mcp.json` a command edits: the project's (`-l`) or the user's.
fn mcp_json(local: bool) -> Result<(PathBuf, &'static str)> {
    let (paths, _) = noel_config::ConfigPaths::discover(std::env::current_dir()?, None)?;
    Ok(if local {
        (
            noel_config::project_config_dir(&paths.cwd).join("mcp.json"),
            "the project's mcp.json",
        )
    } else {
        (paths.agent_dir.join("mcp.json"), "the user's mcp.json")
    })
}

/// `mcp.json` whole, as it is: an object, or nothing yet. A file that is
/// there and is not a JSON object is an error, not an empty one: the
/// write that follows would replace what the user wrote by hand.
fn read_mcp_json(path: &Path) -> Result<serde_json::Map<String, Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(anyhow!("{}: {error}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(root)) => Ok(root),
        Ok(_) => Err(anyhow!(
            "{} is not a JSON object; it is left as it is",
            path.display()
        )),
        Err(error) => Err(anyhow!(
            "{} does not parse ({error}); it is left as it is",
            path.display()
        )),
    }
}

/// Its `mcpServers`, `name -> raw entry`, unknown fields kept.
fn read_servers(path: &Path) -> Result<serde_json::Map<String, Value>> {
    match read_mcp_json(path)?.remove("mcpServers") {
        Some(Value::Object(servers)) => Ok(servers),
        None => Ok(Default::default()),
        Some(_) => Err(anyhow!(
            "mcpServers in {} is not an object; it is left as it is",
            path.display()
        )),
    }
}

/// Writes `mcp.json` with `servers`, its other top-level fields kept.
fn write_servers(path: &Path, servers: serde_json::Map<String, Value>) -> Result<()> {
    let mut root = read_mcp_json(path)?;
    root.insert("mcpServers".to_string(), Value::Object(servers));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(root))?;
    noel_config::write_file_atomic(path, format!("{text}\n").as_bytes())?;
    Ok(())
}

fn list() -> Result<()> {
    let (_, servers, diagnostics) = configured()?;
    for diagnostic in diagnostics {
        println!("! {diagnostic}");
    }
    if servers.is_empty() {
        println!("no MCP servers configured; add one to mcp.json (see docs/configuration.md)");
        return Ok(());
    }
    for server in servers {
        match &server.config {
            noel_config::mcp::McpServerConfig::Stdio { command, args, .. } => {
                println!("{}  stdio: {} {}", server.name, command, args.join(" "));
            }
            noel_config::mcp::McpServerConfig::Http { url, .. } => {
                println!("{}  http: {}", server.name, url);
            }
        }
    }
    Ok(())
}

/// `noel mcp login <server>`: discovery, client registration (or the
/// stored client), PKCE authorization with a loopback callback, token
/// exchange, and the tokens stored in `mcp-auth.json` (spec §9.7).
async fn mcp_login(agent_dir: &std::path::Path, name: &str, url: &str) -> Result<()> {
    // A stored client without its redirect URI cannot complete a flow;
    // drop it and register fresh, in a loop rather than by recursion.
    loop {
        match mcp_login_once(agent_dir, name, url).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if error.to_string().contains("register fresh") {
                    let mut auth = noel_core::read_mcp_auth(agent_dir);
                    auth.remove(name);
                    noel_core::write_mcp_auth(agent_dir, &auth)
                        .map_err(|write| anyhow!("{write}"))?;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

async fn mcp_login_once(agent_dir: &std::path::Path, name: &str, url: &str) -> Result<()> {
    let http = reqwest::Client::new();
    println!("discovering OAuth endpoints for {name}...");
    let (resource, metadata, authorization_server) =
        noel_mcp::discover(&http, url, noel_mcp::LATEST_PROTOCOL_VERSION)
            .await
            .map_err(|error| anyhow!("mcp {name}: {error}"))?;
    let Some(metadata) = metadata else {
        return Err(anyhow!(
            "mcp {name}: no OAuth authorization server metadata found at {authorization_server}"
        ));
    };
    println!("authorization server: {}", metadata.issuer);
    let mut auth = noel_core::read_mcp_auth(agent_dir);
    let entry = auth.entry(name.to_string()).or_default();
    let client = match &entry.client {
        Some(client) => client.clone(),
        None => {
            let callback = noel_mcp::CallbackServer::listen(Some(0), "/callback").await?;
            let redirect_url = callback.redirect_url.clone();
            let endpoint = metadata
                .registration_endpoint
                .clone()
                .unwrap_or_else(|| format!("{authorization_server}/register"));
            println!("registering a client at {endpoint}...");
            let client_metadata = noel_mcp::ClientMetadata {
                redirect_uris: vec![redirect_url.clone()],
                client_name: Some("noel".to_string()),
                scope: None,
            };
            let client = noel_mcp::register_client(&http, &endpoint, &client_metadata)
                .await
                .map_err(|error| anyhow!("mcp {name}: {error}"))?;
            entry.client = Some(client.clone());
            entry.redirect_url = Some(redirect_url.clone());
            // Registration survives a failed sign-in.
            noel_core::write_mcp_auth(agent_dir, &auth).map_err(|error| anyhow!("{error}"))?;
            // A fresh callback bound for the registration's redirect port.
            drop(callback);
            let port = redirect_url
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok());
            let callback = noel_mcp::CallbackServer::listen(port, "/callback").await?;
            sign_in(
                &http,
                agent_dir,
                &mut auth,
                name,
                &metadata,
                resource.resource.as_str(),
                client,
                callback,
            )
            .await?;
            return Ok(());
        }
    };
    let redirect_url = entry.redirect_url.clone().unwrap_or_else(|| {
        // A stored client without its redirect URI: a fresh loopback.
        String::new()
    });
    if redirect_url.is_empty() {
        return Err(anyhow!("stale stored client: register fresh"));
    }
    let port = redirect_url
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok());
    let callback = noel_mcp::CallbackServer::listen(port, "/callback").await?;
    sign_in(
        &http,
        agent_dir,
        &mut auth,
        name,
        &metadata,
        resource.resource.as_str(),
        client,
        callback,
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn sign_in(
    http: &reqwest::Client,
    agent_dir: &std::path::Path,
    auth: &mut std::collections::BTreeMap<String, noel_core::McpAuthEntry>,
    name: &str,
    metadata: &noel_mcp::AuthorizationServerMetadata,
    resource: &str,
    client: noel_mcp::ClientInformation,
    callback: noel_mcp::CallbackServer,
) -> Result<()> {
    let state = noel_mcp::random_state();
    let (verifier, _challenge) = noel_mcp::pkce();
    let url = noel_mcp::authorization_url(
        &metadata.authorization_endpoint,
        &client,
        &callback.redirect_url,
        None,
        Some(&state),
        Some(resource),
        &verifier,
    );
    println!("opening the browser to sign in...");
    println!("  {url}");
    crate::open_browser(&url);
    let code = callback
        .wait_for_callback(std::time::Duration::from_secs(300), &state)
        .await
        .map_err(|error| anyhow!("mcp {name}: {error}"))?;
    let tokens = noel_mcp::exchange_code(
        http,
        &metadata.token_endpoint,
        &client,
        &code,
        &verifier,
        &callback.redirect_url,
    )
    .await
    .map_err(|error| anyhow!("mcp {name}: {error}"))?;
    let entry = auth.entry(name.to_string()).or_default();
    entry.tokens = Some(tokens);
    entry.redirect_url = Some(callback.redirect_url.clone());
    noel_core::write_mcp_auth(agent_dir, auth).map_err(|error| anyhow!("{error}"))?;
    println!("signed in to mcp {name}");
    Ok(())
}

//! `noel mcp`: upstream's MCP server manager (spec §9.7). `list` names the
//! servers of `mcp.json`; `login` and `logout` sign in to and out of an
//! HTTP server with OAuth.

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
        Some(other) => Err(anyhow!(
            "noel mcp {other} is not available (this port ships `list`, `login` and `logout`)"
        )),
    }
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

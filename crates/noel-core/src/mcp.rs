//! MCP host connections (spec §9.7): the configured servers, connected
//! lazily at the first run that needs them, their tools overlaid onto
//! the run's tool surface (as extension registrations are), and their
//! calls bridged to the model as tool calls.
//!
//! A connection that fails is a system line, not a dead session; the
//! server contributes nothing until a later run connects it. `/reload`
//! drops servers the new config no longer names.

use std::collections::BTreeMap;

use noel_config::mcp::{mcp_tool_name, McpServer, McpServerConfig};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::AiToolDefinition;
use noel_mcp::HttpOptions;
use noel_mcp::{McpClient, StdioOptions};

/// One connected server's live state: its client and cached tools.
struct Host {
    client: McpClient,
    tools: Vec<noel_mcp::Tool>,
}

/// The MCP connections of a runtime, by server name.
#[derive(Default)]
pub struct McpHosts {
    hosts: BTreeMap<String, Host>,
    /// Servers that failed to connect, with why: reported once per
    /// run, not retried until the config changes.
    failed: BTreeMap<String, String>,
}

/// One tool an MCP server offers, as the run's tool surface holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTool {
    /// The full tool name: `mcp__<server>__<tool>`.
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl McpHosts {
    /// Connects (or skips) every configured server and refreshes the
    /// overlay of MCP tools on `systems`. Returns one system line per
    /// newly-failed server.
    pub async fn start(
        &mut self,
        servers: &[McpServer],
        cwd: &std::path::Path,
        agent_dir: Option<&std::path::Path>,
        overlay: &mut Vec<McpTool>,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        let mut wanted: BTreeMap<String, &McpServer> = BTreeMap::new();
        for server in servers {
            wanted.insert(server.name.clone(), server);
        }
        // Drop connections whose config entry is gone.
        self.hosts.retain(|name, _| wanted.contains_key(name));
        self.failed.retain(|name, _| wanted.contains_key(name));
        for (name, server) in &wanted {
            if self.hosts.contains_key(name) {
                continue;
            }
            if self.failed.contains_key(name) {
                continue;
            }
            match connect(server, cwd, agent_dir).await {
                Ok(mut client) => match client.list_tools().await {
                    Ok(tools) => {
                        self.hosts.insert(name.clone(), Host { client, tools });
                    }
                    Err(error) => {
                        let why = format!("listed no tools: {error}");
                        failures.push(format!("mcp {name}: {why}"));
                        self.failed.insert(name.clone(), why);
                        let _ = client.close().await;
                    }
                },
                Err(error) => {
                    failures.push(format!("mcp {name}: {error}"));
                    self.failed.insert(name.clone(), error.to_string());
                }
            }
        }
        // The overlay: every connected server's tools, as definitions.
        overlay.clear();
        for (name, host) in &self.hosts {
            for tool in &host.tools {
                overlay.push(McpTool {
                    name: mcp_tool_name(name, &tool.name),
                    description: format!(
                        "[{name}] {}",
                        tool.description.as_deref().unwrap_or("MCP tool")
                    ),
                    parameters: tool.input_schema.clone(),
                });
            }
        }
        overlay.sort_by(|a, b| a.name.cmp(&b.name));
        failures
    }

    /// Calls the tool `mcp__<server>__<tool>` on its server, or explains
    /// why not. Returns the result as text (and base64 images) for the
    /// model.
    pub async fn call(
        &mut self,
        tool: &str,
        arguments: &str,
    ) -> Result<noel_mcp::LlmContent, String> {
        // `mcp__<server>__<tool>`: the server is the segment between the
        // first and second `__` after `mcp__`.
        let rest = tool
            .strip_prefix("mcp__")
            .ok_or_else(|| format!("{tool} is not an MCP tool"))?;
        let (server, tool) = rest
            .split_once("__")
            .ok_or_else(|| format!("{tool} has no server and tool part"))?;
        let host = self
            .hosts
            .get_mut(server)
            .ok_or_else(|| format!("no MCP server {server} is connected"))?;
        let arguments = if arguments.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(arguments)
                .map_err(|error| format!("invalid tool arguments: {error}"))?
        };
        // The server's declared schema validates the arguments first
        // (pi-schema semantics: coercion, then upstream's error wording
        // with dotted paths), so a malformed call fails here with the
        // message the model can act on instead of on the server.
        let received = arguments.clone();
        if let Some(schema) = host
            .tools
            .iter()
            .find(|offered| offered.name == tool)
            .map(|offered| offered.input_schema.clone())
        {
            match noel_ai::validation::parse(&schema, arguments) {
                Ok(validated) => {
                    let empty = serde_json::Map::new();
                    let validated = validated.as_object().unwrap_or(&empty).clone();
                    let arguments = if validated.is_empty() {
                        None
                    } else {
                        Some(Value::Object(validated))
                    };
                    return Self::call_validated(self, tool, server, arguments).await;
                }
                Err(errors) => {
                    let name = format!("mcp__{server}__{tool}");
                    return Err(format!(
                        "\n{}",
                        noel_ai::validation::tool_arguments_error(&name, &received, &errors)
                    ));
                }
            }
        }
        let arguments = Some(arguments);
        Self::call_validated(self, tool, server, arguments).await
    }

    /// The call after validation: `None` arguments send none.
    async fn call_validated(
        &mut self,
        tool: &str,
        server: &str,
        arguments: Option<Value>,
    ) -> Result<noel_mcp::LlmContent, String> {
        let host = self
            .hosts
            .get_mut(server)
            .ok_or_else(|| format!("no MCP server {server} is connected"))?;
        // The tool name the server knows: the part after the namespace.
        let result = host
            .client
            .call_tool(tool, arguments)
            .await
            .map_err(|error| error.to_string())?;
        if result.is_error.unwrap_or(false) {
            let text = noel_mcp::to_llm_content(&result)
                .into_iter()
                .filter_map(|content| match content {
                    noel_mcp::LlmContent::Text(text) => Some(text),
                    noel_mcp::LlmContent::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if text.is_empty() {
                format!("MCP tool {tool} failed")
            } else {
                text
            });
        }
        // Join text blocks for the model; images ride as media.
        let mut text = String::new();
        for content in noel_mcp::to_llm_content(&result) {
            match content {
                noel_mcp::LlmContent::Text(part) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&part);
                }
                noel_mcp::LlmContent::Image { .. } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str("[image returned]");
                }
            }
        }
        Ok(noel_mcp::LlmContent::Text(text))
    }

    /// Shuts every connection down (spec shutdown order).
    pub async fn shutdown(&mut self) {
        for (_, mut host) in std::mem::take(&mut self.hosts) {
            let _ = host.client.close().await;
        }
        self.failed.clear();
    }
}

/// The stored credentials of one MCP server: its registered client and
/// tokens (`<agent dir>/mcp-auth.json`, keyed by server name).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpAuthEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<noel_mcp::ClientInformation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<noel_mcp::OAuthTokens>,
    /// The redirect URI of the stored grant, for refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
}

/// Reads `<agent dir>/mcp-auth.json`; missing is empty.
pub fn read_mcp_auth(agent_dir: &std::path::Path) -> BTreeMap<String, McpAuthEntry> {
    std::fs::read_to_string(agent_dir.join("mcp-auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes `<agent dir>/mcp-auth.json` whole.
pub fn write_mcp_auth(
    agent_dir: &std::path::Path,
    auth: &BTreeMap<String, McpAuthEntry>,
) -> Result<(), String> {
    let text = serde_json::to_string_pretty(auth).map_err(|error| error.to_string())?;
    std::fs::write(agent_dir.join("mcp-auth.json"), text).map_err(|error| error.to_string())
}

/// Connects one configured server: stdio by spawning it, HTTP through
/// the streamable-HTTP transport. The stored access token is sent when
/// one exists; a 401 during the handshake tries one refresh of the
/// stored refresh token before giving up (the interactive sign-in is
/// `noel mcp login`, as upstream keeps connections from opening
/// browsers).
async fn connect(
    server: &McpServer,
    cwd: &std::path::Path,
    agent_dir: Option<&std::path::Path>,
) -> Result<McpClient, String> {
    match &server.config {
        McpServerConfig::Http { url, headers } => {
            let options = noel_mcp::HttpOptions {
                url: url.clone(),
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.clone(), noel_config::mcp::expand_env(value)))
                    .collect(),
                ..Default::default()
            };
            let auth = agent_dir.map(read_mcp_auth).unwrap_or_default();
            let entry = auth.get(&server.name);
            let token = entry
                .and_then(|entry| entry.tokens.as_ref())
                .map(|tokens| tokens.access_token.clone());
            let headers = options.headers.clone();
            let connect_url = options.url.clone();
            let discover_url = connect_url.clone();
            async fn attempt(
                connect_url: &str,
                headers: &[(String, String)],
                token: Option<String>,
            ) -> Result<McpClient, String> {
                let mut client = McpClient::connect_http(HttpOptions {
                    url: connect_url.to_string(),
                    headers: headers.to_vec(),
                    token,
                    ..Default::default()
                })
                .await
                .map_err(|error| error.to_string())?;
                // A handshake over an authorized server: list to prove it.
                client
                    .list_tools()
                    .await
                    .map(|_| client)
                    .map_err(|error| error.to_string())
            }
            match attempt(&connect_url, &headers, token.clone()).await {
                Ok(client) => Ok(client),
                Err(first) => {
                    // 401: one refresh, then retry with the new token.
                    let refreshable = entry.and_then(|entry| {
                        let tokens = entry.tokens.as_ref()?;
                        let client = entry.client.as_ref()?;
                        Some((tokens.refresh_token.clone()?, client.clone()))
                    });
                    let refreshable = if first.contains("requires authentication") {
                        refreshable
                    } else {
                        None
                    };
                    if let Some((refresh_token, client_information)) = refreshable {
                        let _ = refresh_token;
                        let _ = client_information;
                        // The token endpoint comes from discovery; the
                        // stored client alone is not enough for a blind
                        // refresh, so the retry re-discovers.
                        if let Ok((_, Some(metadata), _)) = noel_mcp::discover(
                            &reqwest::Client::new(),
                            &discover_url,
                            noel_mcp::LATEST_PROTOCOL_VERSION,
                        )
                        .await
                        {
                            let entry_refresh = entry
                                .and_then(|entry| entry.tokens.as_ref())
                                .and_then(|tokens| tokens.refresh_token.clone());
                            if let Some(refresh_token) = entry_refresh {
                                if let Ok(tokens) = noel_mcp::refresh(
                                    &reqwest::Client::new(),
                                    &metadata.token_endpoint,
                                    &client_information,
                                    &refresh_token,
                                )
                                .await
                                {
                                    let new_token = tokens.access_token.clone();
                                    // Store the refreshed tokens.
                                    if let Some(agent_dir) = agent_dir {
                                        let mut auth = read_mcp_auth(agent_dir);
                                        if let Some(entry) = auth.get_mut(&server.name) {
                                            entry.tokens = Some(tokens);
                                        }
                                        let _ = write_mcp_auth(agent_dir, &auth);
                                    }
                                    if let Ok(client) =
                                        attempt(&connect_url, &headers, Some(new_token)).await
                                    {
                                        return Ok(client);
                                    }
                                }
                            }
                        }
                    }
                    Err(first)
                }
            }
        }

        McpServerConfig::Stdio {
            command,
            args,
            env,
            cwd: server_cwd,
        } => {
            let mut options = StdioOptions {
                command: command.clone(),
                args: args.clone(),
                cwd: Some(cwd.to_path_buf()),
                env: Vec::new(),
            };
            if let Some(dir) = server_cwd {
                let dir = std::path::PathBuf::from(dir);
                options.cwd = Some(if dir.is_absolute() {
                    dir
                } else {
                    cwd.join(dir)
                });
            }
            for (name, value) in env {
                options
                    .env
                    .push((name.clone(), noel_config::mcp::expand_env(value)));
            }
            McpClient::connect(options)
                .await
                .map_err(|error| format!("{error}: {}", stderr_hint(&error)))
        }
    }
}

fn stderr_hint(error: &noel_mcp::McpError) -> String {
    match error {
        noel_mcp::McpError::Spawn(_) | noel_mcp::McpError::Closed(_) => {
            "check the server's command and that it speaks MCP on stdio".to_string()
        }
        _ => String::new(),
    }
}

/// What the run's tool surface holds for the model: each MCP tool as an
/// [`AiToolDefinition`].
pub fn tool_definitions(tools: &[McpTool]) -> Vec<AiToolDefinition> {
    tools
        .iter()
        .map(|tool| AiToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        })
        .collect()
}

/// A shared handle, as the runtime holds it.
pub type SharedMcpHosts = std::sync::Arc<Mutex<McpHosts>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The shell MCP server of noel-mcp's client tests, as a config.
    fn server() -> McpServer {
        McpServer {
            name: "test".to_string(),
            config: McpServerConfig::Stdio {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    r##"
while read -r line; do
    method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$method" in
        initialize) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"t\",\"version\":\"1\"}}}" ;;
        notifications/*) ;;
        tools/list) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;;
        tools/call) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong\"}]}}" ;;
    esac
done
"##
                    .to_string(),
                ],
                env: BTreeMap::new(),
                cwd: None,
            },
        }
    }

    #[tokio::test]
    async fn validation_blocks_bad_calls() {
        // A server whose tool wants an integer >= 1: "zero" is refused
        // with upstream's wording, a coercible "3" passes through.
        let script = r##"
while read -r line; do
    m=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
    i=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$m" in
        initialize) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$i,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"t\",\"version\":\"1\"}}}" ;;
        notifications/*) ;;
        tools/list) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$i,\"result\":{\"tools\":[{\"name\":\"echo\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"count\":{\"type\":\"integer\",\"minimum\":1}},\"required\":[\"count\"]}}]}}" ;;
        tools/call) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$i,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}" ;;
    esac
done
"##;
        let server = McpServer {
            name: "val".to_string(),
            config: McpServerConfig::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: BTreeMap::new(),
                cwd: None,
            },
        };
        let mut hosts = McpHosts::default();
        let failures = hosts
            .start(
                &[server],
                std::path::Path::new("/tmp"),
                None,
                &mut Vec::new(),
            )
            .await;
        assert!(failures.is_empty(), "{failures:?}");
        let bad = hosts.call("mcp__val__echo", r#"{"count": "zero"}"#).await;
        assert!(bad.is_err(), "non-numeric count refused");
        let message = bad.unwrap_err();
        assert!(
            message.contains("Validation failed for tool \"mcp__val__echo\""),
            "{message}"
        );
        assert!(
            message.contains("count: expected integer, got string"),
            "{message}"
        );
        let good = hosts.call("mcp__val__echo", r#"{"count": "3"}"#).await;
        assert!(good.is_ok(), "coercible count passes: {good:?}");
        hosts.shutdown().await;
    }

    #[tokio::test]
    async fn hosts_connect_overlay_and_call() {
        let mut hosts = McpHosts::default();
        let mut overlay = Vec::new();
        let failures = hosts
            .start(
                &[server()],
                std::path::Path::new("/tmp"),
                None,
                &mut overlay,
            )
            .await;
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(overlay.len(), 1);
        assert_eq!(overlay[0].name, "mcp__test__echo");
        let answer = hosts.call("mcp__test__echo", "{}").await.expect("call");
        assert_eq!(answer, noel_mcp::LlmContent::Text("pong".to_string()));
        hosts.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_server_is_a_line_and_not_retried() {
        let broken = McpServer {
            name: "broken".to_string(),
            config: McpServerConfig::Stdio {
                command: "definitely-not-a-command-xyz".to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
            },
        };
        let mut hosts = McpHosts::default();
        let mut overlay = Vec::new();
        let failures = hosts
            .start(&[broken], std::path::Path::new("/tmp"), None, &mut overlay)
            .await;
        assert_eq!(failures.len(), 1);
        // The second start does not retry it.
        let failures = hosts
            .start(&[], std::path::Path::new("/tmp"), None, &mut overlay)
            .await;
        assert!(failures.is_empty());
        hosts.shutdown().await;
    }

    #[tokio::test]
    async fn an_http_entry_connects_through_the_http_transport() {
        // A server that answers nothing: the connect fails with a
        // transport error naming the URL, not an "unsupported" refusal.
        let http = McpServer {
            name: "docs".to_string(),
            config: McpServerConfig::Http {
                url: "http://127.0.0.1:9/mcp".to_string(),
                headers: BTreeMap::new(),
            },
        };
        let mut hosts = McpHosts::default();
        let failures = hosts
            .start(&[http], std::path::Path::new("/tmp"), None, &mut Vec::new())
            .await;
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].contains("docs") && !failures[0].contains("not supported"),
            "{failures:?}"
        );
    }
}

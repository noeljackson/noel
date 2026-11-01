//! MCP server configuration, as upstream's
//! `packages/coding-agent/src/core/mcp-servers.ts` and the MCP
//! extension's `config.ts`: the `mcpServers` shape shared by other MCP
//! clients, read from `mcp.json` in the agent directory and, for trusted
//! projects, `<project>/.noel/mcp.json`. Project entries replace global
//! entries of the same name; a project entry without `command` or `url`
//! overrides only `enabled` of the global entry with that name (so a
//! repository can turn a server off without holding its credentials).
//!
//! Slice 1 holds stdio servers; HTTP entries parse and are reported as
//! unsupported until the HTTP transport is ported.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// A validated server entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub config: McpServerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        /// Values may reference environment variables (`${NAME}`).
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    /// Parsed and named, but not connectable until the HTTP transport
    /// is ported.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl McpServerConfig {
    /// Slice 1 holds no `enabled: false` in the config itself (the
    /// override-only rule removes such servers at load), so every
    /// surviving server is enabled.
    fn disabled(&self) -> bool {
        false
    }
}

/// A raw entry of `mcpServers`, before validation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    cwd: Option<String>,
    url: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    enabled: Option<bool>,
}

/// One file's `mcpServers` object.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, RawServer>,
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by
/// `_`, like the tool names (upstream `mcpNamespace`).
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// The full tool name: `mcp__<server>__<tool>`.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("{}__{}", mcp_namespace(server), tool)
}

/// Loads the servers of `mcp.json` files: the agent dir's, then the
/// project's (a trusted project's `.noel/mcp.json`), project entries
/// replacing global ones of the same name and `enabled: false` entries
/// overriding without their credentials. Returns the servers and one
/// diagnostic per invalid entry.
pub fn load_mcp_servers(
    agent_dir: &Path,
    project_config_dir: Option<&Path>,
) -> (Vec<McpServer>, Vec<String>) {
    let mut servers: BTreeMap<String, McpServer> = BTreeMap::new();
    let mut diagnostics = Vec::new();
    // The user's (agent dir) file first, then the project's: project
    // entries replace global ones of the same name.
    for (index, path) in [
        Some(agent_dir.join("mcp.json")),
        project_config_dir.map(|dir| dir.join("mcp.json")),
    ]
    .into_iter()
    .flatten()
    .enumerate()
    {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file: McpFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        for (name, raw) in file.mcp_servers {
            match validate(name.as_str(), &raw) {
                Ok(config) => {
                    servers.insert(name.clone(), McpServer { name, config });
                }
                Err(error) => {
                    // A project entry with neither `command` nor `url`
                    // (and nothing invalid otherwise) only overrides the
                    // global entry of that name — turning it off, usually
                    // — keeping its credentials the project could not
                    // set. An override of a name the global file never
                    // had, or a disabled one, is nothing.
                    if index > 0 && is_override_only(&raw) {
                        if let Some(existing) = servers.get(&name)
                            && (raw.enabled == Some(false) || existing.config.disabled())
                        {
                            servers.remove(&name);
                        }
                        continue;
                    }
                    diagnostics.push(format!("{}: {error}", path.display()))
                }
            }
        }
    }
    (servers.into_values().collect(), diagnostics)
}

/// A project entry that carries only `enabled` (upstream's
/// override-only rule): neither `command` nor `url`, and nothing else
/// that would make it a server.
fn is_override_only(raw: &RawServer) -> bool {
    raw.command.is_none()
        && raw.url.is_none()
        && raw.args.is_none()
        && raw.env.is_none()
        && raw.cwd.is_none()
        && raw.headers.is_none()
        && raw.kind.is_none()
        && raw.enabled.is_some()
}

/// Validates one entry (upstream `validateMcpServerConfig`, the parts of
/// the shape this port reads).
fn validate(name: &str, raw: &RawServer) -> Result<McpServerConfig, String> {
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if !valid_name {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    if raw.kind.as_deref() == Some("sse") {
        return Err(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    if let Some(command) = &raw.command {
        if raw.url.is_some() {
            return Err(format!(
                "server \"{name}\" has both \"command\" and \"url\"; pick one"
            ));
        }
        return Ok(McpServerConfig::Stdio {
            command: command.clone(),
            args: raw.args.clone().unwrap_or_default(),
            env: raw.env.clone().unwrap_or_default(),
            cwd: raw.cwd.clone(),
        });
    }
    if let Some(url) = &raw.url
        && raw
            .kind
            .as_deref()
            .is_none_or(|kind| kind == "http" || kind == "streamable-http")
    {
        return Ok(McpServerConfig::Http {
            url: url.clone(),
            headers: raw.headers.clone().unwrap_or_default(),
        });
    }
    Err(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

/// Expands `${NAME}` in a header or env value from the environment
/// (upstream's rule: values may reference environment variables).
pub fn expand_env(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                output.push_str(&std::env::var(name).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                output.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &std::path::PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn names_and_tool_names() {
        assert_eq!(mcp_namespace("internal-tools"), "mcp__internal_tools");
        assert_eq!(
            mcp_tool_name("internal-tools", "search"),
            "mcp__internal_tools__search"
        );
    }

    #[test]
    fn global_then_project_merge() {
        let root =
            std::env::temp_dir().join(format!("noel-mcp-cfg-merge-{}", crate::unique_test_id()));
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join("agent/mcp.json"),
            r#"{"mcpServers": {
                "filesystem": {"command": "npx", "args": ["-y", "fs-server", "."]},
                "off-in-project": {"command": "helper"}
            }}"#,
        );
        write(
            &root.join("project/.noel/mcp.json"),
            r#"{"mcpServers": {
                "filesystem": {"command": "other", "args": ["--fast"]},
                "off-in-project": {"enabled": false}
            }}"#,
        );
        let (servers, diagnostics) =
            load_mcp_servers(&root.join("agent"), Some(&root.join("project/.noel")));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // The project's filesystem replaced the global one.
        let filesystem = servers
            .iter()
            .find(|server| server.name == "filesystem")
            .unwrap();
        match &filesystem.config {
            McpServerConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "other");
                assert_eq!(args, &["--fast".to_string()]);
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invalid_entries_are_diagnostics() {
        let root = std::env::temp_dir().join(format!("noel-mcp-bad-{}", crate::unique_test_id()));
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join("agent/mcp.json"),
            r#"{"mcpServers": {
                "bad name": {"command": "x"},
                "nothing": {},
                "legacy": {"type": "sse", "url": "https://x"}
            }}"#,
        );
        let (servers, diagnostics) = load_mcp_servers(&root.join("agent"), None);
        assert!(servers.is_empty());
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn http_entries_parse() {
        let value = serde_json::json!({"url": "https://example.com/mcp"});
        let raw: RawServer = serde_json::from_value(value).unwrap();
        let config = validate("docs", &raw).unwrap();
        match config {
            McpServerConfig::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert!(headers.is_empty());
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn env_expansion_in_values() {
        // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
        unsafe { std::env::set_var("MCP_TEST_TOKEN", "secret") };
        assert_eq!(expand_env("Bearer ${MCP_TEST_TOKEN}"), "Bearer secret");
        assert_eq!(expand_env("no refs"), "no refs");
        assert_eq!(expand_env("${UNSET_VAR_XYZ}"), "");
        // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
        unsafe { std::env::remove_var("MCP_TEST_TOKEN") };
    }
}

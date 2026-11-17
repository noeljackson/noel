//! A noel extension written in Rust: a persistent protocol-v2 host.
//!
//! Copy this file into `~/.noel/extensions/` (or a project's
//! `.noel/extensions/`), build it, and write the manifest beside it:
//!
//! ```sh
//! rustc -O examples/extensions/rust/host.rs -o ~/.noel/extensions/rust-host
//! echo '{"protocolVersion": 2}' > ~/.noel/extensions/rust-host.noel-extension.json
//! ```
//!
//! The contract is one JSON request per line on stdin, one JSON response
//! per line on stdout; notifications (reload, shutdown, agentSettled) are
//! fire-and-forget and take NO response line. Tools, commands, and the
//! tool gate are the carried surface:
//!
//! - `init` answers the registrations (tools, commands, toolGate).
//! - `tool` runs a tool; `command` runs a `/command`.
//! - `toolCall` is the gate: `{allow, reason}`, and `allow: false` blocks
//!   the call with `reason` shown to the model.
//!
//! This example registers a `word_count` tool, a `/stats` command, and a
//! gate that blocks writes under `/etc`.
//!
//! Cargo users: this file is deliberately dependency-free (std only), so
//! `rustc` alone can build it. A real extension would use serde_json.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut writes = 0u64;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Requests are read by field, not parsed whole: the input string
        // can hold anything, including quotes the extension cares about.
        let kind = json_string_field(line, "kind").unwrap_or_default();
        let command = json_string_field(line, "command").unwrap_or_default();
        let input = json_string_field(line, "input").unwrap_or_default();
        let response = match kind.as_str() {
            "init" => {
                // The handshake: what this extension registers.
                r#"{"output":"ready","tools":[{"name":"word_count","description":"Count words in text.","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}],"commands":[{"name":"stats","description":"Show host statistics"}],"toolGate":true}"#.to_string()
            }
            "tool" if command == "word_count" => {
                writes += 1;
                let text = json_string_field(line, "input")
                    .and_then(|input| json_string_field(&input, "text"))
                    .unwrap_or_default();
                format!(
                    r#"{{"output":"{} words"}}"#,
                    text.split_whitespace().count()
                )
            }
            "tool" => format!(r#"{{"error":"no tool {command}"}}"#),
            "command" if command == "stats" => {
                format!(r#"{{"output":"word_count has run {writes} time(s); the host is one process, state kept between calls"}}"#)
            }
            "command" => format!(r#"{{"error":"no command /{command}"}}"#),
            "toolCall" => {
                // The gate: block writes under /etc, allow the rest.
                if command == "write" && input.contains("\"/etc/") {
                    r#"{"allow":false,"reason":"/etc is protected by rust-host"}"#.to_string()
                } else {
                    r#"{"allow":true}"#.to_string()
                }
            }
            // Notifications (reload, shutdown, agentSettled, sessionStart)
            // take no response; unknown kinds stay silent too.
            _ => continue,
        };
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

/// The string value of `"field":"value"` in `json`, unescaping the two
/// escapes a request line can carry (`\"`, `\\`). Dependency-free reading
/// of the few fields the protocol needs.
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut value = String::new();
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            value.push(match ch {
                'n' => '\n',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(value);
        } else {
            value.push(ch);
        }
    }
    None
}

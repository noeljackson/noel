mod web;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolRequest {
    Read {
        path: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    Bash {
        command: String,
        timeout_ms: Option<u64>,
    },
    Edit {
        path: String,
        find: String,
        replace: String,
    },
    Write {
        path: String,
        content: String,
    },
    Grep {
        path: Option<String>,
        pattern: String,
        literal: Option<bool>,
        ignore_case: Option<bool>,
        context: Option<u64>,
        limit: Option<u64>,
    },
    Find {
        pattern: String,
    },
    Ls {
        path: Option<String>,
    },
    WebSearch {
        query: String,
        limit: Option<u64>,
    },
    WebFetch {
        url: String,
        max_chars: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("web tool: {0}")]
    Web(String),
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("path escapes cwd: {0}")]
    PathEscapesCwd(String),
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("edit target was not found in {0}")]
    EditTargetNotFound(String),
    #[error("command timed out after {0} ms")]
    Timeout(u64),
}

#[derive(Debug, Clone, Default)]
pub struct ToolRuntimeOptions {
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub offline: bool,
}

pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    [
        "read",
        "bash",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
        "todo",
        "web_search",
        "web_fetch",
    ]
    .into_iter()
    .map(|name| ToolDefinition {
        name: name.to_string(),
        read_only: matches!(
            name,
            "read" | "grep" | "find" | "ls" | "todo" | "web_search" | "web_fetch"
        ),
    })
    .collect()
}

pub async fn execute_tool(
    cwd: &Path,
    request: ToolRequest,
    options: &ToolRuntimeOptions,
) -> Result<ToolResult, ToolError> {
    if options.offline
        && matches!(
            &request,
            ToolRequest::WebFetch { .. } | ToolRequest::WebSearch { .. }
        )
    {
        return Err(ToolError::Web(
            "network tools are disabled in offline mode".to_string(),
        ));
    }
    match request {
        ToolRequest::Read {
            path,
            offset,
            limit,
        } => read_tool(cwd, &path, offset, limit),
        ToolRequest::Bash {
            command,
            timeout_ms,
        } => bash_tool(cwd, &command, timeout_ms, options).await,
        ToolRequest::Edit {
            path,
            find,
            replace,
        } => edit_tool(cwd, &path, &find, &replace),
        ToolRequest::Write { path, content } => write_tool(cwd, &path, &content),
        ToolRequest::Grep {
            path,
            pattern,
            literal,
            ignore_case,
            context,
            limit,
        } => grep_tool(
            cwd,
            path.as_deref(),
            &pattern,
            literal.unwrap_or(false),
            ignore_case.unwrap_or(false),
            context.unwrap_or(0),
            limit,
        ),
        ToolRequest::Find { pattern } => find_tool(cwd, &pattern),
        ToolRequest::Ls { path } => ls_tool(cwd, path.as_deref()),
        ToolRequest::WebSearch { query, limit } => {
            tokio::time::timeout(Duration::from_secs(30), web::search(&query, limit))
                .await
                .map_err(|_| ToolError::Timeout(30_000))?
        }
        ToolRequest::WebFetch { url, max_chars } => {
            tokio::time::timeout(Duration::from_secs(30), web::fetch(&url, max_chars))
                .await
                .map_err(|_| ToolError::Timeout(30_000))?
        }
    }
}

/// Default caps on tool outputs entering the model context. Large results
/// are truncated with instructions for narrowing the next request instead of
/// silently consuming the whole window (Claude Code: 2,000-line reads, 30k-char
/// bash output; pi keeps the same order of magnitude).
pub const MAX_READ_LINES: usize = 2_000;
pub const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;
pub const MAX_GREP_MATCHES: usize = 200;

/// Truncation notice appended to oversized outputs so the model retries with
/// narrower parameters instead of assuming it saw everything.
fn truncation_notice(omitted: &str, hint: &str) -> String {
    format!("\n[output truncated: {omitted}. {hint}]")
}

fn truncate_chars(text: &str, max: usize, hint: &str) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut end = text.len();
    for (count, (byte, _)) in text.char_indices().enumerate() {
        if count == max {
            end = byte;
            break;
        }
    }
    let mut cut = text[..end].to_string();
    cut.push_str(&truncation_notice(
        &format!("showing first {max} of {} characters", text.chars().count()),
        hint,
    ));
    cut
}

fn read_tool(
    cwd: &Path,
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let path = resolve_under_cwd(cwd, path)?;
    let content = fs::read_to_string(&path).map_err(|source| ToolError::Io {
        path: path.clone(),
        source,
    })?;
    let total_lines = content.lines().count();
    // offset is 1-based to match the schema advertised to models. An offset
    // past EOF yields an empty read rather than silently restarting at 1.
    let start = match offset {
        Some(offset) => (offset.saturating_sub(1) as usize).min(total_lines),
        None => 0,
    };
    let effective_limit = limit.map(|limit| limit as usize).unwrap_or(MAX_READ_LINES);
    let mut output = String::new();
    let mut shown = 0usize;
    for line in content.lines().skip(start).take(effective_limit) {
        output.push_str(line);
        output.push('\n');
        shown += 1;
    }
    // `lines()` drops the file's own trailing newline; preserve the file's
    // ending verbatim when nothing was truncated so unchanged files round-trip.
    let truncated = start + shown < total_lines;
    if !truncated && !content.ends_with('\n') {
        output.pop();
    }
    if truncated {
        output.push_str(&truncation_notice(
            &format!(
                "showing lines {}-{} of {total_lines}",
                start + 1,
                start + shown
            ),
            &format!(
                "read the rest with offset={}, or narrow with grep/limit",
                start + shown + 1
            ),
        ));
    }
    Ok(ToolResult {
        output,
        is_error: false,
    })
}

/// Head-limited, truncation-noted listing for grep/find-style outputs.
fn truncate_matches(matches: &[String], limit: Option<u64>, max: usize, hint: &str) -> String {
    let shown = limit
        .map(|limit| limit as usize)
        .unwrap_or(max)
        .min(max)
        .max(1);
    let mut output = matches
        .iter()
        .take(shown)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if matches.len() > shown {
        output.push_str(&truncation_notice(
            &format!("showing {shown} of {} matches", matches.len()),
            hint,
        ));
    }
    output
}

fn grep_tool(
    cwd: &Path,
    path: Option<&str>,
    pattern: &str,
    literal: bool,
    ignore_case: bool,
    context: u64,
    limit: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let root = match path {
        Some(path) => resolve_under_cwd(cwd, path)?,
        None => cwd.to_path_buf(),
    };
    let pattern = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let matcher = if ignore_case {
        regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
    } else {
        regex::Regex::new(&pattern)
    }
    .map_err(|source| ToolError::Web(format!("invalid regex: {source}")))?;
    let mut matches = Vec::new();
    visit_files(&root, &mut |file| {
        if let Ok(content) = fs::read_to_string(file) {
            let lines: Vec<&str> = content.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if matcher.is_match(line) {
                    let mut formatted =
                        format!("{}:{}:{line}", display_relative(cwd, file), index + 1);
                    // context lines are additive around the match; cap each
                    // line's output so a context-heavy request cannot exceed
                    // the shared output budget either.
                    for delta in 1..=context {
                        if let Some(before) =
                            lines.get(index.checked_sub(delta as usize).unwrap_or(usize::MAX))
                        {
                            formatted =
                                format!("{}-{}:{before}\n", formatted, index + 1 - delta as usize);
                        }
                        if let Some(after) = lines.get(index + delta as usize) {
                            formatted =
                                format!("{}+{}:{after}\n", formatted, index + delta as usize + 1);
                        }
                    }
                    matches.push(formatted);
                }
            }
        }
    })?;
    Ok(ToolResult {
        output: truncate_matches(
            &matches,
            limit,
            MAX_GREP_MATCHES,
            "narrow the path or pattern, or set limit",
        ),
        is_error: false,
    })
}

async fn bash_tool(
    cwd: &Path,
    command: &str,
    timeout_ms: Option<u64>,
    options: &ToolRuntimeOptions,
) -> Result<ToolResult, ToolError> {
    let command = match &options.shell_command_prefix {
        Some(prefix) if !prefix.trim().is_empty() => format!("{prefix}\n{command}"),
        _ => command.to_string(),
    };
    let shell = options
        .shell_path
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let mut process = Command::new(shell);
    process
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Run the shell in its own process group so a timeout can kill the
        // whole tree (shell + children), not just the shell pid. Killing only
        // the shell orphaned children like a long preflight script, which
        // kept running and consuming resources after the tool call failed.
        .process_group(0)
        .kill_on_drop(true);
    let child = process.spawn().map_err(|source| ToolError::Io {
        path: cwd.to_path_buf(),
        source,
    })?;

    let child_pid = child.id().unwrap_or_default();
    let wait = child.wait_with_output();
    let output = if let Some(timeout_ms) = timeout_ms {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await {
            Ok(output) => output.map_err(|source| ToolError::Io {
                path: cwd.to_path_buf(),
                source,
            })?,
            Err(_) => {
                // wait_with_output() consumed the child; kill the process
                // group the shell led (process_group(0) above) so children
                // die with the shell instead of orphaning.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child_pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                return Err(ToolError::Timeout(timeout_ms));
            }
        }
    } else {
        wait.await.map_err(|source| ToolError::Io {
            path: cwd.to_path_buf(),
            source,
        })?
    };

    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        text.push_str(&format!("\nexit status: {}", output.status));
    }
    Ok(ToolResult {
        output: truncate_chars(
            &text,
            MAX_TOOL_OUTPUT_CHARS,
            "redirect large output to a file and read a slice, or filter with grep/head",
        ),
        is_error: !output.status.success(),
    })
}

fn edit_tool(cwd: &Path, path: &str, find: &str, replace: &str) -> Result<ToolResult, ToolError> {
    let path = resolve_under_cwd(cwd, path)?;
    let content = fs::read_to_string(&path).map_err(|source| ToolError::Io {
        path: path.clone(),
        source,
    })?;
    if !content.contains(find) {
        return Err(ToolError::EditTargetNotFound(path.display().to_string()));
    }
    let next = content.replacen(find, replace, 1);
    fs::write(&path, next).map_err(|source| ToolError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(ToolResult {
        is_error: false,
        output: format!("edited {}", path.display()),
    })
}

fn write_tool(cwd: &Path, path: &str, content: &str) -> Result<ToolResult, ToolError> {
    let path = resolve_under_cwd_for_write(cwd, path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ToolError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&path, content).map_err(|source| ToolError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(ToolResult {
        is_error: false,
        output: format!("wrote {}", path.display()),
    })
}

fn find_tool(cwd: &Path, pattern: &str) -> Result<ToolResult, ToolError> {
    let mut matches = Vec::new();
    visit_files(cwd, &mut |file| {
        let relative = display_relative(cwd, file);
        if relative.contains(pattern) {
            matches.push(relative);
        }
    })?;
    Ok(ToolResult {
        is_error: false,
        output: truncate_matches(
            &matches,
            None,
            MAX_GREP_MATCHES,
            "narrow the pattern, or search a subdirectory",
        ),
    })
}

fn ls_tool(cwd: &Path, path: Option<&str>) -> Result<ToolResult, ToolError> {
    let path = match path {
        Some(path) => resolve_under_cwd(cwd, path)?,
        None => cwd.to_path_buf(),
    };
    let mut entries = fs::read_dir(&path)
        .map_err(|source| ToolError::Io {
            path: path.clone(),
            source,
        })?
        .map(|entry| {
            entry.map(|entry| {
                let marker = entry
                    .file_type()
                    .map(|ty| if ty.is_dir() { "/" } else { "" })
                    .unwrap_or("");
                format!("{}{}", entry.file_name().to_string_lossy(), marker)
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ToolError::Io {
            path: path.clone(),
            source,
        })?;
    entries.sort();
    Ok(ToolResult {
        is_error: false,
        output: entries.join("\n"),
    })
}

fn visit_files(root: &Path, visit: &mut dyn FnMut(&Path)) -> Result<(), ToolError> {
    if root.is_file() {
        visit(root);
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|source| ToolError::Io {
        path: root.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ToolError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".git") || name == "target" || name == "node_modules"
        {
            continue;
        }
        if path.is_dir() {
            visit_files(&path, visit)?;
        } else if path.is_file() {
            visit(&path);
        }
    }
    Ok(())
}

fn resolve_under_cwd(cwd: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        cwd.join(input)
    };
    let canonical_cwd = cwd.canonicalize().map_err(|source| ToolError::Io {
        path: cwd.to_path_buf(),
        source,
    })?;

    if let Ok(canonical_candidate) = candidate.canonicalize() {
        if !canonical_candidate.starts_with(&canonical_cwd) {
            return Err(ToolError::PathEscapesCwd(input.to_string()));
        }
        return Ok(canonical_candidate);
    }

    let parent = candidate.parent().unwrap_or(cwd);
    let canonical_parent = parent.canonicalize().map_err(|source| ToolError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    if !canonical_parent.starts_with(&canonical_cwd) {
        return Err(ToolError::PathEscapesCwd(input.to_string()));
    }
    Ok(candidate
        .file_name()
        .map(|file_name| canonical_parent.join(file_name))
        .unwrap_or(canonical_parent))
}

fn resolve_under_cwd_for_write(cwd: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        cwd.join(input)
    };
    let canonical_cwd = cwd.canonicalize().map_err(|source| ToolError::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
    if let Ok(canonical_candidate) = candidate.canonicalize() {
        if !canonical_candidate.starts_with(&canonical_cwd) {
            return Err(ToolError::PathEscapesCwd(input.to_string()));
        }
        return Ok(canonical_candidate);
    }

    let mut existing_parent = candidate.parent().unwrap_or(cwd);
    while !existing_parent.exists() {
        existing_parent = existing_parent.parent().unwrap_or(cwd);
    }
    let canonical_parent = existing_parent
        .canonicalize()
        .map_err(|source| ToolError::Io {
            path: existing_parent.to_path_buf(),
            source,
        })?;
    if !canonical_parent.starts_with(&canonical_cwd) {
        return Err(ToolError::PathEscapesCwd(input.to_string()));
    }
    Ok(candidate)
}

fn display_relative(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn web_tools_are_read_only_and_offline_rejects_network_before_connecting() {
        let definitions = builtin_tool_definitions();
        for name in ["web_search", "web_fetch"] {
            assert!(
                definitions
                    .iter()
                    .find(|tool| tool.name == name)
                    .unwrap()
                    .read_only
            );
        }
        let options = ToolRuntimeOptions {
            offline: true,
            ..Default::default()
        };
        for request in [
            ToolRequest::WebFetch {
                url: "http://127.0.0.1:1/".into(),
                max_chars: None,
            },
            ToolRequest::WebSearch {
                query: "rust".into(),
                limit: None,
            },
        ] {
            assert!(execute_tool(Path::new("."), request, &options)
                .await
                .unwrap_err()
                .to_string()
                .contains("offline"));
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit_is_preserved_as_a_tool_failure() {
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command: "printf failed-output; exit 7".into(),
                timeout_ms: Some(1000),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert!(result.output.contains("failed-output"));
        assert!(result.output.contains("exit status"));
    }

    #[tokio::test]
    async fn bash_timeout_kills_the_whole_process_group() {
        // A timed-out bash call must not orphan its children: the shell runs
        // in its own process group and the timeout kills the group, so a
        // long-running child script dies with the tool call.
        let marker = std::env::temp_dir().join(format!(
            "pi-bash-orphan-{}-{}",
            std::process::id(),
            new_marker_nonce()
        ));
        let marker_str = marker.to_string_lossy().to_string();
        let command = format!("sh -c 'sleep 30; touch {marker_str}' & sleep 30");
        let started = std::time::Instant::now();
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command,
                timeout_ms: Some(750),
            },
            &ToolRuntimeOptions::default(),
        )
        .await;
        assert!(matches!(result, Err(ToolError::Timeout(750))));
        // Give the group kill a moment to land, then verify no orphan lives on
        // to create the marker file later.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let orphaned = tokio::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("touch {marker_str}"))
            .output()
            .await
            .expect("pgrep");
        assert!(
            !orphaned.status.success(),
            "timed-out bash must kill its process group, not just the shell"
        );
        assert!(!marker.exists(), "orphaned child outlived the timeout");
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_file(&marker);
    }

    fn new_marker_nonce() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NONCE: AtomicU64 = AtomicU64::new(0);
        NONCE.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[tokio::test]
    async fn read_caps_lines_honors_offset_and_limit_and_notes_truncation() {
        let cwd = std::env::temp_dir().join(format!("pi-read-cap-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let content = (1..=3_000)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("big.txt"), content).expect("write big file");

        // Default cap: first 2000 lines plus a truncation notice with the
        // exact resume offset.
        let capped = execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "big.txt".to_string(),
                offset: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("capped read");
        assert!(capped.output.contains("line-2000\n"));
        assert!(!capped.output.contains("line-2001\n"));
        assert!(capped.output.contains("showing lines 1-2000 of 3000"));
        assert!(capped.output.contains("offset=2001"));

        // Explicit offset resumes exactly where the notice said.
        let resumed = execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "big.txt".to_string(),
                offset: Some(2001),
                limit: Some(5),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("resumed read");
        assert!(resumed.output.starts_with("line-2001\n"));
        assert!(resumed.output.contains("line-2005\n"));
        assert!(!resumed.output.contains("line-2006"));
        assert!(resumed.output.contains("showing lines 2001-2005 of 3000"));

        // offset past the end yields an empty result, not an error.
        let past = execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "big.txt".to_string(),
                offset: Some(9_999),
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("past-end read");
        assert_eq!(past.output, "");

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn bash_output_is_capped_with_guidance() {
        let cwd = std::env::temp_dir().join(format!("pi-bash-cap-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let result = execute_tool(
            &cwd,
            ToolRequest::Bash {
                command: "seq 1 20000".to_string(),
                timeout_ms: Some(10_000),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("bash");
        assert!(result.output.chars().count() <= 30_000 + 300);
        assert!(result
            .output
            .contains("[output truncated: showing first 30000 of"));
        assert!(result.output.contains("redirect large output"));
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn grep_supports_regex_case_and_context_and_limits_matches() {
        let cwd = std::env::temp_dir().join(format!("pi-grep-opts-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        fs::write(cwd.join("a.txt"), "alpha\nBETA\ngamma\nAlpha\ndelta\n").expect("write a");

        // Regex, case-insensitive.
        let insensitive = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: None,
                pattern: "^al".to_string(),
                literal: None,
                ignore_case: Some(true),
                context: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("case-insensitive grep");
        assert!(insensitive.output.contains("a.txt:1:alpha"));
        assert!(insensitive.output.contains("a.txt:4:Alpha"));

        // Literal escapes regex metacharacters.
        let literal = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: None,
                pattern: "al.*a".to_string(),
                literal: Some(true),
                ignore_case: None,
                context: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("literal grep");
        assert_eq!(literal.output, "");

        // Context lines surround the match.
        let with_context = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: None,
                pattern: "BETA".to_string(),
                literal: None,
                ignore_case: None,
                context: Some(1),
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("context grep");
        assert!(with_context.output.contains("a.txt:2:BETA"));
        assert!(with_context.output.contains("-1:alpha"));
        assert!(with_context.output.contains("+3:gamma"));

        // Match cap with notice.
        fs::write(
            cwd.join("many.txt"),
            (0..500)
                .map(|i| format!("needle-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("write many");
        let capped = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: Some("many.txt".to_string()),
                pattern: "needle-".to_string(),
                literal: Some(true),
                ignore_case: None,
                context: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("capped grep");
        assert!(capped.output.contains("showing 200 of 500 matches"));
        assert!(!capped.output.contains("needle-499"));

        // limit lowers the cap.
        let limited = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: Some("many.txt".to_string()),
                pattern: "needle-".to_string(),
                literal: Some(true),
                ignore_case: None,
                context: None,
                limit: Some(10),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("limited grep");
        assert!(limited.output.contains("needle-9\n"));
        assert!(limited.output.contains("showing 10 of 500 matches"));

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn find_output_is_capped_with_notice() {
        let cwd = std::env::temp_dir().join(format!("pi-find-cap-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        for i in 0..250 {
            fs::write(cwd.join(format!("file-{i:03}.txt")), "x").expect("write file");
        }
        let result = execute_tool(
            &cwd,
            ToolRequest::Find {
                pattern: "file-".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("find");
        assert!(result.output.contains("showing 200 of 250 matches"));
        // file names are returned in directory order; only assert that some
        // files were omitted rather than any specific name.
        assert_eq!(result.output.lines().count(), 201);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn builtin_tools_cover_upstream_ts_names() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/local-tools.json"
        ))
        .expect("parse local tools fixture");
        let upstream_names = fixture["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_string())
            .collect::<Vec<_>>();
        let rust_tools = builtin_tool_definitions();
        let rust_names = rust_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        // Rust intentionally adds tools beyond the upstream set (see
        // docs/rust-rewrite/non-parity-register.md); upstream names must all
        // be covered, but equality is not required.
        for name in &upstream_names {
            assert!(rust_names.contains(name), "missing upstream tool {name}");
        }

        let read_only = rust_tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.read_only))
            .collect::<BTreeMap<_, _>>();
        assert!(read_only["read"]);
        assert!(read_only["grep"]);
        assert!(read_only["find"]);
        assert!(read_only["ls"]);
        assert!(!read_only["bash"]);
        assert!(!read_only["edit"]);
        assert!(!read_only["write"]);
        assert!(read_only["todo"]);

        let grep = fixture["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .find(|tool| tool["name"] == "grep")
            .expect("grep fixture");
        assert!(grep["parameters"]["properties"]
            .as_array()
            .expect("grep properties")
            .iter()
            .any(|property| property == "glob"));
    }

    #[tokio::test]
    async fn read_write_and_edit_tool_round_trip() {
        let cwd = std::env::temp_dir().join(format!("pi-tools-test-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");

        execute_tool(
            &cwd,
            ToolRequest::Write {
                path: "a.txt".to_string(),
                content: "hello world".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("write");
        execute_tool(
            &cwd,
            ToolRequest::Edit {
                path: "a.txt".to_string(),
                find: "world".to_string(),
                replace: "rust".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("edit");
        let result = execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "a.txt".to_string(),
                offset: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("read");

        assert_eq!(result.output, "hello rust");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn grep_find_ls_and_bash_return_text_outputs() {
        let cwd = std::env::temp_dir().join(format!("pi-tools-search-test-{}", std::process::id()));
        fs::create_dir_all(cwd.join("src")).expect("create temp dir");
        fs::write(cwd.join("src/a.txt"), "alpha\nbeta\n").expect("write a");
        fs::write(cwd.join("src/b.txt"), "gamma\n").expect("write b");

        let grep = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: Some("src".to_string()),
                pattern: "alpha".to_string(),
                literal: None,
                ignore_case: None,
                context: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("grep");
        assert!(grep.output.contains("src/a.txt:1:alpha"));

        let find = execute_tool(
            &cwd,
            ToolRequest::Find {
                pattern: "a.txt".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("find");
        assert_eq!(find.output, "src/a.txt");

        let ls = execute_tool(
            &cwd,
            ToolRequest::Ls {
                path: Some("src".to_string()),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("ls");
        assert_eq!(ls.output, "a.txt\nb.txt");

        let bash = execute_tool(
            &cwd,
            ToolRequest::Bash {
                command: "printf tool-ok".to_string(),
                timeout_ms: Some(1000),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("bash");
        assert_eq!(bash.output, "tool-ok");

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn dot_paths_resolve_to_cwd() {
        let cwd =
            std::env::temp_dir().join(format!("pi-tools-dot-path-test-{}", std::process::id()));
        fs::create_dir_all(cwd.join("src")).expect("create temp dir");
        fs::write(cwd.join("src/a.txt"), "alpha\n").expect("write a");

        let ls = execute_tool(
            &cwd,
            ToolRequest::Ls {
                path: Some(".".to_string()),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("ls dot");
        assert!(ls.output.contains("src/"));

        let grep = execute_tool(
            &cwd,
            ToolRequest::Grep {
                path: Some(".".to_string()),
                pattern: "alpha".to_string(),
                literal: None,
                ignore_case: None,
                context: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("grep dot");
        assert_eq!(grep.output, "src/a.txt:1:alpha");

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn write_creates_nested_directories_inside_cwd() {
        let cwd =
            std::env::temp_dir().join(format!("pi-tools-nested-write-test-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");

        execute_tool(
            &cwd,
            ToolRequest::Write {
                path: "nested/child/file.txt".to_string(),
                content: "content".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("write nested file");

        let content =
            fs::read_to_string(cwd.join("nested/child/file.txt")).expect("read nested file");
        assert_eq!(content, "content");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn write_rejects_paths_outside_cwd() {
        let cwd = std::env::temp_dir().join(format!(
            "pi-tools-outside-write-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&cwd).expect("create temp dir");

        let error = execute_tool(
            &cwd,
            ToolRequest::Write {
                path: "../outside.txt".to_string(),
                content: "content".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect_err("outside write should fail");

        assert!(matches!(error, ToolError::PathEscapesCwd(_)));
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn read_rejects_paths_outside_cwd() {
        let cwd =
            std::env::temp_dir().join(format!("pi-tools-outside-read-test-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");

        let error = execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "../outside.txt".to_string(),
                offset: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect_err("outside read should fail");

        assert!(matches!(error, ToolError::PathEscapesCwd(_)));
        let _ = fs::remove_dir_all(cwd);
    }
}

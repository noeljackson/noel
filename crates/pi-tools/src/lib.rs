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
    },
    Find {
        pattern: String,
    },
    Ls {
        path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
}

#[derive(Debug, Error)]
pub enum ToolError {
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
}

pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    ["read", "bash", "edit", "write", "grep", "find", "ls"]
        .into_iter()
        .map(|name| ToolDefinition {
            name: name.to_string(),
            read_only: matches!(name, "read" | "grep" | "find" | "ls"),
        })
        .collect()
}

pub async fn execute_tool(
    cwd: &Path,
    request: ToolRequest,
    options: &ToolRuntimeOptions,
) -> Result<ToolResult, ToolError> {
    match request {
        ToolRequest::Read { path } => read_tool(cwd, &path),
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
        ToolRequest::Grep { path, pattern } => grep_tool(cwd, path.as_deref(), &pattern),
        ToolRequest::Find { pattern } => find_tool(cwd, &pattern),
        ToolRequest::Ls { path } => ls_tool(cwd, path.as_deref()),
    }
}

fn read_tool(cwd: &Path, path: &str) -> Result<ToolResult, ToolError> {
    let path = resolve_under_cwd(cwd, path)?;
    let output = fs::read_to_string(&path).map_err(|source| ToolError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(ToolResult { output })
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
        .kill_on_drop(true);
    let child = process.spawn().map_err(|source| ToolError::Io {
        path: cwd.to_path_buf(),
        source,
    })?;

    let wait = child.wait_with_output();
    let output = if let Some(timeout_ms) = timeout_ms {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await {
            Ok(output) => output.map_err(|source| ToolError::Io {
                path: cwd.to_path_buf(),
                source,
            })?,
            Err(_) => return Err(ToolError::Timeout(timeout_ms)),
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
    Ok(ToolResult { output: text })
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
        output: format!("wrote {}", path.display()),
    })
}

fn grep_tool(cwd: &Path, path: Option<&str>, pattern: &str) -> Result<ToolResult, ToolError> {
    let root = match path {
        Some(path) => resolve_under_cwd(cwd, path)?,
        None => cwd.to_path_buf(),
    };
    let mut matches = Vec::new();
    visit_files(&root, &mut |file| {
        if let Ok(content) = fs::read_to_string(file) {
            for (index, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    matches.push(format!(
                        "{}:{}:{line}",
                        display_relative(cwd, file),
                        index + 1
                    ));
                }
            }
        }
    })?;
    Ok(ToolResult {
        output: matches.join("\n"),
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
        output: matches.join("\n"),
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
    let parent = candidate.parent().unwrap_or(cwd);
    let canonical_parent = parent.canonicalize().map_err(|source| ToolError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let canonical_cwd = cwd.canonicalize().map_err(|source| ToolError::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
    if !canonical_parent.starts_with(&canonical_cwd) {
        return Err(ToolError::PathEscapesCwd(input.to_string()));
    }
    Ok(canonical_parent.join(candidate.file_name().unwrap_or_default()))
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
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("read");

        assert_eq!(result.output, "hello rust");
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
}

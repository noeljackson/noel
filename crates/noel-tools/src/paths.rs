//! Tool path arguments (upstream `path-utils.ts`): any path the user could
//! name is accepted, relative to the working directory, absolute, or under
//! `~`. Paths are not confined to the working directory. The workspace
//! resolves them (`ExecutionEnv::absolute_path`); a tool only normalizes how
//! it was typed first.

use noel_env::{Context, ExecutionEnv};

use crate::ToolError;

/// A tool's path argument as typed (upstream Durable's
/// `normalizeToolPath`): Unicode spaces that editors and terminals paste in
/// place of a space become spaces, and the `@` prompts put before a file is
/// dropped.
pub(crate) fn normalize_tool_path(input: &str) -> String {
    let input = input.replace(
        |character| {
            matches!(
                character,
                '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            )
        },
        " ",
    );
    match input.strip_prefix('@') {
        Some(rest) => rest.to_string(),
        None => input,
    }
}

/// Resolves a tool's path argument in `env`.
pub(crate) async fn resolve_tool_path(
    env: &dyn ExecutionEnv,
    input: &str,
    cx: &Context,
) -> Result<String, ToolError> {
    env.absolute_path(&normalize_tool_path(input), cx)
        .await
        .map_err(|error| ToolError::Failed(error.message))
}

/// Resolves the path of a file to read. When it does not exist, tries the
/// spellings macOS gives screenshots: a narrow no-break space before AM/PM
/// and a curly apostrophe.
pub(crate) async fn resolve_read_path(
    env: &dyn ExecutionEnv,
    input: &str,
    cx: &Context,
) -> Result<String, ToolError> {
    let resolved = resolve_tool_path(env, input, cx).await?;
    // Only a path known to be missing is respelled; one that cannot be
    // inspected is read as it is and fails there.
    if !matches!(env.exists(&resolved, cx).await, Ok(false)) {
        return Ok(resolved);
    }
    let am_pm = ["AM", "PM", "am", "pm", "Am", "Pm", "aM", "pM"]
        .iter()
        .fold(resolved.clone(), |text, half| {
            text.replace(&format!(" {half}."), &format!("\u{202f}{half}."))
        });
    for variant in [am_pm, resolved.replace('\'', "\u{2019}")] {
        if matches!(env.exists(&variant, cx).await, Ok(true)) {
            return Ok(variant);
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use noel_env::{resolve_local_path, LocalEnv};

    use super::*;

    fn resolve(input: &str) -> PathBuf {
        PathBuf::from(resolve_local_path(
            "/work/dir",
            &normalize_tool_path(input),
            Some("/home/user"),
        ))
    }

    #[test]
    fn paths_resolve_like_upstream_without_confinement() {
        assert_eq!(resolve("a.txt"), PathBuf::from("/work/dir/a.txt"));
        assert_eq!(resolve("./src/../a.txt"), PathBuf::from("/work/dir/a.txt"));
        assert_eq!(
            resolve("../other/a.txt"),
            PathBuf::from("/work/other/a.txt")
        );
        assert_eq!(resolve("/etc/hosts"), PathBuf::from("/etc/hosts"));
        assert_eq!(resolve("/a/../../b"), PathBuf::from("/b"));
        assert_eq!(resolve("~"), PathBuf::from("/home/user"));
        assert_eq!(resolve("~/notes.md"), PathBuf::from("/home/user/notes.md"));
        // Only a leading `~/` is the home directory.
        assert_eq!(resolve("~other/x"), PathBuf::from("/work/dir/~other/x"));
        assert_eq!(
            resolve("@src/lib.rs"),
            PathBuf::from("/work/dir/src/lib.rs")
        );
        assert_eq!(resolve("a\u{a0}b.txt"), PathBuf::from("/work/dir/a b.txt"));
        assert_eq!(
            resolve("file:///tmp/with%20space.txt"),
            PathBuf::from("/tmp/with space.txt")
        );
    }

    #[tokio::test]
    async fn read_paths_fall_back_to_macos_screenshot_spellings() {
        let dir = std::env::temp_dir().join(format!("noel-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let narrow = dir.join("Shot at 9.41\u{202f}AM.png");
        let curly = dir.join("Capture d\u{2019}ecran.png");
        std::fs::write(&narrow, "x").unwrap();
        std::fs::write(&curly, "x").unwrap();
        let env = LocalEnv::new(&dir);
        let resolve_read = |input: &'static str| {
            let env = &env;
            async move {
                PathBuf::from(
                    resolve_read_path(env, input, &Context::background())
                        .await
                        .unwrap(),
                )
            }
        };
        assert_eq!(resolve_read("Shot at 9.41 AM.png").await, narrow);
        assert_eq!(resolve_read("Capture d'ecran.png").await, curly);
        assert_eq!(resolve_read("missing.png").await, dir.join("missing.png"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

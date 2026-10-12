//! Tool path arguments (upstream `path-utils.ts`): any path the user could
//! name is accepted, relative to the working directory, absolute, or under
//! `~`. Paths are not confined to the working directory.

use std::path::{Component, Path, PathBuf};

/// Resolves a tool's path argument against `cwd`.
pub(crate) fn resolve_to_cwd(cwd: &Path, input: &str) -> PathBuf {
    resolve_with_home(cwd, input, std::env::home_dir().as_deref())
}

/// Resolves the path of a file to read. When it does not exist, tries the
/// spellings macOS gives screenshots: a narrow no-break space before AM/PM
/// and a curly apostrophe.
pub(crate) fn resolve_read_path(cwd: &Path, input: &str) -> PathBuf {
    let resolved = resolve_to_cwd(cwd, input);
    if resolved.exists() {
        return resolved;
    }
    let Some(text) = resolved.to_str() else {
        return resolved;
    };
    let am_pm = ["AM", "PM", "am", "pm", "Am", "Pm", "aM", "pM"]
        .iter()
        .fold(text.to_string(), |text, half| {
            text.replace(&format!(" {half}."), &format!("\u{202f}{half}."))
        });
    [am_pm, text.replace('\'', "\u{2019}")]
        .into_iter()
        .map(PathBuf::from)
        .find(|variant| variant.exists())
        .unwrap_or(resolved)
}

fn resolve_with_home(cwd: &Path, input: &str, home: Option<&Path>) -> PathBuf {
    // Unicode spaces that editors and terminals paste in place of a space.
    let input = input.replace(
        |character| {
            matches!(
                character,
                '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            )
        },
        " ",
    );
    // `@path` is how prompts refer to files.
    let input = input.strip_prefix('@').unwrap_or(&input);
    let path = match (input, home) {
        ("~", Some(home)) => home.to_path_buf(),
        (_, Some(home)) if input.starts_with("~/") => home.join(&input[2..]),
        _ => match input.strip_prefix("file://") {
            Some(url_path) => PathBuf::from(percent_decode(
                url_path.strip_prefix("localhost").unwrap_or(url_path),
            )),
            None => PathBuf::from(input),
        },
    };
    // The result is absolute even when `cwd` is not.
    let base = std::env::current_dir().unwrap_or_default().join(cwd);
    normalize(&base.join(path))
}

/// Removes `.` and resolves `..` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .filter(|_| bytes[index] == b'%')
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match hex {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(input: &str) -> PathBuf {
        resolve_with_home(Path::new("/work/dir"), input, Some(Path::new("/home/user")))
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

    #[test]
    fn read_paths_fall_back_to_macos_screenshot_spellings() {
        let dir = std::env::temp_dir().join(format!("pi-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let narrow = dir.join("Shot at 9.41\u{202f}AM.png");
        let curly = dir.join("Capture d\u{2019}ecran.png");
        std::fs::write(&narrow, "x").unwrap();
        std::fs::write(&curly, "x").unwrap();
        assert_eq!(resolve_read_path(&dir, "Shot at 9.41 AM.png"), narrow);
        assert_eq!(resolve_read_path(&dir, "Capture d'ecran.png"), curly);
        assert_eq!(
            resolve_read_path(&dir, "missing.png"),
            dir.join("missing.png")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

//! Paths as `NodeExecutionEnv` handles them: Node's POSIX `path.join`,
//! `path.resolve` and `path.basename`, `fileURLToPath`, and `os.tmpdir()`.
//! Separators and dots are ASCII, so working on UTF-8 bytes gives what Node
//! gives working on UTF-16 code units.

/// Node's `normalizeString` for POSIX: removes `.` segments, empty segments
/// and, where it can, `..` with the segment before it.
fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let bytes = path.as_bytes();
    let mut result = String::new();
    let mut last_segment_length = 0usize;
    let mut last_slash: isize = -1;
    let mut dots: isize = 0;
    let mut code = 0u8;
    for index in 0..=bytes.len() {
        if index < bytes.len() {
            code = bytes[index];
        } else if code == b'/' {
            break;
        } else {
            code = b'/';
        }
        if code == b'/' {
            if last_slash == index as isize - 1 || dots == 1 {
                // A `.` segment or an empty one.
            } else if dots == 2 {
                let ends_with_parent =
                    result.len() >= 2 && last_segment_length == 2 && result.ends_with("..");
                if !ends_with_parent {
                    if result.len() > 2 {
                        match result.rfind('/') {
                            None => {
                                result.clear();
                                last_segment_length = 0;
                            }
                            Some(last) => {
                                result.truncate(last);
                                let slash = result.rfind('/').map_or(-1, |at| at as isize);
                                last_segment_length = (result.len() as isize - 1 - slash) as usize;
                            }
                        }
                        last_slash = index as isize;
                        dots = 0;
                        continue;
                    } else if !result.is_empty() {
                        result.clear();
                        last_segment_length = 0;
                        last_slash = index as isize;
                        dots = 0;
                        continue;
                    }
                }
                if allow_above_root {
                    result.push_str(if result.is_empty() { ".." } else { "/.." });
                    last_segment_length = 2;
                }
            } else {
                let segment = &path[(last_slash + 1) as usize..index];
                if !result.is_empty() {
                    result.push('/');
                }
                result.push_str(segment);
                last_segment_length = index - (last_slash + 1) as usize;
            }
            last_slash = index as isize;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
    }
    result
}

/// Node's `path.posix.normalize`.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing_separator = path.ends_with('/');
    let mut normalized = normalize_string(path, !absolute);
    if normalized.is_empty() {
        if absolute {
            return "/".to_string();
        }
        return if trailing_separator { "./" } else { "." }.to_string();
    }
    if trailing_separator {
        normalized.push('/');
    }
    if absolute {
        format!("/{normalized}")
    } else {
        normalized
    }
}

/// Node's `path.posix.join`.
pub fn join<S: AsRef<str>>(parts: &[S]) -> String {
    let parts = parts
        .iter()
        .map(AsRef::as_ref)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return ".".to_string();
    }
    normalize(&parts.join("/"))
}

/// The process's working directory, as Node's `process.cwd()`.
fn process_cwd() -> String {
    std::env::current_dir()
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

/// Node's `path.posix.resolve`: the parts from the right until one is
/// absolute, else the process's working directory first.
pub fn resolve<S: AsRef<str>>(parts: &[S]) -> String {
    let mut resolved = String::new();
    let mut absolute = false;
    for part in parts
        .iter()
        .map(|part| part.as_ref().to_string())
        .rev()
        .chain(std::iter::once_with(process_cwd))
    {
        if part.is_empty() {
            continue;
        }
        resolved = format!("{part}/{resolved}");
        absolute = part.starts_with('/');
        if absolute {
            break;
        }
    }
    let normalized = normalize_string(&resolved, !absolute);
    if absolute {
        format!("/{normalized}")
    } else if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

/// Node's `path.posix.basename` without a suffix.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "";
    }
    match trimmed.rfind('/') {
        Some(slash) => &trimmed[slash + 1..],
        None => trimmed,
    }
}

/// Node's `os.tmpdir()` on POSIX: `TMPDIR`, `TMP`, `TEMP`, then `/tmp`,
/// without a trailing slash.
pub fn tmpdir() -> String {
    let mut path = ["TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "/tmp".to_string());
    if path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// Node's `fileURLToPath` on POSIX for a string starting with `file://`, or
/// `None` where it throws: a host other than `localhost`, an encoded `/`, or
/// an escape that is not UTF-8.
fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // The URL parser treats `\` as `/` in file URLs and ends the path at a
    // query or a fragment.
    let rest = rest.replace('\\', "/");
    let rest = rest
        .find(['?', '#'])
        .map_or(rest.as_str(), |end| &rest[..end]);
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    // The parser resolves `.` and `..` segments, also percent-encoded ones.
    let mut segments: Vec<&str> = Vec::new();
    let raw_segments = path[1..].split('/').collect::<Vec<_>>();
    let last = raw_segments.len() - 1;
    for (index, segment) in raw_segments.iter().enumerate() {
        let lower = segment.to_ascii_lowercase();
        let single = matches!(lower.as_str(), "." | "%2e");
        let double = matches!(lower.as_str(), ".." | ".%2e" | "%2e." | "%2e%2e");
        if double {
            segments.pop();
            if index == last {
                segments.push("");
            }
        } else if single {
            if index == last {
                segments.push("");
            }
        } else {
            segments.push(segment);
        }
    }
    let pathname = format!("/{}", segments.join("/"));
    if pathname.to_ascii_lowercase().contains("%2f") {
        return None;
    }
    percent_decode_strict(&pathname)
}

/// `decodeURIComponent`: `None` for a malformed escape or bytes that are not
/// UTF-8.
fn percent_decode_strict(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// How `LocalEnv` resolves a path (`NodeExecutionEnv`'s `resolvePath`): `~`
/// and `~/…` under `home`, a `file://` URL to its path, and the rest against
/// `cwd`, normalized. Paths are not confined to `cwd`.
pub fn resolve_local_path(cwd: &str, path: &str, home: Option<&str>) -> String {
    let home = home.unwrap_or("");
    let normalized = if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        join(&[home, rest])
    } else if path.starts_with("file://") {
        file_url_to_path(path).unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    };
    if normalized.starts_with('/') {
        resolve(&[normalized])
    } else {
        resolve(&[cwd, &normalized])
    }
}

/// The user's home directory, as Node's `os.homedir()`.
pub(crate) fn home_dir() -> Option<String> {
    std::env::home_dir().map(|home| home.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_normalize_follow_node() {
        assert_eq!(join(&["a", "b"]), "a/b");
        assert_eq!(join(&["/a/", "../b"]), "/b");
        assert_eq!(join(&["a/", ""]), "a/");
        assert_eq!(join::<&str>(&[]), ".");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(join(&["/x", "..", ".."]), "/");
        assert_eq!(join(&["a", "..", ".."]), "..");
        assert_eq!(join(&["/tmp", "../x"]), "/x");
        assert_eq!(normalize("./"), "./");
        assert_eq!(normalize("a//b/./c/"), "a/b/c/");
        assert_eq!(normalize("../../a"), "../../a");
        assert_eq!(normalize("a/../../b"), "../b");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("..."), "...");
        assert_eq!(normalize("a/.../b"), "a/.../b");
        assert_eq!(normalize("abc/../d"), "d");
        assert_eq!(normalize("ab/cd/../../e"), "e");
    }

    #[test]
    fn resolve_and_basename_follow_node() {
        assert_eq!(resolve(&["/a", "b", "../c"]), "/a/c");
        assert_eq!(resolve(&["/a", "/b"]), "/b");
        assert_eq!(resolve(&["/a/b/"]), "/a/b");
        assert_eq!(resolve(&["/"]), "/");
        assert!(resolve(&["x"]).starts_with('/'));
        assert_eq!(basename("/a/b.txt"), "b.txt");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("/"), "");
        assert_eq!(basename("b"), "b");
    }

    #[test]
    fn local_paths_resolve_like_node_execution_env() {
        let resolve = |path: &str| resolve_local_path("/work/dir", path, Some("/home/user"));
        assert_eq!(resolve("a.txt"), "/work/dir/a.txt");
        assert_eq!(resolve("~"), "/home/user");
        assert_eq!(resolve("~/x/../y"), "/home/user/y");
        assert_eq!(resolve("~other"), "/work/dir/~other");
        assert_eq!(resolve("file:///tmp/a%20b.txt"), "/tmp/a b.txt");
        assert_eq!(resolve("file://localhost/tmp/x"), "/tmp/x");
        assert_eq!(resolve("file:///tmp/a/../b?q#f"), "/tmp/b");
        assert_eq!(resolve("file:///tmp/%E2%82%AC"), "/tmp/\u{20ac}");
        // Where Node's fileURLToPath throws, the URL is an ordinary path.
        assert_eq!(resolve("file://host/x"), "/work/dir/file:/host/x");
        assert_eq!(resolve("file:///a%2Fb"), "/work/dir/file:/a%2Fb");
        assert_eq!(resolve("file:///a%FF"), "/work/dir/file:/a%FF");
        assert_eq!(resolve_local_path("/w", "a", None), "/w/a");
    }
}

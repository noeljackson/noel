//! Context files (AGENTS.md and its kin) from the agent directory and from the cwd up to the git root.

use super::*;

/// Context file names, first match wins: one file per directory.
/// `AGENTS.override.md` replaces only its own directory's file.
pub const CONTEXT_FILE_NAMES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// Context files from the agent dir, then from the outermost ancestor of
/// `cwd` down to `cwd`. Like upstream, these load whatever the project trust
/// decision: trust gates only `.noel` resources and `.agents/skills`.
pub(crate) fn load_context_files(
    cwd: &Path,
    agent_dir: &Path,
    diagnostics: &mut Vec<String>,
) -> Vec<ContextFile> {
    let global = load_context_file_from_dir(agent_dir, diagnostics);
    let shadowed = shadowed_context_file(cwd);
    let mut ancestors = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if let Some(file) = load_context_file_from_dir(dir, diagnostics) {
            // Compare the directory's real path, not the file's: a worktree
            // file that links to the main checkout's would resolve to the
            // shadowed file itself.
            let is_shadowed = file.path.file_name().is_some_and(|name| {
                shadowed.as_deref() == Some(canonical_path(dir).join(name).as_path())
            });
            // The agent dir can also be an ancestor of `cwd`.
            let is_global = global
                .as_ref()
                .is_some_and(|global| global.path == file.path);
            if !is_shadowed && !is_global {
                ancestors.push(file);
            }
        }
        current = dir.parent();
    }
    ancestors.reverse();
    global.into_iter().chain(ancestors).collect()
}

pub(crate) fn load_context_file_from_dir(
    dir: &Path,
    diagnostics: &mut Vec<String>,
) -> Option<ContextFile> {
    for filename in CONTEXT_FILE_NAMES {
        let path = dir.join(filename);
        if !path.is_file() {
            continue;
        }
        match read_text(&path) {
            Ok(content) => return Some(ContextFile { path, content }),
            Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
        }
    }
    None
}

/// The main checkout's context file that a linked worktree nested inside that
/// checkout replaces with its own: both describe the same repository, so
/// loading both would apply its instructions twice. `None` for an ordinary
/// repository, a worktree beside its main checkout, a bare layout, and a
/// submodule, where ancestors load as usual.
pub(crate) fn shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let (worktree_root, common_git_dir) = git_paths(cwd)?;
    let worktree_root = canonical_path(&worktree_root);
    let common_git_dir = canonical_path(&common_git_dir);
    let main_root = common_git_dir.parent()?;
    if worktree_root == main_root || !worktree_root.starts_with(main_root) {
        return None;
    }
    // The common git dir's parent is the main checkout only when that dir is
    // its `.git`: not `proj/.bare`, and not a submodule's `.git/modules/x`.
    if canonical_path(&main_root.join(".git")) != common_git_dir {
        return None;
    }
    let file = load_context_file_from_dir(&worktree_root, &mut Vec::new())?;
    Some(main_root.join(file.path.file_name()?))
}

/// The work tree root and common git dir of the repository holding `cwd`:
/// `.git` is a directory, or a linked worktree's `gitdir:` file.
pub(crate) fn git_paths(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let git = dir.join(".git");
        if git.is_dir() {
            return git.join("HEAD").exists().then(|| (dir.to_path_buf(), git));
        }
        let pointer = fs::read_to_string(&git).unwrap_or_default();
        if let Some(target) = pointer.trim().strip_prefix("gitdir: ") {
            let git_dir = dir.join(target.trim());
            if !git_dir.join("HEAD").exists() {
                return None;
            }
            let common_git_dir = match fs::read_to_string(git_dir.join("commondir")) {
                Ok(common) => git_dir.join(common.trim()),
                Err(_) => git_dir,
            };
            return Some((dir.to_path_buf(), common_git_dir));
        }
        current = dir.parent();
    }
    None
}

/// `path` with symlinks resolved, or as given when it cannot be resolved.
pub(crate) fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

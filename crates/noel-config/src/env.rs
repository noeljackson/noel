//! The environment variables noel reads, named and read here (#184), so a
//! rename or a default changes in one place. `docs/configuration.md` lists
//! them. Crates below noel-config (noel-env, noel-tools, noel-ai's
//! providers) read their own few, which are named where they are read.

use std::ffi::OsString;
use std::path::PathBuf;

/// The agent directory.
pub const AGENT_DIR: &str = "NOEL_DIR";
/// Where session journals go.
pub const SESSION_DIR: &str = "NOEL_SESSION_DIR";
/// Tests refuse the user's real agent directory.
pub const TEST_ISOLATION: &str = "NOEL_TEST_ISOLATION";
/// `1`, `true` or `yes`: offline, as `--offline`.
pub const OFFLINE: &str = "NOEL_OFFLINE";
/// The Bun host shim for TypeScript extensions.
pub const BUN_HOST: &str = "NOEL_BUN_HOST";
/// The `bun` binary (`path` for the one on `PATH`).
pub const BUN_BIN: &str = "NOEL_BUN_BIN";
/// The binary dogfooding's auto restart runs.
pub const DOGFOOD_RESTART_EXE: &str = "NOEL_DOGFOOD_RESTART_EXE";
/// `1` restarts noel when its binary changes.
pub const DOGFOOD_AUTO_RESTART: &str = "NOEL_DOGFOOD_AUTO_RESTART";
/// The external editor, before `VISUAL` and `EDITOR`.
pub const EDITOR_COMMAND: &str = "NOEL_EDITOR_COMMAND";
/// The command `/copy` pipes text into.
pub const CLIPBOARD_COMMAND: &str = "NOEL_CLIPBOARD_COMMAND";
/// The command that reads an image from the clipboard.
pub const CLIPBOARD_IMAGE_COMMAND: &str = "NOEL_CLIPBOARD_IMAGE_COMMAND";
/// The e2e scripts' transcript dump, and where it goes.
pub const TUI_E2E_DUMP: &str = "NOEL_TUI_E2E_DUMP";
pub const TUI_E2E_DUMP_FILE: &str = "NOEL_TUI_E2E_DUMP_FILE";

/// A variable's value, unless it is unset or empty.
pub fn non_empty(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// The home directory (`HOME`).
pub fn home_dir() -> Option<PathBuf> {
    non_empty("HOME").map(PathBuf::from)
}

/// Whether `NOEL_OFFLINE` asks for offline mode.
pub fn offline() -> bool {
    std::env::var(OFFLINE).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

/// The agent directory `NOEL_DIR` names, if it names one.
pub fn agent_dir() -> Option<PathBuf> {
    non_empty(AGENT_DIR).map(PathBuf::from)
}

/// The binary dogfooding restarts into: `NOEL_DOGFOOD_RESTART_EXE`, else
/// this one.
pub fn restart_executable() -> Option<PathBuf> {
    non_empty(DOGFOOD_RESTART_EXE)
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
}

/// The directories of `PATH`, in order.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_the_documented_ones() {
        let docs = include_str!("../../../docs/configuration.md");
        for name in [
            AGENT_DIR,
            SESSION_DIR,
            TEST_ISOLATION,
            OFFLINE,
            BUN_HOST,
            BUN_BIN,
            DOGFOOD_RESTART_EXE,
            DOGFOOD_AUTO_RESTART,
            EDITOR_COMMAND,
            CLIPBOARD_COMMAND,
            CLIPBOARD_IMAGE_COMMAND,
            TUI_E2E_DUMP,
            TUI_E2E_DUMP_FILE,
        ] {
            assert!(docs.contains(&format!("`{name}`")), "{name} is documented");
        }
    }
}

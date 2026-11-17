#!/usr/bin/env bash
set -euo pipefail

# Builds noel as it was with the in-process TUI (commit 7850c69, the last
# with NOEL_CLIENT_TUI=0), plus the exit-time transcript file the render
# suite reads (NOEL_TUI_E2E_DUMP_FILE), and prints the binary's path. The
# render suite runs one faux script with it in process and compares
# (docs/client-tui-plan.md, slice 4).
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
reference="${NOEL_INPROCESS_REF:-7850c69}"
tree="${NOEL_INPROCESS_TREE:-${repo_root}/target/inprocess-reference/tree}"
target="${NOEL_INPROCESS_TARGET:-${repo_root}/target/inprocess-reference/target}"
if [ ! -d "${tree}" ]; then
  mkdir -p "$(dirname "${tree}")"
  git -C "${repo_root}" worktree add -q --detach "${tree}" "${reference}"
fi
main="${tree}/crates/noel-cli/src/main.rs"
if ! grep -q NOEL_TUI_E2E_DUMP_FILE "${main}"; then
  python3 - "${main}" <<'PY'
import sys
path = sys.argv[1]
source = open(path).read()
old = '''    if dump_transcript {
        println!();
        println!("{}", app.transcript_text());
    }
    Ok(())'''
new = '''    if dump_transcript {
        println!();
        println!("{}", app.transcript_text());
    }
    if let Some(path) = std::env::var_os("NOEL_TUI_E2E_DUMP_FILE").filter(|path| !path.is_empty()) {
        fs::write(path, format!("{}\\n", app.transcript_text()))?;
    }
    Ok(())'''
assert source.count(old) == 1, "close_tui_terminal changed"
open(path, "w").write(source.replace(old, new))
PY
fi
(cd "${tree}" && CARGO_TARGET_DIR="${target}" cargo build -q -p noel-cli)
echo "${target}/debug/noel"

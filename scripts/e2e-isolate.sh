# shellcheck shell=bash
# Sourced by every scripts/e2e-*.sh suite, right after repo_root is set.
#
# The first suite re-executes itself under a clean environment: an allowlist of
# variables, an empty HOME, bash as the pane shell and a private tmux server. Panes
# therefore never see the developer's provider keys, logins, config or tmux
# (for example /usage probing real accounts). Work paths live under a short
# scratch root, so pane output that echoes a path stays on one line; the root is
# exec-capable for the suites' extension scripts. Nested suites inherit this
# isolation, and the suite that created it removes it on exit through
# e2e_isolation_cleanup.

if [ -z "${PI_E2E_ROOT:-}" ]; then
  e2e_scratch="${PI_E2E_TMPDIR:-/var/tmp}"
  [ -d "${e2e_scratch}" ] && [ -w "${e2e_scratch}" ] || e2e_scratch="${TMPDIR:-/tmp}"
  e2e_new_root="$(mktemp -d "${e2e_scratch}/pi-e2e.XXXXXX")"
  mkdir -p "${e2e_new_root}/home" "${e2e_new_root}/tmux"
  e2e_env=(
    "PATH=${PATH}"
    "TERM=${TERM:-xterm-256color}"
    "LANG=${LANG:-C.UTF-8}"
    "USER=${USER:-}"
    "LOGNAME=${LOGNAME:-}"
    "CARGO_HOME=${CARGO_HOME:-${HOME}/.cargo}"
    "RUSTUP_HOME=${RUSTUP_HOME:-${HOME}/.rustup}"
    "HOME=${e2e_new_root}/home"
    # bash runs no first-run wizard (zsh's newuser setup swallows sent keys).
    "SHELL=$(command -v bash)"
    "TMUX_TMPDIR=${e2e_new_root}/tmux"
    "PI_E2E_ROOT=${e2e_new_root}"
    "PI_E2E_OWNER_PID=$$"
  )
  # Keep the build configuration (CARGO_INCREMENTAL, RUSTFLAGS, linker and
  # target settings) so panes reuse the outer build instead of recompiling;
  # names that look like credentials never pass.
  for e2e_var in $(compgen -e); do
    case "${e2e_var}" in
      *TOKEN* | *SECRET* | *PASSWORD* | *KEY*) ;;
      CARGO_HOME | RUSTUP_HOME) ;;
      CARGO_* | RUST* | PI_BIN | TMPDIR) e2e_env+=("${e2e_var}=${!e2e_var}") ;;
    esac
  done
  exec env -i "${e2e_env[@]}" bash "$0" "$@"
fi
e2e_root="${PI_E2E_ROOT}"

# Takes the suite's exit status: a failed run keeps its artifacts for inspection.
e2e_isolation_cleanup() {
  local status="${1:-0}"
  if [ "${PI_E2E_OWNER_PID:-}" = "$$" ]; then
    tmux kill-server >/dev/null 2>&1 || true
    if [ "${status}" -eq 0 ]; then
      rm -rf "${PI_E2E_ROOT}"
    else
      echo "e2e artifacts kept in ${PI_E2E_ROOT}" >&2
    fi
  fi
}

#!/usr/bin/env bash
set -euo pipefail

# Fatal signals in the owned-screen TUI, checked live: an outside SIGINT is
# ignored (ctrl+c stays a key), while SIGTERM and SIGHUP kill the running
# shell command's whole process group and SIGTERM restores the terminal.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
work_dir="$(mktemp -d "${e2e_root}/e2e-signals.XXXXXX")"
session="noel-e2e-signals-${$}"
trap 'e2e_status=$?; tmux kill-session -t "${session}" >/dev/null 2>&1 || true; e2e_isolation_cleanup "${e2e_status}"' EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt" 2>/dev/null || true; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 120); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing signals marker: $1"
}
wait_for_file() {
  for _ in $(seq 1 120); do if [ -s "$1" ]; then return; fi; sleep 0.1; done
  capture
  fail "shell command never wrote $1"
}
# A killed process may linger as a zombie until reaped; that counts as gone.
running() {
  local state
  state="$(ps -o stat= -p "$1" 2>/dev/null || true)"
  [ -n "${state}" ] && [ "${state#Z}" = "${state}" ]
}
is_noel() { [ "$(basename "$(ps -o comm= -p "$1" 2>/dev/null || echo gone)")" = noel ]; }
wait_gone() {
  for _ in $(seq 1 50); do if ! running "$1"; then return; fi; sleep 0.1; done
  kill -KILL "$1" 2>/dev/null || true
  capture
  fail "$2"
}
# The exit status and the final marker are printf-assembled so the command
# line echoed in the pane cannot match them.
start_noel() {
  printf '{"type":"started","session_id":"%s","cwd":"."}\n' "$1" > "${work_dir}/$1.jsonl"
  tmux new-session -d -s "${session}" -x 100 -y 40
  tmux pipe-pane -t "${session}" -o "cat > '${work_dir}/terminal.raw'"
  tmux send-keys -t "${session}" \
    "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_DIR='${work_dir}/agent' '${noel_bin}' --session '${work_dir}/$1.jsonl' --model faux/echo --offline; printf 'noel-exit=%s\\n' \$?; stty -a; printf 'shell-after-%s\\n' signal" Enter
  wait_for '│ >'
}
# Runs a `!` command whose shell publishes noel's pid (its parent) and, unless
# told otherwise, the pid of a background child it then waits on.
run_shell_command() {
  rm -f "${work_dir}/noel.pid" "${work_dir}/child.pid"
  local command="! echo \$PPID > noel.pid"
  if [ "$1" = waiting ]; then
    command="${command}; sleep 30 & echo \$! > child.tmp && mv child.tmp child.pid; wait"
  fi
  tmux send-keys -t "${session}" -l "${command}"
  tmux send-keys -t "${session}" Enter
  wait_for_file "${work_dir}/noel.pid"
  noel_pid="$(cat "${work_dir}/noel.pid")"
  is_noel "${noel_pid}" || fail 'the shell command did not run under noel'
  if [ "$1" = waiting ]; then
    wait_for_file "${work_dir}/child.pid"
    child_pid="$(cat "${work_dir}/child.pid")"
    running "${child_pid}" || fail 'background child did not start'
  fi
}

start_noel term
[ "$(tmux display-message -p -t "${session}" '#{alternate_on}')" = 1 ] || fail 'TUI did not own the screen'
run_shell_command quick
# The raw-mode TUI reads ctrl+c as a key; an outside SIGINT must not end it.
kill -INT "${noel_pid}"
sleep 0.5
running "${noel_pid}" || fail 'SIGINT ended the raw-mode TUI'
tui_pid="${noel_pid}"
wait_for '│ >'
run_shell_command waiting
[ "${noel_pid}" = "${tui_pid}" ] || fail 'the TUI process changed'
kill -TERM "${noel_pid}"
wait_for 'shell-after-signal'
grep -Fq 'noel-exit=143' "${work_dir}/pane.txt" || fail 'SIGTERM did not exit with status 143'
wait_gone "${child_pid}" "SIGTERM left the shell command's child ${child_pid} running"
[ "$(tmux display-message -p -t "${session}" '#{alternate_on}')" = 0 ] || fail 'SIGTERM left the alternate screen on'
grep -Eq '(^|[ ;])icanon[ ;]' "${work_dir}/pane.txt" || fail 'SIGTERM left raw mode'
grep -Eq '(^|[ ;])echo[ ;]' "${work_dir}/pane.txt" || fail 'SIGTERM left echo off'
for sequence in $'\033[?1049l' $'\033[?1000l' $'\033[?1006l' $'\033[?2004l'; do
  grep -Fq "${sequence}" "${work_dir}/terminal.raw" || fail 'SIGTERM skipped a terminal restore sequence'
done
# Closing the terminal hangs up noel. The command runs in its own session, so
# the hangup never reaches it: noel must kill its group before exiting.
tmux kill-session -t "${session}"
start_noel hup
run_shell_command waiting
tmux kill-session -t "${session}"
wait_gone "${child_pid}" "SIGHUP left the shell command's child ${child_pid} running"
wait_gone "${noel_pid}" 'noel kept running after SIGHUP'
echo "signals e2e passed (artifacts: ${work_dir})"

#!/usr/bin/env bash
set -euo pipefail

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required for TTY e2e tests" >&2
  exit 127
fi

cargo_bin="$(command -v cargo)"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
session_name="pi-e2e-${$}"
session_dir="${repo_root}/target/e2e-tmux-sessions"
work_dir="${repo_root}/target/e2e-tmux-work"
prompt_file="${work_dir}/prompt-ready"

cleanup() {
  tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

rm -rf "${session_dir}" "${work_dir}"
mkdir -p "${session_dir}" "${work_dir}"

tmux new-session -d -s "${session_name}" -x 100 -y 30
tmux send-keys -t "${session_name}" \
  "cd '${repo_root}' && '${cargo_bin}' run -q -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${session_name}" -p -S -2000 | grep -q "pi>"; then
    touch "${prompt_file}"
    break
  fi
  sleep 0.25
done

if [ ! -f "${prompt_file}" ]; then
  tmux capture-pane -t "${session_name}" -p -S -2000 >&2
  echo "pi prompt did not appear" >&2
  exit 1
fi

send_line() {
  tmux send-keys -t "${session_name}" "$1" Enter
  sleep 0.35
}

send_line "/session"
send_line "hello from tmux e2e"
send_line "/write target/e2e-tmux-work/file.txt e2e-ok"
send_line "/read target/e2e-tmux-work/file.txt"
send_line "/reload"
send_line "after reload"
send_line "/session"
send_line "/name tmux-session"
send_line "/labels e2e tty"
send_line "/settings"
send_line "/scoped-models"
send_line "/hotkeys"
send_line "/export target/e2e-tmux-work/session-export.json"
send_line "/fork"
send_line "/session"
send_line "/tree"
send_line "/compact"
send_line "/quit"
sleep 0.5

pane_output="$(tmux capture-pane -t "${session_name}" -p -S -2000 2>/dev/null || true)"
printf '%s\n' "${pane_output}" > "${work_dir}/pane.txt"

require_output() {
  local expected="$1"
  if ! grep -Fq "${expected}" "${work_dir}/pane.txt"; then
    cat "${work_dir}/pane.txt" >&2
    echo "missing expected pane output: ${expected}" >&2
    exit 1
  fi
}

require_output "pi rust cli"
require_output "[faux/echo] hello from tmux e2e"
require_output "wrote ${repo_root}/target/e2e-tmux-work/file.txt"
require_output "e2e-ok"
require_output "reloaded"
require_output "[faux/echo] after reload"
require_output "name: tmux-session"
require_output "labels: e2e, tty"
require_output "agent dir:"
require_output "* faux/echo"
require_output "submit"
require_output "reload"
require_output "exported target/e2e-tmux-work/session-export.json"
require_output "parent:"
require_output "compacted"

mapfile -t session_lines < <(grep '^session: ' "${work_dir}/pane.txt")
if [ "${#session_lines[@]}" -lt 2 ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "expected at least two session lines" >&2
  exit 1
fi

first_session="${session_lines[0]#session: }"
second_session="${session_lines[1]#session: }"
fork_session="${session_lines[-1]#session: }"
if [ "${first_session}" != "${second_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "session changed across reload: ${first_session} != ${second_session}" >&2
  exit 1
fi
if [ "${first_session}" = "${fork_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "fork did not create a new session: ${first_session}" >&2
  exit 1
fi

session_log="${session_dir}/${first_session}.jsonl"
if [ ! -f "${session_log}" ]; then
  echo "missing session log: ${session_log}" >&2
  exit 1
fi

grep -Fq "hello from tmux e2e" "${session_log}"
grep -Fq "after reload" "${session_log}"
grep -Fq "e2e-ok" "${session_log}"

fork_log="${session_dir}/${fork_session}.jsonl"
if [ ! -f "${fork_log}" ]; then
  echo "missing fork log: ${fork_log}" >&2
  exit 1
fi

grep -Fq "${first_session}" "${fork_log}"
grep -Fq "tmux-session" "${fork_log}"
grep -Fq "tty" "${fork_log}"

if [ ! -f "${work_dir}/session-export.json" ]; then
  echo "missing exported session" >&2
  exit 1
fi
grep -Fq "${first_session}" "${work_dir}/session-export.json"

echo "tmux e2e passed: ${first_session}"

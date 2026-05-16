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
  if tmux capture-pane -t "${session_name}" -p | grep -q "pi>"; then
    touch "${prompt_file}"
    break
  fi
  sleep 0.25
done

if [ ! -f "${prompt_file}" ]; then
  tmux capture-pane -t "${session_name}" -p >&2
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
send_line "/quit"
sleep 0.5

pane_output="$(tmux capture-pane -t "${session_name}" -p 2>/dev/null || true)"
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

mapfile -t session_lines < <(grep '^session: ' "${work_dir}/pane.txt")
if [ "${#session_lines[@]}" -lt 2 ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "expected at least two session lines" >&2
  exit 1
fi

first_session="${session_lines[0]#session: }"
last_session="${session_lines[-1]#session: }"
if [ "${first_session}" != "${last_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "session changed across reload: ${first_session} != ${last_session}" >&2
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

echo "tmux e2e passed: ${first_session}"

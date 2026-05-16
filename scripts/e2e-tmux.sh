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
agent_dir="${work_dir}/agent"
prompt_file="${work_dir}/prompt-ready"

cleanup() {
  tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-disabled-${$}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

rm -rf "${session_dir}" "${work_dir}"
mkdir -p "${session_dir}" "${work_dir}" "${agent_dir}/skills" "${agent_dir}/prompts" "${agent_dir}/themes"
printf 'review this\n' > "${agent_dir}/skills/review.md"
printf 'fix {{input}}\n' > "${agent_dir}/prompts/fix.md"
printf '{"name":"dark"}\n' > "${agent_dir}/themes/dark.json"

tmux new-session -d -s "${session_name}" -x 100 -y 30
tmux send-keys -t "${session_name}" \
  "cd '${repo_root}' && PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
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
send_line "/diagnostics"
send_line "/skills"
send_line "/skill:review skill input"
send_line "/prompts"
send_line "/prompt fix broken thing"
send_line "/themes"
send_line "/theme dark"
send_line "/scoped-models"
send_line "/model"
send_line "/model 1"
send_line "/hotkeys"
send_line "/copy"
send_line "/export target/e2e-tmux-work/session-export.json"
send_line "/clone"
send_line "/session"
send_line "/resume 1"
send_line "/session"
send_line "/import target/e2e-tmux-work/session-export.json"
send_line "/session"
send_line "/fork"
send_line "/session"
send_line "/tree"
send_line "/resume"
send_line "/compact"
send_line "/multiline"
send_line "multi one"
send_line "multi two"
send_line "."
send_line "/delete"
send_line "/session"
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
require_output "no diagnostics"
require_output "review"
require_output "review this"
require_output "skill input"
require_output "fix"
require_output "fix broken thing"
require_output "theme: dark"
require_output "* faux/echo"
require_output "model: faux/echo"
require_output "submit"
require_output "reload"
require_output "[faux/echo] after reload"
require_output "exported target/e2e-tmux-work/session-export.json"
require_output "parent:"
require_output "compacted"
require_output "enter multiline prompt"
require_output "[faux/echo] multi one"
require_output "multi two"
require_output "deleted ${session_dir}/"

mapfile -t session_lines < <(grep '^session: ' "${work_dir}/pane.txt")
if [ "${#session_lines[@]}" -lt 13 ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "expected at least thirteen session lines" >&2
  exit 1
fi

first_session="${session_lines[0]#session: }"
second_session="${session_lines[1]#session: }"
clone_session="${session_lines[4]#session: }"
resumed_session="${session_lines[6]#session: }"
imported_session="${session_lines[8]#session: }"
fork_session="${session_lines[10]#session: }"
post_delete_session="${session_lines[12]#session: }"
if [ "${first_session}" != "${second_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "session changed across reload: ${first_session} != ${second_session}" >&2
  exit 1
fi
if [ "${clone_session}" = "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "clone did not create a new session: ${clone_session}" >&2
  exit 1
fi
if [ "${resumed_session}" != "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "numbered resume did not return to first session: ${resumed_session} != ${first_session}" >&2
  exit 1
fi
if [ "${imported_session}" != "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "import did not restore exported session id: ${imported_session} != ${first_session}" >&2
  exit 1
fi
if [ "${first_session}" = "${fork_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "fork did not create a new session: ${first_session}" >&2
  exit 1
fi
if [ "${post_delete_session}" = "${fork_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "delete did not replace current session: ${post_delete_session}" >&2
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

clone_log="${session_dir}/${clone_session}.jsonl"
if [ ! -f "${clone_log}" ]; then
  echo "missing clone log: ${clone_log}" >&2
  exit 1
fi
grep -Fq "tmux-session" "${clone_log}"

fork_log="${session_dir}/${fork_session}.jsonl"
if [ -f "${fork_log}" ]; then
  echo "deleted fork log still exists: ${fork_log}" >&2
  exit 1
fi

if [ ! -f "${work_dir}/session-export.json" ]; then
  echo "missing exported session" >&2
  exit 1
fi
grep -Fq "${first_session}" "${work_dir}/session-export.json"

disabled_session="pi-e2e-disabled-${$}"
tmux new-session -d -s "${disabled_session}" -x 100 -y 20
tmux send-keys -t "${disabled_session}" \
  "cd '${repo_root}' && PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q -p pi-cli -- --session-dir '${session_dir}' --model faux/echo --no-tools" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${disabled_session}" -p -S -2000 | grep -q "pi>"; then
    break
  fi
  sleep 0.25
done

tmux send-keys -t "${disabled_session}" "/write target/e2e-tmux-work/blocked.txt blocked" Enter
sleep 0.5
tmux send-keys -t "${disabled_session}" "/quit" Enter
sleep 0.5
disabled_output="$(tmux capture-pane -t "${disabled_session}" -p -S -2000 2>/dev/null || true)"
printf '%s\n' "${disabled_output}" > "${work_dir}/disabled-pane.txt"
tmux kill-session -t "${disabled_session}" >/dev/null 2>&1 || true

if ! grep -Fq "tool is disabled: write" "${work_dir}/disabled-pane.txt"; then
  cat "${work_dir}/disabled-pane.txt" >&2
  echo "disabled tool smoke did not fail as expected" >&2
  exit 1
fi
if [ -f "${work_dir}/blocked.txt" ]; then
  echo "disabled tool unexpectedly wrote blocked.txt" >&2
  exit 1
fi

echo "tmux e2e passed: ${first_session}"

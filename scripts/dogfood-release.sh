#!/usr/bin/env bash
set -euo pipefail

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required for release dogfood tests" >&2
  exit 127
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pi_bin="${PI_BIN:-${repo_root}/target/release/pi}"

if [ ! -x "${pi_bin}" ]; then
  echo "release binary is not executable: ${pi_bin}" >&2
  echo "run make release first, or set PI_BIN" >&2
  exit 1
fi

session_name="pi-dogfood-${$}"
session_dir="${repo_root}/target/dogfood-release-sessions"
work_dir="${repo_root}/target/dogfood-release-work"
cwd_dir="${work_dir}/cwd"
agent_dir="${work_dir}/agent"
prompt_file="${work_dir}/prompt-ready"

cleanup() {
  tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

rm -rf "${session_dir}" "${work_dir}"
mkdir -p "${session_dir}" "${cwd_dir}" "${agent_dir}"

cat > "${agent_dir}/settings.json" <<'JSON'
{
  "modelRefresh": {
    "enabled": false
  },
  "showHardwareCursor": true
}
JSON

"${pi_bin}" --version > "${work_dir}/version.txt"
grep -Eq '^pi [0-9]+' "${work_dir}/version.txt"

tmux new-session -d -s "${session_name}" -x 110 -y 34
tmux send-keys -t "${session_name}" \
  "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' PI_CLIPBOARD_COMMAND='cat > ${work_dir}/clipboard.txt' '${pi_bin}' --session-dir '${session_dir}' --model faux/echo" \
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
  echo "pi prompt did not appear in release dogfood" >&2
  exit 1
fi

tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/initial-pane.txt"
grep -Fq "conversation" "${work_dir}/initial-pane.txt"
grep -Fq "pi>" "${work_dir}/initial-pane.txt"
grep -Fq "faux/echo" "${work_dir}/initial-pane.txt"

send_line() {
  tmux send-keys -t "${session_name}" -l "$1"
  tmux send-keys -t "${session_name}" Enter
  sleep 0.35
}

send_key() {
  tmux send-keys -t "${session_name}" "$1"
  sleep 0.25
}

send_line "/session"
send_line "dogfood release prompt"
send_line "/write ${cwd_dir}/note.txt dogfood-ok"
send_line "/read ${cwd_dir}/note.txt"
send_line "/reload"
send_line "/session"
send_line "dogfood after reload"
send_line "/queue dogfood queued"
send_line "dogfood trigger queue"
send_line "/queue"
send_line "/model"
sleep 0.5
tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/selector-pane.txt"
grep -Fq "model selector" "${work_dir}/selector-pane.txt"
tmux send-keys -t "${session_name}" -l "anthropic/claude-opus-4-7"
sleep 0.25
tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/selector-filtered-pane.txt"
grep -Fq "left/right to adjust" "${work_dir}/selector-filtered-pane.txt"
send_key Right
send_key Enter
sleep 0.35
send_line "/thinking"
send_line "/model faux/echo"
send_line "dogfood back on faux"
send_line "/export ${work_dir}/session-export.json"
send_line "/export ${work_dir}/session-export.jsonl"
send_line "/clone"
send_line "/session"
send_line "/resume 1"
send_line "/session"
send_line "/copy"
send_line "/quit"
sleep 0.5

pane_output="$(tmux capture-pane -t "${session_name}" -p -S -2000 2>/dev/null || true)"
printf '%s\n' "${pane_output}" > "${work_dir}/pane.txt"

require_output() {
  local expected="$1"
  if ! grep -Fq "${expected}" "${work_dir}/pane.txt"; then
    cat "${work_dir}/pane.txt" >&2
    echo "missing expected dogfood output: ${expected}" >&2
    exit 1
  fi
}

require_output "pi rust cli"
require_output "assistant>"
require_output "[faux/echo] dogfood release prompt"
require_output "wrote ${cwd_dir}/note.txt"
require_output "dogfood-ok"
require_output "reloaded"
require_output "[faux/echo] dogfood after reload"
require_output "queued: 1"
require_output "[faux/echo] dogfood queued"
require_output "queue is empty"
require_output "model selector"
require_output "model: anthropic/claude-opus-4-7"
require_output "thinking: max"
require_output "model: faux/echo"
require_output "[faux/echo] dogfood back on faux"
require_output "exported ${work_dir}/session-export.json"
require_output "exported ${work_dir}/session-export.jsonl"
require_output "copied to clipboard via cat > ${work_dir}/clipboard.txt"

mapfile -t session_lines < <(grep -E '^(system> )?session: ' "${work_dir}/pane.txt" | sed -E 's/^system> //')
if [ "${#session_lines[@]}" -lt 6 ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "expected at least six dogfood session lines" >&2
  exit 1
fi

first_session="${session_lines[0]#session: }"
post_reload_session="${session_lines[1]#session: }"
clone_session="${session_lines[2]#session: }"
resumed_session="${session_lines[4]#session: }"
if [ "${first_session}" != "${post_reload_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "session changed across release reload: ${first_session} != ${post_reload_session}" >&2
  exit 1
fi
if [ "${clone_session}" = "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "release clone did not create a new session: ${clone_session}" >&2
  exit 1
fi
if [ "${resumed_session}" != "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "release resume did not return to first session: ${resumed_session} != ${first_session}" >&2
  exit 1
fi

session_log="${session_dir}/${first_session}.jsonl"
if [ ! -f "${session_log}" ]; then
  echo "missing dogfood session log: ${session_log}" >&2
  exit 1
fi
grep -Fq "dogfood release prompt" "${session_log}"
grep -Fq "dogfood after reload" "${session_log}"
grep -Fq "dogfood queued" "${session_log}"
grep -Fq "dogfood back on faux" "${session_log}"

if [ ! -f "${work_dir}/session-export.json" ]; then
  echo "missing dogfood exported json session" >&2
  exit 1
fi
grep -Fq "${first_session}" "${work_dir}/session-export.json"
if [ ! -f "${work_dir}/session-export.jsonl" ]; then
  echo "missing dogfood exported jsonl session" >&2
  exit 1
fi
grep -Fq "${first_session}" "${work_dir}/session-export.jsonl"
if [ ! -f "${work_dir}/clipboard.txt" ]; then
  echo "missing dogfood clipboard capture" >&2
  exit 1
fi
grep -Fq "[faux/echo] dogfood back on faux" "${work_dir}/clipboard.txt"

echo "release dogfood passed: ${first_session}"

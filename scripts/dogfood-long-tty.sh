#!/usr/bin/env bash
set -euo pipefail

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required for long TTY dogfood tests" >&2
  exit 127
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pi_bin="${PI_BIN:-${repo_root}/target/release/pi}"

if [ ! -x "${pi_bin}" ]; then
  echo "release binary is not executable: ${pi_bin}" >&2
  echo "run make release first, or set PI_BIN" >&2
  exit 1
fi

session_name="pi-long-${$}"
session_dir="${repo_root}/target/dogfood-long-sessions"
work_dir="${repo_root}/target/dogfood-long-work"
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

tmux new-session -d -s "${session_name}" -x 92 -y 24
tmux send-keys -t "${session_name}" \
  "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${pi_bin}' --session-dir '${session_dir}' --model faux/echo" \
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
  echo "pi prompt did not appear in long dogfood" >&2
  exit 1
fi

send_line() {
  tmux send-keys -t "${session_name}" -l "$1"
  tmux send-keys -t "${session_name}" Enter
  sleep "${2:-0.18}"
}

capture() {
  tmux capture-pane -t "${session_name}" -p -S -2000 > "$1"
}

for index in $(seq -w 1 36); do
  send_line "paint-line-${index} wrap-check alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega"
done

send_line "/multiline"
send_line "paint-multiline-one wraps across the viewport with enough content to stress line layout and transcript bookkeeping"
send_line "paint-multiline-two stays attached to the same assistant response after submit"
send_line "."
send_line "/write ${cwd_dir}/paint.txt paint-tool-output"
send_line "/read ${cwd_dir}/paint.txt"
send_line "/model"
sleep 0.5
capture "${work_dir}/selector-pane.txt"
grep -Fq "model selector" "${work_dir}/selector-pane.txt"
tmux send-keys -t "${session_name}" Escape
sleep 0.3

capture "${work_dir}/tail-pane.txt"
grep -Fq "conversation" "${work_dir}/tail-pane.txt"
grep -Fq "pi>" "${work_dir}/tail-pane.txt"
grep -Fq "paint-tool-output" "${work_dir}/tail-pane.txt"
if grep -Fq "paint-line-01" "${work_dir}/tail-pane.txt"; then
  cat "${work_dir}/tail-pane.txt" >&2
  echo "tail viewport unexpectedly still showed the first long-session message" >&2
  exit 1
fi

tmux send-keys -t "${session_name}" Home
sleep 0.4
capture "${work_dir}/top-pane.txt"
grep -Fq "paint-line-01" "${work_dir}/top-pane.txt"
grep -Fq "conversation" "${work_dir}/top-pane.txt"
grep -Fq "pi>" "${work_dir}/top-pane.txt"

tmux send-keys -t "${session_name}" End
sleep 0.4
capture "${work_dir}/end-pane.txt"
grep -Fq "paint-tool-output" "${work_dir}/end-pane.txt"
if grep -Fq "paint-line-01" "${work_dir}/end-pane.txt"; then
  cat "${work_dir}/end-pane.txt" >&2
  echo "end key did not return the viewport to the tail" >&2
  exit 1
fi

tmux resize-window -t "${session_name}" -x 72 -y 18
sleep 0.4
capture "${work_dir}/small-pane.txt"
grep -Fq "conversation" "${work_dir}/small-pane.txt"
grep -Fq "pi>" "${work_dir}/small-pane.txt"
grep -Fq "status" "${work_dir}/small-pane.txt"

tmux resize-window -t "${session_name}" -x 132 -y 38
sleep 0.4
capture "${work_dir}/large-pane.txt"
grep -Fq "conversation" "${work_dir}/large-pane.txt"
grep -Fq "paint-tool-output" "${work_dir}/large-pane.txt"

send_line "/export ${work_dir}/long-session.json"
send_line "/quit"
sleep 0.5
capture "${work_dir}/transcript.txt"

grep -Fq "paint-line-01" "${work_dir}/transcript.txt"
grep -Fq "paint-line-36" "${work_dir}/transcript.txt"
grep -Fq "paint-multiline-one" "${work_dir}/transcript.txt"
grep -Fq "paint-tool-output" "${work_dir}/transcript.txt"
grep -Fq "exported ${work_dir}/long-session.json" "${work_dir}/transcript.txt"

if [ ! -f "${work_dir}/long-session.json" ]; then
  echo "missing long dogfood export" >&2
  exit 1
fi
grep -Fq "paint-line-01" "${work_dir}/long-session.json"
grep -Fq "paint-line-36" "${work_dir}/long-session.json"

echo "long TTY dogfood passed"

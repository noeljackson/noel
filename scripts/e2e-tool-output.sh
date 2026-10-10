#!/usr/bin/env bash
set -euo pipefail

# Check live tool previews, not the exit-time transcript dump or paid providers.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pi_bin="${PI_BIN:-${repo_root}/target/debug/pi}"
work_dir="$(mktemp -d "${repo_root}/target/e2e-tool-output.XXXXXX")"
session="pi-e2e-tool-output-${$}"
trap 'tmux kill-session -t "${session}" >/dev/null 2>&1 || true' EXIT
mkdir -p "${work_dir}/agent" "${work_dir}/sessions"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
awk 'BEGIN { for (i = 1; i <= 12; i++) printf "tool-file-line-%03d\n", i }' > "${work_dir}/tool-fixture.txt"
awk 'BEGIN { for (i = 1; i <= 5000; i++) printf "large-file-line-%04d\n", i }' > "${work_dir}/large-fixture.txt"

capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 100); do
    capture
    if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi
    sleep 0.1
  done
  fail "missing tool-output marker: $1"
}
absent() {
  capture
  if grep -Fq "$1" "${work_dir}/pane.txt"; then fail "unexpected tool-output marker: $1"; fi
}

for mode in owned inline; do
  args=''
  if [ "${mode}" = inline ]; then args='--no-alt-screen'; fi
  tmux new-session -d -s "${session}" -x 100 -y 40
  tmux send-keys -t "${session}" \
    "cd '${work_dir}' && env -u PI_TUI_E2E_DUMP PI_CODING_AGENT_DIR='${work_dir}/agent' '${pi_bin}' --session-dir '${work_dir}/sessions' --model faux/echo --offline ${args}" Enter
  wait_for '│ >'
  tmux send-keys -t "${session}" '/read tool-fixture.txt' Enter
  wait_for '11 more lines'
  wait_for '… tool-file-line-012'
  absent tool-file-line-001
  tmux send-keys -t "${session}" C-o
  wait_for tool-file-line-001
  absent '11 more lines'
  tmux send-keys -t "${session}" C-o
  wait_for '11 more lines'
  absent tool-file-line-001
  # Inspection of one result must not leave all subsequent reads expanded.
  tmux send-keys -t "${session}" C-o
  wait_for tool-file-line-001
  tmux send-keys -t "${session}" '/read tool-fixture.txt' Enter
  wait_for '11 more lines'
  absent tool-file-line-001
  tmux send-keys -t "${session}" C-o
  wait_for tool-file-line-001
  # Excluded shell commands have no user transcript entry; reset them too.
  tmux send-keys -t "${session}" '! cat tool-fixture.txt' Enter
  wait_for 'Ran a command'
  wait_for '11 more lines'
  absent tool-file-line-001
  tmux send-keys -t "${session}" C-o
  wait_for tool-file-line-001
  tmux send-keys -t "${session}" C-o
  wait_for '11 more lines'
  absent tool-file-line-001
  # Long command headers are clipped, leaving the final result visible.
  long_command=': long-command-start'
  for _ in $(seq 1 30); do long_command+=' README.md'; done
  long_command+=' end-of-command; cat tool-fixture.txt'
  tmux send-keys -t "${session}" -l "! ${long_command}"
  tmux send-keys -t "${session}" Enter
  wait_for 'Ran a command · $ : long-command-start'
  wait_for '… tool-file-line-012'
  absent end-of-command
  tmux send-keys -t "${session}" C-o
  wait_for end-of-command
  tmux send-keys -t "${session}" C-o
  wait_for '… tool-file-line-012'
  absent end-of-command
  if [ "${mode}" = owned ]; then
    tmux send-keys -t "${session}" '/clear' Enter
    wait_for '│ >'
    # Default read is capped at 2000 lines with a resume notice.
    tmux send-keys -t "${session}" '/read large-fixture.txt' Enter
    wait_for '2002 more lines'
    wait_for 'read the rest with offset=2001'
    wait_for '… [output truncated: showing lines 1-2000 of 5000'
    absent large-file-line-2001
    absent large-file-line-0001
    absent large-file-line-2000
    tmux send-keys -t "${session}" C-o
    wait_for large-file-line-1999
    tmux send-keys -t "${session}" C-o
    wait_for '2002 more lines'
    absent large-file-line-1999
    # The resume syntax from the notice reaches the file end.
    tmux send-keys -t "${session}" '/clear' Enter
    wait_for '│ >'
    tmux send-keys -t "${session}" '/read large-fixture.txt offset=4999' Enter
    wait_for large-file-line-5000
    tmux send-keys -t "${session}" '/quit' Enter
    sleep 0.3
    tmux kill-session -t "${session}" >/dev/null 2>&1 || true
    continue
  fi
  tmux send-keys -t "${session}" '/quit' Enter
  sleep 0.3
  tmux kill-session -t "${session}" >/dev/null 2>&1 || true
done
# Display folding must not truncate the tool result in durable context: the
# first read returns the 2000-line cap plus the resume notice; a resumed read
# with offset reaches the file end.
grep -Fq 'read the rest with offset=2001' "${work_dir}/sessions/"*.jsonl
echo "tool output e2e passed (artifacts: ${work_dir})"

#!/usr/bin/env bash
set -euo pipefail

# Check live tool previews, not the exit-time transcript dump or paid providers.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
work_dir="$(mktemp -d "${e2e_root}/e2e-tool-output.XXXXXX")"
session="noel-e2e-tool-output-${$}"
trap 'e2e_status=$?; tmux kill-session -t "${session}" >/dev/null 2>&1 || true; e2e_isolation_cleanup "${e2e_status}"' EXIT
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
    "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_DIR='${work_dir}/agent' '${noel_bin}' --session-dir '${work_dir}/sessions' --model faux/echo --offline ${args}" Enter
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
    # Default read is capped at 2000 lines with a resume notice. The file
    # ends with a newline, which upstream counts as starting line 5001. The
    # result is 2002 rows: the lines, a blank row, and the notice.
    tmux send-keys -t "${session}" '/read large-fixture.txt' Enter
    wait_for '2001 more lines'
    wait_for '… [Showing lines 1-2000 of 5001. Use offset=2001 to continue.]'
    absent large-file-line-2001
    absent large-file-line-0001
    absent large-file-line-2000
    tmux send-keys -t "${session}" C-o
    wait_for large-file-line-1999
    tmux send-keys -t "${session}" C-o
    wait_for '2001 more lines'
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
grep -Fq 'Use offset=2001 to continue.' "${work_dir}/sessions/"*.jsonl

# A command the model runs shows its latest output while it still runs. Then
# the model's reply breaks off: the retry is announced, and the reply that
# replaces it arrives.
wait_for_both() {
  for _ in $(seq 1 100); do
    capture
    if grep -Fq "$1" "${work_dir}/pane.txt" && grep -Fq "$2" "${work_dir}/pane.txt"; then return; fi
    sleep 0.1
  done
  fail "missing together: $1 and $2"
}
cat > "${work_dir}/script.json" <<'JSON'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "for i in 1 2 3 4; do echo live-line-$i; sleep 1; done; sleep 2; echo live-done"}}}],
  [{"text": "Hel"}, {"error": "terminated"}],
  [{"text": "Hello again."}]
]
JSON
mkdir -p "${work_dir}/live-agent"
printf '{"modelRefresh":{"enabled":false},"retry":{"baseDelayMs":1500}}\n' > "${work_dir}/live-agent/settings.json"
tmux new-session -d -s "${session}" -x 100 -y 40
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_FAUX_SCRIPT='${work_dir}/script.json' NOEL_DIR='${work_dir}/live-agent' '${noel_bin}' --no-session --model faux/script --offline" Enter
wait_for '│ >'
tmux send-keys -t "${session}" 'run it' Enter
# The last line so far, under a command that has not ended.
wait_for_both 'Running a command' '… live-line-4'
absent live-done
wait_for 'Ran a command'
wait_for '… live-done'
wait_for 'retrying in 1s (attempt 1 of 3)'
wait_for 'Hello again.'
# The finished turn shows its command once, and the placeholder of a reply
# that began with a tool call is gone.
shown_once() {
  capture
  local count
  count="$(grep -Fc "$1" "${work_dir}/pane.txt" || true)"
  [ "${count}" = 1 ] || fail "shown ${count} times, not once: $1"
}
shown_once 'Ran a command'
absent 'Working...'
tmux send-keys -t "${session}" '/quit' Enter
sleep 0.3
tmux kill-session -t "${session}" >/dev/null 2>&1 || true

# So does a turn that is interrupted after a tool call, and one that fails
# after it: the script has no answer to the last request.
cat > "${work_dir}/ends.json" <<'JSON'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "echo settle-one"}}}],
  [{"wait_for_file": "never"}],
  [{"tool_call": {"name": "bash", "arguments": {"command": "echo settle-two"}}}]
]
JSON
tmux new-session -d -s "${session}" -x 100 -y 40
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_FAUX_SCRIPT='${work_dir}/ends.json' NOEL_DIR='${work_dir}/live-agent' '${noel_bin}' --no-session --model faux/script --offline" Enter
wait_for '│ >'
tmux send-keys -t "${session}" 'first' Enter
wait_for 'Ran a command · $ echo settle-one'
tmux send-keys -t "${session}" Escape
wait_for 'interrupted'
shown_once 'Ran a command · $ echo settle-one'
absent 'Working...'
tmux send-keys -t "${session}" 'second' Enter
wait_for 'request 4 has no response'
shown_once 'Ran a command · $ echo settle-one'
shown_once 'Ran a command · $ echo settle-two'
absent 'Working...'
tmux send-keys -t "${session}" '/quit' Enter
sleep 0.3
tmux kill-session -t "${session}" >/dev/null 2>&1 || true
echo "tool output e2e passed (artifacts: ${work_dir})"

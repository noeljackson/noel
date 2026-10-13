#!/usr/bin/env bash
set -euo pipefail

# Test native scrollback while the TUI is alive, never the exit-time dump.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
pi_bin="${PI_BIN:-${repo_root}/target/debug/pi}"
work_dir="$(mktemp -d "${e2e_root}/e2e-scrollback.XXXXXX")"
session="pi-e2e-scrollback-${$}"
cleanup() {
  local status=$?
  tmux kill-session -t "${session}" >/dev/null 2>&1 || true
  e2e_isolation_cleanup "${status}"
}
trap cleanup EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
# Resumed messages can arrive in one paint, not just one streamed row at a time.
awk 'BEGIN {
  print "{\"type\":\"started\",\"session_id\":\"scrollback\",\"cwd\":\".\"}"
  printf "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\""
  for (i = 1; i <= 80; i++) printf "saved-answer-%03d%s", i, i == 80 ? "" : "\\n"
  print "\",\"tool_calls\":[],\"media\":[]}}"
}' > "${work_dir}/session.jsonl"
tmux new-session -d -s "${session}" -x 80 -y 20
# Explicitly unset the dump flag even if the shared tmux server inherited it.
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && env -u PI_TUI_E2E_DUMP PI_CODING_AGENT_DIR='${work_dir}/agent' '${pi_bin}' --session '${work_dir}/session.jsonl' --model faux/echo --offline --no-alt-screen" Enter
capture() { tmux capture-pane -t "${session}" -p -S -2000 > "${work_dir}/pane.txt"; }
wait_for() {
  for _ in $(seq 1 100); do
    capture
    if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi
    sleep 0.1
  done
  cat "${work_dir}/pane.txt" >&2
  echo "missing live scrollback marker: $1 (artifacts: ${work_dir})" >&2
  exit 1
}
wait_for "│ >"
wait_for "saved-answer-001"
for i in $(seq 1 80); do
  grep -Fq "saved-answer-$(printf '%03d' "${i}")" "${work_dir}/pane.txt"
done
cp "${work_dir}/pane.txt" "${work_dir}/resumed.txt"
# A long single answer, delivered in several large chunks, must retain every row.
awk 'BEGIN { for (i = 1; i <= 100; i++) printf "streamed-answer-%03d\n", i }' > "${work_dir}/prompt.txt"
tmux load-buffer -b "${session}" "${work_dir}/prompt.txt"
tmux paste-buffer -r -p -d -b "${session}" -t "${session}"
tmux send-keys -t "${session}" Enter
wait_for "[faux/echo] streamed-answer-001"
wait_for "streamed-answer-100"
for i in $(seq 1 100); do
  grep -Fq "streamed-answer-$(printf '%03d' "${i}")" "${work_dir}/pane.txt"
done
# Old prompt/chrome frames must not pollute transcript history.
[ "$(grep -Fc '│ >' "${work_dir}/pane.txt")" = 1 ]
# Idle redraws must not append the same transcript again.
count_before="$(grep -Fc '[faux/echo] streamed-answer-001' "${work_dir}/pane.txt")"
sleep 0.5
capture
[ "$(grep -Fc '[faux/echo] streamed-answer-001' "${work_dir}/pane.txt")" = "${count_before}" ]
# Both shrinking and growing the viewport keep the old beginning accessible.
for size in '60 14' '100 30'; do
  read -r width height <<< "${size}"
  tmux resize-window -t "${session}" -x "${width}" -y "${height}"
  sleep 0.3
  wait_for "│ >"
  grep -Fq "saved-answer-001" "${work_dir}/pane.txt"
  grep -Fq "[faux/echo] streamed-answer-001" "${work_dir}/pane.txt"
done
# /clear purges both the viewport and the real terminal history.
tmux send-keys -t "${session}" '/clear' Enter
sleep 0.3
capture
if grep -Eq 'saved-answer-|streamed-answer-' "${work_dir}/pane.txt"; then
  cat "${work_dir}/pane.txt" >&2
  echo '/clear retained transcript rows' >&2
  exit 1
fi
wait_for "│ >"
tmux send-keys -t "${session}" '/quit' Enter
echo "live scrollback e2e passed (artifacts: ${work_dir})"

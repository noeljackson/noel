#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pi_bin="${PI_BIN:-${repo_root}/target/debug/pi}"
work_dir="$(mktemp -d "${repo_root}/target/e2e-selection.XXXXXX")"
session="pi-e2e-selection-${$}"
trap 'tmux kill-session -t "${session}" >/dev/null 2>&1 || true' EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
awk 'BEGIN {
  print "{\"type\":\"started\",\"session_id\":\"selection\",\"cwd\":\".\"}"
  printf "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\""
  for (i = 1; i <= 80; i++) printf "select-row-%03d%s", i, i == 80 ? "" : "\\n"
  print "\",\"tool_calls\":[],\"media\":[]}}"
}' > "${work_dir}/session.jsonl"
cat > "${work_dir}/clipboard-helper" <<SH
#!/bin/sh
if [ -f '${work_dir}/fail-copy' ]; then exit 1; fi
cat > '${work_dir}/clipboard.txt'
SH
chmod +x "${work_dir}/clipboard-helper"
tmux new-session -d -s "${session}" -x 100 -y 24
# The clipboard is a local stub; no desktop, real credentials, or provider calls.
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && env -u PI_TUI_E2E_DUMP PI_CLIPBOARD_COMMAND='${work_dir}/clipboard-helper' PI_CODING_AGENT_DIR='${work_dir}/agent' PI_FAUX_STREAM_DELAY_MS=5 PI_FAUX_STREAM_CHARS=1 '${pi_bin}' --session '${work_dir}/session.jsonl' --model faux/echo --offline; printf 'selection-shell-restored\\n'" Enter
capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 120); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing selection marker: $1"
}
mouse() { tmux send-keys -t "${session}" Escape "[<$1;$2;$3$4"; }
select_row() {
  local marker="$1" row
  wait_for "${marker}"
  row="$(grep -nF "${marker}" "${work_dir}/pane.txt" | head -1 | cut -d: -f1)"
  mouse 0 3 "${row}" M
  mouse 32 16 "${row}" M
  mouse 0 16 "${row}" m
}
wait_for select-row-080
# Selection and clipboard while reading; dragging must show reverse video.
tmux send-keys -t "${session}" -l 'retained selection draft'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
anchor="$(head -1 "${work_dir}/pane.txt")"
marker="$(head -1 "${work_dir}/pane.txt" | sed -n 's/.*\(select-row-[0-9]*\).*/\1/p')"
[ -n "${marker}" ] || fail 'missing selectable top row'
select_row "${marker}"
wait_for 'copied selection'
[ "$(cat "${work_dir}/clipboard.txt")" = "${marker}" ] || fail 'copy-on-release selected wrong text'
[ "$(head -1 "${work_dir}/pane.txt")" = "${anchor}" ] || fail 'selection moved reading anchor'
wait_for '│ > retained selection draft'
tmux capture-pane -t "${session}" -p -e > "${work_dir}/highlight.txt"
grep -Eq '\[[0-9;]*7([;m])' "${work_dir}/highlight.txt" || fail 'selection was not highlighted'
# Ctrl+C copies, rather than clearing the draft or quitting. /copy also uses selection.
rm "${work_dir}/clipboard.txt"
tmux send-keys -t "${session}" C-c
for _ in $(seq 1 40); do [ -f "${work_dir}/clipboard.txt" ] && break; sleep 0.1; done
[ "$(cat "${work_dir}/clipboard.txt")" = "${marker}" ]
wait_for '│ > retained selection draft'
# Preserve selection through reflow, then copy the selection with /copy.
for size in '70 18' '100 24'; do
  read -r width height <<< "${size}"
  tmux resize-window -t "${session}" -x "${width}" -y "${height}"
  sleep 0.3
  wait_for 'Scroll to bottom'
  rm "${work_dir}/clipboard.txt"
  tmux send-keys -t "${session}" C-c
  for _ in $(seq 1 40); do [ -f "${work_dir}/clipboard.txt" ] && break; sleep 0.1; done
  [ "$(cat "${work_dir}/clipboard.txt")" = "${marker}" ] || fail 'reflow changed copied selection'
done
# Backspace edits the draft without clearing transcript selection.
for _ in $(seq 1 24); do tmux send-keys -t "${session}" BSpace; done
rm "${work_dir}/clipboard.txt"
tmux send-keys -t "${session}" '/copy' Enter
for _ in $(seq 1 40); do [ -f "${work_dir}/clipboard.txt" ] && break; sleep 0.1; done
[ "$(cat "${work_dir}/clipboard.txt")" = "${marker}" ] || fail '/copy ignored selection'
tmux send-keys -t "${session}" Escape
sleep 0.3
wait_for select-row-080
tmux send-keys -t "${session}" '/copy' Enter
wait_for 'copied to clipboard via'
# No selection: /copy keeps its existing last-response behavior.
grep -Fq select-row-001 "${work_dir}/clipboard.txt"
grep -Fq select-row-080 "${work_dir}/clipboard.txt"
# Failed copy stays visible and keeps selection available for Ctrl+C retry.
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
marker="$(head -1 "${work_dir}/pane.txt" | sed -n 's/.*\(select-row-[0-9]*\).*/\1/p')"
touch "${work_dir}/fail-copy"
select_row "${marker}"
wait_for 'copy failed:'
rm "${work_dir}/fail-copy"
tmux send-keys -t "${session}" C-c
wait_for 'copied selection'
[ "$(cat "${work_dir}/clipboard.txt")" = "${marker}" ]
# Clicking the composer clears the selection without touching its draft.
tmux send-keys -t "${session}" -l 'composer draft'
wait_for '│ > composer draft'
row="$(grep -nF '│ > composer draft' "${work_dir}/pane.txt" | cut -d: -f1)"
mouse 0 5 "${row}" M
mouse 0 5 "${row}" m
tmux send-keys -t "${session}" C-c
sleep 0.2
wait_for '│ >'
# Esc returns to bottom and clears selection, but leaves draft alone.
tmux send-keys -t "${session}" Escape
wait_for select-row-080
# Select live output without interrupting, then paste the selected text normally.
printf 'stream-selection-marker %0500d\n' 0 > "${work_dir}/prompt.txt"
tmux load-buffer -b "${session}" "${work_dir}/prompt.txt"
tmux paste-buffer -r -p -d -b "${session}" -t "${session}"
tmux send-keys -t "${session}" Enter
wait_for 'esc to interrupt'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
marker="$(head -1 "${work_dir}/pane.txt" | sed -n 's/.*\(select-row-[0-9]*\).*/\1/p')"
select_row "${marker}"
wait_for 'copied selection'
tmux send-keys -t "${session}" C-c
sleep 0.3
wait_for 'ctrl+c copies selection'
# Generation must still complete in the journal after selection Ctrl+C.
completed=''
for _ in $(seq 1 100); do
  if grep -Fq '"content":"[faux/echo] stream-selection-marker' "${work_dir}/session.jsonl"; then completed=1; break; fi
  sleep 0.1
done
[ -n "${completed}" ] || fail 'selection interrupted streaming'
tmux send-keys -t "${session}" Escape
wait_for '│ >'
tmux load-buffer -b "${session}" "${work_dir}/clipboard.txt"
tmux paste-buffer -r -p -d -b "${session}" -t "${session}"
wait_for "│ > ${marker}"
tmux send-keys -t "${session}" C-c '/quit' Enter
wait_for selection-shell-restored
[ "$(tmux display-message -p -t "${session}" '#{alternate_on}')" = 0 ]
# Remote/headless fallback: no clipboard helpers on PATH. Inspect the emitted
# OSC 52 rather than claiming the terminal actually granted clipboard access.
mkdir "${work_dir}/bin"
ln -s /bin/sh "${work_dir}/bin/sh"
tmux pipe-pane -t "${session}" -o "cat > '${work_dir}/osc52.raw'"
tmux send-keys -t "${session}" \
  "env -u PI_CLIPBOARD_COMMAND -u PI_TUI_E2E_DUMP PATH='${work_dir}/bin' PI_CODING_AGENT_DIR='${work_dir}/agent' '${pi_bin}' --session '${work_dir}/session.jsonl' --model faux/echo --offline; printf 'osc52-shell-restored\\n'" Enter
wait_for '│ >'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
marker="$(head -1 "${work_dir}/pane.txt" | sed -n 's/.*\(select-row-[0-9]*\).*/\1/p')"
select_row "${marker}"
wait_for 'clipboard requested (OSC 52)'
encoded="$(printf '%s' "${marker}" | base64 | tr -d '\n')"
for _ in $(seq 1 40); do grep -Fq $'\033]52;c;'"${encoded}"$'\007' "${work_dir}/osc52.raw" && break; sleep 0.1; done
grep -Fq $'\033]52;c;'"${encoded}"$'\007' "${work_dir}/osc52.raw" || fail 'missing OSC 52 clipboard request'
tmux send-keys -t "${session}" Escape
sleep 0.3
tmux send-keys -t "${session}" '/quit' Enter
wait_for osc52-shell-restored
echo "selection e2e passed (artifacts: ${work_dir})"

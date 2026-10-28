#!/usr/bin/env bash
set -euo pipefail

# Owned-screen behavior must be checked live, never via NOEL_TUI_E2E_DUMP.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
work_dir="$(mktemp -d "${e2e_root}/e2e-transcript.XXXXXX")"
session="noel-e2e-transcript-${$}"
trap 'e2e_status=$?; tmux kill-session -t "${session}" >/dev/null 2>&1 || true; e2e_isolation_cleanup "${e2e_status}"' EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
awk 'BEGIN {
  print "{\"type\":\"started\",\"session_id\":\"owned\",\"cwd\":\".\"}"
  printf "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\""
  for (i = 1; i <= 80; i++) printf "saved-answer-%03d%s", i, i == 80 ? "" : "\\n"
  print "\",\"tool_calls\":[],\"media\":[]}}"
}' > "${work_dir}/session.jsonl"
tmux new-session -d -s "${session}" -x 80 -y 20
# Capture raw terminal transitions as well as pane content for teardown checks.
tmux pipe-pane -t "${session}" -o "cat > '${work_dir}/terminal.raw'"
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && printf 'shell-scrollback-before-owned\\n'; for _ in {1..40}; do printf '\\n'; done; printf 'shell-before-owned\\n' && env -u NOEL_TUI_E2E_DUMP NOEL_DIR='${work_dir}/agent' NOEL_FAUX_STREAM_DELAY_MS=2 NOEL_FAUX_STREAM_CHARS=1 '${noel_bin}' --session '${work_dir}/session.jsonl' --model faux/echo --offline --no-splash; stty -a; printf 'shell-after-owned\\n'" Enter
capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 120); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing owned transcript marker: $1"
}
# Batched queued prompts echo as one joined response: match marker B within
# two lines after marker A (the --- separator sits between them).
wait_for_pair() {
  for _ in $(seq 1 120); do
    capture
    if grep -A 2 -F "$1" "${work_dir}/pane.txt" | grep -Fq "$2"; then return; fi
    sleep 0.1
  done
  fail "missing owned transcript marker pair: $1 / $2"
}
absent() {
  for _ in $(seq 1 30); do capture; if ! grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "unexpected marker: $1"
}
first_row() { head -n 1 "${work_dir}/pane.txt"; }
wait_for '│ >'
wait_for saved-answer-080
[ "$(tmux display-message -p -t "${session}" '#{alternate_on}')" = 1 ]
absent saved-answer-001
# The draft stays fixed when PageUp enters reading mode.
tmux send-keys -t "${session}" -l 'retained draft'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
wait_for '│ > retained draft'
reading_first="$(first_row)"
input_row="$(grep -nF '│ > retained draft' "${work_dir}/pane.txt" | cut -d: -f1)"
# Width and height resizing keeps the same logical top row.
for size in '60 14' '100 30' '80 20'; do
  read -r width height <<< "${size}"
  tmux resize-window -t "${session}" -x "${width}" -y "${height}"
  sleep 0.3
  wait_for 'Scroll to bottom'
  [ "$(first_row)" = "${reading_first}" ] || fail 'resize changed reading anchor'
  wait_for '│ > retained draft'
done
# Walk all the way to the beginning, and all the way back without clipping.
for _ in $(seq 1 10); do tmux send-keys -t "${session}" PPage; done
wait_for saved-answer-001
for _ in $(seq 1 10); do tmux send-keys -t "${session}" NPage; done
wait_for saved-answer-080
absent 'Scroll to bottom'
# SGR wheel events are handled by the app; editor history is not touched.
tmux send-keys -t "${session}" Escape '[<64;5;5M'
wait_for 'Scroll to bottom'
wait_for '│ > retained draft'
[ "$(grep -nF '│ > retained draft' "${work_dir}/pane.txt" | cut -d: -f1)" = "${input_row}" ]
tmux send-keys -t "${session}" Escape '[<65;5;5M'
wait_for saved-answer-080
absent 'Scroll to bottom'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
# Click the hint, then test Esc independently.
hint_row="$(grep -nF 'Scroll to bottom' "${work_dir}/pane.txt" | cut -d: -f1)"
tmux send-keys -t "${session}" Escape "[<0;5;${hint_row}M"
wait_for saved-answer-080
absent 'Scroll to bottom'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
tmux send-keys -t "${session}" Escape
wait_for saved-answer-080
wait_for '│ > retained draft'
absent 'Scroll to bottom'
# A selector owns Esc first; dismissing it must preserve reading and draft.
tmux send-keys -t "${session}" C-c PPage
wait_for 'Scroll to bottom'
selector_anchor="$(first_row)"
tmux send-keys -t "${session}" '/models' Enter
wait_for 'provider selector'
tmux send-keys -t "${session}" Escape
# An Escape the TUI has not read yet is taken together with the input after
# it, as a chord: go on once the selector it closes is gone.
absent '┌ provider selector'
wait_for 'Scroll to bottom'
wait_for '│ >'
[ "$(first_row)" = "${selector_anchor}" ] || fail 'selector Escape changed reading anchor'
anchor_before_reload="$(first_row)"
tmux send-keys -t "${session}" '/reload' Enter
sleep 0.4
wait_for 'Scroll to bottom'
[ "$(first_row)" = "${anchor_before_reload}" ] || fail 'reload changed reading anchor'
tmux send-keys -t "${session}" Escape
wait_for reloaded
# Large streamed answer with a live anchored reader. New output cannot move it.
awk 'BEGIN { for (i = 1; i <= 100; i++) printf "streamed-answer-%03d\n", i }' > "${work_dir}/prompt.txt"
tmux load-buffer -b "${session}" "${work_dir}/prompt.txt"
tmux paste-buffer -r -p -d -b "${session}" -t "${session}"
tmux send-keys -t "${session}" Enter
wait_for 'esc to interrupt'
tmux send-keys -t "${session}" PPage
wait_for 'Scroll to bottom'
stream_first="$(first_row)"
tmux send-keys -t "${session}" -l 'stream draft'
sleep 0.4
wait_for 'new output'
[ "$(first_row)" = "${stream_first}" ] || fail 'streaming stole reading position'
wait_for '│ > stream draft'
tmux send-keys -t "${session}" Escape
absent 'Scroll to bottom'
wait_for '│ > stream draft'
wait_for 'esc to interrupt'
# Wait for completion, checking the journal so a user prompt cannot impersonate
# the final assistant row on screen. Esc must not have interrupted the turn.
completed=''
for _ in $(seq 1 160); do
  if grep -Fq '"content":"[faux/echo] streamed-answer-001' "${work_dir}/session.jsonl"; then completed=1; break; fi
  sleep 0.1
done
[ -n "${completed}" ] || fail 'reading Esc interrupted the streamed answer'
wait_for streamed-answer-100
absent interrupted
# Visit every streamed row live, collecting windows, not exit-time output.
: > "${work_dir}/stream-windows.txt"
for _ in $(seq 1 20); do
  capture
  cat "${work_dir}/pane.txt" >> "${work_dir}/stream-windows.txt"
  tmux send-keys -t "${session}" PPage
  sleep 0.1
done
for i in $(seq 1 100); do grep -Fq "streamed-answer-$(printf '%03d' "${i}")" "${work_dir}/stream-windows.txt"; done
tmux send-keys -t "${session}" Escape
absent 'Scroll to bottom'
wait_for '│ > stream draft'
tmux send-keys -t "${session}" C-c '/clear' Enter
sleep 0.3
absent saved-answer-
absent streamed-answer-
absent 'Scroll to bottom'
wait_for '│ >'
tmux send-keys -t "${session}" PPage
absent saved-answer-
tmux send-keys -t "${session}" '/quit' Enter
wait_for shell-after-owned
[ "$(tmux display-message -p -t "${session}" '#{alternate_on}')" = 0 ]
grep -Fq shell-before-owned "${work_dir}/pane.txt"
tmux capture-pane -t "${session}" -p -S -300 > "${work_dir}/shell-history.txt"
grep -Fq shell-scrollback-before-owned "${work_dir}/shell-history.txt"
grep -Eq '(^|[ ;])icanon[ ;]' "${work_dir}/pane.txt"
grep -Eq '(^|[ ;])echo[ ;]' "${work_dir}/pane.txt"
sleep 0.1
for sequence in $'\033[?1049l' $'\033[?1000l' $'\033[?1006l' $'\033[?2004l'; do grep -Fq "${sequence}" "${work_dir}/terminal.raw"; done
# A completed direct /read ends at an unlinked tool result, not an unfinished
# model turn. Reopening it must not claim that generation was interrupted.
restart_fixture() {
  tmux kill-session -t "${session}" >/dev/null 2>&1 || true
  tmux new-session -d -s "${session}" -x 100 -y 24
  tmux send-keys -t "${session}" \
    "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_DOGFOOD_AUTO_RESTART='${3:-0}' NOEL_DOGFOOD_RESTART_EXE='${work_dir}/watched-restart' NOEL_FAUX_STREAM_DELAY_MS='${2:-0}' NOEL_FAUX_STREAM_CHARS=1 NOEL_DIR='${work_dir}/agent' '${noel_bin}' --session '$1' --session-dir '${work_dir}' --model faux/echo --offline; printf 'resume-fixture-shell\\n'" Enter
  wait_for '│ >'
}
printf 'completed-direct-read-marker\n' > "${work_dir}/direct.txt"
printf '{"type":"started","session_id":"direct","cwd":"."}\n' > "${work_dir}/direct.jsonl"
restart_fixture "${work_dir}/direct.jsonl"
tmux send-keys -t "${session}" '/read direct.txt' Enter
wait_for completed-direct-read-marker
tmux send-keys -t "${session}" '/quit' Enter
wait_for resume-fixture-shell
restart_fixture "${work_dir}/direct.jsonl"
wait_for completed-direct-read-marker
absent 'resumed mid-turn'
# Model-linked results resume automatically without a synthetic user prompt
# or replaying saved/uncertain tools. Queued prompts follow the saved turn.
cat > "${work_dir}/unfinished.jsonl" <<'JSONL'
{"type":"started","session_id":"unfinished","cwd":"."}
{"type":"message","message":{"role":"user","content":"inspect fixture","tool_calls":[],"media":[]}}
{"type":"message","message":{"role":"assistant","content":"","tool_calls":[{"id":"read-1","name":"read","arguments":"{\"path\":\"direct.txt\"}"}],"media":[]}}
{"type":"message","message":{"role":"tool","content":"saved-model-tool-result","tool_call_id":"read-1","tool_name":"read","tool_calls":[],"media":[]}}
{"type":"queued_messages_snapshot","messages":["first-resumed-followup","second-resumed-followup"]}
JSONL
restart_fixture "${work_dir}/unfinished.jsonl"
wait_for 'resumed mid-turn'
wait_for 'continuing automatically'
wait_for '[faux/echo] inspect fixture'
wait_for_pair '[faux/echo] first-resumed-followup' 'second-resumed-followup'
[ "$(grep -Fc '"role":"user"' "${work_dir}/unfinished.jsonl")" = 2 ] || fail 'resume inserted a synthetic user prompt'
grep -Fq '"type":"queued_messages_snapshot","messages":[]' "${work_dir}/unfinished.jsonl" || fail 'resume did not drain follow-ups'
[ "$(grep -Fc '"role":"tool"' "${work_dir}/unfinished.jsonl")" = 1 ] || fail 'resume reran a saved tool result'
tmux send-keys -t "${session}" '/quit' Enter
wait_for resume-fixture-shell
restart_fixture "${work_dir}/unfinished.jsonl"
wait_for_pair '[faux/echo] first-resumed-followup' 'second-resumed-followup'
absent 'resumed mid-turn'
tmux send-keys -t "${session}" '/quit' Enter
wait_for resume-fixture-shell
# /resume and the session selector trigger the same one-shot continuation.
for route in command selector select-message; do
  printf '{"type":"started","session_id":"%s","cwd":"."}\n{"type":"active_model","model":{"provider":"faux","id":"echo"}}\n{"type":"message","message":{"role":"user","content":"resume-via-%s","tool_calls":[],"media":[]}}\n' "${route}" "${route}" > "${work_dir}/${route}.jsonl"
  restart_fixture "${work_dir}/direct.jsonl"
  if [ "${route}" = command ]; then
    tmux send-keys -t "${session}" "/resume ${work_dir}/${route}.jsonl" Enter
  elif [ "${route}" = select-message ]; then
    tmux send-keys -t "${session}" '/select resume select-message' Enter
  else
    tmux send-keys -t "${session}" '/selector resume' Enter
    wait_for 'session selector'
    tmux send-keys -t "${session}" -l 'selector'
    tmux send-keys -t "${session}" Enter
  fi
  wait_for "[faux/echo] resume-via-${route}"
  [ "$(grep -Fc '"role":"user"' "${work_dir}/${route}.jsonl")" = 1 ] || fail 'in-process resume duplicated its user prompt'
  tmux send-keys -t "${session}" '/quit' Enter
  wait_for resume-fixture-shell
done
# Interrupted/quit resumed runs must not schedule themselves again. A pending
# development rebuild must not turn an intentional stop into another resume.
long_resume="resume-interruption-$(printf 'x%.0s' $(seq 1 250))"
for stop in interrupt quit; do
  printf '{"type":"started","session_id":"%s","cwd":"."}\n{"type":"message","message":{"role":"user","content":"%s","tool_calls":[],"media":[]}}\n{"type":"queued_messages_snapshot","messages":["retained-after-resume-stop"]}\n' "${stop}" "${long_resume}" > "${work_dir}/${stop}.jsonl"
  printf '#!/bin/sh\nexec sleep infinity\n' > "${work_dir}/watched-restart"
  chmod +x "${work_dir}/watched-restart"
  restart_fixture "${work_dir}/${stop}.jsonl" 20 1
  wait_for 'esc to interrupt'
  if [ "${stop}" = interrupt ]; then
    tmux send-keys -t "${session}" C-c
    wait_for interrupted
    touch "${work_dir}/watched-restart"
    sleep 2.5
    wait_for '❯ retained-after-resume-stop'
    absent 'esc to interrupt'
    absent 'resume-fixture-shell'
    tmux send-keys -t "${session}" '/quit' Enter
  else
    tmux send-keys -t "${session}" '/quit' Enter
  fi
  wait_for resume-fixture-shell
  [ "$(grep -Fc '"role":"user"' "${work_dir}/${stop}.jsonl")" = 1 ] || fail 'resumed stop drained a follow-up'
  # What the stopped reply had streamed is kept as an aborted reply; no
  # reply may have completed.
  if grep -F '"role":"assistant"' "${work_dir}/${stop}.jsonl" | grep -Fvq '"aborted":true'; then fail 'resumed stop retried generation'; fi
done
# Deterministic offline provider-construction failure: attempt once and stay
# idle, preserving both the saved prompt and follow-ups (never a paid API).
# /resume must respect a different saved model rather than carrying over faux.
printf '{"type":"started","session_id":"failed-model","cwd":"."}\n{"type":"active_model","model":{"provider":"openai","id":"gpt-5"}}\n{"type":"message","message":{"role":"user","content":"saved-offline-failure","tool_calls":[],"media":[]}}\n{"type":"queued_messages_snapshot","messages":["after-offline-error"]}\n' > "${work_dir}/failed-model.jsonl"
restart_fixture "${work_dir}/direct.jsonl"
tmux send-keys -t "${session}" "/resume ${work_dir}/failed-model.jsonl" Enter
wait_for 'offline mode only allows local faux models'
sleep 0.5
capture
[ "$(grep -Fc '● error' "${work_dir}/pane.txt")" = 1 ] || fail 'failed resume retried on idle'
[ "$(grep -Fc '"role":"user"' "${work_dir}/failed-model.jsonl")" = 1 ] || fail 'failed resume consumed follow-ups'
wait_for '❯ after-offline-error'
tmux send-keys -t "${session}" '/quit' Enter
wait_for resume-fixture-shell
# A session another noel process holds is refused in place: an error line, and
# the TUI keeps working.
printf '{"type":"started","session_id":"held","cwd":"."}\n' > "${work_dir}/held.jsonl"
mkfifo "${work_dir}/holder.in"
env NOEL_DIR="${work_dir}/agent" "${noel_bin}" --mode rpc --session "${work_dir}/held.jsonl" --model faux/echo --offline < "${work_dir}/holder.in" > /dev/null 2>&1 &
holder_pid=$!
exec 9> "${work_dir}/holder.in"
for _ in $(seq 1 100); do
  if grep -Fq '"type":"active_model"' "${work_dir}/held.jsonl"; then break; fi
  sleep 0.1
done
grep -Fq '"type":"active_model"' "${work_dir}/held.jsonl" || fail 'rpc holder did not open its session'
restart_fixture "${work_dir}/direct.jsonl"
# Wide enough that the error, which names the full journal path, fits a row.
tmux resize-window -t "${session}" -x 240 -y 24
# A resize and input that reach the TUI together can leave the input unread
# until more arrives, so the line is typed once the prompt box has been drawn
# at the new width.
wait_for "╭$(printf '─%.0s' $(seq 1 238))╮"
tmux send-keys -t "${session}" "/resume ${work_dir}/held.jsonl" Enter
wait_for "held.jsonl is in use by another noel process (pid ${holder_pid})"
tmux send-keys -t "${session}" 'after-locked-resume' Enter
wait_for '[faux/echo] after-locked-resume'
grep -Fq '"content":"[faux/echo] after-locked-resume"' "${work_dir}/direct.jsonl" || fail 'refused resume changed the open session'
tmux send-keys -t "${session}" '/quit' Enter
wait_for resume-fixture-shell
kill "${holder_pid}" 2>/dev/null || true
exec 9>&-
wait "${holder_pid}" 2>/dev/null || true
# The dogfood exec-restart reopens the session it had open with --session: the
# lock is close-on-exec, so the new image is not refused by its predecessor.
printf '#!/bin/sh\nexec %s "$@"\n' "'${noel_bin}'" > "${work_dir}/watched-restart"
chmod +x "${work_dir}/watched-restart"
restart_fixture "${work_dir}/direct.jsonl" 0 1
# /todos prints a line that is not journaled, so only a new image clears it.
tmux send-keys -t "${session}" '/todos' Enter
wait_for 'no todos'
touch "${work_dir}/watched-restart"
restarted=''
for _ in $(seq 1 150); do
  capture
  if ! grep -Fq 'no todos' "${work_dir}/pane.txt" && grep -Fq '│ >' "${work_dir}/pane.txt"; then restarted=1; break; fi
  sleep 0.1
done
[ -n "${restarted}" ] || fail 'touching the watched binary did not restart noel'
absent 'resume-fixture-shell'
tmux send-keys -t "${session}" 'after-exec-restart' Enter
wait_for '[faux/echo] after-exec-restart'
grep -Fq '"content":"[faux/echo] after-exec-restart"' "${work_dir}/direct.jsonl" || fail 'exec-restart did not reopen its session'
absent 'in use by another noel process'
tmux send-keys -t "${session}" '/quit' Enter
wait_for resume-fixture-shell
echo "owned transcript e2e passed (artifacts: ${work_dir})"

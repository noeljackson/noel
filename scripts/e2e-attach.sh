#!/usr/bin/env bash
set -euo pipefail

# The TUI attached to a session that noel server holds (`noel --attach`):
# the first attach starts the server, two TUIs follow one session as it
# streams, Esc in one aborts the run in both, a TUI that quits and attaches
# again shows the whole transcript, a killed worker is a note and then the
# run the server resumed, and an unknown session is an error on the plain
# terminal. Every pane runs on a scratch runtime directory.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
work_dir="$(mktemp -d "${e2e_root}/e2e-attach.XXXXXX")"
run_dir="${work_dir}/run"
journal="${work_dir}/agent/sessions/e2e-attach.jsonl"
first="noel-e2e-attach-a-${$}"
second="noel-e2e-attach-b-${$}"

# The pid in a server or worker record.
record_pid() { sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$1" 2>/dev/null || true; }
cleanup() {
  local status=$?
  tmux kill-session -t "${first}" >/dev/null 2>&1 || true
  tmux kill-session -t "${second}" >/dev/null 2>&1 || true
  for record in "${run_dir}"/server.json "${run_dir}"/workers/*.json; do
    pid="$(record_pid "${record}")"
    if [ -n "${pid}" ]; then kill -9 "${pid}" >/dev/null 2>&1 || true; fi
  done
  e2e_isolation_cleanup "${status}"
}
trap cleanup EXIT

capture() { tmux capture-pane -t "$1" -p -S -200 > "${work_dir}/$1.txt"; }
fail() { capture "$1"; cat "${work_dir}/$1.txt" >&2; echo "$2 (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 600); do
    capture "$1"
    if grep -Fq -- "$2" "${work_dir}/$1.txt"; then return; fi
    sleep 0.05
  done
  fail "$1" "missing attach marker: $2"
}
# Types a line and sends it once the TUI shows it as the draft.
send_line() {
  tmux send-keys -t "$1" -l "$2"
  wait_for "$1" "│ > ${2:0:40}"
  tmux send-keys -t "$1" Enter
}
# A pane's shell, in the scratch home, with the faux model streaming one
# character at a time.
start_pane() {
  tmux new-session -d -s "$1" -x 100 -y 40
  tmux send-keys -t "$1" \
    "cd '${work_dir}/cwd' && export NOEL_DIR='${work_dir}/agent' NOEL_RUNTIME_DIR='${run_dir}' NOEL_OFFLINE=1 NOEL_FAUX_STREAM_DELAY_MS=40 NOEL_FAUX_STREAM_CHARS=1 && unset NOEL_TUI_E2E_DUMP XDG_RUNTIME_DIR" Enter
}
attach() {
  tmux send-keys -t "$1" "clear; '${noel_bin}' --attach $2; printf 'noel-%s=%s\\n' exited \$?" Enter
}
quit() {
  send_line "$1" "/quit"
  wait_for "$1" "noel-exited=0"
}

mkdir -p "${work_dir}/agent" "${work_dir}/cwd"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false},"defaultProvider":"faux","defaultModel":"echo"}\n' > "${work_dir}/agent/settings.json"
# A session made by a local run, before any server.
(cd "${work_dir}/cwd" && NOEL_DIR="${work_dir}/agent" NOEL_RUNTIME_DIR="${run_dir}" "${noel_bin}" -p 'first local prompt' --model faux/echo --session-id e2e-attach --offline </dev/null) > "${work_dir}/print.txt"
grep -Fq '[faux/echo] first local prompt' "${work_dir}/print.txt"
[ ! -e "${run_dir}/server.json" ] || { echo "a server ran before the first attach" >&2; exit 1; }

# The first attach starts the server, and shows the session's transcript.
start_pane "${first}"
start_pane "${second}"
attach "${first}" e2e-attach
wait_for "${first}" "started noel server (pid"
wait_for "${first}" "[faux/echo] first local prompt"
wait_for "${first}" "· attached"
[ -S "${run_dir}/server.sock" ] || fail "${first}" "no server socket after the first attach"
attach "${second}" e2e-attach
wait_for "${second}" "[faux/echo] first local prompt"
wait_for "${second}" "· attached"

# Typing in one streams in both: the second shows the run and a partial
# reply before the reply ends.
send_line "${first}" "streamed to both panes while the reply is still being written"
wait_for "${second}" "esc to interrupt"
wait_for "${second}" "[faux/echo] stre"
capture "${second}"
if grep -Fq '[faux/echo] streamed to both panes while the reply is still being written' "${work_dir}/${second}.txt"; then
  fail "${second}" "the reply was whole before the second TUI showed it streaming"
fi
wait_for "${first}" "[faux/echo] streamed to both panes while the reply is still being written"
wait_for "${second}" "[faux/echo] streamed to both panes while the reply is still being written"
wait_for "${second}" "≡ 0 ↺"

# Esc in the second aborts the run the first started, in both.
send_line "${first}" "a slow reply that the other pane aborts with escape"
wait_for "${second}" "[faux/echo] a sl"
tmux send-keys -t "${second}" Escape
wait_for "${second}" "last run aborted"
wait_for "${first}" "last run aborted"
for pane in "${first}" "${second}"; do
  capture "${pane}"
  grep -Fxq '● aborted' "${work_dir}/${pane}.txt" || fail "${pane}" "the aborted reply is not marked aborted"
  if grep -Fq '[faux/echo] a slow reply that the other pane aborts with escape' "${work_dir}/${pane}.txt"; then
    fail "${pane}" "the aborted reply finished"
  fi
done
grep -Fq '"status":"aborted"' "${journal}" || fail "${first}" "the journal has no aborted run"

# A TUI that quits and attaches again shows the whole transcript.
quit "${second}"
attach "${second}" e2e-attach
wait_for "${second}" "· attached"
capture "${second}"
for marker in "first local prompt" "[faux/echo] first local prompt" "[faux/echo] streamed to both panes while the reply is still being written" "a slow reply that the other pane aborts with escape" "● aborted"; do
  grep -Fq -- "${marker}" "${work_dir}/${second}.txt" || fail "${second}" "the reattached transcript lacks: ${marker}"
done
if grep -Fq "started noel server" "${work_dir}/${second}.txt"; then
  fail "${second}" "a second server started"
fi

# A worker killed mid-run: both show a note, then the run the server
# resumed, to its end.
worker="$(record_pid "${run_dir}/workers/e2e-attach.json")"
send_line "${first}" "the worker is killed while this streams"
wait_for "${second}" "[faux/echo] the w"
kill -9 "${worker}"
for pane in "${first}" "${second}"; do
  wait_for "${pane}" "lost the session's worker"
  wait_for "${pane}" "redrawn from its snapshot"
  wait_for "${pane}" "[faux/echo] the worker is killed while this streams"
  wait_for "${pane}" "≡ 0 ↺"
done
grep -Fq '"generation":2' "${run_dir}/workers/e2e-attach.json" || fail "${first}" "the worker was not started again"
grep -Fq "resuming run" "${run_dir}/server.log" || fail "${first}" "the server did not resume the run"

# Commands with no meaning for a session in a server are refused; a model
# change is the session's.
send_line "${first}" "/resume"
wait_for "${first}" "/resume is not available when attached"
send_line "${first}" "/model faux/script"
wait_for "${second}" "faux/script off"
send_line "${first}" "/model faux/echo"
wait_for "${second}" "faux/echo off"

# An unknown session is an error on the plain terminal.
quit "${second}"
attach "${second}" no-such-session
wait_for "${second}" "noel-exited=1"
capture "${second}"
grep -Fq "no session no-such-session on noel server" "${work_dir}/${second}.txt" || fail "${second}" "no clear error for an unknown session"
[ "$(tmux display-message -p -t "${second}" '#{alternate_on}')" = 0 ] || fail "${second}" "the alternate screen stayed on"

quit "${first}"
echo "attach e2e passed (artifacts: ${work_dir})"

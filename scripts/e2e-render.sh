#!/usr/bin/env bash
set -euo pipefail

# One faux script, rendered two ways (docs/client-tui-plan.md, slice 4): live,
# as the TUI streams it from the session's view, and reopened, as the TUI
# rebuilds it from the saved entries (plain noel, and --attach). The
# transcripts must match.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
work_dir="$(mktemp -d "${e2e_root}/e2e-render.XXXXXX")"
session="noel-e2e-render-${$}"
cleanup() {
  local status=$?
  tmux kill-session -t "${session}" >/dev/null 2>&1 || true
  e2e_isolation_cleanup "${status}"
}
trap cleanup EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
printf 'render-file-line-1\nrender-file-line-2\n' > "${work_dir}/notes.txt"
cat > "${work_dir}/script.json" <<'JSON'
[
  [{"thinking": "I should look at the notes."}, {"text": "Reading them."},
   {"tool_call": {"name": "bash", "arguments": {"command": "cat notes.txt"}}}],
  [{"text": "The notes say **two lines**:\n\n- render-file-line-1\n- render-file-line-2"}],
  [{"text": "Second answer."}]
]
JSON
printf '{"type":"started","session_id":"render","cwd":"."}\n' > "${work_dir}/render.jsonl"
capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2 || true; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 150); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing render marker: $1"
}
# Runs noel (with extra arguments) in a new tmux session, the transcript
# written to dump-<name>.txt when it exits.
start() {
  local name="$1"; shift
  tmux new-session -d -s "${session}" -x 120 -y 40
  tmux send-keys -t "${session}" \
    "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_TUI_E2E_DUMP_FILE='${work_dir}/dump-${name}.txt' NOEL_DIR='${work_dir}/agent' NOEL_FAUX_SCRIPT='${work_dir}/script.json' NOEL_FAUX_STREAM_DELAY_MS=2 NOEL_FAUX_STREAM_CHARS=3 '${noel_bin}' $*; printf 'render-%s-exited\\n' done" Enter
  wait_for '│ >'
}
stop() {
  tmux send-keys -t "${session}" '/quit' Enter
  wait_for 'render-done-exited'
  tmux kill-session -t "${session}" >/dev/null 2>&1 || true
}
# The transcript without the greeting, which says how the TUI was opened.
body() { awk 'found || /^user> /{found=1} found' "$1" | sed -e 's/[[:space:]]*$//'; }

start live --offline --session "'${work_dir}/render.jsonl'" --model faux/script
tmux send-keys -t "${session}" 'look at the notes' Enter
wait_for 'render-file-line-2'
wait_for 'two lines'
tmux send-keys -t "${session}" 'and again' Enter
wait_for 'Second answer.'
stop
[ -s "${work_dir}/dump-live.txt" ] || fail 'no live transcript'

start reopened --offline --session "'${work_dir}/render.jsonl'"
wait_for 'Second answer.'
stop

start attached --attach render
wait_for 'Second answer.'
stop

for name in live reopened attached; do body "${work_dir}/dump-${name}.txt" > "${work_dir}/body-${name}.txt"; done
[ -s "${work_dir}/body-live.txt" ] || { cat "${work_dir}/dump-live.txt" >&2; fail 'the live transcript has no turns'; }
for expected in 'I should look at the notes.' 'Reading them.' 'cat notes.txt' 'render-file-line-1' 'two lines' 'Second answer.'; do
  grep -Fq "${expected}" "${work_dir}/body-live.txt" || { cat "${work_dir}/body-live.txt" >&2; fail "the live transcript lacks ${expected}"; }
done
for name in reopened attached; do
  diff -u "${work_dir}/body-live.txt" "${work_dir}/body-${name}.txt" >&2 \
    || fail "the ${name} transcript differs from the live one"
done
echo "render e2e passed (artifacts: ${work_dir})"

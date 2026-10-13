#!/usr/bin/env bash
set -euo pipefail

# First-run import e2e: noel started in a home that has only pi's ~/.pi/agent
# imports it into ~/.noel, says so, and the imported session is listed by the
# /resume picker and opens from it.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
# Anchor to an absolute path so the cd inside tmux cannot break the lookup.
case "${noel_bin}" in
  /*) ;;
  *) noel_bin="${repo_root}/${noel_bin}" ;;
esac
work_dir="$(mktemp -d "${e2e_root}/e2e-import.XXXXXX")"
home="${work_dir}/home"
session="noel-e2e-import-${$}"
trap 'e2e_status=$?; tmux kill-session -t "${session}" >/dev/null 2>&1 || true; e2e_isolation_cleanup "${e2e_status}"' EXIT
mkdir -p "${home}/.pi/agent" "${work_dir}/project"
printf '{"modelRefresh":{"enabled":false}}\n' > "${home}/.pi/agent/settings.json"
# A session in pi's format, written while noel used pi's directory.
(cd "${work_dir}/project" && env HOME="${home}" NOEL_DIR="${home}/.pi/agent" \
  "${noel_bin}" -p --offline --model faux/echo 'topic-from-pi' > /dev/null)
journals=("${home}/.pi/agent/sessions/"*.jsonl)
imported_session="$(basename "${journals[0]}" .jsonl)"

# -J joins wrapped lines: the import notice names two long paths.
capture() { tmux capture-pane -J -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 150); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing marker: $1"
}

tmux new-session -d -s "${session}" -x 100 -y 30
tmux send-keys -t "${session}" \
  "cd '${work_dir}/project' && env -u NOEL_DIR HOME='${home}' '${noel_bin}' --model faux/echo --offline; printf 'noel-exited\\n'" Enter
wait_for '│ >'
[ -d "${home}/.noel/sessions" ] || fail 'the first run did not import ~/.pi/agent'
# The picker lists the imported session by its first prompt; filtering picks
# it over the session this run started, and Enter resumes it once the picker
# shows the filter.
tmux send-keys -t "${session}" '/resume' Enter
wait_for 'topic-from-pi'
tmux send-keys -t "${session}" 'topic-from'
wait_for 'filter topic-from'
tmux send-keys -t "${session}" Enter
wait_for "session: ${imported_session}"
tmux send-keys -t "${session}" '/quit' Enter
wait_for 'noel-exited'
grep -Fq "noel: imported settings.json, sessions from ${home}/.pi/agent into ${home}/.noel" \
  "${work_dir}/pane.txt" || fail 'the import notice is missing'

echo "owned import e2e passed (artifacts: ${work_dir})"

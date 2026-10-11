#!/usr/bin/env bash
set -euo pipefail

# Live queue dispatch against a delayed faux provider, without transcript dumps.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
pi_bin="${PI_BIN:-${repo_root}/target/debug/pi}"
work_dir="$(mktemp -d "${e2e_root}/e2e-queue.XXXXXX")"
session="pi-e2e-queue-${$}"
trap 'e2e_status=$?; tmux kill-session -t "${session}" >/dev/null 2>&1 || true; e2e_isolation_cleanup "${e2e_status}"' EXIT

capture() { tmux capture-pane -t "${session}" -p > "${case_dir}/pane.txt"; }
fail() { capture; cat "${case_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 100); do
    capture
    if grep -Fq "$1" "${case_dir}/pane.txt"; then return; fi
    sleep 0.1
  done
  fail "missing live queue marker: $1"
}
wait_record() {
  for _ in $(seq 1 100); do
    if grep -Fq "\"type\":\"message\",\"message\":{\"role\":\"$1\",\"content\":\"$2\"" "${journal}"; then return; fi
    sleep 0.1
  done
  fail "missing persisted $1 message: $2"
}
send() { tmux send-keys -t "${session}" -l "$1"; tmux send-keys -t "${session}" Enter; }
# Fixtures are plain ASCII, allowing exact journal checks without new tools.
snapshot_is() {
  grep -F '"type":"queued_messages_snapshot"' "${journal}" | tail -n 1 | \
    grep -Fxq "{\"type\":\"queued_messages_snapshot\",\"messages\":$1}"
}
not_sent() {
  ! grep -Fq "\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"$1\"" "${journal}"
}

initial="initial-$(printf 'x%.0s' $(seq 1 80))"
first="first-$(printf 'y%.0s' $(seq 1 100))"
for mode in default legacy-setting; do
  for view in owned inline; do
    case_dir="${work_dir}/${mode}-${view}"
    mkdir -p "${case_dir}/agent" "${case_dir}/sessions"
    # Legacy followUpMode values are still accepted but ignored: queued
    # prose always batches into one combined turn now.
    setting=''
    if [ "${mode}" != default ]; then setting=",\"followUpMode\":\"one-at-a-time\""; fi
    printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}%s}\n' "${setting}" > "${case_dir}/agent/settings.json"
    args=''
    if [ "${view}" = inline ]; then args='--no-alt-screen'; fi
    tmux new-session -d -s "${session}" -x 100 -y 30
    tmux send-keys -t "${session}" \
      "cd '${case_dir}' && env -u PI_TUI_E2E_DUMP PI_CODING_AGENT_DIR='${case_dir}/agent' PI_FAUX_STREAM_DELAY_MS=12 PI_FAUX_STREAM_CHARS=1 '${pi_bin}' --session-dir '${case_dir}/sessions' --model faux/echo --offline ${args}" Enter
    wait_for '│ >'
    journal="$(find "${case_dir}/sessions" -name '*.jsonl' -print -quit)"

    send "${initial}"
    wait_record user "${initial}"
    send "${first}"
    send 'second-followup'
    wait_for '❯ second-followup'
    # This must arrive inside an automatically dispatched turn, not a new
    # manual submission that accidentally wakes a stranded queue. The batch
    # is one user message: first + separator + second-followup.
    wait_record user "${first}\\n---\\nsecond-followup"
    send 'arrived-during-followup'
    wait_for '❯ arrived-during-followup'
    tmux send-keys -t "${session}" -l 'unsent draft'
    wait_record assistant '[faux/echo] arrived-during-followup'
    wait_for '│ > unsent draft'
    for _ in $(seq 1 50); do
      capture
      if ! grep -Fq 'esc to interrupt' "${case_dir}/pane.txt" && ! grep -Fq '❯ ' "${case_dir}/pane.txt"; then break; fi
      sleep 0.1
    done
    if grep -Fq 'esc to interrupt' "${case_dir}/pane.txt" || grep -Fq '❯ ' "${case_dir}/pane.txt"; then fail 'completed queue remained pending'; fi
    # Verify batching/exactly-once: queued prose drains as ONE combined
    # user message joined with the steering separator, and the durable
    # queue snapshot ends empty.
    grep -F '"type":"message","message":{"role":"user"' "${journal}" | \
      sed 's/.*"content":"\([^"]*\)".*/\1/' > "${case_dir}/actual-prompts.txt"
    {
      printf '%s\n' "${initial}"
      # first + second-followup batch into one message; a prompt queued
      # during that combined turn dispatches as its own follow-up after it.
      printf '%s\\n---\\n%s\n' "${first}" 'second-followup'
      printf '%s\n' 'arrived-during-followup'
    } > "${case_dir}/expected-prompts.txt"
    cmp -s "${case_dir}/actual-prompts.txt" "${case_dir}/expected-prompts.txt" || fail 'queue did not batch/exactly once'
    snapshot_is '[]' || fail 'completed queue was not persisted empty'

    # An interrupted turn must leave follow-ups pending and recallable.
    tmux send-keys -t "${session}" C-c
    send "${initial}-interrupt"
    wait_record user "${initial}-interrupt"
    send 'after-interruption'
    wait_for '❯ after-interruption'
    tmux send-keys -t "${session}" C-c
    wait_for 'interrupted'
    sleep 0.2
    snapshot_is '["after-interruption"]' && not_sent 'after-interruption' || fail 'interruption consumed a pending follow-up'
    tmux send-keys -t "${session}" Up
    wait_for '│ > after-interruption'
    tmux send-keys -t "${session}" C-c

    # A failed queued prompt stops the chain without consuming later work.
    send '/queue /read missing-queue-fixture.txt'
    send '/queue after-error'
    send 'trigger-error'
    wait_for '● error'
    snapshot_is '["after-error"]' && not_sent 'after-error' || fail 'error consumed later queued work'
    send '/queue-clear'

    # Quitting mid-turn journals pending work without dispatching it.
    send "${initial}-quit"
    wait_record user "${initial}-quit"
    send 'after-quit'
    wait_for '❯ after-quit'
    send '/quit'
    for _ in $(seq 1 50); do
      if snapshot_is '["after-quit"]'; then break; fi
      sleep 0.1
    done
    sleep 0.2
    snapshot_is '["after-quit"]' && not_sent 'after-quit' || fail 'quit consumed a pending follow-up'
    tmux kill-session -t "${session}" >/dev/null 2>&1 || true
    # Reopen an unfinished turn: finish saved context, then drain pending work.
    tmux new-session -d -s "${session}" -x 100 -y 30
    tmux send-keys -t "${session}" \
      "cd '${case_dir}' && env -u PI_TUI_E2E_DUMP PI_CODING_AGENT_DIR='${case_dir}/agent' '${pi_bin}' --session '${journal}' --offline ${args}" Enter
    wait_for 'continuing automatically'
    wait_record assistant "[faux/echo] ${initial}-quit"
    wait_record assistant '[faux/echo] after-quit'
    for _ in $(seq 1 50); do if snapshot_is '[]'; then break; fi; sleep 0.1; done
    snapshot_is '[]' || fail 'resumed turn did not drain the persisted queue'
    [ "$(grep -Fc "\"role\":\"user\",\"content\":\"${initial}-quit\"" "${journal}")" = 1 ] || fail 'resume duplicated the interrupted prompt'
    send '/quit'
    sleep 0.2
    tmux kill-session -t "${session}" >/dev/null 2>&1 || true
  done
done
echo "queue e2e passed (artifacts: ${work_dir})"

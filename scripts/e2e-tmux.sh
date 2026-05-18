#!/usr/bin/env bash
set -euo pipefail

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required for TTY e2e tests" >&2
  exit 127
fi

cargo_bin="$(command -v cargo)"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
session_name="pi-e2e-${$}"
session_dir="${repo_root}/target/e2e-tmux-sessions"
work_dir="${repo_root}/target/e2e-tmux-work"
agent_dir="${work_dir}/agent"
prompt_file="${work_dir}/prompt-ready"

cleanup() {
  tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-one-follow-${$}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-disabled-${$}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

rm -rf "${session_dir}" "${work_dir}"
mkdir -p "${session_dir}" "${work_dir}" "${agent_dir}/skills" "${agent_dir}/prompts" "${agent_dir}/themes" "${agent_dir}/extensions"
printf 'review this\n' > "${agent_dir}/skills/review.md"
printf 'fix {{input}}\n' > "${agent_dir}/prompts/fix.md"
printf '{"name":"dark"}\n' > "${agent_dir}/themes/dark.json"
printf 'extension says {{input}}\n' > "${agent_dir}/extensions/assist.md"
cat > "${agent_dir}/extensions/exec-ext" <<'SH'
#!/bin/sh
input="$(cat)"
printf 'exec-ext saw %s\n' "${input}"
SH
chmod +x "${agent_dir}/extensions/exec-ext"
cat > "${agent_dir}/extensions/json-ext" <<'SH'
#!/bin/sh
request="$(cat)"
input="$(printf '%s' "${request}" | sed -n 's/.*"input":"\([^"]*\)".*/\1/p')"
printf '{"output":"json-ext saw %s"}\n' "${input}"
SH
chmod +x "${agent_dir}/extensions/json-ext"
printf '{"protocol":"json"}\n' > "${agent_dir}/extensions/json-ext.pi-extension.json"
printf 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=' | base64 -d > "${work_dir}/pixel.png"

(cd "${repo_root}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q -p pi-cli -- config disable extension assist) > "${work_dir}/config-disable.txt"
(cd "${repo_root}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q -p pi-cli -- config show) > "${work_dir}/config-disabled.txt"
grep -Fq "disabled extensions: assist" "${work_dir}/config-disabled.txt"
(cd "${repo_root}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q -p pi-cli -- config enable extension assist) > "${work_dir}/config-enable.txt"
(cd "${repo_root}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q -p pi-cli -- config show) > "${work_dir}/config-enabled.txt"
grep -Fq "disabled extensions: -" "${work_dir}/config-enabled.txt"

tmux new-session -d -s "${session_name}" -x 100 -y 30
tmux send-keys -t "${session_name}" \
  "cd '${repo_root}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' PI_CLIPBOARD_COMMAND='cat > ${work_dir}/clipboard.txt' PI_EDITOR_COMMAND='printf editor-prompt > {file}' '${cargo_bin}' run -q -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
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
  echo "pi prompt did not appear" >&2
  exit 1
fi

tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/live-pane.txt"
if ! grep -Fq "conversation" "${work_dir}/live-pane.txt"; then
  cat "${work_dir}/live-pane.txt" >&2
  echo "TUI conversation pane did not appear" >&2
  exit 1
fi
if ! grep -Fq "pi>" "${work_dir}/live-pane.txt"; then
  cat "${work_dir}/live-pane.txt" >&2
  echo "TUI input prompt did not appear" >&2
  exit 1
fi

send_line() {
  tmux send-keys -t "${session_name}" "$1" Enter
  sleep 0.35
}

send_enter() {
  tmux send-keys -t "${session_name}" Enter
  sleep 0.35
}

send_line "/session"
send_line "hello from tmux e2e"
send_line "/complete /mo"
send_line "/status"
send_line "/editor draft"
send_line "/history"
send_line "/image target/e2e-tmux-work/pixel.png describe image"
send_line "/write target/e2e-tmux-work/file.txt e2e-ok"
send_line "/read target/e2e-tmux-work/file.txt"
send_line "/reload"
send_line "/queue queued followup"
send_line "after reload"
send_line "/queue"
send_line "/queue queued cancel"
send_line "/queue"
send_line "/interrupt"
send_line "! printf shell-ok"
send_line "!!"
send_line "/session"
send_line "/changelog"
send_line "/name tmux-session"
send_line "/labels e2e tty"
send_line "/settings"
send_enter
tmux send-keys -t "${session_name}" Escape
sleep 0.35
send_line "/settings show"
send_line "/diagnostics"
send_line "/skills"
send_line "/skill:review skill input"
send_line "/extensions"
send_line "/extension:assist extension input"
send_line "/extension:exec-ext executable input"
send_line "/extension:json-ext protocol input"
send_line "/prompts"
send_line "/prompt fix broken thing"
send_line "/themes"
send_line "/select theme 1"
send_line "/selector theme"
send_enter
send_line "/theme dark"
send_line "/scoped-models"
send_enter
send_line "/selector model"
send_enter
send_line "/select model 1"
send_line "/model"
send_enter
send_line "/model 1"
send_line "/hotkeys"
send_line "/copy"
send_line "/export target/e2e-tmux-work/session-export.json"
send_line "/export target/e2e-tmux-work/session-export.jsonl"
send_line "/export target/e2e-tmux-work/session-export.html"
send_line "/share target/e2e-tmux-work/session-share.html"
send_line "/clone"
send_line "/session"
send_line "/resume 1"
send_line "/session"
send_line "/import target/e2e-tmux-work/session-export.json"
send_line "/session"
send_line "/import target/e2e-tmux-work/session-export.jsonl"
send_line "/session"
send_line "/fork"
send_line "/session"
send_line "/summaries"
send_line "/tree"
send_line "/resume"
send_line "/compact"
send_line "/summaries"
send_line "/multiline"
send_line "multi one"
send_line "multi two"
send_line "."
send_line "/delete"
send_line "/session"
send_line "/quit"
sleep 0.5

pane_output="$(tmux capture-pane -t "${session_name}" -p -S -2000 2>/dev/null || true)"
printf '%s\n' "${pane_output}" > "${work_dir}/pane.txt"

require_output() {
  local expected="$1"
  if ! grep -Fq "${expected}" "${work_dir}/pane.txt"; then
    cat "${work_dir}/pane.txt" >&2
    echo "missing expected pane output: ${expected}" >&2
    exit 1
  fi
}

require_output "pi rust cli"
require_output "assistant>"
require_output "user>"
require_output "[faux/echo] hello from tmux e2e"
require_output "/model <provider/id>"
require_output "status"
require_output "[faux/echo] editor-prompt"
require_output "hello from tmux e2e"
require_output "image/png"
require_output "1x1"
require_output "[faux/echo] describe image [media:1]"
require_output "wrote ${repo_root}/target/e2e-tmux-work/file.txt"
require_output "e2e-ok"
require_output "reloaded"
require_output "queued: 1"
require_output "[faux/echo] after reload"
require_output "queued> queued followup"
require_output "[faux/echo] queued followup"
require_output "queue is empty"
require_output "queued cancel"
require_output "interrupted; cleared 1 queued message(s)"
require_output "shell-ok"
require_output "What's New"
require_output "No changelog entries found."
require_output "name: tmux-session"
require_output "labels: e2e, tty"
require_output "settings selector"
require_output "setting compaction.enabled: off"
require_output "agent dir:"
require_output "no diagnostics"
require_output "review"
require_output "review this"
require_output "skill input"
require_output "assist"
require_output "extension says {{input}}"
require_output "extension input"
require_output "exec-ext saw executable input"
require_output "json-ext saw protocol input"
require_output "fix"
require_output "fix broken thing"
require_output "theme: dark"
require_output "theme selector"
require_output "model selector"
require_output "model: faux/echo"
require_output "submit"
require_output "reload"
require_output "[faux/echo] after reload"
require_output "copied to clipboard via cat > ${work_dir}/clipboard.txt"
require_output "exported target/e2e-tmux-work/session-export.json"
require_output "exported target/e2e-tmux-work/session-export.jsonl"
require_output "exported target/e2e-tmux-work/session-export.html"
require_output "share exported target/e2e-tmux-work/session-share.html"
require_output "parent:"
require_output "Branched from"
require_output "compacted:"
require_output "compaction Manual"
require_output "enter multiline prompt"
require_output "[faux/echo] multi one"
require_output "multi two"
require_output "deleted ${session_dir}/"

mapfile -t session_lines < <(grep -E '^(system> )?session: ' "${work_dir}/pane.txt" | sed -E 's/^system> //')
if [ "${#session_lines[@]}" -lt 15 ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "expected at least fifteen session lines" >&2
  exit 1
fi

first_session="${session_lines[0]#session: }"
second_session="${session_lines[1]#session: }"
clone_session="${session_lines[4]#session: }"
resumed_session="${session_lines[6]#session: }"
imported_session="${session_lines[8]#session: }"
jsonl_imported_session="${session_lines[10]#session: }"
fork_session="${session_lines[12]#session: }"
post_delete_session="${session_lines[14]#session: }"
if [ "${first_session}" != "${second_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "session changed across reload: ${first_session} != ${second_session}" >&2
  exit 1
fi
if [ "${clone_session}" = "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "clone did not create a new session: ${clone_session}" >&2
  exit 1
fi
if [ "${resumed_session}" != "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "numbered resume did not return to first session: ${resumed_session} != ${first_session}" >&2
  exit 1
fi
if [ "${imported_session}" != "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "import did not restore exported session id: ${imported_session} != ${first_session}" >&2
  exit 1
fi
if [ "${jsonl_imported_session}" != "${first_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "jsonl import did not restore exported session id: ${jsonl_imported_session} != ${first_session}" >&2
  exit 1
fi
if [ "${first_session}" = "${fork_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "fork did not create a new session: ${first_session}" >&2
  exit 1
fi
if [ "${post_delete_session}" = "${fork_session}" ]; then
  cat "${work_dir}/pane.txt" >&2
  echo "delete did not replace current session: ${post_delete_session}" >&2
  exit 1
fi

session_log="${session_dir}/${first_session}.jsonl"
if [ ! -f "${session_log}" ]; then
  echo "missing session log: ${session_log}" >&2
  exit 1
fi

grep -Fq "hello from tmux e2e" "${session_log}"
grep -Fq "after reload" "${session_log}"
grep -Fq "queued followup" "${session_log}"
grep -Fq "e2e-ok" "${session_log}"
if grep -Fq "shell-ok" "${session_log}"; then
  echo "excluded shell output entered session log" >&2
  exit 1
fi

clone_log="${session_dir}/${clone_session}.jsonl"
if [ ! -f "${clone_log}" ]; then
  echo "missing clone log: ${clone_log}" >&2
  exit 1
fi
grep -Fq "tmux-session" "${clone_log}"

fork_log="${session_dir}/${fork_session}.jsonl"
if [ -f "${fork_log}" ]; then
  echo "deleted fork log still exists: ${fork_log}" >&2
  exit 1
fi

if [ ! -f "${work_dir}/session-export.json" ]; then
  echo "missing exported session" >&2
  exit 1
fi
grep -Fq "${first_session}" "${work_dir}/session-export.json"
if [ ! -f "${work_dir}/session-export.jsonl" ]; then
  echo "missing exported jsonl session" >&2
  exit 1
fi
grep -Fq "${first_session}" "${work_dir}/session-export.jsonl"
if [ ! -f "${work_dir}/session-export.html" ]; then
  echo "missing exported html session" >&2
  exit 1
fi
grep -Fq "pi session export" "${work_dir}/session-export.html"
if [ ! -f "${work_dir}/session-share.html" ]; then
  echo "missing shared html session" >&2
  exit 1
fi
grep -Fq "pi session export" "${work_dir}/session-share.html"
if [ ! -f "${work_dir}/clipboard.txt" ]; then
  echo "missing clipboard capture" >&2
  exit 1
fi
grep -Fq "[faux/echo] fix broken thing" "${work_dir}/clipboard.txt"
grep -Fq '"enabled": false' "${agent_dir}/settings.json"

printf '{"followUpMode":"one-at-a-time"}\n' > "${agent_dir}/settings.json"
one_follow_session="pi-e2e-one-follow-${$}"
tmux new-session -d -s "${one_follow_session}" -x 100 -y 20
tmux send-keys -t "${one_follow_session}" \
  "cd '${repo_root}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${one_follow_session}" -p -S -2000 | grep -q "pi>"; then
    break
  fi
  sleep 0.25
done

tmux send-keys -t "${one_follow_session}" "/queue first-one-at-a-time" Enter
sleep 0.35
tmux send-keys -t "${one_follow_session}" "/queue second-one-at-a-time" Enter
sleep 0.35
tmux send-keys -t "${one_follow_session}" "trigger one followup" Enter
sleep 0.8
tmux send-keys -t "${one_follow_session}" "/queue" Enter
sleep 0.35
tmux send-keys -t "${one_follow_session}" "/quit" Enter
sleep 0.5
one_follow_output="$(tmux capture-pane -t "${one_follow_session}" -p -S -2000 2>/dev/null || true)"
printf '%s\n' "${one_follow_output}" > "${work_dir}/one-follow-pane.txt"
tmux kill-session -t "${one_follow_session}" >/dev/null 2>&1 || true

if ! grep -Fq "[faux/echo] first-one-at-a-time" "${work_dir}/one-follow-pane.txt"; then
  cat "${work_dir}/one-follow-pane.txt" >&2
  echo "one-at-a-time follow-up did not process the first queued message" >&2
  exit 1
fi
if ! grep -Fq "second-one-at-a-time" "${work_dir}/one-follow-pane.txt"; then
  cat "${work_dir}/one-follow-pane.txt" >&2
  echo "one-at-a-time follow-up did not leave the second queued message visible" >&2
  exit 1
fi
if grep -Fq "[faux/echo] second-one-at-a-time" "${work_dir}/one-follow-pane.txt"; then
  cat "${work_dir}/one-follow-pane.txt" >&2
  echo "one-at-a-time follow-up drained more than one queued message" >&2
  exit 1
fi

disabled_session="pi-e2e-disabled-${$}"
tmux new-session -d -s "${disabled_session}" -x 100 -y 20
tmux send-keys -t "${disabled_session}" \
  "cd '${repo_root}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q -p pi-cli -- --session-dir '${session_dir}' --model faux/echo --no-tools" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${disabled_session}" -p -S -2000 | grep -q "pi>"; then
    break
  fi
  sleep 0.25
done

tmux send-keys -t "${disabled_session}" "/write target/e2e-tmux-work/blocked.txt blocked" Enter
sleep 0.5
tmux send-keys -t "${disabled_session}" "/quit" Enter
sleep 0.5
disabled_output="$(tmux capture-pane -t "${disabled_session}" -p -S -2000 2>/dev/null || true)"
printf '%s\n' "${disabled_output}" > "${work_dir}/disabled-pane.txt"
tmux kill-session -t "${disabled_session}" >/dev/null 2>&1 || true

if ! grep -Fq "tool is disabled: write" "${work_dir}/disabled-pane.txt"; then
  cat "${work_dir}/disabled-pane.txt" >&2
  echo "disabled tool smoke did not fail as expected" >&2
  exit 1
fi
if [ -f "${work_dir}/blocked.txt" ]; then
  echo "disabled tool unexpectedly wrote blocked.txt" >&2
  exit 1
fi

echo "tmux e2e passed: ${first_session}"

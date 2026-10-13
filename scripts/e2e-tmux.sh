#!/usr/bin/env bash
set -euo pipefail

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required for TTY e2e tests" >&2
  exit 127
fi

cargo_bin="$(command -v cargo)"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
session_name="pi-e2e-${$}"
session_dir="${e2e_root}/e2e-tmux-sessions"
work_dir="${e2e_root}/e2e-tmux-work"
cwd_dir="${work_dir}/cwd"
agent_dir="${work_dir}/agent"
prompt_file="${work_dir}/prompt-ready"

cleanup() {
  local status=$?
  tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-clear-${$}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-one-follow-${$}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-disabled-${$}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-stream-${$}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-modern-${$}" >/dev/null 2>&1 || true
  tmux kill-session -t "pi-e2e-input-${$}" >/dev/null 2>&1 || true
  e2e_isolation_cleanup "${status}"
}
trap cleanup EXIT

rm -rf "${session_dir}" "${work_dir}"
mkdir -p "${session_dir}" "${cwd_dir}" "${agent_dir}/skills" "${agent_dir}/prompts" "${agent_dir}/themes" "${agent_dir}/extensions"
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
kind="$(printf '%s' "${request}" | sed -n 's/.*"kind":"\([^"]*\)".*/\1/p')"
input="$(printf '%s' "${request}" | sed -n 's/.*"input":"\([^"]*\)".*/\1/p')"
if [ "${kind}" != "command" ] && [ -n "${PI_EXTENSION_EVENTS_LOG:-}" ]; then
  printf '%s\n' "${kind}" >> "${PI_EXTENSION_EVENTS_LOG}"
fi
printf '{"output":"json-ext saw %s"}\n' "${input}"
SH
chmod +x "${agent_dir}/extensions/json-ext"
printf '{"protocol":"json"}\n' > "${agent_dir}/extensions/json-ext.pi-extension.json"
printf 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=' | base64 -d > "${work_dir}/pixel.png"

(cd "${cwd_dir}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q --manifest-path "${repo_root}/Cargo.toml" -p pi-cli -- config disable extension assist) > "${work_dir}/config-disable.txt"
(cd "${cwd_dir}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q --manifest-path "${repo_root}/Cargo.toml" -p pi-cli -- config show) > "${work_dir}/config-disabled.txt"
grep -Fq "disabled extensions: assist" "${work_dir}/config-disabled.txt"
(cd "${cwd_dir}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q --manifest-path "${repo_root}/Cargo.toml" -p pi-cli -- config enable extension assist) > "${work_dir}/config-enable.txt"
(cd "${cwd_dir}" && PI_CODING_AGENT_DIR="${agent_dir}" "${cargo_bin}" run -q --manifest-path "${repo_root}/Cargo.toml" -p pi-cli -- config show) > "${work_dir}/config-enabled.txt"
grep -Fq "disabled extensions: -" "${work_dir}/config-enabled.txt"

tmux new-session -d -s "${session_name}" -x 100 -y 30
tmux send-keys -t "${session_name}" \
  "cd '${cwd_dir}' && env -u NO_COLOR PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' PI_EXTENSION_EVENTS_LOG='${work_dir}/extension-events.txt' PI_CLIPBOARD_COMMAND='cat > ${work_dir}/clipboard.txt' PI_CLIPBOARD_IMAGE_COMMAND='cat ${work_dir}/pixel.png' PI_EDITOR_COMMAND='printf editor-prompt > {file}' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${session_name}" -p -S -2000 | grep -q "│ >"; then
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
if ! grep -Fq "pi rust cli" "${work_dir}/live-pane.txt"; then
  cat "${work_dir}/live-pane.txt" >&2
  echo "TUI transcript did not appear" >&2
  exit 1
fi
if ! grep -Fq "│ >" "${work_dir}/live-pane.txt"; then
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

# The idle footer shows the active model with a drained durable queue
# ("≡ 0" — queued messages count) and no "esc to interrupt". ↺ is prompt
# history and never gates submission; ≡ N > 0 means Enter will queue the
# draft behind pending work instead of submitting it.
last_line_is_idle_footer() {
  tail -n 1 "$1" | grep -Eq '^[a-z0-9/._@-]+ .*(≡ 0|≡ -)' \
    && ! tail -n 1 "$1" | grep -Fq 'esc to interrupt'
}

# The main test process unsets NO_COLOR inherited from a shared tmux server.
# Exercise real SGR transitions, including removal of fixed light/dark colors.
require_live_color() {
  local expected="$1"
  local output="$2"
  tmux capture-pane -t "${session_name}" -p -e > "${work_dir}/${output}.txt"
  if ! grep -Fq "${expected}" "${work_dir}/${output}.txt"; then
    cat "${work_dir}/${output}.txt" >&2
    echo "missing terminal color ${expected}" >&2
    exit 1
  fi
}

send_line "/theme light"
require_live_color "38;2;36;41;50" "theme-light"
send_line "/theme dark"
require_live_color "38;2;228;231;236" "theme-dark"
send_line "/theme kimi"
require_live_color "38;2;37;99;235" "theme-kimi"
send_line "/accent magenta"
require_live_color "38;5;5m" "theme-accent"
send_line "/reload"
require_live_color "38;5;5m" "theme-reload"
# Markdown emphasis in assistant entries paints real SGR: bold runs emit
# the bold modifier transition around **marked** text.
send_line 'show **bold** and *italic* and `code`'
sleep 0.6
require_live_color "[1m" "markdown-bold"
grep -Fq '"theme": "kimi"' "${agent_dir}/settings.json"
grep -Fq '"accentColor": "magenta"' "${agent_dir}/settings.json"
send_line "/accent auto"
send_line "/theme system"
tmux capture-pane -t "${session_name}" -p -e > "${work_dir}/theme-system.txt"
if grep -Eq '(38|48);2;' "${work_dir}/theme-system.txt"; then
  cat "${work_dir}/theme-system.txt" >&2
  echo "system theme retained fixed RGB colors" >&2
  exit 1
fi
send_line "/session"
send_line "hello from tmux e2e"
send_line "/complete /mo"
send_line "/complete /extension:j"
send_line "/status"
send_line "/editor draft"
send_line "/history"
send_line "history seed"
tmux send-keys -t "${session_name}" "history draft" Up Down Enter
sleep 0.35
tmux send-keys -t "${session_name}" "wheel draft"
sleep 0.1
tmux send-keys -t "${session_name}" Escape "[<64;5;10M"
sleep 0.1
tmux send-keys -t "${session_name}" Enter
sleep 0.35
tmux send-keys -t "${session_name}" "shift enter one"
tmux send-keys -t "${session_name}" Escape "[13;2u"
sleep 0.1
tmux send-keys -t "${session_name}" "shift enter two"
tmux send-keys -t "${session_name}" Escape "[13;2u"
sleep 0.1
tmux send-keys -t "${session_name}" "shift enter three"
tmux send-keys -t "${session_name}" Escape "[13;2u"
sleep 0.1
tmux send-keys -t "${session_name}" "shift enter four"
sleep 0.2
tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/expanded-input-pane.txt"
for expected in "│ > shift enter one" "│   shift enter two" "│   shift enter three" "│   shift enter four"; do
  if ! grep -Fq "${expected}" "${work_dir}/expanded-input-pane.txt"; then
    cat "${work_dir}/expanded-input-pane.txt" >&2
    echo "expanded input did not show: ${expected}" >&2
    exit 1
  fi
done
if grep -Fq '"content":"shift enter one' "${session_dir}"/*.jsonl; then
  echo "Shift+Enter submitted the draft before plain Enter" >&2
  exit 1
fi
tmux send-keys -t "${session_name}" Enter
sleep 0.35
if ! grep -Fq '"content":"shift enter one\nshift enter two\nshift enter three\nshift enter four"' "${session_dir}"/*.jsonl; then
  echo "plain Enter did not submit the complete Shift+Enter draft" >&2
  exit 1
fi
send_line "/image ${work_dir}/pixel.png describe image"
# Ctrl+V stages an image in the draft (not sent before Enter), and /undo
# restores both prompt and attachment without changing workspace files.
clipboard_session="$(ls -t "${session_dir}"/*.jsonl | head -n 1)"
before_paste="$(wc -l < "${clipboard_session}")"
tmux send-keys -t "${session_name}" C-v
sleep 0.35
tmux capture-pane -t "${session_name}" -p > "${work_dir}/clipboard-draft.txt"
grep -Fq "1 image attached" "${work_dir}/clipboard-draft.txt"
[ "$(wc -l < "${clipboard_session}")" = "${before_paste}" ]
send_line "clipboard image question"
send_line "/undo"
tmux capture-pane -t "${session_name}" -p > "${work_dir}/undo-draft.txt"
grep -Fq "clipboard image question" "${work_dir}/undo-draft.txt"
grep -Fq "1 image attached" "${work_dir}/undo-draft.txt"
grep -Fq '"type":"messages_snapshot"' "${clipboard_session}"
send_enter
send_line "/usage"
grep -Fq '"content":"clipboard image question","media":[{"mime_type":"image/png"' "${clipboard_session}"
# Empty image-only submission uses a default prompt; Ctrl+C clears attachments.
tmux send-keys -t "${session_name}" C-v
sleep 0.2
send_enter
tmux send-keys -t "${session_name}" C-v
sleep 0.2
tmux send-keys -t "${session_name}" C-c
sleep 0.2
tmux capture-pane -t "${session_name}" -p > "${work_dir}/clipboard-cleared.txt"
if grep -Fq "1 image attached" "${work_dir}/clipboard-cleared.txt"; then
  echo "Ctrl+C did not clear the image draft" >&2
  exit 1
fi
send_line "/image ${work_dir}/pixel.png"
for _ in $(seq 1 40); do
  tmux capture-pane -t "${session_name}" -p > "${work_dir}/clipboard-staged-path.txt"
  if grep -Fq "1 image attached" "${work_dir}/clipboard-staged-path.txt"; then break; fi
  sleep 0.1
done
grep -Fq "1 image attached" "${work_dir}/clipboard-staged-path.txt"
tmux send-keys -t "${session_name}" C-v
for _ in $(seq 1 40); do
  tmux capture-pane -t "${session_name}" -p > "${work_dir}/clipboard-multiple.txt"
  if grep -Fq "2 images attached" "${work_dir}/clipboard-multiple.txt"; then break; fi
  sleep 0.1
done
grep -Fq "2 images attached" "${work_dir}/clipboard-multiple.txt"
send_line "multiple clipboard images"
send_line "/write ${cwd_dir}/file.txt e2e-ok"
send_line "/undo"
grep -Fxq "e2e-ok" "${cwd_dir}/file.txt"
send_enter
send_line "/read ${cwd_dir}/file.txt"
send_line "/todos"
send_line "/diff"
sleep 0.35
tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/diff-pane.txt"
for expected in "edited files (1)" "file.txt"; do
  if ! grep -Fq "${expected}" "${work_dir}/diff-pane.txt"; then
    cat "${work_dir}/diff-pane.txt" >&2
    echo "diff panel did not show: ${expected}" >&2
    exit 1
  fi
done
send_line "/diff"
send_line "/reload"
send_line "/queue queued followup"
send_line "after reload"
send_line "/queue"
send_line "/queue queued cancel"
send_line "/queue"
send_line "/interrupt"
send_line "! printf shell-ok"
send_line "!!"
tmux send-keys -t "${session_name}" "! sleep 1; printf delayed-shell-ok" Enter
sleep 0.25
tmux capture-pane -t "${session_name}" -p -S -2000 > "${work_dir}/progress-pane.txt"
if ! grep -Fq "Running a command" "${work_dir}/progress-pane.txt"; then
  cat "${work_dir}/progress-pane.txt" >&2
  echo "shell progress was not visible while command was running" >&2
  exit 1
fi
sleep 1.1
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
# Two-layer picker: first Enter drills into the provider's models (faux is
# the first provider in the test config), second Enter selects its model.
send_enter
send_line "/model 1"
send_line "/hotkeys"
send_line "/copy"
send_line "/export ${work_dir}/session-export.json"
send_line "/export ${work_dir}/session-export.jsonl"
send_line "/export ${work_dir}/session-export.html"
send_line "/share ${work_dir}/session-share.html"
send_line "/clone"
send_line "/session"
send_line "/resume 1"
send_line "/session"
send_line "/import ${work_dir}/session-export.json"
send_line "/session"
send_line "/import ${work_dir}/session-export.jsonl"
send_line "/session"
send_line "/fork"
send_line "/session"
send_line "/summaries"
send_line "/tree"
# /resume opens the interactive picker; esc closes it back to the prompt.
send_line "/resume"
tmux send-keys -t "${session_name}" Escape
send_line "/compact"
send_line "/compact preserve architecture decisions and test failures"
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

require_output_regex() {
  local pattern="$1"
  if ! grep -Eq "${pattern}" "${work_dir}/pane.txt"; then
    cat "${work_dir}/pane.txt" >&2
    echo "missing expected pane output matching: ${pattern}" >&2
    exit 1
  fi
}

require_output "pi rust cli"
require_output "assistant>"
require_output "user>"
require_output "[faux/echo] hello from tmux e2e"
require_output "/model <provider/id>"
require_output "/extension:json-ext"
require_output "status"
require_output "[faux/echo] editor-prompt"
require_output "hello from tmux e2e"
require_output "[faux/echo] history draft"
require_output "[faux/echo] wheel draft"
require_output "[faux/echo] shift enter one"
require_output "shift enter two"
require_output "shift enter three"
require_output "shift enter four"
require_output "image/png"
require_output "1x1"
require_output "[faux/echo] describe image [media:1]"
require_output "wrote ${cwd_dir}/file.txt"
require_output "no todos"
require_output "e2e-ok"
require_output "reloaded"
require_output "queued: 1"
require_output "[faux/echo] after reload"
require_output "queued> queued followup"
require_output "[faux/echo] queued followup"
# Empty-queue text differs by path (mid-turn lists in-memory follow-ups,
# idle lists the durable session queue); either proves the queue drained.
require_output_regex '(queue is empty|no pending follow-ups)'
require_output "queued cancel"
require_output "interrupted; cleared 1 queued message(s)"
require_output "shell-ok"
require_output "completed bash"
require_output "delayed-shell-ok"
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
require_output "theme: light"
require_output "theme: kimi"
require_output "theme: system"
require_output "accent: magenta"
require_output "accent: auto"
require_output "theme selector"
require_output "provider selector"
require_output "faux/echo"
require_output "submit"
require_output "reload"
require_output "[faux/echo] after reload"
require_output "copied to clipboard via cat > ${work_dir}/clipboard.txt"
require_output "exported ${work_dir}/session-export.json"
require_output "exported ${work_dir}/session-export.jsonl"
require_output "exported ${work_dir}/session-export.html"
require_output "share exported ${work_dir}/session-share.html"
require_output "parent:"
require_output "Branched from"
require_output "session usage: no provider token usage reported yet"
require_output "Describe this image."
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
if [ ! -f "${work_dir}/extension-events.txt" ]; then
  echo "missing extension lifecycle events" >&2
  exit 1
fi
grep -Fxq "reload" "${work_dir}/extension-events.txt"
grep -Fxq "shutdown" "${work_dir}/extension-events.txt"
grep -Fq '"enabled": false' "${agent_dir}/settings.json"

printf '{"followUpMode":"one-at-a-time"}\n' > "${agent_dir}/settings.json"
one_follow_session="pi-e2e-one-follow-${$}"
tmux new-session -d -s "${one_follow_session}" -x 100 -y 20
tmux send-keys -t "${one_follow_session}" \
  "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
  Enter
for _ in $(seq 1 80); do
  if tmux capture-pane -t "${one_follow_session}" -p -S -2000 | grep -q "│ >"; then
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
  echo "queued follow-ups did not batch into one combined turn (first missing)" >&2
  exit 1
fi
# The combined turn echoes the joined prompt; the separator line sits between
# the two prompts in the echoed response, so match across lines.
if ! grep -A 2 -F "[faux/echo] first-one-at-a-time" "${work_dir}/one-follow-pane.txt" | grep -Fq "second-one-at-a-time"; then
  cat "${work_dir}/one-follow-pane.txt" >&2
  echo "queued follow-ups did not batch into one combined turn (second missing)" >&2
  exit 1
fi
if ! grep -Eq "(queue is empty|no pending follow-ups)" "${work_dir}/one-follow-pane.txt"; then
  cat "${work_dir}/one-follow-pane.txt" >&2
  echo "completed follow-ups remained queued" >&2
  exit 1
fi

disabled_session="pi-e2e-disabled-${$}"
tmux new-session -d -s "${disabled_session}" -x 100 -y 20
tmux send-keys -t "${disabled_session}" \
  "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session-dir '${session_dir}' --model faux/echo --no-tools" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${disabled_session}" -p -S -2000 | grep -q "│ >"; then
    break
  fi
  sleep 0.25
done

tmux send-keys -t "${disabled_session}" "/write ${cwd_dir}/blocked.txt blocked" Enter
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
if [ -f "${cwd_dir}/blocked.txt" ]; then
  echo "disabled tool unexpectedly wrote blocked.txt" >&2
  exit 1
fi

stream_session="pi-e2e-stream-${$}"
stream_ready="${work_dir}/stream-ready"
stream_done="${work_dir}/stream-done"
stream_prompt="$(printf 'stream%.0s' $(seq 1 120))"
tmux new-session -d -s "${stream_session}" -x 100 -y 24
tmux send-keys -t "${stream_session}" \
  "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' PI_FAUX_STREAM_DELAY_MS=4 PI_FAUX_STREAM_CHARS=1 PI_CLIPBOARD_IMAGE_COMMAND='cat ${work_dir}/pixel.png' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${stream_session}" -p -S -2000 | grep -q "│ >"; then
    touch "${stream_ready}"
    break
  fi
  sleep 0.25
done

if [ ! -f "${stream_ready}" ]; then
  tmux capture-pane -t "${stream_session}" -p -S -2000 >&2
  echo "streaming responsiveness prompt did not appear" >&2
  exit 1
fi

tmux send-keys -t "${stream_session}" -l "${stream_prompt}"
tmux send-keys -t "${stream_session}" Enter
sleep 0.2
tmux send-keys -t "${stream_session}" -l "draft while streaming"
stream_draft_seen=""
for _ in $(seq 1 40); do
  sleep 0.1
  tmux capture-pane -t "${stream_session}" -p -S -2000 > "${work_dir}/stream-draft-pane.txt"
  if grep -Fq "│ > draft while streaming" "${work_dir}/stream-draft-pane.txt"; then
    stream_draft_seen=1
    break
  fi
done
if [ -z "${stream_draft_seen}" ]; then
  cat "${work_dir}/stream-draft-pane.txt" >&2
  echo "typing during streaming did not update the input draft" >&2
  exit 1
fi

# Shift+Enter edits the live draft instead of enqueuing a follow-up.
tmux send-keys -t "${stream_session}" Escape "[13;2u"
tmux send-keys -t "${stream_session}" -l "shift enter streaming second"
sleep 0.15
tmux capture-pane -t "${stream_session}" -p > "${work_dir}/stream-newline-pane.txt"
for expected in "│ > draft while streaming" "│   shift enter streaming second" "esc to interrupt"; do
  if ! grep -Fq "${expected}" "${work_dir}/stream-newline-pane.txt"; then
    cat "${work_dir}/stream-newline-pane.txt" >&2
    echo "Shift+Enter during streaming did not show: ${expected}" >&2
    exit 1
  fi
done
if grep -Fq "❯ draft while streaming" "${work_dir}/stream-newline-pane.txt"; then
  echo "Shift+Enter during streaming queued the draft" >&2
  exit 1
fi
tmux send-keys -t "${stream_session}" Enter
for _ in $(seq 1 40); do
  tmux capture-pane -t "${stream_session}" -p > "${work_dir}/stream-queue-visible.txt"
  if grep -Fq "❯ draft while streaming" "${work_dir}/stream-queue-visible.txt"; then break; fi
  sleep 0.1
done
if ! grep -Fq "❯ draft while streaming" "${work_dir}/stream-queue-visible.txt" \
  || ! grep -Fq "ctrl+s queues after this response" "${work_dir}/stream-queue-visible.txt" \
  || ! grep -Fq "╭" "${work_dir}/stream-queue-visible.txt"; then
  cat "${work_dir}/stream-queue-visible.txt" >&2
  echo "queued draft pane did not render while streaming" >&2
  exit 1
fi
tmux send-keys -t "${stream_session}" Up
for _ in $(seq 1 40); do
  tmux capture-pane -t "${stream_session}" -p > "${work_dir}/stream-queue-recall.txt"
  if grep -Fq "│ > draft while streaming" "${work_dir}/stream-queue-recall.txt"; then break; fi
  sleep 0.1
done
if ! grep -Fq "│ > draft while streaming" "${work_dir}/stream-queue-recall.txt" \
  || ! grep -Fq "│   shift enter streaming second" "${work_dir}/stream-queue-recall.txt"; then
  cat "${work_dir}/stream-queue-recall.txt" >&2
  echo "Up did not recall newest queued draft while streaming" >&2
  exit 1
fi
tmux send-keys -t "${stream_session}" Enter
for _ in $(seq 1 120); do
  tmux capture-pane -t "${stream_session}" -p > "${work_dir}/stream-queued-pane.txt"
  if grep -Fq "[faux/echo] draft while streaming" "${work_dir}/stream-queued-pane.txt" \
    && grep -Fq "shift enter streaming second" "${work_dir}/stream-queued-pane.txt" \
    && ! grep -Fq "esc to interrupt" "${work_dir}/stream-queued-pane.txt"; then
    touch "${stream_done}"
    break
  fi
  sleep 0.1
done

if [ ! -f "${stream_done}" ]; then
  cat "${work_dir}/stream-queued-pane.txt" >&2
  echo "streaming draft was not queued after Enter" >&2
  exit 1
fi

tmux send-keys -t "${stream_session}" -l "${stream_prompt}"
tmux send-keys -t "${stream_session}" Enter
sleep 0.5
tmux send-keys -t "${stream_session}" -l "steer mid turn"
tmux send-keys -t "${stream_session}" C-s
stream_steered=""
for _ in $(seq 1 100); do
  sleep 0.1
  tmux capture-pane -t "${stream_session}" -p -S -2000 > "${work_dir}/stream-steer-pane.txt"
  if grep -Fq "steering> steer mid turn" "${work_dir}/stream-steer-pane.txt" \
    && grep -Fq "[faux/echo] steer mid turn" "${work_dir}/stream-steer-pane.txt"; then
    stream_steered=1
    break
  fi
done
tmux send-keys -t "${stream_session}" -l "${stream_prompt}"
tmux send-keys -t "${stream_session}" Enter
sleep 0.5
tmux send-keys -t "${stream_session}" -l "typed follow-up"
tmux send-keys -t "${stream_session}" Enter
sleep 0.2
tmux send-keys -t "${stream_session}" -l "/queue"
tmux send-keys -t "${stream_session}" Enter
stream_mid_queue=""
for _ in $(seq 1 60); do
  sleep 0.1
  tmux capture-pane -t "${stream_session}" -p -S -2000 > "${work_dir}/stream-midqueue-pane.txt"
  if grep -Fq "1. typed follow-up" "${work_dir}/stream-midqueue-pane.txt" \
    && grep -Fq "+1 queued" "${work_dir}/stream-midqueue-pane.txt" \
    && grep -Fq "esc to interrupt" "${work_dir}/stream-midqueue-pane.txt"; then
    stream_mid_queue=1
    break
  fi
done
for _ in $(seq 1 120); do
  tmux capture-pane -t "${stream_session}" -p -S -2000 > "${work_dir}/stream-followup-pane.txt"
  if grep -Fq "[faux/echo] typed follow-up" "${work_dir}/stream-followup-pane.txt"; then
    break
  fi
  sleep 0.1
done
if [ -z "${stream_mid_queue}" ]; then
  cat "${work_dir}/stream-midqueue-pane.txt" >&2
  echo "mid-turn /queue or activity indicator did not render" >&2
  exit 1
fi

tmux send-keys -t "${stream_session}" -l "${stream_prompt}"
tmux send-keys -t "${stream_session}" Enter
sleep 0.5
tmux send-keys -t "${stream_session}" C-c
stream_interrupted=""
for _ in $(seq 1 60); do
  sleep 0.1
  tmux capture-pane -t "${stream_session}" -p -S -2000 > "${work_dir}/stream-interrupt-pane.txt"
  if grep -Fq "interrupted" "${work_dir}/stream-interrupt-pane.txt"; then
    stream_interrupted=1
    break
  fi
done

tmux send-keys -t "${stream_session}" -l "${stream_prompt}"
tmux send-keys -t "${stream_session}" Enter
sleep 0.5
tmux send-keys -t "${stream_session}" Escape
stream_esc_interrupted=""
for _ in $(seq 1 60); do
  sleep 0.1
  tmux capture-pane -t "${stream_session}" -p -S -2000 > "${work_dir}/stream-esc-pane.txt"
  if grep -Fq "interrupted" "${work_dir}/stream-esc-pane.txt"; then
    stream_esc_interrupted=1
    break
  fi
done
# Image drafts typed mid-turn stay attached until the active response ends.
tmux send-keys -t "${stream_session}" -l "${stream_prompt}"
tmux send-keys -t "${stream_session}" Enter
sleep 0.1
tmux send-keys -t "${stream_session}" C-v
sleep 0.1
tmux send-keys -t "${stream_session}" -l "clipboard followup image"
tmux send-keys -t "${stream_session}" Enter
for _ in $(seq 1 200); do
  tmux capture-pane -t "${stream_session}" -p > "${work_dir}/stream-image-draft.txt"
  # Require the idle footer (model name, no activity hint): a redraw window
  # at turn end can briefly drop "esc to interrupt" while the input handler
  # still treats the turn as running, which would swallow the submit Enter.
  if grep -Fq "1 image attached" "${work_dir}/stream-image-draft.txt" \
    && last_line_is_idle_footer "${work_dir}/stream-image-draft.txt"; then break; fi
  sleep 0.1
done
grep -Fq "1 image attached" "${work_dir}/stream-image-draft.txt"
grep -Fq "clipboard followup image" "${work_dir}/stream-image-draft.txt"
tmux send-keys -t "${stream_session}" Enter
stream_image_sent=""
for _ in $(seq 1 80); do
  if grep -Fq '"content":"clipboard followup image","media":[{"mime_type":"image/png"' "${session_dir}"/*.jsonl; then stream_image_sent=1; break; fi
  sleep 0.1
done
if [ -z "${stream_image_sent}" ]; then
  tmux capture-pane -t "${stream_session}" -p >&2
  echo "image follow-up was not persisted after submission" >&2
  exit 1
fi
tmux send-keys -t "${stream_session}" "/quit" Enter
sleep 0.5
tmux kill-session -t "${stream_session}" >/dev/null 2>&1 || true

if [ -z "${stream_steered}" ]; then
  cat "${work_dir}/stream-steer-pane.txt" >&2
  echo "ctrl+s did not steer the streaming turn" >&2
  exit 1
fi

if [ -z "${stream_interrupted}" ]; then
  cat "${work_dir}/stream-interrupt-pane.txt" >&2
  echo "ctrl+c did not interrupt the streaming turn" >&2
  exit 1
fi

if [ -z "${stream_esc_interrupted}" ]; then
  cat "${work_dir}/stream-esc-pane.txt" >&2
  echo "esc did not interrupt the streaming turn" >&2
  exit 1
fi

clear_session="pi-e2e-clear-${$}"
tmux new-session -d -s "${clear_session}" -x 100 -y 20
tmux send-keys -t "${clear_session}" \
  "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session-dir '${session_dir}' --model faux/echo" \
  Enter

for _ in $(seq 1 80); do
  if tmux capture-pane -t "${clear_session}" -p -S -2000 | grep -q "│ >"; then
    break
  fi
  sleep 0.25
done

tmux send-keys -t "${clear_session}" "clear-screen-marker" Enter
sleep 0.6
clear_before="$(tmux capture-pane -t "${clear_session}" -p -S -2000 2>/dev/null || true)"
if ! printf '%s\n' "${clear_before}" | grep -Fq "clear-screen-marker"; then
  printf '%s\n' "${clear_before}" >&2
  echo "clear smoke marker did not appear before /clear" >&2
  exit 1
fi

tmux send-keys -t "${clear_session}" "/clear" Enter
clear_output=""
for _ in $(seq 1 30); do
  sleep 0.1
  clear_output="$(tmux capture-pane -t "${clear_session}" -p -S -2000 2>/dev/null || true)"
  if ! printf '%s\n' "${clear_output}" | grep -Fq "clear-screen-marker"; then
    break
  fi
done
printf '%s\n' "${clear_output}" > "${work_dir}/clear-pane.txt"
tmux send-keys -t "${clear_session}" "/quit" Enter
sleep 0.5
tmux kill-session -t "${clear_session}" >/dev/null 2>&1 || true

if printf '%s\n' "${clear_output}" | grep -Fq "clear-screen-marker"; then
  cat "${work_dir}/clear-pane.txt" >&2
  echo "/clear did not remove prior pane output" >&2
  exit 1
fi
if ! printf '%s\n' "${clear_output}" | grep -Fq "│ >"; then
  cat "${work_dir}/clear-pane.txt" >&2
  echo "/clear did not redraw the input prompt" >&2
  exit 1
fi

# Exercise wrapping, paste placeholders, deletion, and exact full-text
# submission in a separate local-provider session without transcript dumps.
input_session="pi-e2e-input-${$}"
input_log="${session_dir}/growing-input.jsonl"
tmux new-session -d -s "${input_session}" -x 40 -y 20
tmux send-keys -t "${input_session}" \
  "cd '${cwd_dir}' && PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session-id growing-input --session-dir '${session_dir}' --model faux/echo --offline" Enter
for _ in $(seq 1 80); do
  if tmux capture-pane -t "${input_session}" -p | grep -Fq "│ >"; then break; fi
  sleep 0.25
done
input_capture() { tmux capture-pane -t "${input_session}" -p > "${work_dir}/input-pane.txt"; }
input_require() {
  if ! grep -Fq "$1" "${work_dir}/input-pane.txt"; then
    cat "${work_dir}/input-pane.txt" >&2
    echo "missing growing input marker: $1" >&2
    exit 1
  fi
}
# Seed real history so Up/Down below exercises folded-draft restoration.
tmux send-keys -t "${input_session}" "input history seed" Enter
for _ in $(seq 1 80); do
  if tmux capture-pane -t "${input_session}" -p | grep -Fq "[faux/echo] input history seed"; then break; fi
  sleep 0.1
done
tmux send-keys -t "${input_session}" -l "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"
sleep 0.2
input_capture
input_require "│ > abcdefghijklmnopqrstuvwxyz01234567"
input_require "│   89ABCDEFGHIJ"
if [ "$(tmux display-message -p -t "${input_session}" '#{cursor_x},#{cursor_y}')" != "16,17" ]; then
  cat "${work_dir}/input-pane.txt" >&2
  echo "wrapped input cursor did not follow the second row" >&2
  exit 1
fi
tmux resize-window -t "${input_session}" -x 80 -y 20
sleep 0.2
input_capture
input_require "│ > abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"
if [ "$(tmux display-message -p -t "${input_session}" '#{cursor_x},#{cursor_y}')" != "50,17" ]; then
  echo "resized input did not shrink to one row" >&2
  exit 1
fi
tmux send-keys -t "${input_session}" C-c
sleep 0.1
# tmux -p wraps the buffer in the bracketed-paste protocol; -r preserves LF.
awk 'BEGIN { for (i = 1; i <= 500; i++) printf "paste-line-%03d\n", i }' > "${work_dir}/input-paste.txt"
input_paste() {
  tmux load-buffer -b "${input_session}" "$1"
  tmux paste-buffer -r -p -d -b "${input_session}" -t "${input_session}"
  sleep 0.2
}
tmux send-keys -t "${input_session}" -l "prefix "
input_paste "${work_dir}/input-paste.txt"
tmux send-keys -t "${input_session}" -l " suffix"
sleep 0.2
input_capture
input_require "│ > prefix [paste #1 +500 lines] suffix"
if grep -Fq "paste-line-" "${work_dir}/input-pane.txt"; then
  echo "collapsed paste leaked full contents into the editor" >&2
  exit 1
fi
# The folded draft and cursor survive a round-trip through prompt history.
tmux send-keys -t "${input_session}" Up Down
sleep 0.2
input_capture
input_require "[paste #1 +500 lines] suffix"
tmux send-keys -t "${input_session}" Enter
# Check the exact persisted content, not only the first/last pasted lines.
expected_paste="$(awk 'BEGIN { printf "prefix "; for (i = 1; i <= 500; i++) printf "paste-line-%03d\\n", i; printf " suffix" }')"
submitted=""
for _ in $(seq 1 100); do
  if grep -Fq "\"content\":\"[faux/echo] ${expected_paste}\"" "${input_log}"; then submitted=1; break; fi
  sleep 0.1
done
if [ -z "${submitted}" ]; then
  input_capture
  cat "${work_dir}/input-pane.txt" >&2
  echo "full pasted prompt was not submitted to faux provider" >&2
  exit 1
fi
grep -Fq "\"content\":\"${expected_paste}\"" "${input_log}"
grep -Fq "\"content\":\"[faux/echo] ${expected_paste}\"" "${input_log}"
if grep -Fq "[paste" "${input_log}"; then
  echo "paste placeholder was stored instead of full text" >&2
  exit 1
fi
# Delete a folded paste in one keypress, then paste again with a fresh ID.
tmux send-keys -t "${input_session}" -l "keep "
input_paste "${work_dir}/input-paste.txt"
tmux send-keys -t "${input_session}" BSpace
sleep 0.2
input_capture
input_require "│ > keep"
if grep -Fq "[paste" "${work_dir}/input-pane.txt"; then
  echo "Backspace did not remove the entire folded paste" >&2
  exit 1
fi
input_paste "${work_dir}/input-paste.txt"
input_capture
input_require "[paste #3 +500 lines]"
tmux send-keys -t "${input_session}" C-c
sleep 0.1
printf 'second\npaste' > "${work_dir}/input-paste-small.txt"
input_paste "${work_dir}/input-paste-small.txt"
input_capture
input_require "[paste #4 +2 lines]"
tmux send-keys -t "${input_session}" C-c
sleep 0.1
tmux send-keys -t "${input_session}" "/quit" Enter
sleep 0.3
tmux kill-session -t "${input_session}" >/dev/null 2>&1 || true

# Resume a local fixture to exercise thinking/tool previews, task selection,
# key toggles, and idle queue recall without real-provider requests.
modern_session="pi-e2e-modern-${$}"
modern_log="${session_dir}/modern-harness.jsonl"
cat > "${modern_log}" <<'JSONL'
{"type":"started","session_id":"modern-harness","cwd":"."}
{"type":"message","message":{"role":"user","content":"modern harness fixture","tool_calls":[],"media":[]}}
{"type":"message","message":{"role":"assistant","content":"fixture answer","thinking":"reason-first\nreason-second\nreason-third\nreason-fourth\nreason-fifth\nreason-last","tool_calls":[{"id":"read-fixture","name":"read","arguments":"{\"path\":\"fixture.rs\"}"}],"media":[]}}
{"type":"message","message":{"role":"tool","content":"output-first\noutput-second\noutput-third\noutput-fourth\noutput-fifth\noutput-sixth\noutput-seventh\noutput-last","tool_call_id":"read-fixture","tool_name":"read","tool_calls":[],"media":[]}}
{"type":"message","message":{"role":"assistant","content":"fixture completed","tool_calls":[],"media":[]}}
{"type":"tool","event":{"id":"read-fixture","name":"read","result":"fixture contents","is_error":false}}
{"type":"todos","todos":[{"content":"done-oldest","status":"completed"},{"content":"done-older","status":"completed"},{"content":"done-old","status":"completed"},{"content":"done-recent","status":"completed"},{"content":"active-task","status":"in_progress"},{"content":"pending-one","status":"pending"},{"content":"pending-two","status":"pending"},{"content":"pending-three","status":"pending"}]}
{"type":"queued_message","message":"recall queued fixture"}
JSONL
tmux new-session -d -s "${modern_session}" -x 100 -y 40
tmux send-keys -t "${modern_session}" \
  "cd '${cwd_dir}' && env -u NO_COLOR PI_CODING_AGENT_DIR='${agent_dir}' '${cargo_bin}' run -q --manifest-path '${repo_root}/Cargo.toml' -p pi-cli -- --session '${modern_log}' --session-dir '${session_dir}' --model faux/echo --offline" Enter
for _ in $(seq 1 80); do
  if tmux capture-pane -t "${modern_session}" -p | grep -Fq "│ >"; then break; fi
  sleep 0.25
done
modern_capture() { tmux capture-pane -t "${modern_session}" -p > "${work_dir}/modern-pane.txt"; }
modern_require() {
  if ! grep -Fq "$1" "${work_dir}/modern-pane.txt"; then
    cat "${work_dir}/modern-pane.txt" >&2
    echo "missing modern harness marker: $1" >&2
    exit 1
  fi
}
modern_capture
for marker in "  Todo" "╭" "│ >" "╰" "↑ to edit · runs after next turn" "reason-first" "4 more lines, ctrl+o to expand" "Used Read" "fixture.rs" "… output-last" "7 more lines" "done-recent" "active-task" "+3 more (3 done)" "❯ recall queued fixture"; do modern_require "${marker}"; done
if grep -Eq 'reason-last|output-first|output-seventh|done-oldest' "${work_dir}/modern-pane.txt"; then
  cat "${work_dir}/modern-pane.txt" >&2
  echo "collapsed previews/tasks leaked hidden rows" >&2
  exit 1
fi
tmux send-keys -t "${modern_session}" C-o C-t
sleep 0.3
modern_capture
for marker in "reason-last" "output-first" "output-last" "done-oldest" "ctrl+t to collapse"; do modern_require "${marker}"; done
# Resize with both task and queue panels visible. The hint describes rows
# actually hidden by the shorter viewport, not only the collapsed limit.
tmux send-keys -t "${modern_session}" C-o C-t
tmux resize-window -t "${modern_session}" -x 72 -y 18
sleep 0.3
modern_capture
for marker in "Todo" "+6 more (3 pending, 3 done)" "active-task" "❯ recall queued fixture" "│ >" "╭" "╰"; do modern_require "${marker}"; done
tmux resize-window -t "${modern_session}" -x 100 -y 40
sleep 0.3
tmux send-keys -t "${modern_session}" Up
sleep 0.3
modern_capture
modern_require "│ > recall queued fixture"
if grep -Fq "❯ recall queued fixture" "${work_dir}/modern-pane.txt"; then
  echo "recalled queue message was not removed from pane" >&2
  exit 1
fi
grep -Fq '"type":"queued_messages_snapshot","messages":[]' "${modern_log}"
tmux send-keys -t "${modern_session}" C-c
sleep 0.15
tmux send-keys -t "${modern_session}" "/quit" Enter
sleep 0.3

"${repo_root}/scripts/e2e-scrollback.sh"
"${repo_root}/scripts/e2e-transcript.sh"
"${repo_root}/scripts/e2e-selection.sh"
"${repo_root}/scripts/e2e-tool-output.sh"
"${repo_root}/scripts/e2e-queue.sh"
"${repo_root}/scripts/e2e-models.sh"

echo "tmux e2e passed: ${first_session}"

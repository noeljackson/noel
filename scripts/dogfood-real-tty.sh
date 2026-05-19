#!/usr/bin/env bash
set -euo pipefail

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required for real-provider TTY dogfood tests" >&2
  exit 127
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pi_bin="${PI_BIN:-${repo_root}/target/release/pi}"

if [ ! -x "${pi_bin}" ]; then
  echo "release binary is not executable: ${pi_bin}" >&2
  echo "run make release first, or set PI_BIN" >&2
  exit 1
fi

providers="${PI_DOGFOOD_REAL_PROVIDERS:-claude codex}"
claude_model="${PI_DOGFOOD_CLAUDE_MODEL:-anthropic/claude-sonnet-4-6}"
claude_thinking="${PI_DOGFOOD_CLAUDE_THINKING:-high}"
codex_model="${PI_DOGFOOD_CODEX_MODEL:-openai-codex/gpt-5.4-mini}"
codex_thinking="${PI_DOGFOOD_CODEX_THINKING:-low}"

has_claude_auth() {
  [[ -n "${ANTHROPIC_OAUTH_TOKEN:-}" ]] ||
    [[ -n "${ANTHROPIC_AUTH_TOKEN:-}" ]] ||
    [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]] ||
    [[ -n "${ANTHROPIC_API_KEY:-}" ]] ||
    [[ -f "${HOME}/.claude/.credentials.json" ]]
}

has_codex_auth() {
  [[ -n "${CODEX_ACCESS_TOKEN:-}" ]] ||
    [[ -n "${CODEX_API_KEY:-}" ]] ||
    [[ -f "${HOME}/.codex/auth.json" ]]
}

provider_requested() {
  local provider="$1"
  for requested in ${providers}; do
    if [ "${requested}" = "${provider}" ]; then
      return 0
    fi
  done
  return 1
}

count_fixed() {
  local needle="$1"
  local file="$2"
  grep -Fo "${needle}" "${file}" 2>/dev/null | wc -l | tr -d ' ' || true
}

run_provider() {
  local label="$1"
  local model="$2"
  local thinking="$3"
  local marker="$4"
  local session_name="pi-real-${label}-${$}"
  local root="${repo_root}/target/dogfood-real-tty-${label}"
  local session_dir="${root}/sessions"
  local work_dir="${root}/work"
  local cwd_dir="${work_dir}/cwd"
  local agent_dir="${work_dir}/agent"
  local prompt_file="${work_dir}/prompt-ready"
  local pane_file="${work_dir}/pane.txt"
  local prompt

  prompt="Write a tiny complete Rust program under 20 lines. Include exactly this marker as a comment: ${marker}. Include fn main. Do not use markdown fences."

  rm -rf "${root}"
  mkdir -p "${session_dir}" "${cwd_dir}" "${agent_dir}"
  cat > "${agent_dir}/settings.json" <<'JSON'
{
  "modelRefresh": {
    "enabled": false
  },
  "showHardwareCursor": true
}
JSON

  cleanup_provider() {
    tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
  }
  trap cleanup_provider RETURN

  tmux new-session -d -s "${session_name}" -x 120 -y 36
  tmux send-keys -t "${session_name}" \
    "cd '${cwd_dir}' && PI_TUI_E2E_DUMP=1 PI_CODING_AGENT_DIR='${agent_dir}' '${pi_bin}' --session-dir '${session_dir}' --model '${model}' --thinking '${thinking}'" \
    Enter

  for _ in $(seq 1 120); do
    if tmux capture-pane -t "${session_name}" -p -S -2000 | grep -q "pi>"; then
      touch "${prompt_file}"
      break
    fi
    sleep 0.25
  done
  if [ ! -f "${prompt_file}" ]; then
    tmux capture-pane -t "${session_name}" -p -S -2000 >&2
    echo "${label} TTY prompt did not appear" >&2
    exit 1
  fi

  tmux send-keys -t "${session_name}" -l "/session"
  tmux send-keys -t "${session_name}" Enter
  sleep 0.35
  tmux send-keys -t "${session_name}" -l "${prompt}"
  tmux send-keys -t "${session_name}" Enter

  for _ in $(seq 1 240); do
    tmux capture-pane -t "${session_name}" -p -S -2000 > "${pane_file}"
    marker_count="$(count_fixed "${marker}" "${pane_file}")"
    main_count="$(count_fixed "fn main" "${pane_file}")"
    if [ "${marker_count}" -ge 2 ] && [ "${main_count}" -ge 2 ] && ! grep -Fq "Working..." "${pane_file}"; then
      break
    fi
    if grep -Fq "error>" "${pane_file}"; then
      cat "${pane_file}" >&2
      echo "${label} TTY smoke returned an error" >&2
      exit 1
    fi
    sleep 0.5
  done

  marker_count="$(count_fixed "${marker}" "${pane_file}")"
  main_count="$(count_fixed "fn main" "${pane_file}")"
  if [ "${marker_count}" -lt 2 ]; then
    cat "${pane_file}" >&2
    echo "${label} TTY smoke did not return marker: ${marker}" >&2
    exit 1
  fi
  if [ "${main_count}" -lt 2 ]; then
    cat "${pane_file}" >&2
    echo "${label} TTY smoke did not return a Rust main function" >&2
    exit 1
  fi

  tmux send-keys -t "${session_name}" -l "/quit"
  tmux send-keys -t "${session_name}" Enter
  sleep 0.5
  tmux capture-pane -t "${session_name}" -p -S -2000 > "${pane_file}"
  if ! grep -Fq "assistant>" "${pane_file}"; then
    cat "${pane_file}" >&2
    echo "${label} TTY smoke did not capture an assistant transcript entry" >&2
    exit 1
  fi
  cleanup_provider
  trap - RETURN

  echo "${label} real TTY dogfood passed: ${model} thinking=${thinking}"
}

if provider_requested claude; then
  if ! has_claude_auth; then
    echo "Claude real TTY dogfood requires ANTHROPIC_OAUTH_TOKEN, ANTHROPIC_AUTH_TOKEN, CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY, or ~/.claude/.credentials.json" >&2
    exit 2
  fi
  run_provider claude "${claude_model}" "${claude_thinking}" "PI_TTY_CLAUDE_SMOKE"
fi

if provider_requested codex; then
  if ! has_codex_auth; then
    echo "Codex real TTY dogfood requires CODEX_ACCESS_TOKEN, CODEX_API_KEY, or ~/.codex/auth.json" >&2
    exit 2
  fi
  run_provider codex "${codex_model}" "${codex_thinking}" "PI_TTY_CODEX_SMOKE"
fi

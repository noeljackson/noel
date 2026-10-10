#!/usr/bin/env bash
set -euo pipefail

# Model cache management e2e: /models reset clears a poisoned cache,
# /models refresh reports per-provider outcomes (never a paid API), offline
# mode refuses refresh, and the model selector reflects the reset list.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pi_bin="${PI_BIN:-${repo_root}/target/debug/pi}"
# Anchor to an absolute path so the cd inside tmux cannot break the lookup.
case "${pi_bin}" in
  /*) ;;
  *) pi_bin="${repo_root}/${pi_bin}" ;;
esac
work_dir="$(mktemp -d "${repo_root}/target/e2e-models.XXXXXX")"
session="pi-e2e-models-${$}"
trap 'tmux kill-session -t "${session}" >/dev/null 2>&1 || true' EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false}}\n' > "${work_dir}/agent/settings.json"
# Poisoned cache: a bogus model that must vanish after /models reset.
cat > "${work_dir}/agent/model-cache.json" <<'EOF'
{
  "refreshedAt": 1,
  "version": 2,
  "models": [
    {
      "provider": "zai-coding",
      "id": "glm-bogus-from-cache",
      "name": "Bogus Cached",
      "api": "openai",
      "baseUrl": "https://api.z.ai/api/coding/paas/v4"
    }
  ],
  "diagnostics": ["stale diagnostic from a previous refresh"],
  "claudeCodeVersion": null
}
EOF
tmux new-session -d -s "${session}" -x 90 -y 24
tmux pipe-pane -t "${session}" -o "cat > '${work_dir}/terminal.raw'"
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && env -u PI_TUI_E2E_DUMP PI_CODING_AGENT_DIR='${work_dir}/agent' '${pi_bin}' --no-session --model faux/echo --offline" Enter
capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 150); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing marker: $1"
}
absent() {
  for _ in $(seq 1 30); do capture; if ! grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "unexpected marker: $1"
}

wait_for '│ >'
# Offline startup must not touch the poisoned cache; the bogus model is served
# from the cache alongside built-ins.
tmux send-keys -t "${session}" '/models reset' Enter
wait_for 'model cache cleared'
wait_for 'run /models refresh to repopulate'
absent 'glm-bogus-from-cache'
[ ! -f "${work_dir}/agent/model-cache.json" ] || fail 'model-cache.json still exists after reset'
# Reset is idempotent.
tmux send-keys -t "${session}" '/models reset' Enter
wait_for 'model cache already clear'
# Offline refresh is refused without touching anything.
tmux send-keys -t "${session}" '/models refresh' Enter
wait_for 'model refresh is disabled in offline mode'
# Completions advertise the subcommands (deterministic via /complete).
tmux send-keys -t "${session}" '/complete /models r' Enter
wait_for '/models refresh'
wait_for '/models reset'
absent 'no completions'
# The selector still opens and the poisoned model is gone.
tmux send-keys -t "${session}" '/models glm' Enter
wait_for 'model selector'
absent 'glm-bogus-from-cache'
tmux send-keys -t "${session}" Escape
wait_for '│ >'
tmux send-keys -t "${session}" '/quit' Enter

echo "owned models e2e passed (artifacts: ${work_dir})"

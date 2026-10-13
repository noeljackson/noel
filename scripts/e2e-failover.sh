#!/usr/bin/env bash
set -euo pipefail

# Account failover e2e, on a scripted faux model with two stored accounts:
# a rate-limited account's request is answered by the other without a retry
# and the TUI names both accounts; `/account <name>` pins an account, after
# which a rate limit is the run loop's to retry.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/e2e-isolate.sh
source "${repo_root}/scripts/e2e-isolate.sh"
noel_bin="${NOEL_BIN:-${repo_root}/target/debug/noel}"
# Anchor to an absolute path so the cd inside tmux cannot break the lookup.
case "${noel_bin}" in
  /*) ;;
  *) noel_bin="${repo_root}/${noel_bin}" ;;
esac
work_dir="$(mktemp -d "${e2e_root}/e2e-failover.XXXXXX")"
session="noel-e2e-failover-${$}"
trap 'e2e_status=$?; tmux kill-session -t "${session}" >/dev/null 2>&1 || true; e2e_isolation_cleanup "${e2e_status}"' EXIT
mkdir -p "${work_dir}/agent"
printf '{"modelRefresh":{"enabled":false},"compaction":{"enabled":false},"retry":{"baseDelayMs":1},"failover":{"enabled":true}}\n' > "${work_dir}/agent/settings.json"
(umask 077 && printf '{"faux":{"first":{"type":"api_key","key":"first-key"},"second":{"type":"api_key","key":"second-key"}}}\n' > "${work_dir}/agent/auth.json")
# Requests in order: the first account is rate limited and the second
# answers; then the pinned account is rate limited once and answers.
cat > "${work_dir}/script.json" <<'EOF'
[
  [{"error": {"status": 429, "body": "rate limited"}}],
  [{"text": "served after the switch"}],
  [{"error": {"status": 429, "body": "rate limited"}}],
  [{"text": "served by the pinned account"}]
]
EOF
tmux new-session -d -s "${session}" -x 110 -y 30
tmux pipe-pane -t "${session}" -o "cat > '${work_dir}/terminal.raw'"
tmux send-keys -t "${session}" \
  "cd '${work_dir}' && env -u NOEL_TUI_E2E_DUMP NOEL_DIR='${work_dir}/agent' NOEL_FAUX_SCRIPT='${work_dir}/script.json' NOEL_FAUX_SCRIPT_LOG='${work_dir}/requests.jsonl' '${noel_bin}' --no-session --no-context-files --model faux/script --offline" Enter
capture() { tmux capture-pane -t "${session}" -p > "${work_dir}/pane.txt"; }
fail() { cat "${work_dir}/pane.txt" >&2; echo "$* (artifacts: ${work_dir})" >&2; exit 1; }
wait_for() {
  for _ in $(seq 1 150); do capture; if grep -Fq "$1" "${work_dir}/pane.txt"; then return; fi; sleep 0.1; done
  fail "missing marker: $1"
}
requests() { wc -l < "${work_dir}/requests.jsonl" | tr -d ' '; }

wait_for '│ >'
tmux send-keys -t "${session}" 'first prompt' Enter
wait_for 'served after the switch'
# The switch is reported after the turn, as a system line; it was not a retry.
wait_for 'faux account first is rate limited until'
wait_for 'UTC; switched to second'
capture
if grep -Fq 'retrying in' "${work_dir}/pane.txt"; then fail 'a failover switch was shown as a retry'; fi
[ "$(requests)" = 2 ] || fail "expected 2 requests after the switch, got $(requests)"

# Pinned, the account's rate limit is retried by the run loop instead.
tmux send-keys -t "${session}" '/account first' Enter
wait_for 'account: first'
tmux send-keys -t "${session}" 'second prompt' Enter
wait_for 'served by the pinned account'
wait_for 'retrying in 1ms (attempt 1 of 3): request failed with status 429'
[ "$(requests)" = 4 ] || fail "expected 4 requests with the pinned account, got $(requests)"
tmux send-keys -t "${session}" '/quit' Enter

echo "owned failover e2e passed (artifacts: ${work_dir})"

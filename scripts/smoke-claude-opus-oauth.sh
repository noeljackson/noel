#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_cmd="${CARGO:-cargo}"
model="${NOEL_CLAUDE_OPUS_SMOKE_MODEL:-anthropic/claude-opus-4-7}"
thinking="${NOEL_CLAUDE_OPUS_SMOKE_THINKING:-max}"
expected="${NOEL_CLAUDE_OPUS_SMOKE_EXPECTED:-noel-opus-smoke}"
prompt="Reply with exactly this text and nothing else: ${expected}"

if [[ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" && -z "${ANTHROPIC_AUTH_TOKEN:-}" && ! -f "${HOME}/.claude/.credentials.json" ]]; then
  echo "Claude Code OAuth smoke requires CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_AUTH_TOKEN, or ~/.claude/.credentials.json" >&2
  exit 2
fi

work_dir="$(mktemp -d "${repo_root}/target/claude-opus-smoke.XXXXXX")"
cleanup() {
  rm -rf "${work_dir}"
}
trap cleanup EXIT

output="$(
  cd "${repo_root}" && \
    NOEL_DIR="${work_dir}/agent" \
    "${cargo_cmd}" run -q -p noel-cli -- \
      --no-session \
      --model "${model}" \
      --thinking "${thinking}" \
      -p \
      "${prompt}"
)"

printf '%s\n' "${output}"

if ! grep -Fq "${expected}" <<<"${output}"; then
  echo "Claude Opus OAuth smoke did not contain expected output: ${expected}" >&2
  exit 1
fi

echo "Claude Opus OAuth smoke passed: ${model} thinking=${thinking}"

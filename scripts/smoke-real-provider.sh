#!/usr/bin/env bash
set -euo pipefail

profile="${1:-${PI_SMOKE_REAL_PROFILE:-}}"

profile_model() {
  case "$1" in
    openai) printf '%s\n' "${PI_SMOKE_OPENAI_MODEL:-openai/gpt-4.1}" ;;
    anthropic) printf '%s\n' "${PI_SMOKE_ANTHROPIC_MODEL:-anthropic/claude-sonnet-4-6}" ;;
    gemini | google) printf '%s\n' "${PI_SMOKE_GEMINI_MODEL:-google/gemini-2.5-pro}" ;;
    mistral) printf '%s\n' "${PI_SMOKE_MISTRAL_MODEL:-mistral/devstral-medium-latest}" ;;
    openrouter) printf '%s\n' "${PI_SMOKE_OPENROUTER_MODEL:-openrouter/moonshotai/kimi-k2.6}" ;;
    *) return 1 ;;
  esac
}

profile_auth_hint() {
  case "$1" in
    openai) printf '%s\n' "OPENAI_API_KEY" ;;
    anthropic) printf '%s\n' "ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN, CLAUDE_CODE_OAUTH_TOKEN, or ~/.claude/.credentials.json" ;;
    gemini | google) printf '%s\n' "GEMINI_API_KEY or GOOGLE_API_KEY" ;;
    mistral) printf '%s\n' "MISTRAL_API_KEY" ;;
    openrouter) printf '%s\n' "OPENROUTER_API_KEY" ;;
    *) printf '%s\n' "provider auth" ;;
  esac
}

if [[ -z "${PI_SMOKE_REAL_MODEL:-}" && -n "${profile}" ]]; then
  if ! PI_SMOKE_REAL_MODEL="$(profile_model "${profile}")"; then
    echo "unknown real-provider smoke profile: ${profile}" >&2
    exit 2
  fi
  export PI_SMOKE_REAL_MODEL
fi

if [[ "${PI_SMOKE_REAL:-}" != "1" ]]; then
  if [[ -n "${profile}" ]]; then
    echo "skipping ${profile} real-provider smoke; set PI_SMOKE_REAL=1 and $(profile_auth_hint "${profile}")" >&2
  else
    echo "skipping real-provider smoke; set PI_SMOKE_REAL=1 and PI_SMOKE_REAL_MODEL=provider/model" >&2
  fi
  exit 0
fi

if [[ -z "${PI_SMOKE_REAL_MODEL:-}" ]]; then
  echo "PI_SMOKE_REAL_MODEL or a known profile is required when PI_SMOKE_REAL=1" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_bin="${CARGO:-cargo}"
model="${PI_SMOKE_REAL_MODEL}"
thinking="${PI_SMOKE_REAL_THINKING:-off}"
expected="${PI_SMOKE_REAL_EXPECTED:-pi-real-smoke-ok}"
profile_slug="${profile:-custom}"
work_dir="${repo_root}/target/smoke-real-provider-${profile_slug}"
agent_dir="${work_dir}/agent"

rm -rf "${work_dir}"
mkdir -p "${agent_dir}"

cat > "${agent_dir}/settings.json" <<'JSON'
{
  "modelRefresh": {
    "enabled": false
  }
}
JSON

prompt="Reply with exactly this text and no other words: ${expected}"
output="$(
  PI_CODING_AGENT_DIR="${agent_dir}" \
    "${cargo_bin}" run -q -p pi-cli -- \
      --no-session \
      --model "${model}" \
      --thinking "${thinking}" \
      --print \
      "${prompt}"
)"

printf '%s\n' "${output}" > "${work_dir}/output.txt"
if ! grep -Fq "${expected}" "${work_dir}/output.txt"; then
  cat "${work_dir}/output.txt" >&2
  echo "real-provider smoke did not include expected marker: ${expected}" >&2
  exit 1
fi

echo "real-provider smoke passed: ${model}"

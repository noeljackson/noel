#!/usr/bin/env bash
set -euo pipefail

if [[ "${PI_SMOKE_REAL:-}" != "1" ]]; then
  echo "skipping real-provider smoke; set PI_SMOKE_REAL=1 and PI_SMOKE_REAL_MODEL=provider/model" >&2
  exit 0
fi

if [[ -z "${PI_SMOKE_REAL_MODEL:-}" ]]; then
  echo "PI_SMOKE_REAL_MODEL is required when PI_SMOKE_REAL=1" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_bin="${CARGO:-cargo}"
model="${PI_SMOKE_REAL_MODEL}"
thinking="${PI_SMOKE_REAL_THINKING:-off}"
expected="${PI_SMOKE_REAL_EXPECTED:-pi-real-smoke-ok}"
work_dir="${repo_root}/target/smoke-real-provider"
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

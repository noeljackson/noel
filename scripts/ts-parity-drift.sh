#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

make_bin="${MAKE:-make}"
cargo_bin="${CARGO:-cargo}"
reference_repo="${TS_REFERENCE_REPO:-https://github.com/noeljackson/pi.git}"
tracking_ref="${TS_PARITY_TRACKING_REF:-ts-reference}"
fixtures_dir="${TS_PARITY_FIXTURES_DIR:-tests/fixtures/ts-parity}"
out_dir="${TS_PARITY_DRIFT_DIR:-target/ts-parity-drift}"
diff_file="${out_dir}/fixture.diff"
brief_file="${out_dir}/brief.md"

mkdir -p "${out_dir}" "${fixtures_dir}"

"${make_bin}" ts-parity-fixtures \
  TS_REFERENCE_REPO="${reference_repo}" \
  TS_REFERENCE_REF="${tracking_ref}" \
  TS_PARITY_FIXTURES_DIR="${fixtures_dir}"

git diff -- "${fixtures_dir}" > "${diff_file}"

if [ -s "${diff_file}" ]; then
  {
    printf '# TS Parity Drift Brief\n\n'
    printf 'The TypeScript reference fixtures changed against the tracked reference.\n\n'
    printf '## Reference\n\n'
    printf -- '- Repository: `%s`\n' "${reference_repo}"
    printf -- '- Ref: `%s`\n' "${tracking_ref}"
    printf -- '- Fixtures: `%s`\n' "${fixtures_dir}"
    printf -- '- Diff: `%s`\n\n' "${diff_file}"
    printf '## Constraints\n\n'
    printf -- '- Do not run npm on the host.\n'
    printf -- '- Execute TypeScript only through Docker targets such as `make ts-parity-fixtures`, `make ts-parity-update`, or `make ts-parity-drift`.\n'
    printf -- '- Keep changes scoped to Rust parity code, fixtures, tests, and docs required by the drift.\n'
    printf -- '- Run `cargo test -p pi-ai --lib matches_ts` and `make check` before committing.\n'
    printf -- '- If a GitHub issue is provided, include `closes #<number>` in the commit message.\n\n'
    printf '## Suggested Workflow\n\n'
    printf '1. Inspect `tests/fixtures/ts-parity` and the diff below.\n'
    printf '2. Update Rust request builders/parsers or model metadata to match the official TypeScript reference.\n'
    printf '3. Regenerate fixtures with `make ts-parity-update`.\n'
    printf '4. Run `make ts-parity-drift` until it reports no drift.\n'
    printf '5. Run the full Rust validation suite.\n\n'
    printf '## Fixture Diff\n\n'
    printf '```diff\n'
    cat "${diff_file}"
    printf '```\n'
  } > "${brief_file}"

  printf 'TS parity drift detected.\n' >&2
  printf 'Diff: %s\n' "${diff_file}" >&2
  printf 'Agent brief: %s\n' "${brief_file}" >&2

  if [ -n "${PI_PARITY_AGENT_COMMAND:-}" ]; then
    printf 'Dispatching parity agent command: %s\n' "${PI_PARITY_AGENT_COMMAND}" >&2
    sh -c "${PI_PARITY_AGENT_COMMAND}" < "${brief_file}"
  fi

  exit 1
fi

{
  printf '# TS Parity Drift Brief\n\n'
  printf 'No fixture drift detected.\n\n'
  printf 'Reference: `%s` at `%s`.\n' "${reference_repo}" "${tracking_ref}"
} > "${brief_file}"

"${cargo_bin}" test -p pi-ai --lib matches_ts

printf 'No TS parity drift detected.\n'
printf 'Brief: %s\n' "${brief_file}"

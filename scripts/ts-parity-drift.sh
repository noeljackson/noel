#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

make_bin="${MAKE:-make}"
cargo_bin="${CARGO:-cargo}"
docker_bin="${DOCKER:-docker}"
reference_repo="${TS_REFERENCE_REPO:-https://github.com/earendil-works/pi.git}"
tracking_ref="${TS_PARITY_TRACKING_REF:-main}"
fixtures_dir="${TS_PARITY_FIXTURES_DIR:-tests/fixtures/ts-parity}"
out_dir="${TS_PARITY_DRIFT_DIR:-target/ts-parity-drift}"
fresh_fixtures_dir="${out_dir}/fixtures"
diff_file="${out_dir}/fixture.diff"
brief_file="${out_dir}/brief.md"

rm -rf "${out_dir}"
mkdir -p "${fresh_fixtures_dir}" "${fixtures_dir}"

"${make_bin}" ts-parity-build \
  TS_REFERENCE_REPO="${reference_repo}" \
  TS_REFERENCE_REF="${tracking_ref}"

"${docker_bin}" run --rm "${TS_PARITY_IMAGE:-pi-ts-parity}" \
  git -C /ts-reference rev-parse HEAD > "${out_dir}/reference-sha"
reference_sha="$(cat "${out_dir}/reference-sha")"

"${docker_bin}" run --rm \
  -v "${repo_root}/${fresh_fixtures_dir}:/fixtures" \
  "${TS_PARITY_IMAGE:-pi-ts-parity}"

if ! diff -ru "${fixtures_dir}" "${fresh_fixtures_dir}" > "${diff_file}"; then
  {
    printf '# TS Parity Drift Brief\n\n'
    printf 'The TypeScript reference fixtures changed against the tracked reference.\n\n'
    printf '## Reference\n\n'
    printf -- '- Repository: `%s`\n' "${reference_repo}"
    printf -- '- Ref: `%s`\n' "${tracking_ref}"
    printf -- '- Commit: `%s`\n' "${reference_sha}"
    printf -- '- Fixtures: `%s`\n' "${fixtures_dir}"
    printf -- '- Diff: `%s`\n\n' "${diff_file}"
    printf '## Constraints\n\n'
    printf -- '- Do not run npm on the host.\n'
    printf -- '- Execute TypeScript only through Docker targets such as `make ts-parity-fixtures`, `make ts-parity-update`, or `make ts-parity-drift`.\n'
    printf -- '- Keep changes scoped to Rust parity code, fixtures, tests, and docs required by the drift.\n'
    printf -- '- Run `cargo test -p noel-parity`, `cargo test -p noel-ai --lib matches_ts`, `cargo test -p noel-ai --lib catalog`, `cargo test -p noel-config --lib model`, `cargo test -p noel-cli --test model_parity`, `cargo test -p noel-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools`, `cargo test -p noel-core --lib system_prompt_matches_ts_builder`, `cargo test -p noel-cli --bin noel prompt_and_context_files_match_ts_discovery`, `cargo test -p noel-core --lib tool_parity_tests`, `cargo test -p noel-config --lib _ts`, `cargo test -p noel-core --lib input_expansion_matches_ts_session`, `cargo test -p noel-cli --test rpc_parity get_commands_matches_upstream`, `cargo test -p noel-cli --test rpc_parity golden_`, and `make check` before committing.\n'
    printf -- '- If a GitHub issue is provided, include `closes #<number>` in the commit message.\n\n'
    printf '## Suggested Workflow\n\n'
    printf '1. Inspect `tests/fixtures/ts-parity` and the diff below.\n'
    printf '2. Update Rust request builders/parsers to match the official TypeScript reference. `model-catalog.json` is the built-in model catalog itself: a diff there is new model data, compiled in as it is.\n'
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

  if [ -n "${NOEL_PARITY_AGENT_COMMAND:-}" ]; then
    printf 'Dispatching parity agent command: %s\n' "${NOEL_PARITY_AGENT_COMMAND}" >&2
    sh -c "${NOEL_PARITY_AGENT_COMMAND}" < "${brief_file}"
  fi

  exit 1
fi

{
  printf '# TS Parity Drift Brief\n\n'
  printf 'No fixture drift detected.\n\n'
  printf 'Reference: `%s` at `%s`.\n' "${reference_repo}" "${tracking_ref}"
} > "${brief_file}"

"${cargo_bin}" test -p noel-ai --lib matches_ts
"${cargo_bin}" test -p noel-ai --lib catalog
"${cargo_bin}" test -p noel-config --lib model
"${cargo_bin}" test -p noel-cli --test model_parity
"${cargo_bin}" test -p noel-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools
"${cargo_bin}" test -p noel-core --lib system_prompt_matches_ts_builder
"${cargo_bin}" test -p noel-cli --bin noel prompt_and_context_files_match_ts_discovery
"${cargo_bin}" test -p noel-core --lib tool_parity_tests
"${cargo_bin}" test -p noel-config --lib _ts
"${cargo_bin}" test -p noel-core --lib input_expansion_matches_ts_session
"${cargo_bin}" test -p noel-cli --test rpc_parity get_commands_matches_upstream
"${cargo_bin}" test -p noel-cli --test rpc_parity golden_
"${cargo_bin}" test -p noel-parity

printf 'No TS parity drift detected.\n'
printf 'Brief: %s\n' "${brief_file}"

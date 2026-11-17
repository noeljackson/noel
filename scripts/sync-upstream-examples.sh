#!/usr/bin/env bash
# Syncs upstream's extension examples into examples/extensions/typescript/.
#
# Only examples that run verbatim under noel's bun host are copied (see the
# non-parity register's bun-host amendment): factories whose surface is
# registrations, tool/command dispatch, and gates. Examples needing ctx.ui,
# rendering, providers, or the event bus stay upstream; this list changes
# only when the host's carried surface does.
#
# The provenance of each copy is recorded in .upstream-source.json so a
# vendored file can always be traced to its commit.
#
# Usage: scripts/sync-upstream-examples.sh [ref]
#   ref defaults to TS_REFERENCE_REF (default: main).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
reference_repo="${TS_REFERENCE_REPO:-https://github.com/earendil-works/pi.git}"
reference_ref="${1:-${TS_REFERENCE_REF:-main}}"
clone_dir="${TS_REFERENCE_CLONE:-${repo_root}/target/ts-reference-pi}"
out_dir="${repo_root}/examples/extensions/typescript"

# The curated list; each must run verbatim (tests in
# crates/noel-core/src/extensions.rs cover permission-gate and hello).
examples=(
  hello.ts
  notify.ts
  permission-gate.ts
  protected-paths.ts
  structured-output.ts
)

if [ ! -d "${clone_dir}/.git" ]; then
  git clone --filter=blob:none "${reference_repo}" "${clone_dir}"
fi
git -C "${clone_dir}" fetch --quiet origin "${reference_ref}"
git -C "${clone_dir}" checkout --quiet FETCH_HEAD
sha="$(git -C "${clone_dir}" rev-parse FETCH_HEAD)"

mkdir -p "${out_dir}"
copied=()
for name in "${examples[@]}"; do
  src="${clone_dir}/packages/coding-agent/examples/extensions/${name}"
  if [ ! -f "${src}" ]; then
    echo "missing upstream example: ${name}" >&2
    exit 1
  fi
  install -m 0644 "${src}" "${out_dir}/${name}"
  copied+=("${name}")
done

# Provenance: repo, ref, commit, and the copy's date. Upstream is MIT; the
# license travels with the copied files' origin repository.
python3 - "${out_dir}" "${reference_repo}" "${reference_ref}" "${sha}" "${copied[@]}" <<'EOF'
import json, sys, datetime
out_dir, repo, ref, sha, *files = sys.argv[1:]
source = {
    "repository": repo,
    "ref": ref,
    "commit": sha,
    "copied": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "files": files,
}
with open(f"{out_dir}/.upstream-source.json", "w") as handle:
    json.dump(source, handle, indent=2)
    handle.write("\n")
EOF

echo "synced ${#examples[@]} examples from ${reference_repo}@${reference_ref} (${sha})"
if command -v bun >/dev/null 2>&1; then
  shim="${repo_root}/crates/noel-cli/src/extension-hosts/noel-bun-host.ts"
  for name in "${examples[@]}"; do
    response="$(printf '{"protocolVersion":2,"kind":"init","input":"","cwd":"%s","sessionId":"check"}\n' "${out_dir}" \
      | bun "${shim}" "${out_dir}/${name}" 2>/dev/null | head -1)"
    if printf '%s' "${response}" | grep -q '"output":"ready"'; then
      echo "  ok: ${name}"
    else
      echo "  FAILED init: ${name}: ${response}" >&2
      exit 1
    fi
  done
else
  echo "bun not found; skipped the init check" >&2
fi

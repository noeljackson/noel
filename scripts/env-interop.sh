#!/usr/bin/env bash
# pi-env interop, both ways, without ssh:
#
# 1. Upstream's pi-env client (packages/env at the pin in the ts-parity image)
#    drives `noel-env serve`, which it starts directly through PI_ENV_DAEMON,
#    as upstream's CI tests its release daemons:
#    - Durable's env conformance suite over RemoteExecutionEnv and
#      LocalExecutionEnv (test/conformance.test.ts);
#    - remote.test.ts, differential.test.ts (RemoteExecutionEnv against
#      NodeExecutionEnv) and terminal.test.ts.
#    The same suites run against upstream's own daemon first, as the baseline.
#    noel-env is built static (musl), as upstream ships its Linux daemons, so
#    it runs in the image.
#
# 2. noel's client (noel_env::RemoteEnv, Connection and ssh deployment) drives
#    upstream's daemon: the client tests of crates/noel-env-daemon
#    (remote_env_conformance, remote_env, pi_env_client_golden, ssh_deploy)
#    run with NOEL_ENV_TEST_DAEMON pointing at upstream's pi-env, copied out of
#    the image (a static binary), after the same tests against noel-env serve
#    as the baseline. ssh_deploy deploys the daemon through a stand-in for ssh.
#
# This needs a Linux host. The tests run as the caller's uid, so permission
# cases behave as they do for a user.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
docker_bin="${DOCKER:-docker}"
image="${TS_PARITY_IMAGE:-pi-ts-parity}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
suites=(test/conformance.test.ts test/remote.test.ts test/differential.test.ts test/terminal.test.ts)

if [ "$(uname -s)" != "Linux" ]; then
  printf 'env-interop: needs a Linux host (the daemon runs in the Linux image)\n' >&2
  exit 2
fi
target="$(uname -m)-unknown-linux-musl"
if command -v rustup >/dev/null 2>&1 && ! rustup target list --installed | grep -qx "${target}"; then
  printf 'env-interop: the %s target is missing; run: rustup target add %s\n' "${target}" "${target}" >&2
  exit 2
fi
"${cargo_bin}" build -q -p noel-env-daemon --target "${target}"
"${cargo_bin}" build -q -p noel-env-daemon
daemon="${target_dir}/${target}/debug/noel-env"

work="$(mktemp -d "${TMPDIR:-/tmp}/env-interop.XXXXXX")"
container="env-interop-$$"
cleanup() {
  "${docker_bin}" rm -f "${container}-upstream" "${container}-noel" "${container}-summary" "${container}-copy" >/dev/null 2>&1 || true
  rm -rf "${work}"
}
trap cleanup EXIT

run_image() {
  local name="$1"
  shift
  "${docker_bin}" run --rm --name "${container}-${name}" --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "${daemon}:/noel/noel-env:ro" -v "${work}:/out" "$@"
}

# One run of the suites against the daemon at $2 (a path in the image).
run_suites() {
  local name="$1" path="$2"
  printf '== %s (%s)\n' "${name}" "$(run_image "${name}-version" "${image}" "${path}" --version)"
  set +e
  # The runner config loader needs no temporary file beside the root-owned config.
  run_image "${name}" -e PI_ENV_DAEMON="${path}" -w /chord-reference/packages/env "${image}" \
    /chord-reference/node_modules/.bin/vitest --run --configLoader runner "${suites[@]}" \
    --reporter=default --reporter=json --outputFile.json="/out/${name}.json" > "${work}/${name}.log" 2>&1
  local status=$?
  set -e
  grep -E 'Test Files|Tests |FAIL|✗|×' "${work}/${name}.log" | sed 's/\x1b\[[0-9;]*m//g' || true
  return "${status}"
}

upstream_status=0
run_suites upstream /usr/local/bin/pi-env || upstream_status=$?
noel_status=0
run_suites noel /noel/noel-env || noel_status=$?

# Counts per file, and every test whose outcome differs from upstream's.
run_image summary "${image}" node -e '
const fs = require("node:fs");
const read = (name) => JSON.parse(fs.readFileSync(`/out/${name}.json`, "utf8"));
const outcomes = (report) => {
  const map = new Map();
  for (const file of report.testResults) {
    for (const test of file.assertionResults) map.set(`${file.name.split("/").pop()} > ${test.fullName}`, test.status);
  }
  return map;
};
const upstream = read("upstream");
const noel = read("noel");
for (const [name, report] of [["upstream", upstream], ["noel", noel]]) {
  console.log(`${name}: ${report.numPassedTests} passed, ${report.numFailedTests} failed, ${report.numPendingTests} skipped, of ${report.numTotalTests}`);
}
const before = outcomes(upstream);
let differences = 0;
for (const [test, status] of outcomes(noel)) {
  if (before.get(test) !== status) {
    differences++;
    console.log(`differs: ${test}: upstream ${before.get(test)}, noel ${status}`);
  }
}
console.log(`tests whose outcome differs from upstream: ${differences}`);
'

printf '== upstream client: against upstream exit %s, against noel exit %s\n' "${upstream_status}" "${noel_status}"

# noel's client against noel's daemon (the baseline), then against upstream's.
client_tests=(--test remote_env_conformance --test remote_env --test pi_env_client_golden --test ssh_deploy)
run_client_tests() {
  local name="$1" daemon_path="$2"
  printf '== noel client against %s (%s)\n' "${name}" "$("${daemon_path}" --version)"
  set +e
  NOEL_ENV_TEST_DAEMON="${daemon_path}" NOEL_TEST_ISOLATION=1 \
    "${cargo_bin}" test -q -p noel-env-daemon "${client_tests[@]}" > "${work}/client-${name}.log" 2>&1
  local status=$?
  set -e
  grep -E '^test result|FAILED|panicked' "${work}/client-${name}.log" || true
  return "${status}"
}
"${docker_bin}" run --rm --name "${container}-copy" --user "$(id -u):$(id -g)" -v "${work}:/out" "${image}" \
  cp /usr/local/bin/pi-env /out/pi-env
client_noel_status=0
run_client_tests noel "${target_dir}/debug/noel-env" || client_noel_status=$?
client_upstream_status=0
run_client_tests upstream "${work}/pi-env" || client_upstream_status=$?
printf '== noel client: against noel exit %s, against upstream exit %s\n' "${client_noel_status}" "${client_upstream_status}"

[ "${noel_status}" -eq 0 ] && [ "${client_noel_status}" -eq 0 ] && [ "${client_upstream_status}" -eq 0 ]

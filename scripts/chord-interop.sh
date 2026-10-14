#!/usr/bin/env bash
# Chord interop between noel-chord and upstream's packages/chord (the pin in
# the ts-parity image), over real WebSockets on Unix sockets:
#
#   1. upstream's client host holds the counter on noel's device-server,
#      subscribes, calls `add`, and sees the snapshot, then ops;
#   2. the same client holds and is killed: the server log shows its hold
#      released and its link closed;
#   3. noel's chord-client does the same against upstream's server host.
#
# The noel side runs on the host and the upstream side in Docker, sharing a
# socket directory; the container runs as the caller's uid, which the device
# server's peer-uid check requires. No port is opened.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
docker_bin="${DOCKER:-docker}"
image="${TS_PARITY_IMAGE:-pi-ts-parity}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"

work="$(mktemp -d "${TMPDIR:-/tmp}/chord-interop.XXXXXX")"
chmod 700 "${work}"
server_pid=""
container="chord-interop-$$"
cleanup() {
  if [ -n "${server_pid}" ]; then kill "${server_pid}" 2>/dev/null || true; fi
  "${docker_bin}" rm -f "${container}-hold" "${container}-server" >/dev/null 2>&1 || true
  rm -rf "${work}"
}
trap cleanup EXIT

"${cargo_bin}" build -q -p noel-chord --example device-server --example chord-client

run_node() {
  "${docker_bin}" run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "${work}:/sock" -v "${repo_root}/scripts/ts-parity:/ts-parity:ro" \
    "$@"
}

wait_for() {
  local file="$1" pattern="$2" what="$3"
  for _ in $(seq 1 600); do
    if grep -q "${pattern}" "${file}" 2>/dev/null; then return 0; fi
    sleep 0.2
  done
  printf 'chord-interop: timed out waiting for %s\n' "${what}" >&2
  cat "${file}" >&2 || true
  return 1
}

status=0

# 1. upstream client -> noel server
"${target_dir}/debug/examples/device-server" --socket "${work}/device.sock" > "${work}/server.log" 2>&1 &
server_pid=$!
wait_for "${work}/server.log" '^listening' "the noel device server"
set +e
run_node "${image}" npx tsx /ts-parity/chord-interop-client.ts "ws+unix:///sock/device.sock:/chord" \
  > "${work}/client.log" 2>&1
client_exit=$?
set -e
printf '== upstream client -> noel device-server: exit %s\n' "${client_exit}"
grep -v '^npm ' "${work}/client.log" || true
[ "${client_exit}" -eq 0 ] || status=1

# 2. a held client killed: its hold is released
set +e
run_node --name "${container}-hold" "${image}" npx tsx /ts-parity/chord-interop-client.ts \
  "ws+unix:///sock/device.sock:/chord" --hold > "${work}/hold.log" 2>&1 &
hold_pid=$!
set -e
wait_for "${work}/hold.log" '"holding":true' "the held client"
"${docker_bin}" kill "${container}-hold" >/dev/null
wait "${hold_pid}" 2>/dev/null || true
sleep 1
kill -INT "${server_pid}"
wait "${server_pid}" 2>/dev/null || true
server_pid=""
printf '== noel device-server log\n'
cat "${work}/server.log"
held_link="$(grep -o '^hold link [0-9]*' "${work}/server.log" | tail -1 | awk '{print $3}')"
if [ -n "${held_link}" ] \
  && grep -q "^release link ${held_link} id 1:" "${work}/server.log" \
  && grep -q "^link ${held_link} closed: " "${work}/server.log"; then
  printf '== killed client (link %s): hold released: yes\n' "${held_link}"
else
  printf '== killed client: hold released: NO\n'
  status=1
fi

# 3. noel client -> upstream server
run_node --name "${container}-server" "${image}" npx tsx /ts-parity/chord-interop-server.ts /sock/ts.sock \
  > "${work}/ts-server.log" 2>&1 &
ts_server_pid=$!
wait_for "${work}/ts-server.log" '"listening"' "the upstream server"
set +e
"${target_dir}/debug/examples/chord-client" --socket "${work}/ts.sock" > "${work}/noel-client.log" 2>&1
noel_exit=$?
wait "${ts_server_pid}"
ts_server_exit=$?
set -e
printf '== noel chord-client -> upstream server: exit %s (server exit %s)\n' "${noel_exit}" "${ts_server_exit}"
cat "${work}/noel-client.log"
printf '== upstream server received\n'
grep -v '^npm ' "${work}/ts-server.log" || true
[ "${noel_exit}" -eq 0 ] && [ "${ts_server_exit}" -eq 0 ] || status=1

exit "${status}"

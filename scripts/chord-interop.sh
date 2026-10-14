#!/usr/bin/env bash
# Chord interop between noel-chord and upstream's packages/chord (the pin in
# the ts-parity image), over real WebSockets on Unix sockets:
#
#   1. upstream's client host holds the counter on noel's device-server,
#      subscribes, calls `add`, and sees the snapshot, then ops;
#   2. the same client holds and is killed: the server log shows its hold
#      released and its link closed;
#   3. noel's chord-client does the same against upstream's server host;
#   4. upstream's client host holds `noel.chat` on `noel server` (relayed to
#      the session's worker), sees the view's snapshot, then the ops that a
#      `noel client send` makes.
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
noel_bin="${target_dir}/debug/noel"
noel_env=(
  HOME="${work}/m2/home" NOEL_DIR="${work}/m2/agent" NOEL_OFFLINE=1
  NOEL_FAUX_SCRIPT="${work}/m2/script.json" NOEL_FAUX_SCRIPT_LOG="${work}/m2/requests.jsonl"
  NOEL_RUNTIME_DIR="${work}/run"
)
# Kills the noel server and workers step 4 started, by their records.
stop_noel() {
  local record pid
  for record in "${work}/run/server.json" "${work}/run/workers/"*.json; do
    [ -f "${record}" ] || continue
    pid="$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "${record}")"
    if [ -n "${pid}" ]; then kill -9 "${pid}" 2>/dev/null || true; fi
  done
}
cleanup() {
  if [ -n "${server_pid}" ]; then kill "${server_pid}" 2>/dev/null || true; fi
  stop_noel
  "${docker_bin}" rm -f "${container}-hold" "${container}-server" "${container}-chat" >/dev/null 2>&1 || true
  rm -rf "${work}"
}
trap cleanup EXIT

"${cargo_bin}" build -q -p noel-chord --example device-server --example chord-client
"${cargo_bin}" build -q -p noel-cli

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

# 4. upstream client -> noel server -> worker: noel.chat's view
mkdir -p "${work}/m2/home" "${work}/m2/agent" "${work}/m2/cwd"
mkdir -m 700 "${work}/run"
printf '%s\n' '{"modelRefresh":{"enabled":false},"retry":{"enabled":false}}' > "${work}/m2/agent/settings.json"
printf '%s\n' '[[{"text":"first reply"}],[{"text":"second reply"}]]' > "${work}/m2/script.json"
noel() { (cd "${work}/m2/cwd" && env "${noel_env[@]}" "${noel_bin}" "$@"); }
session="$(noel client new --model faux/script --request-id interop)"
noel client send "${session}" "hello from noel" >/dev/null
for _ in $(seq 1 100); do
  if grep -q '"run_finished"' "${work}/m2/agent/sessions/${session}.jsonl" 2>/dev/null; then break; fi
  sleep 0.1
done
set +e
run_node --name "${container}-chat" "${image}" npx tsx /ts-parity/chord-interop-chat.ts \
  "ws+unix:///sock/run/server.sock:/chord" "${session}" > "${work}/chat.log" 2>&1 &
chat_pid=$!
set -e
wait_for "${work}/chat.log" '"subscribed":true' "the upstream client's snapshot of noel.chat"
noel client send "${session}" "hello again" >/dev/null
set +e
wait "${chat_pid}"
chat_exit=$?
set -e
printf '== upstream client -> noel server noel.chat (session %s): exit %s\n' "${session}" "${chat_exit}"
grep -v '^npm ' "${work}/chat.log" || true
[ "${chat_exit}" -eq 0 ] || status=1
stop_noel

exit "${status}"

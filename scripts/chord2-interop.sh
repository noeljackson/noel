#!/usr/bin/env bash
# Chord v2 interop between noel and upstream's packages/chord at
# CHORD2_REFERENCE_REF, over WebSockets on Unix sockets, with the upstream side
# run by Bun from a reference checkout (CHORD2_REFERENCE_DIR, as for
# `make chord2-fixtures`):
#
#   1. upstream's client host holds `noel.chat` on `noel server` (relayed to
#      the session's worker): the view, a submit that runs (faux model), the
#      ops up to the settled run; then its link drops and it holds again and
#      calls over the next one;
#   2. noel's chord2-client example holds upstream's `interop.counter`:
#      snapshot, a call and its ops, a burst of commits faster than the
#      window (the consumer catches up to the last value), then a dropped
#      link and a re-hold.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
reference="${CHORD2_REFERENCE_DIR:?set CHORD2_REFERENCE_DIR to a pi checkout at CHORD2_REFERENCE_REF}"
cargo_bin="${CARGO:-cargo}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
bun_bin="${BUN:-bun}"

work="$(mktemp -d "${TMPDIR:-/tmp}/chord2-interop.XXXXXX")"
chmod 700 "${work}"
serve_pid=""
noel_bin="${target_dir}/debug/noel"
noel_env=(
  HOME="${work}/home" NOEL_DIR="${work}/agent" NOEL_OFFLINE=1
  NOEL_FAUX_SCRIPT="${work}/script.json" NOEL_FAUX_SCRIPT_LOG="${work}/requests.jsonl"
  NOEL_RUNTIME_DIR="${work}/run"
)
stop_noel() {
  local record pid
  for record in "${work}/run/server.json" "${work}/run/workers/"*.json; do
    [ -f "${record}" ] || continue
    pid="$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "${record}")"
    if [ -n "${pid}" ]; then kill -9 "${pid}" 2>/dev/null || true; fi
  done
}
cleanup() {
  if [ -n "${serve_pid}" ]; then kill "${serve_pid}" 2>/dev/null || true; fi
  stop_noel
  rm -rf "${work}"
}
trap cleanup EXIT

expect() {
  local file="$1" pattern="$2"
  if ! grep -q -- "${pattern}" "${file}"; then
    echo "chord2-interop: expected ${pattern} in ${file}:" >&2
    cat "${file}" >&2
    exit 1
  fi
}

"${cargo_bin}" build -q -p noel-cli
"${cargo_bin}" build -q -p noel-chord --example chord2-client

# 1. upstream's client host on `noel server`.
mkdir -p "${work}/home" "${work}/agent" "${work}/cwd"
printf '%s' '{"modelRefresh":{"enabled":false},"retry":{"enabled":false}}' >"${work}/agent/settings.json"
printf '%s' '[[{"text":"Hello from noel."}]]' >"${work}/script.json"
session="$(env "${noel_env[@]}" "${noel_bin}" client new --model faux/script --cwd "${work}/cwd" --request-id interop)"
"${bun_bin}" --conditions=source scripts/ts-parity/chord2-interop.ts "${reference}" crates/noel-client/contracts/v2 \
  chat "${work}/run/server.sock" "${session}" >"${work}/chat.out" 2>"${work}/chat.err" || {
  cat "${work}/chat.out" "${work}/chat.err" >&2
  tail -40 "${work}/run/server.log" >&2 || true
  exit 1
}
expect "${work}/chat.out" '"step":"held","variant":"participant","entries":0'
expect "${work}/chat.out" '"step":"submitted","disposition":"started"'
expect "${work}/chat.out" '"step":"settled","entries":2,"last":"pi.assistant"'
expect "${work}/chat.out" '"step":"reheld","entries":2'
echo "chord2-interop: upstream client host on noel server: ok"

# 2. noel's client on upstream's server host.
socket="${work}/upstream.sock"
"${bun_bin}" --conditions=source scripts/ts-parity/chord2-interop.ts "${reference}" crates/noel-client/contracts/v2 \
  serve "${socket}" scripts/ts-parity/chord2-interop-counter.json >"${work}/serve.out" 2>"${work}/serve.err" &
serve_pid=$!
for _ in $(seq 100); do [ -S "${socket}" ] && break; sleep 0.1; done
"${target_dir}/debug/examples/chord2-client" "${socket}" scripts/ts-parity/chord2-interop-counter.json \
  >"${work}/client.out" 2>&1 || {
  cat "${work}/client.out" "${work}/serve.out" "${work}/serve.err" >&2
  exit 1
}
expect "${work}/client.out" 'snapshot 0'
expect "${work}/client.out" 'after add 5'
expect "${work}/client.out" 'after burst 205'
expect "${work}/client.out" 'reheld 0 then 1'
echo "chord2-interop: noel client on upstream server host: ok"

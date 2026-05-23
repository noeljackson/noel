#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
log_dir="${repo_root}/target/dogfood-dev"
log_file="${log_dir}/watch.log"
restart_exe="${repo_root}/target/debug/pi"
watcher_pid=""
cargo_bin="${CARGO:-cargo}"

cleanup() {
  if [ -n "${watcher_pid}" ]; then
    kill "${watcher_pid}" >/dev/null 2>&1 || true
    wait "${watcher_pid}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "${log_dir}"
: > "${log_file}"

source_fingerprint() {
  {
    find "${repo_root}/crates" -type f \( -name '*.rs' -o -name 'Cargo.toml' \) -printf '%p %T@\n'
    for path in "${repo_root}/Cargo.toml" "${repo_root}/Cargo.lock"; do
      if [ -f "${path}" ]; then
        stat --printf '%n %Y\n' "${path}"
      fi
    done
  } | sort | sha256sum | awk '{print $1}'
}

build_once() {
  {
    printf '\n[%s] cargo build -p pi-cli\n' "$(date -Is)"
    "${cargo_bin}" build -p pi-cli
  } >> "${log_file}" 2>&1
}

watch_loop() {
  local last
  last="$(source_fingerprint)"
  while sleep 1; do
    local current
    current="$(source_fingerprint)"
    if [ "${current}" != "${last}" ]; then
      last="${current}"
      build_once || true
    fi
  done
}

cd "${repo_root}"
build_once || true
watch_loop &
watcher_pid="$!"

printf 'dogfood watcher log: %s\n' "${log_file}" >&2
PI_DOGFOOD_AUTO_RESTART=1 PI_DOGFOOD_RESTART_EXE="${restart_exe}" "${cargo_bin}" run -p pi-cli -- "$@"

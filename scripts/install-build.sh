#!/usr/bin/env bash
set -euo pipefail

cargo_cmd="${CARGO:-cargo}"

if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
  cargo_path="$(command -v "${cargo_cmd}")"
  exec sudo -u "${SUDO_USER}" env \
    PATH="${PATH}" \
    "${cargo_path}" build --release -p pi-cli
fi

exec "${cargo_cmd}" build --release -p pi-cli

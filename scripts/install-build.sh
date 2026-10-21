#!/usr/bin/env bash
set -euo pipefail

cargo_cmd="${CARGO:-cargo}"

# Under sudo (make install-system), build as the invoking user: keep their
# cargo on PATH, not sudo's secure_path.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
  user_home="$(getent passwd "${SUDO_USER}" | cut -d: -f6)"
  exec sudo -u "${SUDO_USER}" env \
    PATH="${user_home}/.cargo/bin:${PATH}" \
    "${cargo_cmd}" build --release -p noel-cli
fi

exec "${cargo_cmd}" build --release -p noel-cli

#!/usr/bin/env bash
# Builds the noel-env daemons that remote workspaces upload (`noel --workspace
# ssh://…`): static musl binaries in the env-daemon profile, named as noel
# looks them up beside itself, into ENV_DAEMONS_DIR (default
# target/env-daemons):
#
#   noel-env-linux-x64     x86_64-unknown-linux-musl (required)
#   noel-env-linux-arm64   aarch64-unknown-linux-musl, linked with
#                          ENV_DAEMON_ARM64_LINKER (default
#                          aarch64-linux-gnu-gcc); skipped, saying why,
#                          when that linker or the Rust target is missing
#
# Darwin daemons need a Mac build and are not made here; a darwin remote gets
# noel's missing-daemon error.
set -euo pipefail

cargo_cmd="${CARGO:-cargo}"

# Under sudo (make install-system), build as the invoking user.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
  exec sudo -u "${SUDO_USER}" env PATH="${PATH}" CARGO="${cargo_cmd}" \
    ENV_DAEMONS_DIR="${ENV_DAEMONS_DIR:-}" ENV_DAEMON_ARM64_LINKER="${ENV_DAEMON_ARM64_LINKER:-}" \
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}" "$0" "$@"
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
out="${ENV_DAEMONS_DIR:-target/env-daemons}"
target_dir="${CARGO_TARGET_DIR:-target}"
arm64_linker="${ENV_DAEMON_ARM64_LINKER:-aarch64-linux-gnu-gcc}"

# Whether the Rust target is installed; without rustup, assume it is and let
# cargo say otherwise.
has_target() {
  if ! command -v rustup >/dev/null 2>&1; then
    return 0
  fi
  rustup target list --installed | grep -qx "$1"
}

build() {
  local triple="$1" name="$2"
  "${cargo_cmd}" build --profile env-daemon -p noel-env-daemon --target "${triple}"
  install -m 0755 "${target_dir}/${triple}/env-daemon/noel-env" "${out}/${name}"
  printf 'env-daemons: %s/%s\n' "${out}" "${name}"
}

mkdir -p "${out}"
rm -f "${out}"/noel-env-*

if ! has_target x86_64-unknown-linux-musl; then
  printf 'env-daemons: the x86_64-unknown-linux-musl target is missing; run: rustup target add x86_64-unknown-linux-musl\n' >&2
  exit 1
fi
build x86_64-unknown-linux-musl noel-env-linux-x64

if ! command -v "${arm64_linker}" >/dev/null 2>&1; then
  printf 'env-daemons: skipping noel-env-linux-arm64: no %s (set ENV_DAEMON_ARM64_LINKER to an aarch64 linker)\n' "${arm64_linker}"
elif ! has_target aarch64-unknown-linux-musl; then
  printf 'env-daemons: skipping noel-env-linux-arm64: the aarch64-unknown-linux-musl target is missing (rustup target add aarch64-unknown-linux-musl)\n'
else
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${arm64_linker}" \
    build aarch64-unknown-linux-musl noel-env-linux-arm64
fi
printf 'env-daemons: darwin daemons are not built here (they need a Mac build)\n'

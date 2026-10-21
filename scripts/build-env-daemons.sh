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

# Under sudo (make install / install-system), build as the invoking user: keep
# their cargo on PATH, not sudo's secure_path (/usr/bin/cargo may be a Rust
# without the musl std this build needs).
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
  user_home="$(getent passwd "${SUDO_USER}" | cut -d: -f6)"
  reexec=(sudo -u "${SUDO_USER}" env PATH="${user_home}/.cargo/bin:${PATH}" CARGO="${cargo_cmd}"
    ENV_DAEMONS_DIR="${ENV_DAEMONS_DIR:-}" ENV_DAEMON_ARM64_LINKER="${ENV_DAEMON_ARM64_LINKER:-}")
  # An empty CARGO_TARGET_DIR makes cargo fail; pass it only when set.
  [ -n "${CARGO_TARGET_DIR:-}" ] && reexec+=(CARGO_TARGET_DIR="${CARGO_TARGET_DIR}")
  exec "${reexec[@]}" "$0" "$@"
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
out="${ENV_DAEMONS_DIR:-target/env-daemons}"
target_dir="${CARGO_TARGET_DIR:-target}"
arm64_linker="${ENV_DAEMON_ARM64_LINKER:-aarch64-linux-gnu-gcc}"

# Whether the Rust target's std is installed, straight from the toolchain this
# cargo uses (works with or without rustup). cargo and rustc normally sit in
# the same directory; fall back to a PATH lookup.
# Whether the Rust target's std is installed for the given cargo's toolchain
# (works with or without rustup). cargo and rustc normally sit in the same
# directory; fall back to a PATH lookup.
sysroot_has_target() {
  rustc_bin="$(dirname "$(command -v "$1")")/rustc"
  command -v "${rustc_bin}" >/dev/null 2>&1 || rustc_bin=rustc
  sysroot="$("${rustc_bin}" --print sysroot 2>/dev/null)" || return 1
  [ -n "${sysroot}" ] && [ -d "${sysroot}/lib/rustlib/$2" ]
}

has_target() { sysroot_has_target "${cargo_cmd}" "$1"; }

# Shells sometimes lose ~/.cargo/bin (sudo's secure_path, env -i, a stripped
# rc): if the cargo on PATH can't build the static daemons but the invoking
# user's rustup can, use it.
if ! has_target x86_64-unknown-linux-musl; then
  rustup_cargo="${HOME:-}/.cargo/bin/cargo"
  if [ -x "${rustup_cargo}" ] && [ "$(command -v "${cargo_cmd}")" != "${rustup_cargo}" ] \
    && sysroot_has_target "${rustup_cargo}" x86_64-unknown-linux-musl; then
    printf 'env-daemons: using %s; the cargo on PATH lacks the musl target\n' "${rustup_cargo}" >&2
    cargo_cmd="${rustup_cargo}"
  fi
fi

build() {
  local triple="$1" name="$2"
  "${cargo_cmd}" build --profile env-daemon -p noel-env-daemon --target "${triple}"
  install -m 0755 "${target_dir}/${triple}/env-daemon/noel-env" "${out}/${name}"
  printf 'env-daemons: %s/%s\n' "${out}" "${name}"
}

mkdir -p "${out}"
rm -f "${out}"/noel-env-*

if ! has_target x86_64-unknown-linux-musl; then
  printf 'env-daemons: the x86_64-unknown-linux-musl target is not installed for the toolchain of %s\n' "$(command -v "${cargo_cmd}")" >&2
  printf "env-daemons: put rustup's cargo first in PATH (or set CARGO), or run: rustup target add x86_64-unknown-linux-musl\n" >&2
  exit 1
fi
build x86_64-unknown-linux-musl noel-env-linux-x64

if ! command -v "${arm64_linker}" >/dev/null 2>&1; then
  printf 'env-daemons: skipping noel-env-linux-arm64: no %s (set ENV_DAEMON_ARM64_LINKER to an aarch64 linker)\n' "${arm64_linker}"
elif ! has_target aarch64-unknown-linux-musl; then
  printf 'env-daemons: skipping noel-env-linux-arm64: the aarch64-unknown-linux-musl target is not installed for this toolchain (rustup target add aarch64-unknown-linux-musl)\n'
else
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${arm64_linker}" \
    build aarch64-unknown-linux-musl noel-env-linux-arm64
fi
printf 'env-daemons: darwin daemons are not built here (they need a Mac build)\n'

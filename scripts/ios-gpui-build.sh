#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
profile="${PROFILE:-release}"
bundle="${target_dir}/ios/Noel.app"

if [ "${profile}" != "release" ] && [ "${profile}" != "debug" ]; then
  printf 'PROFILE must be release or debug\n' >&2
  exit 2
fi
flag=()
[ "${profile}" = "release" ] && flag=(--release)

rustup target add aarch64-apple-ios-sim >/dev/null
toolchain="$(rustup show active-toolchain | cut -d' ' -f1)"
toolchain_bin="$(dirname "$(rustup which rustc)")"
PATH="${toolchain_bin}:${PATH}"
export PATH
CARGO_TARGET_DIR="${target_dir}"
export CARGO_TARGET_DIR
IPHONEOS_DEPLOYMENT_TARGET=17.0 \
  rustup run "${toolchain}" "${CARGO:-cargo}" build \
  --manifest-path "${repo_root}/apps/ios/Cargo.toml" \
  --target aarch64-apple-ios-sim "${flag[@]}"

mkdir -p "${bundle}"
cp "${target_dir}/aarch64-apple-ios-sim/${profile}/noel-ios" "${bundle}/noel-ios"
cp "${repo_root}/apps/ios/App/Info.plist" "${bundle}/Info.plist"
codesign --force --sign - "${bundle}"
printf 'Built simulator app: %s\n' "${bundle}"

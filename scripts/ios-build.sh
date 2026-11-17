#!/usr/bin/env bash
# The iOS app (roadmap phase 10).
#
#   scripts/ios-build.sh libs   any host: noel-ffi's static libraries for
#                               aarch64-apple-ios and aarch64-apple-ios-sim,
#                               and its Swift bindings into NoelKit;
#   scripts/ios-build.sh app    macOS with Xcode and xcodegen: the above,
#                               then NoelFFI.xcframework and the app built
#                               for the simulator.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
cargo_bin="${CARGO:-cargo}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
profile="${PROFILE:-release}"
ios="${repo_root}/apps/ios"
mode="${1:-libs}"

flag=()
[ "${profile}" = "release" ] && flag=(--release)
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
  rustup target add "${target}" >/dev/null
  # The static library only: linking the cdylib needs Apple's linker.
  "${cargo_bin}" rustc -q -p noel-ffi --lib --crate-type staticlib --target "${target}" "${flag[@]}"
done

# Bindings from a host build of the library (its metadata is the same).
"${cargo_bin}" build -q -p noel-ffi --lib "${flag[@]}"
library=""
for candidate in "${target_dir}/${profile}/libnoel_ffi.dylib" "${target_dir}/${profile}/libnoel_ffi.so"; do
  if [ -f "${candidate}" ]; then library="${candidate}"; fi
done
generated="${target_dir}/noel-ffi-swift"
rm -rf "${generated}"
"${cargo_bin}" run -q -p noel-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library "${library}" --language swift --out-dir "${generated}"
cp "${generated}/noel_ffi.swift" "${ios}/NoelKit/Sources/NoelKit/Generated/noel_ffi.swift"
# The C header must parse: UniFFI once escaped a Swift keyword (`open`) there.
if command -v clang > /dev/null; then
  clang -fsyntax-only -Wno-pragma-once-outside-header -x c "${generated}/noel_ffiFFI.h"
fi
echo "ios-build: static libraries and NoelKit's bindings are up to date"

[ "${mode}" = "app" ] || exit 0
headers="${generated}/headers"
mkdir -p "${headers}"
cp "${generated}/noel_ffiFFI.h" "${headers}/"
cp "${generated}/noel_ffiFFI.modulemap" "${headers}/module.modulemap"
rm -rf "${ios}/build/NoelFFI.xcframework"
xcodebuild -create-xcframework \
  -library "${target_dir}/aarch64-apple-ios/${profile}/libnoel_ffi.a" -headers "${headers}" \
  -library "${target_dir}/aarch64-apple-ios-sim/${profile}/libnoel_ffi.a" -headers "${headers}" \
  -output "${ios}/build/NoelFFI.xcframework"
cd "${ios}"
xcodegen generate
xcodebuild -project Noel.xcodeproj -scheme Noel \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO ARCHS=arm64 build
echo "ios-build: the app builds for the simulator"

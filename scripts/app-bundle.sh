#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source="${repo_root}/apps/desktop"
bundle="${repo_root}/target/desktop/Noel.app"

mkdir -p "${bundle}/Contents/MacOS" "${bundle}/Contents/Resources"
cp "${source}/Info.plist" "${bundle}/Contents/Info.plist"
cp "${repo_root}/target/desktop/release/noel-app" "${bundle}/Contents/MacOS/noel-app"
cp "${repo_root}/target/debug/noel" "${bundle}/Contents/MacOS/noel"
chmod 0755 "${bundle}/Contents/MacOS/noel-app" "${bundle}/Contents/MacOS/noel"
codesign --force --sign - "${bundle}"
printf 'Built %s\n' "${bundle}"

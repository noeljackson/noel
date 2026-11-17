#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bundle="${repo_root}/target/desktop/Noel.app"
pid=""

# shellcheck disable=SC2329
cleanup() {
  if [ -n "${pid}" ]; then kill -TERM "${pid}" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

open -n "${bundle}"
for _ in $(seq 1 80); do
  if [ -z "${pid}" ]; then pid="$(pgrep -n -f 'Noel.app/Contents/MacOS/noel-app' || true)"; fi
  if [ -n "${pid}" ]; then
    windows="$(swift -e 'import CoreGraphics
let options: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
let windows = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] ?? []
let matching = windows.filter { window in
  let owner = window[kCGWindowOwnerName as String] as? String ?? ""
  let pid = window[kCGWindowOwnerPID as String] as? Int ?? 0
  return owner == "noel-app" || owner == "Noel" || pid == Int(CommandLine.arguments[1])
}
print(matching.count)' "${pid}")"
    if [ "${windows}" -gt 0 ]; then
      printf 'window-smoke: %s visible window(s)\n' "${windows}"
      exit 0
    fi
  fi
  sleep 0.25
done
printf 'window-smoke: no visible window appeared\n' >&2
exit 1

#!/usr/bin/env bash
# The desktop app (apps/desktop, roadmap phase 9) against a real `noel
# server` with a faux model and the sample plugin, without a window:
#
#   1. `noel-app --smoke --session <id>` through `noel bridge`: the directory,
#      the plugins and the session's view;
#   2. the same through `noel app` (which finds noel-app on the PATH);
#   3. `--remote` through a stand-in ssh that runs the bridge here: the path
#      a remote server takes.
#
# With SCREENSHOT=<file>, the window is also drawn under Xvfb (software
# Vulkan) and captured, to look at.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
cargo_bin="${CARGO:-cargo}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
"${cargo_bin}" build -q -p noel-cli
(cd apps/desktop && CARGO_TARGET_DIR="${target_dir}/desktop" "${cargo_bin}" build -q)
noel="${target_dir}/debug/noel"
app="${target_dir}/desktop/debug/noel-app"

work="$(mktemp -d /tmp/noel-app.XXXXXX)"
chmod 700 "${work}"
cleanup() {
  local record pid
  for record in "${work}/run/server.json" "${work}/run/workers/"*.json; do
    [ -f "${record}" ] || continue
    pid="$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "${record}")"
    if [ -n "${pid}" ]; then kill -9 "${pid}" 2>/dev/null || true; fi
  done
  rm -rf "${work}"
}
trap cleanup EXIT

mkdir -p "${work}/home" "${work}/agent/plugins/sample" "${work}/cwd" "${work}/bin"
printf '%s' '{"modelRefresh":{"enabled":false},"retry":{"enabled":false}}' >"${work}/agent/settings.json"
printf '%s' '[[{"text":"Hello from noel."}]]' >"${work}/script.json"
printf '{"name":"sample","command":["%s"],"permissions":["tools","commands","panels"]}' \
  "${target_dir}/debug/noel-sample-plugin" >"${work}/agent/plugins/sample/plugin.json"
printf '%s' '{"sample":{"permissions":["tools","commands","panels"]}}' >"${work}/agent/plugins.json"
cat >"${work}/bin/ssh" <<'SSH'
#!/bin/sh
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift 2
exec sh -c "$*"
SSH
chmod +x "${work}/bin/ssh"
export HOME="${work}/home" NOEL_DIR="${work}/agent" NOEL_FAUX_SCRIPT="${work}/script.json" \
  NOEL_OFFLINE=1 NOEL_RUNTIME_DIR="${work}/run"
cd "${work}/cwd"
session="$("${noel}" client new --model faux/script)"
"${noel}" client send "${session}" "hello app" >/dev/null
for _ in $(seq 100); do
  grep -q '"run_finished"' "${work}/agent/sessions/${session}.jsonl" 2>/dev/null && break
  sleep 0.1
done

check() {
  local label="$1" out="$2"
  if ! grep -q '"sessions":1,"plugins":1,"view":{"session":"'"${session}"'","entries":2}' "${out}"; then
    echo "app-smoke: ${label}:" >&2
    cat "${out}" >&2
    exit 1
  fi
  echo "app-smoke: ${label}: ok"
}
timeout 60 "${app}" --smoke --noel "${noel}" --session "${session}" >"${work}/local.out"
check "local" "${work}/local.out"
PATH="$(dirname "${app}"):${PATH}" timeout 60 "${noel}" app --smoke --session "${session}" >"${work}/noel-app.out"
check "noel app" "${work}/noel-app.out"
PATH="${work}/bin:${PATH}" timeout 60 "${app}" --smoke --remote devbox --noel "${noel}" \
  --session "${session}" >"${work}/remote.out"
check "remote" "${work}/remote.out"

if [ -n "${SCREENSHOT:-}" ]; then
  export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"
  Xvfb :91 -screen 0 1280x800x24 >/dev/null 2>&1 &
  xvfb=$!
  sleep 1
  DISPLAY=:91 "${app}" --noel "${noel}" --session "${session}" >/dev/null 2>&1 &
  window=$!
  sleep 6
  DISPLAY=:91 import -window root "${SCREENSHOT}"
  kill "${window}" "${xvfb}" 2>/dev/null || true
  echo "app-smoke: window drawn to ${SCREENSHOT}"
fi

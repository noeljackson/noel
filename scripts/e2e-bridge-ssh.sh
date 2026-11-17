#!/usr/bin/env bash
# A client reaching noel server on another machine over ssh, end to end
# (docs/spec.md §6), in one container with no network: `make bridge-journey`.
#
# It builds a static musl noel (make noel-static), then runs Alpine 3.21,
# pinned by digest, with `--network none` and `--init`. Inside it, sshd
# listens on 127.0.0.1:2222 with host keys made there. The "remote" user
# `owner` has noel on its PATH and its sessions; the "local" user `client`
# runs `noel client --remote box` and `noel --attach --remote box`, where
# `box` is a Host in its own ~/.ssh/config, with a throwaway key made there
# that `owner` authorizes. Nothing on this machine's ssh configuration,
# keys, known hosts, sshd or firewall is read or changed.
#
# Each step asserts what it observes and the script stops at the first
# failure:
#   1. an unknown host key: ssh refuses, the error says so, and no server
#      starts;
#   2. `ls` through the bridge starts `owner`'s server, which lists no
#      sessions; `client` runs no server;
#   3. `new` and `send`: the run's bash runs on the remote as `owner`, in
#      its directory, and the journal is `owner`'s;
#   4. `watch` shows the reply, and the server stamped the bridged links
#      `via: "tunnel"`; `owner`'s own client is `via: "device"`;
#   5. killing the client's ssh mid-watch: the watch reconnects (ssh in
#      batch mode) and gets a new snapshot;
#   6. killing `owner`'s server mid-watch: the bridge ends, the watch
#      reconnects, a new server starts and adopts the worker;
#   7. the TUI attached over ssh: the transcript, a prompt and its reply,
#      then /quit;
#   8. a remote noel that is not there: the error is ssh's and the shell's;
#   and nothing is left running: no ssh of `client`, no bridge of `owner`.
#
# The container and its image are always removed.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
make_bin="${MAKE:-make}"
docker_bin="${DOCKER:-docker}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
base_image="alpine:3.21@sha256:c3f8e73fdb79deaebaa2037150150191b9dcbfba68b4a46d70103204c53f4709"
image="noel-bridge-journey:$$"
container="noel-bridge-journey-$$"
started="${SECONDS}"
checks=()

cleanup() {
  local status=$?
  "${docker_bin}" rm -f "${container}" >/dev/null 2>&1 || true
  "${docker_bin}" rmi -f "${image}" >/dev/null 2>&1 || true
  if [ "${status}" -ne 0 ]; then
    printf 'bridge-journey: FAILED after %ss (%s checks passed)\n' \
      "$((SECONDS - started))" "${#checks[@]}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT

fail() {
  printf 'bridge-journey: FAIL: %s\n' "$*" >&2
  exit 1
}
ok() {
  checks+=("$1")
  printf 'bridge-journey: ok (%ss): %s\n' "$((SECONDS - started))" "$1"
}
# `text` contains `pattern`, else fail with `what` and the text.
contains() {
  case "$1" in
    *"$2"*) ;;
    *) printf '%s\n' "$1" >&2; fail "$3: expected to find: $2" ;;
  esac
}
in_container() { "${docker_bin}" exec "${container}" "$@"; }
# The file in the container has the literal `pattern`.
has() {
  if ! in_container grep -Fq -- "$2" "$1"; then
    in_container tail -c 4000 "$1" >&2 || true
    fail "$3: $1 lacks: $2"
  fi
}
lacks() {
  if in_container grep -Fq -- "$2" "$1"; then fail "$3: $1 has: $2"; fi
}
# `noel` as `client`, with its own scratch state; stdout and stderr apart.
client_env=(-e HOME=/home/client -e NOEL_DIR=/home/client/state
  -e NOEL_RUNTIME_DIR=/tmp/noel-client-run -e NOEL_OFFLINE=1)
noel() {
  "${docker_bin}" exec -i -u client -w /home/client "${client_env[@]}" \
    "${container}" noel "$@" </dev/null
}
# Processes in the container whose `user args` match the ERE `$1`.
processes() { in_container ps -eo pid,user,args | awk -v pattern="$1" 'NR > 1 && $0 ~ pattern && !/awk/'; }
# Waits up to 30 s for the shell condition `$1`, run in the container.
wait_in_container() {
  in_container sh -c "for i in \$(seq 1 150); do if $1; then exit 0; fi; usleep 200000; done; exit 1" \
    || fail "timed out waiting for: $2"
}
# The pid in a JSON record in the container.
record_pid() { in_container sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$1"; }
# The one ssh `client` runs to the bridge.
client_ssh() {
  local pids
  pids="$(processes '^ *[0-9]+ +client +ssh .*bridge' | awk '{print $1}')"
  [ "$(printf '%s\n' "${pids}" | grep -c .)" = 1 ] || fail "expected one client ssh, found: ${pids:-none}"
  printf '%s\n' "${pids}"
}

printf 'bridge-journey: building a static noel\n'
"${make_bin}" noel-static
noel_bin="${target_dir}/x86_64-unknown-linux-musl/release/noel"
[ -x "${noel_bin}" ] || fail "no static noel at ${noel_bin}"

printf 'bridge-journey: image from %s\n' "${base_image}"
"${docker_bin}" build -q -t "${image}" - >/dev/null <<EOF
FROM ${base_image}
RUN apk add --no-cache openssh-server openssh-client openssh-keygen bash procps tmux
EOF
"${docker_bin}" run -d --init --name "${container}" --hostname bridge-box --network none \
  "${image}" sleep infinity >/dev/null
[ "$("${docker_bin}" inspect -f '{{.HostConfig.NetworkMode}}' "${container}")" = none ] \
  || fail "the container has a network"
"${docker_bin}" cp "${noel_bin}" "${container}:/usr/local/bin/noel"
"${docker_bin}" exec -i "${container}" sh -s <<'EOF'
set -eu
chown root:root /usr/local/bin/noel
chmod 755 /usr/local/bin/noel
ssh-keygen -A >/dev/null
for user in owner client; do
  adduser -D -s /bin/bash "$user"
  sed -i "s/^$user:!/$user:*/" /etc/shadow
done
settings='{"modelRefresh":{"enabled":false},"retry":{"enabled":false}}'
su owner -c "mkdir -p /home/owner/.noel /home/owner/work /home/owner/scripts \
  && echo '$settings' > /home/owner/.noel/settings.json"
# The remote sessions' faux model, through sshd: ssh passes no environment.
/usr/sbin/sshd -o ListenAddress=127.0.0.1 -o Port=2222 -o PasswordAuthentication=no \
  -o PermitRootLogin=no -E /var/log/sshd.log \
  -o "SetEnv=NOEL_OFFLINE=1 NOEL_FAUX_SCRIPT=/home/owner/scripts/remote.json NOEL_FAUX_SCRIPT_LOG=/home/owner/scripts/remote.log"
su client -c "mkdir -p /home/client/.ssh /home/client/state \
  && chmod 700 /home/client/.ssh \
  && ssh-keygen -q -t ed25519 -N '' -C journey -f /home/client/.ssh/id_ed25519 \
  && printf 'Host box\n  HostName 127.0.0.1\n  Port 2222\n  User owner\n' > /home/client/.ssh/config \
  && echo '$settings' > /home/client/state/settings.json"
install -d -m 700 -o owner -g owner /home/owner/.ssh
install -m 600 -o owner -g owner /home/client/.ssh/id_ed25519.pub /home/owner/.ssh/authorized_keys
EOF
"${docker_bin}" exec -i -u owner "${container}" sh -c 'cat > /home/owner/scripts/remote.json' <<'EOF'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "hostname; id -un; pwd"}}}],
  [{"text": "remote done"}],
  [{"text": "second reply"}]
]
EOF
owner_run="/tmp/noel-$(in_container id -u owner)"
versions="$(in_container apk info -v 2>/dev/null | grep -E '^(openssh-server|openssh-client|tmux)-' | tr '\n' ' ')"
printf 'bridge-journey: container up (network none): %s\n' "${versions}"

# 1. An unknown host key: ssh refuses with no terminal to ask on.
if output="$(noel client --remote box ls 2>&1)"; then
  fail "ls with an unknown host key: ${output}"
fi
contains "${output}" "noel client: ssh box:" "unknown host key"
contains "${output}" "Host key verification failed." "unknown host key"
if in_container test -e "${owner_run}/server.json"; then fail "a server started"; fi
ok "1. unknown host key refused by ssh, no server started"

# 2. Trusted: ls starts owner's server through the bridge.
in_container su client -c 'ssh-keyscan -p 2222 -t ed25519 127.0.0.1 2>/dev/null > /home/client/.ssh/known_hosts'
output="$(noel client --remote box ls 2>&1)" || fail "ls: ${output}"
# The bridge reports starting the server on ssh's stderr.
contains "${output}" "noel client: box: noel bridge: started noel server (pid" "the bridge's report"
[ "$(printf '%s\n' "${output}" | grep -v '^noel client: ')" = "no sessions" ] || fail "ls answered: ${output}"
server_pid="$(record_pid "${owner_run}/server.json")"
[ -n "$(processes "^ *${server_pid} +owner +noel server --foreground")" ] \
  || fail "no server of owner as pid ${server_pid}: $(processes 'noel')"
if in_container test -e /tmp/noel-client-run/server.json; then fail "client runs a server"; fi
[ -z "$(processes '^ *[0-9]+ +client +noel server')" ] || fail "client runs a server"
ok "2. ls through the bridge started owner's server (pid ${server_pid}); client runs none"

# 3. new and send: the run is owner's, on the remote.
if output="$(noel client --remote box new --model faux/script 2>&1)"; then
  fail "new without --cwd: ${output}"
fi
contains "${output}" "with --remote, pass --cwd: an absolute path on box" "new without --cwd"
session="$(noel client --remote box new --cwd /home/owner/work --model faux/script 2>/dev/null)" \
  || fail "new"
contains "$(noel client --remote box send "${session}" "run there" 2>/dev/null)" "started run" "send"
journal="/home/owner/.noel/sessions/${session}.jsonl"
wait_in_container "grep -q run_finished ${journal}" "the run"
has "${journal}" '"name":"bash","result":"bridge-box\nowner\n/home/owner/work\n","is_error":false' "remote bash"
has "${journal}" '"remote done"' "the reply"
[ "$(in_container find /home/client/state -name '*.jsonl' | wc -l)" = 0 ] \
  || fail "client has sessions: $(in_container find /home/client/state -name '*.jsonl')"
ok "3. new and send: bash ran on the remote as owner in /home/owner/work; the journal is owner's"

# 4. watch shows the reply; the bridged links are the tunnel's.
# A watch runs until stopped: this one for 5 s, inside the container.
view="$("${docker_bin}" exec -u client -w /home/client "${client_env[@]}" "${container}" \
  timeout 5 noel client --remote box watch "${session}" --json </dev/null 2>/dev/null | head -1)" || true
contains "${view}" '"snapshot"' "watch"
contains "${view}" '{"content":[{"text":"remote done","type":"text"}],"role":"assistant"' "watch shows the reply"
has "${owner_run}/server.log" 'opened: client {"kind":"owner","via":"tunnel"}' "the tunnel's principal"
lacks "${owner_run}/server.log" '"via":"device"' "no device link yet"
output="$("${docker_bin}" exec -u owner "${container}" noel client ls </dev/null 2>&1)" \
  || fail "owner's own ls: ${output}"
contains "${output}" "${session}" "owner's own ls"
has "${owner_run}/server.log" 'opened: client {"kind":"owner","via":"device"}' "the device's principal"
ok "4. watch showed the reply; bridged links via tunnel, owner's own client via device"

# 5 and 6. A watch through the bridge outlives its ssh and the server.
watch_log=/home/client/watch.log
"${docker_bin}" exec -d -u client -w /home/client "${client_env[@]}" "${container}" \
  sh -c "noel client --remote box watch ${session} > ${watch_log} 2>&1"
wait_in_container "grep -q 'end of snapshot' ${watch_log}" "the watch's snapshot"
first_ssh="$(client_ssh)"
in_container kill -9 "${first_ssh}"
wait_in_container "grep -q 'reconnected; holding again' ${watch_log}" "the watch to reconnect"
wait_in_container "[ \$(grep -c 'end of snapshot' ${watch_log}) -ge 2 ]" "a new snapshot"
second_ssh="$(client_ssh)"
[ "${second_ssh}" != "${first_ssh}" ] || fail "the same ssh after the kill"
contains "$(processes "^ *${second_ssh} ")" "BatchMode=yes" "the reconnect's ssh"
has "${watch_log}" "server link lost; reconnecting" "the watch's note"
ok "5. killing the client's ssh (pid ${first_ssh}): the watch reconnected through a batch-mode ssh"

worker_pid="$(record_pid "${owner_run}/workers/${session}.json")"
in_container kill -9 "${server_pid}"
wait_in_container "[ \$(grep -c 'reconnected; holding again' ${watch_log}) -ge 2 ]" "the watch to reconnect again"
wait_in_container "[ \$(grep -c 'end of snapshot' ${watch_log}) -ge 3 ]" "a third snapshot"
new_server="$(record_pid "${owner_run}/server.json")"
[ -n "${new_server}" ] && [ "${new_server}" != "${server_pid}" ] || fail "no new server (was ${server_pid})"
has "${owner_run}/server.log" "noel server: adopted worker ${session} (pid Some(${worker_pid}))" \
  "the new server adopted the worker"
[ "$(record_pid "${owner_run}/workers/${session}.json")" = "${worker_pid}" ] \
  || fail "the worker changed (was ${worker_pid})"
has "${watch_log}" "started noel server (pid ${new_server})" "the bridge started the server"
ok "6. killing owner's server: the watch reconnected, the bridge started server ${new_server}, worker ${worker_pid} adopted"
pkill_watch() { in_container sh -c "pkill -u client -f 'noel client --remote box watch' || true"; }
pkill_watch

# 7. The TUI attached over ssh.
tui() { "${docker_bin}" exec -u client "${container}" tmux "$@"; }
see() {
  # shellcheck disable=SC2016 # expanded in the container
  "${docker_bin}" exec -u client "${container}" sh -c \
    'for i in $(seq 1 150); do tmux capture-pane -p -t j | grep -qF -- "$1" && exit 0; usleep 200000; done; exit 1' \
    sh "$1" || { tui capture-pane -p -t j >&2 || true; fail "the TUI did not show: $1"; }
}
tui new-session -d -s j -x 160 -y 30 \
  "env HOME=/home/client NOEL_DIR=/home/client/state NOEL_RUNTIME_DIR=/tmp/noel-client-run NOEL_OFFLINE=1 noel --attach ${session} --remote box --no-alt-screen; echo tui-exited-\$?; sleep 600"
see "attached to session ${session} on noel server on box"
see "remote done"
tui send-keys -t j -l 'again'
tui send-keys -t j Enter
see "second reply"
has "${journal}" '"second reply"' "the TUI's run"
tui send-keys -t j -l '/quit'
tui send-keys -t j Enter
see "tui-exited-0"
tui kill-server
ok "7. the TUI attached over ssh: transcript, a prompt and its reply, /quit"

# 8. A remote noel that is not there.
if output="$(noel client --remote box --remote-noel /nowhere/noel ls 2>&1)"; then
  fail "ls with a missing remote noel: ${output}"
fi
contains "${output}" "noel client: ssh box:" "missing remote noel"
contains "${output}" "/nowhere/noel" "missing remote noel names it"
ok "8. a missing remote noel: ssh's error, naming it"

# Nothing left: no ssh of client, no bridge of owner.
wait_in_container "! ps -eo args | grep -q '[b]ridge'" "nothing left, found: $(processes 'bridge')"
ok "nothing left running: no client ssh, no bridge"

printf 'bridge-journey: passed %s checks in %ss\n' "${#checks[@]}" "$((SECONDS - started))"

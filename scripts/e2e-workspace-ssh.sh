#!/usr/bin/env bash
# Remote workspaces over ssh, end to end (docs/spec.md §10), in one container
# with no network: `make workspace-journey`.
#
# It builds the static noel-env daemons (make env-daemons) and a static musl
# noel (make noel-static), then runs Alpine 3.21, pinned by digest, with
# `--network none` and `--init`. Inside it, sshd listens on 127.0.0.1:2222
# with host keys made there; the "remote" user `worker` owns /work, and the
# "local" user `client` runs noel with a throwaway key made there that
# `worker` authorizes. Nothing on this machine's ssh configuration, keys,
# known hosts, sshd or firewall is read or changed.
#
# Each step asserts what it observes and the script stops at the first
# failure:
#   1. noel workspace trust: no terminal and no fingerprint, a wrong one, the
#      right one (stored in noel's own known_hosts, never ~/.ssh);
#   2. print mode with faux/script: bash, write, read and edit run in /work as
#      `worker`, the local directory is unchanged, and the system prompt has
#      /work as its cwd and the remote AGENTS.md;
#   3. grep and find in the workspace;
#   4. kill the ssh process mid-bash: that call fails, the next one runs on a
#      new daemon, and nothing is left running;
#   5. an untrusted host is refused with the trust command;
#   6. reopening with -c (no --workspace) uses the workspace again;
#   7. Mode 2: noel client new --workspace, then send;
#   8. a local session runs here, with no workspace record;
#   and the TUI's footer shows the workspace connected, lost and connected
#   again, with `!` running in /work.
#
# The container and its image are always removed. The runtime directory is
# under /tmp in the container: Alpine's home directories are setgid, which
# noel's runtime-directory check refuses.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"
make_bin="${MAKE:-make}"
docker_bin="${DOCKER:-docker}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
daemons_dir="${ENV_DAEMONS_DIR:-target/env-daemons}"
base_image="alpine:3.21@sha256:c3f8e73fdb79deaebaa2037150150191b9dcbfba68b4a46d70103204c53f4709"
image="noel-workspace-journey:$$"
container="noel-workspace-journey-$$"
url="ssh://worker@127.0.0.1:2222/work"
started="${SECONDS}"
checks=()

cleanup() {
  local status=$?
  "${docker_bin}" rm -f "${container}" >/dev/null 2>&1 || true
  "${docker_bin}" rmi -f "${image}" >/dev/null 2>&1 || true
  if [ "${status}" -ne 0 ]; then
    printf 'workspace-journey: FAILED after %ss (%s checks passed)\n' \
      "$((SECONDS - started))" "${#checks[@]}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT

fail() {
  printf 'workspace-journey: FAIL: %s\n' "$*" >&2
  exit 1
}
ok() {
  checks+=("$1")
  printf 'workspace-journey: ok: %s\n' "$1"
}
# `text` contains `pattern`, else fail with `what` and the text.
contains() {
  case "$1" in
    *"$2"*) ;;
    *) printf '%s\n' "$1" >&2; fail "$3: expected to find: $2" ;;
  esac
}
in_container() { "${docker_bin}" exec "${container}" "$@"; }
# The file in the container has the literal `pattern` (JSON escapes are kept
# as written in the file, e.g. \n).
has() {
  if ! in_container grep -Fq -- "$2" "$1"; then
    in_container tail -c 4000 "$1" >&2 || true
    fail "$3: $1 lacks: $2"
  fi
}
lacks() {
  if in_container grep -Fq -- "$2" "$1"; then fail "$3: $1 has: $2"; fi
}
# Runs as `client` with noel's scratch state and the faux script `$1`.
as_client() {
  local script="$1"
  shift
  "${docker_bin}" exec -i -u client -w /home/client/project \
    -e HOME=/home/client -e NOEL_DIR=/home/client/state -e NOEL_RUNTIME_DIR=/tmp/noel-run \
    -e NOEL_OFFLINE=1 -e NOEL_FAUX_SCRIPT="/home/client/scripts/${script}.json" \
    -e NOEL_FAUX_SCRIPT_LOG="/home/client/scripts/${script}.log" \
    "${container}" "$@"
}
noel() {
  local script="$1"
  shift
  as_client "${script}" /opt/noel/noel "$@" </dev/null
}
faux_script() { "${docker_bin}" exec -i -u client "${container}" sh -c "cat > /home/client/scripts/$1.json"; }
# Processes in the container whose command line matches the ERE `$1`.
processes() { in_container ps -eo pid,user,args | awk -v pattern="$1" 'NR > 1 && $0 ~ pattern && !/awk/'; }
# Waits up to 30 s for the shell condition `$1`, run in the container.
wait_in_container() {
  in_container sh -c "for i in \$(seq 1 150); do if $1; then exit 0; fi; usleep 200000; done; exit 1" \
    || fail "timed out waiting for: $2"
}
# Daemons end once their ssh does, shortly after noel; `[n]` keeps grep from
# matching its own command line.
no_daemons_left() {
  wait_in_container '! ps -eo args | grep -qE "[n]oel-env-|^[s]leep 30"' \
    "no daemon left, found: $(processes 'noel-env-|sleep 30')"
}
# The one `ssh` that runs a daemon for `client`.
client_ssh() {
  local pids
  pids="$(processes '^ *[0-9]+ +client +ssh .*noel-env-' | awk '{print $1}')"
  [ "$(printf '%s\n' "${pids}" | grep -c .)" = 1 ] || fail "expected one client ssh, found: ${pids:-none}"
  printf '%s\n' "${pids}"
}

printf 'workspace-journey: building the daemons and a static noel\n'
"${make_bin}" env-daemons
"${make_bin}" noel-static
noel_bin="${target_dir}/x86_64-unknown-linux-musl/release/noel"
[ -x "${noel_bin}" ] || fail "no static noel at ${noel_bin}"
[ -x "${daemons_dir}/noel-env-linux-x64" ] || fail "no daemon at ${daemons_dir}/noel-env-linux-x64"
daemon_hash="$(sha256sum "${daemons_dir}/noel-env-linux-x64" | cut -c1-32)"

printf 'workspace-journey: image from %s\n' "${base_image}"
"${docker_bin}" build -q -t "${image}" - >/dev/null <<EOF
FROM ${base_image}
RUN apk add --no-cache openssh-server openssh-client openssh-keygen bash procps tmux
EOF
"${docker_bin}" run -d --init --name "${container}" --hostname ws-box --network none \
  "${image}" sleep infinity >/dev/null
[ "$("${docker_bin}" inspect -f '{{.HostConfig.NetworkMode}}' "${container}")" = none ] \
  || fail "the container has a network"
in_container mkdir -p /opt/noel
"${docker_bin}" cp "${noel_bin}" "${container}:/opt/noel/noel"
"${docker_bin}" cp "${daemons_dir}/noel-env-linux-x64" "${container}:/opt/noel/noel-env-linux-x64"
"${docker_bin}" exec -i "${container}" sh -s <<'EOF'
set -eu
chown -R root:root /opt/noel
chmod 755 /opt/noel/noel /opt/noel/noel-env-linux-x64
ssh-keygen -A >/dev/null
for user in worker client; do
  adduser -D -s /bin/bash "$user"
  sed -i "s/^$user:!/$user:*/" /etc/shadow
done
install -d -m 755 -o worker -g worker /work
printf 'Remote project rules: say hello from /work.\n' > /work/AGENTS.md
chown worker:worker /work/AGENTS.md
/usr/sbin/sshd -o ListenAddress=127.0.0.1 -o Port=2222 -o PasswordAuthentication=no \
  -o PermitRootLogin=no -E /var/log/sshd.log
su client -c "mkdir -p /home/client/.ssh /home/client/project /home/client/state /home/client/scripts \
  && chmod 700 /home/client/.ssh \
  && ssh-keygen -q -t ed25519 -N '' -C journey -f /home/client/.ssh/id_ed25519 \
  && echo 'local only' > /home/client/project/local.txt \
  && echo '{\"modelRefresh\":{\"enabled\":false},\"retry\":{\"enabled\":false}}' > /home/client/state/settings.json"
install -d -m 700 -o worker -g worker /home/worker/.ssh
install -m 600 -o worker -g worker /home/client/.ssh/id_ed25519.pub /home/worker/.ssh/authorized_keys
EOF
versions="$(in_container apk info -v 2>/dev/null | grep -E '^(openssh-server|bash|tmux)-' | tr '\n' ' ')"
printf 'workspace-journey: container up (network none): %s\n' "${versions}"

# 5. An untrusted host: refused, naming the trust command; no session made.
if output="$(noel none -p --model faux/echo --workspace "${url}" hi 2>&1)"; then
  fail "a session started on an untrusted host: ${output}"
fi
contains "${output}" "trust it with: noel workspace trust ssh://127.0.0.1:2222" "untrusted host"
[ "$(in_container sh -c 'ls /home/client/state/sessions 2>/dev/null | wc -l')" = 0 ] \
  || fail "a session was made for an untrusted host"
ok "5. untrusted host refused with the trust command"

# 1. Trust: the fingerprint is checked against the host's own.
fingerprint="$(in_container ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub | awk '{print $2}')"
if output="$(noel none workspace trust ssh://worker@127.0.0.1:2222 2>&1)"; then
  fail "trusted without a terminal or fingerprint"
fi
contains "${output}" "${fingerprint}" "trust shows the fingerprint"
contains "${output}" "not a terminal: pass --fingerprint" "trust without a terminal"
if output="$(noel none workspace trust ssh://127.0.0.1:2222 \
  --fingerprint SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA 2>&1)"; then
  fail "a wrong fingerprint was trusted"
fi
contains "${output}" "nothing was trusted" "wrong fingerprint"
[ "$(in_container sh -c 'cat /home/client/state/workspaces/known_hosts 2>/dev/null | wc -l')" = 0 ] \
  || fail "a wrong fingerprint stored a key"
output="$(noel none workspace trust ssh://worker@127.0.0.1:2222 --fingerprint "${fingerprint}" 2>&1)" \
  || fail "the right fingerprint was refused: ${output}"
contains "${output}" "noel trusts the host key of 127.0.0.1:2222" "right fingerprint"
has /home/client/state/workspaces/known_hosts "noel-env-[127.0.0.1]:2222 ssh-ed25519 " "stored key"
if in_container test -e /home/client/.ssh/known_hosts; then fail "the client's .ssh/known_hosts was written"; fi
ok "1. trust: no terminal refused, wrong fingerprint refused, right one stored in noel's known_hosts only"

# 2. Print mode: bash, write, read and edit in /work.
faux_script s2 <<'EOF'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "hostname; id -un; pwd"}}}],
  [{"tool_call": {"name": "write", "arguments": {"path": "notes.txt", "content": "alpha\nbeta\n"}}}],
  [{"tool_call": {"name": "read", "arguments": {"path": "notes.txt"}}}],
  [{"tool_call": {"name": "edit", "arguments": {"path": "notes.txt", "edits": [{"oldText": "beta", "newText": "gamma"}]}}}],
  [{"tool_call": {"name": "read", "arguments": {"path": "notes.txt"}}}],
  [{"text": "step 2 done"}]
]
EOF
output="$(noel s2 -p --model faux/script --workspace "${url}" "work there" 2>&1)" \
  || fail "print mode: ${output}"
[ "${output}" = "step 2 done" ] || fail "print mode answered: ${output}"
session="$(in_container sh -c 'ls /home/client/state/sessions' | sed -n 's/\.jsonl$//p')"
journal="/home/client/state/sessions/${session}.jsonl"
[ "$(in_container sed -n 2p "${journal}")" = \
  '{"type":"workspace","adapter":"ssh","id":"ssh://127.0.0.1:2222","cwd":"/work","host":"127.0.0.1","port":2222,"user":"worker"}' ] \
  || fail "the journal's second record is not the workspace: $(in_container sed -n 2p "${journal}")"
has "${journal}" '"name":"bash","result":"ws-box\nworker\n/work\n","is_error":false' "remote bash"
has "${journal}" '"name":"write","result":"Successfully wrote to notes.txt","is_error":false' "remote write"
has "${journal}" '"name":"read","result":"alpha\nbeta\n","is_error":false' "remote read"
has "${journal}" '"name":"edit","result":"Successfully replaced 1 block(s) in notes.txt.","is_error":false' "remote edit"
has "${journal}" '"name":"read","result":"alpha\ngamma\n","is_error":false' "remote read after edit"
[ "$(in_container cat /work/notes.txt)" = "$(printf 'alpha\ngamma')" ] || fail "/work/notes.txt"
[ "$(in_container stat -c %U /work/notes.txt)" = worker ] || fail "/work/notes.txt is not worker's"
[ "$(in_container ls -A /home/client/project)" = local.txt ] || fail "the local directory changed"
has /home/client/scripts/s2.log '<cwd>\n/work\n</cwd>' "the prompt's cwd"
has /home/client/scripts/s2.log 'Remote project rules: say hello from /work.' "the remote AGENTS.md"
in_container test -x "/home/worker/.cache/noel-env/noel-env-${daemon_hash}" \
  || fail "no daemon deployed as noel-env-${daemon_hash}"
no_daemons_left
ok "2. print mode: bash/write/read/edit ran in /work as worker, local dir unchanged, prompt from the workspace"

# 3 and 6. Reopened with -c and no --workspace: grep and find run there.
in_container su worker -c 'mkdir -p /work/src/deep /work/docs /work/.hidden \
  && printf "pub fn needle() {}\n" > /work/src/lib.rs \
  && printf "const NEEDLE: u8 = 1;\n" > /work/src/deep/mod.rs \
  && printf "# Guide\nthe needle is here\n" > /work/docs/guide.md \
  && printf "hidden needle\n" > /work/.hidden/h.txt'
faux_script s3 <<'EOF'
[
  [{"tool_call": {"name": "grep", "arguments": {"pattern": "needle", "ignoreCase": true}}}],
  [{"tool_call": {"name": "find", "arguments": {"pattern": "*.rs"}}}],
  [{"tool_call": {"name": "find", "arguments": {"pattern": "*"}}}],
  [{"text": "step 3 done"}]
]
EOF
output="$(noel s3 -p -c --model faux/script "search there" 2>&1)" || fail "reopen: ${output}"
[ "${output}" = "step 3 done" ] || fail "reopen answered: ${output}"
[ "$(in_container sh -c 'ls /home/client/state/sessions | wc -l')" = 1 ] || fail "-c made another session"
has "${journal}" '"name":"grep","result":".hidden/h.txt:1: hidden needle\ndocs/guide.md:2: the needle is here\nsrc/deep/mod.rs:1: const NEEDLE: u8 = 1;\nsrc/lib.rs:1: pub fn needle() {}","is_error":false' "remote grep"
has "${journal}" '"name":"find","result":"src/deep/mod.rs\nsrc/lib.rs","is_error":false' "remote find"
has "${journal}" '"name":"find","result":".hidden/\n.hidden/h.txt\nAGENTS.md\ndocs/\ndocs/guide.md\nnotes.txt\nsrc/\nsrc/deep/\nsrc/deep/mod.rs\nsrc/lib.rs","is_error":false' "remote find *"
no_daemons_left
ok "3. grep and find ran in the workspace"
ok "6. -c reopened the session in its workspace without --workspace"

# 4. Kill ssh mid-bash: the call fails, the next call starts a new daemon.
faux_script s4 <<'EOF'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "echo started; sleep 30; echo finished"}}}],
  [{"tool_call": {"name": "bash", "arguments": {"command": "echo this daemon: $PPID"}}}],
  [{"text": "step 4 done"}]
]
EOF
noel s4 -p -c --model faux/script "run a long command" > /dev/null 2>&1 &
noel_pid=$!
wait_in_container 'ps -eo args | grep -q "^sleep 30"' "the remote sleep"
first_daemon="$(processes '^ *[0-9]+ +worker +/home/worker/.cache/noel-env/' | awk '{print $1}')"
ssh_pid="$(client_ssh)"
[ -n "${first_daemon}" ] && [ "$(printf '%s\n' "${first_daemon}" | grep -c .)" = 1 ] \
  || fail "expected one daemon, found: ${first_daemon:-none}"
in_container kill -9 "${ssh_pid}"
wait "${noel_pid}" || fail "noel exited $? after the kill"
has "${journal}" '"name":"bash","result":"pi-env connection lost","is_error":true' "the call in flight fails"
second_daemon="$(in_container grep -o 'this daemon: [0-9]*' "${journal}" | tail -1 | cut -d' ' -f3)"
[ -n "${second_daemon}" ] && [ "${second_daemon}" != "${first_daemon}" ] \
  || fail "the next call ran on daemon ${second_daemon:-none}, not a new one (was ${first_daemon})"
no_daemons_left
ok "4. killing ssh (pid ${ssh_pid}) failed the call; the next ran on new daemon ${second_daemon} (was ${first_daemon})"

# 7. Mode 2: create in the workspace, then send.
faux_script s7 <<'EOF'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "id -un; pwd; echo from mode 2 > m2.txt"}}}],
  [{"text": "step 7 done"}]
]
EOF
if output="$(noel s7 client new --model faux/script --workspace ssh://worker@localhost:2222/work 2>&1)"; then
  fail "client new on an untrusted host: ${output}"
fi
contains "${output}" "invalid/workspace" "Mode 2 untrusted"
contains "${output}" "noel workspace trust ssh://localhost:2222" "Mode 2 untrusted hint"
m2="$(noel s7 client new --model faux/script --workspace "${url}" 2>/dev/null)" || fail "client new --workspace"
contains "$(noel s7 client ls 2>/dev/null)" "${m2}" "client ls"
contains "$(noel s7 client ls 2>/dev/null)" "${url}" "client ls shows the workspace"
contains "$(noel s7 client send "${m2}" "write m2.txt there" 2>/dev/null)" "started run" "client send"
wait_in_container "grep -q run_finished /home/client/state/sessions/${m2}.jsonl" "the Mode 2 run"
has "/home/client/state/sessions/${m2}.jsonl" '"name":"bash","result":"worker\n/work\n","is_error":false' "Mode 2 bash"
[ "$(in_container cat /work/m2.txt)" = "from mode 2" ] || fail "/work/m2.txt"
# The server and its worker stop, and with them the worker's daemon.
# shellcheck disable=SC2016 # expanded in the container
in_container sh -c 'for record in /tmp/noel-run/server.json /tmp/noel-run/workers/*.json; do
  [ -f "$record" ] && kill "$(sed -n "s/.*\"pid\":\([0-9]*\).*/\1/p" "$record")" 2>/dev/null; done; true'
no_daemons_left
ok "7. Mode 2: untrusted create refused; client new --workspace and send ran in /work"

# The TUI: the footer's connection state, and `!` in the workspace.
tui() { "${docker_bin}" exec -u client "${container}" tmux "$@"; }
see() {
  # shellcheck disable=SC2016 # expanded in the container
  "${docker_bin}" exec -u client "${container}" sh -c \
    'for i in $(seq 1 150); do tmux capture-pane -p -t j | grep -qF -- "$1" && exit 0; usleep 200000; done; exit 1' \
    sh "$1" || { tui capture-pane -p -t j >&2 || true; fail "the TUI did not show: $1"; }
}
tui new-session -d -s j -x 160 -y 30 \
  "env HOME=/home/client NOEL_DIR=/home/client/state NOEL_RUNTIME_DIR=/tmp/noel-run NOEL_OFFLINE=1 /opt/noel/noel --session ${session} --model faux/echo --no-alt-screen; sleep 600"
see "${url} (not connected)"
# shellcheck disable=SC2016 # run by the workspace's shell
tui send-keys -t j -l '!echo "ran-as:$(id -un):$(pwd)"'
tui send-keys -t j Enter
see "ran-as:worker:/work"
see "${url} (connected)"
in_container kill -9 "$(client_ssh)"
tui send-keys -t j x BSpace
see "${url} (lost)"
# shellcheck disable=SC2016 # run by the workspace's shell
tui send-keys -t j -l '!echo "again-as:$(id -un):$(pwd)"'
tui send-keys -t j Enter
see "again-as:worker:/work"
see "${url} (connected)"
tui kill-server
ok "TUI: footer not connected, connected, lost, connected; ! ran in /work"

# 8. A local session: tools here, no workspace record.
faux_script s8 <<'EOF'
[
  [{"tool_call": {"name": "bash", "arguments": {"command": "id -un; pwd; ls"}}}],
  [{"text": "step 8 done"}]
]
EOF
output="$("${docker_bin}" exec -i -u client -w /home/client/project -e HOME=/home/client \
  -e NOEL_DIR=/home/client/state-local -e NOEL_RUNTIME_DIR=/tmp/noel-run-local -e NOEL_OFFLINE=1 \
  -e NOEL_FAUX_SCRIPT=/home/client/scripts/s8.json -e NOEL_FAUX_SCRIPT_LOG=/home/client/scripts/s8.log \
  "${container}" /opt/noel/noel -p --model faux/script "run here" </dev/null 2>&1)" || fail "local session: ${output}"
[ "${output}" = "step 8 done" ] || fail "local session answered: ${output}"
local_journal="$(in_container sh -c 'ls /home/client/state-local/sessions/*.jsonl')"
has "${local_journal}" '"name":"bash","result":"client\n/home/client/project\nlocal.txt\n","is_error":false' "local bash"
lacks "${local_journal}" '"type":"workspace"' "local journal"
ok "8. a local session ran here, with no workspace record"

printf 'workspace-journey: passed %s checks in %ss\n' "${#checks[@]}" "$((SECONDS - started))"

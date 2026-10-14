#!/usr/bin/env bash
# A stand-in for `ssh`, for the pi-env SSH goldens (pi-env-ssh-fixture.ts drives upstream's ssh.ts with it) and
# noel-env's SSH tests (crates/noel-env-daemon/tests/ssh_deploy.rs drives noel's), so both are recorded alike.
#
# It reads fake-ssh.env next to it: LOG (where every invocation is appended: its argument count, then its
# arguments, each followed by a NUL byte), HOME_DIR (the "remote" home) and HOST_KEY (`type key`, the key the
# "host" presents). Like ssh, it takes options up to `--`, then the host, then the remote command, and checks the
# host key against UserKnownHostsFile under HostKeyAlias: a different key for the alias is refused as changed, a
# missing one is added with StrictHostKeyChecking=accept-new and refused otherwise. The remote command then runs
# here, as ssh runs it on the remote machine: joined with spaces, by /bin/sh, in HOME_DIR.
set -u
config="$(dirname "$0")/fake-ssh.env"
# shellcheck source=/dev/null
. "$config"
printf '%s\0' "$#" "$@" >> "$LOG"

known='' alias='' strict=yes
while [ $# -gt 0 ]; do
  case $1 in
    --) shift; break ;;
    -o)
      case $2 in
        UserKnownHostsFile=*)
          known=${2#UserKnownHostsFile=}
          known=${known#\"}
          known=${known%\"}
          known=${known//%%/%}
          ;;
        HostKeyAlias=*) alias=${2#HostKeyAlias=} ;;
        StrictHostKeyChecking=*) strict=${2#StrictHostKeyChecking=} ;;
      esac
      shift 2
      ;;
    -F | -l | -p) shift 2 ;;
    *) shift ;;
  esac
done
shift # the host

if [ -f "$known" ] && grep -q "^$alias " "$known"; then
  if ! grep -qxF "$alias $HOST_KEY" "$known"; then
    echo '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@' >&2
    echo '@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @' >&2
    echo '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@' >&2
    exit 255
  fi
elif [ "$strict" = accept-new ]; then
  echo "$alias $HOST_KEY" >> "$known"
else
  echo "No ED25519 host key is known for $alias and you have requested strict checking." >&2
  echo 'Host key verification failed.' >&2
  exit 255
fi

cd "$HOME_DIR" || exit 255
HOME="$HOME_DIR" SHELL=/bin/sh exec /bin/sh -c "$*"

# noel-env

noel's workspaces: where tools read, write and run commands, on this machine
(`LocalEnv`) or another (`RemoteEnv`, over a pi-env daemon, deployed by SSH).

## The interface

`ExecutionEnv` mirrors upstream Durable's `ExecutionEnv`
(`packages/durable/src/env/index.ts`) at the pinned commit
(`CHORD_REFERENCE_REF` in the Makefile). It is a `FileSystem` plus a `Shell`,
async on tokio and object-safe, so callers hold an `Arc<dyn ExecutionEnv>`.

- **Results, not panics.** Every operation returns `Result`. File system
  failures carry a `FileErrorCode` (`aborted`, `not_found`,
  `permission_denied`, `not_directory`, `is_directory`, `invalid`,
  `not_supported`, `unknown`); commands carry an `ExecutionErrorCode`
  (`aborted`, `timeout`, `shell_unavailable`, `spawn_error`,
  `callback_error`, `unknown`). Only codes are contract, not messages.
- **Context.** Every operation takes a `Context`. Its abort signal is checked
  where upstream checks it; a running command is killed. Dropping a call's
  future stops it as well.
- **File system:** `id` (equal ids see the same files), `cwd`, path
  resolution, text and binary reads, line readers, positional binary readers
  with line scans, writes, appends, truncation, flushing, renames, metadata,
  directory listings and paged directory readers, `watch`, an optional
  `list_files`, directory creation and removal, and temporary files.
- **Shell:** `exec` takes a `ShellCommand`: a `Script` runs through the
  environment's shell, an `Argv` runs its program directly without one. It
  takes `cwd`, `env`, `inherit_env`, `timeout`, an output callback that gets
  every chunk with its stream, `spill` and `window`. Abort, timeout, or
  dropping the call kills only that command's processes.

noel adds three things to `exec`, each documented on its field: `env` entries
can remove an inherited variable, `merge_stderr` sends both streams through
one pipe so output keeps the order it was written in, and a chunk carries the
raw bytes it was decoded from.

## LocalEnv

`LocalEnv` runs on this machine and behaves as upstream's `NodeExecutionEnv`
(`packages/env/docs/semantics.md`). Its `id` is `local`.

- Paths resolve as Node does: `~` and `~/…` under the home directory, a
  `file://` URL to its path, the rest against `cwd`, normalized and not
  confined to `cwd`.
- Errors are coded as `NodeExecutionEnv`'s `toFileError` codes Node's errno.
- Commands start with noel's process code (`spawn_tool_process`): a new
  session (`setsid`), stdin from `/dev/null`, and the process group
  registered, so a timeout, an abort, a dropped call or
  `kill_running_tool_groups` kills the whole tree. A command that runs to its
  end leaves the background jobs it started running. After the process
  exits, output is read until both pipes close or it pauses for 100 ms.
- `watch` and `list_files` fail with `not_supported` for now.

## The tools and the hook

`noel-tools` runs `read`, `write`, `edit`, `ls` and `bash` through an
`ExecutionEnv`; `edit` and `write` of one file run one at a time per file
system id and canonical path. `grep` and `find` search with ripgrep's crates:
in a local workspace (`is_local`) by walking the tree, in another over what
its `list_files` lists and its reads return.

`noel-core`'s `Runtime` asks its `EnvProvider` for the workspace of every
tool call, with `EnvTarget { session_id, cwd }`, as Durable's harness
`env(target)` does. The default is a `LocalEnv` at the session's working
directory; a session whose journal names a workspace (`SessionWorkspace`)
gets it from `EnvProvider::Workspaces`, which the command line sets to open
`ssh` workspaces as a `RemoteEnv` over `ssh::ssh_connection`
(`docs/spec.md` §10).

## RemoteEnv: the pi-env client

`RemoteEnv` is an `ExecutionEnv` on another machine, a Rust port of upstream's
`RemoteExecutionEnv` (`packages/env/src/remote-env.ts`) over a `Connection`
(`connection.ts`) to a pi-env daemon: `noel-env serve` (the
`noel-env-daemon` crate) or upstream's `pi-env`. It speaks protocol 1
unchanged (`packages/env/docs/protocol.md`), and its results match
`NodeExecutionEnv` on that machine, so `LocalEnv`; only error messages may
differ (`packages/env/docs/semantics.md`).

- **The connection** starts any daemon command (a local process,
  `ssh … noel-env serve`, later Codewire's exec) with `serve --token <hex>`,
  discards what a login shell prints before the `PI-ENV <token>` line (and
  logs it), and multiplexes requests by id with results, errors and events.
  It starts lazily on the first request; a command computed at each start
  (`DaemonCommand::Computed`) may detect and deploy first, and its failure
  fails the waiting requests with `spawn_error` until the next request tries
  again. Both sides ping every 5 seconds; 30 seconds of silence, the daemon's
  exit or a corrupt frame lose the connection: requests in flight fail with
  `unknown` and `lost: true`, requests on handles of the old daemon fail
  instead of reaching the next one, and the next request starts a new daemon.
  `close` stops it; so does dropping the last handle.
- **Cancellation:** aborting a call's `Context` sends `cancel` and the call
  still settles with the daemon's answer; `cleanup` kills commands without
  aborting them (exit 137). Dropping an `exec` (or `scanLines`, `listFiles`,
  `watch`) future sends `cancel` too, as dropping a `LocalEnv` call stops it.
- **Large transfers** are split as upstream splits them: writes in 512 KiB
  chunks, eight in flight; binary reader reads in 256 KiB and whole-file
  reads in 512 KiB `pread`s, eight in flight; text lines in 64 KiB reads. A
  request whose frame would pass 16 MiB (a huge argv) fails alone.
- **Paths** resolve as upstream's client resolves them: `~` under the remote
  home, `file://` URLs, the rest against the env's `cwd`, with POSIX rules.
  A Windows daemon is refused with `not_supported`.
- **`watch`** runs in the daemon (Durable's `NodeFileWatcher` next to the
  files). After a lost connection it is opened again once a new daemon starts,
  backing off from 1 to 30 seconds, and reports `Overflow`.
- **`list_files`** lists in the daemon; a daemon without the `listFiles`
  feature gives `not_supported`. `list_files_over` lists through any
  connection.

## SSH deployment

`ssh` is a port of upstream's `ssh.ts` for Linux and macOS remotes:

- `ssh` runs with `BatchMode`, strict host-key checking against the
  application's own known-hosts file under a fixed alias, no forwarding of any
  kind, no shared connections or configured commands, and no locale
  forwarding (`ssh_arguments`).
- `scan_host_key` captures the key the host presents (with `ssh-keygen -lf`
  fingerprints to compare out of band), `accept_host_key` adds it, and a
  changed key is refused until `forget_host_key` removes the old one.
- `detect_platform` runs `uname` through the remote shell. The daemon to
  upload is `noel-env-<platform>-<arch>` (`noel-env-linux-x64`,
  `noel-env-linux-arm64`, `noel-env-darwin-x64`, `noel-env-darwin-arm64`) in
  `SshConnectOptions::daemon_dir`, else `NOEL_ENV_DAEMON_DIR`, else the
  running executable's directory; a missing one is an error naming the path.
- `deploy_daemon` uploads it to
  `~/.cache/noel-env/noel-env-<first 32 hex digits of its SHA-256>` through a
  temporary file that is renamed into place only once its SHA-256 matches,
  and removes daemons of other contents. The daemon is verified before every
  start: `connect_ssh` deploys now, `ssh_connection` on the first request, and
  both check again after a lost connection.
- `login_shell` starts the daemon through `$SHELL -l`.

## Goldens

`scripts/ts-parity/local-env-fixture.ts` runs upstream's env conformance
cases (`packages/durable/src/testing/env-conformance.ts`) against
`NodeExecutionEnv` in the ts-parity Docker image and records every assertion
each case makes into `tests/fixtures/ts-parity/local-env.json`
(`make ts-parity-env`). `tests/conformance/mod.rs` ports the cases for any
`ExecutionEnv` and compares each assertion's values:
`tests/local_env_conformance.rs` runs them against `LocalEnv`, which refuses
the watch cases (recorded by outcome only) with `not_supported`, and
`crates/noel-env-daemon/tests/remote_env_conformance.rs` against `RemoteEnv`
over `noel-env serve`, which also passes the watch cases and upstream's
`listFiles` cases.

The client's other goldens are in `crates/noel-env-daemon/tests`, which has
the daemon to run:

- `pi_env_client_golden.rs`: `RemoteEnv` sends exactly the frames upstream's
  client sent in `pi-env.json`'s scenarios (all but `pty`).
- `ssh_deploy.rs`: through a stand-in for `ssh`
  (`scripts/ts-parity/fake-ssh.sh`), every `ssh` invocation equals upstream's
  in `pi-env-ssh.json` (`make ts-parity-pi-env-ssh`) except for the daemon's
  names, and upstream's SSH tests are ported.
- `remote_env.rs`: upstream's `remote.test.ts` and the connection's lifetime.

`make env-interop` runs upstream's client tests against `noel-env serve`, and
these client tests against upstream's `pi-env`.

Differences from upstream are in `docs/rust-rewrite/non-parity-register.md`
under "Workspaces", "noel-env Daemon" and "noel-env Client".

## pi-env

`frame` is pi-env protocol 1's frame codec (upstream
`packages/env/docs/protocol.md`), shared by the client and the daemon,
`noel-env serve` (the `noel-env-daemon` crate). The daemon also shares
`StreamDecoder` and `LineScanner`, and keeps its own dependencies (`notify`,
`ignore`) out of this library.

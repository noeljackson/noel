# noel-env

noel's workspaces: where tools read, write and run commands.

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
system id and canonical path. `grep` and `find` resolve paths in it and search
with ripgrep's crates, so they need a local workspace (`is_local`).

`noel-core`'s `Runtime` asks its `EnvProvider` for the workspace of every
tool call, with `EnvTarget { session_id, cwd }`, as Durable's harness
`env(target)` does. The default is a `LocalEnv` at the session's working
directory.

## Goldens

`scripts/ts-parity/local-env-fixture.ts` runs upstream's env conformance
cases (`packages/durable/src/testing/env-conformance.ts`) against
`NodeExecutionEnv` in the ts-parity Docker image and records every assertion
each case makes into `tests/fixtures/ts-parity/local-env.json`
(`make ts-parity-env`). `tests/local_env_conformance.rs` runs the same cases
against `LocalEnv` and compares each assertion's values. Watch cases are
recorded by outcome only and are not run here until `LocalEnv` watches.

Differences from upstream are in `docs/rust-rewrite/non-parity-register.md`
under "Workspaces".

## pi-env

`frame` is pi-env protocol 1's frame codec (upstream
`packages/env/docs/protocol.md`). The daemon that speaks it, `noel-env serve`,
is the `noel-env-daemon` crate; it shares this codec, `StreamDecoder` and
`LineScanner`, and keeps its own dependencies (`notify`, `ignore`) out of this
library.

Later in this crate: the rest of the Rust port of upstream's `pi-env`, the
protocol client, a remote `ExecutionEnv` and SSH deployment (`docs/spec.md`
§10).

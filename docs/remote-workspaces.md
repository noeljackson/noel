# Remote Workspaces


A session can run its tools in a directory on another machine, reached over
`ssh` (spec §10). noel uploads its `noel-env` daemon there and every built-in
tool works through it: `read`, `write`, `edit`, `ls`, `bash`, `grep`, `find`,
and `!` or `/bash` in the TUI. The model and noel itself stay on this machine.

```bash
noel workspace trust ssh://build-box          # shows the host key, asks y/N
noel --workspace ssh://me@build-box/srv/app   # a new session, tools on build-box
noel -c                                       # reopens it; the workspace is kept
noel client new --workspace ssh://me@build-box/srv/app   # the same through noel server
```

- **The URL** is `ssh://[user@]host[:port]/absolute/path`. Add `?login=1` to
  start the daemon through the remote login shell (`$SHELL -l`), so commands
  see what `~/.profile` sets; without it they get what `ssh host command`
  gets. The session's journal saves the workspace, `-c`, `-r`, `--session`
  and a server's worker use it again, and `--workspace` naming another one for
  an existing session is an error. Without `--workspace` a session is local,
  as before.
- **Trust is never automatic.** `noel workspace trust ssh://host[:port]`
  connects once, prints the host key's fingerprints (`ssh-keygen -lf`), and
  on a terminal asks before it stores the key; without a terminal pass
  `--fingerprint SHA256:…`, the host's own fingerprint, and only that key is
  trusted. Keys go to noel's own `~/.noel/workspaces/known_hosts`, never to
  `~/.ssh`. `noel workspace forget ssh://host[:port]` removes them. A session
  on a host noel does not trust does not start, and the error names the trust
  command. `ssh` itself still reads your `~/.ssh/config`, keys and agent to
  log in, with prompts off (`BatchMode`) and no forwarding.
- **The daemon** is `noel-env-linux-x64` or `noel-env-linux-arm64` beside
  `noel` (or in `NOEL_ENV_DAEMON_DIR`). `make env-daemons` builds them (static
  musl; arm64 needs `aarch64-linux-gnu-gcc`, or set `ENV_DAEMON_ARM64_LINKER`)
  and `make install` installs them with `noel`. It is uploaded to
  `~/.cache/noel-env/` on the remote machine, named by its SHA-256, and
  checked before every start. macOS remotes need a Mac-built daemon, which
  is not built yet. `make noel-static` builds a static musl `noel` (clang
  compiles ring's C for musl), and `make workspace-journey` runs the whole
  flow (trust, tools, a dropped connection, `-c`, Mode 2, the TUI's footer)
  against `sshd` in an Alpine container with no network.
- **Connecting** happens at the first tool call. An unreachable host makes
  that call fail, not the session; the TUI's footer shows the workspace and
  whether it is connected, connecting or lost. If the connection drops, the
  call in flight fails (a command's outcome is then unknown) and the next
  call starts a new daemon.
- **What comes from the workspace:** the tools' files and commands, the
  project's context files (`AGENTS.md`, `CLAUDE.md` and the like, from the
  workspace's directory and its parents) and the `cwd` the system prompt
  states. Everything that would run or configure noel here stays this
  machine's: settings, `.noel/`, extensions (whose tools run here), skills
  (not listed in the system prompt, since the model's `read` cannot reach
  them; `/skill:<name>` still works), prompt templates, `@file` references
  and `/diff`.

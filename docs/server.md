# Server Sessions


`noel server` holds sessions for clients on this machine (spec §5–§8), and a
client starts it when none listens. Open the TUI on a session the server holds
with `--attach`; any saved session can be attached, including one a local
`noel` made:

```bash
noel client new --model faux/echo   # prints the new session's id
noel --attach <session-id>          # the TUI on that session
noel client watch <session-id>      # its view as text, from another terminal
```

Several TUIs can attach to one session, and each shows the same transcript and
the run as it streams. Enter sends a prompt; during a run it queues a
follow-up, ctrl+s steers, Up takes the last queued submission back into the
draft, and Esc aborts the run (which withdraws what was queued). `/model
<provider/id>`, `/thinking <level>` and `/compact` change the session; commands
for this process's own sessions, settings and accounts say they are not
available when attached. `/quit` detaches, and a run goes on in the server. If
the session's worker or the server stops, the TUI says so, holds the session
again and redraws it; the server resumes an interrupted run.

## A server on another machine

`--remote <ssh-destination>` uses the server on another machine, through
`ssh <destination> noel bridge`. That machine needs `noel` and nothing else;
its server starts when none listens, and its sessions, models, accounts and
tools are its own:

```bash
noel client --remote build-box ls
noel client --remote build-box new --cwd /srv/app --model anthropic/claude-sonnet-4-5
noel --attach <session-id> --remote build-box
```

- **ssh does the authentication,** with your `~/.ssh/config`, keys, agent and
  known hosts, so `user@host`, a `Host` alias and hosts from `cw setup ssh`
  all work. There is no token and no relay. The server stamps these links
  `{kind: "owner", via: "tunnel"}`, since `ssh` admitted the same user that
  owns it (spec §6).
- **`noel bridge`** joins its stdin and stdout to the server's socket, after
  the same checks a local client makes, and exits when either side closes.
  `--remote-noel <path>` names noel on that machine when it is not on the
  `PATH` that `ssh host command` gets (`~/` at its start is that machine's
  home).
- **`new` needs `--cwd`,** an absolute path on that machine.
- **A dropped connection** is a lost link: `watch` and the TUI reconnect, and
  get the view again. The first connection may prompt (a password, a new host
  key); reconnections run ssh with `BatchMode=yes`, so they never take the
  terminal. `make bridge-journey` runs the whole flow against `sshd` in an
  Alpine container with no network.

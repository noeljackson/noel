# Plan: the TUI is always a client

Decided by the owner (2026-11-05): plain interactive `noel` becomes a client of
a local `noel server` and its session worker, as upstream p2's TUI is always a
client of its server (`docs/p2-audit.md`). Local use and `--remote` use are then
one code path; only the connection differs.

## Today

- `noel` (interactive) runs the session in process: `run_interactive` holds a
  `Runtime`, and `handle_tui_submission` (`crates/noel-cli/src/main.rs`, about
  70 slash commands) calls into it directly. Turns render from `TurnEvent`s.
- `noel --attach <id>` (`crates/noel-cli/src/attach.rs`) holds a `noel.chat`
  reference and renders from the view (`entries` plus `docs["pi.live"]`). The
  worker offers `submit`, `withdraw`, `abort`, `compact`, `configure`,
  `resume` and `entries`; the server offers session list and create. Every other
  command says it is not available when attached.

## Target

- Interactive `noel` ensures a local server (started detached, as `noel
  client` does now), creates or opens the session through `noel.sessions`, and
  attaches. `--remote <dest>` and `--server <url>` change only the connection.
- The session's commands run in its worker; the server's (sessions, models,
  accounts, settings) run in the server. The TUI keeps what is the terminal's
  own: editor, keys, themes, clipboard, external editor, reading local files
  to attach.
- Print, JSON and RPC modes stay in process, so their upstream goldens do not
  change.
- Default for quitting: `/quit` while a run is active aborts it, as today, so
  a local `noel` keeps pi's behaviour; `/detach` leaves it running on the
  server. (A default chosen here; the owner can change it.)

## Slices

Each slice keeps `make check` and `make e2e` green and is committed on its own.

1. **Session operations module.** Move the session side of each command out
   of `main.rs` into one module over `Runtime` (`undo`, `fork`, `clone`,
   `name`, `labels`, `export`, `tree` and branch switch, `summaries`, `todos`,
   `usage`, `diff` and file diffs, `reload`, `trust`, `!`/`!!` shell, tool
   shortcuts, prompt templates and skills). The in-process TUI calls it, so
   behaviour is unchanged.
2. **Worker methods.** `noel.chat` methods for those operations, implemented
   by the module in the worker; results are data, and the client decides how
   to show them. Contract fingerprints regenerated from upstream's Chord.
3. **Server objects.** `noel.sessions`: rename, delete, import, fork and clone
   into new sessions, the picker's listing (names, labels, modified, cwd).
   `noel.models`: models with availability and scoped models. New
   `noel.accounts` (list, login with device codes and browser flows relayed
   to the client, logout, failover status) and `noel.settings` (read and
   write the server machine's settings). Credentials never leave the server.
4. **Rendering parity.** The attached renderer reaches the in-process one:
   thinking, streaming tool output, retries, compaction, nested codemode
   calls, queue and todo panels, footer and usage. Gaps in the view are filled
   in the view, which the desktop and iOS apps share. A test runs one faux
   script both ways and compares the rendered transcript.
5. **Full command set when attached.** The attached TUI implements every
   command over slices 2-3, including the selectors (models, accounts,
   settings, tree and branches, the session picker). Extension UI requests
   (confirmations and dialogs) reach the client over the link.
6. **The flip.** Interactive `noel` goes through the server: `--continue`,
   `--resume`, `--session`, `--fork`, a cwd and a prompt argument map to
   `noel.sessions`. An environment variable keeps the in-process TUI until
   `make e2e` passes the new way.
7. **Remove the in-process TUI.** Delete the dead path; update e2e, the spec
   (§7.4, §9), the non-parity register (upstream pi's TUI is in process) and
   the parity status.

## Status

- Slices 1, 2, 4 and 5 are built; slice 6 runs behind `NOEL_CLIENT_TUI=1`,
  under which every `make e2e` suite passes.
- How a local `noel` hands its session over:
  - The client prepares the session as before (`--session`, `--continue`,
    `--fork`, a new one), lets go of the journal and attaches.
  - Its flags, environment and cwd go to the session's worker in
    `workers/<id>.args.json` when the worker starts. A running worker keeps
    its own, and the client says so.
  - A session reached by a switch (`/resume`, a pick) keeps its model.
  - A journal outside the session directory (`--session <path>`) is named
    in `workers/<id>.journal`, and the server opens it from there.
  - `--no-session` uses a journal in the runtime directory, deleted on exit.
  - A session its worker already holds is attached to without waiting for
    the lock.
- The rules that keep `noel`'s behaviour:
  - Opening a session whose context ends mid-turn resumes it once: `resume`
    without a run named answers first, then the queue.
  - Ctrl+C, `/interrupt` and quitting abort with `keepQueue`.
  - `/quit` ends the worker once no client holds the session.
  - A signal that ends a local `noel` that is the only client stops the
    worker too, so its tools end with it.
  - Submissions and commands keep the order typed.
  - A queued slash-tool command is placed alone.
  - Retries and failover switches are notes in the transcript.
  - A rebuilt binary restarts the client (dogfood).
- Left:
  - Slice 3's login relay and extension UI over the link.
  - Making client mode the default, and measuring its latency.
  - Slice 7.

## Risks

- `main.rs` is large and the TUI is the most tested surface (`make e2e`); the
  slices keep the old path working until slice 7.
- A local server adds a hop over a Unix socket and a server start on first
  use. Measure startup and keystroke-to-submit latency in slice 6.
- Extension hosts run in the worker; anything that drew in the TUI process
  needs the link (slice 5).
- `!` commands run in the worker, without the user's terminal; interactive
  programs there are out of scope, as for an attached TUI.

## Not changed

- The journal format, `noel.chat`'s existing methods, RPC/print/JSON modes,
  and remote access (ssh bridge, pairing).

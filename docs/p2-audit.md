# p2 audit (upstream `apps/p2`)

Upstream: `earendil-works/pi` at `7dc38ff` (2026-11-03), `apps/p2`. Paths below
are relative to `apps/p2` unless they name a noel crate. This is an audit only:
nothing here is on the roadmap yet.

## What p2 is

p2 is upstream's self-hosted server for Pi Durable: a Node server on loopback
(`PORT`, default 8788; data in `~/.pi/p2`), one forked worker per session, a
React web UI, and a sandboxed TUI launcher, built from about 35 Chord v2
plugins on Durable 1.5. It runs Chord v2 only.

Its status matters for how much to copy:

- The README calls it an experimental prototype that "will be gone soon. Do
  not build on it" (`README.md:3-6`).
- The plan is a redesign as p3 next to it, with plugins ported one at a time,
  no data migration, and p2 deleted afterwards (`docs/p3-walkthrough.md:12-20`).
  At this commit `apps/p3` does not exist; Chord v2 already moved into
  `packages/chord` and p2 runs on it (`docs/p3-handoff.md:24-37`).
- `docs/p3-functions/` lists 22 domains, and calls out duplicated
  implementations and drop candidates in p2 (for example four SSH-connection
  implementations).

So p2 shows what upstream wants a hosted Durable app to do, not a stable
contract. noel should port behaviors that fit its design and leave p2's
shapes alone.

## Comparison by area

### Process model

| | p2 | noel |
|---|---|---|
| Server | One per data dir, lock `p2:server:<dir>`, loopback HTTP (`src/hosts/server.ts:103-698`) | One per user, owner-only Unix socket (spec §5) |
| Worker | Chord `createWorkers`, forked per session on first hold; generation per start; idle stop after 5 min with no viewers, calls or tasks (`src/hosts/session-workers.ts:63-101`) | `noel worker` per session; generation per start; idle exit 60 s (`server.workerIdleExitMs`) |
| Worker crash | Restarted for its holders at most 5 times in 60 s, then failed until the owner presses Retry; stderr tail shown redacted (`session-workers.ts:36-37,337-386`) | Restarted at once; the run counts restarts in its journal and ends `restart_limit` at 5 (spec §4.1, §8.3) |
| Server restart | Closes its workers; resumes nothing at startup outside dev mode; a session's run continues on its next hold (`server.ts:645-694`) | Workers outlive the server and are adopted |
| Archive | Stops the worker regardless of what it runs | No archive |

noel is already stronger on crash and restart behavior. p2's Retry button and
"failed start" notice (error plus log tail) are UX worth copying.

### Access and identity

| | p2 | noel |
|---|---|---|
| Local owner | Any cookieless loopback request is `owner/local`, including minting owner tokens (`server.ts:470-478`); no peer check (`docs/p2-notes.md:893`) | Unix socket peer uid must match |
| Remote owner | HTTPS tunnel or reverse proxy you provide, plus `npm run owner-link`: one-time link (10 min, single use, in memory) that sets a signed 30-day cookie (`plugins/devices/owner-access.ts`) | ssh bridge (default), or TLS listener with device pairing and Bearer tokens (spec §16) |
| Device management | Sign out this device; sign out all (rotates the key). No device list (`plugins/devices/contract.ts:18-28`) | `noel client devices`: list, approve, revoke one |
| Guests | Share links per session in `view` or `full` mode, unverified names, up to 1000 guests per link, revocation by revision bump; `full` on a remote env needs `remoteConfirmed` (`plugins/sharing`) | Not implemented (roadmap follow-up) |
| Hosted relay | Radius: outbound WebSocket to a hosted gateway, owner and shared-email users, public port forwards (`plugins/radius`) | None |

Security notes from the audit, for noel's own design:

- p2 trusts loopback as owner. On a shared machine any local user can mint an
  owner token. The docs also record that a revoked guest on loopback "silently
  becomes the owner on reconnect" (`docs/p2-tui-port.md` §4.1). noel's uid
  check avoids both; keep it.
- `owner-token.txt` is rewritten on every start and redeemable by anyone who
  can read the data dir.
- The terminal host's socket accepts any connection; on Linux it is an
  abstract socket, which has no filesystem permissions
  (`plugins/terminals/detached.ts:18`, `main.ts:388-410`).

### Clients

| | p2 | noel |
|---|---|---|
| Web | React UI with composer, sheets, workspace panel | None (CLAUDE.md: no web UI) |
| TUI | `npm run tui [https://host]`: downloads the server's TUI bundle (hash-checked) and runs it in a locked-down Node worker (`--permission`, no fs/net, 512 MiB watchdog); saves only cookies (`src/hosts/tui-*.ts`) | `noel --attach <id> [--remote host]`: noel's own TUI, no downloaded code |
| Desktop / mobile | None in this repo | GPUI desktop app, SwiftUI iOS app |
| Reconnect | Backoff 250 ms to 5 s; bundle hot-swapped when its hash changes | Reconnects and re-holds; a new ssh per reconnect |

p2's TUI has the full session command set when connected (`/sessions`,
`/resume`, `/new`, `/cd`, `/name`, `/fork`, `/clone`, `/model`, `/share`,
`/devices`, `/logs`, `/files`, `/changes`, `/grep`, `/artifacts`,
`/agents`). noel's attached TUI refuses everything that acts on a session
other than the attached one (`crates/noel-cli/src/attach.rs`). This is the
largest UX gap.

### Sessions

| | p2 | noel attached / Mode 2 |
|---|---|---|
| Create | Absolute, `~` or `env:` path, model, thinking; title "New session" | `noel client new --cwd --model` |
| List / pick | Sidebar, `/sessions`, `/resume`, FTS search over titles, cwd and messages (`plugins/search`) | `noel client ls`; no picker when attached |
| Rename | 1-120 chars; the agent can rename via `rename_session` | Not over the server |
| Pin / archive / restore | Yes; no delete | No |
| Clone / fork | Clone copies the stores; fork exports the active context before an entry. TUI only; web has no UI | Local TUI only, not over the server |
| Move (`/cd`) | Relocate an idle session, including to another env, without restarting its worker | No |
| Unread markers | `SessionReads` | No |

### Chat

| | p2 | noel |
|---|---|---|
| Submit | `whenBusy` defaults to `steer`; requestId per author | Enter while busy queues a follow-up, steer key steers |
| Continue | No continue action | `noel client continue` (Durable's `continue`) |
| Abort | Withdraws queued inputs first; your own texts return to your composer, others' go to a draft store they can claim | Abort withdraws the queue (Mode 2 policy); texts are not returned |
| Offline sends (web) | Outbox: shown at once, sent in order, retried 500 ms to 10 s, returned to composer after 6 failures | `noel client send` retries `unavailable` up to 10 times |
| Shell | `!cmd` / `!!cmd` on the conversation's env, one at a time, as a `p2.shell` entry | Local TUI only |
| Chat mode | People talk to each other; the agent only reads a `<chat>` digest when mentioned (`@Pi`) | None |
| Retries shown | `generation.retry` text in the transcript | Retry rows in the TUI and RPC |

### Agent capabilities (plugins that give the model tools)

| p2 | What | noel |
|---|---|---|
| `subagents` | `subagent` tool: spawn, send, configure, stop, archive, answer; children are background tasks the parent's Stop does not reach; replies come back as follow-up inputs; a `team` tool lets siblings message each other; subagents can have their own location | None |
| `agent` | `tools.agent` in code mode: one-shot child answering JSON that matches a schema, 3 attempts, read-only conversation | None |
| `history` | `history_search` (server FTS), `history_read` (50k-char budget, quoted as "not instructions"), `history_handoff` | None |
| `mcp` | Server-owned MCP processes; owner `mcp.json` and project `.pi/mcp.json` (stdio only, no secrets); tools as deferred `mcp__<server>__<tool>` found through `tool_search`. OAuth sign-in and an owner panel are still open (WP8) | None (register: MCP) |
| `artifacts` | `artifact` tool: versioned html/svg/markdown/text up to 2 MiB, served sandboxed | None |
| `send-file` | `send_file`: snapshot a file for download | None |
| `files` | Uploads to `<data>/uploads/<session>`, copied to the remote env when the session is remote | Images in prompts locally; none when attached |
| `prompt` | AGENTS.md (global and per directory), skills, prompt templates, watched and reloaded | Same sources, local |
| `codemode` | Durable's extension, on by default (`settings.json` `codemode`) | Off unless selected |
| `powershell` | Offered on Windows machines | None |

### Environments and workspace

| | p2 | noel |
|---|---|---|
| Remote envs | `envs.json` (ssh only), keys generated or uploaded into `<data>/ssh/keys`, p2's own `known_hosts`, Test and Trust key flow, one shared ssh connection per env per worker; no local fallback (`plugins/envs`) | `--workspace ssh://host/path` on hosts trusted with `noel workspace trust`; uses the user's ssh config and keys (noel-env) |
| Per-session location | `{env, cwd}`, movable while idle; subagents may have their own | Fixed at creation |
| Workspace panel | Files, git status, diff, log, search, upload/download, review comments sent to chat | None |
| File completion | `@`/Tab file index with pi's ranking | Local TUI only |
| Terminals | A detached terminal host owns PTYs (via pi-env daemons), survives server and worker restarts, multiple viewers, ghostty WASM renderer; web only, no TUI | None |
| Port forwards | Via Radius only, owner or public URLs, agent `ports` tool | None |
| Voice | Voice mode via an external speech service and a bundled Python voice server | None |

### Operations

| | p2 | noel |
|---|---|---|
| Status | Per-session running, error, usage, tasks, viewers; Retry for a failed worker | `noel client watch`; server and worker logs on disk |
| Logs | In-memory, per source, secrets redacted, viewable in the UI (`/logs`) | Log files in the runtime dir |

## Candidates for noel

Ordered by value for noel's goals (remote use from the TUI and native apps),
not by p2's size. None is scheduled.

1. **Full session management over the server.** List and pick, rename, fork,
   clone, archive and restore, and `/cd`, as `noel.sessions` and `noel.chat`
   methods, so the attached TUI and the apps lose the "not available when
   attached" gaps. Session search over titles and messages.
2. **Model and settings over the server.** A model picker fed by
   `noel.models`, and provider login from a client (p2's `login` plugin runs
   one flow at a time per owner and shows device codes).
3. **Abort returns texts.** Withdrawn queued inputs go back to their author's
   composer, as p2 does, instead of being dropped.
4. **Failed-worker UX.** Show a worker's failed start with the redacted log
   tail and a Retry action, plus `/logs` from a client.
5. **Guest sharing.** View and full links per session with revocation;
   already a roadmap follow-up. p2's model (grant revision bump ends live
   objects) fits noel's principals and revocation.
6. **Subagents and history tools.** Larger, agent-behavior changes; they
   follow Durable's tasks and would need their own design against noel's run
   model.
7. **Workspace views** (files, status, diff) for the apps, read-only, through
   the session's environment.
8. **Per-session location moves** (`/cd`, other env) when idle.

Not candidates, or only after a decision:

- **Web UI, Radius, voice, port forwarding:** a web UI is excluded by
  CLAUDE.md; Radius is upstream's hosted service; voice and forwards depend on
  Radius or an external service.
- **Downloaded, sandboxed TUI facets:** noel's TUI is its own code; nothing is
  shipped from the server.
- **Loopback-as-owner and token files:** noel's uid check is stronger.
- **MCP:** a separate decision already recorded in the register.

## Open questions for the owner

- Should noel follow p2 at all while upstream says it will be replaced, or
  wait for p3 and port from that?
- Session management over the server adds `noel.sessions` and `noel.chat`
  methods beyond what Durable itself defines: is that within "upstream
  parity" (p2 has them) or a contract change to approve?
- Subagents: in scope for noel, and if so, as Durable-style background tasks
  or noel's own design?

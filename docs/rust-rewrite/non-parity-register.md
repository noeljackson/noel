# Rust Rewrite Non-Parity Register

Every Rust behavior should either match a committed TypeScript parity fixture or
appear here with a deliberate reason for not cloning the TypeScript behavior.

## Active Non-Parity Decisions

### No TypeScript Runtime Dependency

Rust must not import, shell out to, or embed TypeScript or JavaScript at
runtime. TypeScript is allowed only inside the Docker reference runner used to
generate parity fixtures.

Reason: the product goal is a native Rust CLI/TUI, not a wrapper around the old
runtime.

### No Web UI Product Path

The old TypeScript web UI is not part of the active Rust product.

Reason: the active product scope is CLI/TUI-only. Historical web UI labels may
remain in GitHub for cutover tracking, but new implementation work should not
add web UI dependencies.

### No npm or Node Host Workflow

Normal development, tests, and validation must not run npm on the host.

Reason: the repository is Rust-only. TypeScript reference execution is isolated
to Docker so parity checks do not contaminate the product toolchain.

### No Automatic Legacy Session Migration

Rust does not automatically read or migrate old TypeScript session logs.

Reason: Rust live sessions have a durable replay JSONL contract. TypeScript v3
JSONL export/import and direct open are supported where practical, but old logs
are not automatically migrated in place. Check out the tip of upstream `main`
for forensic reading of TypeScript behavior.

### No Live Providers in Normal Tests

Normal tests use faux/local providers or sanitized request fixtures.

Reason: CI and local validation must not require credentials, spend money, or
leak request data. Real-provider smoke remains opt-in.

### No Full Autonomous Transcript Oracle

The parity harness does not use full live autonomous transcripts as its primary
correctness signal.

Reason: provider sampling, shell timing, terminal dimensions, and network state
make full transcripts brittle. The harness instead captures deterministic
contracts for request shape, normalized messages, tool dispatch, storage,
settings, and TUI markers.

### Named Multi-Account auth.json

Rust writes `auth.json` as `{ "<provider>": { "<account>": <credential> } }`
instead of the flat TypeScript shape. Flat files are read and migrated
transparently, so existing setups keep working; only the write format diverges.

Reason: multiple accounts per provider (work/personal, plan variants) need a
named account layer for session binding (`/account`), per-account OAuth
refresh, and per-account quota probes (`pi accounts status`). The TypeScript
shape cannot represent that.

### Native OAuth Enrollment Flows

Rust supports interactive OAuth login (`pi login openai-codex` device
authorization, `pi login anthropic` PKCE paste-code) using the official Codex
CLI and Claude Code public client registrations and endpoint contracts.

Reason: upstream pi authenticates via API keys or imported credential files.
Native enrollment removes the manual import step while storing credentials in
the same v2 accounts and refresh machinery. The official CLIs' flows are
public, documented in their shipped binaries, and produce tokens for the same
upstream services.

### Todo Tool and Edited-Files Panel

Rust adds a `todo` builtin tool (checklist with `pending`/`in_progress`/
`completed`, journaled as a `todos` session record) and per-session
edited-file tracking (`edited_files` journal record) powering the TUI task
widget (`ctrl+t`) and the `/diff` side panel. The TypeScript upstream has no
equivalent tool or panel, so `local-tools.json` does not list `todo`; the
parity tests assert upstream coverage rather than exact tool-set equality.

Reason: agent CLIs (Claude Code's TodoWrite, Codex's update_plan, Kimi's task
list) converge on a model-callable task list as standard scaffolding for
multi-step work, and on a session diff view for edited files. Both need
journaled session state to survive resume, which the upstream session schema
does not carry.

### Claude Code Version User-Agent

For Claude Code OAuth requests, TypeScript pins `user-agent: claude-cli/2.1.75`.
Rust reports `claude-cli/<latest published version>` instead: the model refresh
fetches the newest `@anthropic-ai/claude-code` version from the npm registry,
caches it in `model-cache.json`, and falls back to a bundled minimum
(`PI_CLAUDE_CODE_VERSION` overrides both). The parity test pins the version to
the fixture value to keep comparing the identity shape.

Reason: Anthropic's API rejects OAuth requests for new models when the
reported Claude Code version is too old ("version X or newer is required"). A
pinned version strands every new model until the next pi release; tracking the
published version keeps OAuth-gated models usable.

### Native Web Tools and Transcript Previews

Rust adds model-callable `web_search` / `web_fetch`, with DuckDuckGo HTML,
optional Brave/SearXNG search, bounded native HTTP fetching, and local fixture
tests. Offline mode suppresses and blocks these network tools. The upstream
local-tool fixture remains a subset, not the complete Rust inventory.

Rust also provides recent-turn `ctrl+o` thinking/tool-output previews,
status-prioritized todos, segmented tool headers, and a visible editable queue.
These are terminal-only UX additions; no browser or web UI runtime is introduced.

### Clipboard Drafts, Conversation Undo, and Session Usage

Rust adds `ctrl+v` clipboard image drafts via bounded native clipboard helpers,
`/undo` (context-only rewind to the latest user message; no file rollback), and
`/usage` (persisted provider-reported tokens plus cached account quota probes).
Token totals include completed requests, are not estimates or dollar costs,
and are not reduced by undo or compaction. Compaction keeps complete recent
user turns instead of splitting tool calls from their results. Summaries are
provider-independent semantic checkpoints generated by the active model with
tools disabled. Automatic triggers use estimated active-context tokens (80% by
default), not a message count. Focused `/compact` and RPC `compact` are supported.
Original journal records survive; checkpoint/context replacement is atomic and
failures/cancellation never fall back to silent truncation. Persistent instructions
remain outside summarization. A failed automatic compaction no longer aborts the
turn: the run continues with the original history and the failure is reported as
an event (system prompt / tools / persistent-instruction / retained-turn token
breakdown when the projected request still exceeds the budget); re-attempts are
skipped until history changes. Repeated compactions merge previous checkpoints;
oversized transcripts use bounded rolling fragments. Provider-native opaque
compaction remains deferred.

Reason: expose useful terminal/session controls without GUI dependencies or
unsafe workspace rollback. Attachment drafts are not injected into text-only
steering/follow-up queues; they stay editable until the active turn ends.

### Owned Transcript Viewport

Rust defaults to an alternate-screen transcript with a fixed composer and
app-owned mouse/PageUp/PageDown scrolling. Latest-follow and anchored-reading
are separate ephemeral view states; reading anchors use entry/logical-line/source
byte positions so streaming and reflow do not move the reader. Esc dismisses
overlays first, then returns to latest while reading without modifying the draft
or interrupting the turn; at latest existing clear/interrupt behavior remains.
The clickable bottom hint marks new output while reading. Moving live thinking
previews freeze while reading and refresh on return to latest.

Reason: long responses should remain readable while the input, attachments,
queue, tasks, and status stay accessible. `--no-alt-screen` retains native inline
scrollback and terminal-owned selection. Owned mode supports grapheme-aware
left-drag selection with copy-on-release, Ctrl+C, and `/copy`; local clipboard
helpers fall back to terminal OSC 52 requests. Selection follows logical text
through reflow/streaming, excludes folded-away output, and stays ephemeral.
Terminal modifier selection and `/export` remain available. View state is not
durable session state.
Local/faux unit and live tmux tests cover this UI; upstream TS fixtures are unchanged.

### Model Cache Refresh Defenses

The model cache refresh is per-provider defensive rather than all-or-nothing.
An empty provider listing keeps the previously cached models instead of wiping
them (a truncated listing is how new models silently disappear); an exhausted
Anthropic pagination budget fails the fetch instead of caching a partial list;
a pass where every provider failed leaves `refreshedAt` untouched so the next
startup retries immediately instead of waiting out the TTL; and the cache is
re-read right before writing so concurrent pi processes cannot clobber each
other's fetched models. Refresh diagnostics persist into the cache and surface
through `/diagnostics`. `/models refresh` runs a foreground refresh with a
per-provider report and `/models reset` deletes the cache so the next refresh
starts from built-ins plus explicit `models.json` entries only.

Reason: model lists are provider-owned and change out of band; a stale or
poisoned cache must never delete working models, and the user needs a manual
escape hatch when the background refresh cannot recover on its own.

### Configurable Agent Round Cap

The per-run provider round cap (`agent.maxToolRounds`, default 200, including
steering rounds) is configurable in settings and reports a dedicated
round-limit error that names the cap and the setting instead of masquerading
as an invalid tool call.

Reason: the default is a runaway-loop bound, not a usage quota; long working
sessions legitimately exceed it and the recovery path (continue or raise the
cap) should be discoverable from the error itself.

### Batched Queue and Steering Dispatch

Queued prompts and mid-turn steering now dispatch as one combined message
instead of one-at-a-time: everything pending at an injection point (before a
provider round, or when the turn ends) joins into a single user message
separated by `---`, so a burst of queued thoughts reads as one instruction
rather than several consecutive turns. Queued tool commands (`/read`, `/grep`,
…) still dispatch individually ahead of prose because their arguments are
positional. The `steeringMode`/`followUpMode` settings are ignored (a legacy
`queueMode` value is still migrated) since there is no mode left to choose.

Reason: serial one-per-turn delivery made a queued burst look like five
separate conversations with the model, and made steering feel broken when
later messages landed turns after the user sent them.

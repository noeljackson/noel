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

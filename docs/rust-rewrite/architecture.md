# Rust Rewrite Architecture

Original tracking issue: https://github.com/noeljackson/pi/issues/4

Current doc refresh: https://github.com/noeljackson/pi/issues/38

This document records the implemented Rust architecture. The product path is
CLI/TUI-only and Rust-only. TypeScript behavioral reference should use the tip
of upstream `main` from `https://github.com/earendil-works/pi`.

## Goals

- Build and ship a native Rust `pi` binary.
- Remove npm, Node.js, TypeScript, web UI, and browser runtime dependencies from the product path.
- Preserve CLI/TUI coding-agent workflows from the TypeScript reference.
- Preserve active conversation context during explicit reload.
- Keep behavior testable without real provider credentials.
- Keep TypeScript reference fixture generation isolated to Docker.

## Non-Goals

- No web UI.
- No hot module reload.
- No npm package manager workflows.
- No embedded TypeScript or JavaScript extension runtime.
- No automatic compatibility promise for old TypeScript JSONL sessions.

## Workspace Shape

The Rust implementation uses a Cargo workspace with explicit crate boundaries:

- `crates/pi-cli`
  - Binary entrypoint.
  - Argument parsing.
  - Mode dispatch.
  - Interactive TUI loop.
  - Slash command dispatch.
  - Package/config CLI commands.
- `crates/pi-core`
  - Agent loop.
  - Session state.
  - Conversation orchestration.
  - Session persistence and export/import.
  - Runtime reload state.
- `crates/pi-config`
  - Config paths.
  - Settings loading.
  - Auth loading.
  - Keybindings config.
  - Resource/package discovery.
  - Model cache loading.
- `crates/pi-ai`
  - Provider request/response normalization.
  - Model registry data.
  - Credential resolution.
  - Faux provider for tests.
  - Production provider adapters.
- `crates/pi-tools`
  - Shell execution.
  - Filesystem reads/writes.
  - Search/list operations.
  - Patch/edit operations.
- `crates/pi-tui`
  - Terminal renderer and layout primitives.
  - Editor/input helpers.
  - Selectors.
  - Key event normalization.
  - Markdown/text rendering metadata.

Dependencies flow inward: CLI/TUI depend on core, core depends on config/AI/tools,
and config/AI/tools do not depend on TUI.

## Runtime State Split

The Rust implementation separates durable session state from reloadable systems.

`SessionState` owns user work:

- Session id and file path.
- Session cwd.
- Conversation messages.
- Tool calls and tool results.
- Queued messages: steering and follow-ups.
- Runs: id, request id, and status.
- Active model selection.
- Active thinking level.
- Active tool names.
- Session tree metadata.
- Current compaction/retry metadata.
- User-visible session name and labels.

Reloadable runtime state owns replaceable wiring:

- Settings.
- Auth resolver inputs.
- Model registry and model cache.
- Image model registry.
- Provider availability.
- Tool definitions.
- Prompt templates.
- Skills.
- Context files.
- Extensions.
- Keybinding map.
- Theme.
- Dynamic slash completions.

`Runtime` coordinates the current session plus reloadable systems. Reload builds
and validates replacement systems before swapping them into the active runtime.
The active session is never rebuilt as part of a normal reload.

## Reload Semantics

Rust reload is explicit through `/reload`.

Reload behavior:

1. Reject reload if an assistant turn is in progress.
2. Preserve reload-stable session choices:
   - active model
   - active thinking level
   - active tool names
   - cwd
   - queues
   - session tree state
3. Load settings, auth metadata, prompts, context files, keybindings, themes, provider availability, model metadata, package resources, and tool definitions.
4. Validate the replacement systems.
5. If validation fails, keep the old systems and surface diagnostics.
6. If validation succeeds, swap the reloadable systems.
7. Reconcile active model/tool choices and report invalid selections without clearing context.
8. Re-render the TUI using the new keybindings/theme/systems.
9. Notify JSON executable extensions with a `reload` event.

Reload must not clear:

- conversation messages
- cwd
- session id/file
- tool call history
- queued messages
- branch/session tree state
- accumulated context

There is no file-watch hot module reload.

## CLI Mode Behavior

`pi-cli` dispatches:

- `interactive`: default when stdin is a TTY.
- `print`: `--print` or piped stdin.
- `json`: `--mode json`.
- `rpc`: `--mode rpc` with stdin not a TTY.

All modes use the same config, model resolution, auth, session, and provider
machinery where applicable.

## RPC Mode

`--mode rpc` speaks upstream pi's RPC protocol over stdin and stdout, plus the
`run-id`, `expected-run-id`, `request-id`, and `resume-run` extensions (see the
README's RPC Mode and the non-parity register). It lives in two modules of
`pi-cli`:

- `rpc_wire.rs` holds the protocol: serde types for commands, responses,
  events, and messages in upstream's field order, and `Recorder`, which turns
  one run's `TurnEvent`s into upstream's records. A run is one upstream run
  (one `agent_start`, one `agent_end`, one `agent_settled`) however many
  engine calls it takes; the recorder closes whatever an abort or a failure
  left open, ending the run's last turn with an aborted or error reply.
- `rpc.rs` holds the mode: a reader thread that splits stdin on LF only, one
  writer that prints each record as one line, and the command loop. While a
  run is active one task polls the engine call and handles commands that
  arrive meanwhile (`tokio::select!`), so records keep the order things
  happen in. The engine reports events through a callback; the command loop
  and the callback share the queue and the active run behind a lock that is
  never contended.

Each run builds its provider at the prompt (so `set_model` applies to the next
run) and gets a `RunControl`: the shared steering mailbox and its own
cancellation token, which `abort` cancels. Follow-ups need no engine support:
when an engine call returns with follow-ups queued, the run makes another
engine call with them as its prompt. While the engine holds the runtime,
reads answer from a copy of the session that the run's events keep current,
refreshed after each engine call; commands that would change the session are
refused.

Runs are durable. `SessionState.runs` holds each run's id, request id, and
status, apart from the messages, because the messages are what compaction
checkpoints and `/undo` snapshots rewrite: a mid-turn compaction can summarize
a run's prompt away, but not its identity. The journal saves a run's start on
the record of its prompt (`message` with a `run` field, which older versions
ignore), so the two are one write: no run without its prompt, no prompt
applied twice. `Runtime::start_run` makes that write before `prompt` answers;
`run_finished` records how a run ended before `agent_settled`. A load reports
a started run without `run_finished` as interrupted. Compaction checkpoints
copy the journal, so the records survive them; fork and import start new
sessions without runs. `resume_run` continues an interrupted run with the
engine's continue path, the same one the TUI uses to finish a turn on reopen:
it runs again the replay-safe calls the last reply has no results for (as a
`RoundResumed` round), then asks the model.

The queue lives in RPC mode's `Queue` and is saved, whole, in the session's
`queued_messages_snapshot` record: steering first, with a `steering` count
that older versions and the TUI ignore, reading every message as a follow-up.
An enqueue or `clear_queue` is saved before it is applied and acknowledged; a
delivered message leaves the saved queue once its user message is saved, so a
crash between the two delivers it again unless the queue is cleared.

## Config Model

Default root:

- `~/.pi/agent`

File names:

- `settings.json`
- `auth.json`
- `models.json`
- `model-cache.json`
- `keybindings.json`
- `extensions/`
- `skills/`
- `prompts/`
- `themes/`
- `sessions/`

Project-local roots:

- `.pi/`
- `.agents/skills`

Context discovery:

- Load global `AGENTS.md`/`CLAUDE.md` from the agent dir.
- Load cwd ancestor `AGENTS.md`/`CLAUDE.md` in root-to-leaf order.

Validation rules:

- Invalid settings do not replace active settings.
- Invalid keybindings do not replace active keybindings.
- Invalid resources produce diagnostics.
- Missing optional resource paths produce diagnostics.
- Missing required session cwd blocks session resume outside interactive recovery.

## Session Model

Rust live sessions use an append-only replay JSONL log. JSONL export/import and
direct session open support the TypeScript v3 session-tree shape where
applicable, with a `session` header and entry `id`/`parentId` chain.

Required properties:

- Durable across process restarts.
- One writer per session: writers hold an exclusive lock on a separate
  `<journal>.lock` file (compaction replaces the journal by rename), shared by
  every handle in the owning process; another process gets
  `SessionError::Locked`. Listing and fork/clone/import sources only read.
- Supports fork/clone operations and TS-shaped JSONL open/export/import.
- Full in-place active-leaf session-tree editing remains parity work.
- Can recover partial sessions after process interruption: each append writes
  its record and newline in one write, readers ignore a torn final segment,
  and writers drop it (or terminate a complete record) before appending.
- Records RPC runs and the queue (steering and follow-ups) so a restarted pi
  can report an interrupted run and continue it (see RPC Mode).
- Can be tested with in-memory and temp-dir storage.
- Persists active model and active thinking level.
- Supports JSON, JSONL, and local HTML export/import where applicable.

Legacy TypeScript sessions are intentionally not migrated automatically. Check
out the tip of upstream `main` for reading TypeScript behavior.

## Provider Architecture

Provider requests are normalized from session state and reloadable systems.
Provider responses stream normalized events back into the runtime:

- text delta
- thinking delta
- tool call start/delta/complete
- usage
- stop
- error

Implemented providers:

- `faux` for tests.
- OpenAI chat/responses.
- OpenAI Codex.
- Azure OpenAI Responses.
- Anthropic Messages.
- Google Gemini.
- Google Vertex.
- OpenRouter.
- GitHub Copilot.
- Amazon Bedrock bearer-token Converse.
- Mistral.
- Cloudflare Workers AI.
- Cloudflare AI Gateway.
- DeepSeek.
- Groq.
- Cerebras.
- xAI.
- Z.AI.
- Hugging Face.
- Together.
- Moonshot AI.
- OpenCode.
- Vercel AI Gateway.
- Fireworks.
- MiniMax.
- Kimi Coding.
- Xiaomi MiMo.

Credential resolution supports runtime API keys, named multi-account
`auth.json` entries, environment variables, Claude Code login token fallback,
Codex/ChatGPT login token fallback, and provider-specific environment
settings. Imported Claude Code and Codex/ChatGPT OAuth tokens are refreshed
automatically when they expire; refreshed credentials persist to pi's own
`auth.json`. Per-account quota probes feed `pi accounts status` and are cached
in `usage-cache.json`.

Model refresh runs in the background when enabled, online, stale, and
authenticated. It does not block startup. Refreshed models become visible after
`/reload` or the next startup.

## Thinking Architecture

Thinking level is session state and can be set with `--thinking`, `/thinking`,
or left/right while the `/model` selector is open.

Model-specific levels:

- Anthropic Opus adaptive models: `high`, `xhigh`, `max`.
- Other Anthropic adaptive models: `high`, `xhigh`.
- OpenAI/Codex reasoning models: `minimal`, `low`, `medium`, `high`, `xhigh`.
- Google thinking models: mapped to provider thinking budgets.
- Non-thinking models: no active thinking level.

Provider adapters translate normalized thinking levels into provider-specific
payloads.

## Tool Architecture

Tool definitions live in `pi-tools`; active tool selection lives in session
state.

Built-ins:

- `read`
- `bash`
- `edit`
- `write`
- `grep`
- `find`
- `ls`

Rules:

- Tools receive an explicit cwd.
- File mutation tools must protect unrelated user changes.
- Search/list tools are read-only.
- Tool tests do not call providers.
- Shell execution supports configured shell path and command prefix.
- TUI progress rendering is controlled by `terminal.showTerminalProgress`.

Patch/edit behavior uses structured local logic rather than provider-specific
side effects.

## TUI Architecture

`pi-tui` owns terminal mechanics and reusable rendering metadata. `pi-cli` owns
the product event loop and command interactions.

Implemented TUI concepts:

- Editor input with visible cursor.
- Streaming assistant output.
- Tool call/result display.
- Shell command progress display.
- Model selector.
- Settings selector.
- Session selector/tree.
- Dynamic slash completion.
- Diagnostics/status display.
- Reload status/error display.
- Theme application.
- Image model listing and local image generation output commands.

Keybindings:

- Load defaults plus user overrides.
- Support reload without recreating `SessionState`.
- Keep action checks routed through the keybinding layer where user-configurable behavior is required.

## Extension and Package Resource Architecture

Rust package/resource support is local and git based.

Package discovery supports:

- Resource directories by convention: `extensions/`, `skills/`, `prompts/`, `themes/`.
- `package.json` with a `pi` key.
- Object package entries with include/exclude filters.
- `.gitignore`, `.ignore`, and `.fdignore` handling.
- Disabled resource state from settings and `pi config enable|disable`.

Extension execution supports:

- Raw executable extensions.
- JSON stdio executable extensions.
- JSON lifecycle events for `reload` and `shutdown`.
- JSON model-tool registration from adjacent `.pi-extension.json` manifests.
  Registered tools receive `kind: "tool"` protocol requests and return JSON
  command responses.
- Diagnostics for manifest and lifecycle failures.

Intentionally unsupported:

- Running TypeScript/JavaScript extensions in-process.
- Installing npm extension packages.
- Loading npm package-manager metadata as executable code.
- TypeScript UI primitive/custom renderer APIs.

Future Rust extension work should build on the JSON executable protocol with a
Rust SDK or provider registration helpers.

## Validation

Primary validation:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`
- `make check`
- `make e2e`
- `make docker-e2e`
- `make parity-check`

Manual validation:

- `make smoke-claude-opus-oauth`
- `make test-smoke`
- `make ts-parity-fixtures`
- `make ts-parity-update`
- `make ts-parity-drift`
- `make ts-parity-agent`

`make parity-check` runs the committed fixture inventory checks, Rust
fixture-backed assertions, and Docker drift detection. `make
ts-parity-fixtures` is the only supported path for executing TypeScript
reference code. It runs npm inside Docker only and defaults to the upstream
TypeScript repository `https://github.com/earendil-works/pi` on `main`. `make
ts-parity-update` refreshes fixtures from the moving reference ref. `make ts-parity-drift`
regenerates those fixtures, fails if committed fixtures would change, writes
`target/ts-parity-drift/brief.md`, and runs Rust TS-shape tests when no drift is
present. `make ts-parity-agent` uses the same harness and pipes the brief to
`PI_PARITY_AGENT_COMMAND` when that environment variable is set.

The parity fixtures are sanitized provider request captures. They cover request
shape for URLs, selected headers, request bodies, and provider payloads. The
Rust `pi-ai` tests compare local builders against those captures.
`rpc-protocol.json` is a protocol transcript instead: upstream `pi --mode rpc`
driven against a local stub model, with every stdout record in order and
volatile values replaced by placeholders. `pi-cli`'s `rpc_parity` test replays
each scenario against `faux/script` and compares the records. When upstream
changes, the drift harness produces both a raw diff and an agent-oriented brief
that states the constraints: no host npm, TypeScript only in Docker, keep Rust
changes scoped to parity behavior, and rerun Rust validation before committing.
The `pi-parity` crate owns fixture inventory, source metadata, script-output
mapping, redaction checks, and parity documentation coverage.

This is request-shape parity, not a live-provider guarantee. Full product
coverage still relies on Rust unit tests, tmux TTY e2e, and manual real-provider
smoke tests.

## Current Cutover Status

The Rust cutover is complete for the CLI/TUI product path. Remaining gaps should
be tracked as normal GitHub issues and must preserve the Rust-only, no-npm, no-web
scope unless the user explicitly changes that product direction.

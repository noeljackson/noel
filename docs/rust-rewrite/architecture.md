# Rust Rewrite Architecture

Tracking issue: https://github.com/noeljackson/pi/issues/4

This document defines the Rust architecture target before implementation. The design is CLI/TUI-only and keeps `ts-reference` as the behavior reference.

## Goals

- Build a native Rust `pi` binary.
- Remove npm, Node.js, TypeScript, web UI, and browser runtime dependencies from the final product path.
- Preserve required CLI/TUI coding-agent workflows.
- Preserve active conversation context during normal reload.
- Keep behavior testable without real provider credentials.

## Non-Goals

- No web UI.
- No hot module reload.
- No TypeScript extension runtime compatibility in the first Rust cutover.
- No npm package manager workflows.
- No automatic compatibility promise for old JSONL sessions until the Rust session schema is finalized.

## Workspace Shape

Use a Cargo workspace with small crates and explicit boundaries:

- `crates/pi-cli`
  - Binary entrypoint.
  - Argument parsing.
  - Mode dispatch.
  - Process-level setup.
- `crates/pi-core`
  - Agent loop.
  - Session state.
  - Conversation orchestration.
  - Reload coordinator.
  - Slash command dispatch.
- `crates/pi-config`
  - Config paths.
  - Settings loading.
  - Auth loading.
  - Keybindings config.
  - System/context prompt loading.
- `crates/pi-ai`
  - Provider traits.
  - Normalized stream events.
  - Model registry.
  - Credential resolution.
  - Faux provider for tests.
- `crates/pi-tools`
  - Shell execution.
  - Filesystem reads/writes.
  - Search/list operations.
  - Patch/edit operations.
  - Git helpers.
- `crates/pi-tui`
  - Terminal renderer and layout.
  - Editor/input.
  - Selectors.
  - Key event normalization.
  - Markdown/text rendering.

The crate split can be adjusted during implementation, but dependencies should flow inward: CLI/TUI depend on core, core depends on config/AI/tools, and config/AI/tools do not depend on TUI.

## Runtime State Split

The Rust implementation must separate durable session state from reloadable systems.

`SessionState` owns user work:

- Session id and file path.
- Session cwd.
- Conversation messages.
- Tool calls and tool results.
- Queued steering/follow-up messages.
- Active model selection.
- Active thinking level.
- Active tool names.
- Session tree metadata.
- Current compaction/retry metadata.
- User-visible session name/labels.

`ReloadableSystems` owns replaceable runtime wiring:

- Settings.
- Auth resolver.
- Model registry.
- Provider registry.
- Tool definitions.
- Prompt templates.
- Skills.
- Context files.
- System prompt and appended system prompt.
- Keybinding map.
- Theme.
- Slash command registry.

`Runtime` owns the current pair:

- `Arc<RwLock<SessionState>>`
- `ArcSwap<ReloadableSystems>` or equivalent atomic replacement mechanism
- Event bus
- Cancellation handles
- Persistence handle

The exact synchronization primitive can change, but readers must see either the old valid systems or the new valid systems. They must not observe a partially reloaded state.

## Reload Semantics

Rust reload is explicit through `/reload` first. File-watch reload can be added later, but is not required.

Reload algorithm:

1. Reject reload if an assistant turn or compaction is actively mutating the session.
2. Snapshot reload-stable session choices:
   - active model
   - active thinking level
   - active tool names
   - cwd
   - queues
3. Load settings, auth metadata, prompts, context files, keybindings, themes, providers, model metadata, and tool definitions into a new builder-owned value.
4. Validate the new value completely.
5. If validation fails, keep the old `ReloadableSystems`, append or display diagnostics, and leave `SessionState` unchanged.
6. If validation succeeds, atomically swap `ReloadableSystems`.
7. Reconcile active model/tool choices:
   - preserve still-valid model and tool selections;
   - preserve removed model in `SessionState` but mark it invalid for the next send;
   - ask the user to select a valid model instead of clearing context;
   - drop unavailable active tool names from the next request only after surfacing diagnostics.
8. Re-render TUI using the new keybindings/theme/systems.

Reload must not clear:

- conversation messages
- cwd
- session id/file
- tool call history
- queued messages
- branch/session tree state
- accumulated context

## CLI Mode Behavior

`pi-cli` dispatches:

- `interactive`: default when stdin is a TTY.
- `print`: `--print` or piped stdin.
- `json`: `--mode json`.
- `rpc`: `--mode rpc`, if retained after inventory validation.

The first implementation phase should support `--help`, `--version`, `--print`, model selection flags, session flags, tool flags, and prompt/file inputs. Advanced selectors can land in later issues.

## Config Model

Keep the current default root:

- `~/.pi/agent`

Initial file names:

- `settings.json`
- `auth.json`
- `models.json`
- `keybindings.json`
- `prompts/`
- `themes/`
- `sessions/`

Project-local root:

- `.pi/`

Context discovery:

- Load global `AGENTS.md`/`CLAUDE.md` from the agent dir.
- Load cwd ancestor `AGENTS.md`/`CLAUDE.md` in root-to-leaf order.

Validation rules:

- Invalid settings do not replace active settings.
- Invalid keybindings do not replace active keybindings.
- Missing optional resource paths become diagnostics.
- Missing required session cwd blocks session resume outside interactive recovery.

## Session Model

Use an append-only session log unless implementation proves a structured snapshot is simpler and safer.

Required properties:

- Durable across process restarts.
- Supports session tree/fork operations.
- Can recover partial sessions after process interruption.
- Can be tested with in-memory and temp-dir storage.

Before cutover, decide whether to:

- migrate old JSONL sessions;
- read old JSONL sessions without writing them;
- or intentionally abandon old session compatibility.

That decision must be documented in the cutover issue.

## Provider Architecture

Provider trait:

- accepts normalized request input built from `SessionState` and `ReloadableSystems`;
- returns an async stream of normalized events;
- supports cancellation;
- never exposes provider-specific partial state above `pi-ai`.

Normalized events:

- text delta
- thinking delta
- tool call start/delta/complete
- usage
- stop
- error

Initial providers:

- `faux` for tests.
- OpenAI Responses-compatible adapter.
- Anthropic adapter.
- Google Gemini adapter.

Additional providers are follow-up issues after stream normalization and tests are stable.

## Tool Architecture

Tool definitions live in `pi-tools`; active tool selection lives in `SessionState`.

Required built-ins:

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
- Tool tests must not call providers.
- Shell execution must support configurable shell path and command prefix if retained in settings.

Patch/edit behavior should prefer structured diff/patch application instead of ad hoc string replacement where possible.

## TUI Architecture

`pi-tui` should own terminal mechanics and reusable widgets. `pi-core` should own product state and commands.

Required TUI concepts:

- Editor input.
- Streaming assistant output.
- Tool call/result display.
- Model selector.
- Settings selector.
- Session selector/tree.
- Keybinding hint display.
- Diagnostics/status display.
- Reload status/error display.

Keybindings:

- Resolve all action checks through a keybinding manager.
- Load defaults plus user overrides.
- Support reload without recreating `SessionState`.

## Extension and Package Resource Scope

The TypeScript extension runtime is not part of the first Rust cutover.

Initial Rust behavior:

- Load built-in tools and commands.
- Load local prompts and skills from configured paths.
- Do not execute TypeScript/JavaScript extensions.
- Do not install npm extension packages.

A later Rust-native extension design can add:

- static plugin manifests;
- subprocess-based extensions;
- WASM extensions;
- or a Rust dynamic plugin story.

## Validation

During the Rust implementation phase:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`

When TypeScript files are still changed, also run the repository-required TypeScript check. Documentation-only changes do not require `npm run check`.

Issue-specific tests:

- Config reload preserving context.
- Invalid reload preserving old systems.
- Session persistence and recovery.
- Provider stream normalization with faux providers.
- Tool execution and file mutation behavior.
- Keybinding reload.
- TUI smoke tests where feasible.

## Milestone Order

1. Discovery and architecture.
2. Core CLI runtime.
3. Agent, providers, and tools.
4. TUI parity and persistence.
5. Rust-only cutover.

Do not remove npm/Node/web artifacts until CLI/TUI parity and Rust validation are complete.

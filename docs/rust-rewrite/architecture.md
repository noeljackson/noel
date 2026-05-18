# Rust Rewrite Architecture

Original tracking issue: https://github.com/noeljackson/pi/issues/4

Current doc refresh: https://github.com/noeljackson/pi/issues/38

This document records the implemented Rust architecture. The product path is
CLI/TUI-only, Rust-only, and keeps `ts-reference` only as a behavioral reference.

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
- Queued follow-up messages.
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
- `rpc`: `--mode rpc`.

All modes use the same config, model resolution, auth, session, and provider
machinery where applicable.

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

Rust sessions use an append-only JSONL log.

Required properties:

- Durable across process restarts.
- Supports session tree/fork/clone operations.
- Can recover partial sessions after process interruption.
- Can be tested with in-memory and temp-dir storage.
- Persists active model and active thinking level.
- Supports JSON, JSONL, and local HTML export/import where applicable.

Legacy TypeScript sessions are intentionally not migrated automatically. Keep
the `ts-reference` branch for reading old behavior.

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

Credential resolution supports runtime API keys, `auth.json`, environment
variables, Claude Code login token fallback, Codex/ChatGPT login token fallback,
and provider-specific environment settings.

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
- Diagnostics for manifest and lifecycle failures.

Intentionally unsupported:

- Running TypeScript/JavaScript extensions in-process.
- Installing npm extension packages.
- Loading npm package-manager metadata as executable code.
- TypeScript UI primitive/custom renderer APIs.

Future Rust extension work should build on the JSON executable protocol with a
Rust SDK, tool registration helpers, or provider registration helpers.

## Validation

Primary validation:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`
- `make check`
- `make e2e`
- `make docker-e2e`

Manual validation:

- `make smoke-claude-opus-oauth`
- `make test-smoke`
- `make ts-parity-fixtures`

`make ts-parity-fixtures` is the only supported path for executing TypeScript
reference code. It runs npm inside Docker only.

## Current Cutover Status

The Rust cutover is complete for the CLI/TUI product path. Remaining gaps should
be tracked as normal GitHub issues and must preserve the Rust-only, no-npm, no-web
scope unless the user explicitly changes that product direction.

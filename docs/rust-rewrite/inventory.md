# Rust Rewrite Inventory

Tracking issue: https://github.com/noeljackson/pi/issues/3

This document records the TypeScript behavior that the Rust rewrite must either preserve or intentionally drop. The reference branch is `ts-reference`; Rust work happens on separate branches.

## Product Scope

Required target:

- Native Rust CLI/TUI application named `pi`.
- No web UI.
- No npm, Node.js, TypeScript, Vite, React, or browser runtime in the final product path.
- Context-safe reload of runtime systems without clearing the active conversation.

Reference packages:

- `packages/coding-agent`: CLI entrypoint, TUI mode, sessions, tools, settings, extensions, auth, slash commands.
- `packages/agent`: generic agent loop, message/session helpers, compaction.
- `packages/ai`: provider registry, model metadata, streaming adapters, OAuth/env auth.
- `packages/tui`: terminal renderer, editor, keybinding engine, selectable lists, markdown rendering.
- `packages/web-ui`: intentionally dropped unless a later issue explicitly restores web functionality.

## CLI Surface

The current executable is `pi` from `packages/coding-agent/src/cli.ts`, with main dispatch in `packages/coding-agent/src/main.ts`.

Primary modes:

- Interactive TUI by default when stdin is a TTY.
- Print mode via `--print`/`-p` or piped stdin.
- JSON output via `--mode json`.
- RPC mode via `--mode rpc`.

Required CLI flags for Rust parity:

- `--help`, `-h`
- `--version`, `-v`
- `--mode text|json|rpc`
- `--print`, `-p`
- `--continue`, `-c`
- `--resume`, `-r`
- `--session <path|id>`
- `--fork <path|id>`
- `--session-dir <dir>`
- `--no-session`
- `--provider <name>`
- `--model <pattern>`
- `--api-key <key>`
- `--models <patterns>`
- `--thinking off|minimal|low|medium|high|xhigh`
- `--system-prompt <text-or-path>`
- `--append-system-prompt <text-or-path>`
- `--no-tools`, `-nt`
- `--no-builtin-tools`, `-nbt`
- `--tools`, `-t`
- `--skill <path>`
- `--no-skills`, `-ns`
- `--prompt-template <path>`
- `--no-prompt-templates`, `-np`
- `--theme <path>`
- `--no-themes`
- `--no-context-files`, `-nc`
- `--export <file>`
- `--list-models [search]`
- `--verbose`
- `--offline`
- `@file` initial-message inputs

Package-management commands currently exist but are npm-centric:

- `install`
- `remove`
- `uninstall`
- `update`
- `list`
- `config`

Rust target:

- Keep `config` as a local resource/configuration TUI if still useful.
- Drop npm/package-manager install/update/list behavior from the Rust-only product path.
- Do not carry extension package installation forward unless a later Rust-native extension story is approved.

## Config, Auth, and Data Paths

Current default config root:

- `~/.pi/agent`

Current environment overrides:

- `PI_CODING_AGENT_DIR`
- `PI_CODING_AGENT_SESSION_DIR`
- `PI_PACKAGE_DIR`
- `PI_OFFLINE`
- `PI_TELEMETRY`
- `PI_SHARE_VIEWER_URL`

Current user files:

- `settings.json`
- `auth.json`
- `models.json`
- `keybindings.json`
- `tools/`
- `prompts/`
- `themes/`
- `sessions/`
- `pi-debug.log`

Current project-local files:

- `.pi/settings.json`
- `.pi/skills`
- `.pi/prompts`
- `.pi/themes`
- `.pi/extensions`
- `AGENTS.md` or `CLAUDE.md` discovered from the global agent dir and cwd ancestors.

Rust target:

- Preserve the config root and key file names initially to reduce user disruption.
- Do not promise TypeScript session/config backward compatibility until the session format is specified in the Rust architecture.
- Preserve `AGENTS.md` discovery because it is core CLI context behavior.
- Keep `auth.json` permissions strict where the platform supports it.

## Settings Surface

Current settings include:

- Default provider/model/thinking level.
- Transport preference.
- Steering and follow-up queue modes.
- Theme.
- Compaction and branch-summary settings.
- Retry settings.
- Thinking block visibility.
- Shell path and shell command prefix.
- Quiet startup.
- Changelog display.
- Install telemetry.
- Package resource paths.
- Extension, skill, prompt, and theme paths.
- Skill command registration.
- Terminal image/progress settings.
- Image resize/block settings.
- Enabled model scope.
- Double-escape behavior.
- Tree filter mode.
- Thinking budgets.
- Editor padding.
- Autocomplete max visible rows.
- Hardware cursor visibility.
- Markdown formatting.
- Warning toggles.
- Custom session directory.

Rust target:

- Preserve settings needed by CLI/TUI, sessions, providers, tools, prompts, skills, keybindings, terminal behavior, image handling if images remain supported, and reload.
- Drop npm-specific package settings unless a Rust-native package system replaces them.
- Treat settings reload as a validated replacement, not partial mutation of active session state.

## Session Behavior

Current session behavior:

- Sessions are JSONL-backed.
- Sessions are associated with a cwd.
- `--continue` resumes the recent cwd session.
- `--resume` opens a selector.
- `--session` accepts a path or UUID prefix.
- `--fork` creates a new session from an existing one.
- Missing cwd for a restored session prompts interactively or fails in non-interactive mode.
- Session tree operations include new, fork, clone, resume, delete, rename, labels, branch navigation, and filters.
- Session state includes conversation messages, tool calls/results, model info, queues, metadata, and extension state.

Rust target:

- Preserve active conversation context, cwd identity, tool history, and queues across reload.
- Preserve fork/resume/new-session workflows before cutover.
- Specify a Rust session schema before implementation.
- Add a migration decision for old JSONL sessions before final cutover.

## Reload Behavior

Current reload command:

- `/reload` reloads keybindings, extensions, skills, prompts, themes, settings, resource loader output, provider registrations, and tool registry.
- Reload is rejected while an assistant response is in progress.
- Reload is rejected while compaction is in progress.
- Extension runtime receives shutdown/start events around reload.
- Current active tool names are preserved and new extension tools are included.
- Keybindings reload separately through `KeybindingsManager.reload()`.

Rust target:

- Keep explicit `/reload`.
- No hot module reload.
- Reload must not clear messages, cwd, session identity, tool call history, queues, active model state, or accumulated context.
- If the reloaded model/provider becomes invalid, report it and require user action instead of clearing the session.

## TUI and Keybindings

Current keybinding model:

- TUI defaults come from `packages/tui`.
- App bindings extend TUI bindings.
- User bindings load from `~/.pi/agent/keybindings.json`.
- Legacy keybinding names migrate to namespaced keys.

App bindings include:

- Interrupt, clear, exit, suspend.
- Cycle/select model.
- Cycle/toggle thinking.
- Expand tool output.
- Toggle session filters and tree filters.
- External editor.
- Queue/dequeue follow-up messages.
- Paste image.
- New/tree/fork/resume session operations.
- Rename/delete session.
- Model scope selector actions.

Rust target:

- Keybindings must remain configurable.
- No hardcoded key checks in feature logic; resolve through a keybinding manager.
- Reload must refresh keybindings without resetting the active session.

## Slash Commands

Built-in slash commands:

- `/settings`
- `/model`
- `/scoped-models`
- `/export`
- `/import`
- `/share`
- `/copy`
- `/name`
- `/session`
- `/changelog`
- `/hotkeys`
- `/fork`
- `/clone`
- `/tree`
- `/login`
- `/logout`
- `/new`
- `/compact`
- `/resume`
- `/reload`
- `/quit`

Rust target:

- Preserve core CLI/TUI slash commands needed for coding-agent operation.
- Defer `/share` if it depends on GitHub/web presentation not yet defined.
- Drop or replace extension-driven slash commands until Rust extension scope is approved.

## Tool Surface

Built-in tools:

- `read`
- `bash`
- `edit`
- `write`
- `grep`
- `find`
- `ls`

Current defaults:

- Coding tool set defaults to `read`, `bash`, `edit`, `write`.
- Read-only tool set includes `read`, `grep`, `find`, `ls`.
- `grep`, `find`, and `ls` are read-only but off by default in the normal coding tool set.
- CLI flags can disable all tools, disable built-in tools, or allowlist tools.

Rust target:

- Preserve tool names and user-facing semantics.
- Preserve dirty-worktree awareness and the rule that unrelated user changes must not be wiped.
- Implement tests for command execution, file reads/writes, edits/patches, search/listing, and git status handling.

## Provider and Model Surface

Current provider/auth scope is broad. Known provider defaults include:

- Amazon Bedrock
- Anthropic
- OpenAI
- Azure OpenAI Responses
- OpenAI Codex
- DeepSeek
- Google
- Google Vertex
- GitHub Copilot
- OpenRouter
- Vercel AI Gateway
- xAI
- Groq
- Cerebras
- ZAI
- Mistral
- MiniMax
- Moonshot
- Hugging Face
- Fireworks
- Together
- OpenCode
- Kimi Coding
- Cloudflare Workers AI
- Cloudflare AI Gateway
- Xiaomi

Current auth sources:

- Runtime `--api-key`.
- Stored `auth.json` API keys.
- Stored OAuth credentials.
- Environment variables.
- Custom provider fallback from model config.

Rust target:

- Implement `faux` first for tests.
- Implement the initial production provider set as OpenAI Responses-compatible, Anthropic, and Google because those cover the main provider shapes in current use.
- Add additional providers issue-by-issue after the streaming event model is stable.
- Provider tests must not require paid APIs or real credentials.

## Intentionally Dropped or Deferred

Dropped for Rust-only cutover:

- Web UI package and browser UI product path.
- npm workspaces, npm scripts, npm publishing, Node runtime, TypeScript build tooling.
- Hot module reload.
- npm package installation/update workflows.

Deferred until explicitly approved:

- TypeScript extension runtime compatibility.
- Browser HTML export polish beyond session data export.
- Full legacy session migration.
- Full provider parity for every current provider.
- Image generation APIs.

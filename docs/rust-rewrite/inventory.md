# Rust Rewrite Inventory

Original tracking issue: https://github.com/noeljackson/pi/issues/3

Current doc refresh: https://github.com/noeljackson/pi/issues/38

This document records the TypeScript reference behavior that the Rust rewrite
preserved, replaced, or intentionally dropped. The reference branch is
`ts-reference`; active implementation is Rust-only.

## Product Scope

Current target:

- Native Rust CLI/TUI application named `pi`.
- No web UI.
- No npm, Node.js, TypeScript, Vite, React, or browser runtime in the product path.
- Normal explicit reload of runtime systems without clearing the active conversation.
- Docker-only execution path for TypeScript reference fixture generation.

Reference packages from `ts-reference`:

- `packages/coding-agent`: CLI entrypoint, TUI mode, sessions, tools, settings, extensions, auth, slash commands.
- `packages/agent`: generic agent loop, message/session helpers, compaction.
- `packages/ai`: provider registry, model metadata, streaming adapters, OAuth/env auth.
- `packages/tui`: terminal renderer, editor, keybinding engine, selectable lists, markdown rendering.
- `packages/web-ui`: intentionally dropped from the Rust product path.

## CLI Surface

The Rust executable is `pi` from `crates/pi-cli`.

Primary modes:

- Interactive TUI by default when stdin is a TTY.
- Print mode via `--print`/`-p` or piped stdin.
- JSON output via `--mode json`.
- JSON-line RPC mode via `--mode rpc`.

Implemented CLI flags include:

- `--help`, `-h`
- `--version`, `-v`
- `--mode text|json|rpc`
- `--print`, `-p`
- `--continue`, `-c`
- `--resume`, `-r`
- `--session <path|id|name>`
- `--fork <path|id|name>`
- `--session-dir <dir>`
- `--no-session`
- `--provider <name>`
- `--model <provider/id-or-pattern>`
- `--api-key <key>`
- `--models <patterns>`
- `--thinking off|minimal|low|medium|high|xhigh|max`
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
- `--image <path>`
- `--export <file>`
- `--list-models [search]`
- `--verbose`
- `--offline`
- `@file` initial-message inputs

Rust package/config commands:

- `pi install <path-or-git-url>`
- `pi remove|uninstall <source-or-name>`
- `pi update [source|self|pi]`
- `pi list`
- `pi config show`
- `pi config disable <extension|skill|prompt|theme> <name>`
- `pi config enable <extension|skill|prompt|theme> <name>`

Package commands are Rust-path only. They do not run npm, install npm packages,
or load a Node runtime. `pi update` fast-forwards local git package sources and
does not self-update the Rust binary.

## Config, Auth, and Data Paths

Default config root:

- `~/.pi/agent`

Environment overrides:

- `PI_CODING_AGENT_DIR`
- `PI_CODING_AGENT_SESSION_DIR`
- `PI_OFFLINE`

Current user files:

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
- `pi-debug.log`

Project-local files:

- `.pi/settings.json`
- `.pi/extensions`
- `.pi/skills`
- `.pi/prompts`
- `.pi/themes`
- `.agents/skills`
- `AGENTS.md` or `CLAUDE.md` discovered from the global agent dir and cwd ancestors.

Rust behavior:

- Preserve the config root and key file names where useful.
- Keep `auth.json` permissions strict where the platform supports it.
- Reuse Claude Code OAuth and Codex/ChatGPT login credentials when available.
- Do not automatically migrate legacy TypeScript sessions.

## Settings Surface

Current settings cover:

- Default provider/model/thinking level.
- Enabled model scopes and active tools.
- Model refresh settings and cache TTL.
- Theme, keybindings, markdown, and terminal behavior.
- Shell path and shell command prefix.
- Thinking block visibility and thinking budgets.
- Terminal image/progress settings.
- Image resize/block settings.
- Prompt, skill, extension, theme, and package resource paths.
- Package resource filters.
- Disabled resources by name or wildcard.
- Custom session directory.

Rust behavior:

- Settings reload is validated before replacing runtime systems.
- Invalid reloads preserve the old runtime systems and active conversation.
- `terminal.showTerminalProgress` controls visible running/completed tool and shell entries.
- `disabledResources` and `pi config enable|disable` filter loaded extensions, skills, prompts, and themes.
- npm-specific package settings are not preserved.

## Session Behavior

Rust sessions:

- Are append-only JSONL-backed.
- Are associated with a cwd.
- Support `--continue`, `--resume`, `--session`, and `--fork`.
- Support new, fork, clone, resume, delete, rename, labels, summaries, branch navigation, and filters.
- Persist active model and thinking level.
- Export/import JSON, JSONL, and local HTML where applicable.

Rust behavior:

- Preserve active conversation context, cwd identity, tool history, queues, active model state, and session tree state across reload.
- Do not automatically read or migrate old TypeScript session logs.
- `/share` writes a local HTML export; web or gist sharing is intentionally unsupported.

## Reload Behavior

`/reload` reloads:

- Settings.
- Auth metadata.
- Model metadata and cached model data.
- Provider availability.
- Keybindings.
- Extensions.
- Skills.
- Prompts.
- Themes.
- Context files.
- Tool definitions.
- Slash command completions for loaded resources.

Reload behavior:

- No hot module reload.
- Reject reload while an assistant response is in progress.
- Preserve messages, cwd, session id/file, tool history, queued messages, branch state, active model, and active thinking level.
- Surface diagnostics for invalid resources instead of clearing context.
- Notify JSON executable extensions with `reload`; notify them with `shutdown` on quit.

## TUI and Keybindings

Rust TUI parity includes:

- Claude Code-style framed layout.
- Streaming assistant output.
- Visible hardware cursor in prompt input.
- Tool and shell progress display.
- Model selector with left/right thinking adjustment.
- Settings selector.
- Session selectors and tree operations.
- Dynamic slash completion.
- Editor history, undo, kill-ring, multiline mode, and external editor handoff.

Keybinding behavior:

- Defaults live in Rust.
- User overrides load from `~/.pi/agent/keybindings.json`.
- Reload refreshes keybindings without resetting `SessionState`.

## Slash Commands

Implemented built-in slash commands include:

- `/help`
- `/settings`
- `/settings show`
- `/status`
- `/diagnostics`
- `/hotkeys`
- `/complete <prefix>`
- `/history`
- `/editor [text]`
- `/image <path> [prompt]`
- `/skills`
- `/skill:<name> [input]`
- `/prompts`
- `/prompt <name> [input]`
- `/themes`
- `/theme <name>`
- `/queue [prompt]`
- `/queue-clear`
- `/interrupt`
- `/models`
- `/scoped-models`
- `/selector <kind>`
- `/select <kind> <query>`
- `/model <provider/id>`
- `/thinking <level>`
- `/multiline`
- `/session`
- `/changelog`
- `/new`
- `/resume [id|name|path]`
- `/fork [id|name|path]`
- `/clone [id|name|path]`
- `/tree`
- `/summaries`
- `/delete [id|name|path]`
- `/name <name>`
- `/labels <labels...>`
- `/export <file>`
- `/import <file>`
- `/copy`
- `/share [file]`
- `/compact`
- `/login [provider]`
- `/logout <provider>`
- `/reload`
- `/read <path>`
- `/write <path> <text>`
- `/edit <path> <find> <replace>`
- `/grep <text> [path]`
- `/find <text>`
- `/ls [path]`
- `/bash <command>`
- `! <command>`
- `!!`
- `/quit`

Loaded resources add dynamic commands:

- `/extension:<name>`
- `/skill:<name>`
- `/prompt <name>`
- `/theme <name>`

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
- Read-only tools include `grep`, `find`, and `ls`.
- CLI flags can disable all tools, disable built-in tools, or allowlist tools.

Rust behavior:

- Preserve tool names and user-facing semantics.
- Preserve dirty-worktree awareness and avoid wiping unrelated user changes.
- Execute shell commands with configured shell settings.
- Show visible TUI progress when terminal progress is enabled.

## Provider and Model Surface

Implemented provider/auth scope includes:

- Faux test provider.
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

Current auth sources:

- Runtime `--api-key`.
- Stored `auth.json` API keys.
- Stored OAuth credentials.
- Environment variables.
- Claude Code login token fallback.
- Codex/ChatGPT login token fallback.
- Custom provider fallback from model config.

Model refresh:

- Uses built-in models, cached models, and explicit `models.json` immediately.
- Starts a non-blocking refresh when enabled, online, stale, and authenticated.
- Surfaces refreshed data after `/reload` or the next startup.

Provider tests do not require paid APIs or real credentials. Real-provider smoke
tests are manual and excluded from `make check`, `make e2e`, and `make docker-e2e`.

## Extension and Package Resource Surface

Rust supports:

- Local package sources by path.
- Git package sources.
- Package resource discovery by convention.
- Package resource discovery through `package.json` under the `pi` key.
- Resource include/exclude filters.
- `.gitignore`, `.ignore`, and `.fdignore` handling in resource directories.
- Resource enable/disable state.
- Raw executable extensions.
- JSON stdio executable extensions.
- JSON extension lifecycle notifications for reload and shutdown.

Rust intentionally does not support:

- Embedded TypeScript or JavaScript extension execution.
- npm extension installation/update workflows.
- In-process TypeScript UI primitive/custom renderer APIs.

Future Rust extension work can build on the JSON executable protocol with a
Rust SDK, tool registration helpers, or provider registration helpers.

## Validation

Primary validation:

- `make check`
- `make e2e`
- `make docker-e2e`

Additional manual validation:

- `make smoke-claude-opus-oauth`
- `make test-smoke`
- `make ts-parity-fixtures`

`make ts-parity-fixtures` is the only supported TypeScript execution path and
runs npm inside Docker only.

## Intentionally Dropped or Deferred

Dropped for Rust-only product path:

- Web UI package and browser UI product path.
- npm workspaces, npm scripts, npm publishing, Node runtime, TypeScript build tooling.
- Hot module reload.
- npm package installation/update workflows.
- Embedded TypeScript/JavaScript extension runtime compatibility.

Deferred until explicitly approved:

- Browser HTML export polish beyond local session export.
- Automatic legacy TypeScript session migration.
- Image generation APIs.

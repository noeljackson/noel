# Rust Rewrite Inventory

Original tracking issue: https://github.com/noeljackson/pi/issues/3

Current doc refresh: https://github.com/noeljackson/pi/issues/38

This document records the TypeScript reference behavior that the Rust rewrite
preserved, replaced, or intentionally dropped. The TypeScript reference is the
tip of upstream `main` from `https://github.com/earendil-works/pi`; active
implementation is Rust-only.

## Product Scope

Current target:

- Native Rust CLI/TUI application named `pi`.
- No web UI.
- No npm, Node.js, TypeScript, Vite, React, or browser runtime in the product path.
- Normal explicit reload of runtime systems without clearing the active conversation.
- Docker-only execution path for TypeScript reference fixture generation.

Reference packages from the checked-out upstream TypeScript repository:

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
- Upstream's RPC protocol via `--mode rpc` (stdin not a TTY), with the
  `run-id`, `expected-run-id`, `request-id`, and `resume-run` extensions.

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
- `--no-builtin-tools`, `-nbt` (extension tools stay enabled)
- `--tools`, `-t` (names and `*` patterns, or only `+name`/`-name` entries)
- `--exclude-tools`, `-xt` (names and `*` patterns)
- `--skill <path>` (a file or a directory; repeatable)
- `--no-skills`, `-ns` (no discovery; `--skill` paths still load)
- `--prompt-template <path>` (a file or a directory; repeatable)
- `--no-prompt-templates`, `-np` (no discovery; `--prompt-template` paths
  still load)
- `--theme <path>`
- `--no-themes`
- `--no-context-files`, `-nc`
- `--image <path>`
- `images [search]`
- `generate-image --output <file> [--model <provider/model>] [--image <file>] <prompt>`
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
- `usage-cache.json`
- `keybindings.json`
- `extensions/`
- `skills/`
- `prompts/`
- `themes/`
- `sessions/`
- `pi-debug.log`

Project-local files:

- `.pi/settings.json`
- `.pi/SYSTEM.md` and `.pi/APPEND_SYSTEM.md`, which replace the agent dir's
  `SYSTEM.md` and `APPEND_SYSTEM.md`
- `.pi/extensions`
- `.pi/skills`
- `.pi/prompts`
- `.pi/themes`
- `.agents/skills`, in each directory from the cwd up to the repository root
- `AGENTS.override.md`, `AGENTS.md`, or `CLAUDE.md` (one per directory, in
  that order) discovered from the global agent dir and cwd ancestors,
  whatever the project trust decision; trust gates only the `.pi` resources
  and `.agents/skills` above. A linked worktree nested in its main checkout
  replaces the main checkout's same-named file.

Outside both, `~/.agents/skills` holds user skills, as upstream reads it.

Rust behavior:

- Preserve the config root and key file names where useful.
- Write `auth.json` atomically and owner-only (0600; a missing agent directory
  is created 0700). Logins, logouts, and OAuth refreshes take a cross-process
  lock (`auth.json.flock`), re-read the file, and change only their own entry;
  a refresh continues from the stored refresh token, not the startup snapshot.
- Keep `--api-key` values in memory; they are never written to `auth.json`.
- Reuse Claude Code OAuth and Codex/ChatGPT login credentials when available.
- Do not automatically migrate legacy TypeScript sessions.

## Settings Surface

Current settings cover:

- Default provider/model/thinking level.
- Enabled model scopes.
- Tool selection: `defaultTools` (upstream's setting, merged across the user
  and project layers as upstream merges it) and `enabledTools` (a Rust-only
  allowlist).
- Model refresh settings and cache TTL.
- Theme, keybindings, markdown, and terminal behavior.
- Shell path and shell command prefix.
- Custom and appended system prompt text (`systemPrompt`,
  `appendSystemPrompt`; Rust-only settings).
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

## Run Recovery

Rust behavior, in the TUI, print mode, and RPC mode:

- A failed model request is retried only when a retry can fix it, by
  upstream's rules (`error-classification.json`): transport failures, HTTP
  408, 429, 500, 502, 503, 504, 520, and 524, an overloaded or rate-limiting
  provider. Authentication and request errors, a missing credential, and
  quota or billing limits end the turn at once. `retry.enabled`, `retry.maxRetries`, `retry.baseDelayMs`, and
  `retry.maxAgentDelayMs` apply; `retry.provider.*` is read and not used.
- A reply that breaks off mid-stream is retried too, and left out of the
  conversation.
- A context overflow, or a final reply cut off at the output limit, is
  answered by one compaction and one more request.
- RPC mode reports all of it with upstream's records (`agent_end.willRetry`,
  `auto_retry_start`/`auto_retry_end`, `compaction_start`/`compaction_end`
  with reason `overflow`) and takes `abort_retry` and `set_auto_retry`.

## Session Behavior

Rust sessions:

- Are append-only JSONL-backed for live Rust sessions.
- Are associated with a cwd.
- Support `--continue`, `--resume`, `--session`, and `--fork`.
- Support new, fork, clone, resume, delete, rename, labels, summaries, branch navigation, and filters.
- Persist active model and thinking level.
- Open/export/import JSON, TypeScript v3-shaped JSONL, and local HTML where applicable.

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
- Keep the session's project trust decision and re-apply the command-line
  overrides (`--system-prompt`, `--append-system-prompt`, `--no-context-files`,
  `-e`, `--no-extensions`, `--tools`, `--api-key`, `--skill`,
  `--prompt-template`, ...); the same holds for `/models refresh` and
  `/models reset`. RPC mode has no reload: it keeps the config startup built.
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
- `/image-models [search]`
- `/generate-image <output> <prompt>`
- `/skills`
- `/skill:<name> [arguments]`
- `/prompts`
- `/<template> [arguments]`
- `/prompt <name> [input]` (Rust-only)
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
- `/delete <id|name|path>` (bare `/delete` shows usage; it never deletes)
- `/name [name]` (bare `/name` shows the current name)
- `/labels <labels...>`
- `/export [file]` (default `pi-session-<session id>.html` in the session cwd)
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
- `/grep <pattern> [path]`
- `/find <glob>`
- `/ls [path]`
- `/bash <command>`
- `! <command>`
- `!!`
- `/quit`

Loaded resources add dynamic commands:

- `/extension:<name>`
- `/skill:<name>`
- `/<template>`, and `/prompt <template>`
- `/theme <name>`

A line that is none of these, nor a slash-tool prompt, skill command, or
template, is sent to the model as text, as upstream sends it.

## System Prompt

Upstream sends every request a system prompt built by
`core/system-prompt.ts`: the role, the active tools with a one-line snippet
each, rules contributed by the tools, pointers to its documentation, appended
text, project context files, the skills listing, and the working directory.

Rust behavior:

- Same builder, same text, same section order, in every mode; compared with
  upstream's output in `system-prompt.json`.
- `--system-prompt`, the `systemPrompt` setting, or `SYSTEM.md` replace the
  role, tool list, and rules. `--append-system-prompt`, the
  `appendSystemPrompt` setting, or `APPEND_SYSTEM.md` add the `addendum`.
- The Rust-only tools (`todo`, `web_search`, `web_fetch`) have snippets in
  upstream's style. JSON extension tools supply `promptSnippet` and
  `promptGuidelines` in their manifest.
- The model's `bash` commands get `PI_SESSION_ID`, `PI_SESSION_FILE`,
  `PI_PROVIDER`, `PI_MODEL`, and `PI_REASONING_LEVEL`, and the prompt's bash
  rule says so. The process carries `AI_AGENT` and `PI_CODING_AGENT`.
- The skills listing is upstream's `formatSkillsForPrompt` text, present
  while `read` or `bash` is active and without the skills that set
  `disable-model-invocation`.
- Not ported: the documentation section, by decision (see "Default System
  Prompt" in the non-parity register).

## Skills and Prompt Templates

Upstream loads skills (`core/skills.ts`) and prompt templates
(`core/prompt-templates.ts`) through its resource loader and package manager,
lists skills in the system prompt, and expands `/skill:name` and `/template`
input in its session (`core/agent-session.ts`).

Rust behavior, compared with upstream's results in `skills.json`:

- Discovery: `skills/` in the agent dir and a trusted project's `.pi/skills`
  (every `SKILL.md` below, and described Markdown files directly in them);
  `~/.agents/skills` and a trusted project's `.agents/skills` from the cwd up
  to the repository root; the `skills` and `prompts` settings; packages;
  `--skill` and `--prompt-template`, each a file or a directory. Templates
  are the Markdown files directly in a `prompts/` directory.
- Order: project setting, project directories, user setting, user
  directories, packages, command line. The first resource of a name wins;
  the others are reported in `/diagnostics`.
- `--no-skills` and `--no-prompt-templates` turn discovery off and keep the
  command line's paths.
- Frontmatter: `name`, a required `description`, and
  `disable-model-invocation` for skills; `description` and `argument-hint`
  for templates. A `SKILL.md` without a description is not loaded. Names and
  descriptions outside the Agent Skills rules are reported and still load.
- `/skill:name arguments`: the skill's file without frontmatter in
  upstream's `<skill>` block, then the arguments.
- `/name arguments`: the template's text with `$1`, `$@`, `$ARGUMENTS`,
  `${1:-default}`, `${@:N}`, and `${@:N:L}` substituted; quotes keep spaces
  in an argument.
- Expansion applies to prompts in the TUI, print mode, and RPC mode, and to
  steering and follow-ups in the TUI and RPC mode (`pi_core::expand_input`).
  A command that names no skill or template is text for the model.
- RPC `get_commands` lists templates and skills with `source` and
  `sourceInfo`.
- Rust-only: `/skills`, `/prompts`, and `/prompt <name> [input]` with its
  `{{input}}` placeholder; slash-tool prompts come before templates.
- Differences are listed under "Skills and Prompt Templates" in the
  non-parity register.

## Tool Surface

Built-in tools:

- `read`
- `bash`
- `edit`
- `write`
- `grep`
- `find`
- `ls`
- `todo`, `web_search`, `web_fetch` (Rust additions; upstream has none of them)

Upstream also has `powershell`, which Rust does not port (see the non-parity
register).

Defaults:

- Upstream enables `read`, `bash`, `edit`, and `write` at startup; `grep`,
  `find`, and `ls` are available but off until `--tools` or `defaultTools`
  names them.
- Rust enables all ten of its built-in tools at startup. This is a recorded
  difference ("All Built-in Tools Enabled by Default" in the non-parity
  register), not a parity claim.
- `defaultTools`, `--tools` (allowlist with `*` patterns, or `+name`/`-name`
  modifiers), `--exclude-tools`, `--no-builtin-tools`, and `--no-tools` select
  tools as upstream's do, starting from Rust's default set.

Rust behavior:

- `read`, `bash`, `edit`, `write`, `grep`, `find`, and `ls` return what
  upstream's tools return for the calls recorded in `tool-behavior.json`
  (`crates/pi-core/src/tool_parity_tests.rs` compares them exactly), and send
  the model upstream's descriptions and parameter schemas.
- Paths resolve as upstream's do: relative to the working directory, absolute,
  or under `~/`, with a leading `@` dropped. They are not confined to the
  working directory.
- `read`: 2000 lines or 50 KB, whichever comes first, with a continuation
  notice; an offset past the end is an error; images are returned as
  attachments where the provider API carries them (README, "File and shell
  tools").
- `bash`: `bash -c` (`/bin/bash`, then `bash` on `PATH`, then `sh`; `shellPath`
  and `shellCommandPrefix` apply), stdout and stderr as one stream, the last
  2000 lines or 50 KB with the whole output saved to a temp file, no default
  timeout, and a timeout that returns the output so far. A command the model
  called reports its output while it runs (the TUI's running entry, RPC
  `tool_execution_update`).
- `grep` and `find` walk the tree in-process with ripgrep's `ignore` and
  `globset` crates instead of running `rg` and `fd`.
- `edit`: each `oldText` must occur exactly once in the original file and the
  edits must not overlap; all edits apply in memory and the file is written
  once, or nothing changes. An empty `newText` deletes; `edits` may arrive as a
  JSON string. Edits follow the file's CRLF line endings and keep a BOM. No
  fuzzy matching (upstream normalizes quotes, dashes, and trailing spaces when
  an exact match fails).
- `write` accepts empty `content` and creates parent directories.
- `!` and `/bash` run through the same shell path with a 120 second timeout,
  and show their output when they end.
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

Startup model selection (upstream `findInitialModel`):

- `--model`/`--provider`, then the resumed session's model, then the saved
  default if its provider has credentials, then a model with credentials
  (upstream's per-provider default model first), and `faux` only when it is
  requested or nothing else can run. Offline, only `faux` can run.

Request limits:

- Anthropic Messages requests without an explicit cap send the Claude model's
  output limit, lowered to fit the context window; other models on the
  Anthropic API default to 16384 tokens.
- Tool calls in a reply cut off by the output-token limit (`max_tokens`,
  `length`) are not run; each returns an error asking the model to re-issue
  it with complete arguments.
- Provider requests fail after 5 minutes without response headers or without
  new body or stream data (upstream's default HTTP idle timeout, not
  configurable here). The timed-out request error is retried like other
  network errors when nothing was streamed yet.

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
- JSON executable extension model tools. An executable extension can declare
  `protocol: "json"` and a `tools` array in its adjacent
  `.pi-extension.json`; those tools are exposed to providers and receive
  `kind: "tool"` JSON requests on stdin. A non-zero exit is an error result,
  output is capped at 2000 lines or 50 KB with the whole output saved to a
  temp file, and a tool named like a built-in tool replaces it.

Rust intentionally does not support:

- Embedded TypeScript or JavaScript extension execution.
- npm extension installation/update workflows.
- In-process TypeScript UI primitive/custom renderer APIs.

Future Rust extension work can build on the JSON executable protocol with a
Rust SDK or provider registration helpers.

## Validation

Primary validation:

- `make check`
- `make e2e`
- `make docker-e2e`
- `make parity-check`

Additional manual validation:

- `make smoke-claude-opus-oauth`
- `make test-smoke`
- `make ts-parity-fixtures`
- `make ts-parity-update`
- `make ts-parity-drift`
- `make ts-parity-agent`

`make parity-check` runs the committed fixture inventory checks, Rust
fixture-backed assertions, and Docker drift detection. `make
ts-parity-fixtures` is the only supported TypeScript execution path and
runs npm inside Docker only. It defaults to the upstream TypeScript repository
`https://github.com/earendil-works/pi` on `main`. `make ts-parity-update`
refreshes fixtures from the moving TypeScript reference. `make ts-parity-drift` writes
`target/ts-parity-drift/brief.md` and fails on fixture drift. `make
ts-parity-agent` can dispatch that brief through `PI_PARITY_AGENT_COMMAND`,
including a Codewire command such as `cw exec --name ts-parity-agent -- claude`.

Parity fixtures are sanitized upstream provider request captures. They document
URL, method, selected headers, body, and payload shape for covered providers.
Rust tests compare local provider request builders against those captures.
`rpc-protocol.json` records upstream RPC mode instead: every stdout record, in
order, for scripted runs against a local stub model; `pi-cli`'s `rpc_parity`
test compares pi's RPC mode with it record by record. `skills.json` records
what upstream loads, lists, and expands for skills and prompt templates;
`pi-config`, `pi-core`, and the `rpc_parity` test compare with it.
`error-classification.json` records how upstream classifies failed requests
(retryable, context overflow); `pi-ai` is compared with it. Drift
is handled by updating Rust behavior and committed fixtures together, then
running `make parity-check` and `make check`.

The `pi-parity` crate keeps the fixture inventory, source metadata,
script-output mapping, redaction checks, and parity documentation coverage in
sync.

This harness tracks provider request-shape parity. It does not replace Rust unit
coverage, tmux TTY e2e, or manual live-provider smoke tests.

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

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
  - System prompt.
  - Session state.
  - Conversation orchestration.
  - Session persistence and export/import.
  - Runtime reload state.
- `crates/pi-config`
  - Config paths.
  - Settings loading.
  - Auth loading.
  - Keybindings config.
  - Context file and prompt file discovery.
  - Resource/package discovery.
  - Model definitions: the catalog, the model cache, and `models.json`.
  - Model resolution and scoping (`model_resolver.rs`).
- `crates/pi-ai`
  - Provider request/response normalization.
  - The built-in model catalog and the thinking-level rules (`catalog.rs`).
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
- Custom and appended system prompt text.
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
  (`agent_start`, its turns, `agent_end`, and one `agent_settled`) however
  many engine calls it takes; the recorder closes whatever an abort or a
  failure left open, ending the run's last turn with an aborted or error
  reply. When the engine drops a reply to ask again (see Run Recovery), the
  recorder ends that reply's turn with `agent_end` and starts over with
  another `agent_start` under the same run id, as upstream does.
- `rpc.rs` holds the mode: a reader thread that splits stdin on LF only, one
  writer that prints each record as one line, and the command loop. While a
  run is active one task polls the engine call and handles commands that
  arrive meanwhile (`tokio::select!`), so records keep the order things
  happen in. The engine reports events through a callback; the command loop
  and the callback share the queue and the active run behind a lock that is
  never contended.

Each run builds its provider at the prompt (so `set_model` applies to the next
run) and gets a `RunControl`: the shared steering mailbox, its own
cancellation token, which `abort` cancels, and the handle `abort_retry` ends
a backoff with. Follow-ups need no engine support:
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

## Run Recovery

The run loop (`run_model_turn_streaming_events` in `pi-core`) asks for one
reply per round. What follows a failed request is decided in one place,
`Recovery`, the counterpart of upstream's `_handlePostAgentRun`:

- `pi-ai` classifies the failure: `ProviderError::is_retryable` and
  `is_context_overflow` apply upstream's patterns (kept verbatim, and checked
  against `error-classification.json`) to what the provider answered, and
  treat a transport failure as retryable by its kind.
- A retryable failure waits out a backoff and asks again; a context overflow
  compacts first, once (`Runtime::compact_after_overflow`, the same
  compaction the threshold uses, without the threshold). A final reply cut
  off at the output limit below the model's own limit
  (`Provider::output_limit`) is handled as an overflow. Any other failure
  ends the turn.
- Asking again is another round of the same loop: `RetryStarted` or
  `Overflowed` ends the round of the dropped reply, and `RoundStarted` opens
  the next. Steering queued meanwhile joins it. A dropped round does not
  count toward `agent.maxToolRounds`.

A reply is saved only when it stands, after its request succeeded. So a
dropped reply is never in the session or its journal: the request that asks
again is built from the same saved messages, and there is nothing to remove.
The only journal write of a recovery is the compaction checkpoint, which is
atomic. A pi killed during a backoff or a compaction resumes from the saved
prompt and recovers again; killed after the checkpoint, it resumes with the
compacted context.

A turn's cancellation ends a backoff or an overflow compaction at once, like
any other step. `RunControl::retry` ends a backoff only, and the turn then
fails with the error it would have retried.

`pi-agent` keeps a self-contained retry (`complete_with_retry`) with the same
classification for requests that are not a turn's reply: a compaction's
summary.

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
- `.agents/skills`, in each directory from the cwd up to the repository root

User skills are also read from `~/.agents/skills`.

Context discovery, whatever the project trust decision:

- One context file per directory: `AGENTS.override.md`, else `AGENTS.md`,
  else `CLAUDE.md`.
- Load the agent dir's, then the cwd ancestors' in root-to-leaf order.
- In a linked worktree nested inside its main checkout, skip the main
  checkout's file that the worktree's own file of the same name replaces.

Prompt files:

- `SYSTEM.md` and `APPEND_SYSTEM.md`: the trusted project's `.pi/` copy, else
  the agent dir's. The `systemPrompt`/`appendSystemPrompt` settings and the
  `--system-prompt`/`--append-system-prompt` flags come before the files.

Validation rules:

- Invalid settings do not replace active settings.
- Invalid keybindings do not replace active keybindings.
- Invalid resources produce diagnostics.
- Missing optional resource paths produce diagnostics.
- Missing required session cwd blocks session resume outside interactive recovery.

## System Prompt

`crates/pi-core/src/system_prompt.rs` builds the prompt every model request
starts with. It is a port of upstream's `buildSystemPrompt`: a pure function
from the active tools, the custom and appended prompt text, the context
files, the skills, and the working directory to the text, with no runtime
behind it.
`system-prompt.json` holds upstream's output and the module's tests compare
against it to the byte.

The prompt is a one-sentence role followed by tagged sections joined by blank
lines: `tools`, `rules`, `addendum`, `project_context`, `skills`, and `cwd`. A
custom prompt replaces the role, `tools`, and `rules`; the rest still follow
it. `skills` is the listing of the loaded skills
(`pi_config::format_skills_for_prompt`), present while `read` or `bash` is
active.

The runtime builds the prompt once per turn (`runtime_system_prompt`) from
what the session and the reloadable systems hold at that moment:

- The tools are the ones the request declares (`active_tool_definitions`), so
  the tool list and rules always match the request. A builtin tool's line and
  rules come from `PromptTool::builtin`; an extension tool's come from its
  manifest's `promptSnippet` and `promptGuidelines`.
- The custom prompt, appended text, context files, and skills come from the
  loaded config, so `/reload` picks up edited files.
- Session `System` messages follow the prompt in the same request field.

The context estimate counts the same text, so automatic compaction and the
output cap include the prompt.

`PRODUCT_NAME` is the one place the product's name is written for the prompt
and for the `AI_AGENT` process marker.

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

## Model Catalog and Selection

The built-in catalog is data, not code. `scripts/ts-parity/model-catalog-fixture.ts`
runs upstream's model runtime in Docker and writes every provider and chat
model it knows, with every field, to
`tests/fixtures/ts-parity/model-catalog.json`. `pi-ai`'s `catalog.rs` compiles
that file in (`include_str!`) and parses it once, on first use. The file is
both the parity fixture and the product's data, so `make ts-parity-update`
(and the drift job's pull request) updates the models pi knows, and the drift
check reports a catalog that has moved. The providers only this port has
(`faux`, `zai-coding`, `zai-anthropic`, `kimi-coding-openai`) are in
`crates/pi-ai/src/catalog_port_models.json`, written by hand in the same
shape and loaded first, so `faux/echo` stays the first model. Upstream models
on an API pi has no adapter for are skipped at load.

A catalog entry keeps the fields pi reads: provider, id, name, API, base URL,
`reasoning`, `thinkingLevelMap`, `input`, `cost`, `contextWindow`, and
`maxTokens`. `pi-config`'s `ModelDefinition` has the same fields under
upstream's names, all optional past the endpoint, so a built-in model, a
cached provider listing, and a `models.json` entry describe a model the same
way. `load_config` merges the three (`merge_model_definitions`): the catalog
is the base, a listed id the catalog lacks is built on its provider's default
model, and a `models.json` entry is used as written and takes what it leaves
out from the catalog. `LoadedConfig.models` is every known model;
`LoadedConfig.model_scope` is what `--models` or `enabledModels` selects of
them, resolved by the CLI because it depends on credentials.

Resolution is a port of upstream's `core/model-resolver.ts` in
`pi-config/src/model_resolver.rs`: pure functions over a list of models in
catalog order and a predicate for which providers have credentials
(`resolve_cli_model`, `resolve_model_scope`,
`find_exact_model_reference_match`, `find_initial_model`, `custom_model`),
with upstream's collation order and glob matching ported for ASCII ids.
`pi-cli`'s `model_select.rs` applies them to a session: startup selection
(`resolve_cli_selection`, `select_initial_model`), model switches
(`switch_model`), and thinking-level changes (`set_thinking_level`), used
alike by the CLI, the TUI commands, and RPC mode. The session stores a
`ModelRef`; requests look the definition up (`model_definition`) each time.

## Thinking Architecture

A thinking level is one of upstream's seven names, `off` to `max`. Which of
them a model supports follows from its definition (`reasoning` and
`thinkingLevelMap`) through two functions ported from upstream's
`ai/src/models.ts`, `supported_thinking_levels` and `clamp_thinking_level` in
`pi-ai`'s `catalog.rs`; there are no per-provider rules.

The session stores a level only once something sets one. The level in effect
(`effective_thinking_level`) is the session's, else the model's entry in
`modelThinkingLevels`, else `defaultThinkingLevel`, else `medium`, clamped to
the model; `get_state`, the status line, and the bash tool's
`PI_REASONING_LEVEL` report it. Setting a level stores the clamped one, and a
model switch carries the level in effect over unless the settings give the
new model one.

A request gets the level in effect in `ProviderConfig.thinking_level` (`None`
for a model without reasoning) and the model's `ModelTraits`: its thinking
level map, output cap, and context window. Request builders clamp again, as
upstream's do, send the level under the name the map gives it, and send the
output cap lowered to fit the context window. How a level is encoded stays
each builder's own: a reasoning effort for the Responses APIs and OpenRouter,
adaptive or budget thinking for Anthropic and Bedrock, a budget for Google.

## Tool Architecture

Tool definitions live in `pi-tools`. The tools a run may use are computed
from the tool settings and command-line flags whenever the reloadable systems
are built; the session does not record a selection.

Built-ins:

- `read`
- `bash`
- `edit`
- `write`
- `grep`
- `find`
- `ls`

Rules:

- Tools receive an explicit cwd. Paths resolve against it but, as in
  upstream, are not confined to it.
- File mutation tools must protect unrelated user changes.
- Search/list tools are read-only.
- Tool tests do not call providers.
- Shell execution supports configured shell path and command prefix.
- A `bash` call from the model runs with the session's variables
  (`PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`,
  `PI_REASONING_LEVEL`), set per call by `pi-core` through
  `ToolRuntimeOptions::bash_env`; a variable with nothing to report is
  removed from the inherited environment. The reasoning level is the turn's
  provider's thinking level (`Provider::thinking_level`). Commands the user
  types get none.
- `pi-cli` sets the process markers `AI_AGENT` and `PI_CODING_AGENT` before
  starting any thread, so every child process inherits them.
- A `bash` call reports its output while it runs: `pi-tools`
  (`execute_tool_streaming`) passes what the command has printed so far,
  within the limits of the result, to a callback, at most every 100 ms;
  `pi-core` reports it as `TurnEvent::ToolUpdate`, which the TUI shows in the
  running tool's entry and RPC mode sends as `tool_execution_update`. The
  result is unchanged.
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

## Skills and Prompt Templates

Skills and prompt templates are ports of upstream's modules, kept file for
file so each can be read against its source:

- `crates/pi-config/src/frontmatter.rs` (`utils/frontmatter.ts`): the
  frontmatter and body of a Markdown file. Upstream uses a YAML library;
  this is a parser for the YAML the Agent Skills specification uses.
- `crates/pi-config/src/resources.rs` (the skill and prompt parts of
  `core/package-manager.ts` and `core/resource-loader.ts`): which files to
  load, in the order that decides a name collision: project setting, project
  directories, user setting, user directories, packages, then the command
  line's paths. `--no-skills` and `--no-prompt-templates` keep only the last.
- `crates/pi-config/src/skills.rs` (`core/skills.ts`): a skill from its file,
  the Agent Skills name and description rules, first-wins collisions, and the
  system prompt's listing.
- `crates/pi-config/src/prompt_templates.rs` (`core/prompt-templates.ts`): a
  template from its file, argument splitting, and placeholder substitution.
- `crates/pi-core/src/input.rs` (the session's expansion in
  `core/agent-session.ts`): `expand_input`, which turns `/skill:name` and
  `/template` input into the prompt it stands for.

`LoadedConfig` holds the loaded skills and templates, so `/reload` replaces
them with the rest of the configuration. Their diagnostics join the
configuration's. `ReloadableSystems` copies the skills for the system prompt.

`expand_input` is applied where input enters a mode, as upstream's session
applies it in `prompt`, `steer`, and `followUp`: the TUI when a line is
submitted, queued, or sent as steering; print mode for its prompt; RPC mode
for `prompt`, `steer`, and `follow_up`. The engine takes the text it is
given, so a queued message is expanded once, when it is queued, and the queue
and the session hold the expansion. Rust's slash-tool prompts are checked
first: `/read notes.txt` stays a tool command whatever templates exist. Text
that is none of these goes to the model.

`skills.json` holds upstream's results for each step (what loads from a
tree, the listing, the expansions, the argument table, and `get_commands`),
and each module's tests compare with it exactly.

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
  command responses. A tool's optional `promptSnippet` and `promptGuidelines`
  become its line and rules in the system prompt.
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
each scenario against `faux/script` and compares the records.
`error-classification.json` holds the patterns upstream classifies a failed
request with and its verdicts for sample errors; `pi-ai` keeps the same
patterns and is checked against both. When upstream
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

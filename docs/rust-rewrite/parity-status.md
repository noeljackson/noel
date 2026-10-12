# Rust Rewrite Parity Status

The Rust implementation uses the tip of upstream TypeScript `main` from
`https://github.com/earendil-works/pi` as a behavioral oracle. Reference code
must run only inside Docker through `make ts-parity-fixtures`,
`make ts-parity-update`, or `make ts-parity-drift`.

Parity is enforced in three layers:

- Docker-generated TypeScript fixtures in `tests/fixtures/ts-parity/`.
- Rust behavior tests in the owning crates.
- Inventory and documentation checks in `crates/pi-parity`.

`make check` runs the Rust tests, including `pi-parity`. `make
ts-parity-drift` regenerates TypeScript fixtures in Docker, compares them to the
committed fixtures, then runs the fixture-backed Rust parity assertions.

## Fixture Inventory

The committed TypeScript parity fixtures are:

- `agent-tool-loop.json`: upstream agent loop shape for model-callable tools,
  tool-result messages, second model turn, and event sequence.
- `anthropic-api-key.json`: Anthropic Messages request shape with API-key auth.
- `anthropic-claude-code-oauth.json`: Anthropic-compatible Claude Code OAuth
  request shape and static headers.
- `anthropic-tools.json`: Anthropic Messages request shape with model-callable
  tools.
- `auth-discovery.json`: auth precedence, env credential discovery, local login
  file interop, and redaction behavior.
- `bedrock-claude-opus-4.6.json`: Bedrock Claude Converse payload shape.
- `bedrock-tools.json`: Bedrock Claude Converse payload shape with
  model-callable tools.
- `cli-contract.json`: upstream CLI option and command contract.
- `cloudflare-ai-gateway-kimi.json`: Cloudflare AI Gateway OpenAI-compatible
  request shape.
- `error-classification.json`: how upstream reads a failed model request from
  its error text: the pattern lists of `isRetryableAssistantError` and
  `isContextOverflow`, and both verdicts for 124 error messages (HTTP
  statuses and bodies as pi words them, the overflow errors of each provider,
  transport failures, and quota and billing limits).
- `github-copilot-gpt-5.4.json`: GitHub Copilot Responses request shape.
- `google-gemini-2.5-pro.json`: Gemini request shape and thinking config.
- `google-tool-image-routing.json`: Gemini 2 versus Gemini 3 tool-result
  image routing.
- `google-tools.json`: Gemini request shape with model-callable tools.
- `local-tools.json`: built-in local tool inventory (names, labels, prompt
  snippets, parameter names). Rust checks only that every upstream tool is
  present; `tool-behavior.json` covers what the tools do.
- `mistral-devstral.json`: Mistral chat-completions request shape.
- `mistral-tool-id.json`: Mistral tool-history request shape and 9-character
  tool-call ID normalization.
- `mistral-tools.json`: Mistral chat-completions request shape with
  model-callable tools.
- `model-catalog.json`: upstream's whole built-in chat model catalog as its
  coding agent sees it with no credentials and no network: every provider in
  upstream's order, every model with every field, and the model each provider
  starts on (`defaultModelPerProvider`). This file is also data: `pi-ai`
  compiles it in as pi's built-in catalog, so regenerating it changes the
  models pi knows. Upstream generates the catalog from the providers' listings
  when the reference image is built, so the file is a snapshot of that moment
  and drifts with the listings, not only with upstream's source.
- `model-resolution.json`: how upstream picks a model, from its own functions
  on its built-in catalog: `--model`/`--provider`/`--thinking` resolution
  (`resolveCliModel`), scope patterns (`resolveModelScopeFromModels`), exact
  references (`findExactModelReferenceMatch`), the startup default
  (`findInitialModel`), and the glob matcher behind scope patterns
  (`minimatch`), each with its inputs and upstream's answer or error text.
  Its `launches` start upstream `pi --mode rpc` with model and thinking flags,
  settings, and fake provider keys, and record `get_state`, `set_model`, and
  `set_thinking_level` answers, the `thinking_level_changed` events, stderr,
  and the exit code.
- `model-thinking.json`: what upstream's `getSupportedThinkingLevels` and
  `clampThinkingLevel` answer for every shape of model in its built-in
  catalog (one case per distinct `reasoning` flag and `thinkingLevelMap`),
  with the level names and the default level.
- `openai-codex-chatgpt-oauth.json`: Codex Responses request shape using
  ChatGPT OAuth credentials.
- `openai-responses-tool-id.json`: OpenAI Responses pipe-separated tool-call
  ID normalization for cross-provider handoff.
- `openai-responses-tools.json`: OpenAI Responses request shape with function
  tools.
- `openrouter-kimi.json`: OpenRouter OpenAI-compatible request shape.
- `rpc-protocol.json`: upstream `pi --mode rpc` stdout records, in order, for
  scripted runs against a local OpenAI-compatible stub: a text prompt, a tool
  round, steer, follow_up, abort (also with queued messages), prompts while
  busy, state queries, clear_queue, parse and unknown-command errors, and queue
  modes. Run recovery: a retryable failure that is retried, retries that run
  out, an abort and an `abort_retry` during a backoff, `set_auto_retry`, a
  failure that is not retried, a connection dropped mid-reply, a context
  overflow answered by compaction and one retry, a reply cut off at the output
  limit with and without a tool call, and a bash command whose output arrives
  while it runs. Each scenario also lists the commands sent, the settings it
  adds, and what each model request contained. Timestamps, durations, usage,
  ids, and paths are placeholders; the `placeholders` block lists them.
- `session-export.json`: session tree/export shape, including direct open of
  full-tree JSONL active branches.
- `settings.json`: settings merge and normalization cases, including
  `defaultTools` across the user and project layers.
- `skills.json`: how upstream loads and uses skills and prompt templates.
  What its frontmatter parser reads from a table of files; the skills and
  templates its resource loader loads, in order, with their sources and
  diagnostics, from trees of skill and prompt directories, settings, a local
  package, ignore files, and command-line paths, with and without project
  trust; the skills listing of the system prompt; what a session queues for
  `/skill:name` and `/template` input, and which input it leaves alone; how
  template arguments split and substitute; and `get_commands` of upstream's
  RPC mode for `--skill`, `--prompt-template`, `--no-skills`, and
  `--no-prompt-templates`. Directories are listed by name, and the temporary
  directory and the YAML library's error text are placeholders.
- `slash-commands.json`: slash command inventory and selected descriptions.
- `system-prompt.json`: the text of upstream's system prompt builder, with
  its sections, for the default tools, every tool pi shares with upstream, a
  subset, no tools, context files, a custom prompt, appended text, both,
  extension tools, and skills (with `read`, with `bash` only, with neither,
  and after a custom prompt); each case's inputs are the ones a real upstream
  session passed its builder. Also what upstream's resource loader picks from
  a tree of `SYSTEM.md`, `APPEND_SYSTEM.md`, and context files: project
  trust, command-line flags, `AGENTS.override.md`, and nested worktrees. The
  temporary directory, the product name, and the documentation paths are
  placeholders; the `placeholders` block lists them.
- `tool-behavior.json`: what upstream's `read`, `bash`, `edit`, `write`,
  `grep`, `find`, and `ls` return for fixed calls in fixed trees (result text,
  error flag, images, and bash's saved output), and the description and
  parameter schema each sends the model. The trees are part of the fixture.
  The `placeholders` and `normalizations` blocks list what is not recorded
  verbatim: temp paths, the unordered results of `fd`, and long texts, which
  are recorded as a hash with both ends.
- `tui-transcripts.json`: deterministic TUI transcript markers.

## Fixture-Backed Rust Behavior

Current Rust parity coverage includes:

- Provider request normalization for OpenAI Responses/Codex, OpenRouter,
  Copilot, Cloudflare AI Gateway, Anthropic, Google, Mistral, and Bedrock.
- Model-callable tool exposure, assistant tool-call preservation, tool-result
  message preservation, and continuation after tool execution.
- Provider-specific tool request and response shapes for OpenAI-compatible
  chat, OpenAI Responses, Anthropic, Google, Mistral, and Bedrock.
- OpenAI Responses cross-provider tool-call ID normalization.
- Google Gemini version-specific tool-result image routing.
- Mistral tool-history message shape and tool-call ID normalization.
- Auth discovery and credential redaction for supported providers.
- CLI flags, command inventory, settings normalization, slash commands,
  selected TUI markers, local tool inventory, and session export shape.
- The model catalog, in full: every upstream model pi can run is a built-in
  model with upstream's values for every field pi reads, in upstream's order
  (`builtin_catalog_is_upstreams_catalog_in_full` in
  `crates/pi-ai/src/catalog.rs`, and
  `default_models_are_upstreams_catalog_in_full` in `crates/pi-config` for
  what a config loads). The fields pi does not read yet are named in the
  test, so a new upstream field fails it.
- Thinking levels: supported levels and clamping for every case of
  `model-thinking.json`, and every catalog model has one of the recorded
  shapes (`thinking_levels_match_ts_for_every_catalog_model`).
- Model resolution: every `cliModel`, `modelScope`, `exactReference`,
  `initialModel`, and `globs` case of `model-resolution.json`, run on
  upstream's whole catalog (`crates/pi-config/src/model_resolver.rs`).
- Model and thinking selection end to end: every `launches` case, with pi
  started on the same arguments, settings, and keys, answers what upstream
  answered: the model object, the thinking level in effect, events, stderr,
  and exit code (`crates/pi-cli/tests/model_parity.rs`).
- Output caps: the provider request fixtures carry upstream's `maxTokens`
  for their models, and the builders now take them from the catalog.
- Built-in tools: every call of `tool-behavior.json` run against pi's tools in
  the same trees, with results compared exactly, and each tool's description
  and parameter schema compared exactly
  (`crates/pi-core/src/tool_parity_tests.rs`). The differences that remain are
  outside the recorded calls and are listed under "Built-in Tools Run
  In-Process" and the entries after it in the non-parity register.
- TypeScript full-tree session JSONL active-branch loading.
- RPC mode: every `rpc-protocol.json` scenario, record by record and model
  request by model request, allowing the differences listed under "RPC Mode"
  in the non-parity register (`crates/pi-cli/tests/rpc_parity.rs`). In two
  scenarios the model requests are compared only up to where pi's own choice
  begins: after the overflow of `overflow-recovery` (pi's compaction), and
  after the cut-off reply of `length-stop` (which pi keeps).
- Run recovery: `pi-ai` holds upstream's four pattern lists verbatim and
  gives upstream's verdict (retryable, context overflow) for every message of
  `error-classification.json`
  (`error_classification_matches_ts_patterns_and_verdicts`). How the run loop
  acts on a verdict is covered by the RPC scenarios above and by
  `crates/pi-core/src/recovery_tests.rs`.
- The system prompt: for every `system-prompt.json` case, a runtime with
  upstream's inputs sends upstream's text to the byte, apart from the product
  name and the `docs` section
  (`system_prompt_matches_ts_builder` in `crates/pi-core/src/system_prompt.rs`).
- Prompt and context file discovery: for every `system-prompt.json` discovery
  case, startup resolves the custom prompt, appended text, and context files
  upstream's resource loader resolves
  (`prompt_and_context_files_match_ts_discovery` in `crates/pi-cli/src/main.rs`).

Only those two tests cover the system prompt. The provider request fixtures
pass an explicit prompt, and the RPC goldens record messages without it.

- Skills and prompt templates, each against `skills.json`:
  - frontmatter fields, types, and bodies, and which files are rejected
    (`frontmatter_matches_ts_parser` in `crates/pi-config/src/frontmatter.rs`);
  - what loads, in what order, from where, with which diagnostics
    (`discovery_matches_ts_resource_loader` in
    `crates/pi-config/src/resources.rs`);
  - the listing text (`skills_listing_matches_ts_formatter` in
    `crates/pi-config/src/skills.rs`), and inside the whole prompt the
    `skills` cases of `system_prompt_matches_ts_builder`;
  - argument splitting and substitution (`template_arguments_match_ts` in
    `crates/pi-config/src/prompt_templates.rs`);
  - `/skill:name` and `/template` expansion, and the input left as it is
    (`input_expansion_matches_ts_session` in `crates/pi-core/src/input.rs`);
  - `get_commands` and the resource flags, through the binary
    (`get_commands_matches_upstream` in `crates/pi-cli/tests/rpc_parity.rs`).

  Expansion in each mode is covered by Rust's own tests: RPC `prompt`,
  `steer`, and `follow_up` with a skill given by `--skill`
  (`skills_and_templates_expand_in_prompt_steer_and_follow_up`), print mode
  (`print_mode_expands_skills_and_templates_and_honors_the_resource_flags`),
  and the TUI (`typed_prompts_expand_skills_and_templates`,
  `streaming_input_expands_templates_into_the_queues`). Another RPC test has
  the model `read` the listed file of a skill given from outside the working
  directory (`the_model_reads_a_skill_from_outside_the_working_directory`).

## Validation Commands

Use these before accepting intentional parity changes:

```bash
make ts-parity-update
cargo test -p pi-parity
cargo test -p pi-ai --lib matches_ts
cargo test -p pi-ai --lib catalog
cargo test -p pi-config --lib model
cargo test -p pi-cli --test model_parity
cargo test -p pi-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools
cargo test -p pi-core --lib system_prompt_matches_ts_builder
cargo test -p pi-cli --bin pi prompt_and_context_files_match_ts_discovery
cargo test -p pi-core --lib tool_parity_tests
cargo test -p pi-config --lib _ts
cargo test -p pi-core --lib input_expansion_matches_ts_session
cargo test -p pi-cli --test rpc_parity
make check
make ts-parity-drift
```

For TTY behavior changes, also run:

```bash
make e2e
```

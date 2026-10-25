# Rust Rewrite Parity Status

The Rust implementation uses the tip of upstream TypeScript `main` from
`https://github.com/earendil-works/pi` as a behavioral oracle. Reference code
must run only inside Docker through `make ts-parity-fixtures`,
`make ts-parity-update`, or `make ts-parity-drift`.

Parity is enforced in three layers:

- Docker-generated TypeScript fixtures in `tests/fixtures/ts-parity/`.
- Rust behavior tests in the owning crates.
- Inventory and documentation checks in `crates/noel-parity`.

`make check` runs the Rust tests, including `noel-parity`. `make
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
- `chord-delta.json`: upstream Chord's delta ops, recorded from
  `packages/chord` at its own pinned commit (`CHORD_REFERENCE_REF` in the
  Makefile) rather than `main`. Streams of op batches through one `encoder()`
  and `decoder()`, with the exact wire tuples, the decoded ops and each value
  after `applyImmutable`; wire batches the decoder rejects; `applyImmutable`
  results and errors; `diffRevisions` cases, by their result only; vectors of
  `canonicalJson` and `hash64` with non-ASCII and astral text, `-0`, large and
  small numbers and control characters; and contract descriptions with their
  fingerprints, `coverage` between them, and `fits` cases.
- `chord-link.json`: every frame upstream Chord's hosts send on their links,
  recorded from `packages/chord` at the same pinned commit, in fifteen
  scenarios run with noel's kind of contracts (a counter on the server, a
  session object on a `session:` worker): hello; holds, `shape` and `gone`;
  subscriptions with `snap`, `ops` and `unsub`; calls, results and errors;
  cancel, `maxCalls` and release; relay through the server's route, with roots
  and `root_end`; refused routes and a forged principal; `invalidatePrincipal`
  with re-stamping and closing; pings and the ping timeout; window
  coalescing and the four-window reset; version mismatches both ways; and
  protocol violations. Each frame has its link and the side that sent it.
- `cli-contract.json`: upstream CLI option and command contract.
- `cloudflare-ai-gateway-kimi.json`: Cloudflare AI Gateway OpenAI-compatible
  request shape.
- `error-classification.json`: how upstream reads a failed model request from
  its error text: the pattern lists of `isRetryableAssistantError` and
  `isContextOverflow`, and both verdicts for 124 error messages (HTTP
  statuses and bodies as noel words them, the overflow errors of each provider,
  transport failures, and quota and billing limits).
- `github-copilot-gpt-6.1-sol.json`: GitHub Copilot Responses request shape.
- `google-gemini-2.5-pro.json`: Gemini request shape and thinking config.
- `google-tool-image-routing.json`: Gemini 2 versus Gemini 3 tool-result
  image routing.
- `google-tools.json`: Gemini request shape with model-callable tools.
- `local-env.json`: upstream Durable's env conformance cases
  (`packages/durable/src/testing/env-conformance.ts`) run against
  `NodeExecutionEnv`, recorded from the same pinned commit as Chord's goldens:
  every assertion each case makes, in order, with its actual and expected
  values (byte ranges and line scans of a binary reader, directory pages,
  exec output per stream, exit codes, and the error codes of refused calls),
  the case's directory and modification times as placeholders. Watch cases
  record only their outcome, since their change reports vary from run to run.
  `crates/noel-env/tests/local_env_conformance.rs` runs the same cases against
  `LocalEnv` and compares every assertion.
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
  starts on (`defaultModelPerProvider`). This file is also data: `noel-ai`
  compiles it in as noel's built-in catalog, so regenerating it changes the
  models noel knows. Upstream generates the catalog from the providers' listings
  when the reference image is built, so the file is a snapshot of that moment
  and drifts with the listings, not only with upstream's source.
- `model-resolution.json`: how upstream picks a model, from its own functions
  on its built-in catalog: `--model`/`--provider`/`--thinking` resolution
  (`resolveCliModel`), scope patterns (`resolveModelScopeFromModels`), exact
  references (`findExactModelReferenceMatch`), the startup default
  (`findInitialModel`), and the glob matcher behind scope patterns
  (`minimatch`), each with its inputs and upstream's answer or error text.
  Its `credentials` build upstream's real model runtime over an `auth.json`
  and an environment, and record which of the providers a case asks about
  (`anthropic`, `openai`, and `openai-codex`, or the two Cloudflare ones)
  then have credentials, and the model startup takes.
  Its `launches` start upstream `pi --mode rpc` with model and thinking flags,
  settings, an `auth.json`, and fake provider keys and logins, and record
  `get_state`, `set_model`, and `set_thinking_level` answers, the
  `thinking_level_changed` events, stderr, and the exit code.
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
- `pi-env.json`: upstream pi-env's client (`Connection`, `RemoteExecutionEnv`,
  `openTerminal`) driving upstream's own daemon, both from the same pinned
  commit as Chord's goldens, through a tap that records the frames both ways:
  `hello`, file operations and handles, `listFiles`, `exec` with output,
  cancel, kill and timeout, `pty`, `watch`, pings, and the errors of refused
  calls. Machine fields, process ids, file identities and times are
  placeholders; output is merged per stream.
  `crates/noel-env-daemon/tests/pi_env_golden.rs` sends the client's frames to
  `noel-env serve` and compares its answers;
  `crates/noel-env-daemon/tests/pi_env_client_golden.rs` runs the scenarios
  (all but `pty`) with noel's `RemoteEnv` and compares the frames it sends.
  `make env-interop` runs upstream's client tests against `noel-env serve`,
  and noel's client tests against upstream's `pi-env`.
- `pi-env-ssh.json`: upstream pi-env's SSH deployment (`ssh.ts`) through a
  stand-in for `ssh` (`scripts/ts-parity/fake-ssh.sh`) that runs the remote
  commands in a scratch home: `sshArguments` for two targets, and every
  `ssh` invocation of scanning a host key, `connectSsh` deploying and
  finding the daemon, starting it plainly and through the login shell, and
  `sshConnection`'s first request. The scratch directory, the daemon's
  SHA-256 and the token are placeholders.
  `crates/noel-env-daemon/tests/ssh_deploy.rs` drives noel's port through the
  same stand-in; the invocations differ only by the daemon's names.
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
  its sections, for the default tools, every tool noel shares with upstream, a
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
- The model catalog, in full: every upstream model noel can run is a built-in
  model with upstream's values for every field noel reads, in upstream's order
  (`builtin_catalog_is_upstreams_catalog_in_full` in
  `crates/noel-ai/src/catalog.rs`, and
  `default_models_are_upstreams_catalog_in_full` in `crates/noel-config` for
  what a config loads). The fields noel does not read yet are named in the
  test, so a new upstream field fails it.
- Thinking levels: supported levels and clamping for every case of
  `model-thinking.json`, and every catalog model has one of the recorded
  shapes (`thinking_levels_match_ts_for_every_catalog_model`).
- Model resolution: every `cliModel`, `modelScope`, `exactReference`,
  `initialModel`, and `globs` case of `model-resolution.json`, run on
  upstream's whole catalog (`crates/noel-config/src/model_resolver.rs`).
- Credentials: every `credentials` case, with the same `auth.json` and
  environment, gives the same providers credentials and the same startup
  model (`credentials_offer_the_same_models_as_upstream` in
  `crates/noel-config`), but for upstream's own ChatGPT sign-in for `openai`,
  which the test names.
- Model and thinking selection end to end: every `launches` case, with noel
  started on the same arguments, settings, `auth.json`, and keys, answers
  what upstream answered: the model object, the thinking level in effect,
  events, stderr, and exit code (`crates/noel-cli/tests/model_parity.rs`).
- Output caps: the provider request fixtures carry upstream's `maxTokens`
  for their models, and the builders now take them from the catalog.
- Built-in tools: every call of `tool-behavior.json` run against noel's tools in
  the same trees, with results compared exactly, and each tool's description
  and parameter schema compared exactly
  (`crates/noel-core/src/tool_parity_tests.rs`). The differences that remain are
  outside the recorded calls and are listed under "Built-in Tools Run
  In-Process" and the entries after it in the non-parity register.
- TypeScript full-tree session JSONL active-branch loading.
- RPC mode: every `rpc-protocol.json` scenario, record by record and model
  request by model request, allowing the differences listed under "RPC Mode"
  in the non-parity register (`crates/noel-cli/tests/rpc_parity.rs`). In two
  scenarios the model requests are compared only up to where noel's own choice
  begins: after the overflow of `overflow-recovery` (noel's compaction), and
  after the cut-off reply of `length-stop` (which noel keeps).
- Run recovery: `noel-ai` holds upstream's four pattern lists verbatim and
  gives upstream's verdict (retryable, context overflow) for every message of
  `error-classification.json`
  (`error_classification_matches_ts_patterns_and_verdicts`). How the run loop
  acts on a verdict is covered by the RPC scenarios above and by
  `crates/noel-core/src/recovery_tests.rs`.
- The system prompt: for every `system-prompt.json` case, a runtime with
  upstream's inputs sends upstream's text to the byte, apart from noel's
  names (`noel_config::UPSTREAM_NAMES`: the product, and the `NOEL_*`
  variables of bash's rule) and the `docs` section
  (`system_prompt_matches_ts_builder` in `crates/noel-core/src/system_prompt.rs`).
- Prompt and context file discovery: for every `system-prompt.json` discovery
  case, startup resolves the custom prompt, appended text, and context files
  upstream's resource loader resolves
  (`prompt_and_context_files_match_ts_discovery` in `crates/noel-cli/src/main.rs`).
  Each case runs on the tree as upstream wrote it (a project's `.pi/`, which
  noel reads when there is no `.noel/`) and again under noel's names.

Only those two tests cover the system prompt. The provider request fixtures
pass an explicit prompt, and the RPC goldens record messages without it.

- Skills and prompt templates, each against `skills.json`:
  - frontmatter fields, types, and bodies, and which files are rejected
    (`frontmatter_matches_ts_parser` in `crates/noel-config/src/frontmatter.rs`);
  - what loads, in what order, from where, with which diagnostics
    (`discovery_matches_ts_resource_loader` in
    `crates/noel-config/src/resources.rs`), on the tree as upstream wrote it
    (`.pi/`) and again under noel's names (`.noel/`);
  - the listing text (`skills_listing_matches_ts_formatter` in
    `crates/noel-config/src/skills.rs`), and inside the whole prompt the
    `skills` cases of `system_prompt_matches_ts_builder`;
  - argument splitting and substitution (`template_arguments_match_ts` in
    `crates/noel-config/src/prompt_templates.rs`);
  - `/skill:name` and `/template` expansion, and the input left as it is
    (`input_expansion_matches_ts_session` in `crates/noel-core/src/input.rs`);
  - `get_commands` and the resource flags, through the binary and in both
    of those layouts
    (`get_commands_matches_upstream` in `crates/noel-cli/tests/rpc_parity.rs`).

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
cargo test -p noel-parity
cargo test -p noel-ai --lib matches_ts
cargo test -p noel-ai --lib catalog
cargo test -p noel-config --lib model
cargo test -p noel-cli --test model_parity
cargo test -p noel-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools
cargo test -p noel-core --lib system_prompt_matches_ts_builder
cargo test -p noel-cli --bin noel prompt_and_context_files_match_ts_discovery
cargo test -p noel-core --lib tool_parity_tests
cargo test -p noel-config --lib _ts
cargo test -p noel-core --lib input_expansion_matches_ts_session
cargo test -p noel-cli --test rpc_parity
cargo test -p noel-chord
cargo test -p noel-env
make check
make ts-parity-drift
```

For TTY behavior changes, also run:

```bash
make e2e
```

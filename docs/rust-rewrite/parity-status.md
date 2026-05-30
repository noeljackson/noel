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
- `github-copilot-gpt-5.4.json`: GitHub Copilot Responses request shape.
- `google-gemini-2.5-pro.json`: Gemini request shape and thinking config.
- `google-tool-image-routing.json`: Gemini 2 versus Gemini 3 tool-result
  image routing.
- `google-tools.json`: Gemini request shape with model-callable tools.
- `local-tools.json`: built-in local tool inventory and model-callable schema
  key coverage.
- `mistral-devstral.json`: Mistral chat-completions request shape.
- `mistral-tool-id.json`: Mistral tool-history request shape and 9-character
  tool-call ID normalization.
- `mistral-tools.json`: Mistral chat-completions request shape with
  model-callable tools.
- `model-catalog.json`: selected upstream model catalog targets.
- `openai-codex-chatgpt-oauth.json`: Codex Responses request shape using
  ChatGPT OAuth credentials.
- `openai-responses-tool-id.json`: OpenAI Responses pipe-separated tool-call
  ID normalization for cross-provider handoff.
- `openai-responses-tools.json`: OpenAI Responses request shape with function
  tools.
- `openrouter-kimi.json`: OpenRouter OpenAI-compatible request shape.
- `session-export.json`: session tree/export shape, including direct open of
  full-tree JSONL active branches.
- `settings.json`: settings merge and normalization cases.
- `slash-commands.json`: slash command inventory and selected descriptions.
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
  selected TUI markers, local tool inventory/schema keys, session export shape,
  and model catalog targets.
- TypeScript full-tree session JSONL active-branch loading.

## Validation Commands

Use these before accepting intentional parity changes:

```bash
make ts-parity-update
cargo test -p pi-parity
cargo test -p pi-ai --lib matches_ts
cargo test -p pi-core --lib upstream_agent_tool_loop_fixture_documents_model_callable_tools
make check
make ts-parity-drift
```

For TTY behavior changes, also run:

```bash
make e2e
```

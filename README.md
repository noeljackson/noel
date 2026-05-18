# pi

`pi` is a native Rust CLI coding agent.

The repository has been cut over from the previous TypeScript/npm monorepo to a Rust-only Cargo workspace. The web UI and npm package runtime are no longer part of the product path.

## Status

Current Rust implementation:

- CLI binary: `pi`
- Interactive terminal loop
- Print mode
- Append-only JSONL sessions
- Context-safe `/reload`
- Config/auth/model loading from `~/.pi/agent`
- `AGENTS.md` and `CLAUDE.md` context discovery
- Built-in local tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`
- Providers: faux test provider, OpenAI chat/responses/Codex, Azure OpenAI Responses, Anthropic Messages, Google Gemini/Vertex, OpenRouter, GitHub Copilot, Amazon Bedrock bearer-token Converse, Mistral, Cloudflare Workers AI/AI Gateway, OpenAI-compatible aliases for DeepSeek, Groq, Cerebras, xAI, Z.AI, Hugging Face, Together, Moonshot AI, and OpenCode, plus Anthropic-compatible aliases for Vercel AI Gateway, Fireworks, MiniMax, Kimi Coding, and Xiaomi MiMo

Intentionally removed:

- npm workspaces
- Node.js and TypeScript runtime
- web UI
- hot module reload
- npm extension package management

## Build

```bash
cargo build --release
```

The binary is:

```bash
target/release/pi
```

Install for the current user:

```bash
make install
```

Install under a different prefix:

```bash
make install PREFIX="/opt/pi"
```

Install system-wide:

```bash
sudo make install PREFIX="/usr/local"
```

## Run

Interactive mode:

```bash
cargo run -p pi-cli
```

Print mode:

```bash
cargo run -p pi-cli -- -p --model faux/echo "hello"
```

JSON-line RPC mode:

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"prompt","params":{"prompt":"hello"}}\n' \
  | cargo run -p pi-cli -- --mode rpc --model faux/echo
```

List models:

```bash
cargo run -p pi-cli -- --list-models
```

List image-generation models:

```bash
cargo run -p pi-cli -- images
```

Generate an image through a configured OpenRouter API key:

```bash
cargo run -p pi-cli -- generate-image --output image.png \
  --model openrouter/google/gemini-3.1-flash-image-preview "a compact rust cli logo"
```

Resume or fork sessions:

```bash
cargo run -p pi-cli -- --continue
cargo run -p pi-cli -- --session <id-prefix|name|path>
cargo run -p pi-cli -- --fork <id-prefix|name|path>
```

Useful CLI scope flags:

```bash
cargo run -p pi-cli -- --models faux/echo --tools read,write -p "hello"
cargo run -p pi-cli -- --no-tools -p "no tools"
cargo run -p pi-cli -- --system-prompt prompt.md --append-system-prompt extra.md -p "hello"
cargo run -p pi-cli -- --image screenshot.png -p "describe this"
cargo run -p pi-cli -- --export session.json -p "hello"
cargo run -p pi-cli -- --export session.html -p "hello"
cargo run -p pi-cli -- --export session.jsonl -p "hello"
```

Prompt arguments starting with `@` are expanded from files:

```bash
cargo run -p pi-cli -- -p --model faux/echo @prompt.txt
```

## Configuration

Default config root:

```text
~/.pi/agent
```

Supported files:

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

Project skills are also discovered from `.agents/skills` directories from the
current working directory up to the git root. `SKILL.md` files are named by
their containing directory.

Environment overrides:

- `PI_CODING_AGENT_DIR`
- `PI_CODING_AGENT_SESSION_DIR`

`settings.json` supports default model selection, shell configuration, prompt inputs, enabled models/tools, and `sessionDir`:

```json
{
  "defaultProvider": "faux",
  "defaultModel": "echo",
  "defaultThinkingLevel": "xhigh",
  "enabledModels": ["faux/echo"],
  "enabledTools": ["read", "bash", "edit", "write", "grep", "find", "ls"],
  "sessionDir": "sessions",
  "modelRefresh": {
    "enabled": true,
    "ttlHours": 24
  }
}
```

Provider API keys can be stored in `auth.json`:

```json
{
  "openai": { "type": "api_key", "key": "env:OPENAI_API_KEY" },
  "openai-codex": { "type": "oauth", "access_token": "env:CODEX_ACCESS_TOKEN", "expires": 0 },
  "azure-openai-responses": { "type": "api_key", "key": "env:AZURE_OPENAI_API_KEY" },
  "anthropic": { "type": "api_key", "key": "env:ANTHROPIC_API_KEY" },
  "google": { "type": "api_key", "key": "env:GEMINI_API_KEY" },
  "google-vertex": { "type": "api_key", "key": "env:GOOGLE_CLOUD_API_KEY" },
  "github-copilot": { "type": "api_key", "key": "env:COPILOT_GITHUB_TOKEN" },
  "openrouter": { "type": "api_key", "key": "env:OPENROUTER_API_KEY" },
  "deepseek": { "type": "api_key", "key": "env:DEEPSEEK_API_KEY" },
  "groq": { "type": "api_key", "key": "env:GROQ_API_KEY" },
  "cerebras": { "type": "api_key", "key": "env:CEREBRAS_API_KEY" },
  "xai": { "type": "api_key", "key": "env:XAI_API_KEY" },
  "zai": { "type": "api_key", "key": "env:ZAI_API_KEY" },
  "huggingface": { "type": "api_key", "key": "env:HF_TOKEN" },
  "together": { "type": "api_key", "key": "env:TOGETHER_API_KEY" },
  "moonshotai": { "type": "api_key", "key": "env:MOONSHOT_API_KEY" },
  "moonshotai-cn": { "type": "api_key", "key": "env:MOONSHOT_API_KEY" },
  "opencode": { "type": "api_key", "key": "env:OPENCODE_API_KEY" },
  "opencode-go": { "type": "api_key", "key": "env:OPENCODE_API_KEY" },
  "vercel-ai-gateway": { "type": "api_key", "key": "env:AI_GATEWAY_API_KEY" },
  "fireworks": { "type": "api_key", "key": "env:FIREWORKS_API_KEY" },
  "minimax": { "type": "api_key", "key": "env:MINIMAX_API_KEY" },
  "minimax-cn": { "type": "api_key", "key": "env:MINIMAX_CN_API_KEY" },
  "kimi-coding": { "type": "api_key", "key": "env:KIMI_API_KEY" },
  "xiaomi": { "type": "api_key", "key": "env:XIAOMI_API_KEY" },
  "xiaomi-token-plan-cn": { "type": "api_key", "key": "env:XIAOMI_TOKEN_PLAN_CN_API_KEY" },
  "xiaomi-token-plan-ams": { "type": "api_key", "key": "env:XIAOMI_TOKEN_PLAN_AMS_API_KEY" },
  "xiaomi-token-plan-sgp": { "type": "api_key", "key": "env:XIAOMI_TOKEN_PLAN_SGP_API_KEY" },
  "amazon-bedrock": { "type": "api_key", "key": "env:AWS_BEARER_TOKEN_BEDROCK" },
  "mistral": { "type": "api_key", "key": "env:MISTRAL_API_KEY" },
  "cloudflare-workers-ai": { "type": "api_key", "key": "env:CLOUDFLARE_API_KEY" },
  "cloudflare-ai-gateway": { "type": "api_key", "key": "env:CLOUDFLARE_API_KEY" }
}
```

Or use the CLI helper:

```bash
pi login anthropic --api-key env:ANTHROPIC_API_KEY
printf '%s' "$ANTHROPIC_API_KEY" | pi login anthropic --api-key -
pi logout anthropic
```

When no explicit API key is configured, `pi` can reuse existing CLI login credentials:

- Claude Code: `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or `~/.claude/.credentials.json`
- Codex/ChatGPT: `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json` for `openai` and `openai-codex`

Explicit API keys still take precedence over login tokens.

Provider-specific environment:

- Azure OpenAI: set `AZURE_OPENAI_BASE_URL` or `AZURE_OPENAI_RESOURCE_NAME`; optionally set `AZURE_OPENAI_DEPLOYMENT_NAME`, `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`, and `AZURE_OPENAI_API_VERSION`.
- Google Vertex: set `GOOGLE_CLOUD_PROJECT` or `GCLOUD_PROJECT`, plus `GOOGLE_CLOUD_LOCATION`.
- Cloudflare: set `CLOUDFLARE_ACCOUNT_ID`; AI Gateway also needs `CLOUDFLARE_GATEWAY_ID`.
- Amazon Bedrock: Rust direct calls currently use Bedrock bearer-token auth via `AWS_BEARER_TOKEN_BEDROCK`.

`models.json` may override the built-in model list:

```json
[
  {
    "provider": "openai",
    "id": "gpt-5.4",
    "api": "openai-responses"
  },
  {
    "provider": "anthropic",
    "id": "claude-sonnet-4-5",
    "api": "anthropic-messages"
  },
  {
    "provider": "openrouter",
    "id": "moonshotai/kimi-k2.6",
    "api": "openai-completions",
    "baseUrl": "https://openrouter.ai/api/v1"
  }
]
```

`model-cache.json` is managed by `pi`. On startup, `pi` uses built-in models,
cached models, and explicit `models.json` entries immediately, then starts a
non-blocking background refresh when `modelRefresh.enabled` is not false,
`PI_OFFLINE`/`--offline` is not set, the cache is older than `ttlHours`, and a
provider has supported auth. Refreshed models are available after `/reload` or
the next startup. Anthropic API-key refresh uses the official Models API;
Claude Code OAuth and ChatGPT/Codex OAuth refresh are best-effort against the
same provider auth paths used for model requests. Refresh failures are ignored
unless verbose logging is enabled.

Thinking levels can be set with `--thinking <level>` or `/thinking <level>`.
Supported levels are model-specific. Opus exposes `high`, `xhigh`, and `max`;
OpenAI/Codex reasoning models expose `minimal` through `xhigh`. While the
`/model` selector is open, left/right adjusts the pending thinking level and
Enter applies both the model and thinking level to the session.

`keybindings.json` may be either an array:

```json
[{ "action": "submit", "keys": ["enter"] }]
```

or an object map:

```json
{ "submit": ["enter"], "cancel": ["escape"] }
```

Local Rust-path packages are configured with `packages`. A package may expose
resources by convention through `extensions/`, `skills/`, `prompts/`, and
`themes/`, or with `package.json` under the `pi` key:

```json
{
  "pi": {
    "extensions": ["extensions/assist"],
    "skills": ["skills/review.md"],
    "prompts": ["prompts/fix.md"],
    "themes": ["themes/dark.json"]
  }
}
```

Object package entries filter package resources without npm:

```json
{
  "packages": [
    {
      "source": "vendor/pi-package",
      "extensions": ["extensions/*.md", "!extensions/legacy.md", "+extensions/force.txt"],
      "skills": [],
      "prompts": ["prompts/review.md"]
    }
  ]
}
```

Omitting a resource key loads all resources of that type. `[]` loads none.
`!pattern` excludes wildcard matches, `+path` force-includes an exact path, and
`-path` force-excludes an exact path.
Resource discovery honors `.gitignore`, `.ignore`, and `.fdignore` files in
scanned resource directories.

Resources can be disabled by name or wildcard through `disabledResources`, or
managed with `pi config disable <extension|skill|prompt|theme> <name>` and
`pi config enable <extension|skill|prompt|theme> <name>`:

```json
{
  "disabledResources": {
    "extensions": ["legacy"],
    "prompts": ["prompt:old-*"]
  }
}
```

## Interactive Commands

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

`/reload` reloads config, prompts, context files, model metadata, keybindings, provider availability, and tool definitions without clearing the current session state.

Interactive assistant responses stream text as provider deltas arrive. `/queue <prompt>` adds follow-up prompts that run after the next assistant turn, `/interrupt` clears queued follow-ups, and `!`/`!!` execute shell commands without adding them to the conversation context. Manual and automatic compaction persist summary records, and forked or cloned sessions persist branch summaries. Editor state tracks history, undo, kill-ring, and slash completions; `/editor` uses `PI_EDITOR_COMMAND`, `VISUAL`, or `EDITOR`. Image inputs are encoded as provider attachments with terminal text fallback.

## RPC Methods

`--mode rpc` reads one JSON object per line from stdin and writes one JSON object per line to stdout.

- `prompt` with `{ "prompt": "..." }`
- `reload`
- `session`
- `model` with `{ "model": "provider/id" }`

## Validation

```bash
make check
```

TTY e2e test:

```bash
make e2e
```

Dockerized TTY e2e test:

```bash
make docker-e2e
```

Docker-only TS reference fixture generation:

```bash
make ts-parity-fixtures
```

This is the only supported path for executing TypeScript reference code. It
clones the upstream TypeScript repo `https://github.com/earendil-works/pi`
inside Docker, runs npm inside Docker, and writes sanitized fixtures under
`tests/fixtures/ts-parity/`.

Update committed parity fixtures from the moving TypeScript reference:

```bash
make ts-parity-update
```

Check for drift against the moving reference and write an agent brief:

```bash
make ts-parity-drift
```

Dispatch the drift brief to an external CLI agent:

```bash
PI_PARITY_AGENT_COMMAND='cw exec --name ts-parity-agent -- claude' make ts-parity-agent
```

The agent command receives `target/ts-parity-drift/brief.md` on stdin. Scheduled
GitHub Actions runs the same Docker-only drift harness and opens or updates a
`TS parity drift detected` issue when the TypeScript reference changes.
Override `TS_REFERENCE_REPO` or `TS_PARITY_TRACKING_REF` to compare against a
different TypeScript repository or ref.

Manual real-provider Opus smoke with Claude Code OAuth:

```bash
make smoke-claude-opus-oauth
```

Full manual smoke suite:

```bash
make test-smoke
```

The real-provider smoke is intentionally not part of `test`, `check`, or `e2e`.
It requires `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or
`~/.claude/.credentials.json`, sends one tiny prompt to
`anthropic/claude-opus-4-7`, and defaults to `--thinking max`. `test-smoke`
runs local tmux e2e first, then the real-provider Opus OAuth smoke.

## Development Notes

The old TypeScript implementation is preserved on the `ts-reference` branch for behavioral reference. Active development on `main` is Rust-only.

Rust sessions use a new append-only JSONL schema plus JSON, JSONL, and HTML export/import where applicable. Legacy TypeScript session logs are not migrated automatically; keep `ts-reference` for reading old session behavior and export/import only through the Rust schema. `/share` writes a local HTML export; web or gist sharing is intentionally unsupported in the Rust-only CLI.

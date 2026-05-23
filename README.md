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

## Why Rust

The active product is a terminal-first coding agent. Rust keeps the shipped path
to one native binary with no Node.js runtime, no npm install step, and no browser
UI dependency. That matters for dogfooding on remote shells, Raspberry Pi class
machines, tmux sessions, and locked-down environments where a small predictable
binary is easier to install, restart, and debug.

The rewrite also makes terminal behavior a first-class part of the product. TTY
input, mouse handling, scrollback, session replay, tool execution, provider
streaming, and reload behavior live in one Cargo workspace instead of being split
between a web UI, Node process, and package runtime.

## How Development Works

Use Cargo and Make targets only. The main binary is `crates/pi-cli`; shared
behavior lives in `pi-core`, provider adapters in `pi-ai`, configuration in
`pi-config`, local tools in `pi-tools`, terminal rendering helpers in `pi-tui`,
and TypeScript parity checks in `pi-parity`.

For normal local work:

```bash
make dogfood
make check
make e2e
```

`make dogfood` starts the development TUI with the faux provider and a Rust
rebuild/restart watcher. Durable state stays in the session store, so a rebuild
should not clear conversation messages, cwd, session identity, tool history,
queued messages, or active context. `make check` runs formatting, clippy, and
Rust tests. `make e2e` runs tmux-based terminal behavior checks.

TypeScript is reference material, not a runtime dependency. Parity fixtures are
generated only through Docker:

```bash
make parity-check
make ts-parity-update
```

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

Local dogfood mode with session resume, the faux provider, and automatic
rebuild/restart on Rust source changes:

```bash
make dogfood
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

Executable extensions can opt into the JSON protocol with an adjacent
`.pi-extension.json` manifest. A manifest `tools` array registers model-callable
tools; Pi sends `kind: "tool"` JSON requests on stdin and expects a JSON
response with `output` or `error`:

```json
{
  "protocol": "json",
  "tools": [
    {
      "name": "fixture_echo",
      "description": "Echo text.",
      "parameters": {
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"]
      }
    }
  ]
}
```

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

Interactive assistant responses stream text as provider deltas arrive. `/queue <prompt>` adds follow-up prompts that run after the next assistant turn, `/interrupt` clears queued follow-ups, and `!`/`!!` execute shell commands without adding them to the conversation context. Manual and automatic compaction persist summary records, and forked or cloned sessions persist branch summaries. Editor state tracks history, undo, kill-ring, and slash completions; restored session user prompts repopulate prompt history. `/editor` uses `PI_EDITOR_COMMAND`, `VISUAL`, or `EDITOR`. Mouse wheel scrolls the transcript, terminal selection remains available through the terminal selection modifier, and bracketed paste inserts pasted text into the prompt. Image inputs are encoded as provider attachments with terminal text fallback.

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

Release-binary dogfood smoke:

```bash
make dogfood-release
```

Long TTY paint and scroll dogfood:

```bash
make dogfood-long
```

Dockerized TTY e2e test:

```bash
make docker-e2e
```

## Upstream TypeScript Parity

Rust parity is tracked against the upstream TypeScript implementation at
`https://github.com/earendil-works/pi` on `main`. The Rust repo does not run
npm or TypeScript on the host. All TypeScript reference execution happens inside
`Dockerfile.ts-parity`.

The parity harness captures sanitized fixtures from upstream TypeScript code
under `tests/fixtures/ts-parity/`. Provider fixtures record request shape:
URL, method, selected headers, request body, or payload. Contract fixtures cover
CLI flags, slash commands, settings normalization, and session/export shape
where Rust intentionally supports compatibility or analogous behavior. Secrets
are redacted. Rust tests compare covered behavior against those fixtures, so
upstream changes become visible as fixture diffs and test failures.

Generate fixtures from the configured reference:

```bash
make ts-parity-fixtures
```

Refresh committed fixtures from upstream:

```bash
make ts-parity-update
```

Check for drift without accepting it:

```bash
make ts-parity-drift
```

Run the committed fixture inventory checks, Rust fixture assertions, and Docker
drift check together:

```bash
make parity-check
```

`make ts-parity-drift` regenerates fixtures in Docker, compares them with the
committed fixtures, and fails if they differ. On drift it writes:

- `target/ts-parity-drift/fixture.diff`
- `target/ts-parity-drift/brief.md`

The brief is designed for a coding agent. It includes the upstream reference,
constraints, suggested workflow, and the fixture diff. To dispatch that brief to
an external CLI agent:

```bash
PI_PARITY_AGENT_COMMAND='cw exec --name ts-parity-agent -- claude' make ts-parity-agent
```

`PI_PARITY_AGENT_COMMAND` receives `brief.md` on stdin. Scheduled GitHub Actions
runs the same drift harness and opens or updates a `TS parity drift detected`
issue when upstream changes. Override `TS_REFERENCE_REPO` or
`TS_PARITY_TRACKING_REF` to compare against a different TypeScript repository or
ref.

When drift is intentional, update Rust behavior and committed fixtures together:

```bash
make ts-parity-update
make parity-check
make check
```

Parity fixtures prove compatibility only for covered provider paths and product
contracts. They do not prove full product parity, live provider success, or TUI
behavior; those are covered by Rust unit tests, tmux e2e tests, and manual
real-provider smoke tests.

The parity inventory and deliberate non-parity decisions are tracked in
`docs/rust-rewrite/parity-status.md` and
`docs/rust-rewrite/non-parity-register.md`. The `pi-parity` crate keeps fixture
inventory, fixture source metadata, redaction checks, and parity documentation
in sync.

Manual real-provider Opus smoke with Claude Code OAuth:

```bash
make smoke-claude-opus-oauth
```

Generic opt-in real-provider print smoke:

```bash
PI_SMOKE_REAL=1 PI_SMOKE_REAL_MODEL=provider/model make smoke-real
```

Provider profile smokes:

```bash
PI_SMOKE_REAL=1 make smoke-real-openai
PI_SMOKE_REAL=1 make smoke-real-anthropic
PI_SMOKE_REAL=1 make smoke-real-gemini
PI_SMOKE_REAL=1 make smoke-real-mistral
PI_SMOKE_REAL=1 make smoke-real-openrouter
```

Full manual smoke suite:

```bash
make test-smoke
```

`make dogfood` runs the development TUI with `--continue --model faux/echo`.
It also starts a quiet background `cargo build -p pi-cli` watcher; successful
rebuilds restart the running TUI process, and the continued session restores
conversation messages from disk. Watcher output is written to
`target/dogfood-dev/watch.log`. `make dogfood-release` builds
`target/release/pi` and runs the binary in tmux with an isolated agent/session
directory under `target/`. It uses the faux provider, so it does not require
provider credentials or network access. `make dogfood-long` uses the same
release binary and faux provider, but creates a long transcript, checks
PageUp/Home and End scroll behavior, resizes the tmux pane, and verifies the
exported session still contains the full transcript. `make dogfood-real` is
opt-in and runs real Claude and Codex TTY smoke tests when local credentials are
available. It asks each provider for a tiny Rust program and checks that a real
assistant message contains the expected marker and `fn main`. `make smoke-real`
is a generic print-mode live-provider smoke; it exits without network access
unless `PI_SMOKE_REAL=1` and `PI_SMOKE_REAL_MODEL=provider/model` are set.
The profile targets preselect default models for OpenAI, Anthropic, Gemini,
Mistral, and OpenRouter while still using the normal auth resolver.

The real TTY dogfood target can be narrowed with `PI_DOGFOOD_REAL_PROVIDERS`:

```bash
PI_DOGFOOD_REAL_PROVIDERS=claude make dogfood-real
PI_DOGFOOD_REAL_PROVIDERS=codex make dogfood-real
```

The default models can be overridden with `PI_DOGFOOD_CLAUDE_MODEL` and
`PI_DOGFOOD_CODEX_MODEL`.

The real-provider smoke is intentionally not part of `test`, `check`, or `e2e`.
It requires `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or
`~/.claude/.credentials.json`, sends one tiny prompt to
`anthropic/claude-opus-4-7`, and defaults to `--thinking max`. The generic
`smoke-real` target uses the normal auth resolver for the selected model and
defaults to `--thinking off` unless `PI_SMOKE_REAL_THINKING` is set.
Provider profile defaults can be overridden with `PI_SMOKE_OPENAI_MODEL`,
`PI_SMOKE_ANTHROPIC_MODEL`, `PI_SMOKE_GEMINI_MODEL`, `PI_SMOKE_MISTRAL_MODEL`,
or `PI_SMOKE_OPENROUTER_MODEL`. `PI_SMOKE_REAL_EXPECTED` changes the expected
marker for all real-provider print smokes.
`test-smoke` runs local tmux e2e first, then the opt-in generic real-provider
smoke and the real-provider Opus OAuth smoke.

## Development Notes

The old TypeScript implementation is preserved on the `ts-reference` branch for behavioral reference. Active development on `main` is Rust-only.

Rust live sessions use an append-only replay JSONL log. JSONL export/import and direct session open support the TypeScript v3 session-tree shape where applicable, including a `type:"session"` header and entry `id`/`parentId` chain. Full in-place active-leaf tree editing is still tracked as parity work. Legacy TypeScript session logs are not migrated automatically; keep `ts-reference` for reading old session behavior. `/share` writes a local HTML export; web or gist sharing is intentionally unsupported in the Rust-only CLI.

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
- Providers: faux test provider, OpenAI-compatible chat completions, Anthropic Messages, Google Gemini

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

## Configuration

Default config root:

```text
~/.pi/agent
```

Supported files:

- `settings.json`
- `auth.json`
- `models.json`
- `keybindings.json`
- `sessions/`

Environment overrides:

- `PI_CODING_AGENT_DIR`
- `PI_CODING_AGENT_SESSION_DIR`

`settings.json` supports default model selection, shell configuration, prompt inputs, enabled models/tools, and `sessionDir`:

```json
{
  "defaultProvider": "faux",
  "defaultModel": "echo",
  "enabledModels": ["faux/echo"],
  "enabledTools": ["read", "bash", "edit", "write", "grep", "find", "ls"],
  "sessionDir": "sessions"
}
```

Provider API keys can be stored in `auth.json`:

```json
{
  "openai": { "type": "api_key", "key": "env:OPENAI_API_KEY" },
  "anthropic": { "type": "api_key", "key": "env:ANTHROPIC_API_KEY" },
  "google": { "type": "api_key", "key": "env:GEMINI_API_KEY" }
}
```

When no explicit API key is configured, `pi` can reuse existing CLI login credentials:

- Claude Code: `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or `~/.claude/.credentials.json`
- Codex/ChatGPT: `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`

Explicit API keys still take precedence over login tokens.

`models.json` may override the built-in model list:

```json
[
  {
    "provider": "openai",
    "id": "gpt-4.1",
    "api": "open-ai"
  },
  {
    "provider": "anthropic",
    "id": "claude-sonnet-4-5",
    "api": "anthropic"
  }
]
```

`keybindings.json` may be either an array:

```json
[{ "action": "submit", "keys": ["enter"] }]
```

or an object map:

```json
{ "submit": ["enter"], "cancel": ["escape"] }
```

## Interactive Commands

- `/help`
- `/models`
- `/model <provider/id>`
- `/session`
- `/reload`
- `/read <path>`
- `/write <path> <text>`
- `/edit <path> <find> <replace>`
- `/grep <text> [path]`
- `/find <text>`
- `/ls [path]`
- `/bash <command>`
- `/quit`

`/reload` reloads config, prompts, context files, model metadata, keybindings, provider availability, and tool definitions without clearing the current session state.

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

## Development Notes

The old TypeScript implementation is preserved on the `ts-reference` branch for behavioral reference. Active development on `main` is Rust-only.

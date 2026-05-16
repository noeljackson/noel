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
- `keybindings.json`
- `skills/`
- `prompts/`
- `themes/`
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
- `/settings`
- `/status`
- `/diagnostics`
- `/hotkeys`
- `/complete <prefix>`
- `/history`
- `/editor [text]`
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
- `/multiline`
- `/session`
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

Interactive assistant responses stream text as provider deltas arrive. `/queue <prompt>` adds follow-up prompts that run after the next assistant turn, `/interrupt` clears queued follow-ups, and `!`/`!!` execute shell commands without adding them to the conversation context. Manual and automatic compaction persist summary records, and forked or cloned sessions persist branch summaries. Editor state tracks history, undo, kill-ring, and slash completions; `/editor` uses `PI_EDITOR_COMMAND`, `VISUAL`, or `EDITOR`.

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

Rust sessions use a new append-only JSONL schema plus JSON, JSONL, and HTML export/import where applicable. Legacy TypeScript session logs are not migrated automatically; keep `ts-reference` for reading old session behavior and export/import only through the Rust schema. `/share` writes a local HTML export; web or gist sharing is intentionally unsupported in the Rust-only CLI.

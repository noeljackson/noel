# noel

```
  ███▄▄▄▄    ▄██████▄     ▄████████  ▄█
  ███▀▀▀██▄ ███    ███   ███    ███ ███
  ███   ███ ███    ███   ███    █▀  ███
  ███   ███ ███    ███  ▄███▄▄▄     ███
  ███   ███ ███    ███ ▀▀███▀▀▀     ███
  ███   ███ ███    ███   ███    █▄  ███
  ███   ███ ███    ███   ███    ███ ███▌    ▄
   ▀█   █▀   ▀██████▀    ██████████ █████▄▄██
                                    ▀
   ▄████████  ▄██████▄  ████████▄     ▄████████
  ███    ███ ███    ███ ███   ▀███   ███    ███
  ███    █▀  ███    ███ ███    ███   ███    █▀
  ███        ███    ███ ███    ███  ▄███▄▄▄
  ███        ███    ███ ███    ███ ▀▀███▀▀▀
  ███    █▄  ███    ███ ███    ███   ███    █▄
  ███    ███ ███    ███ ███   ▄███   ███    ███
  ████████▀   ▀██████▀  ████████▀    ██████████
  
```

`noel` is a native Rust CLI coding agent.

The logo (`docs/logo.txt`, shown above and as the TUI greeting): a two-line
ANSI-shadow wordmark, `noel` over `NOEL`. A single-glyph pixel N mark also
exists (`docs/n-icon.svg`), with a drawing scratchpad at
`docs/pixel-icon-lab.html`.

The repository has been cut over from the previous TypeScript/npm monorepo to a Rust-only Cargo workspace. The web UI and npm package runtime are no longer part of the product path.

## Documentation

- [Configuration](docs/configuration.md)
- [Interactive commands](docs/commands.md)
- [Context compaction](docs/compaction.md)
- [Server sessions](docs/server.md)
- [Remote workspaces](docs/remote-workspaces.md)
- [RPC mode](docs/rpc.md)
- [Upstream TypeScript parity](docs/parity.md)
- [Specification](docs/spec.md)
- [Roadmap](docs/roadmap.md)

## Status

Current Rust implementation:

- CLI binary: `noel`
- Interactive terminal loop
- Print mode
- Server sessions: `noel server`, `noel client`, and the TUI attached to a
  server session (`noel --attach`)
- Remote workspaces: a session whose tools run in a directory on another
  machine, over ssh (`--workspace ssh://host/path`)
- Append-only JSONL sessions
- Context-safe `/reload`
- Config/auth/model loading from `~/.noel`
- Upstream's default system prompt, with `SYSTEM.md`, `APPEND_SYSTEM.md`,
  and `AGENTS.md`/`CLAUDE.md` context discovery
- Built-in tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`, `todo`, `task` (subagents; offered when an agent definition is loaded), `web_search`, `web_fetch`
- Models: upstream's built-in catalog (every provider and chat model upstream ships, with thinking levels, limits, and prices), plus the faux test models and three providers of this port's own
- Provider APIs: OpenAI chat completions, OpenAI Responses, Codex, Azure OpenAI Responses, Anthropic Messages, Google Gemini and Vertex, Amazon Bedrock bearer-token Converse, and Mistral, which between them serve the catalog's providers

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

Use Cargo and Make targets only. The main binary is `crates/noel-cli`; shared
behavior lives in `noel-core`, provider adapters in `noel-ai`, configuration in
`noel-config`, local tools in `noel-tools`, terminal rendering helpers in `noel-tui`,
and TypeScript parity checks in `noel-parity`.

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

Tests never touch the real `~/.noel` or `~/.pi`. `make test` sets
`NOEL_TEST_ISOLATION=1`, under which noel refuses to fall back to `~/.noel` (a
test names its agent dir with `NOEL_DIR`), imports neither `~/.pi/agent` nor a
Claude Code or Codex login from `$HOME`, and starts no background model
refresh. The CLI tests run every `noel` through `noel_command()`, which also
sets that marker and gives each run a cleared environment and its own agent
dir and `HOME` under the target directory. The tmux e2e suites run without the
marker: `scripts/e2e-isolate.sh` gives them an empty `HOME`, and the import
suite starts noel on its default directory there.

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
target/release/noel
```

Install for the current user:

```bash
make install
```

Install under a different prefix:

```bash
make install PREFIX="/opt/noel"
```

Install system-wide:

```bash
sudo make install PREFIX="/usr/local"
```

### Nix

The repository is a flake (Linux on x86-64 and arm64, macOS on Apple silicon):

```bash
nix run github:noeljackson/noel          # run noel
nix profile install github:noeljackson/noel
nix develop                              # a shell with the toolchain of rust-toolchain.toml, bun, make and tmux
```

`packages.default` is `noel` built with the Rust of `rust-toolchain.toml`, with what `make install` puts beside it: `noel-bun-host.ts` and, on Linux, the static `noel-env` daemon for remote workspaces of the same architecture (also `packages.noel-env`). `noel bun install` downloads a bun that NixOS cannot run, so the package puts nixpkgs' bun last on noel's `PATH`; `noel.override { withBun = false; }` leaves it out. `overlays.default` adds `noel` to a nixpkgs. The package does not run the tests (`make check` does), and the desktop and iOS apps are not packaged.

## Run

Interactive mode:

```bash
cargo run -p noel-cli
```

Local dogfood mode with session resume, the faux provider, and automatic
rebuild/restart on Rust source changes:

```bash
make dogfood
```

Print mode:

```bash
cargo run -p noel-cli -- -p --model faux/echo "hello"
```

RPC mode (see [RPC Mode](docs/rpc.md#rpc-mode)). `faux/echo` answers at once; with a real
model, keep stdin open until `agent_settled`, since closing it stops the run:

```bash
printf '%s\n' '{"id":"1","type":"prompt","message":"hello"}' \
  | cargo run -p noel-cli -- --mode rpc --model faux/echo
```

List models, or the ones whose provider, id, name, or API contains a text:

```bash
cargo run -p noel-cli -- --list-models
cargo run -p noel-cli -- --list-models opus
```

Choose a model and a thinking level (see [Models and thinking levels](docs/configuration.md#models-and-thinking-levels)):

```bash
cargo run -p noel-cli -- --model anthropic/claude-opus-4-8 --thinking high
cargo run -p noel-cli -- --model sonnet:high
cargo run -p noel-cli -- --provider openai --model gpt-5
```

List image-generation models:

```bash
cargo run -p noel-cli -- images
```

Generate an image through a configured OpenRouter API key:

```bash
cargo run -p noel-cli -- generate-image --output image.png \
  --model openrouter/google/gemini-3.1-flash-image-preview "a compact rust cli logo"
```

Resume or fork sessions:

```bash
cargo run -p noel-cli -- --continue
cargo run -p noel-cli -- --session <id-prefix|name|path>
cargo run -p noel-cli -- --fork <id-prefix|name|path>
```

A session has one writer at a time: opening it for writing takes a lock on
`<session>.jsonl.lock` beside the journal. Startup waits up to 5 seconds for
another noel process to release the session, then exits with status 75, naming
the session path and the holder's pid. In the TUI, `/resume`, the picker,
`/import` onto that session id, and `/delete` report the conflict and keep
running. Listing, `--fork`, `/fork`, and `/clone` only read. A crash during an
append leaves at most a torn final line, which readers ignore and the next
writer drops; a journal that fails to load is left out of listings.

Useful CLI scope flags:

```bash
cargo run -p noel-cli -- --models faux/echo --tools read,write -p "hello"
cargo run -p noel-cli -- --tools -bash,-write -p "everything but bash and write"
cargo run -p noel-cli -- --no-builtin-tools -p "extension tools only"
cargo run -p noel-cli -- --no-tools -p "no tools"
cargo run -p noel-cli -- --system-prompt prompt.md --append-system-prompt extra.md -p "hello"
cargo run -p noel-cli -- --skill ./skills/review -p "/skill:review src/"
cargo run -p noel-cli -- --image screenshot.png -p "describe this"
cargo run -p noel-cli -- --export session.json -p "hello"
cargo run -p noel-cli -- --export session.html -p "hello"
cargo run -p noel-cli -- --export session.jsonl -p "hello"
```

Prompt arguments starting with `@` are expanded from files:

```bash
cargo run -p noel-cli -- -p --model faux/echo @prompt.txt
```

### Faux models

The `faux` provider needs no credentials and is the only provider `--offline`
allows.

`faux/echo` answers `[faux/echo] ` followed by the last user message.
`NOEL_FAUX_STREAM_DELAY_MS` sleeps before each streamed event, and
`NOEL_FAUX_STREAM_CHARS=1` streams its text one character at a time.

`faux/script` replays scripted responses for tests. `NOEL_FAUX_SCRIPT` names a
JSON file that holds an array of responses. Each response is an array of steps,
and each step is an object with one key:

```json
[
  [
    {"thinking": "Check the notes first."},
    {"tool_call": {"name": "read", "arguments": {"path": "notes.txt"}}}
  ],
  [{"error": "overloaded"}],
  [
    {"delay_ms": 200},
    {"text": "The notes list alpha and beta."},
    {"usage": {"input": 120, "output": 9}}
  ]
]
```

- `{"text": "..."}` and `{"thinking": "..."}` stream text or thinking.
- `{"tool_call": {"id": "...", "name": "...", "arguments": {...}}}` calls a
  tool. `id` defaults to `call_<response>_<n>` and `arguments` to `{}`.
- `{"usage": {"input": 120, "output": 9}}` reports token usage.
- `{"stop": "..."}` sets the stop reason. Without it, a response ends with
  `tool_use` after a tool call and `stop` otherwise.
- `{"error": "..."}` fails the request with a provider error (status 503),
  which the agent retries: the retry gets the next response. After other
  steps it breaks the reply off where it is. `{"error": {"status": 401,
  "body": "..."}}` fails the request with another status and body; whether
  that is retried follows the rules for a real provider's error.
- `{"delay_ms": 200}` pauses the response.
- `{"wait_for_file": "path"}` holds the response until the file exists, for
  example while a test steers or kills noel. Relative paths resolve against the
  script's directory.

If `NOEL_FAUX_SCRIPT_LOG` names a file, every request is appended to it as one
JSON line and fsynced before the response starts. A line holds the
`system_prompt`, the `messages` (`role`, `content`, and when present
`tool_calls`, `tool_call_id`, `tool_name`, and a `media` count), and the names
of the offered `tools`. The number of requests already in the log selects the
response, so a restarted noel continues the script where the previous process
stopped. Without a log, a counter in the process selects it. A request past the
end of the script fails at once with a configuration error, which is never
retried.

```bash
NOEL_FAUX_SCRIPT=script.json NOEL_FAUX_SCRIPT_LOG=requests.jsonl \
  cargo run -p noel-cli -- --offline --no-session -p --model faux/script "read notes.txt"
```

## Configuration

Moved to [docs/configuration.md](docs/configuration.md).

## Interactive Commands

Moved to [docs/commands.md](docs/commands.md).

## Context Compaction

Moved to [docs/compaction.md](docs/compaction.md).

## Specification

`docs/spec.md` is noel's normative specification for sessions, durability, the
queue and abort, recovery, and `noel server`, `noel worker` and the clients
that attach to it. It marks what is built, what is decided and what is
proposed, and cites upstream where noel follows it.

## Server Sessions

Moved to [docs/server.md](docs/server.md).

## Remote Workspaces

Moved to [docs/remote-workspaces.md](docs/remote-workspaces.md).

## RPC Mode

Moved to [docs/rpc.md](docs/rpc.md).

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

Moved to [docs/parity.md](docs/parity.md).

## Development Notes

For TypeScript behavioral reference, check out the tip of upstream `main` from
`https://github.com/earendil-works/pi`. Active development in this repository's
`main` branch is Rust-only.

Rust live sessions use an append-only replay JSONL log. JSONL export/import and direct session open support the TypeScript v3 session-tree shape where applicable, including a `type:"session"` header and entry `id`/`parentId` chain. Full in-place active-leaf tree editing is still tracked as parity work. Legacy TypeScript session logs are not migrated automatically; use the tip of upstream `main` for reading TypeScript session behavior. `/import` also reads a Claude Code transcript (`~/.claude/projects/<folder>/<id>.jsonl`), and `/import claude` picks one of the current directory's: the import is a new session holding the conversation on the transcript's active branch since its last compaction, with Claude Code's tool names and arguments as history, without subagent records, attachments or injected messages; a turn left unfinished there is ended with a note instead of being continued. `/share` writes a local HTML export; web or gist sharing is intentionally unsupported in the Rust-only CLI.

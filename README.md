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
- Upstream's default system prompt, with `SYSTEM.md`, `APPEND_SYSTEM.md`,
  and `AGENTS.md`/`CLAUDE.md` context discovery
- Built-in tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`, `todo`, `web_search`, `web_fetch`
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

Tests never touch the real `~/.pi`. `make test` sets `PI_TEST_ISOLATION=1`,
under which pi refuses to fall back to `~/.pi/agent` (a test names its agent
dir with `PI_CODING_AGENT_DIR`), imports no Claude Code or Codex login from
`$HOME`, and starts no background model refresh. The
CLI tests run every `pi` through `pi_command()`, which also sets that marker
and gives each run a cleared environment and its own agent dir and `HOME`
under the target directory.

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

RPC mode (see [RPC Mode](#rpc-mode)). `faux/echo` answers at once; with a real
model, keep stdin open until `agent_settled`, since closing it stops the run:

```bash
printf '%s\n' '{"id":"1","type":"prompt","message":"hello"}' \
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

A session has one writer at a time: opening it for writing takes a lock on
`<session>.jsonl.lock` beside the journal. Startup waits up to 5 seconds for
another pi process to release the session, then exits with status 75, naming
the session path and the holder's pid. In the TUI, `/resume`, the picker,
`/import` onto that session id, and `/delete` report the conflict and keep
running. Listing, `--fork`, `/fork`, and `/clone` only read. A crash during an
append leaves at most a torn final line, which readers ignore and the next
writer drops; a journal that fails to load is left out of listings.

Useful CLI scope flags:

```bash
cargo run -p pi-cli -- --models faux/echo --tools read,write -p "hello"
cargo run -p pi-cli -- --tools -bash,-write -p "everything but bash and write"
cargo run -p pi-cli -- --no-builtin-tools -p "extension tools only"
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

### Faux models

The `faux` provider needs no credentials and is the only provider `--offline`
allows.

`faux/echo` answers `[faux/echo] ` followed by the last user message.
`PI_FAUX_STREAM_DELAY_MS` sleeps before each streamed event, and
`PI_FAUX_STREAM_CHARS=1` streams its text one character at a time.

`faux/script` replays scripted responses for tests. `PI_FAUX_SCRIPT` names a
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
- `{"error": "..."}` fails the request with a provider error (status 503).
  Before any output the agent retries it, and the retry gets the next response.
- `{"delay_ms": 200}` pauses the response.
- `{"wait_for_file": "path"}` holds the response until the file exists, for
  example while a test steers or kills pi. Relative paths resolve against the
  script's directory.

If `PI_FAUX_SCRIPT_LOG` names a file, every request is appended to it as one
JSON line and fsynced before the response starts. A line holds the
`system_prompt`, the `messages` (`role`, `content`, and when present
`tool_calls`, `tool_call_id`, `tool_name`, and a `media` count), and the names
of the offered `tools`. The number of requests already in the log selects the
response, so a restarted pi continues the script where the previous process
stopped. Without a log, a counter in the process selects it. A request past the
end of the script fails with a provider error, which is retried like any other;
set `"retry": {"enabled": false}` in `settings.json` to fail at once.

```bash
PI_FAUX_SCRIPT=script.json PI_FAUX_SCRIPT_LOG=requests.jsonl \
  cargo run -p pi-cli -- --offline --no-session -p --model faux/script "read notes.txt"
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
- `usage-cache.json`
- `keybindings.json`
- `SYSTEM.md`, `APPEND_SYSTEM.md`, and a context file (see
  [System prompt](#system-prompt))
- `extensions/`
- `skills/`
- `prompts/`
- `themes/`
- `sessions/`

Project skills are also discovered from `.agents/skills` directories from the
current working directory up to the git root. `SKILL.md` files are named by
their containing directory.

Project resources (`.pi/settings.json`, `.pi/SYSTEM.md`,
`.pi/APPEND_SYSTEM.md`, `.pi/extensions`, `.pi/skills`, `.pi/prompts`,
`.pi/themes`, and `.agents/skills`) load only for a trusted project: pass
`-a`/`--approve` (or `--no-approve` to refuse) or save a decision with
`/trust`. Context files (`AGENTS.md`, `CLAUDE.md`) from the agent directory
and from the current directory and its ancestors load whatever the trust
decision, as upstream does; treat a repository's instructions as untrusted
input.

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
  "enabledTools": ["read", "bash", "edit", "write", "grep", "find", "ls", "todo", "web_search", "web_fetch"],
  "sessionDir": "sessions",
  "modelRefresh": {
    "enabled": true,
    "ttlHours": 24
  }
}
```

At startup the model comes from `--model`/`--provider`, then the resumed
session, then the saved default if its provider has credentials, then a model
whose provider has credentials (preferring that provider's usual model, as
upstream does). A `faux` model is chosen only when it is requested or nothing
else is available; with `--offline` only `faux` can run, so it is chosen
unless `--model` names another.

### System prompt

Every model request starts with pi's system prompt, in every mode. It is
upstream's prompt, word for word: one sentence naming the role, then tagged
sections.

- `<tools>`: the active tools, one line each. An extension tool is listed
  when its manifest has a `promptSnippet`.
- `<rules>`: each active tool's guidelines, then two general rules. Without
  `grep`, `find`, and `ls`, a rule sends file operations to `bash`.
- `<addendum>`: appended text, when there is any.
- `<project_context>`: the context files, each inside
  `<project_instructions path="...">`.
- `<cwd>`: the working directory.

The tool list and rules follow the tools the request declares, so
`defaultTools`, `enabledTools`, `--tools`, `--exclude-tools`,
`--no-builtin-tools`, `--no-tools`, and `--offline` (which removes the web
tools) change them. An extension tool named like a built-in tool replaces it
here too: the tool appears once, with the extension's snippet and guidelines. Upstream's `<docs>` section, which
points at its installed documentation, is left out: pi installs none.

A custom prompt replaces the role, `<tools>`, and `<rules>`. The appended
text, context files, and working directory still follow it. It is the first
of these that exists:

1. `--system-prompt <text-or-path>`
2. the `systemPrompt` setting
3. `.pi/SYSTEM.md` in the working directory, if the project is trusted
4. `SYSTEM.md` in the agent directory

Appended text is the `appendSystemPrompt` setting's entries followed by each
`--append-system-prompt <text-or-path>`. Without either, it is
`.pi/APPEND_SYSTEM.md` of a trusted project, else `APPEND_SYSTEM.md` in the
agent directory. Files with the same name are not combined: the project's
replaces the agent directory's.

Context files come from the agent directory, then from each directory from
the filesystem root down to the working directory. A directory contributes
one file: `AGENTS.override.md`, else `AGENTS.md`, else `CLAUDE.md`. An
override replaces only its own directory's file. In a linked git worktree
checked out inside its main checkout, the worktree's file replaces the main
checkout's file of the same name instead of adding to it.
`--no-context-files` (`-nc`) loads none.

### Session variables

Commands the model runs with `bash` see the session they belong to:

| Variable | Value |
|---|---|
| `PI_SESSION_ID` | The session id (`ephemeral` with `--no-session`) |
| `PI_SESSION_FILE` | Absolute path of the session journal; unset with `--no-session` |
| `PI_PROVIDER` | The active model's provider |
| `PI_MODEL` | The active model's id |
| `PI_REASONING_LEVEL` | The reasoning level the turn's requests use: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` |

They are set for each command as it starts, and the system prompt tells the
model they exist. A variable with nothing to report is removed rather than
inherited, so a pi started from another pi's command never reports its
parent's session. Commands you type (`/bash`, `!`, `!!`) are not given them.

pi also marks its own process, and so every command and tool it starts:
`AI_AGENT=pi` names the agent for any tool that asks, and
`PI_CODING_AGENT=true` is pi's own marker.

### Tool selection

Every built-in tool is enabled by default. Upstream pi enables only `read`,
`bash`, `edit`, and `write`; apart from that default set, selection works as
upstream's does:

- `defaultTools` in `settings.json` selects the built-in tools. Plain names
  replace the default set; `+name` adds a tool and `-name` removes one, in
  list order. A project list of only `+name`/`-name` entries changes the
  user's list instead of replacing it. `[]` disables every built-in tool.
  Extension tools are not part of this selection and stay enabled.
- `--tools` with plain names is an allowlist over built-in and extension
  tools; `*` matches any characters (`--tools 'read,web_*'`). With only
  `+name`/`-name` entries it changes the default selection for one run
  (`--tools -bash,+grep`). The two forms cannot be mixed.
- `--exclude-tools` removes tools by name or pattern.
- `--no-builtin-tools` disables the built-in tools and keeps extension tools.
  `--no-tools` disables every tool `--tools` does not name.
- `enabledTools` in `settings.json` is a persistent allowlist, like `--tools`
  with plain names; `--tools`, `--no-builtin-tools`, and `--no-tools` replace
  it for the run. Upstream has no such setting.

The flags apply to the run they are given to and are not saved with the
session. `/reload` selects again from the reloaded settings and the same flags.

### File and shell tools

`read`, `bash`, `edit`, `write`, `grep`, `find`, and `ls` take upstream pi's
arguments and return upstream's results: limits, notices, and wording are
compared with results recorded from upstream's tools
(`tests/fixtures/ts-parity/tool-behavior.json`). A path may be relative to the
working directory, absolute, or under `~/`; as in upstream, tools are not
confined to the working directory. Differences from upstream are listed in
`docs/rust-rewrite/non-parity-register.md`.

- `read` returns at most 2000 lines or 50 KB of a text file, whichever comes
  first, and says which `offset` continues. `offset` is the first line
  (1-based) and `limit` the number of lines; an `offset` past the end is an
  error. A line longer than 50 KB is not returned: the result gives a `sed`
  command for it. An image (PNG, JPEG, GIF, WebP, or BMP, recognized by its
  content) is attached to the result; see below.
- `bash` runs `bash -c` in its own session with empty stdin: `/bin/bash`, else
  `bash` on `PATH`, else `sh`. The `shellPath` setting names another shell and
  `shellCommandPrefix` is put before every command. It is not a login shell.
  stdout and stderr come back as one stream in the order written. The model
  gets the last 2000 lines or 50 KB; longer output is also saved whole to
  `pi-bash-<random>.log` in the temp directory (readable only by you), and
  the result names that file. There is no default timeout. With `timeout`
  (seconds) the command and its process group are killed when it passes, and
  the result is the output so far followed by `Command timed out after N
  seconds`. A non-zero exit is an error result ending in `Command exited with
  code N`.
- `edit` replaces exact text: every `oldText` must occur once, the edits must
  not overlap, and all apply or none do.
- `write` creates or replaces a file, creating its parent directories.
- `grep` searches like `rg --hidden`: hidden files are searched, `.ignore` and
  `.rgignore` files apply, and so does `.gitignore` inside a git repository.
  `glob` filters the files, `ignoreCase` and `literal` change matching, and
  `context` adds lines around each match. It returns at most 100 matches
  (`limit`) or 50 KB, cuts lines at 500 characters, and lists files in path
  order. `.git/` is searched like any hidden directory.
- `find` matches a glob like `fd --glob --hidden`: against the file name, or
  against the path when the glob contains `/`. It lists files and directories
  (directories end in `/`) below `path`, applies `.gitignore`, `.ignore`, and
  `.fdignore`, ignores case unless the glob has an uppercase letter, and
  returns at most 1000 results (`limit`) or 50 KB in path order.
- `ls` lists a directory, dotfiles included and directories ending in `/`,
  sorted without regard to case, up to 500 entries (`limit`) or 50 KB.

Images that `read` returns reach the model only where the provider API carries
images in tool results:

| Provider API | Providers | Image in a `read` result |
|---|---|---|
| Anthropic Messages | Anthropic, Claude Code OAuth, Anthropic-compatible aliases | Sent in the tool result |
| OpenAI Responses | OpenAI, Codex, Azure OpenAI, GitHub Copilot | Sent in the tool result |
| Google Gemini / Vertex | Google | Sent in the tool result (Gemini 3) or the next user turn |
| Bedrock Converse | Amazon Bedrock | Sent in the tool result |
| OpenAI chat completions | OpenAI chat models, OpenRouter, Cloudflare, Mistral, and the OpenAI-compatible aliases | Not sent; the result says the image was left out |

pi's model catalog does not record which models accept images, so an image is
sent to every model behind the first four APIs. `images.blockImages` keeps
`read` from returning images at all, and `images.autoResize` (on by default)
scales an image down to at most 2000 pixels a side and 4.5 MB.

### Web tools

`web_search` returns numbered titles, URLs, and snippets; `web_fetch` retrieves
readable HTML/text/JSON/XML, retaining source URLs and links. They are enabled
by default unless `defaultTools`, `enabledTools`, `--tools`, `--exclude-tools`,
`--no-builtin-tools`, or `--no-tools` leaves them out. `--offline` /
`PI_OFFLINE=1` blocks both, including after reload.

Search uses DuckDuckGo HTML without an API key by default. For a more reliable
backend, set `BRAVE_SEARCH_API_KEY`, or set `PI_SEARXNG_URL` to the complete
search endpoint (for example `https://search.example/search`), with JSON output
enabled on that SearXNG instance. SearXNG takes precedence over Brave; keys are
only sent in the Brave request header and never included in results/errors.

Fetch limits: 30 seconds overall, at most five redirects, a 1 MiB response, and
20,000 output characters by default (`max_chars`: 1–50,000). Search accepts
`limit`: 1–10 (default 5). No JavaScript execution, browser sessions, or PDF/image
extraction. HTTP errors, CAPTCHAs, and oversized responses are surfaced, not
reported as successful empty results. Web content is labeled untrusted: treat
it as evidence, never instructions. HTTP(S) fetch permits local/private hosts
for development; this is a local harness, not a sandbox for untrusted users.

Manual commands: `/web-search <query>` and `/web-fetch <url>`.

### Terminal themes

The default `system` theme inherits your terminal's foreground, background, and
ANSI palette, including on light terminals. Secondary text uses terminal dim
intensity rather than a fixed gray. OS accent colors are not detected.

Use `/themes` to list presets and resources, `/theme` to open the selector, or
`/theme system`, `/theme light`, `/theme dark`, or `/theme kimi` to switch
immediately. `kimi` is a Kimi-inspired light preset with neutral conversation
text and restrained blue accents, not an exact reproduction. `default` is an
alias for `system`.

`/accent magenta` or `/accent #2563EB` overrides the accent; `/accent auto`
restores the preset's accent. Colors accept ANSI names (such as `cyan` or
`light-blue`), palette indices `0`–`255`, `#RRGGBB`, and `default`/`reset` for the
terminal default. Theme and accent commands save to user `settings.json`,
preserving unrelated keys:

```json
{
  "theme": "system",
  "accentColor": "cyan"
}
```

Set `accentColor` to `null` or omit it to use the theme accent. Project settings
still take precedence on startup and `/reload`. `--theme <name|path>` selects a
preset, loaded resource, or JSON file for this invocation; `--no-themes` starts
with native colors and no accent override.

Custom resources in `themes/` or trusted `.pi/themes/` use their filename stem
as the theme name. They can extend a built-in preset and override any of
`foreground`, `background`, `surface`, `muted`, `border`, `accent`, `success`,
`warning`, and `error`:

```json
{
  "base": "system",
  "colors": {
    "accent": "magenta",
    "surface": "default"
  }
}
```

Name-only legacy files still work: a built-in filename uses its preset;
other names inherit `system`. Invalid selections are rejected without replacing
saved settings. Invalid configured themes produce a diagnostic and fall back
to `system`. `/reload` picks up resource and settings edits without clearing
conversation, session identity, tool history, queued messages, or input.

Provider API keys can be stored in `auth.json`, keyed by provider and named
account:

```json
{
  "openai": { "default": { "type": "api_key", "key": "env:OPENAI_API_KEY" } },
  "openai-codex": { "default": { "type": "oauth", "access_token": "env:CODEX_ACCESS_TOKEN", "expires": 0 } },
  "anthropic": {
    "default": { "type": "api_key", "key": "env:ANTHROPIC_API_KEY" },
    "work": { "type": "api_key", "key": "env:WORK_ANTHROPIC_API_KEY" }
  },
  "azure-openai-responses": { "default": { "type": "api_key", "key": "env:AZURE_OPENAI_API_KEY" } },
  "google": { "default": { "type": "api_key", "key": "env:GEMINI_API_KEY" } },
  "google-vertex": { "default": { "type": "api_key", "key": "env:GOOGLE_CLOUD_API_KEY" } },
  "github-copilot": { "default": { "type": "api_key", "key": "env:COPILOT_GITHUB_TOKEN" } },
  "openrouter": { "default": { "type": "api_key", "key": "env:OPENROUTER_API_KEY" } },
  "deepseek": { "default": { "type": "api_key", "key": "env:DEEPSEEK_API_KEY" } },
  "groq": { "default": { "type": "api_key", "key": "env:GROQ_API_KEY" } },
  "cerebras": { "default": { "type": "api_key", "key": "env:CEREBRAS_API_KEY" } },
  "xai": { "default": { "type": "api_key", "key": "env:XAI_API_KEY" } },
  "zai": { "default": { "type": "api_key", "key": "env:ZAI_API_KEY" } },
  "huggingface": { "default": { "type": "api_key", "key": "env:HF_TOKEN" } },
  "together": { "default": { "type": "api_key", "key": "env:TOGETHER_API_KEY" } },
  "moonshotai": { "default": { "type": "api_key", "key": "env:MOONSHOT_API_KEY" } },
  "moonshotai-cn": { "default": { "type": "api_key", "key": "env:MOONSHOT_API_KEY" } },
  "opencode": { "default": { "type": "api_key", "key": "env:OPENCODE_API_KEY" } },
  "opencode-go": { "default": { "type": "api_key", "key": "env:OPENCODE_API_KEY" } },
  "vercel-ai-gateway": { "default": { "type": "api_key", "key": "env:AI_GATEWAY_API_KEY" } },
  "fireworks": { "default": { "type": "api_key", "key": "env:FIREWORKS_API_KEY" } },
  "minimax": { "default": { "type": "api_key", "key": "env:MINIMAX_API_KEY" } },
  "minimax-cn": { "default": { "type": "api_key", "key": "env:MINIMAX_CN_API_KEY" } },
  "kimi-coding": { "default": { "type": "api_key", "key": "env:KIMI_API_KEY" } },
  "xiaomi": { "default": { "type": "api_key", "key": "env:XIAOMI_API_KEY" } },
  "xiaomi-token-plan-cn": { "default": { "type": "api_key", "key": "env:XIAOMI_TOKEN_PLAN_CN_API_KEY" } },
  "xiaomi-token-plan-ams": { "default": { "type": "api_key", "key": "env:XIAOMI_TOKEN_PLAN_AMS_API_KEY" } },
  "xiaomi-token-plan-sgp": { "default": { "type": "api_key", "key": "env:XIAOMI_TOKEN_PLAN_SGP_API_KEY" } },
  "amazon-bedrock": { "default": { "type": "api_key", "key": "env:AWS_BEARER_TOKEN_BEDROCK" } },
  "mistral": { "default": { "type": "api_key", "key": "env:MISTRAL_API_KEY" } },
  "cloudflare-workers-ai": { "default": { "type": "api_key", "key": "env:CLOUDFLARE_API_KEY" } },
  "cloudflare-ai-gateway": { "default": { "type": "api_key", "key": "env:CLOUDFLARE_API_KEY" } }
}
```

Older flat files (`{ "openai": { "type": "api_key", ... } }`) are migrated on
read; writes always use the nested form. Without an explicit selection the
`default` account is used, or the sole account when only one exists.

`auth.json` is written atomically and owner-only (`0600`; a missing agent
directory is created `0700`). Logins, logouts, and OAuth refreshes hold an
advisory lock on `auth.json.flock`, re-read the file, and change only their
own entry, so concurrent pi processes never drop each other's credentials or
rotated refresh tokens. A `--api-key` value is used for that run only and is
never written.

Or use the CLI helper:

```bash
pi login anthropic --api-key env:ANTHROPIC_API_KEY
printf '%s' "$ANTHROPIC_API_KEY" | pi login anthropic --api-key -
pi login anthropic --account work --api-key env:WORK_ANTHROPIC_API_KEY
pi logout anthropic
pi logout anthropic --account work
```

Native OAuth enrollment is available for ChatGPT/Codex and Claude using the
official CLIs' public clients:

```bash
pi login openai-codex              # device authorization flow (browser + one-time code)
pi login openai-codex --account work
pi login anthropic                 # PKCE authorize + paste-code flow
pi login anthropic --account work
```

`pi login openai-codex` stores the OAuth credential for both `openai` and
`openai-codex` (mirroring the codex import mapping); `pi login anthropic`
stores it for `anthropic`. Expiry and `account_id` come from the issued token
(JWT `exp` for Codex, `expires_in` for Claude), and the stored credentials
refresh automatically when they expire, also in the middle of a turn: every
model request resolves them anew. Importing credentials from
`~/.codex/auth.json` and `~/.claude/.credentials.json` still works as a
fallback when no stored account exists. Re-running login for an existing
account replaces it.

When no explicit API key is configured, `pi` can reuse existing CLI login credentials:

- Claude Code: `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or `~/.claude/.credentials.json`
- Codex/ChatGPT: `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json` for `openai` and `openai-codex`

Explicit API keys still take precedence over login tokens. Imported OAuth
credentials are refreshed automatically when they expire; refreshed tokens are
persisted to pi's own `auth.json` as `codex-import`/`claude-import` accounts
and never written back to the imported files.

`/account [name]` binds an account to the current session (bare `/account`
opens a selector; the binding is journaled and survives resume). `pi accounts
status` and `/accounts` show auth state, plan windows, and balance for every
configured account, including environment and imported pseudo-accounts; results
are cached for 15 minutes in `usage-cache.json` (`--refresh` bypasses the
cache).

For scripts, `pi auth check --provider <p>` (or `--model <ref>`) reports whether
credentials are ready (exit 0 ready, 1 not ready, 2 invalid; `--json`,
`--no-refresh`, `--account <name>`). As upstream, an unknown subcommand or
option exits 1, while other argument errors exit 2 for `check`. A ready OAuth
token that expires within five minutes gets `"expiresSoon": true` with
`--json`, or a warning on stderr in text output. `pi auth print-api-key` and
`pi auth print-bearer-token` write a usable credential to stdout; the bearer
form first refreshes OAuth tokens that expire within `--min-expiry` (default
30m). With `--account`, `--model` resolves to the providers that have that
account. Logins read from `~/.claude` or `~/.codex` are the exception to early
refresh: refreshing one spends that CLI's single-use refresh token and signs it
out, so `pi auth` refreshes them only once they expire, as requests do, and
prints or reports a token that expires sooner with a warning on stderr. pi's
own accounts, including saved `claude-import`/`codex-import` copies, refresh
early. pi has no MCP support, so `--no-mcp` is accepted and does
nothing, and `pi mcp` exits with an error.

Provider-specific environment:

- Azure OpenAI: set `AZURE_OPENAI_BASE_URL` or `AZURE_OPENAI_RESOURCE_NAME`; optionally set `AZURE_OPENAI_DEPLOYMENT_NAME`, `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`, and `AZURE_OPENAI_API_VERSION`.
- Google Vertex: set `GOOGLE_CLOUD_PROJECT` or `GCLOUD_PROJECT`, plus `GOOGLE_CLOUD_LOCATION`.
- Cloudflare: set `CLOUDFLARE_ACCOUNT_ID`; AI Gateway also needs `CLOUDFLARE_GATEWAY_ID`.
- Amazon Bedrock: Rust direct calls currently use Bedrock bearer-token auth via `AWS_BEARER_TOKEN_BEDROCK`.
- Claude Code OAuth: requests identify as `claude-cli/<version>`. The version tracks the latest published Claude Code release (fetched from the npm registry during model refresh, cached in `model-cache.json`); set `PI_CLAUDE_CODE_VERSION` to pin it.

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
provider has supported auth. `/reload` also kicks a refresh. Refreshed models
are available after `/reload` or the next startup. Refresh covers `anthropic`,
`openai`, and `openai-codex` (API key or OAuth login), plus the
OpenAI-compatible `zai`, `zai-coding`, `moonshotai`, and `kimi-coding-openai`
endpoints via their API keys. Anthropic refresh paginates the official Models
API; OAuth refresh is best-effort against the same provider auth paths used
for model requests.

Refresh is defensive about partial failures. A provider that returns an empty
list keeps its cached models instead of wiping them, an exhausted pagination
budget or failed fetch records a diagnostic in the cache instead of silently
stopping, and a pass where every fetch fails leaves `refreshedAt` untouched so
the next startup retries instead of waiting out the full TTL. Diagnostics
surface in `/diagnostics` output.

Two slash commands manage the cache directly:

- `/models refresh` — refetch now (skipped in offline mode or when
  `modelRefresh.enabled` is false), reload config, and print a per-provider
  report (`refreshed: zai-coding (11 models)`, `failed: ...`).
- `/models reset` — delete `model-cache.json` and reload. The next refresh
  starts from built-in models plus explicit `models.json` entries only; run
  `/models refresh` to repopulate.

Thinking levels can be set with `--thinking <level>` or `/thinking <level>`.
Supported levels are model-specific. Opus exposes `high`, `xhigh`, and `max`;
OpenAI/Codex reasoning models expose `minimal` through `xhigh`. While the
`/model` selector is open, left/right adjusts the pending thinking level and
Enter applies both the model and thinking level to the session. `/new` starts
fresh conversation history while retaining the current model, thinking level,
and account selection; these selections are persisted in the new session.
`/thinking off` remains explicitly off rather than inheriting a default.

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
response with `output` or `error`. Like `bash`, a tool process has no
controlling terminal, and an interrupted call kills its process group. A
non-zero exit is a failed call: the model gets what the extension printed as
an error result. Output beyond 2000 lines or 50 KB is cut there and saved
whole to a temp file the result names. A tool named like a built-in tool
replaces it: the model is offered the extension's definition only. A
call that a stop cut off never runs again when its turn continues unless the
tool declares `"replaySafe": true` (it only reads, so running it twice is
harmless). `promptSnippet` is the tool's line in the system prompt's tool
list, and `promptGuidelines` are rules added to the prompt while the tool is
active; both are optional, and a tool without a snippet is not listed:

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
      },
      "replaySafe": true,
      "promptSnippet": "Echo text back",
      "promptGuidelines": ["Use fixture_echo only in tests."]
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
- `/clear`
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
- `/accent <color|auto>`
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
- `/resume` (no argument opens the interactive session picker: topic
  preview, relative age, message count, model, and an unfinished-turn
  marker for sessions that died mid-turn; type to filter, enter to resume,
  esc to cancel)
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
- `/compact [focus]` (model-generated checkpoint, retaining complete recent turns)
- `/undo` (remove the latest conversation turn and restore its prompt; files unchanged)
- `/usage` (recorded session token totals and account quotas)
- `/login [provider]`
- `/logout <provider>`
- `/account [name]`
- `/accounts`
- `/todos`
- `/diff`
- `/bug [description]` (writes a local `pi-bug-report-<id>.md` with environment,
  model, auth type, redacted settings and session errors (URL secrets, `Bearer`
  and `Basic` credentials, and API-key-shaped tokens redacted), never the
  transcript, and prints a link for filing it; nothing is uploaded)
- `/reload`
- `/web-search <query>`
- `/web-fetch <url>`
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

`/reload` reloads config, prompts, context files, model metadata, keybindings, provider availability, and tool definitions without clearing the current session state. It keeps the session's project trust decision and command-line overrides (such as `--system-prompt`, `-e`, `--tools`, and `--api-key`), as do `/models refresh` and `/models reset`. RPC mode has no reload: it keeps the config startup built.

The editor uses a rounded box with a `>` prompt. A labeled Todo section and
queued-message previews sit above it, separated by muted rules. Drafts wrap
and the box grows upward with newlines or wrapping, up to ten visible input
rows; longer drafts scroll vertically to keep the cursor visible. Shift+Enter
inserts a newline without submitting (also while streaming); Enter submits or
queues the complete draft. Ctrl+J is an alternative when the terminal does not
report Shift+Enter distinctly. Multiline pastes (or single-line pastes over
1,000 characters) collapse to numbered markers such as
`[paste #1 +500 lines]`. Left/Right skip a marker and
Backspace/Delete remove the whole paste. Submission, queueing, history, and
external editors retain the full text, not the marker. Narrow/short terminals
reduce panel rows and report the actual hidden counts.

The model can track multi-step work with the built-in `todo` tool: it maintains a checklist (`pending`/`in_progress`/`completed`) that is journaled with the session and survives resume. The list renders above the prompt: collapsed lists select active tasks, upcoming tasks, and the most recent completed item while preserving task order. Completed titles are muted and struck through; hidden counts describe only hidden tasks. `ctrl+t` expands/collapses the list and survives updates; `/todos` reprints it in the transcript. Files the model changes with the `edit`/`write` tools are tracked per session; `/diff` toggles a right-side panel listing them with git change stats, and Enter on a file shows its uncommitted diff (new files show their content).

Collapsed thinking and tool blocks use at most three terminal rows (excluding the blank separator). Thinking shows the latest two wrapped lines while streaming, then the first two lines plus a clipped hidden-line/`ctrl+o` hint when finished. Tool headers stay on one row with long commands/paths clipped; short results remain visible, while longer results show a hidden-line count in the header and an ellipsis-prefixed preview of the final output line. `ctrl+o` expands/collapses thinking and tool/command output for the latest three user turns, including while a turn is running; a new prompt or shell command restores collapsed previews. Expansion restores full commands and output. This only changes display, not stored content or model context usage; `expand` is configurable in `keybindings.json`. Tool headers highlight only tool names (or the command phrase), with muted paths/counts and success/failure markers.

Queued follow-ups are visible immediately above the editor, one truncated line per message (the newest four when the queue overflows). Up with an empty editor recalls the newest queued message for editing (otherwise prompt-history navigation is unchanged). Mid-turn `ctrl+s` steers queued messages followed by the draft at the next provider boundary (a prose-only response has no interior boundary, so steering is delivered after it finishes and the queue hint says so); the existing steering mode still controls batch size. Queue edits are journaled during streaming and survive reload/resume.

Interactive assistant responses stream text as provider deltas arrive. While a response streams, a footer spinner shows the current phase (waiting, thinking, writing, or the running tool) with elapsed time, ctrl+c interrupts the turn unless transcript text is selected (then it copies); Esc interrupts only at the transcript bottom without a selection, Enter queues the draft as a follow-up, Up recalls the newest pending follow-up for editing, and ctrl+s steers the running turn (including promoting queued drafts): everything queued joins the conversation before the next provider request, combined into one user message so a burst of steering reads as a single instruction rather than several interruptions. Slash commands typed mid-turn still execute: `/queue` lists pending follow-ups, `/queue-clear` clears them, `/interrupt` and `/quit` act on the turn. `/queue <prompt>` adds follow-up prompts that run after the next assistant turn. Completed turns drain queued prose as one combined turn (queued tool commands such as `/read` still dispatch individually because their arguments are positional), including prompts queued during a follow-up, so nothing waits for a manual resubmission. Interruption, quit, or failure stops automatic dispatch and preserves remaining work. At idle, `/interrupt` clears queued follow-ups, and `!`/`!!` execute shell commands without adding them to the conversation context. Shell commands (`!`, `/bash`, and the model's `bash` tool) are non-interactive: stdin is empty and the command runs in its own session without a controlling terminal, so a program that prompts for input fails instead of waiting. A command that times out or whose turn is interrupted is killed together with its whole process group, and a timeout returns the output so far; background jobs started by a command that finished normally keep running, and the command returns without waiting for them. `!` and `/bash` commands time out after 120 seconds; the model's `bash` calls have no timeout unless the model sets one. Editing keys follow common agentic-CLI conventions: shift+enter or ctrl+j inserts a newline, ctrl+g opens the draft in an external editor (`PI_EDITOR_COMMAND`, `VISUAL`, or `EDITOR`), ctrl+c with a draft clears it, and ctrl+c/ctrl+d on an empty input require a second press to quit. Manual and automatic compaction persist summary records, and forked or cloned sessions persist branch summaries. Editor state tracks history, undo, kill-ring, and slash completions; restored session user prompts repopulate prompt history.

The TUI defaults to an owned alternate-screen transcript with a fixed composer. Mouse wheel and PageUp/PageDown scroll only the conversation; Up/Down retain prompt-history/queue navigation. At the bottom, new output follows automatically. Scrolling up anchors the reader to logical text: streaming, resizing, panel changes, and reload do not steal the reading position. `↓ Scroll to bottom · Esc` appears while reading (with `new output` when text arrives); click it, press Esc, or scroll down to resume following. Live thinking-tail previews are frozen while reading; returning to latest refreshes them. Esc dismisses an open selector/diff panel first, otherwise returns to latest without clearing the draft/attachments or interrupting generation. At latest it retains the existing clear/interrupt behavior. Bracketed paste inserts pasted text into the prompt. Image inputs are encoded as provider attachments with terminal text fallback.

In owned mode, drag with the left mouse button to select transcript text;
release copies it to the clipboard. Selection stays attached to logical text
while scrolling, resizing, and receiving streamed output. Drag past the transcript
edge to scroll, or use the wheel/PageUp/PageDown. Soft wraps copy as one line;
folded blocks copy only their visible preview (expand with Ctrl+O first to copy
all output; preview selection clears if the preview changes). Ctrl+C or `/copy`
copies the selection again without clearing the draft or interrupting generation. Esc clears selection and returns to latest;
clicking the composer clears selection without changing the draft.

Clipboard writes use `pbcopy` on macOS, `wl-copy`/`xclip`/`xsel` on Linux, or
`clip.exe` on Windows. `PI_CLIPBOARD_COMMAND` overrides the helper. If local
helpers are unavailable, Pi requests the terminal clipboard via OSC 52 (useful
over SSH; terminal/tmux clipboard permissions must allow it). An OSC 52 request
cannot confirm delivery. Copy errors appear in the transcript hint; selection
remains available for retry. Without a selection, `/copy` copies the last assistant
message. Terminal-native selection via Shift+drag (sometimes Option on macOS)
remains available; `/export <file>` exports the conversation.

Run `pi --no-alt-screen` (or upstream's `--tui-mode regular`) for native inline
compatibility: overflow is written once to terminal scrollback, mouse
selection/wheel remain terminal-owned, and the composer scrolls with the
terminal. `--tui-mode fullscreen` selects the owned screen explicitly.
`--use-theme <name>` applies a theme for one run without saving it; it cannot
be combined with `--theme` or `--no-themes`.
Print/JSON/RPC modes are unchanged.
Owned mode restores the previous screen, mouse reporting, paste, and raw mode
on exit. This viewport is an intentional Rust UI divergence; upstream parity
fixtures are not rewritten.

SIGTERM and SIGHUP end pi in every mode, as does SIGINT outside the TUI (print,
JSON, RPC, and login); in the TUI ctrl+c stays a key and an outside SIGINT is
ignored. pi first kills running tool commands with their process groups and
restores the terminal, then exits like upstream pi: with status 128 + the
signal number for SIGTERM and SIGHUP, and by the signal itself for SIGINT, so
ctrl+c also stops a shell loop that runs pi.

Ctrl+V reads a clipboard **image** into the current draft; type a prompt and
press Enter to send it. Image-only drafts use “Describe this image.” Esc or
Ctrl+C clears the draft when idle unless transcript text is selected (Esc first
clears selection/returns to bottom if scrolled up). Multiple images are supported. `/image <path>`
without a prompt also stages an attachment. Bracketed text paste is unchanged.
Images respect `images.blockImages` and `images.autoResize`. While a turn runs,
image drafts are kept for submission after it finishes (not sent as text-only
steering/follow-ups). Linux needs `wl-paste` (Wayland) or `xclip` (X11); macOS
uses AppKit via Swift (Xcode command-line tools), and Windows uses PowerShell.
For remote/headless sessions, `PI_CLIPBOARD_IMAGE_COMMAND` can supply encoded
PNG/JPEG/GIF/WebP bytes on stdout; otherwise use `/image <path>`.

`/undo` changes only conversation context and restores the prompt, including
images. It does **not** revert file edits, shell effects, tool audit history,
queued messages, or token usage. `/usage` persists provider-reported token totals
for completed requests across resume/reload/compaction/undo; old sessions and
providers without usage report “no provider token usage reported yet” rather
than estimated totals. Account quota checks use the same cache as `/accounts`
and are skipped offline. Dollar costs are not calculated.


## Context Compaction

`/compact preserve architecture decisions and outstanding test failures` asks the
active model for a continuation checkpoint with tools disabled. The summary
preserves goals/constraints, decisions, completed work/files, errors, next steps,
and references. Recent complete user turns remain unchanged, including paired
tool calls/results and attachments. Persistent system/project instructions are
not summarized and remain loaded independently. Prior checkpoints are merged on
subsequent compactions, rather than nested first/last snippets.

Automatic compaction runs before every model request, including tool rounds and
queued prompts. It uses **estimated active-context tokens**, including instructions,
tool schemas, calls, and attachment allowances—not billed session totals or message
count. Configure `.pi/settings.json` or global settings:

```json
{
  "compaction": {
    "enabled": true,
    "triggerPercent": 80,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  }
}
```

The trigger fires at the percentage threshold or when the response reserve is
needed. `triggerPercent` is clamped to 1–99; the reserve is capped at half the
window. The recent-token budget is capped at a quarter-window and applies to
older complete turns; the latest turn is always retained intact. Oversized
histories are summarized in bounded rolling fragments without dropping text.
If automatic compaction cannot get under the budget (for example a very large
system prompt, tool set, or persistent instructions, or a single turn larger
than the window), the turn continues with the original history instead of
failing: the failure is reported in the transcript with a token breakdown
(`system prompt`/`tools`/`persistent instructions`/`retained recent turns`),
and re-attempts are skipped until history changes. The provider still returns
a real error if the request genuinely exceeds its window.
Known model windows come from the catalog/provider metadata; custom `models.json`
entries can specify `contextWindow`. Unknown windows fall back to 128,000 tokens.
Token counts are heuristics, not exact provider tokenization.

The agent per-run round cap is configurable alongside compaction and retry:

```json
{
  "agent": {
    "maxToolRounds": 120
  }
}
```

Provider requests retry automatically on transient transport errors
(default 3 retries, exponential backoff from 2s; `retry.enabled`,
`retry.maxRetries`, `retry.baseDelayMs` in settings). When retries are
exhausted the error names the likely causes — including a local firewall
that denies connections from a freshly built binary until its prompt is
approved — because on default-deny hosts an unanswered prompt is
indistinguishable from an outage.
```

`agent.maxToolRounds` bounds provider rounds per user run, including steering
rounds. The default is 200; values are clamped to at least 1. When the cap is
reached the turn stops with a round-limit error, all work stays saved, and
sending any prompt (or restarting, which auto-continues) gets a fresh budget.

The original transcript remains in the session journal. A validated, smaller
checkpoint and its replacement context are committed together through an atomic
file replacement. Empty/truncated/oversized/tool-bearing summaries, provider
failures, or cancellation leave active history intact; no silent truncation
fallback is used. If the latest turn or persistent instructions still cannot fit,
pi reports an error instead of repeatedly compacting. Completed summarization
requests count toward `/usage`. `/export` exports the current checkpoint and
retained context, not the pre-compaction journal transcript.


## RPC Mode

`pi --mode rpc` speaks upstream pi's RPC protocol (upstream's `docs/rpc.md`,
`docs/rpc-commands.md` and `docs/json.md`): one JSON command per line on
stdin, and responses and session events as one JSON object per line on stdout.
Lines split on LF only: a trailing CR is dropped, and U+2028 or U+2029 inside a
string is not a line break. stdout carries only protocol records; diagnostics
go to stderr. RPC mode starts when stdin is not a terminal and takes no prompt
arguments.

Commands: `prompt` (with `images`, `streamingBehavior`, and `requestId`),
`steer`, `follow_up`, `abort`, `resume_run`, `clear_queue`, `get_state`,
`get_messages`, `get_last_assistant_text`, `new_session`, `switch_session`,
`set_model`, `set_thinking_level`, `set_steering_mode`, `set_follow_up_mode`,
`compact`, and `set_session_name`. Other upstream commands answer
`Unsupported command: <type>`, and unknown types `Unknown command: <type>`.

A `prompt` that starts a run answers `{"disposition":"started","runId":"…"}`,
then the run reports `agent_start`, its turns (`turn_start` … `turn_end`),
`agent_end`, and exactly one `agent_settled` with `aborted` and the `runId`,
after an abort or a failure too; a failed request ends with an assistant
message whose `stopReason` is `error`. Follow-ups queued during a run are
delivered in it, after the turn that would otherwise end it. `abort` answers
once the run has settled, or at once when no run is active, and leaves queued
messages for the next prompt. While a run is active, a `prompt` needs
`streamingBehavior` (`steer` or `followUp`); `set_model`,
`set_thinking_level`, `compact`, `new_session`, `switch_session`,
`set_session_name`, and `resume_run` are refused; and `get_state`,
`get_messages`, and `get_last_assistant_text` answer from the run's current
state. `set_model` changes the session's model, not the default model in
`settings.json`.

pi adds four extensions, listed in `get_state.rpcExtensions`:

- `run-id`: every run has a `runId` (a UUID), given in the `prompt` response,
  `agent_start`, `agent_settled`, and `get_state` while the run is active.
- `expected-run-id`: `steer`, `follow_up`, and `abort` take an optional
  `expectedRunId`. Unless it names the active run, the command fails and
  changes nothing.
- `request-id`: `prompt` takes an optional `requestId`. A prompt that repeats
  one the session knows starts nothing and answers with that run's `runId`:
  `{"disposition":"started"}` while the run is running or interrupted, and
  `{"disposition":"handled"}` once it completed, failed, or was aborted. A
  queued prompt (`streamingBehavior` during a run) cannot have one.
- `resume-run`: `get_state.lastRun` describes the session's last run, and
  `resume_run` continues it after a stop interrupted it.

Runs are saved in the session journal: a run's start together with its
prompt before `prompt` answers, and how it ended before `agent_settled`. A run
that started and never ended is `interrupted`: pi crashed, was killed or
ended by a signal, or saw stdin close mid-run. pi never resumes a run by
itself.

`get_state.lastRun` is `{runId, requestId, status, phase, pendingTools}`,
without the fields that do not apply, and absent while the session has no run.
`status` is `running`, `interrupted`, `completed`, `failed`, or `aborted`. For
an interrupted run, `phase` is `model` when the saved context needs a model
request and `tools` when the last reply has tool calls without results, which
`pendingTools` lists as `{id, name, replaySafe}`. Without a `phase`, the run
stopped after its last reply was saved, and resuming it just completes it.

`resume_run {expectedRunId}` fails unless `expectedRunId` names the last run
and that run is interrupted. It answers `{"disposition":"started","runId":…}`,
and the run reports `agent_start` through `agent_settled` under the same
`runId`. A tool call without a result runs again only if it is replay-safe:
`read`, `grep`, `find`, `ls`, and extension tools whose manifest declares
`"replaySafe": true`. Those run first, in the turn of the reply that called
them, which is reported again as saved. Every other call (`bash`, `edit`,
`write`, `todo`, the web tools, other extension tools) never runs again, and
the model is told it was interrupted. A model request the stop cut off is sent
again, and billed again. So a prompt is applied once, a call that is not
replay-safe runs at most once, and a replay-safe one at least once.

With no run active, `abort` ends an interrupted last run as `aborted` (with
`expectedRunId`, only the run it names), and a new `prompt` ends it the same
way before it starts. So a supervisor that restarts reads `lastRun`, then
resumes the run it started, aborts it, or reads how it ended.

The queue is saved with the session. `steer`, `follow_up`, and queued
prompts are saved before they are acknowledged; a delivered message leaves
the saved queue once its user message is saved; `clear_queue` saves the empty
queue. RPC mode loads the saved queue at startup and on `switch_session`, so
`pendingMessageCount` counts it; `new_session` starts empty. The TUI shares
the saved queue and delivers saved steering as follow-ups.

Closing stdin while no run is active exits with status 0. Closing it during a
run stops the run where it is, kills its tool processes, and exits with status
0 without `agent_settled`; the run stays interrupted.

Differences from upstream are listed under "RPC Mode" in
`docs/rust-rewrite/non-parity-register.md`.
`crates/pi-cli/tests/rpc_parity.rs` runs every scenario of
`tests/fixtures/ts-parity/rpc-protocol.json` and compares the records with
upstream's, allowing exactly those differences.

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
where Rust intentionally supports compatibility or analogous behavior. The RPC
protocol fixture records every stdout record of upstream `pi --mode rpc` for
scripted runs against a local stub model, and pi's RPC mode is compared with it
record by record. The system prompt fixture records upstream's prompt text
for fixed tools, prompt sources, and context files, and which prompt and
context files upstream picks from a directory tree; pi's prompt is compared
with it to the byte. Secrets are redacted. Rust tests
compare covered behavior against those fixtures, so upstream changes become
visible as fixture diffs and test failures.

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
rebuilds restart the running TUI process into the session it had open
(`--session <file>` replaces the original session flags, so a restart never
jumps to a newer session or forks again), which restores conversation
messages from disk. Restarts wait until the active turn and its
follow-ups finish — or until you are idle between prompts with nothing
streaming — so a rebuild never interrupts work or lands under your fingers
while typing. Interactive startup and `/resume` automatically continue
unfinished model turns from saved context, then drain queued follow-ups. No
synthetic user prompt is added, and saved results are reused. A tool call
without a result runs again only if it is replay-safe (`read`, `grep`, `find`,
`ls`, and extension tools whose manifest declares `"replaySafe": true`); any
other call never runs again and is reported to the provider as interrupted.
Completed direct
slash tools do not trigger continuation. Ctrl+C/Esc, `/quit`, and errors still
stop the active run; rebuild restarts wait for a completed boundary so a stop
cannot turn into an automatic restart/retry loop. The agent safety limit is
200 provider rounds per run by default, including steering; raise or lower it
with `agent.maxToolRounds` in `settings.json`. Hitting the limit reports the
cap and saves all work; a resumed run gets a fresh
budget without discarding the saved work.
Watcher output is written to
`target/dogfood-dev/watch.log`. `make dogfood-release` builds
`target/release/pi` and runs the binary in tmux with an isolated agent/session
directory under `target/`. It uses the faux provider, so it does not require
provider credentials or network access. `make dogfood-long` uses the same
release binary and faux provider in `--no-alt-screen` compatibility mode: it
creates a long transcript, checks terminal scrollback, resizes the tmux pane,
and verifies the
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

For TypeScript behavioral reference, check out the tip of upstream `main` from
`https://github.com/earendil-works/pi`. Active development in this repository's
`main` branch is Rust-only.

Rust live sessions use an append-only replay JSONL log. JSONL export/import and direct session open support the TypeScript v3 session-tree shape where applicable, including a `type:"session"` header and entry `id`/`parentId` chain. Full in-place active-leaf tree editing is still tracked as parity work. Legacy TypeScript session logs are not migrated automatically; use the tip of upstream `main` for reading TypeScript session behavior. `/share` writes a local HTML export; web or gist sharing is intentionally unsupported in the Rust-only CLI.

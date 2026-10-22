# noel

`noel` is a native Rust CLI coding agent.

The repository has been cut over from the previous TypeScript/npm monorepo to a Rust-only Cargo workspace. The web UI and npm package runtime are no longer part of the product path.

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
- Built-in tools: `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`, `todo`, `web_search`, `web_fetch`
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

RPC mode (see [RPC Mode](#rpc-mode)). `faux/echo` answers at once; with a real
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

Choose a model and a thinking level (see [Models and thinking levels](#models-and-thinking-levels)):

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

Default config root:

```text
~/.noel
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

Skills are also read from `.agents/skills` directories; see [Skills](#skills)
and [Prompt templates](#prompt-templates) for what `skills/` and `prompts/`
hold.

Project resources (`.noel/settings.json`, `.noel/SYSTEM.md`,
`.noel/APPEND_SYSTEM.md`, `.noel/extensions`, `.noel/skills`, `.noel/prompts`,
`.noel/themes`, and `.agents/skills`) load only for a trusted project: pass
`-a`/`--approve` (or `--no-approve` to refuse) or save a decision with
`/trust`. Context files (`AGENTS.md`, `CLAUDE.md`) from the agent directory
and from the current directory and its ancestors load whatever the trust
decision, as upstream does; treat a repository's instructions as untrusted
input. A project without a `.noel/` directory is read from its `.pi/`
instead; see [Moving from pi](#moving-from-pi).

Environment overrides:

- `NOEL_DIR`
- `NOEL_SESSION_DIR`

`settings.json` supports default model selection, shell configuration, prompt inputs, enabled models/tools, and `sessionDir`:

```json
{
  "defaultProvider": "faux",
  "defaultModel": "echo",
  "defaultThinkingLevel": "high",
  "modelThinkingLevels": { "anthropic/claude-opus-4-8": "xhigh" },
  "enabledModels": ["faux/echo", "anthropic/claude-*:high"],
  "enabledTools": ["read", "bash", "edit", "write", "grep", "find", "ls", "todo", "web_search", "web_fetch"],
  "sessionDir": "sessions",
  "modelRefresh": {
    "enabled": true,
    "ttlHours": 24
  }
}
```

### Moving from pi

noel keeps pi's layout under new names: `~/.pi/agent` is `~/.noel`,
`PI_CODING_AGENT_DIR` is `NOEL_DIR`, `PI_CODING_AGENT_SESSION_DIR` is
`NOEL_SESSION_DIR`, the other `PI_*` variables are `NOEL_*`, and the project
directory is `.noel/`. noel reads only the new names: a `PI_*` variable set
for pi has no effect.

The first command that loads noel's configuration without `NOEL_DIR`, if
`~/.noel` does not exist and `~/.pi/agent` does, imports pi's state once.
`noel --version`, `noel --help`, and a command line noel rejects write
nothing, so a version probe never imports. The import takes settings,
`trust.json`, `keybindings.json`, `models.json`, `auth.json`, `SYSTEM.md`,
`APPEND_SYSTEM.md`, the global context file (`AGENTS.override.md`,
`AGENTS.md` or `CLAUDE.md`), skills, prompts, themes, extensions and
sessions, so `noel -c` and `/resume` find pi's sessions. It prints what it
imported and never modifies `~/.pi`; caches are rebuilt. A `models.json` or
`keybindings.json` that noel cannot load is left behind with a notice. The
copy is staged next to `~/.noel` and renamed into place, so an interrupted
import never leaves a partial `~/.noel`, and the next start imports again.

OAuth refresh tokens rotate: once noel refreshes an imported login, only noel's
copy stays valid, so pi on the same host (upstream, or an earlier build of this
port) has to log in again.

Projects written for pi keep working. Without a `.noel/` directory, noel uses
the project's `.pi/` for everything a project configures: its settings,
`SYSTEM.md`, `APPEND_SYSTEM.md`, extensions, skills, prompt templates and
themes, including what noel writes there (`noel install -l`, `noel config ...
-l`). Once the project has a `.noel/`, its `.pi/` is not read at all. noel
also reads `.pi-extension.json` manifests and the `pi` key of a package's
`package.json`, gives extensions both the `NOEL_EXTENSION_*` and the
`PI_EXTENSION_*` variables, and sets the [session variables](#session-variables)
under pi's names too, for skills and scripts written for pi.

### Models and thinking levels

noel knows the models upstream pi knows. The built-in catalog is upstream's,
generated from upstream's own model runtime and compiled in
(`tests/fixtures/ts-parity/model-catalog.json`; `make ts-parity-update`
regenerates it). Each model has its API and base URL, context window, output
cap, what it accepts (`text`, `image`), prices, and which thinking levels it
supports. Beside it sit the providers only this port has: `faux`,
`zai-coding`, `zai-anthropic`, and `kimi-coding-openai`. The catalog is a
snapshot: noel does not fetch upstream's hosted catalog or OpenRouter's
per-key listing, and it leaves out upstream's Radius gateway, whose API it
does not speak. Provider listings fetched by the model refresh and
`models.json` add to it (see below).

`--model <pattern>` picks a model as upstream's does:

- `provider/id` or a bare `id`, ignoring case. An id several providers have
  goes to the one provider with credentials; with none or several, noel names
  the candidates and asks for `--provider` or `provider/id`.
- Otherwise the best match on id or name: an alias before a dated id
  (`claude-haiku-4-5` before `claude-haiku-4-5-20251001`), then the id that
  sorts last. The match runs over every provider unless `--provider` or a
  `provider/` prefix narrows it, so `--model sonnet` may pick a provider you
  have no key for.
- `:<thinking>` at the end sets the thinking level: `--model sonnet:high`.
  `--thinking` overrides it.
- An id the provider does not list, as in `--model anthropic/new-id`, becomes
  a custom model built on that provider's default model, with a warning.
- `--provider <name>` narrows `--model` to one provider and requires it.

`--models <patterns>` and the `enabledModels` setting scope model selection to
the models with credentials that match: exact references, fuzzy matches, or
globs over `provider/id` and over the id (`anthropic/*`, `*sonnet*`,
`openrouter/**`; `*` does not cross a `/`), ignoring case, each optionally
ending in `:<thinking>`. A scope removes no model: a new session starts on the
saved default if it is in scope, else on the scope's first model, the model
picker lists the scope, and `--model` and `--list-models` still see
everything. A pattern that matches nothing prints a warning.

At startup the model comes from `--model`, then the resumed session, then the
scope, then the saved default if its provider has credentials, then a model
whose provider has credentials (preferring that provider's usual model, as
upstream does). A saved default or a session's model that the catalog no
longer lists keeps working: it runs as a custom id on its provider's default
model. A `faux` model is chosen only when it is requested
or nothing else is available; with `--offline` only `faux` can run, so it is
chosen unless `--model` names another.

Thinking levels are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and
`max`. Which of them a model supports comes from its catalog entry, and a
level the model lacks becomes the nearest one it has (the next higher one
first), as upstream clamps it; a model without reasoning is always `off`. A
session starts on `medium` unless `--thinking`, a `:<thinking>` suffix, the
model's entry in `modelThinkingLevels`, or `defaultThinkingLevel` says
otherwise, in that order. Switching models keeps the level in effect, unless
`modelThinkingLevels` or `defaultThinkingLevel` sets one for the new model.
Requests carry the level as the model's catalog entry names it, and the
model's output cap, lowered to fit its context window.

### System prompt

Every model request starts with noel's system prompt, in every mode. It is
upstream's prompt, word for word under noel's names (the product in the first
sentence, and the `NOEL_*` variables of bash's rule): one sentence naming the
role, then tagged sections.

- `<tools>`: the active tools, one line each. An extension tool is listed
  when its manifest has a `promptSnippet`.
- `<rules>`: each active tool's guidelines, then two general rules. Without
  `grep`, `find`, and `ls`, a rule sends file operations to `bash`.
- `<addendum>`: appended text, when there is any.
- `<project_context>`: the context files, each inside
  `<project_instructions path="...">`.
- `<skills>`: the loaded skills, while `read` or `bash` is active (see
  [Skills](#skills)).
- `<cwd>`: the working directory.

The tool list and rules follow the tools the request declares, so
`defaultTools`, `enabledTools`, `--tools`, `--exclude-tools`,
`--no-builtin-tools`, `--no-tools`, and `--offline` (which removes the web
tools) change them. An extension tool named like a built-in tool replaces it
here too: the tool appears once, with the extension's snippet and guidelines. Upstream's `<docs>` section, which
points at its installed documentation, is left out: noel installs none.

A custom prompt replaces the role, `<tools>`, and `<rules>`. The appended
text, context files, skills, and working directory still follow it. It is the
first of these that exists:

1. `--system-prompt <text-or-path>`
2. the `systemPrompt` setting
3. `.noel/SYSTEM.md` in the working directory, if the project is trusted
4. `SYSTEM.md` in the agent directory

Appended text is the `appendSystemPrompt` setting's entries followed by each
`--append-system-prompt <text-or-path>`. Without either, it is
`.noel/APPEND_SYSTEM.md` of a trusted project, else `APPEND_SYSTEM.md` in the
agent directory. Files with the same name are not combined: the project's
replaces the agent directory's.

Context files come from the agent directory, then from each directory from
the filesystem root down to the working directory. A directory contributes
one file: `AGENTS.override.md`, else `AGENTS.md`, else `CLAUDE.md`. An
override replaces only its own directory's file. In a linked git worktree
checked out inside its main checkout, the worktree's file replaces the main
checkout's file of the same name instead of adding to it.
`--no-context-files` (`-nc`) loads none.

### Skills

A skill is a directory with a `SKILL.md`: frontmatter with a `description`,
then instructions. noel loads and uses skills as upstream does (its
`docs/skills.md`, and the Agent Skills specification).

```markdown
---
name: pdf-tools
description: Extract text and tables from PDF files. Use when reading PDFs.
---
# PDF tools

Read `references/formats.md` before converting a document.
```

Where skills come from, in the order they load:

1. The `skills` setting of a trusted project, then `.noel/skills`, then
   `.agents/skills` in each directory from the working directory up to the
   repository root.
2. The user's `skills` setting, then `skills/` in the agent directory, then
   `~/.agents/skills`.
3. Packages.
4. Each `--skill <path>`, a file or a directory. `--no-skills` (`-ns`) turns
   everything above off and keeps these.

In a directory, every `SKILL.md` below it is a skill, and nothing below a
skill's own directory is searched, so its scripts and references are not
skills. A Markdown file with a `description` is a skill too when it sits
directly in `skills/` (in `.agents/skills`, when it sits below it). A skill is
named by its frontmatter `name`, else by its directory. A `SKILL.md` without
a `description`, or with frontmatter noel cannot read, is not loaded.
Frontmatter is read as the YAML the Agent Skills specification uses: plain
and quoted values, `true`/`false`, and `|` or `>` blocks; a nested mapping or
a list is accepted and ignored.

When two skills have one name, the first loaded wins. `/diagnostics` reports
the others, and names and descriptions that break the specification's rules
(those still load).

The system prompt lists each skill's name, description, and file, and tells
the model to load the file with `read` (with `bash` when `read` is off) when
a task matches. A skill with `disable-model-invocation: true` is not listed.

`/skill:<name> [arguments]` sends the skill itself, in every mode: the file's
text without its frontmatter, in a `<skill>` block that names the file and
the directory its relative paths start from, followed by the arguments.
`/skills` lists the loaded skills.

### Prompt templates

A Markdown file `name.md` is the command `/name`, as in upstream's
`docs/prompt-templates.md`. Templates come from the `prompts` setting and
`.noel/prompts` of a trusted project, the user's `prompts` setting and
`prompts/` in the agent directory, packages, and each
`--prompt-template <path>` (a file or a directory), in that order; the first
of a name wins. `--no-prompt-templates` (`-np`) keeps only the
`--prompt-template` paths. A prompts directory gives the Markdown files
directly in it; a directory named by a setting or a package gives every
Markdown file below it.

```markdown
---
description: Review staged git changes
argument-hint: "[focus]"
---
Review the staged changes. Focus on ${1:-correctness}.
```

`/review concurrency` sends the file's text without its frontmatter, with the
command's arguments substituted:

| Placeholder | Stands for |
|---|---|
| `$1`, `$2`, ... | one argument |
| `$@`, `$ARGUMENTS` | all arguments, joined by spaces |
| `${1:-default}`, `${@:-default}` | the argument, or all of them, or the default |
| `${@:N}`, `${@:N:L}` | the arguments from the Nth on, or `L` of them |

Quotes keep spaces in an argument: `/review "API compatibility"`. `/prompts`
lists the templates, and command completion shows each one's argument hint
and description.

Skill commands and templates expand wherever input enters: a prompt in the
TUI, in print mode, and in RPC mode, and steering and follow-ups in the TUI
and in RPC mode. A command that names neither is sent to the model as text.
The TUI's own commands come first, then noel's slash-tool prompts (`/read
<path>`, `/bash <command>`, ...), then skills, then templates.

noel also keeps `/prompt <name> [input]` in the TUI: `{{input}}` in the
template stands for the input, which otherwise follows the template's text.

### Session variables

Commands the model runs with `bash` see the session they belong to:

| Variable | Value |
|---|---|
| `NOEL_SESSION_ID` | The session id (`ephemeral` with `--no-session`) |
| `NOEL_SESSION_FILE` | Absolute path of the session journal; unset with `--no-session` |
| `NOEL_PROVIDER` | The active model's provider |
| `NOEL_MODEL` | The active model's id |
| `NOEL_REASONING_LEVEL` | The reasoning level the turn's requests use: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` |

They are set for each command as it starts, and the system prompt tells the
model they exist. Each is also set under pi's name, with the same value, for
skills and scripts written for pi: `PI_SESSION_ID`, `PI_SESSION_FILE`,
`PI_PROVIDER`, `PI_MODEL`, and `PI_REASONING_LEVEL`. A variable with nothing
to report is removed under both names rather than inherited, so a noel started
from another noel's command (or pi's) never reports its parent's session.
Commands you type (`/bash`, `!`, `!!`) are not given them.

noel also marks its own process, and so every command and tool it starts:
`AI_AGENT=noel` names the agent for any tool that asks, and
`NOEL_CODING_AGENT=true` is noel's own marker, which it also sets as
`PI_CODING_AGENT=true`.

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
  `noel-bash-<random>.log` in the temp directory (readable only by you), and
  the result names that file. There is no default timeout. With `timeout`
  (seconds) the command and its process group are killed when it passes, and
  the result is the output so far followed by `Command timed out after N
  seconds`. A non-zero exit is an error result ending in `Command exited with
  code N`. While a command the model called runs, its output so far (within
  the same limits) is reported at most every 100 ms: the TUI shows the latest
  line under the running command (`ctrl+o` shows all of it), and RPC mode
  sends `tool_execution_update` records.
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

noel decides by API, not yet by the model's `input` in the catalog, so an image
is sent to every model behind the first four APIs. `images.blockImages` keeps
`read` from returning images at all, and `images.autoResize` (on by default)
scales an image down to at most 2000 pixels a side and 4.5 MB.

### Web tools

`web_search` returns numbered titles, URLs, and snippets; `web_fetch` retrieves
readable HTML/text/JSON/XML, retaining source URLs and links. They are enabled
by default unless `defaultTools`, `enabledTools`, `--tools`, `--exclude-tools`,
`--no-builtin-tools`, or `--no-tools` leaves them out. `--offline` /
`NOEL_OFFLINE=1` blocks both, including after reload.

Search uses DuckDuckGo HTML without an API key by default. For a more reliable
backend, set `BRAVE_SEARCH_API_KEY`, or set `NOEL_SEARXNG_URL` to the complete
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

Custom resources in `themes/` or trusted `.noel/themes/` use their filename stem
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
  "azure": { "default": { "type": "api_key", "key": "env:AZURE_OPENAI_API_KEY" } },
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
read; writes always use the nested form. The Azure provider is `azure`, as
upstream names it; an entry, a saved default, a session, or a `models.json`
model under the id earlier versions used, `azure-openai-responses`, is read
as `azure`. Without an explicit selection the
`default` account is used, or the sole account when only one exists.

`auth.json` is written atomically and owner-only (`0600`; a missing agent
directory is created `0700`). Logins, logouts, and OAuth refreshes hold an
advisory lock on `auth.json.flock`, re-read the file, and change only their
own entry, so concurrent noel processes never drop each other's credentials or
rotated refresh tokens. A `--api-key` value is used for that run only and is
never written.

Or use the CLI helper:

```bash
noel login anthropic --api-key env:ANTHROPIC_API_KEY
printf '%s' "$ANTHROPIC_API_KEY" | noel login anthropic --api-key -
noel login anthropic --account work --api-key env:WORK_ANTHROPIC_API_KEY
noel logout anthropic
noel logout anthropic --account work
```

Native OAuth enrollment is available for ChatGPT/Codex and Claude using the
official CLIs' public clients:

```bash
noel login openai-codex              # device authorization flow (browser + one-time code)
noel login openai-codex --account work
noel login anthropic                 # PKCE authorize + paste-code flow
noel login anthropic --account work
```

`noel login openai-codex` stores the login for `openai-codex`, and
`noel login anthropic` stores it for `anthropic`. A ChatGPT login is
`openai-codex`'s credential and its only one; `openai` takes an API key
(`noel login openai --api-key ...`, `OPENAI_API_KEY`, or the Codex CLI's
`CODEX_API_KEY`). Neither provider's models count as available on the
strength of the other's credential: with only a ChatGPT login noel starts on an
`openai-codex` model, and a `--models` pattern, `/model <reference>`, or
`set_model` does not reach `openai`'s. Earlier versions stored the ChatGPT
login under `openai` as well; that copy is ignored and `noel logout openai`
removes it. Expiry and `account_id` come from the issued token
(JWT `exp` for Codex, `expires_in` for Claude), and the stored credentials
refresh automatically when they expire, also in the middle of a turn: every
model request resolves them anew. Importing credentials from
`~/.codex/auth.json` and `~/.claude/.credentials.json` still works as a
fallback when no stored account exists. Re-running login for an existing
account replaces it.

When no explicit API key is configured, `noel` can reuse existing CLI login credentials:

- Claude Code: `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or `~/.claude/.credentials.json`
- Codex/ChatGPT: `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json` for `openai-codex`

Explicit API keys still take precedence over login tokens. Imported OAuth
credentials are refreshed automatically when they expire; refreshed tokens are
persisted to noel's own `auth.json` as `codex-import`/`claude-import` accounts
and never written back to the imported files.

`/account [name]` binds an account to the current session (bare `/account`
opens a selector; the binding is journaled and survives resume). `noel accounts
status` and `/accounts` show auth state, plan windows, and balance for every
configured account, including environment and imported pseudo-accounts; results
are cached for 15 minutes in `usage-cache.json` (`--refresh` bypasses the
cache).

For scripts, `noel auth check --provider <p>` (or `--model <ref>`) reports whether
credentials are ready (exit 0 ready, 1 not ready, 2 invalid; `--json`,
`--no-refresh`, `--account <name>`). As upstream, an unknown subcommand or
option exits 1, while other argument errors exit 2 for `check`. A ready OAuth
token that expires within five minutes gets `"expiresSoon": true` with
`--json`, or a warning on stderr in text output. `noel auth print-api-key` and
`noel auth print-bearer-token` write a usable credential to stdout; the bearer
form first refreshes OAuth tokens that expire within `--min-expiry` (default
30m). With `--account`, `--model` resolves to the providers that have that
account. Logins read from `~/.claude` or `~/.codex` are the exception to early
refresh: refreshing one spends that CLI's single-use refresh token and signs it
out, so `noel auth` refreshes them only once they expire, as requests do, and
prints or reports a token that expires sooner with a warning on stderr. noel's
own accounts, including saved `claude-import`/`codex-import` copies, refresh
early. noel has no MCP support, so `--no-mcp` is accepted and does
nothing, and `noel mcp` exits with an error.

### Account failover

With more than one stored account for a provider, `noel` can fail over between
them when one is rate limited, instead of retrying that account:

```json
{ "failover": { "enabled": true } }
```

Failover is availability-only, not load balancing: one sticky account carries
the provider's requests, and another takes over only when

- the provider answers the sticky account with a 429 before any output. That
  account then sits out until the time the provider gives: the answer's
  `retry-after-ms` or `retry-after` (in seconds), or the `error.resets_at` of
  an `openai-codex` usage limit, whichever is later; five minutes when the
  answer gives none. The switch is made inside the request, before
  [run recovery](#run-recovery) sees a failure: it is not a retry and waits
  for no backoff.
- or the sticky account's 5h usage window is at the exhaustion threshold
  (default 90%, `failover.providers.<provider>.exhaustionThresholdPct`), by
  the usage cache that `noel accounts status` and `/usage` show. Such an
  account is set aside until its window resets, and still serves when every
  other account is set aside too. While failover is on, noel refreshes the
  sticky account's cached usage in the background when it is older than 15
  minutes (`anthropic` and `openai-codex` logins and `zai` keys report a 5h
  window).
  A switch away from an account near its limit waits until
  `minSwitchIntervalMs` (default 10000) has passed since the last switch, so
  the pool does not flap between accounts near their limits; a switch away
  from a rate-limited account never waits.

Requests are never spread across accounts to maximize throughput. Pinning an
account with `/account name`, or giving `--api-key`, turns failover off.
`noel accounts --help` has the summary.

The sticky account, the rate limits and the time of the last switch are kept
in `failover-state.json` in the agent directory, so they hold across turns and
across `noel` processes: a `noel -p` does not ask an account the last one found
limited. The TUI reports a switch as a system line after the turn, print mode
on stderr, and RPC mode as a notification (see [RPC Mode](#rpc-mode)); each
names both accounts and when the first may be asked again.

When every account is rate limited the turn ends at once with each account's
answer and when the first may be asked again, and until then requests fail
with the same time; neither is retried. Any other failure stays on the sticky
account and is retried as run recovery describes: an overloaded provider
(529), which is overloaded for every account alike, a reply that breaks off,
and errors that are not the provider's answer. An account whose login cannot be
refreshed is left out, with one notice, and the others serve. The pool holds
the accounts the provider takes: an API key stored under `openai-codex`, or a
ChatGPT login stored under `openai`, is not one of them. With failover on, two
or more named accounts make the provider's models available, as an account
named `default`, an environment key or an imported login does without it.

Pooled subscriptions remain subject to each provider's terms: failover is
intended for accounts you are individually entitled to use, and the sticky
behavior exists so the client does not multiply quota by rotation.

Provider-specific environment:

- Anthropic and OpenAI: `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` move the provider's built-in models, and `models.json` entries without a `baseUrl`, to another address, as Claude Code and the OpenAI SDK read them. Anthropic's value has no `/v1` (for example `http://127.0.0.1:4141/_codewire/anthropic`); OpenAI's includes it. A Codewire Sandbox sets both to its credential proxy.
- Azure OpenAI: set `AZURE_OPENAI_BASE_URL` or `AZURE_OPENAI_RESOURCE_NAME`; optionally set `AZURE_OPENAI_DEPLOYMENT_NAME`, `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`, and `AZURE_OPENAI_API_VERSION`.
- Google Vertex: set `GOOGLE_CLOUD_PROJECT` or `GCLOUD_PROJECT`, plus `GOOGLE_CLOUD_LOCATION`.
- Cloudflare: set `CLOUDFLARE_ACCOUNT_ID`; AI Gateway also needs `CLOUDFLARE_GATEWAY_ID`. Without them the provider's models do not count as available, whatever key is set.
- Amazon Bedrock: Rust direct calls currently use Bedrock bearer-token auth via `AWS_BEARER_TOKEN_BEDROCK`.
- Claude Code OAuth: requests identify as `claude-cli/<version>`. The version tracks the latest published Claude Code release (fetched from the npm registry during model refresh, cached in `model-cache.json`); set `NOEL_CLAUDE_CODE_VERSION` to pin it.

`models.json` adds models to the built-in catalog or redefines them:

```json
[
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
  },
  {
    "provider": "local",
    "id": "my-model",
    "api": "openai-completions",
    "baseUrl": "http://127.0.0.1:8080/v1",
    "contextWindow": 32000,
    "maxTokens": 4000,
    "reasoning": true,
    "thinkingLevelMap": { "minimal": null, "xhigh": "xhigh" }
  }
]
```

An entry is used as written: `provider`, `id`, `name`, `api`, and `baseUrl`
(without one, the API's own default endpoint). The fields that describe the
model are upstream's and optional: `contextWindow`, `maxTokens` (the output
cap), `reasoning`, `thinkingLevelMap` (`null` for a level the model lacks, a
string for what the provider is sent for a level; `xhigh` and `max` exist only
when named), `input`, and `cost`. What an entry leaves out comes from the
built-in model of the same id, else from its provider's default model, as for
`--model provider/new-id`. For a provider the catalog does not know, a model
has no thinking levels and requests carry no output cap unless the entry says
so (the Anthropic API requires one: 16384). The file's format is this port's
own; upstream's `providers` format is not read.

A base URL names the API's root, as it does upstream: noel appends
`/v1/messages` (Anthropic), `/chat/completions` (OpenAI chat),
`/responses`, or `/v1/chat/completions` (Mistral). An Anthropic or Mistral
URL that already ends in `/v1` gets only the rest of the path.

`model-cache.json` is managed by `noel`. On startup, `noel` uses built-in models,
cached models, and explicit `models.json` entries immediately, then starts a
non-blocking background refresh when `modelRefresh.enabled` is not false,
`NOEL_OFFLINE`/`--offline` is not set, the cache is older than `ttlHours`, and a
provider has supported auth. `/reload` also kicks a refresh. Refreshed models
are available after `/reload` or the next startup. Refresh covers `anthropic`
(API key or Claude login), `openai` (API key), and `openai-codex` (ChatGPT
login), plus the OpenAI-compatible `zai`, `zai-coding`, `moonshotai`, and `kimi-coding-openai`
endpoints via their API keys, each at the base URL the catalog gives the
provider. Anthropic refresh paginates the official Models API; OAuth refresh
is best-effort against the same provider auth paths used for model requests.

A listing says which models a provider serves, not what they can do. A listed
model the catalog has stays as the catalog defines it; one it lacks is built
on the provider's default model, like `--model provider/new-id`, and keeps
the listing's name and context window.

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

Thinking levels can be set with `--thinking <level>` or `/thinking <level>`
(see [Models and thinking levels](#models-and-thinking-levels)). `/thinking`
alone shows the level in effect, and `/thinking <level>` takes one of the
levels the model supports, naming them when it is given another. `/model
<provider/id>` or `/model <id>` switches to that model, ignoring case; any
other text opens the picker with it as the search. While the `/model`
selector is open, each model shows the level selecting it would apply,
left/right walks the model's own levels, and Enter applies the model and that
level to the session. `/new` starts
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
`themes/`, or with `package.json` under the `noel` key:

```json
{
  "noel": {
    "extensions": ["extensions/assist"],
    "skills": ["skills/review"],
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
      "source": "vendor/noel-package",
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
`.noel-extension.json` manifest. A manifest `tools` array registers model-callable
tools; Noel sends `kind: "tool"` JSON requests on stdin and expects a JSON
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
managed with `noel config disable <extension|skill|prompt|theme> <name>` and
`noel config enable <extension|skill|prompt|theme> <name>`:

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
- `/skill:<name> [arguments]` (see [Skills](#skills))
- `/prompts`
- `/<template> [arguments]` (see [Prompt templates](#prompt-templates))
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
- `/export [file]` (default `noel-session-<session id>.html` in the session cwd)
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
- `/bug [description]` (writes a local `noel-bug-report-<id>.md` with environment,
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

Interactive assistant responses stream text as provider deltas arrive. While a response streams, a footer spinner shows the current phase (waiting, thinking, writing, or the running tool) with elapsed time, ctrl+c interrupts the turn unless transcript text is selected (then it copies); Esc interrupts only at the transcript bottom without a selection, Enter queues the draft as a follow-up, Up recalls the newest pending follow-up for editing, and ctrl+s steers the running turn (including promoting queued drafts): everything queued joins the conversation before the next provider request, combined into one user message so a burst of steering reads as a single instruction rather than several interruptions. Slash commands typed mid-turn still execute: `/queue` lists pending follow-ups, `/queue-clear` clears them, `/interrupt` and `/quit` act on the turn. A skill command or a prompt template typed mid-turn is queued expanded: as a follow-up with Enter, as steering with ctrl+s. `/queue <prompt>` adds follow-up prompts that run after the next assistant turn. Completed turns drain queued prose as one combined turn (queued tool commands such as `/read` still dispatch individually because their arguments are positional), including prompts queued during a follow-up, so nothing waits for a manual resubmission. Interruption, quit, or failure stops automatic dispatch and preserves remaining work. At idle, `/interrupt` clears queued follow-ups, and `!`/`!!` execute shell commands without adding them to the conversation context. Shell commands (`!`, `/bash`, and the model's `bash` tool) are non-interactive: stdin is empty and the command runs in its own session without a controlling terminal, so a program that prompts for input fails instead of waiting. A command that times out or whose turn is interrupted is killed together with its whole process group, and a timeout returns the output so far; background jobs started by a command that finished normally keep running, and the command returns without waiting for them. `!` and `/bash` commands time out after 120 seconds; the model's `bash` calls have no timeout unless the model sets one. Editing keys follow common agentic-CLI conventions: shift+enter or ctrl+j inserts a newline, ctrl+g opens the draft in an external editor (`NOEL_EDITOR_COMMAND`, `VISUAL`, or `EDITOR`), ctrl+c with a draft clears it, and ctrl+c/ctrl+d on an empty input require a second press to quit. Manual and automatic compaction persist summary records, and forked or cloned sessions persist branch summaries. Editor state tracks history, undo, kill-ring, and slash completions; restored session user prompts repopulate prompt history.

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
`clip.exe` on Windows. `NOEL_CLIPBOARD_COMMAND` overrides the helper. If local
helpers are unavailable, Noel requests the terminal clipboard via OSC 52 (useful
over SSH; terminal/tmux clipboard permissions must allow it). An OSC 52 request
cannot confirm delivery. Copy errors appear in the transcript hint; selection
remains available for retry. Without a selection, `/copy` copies the last assistant
message. Terminal-native selection via Shift+drag (sometimes Option on macOS)
remains available; `/export <file>` exports the conversation.

Run `noel --no-alt-screen` (or upstream's `--tui-mode regular`) for native inline
compatibility: overflow is written once to terminal scrollback, mouse
selection/wheel remain terminal-owned, and the composer scrolls with the
terminal. `--tui-mode fullscreen` selects the owned screen explicitly.
`--use-theme <name>` applies a theme for one run without saving it; it cannot
be combined with `--theme` or `--no-themes`.
Print/JSON/RPC modes are unchanged.
Owned mode restores the previous screen, mouse reporting, paste, and raw mode
on exit. This viewport is an intentional Rust UI divergence; upstream parity
fixtures are not rewritten.

SIGTERM and SIGHUP end noel in every mode, as does SIGINT outside the TUI (print,
JSON, RPC, and login); in the TUI ctrl+c stays a key and an outside SIGINT is
ignored. noel first kills running tool commands with their process groups and
restores the terminal, then exits like upstream pi: with status 128 + the
signal number for SIGTERM and SIGHUP, and by the signal itself for SIGINT, so
ctrl+c also stops a shell loop that runs noel.

Ctrl+V reads a clipboard **image** into the current draft; type a prompt and
press Enter to send it. Image-only drafts use “Describe this image.” Esc or
Ctrl+C clears the draft when idle unless transcript text is selected (Esc first
clears selection/returns to bottom if scrolled up). Multiple images are supported. `/image <path>`
without a prompt also stages an attachment. Bracketed text paste is unchanged.
Images respect `images.blockImages` and `images.autoResize`. While a turn runs,
image drafts are kept for submission after it finishes (not sent as text-only
steering/follow-ups). Linux needs `wl-paste` (Wayland) or `xclip` (X11); macOS
uses AppKit via Swift (Xcode command-line tools), and Windows uses PowerShell.
For remote/headless sessions, `NOEL_CLIPBOARD_IMAGE_COMMAND` can supply encoded
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
count. Configure `.noel/settings.json` or global settings:

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
and re-attempts are skipped until history changes. If the provider then
refuses the request as too large, "Run recovery" below applies.
Model windows come from the catalog; custom `models.json` entries can specify
`contextWindow`. Unknown windows fall back to 128,000 tokens.
Token counts are heuristics, not exact provider tokenization.

The agent per-run round cap is configurable alongside compaction and retry:

```json
{
  "agent": {
    "maxToolRounds": 120
  }
}
```

`agent.maxToolRounds` bounds provider rounds per user run, including steering
rounds. The default is 200; values are clamped to at least 1. When the cap is
reached the turn stops with a round-limit error, all work stays saved, and
sending any prompt (or restarting, which auto-continues) gets a fresh budget.
A request that is retried, or asked again after an overflow, is one round.

### Run recovery

A model request that fails is handled as upstream pi handles it, in the TUI,
print mode, and RPC mode alike:

- **Retry.** A failure that a retry can fix is retried, whether the request
  failed before any output or broke off mid-reply: a connection that cannot
  be made or drops, a timeout, HTTP 408, 429, 500, 502, 503, 504, 520, and
  524, and any answer that says the provider is overloaded or rate limited.
  The reply that broke off is dropped: it is not saved and not sent back to
  the model. Defaults are 3 retries with exponential backoff from
  2 s, capped at 60 s (`retry.enabled`, `retry.maxRetries`,
  `retry.baseDelayMs`, `retry.maxAgentDelayMs`). Other failures end the turn
  at once: 400, 401, 403, and 404 responses, other statuses whose body names
  nothing transient, a missing credential, and quota or billing limits even
  when they arrive as a 429. With [account failover](#account-failover) on,
  a 429 before any output goes to another account first. The rules
  are upstream's patterns over the error text
  (`error-classification.json`). When retries
  run out, the error names the likely causes, including a local firewall
  that denies connections from a freshly built binary until its prompt is
  approved, because on default-deny hosts an unanswered prompt is
  indistinguishable from an outage.
- **Context overflow.** When the provider refuses a request as too large for
  the model's context window, noel compacts and asks again, once. The same
  happens when a final reply is cut off at the output limit although the
  model could have written more, by its catalog `maxTokens` (for a
  `models.json` model of a provider the catalog does not know that names no
  `maxTokens`, noel assumes the context was the cause): the cut-off reply is
  dropped and the compacted request is sent again. A reply cut off
  while calling a tool is kept instead; its calls get error results and the
  model issues them again. If there is nothing to compact, or compaction is
  off or fails, an overflow error ends the turn, and a cut-off reply stands
  as the turn's answer. A second overflow in a row is not compacted again.

In the TUI a retry or an overflow appears as a system line (`retrying in 2s
(attempt 1 of 3): …`); text the dropped reply had streamed stays above it,
and the reply that replaces it starts below. Esc during a backoff interrupts
the turn.

The original transcript remains in the session journal. A validated, smaller
checkpoint and its replacement context are committed together through an atomic
file replacement. Empty/truncated/oversized/tool-bearing summaries, provider
failures, or cancellation leave active history intact; no silent truncation
fallback is used. If the latest turn or persistent instructions still cannot fit,
noel reports an error instead of repeatedly compacting. Completed summarization
requests count toward `/usage`. `/export` exports the current checkpoint and
retained context, not the pre-compaction journal transcript.


## Specification

`docs/spec.md` is noel's normative specification for sessions, durability, the
queue and abort, recovery, and `noel server`, `noel worker` and the clients
that attach to it. It marks what is built, what is decided and what is
proposed, and cites upstream where noel follows it.

## Server Sessions

`noel server` holds sessions for clients on this machine (spec §5–§8), and a
client starts it when none listens. Open the TUI on a session the server holds
with `--attach`; any saved session can be attached, including one a local
`noel` made:

```bash
noel client new --model faux/echo   # prints the new session's id
noel --attach <session-id>          # the TUI on that session
noel client watch <session-id>      # its view as text, from another terminal
```

Several TUIs can attach to one session, and each shows the same transcript and
the run as it streams. Enter sends a prompt; during a run it queues a
follow-up, ctrl+s steers, Up takes the last queued submission back into the
draft, and Esc aborts the run (which withdraws what was queued). `/model
<provider/id>`, `/thinking <level>` and `/compact` change the session; commands
for this process's own sessions, settings and accounts say they are not
available when attached. `/quit` detaches, and a run goes on in the server. If
the session's worker or the server stops, the TUI says so, holds the session
again and redraws it; the server resumes an interrupted run.

### A server on another machine

`--remote <ssh-destination>` uses the server on another machine, through
`ssh <destination> noel bridge`. That machine needs `noel` and nothing else;
its server starts when none listens, and its sessions, models, accounts and
tools are its own:

```bash
noel client --remote build-box ls
noel client --remote build-box new --cwd /srv/app --model anthropic/claude-sonnet-4-5
noel --attach <session-id> --remote build-box
```

- **ssh does the authentication,** with your `~/.ssh/config`, keys, agent and
  known hosts, so `user@host`, a `Host` alias and hosts from `cw setup ssh`
  all work. There is no token and no relay. The server stamps these links
  `{kind: "owner", via: "tunnel"}`, since `ssh` admitted the same user that
  owns it (spec §6).
- **`noel bridge`** joins its stdin and stdout to the server's socket, after
  the same checks a local client makes, and exits when either side closes.
  `--remote-noel <path>` names noel on that machine when it is not on the
  `PATH` that `ssh host command` gets (`~/` at its start is that machine's
  home).
- **`new` needs `--cwd`,** an absolute path on that machine.
- **A dropped connection** is a lost link: `watch` and the TUI reconnect, and
  get the view again. The first connection may prompt (a password, a new host
  key); reconnections run ssh with `BatchMode=yes`, so they never take the
  terminal. `make bridge-journey` runs the whole flow against `sshd` in an
  Alpine container with no network.

## Remote Workspaces

A session can run its tools in a directory on another machine, reached over
`ssh` (spec §10). noel uploads its `noel-env` daemon there and every built-in
tool works through it: `read`, `write`, `edit`, `ls`, `bash`, `grep`, `find`,
and `!` or `/bash` in the TUI. The model and noel itself stay on this machine.

```bash
noel workspace trust ssh://build-box          # shows the host key, asks y/N
noel --workspace ssh://me@build-box/srv/app   # a new session, tools on build-box
noel -c                                       # reopens it; the workspace is kept
noel client new --workspace ssh://me@build-box/srv/app   # the same through noel server
```

- **The URL** is `ssh://[user@]host[:port]/absolute/path`. Add `?login=1` to
  start the daemon through the remote login shell (`$SHELL -l`), so commands
  see what `~/.profile` sets; without it they get what `ssh host command`
  gets. The session's journal saves the workspace, `-c`, `-r`, `--session`
  and a server's worker use it again, and `--workspace` naming another one for
  an existing session is an error. Without `--workspace` a session is local,
  as before.
- **Trust is never automatic.** `noel workspace trust ssh://host[:port]`
  connects once, prints the host key's fingerprints (`ssh-keygen -lf`), and
  on a terminal asks before it stores the key; without a terminal pass
  `--fingerprint SHA256:…`, the host's own fingerprint, and only that key is
  trusted. Keys go to noel's own `~/.noel/workspaces/known_hosts`, never to
  `~/.ssh`. `noel workspace forget ssh://host[:port]` removes them. A session
  on a host noel does not trust does not start, and the error names the trust
  command. `ssh` itself still reads your `~/.ssh/config`, keys and agent to
  log in, with prompts off (`BatchMode`) and no forwarding.
- **The daemon** is `noel-env-linux-x64` or `noel-env-linux-arm64` beside
  `noel` (or in `NOEL_ENV_DAEMON_DIR`). `make env-daemons` builds them (static
  musl; arm64 needs `aarch64-linux-gnu-gcc`, or set `ENV_DAEMON_ARM64_LINKER`)
  and `make install` installs them with `noel`. It is uploaded to
  `~/.cache/noel-env/` on the remote machine, named by its SHA-256, and
  checked before every start. macOS remotes need a Mac-built daemon, which
  is not built yet. `make noel-static` builds a static musl `noel` (clang
  compiles ring's C for musl), and `make workspace-journey` runs the whole
  flow (trust, tools, a dropped connection, `-c`, Mode 2, the TUI's footer)
  against `sshd` in an Alpine container with no network.
- **Connecting** happens at the first tool call. An unreachable host makes
  that call fail, not the session; the TUI's footer shows the workspace and
  whether it is connected, connecting or lost. If the connection drops, the
  call in flight fails (a command's outcome is then unknown) and the next
  call starts a new daemon.
- **What comes from the workspace:** the tools' files and commands, the
  project's context files (`AGENTS.md`, `CLAUDE.md` and the like, from the
  workspace's directory and its parents) and the `cwd` the system prompt
  states. Everything that would run or configure noel here stays this
  machine's: settings, `.noel/`, extensions (whose tools run here), skills
  (not listed in the system prompt, since the model's `read` cannot reach
  them; `/skill:<name>` still works), prompt templates, `@file` references
  and `/diff`.

## RPC Mode

`noel --mode rpc` speaks upstream pi's RPC protocol (upstream's `docs/rpc.md`,
`docs/rpc-commands.md` and `docs/json.md`): one JSON command per line on
stdin, and responses and session events as one JSON object per line on stdout.
Lines split on LF only: a trailing CR is dropped, and U+2028 or U+2029 inside a
string is not a line break. stdout carries only protocol records; diagnostics
go to stderr. RPC mode starts when stdin is not a terminal and takes no prompt
arguments.

Commands: `prompt` (with `images`, `streamingBehavior`, and `requestId`),
`steer`, `follow_up`, `abort`, `abort_retry`, `set_auto_retry`, `resume_run`,
`clear_queue`, `get_state`, `get_messages`, `get_last_assistant_text`,
`get_commands`, `new_session`, `switch_session`, `set_model`,
`set_thinking_level`, `set_steering_mode`, `set_follow_up_mode`, `compact`,
and `set_session_name`. Other upstream commands answer
`Unsupported command: <type>`, and unknown types `Unknown command: <type>`.

A `prompt`, `steer`, or `follow_up` whose message is a skill command or a
prompt template is expanded when it arrives, so the queue and the session
hold the expansion. `get_commands` lists the templates and the skills (as
`skill:<name>`) with their descriptions and where each was loaded from. A
message that starts with `/` and names neither goes to the model as text,
except noel's slash-tool prompts, which run their tool.

A `prompt` that starts a run answers `{"disposition":"started","runId":"…"}`,
then the run reports `agent_start`, its turns (`turn_start` … `turn_end`),
`agent_end`, and exactly one `agent_settled` with `aborted` and the `runId`,
after an abort or a failure too; a failed request ends with an assistant
message whose `stopReason` is `error`. Follow-ups queued during a run are
delivered in it, after the turn that would otherwise end it. `abort` answers
once the run has settled, or at once when no run is active, and leaves queued
messages for the next prompt. While a run is active, a `prompt` needs
`streamingBehavior` (`steer` or `followUp`); `set_model`,
`set_thinking_level`, `set_auto_retry`, `compact`, `new_session`,
`switch_session`, `set_session_name`, and `resume_run` are refused; and
`get_state`, `get_messages`, and `get_last_assistant_text` answer from the
run's current state. `set_model` takes a model that can run now (its provider
has credentials) by exact provider and id, and changes the session's model,
not the default model in `settings.json`; the thinking level follows as it
does for any model switch. `set_thinking_level` clamps the level to what the
model supports. Both report a change of the level in effect with
`thinking_level_changed`. `get_state` gives the model with the catalog's
fields (`reasoning`, `thinkingLevelMap`, `input`, `cost`, `contextWindow`,
`maxTokens`) and, as `thinkingLevel`, the level in effect. `set_auto_retry`
turns retries on or off for this process only.

A run that recovers (see "Run recovery") reports it as upstream does. The
reply it drops ends its turn: `message_end` with `stopReason` `error` (or
`length` for a reply cut off at the output limit), `turn_end`, and
`agent_end`, whose `willRetry` is true before a retry. Then come
`auto_retry_start` and the backoff, or `compaction_start` and
`compaction_end` with reason `overflow` (`willRetry` true when the run asks
again). The run then starts over with another `agent_start`, under the same
`runId`, and still settles exactly once. `auto_retry_end` follows the reply
that succeeded, or the `agent_end` of the last failure. `abort_retry` ends a
backoff in progress: the run settles with the failure it would have retried,
and `auto_retry_end` reports `Retry cancelled`.

When [account failover](#account-failover) switches accounts, noel sends one
of upstream's notifications before the reply: an `extension_ui_request` with
`method` `notify`, `notifyType` `warning` and the switch as `message`. It
needs no answer, and an `extension_ui_response` gets none, as upstream ignores
one that nothing waits for. noel sends no other extension UI request.

While a `bash` call runs, `tool_execution_update` records carry its output so
far in `partialResult.content`: none when the command starts, then all of it
(within the result's limits) each time more arrives, at most every 100 ms.

noel adds four extensions, listed in `get_state.rpcExtensions`:

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
that started and never ended is `interrupted`: noel crashed, was killed or
ended by a signal, or saw stdin close mid-run. noel never resumes a run by
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
`crates/noel-cli/tests/rpc_parity.rs` runs every scenario of
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
scripted runs against a local stub model, and noel's RPC mode is compared with it
record by record. The system prompt fixture records upstream's prompt text
for fixed tools, prompt sources, context files, and skills, and which prompt
and context files upstream picks from a directory tree; noel's prompt is
compared with it to the byte, under noel's names. The skills fixture records
which skills and prompt templates upstream loads from directory trees,
settings, and flags, how it reads their frontmatter, and what it sends for
`/skill:name` and `/template` input; noel is compared with each. The error
classification fixture records which failed requests upstream retries and
which it reads as a context overflow: its pattern lists, which noel keeps
verbatim, and its verdicts for sample error messages. Secrets are redacted.
Rust tests compare covered behavior against those fixtures, so upstream
changes become visible as fixture diffs and test failures.

The fixtures are upstream's output and keep upstream's names. Where a test
needs noel's (the product and the `NOEL_*` variables in the system prompt, a
project's `.noel/`), it reads the fixture through
`noel_config::with_noel_names`, the one substitution, listed in the
non-parity register. The tests that load a project tree run it both ways: as
upstream wrote it, which is a project written for pi, and under noel's names.

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
NOEL_PARITY_AGENT_COMMAND='cw exec --name ts-parity-agent -- claude' make ts-parity-agent
```

`NOEL_PARITY_AGENT_COMMAND` receives `brief.md` on stdin. Scheduled GitHub Actions
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
`docs/rust-rewrite/non-parity-register.md`. The `noel-parity` crate keeps fixture
inventory, fixture source metadata, redaction checks, and parity documentation
in sync.

Manual real-provider Opus smoke with Claude Code OAuth:

```bash
make smoke-claude-opus-oauth
```

Generic opt-in real-provider print smoke:

```bash
NOEL_SMOKE_REAL=1 NOEL_SMOKE_REAL_MODEL=provider/model make smoke-real
```

Provider profile smokes:

```bash
NOEL_SMOKE_REAL=1 make smoke-real-openai
NOEL_SMOKE_REAL=1 make smoke-real-anthropic
NOEL_SMOKE_REAL=1 make smoke-real-gemini
NOEL_SMOKE_REAL=1 make smoke-real-mistral
NOEL_SMOKE_REAL=1 make smoke-real-openrouter
```

Full manual smoke suite:

```bash
make test-smoke
```

`make dogfood` runs the development TUI with `--continue --model faux/echo`.
It also starts a quiet background `cargo build -p noel-cli` watcher; successful
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
`target/release/noel` and runs the binary in tmux with an isolated agent/session
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
unless `NOEL_SMOKE_REAL=1` and `NOEL_SMOKE_REAL_MODEL=provider/model` are set.
The profile targets preselect default models for OpenAI, Anthropic, Gemini,
Mistral, and OpenRouter while still using the normal auth resolver.

The real TTY dogfood target can be narrowed with `NOEL_DOGFOOD_REAL_PROVIDERS`:

```bash
NOEL_DOGFOOD_REAL_PROVIDERS=claude make dogfood-real
NOEL_DOGFOOD_REAL_PROVIDERS=codex make dogfood-real
```

The default models can be overridden with `NOEL_DOGFOOD_CLAUDE_MODEL` and
`NOEL_DOGFOOD_CODEX_MODEL`.

The real-provider smoke is intentionally not part of `test`, `check`, or `e2e`.
It requires `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, or
`~/.claude/.credentials.json`, sends one tiny prompt to
`anthropic/claude-opus-4-7`, and defaults to `--thinking max`. The generic
`smoke-real` target uses the normal auth resolver for the selected model and
defaults to `--thinking off` unless `NOEL_SMOKE_REAL_THINKING` is set.
Provider profile defaults can be overridden with `NOEL_SMOKE_OPENAI_MODEL`,
`NOEL_SMOKE_ANTHROPIC_MODEL`, `NOEL_SMOKE_GEMINI_MODEL`, `NOEL_SMOKE_MISTRAL_MODEL`,
or `NOEL_SMOKE_OPENROUTER_MODEL`. `NOEL_SMOKE_REAL_EXPECTED` changes the expected
marker for all real-provider print smokes.
`test-smoke` runs local tmux e2e first, then the opt-in generic real-provider
smoke and the real-provider Opus OAuth smoke.

## Development Notes

For TypeScript behavioral reference, check out the tip of upstream `main` from
`https://github.com/earendil-works/pi`. Active development in this repository's
`main` branch is Rust-only.

Rust live sessions use an append-only replay JSONL log. JSONL export/import and direct session open support the TypeScript v3 session-tree shape where applicable, including a `type:"session"` header and entry `id`/`parentId` chain. Full in-place active-leaf tree editing is still tracked as parity work. Legacy TypeScript session logs are not migrated automatically; use the tip of upstream `main` for reading TypeScript session behavior. `/share` writes a local HTML export; web or gist sharing is intentionally unsupported in the Rust-only CLI.

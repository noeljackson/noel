# Configuration


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

## Moving from pi

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

## Models and thinking levels

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

## System prompt

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

## Skills

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

## MCP servers

`mcp.json` — in the agent directory (`~/.noel/mcp.json`) and, for a
trusted project, `.noel/mcp.json` — configures Model Context Protocol
servers with the `mcpServers` shape shared by other MCP clients:

```json
{
  "mcpServers": {
    "calc": {"command": "calc-server", "args": ["--fast"]},
    "helper": {"command": "helper", "env": {"TOKEN": "${MY_TOKEN}"}}
  }
}
```

Project entries replace global entries of the same name; a project entry
with only `{"enabled": false}` turns a global server off without holding
its credentials. `${NAME}` in `env` expands from the environment. Or
skip the file: `noel mcp add <name> --command <cmd> [args...]` and
`noel mcp add <name> --url <url>` write it (`-l` for the project's,
`--` before server args that start with `-`), `noel mcp remove <name>`
removes, and `noel mcp list` names them all. A file that does not parse
is left as it is, with an error.

Servers connect — stdio entries by spawning the command, HTTP entries
(`"url": ...`) over the streamable-HTTP transport (JSON or SSE answers,
session and protocol-version headers, stream resumption and reconnect)
— at the first run that needs them, and their tools join the session as
`mcp__<server>__<tool>`, callable by the model like any tool; a server
that fails to connect is one system line and its tools are simply
absent until a later run retries it. `/mcp` lists the servers and their
live tool counts, `noel mcp list` names them from the shell, and
`/reload` re-reads `mcp.json`. `--no-mcp` keeps every server from
connecting. A tool's declared schema validates the model's arguments
first — with coercion (a numeric string for a number, a lone value for
an array) and pi-style errors (`- count: expected integer, got string`)
— so a malformed call fails with a message the model can act on. An
HTTP server that answers 401 signs in with `noel mcp login <name>`: OAuth
discovery, client registration, a PKCE authorization in the browser
against a loopback callback, and tokens stored in `mcp-auth.json`;
connections send the stored access token (trying one refresh after a
401), and `noel mcp logout <name>` clears them. Connections never open a
browser on their own.

## Subagents

A subagent definition is a Markdown file with YAML frontmatter, loaded from
`.noel/agents/` of a trusted project and then `agents/` in the agent
directory, the first of a name winning (as skills load; `/reload` reloads
them):

```markdown
---
description: Reviews Rust changes for correctness and style.
tools: read, grep, bash
model: anthropic/claude-sonnet-4-5
---
You are a code reviewer. Read the diff, list concrete problems,
and rank them by severity. Be terse.
```

- `description` (required) is how the model chooses among them.
- `tools` scopes the subagent to a comma list of noel tool names; omitted
  gives it the parent's active tools. The list must resolve to at least
  one tool the parent has, and `task` is refused: subagents do not nest.
- `model` names `provider/id` for the subagent's own requests; `thinking`
  its thinking level.
- The body is the subagent's system prompt.

When any definition is loaded the model is offered the `task` tool, whose
description names them. A call runs the named subagent in-process: its own
provider rounds, its own context window, its scoped tools, in the same
workspace — it does not see this conversation, so the prompt must carry
everything it needs. The parent receives only the subagent's final reply,
as the tool's result, and the transcript shows `● Used Task
<name>(<description>)` with the reply below (ctrl+o expands it). An abort
ends the subagent with the turn.

## Prompt templates

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

## Session variables

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

## Hooks

`hooks` in the settings names shell commands that run around tool calls —
plain settings, no host program:

```json
{
  "hooks": {
    "preToolUse": [
      {"matcher": "edit|write", "command": "echo 'not today' >&2; exit 2"}
    ],
    "postToolUse": [
      {"matcher": "bash", "command": "date >> ~/noel-tool.log"}
    ]
  }
}
```

`matcher` is a `|`-separated list of tool names; empty or `*` matches every
tool. Names match in any case, and Claude Code's names work for the tools
named otherwise here (`Glob` is `find`, `WebFetch` `web_fetch`, `WebSearch`
`web_search`, `Agent` `task`, `TodoWrite` `todo`, `MultiEdit` `edit`), so a
matcher written for Claude Code, such as `Edit|Write`, applies unchanged.
A hook runs in the session's cwd with its shell, one JSON object on stdin
(`tool`, `arguments`, `matcher`, `cwd`, `sessionId`) and `NOEL_SESSION_ID`/
`NOEL_TOOL` in its environment. A **pre**-hook that exits 2 blocks the
call, its stderr the reason the model sees; any other exit allows, a
non-zero one adding a notice. A **post**-hook runs after the result is
saved; its output is ignored, a non-zero exit is a notice. A hook that
cannot run at all blocks, fail-closed. Hooks reload with `/reload`.


## Tool selection

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

## File and shell tools

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

## Web tools

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

## Terminal themes

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
early. noel's MCP covers stdio and streamable-HTTP servers (see
[MCP servers](#mcp-servers)); `--no-mcp` keeps configured servers from
connecting this run.

## Account failover

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
  [run recovery](compaction.md#run-recovery) sees a failure: it is not a retry and waits
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
on stderr, and RPC mode as a notification (see [RPC Mode](rpc.md#rpc-mode)); each
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

## Environment variables

Variables noel reads. Those marked *development* are for tests, e2e runs
and dogfooding; nothing else needs them.

| Variable | What it does |
|---|---|
| `NOEL_DIR` | The agent directory (default `~/.noel`): settings, auth, sessions, resources. |
| `NOEL_SESSION_DIR` | Where session journals go, instead of the agent directory's `sessions`. |
| `NOEL_RUNTIME_DIR` | Where `noel server` keeps its sockets and worker records. |
| `NOEL_OFFLINE` | `1`, `true` or `yes` runs offline, as `--offline` does. |
| `NOEL_EDITOR_COMMAND` | The external editor for `/editor` and ctrl+g, before `VISUAL` and `EDITOR`. |
| `NOEL_CLIPBOARD_COMMAND` | The command `/copy` pipes text into, instead of the platform's. |
| `NOEL_CLIPBOARD_IMAGE_COMMAND` | The command that reads an image from the clipboard for a paste. |
| `NOEL_LOG` | Which diagnostics are written (a tracing filter such as `debug` or `noel_core=debug`; `RUST_LOG` if unset; default `info`). They go to stderr, which is the log for `noel server` and its workers, and nowhere while the TUI holds the terminal. |
| `NOEL_SEARXNG_URL` | A SearXNG instance for `web_search`. |
| `NOEL_CODEMODE_MAX_WORKERS` | How many codemode scripts run at once. |
| `NOEL_CLAUDE_CODE_VERSION` | The Claude Code version the Anthropic OAuth requests name. |
| `NOEL_BUN_BIN` | The `bun` that runs TypeScript extensions (`path` for the one on `PATH`). |
| `NOEL_BUN_HOST` | The Bun host shim for TypeScript extensions, instead of the installed one. |
| `NOEL_ENV_DAEMON_DIR` | The directory of `noel-env` daemon binaries an SSH workspace uploads. |
| `NOEL_CLIENT_DEBUG` | *Development*: the client logs its link's lifecycle to stderr. |
| `NOEL_DOGFOOD_AUTO_RESTART` | *Development*: `1` restarts noel when its binary changes. |
| `NOEL_DOGFOOD_RESTART_EXE` | *Development*: the binary the auto restart runs. |
| `NOEL_FAUX_SCRIPT`, `NOEL_FAUX_SCRIPT_LOG` | *Development*: the faux provider's scripted replies, and where it logs requests. |
| `NOEL_FAUX_STREAM_CHARS`, `NOEL_FAUX_STREAM_DELAY_MS` | *Development*: how the faux provider streams. |
| `NOEL_TUI_E2E_DUMP`, `NOEL_TUI_E2E_DUMP_FILE` | *Development*: the e2e scripts' transcript dump. |
| `NOEL_TEST_ISOLATION` | *Development*: tests ignore the user's real agent directory. |

Set by noel for what it runs (and under `PI_` names for what was written for
pi): `NOEL_CODING_AGENT`, `NOEL_SESSION_ID`, `NOEL_SESSION_FILE`,
`NOEL_PROVIDER`, `NOEL_MODEL`, `NOEL_REASONING_LEVEL`; for extensions
`NOEL_EXTENSION_NAME`, `NOEL_EXTENSION_PATH`, `NOEL_EXTENSION_DIR`,
`NOEL_EXTENSION_PROTOCOL`, `NOEL_EXTENSION_TOOL`; for plugins
`NOEL_PLUGIN_NAME`.

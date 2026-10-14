# Rust Rewrite Non-Parity Register

Every Rust behavior should either match a committed TypeScript parity fixture or
appear here with a deliberate reason for not cloning the TypeScript behavior.

## Active Non-Parity Decisions

### No TypeScript Runtime Dependency

Rust must not import, shell out to, or embed TypeScript or JavaScript at
runtime. TypeScript is allowed only inside the Docker reference runner used to
generate parity fixtures.

Reason: the product goal is a native Rust CLI/TUI, not a wrapper around the old
runtime.

### No Web UI Product Path

The old TypeScript web UI is not part of the active Rust product.

Reason: the active product scope is CLI/TUI-only. Historical web UI labels may
remain in GitHub for cutover tracking, but new implementation work should not
add web UI dependencies.

### No npm or Node Host Workflow

Normal development, tests, and validation must not run npm on the host.

Reason: the repository is Rust-only. TypeScript reference execution is isolated
to Docker so parity checks do not contaminate the product toolchain.

### No Automatic Legacy Session Migration

Rust does not automatically read or migrate old TypeScript session logs.

Reason: Rust live sessions have a durable replay JSONL contract. TypeScript v3
JSONL export/import and direct open are supported where practical, but old logs
are not automatically migrated in place. Check out the tip of upstream `main`
for forensic reading of TypeScript behavior.

### No Live Providers in Normal Tests

Normal tests use faux/local providers or sanitized request fixtures.

Reason: CI and local validation must not require credentials, spend money, or
leak request data. Real-provider smoke remains opt-in.

### No Full Autonomous Transcript Oracle

The parity harness does not use full live autonomous transcripts as its primary
correctness signal.

Reason: provider sampling, shell timing, terminal dimensions, and network state
make full transcripts brittle. The harness instead captures deterministic
contracts for request shape, normalized messages, tool dispatch, storage,
settings, and TUI markers.

### Named Multi-Account auth.json

Rust writes `auth.json` as `{ "<provider>": { "<account>": <credential> } }`
instead of the flat TypeScript shape. Flat files are read and migrated
transparently, so existing setups keep working; only the write format diverges.

Reason: multiple accounts per provider (work/personal, plan variants) need a
named account layer for session binding (`/account`), per-account OAuth
refresh, and per-account quota probes (`noel accounts status`). The TypeScript
shape cannot represent that.

### Native OAuth Enrollment Flows

Rust supports interactive OAuth login (`noel login openai-codex` device
authorization, `noel login anthropic` PKCE paste-code) using the official Codex
CLI and Claude Code public client registrations and endpoint contracts.

Reason: upstream pi authenticates via API keys or imported credential files.
Native enrollment removes the manual import step while storing credentials in
the same v2 accounts and refresh machinery. The official CLIs' flows are
public, documented in their shipped binaries, and produce tokens for the same
upstream services.

### Credentials That Make a Provider Available

Rust keeps upstream's rule that a credential belongs to the provider it is
stored for, and that a provider takes only its own kinds: `openai-codex` a
ChatGPT login and no API key, `openai` an API key. A stored credential of the
other kind is not a credential. The two Cloudflare providers share one key,
and each also needs the ids its URLs are built from (`CLOUDFLARE_ACCOUNT_ID`,
and `CLOUDFLARE_GATEWAY_ID` for the gateway). The models of a provider
without what it needs are not available: startup does not take one, and a
scope pattern, `/model <reference>`, or `set_model` does not reach one
(`credentials` and the last `launches` of `model-resolution.json`). These
parts differ:

- Upstream's `openai` also has a ChatGPT sign-in of its own, whose tokens its
  Responses API takes. Rust has no such flow, so a login stored under
  `openai` does not count. It is the Codex login earlier versions of
  `noel login` stored there as well, which that API does not take; it is
  ignored wherever credentials are read, and `noel logout openai` removes it.
- Rust also reads the Codex CLI's own credentials, each for the provider it
  can serve: `CODEX_ACCESS_TOKEN` and `~/.codex/auth.json` for
  `openai-codex`, and `CODEX_API_KEY` as a second name for `openai`'s API
  key. Upstream reads neither.
- `noel login openai` without `--api-key` stores `env:OPENAI_API_KEY` when that
  variable is set and otherwise prints the usage; `noel login openai-codex
  --api-key` is refused.
- Upstream also takes the Cloudflare ids from a stored credential's `env`.
  Rust reads them from the process environment only, and `noel auth check`
  reports on the key alone.

Reason: a ChatGPT login cannot authenticate `openai`'s Responses models, and
counting it made a user with only that login start on a model that fails
with a missing API key. A Cloudflare key without the ids fails the same way.

### Todo Tool and Edited-Files Panel

Rust adds a `todo` builtin tool (checklist with `pending`/`in_progress`/
`completed`, journaled as a `todos` session record) and per-session
edited-file tracking (`edited_files` journal record) powering the TUI task
widget (`ctrl+t`) and the `/diff` side panel. The TypeScript upstream has no
equivalent tool or panel, so `local-tools.json` does not list `todo`; the
parity tests assert upstream coverage rather than exact tool-set equality.

Reason: agent CLIs (Claude Code's TodoWrite, Codex's update_plan, Kimi's task
list) converge on a model-callable task list as standard scaffolding for
multi-step work, and on a session diff view for edited files. Both need
journaled session state to survive resume, which the upstream session schema
does not carry.

### Claude Code Version User-Agent

For Claude Code OAuth requests, TypeScript pins `user-agent: claude-cli/<version>`
(2.1.280 at the current fixture). Rust reports `claude-cli/<latest published
version>` instead: the model refresh fetches the newest `@anthropic-ai/claude-code`
version from the npm registry, caches it in `model-cache.json`, and falls back to
`DEFAULT_CLAUDE_CODE_VERSION`, which tracks upstream's pin
(`NOEL_CLAUDE_CODE_VERSION` overrides both). The parity test compares the fixture
against that fallback, so it fails when upstream moves its pin.

Reason: Anthropic's API rejects OAuth requests for new models when the
reported Claude Code version is too old ("version X or newer is required"). A
pinned version strands every new model until the next noel release; tracking the
published version keeps OAuth-gated models usable.

### Native Web Tools and Transcript Previews

Rust adds model-callable `web_search` / `web_fetch`, with DuckDuckGo HTML,
optional Brave/SearXNG search, bounded native HTTP fetching, and local fixture
tests. Offline mode suppresses and blocks these network tools. The upstream
local-tool fixture remains a subset, not the complete Rust inventory.

Rust also provides recent-turn `ctrl+o` thinking/tool-output previews,
status-prioritized todos, segmented tool headers, and a visible editable queue.
These are terminal-only UX additions; no browser or web UI runtime is introduced.

### Clipboard Drafts, Conversation Undo, and Session Usage

Rust adds `ctrl+v` clipboard image drafts via bounded native clipboard helpers,
`/undo` (context-only rewind to the latest user message; no file rollback), and
`/usage` (persisted provider-reported tokens plus cached account quota probes).
Token totals include completed requests, are not estimates or dollar costs,
and are not reduced by undo or compaction. Compaction keeps complete recent
user turns instead of splitting tool calls from their results. Summaries are
provider-independent semantic checkpoints generated by the active model with
tools disabled. Automatic triggers use estimated active-context tokens (80% by
default), not a message count. Focused `/compact` and RPC `compact` are supported.
Original journal records survive; checkpoint/context replacement is atomic and
failures/cancellation never fall back to silent truncation. Persistent instructions
remain outside summarization. A failed automatic compaction no longer aborts the
turn: the run continues with the original history and the failure is reported as
an event (system prompt / tools / persistent-instruction / retained-turn token
breakdown when the projected request still exceeds the budget); re-attempts are
skipped until history changes. Repeated compactions merge previous checkpoints;
oversized transcripts use bounded rolling fragments. Provider-native opaque
compaction remains deferred.

Reason: expose useful terminal/session controls without GUI dependencies or
unsafe workspace rollback. Attachment drafts are not injected into text-only
steering/follow-up queues; they stay editable until the active turn ends.

### Owned Transcript Viewport

Rust defaults to an alternate-screen transcript with a fixed composer and
app-owned mouse/PageUp/PageDown scrolling. Latest-follow and anchored-reading
are separate ephemeral view states; reading anchors use entry/logical-line/source
byte positions so streaming and reflow do not move the reader. Esc dismisses
overlays first, then returns to latest while reading without modifying the draft
or interrupting the turn; at latest existing clear/interrupt behavior remains.
The clickable bottom hint marks new output while reading. Moving live thinking
previews freeze while reading and refresh on return to latest.

Reason: long responses should remain readable while the input, attachments,
queue, tasks, and status stay accessible. `--no-alt-screen` retains native inline
scrollback and terminal-owned selection. Owned mode supports grapheme-aware
left-drag selection with copy-on-release, Ctrl+C, and `/copy`; local clipboard
helpers fall back to terminal OSC 52 requests. Selection follows logical text
through reflow/streaming, excludes folded-away output, and stays ephemeral.
Terminal modifier selection and `/export` remain available. View state is not
durable session state.
Local/faux unit and live tmux tests cover this UI; upstream TS fixtures are unchanged.

### Model Cache Refresh Defenses

The model cache refresh is per-provider defensive rather than all-or-nothing.
An empty provider listing keeps the previously cached models instead of wiping
them (a truncated listing is how new models silently disappear); an exhausted
Anthropic pagination budget fails the fetch instead of caching a partial list;
a pass where every provider failed leaves `refreshedAt` untouched so the next
startup retries immediately instead of waiting out the TTL; and the cache is
re-read right before writing so concurrent noel processes cannot clobber each
other's fetched models. Refresh diagnostics persist into the cache and surface
through `/diagnostics`. `/models refresh` runs a foreground refresh with a
per-provider report and `/models reset` deletes the cache so the next refresh
starts from built-ins plus explicit `models.json` entries only.

Reason: model lists are provider-owned and change out of band; a stale or
poisoned cache must never delete working models, and the user needs a manual
escape hatch when the background refresh cannot recover on its own.

### Built-in Model Catalog Is a Compiled Snapshot

Rust's built-in models are upstream's catalog, generated from upstream's model
runtime into `tests/fixtures/ts-parity/model-catalog.json` and compiled in
(`crates/noel-ai/src/catalog.rs`); tests compare every model and every field
Rust reads with that file. These parts differ:

- No catalog refresh. Upstream wraps each built-in provider with a refresh
  from its hosted catalog service (`models-store.json`, `pi update --models`)
  and narrows OpenRouter to the models, limits, and prices of the user's key.
  Rust has neither; its own model refresh still fetches listings for seven
  providers (see Model Cache Refresh Defenses and the next entry). Upstream
  itself generates the catalog from the providers' listings at build time, so
  Rust's snapshot is as old as the last regeneration of the fixture.
- No Radius models. Upstream's Radius gateway speaks its own `pi-messages`
  API, for which Rust has no adapter, so its 29 models are in the file but not
  offered: `--model radius/...` is "not found", `--model balanced` (an id only
  Radius has) matches nothing, and the error for an id several providers
  have does not list Radius.
- Chat models only. Upstream's image and classifier models are not in the
  file; Rust keeps its own short list of image models.
- Fields Rust does not read yet: `compat`, `headers`, `inputLimits`,
  `promptCache`, and `type`. So request details upstream takes from them
  still come from Rust's own rules:
  - Thinking on the chat completions API is sent only to OpenRouter. The
    other chat completions providers (DeepSeek, Z.AI, Hugging Face, OpenCode,
    Qwen, Moonshot, Groq, ...) and Mistral have thinking levels in the
    catalog, and Rust tracks and clamps them, but their requests carry no
    thinking parameter: each needs the provider's own format
    (`compat.thinkingFormat`).
  - Anthropic and Bedrock choose adaptive or budget thinking, and Google
    chooses its thinking budget, from the model id, not from `compat`.
  - The catalog's per-model headers (GitHub Copilot, NVIDIA) are not sent.
    GitHub Copilot's Responses models use Rust's own fixed headers; its
    models on the Anthropic Messages and chat completions APIs are listed but
    get the plain headers of those APIs.

Reason: fetching upstream's hosted catalog and OpenRouter's key listing needs
network destinations this port has no approval for, and reading `compat` is
its own piece of work. A compiled snapshot gives every model its limits,
prices, and thinking levels without either.

### Port-Only Providers Beside the Catalog

Four providers exist only in Rust and are written by hand in
`crates/noel-ai/src/catalog_port_models.json`, in the catalog's shape: `faux`
(`echo`, `script`), `zai-coding`, `zai-anthropic`, and `kimi-coding-openai`.
They come first in the model list, never redefine an upstream provider (a
test checks), and have no thinking levels. Upstream's `zai` is the coding
plan endpoint (`https://api.z.ai/api/coding/paas/v4`), so `zai-coding` now
duplicates it under another id; `zai/glm-5.2` and `zai/glm-5.3`, which this
port used to send to the general endpoint (`.../api/paas/v4`), follow
upstream's `zai`.

Reason: the faux models are how tests and offline runs work, and the three
providers are endpoints this port's users have configured and saved as
defaults. Keeping them in a separate file keeps the generated catalog exactly
upstream's.

### Provider Listings and `models.json` Extend the Catalog

Rust's model refresh caches provider listings (`model-cache.json`), and
`models.json` is Rust's own format, a list of model objects. Upstream's
`models.json` (`providers` with `models` and `modelOverrides`) is not read.
Both add to the catalog:

- A cached listing says which models a provider serves. A listed id the
  catalog has changes nothing; one it lacks is built on the provider's default
  model, as upstream builds `--model provider/new-id`, keeping the listing's
  name and context window. Upstream has no such listings for these providers.
- A `models.json` entry is used as written (provider, id, name, API, base
  URL). The fields upstream's model object has and the entry leaves out
  (`contextWindow`, `maxTokens`, `reasoning`, `thinkingLevelMap`, `input`,
  `cost`) come from the built-in model of the same id, else from the
  provider's default model. Upstream gives a custom model fixed defaults
  instead (no reasoning, 128000 context, 16384 output tokens).
- For a provider the catalog does not know, a model has no thinking levels,
  and requests carry no output cap unless the entry gives `maxTokens`; only
  the Anthropic API, which requires one, gets upstream's 16384. Upstream sends
  16384, lowered to fit the context window, on every API.
- A base URL names the API's root for every provider, as it does upstream.
  Earlier Rust versions appended only `/messages` to a custom `anthropic` base
  URL and `/chat/completions` to a Mistral one; they now get `/v1/messages`
  and `/v1/chat/completions` unless the URL already ends in `/v1`.

Reason: the listings keep new models usable between catalog updates, and a
model nobody described is safest on its provider's defaults. The `models.json`
format, with credential syntax and offline mode, is a later piece of work; the
model object already has upstream's field names so upstream's file can map
onto it.

### Provider Base-URL Variables

`ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` point the `anthropic` and `openai`
providers at another address:
- every built-in model of the provider follows the variable;
- a `models.json` entry with no `baseUrl` follows it too;
- an entry that names its own `baseUrl` keeps that address.

Each value follows its provider's own convention: Anthropic's has no `/v1`,
and noel adds `/v1/messages`; OpenAI's includes `/v1`. Upstream reads neither
variable and takes each model's address from its catalog or `models.json`.

Reason: Claude Code and the OpenAI SDK read these variables, and environments
that keep provider credentials out of the workload rely on them. A Codewire
Sandbox sets them to its local credential proxy, together with a placeholder
credential that the proxy replaces. noel ignoring them would send the
placeholder straight to the provider.

### Model Scope, Picker, and Model Commands

`--models` and `enabledModels` resolve as upstream's scope does (exact, fuzzy,
and glob patterns with `:<thinking>`, over the models with credentials), and a
scope removes no model. These parts differ:

- The model picker lists the scope when there is one and every known model
  otherwise. Upstream lists the models with credentials, with a tab to switch
  between the scope and all of them, and cycles through the scope with a key.
- Scope globs support `*`, `?`, `[...]`, `**`, `{a,b}`, `{1..3}`, and a
  leading `!`, as recorded from upstream's matcher in
  `model-resolution.json`; its extended globs (`+(a|b)`) are not supported.
- `--list-models` lists every known model, in Rust's own format. Upstream
  lists the models with credentials, in columns.
- `/model <text>` switches on an exact `provider/id` or id and otherwise
  opens the picker with the text as its search, as upstream's does. It also
  accepts a 1-based position in the model list, and it saves the model as the
  default; upstream's saves nothing.
- `/thinking` alone prints the level in effect; upstream opens a selector.
- Thinking levels also answer to the names earlier Rust versions accepted
  (`none`, `disabled`, `min`, `med`, `extra-high`), ignoring case, in
  `--thinking`, `/thinking`, `set_thinking_level`, and settings. A
  `:<thinking>` suffix on `--model` or a scope pattern takes upstream's seven
  names only, because any other suffix is part of the model id.
- The Azure provider is `azure`, as upstream names it. The id Rust used
  before, `azure-openai-responses`, still names it in an `auth.json` entry
  (which stays under that key), `defaultProvider`, `enabledModels`,
  `modelThinkingLevels`, `models.json`, the model cache, a saved session,
  `--provider`, and `--model`.

Reason: the picker's tabs, key, and authenticated-only list are separate TUI
work. The former names cost nothing to keep and saved files hold them.

### Session Model and Thinking Level

Startup follows upstream's order (`--model`, the resumed session, the scope,
the saved default, a model with credentials), and a thinking level is clamped
to the model as upstream clamps it. These parts differ:

- A resumed session keeps its model even when the model has no credentials
  now, and when the catalog no longer lists it: the id then runs on its
  provider's default model, as `--model provider/new-id` would. Upstream
  falls back to its default model with a message in both cases.
- The saved default (`defaultProvider`, `defaultModel`) is used whenever its
  provider has credentials, listed or not, and an unlisted id runs the same
  way. Upstream ignores a saved default its catalog does not list and starts
  on a provider's default model. Nineteen ids this port listed by hand are
  not in upstream's catalog (for example `openai-codex/gpt-5.4`,
  `zai/glm-5.1`, `moonshotai/k3`), so settings and sessions that name them
  depend on this.
- Rust saves a thinking level in the session only once something sets one
  (`--thinking`, a `:<thinking>` suffix, a scope pattern, `/thinking`,
  `set_thinking_level`, a model switch). Until then the level in effect is
  read from `modelThinkingLevels` and `defaultThinkingLevel` each time, so a
  change to those settings reaches a session that never set a level.
  Upstream saves the level when it creates the session.
- A `faux` model is the startup model when it is the saved default or when
  nothing else can run. Upstream starts without a model then.

Reason: a session's model is the user's choice for that conversation, and
switching it silently is worse than failing on the next request. The faux
fallback keeps a fresh install usable offline.

### auth.json Lock and Mode

Upstream serializes `auth.json` updates with a `proper-lockfile` directory
(`auth.json.lock`, stale after 30 seconds) and applies mode 0600 only when it
creates the file. Rust takes an advisory `flock` on a sibling
`auth.json.flock` and replaces `auth.json` by an atomic rename, so every write
leaves it 0600.

Reason: the operating system releases a `flock` when its holder exits, so no
stale-lock recovery is needed, and the separate name never collides with an
upstream process's lock directory. An atomic rename cannot keep the previous
file's mode, and credentials should never be left group- or world-readable.

### Exact-Match edit Tool

The `edit` tool follows upstream's rules (unique `oldText`, no overlapping
edits, all edits applied at once, empty `newText`, `edits` as a JSON string,
BOM and CRLF kept) but matches exactly: it does not fall back to upstream's
fuzzy matching of smart quotes, Unicode dashes and spaces, and trailing
whitespace. It also converts only the edited text to the file's line endings
instead of normalizing a mixed-ending file.

Reason: an exact match cannot edit the wrong region, and bytes outside the
edits stay untouched.

### Configurable Agent Round Cap

The per-run provider round cap (`agent.maxToolRounds`, default 200, including
steering rounds) is configurable in settings and reports a dedicated
round-limit error that names the cap and the setting instead of masquerading
as an invalid tool call.

Reason: the default is a runaway-loop bound, not a usage quota; long working
sessions legitimately exceed it and the recovery path (continue or raise the
cap) should be discoverable from the error itself.

### Batched Queue and Steering Dispatch

Queued prompts and mid-turn steering now dispatch as one combined message
instead of one-at-a-time: everything pending at an injection point (before a
provider round, or when the turn ends) joins into a single user message
separated by `---`, so a burst of queued thoughts reads as one instruction
rather than several consecutive turns. Queued tool commands (`/read`, `/grep`,
…) still dispatch individually ahead of prose because their arguments are
positional. The `steeringMode`/`followUpMode` settings are ignored (a legacy
`queueMode` value is still migrated) since there is no mode left to choose.

Reason: serial one-per-turn delivery made a queued burst look like five
separate conversations with the model, and made steering feel broken when
later messages landed turns after the user sent them.

### Process-Group Kill for Timed-Out Bash

A bash tool call that hits its timeout now kills the shell's whole process
group instead of only the shell pid. Previously the children of a timed-out
command (for example a long preflight script) were orphaned and kept running
indefinitely, consuming resources and confusing later turns that found them
still alive.

Reason: `kill_on_drop` on the tokio child kills one pid; shells spawn trees.
The shell leads its own session and process group and the timeout path issues
`killpg`, so the tree dies with the call.

### Interactive Session Picker

`/resume` without arguments opens an interactive picker in the
Kimi/Claude/Codex style: one line per session with the first user prompt as a
topic preview (truncated), relative age (`5s`, `2h`, `3d`), message count,
active model, and an `unfinished` marker for sessions that ended mid-turn.
Typing filters, Enter resumes, Esc cancels. Bare `noel` startup deliberately
does not auto-open the picker: scripting and e2e harnesses send their first
prompt immediately, and trapping that text in a selector would break them;
the industry CLIs also gate the picker behind an explicit resume action.

Reason: the previous flat `/resume` list showed only ids and names, which
made picking the right session impossible without opening journals by hand.

### Default System Prompt

Rust sends upstream's system prompt (`core/system-prompt.ts`) and reads the
files upstream builds it from. `system_prompt_matches_ts_builder` compares
the text with `system-prompt.json` to the byte, and
`prompt_and_context_files_match_ts_discovery` compares the files chosen. The
differences:

- noel's names. The first sentence says "operating inside noel, a coding
  agent harness" where upstream's names pi, and bash's rule reads "You can
  inspect NOEL_* environment variables for current model and session
  details." where upstream's names `PI_*`; both sets of variables exist (see
  Named noel, With Its State in `~/.noel`). The parity test reads upstream's
  text through `noel_config::with_noel_names`, which changes exactly these
  names.
- No `<docs>` section. Upstream's points at the README, docs, and examples
  its package installs, and tells the model to read them for questions about
  pi. Rust installs one binary and no documentation, so a path would dangle;
  a link to this repository would need network access and a model that
  fetches it. The parity test drops exactly this section from upstream's
  text.
- The `<skills>` section is upstream's, and present while `read` or `bash`
  is active. Upstream also keeps it, naming no tool, when an extension hides
  the reader from the model; Rust has no hidden tools.
- Rust enables every builtin tool by default; upstream starts with `read`,
  `bash`, `edit`, and `write`. So the default prompt also lists `grep`,
  `find`, and `ls`, and leaves out the rule "Use bash for file operations
  like ls, rg, find", as upstream's does once those tools are on (the
  fixture's `all-tools` case).
- `todo`, `web_search`, and `web_fetch` exist only in Rust, so their lines in
  the tool list are Rust's own: "Track multi-step work as a checklist",
  "Search the web for current information", and "Fetch a web page as
  readable text". They add no rules.
- Extension tools are listed in name order, the order Rust declares them in a
  request. Upstream lists them in the order extensions registered them.
- The `systemPrompt` and `appendSystemPrompt` settings are Rust's own.
  `systemPrompt` ranks below `--system-prompt` and above `SYSTEM.md`.
  `appendSystemPrompt` entries come before the `--append-system-prompt` text,
  and either of the two replaces `APPEND_SYSTEM.md`. Upstream has only the
  flags and the files.
- Extensions cannot change the prompt beyond a tool's `promptSnippet` and
  `promptGuidelines`. Upstream's `before_agent_start` handlers can replace
  the prompt, add rules and sections, and hide tools.
- The prompt is not a transcript message. Upstream records it as the leading
  system message and sends later changes as section updates (upstream
  #9548); Rust rebuilds the prompt each turn and sends it whole (see RPC
  Mode).
- A `SYSTEM.md`, `APPEND_SYSTEM.md`, or context file that cannot be read is
  listed in `/diagnostics` and skipped. Upstream prints a warning, and for
  the two prompt files uses the file's path as the prompt text.
- With `--no-session`, bash's `NOEL_SESSION_ID` is `ephemeral`, Rust's id for a
  session without a journal; upstream generates an id for it.
- The rules for the `powershell` tool are not ported (see Windows-Only
  PowerShell Tool).

Reason: the prompt is how the model learns its role, its tools, and the
project's instructions, so Rust keeps upstream's text and order and diverges
only where it has nothing to point at (documentation, extension hooks) or has
something upstream lacks (its own tools and settings).

### Skills and Prompt Templates

Rust loads, lists, and expands skills and prompt templates as upstream does
(`core/skills.ts`, `core/prompt-templates.ts`, and the session's expansion
in `core/agent-session.ts`). `skills.json` holds upstream's results, and
`discovery_matches_ts_resource_loader`, `frontmatter_matches_ts_parser`,
`skills_listing_matches_ts_formatter`, `template_arguments_match_ts`,
`input_expansion_matches_ts_session`, and `get_commands_matches_upstream`
compare with them exactly. The differences:

Loading:

- Frontmatter is read by a parser for the part of YAML the Agent Skills
  specification uses: one mapping of plain or quoted scalars, booleans, and
  `|` / `>` block scalars. A nested mapping, a sequence, or a flow collection
  is accepted as a value that is not a string. Anchors, aliases, tags,
  complex keys, directives, and a frontmatter that is one flow mapping are
  malformed here and load in upstream. A file with malformed frontmatter is
  reported and not loaded, as in upstream; the message is Rust's, not the
  YAML library's (the fixture records it as `<yaml-error>`).
- A directory's entries are read by name. Upstream reads them in the order
  the filesystem lists them, so which of two same-named skills in one
  directory wins is not defined there. The fixture script lists directories
  by name to record a stable order.
- Ignore files (`.gitignore`, `.ignore`, `.fdignore`) use Rust's matcher: `*`
  and `?`, a trailing `/` for directories, and `!` to negate; a pattern with
  a slash matches the path below the top directory and one without matches a
  name at any depth. Upstream applies full gitignore rules (`**`, character
  classes, escapes).
- The `skills` and `prompts` settings name paths. Upstream also reads
  pattern entries there (`!pattern`, `+path`, `-path`, and globs), which
  narrow the named paths and switch discovered resources on and off. Rust
  reads none; it has `disabledResources` and `noel config enable|disable`,
  applied after loading, so disabling the skill that won a name does not
  load the one it beat. A setting's path that does not exist is listed in
  `/diagnostics`; upstream passes over it.
- Packages are Rust's local package directories. The project's `packages`
  setting replaces the user's instead of adding to it, so package resources
  are the project's when the project lists packages and the user's
  otherwise. Upstream's extensions can add skill and template paths
  (`resources_discover`, and packages given with `-e`); Rust's cannot.
- A `file://` URL as a resource path is not resolved.
- Under the test harness (`NOEL_TEST_ISOLATION`), `~/.agents/skills` is not
  read.
- Diagnostics appear in `/diagnostics` and the footer's count, all alike.
  Upstream prints skill and prompt conflicts at startup, a
  `--prompt-template` path that does not exist as an error and the rest as
  warnings.

Use:

- Input is expanded where each mode takes it (`noel_core::expand_input`): the
  TUI on submit, on steering, and on queueing, print mode for its prompt, and
  RPC mode for `prompt`, `steer`, and `follow_up`. Upstream expands in its
  session, after extension commands and `input` handlers, which Rust lacks.
- Rust's slash-tool prompts (`/read <path>`, `/bash <command>`, `/write`,
  `/edit`, `/grep`, `/find`, `/ls`, `/web-search`, `/web-fetch`) come before
  skills and templates in every mode, so a template with one of those names
  is not reachable by its command. A template's expansion is submitted as
  typed text: one that begins with a slash-tool command runs that tool.
  Upstream has no slash-tool prompts.
- A skill whose file can no longer be read when its command is used is sent
  as the command's text, as in upstream, without upstream's error event.
- Print mode joins its arguments into one prompt, so `noel -p /fix 42` is the
  command `/fix 42`. Upstream sends each argument as its own prompt.
- TUI: `/skills`, `/prompts`, and `/prompt <name> [input]` are Rust's own.
  `/prompt` sends the template's text without its frontmatter, with
  `{{input}}` replaced by the input or the input appended; it substitutes no
  `$1` placeholders. A template named like a TUI command is reachable only in
  print and RPC modes, as with upstream's built-in commands.
- TUI: a skill command or template entered while a turn runs is queued as a
  follow-up, as any text entered then is, or as steering with the steer key.
  Upstream steers it. Other unknown commands are refused mid-turn.
- TUI: the transcript shows a submitted skill command as it was typed, and a
  resumed session shows the saved `<skill>` block as text. Upstream renders
  the block collapsed in both.
- TUI: completion matches commands by prefix, a skill also by its bare name;
  upstream matches fuzzily. Skill commands always complete: upstream's
  `enableSkillCommands` setting is not read.

Reason: a skill or template written for upstream, or for another Agent
Skills tool, should load and read the same here, so the loading order, the
listing, and the expansions are upstream's to the byte. Rust differs where it
has no YAML library, no extension runtime, and its own commands and queue.

### Windows-Only PowerShell Tool

Rust does not ship upstream's `powershell` builtin tool. Upstream registers it
on every platform, so `local-tools.json` lists it, but leaves it out of the
default tool selection and executes it only on Windows: `getPowerShellConfig()`
throws "The powershell tool is only available on Windows." elsewhere, and the
CLI help describes it as "Execute PowerShell commands on Windows". The
`local-tools.json` parity test (`builtin_tools_cover_upstream_ts_names`)
exempts exactly this tool and fails once Rust defines it, so the exemption and
this entry go together.

Reason: noel targets Linux and macOS terminals, where the upstream tool can only
fail, and Rust enables every builtin tool by default, so porting it would offer
the model a tool that cannot run. `bash` covers shell execution on the supported
platforms. Revisit if noel adds Windows support.

### All Built-in Tools Enabled by Default

Upstream enables `read`, `bash`, `edit`, and `write` at startup and leaves
`grep`, `find`, and `ls` off until `--tools` or `defaultTools` names them. Rust
enables every built-in tool it has: those seven and its own `todo`,
`web_search`, and `web_fetch`. So a `defaultTools` or `--tools` list of only
`+name`/`-name` entries starts from ten tools in Rust and four upstream, and
`-grep` removes a tool that upstream never had on.

Reason: this is how the Rust port has always started, recorded here rather
than chosen in this entry. Which tools are on by default is a product decision
for the owner; the selection settings and flags already follow upstream, so
changing the default set is a one-line change to that list.

### Tool Selection Is Computed From Settings and Flags

`defaultTools`, `--tools` (an allowlist with `*` patterns, or `+name`/`-name`
entries), `--exclude-tools`, `--no-builtin-tools`, and `--no-tools` select
tools as upstream's do, and `settings.json` layers merge `defaultTools` as
upstream merges them (`settings.json` fixture). These parts differ:

- Rust computes the selection from the settings and the flags at startup and
  again on `/reload`. Upstream keeps an active-tool list in the session: its
  `/reload` only adds tools newly named in `defaultTools`, and extensions can
  change the list with `setActiveTools()`. In Rust a tool removed from
  `defaultTools` is gone after `/reload`.
- `enabledTools` is a Rust-only setting: a persistent allowlist, like `--tools`
  with plain names.
- Upstream's `defaultActive: false` extension tools (`codemode`,
  `tool_search`) and its MCP rules for `--tools` do not exist in Rust; every
  extension tool is on unless an allowlist or `--exclude-tools` leaves it out.
- Tool flags are not saved with the session. Earlier Rust versions saved them
  as the session's disabled tools; a saved list is cleared when the session is
  opened, so a resumed session follows the flags of the run that resumes it.
- Rust declares the built-in tools in its own fixed order, then the extension
  tools by name, whatever order `--tools` or `defaultTools` names them in.
  Upstream declares tools in the order the selection names them. The system
  prompt lists tools in the declared order, so its list differs the same way.

Reason: Rust's reloadable systems are rebuilt from configuration, and durable
session state is kept apart from them (see the reload rules in `AGENTS.md`).
A selection computed from settings and flags needs no session record, and the
flags mean the same on every run.

### Built-in Tools Run In-Process

Upstream's `grep` runs ripgrep and its `find` runs fd, downloading them when
they are missing. Rust searches in-process with ripgrep's own `ignore` and
`globset` crates and the `regex` crate, passing the options upstream passes
(`rg --hidden [--ignore-case] [--fixed-strings] [--glob]`, `fd --glob
--hidden`). `tool-behavior.json` records upstream's results for 93 calls and
`crates/noel-core/src/tool_parity_tests.rs` requires Rust's to be identical.
Outside those calls these differences are known:

- Order. ripgrep and fd walk in parallel, so upstream's results have no fixed
  order and a `find` limit keeps whichever results arrived first. Rust lists
  files in path order and a limit keeps the first in that order. (The fixture
  runs ripgrep with `--sort=path` and sorts or counts fd's lines.)
- Configuration of the binaries does not apply: `RIPGREP_CONFIG_PATH` and
  fd's global ignore file. `.gitignore`, `.ignore`, `.rgignore`, `.fdignore`,
  `.git/info/exclude`, and git's global excludes file do.
- Binary files. ripgrep reads a file in 64 KB blocks and stops at a block
  holding a NUL byte; Rust searches the lines before the 64 KB block that
  holds the first NUL, which is the same file prefix only approximately.
- UTF-16 files are not transcoded; ripgrep recognizes their byte order mark.
- A file or directory that cannot be read is skipped. ripgrep reports it and
  exits with status 2, which makes upstream fail the whole call.
- Error text is Rust's: an invalid pattern gives the `regex` crate's message
  without ripgrep's `rg:` prefix, a pattern containing a newline gives a
  one-line error, and a `find` path that does not exist gives `Path not found`
  rather than fd's message.
- `find` ignores case when the glob has no uppercase character; fd decides
  from the regular expression the glob compiles to.
- `ls` sorts ASCII names in upstream's collation order; names with other
  characters sort after them by code point, where upstream uses the locale's
  collation.

Reason: a native binary should not download or depend on other binaries, and
the two crates are the ones ripgrep and fd walk with, so ignore rules and globs
behave the same. Path order makes results repeatable.

### Shell Tool Differences

The `bash` tool follows upstream's (`bash -c`, no default timeout, the last
2000 lines or 50 KB with the whole output saved to a temp file, a timeout that
keeps the output). These parts differ:

- stdout and stderr share one pipe, so the output is in the order the command
  wrote it. Upstream reads two pipes and appends chunks as they arrive, which
  can reorder writes that come close together.
- A running command's output is reported as upstream reports it (all of it
  so far, within the result's limits, at most every 100 ms), but without
  upstream's `details`: the truncation data and the path of the saved output
  come with the result only.
- Upstream puts its managed tool directory (`~/.pi/agent/bin`) on the
  command's `PATH`. Rust has no such directory.
- A timeout is kept in milliseconds, so the notice prints at most three
  decimals of the seconds the model gave.
- `!` and `/bash` in the TUI run through the same tool with a 120 second
  timeout, show their output when they end, and cannot be interrupted on
  their own. Upstream runs them without a timeout, shows their output while
  they run, strips terminal escapes from it, and lets Esc cancel them.

Reason: one pipe is the only way to keep the order exact, and it makes results
repeatable. The TUI's command UX is separate work; the timeout keeps a command
that never ends from blocking the TUI until then.

### File Tool Differences

`read`, `write`, and `edit` resolve paths and word their results as upstream
does, including the common failures (`ENOENT: no such file or directory,
access '<path>'`, `Could not edit file: <path>. Error code: ENOENT.`). These
parts differ:

- Other I/O failures are worded by Rust (`failed to access <path>: <error>`),
  not by Node.
- `read` tries the macOS screenshot spellings of a missing path (a narrow
  no-break space before AM/PM, a curly apostrophe) but not the decomposed
  Unicode (NFD) spelling upstream also tries.
- Images are resized with the `image` crate rather than upstream's Photon, so
  a resized image is not byte-identical; the limits (2000 pixels a side, 4.5 MB
  of base64), the PNG-then-JPEG candidates, and the notes are the same.
- `edit` matches exactly (see Exact-Match edit Tool) and, like `write`,
  returns no diff: tool results are text.

Reason: the remaining differences are either invisible to the model or need a
dependency (Unicode normalization) for a rare case.

### Tool Result Images Depend on the Provider API

Upstream sends the images of a tool result (`read` of an image file) to every
model whose catalog entry lists image input, and replaces them with a note for
other models. Rust's catalog records each model's `input` too, but nothing
reads it yet, so Rust still decides by provider API:

- Anthropic Messages, OpenAI Responses (OpenAI, Codex, Azure, GitHub Copilot),
  Google Gemini and Vertex, and Bedrock Converse: the image is sent in the tool
  result (for Gemini 2, in a user turn after it), to every model.
- OpenAI chat completions and Mistral: the image is not sent. The tool result
  text ends with `(tool image omitted: this provider API does not accept images
  in tool results)`. Upstream sends it in a user message after the tool
  results.
- Upstream adds `[Current model does not support images. ...]` to the `read`
  result for a model without image input. Rust never does.
- With `images.blockImages`, `read` returns `Image reading is disabled.` in
  place of the image. Upstream returns the image and removes it from each
  request, so turning the setting off later sends it; in Rust it is not saved.

Reason: chat completions is the API of most text-only models, and a tool
message there cannot hold an image. This rule predates the catalog; sending an
image to a model that does not accept one risks an error on every later
request of the session, because the image stays in the conversation. The four
APIs that carry images in tool results serve models that almost all accept
them. The catalog now says which models accept images; deciding by it is
separate work.

### No MCP Support

Rust has no Model Context Protocol client. Upstream's `pi mcp` command
(`list`, `login`, `logout`, `add`, `remove`), the `/mcp` slash command, MCP
tools, and `mcp.json` are not ported. `noel mcp` exits with an error instead of
sending "mcp ..." to the model as a prompt, and `--no-mcp` is accepted as a
documented no-op so upstream invocations keep working. The CLI contract test
(`cli_contract_covers_upstream_ts_options_and_commands`) exempts exactly the
`mcp` command and requires this entry.

Reason: MCP is a large protocol surface (stdio and HTTP transports, OAuth,
tool exposure modes, codemode) outside the native CLI/TUI scope today.
Revisit if noel adds an MCP client.

### Local `/bug` Reports for This Port

`/bug [description]` writes `noel-bug-report-<id>.md` to the current directory
(noel, OS, and terminal versions, model and provider configuration without
credentials, extensions, redacted settings, configuration diagnostics, and
errors shown in the session) and prints a new-issue link for
https://github.com/noeljackson/noel/issues. Upstream uploads the report to its
Radius gateway or exports a zip, can attach the transcript or a model-written
summary, records a `pi.bug-report` session entry, attaches its crash log, and
suggests `/bug` after unexplained errors. Rust does none of these; the
transcript is shared only through an explicit `/export`. Error text is
redacted before it is written: secret query values in any URL it mentions,
`Bearer`/`Basic` credentials, and API-key-shaped tokens (`sk-…`, `AIza…`,
GitHub, Slack, and AWS access key ids).

Reason: upstream reports go to the upstream developers' service, while this
port's bugs belong in its own tracker. A local file that the user reviews and
attaches keeps upstream's consent model without network calls or a zip
dependency.

### `--use-theme` Takes One Theme Name

`--use-theme <name>` selects the initial theme for one run without saving it,
as Rust's `--theme` does. Upstream also accepts a `light/dark` pair and picks
a half from the terminal's reported background. Rust has no terminal
appearance detection, so it rejects pairs with an error, and a `light/dark`
`theme` setting still falls back to the system theme with a diagnostic.
`--use-theme` also conflicts with `--theme` and `--no-themes`: upstream
combines them because its `--theme` loads theme files and `--no-themes` only
skips discovery, while Rust's `--theme` selects the run's theme and
`--no-themes` clears it, so one flag would silently override the other.

Reason: choosing a half without asking the terminal would silently guess.

### `noel auth` Named Accounts

`noel auth check|print-api-key|print-bearer-token` follow upstream's options,
output, and exit codes, and add `--account <name>` to choose one of noel's named
accounts. Credentials resolve in noel's usual order (stored account,
environment, imported CLI logins), so environment keys and imported logins
count as configured; upstream's `--model` lookup and bearer-token printing
consider only stored credentials. With `--account`, a `--model` several
providers serve resolves to the providers that have that account.

Reason: noel stores several accounts per provider (see Named Multi-Account
auth.json), and `noel auth` should report the credential a session would use.

### `noel auth` Refreshes Imported Logins Only Once They Expire

`print-bearer-token` and `check` refresh noel's own OAuth tokens that expire
within `--min-expiry` (default 30m) or five minutes, as upstream does. A login
noel reads from Claude Code's `~/.claude/.credentials.json` or Codex's
`~/.codex/auth.json` refreshes only once it expires, as on the request path;
a sooner expiry is printed, or reported ready, with a warning on stderr.
`check --json` adds `"expiresSoon": true` to a ready OAuth token that expires
within five minutes, a field upstream's output lacks.

Reason: those CLIs rotate single-use refresh tokens and noel never writes
refreshed tokens back to their files, so an early refresh from `noel auth` would
sign the user out of Claude Code or Codex. After noel refreshes such a login it
keeps its own `claude-import`/`codex-import` account, which refreshes early
like any stored account.

### Run Recovery

noel retries a failed model request and answers a context overflow with one
compaction and one more request, as upstream's agent session does after a
failed run. `error-classification.json` holds upstream's rules and
`rpc-protocol.json` its records for both; noel is compared with each. These
parts differ:

- Upstream's patterns read the error text, and noel applies them to its own
  text for what the provider answered: an HTTP status with its body, or a
  response noel could not use. A transport failure (no connection, a timeout, a
  stream that broke) is retried by its kind instead, because its text is the
  HTTP library's and not the one the patterns expect. noel's own errors (no
  credential, an invalid header, its configuration, a faux script that ran
  out) are never retried, whatever their text contains.
- A reply the run drops is not kept. Upstream saves the failed or cut-off
  attempt and hides it from the model with a `context_edit` journal entry, so
  its history and exports still show it. noel keeps a reply in the
  transcript only once it stands, or once it is cut off for good (see
  `docs/spec.md` §4.2): what a retried or overflowed attempt streamed is
  saved as it goes and superseded as the turn asks again, so it leaves
  nothing in the transcript, and a noel killed during a recovery resumes
  from the prompt, or from the compaction checkpoint once that is written.
- A final reply cut off at the output limit stands when nothing replaces it:
  there is nothing to compact, compaction is off, or the compaction fails.
  Upstream hides the reply from the model in those cases too, so its next
  request no longer contains it. noel could only drop it altogether, and it
  may be the only answer the user got, so noel keeps it: saved, and sent with
  the next request.
- Upstream decides whether a cut-off reply could have been longer from the
  model's `maxTokens` (`isRecoverableLength`), and so does noel, with the
  catalog's number: a final reply cut off below it is an overflow, and one
  that reached it stands. Only a `models.json` model of a provider the catalog
  does not know has no such number, when its entry gives no `maxTokens` and
  it is not served over the Anthropic API (see Provider Listings and
  `models.json` Extend the Catalog). noel takes that model's cut-off final
  reply for a context overflow and compacts once. Upstream gives the model
  16384 and lets a reply that reached it stand.
- Upstream also reads a reply that completed as an overflow when the usage
  it reports exceeds the context window, and compacts right after it. noel
  compacts before the next request, when its own estimate crosses the
  threshold.
- After a second overflow in a row upstream also reports a failed compaction
  ("recovery failed after one compact-and-retry attempt"). noel ends the turn
  with the provider's error alone.
- `retry.provider.timeoutMs`, `retry.provider.maxRetries`, and
  `retry.provider.maxRetryDelayMs` are read and not used: noel has no retries
  inside one provider request and does not follow `Retry-After`. Only the
  retry above applies.
- In the TUI a retry or an overflow is a system line, text the dropped reply
  had streamed stays above it (thinking included), and Esc during a backoff
  interrupts the turn. Upstream shows a retry indicator with the attempt and
  the delay, and Esc during the backoff cancels only the retry.

Reason: the rules, the single compact-and-retry, and the records are
upstream's, so a run survives what upstream's survives and a client sees the
same protocol. The differences follow from noel's journal, which saves what the
model will see again and nothing else, and from its own HTTP stack.

### Durable Partial Output

A streaming reply's text and thinking, and a running tool call's output, are
saved as they arrive, and recovered after a stop, as upstream Durable does
(`docs/spec.md` §4.2, `durable/docs/spec.md:3735-3745, 3999-4006`): at most
every `progress.partialIntervalMs` and `progress.outputIntervalMs` (default
100), a committed partial becomes an aborted reply that later requests leave
out, and a call that does not run again gets an interrupted result holding
its saved output. These parts differ:

- Upstream overwrites a live document; noel's journal only grows. So a
  reply's records append what streamed since the last one, and a tool's
  output is appended while it grows. Once the output passes its result's
  limits the kept end moves and each record rewrites it, at intervals that
  double from one rewrite to the next; upstream waits the interval and the
  rewrite's size at 100 KiB/s. A crash then loses the output since the last
  rewrite, which can be more than one interval.
- A reply's first change after a quiet interval is saved at once; upstream
  saves a reply's first partial one interval after it arrives.
- The saved partial holds the reply's text and thinking, not the tool calls
  it was streaming or its usage. An abort saves everything the reply
  streamed, including what the last interval had not saved yet; upstream's
  abort converts what was committed.
- What a stop left is recovered when the session is next opened for
  writing, before anything else happens to it; upstream converts it when the
  request is sent again, or when the run is settled otherwise.
- A failure that ends the run keeps what the reply streamed as an aborted
  reply. Upstream keeps the failed reply as an error reply, with its error
  text and usage.
- The interrupted result is one text: the saved output, a blank line, and
  upstream's `<harness>` block. Its truncation line has no line and byte
  counts (`[warn] Output truncated to its end`), which noel does not save.
- An abort's result for a tool call is published pi's `Operation aborted`
  after what the call printed, not Durable's `aborted` diagnostic.

Reason: an append-only journal must not grow with every save of a long reply
or a chatty command. The rest keeps RPC mode's records published pi's, and
saves what noel's journal can say.

### RPC Mode

`noel --mode rpc` speaks upstream's RPC protocol, with four additions listed in
`get_state.rpcExtensions`: `run-id` (every run has a `runId`),
`expected-run-id` (`steer`, `follow_up`, and `abort` change nothing unless
their `expectedRunId` names the active run), `request-id` (a `prompt` that
repeats a `requestId` gets the run the first one started), and `resume-run`
(`get_state.lastRun`, and `resume_run` to continue an interrupted run).
`crates/noel-cli/tests/rpc_parity.rs` compares every scenario of
`rpc-protocol.json` with noel's records and allows exactly the differences in
the first list; its `normalize`, `expected`, and `without_own` functions
name them, so the two lists change together. The extensions' keys
(`runId`, `requestId`, `lastRun`, `rpcExtensions`) are the allowed extras.

Differences in the golden records:

- No system messages: upstream's transcript starts with a system message
  (upstream #9548, which noel has not ported), reported by `message_start` and
  `message_end` and kept in `agent_end` and `get_messages`. noel reports none:
  it sends its system prompt with every request and keeps it out of the
  transcript.
- `messageCount` counts the messages noel keeps: not upstream's system
  message.
- Queue modes: noel delivers everything queued at once (see Batched Queue and
  Steering Dispatch). `steeringMode` and `followUpMode` are always `all`, and
  setting `one-at-a-time` fails.
- Model object: `faux/script` has no `baseUrl`, and its `input` and
  `maxTokens` are upstream's faux provider's rather than the stub model's.
- Assistant messages have no `responseId`. Usage holds the input and output
  tokens providers report to noel; cache tokens and costs are zero, and there is
  no `reasoning` count.
- Tool results are text, and for a `read` of an image the image: no
  `structuredContent` or `details`, in a `tool_execution_update`'s
  `partialResult` either.
- No `entry_appended` records: they report the journal entry with which
  upstream hides a dropped reply from the model, and noel never saves that
  reply (see Run Recovery).
- A provider error is in noel's words, in an error reply's `errorMessage` and
  in `auto_retry_start` and `auto_retry_end`: `request failed with status 503
  Service Unavailable: <body>` where upstream has `503: <body>`.
- A compaction's `result` is `{summary, details: {omittedMessages,
  retainedMessages}}`, not upstream's entry ids and token estimates. What a
  compaction summarizes is noel's choice (see Clipboard Drafts, Conversation
  Undo, and Session Usage), so after an overflow the summary request and the
  request that asks again are not upstream's.
- A final reply cut off at the output limit stays in the conversation when
  no compaction replaces it, so the next request contains it (see Run
  Recovery).
- `get_messages` returns what the session saves: no timestamps, and assistant
  messages with their content and `stopReason` (`toolUse` or `stop`) but no
  model, usage, raw stop reason, or timings.

Other differences:

- While a run is active, `set_model`, `set_thinking_level`, `set_auto_retry`,
  `compact`, `new_session`, `switch_session`, and `set_session_name` are
  refused. Upstream takes `set_auto_retry` at any time and saves it in
  `settings.json`; noel's lasts for the process.
- A tool call an abort stops keeps what it printed before its result's
  `Operation aborted`, and the calls after it that the abort kept from
  starting get `Operation aborted` alone. Upstream's bash ends its output with
  `Command aborted`.
- Closing stdin during a run stops it where it is, kills its tool processes,
  and exits without `agent_end` or `agent_settled`; what the run saved stays
  in the session, and the run stays interrupted for `resume_run`. A signal
  that ends noel does the same. Upstream disposes the runtime first.
- Runs outlive noel: the journal saves each run's start and end, a restarted noel
  reports a run that never ended as interrupted, and `resume_run` continues
  it under its `runId`. Upstream keeps runs in memory, and its agent cannot
  continue from a reply whose tool calls have no results.
- A resumed run reports the turn it finishes: the saved reply again, as
  `get_messages` shows it (no model, usage, or timings), then the replay-safe
  calls it runs again, then `turn_end`. Calls that are not replay-safe get no
  `tool_execution_*` records: their result, saved unreported, is the output
  they had saved and says they were interrupted (`docs/spec.md` §4.2).
- `abort` with no run active ends an interrupted run as `aborted`; upstream's
  does nothing. A new `prompt` ends an interrupted run the same way.
- The queue is saved with the session: an enqueue is saved before it is
  acknowledged, startup and `switch_session` load the saved queue, and
  `new_session` starts empty. Upstream's queue lives in memory.
- A queued prompt (`streamingBehavior` during a run) cannot have a
  `requestId`, which names runs.
- Steering queued together arrives as one user message, its parts joined by
  `---`, and so do follow-ups. Queued messages are text only: `steer`,
  `follow_up`, and a queued `prompt` with images fail.
- Tool calls arrive whole, so `toolcall_delta` carries all of the arguments
  at once. A tool call that fails before its tool runs (unknown tool, invalid
  arguments) still has a `durationMs`; a call of a reply cut off at a token
  limit has none, as upstream's.
- Stop reasons: a reply cut off at a token limit is `length`, another reply
  with tool calls `toolUse`, and anything else `stop`, with the provider's
  reason in `rawStopReason`. Upstream treats content-filter and refusal
  reasons as errors.
- During a run, `get_messages` and `messageCount` show an automatic
  compaction only once the engine call that ran it returns.
- `compact` runs to completion: commands sent meanwhile wait, and `abort` does
  not cancel it.
- The model object in `get_state` and `set_model` has the fields of upstream's
  that noel's model definition has: `id`, `name`, `api`, `provider`, `baseUrl`,
  `reasoning`, `thinkingLevelMap`, `input`, `cost`, `contextWindow`, and
  `maxTokens`. It has no `compat`, `headers`, `inputLimits`, `promptCache`, or
  `type`.
- `set_thinking_level` with a name that is not a thinking level fails;
  upstream sets the model's lowest level. A level the model lacks is clamped,
  as upstream clamps it.
- `new_session` refuses `parentSession`. With `--no-session` the session id
  stays `ephemeral`.
- A prompt that is a noel tool command (`/bash ls`, `/read path`) runs the tool
  without the model; the run reports its user message and tool result, but no
  turn.
- Upstream commands noel does not implement answer `Unsupported command:
  <type>`: `bash`, `abort_bash`, `set_auto_compaction`, `cycle_model`,
  `get_available_models`, `cycle_thinking_level`,
  `get_available_thinking_levels`, `get_session_stats`, `export_html`,
  `fork`, `clone`, `get_fork_messages`, `get_entries`, and `get_tree`. noel
  opens no extension UI dialogs: its one extension UI request is a `notify`
  for an account failover switch.
- `get_commands` lists prompt templates, then skills. noel has no extension
  commands to list before them, and its own `/extension:<name>` commands
  exist only in the TUI.
- `--mode rpc` takes no prompt arguments; upstream ignores them.

Reason: noel's run engine, session journal, and queue are its own (see Batched
Queue and Steering Dispatch). The protocol keeps upstream's records and
command contract, so a client written for upstream works; where noel lacks data
it leaves the field out rather than inventing it. Durable runs serve a
supervisor (Codewire) that must learn, after its own restart or noel's, what
became of the run it started, and have it finished when that is safe: a
prompt is applied once, a call that is not replay-safe runs at most once, a
replay-safe one at least once, and a model request cut off by a stop is sent
(and billed) again.

### Submissions and the Session Controller

One session controller (`crates/noel-core/src/controller.rs`, `docs/spec.md`
§3 and §9) takes a session's submissions, as Durable's harness does
(`durable/docs/spec.md:2329-2360`): a `requestId` on a prompt, steer or
follow-up, `whenBusy`, an id per queued submission, `withdraw`, and an abort
that keeps or withdraws the queue as the caller chooses. These parts differ:

- The kinds a `requestId` names are the client's verbs: prompt, steer and
  follow-up. Durable's are its two submission types, input and write; noel has
  no writes.
- A submission that starts a run is named by the run's id, and how it ended is
  the run's end. Durable keeps a submission record of its own, with an answer
  entry or a reason.
- While no run is active, a prompt starts a run even with submissions queued,
  and they join it: steering in its first round, follow-ups when it would
  answer. Durable queues the new input behind them and places all of them at
  one boundary.
- A queued submission without a `requestId` keeps its id only while the
  process that queued it runs. RPC mode reads no `requestId` but a prompt's
  and reports no submission ids, so its journal records are the ones it wrote
  before.

Reason: the controller extends noel's run engine and journal rather than
replacing them, so RPC mode keeps its records and journals (its goldens are
unchanged), and Mode 2 gets Durable's exactly-once submissions without a
second record of what the journal already says.

### Named noel, With Its State in `~/.noel`

The port is named noel. The binary is `noel`, its state directory is
`~/.noel` (the layout of upstream's `~/.pi/agent`), `NOEL_DIR` replaces
`PI_CODING_AGENT_DIR`, `NOEL_SESSION_DIR` replaces
`PI_CODING_AGENT_SESSION_DIR`, the other `PI_*` variables are `NOEL_*`, and the
project directory is `.noel/`. The first command that loads the configuration
without `NOEL_DIR` imports `~/.pi/agent` once when `~/.noel` does not exist
yet, and never modifies `~/.pi`; `--version`, `--help`, and a usage error
import nothing. Identities pinned by the parity fixtures stay pi: the Codex
`originator` and `User-Agent` headers and the `pi (<platform> <release>;
<arch>)` user-agent of Mistral requests. `scripts/rename-to-noel.sh` lists
every name that stays pi and why.

What the model is told changes with the name, and nothing else of upstream's
text does:

- The system prompt's first sentence names noel, and its bash rule names the
  `NOEL_*` variables (see Default System Prompt).
- `AI_AGENT` is `noel`.
- bash saves long output to `noel-bash-<random>.log` in the temp directory,
  where upstream writes `pi-bash-<random>.log`; the result names that file.

The fixtures under `tests/fixtures/ts-parity` are upstream's captured output
and keep upstream's names. A parity test that needs noel's reads the fixture
through one substitution, `noel_config::with_noel_names`, whose whole list is
`noel_config::UPSTREAM_NAMES`:

| In the fixture | Read as | Where |
|---|---|---|
| `<product>` | `noel` | the system prompt's first sentence (the fixture's own placeholder for upstream's "pi") |
| `PI_*` | `NOEL_*` | the system prompt's bash rule |
| `.pi` as a directory in a path | `.noel` | a project's directory in the trees and paths of `system-prompt.json` and `skills.json` |

`system_prompt_matches_ts_builder` reads `system-prompt.json` this way. The
tests that load a project tree (`discovery_matches_ts_resource_loader`,
`prompt_and_context_files_match_ts_discovery`, and
`get_commands_matches_upstream`) run every case twice: on the tree as
upstream wrote it, which noel reads as a project written for pi, and on the
same tree under noel's names. Both must give upstream's result, so the order
skills and prompt templates load in is upstream's in either directory.

Reason: noel is its own product, not a drop-in build of upstream pi. With
pi's names, both programs would install as `pi` and share one state directory,
where each rotates OAuth refresh tokens the other holds and noel writes
records (named accounts, todos, edited files) that upstream does not read.

### Reads pi's Project Files and Sets pi's Variables

noel uses a project's `.pi/` when the project has no `.noel/`: for its
settings, `SYSTEM.md`, `APPEND_SYSTEM.md`, extensions, skills, prompt
templates, and themes, and for what it writes there. A project with a
`.noel/` is read from that directory alone; the two are never combined.
`.agents/skills` is not pi's name and is read as before. noel also reads a
`.pi-extension.json` manifest when there is no `.noel-extension.json`, and
the `pi` key of a package's `package.json` when there is no `noel` key.

What noel sets for the programs it starts, it sets under both names:
extensions get `NOEL_EXTENSION_*` and `PI_EXTENSION_*`; the model's bash
commands get `NOEL_SESSION_ID`, `NOEL_SESSION_FILE`, `NOEL_PROVIDER`,
`NOEL_MODEL`, and `NOEL_REASONING_LEVEL` and the same five as `PI_*`; and the
process marker is both `NOEL_CODING_AGENT` and `PI_CODING_AGENT`. noel itself
reads only the `NOEL_*` names.

Reason: repositories written for pi, this one and upstream's included, track
`.pi/`, and their extensions, packages, skills, and scripts use pi's names.
noel loads upstream-format skills, so a skill that reads `PI_SESSION_FILE`
has to find it.

### Chord Delta, Canonical JSON and Contracts

`crates/noel-chord` matches upstream Chord's delta ops, wire codec,
`canonicalJson`, `hash64`, contract fingerprints, `coverage` and `fits` at the
pinned commit (`chord-delta.json`). These parts differ:

- noel's strings are Unicode text, so they cannot hold a lone surrogate. A `t`
  op that would cut a surrogate pair in half fails with `SplitSurrogate`
  instead of leaving half of the pair, and JSON carrying a lone surrogate
  escape does not parse. Upstream's own diff never cuts a pair.
- A numeric path segment of 2^64 or more is refused as an unsafe segment.
  Upstream accepts any non-negative integer and, on an object, writes the key
  JavaScript prints for it.
- Where upstream's `fits` throws on a malformed schema (a `required` that is
  neither a list nor a string, a `parameters` that is not a list, an invalid
  `patternProperties` pattern), noel answers "does not fit", so `coverage`
  reports a difference where upstream throws.
- `patternProperties` patterns are compiled by Rust's `regex` crate, not as
  JavaScript `u` regexes. A pattern using lookaround or backreferences does
  not fit, and `\d`, `\w`, `\b` and `.` can match differently on names with
  non-ASCII characters or `\r`.
- noel parses JSON numbers with `serde_json`'s default float parser, which can
  round a number with more than 15 significant digits, or a large exponent,
  one unit differently from `JSON.parse`. Such a number received as text can
  then differ in its last bit, and so in canonical JSON and `hash64`. Contract
  descriptions hold no such numbers. `serde_json`'s `float_roundtrip` feature
  would close this, for every crate in the workspace at once.

Reason: Rust strings and integers cannot represent the first two. Matching
the regex differences would need a JavaScript regex engine, and the float
parser is a choice for the whole workspace, not for this crate.

### Chord Link and Hosts

`crates/noel-chord`'s links, connections and hosts send the frames upstream
Chord sends in every scenario of `chord-link.json`, at the pinned commit, and
interoperate with upstream's client and server hosts (`make chord-interop`).
noel implements a subset, and these parts differ:

- **No reload, plugins or code versions.** A host runs code version 1. It
  answers `prepare` with a `vote` carrying `unavailable/not_coordinator` and
  ignores `commit`, `abort` and `retire`. A child host that announces another
  code version is closed instead of being fenced and reconciled.
- **Objects are simpler.** Factories return an implementation (a variant,
  state cells and methods) and the object is live at once: there are no
  resources, listeners, tasks, derivations, registries, `lifetime: "host"`
  objects or `linger`. A subscription that arrives with its hold therefore
  gets `snap`; upstream sends `reset` when the subscription arrives while
  the object is still activating, as it does over a real socket.
- **No `accepts` adapters.** A consumer of another contract version gets
  `contract_mismatch` (`upgrade_required` for a client).
- **`Host::get` reaches other hosts only**: a host does not hold its own
  objects through a reference.
- **Cell writes are always validated** against the cell's schema; upstream
  validates them in dev mode only. Values are checked by a subset of JSON
  Schema (the keywords `noel_chord::validate` lists), not TypeBox's
  `Value.Check`: `format` and `$ref` are not checked, and `Value.Clean` on a
  union keeps the first branch the cleaned value fits.
- **Packed payloads are inflated on receipt.** noel never packs what it
  sends. A relaying noel host forwards a packed payload inflated, where
  upstream forwards the deflated bytes unread.
- **Error messages.** A method that fails with a plain error upstream sends
  its message bare; a noel method fails with a `ChordError`, whose message
  carries its `code: ` prefix. Validation messages follow noel's checker.
  Codes and reasons are the same; the goldens compare no messages.
- **Tokens.** noel sends its consumer token in its declared member order, but
  the JSON Schemas inside it, and a token it relays, with sorted keys.
  Fingerprints are canonical JSON, so they are unchanged; only the first
  difference upstream's `coverage` names can change.
- **Frame text.** Frames are written by `serde_json`. A float payload with an
  exponent is written `1e21` where JavaScript writes `1e+21`, so such a
  frame's size, as the window counts it, can differ by a character.
- **Acknowledgements** go out after each batch of queued messages the host
  processed; upstream sends them from a 0 ms timer. Both acknowledge every
  frame once processed.
- **Transports.** NDJSON lines `{"chord2": frame}` and
  `{"chord2control": message}` carry the shape of Chord's Node IPC messages;
  upstream serializes them through Node's IPC channel. WebSocket framing
  over a Unix socket is noel's device transport; Chord has no Unix-socket
  transport.
- **Revocation order.** `invalidate_principal` disposes matching objects in
  address order; upstream disposes holders before what they reference. noel
  objects hold no references.
- **Closing.** `Host::close` waits up to two seconds for its links to write
  what they were sent, so a process that exits after it does not lose its
  `release` and `bye` frames.

Reason: noel needs Chord's protocol, not its runtime: one code version, no
plugins, and objects whose state it owns directly. The rest are a native
JSON library and scheduler standing in for JavaScript's.

### Mode 2 Server, Worker and Client

`noel server`, `noel worker` and `noel client` (`crates/noel-cli/src/server`,
`docs/spec.md` §5–§8) follow the roles of Chord's guide (`chord/docs/guide.md`
"Hosts and links") and take their submissions and view from Durable. Upstream
publishes no server or client to match (its application on Chord is
private), so the objects and processes are noel's, and these parts differ:

- **Objects.** `noel.sessions`, `noel.chat` and `noel.models` are noel's
  contracts (version 1, `crates/noel-cli/src/server/contracts/`). Upstream's
  guide shows a `Chat` with `viewer` and `participant` variants and a
  `Sessions` at `server` only as examples.
- **The view** has Durable's shape `{conversation, entries, docs}`
  (`durable/docs/spec.md` §9.3) with what noel's journal holds:
  - `entries` are upstream pi's `Message` JSON, as RPC mode's `get_messages`
    reports them (no entry ids, timestamps or usage), not Durable's entry
    records;
  - `docs["pi.agent"]` is `{model, thinkingLevel, cwd}`; Durable's stores
    the chosen extensions, tools and instructions;
  - `docs["pi.live"]` has `run` (`runId`, `requestId`, `inputs`, where
    Durable names a task), `generation` (`attempt`, `message`, `retry`; no
    deferred polls), `tools` (`callId`, `name`, `status`, `output`,
    `truncated`; no task ids, details, diagnostics or dropped counts) and
    `compaction` (`reason`), and no nested tools or compaction failure. A
    tool slot can also be `interrupted`: a resumed run's call that gets its
    interrupted result. The partial `message` is `{role, content}` only;
  - `docs["pi.inbox"]` items are `{id, mode, content, requestId?}` with the
    text as `content`; there are no writes;
  - `docs["pi.usage"]` is `{total: {input, output, requests}}`: noel counts
    a session's usage, not per model or tool. There is no `pi.provider`;
  - `lastRun` is added: RPC mode's `get_state.lastRun`.
- **Only committed progress, nearly.** A partial reply and a tool's output
  come from the journal's own records once saved, as Durable's view shows
  only committed state. Two things are shown as the run engine reports them,
  just before it saves them: a message it adds, and, as a resumed run
  starts, the interrupted results of calls that do not run again.
- **Ops.** The worker makes ops per change: a splice at the end of
  `entries`, appends to a streaming text, and diffs of the documents. Durable
  derives view ops from document ops, and bounds a watch to 100 pending
  frames; noel relies on Chord's four-window reset.
- **Submissions.** `submit`'s `whenBusy` names the submission's kind
  (`reject` a prompt, `steer`, `followUp`). A refusal is `invalid` with a
  reason (`busy`, Durable's `ConversationBusy`; `request_id_reused`;
  `queued_prompt`; `refused`), never `unavailable`, so a client retries only
  what may not have happened. A repeat answers `disposition: "repeated"` with
  the first submission's `status`. `abort` answers once the run settled, with
  the run and how many queued submissions it withdrew. `compact` takes no
  `requestId` and answers counts, since noel's compaction is not a run;
  `configure` answers the model and level in effect; `fork` takes no
  `atMessage`.
- **Sessions named by request.** `create` and `fork` name the new session
  `s-` and 24 hex digits of a SHA-256 of their `requestId`, so a repeat finds
  it after any restart without a table of requests.
- **Processes.** A worker is a separate `noel worker` that the server dials
  on `workers/<id>.sock` and speaks NDJSON to, where upstream's host manager
  forks a Node child on an IPC channel and kills its process group when the
  manager exits. A worker outlives its server, and a new server adopts the
  workers of its build. The worker's kernel lock is its journal's lock
  (spec §2), not a lock named for Chord. Control messages: `ready` carries
  `pid`, `build`, `session` and `generation`; `status` carries `viewers`,
  `running`, `queued` and `runId` (no `activeCalls` or `durableTasks`);
  `fatal` has reason `locked` for a held journal; `shutdown` is also sent by
  a worker as it exits.
- **Resume.** The server resumes a session's interrupted last run whenever it
  starts its worker (after a crash, or for the first hold after a machine
  stopped), at most three times per run. The count lives in the server, so a
  new server counts again.
- **Access.** Device only: the owner's processes, by the socket's peer uid,
  with no token. No relay, tunnel, pairing or guests yet (spec §6); a guest
  principal would get its variant, but nothing stamps one.
- **The runtime directory** can be named by `NOEL_RUNTIME_DIR`, for tests and
  scratch servers.
- **`noel.models`** is the models that can run when the server starts; it is
  not refreshed while it runs.

Reason: upstream's application on Chord is private, so noel's objects carry
what its journal and run engine hold, in Durable's shape where it can. The
process model keeps one writer per session (the journal lock) and lets a run
finish while no server is attached.

### The TUI Attached to a Server Session

`noel --attach <session-id>` (`crates/noel-cli/src/attach.rs`,
`docs/spec.md` §7.4) opens the TUI on a session that `noel server` holds.
Upstream has no such client (its experimental `pi client` was removed in
Chord 1.2.0), so the comparison is with noel's own TUI on a local session,
which it otherwise matches: the same terminal, editor, keys and transcript
rendering. It differs where the session lives in its worker and is shared
with other clients:

- **Abort withdraws the queue.** Esc (or the interrupt key) aborts with
  Mode 2's rule (spec §3.3): queued steers and follow-ups are withdrawn. The
  local TUI keeps them.
- **The queue is the session's.** During a run, Enter queues a follow-up and
  ctrl+s a steer in the session's queue (`pi.inbox`), which every attached
  client shows; ctrl+s does not turn queued follow-ups into steers. Up on an
  empty draft withdraws the last queued submission, whichever client queued
  it, into the draft.
- **Input is expanded by the worker,** with the session's configuration:
  skills, prompt templates and tool commands (`/bash`, `/read`, …) reach the
  worker as typed. Other clients' prompts show as the session saved them, as
  reopening a session shows them.
- **Commands for this process's own session are not available.** Session
  commands (`/new`, `/resume`, `/fork`, `/clone`, `/import`, `/export`,
  `/name`, `/delete`, `/undo`, `/tree`), settings, themes, accounts and
  logins, `/reload`, `/diff`, `/todos`, images, local shell commands (`!`),
  `/queue <prompt>`, and the model picker say they are not available when
  attached. `/model <provider/id>` and `/thinking <level>` are `configure`,
  `/compact` is `compact`, and `/interrupt` is `abort`.
- **Live output is what was saved:** a reply's partial and a tool's output
  appear as the worker commits them (spec §4.2, at most every 100 ms), not
  each streamed piece.
- **Status.** The spinner's time counts from when this TUI first saw the run.
  A reply aborted mid-stream shows `aborted` after it in every attached TUI,
  where the local TUI notes `interrupted`.
- **Recovery is the server's.** The TUI never continues an interrupted run
  (the local TUI does when it reopens one): a lost worker is a note, then the
  view is held again and redrawn from its snapshot, and a lost server is
  reconnected (and started).

Reason: the session's runtime is in its worker, and the TUI is one of the
clients that follow its view (spec §7.3); a command that acts on a session in
this process has no session here.

### Workspaces

`crates/noel-env`'s `ExecutionEnv` has upstream Durable's methods, results
and error codes, and `LocalEnv` makes the same observations as
`NodeExecutionEnv` in every case of upstream's env conformance suite that it
runs (`local-env.json`, at the pinned commit). These parts differ:

- **No watching yet.** `LocalEnv::watch` fails with `not_supported`, so the
  suite's nine watch cases are not run against it; the fixture records them
  by outcome only, since their change reports vary from run to run.
  `list_files` fails with `not_supported` too, as `NodeExecutionEnv` has no
  `listFiles`.
- **Id.** `LocalEnv`'s id is `local`; `NodeExecutionEnv`'s is `node:local`.
  Ids only say which environments share files.
- **Rust types.** Offsets, lengths, sizes and counts are unsigned integers and
  timeouts are durations, so the negative and fractional values upstream
  rejects at run time with `invalid` cannot be passed. The conformance port
  passes the nearest values that are not safe integers either (above 2^53 -
  1), which `LocalEnv` rejects with `invalid` as upstream rejects any value
  that is not a non-negative safe integer. A zero timeout is rejected with
  `timeout`, as upstream rejects a timeout that is not above zero.
- **Callbacks.** An output callback fails a command by returning `Err`
  (upstream: by throwing), which kills it and settles it with
  `callback_error`.
- **Additions to `exec`.** An `env` entry with no value removes an inherited
  variable (noel's bash calls drop stale `NOEL_*` and `PI_*` values); with
  `merge_stderr` both streams share one pipe, so output keeps its order and
  every chunk is stdout (the bash tool uses this, as before: see Shell Tool
  Differences); and each chunk also carries its raw bytes, so the bash tool
  saves the exact output. A chunk whose bytes end inside a character has
  empty text.
- **Shell lookup.** Without a configured shell, `/bin/bash`, then `bash`
  found on `PATH`, then `sh`; upstream runs `which bash` for the middle step.
- **One `cleanup`.** Upstream's `FileSystem` and `Shell` each declare
  `cleanup`, which TypeScript merges; Rust declares it once, on `Shell`.
- **Unix only.** Upstream's Windows rules (drive-relative paths, Git Bash,
  `taskkill`) are not ported.

The tools now reach files and commands through the workspace. Their limits,
results and error texts are unchanged, except in these corners:

- `ls` lists through the workspace, which describes files, directories and
  links only, so sockets, FIFOs and devices inside the directory are left out
  of the listing (one named as the path is still `Not a directory`). A
  directory that can be read but not searched (`r` without `x`) fails with
  `Cannot read directory` instead of listing nothing.
- `file://` paths resolve as Node's `fileURLToPath` resolves them: a query or
  fragment is dropped, and a URL with a host other than `localhost`, an
  encoded `/`, or an escape that is not UTF-8 is an ordinary relative path.
- `grep` and `find` search with ripgrep's crates on this machine's files, so
  in a workspace that is not local they fail and say so.
- `edit` and `write` of one file run one at a time within noel, keyed by the
  workspace id and the canonical path (upstream's file mutation queue). Where
  the canonical path cannot be found for another reason than a missing file,
  noel keys by the absolute path and lets the edit or write report the
  failure; upstream fails the call with the lookup's error.

Reason: the interface is upstream's so that the remote workspace (pi-env
protocol 1) can follow without changing the tools. Watching waits for the
features that need it. The additions keep the bash tool's output exactly as
it was, and the rest are Rust's type system and noel being Unix-only.

### noel-env Daemon

`noel-env serve` (`crates/noel-env-daemon`) is a port of upstream's pi-env
daemon (`packages/env/daemon`) at the pinned commit and speaks pi-env
protocol 1 unchanged, so upstream's client and noel's can use either daemon.
It answers upstream's client as upstream's daemon does in every recorded
scenario of `pi-env.json`, and passes the same 78 upstream client tests
(`make env-interop`). These parts differ:

- **Name and version.** The binary is `noel-env`: `noel-env --version` prints
  `noel-env <version>` and the usage text names `noel-env`. `hello.version`
  is noel's version (`0.3.0`), not the npm package version upstream's daemon
  ships with. The `PI-ENV <token>` sync line, the frames and every other
  `hello` field are upstream's.
- **Linux and macOS only.** Upstream's Windows paths (job objects, libuv's
  program search, the `=C:` drive working directories, legacy WSL `bash.exe`
  reading the command on stdin, `pty` failing with `ENOTSUP`) and its Android
  (Termux) paths (`$PREFIX/tmp` as the temporary directory, Android's
  unreliable file systems for watching, process names) are not ported.
  `hello.driveCwds` is always `{}` and `hello.separator` is `/`.
- **UTF-8 decoding.** Command output and line scans decode with noel-env's
  `StreamDecoder` and `LineScanner` (Rust's lossy rule, which replaces
  maximal subparts as WHATWG does) instead of `encoding_rs`, with the same
  results. A `scanLines` range whose line numbers are above 2^53 - 1 fails
  with `EINVAL`, as noel-env's `LineScanner` rejects them; upstream scans
  such a range, which a JavaScript client cannot send exactly.
- **Build profile.** Upstream's release profile (LTO, one codegen unit,
  stripped) is `profile.env-daemon` (`make env-daemon`), so the noel CLI's
  release build is unchanged.

The goldens compare what does not depend on the machine or on timing:
`hello`'s description of the machine, process ids, device and inode numbers,
modification times and the size of a directory are placeholders; `readdir`
entries are compared by name, not in the file system's order; a request's
`output` events are compared merged per stream, and `change` events by the
paths they report, since how they are split depends on timing.

Reason: noel ships its own daemon, versioned with noel; noel runs on Linux
and macOS; and reusing noel-env's decoder and line scanner keeps one
implementation of each, checked against the same goldens, and one dependency
fewer.

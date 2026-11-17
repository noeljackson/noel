# Upstream TypeScript Parity


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

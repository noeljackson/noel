# Plan: reconcile with upstream (November 2026)

Asked by the owner (2026-11-06): bring noel level with upstream pi `main`
(4b11a722 when this plan was written). noel has no legacy needs, so where
upstream changed behaviour noel follows it; differences that stay are recorded
in the non-parity register.

## Where noel stands

- The TypeScript parity fixtures (`tests/fixtures/ts-parity`) were last
  regenerated against upstream `main` on 2026-10-11. The nightly drift job has
  kept PR #86 open since 2026-10-14; nobody worked through it.
- Chord v1 (`chord-delta.json`) and the env fixtures (`local-env.json`,
  `pi-env.json`, `pi-env-ssh.json`) are pinned to `CHORD_REFERENCE_REF`
  b45e0ebc (2026-10-14). Upstream deleted Chord v1 (ed4f4884), so that pin can
  no longer move.
- Chord v2 is pinned to `CHORD2_REFERENCE_REF` 3551ec07 (2026-11-03); 35
  `packages/chord` commits came after it.
- Since 2026-11-03 upstream also changed Durable (6 commits), `env` (2),
  `schema` (5), `coding-agent` (61), `ai` (16), `tui` (3) and `apps/p2` (56).

## Slices

Each slice keeps `make check` and `make e2e` green and is committed on its own.

1. **Fixtures regenerate against upstream `main`.** The fixture image builds
   at upstream `main`, and every fixture script runs. A fixture that comes out
   empty is a broken script, not a behaviour: the script is fixed so it records
   what upstream does.
   Done when `make ts-parity-fixtures` at upstream `main` writes every fixture
   with content.
2. **noel matches the regenerated fixtures.** Each fixture that changed is
   ported: the Rust behaviour follows upstream, or the difference is recorded
   in the non-parity register with a reason.
   Done when `make check` passes with the fixtures from slice 1 committed, and
   PR #86 is closed as superseded.
3. **The env fixtures leave the Chord v1 pin.** `local-env.json`, `pi-env.json`
   and `pi-env-ssh.json` are generated from upstream `main`'s `packages/env`
   and Durable's env conformance cases. `chord-delta.json` is retired with
   Chord v1, its cases kept only where Chord v2's delta still has them.
   Done when `CHORD_REFERENCE_REF` is gone, or names a commit that builds, and
   the drift job reports no pinned package behind `main`.
4. **Chord v2 at upstream `main`.** The 35 `packages/chord` commits after
   3551ec07 are audited the way `docs/chord-v2-port.md` audited the port:
   each is ported, recorded as non-parity, or noted as not affecting noel
   (browser pages, the app kit). `CHORD2_REFERENCE_REF` moves to `main`, and
   `make chord2-fixtures` and `make chord2-interop` pass.
   Done when the audit table is in this doc and the pin is at the tip.
5. **Durable, env and schema.** Per-conversation compaction settings,
   `Tx.submission(id)`, the thinking budget and websocket timeout options, and
   the env fixes (Stop and timeouts end remote calls at once; a process
   program's leftovers are killed), each ported or recorded.
   Done when each has a line in this doc's audit with its outcome.
6. **coding-agent, ai and tui.** Each behaviour fix after 2026-11-03 that has
   a noel counterpart is ported, with a test where upstream has one; the rest
   are listed as not applicable. Among them: the read tool streams, grep
   context from ripgrep, find and ls limits, the bash exit code after a
   timeout, no split surrogate pairs, reload refused while running, the
   default models, apostrophes in template arguments.
   Done when each has a line in this doc's audit with its outcome.
7. **p2's client fixes that apply to noel's TUI.** The client TUI is noel's
   version of p2's, so p2 fixes to its client (a link that went silent
   without a close reconnects, 4b11a722) are checked against noel's client.
   Done when each has a line in this doc's audit with its outcome.

## Audit

Outcomes: **ported** (noel now does what upstream does, with a test),
**already** (noel already behaves so), **n/a** (no noel counterpart), or
**recorded** (a difference kept, in the non-parity register).

### Slices 1-3: fixtures at upstream `main`

Every fixture regenerates at upstream `main` (4b11a722) and has content.
Fixes to the scripts:

- pi-ai dropped its TypeBox re-export (6536f801): seven scripts import
  `Type` from TypeBox itself.
- Scripts that start pi with their own `HOME` ran `npx tsx` with an empty
  npm cache; the image installs a pinned tsx globally (`TSX_VERSION`).
- The system prompt now says "running inside pi" (705d047c).
- pi-env's watcher stops with `stop`, not `close`.
- `chord-delta.json` is made from Chord v2's delta module at `main`: the
  codec, decode errors, the applier, diffs and canonical JSON. Its v1
  sections (`hash64`, contracts, coverage, fits) are gone with Chord v1.
- `local-env.json`, `pi-env.json`, `pi-env-ssh.json` and `chord-delta.json`
  record `ref: main` and the commit; `CHORD_REFERENCE_REF` and the
  `/chord-reference` worktree are removed, and the drift job reports Chord v2
  commits after `CHORD2_REFERENCE_REF` instead.

What the regenerated fixtures changed, and noel's outcome:

| fixture | change | outcome |
|---|---|---|
| provider requests | `pi/1.5.0`, Node v24.22.2 in the user agent | ported: the reference versions |
| `openai-responses-tools.json` | tools top-level again (catalog compat) | ported: from the catalog |
| `model-catalog.json`, `model-resolution.json`, `model-thinking.json` | new models, prices, defaults (Claude Opus 5.5 for Anthropic) | ported: the compiled catalog; tests follow the new defaults |
| `system-prompt.json`, `skills.json` | the tightened prompt, skill and edit guidelines (705d047c) | ported |
| `skills.json` | template argument quotes (96667f34) | ported (slice 6) |
| `slash-commands.json` | `/import` and `/resume` descriptions | ported: read from the fixture |
| `tool-behavior.json` | grep context and limit notice, find limit notice | ported (slice 6) |
| `error-classification.json` | "context window exceeded" is an overflow | ported |
| `rpc-protocol.json` | a dropped connection's error keeps its cause | ported (4c992154) |
| `local-env.json` | a case renamed (`stop`) | ported |
| `pi-env.json` | exec's timeout cancels the connection (c96e368e); hello lists `process` | ported; `process` recorded (noel-env Daemon) |
| `chord-delta.json` | from Chord v2 at `main` | ported: unchanged codec |

Slices 1, 2, 3 and 5-7 land in one commit: the env port (c96e368e) and the
regenerated env goldens need each other, as do the prompt and its fixtures.

### Slice 4: Chord v2 at upstream `main`

`CHORD2_REFERENCE_REF` is 4b11a722. `make chord2-fixtures` adds upstream's
bytes and verdicts for traces, `old` and the new reasons, and the existing
cases are unchanged. `make chord2-interop` passes both ways.

| upstream | change | outcome |
|---|---|---|
| 378d45a0, 2205a824 | trace carrier on `hold` and `call` (kind flag 0x40), at most 8 printable-ASCII strings; type and host kind names at most 256 | ported: `binary`, `Message`, `check`; relays forward the carrier once; fixture cases; no spans (recorded) |
| a5d209af, 9ff0ebfd | `call.old` (kind flag 0x20, calls only), honoured from trusted links; `unavailable` reasons `version` and `reload_cycle` | ported: decoded, refused on other kinds, relayed only from a trusted link; reasons accepted; noel never reloads (recorded) |
| d7a988d1 | a call's answer follows its state, past a full window | ported: `answer_follows_state` for own and relayed answers; test `a_calls_answer_follows_its_state_past_a_full_window` |
| 6515a797, d6bf3549, edc9c698 | calls in flight survive a `loading` unbind; reload suite | already: the consumer rejects only unsent calls on `loading`, and the relay keeps relayed calls; noel's provider never unbinds a bound hold |
| 9f7d909f | a late heartbeat tick after a sleep is not silence; a worker's lost link is a stop | recorded: noel's heartbeat runs on a monotonic clock that stops during sleep; workers are noel's own |
| 8f9446a0 | `invalid` and `forbidden` helpers, page state | already: `forbidden` without a reason is accepted; the rest is the app kit |
| 7aed50ae, 5ff8c027, 7ac1a9e3 | Node worker locks and base code | n/a: noel's workers are its own |
| 2439933c, 8889e623, e5f3b415 | live queries, group commit, replication | n/a: store internals |
| 3d11c351, 0efe50cb, f4427f8a, 15a51145, ff33ef42 | i18n, tutorial, telemetry ingest | n/a: app kit and telemetry |
| 0c2b568f | swaps never answer `not_ready` | n/a: in-process reload |
| 20d86c36, bb5eafa8, f617ed31 | p2 cleanup, per-host stores, Files v2 | n/a: p2's app (`docs/p2-audit.md`) |
| docs: d6fb784c, 828bcadc, 596523e0, 72dd26e0, f6660876, 80f7a114, 395f3e03, fcebc099, 55d3fda7, 67afac18 | design decisions | n/a: no wire decision beyond the rows above |

### Slice 5: Durable, env and schema

| upstream | change | outcome |
|---|---|---|
| 922c3ff2 | per-conversation compaction settings: `configure({compaction})`, `modelOverrides`, resolved per conversation and model | ported: `CompactionSettings.modelOverrides`, `resolve_compaction` (Durable's example as a test), the session's own settings journaled and inherited by forks, `noel.chat` `configure({compaction})`, `pi.agent.compaction`; every compaction reader uses `Runtime::compaction_policy` |
| 9ca15920 | `Tx.submission(id)` for tools | n/a: noel's tools have no Durable `Tx` |
| dbc61d64 | thinking budgets and the websocket connect timeout as stream options | already for budgets (`thinkingBudgets` settings reach every provider); n/a for websockets (no websocket transport) |
| fabf916f, 469d8651 | SqliteStorage group commit; test timeouts | n/a: noel's storage is its journal |
| c96e368e | env: an abort while connecting ends at once; an unanswered cancel settles in 1 s; exec's timeout covers the connection; ssh `ConnectTimeout=15` | ported: `noel-env` connection and remote exec, tests with a daemon that never answers |
| 032656b6 | env daemon: a `process` program's leftovers are killed | n/a: noel's daemon has no `process` operation (Durable audit: needed only for MCP over pi-env) |
| 55a5c691, af79b0c3, 20c637b1, 2bd2c2d0 | schema: numbers without NaN, TypeBox parity, inline checks, relative `$id` | n/a: noel's schemas are JSON (no NaN), and its contract checks follow Chord's subset |

### Slice 6: coding-agent, ai and tui

| upstream | change | outcome |
|---|---|---|
| 96667f34 | quotes open only at an argument's start; escaped and unclosed quotes | ported: `parse_command_args`, upstream's cases as a test |
| 9e870f6f | truncation never splits a surrogate pair | ported: codemode's output head and tail and its argument and error previews (`slice_head_utf16`, `slice_tail_utf16`); grep lines already; compaction's serializer has no noel counterpart (noel's checkpoints are its own) |
| 676af1d7 | find: limit at least 1; the notice only past the limit | ported: one result past the limit decides the notice (local and workspace); the clamp and `ls` already |
| 8da141c2 | grep: ripgrep's merged context, trailing context after the last match, the notice needs one more match, a lone CR kept | ported: `GrepSearch`, with a test of each rule |
| 06aa6563 | bash: a timeout after the shell exited reports its exit code | already: once the shell exits noel waits a short grace for the pipes and reports the exit code, so a background writer cannot turn it into a timeout (Durable env semantics) |
| c3b938e3 | reload refused while running or compacting | already: the worker refuses commands during a run, and the TUI says so |
| 5732bac0 | Ctrl+P cycles to the only model in scope | n/a: noel has no model cycling (RPC `cycle_model` is unsupported) |
| 081a6e87 | the `auth.json` lock is shared across symlinked paths | ported: the lock follows the real path; test with a symlinked file |
| 3a7bb6f4 | the read tool streams instead of loading whole files | recorded: output unchanged; noel reads the file into a byte buffer (no string-length limit in Rust), streaming not ported |
| 66c65567 | a user `!` command's discarded output is flagged truncated | already: `!` uses the bash tool's own output and its truncation |
| a63c4ec5, 4cf69332 | image-only queue entries clear; queued images are resized | already: the controller places items whatever their text, and the worker turns every submitted image through the same resize |
| 7a53c310 | tool results settle before disposal | already: an interrupted run resumes from its journal (Durable) |
| ace560c3, 1708b478 | RPC shutdown on stdin end and signals | already: the journal is written as it goes, so exit loses nothing |
| b2cb6292 | a started OAuth refresh is saved before exit | n/a: noel refreshes OAuth only inside a request, never in the background |
| 6d3e342b | codemode follows local `$ref`s in tool schemas | ported: the output summary and search text resolve local refs (each once); tests `the_output_summary_follows_local_refs`, `search_text_follows_local_refs_once` |
| 54280c4f | markdown hides HTML comments | ported: the TUI's markdown restyle drops `<!-- -->`, also across lines; test `html_comments_are_not_shown` |
| 2af8a42f | OSC 8 hyperlinks | n/a: noel's TUI does not emit OSC 8 |
| 25490ea4 | the hardware cursor follows the editor | n/a: noel draws its own cursor |
| c13f6acf, 51313540, dca1f555, df0d88aa, ea0f49a3, 8a3e163b, 2cfab7c2, 3b57ed7e | pi interactive-mode components (selectors, footer, dialogs) | n/a: noel's TUI is its own; no counterpart components |
| 9a195396 | the changelog shown since the last-seen version | n/a: noel shows no changelog |
| e6865ec7 | extension loading through jiti | n/a: noel's extensions run in Bun |
| 5bdf988b | tool call ids over 40 characters replayed to OpenRouter are shortened with a hash | ported: `normalize_openai_chat_tool_call_id` (OpenAI and OpenRouter, by provider or base URL); test `long_tool_call_ids_are_shortened_for_openai_and_openrouter` |
| 4c992154 | a connection error keeps its underlying cause | ported: `ProviderError::Request` appends the deepest cause (`with_cause_detail`); test against a closed port |
| c5d39783, 0f4d5e96 | `session-id` instead of `session_id` on OpenAI Responses; prompt cache keys for compatible proxies | recorded: noel passes no session id to providers, so it sends no session-affinity header and no `prompt_cache_key` (non-parity register) |
| 14b804d8 | `requiresNonNullAssistantContent` compat flag | n/a: noel has no per-model compat flags |
| 4b977773 | Claude Sonnet 5 sends no temperature | n/a: noel sends no temperature |
| f3a64af0 | an abort during lazy provider setup ends as aborted | already: an abort ends the run as interrupted at any step (run-end outcomes) |
| d84123b3, b425754a, 36142c1c | default models; Codex provider name | through the catalog and model fixtures (slice 2) |
| a013448f | cache warming | n/a: noel does not warm caches |

### Slice 7: p2's client fixes

| upstream | change | outcome |
|---|---|---|
| 4b11a722 | a link gone silent without a close reconnects | already: Chord's heartbeat closes it and the client reconnects; test `a_link_gone_silent_without_a_close_is_reconnected` |
| a7a75a9d | `hideThinkingBlock` applies to the TUI's thinking view | already: the client renders with the shared renderer, which honours it |
| f0811457 | exit on a repeated signal, SIGHUP or a stalled close | already: SIGTERM and SIGHUP exit at once (`signals.rs`), and the raw-mode TUI reads ctrl+c as a key |
| others | app features of p2 (sandboxes, files, MCP, subagents, tree, artifacts) | n/a here: candidates in `docs/p2-audit.md`, not gaps |

## Status

Started 2026-11-06.

- Every slice is done; `make check` and `make e2e` pass.
- PR #86 (the nightly drift) is superseded by these fixtures.

# Plan: the durable task model and a Rust SDK (November 2026)

Asked by the owner (2026-11-14): a Rust SDK for building harnesses that are
not the TUI, modelled on upstream's `packages/durable` (pi-durable). Tracked
in #210.

## Why Durable is the model

Durable is a headless engine. It owns storage, conversations, runs, tools
and recovery, and draws nothing. Every front end is a host on top of it:
upstream's examples include print mode (`17-print`), JSON mode (`18-json`),
a coding agent (`25-coding-agent`), a Cloudflare Durable Object (README,
"Sleeping and Waking Up") and Chord objects (`35`-`37`). A program opens a
Harness on storage, submits input to conversations, waits for or watches
the result, and gets the same work back after a crash by opening the same
storage again.

noel already works this way inside: `SessionController` is the one driver
(`docs/spec.md:38`), and RPC mode and the server's workers are two hosts of
it (`crates/noel-core/src/controller.rs:47`). The TUI is a client of the
server. A program cannot be a third host today, and the host has no task
model to program against.

## Where noel stands

Already Durable's (phases 5 and 6 of `docs/roadmap.md`):

- The journal: a record is stored before it is shown; recovery of a torn
  tail, partial output and compaction checkpoints; `JournalStorage` with
  `JsonlFile` and `MemoryJournal` (`crates/noel-core/src/storage.rs`).
- Submissions: `requestId` dedup, steer and follow-up, `continue`,
  `whenIdle: "reject"`, abort, withdraw, resume.
- Run-end rules: `runFailure` reasons, an interrupted request charged to
  `retry`, `restartLimit` counted in the journal, `turn_limit`.
- History ids that survive compaction, the windowed view, `pi.live` v3.
- Replay-safe tools, subagents, the server resuming runs, `noel-client`.

Missing, the part `docs/durable-1.5-audit.md:296-312` set aside as "wait":

| Durable | noel |
|---|---|
| `defineTask`: phases, a checkpoint per phase, recovery | turns and tools only |
| `sleepUntil`, `wake` and `onWake`, an injectable clock (`now`, `setTimer`) | none |
| `defineSignal`, `harness.signal()`, `readSignal` | none |
| Tools that park (`{ sleep }`, `{ wait }`), `api.memo`, `cancel`, `beforeTool` gates that park | none |
| Child tasks, `failFast` / `allSettled` / `any`, bottom-up abort | subagents, not owned tasks |
| `defineDoc`: your own documents in the same commit as entries | none |
| Task graph, `afterRun` tasks, observers and `observeCommits`, `readOnly` | none |
| One Harness holding many conversations, embedded in a program | one session per controller |

And for embedding:

- Provider resolution is `pub(crate)` in the CLI
  (`crates/noel-cli/src/provider.rs:101`); the ssh workspace opener is in
  `crates/noel-cli/src/workspace.rs`.
- `create_provider` refuses a model without a credential unless it is faux
  (`crates/noel-providers/src/lib.rs:243-249`); a local vLLM often has no key.
- `Host::event` gives a borrowed `Event<'_>`; a program wants owned,
  serialisable events it can send across tasks.
- Tools from a program go through a plugin process (`noel-plugin`); there is
  no in-process Rust tool at the `Runtime` level.

## Decisions

- **The task model is ported.** This reverses the audit's "wait" for it.
- **Durable's names and semantics.** `Harness`, `Conversation`,
  `Submission`, tasks, signals, parks and documents keep Durable's meaning,
  so its spec and examples stay the reference. Differences go in the
  non-parity register.
- **Storage stays a journal.** Tasks, signals and documents become journal
  records over `JournalStorage`, not Durable's table store (audit item 10).
  What Durable's `storage-conformance.ts` checks that applies (atomic
  batches with receipts, refused versus unknown, ids that only grow) is
  ported as a conformance suite for `JournalStorage`.
- **One crate for programs.** `crates/noel-durable` is the public API; it
  wraps `noel-core`, `noel-env`, `noel-providers` and `noel-tools`. The CLI,
  RPC mode and the server become hosts of the same API where that removes
  duplication, not as a goal of their own.
- **Not ported:** `StoreStorage`, IndexedDB, Durable Objects and telemetry
  spans. Recorded in the register.

## Goldens

The same mechanism as the other parity fixtures: a script in
`scripts/ts-parity/` runs upstream at `TS_REFERENCE_REF` in the fixture
image and writes JSON to `tests/fixtures/ts-parity/`; a Rust test replays it.

- Each fixture runs upstream's own example or test scenario on the faux
  provider and an injected clock, crashes and restarts included, and records
  what a host can observe: submission outcomes, entries, task states and
  outcomes, signal results, wakes and `pi.live`.
- Goldens record behaviour, not ids or timing: ids are normalised to their
  order of first appearance, times to the fake clock.
- `scripts/ts-parity-drift.sh` covers the new fixtures like the others.

## Slices

Each slice keeps `make check` green (and `make e2e` when it touches the TUI,
the client or the server), is committed on its own, and is done when its
goldens pass.

1. **Embedding host.** `Harness::open(storage, options)`,
   `Conversation::submit` returning a `Submission` with `wait()`, an owned
   event stream, an in-process `Tool` trait, `models` built from a caller's
   `ModelSpec`s (a missing key allowed for OpenAI-compatible endpoints), an
   `env(target)` hook, provider and workspace resolution moved out of
   `noel-cli`. Goldens: `13-chat`, `17-print`, `19-inbox`,
   `33-turn-limit-and-continue`.
   Done when a test outside `noel-cli` runs a session to its answer, survives
   a restart with the same `requestId`, and `rpc_parity.rs` still passes.
2. **A Harness of many conversations.** Create, fork and find conversations
   in one Harness; one scheduler; the journal layout across conversations
   decided and written in `docs/spec.md`; a SQLite `JournalStorage`; the
   conformance suite. Goldens: `05-harness`, `07-harness-conversations`,
   `02-forks`, `harness-conversations`.
3. **Tasks.** `define_task` with phases and checkpoints, owners, recovery
   after a crash, the per-task restart limit. Goldens: `11-tasks`,
   `12-recovery`, `harness-tasks`, `harness-tasks-recovery`,
   `harness-restart-failure`.
4. **Sleep and wake.** Stored sleeps, an injectable clock, `wake()` and
   `on_wake` for hosts that are evicted. Goldens: `harness-sleep`.
5. **Signals.** `define_signal`, `signal()` results, an early signal waking a
   sleep. Goldens: `38-signals`, `39-ask-a-person`, `harness-signals`.
6. **Tools that park.** `{ sleep }` and `{ wait }` parks, `memo`, `cancel`,
   `executeTool` without waiting, parks in `beforeTool` gates. Goldens:
   `40-tool-waits-for-webhook`, `41-tool-polls`, `44-approval-gate`,
   `harness-tool-parks`, `harness-tool-gates`.
7. **Child tasks.** Ownership, `failFast` / `allSettled` / `any`,
   `completing`, bottom-up abort; subagents as owned tasks with `notify`.
   Goldens: `23-child-tasks`, `42-parallel-children`, `43-subagent-waits`,
   `harness-park-children`, `harness-ownership`.
8. **Your own documents.** `define_doc`, commits with entries, deferred
   persistence, `conversation_created`. Goldens: `01-documents`,
   `10-extension-state`, `26-plan-mode`, `session-documents`.
9. **Task graph and observers.** `watch_task_graph`, `inspect`,
   `observe_commits`, `read_only`, `afterRun` / `onYield` / `afterTools`
   tasks. Goldens: `harness-task-graph`, `harness-observers`,
   `harness-read-only`, `32-tests-after-edits`, `45-review-on-yield`,
   `46-after-tools-terminate`.

## First consumer

A control plane that takes a security request, runs noel sessions in
Codewire VMs against models served by vLLM on RunPod, and must not leak a
GPU pod when it crashes. Starting a pod is a tool that memoizes its
idempotency key, parks until the server is ready and stops the pod in
`cancel` (slice 6); each VM's session is an owned child (slice 7).

# Durable 1.5 audit (1692bba..7dc38ff)

Read-only audit of upstream `packages/durable` (and `packages/env`, `packages/codemode`) from noel's pin `1692bba` (2026-10-14) to tip `7dc38ff` (2026-11-03). 77 Durable commits; releases 1.3.0 (2026-10-15), 1.4.0 (2026-10-21), 1.5.0 (2026-11-02). Upstream cites are `durable/docs/spec.md:<line>` at tip unless marked `@pin`; changelog cites are `durable/CHANGELOG.md:<line>` at tip. Roadmap phases refer to `docs/roadmap.md`.

## Summary

- **The view changed shape.** `ConversationView.entries` is now a window (newest 50, at most 100) over the fork-aware raw history, with head markers in place, plus `before` (spec:5595-5680). `ConversationTranscript` is the client-side layer that pages history and tracks gaps (spec:5705-5780). `watchEvents()` and old spec §9.4 are removed: renderers render state only (CHANGELOG:16-50). noel's view still sends the active (compacted) message list in full, without entry ids.
- **`pi.live` is version 3.** New: `run.turns`, `run.first`, `run.afterRun`, `runFailure`, `tools[].unoffered`, and `reason` renamed `trigger` in `compactions`/`compactionFailure` (spec:4539-4680). `pi.live` is no longer `{}` after a failed run. `nestedTools`, `compactions`, `compactionFailure` and `generation.deferred` already existed at the pin. noel never had them.
- **Runs end visibly.** Restart-limited tasks no longer park: they end with `restart_limit` (spec:2533-2550). A model request that was in flight when the host died is no longer resent: it counts as a failed attempt under `settings.retry`, or ends the run with `runFailure.reason = "interrupted"` (spec:4884-4901). noel still resends it (`docs/spec.md` §4.1).
- **New submission types.** `{type: "continue"}` (spec:2905-2906, 2958-2977), `whenIdle: "reject"` (spec:2907, 2924-2932), `maxTurns` with `turn_limit` (spec:2918), `afterFailedRun` (1.4, spec:2978-2985), and `Conversation.submit(tx, draft)` (spec:2939-2956).
- **Partial output is deferred.** Throttled partials still commit every 100 ms and are shown at once. Storage gets them with the next commit that reads `pi.live`, or after `persistWithinMs` (default 1000) (spec:71-75, 1977-2060, 4502-4512). noel saves every 100 ms and shows only what it saved.
- **Storage contract.** `commit(batches, receive, context)` returns `{stored, refused?}`, adds an optional `publishing()`, and narrows `mintId` to ids that only grow (spec:5991-6007). Durable also adds group commit, a `StoreStorage`, and IndexedDB (1.4).
- **Tools.** `afterTool` runs on every result, including results the harness writes itself. Unoffered calls get a `pi.tool` task (`unoffered: true`). `tool_search` ranking was rewritten. Tool schemas use pi-schema instead of TypeBox (CHANGELOG:9, 63-64, 119-120).
- **Codemode existed at the pin.** `packages/codemode` was already published at the pin (version 1.2.0, no `private` flag), and so was `durable/src/codemode`. Since then: a few fixes (Harness clock, `read` `structuredOutputSchema`, search ranking), plus deterministic `start()` and remote/inline sandboxes in codemode 1.2/1.3. Nothing there was rewritten.
- **Env.** `WatchChange` is renamed `FileChange` and tagged by `kind` (`error` becomes `failed`), and `FileWatcher.close` becomes `stop`. The pi-env daemon now kills process groups on SIGTERM/SIGINT/SIGHUP, adds a `process` operation (feature flag), and reports an overflow when a change list exceeds the frame limit. noel's daemon has none of these.

## Changes by area

Format: **upstream now** / **noel today** / **gap**.

### Runs, submissions, abort, withdraw, recovery

1. **Restart limit (1.4 park, then 1.5 end).**
   - Upstream: each task counts its crash restarts. At `settings.restartLimit` (default 5) a tool ends with a `restart_limit` error result, a compaction fails into `compactionFailure`, and a generation ends the run with `runFailure.reason = "restart_limit"`. Progress resets the count. `retryTask()` and `task_parked` are removed (spec:2525-2560; CHANGELOG:51).
   - noel: the server resumes an interrupted run at most 3 times, counted per run in the server; a new server counts again (`crates/noel-cli/src/server/serve.rs:73,687`; `docs/spec.md` §8.3). After that the run stays interrupted until a client resumes or aborts it.
   - Gap: different unit (per run, not per task), different default, and noel's count is not durable. Upstream ends the run with a reason; noel leaves it interrupted. The view has no reason to show.

2. **Interrupted model request is not resent.**
   - Upstream: a request that was in flight when the host died is a failed attempt. The commit either sleeps until `retry.at` with the text "The host stopped while the model was answering, on attempt N", or settles the inputs `unanswered` with `runFailure.reason = "interrupted"`. A close still resends (spec:4884-4901; CHANGELOG:52, 116).
   - noel: a request a crash cut off is sent again after its partial is saved as aborted (`docs/spec.md` §4.1).
   - Gap: behaviour change. noel resends without charging retry. This is the loop upstream fixed (#11139), although noel's 3-resume cap bounds it.

3. **`runFailure` (1.5).**
   - Upstream: `pi.live.runFailure` has `taskId`, `inputs`, `reason`, `detail` and `first`. Reasons: `model_error`, `no_model`, `aborted`, `reset`, `faulted`, `orphaned`, `interrupted`, `restart_limit`, `nothing_to_answer`, `turn_limit`. It is set by the commit that ends the run and removed by the next run start (spec:4640-4665, 4731-4760).
   - noel: `Outcome::{Completed, Aborted, Failed(String)}` (`crates/noel-core/src/controller.rs:200-213`), and `lastRun` outside `docs` in the view (`docs/spec.md` §7.3; `view.rs:597`).
   - Gap: no reason vocabulary, and the failure is not in `pi.live`. `lastRun` is noel-only.

4. **`continue` submissions.**
   - Upstream: `submit({type: "continue"})` runs on the context as it is. Only an idle conversation accepts it; a busy one gets `ConversationBusy`. Queued items are placed first. If nothing is left to answer, the run ends `nothing_to_answer`. `SubmissionRecord` gains `type: "continue"` (spec:2905-2906, 2958-2977; CHANGELOG:65, 81).
   - noel: `resume({expectedRunId})` continues only an interrupted run (`controller.rs:546-564`; `docs/spec.md` §7.2).
   - Gap: no continue after a failed or limited run, and no `nothing_to_answer`.

5. **`whenIdle: "reject"`.**
   - Upstream: an idle conversation refuses the input with `ConversationIdle` and writes nothing. Combined with `whenBusy: "steer"`, the input steers the current run or is refused, in one step (spec:2924-2932; CHANGELOG:83).
   - noel: the attached TUI, when a prompt is refused `busy`, sends it again as a follow-up (`docs/spec.md` §7.4). That is the race upstream's flag closes.
   - Gap: small; `noel.chat.submit` has no `whenIdle`.

6. **`maxTurns`, `turn_limit`, `run.turns`.**
   - Upstream: `AgentState.maxTurns` bounds tool rounds per run. Reaching it ends the run `unanswered` with `turn_limit`, and a continue can resume it (CHANGELOG:82; spec:2918).
   - noel: `agent.maxToolRounds` (default 200) ends the turn with its own error (`crates/noel-core/src/lib.rs:732`; register "Configurable Agent Round Cap").
   - Gap: same idea under a different name and failure shape. There is no `run.turns` in the view.

7. **`afterFailedRun` (1.4).**
   - Upstream: `keep` (default), `continue` or `withdraw` for the queue behind a failed run (spec:2978-2985).
   - noel: the queue stays after a failure (`controller.rs:678-690`), which is effectively `keep`. Placement differs, as the register already says ("Submissions and the Session Controller").
   - Gap: only `keep` exists. No new divergence.

8. **Abort and withdraw.**
   - Upstream: unchanged. Withdraw answers `aborted`, `already_placed` or `settled`, and conversation abort withdraws queued steers and follow-ups (spec:2986-2989). New: an aborted run ends with `runFailure.reason = "aborted"`.
   - noel: matches (`docs/spec.md` §3.2-§3.3).
   - Gap: only `runFailure`.

9. **Handoff fix (1.5).**
   - Upstream: `control.handoff` continues the same run in the new context (CHANGELOG:107).
   - noel: no handoff control. Not applicable.

10. **Run-end tasks, `afterRun` hooks, `beforeProviderRequest`, `readOnly` Harness** (CHANGELOG:80, 86, 89).
    - noel: no equivalents. These belong with the task model (see Tasks below).

### Partial output persistence

11. **Deferred commits.**
    - Upstream: partials and running tool output are deferred commits. They are shown to watches at once and stored with the next commit that reads `pi.live`, at `persistWithinMs` (default 1000, `0` restores the old behaviour), at close, or when the document is dropped. A crash can lose about 1 s, and a stream that crashes before its first store leaves no aborted entry (spec:71-75, 1977-2060, 4502-4512, 4760-4770; CHANGELOG:17, 74).
    - noel: every 100 ms the partial is appended to the journal, and the view shows only what was saved (`crates/noel-core/src/progress.rs:3-38`; `docs/spec.md` §4.2, §7.3).
    - Gap: noel is stricter (it loses at most 100 ms). The register's "Durable Partial Output" should note that upstream now stores later than it shows. Porting this is optional: noel's appends are already cheap.

12. **A slot's `running` status, final nested arguments and a generation's attempt** are now deferred too (CHANGELOG:97).
    - noel: N/A (no task checkpoints).

13. **Partial append ops.**
    - Upstream: partial commits use Chord `appendText` instead of copying the text (CHANGELOG:108-111).
    - noel: already appends (`docs/spec.md` §4.2).
    - Gap: none. Register line 967 ("Upstream overwrites a live document") is still roughly true in storage terms but is worth rewording.

### Conversation view

14. **Windowed `entries` and `before`.**
    - Upstream: spec:5595-5680; CHANGELOG:23-24. `ConversationWindow {from, min=50, max=2*min}`. New entries are appends. When the window passes `max`, its oldest entries leave in one splice and `before` moves.
    - noel: the active message list in full, as `Message` JSON without ids (`crates/noel-cli/src/server/view.rs:487-503`). After a compaction the entries are replaced by the checkpoint's messages (`crates/noel-core/src/lib.rs:2036-2040`).
    - Gap: L. This needs stable entry ids on journal records and a raw history that survives compaction, with head markers in place. noel's in-memory state replaces `messages` at a checkpoint, although the older `Message` records stay in the journal file.

15. **`ConversationTranscript` and `Conversation.entries()` paging.**
    - Upstream: spec:5705-5780; CHANGELOG:76.
    - noel: none. The TUI rebuilds the transcript from `entries` (`docs/spec.md` §7.4).
    - Gap: M. This lives client side (`noel-client`). It needs an entries paging method on `noel.chat`.

16. **`watchEvents` removed; render from state.**
    - Upstream: CHANGELOG:16-50 (event-to-state table).
    - noel: never had events. The attached TUI already renders from the view.
    - Gap: none. noel is already aligned.

17. **`pi.live` fields.**
    - Upstream: v3 (spec:4539-4680):
      - `run{taskId, inputs, turns, first?, afterRun?}`
      - `generation{attempt, message(+usage), retry, deferred}`
      - `nestedTools[]`
      - `tools[]{callId, name, taskId?, unoffered?, status, output, droppedBytes, droppedLines, details, diagnostics, entry}`
      - `compactions[]{taskId, trigger, blocking, attempt, retry}`
      - `compactionFailure{taskId, trigger, error}`
      - `runFailure`
    - noel (`view.rs:505-554`; register 1340-1346):
      - `run{runId, requestId?, inputs}`
      - `generation{attempt, message{role, content}, retry}`
      - `tools[]{callId, name, status (+interrupted), output, truncated}`
      - `compaction{reason}`
    - Gap: M. Missing: `turns`, `runFailure`, `nestedTools`, `compactions` as an array with `trigger`, `compactionFailure`, the tool slot's `entry`, `details`, `diagnostics` and dropped counts, and usage on the partial. The noel-only `interrupted` status and `truncated` field need a decision.

18. **Watch delivery.**
    - Upstream: pending commits are folded into one callback, and past 10,000 pending ops the callback gets a root replacement (CHANGELOG:15). Watch listeners take `{kind: "value", value, ops, retired}` (CHANGELOG:53).
    - noel: register line 1354 still says Durable "bounds a watch to 100 pending frames".
    - Gap: the register text is stale. This is a doc fix.

### Tools

19. **`afterTool` on every result.**
    - Upstream: also runs on the harness's own error results (`tool_unavailable`, `invalid_arguments`, `blocked`, `interrupted`, `restart_limit`, `aborted`, `abandoned`) (spec:5094-5100; CHANGELOG:63).
    - noel: follows the coding-agent extension API (`tool_result`), not Durable hooks.
    - Gap: not verified whether coding-agent's `tool_result` changed the same way. Check before phase 7.

20. **Unoffered tool calls.**
    - Upstream: the call gets a `pi.tool` task with `unoffered: true` that settles `tool_unavailable` and runs `afterTool`. `pi.tool` is v3 and `pi.generation` v2 (spec:3974-3976, 4601; CHANGELOG:64).
    - noel: "Tool X not found" error result (`crates/noel-core/src/lib.rs:5135, 5189`).
    - Gap: S. Only the view slot flag and hook coverage differ.

21. **`tool_search` and `searchTools()` ranking.**
    - Upstream: BM25 with the name as its own field, exact-name first, a whole-name bonus, and a Unicode tokenizer (`durable/src/search.ts`, 181 lines; spec:3802-3812; CHANGELOG:119-120).
    - noel: no `tool_search` (register line 691).
    - Gap: part of codemode/deferred-tools work.

22. **Codemode** (`durable/src/codemode/*`, 1707 lines; `packages/codemode`; spec:4365-4482).
    - Changes since the pin:
      - Harness clock (`api.now()`) for deadlines and wall time
      - `checkpointWhen` on its docs
      - `read` declares `structuredOutputSchema` (#11015)
      - import from `/declarations` so the QuickJS WASM is not loaded (#11254)
      - pi-schema params
      - namespace instructions in search
    - In codemode itself (1.2.0 at the pin, so before it): deterministic `sandbox.start()` with journaled replay, plus remote and inline sandboxes.
    - noel: none.
    - Gap: L (phase 3). It relies on nested calls (`NestedCallDoc`, `pi.live.nestedTools`) and three task- or conversation-scoped docs (`pi.codemode.run`, `.journal`, `.store`).

23. **pi-schema replaces TypeBox in tool schemas** (CHANGELOG:9).
    - noel: Rust schemas, plus the Bun extension host.
    - Gap: verify that the Bun host fixtures do not import TypeBox from pi-ai. Not checked.

### Storage

24. **`Storage.commit` takes batches.**
    - Upstream: one atomic seq per batch. `receive(index, seq)` is called per stored batch, a refusal stops at that batch, and any other error rejects the whole call. Optional `publishing()`. Conformance suite in `durable/src/testing/storage-conformance.ts` (spec:5991-6001; CHANGELOG:10-41).
    - noel: no trait. One append-only JSONL per session, and a compaction rewrite by atomic rename (`crates/noel-core/src/lib.rs:1601-1636`).
    - Gap: phase 6. Durable's Storage is a table and document store with global ids. noel's journal is a record log, so a 1:1 trait does not fit.

25. **`mintId` ids only grow, also across reopen** (spec:6002-6007).
    - noel: no global ids.
    - Gap: needed if phase 5 adds entry ids. Monotonic per session is enough.

26. **Group commit** (CHANGELOG:94-95), **`StoreStorage`** (Chord v2 store), **IndexedDB** (1.4), **SQLite `prefix`** (1.3), **SQLite schema v2** (1.4), and **no reads on the normal path** (caches).
    - noel: single writer per session.
    - Gap: not needed. Record `StoreStorage` and IndexedDB as non-parity.

27. **JSONL backend** (spec:6281-6320): sidecars, then one main marker, with optional fsync.
    - noel: fsync on run start and enqueue; a different layout.
    - Gap: deliberate non-parity, already implied.

### Tasks, scheduler, task graph

28. **New in 1.4 and 1.5:**
    - durable sleeping tasks (`sleepUntil`, `setTimer`) (1.4)
    - `Harness.wake()` and `onWake` (spec:447, 838)
    - task graph `blocked` reasons
    - the `task_interrupted` report
    - `TaskInspection.state` `ready` becomes `pending`
    - observers and the `observeCommits` shape (CHANGELOG:53-56, 69)
    - `Report.kind`
    - the scheduler scales to thousands of conversations (CHANGELOG:100)

    noel: no task model. Runs live in the controller.

    Gap: wait, or record as non-parity. Only the visible results matter for the view: `runFailure`, `compactionFailure`, and backoffs as `retry.at`.

### Env

29. **`FileChange` and `stop`.**
    - Upstream: `FileChange = {kind: "paths"|"overflow"|"failed"}`, and `FileWatcher.stop(context)` (`durable/src/env/index.ts:120-141`).
    - noel: `WatchChange::{Paths, Overflow, Error}` and `close` (`crates/noel-env/src/lib.rs:166-187`).
    - Gap: S. A rename only. The protocol is unchanged.

30. **pi-env daemon fixes and features** (`env/CHANGELOG.md` 1.5.0 and Unreleased; `env/docs/protocol.md`).
    - Upstream:
      - kills its process groups on SIGTERM, SIGINT and SIGHUP
      - `spawn_error` when a helper thread cannot start (#11433)
      - watch overflow when a change list exceeds the frame limit (#11605)
      - new `process` operation with `processWrite` and `processCloseStdin`, in `features`
    - noel: `FEATURES = ["listFiles"]` (`crates/noel-env-daemon/src/main.rs:36`). No signal handling found in `crates/noel-env-daemon/src`.
    - Gap: M. The SIGTERM kill is a correctness fix (a closed connection leaves commands running). `process` is needed only for MCP over pi-env. Not verified: whether noel's watch hits #11605.

### Compaction

31. **Live state.**
    - Upstream: `compactions[].trigger` and `compactionFailure.trigger` (renamed from `reason`, `pi.live` v2). An aborted compaction leaves no failure (CHANGELOG:51, 58).
    - noel: `compaction{reason}`; failures are not in the view.
    - Gap: covered by item 17.

32. **Per-model compaction thresholds.**
    - Upstream: `settings.compaction.modelOverrides`, keyed `"provider/modelId"` (1.4, #10942).
    - noel: none found.
    - Gap: S. A settings port.

33. **System baseline after compaction** (1.3.0, #10904).
    - Upstream: the baseline leads the context when the kept history has an assistant message.
    - noel: not verified. noel's system prompt is not a transcript entry. Probably N/A.

34. **Compaction rewrites history.**
    - Upstream: head markers stay in place in the view, and compaction never changes raw history (`HookApi.entries()`, CHANGELOG:85).
    - noel: compaction replaces the in-memory messages (item 14).
    - Gap: this blocks the windowed view. It is the main design question for phase 5.

## Moved spec citations

At tip `durable/docs/spec.md` has 6525 lines; at the pin it had 5352. Every citation noel makes has moved.

| noel cite (where) | content | tip location | content changed? |
|---|---|---|---|
| `spec.md:67` (`docs/spec.md:16,89`) | invariant 3, "All visible progress is durable" | `:71-75` | yes: "except what a deferred commit holds" |
| `spec.md:2329-2360` (`docs/spec.md:50`, register:1132) | admission table, then withdraw and abort | `:2903-2990` | yes: continue, whenIdle, maxTurns, afterRun, afterFailedRun rows and paragraphs |
| `spec.md:2355-2357` (`docs/spec.md:63`) | withdraw outcomes | `:2986-2988` | no |
| `spec.md:2357-2358` (`docs/spec.md:68`) | abort withdraws queued steer and follow-up | `:2988-2989` | no |
| `spec.md:3735-3745` (register:961) | progress is throttled durable commits; recovery | `:4502-4512` | yes: deferred, `persistWithinMs`, "may leave no aborted entry" |
| `spec.md:3738` (`docs/spec.md:89`) | same paragraph | `:4502-4507` | yes |
| `spec.md:3999-4006` (`docs/spec.md:89`, register:961) | `request` converts the leftover partial and throttles | `:4884-4908` | yes: interrupted request charged as a failed attempt |
| `spec.md:4078` (`docs/spec.md:89`) | settlement converts the partial | `:5026` | no |
| `spec.md:4118` (`docs/spec.md:89`) | `execute` interrupted result | `:5071` | yes: `afterTool` chain, `restart_limit` |
| `spec.md:4542-4558` (`docs/spec.md:228`) | §9.3 `ConversationView` | `:5595-5642` | yes: window, `before`, `ConversationWindow` |
| `spec.md` §9.3 (register:1329) | conversation view | §9.3 (`:5595`) | number unchanged; old §9.4 (`watchEvents`) removed, old §9.5 task graph is now §9.4 |
| `README.md:404` (`docs/spec.md:226`) | "nothing is replayed" | `README.md:525` | yes: the 100-frame cap becomes 10,000 ops, folded |
| `README.md:406` (`docs/spec.md:89`) | 100 ms commit window | `README.md:527` | yes: deferred, `persistWithinMs` |
| `src/env/index.ts:208-373` (`docs/spec.md:283`) | `FileSystem` .. `ExecutionEnv` | `:211-377` | yes: `FileChange`, `stop` |
| `src/harness/types.ts:569-582` (`docs/spec.md:284`) | `EnvTarget` and `HarnessOptions.env` | `EnvTarget :679`, `env :705` | the `env` signature is unchanged |

Section numbers for spec §4 (now with §4.1 Deferred commits), §7.6 Code mode (`:4365`) and §8.2 Live document (`:4539`) are stable.

## Ranked port list

Sizes: S is under a day, M is a few days, L is a week or more. These are estimates and have not been checked against code effort.

### (a) Codemode (phase 3)

1. **L. Codemode tool and journal.** Port `durable/src/codemode/codemode.ts` (deterministic `start()`, journal, replay, divergence) into `noel-codemode` on djs. The run, journal and store docs map to JSONL records. Decide first whether noel follows Durable's codemode or coding-agent's (`coding-agent/src/extensions/codemode/`), which is not journaled (see Open questions).
2. **M. Nested calls.** Model-invisible nested tool calls with their own live rows (`pi.live.nestedTools`), abort, and replay policy. Codemode needs these, and a GUI shows them.
3. **S. Search.** `search.ts` BM25 ranking, Unicode tokenizer, and `tool_search`. Small and self-contained; can land first with goldens.
4. **S. `read` structured output.** `read` declares `structuredOutputSchema` (text, or `ImageContent`) so scripts get images.

### (b) View parity for GUI clients (phase 5)

5. **L. Entry ids and raw history.** Give journal message records monotonic, per-session entry ids. Keep the raw history across compaction, with the compaction summary as a head marker in place instead of replacing `messages`. Everything else in the view depends on this. Timestamps: `EntryRecord` has no timestamp field; messages carry `timestamp`.
6. **M. Windowed view.** `{min, max, from}`, `before`, the splice on overflow, and an entries paging method on `noel.chat` (a new contract version).
7. **M. `pi.live` v3 subset.**
   - `run.turns`
   - `runFailure` with noel's reasons, replacing or adding to `lastRun`
   - `compactions[]` with `trigger`, and `compactionFailure`
   - tool slot `entry`, `droppedBytes`, `droppedLines`, `details`
   - usage on the partial
   - decide on `interrupted` and `truncated`
8. **M. `ConversationTranscript` in `noel-client`.** `merge`, `loadEarlier`, `fillGap`, `resume`. The TUI attach then renders from `items`.
9. **S-M. Run-end semantics:**
   - charge an interrupted request to retry, and end with `interrupted`, instead of resending
   - restart limit gives `restart_limit`
   - the `continue` submission
   - `whenIdle: "reject"` on `submit`
   - map `maxToolRounds` to `turn_limit`

   All of these feed `runFailure`.

### (c) Storage trait (phase 6)

10. **M. Journal trait.** Model the trait on noel's journal (append a group of records atomically; rewrite by rename), not on Durable's table/document `Storage`. Adopt from Durable: batch commit with per-batch receipts, the refused/unknown distinction, monotonic ids, and a conformance suite modelled on `storage-conformance.ts`.
11. **S, optional.** `persistWithinMs`-style deferral, if a remote or SQLite backend makes the 100 ms appends costly.

### Independent of the phases

12. **M. noel-env-daemon** (do this soon; it is a correctness fix):
    - kill process groups on SIGTERM, SIGINT and SIGHUP
    - `spawn_error` on helper-thread failure
    - watch overflow at the frame limit
    - later, `process`
    - then re-record `pi-env.json` at a newer pin
13. **S. noel-env names.** `FileChange` with a `Failed` variant, and `stop`.
14. **S. Per-model compaction thresholds** (`compaction.modelOverrides`).

### Wait, or record as deliberate non-parity

- The task model:
  - sleeping tasks
  - `wake` and `onWake`
  - the task graph
  - `afterRun` run-end tasks
  - `readOnly` Harness
  - observers and `observeCommits`
  - `Report` and telemetry
- Group commit, `StoreStorage`, IndexedDB, and the SQLite schema.
- `Conversation.submit(tx, draft)`, `beforeProviderRequest`, and the handoff control.
- `afterTool` on every result, unless coding-agent's `tool_result` changed the same way (check first).

### Documentation fixes

These are text-only updates for when the owner approves this audit:

- the register's "Durable Partial Output": deferred storage
- the register's "Mode 2" view entry: the 100-frame claim (register line 1354), `pi.live` v3, the window
- `docs/spec.md` §4.1, which now differs from upstream on resend
- every moved citation in the table above

## Open questions

- **Which codemode to follow?** Durable's journaled, replayable codemode (Mode 2 recovery) or coding-agent's codemode extension (local TUI)? The roadmap names both `packages/codemode` and `durable/src/codemode`. Their results and details may differ.
- **Upstream's interrupted-request rule for RPC mode?** Should noel adopt it in every mode, or only in Mode 2? Published pi (coding-agent) has no crash recovery to compare against.
- **`lastRun` and `runFailure`.** Replace `lastRun` with `runFailure` in `pi.live`, or keep both for one contract version?
- **Entry ids.** New ids on existing journals: assign them at load (deterministic by position), or only to new records? Durable ids are global and monotonic; per-session is enough for noel.
- **Not verified:**
  - whether coding-agent's `tool_result` hook now covers harness-written errors
  - whether noel's daemon hits pi-env #11605
  - whether the Bun extension host or `scripts/ts-parity/` fixtures import TypeBox from pi-ai
  - whether upstream still saves a reply's first partial one interval late (register line 975)
  - the #10904 baseline fix's relevance to noel
- **The roadmap's "codemode is now public".** Codemode was already public (1.2.0) at the pin. Should the roadmap be worded differently?

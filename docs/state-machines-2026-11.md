# Plan: explicit state machines (November 2026)

Asked by the owner (2026-11-07): finish #129 and #162 with all their
sub-issues, #130 through #167, today.

An audit found about 70 lifecycles held as parallel bools, Options, counters
and status strings. Each is to become one enum, or a type whose states cannot
contradict each other. The structural cleanups (#163–#167) make those fixes
cheaper, so they come first where a fix depends on them.

## Rules

- **Confirm before fixing.** An issue found by reading the code only is
  confirmed against the code first. If it is wrong, it is closed with the
  reason and gets an audit row saying so.
- **A failing test first.** A bug gets a test that fails before the fix and
  passes after it. Provider tests use the faux provider.
- **Checks.** `make check` passes for every commit, and `make e2e` too for
  TTY behaviour (attach, the TUI, the client and worker lifecycle).
- **One issue per commit**, or one commit per tightly coupled group. Each
  commit message says `Fixes #N`.
- **Merging.** The work merges to `main` through a PR at each group's end,
  and the issues close on merge.

## Order

1. **Small, clearly wrong:**
   - #130: Run.closed is never reset on resume.
   - #131: the extension host pipe breaks after one failed request.
   - #132: the Bun host's duplicate sessionStart case.
2. **Provider streams:** #135, with noel-ai's part of #163 (inline tests
   out, the stream parsers split).
3. **Typed protocol, then the client and worker lifecycles:**
   - #164 first;
   - then #133 (attach's working counter);
   - #134 (worker exit between accept and start);
   - #144 (server workers);
   - #137 (client reconnect and submit retry);
   - #146 (attach session switch);
   - #145 (view ToolSlot);
   - #143 (rpc_wire Recorder).
4. **Agent and core:**
   - #138 (saved queue);
   - #140 (dropped Start token);
   - #141 (turn loop mode);
   - #142 (duplicated run state);
   - #139 (ExtensionHosts);
   - #156 (codemode VmSession).
5. **Chord and env:**
   - #136 (relay PathState);
   - #150 (RefEntry);
   - #151 (close racing start_session);
   - #152 (remote watcher);
   - #153 (exec and pty outcome);
   - #154 (FileWatcher).
6. **Config, auth and TUI:**
   - #147 (failover pool);
   - #148 (OAuth refresh result);
   - #155 (CliModel and ParsedModel);
   - #149 (TranscriptView);
   - #157 (iOS NoelModel and SSHChannelOpener; Swift, built where Xcode is
     available, or recorded as not verifiable here).
7. **Per-crate leftovers:** #158 (noel-cli), #159 (noel-core), #160
   (noel-ai, noel-config, noel-tools), #161 (noel-chord, noel-env).
8. **Structure:**
   - the rest of #163: noel-cli main.rs, noel-core lib.rs and noel-config
     lib.rs, inline tests out first, then the split;
   - #165: tool schemas next to the tools;
   - #166: mode strings become enums;
   - #167: long functions broken up.

   These are behaviour-preserving: the tests and fixtures stay the same.

## Done

- **Issues:** every issue from #130 to #167 is closed by a merged commit, or
  closed as not reproducible with the reason.
- **Trackers:** #129 and #162 are closed.
- **Audit:** every issue has a row below with its outcome.
- **Main:** `main` passes `make check` and `make e2e`.

## Audit

Outcomes: **fixed** (with the test that fails before the fix), **refactored**
(no behaviour change), or **not reproducible** (with the reason).

| issue | outcome |
|---|---|
| #130 | fixed: Run's transitions live on `Run` (close, restart, reopen, finish), the same live and on replay; resuming a closed run journals `RunReopened`. Test `a_crash_after_resuming_a_closed_run_counts_a_restart` |
| #131 | fixed: the host's stdout is `Pipe::Idle/Busy/Broken`, kept by the host; a broken or exited host is started again on its next call. Test `a_cancelled_request_does_not_break_the_host` |
| #132 | fixed: the Bun host has a `Phase` (loading, factoryFailed, ready, shuttingDown); session_start runs at init, session_shutdown at shutdown, unknown kinds get an error; shutdown waits for the host. Test `bun_host_runs_session_start_and_shutdown_handlers` |
| #135 | fixed: one `SseParser` per wire format serves the streaming and the batch path, and its `finish` returns the raw stop reason or upstream's error; exactly one `Stop`, last. The `StopReason` enum is not adopted: the raw reason is upstream's `rawStopReason` (non-parity register, "Provider Stream Endings"). Test `streams_end_with_their_provider_terminal_or_an_error` (fails before the fix) |
| #163 (noel-ai) | refactored: tests in `tests.rs`, one module per provider; the other three files follow in group 8 |
| #164 | fixed: `noel.chat` call arguments and answers are serde types in `server/protocol.rs`, shared by attach and the worker; the wire is unchanged. Tests `calls_keep_their_wire_shape`, `an_answer_names_the_run_it_started` |
| #133 | fixed: attach's run is `RunView { Idle, Starting, Running }` with one `advance`; a start never seen gives up after 10s; pending calls are a set of tokens, so the counter cannot drift. Test `the_run_view_follows_the_live_run_and_gives_up_on_an_unseen_start` |
| #134 | fixed: the worker is `Slot { Idle, Working, Starting, Running }` and `Phase { Serving, Draining, Leaving }`; it exits only with an idle slot, a new server connection resets Draining, and Leaving refuses new work. Test `the_worker_exits_only_with_nothing_in_hand` |
| #144 | fixed: the server keeps a `Starting` guard per spawning worker, so an idle exit cannot happen mid-spawn, and a leaving worker is never handed a new hold (`live_link` skips it). Covered by `the_worker_exits_only_with_nothing_in_hand` and the server tests |
| #137 | fixed in part, rest wrong: the connection is `Connected/Lost` in a watch channel, reconnected by a supervisor task that needs no follower. Test `a_lost_link_is_reconnected_without_a_follower`. The duplicate-submit claim is wrong: a retry keeps its `requestId`, which the worker dedupes |
| #146 | fixed: answers carry the session generation they were sent in, and one from an earlier session is dropped; the shell row is found again after the transcript syncs (`ShellRow`, `Transcript::sync` returns removed rows) |
| #145 | fixed: `ToolSlot` status is `ToolPhase`, compaction triggers are `CompactionTrigger`; progress for a finished tool is ignored; a failed manual compaction shows. Test `late_tool_progress_is_ignored_and_a_manual_compaction_failure_shows` |
| #143 | fixed: the recorder's tools are one list of calls and compaction is `CompactionPhase { None, Overflowed, Running }`; a stop ends every open tool. Test `a_stop_ends_every_open_tool` |
| #138 | fixed: the saved queue is one list of `QueuedEntry { mode, content, id }`; enqueueing keeps the other entries' ids. Test `queueing_a_message_keeps_the_saved_ids_of_the_others` (fails before the fix) |
| #140 | fixed: the controller's start gate is `Gate { Open, Accepted, Closed }`; a Start dropped unrun leaves its run interrupted, not running. Test `a_start_dropped_unrun_leaves_its_run_interrupted` (fails before the fix) |
| #141 | fixed in part, rest wrong: the attempt counts retries only (a compaction restarts at 1), so a resume no longer charges a compaction as a retry; `ResumeHint` is one enum, set once at open and taken by the resume. Wrong: `can_compact` is last in its `&&` chain and takes the attempt only when it can compact; a second `RoundStarted` after a retry is the new attempt's, which the view and RPC treat as such. The loop keeps its locals: `Recovery` already holds the retry and overflow state. Test `a_reply_saves_what_streamed_since_its_last_record_at_most_every_interval` (extended) |
| #142 | fixed: the run's session copy follows every saved change; `HistoryRewritten` carries the history a compaction saved, and todos and compaction records are mirrored. `Live.saved` (last saved queue) and `Live.past` are the controller's own while the session is open, read from the session only when it opens. Test `a_session_kept_from_the_events_follows_a_compaction` |
| #139 | fixed: one `HostEntry { spec, phase: Running \| Failed }` per extension. A failed host is not respawned each run until its file, cwd or session changes; a success replaces the failure; another path or session restarts the host; shutdown clears all. Test `a_failed_host_waits_for_a_change_and_a_success_clears_it` |
| #156 | fixed: `VmSession` is `Phase { Starting, Preparing, Ready, Ended }`; nothing is posted after the end; the bridge message is an enum and a bad kind or id crashes the VM; `VmResult` carries a `Result`; `CallStatus::Running`. Tests `nothing_is_posted_after_the_end`, `a_broken_bridge_message_is_refused` |

## Status

Started 2026-11-07.

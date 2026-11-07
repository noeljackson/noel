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
| #136 | fixed: a path is `PathPhase { Unlinked, Resolving, Linked, Failed }`; a failed route address resolves again for its next holder (upstream `route.ts`), one resolve at a time, and a link resolved after another was adopted is closed. Retry on a ref failed without a hold does nothing, as upstream's `retry` (no hold to ask). Test `a_failed_route_address_resolves_again_for_a_new_holder` (fails before the fix) |
| #150 | fixed: a reference's hold and replica are one `Binding { Unheld, Held { link, hold, replica } }`; loading, failed and gone drop the replica, so call and submit agree and no stale state is served. Test `an_unbound_reference_keeps_no_stale_state` (fails before the fix) |
| #151 | fixed: the session is published under the lock that checks `closed`, before the spawn; a close either comes first or tears it down, and the start then kills the child. The race needs a close inside `spawn()`, so no test reproduces it; covered by the existing connection tests |
| #152 | wrong, closed: the daemon ends a watch after its `error` event, so nothing keeps running; the stop flag and the kept mode are upstream's (`env/src/watch.ts`) |
| #153 | wrong, closed: a cancel in the stdio grace period gives `aborted` in upstream too (`durable/src/env/node.ts` settles after the grace period and checks the signal), and the outcome order matches upstream |
| #154 | fixed: the flush and poll deadlines live on the watcher; switching to polling clears the native deadlines and queued events, sets the first poll one interval away, and ignores late native events. Test `switching_to_polling_drops_the_native_state` |
| #147 | fixed in part, rest wrong: the state file's lock now spans read and write, so concurrent saves keep every change (test `concurrent_saves_keep_every_change`, fails before the fix); each attempt reads the clock. Wrong: memory and disk agree (a switch is saved with the sticky change that carries it); the limited and exhausted maps hold two different facts and are bounded by the accounts |
| #148 | fixed: the refresh returns `RefreshOutcome { Fresh, Refreshed, Kept { reason } }`; callers read it instead of comparing credentials; a refreshed credential keeps its kind. Test `failed_refresh_keeps_unexpired_token` (extended) |
| #155 | refactored: `CliModel` is `Empty / Found / Custom / Error`, the provider reading `ProviderHint`; the upstream parity fixtures still pass |
| #149 | wrong, closed: entering Reading needs no other field set, a removed anchor moves to the next entry, `pending_copy` is empty during a drag, and `was_latest` is the state before the press (issue comment) |
| #157 | fixed, Swift not compiled: the app and FFI report `Reconnecting`, `Connected` again, and `Failed` (test `the_app_reports_the_link_coming_back`); the Swift bindings are regenerated; `NoelModel` keeps a `ConnectionState`, does not connect twice, drops a failed `Noel`, clears `streaming` on select and ignores a late view; `SSHChannelOpener` settles once under a lock, times out, and closes the client. Not verifiable here without Xcode |
| #160 | fixed: a chat tool call's first id and name stand (upstream), done text is tracked per output item, and `cap_tool_output` gives a notice for an over-long first line. Tests: the chat parser test (id and name resent), `openai_responses_done_text_of_an_item_without_deltas_is_kept`, `oversized_extension_output_is_capped_and_saved` (extended). Wrong: grep's limit needs another match to report a cut (`more`); project trust stores no denial (noel has no prompt). The rest is representation, in #175 |
| #159 | fixed: a reload keeps the live v2 registrations of the extensions still loaded (`a_reload_keeps_the_live_registrations_of_loaded_extensions`); noel-agent reports `TurnCancelled` for a cancel during a request (`a_cancel_during_the_request_reports_the_turn_cancelled`). Wrong: the `offline` latch keeps the command line's `--offline`; a codemode cancel after finishing still ends the turn because the run was aborted; a sticky save error ends the request. Unfixed and filed in #175: manual `/compact` cannot be aborted |
| #158 | fixed: `~~~` fences open and close code blocks (`classifies_common_blocks`); the device login timeout names its length. Wrong: rpc's `CallStarted` always follows `RunStarted`; the worker's aborts belong to the one active run; Codex device auth has no `slow_down`. Real gaps left in #175: model and thinking writes, session setup, pairing and login bookkeeping, the `OPENED` cache |
| #161 | matches upstream, closed: remote readers' handles are session-bound and have no finaliser upstream too; upstream's daemon does not require `hello` first. The representation items are in #175 |

## Status

Started 2026-11-07.

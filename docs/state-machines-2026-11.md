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

## Status

Started 2026-11-07.

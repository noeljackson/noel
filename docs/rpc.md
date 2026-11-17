# RPC Mode


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

When [account failover](configuration.md#account-failover) switches accounts, noel sends one
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

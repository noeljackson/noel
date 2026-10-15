# noel specification: sessions, durability, server and client

This is noel's normative specification for what outlives a single process:
sessions and their journal, runs and their recovery, submissions and the
queue, and the server, worker and client that let several clients share a
session (Mode 2). Where noel follows upstream pi, the section says so and cites
upstream's own spec; where noel differs, it says why. When this document and
the code disagree, one of them has a bug.

Status markers:
- **Built**: in noel `main` today.
- **Decided**: decided by the owner, not built yet.
- **Proposed**: this document's proposal, awaiting the owner.

Upstream is pinned at `earendil-works/pi` `1692bba` (2026-10-14). Citations
such as `durable/docs/spec.md:67` are under upstream's `packages/`.

## 1. Two modes

**Decided (2026-10-14).**

1. **Mode 1, agent in the environment.**
   - A supervisor runs `noel --mode rpc` inside an environment and messages it through upstream pi's RPC protocol, with noel's extensions: run ids, expected-run ids, request ids, and run resume.
   - Codewire's managed conversations are one supervisor. They are **Built** on trusted Linux hosts; Codewire Sandboxes are **Decided**.
2. **Mode 2, server and client.**
   - A long-running `noel server` holds sessions, and clients attach to it from anywhere (§5–§9). **Built** for clients on the same machine (device access, §6): `noel server`, `noel worker`, the line-mode `noel client`, and the TUI attached to a session, `noel --attach` (§7.4).
   - A session's tools run in a workspace chosen through an adapter: local, a remote machine through `pi-env`, a Codewire environment, or anything else that implements the workspace interface (§10). **Built** for local and for a remote machine over `ssh`, in every mode.

noel works without Codewire in both modes. Every Codewire path is optional.

Both modes use one run engine (`noel-core`), so the rules in §2–§4 hold for
the TUI, print mode, RPC mode and the server alike.

## 2. Sessions and the journal

**Built.**
- **The journal.** A session is a JSONL journal, `<session-id>.jsonl`. Each record is written with its newline in one write, and a torn final line is truncated when a writer opens the file. Listing skips files it cannot read.
- **The lock.** One process writes a session at a time. It holds `File::try_lock` on `<id>.jsonl.lock`; the lock is on that file because compaction replaces the journal itself. A second writer waits up to 5 s, then exits with code 75. Listing, forking and import read without the lock.
- **Runs.** Each run's start (`runId`, `requestId`) is saved with its prompt and fsynced before the prompt is acknowledged. Its end (completed, failed or aborted) is saved before the run is reported settled. A run with a start and no end is *interrupted*.
- **The queue.** Queued steers and follow-ups are saved with the session, and each enqueue is saved before it is acknowledged. A queued submission with a `requestId` is saved with its identity `{submission_id, request_id, mode}` (the queue record's `items`); the user message that places it into a run names it (`placed`), so it leaves the queue in the same record; a queue record that withdraws it names it too (`withdrawn`). All three fields are additive: older journals lack them and load as before, and a submission without a `requestId` gets a new `submissionId` each time the session opens, since nothing can ask about it after its process ends.

**Built:** durable partial output (§4.2): the journal also saves what a
streaming reply and a running tool call have produced so far, and an aborted
reply stays in the transcript.

## 3. Submissions, the queue and abort

### 3.1 Submissions

**Built** in the session controller (§9), following Durable (`durable/docs/spec.md:2329-2360`):
- **Every submission can carry a `requestId`:** a prompt, a steer or a follow-up. A repeat returns the first submission and writes nothing. A `requestId` reused for a different kind of submission is refused. Deduplication happens within one session, before any write, and holds across restarts: a run's start saves its `requestId` and kind (`kind`, left out for a prompt), and a queued submission's is saved as §2 says.
- **When the session is busy, a submission says what to do:**
  - `reject` refuses it, and writes nothing;
  - `steer` queues it for the next boundary inside the run, after the current tool round;
  - `followUp` queues it for when the run would answer.
- **Each queued submission has its own id.** A submission that starts a run is named by the run's id. A submission ends either *done*, when the run it was placed into completed, or *unanswered*: withdrawn, or placed into a run that failed or was aborted.
- **While no run is active,** a prompt starts one, and what is still queued joins it: steering in its first round, follow-ups after it would answer.

**RPC mode** keeps its wire: only a `prompt`'s `requestId` is read (a repeat replies `started` or `handled` with the run's id, as before), a queued `prompt` cannot carry one, and replies carry no submission ids.

### 3.2 Withdraw

**Built** in the session controller (Durable `spec.md:2355-2357`). `withdraw(submissionId)` removes a submission that is still queued, and reports it unanswered with reason `aborted`. For an input already placed into a run that has not ended (running or interrupted) it reports `already_placed`; for a submission that has ended it reports `settled`. RPC mode has no withdraw command.

### 3.3 Abort

**Decided (2026-10-14); built** in the session controller as a policy each caller chooses (`AbortPolicy`). Each mode follows the upstream it mirrors:
- **Mode 2** (server, worker, client) follows Durable (`spec.md:2357-2358`): aborting a run also withdraws every queued steer and follow-up of that session. Each is reported unanswered with reason `aborted`, and the next prompt starts with an empty queue. An abort with no run active withdraws them too. (`WithdrawQueue`, for the worker.)
- **RPC mode, the TUI and print mode** follow published upstream pi: abort stops the run and keeps the queue. Upstream's `Agent.abort()` only cancels the active run, and its queues are cleared only by `reset()` (`agent/src/agent.ts:341-367`). The RPC goldens guard this. (`KeepQueue`, which RPC mode uses.)

**Built, kept:**
- An abort may name `expectedRunId`, and then changes nothing unless that run is the active one.
- An abort with no run active ends an interrupted run as aborted.

## 4. Runs, partial output and recovery

### 4.1 Recovery

**Built.**
- A supervisor decides when an interrupted run resumes: the Mode 1 driver, or the Mode 2 server (§8.3). noel never resumes in RPC mode on its own.
- The TUI continues an unfinished session when you reopen it.
- **What runs again on resume:**
  - a model request that a crash cut off is sent again, after what its reply had streamed is saved as an aborted reply (§4.2);
  - replay-safe tools (`read`, `grep`, `find`, `ls`) run again;
  - any other interrupted tool call gets an "interrupted" result holding the output it had saved (§4.2), and never runs again.

### 4.2 Partial output is durable

**Decided (2026-10-14, follows Durable `spec.md:67`, `README.md:406`, `spec.md:3738, 3999-4006, 4078, 4118`); built** in every mode (TUI, print, RPC), in the run engine (`crates/noel-core/src/progress.rs`). Where it differs from Durable is listed in the non-parity register ("Durable Partial Output").
- **What is saved while a call runs:** a streaming reply's text and thinking so far, and a running tool's output so far (the end of it, within its result's limits). Each is committed to the journal at most every 100 ms, under the settings `progress.partialIntervalMs` and `progress.outputIntervalMs` (default 100), and only when it changed. The first change after a quiet interval is saved at once and later ones together at the interval's end, so a crash loses at most the last interval. Saving writes nothing to RPC mode's stdout.
- **An interrupted reply:** a stop leaves its committed partial in the journal. Opening the session for writing (the next resume, RPC mode starting or switching to it, the TUI reopening it) saves it as an *aborted* assistant entry before anything else happens, so a resumed run sends the request again without it. A reply that streamed nothing leaves nothing.
- **An aborted or failed reply:** an abort, and a failure that ends the run, save what the reply streamed as an aborted entry. A reply the run drops to ask again (a retry, or a compaction after an overflow) is superseded instead and leaves nothing in the transcript (non-parity register, "Run Recovery").
- **An interrupted tool call:** its result is the output saved so far, then upstream's diagnostics block, and is an error:

  ```text
  <output so far>

  <harness>
  [warn] Output truncated to its end        (only when the start was dropped)
  [error] Tool bash was interrupted and may have partially run
  </harness>
  ```

  A resumed run saves it for each pending call that is not replay-safe, without reporting it; replay-safe calls run again. A new prompt after such calls saves it for each of them, since nothing will run them.
- **An aborted tool call:** an abort saves its result as what it printed, then `Operation aborted` (published pi's text); calls of the same reply that had not started get `Operation aborted` alone.
- **Aborted replies** are kept in the transcript and left out of later model requests, of the context estimate and of compaction summaries, as upstream pi does. `get_messages` reports one with `stopReason: "aborted"` and `errorMessage: "Request was aborted"`, `messageCount` counts it, the TUI shows it followed by `aborted`, and a turn whose last reply is aborted is still unfinished.
- **What clients see (Decided):** a client that joins late or reconnects sees the committed partial, as part of the current view (§7.3).

**Journal shape (built):** two records, additive, which older versions ignore (`#[serde(other)]`); older journals have neither and open as before. A session's journal only grows, so neither record rewrites what an earlier one saved while it can append:
- `{"type":"partial","run_id"?,"attempt","text"?,"thinking"?,"append"?}`: what a reply streamed. A reply attempt's first record holds everything so far and supersedes any earlier partial; each later one (`append: true`) holds only what streamed since. `attempt` counts the requests for one reply from 1 (a retry or a re-ask after compaction is the next). A long reply therefore adds about its own size to the journal: a 90 000-byte faux reply streamed over 13 s wrote 128 records, 105 376 bytes, where a full snapshot per commit would have written about 5.8 MB.
- `{"type":"tool_output","run_id"?,"call_id","output","truncated"?,"append"?}`: what a call printed. It appends while the output grows. Once the output passes its result's limits, the kept end moves (`truncated`): each record then rewrites it whole, and the wait before the next rewrite doubles, from `progress.outputIntervalMs` up to 2^16 times it, so a command that prints for an hour rewrites its output about fifteen times.
- **Supersession:** a reply's message (or any message, a messages snapshot, a compaction checkpoint, or the run's end) supersedes the partial; a dropped reply is superseded by an empty partial record; the call's result message (or the run's end) supersedes its tool output.
- **Compaction** rewrites the journal (§2) without the records it makes stale: every partial, and every tool output except the current one of a call still without a result.
- `run_id` is the run's id in RPC mode and in the Mode 2 worker, and absent in the TUI and print mode, which have no runs.

## 5. Server, worker and client roles (Mode 2)

**Decided (2026-10-14).** noel follows upstream's current direction: Chord's host runtime (`chord/docs/guide.md:657-692`). Upstream removed its experimental `pi server` and `pi client` in 1.2.0 because the runtime they served was replaced (`chord/CHANGELOG.md:53`).
- **Server:** one host per user. It owns principals, routes and the workers' lifecycle.
- **Worker:** one host per session, named `session:<id>`. It is the session's only writer: it holds the journal lock (§2) and runs the run engine.
- **Client:** the TUI, and a line-mode `noel client`. A client holds remote objects through the server, which relays between clients and workers.

**Built** (`crates/noel-cli/src/server`):
- **The runtime directory** is `$XDG_RUNTIME_DIR/noel`, else `/tmp/noel-<uid>` (`NOEL_RUNTIME_DIR` overrides both). It must be a directory, not a link, owned by the user with mode 0700; it is made so when missing, and anything else is refused.
- **The server:**
  - `noel server --foreground` takes `server.lock` there (`File::try_lock`); a second server exits with code 75. Without `--foreground`, `noel server` starts a detached server (output in `server.log`) and returns once it listens, or exits 75 if one already runs;
  - it binds `server.sock` (0600), removing a stale one only when it is a socket the user owns, and writes `server.json` (`pid`, `build`, `startedAt`);
  - it reads the global settings only, from the home directory, and gives its session directory to its workers;
  - with no client link and no worker for `server.idleExitMs` (default 600000, ten minutes) it exits, removing its socket.
- **Workers:**
  - the server starts each as `noel worker --session <id> --generation <g> [--resume <runId>]` in its own session (`setsid`), with its output in `workers/<id>.log`;
  - the worker opens the journal under its lock (`fatal`, reason `locked`, exit 75 if another process holds it), binds `workers/<id>.sock` (0600), writes `workers/<id>.json` (`pid`, `session`, `generation`, `build`), then prints the control line `ready` on stdout;
  - the server dials the socket, checks the peer's uid and pid, reads `ready` again, and runs the Chord link as the worker's parent, over NDJSON lines `{"chord2": frame}` and `{"chord2control": message}`: `ready` (`pid`, `build`, `session`, `generation`), `status` (`viewers`, `running`, `queued`, `runId`) and `shutdown`;
  - a worker with no holds, no run and no queued input for `server.workerIdleExitMs` (default 60000) exits, removing its socket and record;
  - a worker outlives its server: it finishes its run, then waits up to that same idle time for a new server to adopt it;
  - a new server reads `workers/*.json`, dials each socket and checks uid, pid and `build`. A match is adopted; another build is told `shutdown` and exits once its run settles; a record whose process is gone is removed.
- **Clients:** a client starts the server when none listens, after checking the runtime directory and the listening process's uid. The clients are the line-mode `noel client` and the TUI, `noel --attach <session-id>` (§7.4).

## 6. Principals and access

**Decided (2026-10-14), following Chord's model (`chord/docs/guide.md:315-334`, `spec.md:230, 275`).**
- **Owner:** `{kind:"owner", via:"device"|"tunnel"|"relay"}`.
- **Guest:** `{kind:"guest", id, name, session, mode:"view"|"full", revision}`. A guest is invited to one session and can be revoked.
- **Worker:** `{kind:"worker", session, generation}`.
- **Stamping:** the side that accepts a connection stamps its principal. A principal forwarded from another host is honoured only from the parent link, and principal fields inside payloads are ignored.
- **Routing:** a route's `authorize(principal, session)` runs before a worker is resolved or spawned, and again after.

**Build order (Decided):**
1. **Device** (**Built**). The owner-only local socket, with no token. The server checks the peer's uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and closes any other uid before reading a byte. Any process running as the owner can connect, including the agent's own `bash`; that grants nothing it could not already do. Accepted links are stamped `{kind:"owner", via:"device"}`; the `session:` route lets the owner reach any session and a guest only its own, and `noel.chat` gives the owner and a `full` guest `participant`, a `view` guest `viewer`. Workers act as `{kind:"worker", session, generation}`. Objects are kept by `principalKey` (`owner@<via>`, `guest:<id>:r<revision>`, `worker:<session>:<generation>`).
2. **Relay,** after a design pass of its own.
3. **Remote authentication,** offering both device-code pairing (the server issues and verifies its own token) and an optional Codewire login. A Codewire tailnet connection may be stamped `via:"tunnel"`, because admission to an environment's tailnet already grants shell access there.
4. **Guests.**

## 7. Wire protocol and objects

### 7.1 Link protocol

**Decided:** Chord link protocol version 2 (`chord/docs/spec.md` §9; frames in `chord/src/wire.ts`).

**Built** in `crates/noel-chord`, matching upstream's `packages/chord` at the pinned commit (`CHORD_REFERENCE_REF` in the Makefile), checked against `tests/fixtures/ts-parity/chord-delta.json` and `chord-link.json`, and against upstream's runtime by `make chord-interop`:
- **the delta codec:** the ops `r`, `s`, `d`, `a`, `t`, `p` and `m` applied immutably, with `t` and `a` counting UTF-16 code units; the wire encoder and decoder, with path definitions, numeric references and short tuples; and `diff`, whose batches are compared by the value they produce;
- **canonical JSON and `hash64`;**
- **contracts:** descriptions, their fingerprints, `coverage` and `fits`;
- **frames:** every protocol 2 frame, checked on receipt as upstream's frame schema checks it. A malformed frame, a duplicate id or a control frame from the wrong side closes the link;
- **links:**
  - WebSocket, one JSON text message per frame, also over a Unix socket (Chord has no Unix-socket transport of its own); packed binary frames are decoded where accepted, never sent, and refused from clients;
  - NDJSON lines `{"chord2": frame}`, Chord's IPC encoding, with `{"chord2control": message}` lines for control, unwindowed;
  - an in-memory pair, for tests;
- **connections:** `hello` with protocol 2, and `bye` with `protocol-version 2` to a peer of another version; holds (`shape`, `gone`, `release`); calls (`result`, `cancel`, `maxCalls`); subscriptions (`snap`, `ops` through one delta encoder per subscription, `reset`, `unsub`); `ping` and `pong` with a configurable interval and timeout; `ack` with the window, where only state waits, coalesced per subscription, and a subscription waiting beyond four windows is reset;
- **hosts:** object types served with contracts and a variant per principal; principal stamping by the accepting side, with forwarded principals honoured only from the parent link; routes whose `authorize(principal, host)` runs before and after `resolve`; relaying between a client and a child host, with roots and `root_end` on relayed calls; and `invalidate_principal`, which re-stamps or closes links, closes their relays and propagates to child hosts.

**Left out:** hot reload and plugins. noel sends no reload control; a host that receives `prepare` votes `unavailable/not_coordinator`, and ignores `commit`, `abort` and `retire`.

Where these differ from upstream is listed in `docs/rust-rewrite/non-parity-register.md`.

**Built:** WebSocket framing over the device socket (path `/chord`; packed frames refused from clients), and NDJSON between server and worker (§5).

**Decided (2026-10-14):**
- noel builds on protocol 2 now, although it is under `[Unreleased]` in Chord's changelog.
- The goldens are pinned to an exact upstream commit, and `ts-parity-drift` flags drift.
- Until Chord releases protocol 2, the server offers only device access.

### 7.2 Objects

**Built.** Each object has contract version 1; their descriptions, with their JSON Schemas, are checked in as `crates/noel-cli/src/server/contracts/*.json`.

```text
noel.sessions  at "server", input {}           owner only (variant owner)
  state directory: {sessions: [{sessionId, name?, cwd, workspace?, modified,
                                messageCount, firstPrompt?, model?,
                                live: {worker, busy, viewers},
                                lastRun?: {runId, status}}]}        newest first
  create({requestId, cwd, name?, model?, thinkingLevel?, workspace?})
      -> {sessionId, created}
  fork({requestId, sessionId}) -> {sessionId, created}

noel.chat      at "session:<id>", input {session}
  viewer:      state view
  participant: state view
    submit({requestId, text, images?, whenBusy?: "reject"|"steer"|"followUp"})
        -> {submissionId, runId?, disposition: "started"|"queued"|"repeated", status?}
    withdraw({submissionId}) -> {outcome: "withdrawn"|"already_placed"|"settled"}
    abort({expectedRunId?}) -> {runId?, withdrawn}
    compact({instructions?}) -> {omittedMessages, retainedMessages}
    configure({model?, thinkingLevel?}) -> {model?, thinkingLevel}
    resume({expectedRunId}) -> {runId}

noel.models    at "server", input {}           owner only (variant owner)
  state catalog: {models: [{provider, modelId, name, reasoning, thinkingLevels}]}
```

The owner, and a guest with `mode:"full"`, get `participant`; a guest with
`mode:"view"` gets `viewer`. Methods check their arguments: for example, that
the input names this worker's session.

- **`submit`** maps to the session controller (§3.1): `whenBusy` names the kind (`reject` a prompt, `steer`, `followUp`, default `reject`). While idle any submission starts a run. A repeated `requestId` answers `repeated` with the first submission's `status` (`queued`, `running`, `interrupted`, `completed`, `failed`, `aborted`, `withdrawn`) and its `runId`. A refusal writes nothing and is `invalid` with reason `busy`, `queued_prompt`, `request_id_reused` or `refused`.
- **`abort`** follows Mode 2's rule (§3.3): it withdraws the queue, and answers once the run settled.
- **`compact`, `configure` and `resume`** need no run active (`invalid/busy` otherwise).
- **`create` and `fork`** name the new session by their `requestId` (`s-` and 24 hex digits of its SHA-256), so a repeat returns the same session with `created: false`. A new session gets its model and thinking level as `noel` would start it; `create` lists it in the directory before answering. `create`'s `workspace` is a URL as `--workspace` takes it (§10): the session's tools run there, and a URL that is not valid, or names a host noel does not trust, is `invalid` with reason `workspace` and a message naming the trust command. A fork keeps its source's workspace. The directory's `workspace` is the URL of a session's workspace, absent for a local one.
- **The directory** is read from the session directory (each journal again only when it changed) and the workers' `status`, on each change and every 5 s. Changes that arrive together are read together, so a listing taken the moment a `submit` is answered may not show it yet; the session's view (§7.3) is current.

### 7.3 The view and late join

**Decided:** late join follows Durable. A client that joins late or reconnects starts from the current view, and nothing is replayed (`durable/README.md:404`). Subscribing sends the current snapshot first, then deltas; a reconnect is a new hold and a new snapshot.

**Built** (`crates/noel-cli/src/server/view.rs`): the view follows Durable's shape `{conversation, entries, docs}` (`durable/docs/spec.md:4542-4558`), plus `lastRun`:
- **`conversation`:** `{id, cwd, name?}`;
- **`entries`:** messages in upstream pi's `Message` JSON, as RPC mode's `get_messages`;
- **`docs["pi.agent"]`:** `{model?, thinkingLevel, cwd}`;
- **`docs["pi.live"]`:** `{}` while idle; during a run `run` (`{runId, requestId?, inputs}`), `generation` (`{attempt, message?, retry?}`, where `message` is the committed partial, §4.2), `tools` (`{callId, name, status, output?, truncated?}` with the committed output; `status` is `pending`, `running`, `done`, or `interrupted` for a resumed run's call that does not run again) and `compaction` (`{reason}`);
- **`docs["pi.inbox"]`:** `{items: [{id, mode, content, requestId?}]}`;
- **`docs["pi.usage"]`:** `{total: {input, output, requests}}`;
- **`lastRun`:** `get_state.lastRun`.

Partials and tool output reach the view from the journal's records as each is saved (`SessionStore::observe_progress`), so a client sees what a crash would keep. A subscription gets the snapshot, then ops: a splice at the end of `entries` for each message, appends to a streaming reply or a tool's output, and diffs of the documents. Where this differs from Durable is in the non-parity register ("Mode 2 Server, Worker and Client").

### 7.4 The attached TUI

**Built** (`crates/noel-cli/src/attach.rs`). `noel --attach <session-id>` opens the TUI on a session the server holds, starting the server when none listens (§5). It is the local TUI (terminal, editor, keys and transcript rendering) with a remote backend: a `noel.chat` reference through the server instead of a `Runtime` in the process. The local TUI's in-process path is unchanged.
- **Before the terminal:** the TUI holds the session and waits for its first view before it takes the terminal, so an unknown session (`invalid/session` from the server's route) or an invalid id is an error on the plain terminal, exit status 1. `--attach` takes no prompt, print mode or session flag, and refuses the options that shape a local session (model, thinking, tools, skills, templates, extensions, images and the like): the session runs with its own settings.
- **The transcript** is rebuilt from the view's `entries` as reopening a session rebuilds it from saved messages: a reply's thinking, then its text, then `aborted` for an aborted reply; a prompt; a tool's result with its call's command or path. After the entries, what the run streams in `docs["pi.live"]`: the committed partial's thinking and text, a retry it waits out, and each running or interrupted tool with its committed output. Each change of the view updates the rows it touches in place (a text that grows is appended to), so a streaming reply is one entry that becomes the saved reply, and the TUI's own notes stay where they were added.
- **Input:** Enter while no run is active submits with `whenBusy: "reject"`; during a run Enter queues a follow-up and the steer key (ctrl+s) a steer. Each submission has a new `requestId`, and one that fails `unavailable` is sent again with the same id (§8.1). A prompt refused `busy` because another client started a run meanwhile is queued as a follow-up, as Enter does during a run. Esc (or the interrupt key) during a run sends `abort` with the run as `expectedRunId`, once; Up on an empty draft withdraws the session's last queued submission into the draft.
- **Commands:** `/model <provider/id>` and `/thinking <level>` are `configure`, `/compact [instructions]` is `compact`, `/interrupt` is `abort`, `/queue` lists `pi.inbox` and `/queue-clear` withdraws it. Tool commands (`/bash`, `/read`, …), skills, templates and other text are prompts, which the worker expands with the session's configuration. The TUI's own display commands work (`/help`, `/hotkeys`, `/history`, `/status`, `/session`, `/clear`, `/copy` of a selection, `/quit`); any other of its commands acts on a session in this process and says it is not available when attached.
- **The footer** shows, during a run, what it does (from `pi.live`: compacting, a running tool, writing, thinking or waiting) and how many submissions are queued; otherwise the model and thinking level from `pi.agent`, the queue, and the last run when it was aborted, failed or interrupted (`lastRun`). A run that failed is noted with its error, and an abort another client made is noted when no aborted reply shows it.
- **Resume** is the server's (§8.3): the TUI never continues an interrupted run.
- **Recovery** follows `noel client watch` (§8.1): a lost worker is noted, then the view is held again and redrawn from its snapshot; a lost server is noted and reconnected, starting a server.

Where the attached TUI differs from the local one is in the non-parity register ("The TUI Attached to a Server Session").

## 8. Failure handling (Mode 2)

**Built** (§8.1–§8.3).

### 8.1 Client
- **A disconnect:** the server releases everything the client held. The run continues.
- **A submission's outcome is unknown when the link drops.** The client retries with the same `requestId`, which reports either `repeated` or the outcome. `noel client send` retries a hold or a `submit` that fails `unavailable` up to ten times.
- **`noel client watch`** and the attached TUI (§7.4) hold the view again when its worker is lost, and reconnect (starting a server) when the server is. The TUI sends a `submit` that fails `unavailable` again with the same `requestId`, as `noel client send` does.

### 8.2 Server
- **Losing the server stops no run:** a worker finishes its run while no server is attached.
- **A new server adopts the workers** (§5). Clients reconnect, hold their objects again, and get a fresh snapshot.

### 8.3 Worker
- **A worker crash:** the kernel releases its lock, and the run is interrupted.
- **Restart:** the server starts it again with the next generation. The new worker repairs a torn tail, applies §4.2, and resumes the run.
- **Resume limit:** the server resumes a run by itself at most 3 times. After that the run stays interrupted until a client resumes or aborts it.
- **When:** a worker lost with its run interrupted is started again at once. Any worker the server starts resumes the journal's interrupted last run while its resumes last, so after a machine stopped the first hold resumes it. The count is the server's: a new server counts again.
- **Clients:** they see `gone`, hold the object again, and get a snapshot.

## 9. Mode 1 relationship

**Decided:**
- RPC mode keeps upstream pi's record format and its abort rule (§3.3), and its goldens (`crates/noel-cli/tests/rpc_parity.rs`) guard both.
- §4.2 applies to every mode, so RPC mode saves partial output and aborted replies too. Upstream pi also keeps aborted replies. **Built.** Keeping them moved `get_state.messageCount` to upstream's after an abort (the `abort-with-queue` golden now differs from upstream's only by the system message).

**Built:** one session controller (`crates/noel-core/src/controller.rs`) drives runs. It owns, for one open session, the active run with its cancellation and retry backoff, the queue, submissions (§3.1), withdraw (§3.2), abort with a policy per caller (§3.3), resuming an interrupted run, compaction, and changes to the model and thinking level, which wait for no run to be active. It reports what happens as events, in order. RPC mode (`crates/noel-cli/src/rpc.rs`) is a thin adapter over it: commands in, records out, with abort keeping the queue; the Mode 2 worker (`crates/noel-cli/src/server/worker.rs`) is the second caller, with abort withdrawing the queue. The TUI on a local session still drives its own loop; attached to a server session (§7.4) it goes through the worker's controller.

## 10. Workspaces

**Decided (2026-10-14):** workspaces match upstream.
- **The interface** mirrors Durable's `ExecutionEnv`, a file system plus a shell (`durable/src/env/index.ts:208-373`). Operations return results and never throw. A string command runs through the shell, and an argument list runs without one. Abort or timeout kills only that command's processes.
- **Choosing a workspace:** the run engine asks a hook for each tool call, as Durable's harness `env(target)` does (`durable/src/harness/types.ts:569-582`).
- **`LocalEnv`** is first.
- **The remote adapter** speaks `pi-env`'s framed protocol version 1 (`env/docs/protocol.md`).
- **A Codewire adapter** is built on the MIT Rust SDK. It is optional.

**Built** in `crates/noel-env`, matching upstream's `durable/src/env` at the pinned commit (`CHORD_REFERENCE_REF` in the Makefile):
- **the interface:** `ExecutionEnv`, a `FileSystem` plus a `Shell` with upstream's methods, results and error codes (`FileErrorCode`, `ExecutionErrorCode`). It is async, object-safe (`Arc<dyn ExecutionEnv>`), and every operation takes a `Context` whose abort signal it checks;
- **`LocalEnv`** (id `local`), which behaves as `NodeExecutionEnv` (`env/docs/semantics.md`). A string command runs through the shell and an argument list without one, each in a new session with stdin from `/dev/null`; abort, timeout, or dropping the call kills only that command's process group. It is checked against `tests/fixtures/ts-parity/local-env.json`, the observations of upstream's env conformance cases (`durable/src/testing/env-conformance.ts`) on `NodeExecutionEnv`. `watch` and `listFiles` fail with `not_supported` for now;
- **the tools:** `read`, `write`, `edit`, `ls` and `bash` reach files and run commands only through the workspace, with their limits, results and error texts unchanged; `edit` and `write` of one file run one at a time per file system id and canonical path (`durable/src/tools/file-mutation-queue.ts`). `grep` and `find` search with ripgrep's crates: here by walking the tree, and in a workspace on another machine over what its `list_files` lists (the daemon walks with the same `ignore` crate) and, for `grep`, what it reads;
- **the hook:** `Runtime` asks its `EnvProvider` for the workspace of each tool call with `EnvTarget {session_id, cwd}`, as Durable's harness `env(target)` does; a failure is that call's error result. The default is a `LocalEnv` at the session's working directory, so every mode behaves as before.

Where these differ from upstream is listed in `docs/rust-rewrite/non-parity-register.md` ("Workspaces").

**Built:** a session's workspace is saved as a journal record `Workspace {adapter, …}`; when there is none, the workspace is local (below, "Remote workspaces").

**Decided (2026-10-14):** noel has its own `noel-env`, a Rust port of upstream's whole `pi-env` package (`env/`), which upstream writes partly in TypeScript and partly in Rust.
- **The client:** the framed-protocol connection; a remote `ExecutionEnv` whose results match the local one (`env/docs/semantics.md`); and SSH deployment that accepts the host key once, names the daemon by its SHA-256 and verifies it before each start (`env/README.md`).
- **The daemon:** `noel-env serve`, speaking `pi-env` protocol version 1 unchanged (`env/docs/protocol.md`), so either side can talk to upstream's.
- **Proof:** goldens recorded from upstream's `RemoteExecutionEnv` and daemon at a pinned commit, plus interop tests that connect noel's client to upstream's daemon and upstream's client to noel's daemon.
- **The Codewire adapter** starts `noel-env serve` inside a Sandbox through Codewire's exec API. There is one remote-workspace protocol; SSH, Codewire or another VM system only changes how the daemon is started.

**Built:** the daemon, `noel-env serve --token <hex>` (`crates/noel-env-daemon`, binary `noel-env`), a port of upstream's `env/daemon` at the pinned commit:
- **the wire** is `pi-env` protocol 1 unchanged (`env/docs/protocol.md`): the `PI-ENV <token>` sync line, the frames, every operation (file operations and handles, `listFiles`, `exec` with output windows and spill files, `pty` with `ptyWrite`, `ptyResize` and `ptyForeground`, `watch`), cancellation, the scheduling of replies before bulk output, and liveness (a ping every 5 seconds; everything it started is killed and it exits after 30 seconds of silence or when stdin ends). `hello` reports noel's version, as `noel-env --version` does. The frame codec is `noel_env::frame`, for the client to share;
- **its behaviour** is upstream's daemon's (`env/docs/semantics.md`): Node's file rules, each command in a new session killed as a group, output decoded per stream, Durable's `NodeFileWatcher` running next to the files;
- **platforms:** Linux and macOS. Upstream's Windows and Android paths are not ported;
- **size:** `make env-daemon` builds it as upstream builds `pi-env` (LTO, one codegen unit, no symbols) in its own profile, so the other binaries keep theirs. Built static for Linux x86-64 (musl), as upstream ships its Linux daemons, it is 2.2 MB, against 2.4 MB for upstream's;
- **proof:** `tests/fixtures/ts-parity/pi-env.json` records upstream's `Connection`, `RemoteExecutionEnv` and `openTerminal` driving upstream's daemon through a tap (`make ts-parity-pi-env`); `crates/noel-env-daemon/tests/pi_env_golden.rs` sends the recorded client frames to `noel-env serve` and compares its answers. `make env-interop` runs upstream's client tests against it: Durable's env conformance suite over `RemoteExecutionEnv` and `LocalExecutionEnv`, and pi-env's `remote`, `differential` and `terminal` tests, 78 of 78 passing, as against upstream's daemon.

Where it differs from upstream's daemon is listed in `docs/rust-rewrite/non-parity-register.md` ("noel-env Daemon").

**Built:** the client and SSH deployment, in `crates/noel-env`, ports of upstream's `env/src` at the pinned commit:
- **the connection** (`Connection`, `connection.ts`) starts any daemon command with `serve --token <hex>`: a local process, `ssh … noel-env serve`, later Codewire's exec. It discards what a shell prints before the `PI-ENV <token>` line, multiplexes requests by id with results, errors, events, `cancel` and pings, and splits large transfers as upstream does (512 KiB writes, 256 or 512 KiB reads, several in flight), well under the 16 MiB frame limit. It starts lazily, and a command computed at each start may detect and deploy first. A lost daemon (exit, 30 seconds of silence, a corrupt frame) fails its requests in flight with `lost`, requests on its handles fail instead of reaching the next daemon, and the next request starts a new one;
- **`RemoteEnv`** (`remote-env.ts`, `watch.ts`, `list-files.ts`) implements `ExecutionEnv` over a connection, with `NodeExecutionEnv`'s results on the remote machine (`env/docs/semantics.md`): the client applies Node's path, error, read-size and abort rules, and the daemon makes the system calls. `watch` runs in the daemon and reopens after a lost connection with an overflow; `list_files` lists in the daemon. Upstream's terminals (`terminal.ts`) are not ported, as `ExecutionEnv` has none;
- **SSH deployment** (`ssh.ts`) for Linux and macOS remotes: `ssh` with `BatchMode`, strict host-key checking against the application's own known-hosts file under a fixed alias and no forwarding; host keys scanned (with `ssh-keygen -lf` fingerprints) and accepted once, a changed key refused; the remote system detected with `uname`; the daemon `noel-env-<platform>-<arch>` taken from a directory (an option, else `NOEL_ENV_DAEMON_DIR`, else beside the running executable), uploaded to a temporary file, renamed to `~/.cache/noel-env/noel-env-<SHA-256 prefix>` only once its SHA-256 matches, verified before every start, and daemons of other contents removed. Upstream's Windows and Android (Termux) paths are not ported;
- **proof:** the conformance cases `LocalEnv` runs (`local-env.json`) make the same observations over `RemoteEnv` and `noel-env serve`, which also passes upstream's watch and `listFiles` cases (`crates/noel-env-daemon/tests/remote_env_conformance.rs`); the client sends exactly the frames upstream's client sent in `pi-env.json` (`pi_env_client_golden.rs`); every `ssh` invocation equals upstream's through the same stand-in for `ssh`, except for the daemon's names (`pi-env-ssh.json`, `ssh_deploy.rs`), and upstream's SSH and remote tests are ported; `make env-interop` runs these client tests against upstream's `pi-env` too.

Where the client differs from upstream is listed in `docs/rust-rewrite/non-parity-register.md` ("noel-env Client").

**Built:** remote workspaces over `ssh` (`crates/noel-cli/src/workspace.rs`, `crates/noel-core/src/workspace.rs`):
- **Choosing one.** `--workspace ssh://[user@]host[:port]/absolute/path` starts a session (the TUI, `-p` and `--mode rpc`) whose tools run there; Mode 2's `noel.sessions.create` takes the same URL as `workspace` (§7.2), and `noel client new --workspace` passes it. The path is percent-decoded and normalized. `?login=1` starts the daemon through the remote login shell (`$SHELL -lc`); without it, as upstream, the daemon runs from the remote `ssh` command, with what that environment has. The option is part of the URL, so it is saved with the workspace and a reopened session starts the daemon the same way.
- **The journal record** is `{"type":"workspace","adapter":"ssh","id","cwd","host","user"?,"port"?,"login"?}`. A new session in a workspace is written as one atomic file holding `started` then `workspace`, so it never exists without the record; a local session is written as before, one `started` record, and older journals have no record and are local. A fork keeps the record, compaction's rewrite keeps it, an import (an export carries none) is local. `id` is the file namespace, `ssh://host[:port]`: every path and user on one machine see the same files, so `edit` and `write` of one file wait for each other across sessions there. Unknown adapter fields are kept as saved. Older versions ignore the record (`#[serde(other)]`) and so would run such a session's tools here.
- **Reopening** (`-c`, `-r`, `--session`, `--session-id`, a Mode 2 worker, `/resume`) uses the saved workspace; `--workspace` naming another for an existing session, or any for a local one, is an error. `/new` and RPC's `new_session` keep the workspace, as they keep the model.
- **Never here by accident.** The run engine opens a session's workspace through `EnvProvider::Workspaces`, which gives a session without a record a `LocalEnv` exactly as before; a session with a record that no provider can open, or with an adapter this version lacks, fails every tool call rather than running it here.
- **Trust** is never automatic. `noel workspace trust ssh://[user@]host[:port][/path]` connects once with `accept-new` against a temporary file (`scan_host_key`), prints the `ssh-keygen -lf` fingerprints, and on a terminal asks `y/N`; without a terminal it needs `--fingerprint SHA256:…` and trusts only the key with that fingerprint. The key is stored in noel's own known-hosts file, `<agent dir>/workspaces/known_hosts` (`~/.noel` by default), under the alias `noel-env-<host>`, or `noel-env-[<host>]:<port>` with a port; a changed key is refused until `noel workspace forget ssh://host[:port]` removes the old one. `~/.ssh` is never written. Starting a session, or `create`, on a host whose alias has no key fails with a message naming the trust command; nothing prompts mid-session, and `ssh` checks the key strictly at every connection.
- **Connecting.** The connection is lazy (`ssh_connection`): the first tool call, or the first run's context files, start it, so an unreachable host fails that call with the reason, not the session. Every session on one workspace URL in a process shares one connection. A lost connection (the `ssh` process gone, 30 s of silence) fails the call in flight with `pi-env connection lost`, and the next call starts a new daemon; the daemon kills what it ran when its stdin ends.
- **What comes from the workspace:** the tools' files and commands; `!` and `/bash` in the TUI; the system prompt's `cwd` (the workspace's directory); and the project context files, read through the workspace from its directory up to `/` as `noel_config` reads local ones, once per process and again after `/reload` or a session switch. A run that cannot read them (the host unreachable) waits up to 10 s and goes on without them, trying again at the next run. The user's own context file from the agent dir still applies. Never from the workspace: settings, `.noel/`, extensions, skills, prompt templates and themes, which would configure or run noel here; extension tools run here, and skills are left out of the system prompt, whose `read` could not reach their files.
- **The tools** in a remote workspace: `grep` and `find` list with the daemon's `listFiles` (hidden entries, git's ignore rules inside a git work tree, never `.git`) and `grep` reads 16 files at a time through the workspace; `bash` keeps its complete output in the workspace's temporary directory (exec's spill, as Durable's bash tool) so the path it names can be read there; the commands get noel's process markers (`AI_AGENT`, `NOEL_CODING_AGENT`, `PI_CODING_AGENT`).
- **The TUI footer** shows the workspace's URL and whether it is connected, connecting, lost or not connected yet, in place of the directory.
- **The daemons** are built by `make env-daemons` (static musl `noel-env-linux-x64`, and `noel-env-linux-arm64` with an aarch64 linker) and installed beside `noel` by `make install`; a remote without its daemon (macOS, for now) fails its tool calls naming the file it needs.
- **Proof:** `crates/noel-env-daemon/tests/tools_over_remote_env.rs` runs every built-in tool through `RemoteEnv` and `noel-env serve` and compares with `LocalEnv`; `make workspace-journey` (`scripts/e2e-workspace-ssh.sh`, not part of `make e2e`) runs a static `noel` against `sshd` in an Alpine container pinned by digest, with no network, and asserts trust, print-mode tools in the workspace, `grep` and `find`, a killed `ssh` mid-command and the new daemon after it, an untrusted host, `-c`, Mode 2's `create` and `send`, a local session, and the TUI's footer.

Where remote workspaces differ from upstream and from a local workspace is listed in `docs/rust-rewrite/non-parity-register.md` ("Remote Workspaces").

## 11. Compatibility and goldens

**Decided:** compatibility is proved by goldens recorded from upstream, as the RPC goldens are.
- **No server or client to record.** Upstream publishes no `pi server` or `pi client`, so the Chord goldens drive upstream's public `packages/chord` with noel's contracts, covering delta and canonical-JSON vectors and the link frames.
- **Interop:** a test in Docker connects an upstream Chord client to a noel server, and the reverse (`make chord-interop`). **Built**, including upstream's client holding `noel.chat` on `noel server` through its relay to a worker, following the view's snapshot and ops.
- **pi-env goldens** come from upstream's `RemoteExecutionEnv` talking to its own daemon (`pi-env.json`), and from upstream's SSH deployment through a stand-in for `ssh` (`pi-env-ssh.json`). **Built** for the daemon and the client, with interop both ways (`make env-interop`): upstream's client against `noel-env serve`, and noel's client against upstream's `pi-env`.
- **`LocalEnv`** is checked against the conformance results of upstream's `NodeExecutionEnv` (**Built**: `local-env.json`).

## 12. Deviations from upstream

Every place noel differs from upstream is listed in
`docs/rust-rewrite/non-parity-register.md` with its reason. Building §4.2
removed one entry: aborted replies not being saved (upstream pi keeps them,
leaving them out of model requests), and added "Durable Partial Output" for
where noel's saving differs from Durable's. Building Mode 2's server, worker
and client added "Mode 2 Server, Worker and Client": noel's own objects, the
view's subset of Durable's documents, and workers that outlive their server.
The attached TUI added "The TUI Attached to a Server Session": where it
differs from the TUI on a local session. Remote workspaces added "Remote
Workspaces": noel's selection and trust, and where tools there differ from
local ones.

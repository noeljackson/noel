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
   - A long-running `noel server` holds sessions, and clients attach to it from anywhere (§5–§9).
   - A session's tools run in a workspace chosen through an adapter: local, a remote machine through `pi-env`, a Codewire environment, or anything else that implements the workspace interface (§10).

noel works without Codewire in both modes. Every Codewire path is optional.

Both modes use one run engine (`noel-core`), so the rules in §2–§4 hold for
the TUI, print mode, RPC mode and the server alike.

## 2. Sessions and the journal

**Built.**
- **The journal.** A session is a JSONL journal, `<session-id>.jsonl`. Each record is written with its newline in one write, and a torn final line is truncated when a writer opens the file. Listing skips files it cannot read.
- **The lock.** One process writes a session at a time. It holds `File::try_lock` on `<id>.jsonl.lock`; the lock is on that file because compaction replaces the journal itself. A second writer waits up to 5 s, then exits with code 75. Listing, forking and import read without the lock.
- **Runs.** Each run's start (`runId`, `requestId`) is saved with its prompt and fsynced before the prompt is acknowledged. Its end (completed, failed or aborted) is saved before the run is reported settled. A run with a start and no end is *interrupted*.
- **The queue.** Queued steers and follow-ups are saved with the session, and each enqueue is saved before it is acknowledged.

**Decided:** durable partial output (§4.2).

## 3. Submissions, the queue and abort

### 3.1 Submissions

**Built:** a prompt carries an optional `requestId`. A prompt that repeats a `requestId` gets the run the first one started, and nothing is sent again.

**Decided (follows Durable, `durable/docs/spec.md:2329-2360`):**
- **Every submission can carry a `requestId`:** a prompt, a steer or a follow-up. A repeat returns the first submission and writes nothing. A `requestId` reused for a different kind of submission is refused. Deduplication happens within one session, before any write.
- **When the session is busy, a submission says what to do:**
  - `reject` refuses it, and writes nothing;
  - `steer` queues it for the next boundary inside the run, after the current tool round;
  - `followUp` queues it for when the run would answer.
- **Each queued submission has its own id.** A submission ends either *done*, when it was placed into a run, or *unanswered*, with a reason.

### 3.2 Withdraw

**Decided (Durable `spec.md:2355-2357`).** `withdraw(submissionId)` removes a submission that is still queued, and reports it unanswered with reason `aborted`. For an input already placed into a run it reports `already_placed`; for a submission that has ended it reports `settled`.

### 3.3 Abort

**Decided (2026-10-14).** Each mode follows the upstream it mirrors:
- **Mode 2** (server, worker, client) follows Durable (`spec.md:2357-2358`): aborting a run also withdraws every queued steer and follow-up of that session. Each is reported unanswered with reason `aborted`, and the next prompt starts with an empty queue.
- **RPC mode, the TUI and print mode** follow published upstream pi: abort stops the run and keeps the queue. Upstream's `Agent.abort()` only cancels the active run, and its queues are cleared only by `reset()` (`agent/src/agent.ts:341-367`). The RPC goldens guard this.

**Built, kept:**
- An abort may name `expectedRunId`, and then changes nothing unless that run is the active one.
- An abort with no run active ends an interrupted run as aborted.

## 4. Runs, partial output and recovery

### 4.1 Recovery

**Built.**
- A supervisor decides when an interrupted run resumes: the Mode 1 driver, or the Mode 2 server (§8.3). noel never resumes in RPC mode on its own.
- The TUI continues an unfinished session when you reopen it.
- **What runs again on resume:**
  - a model request that a crash cut off is sent again;
  - replay-safe tools (`read`, `grep`, `find`, `ls`) run again;
  - any other interrupted tool call gets an "interrupted" result, and the model is told so.

### 4.2 Partial output is durable

**Decided (2026-10-14, follows Durable `spec.md:67`, `README.md:406`, `spec.md:3738, 3999-4006, 4078, 4118`).**
- **What is saved while a call runs:** a streaming reply's text and thinking so far, and a running tool's output so far. Each is committed to the journal at most every 100 ms, under the setting `progress.partialIntervalMs` / `progress.outputIntervalMs`, default 100. A crash loses at most the last interval.
- **An interrupted reply:** on recovery, its committed partial becomes an *aborted* assistant entry in the transcript before the request is sent again. Any of a run's failure or fault endings do the same.
- **An interrupted tool call:** its result is the output saved so far, with an "interrupted" error, so the model sees how far it got.
- **An aborted reply** is kept in the transcript and left out of later model requests. That matches upstream pi, which keeps it too, and noel's matching non-parity entry goes.
- **What clients see:** a client that joins late or reconnects sees the committed partial, as part of the current view (§7.3).

Journal shape **(Proposed):**
- A `Partial {runId, attempt, text, thinking}` record, and a `ToolOutput {runId, callId, output, truncated}` record, each superseding the last one for the same key.
- The terminal message or result supersedes both.
- Compaction drops superseded partials.

## 5. Server, worker and client roles (Mode 2)

**Decided (2026-10-14).** noel follows upstream's current direction: Chord's host runtime (`chord/docs/guide.md:657-692`). Upstream removed its experimental `pi server` and `pi client` in 1.2.0 because the runtime they served was replaced (`chord/CHANGELOG.md:53`).
- **Server:** one host per user. It owns principals, routes and the workers' lifecycle.
- **Worker:** one host per session, named `session:<id>`. It is the session's only writer: it holds the journal lock (§2) and runs the run engine.
- **Client:** the TUI, and a line-mode `noel client`. A client holds remote objects through the server, which relays between clients and workers.

**Proposed:**
- **The server:**
  - `noel server` takes `server.lock`, and a second server exits with code 75;
  - it binds `server.sock` in `$XDG_RUNTIME_DIR/noel` (or `/tmp/noel-<uid>`), a directory it requires to be owned by the user with mode 0700.
- **Workers:**
  - the server starts each worker as `noel worker --session <id> --generation <g>` in its own process group;
  - the worker opens the journal under its lock, binds `workers/<id>.sock`, then reports `ready`;
  - a worker with no holds, no run and no queued input for 60 s shuts down;
  - a worker outlives its server: it finishes its run, then waits up to that same idle time for a new server to adopt it;
  - a new server adopts surviving workers of the same build and shuts down the others once their run settles.
- **Clients:** a client starts the server when none is running.

## 6. Principals and access

**Decided (2026-10-14), following Chord's model (`chord/docs/guide.md:315-334`, `spec.md:230, 275`).**
- **Owner:** `{kind:"owner", via:"device"|"tunnel"|"relay"}`.
- **Guest:** `{kind:"guest", id, name, session, mode:"view"|"full", revision}`. A guest is invited to one session and can be revoked.
- **Worker:** `{kind:"worker", session, generation}`.
- **Stamping:** the side that accepts a connection stamps its principal. A principal forwarded from another host is honoured only from the parent link, and principal fields inside payloads are ignored.
- **Routing:** a route's `authorize(principal, session)` runs before a worker is resolved or spawned, and again after.

**Build order (Decided):**
1. **Device.** The owner-only local socket, with no token. The server checks the peer's uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and closes any other uid before reading a byte. Any process running as the owner can connect, including the agent's own `bash`; that grants nothing it could not already do.
2. **Relay,** after a design pass of its own.
3. **Remote authentication,** offering both device-code pairing (the server issues and verifies its own token) and an optional Codewire login. A Codewire tailnet connection may be stamped `via:"tunnel"`, because admission to an environment's tailnet already grants shell access there.
4. **Guests.**

## 7. Wire protocol and objects

### 7.1 Link protocol

**Decided:** Chord link protocol version 2 (`chord/docs/spec.md` §9; frames in `chord/src/wire.ts`).

**Proposed:**
- **Framing:**
  - WebSocket framing over the local socket, Chord's own WebSocket framing unchanged; Chord has no Unix-socket transport;
  - NDJSON `{"chord2": frame}` lines between server and worker, Chord's IPC encoding.
- **The subset noel implements:**
  - the frames `hello`, `hold`/`shape`/`gone`/`release`, `call`/`result`/`cancel`, `sub`/`state`/`unsub`, `ping`/`pong`, `ack` and `bye`;
  - delta ops and their wire compression;
  - contracts and their fingerprints (`hash64` of canonical JSON);
  - principal stamping and relay;
  - flow control by window, including the reset when a waiting subscription outgrows four windows.
- **Left out:** hot reload and plugins. noel sends no reload control, and a worker that receives it answers `unavailable`.

**Decided (2026-10-14):**
- noel builds on protocol 2 now, although it is under `[Unreleased]` in Chord's changelog.
- The goldens are pinned to an exact upstream commit, and `ts-parity-drift` flags drift.
- Until Chord releases protocol 2, the server offers only device access.

### 7.2 Objects

**Proposed.** Each object has contract version 1, with JSON Schemas checked into the repository.

```text
noel.sessions  at "server", input {}           owner only
  state directory: {sessions: [{sessionId, name?, cwd, modified, messageCount,
                                live: {worker, busy, viewers}, lastRun?}]}
  create({requestId, cwd, name?, model?, thinkingLevel?}) -> {sessionId}
  fork({requestId, sessionId, atMessage?}) -> {sessionId}

noel.chat      at "session:<id>", input {session}
  viewer:      state view
  participant: state view
    submit({requestId, text, images?, whenBusy: "reject"|"steer"|"followUp"})
        -> {submissionId, runId?, disposition: "started"|"queued"|"repeated"}
    withdraw({submissionId}) -> {outcome: "withdrawn"|"already_placed"|"settled"}
    abort({expectedRunId?}) -> {}
    compact({requestId, instructions?}) -> {runId}
    configure({model?, thinkingLevel?}) -> {}
    resume({expectedRunId}) -> {runId}

noel.models    at "server", input {}           owner only
  state catalog: {models: [{provider, modelId, name, reasoning, thinkingLevels}]}
```

The owner, and a guest with `mode:"full"`, get `participant`; a guest with
`mode:"view"` gets `viewer`. Methods check their arguments: for example, that
a submission belongs to this session.

### 7.3 The view and late join

**Decided:** late join follows Durable. A client that joins late or reconnects starts from the current view, and nothing is replayed (`durable/README.md:404`). Subscribing sends the current snapshot first, then deltas; a reconnect is a new hold and a new snapshot.

**Proposed:** the view follows Durable's shape `{conversation, entries, docs}` (`durable/docs/spec.md:4542-4558`):
- **`entries`:** messages in upstream pi's `Message` JSON;
- **`docs["pi.agent"]`:** the model, thinking level and working directory;
- **`docs["pi.live"]`:** the running generation, including its committed partial (§4.2), its tool calls with their output so far, and any compaction in progress;
- **`docs["pi.inbox"]`:** the queued submissions;
- **`docs["pi.usage"]`**;
- **`lastRun`**.

## 8. Failure handling (Mode 2)

### 8.1 Client
- **A disconnect:** the server releases everything the client held. The run continues.
- **A submission's outcome is unknown when the link drops.** The client retries with the same `requestId`, which reports either `repeated` or the outcome.

### 8.2 Server
- **Losing the server stops no run:** a worker finishes its run while no server is attached.
- **A new server adopts the workers** (§5). Clients reconnect, hold their objects again, and get a fresh snapshot.

### 8.3 Worker
- **A worker crash:** the kernel releases its lock, and the run is interrupted.
- **Restart:** the server starts it again with the next generation. The new worker repairs a torn tail, applies §4.2, and resumes the run.
- **Resume limit (Proposed):** the server resumes a run by itself at most 3 times. After that the run stays interrupted until a client resumes or aborts it.
- **Clients:** they see `gone`, hold the object again, and get a snapshot.

## 9. Mode 1 relationship

**Decided:**
- RPC mode keeps upstream pi's record format and its abort rule (§3.3), and its goldens (`crates/noel-cli/tests/rpc_parity.rs`) guard both.
- §4.2 applies to every mode, so RPC mode saves partial output and aborted replies too. Upstream pi also keeps aborted replies.

**Proposed:** one session controller, extracted from `crates/noel-cli/src/rpc.rs`, drives runs for RPC mode and for the worker.

## 10. Workspaces

**Decided (2026-10-14):** workspaces match upstream.
- **The interface** mirrors Durable's `ExecutionEnv`, a file system plus a shell (`durable/src/env/index.ts:208-373`). Operations return results and never throw. A string command runs through the shell, and an argument list runs without one. Abort or timeout kills only that command's processes.
- **Choosing a workspace:** the run engine asks a hook for each tool call, as Durable's harness `env(target)` does (`durable/src/harness/types.ts:569-582`).
- **`LocalEnv`** is first.
- **The remote adapter** speaks `pi-env`'s framed protocol version 1 (`env/docs/protocol.md`).
- **A Codewire adapter** is built on the MIT Rust SDK. It is optional.

**Proposed:** a session's workspace is saved as a journal record `Workspace {adapter, …}`; when there is none, the workspace is local.

**Decided (2026-10-14):** noel has its own `noel-env`, a Rust port of upstream's whole `pi-env` package (`env/`), which upstream writes partly in TypeScript and partly in Rust.
- **The client:** the framed-protocol connection; a remote `ExecutionEnv` whose results match the local one (`env/docs/semantics.md`); and SSH deployment that accepts the host key once, names the daemon by its SHA-256 and verifies it before each start (`env/README.md`).
- **The daemon:** `noel-env serve`, speaking `pi-env` protocol version 1 unchanged (`env/docs/protocol.md`), so either side can talk to upstream's.
- **Proof:** goldens recorded from upstream's `RemoteExecutionEnv` and daemon at a pinned commit, plus interop tests that connect noel's client to upstream's daemon and upstream's client to noel's daemon.
- **The Codewire adapter** starts `noel-env serve` inside a Sandbox through Codewire's exec API. There is one remote-workspace protocol; SSH, Codewire or another VM system only changes how the daemon is started.

## 11. Compatibility and goldens

**Decided:** compatibility is proved by goldens recorded from upstream, as the RPC goldens are.
- **No server or client to record.** Upstream publishes no `pi server` or `pi client`, so the Chord goldens drive upstream's public `packages/chord` with noel's contracts, covering delta and canonical-JSON vectors and the link frames.
- **Interop:** a test in Docker connects an upstream Chord client to a noel server, and the reverse.
- **pi-env goldens** come from upstream's `RemoteExecutionEnv` talking to its own daemon.
- **`LocalEnv`** is checked against the conformance results of upstream's `NodeExecutionEnv`.

## 12. Deviations from upstream

Every place noel differs from upstream is listed in
`docs/rust-rewrite/non-parity-register.md` with its reason. Building §4.2
removes one entry: aborted replies not being saved. Upstream pi keeps them,
leaving them out of model requests.

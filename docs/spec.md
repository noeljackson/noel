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
- **Storage (built).** The store writes through `noel_core::storage::JournalStorage` (append a record, read, rewrite atomically): the journal file above (`JsonlFile`, under the lock below) by default, and `MemoryJournal` for tests and embedders. The records and their recovery are the same on any storage; listing, forking and import read journal files.
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
- **`whenIdle: "reject"`** (Durable `spec.md:2924-2932`): while no run is active the input is refused (`idle`) and nothing is written, so it never starts a run of its own; with `whenBusy: "steer"` it steers the current run or is refused, in one step.
- **A continue** (`type: "continue"`, Durable `spec.md:2958-2977`) starts a run on the context as it is, without a user message, for example after a run that ended without an answer. Only an idle session admits it; a busy one refuses it `busy` before any write. Queued follow-ups are placed into its run first. A context that does not end in a user message or tool results (system messages aside) ends the run without a request, failed with reason `nothing_to_answer`; a reply whose tool calls never ran is continued, its calls settled as on a resume. Its start is saved as a `run_started` record (no message), and its `requestId` deduplicates as a prompt's does.
- **How a run ends without an answer** is saved with its end (`run_finished.failure`: Durable's `runFailure` reason and detail) and shown in the view's `pi.live.runFailure` until another run starts: `model_error` (the request failed, retries included), `interrupted`, `restart_limit`, `nothing_to_answer`, `turn_limit` (its tool rounds reached `agent.maxToolRounds`; a continue goes on). An aborted run is `aborted`.

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
- **Close and crash** (Durable `spec.md:2525-2560`): a stop that closes a run (its driver's inbox ends: the worker shutting down, RPC mode exiting) saves `run_closed`; a resume after any other stop is a crash restart, saved as `run_restarted`. A message the run saves is progress and clears its restarts. At `restartLimit` (setting, default 5) restarts in a row the run ends at its resume instead of running again, failed with reason `restart_limit`.
- **What runs again on resume:**
  - a model request a close stopped is sent again, after what its reply had streamed is saved as an aborted reply (§4.2);
  - a model request a crash cut off is a failed attempt (Durable `spec.md:4884-4901`): when the retry policy (`retry`) allows another attempt the run waits out its backoff, reported as a retry with the text "The host stopped while the model was answering, on attempt N", and sends it again; otherwise the run ends failed with reason `interrupted` and that text. A crash while the turn waited out a retry's backoff or compacted after an overflow (a `retry_wait` record is its last) cut off no request and is not charged;
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
- **Worker:** one host per session, at the address `Session({id})`. It is the session's only writer: it holds the journal lock (§2) and runs the run engine.
- **Client:** the TUI, and a line-mode `noel client`. A client holds remote objects through the server, which relays between clients and workers.

**Built** (`crates/noel-cli/src/server`):
- **The runtime directory** is `$XDG_RUNTIME_DIR/noel`, else `/tmp/noel-<uid>` (`NOEL_RUNTIME_DIR` overrides both). It must be a directory, not a link, owned by the user with permissions 0700; it is made so when missing (clearing a setgid bit its parent passes on), a setgid or sticky bit on an existing one is allowed, and anything else is refused.
- **The server:**
  - `noel server --foreground` takes `server.lock` there (`File::try_lock`); a second server exits with code 75. Without `--foreground`, `noel server` starts a detached server (output in `server.log`) and returns once it listens, or exits 75 if one already runs;
  - it binds `server.sock` (0600), removing a stale one only when it is a socket the user owns, and writes `server.json` (`pid`, `build`, `startedAt`);
  - it reads the global settings only, from the home directory, and gives its session directory to its workers;
  - with no client link and no worker for `server.idleExitMs` (default 600000, ten minutes) it exits, removing its socket.
- **Workers:**
  - the server starts each as `noel worker --session <id> --generation <g> [--resume <runId>]` in its own session (`setsid`), with its output in `workers/<id>.log`;
  - the worker opens the journal under its lock (`fatal`, reason `locked`, exit 75 if another process holds it), binds `workers/<id>.sock` (0600), writes `workers/<id>.json` (`pid`, `session`, `generation`, `build`), then prints the control line `ready` on stdout;
  - the server dials the socket, checks the peer's uid and pid, reads the `ready` line again, and runs the Chord link as the worker's parent, over framed byte streams (`[u32 BE length][tag][payload]`: tag 0 a Chord message, tag 1 a JSON control message): `ready` (`pid`, `build`, `session`, `generation`), `status` (`viewers`, `running`, `queued`, `runId`) and `shutdown`. On the server the link is the `session` route's link for that address;
  - a worker with no holds, no run and no queued input for `server.workerIdleExitMs` (default 60000) exits, removing its socket and record;
  - a worker outlives its server: it finishes its run, then waits up to that same idle time for a new server to adopt it;
  - a new server reads `workers/*.json`, dials each socket and checks uid, pid and `build`. A match is adopted; another build is told `shutdown` and exits once its run settles; a record whose process is gone is removed.
- **Clients:** a client starts the server when none listens, after checking the runtime directory and the listening process's uid. The clients are the line-mode `noel client` and the TUI, `noel --attach <session-id>` (§7.4).
- **The bridge** (`noel bridge`, `bridge.rs`) is how a client reaches the server on another machine (§6, tunnel). It dials `server.sock` as a client does, with the same checks, starting the server when none listens. It reads the client's WebSocket upgrade request from stdin (at most 16 KiB), drops every `Noel-Via` header in it and adds `Noel-Via: tunnel`, then copies bytes between stdin and stdout and the socket, and exits when either side closes. Its own notes (starting a server) go to stderr.

## 6. Principals and access

**Decided (2026-10-14), following Chord's model (`chord/docs/guide.md:315-334`, `spec.md:230, 275`).**
- **Owner:** `{kind:"owner", via:"device"|"tunnel"|"relay"}`.
- **Guest:** `{kind:"guest", id, name, session, mode:"view"|"full", revision}`. A guest is invited to one session and can be revoked.
- **Worker:** `{kind:"worker", session, generation}`.
- **Stamping:** the side that accepts a connection stamps its principal. A principal forwarded from another host is honoured only from the parent link, and principal fields inside payloads are ignored.
- **Routing:** a route's `authorize(principal, session)` runs before a worker is resolved or spawned, and again after.

**Build order (Decided; the ssh tunnel was added first on 2026-10-15):**
1. **Device** (**Built**). The owner-only local socket, with no token. The server checks the peer's uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and closes any other uid before reading a byte. Any process running as the owner can connect, including the agent's own `bash`; that grants nothing it could not already do. Accepted links are stamped `{kind:"owner", via:"device"}`; the `session` route lets the owner reach any session and a guest only its own, and `noel.chat` gives the owner and a `full` guest `participant`, a `view` guest `viewer`. Workers act as `{kind:"worker", session, generation}`. Objects are kept by type, params and principal, each as canonical JSON.
2. **Tunnel over ssh** (**Built**). `--remote <ssh-destination>` on `noel client` and `noel --attach` runs `ssh -T -e none [-o BatchMode=yes] -- <destination> '<program>' bridge` and runs the same WebSocket link over its stdin and stdout. ssh authenticates with the user's own configuration, keys and known hosts; noel adds no token, crypto or service. The server accepts a link as `{kind:"owner", via:"tunnel"}` when its upgrade request carries exactly `Noel-Via: tunnel`, as `via:"device"` when it has none, and refuses any other `Noel-Via` (400). The bridge, being the user's own process on the server's machine, is what sets it: a device process could claim `tunnel` too, which only describes it as remote, but nothing on the other machine can claim `device`. The first connection may let ssh ask on the terminal; reconnections use `BatchMode=yes`. `new` with `--remote` needs `--cwd`, an absolute path there; `--remote-noel` names the remote program (default `noel` on its `PATH`), quoted for the remote shell except a leading `~/`.
3. **Relay,** after a design pass of its own.
4. **Remote authentication,** for clients that cannot run ssh, offering both device-code pairing (the server issues and verifies its own token) and an optional Codewire login. A Codewire tailnet connection may be stamped `via:"tunnel"`, because admission to an environment's tailnet already grants shell access there.
5. **Guests.**

## 7. Wire protocol and objects

### 7.1 Link protocol

**Decided (2026-11-04):** Chord v2 (upstream `packages/chord` at `CHORD2_REFERENCE_REF`, link `PROTOCOL_VERSION` 1). The audit and port plan are in `docs/chord-v2-port.md`; v2's wire is not stable upstream, so the pin moves only with the goldens.

**Built** in `crates/noel-chord/src/v2`, checked against `tests/fixtures/ts-parity/chord-v2-contracts.json` and `chord-v2-messages.json`:
- **contracts:** declarations (`contracts/v2/*.json` in noel-client) with their schema normal form and fingerprints (`hash64` over UTF-8); variants, state members and methods indexed by sorted names; params checked against their fields; host addresses `Server()` and `Session({id})`;
- **messages:** the 18 kinds in the binary codec, checked on receipt (unless the link is trusted) as upstream's message schema checks them; the closed error table and wire errors (`{kind: "chord", error}`, `{kind: "plugin", code, message, details?}`);
- **parts:** limits per part (16 MiB on the wire, 64 MiB decoded on untrusted links), deflate above 16 KiB at an origin that compresses, inflate within the decoded limit;
- **the channel:** `hello` with incarnations (uuidv7) and call windows, messages queued until the peer's `hello`, protocol errors, and the heartbeat (`ping` after 15 s of silence, close after 30 s);
- **hosts:** placement by address (here, a route of the address's kind, or the parent link); holds bound with a snapshot of each state member, then `state` ops through a window of 2 unacknowledged messages per origin, pending ops replaced by a value past 10,000; calls within 256 per side and the peer's window, `abort`; holds refused `failed {waiting}` (not served, another contract) or `failed {broken}` (params, address, factory), `loading` and `retry`; routes resolving one link per address, again when it drops; relayed holds keeping their ids under a relay origin, with acks returned to the link that sent each message and `closed` when the origin goes; revocation (`revoked_by`, `revoke`, `gone forbidden/revoked`);
- **transports:** WebSocket binary messages (a text message ends the link), also over a Unix socket; framed byte streams for server and worker (§5); an in-memory pair.

`make chord2-interop` (`scripts/chord2-interop.sh`, Bun against a checkout at the pin) runs both directions against upstream: upstream's client host holds `noel.chat` on `noel server` (relayed to the worker), submits, follows the run and holds again after a drop; noel's `chord2-client` holds an upstream server host's object, calls it, follows a burst of commits past the window and holds again after a drop. Where v2 differs from upstream is listed in `docs/rust-rewrite/non-parity-register.md` ("Chord v2").

### 7.2 Objects

**Built.** Each object has contract version 1; their declarations, with their JSON Schemas, are checked in as `crates/noel-client/contracts/v2/*.json`.

```text
noel.sessions  at Server(), params {}          owner only (variant owner)
  state directory: {sessions: [{sessionId, name?, labels?, cwd, workspace?,
                                modified, messageCount, firstPrompt?, model?,
                                live: {worker, busy, viewers},
                                lastRun?: {runId, status}}]}        newest first
  create({requestId, cwd, name?, model?, thinkingLevel?, workspace?})
      -> {sessionId, created}
  fork({requestId, sessionId, clone?}) -> {sessionId, created}
  rename({sessionId, name}) -> {sessionId}
  delete({sessionId}) -> {sessionId}
  import({requestId, path}) -> {sessionId, created}

noel.chat      at Session({id: session}), params {session}
  viewer:      state view
  participant: state view
    submit({requestId, text, images?, whenBusy?: "reject"|"steer"|"followUp"})
        -> {submissionId, runId?, disposition: "started"|"queued"|"repeated", status?}
    withdraw({submissionId}) -> {outcome: "withdrawn"|"already_placed"|"settled"}
    abort({expectedRunId?}) -> {runId?, withdrawn}
    compact({instructions?}) -> {omittedMessages, retainedMessages}
    configure({model?, thinkingLevel?}) -> {model?, thinkingLevel}
    resume({expectedRunId}) -> {runId}

noel.models    at Server(), params {}          owner only (variant owner)
  state catalog: {models: [{provider, modelId, name, reasoning, thinkingLevels,
                            available, scoped, thinkingLevel?}]}

noel.accounts  at Server(), params {}          owner only (variant owner)
  state accounts: {providers: [{provider, accounts: [{name, kind: "api_key"|"oauth",
                                                      limitedUntil?}], current?}],
                   logins: [{loginId, provider, account,
                             status: "waiting"|"done"|"failed", error?}]}
  login({provider, account}) -> {loginId, kind: "device"|"code"|"key", url?, userCode?}
  finishLogin({loginId, code?, key?}) -> {provider, account, replaced}
  logout({provider, account}) -> {removed}

noel.settings  at Server(), params {}          owner only (variant owner)
  state settings: {path, values}
  set({key, value}) -> {values}
```

The owner, and a guest with `mode:"full"`, get `participant`; a guest with
`mode:"view"` gets `viewer`. Methods check their arguments against their schemas (`invalid/args`).
noel's own refusals are plugin errors whose code names the reason (`busy`,
`session`, `cwd`, `workspace`, `model`, `thinkingLevel`, `configure`, ...):
v2's error table is closed.

- **`submit`** maps to the session controller (§3.1): `whenBusy` names the kind (`reject` a prompt, `steer`, `followUp`, default `reject`). While idle any submission starts a run. A repeated `requestId` answers `repeated` with the first submission's `status` (`queued`, `running`, `interrupted`, `completed`, `failed`, `aborted`, `withdrawn`) and its `runId`. A refusal writes nothing and is the plugin error `busy`, `queued_prompt`, `request_id_reused` or `refused`.
- **`abort`** follows Mode 2's rule (§3.3): it withdraws the queue, and answers once the run settled.
- **`compact`, `configure` and `resume`** need no run active (the plugin error `busy` otherwise).
- **`create` and `fork`** name the new session by their `requestId` (`s-` and 24 hex digits of its SHA-256), so a repeat returns the same session with `created: false`. A new session gets its model and thinking level as `noel` would start it; `create` lists it in the directory before answering. `create`'s `workspace` is a URL as `--workspace` takes it (§10): the session's tools run there, and a URL that is not valid, or names a host noel does not trust, is the plugin error `workspace` with a message naming the trust command. A fork keeps its source's workspace. The directory's `workspace` is the URL of a session's workspace, absent for a local one.
- **`rename` and `delete`** of a session no worker holds act on its journal. A session a worker holds is renamed through that worker (`/name`; a name is cleared only while no worker holds it). It is deleted once that worker ends: the server asks it to quit, which it does only while no client holds the session and no run is active (the plugin error `busy` otherwise), and then removes the journal. The server's own references to sessions have a null principal; its session route lets them reach any session, and a worker treats its parent's (`{kind: "server"}`) as a participant. **`fork`** with `clone` copies the session whole, keeping its parent. **`import`** reads an exported file or a journal on the server's machine (an absolute path) into a new session; a repeated `requestId` names the first.
- **`noel.models`** lists the models a session can use (`available`: with auth, or local) and the scoped ones (`enabledModels`, `--models`; `scoped`, with the scope's `thinkingLevel`), usable or not.
- **`noel.accounts`** lists the server machine's provider accounts, never their secrets, with the account failover chose (`current`) and rate limits (`limitedUntil`, epoch seconds). A login runs on the server and writes its auth file, so credentials never leave it; the client relays what the person does. `login` says what it needs: `device` (open `url`, enter `userCode`; the server polls), `code` (open `url`, then paste the code shown) or `key` (an API key, for the other providers). `finishLogin` hands over the code or the key and answers once the account is stored, or with the plugin error `login`. A session uses a new account at its next request; `/login` in the TUI (§7.4) runs through this object when the server is on another machine.
- **`noel.settings`** is the user settings file of the server's machine (`values`). `set` writes the value at `key`, a dotted path, and `null` removes it. A session's worker takes a change on `/reload`.
- **The directory** is read from the session directory (each journal again only when it changed) and the workers' `status`, on each change and every 5 s. Changes that arrive together are read together, so a listing taken the moment a `submit` is answered may not show it yet; the session's view (§7.3) is current.

### 7.3 The view and late join

**Decided:** late join follows Durable. A client that joins late or reconnects starts from the current view, and nothing is replayed (`durable/README.md:404`). Subscribing sends the current snapshot first, then deltas; a reconnect is a new hold and a new snapshot.

**Built** (`crates/noel-cli/src/server/view.rs`): the view follows Durable's shape `{conversation, entries, docs}` (`durable/docs/spec.md:4542-4558`), plus `lastRun`:
- **`conversation`:** `{id, cwd, name?}`;
- **`entries`:** a window over the session's history (`crates/noel-core/src/history.rs`): every message the session saved, with markers where compaction or an undo cut the model's context, as Durable's entry records `{id, kind, model?, data?, head?, timestamp?}`. Ids count entries in journal order, so they survive restarts and compaction checkpoints. The view starts with the newest 50; past 100 the oldest leave in one splice down to 50, and `before` names the entry before the window. `noel.chat`'s `entries({before, limit?})` pages back, and `noel-client`'s `Transcript` keeps every entry a client has seen and loads the ones before the window. The attached TUI shows the whole history: compaction and undo show as markers in place, not as a rewritten transcript;
- **`docs["pi.agent"]`:** `{model?, thinkingLevel, cwd}`;
- **`docs["pi.live"]`:** `{}` while idle; during a run `run` (`{runId, requestId?, inputs, turns}`), `generation` (`{attempt, message?, retry?}`, where `message` is the committed partial, §4.2), `tools` (`{callId, name, status, output?, truncated?, entry?}` with the committed output and the result's entry once saved; `status` is `pending`, `running`, `done`, or `interrupted` for a resumed run's call that does not run again) `compactions` (`[{trigger}]`, `manual`, `threshold` or `overflow`) while one runs; after a failed compaction `compactionFailure` (`{trigger, error}`), and after a run that ended without an answer `runFailure` (`{reason, detail?, inputs}`, reason `model_error` or `aborted`) until the next run starts;
- **`docs["pi.inbox"]`:** `{items: [{id, mode, content, requestId?}]}`;
- **`docs["pi.usage"]`:** `{total: {input, output, requests}}`;
- **`lastRun`:** `get_state.lastRun`.

Partials and tool output reach the view from the journal's records as each is saved (`SessionStore::observe_progress`), so a client sees what a crash would keep. A subscription gets the snapshot, then ops: a splice at the end of `entries` for each message, appends to a streaming reply or a tool's output, and diffs of the documents. Where this differs from Durable is in the non-parity register ("Mode 2 Server, Worker and Client").

### 7.4 The TUI

**Built** (`crates/noel-cli/src/attach.rs`). The TUI is always a client of a session a server holds, as upstream p2's is (`docs/client-tui-plan.md`). Plain interactive `noel` and `noel --attach <session-id>` are the same TUI with the same commands; only how the session is reached and what quitting does differ. The server is started when none listens (§5).
- **Plain `noel`:**
  - It prepares its session as before: `--session`, `--continue`, `--resume`, `--fork`, `--no-session` or a new one, with the model and the other flags. Then it lets go of the journal and attaches to the local server.
  - Its flags, environment and cwd go to the session's worker in `workers/<id>.args.json` when the worker starts (§5). A worker already running keeps its own, and the TUI says so. A session reached by a switch (`/resume`, a picker) keeps its own model.
  - A journal outside the server's session directory is named to the server in `workers/<id>.journal`. `--no-session` uses a journal in the runtime directory that is deleted on exit.
  - A session its own worker already holds is attached to without waiting for the lock.
  - Quitting aborts the active run and keeps the queue. With no other client, the worker ends, so the next `noel` opens the session as this one left it. A SIGTERM or SIGHUP that ends the only client also stops the worker, and the worker's tools with it.
  - Opening a session whose context ends mid-turn continues it once with Durable's `continue` submission, which places what is queued first.
  - `/login` runs on the terminal, as before, and the worker then reloads. A rebuilt binary (dogfood auto-restart) restarts the client between turns.
- **`--attach`:**
  - It takes no prompt, print mode or session flag, and refuses the options that shape a local session (model, thinking, tools, skills, templates, extensions, images and the like): the session runs with its own settings.
  - Quitting detaches; the run goes on.
  - `/login` on a server on another machine runs there, relayed through `noel.accounts` (§7.2): the TUI shows the device code or the sign-in URL and reads the pasted code or the API key on the plain terminal.
- **Before the terminal:** the TUI holds the session and waits for its first view before it takes the terminal. An unknown session (`unavailable` with `details.missing: "session"` from the server's route) or an invalid id is an error on the plain terminal, exit status 1.
- **The transcript** is rebuilt from the view's `entries`, as reopening a session rebuilds it from saved messages:
  - a reply's thinking, then its text, then `aborted` for an aborted reply;
  - a prompt (`queued>` or `steering>` for one placed from the queue);
  - a tool's result with its call's command or path.

  After the entries comes what the run streams in `docs["pi.live"]`: the committed partial's thinking and text, and each running or interrupted tool with its committed output. Each change of the view updates the rows it touches in place (a text that grows is appended to). A streaming reply is therefore one entry that becomes the saved reply. The TUI's own notes stay where they were added: a retry the run waits out, an account failover's switch (`noel.notices`), command output.
- **Input:**
  - Enter while no run is active submits with `whenBusy: "reject"`; during a run Enter queues a follow-up and the steer key (ctrl+s) queues a steer.
  - Submissions and commands go in the order typed. Each submission has a new `requestId`, and one that fails `unavailable` is sent again with the same id (§8.1). A prompt refused `busy`, because another client started a run meanwhile, is queued as a follow-up, as Enter does during a run.
  - Esc, ctrl+c or `/interrupt` during a run sends `abort` with the run as `expectedRunId` and `keepQueue`, once.
  - Up on an empty draft withdraws the session's last queued submission into the draft.
- **Commands:**
  - The session's commands run on its worker through `command({line})` (§7.2), whose answer gives the rows to show and what to do next: a run, a selector, another session to attach to, a draft, the diff panel, a login.
  - Managing sessions is the server's (`noel.sessions`, §7.2): `/fork` and `/clone` (of this session, or one named by id, id prefix or name), `/import <path>` and `/delete` of another session, after which the TUI attaches to the session made. Deleting the session shown, and importing the export of a session a worker holds, run on the worker.
  - The terminal's own commands run in the TUI: themes, accent, editor, multiline, clipboard, images, `/changelog`, the selectors' movement. `!` commands run on the worker.
  - `/bug` writes its report in the session's directory, with the error rows the TUI shows.
- **The footer** comes from `pi.live` during a run: what the run does (compacting, a running tool, writing, thinking or waiting) and how many submissions are queued. Otherwise it shows the model and thinking level from `pi.agent`, the queue, and the last run when it was aborted, failed or interrupted (`lastRun`). A run that failed is noted with its error. An abort another client made is noted when no aborted reply shows it.
- **Resume** of an interrupted run is the server's (§8.3).
- **Recovery** follows `noel client watch` (§8.1): a lost worker is noted, then the view is held again and redrawn from its snapshot; a lost server is noted and reconnected, starting a server.

Where the TUI differs from upstream pi's in-process TUI is in the non-parity register ("The TUI Is a Client of a Local Server").

## 8. Failure handling (Mode 2)

**Built** (§8.1–§8.3).

### 8.1 Client
- **A disconnect:** the server releases everything the client held. The run continues.
- **A submission's outcome is unknown when the link drops.** The client retries with the same `requestId`, which reports either `repeated` or the outcome. `noel client send` retries a hold or a `submit` that fails `unavailable` up to ten times.
- **`noel client watch`** and the attached TUI (§7.4) hold the view again when its worker is lost, and reconnect (starting a server) when the server is. Through the bridge (§6), a lost ssh or a lost server both end the link; reconnecting runs ssh again, and the bridge starts a server if none listens. The TUI sends a `submit` that fails `unavailable` again with the same `requestId`, as `noel client send` does.

### 8.2 Server
- **Losing the server stops no run:** a worker finishes its run while no server is attached.
- **A new server adopts the workers** (§5). Clients reconnect, hold their objects again, and get a fresh snapshot.

### 8.3 Worker
- **A worker crash:** the kernel releases its lock, and the run is interrupted.
- **Restart:** the server starts it again with the next generation. The new worker repairs a torn tail, applies §4.2, and resumes the run.
- **Restart limit:** the worker counts each crash restart of a run in its journal and ends the run `restart_limit` at `restartLimit` (§4.1), so the count holds across servers. One server also stops resuming a run by itself after 20 resumes, for a worker that crashes before it counts one.
- **When:** a worker lost with its run interrupted is started again at once. Any worker the server starts resumes the journal's interrupted last run, so after a machine stopped the first hold resumes it.
- **Clients:** they see `gone`, hold the object again, and get a snapshot.

## 9. Mode 1 relationship

**Decided:**
- RPC mode keeps upstream pi's record format and its abort rule (§3.3), and its goldens (`crates/noel-cli/tests/rpc_parity.rs`) guard both.
- §4.2 applies to every mode, so RPC mode saves partial output and aborted replies too. Upstream pi also keeps aborted replies. **Built.** Keeping them moved `get_state.messageCount` to upstream's after an abort (the `abort-with-queue` golden now differs from upstream's only by the system message).

**Built:** one session controller (`crates/noel-core/src/controller.rs`) drives runs. It owns, for one open session, the active run with its cancellation and retry backoff, the queue, submissions (§3.1), withdraw (§3.2), abort with a policy per caller (§3.3), resuming an interrupted run, compaction, and changes to the model and thinking level, which wait for no run to be active. It reports what happens as events, in order. RPC mode (`crates/noel-cli/src/rpc.rs`) is a thin adapter over it: commands in, records out, with abort keeping the queue; the Mode 2 worker (`crates/noel-cli/src/server/worker.rs`) is the second caller, with abort withdrawing the queue. The TUI (§7.4) goes through the worker's controller for every session.

### 9.1 Subagents

**Decided (2026-10-31):** noel's own feature (upstream has none), shaped after
Claude Code's.

- **Definitions:** a Markdown file with frontmatter — `description` (required,
 how the model chooses), optional `tools` (a comma list of noel tool names;
 omitted gives the parent's active tools), optional `model` (`provider/id`,
 built through a provider factory each mode installs), optional `thinking` —
 and a body that is the subagent's system prompt. They load from the
 project's `agents/` config directory, then the user's, the first of a name
 winning, as skills load; `/reload` reloads them.
- **The `task` tool:** offered to the model only when a definition is loaded;
 its description names the loaded agents. Its arguments are `subagent`,
 `description` (shown in the UI) and `prompt`. The subagent does not see the
 parent's conversation: its prompt must carry what it needs.
- **The run:** the child runs its own provider rounds in the parent's process,
 with its own session state (not journaled separately; the tool event is the
 record), the parent's systems (extension tools, retry, compaction, progress)
 and its scoped tools. The parent receives only the child's final reply, as
 the tool's result.
- **No nesting:** a definition whose `tools` list the `task` tool is refused,
 and the default scope excludes it.
- **Refusals like Claude's:** a definition whose tools resolve to nothing is
 refused with the offending names; an unknown subagent names the loaded ones.
- **Abort:** the parent's cancellation ends the child (the call's future is
 dropped); the parent saves its interrupted result, as for any tool.
- **Out of scope (for now):** parallel fan-out beyond the model's own
 concurrent tool calls, background subagents, and subagent-to-subagent
 messaging.

**Built:** `crates/noel-core/src/subagents.rs`, loader in
`crates/noel-config/src/agents.rs`, TUI label `● Used Task counter(Count
files)` with the child's reply as the tool output (ctrl+o expands).

### 9.2 Rewind

**Decided (2026-10-31, revised 2026-11-12):** Claude Code's Esc-Esc, in
noel's journal. `/undo` is the rewind to the last prompt.

- **Checkpoints:** every user prompt is one. The picker (Esc-Esc within
  400 ms at an empty draft with no run active, or `/rewind`) lists them
  newest first, `(current)` at the top, then each prompt with the files
  the turns from it on changed. Going back to a checkpoint hands its
  prompt back to the draft.
- **What to restore:** `/undo` restores the conversation and the files.
  A pick in the picker whose turns changed files asks: the conversation
  and the files, or the conversation only. With the conversation only,
  the files keep their changes and the dropped turns' snapshots move to
  the last turn kept, so a rewind to an earlier prompt still takes them
  back; with no turn kept they are forgotten.
- **Conversation:** rewinding drops the messages after the checkpoint and
  the progress saved for them. Runs, past submissions, the queue, the
  tool audit, `edited_files` and billed usage stay: they record what was
  asked and billed, and what a request id answers. The journal stays
  append-only: a messages snapshot (what the history shows, with its
  `reset to N messages` line) followed by a `rewound` record, so a
  restart loads the rewound state while the original records stay in the
  file.
- **Code:** every `edit`/`write`, the model's or a typed command, first
  snapshots the file's bytes into `<journal>.files/` (write-atomic,
  beside the journal; a file that does not exist is a *created* marker,
  and one that exists but cannot be read gets no snapshot), and once it
  has run a `file_edited` record keeps the hash of what it left. A rewind
  restores, per path, the oldest snapshot inside the dropped range (the
  state before the range's first edit) and deletes files the range
  created, through the workspace the tools ran in. A file that no longer
  holds what its last edit left is refused and named. The snapshots of a
  range whose files were restored are spent. A compaction, or any other
  replacement of the messages that does not keep their start, forgets
  the snapshots: message counts no longer place them, so a rewind does
  not reach past it.

**Built:** `crates/noel-core/src/rewind.rs` (`snapshot_before_edit`,
  `settle_snapshot`, `rewind`, `rewind_keeping_files`, `undo_last_turn`),
  the `file_snapshot`, `file_edited` and `rewound` records, and
  `crates/noel-cli/src/rewind_commands.rs` (the picker, the choice,
  `/rewind`, `/undo`).

### 9.3 Session goals

**Decided (2026-10-31, revised 2026-11-12):** Claude Code's `/goal`, in
noel's journal, run by the session's worker.

- **`/goal <condition>`** sets the session's goal — a condition a separate
  check can verify from the conversation ("all tests in `test/auth`
  pass"), at most one active; a new one replaces it. Bare `/goal` shows it
  with its turn count and last evaluation; `/goal clear` stops early. The
  goal is journaled (`goal`, `goal_cleared`), so a resumed session keeps
  working toward it.
- **The evaluator:** after each turn settles, one provider request over
  the session's transcript — the session's model, **no tools**, a strict
  first-line `MET`/`NOT MET` answer plus one sentence why. An answer that
  fails or does not parse changes nothing: the goal stays and the next
  settle tries again (a flaky evaluator must neither stop the work nor
  confirm a goal the work did not meet).
- **The loop:** the worker runs it, so it goes on with no client
  attached. A goal set on a session with no prompt yet starts the work
  with the goal as the prompt; otherwise the evaluator reads what is
  there. `NOT MET` submits a continuation prompt (the condition, the why,
  an instruction to keep working) and runs it; `MET` clears the goal.
  Each step is a note in `noel.notices`. A command that arrives while the
  evaluator is asked goes first and the evaluator is asked again after
  it. Interrupting the goal's turn, or the evaluator, pauses the goal, as
  does a run that fails: the goal stays, and the next prompt's turn is
  judged again. A turn cap ([`GOAL_MAX_TURNS`], 20) clears a wrong or
  never-satisfiable condition instead of looping. `/goal` is refused
  while a run is active, like any command.
- **Out of scope (for now):** the model-proposed goal dialog (Claude's
  `ProposeGoal` with one-keypress approval); a typed `/goal` covers the
  core.

**Built:** `crates/noel-core/src/goals.rs`, `Runtime::set_goal`/
  `clear_goal`/`advance_goal`, `/goal` in `session_commands.rs`, and the
  loop (`goal_step`) in `crates/noel-cli/src/server/worker.rs`.

### 9.4 Plan mode

**Decided (2026-10-31, revised 2026-11-12):** Claude Code's plan mode, as
a journaled mode.

- **`/plan`** toggles it (the footer says `plan`). In plan mode the
  system prompt gains the rules — research with read-only tools, `bash`
  for inspection only, present a concrete plan and do not implement —
  and `edit`/`write` are refused outright (`Tool edit not found`, the
  existing disabled-tool answer), so planning cannot change files.
  Extension, plugin and MCP tools are unaffected: their own gates decide
  them. A subagent of a planning session is in plan mode too.
- **Approval:** the view says the mode is on (`pi.agent.planMode`), and
  a client opens the choice when a plan-mode run it watched ends
  completed: `approve the plan and implement it` or `keep planning`. The
  pick is `/select plan-approval approve|keep`. Approve journals the mode
  off and submits `The plan is approved. Implement it.`, so the work
  starts at once; keep planning, or closing the choice, stays in the
  mode. A goal active under plan mode runs its loop only once planning
  is done.
- The mode is journaled (`plan_mode` records), so a resumed session keeps
  it; it is not part of a session export (a mode of the moment, not
  durable identity).
- **Out of scope (for now):** a dedicated plan-approval tool the model
  calls (Claude's `ExitPlanMode`) — the settle selector covers the loop.

**Built:** `Runtime::set_plan_mode`, the tool refusal in
  `model_tool_enabled`, the prompt addendum, `/plan` and the pick in
  `session_commands.rs`, and the choice in `attach.rs`.

### 9.5 Hooks

**Decided (2026-10-31):** Claude Code's tool hooks, as plain settings —
no host program, as extensions need.

- **Settings:** `hooks.preToolUse` and `hooks.postToolUse`, each a list of
  `{matcher, command}`. `matcher` is a `|`-separated list of tool names;
  empty matches every tool. The user's settings and a trusted project's
  merge per list (the project's list replaces the user's for a kind it
  configures), and `/reload` reloads them.
- **The run:** a hook is a shell one-liner run in the session's cwd
  (the session's shell) with one JSON object on stdin — `tool`,
  `arguments` (the raw JSON the model sent), `matcher`, `cwd`,
  `sessionId` — and `NOEL_SESSION_ID`/`NOEL_TOOL` in its environment.
- **Pre-hooks** run before the extension gate. **Exit 2 blocks** the
  call, the hook's stderr the reason the model sees (Claude's rule);
  any other exit allows, a non-zero one adding a notice line. A hook
  that cannot run at all blocks, fail-closed like the gate.
- **Post-hooks** run after the call's result is saved and reported;
  output is ignored, a non-zero exit is a notice line.
- **Out of scope (for now):** the other lifecycle events (prompt submit,
  stop, session start); a hook's stdout as a decision JSON (exit 2
  covers the blocking case).

**Built:** `crates/noel-core/src/hooks.rs`, the runs in `run_tool_call`,
  the settings in noel-config, notices as a `Notice` turn event in every
  mode.

### 9.7 MCP

**Decided (2026-11-01):** port upstream's `packages/mcp` — the stdio
slice first.

- **The client** (`crates/noel-mcp`): JSON-RPC 2.0 with upstream's
  discriminators, the initialize handshake (the server's chosen version
  must be one noel accepts), paged `tools/list` with duplicate-cursor
  and page guards, `tools/call` with content validated and defaulted as
  upstream's validators do, result content converted for the model
  (text and images through, audio/links/binary as placeholders,
  `structuredContent` standing in for empty content). A request times
  out (30 s default) and tells the server it was cancelled.
- **The stdio transport**: newline-delimited JSON over the server's
  stdin/stdout, the server in its own process group, shutdown per the
  spec (stdin close, SIGTERM, SIGKILL).
- **The streamable-HTTP transport** (upstream's
  `transports/streamable-http.ts`): JSON-RPC over POST answered as JSON
  or an SSE stream (whose events the client parses with the spec's line
  discipline — joined `data:` lines, `id`/`retry` fields, bounded
  sizes), a server-to-client GET stream opened after initialization
  (405 means the server offers none), `Mcp-Session-Id` and
  `MCP-Protocol-Version` headers, resumption by `Last-Event-ID` when a
  stream drops after the server assigned ids, reconnection with
  exponential backoff or the server's `retry`, and a best-effort DELETE
  on close. A 401 (or a 403 asking more scope) surfaces as
  `AuthRequired` for the OAuth layer; a 404 with a session is
  `SessionExpired`.
- **The config**: `mcp.json` in the agent dir and `.noel/mcp.json` of a
  trusted project, the shared `mcpServers` shape; project entries
  replace global ones of a name, and a project entry with only
  `enabled: false` overrides the global entry without its credentials.
  `${NAME}` in `env` expands from the environment.
- **OAuth for HTTP servers** (upstream's `packages/mcp/src/oauth`, adapted
  from the MCP TypeScript SDK): protected-resource and
  authorization-server metadata discovery (both well-known paths, with
  issuer validation and http(s)-only URLs), dynamic client registration,
  the authorization-code flow with PKCE S256 against a loopback callback
  (RFC 8252), token exchange and refresh with `client_secret_basic`,
  `client_secret_post` or `none` as the server supports, and the
  `WWW-Authenticate` challenge parsed for `resource_metadata`/`scope`.
  The callback answers only the redirect that carries the sign-in's
  random `state`. Credentials live in `<agent dir>/mcp-auth.json`, mode
  `0600`, keyed by server name (client, tokens, redirect URI). Connections never open a browser: the
  stored access token is sent from the handshake on, a 401 during
  connect tries one refresh of the stored refresh token, and the
  interactive sign-in is `noel mcp login <server>` (`logout` clears the
  stored credentials).
- **The surface**: servers connect at the first run that needs them (as
  extension hosts start); their tools overlay the run's tool surface as
  `mcp__<server>__<tool>` and execute through the connection, results
  joined as the tool's text. A failed connect is one system line, its
  tools absent until a later run retries. `/mcp` shows state,
  `noel mcp list` names them, `--no-mcp` connects none, `/reload`
  re-reads the config. `noel mcp add <name> --command … | --url … [-l]`
  and `noel mcp remove <name> [-l]` edit the file (`-l`: the project's),
  preserving unknown fields, with `--` separating server args that start
  with `-`; a file that does not parse is refused, not replaced.
- **Tool-argument validation** (pi-schema's semantics, upstream's
  `packages/ai/src/utils/validation.ts`): an MCP tool's declared schema
  validates the model's arguments before they reach the server —
  coercion first (a numeric string for a number, a single value for an
  array, `null` for an omitted optional, TypeBox's convert rules), then
  upstream's error wording with dotted paths
  (`- count: expected integer, got string`) and the received arguments
  in the message the model can act on. Built-in tools keep their
  hand-written validators, which the tool parity tests guard.
- **Not ported (yet)**: the Client ID Metadata Document preference of
  registration (upstream's `oauth.clientRegistration`), resources and
  prompts, exposure modes (upstream's codemode/deferred/tool-search).

**Built:** `crates/noel-mcp`, `crates/noel-config/src/mcp.rs`,
  `crates/noel-core/src/mcp.rs`, the `mcp` command and `/mcp` in the
  CLI and TUI.

### 9.9 Sessions the model can reach

**Decided (2026-11-02):** Claude Code's roster, taught through the
  system prompt — no new protocol.

- A **journaled** session's prompt gains a `<sessions>` section naming
  the `noel client` verbs (`ls --json`, `send` with `--steer`/
  `--follow-up`, `watch`) and the rule of thumb: check the listing for
  an idle session, delegate work that can proceed without this
  conversation's context, name yours so others can reach it. Ephemeral
  runs, subagent children (whose ids end `-task`) and the TS parity
  fixtures (no store) get no section, so prompt parity stays clean.
- `/sessions` lists the other sessions in the TUI (what `noel client
  ls` prints).
- No new wire: the messaging is the Mode 2 server's own (§5–§8),
  reached from the model's `bash`. A first-class `message` tool and a
  live status footer are future work.

**Built:** the section in `runtime_system_prompt`, `/sessions` in
  `session_commands.rs`.

## 10. Workspaces

**Decided (2026-10-14):** workspaces match upstream.
- **The interface** mirrors Durable's `ExecutionEnv`, a file system plus a shell (`durable/src/env/index.ts:208-373`). Operations return results and never throw. A string command runs through the shell, and an argument list runs without one. Abort or timeout kills only that command's processes.
- **Choosing a workspace:** the run engine asks a hook for each tool call, as Durable's harness `env(target)` does (`durable/src/harness/types.ts:569-582`).
- **`LocalEnv`** is first.
- **The remote adapter** speaks `pi-env`'s framed protocol version 1 (`env/docs/protocol.md`).
- **A Codewire adapter** is built on the MIT Rust SDK. It is optional.

**Built** in `crates/noel-env`, matching upstream's `durable/src/env` at upstream `main` (the TS parity fixtures' reference):
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
- **Interop:** `make chord2-interop` connects an upstream Chord v2 client host to `noel server`, and noel's client to an upstream server host (§7.1). **Built**, including upstream's client holding `noel.chat` on `noel server` through its relay to a worker, following a run and holding again after a drop.
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
The TUI becoming a client of a local server replaced "The TUI Attached to
a Server Session" with "The TUI Is a Client of a Local Server": where it
differs from upstream pi's in-process TUI. Remote workspaces added "Remote
Workspaces": noel's selection and trust, and where tools there differ from
local ones.

## 13. Plugins

**Built (2026-11-04, roadmap phase 7).** A plugin is a process `noel server` starts and keeps running, a Chord v2 host at `Plugin({name})` serving `noel.plugin`. The Rust SDK is `crates/noel-plugin`; `noel-sample-plugin` is its example and the tests' plugin.

- **Manifests:** `<agent dir>/plugins/<dir>/plugin.json`: `{name, version?, description?, command: [program, args...], permissions: [...]}`, the program relative to the manifest's directory when it is there. Names are `[a-z0-9-]`.
- **Grants:** `<agent dir>/plugins.json`: `{"<name>": {"permissions": [...]}}`. A plugin gets the permissions it requests that are granted there: `tools`, `commands` and `panels` for its contributions to count, `sessions` to hold `noel.sessions`. The server stamps its link `{kind: "plugin", name, permissions}`, and each object checks that principal (`forbidden` otherwise).
- **Supervision:** the plugin's stdin and stdout are its framed link to the server (§5's framing), the `plugin` route's link for its address; its stderr goes to `plugins/<name>.log` in the runtime directory. One that exits is started again after a backoff (0.5 s doubling to 30 s, back to 0.5 s after a minute up). Only the server's own references reach a plugin.
- **`noel.plugin`** (params `{plugin}`, variant `host`): state `contributions` `{tools: [{name, description, parameters, replaySafe?}], commands: [{name, description}], panels: [{id, title, view}]}`; `callTool({name, args})` (`args` as JSON text) `-> {output, isError}`, `runCommand({name, args?}) -> {output}`, `panelAction({panel, action, value?})`.
- **`noel.plugins`** (at `Server()`): state `directory` `{plugins: [{name, version?, description?, running, restarts, requested, granted, contributions, error?}]}`, each plugin's contributions limited to the kinds it was granted. The owner gets `runCommand` and `panelAction` (`{plugin, ...}`), a worker gets `callTool`; the server forwards each to the plugin when its kind is granted.
- **Tools:** each worker follows the directory and offers the plugins' tools to its model as extension tools (protocol `plugin`), selected by `enabledTools` and `excludedTools` as extension tools are, and kept across reloads; a new worker waits up to 3 s for the first directory before it handles a command. Calls go through the server's `callTool`.
- **Commands and panels:** `noel client plugins [--json]` lists the plugins with their panels rendered as text, `noel client run <plugin> <command> [args]` runs a command, and `noel client panel <plugin> <panel> <action> [value]` sends a panel action.
- **Panels** are views in a declarative vocabulary each client renders natively: `{type: "text", text}`, `{type: "stack", children}`, `{type: "list", items}` and `{type: "button", label, action}`.
- **Script plugins (built, roadmap phase 8):** a manifest with `script` instead of `command` (and optional `limits: {timeoutMs, memoryBytes}`, default 5000 ms and 64 MiB) runs in the server's process on the codemode VM (djs: sandboxed, fuel and memory limited, no JIT, so it also runs on iOS). The script defines `const plugin = {tools, commands, panels}`; tools and commands have `run(args, ctx)`, panels `view(state)` and `action(action, value, ctx)`. Each call runs on a fresh VM with no host calls; `ctx.state` and `ctx.save(state)` keep the plugin's state in `state.json` beside its manifest, across restarts, and the panels are drawn again from it after each call. It is served by the same `noel-plugin` host as a process plugin, over a link in memory, so grants, `noel.plugins` and tools work alike; a call past its limits fails (`timeout`, `sandbox`) and the plugin goes on. Its state is the plugin's, not a session's: plugins belong to the server.
- **Not built yet:** a Bun SDK; pi's extensions keep working through the extension host (unchanged).

## 14. Desktop app

**Built (2026-11-04, roadmap phase 9).** `noel-app` (`apps/desktop`, GPUI's community edition, gpui-ce) is a window on `noel server`; `noel app [--remote <ssh-destination>] [--session <id>]` runs it (beside `noel` or on the PATH; `make app` builds it), and `/app` in the TUI (local or attached) opens the current session in it. Plain `noel` is unchanged: the app is its own workspace, outside `make check`, checked by `make app-check`.

- **Link:** noel-client over the bridge's stdin and stdout: `noel bridge` for the local server (started when none runs), `ssh -T <destination> '<noel>' bridge` for a remote one (§6, tunnel), reconnected as `noel client` reconnects.
- **Window:** the sessions (from `noel.sessions`' directory) with a new-session button; the selected session's transcript (Durable entries, with the history before the view's window loaded once, the streaming reply and running tools from `pi.live`, tool results with `+`/`-` lines colored as diffs); a composer (secondary-enter sends a `followUp` submission; Abort); the plugins (`noel.plugins`) with their commands and panels drawn natively (§13's widgets; buttons send panel actions); and a command palette (secondary-k: new session, abort, open a session, plugin commands; a typed `/directory` is where a new session starts, which a remote server needs).
- **`--smoke`:** connects, waits for the directories and the session's view, prints them as JSON and exits, without a window; `scripts/app-smoke.sh` runs it locally, through `noel app`, and through a stand-in ssh, and with `SCREENSHOT=<file>` draws the window under Xvfb (software Vulkan).
- **macOS package:** `make app-bundle` creates `target/desktop/Noel.app` with the CLI beside it; `make app-window-smoke` checks a real CoreGraphics window. The vendored `gpui_ce_macos 0.3.0` drops its launch-callback mutex before opening the window.
- **Shared components:** `apps/shared/gpui-ui` contains the platform-neutral session, transcript, diff-block and plugin-panel components used by desktop and the GPUI iOS shell.
- **A Mode 1 session** open in this TUI holds its lock: the app shows it once this process lets it go (one writer at a time).

## 15. iOS

**Built (Rust side, 2026-11-04, roadmap phase 10).** `crates/noel-ffi` is noel-client's [`app`](../crates/noel-client/src/app.rs) link through UniFFI for Swift and Kotlin: `Noel.connect(opener, listener)`, then `select`, `send`, `newSession`, `abort`, `runPluginCommand`, `panelAction`; events (`connected`, `status`, `sessions`, `view` with the history's entries and the streaming reply, `plugins` with panels as JSON) come to the listener as records. The app opens the byte channel itself: `ChannelOpener.openChannel(delivery)` returns a `ByteChannel` for what the link writes, and the app hands what arrives to `delivery`; on iOS that channel is an SSH exec channel running `noel bridge` (`SSHChannelOpener`, on Citadel), and a lost channel is opened again. Its static library builds for `aarch64-apple-ios` and `aarch64-apple-ios-sim` on any host (`make ios-libs`, which also writes NoelKit's Swift bindings); `tests/channel.rs` runs it over a channel to a server it routes to a session host. The app builds on macOS in `.github/workflows/ios.yml` (`make ios`: xcframework, NoelKit and the app for the arm64 simulator; UniFFI's C header is syntax-checked with clang by `make ios-libs`). NoelKit pins Citadel by commit for `withExec`, which gives one exec channel's stdin and stdout.

**Written, built only on macOS:** `apps/ios`: NoelKit (the bindings, `NoelModel`, `SSHChannelOpener`) and a SwiftUI app (connect over SSH, sessions, chat with a composer and Abort, plugins with commands and panels in §13's widgets). `make ios` (macOS, Xcode, xcodegen) makes `NoelFFI.xcframework` and builds the app for the simulator; `.github/workflows/ios.yml` runs it on a macOS runner.

**Separate GPUI client:** `apps/ios/src/main.rs` starts a UIKit application through `gpui_platform`, with mobile session, chat and plugin screens composed from `apps/shared/gpui-ui`. `apps/ios/src/backend.rs` uses `noel-client` directly over a `russh` exec channel running `noel bridge`, not UniFFI. The connector keeps the SSH session alive, times out failed attempts, records a SHA-256 host-key fingerprint on first use and rejects later key changes; passwords are never persisted. `make ios-gpui` builds and ad-hoc signs the simulator app with the scene manifest in `apps/ios/App/Info.plist`. The SwiftUI path remains during GPUI validation.

## 16. Pairing and remote access

**Built (2026-11-04, roadmap phase 11); off by default.** After upstream Chord's app kit (`node/app/pairing.ts`, `owner.ts`), without its web pages (noel has no web UI): the owner approves pairings from the device socket.

- **Listening:** `server.listen` (`host:port`) makes `noel server` also listen over TLS. Its certificate is self-signed and kept in `<agent dir>/server-tls/` (0600); devices pin its SHA-256 fingerprint, which `noel client devices` shows on the server's machine.
- **Pairing:** a device runs `noel client pair https://host:port [--label L]`: `POST /_chord/pair` (public, `{label?}` → `{code, poll}`; 429 past 100 pending pairings or 10 per client address) gives a code, shown with the certificate's fingerprint; the owner compares the fingerprint and runs `noel client devices approve <code>` (`noel.devices`' `approve`); the device's poll (`POST /_chord/pair/poll`) gets its token once and keeps it, pinned to the certificate, in `<agent dir>/remotes.json` (0600). Codes are `XXXX-XXXX` without 0/O/1/I/L, single use, ten minutes; poll secrets and device tokens are stored only as SHA-256 hashes (`<agent dir>/devices.json`, 0600).
- **Using it:** `noel client --server https://host:port <command>` opens `GET /chord` over TLS with `Authorization: Bearer <token>`; the server stamps the link `{kind: "owner", via: "remote", device}`, an owner for everything but `noel.devices`, which only the owner on the server's machine holds. No valid token: 401. A certificate other than the pinned one: the TLS handshake fails.
- **Revoking:** `noel client devices revoke <id>` drops the device's token and revokes `{device: id}` (v2 revocation): its objects end and its links close.
- **Not built:** guests (§6), owner links for browsers, and a security review, which comes before `server.listen` is ever on by default.

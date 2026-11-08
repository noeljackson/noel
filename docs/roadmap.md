# noel roadmap

The plan for codemode, Chord v2, the view, plugins, a native desktop app and an
iOS client. Tracked in GitHub issue #114, one sub-issue per phase. Each phase lists what it delivers and when it is done. Status:
**Done**, **Next**, **Planned**. Update this file in the same change that moves
a phase.

Upstream reference: `earendil-works/pi` `main` (`4b11a722`, 2026-11-06). Every
TS parity fixture, the env goldens and `chord-delta.json` included, is made
from `main`; Chord v2 has its own pin, `CHORD2_REFERENCE_REF`
(`docs/upstream-reconcile-2026-11.md`).

## Direction

- **One engine, many clients.** `noel server` holds sessions; the TUI, a GPUI
  desktop app and a SwiftUI iOS app are clients through `noel-client`. Plain
  `noel` keeps working as today.
- **Follow Chord v2 and Durable 1.5 for protocol and view; do not follow p2.**
  `apps/p2` (pi's React web UI) is marked as a prototype to be replaced. noel
  takes the protocol and state shapes, not the app.
- **Native UI, no plugin code in UI processes.** Plugins publish state and a
  small widget vocabulary; each client renders it natively.
- **Two plugin tiers.** Out-of-process Chord hosts (any language) for heavy
  work; in-process djs scripts (sandboxed, no JIT, runs on iOS) for hooks,
  commands and panels.

## Phases

### 0. Reliable `make check` (Done, f5907b0, #115)
- Lost Chord snapshots (a snapshot before `subscribe` was dropped), bun test
  env races, bun auto-installing pi 1.5.0 for the vendored examples, and a
  config test reading `ANTHROPIC_BASE_URL`. Five passes in a row.
- `make check` needs `ssh-keygen` (OpenSSH client) for `noel-env-daemon`'s
  ssh tests.

### 1. `noel-client` (Done, c8086d9)
- Chord client, contracts, reconnect, follow; transports through `Connector`.
  Type-checks for `aarch64-apple-ios`.

### 2. Upstream audits (Done, approved by the owner 2026-11-05; #116, #117)
- **Durable 1.5.0:** `docs/durable-1.5-audit.md`. Top ports: entry ids and
  raw history kept across compaction (L, blocks phase 5); codemode on djs
  with nested calls; `pi.live` v3 subset, `runFailure` and run-end rules;
  windowed view with paging; `noel-env-daemon` signal handling. Every
  Durable citation in noel's docs has moved (table in the audit).
- **Chord v2:** `docs/chord-v2-port.md`. v2 is a new protocol (binary codec,
  18 message kinds, typed host addresses, new fingerprints), wire marked
  unstable upstream; only the delta op encoding carries over. Proposed pin
  `3551ec07`; about 8-11k lines, replacing most of `noel-chord`'s host.
- Approved by the owner on 2026-11-05. The spec citations and the
  non-parity register were updated with phases 3-5.

### 3. Codemode (Done, #118)
- Done: `crates/noel-codemode` runs upstream's prelude on djs (pinned by
  commit; type-checks for iOS), with upstream's source options, execution
  layer and result format. The built-in `codemode` tool is off unless
  selected by name; nested calls go through the gate and run in parallel;
  the store is saved in the journal. Spec: `docs/codemode-port.md`;
  differences: the register's Codemode section.
- Also done: nested-call events and TUI rows, structured `bash`/`read`
  results, the `Script aborted` result; upstream's declarations
  (`noel_codemode::declarations`) and BM25 tool search
  (`noel_codemode::search`) with goldens recorded from upstream's own tests
  (`scripts/ts-parity/codemode-declarations-fixture.ts`,
  `tool-search-fixture.ts`); the description catalog with
  `codemode.mode`/`inlineBudget`; the discovery globals.
- Not ported: the `tool_search` tool, which loads `deferred` tools; noel
  has no tool exposures (register: Tools).

### 4. Chord v2 port (Done, #119)
- noel's server, workers and clients speak Chord v2 (`noel_chord::v2`):
  contracts and fingerprints, messages and the binary codec with the message
  check, the error table, part limits, the channel, hosts with routes,
  relays and revocation, WebSocket and framed transports. Goldens from
  upstream at `CHORD2_REFERENCE_REF`; `make chord2-interop` passes both ways;
  `mode2` passes on v2. The v1 implementation is deleted. Differences are in
  the non-parity register ("Chord v2"); noel's refusals are plugin error
  codes since v2's table is closed.

### 5. View parity (Done, #120)
- Done: the session history (`noel-core/src/history.rs`, ids that survive
  restarts and compaction, markers for compaction and undo), the windowed
  `noel.chat` view in Durable's entry shape with `before` and an `entries`
  paging method, `noel-client`'s `Transcript`, the attached TUI showing the
  whole history, and `pi.live` v3's `turns`, `compactions`,
  `compactionFailure`, `runFailure` and slot `entry`.
- Durable's run-end rules, adopted by the owner (2026-11-05): `runFailure`
  reasons saved with each run's end, a crash-interrupted request charged to
  `retry` (else `interrupted`), `restartLimit` (default 5, counted in the
  journal), `continue` (`noel client continue`), `whenIdle: "reject"`, and
  `turn_limit` at `agent.maxToolRounds`. Spec §3.1, §4.1, §8.3; differences
  in the register ("Run-End Rules").

### 6. Storage trait (Done, #121)
- `noel_core::storage::JournalStorage`: append a record, read the journal,
  rewrite it atomically. `SessionStore` keeps the records' meaning
  (recovery, checkpoints, history) over any storage: `JsonlFile` (the
  journal file under the session lock, the default) and `MemoryJournal`.
  Every recovery and journal test passes through the trait; the recovery
  paths (compaction checkpoint, partial recovery, torn tail) also run on
  `MemoryJournal`. SQLite can be added as another `JournalStorage`.

### 7. Plugin host (Done, #122)
- Plugins are Chord v2 hosts at `Plugin({name})` that `noel server` starts
  from manifests and restarts when they exit; they contribute tools,
  commands and panels (`noel.plugin`), which the server publishes in
  `noel.plugins` limited to the permissions `plugins.json` grants; each
  object checks the plugin principal. Workers offer plugin tools to their
  model; `noel client plugins|run|panel` lists, runs and acts on panels.
  Rust SDK: `crates/noel-plugin`. Spec §13.
- Done: `crates/noel-cli/tests/plugins.rs` runs the sample plugin: its tool
  called by a session's model, its command and panel, a refused
  `sessions` permission, and a restart after a crash.
- Follow-up: a Bun SDK (pi extensions keep working through the extension
  host meanwhile); panels in the attached TUI and the desktop app (phase 9).

### 8. djs plugin tier (Done, #123)
- Script plugins (`script` in the manifest) run in the server's process on
  the codemode VM: sandboxed, time and memory limited, no JIT; same
  contributions, grants and `noel.plugins` as phase 7, through the same
  `noel-plugin` host over a memory link. Their state is kept with the
  plugin (`state.json`) across restarts (plugins belong to the server, not
  a session). Spec §13; tested by `tests/plugins.rs`.

### 9. Desktop app, GPUI (Done, #124)
- `noel app` / `noel-app` (`apps/desktop`, gpui-ce): sessions sidebar,
  transcript with diff coloring, composer, command palette, plugin
  commands and panels; local server through `noel bridge`, remote through
  `ssh … noel bridge`. `/app` in the TUI opens the current session. Its own
  workspace (`make app`, `make app-check`), so plain `noel` and `make check`
  are unchanged. Spec §14.
- Done: `scripts/app-smoke.sh` runs it against a real server locally,
  through `noel app` and through ssh, and draws the window under Xvfb.
  `make app-bundle` packages `Noel.app`; `make app-window-smoke` verifies a
  real macOS window. Platform-neutral transcript and panel components live in
  `apps/shared/gpui-ui` for the desktop and GPUI iOS shells.

### 10. `noel-ffi` and iOS (Done, #125)
- `crates/noel-ffi`: noel-client's app link through UniFFI, over a byte
  channel the app opens (SSH to `noel bridge`), events as records; tested
  over a channel (`tests/channel.rs`); static libraries for
  `aarch64-apple-ios` and `aarch64-apple-ios-sim` build on Linux
  (`make ios-libs`). `noel-client::app` is shared with the desktop app.
- `apps/ios`: NoelKit and the SwiftUI app (sessions, chat, plugin panels),
  `make ios` and `.github/workflows/ios.yml` (macOS runner). Spec §15.
- A separate GPUI iOS shell uses `apps/shared/gpui-ui` and `noel-client`
  directly, with a `russh` bridge and first-use host-key fingerprints;
  `make ios-gpui` packages its simulator app. The SwiftUI path remains while
  the GPUI path is being validated.
- The macOS workflow (`macos-15`, Xcode 16) builds the static libraries,
  the xcframework, NoelKit and the app for the arm64 simulator. Citadel is
  pinned by commit for `withExec` (not in a tagged release yet). The app
  has not been run against a server from a device or simulator.

### 11. Pairing and remote access (Done, off by default, #126)
- `server.listen` adds a TLS listener (self-signed, fingerprint pinned);
  devices pair with `noel client pair` (upstream's `/_chord/pair` and
  `/_chord/pair/poll`), the owner approves with `noel client devices
  approve`, and devices connect with `noel client --server` (Bearer token,
  stamped `{kind: "owner", via: "remote", device}`); `devices revoke` uses
  v2 revocation. Spec §16; tested by `tests/remote.rs`.
- Open: guests, and the security review before it is on by default
  (the owner's).

## Decisions pending

- Plain `noel` through the server by default (`server.default`), once the
  attached TUI passes `make e2e`.
- Which TypeScript runtime wins per use: Bun (extensions) or djs (codemode,
  in-process plugins).

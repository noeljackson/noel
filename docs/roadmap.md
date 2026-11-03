# noel roadmap

The plan for codemode, Chord v2, the view, plugins, a native desktop app and an
iOS client. Tracked in GitHub issue #114, one sub-issue per phase. Each phase lists what it delivers and when it is done. Status:
**Done**, **Next**, **Planned**. Update this file in the same change that moves
a phase.

Upstream reference: `earendil-works/pi` tip `7dc38ff` (2026-11-03). noel's
parity pins are older: Durable `1692bba` and Chord `b45e0ebc` (both
2026-10-14). Since then Durable has 77 commits (1.5.0, many breaking changes)
and Chord 215; Chord v2 is the package root and v1 is removed upstream (apps on
v1 pin `@earendil-works/chord@1.5.0`).

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

### 2. Upstream audits (Written, awaiting owner review; #116, #117)
- **Durable 1.5.0:** `docs/durable-1.5-audit.md`. Top ports: entry ids and
  raw history kept across compaction (L, blocks phase 5); codemode on djs
  with nested calls; `pi.live` v3 subset, `runFailure` and run-end rules;
  windowed view with paging; `noel-env-daemon` signal handling. Every
  Durable citation in noel's docs has moved (table in the audit).
- **Chord v2:** `docs/chord-v2-port.md`. v2 is a new protocol (binary codec,
  18 message kinds, typed host addresses, new fingerprints), wire marked
  unstable upstream; only the delta op encoding carries over. Proposed pin
  `3551ec07`; about 8-11k lines, replacing most of `noel-chord`'s host.
- Done when the owner approves both and the non-parity register and spec
  citations are updated.

### 3. Codemode (Planned, #118)
- djs spike (done, see #118): `djs-core` embeds natively (async host
  functions, parallel calls, fuel, abort) and type-checks for
  `aarch64-apple-ios`.
- `noel-codemode` crate and a `codemode` tool, off by default, matching
  upstream's `packages/codemode` and `durable/src/codemode`.
- Tests with the faux provider; goldens recorded from upstream.
- Done: `make check` and `make e2e` pass; codemode leaves the non-parity
  register.

### 4. Chord v2 port (Planned, #119)
- `noel-chord` speaks v2's link protocol; server, worker and client move to v2
  hosts and contracts; `noel-client` keeps its API so clients do not change.
- Plan in `docs/chord-v2-port.md`: contracts and fingerprints, codecs,
  channel, provider/consumer, relay, transports, then a `chord2-interop`
  target; noel generates its own goldens (upstream has none).
- Done: interop passes both ways; `mode2` tests pass on v2.

### 5. View parity (Planned, #120)
- Journal entries get ids and timestamps; the view is a window (newest 50, at
  most 100, `before`), `pi.live` v3 (`run.turns`, `runFailure`, nested tools,
  compaction states), rendering from state only.
- Done: goldens against Durable 1.5's view; TUI attach renders from it.

### 6. Storage trait (Planned, #121)
- The JSONL journal behind a storage trait; SQLite as a second backend later.
- Done: existing recovery and journal tests pass through the trait.

### 7. Plugin host (Planned, #122)
- Plugins as Chord v2 hosts with a manifest; contributions through v2-style
  registries: tools, commands, hooks, panels. Permissions checked per
  principal. Rust SDK first, then Bun.
- Done: a sample plugin adds a tool, a command and a panel; a crash restarts
  it; a denied permission is refused.

### 8. djs plugin tier (Planned, #123)
- In-process plugins on the codemode host layer; same contributions as phase 7.

### 9. Desktop app, GPUI (Planned, #124)
- `noel app`: sessions sidebar, transcript, composer, palette, diffs, plugin
  panels; local or ssh server. `/app` in the TUI opens the current session.
- Done: usable against local and remote servers; plain `noel` unchanged.

### 10. `noel-ffi` and iOS (Planned, #125)
- UniFFI over `noel-client` and the view types; SwiftUI app over an ssh
  channel to `noel bridge`. Built on macOS.

### 11. Pairing and remote access (Planned, #126)
- Owner links or device pairing over TLS, following Chord v2's HTTP auth;
  guests later. Security review before default-on.

## Decisions pending

- Plain `noel` through the server by default (`server.default`), once the
  attached TUI passes `make e2e`.
- Which TypeScript runtime wins per use: Bun (extensions) or djs (codemode,
  in-process plugins).

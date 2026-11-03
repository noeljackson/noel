# noel roadmap

The plan for codemode, Chord v2, the view, plugins, a native desktop app and an
iOS client. Each phase lists what it delivers and when it is done. Status:
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

### 0. Reliable `make check` (Next)
- `noel-config`'s `model_cache_extends_builtin_and_explicit_models` must not
  read `ANTHROPIC_BASE_URL` from the environment.
- Find why `noel-core`'s `upstream_hello_tool_example_runs_under_bun_host`
  and `noel-cli`'s `mode2` client test fail under load.
- Done: `make check` passes 5 runs in a row.

### 1. `noel-client` (Done)
- Chord client, contracts, reconnect, follow; transports through `Connector`.
  Type-checks for `aarch64-apple-ios`.

### 2. Upstream audits (Next; read-only)
- **Durable 1.5.0:** diff against `1692bba` for runs, submissions, view,
  `pi.live` v3, `runFailure`, storage, tasks, `codemode`, `tool_search`.
  Output: updated `non-parity-register.md` entries and a list of ports.
- **Chord v2:** read `packages/chord/docs/design.md`, `decisions.md`,
  `slices.md`; record the link protocol, contracts, hosts (`Server`,
  `Session({id})`), registries, workers and auth. Output: a port plan for
  `noel-chord` and the server, with what stays noel's own (ssh bridge,
  workers outliving the server, JSONL journal).
- Done: both audits written under `docs/`, owner approved.

### 3. Codemode (Planned)
- djs spike: embed `djs-core` natively (VM, host functions, async calls,
  fuel, abort), build for `aarch64-apple-ios`, pin by commit.
- `noel-codemode` crate and a `codemode` tool, off by default, matching
  upstream's `packages/codemode` and `durable/src/codemode`.
- Tests with the faux provider; goldens recorded from upstream.
- Done: `make check` and `make e2e` pass; codemode leaves the non-parity
  register.

### 4. Chord v2 port (Planned)
- `noel-chord` speaks v2's link protocol; server, worker and client move to v2
  hosts and contracts; `noel-client` keeps its API so clients do not change.
- Interop test against upstream tip in Docker, as `make chord-interop` does
  for v1.
- Done: interop passes both ways; `mode2` tests pass on v2.

### 5. View parity (Planned)
- Journal entries get ids and timestamps; the view is a window (newest 50, at
  most 100, `before`), `pi.live` v3 (`run.turns`, `runFailure`, nested tools,
  compaction states), rendering from state only.
- Done: goldens against Durable 1.5's view; TUI attach renders from it.

### 6. Storage trait (Planned)
- The JSONL journal behind a storage trait; SQLite as a second backend later.
- Done: existing recovery and journal tests pass through the trait.

### 7. Plugin host (Planned)
- Plugins as Chord v2 hosts with a manifest; contributions through v2-style
  registries: tools, commands, hooks, panels. Permissions checked per
  principal. Rust SDK first, then Bun.
- Done: a sample plugin adds a tool, a command and a panel; a crash restarts
  it; a denied permission is refused.

### 8. djs plugin tier (Planned)
- In-process plugins on the codemode host layer; same contributions as phase 7.

### 9. Desktop app, GPUI (Planned)
- `noel app`: sessions sidebar, transcript, composer, palette, diffs, plugin
  panels; local or ssh server. `/app` in the TUI opens the current session.
- Done: usable against local and remote servers; plain `noel` unchanged.

### 10. `noel-ffi` and iOS (Planned)
- UniFFI over `noel-client` and the view types; SwiftUI app over an ssh
  channel to `noel bridge`. Built on macOS.

### 11. Pairing and remote access (Planned)
- Owner links or device pairing over TLS, following Chord v2's HTTP auth;
  guests later. Security review before default-on.

## Decisions pending

- Plain `noel` through the server by default (`server.default`), once the
  attached TUI passes `make e2e`.
- Which TypeScript runtime wins per use: Bun (extensions) or djs (codemode,
  in-process plugins).

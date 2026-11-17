# noel-chord

Chord for noel: the Chord v2 link protocol that `noel server`, its workers
and its clients speak, ported from upstream's `packages/chord` at
`CHORD2_REFERENCE_REF` (Makefile), and the Delta op codec its parts carry.

## Modules

- `v2::contract`: contracts as noel declares them
  (`crates/noel-client/contracts/v2/*.json`), their schema normal form and
  fingerprints (`hash64` over UTF-8), variant and member order, params and
  host addresses.
- `v2::message`, `v2::binary`, `v2::check`, `v2::error`: the 18 message
  kinds, the binary codec, the message schema check, the closed error table
  and wire errors.
- `v2::part`: limits per part, deflate at an origin, inflate within limits.
- `v2::channel`: one end of a link, without I/O: handshake, checks,
  heartbeat.
- `v2::host`: a host actor with both sides of each link (holds, the state
  window, calls and call windows, refused holds, `loading`, `retry`), routes
  and relayed holds, revocation, and remote references with state streams.
- `v2::object`: what a provider serves: cells and methods per variant.
- `v2::link`, `v2::transport`: byte links (in memory, WebSocket binary
  messages, framed byte streams with control frames).
- `delta`: the Delta op codec, immutable applier and `diff`.
- `schema::canonical_json`, `validate::check`: canonical JSON and the JSON
  Schema subset contracts use.

## Checks

```bash
cargo test -p noel-chord      # unit tests, v2 goldens, hosts over memory links
make chord2-fixtures CHORD2_REFERENCE_DIR=<pi checkout at the pin>
make chord2-interop  CHORD2_REFERENCE_DIR=<pi checkout at the pin>
```

- `tests/chord_v2_contracts.rs` and `tests/chord_v2_messages.rs` match
  `tests/fixtures/ts-parity/chord-v2-*.json`, written from upstream by
  `scripts/ts-parity/chord-v2-*-fixture.ts`: fingerprints, every message
  kind byte for byte, upstream's message-check verdicts and ChordError
  envelopes.
- `tests/chord_v2_host.rs` runs hosts over memory links, WebSockets and
  framed streams: state and calls, refused holds, the window against a
  consumer that does not acknowledge, drops and re-holds, relays to
  workers, revocation.
- `tests/chord_delta_parity.rs` matches `chord-delta.json` (Chord's delta
  codec, applier, diff and canonical JSON at upstream `main`).
- `make chord2-interop` (`scripts/chord2-interop.sh`) runs both directions
  against upstream with Bun over Unix sockets; `examples/chord2-client.rs`
  is noel's side.

Where noel differs from upstream is listed in
`docs/rust-rewrite/non-parity-register.md` ("Chord v2").

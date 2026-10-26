# noel-chord

Chord link protocol 4 for noel, ported from upstream pi's `packages/chord`:

- `delta`: the ops `r`, `s`, `d`, `a`, `t`, `p` and `m` over
  `serde_json::Value`, applied without changing the base
  (`apply_immutable`, `apply_immutable_batches`, `apply_owned`); the wire
  `Encoder` and `Decoder`; and `diff`. `t` and `a` count UTF-16 code units.
- `schema`: `canonical_json`, `hash64` and `fits`.
- `contract`: `Description`, its `fingerprint`, and `coverage`.
- `wire`: every protocol 4 frame (`Frame`), checked on receipt as upstream's
  frame schema checks it.
- `link`: links and their transports: `Link::memory_pair` (tests),
  `websocket` (one JSON text message per frame; packed binary frames are
  decoded when accepted and never sent) and `ndjson` (`{"chord2": frame}`
  lines, unwindowed, with `{"chord2control": message}` lines for control).
  `Link::new` builds a link over any other medium.
- `host`: a `Host` serves object types (a contract and a factory that picks a
  variant per principal), accepts links from clients and child hosts,
  connects to a parent, relays holds through routes (`authorize` before and
  after `resolve`), stamps principals, holds remote objects (`Host::get`,
  `RemoteRef`), and revokes principals (`invalidate_principal`). Each
  connection runs `hello`, holds, calls with cancellation and `maxCalls`,
  state subscriptions through one delta encoder each, pings, and
  acknowledgements with the flow-control window.
- `object`: what a factory returns (`Implementation`: a variant, state
  `Cell`s and methods) and what a method gets (`CallContext`).
- `validate`: values against contract schemas, and input normalization.

There is no hot reload and there are no plugins: a host runs one code
version and answers reload control with `unavailable`.

```rust
let server = Host::new(HostOptions::host("server"));
server.serve(contract, |input, principal| {
    let count = Cell::new(schema, json!({"n": 0}))?;
    Ok(Implementation::new("owner").state("count", &count).method("add", ...))
});
server.accept(link, AcceptOptions::client(json!({"kind": "owner", "via": "device"})));

let client = Host::new(HostOptions::client("client"));
client.connect(peer_link, LinkOptions::default());
let counter = client.get(contract, json!({"name": "a"}), "server");
let mut count = counter.subscribe("count"); // Snapshot, then Ops
counter.call("add", Some(json!(1))).await?;
```

## Examples

```sh
# A server on an owner-only Unix socket (WebSocket upgrade on /chord). It
# checks each peer's uid before reading a byte and logs holds and releases.
cargo run -p noel-chord --example device-server -- --socket /run/user/$UID/dev.sock

# Hold the counter there: snapshot, then ops.
cargo run -p noel-chord --example chord-client -- --socket /run/user/$UID/dev.sock

# Decode the delta fixture's batches and compare them with upstream's values.
cargo run -p noel-chord --example replay
```

## Parity

Each piece matches upstream at one pinned commit, `CHORD_REFERENCE_REF` in
the repository's Makefile, independent of the `TS_REFERENCE_REF` the other
goldens use. Two fixtures run upstream's code in the ts-parity Docker image:

- `scripts/ts-parity/chord-delta-fixture.ts` writes
  `tests/fixtures/ts-parity/chord-delta.json`, and
  `tests/chord_delta_parity.rs` checks the codec, canonical JSON, `hash64` and
  contracts against every vector in it;
- `scripts/ts-parity/chord-link-fixture.ts` runs upstream's hosts in fifteen
  scenarios (hello; holds and `gone`; subscriptions; calls and errors;
  cancel, `maxCalls` and release; relay; refused routes and forged
  principals; `invalidatePrincipal`; pings and the ping timeout; window
  coalescing and the four-window reset; version mismatches; protocol
  violations), recording every frame on every link, and writes
  `tests/fixtures/ts-parity/chord-link.json`. `tests/chord_link_parity.rs`
  runs the same scenarios on noel's hosts and compares the frames of each
  link and direction in order, ids included.

`make chord-interop` (`scripts/chord-interop.sh`) then runs upstream's
client host against `device-server` over a real WebSocket on a Unix socket,
kills a client that holds the counter to see its hold released, and runs
`chord-client` against upstream's server host.

```sh
make ts-parity-chord      # regenerate both chord goldens in Docker
make chord-interop        # interop with upstream, in Docker
cargo test -p noel-chord
```

`make ts-parity-drift` lists the `packages/chord` commits upstream made after
the pin. To move the pin, change `CHORD_REFERENCE_REF` (the image build fails
if `packages/chord` does not build there), regenerate, and bring this crate
level with the new goldens.

Error messages are upstream's where noel makes the same error, and
`DeltaError::upstream_name` names the error upstream throws. Where noel
differs, see `docs/rust-rewrite/non-parity-register.md`, "Chord Delta,
Canonical JSON and Contracts" and "Chord Link and Hosts".

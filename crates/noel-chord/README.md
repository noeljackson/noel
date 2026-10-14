# noel-chord

The parts of Chord link protocol 2 that need no link, ported from upstream
pi's `packages/chord`:

- `delta`: the ops `r`, `s`, `d`, `a`, `t`, `p` and `m` over
  `serde_json::Value`, applied without changing the base
  (`apply_immutable`, `apply_immutable_batches`, `apply_owned`); the wire
  `Encoder` and `Decoder`; and `diff`. `t` and `a` count UTF-16 code units.
- `schema`: `canonical_json`, `hash64` and `fits`.
- `contract`: `Description`, its `fingerprint`, and `coverage`.

Links, frames, hosts and transports are not here yet.

## Parity

Each piece matches upstream at one pinned commit, `CHORD_REFERENCE_REF` in
the repository's Makefile, independent of the `TS_REFERENCE_REF` the other
goldens use. `scripts/ts-parity/chord-delta-fixture.ts` runs upstream's code
in the ts-parity Docker image and writes
`tests/fixtures/ts-parity/chord-delta.json`; `tests/chord_delta_parity.rs`
checks this crate against every vector in it:

```sh
make ts-parity-fixtures   # regenerate the goldens in Docker
cargo test -p noel-chord
cargo run -p noel-chord --example replay   # decode upstream's batches and compare
```

`make ts-parity-drift` lists the `packages/chord` commits upstream made after
the pin. To move the pin, change `CHORD_REFERENCE_REF` (the image build fails
if `packages/chord` does not build there), regenerate, and bring this crate
level with the new goldens.

Error messages are upstream's, and `DeltaError::upstream_name` names the
error upstream throws. Where noel differs, see
`docs/rust-rewrite/non-parity-register.md`, "Chord Delta, Canonical JSON and
Contracts".

# Rust Rewrite Non-Parity Register

Every Rust behavior should either match a committed TypeScript parity fixture or
appear here with a deliberate reason for not cloning the TypeScript behavior.

## Active Non-Parity Decisions

### No TypeScript Runtime Dependency

Rust must not import, shell out to, or embed TypeScript or JavaScript at
runtime. TypeScript is allowed only inside the Docker reference runner used to
generate parity fixtures.

Reason: the product goal is a native Rust CLI/TUI, not a wrapper around the old
runtime.

### No Web UI Product Path

The old TypeScript web UI is not part of the active Rust product.

Reason: the active product scope is CLI/TUI-only. Historical web UI labels may
remain in GitHub for cutover tracking, but new implementation work should not
add web UI dependencies.

### No npm or Node Host Workflow

Normal development, tests, and validation must not run npm on the host.

Reason: the repository is Rust-only. TypeScript reference execution is isolated
to Docker so parity checks do not contaminate the product toolchain.

### No Automatic Legacy Session Migration

Rust does not automatically read or migrate old TypeScript session logs.

Reason: Rust live sessions have a durable replay JSONL contract. TypeScript v3
JSONL export/import and direct open are supported where practical, but old logs
are not automatically migrated in place. The TypeScript reference branch remains
available for forensic reading of old behavior.

### No Live Providers in Normal Tests

Normal tests use faux/local providers or sanitized request fixtures.

Reason: CI and local validation must not require credentials, spend money, or
leak request data. Real-provider smoke remains opt-in.

### No Full Autonomous Transcript Oracle

The parity harness does not use full live autonomous transcripts as its primary
correctness signal.

Reason: provider sampling, shell timing, terminal dimensions, and network state
make full transcripts brittle. The harness instead captures deterministic
contracts for request shape, normalized messages, tool dispatch, storage,
settings, and TUI markers.

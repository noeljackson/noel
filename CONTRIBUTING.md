# Contributing to pi

This repository is now a Rust-only Cargo workspace.

## Before Submitting

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Do not add npm, Node.js, TypeScript, Vite, React, or web UI dependencies unless the maintainer explicitly reopens that scope.

## Issue and PR Gate

New issues and PRs from new contributors may be auto-closed. Maintainers review worthwhile reports and can approve future participation with:

- `lgtmi`: future issues stay open
- `lgtm`: future issues and PRs stay open

Keep issues short, concrete, and reproducible.

## Code Expectations

- Prefer idiomatic Rust over ports of the old TypeScript structure.
- Keep session state separate from reloadable systems.
- Add tests for behavior changes.
- Do not use real provider credentials in tests.
- Preserve user files and dirty worktrees in tool behavior.

# Extension Examples

Working examples of noel extensions, one directory per host kind. Copy a
file into `~/.noel/extensions/` (user-wide) or the project's
`.noel/extensions/` and add the manifest beside it; `/reload` picks it up.

## `typescript/` — runs under the bun host

A `.ts` extension whose default export is a factory (upstream pi's
`ExtensionAPI` shape) runs under `bun` through noel's shipped shim:
`registerTool`, `registerCommand`, and `on("tool_call", …)` gates work;
type-only imports are erased by bun, so upstream examples load verbatim.

Requires `bun` on PATH. These files are synced from upstream by
`scripts/sync-upstream-examples.sh` (see `.upstream-source.json` for the
commit they came from; upstream is MIT, license in `.upstream-LICENSE`).
Run the script again when the parity pin moves; the noel-core
`extensions::` tests load the vendored copies and fail closed if one
stops initializing.

- `hello.ts` — the minimal `defineTool` tool
- `permission-gate.ts` — blocks dangerous bash commands (`tool_call` gate)
- `protected-paths.ts` — blocks writes to sensitive paths
- `notify.ts` — terminal notification when the agent settles
- `structured-output.ts` — a terminating tool with prompt guidelines

## `rust/` — a host program in Rust

Any executable speaking protocol v2 over stdio is a host: one JSON request
line in, one JSON response line out; notifications take no response.
`host.rs` is deliberately dependency-free (std only) so `rustc` alone
builds it:

```sh
rustc -O examples/extensions/rust/host.rs -o ~/.noel/extensions/rust-host
echo '{"protocolVersion": 2}' > ~/.noel/extensions/rust-host.noel-extension.json
```

It registers a `word_count` tool, a `/stats` command, and a gate that
blocks writes under `/etc` — the same surface the TypeScript shim offers,
from Rust. Shell hosts (any executable) work the same way; see the tests
in `crates/noel-core/src/extensions.rs`, including one running Rust,
TypeScript, and shell extensions side by side in a single session.

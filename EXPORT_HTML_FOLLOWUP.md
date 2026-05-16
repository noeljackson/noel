# HTML Export Follow-Up

HTML export is deferred from Phase 14.

The TypeScript implementation is not a small template port. It includes a large browser-side renderer, theme variable derivation, built-in tool rendering, ANSI-to-HTML conversion, and vendored Markdown/highlight assets under `packages/coding-agent/src/core/export-html/`. Porting it faithfully in Go would require either embedding the TS browser renderer and matching the session entry shape exactly, or reimplementing the renderer and all tool transcript states.

Phase 14 adds JSONL import/export and leaves `/export-html` as an explicit deferred command so the CLI surface is discoverable without inventing a different HTML format.

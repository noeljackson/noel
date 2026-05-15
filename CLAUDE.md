# pi — Go rewrite working context

## The goal

This branch (`go-rewrite`) is a **complete rewrite** of the TypeScript `main` branch into Go. Not a v0.1. Not a "minimum viable harness." A full port that matches `main` in capability, stability, and performance.

**Speed is not the priority. Completeness is.**

Anything that exists in `main` and is reachable from the terminal harness gets ported. The TS code in `packages/` is the source of truth and behavioral reference — read it before re-implementing any feature in Go.

## Hard scope

In scope (must be ported, faithfully):

- `packages/agent/` — agent loop, message types, compaction, session machinery, branching.
- `packages/coding-agent/` — every built-in tool, session storage details, permissions/approval gates, subagent/task support, dynamic resources, every behavior reachable from `pi`-the-CLI.
- `packages/tui/` — **the reference TUI is the bar** (~11k LOC, 14 files). Custom editor with undo + kill-ring + bracketed paste, autocomplete with fuzzy matching, anchor-based layout engine, image rendering, terminal capability detection. Bubble Tea is the foundation; we will write significant code on top of it to match the TS UX, including potentially replacing bubbles' textarea with a custom editor component.
- `packages/ai/` — provider abstraction adequate for at least the Anthropic + OAuth path. Add other providers only when they map cheaply.

Out of scope (already decided in #1):

- `packages/web-ui/` and any browser surface.
- Public extension API for third parties. Personal extensions live in this source tree, statically linked.
- npm publishing or anything that needs npm to build/install.

## Working rhythm

- **Read the TS first**, every time. Don't infer Go shapes from prose alone — the TS has years of behavior baked in.
- **No "v0.1 caveats" for missing TS-baseline features.** If `main` has it and a user touches that path, the Go version must too. If a port is genuinely deferred (Phase 6 MCP is the example) it gets its own follow-up issue with an explicit reason.
- **Visible TUI artifacts are P0.** Render glitches, escaped raw text, missing widgets — all bugs, not polish.
- **Performance baseline:** TUI cold start under 50ms, sub-second incremental `go build`, no perceptible lag during streaming. If Go feels slower than TS at any user-facing surface, that's a regression.
- **No co-author / "Generated with" trailers** in commits, PRs, issues, or any artifact.
- **Tests are not optional** for non-trivial business logic. `internal/agent/loop.go`, `internal/anthropic/client.go` streaming translation, and the TUI Update loop all need real unit tests, not just smoke runs.

## Build / dev

- Module: `github.com/noeljackson/pi`. Stdlib + a small set of well-pinned deps. No npm-in-Go-build crutches.
- `make help` shows the current dev targets.
- Sessions persist to `~/.pi/sessions/<id>.jsonl` per the Phase 3 design.
- Auth: `ANTHROPIC_API_KEY` env → API key; else `~/.claude/.credentials.json` → OAuth bearer. `claude login` refreshes the OAuth file.

## Where to find things

- TS reference: `packages/*` (read-only — do not edit).
- Go rewrite: `cmd/pi`, `cmd/pi-tui-demo`, `internal/{agent,anthropic,session,tools,tui,compaction}`.
- Tracking: GitHub issue #1, PR #2.

# Development Rules

## Style

- Keep answers short and technical.
- No emojis in commits, issues, PR comments, docs, or code.
- Answer direct questions before making changes.

## Rust Workflow

- This repository is Rust-only.
- Do not add npm, Node.js, TypeScript, Vite, React, or web UI code.
- Do not run `npm install`.
- Do not run npm scripts as a validation path.

After code changes, run:

```bash
make check
```

For TTY behavior changes, also run:

```bash
make e2e
```

## Architecture

- Keep durable session state separate from reloadable systems.
- Reload must not clear conversation messages, cwd, session identity, tool history, queued messages, or active context.
- Provider tests must use faux/local providers, not paid APIs or real credentials.
- File tools must not wipe unrelated user changes.

## Git

- Never use `git add -A` or `git add .`.
- Add only files changed in the current session.
- Never use `git reset --hard`, `git checkout .`, `git clean -fd`, or `git stash`.
- Commit only files changed in the current session.
- Do not use `git commit --no-verify`.

## GitHub Issues

Use `pkg:*` labels when creating issues:

- `pkg:agent`
- `pkg:ai`
- `pkg:coding-agent`
- `pkg:tui`
- `pkg:web-ui`

The `pkg:web-ui` label remains only for historical/cutover issues. The active product no longer includes a web UI.

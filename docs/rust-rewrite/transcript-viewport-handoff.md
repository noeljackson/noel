# Transcript viewport execution handoff

## Scope / issues

- #71: owned transcript viewport, fixed composer, logical reading anchors.
- #72: wheel/PageUp/PageDown, bottom hint/click, Esc precedence, mouse lifecycle.
- #73: local live TTY regression coverage, docs, inline compatibility.
- Existing umbrella: #65. Upstream TS fixtures must remain unchanged.

## Compact plan

1. Default interactive mode uses alternate screen; `--no-alt-screen` retains the existing native inline scrollback implementation.
2. Keep transcript entries and all runtime/session state intact. Add independent ephemeral viewport state (`Latest` or `Reading(entry, logical line, source byte)`). Reuse styled grapheme-aware positioned wrapping; cache entry layouts instead of formatting all session history every frame.
3. Reserve transcript hint space independently from composer/tasks/queue/footer. Wheel/PageUp/PageDown change only transcript position; Up/Down keep their editor semantics. Reading never follows new deltas. Esc dismisses overlays first, then jumps to latest while reading without touching drafts/attachments/turn control; existing behavior at latest remains.
4. Enable mouse capture only in owned mode. A terminal modifier permits native selection; `/copy` and `/export` remain explicit copy paths. Restore capture, paste, keyboard enhancement, raw mode, and alternate screen on exit.
5. Add owned-mode live tmux tests, explicitly run native scrollback regression with compatibility mode, document non-parity, then run `make check` and `make e2e`.
6. Update issues with evidence. Do not commit or close issues as shipped without explicit publication.

## Code map / safeguards

- `crates/pi-cli/src/main.rs`: interactive event loop ~2577, terminal restoration ~2788, TuiApp ~3015, positioned wrap/archive ~3560, draw/layout ~4064, keys ~5024, streaming events ~5451, hotkeys ~7747, unit tests ~9000+.
- `scripts/e2e-tmux.sh` calls `scripts/e2e-scrollback.sh`; current native scrollback tests are valuable and should be retained in inline mode.
- `pi-config::TerminalSettings` currently only configures hardware cursor; use a CLI compatibility flag rather than gratuitous config/schema changes.
- Worktree already had substantial uncommitted user/earlier-session changes. Baseline diff and copies of touched files saved under `target/transcript-viewport-handoff/`. Do not wipe/revert/commit unrelated work; no broad staging.
- Rust only, no host npm. Faux/local providers only. Follow AGENTS.md.

## Progress

Implemented locally for #71, #72, and #73. Issues updated with validation;
left open pending publication/review. No commits or broad staging.

- Default owned alternate screen plus `--no-alt-screen` native compatibility.
- `crates/pi-cli/src/transcript_view.rs`: cached layouts, logical reading anchors,
  frozen live thinking previews, follow/page/scroll/hint state, eight focused tests.
- `main.rs`: mode setup/restoration, fixed transcript layout, wheel/click dispatch,
  idle/streaming Esc precedence, no shell-scrollback purge on owned `/clear`.
- `scripts/e2e-transcript.sh`: live faux-provider owned-mode assertions, all-row
  navigation, resize/reload/drafts, selector Esc, and terminal/shell restoration.
- Native e2e explicitly uses `--no-alt-screen`; main tmux suite tests owned mode.
  Short-screen task count assertion updated for the extra available screen rows;
  image follow-up test now polls persistence instead of racing a fixed sleep.
- README/non-parity register explain mode selection and selection/copy paths.
- Long native dogfood supports `PI_DOGFOOD_LINE_DELAY` for debug builds.

Validation passed: `make check`, `make e2e`, `git diff --check`, shell syntax,
and `PI_DOGFOOD_LINE_DELAY=0.5 PI_BIN=target/debug/pi scripts/dogfood-long-tty.sh`.
Logs remain under `target/transcript-viewport-handoff/`. No paid providers used;
upstream TypeScript parity fixtures unchanged.

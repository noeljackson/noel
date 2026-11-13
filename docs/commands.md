# Interactive Commands


- `/help`
- `/clear`
- `/settings`
- `/settings show`
- `/status`
- `/diagnostics`
- `/hotkeys`
- `/complete <prefix>`
- `/history`
- `/editor [text]`
- `/image <path> [prompt]`
- `/image-models [search]`
- `/generate-image <output> <prompt>`
- `/skills`
- `/skill:<name> [arguments]` (see [Skills](configuration.md#skills))
- `/prompts`
- `/<template> [arguments]` (see [Prompt templates](configuration.md#prompt-templates))
- `/prompt <name> [input]`
- `/themes`
- `/theme <name>`
- `/accent <color|auto>`
- `/queue [prompt]`
- `/queue-clear`
- `/interrupt`
- `/models`
- `/scoped-models`
- `/selector <kind>`
- `/select <kind> <query>`
- `/model <provider/id>`
- `/thinking <level>`
- `/multiline`
- `/session`
- `/changelog`
- `/new`
- `/resume` (no argument opens the interactive session picker: topic
  preview, relative age, message count, model, and an unfinished-turn
  marker for sessions that died mid-turn; type to filter, enter to resume,
  esc to cancel)
- `/resume [id|name|path]`
- `/fork [id|name|path]`
- `/clone [id|name|path]`
- `/tree`
- `/summaries`
- `/delete <id|name|path>` (bare `/delete` shows usage; it never deletes)
- `/name [name]` (bare `/name` shows the current name)
- `/labels <labels...>`
- `/export [file]` (default `noel-session-<session id>.html` in the session cwd)
- `/import <file>` (a noel export or journal, a pi JSONL export, or a Claude Code transcript)
- `/import claude [session]` (a Claude Code session of this directory: alone it lists them to pick from, newest first; `session` is a number in that list, an id or an id prefix)
- `/copy`
- `/share [file]`
- `/compact [focus]` (model-generated checkpoint, retaining complete recent turns)
- `/undo` (undo the last turn and its file changes; its prompt goes back to the draft)
- `/bg [prompt]` (run a prompt in a background fork of this session, also `& <prompt>`; list the forks)
- `/runs` (each run's tokens, request count, and estimated cost at catalog prices)
- `/cost` (a forecast: context size, recent average, queued work priced)
- `/rewind` (the checkpoint picker: go back to any earlier prompt, with or without the file changes since; also Esc-Esc at an empty draft)
- `/sessions` (list the server's sessions, as `noel client ls` does)
- `/plan` (toggle plan mode: read-only research, then approve the plan to have it implemented)
- `/goal [condition|clear]` (set the session goal and start on it; show it, or stop early)
- `/usage` (recorded session token totals and account quotas)
- `/login [provider]`
- `/logout <provider>`
- `/account [name]`
- `/accounts`
- `/todos`
- `/diff`
- `/bug [description]` (writes a local `noel-bug-report-<id>.md` with environment,
  model, auth type, redacted settings and session errors (URL secrets, `Bearer`
  and `Basic` credentials, and API-key-shaped tokens redacted), never the
  transcript, and prints a link for filing it; nothing is uploaded)
- `/reload`
- `/web-search <query>`
- `/web-fetch <url>`
- `/read <path>`
- `/write <path> <text>`
- `/edit <path> <find> <replace>`
- `/grep <pattern> [path]`
- `/find <glob>`
- `/ls [path]`
- `/bash <command>`
- `! <command>`
- `!!`
- `/quit`

`/reload` reloads config, prompts, context files, model metadata, keybindings, provider availability, and tool definitions without clearing the current session state. It keeps the session's project trust decision and command-line overrides (such as `--system-prompt`, `-e`, `--tools`, and `--api-key`), as do `/models refresh` and `/models reset`. RPC mode has no reload: it keeps the config startup built.

The editor uses a rounded box with a `>` prompt. A labeled Todo section and
queued-message previews sit above it, separated by muted rules. Drafts wrap
and the box grows upward with newlines or wrapping, up to ten visible input
rows; longer drafts scroll vertically to keep the cursor visible. Shift+Enter
inserts a newline without submitting (also while streaming); Enter submits or
queues the complete draft. Ctrl+J is an alternative when the terminal does not
report Shift+Enter distinctly. A draft that starts with `/` shows the
slash-command list under the input, as upstream's autocomplete: one row per
match with its description in a muted second column, an accent-colored arrow
on the selected row, Up/Down to move the selection (wrapping, and taking
precedence over history recall while the list is visible), Tab to accept it,
and Enter to accept it unless the draft is already the exact command, in which
case Enter runs it. Accepting fills the command head and a space, ready for
arguments; typing more narrows the list and a filter with no matches says so.
Multiline pastes (or single-line pastes over
1,000 characters) collapse to numbered markers such as
`[paste #1 +500 lines]`. Left/Right skip a marker and
Backspace/Delete remove the whole paste. Submission, queueing, history, and
external editors retain the full text, not the marker. Narrow/short terminals
reduce panel rows and report the actual hidden counts.

The model can track multi-step work with the built-in `todo` tool: it maintains a checklist (`pending`/`in_progress`/`completed`) that is journaled with the session and survives resume. The list renders above the prompt: collapsed lists select active tasks, upcoming tasks, and the most recent completed item while preserving task order. Completed titles are muted and struck through; hidden counts describe only hidden tasks. `ctrl+t` expands/collapses the list and survives updates; `/todos` reprints it in the transcript. Files the model changes with the `edit`/`write` tools are tracked per session; `/diff` toggles a right-side panel listing them with git change stats, and Enter on a file shows its uncommitted diff (new files show their content).

Collapsed thinking and tool blocks use at most three terminal rows (excluding the blank separator). Thinking shows the latest two wrapped lines while streaming, then the first two lines plus a clipped hidden-line/`ctrl+o` hint when finished. Tool headers stay on one row with long commands/paths clipped; short results remain visible, while longer results show a hidden-line count in the header and an ellipsis-prefixed preview of the final output line. `ctrl+o` expands/collapses thinking and tool/command output for the latest three user turns, including while a turn is running; a new prompt or shell command restores collapsed previews. Expansion restores full commands and output. This only changes display, not stored content or model context usage; `expand` is configurable in `keybindings.json`. Tool headers highlight only tool names (or the command phrase), with muted paths/counts and success/failure markers.

Assistant markdown renders GFM pipe tables as an aligned grid: a bold header row, a rule, and one row per data line, each column aligned as its delimiter row asks (`:---` left, `:--:` center, `---:` right), with `\|` escapes kept literal. A column wider than 28 cells truncates with an ellipsis so a table stays one line per row; cells keep their inline emphasis, and selection still copies the raw markdown source.

Queued follow-ups are visible immediately above the editor, one truncated line per message (the newest four when the queue overflows). Up with an empty editor recalls the newest queued message for editing (otherwise prompt-history navigation is unchanged). Mid-turn `ctrl+s` steers queued messages followed by the draft at the next provider boundary (a prose-only response has no interior boundary, so steering is delivered after it finishes and the queue hint says so); the existing steering mode still controls batch size. Queue edits are journaled during streaming and survive reload/resume.

Interactive assistant responses stream text as provider deltas arrive. While a response streams, a footer spinner shows the current phase (waiting, thinking, writing, or the running tool) with elapsed time, ctrl+c interrupts the turn unless transcript text is selected (then it copies); Esc interrupts only at the transcript bottom without a selection, Enter queues the draft as a follow-up, Up recalls the newest pending follow-up for editing, and ctrl+s steers the running turn (including promoting queued drafts): everything queued joins the conversation before the next provider request, combined into one user message so a burst of steering reads as a single instruction rather than several interruptions. Slash commands typed mid-turn still execute: `/queue` lists pending follow-ups, `/queue-clear` clears them, `/interrupt` and `/quit` act on the turn. A skill command or a prompt template typed mid-turn is queued expanded: as a follow-up with Enter, as steering with ctrl+s. `/queue <prompt>` adds follow-up prompts that run after the next assistant turn. Completed turns drain queued prose as one combined turn (queued tool commands such as `/read` still dispatch individually because their arguments are positional), including prompts queued during a follow-up, so nothing waits for a manual resubmission. Interruption, quit, or failure stops automatic dispatch and preserves remaining work. At idle, `/interrupt` clears queued follow-ups, and `!`/`!!` execute shell commands without adding them to the conversation context. Shell commands (`!`, `/bash`, and the model's `bash` tool) are non-interactive: stdin is empty and the command runs in its own session without a controlling terminal, so a program that prompts for input fails instead of waiting. A command that times out or whose turn is interrupted is killed together with its whole process group, and a timeout returns the output so far; background jobs started by a command that finished normally keep running, and the command returns without waiting for them. `!` and `/bash` commands time out after 120 seconds; the model's `bash` calls have no timeout unless the model sets one. Editing keys follow common agentic-CLI conventions: shift+enter or ctrl+j inserts a newline, ctrl+g opens the draft in an external editor (`NOEL_EDITOR_COMMAND`, `VISUAL`, or `EDITOR`), ctrl+c with a draft clears it, and ctrl+c/ctrl+d on an empty input require a second press to quit. Manual and automatic compaction persist summary records, and forked or cloned sessions persist branch summaries. Editor state tracks history, undo, kill-ring, and slash completions; restored session user prompts repopulate prompt history.

The TUI defaults to an owned alternate-screen transcript with a fixed composer. Mouse wheel and PageUp/PageDown scroll only the conversation; Up/Down retain prompt-history/queue navigation. At the bottom, new output follows automatically. Scrolling up anchors the reader to logical text: streaming, resizing, panel changes, and reload do not steal the reading position. `↓ Scroll to bottom · Esc` appears while reading (with `new output` when text arrives); click it, press Esc, or scroll down to resume following. Live thinking-tail previews are frozen while reading; returning to latest refreshes them. Esc dismisses an open selector/diff panel first, otherwise returns to latest without clearing the draft/attachments or interrupting generation. At latest it retains the existing clear/interrupt behavior. Bracketed paste inserts pasted text into the prompt. Image inputs are encoded as provider attachments with terminal text fallback.

In owned mode, drag with the left mouse button to select transcript text;
release copies it to the clipboard. Selection stays attached to logical text
while scrolling, resizing, and receiving streamed output. Drag past the transcript
edge to scroll, or use the wheel/PageUp/PageDown. Soft wraps copy as one line;
folded blocks copy only their visible preview (expand with Ctrl+O first to copy
all output; preview selection clears if the preview changes). Ctrl+C or `/copy`
copies the selection again without clearing the draft or interrupting generation. Esc clears selection and returns to latest;
clicking the composer clears selection without changing the draft.

Clipboard writes use `pbcopy` on macOS, `wl-copy`/`xclip`/`xsel` on Linux, or
`clip.exe` on Windows. `NOEL_CLIPBOARD_COMMAND` overrides the helper. If local
helpers are unavailable, Noel requests the terminal clipboard via OSC 52 (useful
over SSH; terminal/tmux clipboard permissions must allow it). An OSC 52 request
cannot confirm delivery. Copy errors appear in the transcript hint; selection
remains available for retry. Without a selection, `/copy` copies the last assistant
message. Terminal-native selection via Shift+drag (sometimes Option on macOS)
remains available; `/export <file>` exports the conversation.

Run `noel --no-alt-screen` (or upstream's `--tui-mode regular`) for native inline
compatibility: overflow is written once to terminal scrollback, mouse
selection/wheel remain terminal-owned, and the composer scrolls with the
terminal. `--tui-mode fullscreen` selects the owned screen explicitly.
`--use-theme <name>` applies a theme for one run without saving it; it cannot
be combined with `--theme` or `--no-themes`.
Print/JSON/RPC modes are unchanged.
Owned mode restores the previous screen, mouse reporting, paste, and raw mode
on exit. This viewport is an intentional Rust UI divergence; upstream parity
fixtures are not rewritten.

SIGTERM and SIGHUP end noel in every mode, as does SIGINT outside the TUI (print,
JSON, RPC, and login); in the TUI ctrl+c stays a key and an outside SIGINT is
ignored. noel first kills running tool commands with their process groups and
restores the terminal, then exits like upstream pi: with status 128 + the
signal number for SIGTERM and SIGHUP, and by the signal itself for SIGINT, so
ctrl+c also stops a shell loop that runs noel.

Ctrl+V reads a clipboard **image** into the current draft; type a prompt and
press Enter to send it. Image-only drafts use “Describe this image.” Esc or
Ctrl+C clears the draft when idle unless transcript text is selected (Esc first
clears selection/returns to bottom if scrolled up). Multiple images are supported. `/image <path>`
without a prompt also stages an attachment. Bracketed text paste is unchanged.
Images respect `images.blockImages` and `images.autoResize`. While a turn runs,
image drafts are kept for submission after it finishes (not sent as text-only
steering/follow-ups). Linux needs `wl-paste` (Wayland) or `xclip` (X11); macOS
uses AppKit via Swift (Xcode command-line tools), and Windows uses PowerShell.
For remote/headless sessions, `NOEL_CLIPBOARD_IMAGE_COMMAND` can supply encoded
PNG/JPEG/GIF/WebP bytes on stdout; otherwise use `/image <path>`.

`/undo` and `/rewind` are one mechanism. Every user prompt is a checkpoint, and
going back to one drops the messages after it and hands its prompt, images
included, back to the draft. `/undo` goes back to the last prompt and restores
the files that turn changed. `/rewind` (or Esc pressed twice within 400 ms at
an empty draft, while no run is active) opens a picker with one row per past
prompt, newest first under `(current)`, each naming the files the turns from
it on changed. When there are any, picking a row asks what to restore:
`conversation and files`, or `conversation only`, which leaves the files as
they are; a later rewind to an earlier prompt can still take those changes
back.

Restoring files works from snapshots: every `edit`/`write` (the model's or a
typed command) first saves the file's bytes into a `*.files` directory beside
the session journal, and afterwards records what it left. A rewind writes each
changed file back to what it held before the dropped turns and deletes the
files they created, through the workspace the tools ran in. A file that no
longer holds what the session's last edit left there (changed outside the
session) is not touched and is named. Neither command reverts shell effects,
tool audit history, queued messages, runs, or token usage, and a rewind does
not reach past a compaction. The journal stays append-only: a restart loads
the rewound state while nothing is erased from the file, and the history a
client shows keeps the dropped entries above a `reset to N messages` line.

A journaled session's system prompt tells the model other noel sessions
exist and how to reach them from its bash tool: `noel client ls --json`
(idle/busy state), `noel client send <id> <text>` (with `--steer` or
`--follow-up`), `noel client watch <id>`. The model can delegate
independent work to another session this way — the other session sees
only what is sent. `/sessions` lists them for you; a subagent (`task`
tool) is the same-session, scoped-tools alternative for decomposition.

`& <prompt>` (or `/bg <prompt>`) starts a background run: the session is
forked as it is and the prompt is sent to the fork, which the server runs in
a worker of its own, so it carries on whatever this session does next and
after the terminal is closed. `/bg` lists this session's forks with their last
run's state; `/resume <id>` opens one and `noel client watch <id>` follows it.
Both need the session to be idle.

`/plan` toggles plan mode (the footer says `plan`): the system prompt tells
the model to research with read-only tools, `bash` for inspection only, and
present a concrete plan, and the `edit`/`write` tools are refused outright,
so planning cannot change files. When a plan-mode turn settles, a choice
opens: `approve the plan and implement it` leaves plan mode and starts the
implementation; `keep planning`, or Esc, stays in the mode. The mode is
journaled, so a restarted session keeps it, and a goal set under it waits
for the plan to be approved.

`/goal <condition>` sets a goal that keeps the session working until a
separate evaluator confirms it is met: a condition verifiable from the
conversation alone ("all tests in `test/auth` pass", "every call site
migrated"), since the evaluator reads the transcript and cannot run commands.
Setting one starts the work. After each turn settles, one no-tools request to
the session's model answers `MET` or `NOT MET` with a sentence why; not met
sends a continuation automatically, met clears the goal, and each step is
noted. The session's worker runs this, so it goes on after the terminal is
closed. Interrupting pauses the goal (it stays; the next prompt resumes it;
`/goal clear` stops early), and 20 turns without a met condition clears it
instead of looping. Bare `/goal` shows the condition, its turn count, and the
last evaluation. Goals are journaled, so a restarted session keeps its goal.

`/usage` persists provider-reported token totals
for completed requests across resume/reload/compaction/undo/rewind; old sessions and
providers without usage report “no provider token usage reported yet” rather
than estimated totals. Account quota checks use the same cache as `/accounts`
and are skipped offline. `/usage` shows tokens, not dollars.

`/runs` estimates each run's cost at the active model's catalog list prices
(per million tokens, `free` for unpriced models), with a session total; the
runs' tokens are journaled (`run_usage` records), so they survive a restart.
Cache discounts and price tiers are not counted. `/cost` forecasts the next
work: the context estimate, the average cost of the last five runs, and the
queued follow-ups priced at that average.

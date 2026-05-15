# pi: TS -> Go rewrite gap

This branch (`go-rewrite`) is a complete Go port of the TypeScript terminal harness, not the minimal v0.1 harness described in GitHub issue #1. This inventory compares the in-scope TypeScript surface against the current Go implementation at commit `29dfcecbccb4ff35cd26af959d20156064e18749`.

## Methodology

Walked `packages/agent/src/`, `packages/coding-agent/src/`, `packages/tui/src/`, and `packages/ai/src/` file by file at commit `29dfcecbccb4ff35cd26af959d20156064e18749`; classified each file/behavior against `cmd/pi`, `cmd/pi-tui-demo`, and `internal/{agent,anthropic,compaction,session,tools,tui}`. GitHub issue #1 was read as historical context only; `CLAUDE.md` defines the current complete-port scope.

## packages/agent

### Agent loop & turns

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/agent/src/agent-loop.ts` | `agentLoop`, `agentLoopContinue`, `runAgentLoop`, `runAgentLoopContinue`, `runLoop` with `agent_start`, `turn_start`, `message_start`, `message_update`, `message_end`, `tool_execution_*`, `turn_end`, `agent_end` | Basic `Run`, `Continue`, turn loop, provider stream, tool loop in `internal/agent/loop.go`; event structs in `internal/agent/event.go`. Missing `EventStream` result semantics, queued steering/follow-up/next-turn behavior, `prepareNextTurn`, `shouldStopAfterTurn`, and stop on `AgentToolResult.terminate`. |
| 🟡 Partial | `packages/agent/src/agent-loop.ts` | Parallel tool execution with sequential override when any tool has `executionMode: "sequential"`; ordered tool-result messages after parallel completion | `internal/agent/loop.go` runs parallel-safe tools in goroutines and sequential tools inline. Missing TS source-order result emission guarantees, `beforeToolCall`, `afterToolCall`, TypeBox validation, `prepareArguments`, and batch termination. |
| ❌ Not started | `packages/agent/src/agent.ts` | `Agent` class, public `prompt`, `continue`, `retry`, `steer`, `followUp`, queue modes, subscriptions, abort/wait-for-idle state, `streamingMessage`, `pendingToolCalls` | No stateful `Agent` class equivalent; `cmd/pi/main.go` owns a small queue for TUI submissions only. |
| ❌ Not started | `packages/agent/src/proxy.ts` | Agent proxy surface for forwarding events and state | No Go equivalent. |
| ❌ Not started | `packages/agent/src/harness/agent-harness.ts` | `AgentHarness` orchestration around session, resources, hooks, provider payload hooks, queues, compaction, tree navigation, model/thinking/tool mutations | No Go equivalent. Pieces exist separately in `cmd/pi/main.go`, `internal/session`, and `internal/compaction` but not the harness contract. |

### Message types & content blocks

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/agent/src/types.ts`, `packages/ai/src/types.ts` | `UserMessage`, `AssistantMessage`, `ToolResultMessage`, `TextContent`, `ThinkingContent`, `ImageContent`, `ToolCall`, `Usage`, `StopReason`, timestamps/provider/api/model metadata | Go has `UserMessage`, `AssistantMessage`, `ToolResultMessage`, `TextContent`, `ThinkingContent`, `ImageContent`, `ToolUseContent`, `Usage` in `internal/agent/types.go`. Missing timestamps, `api`, `provider`, `responseId`, `responseModel`, diagnostics, `errorMessage`, cost fields, `totalTokens`, TS stop-reason normalization. |
| 🟡 Partial | `packages/agent/src/harness/messages.ts` | `BashExecutionMessage`, `CustomMessage`, `BranchSummaryMessage`, `CompactionSummaryMessage`, `convertToLlm`, `bashExecutionToText` | Go persists generic compaction records and messages in `internal/session`, but no custom agent-message roles, no `bashExecution` context conversion, no `custom_message` rendering/context conversion, no branch-summary message injection. |
| ❌ Not started | `packages/agent/src/harness/prompt-templates.ts`, `packages/agent/src/harness/skills.ts`, `packages/agent/src/harness/system-prompt.ts` | Skill XML formatting, prompt template argument substitution, system-prompt resource insertion | No Go equivalent. |

### Session storage

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/agent/src/harness/session/session.ts` | `Session`, `buildSessionContext`, `appendMessage`, `appendThinkingLevelChange`, `appendModelChange`, `appendCompaction`, `appendCustomEntry`, `appendCustomMessageEntry`, `appendLabel`, `appendSessionName`, `moveTo` | Go has `Session`, `AppendMessage`, `AppendRunState`, `AppendCompactionRecord`, `AppendSavePoint`, `Messages`, `LastMessage` in `internal/session/session.go`. Missing model/thinking changes, labels, custom entries, custom messages, session name, tree `moveTo`, and context rebuild from arbitrary leaf. |
| 🟡 Partial | `packages/agent/src/harness/session/storage/jsonl.ts` | v3 JSONL header, `leafId`, entry id generation, partial-line tolerance, label cache, `getPathToRoot`, `findEntries`, session metadata | Go has JSONL `Record`, `SessionHeader`, partial-line tolerance, sidecar current turn, list/open/create in `internal/session/{record,jsonl,session,restore,message_payload}.go`. Missing v3 entry schema parity, `leafId` tree storage, labels, parent session import metadata, `getPathToRoot`. |
| 🟡 Partial | `packages/agent/src/harness/session/repo/jsonl.ts`, `repo/memory.ts`, `repo/shared.ts`, `storage/memory.ts`, `uuid.ts` | Session repositories, fork/list/delete, in-memory storage, UUID helpers | Go has `JSONLStore.Create/Open/List` in `internal/session/jsonl.go`; no fork/delete repo API, in-memory session backend, or UUID-compatible id format. |

### Compaction

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/agent/src/harness/compaction/compaction.ts` | `DEFAULT_COMPACTION_SETTINGS`, `estimateContextTokens` from last assistant usage plus trailing estimate, `shouldCompact`, `findCutPoint`, split-turn summaries, `prepareCompaction`, `compact`, previous-summary update, file-op details | Go has `Settings`, `ContextEstimator`, `FindCutPoint`, `Compactor.ShouldCompact`, `Compactor.Compact`, `MaybeCompact`, serialization, and session record helpers in `internal/compaction/*.go`. Missing TS usage-based estimate, exact cut algorithm, split-turn prefix summary, previous-summary update prompt, file-op extraction/details, hook override points. |
| 🟡 Partial | `packages/agent/src/harness/compaction/utils.ts` | `serializeConversation`, `extractFileOpsFromMessage`, `computeFileLists`, `formatFileOperations`, summary prompts | Go has `serializeConversation`, `serializeToolUse`, `serializeToolResults` in `internal/compaction/serialize.go`. Missing file-operation tracking and exact TS serialization rules. |
| ❌ Not started | `packages/agent/src/harness/compaction/branch-summarization.ts` | `collectEntriesForBranchSummary`, `generateBranchSummary`, read/modified file extraction, branch summary prompts | No Go branch-summary implementation. |

### Branching / session tree

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/agent/src/harness/session/session.ts`, `packages/agent/src/harness/agent-harness.ts` | Parent-linked session tree, `moveTo`, fork before/at selected entry, branch summaries, labels, editor text extraction from selected user/custom message | Go JSONL records have `ParentID` fields, but no leaf/tree navigation, branch summary, fork semantics, labels, or selected-message editor restore. |

### Other harness internals

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/agent/src/harness/env/nodejs.ts`, `execution-env.ts`, `utils/shell-output.ts`, `utils/truncate.ts` | `ExecutionEnv` abstraction with fallible `Result`, filesystem/process error codes, shell output classification, truncation helpers | Go tools directly use OS APIs in `internal/tools/*`; no reusable execution environment contract or shared TS truncation semantics. |
| ❌ Not started | `packages/agent/src/harness/types.ts` | Harness events: `queue_update`, `save_point`, `abort`, `settled`, `before_agent_start`, `context`, provider hooks, tool hooks, `session_before_compact`, `session_tree`, model/thinking/resource events | Go has only agent loop events in `internal/agent/event.go`; no harness event layer. |
| 🚫 Out of scope | `packages/agent/src/index.ts` package exports | Public npm package boundary | npm publishing and public package API are out of scope per `CLAUDE.md` and issue #1. |

## packages/coding-agent

### Built-in tools

| Tool | Status | TS reference | Behavior | Go status |
|---|---|---|---|---|
| bash | 🟡 Partial | `packages/coding-agent/src/core/tools/bash.ts`, `bash-executor.ts`, `output-accumulator.ts` | `createBashToolDefinition`, timeout seconds, cwd validation, shell hook, streamed stdout/stderr updates, `OutputAccumulator`, tail truncation to `DEFAULT_MAX_LINES`/`DEFAULT_MAX_BYTES`, full output temp file, render states, user bash messages | Go has `bash.Tool` with `command`, `timeout_ms`, `cwd`, process-group cleanup, streaming partial chunks, exit code, truncation in `internal/tools/bash/bash.go`. Missing seconds-compatible schema (`timeout`), full-output temp file, shell hook, detailed stdout/stderr separation/render details, user `!`/`!!` bash session message behavior. |
| read | 🟡 Partial | `core/tools/read.ts`, `utils/image-*`, `utils/mime.ts`, `utils/exif-orientation.ts` | Text read with offset/limit, truncation, continuation hints, image read as base64 attachments for jpg/png/gif/webp, auto-resize/orientation, non-vision model notes, compact `.pi` docs classification, syntax-highlighted render | Go has text `ReadTool` with path/offset/limit and oversize check in `internal/tools/file/read.go`. Missing image attachments, resize/orientation, MIME detection, continuation parity, syntax highlighting, compact docs classification. |
| write | 🟡 Partial | `core/tools/write.ts` | `createWriteToolDefinition`, parent creation, existing-file overwrite, atomic write through mutation queue, bytes/lines details, highlighted preview rendering | Go has atomic temp-file rename and mutation lock in `internal/tools/file/write.go` plus `mutqueue.go`. Missing TS details shape, render preview/highlight cache, exact error messages. |
| edit | 🟡 Partial | `core/tools/edit.ts`, `edit-diff.ts` | Multi-edit schema `edits[{oldText,newText,replaceAll}]`, legacy `{old_string,new_string}` argument prep, fuzzy whitespace matching, duplicate/not-found/no-change errors, BOM and CRLF preservation, unified diff preview | Go has single-edit schema `{path,old_string,new_string,replace_all}`, BOM/line-ending preservation, occurrence checks in `internal/tools/file/edit.go`. Missing multi-edit batching, legacy/new schema parity, fuzzy matching, diff generation/preview, no-change error parity. |
| grep | 🟡 Partial | `core/tools/grep.ts` | rg JSON parsing, regex/literal, `ignoreCase`, `context`, `limit`, `glob`, long-line truncation, files/count/content modes, render summaries | Go has `pattern`, `path`, `glob`, `type`, `output_mode`, `head_limit`, rg fallback walk in `internal/tools/file/grep.go`. Missing `ignoreCase`, literal mode, context lines, TS `limit` semantics, rg JSON detail parity, long-line truncation shape. |
| find | 🟡 Partial | `core/tools/find.ts` | Glob file search via `fd`, default limit 1000, `.gitignore` respect, path normalization, truncation details | Go has `FindTool` with `pattern`, `path`, `type`, fd fallback walk in `internal/tools/file/find.go`. Missing `limit`, exact glob semantics and TS truncation details. |
| ls | 🟡 Partial | `core/tools/ls.ts` | Directory list, dotfiles included, alpha sort, `/` suffix, `limit`, truncation and notices | Go has list with ignore patterns and sort in `internal/tools/file/ls.go`. Missing `limit`, TS details/notices shape, exact hidden/error rendering. |
| todo | ❌ Not started | No core tool file; related user-facing surface expected by rewrite scope, and extension examples may define todo-like behavior | No Go tool under `internal/tools/todo`. |
| task/subagent | ❌ Not started | `packages/coding-agent/examples/extensions/subagent/*` for TS extension-shaped reference; `packages/ai/src/providers/anthropic.ts` also maps Claude Code `Task` naming | No Go `task` or subagent tool. Public extension API compatibility is out of scope, but a statically linked task/subagent behavior remains in scope if reachable from the CLI. |
| more | ❌ Not started | No `packages/coding-agent/src/core/tools/more.ts` found; rewrite scope names it as expected terminal-harness behavior | No Go equivalent. Need source-of-truth clarification before implementation. |
| registry | 🟡 Partial | `core/tools/index.ts`, `tool-definition-wrapper.ts` | `allToolNames`, coding/read-only tool sets, `ToolDefinition` render wrappers, TypeBox parameters | Go has `Registry` and explicit built-in registration in `internal/tools/registry.go` and `cmd/pi/main.go`. Missing read-only/coding sets, render wrappers, TypeBox validation/coercion. |

### Session machinery

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `core/session-manager.ts` | Session create/open/list/listAll/continueRecent/fork/import/export, path by cwd, in-memory sessions, session stats, labels, name, tree, context build | Go has JSONL create/open/list and latest-ish resume by id in `internal/session/jsonl.go` and `cmd/pi/main.go`. Missing cwd-scoped discovery parity, global list, import/export, fork, labels/name/tree/stats. |
| 🟡 Partial | `core/agent-session.ts` | Main CLI session object: prompt/steer/followUp queues, auto-compaction, retry, bash execution, model/thinking changes, tool activation, resource reload, slash command helpers, HTML/JSONL export | Go has thin CLI/TUI glue in `cmd/pi/main.go` and `internal/tui/app.go`. Missing most `AgentSession` API and state machine. |
| ❌ Not started | `core/agent-session-runtime.ts`, `agent-session-services.ts`, `sdk.ts` | Runtime replacement for new/resume/fork/import, extension shutdown/switch/fork hooks, service creation, SDK surface | No Go equivalent. SDK/public API is mostly out of scope, but runtime session switching reachable from CLI is in scope. |

### Permissions / approval gates

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `core/extensions/runner.ts`, `core/extensions/types.ts`, `core/agent-session.ts` | `tool_call` hook can block, `tool_result` hook can patch, extension approval dialogs, project-local subagent confirmation, config-driven tools allowlist | Go has no approval policy; tools execute once emitted by the model. |
| 🟡 Partial | `cli/args.ts`, `core/sdk.ts` | `--no-tools`, `--no-builtin-tools`, `--tools` allowlist | Go has no matching CLI flags; `cmd/pi/main.go` always registers current built-ins. |

### Subagent / task tool

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/coding-agent/examples/extensions/subagent/index.ts`, `agents.ts`, `README.md` | Single/parallel/chain delegation, `agentScope`, project-agent confirmation, subprocess `pi --mode json`, concurrency limits, progress updates, task result details/rendering | No Go subagent/task implementation. TS extension API is out of scope, but a static Go task tool is needed for equivalent CLI capability. |

### Dynamic resources / dynamic tools

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `core/resource-loader.ts` | `DefaultResourceLoader`: discovers extensions, skills, prompt templates, themes, AGENTS/CLAUDE context files; collision diagnostics; resource reload | No Go resource loader. `CLAUDE.md` was manually read for this task; the binary does not load it dynamically. |
| ❌ Not started | `core/skills.ts`, `core/prompt-templates.ts` | Skill validation/frontmatter, `.pi`/`.agents` discovery, ignore rules, prompt template parsing/substitution/expansion | No Go equivalent. |
| ❌ Not started | `core/extensions/{loader,runner,wrapper,types,index}.ts` | Dynamic TS extension loader, tools, commands, event handlers, custom UI, message renderers, command context, input handlers | Public compatible extension API is out of scope, but any personal/static tools or behaviors reachable from the CLI need Go-native replacements. |
| ❌ Not started | `core/package-manager.ts`, `package-manager-cli.ts` | install/remove/update/list for extensions/skills/prompts/themes and npm/git sources | Out of complete terminal-harness behavior only if package management is no longer desired; npm publishing is out of scope but local resource install UX is undecided. |

### Hooks system (which hooks exist, what they do, which port)

| Hook/event | TS reference | Effect | Go status |
|---|---|---|---|
| `before_agent_start` | `agent-harness.ts`, `extensions/runner.ts` | Inject messages or override system prompt before prompt run | ❌ Not started. |
| `context` | `agent-harness.ts`, `extensions/runner.ts` | Transform messages before provider request | ❌ Not started. |
| `before_provider_request` | `agent-harness.ts`, `sdk.ts`, `extensions/runner.ts` | Patch stream options, headers, metadata | ❌ Not started. |
| `before_provider_payload` | `agent-harness.ts` | Inspect/replace raw provider payload | ❌ Not started. |
| `after_provider_response` | `agent-harness.ts`, `sdk.ts` | Observe HTTP status/headers | ❌ Not started. |
| `tool_call` | `agent-loop.ts`, `agent-harness.ts`, `extensions/runner.ts` | Block tool call with reason | ❌ Not started. |
| `tool_result` | `agent-loop.ts`, `agent-harness.ts`, `extensions/runner.ts` | Patch result content/details/isError/terminate | ❌ Not started. |
| `message_end` | `extensions/runner.ts` | Rewrite completed message preserving role | ❌ Not started. |
| `session_before_compact`, `session_compact` | `agent-harness.ts`, `extensions/runner.ts` | Cancel/override compaction and observe result | ❌ Not started. |
| `session_before_tree`, `session_tree` | `agent-harness.ts`, `extensions/runner.ts` | Cancel/override tree navigation summary | ❌ Not started. |
| `session_start`, `session_shutdown`, `session_before_switch`, `session_before_fork` | `agent-session-runtime.ts`, `extensions/runner.ts` | Runtime lifecycle around session replacement | ❌ Not started. |
| `resources_discover`, `resources_update` | `resource-loader.ts`, `extensions/runner.ts`, `agent-harness.ts` | Dynamic skills/prompts/themes/extensions discovery | ❌ Not started. |
| `input`, `user_bash` | `extensions/runner.ts`, `interactive-mode.ts` | Intercept input or shell command | ❌ Not started. |

### Slash commands

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `core/slash-commands.ts`, `modes/interactive/interactive-mode.ts` | Built-ins: `/settings`, `/model`, `/scoped-models`, `/export`, `/import`, `/share`, `/copy`, `/name`, `/session`, `/changelog`, `/hotkeys`, `/fork`, `/clone`, `/tree`, `/login`, `/logout`, `/new`, `/compact`, `/resume`, `/reload`, `/quit` | No slash command parser in Go TUI. Only prompt submit, queue, abort/quit, page up/down, clear are implemented in `internal/tui/app.go`. |
| ❌ Not started | `interactive-mode.ts` | `!command` and `!!command` user bash commands with optional context exclusion | No Go interactive bash shortcut. |
| ❌ Not started | `core/extensions/runner.ts`, `core/prompt-templates.ts`, `core/skills.ts` | Extension commands, prompt-template commands, `skill:<name>` commands | No Go equivalent. |

### Anything reachable from the CLI that isn't covered above

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `main.ts`, `cli/args.ts` | Modes: interactive, print, json, rpc; stdin/file args; `--continue`, `--resume`, `--session`, `--fork`, `--session-dir`, model/provider/thinking flags, tool flags, resources flags, export/list-models/offline/verbose | Go `cmd/pi/main.go` supports minimal headless prompt, `--tui`, `--resume`, model env, and TUI. Missing most CLI flags/modes and argument compatibility. |
| ❌ Not started | `modes/print-mode.ts`, `modes/rpc/*` | Text/json print mode and JSONL RPC mode/client | Go headless prints basic events; no JSON output or RPC protocol. |
| ❌ Not started | `core/export-html/*` | HTML transcript export with ANSI/tool rendering/templates | No Go equivalent. Issue #1 listed HTML export as v0.1 non-goal, but complete CLI parity needs an explicit decision. |
| ❌ Not started | `core/auth-storage.ts`, `model-registry.ts`, `model-resolver.ts`, `provider-display-names.ts`, `auth-guidance.ts` | `auth.json`, OAuth login/logout, env/model config precedence, scoped models, provider display names, auth guidance | Go has `ANTHROPIC_API_KEY` or `~/.claude/.credentials.json` in `internal/anthropic/auth.go`; no `auth.json`, `/login`, model registry, or scoped models. |
| ❌ Not started | `core/settings-manager.ts`, `core/keybindings.ts`, theme files | Settings JSON, keybinding reload/migrations, theme selection, quiet startup, auto-compact toggles, tree filters | No Go settings manager; `internal/tui/keys.go` has fixed Bubble Tea keys. |
| ❌ Not started | `utils/clipboard*`, `clipboard-image.ts`, `image-convert.ts`, `image-resize.ts`, `syntax-highlight.ts`, `fs-watch.ts`, `version-check.ts`, `git.ts`, `frontmatter.ts`, `html.ts`, `shell.ts` | Clipboard text/images, image conversion, syntax highlighting, file watch, update checks, git helpers, frontmatter, shell utilities | No Go equivalents except direct shell/file code in tools. |

## packages/tui

### Editor

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/tui/src/components/editor.ts`, `editor-component.ts` | Custom multi-line editor with grapheme cursor movement, visual-line wrapping, sticky visual column, history, jump-to-char, slash/file autocomplete, large paste markers, undo coalescing, kill ring, yank-pop, bracketed paste handling | Go uses `bubbles/textarea` in `internal/tui/app.go`; no custom editor behavior. |
| ❌ Not started | `packages/tui/src/kill-ring.ts`, `undo-stack.ts` | Emacs kill/yank ring and clone-on-push undo stack | No Go equivalent. |

### Autocomplete + fuzzy matching

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/tui/src/autocomplete.ts`, `fuzzy.ts` | `CombinedAutocompleteProvider`, slash command completions, argument completions, `@` file completion, quoted/tilde paths, fd-backed fuzzy file search, alphanumeric swapped fuzzy matching | No Go autocomplete or fuzzy matching. |

### Keybindings

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/tui/src/keybindings.ts`, `keys.ts`; `packages/coding-agent/src/core/keybindings.ts` | Configurable `TUI_KEYBINDINGS`, app keybindings, migrations, conflicts, Kitty keyboard protocol, modifyOtherKeys, release/repeat detection, printable decoding | Go has fixed `enter`, `shift+enter/alt+enter`, `ctrl+c`, `pgup`, `pgdown`, `ctrl+l` in `internal/tui/keys.go`. Missing configurability and almost all key decoding. |

### Layout / anchor-positioning engine

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/tui/src/tui.ts`, `components/box.ts`, `spacer.ts`, `text.ts`, `truncated-text.ts` | Custom component tree, overlays, anchor/percent row/col layout, margin handling, composite overlays, differential renderer, cursor marker extraction | Go uses Bubble Tea viewport/editor/footer layout in `internal/tui/app.go`; no overlay/anchor engine. |

### Stdin buffering (bracketed paste, mouse handling)

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/tui/src/stdin-buffer.ts`, `terminal.ts`, `keys.ts` | Escape-sequence buffering, complete CSI/OSC/DCS/APC extraction, bracketed paste event, old/SGR mouse sequence handling, high-bit Alt handling | Bubble Tea handles basic input; no custom stdin buffer or mouse/paste parity. |

### Image rendering (terminal-image, kitty/iTerm protocols)

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `packages/tui/src/terminal-image.ts`, `components/image.ts` | Terminal capability detection for Kitty/iTerm2, image protocol encoding/deletion, PNG/JPEG/GIF/WebP dimension parsing, cell-size query, fallback text | No Go terminal image support. |

### Terminal capability detection

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `terminal-image.ts`, `terminal.ts`, `tui.ts`, `utils.ts` | truecolor/hyperlink/image detection, tmux/screen safeguards, cell pixel query `CSI 16 t`, OSC 8 hyperlink width-preserving wrapping, ANSI state tracking | No Go capability layer. Lipgloss colors are used directly in `internal/tui/theme.go` and components. |

### Components (assistant messages, tool cards, footer, etc.)

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/coding-agent/src/modes/interactive/components/assistant-message.ts`, `tool-execution.ts`, `bash-execution.ts`, `user-message.ts`, `footer.ts`, `diff.ts`, `branch-summary-message.ts`, `compaction-summary-message.ts`, `skill-invocation-message.ts`, selector components | Go has simple message, tool card, footer views in `internal/tui/components/{message,toolcard,footer}.go`. Missing markdown renderer, diff view, thinking toggle, bash execution component, compaction/branch/skill/custom messages, selectors, dialogs, settings/model/session/tree components. |
| ❌ Not started | `packages/tui/src/components/markdown.ts`, `select-list.ts`, `settings-list.ts`, `input.ts`, `loader.ts`, `cancellable-loader.ts`, `image.ts` | Markdown tables/lists/code/links, select/settings controls, input, loaders, image component | No Go equivalents beyond Bubble Tea textarea and simple strings. |

### Streaming render

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `interactive-mode.ts`, `packages/tui/src/tui.ts` | Incremental assistant text/thinking/tool arg streaming, tool cards update, queued messages, auto-scroll, differential redraw, raw terminal safety checks | Go handles `MessageUpdateEvent` text/thinking/tool partials and tool execution updates in `internal/tui/app.go`. Missing differential renderer, markdown streaming, thinking visibility toggle, robust width checks, selector overlays, and most TS event cases. |

## packages/ai

### Anthropic adapter

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/ai/src/providers/anthropic.ts` | Anthropic Messages streaming with custom SSE decoder, `message_start`, `content_block_start`, `content_block_delta` for `text_delta`, `thinking_delta`, `input_json_delta`, `signature_delta`, `content_block_stop`, `message_delta`, `message_stop` | Go uses `anthropic-sdk-go` streaming and accumulation in `internal/anthropic/client.go`. It emits text/thinking/input JSON deltas and final message. Missing custom SSE repair, `signature_delta` streaming preservation during partials, redacted thinking, exact event protocol, `responseId`, cost, diagnostics, stop-reason mapping. |
| 🟡 Partial | `providers/anthropic.ts` | OAuth bearer behavior: Claude Code headers, beta headers, Claude Code tool name casing (`Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`, `Task`, `TodoWrite`, etc.) | Go reads Claude credentials and sets headers in `internal/anthropic/auth.go`. Missing token refresh, login, Claude Code tool-name casing/remapping, cache/session affinity, interleaved/fine-grained beta selection parity. |
| 🟡 Partial | `providers/anthropic.ts` | Payload building: system prompt cache control, image blocks, consecutive tool results grouped into one user message, thinking adaptive/budget modes, metadata, tool choice, eager tool input streaming | Go builds basic system/messages/tools in `internal/anthropic/client.go`. Missing cache control, image content conversion, consecutive tool-result grouping parity, thinking options, metadata, tool choice, eager streaming flags. |

### Other providers

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `providers/openai-completions.ts`, `openai-responses.ts`, `openai-codex-responses.ts`, `azure-openai-responses.ts`, `mistral.ts`, `google.ts`, `google-vertex.ts`, `amazon-bedrock.ts`, `cloudflare.ts`, `github-copilot-headers.ts`, `providers/images/*` | OpenAI-compatible, Responses, Codex, Azure, Mistral, Google, Vertex, Bedrock, Cloudflare, Copilot, OpenRouter images | No Go equivalents. `CLAUDE.md` requires Anthropic + OAuth as adequate baseline; later providers are lower priority unless cheap. |
| ❌ Not started | `api-registry.ts`, `providers/register-builtins.ts`, `images-api-registry.ts`, `images.ts` | Provider registries, lazy providers, reset/unregister, image model registry | No Go provider registry; `cmd/pi/main.go` directly constructs `internal/anthropic.Client`. |

### Streaming event translation

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/ai/src/types.ts`, `utils/event-stream.ts`, `stream.ts` | `AssistantMessageEventStream` with `start`, `text_start/delta/end`, `thinking_start/delta/end`, `toolcall_start/delta/end`, terminal `done/error`, `result()` | Go streams `agent.Event` directly; no separate assistant-event stream or `result()` abstraction. |
| ❌ Not started | `providers/transform-messages.ts`, `utils/sanitize-unicode.ts`, `utils/json-parse.ts`, `utils/overflow.ts` | Cross-provider message transform, unsupported image downgrades, thinking-to-text for cross-model replay, tool-call id normalization and synthetic missing tool results, streaming JSON repair, Unicode surrogate sanitization, context overflow helpers | No Go equivalent except basic JSON unmarshal and SDK accumulation. |

### Tool schema / typebox

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/ai/src/types.ts`, `utils/validation.ts`, `utils/typebox-helpers.ts` | TypeBox `Tool.parameters`, validation cache, primitive coercion, path-aware validation errors, `validateToolCall`, `validateToolArguments` | Go tools expose raw JSON schemas in `internal/agent/tool.go`; no schema validation/coercion before execution. Individual tools unmarshal argument structs. |

## Cross-cutting concerns

### Authentication (API key / OAuth / refresh)

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/ai/src/env-api-keys.ts`, `utils/oauth/*`, `packages/coding-agent/src/core/auth-storage.ts` | Env var matrix, provider-specific auth, `auth.json`, OAuth login/refresh/logout, Anthropic callback server + PKCE, GitHub Copilot/OpenAI Codex OAuth | Go supports `ANTHROPIC_API_KEY` then `~/.claude/.credentials.json` bearer in `internal/anthropic/auth.go`. Missing refresh/login/logout, `auth.json`, provider matrix, env var matrix. |

### Logging / observability

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `core/diagnostics.ts`, `core/timings.ts`, `core/telemetry.ts`, `output-guard.ts`, `tui.ts` debug logs | Startup diagnostics, extension/resource errors, timings, stdout guard, redraw/crash logs | Go imports `log/slog` in `internal/agent/loop.go` for tool errors but has no structured diagnostic/timing/logging surface. |

### Configuration (`$PI_HOME`, settings files, env vars)

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `packages/coding-agent/src/config.ts`, `settings-manager.ts`, `model-registry.ts`, `resource-loader.ts` | `PI_AGENT_DIR`, `PI_SESSION_DIR`, settings JSON, models JSON, keybindings JSON, themes, context files, package manager metadata, env var docs | Go hardcodes sessions under `~/.pi/sessions` in `cmd/pi/main.go`/`internal/session`; model from `PI_MODEL` or default. Missing `PI_AGENT_DIR`/`PI_SESSION_DIR` parity and settings/models/resources. |

### Hot-reload story

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| 🟡 Partial | `CLAUDE.md`, issue #1 design, TS `/reload` in `interactive-mode.ts` | TS reloads resources/keybindings/themes; Go design relies on process restart plus durable JSONL/current-turn sidecar | Go has session JSONL and throttled current-turn sidecar in `internal/session/session.go`; no `/reload`, resource reload, or interrupted-turn user policy beyond detection helpers. |

### Test coverage gaps

| Status | Area | Go coverage |
|---|---|---|
| ✅ Ported | Basic JSONL, current-turn sidecar, interrupted-turn detection | `internal/session/*_test.go`. |
| ✅ Ported | Basic compaction estimator/cutpoint/record integration | `internal/compaction/*_test.go`. |
| ✅ Ported | Basic bash/read/write/edit and TUI component smoke | `internal/tools/**/*_test.go`, `internal/tui/components/message_test.go`, `internal/tui/mock/source_test.go`. |
| ❌ Not started | Agent loop parity, Anthropic streaming edge cases, TUI update loop, key decoding, autocomplete, branch/session tree, settings/resource loading | No equivalent tests. `CLAUDE.md` explicitly calls for real tests around `internal/agent/loop.go`, `internal/anthropic/client.go`, and TUI update loop. |

### Performance baseline

| Status | TS reference | Behavior | Go status |
|---|---|---|---|
| ❌ Not started | `CLAUDE.md`, `packages/tui/src/tui.ts` | TUI cold start under 50ms, sub-second incremental build, no streaming lag, no terminal redraw regressions | No benchmark or measurement harness. Go TUI is simpler but not yet measured against the stated baseline. |

## Phase assignment (proposed)

1. **Agent state + loop parity**  
   Scope: port `Agent` queue/state semantics from `packages/agent/src/agent.ts` and close gaps in `agent-loop.ts`. Success: `prompt`, `continue`, `retry`, `steer`, `followUp`, queue modes, `prepareNextTurn`, `shouldStopAfterTurn`, `terminate` semantics, and event ordering match TS in unit tests. Estimate: 1200-1800 LOC. Risks: queue behavior interacts with TUI and session writes.

2. **Message/session schema parity**  
   Scope: extend `internal/agent/types.go` and `internal/session` for timestamps, provider/api/model metadata, stop reasons, model/thinking changes, custom entries/messages, labels, session names. Reference: `packages/agent/src/types.ts`, `harness/messages.ts`, `harness/session/*`. Success: Go can round-trip all in-scope TS session entry kinds except extension-only payloads. Estimate: 1000-1600 LOC. Risks: migration choice for existing Go JSONL.

3. **Session tree + fork/navigation**  
   Scope: leaf id, path-to-root, fork before/at selected entry, branch summaries, label editing. Reference: `harness/session/session.ts`, `agent-harness.ts`, `core/session-manager.ts`, `interactive-mode.ts` tree/fork handlers. Success: `/fork`, `/clone`, `/tree` equivalents can navigate and resume context correctly. Estimate: 1500-2200 LOC. Risks: branch summary requires provider call and UI selector work.

4. **Anthropic adapter fidelity**  
   Scope: stop-reason mapping, response metadata, cost/usage, redacted thinking, signature deltas, cache control, grouped tool results, image blocks, thinking modes, OAuth Claude Code headers/tool casing. Reference: `packages/ai/src/providers/anthropic.ts`, `types.ts`, `transform-messages.ts`. Success: golden stream tests cover `input_json_delta`, `signature_delta`, redacted thinking, tool-use replay, OAuth payloads. Estimate: 1400-2000 LOC. Risks: Go SDK may hide low-level SSE cases; custom decoder may be needed.

5. **Tool contract + validation**  
   Scope: JSON schema validation/coercion, per-tool details parity, result termination, read-only/coding tool sets, TypeBox-equivalent schemas as JSON. Reference: `packages/ai/src/utils/validation.ts`, `packages/coding-agent/src/core/tools/index.ts`. Success: invalid tool args fail before execution with path-aware errors; tool registry can activate/allowlist tools. Estimate: 700-1200 LOC. Risks: schema validation library choice.

6. **File/shell tool parity**  
   Scope: close bash/read/write/edit/grep/find/ls gaps, especially read images, edit multi-edit/fuzzy diff, grep literal/context/ignoreCase, find/ls limits, output temp files. Reference: `packages/coding-agent/src/core/tools/*.ts`. Success: per-tool regression tests mirror TS edge cases. Estimate: 1800-2600 LOC. Risks: image conversion dependencies and exact rg/fd behavior.

7. **CLI modes + flags**  
   Scope: parse TS-compatible flags and implement print/json/rpc enough for terminal harness users. Reference: `main.ts`, `cli/args.ts`, `modes/print-mode.ts`, `modes/rpc/*`. Success: common invocations with `--continue`, `--resume`, `--session`, `--fork`, `--tools`, `--thinking`, `--model`, `--print`, `--mode json` work. Estimate: 1200-2000 LOC. Risks: flags depend on model registry/settings.

8. **Settings, auth storage, model registry**  
   Scope: `auth.json`, `/login`/`/logout` API-key path, settings, models, scoped model resolution, provider display names, env var precedence. Reference: `auth-storage.ts`, `model-registry.ts`, `model-resolver.ts`, `settings-manager.ts`. Success: settings survive restart; scoped model cycling and Anthropic OAuth/API-key login work. Estimate: 1600-2400 LOC. Risks: OAuth refresh/login flow and model catalog update policy.

9. **Resource loader: context files, skills, prompts, themes**  
   Scope: AGENTS/CLAUDE discovery, skill loading/formatting, prompt templates, theme loading, reload diagnostics. Reference: `resource-loader.ts`, `skills.ts`, `prompt-templates.ts`, `system-prompt.ts`, theme files. Success: system prompt includes context files/resources and `/reload` refreshes them. Estimate: 1400-2200 LOC. Risks: deciding `.pi`/`.agents` compatibility without public extension API.

10. **TUI custom editor, keybindings, autocomplete**  
    Scope: replace `bubbles/textarea` with Go editor matching `packages/tui/src/components/editor.ts`, keybinding manager, key decoder, fuzzy/file/slash autocomplete. Success: undo, kill-ring, bracketed paste markers, configurable keybindings, slash/file completion pass behavioral tests. Estimate: 2200-3500 LOC. Risks: keyboard protocol parity is deep and platform-sensitive.

11. **TUI layout/components/render parity**  
    Scope: markdown, tool cards, diff, selectors/dialogs, footer, thinking toggle, queue UI, terminal-image support, terminal capability detection, overlay/anchor layout or equivalent Bubble Tea implementation. Reference: `packages/tui/src/*`, `modes/interactive/components/*`, `interactive-mode.ts`. Success: all slash-command UIs and streaming renders are usable without raw escape artifacts. Estimate: 2500-4500 LOC. Risks: image protocols and differential redraw correctness.

12. **Compaction + auto-retry parity**  
    Scope: TS estimate/cut/split-turn algorithm, file-op details, manual/auto compaction, cancel, queued messages during compaction, overflow retry. Reference: `core/compaction/*`, `agent-session.ts`, `interactive-mode.ts`. Success: context overflow triggers compaction and resumes queued work correctly. Estimate: 1000-1700 LOC. Risks: compaction depends on model registry/auth and session tree.

13. **Static task/todo/more tools**  
    Scope: implement Go-native static tools for task/subagent/todo/more as required by complete terminal-harness scope; do not port public TS extension API. Reference: subagent behavior in `packages/coding-agent/examples/extensions/subagent/*`; source for todo/more needs confirmation. Success: task delegation works with isolated sessions and permission prompts; todo/more behavior is documented and tested. Estimate: 1500-3000 LOC. Risks: incomplete TS source-of-truth for todo/more in current tree.

14. **Observability, import/export, performance gates**  
    Scope: diagnostics, timing, logs, JSONL import/export, optional HTML export decision, startup/build/redraw benchmarks. Reference: `diagnostics.ts`, `timings.ts`, `export-html/*`, `version-check.ts`, `CLAUDE.md`. Success: benchmarks enforce cold start/build/render targets; diagnostics are visible in CLI/TUI. Estimate: 900-1800 LOC. Risks: HTML export may be explicitly deferred if terminal-only scope excludes it.

## Out of scope (cross-reference)

- `packages/web-ui/` and browser surfaces: excluded by `CLAUDE.md` and issue #1.
- Public extension API compatibility: excluded by `CLAUDE.md` and issue #1. Static personal tools/resources may still be ported as Go code.
- npm publishing/package workspace API stability: excluded by `CLAUDE.md` and issue #1.
- Browser image rendering/share viewer: excluded unless `/share` or HTML export is re-scoped for CLI parity.
- TypeScript examples as public ABI: excluded. Use them only as behavioral references for in-scope static features such as task/subagent.

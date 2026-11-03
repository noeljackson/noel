# Codemode port specification

Port of upstream's `codemode` tool to noel, with scripts running natively on djs.

Sources (read-only):

- pi at `7dc38ff`: `cm/` is `packages/codemode`, `ca/` is `packages/coding-agent`, and `du/` is
  `packages/durable`.
- djs at `16b7438`.

Citations are `file:line` at those commits. "Unverified" marks claims not checked by reading the
code or running it.

Upstream has two codemode tools on one sandbox package:

- `ca/src/extensions/codemode/*` is the coding agent's tool. noel ports the coding agent, so this
  is the reference.
- `du/src/codemode/*` is Durable's tool. It adds replay after a restart, via a journal and
  `CodemodeSandboxBase.start()`. Its output format and description are "copied from the coding
  agent's code mode" (`du/src/codemode/output.ts:2`, `describe.ts:2`).
- `defaultActive: false` exists only in the coding agent (`ca/.../codemode/index.ts:44`). Durable
  ships the tool inside its `codemode` extension (`du/src/codemode/codemode.ts:269-286`).

## 1. Script environment

### 1.1 Shape

- The script is an async function body: top-level `await` and `return` work (`cm/README.md:47`).
- It is compiled as `(async (tools, console) => {<code>\n})` in file `codemode.js`. The prefix
  shares line 1 with the script, so line numbers match and only line-1 columns shift
  (`cm/src/runtime/vm.ts:323-327`).
- Stacks read `Name: message` followed by QuickJS frames, with prelude frames dropped
  (`prelude-source.ts:164-171`).
- Engine output on fd 1/2 is discarded (`vm.ts:128-145`).

### 1.2 `tools.<id>(args)`

- **Identifier.** `toCodemodeIdentifier` (`cm/src/identifier.ts:5-12`) keeps each code point
  matching `[A-Za-z_$]`, or `[A-Za-z0-9_$]` after the first, and turns any other into `_`. ASCII
  only, so `é` becomes `_`; an empty name becomes `_`.
- **Names.** `tools` is a null-prototype object with each tool under both `jsName` and its raw
  name. On a collision the first tool wins, and only it goes into `ALL_TOOLS`. Both are frozen
  (`prelude-source.ts:213-225`; `sandbox.test.ts:306-329`).
- **Arguments.** A call sends `JSON.stringify(args[0])`; `undefined` is sent as absent, and a
  stringify error rejects. Call ids `1, 2, ...` are shared with globals (`:197-211`).
- **Unknown names.** A proxy (`:229-253`) throws a `TypeError` on reading a missing string
  member: `tools.X does not exist.`, then `Did you mean tools.a, ...?` (up to 5 matches,
  compared lower-case with `[^a-z0-9]` stripped) or `Available: ...` (at most 20 tools), then
  `ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with
  "X" in tools.` The proxy passes `in`, `then`, `toJSON` and `Object.prototype` keys through.
  Exact text: `sandbox.test.ts:384-398`.
- **Results.** Results cross as JSON. A failure rejects with `new Error(message)`. In
  deterministic runs, `properties` are copied onto that error, except `name`, `message` and
  `stack` (`:459-483`).
- **Coding-agent mapping** (`ca/.../execute.ts:480-486`):
  - A tool with an `outputSchema` resolves to its `structuredContent`, even on an error result.
    bash returns `{output,truncated,full_output_path?,exit_code,wall_time_seconds}`
    (`ca/src/core/tools/bash.ts:55-61`); read returns `string|{type:"image",data,mimeType,note}`
    (`read.ts:32-35`).
  - Any other tool resolves to its text blocks joined with `\n`, without images.
  - Errors reject with that text, or with `Tool "<name>" failed`.

### 1.3 `ALL_TOOLS`

A frozen array of frozen `{name: jsName, description}` (`prelude-source.ts:220,225`). In the
coding agent, `description` is `renderToolSample`: the description, the guidelines as `- `
bullets, and a TypeScript declaration (`execute.ts:520-527`, `tool.ts:145-156`).

### 1.4 Output

- **`text(v)`** (`prelude-source.ts:358-374`): primitives, `null` and `undefined` go through
  `String`. Objects go through `JSON.stringify`, falling back to `String` when that returns
  `undefined`. A stringify error is rethrown as a `TypeError`.
- **`console.log|info|warn|error|debug`** (`:443-449`): each argument is formatted
  (`:173-182`): strings as they are, Errors via `errorText`, everything else as JSON or
  `String`. The parts are joined with a space and marked `console: true`. `console` is frozen.
- **`image(x)`** (`:376-429`) takes a URL string, `{image_url}`, or a
  `{type:"image",data,mimeType?}` block. Block data that starts with `data:` is used as is;
  otherwise it becomes `data:;base64,<data>`. Checks, in order, each throwing a `TypeError`
  (`sandbox.test.ts:132-177`):
  1. Wrong type, array, null or empty: `image expects a non-empty image URL string, an object
     with image_url, or an image block { type: "image", data, mimeType }`.
  2. Block type is not image: `image only accepts image blocks, got "<type>"`.
  3. Empty data: `image expected image block data`.
  4. http(s) URL: `remote image URLs are not supported in tool outputs. Pass a base64 data URI
     instead`.
  5. Not `data:` or no `;base64`: `invalid image output. Pass a base64 data URI instead`.
  6. After stripping whitespace, length not a multiple of 4 or not
     `^[A-Za-z0-9+/]+={0,2}$`: `invalid image output. The image data is not valid base64
     (truncated or corrupted?)`.
  7. No signature in the first 16 characters: `invalid image output. The image data is not a
     PNG, JPEG, GIF, or WebP image`.

  Signatures (`:396-401`): PNG `^iVBORw0KGg`, JPEG `^/9j/(?!9)`, GIF `^R0lGOD[dl]h`, WebP
  `^UklG.{8}RUJQ`. The detected MIME type wins over the declared one.
- **Limits** (`:42-43,342-355`): 16 Mi characters (text plus base64) and 100000 items. Past
  either, the prelude reports a failed `done` and then throws `RangeError: script output
  exceeded the limit of 16777216 characters or 100000 text(), image(), and console calls. Print
  a summary instead, or write large data to a file with a tool.` Catching it does not help
  (`sandbox.test.ts:629-656`). Output after `done` is dropped.

### 1.5 `exit()`

`exit()` (`prelude-source.ts:431-441`) reports `done(ok, undefined, writes)`, then throws a
frozen sentinel. Output and writes made before it are kept. A script that catches the sentinel
runs on, but changes nothing (`sandbox.test.ts:194-210`).

### 1.6 `store` / `load`

- **Model** (`prelude-source.ts:273-335`, `README.md:80-92`): both are synchronous, over a
  key-to-JSON snapshot of `options.store`. The host drops `undefined` values from the snapshot
  (`execution.ts:88-95`).
- **Errors:** a non-string key throws `TypeError: store() key must be a string`, or `load() ...`.
  A value that cannot be stringified, or stringifies to `undefined`, throws a `TypeError`.
- **Deletes and reads:** `store(k, undefined)` deletes the key. `load` returns a fresh parse.
- **Limits:** a value over 262144 characters of JSON throws `RangeError: store("k") value has N
  characters of JSON, more than the limit of 262144. <HINT>`. A total of keys plus JSON over
  1048576 throws `RangeError: store is full: stored values would exceed 1048576 characters of
  JSON. Delete keys with store(key, undefined). <HINT>`. `<HINT>` is `store() is for small state
  such as IDs or summaries. Show images with image(), keep large data in variables, or write it
  to a file with a tool.`
- **Writes:** the last state of each touched key, reported only on success (return or `exit()`)
  as `[[key,json]|[key]]`. The host returns them as `storeWrites{set,delete}`
  (`execution.ts:112-129`).
- **Persistence, coding agent:** a successful script with writes appends one custom entry,
  `customType:"codemode-store"`, `data:{set,delete}` (`ca/.../tool.ts:42`,
  `execute.ts:618-622`). `load` folds those entries along the branch from the root and skips
  malformed ones (`execute.ts:293-303`), so the store is per branch. Without a session the store
  starts empty and writes are dropped.
- **Persistence, Durable:** a `pi.codemode.store` document per conversation, rewindable,
  `fork: asOf` (`du/.../codemode.ts:178-186`), committed together with the outcome (`:572-582`).

### 1.7 `// @options:` (`cm/src/source.ts:11-125`)

- **Position:** first line only, after leading whitespace is trimmed. An options line anywhere
  else throws `The // @options: line must be the first line of the input; found one on line N`.
- **Fields:** `max_output_tokens` (a non-negative safe integer) and `timeout_ms` (1 to
  2147483647).
- **Rejected,** each with an exact message in `source.ts`: empty input, invalid JSON, a
  non-object, an unknown field (``@options only supports `max_output_tokens` and `timeout_ms`;
  got `x` ``), and an options line with no code after it.
- **Line numbers:** the options line is replaced by an empty line, so they stay the same.
- **Grammar:** `CODEMODE_SOURCE_GRAMMAR` (`source.ts:22-30`) is an OpenAI Lark grammar; noel can
  skip it.
- **Errors:** in the coding agent the parse error is thrown, so the result is the bare message
  with no header (`agent-session-codemode.test.ts:530-555`).

### 1.8 Globals and `spread`

- **Calls:** globals are top-level host functions. They behave like tools but are not in
  `calls` (`types.ts:115-120`).
- **Namespaces:** `a.b` names go on a frozen, guarded `a` object (`prelude-source.ts:255-271`).
- **`spread: true`:** the function gets every argument as one array, with `undefined` sent as
  `null` (`sandbox.test.ts:494-511`).
- **Names** (`execution.ts:51-61,152-167`): an identifier or `ns.member`. Reserved: `tools`,
  `ALL_TOOLS`, `console`, `text`, `image`, `exit`, `globalThis`, `store`, `load`. No duplicates,
  and no namespace clashes.
- **Coding-agent globals:** always the spread globals `searchTools(query,{limit?,namespace?})`
  (BM25), `describeTool(name)` and `describeNamespace(name)` (`execute.ts:669-735`). With
  `models` (the default, `index.ts:37`), also `models.getModelsOfType`, `getAvailableOfType`,
  `getModelOfType`, `classify` and `generateImages`: at most 4 at once, each shown as a nested
  row with its cost (`execute.ts:742-850`).

### 1.9 Absent

No timers, `fetch`, `process`, `require`, modules or `WebAssembly`. `eval` and `Function` stay
inside the VM, and `import()` rejects (`README.md:56`, `sandbox.test.ts:686-734`).

### 1.10 Prelude and lockdown

`PRELUDE_SOURCE` (`prelude-source.ts:48-522`) is
`(bridge, toolsJson, globalsJson, storeJson) => {settle, run, stalled}`.

- **Lockdown** (`:58-139`) runs first. It freezes everything reachable from the globals plus the
  iterator, generator, async and TypedArray intrinsics, and makes the built-in globals
  read-only and non-configurable.
- **Override mistake:** writable data properties of `Object.prototype`, plus `constructor`,
  `name`, `message`, `toString`, `toLocaleString`, `valueOf` and `toJSON`, become accessors.
  Their setters define an own property on the instance, and throw on the built-in itself.
- **Captured early:** `JSON`, `Promise.prototype.then` and the Error constructors. Script
  globals are then defined as enumerable, non-writable properties.
- **`bridge(kind,a,b,c)`** takes primitives only: `("call"|"global", id, name, argsJson?)`,
  `("output", "text"|"console", text)`, `("output", "image", data, mime)`, and
  `("done", ok, valueJson|errorJson, writesJson?)`.
- **`run(fn)`** calls `fn(toolsProxy, console)`. A return value that cannot be serialized (for
  example `10n`) is a `script` `TypeError`.
- **`stalled()`** runs after every drain (`:509-520`). If the script is not finished and no call
  (tool or global) is pending, it fails with `Error: The script is waiting on a promise that can
  never settle: no tool call is pending, and timers do not exist here.`
- **`describeError`** (`:186-195`) coerces `name` and `message` to strings. A thrown non-Error
  becomes `{message: format(v)}`. The fallback is `The script threw a value that cannot be
  described`.

## 2. Result, errors, limits

- **Result** (`cm/src/types.ts:94-109`): `{ok:true,value,output,calls,storeWrites}` or
  `{ok:false,error{kind,name?,message,stack?},output,calls}`. Output made before a failure is
  kept.
- **Items:** `{type:"text",text,console?:true}` or `{type:"image",data,mimeType}`.
- **`calls`:** tool calls only, in order, as `{name,status:ok|error|cancelled,durationMs}`
  (`execution.ts:378-408`). Each starts `cancelled`; `finish()` aborts the pending ones, which
  stay `cancelled`.

| kind | when (`types.ts:59-73`, `README.md:347-354`) |
| --- | --- |
| `script` | throw, parse error, stall, output limit, or a return value that cannot be serialized |
| `timeout` | `Execution timed out after N ms` (`execution.ts:214-218`); interrupt budget (inline/remote) |
| `aborted` | signal reason message or `Execution aborted` (`:305-308`); `Sandbox closed` |
| `sandbox` | trap or crash (`Execution interrupted`), launcher failure, `Sandbox bridge broken: ...` (`:316-328`) |

Defaults:

- **Sandbox timeout:** 300000 ms (`execution.ts:49`).
- **Coding-agent deadline:** it passes `timeout_ms ?? Infinity` (`execute.ts:555,592`), so **by
  default there is no deadline** and only an abort stops a script.
- **Durable deadline:** none unless `timeout_ms` is set (`du/.../codemode.ts:472,488`).
- **Memory:** unlimited in the sandbox (`types.ts:128-131`); the coding agent sets 256 MiB
  (`execute.ts:56`). Running out gives a catchable `InternalError: out of memory`.
- **Stack:** a QuickJS stack guard of 512 KiB makes deep recursion a catchable `RangeError`.
- **Workers:** the coding agent runs at most 2 per process (`PI_CODEMODE_MAX_WORKERS`) and waits
  up to 60 s for one. Aborted while waiting, the result says "The script did not start."
  (`execute.ts:58-111,557-576`).
- **Stall:** a promise nothing can settle fails at once (`sandbox.test.ts:565-579`). Returning
  while a call is pending is not a stall; the call is cancelled.

## 3. The codemode tool

### 3.1 Registration

- **Name** `codemode`. **Parameters** `{code: string}`, described as "Raw JavaScript source."
  (`ca/.../tool.ts:78-82`). Durable's schema is `open: true` (`du/.../codemode.ts:227`).
- **No recursion:** exposure is `model-only`, and the callable set drops `codemode` by name
  (`tool.ts:159-161,405-406`). Durable uses `callers:["model"]` and `replay:"safe"`
  (`codemode.ts:260-261`).
- **Enabling:** `defaultActive: false` (`index.ts:44`). It turns on with `--tools codemode`,
  `defaultTools: ["+codemode"]` (`ca/docs/settings.md:46-52`), `setActiveTools()`, or the MCP
  extension when MCP tools are script-only (`index.ts:5-7`).
- **Settings:** `codemode.mode` is `"on"` or `"only"`; `codemode.inlineBudget` defaults to 3000
  (`settings-defaults.ts:57-60`).
- **Prompt** (`tool.ts:108-113`): snippet "Run JavaScript that calls other tools", plus the
  guideline "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or
  filter large output, instead of many separate calls."

### 3.2 Description

The description is `DESCRIPTION_INTRO` (`tool.ts:118-120`) followed by `Globals:` lines
(`:123-134`).

- **Catalog** (`:228-270`): a `Nested tools:` section. Each tool gets `### \`id\`` (or
  `### \`id\` (\`raw\`)`) and its sample. Tools without a namespace come first, then namespaces
  by name. Tools are placed round-robin, cheapest first, within `inlineBudget` (chars / 4).
  Partial groups are marked `(some tools not listed)` or `(tools not listed)`. Deferred tools
  are never listed.
- **Modes** (`prepareLoadout`, `:343-379`):
  - `on` lists only non-direct callable tools, and appends `Codemode: \`tools.<id>(args)\`
    resolves to <...>.` to each declared callable tool (`:301-329`).
  - `only` lists every callable tool and hides the active direct tools' declarations.
- **Durable** adds "The script survives a restart..." and moves the catalog into a system-prompt
  section (`du/src/codemode/describe.ts:29-48,118-157`).

### 3.3 Callable set

- **Coding agent** (`ca/src/core/agent-session.ts:1598-1604`): active `direct` tools plus every
  `codemode` or `deferred` tool. Never `model-only` tools or `codemode` itself. The set is
  captured when the script starts (`execute.ts:519`).
- **Durable:** `agent.callable`, minus the internal `pi.codemode.*` tools (`codemode.ts:389-396`).

### 3.4 Nested calls

- **Pipeline:** `ctx.executeTool` (`ca/src/core/extensions/types.ts:396-415`) goes through
  `NestedToolCallRunner` (`ca/src/core/nested-tool-calls.ts:161-262`), with id `<parent>/<n>`
  (`:189`). Calls run through `runToolCall` with the session's `beforeToolCall` and
  `afterToolCall` plus `parentToolCallId` (`agent-session.ts:746-778`). Validation, the
  `tool_call` hook (blocking makes the script reject with the reason) and the `tool_result`
  rewrite all apply (`agent-session-codemode.test.ts:294-342`).
- **Events:** `tool_execution_*` events carry `parentToolCallId`.
- **Concurrency:** calls run in parallel unless the agent runs tools sequentially or the tool is
  `executionMode:"sequential"` (`:202-212`).
- **Transcript:** nested calls never become transcript results. The codemode result message gets
  `nestedCalls{calls:[{id,name,arguments?|argumentsBytes,status:ok|error|unfinished,durationMs,
  error}],complete}`, capped at 256 calls, 8 KiB of arguments per call and 32 KiB in total, and
  500 characters of error (`nested-tool-calls.ts:26-31`, `agent-session.ts:1123-1134`,
  `ai/src/types.ts:626-645`). Nested usage is added to the message's usage. Compaction reads
  `nestedCalls` to track file operations (`ca/src/core/compaction/utils.ts:33`).
- **`details.calls`** (`tool.ts:86-106`, `execute.ts:525-552`): rows of
  `{id,name,args(200),status:running|ok|error|cancelled,durationMs,error(500),cost?}`,
  published through `onUpdate` on every change. A row still `running` at the end, or an error
  after an abort, becomes `cancelled`.
- **Renderer** (`renderer.ts`): no separate rows. The codemode row lists the calls (`…` `✓` `✗`
  `⊘`, name, args cut to 80 characters, duration, cost), then the output without its header.
- **Durable:** `api.executeTool(...,{key:"c<id>"})`, id `<callId>/c<id>` (`codemode.ts:839`).
  Each call is its own task; `beforeTool` and `afterTool` see `call.parent`. Live progress is in
  `pi.live.nestedTools: NestedToolSlot[]`
  `{callId,parentCallId,parentTaskId,taskId,name,arguments,status,summary?}`
  (`du/src/harness/live.ts:50-69,179`; `harness/tool.ts:948-958`), removed when the parent
  settles. Results go to `NestedCallDoc` (`harness/tool.ts:117`).

### 3.5 Result text (`ca/.../execute.ts:617-651`)

1. **Value.** On success, append the store entry if there are writes. Then, if
   `value !== undefined`, push `valueText`: a string as is, anything else as compact JSON.
2. **Items** (`formatOutput`, `:322-341`). With more than one non-console text item, each starts
   with `==> text N/M <==\n`. Images keep their place. Console lines go last in one
   `<console_output>\n...\n</console_output>` item.
3. **Error.** On failure, push `Script error:\n` plus `formatError` (`:379-390`). The head is
   `stack` (or `${name??"Error"}: ${message}`) for `script`, `Script timed out: <m>` for
   `timeout`, `Script aborted: <m>` for `aborted`, and `Script sandbox failed: <m>` for
   `sandbox`. Then `\n\n` and either `No tool calls were made.` or `Tool calls made before the
   failure (they are not undone): a (ok), b (error)`.
4. **Image note.** A note when `models.generateImages()` images were never shown (`:628-633`).
5. **Join** (`joinAdjacentText`, `:348-372`). Adjacent texts merge, with `\n` added when the
   last non-empty part does not end with one.
6. **Truncate** (`truncateOutput(items, max_output_tokens ?? 10000)`, `:449-472`). The budget is
   `tokens*4` characters over the texts joined by `\n`. Over budget, the text becomes `Warning:
   truncated output (original token count: ceil(len/4))\nTotal output lines: L\n\n<head>
   …<ceil(removed/4)> tokens truncated…<tail>`, where head is `floor(budget/2)` characters and
   tail the rest. It ends with `\n\n[Full output: <path> (read with offset/limit)]`, or `[Could
   not save the full output: <err>]` if the save failed. Images follow, and the path goes to
   `details.fullOutputPath`.
7. **Images** (`saveImages`, `:414-442`). Each distinct image is saved once. Every occurrence is
   preceded by `[Image saved to <path> (<mime>, <formatSize>)]`, or `[Image (<kind>) could not
   be saved: <err>]`. Then join again.
8. **Header.** `content = [{text:"Script completed|failed\nWall time X.X seconds\nOutput:\n"},
   ...items]`, and `isError = !ok`.

Durable (`du/src/codemode/output.ts:98-152`) also caps the budget at `maxBytes/4` and by lines
(limits of 4 MiB and 100000 lines, `codemode.ts:245`). Its image labels say `N bytes`, and its
files are `output.txt` and `image-N.ext`.

### 3.6 Abort

On abort the script ends with `aborted`, the pending nested calls are aborted, and their rows
become `cancelled`. The tool still returns `Script failed ... Script aborted: <reason>`
(`execute.ts:604,613-615`). In Durable, a sandbox the host closed stores nothing and throws, so
the call replays later (`codemode.ts:500-507`).

## 4. Upstream tests as goldens

Record goldens with a Bun fixture in the style of `scripts/ts-parity/*-fixture.ts`. It runs
upstream `InlineCodemodeSandbox` (or `start()` with a fixed seed) and writes
`tests/fixtures/ts-parity/codemode.json`.

| file:lines | asserts (deterministic unless noted) |
| --- | --- |
| `cm/test/source.test.ts` (all) | options parsing and exact errors |
| `sandbox.test.ts:79-262` | return round trip; output order; `image()` errors; wrapped base64; `exit()`; partial output; `codemode.js:2` errors; stack format; non-Error throw; unserializable return |
| `sandbox.test.ts:265-398` | call records; identifiers and `ALL_TOOLS`; `undefined` becomes `null`; catchable errors; unknown tool; unawaited call is `cancelled`; close-match texts |
| `sandbox.test.ts:411-546` | store (limits, copies, writes); globals and namespaces; spread |
| `sandbox.test.ts:565-683` | stall; output limit; recursion `RangeError` |
| `sandbox.test.ts:686-809` | no host globals; frozen intrinsics and overrides; error names; frozen `tools`/`console` |
| `cm/test/deterministic.test.ts:108-289` | same calls for the same seed and results; UTC and no WeakRef; seq order; rejection properties (fingerprints are engine-specific) |
| `cm/test/declarations.test.ts` | pure renderers: description and `ALL_TOOLS` goldens |
| `ca/test/suite/agent-session-codemode.test.ts` | description per mode `144-232`; parallel nested exact text `252-292`; hooks `294-342`; image labels and dedupe `402-440`; layout `442-464`; failure summary `466-491`; options `530-555`; memory `556-567`; truncation `569-599`; bash and read values `612-644`; store and branches `646-708` |

Compare by regex or shape only: wall time, `durationMs`, temp paths, timeout and abort timing,
stack columns.

djs already runs pi's suite through its adapter: 221 of 226 pass. The failures are the
fingerprint test, the strict-mode patch test and the worker-lifetime timing
(`djs/docs/host.md:574-605`).

## 5. Mapping onto noel

### 5.1 Layout

New crate `crates/noel-codemode`:

- `source.rs` and `identifier.rs`.
- `prelude.rs`: `include_str!("prelude.js")`, byte-identical to upstream and pinned by a FNV-1a
  test, plus djs's `HOST_GLOBALS` `performance` shim (`djs/js/pi/vm.ts:146-156`).
- `vm.rs`: a port of `djs/js/pi/vm.ts`. One VM per call on its own thread. It talks only through
  messages mirroring `cm/src/runtime/protocol.ts:26-88`: `Call`, `Output`, `Done` and `Crash`
  out, `Result` in. Interrupts use an `Arc<AtomicBool>`.
- `execution.rs`: a tokio port of `execution.ts`: tools, records, output, deadline,
  cancellation (a child token per nested call), store writes, bridge errors, `checkGlobals`.
- `format.rs`: §3.5, with the spill and image writers injected.
- `describe.rs`: the description and catalog, with a `schemaToType` subset.

Dependencies:

- `djs-core` and `djs-prelude`, as git dependencies pinned by `rev`.
- They are edition 2024. That is fine from noel's edition-2021 workspace on toolchain 1.99.
- `djs-prelude`'s build script builds the base image with a host `djs-core`, so the first build
  is slower (unverified cost).
- Default features `full` and `unicode`. A slim build is a later option
  (`djs/docs/slim-codemode.md`).

`noel-core` changes:

- **Tool.** A built-in `codemode` beside `todo`: dispatch in `execute_model_tool_call`
  (`crates/noel-core/src/lib.rs:5057-5201`), definition in `model_tool_definition`
  (`lib.rs:4629`), listing in `active_tool_definitions` (`lib.rs:4595-4615`).
- **Nested executor.** Factor the per-tool branches of `execute_model_tool_call` (extension v1
  and v2, `todo`, built-ins via `execute_tool_streaming_in` in
  `crates/noel-tools/src/lib.rs:263-335`) into a function that returns `(output, images,
  is_error)`. It must not call `push_tool_result` (`lib.rs:2979-3002`) or touch `messages` or
  `tool_history`. It runs `gate_tool_call` (`noel-core/src/extensions.rs:503`) for every nested
  call, as `run_tool_call` does (`lib.rs:4150-4180`). A blocked call rejects in the script with
  the reason.
- **Store.** A custom entry `customType:"codemode-store"` for session-file parity (pattern at
  `lib.rs:2155-2175`), or a new `SessionRecord` (`lib.rs:1075`). Fold it on load; fork copies it
  (`lib.rs:1315-1335`).
- **Enabling.** `default_active: false`: on only via `--tools`, `defaultTools: ["+codemode"]` or
  `enabledTools`. Update `docs/rust-rewrite/non-parity-register.md:691-693`.

### 5.2 djs driver rules (`djs/js/pi/vm.ts`, `djs/docs/host.md:522-670`)

- **VM.** `djs_prelude::new_vm(VmOptions{memory_limit: 256 MiB, random_seed: <entropy>, ..})`
  (`djs-prelude/src/lib.rs:81`). Not `Vm::new`, which compiles djs's builtin prelude on every
  call (`vm/mod.rs:174-200`). Leave `frozen_builtins` off (pi's lockdown freezes). Use
  `WeakRefs::Absent` only in deterministic runs.
- **Setup** (`vm.ts:527-556`). `new_host_function(1,"bridge")` is a sync function, so the VM
  parks on it. Compile `HOST_GLOBALS`, then `"return "+PRELUDE`, with
  `CompileOptions::async_body("codemode-prelude.js",&[])`. Run each and read the fulfilled
  value with `promise_state`. Call the factory with the bridge and the three JSON strings, keep
  handles to `api`, `settle`, `stalled` and `run`, then `seal()`.
- **Clock.** While the prelude runs: no interrupt polling and a fixed clock (0, UTC). Before
  every later slice: `set_now(now_ms)` and `set_time_zone_offset(local)` (`vm.ts:383-396`).
- **Script.** Compile `"return (async (tools, console) => {"+code+"\n})"` with
  `async_body("codemode.js",&[])` (`vm.ts:138,628`). A `CompileError` (it has `line` and `col`,
  `compiler/mod.rs:155-162`) becomes `{name,message,stack:"Name: message\n    at
  codemode.js:L:C"}`, with the prefix length subtracted from line-1 columns (`vm.ts:202-211`).
  Evaluate the result to the function, call `run(api, fn)`, then drain.
- **Invoke loop** (`vm.ts:339-381`), slices of `run(10_000)`:
  - `OutOfFuel`: if interrupted, `abort()` and crash with `Execution interrupted`; otherwise
    continue.
  - `HostCall`: read the arguments at once (`host_call`, `heap.str_to_string`), handle the
    message, then `resolve_host_call(id, Ok(UNDEFINED))`.
  - `Done` or `Idle`: return.
  - `Threw(v)`: crash with `display(v)`. The prelude catches script errors itself.
  - `HostOutOfMemory`: crash.
- **Drain.** `run` empties the job queue (D1370). Call `stalled(api)` after every `settle` or
  `run`. To deliver a result, create values for `(id, ok, payload, properties)`, call `settle`,
  then drain.
- **GC.** A `Value` is valid only until the next `run` or collection, because objects move
  (`host.md:390-392`). Keep long-lived values in `heap_mut().new_handle(v)`
  (`heap/handle.rs:68`) and read them back with `get_handle` before each `call`. `call` itself
  never collects (`vm/mod.rs:601-604`).
- **`Vm::define`** creates non-enumerable, writable, configurable properties
  (`vm/mod.rs:541-545`). Use it only for globals defined from Rust; pi's prelude defines every
  name scripts see.
- **Async host functions.** Keep the sync bridge and prelude-owned promises, so ids, stall
  detection and ordering stay exact. `new_async_host_function` is a later option.
- **Send.** `Vm: Send` is likely (a `Vec<u8>` arena, no `Rc` found). Unverified: add a static
  assert.
- **Prelude image cache.** Optional: `image()` and `from_bytes` make starts about 10x faster
  (`host.md:563-572`).

### 5.3 What noel lacks

- **Interleaved content.** `ToolResult` is `output: String` plus `images`
  (`noel-tools/src/lib.rs:102-133`). Messages and `TurnEvent::ToolOutput` split content and media
  the same way (`lib.rs:3612-3618`). Upstream sends `[header, text, image, text]`. Add ordered
  blocks, or register the difference.
- **Nested-call events.** No `details`, `nestedCalls` or `parentToolCallId`; `TurnEvent` is
  defined at `lib.rs:3566-3634`. Add `NestedTool{parent_id,id,name,args,status,duration,error}`
  and rows for it in the TUI and RPC (`noel-cli/src/main.rs:7212-7260`, `rpc_wire.rs:1196-1230`).
- **Structured outputs.** noel's `bash` and `read` return strings, so script values differ.
  Upstream tests `612-644` depend on the structured forms.
- **Concurrency.** Model calls run sequentially under `&mut Runtime` (`lib.rs:3982-3990`).
  `Promise.all` needs an executor over shared state: env, options and
  `Arc<Mutex<ExtensionHosts>>`. Defer `note_edited_files` and todo updates until the call ends.
- **Abort.** noel drops the future and saves `aborted_output` (`lib.rs:4206-4227`). Upstream
  returns `Script aborted: <reason>`, so the codemode tool must handle cancellation itself.
- **Not present:** exposure modes, namespaces, `tool_search` and BM25, `codemode.mode` and
  `inlineBudget`, MCP, `models.*`, and durable replay. If replay is ever needed, djs
  `snapshot()`/`resumeVm` is simpler (`host.md:552-560`).

### 5.4 Plan

1. **Pin djs** with the full `rev`. Spike: VM, async body, sync host call, fuel, abort. Build for
   `aarch64-apple-ios` (#118).
2. **Source and identifier,** with goldens from `source.test.ts`.
3. **VM:** vendor the prelude; port `vm.rs`, the protocol and `execution.rs`.
4. **Goldens:** a Bun fixture `scripts/ts-parity/codemode-fixture.ts` writing
   `tests/fixtures/ts-parity/codemode.json`. The Rust test compares `ok`, `value`, `output`,
   `calls[].{name,status}`, `storeWrites` and `error.{kind,name,message}`, and checks stacks by
   `codemode.js:<line>` only.
5. **`format.rs`,** with goldens and the wall time matched by regex.
6. **The `noel-core` tool:** definition (a plain `Nested tools:` list of built-ins at first),
   nested executor with the gate, store records, `default_active: false`, abort handling.
7. **View:** nested rows in events, TUI and RPC, then `make e2e`.
8. **Structured `bash` and `read` outputs.**
9. **Catalog and discovery:** `inlineBudget` and `mode`, the discovery globals, `ALL_TOOLS`
   samples (a `declarations.ts` port).
10. **Register what is deferred:** `models.*`, MCP, durable replay, grammar sampling.

Tests, with the faux provider and no credentials:

- **`noel-codemode`:** the goldens; stall; `timeout_ms` on `while(true){}`; cancellation; output
  and memory limits; recursion `RangeError`; a prelude hash pin.
- **`noel-core`:** parallel nested calls with exact text (port of `252-292`); a gate block;
  failure summary; option errors; truncation and spill; store persistence, nothing on failure,
  resume and fork; off unless enabled; not callable from scripts.
- **e2e:** a tmux run showing the nested rows.

### 5.5 Differences on djs (register them)

- **Strict mode.** djs is always strict. Assigning to an undeclared variable throws
  `ReferenceError`, and writes to frozen built-ins throw. `sandbox.test.ts:736-748` fails
  (`host.md:595-597,609-611`).
- **Eval.** `eval` is always indirect, so `Function("return this")()` is `undefined`
  (`host.md:618-620`).
- **Interrupts.** One poll per 10,000 fuel. There is no host-stack guard; recursion fails at
  about 131,000 frames with the same `RangeError` (`host.md:549`).
- **Engine text.** Fingerprints and stack columns differ. The exact frame text on line 1 is
  unverified.
- **Heap minimum.** 512 KB for the prelude (`host.md:627-629`).

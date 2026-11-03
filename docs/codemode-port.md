# Codemode port specification

Port of upstream's `codemode` tool to noel, with scripts running natively on djs.

Sources (read-only):

- pi at `7dc38ff`. Path prefixes: `cm/` = `packages/codemode`, `ca/` = `packages/coding-agent`,
  `du/` = `packages/durable`.
- djs at `16b7438`: `js/pi/vm.ts`, `docs/host.md`, `crates/djs-core/src/vm/mod.rs`.

Citations are `file:line` at those commits. "Unverified" marks claims not checked by reading the
code or running it.

Upstream has two codemode tools on one sandbox package:

- The coding agent's tool, `ca/src/extensions/codemode/*`. noel ports the coding agent, so this
  tool is the reference for behavior and output text.
- Durable's tool, `du/src/codemode/*`. It adds restart replay: a journal plus
  `CodemodeSandboxBase.start()`. Its formatting and description are "copied from the coding
  agent's code mode" (`du/src/codemode/output.ts:2`, `describe.ts:2`).

`defaultActive: false` comes from the coding agent (`ca/.../codemode/index.ts:44`). Durable has
no such flag. Its tool ships with the `codemode` extension (`du/src/codemode/codemode.ts:269-286`).

## 1. Script environment

### 1.1 Shape

The script is an async function body, so top-level `await` and `return` work (`cm/README.md:47`).

pi compiles `(async (tools, console) => {<code>\n})` as `codemode.js`. The prefix shares line 1
with the script, so line numbers match and only columns on line 1 shift
(`cm/src/runtime/vm.ts:323-327`, `README.md:373`).

Stacks read `Name: message` followed by QuickJS frames, with the prelude's frames removed
(`prelude-source.ts:164-171`). Engine writes to fd 1/2 are discarded (`vm.ts:128-145`).

### 1.2 `tools.<id>(args)`

Identifier mapping, `toCodemodeIdentifier` (`cm/src/identifier.ts:5-12`):

- Each code point that matches `[A-Za-z_$]` (first position) or `[A-Za-z0-9_$]` (later
  positions) is kept. Any other becomes `_`.
- The check is ASCII only, so `é` becomes `_`.
- An empty result becomes `_`.

The `tools` object (`prelude-source.ts:213-225`):

- It is a null-prototype object. Each tool sits under both its `jsName` and its raw name.
- When two names collide, the first tool wins, and only it appears in `ALL_TOOLS`.
- The object and `ALL_TOOLS` are frozen (`sandbox.test.ts:306-329`).

Calls (`:197-211`):

- A call sends `JSON.stringify(args[0])`. `undefined` crosses as absent.
- A stringify error rejects the call.
- Ids run 1, 2, ..., shared with globals.

A proxy guards `tools` (`:229-253`). Reading a missing string member throws a `TypeError`
(exact text in `sandbox.test.ts:384-398`), made of:

1. `tools.X does not exist.`
2. One of these:
   - `Did you mean tools.a, ...?`, with at most 5 close matches. Names are compared lower-cased
     with `[^a-z0-9]` removed.
   - `Available: ...`, when there are at most 20 tools.
3. `ALL_TOOLS lists every tool; searchTools(query) finds tools by topic.`
4. `Check for a member with "X" in tools.`

The proxy passes `in`, `then`, `toJSON` and `Object.prototype` keys through.

Results cross as JSON and are parsed in the VM (`:459-483`):

- A failure rejects with `new Error(message)`.
- In deterministic runs it also copies the outcome's `properties` onto that error, except
  `name`, `message` and `stack`.

Nested results in the coding agent (`ca/.../execute.ts:480-486`):

- A tool with `outputSchema` resolves to its `structuredContent`, even on an error result:
  - bash gives `{output,truncated,full_output_path?,exit_code,wall_time_seconds}`
    (`ca/src/core/tools/bash.ts:55-61`);
  - read gives `string | {type:"image",data,mimeType,note}` (`read.ts:32-35`).
- Any other tool resolves to its text blocks joined by `\n`. Images are dropped.
- On error the call rejects with that text, or with `Tool "<name>" failed`.

### 1.3 `ALL_TOOLS`

`ALL_TOOLS` is a frozen array of frozen `{name: jsName, description}` (`prelude-source.ts:220,225`).

In the coding agent, `description` is `renderToolSample`: the tool's description, its prompt
guidelines as `- ` bullets, and a TypeScript declaration (`execute.ts:520-527`, `tool.ts:145-156`).

### 1.4 Output helpers

`text(v)` (`prelude-source.ts:358-374`):

- Primitives, `null` and `undefined` go through `String(v)`.
- Objects and functions go through `JSON.stringify`, falling back to `String(v)` when that
  returns `undefined`.
- A stringify error is rethrown as a `TypeError`.

`console.log|info|warn|error|debug` (`:443-449`):

- Each argument is formatted (`:173-182`): strings as they are, Errors via `errorText`,
  everything else as JSON or `String()`.
- Arguments are joined with a space. The item gets `console: true`.
- `console` itself is frozen.

`image(x)` (`:376-429`) accepts:

- a URL string;
- `{image_url}`;
- a block `{type:"image",data,mimeType?}`. Its `data` is used as is when it starts with
  `data:`, and as `data:;base64,<data>` otherwise.

It throws a `TypeError` at the first failing check, in this order (exact list in
`sandbox.test.ts:132-177`):

1. Wrong type, array, null or empty: `image expects a non-empty image URL string, an object with
   image_url, or an image block { type: "image", data, mimeType }`.
2. A block whose type is not `"image"`: `image only accepts image blocks, got "<type>"`.
3. Empty `data`: `image expected image block data`.
4. `http` or `https`: `remote image URLs are not supported in tool outputs. Pass a base64 data
   URI instead`.
5. Not `data:`, or no `;base64`: `invalid image output. Pass a base64 data URI instead`.
6. With whitespace removed, the length is not a multiple of 4 or does not match
   `^[A-Za-z0-9+/]+={0,2}$`: `invalid image output. The image data is not valid base64
   (truncated or corrupted?)`.
7. No signature matches the first 16 characters: `invalid image output. The image data is not a
   PNG, JPEG, GIF, or WebP image`.

The signatures (`:396-401`) are PNG `^iVBORw0KGg`, JPEG `^/9j/(?!9)`, GIF `^R0lGOD[dl]h` and WebP
`^UklG.{8}RUJQ`. The MIME type is the detected one; the declared one is ignored.

Output limits (`:42-43,342-355`):

- At most 16 Mi characters of text plus base64, and at most 100000 items.
- Going past either reports a failed `done`, then throws `RangeError: script output exceeded the
  limit of 16777216 characters or 100000 text(), image(), and console calls. Print a summary
  instead, or write large data to a file with a tool.`
- Catching it does not help (`sandbox.test.ts:629-656`).
- Output after `done` is dropped.

### 1.5 `exit()`

`exit()` (`prelude-source.ts:431-441`) reports `done(ok, undefined, writes)`, then throws a frozen
sentinel to unwind. Output and writes made before it are kept. A script that catches the sentinel
runs on, but nothing it does changes the result (`sandbox.test.ts:194-210`).

### 1.6 `store(key, value)` / `load(key)`

Basics (`prelude-source.ts:273-335`, `README.md:80-92`):

- Both are synchronous, over a snapshot of key to JSON text taken from `options.store`.
- The host drops `undefined` values (`execution.ts:88-95`).
- A non-string key throws `TypeError: store() key must be a string`; `load()` says the same.
- A value that cannot be serialized, or that serializes to `undefined`, throws a `TypeError`.
- `store(k, undefined)` deletes the key.
- `load` returns a fresh parse.

Limits:

- More than 262144 characters of JSON in one value throws `RangeError: store("k") value has N
  characters of JSON, more than the limit of 262144. <HINT>`.
- More than 1048576 characters across all keys plus JSON throws `RangeError: store is full: stored
  values would exceed 1048576 characters of JSON. Delete keys with store(key, undefined). <HINT>`.
- `<HINT>` is `store() is for small state such as IDs or summaries. Show images with image(), keep
  large data in variables, or write it to a file with a tool.`

`writes` holds the last state of each touched key:

- It is reported only on success (`return` or `exit()`), as `[[key,json]|[key]]`.
- The host turns it into `storeWrites{set,delete}` (`execution.ts:112-129`).

Persistence in the coding agent:

- Each successful script with writes appends one session custom entry, `customType:
  "codemode-store"`, `data {set,delete}` (`ca/.../tool.ts:42`, `execute.ts:618-622`).
- `load` folds those entries along the current branch from the root, skipping malformed ones
  (`execute.ts:293-303`), so each branch has its own store.
- Without a session the store starts empty and writes are dropped.

Durable keeps the store in a `pi.codemode.store` document: one per conversation, rewindable,
`fork: asOf` (`du/.../codemode.ts:178-186`). It is committed together with the outcome
(`:572-582`).

### 1.7 `// @options:` line (`cm/src/source.ts:11-125`)

Placement:

- Only the first line counts, after its leading whitespace is trimmed.
- An options line anywhere else throws `The // @options: line must be the first line of the
  input; found one on line N`.

Fields:

- `max_output_tokens`: a non-negative safe integer.
- `timeout_ms`: 1 to 2147483647.

Rejected, with the exact messages in `source.ts`:

- empty input;
- invalid JSON;
- a value that is not an object;
- an unknown field (``@options only supports `max_output_tokens` and `timeout_ms`; got `x` ``);
- an options line with no code after it.

Other rules:

- The options line is replaced by an empty line, so line numbers stay the same.
- `CODEMODE_SOURCE_GRAMMAR` (`source.ts:22-30`) is a Lark grammar for OpenAI; noel can skip it.
- In the coding agent a source error is thrown, so the result is the bare message, without the
  header (`agent-session-codemode.test.ts:530-555`).

### 1.8 Globals and `spread`

How globals behave:

- Globals are top-level host functions. They act like tools, but `calls` does not record them
  (`types.ts:115-120`).
- An `a.b` name puts the function on a frozen, proxy-guarded `a` object
  (`prelude-source.ts:255-271`).
- With `spread: true` the function gets every argument as one array, with `undefined` sent as
  `null` (`sandbox.test.ts:494-511`).

Names (`execution.ts:51-61,152-167`):

- A name is an identifier or `ns.member`.
- These are reserved: `tools`, `ALL_TOOLS`, `console`, `text`, `image`, `exit`, `globalThis`,
  `store`, `load`.
- Duplicates are rejected, and so is a name that clashes with a namespace.

The coding agent always adds these spread globals (`execute.ts:669-735`):

- `searchTools(query,{limit?,namespace?})`, BM25-ranked;
- `describeTool(name)`;
- `describeNamespace(name)`.

With `models` on, which is the default (`index.ts:37`), it also adds `models.getModelsOfType`,
`getAvailableOfType`, `getModelOfType`, `classify` and `generateImages`. At most 4 run at once,
and each shows as a nested row with its cost (`execute.ts:742-850`).

### 1.9 Absent

Scripts get no timers, `fetch`, `process`, `require`, modules or `WebAssembly`. `eval` and
`Function` work, but only inside the VM. `import()` rejects (`README.md:56`,
`sandbox.test.ts:686-734`).

### 1.10 Prelude and lockdown

`PRELUDE_SOURCE` (`prelude-source.ts:48-522`) is
`(bridge, toolsJson, globalsJson, storeJson) => {settle, run, stalled}`.

Lockdown runs first (`:58-139`):

- It freezes everything reachable from the globals, plus the iterator, generator, async and
  TypedArray intrinsics.
- It makes the built-in globals non-writable and non-configurable.
- It handles the override mistake: writable data properties of `Object.prototype`, and
  `constructor`, `name`, `message`, `toString`, `toLocaleString`, `valueOf` and `toJSON`, become
  accessors. Their setters define an own property on the instance, and throw on the built-in
  itself.

After lockdown, the prelude:

- captures `JSON`, `Promise.prototype.then` and the Error constructors;
- defines the script's globals as enumerable, non-writable properties.

`bridge(kind,a,b,c)` takes primitives only:

- `("call"|"global", id, name, argsJson?)`
- `("output", "text"|"console", text)`
- `("output", "image", data, mime)`
- `("done", ok, valueJson|errorJson, writesJson?)`

`run(fn)` calls `fn(toolsProxy, console)`. A return value that cannot be serialized, such as
`10n`, becomes a `script` `TypeError`.

`stalled()` runs after every drain (`:509-520`). If the script has not finished and no call is
pending, tools and globals both counted, it fails with `Error: The script is waiting on a promise
that can never settle: no tool call is pending, and timers do not exist here.`

`describeError` (`:186-195`):

- coerces `name` and `message` to strings;
- turns a thrown value that is not an Error into `{message: format(v)}`;
- falls back to `The script threw a value that cannot be described`.

## 2. Result, errors, limits

The result shape (`cm/src/types.ts:94-109`):

- Success: `{ok:true,value,output,calls,storeWrites}`.
- Failure: `{ok:false,error{kind,name?,message,stack?},output,calls}`, keeping the output made
  before the failure.
- Items are `{type:"text",text,console?:true}` or `{type:"image",data,mimeType}`.
- `calls` lists tool calls only, in order: `{name,status,durationMs}` with status `ok`, `error` or
  `cancelled` (`execution.ts:378-408`). A record starts as `cancelled`. `finish()` aborts calls
  still pending, so they stay `cancelled`.

Error kinds (`types.ts:59-73`, `README.md:347-354`):

| kind | when |
| --- | --- |
| `script` | the script throws, does not parse, stalls, passes the output limit, or returns a value that cannot be serialized |
| `timeout` | `Execution timed out after N ms` (`execution.ts:214-218`), or the interrupt budget ran out (inline and remote sandboxes) |
| `aborted` | the signal's reason message or `Execution aborted` (`:305-308`), or `Sandbox closed` |
| `sandbox` | a trap or crash (including `Execution interrupted`), a launcher failure, or `Sandbox bridge broken: ... The script may have modified built-ins such as a prototype's toJSON.` (`:316-328`) |

Defaults:

- The sandbox's own timeout is 300000 ms (`execution.ts:49`).
- The coding agent passes `timeout_ms ?? Infinity` (`execute.ts:555,592`). **A coding-agent
  script has no deadline by default.** Only an abort stops it.
- Durable also has no deadline unless `timeout_ms` is set (`du/.../codemode.ts:472,488`).
- Memory: the sandbox sets no limit (`types.ts:128-131`). The coding agent sets 256 MiB
  (`execute.ts:56`). Running out throws a catchable `InternalError: out of memory`.
- The QuickJS stack guard is 512 KiB, so deep recursion throws a catchable `RangeError`.

The coding agent runs at most 2 workers per process (`PI_CODEMODE_MAX_WORKERS`). A call waits up
to 60 s for a free worker. If it is aborted while waiting, the result says "The script did not
start." (`execute.ts:58-111,557-576`).

A promise nothing can settle ends the run at once with the stall error
(`sandbox.test.ts:565-579`). Returning while a call is still pending is not a stall; the pending
call is cancelled.

## 3. The codemode tool

### 3.1 Registration and enabling

- Name and label: `codemode`.
- Parameters: `{code: string}`, described as "Raw JavaScript source." (`ca/.../tool.ts:78-82`).
  Durable makes the schema `open: true` (`du/.../codemode.ts:227`).
- Exposure is `model-only`, and the callable set filters `codemode` out, so scripts cannot call it
  (`tool.ts:159-161,405-406`). Durable uses `callers:["model"]` and `replay:"safe"`
  (`codemode.ts:260-261`).
- Registered with `defaultActive: false` (`index.ts:44`). Users turn it on in any of these ways:
  - `--tools codemode`;
  - `defaultTools: ["+codemode"]` (`ca/docs/settings.md:46-52`);
  - `setActiveTools()`;
  - the MCP extension does it when MCP tools are reachable only from scripts (`index.ts:5-7`).
- Settings (`ca/src/core/settings-defaults.ts:57-60`, `settings.md:43-44`):
  - `codemode.mode`: `"on"` or `"only"`;
  - `codemode.inlineBudget`: default 3000.
- Prompt snippet and guideline (`tool.ts:108-113`):
  - "Run JavaScript that calls other tools";
  - "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter
    large output, instead of many separate calls."

### 3.2 Description

The description is `DESCRIPTION_INTRO` (`tool.ts:118-120`) followed by `Globals:` lines
(`:123-134`).

When scripts can call tools, a `Nested tools:` catalog follows (`tool.ts:228-270`):

- One entry per tool: the heading `### \`id\`` (or `### \`id\` (\`raw\`)`), then the tool's
  sample.
- Grouping: tools without a namespace first, then namespaces by name.
- Budget: `inlineBudget` counts characters / 4. Each pass adds the cheapest remaining tool
  first, round-robin.
- Partial listings are marked `(some tools not listed)` or `(tools not listed)`.
- Deferred tools are never listed.

`prepareLoadout` (`tool.ts:343-379`) depends on the mode:

- `on` lists only the callable tools that are not direct. Each callable tool the model already
  declares gets `Codemode: \`tools.<id>(args)\` resolves to <...>.` (`:301-329`).
- `only` lists every callable tool and hides the declarations of active direct tools.

Durable adds "The script survives a restart..." and moves the catalog into a `codemode` section
of the system prompt (`du/src/codemode/describe.ts:29-48,118-157`).

### 3.3 Callable set

The coding agent (`ca/src/core/agent-session.ts:1598-1604`):

- Callable: active `direct` tools, and every `codemode` or `deferred` tool.
- Never callable: `model-only` tools and `codemode` itself.
- The set is captured when the script starts (`execute.ts:519`).

Durable uses `agent.callable`, minus the internal `pi.codemode.*` tools, which scripts reach only
through `models.*` (`codemode.ts:389-396,484`).

### 3.4 Nested calls, hooks, display

How the coding agent runs nested calls:

- `ctx.executeTool` (`ca/src/core/extensions/types.ts:396-415`) hands calls to
  `NestedToolCallRunner` (`ca/src/core/nested-tool-calls.ts:161-262`).
- Each call gets the id `<parentId>/<n>` (`:189`).
- Calls go through `runToolCall`, with the session's `beforeToolCall`/`afterToolCall` and the
  `parentToolCallId` (`agent-session.ts:746-778`):
  - validation applies;
  - a `tool_call` hook can block the call, and the script then rejects with the block's reason;
  - a `tool_result` hook can rewrite the result (`agent-session-codemode.test.ts:294-342`).
- The runner emits `tool_execution_start`, `tool_execution_update` and `tool_execution_end`, each
  carrying `parentToolCallId`.
- Calls run concurrently. They run one at a time only under sequential execution or
  `executionMode:"sequential"` (`:202-212`).

Records:

- Nested calls never become transcript results.
- The result message gets `nestedCalls{calls,complete}` (`ai/src/types.ts:626-645`):
  - each call is `{id,name,arguments?|argumentsBytes,status:ok|error|unfinished,durationMs,error}`;
  - limits (`nested-tool-calls.ts:26-31`, `agent-session.ts:1123-1134`): at most 256 calls,
    8 KiB of arguments per call, 32 KiB of arguments in total, and 500 characters of error;
  - nested `usage` is added to the result's usage.
- Compaction reads `nestedCalls` to find file operations (`ca/src/core/compaction/utils.ts:33`).
- `details.calls` (`tool.ts:86-106`, `execute.ts:525-552`):
  - each row is `{id,name,args,status,durationMs,error,cost?}`;
  - `args` is JSON cut to 200 characters, and `error` is cut to 500;
  - status is `running`, `ok`, `error` or `cancelled`;
  - every change publishes the rows through `onUpdate`;
  - a row still `running` at the end, or one that errored after an abort, becomes `cancelled`.

Display (`renderer.ts`): nested calls get no rows of their own. The codemode row lists them with
`…`, `✓`, `✗` or `⊘`, the tool name, the arguments (80 characters collapsed), the duration and the
cost, then shows the output without its header.

Durable does it differently:

- `api.executeTool(...,{key:"c<VM id>"})` gives the id `<callId>/c<id>` (`codemode.ts:839`).
- Each call is its own task. Its `beforeTool`/`afterTool` hooks see `call.parent`.
- Progress lives in `pi.live.nestedTools: NestedToolSlot[]`, as
  `{callId,parentCallId,parentTaskId,taskId,name,arguments,status,summary?}`
  (`du/src/harness/live.ts:50-69,179`, `harness/tool.ts:948-958`). The slots are removed when the
  parent settles.
- Results go to `NestedCallDoc` (`harness/tool.ts:117`).

### 3.5 Result text (`ca/.../execute.ts:617-651`)

The coding agent builds the result in eight steps:

1. On success, append the store entry when there are writes. Then, when `value !== undefined`,
   push `valueText(value)`: a string stays as it is; anything else becomes compact JSON.
2. `formatOutput` (`:322-341`):
   - If there is more than one non-console text item, each starts with `==> text N/M <==\n`.
   - Images stay in place.
   - All console lines go last, in one `<console_output>\n...\n</console_output>` item.
3. On failure, push `Script error:\n` + `formatError` (`:379-390`). The head depends on the kind:
   - `script`: `stack`, or else `${name??"Error"}: ${message}`;
   - `timeout`: `Script timed out: <m>`;
   - `aborted`: `Script aborted: <m>`;
   - `sandbox`: `Script sandbox failed: <m>`.

   Then `\n\n`, then either `No tool calls were made.` or `Tool calls made before the failure
   (they are not undone): a (ok), b (error)`.
4. A note follows when images from `models.generateImages()` were never shown (`:628-633`).
5. `joinAdjacentText` (`:348-372`) merges adjacent text items. It inserts `\n` when the last
   non-empty part does not end with one.
6. `truncateOutput(items, max_output_tokens ?? 10000)` (`:449-472`):
   - The budget is `tokens*4` characters, measured over the texts joined with `\n`.
   - Over budget, the text becomes `Warning: truncated output (original token count:
     ceil(len/4))\nTotal output lines: L\n\n<head>…<ceil(removed/4)> tokens truncated…<tail>`.
     The head is `floor(budget/2)` characters; the tail is the rest of the budget.
   - Then comes `\n\n[Full output: <path> (read with offset/limit)]`, or
     `\n\n[Could not save the full output: <err>]`. The path also goes into
     `details.fullOutputPath`.
   - Images come after the truncated text.
7. `saveImages` (`:414-442`):
   - Each distinct image is saved to a file once.
   - Every occurrence is preceded by `[Image saved to <path> (<mime>, <formatSize>)]`, or by
     `[Image (<kind>) could not be saved: <err>]`.
   - Then adjacent texts are joined again.
8. The content is `[{text:"Script completed|failed\nWall time X.X seconds\nOutput:\n"},
   ...items]`, and `isError = !ok`.

Durable (`du/src/codemode/output.ts:98-152`) differs:

- The budget is also capped at `maxBytes/4` and by a line budget (4 MiB and 100000 lines,
  `codemode.ts:245`).
- Image labels say `N bytes`.
- The files are named `output.txt` and `image-N.ext`.

### 3.6 Abort

When the tool's signal aborts:

- The script ends with `kind:"aborted"`.
- Pending nested calls are aborted and their rows become `cancelled`.
- The tool still returns a normal result, `Script failed ... Script aborted: <reason>`
  (`execute.ts:604,613-615`).

In Durable, if the host closes the sandbox, the call stores nothing and throws, so it is replayed
later (`codemode.ts:500-507`).

## 4. Upstream tests as goldens

Record them with a Bun fixture, the way the existing `scripts/ts-parity/*-fixture.ts` work. It
runs upstream `InlineCodemodeSandbox` (or `start()` with a fixed seed) and writes
`tests/fixtures/ts-parity/codemode.json`.

`cm/test/source.test.ts`: every case. They are pure, with exact errors.

`cm/test/sandbox.test.ts`:

| lines | asserts |
| --- | --- |
| `79-89` | JSON round trip of the return value |
| `97-130` | output order |
| `132-177` | `image()` errors |
| `180-192` | wrapped base64 |
| `194-210` | `exit()` |
| `212-262` | partial output; syntax errors and throws at `codemode.js:2`; stack format; a thrown non-Error; an unserializable return |
| `265-292` | call records |
| `306-329` | identifiers and `ALL_TOOLS` |
| `331-362` | `undefined` sent as `null`; catchable tool errors; unknown tool |
| `364-382` | unawaited calls are `cancelled` |
| `384-398` | close-match texts |
| `411-474` | store |
| `476-546` | globals and namespaces |
| `565-579` | stall |
| `629-656` | output limit |
| `674-683` | recursion |
| `686-708` | no host globals |
| `750-809` | frozen intrinsics, overrides, error names, frozen `tools` and `console` |

`cm/test/deterministic.test.ts`:

- the same seed and results give the same calls and output (`108-146`);
- UTC and no WeakRef (`148-157`);
- results apply in seq order (`159-178`);
- rejection properties (`275-289`).

Fingerprints in that file are engine-specific.

`cm/test/declarations.test.ts`: pure renderers, usable as goldens for the description and
`ALL_TOOLS`.

`ca/test/suite/agent-session-codemode.test.ts`, with the faux provider:

| lines | asserts |
| --- | --- |
| `144-232` | description per mode |
| `252-292` | parallel nested calls, exact text |
| `294-342` | hooks block and rewrite |
| `402-440` | image labels and dedupe |
| `442-464` | layout |
| `466-491` | failure summary |
| `530-555` | options |
| `556-567` | memory |
| `569-599` | truncation and spill |
| `612-644` | bash and read values |
| `646-708` | store entries and branches |

Not deterministic, so compare by regex or shape: wall time, `durationMs`, temp paths, timeout and
abort timing, stack columns.

djs already runs pi's suite through its adapter: 221 of 226 pass. The failures are the
fingerprint test, the strict-mode patch test, and the timing of the worker-lifetime fixture
(`djs/docs/host.md:574-605`).

## 5. Mapping onto noel

### 5.1 Crate layout

A new crate, `crates/noel-codemode`:

- `source.rs`, `identifier.rs`: §1.7 and §1.2.
- `prelude.rs`:
  - `include_str!("prelude.js")`, byte-identical to upstream, with a test that pins its FNV-1a;
  - djs's `performance` shim, `HOST_GLOBALS` (`djs/js/pi/vm.ts:146-156`).
- `vm.rs`, a port of `djs/js/pi/vm.ts`:
  - one VM per call, on its own thread (`std::thread` or `spawn_blocking`);
  - messages mirror `cm/src/runtime/protocol.ts:26-88`: `Call`, `Output`, `Done` and `Crash`
    out, `Result` in;
  - interrupts through an `Arc<AtomicBool>`.
- `execution.rs`, a port of `execution.ts` on tokio:
  - tool table, call records, output, deadline, cancellation (a child token per nested call),
    store writes;
  - bridge errors and `checkGlobals`.
- `format.rs`: §3.5, pure, with the spill and image writers injected.
- `describe.rs`: the description, the `Nested tools:` catalog, and a subset of `schemaToType`.

Dependencies:

- `djs-core` and `djs-prelude` as git dependencies, pinned by `rev`.
- Both crates are edition 2024. They build from noel's edition-2021 workspace on toolchain 1.99.
- `djs-prelude`'s build script builds the base image with a host `djs-core`, so the first build
  is slower. The cost is unverified.
- Default features `full` and `unicode`. A slim build is possible later
  (`djs/docs/slim-codemode.md`).

What changes in `noel-core`:

- A built-in `codemode` tool next to `todo`:
  - dispatch in `execute_model_tool_call` (`crates/noel-core/src/lib.rs:5057-5201`);
  - definition in `model_tool_definition` (`lib.rs:4629`);
  - listing in `active_tool_definitions` (`lib.rs:4595-4615`).
- A nested executor:
  - Factor the per-tool branches of `execute_model_tool_call` (extension v1 and v2, `todo`, and
    built-ins via `execute_tool_streaming_in` at `crates/noel-tools/src/lib.rs:263-335`) into a
    function that returns `(output, images, is_error)`.
  - That function must not call `push_tool_result` (`lib.rs:2979-3002`) and must not touch
    `messages` or `tool_history`.
  - Every nested call runs `gate_tool_call` (`crates/noel-core/src/extensions.rs:503`), as
    `run_tool_call` does (`lib.rs:4150-4180`). A blocked call rejects in the script with the
    gate's reason.
- Store:
  - a `custom` entry, `customType:"codemode-store"`, for parity of the session file (writer
    pattern at `lib.rs:2155-2175`), or a new `SessionRecord` (`lib.rs:1075`);
  - folded when the session loads;
  - copied by fork (`lib.rs:1315-1335`).
- Enabling: `default_active: false`. The tool is then on only via `--tools`, `defaultTools:
  ["+codemode"]` or `enabledTools`. Update `docs/rust-rewrite/non-parity-register.md:691-693`.

### 5.2 djs driver rules (`djs/js/pi/vm.ts`, `djs/docs/host.md:522-670`)

VM creation:

- Use `djs_prelude::new_vm(VmOptions{memory_limit: 256 MiB, random_seed: <entropy>, ..})`
  (`djs-prelude/src/lib.rs:81`). `Vm::new` compiles djs's own builtin prelude on every call
  (`vm/mod.rs:174-200`).
- Leave `frozen_builtins` off: pi's lockdown does the freezing.
- Use `WeakRefs::Absent` only for deterministic runs.

Setup (`vm.ts:527-556`):

1. Create the bridge with `new_host_function(1,"bridge")`. It is sync: the VM parks on each call.
2. Compile `HOST_GLOBALS`, then `"return "+PRELUDE`, with
   `CompileOptions::async_body("codemode-prelude.js",&[])`. Run each, and take the fulfilled
   value from `promise_state`.
3. Call the factory with the bridge and the three JSON strings. Keep handles to `api`, `settle`,
   `stalled` and `run`.
4. Call `seal()`.

Clock and polling:

- While the prelude runs: no polling, and a fixed clock (0, UTC).
- Before every later slice: `set_now(now_ms)` and `set_time_zone_offset(local)`
  (`vm.ts:383-396`).

The script:

- Compile `"return (async (tools, console) => {"+code+"\n})"` with
  `async_body("codemode.js",&[])` (`vm.ts:138,628`).
- A `CompileError` (it has `line` and `col`, `compiler/mod.rs:155-162`) becomes `{name,message,
  stack:"Name: message\n    at codemode.js:L:C"}`. On line 1, subtract the prefix length from
  the column (`vm.ts:202-211`).
- Evaluate it to get the function, call `run(api, fn)`, then drain.

The invoke loop runs `run(10_000)` slices (`vm.ts:339-381`) and handles each status:

| status | action |
| --- | --- |
| `OutOfFuel` | if interrupted, `abort()` and report a crash with `Execution interrupted`; else continue |
| `HostCall` | read the arguments at once (`host_call`, `heap.str_to_string`), handle the bridge message, `resolve_host_call(id, Ok(UNDEFINED))` |
| `Done`, `Idle` | return |
| `Threw(v)` | a crash with `display(v)`; the prelude catches the script's own errors |
| `HostOutOfMemory` | a crash |

Draining and delivery:

- `run` empties the job queue itself (D1370). After each `settle` or `run`, call `stalled(api)`.
- To deliver a result: create values for `(id,ok,payload,properties)`, call `settle`, then
  drain.

Values and handles:

- A `Value` is valid only until the next `run` or collection: the GC moves objects
  (`host.md:390-392`).
- Keep long-lived functions behind `heap_mut().new_handle(v)` (`heap/handle.rs:68`). Read them
  back with `get_handle` before each `call`.
- `call` itself never collects (`vm/mod.rs:601-604`).

Other notes:

- `Vm::define` creates non-enumerable, writable, configurable properties
  (`vm/mod.rs:541-545`). Use it only for globals injected from Rust. pi's prelude defines every
  name the script sees.
- Keep pi's sync bridge and its prelude-owned promises. That keeps ids, stall detection and
  ordering exactly as upstream. djs's async host functions are a later option.
- `Vm: Send` is likely: the heap is a `Vec<u8>` arena, and no `Rc` was found. Unverified, so add
  a compile-time assert.
- Optional: a prelude image cache (`image()` and `from_bytes`) makes starts about 10x faster
  (`host.md:563-572`).

### 5.3 What noel lacks

- **Interleaved content.**
  - `ToolResult` is `output: String` plus `images` (`noel-tools/src/lib.rs:102-133`).
  - Messages and `TurnEvent::ToolOutput` split content and media the same way
    (`lib.rs:3612-3618`).
  - Upstream sends `[header, text, image, text]`. Add ordered blocks, or register the
    difference.
- **`details`, `nestedCalls` and `parentToolCallId`.**
  - `TurnEvent` (`lib.rs:3566-3634`) has no way to report a nested call.
  - Needs a `NestedTool{parent_id,id,name,args,status,duration,error}` event.
  - Needs rows in the TUI and RPC (`noel-cli/src/main.rs:7212-7260`, `rpc_wire.rs:1196-1230`).
- **Structured outputs.** noel's `bash` and `read` return strings, so their script values
  differ from upstream. Upstream tests `612-644` depend on them.
- **Concurrency.**
  - Model calls run one at a time under `&mut Runtime` (`lib.rs:3982-3990`).
  - `Promise.all` needs an executor over shared state: the env, the options, and
    `Arc<Mutex<ExtensionHosts>>`.
  - `note_edited_files` and todo updates are deferred until the codemode call ends.
- **Abort.** noel drops the tool future and saves `aborted_output` (`lib.rs:4206-4227`).
  Upstream returns `Script aborted: <reason>`, so the codemode tool must handle cancellation
  itself.
- **Missing features:**
  - exposure modes;
  - namespaces;
  - `tool_search` and BM25 (`ca/src/extensions/tool-search/tool.ts`);
  - `codemode.mode` and `codemode.inlineBudget`;
  - MCP;
  - `models.*`;
  - durable replay. If it is ever needed, djs `snapshot()`/`resumeVm` is the simpler route
    (`host.md:552-560`).

### 5.4 Plan

1. Pin djs in `[workspace.dependencies]` with the full `rev`.
   - Spike: create a VM, compile an async body, make a sync host call, run with fuel, abort.
   - Build for `aarch64-apple-ios` (#118).
2. Port `source` and `identifier`, with goldens from `source.test.ts`.
3. Vendor the prelude, then port `vm.rs`, the protocol and `execution.rs`.
4. Write the Bun fixture `scripts/ts-parity/codemode-fixture.ts`, which produces
   `tests/fixtures/ts-parity/codemode.json`.
   - The Rust test compares `ok`, `value`, `output`, `calls[].{name,status}`, `storeWrites` and
     `error.{kind,name,message}`.
   - For stacks it compares only `codemode.js:<line>`.
5. Port `format.rs`, with goldens. Match the wall time by regex.
6. Add the `noel-core` tool:
   - its definition, at first with a plain `Nested tools:` list of the built-ins;
   - the nested executor with the gate;
   - store records;
   - `default_active: false`;
   - abort handling.
7. View: nested rows in events, the TUI and RPC. Run `make e2e`.
8. Structured outputs for `bash` and `read`.
9. The catalog (`inlineBudget`, `mode`), the discovery globals, and `ALL_TOOLS` samples (a port
   of `declarations.ts`).
10. Register what is deferred: `models.*`, MCP, durable replay, grammar sampling.

Tests to add, with the faux provider and no credentials:

- `noel-codemode`:
  - the golden suite;
  - stall;
  - `timeout_ms` with `while(true){}`;
  - cancellation;
  - output limit and memory limit;
  - recursion `RangeError`;
  - a pin of the prelude hash.
- `noel-core`:
  - parallel nested calls with exact text (a port of `252-292`);
  - a gate blocks a nested call;
  - the failure summary;
  - option errors;
  - truncation and spill;
  - the store: persisted, nothing written on failure, kept across resume and fork;
  - `codemode` off unless enabled, and not callable from scripts.
- e2e: a tmux run showing the nested rows.

### 5.5 Differences on djs (for the register)

- **Strict mode always.** Assigning to an undeclared variable throws `ReferenceError`, and
  writing to a frozen built-in throws. `sandbox.test.ts:736-748` fails
  (`host.md:595-597,609-611`).
- **Indirect `eval` always.** `Function("return this")()` is `undefined` (`host.md:618-620`).
- **Fuel-based interrupts.** Polls come every 10,000 fuel. There is no host-stack guard;
  recursion fails at about 131,000 frames with the same `RangeError` (`host.md:549`).
- **Engine-specific output.** Fingerprints and stack columns differ. The exact frame text on
  line 1 is unverified.
- **Minimum heap.** At least 512 KB to run the prelude (`host.md:627-629`).

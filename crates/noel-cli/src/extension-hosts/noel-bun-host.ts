// The bun host shim: runs one TypeScript extension under bun, speaking
// noel's extension protocol v2 over stdio (one JSON line per request).
//
// An extension is a module whose default export is a factory receiving an
// ExtensionAPI, like upstream pi's:
//
//   export default function (pi) {
//     pi.registerTool({ name: "deploy", description: "…", parameters: {...},
//                       execute: async (args) => ({ output: "done" }) });
//     pi.registerCommand("ship", { handler: async (args) => "shipped" });
//     pi.on("toolCall", async (event) => ({ allow: false, reason: "blocked" }));
//   }
//
// The manifest beside the extension opts in:
//   { "protocolVersion": 2, "runtime": "bun" }
//
// noel spawns: bun <this shim> <extension.ts>

const PROTOCOL_VERSION = 2;

interface WireRequest {
  protocolVersion: number;
  kind: string;
  command?: string;
  toolCallId?: string;
  input: string;
  cwd: string;
  sessionId?: string;
}

interface WireResponse {
  output?: string;
  error?: string;
  tools?: unknown[];
  commands?: unknown[];
  toolGate?: boolean;
  allow?: boolean;
  reason?: string;
}

interface ToolDefinition {
  name: string;
  description?: string;
  label?: string;
  parameters?: unknown;
  replaySafe?: boolean;
  promptSnippet?: string;
  promptGuidelines?: string[];
  // Upstream's signature: (toolCallId, params, signal, onUpdate, ctx).
  execute: (
    toolCallId: string,
    params: unknown,
    signal: undefined,
    onUpdate: undefined,
    ctx: ExtensionContext,
  ) => unknown;
}

interface CommandOptions {
  description?: string;
  handler: (args: string, ctx: ExtensionContext) => unknown;
}

interface ExtensionContext {
  cwd: string;
  sessionId?: string;
}

interface ToolCallEvent {
  tool: string;
  input: unknown;
}

interface ToolCallResult {
  allow?: boolean;
  reason?: string;
  output?: string;
}

type Handler = (event: unknown, ctx: ExtensionContext) => unknown;

const extensionArg = process.argv[2];
if (!extensionArg) {
  console.error("usage: bun noel-bun-host.ts <extension.ts>");
  process.exit(1);
}
// A relative path must resolve against noel's cwd (where the extension
// was found), not against this shim's own directory: absolutize before
// the dynamic import, the way bun's CLI would.
const extensionPath = Bun.resolveSync(
  extensionArg,
  process.env.NOEL_EXTENSION_DIR ?? process.cwd(),
);

// The registrations and handlers the factory made.
const tools = new Map<string, ToolDefinition>();
const commands = new Map<string, CommandOptions>();
const toolCallHandlers: Handler[] = [];
const settledHandlers: Handler[] = [];
const sessionStartHandlers: Handler[] = [];

let cwd = process.cwd();
let sessionId: string | undefined;

function errorResponse(message: string): WireResponse {
  return { error: message };
}

function makeApi(): Record<string, unknown> {
  return {
    registerTool(tool: ToolDefinition) {
      if (typeof tool?.name !== "string" || tool.name.length === 0) {
        throw new Error("registerTool needs a name");
      }
      if (typeof tool?.execute !== "function") {
        throw new Error(`tool ${tool.name} needs execute()`);
      }
      tools.set(tool.name, tool);
    },
    registerCommand(name: string, options: CommandOptions) {
      if (typeof name !== "string" || name.length === 0) {
        throw new Error("registerCommand needs a name");
      }
      if (typeof options?.handler !== "function") {
        throw new Error(`command /${name} needs handler()`);
      }
      commands.set(name, options);
    },
    on(event: string, handler: Handler) {
      if (typeof handler !== "function") {
        throw new Error(`on(${event}) needs a handler`);
      }
      switch (event) {
        // Upstream's event names; noel's protocol kinds carry the same
        // few that matter for a host process (agent_settled is noel's
        // settled notification, upstream's agent_end).
        case "toolCall":
        case "tool_call":
          toolCallHandlers.push(handler);
          break;
        case "agentSettled":
        case "agent_settled":
        case "agent_end":
          settledHandlers.push(handler);
          break;
        case "sessionStart":
        case "session_start":
        case "session_shutdown":
          sessionStartHandlers.push(handler);
          break;
        default:
          // Upstream has ~40 events; the protocol carries these and
          // ignores the rest of the registrations rather than failing.
          break;
      }
      return () => {
        const at = toolCallHandlers.indexOf(handler);
        if (at >= 0) toolCallHandlers.splice(at, 1);
        const at2 = settledHandlers.indexOf(handler);
        if (at2 >= 0) settledHandlers.splice(at2, 1);
        const at3 = sessionStartHandlers.indexOf(handler);
        if (at3 >= 0) sessionStartHandlers.splice(at3, 1);
      };
    },
    log(message: unknown) {
      console.error(String(message));
    },
  };
}

function registrations(): WireResponse {
  return {
    tools: [...tools.values()].map((tool) => ({
      name: tool.name,
      description: tool.description ?? `Run extension tool ${tool.name}.`,
      parameters: tool.parameters ?? { type: "object", properties: {} },
      replaySafe: tool.replaySafe === true,
      promptSnippet: tool.promptSnippet,
      promptGuidelines: tool.promptGuidelines ?? [],
    })),
    commands: [...commands.entries()].map(([name, options]) => ({
      name,
      description: options.description ?? "",
    })),
    toolGate: toolCallHandlers.length > 0,
  };
}

async function runHandlers(
  handlers: Handler[],
  event: unknown,
): Promise<unknown> {
  let chained: unknown = event;
  for (const handler of handlers) {
    chained = await handler(chained, { cwd, sessionId });
  }
  return chained;
}

function textOf(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

/// Upstream's AgentToolResult, flattened to the protocol's output string:
/// every text content item joined, with details kept beside it.
function textOfToolResult(result: unknown): string {
  if (result !== null && typeof result === "object" && "content" in (result as object)) {
    const content = (result as { content: unknown }).content;
    if (Array.isArray(content)) {
      const text = content
        .filter((part) => part?.type === "text")
        .map((part) => part.text)
        .filter((text) => typeof text === "string")
        .join("\n");
      if (typeof text === "string" && text.length > 0) return text;
    }
  }
  return textOf(result);
}

async function handle(request: WireRequest): Promise<WireResponse | null> {
  cwd = request.cwd || cwd;
  if (request.sessionId) sessionId = request.sessionId;
  const json = () => {
    try {
      return JSON.parse(request.input || "{}");
    } catch {
      return {};
    }
  };
  const input = request.kind === "tool" || request.kind === "toolCall" ? json() : request.input;
  switch (request.kind) {
    // Notifications: fire-and-forget by protocol (the host reads no
    // response), so no response line may be written. `sessionStart` is
    // also notified this way by some hosts; treat unknown kinds the
    // same, silently.
    case "reload":
    case "shutdown":
    case "sessionStart":
      return null;
    case "init": {
      // The factory ran at startup (below); the handshake returns what it
      // registered. A factory error is the extension's whole failure.
      return { output: "ready", ...registrations() };
    }
    case "tool": {
      const tool = tools.get(request.command ?? "");
      if (!tool) return errorResponse(`no tool ${request.command}`);
      // Upstream's execute signature; the protocol's toolCallId and input
      // (JSON) become its first two arguments. The result is upstream's
      // AgentToolResult: { content: [{type:"text", text}], ... }.
      const result = await tool.execute(
        request.toolCallId ?? "",
        input,
        undefined,
        undefined,
        { cwd, sessionId },
      );
      return { output: textOfToolResult(result) };
    }
    case "command": {
      let commandName = request.command ?? "";
      let command = commands.get(commandName);
      if (!command && commandName === process.env.NOEL_EXTENSION_NAME && commands.size === 1) {
        // `/extension:<name>` names the extension, not a command: with one
        // registered command it means that one (upstream's extension
        // command entry point).
        command = [...commands.values()][0];
      }
      if (!command) {
        const names = [...commands.keys()].map((name) => `/${name}`).join(", ");
        return errorResponse(
          commands.size === 0
            ? "this extension registers no commands"
            : `no command /${commandName}; registered: ${names}`,
        );
      }
      // The protocol's `input` is the command's argument text as typed,
      // not JSON (only `tool` and `toolCall` carry JSON arguments).
      const result = await command.handler(request.input ?? "", { cwd, sessionId });
      return { output: textOf(result) };
    }
    case "toolCall": {
      // Gate semantics: run every handler in order; the first explicit
      // block wins; a handler error fails closed (blocks with the error).
      // The event carries upstream's fields (toolName, input); the result
      // is upstream's `{ block, reason }` (mutation patches `input` in
      // place for later handlers and the call itself).
      let blocked: { reason: string } | undefined;
      try {
        const event = { tool: request.command, toolName: request.command, input };
        const outcome = await runHandlers(toolCallHandlers, event);
        if (outcome !== null && typeof outcome === "object") {
          const result = outcome as ToolCallResult;
          if (result.block === true) {
            blocked = { reason: result.reason ?? "blocked by extension" };
          } else if (result.allow === false) {
            blocked = { reason: result.reason ?? "blocked by extension" };
          }
        }
      } catch (error) {
        blocked = { reason: `extension gate failed: ${error}` };
      }
      if (blocked) {
        return { allow: false, reason: blocked.reason };
      }
      return { allow: true };
    }
    case "agentSettled": {
      await runHandlers(settledHandlers, {});
      return null;
    }
    case "sessionStart": {
      await runHandlers(sessionStartHandlers, {});
      return null;
    }
    default:
      return null;
  }
}

async function main() {
  // Load the extension: its default export is the factory. The factory's
  // registrations stage in the maps above; a throw is fatal and reported as
  // the init error, exactly upstream's discard().
  let factoryError: string | undefined;
  try {
    const module = await import(extensionPath);
    const factory = module.default;
    if (typeof factory !== "function") {
      factoryError = `${extensionPath} does not export a default factory function`;
    } else {
      await factory(makeApi());
    }
  } catch (error) {
    factoryError = error instanceof Error ? error.message : String(error);
  }

  const decoder = new TextDecoder();
  let buffer = "";
  const write = (response: WireResponse) => {
    process.stdout.write(JSON.stringify(response) + "\n");
  };

  // Line-at-a-time over stdin: one request in, one response out.
  for await (const chunk of Bun.stdin.stream()) {
    buffer += decoder.decode(chunk);
    let newline: number;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      let request: WireRequest;
      try {
        request = JSON.parse(line) as WireRequest;
      } catch {
        write(errorResponse(`malformed request: ${line.slice(0, 100)}`));
        continue;
      }
      if (request.kind === "init" && factoryError) {
        write(errorResponse(factoryError));
        continue;
      }
      try {
        const response = await handle(request);
        if (response !== null) write(response);
      } catch (error) {
        write(errorResponse(error instanceof Error ? error.message : String(error)));
      }
    }
  }
}

await main();

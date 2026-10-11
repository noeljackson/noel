import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Drives upstream `pi --mode rpc` against a local OpenAI-compatible streaming
 * stub and records every stdout record in order, per scenario.
 *
 * Record order is deterministic because the fixture sends a command only while
 * pi is idle or blocked on a stub reply the fixture holds, and because the stub
 * streams in lock-step: it sends one chunk, then waits for pi's record of it.
 * Volatile values are replaced by placeholders; see PLACEHOLDERS.
 */

const CLI = "/ts-reference/packages/coding-agent/src/cli.ts";
const WAIT_MS = 60_000;
/** No catalog refresh, version check, or telemetry. */
const PI_ENV = { PI_OFFLINE: "1", PI_SKIP_VERSION_CHECK: "1", PI_TELEMETRY: "0" };
/** The stub model is not eligible for cache warming; turn it off anyway. */
const SETTINGS = { cacheWarming: "off" };
const NOTES = "alpha\nbeta\n";
const STUB_MODEL = {
	id: "stub-model",
	name: "Stub Model",
	reasoning: false,
	input: ["text"],
	contextWindow: 128000,
	maxTokens: 4096,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};

const PLACEHOLDERS = {
	"<timestamp>": "Message and event timestamps (milliseconds since the epoch).",
	"<duration-ms>": "durationMs of assistant messages and tool executions.",
	"<tokens>": "Token counts in usage objects.",
	"<cost>": "Cost amounts in usage objects.",
	"<session-id>": "Session ids pi generates.",
	"<tmp>": "The scenario's temporary directory (cwd, agent dir, and session dir live below it).",
	"<session-file>": "Session file name (timestamp and session id).",
	"<stub-url>": "Base URL of the local OpenAI-compatible stub model.",
	"<text>": "System prompt prose and tool descriptions; other fixtures cover them.",
	"<schema>": "Tool parameter JSON schemas; local-tools.json covers them.",
	"<parse-error>": "The JavaScript JSON.parse error text after pi's own prefix.",
};

type Json = Record<string, any>;
type Gate = { promise: Promise<void>; open: () => void };

/** One scripted model reply: text deltas or one tool call. */
type Reply = {
	text?: string[];
	toolCall?: { id: string; name: string; arguments: Json };
	/** "headers": answer only after release(n). "stream": send the first delta, then hang until pi disconnects. */
	hold?: "headers" | "stream";
};

function gate(): Gate {
	let open = () => {};
	const promise = new Promise<void>((resolve) => {
		open = resolve;
	});
	return { promise, open };
}

async function withTimeout<T>(promise: Promise<T>, label: string): Promise<T> {
	let timer: NodeJS.Timeout | undefined;
	const timeout = new Promise<never>((_, reject) => {
		timer = setTimeout(() => reject(new Error(`timed out waiting for ${label}`)), WAIT_MS);
	});
	try {
		return await Promise.race([promise, timeout]);
	} finally {
		clearTimeout(timer);
	}
}

const isAssistantStart = (record: Json) => record.type === "message_start" && record.message?.role === "assistant";
const isDelta = (record: Json) =>
	record.type === "message_update" && String(record.assistantMessageEvent?.type).endsWith("_delta");

/** OpenAI-compatible chat-completions stub that replays scripted replies. */
class ModelStub {
	readonly requests: Json[] = [];
	private readonly gates: Gate[];
	private readonly arrivals: Gate[];
	private server?: Server;
	pi?: RpcPi;
	url = "";

	constructor(private readonly replies: Reply[]) {
		this.gates = replies.map(() => gate());
		this.arrivals = replies.map(() => gate());
	}

	async start(): Promise<void> {
		this.server = createServer((request, response) => {
			this.handle(request, response).catch((error) => {
				console.error(error);
				response.destroy();
			});
		});
		await new Promise<void>((resolve) => this.server!.listen(0, "127.0.0.1", resolve));
		this.url = `http://127.0.0.1:${(this.server.address() as AddressInfo).port}/v1`;
	}

	async stop(): Promise<void> {
		this.server?.closeAllConnections();
		await new Promise<void>((resolve) => (this.server ? this.server.close(() => resolve()) : resolve()));
	}

	release(request: number): void {
		this.gates[request - 1].open();
	}

	waitForRequest(request: number): Promise<void> {
		return withTimeout(this.arrivals[request - 1].promise, `model request ${request}`);
	}

	private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
		let body = "";
		for await (const chunk of request) body += chunk;
		const index = this.requests.length;
		this.requests.push(JSON.parse(body));
		const reply = this.replies[index];
		if (!reply) {
			response.writeHead(500, { "content-type": "application/json" });
			response.end(JSON.stringify({ error: { message: `stub has no reply ${index + 1}` } }));
			return;
		}
		this.arrivals[index].open();
		if (reply.hold === "headers") await this.gates[index].promise;
		const pi = this.pi!;

		response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
		response.flushHeaders();
		const start = await pi.waitForNth(`assistant message_start ${index + 1}`, isAssistantStart, index + 1);
		const id = `chatcmpl-stub-${index + 1}`;
		const send = (delta: Json, finish: string | null = null, usage?: Json) =>
			response.write(
				`data: ${JSON.stringify({
					id,
					object: "chat.completion.chunk",
					created: 0,
					model: "stub-model",
					choices: [{ index: 0, delta, finish_reason: finish }],
					...(usage ? { usage } : {}),
				})}\n\n`,
			);
		const deltas: Json[] = (reply.text ?? []).map((content, position) =>
			position === 0 ? { role: "assistant", content } : { content },
		);
		if (reply.toolCall) {
			const { id: callId, name, arguments: args } = reply.toolCall;
			deltas.push({
				role: "assistant",
				tool_calls: [{ index: 0, id: callId, type: "function", function: { name, arguments: "" } }],
			});
			deltas.push({ tool_calls: [{ index: 0, function: { arguments: JSON.stringify(args) } }] });
		}
		// Lock-step: each chunk produces exactly one *_delta record; wait for it before sending the next.
		for (const [position, delta] of deltas.entries()) {
			send(delta);
			await pi.waitForNth(`delta ${position + 1} of reply ${index + 1}`, isDelta, position + 1, start + 1);
			if (reply.hold === "stream") {
				await new Promise<void>((resolve) => (response.destroyed ? resolve() : response.on("close", resolve)));
				return;
			}
		}
		send({}, reply.toolCall ? "tool_calls" : "stop", { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 });
		response.end("data: [DONE]\n\n");
	}
}

/** One upstream `pi --mode rpc` process. */
class RpcPi {
	readonly records: Json[] = [];
	readonly commands: Json[] = [];
	private stderr = "";
	private buffer = "";
	private readonly child: ChildProcessWithoutNullStreams;
	private readonly exited: Promise<number | null>;
	private readonly listeners = new Set<() => void>();

	constructor(dir: string, args: string[]) {
		this.child = spawn("npx", ["tsx", CLI, "--mode", "rpc", ...args], {
			cwd: join(dir, "cwd"),
			// Own process group, so a failed scenario can kill npx, tsx, and pi together.
			detached: true,
			env: {
				PATH: process.env.PATH,
				HOME: join(dir, "home"),
				// Reuse the npx cache that already holds tsx.
				npm_config_cache: process.env.npm_config_cache ?? join(homedir(), ".npm"),
				// The upstream tsconfig maps workspace packages to their sources.
				TSX_TSCONFIG_PATH: "/ts-reference/tsconfig.json",
				PI_CODING_AGENT_DIR: join(dir, "agent"),
				...PI_ENV,
			},
		});
		this.child.stdout.setEncoding("utf8");
		this.child.stdout.on("data", (chunk: string) => {
			this.buffer += chunk;
			for (let newline = this.buffer.indexOf("\n"); newline !== -1; newline = this.buffer.indexOf("\n")) {
				this.records.push(JSON.parse(this.buffer.slice(0, newline)));
				this.buffer = this.buffer.slice(newline + 1);
			}
			for (const listener of this.listeners) listener();
		});
		this.child.stderr.setEncoding("utf8");
		this.child.stderr.on("data", (chunk: string) => {
			this.stderr += chunk;
		});
		this.exited = new Promise((resolve) => this.child.on("exit", (code) => resolve(code)));
	}

	send(command: Json | string): void {
		this.commands.push({ afterRecords: this.records.length, command });
		this.child.stdin.write(`${typeof command === "string" ? command : JSON.stringify(command)}\n`);
	}

	/** Waits for the nth record matching at or after `from`; returns its index. */
	async waitForNth(label: string, match: (record: Json) => boolean, nth = 1, from = 0): Promise<number> {
		const find = () => {
			let seen = 0;
			for (let index = from; index < this.records.length; index++) {
				if (match(this.records[index]) && ++seen === nth) return index;
			}
			return -1;
		};
		const found = find();
		if (found !== -1) return found;
		return new Promise<number>((resolve, reject) => {
			const timer = setTimeout(() => {
				this.listeners.delete(check);
				reject(new Error(`timed out waiting for ${label}\n${this.dump()}`));
			}, WAIT_MS);
			const check = () => {
				const index = find();
				if (index === -1) return;
				clearTimeout(timer);
				this.listeners.delete(check);
				resolve(index);
			};
			this.listeners.add(check);
		});
	}

	waitFor(label: string, match: (record: Json) => boolean): Promise<number> {
		return this.waitForNth(label, match);
	}

	response(id: string): Promise<number> {
		return this.waitFor(`response ${id}`, (record) => record.type === "response" && record.id === id);
	}

	/** Waits for the nth agent_settled event. */
	settled(nth = 1): Promise<number> {
		return this.waitForNth(`agent_settled ${nth}`, (record) => record.type === "agent_settled", nth);
	}

	/** Waits until model request `n` is blocked in the stub, after pi recorded the user message it sends. */
	async blocked(stub: ModelStub, request: number, userText: string): Promise<void> {
		await stub.waitForRequest(request);
		await this.waitFor(
			`user message ${userText}`,
			(record) =>
				record.type === "message_end" &&
				record.message?.role === "user" &&
				record.message.content?.[0]?.text === userText,
		);
	}

	async close(): Promise<void> {
		this.child.stdin.end();
		const code = await withTimeout(this.exited, "pi exit");
		if (code !== 0) throw new Error(`pi exited with ${code}\n${this.dump()}`);
	}

	kill(): void {
		process.kill(-this.child.pid!, "SIGKILL");
	}

	private dump(): string {
		return `records:\n${this.records.map((record) => JSON.stringify(record)).join("\n")}\nstderr:\n${this.stderr}`;
	}
}

type Scenario = {
	name: string;
	description: string;
	/** Session flags; defaults to --no-session. */
	session?: "none" | "dir";
	replies: Reply[];
	run: (pi: RpcPi, stub: ModelStub) => Promise<void>;
};

const text = (...deltas: string[]): Reply => ({ text: deltas });

const scenarios: Scenario[] = [
	{
		name: "prompt-text",
		description: "A prompt answered with streamed text.",
		replies: [text("Hello", " there")],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "say hello" });
			await pi.settled();
		},
	},
	{
		name: "tool-round",
		description: "The model calls read on a file in the cwd, gets the result, then answers.",
		replies: [
			{ toolCall: { id: "call_read_1", name: "read", arguments: { path: "notes.txt" } } },
			text("The file lists alpha and beta."),
		],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "read notes.txt" });
			await pi.settled();
		},
	},
	{
		name: "steer",
		description: "A steer sent while the first reply is held is delivered after that reply, before the next model call.",
		replies: [{ ...text("One, two"), hold: "headers" }, text("Stopped.")],
		run: async (pi, stub) => {
			pi.send({ id: "1", type: "prompt", message: "count to three" });
			await pi.blocked(stub, 1, "count to three");
			pi.send({ id: "2", type: "steer", message: "stop counting" });
			await pi.response("2");
			stub.release(1);
			await pi.settled();
		},
	},
	{
		name: "follow-up",
		description: "A follow_up sent while the first reply is held runs after the agent would otherwise stop.",
		replies: [{ ...text("Roses are red."), hold: "headers" }, text("Les roses sont rouges.")],
		run: async (pi, stub) => {
			pi.send({ id: "1", type: "prompt", message: "write a rhyme" });
			await pi.blocked(stub, 1, "write a rhyme");
			pi.send({ id: "2", type: "follow_up", message: "now in French" });
			await pi.response("2");
			stub.release(1);
			await pi.settled();
		},
	},
	{
		name: "abort",
		description: "An abort while the reply is streaming, then an abort while idle.",
		replies: [{ ...text("Once", " upon a time"), hold: "stream" }],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "tell a long story" });
			await pi.waitFor("first text delta", isDelta);
			pi.send({ id: "2", type: "abort" });
			await pi.response("2");
			pi.send({ id: "3", type: "abort" });
			await pi.response("3");
		},
	},
	{
		name: "abort-with-queue",
		description:
			"An abort while a steer and a follow_up are queued: they stay queued, and the next prompt delivers them.",
		replies: [{ ...text("Once", " upon a time"), hold: "stream" }, text("Shorter."), text("Summary.")],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "tell a long story" });
			await pi.waitFor("first text delta", isDelta);
			pi.send({ id: "2", type: "steer", message: "shorter please" });
			await pi.response("2");
			pi.send({ id: "3", type: "follow_up", message: "then summarize" });
			await pi.response("3");
			pi.send({ id: "4", type: "abort" });
			await pi.response("4");
			pi.send({ id: "5", type: "get_state" });
			await pi.response("5");
			pi.send({ id: "6", type: "prompt", message: "next" });
			await pi.settled(2);
		},
	},
	{
		name: "prompt-while-busy",
		description:
			"Prompts sent during a run: without streamingBehavior (an error), then with steer and followUp, then get_state.",
		replies: [{ ...text("First."), hold: "headers" }, text("Steered."), text("Followed.")],
		run: async (pi, stub) => {
			pi.send({ id: "1", type: "prompt", message: "first" });
			await pi.blocked(stub, 1, "first");
			pi.send({ id: "2", type: "prompt", message: "second" });
			await pi.response("2");
			pi.send({ id: "3", type: "prompt", message: "third", streamingBehavior: "steer" });
			await pi.response("3");
			pi.send({ id: "4", type: "prompt", message: "fourth", streamingBehavior: "followUp" });
			await pi.response("4");
			pi.send({ id: "5", type: "get_state" });
			await pi.response("5");
			stub.release(1);
			await pi.settled();
		},
	},
	{
		name: "state",
		description:
			"get_state, get_messages, and get_last_assistant_text before and after a run, with a session dir; then set_session_name and get_state.",
		session: "dir",
		replies: [text("Hi.")],
		run: async (pi) => {
			pi.send({ id: "1", type: "get_state" });
			await pi.response("1");
			pi.send({ id: "2", type: "get_messages" });
			await pi.response("2");
			pi.send({ id: "3", type: "get_last_assistant_text" });
			await pi.response("3");
			pi.send({ id: "4", type: "prompt", message: "hello" });
			await pi.settled();
			pi.send({ id: "5", type: "get_messages" });
			await pi.response("5");
			pi.send({ id: "6", type: "get_last_assistant_text" });
			await pi.response("6");
			pi.send({ id: "7", type: "get_state" });
			await pi.response("7");
			pi.send({ id: "8", type: "set_session_name", name: "golden" });
			await pi.response("8");
			pi.send({ id: "9", type: "get_state" });
			await pi.response("9");
		},
	},
	{
		name: "clear-queue",
		description: "clear_queue during a run returns the queued steer and follow_up, which then never run.",
		replies: [{ ...text("Working."), hold: "headers" }],
		run: async (pi, stub) => {
			pi.send({ id: "1", type: "prompt", message: "start" });
			await pi.blocked(stub, 1, "start");
			pi.send({ id: "2", type: "steer", message: "steer me" });
			await pi.response("2");
			pi.send({ id: "3", type: "follow_up", message: "follow me" });
			await pi.response("3");
			pi.send({ id: "4", type: "clear_queue" });
			await pi.response("4");
			stub.release(1);
			await pi.settled();
		},
	},
	{
		name: "malformed-and-unknown",
		description:
			"A line that is not JSON gets a parse response; an unknown command type gets an error response; a command without an id gets a response without one.",
		replies: [],
		run: async (pi) => {
			pi.send("this is not json");
			await pi.waitFor("parse response", (record) => record.type === "response" && record.command === "parse");
			pi.send({ id: "2", type: "no_such_command" });
			await pi.response("2");
			pi.send({ type: "get_last_assistant_text" });
			await pi.waitFor(
				"response without id",
				(record) => record.type === "response" && record.command === "get_last_assistant_text",
			);
		},
	},
	{
		name: "queue-modes",
		description: "set_steering_mode and set_follow_up_mode responses, then get_state.",
		replies: [],
		run: async (pi) => {
			pi.send({ id: "1", type: "set_steering_mode", mode: "all" });
			await pi.response("1");
			pi.send({ id: "2", type: "set_follow_up_mode", mode: "all" });
			await pi.response("2");
			pi.send({ id: "3", type: "get_state" });
			await pi.response("3");
		},
	},
];

function messageText(message: Json): string {
	if (typeof message.content === "string") return message.content;
	return (message.content ?? []).map((part: Json) => part.text ?? "").join("");
}

/** What each model request contained, without the system prompt. */
function summarizeRequest(body: Json): Json[] {
	return body.messages
		.filter((message: Json) => message.role !== "system")
		.map((message: Json) => ({
			role: message.role,
			...(messageText(message) ? { text: messageText(message) } : {}),
			...(message.tool_calls
				? { toolCalls: message.tool_calls.map((call: Json) => `${call.id} ${call.function.name}`) }
				: {}),
			...(message.tool_call_id ? { toolCallId: message.tool_call_id } : {}),
		}));
}

type Volatile = { tmp: string; stubUrl: string };

function normalize(value: unknown, volatile: Volatile, key = ""): unknown {
	if (typeof value === "string") {
		const replaced = value.split(volatile.stubUrl).join("<stub-url>").split(volatile.tmp).join("<tmp>");
		if (key === "sessionFile") return replaced.replace(/[^/]+\.jsonl$/, "<session-file>");
		if (key === "sessionId") return "<session-id>";
		if (key === "error" && replaced.startsWith("Failed to parse command: ")) {
			return "Failed to parse command: <parse-error>";
		}
		return replaced;
	}
	if (typeof value === "number") {
		if (key === "timestamp") return "<timestamp>";
		if (key === "durationMs") return "<duration-ms>";
		return value;
	}
	if (Array.isArray(value)) return value.map((item) => normalize(item, volatile));
	if (value === null || typeof value !== "object") return value;
	const object = value as Json;
	if (key === "usage") return normalizeUsage(object);
	if (object.role === "system" && object.sections) return normalizeSystemMessage(object, volatile);
	return Object.fromEntries(Object.entries(object).map(([name, item]) => [name, normalize(item, volatile, name)]));
}

function normalizeUsage(usage: Json): Json {
	return Object.fromEntries(
		Object.entries(usage).map(([name, item]) => [
			name,
			name === "cost"
				? Object.fromEntries(Object.keys(item).map((costName) => [costName, "<cost>"]))
				: typeof item === "number"
					? "<tokens>"
					: item,
		]),
	);
}

function normalizeSystemMessage(message: Json, volatile: Volatile): Json {
	return Object.fromEntries(
		Object.entries(message).map(([name, item]) => {
			if (name === "sections") {
				return [name, Object.fromEntries(Object.keys(item).map((section) => [section, "<text>"]))];
			}
			if (name === "toolsAdded") {
				return [
					name,
					item.map((tool: Json) => ({
						...tool,
						description: "<text>",
						parameters: "<schema>",
					})),
				];
			}
			return [name, normalize(item, volatile, name)];
		}),
	);
}

async function runScenario(scenario: Scenario) {
	const dir = await mkdtemp(join(tmpdir(), "pi-rpc-"));
	const stub = new ModelStub(scenario.replies);
	await stub.start();
	for (const sub of ["cwd", "home", "agent"]) await mkdir(join(dir, sub), { recursive: true });
	await writeFile(join(dir, "cwd", "notes.txt"), NOTES);
	await writeFile(
		join(dir, "agent", "models.json"),
		JSON.stringify({
			providers: {
				stub: {
					baseUrl: stub.url,
					api: "openai-completions",
					apiKey: "stub-key",
					models: [STUB_MODEL],
				},
			},
		}),
	);
	await writeFile(join(dir, "agent", "settings.json"), JSON.stringify(SETTINGS));
	const args = [
		"--model",
		"stub/stub-model",
		"--no-extensions",
		"--no-skills",
		"--no-prompt-templates",
		"--no-themes",
		"--no-context-files",
		...(scenario.session === "dir" ? ["--session-dir", join(dir, "sessions")] : ["--no-session"]),
	];
	const pi = new RpcPi(dir, args);
	stub.pi = pi;
	try {
		await scenario.run(pi, stub);
		await pi.close();
	} catch (error) {
		pi.kill();
		throw error;
	} finally {
		await stub.stop();
	}
	const volatile = { tmp: dir, stubUrl: stub.url };
	await rm(dir, { recursive: true, force: true });
	return {
		name: scenario.name,
		description: scenario.description,
		args: ["--mode", "rpc", ...args].map((arg) => normalize(arg, volatile)),
		commands: normalize(pi.commands, volatile),
		records: normalize(pi.records, volatile),
		modelRequests: stub.requests.map(summarizeRequest),
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: rpc-protocol-fixture.ts <output-dir>");
	}
	const results = [];
	for (const scenario of scenarios) {
		results.push(await runScenario(scenario));
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "rpc-protocol.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				format: {
					args: "Arguments after setup.command.",
					commands: "Stdin lines in order; afterRecords counts the stdout records received before the line was sent.",
					records: "Every stdout record, in order, with placeholders.",
					modelRequests: "For each request the stub model received, its messages without the system prompt.",
				},
				setup: {
					command: ["npx", "tsx", CLI],
					cwd: "<tmp>/cwd",
					environment: { ...PI_ENV, HOME: "<tmp>/home", PI_CODING_AGENT_DIR: "<tmp>/agent" },
					settings: SETTINGS,
					files: { "<tmp>/cwd/notes.txt": NOTES },
					model: {
						note: "A local OpenAI-compatible streaming stub, configured in <tmp>/agent/models.json.",
						provider: "stub",
						api: "openai-completions",
						...STUB_MODEL,
					},
				},
				placeholders: PLACEHOLDERS,
				scenarios: results,
			},
			null,
			2,
		)}\n`,
	);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

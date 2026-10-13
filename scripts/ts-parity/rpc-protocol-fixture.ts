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
 * pi is idle, blocked on a stub reply the fixture holds, or waiting out a long
 * retry backoff, and because the stub streams in lock-step: it sends one chunk,
 * then waits for pi's record of it. Volatile values are replaced by
 * placeholders; see PLACEHOLDERS.
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
	"<timestamp>": "Message and event timestamps (milliseconds since the epoch), and entry timestamps (ISO 8601).",
	"<duration-ms>": "durationMs of assistant messages and tool executions.",
	"<seconds>": "wall_time_seconds of a bash result.",
	"<tokens>": "Token counts in usage objects.",
	"<cost>": "Cost amounts in usage objects.",
	"<session-id>": "Session ids pi generates.",
	"<entry-id>": "Session entry ids pi generates (id, parentId, targetId, firstKeptEntryId).",
	"<tmp>": "The scenario's temporary directory (cwd, agent dir, and session dir live below it).",
	"<session-file>": "Session file name (timestamp and session id).",
	"<stub-url>": "Base URL of the local OpenAI-compatible stub model.",
	"<text>": "System prompt prose and tool descriptions; other fixtures cover them.",
	"<schema>": "Tool parameter JSON schemas; local-tools.json covers them.",
	"<parse-error>": "The JavaScript JSON.parse error text after pi's own prefix.",
};

type Json = Record<string, any>;
type Gate = { promise: Promise<void>; open: () => void };

/** One scripted model reply: text deltas or one tool call, or a failure. */
type Reply = {
	text?: string[];
	toolCall?: { id: string; name: string; arguments: Json };
	/** "headers": answer only after release(n). "stream": send the first delta, then hang until pi disconnects. */
	hold?: "headers" | "stream";
	/** Answer with this HTTP status and an OpenAI error body instead of a stream. */
	error?: { status: number; message: string; type: string; code?: string };
	/** Close the connection after the deltas, before the stream finishes. */
	drop?: true;
	/** The last chunk's finish_reason; "stop", or "tool_calls" after a tool call, by default. */
	finish?: string;
	/** A reply pi records no assistant message for (a compaction summary): streamed without lock-step. */
	unrecorded?: true;
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
		if (reply.error) {
			const { status, ...error } = reply.error;
			response.writeHead(status, { "content-type": "application/json" });
			response.end(JSON.stringify({ error: { param: null, code: null, ...error } }));
			return;
		}

		response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
		response.flushHeaders();
		// pi records an assistant message for every reply before this one, except the unrecorded ones.
		const recorded = this.replies.slice(0, index).filter((earlier) => !earlier.unrecorded).length;
		const start = reply.unrecorded
			? 0
			: await pi.waitForNth(`assistant message_start ${recorded + 1}`, isAssistantStart, recorded + 1);
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
			if (reply.unrecorded) continue;
			await pi.waitForNth(`delta ${position + 1} of reply ${index + 1}`, isDelta, position + 1, start + 1);
			if (reply.hold === "stream") {
				await new Promise<void>((resolve) => (response.destroyed ? resolve() : response.on("close", resolve)));
				return;
			}
		}
		if (reply.drop) {
			response.destroy();
			return;
		}
		send({}, reply.finish ?? (reply.toolCall ? "tool_calls" : "stop"), {
			prompt_tokens: 10,
			completion_tokens: 5,
			total_tokens: 15,
		});
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

	/** Waits until pi is waiting out a retry's backoff: it announced the retry and left the failed attempt out. */
	async backingOff(): Promise<void> {
		const start = await this.waitFor("auto_retry_start", (record) => record.type === "auto_retry_start");
		await this.waitForNth("entry_appended", (record) => record.type === "entry_appended", 1, start);
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
	/** Settings added to SETTINGS in <tmp>/agent/settings.json. */
	settings?: Json;
	replies: Reply[];
	run: (pi: RpcPi, stub: ModelStub) => Promise<void>;
};

const text = (...deltas: string[]): Reply => ({ text: deltas });

const OVERLOADED: Reply = { error: { status: 503, message: "The server is overloaded.", type: "server_error" } };
const OVERFLOW =
	"This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens. Please reduce the length of the messages.";
/** The pause is several times upstream's 100 ms update throttle, so each line is one update. */
const BASH_COMMAND = "printf 'one\\n'; sleep 1; printf 'two\\n'";

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
	{
		name: "retry-then-success",
		description:
			"A request that fails with a retryable error (HTTP 503) is retried after a backoff and then answered.",
		settings: { retry: { baseDelayMs: 10 } },
		replies: [OVERLOADED, text("Recovered.")],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "hello" });
			await pi.settled();
		},
	},
	{
		name: "retry-exhausted",
		description:
			"Every attempt fails with a retryable error: after maxRetries retries the run ends with the last error.",
		settings: { retry: { maxRetries: 1, baseDelayMs: 10 } },
		replies: [OVERLOADED, OVERLOADED],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "hello" });
			await pi.settled();
		},
	},
	{
		name: "abort-during-retry",
		description: "An abort during a retry's backoff ends the run without another request.",
		settings: { retry: { baseDelayMs: 60000 } },
		replies: [OVERLOADED],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "hello" });
			await pi.backingOff();
			pi.send({ id: "2", type: "abort" });
			await pi.response("2");
			await pi.settled();
		},
	},
	{
		name: "abort-retry",
		description: "abort_retry during a retry's backoff cancels the retry: the run ends with the failed attempt.",
		settings: { retry: { baseDelayMs: 60000 } },
		replies: [OVERLOADED],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "hello" });
			await pi.backingOff();
			pi.send({ id: "2", type: "abort_retry" });
			await pi.response("2");
			await pi.settled();
		},
	},
	{
		name: "auto-retry-off",
		description: "After set_auto_retry turns retries off, a retryable failure ends the run at once.",
		settings: { retry: { baseDelayMs: 10 } },
		replies: [OVERLOADED],
		run: async (pi) => {
			pi.send({ id: "1", type: "set_auto_retry", enabled: false });
			await pi.response("1");
			pi.send({ id: "2", type: "prompt", message: "hello" });
			await pi.settled();
		},
	},
	{
		name: "no-retry",
		description: "A request that fails with an error that is not retryable (HTTP 401) ends the run at once.",
		settings: { retry: { baseDelayMs: 10 } },
		replies: [
			{
				error: {
					status: 401,
					message: "Incorrect API key provided.",
					type: "invalid_request_error",
					code: "invalid_api_key",
				},
			},
		],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "hello" });
			await pi.settled();
		},
	},
	{
		name: "mid-stream-drop",
		description:
			"The connection drops after the reply's first delta: the partial reply is left out and the request is retried. Then get_state and get_messages.",
		settings: { retry: { baseDelayMs: 10 } },
		replies: [{ ...text("Hel"), drop: true }, text("Hello again.")],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "hello" });
			await pi.settled();
			pi.send({ id: "2", type: "get_state" });
			await pi.response("2");
			pi.send({ id: "3", type: "get_messages" });
			await pi.response("3");
		},
	},
	{
		name: "overflow-recovery",
		description:
			"The third prompt's request fails with a context overflow: pi leaves the attempt out, compacts with reason overflow (the first turn; keepRecentTokens covers the rest), and retries once.",
		settings: { compaction: { keepRecentTokens: 8 } },
		replies: [
			text("First answer."),
			text("Second answer."),
			{ error: { status: 400, message: OVERFLOW, type: "invalid_request_error", code: "context_length_exceeded" } },
			{ ...text("Summary of the first turn."), unrecorded: true },
			text("Third answer."),
		],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "first" });
			await pi.settled();
			pi.send({ id: "2", type: "prompt", message: "second" });
			await pi.settled(2);
			pi.send({ id: "3", type: "prompt", message: "third" });
			await pi.settled(3);
		},
	},
	{
		name: "length-stop",
		description:
			"A reply cut off at the output limit (finish_reason length) with nothing to compact, then another prompt.",
		replies: [{ ...text("This answer is cut"), finish: "length" }, text("Next answer.")],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "write a lot" });
			await pi.settled();
			pi.send({ id: "2", type: "prompt", message: "next" });
			await pi.settled(2);
		},
	},
	{
		name: "length-with-tool-call",
		description:
			"A reply cut off at the output limit while calling a tool: the call is not run, and the model is asked again.",
		replies: [
			{ toolCall: { id: "call_read_1", name: "read", arguments: { path: "notes.txt" } }, finish: "length" },
			text("The call was cut off."),
		],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "read notes.txt" });
			await pi.settled();
		},
	},
	{
		name: "bash-streamed-output",
		description:
			"The model calls bash with a command that prints, waits, and prints again: its output arrives while it runs.",
		replies: [
			{ toolCall: { id: "call_bash_1", name: "bash", arguments: { command: BASH_COMMAND } } },
			text("Both lines printed."),
		],
		run: async (pi) => {
			pi.send({ id: "1", type: "prompt", message: "run it" });
			await pi.settled();
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
		if (key === "firstKeptEntryId") return "<entry-id>";
		if (key === "timestamp") return "<timestamp>";
		if (key === "error" && replaced.startsWith("Failed to parse command: ")) {
			return "Failed to parse command: <parse-error>";
		}
		return replaced;
	}
	if (typeof value === "number") {
		if (key === "timestamp") return "<timestamp>";
		if (key === "durationMs") return "<duration-ms>";
		if (key === "wall_time_seconds") return "<seconds>";
		return value;
	}
	if (Array.isArray(value)) return value.map((item) => normalize(item, volatile));
	if (value === null || typeof value !== "object") return value;
	const object = value as Json;
	if (key === "usage") return normalizeUsage(object);
	if (key === "entry") return normalizeEntry(object, volatile);
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

/** A session entry of an entry_appended record: its ids are random. */
function normalizeEntry(entry: Json, volatile: Volatile): Json {
	return Object.fromEntries(
		Object.entries(entry).map(([name, item]) => [
			name,
			["id", "parentId", "targetId"].includes(name) && typeof item === "string"
				? "<entry-id>"
				: normalize(item, volatile, name),
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
	await writeFile(join(dir, "agent", "settings.json"), JSON.stringify({ ...SETTINGS, ...scenario.settings }));
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
		...(scenario.settings ? { settings: scenario.settings } : {}),
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

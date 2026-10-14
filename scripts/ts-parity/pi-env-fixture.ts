import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BACKGROUND_CONTEXT, withAbortSignal } from "/chord-reference/packages/chord/src/context/index.ts";
import { Connection } from "/chord-reference/packages/env/src/connection.ts";
import { RemoteExecutionEnv } from "/chord-reference/packages/env/src/remote-env.ts";
import { openTerminal } from "/chord-reference/packages/env/src/terminal.ts";

/**
 * Goldens for `noel-env serve` (crates/noel-env-daemon): upstream pi-env's own client (`Connection`,
 * `RemoteExecutionEnv`, `openTerminal`) drives upstream's own daemon, built from the pinned checkout
 * (/usr/local/bin/pi-env in the ts-parity image), through a tap (pi-env-tap.mjs) that records the bytes both ways.
 * The frames are written to pi-env.json; crates/noel-env-daemon/tests/pi_env_golden.rs sends the client's frames to
 * `noel-env serve` and compares what it answers.
 *
 * Run with Node's type stripping and the `source` export condition, so upstream's packages load from their sources:
 * `node --conditions=source /ts-parity/pi-env-fixture.ts <fixtures-dir>`.
 */

const REFERENCE = "/chord-reference";
const DAEMON = "/usr/local/bin/pi-env";
const TAP = join(import.meta.dirname, "pi-env-tap.mjs");
const context = BACKGROUND_CONTEXT;

const FRAME_TYPES: Record<number, string> = {
	1: "request",
	2: "result",
	3: "error",
	4: "event",
	5: "cancel",
	6: "ping",
};
/**
 * Daemon fields that differ from machine to machine or run to run: `hello`'s description of the machine, process
 * ids, and file identities and times.
 */
const MASKED = ["version", "os", "arch", "home", "tmpdir", "cwd", "pid", "dev", "ino", "mtimeSec", "mtimeNsec"];

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Frame = { type: number; id: number; json: Json; payload: Buffer };

// The checkout belongs to root; the fixture runs as the caller.
const git = (...args: string[]): string =>
	execFileSync("git", ["-c", `safe.directory=${REFERENCE}`, "-C", REFERENCE, ...args], { encoding: "utf8" }).trim();

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Strings with the scenario's root as `<root>`. */
function rooted(value: Json, root: string): Json {
	if (typeof value === "string") return value.split(root).join("<root>");
	if (Array.isArray(value)) return value.map((item) => rooted(item, root));
	if (value !== null && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([key, field]) => [key, rooted(field, root)]));
	}
	return value;
}

/**
 * A daemon frame's JSON as the goldens keep it: masked fields as `<field>`, a directory's size (which depends on the
 * file system) as `<size>`, and `readdir` entries (in the file system's order) sorted by name.
 */
function normalized(value: Json): Json {
	if (Array.isArray(value)) return value.map(normalized);
	if (value === null || typeof value !== "object") return value;
	const object: { [key: string]: Json } = {};
	for (const [key, field] of Object.entries(value)) {
		object[key] = MASKED.includes(key) ? `<${key}>` : normalized(field);
	}
	if (object.kind === "directory" && "size" in object) object.size = "<size>";
	if (Array.isArray(object.entries)) {
		const name = (entry: Json) => (entry !== null && typeof entry === "object" && !Array.isArray(entry) ? String(entry.name) : "");
		object.entries = [...object.entries].sort((a, b) => Buffer.compare(Buffer.from(name(a)), Buffer.from(name(b))));
	}
	return object;
}

/**
 * Payload bytes: UTF-8 text, hex, or for long periodic content (file chunks) the period and the length.
 */
function payloadJson(bytes: Buffer): Json {
	const text = bytes.toString("utf8");
	const utf8 = Buffer.from(text, "utf8").equals(bytes);
	if (bytes.length > 256) {
		for (let period = 1; period <= 64; period++) {
			let periodic = true;
			for (let index = period; index < bytes.length && periodic; index++) {
				periodic = bytes[index] === bytes[index - period];
			}
			const unit = bytes.subarray(0, period);
			if (periodic && Buffer.from(unit.toString("utf8"), "utf8").equals(unit)) {
				return { repeat: unit.toString("utf8"), length: bytes.length };
			}
		}
	}
	return utf8 ? { utf8: text } : { hex: bytes.toString("hex") };
}

/** Frames in the order the tap saw them complete, both directions merged. */
function parseTap(log: Buffer, token: string): { from: "client" | "daemon"; frame: Frame }[] {
	const buffers = { client: Buffer.alloc(0), daemon: Buffer.alloc(0) };
	let synced = false;
	const sync = Buffer.from(`PI-ENV ${token}\n`);
	const frames: { from: "client" | "daemon"; frame: Frame }[] = [];
	for (let offset = 0; offset < log.length; ) {
		const from = log[offset] === 0 ? "client" : "daemon";
		const length = log.readUInt32BE(offset + 1);
		buffers[from] = Buffer.concat([buffers[from], log.subarray(offset + 5, offset + 5 + length)]);
		offset += 5 + length;
		if (from === "daemon" && !synced) {
			if (buffers.daemon.length < sync.length) continue;
			if (!buffers.daemon.subarray(0, sync.length).equals(sync)) throw new Error("the daemon did not sync first");
			buffers.daemon = buffers.daemon.subarray(sync.length);
			synced = true;
		}
		for (;;) {
			const buffer = buffers[from];
			if (buffer.length < 4) break;
			const size = buffer.readUInt32BE(0);
			if (buffer.length < 4 + size) break;
			const jsonLength = buffer.readUInt32BE(9);
			frames.push({
				from,
				frame: {
					type: buffer.readUInt8(4),
					id: buffer.readUInt32BE(5),
					json: JSON.parse(buffer.subarray(13, 13 + jsonLength).toString("utf8")) as Json,
					payload: Buffer.from(buffer.subarray(13 + jsonLength, 4 + size)),
				},
			});
			buffers[from] = buffer.subarray(4 + size);
		}
	}
	if (buffers.client.length > 0 || buffers.daemon.length > 0) throw new Error("a frame was cut off");
	return frames;
}

type Step = { send: Json } | { receive: Json };

function frameJson(frame: Frame, json: Json): Json {
	return {
		type: FRAME_TYPES[frame.type] ?? frame.type,
		id: frame.id,
		json,
		...(frame.payload.length > 0 ? { payload: payloadJson(frame.payload) } : {}),
	};
}

/**
 * The goldens' steps: the client's frames, and the daemon's in the order the tap saw them. A request's `output` events
 * are merged per stream into one event each, placed right before the request's result or error, since how output is
 * split into events depends on timing. Pings are kept only where the scenario asks for them: the first each way, at
 * the end (the client's first), since when each side pings depends on when it started.
 */
function steps(frames: { from: "client" | "daemon"; frame: Frame }[], root: string, keepPings: boolean): Step[] {
	const result: Step[] = [];
	const output = new Map<number, Map<string, Buffer[]>>();
	const pings: { client?: Step; daemon?: Step } = {};
	for (const { from, frame } of frames) {
		if (frame.type === 6) {
			if (keepPings && pings[from] === undefined) {
				pings[from] = from === "client" ? { send: frameJson(frame, frame.json) } : { receive: frameJson(frame, frame.json) };
			}
			continue;
		}
		if (from === "client") {
			result.push({ send: frameJson(frame, rooted(frame.json, root)) });
			continue;
		}
		const json = normalized(rooted(frame.json, root)) as { [key: string]: Json };
		if (frame.type === 4 && json.kind === "output") {
			if ("skipped" in json) throw new Error("the goldens do not record windowed output");
			const streams = output.get(frame.id) ?? new Map<string, Buffer[]>();
			output.set(frame.id, streams);
			const stream = typeof json.stream === "string" ? json.stream : "";
			streams.set(stream, [...(streams.get(stream) ?? []), frame.payload]);
			continue;
		}
		if (frame.type === 2 || frame.type === 3) {
			const streams = output.get(frame.id);
			for (const stream of ["stdout", "stderr", ""]) {
				const chunks = streams?.get(stream);
				if (chunks === undefined) continue;
				const event = { type: 4, id: frame.id, json: stream === "" ? { kind: "output" } : { kind: "output", stream }, payload: Buffer.concat(chunks) };
				result.push({ receive: frameJson(event, event.json) });
			}
			output.delete(frame.id);
		}
		result.push({ receive: frameJson(frame, json) });
	}
	if (output.size > 0) throw new Error("output of a request that did not finish");
	if (keepPings) {
		if (pings.client === undefined || pings.daemon === undefined) throw new Error("no ping each way");
		result.push(pings.client, pings.daemon);
	}
	return result;
}

type Scenario = {
	name: string;
	description: string;
	keepPings?: boolean;
	run: (env: RemoteExecutionEnv, connection: Connection, root: string) => Promise<void>;
};

function check(condition: unknown, what: string): void {
	if (!condition) throw new Error(`unexpected: ${what}`);
}

const SCENARIOS: Scenario[] = [
	{
		name: "files",
		description:
			"hello; write (one frame, and kept open with writeChunk) and lstat; open and pread, fstat, scanLines; readdir; mkdir, rename, truncate, fsync, realpath, rm, listFiles; ENOENT, EISDIR, ERR_FS_EISDIR and EINVAL errors",
		run: async (env, connection) => {
			const ok = <T>(result: { ok: true; value: T } | { ok: false; error: { code: string } }, what: string): T => {
				if (!result.ok) throw new Error(`${what}: ${result.error.code}`);
				return result.value;
			};
			ok(await env.writeFile("a.txt", "hello\nworld\n", context), "writeFile a.txt");
			ok(await env.fileInfo("a.txt", context), "fileInfo a.txt");
			check(new TextDecoder().decode(ok(await env.readBinaryFile("a.txt", context), "readBinaryFile")) === "hello\nworld\n", "a.txt");
			// More than one 512 KiB write: `write` keeps the file open for `writeChunk`.
			const big = "0123456789abcdef\n".repeat(35_295).slice(0, 600_000);
			ok(await env.writeFile("big.txt", big, context), "writeFile big.txt");
			const reader = ok(await env.openBinaryReader("big.txt", undefined, context), "openBinaryReader");
			check(new TextDecoder().decode(ok(await reader.read(524_280, 16, context), "read")) === big.slice(524_280, 524_296), "a range across chunks");
			ok(await reader.info(context), "info");
			ok(await reader.scanLines({ startLine: 30_000, endLine: 30_002 }, context), "scanLines");
			await reader.close(context);
			ok(await env.createDir("d/e", { recursive: true }, context), "createDir");
			ok(await env.writeFile("d/e/f.txt", "f", context), "writeFile d/e/f.txt");
			ok(await env.listDir(".", context), "listDir");
			ok(await env.renameFile("d/e/f.txt", "d/g.txt", context), "renameFile");
			ok(await env.truncateFile("d/g.txt", 0, context), "truncateFile");
			ok(await env.flushFile("a.txt", context), "flushFile");
			ok(await env.canonicalPath("d/../a.txt", context), "canonicalPath");
			ok(await env.listFiles!(".", undefined, context), "listFiles");
			ok(await env.remove("d/e", { recursive: true }, context), "remove");
			// Errors.
			check(!(await env.fileInfo("missing", context)).ok, "lstat of a missing file");
			check(!(await env.readBinaryFile("d", context)).ok, "reading a directory");
			check(!(await env.openBinaryReader("d", undefined, context)).ok, "a binary reader of a directory");
			check(!(await env.flushFile("d", context)).ok, "fsync of a directory");
			check(!(await env.remove("d", undefined, context)).ok, "rm of a directory");
			await connection.request("bogus", {}).then(
				() => check(false, "an unknown operation"),
				() => {},
			);
			await connection.request("lstat", {}).then(
				() => check(false, "lstat without a path"),
				() => {},
			);
		},
	},
	{
		name: "exec",
		description:
			"exec of a shell string and of argv with output events, a signal exit and undecodable output; cancel (abort and kill); timeout; spawn_error and shell_unavailable",
		run: async (env, connection, root) => {
			const output: string[] = [];
			const onOutput = (text: string) => output.push(text);
			const done = await env.exec("echo out; echo err >&2", { onOutput }, context);
			check(done.ok && done.value.exitCode === 0, "echo");
			check(output.join("").includes("out\n"), "echo output");
			const exited = await env.exec(["sh", "-c", "printf 'a\\377b'; exit 3"], { onOutput }, context);
			check(exited.ok && exited.value.exitCode === 3, "exit 3");
			// Abort while it runs: a cancel.
			const controller = new AbortController();
			const sleeping = env.exec(["sleep", "30"], undefined, withAbortSignal(controller.signal, context));
			await sleep(200);
			controller.abort();
			const aborted = await sleeping;
			check(!aborted.ok && aborted.error.code === "aborted", "abort");
			// cleanup(): a cancel with mode kill; the command settles with its status.
			const killed = env.exec(["sleep", "30"], undefined, context);
			await sleep(200);
			await env.cleanup(context);
			const settled = await killed;
			check(settled.ok && settled.value.exitCode === 137, "kill");
			const timedOut = await env.exec(["sleep", "5"], { timeout: 0.3 }, context);
			check(!timedOut.ok && timedOut.error.code === "timeout", "timeout");
			const missingCwd = await env.exec("true", { cwd: "missing" }, context);
			check(!missingCwd.ok && missingCwd.error.code === "spawn_error", "a missing cwd");
			const missingProgram = await env.exec(["/nonexistent/program"], undefined, context);
			check(!missingProgram.ok && missingProgram.error.code === "spawn_error", "a missing program");
			const noShell = new RemoteExecutionEnv({ connection, id: "pi-env:golden", cwd: root, shellPath: "/nonexistent/sh" });
			const unavailable = await noShell.exec("true", undefined, context);
			check(!unavailable.ok && unavailable.error.code === "shell_unavailable", "a missing shell");
		},
	},
	{
		name: "pty",
		description: "pty with started and raw output events; ptyResize, ptyForeground, ptyWrite; EBADF once it has ended",
		run: async (_env, connection, root) => {
			const chunks: Buffer[] = [];
			const terminal = await openTerminal(connection, {
				cwd: root,
				argv: ["sh", "-c", 'IFS= read -r line; printf "got %s\\n" "$line"; exit 3'],
				env: { TERM: "dumb" },
				cols: 80,
				rows: 24,
				onData: (bytes) => chunks.push(Buffer.from(bytes)),
			});
			// `RemoteTerminal.resize` and `write` do not wait for their replies, so whether a reply comes before the next
			// frame would be timing; where a frame follows, the request is made and awaited through the connection. The
			// terminal is request 2, after hello.
			const target = 2;
			await connection.request("ptyResize", { target, cols: 100, rows: 30 });
			check((await terminal.foreground()) === undefined, "the program in front");
			terminal.write("abc\n");
			check((await terminal.exited) === 3, "exit 3");
			check(Buffer.concat(chunks).toString("utf8").includes("got abc"), "the program's output");
			await connection.request("ptyWrite", { target }, { payload: Buffer.from("late\n") }).then(
				() => check(false, "a write to an ended terminal"),
				() => {},
			);
			await terminal.foreground().then(
				() => check(false, "a foreground of an ended terminal"),
				() => {},
			);
		},
	},
	{
		name: "watch",
		description: "watch: ready, a change for a written file, and the result once cancelled",
		run: async (env, _connection, root) => {
			let changed: () => void = () => {};
			const seen = new Promise<void>((resolve) => (changed = resolve));
			const watcher = await env.watch(
				[{ path: ".", recursive: true }],
				(change) => {
					if ("paths" in change && change.paths.includes(join(root, "w.txt"))) changed();
				},
				context,
			);
			check(watcher.ok, "watch");
			if (!watcher.ok) return;
			const written = await env.writeFile("w.txt", "w", context);
			check(written.ok, "writeFile w.txt");
			await Promise.race([seen, sleep(5000).then(() => check(false, "a change within 5 s"))]);
			await watcher.value.close(context);
		},
	},
	{
		name: "liveness",
		description: "hello, then a ping each way (both sides ping every 5 s)",
		keepPings: true,
		run: async () => {
			await sleep(5_500);
		},
	},
];

async function record(scenario: Scenario, logs: string): Promise<{ steps: Step[]; version: string }> {
	const root = mkdtempSync(join(tmpdir(), "pi-env-golden-"));
	const log = join(logs, `${scenario.name}.log`);
	writeFileSync(log, "");
	const connection = new Connection({ command: [process.execPath, TAP, log, DAEMON] });
	try {
		const env = new RemoteExecutionEnv({ connection, id: "pi-env:golden", cwd: root });
		const info = await connection.info();
		await scenario.run(env, connection, root);
		connection.close();
		// The tap appends synchronously; give the daemon a moment to exit.
		await sleep(300);
		const bytes = readFileSync(log);
		const token = (() => {
			const text = bytes.toString("latin1");
			const match = /PI-ENV ([0-9a-f]+)\n/.exec(text);
			if (!match) throw new Error("no sync line");
			return match[1]!;
		})();
		return { steps: steps(parseTap(bytes, token), root, scenario.keepPings === true), version: info.version };
	} finally {
		connection.close();
		rmSync(root, { recursive: true, force: true });
	}
}

async function main(): Promise<void> {
	const fixturesDir = process.argv[2];
	if (!fixturesDir) throw new Error("usage: pi-env-fixture.ts <fixtures-dir>");
	const logs = mkdtempSync(join(tmpdir(), "pi-env-golden-logs-"));
	const scenarios = [];
	let version = "";
	try {
		for (const scenario of SCENARIOS) {
			const recorded = await record(scenario, logs);
			version = recorded.version;
			scenarios.push({ name: scenario.name, description: scenario.description, steps: recorded.steps });
		}
	} finally {
		rmSync(logs, { recursive: true, force: true });
	}
	writeFileSync(
		join(fixturesDir, "pi-env.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: git("rev-parse", "HEAD"),
					package: "packages/env",
					envCommit: git("log", "-1", "--format=%H", "HEAD", "--", "packages/env"),
					daemon: `pi-env ${version}, built from packages/env/daemon at ref (release, musl)`,
					script: fileURLToPath(import.meta.url),
				},
				format: {
					scenarios:
						"Each scenario is one connection: upstream's Connection starts the daemon through a tap, its first request is hello, and the scenario's calls follow, each awaited. The daemon's working directory is the scenario's own mkdtemp directory, <root>.",
					steps:
						"send: a frame the client sent. receive: a frame the daemon sent, in the order the tap saw them. Types: request 1, result 2, error 3, event 4, cancel 5, ping 6.",
					output:
						"A request's output events are merged per stream (stdout, stderr; none for pty) and placed right before its result or error; how output splits into events depends on timing.",
					normalized: `In daemon frames, ${MASKED.join(", ")} are "<field>", a directory's size is "<size>", and readdir entries are sorted by name. <root> is the scenario's directory in both directions.`,
					payload:
						"Payload bytes as {utf8}, {hex}, or {repeat, length} for content of a short period. No payload: no field.",
					pings: "Pings are kept only in the liveness scenario: the first each way, at its end.",
				},
				scenarios,
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

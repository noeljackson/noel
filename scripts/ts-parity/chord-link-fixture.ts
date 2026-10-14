import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "/chord-reference/node_modules/typebox/build/index.mjs";
import {
	type AcceptOptions,
	type Ctx,
	createHost,
	type Frame,
	forbidden,
	type Host,
	type Link,
	type LinkOptions,
	method,
	type Principal,
	remote,
	type WebSocketLike,
	webSocketLink,
} from "/chord-reference/packages/chord/src/index.ts";

/**
 * Goldens for noel-chord's link: every frame upstream Chord's runtime puts on
 * its links in a set of small scenarios, with noel's own kind of contracts.
 *
 * Chord is read from /chord-reference, a checkout of its own pinned commit
 * (CHORD_REFERENCE_REF in the Makefile).
 *
 * Topology: a server host; a client host over a WebSocket link (an in-memory
 * WebSocket pair, so the run is deterministic); worker hosts named
 * `session:<id>`, spawned by the server's `session:` route over in-memory
 * links. Raw peers are bare links driven by hand. Every link delivers each
 * frame in its own macrotask, as a socket does, and records what each side
 * sends.
 */

const CHORD = "/chord-reference";

const git = (...args: string[]): string =>
	execFileSync("git", ["-C", CHORD, ...args], { encoding: "utf8" }).trim();

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
/** Lets in-memory traffic finish: every link delivers in its own macrotask. */
const settle = () => sleep(25);

// ─── Recording ───────────────────────────────────────────────────────────────

type Entry = { link: string; from: string; frame: Frame };
let log: Entry[] = [];

/** Root ids carry a random incarnation (`server#k3x9q2:4`); it is replaced by `*`. */
function normalize(frame: Frame): Frame {
	const text = JSON.stringify(frame).replace(/"rootId":"([^"#]*)#[a-z0-9]*:/g, '"rootId":"$1#*:');
	return JSON.parse(text) as Frame;
}

function record(link: string, from: string, data: string): void {
	const frame = JSON.parse(data) as Frame;
	// Acknowledgements are left out: when one is sent depends on timer order, and nothing in these scenarios depends
	// on them except the flow scenarios, which hold them back by hand (see `gate`).
	if (frame.t === "ack") return;
	log.push({ link, from, frame: normalize(frame) });
}

// ─── Links ───────────────────────────────────────────────────────────────────

/** An in-memory link end: FIFO, one macrotask per frame, `bye` on close. */
class TapLink implements Link {
	peer: TapLink | undefined;
	readonly #link: string;
	readonly #side: string;
	#handler: ((frame: Frame) => void) | undefined;
	#queue: Frame[] = [];
	readonly #closeHandlers = new Set<(reason: string) => void>();
	#closed = false;

	constructor(link: string, side: string) {
		this.#link = link;
		this.#side = side;
	}

	get closed(): boolean {
		return this.#closed;
	}

	send(frame: Frame): number | undefined {
		if (this.#closed) return undefined;
		return this.#transmit(frame);
	}

	#transmit(frame: Frame): number {
		const data = JSON.stringify(frame);
		record(this.#link, this.#side, data);
		const peer = this.peer;
		setImmediate(() => peer?.#arrive(JSON.parse(data) as Frame));
		return data.length;
	}

	#arrive(frame: Frame): void {
		if (this.#closed) return;
		if (frame.t === "bye") {
			this.#ended(frame.reason ?? "peer closed");
			return;
		}
		if (this.#handler === undefined) this.#queue.push(frame);
		else this.#handler(frame);
	}

	receive(handler: (frame: Frame) => void): void {
		this.#handler = handler;
		for (const frame of this.#queue.splice(0)) handler(frame);
	}

	onClose(handler: (reason: string) => void): void {
		if (this.#closed) handler("closed");
		else this.#closeHandlers.add(handler);
	}

	close(reason = "closed"): void {
		if (this.#closed) return;
		this.#transmit({ t: "bye", reason });
		this.#ended(reason);
	}

	#ended(reason: string): void {
		if (this.#closed) return;
		this.#closed = true;
		this.#queue = [];
		const peer = this.peer;
		setImmediate(() => peer?.#ended("peer closed"));
		for (const handler of this.#closeHandlers) handler(reason);
		this.#closeHandlers.clear();
	}
}

function tapPair(link: string, a: string, b: string): [TapLink, TapLink] {
	const left = new TapLink(link, a);
	const right = new TapLink(link, b);
	left.peer = right;
	right.peer = left;
	return [left, right];
}

/** One end of an in-memory WebSocket: text messages, one macrotask each. */
class FakeSocket implements WebSocketLike {
	readyState = 1;
	peer: FakeSocket | undefined;
	readonly #link: string;
	readonly #side: string;
	readonly #listeners = new Map<string, ((event: { readonly data: unknown }) => void)[]>();

	constructor(link: string, side: string) {
		this.#link = link;
		this.#side = side;
	}

	send(data: string | Uint8Array): void {
		if (typeof data !== "string") throw new TypeError("the fixture expects text messages only");
		record(this.#link, this.#side, data);
		const peer = this.peer;
		setImmediate(() => {
			if (peer !== undefined && peer.readyState === 1) peer.#emit("message", { data });
		});
	}

	close(): void {
		if (this.readyState >= 2) return;
		this.readyState = 3;
		const peer = this.peer;
		setImmediate(() => {
			this.#emit("close", { data: undefined });
			if (peer !== undefined && peer.readyState < 2) {
				peer.readyState = 3;
				peer.#emit("close", { data: undefined });
			}
		});
	}

	addEventListener(type: string, listener: (event: { readonly data: unknown }) => void): void {
		const listeners = this.#listeners.get(type) ?? [];
		listeners.push(listener);
		this.#listeners.set(type, listeners);
	}

	#emit(type: string, event: { readonly data: unknown }): void {
		for (const listener of this.#listeners.get(type) ?? []) listener(event);
	}
}

/** A client's link that can hold back its acknowledgements (the latest one waits; release sends it). */
class AckGate implements Link {
	readonly inner: Link;
	paused = false;
	held: Frame | undefined;
	constructor(inner: Link) {
		this.inner = inner;
	}
	get closed(): boolean {
		return this.inner.closed;
	}
	send(frame: Frame): number | undefined {
		if (frame.t === "ack" && this.paused) {
			this.held = frame;
			return 0;
		}
		return this.inner.send(frame);
	}
	release(): Frame | undefined {
		this.paused = false;
		const held = this.held;
		if (held !== undefined) this.inner.send(held);
		this.held = undefined;
		return held;
	}
	receive(handler: (frame: Frame) => void): void {
		this.inner.receive(handler);
	}
	onClose(handler: (reason: string) => void): void {
		this.inner.onClose(handler);
	}
	close(reason?: string): void {
		this.inner.close(reason);
	}
}

// ─── Contracts ───────────────────────────────────────────────────────────────

const CountSchema = Type.Object({ n: Type.Integer() });

/** A counter on the server: a state cell and methods. Owners get `owner`; guests `viewer`. */
const Counter = remote("noel.test.counter", {
	version: 1,
	input: Type.Object({ name: Type.String() }),
	variants: {
		owner: {
			state: { count: CountSchema },
			methods: {
				add: method(Type.Integer(), Type.Integer()),
				set: method(Type.Integer()),
				wait: method(),
				fail: method(undefined, Type.Integer()),
			},
		},
		viewer: { state: { count: CountSchema } },
	},
});

/** The same type and version, but a consumer expecting another result: not covered. */
const CounterDrifted = remote("noel.test.counter", {
	version: 1,
	input: Type.Object({ name: Type.String() }),
	variants: {
		owner: {
			state: { count: CountSchema },
			methods: { add: method(Type.Integer(), Type.String()) },
		},
	},
});

/** A newer contract version, which the provider has no adapter for. */
const CounterV2 = remote("noel.test.counter", {
	version: 2,
	input: Type.Object({ name: Type.String() }),
	variants: { owner: { state: { count: CountSchema } } },
});

/** A type nobody serves. */
const Missing = remote("noel.test.missing", {
	version: 1,
	input: Type.Object({}),
	variants: { main: { methods: { ping: method() } } },
});

/** A session object on a `session:<id>` worker, reached through the server's route. */
const Session = remote("noel.test.session", {
	version: 1,
	input: Type.Object({ session: Type.String() }),
	variants: {
		participant: {
			state: { log: Type.Array(Type.String()) },
			methods: {
				say: method(Type.String(), Type.Integer()),
				who: method(undefined, Type.Unknown()),
				wait: method(),
			},
		},
		viewer: { state: { log: Type.Array(Type.String()) } },
	},
});

// ─── Principals and serving ──────────────────────────────────────────────────

type P = { kind?: string; session?: string; mode?: string } & Record<string, unknown>;

const owner = { kind: "owner", via: "device" };
const guest = (session: string, mode = "full", revision = 1) => ({
	kind: "guest",
	id: "g1",
	name: "Guest",
	session,
	mode,
	revision,
});

/** Waits until the call's ctx is aborted, then rejects with its reason. */
const waitForAbort = (ctx: Ctx): Promise<never> =>
	new Promise((_, reject) => {
		if (ctx.signal.aborted) reject(ctx.signal.reason);
		else ctx.signal.addEventListener("abort", () => reject(ctx.signal.reason), { once: true });
	});

function serveCounter(host: Host): void {
	host.serve(Counter, (input, principal, self) => {
		const who = principal as P;
		if (who.kind !== "owner" && who.kind !== "guest") throw forbidden(`${input.name} is not for this principal`);
		if (who.kind !== "owner" && input.name.startsWith("private")) throw forbidden(`${input.name} is private`);
		const count = self.state("count", CountSchema, { n: 0 });
		if (who.kind !== "owner") return { variant: "viewer", count };
		return {
			variant: "owner",
			count,
			add: (delta: number, ctx: Ctx) => {
				count.change(ctx, (draft) => {
					draft.n += delta;
				});
				return count.value.n;
			},
			set: (value: number, ctx: Ctx) => {
				count.replace(ctx, { n: value });
			},
			wait: waitForAbort,
			fail: () => {
				throw new Error("boom");
			},
		};
	});
}

function serveSession(host: Host): void {
	host.serve(Session, (input, principal, self) => {
		const who = principal as P;
		const allowed = who.kind === "owner" || (who.kind === "guest" && who.session === input.session);
		if (!allowed) throw forbidden(`session ${input.session} is not for this principal`);
		const log = self.state("log", Type.Array(Type.String()), [] as string[]);
		if (who.kind === "guest" && who.mode === "view") return { variant: "viewer", log };
		return {
			variant: "participant",
			log,
			say: (text: string, ctx: Ctx) => {
				log.change(ctx, (draft) => {
					draft.push(text);
				});
				return log.value.length;
			},
			who: () => principal,
			wait: waitForAbort,
		};
	});
}

const errors: unknown[] = [];
const onError = (error: unknown) => errors.push(error);

/** The server, with the `session:` route spawning workers over in-memory links. */
function server(): Host {
	const host = createHost({ name: "server", onError });
	serveCounter(host);
	const workers = new Map<string, Link>();
	host.route("session:", {
		authorize: (principal, hostName) => {
			const who = principal as P;
			return who.kind === "owner" || (who.kind === "guest" && hostName === `session:${who.session}`);
		},
		resolve: async (hostName) => {
			let link = workers.get(hostName);
			if (link === undefined) {
				const [down, up] = tapPair("worker", "server", "worker");
				const worker = createHost({ name: hostName, upstream: up, onError });
				serveSession(worker);
				workers.set(hostName, down);
				hosts.push(worker);
				link = down;
			}
			return link;
		},
		principal: (hostName) => ({ kind: "worker", session: hostName.slice("session:".length), generation: 1 }),
	});
	hosts.push(host);
	return host;
}

let hosts: Host[] = [];

/** A client host over an in-memory WebSocket, accepted by `server` as `principal`. */
function client(
	server: Host,
	principal: Principal,
	options: {
		readonly link?: string;
		readonly accept?: Partial<AcceptOptions>;
		readonly upstream?: LinkOptions;
		readonly gate?: boolean;
	} = {},
) {
	const name = options.link ?? "client";
	const serverSocket = new FakeSocket(name, "server");
	const clientSocket = new FakeSocket(name, "client");
	serverSocket.peer = clientSocket;
	clientSocket.peer = serverSocket;
	// As acceptWebSockets accepts a socket.
	server.accept(webSocketLink(serverSocket, { compress: true, acceptPacked: false }), {
		...options.accept,
		principal,
		kind: "client",
	});
	const link = webSocketLink(clientSocket);
	const gate = options.gate === true ? new AckGate(link) : undefined;
	const host = createHost({
		name: "client",
		kind: "client",
		upstream: gate ?? link,
		upstreamOptions: options.upstream,
		onError,
	});
	hosts.push(host);
	return { host, gate };
}

/** A bare link end accepted by `server`; the fixture sends its frames by hand. */
function raw(server: Host, link: string, accept: Partial<AcceptOptions> = {}) {
	const [down, up] = tapPair(link, "server", "raw");
	server.accept(down, { principal: guest("s1"), kind: "client", ...accept });
	up.receive(() => {});
	return up;
}

async function closeAll(): Promise<void> {
	// Clients first, then the server, then workers: each closes its links with a `bye`.
	const order = [
		...hosts.filter((host) => host.name === "client"),
		...hosts.filter((host) => host.name === "server"),
		...hosts.filter((host) => host.name !== "client" && host.name !== "server"),
	];
	for (const host of order) {
		await host.close();
		await settle();
	}
	hosts = [];
}

/** Runs `steps` against fresh hosts and returns the frames they sent. */
async function scenario(name: string, description: string, steps: () => Promise<unknown>) {
	log = [];
	hosts = [];
	await steps();
	await settle();
	await closeAll();
	const frames = log;
	log = [];
	return { name, description, frames };
}

const quietly = <T>(promise: Promise<T>): Promise<T | unknown> => promise.catch((error: unknown) => error);

// ─── Scenarios ───────────────────────────────────────────────────────────────

const scenarios = async () => [
	await scenario("hello", "A client connects and leaves: both hellos, then the client's bye.", async () => {
		const s = server();
		client(s, owner);
		await settle();
	}),

	await scenario(
		"hold",
		"A guest holds a counter (shape, variant viewer); holds that fail are gone: forbidden by the factory, an unknown type, a contract that is not covered and one of another version (upgrade_required for a client), and an invalid input. Then it releases the live hold.",
		async () => {
			const s = server();
			const c = client(s, guest("s1")).host;
			const a = c.get(Counter, { name: "a" }, { at: "server" });
			await a.ready;
			for (const ref of [
				c.get(Counter, { name: "private" }, { at: "server" }),
				c.get(Missing, {}, { at: "server" }),
				c.get(CounterDrifted, { name: "a" }, { at: "server" }),
				c.get(CounterV2, { name: "a" }, { at: "server" }),
				c.get(Counter, { name: 5 } as never, { at: "server" }),
			]) {
				await quietly(ref.ready);
			}
			a.release();
			await settle();
		},
	),

	await scenario(
		"state",
		"An owner subscribes to a counter: snapshot, then ops through one delta encoder (an inline path, then a definition and references, an `r` that clears them), unsubscribes, subscribes again for a new snapshot, and releases.",
		async () => {
			const s = server();
			const c = client(s, owner).host;
			const a = c.get(Counter, { name: "a" }, { at: "server" });
			const stop = a.count.subscribe(() => {});
			await a.ready;
			await settle();
			await a.add(1);
			await a.add(2);
			await a.add(3);
			await a.set(10);
			await a.add(1);
			await settle();
			stop();
			await settle();
			a.count.subscribe(() => {});
			await settle();
			a.release();
			await settle();
		},
	),

	await scenario(
		"call",
		"Calls with a result, a void result, invalid arguments (invalid/args) and a method that throws (unavailable).",
		async () => {
			const s = server();
			const c = client(s, owner).host;
			const a = c.get(Counter, { name: "a" }, { at: "server" });
			await a.ready;
			await a.add(5);
			await a.set(7);
			await quietly(a.add("x" as never));
			await quietly(a.fail());
			await settle();
		},
	),

	await scenario(
		"cancel",
		"With maxCalls 1, a waiting call is cancelled by the caller (aborted), a second call meanwhile is refused at once (unavailable/too_many_calls), and releasing a subscribed hold sends unsub then release.",
		async () => {
			const s = server();
			const c = client(s, owner, { accept: { maxCalls: 1 } }).host;
			const a = c.get(Counter, { name: "a" }, { at: "server" });
			a.count.subscribe(() => {});
			await a.ready;
			await settle();
			const abort = new AbortController();
			const waiting = quietly(c.root((ctx) => a.wait(ctx), { signal: abort.signal }));
			await settle();
			await quietly(a.add(1));
			await settle();
			abort.abort();
			await waiting;
			await settle();
			a.release();
			await settle();
		},
	),

	await scenario(
		"relay",
		"An owner holds a session object on a worker through the server's route: the server stamps the principal on the relayed hold, forwards the subscription, calls (with a root, ended by root_end) and a cancel, then the release.",
		async () => {
			const s = server();
			const c = client(s, owner).host;
			const session = c.get(Session, { session: "s1" }, { at: "session:s1" });
			session.log.subscribe(() => {});
			await session.ready;
			await settle();
			await session.say("hi");
			await session.who();
			await settle();
			const abort = new AbortController();
			const waiting = quietly(c.root((ctx) => session.wait(ctx), { signal: abort.signal }));
			await settle();
			abort.abort();
			await waiting;
			await settle();
			session.release();
			await settle();
		},
	),

	await scenario(
		"refused",
		"A guest of session s1: its hold on s2 is refused by the route (forbidden), a host without a route is unavailable, and a principal it forges in its hold is ignored: the worker sees the stamped guest.",
		async () => {
			const s = server();
			const c = client(s, guest("s1")).host;
			await quietly(c.get(Session, { session: "s2" }, { at: "session:s2" }).ready);
			await quietly(c.get(Session, { session: "s1" }, { at: "nowhere" }).ready);
			const forged = c.get(Session, { session: "s1" }, { at: "session:s1", principal: owner });
			await forged.ready;
			await forged.who();
			await settle();
			forged.release();
			await settle();
		},
	),

	await scenario(
		"invalidate",
		"invalidatePrincipal({kind: guest, id: g1}) on the server: a link accepted with restamp keeps its link under revision 2 and holds again; one without is closed. Relays under the old principal are gone, released downstream, and the pattern propagates to the worker.",
		async () => {
			const s = server();
			const kept = client(s, guest("s1"), {
				accept: { restamp: (principal) => ({ ...(principal as object), revision: 2 }) },
			}).host;
			const closed = client(s, guest("s1", "view"), { link: "client2" }).host;
			const a = kept.get(Session, { session: "s1" }, { at: "session:s1" });
			a.log.subscribe(() => {});
			await a.ready;
			const b = closed.get(Session, { session: "s1" }, { at: "session:s1" });
			await b.ready;
			await settle();
			await s.invalidatePrincipal({ kind: "guest", id: "g1" });
			await settle();
			const again = kept.get(Session, { session: "s1" }, { at: "session:s1" });
			await again.who();
			await settle();
		},
	),

	await scenario(
		"ping",
		"Both sides ping every 100 ms and answer pings with pongs; the run lasts 150 ms.",
		async () => {
			const s = server();
			client(s, owner, { accept: { pingInterval: 100 }, upstream: { pingInterval: 100 } });
			await sleep(150);
		},
	),

	await scenario(
		"ping-timeout",
		"A peer that says hello and then nothing: pings every 100 ms, and after 250 ms without a frame the link closes with `ping timeout`.",
		async () => {
			const s = server();
			const peer = raw(s, "raw", { pingInterval: 100, pingTimeout: 250 });
			peer.send({ t: "hello", protocol: 2, name: "raw", kind: "client", codeVersion: 1 });
			await sleep(400);
		},
	),

	await scenario(
		"flow-coalesce",
		"Window 200: while the client holds back its acknowledgements, state waits once the window is full while results go out; the released ack sends the waiting ops as one frame.",
		async () => {
			const s = server();
			const { host: c, gate } = client(s, owner, { accept: { window: 200 }, gate: true });
			const a = c.get(Counter, { name: "a" }, { at: "server" });
			a.count.subscribe(() => {});
			await a.ready;
			await settle();
			gate!.paused = true;
			for (let i = 0; i < 40; i++) await a.add(1);
			await settle();
			gateEvents.push({ scenario: "flow-coalesce", released: gate!.release() ?? null });
			await settle();
		},
	),

	await scenario(
		"flow-reset",
		"Window 64: a waiting subscription whose ops outgrow four windows is replaced by a reset of the current value; the released ack sends the reset and the ops after it.",
		async () => {
			const s = server();
			const { host: c, gate } = client(s, owner, { accept: { window: 64 }, gate: true });
			const a = c.get(Counter, { name: "a" }, { at: "server" });
			a.count.subscribe(() => {});
			await a.ready;
			await settle();
			gate!.paused = true;
			for (let i = 0; i < 100; i++) await a.add(1);
			await settle();
			gateEvents.push({ scenario: "flow-reset", released: gate!.release() ?? null });
			await settle();
		},
	),

	await scenario(
		"version-old-peer",
		"A peer whose hello has no protocol (version 1): the server answers with its hello, then closes with `protocol-version 2`.",
		async () => {
			const s = server();
			const peer = raw(s, "raw");
			peer.send({ t: "hello", name: "old", kind: "client", codeVersion: 1 } as Frame);
			await settle();
		},
	),

	await scenario(
		"version-new-server",
		"A client whose server says protocol 3: the client closes with `protocol-version 2`.",
		async () => {
			const [down, up] = tapPair("raw", "server", "client");
			down.receive(() => {});
			hosts.push(createHost({ name: "client", kind: "client", upstream: up, onError }));
			down.send({ t: "hello", protocol: 3, name: "server", kind: "host", codeVersion: 1, acks: true });
			await settle();
		},
	),

	await scenario(
		"violations",
		"Protocol violations close the link: a malformed frame, a duplicate hold id, a control frame from a child, a hello that does not match how the peer was accepted, and an ack outside the expected range.",
		async () => {
			const s = server();
			const hello = { t: "hello", protocol: 2, name: "raw", kind: "client", codeVersion: 1 } as const;
			const token = Counter.description;
			const malformed = raw(s, "raw1");
			malformed.send(hello);
			malformed.send({ t: "call", hold: 1 } as never);
			await settle();
			const duplicate = raw(s, "raw2");
			duplicate.send(hello);
			duplicate.send({ t: "hold", id: 1, at: "server", token, input: { name: "a" } });
			await settle();
			duplicate.send({ t: "hold", id: 1, at: "server", token, input: { name: "b" } });
			await settle();
			const control = raw(s, "raw3");
			control.send(hello);
			control.send({ t: "invalidate", pattern: {} });
			await settle();
			const host = raw(s, "raw4");
			host.send({ ...hello, kind: "host", name: "session:x" });
			await settle();
			const ack = raw(s, "raw5");
			ack.send(hello);
			ack.send({ t: "ack", n: 5 });
			await settle();
		},
	),
];

const gateEvents: { scenario: string; released: Frame | null }[] = [];

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: chord-link-fixture.ts <output-dir>");
	}
	const recorded = await scenarios();
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "chord-link.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: git("rev-parse", "HEAD"),
					package: "packages/chord",
					packageCommit: git("log", "-1", "--format=%H", "HEAD", "--", "packages/chord"),
					script: fileURLToPath(import.meta.url),
				},
				format: {
					scenarios:
						"Each scenario starts fresh hosts, runs its steps with a pause after each for in-memory traffic to settle, then closes the clients, the server and the workers in that order. `frames` lists every frame sent, in the order sent, with its link and the side that sent it. Within one link and direction the order is the protocol's; across links it is this run's.",
					links:
						"`client` and `client2` are WebSocket links (an in-memory WebSocket pair: one text message per frame) between a client host and the server, accepted as acceptWebSockets accepts them. `worker` is an in-memory link between the server and the worker its `session:` route spawned. `raw` links are bare peers the fixture drives by hand.",
					omitted:
						"`ack` frames are not recorded: when they are sent depends on timer order. The flow scenarios hold the client's acknowledgements back; `gate` records the one they release.",
					normalized:
						"Root ids carry a random incarnation, replaced by `*` (`server#*:3`). Compare ping and pong `n` and error messages as placeholders.",
					contracts: "The contract descriptions the scenarios use, after a JSON round trip.",
				},
				contracts: {
					counter: JSON.parse(JSON.stringify(Counter.description)),
					counterDrifted: JSON.parse(JSON.stringify(CounterDrifted.description)),
					counterV2: JSON.parse(JSON.stringify(CounterV2.description)),
					missing: JSON.parse(JSON.stringify(Missing.description)),
					session: JSON.parse(JSON.stringify(Session.description)),
				},
				scenarios: recorded,
				gate: gateEvents,
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

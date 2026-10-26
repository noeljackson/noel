import WebSocket from "/chord-reference/node_modules/ws/wrapper.mjs";
import { Type } from "/chord-reference/node_modules/typebox/build/index.mjs";
import {
	hostNamed,
	createHost,
	type Frame,
	type Link,
	method,
	remote,
	type WebSocketLike,
	webSocketLink,
} from "/chord-reference/packages/chord/src/index.ts";

/**
 * Interop: upstream Chord's client host against a noel server, over a real
 * WebSocket on a Unix socket (`ws+unix:///path.sock:/chord`).
 *
 * Holds `noel.test.counter` at `server`, subscribes to `count`, calls `add`
 * twice, and checks that the server sent the snapshot first, then ops.
 * Prints what it saw as JSON lines; exits 0 when it matches. `--hold` keeps
 * the hold until the process is killed.
 *
 * Run inside the ts-parity image (see scripts/chord-interop.sh).
 */

const CountSchema = Type.Object({ n: Type.Integer() });
const Counter = remote("noel.test.counter", {
	host: hostNamed("server"),
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

/** Records the frames the client receives. */
class Tap implements Link {
	readonly inner: Link;
	readonly received: Frame[] = [];
	constructor(inner: Link) {
		this.inner = inner;
	}
	get closed(): boolean {
		return this.inner.closed;
	}
	send(frame: Frame): number | undefined {
		return this.inner.send(frame);
	}
	receive(handler: (frame: Frame) => void): void {
		this.inner.receive((frame) => {
			if (frame.t !== "ack") this.received.push(frame);
			handler(frame);
		});
	}
	onClose(handler: (reason: string) => void): void {
		this.inner.onClose(handler);
	}
	close(reason?: string): void {
		this.inner.close(reason);
	}
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

async function until(check: () => boolean, what: string): Promise<void> {
	for (let i = 0; i < 500 && !check(); i++) await sleep(10);
	if (!check()) throw new Error(`timed out waiting for ${what}`);
}

async function main(): Promise<void> {
	const [url, ...flags] = process.argv.slice(2);
	if (url === undefined) throw new Error("usage: chord-interop-client.ts ws+unix:///path.sock:/chord [--hold]");
	const hold = flags.includes("--hold");
	const socket = new WebSocket(url);
	socket.on("error", (error: Error) => {
		console.error(`socket: ${error.message}`);
		process.exit(1);
	});
	const tap = new Tap(webSocketLink(socket as unknown as WebSocketLike));
	const host = createHost({
		name: "client",
		kind: "client",
		self: { kind: "owner", via: "device" },
		upstream: { link: tap, principal: { kind: "owner", via: "device" } },
		onError: (error) => console.error("chord:", error),
	});
	tap.onClose((reason) => console.log(JSON.stringify({ closed: reason })));
	const counter = await host.get(Counter, { name: "interop" });
	const values: unknown[] = [];
	counter.count.subscribe((value) => values.push(value));
	await counter.ready;
	console.log(JSON.stringify({ shape: counter.shape }));
	const states = () => tap.received.filter((frame) => frame.t === "state");
	await until(() => states().length >= 1, "the snapshot");
	const results = [await counter.add(1), await counter.add(2)];
	await until(() => states().length >= 3, "two ops frames");
	for (const frame of states()) console.log(JSON.stringify(frame));
	console.log(JSON.stringify({ results, value: values.at(-1) }));
	const [snap, first, second] = states();
	const ok =
		snap !== undefined &&
		snap.as === "snap" &&
		JSON.stringify(snap.value) === JSON.stringify({ n: 0 }) &&
		first !== undefined &&
		first.as === "ops" &&
		second !== undefined &&
		second.as === "ops" &&
		JSON.stringify(results) === "[1,3]" &&
		JSON.stringify(values.at(-1)) === JSON.stringify({ n: 3 });
	if (!ok) {
		console.log(JSON.stringify({ ok: false }));
		process.exit(1);
	}
	if (hold) {
		console.log(JSON.stringify({ holding: true }));
		await new Promise(() => {});
	}
	counter.release();
	await host.close();
	console.log(JSON.stringify({ ok: true }));
	process.exit(0);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});

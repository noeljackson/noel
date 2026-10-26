import { createServer } from "node:http";
import { unlinkSync } from "node:fs";
import { WebSocketServer } from "/chord-reference/node_modules/ws/wrapper.mjs";
import { Type } from "/chord-reference/node_modules/typebox/build/index.mjs";
import {
	hostNamed,
	type Ctx,
	createHost,
	type Frame,
	forbidden,
	type Link,
	method,
	remote,
	type WebSocketLike,
	webSocketLink,
} from "/chord-reference/packages/chord/src/index.ts";

/**
 * Interop: upstream Chord's server host, serving `noel.test.counter` on a
 * Unix socket (WebSocket upgrade on `/chord`), for a noel client.
 *
 * Accepts each socket as acceptWebSockets does (deflating large payloads,
 * refusing packed frames from clients), stamped `{kind: owner, via: device}`.
 * Prints the frames it received from the first client as JSON lines when that
 * client leaves, then exits 0; exits 1 after 60 s without one.
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

/** Records the frames the server receives. */
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

const path = process.argv[2];
if (path === undefined) throw new Error("usage: chord-interop-server.ts /path/to/socket");

const host = createHost({ name: "server", self: { kind: "owner", via: "device" }, onError: (error) => console.error("chord:", error) });
host.serve(Counter, (input, principal, self) => {
	const who = principal as { kind?: string };
	if (who.kind !== "owner" && who.kind !== "guest") throw forbidden(`${input.name} is not for this principal`);
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
		wait: (ctx: Ctx) =>
			new Promise<never>((_, reject) => ctx.signal.addEventListener("abort", () => reject(ctx.signal.reason))),
		fail: () => {
			throw new Error("boom");
		},
	};
});

try {
	unlinkSync(path);
} catch {
	// No stale socket.
}
const http = createServer();
const sockets = new WebSocketServer({ server: http, path: "/chord" });
let served = 0;
sockets.on("connection", (socket: unknown) => {
	served++;
	const tap = new Tap(webSocketLink(socket as WebSocketLike, { compress: true, acceptPacked: false }));
	host.accept(tap, { principal: { kind: "owner", via: "device" }, kind: "client" });
	tap.onClose((reason) => {
		for (const frame of tap.received) console.log(JSON.stringify(frame));
		console.log(JSON.stringify({ closed: reason }));
		void host.close().then(() => {
			http.close();
			process.exit(0);
		});
	});
});
http.listen(path, () => console.log(JSON.stringify({ listening: path })));
setTimeout(() => {
	if (served === 0) {
		console.log(JSON.stringify({ ok: false, error: "no client within 60 s" }));
		process.exit(1);
	}
}, 60_000).unref();

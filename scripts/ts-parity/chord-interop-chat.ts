import WebSocket from "/chord-reference/node_modules/ws/wrapper.mjs";
import { Type } from "/chord-reference/node_modules/typebox/build/index.mjs";
import {
	createHost,
	type Frame,
	hostFamily,
	type Link,
	remote,
	type WebSocketLike,
	webSocketLink,
} from "/chord-reference/packages/chord/src/index.ts";

/**
 * Interop: upstream Chord's client host holds `noel.chat` on a noel server
 * (`noel server`, over a real WebSocket on its Unix socket), relayed to the
 * session's worker, and follows its `view`.
 *
 * The consumer declares only the view, with an open schema; noel's contract
 * (`crates/noel-cli/src/server/contracts/noel.chat.json`) covers it, so the
 * hold binds by coverage. Prints `{"subscribed": ...}` once the snapshot came,
 * then waits for `noel client send` (run by scripts/chord-interop.sh) to add
 * two entries, which must arrive as ops. Exits 0 when both hold.
 *
 * Run inside the ts-parity image:
 *   chord-interop-chat.ts ws+unix:///sock/run/server.sock:/chord <sessionId>
 */

const Chat = remote("noel.chat", {
	host: hostFamily("session:", "session"),
	version: 1,
	input: Type.Object({ session: Type.String() }),
	variants: {
		viewer: { state: { view: Type.Unknown() } },
		participant: { state: { view: Type.Unknown() } },
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

async function until(check: () => boolean, what: string, ms = 30_000): Promise<void> {
	for (let waited = 0; waited < ms && !check(); waited += 10) await sleep(10);
	if (!check()) throw new Error(`timed out waiting for ${what}`);
}

type View = { conversation: { id: string }; entries: { role: string }[]; docs: Record<string, unknown> };

async function main(): Promise<void> {
	const [url, session] = process.argv.slice(2);
	if (url === undefined || session === undefined) {
		throw new Error("usage: chord-interop-chat.ts ws+unix:///path.sock:/chord <sessionId>");
	}
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
	const chat = await host.get(Chat, { session });
	const values: View[] = [];
	// A subscriber hears the current value at once: nothing yet, until the
	// snapshot arrives.
	chat.view.subscribe((value) => {
		if (value !== undefined) values.push(value as View);
	});
	await chat.ready;
	console.log(JSON.stringify({ shape: chat.shape }));
	const states = () => tap.received.filter((frame) => frame.t === "state");
	await until(() => states().length >= 1 && values.length >= 1, "the snapshot");
	const snapshot = states()[0];
	const first = values[0] as View;
	console.log(
		JSON.stringify({
			subscribed: true,
			snap: snapshot !== undefined && snapshot.as === "snap",
			conversation: first.conversation,
			entries: first.entries.map((entry) => entry.role),
			docs: Object.keys(first.docs).sort(),
		}),
	);
	const before = first.entries.length;
	await until(() => (values.at(-1)?.entries.length ?? 0) >= before + 2, "two more entries");
	const ops = states().filter((frame) => frame.as === "ops").length;
	const last = values.at(-1) as View;
	console.log(JSON.stringify({ ops, entries: last.entries.map((entry) => entry.role) }));
	const ok =
		snapshot !== undefined &&
		snapshot.as === "snap" &&
		first.conversation.id === session &&
		ops > 0 &&
		last.entries.at(-2)?.role === "user" &&
		last.entries.at(-1)?.role === "assistant";
	chat.release();
	await host.close();
	console.log(JSON.stringify({ ok }));
	process.exit(ok ? 0 : 1);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});

// Chord v2 interop between noel and upstream's packages/chord at CHORD2_REFERENCE_REF, over WebSockets on Unix
// sockets, run with Bun (whose WebSocket client dials `ws+unix://`). Run by scripts/chord2-interop.sh.
//
// Usage: bun --conditions=source scripts/ts-parity/chord2-interop.ts <pi-reference-root> <contracts-dir> <mode> ...
//   chat <server.sock> <session>  upstream's client host holds noel.chat on `noel server` (relayed to the session's
//                                 worker): the view, a submit and its ops, then a dropped link and a re-hold.
//   serve <socket> <counter.json> upstream's server host serves interop.counter for noel's chord2-client example.
// Prints one JSON line per step; the driver checks them.
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
interface Declaration {
	name: string;
	version: number;
	host: "server" | "session" | "plugin";
	params: [string, Json][];
	variants: Record<string, { state?: Record<string, Json>; methods?: Record<string, { args: Json; result: Json }> }>;
}

const say = (step: string, value: Record<string, unknown> = {}) => console.log(JSON.stringify({ step, ...value }));

async function until<T>(what: string, check: () => T | undefined | false, ms = 20_000): Promise<T> {
	const deadline = Date.now() + ms;
	for (;;) {
		const value = check();
		if (value) return value;
		if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
		await new Promise((resolve) => setTimeout(resolve, 20));
	}
}

async function main() {
	const [reference, contractsDir, mode, ...rest] = process.argv.slice(2);
	if (!reference || !contractsDir || !mode) throw new Error("usage: chord2-interop.ts <reference> <contracts-dir> <mode> ...");
	const src = join(reference, "packages/chord/src");
	const chord = await import(join(src, "contracts/index.ts"));
	const { createHost, defineFacet } = await import(join(src, "host/index.ts"));
	const { webSocketLink } = await import(join(src, "link/index.ts"));
	const { defineState } = await import(join(src, "documents/index.ts"));
	const { BACKGROUND_CONTEXT: context } = await import(join(src, "context/index.ts"));
	const Server = chord.hostKind("server");
	const Session = chord.hostKind("session", { id: chord.str() });
	const Plugin = chord.hostKind("plugin", { name: chord.str() });
	const declare = (declaration: Declaration) =>
		chord.remote(declaration.name, {
			version: declaration.version,
			params: Object.fromEntries(declaration.params),
			hostAddress:
				declaration.host === "server"
					? () => Server()
					: declaration.host === "plugin"
						? (params: { plugin: string }) => Plugin({ name: params.plugin })
						: (params: { session: string }) => Session({ id: params.session }),
			variants: Object.fromEntries(
				Object.entries(declaration.variants).map(([variant, members]) => [
					variant,
					{
						...(members.state ? { state: members.state } : {}),
						...(members.methods
							? {
									methods: Object.fromEntries(
										Object.entries(members.methods).map(([name, { args, result }]) => [
											name,
											chord.method(args, result),
										]),
									),
								}
							: {}),
					},
				]),
			),
		});
	const load = (path: string): Declaration => JSON.parse(readFileSync(path, "utf8"));

	if (mode === "chat") {
		const [socket, session] = rest;
		const contracts = readdirSync(contractsDir).filter((file) => file.endsWith(".json"));
		const Chat = declare(load(join(contractsDir, contracts.find((file) => file === "noel.chat.json")!)));
		const host = createHost({ onError: (error: unknown) => console.error("client:", error) });
		const dial = () => {
			const ws = new WebSocket(`ws+unix://${socket}:/chord`);
			host.connect(webSocketLink(ws as never), { principal: { kind: "server" } }, context);
			return ws;
		};
		let ws = dial();
		await host.load([], context);
		const chat = await host.get(Chat, { session }, context);
		say("held", { variant: chat.variant, entries: chat.view.value.entries.length });
		const submitted = await chat.submit({ requestId: crypto.randomUUID(), text: "hello from upstream" }, context);
		say("submitted", { disposition: submitted.disposition });
		const settled = await until("the run to settle", () => chat.view.value.lastRun?.status === "completed" && chat.view.value);
		say("settled", { entries: settled.entries.length, last: settled.entries.at(-1)?.kind });
		// A dropped link: the reference goes loading, then binds again over the next one.
		ws.close();
		await new Promise((resolve) => setTimeout(resolve, 100));
		ws = dial();
		const page = await until("a call after the re-hold", () => true).then(async () => {
			for (let attempt = 0; ; attempt++) {
				try {
					return await chat.entries({ before: settled.entries.at(-1).id + 1 }, context);
				} catch (error) {
					if (attempt > 100) throw error;
					await new Promise((resolve) => setTimeout(resolve, 50));
				}
			}
		});
		say("reheld", { entries: page.entries.length, view: chat.view.value.entries.length });
		await host.close(context);
		ws.close();
		return;
	}

	if (mode === "serve") {
		const [socket, counterPath] = rest;
		const Counter = declare(load(counterPath!));
		const Doc = defineState("counter", {
			schema: chord.obj({ n: chord.int() }),
			version: 1,
			initial: () => ({ n: 0 }),
		});
		const facet = defineFacet((host: any) => {
			host.serve(Counter, (_params: unknown, _principal: unknown, self: any) => {
				const doc = self.state(Doc);
				return {
					variant: "owner",
					members: {
						count: self.derive([doc], ([d]: [{ n: number }]) => d.n),
						add: async ({ by }: { by: number }, ctx: unknown) => {
							await doc.change((draft: { n: number }) => void (draft.n += by), ctx);
							return null;
						},
						// Many commits at once, for a consumer that acknowledges slower than they come.
						burst: async ({ times }: { times: number }, ctx: unknown) => {
							for (let i = 0; i < times; i++) await doc.change((draft: { n: number }) => void (draft.n += 1), ctx);
							return null;
						},
					},
				};
			});
		});
		const host = createHost({ address: Server(), principal: { kind: "server" }, onError: (error: unknown) => console.error("server:", error) });
		const loaded = await host.load([{ name: "@noel/interop", facet }], context);
		for (const failure of loaded?.failed ?? []) console.error("plugin failed:", failure.error);
		if ((loaded?.failed ?? []).length > 0) process.exit(1);
		// Bun's server sockets, as the WebSocketLike upstream's link takes.
		const sockets = new Map<unknown, { listeners: Record<string, ((event?: unknown) => void)[]>; state: number }>();
		Bun.serve({
			unix: socket!,
			fetch(request, server) {
				return server.upgrade(request) ? undefined : new Response("no", { status: 400 });
			},
			websocket: {
				open(ws) {
					const entry = { listeners: {} as Record<string, ((event?: unknown) => void)[]>, state: 1 };
					sockets.set(ws, entry);
					const like = {
						get readyState() {
							return entry.state;
						},
						binaryType: "arraybuffer",
						send: (data: Uint8Array) => ws.sendBinary(data),
						close: () => ws.close(),
						addEventListener: (type: string, listener: (event?: unknown) => void) =>
							(entry.listeners[type] ??= []).push(listener),
						removeEventListener: (type: string, listener: (event?: unknown) => void) => {
							entry.listeners[type] = (entry.listeners[type] ?? []).filter((each) => each !== listener);
						},
					};
					host.accept(webSocketLink(like as never), { principal: { kind: "owner" } }, context);
					say("accepted");
				},
				message(ws, message) {
					const entry = sockets.get(ws)!;
					const data =
						typeof message === "string"
							? message
							: (message.buffer as ArrayBuffer).slice(message.byteOffset, message.byteOffset + message.byteLength);
					for (const listener of entry.listeners.message ?? []) listener({ data });
				},
				close(ws) {
					const entry = sockets.get(ws)!;
					entry.state = 3;
					for (const listener of entry.listeners.close ?? []) listener();
					sockets.delete(ws);
				},
			},
		});
		say("listening");
		return;
	}
	throw new Error(`unknown mode ${mode}`);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});

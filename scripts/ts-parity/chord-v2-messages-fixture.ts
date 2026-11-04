// Chord v2 messages in upstream's binary codec, for noel-chord's v2 codec to match byte for byte.
//
// Usage: bun --conditions=source scripts/ts-parity/chord-v2-messages-fixture.ts <pi-reference-root> <output-dir>
// (as for chord-v2-contracts-fixture.ts). Writes <output-dir>/chord-v2-messages.json: each message as JSON (parts
// as {text} or {deflated: hex}, byte segments as hex) and its bytes in hex.
import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const hexOf = (bytes: Uint8Array): string => Buffer.from(bytes).toString("hex");

async function main() {
	const args = process.argv.slice(2);
	if (args.length === 1) {
		console.error("chord-v2-messages-fixture.ts: skipped here; run make chord2-fixtures");
		return;
	}
	const [reference, outputDir] = args;
	const { binaryCodec } = await import(join(reference, "packages/chord/src/link/binary.ts"));
	const { checkMessage } = await import(join(reference, "packages/chord/src/link/message.ts"));
	const { DeflatedPart } = await import(join(reference, "packages/chord/src/link/part.ts"));
	const { chordErrorFromJson } = await import(join(reference, "packages/chord/src/data/internal.ts"));
	const { BACKGROUND_CONTEXT } = await import(join(reference, "packages/chord/src/context/index.ts"));
	const commit = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();

	const incarnation = "0190f5a1-2b3c-7d4e-8f00-112233445566";
	const other = "0190f5a1-2b3c-7d4e-8f00-ffeeddccbbaa";
	const deflated = new DeflatedPart(Uint8Array.from([0x4b, 0x4c, 0x4a, 0x06, 0x00]));
	const long = "x".repeat(4096);
	// JSON fields with their keys sorted: noel writes JSON with sorted keys, and receivers parse it.
	const plugin = { code: "busy", kind: "plugin", message: "a run is active" };
	const chordError = { error: { code: "unavailable", message: "worker starting", reason: "starting" }, kind: "chord" };
	const messages: Record<string, unknown>[] = [
		{ kind: "hello", protocol: 1, incarnation, calls: 256 },
		{
			kind: "hold",
			hold: 0,
			type: "noel.chat",
			version: 1,
			fingerprint: "2a6091124251ff09",
			params: '{"session":"s1"}',
			address: { kind: "session", params: { id: "s1" } },
		},
		{
			kind: "hold",
			origin: 3,
			hold: 200,
			type: "noel.sessions",
			version: 2,
			fingerprint: "0f0f53cdbca05450",
			params: "{}",
			address: { kind: "server", params: {} },
			principal: { kind: "owner", via: "device" },
			incarnation,
			chain: [incarnation, other],
			limit: 67108864,
		},
		{ kind: "release", hold: 7 },
		{ kind: "release", origin: 1, hold: 7 },
		{ kind: "call", call: 1, hold: 0, method: 3, args: '{"text":"héllo €"}' },
		{ kind: "call", origin: 2, call: 128, hold: 0, method: 0 },
		{
			kind: "call",
			call: 2,
			hold: 0,
			method: 1,
			args: '{"data":{"$bytes":0}}',
			segments: [Uint8Array.from([1, 2, 3]), Uint8Array.from([])],
		},
		{ kind: "abort", call: 16384 },
		{ kind: "retry", hold: 1, hops: 16 },
		{
			kind: "bound",
			holds: [
				{ hold: 0, variant: 1, members: [{ member: 0, value: '{"entries":[]}' }], failed: [] },
				{ hold: 1, variant: 0, members: [], failed: [{ member: 2, error: plugin }] },
			],
		},
		{
			kind: "state",
			origin: 9,
			holds: [
				{ hold: 0, members: [{ member: 0, ops: '[["a",["entries",0],"x"]]' }], failed: [], retired: [] },
				{ hold: 1, members: [{ member: 1, ops: deflated }], failed: [{ member: 0, error: chordError }], retired: [0, 2] },
			],
		},
		{ kind: "ack" },
		{ kind: "ack", origin: 4 },
		{ kind: "closed", origin: 5 },
		{ kind: "revoke", revocation: { device: "d1" } },
		{ kind: "ping" },
		{ kind: "pong" },
		{ kind: "failed", hold: 3, cause: { kind: "transient", error: chordError }, retryIn: 0 },
		{ kind: "failed", origin: 1, hold: 3, cause: { kind: "waiting", error: plugin } },
		{ kind: "failed", hold: 3, cause: { kind: "broken", error: plugin } },
		{ kind: "loading", hold: 3 },
		{ kind: "result", call: 1, value: '{"ok":true}' },
		{ kind: "result", origin: 7, call: 1 },
		{ kind: "result", call: 1, value: long },
		{ kind: "error", call: 1, error: plugin },
		{ kind: "gone", hold: 4, error: chordError },
	];

	const toJson = (message: Record<string, unknown>): unknown =>
		JSON.parse(
			JSON.stringify(message, (_key, value) => {
				if (value instanceof DeflatedPart) return { deflated: hexOf(value.bytes) };
				if (value instanceof Uint8Array) return { bytes: hexOf(value) };
				return value;
			}),
		);
	const cases = messages.map((message) => {
		const bytes: Uint8Array = binaryCodec.encode(message);
		// Upstream decodes and checks what it encoded: the case is a valid message.
		const decoded = binaryCodec.decode(bytes);
		checkMessage(decoded, BACKGROUND_CONTEXT);
		return { message: toJson(message), hex: hexOf(bytes) };
	});

	// The message check on what the binary codec decodes: messages it encodes, valid or not, with the verdict.
	const hold = messages[1]!;
	const checked: Record<string, unknown>[] = [
		{ kind: "hello", protocol: 1, incarnation, calls: 1 },
		{ kind: "hello", protocol: 1, incarnation, calls: 0 },
		{ kind: "hello", protocol: 9, incarnation, calls: 1 },
		{ kind: "ack", origin: 0 },
		{ kind: "ack", origin: 1 },
		{ kind: "closed", origin: 0 },
		{ kind: "retry", hold: 0, hops: 0 },
		{ kind: "retry", hold: 0, hops: 1 },
		{ kind: "retry", hold: 0, hops: 17 },
		{ ...hold, version: 0 },
		{ ...hold, principal: { via: "device" } },
		{ ...hold, principal: { kind: 1 } },
		{ ...hold, principal: { kind: "owner" } },
		{ ...hold, chain: Array.from({ length: 32 }, () => incarnation) },
		{ ...hold, chain: Array.from({ length: 33 }, () => incarnation) },
		{ kind: "error", call: 0, error: { code: "c", kind: "plugin", message: "m" } },
		{ kind: "error", call: 0, error: { code: "c", details: [1], kind: "plugin", message: "m" } },
		{ kind: "error", call: 0, error: { code: "c", kind: "plugin" } },
		{ kind: "error", call: 0, error: { code: "c", kind: "plugin", message: "m", stack: "s" } },
		{ kind: "error", call: 0, error: { error: 1, kind: "chord" } },
		{ kind: "error", call: 0, error: { error: {}, kind: "chord", x: 1 } },
		{ kind: "error", call: 0, error: { kind: "other" } },
		{ kind: "gone", hold: 0, error: { kind: "chord" } },
		{ kind: "failed", hold: 0, cause: { kind: "broken", error: { kind: "plugin", code: 1, message: "m" } } },
		{ kind: "bound", holds: [{ hold: 0, variant: 0, members: [], failed: [{ member: 0, error: { kind: "x" } }] }] },
		{
			kind: "state",
			holds: [{ hold: 0, members: [], failed: [{ member: 0, error: { kind: "x" } }], retired: [] }],
		},
	];
	const checks = checked.map((message) => {
		const decoded = binaryCodec.decode(binaryCodec.encode(message));
		let ok = true;
		try {
			checkMessage(decoded, BACKGROUND_CONTEXT);
		} catch {
			ok = false;
		}
		return { message: toJson(message), ok };
	});

	// ChordError envelopes as a consumer rebuilds them: the error's JSON again, or why it is refused.
	const envelopes = [
		{ code: "gone", message: "m" },
		{ code: "invalid", reason: "args", message: "m", details: { at: ["x"] } },
		{ code: "too_large", reason: "part", message: "m" },
		{ code: "released", reason: "settled", message: "m" },
		{ code: "invalid", message: "m" },
		{ code: "gone", reason: "x", message: "m" },
		{ code: "busy", message: "m" },
		{ code: "gone" },
		{ code: "gone", message: "m", stack: "s" },
		[],
		"gone",
	].map((input) => {
		try {
			return { input, json: chordErrorFromJson(input).toJSON() };
		} catch (error) {
			return { input, error: (error as Error).message };
		}
	});

	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "chord-v2-messages.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: commit,
					package: "packages/chord",
					packageCommit: commit,
					script: `/ts-parity/${fileURLToPath(import.meta.url).split("/").pop()}`,
				},
				messages: cases,
				checks,
				envelopes,
			},
			null,
			2,
		)}\n`,
	);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});

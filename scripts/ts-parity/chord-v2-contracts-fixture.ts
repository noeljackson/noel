// Chord v2 contract fingerprints, for noel-chord's v2 contracts to match byte for byte.
//
// Usage: bun --conditions=source scripts/ts-parity/chord-v2-contracts-fixture.ts \
//          <pi-reference-root> <noel-contracts-v2-dir> <output-dir>
// The reference root is earendil-works/pi at CHORD2_REFERENCE_REF, with node_modules/@earendil-works/pi-schema
// and pi-telemetry linked to its packages/schema and packages/telemetry. Writes <output-dir>/chord-v2-contracts.json:
// hash64 of some strings, and the fingerprint of each contract as Chord declares it from the same JSON.
import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** A contract as noel writes it: its params in order and, per variant, its state and methods as JSON Schema. */
interface Declaration {
	name: string;
	version: number;
	host: "server" | "session";
	params: [string, Json][];
	variants: Record<string, { state?: Record<string, Json>; methods?: Record<string, { args: Json; result: Json }> }>;
}

async function main() {
	const args = process.argv.slice(2);
	// The v1 image runs every fixture script with only an output directory:
	// this one needs Chord v2's pin, so it is made by `make chord2-fixtures`.
	if (args.length === 1) {
		console.error("chord-v2-contracts-fixture.ts: skipped here; run make chord2-fixtures");
		return;
	}
	const [reference, contractsDir, outputDir] = args;
	if (!reference || !contractsDir || !outputDir) {
		throw new Error("usage: chord-v2-contracts-fixture.ts <pi-reference-root> <noel-contracts-v2-dir> <output-dir>");
	}
	const chord = await import(join(reference, "packages/chord/src/contracts/index.ts"));
	const { hash64 } = await import(join(reference, "packages/chord/src/contracts/fingerprint.ts"));
	const commit = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
	const Server = chord.hostKind("server");
	const Session = chord.hostKind("session", { id: chord.str() });

	const declare = (declaration: Declaration) =>
		chord.remote(declaration.name, {
			version: declaration.version,
			params: Object.fromEntries(declaration.params),
			hostAddress:
				declaration.host === "server"
					? () => Server()
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

	const noel: Declaration[] = readdirSync(contractsDir)
		.filter((file) => file.endsWith(".json"))
		.sort()
		.map((file) => JSON.parse(readFileSync(join(contractsDir, file), "utf8")));

	const json = (schema: unknown): Json => JSON.parse(JSON.stringify(schema));
	// Edge cases of the normal form: key order and annotations, sorted `required` and `enum`, data keywords,
	// $defs and $ref, anyOf, tuple items, records, non-ASCII text, and a second variant with other state.
	const object = (properties: Record<string, Json>, extra: Record<string, Json> = {}): Json => ({
		type: "object",
		properties,
		additionalProperties: false,
		...extra,
	});
	const cases: Declaration[] = [
		// Upstream's own pinned case (test/contracts/fingerprint.test.ts), built with its builders.
		{
			name: "notes",
			version: 1,
			host: "server",
			params: [["session", json(chord.str())]],
			variants: {
				editor: {
					state: { notes: json(chord.obj({ text: chord.str() })) },
					methods: { add: { args: json(chord.obj({ text: chord.str() })), result: json(chord.str()) } },
				},
				reader: { state: { notes: json(chord.obj({ text: chord.str() })) } },
			},
		},
		{
			name: "edge",
			version: 3,
			host: "session",
			params: [
				["session", { type: "string", minLength: 1 }],
				["page", { type: "integer", minimum: 1 }],
			],
			variants: {
				a: {
					state: {
						s: {
							title: "T",
							required: ["b", "a"],
							properties: {
								b: { type: "integer", description: "d", minimum: 0, default: 3 },
								a: { $comment: "c", type: "string", examples: ["x"], pattern: "^é+$" },
								list: { type: "array", items: [{ type: "string" }, { type: "number" }], minItems: 1 },
								either: { anyOf: [{ type: "null" }, { $ref: "#/$defs/thing" }] },
								map: { type: "object", patternProperties: { "^x": { type: "boolean" } } },
							},
							$defs: { thing: { type: "object", properties: { z: { const: { b: 1, a: [2, 1.5] } } } } },
							type: "object",
							deprecated: true,
						},
					},
					methods: {
						go: {
							args: object(
								{ when: { enum: [3, "x", null, true] }, nested: object({ deep: { type: "string" } }) },
								{ required: ["when"] },
							),
							result: { type: "null" },
						},
					},
				},
			},
		},
	];

	const contracts = [...noel, ...cases].map((declaration) => ({
		declaration,
		fingerprint: declare(declaration).fingerprint as string,
	}));
	const notes = contracts.find(({ declaration }) => declaration.name === "notes")!;
	if (notes.fingerprint !== "b3eda86070a7e319") throw new Error(`notes: ${notes.fingerprint}, not upstream's golden`);
	const texts = ["", "a", "noel", "é€😀", JSON.stringify({ normalFormVersion: 2, value: null })];

	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "chord-v2-contracts.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: commit,
					package: "packages/chord",
					packageCommit: commit,
					script: `/ts-parity/${fileURLToPath(import.meta.url).split("/").pop()}`,
				},
				hash64: texts.map((text) => [text, hash64(text)]),
				contracts,
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

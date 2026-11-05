// Upstream codemode's declaration rendering, recorded from its own tests, for noel-codemode's
// declarations module to match byte for byte.
//
// Usage: bun scripts/ts-parity/codemode-declarations-fixture.ts <pi-reference-root> <output-dir> (make codemode-fixtures)
// Runs packages/codemode/test/declarations.test.ts with `vitest` and `../src/index.ts` replaced by
// recorders, and writes <output-dir>/codemode-declarations.json: every call of schemaToType,
// renderToolSignature, renderToolSample and renderDeclarations with its arguments and result,
// plus cases of noel's own.
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Call = { fn: string; args: unknown[]; result: string };

const NOEL_CASES: Array<[string, unknown[]]> = [
	["schemaToType", [{ type: "object", properties: { "\u{1F600}": { type: "string" }, "": { type: "number" } } }]],
	["schemaToType", [{ type: "object", properties: { n: { type: "number", minimum: 0.5, maximum: 1e3 } } }]],
	["schemaToType", [{ type: "object", properties: { s: { type: "string", default: "a b", description: " x   y " } } }]],
	["schemaToType", [{ $ref: "#", properties: { a: { $ref: "#/properties/b" }, b: { type: "string" } } }]],
	["schemaToType", [{ $ref: "#/$defs/a~1b", $defs: { "a/b": { type: "boolean" } } }]],
	["schemaToType", [{ items: { type: "string" } }]],
	["schemaToType", [{ prefixItems: [{ type: "string" }, { type: "number" }] }]],
	["schemaToType", [{ type: "array" }]],
	["schemaToType", [{ required: ["a"] }]],
	["schemaToType", [{ oneOf: [{ type: "string" }, { type: "string" }] }]],
	["schemaToType", [{ allOf: [{}, true] }]],
	["schemaToType", [{ type: "object", properties: { a: { type: "string" } }, additionalProperties: true }]],
	["schemaToType", [{ type: "object", properties: { a: { type: "integer", multipleOf: 2, uniqueItems: true, deprecated: true } } }]],
	["schemaToType", [{ type: "object", properties: { a: { oneOf: [{ type: "string", minLength: 2 }, { type: "null" }], minLength: 1 } } }]],
	["schemaToType", [{ type: "object", properties: { a: { type: "object", description: "line one\nline two", properties: { b: { type: "string", description: "B" } } } } }]],
	["renderDeclarations", [{ tools: [{ name: "multi", description: "First.\n\nSecond.\r\nThird." }] }]],
	["renderDeclarations", [{ globals: [{ name: "a.b.c", description: "Nested." }, { name: "solo" }, { name: "a.d" }] }]],
	["renderDeclarations", [{}]],
	["renderToolSignature", [{ name: "big", inputSchema: { type: "object", properties: { a: { type: "string", description: "x".repeat(40) } } } }, { inputMaxChars: 30 }]],
	["renderToolSample", [{ name: "bare" }]],
];

async function main() {
	const args = process.argv.slice(2);
	if (args.length === 1) {
		console.error("codemode-declarations-fixture.ts: skipped here; run make codemode-fixtures");
		return;
	}
	const [reference, outputDir] = args;
	if (!reference || !outputDir) {
		throw new Error("usage: codemode-declarations-fixture.ts <pi-reference-root> <output-dir>");
	}
	const source = join(reference, "packages/codemode/src/declarations.ts");
	const upstream = await import(source);
	const commit = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();

	const calls: Call[] = [];
	const record =
		(fn: string) =>
		(...args: unknown[]) => {
			const result = upstream[fn](...args);
			calls.push({ fn, args: JSON.parse(JSON.stringify(args)), result });
			return result;
		};
	const recorders = Object.fromEntries(
		["schemaToType", "renderToolSignature", "renderToolSample", "renderDeclarations"].map((fn) => [fn, record(fn)]),
	);
	(globalThis as Record<string, unknown>).__codemodeRecorders = recorders;

	const tests: Array<{ name: string; run: () => unknown }> = [];
	const prefix: string[] = [];
	const failures: string[] = [];
	const expect = (actual: unknown) => ({
		toBe(expected: unknown) {
			if (actual !== expected) failures.push(`${JSON.stringify(actual)} !== ${JSON.stringify(expected)}`);
		},
		toContain(expected: string) {
			if (typeof actual !== "string" || !actual.includes(expected)) failures.push(`missing ${expected}`);
		},
	});
	(globalThis as Record<string, unknown>).__codemodeVitest = {
		describe(name: string, body: () => void) {
			prefix.push(name);
			body();
			prefix.pop();
		},
		it(name: string, run: () => unknown) {
			tests.push({ name: [...prefix, name].join(" > "), run });
		},
		expect,
	};

	const work = await mkdtemp(join(tmpdir(), "codemode-declarations-"));
	try {
		await writeFile(
			join(work, "vitest.ts"),
			"const v = (globalThis as any).__codemodeVitest;\nexport const describe = v.describe, it = v.it, expect = v.expect;\n",
		);
		await writeFile(
			join(work, "index.ts"),
			"const r = (globalThis as any).__codemodeRecorders;\nexport const schemaToType = r.schemaToType, renderToolSignature = r.renderToolSignature, renderToolSample = r.renderToolSample, renderDeclarations = r.renderDeclarations;\n",
		);
		const test = await readFile(join(reference, "packages/codemode/test/declarations.test.ts"), "utf8");
		const rewritten = test
			.replace(/from "vitest"/, `from ${JSON.stringify(join(work, "vitest.ts"))}`)
			.replace(/from "\.\.\/src\/index\.ts"/, `from ${JSON.stringify(join(work, "index.ts"))}`);
		await writeFile(join(work, "declarations.test.ts"), rewritten);
		await import(join(work, "declarations.test.ts"));
		for (const test of tests) await test.run();
	} finally {
		await rm(work, { recursive: true, force: true });
	}
	if (failures.length > 0) throw new Error(`upstream tests failed:\n${failures.join("\n")}`);
	for (const [fn, args] of NOEL_CASES) recorders[fn](...args);

	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "codemode-declarations.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: process.env.TS_REFERENCE_REF ?? "main",
					commit,
					script: "/ts-parity/codemode-declarations-fixture.ts",
					files: ["packages/codemode/src/declarations.ts", "packages/codemode/test/declarations.test.ts"],
				},
				defaultInputSchemaMaxChars: upstream.DEFAULT_INPUT_SCHEMA_MAX_CHARS,
				calls,
			},
			null,
			2,
		)}\n`,
	);
	console.log(`codemode-declarations: ${tests.length} tests, ${calls.length} calls`);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});

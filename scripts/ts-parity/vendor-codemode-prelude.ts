// Writes upstream codemode's prelude, as the VM evaluates it, for noel-codemode to vendor.
//
// Usage: bun scripts/ts-parity/vendor-codemode-prelude.ts <pi-reference-root> <output-dir>
// Writes <output-dir>/prelude.js (PRELUDE_SOURCE with its constants filled in) and
// <output-dir>/prelude.json (the limits it embeds and its source commit).
import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

async function main() {
	const [reference, outputDir] = process.argv.slice(2);
	if (!reference || !outputDir) {
		throw new Error("usage: vendor-codemode-prelude.ts <pi-reference-root> <output-dir>");
	}
	const module = await import(join(reference, "packages/codemode/src/runtime/prelude-source.ts"));
	const commit = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "prelude.js"), module.PRELUDE_SOURCE);
	await writeFile(
		join(outputDir, "prelude.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					commit,
					file: "packages/codemode/src/runtime/prelude-source.ts",
				},
				maxStoreValueChars: module.MAX_STORE_VALUE_CHARS,
				maxStoreTotalChars: module.MAX_STORE_TOTAL_CHARS,
				maxOutputChars: module.MAX_OUTPUT_CHARS,
				maxOutputItems: module.MAX_OUTPUT_ITEMS,
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

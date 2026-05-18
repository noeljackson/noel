import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BUILTIN_SLASH_COMMANDS } from "/ts-reference/packages/coding-agent/src/core/slash-commands.ts";

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: slash-command-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "slash-commands.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				commands: [...BUILTIN_SLASH_COMMANDS].sort((left, right) => left.name.localeCompare(right.name)),
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

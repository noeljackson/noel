import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BUILTIN_SLASH_COMMANDS } from "/ts-reference/packages/coding-agent/src/core/slash-commands.ts";

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: tui-transcript-fixture.ts <output-dir>");
	}
	const fixture = {
		source: {
			repository: "https://github.com/earendil-works/pi",
			ref: "main",
			script: fileURLToPath(import.meta.url),
		},
		transcripts: [
			{
				name: "help",
				requiredMarkers: [...BUILTIN_SLASH_COMMANDS].map((command) => `/${command.name}`).sort(),
			},
			{
				name: "session",
				requiredMarkers: ["session:", "cwd:", "name:", "labels:", "parent:", "file:"],
			},
			{
				name: "status",
				requiredMarkers: ["status", "model:", "thinking:", "theme:", "queue:", "history:", "diagnostics:"],
			},
			{
				name: "selector",
				requiredMarkers: ["selector", "filter", "enter select", "esc cancel"],
			},
			{
				name: "reload",
				requiredMarkers: ["Reloaded keybindings, extensions, skills, prompts, themes"],
			},
		],
	};
	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "tui-transcripts.json"), `${JSON.stringify(fixture, null, 2)}\n`);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

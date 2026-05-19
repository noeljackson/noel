import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	allToolNames,
	createToolDefinition,
	type ToolName,
} from "/ts-reference/packages/coding-agent/src/core/tools/index.ts";

function schemaKeys(schema: unknown): { properties: string[]; required: string[] } {
	const typed = schema as { properties?: Record<string, unknown>; required?: string[] };
	return {
		properties: Object.keys(typed.properties ?? {}).sort(),
		required: [...(typed.required ?? [])].sort(),
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: local-tools-fixture.ts <output-dir>");
	}
	const toolNames = [...allToolNames];
	const tools = toolNames.map((name) => {
		const definition = createToolDefinition(name as ToolName, "/repo");
		return {
			name: definition.name,
			label: definition.label,
			promptSnippet: definition.promptSnippet,
			parameters: schemaKeys(definition.parameters),
		};
	});
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "local-tools.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				tools,
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

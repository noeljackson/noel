import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { getModels, getProviders, getSupportedThinkingLevels } from "/ts-reference/packages/ai/src/models.ts";

const targets = [
	["anthropic", "claude-opus-4-7"],
	["anthropic", "claude-sonnet-4-5"],
	["openai", "gpt-5.4"],
	["openai-codex", "gpt-5.5"],
	["github-copilot", "gpt-5.4"],
	["amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"],
	["openrouter", "moonshotai/kimi-k2.6"],
	["google", "gemini-2.5-pro"],
	["mistral", "devstral-medium-latest"],
];

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: model-catalog-fixture.ts <output-dir>");
	}
	const providers = getProviders();
	const models = targets.map(([provider, id]) => {
		const model = getModels(provider as any).find((candidate) => candidate.id === id);
		return {
			provider,
			id,
			found: Boolean(model),
			api: model?.api ?? null,
			name: model?.name ?? null,
			reasoning: Boolean(model?.reasoning),
			thinkingLevels: model ? getSupportedThinkingLevels(model as any) : [],
			thinkingLevelMap: model?.thinkingLevelMap ?? null,
			context: model?.context ?? null,
		};
	});
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "model-catalog.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				providerCount: providers.length,
				targets: models,
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

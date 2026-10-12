import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { clampThinkingLevel, getSupportedThinkingLevels } from "/ts-reference/packages/ai/src/models.ts";
import { DEFAULT_THINKING_LEVEL, THINKING_LEVEL_OPTIONS } from "/ts-reference/packages/coding-agent/src/core/defaults.ts";
import { builtinModelRuntime, fixtureSource } from "./model-runtime.ts";

/**
 * What upstream's `getSupportedThinkingLevels` and `clampThinkingLevel` answer
 * for every shape of model in the built-in catalog. Models with the same
 * `reasoning` flag and the same `thinkingLevelMap` behave alike, so the
 * fixture has one case per such shape, in catalog order, with the first
 * models of that shape as examples.
 */
const EXAMPLES = 2;

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: model-thinking-fixture.ts <output-dir>");
	}
	const runtime = await builtinModelRuntime();
	const cases = new Map<string, any>();
	for (const model of runtime.getModels()) {
		const thinkingLevelMap = model.thinkingLevelMap ?? null;
		const key = JSON.stringify([model.reasoning, thinkingLevelMap]);
		let entry = cases.get(key);
		if (!entry) {
			entry = {
				reasoning: model.reasoning,
				thinkingLevelMap,
				examples: [],
				supported: getSupportedThinkingLevels(model),
				clamp: {
					...Object.fromEntries(THINKING_LEVEL_OPTIONS.map((level) => [level, clampThinkingLevel(model, level)])),
					// A name that is not a thinking level.
					invalid: clampThinkingLevel(model, "invalid" as any),
				},
			};
			cases.set(key, entry);
		}
		if (entry.examples.length < EXAMPLES) entry.examples.push(`${model.provider}/${model.id}`);
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "model-thinking.json"),
		`${JSON.stringify(
			{
				source: fixtureSource(fileURLToPath(import.meta.url)),
				levels: THINKING_LEVEL_OPTIONS,
				defaultLevel: DEFAULT_THINKING_LEVEL,
				cases: [...cases.values()],
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

import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { defaultModelPerProvider } from "/ts-reference/packages/coding-agent/src/core/model-resolver.ts";
import { builtinModelRuntime, fixtureSource } from "./model-runtime.ts";

/**
 * Upstream's whole built-in chat model catalog, as its coding agent sees it:
 * every provider in upstream's order, every model with every field, and the
 * model each provider starts on (`defaultModelPerProvider`, in upstream's
 * order, which is the order startup tries them in).
 *
 * pi compiles this file in as its own built-in catalog
 * (`crates/pi-ai/src/catalog.rs`), so regenerating it updates the models pi
 * knows. One model per line keeps a regenerated catalog reviewable.
 */
async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: model-catalog-fixture.ts <output-dir>");
	}
	const runtime = await builtinModelRuntime();
	const providers = runtime.getProviders().map((provider) => ({
		id: provider.id,
		name: provider.name,
		models: [...runtime.getModels(provider.id)],
	}));
	const defaultModels = Object.entries(defaultModelPerProvider).map(([provider, id]) => ({ provider, id }));

	const lines = ["{", `  "source": ${JSON.stringify(fixtureSource(fileURLToPath(import.meta.url)))},`];
	lines.push('  "defaultModels": [');
	lines.push(defaultModels.map((entry) => `    ${JSON.stringify(entry)}`).join(",\n"));
	lines.push("  ],");
	lines.push('  "providers": [');
	lines.push(
		providers
			.map((provider) => {
				const head = `    {\n      "id": ${JSON.stringify(provider.id)},\n      "name": ${JSON.stringify(provider.name)},\n      "models": [`;
				if (provider.models.length === 0) return `${head}]\n    }`;
				const models = provider.models.map((model) => `        ${JSON.stringify(model)}`).join(",\n");
				return `${head}\n${models}\n      ]\n    }`;
			})
			.join(",\n"),
	);
	lines.push("  ]", "}");

	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "model-catalog.json"), `${lines.join("\n")}\n`);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	SettingsManager,
	type Settings,
	type SettingsScope,
	type SettingsStorage,
} from "/ts-reference/packages/coding-agent/src/core/settings-manager.ts";

class FixtureStorage implements SettingsStorage {
	private values: Record<SettingsScope, string | undefined>;

	constructor(globalSettings?: Settings, projectSettings?: Settings) {
		this.values = {
			global: globalSettings === undefined ? undefined : JSON.stringify(globalSettings),
			project: projectSettings === undefined ? undefined : JSON.stringify(projectSettings),
		};
	}

	withLock(scope: SettingsScope, fn: (current: string | undefined) => string | undefined): void {
		const next = fn(this.values[scope]);
		if (next !== undefined) {
			this.values[scope] = next;
		}
	}
}

const cases: Array<{
	name: string;
	globalSettings?: Settings & Record<string, unknown>;
	projectSettings?: Settings & Record<string, unknown>;
}> = [
	{
		name: "deep-merge-project-overrides",
		globalSettings: {
			defaultProvider: "anthropic",
			defaultModel: "claude-global",
			defaultThinkingLevel: "high",
			transport: "auto",
			steeringMode: "all",
			followUpMode: "all",
			enabledModels: ["anthropic/*", "openai/*"],
			compaction: { enabled: true, reserveTokens: 100 },
			retry: { enabled: true, maxRetries: 4, provider: { timeoutMs: 1000 } },
			terminal: { showImages: true, imageWidthCells: 80 },
			images: { autoResize: true },
			thinkingBudgets: { minimal: 1000, high: 4000 },
		},
		projectSettings: {
			defaultModel: "claude-project",
			followUpMode: "one-at-a-time",
			enabledModels: ["anthropic/claude-*"],
			compaction: { keepRecentTokens: 200 },
			retry: { provider: { maxRetries: 2 } },
			terminal: { imageWidthCells: 40, clearOnShrink: true },
			images: { blockImages: true },
			thinkingBudgets: { medium: 2500 },
		},
	},
	{
		name: "legacy-migrations",
		globalSettings: {
			queueMode: "all",
			websockets: false,
			retry: { maxDelayMs: 12345 },
		},
	},
	{
		name: "empty-defaults",
	},
];

function summarize(manager: SettingsManager) {
	return {
		defaultProvider: manager.getDefaultProvider() ?? null,
		defaultModel: manager.getDefaultModel() ?? null,
		defaultThinkingLevel: manager.getDefaultThinkingLevel() ?? null,
		transport: manager.getTransport(),
		steeringMode: manager.getSteeringMode(),
		followUpMode: manager.getFollowUpMode(),
		enabledModels: manager.getEnabledModels() ?? null,
		compaction: manager.getCompactionSettings(),
		retry: manager.getRetrySettings(),
		providerRetry: manager.getProviderRetrySettings(),
		terminal: {
			showImages: manager.getShowImages(),
			imageWidthCells: manager.getImageWidthCells(),
			clearOnShrink: manager.getClearOnShrink(),
			showTerminalProgress: manager.getShowTerminalProgress(),
		},
		images: {
			autoResize: manager.getImageAutoResize(),
			blockImages: manager.getBlockImages(),
		},
		thinkingBudgets: manager.getThinkingBudgets() ?? null,
		doubleEscapeAction: manager.getDoubleEscapeAction(),
		treeFilterMode: manager.getTreeFilterMode(),
		quietStartup: manager.getQuietStartup(),
		hideThinkingBlock: manager.getHideThinkingBlock(),
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: settings-fixture.ts <output-dir>");
	}
	delete process.env.PI_CLEAR_ON_SHRINK;
	const fixture = {
		source: {
			repository: "https://github.com/earendil-works/pi",
			ref: "main",
			script: fileURLToPath(import.meta.url),
		},
		cases: cases.map((testCase) => {
			const manager = SettingsManager.fromStorage(
				new FixtureStorage(testCase.globalSettings, testCase.projectSettings),
			);
			return {
				name: testCase.name,
				globalSettings: testCase.globalSettings ?? {},
				projectSettings: testCase.projectSettings ?? {},
				expected: summarize(manager),
			};
		}),
	};
	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "settings.json"), `${JSON.stringify(fixture, null, 2)}\n`);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

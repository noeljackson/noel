import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	AuthStorage,
	type AuthCredential,
} from "/ts-reference/packages/coding-agent/src/core/auth-storage.ts";

type ProviderEnvCase = {
	provider: string;
	env: Record<string, string>;
	statusLabel?: string;
	expectedApiKey?: string;
	expectedSource?: string;
};

const envCases: ProviderEnvCase[] = [
	{
		provider: "anthropic",
		env: {
			ANTHROPIC_OAUTH_TOKEN: "claude-oauth-env-token",
			ANTHROPIC_API_KEY: "anthropic-api-key",
		},
		statusLabel: "ANTHROPIC_OAUTH_TOKEN",
		expectedApiKey: "claude-oauth-env-token",
		expectedSource: "environment",
	},
	{
		provider: "openai",
		env: { OPENAI_API_KEY: "openai-api-key" },
		statusLabel: "OPENAI_API_KEY",
		expectedApiKey: "openai-api-key",
		expectedSource: "environment",
	},
	{
		provider: "azure-openai-responses",
		env: { AZURE_OPENAI_API_KEY: "azure-api-key" },
		statusLabel: "AZURE_OPENAI_API_KEY",
		expectedApiKey: "azure-api-key",
		expectedSource: "environment",
	},
];

function withEnv<T>(values: Record<string, string>, fn: () => T): T {
	const saved = new Map<string, string | undefined>();
	for (const key of Object.keys(values)) {
		saved.set(key, process.env[key]);
		process.env[key] = values[key];
	}
	try {
		return fn();
	} finally {
		for (const [key, value] of saved.entries()) {
			if (value === undefined) {
				delete process.env[key];
			} else {
				process.env[key] = value;
			}
		}
	}
}

async function summarizeEnvCase(testCase: ProviderEnvCase) {
	const storage = AuthStorage.inMemory();
	return withEnv(testCase.env, async () => {
		return {
			...testCase,
			status: storage.getAuthStatus(testCase.provider),
			apiKey: await storage.getApiKey(testCase.provider, { includeFallback: false }),
		};
	});
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: auth-discovery-fixture.ts <output-dir>");
	}

	const storedCredentials: Record<string, AuthCredential> = {
		anthropic: {
			type: "api_key",
			key: "stored-anthropic-api-key",
		},
		"openai-codex": {
			type: "oauth",
			access: "stored-codex-access-token",
			refresh: "stored-codex-refresh-token",
			expires: 4102444800000,
			accountId: "account-id",
		},
	};
	const stored = AuthStorage.inMemory(storedCredentials);
	stored.setRuntimeApiKey("anthropic", "runtime-anthropic-api-key");
	stored.setFallbackResolver((provider) =>
		provider === "custom-provider" ? "fallback-custom-provider-key" : undefined,
	);

	const fixture = {
		source: {
			repository: "https://github.com/earendil-works/pi",
			ref: "main",
			script: fileURLToPath(import.meta.url),
		},
		fakeTokensOnly: true,
		authJson: {
			pathSuffix: ".pi/agent/auth.json",
			credentialTypes: ["api_key", "oauth"],
			apiKeyCredential: storedCredentials.anthropic,
			oauthCredential: storedCredentials["openai-codex"],
			providers: stored.list(),
			status: {
				anthropic: stored.getAuthStatus("anthropic"),
				openaiCodex: stored.getAuthStatus("openai-codex"),
			},
		},
		precedence: {
			order: [
				"runtime",
				"stored_api_key",
				"stored_oauth",
				"environment",
				"fallback",
			],
			runtimeOverride: await stored.getApiKey("anthropic", { includeFallback: false }),
			fallback: await stored.getApiKey("custom-provider"),
		},
		env: await Promise.all(envCases.map(summarizeEnvCase)),
		interopLoginFiles: {
			claudeCode: {
				pathSuffix: ".claude/.credentials.json",
				tokenPointer: "/claudeAiOauth/accessToken",
				sample: {
					claudeAiOauth: {
						accessToken: "claude-access",
						refreshToken: "redacted",
						expiresAt: 1,
					},
				},
			},
			codex: {
				pathSuffix: ".codex/auth.json",
				tokenPointer: "/tokens/access_token",
				accountPointer: "/tokens/account_id",
				sample: {
					auth_mode: "chatgpt",
					tokens: {
						access_token: "codex-access",
						refresh_token: "redacted",
						account_id: "account-id",
					},
				},
			},
		},
		redaction: {
			persistLiveCredentials: false,
			fixturesUseOnlyFakeTokens: true,
		},
	};

	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "auth-discovery.json"), `${JSON.stringify(fixture, null, 2)}\n`);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

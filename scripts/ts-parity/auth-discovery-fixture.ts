import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	AuthStorage,
	type AuthCredential,
} from "/ts-reference/packages/coding-agent/src/core/auth-storage.ts";
import { RuntimeCredentials } from "/ts-reference/packages/coding-agent/src/core/runtime-credentials.ts";
import { defaultProviderAuthContext } from "/ts-reference/packages/ai/src/auth/context.ts";
import { resolveProviderAuth } from "/ts-reference/packages/ai/src/auth/resolve.ts";
import { anthropicProvider } from "/ts-reference/packages/ai/src/providers/anthropic.ts";
import { azureOpenAIResponsesProvider } from "/ts-reference/packages/ai/src/providers/azure-openai-responses.ts";
import { openaiProvider } from "/ts-reference/packages/ai/src/providers/openai.ts";

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
	const provider = {
		anthropic: anthropicProvider(),
		openai: openaiProvider(),
		"azure-openai-responses": azureOpenAIResponsesProvider(),
	}[testCase.provider];
	if (!provider) {
		throw new Error(`unsupported auth fixture provider: ${testCase.provider}`);
	}
	const result = await resolveProviderAuth(provider, AuthStorage.inMemory(), defaultProviderAuthContext(), {
		env: testCase.env,
	});
	return {
		...testCase,
		status: {
			configured: Boolean(result),
			source: "environment",
			label: result?.source,
		},
		apiKey: result?.auth.apiKey,
	};
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
	const runtime = new RuntimeCredentials(stored);
	runtime.setRuntimeApiKey("anthropic", "runtime-anthropic-api-key");

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
			providers: await stored.list(),
			status: {
				anthropic: await stored.read("anthropic"),
				openaiCodex: await stored.read("openai-codex"),
			},
		},
		precedence: {
			order: [
				"runtime",
				"stored",
				"environment",
			],
			runtimeOverride: await runtime.read("anthropic"),
			stored: await stored.read("anthropic"),
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

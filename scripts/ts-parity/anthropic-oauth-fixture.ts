import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

type CapturedRequest = {
	url: string;
	method: string;
	headers: Record<string, string>;
	body: unknown;
};

function normalizeHeaders(headers: HeadersInit | undefined): Record<string, string> {
	const normalized: Record<string, string> = {};
	if (!headers) {
		return normalized;
	}
	new Headers(headers).forEach((value, key) => {
		normalized[key.toLowerCase()] = value;
	});
	return normalized;
}

async function requestBody(input: RequestInfo | URL, init: RequestInit | undefined): Promise<string> {
	if (typeof init?.body === "string") {
		return init.body;
	}
	if (input instanceof Request) {
		return input.clone().text();
	}
	return "";
}

async function captureAnthropicRequest(apiKey: string): Promise<CapturedRequest> {
	let captured: CapturedRequest | undefined;
	const originalFetch = globalThis.fetch;
	globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
		const inputHeaders = input instanceof Request ? normalizeHeaders(input.headers) : {};
		const initHeaders = normalizeHeaders(init?.headers);
		const rawBody = await requestBody(input, init);
		captured = {
			url: input instanceof Request ? input.url : input.toString(),
			method: init?.method ?? (input instanceof Request ? input.method : "GET"),
			headers: sanitizeHeaders({ ...inputHeaders, ...initHeaders }),
			body: rawBody ? JSON.parse(rawBody) : null,
		};
		throw new Error("TS_PARITY_CAPTURED_REQUEST");
	};

	try {
		process.env.ANTHROPIC_API_KEY = apiKey;
		const [{ getModel }, { streamSimple }] = await Promise.all([
			import("/ts-reference/packages/ai/src/models.ts"),
			import("/ts-reference/packages/ai/src/stream.ts"),
		]);
		const model = getModel("anthropic", "claude-sonnet-4-6");
		const stream = streamSimple(
			model,
			{
				systemPrompt: "pi rust cli",
				messages: [
					{
						role: "user",
						content: "hello",
						timestamp: 0,
					},
				],
			},
			{
				apiKey,
				reasoning: undefined,
				maxTokens: 4096,
			},
		);
		await stream.result();
	} finally {
		globalThis.fetch = originalFetch;
	}

	if (!captured) {
		throw new Error("TS provider did not issue an Anthropic request");
	}
	return captured;
}

function sanitizeHeaders(headers: Record<string, string>): Record<string, string> {
	const sanitized = { ...headers };
	if (sanitized.authorization) {
		sanitized.authorization = "Bearer <redacted>";
	}
	if (sanitized["x-api-key"]) {
		sanitized["x-api-key"] = "<redacted>";
	}
	return sanitized;
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: anthropic-oauth-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFixture(outputDir, "anthropic-claude-code-oauth.json", {
		source: {
			branch: "ts-reference",
			script: fileURLToPath(import.meta.url),
		},
		provider: "anthropic",
		auth: "claude-code-oauth",
		request: await captureAnthropicRequest("sk-ant-oat-ts-parity-token"),
	});
	await writeFixture(outputDir, "anthropic-api-key.json", {
		source: {
			branch: "ts-reference",
			script: fileURLToPath(import.meta.url),
		},
		provider: "anthropic",
		auth: "api-key",
		request: await captureAnthropicRequest("sk-ant-api03-ts-parity-token"),
	});
}

async function writeFixture(outputDir: string, name: string, fixture: unknown) {
	await writeFile(join(outputDir, name), `${JSON.stringify(fixture, null, 2)}\n`);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

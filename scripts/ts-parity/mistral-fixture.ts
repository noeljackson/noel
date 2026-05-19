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

async function captureMistralRequest(): Promise<CapturedRequest> {
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
		const [{ getModel }, { streamSimple }] = await Promise.all([
			import("/ts-reference/packages/ai/src/models.ts"),
			import("/ts-reference/packages/ai/src/stream.ts"),
		]);
		const model = getModel("mistral", "devstral-medium-latest");
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
				apiKey: "mistral_ts_parity_token",
				reasoning: "high",
				sessionId: "session_ts_parity",
			},
		);
		await stream.result();
	} finally {
		globalThis.fetch = originalFetch;
	}

	if (!captured) {
		throw new Error("TS provider did not issue a Mistral request");
	}
	return captured;
}

function sanitizeHeaders(headers: Record<string, string>): Record<string, string> {
	const sanitized = { ...headers };
	if (sanitized.authorization) {
		sanitized.authorization = "Bearer <redacted>";
	}
	return sanitized;
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: mistral-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "mistral-devstral.json"),
		`${JSON.stringify(
			{
				source: {
					branch: "ts-reference",
					script: fileURLToPath(import.meta.url),
				},
				provider: "mistral",
				auth: "api-key",
				request: await captureMistralRequest(),
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

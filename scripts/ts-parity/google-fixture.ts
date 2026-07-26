import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "/ts-reference/packages/ai/src/index.ts";
import { getModel, streamSimple } from "/ts-reference/packages/ai/src/compat.ts";
import { convertMessages } from "/ts-reference/packages/ai/src/api/google-shared.ts";

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

async function captureGoogleRequest(withTools = false): Promise<CapturedRequest> {
	let captured: CapturedRequest | undefined;
	const originalFetch = globalThis.fetch;
	globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
		const inputHeaders = input instanceof Request ? normalizeHeaders(input.headers) : {};
		const initHeaders = normalizeHeaders(init?.headers);
		const rawBody = await requestBody(input, init);
		const rawUrl = input instanceof Request ? input.url : input.toString();
		captured = {
			url: sanitizeUrl(rawUrl),
			method: init?.method ?? (input instanceof Request ? input.method : "GET"),
			headers: sanitizeHeaders({ ...inputHeaders, ...initHeaders }),
			body: rawBody ? JSON.parse(rawBody) : null,
		};
		throw new Error("TS_PARITY_CAPTURED_REQUEST");
	};

	try {
		const model = getModel("google", "gemini-2.5-pro");
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
				tools: withTools
					? [
							{
								name: "fixture_echo",
								description: "Echo text for the parity fixture.",
								parameters: Type.Object({
									text: Type.String(),
								}),
							},
						]
					: undefined,
			},
			{
				apiKey: "google_ts_parity_token",
				reasoning: "high",
			},
		);
		await stream.result();
	} finally {
		globalThis.fetch = originalFetch;
	}

	if (!captured) {
		throw new Error("TS provider did not issue a Google request");
	}
	return captured;
}

function sanitizeUrl(url: string): string {
	return url.replace(/key=[^&]+/g, "key=<redacted>");
}

function sanitizeHeaders(headers: Record<string, string>): Record<string, string> {
	const sanitized = { ...headers };
	if (sanitized["x-goog-api-key"]) {
		sanitized["x-goog-api-key"] = "<redacted>";
	}
	return sanitized;
}

function captureGoogleToolImageRouting() {
	const context = {
		messages: [
			{
				role: "toolResult",
				toolCallId: "call_img",
				toolName: "read",
				content: [
					{ type: "text", text: "alpha text" },
					{ type: "image", data: "YWJj", mimeType: "image/png" },
				],
				isError: false,
				timestamp: 0,
			},
		],
	};
	const gemini2 = getModel("google", "gemini-2.5-flash");
	const gemini3 = { ...getModel("google", "gemini-2.5-flash"), id: "gemini-3-pro-preview" };
	return {
		gemini2: convertMessages(gemini2, context as never),
		gemini3: convertMessages(gemini3, context as never),
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: google-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "google-gemini-2.5-pro.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				provider: "google",
				auth: "api-key",
				request: await captureGoogleRequest(),
			},
			null,
			2,
		)}\n`,
	);
	await writeFile(
		join(outputDir, "google-tools.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				provider: "google",
				auth: "api-key",
				request: await captureGoogleRequest(true),
			},
			null,
			2,
		)}\n`,
	);
	await writeFile(
		join(outputDir, "google-tool-image-routing.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				provider: "google",
				routing: captureGoogleToolImageRouting(),
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

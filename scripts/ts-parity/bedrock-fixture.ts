import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "/ts-reference/packages/ai/src/index.ts";
import { getModel, streamSimple } from "/ts-reference/packages/ai/src/compat.ts";

async function captureBedrockPayload(withTools = false): Promise<unknown> {
	let captured: unknown;
	const model = getModel("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1");
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
			reasoning: "xhigh",
			onPayload: (payload) => {
				captured = payload;
				throw new Error("TS_PARITY_CAPTURED_PAYLOAD");
			},
		},
	);
	await stream.result().catch(() => undefined);
	if (!captured) {
		throw new Error("TS provider did not build a Bedrock payload");
	}
	return captured;
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: bedrock-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "bedrock-claude-opus-4.6.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				provider: "amazon-bedrock",
				auth: "payload-only",
				payload: await captureBedrockPayload(),
			},
			null,
			2,
		)}\n`,
	);
	await writeFile(
		join(outputDir, "bedrock-tools.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				provider: "amazon-bedrock",
				auth: "payload-only",
				payload: await captureBedrockPayload(true),
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

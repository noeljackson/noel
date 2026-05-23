import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { runAgentLoop } from "/ts-reference/packages/agent/src/agent-loop.ts";
import type {
	AgentEvent,
	AgentMessage,
	AgentTool,
	StreamFn,
} from "/ts-reference/packages/agent/src/types.ts";
import { getModel } from "/ts-reference/packages/ai/src/models.ts";
import { Type } from "/ts-reference/packages/ai/src/index.ts";
import { createAssistantMessageEventStream } from "/ts-reference/packages/ai/src/utils/event-stream.ts";

type StreamCall = {
	roles: string[];
	toolNames: string[];
	lastRole: string | null;
};

function textOf(message: AgentMessage): string {
	if (!("content" in message)) return "";
	if (typeof message.content === "string") return message.content;
	return message.content
		.filter((block): block is { type: "text"; text: string } => block.type === "text")
		.map((block) => block.text)
		.join("");
}

async function captureAgentToolLoop() {
	const model = getModel("openai", "gpt-5.4");
	const tool: AgentTool = {
		name: "fixture_echo",
		label: "Fixture echo",
		description: "Echo text for the parity fixture.",
		parameters: Type.Object({
			text: Type.String(),
		}),
		executionMode: "sequential",
		execute: async (_toolCallId, params) => ({
			content: [{ type: "text", text: `echo:${params.text}` }],
			details: { echoed: params.text },
		}),
	};

	const streamCalls: StreamCall[] = [];
	const streamFn: StreamFn = (streamModel, context) => {
		streamCalls.push({
			roles: context.messages.map((message) => message.role),
			toolNames: context.tools?.map((candidate) => candidate.name) ?? [],
			lastRole: context.messages.at(-1)?.role ?? null,
		});

		const stream = createAssistantMessageEventStream();
		const timestamp = streamCalls.length;
		if (streamCalls.length === 1) {
			const message = {
				role: "assistant" as const,
				content: [
					{
						type: "toolCall" as const,
						id: "call_fixture_1",
						name: "fixture_echo",
						arguments: { text: "hello" },
					},
				],
				api: streamModel.api,
				provider: streamModel.provider,
				model: streamModel.id,
				usage: {
					input: 1,
					output: 1,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 2,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
				stopReason: "toolUse" as const,
				timestamp,
			};
			stream.push({ type: "done", reason: "toolUse", message });
			return stream;
		}

		const message = {
			role: "assistant" as const,
			content: [{ type: "text" as const, text: "final" }],
			api: streamModel.api,
			provider: streamModel.provider,
			model: streamModel.id,
			usage: {
				input: 1,
				output: 1,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 2,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop" as const,
			timestamp,
		};
		stream.push({ type: "done", reason: "stop", message });
		return stream;
	};

	const events: string[] = [];
	const messages = await runAgentLoop(
		[{ role: "user", content: "use the fixture tool", timestamp: 0 }],
		{
			systemPrompt: "pi rust cli",
			messages: [],
			tools: [tool],
		},
		{
			model,
			convertToLlm: (messages) => messages as any,
			toolExecution: "sequential",
		},
		async (event: AgentEvent) => {
			events.push(event.type);
		},
		undefined,
		streamFn,
	);

	return {
		streamCalls,
		events,
		messages: messages.map((message) => ({
			role: message.role,
			text: textOf(message),
			toolCallNames:
				"content" in message && Array.isArray(message.content)
					? message.content.filter((block) => block.type === "toolCall").map((block) => block.name)
					: [],
			toolResultFor: message.role === "toolResult" ? message.toolName : null,
		})),
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: agent-tool-loop-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "agent-tool-loop.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				toolLoop: await captureAgentToolLoop(),
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

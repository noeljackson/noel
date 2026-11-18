// Goldens for noel-durable (docs/durable-sdk-2026-11.md, slice 1): the
// scenarios of upstream Durable's examples 13 (chat), 17 (print), 19
// (inbox) and 33 (turn limit and continue), recorded from upstream's own
// Harness at TS_REFERENCE_REF, on the faux provider, and written as JSON
// a Rust test replays against noel-durable.
//
// Durable is read from /ts-reference like every other fixture; the
// scenarios are the examples' own, run through the public API the SDK
// models (Harness.open, Conversation.submit, Submission.wait).
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";





// The reference checkout: DURABLE_REFERENCE_DIR (the Makefile's default
// is the local pi checkout), /ts-reference in the fixture image.
const REFERENCE = process.env.DURABLE_REFERENCE_DIR ?? "/home/noel/src/noeljackson/pi";

const { BACKGROUND_CONTEXT } = await import(`${REFERENCE}/node_modules/@earendil-works/chord/src/context/index.ts`);
const { createModels } = await import(`${REFERENCE}/packages/ai/src/models`);
const { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall } = await import(
	`${REFERENCE}/packages/ai/src/providers/faux`
);
const {
	ConversationBusy,
	createRegistry,
	createSession,
	defineExtension,
	defineTask,
	defineTool,
	Harness,
	InboxDoc,
	LiveDoc,
	MemoryStorage,
} = await import(`${REFERENCE}/packages/durable/src/index.ts`);
const { openNodeSqliteStorage } = await import(`${REFERENCE}/packages/durable/src/storage/sqlite/node.ts`);

const git = (...args: string[]): string =>
	execFileSync("git", ["-C", REFERENCE, ...args], { encoding: "utf8" }).trim();

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** 13-chat: one prompt answered; the transcript it leaves. */
async function chat(): Promise<Json> {
	const faux = fauxProvider();
	faux.setResponses([fauxAssistantMessage([fauxText("Paris.")])]);
	const models = createModels();
	models.setProvider(faux.provider);
	const harness = await Harness.open(new MemoryStorage(), { models, registry: createRegistry() }, BACKGROUND_CONTEXT);
	const root = await harness.root(BACKGROUND_CONTEXT, {
		agent: { model: { provider: "faux", modelId: "faux-1" } },
	});
	const submission = await root.submit({ type: "input", content: "Capital of France?" }, BACKGROUND_CONTEXT);
	const settled = await submission.wait(BACKGROUND_CONTEXT);
	const transcript = await root.entries({}, 10, undefined, BACKGROUND_CONTEXT);
	const kinds = [...transcript.items].reverse().map((entry) => entry.kind);
	const answer = answerOf(transcript);
	await harness.close(BACKGROUND_CONTEXT);
	return { status: settled.status, answer, transcript: kinds };
}

/** The text of the transcript's last assistant entry, as 13-chat prints it. */
function answerOf(transcript: { items: { model?: unknown[] }[] }): string {
	for (const entry of [...transcript.items].reverse()) {
		const message = entry.model?.[0] as
			| { role?: string; content?: { type: string; text?: string }[] }
			| undefined;
		if (message?.role === "assistant" && Array.isArray(message.content)) {
			return message.content
				.filter((part): part is { type: "text"; text: string } => part.type === "text")
				.map((part) => part.text)
				.join("");
		}
	}
	return "";
}

/** 17-print, faux variant: a tool call round, then the answer. */
async function printMode(): Promise<Json> {
	const step = defineTool({
		name: "step",
		description: "Do one step of the work",
		parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] } as never,
		execute: async (args: { n: number }) => ({
			output: [{ type: "text", text: `step ${args.n} done` }],
		}),
	});
	const faux = fauxProvider();
	faux.setResponses([
		fauxAssistantMessage([fauxToolCall("step", { n: 1 }, { id: "call-1" })], { stopReason: "toolUse" }),
		fauxAssistantMessage([fauxText("This directory holds the durable package sources, tests, and docs.")]),
	]);
	const models = createModels();
	models.setProvider(faux.provider);
	const registry = createRegistry();
	registry.install(defineExtension({ name: "steps", tools: [step] }));
	const harness = await Harness.open(new MemoryStorage(), { models, registry }, BACKGROUND_CONTEXT);
	const root = await harness.root(BACKGROUND_CONTEXT, {
		agent: { model: { provider: "faux", modelId: "faux-1" } },
	});
	const submission = await root.submit({ type: "input", content: "What is here?" }, BACKGROUND_CONTEXT);
	const settled = await submission.wait(BACKGROUND_CONTEXT);
	const transcript = await root.entries({ order: "ascending" }, 100, undefined, BACKGROUND_CONTEXT);
	const roles = transcript.items
		.map((entry) => entry.model?.[0]?.role)
		.filter((role) => role !== undefined);
	const answer = answerOf(transcript);
	await harness.close(BACKGROUND_CONTEXT);
	return { status: settled.status, answer, turns: roles };
}

/** 19-inbox: what happens to submissions while busy. */
async function inbox(): Promise<Json> {
	let release!: () => void;
	const held = new Promise<void>((resolve) => {
		release = resolve;
	});
	const slow = async () => {
		await held;
		return fauxAssistantMessage("Answer to the first question.");
	};
	const faux = fauxProvider();
	faux.setResponses([slow, fauxAssistantMessage("Answer to the follow-up.")]);
	const models = createModels();
	models.setProvider(faux.provider);
	const harness = await Harness.open(
		new MemoryStorage(),
		{ models, registry: createRegistry(), settings: { followUpMode: "all" } },
		BACKGROUND_CONTEXT,
	);
	const root = await harness.root(BACKGROUND_CONTEXT, {
		agent: { model: { provider: "faux", modelId: "faux-1" } },
	});
	const first = await root.submit({ type: "input", content: "First question" }, BACKGROUND_CONTEXT);
	const followUp = await root.submit({ type: "input", content: "A follow-up" }, BACKGROUND_CONTEXT);
	const steer = await root.submit({ type: "input", content: "A steer", whenBusy: "steer" }, BACKGROUND_CONTEXT);
	const withdrawn = await root.submit({ type: "input", content: "Never mind" }, BACKGROUND_CONTEXT);
	let rejected: string | null = null;
	await root
		.submit({ type: "input", content: "Now or never", whenBusy: "reject" }, BACKGROUND_CONTEXT)
		.catch((error: Error) => {
			rejected = error instanceof ConversationBusy ? "ConversationBusy" : error.message;
		});
	const withdraw = await withdrawn.abort(BACKGROUND_CONTEXT);
	const inbox = await harness.snapshot(InboxDoc, root.id, BACKGROUND_CONTEXT);
	const queued = (inbox?.items ?? []).map((item) => item.mode);
	release();
	const record = async (submission: Awaited<ReturnType<typeof root.submit>>) => {
		const settled = await submission.wait(BACKGROUND_CONTEXT);
		return {
			status: settled.status,
			reason: settled.status === "unanswered" ? settled.reason : null,
		};
	};
	const outcomes = {
		first: await record(first),
		followUp: await record(followUp),
		steer: await record(steer),
		withdrawn: await record(withdrawn),
	};
	await harness.close(BACKGROUND_CONTEXT);
	return { rejected, withdraw, queued, outcomes };
}

/** 33-turn-limit-and-continue: the limit stops a run; a continue resumes. */
async function turnLimit(): Promise<Json> {
	const call = (n: number) => fauxAssistantMessage(fauxToolCall("step", { n }), { stopReason: "toolUse" });
	const step = defineTool({
		name: "step",
		description: "Do one step of the work",
		parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] } as never,
		execute: async (args: { n: number }) => ({
			output: [{ type: "text", text: `step ${args.n} done` }],
		}),
	});
	const faux = fauxProvider();
	faux.setResponses([call(1), call(2), call(3), call(4), fauxAssistantMessage("All four steps are done.")]);
	const models = createModels();
	models.setProvider(faux.provider);
	const registry = createRegistry();
	registry.install(defineExtension({ name: "steps", tools: [step] }));
	const harness = await Harness.open(new MemoryStorage(), { models, registry }, BACKGROUND_CONTEXT);
	const root = await harness.root(BACKGROUND_CONTEXT, {
		agent: { model: { provider: "faux", modelId: "faux-1" }, maxTurns: 2 },
	});
	const first = await (await root.submit({ type: "input", content: "Do the four steps." }, BACKGROUND_CONTEXT)).wait(BACKGROUND_CONTEXT);
	const failure = (await harness.snapshot(LiveDoc, root.id, BACKGROUND_CONTEXT))?.runFailure;
	const resumed = await root.submit({ type: "continue" }, BACKGROUND_CONTEXT);
	// A second continue while that run works is refused, never queued.
	let busyWhileRunning: string | null = null;
	try {
		await root.submit({ type: "continue" }, BACKGROUND_CONTEXT);
	} catch (error) {
		busyWhileRunning = error instanceof ConversationBusy ? "ConversationBusy" : String(error);
	}
	const second = await resumed.wait(BACKGROUND_CONTEXT);
	const live = await harness.snapshot(LiveDoc, root.id, BACKGROUND_CONTEXT);
	const after = await (await root.submit({ type: "continue" }, BACKGROUND_CONTEXT)).wait(BACKGROUND_CONTEXT);
	await harness.close(BACKGROUND_CONTEXT);
	return {
		first: { status: first.status, reason: (first as { reason?: string }).reason ?? null },
		runFailure: failure ? { reason: failure.reason, detail: failure.detail ?? null } : null,
		second: { status: second.status, reason: (second as { reason?: string }).reason ?? null },
		runFailureCleared: live?.runFailure === undefined,
		afterAnswer: { status: after.status, reason: (after as { reason?: string }).reason ?? null },
		steps: 4,
		busyWhileRunning,
	};
}

/** 05/07 (harness and conversations): a harness of many conversations,
 * created, found again, listed; handles are stateless. */
async function harnessMany(): Promise<Json> {
	const faux = fauxProvider();
	faux.setResponses([fauxAssistantMessage([fauxText("one.")]), fauxAssistantMessage([fauxText("two.")])]);
	const models = createModels();
	models.setProvider(faux.provider);
	const registry = createRegistry();
	const harness = await Harness.open(new MemoryStorage(), { models, registry }, BACKGROUND_CONTEXT);
	const root = await harness.root(BACKGROUND_CONTEXT, {
		agent: { model: { provider: "faux", modelId: "faux-1" } },
	});
	const helper = await harness.createConversation(
		{ ownership: { kind: "ownerless" }, agent: { model: { provider: "faux", modelId: "faux-1" } } },
		BACKGROUND_CONTEXT,
	);
	// Each conversation answers its own prompt.
	const first = await (await root.submit({ type: "input", content: "to the root" }, BACKGROUND_CONTEXT)).wait(BACKGROUND_CONTEXT);
	const second = await (await helper.submit({ type: "input", content: "to the helper" }, BACKGROUND_CONTEXT)).wait(BACKGROUND_CONTEXT);
	// Found again by id: the same conversation.
	const found = await harness.conversation(helper.id, BACKGROUND_CONTEXT);
	await harness.close(BACKGROUND_CONTEXT);
	return {
		rootId: root.id,
		conversations: [root.id !== helper.id, found?.id === helper.id],
		settled: [first.status, second.status],
	};
}

/** 02 (forks): a fork continues from one entry of another conversation. */
async function forks(): Promise<Json> {
	const session = createSession(new MemoryStorage());
	const chat = await session.commit(
		(tx) => tx.createConversation({ ownership: { kind: "ownerless" } }),
		BACKGROUND_CONTEXT,
	);
	const first = await session.commit(
		(tx) => tx.appendEntry(chat.id, { kind: "note", data: "hello" }),
		BACKGROUND_CONTEXT,
	);
	await session.commit(
		(tx) => tx.appendEntry(chat.id, { kind: "note", data: "goodbye" }),
		BACKGROUND_CONTEXT,
	);
	const branch = await session.commit(
		(tx) => tx.forkConversation(chat.id, first.id, { ownership: { kind: "ownerless" } }),
		BACKGROUND_CONTEXT,
	);
	const branchEntries = await session.commit(
		(tx) => tx.scanEntries({ conversationId: branch.id }, 10),
		BACKGROUND_CONTEXT,
	);
	const parentEntries = await session.commit(
		(tx) => tx.scanEntries({ conversationId: chat.id }, 10),
		BACKGROUND_CONTEXT,
	);
	// The fork's own writes leave the parent unchanged.
	await session.commit((tx) => tx.appendEntry(branch.id, { kind: "note", data: "branch note" }), BACKGROUND_CONTEXT);
	const after = await session.commit(
		(tx) => tx.scanEntries({ conversationId: chat.id }, 10),
		BACKGROUND_CONTEXT,
	);
	return {
		forkTranscript: branchEntries.items.map((entry) => entry.data),
		parentTranscript: parentEntries.items.map((entry) => entry.data),
		independent: after.items.map((entry) => entry.data),
	};
}

/** 11-tasks: a task with phases runs to a terminal outcome. */
async function tasks(): Promise<Json> {
	const payments = new Map<string, number>();
	type PaymentState = { phase: "prepare" } | { phase: "charge"; key: string };
	const Payment = defineTask<{ amount: number }, PaymentState, { receipt: number }>({
		name: "example.payment",
		version: 1,
		initial: () => ({ phase: "prepare" }),
		phases: {
			prepare: async (task, runtime, ctx) => {
				await runtime.commit(
					() => ({ status: "running", checkpoint: { phase: "charge", key: `payment-${task.id}` } }),
					ctx,
				);
			},
			charge: async (task, runtime, ctx) => {
				const key = task.state.checkpoint.key;
				if (!payments.has(key)) payments.set(key, task.input.amount * 100);
				const receipt = payments.get(key)!;
				await runtime.commit(
					() => ({ status: "terminal", outcome: { status: "completed", result: { receipt } } }),
					ctx,
				);
			},
		},
		abort: async (_task, runtime, ctx) => {
			await runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), ctx);
		},
	});
	const registry = createRegistry();
	registry.install(defineExtension({ name: "payments", tasks: [Payment] }));
	const harness = await Harness.open(
		new MemoryStorage(),
		{ models: createModels(), registry },
		BACKGROUND_CONTEXT,
	);
	const root = await harness.root(BACKGROUND_CONTEXT);
	const paymentId = await root.commit(
		(tx) => tx.createTask(Payment, { amount: 5 }, { ownership: { kind: "conversation" } }),
		BACKGROUND_CONTEXT,
	);
	const paid = await harness.waitForTask(paymentId, BACKGROUND_CONTEXT);
	await harness.close(BACKGROUND_CONTEXT);
	return {
		payment: paid.state.outcome?.status ?? null,
		receipt: paid.state.outcome?.status === "completed" ? paid.state.outcome.result.receipt : null,
	};
}

/** 12-recovery: a task interrupted before its checkpoint is saved
 * resumes at its last checkpoint after a reopen; memos are exactly-once. */
async function taskRecovery(): Promise<Json> {
	const directory = await mkdtemp(join(tmpdir(), "pi-durable-fixture-"));
	const databasePath = join(directory, "session.sqlite");
	let reachedTick = (_n: number): void => {};
	const Ticker = defineTask<{ to: number }, { phase: "tick"; n: number }, string>({
		name: "example.ticker",
		version: 1,
		initial: () => ({ phase: "tick", n: 1 }),
		phases: {
			tick: async (task, runtime, ctx) => {
				const n = task.state.checkpoint.n;
				if ((await runtime.memo(`printed-${n}`, ctx)) === undefined) {
					await runtime.memo(`printed-${n}`, true, ctx);
				}
				reachedTick(n);
				if (n === task.input.to) {
					await runtime.commit(
						() => ({ status: "terminal", outcome: { status: "completed", result: `counted to ${n}` } }),
						ctx,
					);
					return;
				}
				await runtime.sleepUntil(runtime.now() + 20, { phase: "tick", n: n + 1 }, ctx);
			},
		},
		abort: async (_task, runtime, ctx) => {
			await runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), ctx);
		},
	});
	const registry = createRegistry();
	registry.install(defineExtension({ name: "ticker", tasks: [Ticker] }));
	const open = async () =>
		Harness.open(await openNodeSqliteStorage(databasePath), { models: createModels(), registry }, BACKGROUND_CONTEXT);

	const firstRun = await open();
	const tickerId = await (await firstRun.root(BACKGROUND_CONTEXT)).commit(
		(tx) => tx.createTask(Ticker, { to: 3 }, { ownership: { kind: "conversation" } }),
		BACKGROUND_CONTEXT,
	);
	const tickTwo = new Promise<void>((resolve) => {
		reachedTick = (n) => {
			if (n === 2) resolve();
		};
	});
	firstRun.resume();
	await tickTwo;
	await firstRun.close(BACKGROUND_CONTEXT);
	reachedTick = () => {};

	const reader = await open();
	const saved = (await reader.getTask(tickerId, BACKGROUND_CONTEXT))!;
	await reader.close(BACKGROUND_CONTEXT);

	const secondRun = await open();
	const counted = await secondRun.waitForTask(tickerId, BACKGROUND_CONTEXT);
	await secondRun.close(BACKGROUND_CONTEXT);
	await rm(directory, { recursive: true, force: true });
	return {
		closedState: saved.state.status,
		closedCheckpoint: saved.state.checkpoint,
		afterReopen: counted.state.outcome?.status ?? null,
		result: counted.state.outcome?.status === "completed" ? counted.state.outcome.result : null,
		restartsAfterReopen: counted.restarts ?? 0,
	};
}

const scenarios: Record<string, () => Promise<Json>> = {
	chat,
	print: printMode,
	inbox,
	turnLimit,
	harnessMany,
	forks,
	tasks,
	taskRecovery,
};

const outDir = process.argv[2];
if (outDir === undefined) throw new Error("usage: durable-fixture.ts <fixtures-dir>");

const source = {
	repository: "https://github.com/earendil-works/pi",
	ref: process.env.TS_REFERENCE_REF ?? "main",
	commit: git("rev-parse", "HEAD"),
	package: "packages/durable",
	script: "/ts-parity/durable-fixture.ts",
};

for (const [name, run] of Object.entries(scenarios)) {
	const outcome = await run();
	await writeFile(
		join(outDir, `durable-${name}.json`),
		`${JSON.stringify({ source, scenario: name, outcome }, null, "\t")}\n`,
	);
}

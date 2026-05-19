import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	CURRENT_SESSION_VERSION,
	SessionManager,
	type SessionEntry,
	type SessionHeader,
} from "/ts-reference/packages/coding-agent/src/core/session-manager.ts";

function summarizeEntry(entry: SessionEntry) {
	return {
		type: entry.type,
		parent: entry.parentId === null ? null : "set",
		role: entry.type === "message" ? entry.message.role : undefined,
	};
}

function linearizedBranchExport(session: SessionManager) {
	const header: SessionHeader = {
		type: "session",
		version: CURRENT_SESSION_VERSION,
		id: session.getSessionId(),
		timestamp: new Date().toISOString(),
		cwd: session.getCwd(),
	};
	const lines = [header];
	let previousId: string | null = null;
	for (const entry of session.getBranch()) {
		lines.push({ ...entry, parentId: previousId });
		previousId = entry.id;
	}
	return lines.map((entry) => JSON.parse(JSON.stringify(entry)));
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: session-export-fixture.ts <output-dir>");
	}

	const session = SessionManager.inMemory("/repo");
	const userId = session.appendMessage({
		role: "user",
		content: [{ type: "text", text: "hello <world>" }],
		timestamp: 1,
	});
	session.appendMessage({
		role: "assistant",
		content: [{ type: "text", text: "done" }],
		api: "anthropic-messages",
		provider: "anthropic",
		model: "claude",
		usage: {
			input: 1,
			output: 1,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 2,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop",
		timestamp: 2,
	});
	session.appendSessionInfo("demo session");
	session.appendModelChange("anthropic", "claude");
	session.appendThinkingLevelChange("xhigh");
	session.appendCustomEntry("fixture_state", { value: 1 });
	session.appendCompaction("summary", userId, 100);
	session.branch(userId);
	session.appendMessage({ role: "user", content: "branch", timestamp: 3 });
	session.appendLabelChange(userId, "important");

	const entries = session.getEntries();
	const branch = session.getBranch();
	const exported = linearizedBranchExport(session);
	const fixture = {
		source: {
			repository: "https://github.com/earendil-works/pi",
			ref: "main",
			script: fileURLToPath(import.meta.url),
		},
		header: {
			type: session.getHeader()?.type,
			version: session.getHeader()?.version,
			hasCwd: Boolean(session.getHeader()?.cwd),
		},
		entryTypes: entries.map((entry) => entry.type),
		treeRootCount: session.getTree().length,
		sessionName: session.getSessionName(),
		labelForFirstMessage: session.getLabel(userId),
		branch: branch.map(summarizeEntry),
		jsonlBranchExport: {
			recordTypes: exported.map((entry) => entry.type),
			parentChain: exported.slice(1).map((entry) => entry.parentId === null ? null : "set"),
			firstRecordVersion: exported[0]?.version,
			firstRecordHasCwd: Boolean(exported[0]?.cwd),
		},
	};

	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "session-export.json"), `${JSON.stringify(fixture, null, 2)}\n`);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});

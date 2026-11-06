// Upstream's tool search (tokenizer, search documents, BM25 ranker), for noel-codemode's search module.
//
// Usage: bun scripts/ts-parity/tool-search-fixture.ts <pi-reference-root> <output-dir> (make codemode-fixtures)
// Writes <output-dir>/tool-search.json: tokenize() of each text, and rank() of each query over
// documents built by createToolSearchDocument(), with the cases of upstream's tool-search.test.ts.
// `typebox` is stubbed: tool.ts uses it only for its input schema.
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

type Tool = { name: string; description: string; parameters?: unknown; namespace?: Record<string, string> };

const tool = (name: string, description: string, properties: Record<string, unknown> = {}): Tool => ({
	name,
	description,
	parameters: { type: "object", properties },
});

const TEXTS = [
	"listIssues for the GitHub_repo",
	"searches queries HTTPServer",
	"repo.issues/search:queries",
	"搜索仓库问题",
	"文書を検索",
	"문서 검색",
	"ค้นหาเอกสาร",
	"Поиск документов",
	"البحث عن الملفات",
	"फ़ाइल खोजें",
	"Éléments cafés",
	"搜索GitHub_repoIssues résumé cafés",
	"CAFÉ café",
	" \t，。!? 😀 ",
	"boxes buzzes classes ties pies glass is as",
	"XMLHttpRequest parseJSON v2Api ABCdef",
	"mcp__dev-app__thread_read",
];

const BASE = [
	tool("mcp__github__list_issues", "List issues in a repository.", { state: { type: "string", description: "open or closed" } }),
	tool("mcp__github__create_pull_request", "Open a pull request."),
	tool("mcp__linear__search_issues", "Search Linear issues by text."),
	tool("mcp__docs__search", "Search the documentation."),
];
const NAMED = [
	tool("mcp__app__queue_read", "Read a queued message of this thread."),
	tool(
		"mcp__dev-app__thread_read",
		"Read durable state and a paginated timeline from any thread. The default view returns user messages, assistant messages and plans. Continue with afterPosition until hasMore is false.",
	),
];
const CI = [
	tool("mcp__ci__status", "Deploy status: last deploy, deploy log, deploy errors."),
	tool(
		"mcp__ci__deploy_release",
		"Start a release of the current branch to an environment, run the migrations, switch traffic over, and wait until the new version is live and healthy.",
	),
];
const peekaboo = { name: "mcp__peekaboo", instructions: "Peekaboo controls macOS apps by app name." };
const PEEKABOO = [
	tool("mcp__peekaboo__see", "Capture the app window of the peekaboo app and describe the app."),
	tool("mcp__peekaboo__menu", "Click a menu item of an app."),
	tool(
		"mcp__peekaboo__app",
		"Launch, quit, relaunch, focus, hide, or switch applications. Targets an application by name, bundle identifier, or process ID, and can wait until it is ready.",
	),
	tool("mcp__app__queue_read", "Read a queued message of this thread."),
	tool(
		"mcp__dev-app__thread_read",
		"Read durable state and a paginated timeline from any thread. The default view returns user messages, assistant messages and plans.",
	),
].map((entry) => (entry.name.startsWith("mcp__peekaboo__") ? { ...entry, namespace: peekaboo } : entry));
const COVERAGE = [
	tool("mcp__peekaboo__app", "Launch, quit, focus, or switch applications by name or bundle identifier."),
	tool("mcp__peekaboo__see", "Capture the app window and describe the UI elements of the app."),
	tool("mcp__peekaboo__window", "Move, resize, or close a window of an app."),
	tool("mcp__peekaboo__image", "Take a screenshot of the screen, an app, or a window."),
];
const NESTED = [
	{
		name: "deep",
		description: "Nested schema.",
		parameters: {
			type: "object",
			properties: {
				filter: {
					type: "object",
					description: "Filter rows",
					properties: { owner: { type: "string" }, tags: { type: "array", items: { description: "label name" } } },
				},
				mode: { anyOf: [{ description: "fast path" }, { description: "slow path" }] },
			},
		},
	},
	{ name: "blank", description: "   ", parameters: { description: "" } },
];

const RANKS: Array<[Tool[], string, number]> = [
	[BASE, "issue", 8],
	[BASE, "pull requests", 8],
	[BASE, "search", 1],
	[BASE, "closed", 8],
	[BASE, "kubernetes", 8],
	[BASE, "the", 8],
	[BASE, "tickets", 8],
	[BASE, "", 8],
	[BASE, "search issues", 0],
	[[...BASE, tool("localized", "Поиск документов")], "документов", 8],
	[[...BASE, tool("localized", "البحث عن الملفات")], "الملفات", 8],
	[[...BASE, tool("localized", "फ़ाइल खोजें")], "खोजें", 8],
	[[...BASE, tool("localized", "Rechercher un café")], "café", 8],
	[[...BASE, tool("localized", "문서 검색")], "검색", 8],
	...["thread_read", "dev_app__thread_read", "mcp__dev-app__thread_read", "Thread_Read", "thread read", "read"].map(
		(query): [Tool[], string, number] => [NAMED, query, 1],
	),
	[NAMED, "read", 8],
	[CI, "deploy", 8],
	...["peekaboo app", "open the peekaboo app tool", "read thread_read state", "describe window", "app"].map(
		(query): [Tool[], string, number] => [PEEKABOO, query, 8],
	),
	...["screenshot window", "describe app", "window", "app window"].map((query): [Tool[], string, number] => [
		COVERAGE,
		query,
		8,
	]),
	[[{ ...tool("mcp__x__run", "Run it."), namespace: { name: "mcp__x", description: "Kubernetes cluster tools" } }], "kubernetes", 8],
	[NESTED, "label owner", 8],
	[NESTED, "slow rows", 8],
	[NESTED, "blank", 8],
];

async function main() {
	const args = process.argv.slice(2);
	if (args.length === 1) {
		console.error("tool-search-fixture.ts: skipped here; run make codemode-fixtures");
		return;
	}
	const [reference, outputDir] = args;
	if (!reference || !outputDir) throw new Error("usage: tool-search-fixture.ts <pi-reference-root> <output-dir>");
	const stub = new Proxy({}, { get: () => () => ({}) });
	Bun.plugin({
		name: "typebox-stub",
		setup(build) {
			build.module("typebox", () => ({ exports: { Type: stub }, loader: "object" }));
		},
	});
	const search = await import(join(reference, "packages/coding-agent/src/extensions/tool-search/tool.ts"));
	const ranker = new search.Bm25Ranker();
	const fixture = {
		source: {
			repository: "https://github.com/earendil-works/pi",
			ref: process.env.TS_REFERENCE_REF ?? "main",
			script: "/ts-parity/tool-search-fixture.ts",
			files: [
				"packages/coding-agent/src/extensions/tool-search/tool.ts",
				"packages/coding-agent/test/tool-search.test.ts",
			],
		},
		defaultLimit: search.DEFAULT_TOOL_SEARCH_LIMIT,
		tokenize: TEXTS.map((text) => ({ text, terms: search.tokenize(text) })),
		rank: RANKS.map(([tools, query, limit]) => {
			const documents = tools.map((entry) => search.createToolSearchDocument(entry, entry.namespace));
			return { tools, documents, query, limit, matches: ranker.rank(query, documents, limit) };
		}),
	};
	await mkdir(outputDir, { recursive: true });
	await writeFile(join(outputDir, "tool-search.json"), `${JSON.stringify(fixture, null, 2)}\n`);
	console.log(`tool-search: ${fixture.tokenize.length} texts, ${fixture.rank.length} rankings`);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});

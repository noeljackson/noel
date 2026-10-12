import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "/ts-reference/packages/ai/src/index.ts";
import { getDocsPath, getExamplesPath, getReadmePath } from "/ts-reference/packages/coding-agent/src/config.ts";
import { DefaultResourceLoader } from "/ts-reference/packages/coding-agent/src/core/resource-loader.ts";
import { createAgentSession } from "/ts-reference/packages/coding-agent/src/core/sdk.ts";
import { SessionManager } from "/ts-reference/packages/coding-agent/src/core/session-manager.ts";
import { SettingsManager } from "/ts-reference/packages/coding-agent/src/core/settings-manager.ts";
import {
	type BuildSystemPromptOptions,
	buildSystemPrompt,
	buildSystemPromptSections,
} from "/ts-reference/packages/coding-agent/src/core/system-prompt.ts";
import { allToolNames, createToolDefinition, type ToolName } from "/ts-reference/packages/coding-agent/src/core/tools/index.ts";

/**
 * Records upstream's default system prompt and the files it is built from.
 *
 * `prompts`: for fixed inputs, the text of upstream's `buildSystemPrompt` and
 * its sections. The inputs are the ones a real session passes its builder: each
 * case runs a real resource loader and agent session in a temporary tree, and
 * the fixture fails unless the session's prompt is the recorded text.
 *
 * `discovery`: what upstream's resource loader picks from a tree of
 * `SYSTEM.md`, `APPEND_SYSTEM.md`, and context files, with and without project
 * trust and command-line flags.
 *
 * Volatile values are placeholders; see PLACEHOLDERS.
 */

const PLACEHOLDERS = {
	"<tmp>": "The case's temporary directory. The agent dir is <tmp>/agent; the working directory is below it.",
	"<bom>": "A UTF-8 byte order mark (U+FEFF) at the start of a file to write.",
	"<product>": "The product name in the prompt's first sentence (upstream: pi).",
	"<readme-path>": "Upstream's installed README.md (docs section).",
	"<docs-path>": "Upstream's installed docs directory (docs section).",
	"<examples-path>": "Upstream's installed examples directory (docs section).",
};

/** The one place upstream's prompt names the product outside the docs section. */
const PRODUCT_SENTENCE = "operating inside pi, a coding agent harness";

type ExtensionTool = { name: string; description: string; promptSnippet?: string; promptGuidelines?: string[] };

type PromptCase = {
	name: string;
	description: string;
	/** `--tools`; omitted for upstream's default selection. */
	tools?: string[];
	noTools?: "all";
	/** Files to write, by path below the temporary directory. */
	files?: Record<string, string>;
	/** Working directory below the temporary directory. Default: repo. */
	cwd?: string;
	/** `--system-prompt`. */
	systemPrompt?: string;
	/** `--append-system-prompt`, once per entry. */
	appendSystemPrompt?: string[];
	extensionTools?: ExtensionTool[];
};

const PROJECT_RULES = "# Project\n\nRun `make check` before committing.\n";

const PROMPT_CASES: PromptCase[] = [
	{
		name: "default-tools",
		description: "Upstream's default tool selection: read, bash, edit, write.",
	},
	{
		name: "all-tools",
		description: "Every upstream tool pi has: the search tools replace the bash file-operations rule.",
		tools: ["read", "bash", "edit", "write", "grep", "find", "ls"],
	},
	{
		name: "tool-subset",
		description: "A read-only subset without bash.",
		tools: ["read", "grep", "find", "ls"],
	},
	{
		name: "no-tools",
		description: "No tools: the tool list is (none) and only the general rules remain.",
		noTools: "all",
	},
	{
		name: "context-files",
		description: "Context files from the agent dir and from the outermost ancestor down to the working directory.",
		files: {
			"agent/AGENTS.md": "Prefer small commits.\n",
			"repo/AGENTS.md": PROJECT_RULES,
			"repo/app/CLAUDE.md": "The app uses <strict> mode & \"quotes\".",
		},
		cwd: "repo/app",
	},
	{
		name: "custom-prompt",
		description: "A custom prompt replaces the role, tools, rules, and docs; project context and cwd stay.",
		systemPrompt: "You are a release bot.\nOnly discuss releases.",
		files: { "repo/AGENTS.md": PROJECT_RULES },
	},
	{
		name: "appended-prompt",
		description: "Appended text follows the default prompt and comes before project context.",
		appendSystemPrompt: ["Answer in French.", "Never push to main."],
		files: { "repo/AGENTS.md": PROJECT_RULES },
	},
	{
		name: "custom-and-appended-prompt",
		description: "A custom prompt with appended text.",
		systemPrompt: "You are a release bot.",
		appendSystemPrompt: ["Answer in French."],
	},
	{
		name: "extension-tool",
		description:
			"Extension tools: one with a snippet and guidelines (given untidy, as a manifest may), and one with guidelines only, which is not listed.",
		// In name order, the order pi declares extension tools in; upstream uses registration order.
		extensionTools: [
			{
				name: "deploy",
				description: "Deploy the app.",
				promptSnippet: "  Deploy the app\n to an   environment ",
				promptGuidelines: [
					" Ask before deploying to production. ",
					"",
					"Ask before deploying to production.",
					"Be concise in your responses",
				],
			},
			{
				name: "scan",
				description: "Scan dependencies.",
				promptGuidelines: ["Run scan after changing dependencies."],
			},
		],
	},
];

type DiscoveryCase = {
	name: string;
	description: string;
	/** Files to write, by path below the temporary directory. A content may use the `<tmp>` and `<bom>` placeholders. */
	files: Record<string, string>;
	/** Directories to create, by path below the temporary directory. */
	dirs?: string[];
	/** Working directory below the temporary directory. */
	cwd: string;
	trusted: boolean;
	/** Command-line flags; `<tmp>` in a value is the temporary directory. */
	flags?: { systemPrompt?: string; appendSystemPrompt?: string[]; noContextFiles?: boolean };
};

const BOTH_SCOPES = {
	"agent/SYSTEM.md": "agent system\n",
	"agent/APPEND_SYSTEM.md": "agent append\n",
	"repo/.pi/SYSTEM.md": "project system\n",
	"repo/.pi/APPEND_SYSTEM.md": "project append\n",
};

const DISCOVERY_CASES: DiscoveryCase[] = [
	{
		name: "agent-dir-files",
		description: "SYSTEM.md and APPEND_SYSTEM.md in the agent dir.",
		files: { "agent/SYSTEM.md": "agent system\n", "agent/APPEND_SYSTEM.md": "agent append\n" },
		cwd: "repo",
		trusted: true,
	},
	{
		name: "trusted-project-files-win",
		description: "A trusted project's .pi files replace the agent dir's; same-named files are not combined.",
		files: BOTH_SCOPES,
		cwd: "repo",
		trusted: true,
	},
	{
		name: "untrusted-project-files-ignored",
		description: "An untrusted project's .pi files are ignored.",
		files: BOTH_SCOPES,
		cwd: "repo",
		trusted: false,
	},
	{
		name: "each-file-resolves-alone",
		description: "A project SYSTEM.md does not hide the agent dir's APPEND_SYSTEM.md.",
		files: { "repo/.pi/SYSTEM.md": "project system\n", "agent/APPEND_SYSTEM.md": "agent append\n" },
		cwd: "repo",
		trusted: true,
	},
	{
		name: "project-dir-is-the-working-directory",
		description: "Only the working directory's .pi is read, not an ancestor's.",
		files: { "repo/.pi/SYSTEM.md": "project system\n", "repo/.pi/APPEND_SYSTEM.md": "project append\n" },
		dirs: ["repo/app"],
		cwd: "repo/app",
		trusted: true,
	},
	{
		name: "flags-replace-discovered-files",
		description:
			"--system-prompt and --append-system-prompt replace the discovered files; a flag naming a file reads it, without its byte order mark.",
		files: { ...BOTH_SCOPES, "extra.md": "<bom>extra from a file\n" },
		cwd: "repo",
		trusted: true,
		flags: { systemPrompt: "flag system", appendSystemPrompt: ["flag append", "<tmp>/extra.md"] },
	},
	{
		name: "one-flag-leaves-the-other-file",
		description: "--append-system-prompt alone leaves the discovered SYSTEM.md in place.",
		files: BOTH_SCOPES,
		cwd: "repo",
		trusted: true,
		flags: { appendSystemPrompt: ["flag append"] },
	},
	{
		name: "byte-order-mark-and-empty-files",
		description: "A byte order mark is dropped; an empty APPEND_SYSTEM.md appends nothing.",
		files: { "agent/SYSTEM.md": "<bom>agent system\n", "agent/APPEND_SYSTEM.md": "", "repo/AGENTS.md": "<bom>rules\n" },
		cwd: "repo",
		trusted: true,
	},
	{
		name: "context-file-names",
		description:
			"One context file per directory: AGENTS.override.md, then AGENTS.md, then CLAUDE.md. An override replaces only its own directory's file, and a directory with a context file's name is skipped.",
		files: {
			"agent/AGENTS.override.md": "agent override\n",
			"agent/AGENTS.md": "agent rules\n",
			"repo/AGENTS.md": "repo rules\n",
			"repo/CLAUDE.md": "repo claude\n",
			"repo/app/AGENTS.override.md": "app override\n",
			"repo/app/AGENTS.md": "app rules\n",
			"repo/app/src/CLAUDE.md": "src claude\n",
		},
		dirs: ["repo/app/src/AGENTS.md"],
		cwd: "repo/app/src",
		trusted: true,
	},
	{
		name: "working-directory-inside-the-agent-dir",
		description: "The agent dir's context file is listed once when the agent dir is also an ancestor.",
		files: { "agent/AGENTS.md": "agent rules\n", "agent/work/AGENTS.md": "work rules\n" },
		cwd: "agent/work",
		trusted: true,
	},
	{
		name: "untrusted-project-keeps-context-files",
		description: "Context files load whatever the trust decision.",
		files: { "agent/AGENTS.md": "agent rules\n", "repo/AGENTS.md": "repo rules\n", "repo/.pi/SYSTEM.md": "project system\n" },
		cwd: "repo",
		trusted: false,
	},
	{
		name: "no-context-files-flag",
		description: "--no-context-files drops context files and keeps SYSTEM.md.",
		files: { "agent/AGENTS.md": "agent rules\n", "repo/AGENTS.md": "repo rules\n", "agent/SYSTEM.md": "agent system\n" },
		cwd: "repo",
		trusted: true,
		flags: { noContextFiles: true },
	},
	{
		name: "nested-worktree-shadows-main-checkout",
		description:
			"A linked worktree inside its main checkout: the worktree's context file replaces the main checkout's same-named file. Other ancestors still load.",
		files: {
			"outer/AGENTS.md": "outer rules\n",
			"outer/repo/.git/HEAD": "ref: refs/heads/main\n",
			"outer/repo/.git/worktrees/wt/HEAD": "ref: refs/heads/wt\n",
			"outer/repo/.git/worktrees/wt/commondir": "../..\n",
			"outer/repo/AGENTS.md": "main checkout rules\n",
			"outer/repo/trees/AGENTS.md": "trees rules\n",
			"outer/repo/trees/wt/.git": "gitdir: <tmp>/outer/repo/.git/worktrees/wt\n",
			"outer/repo/trees/wt/AGENTS.md": "worktree rules\n",
		},
		dirs: ["outer/repo/trees/wt/src"],
		cwd: "outer/repo/trees/wt/src",
		trusted: true,
	},
	{
		name: "nested-worktree-with-another-file-name",
		description: "The worktree's CLAUDE.md shadows only the main checkout's CLAUDE.md, so its AGENTS.md still loads.",
		files: {
			"repo/.git/HEAD": "ref: refs/heads/main\n",
			"repo/.git/worktrees/wt/HEAD": "ref: refs/heads/wt\n",
			"repo/.git/worktrees/wt/commondir": "../..\n",
			"repo/AGENTS.md": "main checkout rules\n",
			"repo/wt/.git": "gitdir: <tmp>/repo/.git/worktrees/wt\n",
			"repo/wt/CLAUDE.md": "worktree claude\n",
		},
		cwd: "repo/wt",
		trusted: true,
	},
	{
		name: "sibling-worktree-shadows-nothing",
		description: "A linked worktree beside its main checkout shadows nothing.",
		files: {
			"work/AGENTS.md": "work rules\n",
			"work/repo/.git/HEAD": "ref: refs/heads/main\n",
			"work/repo/.git/worktrees/wt/HEAD": "ref: refs/heads/wt\n",
			"work/repo/.git/worktrees/wt/commondir": "../..\n",
			"work/repo/AGENTS.md": "main checkout rules\n",
			"work/wt/.git": "gitdir: <tmp>/work/repo/.git/worktrees/wt\n",
			"work/wt/AGENTS.md": "worktree rules\n",
		},
		cwd: "work/wt",
		trusted: true,
	},
];

/** How upstream's agent session tidies a tool's snippet (it is private there; the session check below proves this copy). */
function normalizeSnippet(text: string | undefined): string | undefined {
	const oneLine = (text ?? "").replace(/\s+/g, " ").trim();
	return oneLine.length > 0 ? oneLine : undefined;
}

/** How upstream's agent session tidies a tool's guidelines (see normalizeSnippet). */
function normalizeGuidelines(guidelines: readonly string[] | undefined): string[] {
	return [...new Set((guidelines ?? []).map((guideline) => guideline.trim()).filter((guideline) => guideline.length > 0))];
}

async function writeTree(root: string, files: Record<string, string>, dirs: string[] = []): Promise<void> {
	for (const dir of ["agent", ...dirs]) await mkdir(join(root, dir), { recursive: true });
	for (const [path, content] of Object.entries(files)) {
		await mkdir(dirname(join(root, path)), { recursive: true });
		await writeFile(join(root, path), content.replaceAll("<tmp>", root).replaceAll("<bom>", "\uFEFF"));
	}
}

function loader(root: string, cwd: string, trusted: boolean, flags: DiscoveryCase["flags"] = {}) {
	const agentDir = join(root, "agent");
	const settingsManager = SettingsManager.create(cwd, agentDir);
	settingsManager.setProjectTrusted(trusted);
	const resourceLoader = new DefaultResourceLoader({
		cwd,
		agentDir,
		settingsManager,
		noExtensions: true,
		noSkills: true,
		noPromptTemplates: true,
		noThemes: true,
		noContextFiles: flags.noContextFiles,
		systemPrompt: flags.systemPrompt?.replaceAll("<tmp>", root),
		appendSystemPrompt: flags.appendSystemPrompt?.map((value) => value.replaceAll("<tmp>", root)),
	});
	return { agentDir, settingsManager, resourceLoader };
}

/** Replaces the volatile parts of every string in `value`. */
function stable<T>(value: T, root: string): T {
	if (typeof value === "string") {
		return value
			.replaceAll(root, "<tmp>")
			.replaceAll(getReadmePath(), "<readme-path>")
			.replaceAll(getDocsPath(), "<docs-path>")
			.replaceAll(getExamplesPath(), "<examples-path>")
			.replaceAll(PRODUCT_SENTENCE, PRODUCT_SENTENCE.replace(" pi,", " <product>,")) as T;
	}
	if (Array.isArray(value)) return value.map((item) => stable(item, root)) as T;
	if (value && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, stable(item, root)])) as T;
	}
	return value;
}

async function capturePrompt(testCase: PromptCase) {
	const root = await mkdtemp(join(tmpdir(), "pi-system-prompt-"));
	try {
		const cwd = join(root, testCase.cwd ?? "repo");
		await writeTree(root, testCase.files ?? {}, [testCase.cwd ?? "repo"]);
		const { agentDir, settingsManager, resourceLoader } = loader(root, cwd, true, {
			systemPrompt: testCase.systemPrompt,
			appendSystemPrompt: testCase.appendSystemPrompt,
		});
		await resourceLoader.reload();
		const extensionTools = testCase.extensionTools ?? [];
		const { session } = await createAgentSession({
			cwd,
			agentDir,
			settingsManager,
			resourceLoader,
			sessionManager: SessionManager.inMemory(cwd),
			tools: testCase.tools,
			noTools: testCase.noTools,
			customTools: extensionTools.map((tool) => ({
				...tool,
				label: tool.name,
				parameters: Type.Object({}),
				execute: async () => ({ content: [{ type: "text" as const, text: "" }], details: undefined }),
			})),
		});

		const selectedTools = session.getActiveToolNames();
		const toolSnippets: Record<string, string> = {};
		const toolGuidelines: Record<string, string[]> = {};
		for (const name of selectedTools) {
			const tool = allToolNames.has(name as ToolName)
				? createToolDefinition(name as ToolName, cwd)
				: extensionTools.find((candidate) => candidate.name === name);
			const snippet = normalizeSnippet(tool?.promptSnippet);
			if (snippet) toolSnippets[name] = snippet;
			const guidelines = normalizeGuidelines(tool?.promptGuidelines);
			if (guidelines.length > 0) toolGuidelines[name] = guidelines;
		}
		const options: BuildSystemPromptOptions = {
			cwd,
			selectedTools,
			toolSnippets,
			toolGuidelines,
			customPrompt: resourceLoader.getSystemPrompt(),
			appendSystemPrompt: resourceLoader.getAppendSystemPrompt().join("\n\n"),
			contextFiles: resourceLoader.getAgentsFiles().agentsFiles,
		};
		const prompt = buildSystemPrompt(options);
		const sections = Object.entries(buildSystemPromptSections(options)).map(([name, text]) => ({ name, text }));
		const sessionPrompt = session.systemPrompt;
		session.dispose();
		if (prompt !== sessionPrompt) {
			throw new Error(`${testCase.name}: the session's prompt is not the builder's for the recorded inputs:\n${sessionPrompt}`);
		}
		if (prompt !== sections.map((section) => section.text).join("\n\n")) {
			throw new Error(`${testCase.name}: the prompt is not its sections joined by blank lines`);
		}
		const hasProduct = prompt.includes(PRODUCT_SENTENCE);
		if (hasProduct !== (options.customPrompt === undefined)) {
			throw new Error(`${testCase.name}: the default prompt no longer says "${PRODUCT_SENTENCE}"`);
		}
		return stable(
			{
				name: testCase.name,
				description: testCase.description,
				input: {
					cwd,
					selectedTools,
					extensionTools: extensionTools.map(({ name, promptSnippet, promptGuidelines }) => ({
						name,
						promptSnippet: promptSnippet ?? null,
						promptGuidelines: promptGuidelines ?? [],
					})),
					customPrompt: options.customPrompt ?? null,
					appendSystemPrompt: options.appendSystemPrompt,
					contextFiles: options.contextFiles,
				},
				sections,
				prompt,
			},
			root,
		);
	} finally {
		await rm(root, { recursive: true, force: true });
	}
}

async function captureDiscovery(testCase: DiscoveryCase) {
	const root = await mkdtemp(join(tmpdir(), "pi-system-prompt-"));
	try {
		const cwd = join(root, testCase.cwd);
		await writeTree(root, testCase.files, [testCase.cwd, ...(testCase.dirs ?? [])]);
		const { resourceLoader } = loader(root, cwd, testCase.trusted, testCase.flags);
		await resourceLoader.reload();
		return stable(
			{
				name: testCase.name,
				description: testCase.description,
				files: testCase.files,
				dirs: testCase.dirs ?? [],
				cwd: testCase.cwd,
				trusted: testCase.trusted,
				flags: {
					systemPrompt: testCase.flags?.systemPrompt ?? null,
					appendSystemPrompt: testCase.flags?.appendSystemPrompt ?? [],
					noContextFiles: testCase.flags?.noContextFiles ?? false,
				},
				result: {
					customPrompt: resourceLoader.getSystemPrompt() ?? null,
					appendSystemPrompt: resourceLoader.getAppendSystemPrompt().join("\n\n"),
					contextFiles: resourceLoader.getAgentsFiles().agentsFiles,
				},
			},
			root,
		);
	} finally {
		await rm(root, { recursive: true, force: true });
	}
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: system-prompt-fixture.ts <output-dir>");
	}
	const prompts = [];
	for (const testCase of PROMPT_CASES) prompts.push(await capturePrompt(testCase));
	const discovery = [];
	for (const testCase of DISCOVERY_CASES) discovery.push(await captureDiscovery(testCase));
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "system-prompt.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				format: {
					prompts:
						"input: what upstream's session passed its prompt builder (extensionTools as their definitions give them). sections: the builder's sections in order; prompt: their text joined by blank lines.",
					discovery:
						"files, dirs, cwd, trusted, flags: the tree and the command line. result: the custom prompt, the appended text, and the context files upstream's resource loader passes the builder.",
				},
				placeholders: PLACEHOLDERS,
				prompts,
				discovery,
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

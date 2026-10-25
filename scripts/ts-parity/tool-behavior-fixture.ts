import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	createToolDefinition,
	type ToolName,
} from "/ts-reference/packages/coding-agent/src/core/tools/index.ts";

/**
 * Runs upstream's built-in tools on fixed inputs and records what the model
 * would see: the text of each result, whether it is an error, and any images.
 *
 * The trees the tools run in are part of the fixture (`roots`), so the Rust
 * test builds the same files and runs the same calls. Volatile values are
 * replaced by placeholders; see PLACEHOLDERS and NORMALIZATIONS.
 */

/** `prefix + n + suffix` for n = 1..count, with n zero-padded to `pad` digits. */
type Series = { count: number; prefix: string; pad: number; suffix: string };
type Part = string | { repeat: string; count: number };
type Entry =
	| { path: string; parts: Part[] }
	/** One series item per line, each followed by a newline. */
	| { path: string; lines: Series }
	/** A directory holding one file per series item, each containing `text`. */
	| { path: string; files: Series; text: string }
	| { path: string; base64: string }
	| { path: string; dir: true };

type Call = {
	name: string;
	/** Root the tool is created in; also the working directory of the process. */
	cwd: "repo" | "plain" | "files";
	tool: ToolName;
	args: Record<string, unknown>;
	/** See NORMALIZATIONS. */
	normalize?: "sorted-lines" | "result-count";
};

const PIXEL_PNG =
	"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

const PLACEHOLDERS = {
	"<tmp>": "The directory holding the roots. Arguments may use it too.",
	"<output-file>": "The temp file bash saves its full output to (<os tmpdir>/pi-bash-<16 hex>.log).",
};

const NORMALIZATIONS = {
	"sorted-lines":
		"fd lists files in no fixed order, so the result lines before the first blank line are sorted.",
	"result-count":
		"Which results fd returns under a limit is not fixed, so the result lines before the first blank line are replaced by <N results>.",
	digest:
		"A text longer than 2000 characters is recorded as {bytes, lines, sha256, head, tail}, with head and tail its first and last 200 characters.",
	"ripgrep-order":
		"ripgrep searches files in no fixed order; upstream's grep passes --no-config, which ignores RIPGREP_CONFIG_PATH, so multi-file grep calls normalize with sorted-lines instead.",
};

const series = (count: number, prefix: string, pad: number, suffix: string): Series => ({ count, prefix, pad, suffix });

const ROOTS: Record<Call["cwd"], Entry[]> = {
	// A git repository: ripgrep and fd apply its .gitignore.
	repo: [
		{ path: ".git/description", parts: ["needle in git metadata\n"] },
		{ path: ".gitignore", parts: ["ignored.txt\nbuild/\n"] },
		{ path: ".github/workflows/ci.yml", parts: ["name: ci\n# needle in workflow\n"] },
		{ path: ".hidden/secret.txt", parts: ["needle hidden\n"] },
		{ path: "ignored.txt", parts: ["needle ignored\n"] },
		{ path: "build/out.txt", parts: ["needle build\n"] },
		{
			path: "src/alpha.ts",
			parts: ["export const alpha = 1;\n// needle one\nconst beta = 2;\n// Needle two\nconst gamma = 3;\n"],
		},
		{ path: "src/nested/beta.spec.ts", parts: ["// needle nested\n"] },
		{ path: "src/data.json", parts: ['{"needle": true}\n'] },
		{ path: "docs/GUIDE.MD", parts: ["# Guide\n\nneedle docs\n"] },
		{ path: "crlf.txt", parts: ["needle crlf\r\nplain\r\n"] },
	],
	// Not a git repository: ripgrep ignores its .gitignore, fd applies it.
	plain: [
		{ path: ".gitignore", parts: ["skipped.txt\n"] },
		{ path: "kept.txt", parts: ["needle kept\n"] },
		{ path: "skipped.txt", parts: ["needle skipped\n"] },
	],
	files: [
		{ path: "notes.txt", parts: ["one\ntwo\nthree\nfour\nfive\n"] },
		{ path: "no-newline.txt", parts: ["alpha\nbeta"] },
		{ path: "big.txt", lines: series(3000, "line-", 0, "") },
		{ path: "wide.txt", lines: series(1200, "w", 4, `-${"w".repeat(94)}`) },
		{ path: "huge-line.txt", parts: ["first\n", { repeat: "y", count: 60000 }, "\nlast\n"] },
		{ path: "long-line.txt", parts: ["needle ", { repeat: "x", count: 600 }, "\nshort needle\n"] },
		{ path: "many.txt", lines: series(150, "match-", 0, " needle") },
		{ path: "pixel.png", base64: PIXEL_PNG },
		{ path: "empty", dir: true },
		{ path: "ls-dir/Zeta.txt", parts: ["z"] },
		{ path: "ls-dir/alpha.txt", parts: ["a"] },
		{ path: "ls-dir/_under.txt", parts: ["u"] },
		{ path: "ls-dir/.dot", parts: ["d"] },
		{ path: "ls-dir/10.txt", parts: ["10"] },
		{ path: "ls-dir/9.txt", parts: ["9"] },
		{ path: "ls-dir/beta-file.txt", parts: ["b"] },
		{ path: "ls-dir/betafile.txt", parts: ["b"] },
		{ path: "ls-dir/sub/inner.txt", parts: ["i"] },
		{ path: "many-files", files: series(30, "file-", 3, ".txt"), text: "x" },
		{ path: "thousand", files: series(1005, "f-", 4, ".dat"), text: "x" },
		{ path: "long-names", files: series(600, `${"n".repeat(96)}-`, 4, ""), text: "x" },
	],
};

const CALLS: Call[] = [
	// read
	{ name: "read-whole-file", cwd: "files", tool: "read", args: { path: "notes.txt" } },
	{ name: "read-without-trailing-newline", cwd: "files", tool: "read", args: { path: "no-newline.txt" } },
	{ name: "read-offset-and-limit", cwd: "files", tool: "read", args: { path: "notes.txt", offset: 2, limit: 2 } },
	{ name: "read-limit-reaching-the-end", cwd: "files", tool: "read", args: { path: "notes.txt", offset: 4, limit: 10 } },
	{ name: "read-offset-of-the-last-empty-line", cwd: "files", tool: "read", args: { path: "notes.txt", offset: 6 } },
	{ name: "read-offset-past-end", cwd: "files", tool: "read", args: { path: "notes.txt", offset: 7 } },
	{ name: "read-line-cap", cwd: "files", tool: "read", args: { path: "big.txt" } },
	{ name: "read-continues-after-line-cap", cwd: "files", tool: "read", args: { path: "big.txt", offset: 2001 } },
	{ name: "read-limit-inside-large-file", cwd: "files", tool: "read", args: { path: "big.txt", offset: 2990, limit: 5 } },
	{ name: "read-limit-above-line-cap", cwd: "files", tool: "read", args: { path: "big.txt", limit: 2500 } },
	{ name: "read-offset-past-end-of-large-file", cwd: "files", tool: "read", args: { path: "big.txt", offset: 4000 } },
	{ name: "read-byte-cap", cwd: "files", tool: "read", args: { path: "wide.txt" } },
	{ name: "read-stops-before-long-line", cwd: "files", tool: "read", args: { path: "huge-line.txt" } },
	{ name: "read-long-line", cwd: "files", tool: "read", args: { path: "huge-line.txt", offset: 2 } },
	{ name: "read-absolute-path", cwd: "repo", tool: "read", args: { path: "<tmp>/files/notes.txt", limit: 1 } },
	{ name: "read-path-outside-cwd", cwd: "repo", tool: "read", args: { path: "../plain/kept.txt" } },
	{ name: "read-at-prefixed-path", cwd: "files", tool: "read", args: { path: "@no-newline.txt" } },
	{ name: "read-image", cwd: "files", tool: "read", args: { path: "pixel.png" } },
	{ name: "read-missing-file", cwd: "files", tool: "read", args: { path: "missing.txt" } },
	{ name: "read-directory", cwd: "files", tool: "read", args: { path: "empty" } },

	// bash
	{ name: "bash-stdout", cwd: "files", tool: "bash", args: { command: "echo hello" } },
	{ name: "bash-no-output", cwd: "files", tool: "bash", args: { command: "true" } },
	{ name: "bash-runs-in-cwd", cwd: "files", tool: "bash", args: { command: "pwd" } },
	{ name: "bash-is-bash", cwd: "files", tool: "bash", args: { command: "echo ${BASH_VERSION:+bash}" } },
	{
		name: "bash-is-not-a-login-shell",
		cwd: "files",
		tool: "bash",
		args: { command: "shopt -q login_shell && echo login || echo non-login" },
	},
	{ name: "bash-stdin-is-empty", cwd: "files", tool: "bash", args: { command: "cat; echo done" } },
	{
		name: "bash-interleaves-stdout-and-stderr",
		cwd: "files",
		tool: "bash",
		args: { command: "echo out1; sleep 0.2; echo err1 >&2; sleep 0.2; echo out2; sleep 0.2; echo err2 >&2" },
	},
	{
		name: "bash-exit-code",
		cwd: "files",
		tool: "bash",
		args: { command: "echo out; sleep 0.2; echo err >&2; exit 3" },
	},
	{ name: "bash-exit-code-without-output", cwd: "files", tool: "bash", args: { command: "exit 2" } },
	{ name: "bash-killed-by-signal", cwd: "files", tool: "bash", args: { command: "kill -TERM $$" } },
	{ name: "bash-tail-by-lines", cwd: "files", tool: "bash", args: { command: "seq 1 5000" } },
	{
		name: "bash-tail-by-lines-with-failure",
		cwd: "files",
		tool: "bash",
		args: { command: "seq 1 2500; echo failed >&2; exit 1" },
	},
	{ name: "bash-tail-by-bytes", cwd: "files", tool: "bash", args: { command: "cat wide.txt" } },
	{
		name: "bash-tail-of-one-long-line",
		cwd: "files",
		tool: "bash",
		args: { command: "head -c 60000 /dev/zero | tr '\\0' z" },
	},
	{ name: "bash-exactly-at-line-cap", cwd: "files", tool: "bash", args: { command: "seq 1 2000" } },
	{
		name: "bash-timeout-keeps-partial-output",
		cwd: "files",
		tool: "bash",
		args: { command: "echo started; sleep 20", timeout: 1 },
	},
	{ name: "bash-timeout-without-output", cwd: "files", tool: "bash", args: { command: "sleep 20", timeout: 0.5 } },
	{ name: "bash-invalid-timeout", cwd: "files", tool: "bash", args: { command: "echo never", timeout: 0 } },
	{
		name: "bash-returns-while-a-background-job-holds-the-pipes",
		cwd: "files",
		tool: "bash",
		args: { command: "sleep 3 & echo started" },
	},

	// grep
	{ name: "grep-respects-gitignore-and-searches-hidden-files", cwd: "repo", tool: "grep", args: { pattern: "needle" }, normalize: "sorted-lines" },
	{ name: "grep-in-directory", cwd: "repo", tool: "grep", args: { pattern: "needle", path: "src" }, normalize: "sorted-lines" },
	{ name: "grep-in-file", cwd: "repo", tool: "grep", args: { pattern: "needle", path: "src/alpha.ts" } },
	{
		name: "grep-ignore-case",
		cwd: "repo",
		tool: "grep",
		args: { pattern: "needle", path: "src/alpha.ts", ignoreCase: true },
	},
	{ name: "grep-regex", cwd: "repo", tool: "grep", args: { pattern: "^const \\w+ = \\d;$", path: "src" }, normalize: "sorted-lines" },
	{ name: "grep-literal", cwd: "repo", tool: "grep", args: { pattern: '{"needle"', path: "src", literal: true } },
	{ name: "grep-glob", cwd: "repo", tool: "grep", args: { pattern: "needle", glob: "*.ts" }, normalize: "sorted-lines" },
	{ name: "grep-glob-with-directory", cwd: "repo", tool: "grep", args: { pattern: "needle", glob: "src/*.ts" }, normalize: "sorted-lines" },
	{ name: "grep-glob-overrides-gitignore", cwd: "repo", tool: "grep", args: { pattern: "needle", glob: "*.txt" }, normalize: "sorted-lines" },
	{ name: "grep-negated-glob", cwd: "repo", tool: "grep", args: { pattern: "needle", glob: "!*.ts" }, normalize: "sorted-lines" },
	{
		name: "grep-context",
		cwd: "repo",
		tool: "grep",
		args: { pattern: "needle one", path: "src/alpha.ts", context: 1 },
	},
	{
		name: "grep-context-overlaps-and-reaches-file-end",
		cwd: "repo",
		tool: "grep",
		args: { pattern: "const (beta|gamma)", path: "src/alpha.ts", context: 2 },
	},
	{ name: "grep-crlf-file", cwd: "repo", tool: "grep", args: { pattern: "needle", path: "crlf.txt" } },
	{ name: "grep-no-matches", cwd: "repo", tool: "grep", args: { pattern: "absent-pattern" } },
	{ name: "grep-path-not-found", cwd: "repo", tool: "grep", args: { pattern: "needle", path: "missing" } },
	{ name: "grep-outside-git-ignores-gitignore", cwd: "plain", tool: "grep", args: { pattern: "needle" }, normalize: "sorted-lines" },
	{ name: "grep-default-limit", cwd: "files", tool: "grep", args: { pattern: "needle", path: "many.txt" } },
	{ name: "grep-limit", cwd: "files", tool: "grep", args: { pattern: "needle", path: "many.txt", limit: 3 } },
	{
		name: "grep-limit-equal-to-match-count",
		cwd: "files",
		tool: "grep",
		args: { pattern: "needle", path: "long-line.txt", limit: 2 },
	},
	{ name: "grep-long-line", cwd: "files", tool: "grep", args: { pattern: "needle", path: "long-line.txt" } },
	{ name: "grep-byte-cap", cwd: "files", tool: "grep", args: { pattern: "w", path: "wide.txt", limit: 1000 } },

	// find
	{ name: "find-glob", cwd: "repo", tool: "find", args: { pattern: "*.ts" }, normalize: "sorted-lines" },
	{
		name: "find-respects-gitignore-and-lists-hidden-files",
		cwd: "repo",
		tool: "find",
		args: { pattern: "*.txt" },
		normalize: "sorted-lines",
	},
	{ name: "find-everything", cwd: "repo", tool: "find", args: { pattern: "*" }, normalize: "sorted-lines" },
	{ name: "find-directory", cwd: "repo", tool: "find", args: { pattern: "nested" }, normalize: "sorted-lines" },
	{ name: "find-path", cwd: "repo", tool: "find", args: { pattern: "*.ts", path: "src/nested" }, normalize: "sorted-lines" },
	{
		name: "find-glob-with-directory",
		cwd: "repo",
		tool: "find",
		args: { pattern: "src/**/*.spec.ts" },
		normalize: "sorted-lines",
	},
	{ name: "find-lowercase-pattern-ignores-case", cwd: "repo", tool: "find", args: { pattern: "*.md" }, normalize: "sorted-lines" },
	{ name: "find-uppercase-pattern-matches-case", cwd: "repo", tool: "find", args: { pattern: "*.Md" }, normalize: "sorted-lines" },
	{ name: "find-no-matches", cwd: "repo", tool: "find", args: { pattern: "*.absent" } },
	{
		name: "find-outside-git-respects-gitignore",
		cwd: "plain",
		tool: "find",
		args: { pattern: "*.txt" },
		normalize: "sorted-lines",
	},
	{
		name: "find-limit",
		cwd: "files",
		tool: "find",
		args: { pattern: "file-*.txt", path: "many-files", limit: 5 },
		normalize: "result-count",
	},
	{
		name: "find-limit-equal-to-result-count",
		cwd: "files",
		tool: "find",
		args: { pattern: "file-*.txt", path: "many-files", limit: 30 },
		normalize: "result-count",
	},
	{
		name: "find-default-limit",
		cwd: "files",
		tool: "find",
		args: { pattern: "*.dat", path: "thousand" },
		normalize: "result-count",
	},
	{
		name: "find-byte-cap",
		cwd: "files",
		tool: "find",
		args: { pattern: "n*", path: "long-names" },
		normalize: "result-count",
	},

	// ls
	{ name: "ls-sorts-and-marks-directories", cwd: "files", tool: "ls", args: { path: "ls-dir" } },
	{ name: "ls-limit", cwd: "files", tool: "ls", args: { path: "ls-dir", limit: 3 } },
	{ name: "ls-limit-equal-to-entry-count", cwd: "files", tool: "ls", args: { path: "ls-dir", limit: 9 } },
	{ name: "ls-default-limit", cwd: "files", tool: "ls", args: { path: "thousand" } },
	{ name: "ls-byte-cap", cwd: "files", tool: "ls", args: { path: "long-names", limit: 600 } },
	{ name: "ls-cwd", cwd: "plain", tool: "ls", args: {} },
	{ name: "ls-empty-directory", cwd: "files", tool: "ls", args: { path: "empty" } },
	{ name: "ls-not-a-directory", cwd: "files", tool: "ls", args: { path: "notes.txt" } },
	{ name: "ls-path-not-found", cwd: "files", tool: "ls", args: { path: "missing" } },

	// write and edit. These change the tree, so they come last and stay in their own directory.
	{
		name: "write-creates-parent-directories",
		cwd: "files",
		tool: "write",
		args: { path: "written/deep/file.txt", content: "alpha\nbeta\ngamma\nbeta again\n" },
	},
	{ name: "write-result-is-readable", cwd: "files", tool: "read", args: { path: "written/deep/file.txt" } },
	{ name: "write-absolute-path", cwd: "repo", tool: "write", args: { path: "<tmp>/files/written/absolute.txt", content: "" } },
	{ name: "write-path-outside-cwd", cwd: "repo", tool: "write", args: { path: "../files/written/outside.txt", content: "outside" } },
	{ name: "write-outside-cwd-is-readable", cwd: "files", tool: "read", args: { path: "written/outside.txt" } },
	{
		name: "edit-replaces-blocks",
		cwd: "files",
		tool: "edit",
		args: {
			path: "written/deep/file.txt",
			edits: [
				{ oldText: "gamma", newText: "GAMMA" },
				{ oldText: "alpha\n", newText: "" },
			],
		},
	},
	{ name: "edit-result-is-readable", cwd: "files", tool: "read", args: { path: "written/deep/file.txt" } },
	{
		name: "edit-text-not-found",
		cwd: "files",
		tool: "edit",
		args: { path: "written/deep/file.txt", edits: [{ oldText: "absent", newText: "x" }] },
	},
	{
		name: "edit-text-not-unique",
		cwd: "files",
		tool: "edit",
		args: { path: "written/deep/file.txt", edits: [{ oldText: "beta", newText: "x" }] },
	},
	{
		name: "edit-missing-file",
		cwd: "files",
		tool: "edit",
		args: { path: "written/missing.txt", edits: [{ oldText: "a", newText: "b" }] },
	},
];

/** The tools whose descriptions and parameter schemas the model is sent. */
const DEFINED_TOOLS: ToolName[] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

const seriesItems = ({ count, prefix, pad, suffix }: Series): string[] =>
	Array.from({ length: count }, (_, index) => `${prefix}${String(index + 1).padStart(pad, "0")}${suffix}`);

async function writeEntry(root: string, entry: Entry): Promise<void> {
	const path = join(root, entry.path);
	if ("dir" in entry) {
		await mkdir(path, { recursive: true });
		return;
	}
	if ("files" in entry) {
		await mkdir(path, { recursive: true });
		for (const name of seriesItems(entry.files)) await writeFile(join(path, name), entry.text);
		return;
	}
	await mkdir(dirname(path), { recursive: true });
	if ("base64" in entry) {
		await writeFile(path, Buffer.from(entry.base64, "base64"));
	} else if ("lines" in entry) {
		await writeFile(path, seriesItems(entry.lines).map((line) => `${line}\n`).join(""));
	} else {
		await writeFile(path, entry.parts.map((part) => (typeof part === "string" ? part : part.repeat.repeat(part.count))).join(""));
	}
}

const sha256 = (data: string | Uint8Array) => createHash("sha256").update(data).digest("hex");

function recordText(text: string): unknown {
	if (text.length <= 2000) return text;
	return {
		bytes: Buffer.byteLength(text, "utf-8"),
		lines: text.split("\n").length,
		sha256: sha256(text),
		head: text.slice(0, 200),
		tail: text.slice(-200),
	};
}

/** Applies a call's normalization to the result lines, which end at the first blank line. */
function normalizeResults(text: string, mode: Call["normalize"]): string {
	if (!mode) return text;
	const blank = text.indexOf("\n\n");
	const results = (blank === -1 ? text : text.slice(0, blank)).split("\n");
	const rest = blank === -1 ? "" : text.slice(blank);
	// Messages such as "No files found matching pattern" are not result lines.
	if (results.length === 1 && results[0].includes(" ")) return text;
	if (mode === "sorted-lines") return `${results.sort().join("\n")}${rest}`;
	return `<${results.length} results>${rest}`;
}

async function runCall(tmp: string, call: Call): Promise<unknown> {
	const cwd = join(tmp, call.cwd);
	// ripgrep resolves --glob against its working directory, which is pi's.
	process.chdir(cwd);
	const definition = createToolDefinition(call.tool, cwd);
	const args = JSON.parse(JSON.stringify(call.args).replaceAll("<tmp>", tmp));
	let text: string;
	let isError: boolean;
	let images: Array<{ mimeType: string; bytes: number; sha256: string }> = [];
	try {
		const result = (await definition.execute(`call-${call.name}`, args, undefined, undefined, undefined as never)) as {
			content: Array<{ type: "text"; text: string } | { type: "image"; data: string; mimeType: string }>;
			isError?: boolean;
		};
		text = result.content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("\n");
		images = result.content.flatMap((block) => {
			if (block.type !== "image") return [];
			const bytes = Buffer.from(block.data, "base64");
			return [{ mimeType: block.mimeType, bytes: bytes.length, sha256: sha256(bytes) }];
		});
		isError = result.isError === true;
	} catch (error) {
		// The agent loop reports a thrown error as an error result with its message.
		text = error instanceof Error ? error.message : String(error);
		isError = true;
	}

	let savedOutput: unknown;
	const outputFile = text.match(/Full output: (\S+pi-bash-[0-9a-f]{16}\.log)\]/)?.[1];
	if (outputFile) {
		const saved = await readFile(outputFile);
		savedOutput = { bytes: saved.length, sha256: sha256(saved) };
		await rm(outputFile);
		text = text.replaceAll(outputFile, "<output-file>");
	}
	text = normalizeResults(text.replaceAll(tmp, "<tmp>"), call.normalize);

	return {
		name: call.name,
		cwd: call.cwd,
		tool: call.tool,
		args: call.args,
		...(call.normalize ? { normalize: call.normalize } : {}),
		result: {
			text: recordText(text),
			isError,
			...(images.length > 0 ? { images } : {}),
			...(savedOutput ? { savedOutput } : {}),
		},
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: tool-behavior-fixture.ts <output-dir>");
	}
	const tmp = await mkdtemp(join(tmpdir(), "pi-tool-behavior-"));
	try {
		for (const [root, entries] of Object.entries(ROOTS)) {
			await mkdir(join(tmp, root), { recursive: true });
			for (const entry of entries) await writeEntry(join(tmp, root), entry);
		}
		const calls = [];
		for (const call of CALLS) calls.push(await runCall(tmp, call));
		process.chdir(tmpdir());

		const definitions = DEFINED_TOOLS.map((name) => {
			const definition = createToolDefinition(name, "/repo");
			return {
				name: definition.name,
				description: definition.description,
				parameters: JSON.parse(JSON.stringify(definition.parameters)),
			};
		});

		await mkdir(outputDir, { recursive: true });
		await writeFile(
			join(outputDir, "tool-behavior.json"),
			`${JSON.stringify(
				{
					source: {
						repository: "https://github.com/earendil-works/pi",
						ref: "main",
						script: fileURLToPath(import.meta.url),
					},
					placeholders: PLACEHOLDERS,
					normalizations: NORMALIZATIONS,
					definitions,
					roots: ROOTS,
					calls,
				},
				null,
				2,
			)}\n`,
		);
	} finally {
		await rm(tmp, { recursive: true, force: true });
	}
}

main()
	.then(() => {
		// A background job of a bash call may still hold the event loop open.
		process.exit(process.exitCode ?? 0);
	})
	.catch((error) => {
		console.error(error);
		process.exit(1);
	});

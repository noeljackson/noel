import { execFileSync } from "node:child_process";
import { isDeepStrictEqual } from "node:util";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { NodeExecutionEnv } from "/chord-reference/packages/durable/src/env/node.ts";
import { createEnvConformance } from "/chord-reference/packages/durable/src/testing/env-conformance.ts";

/**
 * Goldens for noel-env's LocalEnv: upstream Durable's env conformance cases
 * (packages/durable/src/testing/env-conformance.ts) run against
 * NodeExecutionEnv, the behavioural reference (packages/env/docs/semantics.md).
 *
 * Every assertion a case makes is recorded in order with its actual and
 * expected values, so the Rust test can run the same case against LocalEnv
 * and compare what it observes. Durable is read from /chord-reference, the
 * checkout of the pinned commit (CHORD_REFERENCE_REF in the Makefile).
 */

const REFERENCE = "/chord-reference";
const SHELL = ["sh", "-c"];

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

type Assertion = { assert: string; actual: Json; expected?: Json; message?: string };

const git = (...args: string[]): string =>
	execFileSync("git", ["-C", REFERENCE, ...args], { encoding: "utf8" }).trim();

/** The case running now: its log and its working directory. */
let current: { log: Assertion[]; cwd: string } | undefined;

/**
 * A recorded value: the case's working directory and the temporary directory
 * become placeholders, and so do modification times.
 */
function recorded(value: unknown): Json {
	if (value === undefined) return null;
	if (value === null || typeof value === "boolean" || typeof value === "number") return value;
	if (typeof value === "string") {
		let text = value;
		if (current !== undefined) text = text.split(current.cwd).join("<cwd>");
		return text.split(tmpdir()).join("<tmp>");
	}
	if (value instanceof Uint8Array) return Array.from(value);
	if (Array.isArray(value)) return value.map(recorded);
	if (typeof value === "object") {
		const object: { [key: string]: Json } = {};
		for (const [key, field] of Object.entries(value as Record<string, unknown>)) {
			object[key] = key === "mtimeMs" && typeof field === "number" ? "<mtime>" : recorded(field);
		}
		return object;
	}
	throw new Error(`cannot record ${typeof value}`);
}

function record(assert: string, actual: unknown, expected?: unknown, message?: string): void {
	if (current === undefined) throw new Error("an assertion outside a case");
	current.log.push({
		assert,
		actual: recorded(actual),
		...(expected === undefined ? {} : { expected: recorded(expected) }),
		...(message === undefined ? {} : { message }),
	});
}

function fail(what: string, actual: unknown, expected: unknown): never {
	throw new Error(`${what}: ${JSON.stringify(recorded(actual))} vs ${JSON.stringify(recorded(expected))}`);
}

/** Vitest's toMatchObject on plain data: every expected field matches. */
function matches(actual: unknown, expected: unknown): boolean {
	if (Array.isArray(expected)) {
		return (
			Array.isArray(actual) &&
			actual.length === expected.length &&
			expected.every((item, index) => matches(actual[index], item))
		);
	}
	if (expected !== null && typeof expected === "object") {
		if (actual === null || typeof actual !== "object") return false;
		return Object.entries(expected).every(([key, field]) =>
			matches((actual as Record<string, unknown>)[key], field),
		);
	}
	return Object.is(actual, expected);
}

/** The assertions upstream's runner adapter makes (createExpectAssertions), recorded. */
const assertions = {
	ok(value: unknown, message?: string) {
		record("ok", value, undefined, message);
		if (!value) fail(message ?? "ok", value, true);
	},
	strictEqual(actual: unknown, expected: unknown) {
		record("strictEqual", actual, expected);
		if (!Object.is(actual, expected)) fail("strictEqual", actual, expected);
	},
	deepEqual(actual: unknown, expected: unknown) {
		record("deepEqual", actual, expected);
		if (!isDeepStrictEqual(actual, expected)) fail("deepEqual", actual, expected);
	},
	partialDeepEqual(actual: unknown, expected: unknown) {
		record("partialDeepEqual", actual, expected);
		if (!matches(actual, expected)) fail("partialDeepEqual", actual, expected);
	},
	greaterThan(actual: number, expected: number) {
		record("greaterThan", actual, expected);
		if (!(actual > expected)) fail("greaterThan", actual, expected);
	},
	async rejects(operation: Promise<unknown>, messageIncludes: string) {
		let message: string | undefined;
		try {
			await operation;
		} catch (error) {
			message = error instanceof Error ? error.message : String(error);
		}
		record("rejects", message ?? null, messageIncludes);
		if (message === undefined || !message.includes(messageIncludes)) fail("rejects", message, messageIncludes);
	},
};

/** Watch cases wait for change reports, whose number and timing vary from run to run. */
const timingDependent = (name: string): boolean => name.startsWith("watch ");

async function main(): Promise<void> {
	const fixturesDir = process.argv[2];
	if (!fixturesDir) throw new Error("usage: local-env-fixture.ts <fixtures-dir>");
	const cases = createEnvConformance({
		assertions,
		shell: SHELL,
		symlinks: true,
		listFiles: false,
		withEnv: async (use) => {
			const cwd = await mkdtemp(join(tmpdir(), "noel-local-env-"));
			try {
				current = { log: [], cwd };
				await use(new NodeExecutionEnv({ cwd }));
			} finally {
				await rm(cwd, { recursive: true, force: true });
			}
		},
	});
	const results = [];
	for (const testCase of cases) {
		let outcome: { outcome: "pass" } | { outcome: "fail"; error: string } = { outcome: "pass" };
		try {
			await testCase.run();
		} catch (error) {
			outcome = { outcome: "fail", error: String(recorded(error instanceof Error ? error.message : error)) };
		}
		const log = current?.log ?? [];
		current = undefined;
		results.push(
			timingDependent(testCase.name)
				? { name: testCase.name, ...outcome, recorded: "outcome" }
				: { name: testCase.name, ...outcome, recorded: "assertions", assertions: log },
		);
	}
	const failed = results.filter((result) => result.outcome !== "pass");
	if (failed.length > 0) throw new Error(`NodeExecutionEnv failed ${failed.map((r) => r.name).join(", ")}`);

	await writeFile(
		join(fixturesDir, "local-env.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: git("rev-parse", "HEAD"),
					package: "packages/durable",
					envCommit: git(
						"log",
						"-1",
						"--format=%H",
						"HEAD",
						"--",
						"packages/durable/src/env",
						"packages/durable/src/testing/env-conformance.ts",
					),
					script: fileURLToPath(import.meta.url),
				},
				format: {
					cases: "createEnvConformance() against new NodeExecutionEnv({cwd}), cwd a fresh mkdtemp directory per case, in the suite's order.",
					options: "shell [\"sh\", \"-c\"], symlinks true, listFiles false (NodeExecutionEnv has no listFiles).",
					assertions:
						"Every assertion the case made, in order: the assertion, the actual value and the expected one. The case's cwd is <cwd>, the temporary directory <tmp>, and every mtimeMs <mtime>. Bytes are arrays of numbers; undefined is null.",
					outcome:
						"Watch cases wait for change reports whose number and timing vary, so only their outcome is recorded.",
				},
				options: { shell: SHELL, symlinks: true, listFiles: false },
				cases: results,
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

import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

type CliOption = {
	long: string;
	short?: string;
};

function runHelp(): string {
	const result = spawnSync(
		"npx",
		["tsx", "/ts-reference/packages/coding-agent/src/cli.ts", "--help"],
		{
			cwd: "/ts-reference",
			encoding: "utf8",
			env: {
				...process.env,
				PI_OFFLINE: "1",
			},
		},
	);
	if (result.status !== 0) {
		throw new Error(`failed to capture CLI help:\n${result.stdout}\n${result.stderr}`);
	}
	return result.stdout || result.stderr;
}

function parseOptions(help: string): CliOption[] {
	const options = new Map<string, CliOption>();
	for (const line of help.split("\n")) {
		const match = line.match(/^\s+(--[a-z0-9-]+)(?:[ ,]+(-[a-z0-9]+))?/i);
		if (!match) continue;
		const long = match[1].slice(2);
		const short = match[2]?.slice(1);
		options.set(long, short ? { long, short } : { long });
	}
	return [...options.values()].sort((left, right) => left.long.localeCompare(right.long));
}

function parseCommands(help: string): string[] {
	const commands = new Set<string>();
	for (const line of help.split("\n")) {
		const match = line.match(/^\s+pi\s+([a-z][a-z-]*)\b/);
		if (match) commands.add(match[1]);
	}
	return [...commands].sort();
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: cli-contract-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	const help = runHelp();
	await writeFile(
		join(outputDir, "cli-contract.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				about: help.split("\n")[0],
				options: parseOptions(help),
				commands: parseCommands(help),
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

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import type { Connection } from "/chord-reference/packages/env/src/connection.ts";
import {
	acceptHostKey,
	connectSsh,
	scanHostKey,
	type SshTarget,
	sshArguments,
	sshConnection,
} from "/chord-reference/packages/env/src/ssh.ts";

/**
 * Goldens for noel-env's SSH deployment (crates/noel-env/src/ssh.rs): upstream pi-env's `ssh.ts` drives a stand-in
 * for `ssh` (fake-ssh.sh, next to this script) that runs each remote command here, in a scratch home, and logs every
 * invocation. The arguments of each invocation (ssh's options, the host, the remote command: detection, the daemon
 * check, the upload script, the daemon's launch) are written to pi-env-ssh.json, with `sshArguments` for two targets.
 * crates/noel-env-daemon/tests/ssh_deploy.rs drives noel's port through the same stand-in and compares.
 *
 * Run with Node's type stripping and the `source` export condition, as the caller:
 * `node --conditions=source /ts-parity/pi-env-ssh-fixture.ts <fixtures-dir>`.
 */

const REFERENCE = "/chord-reference";
const DAEMON = "/usr/local/bin/pi-env";
const FAKE = join(import.meta.dirname, "fake-ssh.sh");

const git = (...args: string[]): string =>
	execFileSync("git", ["-c", `safe.directory=${REFERENCE}`, "-C", REFERENCE, ...args], { encoding: "utf8" }).trim();

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** A value for a sourced shell file. */
const shellQuote = (text: string) => `'${text.replaceAll("'", "'\\''")}'`;

/** Invocations in the log: an argument count, then the arguments, each followed by a NUL byte. */
function invocations(log: string): string[][] {
	const fields = readFileSync(log, "utf8").split("\0");
	fields.pop();
	const result: string[][] = [];
	for (let index = 0; index < fields.length; ) {
		const count = Number(fields[index]);
		result.push(fields.slice(index + 1, index + 1 + count));
		index += 1 + count;
	}
	return result;
}

async function main(): Promise<void> {
	const fixturesDir = process.argv[2];
	if (!fixturesDir) throw new Error("usage: pi-env-ssh-fixture.ts <fixtures-dir>");

	const full: SshTarget = {
		host: "gpu-box",
		user: "me",
		port: 2222,
		identityFile: "/keys/id %h",
		knownHostsFile: "/data/known hosts",
		hostKeyAlias: "env-gpu",
		configFile: "/data/ssh config",
	};
	const minimal: SshTarget = { host: "gpu-box", knownHostsFile: "/data/known_hosts", hostKeyAlias: "env-gpu" };
	const args = {
		minimal: sshArguments(minimal),
		full: sshArguments(full),
		scan: sshArguments(full, false, "/scan/known_hosts"),
	};

	const root = mkdtempSync(join(tmpdir(), "pi-env-ssh-golden-"));
	const steps: { step: string; invocations: string[][] }[] = [];
	try {
		const home = join(root, "home");
		mkdirSync(home);
		execFileSync("ssh-keygen", ["-q", "-t", "ed25519", "-N", "", "-f", join(root, "host_key")]);
		const hostKey = readFileSync(join(root, "host_key.pub"), "utf8").split(" ").slice(0, 2).join(" ");
		const ssh = join(root, "ssh");
		copyFileSync(FAKE, ssh);
		chmodSync(ssh, 0o755);
		const log = join(root, "ssh.log");
		writeFileSync(log, "");
		writeFileSync(
			join(root, "fake-ssh.env"),
			`LOG=${shellQuote(log)}\nHOME_DIR=${shellQuote(home)}\nHOST_KEY=${shellQuote(hostKey)}\n`,
		);
		const target: SshTarget = {
			host: "fake-host",
			knownHostsFile: join(root, "known_hosts"),
			hostKeyAlias: "env-test",
			ssh,
		};
		const sha256 = createHash("sha256").update(readFileSync(DAEMON)).digest("hex");

		let seen = 0;
		const step = async (name: string, run: () => Promise<void>) => {
			await run();
			const all = invocations(log);
			steps.push({ step: name, invocations: all.slice(seen) });
			seen = all.length;
		};
		const start = async (connection: Connection) => {
			await connection.info();
			connection.close();
			// The fake's shell and the daemon see their input end.
			await sleep(300);
		};

		await step("scanHostKey", async () => {
			await acceptHostKey(target, (await scanHostKey(target)).lines);
		});
		let connection: Connection | undefined;
		await step("connectSsh deploys", async () => {
			connection = (await connectSsh({ ...target, binary: DAEMON })).connection;
		});
		await step("the first start uses the deployment's check", () => start(connection!));
		await step("connectSsh finds the daemon", async () => {
			connection = (await connectSsh({ ...target, binary: DAEMON })).connection;
		});
		await step("start", () => start(connection!));
		await step("connectSsh with loginShell", async () => {
			connection = (await connectSsh({ ...target, binary: DAEMON, loginShell: true })).connection;
		});
		await step("start through the login shell", () => start(connection!));
		await step("sshConnection: the first request detects, checks and starts", () =>
			start(sshConnection({ ...target, binary: DAEMON }).connection),
		);

		// Placeholders: the scratch directory, the scan's temporary directory, the daemon's hash and the token.
		const normalized = (text: string, previous: string | undefined) => {
			if (previous === "--token") return "<token>";
			return text
				.replaceAll(root, "<root>")
				.replace(/\/[^"]*\/pi-env-hostkey-[^/"]*\//g, "<scan>/")
				.replaceAll(sha256, "<sha256>")
				.replaceAll(sha256.slice(0, 32), "<sha32>");
		};
		for (const entry of steps) {
			entry.invocations = entry.invocations.map((argv) =>
				argv.map((arg, index) => normalized(arg, argv[index - 1])),
			);
		}
	} finally {
		rmSync(root, { recursive: true, force: true });
	}

	writeFileSync(
		join(fixturesDir, "pi-env-ssh.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: git("rev-parse", "HEAD"),
					package: "packages/env",
					envCommit: git("log", "-1", "--format=%H", "HEAD", "--", "packages/env"),
					// Where the image puts it (`make ts-parity-pi-env-ssh`).
					script: `/ts-parity/${basename(import.meta.filename)}`,
				},
				format: {
					arguments:
						"sshArguments(target) for a minimal and a full target, and sshArguments(full, false, '/scan/known_hosts') as scanHostKey calls it.",
					steps:
						"Each step's ssh invocations, in order, as argument lists after the program: ssh's options, the host, the remote command (and the daemon's serve arguments). The stand-in (fake-ssh.sh) runs each command in a scratch home on Linux x86-64 or arm64.",
					normalized:
						"<root> is the scratch directory (home, known hosts, the stand-in); <scan> the directory scanHostKey makes; <sha256> and <sha32> the daemon's SHA-256 and its first 32 hex digits; <token> the serve token.",
				},
				arguments: args,
				steps,
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

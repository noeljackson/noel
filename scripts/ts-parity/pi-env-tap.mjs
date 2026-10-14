// A transparent tap between upstream's pi-env client and a daemon, for the pi-env goldens (pi-env-fixture.ts). The
// client starts it as its daemon command: `node pi-env-tap.mjs <log> <daemon> serve --token <hex>`. It starts
// `<daemon> serve --token <hex>`, passes bytes both ways unchanged, and appends every chunk to <log> in the order it
// saw them: one byte for the direction (0 client to daemon, 1 daemon to client), a big-endian u32 length, the bytes.
import { spawn } from "node:child_process";
import { openSync, writeSync } from "node:fs";

const [log, daemon, ...args] = process.argv.slice(2);
const fd = openSync(log, "a");

function record(direction, chunk) {
	const header = Buffer.alloc(5);
	header.writeUInt8(direction, 0);
	header.writeUInt32BE(chunk.length, 1);
	writeSync(fd, Buffer.concat([header, chunk]));
}

const child = spawn(daemon, args, { stdio: ["pipe", "pipe", "inherit"] });
process.stdin.on("data", (chunk) => {
	record(0, chunk);
	child.stdin.write(chunk);
});
process.stdin.on("end", () => child.stdin.end());
child.stdin.on("error", () => {});
child.stdout.on("data", (chunk) => {
	record(1, chunk);
	process.stdout.write(chunk);
});
child.on("exit", (code) => process.exit(code ?? 1));
// The client stops a session by killing its command: stop the daemon too, as a closed ssh connection would.
process.on("SIGTERM", () => child.stdin.end());

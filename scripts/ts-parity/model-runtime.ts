import { mkdir, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * Upstream's model runtime as a fresh install sees it: an empty agent
 * directory (no credentials, no models.json, no stored catalogs) and no
 * network. Its models are the built-in catalog.
 *
 * The catalog is not part of upstream's source tree. `npm run generate-models`
 * writes it from the providers' listings when the reference image is built, so
 * it is a snapshot of that moment, fixed for the life of the image.
 */
export async function builtinModelRuntime() {
	const dir = await mkdtemp(join(tmpdir(), "pi-model-fixture-"));
	const agentDir = join(dir, "agent");
	await mkdir(agentDir, { recursive: true });
	process.env.PI_CODING_AGENT_DIR = agentDir;
	process.env.PI_OFFLINE = "1";
	const { ModelRuntime } = await import("/ts-reference/packages/coding-agent/src/core/model-runtime.ts");
	return ModelRuntime.create({ modelsPath: null, authPath: join(agentDir, "auth.json") });
}

/** The fixture header every parity fixture carries. */
export function fixtureSource(script: string) {
	return {
		repository: "https://github.com/earendil-works/pi",
		ref: "main",
		script,
	};
}

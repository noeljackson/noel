import { spawn } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	findExactModelReferenceMatch,
	findInitialModel,
	resolveCliModel,
	resolveModelScopeFromModels,
} from "/ts-reference/packages/coding-agent/src/core/model-resolver.ts";
import { ModelRuntime } from "/ts-reference/packages/coding-agent/src/core/model-runtime.ts";
// The glob matcher upstream's scope resolution calls.
import { minimatch } from "/ts-reference/node_modules/minimatch/dist/esm/index.js";
import { builtinModelRuntime, fixtureSource } from "./model-runtime.ts";

/**
 * How upstream picks a model, recorded from its own functions.
 *
 * `cliModel`, `modelScope`, `exactReference`, and `initialModel` call the
 * resolver functions on the built-in catalog (`model-catalog.json` holds the
 * same catalog) with a stand-in for the model runtime that reports the
 * providers in `authenticated` as having credentials.
 *
 * `credentials` are the one part the stand-in replaces: which credentials make
 * a provider's models available. Each case builds upstream's real model
 * runtime over an agent directory holding the case's `auth.json`, with the
 * case's environment, and records which of the providers it asks about have
 * credentials (`hasConfiguredAuth`) and the model `findInitialModel` then
 * starts on. Upstream keeps credentials per provider, and each provider says
 * what it takes: `openai-codex` a ChatGPT login and no API key, `openai` an
 * API key or its own ChatGPT sign-in, `anthropic` a key or a Claude login,
 * and the two Cloudflare providers one key plus the ids their URLs hold.
 *
 * `globs` are what the matcher behind scope globs answers, called as upstream
 * calls it: `minimatch(text, pattern, { nocase: true })`.
 *
 * `launches` start upstream `pi --mode rpc` in an agent directory holding
 * only the case's settings and `auth.json`, with the case's arguments and
 * provider keys, send the RPC commands, and record the answers: what a
 * supervisor sees of the model and the thinking level in effect. The keys
 * and logins are fake, the logins never expire, and no request is sent.
 */

type Json = Record<string, any>;

const CLI = "/ts-reference/packages/coding-agent/src/cli.ts";
const WAIT_MS = 60_000;
/** No catalog refresh, version check, or telemetry. */
const PI_ENV = { PI_OFFLINE: "1", PI_SKIP_VERSION_CHECK: "1", PI_TELEMETRY: "0" };
/** Fake keys: a provider counts as authenticated when its variable is set. */
const KEYS: Record<string, Record<string, string>> = {
	anthropic: { ANTHROPIC_API_KEY: "sk-ant-fixture" },
	openai: { OPENAI_API_KEY: "sk-fixture" },
	google: { GEMINI_API_KEY: "fixture" },
};

const cliModelCases: Array<{ provider?: string; model: string; thinking?: string; authenticated?: string[] }> = [
	// Exact references.
	{ model: "anthropic/claude-opus-4-8" },
	{ model: "ANTHROPIC/Claude-Opus-4-8" },
	{ model: "anthropic/claude-opus-4-8:max" },
	{ model: "anthropic/claude-opus-4-8:max", thinking: "low" },
	{ model: "anthropic/claude-opus-4-8:bogus" },
	{ model: "claude-haiku-4-5-20251001" },
	{ model: "Claude-Haiku-4-5-20251001" },
	{ model: "CLAUDE-OPUS-4-8", provider: "anthropic" },
	{ model: "devstral-medium-latest:high" },
	// A bare id several providers have: credentials decide, or it is an error.
	{ model: "gpt-5.5" },
	{ model: "gpt-5.5", authenticated: ["openai"] },
	{ model: "gpt-5.5", authenticated: ["openai", "azure"] },
	{ model: "GPT-5.5", authenticated: ["github-copilot"] },
	{ model: "claude-opus-4-8", authenticated: ["anthropic"] },
	// Fuzzy matches on id or name: an alias before a dated id, the highest id first.
	{ model: "sonnet:high" },
	{ model: "sonnet" },
	{ model: "opus" },
	{ model: "haiku:low", authenticated: ["anthropic"] },
	{ model: "opus", provider: "anthropic" },
	{ model: "haiku-4", provider: "anthropic" },
	{ model: "20251001", provider: "anthropic" },
	{ model: "Opus 4.8", provider: "anthropic" },
	{ model: "gpt-5", provider: "openai" },
	{ model: "gpt", provider: "openai" },
	{ model: "codex", provider: "openai" },
	{ model: "gemini", provider: "google" },
	{ model: "kimi", provider: "huggingface" },
	{ model: "minimax", provider: "minimax" },
	{ model: "glm", provider: "zai" },
	{ model: "claude", provider: "amazon-bedrock" },
	{ model: "claude", provider: "openrouter" },
	{ model: "kimi" },
	{ model: "glm-5" },
	// --provider.
	{ model: "claude-opus-4-8", provider: "ANTHROPIC" },
	{ model: "anthropic/opus", provider: "anthropic" },
	{ model: "gemini-2.5-pro:xhigh", provider: "google" },
	{ model: "moonshotai/kimi-k2.6", provider: "openrouter" },
	{ model: "new-model", provider: "nope" },
	// provider/ prefixes and ids that contain slashes.
	{ model: "zai/glm-5" },
	{ model: "zai/glm-5.1" },
	{ model: "moonshotai/kimi-k2.6" },
	{ model: "moonshotai/kimi-k2.6", authenticated: ["openrouter"] },
	{ model: "moonshotai/kimi-k2.6", authenticated: ["openrouter", "moonshotai"] },
	{ model: "openai/gpt-4o:extended" },
	{ model: "amazon.nova-pro-v1:0" },
	{ model: "amazon-bedrock/amazon.nova-pro-v1:0:low" },
	{ model: "amazon.nova-pro-v1:0:low" },
	// An id the provider does not list becomes a custom model.
	{ model: "anthropic/new-model" },
	{ model: "anthropic/new-model:high" },
	{ model: "anthropic/new-model:off" },
	{ model: "anthropic/new-model:bogus" },
	{ model: "anthropic/new-model", thinking: "high" },
	{ model: "anthropic/new-model:low", thinking: "off" },
	{ model: "openai/gpt-4.1-custom" },
	{ model: "openai/gpt-4.1-custom", thinking: "medium" },
	{ model: "new-model", provider: "mistral" },
	// Asking a custom model to think makes it a reasoning model.
	{ model: "new-model", provider: "mistral", thinking: "high" },
	{ model: "new-model", provider: "mistral", thinking: "off" },
	{ model: "mistral/new-model:low" },
	{ model: "typesafe/new-model" },
	// Nothing matches.
	{ model: "totally-unknown" },
	{ model: "nope/new-model" },
	{ model: "" },
];

const modelScopeCases: Array<{ patterns: string[]; authenticated: string[] }> = [
	{ patterns: ["anthropic/*"], authenticated: ["anthropic"] },
	{ patterns: ["anthropic/*"], authenticated: ["anthropic", "openrouter"] },
	{ patterns: ["anthropic/*", "openai/*"], authenticated: ["anthropic"] },
	{ patterns: ["ANTHROPIC/CLAUDE-OPUS-*:high"], authenticated: ["anthropic"] },
	{ patterns: ["*sonnet*"], authenticated: ["anthropic", "openrouter"] },
	{ patterns: ["*sonnet*:bogus"], authenticated: ["anthropic"] },
	{ patterns: ["openrouter/*"], authenticated: ["openrouter"] },
	{ patterns: ["openrouter/anthropic/**"], authenticated: ["openrouter"] },
	{ patterns: ["openrouter/anthropic/claude-opus-4.?"], authenticated: ["openrouter"] },
	{ patterns: ["gpt-5.[45]", "gpt-[!0-5]*-sol"], authenticated: ["openai"] },
	{ patterns: ["{anthropic,openai}/gpt-5.5", "{anthropic,openai}/gpt-5.?"], authenticated: ["openai"] },
	{ patterns: ["gpt-5.5", "openai/gpt-5.5", "gpt-5.*"], authenticated: ["openai"] },
	{
		patterns: ["claude-opus-4-8:high", "gpt-5.5", "sonnet:bogus", "haiku:low", "nomatch", "anthropic/claude-opus-4-8"],
		authenticated: ["anthropic", "openai"],
	},
	{ patterns: ["gpt-5.5"], authenticated: ["openai", "azure"] },
	{ patterns: ["amazon.nova-pro-v1:0", "amazon.nova-lite-v1:0:low", "*nova-micro*:high"], authenticated: ["amazon-bedrock"] },
	// Ids with capitals: matching ignores case.
	{ patterns: ["minimax-m*", "moonshotai/kimi-*:low", "MINIMAX-M3"], authenticated: ["minimax", "huggingface"] },
	{ patterns: ["anthropic/*"], authenticated: [] },
	{ patterns: [], authenticated: ["anthropic"] },
];

const globCases: Array<[pattern: string, text: string]> = [
	["anthropic/*", "anthropic/claude-opus-4-8"],
	["anthropic/*", "openrouter/anthropic/claude-opus-4.8"],
	["ANTHROPIC/Claude-*", "anthropic/claude-opus-4-8"],
	["*sonnet*", "claude-sonnet-5"],
	["*sonnet*", "anthropic/claude-sonnet-5"],
	["*.*", "gpt-5.5"],
	["gpt-5.5", "GPT-5.5"],
	["a*b*c", "aXXbYYc"],
	["*:free", "qwen3-coder:free"],
	["@cf/*/*", "@cf/moonshotai/kimi-k2.6"],
	// `*` and `?` stay inside one `/`-separated part and need a character.
	["a?c", "a/c"],
	["a[/]c", "a/c"],
	["*", ""],
	["", ""],
	["*/", "anthropic/"],
	["anthropic/", "anthropic"],
	// `**` spans parts.
	["openrouter/**", "openrouter/anthropic/claude-opus-4.8"],
	["openrouter/**", "openrouter/auto"],
	["openrouter/**", "openrouter"],
	["**/claude-*", "openrouter/anthropic/claude-opus-4.8"],
	["**/claude-*", "claude-opus-4.8"],
	["**", "openrouter/anthropic/claude-opus-4.8"],
	["**", "claude"],
	["*/**/claude-*", "openrouter/anthropic/claude-opus-4.8"],
	["a/**/b", "a/b"],
	["a/**/b", "a/x/y/b"],
	["claude**", "claude-opus"],
	["claude**", "claude/opus"],
	// Classes.
	["gpt-5.[45]", "gpt-5.4"],
	["gpt-5.[45]", "gpt-5.6"],
	["gpt-[!0-5]*", "gpt-6-sol"],
	["gpt-[^0-5]*", "gpt-5"],
	["gpt-?.5", "gpt-5.5"],
	["gpt-?.5", "gpt-5.55"],
	["[a-c]*", "Claude"],
	["[A-C]*", "claude"],
	["[]x]*", "]y"],
	["[!]x]*", "ay"],
	["gpt-5[", "gpt-5["],
	["gpt-5[*", "gpt-5[x"],
	// Alternatives and ranges.
	["{anthropic,openai}/gpt-5.?", "openai/gpt-5.5"],
	["{anthropic,openai}/gpt-5.?", "azure/gpt-5.5"],
	["a{b,c{d,e}}*", "acez"],
	["a{b,c{d,e}}*", "abz"],
	["{single}*", "{single}x"],
	["{single}*", "singlex"],
	["gpt-{4..6}*", "gpt-5.5"],
	["gpt-{4..6}*", "gpt-7"],
	// Escapes, negation, comments, hidden parts.
	["x\\*y", "x*y"],
	["x\\*y", "xzy"],
	["!anthropic/*", "openai/gpt-5.5"],
	["!anthropic/*", "anthropic/claude-opus-4-8"],
	["!!anthropic/*", "anthropic/claude-opus-4-8"],
	["#*", "#x"],
	["*", ".hidden"],
	[".*", ".hidden"],
	["**", ".hidden/x"],
	["**", "x/.hidden"],
];

const exactReferenceCases: Array<{
	reference: string;
	authenticated: string[];
	/** Models of a `models.json` provider, added to the list. */
	custom?: Array<{ provider: string; id: string }>;
}> = [
	{ reference: "anthropic/claude-opus-4-8", authenticated: ["anthropic"] },
	{ reference: " Anthropic/Claude-Opus-4-8 ", authenticated: ["anthropic"] },
	{ reference: "claude-opus-4-8", authenticated: ["anthropic"] },
	{ reference: "CLAUDE-OPUS-4-8", authenticated: ["anthropic"] },
	// An id with capitals, and a reference with spaces around its slash.
	{ reference: "minimax-m3", authenticated: ["minimax"] },
	{ reference: "MINIMAX/minimax-m3", authenticated: ["minimax"] },
	{ reference: "Anthropic / Claude-Opus-4-8", authenticated: ["anthropic"] },
	{ reference: "minimax / MINIMAX-M3", authenticated: ["minimax"] },
	{ reference: "claude-opus-4-8", authenticated: ["anthropic", "github-copilot"] },
	{ reference: "moonshotai/kimi-k2.6", authenticated: ["openrouter"] },
	{ reference: "moonshotai/kimi-k2.6", authenticated: ["openrouter", "moonshotai"] },
	{ reference: "openrouter/moonshotai/kimi-k2.6", authenticated: ["openrouter", "moonshotai"] },
	{ reference: "opus", authenticated: ["anthropic"] },
	{ reference: "anthropic/claude-opus-4-8", authenticated: ["openai"] },
	{ reference: "", authenticated: ["anthropic"] },
	// A provider id may hold a slash: the whole reference is tried before it is split.
	{ reference: "Team/Local/m1", authenticated: [], custom: [{ provider: "team/Local", id: "M1" }] },
	{ reference: "team/local/m1", authenticated: ["anthropic"], custom: [{ provider: "team", id: "local/m1" }] },
];

const initialModelCases: Array<{ defaultProvider?: string; defaultModel?: string; authenticated: string[] }> = [
	{ authenticated: [] },
	{ authenticated: ["anthropic"] },
	{ authenticated: ["openai", "anthropic"] },
	{ authenticated: ["zai", "openrouter"] },
	{ authenticated: ["typesafe"] },
	{ defaultProvider: "openai", defaultModel: "gpt-5.4", authenticated: ["openai", "anthropic"] },
	{ defaultProvider: "openai", defaultModel: "gpt-5.4", authenticated: ["anthropic"] },
	{ defaultProvider: "anthropic", defaultModel: "no-such-model", authenticated: ["anthropic"] },
	{ defaultProvider: "anthropic", authenticated: ["openai"] },
];

/** A login as upstream stores one in `auth.json`. It never expires, so nothing refreshes it. */
const login = { type: "oauth", access: "fixture-access-token", refresh: "fixture-refresh-token", expires: 4102444800000 };
const storedKey = { type: "api_key", key: "sk-fixture" };
/** The providers whose logins and keys a user could take for one another. */
const LOGIN_PROVIDERS = ["anthropic", "openai", "openai-codex"];
const CLOUDFLARE_PROVIDERS = ["cloudflare-workers-ai", "cloudflare-ai-gateway"];
const cloudflareKey = { CLOUDFLARE_API_KEY: "fixture" };

const credentialCases: Array<{
	name: string;
	auth?: Json;
	env?: Record<string, string>;
	/** The providers the case asks about; the login providers when absent. */
	providers?: string[];
}> = [
	{ name: "chatgpt-login-stored-for-openai-codex", auth: { "openai-codex": login } },
	{ name: "api-key-stored-for-openai", auth: { openai: storedKey } },
	{ name: "api-key-in-the-environment", env: KEYS.openai },
	{ name: "api-key-stored-for-openai-codex", auth: { "openai-codex": storedKey } },
	{ name: "chatgpt-login-beside-an-api-key", auth: { "openai-codex": login }, env: KEYS.openai },
	{ name: "api-key-stored-for-openai-codex-beside-an-api-key", auth: { "openai-codex": storedKey }, env: KEYS.openai },
	// Upstream's own sign-in for `openai`: a token its Responses API takes, from a flow pi does not have.
	{ name: "chatgpt-sign-in-stored-for-openai", auth: { openai: login } },
	{ name: "claude-login-stored-for-anthropic", auth: { anthropic: login } },
	{ name: "api-key-stored-for-anthropic", auth: { anthropic: storedKey } },
	{ name: "claude-token-in-the-environment", env: { ANTHROPIC_OAUTH_TOKEN: "fixture-oauth-token" } },
	{ name: "claude-login-beside-a-chatgpt-login", auth: { anthropic: login, "openai-codex": login } },
	{ name: "nothing" },
	// One key serves both Cloudflare providers, each only with the ids its URLs are built from.
	{ name: "cloudflare-key", env: cloudflareKey, providers: CLOUDFLARE_PROVIDERS },
	{
		name: "cloudflare-key-and-account",
		env: { ...cloudflareKey, CLOUDFLARE_ACCOUNT_ID: "fixture-account" },
		providers: CLOUDFLARE_PROVIDERS,
	},
	{
		name: "cloudflare-key-account-and-gateway",
		env: { ...cloudflareKey, CLOUDFLARE_ACCOUNT_ID: "fixture-account", CLOUDFLARE_GATEWAY_ID: "fixture-gateway" },
		providers: CLOUDFLARE_PROVIDERS,
	},
	{ name: "cloudflare-key-stored", auth: { "cloudflare-workers-ai": storedKey }, providers: CLOUDFLARE_PROVIDERS },
];

type Launch = {
	name: string;
	args: string[];
	authenticated: string[];
	/** The agent directory's `auth.json`. */
	auth?: Json;
	settings?: Json;
	/** RPC commands after startup; `get_state` alone when absent. */
	commands?: Json[];
};

const state = { type: "get_state" };
const setModel = (provider: string, modelId: string) => ({ type: "set_model", provider, modelId });
const setThinking = (level: string) => ({ type: "set_thinking_level", level });

const launches: Launch[] = [
	{ name: "model", args: ["--model", "anthropic/claude-opus-4-8"], authenticated: ["anthropic"] },
	{ name: "model-without-credentials", args: ["--model", "anthropic/claude-opus-4-8"], authenticated: [] },
	{
		name: "thinking",
		args: ["--model", "anthropic/claude-opus-4-8", "--thinking", "max"],
		authenticated: ["anthropic"],
	},
	{
		name: "thinking-clamped-down",
		args: ["--model", "anthropic/claude-haiku-4-5", "--thinking", "xhigh"],
		authenticated: ["anthropic"],
	},
	{
		name: "thinking-clamped-up",
		args: ["--model", "anthropic/claude-opus-5", "--thinking", "off"],
		authenticated: ["anthropic"],
	},
	{
		name: "thinking-on-a-model-without-reasoning",
		args: ["--model", "openai/gpt-4.1", "--thinking", "high"],
		authenticated: ["openai"],
	},
	{
		name: "thinking-level-the-model-maps-away",
		args: ["--model", "openai/gpt-5.5", "--thinking", "minimal"],
		authenticated: ["openai"],
	},
	{
		name: "thinking-on-gemini",
		args: ["--model", "google/gemini-2.5-pro", "--thinking", "xhigh"],
		authenticated: ["google"],
	},
	{ name: "thinking-suffix", args: ["--model", "anthropic/claude-opus-4-8:low"], authenticated: ["anthropic"] },
	{
		name: "thinking-flag-beats-suffix",
		args: ["--model", "anthropic/claude-opus-4-8:low", "--thinking", "high"],
		authenticated: ["anthropic"],
	},
	{
		name: "thinking-invalid",
		args: ["--model", "anthropic/claude-haiku-4-5", "--thinking", "bogus"],
		authenticated: ["anthropic"],
	},
	{ name: "fuzzy-with-thinking", args: ["--model", "sonnet:high"], authenticated: ["anthropic"] },
	{ name: "provider-and-fuzzy-model", args: ["--provider", "anthropic", "--model", "opus"], authenticated: ["anthropic"] },
	{ name: "custom-model", args: ["--model", "anthropic/new-id"], authenticated: ["anthropic"] },
	{
		name: "custom-model-with-thinking",
		args: ["--model", "openai/gpt-4.1-custom", "--thinking", "high"],
		authenticated: ["openai"],
	},
	{
		name: "custom-model-made-to-reason",
		args: ["--model", "mistral/new-model", "--thinking", "high"],
		authenticated: [],
	},
	{ name: "custom-model-of-a-provider-without-reasoning", args: ["--model", "mistral/new-model"], authenticated: [] },
	{ name: "provider-without-model", args: ["--provider", "anthropic"], authenticated: ["anthropic"] },
	{ name: "unknown-provider", args: ["--provider", "nope", "--model", "x"], authenticated: ["anthropic"] },
	{ name: "unknown-model", args: ["--model", "nope/x"], authenticated: ["anthropic"] },
	{ name: "default-model", args: [], authenticated: ["anthropic"] },
	{ name: "default-model-of-the-first-provider", args: [], authenticated: ["openai", "anthropic"] },
	{
		name: "saved-default-model",
		args: [],
		authenticated: ["openai", "anthropic"],
		settings: { defaultProvider: "openai", defaultModel: "gpt-5.4" },
	},
	{
		name: "saved-default-without-credentials",
		args: [],
		authenticated: ["anthropic"],
		settings: { defaultProvider: "openai", defaultModel: "gpt-5.4" },
	},
	{
		name: "default-thinking-level-setting",
		args: [],
		authenticated: ["anthropic"],
		settings: { defaultThinkingLevel: "high" },
	},
	{
		name: "model-thinking-level-setting",
		args: [],
		authenticated: ["anthropic"],
		settings: { defaultThinkingLevel: "high", modelThinkingLevels: { "anthropic/claude-opus-4-8": "xhigh" } },
	},
	{
		name: "thinking-flag-beats-settings",
		args: ["--thinking", "low"],
		authenticated: ["anthropic"],
		settings: { defaultThinkingLevel: "high", modelThinkingLevels: { "anthropic/claude-opus-4-8": "xhigh" } },
	},
	{ name: "scope-flag", args: ["--models", "anthropic/claude-sonnet-*:low"], authenticated: ["anthropic"] },
	{
		name: "scope-flag-keeps-the-saved-default",
		args: ["--models", "anthropic/claude-sonnet-*:low"],
		authenticated: ["anthropic"],
		settings: { defaultProvider: "anthropic", defaultModel: "claude-sonnet-5" },
	},
	{
		name: "scope-setting",
		args: [],
		authenticated: ["anthropic"],
		settings: { enabledModels: ["anthropic/claude-haiku-*", "anthropic/claude-opus-4-8:high"] },
	},
	{
		name: "scope-flag-replaces-the-setting",
		args: ["--models", "claude-opus-4-7:max,nomatch"],
		authenticated: ["anthropic"],
		settings: { enabledModels: ["anthropic/claude-haiku-*"] },
	},
	{
		name: "scope-without-a-match",
		args: ["--models", "openai/*"],
		authenticated: ["anthropic"],
	},
	{
		name: "model-flag-beats-scope",
		args: ["--models", "anthropic/claude-sonnet-*:low", "--model", "anthropic/claude-opus-4-8"],
		authenticated: ["anthropic"],
	},
	{
		name: "set-thinking-level",
		args: ["--model", "anthropic/claude-haiku-4-5"],
		authenticated: ["anthropic"],
		commands: [state, setThinking("xhigh"), state, setThinking("high"), state, setThinking("off"), state],
	},
	{
		name: "set-thinking-level-clamped-up",
		args: ["--model", "anthropic/claude-opus-5"],
		authenticated: ["anthropic"],
		commands: [setThinking("off"), state],
	},
	{
		name: "set-model",
		args: ["--model", "anthropic/claude-opus-4-8", "--thinking", "max"],
		authenticated: ["anthropic", "openai"],
		commands: [
			state,
			setModel("anthropic", "claude-haiku-4-5"),
			state,
			setModel("anthropic", "claude-opus-4-8"),
			state,
			setModel("openai", "gpt-4.1"),
			state,
			setModel("openai", "gpt-5.5"),
			state,
		],
	},
	{
		name: "set-model-with-thinking-settings",
		args: ["--model", "anthropic/claude-opus-4-8", "--thinking", "max"],
		authenticated: ["anthropic"],
		settings: { defaultThinkingLevel: "low", modelThinkingLevels: { "anthropic/claude-sonnet-5": "xhigh" } },
		commands: [state, setModel("anthropic", "claude-sonnet-5"), state, setModel("anthropic", "claude-opus-4-7"), state],
	},
	{
		name: "set-model-not-found",
		args: ["--model", "anthropic/claude-opus-4-8"],
		authenticated: ["anthropic"],
		commands: [setModel("anthropic", "no-such-model"), setModel("openai", "gpt-5.5"), setModel("ANTHROPIC", "claude-opus-4-8"), state],
	},
	// A ChatGPT login is `openai-codex`'s credential and an API key is `openai`'s; neither serves the other.
	{
		name: "chatgpt-login",
		args: [],
		authenticated: [],
		auth: { "openai-codex": login },
		commands: [state, setModel("openai", "gpt-5.5"), state],
	},
	{ name: "chatgpt-login-beside-an-api-key", args: [], authenticated: ["openai"], auth: { "openai-codex": login } },
	{
		name: "api-key",
		args: [],
		authenticated: ["openai"],
		commands: [state, setModel("openai-codex", "gpt-6.1-sol"), state],
	},
	{
		name: "api-key-stored-for-openai-codex",
		args: [],
		authenticated: ["anthropic"],
		auth: { "openai-codex": storedKey },
		commands: [state, setModel("openai-codex", "gpt-6.1-sol"), state],
	},
];

function modelRef(model: Json | undefined) {
	return model ? { provider: model.provider, id: model.id } : null;
}

/** The fields of upstream's model object that pi's model definition has. */
function modelFields(model: Json | undefined) {
	if (!model) return null;
	const fields: Json = {};
	for (const key of [
		"id",
		"name",
		"api",
		"provider",
		"baseUrl",
		"reasoning",
		"thinkingLevelMap",
		"input",
		"cost",
		"contextWindow",
		"maxTokens",
	]) {
		if (model[key] !== undefined) fields[key] = model[key];
	}
	return fields;
}

/** Starts upstream pi in RPC mode, sends the commands one at a time, and records the answers. */
async function launch(testCase: Launch): Promise<Json> {
	const dir = await mkdtemp(join(tmpdir(), "pi-model-launch-"));
	for (const sub of ["cwd", "home", "agent"]) await mkdir(join(dir, sub), { recursive: true });
	if (testCase.settings) await writeFile(join(dir, "agent", "settings.json"), JSON.stringify(testCase.settings));
	if (testCase.auth) await writeFile(join(dir, "agent", "auth.json"), JSON.stringify(testCase.auth));
	const keys = Object.assign({}, ...testCase.authenticated.map((provider) => KEYS[provider]));
	const child = spawn("npx", ["tsx", CLI, "--mode", "rpc", "--no-session", ...testCase.args], {
		cwd: join(dir, "cwd"),
		env: {
			PATH: process.env.PATH,
			HOME: join(dir, "home"),
			// Reuse the npx cache that already holds tsx.
			npm_config_cache: process.env.npm_config_cache ?? join(homedir(), ".npm"),
			// The upstream tsconfig maps workspace packages to their sources.
			TSX_TSCONFIG_PATH: "/ts-reference/tsconfig.json",
			PI_CODING_AGENT_DIR: join(dir, "agent"),
			...PI_ENV,
			...keys,
		},
	});
	const records: Json[] = [];
	let buffer = "";
	let stderr = "";
	let wake = () => {};
	child.stdout.setEncoding("utf8");
	child.stdout.on("data", (chunk: string) => {
		buffer += chunk;
		for (let newline = buffer.indexOf("\n"); newline !== -1; newline = buffer.indexOf("\n")) {
			records.push(JSON.parse(buffer.slice(0, newline)));
			buffer = buffer.slice(newline + 1);
		}
		wake();
	});
	child.stderr.setEncoding("utf8");
	child.stderr.on("data", (chunk: string) => {
		stderr += chunk;
	});
	let exitCode: number | null | undefined;
	const exited = new Promise<void>((resolve) =>
		child.on("exit", (code) => {
			exitCode = code;
			wake();
			resolve();
		}),
	);
	const timer = setTimeout(() => child.kill("SIGKILL"), WAIT_MS);

	const exchanges: Json[] = [];
	let seen = 0;
	const commands = testCase.commands ?? [state];
	for (const [index, command] of commands.entries()) {
		const id = String(index + 1);
		child.stdin.write(`${JSON.stringify({ id, ...command })}\n`);
		let response: Json | undefined;
		while (!response && exitCode === undefined) {
			response = records.slice(seen).find((record) => record.type === "response" && record.id === id);
			if (!response) await new Promise<void>((resolve) => (wake = resolve));
		}
		if (!response) break;
		// Records before the answer are the events the command caused.
		const position = records.indexOf(response);
		const events = records.slice(seen, position);
		seen = position + 1;
		const answer: Json = { success: response.success };
		if (response.error !== undefined) answer.error = response.error;
		if (command.type === "get_state") {
			answer.model = modelFields(response.data.model);
			answer.thinkingLevel = response.data.thinkingLevel;
		} else if (command.type === "set_model" && response.success) {
			answer.model = modelRef(response.data);
		}
		exchanges.push({ command, events, response: answer });
	}
	child.stdin.end();
	await exited;
	clearTimeout(timer);
	await rm(dir, { recursive: true, force: true });
	const result: Json = {
		name: testCase.name,
		args: testCase.args,
		authenticated: testCase.authenticated,
		auth: testCase.auth ?? {},
		settings: testCase.settings ?? {},
		exitCode,
		stderr: stderr
			.replace(/\x1b\[[0-9;]*m/g, "")
			.split("\n")
			.filter((line) => line.length > 0),
		exchanges,
	};
	if (exitCode === 0 && exchanges.length !== commands.length) {
		throw new Error(`${testCase.name}: missing answers\n${JSON.stringify(result, null, 2)}`);
	}
	return result;
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: model-resolution-fixture.ts <output-dir>");
	}
	const runtime = await builtinModelRuntime();
	const models = [...runtime.getModels()];
	const available = (authenticated: string[]) => models.filter((model) => authenticated.includes(model.provider));
	/** Stands in for the model runtime: the built-in catalog, and credentials for `authenticated`. */
	const runtimeWith = (authenticated: string[]): any => ({
		getModels: () => models,
		getModel: (provider: string, id: string) => models.find((model) => model.provider === provider && model.id === id),
		hasConfiguredAuth: (provider: string) => authenticated.includes(provider),
		getAvailableSnapshot: () => available(authenticated),
	});

	const cliModel = cliModelCases.map((input) => {
		const authenticated = input.authenticated ?? [];
		const resolved = resolveCliModel({
			cliProvider: input.provider,
			cliModel: input.model,
			cliThinking: input.thinking as any,
			modelRuntime: runtimeWith(authenticated),
		});
		const expected: Json = {
			model: modelRef(resolved.model),
			thinkingLevel: resolved.thinkingLevel ?? null,
			warning: resolved.warning ?? null,
			error: resolved.error ?? null,
		};
		// A model the catalog does not hold was built from the provider's default.
		if (resolved.model && !models.includes(resolved.model)) expected.custom = modelFields(resolved.model);
		return { input: { ...input, authenticated }, expected };
	});

	const modelScope = modelScopeCases.map((input) => {
		const resolved = resolveModelScopeFromModels(input.patterns, available(input.authenticated));
		return {
			input,
			expected: {
				models: resolved.scopedModels.map((scoped) => ({
					...modelRef(scoped.model),
					thinkingLevel: scoped.thinkingLevel ?? null,
				})),
				diagnostics: resolved.diagnostics.map((diagnostic) => diagnostic.message),
			},
		};
	});

	const exactReference = exactReferenceCases.map((input) => {
		const custom = (input.custom ?? []).map((model) => ({ ...models[0], ...model, name: model.id }));
		const listed = [...available(input.authenticated), ...custom];
		return { input, expected: modelRef(findExactModelReferenceMatch(input.reference, listed)) };
	});

	const initialModel = [];
	for (const input of initialModelCases) {
		const resolved = await findInitialModel({
			scopedModels: [],
			isContinuing: false,
			defaultProvider: input.defaultProvider,
			defaultModelId: input.defaultModel,
			modelRuntime: runtimeWith(input.authenticated),
		});
		initialModel.push({ input, expected: modelRef(resolved.model) });
	}

	const credentials = [];
	for (const input of credentialCases) {
		const dir = await mkdtemp(join(tmpdir(), "pi-model-credentials-"));
		const authPath = join(dir, "auth.json");
		await writeFile(authPath, JSON.stringify(input.auth ?? {}));
		const env = input.env ?? {};
		const providers = input.providers ?? LOGIN_PROVIDERS;
		Object.assign(process.env, env);
		try {
			const real = await ModelRuntime.create({ modelsPath: null, authPath });
			// Fills the runtime's record of which providers have credentials.
			await real.getAvailable();
			const resolved = await findInitialModel({ scopedModels: [], isContinuing: false, modelRuntime: real });
			credentials.push({
				name: input.name,
				input: { auth: input.auth ?? {}, env, providers },
				expected: {
					configured: providers.filter((provider) => real.hasConfiguredAuth(provider)),
					initialModel: modelRef(resolved.model),
				},
			});
		} finally {
			for (const name of Object.keys(env)) delete process.env[name];
			await rm(dir, { recursive: true, force: true });
		}
	}

	const globs = globCases.map(([pattern, text]) => ({
		pattern,
		text,
		matches: minimatch(text, pattern, { nocase: true }),
	}));

	const launched = [];
	for (const testCase of launches) launched.push(await launch(testCase));

	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "model-resolution.json"),
		`${JSON.stringify(
			{
				source: fixtureSource(fileURLToPath(import.meta.url)),
				cliModel,
				modelScope,
				exactReference,
				initialModel,
				credentials,
				globs,
				launches: launched,
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

import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BACKGROUND_CONTEXT } from "/ts-reference/packages/chord/src/context/index.ts";
import { canonicalJson as chordCanonicalJson } from "/ts-reference/packages/chord/src/data/principal.ts";
import {
	applyImmutable,
	decoder,
	diffRevisions,
	encoder,
	type Op,
} from "/ts-reference/packages/chord/src/delta/index.ts";

/**
 * Goldens for noel-chord: upstream Chord's delta ops and their wire codec,
 * the immutable applier, diff results and canonical JSON.
 *
 * Chord is read from /ts-reference, upstream at TS_REFERENCE_REF, like every
 * other fixture. (Chord v1's sections, hash64 over UTF-16 and v1 contracts,
 * coverage and fits, went with Chord v1; chord-v2-contracts.json covers v2's
 * contracts.)
 */

const CHORD = "/ts-reference";

const canonicalJson = (value: unknown): string => chordCanonicalJson(value as never, BACKGROUND_CONTEXT);

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** A detached copy, as a value that crossed a wire. */
const json = <T>(value: T): T => JSON.parse(JSON.stringify(value)) as T;

const git = (...args: string[]): string =>
	execFileSync("git", ["-C", CHORD, ...args], { encoding: "utf8" }).trim();

function errorOf(error: unknown): { name: string; message: string } {
	if (!(error instanceof Error)) throw error;
	return { name: error.name, message: error.message };
}

function check(condition: boolean, what: string): void {
	if (!condition) throw new Error(`fixture invariant failed: ${what}`);
}

const same = (a: unknown, b: unknown): boolean => canonicalJson(a) === canonicalJson(b);

// ─── Codec ───────────────────────────────────────────────────────────────────

/** One state stream: one encoder, one decoder, every batch in order from `base`. */
function stream(name: string, base: Json, batches: Op[][]) {
	const enc = encoder();
	const dec = decoder();
	let value: Json = json(base);
	return {
		name,
		base,
		batches: batches.map((ops) => {
			const wire = json(enc.encode(ops));
			const decoded = json(dec.decode(wire));
			check(same(decoded, ops), `${name}: decoded ops equal the encoded ones`);
			value = applyImmutable(value, decoded);
			return { ops: json(ops), wire, decoded, value: json(value) };
		}),
	};
}

/** Successive revisions of a conversation view, diffed by upstream into a stream. */
function viewRevisions(): { base: Json; batches: Op[][]; last: Json } {
	const user = { role: "user", content: [{ type: "text", text: "Summarise notes.txt" }] };
	const reply = (text: string) => ({ role: "assistant", content: [{ type: "text", text }] });
	const view = (entries: Json[], live: Json): Json => ({
		conversation: { id: "c1", cwd: "/repo" },
		entries,
		docs: { "pi.agent": { model: "faux/echo", thinkingLevel: "medium" }, "pi.live": live },
	});
	const revisions: Json[] = [
		view([], {}),
		view([user], { run: { runId: "r1", requestId: "q1" } }),
		view([user], { run: { runId: "r1", requestId: "q1" }, generation: { attempt: 1, message: reply("The") } }),
		view([user], {
			run: { runId: "r1", requestId: "q1" },
			generation: { attempt: 1, message: reply("The notes say alpha") },
		}),
		view([user], {
			run: { runId: "r1", requestId: "q1" },
			generation: { attempt: 1, message: reply("The notes say alpha, then beta.") },
			tools: [{ callId: "t1", name: "read", status: "running", output: "alpha\n" }],
		}),
		view([user], {
			run: { runId: "r1", requestId: "q1" },
			generation: { attempt: 1, message: reply("The notes say alpha, then beta.") },
			tools: [{ callId: "t1", name: "read", status: "done", output: "alpha\nbeta\n" }],
		}),
		view([user, reply("The notes say alpha, then beta.")], {}),
	];
	const batches = revisions.slice(1).map((next, index) => diffRevisions(json(revisions[index]!), json(next)));
	return { base: revisions[0]!, batches, last: revisions[revisions.length - 1]! };
}

function codecStreams() {
	const items = Array.from({ length: 20 }, (_, index) => ({ id: index, text: `item ${index}` }));
	const view = viewRevisions();
	const streams = [
		stream("interning", {}, [
			[
				["s", ["a"], 1],
				["s", ["a"], 2],
				["s", ["b"], "x"],
				["s", ["a"], 3],
				["s", ["b"], "y"],
				["s", ["a"], 4],
			],
			[
				["s", ["a"], 5],
				["s", ["c"], 1],
				["s", ["c"], 2],
			],
			[["s", ["c"], 3]],
		]),
		stream("short forms for every verb", { text: "hello", list: [1, 2, 3], obj: { k: 1, j: 2 } }, [
			[
				["a", ["text"], " world"],
				["t", ["text"], 6],
				["a", ["text"], "!"],
				["s", ["text"], "x"],
				["d", ["obj", "k"]],
				["p", ["list"], 1, 1, [9, 8]],
				["p", ["list"], 0, 0, [0]],
				["m", ["list"], [4, 3, 2, 1, 0]],
				["d", ["list", 0]],
				["d", ["list", 0]],
				["d", ["obj", "j"]],
			],
			[
				["a", ["text"], "y"],
				["p", ["list"], 3, 0, ["z"]],
			],
		]),
		stream("replacement resets the dictionary", null, [
			[
				["r", { a: { b: 1 } }],
				["s", ["a", "b"], 2],
				["s", ["a", "c"], 3],
				["s", ["a", "b"], 4],
			],
			[["s", ["a", "b"], 5]],
			[
				["s", ["a", "b"], 6],
				["r", { a: { b: 0 } }],
				["s", ["a", "b"], 8],
				["s", ["a", "c"], 9],
				["s", ["a", "b"], 10],
			],
			[["s", ["a", "b"], 11]],
		]),
		stream("root array", [], [
			[
				["p", [], 0, 0, [1, 2, 3]],
				["p", [], 3, 0, [4]],
				["m", [], [3, 2, 1, 0]],
				["p", [], 0, 1, []],
			],
			[
				["p", [], 0, 0, ["z"]],
				["s", [0], "y"],
			],
		]),
		stream("UTF-16 counts", { msg: "", é: { "😀": "x" } }, [
			[
				["a", ["msg"], "héllo 😀"],
				["a", ["msg"], " 𝄞"],
				["t", ["msg"], 3],
				["t", ["msg"], 5],
				["s", ["é", "😀"], "y"],
				["s", ["é", "😀"], "z"],
			],
			[
				["a", ["msg"], "ü"],
				["s", ["é", "😀"], "w"],
			],
		]),
		stream("numeric and string segments are distinct paths", { o: {} }, [
			[
				["s", ["o", 0], "zero"],
				["s", ["o", "0"], "string"],
				["s", ["o", 0], "again"],
			],
		]),
		stream("splices", { items: [] }, [
			[["p", ["items"], 0, 0, items]],
			[["p", ["items"], 10, 25, [{ id: "new" }]]],
			[["p", ["items"], 1000, 3, ["clamped"]]],
			[["m", ["items"], [11, ...Array.from({ length: 11 }, (_, index) => index)]]],
		]),
		stream("diffed view revisions", view.base, view.batches),
	];
	const last = streams[streams.length - 1]!;
	check(same(last.batches[last.batches.length - 1]!.value, view.last), "view revisions replay to the last view");
	return streams;
}

/** Wire batches through one decoder until one throws. */
function decodeCase(name: string, batches: unknown[][]) {
	const dec = decoder();
	const out: Record<string, unknown>[] = [];
	for (const wire of batches) {
		try {
			out.push({ wire, decoded: json(dec.decode(wire)) });
		} catch (error) {
			out.push({ wire, error: errorOf(error) });
			return { name, batches: out };
		}
	}
	throw new Error(`decode case ${name} did not fail`);
}

function decodeErrors() {
	return [
		decodeCase("short form first in a batch", [[["s", 1]]]),
		decodeCase("short forms do not span batches", [[["s", ["a"], 1]], [["s", 2]]]),
		decodeCase("short form after a replacement", [[["s", ["a"], 1], ["r", {}], ["s", 2]]]),
		decodeCase("unknown id", [[["s", 3, 1]]]),
		decodeCase("id cleared by a replacement", [
			[
				["#", 0, ["a"]],
				["s", 0, 1],
			],
			[
				["r", {}],
				["s", 0, 2],
			],
		]),
		decodeCase("empty path for a set", [[["s", [], 1]]]),
		decodeCase("empty path through an id", [[["#", 0, []], ["d", 0]]]),
		decodeCase("reserved segment", [[["s", ["__proto__", "x"], 1]]]),
		decodeCase("reserved segment in a definition", [[["#", 0, ["constructor"]]]]),
		decodeCase("negative segment", [[["s", ["a", -1], 1]]]),
		decodeCase("fractional segment", [[["s", ["a", 1.5], 1]]]),
		decodeCase("null segment", [[["s", ["a", null], 1]]]),
		decodeCase("boolean segment", [[["s", [true], 1]]]),
		decodeCase("string as a path", [[["s", "a", 1]]]),
		decodeCase("negative id", [[["s", -1, 1]]]),
		decodeCase("fractional id", [[["d", 0.5]]]),
		decodeCase("unknown verb", [[["x", ["a"]]]]),
		decodeCase("numeric verb", [[[7]]]),
		decodeCase("object as an op", [[{}]]),
		decodeCase("empty op", [[[]]]),
		decodeCase("r arity", [[["r"]]]),
		decodeCase("s arity", [[["s"]]]),
		decodeCase("d arity", [[["d", ["a"], 1]]]),
		decodeCase("a arity", [[["a"]]]),
		decodeCase("a value, short", [[["s", ["x"], ""], ["a", 1]]]),
		decodeCase("a value, full", [[["a", ["x"], 1]]]),
		decodeCase("t arity", [[["t"]]]),
		decodeCase("t count negative", [[["t", ["x"], -1]]]),
		decodeCase("t count fractional", [[["s", ["x"], ""], ["t", 1.5]]]),
		decodeCase("p arity", [[["p"]]]),
		decodeCase("p short form read as a full one", [[["p", ["x"], 0, 0]]]),
		decodeCase("p remove", [[["p", ["x"], 0, -2, []]]]),
		decodeCase("p items", [[["p", ["x"], 0, 0, "x"]]]),
		decodeCase("m arity", [[["m"]]]),
		decodeCase("m permutation not an array", [[["m", ["x"], "y"]]]),
		decodeCase("m permutation repeats", [[["m", ["x"], [1, 1]]]]),
		decodeCase("m permutation out of range", [[["m", [], [0, 2]]]]),
		decodeCase("# shape", [[["#", 0, "x"]]]),
		decodeCase("# negative id", [[["#", -1, []]]]),
	];
}

// ─── applyImmutable ──────────────────────────────────────────────────────────

function applyCase(name: string, base: Json, ops: unknown[]) {
	const input = json(base);
	try {
		const result = applyImmutable(input, ops as Op[]);
		check(same(input, base), `${name}: the base is unchanged`);
		return { name, base, ops, result: json(result) };
	} catch (error) {
		check(same(input, base), `${name}: the base is unchanged`);
		return { name, base, ops, error: errorOf(error) };
	}
}

function applyCases() {
	const doc: Json = { o: { k: "v", n: 5 }, arr: [{ x: 1 }, "s", 3], text: "a😀b", n: 5 };
	return [
		applyCase("set a key", doc, [["s", ["o", "k"], "w"]]),
		applyCase("add a key", doc, [["s", ["o", "new"], { deep: [1] }]]),
		applyCase("set an element", doc, [["s", ["arr", 1], "t"]]),
		applyCase("append an element by index", doc, [["s", ["arr", 3], "end"]]),
		applyCase("set past the end", doc, [["s", ["arr", 5], "far"]]),
		applyCase("numeric segment on an object", doc, [["s", ["o", 0], "zero"]]),
		applyCase("key on an array", doc, [["s", ["arr", "x"], 1]]),
		applyCase("walk an array by a missing key", doc, [["s", ["arr", "x", "y"], 1]]),
		applyCase("walk an array by length", doc, [["s", ["arr", "length", "y"], 1]]),
		applyCase("walk an array by an index string", doc, [["s", ["arr", "0", "y"], 1]]),
		applyCase("missing parent", doc, [["s", ["missing", "x"], 1]]),
		applyCase("primitive parent", doc, [["s", ["n", "x"], 1]]),
		applyCase("primitive root", 5, [["s", ["a"], 1]]),
		applyCase("delete a key", doc, [["d", ["o", "k"]]]),
		applyCase("delete a missing key", doc, [["d", ["o", "missing"]]]),
		applyCase("delete an element", doc, [["d", ["arr", 0]]]),
		applyCase("delete at the end", doc, [["d", ["arr", 3]]]),
		applyCase("delete past the end", doc, [["d", ["arr", 9]]]),
		applyCase("append text", doc, [["a", ["o", "k"], "+more 😀"]]),
		applyCase("append to an element", doc, [["a", ["arr", 1], "!"]]),
		applyCase("append to a number", doc, [["a", ["n"], "x"]]),
		applyCase("append to a missing key", doc, [["a", ["o", "missing"], "x"]]),
		applyCase("trim an astral character", doc, [["t", ["text"], 3]]),
		applyCase("trim nothing", doc, [["t", ["text"], 0]]),
		applyCase("trim past the end", doc, [["t", ["text"], 99]]),
		applyCase("trim a number", doc, [["t", ["n"], 1]]),
		applyCase("splice the middle", doc, [["p", ["arr"], 1, 1, ["a", "b"]]]),
		applyCase("splice past the end", doc, [["p", ["arr"], 10, 4, ["tail"]]]),
		applyCase("splice removes past the end", doc, [["p", ["arr"], 1, 10, []]]),
		applyCase("splice the root", [1, 2, 3], [["p", [], 0, 2, [9]]]),
		applyCase("splice an object", doc, [["p", ["o"], 0, 0, [1]]]),
		applyCase("splice a missing array", doc, [["p", ["missing"], 0, 0, [1]]]),
		applyCase("permute", doc, [["m", ["arr"], [2, 0, 1]]]),
		applyCase("permute the root", [1, 2, 3], [["m", [], [1, 2, 0]]]),
		applyCase("permute a different length", doc, [["m", ["arr"], [1, 0]]]),
		applyCase("permute an object", doc, [["m", ["o"], []]]),
		applyCase("replace, then set", doc, [
			["r", { fresh: true }],
			["s", ["more"], 1],
		]),
		applyCase("replace with a primitive", doc, [["r", "flat"]]),
		applyCase("several ops", doc, [
			["s", ["o", "k"], "1"],
			["a", ["o", "k"], "2"],
			["d", ["arr", 2]],
			["p", ["arr"], 0, 0, [0]],
			["t", ["o", "k"], 1],
		]),
		applyCase("an error after earlier ops", doc, [
			["s", ["o", "k"], "1"],
			["d", ["arr", 9]],
			["x"],
		]),
		applyCase("empty path for a set", doc, [["s", [], 1]]),
		applyCase("s arity", doc, [["s", ["a"]]]),
		applyCase("a shape", doc, [["a", ["o", "k"], 1]]),
		applyCase("t shape", doc, [["t", ["text"], 1.5]]),
		applyCase("p index", doc, [["p", ["arr"], -1, 0, []]]),
		applyCase("p items", doc, [["p", ["arr"], 0, 0, "x"]]),
		applyCase("m not a bijection", [1, 2], [["m", [], [0, 0]]]),
		applyCase("path not an array", doc, [["d", "o"]]),
		applyCase("reserved segment", doc, [["s", ["o", "prototype"], 1]]),
		applyCase("unknown verb", doc, [["#", 0, ["o"]]]),
		applyCase("not a tuple", doc, ["s"]),
	];
}

// ─── diffRevisions ───────────────────────────────────────────────────────────

/** diffRevisions applied to `before`; only the result is contractual. */
function diffCase(name: string, before: Json, after: Json) {
	const ops = diffRevisions(json(before), json(after));
	const result = applyImmutable(json(before), ops);
	check(same(result, after), `${name}: the diff produces the new value`);
	const viaWire = applyImmutable(json(before), decoder().decode(json(encoder().encode(ops))));
	check(same(viaWire, after), `${name}: the diff survives the wire`);
	return { name, before, after, result: json(result) };
}

function diffCases() {
	const rows = (n: number, label: (index: number) => string) =>
		Array.from({ length: n }, (_, index) => ({ id: index, label: label(index) }));
	return [
		diffCase("equal", { a: [1, { b: 2 }] }, { a: [1, { b: 2 }] }),
		diffCase("root number", 1, 2),
		diffCase("root string", "a", "b"),
		diffCase("root type change", null, { a: 1 }),
		diffCase("object to array", {}, []),
		diffCase("string append", { t: "hello" }, { t: "hello world" }),
		diffCase("string rolling window", { t: "abcdef" }, { t: "defghi" }),
		diffCase("string replace", { t: "abc" }, { t: "xyz" }),
		diffCase("string shrinks to a prefix", { t: "hello world" }, { t: "hello" }),
		diffCase("astral rolling window", { t: "a😀b" }, { t: "😀bc" }),
		diffCase("astral append", { t: "𝄞" }, { t: "𝄞😀" }),
		diffCase("nested objects", { a: { b: { c: 1, d: 2 }, e: 3 } }, { a: { b: { c: 1, f: 4 }, g: 5 } }),
		diffCase("number to float", { n: 1 }, { n: 1.5 }),
		diffCase("array append", { xs: [1, 2] }, { xs: [1, 2, 3, 4] }),
		diffCase("array prepend", { xs: [1, 2] }, { xs: [0, 1, 2] }),
		diffCase("array insert", { xs: ["a", "c"] }, { xs: ["a", "b", "c"] }),
		diffCase("array delete", { xs: ["a", "b", "c"] }, { xs: ["a", "c"] }),
		diffCase("array emptied", { xs: [1, 2, 3] }, { xs: [] }),
		diffCase("array replaced", { xs: [1, 2, 3] }, { xs: ["x", "y"] }),
		diffCase("primitive permutation", [1, 2, 3, 4], [4, 3, 2, 1]),
		diffCase("permutation with repeats", [1, 1, 2], [2, 1, 1]),
		diffCase("object rotation", rows(3, (i) => `r${i}`), [...rows(3, (i) => `r${i}`).slice(2), ...rows(2, (i) => `r${i}`)]),
		diffCase(
			"one element edited",
			rows(5, (i) => `row ${i}`),
			rows(5, (i) => (i === 3 ? "row 3, edited" : `row ${i}`)),
		),
		diffCase("elements edited and inserted", rows(4, (i) => `${i}`), [
			{ id: 0, label: "0" },
			{ id: 9, label: "new" },
			{ id: 1, label: "1!" },
			{ id: 2, label: "2" },
			{ id: 3, label: "3" },
			{ id: 4, label: "4" },
		]),
		diffCase("primitive subsequence", [1, 2, 3, 4, 5, 6], [2, 4, 6]),
		diffCase("primitive anchors", ["a", "x", "b", "y", "c"], ["c", "a", "z", "b"]),
		diffCase("mixed array", [1, { a: 1 }, "s", [2]], ["s", { a: 2 }, 1, [2, 3]]),
		diffCase(
			"reserved key folds into its object",
			JSON.parse('{"o":{"__proto__":1,"a":1}}') as Json,
			JSON.parse('{"o":{"__proto__":1,"a":2}}') as Json,
		),
		diffCase("reserved key at the root", JSON.parse('{"constructor":1}') as Json, JSON.parse('{"constructor":2}') as Json),
		diffCase(
			"view after a reply",
			{ entries: [{ role: "user", text: "hi" }], live: { text: "Hel" } },
			{ entries: [{ role: "user", text: "hi" }, { role: "assistant", text: "Hello" }], live: {} },
		),
	];
}

// ─── canonicalJson ───────────────────────────────────────────────────────────

function canonicalCases() {
	const texts: [string, string][] = [
		["null", "null"],
		["true", "true"],
		["false", "false"],
		["zero", "0"],
		["negative zero", "-0"],
		["integral float", "1.0"],
		["exponent", "1e2"],
		["negative", "-1.5"],
		["tenth", "0.1"],
		["large integer", "123456789012"],
		["u64 max", "18446744073709551615"],
		["i64 min", "-9223372036854775808"],
		["above 2^53", "9007199254740993"],
		["small and large", "[0.5,100,1e21,1e-7,2.5e-5,1e-6,1e20,-0]"],
		["plain string", '"plain"'],
		["escapes", String.raw`"quote \" backslash \\ slash \/ tab \t newline \n"`],
		["control characters", String.raw`"\u0000\u0001\b\t\n\u000b\f\r\u001f\u007f"`],
		["line separators", String.raw`"  "`],
		["non-ASCII", '"é ü ñ 中文 ﬁ"'],
		["astral", '"😀 𝄞"'],
		["escaped surrogate pair", String.raw`"😀"`],
		["empty containers", "[[],{},[{}]]"],
		["nested object", '{"b":1,"a":{"d":[3,{"z":0,"y":1}],"c":null},"":true}'],
		["keys sort by UTF-16", String.raw`{"😀":1,"｡":2,"a":3,"Z":4,"10":5,"9":6,"":7,"é":8}`],
		["contract-like description", '{"name":"noel.chat","version":2,"methods":{"submit":{"args":{"type":"object","additionalProperties":false,"properties":{"text":{"type":"string"}}},"result":{"type":"null"}}},"state":{"anyOf":[{"type":"null"},{"type":"object"}]}}'],
	];
	return texts.map(([name, text]) => {
		const canonical = canonicalJson(JSON.parse(text));
		return { name, json: text, canonical };
	});
}

function numberCases() {
	const literals = [
		"0",
		"-0",
		"1",
		"-1",
		"0.1",
		"0.5",
		"1.5",
		"100",
		"123.456",
		"12345678.9",
		"1e21",
		"1e+21",
		"1E21",
		"1e20",
		"999999999999999900000",
		"123456789012345680000",
		"1e-5",
		"1e-6",
		"1e-7",
		"1.5e-7",
		"2.5e-5",
		"0.000001",
		"5e-324",
		"2.2250738585072014e-308",
		"1.7976931348623157e308",
		"1e300",
		"-1e-300",
		"0.30000000000000004",
		"33.333333333333336",
		"1.0000000000000002",
		"9007199254740991",
		"9007199254740993",
		"18446744073709551616",
		"6.02214076e23",
		"4.35",
	];
	return literals.map((literal) => {
		const canonical = canonicalJson(JSON.parse(literal));
		return { literal, canonical };
	});
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: chord-delta-fixture.ts <output-dir>");
	}
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "chord-delta.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					package: "packages/chord",
					packageCommit: git("log", "-1", "--format=%H", "HEAD", "--", "packages/chord"),
					script: fileURLToPath(import.meta.url),
				},
				format: {
					codec:
						"Each stream runs one encoder() and one decoder() over its batches in order, from base. Per batch: the ops, the exact wire tuples (after a JSON round trip), the decoded ops, and the value after applyImmutable.",
					decodeErrors:
						"Wire batches through one decoder(), in order; the last one throws. Errors are the class name and message.",
					apply: "applyImmutable(base, ops) on decoded tuples: the result, or the error's class name and message.",
					diff: "diffRevisions(before, after) applied to before. Batches are exact but not canonical, so only the result is recorded.",
					canonicalJson: "canonicalJson(JSON.parse(json)).",
					numbers: "canonicalJson(JSON.parse(literal)). Parse the literal as a correctly rounded double.",
				},
				codec: codecStreams(),
				decodeErrors: decodeErrors(),
				apply: applyCases(),
				diff: diffCases(),
				canonicalJson: canonicalCases(),
				numbers: numberCases(),
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

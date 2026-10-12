import { spawn } from "node:child_process";
import fs from "node:fs";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { DefaultResourceLoader } from "/ts-reference/packages/coding-agent/src/core/resource-loader.ts";
import { parseCommandArgs, substituteArgs } from "/ts-reference/packages/coding-agent/src/core/prompt-templates.ts";
import { createAgentSession } from "/ts-reference/packages/coding-agent/src/core/sdk.ts";
import { SessionManager } from "/ts-reference/packages/coding-agent/src/core/session-manager.ts";
import { SettingsManager } from "/ts-reference/packages/coding-agent/src/core/settings-manager.ts";
import { formatSkillsForPrompt, type Skill } from "/ts-reference/packages/coding-agent/src/core/skills.ts";
import { parseFrontmatter } from "/ts-reference/packages/coding-agent/src/utils/frontmatter.ts";

/**
 * Records how upstream loads and uses skills and prompt templates.
 *
 * `frontmatter`: what upstream's frontmatter parser reads from a file.
 * `discovery`: what upstream's resource loader loads from a tree of skill and
 * prompt directories, settings, and command-line paths: the skills and
 * templates in order, with their diagnostics.
 * `listing`: the skills text upstream's prompt builder adds to the system prompt.
 * `expansion`: what a session queues for `/skill:name` and `/template` input.
 * `templateArguments`: how upstream splits and substitutes template arguments.
 * `commands`: `get_commands` of upstream's RPC mode for command-line flags.
 *
 * Upstream loads a directory's entries in the order the filesystem lists
 * them. This script lists them by name, so the goldens do not depend on the
 * filesystem. Volatile values are placeholders; see PLACEHOLDERS.
 */

const listDirectory = fs.readdirSync;
fs.readdirSync = ((path: fs.PathLike, options?: unknown) => {
	const entries = (listDirectory as (path: fs.PathLike, options?: unknown) => Array<string | fs.Dirent>)(path, options);
	const name = (entry: string | fs.Dirent) => (typeof entry === "string" ? entry : entry.name);
	return entries.sort((a, b) => (name(a) < name(b) ? -1 : name(a) > name(b) ? 1 : 0));
}) as typeof fs.readdirSync;
syncBuiltinESMExports();

const PLACEHOLDERS = {
	"<tmp>": "The case's temporary directory. The agent dir is <tmp>/agent and the home directory <tmp>/home.",
	"<bom>": "A UTF-8 byte order mark (U+FEFF) in a file to write.",
	"<yaml-error>": "The message of upstream's YAML parser for frontmatter it rejects.",
};

type Json = Record<string, any>;

const CLI = "/ts-reference/packages/coding-agent/src/cli.ts";

/** A SKILL.md with `fields` as its frontmatter lines. */
function skill(fields: string[], body = "Instructions.\n"): string {
	return `---\n${fields.join("\n")}\n---\n${body}`;
}

const described = (description: string, body?: string) => skill([`description: ${description}`], body);

// Frontmatter.

const FRONTMATTER_CASES: Record<string, string> = {
	"plain-scalars": "---\nname: pdf-tools\ndescription: Extract text and tables from PDF files.\n---\n# PDF tools\n",
	"no-frontmatter": "Just a body.\n",
	"empty-frontmatter": "---\n---\nbody",
	"comments-and-blank-lines": "---\n# a comment\n\nname: x # trailing comment\n\ndescription: d\n---\nbody",
	"unclosed-frontmatter": "---\nname: x\nbody",
	"byte-order-mark-and-crlf": "<bom>---\r\nname: x\r\ndescription: two words\r\n---\r\nbody\r\nmore\r\n",
	"body-is-trimmed": "---\nname: x\n---\n\n\n  body with space around  \n\n",
	"body-keeps-later-rules": "---\nname: x\n---\nbody\n---\nmore\n",
	"body-without-frontmatter-is-not-trimmed": "\n  spaced body  \n\n",
	"closing-line-with-text": "---\nname: x\n---more\nbody",
	"four-dashes-open": "----\nname: x\n---\nbody",
	"double-quoted": '---\ndescription: "a: b \\"q\\" \\n x \\u00e9 \\t \\\\ end"\n---\nbody',
	"single-quoted": "---\ndescription: 'it''s: fine # not a comment'\n---\nbody",
	"quoted-then-comment": '---\nname: "x" # comment\n---\nbody',
	"empty-quoted": "---\nname: \"\"\ndescription: ''\n---\nbody",
	"quoted-over-lines": '---\ndescription: "line one\n  line two\n\n  after a blank"\n---\nbody',
	"quoted-keys": "---\n\"name\": x\n'description': d\n---\nbody",
	"double-quoted-escapes": '---\ndescription: "a\\x41\\/ \\0 \\e \\_ \\N \\L \\P \\U00010348 \\a\\b\\v\\f\\r"\n---\nbody',
	"double-quoted-line-continuation": '---\ndescription: "a\\\n  b"\n---\nbody',
	booleans:
		'---\na: true\nb: True\nc: TRUE\nd: false\ne: False\nf: FALSE\ng: yes\nh: on\ni: "true"\nj: \'false\'\nk: true # comment\n---\nbody',
	nulls: "---\na:\nb: ~\nc: null\nd: Null\ne: NULL\nf: nul\ng: # only a comment\n---\nbody",
	numbers:
		"---\na: 123\nb: 1.5\nc: 0x1f\nd: 0o17\ne: 1e3\nf: .5\ng: +1\nh: -1\ni: 1_000\nj: 1.2.3\nk: .inf\nl: .NaN\nm: 0b11\nn: 12abc\no: 5.\np: -.inf\nq: 1e\nr: 12:30\ns: 2024-01-01\n---\nbody",
	"plain-with-punctuation":
		"---\na: a:b c\nb: text # comment\nc: a#b\nd: say \"hi\" and 'bye'\ne: a, b {c} [d]\nf: see https://example.com/a?b=c#frag\ng: C:\\Users\\x\nh: -x\ni: ?x\nj: :x\nk: <file> [focus]\n---\nbody",
	"plain-over-lines": "---\ndescription: first line\n  continues here\n  and here\nname: x\n---\nbody",
	"plain-over-lines-with-blank": "---\ndescription: first\n\n  second paragraph\nname: x\n---\nbody",
	"value-on-next-line": '---\ndescription:\n  on the next line\nname:\n  "quoted next"\n---\nbody',
	"trailing-space": "---\nname: x   \ndescription:   spaced   \n---\nbody",
	"tab-after-colon": "---\nname:\tx\ndescription: a\tb\n---\nbody",
	"indented-keys": "---\n  name: x\n  description: d\n---\nbody",
	unicode: "---\ndescription: caf\u00e9 \u2014 na\u00efve \ud800\udf48\n---\nbody",
	"literal-block": "---\ndescription: |\n  line one\n  line two\n\n  after blank\nname: x\n---\nbody",
	"folded-block":
		"---\ndescription: >\n  folded one\n  folded two\n\n  paragraph two\n    indented more\n  back\nname: x\n---\nbody",
	"block-strip": "---\ndescription: |-\n  strip\n\nname: x\n---\nbody",
	"block-keep": "---\ndescription: |+\n  keep\n\nname: x\n---\nbody",
	"folded-strip": "---\ndescription: >-\n  a\n  b\n---\nbody",
	"block-indentation-indicator": "---\ndescription: |2\n   three\n  two\n---\nbody",
	"block-with-comment": "---\ndescription: | # comment\n  text\n---\nbody",
	"block-empty": "---\ndescription: |\nname: x\n---\nbody",
	"block-trailing-blank-lines": "---\ndescription: |\n  a\n\n\n---\nbody",
	"folded-leading-blank-line": "---\ndescription: >\n\n  a\n  b\n---\nbody",
	"block-with-tabs": "---\ndescription: |\n  a\tb\n  \tc\n---\nbody",
	"block-last-key": "---\nname: x\ndescription: >\n  last key\n  of the file\n---\nbody",
	"nested-mapping": '---\nname: x\nmetadata:\n  author: me\n  version: "1.0"\ndescription: d\n---\nbody',
	"nested-deep":
		"---\nmetadata:\n  a:\n    b:\n      - c\n      - d: e\n  f: g\ndescription: d\n---\nbody",
	"sequence-value": "---\nallowed-tools:\n  - Bash\n  - Read\ndescription: d\n---\nbody",
	"sequence-at-key-indent": "---\nallowed-tools:\n- Bash\n- Read\ndescription: d\n---\nbody",
	"flow-sequence": "---\nallowed-tools: [Bash, Read]\nargument-hint: [focus]\ndescription: d\n---\nbody",
	"flow-mapping": "---\nmetadata: {a: 1, b: [c, d]}\ndescription: d\n---\nbody",
	"flow-over-lines": "---\nallowed-tools: [\n  Bash,\n  Read\n]\ndescription: d\n---\nbody",
	"skill-fields":
		'---\nname: review\ndescription: Review code.\nlicense: MIT\ncompatibility: Needs git\ndisable-model-invocation: true\nargument-hint: "[focus]"\n---\nbody',
	"document-end-marker": "---\nname: x\n...\n---\nbody",
	"scalar-document": "---\njust text\n---\nbody",
	"sequence-document": "---\n- a\n- b\n---\nbody",
	"key-without-space": "---\nname:x\n---\nbody",
	"colon-in-plain": "---\ndescription: Use when: reading\n---\nbody",
	"colon-ends-plain": "---\ndescription: ends with colon:\n---\nbody",
	"colon-in-continuation": "---\ndescription: first\n  second: third\n---\nbody",
	"comment-in-continuation": "---\ndescription: text\n  # a comment\n  more\n---\nbody",
	"duplicate-keys": "---\nname: a\nname: b\n---\nbody",
	"tab-indentation": "---\nmetadata:\n\tauthor: me\n---\nbody",
	"line-without-key": "---\nname: x\njust text\n---\nbody",
	"unclosed-quote": '---\nname: "x\n---\nbody',
	"text-after-quote": '---\nname: "x" y\n---\nbody',
	"bad-escape": '---\ndescription: "a\\q"\n---\nbody',
	"reserved-at": "---\nname: @x\n---\nbody",
	"reserved-backtick": "---\nname: `x`\n---\nbody",
	"sequence-entry-as-value": "---\nname: - x\n---\nbody",
	"block-header-with-text": "---\nname: | x\n---\nbody",
	"block-less-indented-line": "---\ndescription: |\n    a\n  b\n---\nbody",
	"content-after-document-end": "---\nname: x\n...\nbody\n---\nrest",
};

function field(key: string, value: unknown): Json {
	if (typeof value === "string") return { key, type: "string", value };
	if (typeof value === "boolean") return { key, type: "boolean", value };
	return { key, type: "other" };
}

function captureFrontmatter(name: string, content: string): Json {
	const base = { name, content };
	try {
		const { frontmatter, body } = parseFrontmatter(content.replaceAll("<bom>", "\uFEFF"));
		const isMapping = typeof frontmatter === "object" && frontmatter !== null && !Array.isArray(frontmatter);
		return {
			...base,
			error: false,
			// null: the frontmatter is not a mapping, so it has no fields.
			fields: isMapping ? Object.entries(frontmatter).map(([key, value]) => field(key, value)) : null,
			body,
		};
	} catch {
		return { ...base, error: true };
	}
}

// Discovery.

type Flags = { skill?: string[]; noSkills?: boolean; promptTemplate?: string[]; noPromptTemplates?: boolean };

type DiscoveryCase = {
	name: string;
	description: string;
	/** Files to write, by path below the temporary directory. A content may use the `<tmp>` and `<bom>` placeholders. */
	files: Record<string, string>;
	/** Symbolic links to create: path below the temporary directory, and its target below it. */
	symlinks?: Record<string, string>;
	/** Working directory below the temporary directory. */
	cwd: string;
	trusted: boolean;
	/** Command-line flags; `<tmp>` in a path is the temporary directory. */
	flags?: Flags;
};

const REVIEW_PROMPT = '---\ndescription: Review staged changes\nargument-hint: "[focus]"\n---\nReview the staged changes. Focus on ${1:-correctness}.\n';

const DEFAULT_TREE = {
	"agent/skills/alpha/SKILL.md": skill(["name: alpha", "description: The alpha skill."]),
	"agent/skills/alpha/extra.md": described("Not a skill, because its directory already has SKILL.md."),
	"agent/skills/alpha/references/guide.md": described("A reference, not a skill."),
	"agent/skills/alpha/scripts/run.sh": "#!/bin/sh\n",
	"agent/skills/nested/deep/beta/SKILL.md": described("Found below two directories. Named after its directory."),
	"agent/skills/group/readme.md": described("Not a skill, because it is neither SKILL.md nor in the skills directory itself."),
	"agent/skills/root-note.md": skill(["name: root-note", "description: A Markdown file in the skills directory itself."]),
	"agent/skills/unnamed-note.md": described("Without a name, a file is called after its directory."),
	"agent/skills/no-frontmatter.md": "Plain notes.\n",
	"agent/skills/readme.txt": "Not Markdown.\n",
	"agent/skills/.hidden/SKILL.md": described("In a hidden directory."),
	"agent/skills/node_modules/dep/SKILL.md": described("In node_modules."),
	"agent/prompts/review.md": REVIEW_PROMPT,
	"agent/prompts/plain.md":
		"\nSummarize the conversation so far in a single paragraph, please, and keep it short.\n\nSecond paragraph.\n",
	"agent/prompts/short.md": "Explain $@\n",
	"agent/prompts/notes.txt": "Not Markdown.\n",
	"agent/prompts/sub/nested.md": "Not a direct child.\n",
	"agent/prompts/.hidden.md": "Hidden.\n",
	"repo/.git/HEAD": "ref: refs/heads/main\n",
	"repo/.pi/skills/gamma/SKILL.md": skill(["name: gamma", "description: A project skill."]),
	"repo/.pi/skills/project-note.md": skill(["name: project-note", "description: A project file skill."]),
	"repo/.pi/prompts/ship.md": "---\ndescription: Ship it\n---\nShip $1.\n",
	"repo/.agents/skills/delta/SKILL.md": described("A project skill in .agents."),
	"home/.agents/skills/epsilon/SKILL.md": described("A user skill in ~/.agents."),
};

const EXPLICIT_TREE = {
	"agent/skills/alpha/SKILL.md": skill(["name: alpha", "description: The alpha skill."]),
	"agent/prompts/review.md": REVIEW_PROMPT,
	"agent/settings.json": JSON.stringify({ skills: ["extra-skills"], prompts: ["extra-prompts"] }),
	"agent/extra-skills/from-settings/SKILL.md": described("From the skills setting."),
	"agent/extra-prompts/from-settings.md": "From the prompts setting.\n",
	"cli/one/SKILL.md": skill(["name: one", "description: Given as a SKILL.md file."]),
	"cli/two/SKILL.md": described("Given as its directory."),
	"cli/two/scripts/run.sh": "#!/bin/sh\n",
	"cli/many/a/SKILL.md": described("First of a directory of skills."),
	"cli/many/b/SKILL.md": described("Second of a directory of skills."),
	"cli/many/b/references/guide.md": described("A reference."),
	"cli/many/note.md": skill(["name: note", "description: A Markdown file in the given directory."]),
	"cli/many/plain.md": "No description.\n",
	"cli/plain.md": "No description.\n",
	"cli/tool.sh": "#!/bin/sh\n",
	"cli/declared/SKILL.md": "No description either.\n",
	"cli/prompts/explain.md": "---\ndescription: Explain something\n---\nExplain $@.\n",
	"cli/prompt-dir/a.md": "Prompt a.\n",
	"cli/prompt-dir/b.md": "Prompt b.\n",
	"cli/prompt-dir/c.txt": "Not Markdown.\n",
	"cli/prompt-dir/sub/d.md": "Not a direct child.\n",
	"repo/relative/rel/SKILL.md": described("Given relative to the working directory."),
	"home/home-skill/SKILL.md": described("Given below the home directory."),
};

const EXPLICIT_FLAGS: Flags = {
	skill: [
		"<tmp>/cli/one/SKILL.md",
		"<tmp>/cli/two",
		"<tmp>/cli/many",
		"relative/rel",
		"~/home-skill",
		"<tmp>/cli/missing",
		"<tmp>/cli/tool.sh",
		"<tmp>/cli/plain.md",
		"<tmp>/cli/declared/SKILL.md",
	],
	promptTemplate: ["<tmp>/cli/prompts/explain.md", "<tmp>/cli/prompt-dir", "<tmp>/cli/missing.md", "<tmp>/cli/tool.sh"],
};

const SETTINGS_TREE = {
	"agent/settings.json": JSON.stringify({
		skills: ["team-skills", "<tmp>/shared/solo/SKILL.md"],
		prompts: ["team-prompts"],
	}),
	"agent/team-skills/review/SKILL.md": described("From a directory in the user skills setting."),
	"agent/team-skills/review/scripts/check.sh": "#!/bin/sh\n",
	"agent/team-skills/review/references/rules.md": described("A reference, not a skill."),
	"agent/team-skills/root.md": skill(["name: root", "description: A Markdown file in the configured directory."]),
	"agent/team-skills/sub/notes.md": described("Not a skill, in a subdirectory without SKILL.md."),
	"agent/team-prompts/standup.md": "Write the standup.\n",
	"agent/team-prompts/deep/retro.md": "Write the retro.\n",
	"agent/team-prompts/deep/data.json": "{}\n",
	"agent/skills/user-auto/SKILL.md": described("From the user skills directory."),
	"agent/prompts/user-auto.md": "From the user prompts directory.\n",
	"shared/solo/SKILL.md": skill(["name: solo", "description: A file in the user skills setting."]),
	"repo/.pi/settings.json": JSON.stringify({ skills: ["../project-skills"], prompts: ["../project-prompts/one.md"] }),
	"repo/project-skills/lint/SKILL.md": described("From the project skills setting."),
	"repo/project-prompts/one.md": "From the project prompts setting.\n",
	"repo/project-prompts/two.md": "Not listed in the setting.\n",
	"repo/.pi/skills/project-auto/SKILL.md": described("From the project skills directory."),
	"repo/.pi/prompts/project-auto.md": "From the project prompts directory.\n",
};

const DISCOVERY_CASES: DiscoveryCase[] = [
	{
		name: "default-directories",
		description:
			"The skill and prompt directories of the agent dir, the trusted project, and both .agents directories. Project resources come first. Only SKILL.md files and described Markdown files directly in a skills directory are skills; only Markdown files directly in a prompts directory are templates.",
		files: DEFAULT_TREE,
		cwd: "repo",
		trusted: true,
	},
	{
		name: "untrusted-project",
		description: "An untrusted project's .pi and .agents resources are not loaded; the user's are.",
		files: DEFAULT_TREE,
		cwd: "repo",
		trusted: false,
	},
	{
		name: "agents-directories",
		description:
			"Project .agents/skills directories from the working directory up to the repository root, nearest first, then ~/.agents/skills. In .agents, a described Markdown file is a skill only below the skills directory, not directly in it.",
		files: {
			".agents/skills/above/SKILL.md": described("Above the repository root."),
			"repo/.git/HEAD": "ref: refs/heads/main\n",
			"repo/.agents/skills/root-skill/SKILL.md": described("At the repository root."),
			"repo/.agents/skills/top.md": skill(["name: top", "description: Directly in .agents/skills."]),
			"repo/app/.agents/skills/app-skill/SKILL.md": described("In the app directory."),
			"repo/app/src/.agents/skills/src-skill/SKILL.md": described("In the working directory."),
			"repo/app/src/.agents/skills/pack/inner.md": skill(["name: inner", "description: A file below .agents/skills."]),
			"repo/app/src/.agents/skills/pack/unnamed.md": described("A file below .agents/skills, named after its directory."),
			"home/.agents/skills/user-skill/SKILL.md": described("In ~/.agents/skills."),
			"home/.agents/skills/top.md": skill(["name: user-top", "description: Directly in ~/.agents/skills."]),
		},
		cwd: "repo/app/src",
		trusted: true,
	},
	{
		name: "agents-directories-without-repository",
		description:
			"Without a repository, every ancestor's .agents/skills is a project directory, except the home directory's, which stays the user's.",
		files: {
			".agents/skills/outer/SKILL.md": described("Above the home directory."),
			"home/.agents/skills/user-skill/SKILL.md": described("In ~/.agents/skills."),
			"home/work/.agents/skills/work-skill/SKILL.md": described("In the working directory."),
		},
		cwd: "home/work",
		trusted: true,
	},
	{
		name: "untrusted-project-below-home",
		description: "~/.agents/skills loads for an untrusted project below the home directory.",
		files: {
			"home/.agents/skills/user-skill/SKILL.md": described("In ~/.agents/skills."),
			"home/work/.agents/skills/work-skill/SKILL.md": described("In the working directory."),
		},
		cwd: "home/work",
		trusted: false,
	},
	{
		name: "frontmatter-rules",
		description:
			"A skill needs a description. A name defaults to the directory's. Names and descriptions outside the Agent Skills rules load with warnings; malformed frontmatter does not load.",
		files: {
			"agent/skills/Bad_Name/SKILL.md": described("Named after a directory with invalid characters."),
			"agent/skills/blank-description/SKILL.md": skill(['description: "   "']),
			"agent/skills/block-description/SKILL.md": skill([
				"description: >",
				"  Extract text and tables from PDF files.",
				"  Use when reading PDFs.",
			]),
			"agent/skills/crlf/SKILL.md": "<bom>---\r\nname: crlf\r\ndescription: Windows line endings.\r\n---\r\nBody.\r\n",
			"agent/skills/empty-name/SKILL.md": skill(['name: ""', "description: An empty name."]),
			"agent/skills/hidden/SKILL.md": skill(["description: Only by command.", "disable-model-invocation: true"]),
			"agent/skills/hyphens/SKILL.md": skill(["name: -lead--double-", "description: Hyphens in the wrong places."]),
			"agent/skills/list-description/SKILL.md": skill(["description:", "  - not", "  - a string"]),
			"agent/skills/long-description/SKILL.md": described("d".repeat(1025)),
			"agent/skills/long-name/SKILL.md": skill([`name: ${"n".repeat(65)}`, "description: A name that is too long."]),
			"agent/skills/malformed-note.md": "---\ndescription: Use when: broken\n---\nbody\n",
			"agent/skills/malformed/SKILL.md": "---\ndescription: Use when: broken\n---\nbody\n",
			"agent/skills/missing-description/SKILL.md": skill(["name: missing-description"]),
			"agent/skills/no-frontmatter/SKILL.md": "Just text.\n",
			"agent/skills/numeric-name/SKILL.md": skill(["name: 123", "description: A name that is a number."]),
			"agent/skills/quoted-flag/SKILL.md": skill(["description: A quoted flag.", 'disable-model-invocation: "true"']),
			"agent/skills/renamed/SKILL.md": skill(["name: other-name", "description: Named unlike its directory."]),
			"agent/skills/special/SKILL.md": skill(['description: "Caf\u00e9 <tags> & \\"quotes\\" \u2014 it\'s fine"']),
			"agent/prompts/malformed.md": "---\ndescription: Use when: broken\n---\nbody\n",
			"agent/prompts/number-description.md": "---\ndescription: 42\nargument-hint: [focus]\n---\nThe body's first line.\n",
			"agent/prompts/only-frontmatter.md": "---\ndescription: Nothing else\n---\n",
			"agent/prompts/empty.md": "",
			"agent/prompts/crlf.md": "<bom>---\r\ndescription: Windows\r\n---\r\nLine one\r\nLine two\r\n",
		},
		cwd: "repo",
		trusted: true,
	},
	{
		name: "name-collisions",
		description:
			"The first skill or template of a name wins and each later one is reported. A file reached by two paths loads once, without a report.",
		files: {
			"repo/.git/HEAD": "ref: refs/heads/main\n",
			"repo/.pi/skills/shared/SKILL.md": described("The project's."),
			"repo/.agents/skills/shared/SKILL.md": described("The project's .agents."),
			"agent/skills/shared/SKILL.md": described("The user's."),
			"agent/skills/other/SKILL.md": skill(["name: shared", "description: The user's, under another directory."]),
			"home/.agents/skills/shared/SKILL.md": described("The user's .agents."),
			"store/real/SKILL.md": described("Reached by a link and by a flag."),
			"cli/shared/SKILL.md": described("The command line's."),
			"repo/.pi/prompts/review.md": "The project's.\n",
			"agent/prompts/review.md": "The user's.\n",
			"cli/review.md": "The command line's.\n",
		},
		symlinks: { "agent/skills/linked": "store/real" },
		cwd: "repo",
		trusted: true,
		flags: { skill: ["<tmp>/store/real", "<tmp>/cli/shared"], promptTemplate: ["<tmp>/cli/review.md"] },
	},
	{
		name: "explicit-paths",
		description:
			"--skill and --prompt-template take files and directories, relative to the working directory or below ~, and load after everything discovered. A path that does not exist, or a skill path that is not Markdown, is reported.",
		files: EXPLICIT_TREE,
		cwd: "repo",
		trusted: true,
		flags: EXPLICIT_FLAGS,
	},
	{
		name: "no-skills-and-no-prompt-templates",
		description: "--no-skills and --no-prompt-templates drop what is discovered or configured and keep the explicit paths.",
		files: EXPLICIT_TREE,
		cwd: "repo",
		trusted: true,
		flags: { ...EXPLICIT_FLAGS, noSkills: true, noPromptTemplates: true },
	},
	{
		name: "no-skills-without-paths",
		description: "--no-skills and --no-prompt-templates alone load nothing.",
		files: EXPLICIT_TREE,
		cwd: "repo",
		trusted: true,
		flags: { noSkills: true, noPromptTemplates: true },
	},
	{
		name: "settings-paths",
		description:
			"The skills and prompts settings name files and directories, relative to the agent dir or the project's .pi. A skills directory gives its skills, not its scripts and references; a prompts directory gives every Markdown file below it. Order: project setting, project directory, user setting, user directory.",
		files: SETTINGS_TREE,
		cwd: "repo",
		trusted: true,
	},
	{
		name: "settings-paths-untrusted",
		description: "An untrusted project's settings are not read.",
		files: SETTINGS_TREE,
		cwd: "repo",
		trusted: false,
	},
	{
		name: "local-package",
		description:
			"A package directory named in settings gives its skills and prompts after the user's own: by its manifest, or from its skills and prompts directories.",
		files: {
			"agent/settings.json": JSON.stringify({ packages: ["<tmp>/packages/manifest", "<tmp>/packages/convention"] }),
			"agent/skills/user-auto/SKILL.md": described("From the user skills directory."),
			"agent/prompts/user-auto.md": "From the user prompts directory.\n",
			"packages/manifest/package.json": JSON.stringify({ name: "manifest", pi: { skills: ["tools"], prompts: ["texts"] } }),
			"packages/manifest/tools/pack-skill/SKILL.md": described("From a package manifest's skills."),
			"packages/manifest/tools/pack-skill/scripts/run.sh": "#!/bin/sh\n",
			"packages/manifest/tools/pack-skill/references/guide.md": described("A reference, not a skill."),
			"packages/manifest/tools/note.md": skill(["name: pack-note", "description: A Markdown file in a package's skills directory."]),
			"packages/manifest/texts/pack-prompt.md": "From a package manifest's prompts.\n",
			"packages/manifest/texts/deep/pack-deep.md": "Below a package's prompts directory.\n",
			"packages/manifest/texts/data.json": "{}\n",
			"packages/manifest/skills/unlisted/SKILL.md": described("Not in the manifest."),
			"packages/convention/skills/conv-skill/SKILL.md": described("From a package's skills directory."),
			"packages/convention/prompts/conv-prompt.md": "From a package's prompts directory.\n",
		},
		cwd: "repo",
		trusted: true,
	},
	{
		name: "ignore-files",
		description: "A .gitignore, .ignore, or .fdignore in a skills or prompts directory hides what it names.",
		files: {
			"agent/skills/.gitignore": "drafts/\nwip-*\n",
			"agent/skills/drafts/idea/SKILL.md": described("Ignored with its directory."),
			"agent/skills/wip-thing/SKILL.md": described("Ignored by a wildcard."),
			"agent/skills/wip-note.md": skill(["name: wip-note", "description: An ignored file."]),
			"agent/skills/kept/SKILL.md": described("Not ignored."),
			"agent/skills/own-ignore/SKILL.md": described("Ignored by the .ignore beside it."),
			"agent/skills/own-ignore/.ignore": "SKILL.md\n",
			"agent/skills/group/.fdignore": "inner/\n",
			"agent/skills/group/inner/SKILL.md": described("Ignored by its parent's .fdignore."),
			"agent/skills/group/outer/SKILL.md": described("Beside an ignored directory."),
			"agent/prompts/.gitignore": "secret.md\n",
			"agent/prompts/secret.md": "Ignored.\n",
			"agent/prompts/public.md": "Not ignored.\n",
		},
		cwd: "repo",
		trusted: true,
	},
];

async function writeTree(root: string, testCase: { files: Record<string, string>; symlinks?: Record<string, string>; cwd: string }) {
	for (const dir of ["agent", "home", testCase.cwd]) await mkdir(join(root, dir), { recursive: true });
	for (const [path, content] of Object.entries(testCase.files)) {
		await mkdir(dirname(join(root, path)), { recursive: true });
		await writeFile(join(root, path), content.replaceAll("<tmp>", root).replaceAll("<bom>", "\uFEFF"));
	}
	for (const [path, target] of Object.entries(testCase.symlinks ?? {})) {
		await mkdir(dirname(join(root, path)), { recursive: true });
		await symlink(join(root, target), join(root, path));
	}
}

/** Replaces the temporary directory in every string of `value`. */
function stable<T>(value: T, root: string): T {
	if (typeof value === "string") return value.replaceAll(root, "<tmp>") as T;
	if (Array.isArray(value)) return value.map((item) => stable(item, root)) as T;
	if (value && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, stable(item, root)])) as T;
	}
	return value;
}

const OWN_MESSAGES = [
	/^description is required$/,
	/^description exceeds \d+ characters \(\d+\)$/,
	/^name (exceeds|contains|must not) /,
	/^name ".*" collision$/,
	/^skill path (does not exist|is not a markdown file)$/,
	/^(Skill|Prompt template) path does not exist$/,
];

/** A diagnostic, with the YAML parser's message replaced: only upstream's own messages are compared. */
function diagnostic(value: Json): Json {
	const message = OWN_MESSAGES.some((pattern) => pattern.test(value.message)) ? value.message : "<yaml-error>";
	return { ...value, message };
}

/** Runs `run` with upstream's resource loader on `testCase`'s tree, with HOME inside it. */
async function withLoader<T>(
	testCase: { files: Record<string, string>; symlinks?: Record<string, string>; cwd: string; trusted: boolean; flags?: Flags },
	run: (context: { root: string; cwd: string; agentDir: string; settingsManager: SettingsManager; resourceLoader: DefaultResourceLoader }) => Promise<T>,
): Promise<T> {
	const root = await mkdtemp(join(tmpdir(), "pi-skills-"));
	const home = process.env.HOME;
	process.env.HOME = join(root, "home");
	try {
		const cwd = join(root, testCase.cwd);
		const agentDir = join(root, "agent");
		await writeTree(root, testCase);
		const settingsManager = SettingsManager.create(cwd, agentDir);
		settingsManager.setProjectTrusted(testCase.trusted);
		const expand = (paths?: string[]) => paths?.map((path) => path.replaceAll("<tmp>", root));
		const resourceLoader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager,
			noExtensions: true,
			noThemes: true,
			noContextFiles: true,
			noSkills: testCase.flags?.noSkills,
			noPromptTemplates: testCase.flags?.noPromptTemplates,
			additionalSkillPaths: expand(testCase.flags?.skill),
			additionalPromptTemplatePaths: expand(testCase.flags?.promptTemplate),
		});
		await resourceLoader.reload();
		return await run({ root, cwd, agentDir, settingsManager, resourceLoader });
	} finally {
		process.env.HOME = home;
		await rm(root, { recursive: true, force: true });
	}
}

const skillRecord = (skill: Skill) => ({
	name: skill.name,
	description: skill.description,
	filePath: skill.filePath,
	baseDir: skill.baseDir,
	disableModelInvocation: skill.disableModelInvocation,
	sourceInfo: skill.sourceInfo,
});

async function captureDiscovery(testCase: DiscoveryCase) {
	return withLoader(testCase, async ({ root, resourceLoader }) => {
		const skills = resourceLoader.getSkills();
		const prompts = resourceLoader.getPrompts();
		return stable(
			{
				name: testCase.name,
				description: testCase.description,
				files: testCase.files,
				symlinks: testCase.symlinks ?? {},
				cwd: testCase.cwd,
				trusted: testCase.trusted,
				flags: {
					skill: testCase.flags?.skill ?? [],
					noSkills: testCase.flags?.noSkills ?? false,
					promptTemplate: testCase.flags?.promptTemplate ?? [],
					noPromptTemplates: testCase.flags?.noPromptTemplates ?? false,
				},
				result: {
					skills: skills.skills.map(skillRecord),
					skillDiagnostics: skills.diagnostics.map(diagnostic),
					prompts: prompts.prompts.map((prompt) => ({
						name: prompt.name,
						description: prompt.description,
						argumentHint: prompt.argumentHint ?? null,
						content: prompt.content,
						filePath: prompt.filePath,
						sourceInfo: prompt.sourceInfo,
					})),
					promptDiagnostics: prompts.diagnostics.map(diagnostic),
				},
			},
			root,
		);
	});
}

// The system prompt's skills listing.

const LISTED_SKILLS = [
	{
		name: "pdf-tools",
		description: "Extract text and tables from PDF files. Use when reading PDFs.",
		filePath: "/home/user/.pi/agent/skills/pdf-tools/SKILL.md",
	},
	{
		name: "release",
		description: 'Cut a release <major|minor> & "tag" it; it\'s quick.',
		filePath: "/work/repo/.pi/skills/release & ship/SKILL.md",
	},
	{
		name: "deploy",
		description: "Only by command.",
		filePath: "/work/repo/.pi/skills/deploy/SKILL.md",
		disableModelInvocation: true,
	},
	{
		name: "notes",
		description: "Two lines\nof description.",
		filePath: "/work/repo/.agents/skills/notes/SKILL.md",
	},
];

function captureListing() {
	const skills: Skill[] = LISTED_SKILLS.map((skill) => ({
		...skill,
		baseDir: dirname(skill.filePath),
		disableModelInvocation: skill.disableModelInvocation ?? false,
		sourceInfo: { path: skill.filePath, source: "auto", scope: "user", origin: "top-level" },
	}));
	const hidden = skills.filter((skill) => skill.disableModelInvocation);
	const record = (name: string, description: string, listed: Skill[], tool: "read" | "bash") => ({
		name,
		description,
		tool,
		skills: listed.map(({ name, description, filePath, baseDir, disableModelInvocation }) => ({
			name,
			description,
			filePath,
			baseDir,
			disableModelInvocation,
		})),
		text: formatSkillsForPrompt(listed, tool),
	});
	return [
		record("read", "The read tool is active: skills are loaded with it. A skill that disables model invocation is not listed.", skills, "read"),
		record("bash", "Only bash is active: skills are loaded with it.", skills, "bash"),
		record("only-hidden-skills", "Nothing to list.", hidden, "read"),
		record("no-skills", "Nothing to list.", [], "read"),
	];
}

// Expansion of /skill:name and /template input.

const EXPANSION_TREE = {
	"agent/skills/review/SKILL.md": skill(
		["name: review", "description: Review code."],
		"\n# Review\n\nRead `references/checklist.md` first.\n\n",
	),
	"agent/skills/hidden/SKILL.md": skill(["description: Only by command.", "disable-model-invocation: true"], "Deploy carefully.\n"),
	"agent/skills/windows/SKILL.md": "<bom>---\r\ndescription: Windows line endings.\r\n---\r\nLine one\r\nLine two\r\n",
	"agent/skills/empty-body/SKILL.md": skill(["description: Nothing after the frontmatter."], ""),
	"agent/skills/gone/SKILL.md": described("Deleted after it was loaded."),
	"agent/skills/changed/SKILL.md": described("Malformed after it was loaded."),
	"agent/skills/note.md": skill(["name: note", "description: A file skill."], "Take a note.\n"),
	"agent/prompts/fix.md": '---\ndescription: Fix a bug\nargument-hint: "<issue> [notes]"\n---\n\nFix $1.\n\nNotes: ${@:2}\n\n',
	"agent/prompts/echo.md": "Say $ARGUMENTS\n",
	"agent/prompts/slash.md": "/fix nested\n",
	"agent/prompts/input.md": "Summarize {{input}} for $1.\n",
	"agent/prompts/skill:tpl.md": "A template named like a skill command: $@\n",
	"agent/prompts/skill:review.md": "A template named like a loaded skill's command: used when the skill command names no skill.\n",
};

const EXPANSION_INPUTS = [
	"/skill:review",
	"/skill:review check the auth module",
	"/skill:review   spaced   args  ",
	"/skill:review \n second line",
	"/skill:review\targs after a tab",
	"/skill:review\nargs after a newline",
	"/skill:hidden go",
	"/skill:windows",
	"/skill:empty-body",
	"/skill:empty-body with args",
	"/skill:note",
	"/skill:gone now",
	"/skill:changed now",
	"/skill:unknown do it",
	"/skill:",
	"/skill: review",
	" /skill:review",
	"/SKILL:review",
	"/skill:Review",
	"/skill:tpl a b",
	"/skill:review.md",
	"/fix",
	"/fix 42",
	'/fix "issue 12" needs care',
	"/fix   a   b   c",
	"/fix\ta\tb",
	"/fix\nline two\nline three",
	"/fix ",
	"/echo one  two",
	"/echo",
	"/echo 'it is' \"x y\" z",
	"/slash",
	"/input a b",
	"/unknown arg",
	"/",
	"/ fix",
	"//fix",
	"/fix/more",
	"/Fix 1",
	" /fix 1",
	"fix 1",
	"plain text",
	"/home/user/project/file.rs is broken",
	"",
];

async function captureExpansion() {
	const testCase = { files: EXPANSION_TREE, cwd: "repo", trusted: true };
	return withLoader(testCase, async ({ root, cwd, agentDir, settingsManager, resourceLoader }) => {
		const { session } = await createAgentSession({
			cwd,
			agentDir,
			settingsManager,
			resourceLoader,
			sessionManager: SessionManager.inMemory(cwd),
		});
		await rm(join(root, "agent/skills/gone/SKILL.md"));
		await writeFile(join(root, "agent/skills/changed/SKILL.md"), "---\ndescription: Use when: broken\n---\nbody\n");
		const cases = [];
		for (const input of EXPANSION_INPUTS) {
			// An idle session's steer() expands the input as prompt() and followUp() do, and queues it.
			await session.steer(input);
			const queued = session.clearQueue().steering;
			if (queued.length !== 1) throw new Error(`steer(${JSON.stringify(input)}) queued ${queued.length} messages`);
			cases.push({ input, output: queued[0] });
		}
		const skills = resourceLoader.getSkills().skills.map(skillRecord);
		const prompts = resourceLoader.getPrompts().prompts.map(({ name, content, filePath }) => ({ name, content, filePath }));
		session.dispose();
		return stable(
			{
				files: testCase.files,
				cwd: testCase.cwd,
				removed: ["agent/skills/gone/SKILL.md"],
				rewritten: { "agent/skills/changed/SKILL.md": "---\ndescription: Use when: broken\n---\nbody\n" },
				skills,
				prompts,
				cases,
			},
			root,
		);
	});
}

// Template arguments.

const ARGUMENT_STRINGS = [
	"",
	"one",
	"one two three",
	'"two words" three four',
	"'single quoted' \"double quoted\"",
	"a  b\tc\nd",
	"  leading and trailing  ",
	'"unterminated quote',
	"it's unterminated",
	'"" \'\' empty',
	'a"b c"d',
	'x="1 2" y',
	"back\\slash \\\"quote",
	"$1 $@ ${1:-d}",
	"\u00e9 \ud800\udf48 \u00f1",
	"a\u00a0b",
	"a\u3000b",
	"a\ufeffb",
	"a\u2028b",
	"a\u0085b",
	"a\u200bb",
	"a\u000bb\u000cc",
];

const TEMPLATE_CONTENTS = [
	"$1",
	"$1 $2 $3",
	"$@",
	"$ARGUMENTS",
	"[$@] [$ARGUMENTS]",
	"${1:-default}",
	"${2:-two words}",
	"${@:-nothing}",
	"${ARGUMENTS:-none}",
	"${@:2}",
	"${@:2:1}",
	"${@:0}",
	"${@:1:0}",
	"${@:5}",
	"a${@:2:2}z",
	"${1}",
	"${@}",
	"${ARGUMENTS}",
	"$10 $1",
	"$0",
	"$01",
	"$",
	"$$1",
	"$1$2",
	"$1abc",
	"$ARGUMENTSx",
	"$arguments $Arguments",
	"${1:-}",
	"${1:-a}b}",
	"${3:-$1}",
	"${3:-line\nbreak}",
	"${@:-}",
	"${ @:2}",
	"${@:x}",
	"${@:2:}",
	"${@:2:x}",
	"${12:-d}",
	"${0:-zero}",
	"${ARGUMENTS:2}",
	"\\$1",
	"$@$@",
	"price: $5.00",
	"$99999999999999999999",
	"${99999999999999999999:-big}",
	"${@:99999999999999999999}",
	"${@:1:99999999999999999999}",
	"$\u0661",
	"Fix $1.\n\nNotes: ${@:2}\nAll: $@\n",
];

const SUBSTITUTION_ARGUMENTS = ["", "one", "one two three", '"two words" three four five', "$1 ${2:-x} $@", '"" filled'];

function captureTemplateArguments() {
	return {
		parsing: ARGUMENT_STRINGS.map((input) => ({ input, args: parseCommandArgs(input) })),
		substitution: TEMPLATE_CONTENTS.map((content) => ({
			content,
			outputs: SUBSTITUTION_ARGUMENTS.map((input) => ({ input, output: substituteArgs(content, parseCommandArgs(input)) })),
		})),
	};
}

// get_commands of upstream's RPC mode.

type CommandsCase = { name: string; description: string; args: string[] };

/** The working directory is <tmp>/cwd. Every directory holds at most one skill or template, so no listing order matters here. */
const COMMANDS_TREE = {
	"agent/skills/alpha/SKILL.md": skill(["name: alpha", "description: The alpha skill."]),
	"agent/prompts/review.md": REVIEW_PROMPT,
	"cwd/.pi/skills/gamma/SKILL.md": skill(["name: gamma", "description: A project skill.", "disable-model-invocation: true"]),
	"cwd/.pi/prompts/ship.md": "Ship $1.\n",
	"cli/one/SKILL.md": skill(["name: one", "description: Given on the command line."]),
	"cli/explain.md": "---\ndescription: Explain something\n---\nExplain $@.\n",
};

const COMMANDS_CASES: CommandsCase[] = [
	{ name: "discovered", description: "A trusted project: templates first, then skills.", args: ["--approve"] },
	{ name: "untrusted", description: "An untrusted project.", args: ["--no-approve"] },
	{
		name: "explicit-paths",
		description: "--skill and --prompt-template add to what is discovered.",
		args: ["--approve", "--skill", "<tmp>/cli/one", "--prompt-template", "<tmp>/cli/explain.md"],
	},
	{
		name: "no-skills-with-skill",
		description: "--no-skills keeps the --skill path and the templates.",
		args: ["--approve", "--no-skills", "--skill", "<tmp>/cli/one"],
	},
	{
		name: "no-prompt-templates-with-template",
		description: "--no-prompt-templates keeps the --prompt-template path and the skills.",
		args: ["--approve", "-np", "--prompt-template", "<tmp>/cli/explain.md"],
	},
	{ name: "short-flags-alone", description: "-ns and -np alone leave no commands.", args: ["--approve", "-ns", "-np"] },
];

async function captureCommands(testCase: CommandsCase) {
	const root = await mkdtemp(join(tmpdir(), "pi-skills-"));
	try {
		await writeTree(root, { files: COMMANDS_TREE, cwd: "cwd" });
		// A model the process never calls: get_commands needs none, but RPC mode refuses to start without one.
		await writeFile(
			join(root, "agent/models.json"),
			JSON.stringify({
				providers: {
					stub: {
						baseUrl: "http://127.0.0.1:9/v1",
						api: "openai-completions",
						apiKey: "stub-key",
						models: [{ id: "stub-model", name: "Stub Model", reasoning: false, input: ["text"], contextWindow: 128000, maxTokens: 4096, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
					},
				},
			}),
		);
		const args = testCase.args.map((arg) => arg.replaceAll("<tmp>", root));
		const child = spawn(
			"npx",
			["tsx", CLI, "--mode", "rpc", "--model", "stub/stub-model", "--no-session", "--no-extensions", "--no-themes", "--no-context-files", ...args],
			{
				cwd: join(root, "cwd"),
				env: {
					PATH: process.env.PATH,
					HOME: join(root, "home"),
					npm_config_cache: process.env.npm_config_cache ?? join(homedir(), ".npm"),
					TSX_TSCONFIG_PATH: "/ts-reference/tsconfig.json",
					PI_CODING_AGENT_DIR: join(root, "agent"),
					PI_OFFLINE: "1",
					PI_SKIP_VERSION_CHECK: "1",
					PI_TELEMETRY: "0",
				},
			},
		);
		let stdout = "";
		let stderr = "";
		child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
			stdout += chunk;
		});
		child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
			stderr += chunk;
		});
		child.stdin.end(`${JSON.stringify({ id: "1", type: "get_commands" })}\n`);
		const code = await new Promise<number | null>((resolve) => child.on("exit", resolve));
		const responses = stdout
			.split("\n")
			.filter((line) => line.trim())
			.map((line) => JSON.parse(line))
			.filter((record) => record.type === "response" && record.id === "1");
		if (code !== 0 || responses.length !== 1 || !responses[0].success) {
			throw new Error(`${testCase.name}: pi exited with ${code}\nstdout:\n${stdout}\nstderr:\n${stderr}`);
		}
		return stable({ ...testCase, response: responses[0] }, root);
	} finally {
		await rm(root, { recursive: true, force: true });
	}
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: skills-fixture.ts <output-dir>");
	}
	const frontmatter = Object.entries(FRONTMATTER_CASES).map(([name, content]) => captureFrontmatter(name, content));
	const discovery = [];
	for (const testCase of DISCOVERY_CASES) discovery.push(await captureDiscovery(testCase));
	const expansion = await captureExpansion();
	const commands = [];
	for (const testCase of COMMANDS_CASES) commands.push(await captureCommands(testCase));
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "skills.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				format: {
					frontmatter:
						"content: a file. error: whether upstream rejects its frontmatter. fields: the frontmatter's top-level keys in order, each a string, a boolean, or another type (null when the frontmatter is not a mapping); body: the text after the frontmatter.",
					discovery:
						"files, symlinks, cwd, trusted, flags: the tree and the command line. result: the skills and prompt templates upstream's resource loader loads, in order, and their diagnostics.",
					listing: "skills, tool: what upstream's prompt builder is given. text: formatSkillsForPrompt's result, which the builder trims.",
					expansion:
						"files, cwd: the tree a session loads; removed, rewritten: changes made after loading. cases: for each input, the text a session queues for it.",
					templateArguments:
						"parsing: parseCommandArgs. substitution: substituteArgs on a template's content, for each input's parsed arguments.",
					commands: "files, cwd: the tree. args: upstream's command line after --mode rpc and the setup arguments. response: its answer to get_commands.",
				},
				placeholders: PLACEHOLDERS,
				frontmatter,
				discovery,
				listing: captureListing(),
				expansion,
				templateArguments: captureTemplateArguments(),
				commands: { files: COMMANDS_TREE, cwd: "cwd", cases: commands },
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

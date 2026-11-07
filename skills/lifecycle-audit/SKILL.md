---
name: lifecycle-audit
description: Audit a codebase for implicit state machines (lifecycles encoded as parallel bools, Options, counters, status strings, or enums with leaky side fields), verify the strongest findings against the source, then run a structural quality pass (file size, long functions, string-typed modes, error types, dependencies, tests, exits, logging) and file the results as tiered GitHub issues with a handoff summary. Use this whenever the user asks to find implicit or hidden state machines, state spread across flags, lifecycle bugs, "where is state modelled badly", or asks for a code quality / tech debt / cleanup audit of a whole repo or crate, even if they do not say "state machine". Also use it when they say "any other code quality improvements" after an audit, or ask to turn audit findings into issues.
---

# Lifecycle audit

An audit in two passes: first implicit state machines, then general structure.
Each pass ends with a chat report; issues are filed only after the user says so.

Why implicit state machines first: they are where lifecycle bugs live (a flag
that is never reset, a race between two fields, an illegal combination nobody
intended), and they are cheap to find by reading. Structural problems come
second because fixing them is what makes the lifecycle fixes cheap.

## 1. Size the codebase and partition it

Count lines per crate/package and find the largest files, so the partition is
by volume, not by directory count:

```bash
bash <skill-dir>/scripts/metrics.sh <repo-root> size
```

Make 4-6 partitions of roughly equal production-line volume. Put the giant
files (anything over ~5k lines) in a partition of their own. Keep tests out:
state machines in tests are not the target.

## 2. Fan out read-only searches

Spawn one read-only agent per partition in the same turn (`Explore` type or
equivalent). Give every agent the identical brief from
`references/agent-brief.md`, with only the scope changed. The brief carries the
signal list, the required report fields, a cap of about 15 findings, and the
instruction to ignore explicit enums with centralized transitions. A consistent
brief is what makes the reports mergeable.

While they run, tell the user what was launched and what the signals are. Do
not predict results.

## 3. Verify before reporting

Agents report what they infer from reading, and they are confident whether
right or wrong. Before any finding reaches the user as a "bug", read the cited
lines yourself. One `sed -n` of the cited range usually settles it. Mark every
finding as one of:

- **verified**: you read the lines and the defect is visible in them
- **from static read**: inferred by an agent, not reproduced

This label travels into the chat report and into every issue. It is the single
most useful thing in the handoff: it tells the next person what to trust and
what to check first.

Reasons to drop a finding: the "illegal combination" is unreachable in
practice and the agent said so; the enum already exists and transitions are
centralized; it is test tooling; it is a config pairing, not a lifecycle.

## 4. Rank into tiers and report in chat

Tier by how much an explicit state type would help, not by how large the code
is:

- **Tier 1, bugs**: a reachable wrong outcome (flag never reset, race window,
  dropped event, duplicate send). Say what the user sees go wrong.
- **Tier 2, explicit type would clearly help**: illegal combinations are
  reachable, transitions are scattered, or sub-state lives beside an enum.
- **Tier 3, lower value**: string-typed tags, redundant fields, conventions
  held only by doc comments. Group these per crate.

Chat report shape: Tier 1 as a numbered list with one line of consequence
each; Tier 2 as a table (location, entity, encoding/risk); Tier 3 as grouped
bullets; then "already fine" (what was checked and clean), then a suggested fix
order. End by offering to file issues or start on the smallest Tier 1 fix. Do
not file anything yet; issues are outward-facing.

## 5. File issues (on approval)

Read the repo's CLAUDE.md or CONTRIBUTING for label conventions first. Then:

- One tracking issue that explains the tiers, the verified/static-read
  distinction, the suggested order, and what was checked and found clean.
- One sub-issue per Tier 1 and Tier 2 finding, attached to the tracking issue
  (`parent_issue_number` with the GitHub MCP `issue_write`, or `gh issue create`
  plus a sub-issue link). Tier 3 becomes one grab-bag issue per crate, with a
  note to split items out when picked up.
- Each sub-issue follows `references/issue-template.md`: status line, Where
  (file:line per field/function), Problem (states, encoding, consequences),
  Suggested shape (a concrete enum sketch). The enum sketch matters: it turns
  a complaint into a design the fixer can start from.
- Labels: `bug` for Tier 1, `enhancement` otherwise, plus the repo's package
  labels. Fire all `issue_write` calls for a tier in one turn.

Keep issue text plain and technical. No emojis. Cite lines as `file.rs:123`.

## 6. Structural pass (when asked "anything else?")

Run the metrics and read the numbers before forming opinions:

```bash
bash <skill-dir>/scripts/metrics.sh <repo-root> all
```

It reports: files over 3k lines and where their test module starts, longest
functions, `unwrap`/`expect` in production vs test regions, `#[allow]`
attributes, TODO markers, string-literal comparisons per file, untyped JSON
use, `Result<_, String>` per crate, `process::exit` sites, `unsafe` blocks vs
`SAFETY` comments, duplicate dependency versions, test lines per crate,
sleep-based waits in tests, env-var reads and which `NOEL_*`-style variables
are undocumented, `eprintln!` vs structured logging.

Interpret, do not just list. For each metric decide whether it is a problem
here: 4 `unwrap`s in 8k production lines is fine and worth saying so; 94
`eprintln!` next to a TUI is not. Also report what is already good, so nobody
"fixes" it. Then run clippy/the repo's lint target if the toolchain is
available; if it is not (pinned toolchain missing, proxy blocks the download),
say so instead of guessing it is clean.

Typical issues from this pass, each under a second tracking issue: split
oversized files (move inline tests out first, then by seam); type the wire
protocol instead of `json!` and string keys; move schemas next to
implementations; replace string modes with enums (link the state-machine
issues that already cover some); break up long functions after the related
lifecycle fix so each is restructured once; typed errors; consolidate
`process::exit`; dependency hygiene; test gaps and sleep-based timing; unify
diagnostics; centralize env-var reads and document them.

Security-sensitive paths (secret file modes, atomic writes, RNG source, token
comparison, TLS, `SAFETY` comments) are checked in the same pass. When they
are clean, say so explicitly; that is a finding too.

## 7. Handoff paragraph

When asked for a summary or when the audit ends, write one paragraph that a
person with no context can act on: the three tracking roots and their counts,
the Tier 1 list with one clause each and which were verified, the suggested
order, the repo's validation rule (`make check` or equivalent), and the fact
that no code was changed. Say what could not be done (for example the
toolchain did not install) so nobody assumes clippy ran.

## Stopping

Reading finds structure. It does not find performance problems, flaky tests,
or parser behaviour under real streams; those need the code running. When the
metrics come back with nothing new, say that the reading pass is exhausted and
what a running pass would add, rather than stretching for more findings.

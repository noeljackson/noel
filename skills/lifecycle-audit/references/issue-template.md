# Issue templates

## Tracking issue

```
Title: Tracking: replace implicit state machines with explicit state types

An audit of <scope> found lifecycles encoded as parallel bools, Options,
counters and status strings instead of one enum with centralized transitions.
Several are reachable bugs.

Each finding is a sub-issue. Every sub-issue says whether it was:
- verified: the cited lines were read and the defect is visible in them, or
- from static read: inferred by reading, not reproduced. Confirm before fixing.

Tiers:
- Tier 1: concrete bugs. Fix first.
- Tier 2: an explicit enum would remove illegal combinations that are reachable.
- Tier 3: lower value, grouped by crate.

Suggested order: <3-4 steps, smallest clear bugs first, then the fix that
unblocks the most others>.

Checked and found clean: <modules with explicit enums, event logs, etc.>.
```

## Sub-issue (Tier 1 or 2)

```
Title: <Entity>: <what goes wrong>, in one line

Status: **verified** | **from static read**. Tier <1|2>.

## Where
- `path/file.rs:LINE`: `struct Foo` with fields `a: bool`, `b: Option<..>`
- `path/file.rs:LINE-LINE`: the functions that mutate them

## Problem
Implied states: A -> B -> C | D.
How it is encoded: <fields>.
- <consequence 1, with the line that causes it>
- <consequence 2>
Illegal combinations that are representable: <list>.

## Suggested shape
enum FooState {
    A,
    B { since: Instant },
    C(Outcome),
}
One transition function; <what should reset where>.
```

## Grab-bag issue (Tier 3, one per crate)

```
Title: Minor implicit state in <crate> (<3-4 words of topics>)

Status: from static read. Tier 3. Split any item into its own issue when
picked up.

- `file.rs:LINE` <entity>: <one line encoding + one line risk>
- ...
```

## Labels

- Tier 1: `bug` plus the package label(s)
- Tier 2 and 3: `enhancement` plus the package label(s)
- Package labels come from the repo's CLAUDE.md / CONTRIBUTING; check an
  existing label with `get_label` before using it

## Wording

- Plain, technical, no emojis.
- Cite as `file.rs:123`; GitHub does not link these but reviewers grep them.
- Say what the user observes for Tier 1 ("Working... disappears early",
  "restart limit never applies again"), not only the mechanism.
- When a Tier 3 item is already covered by a Tier 1/2 issue, link it instead
  of repeating it.

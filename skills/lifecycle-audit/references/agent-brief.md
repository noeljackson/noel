# Brief for each read-only search agent

Send this verbatim to every partition agent, changing only the Scope line.
Identical briefs produce reports with the same fields, which is what lets you
merge and rank them without re-reading the code.

```
Repo: <root> (<languages>). Goal: find IMPLICIT state machines: places where an
entity moves through lifecycle phases but the state is NOT modeled as one
explicit enum with guarded transitions.

Signals:
- several bool/Option fields on one struct that constrain each other
  (connected / closing / pending: Option<..>; usage / unsupported / error)
- string-typed status or phase fields compared with ==
- integer counters or attempt numbers that drive behavior
- loops with flag variables that change mode (in_tool, saw_x, done, round_open)
- retry / backoff / reconnect / failover loops
- "if self.a.is_some() && !self.b" style guards
- ordering-sensitive call sequences enforced only by convention
  (must call X before Y; handshake before send)
- an enum exists but transitions are scattered and unchecked, or side fields
  beside it encode sub-state
- sentinel values (0 = unset, None = timed out, "" = unknown)

Ignore explicit enum FSMs with centralized transitions unless they carry leaky
side flags. Skip tests and test fixtures.

Scope (very thorough): <files / crates>.

Report, per finding:
- file:line (struct or function)
- the entity
- the implied states and transitions as you infer them
- which fields / variables encode it
- the concrete risk: illegal combination reachable, missing reset, race,
  dropped event, duplicate send
- rating high / medium / low by how much an explicit enum would help

Be concrete and terse; no generic advice. Say "inferred from reading, not
run" where that is the case. Cap at ~15 strongest findings. End with a short
"checked and clean" list so the merger knows what was covered.
```

## Partition hints

- Giant files (5k+ lines) get their own agent; name the functions to focus on
  (turn loop, replay, stream parsers) so the agent does not skim.
- Keep protocol/transport crates together (chord, env, client); their
  lifecycles reference each other.
- Put native clients (Swift, Kotlin) with the FFI crate they consume.
- TypeScript hosts go with the Rust side that spawns them; the handshake is
  the interesting part.

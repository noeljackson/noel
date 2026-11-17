# Context Compaction


`/compact preserve architecture decisions and outstanding test failures` asks the
active model for a continuation checkpoint with tools disabled. The summary
preserves goals/constraints, decisions, completed work/files, errors, next steps,
and references. Recent complete user turns remain unchanged, including paired
tool calls/results and attachments. Persistent system/project instructions are
not summarized and remain loaded independently. Prior checkpoints are merged on
subsequent compactions, rather than nested first/last snippets.

Automatic compaction runs before every model request, including tool rounds and
queued prompts. It uses **estimated active-context tokens**, including instructions,
tool schemas, calls, and attachment allowances—not billed session totals or message
count. Configure `.noel/settings.json` or global settings:

```json
{
  "compaction": {
    "enabled": true,
    "triggerPercent": 80,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  }
}
```

The trigger fires at the percentage threshold or when the response reserve is
needed. `triggerPercent` is clamped to 1–99; the reserve is capped at half the
window. The recent-token budget is capped at a quarter-window and applies to
older complete turns; the latest turn is always retained intact. Oversized
histories are summarized in bounded rolling fragments without dropping text.
If automatic compaction cannot get under the budget (for example a very large
system prompt, tool set, or persistent instructions, or a single turn larger
than the window), the turn continues with the original history instead of
failing: the failure is reported in the transcript with a token breakdown
(`system prompt`/`tools`/`persistent instructions`/`retained recent turns`),
and re-attempts are skipped until history changes. If the provider then
refuses the request as too large, "Run recovery" below applies.
Model windows come from the catalog; custom `models.json` entries can specify
`contextWindow`. Unknown windows fall back to 128,000 tokens.
Token counts are heuristics, not exact provider tokenization.

The agent per-run round cap is configurable alongside compaction and retry:

```json
{
  "agent": {
    "maxToolRounds": 120
  }
}
```

`agent.maxToolRounds` bounds provider rounds per user run, including steering
rounds. The default is 200; values are clamped to at least 1. When the cap is
reached the turn stops with a round-limit error, all work stays saved, and
sending any prompt (or restarting, which auto-continues) gets a fresh budget.
A request that is retried, or asked again after an overflow, is one round.

## Run recovery

A model request that fails is handled as upstream pi handles it, in the TUI,
print mode, and RPC mode alike:

- **Retry.** A failure that a retry can fix is retried, whether the request
  failed before any output or broke off mid-reply: a connection that cannot
  be made or drops, a timeout, HTTP 408, 429, 500, 502, 503, 504, 520, and
  524, and any answer that says the provider is overloaded or rate limited.
  The reply that broke off is dropped: it is not saved and not sent back to
  the model. Defaults are 3 retries with exponential backoff from
  2 s, capped at 60 s (`retry.enabled`, `retry.maxRetries`,
  `retry.baseDelayMs`, `retry.maxAgentDelayMs`). Other failures end the turn
  at once: 400, 401, 403, and 404 responses, other statuses whose body names
  nothing transient, a missing credential, and quota or billing limits even
  when they arrive as a 429. With [account failover](configuration.md#account-failover) on,
  a 429 before any output goes to another account first. The rules
  are upstream's patterns over the error text
  (`error-classification.json`). When retries
  run out, the error names the likely causes, including a local firewall
  that denies connections from a freshly built binary until its prompt is
  approved, because on default-deny hosts an unanswered prompt is
  indistinguishable from an outage.
- **Context overflow.** When the provider refuses a request as too large for
  the model's context window, noel compacts and asks again, once. The same
  happens when a final reply is cut off at the output limit although the
  model could have written more, by its catalog `maxTokens` (for a
  `models.json` model of a provider the catalog does not know that names no
  `maxTokens`, noel assumes the context was the cause): the cut-off reply is
  dropped and the compacted request is sent again. A reply cut off
  while calling a tool is kept instead; its calls get error results and the
  model issues them again. If there is nothing to compact, or compaction is
  off or fails, an overflow error ends the turn, and a cut-off reply stands
  as the turn's answer. A second overflow in a row is not compacted again.

In the TUI a retry or an overflow appears as a system line (`retrying in 2s
(attempt 1 of 3): …`); text the dropped reply had streamed stays above it,
and the reply that replaces it starts below. Esc during a backoff interrupts
the turn.

The original transcript remains in the session journal. A validated, smaller
checkpoint and its replacement context are committed together through an atomic
file replacement. Empty/truncated/oversized/tool-bearing summaries, provider
failures, or cancellation leave active history intact; no silent truncation
fallback is used. If the latest turn or persistent instructions still cannot fit,
noel reports an error instead of repeatedly compacting. Completed summarization
requests count toward `/usage`. `/export` exports the current checkpoint and
retained context, not the pre-compaction journal transcript.

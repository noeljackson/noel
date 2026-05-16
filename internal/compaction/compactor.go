package compaction

import (
	"context"
	"errors"
	"strings"
	"time"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/session"
)

const (
	defaultReserveTokens = 16384
	defaultKeepTokens    = 20000
)

const summarizationSystemPrompt = `You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.`

const summarizationPrompt = `The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.`

const updateSummarizationPrompt = `The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.`

const turnPrefixSummarizationPrompt = `This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix.`

type Settings struct {
	Enabled bool
	// ReserveTokens triggers compaction when MaxTokens-ReserveTokens is exceeded.
	ReserveTokens int
	// KeepRecentTokens is the target preserved suffix size.
	KeepRecentTokens int
	// MaxTokens is the maximum model context window.
	MaxTokens int
	// SummaryProvider generates compaction summaries.
	SummaryProvider SummaryProvider

	// Legacy fields accepted for existing callers.
	TriggerFraction float64
	TargetTokens    int
}

type SummaryProvider interface {
	Summarize(ctx context.Context, messages []agent.Message) (summary string, err error)
}

type Compactor struct {
	settings  Settings
	estimator ContextEstimator
}

func New(s Settings) *Compactor {
	if !s.Enabled {
		s.Enabled = true
	}
	if s.ReserveTokens == 0 {
		s.ReserveTokens = defaultReserveTokens
	}
	if s.KeepRecentTokens == 0 {
		s.KeepRecentTokens = defaultKeepTokens
	}
	if s.TargetTokens > 0 {
		s.KeepRecentTokens = s.TargetTokens
	}
	return &Compactor{settings: s}
}

type Plan struct {
	CutIndex              int
	SummarizedKept        []agent.Message
	PreservedSuffix       []agent.Message
	EstimatedTokensBefore int
	EstimatedTokensAfter  int
}

func (c *Compactor) ShouldCompact(messages []agent.Message, system string) bool {
	if c == nil || !c.settings.Enabled || c.settings.MaxTokens <= 0 {
		return false
	}
	tokens := c.estimator.EstimateTokens(messages, system)
	if c.settings.TriggerFraction > 0 {
		return float64(tokens) > float64(c.settings.MaxTokens)*c.settings.TriggerFraction
	}
	return tokens > c.settings.MaxTokens-c.settings.ReserveTokens
}

// PrepareCompaction returns a preview plan for how compaction will rewrite
// messages.
func (c *Compactor) PrepareCompaction(messages []agent.Message, system string) (Plan, error) {
	if c == nil {
		return Plan{}, nil
	}
	cutPoint := FindCutPointResult(messages, c.settings.KeepRecentTokens)
	if cutPoint.CutIndex == 0 {
		return Plan{
			CutIndex:              0,
			SummarizedKept:        nil,
			PreservedSuffix:       append([]agent.Message(nil), messages...),
			EstimatedTokensBefore: c.estimator.EstimateTokens(messages, system),
			EstimatedTokensAfter:  c.estimator.EstimateTokens(messages, system),
		}, nil
	}
	historyEnd := cutPoint.CutIndex
	if cutPoint.IsSplitTurn {
		historyEnd = cutPoint.TurnStartIndex
	}
	before := c.estimator.EstimateTokens(messages, system)
	suffix := append([]agent.Message(nil), messages[cutPoint.CutIndex:]...)
	after := estimateMessagesTokens(suffix)
	if after > 0 {
		after += estimateMessageTokens(agent.CompactionSummaryMessage{})
	}
	return Plan{
		CutIndex:              cutPoint.CutIndex,
		SummarizedKept:        append([]agent.Message(nil), messages[:historyEnd]...),
		PreservedSuffix:       suffix,
		EstimatedTokensBefore: before,
		EstimatedTokensAfter:  after,
	}, nil
}

// SplitTurn returns the prefix messages of a split turn. An empty result means
// the cut point is already at a turn boundary.
func (c *Compactor) SplitTurn(messages []agent.Message, cut int) ([]agent.Message, error) {
	if cut <= 0 || cut >= len(messages) {
		return nil, nil
	}
	if isTurnStart(messages[cut]) {
		return nil, nil
	}
	start := findTurnStartIndex(messages, cut)
	if start < 0 || start >= cut {
		return nil, nil
	}
	return append([]agent.Message(nil), messages[start:cut]...), nil
}

// UpdatePreviousSummary regenerates the running summary by combining a previous
// summary with new turns since then.
func (c *Compactor) UpdatePreviousSummary(ctx context.Context, prev string, newMessages []agent.Message) (string, error) {
	if c == nil || c.settings.SummaryProvider == nil {
		return "", errors.New("compaction summary provider is required")
	}
	return c.generateSummary(ctx, newMessages, prev)
}

// Compact replaces the summarized prefix with a compaction summary message and
// preserves the suffix from the selected cut point.
func (c *Compactor) Compact(ctx context.Context, messages []agent.Message, system string) ([]agent.Message, error) {
	if c == nil {
		return messages, nil
	}
	plan, err := c.PrepareCompaction(messages, system)
	if err != nil {
		return nil, err
	}
	if plan.CutIndex == 0 {
		return messages, nil
	}
	if c.settings.SummaryProvider == nil {
		return nil, errors.New("compaction summary provider is required")
	}

	summary, err := c.buildSummary(ctx, messages, plan.CutIndex)
	if err != nil {
		return nil, err
	}

	compacted := make([]agent.Message, 0, 1+len(messages)-plan.CutIndex)
	compacted = append(compacted, agent.CompactionSummaryMessage{
		Timestamp:    time.Now(),
		Summary:      summary,
		TokensBefore: plan.EstimatedTokensBefore,
		DroppedCount: plan.CutIndex,
	})
	compacted = append(compacted, messages[plan.CutIndex:]...)
	return compacted, nil
}

// MaybeCompact is called before a provider stream request.
func (c *Compactor) MaybeCompact(ctx context.Context, messages []agent.Message, system string) ([]agent.Message, error) {
	if c == nil || !c.ShouldCompact(messages, system) {
		return messages, nil
	}
	return c.Compact(ctx, messages, system)
}

func (c *Compactor) ShouldRetryOnOverflow(err error) bool {
	if err == nil {
		return false
	}
	lower := strings.ToLower(err.Error())
	return strings.Contains(lower, "context") &&
		(strings.Contains(lower, "exceed") || strings.Contains(lower, "too long") || strings.Contains(lower, "overflow"))
}

func (c *Compactor) AfterOverflowRetry(ctx context.Context, messages []agent.Message, system string) ([]agent.Message, error) {
	return c.Compact(ctx, messages, system)
}

// MaybeCompactSession is called before a provider stream request when the
// caller also wants a compaction record persisted.
func (c *Compactor) MaybeCompactSession(ctx context.Context, sess *session.Session, messages []agent.Message, system string) ([]agent.Message, error) {
	if c == nil || !c.ShouldCompact(messages, system) {
		return messages, nil
	}
	cut := FindCutPoint(messages, c.settings.KeepRecentTokens)
	compacted, err := c.Compact(ctx, messages, system)
	if err != nil {
		return nil, err
	}
	if len(compacted) == len(messages) {
		return compacted, nil
	}
	summary := summaryText(compacted[0])
	parentID := ""
	if sess != nil {
		parentID = sess.LastMessageRecordID()
		if err := Record(sess, parentID, summary, cut); err != nil {
			return nil, err
		}
	}
	return compacted, nil
}

func (c *Compactor) buildSummary(ctx context.Context, messages []agent.Message, cut int) (string, error) {
	cutPoint := FindCutPointResult(messages, c.settings.KeepRecentTokens)
	historyEnd := cut
	turnPrefixMessages := []agent.Message(nil)
	if cutPoint.IsSplitTurn {
		historyEnd = cutPoint.TurnStartIndex
		prefix, err := c.SplitTurn(messages, cut)
		if err != nil {
			return "", err
		}
		turnPrefixMessages = prefix
	}

	previousSummary, historyMessages := extractPreviousSummary(messages[:historyEnd])
	summary, err := c.generateSummary(ctx, historyMessages, previousSummary)
	if err != nil {
		return "", err
	}
	if summary == "" {
		summary = "No prior history."
	}
	if len(turnPrefixMessages) > 0 {
		prefixSummary, err := c.generateTurnPrefixSummary(ctx, turnPrefixMessages)
		if err != nil {
			return "", err
		}
		summary = summary + "\n\n---\n\n**Turn Context (split turn):**\n\n" + prefixSummary
	}

	ops := make([]FileOperation, 0)
	for _, message := range historyMessages {
		ops = append(ops, ExtractFileOps(message)...)
	}
	for _, message := range turnPrefixMessages {
		ops = append(ops, ExtractFileOps(message)...)
	}
	readOnly, modified := ComputeFileLists(ops, 0)
	summary += FormatFileOperations(readOnly, modified)
	return normalizeSummary(summary), nil
}

func (c *Compactor) generateSummary(ctx context.Context, messages []agent.Message, previousSummary string) (string, error) {
	summary, err := c.settings.SummaryProvider.Summarize(ctx, summaryMessages(messages, previousSummary, summarizationPrompt, updateSummarizationPrompt))
	if err != nil {
		return "", err
	}
	return normalizeSummary(summary), nil
}

func (c *Compactor) generateTurnPrefixSummary(ctx context.Context, messages []agent.Message) (string, error) {
	summary, err := c.settings.SummaryProvider.Summarize(ctx, summaryMessages(messages, "", turnPrefixSummarizationPrompt, ""))
	if err != nil {
		return "", err
	}
	return normalizeSummary(summary), nil
}

func summaryMessages(messages []agent.Message, previousSummary string, initialPrompt string, updatePrompt string) []agent.Message {
	basePrompt := initialPrompt
	if previousSummary != "" && updatePrompt != "" {
		basePrompt = updatePrompt
	}
	prompt := "<conversation>\n" + serializeConversation(agent.ConvertToLLM(messages)) + "\n</conversation>\n\n"
	if previousSummary != "" {
		prompt += "<previous-summary>\n" + previousSummary + "\n</previous-summary>\n\n"
	}
	prompt += basePrompt
	return []agent.Message{
		agent.SystemMessage{Content: []agent.Content{agent.TextContent{Text: summarizationSystemPrompt}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: prompt}}},
	}
}

func extractPreviousSummary(messages []agent.Message) (string, []agent.Message) {
	if len(messages) == 0 {
		return "", nil
	}
	switch msg := messages[0].(type) {
	case agent.CompactionSummaryMessage:
		return msg.Summary, append([]agent.Message(nil), messages[1:]...)
	case *agent.CompactionSummaryMessage:
		if msg != nil {
			return msg.Summary, append([]agent.Message(nil), messages[1:]...)
		}
	case agent.SystemMessage:
		if text := systemMessageText(msg); text != "" {
			return text, append([]agent.Message(nil), messages[1:]...)
		}
	case *agent.SystemMessage:
		if msg != nil {
			if text := systemMessageText(*msg); text != "" {
				return text, append([]agent.Message(nil), messages[1:]...)
			}
		}
	}
	return "", append([]agent.Message(nil), messages...)
}

func summaryText(message agent.Message) string {
	switch msg := message.(type) {
	case agent.CompactionSummaryMessage:
		return msg.Summary
	case *agent.CompactionSummaryMessage:
		if msg != nil {
			return msg.Summary
		}
	case agent.SystemMessage:
		return systemMessageText(msg)
	case *agent.SystemMessage:
		if msg != nil {
			return systemMessageText(*msg)
		}
	}
	return ""
}

func systemMessageText(systemMessage agent.SystemMessage) string {
	parts := make([]string, 0, len(systemMessage.Content))
	for _, content := range systemMessage.Content {
		if text, ok := content.(agent.TextContent); ok {
			parts = append(parts, text.Text)
		}
	}
	return strings.Join(parts, "\n")
}

func normalizeSummary(summary string) string {
	summary = strings.TrimSpace(summary)
	if summary == "" {
		return "No summary generated."
	}
	return summary
}

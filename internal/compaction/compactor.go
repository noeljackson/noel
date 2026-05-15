package compaction

import (
	"context"
	"errors"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/session"
)

const (
	defaultTriggerFraction = 0.85
	defaultTargetFraction  = 0.5
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

type Settings struct {
	// TriggerFraction triggers compaction at this fraction of the model window.
	TriggerFraction float64
	// TargetTokens is the target token count after compaction.
	TargetTokens int
	// MaxTokens is the maximum model context window.
	MaxTokens int
	// SummaryProvider generates compaction summaries.
	SummaryProvider SummaryProvider
}

type SummaryProvider interface {
	Summarize(ctx context.Context, messages []agent.Message) (summary string, err error)
}

type Compactor struct {
	settings  Settings
	estimator ContextEstimator
}

func New(s Settings) *Compactor {
	if s.TriggerFraction == 0 {
		s.TriggerFraction = defaultTriggerFraction
	}
	if s.TargetTokens == 0 && s.MaxTokens > 0 {
		s.TargetTokens = int(float64(s.MaxTokens) * defaultTargetFraction)
	}
	return &Compactor{settings: s}
}

func (c *Compactor) ShouldCompact(messages []agent.Message, system string) bool {
	if c == nil || c.settings.MaxTokens <= 0 {
		return false
	}
	tokens := c.estimator.EstimateTokens(messages, system)
	return float64(tokens) > float64(c.settings.MaxTokens)*c.settings.TriggerFraction
}

// Compact replaces the summarized prefix with a single system summary message
// and preserves the suffix from the selected cut point.
func (c *Compactor) Compact(ctx context.Context, messages []agent.Message, system string) ([]agent.Message, error) {
	if c == nil {
		return messages, nil
	}
	cut := FindCutPoint(messages, c.settings.TargetTokens)
	if cut == 0 {
		return messages, nil
	}
	if c.settings.SummaryProvider == nil {
		return nil, errors.New("compaction summary provider is required")
	}

	summary, err := c.settings.SummaryProvider.Summarize(ctx, summaryMessages(messages[:cut]))
	if err != nil {
		return nil, err
	}
	summary = strings.TrimSpace(summary)
	if summary == "" {
		summary = "No summary generated."
	}

	compacted := make([]agent.Message, 0, 1+len(messages)-cut)
	compacted = append(compacted, agent.SystemMessage{Content: []agent.Content{agent.TextContent{Text: summary}}})
	compacted = append(compacted, messages[cut:]...)
	return compacted, nil
}

// MaybeCompact is called before a provider stream request. It records the
// compaction event when compaction occurs.
func (c *Compactor) MaybeCompact(ctx context.Context, sess *session.Session, messages []agent.Message, system string) ([]agent.Message, error) {
	if c == nil || !c.ShouldCompact(messages, system) {
		return messages, nil
	}
	cut := FindCutPoint(messages, c.settings.TargetTokens)
	if cut == 0 {
		return messages, nil
	}
	compacted, err := c.Compact(ctx, messages, system)
	if err != nil {
		return nil, err
	}
	if len(compacted) == len(messages) {
		return compacted, nil
	}
	summary := systemMessageText(compacted[0])
	parentID := ""
	if sess != nil {
		parentID = sess.LastMessageRecordID()
		if err := Record(sess, parentID, summary, cut); err != nil {
			return nil, err
		}
	}
	return compacted, nil
}

func summaryMessages(messages []agent.Message) []agent.Message {
	prompt := "<conversation>\n" + serializeConversation(messages) + "\n</conversation>\n\n" + summarizationPrompt
	return []agent.Message{
		agent.SystemMessage{Content: []agent.Content{agent.TextContent{Text: summarizationSystemPrompt}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: prompt}}},
	}
}

func systemMessageText(message agent.Message) string {
	systemMessage, ok := message.(agent.SystemMessage)
	if !ok {
		return ""
	}
	parts := make([]string, 0, len(systemMessage.Content))
	for _, content := range systemMessage.Content {
		if text, ok := content.(agent.TextContent); ok {
			parts = append(parts, text.Text)
		}
	}
	return strings.Join(parts, "\n")
}

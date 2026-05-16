// Package compaction provides context-window compaction helpers.
package compaction

import (
	"encoding/json"
	"math"

	"github.com/noeljackson/pi/internal/agent"
)

const (
	imageChars = 4800
)

type ContextEstimator struct{}

// ContextUsageEstimate describes a TS-compatible context estimate.
type ContextUsageEstimate struct {
	Tokens         int
	UsageTokens    int
	TrailingTokens int
	LastUsageIndex *int
}

// CalculateContextTokens returns provider-reported total context usage.
func CalculateContextTokens(usage agent.Usage) int {
	if usage.TotalTokens > 0 {
		return usage.TotalTokens
	}
	return usage.InputTokens + usage.OutputTokens + usage.CacheReadInputTokens + usage.CacheCreationInputTokens
}

// EstimateContextTokens estimates context tokens from messages using the last
// successful assistant usage as an anchor and estimating only trailing messages.
func (e *ContextEstimator) EstimateContextTokens(messages []agent.Message) ContextUsageEstimate {
	usage, index, ok := lastAssistantUsage(messages)
	if !ok {
		estimated := 0
		for _, message := range messages {
			estimated += estimateMessageTokens(message)
		}
		return ContextUsageEstimate{Tokens: estimated, TrailingTokens: estimated}
	}

	usageTokens := CalculateContextTokens(usage)
	trailingTokens := 0
	for i := index + 1; i < len(messages); i++ {
		trailingTokens += estimateMessageTokens(messages[i])
	}
	lastUsageIndex := index
	return ContextUsageEstimate{
		Tokens:         usageTokens + trailingTokens,
		UsageTokens:    usageTokens,
		TrailingTokens: trailingTokens,
		LastUsageIndex: &lastUsageIndex,
	}
}

// EstimateTokens returns the TS-compatible estimate for the given messages.
// The system prompt parameter is accepted for caller compatibility; TS
// compaction estimates only conversation messages.
func (e *ContextEstimator) EstimateTokens(messages []agent.Message, system string) int {
	_ = system
	return e.EstimateContextTokens(messages).Tokens
}

func lastAssistantUsage(messages []agent.Message) (agent.Usage, int, bool) {
	for i := len(messages) - 1; i >= 0; i-- {
		msg, ok := asAssistant(messages[i])
		if !ok {
			continue
		}
		if msg.StopReason == agent.StopAbort || msg.StopReason == agent.StopError {
			continue
		}
		if !usagePopulated(msg.Usage) {
			continue
		}
		return msg.Usage, i, true
	}
	return agent.Usage{}, -1, false
}

func usagePopulated(usage agent.Usage) bool {
	return usage.InputTokens != 0 ||
		usage.OutputTokens != 0 ||
		usage.CacheReadInputTokens != 0 ||
		usage.CacheCreationInputTokens != 0 ||
		usage.TotalTokens != 0 ||
		usage.CacheWriteTokens != 0
}

func asAssistant(message agent.Message) (agent.AssistantMessage, bool) {
	switch msg := message.(type) {
	case agent.AssistantMessage:
		return msg, true
	case *agent.AssistantMessage:
		if msg == nil {
			return agent.AssistantMessage{}, false
		}
		return *msg, true
	default:
		return agent.AssistantMessage{}, false
	}
}

func estimateMessagesTokens(messages []agent.Message) int {
	total := 0
	for _, message := range messages {
		total += estimateMessageTokens(message)
	}
	return total
}

func estimateMessageTokens(message agent.Message) int {
	if message == nil {
		return 0
	}
	chars := 0
	switch msg := message.(type) {
	case agent.UserMessage:
		chars += estimateTextContentsChars(msg.Content)
	case *agent.UserMessage:
		chars += estimateTextContentsChars(msg.Content)
	case agent.AssistantMessage:
		chars += estimateAssistantChars(msg.Content)
	case *agent.AssistantMessage:
		chars += estimateAssistantChars(msg.Content)
	case agent.ToolResultMessage:
		chars += estimateToolResultChars(msg.Results)
	case *agent.ToolResultMessage:
		chars += estimateToolResultChars(msg.Results)
	case agent.SystemMessage:
		chars += estimateToolResultContentChars(msg.Content)
	case *agent.SystemMessage:
		chars += estimateToolResultContentChars(msg.Content)
	case agent.BashExecutionMessage:
		chars += len(msg.Command) + len(msg.Output)
	case *agent.BashExecutionMessage:
		chars += len(msg.Command) + len(msg.Output)
	case agent.CustomMessage:
		chars += estimateToolResultContentChars(msg.Content)
	case *agent.CustomMessage:
		chars += estimateToolResultContentChars(msg.Content)
	case agent.BranchSummaryMessage:
		chars += len(msg.Summary)
	case *agent.BranchSummaryMessage:
		chars += len(msg.Summary)
	case agent.CompactionSummaryMessage:
		chars += len(msg.Summary)
	case *agent.CompactionSummaryMessage:
		chars += len(msg.Summary)
	}
	return estimateTextTokensFromChars(chars)
}

func estimateTextContentsChars(contents []agent.Content) int {
	chars := 0
	for _, content := range contents {
		switch block := content.(type) {
		case agent.TextContent:
			chars += len(block.Text)
		case *agent.TextContent:
			chars += len(block.Text)
		}
	}
	return chars
}

func estimateAssistantChars(contents []agent.Content) int {
	chars := 0
	for _, content := range contents {
		switch block := content.(type) {
		case agent.TextContent:
			chars += len(block.Text)
		case *agent.TextContent:
			chars += len(block.Text)
		case agent.ThinkingContent:
			chars += len(block.Thinking)
		case *agent.ThinkingContent:
			chars += len(block.Thinking)
		case agent.ToolUseContent:
			chars += estimateToolUseChars(block)
		case *agent.ToolUseContent:
			chars += estimateToolUseChars(*block)
		}
	}
	return chars
}

func estimateToolUseChars(block agent.ToolUseContent) int {
	input := string(block.Input)
	if len(block.Input) > 0 {
		var decoded any
		if err := json.Unmarshal(block.Input, &decoded); err == nil {
			if encoded, err := json.Marshal(decoded); err == nil {
				input = string(encoded)
			}
		}
	}
	return len(block.Name) + len(input)
}

func estimateToolResultChars(results []agent.ToolResult) int {
	chars := 0
	for _, result := range results {
		chars += estimateToolResultContentChars(result.Content)
	}
	return chars
}

func estimateToolResultContentChars(contents []agent.Content) int {
	chars := 0
	for _, content := range contents {
		switch block := content.(type) {
		case agent.TextContent:
			chars += len(block.Text)
		case *agent.TextContent:
			chars += len(block.Text)
		case agent.ImageContent, *agent.ImageContent:
			chars += imageChars
		}
	}
	return chars
}

func estimateTextTokens(text string) int {
	if text == "" {
		return 0
	}
	return int(math.Ceil(float64(len(text)) / 4))
}

func estimateTextTokensFromChars(chars int) int {
	if chars <= 0 {
		return 0
	}
	return int(math.Ceil(float64(chars) / 4))
}

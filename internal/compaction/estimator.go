// Package compaction provides context-window compaction helpers.
package compaction

import (
	"encoding/json"
	"math"

	"github.com/noeljackson/pi/internal/agent"
)

const (
	messageEnvelopeTokens = 8
	contentEnvelopeTokens = 4
	imageTokens           = 1200
)

type ContextEstimator struct{}

// EstimateTokens returns an approximate token count for the given messages.
// It uses a cheap 4-chars-per-token heuristic plus envelope overhead.
func (e *ContextEstimator) EstimateTokens(messages []agent.Message, system string) int {
	total := estimateTextTokens(system)
	for _, message := range messages {
		total += estimateMessageTokens(message)
	}
	return total
}

func estimateMessageTokens(message agent.Message) int {
	if message == nil {
		return 0
	}
	total := messageEnvelopeTokens
	switch msg := message.(type) {
	case agent.UserMessage:
		total += estimateContentsTokens(msg.Content)
	case *agent.UserMessage:
		total += estimateContentsTokens(msg.Content)
	case agent.AssistantMessage:
		total += estimateContentsTokens(msg.Content)
	case *agent.AssistantMessage:
		total += estimateContentsTokens(msg.Content)
	case agent.ToolResultMessage:
		for _, result := range msg.Results {
			total += contentEnvelopeTokens + estimateTextTokens(result.ToolUseID)
			total += estimateContentsTokens(result.Content)
		}
	case *agent.ToolResultMessage:
		for _, result := range msg.Results {
			total += contentEnvelopeTokens + estimateTextTokens(result.ToolUseID)
			total += estimateContentsTokens(result.Content)
		}
	case agent.SystemMessage:
		total += estimateContentsTokens(msg.Content)
	case *agent.SystemMessage:
		total += estimateContentsTokens(msg.Content)
	}
	return total
}

func estimateContentsTokens(contents []agent.Content) int {
	total := 0
	for _, content := range contents {
		total += contentEnvelopeTokens + estimateContentTokens(content)
	}
	return total
}

func estimateContentTokens(content agent.Content) int {
	switch block := content.(type) {
	case agent.TextContent:
		return estimateTextTokens(block.Text)
	case *agent.TextContent:
		return estimateTextTokens(block.Text)
	case agent.ThinkingContent:
		return estimateTextTokens(block.Thinking)
	case *agent.ThinkingContent:
		return estimateTextTokens(block.Thinking)
	case agent.ImageContent, *agent.ImageContent:
		return imageTokens
	case agent.ToolUseContent:
		return estimateToolUseTokens(block)
	case *agent.ToolUseContent:
		return estimateToolUseTokens(*block)
	case agent.ToolResultContent:
		return estimateTextTokens(block.ToolUseID) + estimateContentsTokens(block.Content)
	case *agent.ToolResultContent:
		return estimateTextTokens(block.ToolUseID) + estimateContentsTokens(block.Content)
	default:
		return 0
	}
}

func estimateToolUseTokens(block agent.ToolUseContent) int {
	input := string(block.Input)
	if len(block.Input) > 0 {
		var decoded any
		if err := json.Unmarshal(block.Input, &decoded); err == nil {
			if encoded, err := json.Marshal(decoded); err == nil {
				input = string(encoded)
			}
		}
	}
	return estimateTextTokens(block.Name + input)
}

func estimateTextTokens(text string) int {
	if text == "" {
		return 0
	}
	return int(math.Ceil(float64(len(text)) / 4))
}

package compaction

import (
	"encoding/json"
	"fmt"
	"sort"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
)

const toolResultMaxChars = 2000

func serializeConversation(messages []agent.Message) string {
	parts := make([]string, 0, len(messages))
	for _, message := range messages {
		switch msg := message.(type) {
		case agent.UserMessage:
			if content := serializeContentText(msg.Content); content != "" {
				parts = append(parts, "[User]: "+content)
			}
		case *agent.UserMessage:
			if content := serializeContentText(msg.Content); content != "" {
				parts = append(parts, "[User]: "+content)
			}
		case agent.AssistantMessage:
			parts = append(parts, serializeAssistant(msg.Content)...)
		case *agent.AssistantMessage:
			parts = append(parts, serializeAssistant(msg.Content)...)
		case agent.ToolResultMessage:
			parts = append(parts, serializeToolResults(msg.Results)...)
		case *agent.ToolResultMessage:
			parts = append(parts, serializeToolResults(msg.Results)...)
		case agent.SystemMessage:
			if content := serializeContentText(msg.Content); content != "" {
				parts = append(parts, "[System]: "+content)
			}
		case *agent.SystemMessage:
			if content := serializeContentText(msg.Content); content != "" {
				parts = append(parts, "[System]: "+content)
			}
		}
	}
	return strings.Join(parts, "\n\n")
}

func serializeAssistant(contents []agent.Content) []string {
	textParts := make([]string, 0)
	thinkingParts := make([]string, 0)
	toolCalls := make([]string, 0)
	for _, content := range contents {
		switch block := content.(type) {
		case agent.TextContent:
			textParts = append(textParts, block.Text)
		case *agent.TextContent:
			textParts = append(textParts, block.Text)
		case agent.ThinkingContent:
			thinkingParts = append(thinkingParts, block.Thinking)
		case *agent.ThinkingContent:
			thinkingParts = append(thinkingParts, block.Thinking)
		case agent.ToolUseContent:
			toolCalls = append(toolCalls, serializeToolUse(block))
		case *agent.ToolUseContent:
			toolCalls = append(toolCalls, serializeToolUse(*block))
		}
	}

	parts := make([]string, 0, 3)
	if len(thinkingParts) > 0 {
		parts = append(parts, "[Assistant thinking]: "+strings.Join(thinkingParts, "\n"))
	}
	if len(textParts) > 0 {
		parts = append(parts, "[Assistant]: "+strings.Join(textParts, "\n"))
	}
	if len(toolCalls) > 0 {
		parts = append(parts, "[Assistant tool calls]: "+strings.Join(toolCalls, "; "))
	}
	return parts
}

func serializeToolUse(block agent.ToolUseContent) string {
	args := string(block.Input)
	if len(block.Input) > 0 {
		var decoded map[string]any
		if err := json.Unmarshal(block.Input, &decoded); err == nil {
			pairs := make([]string, 0, len(decoded))
			keys := make([]string, 0, len(decoded))
			for key := range decoded {
				keys = append(keys, key)
			}
			sort.Strings(keys)
			for _, key := range keys {
				value := decoded[key]
				encoded, err := json.Marshal(value)
				if err != nil {
					continue
				}
				pairs = append(pairs, fmt.Sprintf("%s=%s", key, encoded))
			}
			args = strings.Join(pairs, ", ")
		}
	}
	return fmt.Sprintf("%s(%s)", block.Name, args)
}

func serializeToolResults(results []agent.ToolResult) []string {
	parts := make([]string, 0, len(results))
	for _, result := range results {
		content := truncateForSummary(serializeContentText(result.Content), toolResultMaxChars)
		if content != "" {
			parts = append(parts, "[Tool result]: "+content)
		}
	}
	return parts
}

func serializeContentText(contents []agent.Content) string {
	parts := make([]string, 0, len(contents))
	for _, content := range contents {
		switch text := content.(type) {
		case agent.TextContent:
			parts = append(parts, text.Text)
		case *agent.TextContent:
			parts = append(parts, text.Text)
		}
	}
	return strings.Join(parts, "")
}

func truncateForSummary(text string, maxChars int) string {
	if len(text) <= maxChars {
		return text
	}
	truncated := len(text) - maxChars
	return fmt.Sprintf("%s\n\n[... %d more characters truncated]", text[:maxChars], truncated)
}

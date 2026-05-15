package compaction

import "github.com/noeljackson/pi/internal/agent"

// FindCutPoint returns the index in messages such that messages[:cut] are
// summarized and messages[cut:] are preserved verbatim. It only cuts after a
// completed turn, never between an assistant tool use and its tool result.
func FindCutPoint(messages []agent.Message, targetTokens int) int {
	if targetTokens <= 0 || len(messages) == 0 {
		return 0
	}

	safeCuts := make([]int, 0, len(messages))
	for i, message := range messages {
		if isSafeCutMessage(message) {
			safeCuts = append(safeCuts, i+1)
		}
	}
	if len(safeCuts) == 0 {
		return 0
	}

	accumulated := 0
	thresholdIndex := 0
	for i := len(messages) - 1; i >= 0; i-- {
		accumulated += estimateMessageTokens(messages[i])
		thresholdIndex = i
		if accumulated >= targetTokens {
			break
		}
	}

	cut := 0
	for _, candidate := range safeCuts {
		if candidate <= thresholdIndex {
			cut = candidate
			continue
		}
		break
	}
	return cut
}

func isSafeCutMessage(message agent.Message) bool {
	switch msg := message.(type) {
	case agent.AssistantMessage:
		return !hasToolUse(msg.Content)
	case *agent.AssistantMessage:
		return !hasToolUse(msg.Content)
	case agent.ToolResultMessage, *agent.ToolResultMessage:
		return true
	default:
		return false
	}
}

func hasToolUse(contents []agent.Content) bool {
	for _, content := range contents {
		switch content.(type) {
		case agent.ToolUseContent, *agent.ToolUseContent:
			return true
		}
	}
	return false
}

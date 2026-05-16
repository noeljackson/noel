package compaction

import "github.com/noeljackson/pi/internal/agent"

type CutPointResult struct {
	CutIndex       int
	TurnStartIndex int
	IsSplitTurn    bool
}

// FindCutPoint returns the index in messages such that messages[:cut] are
// summarized and messages[cut:] are preserved verbatim.
func FindCutPoint(messages []agent.Message, targetTokens int) int {
	return FindCutPointResult(messages, targetTokens).CutIndex
}

// FindCutPointResult finds the suffix boundary while preserving valid turn and
// tool-use structure.
func FindCutPointResult(messages []agent.Message, targetTokens int) CutPointResult {
	if targetTokens <= 0 || len(messages) == 0 {
		return CutPointResult{}
	}

	cutPoints := make([]int, 0, len(messages))
	for i, message := range messages {
		if isValidCutPoint(messages, i, message) {
			cutPoints = append(cutPoints, i)
		}
	}
	if len(cutPoints) == 0 {
		return CutPointResult{}
	}

	accumulated := 0
	thresholdIndex := cutPoints[0]
	for i := len(messages) - 1; i >= 0; i-- {
		accumulated += estimateMessageTokens(messages[i])
		thresholdIndex = i
		if accumulated >= targetTokens {
			break
		}
	}

	cut := firstCutAtOrAfter(cutPoints, messages, thresholdIndex)
	turnStart := -1
	if !isTurnStart(messages[cut]) {
		turnStart = findTurnStartIndex(messages, cut)
	}
	return CutPointResult{
		CutIndex:       cut,
		TurnStartIndex: turnStart,
		IsSplitTurn:    turnStart >= 0 && turnStart < cut,
	}
}

func firstCutAtOrAfter(cutPoints []int, messages []agent.Message, thresholdIndex int) int {
	first := cutPoints[0]
	lastBefore := cutPoints[0]
	for _, candidate := range cutPoints {
		if candidate < thresholdIndex {
			lastBefore = candidate
			continue
		}
		first = candidate
		if isTurnStart(messages[candidate]) {
			return candidate
		}
	}
	if first < thresholdIndex {
		return lastBefore
	}
	return first
}

func isValidCutPoint(messages []agent.Message, index int, message agent.Message) bool {
	if message == nil || isToolResult(message) {
		return false
	}
	if isAssistant(message) && index > 0 && isBashExecution(messages[index-1]) {
		return false
	}
	return isTurnStart(message) || isAssistant(message) || isSummary(message) || isCustom(message)
}

func isTurnStart(message agent.Message) bool {
	return isUser(message) || isBashExecution(message)
}

func isUser(message agent.Message) bool {
	switch message.(type) {
	case agent.UserMessage, *agent.UserMessage:
		return true
	default:
		return false
	}
}

func isAssistant(message agent.Message) bool {
	switch message.(type) {
	case agent.AssistantMessage, *agent.AssistantMessage:
		return true
	default:
		return false
	}
}

func isToolResult(message agent.Message) bool {
	switch message.(type) {
	case agent.ToolResultMessage, *agent.ToolResultMessage:
		return true
	default:
		return false
	}
}

func isBashExecution(message agent.Message) bool {
	switch message.(type) {
	case agent.BashExecutionMessage, *agent.BashExecutionMessage:
		return true
	default:
		return false
	}
}

func isCustom(message agent.Message) bool {
	switch message.(type) {
	case agent.CustomMessage, *agent.CustomMessage:
		return true
	default:
		return false
	}
}

func isSummary(message agent.Message) bool {
	switch message.(type) {
	case agent.BranchSummaryMessage, *agent.BranchSummaryMessage, agent.CompactionSummaryMessage, *agent.CompactionSummaryMessage:
		return true
	default:
		return false
	}
}

func findTurnStartIndex(messages []agent.Message, index int) int {
	for i := index; i >= 0; i-- {
		if isTurnStart(messages[i]) {
			return i
		}
	}
	return -1
}

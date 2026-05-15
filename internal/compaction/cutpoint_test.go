package compaction

import (
	"encoding/json"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestFindCutPointDoesNotSplitToolUseAndResult(t *testing.T) {
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "please inspect the file"}}},
		agent.AssistantMessage{Content: []agent.Content{
			agent.TextContent{Text: "I will read it"},
			agent.ToolUseContent{ID: "call-1", Name: "read", Input: json.RawMessage(`{"path":"README.md"}`)},
		}},
		agent.ToolResultMessage{Results: []agent.ToolResult{{
			ToolUseID: "call-1",
			Content:   []agent.Content{agent.TextContent{Text: "file contents"}},
		}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "now summarize it with the important details"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "summary with enough text to cross the target"}}},
	}

	cut := FindCutPoint(messages, 25)
	if cut != 3 {
		t.Fatalf("expected cut after tool result at index 3, got %d", cut)
	}
}

func TestFindCutPointCutsAfterAssistantWithoutToolCalls(t *testing.T) {
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "first request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "first answer"}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "second request with additional detail"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "second answer with additional detail"}}},
	}

	cut := FindCutPoint(messages, 20)
	if cut != 2 {
		t.Fatalf("expected cut after first assistant at index 2, got %d", cut)
	}
}

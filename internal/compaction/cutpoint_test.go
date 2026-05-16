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

func TestFindCutPointNeverStartsAtToolResult(t *testing.T) {
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "old"}}},
		agent.AssistantMessage{Content: []agent.Content{
			agent.ToolUseContent{ID: "call-1", Name: "read", Input: json.RawMessage(`{"path":"README.md"}`)},
		}},
		agent.ToolResultMessage{Results: []agent.ToolResult{{ToolUseID: "call-1"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "recent answer with enough detail"}}},
	}

	cut := FindCutPoint(messages, 10)
	if cut == 2 {
		t.Fatal("cut point started at a tool result")
	}
}

func TestFindCutPointDoesNotSplitBashExecutionGroup(t *testing.T) {
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "old request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "old answer"}}},
		agent.BashExecutionMessage{Command: "go test ./...", Output: "ok"},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "recent answer after bash output"}}},
	}

	cut := FindCutPoint(messages, 8)
	if cut != 2 {
		t.Fatalf("expected cut at bash execution message, got %d", cut)
	}
}

func TestFindCutPointPrefersUserBoundary(t *testing.T) {
	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "first request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "first answer"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "extra assistant context"}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "second request"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "second answer with detail"}}},
	}

	cut := FindCutPoint(messages, 10)
	if cut != 3 {
		t.Fatalf("expected user boundary cut at 3, got %d", cut)
	}
}

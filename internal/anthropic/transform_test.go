package anthropic

import (
	"encoding/json"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestSanitizeUnicodeStripsSurrogates(t *testing.T) {
	got := SanitizeUnicode("a" + string([]byte{0xed, 0xa0, 0x80}) + "b" + string([]byte{0xed, 0xb0, 0x80}) + "c")
	if got != "abc" {
		t.Fatalf("SanitizeUnicode() = %q, want %q", got, "abc")
	}
}

func TestRepairJSONClosesUnbalancedValues(t *testing.T) {
	repaired := RepairJSON(`{"cmd":"echo`)
	var decoded map[string]string
	if err := json.Unmarshal([]byte(repaired), &decoded); err != nil {
		t.Fatalf("repaired JSON is invalid: %v; %q", err, repaired)
	}
	if decoded["cmd"] != "echo" {
		t.Fatalf("cmd = %q, want echo", decoded["cmd"])
	}
}

func TestEnsureSyntheticToolResults(t *testing.T) {
	messages := []agent.Message{
		agent.AssistantMessage{
			Content: []agent.Content{
				agent.ToolUseContent{ID: "call-1", Name: "bash", Input: json.RawMessage(`{"command":"true"}`)},
			},
			StopReason: agent.StopToolUse,
		},
	}
	got := EnsureSyntheticToolResults(messages)
	if len(got) != 2 {
		t.Fatalf("len = %d, want 2", len(got))
	}
	result, ok := got[1].(agent.ToolResultMessage)
	if !ok {
		t.Fatalf("second message = %T, want ToolResultMessage", got[1])
	}
	if len(result.Results) != 1 || result.Results[0].ToolUseID != "call-1" || !result.Results[0].IsError {
		t.Fatalf("synthetic result = %#v", result.Results)
	}
	text, ok := result.Results[0].Content[0].(agent.TextContent)
	if !ok || text.Text != "No result provided" {
		t.Fatalf("synthetic content = %#v", result.Results[0].Content)
	}
}

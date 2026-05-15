package bash

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestBashExitCodePropagation(t *testing.T) {
	tool := NewTool()
	result, err := tool.Execute(context.Background(), json.RawMessage(`{"command":"echo out; echo err >&2; exit 7"}`), agent.ToolCallContext{})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !result.IsError {
		t.Fatalf("expected non-zero exit to mark result as error")
	}

	var details struct {
		ExitCode int `json:"exitCode"`
	}
	if err := json.Unmarshal(result.Details, &details); err != nil {
		t.Fatalf("unmarshal details: %v", err)
	}
	if details.ExitCode != 7 {
		t.Fatalf("exit code = %d, want 7", details.ExitCode)
	}
	if textContent(t, result) != "out\nerr\n" {
		t.Fatalf("content = %q, want combined output", textContent(t, result))
	}
}

func TestBashTimeout(t *testing.T) {
	tool := NewTool()
	result, err := tool.Execute(context.Background(), json.RawMessage(`{"command":"sleep 2","timeout_ms":100}`), agent.ToolCallContext{})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !result.IsError {
		t.Fatalf("expected timeout to mark result as error")
	}

	var details struct {
		ExitCode int `json:"exitCode"`
	}
	if err := json.Unmarshal(result.Details, &details); err != nil {
		t.Fatalf("unmarshal details: %v", err)
	}
	if details.ExitCode != -1 {
		t.Fatalf("exit code = %d, want -1", details.ExitCode)
	}
}

func textContent(t *testing.T, result agent.ToolResult) string {
	t.Helper()
	if len(result.Content) != 1 {
		t.Fatalf("content length = %d, want 1", len(result.Content))
	}
	text, ok := result.Content[0].(agent.TextContent)
	if !ok {
		t.Fatalf("content type = %T, want agent.TextContent", result.Content[0])
	}
	return text.Text
}

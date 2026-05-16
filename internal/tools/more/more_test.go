package more

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestMoreRetrievesByCallID(t *testing.T) {
	buffer := NewDiskBuffer(t.TempDir())
	if err := buffer.Put("call-1", "one\ntwo\nthree"); err != nil {
		t.Fatalf("Put returned error: %v", err)
	}
	tool := NewTool(buffer)

	result, err := tool.Execute(context.Background(), json.RawMessage(`{"call_id":"call-1"}`), agent.ToolCallContext{CallID: "more-1"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if got := textContent(t, result); got != "one\ntwo\nthree" {
		t.Fatalf("content = %q", got)
	}
}

func TestMoreOffsetLimit(t *testing.T) {
	buffer := NewDiskBuffer(t.TempDir())
	if err := buffer.Put("call-1", "one\ntwo\nthree\nfour"); err != nil {
		t.Fatalf("Put returned error: %v", err)
	}
	tool := NewTool(buffer)

	result, err := tool.Execute(context.Background(), json.RawMessage(`{"call_id":"call-1","offset":1,"limit":2}`), agent.ToolCallContext{CallID: "more-1"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if got := textContent(t, result); got != "two\nthree" {
		t.Fatalf("content = %q", got)
	}
}

func TestMoreMissingCallIDReturnsClearError(t *testing.T) {
	tool := NewTool(NewDiskBuffer(t.TempDir()))
	result, err := tool.Execute(context.Background(), json.RawMessage(`{"call_id":"missing"}`), agent.ToolCallContext{CallID: "more-1"})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !result.IsError {
		t.Fatalf("expected missing call_id to mark result error")
	}
	if !strings.Contains(textContent(t, result), "No buffered output found") {
		t.Fatalf("content = %q", textContent(t, result))
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

package file

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestReadOffsetLimit(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "file.txt"), []byte("a\nb\nc\nd\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","offset":2,"limit":2}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	got := toolText(t, result)
	want := "     2\tb\n     3\tc"
	if got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestReadOversize(t *testing.T) {
	dir := t.TempDir()
	content := strings.Repeat("x", maxReadBytes+1)
	if err := os.WriteFile(filepath.Join(dir, "large.txt"), []byte(content), 0o666); err != nil {
		t.Fatal(err)
	}

	_, err := NewReadTool().Execute(context.Background(), json.RawMessage(`{"path":"large.txt"}`), agent.ToolCallContext{Cwd: dir})
	if err == nil {
		t.Fatalf("expected oversize error")
	}
}

func toolText(t *testing.T, result agent.ToolResult) string {
	t.Helper()
	text, ok := result.Content[0].(agent.TextContent)
	if !ok {
		t.Fatalf("content type = %T, want agent.TextContent", result.Content[0])
	}
	return text.Text
}

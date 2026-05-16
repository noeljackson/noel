package file

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestGrepIgnoreCaseContextAndLimit(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "a.txt"), []byte("before\nNeedle\nafter\nneedle again\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	result, err := NewGrepTool().Execute(context.Background(), json.RawMessage(`{"pattern":"needle","path":".","ignoreCase":true,"context":1,"limit":1}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	got := toolText(t, result)
	if !strings.Contains(got, "a.txt-1- before") || !strings.Contains(got, "a.txt:2: Needle") || !strings.Contains(got, "1 matches limit reached") {
		t.Fatalf("content = %q", got)
	}
}

func TestGrepOutputModes(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "a.txt"), []byte("hit\nhit\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "b.txt"), []byte("miss\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		name string
		args string
		want string
	}{
		{name: "content", args: `{"pattern":"hit","path":".","output_mode":"content"}`, want: "a.txt:1: hit"},
		{name: "files", args: `{"pattern":"hit","path":".","output_mode":"files_with_matches"}`, want: "a.txt"},
		{name: "count", args: `{"pattern":"hit","path":".","output_mode":"count"}`, want: "a.txt:2"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result, err := NewGrepTool().Execute(context.Background(), json.RawMessage(tt.args), agent.ToolCallContext{Cwd: dir})
			if err != nil {
				t.Fatalf("Execute returned error: %v", err)
			}
			if !strings.Contains(toolText(t, result), tt.want) {
				t.Fatalf("content = %q, want %q", toolText(t, result), tt.want)
			}
		})
	}
}

func TestGrepLongLineTruncation(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "a.txt"), []byte("needle "+strings.Repeat("x", 700)+"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	result, err := NewGrepTool().Execute(context.Background(), json.RawMessage(`{"pattern":"needle","path":"."}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !strings.Contains(toolText(t, result), "... [truncated]") {
		t.Fatalf("content = %q, want long line truncation", toolText(t, result))
	}
}

func TestGrepUsesRipgrepJSONWhenAvailable(t *testing.T) {
	if _, err := exec.LookPath("rg"); err != nil {
		t.Skip("rg not installed")
	}
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "a.txt"), []byte("Needle\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	result, err := NewGrepTool().Execute(context.Background(), json.RawMessage(`{"pattern":"needle","path":".","ignoreCase":true}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !strings.Contains(toolText(t, result), "a.txt:1: Needle") {
		t.Fatalf("content = %q", toolText(t, result))
	}
}

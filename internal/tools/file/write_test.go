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

func TestWriteAtomicRename(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "nested", "file.txt")
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("old"), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewWriteTool().Execute(context.Background(), json.RawMessage(`{"path":"nested/file.txt","content":"new content"}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("result unexpectedly marked as error")
	}

	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "new content" {
		t.Fatalf("content = %q", string(content))
	}

	matches, err := filepath.Glob(path + ".tmp-*")
	if err != nil {
		t.Fatal(err)
	}
	if len(matches) != 0 {
		t.Fatalf("temporary files remain: %v", matches)
	}
	if !strings.Contains(toolText(t, result), "Successfully wrote 11 bytes to nested/file.txt") {
		t.Fatalf("content = %q", toolText(t, result))
	}
	var details struct {
		Path  string `json:"path"`
		Bytes int    `json:"bytes"`
		Lines int    `json:"lines"`
	}
	if err := json.Unmarshal(result.Details, &details); err != nil {
		t.Fatalf("unmarshal details: %v", err)
	}
	if details.Bytes != 11 || details.Lines != 1 {
		t.Fatalf("details = %#v", details)
	}
}

func TestWriteCreatesParentDirectories(t *testing.T) {
	dir := t.TempDir()
	_, err := NewWriteTool().Execute(context.Background(), json.RawMessage(`{"path":"a/b/c.txt","content":"created"}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	content, err := os.ReadFile(filepath.Join(dir, "a", "b", "c.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "created" {
		t.Fatalf("content = %q", string(content))
	}
}

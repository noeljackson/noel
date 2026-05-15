package file

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
)

func TestEditMultiOccurrenceError(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "file.txt")
	if err := os.WriteFile(path, []byte("same\nsame\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	_, err := NewEditTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","old_string":"same","new_string":"other"}`), agent.ToolCallContext{Cwd: dir})
	if err == nil {
		t.Fatalf("expected multi-occurrence error")
	}
}

func TestEditReplaceAll(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "file.txt")
	if err := os.WriteFile(path, []byte("same\nsame\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewEditTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","old_string":"same","new_string":"other","replace_all":true}`), agent.ToolCallContext{Cwd: dir})
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
	if string(content) != "other\nother\n" {
		t.Fatalf("content = %q", string(content))
	}
}

func TestEditPreservesLineEndings(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "file.txt")
	if err := os.WriteFile(path, []byte("one\r\ntwo\r\nthree\r\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	_, err := NewEditTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","old_string":"two\nthree","new_string":"dos\ntres"}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "one\r\ndos\r\ntres\r\n" {
		t.Fatalf("content = %q", string(content))
	}
}

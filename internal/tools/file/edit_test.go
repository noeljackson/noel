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

func TestEditMultiEditSchema(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "file.txt")
	if err := os.WriteFile(path, []byte("one\ntwo\nthree\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewEditTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","edits":[{"oldText":"one","newText":"uno"},{"oldText":"three","newText":"tres"}]}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "uno\ntwo\ntres\n" {
		t.Fatalf("content = %q", string(content))
	}
	var details struct {
		Hunks []struct {
			Method string `json:"method"`
			Diff   string `json:"diff"`
		} `json:"hunks"`
	}
	if err := json.Unmarshal(result.Details, &details); err != nil {
		t.Fatalf("unmarshal details: %v", err)
	}
	if len(details.Hunks) == 0 || !strings.Contains(details.Hunks[0].Diff, "-one") || !strings.Contains(details.Hunks[0].Diff, "+uno") {
		t.Fatalf("details hunks = %#v", details.Hunks)
	}
}

func TestEditFuzzyWhitespaceFallback(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "file.txt")
	if err := os.WriteFile(path, []byte("alpha    beta\ngamma\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	result, err := NewEditTool().Execute(context.Background(), json.RawMessage(`{"path":"file.txt","edits":[{"oldText":"alpha beta","newText":"alpha beta updated"}]}`), agent.ToolCallContext{Cwd: dir})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "alpha beta updated\ngamma\n" {
		t.Fatalf("content = %q", string(content))
	}
	var details struct {
		Hunks []struct {
			Method string `json:"method"`
		} `json:"hunks"`
	}
	if err := json.Unmarshal(result.Details, &details); err != nil {
		t.Fatalf("unmarshal details: %v", err)
	}
	if len(details.Hunks) == 0 || details.Hunks[0].Method != "fuzzy" {
		t.Fatalf("method = %#v, want fuzzy", details.Hunks)
	}
}

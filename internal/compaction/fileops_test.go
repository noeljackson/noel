package compaction

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/tools"
)

func TestExtractFileOpsFromAssistantToolCalls(t *testing.T) {
	message := agent.AssistantMessage{Content: []agent.Content{
		agent.ToolUseContent{ID: "read-1", Name: "read", Input: json.RawMessage(`{"path":"a.txt"}`)},
		agent.ToolUseContent{ID: "write-1", Name: "write", Input: json.RawMessage(`{"path":"b.txt"}`)},
		agent.ToolUseContent{ID: "edit-1", Name: "edit", Input: json.RawMessage(`{"path":"c.txt"}`)},
		agent.ToolUseContent{ID: "bash-1", Name: "bash", Input: json.RawMessage(`{"command":"go test ./..."}`)},
	}}

	got := ExtractFileOps(message)
	if len(got) != 4 {
		t.Fatalf("ops length = %d, want 4: %#v", len(got), got)
	}
	want := []FileOperation{
		{Path: "a.txt", Tool: "read", Action: "read"},
		{Path: "b.txt", Tool: "write", Action: "write"},
		{Path: "c.txt", Tool: "edit", Action: "modify"},
		{Path: "go test ./...", Tool: "bash", Action: "modify"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("ops = %#v, want %#v", got, want)
	}
}

func TestExtractFileOpsFromToolResultDetails(t *testing.T) {
	readDetails, _ := tools.MarshalDetails(tools.ReadDetails{Path: "a.txt", Bytes: 12, Lines: 1})
	writeDetails, _ := tools.MarshalDetails(tools.WriteDetails{Path: "b.txt", Bytes: 20, Lines: 2})
	editDetails, _ := tools.MarshalDetails(tools.EditDetails{Path: "c.txt", EditsApplied: 1})
	bashDetails, _ := tools.MarshalDetails(tools.BashDetails{Command: "make check", StdoutBytes: 5, StderrBytes: 7})
	message := agent.ToolResultMessage{Results: []agent.ToolResult{
		{Details: readDetails},
		{Details: writeDetails},
		{Details: editDetails},
		{Details: bashDetails},
	}}

	got := ExtractFileOps(message)
	if len(got) != 4 {
		t.Fatalf("ops length = %d, want 4: %#v", len(got), got)
	}
	if got[0].Tool != "read" || got[1].Tool != "write" || got[2].Tool != "edit" || got[3].Tool != "bash" {
		t.Fatalf("unexpected ops: %#v", got)
	}
	if got[3].Bytes != 12 {
		t.Fatalf("bash bytes = %d, want 12", got[3].Bytes)
	}
}

func TestComputeFileListsWithCap(t *testing.T) {
	ops := []FileOperation{
		{Path: "old-read.txt", Action: "read"},
		{Path: "read.txt", Action: "read"},
		{Path: "modified.txt", Action: "read"},
		{Path: "modified.txt", Action: "modify"},
		{Path: "new-read.txt", Action: "read"},
		{Path: "write.txt", Action: "write"},
	}

	readOnly, modified := ComputeFileLists(ops, 2)
	if !reflect.DeepEqual(readOnly, []string{"new-read.txt", "read.txt"}) {
		t.Fatalf("readOnly = %#v", readOnly)
	}
	if !reflect.DeepEqual(modified, []string{"modified.txt", "write.txt"}) {
		t.Fatalf("modified = %#v", modified)
	}
}

func TestFormatFileOperations(t *testing.T) {
	got := FormatFileOperations([]string{"a.txt"}, []string{"b.txt"})
	if !strings.Contains(got, "<read-files>\na.txt\n</read-files>") {
		t.Fatalf("missing read files: %q", got)
	}
	if !strings.Contains(got, "<modified-files>\nb.txt\n</modified-files>") {
		t.Fatalf("missing modified files: %q", got)
	}
}

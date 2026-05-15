package bash

import (
	"context"
	"encoding/json"
	"os"
	"strings"
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
	result, err := tool.Execute(context.Background(), json.RawMessage(`{"command":"sleep 2","timeout":1}`), agent.ToolCallContext{})
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
	if !strings.Contains(textContent(t, result), "Command timed out after 1 seconds") {
		t.Fatalf("content = %q, want seconds timeout message", textContent(t, result))
	}
}

func TestBashTimeoutMSBackCompat(t *testing.T) {
	tool := NewTool()
	result, err := tool.Execute(context.Background(), json.RawMessage(`{"command":"sleep 2","timeout_ms":100}`), agent.ToolCallContext{})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	if !result.IsError {
		t.Fatalf("expected timeout to mark result as error")
	}
}

func TestBashFullOutputFileAndStreamDetails(t *testing.T) {
	tool := NewTool()
	result, err := tool.Execute(context.Background(), json.RawMessage(`{"command":"python3 -c 'import sys; print(\"o\"*60000); print(\"e\"*60000, file=sys.stderr)'","timeout":5}`), agent.ToolCallContext{})
	if err != nil {
		t.Fatalf("Execute returned error: %v", err)
	}
	var details struct {
		Stdout      string `json:"stdout"`
		Stderr      string `json:"stderr"`
		StdoutBytes int    `json:"stdoutBytes"`
		StderrBytes int    `json:"stderrBytes"`
		OutputFile  string `json:"outputFile"`
	}
	if err := json.Unmarshal(result.Details, &details); err != nil {
		t.Fatalf("unmarshal details: %v", err)
	}
	if details.OutputFile == "" {
		t.Fatalf("expected output file")
	}
	full, err := os.ReadFile(details.OutputFile)
	if err != nil {
		t.Fatalf("read output file: %v", err)
	}
	if !strings.Contains(string(full), strings.Repeat("o", 1000)) || !strings.Contains(string(full), strings.Repeat("e", 1000)) {
		t.Fatalf("full output file missing stdout/stderr content")
	}
	if details.Stdout == "" || details.Stderr == "" {
		t.Fatalf("stdout/stderr details were not separated: %#v", details)
	}
	if details.StdoutBytes == 0 || details.StderrBytes == 0 {
		t.Fatalf("stdout/stderr byte counts missing: %#v", details)
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

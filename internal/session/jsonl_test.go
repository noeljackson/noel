package session

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

func TestJSONLSessionRoundTripMessages(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	session, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()

	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{
			agent.TextContent{Text: "hello"},
			agent.ImageContent{Source: agent.ImageSource{Type: "base64", MediaType: "image/png", Data: "abcd"}},
		}},
		agent.AssistantMessage{
			Content: []agent.Content{
				agent.ThinkingContent{Thinking: "working", Signature: "sig"},
				agent.TextContent{Text: "use tool"},
				agent.ToolUseContent{ID: "call-1", Name: "read", Input: json.RawMessage(`{"path":"README.md"}`)},
			},
			StopReason: "tool_use",
			Model:      "test-model",
			Usage:      agent.Usage{InputTokens: 10, OutputTokens: 5},
		},
		agent.ToolResultMessage{Results: []agent.ToolResult{{
			ToolUseID: "call-1",
			Content:   []agent.Content{agent.TextContent{Text: "contents"}},
			Details:   json.RawMessage(`{"duration_ms":1}`),
		}}},
		agent.SystemMessage{Content: []agent.Content{agent.TextContent{Text: "system"}}},
	}

	for _, message := range messages {
		if err := session.AppendMessage(message); err != nil {
			t.Fatal(err)
		}
	}
	if err := session.AppendRunState(map[string]string{"phase": "test"}, ""); err != nil {
		t.Fatal(err)
	}
	if err := session.AppendCompaction("summary", ""); err != nil {
		t.Fatal(err)
	}
	if err := session.AppendSavePoint(""); err != nil {
		t.Fatal(err)
	}

	got, err := session.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, messages) {
		t.Fatalf("messages mismatch\n got: %#v\nwant: %#v", got, messages)
	}
}

func TestJSONLSessionToleratesTrailingPartialLine(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	session, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	want := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "persisted"}}},
	}
	if err := session.AppendMessage(want[0]); err != nil {
		t.Fatal(err)
	}
	if err := session.Close(); err != nil {
		t.Fatal(err)
	}

	file, err := os.OpenFile(session.path, os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteString(`{"type":"message","id":"partial"`); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, err := store.Open(session.ID())
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()

	got, err := reopened.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("messages mismatch\n got: %#v\nwant: %#v", got, want)
	}
}

func TestInterruptedTurnDetectsPendingToolCalls(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	session, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()

	if err := session.AppendMessage(agent.AssistantMessage{Content: []agent.Content{
		agent.TextContent{Text: "checking"},
		agent.ToolUseContent{ID: "call-1", Name: "read", Input: json.RawMessage(`{"path":"README.md"}`)},
	}}); err != nil {
		t.Fatal(err)
	}

	interrupted, err := session.InterruptedTurn()
	if err != nil {
		t.Fatal(err)
	}
	if interrupted == nil {
		t.Fatal("expected interrupted turn")
	}
	if interrupted.LastAssistantMessage == nil {
		t.Fatal("expected last assistant message")
	}
	if len(interrupted.PendingToolCalls) != 1 || interrupted.PendingToolCalls[0].ID != "call-1" {
		t.Fatalf("unexpected pending tool calls: %#v", interrupted.PendingToolCalls)
	}
}

func TestInterruptedTurnCompleteTurnReturnsNil(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	session, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()

	if err := session.AppendMessage(agent.AssistantMessage{Content: []agent.Content{
		agent.ToolUseContent{ID: "call-1", Name: "read", Input: json.RawMessage(`{"path":"README.md"}`)},
	}}); err != nil {
		t.Fatal(err)
	}
	if err := session.AppendMessage(agent.ToolResultMessage{Results: []agent.ToolResult{{
		ToolUseID: "call-1",
		Content:   []agent.Content{agent.TextContent{Text: "ok"}},
	}}}); err != nil {
		t.Fatal(err)
	}
	if err := session.AppendSavePoint(""); err != nil {
		t.Fatal(err)
	}

	interrupted, err := session.InterruptedTurn()
	if err != nil {
		t.Fatal(err)
	}
	if interrupted != nil {
		t.Fatalf("expected no interrupted turn, got %#v", interrupted)
	}
}

func TestCurrentTurnSidecarIgnoresPartialTempWrite(t *testing.T) {
	dir := t.TempDir()
	store := NewJSONLStore(dir)
	session, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()

	oldTurn := CurrentTurn{PartialText: "old"}
	if err := session.WriteCurrentTurn(oldTurn); err != nil {
		t.Fatal(err)
	}
	time.Sleep(currentTurnWriteInterval)
	newTurn := CurrentTurn{
		PartialText: "new",
		PartialToolUse: &agent.ToolUseContent{
			ID:    "call-1",
			Name:  "read",
			Input: json.RawMessage(`{"path":"README.md"}`),
		},
	}
	if err := session.WriteCurrentTurn(newTurn); err != nil {
		t.Fatal(err)
	}

	tmpPath := filepath.Join(dir, session.ID()+".current-turn.json.tmp-deadbeef")
	if err := os.WriteFile(tmpPath, []byte(`{"partial_text":`), 0o600); err != nil {
		t.Fatal(err)
	}

	interrupted, err := session.InterruptedTurn()
	if err != nil {
		t.Fatal(err)
	}
	if interrupted == nil {
		t.Fatal("expected sidecar interrupted turn")
	}
	if interrupted.PartialText != oldTurn.PartialText && interrupted.PartialText != newTurn.PartialText {
		t.Fatalf("unexpected partial text %q", interrupted.PartialText)
	}
	if interrupted.PartialText == newTurn.PartialText && interrupted.PartialToolUse == nil {
		t.Fatal("expected new sidecar tool use")
	}
}

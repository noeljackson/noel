package session

import (
	"encoding/json"
	"reflect"
	"testing"
	"time"

	"github.com/noeljackson/pi/internal/agent"
)

func TestSessionNewRecordTypesRoundTrip(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()

	ts := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	if err := sess.AppendMessage(agent.UserMessage{Timestamp: ts, Content: []agent.Content{
		agent.TextContent{Text: "hello", CacheControl: &agent.CacheControl{Type: "ephemeral"}},
		agent.ImageContent{Source: agent.ImageSource{Type: "base64", MediaType: "image/png", Data: "abcd"}},
	}}); err != nil {
		t.Fatal(err)
	}
	if err := sess.AppendThinkingChange("high"); err != nil {
		t.Fatal(err)
	}
	if err := sess.AppendModelChange("claude-opus-4-1", "anthropic", "anthropic-messages", "user"); err != nil {
		t.Fatal(err)
	}
	customData := json.RawMessage(`{"ok":true}`)
	if err := sess.AppendCustomEntry("audit", customData); err != nil {
		t.Fatal(err)
	}
	customMessage := agent.CustomMessage{
		Timestamp: ts.Add(time.Second),
		Kind:      "notice",
		Content:   []agent.Content{agent.TextContent{Text: "custom visible"}},
		Display:   true,
		Details:   json.RawMessage(`{"severity":"info"}`),
	}
	if err := sess.AppendCustomMessage(customMessage); err != nil {
		t.Fatal(err)
	}
	exitCode := 7
	bash := agent.BashExecutionMessage{
		Timestamp:      ts.Add(2 * time.Second),
		Command:        "go test ./...",
		Output:         "failed",
		ExitCode:       &exitCode,
		Truncated:      true,
		FullOutputPath: "/tmp/full-output",
	}
	if err := sess.AppendBashExecution(bash); err != nil {
		t.Fatal(err)
	}
	if err := sess.AppendSessionName("  Schema phase  "); err != nil {
		t.Fatal(err)
	}

	path, err := sess.PathToCurrentLeaf()
	if err != nil {
		t.Fatal(err)
	}
	seen := map[string]bool{}
	for _, record := range path {
		seen[record.Type] = true
	}
	for _, recordType := range []string{
		RecordTypeMessage,
		RecordTypeThinkingChange,
		RecordTypeModelChange,
		RecordTypeCustomEntry,
		RecordTypeCustomMessage,
		RecordTypeBashExecution,
		RecordTypeSessionName,
	} {
		if !seen[recordType] {
			t.Fatalf("missing record type %s in path %#v", recordType, path)
		}
	}

	messages, err := sess.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if len(messages) != 3 {
		t.Fatalf("expected user, custom, and bash messages, got %d", len(messages))
	}
	if !reflect.DeepEqual(messages[1], customMessage) {
		t.Fatalf("custom message mismatch\n got: %#v\nwant: %#v", messages[1], customMessage)
	}
	if !reflect.DeepEqual(messages[2], bash) {
		t.Fatalf("bash message mismatch\n got: %#v\nwant: %#v", messages[2], bash)
	}
	if sess.Name() != "Schema phase" {
		t.Fatalf("name mismatch: %q", sess.Name())
	}
}

func TestSessionPathToLeafBranches(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()

	appendText := func(text string) string {
		t.Helper()
		if err := sess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: text}}}); err != nil {
			t.Fatal(err)
		}
		return sess.LeafID()
	}

	first := appendText("first")
	second := appendText("second")
	third := appendText("third")
	if err := sess.SetLeafID(first); err != nil {
		t.Fatal(err)
	}
	branch := appendText("branch")

	mainPath, err := sess.PathToLeaf(third)
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, mainPath, []string{"first", "second", "third"})

	branchPath, err := sess.PathToLeaf(branch)
	if err != nil {
		t.Fatal(err)
	}
	assertPathTexts(t, branchPath, []string{"first", "branch"})

	if err := sess.SetLeafID(second); err != nil {
		t.Fatal(err)
	}
	messages, err := sess.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if len(messages) != 2 {
		t.Fatalf("expected active branch to contain two messages, got %d", len(messages))
	}
	if err := sess.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := store.Open(sess.ID())
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if reopened.LeafID() != second {
		t.Fatalf("reopened leaf mismatch: got %q want %q", reopened.LeafID(), second)
	}
}

func TestSessionLabelsRoundTrip(t *testing.T) {
	dir := t.TempDir()
	store := NewJSONLStore(dir)
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	if err := sess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "target"}}}); err != nil {
		t.Fatal(err)
	}
	targetID := sess.LeafID()
	if err := sess.AppendLabel(targetID, "important"); err != nil {
		t.Fatal(err)
	}
	if got := sess.Labels()[targetID]; got != "important" {
		t.Fatalf("label mismatch before reopen: %q", got)
	}
	if err := sess.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, err := store.Open(sess.ID())
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if got := reopened.Labels()[targetID]; got != "important" {
		t.Fatalf("label mismatch after reopen: %q", got)
	}
	if err := reopened.AppendLabel(targetID, ""); err != nil {
		t.Fatal(err)
	}
	if _, ok := reopened.Labels()[targetID]; ok {
		t.Fatal("expected label to be cleared")
	}
}

func TestSessionNewContentTypesRoundTrip(t *testing.T) {
	store := NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()

	message := agent.AssistantMessage{
		Content: []agent.Content{
			agent.ThinkingContent{Thinking: "hidden", Signature: "legacy", ThinkingSignature: "sig", Redacted: true},
			agent.RedactedThinkingContent{Data: "opaque"},
			agent.ServerToolUseContent{ID: "srv-1", Name: "web_search", Input: json.RawMessage(`{"query":"pi"}`)},
			agent.ToolResultContent{
				ToolUseID: "call-1",
				Content:   []agent.Content{agent.TextContent{Text: "nested"}},
				IsError:   true,
			},
		},
		StopReason:    agent.StopOverloaded,
		Model:         "requested",
		API:           "anthropic-messages",
		Provider:      "anthropic",
		ResponseID:    "msg-1",
		ResponseModel: "actual",
		ErrorMessage:  "busy",
		Cost:          &agent.Cost{Input: 1, Output: 2, Total: 3},
		Usage:         agent.Usage{InputTokens: 10, OutputTokens: 11, TotalTokens: 21, CacheCreationInputTokens: 3, CacheReadInputTokens: 4},
		Timestamp:     time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC),
	}
	if err := sess.AppendMessage(message); err != nil {
		t.Fatal(err)
	}
	messages, err := sess.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if len(messages) != 1 {
		t.Fatalf("expected one message, got %d", len(messages))
	}
	if !reflect.DeepEqual(messages[0], message) {
		t.Fatalf("message mismatch\n got: %#v\nwant: %#v", messages[0], message)
	}
}

func assertPathTexts(t *testing.T, path []Record, want []string) {
	t.Helper()
	got := make([]string, 0, len(path))
	for _, record := range path {
		if record.Type != RecordTypeMessage {
			continue
		}
		message, err := decodeMessagePayload(record.Payload)
		if err != nil {
			t.Fatal(err)
		}
		userMessage, ok := message.(agent.UserMessage)
		if !ok {
			t.Fatalf("expected user message, got %T", message)
		}
		text, ok := userMessage.Content[0].(agent.TextContent)
		if !ok {
			t.Fatalf("expected text content, got %T", userMessage.Content[0])
		}
		got = append(got, text.Text)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("path mismatch\n got: %#v\nwant: %#v", got, want)
	}
}

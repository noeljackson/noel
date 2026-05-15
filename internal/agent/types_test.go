package agent

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
	"time"
)

func TestStopReasonString(t *testing.T) {
	reasons := []StopReason{
		StopEndTurn,
		StopMaxTokens,
		StopToolUse,
		StopStopSequence,
		StopAbort,
		StopError,
		StopRefusal,
		StopPauseTurn,
		StopOverloaded,
	}
	for _, reason := range reasons {
		if reason.String() != string(reason) {
			t.Fatalf("String mismatch for %q", reason)
		}
	}
}

func TestMessageTimestamp(t *testing.T) {
	base := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	messages := []Message{
		UserMessage{Timestamp: base.Add(time.Second)},
		AssistantMessage{Timestamp: base.Add(2 * time.Second)},
		ToolResultMessage{Timestamp: base.Add(3 * time.Second)},
		SystemMessage{Timestamp: base.Add(4 * time.Second)},
		BashExecutionMessage{Timestamp: base.Add(5 * time.Second)},
		CustomMessage{Timestamp: base.Add(6 * time.Second)},
		BranchSummaryMessage{Timestamp: base.Add(7 * time.Second)},
		CompactionSummaryMessage{Timestamp: base.Add(8 * time.Second)},
	}

	for _, message := range messages {
		if got := MessageTimestamp(message); !got.Equal(reflect.ValueOf(message).FieldByName("Timestamp").Interface().(time.Time)) {
			t.Fatalf("timestamp mismatch for %T: %s", message, got)
		}
	}
}

func TestConvertToLLM(t *testing.T) {
	ts := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	exitCode := 2
	user := UserMessage{Content: []Content{TextContent{Text: "keep"}}, Timestamp: ts}
	messages := []Message{
		user,
		BashExecutionMessage{Timestamp: ts, Command: "ls", Output: "out", ExitCode: &exitCode},
		BashExecutionMessage{Timestamp: ts, Command: "secret", Output: "hidden", ExcludeFromContext: true},
		CustomMessage{Timestamp: ts, Content: []Content{TextContent{Text: "custom"}}},
		BranchSummaryMessage{Timestamp: ts, Summary: "branch"},
		CompactionSummaryMessage{Timestamp: ts, Summary: "compact"},
	}

	got := ConvertToLLM(messages)
	if len(got) != 5 {
		t.Fatalf("expected 5 messages, got %d", len(got))
	}
	if !reflect.DeepEqual(got[0], user) {
		t.Fatalf("standard message was not preserved: %#v", got[0])
	}
	texts := make([]string, 0, len(got)-1)
	for _, message := range got[1:] {
		userMessage, ok := message.(UserMessage)
		if !ok {
			t.Fatalf("converted message has type %T", message)
		}
		text, ok := userMessage.Content[0].(TextContent)
		if !ok {
			t.Fatalf("converted content has type %T", userMessage.Content[0])
		}
		texts = append(texts, text.Text)
	}
	if !strings.Contains(texts[0], "Ran `ls`") || !strings.Contains(texts[0], "Command exited with code 2") {
		t.Fatalf("bash text mismatch: %q", texts[0])
	}
	if texts[1] != "custom" {
		t.Fatalf("custom text mismatch: %q", texts[1])
	}
	if !strings.Contains(texts[2], "summary of a branch") || !strings.Contains(texts[2], "branch") {
		t.Fatalf("branch summary text mismatch: %q", texts[2])
	}
	if !strings.Contains(texts[3], "conversation history") || !strings.Contains(texts[3], "compact") {
		t.Fatalf("compaction summary text mismatch: %q", texts[3])
	}
}

func TestBashExecutionToText(t *testing.T) {
	cancelled := BashExecutionToText(BashExecutionMessage{Command: "sleep", Cancelled: true})
	if !strings.Contains(cancelled, "(no output)") || !strings.Contains(cancelled, "command cancelled") {
		t.Fatalf("cancelled text mismatch: %q", cancelled)
	}

	truncated := BashExecutionToText(BashExecutionMessage{
		Command:        "go test",
		Stdout:         "ok",
		Stderr:         "warn",
		Truncated:      true,
		FullOutputPath: "/tmp/full.txt",
	})
	if !strings.Contains(truncated, "ok\nwarn") || !strings.Contains(truncated, "/tmp/full.txt") {
		t.Fatalf("truncated text mismatch: %q", truncated)
	}
}

func TestNewContentJSONRoundTrip(t *testing.T) {
	text := TextContent{Text: "cache me", TextSignature: "sig", CacheControl: &CacheControl{Type: "ephemeral"}}
	var textRoundTrip TextContent
	mustJSONRoundTrip(t, text, &textRoundTrip)
	if !reflect.DeepEqual(textRoundTrip, text) {
		t.Fatalf("text content mismatch: %#v", textRoundTrip)
	}

	thinking := ThinkingContent{Thinking: "thought", Signature: "legacy", ThinkingSignature: "sig", Redacted: true}
	var thinkingRoundTrip ThinkingContent
	mustJSONRoundTrip(t, thinking, &thinkingRoundTrip)
	if !reflect.DeepEqual(thinkingRoundTrip, thinking) {
		t.Fatalf("thinking content mismatch: %#v", thinkingRoundTrip)
	}

	redacted := RedactedThinkingContent{Data: "opaque"}
	var redactedRoundTrip RedactedThinkingContent
	mustJSONRoundTrip(t, redacted, &redactedRoundTrip)
	if redactedRoundTrip != redacted {
		t.Fatalf("redacted content mismatch: %#v", redactedRoundTrip)
	}

	serverTool := ServerToolUseContent{ID: "srv-1", Name: "web_search", Input: json.RawMessage(`{"q":"pi"}`)}
	var serverToolRoundTrip ServerToolUseContent
	mustJSONRoundTrip(t, serverTool, &serverToolRoundTrip)
	if !reflect.DeepEqual(serverToolRoundTrip, serverTool) {
		t.Fatalf("server tool content mismatch: %#v", serverToolRoundTrip)
	}
}

func mustJSONRoundTrip(t *testing.T, value interface{}, target interface{}) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, target); err != nil {
		t.Fatal(err)
	}
}

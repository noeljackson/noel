package exporter

import (
	"bytes"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/session"
)

func TestExportImportRoundTrip(t *testing.T) {
	store := session.NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()
	if err := sess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "hello"}}}); err != nil {
		t.Fatal(err)
	}
	if err := sess.AppendMessage(agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "world"}}}); err != nil {
		t.Fatal(err)
	}

	var buf bytes.Buffer
	if err := Export(sess, &buf); err != nil {
		t.Fatal(err)
	}
	importedID, err := Import(store, &buf)
	if err != nil {
		t.Fatal(err)
	}
	imported, err := store.Open(importedID)
	if err != nil {
		t.Fatal(err)
	}
	defer imported.Close()

	messages, err := imported.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if len(messages) != 2 {
		t.Fatalf("messages = %d, want 2", len(messages))
	}
	if got := text(messages[0]); got != "hello" {
		t.Fatalf("first message = %q, want hello", got)
	}
	if got := text(messages[1]); got != "world" {
		t.Fatalf("second message = %q, want world", got)
	}
}

func text(message agent.Message) string {
	switch typed := message.(type) {
	case agent.UserMessage:
		return typed.Content[0].(agent.TextContent).Text
	case agent.AssistantMessage:
		return typed.Content[0].(agent.TextContent).Text
	default:
		return ""
	}
}

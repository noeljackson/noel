package compaction

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"testing"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/session"
)

func TestRecordPersistsCompactionPayload(t *testing.T) {
	store := session.NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()

	if err := Record(sess, "parent-1", "summary text", 3); err != nil {
		t.Fatal(err)
	}

	infos, err := store.List()
	if err != nil {
		t.Fatal(err)
	}
	if len(infos) != 1 {
		t.Fatalf("expected one session, got %d", len(infos))
	}

	file, err := os.Open(infos[0].Path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()

	var found bool
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		var record session.Record
		if err := json.Unmarshal(scanner.Bytes(), &record); err != nil {
			t.Fatal(err)
		}
		if record.Type != session.RecordTypeCompaction {
			continue
		}
		found = true
		var payload struct {
			Summary             string `json:"summary"`
			DroppedMessageCount int    `json:"dropped_message_count"`
			CompactedAt         string `json:"compacted_at"`
		}
		if err := json.Unmarshal(record.Payload, &payload); err != nil {
			t.Fatal(err)
		}
		if payload.Summary != "summary text" || payload.DroppedMessageCount != 3 || payload.CompactedAt == "" {
			t.Fatalf("unexpected payload: %#v", payload)
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	if !found {
		t.Fatal("compaction record not found")
	}
}

func TestMaybeCompactRecordsAndReturnsCompactedMessages(t *testing.T) {
	store := session.NewJSONLStore(t.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		t.Fatal(err)
	}
	defer sess.Close()

	messages := []agent.Message{
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "older request with enough text"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "older answer with enough text"}}},
		agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "recent request with enough text"}}},
		agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "recent answer with enough text"}}},
	}
	for _, message := range messages {
		if err := sess.AppendMessage(message); err != nil {
			t.Fatal(err)
		}
	}

	compactor := New(Settings{
		TriggerFraction: 0.1,
		MaxTokens:       100,
		TargetTokens:    20,
		SummaryProvider: &mockSummaryProvider{summary: "summary"},
	})
	got, err := compactor.MaybeCompact(context.Background(), sess, messages, "")
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 {
		t.Fatalf("expected compacted messages, got %d", len(got))
	}

	reopened, err := store.Open(sess.ID())
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()

	restored, err := reopened.Messages()
	if err != nil {
		t.Fatal(err)
	}
	if len(restored) != 3 {
		t.Fatalf("expected restored compacted messages, got %d", len(restored))
	}
	if _, ok := restored[0].(agent.SystemMessage); !ok {
		t.Fatalf("expected restored first message to be system summary, got %T", restored[0])
	}
}
